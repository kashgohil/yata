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
    let mut dom = html::parse(html);
    // Scripts run before the snapshot, under the headless rule (one pass, no
    // timers, no subresource fetches) that `headless::run_scripts` documents.
    // A grid that showed a page as it was *before* its script would pin a
    // browser nobody uses (M10.2).
    let _ = yata::headless::run_scripts(&mut dom, None);
    let dom = dom;
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

/// M9.11: a badge inside a paragraph, through the paint path.
///
/// The layout goldens say where the box is; this says that being a box is what
/// it looks like — a background that fills its own padded rectangle and stops,
/// with the sentence running past it on the baseline row. As a plain `inline`
/// (what `inline-block` degraded to before M9.11) there was no rectangle to
/// fill: the padding and the background went nowhere.
#[test]
fn an_inline_block_badge_paints_its_own_box_inside_the_line() {
    let html = r#"<!doctype html>
<html><head><style>
body { margin: 0 } p { margin: 0 }
.badge { display: inline-block; padding: 0 8px; background: #eee }
</style></head>
<body><p>build <span class="badge">passing</span> now</p></body></html>"#;
    let grid = render_grid(html, 24, 3);
    // "build " is 6 cells, the badge is 7 + 2 of padding, " now" follows at 15.
    assert_eq!(
        grid.lines().collect::<Vec<_>>(),
        ["build  passing  now"],
        "{grid}"
    );
    let backgrounds = render_backgrounds(html, 24, 3);
    assert_eq!(
        backgrounds.lines().collect::<Vec<_>>(),
        ["      #########"],
        "the badge's background did not fill its padding box:\n{backgrounds}"
    );
    assert_snapshot("inline-block-badge", &grid);
    assert_snapshot("inline-block-badge-backgrounds", &backgrounds);
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

/// M9.12: a flex line wider than the terminal, and the rule for it.
///
/// **The rule.** A flex line that cannot fit is laid out at its real width —
/// the boxes past the column edge exist, with their real x — and paint culls
/// at that edge. There is no horizontal scroll to reach them and no sideways
/// scrollbar to hint at them: the reader gets the content by making the
/// terminal wider, which relayouts. Nothing is silently *dropped* in layout,
/// which is what would make the geometry a lie; it is dropped at the frame,
/// where the terminal really does end.
///
/// The alternative — shrinking items past their `flex-shrink: 0` to make the
/// line fit — was rejected: it would let a page that says "this column is
/// exactly 320px" render at some other width and look correct, which is the
/// bug class M9.10's review was named after.
///
/// `layout/spec/flex-justify-overflow` already pins the geometry half of this.
/// This is the paint half: which cells actually survive.
#[test]
fn a_flex_line_wider_than_the_terminal_is_culled_at_the_column_edge() {
    let html = r#"<!doctype html>
<html><head><style>
body { margin: 0 } div { margin: 0 }
.row { display: flex }
.row div { flex: 0 0 96px; background: #eee }
</style></head>
<body><div class="row"><div>alpha</div><div>bravo</div><div>charlie</div></div>
<p style="margin: 0">after</p></body></html>"#;

    // Three 12-cell items in a 20-cell terminal: 36 cells of line for 20 cells
    // of screen. `alpha` is whole, `bravo` starts at 12 and is cut mid-box,
    // `charlie` starts at 24 and never appears at all.
    let grid = render_grid(html, 20, 4);
    assert_eq!(
        grid.lines().collect::<Vec<_>>(),
        ["alpha       bravo", "after"],
        "{grid}"
    );
    assert!(
        !grid.contains("charlie"),
        "culling let a whole item through:\n{grid}"
    );

    // The cull is at the terminal edge, not at the item boundary: the second
    // item's background fills to column 20 and stops there mid-box.
    let backgrounds = render_backgrounds(html, 20, 4);
    assert_eq!(
        backgrounds.lines().collect::<Vec<_>>(),
        ["####################"],
        "{backgrounds}"
    );

    // Layout keeps the geometry honest — the boxes past the edge are still
    // there at their real x, so widening the terminal reveals them instead of
    // reflowing something that was never laid out.
    let dom = html::parse(html);
    let sheets = style::sources::inline_sheets(&dom);
    let refs: Vec<_> = sheets.iter().collect();
    let styles = style::style_tree(&dom, &refs);
    let tree = layout::layout_document(&dom, &styles, 20, Hidden::Respect);
    let mut xs = Vec::new();
    tree.walk(tree.root, &mut |_, b| {
        if b.kind == layout::BoxKind::Text {
            xs.push((b.text.clone().unwrap_or_default(), b.dimensions.content.x));
        }
    });
    assert!(
        xs.iter().any(|(t, x)| t == "charlie" && *x == 24),
        "the culled item lost its geometry: {xs:?}"
    );

    assert_snapshot("flex-viewport-overflow", &grid);
    assert_snapshot("flex-viewport-overflow-backgrounds", &backgrounds);
}

/// M9.12: the whole milestone on one page, through the paint path.
///
/// The spec goldens each pin one rule in isolation; a real page asks for four
/// of them at once and the interesting failures live between them — a card
/// list that wraps *because* the sidebar took 12 cells first, a clip whose two
/// surviving rows decide where the next box starts. 60 cells is chosen so the
/// third card cannot fit beside the other two: at 80 it would not wrap and the
/// fixture would stop testing M9.10.
#[test]
fn flex_page_snapshot() {
    let grid = render_grid(&fixture("flex.html"), 60, 10);
    let rows: Vec<&str> = grid.lines().collect();

    // Row 0 — `justify-content: space-between`: the first item at the line's
    // start, the last ending at its end, the rest of the 47 free cells split
    // between them.
    assert!(rows[0].starts_with("home"), "{grid}");
    assert_eq!(
        UnicodeWidthStr::width(rows[0]),
        60,
        "space-between must reach the far edge:\n{grid}"
    );
    assert!(rows[0].ends_with("about"), "{grid}");

    // Rows 1-2 — a 12-cell sidebar (`flex: 0 0 96px`) beside a `flex: 1`
    // column that wraps at the 48 cells that leaves it.
    assert!(rows[1].starts_with("one"), "{grid}");
    assert!(rows[2].starts_with("two"), "{grid}");
    assert_eq!(
        &rows[1][12..],
        "body text that wraps inside the content column",
        "{grid}"
    );
    assert_eq!(&rows[2][12..], "beside the sidebar", "{grid}");

    // Rows 3-5 — two 18-cell cards and a 1-cell gap fit in 48; the third
    // starts a second line rather than shrinking. `gap: 8px` is half a line
    // vertically, and a nonzero length never rounds away to nothing, so the
    // two card lines are separated by a blank row.
    assert_eq!(&rows[3][12..], "card one           card two", "{grid}");
    assert_eq!(rows[4], "", "the cross-axis gap is one row:\n{grid}");
    assert_eq!(&rows[5][12..], "card three", "{grid}");

    // Rows 6-7 — `max-height: 32px` is two lines, and the third paragraph is
    // clipped away rather than painted over what follows.
    assert_eq!(&rows[6][12..], "kept row", "{grid}");
    assert_eq!(&rows[7][12..], "kept too", "{grid}");
    assert!(!grid.contains("cut away"), "clip leaked:\n{grid}");
    assert_eq!(rows.len(), 8, "unexpected page height:\n{grid}");

    assert_snapshot("flex", &grid);
}

/// A page shaped like something a site would ship: a script that builds a list
/// from data, a `DOMContentLoaded` handler, a click handler on a script-built
/// button, an element revealed by removing a class, and one deliberate error.
///
/// The single features are pinned by M10.1–M10.13; what this pins is them
/// *interacting* — the list only exists because `DOMContentLoaded` fired after
/// the pass, the notice is only visible because a class was removed and the
/// cascade re-ran, and the error did not stop any of it.
#[test]
fn js_page_snapshot() {
    let grid = render_grid(&fixture("js.html"), 80, 24);

    // Built by the `DOMContentLoaded` handler, from data.
    assert!(grid.contains("The Mythical Man-Month (1975)"), "{grid}");
    assert!(
        grid.contains("The Practice of Programming (1999)"),
        "{grid}"
    );
    // Written by the same handler, replacing what the markup said.
    assert!(grid.contains("3 books"), "{grid}");
    assert!(!grid.contains("loading…"), "{grid}");
    // Hidden in the markup, revealed by removing a class — the cascade saw it.
    assert!(grid.contains("Revealed by removing a class."), "{grid}");
    // The script-built button is on the page even though the script that made
    // it also threw afterwards.
    assert!(grid.contains("add another"), "{grid}");

    assert_snapshot("js", &grid);
}
