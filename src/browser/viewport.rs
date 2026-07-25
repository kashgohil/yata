use crate::layout::{Line, Span};
use crate::term::Style;
use unicode_width::UnicodeWidthChar;

/// Where the display lines came from, which is what a resize has to know.
enum Source {
    /// Plain text — the body before it is parsed, an error page, the `F1` tree.
    /// The unwrapped lines are kept so a resize can re-wrap them here without
    /// the producer running again.
    Text(Vec<String>),
    /// Lines from `layout`, already wrapped to a column width. Nothing to
    /// re-wrap: a resize re-runs layout and hands over new lines instead.
    Laid,
}

/// The scrollable body: display lines and a scroll offset. Pure logic — no
/// terminal types beyond the `Style` its spans carry.
///
/// Lines are built exactly twice, on new content and on resize; scrolling only
/// moves the offset. This is the CLAUDE.md scroll invariant and the
/// scroll-step-<5 ms budget: a keypress never re-wraps and never relayouts.
pub struct Viewport {
    source: Source,
    /// What `draw` slices from: text hard-wrapped to `width`, or laid-out lines
    /// exactly as `layout` produced them.
    lines: Vec<Line>,
    /// Index of the first visible line. Always in `0..=max_offset`.
    offset: usize,
    /// Wrap width and page height, both in cells / lines, from the last frame.
    width: usize,
    page: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            source: Source::Text(Vec::new()),
            lines: Vec::new(),
            offset: 0,
            width: 0,
            page: 0,
        }
    }
}

impl Viewport {
    /// Replace the content: sanitize, wrap to `width`, and reset to the top —
    /// new content always starts at the first line. `page` is the visible line
    /// count (frame height minus the status row).
    pub fn set_content(&mut self, content: &str, width: u16, page: u16) {
        self.source = Source::Text(sanitize(content));
        self.width = width as usize;
        self.page = page as usize;
        self.rewrap();
        self.offset = 0;
    }

    /// Show lines that arrive already wrapped, from `layout`. The offset is kept
    /// and clamped, never reset: both callers are relayouts of a page the reader
    /// is already looking at, and throwing them to the top on a resize would be
    /// worse than landing a few lines off.
    ///
    /// A *new* page is put back at the top upstream, by the `set_content` that
    /// shows its raw body while the parse is still running — by the time lines
    /// reach here the offset is already 0. That is a load-path invariant rather
    /// than something this method enforces, so
    /// `a_new_page_starts_at_the_top_even_after_scrolling_the_last_one` pins it
    /// from the outside.
    pub fn set_lines(&mut self, lines: Vec<Line>, page: u16) {
        self.source = Source::Laid;
        self.lines = lines;
        self.page = page as usize;
        self.offset = self.offset.min(self.max_offset());
    }

    /// Re-wrap at a new frame size (resize is one of the two wrap points), then
    /// clamp the offset so it still points at real content. Laid-out lines are
    /// left alone: their producer re-runs layout and calls `set_lines`.
    pub fn resize(&mut self, width: u16, page: u16) {
        self.width = width as usize;
        self.page = page as usize;
        self.rewrap();
        self.offset = self.offset.min(self.max_offset());
    }

    /// The display lines currently on screen: `page` of them from `offset`,
    /// fewer near the end. `draw` paints these into the page area.
    pub fn visible(&self) -> &[Line] {
        let end = (self.offset + self.page).min(self.lines.len());
        &self.lines[self.offset..end]
    }

    /// Scroll position as a percentage: 0 at the top, 100 with the last page
    /// visible. Content that fits entirely reads 100 — everything is on
    /// screen. `None` until there is content at all (the statusline omits the
    /// segment rather than showing a made-up number).
    pub fn scroll_percent(&self) -> Option<u8> {
        if self.lines.is_empty() {
            return None;
        }
        let max = self.max_offset();
        if max == 0 {
            return Some(100);
        }
        Some(((self.offset * 100 + max / 2) / max) as u8)
    }

    #[cfg(test)]
    pub fn offset(&self) -> usize {
        self.offset
    }

