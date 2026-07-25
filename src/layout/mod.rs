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
//! This is M3's whole style system: a hardcoded table of what `<h1>` and `<a>`
//! look like. M4 replaces it with a user-agent stylesheet and a real cascade.

use crate::dom::{Dom, NodeData, NodeId};
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

/// Link color. `layout` is pure and cannot see `detect_caps`, so it emits an
/// ANSI index and leaves truecolor mapping to M4. 12 is bright blue: legible on
/// both light and dark terminals, and distinct from the `Default` foreground.
const LINK: Color = Color::Ansi(12);

/// The bullet and the indent it reserves — 2 cells, matching one nesting level.
const BULLET: &str = "• ";

/// Indent step for a nesting level of list or quote, in cells.
const INDENT: usize = 2;

/// Subtrees that contribute nothing. `script`/`style` hold code, not prose, and
/// every ladder page has both; `template` is inert by definition; `head` is
/// skipped by starting at `<body>`, and listed so a stray in-body `<head>` from
/// the builder's recovery stays silent too.
const SKIP: &[&str] = &["head", "script", "style", "template"];

/// Blocks that stand alone: a line break either side and one blank line between
/// siblings. `section`/`figure`/`figcaption`/`header`/`footer`/`nav`/`main`/
/// `aside` are Wikipedia's and motherfuckingwebsite's structural wrappers,
/// `center`/`form` are Hacker News's, and `table`/`tbody` are stacked blocks
/// until M5 gives tables a grid. Tags not on the ladder are left unclassified
/// and flow inline — danluu.com's homemade `<d>` date tag has to sit on the same
/// line as the link beside it.
const BLOCK: &[&str] = &[
    "p",
    "div",
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
    "table",
    "tbody",
    "section",
    "figure",
    "figcaption",
    "header",
    "footer",
    "nav",
    "main",
    "aside",
    "form",
    "center",
];

/// Blocks that break the line but take no blank-line gap. Items come in runs: a
/// blank between every one would double the length of danluu.com's 196-item
/// link list, and danluu is the page the M3 demo gate calls "comfortably
/// readable". Nested `<ul>`/`<ol>` join them (see `Layouter::element`) for the
/// same reason browsers zero the margin on a nested list.
const TIGHT: &[&str] = &["li", "tr", "td", "th"];

/// Lay the document out at `width` cells. Lines never exceed `width` except
/// inside `<pre>`, which does not wrap — clipping overflow is the painter's job
/// (M3.2). At a width too small to hold even the indent, forward progress wins
/// over the width bound: one cell per line beats an infinite loop.
pub fn layout(dom: &Dom, width: u16) -> Vec<Line> {
    let mut l = Layouter {
        dom,
        width: width as usize,
        out: Vec::new(),
        cur: Vec::new(),
        cur_cells: 0,
        line_has_text: false,
        pending_blank: false,
        pending_space: None,
        indent: 0,
        list_depth: 0,
        marker: None,
    };
    // The builder synthesizes a `<body>` for every page, including danluu.com,
    // which ships without the tag. The fallback keeps `layout` total for a
    // hand-built arena rather than pretending a missing body is an error.
    let start = body(dom).unwrap_or(dom.root);
    for child in dom.children(start) {
        l.walk(child, Style::default(), false);
    }
    l.flush(false);
    l.out
}

/// The first `<body>` element in document order.
fn body(dom: &Dom) -> Option<NodeId> {
    let mut stack = vec![dom.root];
    while let Some(id) = stack.pop() {
        if let NodeData::Element { tag, .. } = &dom.node(id).data
            && tag == "body"
        {
            return Some(id);
        }
        // Reversed so the walk leaves the arena in document order.
        let kids: Vec<NodeId> = dom.children(id).collect();
        stack.extend(kids.into_iter().rev());
    }
    None
}

