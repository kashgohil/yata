use std::error::Error;
use std::io::Read;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Instant;

use crate::css;
use crate::html;
use crate::msg::Msg;
use crate::net::FetchId;

/// Read size per chunk: small enough that progress messages arrive steadily
/// on slow links, large enough that syscall overhead is irrelevant.
const CHUNK: usize = 16 * 1024;

/// Fetch `url` on a detached worker thread; returns immediately. The worker
/// talks to the rest of the program only by sending `Msg`s into `tx`:
/// - progress: zero or more `Loading`
/// - terminal success: always one `Loaded` (so `--dump` can print any body)
/// - parse: one `Parsed` only when [`is_document`] is true (2xx + document
///   content-type). Non-documents and HTTP errors skip `Parsed` so the TUI
///   error page is not clobbered (M7).
/// - failure: one `NetError` (DNS, TLS, connect, mid-body disconnect)
///
/// It never panics and never prints; if the channel is closed (the app quit),
/// it just stops.
///
/// Parsing lives here, not in the `Loaded` handler: the worker already owns
/// the bytes and already runs off the UI thread, and a Wikipedia-sized parse
/// (~tens of ms) would blow the keypress→screen budget if the UI thread did
/// it. `Loaded` goes out first so a document body shows without waiting on
/// the parse.
pub fn spawn_fetch(id: FetchId, url: String, tx: Sender<Msg>) {
    thread::spawn(move || {
        match fetch(id, &url, &tx) {
            Ok(Some(loaded)) => {
                let Msg::Loaded {
                    body,
                    status,
                    content_type,
                    ..
                } = &loaded
                else {
                    unreachable!("fetch's success message is always Loaded");
                };
                let should_parse = is_document(*status, content_type.as_deref());
                let text = should_parse.then(|| html::decode_body(body));
                if tx.send(loaded).is_err() {
                    return;
                }
                if let Some(text) = text {
                    let started = Instant::now();
                    let dom = html::parse(&text);
                    let _ = tx.send(Msg::Parsed {
                        id,
                        dom,
                        elapsed: started.elapsed(),
                    });
                }
            }
            // Channel closed mid-stream: nobody is listening anymore.
            Ok(None) => {}
            Err((url, reason)) => {
                let _ = tx.send(Msg::NetError { id, url, reason });
            }
        }
    });
}

/// Whether a response should be parsed and styled as a document (vs an error
/// page). Charset parameters are ignored. Missing/empty content-type is
/// treated as a document — many servers omit it for HTML.
///
/// Single source of truth for the worker (`Parsed` gate) and the TUI
/// (`error_page` / `App` error-page path). Do not reimplement elsewhere.
pub fn is_document(status: u16, content_type: Option<&str>) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    let Some(ct) = content_type else {
        return true;
    };
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.is_empty()
        || mime == "text/html"
        || mime == "application/xhtml+xml"
        || mime == "text/plain"
        || mime == "text/xml"
        || mime == "application/xml"
}

/// Fetch one linked stylesheet on its own detached worker (M4.3). One
/// `Msg::Stylesheet` goes out and nothing else — a stylesheet has no visible
/// byte counter, so there is no `Loading` progress to report.
///
/// The CSS is parsed here for the same reason the HTML is: the worker owns the
/// bytes and is already off the UI thread. Anything short of a successful
/// response yields `None`; the page is then styled by whatever else it has,
/// which is what "render unstyled, then restyle" means when a sheet is missing
/// rather than slow.
pub fn spawn_stylesheet(id: FetchId, slot: usize, url: String, tx: Sender<Msg>) {
    thread::spawn(move || {
        let sheet = match get(&url) {
            // A 404's body is an error page, not CSS; parsing it would put
            // whatever HTML-shaped garbage recovers into the cascade.
            Ok((status, body)) if (200..300).contains(&status) => {
                // Charset: lossy UTF-8, the same seam `html::decode_body`
                // documents. `@charset` and the HTTP header wait for a page
                // that needs them.
                Some(css::parse(&String::from_utf8_lossy(&body)))
            }
            _ => None,
        };
        let _ = tx.send(Msg::Stylesheet { id, slot, sheet });
    });
}

