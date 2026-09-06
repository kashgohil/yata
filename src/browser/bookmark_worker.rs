//! The sole bookmark filesystem owner.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::browser::bookmarks::{self, Bookmark, Bookmarks, MAX_FILE_BYTES};
use crate::msg::Msg;

#[derive(Debug)]
struct Save {
    revision: u64,
    records: Arc<[Bookmark]>,
}

#[derive(Default, Debug)]
struct State {
    pending: Option<Save>,
    stopping: bool,
}

/// A short-lock submission handle. `shutdown` is deliberately separate: the
/// interactive loop only calls `submit`; joining happens after it has exited.
pub struct BookmarkWorker {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(test)]
struct SaveGate {
    released: Mutex<bool>,
    wake: Condvar,
    entered: Sender<()>,
}

impl BookmarkWorker {
    pub fn spawn(path: Option<PathBuf>, events: Sender<Msg>) -> Self {
        Self::spawn_inner(path, events, None)
    }

    fn spawn_inner(
        path: Option<PathBuf>,
        events: Sender<Msg>,
        #[cfg(test)] gate: Option<Arc<SaveGate>>,
        #[cfg(not(test))] _gate: Option<()>,
    ) -> Self {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let thread = thread::spawn(move || {
            run(
                path,
                events,
                worker_shared,
                #[cfg(test)]
                gate,
            )
        });
        Self {
            shared,
            thread: Some(thread),
        }
    }

    /// Latest wins while the worker is busy, but a currently-writing revision
    /// is never interrupted and writes are therefore always serialized.
    pub fn submit(&self, revision: u64, records: Arc<[Bookmark]>) {
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.stopping {
            state.pending = Some(Save { revision, records });
            wake.notify_one();
        }
    }

