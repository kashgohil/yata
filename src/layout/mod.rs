//! Layout: DOM → lines of styled text (PLAN.md M3).
//!
//! A pure transform — `&Dom` + a width in cells goes in, `Vec<Line>` comes out,
//! deterministically. Nothing here touches `App`, `Frame`, the terminal or the
//! network, and nothing mutates the DOM: the caller (M3.2) caches the lines and
//! repaints them at a new scroll offset without ever re-running layout. That
//! cache is the reason scrolling costs nothing, so it is also the reason this
//! function must stay a function.
//!
//! Everything is measured in terminal cells with `unicode-width`. A `char` is
//! not a cell (CJK is two, combining marks are zero) and a byte is not a cell,
//! so `str::len()` and `chars().count()` appear nowhere in this file.
//!
//! Styling comes from the cascade (M4.4): every node arrives with a
//! `ComputedStyle`, so this stage no longer knows that `<h1>` is bold or that
//! `<script>` is invisible — `style/ua.css` says both, and a page can say
//! otherwise. What stays tag-driven is what M4 has no property for: `<pre>`
//! whitespace, list bullets and indents, `<br>`, `<hr>`.

use crate::dom::{Dom, NodeData, NodeId};
use crate::style::Styles;
use crate::style::values::{ColorValue, Display, FontStyle, FontWeight};
use crate::term::{Attrs, Color, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A run of text sharing one style. Never contains a newline: the painter walks
/// a line's spans left to right, one `Frame::put_str` each.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One row of laid-out content. Empty lines are the blank rows between blocks.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

/// The bullet and the indent it reserves — 2 cells, matching one nesting level.
const BULLET: &str = "• ";

/// Indent step for a nesting level of list or quote, in cells.
const INDENT: usize = 2;

/// Blocks that stand alone with a blank line between siblings. This is a
/// *margin* table, and it stays a table: `display:block` (from the cascade)
/// decides that a box breaks the line, but the gap between two paragraphs is
/// `margin`, which M4 has no property for — M5's box model takes this over
/// along with the rest of the box model. Membership is exactly M3's, so
/// paragraph spacing does not shift under the cascade landing.
///
/// Everything else that is `display:block` — `div`, `li`, `tr`, `td`, the
/// table wrappers — breaks the line and takes no gap, which is what browsers
/// give them: zero margin. A blank line there is what once blanked out every
/// other row of Wikipedia's table of contents and every Hacker News story row.
const GAP: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "blockquote",
    "pre",
    "figure",
    "form",
];

/// Whether `display:none` is honoured (M4 review).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hidden {
    /// What the cascade said. The normal path.
    Respect,
    /// Render hidden boxes anyway — except the ones the user-agent sheet
    /// locked (`hidden_by_ua`: script, style, metadata). Only
    /// `layout_readable` asks for this, and only when respecting the cascade
    /// would leave the reader with a blank screen.
    Reveal,
}

/// Lay the document out, and if honouring `display:none` leaves nothing to
/// read, lay it out again revealing what the page hid. Returns the lines and
/// whether that fallback fired.
///
/// The pattern this exists for is everywhere: a page sets `html{display:none}`
/// or hides its content behind a `.js-loading` wrapper, and a script reveals
/// it on load. A browser runs the script; yata will not until M10. Honouring
/// the rule faithfully means a reader with a perfectly good document in memory
/// gets a blank screen — the worst outcome a browser has, and worse than
/// showing something the page meant to hide for a few hundred milliseconds.
///
/// The second pass costs a layout, and only on pages that came out empty.
pub fn layout_readable(dom: &Dom, styles: &Styles, width: u16) -> (Vec<Line>, bool) {
    let lines = layout(dom, styles, width, Hidden::Respect);
    if lines.iter().any(|line| !line.spans.is_empty()) {
        return (lines, false);
    }
    // Nothing came out. If the page has readable content behind its own
    // `display:none`, show that rather than a blank page; if it does not, the
    // page really is empty and the first answer stands.
    let revealed = layout(dom, styles, width, Hidden::Reveal);
    if revealed.iter().any(|line| !line.spans.is_empty()) {
        (revealed, true)
    } else {
        (lines, false)
    }
}

/// Lay the document out at `width` cells. Lines never exceed `width` — a
/// guarantee that holds while the content width left after the indent is above
/// zero — except inside `<pre>`, which does not wrap: clipping overflow is the
/// painter's job (M3.2). Where an indent has eaten the whole width, forward
/// progress wins over the bound, because one cell per line beats a hang.
pub fn layout(dom: &Dom, styles: &Styles, width: u16, hidden: Hidden) -> Vec<Line> {
    let mut l = Layouter {
        dom,
        styles,
        hidden,
        width: width as usize,
        out: Vec::new(),
        cur: Vec::new(),
        cur_cells: 0,
        line_has_text: false,
        pending_blank: false,
        pending_space: None,
        run: Vec::new(),
        run_cells: 0,
        indent: 0,
        list_depth: 0,
        marker: None,
    };
    // The walk starts at the document, not at `<body>`: `<head>` disappears
    // because ua.css says `display:none`, exactly as everything else does.
    // Starting lower would mean `<html>` and `<body>` never had their own
    // computed `display` consulted, so a page that hides itself with
    // `body { display: none }` would render as though it had not — a decision
    // `layout_readable` should make deliberately, not this function by
    // accident.
    for child in dom.children(dom.root) {
        l.walk(child, false);
    }
    l.flush(false);
    l.out
}

/// A node's computed values as terminal style. No nesting or inheritance here:
/// the cascade already did both, so a node's own values are final.
///
/// `background-color` is computed but not painted. Filling a text run's cells
/// while the gutter beside it stays bare looks worse than not filling at all;
/// real background fills need boxes, which is M5.
fn term_style(computed: &crate::style::ComputedStyle) -> Style {
    let mut attrs = Attrs::NONE;
    if computed.font_weight == FontWeight::Bold {
        attrs = attrs | Attrs::BOLD;
    }
    // M3 dropped italics on the grounds that terminals rendered them badly.
    // The UA sheet asks for them on `<i>`/`<em>`, and modern terminals oblige.
    if computed.font_style == FontStyle::Italic {
        attrs = attrs | Attrs::ITALIC;
    }
    if computed.underline {
        attrs = attrs | Attrs::UNDERLINE;
    }
    Style {
        fg: term_color(computed.color),
        bg: Color::Default,
        attrs,
    }
}

