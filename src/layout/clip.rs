//! Clip regions: which document cells a box's content is allowed to reach
//! (PLAN.md M9, task M9.3).
//!
//! `overflow` is honoured when the display list is *built*, not when it is
//! drawn. The commands that come out are already trimmed, so `paint_to_frame`
//! — the scroll path — never learns that clipping exists and a scroll step
//! costs exactly what it did before. Hit-testing and `/` search read the same
//! rule from here: content the reader cannot see must not be clickable or
//! findable either.
//!
//! Deviation from a browser, deliberate: when only one axis is non-`visible`,
//! CSS computes the other one to `auto` and gives that axis a scrollbar. There
//! is no inner scrolling in a terminal (PLAN.md §M11+), so the `visible` axis
//! here stays genuinely unclipped — content runs out of the box sideways
//! instead of being reachable by scrolling it.

use crate::layout::boxes::LayoutBox;
use crate::layout::dimensions::Rect;

/// A clip region in document cells: a half-open `[start, end)` range per axis,
/// `None` meaning unbounded. Per-axis rather than a single rectangle because
/// `overflow-x: hidden` must clip horizontally and leave the vertical axis
/// alone, which no rectangle can express.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Clip {
    x: Option<(i32, i32)>,
    y: Option<(i32, i32)>,
}

impl Clip {
    /// The whole page: what the root paints into.
    pub const NONE: Clip = Clip { x: None, y: None };

    /// The clip that applies to `b`'s content and descendants: this clip,
    /// narrowed to `b`'s **padding box** on each axis whose `overflow` is not
    /// `visible` (CSS: the padding edge is what content is clipped to).
    ///
    /// A box's own background and border are not narrowed by its own
    /// `overflow` — they are painted on and outside that same padding edge.
    /// Which is why paint gives a box `clip` and its children `clip.inside(b)`.
    pub fn inside(self, b: &LayoutBox) -> Clip {
        let clip_x = b.computed.overflow_x.clips();
        let clip_y = b.computed.overflow_y.clips();
        if !clip_x && !clip_y {
            return self;
        }
        let pad = b.dimensions.padding_box();
        Clip {
            x: if clip_x {
                tighten(self.x, (pad.x, pad.right()))
            } else {
                self.x
            },
            y: if clip_y {
                tighten(self.y, (pad.y, pad.bottom()))
            } else {
                self.y
            },
        }
    }

    /// A clip with explicit ranges. Test-only seam: content to the *left* of a
    /// clip edge needs a negative margin or a flex offset to reach paint, and
    /// this engine has neither yet — but text is trimmed from the left, and an
    /// untested branch is an untrue one.
    #[cfg(test)]
    pub fn ranges(x: Option<(i32, i32)>, y: Option<(i32, i32)>) -> Clip {
        Clip { x, y }
    }

    /// Nothing can survive this clip — the region has no area on some axis.
    /// A `height: 0; overflow: hidden` box collapses to exactly this, and it
    /// is why paint can stop walking a subtree instead of trimming every
    /// command in it to nothing.
    pub fn is_empty(self) -> bool {
        let empty = |r: Option<(i32, i32)>| r.is_some_and(|(s, e)| e <= s);
        empty(self.x) || empty(self.y)
    }

