//! Rasterise a layout tree into display lines (one `Line` per row of cells).
//!
//! The viewport and `--dump-text` still speak in lines. The display-list path
//! (M5.3) paints the tree directly; this adapter keeps the existing surfaces
//! working and is how unit tests pin readable text.

use crate::layout::boxes::{BoxKind, LayoutTree};
use crate::layout::{Line, Span};
use crate::term::Style;
use unicode_width::UnicodeWidthStr;

/// Flatten text boxes into rows. Empty rows (pure vertical margin) become empty
/// lines so the page's vertical rhythm matches the box tree's y coordinates.
pub fn from_tree(tree: &LayoutTree) -> Vec<Line> {
    if tree.height <= 0 {
        return Vec::new();
    }
    let height = tree.height as usize;
    let mut rows: Vec<Vec<(i32, String, Style)>> = vec![Vec::new(); height];

    tree.walk(tree.root, &mut |_id, b| {
        if b.kind != BoxKind::Text {
            return;
        }
        let Some(text) = b.text.as_ref() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let y = b.dimensions.content.y;
        if y < 0 || y as usize >= height {
            return;
        }
        rows[y as usize].push((b.dimensions.content.x, text.clone(), b.term_style));
    });

    rows.into_iter()
        .map(|mut frags| {
            if frags.is_empty() {
                return Line::default();
            }
            frags.sort_by_key(|(x, _, _)| *x);
            let mut spans = Vec::new();
            let mut cursor = 0i32;
            for (x, text, style) in frags {
                if x > cursor {
                    // Gap (from text-align shift or horizontal margin): pad.
                    let pad = (x - cursor) as usize;
                    spans.push(Span {
                        text: " ".repeat(pad),
                        style: Style::default(),
                    });
                    cursor = x;
                }
                let cells = text.width() as i32;
                // Merge with previous span when styles match.
                match spans.last_mut() {
                    Some(last) if last.style == style => last.text.push_str(&text),
                    _ => spans.push(Span { text, style }),
                }
                cursor += cells;
            }
            Line { spans }
        })
        .collect()
}