/// A computed colour as a terminal colour — with the one rule a terminal
/// browser needs and a real browser does not.
///
/// A page declares colours against a background it also controls. This browser
/// controls neither: the terminal's background could be anything, and there is
/// no portable way to ask. Honouring `color:#000000` faithfully paints black
/// text into a black terminal and the page disappears — and `#000` is what
/// Hacker News sets on every link, and near enough what most of the web sets
/// on its body copy.
///
/// So the two ends of the luminance range — where a colour risks matching the
/// background — fall back to the terminal's own foreground, which is legible
/// by definition. Everything between renders exactly as written:
/// HN `#000000` (0.00) and white `#ffffff` (1.00) become `Default`, while
/// Wikipedia's `#202122` (0.13) does too, and danluu's `#5c5cff` (0.41) and
/// example.com's `#334488` (0.27) keep their colour.
///
/// This is a readability choice over fidelity, and it is two constants wide if
/// the trade ever wants reversing.
fn term_color(color: ColorValue) -> Color {
    const TOO_DARK: f32 = 0.20;
    const TOO_LIGHT: f32 = 0.85;
    match color {
        ColorValue::Default => Color::Default,
        ColorValue::Rgb(r, g, b) => {
            // Relative luminance, ITU-R BT.709 coefficients.
            let luma =
                (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
            if !(TOO_DARK..=TOO_LIGHT).contains(&luma) {
                Color::Default
            } else {
                Color::Rgb(r, g, b)
            }
        }
    }
}

/// Collapsible whitespace per HTML, which is *not* `char::is_whitespace`: that
/// includes U+00A0 NBSP, whose whole job is to neither collapse nor wrap.
fn is_html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

/// One styled chunk of a buffered run, measured once as it is buffered. The text
/// is borrowed straight from the arena's text nodes — buffering a run must not
/// cost an allocation per word, and the tree outlives the layout that reads it.
struct Piece<'a> {
    text: &'a str,
    cells: usize,
    style: Style,
}

struct Layouter<'a> {
    dom: &'a Dom,
    /// The cascade's output for this tree: what every node looks like, and
    /// whether it renders at all.
    styles: &'a Styles,
    /// Whether `display:none` is honoured on this pass.
    hidden: Hidden,
    width: usize,
    out: Vec<Line>,
    /// Spans of the line being built.
    cur: Vec<Span>,
    /// Cells used on the current line, indent included.
    cur_cells: usize,
    /// The current line holds real text, not just its indent prefix. Distinct
    /// from `!cur.is_empty()`: wrapping must never push a prefix-only line, and
    /// a line never opens with a collapsed space.
    line_has_text: bool,
    /// A block gap is owed before the next line of content. Held rather than
    /// emitted so nested blocks collapse to one gap and a trailing gap — one
    /// nothing follows — never materializes at all.
    pending_blank: bool,
    /// A collapsed space is owed before the next word on this line, carrying the
    /// style of the element the whitespace sat inside.
    pending_space: Option<Style>,
    /// The unplaced run: one maximal stretch of non-whitespace text, which may
    /// cross text nodes and styles. Held whole rather than placed piece by piece
    /// because a line may only break where the source offers a break
    /// opportunity. Hacker News's `(<a>ziggit.dev</a>)` is three text nodes with
    /// no whitespace between them, and breaking between them stranded the `)`
    /// alone on a line; Wikipedia's `<a>wildcat</a>.<sup>[54]</sup>` did the same
    /// to its citation marks.
    run: Vec<Piece<'a>>,
    /// Cells the buffered run occupies. Summed as pieces arrive so each is
    /// measured exactly once (PLAN.md §4: no re-measuring per wrap attempt).
    run_cells: usize,
    /// Left indent in cells, from lists and blockquotes.
    indent: usize,
    /// Nesting depth of `<ul>`/`<ol>`, which decides whether a list takes a gap.
    list_depth: usize,
    /// Bullet owed to the first line of an `<li>`; later lines hang-indent.
    marker: Option<&'static str>,
}

impl<'a> Layouter<'a> {
    fn walk(&mut self, id: NodeId, pre: bool) {
        // Copied out so the borrow lives as long as the arena, not as long as
        // `&self` — the walk mutates `self` while reading node data, and the
        // run buffer holds `&'a str` into these text nodes across the walk.
        let dom: &'a Dom = self.dom;
        match &dom.node(id).data {
            NodeData::Text(text) => {
                // A text node carries its parent's inherited values (M4.2), so
                // its own computed style is the run's style.
                let style = term_style(self.styles.get(id));
                if pre {
                    self.pre_text(text, style)
                } else {
                    self.flow_text(text, style)
                }
            }
            NodeData::Element { tag, .. } => self.element(id, tag, pre),
            // Comments, doctypes and the document node never render.
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => {}
        }
    }

    fn element(&mut self, id: NodeId, tag: &str, pre: bool) {
        // The whole subtree goes. `<head>`, `<script>` and `<style>` land here
        // because ua.css says `display:none`, and so does anything a page
        // hides — layout no longer keeps a list of names it distrusts.
        //
        // On a `Reveal` pass the page's own hiding is ignored, but the
        // user-agent sheet's `!important` hiding is not: rescuing a page from
        // its own loading spinner must never mean printing its JavaScript.
        let computed = self.styles.get(id);
        if computed.display == Display::None
            && (self.hidden == Hidden::Respect || computed.hidden_by_ua)
        {
            return;
        }
        match tag {
            "br" => return self.hard_line_break(),
            "hr" => return self.rule(),
            _ => {}
        }

        let pre = pre || tag == "pre";
        let list = tag == "ul" || tag == "ol";
        // `display:block` decides that a box breaks the line either side of
        // itself; the GAP table decides whether it also owes a blank line. A
        // nested list takes no gap of its own — it is part of the item above
        // it, the way a browser zeroes the margin on a nested list.
        let boxed = self.styles.get(id).display == Display::Block;
        let block = boxed && GAP.contains(&tag) && !(list && self.list_depth > 0);

        if block {
            self.block_gap();
        } else if boxed {
            self.flush(false);
        }

        let indent = usize::from(list || tag == "blockquote") * INDENT;
        self.indent += indent;
        self.list_depth += usize::from(list);
        if tag == "li" {
            self.marker = Some(BULLET);
        }

        let dom = self.dom;
        for child in dom.children(id) {
            self.walk(child, pre);
        }

        // Closing the box comes before its indent and bullet are torn down: the
        // last run of an element is still unplaced at this point, and it belongs
        // to the box that produced it. Getting this order wrong loses the bullet
        // of every one-run `<li>` on danluu.com.
        if block {
            self.block_gap();
        } else if boxed {
            self.flush(false);
        }

        if tag == "li" {
            // An empty item must not lend its bullet to whatever comes next.
            self.marker = None;
        }
        self.list_depth -= usize::from(list);
        self.indent -= indent;
    }

