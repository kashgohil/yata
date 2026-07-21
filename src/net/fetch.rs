use std::error::Error;
use std::io::Read;
use std::sync::mpsc::Sender;
use std::thread;

use crate::msg::Msg;
use crate::net::FetchId;

/// Read size per chunk: small enough that progress messages arrive steadily
/// on slow links, large enough that syscall overhead is irrelevant.
const CHUNK: usize = 16 * 1024;

/// Fetch `url` on a detached worker thread; returns immediately. The worker
/// talks to the rest of the program only by sending `Msg`s into `tx`: one
/// `Loading` per body chunk, then exactly one `Loaded` on success or one
/// `NetError` on any failure. It never panics and never prints; if the
/// channel is closed (the app quit), it just stops.
pub fn spawn_fetch(id: FetchId, url: String, tx: Sender<Msg>) {
    thread::spawn(move || {
        match fetch(id, &url, &tx) {
            Ok(Some(loaded)) => {
                let _ = tx.send(loaded);
            }
            // Channel closed mid-stream: nobody is listening anymore.
            Ok(None) => {}
            Err(reason) => {
                let _ = tx.send(Msg::NetError { id, url, reason });
            }
        }
    });
}

/// The whole request, run on the worker. `Ok(Some(Loaded))` on success,
/// `Ok(None)` if the channel closed mid-stream, `Err(reason)` on any failure
/// (bad URL, DNS, connect, TLS, mid-body disconnect).
fn fetch(id: FetchId, url: &str, tx: &Sender<Msg>) -> Result<Option<Msg>, String> {
    // Built here, not in `spawn_fetch`, so the UI thread never touches
    // reqwest. Defaults follow redirects and (via the gzip feature)
    // transparently decompress.
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(describe)?;
    let mut resp = client.get(url).send().map_err(describe)?;
    let status = resp.status().as_u16();
    // The final URL, after redirects — what M1.5's URL bar should display.
    let final_url = resp.url().to_string();

    let mut body = Vec::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = resp.read(&mut buf).map_err(describe)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
        let progress = Msg::Loading {
            id,
            bytes_so_far: body.len() as u64,
        };
        if tx.send(progress).is_err() {
            return Ok(None);
        }
    }
    Ok(Some(Msg::Loaded {
        id,
        url: final_url,
        status,
        body,
    }))
}

/// reqwest's top-level Display is vague ("error sending request…"); the
/// human-readable cause ("Connection refused") lives down the source chain,
/// so flatten the chain into the reason the user will see.
fn describe(err: impl Error) -> String {
    let mut reason = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        reason.push_str(": ");
        reason.push_str(&cause.to_string());
        source = cause.source();
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::Duration;

    /// Serve one canned HTTP response on an ephemeral local port, from a test
    /// thread. Tests never hit the network (CLAUDE.md conventions).
    fn serve_once(response: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the request through the blank line before answering, so
            // the client is never racing a response to an unsent request.
            let mut req = Vec::new();
            let mut buf = [0u8; 512];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(response);
        });
        addr
    }

    /// Collect every message the worker sends, ending when it drops its
    /// sender. The timeout turns a hung worker into a test failure instead of
    /// a hung test run.
    fn drain(rx: Receiver<Msg>) -> Vec<Msg> {
        let mut msgs = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(msg) => msgs.push(msg),
                Err(RecvTimeoutError::Disconnected) => return msgs,
                Err(RecvTimeoutError::Timeout) => panic!("fetch worker never finished"),
            }
        }
    }

    #[test]
    fn local_server_success_sends_loading_then_exactly_one_loaded() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
        );
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(1), url.clone(), tx);

        let msgs = drain(rx);
        let (last, progress) = msgs.split_last().expect("worker sent nothing");
        assert!(
            !progress.is_empty(),
            "expected at least one Loading before Loaded, got only {last:?}"
        );
        let mut prev = 0;
        for msg in progress {
            match msg {
                Msg::Loading { id, bytes_so_far } => {
                    assert_eq!(*id, FetchId(1));
                    assert!(*bytes_so_far > prev, "byte counts must grow");
                    prev = *bytes_so_far;
                }
                other => panic!("expected only Loading before Loaded, got {other:?}"),
            }
        }
        assert_eq!(
            *last,
            Msg::Loaded {
                id: FetchId(1),
                url,
                status: 200,
                body: b"hello world".to_vec(),
            }
        );
    }

    #[test]
    fn closed_port_sends_exactly_one_net_error_with_reason() {
        // Bind then drop: the freed ephemeral port refuses connections.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(2), url.clone(), tx);

        let msgs = drain(rx);
        assert_eq!(msgs.len(), 1, "exactly one message expected, got {msgs:?}");
        match &msgs[0] {
            Msg::NetError {
                id,
                url: reported,
                reason,
            } => {
                assert_eq!(*id, FetchId(2));
                assert_eq!(*reported, url);
                assert!(!reason.is_empty(), "reason must be human-readable");
            }
            other => panic!("expected NetError, got {other:?}"),
        }
    }

    #[test]
    fn bad_url_sends_exactly_one_net_error() {
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(3), "not a url".to_string(), tx);
        let msgs = drain(rx);
        assert_eq!(msgs.len(), 1, "exactly one message expected, got {msgs:?}");
        assert!(matches!(
            &msgs[0],
            Msg::NetError { id: FetchId(3), url, reason }
                if url == "not a url" && !reason.is_empty()
        ));
    }
}