/// A whole response in one go, no progress reporting. Used for subresources,
/// where there is no byte counter to feed.
fn get(url: &str) -> Result<(u16, Vec<u8>), String> {
    let mut resp = client()?.get(url).send().map_err(describe)?;
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    resp.read_to_end(&mut body).map_err(describe)?;
    Ok((status, body))
}

/// One blocking client. Built on the worker, never on the UI thread; defaults
/// follow redirects and (via the gzip feature) transparently decompress.
fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .build()
        .map_err(describe)
}

/// The whole request, run on the worker. `Ok(Some(Loaded))` on success,
/// `Ok(None)` if the channel closed mid-stream, `Err((url, reason))` on any
/// failure (bad URL, DNS, connect, TLS, mid-body disconnect). The error's url
/// is the most precise one known at the point of failure: the requested URL
/// until headers arrive, the post-redirect final URL after.
fn fetch(id: FetchId, url: &str, tx: &Sender<Msg>) -> Result<Option<Msg>, (String, String)> {
    // Timed on the worker, where the request happens: the duration reaches
    // the app as message data, so the app never reads the clock. The span is
    // the whole request — client build → last body byte.
    let started = Instant::now();
    // Built on the worker (see `client`), so the UI thread never touches
    // reqwest.
    let client = client().map_err(|reason| (url.to_string(), reason))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| (url.to_string(), describe(e)))?;
    let status = resp.status().as_u16();
    // The final URL, after redirects — what M1.5's URL bar should display.
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let mut body = Vec::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| (final_url.clone(), describe(e)))?;
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
        elapsed: started.elapsed(),
        content_type,
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
    fn serve_once(response: Vec<u8>) -> SocketAddr {
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
            let _ = stream.write_all(&response);
        });
        addr
    }

    /// Serve a redirect from `/start` to `/final`, then `final_response` on
    /// the follow-up request. `Connection: close` on the redirect forces the
    /// client onto a second connection, so each request is its own accept.
    fn serve_redirect_then(final_response: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 512];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let response: &[u8] = if req.starts_with(b"GET /start") {
                    b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    final_response
                };
                let _ = stream.write_all(response);
            }
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

    /// Split a successful worker's message stream into (progress, loaded,
    /// parsed), asserting the M2.3 shape: `Loading`* then `Loaded` then
    /// exactly one `Parsed`.
    fn split_success(msgs: &[Msg]) -> (&[Msg], &Msg, &Msg) {
        let (parsed, rest) = msgs.split_last().expect("worker sent nothing");
        assert!(
            matches!(parsed, Msg::Parsed { .. }),
            "expected Parsed last, got {parsed:?}"
        );
        let (loaded, progress) = rest.split_last().expect("no Loaded before Parsed");
        assert!(
            matches!(loaded, Msg::Loaded { .. }),
            "expected Loaded before Parsed, got {loaded:?}"
        );
        (progress, loaded, parsed)
    }

    /// Serve `body` as CSS on an ephemeral port, once.
    fn serve_css(status_line: &str, body: &'static str) -> SocketAddr {
        serve_once(
            format!(
                "{status_line}\r\nContent-Type: text/css\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )
    }

    /// Accept **two** connections before answering either. A sequential
    /// fetcher never opens the second one, so it waits forever on a response
    /// that will not come and the test times out. This is the structural test
    /// for "stylesheets are fetched in parallel" (CLAUDE.md: the UI thread
    /// never blocks) — a wall-clock assertion would only be a guess.
    fn serve_two_but_only_after_both_connect(body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut waiting = Vec::new();
            for _ in 0..2 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                waiting.push(stream);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            for mut stream in waiting {
                let mut req = Vec::new();
                let mut buf = [0u8; 512];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn a_stylesheet_worker_sends_one_parsed_sheet() {
        let addr = serve_css("HTTP/1.1 200 OK", "a:link { color: #000 } p { color: red }");
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(FetchId(7), 3, format!("http://{addr}/news.css"), tx);

        let msgs = drain(rx);
        assert_eq!(msgs.len(), 1, "one message and nothing else: {msgs:?}");
        match &msgs[0] {
            Msg::Stylesheet { id, slot, sheet } => {
                assert_eq!(*id, FetchId(7));
                // The slot travels with the sheet: it is the document position
                // the cascade needs, not the arrival order.
                assert_eq!(*slot, 3);
                let sheet = sheet.as_ref().expect("a 200 text/css body must parse");
                assert_eq!(sheet.rules.len(), 2);
            }
            other => panic!("expected Stylesheet, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_or_unsuccessful_sheet_resolves_to_none() {
        // A 404's body is an error page, not CSS: parsing it would feed
        // whatever the recovery path salvages into the cascade.
        let addr = serve_css("HTTP/1.1 404 Not Found", "<html>nope</html>");
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(FetchId(8), 0, format!("http://{addr}/missing.css"), tx);
        assert!(matches!(
            drain(rx).as_slice(),
            [Msg::Stylesheet {
                id: FetchId(8),
                slot: 0,
                sheet: None
            }]
        ));

        // A closed port is the same story: a degraded page, not an error page.
        let dead = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(FetchId(9), 1, format!("http://{dead}/x.css"), tx);
        assert!(matches!(
            drain(rx).as_slice(),
            [Msg::Stylesheet {
                id: FetchId(9),
                slot: 1,
                sheet: None
            }]
        ));
    }

    #[test]
    fn stylesheets_are_fetched_in_parallel() {
        // The server answers nobody until both workers have connected, so this
        // test cannot pass if the fetches happen one after the other — it
        // deadlocks and `drain`'s timeout fails it.
        let addr = serve_two_but_only_after_both_connect("p { color: red }");
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(FetchId(10), 0, format!("http://{addr}/a.css"), tx.clone());
        spawn_stylesheet(FetchId(10), 1, format!("http://{addr}/b.css"), tx);

        let msgs = drain(rx);
        assert_eq!(msgs.len(), 2, "one message per sheet: {msgs:?}");
        let mut slots: Vec<usize> = msgs
            .iter()
            .map(|m| match m {
                Msg::Stylesheet { slot, sheet, .. } => {
                    assert!(sheet.is_some(), "both sheets must parse");
                    *slot
                }
                other => panic!("expected Stylesheet, got {other:?}"),
            })
            .collect();
        slots.sort_unstable();
        assert_eq!(slots, vec![0, 1]);
    }

    #[test]
    fn local_server_success_sends_loading_then_loaded_then_parsed() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world"
                .to_vec(),
        );
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(1), url.clone(), tx);

        let msgs = drain(rx);
        let (progress, loaded, parsed) = split_success(&msgs);
        assert!(
            !progress.is_empty(),
            "expected at least one Loading before Loaded"
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
        // The worker measures the whole request; even against localhost the
        // elapsed time can never be zero.
        let Msg::Loaded { elapsed, .. } = loaded else {
            unreachable!()
        };
        assert!(
            *elapsed > Duration::ZERO,
            "the worker must measure the request"
        );
        assert_eq!(
            *loaded,
            Msg::Loaded {
                id: FetchId(1),
                url,
                status: 200,
                body: b"hello world".to_vec(),
                elapsed: *elapsed,
                content_type: None,
            }
        );
        // The Parsed message carries the body's tree, built on the worker.
        let Msg::Parsed { id, dom, .. } = parsed else {
            unreachable!()
        };
        assert_eq!(*id, FetchId(1));
        assert!(
            html::debug_tree(dom).contains("#text \"hello world\""),
            "the parsed tree must contain the body text:\n{}",
            html::debug_tree(dom)
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
    fn mid_body_failure_reports_the_post_redirect_url() {
        // Headers promise 100 bytes; the connection dies after 5.
        let addr = serve_redirect_then(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nhello",
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(4), format!("http://{addr}/start"), tx);

        let msgs = drain(rx);
        let (last, progress) = msgs.split_last().expect("worker sent nothing");
        // The bytes that did arrive before the cut may or may not have
        // produced Loading messages; only the terminal message is pinned.
        for msg in progress {
            assert!(
                matches!(msg, Msg::Loading { id: FetchId(4), .. }),
                "expected only Loading before NetError, got {msg:?}"
            );
        }
        match last {
            Msg::NetError { id, url, reason } => {
                assert_eq!(*id, FetchId(4));
                assert_eq!(
                    *url,
                    format!("http://{addr}/final"),
                    "a failure after redirects must report the final URL"
                );
                assert!(!reason.is_empty(), "reason must be human-readable");
            }
            other => panic!("expected NetError last, got {other:?}"),
        }
    }

    #[test]
    fn success_after_redirect_reports_the_final_url() {
        let addr = serve_redirect_then(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(5), format!("http://{addr}/start"), tx);

        let msgs = drain(rx);
        let (_, loaded, _) = split_success(&msgs);
        let Msg::Loaded { elapsed, .. } = loaded else {
            unreachable!()
        };
        assert_eq!(
            *loaded,
            Msg::Loaded {
                id: FetchId(5),
                url: format!("http://{addr}/final"),
                status: 200,
                body: b"hello".to_vec(),
                elapsed: *elapsed,
                content_type: None,
            },
            "Loaded must carry the post-redirect URL and the final status"
        );
    }

    #[test]
    fn gzip_body_is_transparently_decompressed() {
        // `printf 'hello world' | gzip -n -9`, embedded so the test stays
        // offline and dependency-free.
        const GZ: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xcb, 0x48, 0xcd, 0xc9,
            0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01, 0x00, 0x85, 0x11, 0x4a, 0x0d, 0x0b,
            0x00, 0x00, 0x00,
        ];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            GZ.len()
        )
        .into_bytes();
        response.extend_from_slice(GZ);
        let addr = serve_once(response);
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(6), url.clone(), tx);

        let msgs = drain(rx);
        let (_, loaded, _) = split_success(&msgs);
        let Msg::Loaded { elapsed, .. } = loaded else {
            unreachable!()
        };
        assert_eq!(
            *loaded,
            Msg::Loaded {
                id: FetchId(6),
                url,
                status: 200,
                body: b"hello world".to_vec(),
                elapsed: *elapsed,
                content_type: None,
            },
            "the body must arrive decompressed, not as gzip bytes"
        );
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

    #[test]
    fn non_document_responses_send_loaded_without_parsed() {
        // HTTP error: body is still delivered (dump/curl semantics) but never
        // parsed into a DOM the TUI would style.
        let addr = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .to_vec(),
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(20), format!("http://{addr}/"), tx);
        let msgs = drain(rx);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Msg::Loaded { status: 404, .. })),
            "expected Loaded for 404: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::Parsed { .. })),
            "404 must not produce Parsed: {msgs:?}"
        );

        // 200 with a non-document content-type: same shape.
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\x89PNG"
                .to_vec(),
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(FetchId(21), format!("http://{addr}/"), tx);
        let msgs = drain(rx);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                Msg::Loaded {
                    status: 200,
                    content_type: Some(ct),
                    ..
                } if ct.starts_with("image/png")
            )),
            "expected Loaded image/png: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::Parsed { .. })),
            "image/png must not produce Parsed: {msgs:?}"
        );
    }

    #[test]
    fn is_document_matrix() {
        assert!(is_document(200, None));
        assert!(is_document(200, Some("text/html; charset=utf-8")));
        assert!(!is_document(404, Some("text/html")));
        assert!(!is_document(200, Some("image/png")));
    }
}
