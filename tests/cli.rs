//! Integration tests for the headless CLI modes (`--dump`, `--dump-dom`,
//! `--dump-text`, `--timing`), running the real binary against a local one-shot
//! server. Tests never hit the network (CLAUDE.md conventions).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::{Command, Output};
use std::thread;
use unicode_width::UnicodeWidthStr;

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
fn dump_dom_prints_the_parsed_tree_to_stdout() {
    let addr = serve_once(response_with_body("200 OK", b"<title>T</title><p>hi</p>"));
    let out = yata(&["--dump-dom", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8(out.stdout).unwrap();
    // The synthesized spine plus real content, indented — the same shape the
    // in-crate `debug_tree` snapshot tests pin.
    for line in ["#document", "  <html>", "    <head>", "    <body>"] {
        assert!(
            tree.lines().any(|l| l == line),
            "missing {line:?} in:\n{tree}"
        );
    }
    assert!(tree.contains("<p>"), "tree was:\n{tree}");
    assert!(tree.contains("#text \"hi\""), "tree was:\n{tree}");
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dump_text_prints_the_laid_out_page_to_stdout() {
    // A heading, a wrapping paragraph and a list: enough to show that the
    // output is laid out (blank line between blocks, bullet, wrap at the fixed
    // 80-cell column) rather than raw text with the tags stripped.
    // The CJK paragraph is what makes the width guard below mean anything: at
    // two cells per character it breaks any check counting chars instead of
    // cells, which is the bug CLAUDE.md's layout invariant exists to prevent.
    let body = format!(
        "<h1>Title</h1><p>{}</p><p>{}</p><ul><li>one</li><li>two</li></ul>",
        "word ".repeat(20),
        "文字".repeat(30)
    );
    let addr = serve_once(response_with_body("200 OK", body.as_bytes()));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Title");
    assert_eq!(lines[1], "", "blocks are separated by a blank line");
    // Every line fits the fixed column — measured in cells, so the 2-cell CJK
    // above is held to 40 characters a line, not 80.
    assert!(
        lines.iter().all(|l| l.width() <= 80),
        "a line ran past the fixed column: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains('文')),
        "the wide-character paragraph never made it into the output"
    );
    // List markers sit inside the UA padding-left indent (M5 box model).
    assert!(
        text.contains("• one") && text.contains("• two"),
        "text was:\n{text}"
    );
    // Styles are dropped, not rendered as markers, and no escape sequence
    // reaches a pipe — attributes belong to the renderer alone.
    assert!(!text.contains('['), "style markers reached stdout:\n{text}");
    assert!(
        !text.contains('\u{1b}'),
        "an escape reached stdout:\n{text}"
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn timing_prints_every_pipeline_stage_to_stderr_only() {
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
    // Every stage a page passes through, in pipeline order — `style` included
    // since M4, because a restyle is 41 ms on a large page and an instrument
    // that cannot show it is not an instrument.
    let stages = ["fetch", "parse", "style", "layout", "frame"];
    let mut seen = Vec::new();
    for stage in stages {
        let row = table
            .lines()
            .find(|l| l.starts_with(stage))
            .unwrap_or_else(|| panic!("no {stage} row in {table:?}"));
        assert!(row.ends_with("ms"), "{stage} row was {row:?}");
        seen.push(table.lines().position(|l| l.starts_with(stage)).unwrap());
    }
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "rows must come in pipeline order: {table:?}"
    );
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
fn dump_boxes_prints_the_box_tree_to_stdout() {
    // The layout stage's headless hook (M9.1): one line per box with its
    // geometry, indented like the tree — the same text F3 shows and the same
    // text `tests/layout.rs` compares its goldens against.
    let body = r#"<body style="margin: 0"><p style="margin: 0">hi</p>
<img src="pic.png" width="80" height="64" alt="cat"></body>"#;
    let addr = serve_once(response_with_body("200 OK", body.as_bytes()));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("<html>  x=0 y=0 w=80"),
        "the dump must start at the root box, at the fixed 80-cell column:\n{text}"
    );
    assert!(
        text.contains(r#"#text "hi"  x=0 y=0 w=2 h=1"#),
        "no text box with geometry:\n{text}"
    );
    // Images are discovered headlessly, so the dump shows the boxes the screen
    // shows: 80px/8 = 10 cells wide, 64px/16 = 4 lines tall.
    assert!(
        text.contains(r#"img "cat" http://"#) && text.contains("w=10 h=4"),
        "no image box:\n{text}"
    );
    assert!(
        !text.contains('\u{1b}'),
        "an escape reached stdout:\n{text}"
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_headless_flag_without_a_url_is_a_usage_error() {
    for flags in [
        &["--dump"][..],
        &["--dump-dom"][..],
        &["--dump-text"][..],
        &["--dump-boxes"][..],
        &["--timing"][..],
    ] {
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
fn two_headless_flags_together_is_a_usage_error() {
    // A URL is present; the flag combination alone must fail, before any
    // fetch is attempted.
    for flags in [
        ["--dump", "--timing"],
        ["--dump", "--dump-dom"],
        ["--dump-dom", "--timing"],
        ["--dump", "--dump-text"],
        ["--dump-dom", "--dump-text"],
        ["--dump-text", "--timing"],
        ["--dump-boxes", "--dump-text"],
        ["--dump-boxes", "--timing"],
    ] {
        let out = yata(&[flags[0], flags[1], "http://127.0.0.1:9/"]);
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