    /// End the current line and owe a blank one before the next content.
    fn block_gap(&mut self) {
        self.flush(false);
        // Only between siblings: nothing precedes the first line, the gap owed
        // after the last block is never claimed, and a line that is already
        // blank — a `<br>` just above, say — is the separation, so `<br><p>` is
        // one break rather than two.
        if self.out.last().is_some_and(|l| !l.spans.is_empty()) {
            self.pending_blank = true;
        }
    }

    /// `<br>`: ends the line even when the line is empty, so `a<br><br>b` keeps
    /// the blank between the two breaks — the blank-line collapsing in
    /// `block_gap` is a property of block gaps, not a rule against two empty
    /// lines in a row.
    ///
    /// An owed block gap already stands for an empty line, so a `<br>` on top of
    /// one adds nothing: `</p><br>` is a gap, not a gap plus a blank. Consecutive
    /// `<br>`s are unaffected because the first leaves no gap pending. The trade
    /// is that `</p><br><br>` yields one blank line where a browser shows two;
    /// matching that would mean claiming the gap here and trimming trailing
    /// blanks at the end, which is more rule than that markup is worth.
    fn hard_line_break(&mut self) {
        self.flush(!self.pending_blank);
    }

    /// `<hr>`: a rule across the content width, so one inside a `<blockquote>`
    /// or an `<li>` lines up with the text it separates.
    fn rule(&mut self) {
        self.block_gap();
        let cells = self.width.saturating_sub(self.indent);
        if cells > 0 {
            self.take_pending_blank();
            let text = " ".repeat(self.indent) + &"─".repeat(cells);
            self.out.push(Line {
                spans: vec![Span {
                    text,
                    style: Style::default(),
                }],
            });
        }
        self.block_gap();
    }

    /// Text outside `<pre>`: runs of whitespace collapse to one space, and the
    /// spaces at a line box's edges are dropped rather than painted. Whitespace
    /// is also the only place a line may break, so each run of it commits the
    /// buffered run and owes a space before the next one.
    fn flow_text(&mut self, text: &'a str, style: Style) {
        if text.starts_with(is_html_space) {
            self.space(style);
        }
        let mut first = true;
        for word in text.split(is_html_space).filter(|w| !w.is_empty()) {
            if !first {
                self.space(style);
            }
            first = false;
            self.buffer(word, style);
        }
        // Owed to the next text node: `a <b>b</b>` is "a b" across the tags.
        if text.ends_with(is_html_space) {
            self.space(style);
        }
    }

    /// A break opportunity: place whatever run was being built, then owe a
    /// collapsed space before the next one.
    fn space(&mut self, style: Style) {
        self.commit_run();
        self.pending_space = Some(style);
    }

    /// Add a piece to the run being built, measuring it once.
    fn buffer(&mut self, text: &'a str, style: Style) {
        let cells = text.width();
        self.run_cells += cells;
        self.run.push(Piece { text, cells, style });
    }

    /// Place the buffered run, wrapping to the next line if it no longer fits.
    /// The run's measurement is the one taken as its pieces arrived, so no wrap
    /// attempt re-measures anything.
    fn commit_run(&mut self) {
        if self.run.is_empty() {
            return;
        }
        let sep = usize::from(self.pending_space.is_some());
        if self.line_has_text && self.cur_cells + sep + self.run_cells > self.width {
            // Wrapping consumes the space; the next line starts flush left.
            self.flush_line(false);
        }
        self.start_line();
        if let Some(space_style) = self.pending_space.take()
            && self.line_has_text
        {
            // The space wears the style of the element it sits *inside*: within
            // `<a>two words</a>` it is part of the link, so the underline runs
            // unbroken and the whole link collapses into one span (one
            // `put_str` for the painter). Between two adjacent links it belongs
            // to their parent, so the underline breaks the way a browser breaks
            // it. Keying off the neighbouring span's style instead would weld
            // Hacker News's "kensai" and "3 hours ago" links into one run.
            self.push_text(" ", 1, space_style);
        }
        if self.cur_cells + self.run_cells <= self.width {
            for piece in std::mem::take(&mut self.run) {
                self.push_text(piece.text, piece.cells, piece.style);
            }
            self.run_cells = 0;
            self.line_has_text = true;
        } else {
            self.break_run();
        }
    }

    /// A run wider than the line breaks at a cell boundary — danluu.com is full
    /// of unbreakable URLs. Never splits a double-width character, and always
    /// places at least one character per line: that guarantee is what makes the
    /// loop finite when the indent has already eaten the whole width (a 1-cell
    /// terminal is reachable, and a hang is a bug, not an error path).
    fn break_run(&mut self) {
        for piece in std::mem::take(&mut self.run) {
            let mut rest = piece.text;
            while !rest.is_empty() {
                // Pieces continue on the line the one before them ended on, so
                // a full line has to be closed here rather than overrun.
                if self.line_has_text && self.cur_cells >= self.width {
                    self.flush_line(false);
                }
                self.start_line();
                let limit = self.width.saturating_sub(self.cur_cells).max(1);
                let mut cells = 0;
                let mut end = 0;
                for (i, ch) in rest.char_indices() {
                    let w = ch.width().unwrap_or(0);
                    if end > 0 && cells + w > limit {
                        break;
                    }
                    cells += w;
                    end = i + ch.len_utf8();
                }
                self.push_text(&rest[..end], cells, piece.style);
                self.line_has_text = true;
                rest = &rest[end..];
                if !rest.is_empty() {
                    self.flush_line(false);
                }
            }
        }
        self.run_cells = 0;
    }

