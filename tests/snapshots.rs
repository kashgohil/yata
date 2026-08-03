//! Snapshot tests (PLAN.md M5): fixture HTML → rendered cell grid text.
//!
//! Snapshots go through layout → paint → frame so borders and backgrounds are
//! part of the golden output, not only the text-line dump. Update only when
//! rendering is *supposed* to change (CLAUDE.md): `UPDATE_SNAPSHOTS=1 cargo test
//! --test snapshots`.

use std::fs;
use unicode_width::UnicodeWidthStr;
use yata::html;
use yata::layout::{self, Hidden};
use yata::paint;
use yata::style;
use yata::term::{Cell, Color, Frame};

fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture must be committed")
}

/// Full paint path: layout tree → display list → frame.
fn render_frame(html: &str, width: u16, height: u16) -> Frame {
    let dom = html::parse(html);
    let sheets = style::sources::inline_sheets(&dom);
    let refs: Vec<_> = sheets.iter().collect();
    let styles = style::style_tree(&dom, &refs);
    let tree = layout::layout_document(&dom, &styles, width, Hidden::Respect);
    let list = paint::paint(&tree);
    let mut frame = Frame::new(width, height);
    paint::paint_to_frame(&list, &mut frame, 0, 0, height);
    frame
}

/// One character per cell, chosen by `of`; rows joined by `\n`.
fn grid_text(frame: &Frame, width: u16, height: u16, of: impl Fn(Cell) -> char) -> String {
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push(of(frame.get(x, y)));
        }
        // Trim trailing spaces so snapshots stay stable if height is generous.
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    // Drop trailing empty rows so height padding does not churn the golden.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The glyphs in each frame row (spaces kept).
fn render_grid(html: &str, width: u16, height: u16) -> String {
    let frame = render_frame(html, width, height);
    // Continuation cells of wide glyphs are nul; treat as space.
    grid_text(
        &frame,
        width,
        height,
        |c| if c.ch == '\0' { ' ' } else { c.ch },
    )
}

/// The same frame as background *coverage*: `#` where a cell has a background
/// colour, space where it does not.
///
/// A background is invisible in a glyph dump — it paints spaces — and how far
/// one reaches is exactly what `align-items: stretch` decides, so pinning it
/// needs a view of the cells that is not the text.
fn render_backgrounds(html: &str, width: u16, height: u16) -> String {
    let frame = render_frame(html, width, height);
    grid_text(&frame, width, height, |c| {
        if c.bg == Color::Default { ' ' } else { '#' }
    })
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
    let grid = render_grid(&fixture("example.com.html"), 80, 24);
    assert!(grid.to_lowercase().contains("example"));
    for line in grid.lines() {
        assert!(
            UnicodeWidthStr::width(line) <= 80,
            "line too wide: {line:?}"
        );
    }
    assert_snapshot("example.com", &grid);
}

#[test]
fn motherfuckingwebsite_snapshot() {
    let grid = render_grid(&fixture("motherfuckingwebsite.com.html"), 80, 40);
    assert!(grid.to_lowercase().contains("motherfucking"));
    assert_snapshot("motherfuckingwebsite.com", &grid);
}

#[test]
fn simple_box_model_snapshot() {
    // Pins border box-drawing and background fill through the paint path.
    let html = r#"<!doctype html>
<html><body>
<div style="max-width: 320px; border: 1px solid black; padding: 8px; background: #eee; margin: 0">
<p style="margin: 0">boxed</p>
</div>
<p style="margin: 0">after</p>
</body></html>"#;
    let grid = render_grid(html, 40, 12);
    assert!(grid.contains("boxed"), "{grid}");
    assert!(grid.contains("after"), "{grid}");
    assert!(
        grid.contains('┌') && grid.contains('┐') && grid.contains('└') && grid.contains('┘'),
        "border corners missing:\n{grid}"
    );
    assert_snapshot("simple-box-model", &grid);
}

/// M9.3: the paint half of `layout/spec/overflow-clip`. That golden pins where
/// the boxes are (clipping moves nothing); this one pins which cells survive,
/// which is the only place the property is visible.
#[test]
fn overflow_clip_snapshot() {
    let grid = render_grid(&fixture("layout/spec/overflow-clip.html"), 40, 6);
    let rows: Vec<&str> = grid.lines().collect();
    // Derived in the golden's header: the collapsed menu paints nothing, the
    // card shows 2 of its 3 rows, the <pre> keeps 20 of its 36 cells.
    assert_eq!(
        rows,
        ["card one", "card two", "0123456789abcdefghij"],
        "{grid}"
    );
    assert_snapshot("overflow-clip", &grid);
}

/// M9.8: the paint half of `layout/spec/flex-stretch`. That golden says the
/// stretched item's box is 3 rows tall; this says its background actually
/// fills them, which is the whole reason a page asks for `stretch` — equal
/// height cards, not equal height text.
#[test]
fn flex_stretch_backgrounds_reach_the_full_line() {
    let html = fixture("layout/spec/flex-stretch.html");
    let grid = render_grid(&html, 20, 4);
    // The text is untouched by stretching: three items side by side, each
    // starting on the line's first row.
    assert_eq!(
        grid.lines().collect::<Vec<_>>(),
        ["one  two  aaaaa", "          bbbbb", "          ccccc"],
        "{grid}"
    );
    // Both cards carry the same background. The first stretched to the line's
    // 3 rows; the second asked for `align-self: flex-start` and kept its 1.
    let backgrounds = render_backgrounds(&html, 20, 4);
    assert_eq!(
        backgrounds.lines().collect::<Vec<_>>(),
        ["##########", "#####", "#####"],
        "{backgrounds}"
    );
    assert_snapshot("flex-stretch", &grid);
    assert_snapshot("flex-stretch-backgrounds", &backgrounds);
}