/// Default styling — M3's entire style system, folded into the user-agent
/// stylesheet in M4. `i`/`em`/`code` deliberately pass through unstyled: the
/// terminal's italic is widely unsupported and a cell grid has no second
/// typeface to switch to.
fn tag_style(tag: &str, inherited: Style) -> Style {
    match tag {
        // Headings and bold are the only emphasis M3 has; styles nest, so a
        // bold link ends up BOLD | UNDERLINE with the link color.
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "b" | "strong" => Style {
            attrs: inherited.attrs | Attrs::BOLD,
            ..inherited
        },
        // Underline carries the affordance where color is unavailable.
        "a" => Style {
            fg: LINK,
            attrs: inherited.attrs | Attrs::UNDERLINE,
            ..inherited
        },
        _ => inherited,
    }
}

/// Collapsible whitespace per HTML, which is *not* `char::is_whitespace`: that
/// includes U+00A0 NBSP, whose whole job is to neither collapse nor wrap.
fn is_html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

struct Layouter<'a> {
    dom: &'a Dom,
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
    /// Left indent in cells, from lists and blockquotes.
    indent: usize,
    /// Nesting depth of `<ul>`/`<ol>`, which decides whether a list takes a gap.
    list_depth: usize,
    /// Bullet owed to the first line of an `<li>`; later lines hang-indent.
    marker: Option<&'static str>,
}

