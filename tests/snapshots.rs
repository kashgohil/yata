//! Snapshot tests (PLAN.md M5): fixture HTML → rendered cell grid text.
//!
//! These pin what the reader sees for the lower rungs of the test ladder at a
//! fixed 80×24 column. Update only when rendering is *supposed* to change
//! (CLAUDE.md), and say so in the PR.

use std::fs;
use unicode_width::UnicodeWidthStr;
use yata::html;
use yata::layout::{self, Hidden};
use yata::style;

fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture must be committed")
}

/// Render a page to plain lines at width 80, joining with newlines. Styles are
/// the UA sheet plus any inline blocks; linked sheets are not fetched.
fn render_text(html: &str, width: u16) -> String {
    let dom = html::parse(html);
    let sheets = style::sources::inline_sheets(&dom);
    let refs: Vec<_> = sheets.iter().collect();
    let styles = style::style_tree(&dom, &refs);
    let lines = layout::layout(&dom, &styles, width, Hidden::Respect);
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_snapshot(name: &str, got: &str) {
    let path = format!(
        "{}/tests/fixtures/snapshots/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        if let Some(parent) = std::path::Path::new(&path).parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, got).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing snapshot {path}; run with UPDATE_SNAPSHOTS=1 to create it")
    });
    assert_eq!(
        got, expected,
        "snapshot {name} drifted — review the diff, then UPDATE_SNAPSHOTS=1 if intentional"
    );
}

#[test]
fn example_com_snapshot() {
    let text = render_text(&fixture("example.com.html"), 80);
    // Smoke: content is there and lines fit the width.
    assert!(text.to_lowercase().contains("example"));
    for line in text.lines() {
        assert!(
            UnicodeWidthStr::width(line) <= 80,
            "line too wide: {line:?}"
        );
    }
    assert_snapshot("example.com", &text);
}

#[test]
fn motherfuckingwebsite_snapshot() {
    let text = render_text(&fixture("motherfuckingwebsite.com.html"), 80);
    assert!(text.to_lowercase().contains("motherfucking"));
    assert_snapshot("motherfuckingwebsite.com", &text);
}

#[test]
fn simple_box_model_snapshot() {
    // Tiny synthetic page that pins margins, max-width and a border without
    // depending on a live site's markup.
    let html = r#"<!doctype html>
<html><body>
<div style="max-width: 320px; border: 1px solid black; padding: 8px; background: #eee">
<p style="margin: 0">boxed</p>
</div>
<p>after</p>
</body></html>"#;
    let text = render_text(html, 40);
    assert!(text.contains("boxed"));
    assert!(text.contains("after"));
    assert_snapshot("simple-box-model", &text);
}
