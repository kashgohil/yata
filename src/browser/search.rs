//! In-page text search over the layout tree (PLAN.md M7).
//!
//! Pure query: layout tree + query string → match rectangles in document
//! cells. Highlighting is paint-time (frame overlay, same pattern as focus
//! and link hints — not a display-list command); this module never mutates
//! the tree.

use crate::layout::{BoxKind, LayoutTree};
use unicode_width::UnicodeWidthChar;

/// One match in document cell coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub x: i32,
    pub y: i32,
    /// Width in terminal cells (unicode-width of the matched slice).
    pub width: i32,
}

/// Case-insensitive substring search over every text box, document order.
///
/// Matching uses full Unicode lowercasing when it preserves 1:1 char length
/// (the common case for Latin web text). When a character expands under
/// case-fold (e.g. some ligatures), that text box is searched with a
/// **ASCII-only** fallback so widths stay measurable; non-ASCII case pairs
/// in that box may be missed. Good enough for the ladder pages.
pub fn find_matches(tree: &LayoutTree, query: &str) -> Vec<Match> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let q_chars: Vec<char> = q.chars().flat_map(|c| c.to_lowercase()).collect();
    let m = q_chars.len();
    if m == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    tree.walk(tree.root, &mut |_, b| {
        if b.kind != BoxKind::Text {
            return;
        }
        let Some(text) = &b.text else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let chars: Vec<char> = text.chars().collect();
        let lower_chars: Vec<char> = text.chars().flat_map(|c| c.to_lowercase()).collect();
        // Multi-char case folds break 1:1 indexing; fall back to ASCII-only.
        if lower_chars.len() != chars.len() {
            find_ascii_fallback(
                text,
                &q_chars.iter().collect::<String>(),
                b.dimensions.content.x,
                b.dimensions.content.y,
                &mut out,
            );
            return;
        }
        let n = lower_chars.len();
        if m > n {
            return;
        }
        let mut i = 0;
        while i + m <= n {
            if lower_chars[i..i + m] == q_chars[..] {
                let x_off = cells_width(&chars[..i]);
                let width = cells_width(&chars[i..i + m]);
                if width > 0 {
                    out.push(Match {
                        x: b.dimensions.content.x + x_off as i32,
                        y: b.dimensions.content.y,
                        width: width as i32,
                    });
                }
                i += m; // non-overlapping
            } else {
                i += 1;
            }
        }
    });
    out
}

fn cells_width(chars: &[char]) -> usize {
    chars.iter().map(|c| c.width().unwrap_or(0)).sum()
}

/// Byte-oriented ASCII lowercase search. Only used when Unicode lowercasing
/// expands a character so char indices no longer align with the original.
fn find_ascii_fallback(text: &str, q_lower: &str, base_x: i32, y: i32, out: &mut Vec<Match>) {
    // Query may contain non-ASCII lowercased chars; ASCII find only helps when
    // the query itself is ASCII.
    if !q_lower.is_ascii() || q_lower.is_empty() {
        return;
    }
    let lower = text.to_ascii_lowercase();
    let mut start = 0;
    while let Some(rel) = lower[start..].find(q_lower) {
        let abs = start + rel;
        let end = abs + q_lower.len();
        if end > text.len() {
            break;
        }
        let prefix = &text[..abs];
        let matched = &text[abs..end];
        let x_off = unicode_width::UnicodeWidthStr::width(prefix) as i32;
        let width = unicode_width::UnicodeWidthStr::width(matched) as i32;
        if width > 0 {
            out.push(Match {
                x: base_x + x_off,
                y,
                width,
            });
        }
        start = end.max(start + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::layout::{self, Hidden};
    use crate::style;

    fn tree(html: &str) -> LayoutTree {
        let dom = html::parse(html);
        let styles = style::style_tree(&dom, &[]);
        layout::layout_document(&dom, &styles, 60, Hidden::Respect)
    }

    #[test]
    fn finds_case_insensitive_substring() {
        let t = tree("<p>Hello World</p>");
        let hits = find_matches(&t, "hello");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].width > 0);
    }

    #[test]
    fn empty_query_is_no_matches() {
        let t = tree("<p>Hello</p>");
        assert!(find_matches(&t, "  ").is_empty());
    }

    #[test]
    fn multiple_hits_in_document_order() {
        let t = tree("<p>cat</p><p>concatenate</p>");
        let hits = find_matches(&t, "cat");
        assert!(hits.len() >= 2, "{hits:?}");
        assert!(hits[0].y <= hits[1].y);
    }
}