    /// Text inside `<pre>`, where whitespace is content: newlines break lines,
    /// everything else lands verbatim and nothing wraps.
    fn pre_text(&mut self, text: &str, style: Style) {
        for (i, seg) in text.split('\n').enumerate() {
            if i > 0 {
                self.flush(true);
            }
            if seg.is_empty() {
                continue;
            }
            self.start_line();
            self.push_text(seg, seg.width(), style);
            self.line_has_text = true;
        }
    }

    /// Open a line if one isn't open: claim any owed blank and lay the indent
    /// down as spaces. Called only when text is about to land, so an indent is
    /// never painted on a line of its own.
    fn start_line(&mut self) {
        if !self.cur.is_empty() {
            return;
        }
        self.take_pending_blank();
        let prefix = match self.marker.take() {
            // The bullet sits inside the indent it reserves: "• " at the top
            // level, "  • " one level in.
            Some(m) => " ".repeat(self.indent.saturating_sub(m.width())) + m,
            None => " ".repeat(self.indent),
        };
        if !prefix.is_empty() {
            let cells = prefix.width();
            self.push_text(&prefix, cells, Style::default());
        }
    }

    fn take_pending_blank(&mut self) {
        if self.pending_blank {
            self.out.push(Line::default());
            self.pending_blank = false;
        }
    }

    /// Append to the previous span when the style matches, so a line is a
    /// handful of spans rather than one per word — the painter emits one write
    /// per span. `cells` is the caller's already-taken measurement.
    fn push_text(&mut self, text: &str, cells: usize, style: Style) {
        match self.cur.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => self.cur.push(Span {
                text: text.to_string(),
                style,
            }),
        }
        self.cur_cells += cells;
    }

    /// End the current line. `force` emits an empty line when there is nothing
    /// to end — what `<br>` needs and what a block boundary must not do. Any
    /// buffered run lands first: every line boundary is also a break
    /// opportunity, so nothing may be left unplaced across one.
    fn flush(&mut self, force: bool) {
        self.commit_run();
        self.flush_line(force);
    }

    /// Close the line without touching the run buffer — the wrap primitive for
    /// `commit_run` and `break_run`, which call it while a run is mid-placement
    /// and must not re-enter the commit.
    fn flush_line(&mut self, force: bool) {
        if self.cur.is_empty() {
            if force {
                self.out.push(Line::default());
            }
        } else {
            self.out.push(Line {
                spans: std::mem::take(&mut self.cur),
            });
        }
        self.cur_cells = 0;
        self.line_has_text = false;
        self.pending_space = None;
    }
}