    /// `rect` trimmed to the visible region. A rectangle clipped away entirely
    /// comes back with zero width or height, never with negative sides.
    pub fn apply(self, rect: Rect) -> Rect {
        let (x, width) = trim(self.x, rect.x, rect.width);
        let (y, height) = trim(self.y, rect.y, rect.height);
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Is this single cell visible?
    pub fn contains(self, x: i32, y: i32) -> bool {
        let inside = |r: Option<(i32, i32)>, v: i32| r.is_none_or(|(s, e)| v >= s && v < e);
        inside(self.x, x) && inside(self.y, y)
    }

    /// Is any of document row `y` visible? (Text runs are one row tall, so the
    /// vertical question is answered once for the whole run.)
    fn shows_row(self, y: i32) -> bool {
        self.y.is_none_or(|(s, e)| y >= s && y < e)
    }

    /// Trim a one-row text run starting at (`x`, `y`) to the cells this clip
    /// leaves visible, returning its new origin and text — `None` when nothing
    /// survives.
    ///
    /// Truncation is by **cells** (`unicode-width`, CLAUDE.md), from either
    /// end: a clip that starts mid-run drops the characters to its left and
    /// moves the run's origin to the first surviving cell. A wide glyph
    /// straddling an edge is dropped rather than half-drawn, leaving its far
    /// half blank — half a CJK glyph is not a glyph, and the cell it would
    /// occupy belongs to the box on the other side of the clip.
    ///
    /// Paint trims the display list with this, and the focus overlay trims
    /// what it reverse-videos with it. Two implementations of "what does the
    /// reader see" is exactly one too many.
    pub fn trim_text(self, x: i32, y: i32, text: &str) -> Option<(i32, String)> {
        use unicode_width::UnicodeWidthChar;
        if !self.shows_row(y) {
            return None;
        }
        let Some((left, right)) = self.x else {
            return Some((x, text.to_string()));
        };
        let mut out = String::new();
        let mut out_x = x;
        let mut cx = x;
        for ch in text.chars() {
            if cx >= right {
                break;
            }
            let w = UnicodeWidthChar::width(ch).unwrap_or(0) as i32;
            if w == 0 {
                // Combining marks ride along with the character they modify,
                // and are dropped with it.
                if !out.is_empty() {
                    out.push(ch);
                }
                continue;
            }
            if cx >= left && cx + w <= right {
                if out.is_empty() {
                    out_x = cx;
                }
                out.push(ch);
            }
            cx += w;
        }
        (!out.is_empty()).then_some((out_x, out))
    }

    /// Does `b` put anything on screen under this clip? Used by hit-testing
    /// and link discovery, which ask about a whole box rather than a cell.
    /// A zero-area box (an inline that generated no fragment) is judged by its
    /// origin, since intersecting an empty rectangle answers nothing.
    pub fn shows(self, b: &LayoutBox) -> bool {
        let rect = b.dimensions.border_box();
        if rect.width <= 0 || rect.height <= 0 {
            return self.contains(rect.x, rect.y);
        }
        let visible = self.apply(rect);
        visible.width > 0 && visible.height > 0
    }
}

/// Intersect a range with `add`, keeping `start <= end` so an already-empty
/// clip cannot invert into a negative-width one.
fn tighten(current: Option<(i32, i32)>, add: (i32, i32)) -> Option<(i32, i32)> {
    let (add_start, add_end) = add;
    Some(match current {
        None => (add_start, add_end.max(add_start)),
        Some((start, end)) => {
            let start = start.max(add_start);
            (start, end.min(add_end).max(start))
        }
    })
}

/// One axis of [`Clip::apply`]: `(origin, length)` trimmed to the range.
fn trim(range: Option<(i32, i32)>, origin: i32, length: i32) -> (i32, i32) {
    let Some((start, end)) = range else {
        return (origin, length);
    };
    let low = origin.max(start);
    let high = (origin + length).min(end);
    (low, (high - low).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// A clip that bounds both axes, built the way `inside` builds one.
    fn both(x: (i32, i32), y: (i32, i32)) -> Clip {
        Clip {
            x: Some(x),
            y: Some(y),
        }
    }

    #[test]
    fn an_unbounded_clip_changes_nothing() {
        let r = rect(3, 4, 10, 2);
        assert_eq!(Clip::NONE.apply(r), r);
        assert!(Clip::NONE.contains(-5, 999));
        assert!(!Clip::NONE.is_empty());
        assert_eq!(
            Clip::NONE.trim_text(3, 4, "untouched"),
            Some((3, "untouched".to_string()))
        );
    }

    #[test]
    fn nested_clips_intersect_to_the_inner_rectangle() {
        let outer = both((0, 20), (0, 10));
        let inner = Clip {
            x: tighten(outer.x, (5, 30)),
            y: tighten(outer.y, (2, 4)),
        };
        assert_eq!(inner.apply(rect(0, 0, 40, 40)), rect(5, 2, 15, 2));
    }

    #[test]
    fn a_rect_outside_the_clip_comes_back_empty_not_negative() {
        let clip = both((10, 20), (0, 5));
        let away = clip.apply(rect(0, 0, 4, 4));
        assert_eq!(away.width, 0);
        assert_eq!(away.height, 4);
        let below = clip.apply(rect(12, 40, 4, 4));
        assert_eq!(below.height, 0);
    }

    #[test]
    fn tightening_an_empty_range_keeps_it_empty() {
        // `height: 0` gives (y, y); anything inside it stays collapsed rather
        // than inverting into a negative-length range.
        let empty = tighten(None, (7, 7));
        assert_eq!(empty, Some((7, 7)));
        assert_eq!(tighten(empty, (0, 100)), Some((7, 7)));
        let clip = Clip { x: None, y: empty };
        assert!(clip.is_empty());
        assert_eq!(clip.apply(rect(0, 0, 40, 40)).height, 0);
    }
}