    pub fn shutdown(mut self) {
        let (lock, wake) = &*self.shared;
        {
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.stopping = true;
            wake.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    path: Option<PathBuf>,
    events: Sender<Msg>,
    shared: Arc<(Mutex<State>, Condvar)>,
    #[cfg(test)] gate: Option<Arc<SaveGate>>,
) {
    let loaded = match &path {
        Some(path) => load(path),
        None => Ok(Bookmarks::new()),
    };
    let writable = path.is_some() && loaded.is_ok();
    let _ = events.send(Msg::BookmarksLoaded(loaded));

    loop {
        let save = {
            let (lock, wake) = &*shared;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.pending.is_none() && !state.stopping {
                state = wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            match state.pending.take() {
                Some(save) => Some(save),
                None if state.stopping => None,
                None => continue,
            }
        };
        let Some(save) = save else { break };
        #[cfg(test)]
        if let Some(gate) = &gate {
            let _ = gate.entered.send(());
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.wake.wait(released).unwrap();
            }
        }
        let result = if writable {
            save_atomic(
                path.as_ref().expect("writable worker has a path"),
                &save.records,
            )
        } else {
            Err("bookmark persistence is unavailable or read-only".into())
        };
        let _ = events.send(Msg::BookmarksSaved {
            revision: save.revision,
            result,
        });

        let (lock, _) = &*shared;
        let state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.stopping && state.pending.is_none() {
            break;
        }
    }
}

fn load(path: &Path) -> Result<Bookmarks, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Bookmarks::new()),
        Err(error) => return Err(path_error(path, "read", &error)),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| path_error(path, "read", &error))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{}: bookmark file is larger than 16 MiB",
            path.display()
        ));
    }
    bookmarks::decode(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn save_atomic(path: &Path, records: &[Bookmark]) -> Result<(), String> {
    let bytes = bookmarks::encode(records).map_err(|error| error.to_string())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| path_error(path, "create parent", &error))?;
    let temp = temporary_path(path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(replacement_mode(path));
        }
        let mut file = options
            .open(&temp)
            .map_err(|error| path_error(path, "open temporary file", &error))?;
        file.write_all(&bytes)
            .map_err(|error| path_error(path, "write", &error))?;
        file.flush()
            .map_err(|error| path_error(path, "flush", &error))?;
        file.sync_all()
            .map_err(|error| path_error(path, "sync", &error))?;
        fs::rename(&temp, path).map_err(|error| path_error(path, "replace", &error))?;
        sync_directory(parent).map_err(|error| path_error(path, "sync parent", &error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.yata-tmp-{}", std::process::id()))
}

#[cfg(unix)]
fn replacement_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o600)
        .unwrap_or(0o600)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn path_error(path: &Path, operation: &str, error: &io::Error) -> String {
    let mut message = format!("{}: cannot {operation}: {error}", path.display());
    message.truncate(1_024);
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "yata-bookmarks-{}-{nonce}-{sequence}",
                std::process::id()
            ))
            .join(name)
    }

    fn record(url: &str, title: &str) -> Bookmark {
        Bookmark {
            url: Arc::from(url),
            title: Arc::from(title),
        }
    }

    #[test]
    fn missing_load_then_save_creates_private_file_and_round_trips() {
        let path = temp_path("nested/bookmarks");
        let (tx, rx) = mpsc::channel();
        let worker = BookmarkWorker::spawn(Some(path.clone()), tx);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::BookmarksLoaded(Ok(Bookmarks::new()))
        );
        worker.submit(1, Arc::from([record("https://example.test/", "Example")]));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::BookmarksSaved {
                revision: 1,
                result: Ok(())
            }
        );
        worker.shutdown();
        assert_eq!(
            load(&path).unwrap().records(),
            &[record("https://example.test/", "Example")]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(path.ancestors().nth(2).unwrap());
    }

    #[test]
    fn malformed_named_file_is_read_only_and_preserved() {
        let path = temp_path("bookmarks");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"future-format\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = BookmarkWorker::spawn(Some(path.clone()), tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::BookmarksLoaded(Err(_))
        ));
        worker.submit(1, Arc::from([record("https://example.test/", "Example")]));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::BookmarksSaved {
                revision: 1,
                result: Err(_)
            }
        ));
        worker.shutdown();
        assert_eq!(fs::read(&path).unwrap(), b"future-format\n");
        assert!(!temporary_path(&path).exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn shutdown_flushes_the_latest_pending_snapshot() {
        let path = temp_path("bookmarks");
        let (tx, rx) = mpsc::channel();
        let worker = BookmarkWorker::spawn(Some(path.clone()), tx);
        let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.submit(1, Arc::from([record("https://one.test/", "One")]));
        worker.submit(2, Arc::from([record("https://two.test/", "Two")]));
        worker.submit(3, Arc::from([record("https://three.test/", "Three")]));
        worker.shutdown();
        assert_eq!(
            load(&path).unwrap().records(),
            &[record("https://three.test/", "Three")]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn blocked_first_write_coalesces_pending_revisions_to_the_latest() {
        let path = temp_path("bookmarks");
        let (tx, rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let gate = Arc::new(SaveGate {
            released: Mutex::new(false),
            wake: Condvar::new(),
            entered: entered_tx,
        });
        let worker = BookmarkWorker::spawn_inner(Some(path.clone()), tx, Some(Arc::clone(&gate)));
        let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.submit(1, Arc::from([record("https://one.test/", "One")]));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.submit(2, Arc::from([record("https://two.test/", "Two")]));
        worker.submit(3, Arc::from([record("https://three.test/", "Three")]));
        *gate.released.lock().unwrap() = true;
        gate.wake.notify_one();

        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            first,
            Msg::BookmarksSaved {
                revision: 1,
                result: Ok(())
            }
        ));
        assert!(matches!(
            second,
            Msg::BookmarksSaved {
                revision: 3,
                result: Ok(())
            }
        ));
        assert!(
            rx.try_recv().is_err(),
            "revision 2 was written instead of coalesced"
        );
        worker.shutdown();
        assert_eq!(
            load(&path).unwrap().records(),
            &[record("https://three.test/", "Three")]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn temporary_open_failure_preserves_the_last_good_named_file() {
        let path = temp_path("bookmarks");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = bookmarks::encode(&[record("https://old.test/", "Old")]).unwrap();
        fs::write(&path, &old).unwrap();
        fs::create_dir(temporary_path(&path)).unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = BookmarkWorker::spawn(Some(path.clone()), tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::BookmarksLoaded(Ok(_))
        ));
        worker.submit(1, Arc::from([record("https://new.test/", "New")]));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Msg::BookmarksSaved {
                revision: 1,
                result: Err(_)
            }
        ));
        worker.shutdown();
        assert_eq!(fs::read(&path).unwrap(), old);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
