//! Integration tests for the headless CLI modes (`--dump`, `--dump-dom`,
//! `--dump-text`, `--timing`), running the real binary against a local one-shot
//! server. Tests never hit the network (CLAUDE.md conventions).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Serve `count` requests on one ephemeral port, answering each with
/// `respond(request_text)`. One port means one origin, which is what a
/// same-origin round trip needs; `serve_once` cannot do it because a
/// subresource is a second request.
fn serve_site(count: usize, respond: impl Fn(&str) -> Vec<u8> + Send + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for _ in 0..count {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(&respond(&String::from_utf8_lossy(&req)));
        }
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
fn every_headless_mode_ignores_the_bookmark_path() {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for mode in [
        "--dump",
        "--dump-dom",
        "--dump-text",
        "--dump-boxes",
        "--dump-js",
        "--timing",
    ] {
        let addr = serve_once(response_with_body("200 OK", b"<p>headless</p>"));
        let path = std::env::temp_dir()
            .join(format!(
                "yata-headless-bookmarks-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .join("never/bookmarks");
        let output = Command::new(env!("CARGO_BIN_EXE_yata"))
            .env("YATA_BOOKMARKS_PATH", &path)
            .args([mode, &format!("http://{addr}/")])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!path.exists(), "{mode} created the bookmark file");
        assert!(
            !path.parent().unwrap().exists(),
            "{mode} created bookmark directories"
        );
    }
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
fn dump_text_shows_a_form_field_as_a_field() {
    // M11.8: the headless dumps show what the TUI shows, so a page with a form
    // has to dump one — a control drawn as nothing at all is what `--dump-text`
    // used to show for every field on the ladder.
    let body = r#"<body style="margin: 0"><p style="margin: 0">Search:                   <input type="text" name="q" size="17" value="typed"><button>Go</button>                  <input type="hidden" name="t" value="x"></p></body>"#;
    let addr = serve_once(response_with_body("200 OK", body.as_bytes()));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    // The frame is glyphs, so a dump of a field reads as a field: 17 cells of
    // value between brackets, and a bracketed label for the button. The hidden
    // input takes no cells at all.
    assert_eq!(
        text.lines().next(),
        Some("Search: [typed            ][Go]"),
        "{text}"
    );
}

#[test]
fn dump_boxes_names_a_field_box_and_its_geometry() {
    // F3's other half (M11.8): a control's box says which element it came from,
    // what it is showing, and how many cells the page asked for — `size="17"`
    // is 17 of them, and the hidden input has no box to name.
    let body = r#"<body style="margin: 0"><p style="margin: 0">                  <input type="text" name="q" size="17" value="typed">                  <input type="hidden" name="t" value="x"></p></body>"#;
    let addr = serve_once(response_with_body("200 OK", body.as_bytes()));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(r#"<input …> field value "typed"  x=1 y=0 w=17 h=1"#),
        "no field box with geometry:\n{text}"
    );
    assert_eq!(
        text.matches("field").count(),
        1,
        "the hidden input got a box:\n{text}"
    );
}

#[test]
fn dump_boxes_reports_positioned_final_geometry() {
    let body = br#"<style>*{margin:0}.card{position:relative;padding:1em}.close{position:absolute;top:1em;right:1em;width:8px}</style><div class=card><a class=close href=/x>x</a><p>copy</p></div>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let boxes = String::from_utf8(out.stdout).unwrap();
    // The root is 80 cells wide; 1em is two horizontal cells. The close link
    // has a one-cell width and sits two cells from the card padding end.
    assert!(
        boxes.contains("<a.close href=\"/x\">  x=77 y=1 w=1 h=1"),
        "{boxes}"
    );
    assert!(boxes.contains("#text \"copy\"  x=2 y=1 w=4 h=1"), "{boxes}");
}

#[test]
fn dump_boxes_exposes_resolved_grid_tracks_and_item_rectangles() {
    let body = br#"<style>*{margin:0}.g{display:grid;grid-template-columns:8px 1fr;gap:1em}.wide{grid-column:1 / span 2}</style><div class=g><p>rail</p><p>article</p><p class=wide>footer</p></div>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let boxes = String::from_utf8(out.stdout).unwrap();
    assert!(boxes.contains("<div.g> grid cols=[1,"), "{boxes}");
    // The one-cell rail wraps its four characters, making the auto row four
    // lines high; the row gap then puts the spanning footer at line five.
    assert!(boxes.contains("rows=[4, 1]"), "{boxes}");
    assert!(boxes.contains("#text \"article\"  x=3 y=0"), "{boxes}");
    assert!(boxes.contains("#text \"footer\"  x=0 y=5"), "{boxes}");

    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.first().copied(), Some("r  article"), "{text}");
    assert_eq!(lines.get(5).copied(), Some("footer"), "{text}");
}

#[test]
fn dump_text_and_boxes_expose_choice_controls_without_option_prose() {
    let body = br#"<body style="margin:0"><p style="margin:0">Choice <input type=checkbox checked> <select><option>One</option><option selected>Two</option></select></p></body>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().lines().next(),
        Some("Choice [x] [Two v]")
    );

    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(r#"<input …> field checkbox checked "x"  x=8 y=0 w=1 h=1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"<select> field select "One Two"  x=12 y=0 w=5 h=1"#),
        "{text}"
    );
    assert_eq!(text.matches(" field ").count(), 2, "{text}");
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn dump_text_and_boxes_expose_table_roles_in_the_same_tree() {
    let body = br#"<body style="margin:0"><table><tr><th>Key</th><th>Value</th></tr><tr><td><a href="/docs">docs</a></td><td><input value="go" size="2"></td></tr></table></body>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().next(), Some("Key Value"), "{text}");
    assert_eq!(text.lines().nth(1), Some("docs[go]"), "{text}");

    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let boxes = String::from_utf8(out.stdout).unwrap();
    for role in ["table <table>", "table-row <tr>", "table-cell <td>"] {
        assert!(boxes.contains(role), "missing {role:?} in:\n{boxes}");
    }
    assert!(boxes.contains("table <table>  x=0 y=0 w=9 h=2"), "{boxes}");
    assert!(
        boxes.contains("table-cell <th>  x=0 y=0 w=4 h=1"),
        "{boxes}"
    );
    assert!(
        boxes.contains("table-cell <th>  x=4 y=0 w=5 h=1"),
        "{boxes}"
    );
    assert!(boxes.contains("field value \"go\""), "{boxes}");
}

#[test]
fn dump_modes_expose_final_spanning_cell_geometry() {
    let body = br#"<body style="margin:0"><table><tr><th colspan="2">Language</th><th>Year</th></tr><tr><td rowspan="2">Rust</td><td>stable</td><td>2015</td></tr><tr><td>edition</td><td>2024</td></tr></table></body>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.lines()
            .next()
            .is_some_and(|line| line.contains("Language") && line.contains("Year")),
        "{text}"
    );
    assert!(
        text.contains("Ruststable") && text.contains("2015"),
        "{text}"
    );
    assert!(text.contains("edition2024"), "{text}");

    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let boxes = String::from_utf8(out.stdout).unwrap();
    assert!(boxes.contains("table-cell <th …>"), "{boxes}");
    assert!(boxes.contains("table-cell <td …>"), "{boxes}");
    assert!(
        boxes.contains("#text \"edition\""),
        "the row-spanned first column displaced later cells incorrectly:\n{boxes}"
    );
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
fn dump_boxes_exposes_fixed_and_sticky_annotations() {
    let body = br#"<style>
        .fixed { position: fixed; top: 0 }
        .sticky { position: sticky; top: 0 }
    </style><div class=fixed>fixed</div><div class=sticky>sticky</div>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-boxes", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let boxes = String::from_utf8(out.stdout).unwrap();
    assert!(boxes.contains("fixed viewport"), "{boxes}");
    assert!(boxes.contains("sticky top 0 range"), "{boxes}");
}

#[test]
fn dump_text_includes_fixed_and_sticky_static_content() {
    let body = br#"<style>
        .fixed { position: fixed; top: 0 }
        .sticky { position: sticky; top: 0 }
        p { margin: 0 }
    </style><div class=fixed>fixed</div><div class=sticky>sticky</div><p>flow</p>"#;
    let addr = serve_once(response_with_body("200 OK", body));
    let out = yata(&["--dump-text", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("fixed"), "{text}");
    assert!(text.contains("sticky"), "{text}");
    assert!(text.contains("flow"), "{text}");
}

#[test]
fn dump_js_runs_the_page_scripts_in_document_order() {
    // Three scripts, the middle one throwing: the third must still run, and
    // the page must still exit 0 — a broken script is a degraded page, not an
    // error page.
    let addr = serve_once(response_with_body(
        "200 OK",
        b"<script>var n = 1;</script>\
          <p>prose in between</p>\
          <script>n.missing.deeper;</script>\
          <script>n + 41;</script>",
    ));
    let out = yata(&["--dump-js", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dumped = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = dumped.lines().collect();
    // Two sections: one line per script, then the console pane in order
    // (M10.7). The thrown exception appears in both — once as the script's
    // outcome, once as the console entry a reader would see on `F5`.
    assert_eq!(
        lines.len(),
        4,
        "script results then console, got:\n{dumped}"
    );
    assert_eq!(lines[0], "inline#1 ok undefined");
    assert!(
        lines[1].starts_with("inline#2 error 1: "),
        "the error line must carry its line number, got {:?}",
        lines[1]
    );
    assert_eq!(lines[2], "inline#3 ok 42");
    assert!(
        lines[3].starts_with("error inline#2:1: "),
        "the console entry must carry level, source and line, got {:?}",
        lines[3]
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dump_js_shows_the_cookies_a_page_sets_and_reads() {
    // M11.6: the dump has to behave the way the TUI does, or `--dump-js` — what
    // the M11.25 ladder sweep reads — would report that a page's cookies do
    // nothing. The jar is the dump's own and dies with the process, so what one
    // script writes the next one reads and nothing survives the run.
    let addr = serve_once(response_with_body(
        "200 OK",
        b"<script>document.cookie = 'a=1; path=/'; document.cookie = 'rubbish';</script>\
          <script>document.cookie;</script>",
    ));
    let out = yata(&["--dump-js", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dumped = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = dumped.lines().collect();
    assert_eq!(
        lines,
        [
            // An assignment evaluates to what was assigned, even the one that
            // was ignored — the warning below is where the ignoring shows.
            "inline#1 ok \"rubbish\"",
            "inline#2 ok \"a=1\"",
            "warn  ignored document.cookie = \"rubbish\": it has no name=value pair",
        ],
        "the dump does not show what the TUI would:\n{dumped}"
    );
}

#[test]
fn dump_js_shows_a_server_set_cookie_going_back_out_on_the_wire() {
    // M11.7's round trip, end to end through the real binary: the document
    // response sets two cookies, the page's own script reads back the one it
    // is allowed to, and the `<script src>` request that follows carries
    // **both** — the server echoes the `Cookie:` header it received into the
    // JavaScript it serves, which is the only way to see the wire from here.
    //
    // Two requests, one port, so the script is same-origin with the page.
    let addr = serve_site(2, |request| {
        if request.starts_with("GET /lib.js") {
            let seen = request
                .lines()
                .find_map(|line| line.strip_prefix("cookie: "))
                .unwrap_or("<none>")
                .trim()
                .to_string();
            response_with_body(
                "200 OK",
                format!("console.log('server saw: {seen}');").as_bytes(),
            )
        } else {
            let body = b"<script>console.log('script sees: ' + document.cookie);</script>\
                         <script src=/lib.js></script>";
            let mut resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Set-Cookie: sid=abc; Path=/; HttpOnly\r\n\
                 Set-Cookie: theme=dark; Path=/\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            resp.extend_from_slice(body);
            resp
        }
    });

    let out = yata(&["--dump-js", &format!("http://{addr}/")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dumped = String::from_utf8(out.stdout).unwrap();
    assert!(
        dumped.contains("script sees: theme=dark"),
        "the page's script should see the non-HttpOnly cookie and only that:\n{dumped}"
    );
    assert!(
        dumped.contains("server saw: sid=abc; theme=dark"),
        "the subresource request did not carry the session:\n{dumped}"
    );
}

#[test]
fn dump_js_of_a_page_without_script_prints_nothing() {
    let addr = serve_once(response_with_body("200 OK", b"<p>just prose</p>"));
    let out = yata(&["--dump-js", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "expected no lines, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn dump_js_against_a_closed_port_reports_the_reason_and_exits_1() {
    let addr = serve_once(Vec::new());
    // Let the one-shot server bind and immediately be gone.
    let out = yata(&["--dump-js", &format!("http://{addr}/")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty(), "the reason must reach stderr");
}

#[test]
fn a_headless_flag_without_a_url_is_a_usage_error() {
    for flags in [
        &["--dump"][..],
        &["--dump-dom"][..],
        &["--dump-text"][..],
        &["--dump-boxes"][..],
        &["--dump-js"][..],
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
        ["--dump-js", "--dump-text"],
        ["--dump-js", "--timing"],
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

// ---- redirects through the loop (M11.7a) ----------------------------------

/// A login-shaped chain: `/login` answers 302 with a session cookie and points
/// at `/app`, which echoes the `Cookie:` header it received.
///
/// The `Connection: close` on every response is what makes each request its own
/// accept, which is also what the hop really costs: a fresh connection per hop
/// instead of one the library reused inside a worker.
fn serve_login_flow(landing: &'static str) -> std::net::SocketAddr {
    serve_site(2, move |request| {
        if request.starts_with("GET /login") {
            b"HTTP/1.1 302 Found\r\nLocation: /app\r\n\
              Set-Cookie: sid=abc; Path=/\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec()
        } else {
            let seen = request
                .lines()
                .find_map(|line| line.strip_prefix("cookie: "))
                .unwrap_or("<none>")
                .trim()
                .to_string();
            response_with_body("200 OK", landing.replace("{cookie}", &seen).as_bytes())
        }
    })
}

#[test]
fn a_redirect_hands_out_a_session_and_the_next_request_carries_it() {
    // **The round trip this task exists for**, end to end through the real
    // binary. Before M11.7a the hop happened inside the worker: its
    // `Set-Cookie` never reached the jar, and the request that followed carried
    // the header computed for the URL the reader typed. Both halves are here —
    // the landing page prints what the server actually received.
    let addr = serve_login_flow("<p>server saw: {cookie}</p>");
    let out = yata(&["--dump-text", &format!("http://{addr}/login")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.lines().next(),
        Some("server saw: sid=abc"),
        "the hop's cookie did not reach the request it authorises:\n{text}"
    );
}

#[test]
fn dump_of_a_redirect_is_the_final_body_and_nothing_else() {
    // `--dump`'s stdout is bytes and only bytes: a hop must not add one, and
    // the 302's own empty body must not be what lands.
    let addr = serve_login_flow("landed\n");
    let out = yata(&["--dump", &format!("http://{addr}/login")]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"landed\n");
    assert!(out.stderr.is_empty());
}

#[test]
fn dump_js_shows_a_script_reading_a_cookie_a_hop_set() {
    // The dumps behave the way the TUI does, hops included: `--dump-js` is what
    // M11.25's ladder sweep reads, so a cookie set by a 302 the reader never
    // saw has to be in the jar the page's script asks.
    let addr = serve_login_flow(
        "<script>console.log('script sees: ' + document.cookie);</script>\
         <p>server saw: {cookie}</p>",
    );
    let out = yata(&["--dump-js", &format!("http://{addr}/login")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dumped = String::from_utf8(out.stdout).unwrap();
    assert!(
        dumped.contains("script sees: sid=abc"),
        "a hop's cookie never reached the dump's jar:\n{dumped}"
    );
}

#[test]
fn timing_of_a_redirect_reports_the_whole_chain() {
    // Deliverable 5, where getting it wrong is silent: the fetch row has to be
    // both hops, not the last one. The server sleeps 40 ms on the redirect, so
    // a fetch row under it could only have come from timing the landing page
    // alone.
    let addr = serve_site(2, |request| {
        if request.starts_with("GET /login") {
            thread::sleep(std::time::Duration::from_millis(40));
            b"HTTP/1.1 302 Found\r\nLocation: /app\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n"
                .to_vec()
        } else {
            response_with_body("200 OK", b"<p>landed</p>")
        }
    });
    let out = yata(&["--timing", &format!("http://{addr}/login")]);
    assert_eq!(out.status.code(), Some(0));
    let rows = String::from_utf8(out.stderr).unwrap();
    let fetch = rows
        .lines()
        .find(|row| row.trim_start().starts_with("fetch"))
        .expect("a fetch row");
    // `fetch 59.4 ms`
    let ms: f64 = fetch
        .split_whitespace()
        .nth(1)
        .and_then(|word| word.parse().ok())
        .unwrap_or_else(|| panic!("no milliseconds in {fetch:?}"));
    assert!(
        ms >= 40.0,
        "the fetch row hid a slow hop inside a fast landing page: {fetch:?}"
    );
}

#[test]
fn a_headless_redirect_loop_stops_at_the_bound_instead_of_hanging() {
    // The dumps follow the same chain with the same constant the TUI stops at
    // (`app::MAX_REDIRECTS`), so a page that bounces forever is an exit code
    // and a reason on stderr rather than a command that never returns.
    // 21 responses: one more than the bound will ever ask for.
    let addr = serve_site(21, |_| {
        b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\
          Connection: close\r\n\r\n"
            .to_vec()
    });
    let out = yata(&["--dump", &format!("http://{addr}/start")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "a loop wrote a body");
    let reason = String::from_utf8(out.stderr).unwrap();
    assert!(reason.contains("too many redirects"), "{reason}");
}