/// Lines as text, with styles as markers: `[b]bold[/]`, `[u #5c5cff]link[/]`,
/// unstyled runs verbatim. Tests pin rendering as a string this way, and M3.2's
/// painter tests reuse it. No escaping — fixtures must not contain `[`.
pub fn debug_lines(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            let mut markers = String::new();
            if span.style.attrs.contains(Attrs::BOLD) {
                markers.push('b');
            }
            if span.style.attrs.contains(Attrs::UNDERLINE) {
                markers.push('u');
            }
            if span.style.attrs.contains(Attrs::ITALIC) {
                markers.push('i');
            }
            let color = match span.style.fg {
                Color::Ansi(n) => format!("c{n}"),
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                Color::Default => String::new(),
            };
            if markers.is_empty() && color.is_empty() {
                out.push_str(&span.text);
            } else {
                let sep = if !markers.is_empty() && !color.is_empty() {
                    " "
                } else {
                    ""
                };
                out.push_str(&format!("[{markers}{sep}{color}]{}[/]", span.text));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse;

    /// Lay a tree out with **user-agent styling only**, shadowing the real
    /// `layout` for the tests below. Most of them are about boxes and
    /// wrapping, and threading an empty stylesheet through every call site
    /// would say nothing; the tests that are about the cascade build their
    /// styles explicitly (see `lines_styled`).
    fn layout(dom: &Dom, width: u16) -> Vec<Line> {
        let styles = crate::style::style_tree(dom, &[]);
        super::layout(dom, &styles, width, Hidden::Respect)
    }

    /// Lay `html` out at `width` and render it with style markers.
    fn lines(html: &str, width: u16) -> String {
        debug_lines(&layout(&parse(html), width))
    }

    /// The same, but with `css` as the page's stylesheet — the cascade
    /// reaching layout, which is what M4.4 added.
    fn lines_styled(html: &str, css: &str, width: u16) -> String {
        let dom = parse(html);
        let sheet = crate::css::parse(css);
        let styles = crate::style::style_tree(&dom, &[&sheet]);
        debug_lines(&super::layout(&dom, &styles, width, Hidden::Respect))
    }

    /// Cell width of a laid-out line, spans included.
    fn cells(line: &Line) -> usize {
        line.spans.iter().map(|s| s.text.width()).sum()
    }

    #[test]
    fn blocks_are_separated_by_one_blank_line() {
        assert_eq!(lines("<p>one</p><p>two</p>", 20), "one\n\ntwo\n");
    }

    #[test]
    fn nested_blocks_do_not_inflate_the_gap() {
        // Two divs deep is still one blank line, and no gap leads or trails.
        assert_eq!(
            lines("<div><div><p>a</p></div></div><p>b</p>", 20),
            "a\n\nb\n"
        );
    }

    #[test]
    fn zero_margin_containers_break_the_line_without_a_gap() {
        // A browser gives `<div>` and friends no margin, so they must not open
        // a blank line the way `<p>` does. Wikipedia's table of contents is a
        // `<div>` inside every `<li>`; a gap there blanked out every other row.
        assert_eq!(lines("<div>a</div><div>b</div>", 20), "a\nb\n");
        assert_eq!(
            lines("<nav><a href=x>a</a></nav><p>b</p>", 20),
            "[u #5c5cff]a[/]\n\nb\n"
        );
        assert_eq!(
            lines(
                "<ul><li><a href=x><div>1 Etymology</div></a></li>\
                 <li><a href=y><div>2 Taxonomy</div></a></li></ul>",
                20
            ),
            "• [u #5c5cff]1 Etymology[/]\n• [u #5c5cff]2 Taxonomy[/]\n"
        );
    }

    #[test]
    fn an_empty_container_between_cells_adds_no_blank_line() {
        // Hacker News's story row, trimmed to its shape: the rank cell, an
        // empty vote-arrow `<div>` inside a `<center>`, then the title cell.
        // The empty container used to owe a gap, which landed as a blank line
        // inside every one of the thirty stories.
        assert_eq!(
            lines(
                "<table><tr><td><span>1.</span></td>\
                 <td><center><a href=v><div></div></a></center></td>\
                 <td><a href=x>Title</a></td></tr></table>",
                20
            ),
            "1.\n[u #5c5cff]Title[/]\n"
        );
    }

    #[test]
    fn whitespace_between_blocks_contributes_nothing() {
        assert_eq!(
            lines("<p>a</p>\n  \n<p>b</p>", 20),
            lines("<p>a</p><p>b</p>", 20)
        );
    }

    #[test]
    fn br_breaks_without_a_blank_and_two_brs_keep_theirs() {
        assert_eq!(lines("<p>a<br>b</p>", 20), "a\nb\n");
        // Each <br> breaks, so the line between them survives — the block-gap
        // collapsing must not be implemented as "never two blank lines".
        assert_eq!(lines("a<br><br>b", 20), "a\n\nb\n");
    }

    #[test]
    fn br_does_not_stack_with_a_block_gap() {
        // The gap a block already owes is an empty line; the <br> must not add
        // a second one on top of it.
        assert_eq!(lines("<p>a</p><br>b", 20), "a\n\nb\n");
        assert_eq!(lines("<p>a</p><br><p>b</p>", 20), "a\n\nb\n");
        // A leading <br> is one break, and the block after it adds none.
        assert_eq!(lines("<br><p>b</p>", 20), "\nb\n");
        // Nor does a trailing <br> leave the page ending in a blank line.
        assert_eq!(lines("<p>a</p><br>", 20), "a\n");
    }

    #[test]
    fn words_wrap_on_word_boundaries() {
        let out = layout(&parse("<p>aaa bbb ccc ddd</p>"), 10);
        assert_eq!(debug_lines(&out), "aaa bbb\nccc ddd\n");
        assert!(out.iter().all(|l| cells(l) <= 10));
    }

    #[test]
    fn no_line_ends_in_a_space_at_a_wrap() {
        // The space at a wrap point is consumed, not painted: a trailing space
        // is invisible here but paints as a stray styled cell in M3.2.
        let out = layout(&parse("<p>aaaaa bbbbb ccccc</p>"), 11);
        assert_eq!(debug_lines(&out), "aaaaa bbbbb\nccccc\n");
        assert!(!out.iter().any(|l| {
            l.spans
                .last()
                .is_some_and(|s| s.text.ends_with(is_html_space))
        }));
    }

    #[test]
    fn a_run_glued_across_tags_wraps_as_one_unit() {
        // Hacker News's site bit is `(`, a link, `)` — three text nodes with no
        // whitespace between them. A line may only break where the source
        // offers a break opportunity, so the run moves down whole rather than
        // stranding the `)` on a line of its own.
        assert_eq!(
            lines("<p>aaa (<a href=x>bbbbb</a>)</p>", 10),
            "aaa\n([u #5c5cff]bbbbb[/])\n"
        );
        // Same rule around Wikipedia's citation marks: the full stop and the
        // superscript stay welded to the word they follow.
        assert_eq!(
            lines("<p>aaaa <a href=x>bbb</a>.<sup>[54]</sup></p>", 10),
            "aaaa\n[u #5c5cff]bbb[/].[54]\n"
        );
    }

    #[test]
    fn a_glued_run_wider_than_the_line_hard_breaks_and_keeps_its_styles() {
        // The unit is unbreakable only at break *opportunities*; one wider than
        // the whole line still splits at cell boundaries, and each piece keeps
        // the style it came in with.
        assert_eq!(
            lines("<p><a href=x>aaaa</a>bbbb</p>", 3),
            "[u #5c5cff]aaa[/]\n[u #5c5cff]a[/]bb\nbb\n"
        );
    }

    #[test]
    fn a_long_word_hard_breaks_at_the_cell_boundary() {
        let word = "x".repeat(30);
        let out = layout(&parse(&format!("<p>{word}</p>")), 10);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|l| cells(l) == 10));
    }

    #[test]
    fn wide_chars_wrap_by_cell_width_not_char_count() {
        // Six 2-cell chars at width 10: five fit, the sixth wraps.
        let out = layout(&parse("<p>你好世界测试</p>"), 10);
        assert_eq!(debug_lines(&out), "你好世界测\n试\n");
        assert_eq!(cells(&out[0]), 10);
    }

    #[test]
    fn a_double_width_char_is_never_split_at_an_odd_width() {
        // Five 2-cell chars at width 9: four fit in 8 cells, the odd cell goes
        // unused. A break computed on char indices passes the even case above
        // and fails this one.
        let out = layout(&parse("<p>你好世界测</p>"), 9);
        assert_eq!(debug_lines(&out), "你好世界\n测\n");
        assert_eq!(cells(&out[0]), 8);
        assert_eq!(cells(&out[1]), 2);
    }

    #[test]
    fn degenerate_widths_terminate() {
        // The indent alone exceeds the width here, so the fit test can never
        // succeed; one character per line is what keeps the loop finite.
        let html = "<ul><li>hello world<ul><li>deep</li></ul></li></ul>";
        for width in [0, 1] {
            let out = layout(&parse(html), width);
            assert!(!out.is_empty());
            assert!(out.iter().all(|l| !l.spans.is_empty()));
        }
    }

    #[test]
    fn list_items_are_bulleted_with_a_hanging_indent() {
        // Items sit on consecutive lines — a blank between each would double
        // danluu.com's 196-item list, the page the M3 demo gate names.
        assert_eq!(
            lines("<ul><li>alpha beta gamma</li><li>x</li></ul>", 10),
            "• alpha\n  beta\n  gamma\n• x\n"
        );
    }

    #[test]
    fn nested_lists_indent_two_cells_per_level() {
        assert_eq!(
            lines("<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>", 20),
            "• a\n  • b\n• c\n"
        );
    }

    #[test]
    fn blockquote_indents_its_content() {
        assert_eq!(lines("<blockquote><p>a b</p></blockquote>", 20), "  a b\n");
    }

    #[test]
    fn hr_is_a_rule_across_the_content_width() {
        assert_eq!(lines("<p>a</p><hr><p>b</p>", 6), "a\n\n──────\n\nb\n");
        // Indented, it lines up with the text it separates.
        assert_eq!(lines("<blockquote><hr></blockquote>", 6), "  ────\n");
    }

    #[test]
    fn pre_preserves_whitespace_and_may_exceed_the_width() {
        assert_eq!(lines("<pre>a  b\n  c</pre>", 5), "a  b\n  c\n");
        let out = layout(&parse("<pre>abcdefgh</pre>"), 5);
        assert_eq!(out.len(), 1);
        assert_eq!(cells(&out[0]), 8);
    }

    #[test]
    fn headings_are_bold_and_links_are_underlined_and_colored() {
        assert_eq!(lines("<h1>Hi</h1>", 20), "[b]Hi[/]\n");
        assert_eq!(
            lines("<p>see <a href=x>docs</a></p>", 20),
            "see [u #5c5cff]docs[/]\n"
        );
    }

    #[test]
    fn a_space_inside_a_link_carries_the_link_style() {
        // The underline runs unbroken through the link — one span, so the
        // painter writes the whole link in one call — while the space after it
        // is neutral and merges with the text that follows.
        let out = layout(&parse("<p><a href=x>two words</a> after</p>"), 40);
        assert_eq!(debug_lines(&out), "[u #5c5cff]two words[/] after\n");
        assert_eq!(out[0].spans.len(), 2);
    }

    #[test]
    fn styles_nest() {
        assert_eq!(lines("<b><a href=x>x</a></b>", 20), "[bu #5c5cff]x[/]\n");
    }

    #[test]
    fn whitespace_collapses_across_tags() {
        // Two spaces become one. The space sits *inside* the <b>, so it wears
        // the bold — the same reason the underline of `<u> x</u>` starts at the
        // space in a browser. Either way it renders as "a b".
        assert_eq!(lines("a<b>  b</b>", 20), "a[b] b[/]\n");
        // Outside the tag, the space is neutral and merges with the text before.
        assert_eq!(lines("a <b>b</b>", 20), "a [b]b[/]\n");
        // No whitespace between the tags means no space in the output either.
        assert_eq!(lines("a<b>b</b>", 20), "a[b]b[/]\n");
    }

    #[test]
    fn adjacent_links_keep_separate_underlines() {
        // Hacker News's byline is two links with a space between them; the space
        // belongs to their parent, so the underline breaks the way a browser
        // breaks it rather than welding them into one run.
        assert_eq!(
            lines("<p><a href=u>kensai</a> <a href=i>3 hours ago</a></p>", 40),
            "[u #5c5cff]kensai[/] [u #5c5cff]3 hours ago[/]\n"
        );
    }

    // ---- the cascade reaching layout (M4.4) -------------------------------

    #[test]
    fn inline_list_items_keep_the_space_the_source_put_between_them() {
        // The case M5.0 exists for: Wikipedia's navbox hlists are `<li>`s made
        // inline by the page's own CSS, one per source line. The newline
        // between them is the space between the words — drop it in the parser
        // and the items render as `AnatomyGenetics`.
        assert_eq!(
            lines_styled(
                "<ul><li>a</li>\n<li>b</li>\n<li>c</li></ul>",
                "li { display: inline }",
                20
            ),
            // The two leading cells are the list indent, which is still
            // tag-driven — M4 has no `padding`, so M5's box model is what will
            // let a page zero it. The point here is the spaces *between* the
            // items, which only exist because the parser kept the newlines.
            "  a b c\n"
        );
        // Blocks are unaffected: the break already separated them, and the
        // whitespace node must not add a stray space or a blank line.
        assert_eq!(lines("<ul><li>a</li>\n<li>b</li></ul>", 20), "• a\n• b\n");
    }

    // ---- never a blank page (M4 review) -----------------------------------

    fn readable(html: &str, css: &str, width: u16) -> (String, bool) {
        let dom = parse(html);
        let sheet = crate::css::parse(css);
        let styles = crate::style::style_tree(&dom, &[&sheet]);
        let (lines, revealed) = layout_readable(&dom, &styles, width);
        (debug_lines(&lines), revealed)
    }

    #[test]
    fn a_page_that_hides_itself_is_shown_anyway() {
        // The anti-FOUC pattern: hide everything, reveal it from a script. A
        // browser runs the script; yata will not until M10, so honouring the
        // rule faithfully hands the reader a blank screen with the article
        // sitting in memory.
        for hider in [
            "html { display: none }",
            "body { display: none }",
            // The wrapper form, which a rule about <html>/<body> would miss.
            ".js-loading { display: none }",
        ] {
            let (text, revealed) = readable(
                "<body><div class='js-loading'><p>the article</p></div></body>",
                hider,
                30,
            );
            assert_eq!(text, "the article\n", "{hider}");
            assert!(revealed, "{hider} must report that it fell back");
        }
    }

    #[test]
    fn rescuing_a_page_never_prints_its_code() {
        // The fallback ignores the page's hiding, not the user-agent sheet's:
        // ua.css marks script/style `display:none !important`, and revealing
        // those would dump JavaScript into what someone is reading.
        let (text, revealed) = readable(
            "<body style='display:none'><script>var secret = 1</script><p>text</p></body>",
            "",
            30,
        );
        assert_eq!(text, "text\n");
        assert!(revealed);
        assert!(!text.contains("secret"));
    }

    #[test]
    fn a_page_with_nothing_to_show_stays_empty() {
        // No text anywhere: the fallback finds nothing either, so the honest
        // answer is the empty one — no flag, no second-guessing.
        let (text, revealed) = readable("<body><script>var x = 1</script></body>", "", 30);
        assert_eq!(text, "");
        assert!(!revealed, "an empty page is not a hidden page");
    }

    #[test]
    fn a_normal_page_never_takes_the_fallback() {
        // The common case must not pay for the rescue, and hidden sections of
        // a page that renders fine stay hidden.
        let (text, revealed) = readable(
            "<p>visible</p><p class=ad>hidden</p>",
            ".ad { display: none }",
            30,
        );
        assert_eq!(text, "visible\n");
        assert!(!revealed);
    }

    #[test]
    fn a_page_can_hide_its_own_content() {
        // Half the M4 demo gate. `display:none` is no longer a list of tag
        // names layout distrusts — it is whatever the cascade computed, so a
        // page hiding its own nav works the same way <script> does.
        assert_eq!(
            lines_styled(
                "<p>seen</p><div class=ad><p>gone</p></div><p>also seen</p>",
                ".ad { display: none }",
                20
            ),
            "seen\n\nalso seen\n"
        );
    }

    #[test]
    fn display_decides_what_breaks_the_line_not_the_tag_name() {
        // A span made block breaks either side of itself...
        assert_eq!(
            lines_styled("a<span>b</span>c", "span { display: block }", 20),
            "a\nb\nc\n"
        );
        // ...and a paragraph made inline stops breaking, gap and all. The two
        // words glue together because the source has no whitespace between
        // `</p>` and `<p>` — which is what a browser does with inline boxes
        // too, and the reason the spaced variant below is the interesting one.
        assert_eq!(
            lines_styled("<p>a</p><p>b</p>", "p { display: inline }", 20),
            "ab\n"
        );
        // The contrast is the point: same markup, one stylesheet apart. (The
        // words glue because the tree builder drops whitespace between two
        // blocks — there is no space node in the DOM to collapse.)
        assert_eq!(lines("<p>a</p><p>b</p>", 20), "a\n\nb\n");
    }

    #[test]
    fn a_pages_colour_reaches_the_span_and_inherits_into_it() {
        assert_eq!(
            lines_styled("<p>text</p>", "p { color: #348 }", 20),
            "[#334488]text[/]\n"
        );
        // Bold from the UA sheet, colour inherited from the block: the cascade
        // composed them, and layout no longer nests styles itself.
        assert_eq!(
            lines_styled("<p>a <b>b</b></p>", "p { color: #348 }", 20),
            "[#334488]a [/][b #334488]b[/]\n"
        );
    }

    #[test]
    fn em_is_italic_now_that_the_ua_sheet_says_so() {
        // A deliberate change from M3, which dropped italics on the grounds
        // that terminals rendered them badly. ua.css asks for them; modern
        // terminals oblige.
        assert_eq!(lines("<em>x</em>", 20), "[i]x[/]\n");
        assert_eq!(lines("<i>x</i>", 20), "[i]x[/]\n");
    }

    #[test]
    fn colours_that_could_vanish_fall_back_to_the_terminals_own() {
        // Black text into a black terminal is the failure this prevents, and
        // `#000` is exactly what Hacker News sets on every link. White is the
        // same problem on a light terminal. Both render as the terminal's
        // foreground — no marker at all.
        assert_eq!(
            lines_styled("<p>hn</p>", "p { color: #000000 }", 20),
            "hn\n"
        );
        assert_eq!(lines_styled("<p>w</p>", "p { color: #ffffff }", 20), "w\n");
        // Wikipedia's body colour is near-black too, and would be unreadable.
        assert_eq!(lines_styled("<p>w</p>", "p { color: #202122 }", 20), "w\n");
        // Anything with room on both sides keeps its colour, including the UA
        // sheet's own link blue.
        assert_eq!(
            lines_styled("<p>d</p>", "p { color: #5c5cff }", 20),
            "[#5c5cff]d[/]\n"
        );
        assert_eq!(
            lines_styled("<p>e</p>", "p { color: #348 }", 20),
            "[#334488]e[/]\n"
        );
    }

    #[test]
    fn background_colour_is_computed_but_not_painted_yet() {
        // M5 owns background fills. Filling a text run's cells while the
        // gutter beside it stays bare looks worse than not filling at all, so
        // the value is carried and ignored — deliberately, not by omission.
        assert_eq!(
            lines_styled("<p>x</p>", "p { background-color: #eee }", 20),
            "x\n"
        );
    }

    #[test]
    fn script_and_style_contribute_nothing() {
        assert_eq!(
            lines(
                "<p>a</p><script>var x = 1;</script><style>p{color:red}</style><p>b</p>",
                20
            ),
            "a\n\nb\n"
        );
    }

    #[test]
    fn unknown_tags_flow_inline() {
        // danluu.com's list, verbatim: its homemade <d> date tag is unclassified
        // and so must not break the line — the date sits beside its link, not
        // above it. The two run together because the source has no space between
        // them and M3 has no CSS to add one; M4's cascade is what fixes that.
        assert_eq!(
            lines("<ul><li><d>07/26</d><a href=x>Post</a></li></ul>", 20),
            "• 07/26[u #5c5cff]Post[/]\n"
        );
    }

    #[test]
    fn comments_and_doctypes_render_nothing() {
        assert_eq!(lines("<!doctype html><!-- hi --><p>a</p>", 20), "a\n");
    }
}