impl Layouter<'_> {
    fn walk(&mut self, id: NodeId, style: Style, pre: bool) {
        // Copied out so the borrow lives as long as the arena, not as long as
        // `&self` — the walk mutates `self` while reading node data.
        let dom = self.dom;
        match &dom.node(id).data {
            NodeData::Text(text) => {
                if pre {
                    self.pre_text(text, style)
                } else {
                    self.flow_text(text, style)
                }
            }
            NodeData::Element { tag, .. } => self.element(id, tag, style, pre),
            // Comments, doctypes and the document node never render.
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => {}
        }
    }

    fn element(&mut self, id: NodeId, tag: &str, style: Style, pre: bool) {
        if SKIP.contains(&tag) {
            return;
        }
        match tag {
            "br" => return self.hard_line_break(),
            "hr" => return self.rule(),
            _ => {}
        }

        let style = tag_style(tag, style);
        let pre = pre || tag == "pre";
        let list = tag == "ul" || tag == "ol";
        // A nested list takes no gap of its own — it is part of the item above
        // it, the way a browser zeroes the margin on a nested list.
        let block = BLOCK.contains(&tag) && !(list && self.list_depth > 0);
        let boxed = block || TIGHT.contains(&tag) || list;

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
            self.walk(child, style, pre);
        }

        if tag == "li" {
            // An empty item must not lend its bullet to whatever comes next.
            self.marker = None;
        }
        self.list_depth -= usize::from(list);
        self.indent -= indent;

        if block {
            self.block_gap();
        } else if boxed {
            self.flush(false);
        }
    }

    /// End the current line and owe a blank one before the next content.
    fn block_gap(&mut self) {
        self.flush(false);
        // Only between siblings: nothing precedes the first line, and the gap
        // owed after the last block is never claimed.
        if !self.out.is_empty() {
            self.pending_blank = true;
        }
    }

    /// `<br>`: ends the line even when the line is empty, so `a<br><br>b` keeps
    /// the blank between the two breaks. This is why the blank-line collapsing
    /// in `block_gap` is a property of block gaps and not a rule against two
    /// empty lines in a row.
    fn hard_line_break(&mut self) {
        self.flush(true);
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
    /// spaces at a line box's edges are dropped rather than painted.
    fn flow_text(&mut self, text: &str, style: Style) {
        if text.starts_with(is_html_space) {
            self.pending_space = Some(style);
        }
        let mut first = true;
        for word in text.split(is_html_space).filter(|w| !w.is_empty()) {
            if !first {
                self.pending_space = Some(style);
            }
            first = false;
            self.word(word, style);
        }
        // Owed to the next text node: `a <b>b</b>` is "a b" across the tags.
        if text.ends_with(is_html_space) {
            self.pending_space = Some(style);
        }
    }

    /// Place one word, wrapping to the next line if it no longer fits. The word
    /// is measured once here and the measurement is reused for the fit test and
    /// the push (PLAN.md §4: no re-measuring per wrap attempt).
    fn word(&mut self, word: &str, style: Style) {
        let cells = word.width();
        let sep = usize::from(self.pending_space.is_some());
        if self.line_has_text && self.cur_cells + sep + cells > self.width {
            // Wrapping consumes the space; the next line starts flush left.
            self.flush(false);
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
        self.pending_space = None;
        if self.cur_cells + cells <= self.width {
            self.push_text(word, cells, style);
            self.line_has_text = true;
        } else {
            self.break_word(word, style);
        }
    }

    /// A word wider than the line breaks at a cell boundary — danluu.com is full
    /// of unbreakable URLs. Never splits a double-width character, and always
    /// places at least one character per line: that guarantee is what makes the
    /// loop finite when the indent has already eaten the whole width (a 1-cell
    /// terminal is reachable, and a hang is a bug, not an error path).
    fn break_word(&mut self, word: &str, style: Style) {
        let mut rest = word;
        while !rest.is_empty() {
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
            self.push_text(&rest[..end], cells, style);
            self.line_has_text = true;
            rest = &rest[end..];
            if !rest.is_empty() {
                self.flush(false);
            }
        }
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
    /// to end — what `<br>` needs and what a block boundary must not do.
    fn flush(&mut self, force: bool) {
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

/// Lines as text, with styles as markers: `[b]bold[/]`, `[u c12]link[/]`,
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
                // Default, and Rgb, which nothing emits in M3.
                _ => String::new(),
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

    /// Lay `html` out at `width` and render it with style markers.
    fn lines(html: &str, width: u16) -> String {
        debug_lines(&layout(&parse(html), width))
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
            "see [u c12]docs[/]\n"
        );
    }

    #[test]
    fn a_space_inside_a_link_carries_the_link_style() {
        // The underline runs unbroken through the link — one span, so the
        // painter writes the whole link in one call — while the space after it
        // is neutral and merges with the text that follows.
        let out = layout(&parse("<p><a href=x>two words</a> after</p>"), 40);
        assert_eq!(debug_lines(&out), "[u c12]two words[/] after\n");
        assert_eq!(out[0].spans.len(), 2);
    }

    #[test]
    fn styles_nest() {
        assert_eq!(lines("<b><a href=x>x</a></b>", 20), "[bu c12]x[/]\n");
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
            "[u c12]kensai[/] [u c12]3 hours ago[/]\n"
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
            "• 07/26[u c12]Post[/]\n"
        );
    }

    #[test]
    fn comments_and_doctypes_render_nothing() {
        assert_eq!(lines("<!doctype html><!-- hi --><p>a</p>", 20), "a\n");
    }
}

/// Ladder smoke test: every committed fixture lays out at width 80 without
/// panicking, produces content, and honors the width and trailing-space
/// invariants the painter depends on. No ladder page contains a `<pre>`, so the
/// width bound applies to every line here.
#[cfg(test)]
mod ladder {
    use super::*;
    use crate::html::parse;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    fn check(html: &str, min_lines: usize) {
        let lines = layout(&parse(html), 80);
        assert!(lines.len() >= min_lines, "only {} lines", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let cells: usize = line.spans.iter().map(|s| s.text.width()).sum();
            assert!(cells <= 80, "line {i} is {cells} cells: {line:?}");
            if let Some(last) = line.spans.last() {
                assert!(
                    !last.text.ends_with(is_html_space),
                    "line {i} ends in whitespace: {line:?}"
                );
            }
        }
    }

    #[test]
    fn example_com() {
        check(fixture!("example.com.html"), 3);
    }

    #[test]
    fn motherfuckingwebsite_com() {
        check(fixture!("motherfuckingwebsite.com.html"), 20);
    }

    #[test]
    fn danluu_com() {
        check(fixture!("danluu.com.html"), 196);
    }

    #[test]
    fn news_ycombinator_com() {
        check(fixture!("news.ycombinator.com.html"), 30);
    }

    #[test]
    fn en_wikipedia_org() {
        check(fixture!("en.wikipedia.org.html"), 100);
    }
}
