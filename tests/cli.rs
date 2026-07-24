//! Integration tests for the headless CLI modes (`--dump`, `--timing`),
//! running the real binary against a local one-shot server. Tests never hit
//! the network (CLAUDE.md conventions).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::{Command, Output};
use std::thread;

/// Serve one canned HTTP response on an ephemeral local port, from a test
/// thread. Duplicated from `src/net/fetch.rs`'s tests: integration tests
/// cannot reach `#[cfg(test)]` code inside the crate.
fn serve_once(response: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Read the request through the blank line before answering, so the
        // client is never racing a response to an unsent request.
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

fn response_with_body(status_line: &str, body: &[u8]) -> Vec<u8> {
    let mut resp = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn yata(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yata"))
        .args(args)
        .output()
        .expect("failed to run the yata binary")
}

#[test]
fn dump_writes_the_served_body_verbatim() {
    // The non-UTF-8 byte (0xff) pins "raw bytes, not a lossy decode"; the
    // exact equality pins "no trailing newline".
    let body = b"<html>\xff</html>".to_vec();
    let addr = serve_once(response_with_body("200 OK", &body));
    let out = yata(&["--dump", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout, body,
        "stdout must be byte-identical to the body"
    );
    assert!(
        !out.stdout.contains(&0x1b),
        "no escape sequences may reach stdout"
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dump_of_a_404_still_dumps_the_body() {
    let addr = serve_once(response_with_body("404 Not Found", b"not here"));
    let out = yata(&["--dump", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "curl semantics: a 404 page is still a page"
    );
    assert_eq!(out.stdout, b"not here");
}

#[test]
fn dump_against_a_closed_port_reports_the_reason_and_exits_1() {
    // Bind then drop: the freed ephemeral port refuses connections.
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let out = yata(&["--dump", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "a failed dump must write nothing to stdout"
    );
    assert!(!out.stderr.is_empty(), "the reason must reach stderr");
}

#[test]
fn timing_prints_fetch_and_frame_rows_to_stderr_only() {
    let addr = serve_once(response_with_body("200 OK", b"<html>hello</html>"));
    let out = yata(&["--timing", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must stay empty: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let table = String::from_utf8(out.stderr).unwrap();
    let fetch = table
        .lines()
        .find(|l| l.starts_with("fetch"))
        .unwrap_or_else(|| panic!("no fetch row in {table:?}"));
    assert!(fetch.ends_with("ms"), "fetch row was {fetch:?}");
    let frame = table
        .lines()
        .find(|l| l.starts_with("frame"))
        .unwrap_or_else(|| panic!("no frame/render row in {table:?}"));
    assert!(frame.ends_with("ms"), "frame row was {frame:?}");
}

#[test]
fn timing_against_a_closed_port_reports_the_reason_and_exits_1() {
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let out = yata(&["--timing", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty(), "the reason must reach stderr");
}

#[test]
fn a_headless_flag_without_a_url_is_a_usage_error() {
    for flags in [&["--dump"][..], &["--timing"][..]] {
        let out = yata(flags);
        assert_eq!(out.status.code(), Some(2), "flags: {flags:?}");
        assert!(out.stdout.is_empty());
        assert_eq!(
            out.stderr.iter().filter(|&&b| b == b'\n').count(),
            1,
            "exactly one usage line, got {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn dump_and_timing_together_is_a_usage_error() {
    // A URL is present; the flag combination alone must fail, before any
    // fetch is attempted.
    let out = yata(&["--dump", "--timing", "http://127.0.0.1:9/"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr.iter().filter(|&&b| b == b'\n').count(),
        1,
        "exactly one usage line, got {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