/// Ladder readability tests (M3.3): every committed fixture lays out at the
/// column width without panicking, and each page's own structure — the shape a
/// reader judges it by — is pinned per fixture. Assertions rather than cell-grid
/// snapshots: snapshots start at M5 (PLAN.md), and a 3600-line Wikipedia grid
/// would be re-blessed rather than read.
#[cfg(test)]
mod ladder {
    use super::*;
    use crate::html::parse;

    /// The column these tests measure against — `--dump-text`'s fixed width, so
    /// a failure here reproduces from the command line.
    const COLUMN: u16 = 80;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    /// A line's text, spans concatenated — what the reader sees.
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn cells(line: &Line) -> usize {
        line.spans.iter().map(|s| s.text.width()).sum()
    }

    /// The user-agent sheet's link styling as it reaches a cell: `#5c5cff`
    /// (the RGB of ANSI 12, which M3 hardcoded) and underlined.
    fn is_link(span: &Span) -> bool {
        span.style.fg == Color::Rgb(0x5c, 0x5c, 0xff) && span.style.attrs.contains(Attrs::UNDERLINE)
    }

    /// Whether the tree holds a `<pre>` anywhere. Asked of the parsed tree, not
    /// of the source text: `contains("<pre")` also fires on `<presentation>` and
    /// on any attribute value that happens to spell it.
    fn has_pre(dom: &Dom) -> bool {
        let mut stack = vec![dom.root];
        while let Some(id) = stack.pop() {
            if let NodeData::Element { tag, .. } = &dom.node(id).data
                && tag == "pre"
            {
                return true;
            }
            stack.extend(dom.children(id));
        }
        false
    }