    #[cfg(test)]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn scroll_down(&mut self) -> bool {
        self.scroll_to(self.offset + 1)
    }

    pub fn scroll_up(&mut self) -> bool {
        self.scroll_to(self.offset.saturating_sub(1))
    }

    pub fn half_page_down(&mut self) -> bool {
        self.scroll_to(self.offset + self.half_page())
    }

    pub fn half_page_up(&mut self) -> bool {
        self.scroll_to(self.offset.saturating_sub(self.half_page()))
    }

    pub fn scroll_to_top(&mut self) -> bool {
        self.scroll_to(0)
    }

    pub fn scroll_to_bottom(&mut self) -> bool {
        self.scroll_to(self.max_offset())
    }

    /// The largest offset that still fills the page (or 0 when content is
    /// shorter than the page); scrolling never goes past it.
    fn max_offset(&self) -> usize {
        self.lines.len().saturating_sub(self.page)
    }

    /// At least one line, so a half-page scroll always moves when it can.
    fn half_page(&self) -> usize {
        (self.page / 2).max(1)
    }

    /// Clamp `target` to content and move there. Returns whether the offset
    /// actually changed: a scroll at the limit reports not-dirty (no dead
    /// redraws).
    fn scroll_to(&mut self, target: usize) -> bool {
        let clamped = target.min(self.max_offset());
        let changed = clamped != self.offset;
        self.offset = clamped;
        changed
    }

    fn rewrap(&mut self) {
        let Source::Text(source) = &self.source else {
            return;
        };
        self.lines = source
            .iter()
            .flat_map(|line| wrap_line(line, self.width))
            .map(plain_line)
            .collect();
    }
}

/// A wrapped text line as a display line: one unstyled span, or no spans at all
/// for a blank line — the same shape `layout` gives its blank rows.
fn plain_line(text: String) -> Line {
    Line {
        spans: if text.is_empty() {
            Vec::new()
        } else {
            vec![Span {
                text,
                style: Style::default(),
            }]
        },
    }
}

/// Split `content` into logical lines: strip `\r`, split on `\n`, and replace
/// any remaining control character with a space so it cannot corrupt the frame.
fn sanitize(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| {
            line.chars()
                .filter(|&ch| ch != '\r')
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect()
        })
        .collect()
}

