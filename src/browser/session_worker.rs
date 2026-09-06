//! The sole session-checkpoint filesystem owner.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::browser::session::{self, MAX_FILE_BYTES, SessionSnapshot};
use crate::msg::Msg;

const QUIET_PERIOD: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct Save {
    revision: u64,
    snapshot: SessionSnapshot,
}

#[derive(Default, Debug)]
struct State {
    pending: Option<Save>,
    latest_revision: u64,
    stopping: bool,
}

/// Submission only replaces shallow in-memory state. The owned thread holds
/// every file resource and is joined explicitly after the interactive loop.
pub struct SessionWorker {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl SessionWorker {
    pub fn spawn(path: Option<PathBuf>, events: Sender<Msg>) -> Self {
        Self::spawn_with_quiet_period(path, events, QUIET_PERIOD)
    }

    fn spawn_with_quiet_period(
        path: Option<PathBuf>,
        events: Sender<Msg>,
        quiet_period: Duration,
    ) -> Self {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let thread = thread::spawn(move || run(path, events, worker_shared, quiet_period));
        Self {
            shared,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, revision: u64, snapshot: SessionSnapshot) {
        if revision == 0 {
            return;
        }
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.stopping && revision > state.latest_revision {
            state.latest_revision = revision;
            state.pending = Some(Save { revision, snapshot });
            wake.notify_one();
        }
    }

    /// Bypasses the quiet period and flushes the newest accepted snapshot.
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
    quiet_period: Duration,
) {
    let loaded = match &path {
        Some(path) => load(path),
        None => Ok(None),
    };
    let writable = path.is_some() && loaded.is_ok();
    let _ = events.send(Msg::SessionLoaded(loaded));

    loop {
        let save = {
            let (lock, wake) = &*shared;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.pending.is_none() && !state.stopping {
                state = wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            while !state.stopping && state.pending.is_some() && !quiet_period.is_zero() {
                let pending_revision = state.pending.as_ref().map(|save| save.revision);
                let (next, timeout) = wake
                    .wait_timeout(state, quiet_period)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state = next;
                if timeout.timed_out()
                    && state.pending.as_ref().map(|save| save.revision) == pending_revision
                {
                    break;
                }
            }
            match state.pending.take() {
                Some(save) => Some(save),
                None if state.stopping => None,
                None => continue,
            }
        };
        let Some(save) = save else { break };
        let result = if writable {
            save_atomic(
                path.as_ref().expect("writable session worker has a path"),
                &save.snapshot,
            )
        } else {
            Err("session persistence is unavailable or read-only".into())
        };
        let _ = events.send(Msg::SessionSaved {
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

fn load(path: &Path) -> Result<Option<SessionSnapshot>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(path_error(path, "read", &error)),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| path_error(path, "read", &error))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{}: session file is larger than 256 KiB",
            path.display()
        ));
    }
    session::decode(&bytes)
        .map(Some)
        .map_err(|error| bounded(format!("{}: {error}", path.display())))
}

fn save_atomic(path: &Path, snapshot: &SessionSnapshot) -> Result<(), String> {
    save_atomic_inner(
        path,
        snapshot,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailurePoint {
    CreateParent,
    Open,
    Write,
    Sync,
    Rename,
}

fn save_atomic_inner(
    path: &Path,
    snapshot: &SessionSnapshot,
    #[cfg(test)] failure: Option<FailurePoint>,
) -> Result<(), String> {
    let bytes = session::encode(snapshot).map_err(|error| error.to_string())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(test)]
    fail_at(failure, FailurePoint::CreateParent)?;
    create_parent(parent).map_err(|error| path_error(path, "create parent", &error))?;
    let temp = temporary_path(path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(replacement_mode(path));
        }
        #[cfg(test)]
        fail_at(failure, FailurePoint::Open)?;
        let mut file = options
            .open(&temp)
            .map_err(|error| path_error(path, "open temporary file", &error))?;
        #[cfg(test)]
        fail_at(failure, FailurePoint::Write)?;
        file.write_all(&bytes)
            .map_err(|error| path_error(path, "write", &error))?;
        file.flush()
            .map_err(|error| path_error(path, "flush", &error))?;
        #[cfg(test)]
        fail_at(failure, FailurePoint::Sync)?;
        file.sync_all()
            .map_err(|error| path_error(path, "sync", &error))?;
        #[cfg(test)]
        fail_at(failure, FailurePoint::Rename)?;
        fs::rename(&temp, path).map_err(|error| path_error(path, "replace", &error))?;
        sync_directory(parent).map_err(|error| path_error(path, "sync parent", &error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn create_parent(parent: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)
}

#[cfg(test)]
fn fail_at(actual: Option<FailurePoint>, expected: FailurePoint) -> Result<(), String> {
    if actual == Some(expected) {
        Err(format!("injected {expected:?} failure"))
    } else {
        Ok(())
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.yata-session-tmp-{}", std::process::id()))
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
    bounded(format!("{}: cannot {operation}: {error}", path.display()))
}

fn bounded(mut message: String) -> String {
    if message.len() > 1_024 {
        let at = message
            .char_indices()
            .map(|(at, _)| at)
            .take_while(|at| *at <= 1_024)
            .last()
            .unwrap_or(0);
        message.truncate(at);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::session::SessionTab;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "yata-session-{}-{nonce}-{sequence}",
                std::process::id()
            ))
            .join(name)
    }

    fn snapshot(url: &str, scroll: u32) -> SessionSnapshot {
        SessionSnapshot::new(
            0,
            Arc::from([SessionTab {
                url: Some(Arc::from(url)),
                scroll,
            }]),
        )
        .unwrap()
    }

    #[test]
    fn missing_load_then_save_creates_private_file_and_round_trips() {
        let path = temp_path("nested/session");
        let (tx, rx) = mpsc::channel();
        let worker = SessionWorker::spawn_with_quiet_period(Some(path.clone()), tx, Duration::ZERO);
        assert_eq!(rx.recv().unwrap(), Msg::SessionLoaded(Ok(None)));
        let expected = snapshot("https://example.test/#part", i32::MAX as u32);
        worker.submit(1, expected.clone());
        assert_eq!(
            rx.recv().unwrap(),
            Msg::SessionSaved {
                revision: 1,
                result: Ok(())
            }
        );
        worker.shutdown();
        assert_eq!(load(&path).unwrap(), Some(expected));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = fs::remove_dir_all(path.ancestors().nth(2).unwrap());
    }

    #[test]
    fn latest_revision_wins_and_shutdown_flushes_without_a_timer_wait() {
        let path = temp_path("session");
        let (tx, rx) = mpsc::channel();
        let worker =
            SessionWorker::spawn_with_quiet_period(Some(path.clone()), tx, Duration::from_secs(60));
        let _ = rx.recv().unwrap();
        worker.submit(1, snapshot("https://one.test/", 1));
        worker.submit(2, snapshot("https://two.test/", 2));
        worker.submit(3, snapshot("https://three.test/", 3));
        worker.shutdown();
        assert_eq!(
            load(&path).unwrap(),
            Some(snapshot("https://three.test/", 3))
        );
        let saved: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            saved,
            [Msg::SessionSaved {
                revision: 3,
                result: Ok(())
            }]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_stale_submission_cannot_follow_a_newer_accepted_revision() {
        let path = temp_path("session");
        let (tx, rx) = mpsc::channel();
        let worker =
            SessionWorker::spawn_with_quiet_period(Some(path.clone()), tx, Duration::from_secs(60));
        let _ = rx.recv().unwrap();
        worker.submit(3, snapshot("https://new.test/", 3));
        worker.submit(2, snapshot("https://stale.test/", 2));
        worker.shutdown();
        assert_eq!(load(&path).unwrap(), Some(snapshot("https://new.test/", 3)));
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [Msg::SessionSaved {
                revision: 3,
                result: Ok(())
            }]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn malformed_named_file_is_read_only_and_preserved() {
        let path = temp_path("session");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"yata-session-v2\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = SessionWorker::spawn_with_quiet_period(Some(path.clone()), tx, Duration::ZERO);
        assert!(matches!(rx.recv().unwrap(), Msg::SessionLoaded(Err(_))));
        worker.submit(1, snapshot("https://example.test/", 0));
        assert!(matches!(
            rx.recv().unwrap(),
            Msg::SessionSaved {
                revision: 1,
                result: Err(_)
            }
        ));
        worker.shutdown();
        assert_eq!(fs::read(&path).unwrap(), b"yata-session-v2\n");
        assert!(!temporary_path(&path).exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn every_pre_rename_failure_preserves_the_named_checkpoint_and_cleans_temp() {
        let path = temp_path("session");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = snapshot("https://old.test/", 4);
        save_atomic(&path, &old).unwrap();
        for point in [
            FailurePoint::CreateParent,
            FailurePoint::Open,
            FailurePoint::Write,
            FailurePoint::Sync,
            FailurePoint::Rename,
        ] {
            assert!(
                save_atomic_inner(&path, &snapshot("https://new.test/", 9), Some(point)).is_err()
            );
            assert_eq!(load(&path).unwrap(), Some(old.clone()), "{point:?}");
            assert!(!temporary_path(&path).exists(), "{point:?}");
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