    /// Every page's invariants, then the lines for the per-page assertions.
    fn check(html: &str, min_lines: usize) -> Vec<Line> {
        let dom = parse(html);
        // The width bound below applies to every line only because no ladder
        // page has a `<pre>`, the one box that may overflow the column. If a
        // refreshed fixture ever brings one in, this says why the bound broke.
        assert!(!has_pre(&dom), "fixture gained a <pre>");
        // Ladder pages lay out with their own inline CSS, exactly as
        // `--dump-text` renders them.
        let sheets = crate::style::sources::inline_sheets(&dom);
        let styles = crate::style::style_tree(&dom, &sheets.iter().collect::<Vec<_>>());
        let lines = super::layout(&dom, &styles, COLUMN, Hidden::Respect);
        assert!(lines.len() >= min_lines, "only {} lines", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let cells = cells(line);
            assert!(
                cells <= COLUMN as usize,
                "line {i} is {cells} cells: {line:?}"
            );
            if let Some(last) = line.spans.last() {
                assert!(
                    !last.text.ends_with(is_html_space),
                    "line {i} ends in whitespace: {line:?}"
                );
            }
            // Two blank lines in a row is the signature of a gap bug — a block
            // owing a separation it never earned. One is a paragraph break.
            if i > 0 {
                assert!(
                    !(line.spans.is_empty() && lines[i - 1].spans.is_empty()),
                    "blank lines {} and {i} run together",
                    i - 1
                );
            }
        }
        lines
    }