/// Hard-wrap one logical line to `width` cells (measured via `unicode-width`,
/// never byte or char counts). Naive: breaks mid-word, but never splits a wide
/// character across two lines. An empty logical line yields one empty display
/// line, preserving blank lines.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    // A zero-width frame would otherwise loop forever; one cell is harmless.
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for ch in line.chars() {
        let cw = ch.width().unwrap_or(0);
        // Break before a char that would overflow, unless the current line is
        // empty — a lone char wider than the page still gets its own line
        // rather than an empty one.
        if cur_w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// A display line's text, spans concatenated — what the reader sees, with
    /// the styles that these tests do not care about dropped.
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn texts(vp: &Viewport) -> Vec<String> {
        vp.lines.iter().map(text).collect()
    }

    #[test]
    fn overlong_ascii_wraps_at_width() {
        let mut vp = Viewport::default();
        vp.set_content("abcdefghij", 4, 10);
        assert_eq!(texts(&vp), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn newlines_split_and_blank_lines_survive() {
        let mut vp = Viewport::default();
        vp.set_content("a\n\nb", 10, 10);
        assert_eq!(texts(&vp), ["a", "", "b"]);
        // A blank line carries no span at all, the shape `layout` also uses, so
        // the painter has nothing to write for it.
        assert!(vp.lines[1].spans.is_empty());
    }

    #[test]
    fn cjk_wraps_within_width_and_never_splits_a_wide_char() {
        let mut vp = Viewport::default();
        // Nine wide characters (2 cells each) at a page width of 5 cells.
        vp.set_content(&"世".repeat(9), 5, 10);
        for line in texts(&vp) {
            assert!(
                line.width() <= 5,
                "line {line:?} is {} cells, over the width",
                line.width()
            );
            // No half of a wide char can appear: every cell here is width 2, so
            // the count is even and the char is whole.
            assert!(line.chars().all(|c| c.width() == Some(2)));
        }
        assert_eq!(
            texts(&vp).iter().map(|l| l.chars().count()).sum::<usize>(),
            9
        );
    }

    #[test]
    fn control_chars_become_spaces_and_never_reach_a_line() {
        let mut vp = Viewport::default();
        vp.set_content("a\tb\u{7}c", 80, 10);
        assert_eq!(texts(&vp), ["a b c"]);
        assert!(
            texts(&vp)
                .iter()
                .flat_map(|l| l.chars())
                .all(|c| !c.is_control()),
            "a control character reached a display line"
        );
    }

    #[test]
    fn laid_out_lines_are_shown_as_given_and_survive_a_resize() {
        let mut vp = Viewport::default();
        let line = |s: &str| Line {
            spans: vec![Span {
                text: s.into(),
                style: Style::default(),
            }],
        };
        vp.set_lines(vec![line("a wide laid-out line"), line("second")], 10);
        assert_eq!(texts(&vp), ["a wide laid-out line", "second"]);

        // A resize must not re-wrap them: layout owns their width, and the
        // producer re-runs it and calls `set_lines` again.
        vp.resize(4, 10);
        assert_eq!(texts(&vp), ["a wide laid-out line", "second"]);
    }

    #[test]
    fn relaying_out_keeps_the_reader_where_they_were() {
        let mut vp = Viewport::default();
        let lines = |n: usize| {
            (0..n)
                .map(|i| Line {
                    spans: vec![Span {
                        text: i.to_string(),
                        style: Style::default(),
                    }],
                })
                .collect::<Vec<_>>()
        };
        vp.set_lines(lines(100), 10);
        vp.scroll_to_bottom();
        assert_eq!(vp.offset(), 90);

        // A narrower column produces more lines; the offset stays put.
        vp.set_lines(lines(120), 10);
        assert_eq!(vp.offset(), 90);

        // A shorter page clamps it rather than leaving it past the end.
        vp.set_lines(lines(20), 10);
        assert_eq!(vp.offset(), 10);

        // And a new page — no lines yet — puts the reader back at the top.
        vp.set_lines(Vec::new(), 10);
        assert_eq!(vp.offset(), 0);
    }

    fn tall() -> Viewport {
        let mut vp = Viewport::default();
        // 100 single-cell lines, page of 10: max offset 90.
        let body = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        vp.set_content(&body, 80, 10);
        vp
    }

    #[test]
    fn line_scroll_moves_and_clamps() {
        let mut vp = tall();
        assert!(vp.scroll_down());
        assert_eq!(vp.offset(), 1);
        assert!(vp.scroll_up());
        assert_eq!(vp.offset(), 0);
        // Up at the top does nothing and is not dirty.
        assert!(!vp.scroll_up());
        assert_eq!(vp.offset(), 0);
    }

    #[test]
    fn half_page_moves_by_half_the_page() {
        let mut vp = tall();
        assert!(vp.half_page_down());
        assert_eq!(vp.offset(), 5);
        assert!(vp.half_page_up());
        assert_eq!(vp.offset(), 0);
    }

    #[test]
    fn top_and_bottom_jump_and_clamp() {
        let mut vp = tall();
        assert!(vp.scroll_to_bottom());
        assert_eq!(vp.offset(), 90, "bottom leaves the last page filled");
        // Already at the bottom: not dirty.
        assert!(!vp.scroll_to_bottom());
        assert!(!vp.scroll_down(), "cannot scroll past the last page");
        assert!(vp.scroll_to_top());
        assert_eq!(vp.offset(), 0);
        assert!(!vp.scroll_to_top());
    }

    #[test]
    fn visible_is_the_page_slice_at_the_offset() {
        let mut vp = tall();
        assert_eq!(vp.visible().len(), 10);
        assert_eq!(text(&vp.visible()[0]), "0");
        vp.scroll_down();
        assert_eq!(text(&vp.visible()[0]), "1");
    }

    #[test]
    fn new_content_resets_the_scroll_to_the_top() {
        let mut vp = tall();
        vp.scroll_to_bottom();
        vp.set_content("fresh page", 80, 10);
        assert_eq!(vp.offset(), 0, "a new page must start at its first line");
    }

    #[test]
    fn scroll_percent_is_none_without_content_and_tracks_position() {
        assert_eq!(Viewport::default().scroll_percent(), None);
        let mut vp = tall();
        assert_eq!(vp.scroll_percent(), Some(0));
        vp.scroll_to_bottom();
        assert_eq!(vp.scroll_percent(), Some(100));
        // One line up from the bottom: 89/90 rounds to 99, not a premature 100.
        vp.scroll_up();
        assert_eq!(vp.scroll_percent(), Some(99));
    }

    #[test]
    fn content_that_fits_entirely_reads_100_percent() {
        let mut vp = Viewport::default();
        vp.set_content("a\nb", 80, 10);
        assert_eq!(vp.scroll_percent(), Some(100));
    }

    #[test]
    fn resize_narrower_rewraps_to_more_lines_and_clamps_offset() {
        let mut vp = Viewport::default();
        // Ten lines of 8 cells each; at width 8 that is ten display lines.
        let body = ["abcdefgh"; 10].join("\n");
        vp.set_content(&body, 8, 4);
        assert_eq!(vp.line_count(), 10);
        vp.scroll_to_bottom();
        assert_eq!(vp.offset(), 6);

        // Halve the width: each source line now wraps to two, 20 display lines.
        vp.resize(4, 4);
        assert_eq!(vp.line_count(), 20);
        assert!(
            vp.offset() <= vp.line_count() - 4,
            "offset {} left past the new content end",
            vp.offset()
        );
    }
}