    #[test]
    fn example_com() {
        let lines = check(fixture!("example.com.html"), 3);
        assert_eq!(text(&lines[0]), "Example Domain");
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| s.style.attrs.contains(Attrs::BOLD))
        );
    }

    #[test]
    fn motherfuckingwebsite_com() {
        let lines = check(fixture!("motherfuckingwebsite.com.html"), 20);
        let bold = |l: &Line| l.spans.iter().all(|s| s.style.attrs.contains(Attrs::BOLD));

        // The page opens `<h1>`, `<aside>`, `<h2>` — one line each, one blank
        // between, and the two headings bold while the aside is not.
        assert_eq!(text(&lines[0]), "This is a motherfucking website.");
        assert!(bold(&lines[0]), "h1 not bold: {:?}", lines[0]);
        assert_eq!(text(&lines[1]), "");
        assert_eq!(text(&lines[2]), "And it's fucking perfect.");
        assert!(!bold(&lines[2]), "the aside must not be bold");
        assert_eq!(text(&lines[3]), "");
        assert_eq!(
            text(&lines[4]),
            "Seriously, what the fuck else do you want?"
        );
        assert!(bold(&lines[4]), "h2 not bold: {:?}", lines[4]);

        // And two adjacent `<p>`s — the deliverable's "paragraphs separated by
        // exactly one blank line", which the opening three blocks do not
        // actually contain. The first wraps, so this also pins that the blank
        // falls after its *last* line, not after its first.
        let end = lines
            .iter()
            .position(|l| text(l) == "see that shit, but they don't see any of your shitty shit.")
            .expect("the first paragraph's last line must be present");
        assert_eq!(text(&lines[end + 1]), "");
        assert_eq!(
            text(&lines[end + 2]),
            "You never knew it, but this is your perfect website. Here's why."
        );
    }

    #[test]
    fn danluu_com() {
        let lines = check(fixture!("danluu.com.html"), 196);
        // Every post title is a link span, carrying the link style the painter
        // turns into underline + color.
        let title = "Steve Ballmer was an underrated CEO";
        let span = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.text == title)
            .expect("post title must survive layout as its own span");
        assert!(
            is_link(span),
            "post title is not styled as a link: {span:?}"
        );

        // The body wraps to the column rather than to the source's line breaks:
        // the one title too long for a line fills it and continues, indented,
        // under the bullet.
        let long = lines
            .iter()
            .position(|l| text(l).contains("Agentic test processes"))
            .expect("the long title must be present");
        assert!(
            cells(&lines[long]) > 70,
            "the wrapped line only fills {} of {COLUMN} cells",
            cells(&lines[long])
        );
        assert_eq!(text(&lines[long + 1]), "  from Galapagos Island");
    }

    #[test]
    fn news_ycombinator_com() {
        let lines = check(fixture!("news.ycombinator.com.html"), 30);
        // Thirty stories, in order, each a rank line followed by its title as a
        // styled link. Hacker News is nested tables all the way down, so this is
        // the page that would show table soup scrambling or dropping content.
        let mut from = 0;
        for rank in 1..=30 {
            let marker = format!("{rank}.");
            let at = lines[from..]
                .iter()
                .position(|l| text(l) == marker)
                .map(|i| i + from)
                .unwrap_or_else(|| panic!("story {rank} missing or out of order"));
            let title = &lines[at + 1];
            assert!(
                title.spans.first().is_some_and(is_link),
                "story {rank}'s title is not a link line: {title:?}"
            );
            from = at + 1;
        }
        // And the site bit stays welded to its parentheses instead of wrapping
        // a lone `)` onto the next line.
        assert!(
            lines.iter().any(|l| text(l) == "(ziggit.dev)"),
            "the site bit was split across lines"
        );
    }

    #[test]
    fn en_wikipedia_org() {
        // The big one: it must lay out at all, keep its first heading, and stay
        // in the same ballpark of lines. The range is wide enough to survive
        // wording changes in a refreshed fixture and tight enough that a gap or
        // collapse regression — the kind that added 130 blank lines before this
        // task — moves it out of range.
        let lines = check(fixture!("en.wikipedia.org.html"), 100);
        assert!(
            (3_000..4_500).contains(&lines.len()),
            "{} lines is outside the pinned range",
            lines.len()
        );
        // The article's own headings, bold and in order. "First heading" means
        // the article's `<h1>`, not the first bold line on the page: Wikipedia's
        // sidebar chrome ("Contents") is a heading too and comes before it in
        // source order, which is what a reader sees scrolling from the top.
        let heading = |want: &str| {
            lines
                .iter()
                .position(|l| {
                    text(l) == want && l.spans.iter().all(|s| s.style.attrs.contains(Attrs::BOLD))
                })
                .unwrap_or_else(|| panic!("{want:?} missing, or not bold"))
        };
        assert!(
            heading("Cat") < heading("Etymology and naming"),
            "the article title must come before its first section"
        );
    }
}
