//! Form controls: what an element is as a control, how big its box is, and
//! what that box draws (PLAN.md M11, task M11.8).
//!
//! **A field's value is state, not an attribute.** `value="x"` is the *default*
//! — what a reset would restore — while what the field currently holds lives
//! beside the tree in [`Dom::field_value`](crate::dom::Dom::field_value), and
//! only ever differs once a reader has typed (HTML's dirty value flag). Storing
//! the current value back on the element would make `input[value="x"]` match
//! what someone typed (M11.2 made attribute selectors real), and would put a
//! subtree restyle (M11.3) on every keystroke. Layout reads the state and falls
//! back to the attribute; M11.8 only reads, M11.9 is the first writer.
//!
//! **A character is a cell.** `size`, `cols` and `rows` are counts of
//! *characters*, which a pixel browser has to turn into a width by multiplying
//! by some average glyph and guessing. Here the guess does not exist:
//! `size="17"` is 17 cells, exactly, and HN's search box comes out the width HN
//! asked for. That is the one place this engine is *more* correct than a
//! browser rather than less, and it is worth saying out loud.
//!
//! **One function decides what a control looks like.** [`runs`] is read by
//! paint, by `--dump-text` and by the focus overlay, so the three cannot
//! disagree about where a field's cells are or what is in them.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::dom::{Dom, NodeData, NodeId};
use crate::layout::boxes::{BoxKind, LayoutBox};
use crate::term::{Attrs, Color, Style};

/// HTML's own defaults for a control the page did not size (§4.10).
const DEFAULT_SIZE: i32 = 20;
const DEFAULT_COLS: i32 = 20;
const DEFAULT_ROWS: i32 = 2;

/// The most characters an attribute may ask for on either axis.
///
/// Not tidiness: `size` and `rows` are page-controlled numbers that decide how
/// many cells [`runs`] builds a string for, so an uncapped `size="99999999"`
/// would be a page buying a hundred megabytes of paint with one attribute. No
/// terminal is a thousand columns wide, so the cap costs nothing real.
const MAX_CHARS: i32 = 1000;

/// A password's mask, one per source character.
///
/// ASCII, and that is the whole of the reasoning: `•` is East Asian Ambiguous,
/// so a CJK-configured terminal draws it two cells wide and the mask becomes a
/// lie about the value's length — in exactly the environment where a reader is
/// least able to check. `unicode-width` says `*` is one cell everywhere.
const MASK: char = '*';

/// The two cells of a control's horizontal padding that a terminal draws
/// instead of a border, and what a reader learns from them.
///
/// A browser frames a field with a border *outside* its content, and so does
/// this: the UA sheet gives every control one cell of horizontal padding and
/// these glyphs go in it, which is why `size="17"` still buys 17 cells of value
/// and not 15. Glyphs rather than attributes because a monochrome terminal is a
/// real terminal (PLAN.md's renderer supports one) and an underline alone
/// cannot tell a full field from a link — both are underlined runs. Brackets
/// can be read with no colour at all.
///
/// Parentheses for a `disabled` control: it is still a control, and it is still
/// not for you. A page that zeroes the padding gets a flush field with no
/// frame, exactly as it gets a browser field with no border.
const FRAME: (char, char) = ('[', ']');
const FRAME_DISABLED: (char, char) = ('(', ')');

/// What a control's box is showing. The box carries the finished text — masked,
/// defaulted and all — so nothing downstream has to re-derive it; this says
/// what that text *means*.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shows {
    /// The field's value (already masked, if this is a `password`).
    Value,
    /// The value is empty and the page offered a `placeholder`.
    Placeholder,
    /// A button's label. A button takes no caret: there is nothing to type
    /// into it.
    Label,
}

/// How a form control's box is painted (M11.8) — the payload of
/// [`BoxKind::Field`](crate::layout::BoxKind::Field).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldPaint {
    pub shows: Shows,
    /// `disabled`: the Tab cycle skips it and the frame says why.
    pub disabled: bool,
}

/// A control with a box: everything layout needs to build one, derived once.
pub(crate) struct Control {
    /// What the box shows, finished — the value, the mask, the placeholder or
    /// the label. A `password`'s real value never leaves this module.
    pub text: String,
    pub shows: Shows,
    /// The content box, in characters (= cells).
    pub cols: i32,
    pub rows: i32,
    pub disabled: bool,
}

/// What kind of control an element is, before its value is read.
enum Kind {
    /// A text field. `masked` is `type="password"`.
    Text { masked: bool },
    /// `<button>` and the three button `type`s: the same box with a label in
    /// it and no caret.
    Button,
    /// Recognized, and deliberately drawn as nothing: `hidden`, plus every type
    /// whose real rendering is a control M11.12 owns.
    Absent,
}

/// Is this tag one of the elements that can be a form control at all?
///
/// Cheap enough to ask on the inline path, which is why the decision is split
/// this way: everything else about a control costs a `Dom` lookup.
pub(crate) fn is_control_tag(tag: &str) -> bool {
    detect() && matches!(tag, "input" | "textarea" | "button")
}

/// Does this element generate no box at all — `<input type=hidden>`, and the
/// control types this engine recognizes but has not implemented?
///
/// A deliberate departure from HTML, which falls back to a text field for a
/// type it does not know: Wikipedia drives eight CSS-only dropdowns from
/// `<input type="checkbox">`, and rendering those as eight empty text boxes
/// would put stray fields in the article chrome to nobody's benefit. They draw
/// nothing today and they draw nothing until M11.12 gives them a real box.
pub(crate) fn generates_no_box(dom: &Dom, node: NodeId, tag: &str) -> bool {
    matches!(kind(dom, node, tag), Some(Kind::Absent))
}

/// The control this element is, with everything its box needs — or `None` when
/// it is not a control, or is one this engine draws as nothing.
pub(crate) fn control(dom: &Dom, node: NodeId, tag: &str) -> Option<Control> {
    // The tag first, and nothing before it. Layout asks this of *every* block
    // element it builds, so anything ahead of the `matches!` in `kind` — an
    // attribute lookup, say — is work the whole page pays to be told that a
    // `<div>` is not a form control.
    let kind = kind(dom, node, tag)?;
    let disabled = dom.attr(node, "disabled").is_some();
    match kind {
        Kind::Absent => None,
        Kind::Button => {
            let text = label(dom, node, tag);
            Some(Control {
                cols: (text.width() as i32).min(MAX_CHARS),
                rows: 1,
                text,
                shows: Shows::Label,
                disabled,
            })
        }
        Kind::Text { masked } => {
            let value = value(dom, node, tag);
            let (text, shows) = match (value.is_empty(), dom.attr(node, "placeholder")) {
                (true, Some(hint)) if !hint.is_empty() => (one_line(hint), Shows::Placeholder),
                (_, _) if masked => (mask(&value), Shows::Value),
                _ => (value, Shows::Value),
            };
            let (cols, rows) = if tag == "textarea" {
                (
                    characters(dom, node, "cols", DEFAULT_COLS),
                    characters(dom, node, "rows", DEFAULT_ROWS),
                )
            } else {
                (characters(dom, node, "size", DEFAULT_SIZE), 1)
            };
            Some(Control {
                text,
                shows,
                cols,
                rows,
                disabled,
            })
        }
    }
}

fn kind(dom: &Dom, node: NodeId, tag: &str) -> Option<Kind> {
    if !is_control_tag(tag) {
        return None;
    }
    match tag {
        "textarea" => Some(Kind::Text { masked: false }),
        "button" => Some(Kind::Button),
        _ => {
            let ty = dom.attr(node, "type").unwrap_or("text").trim();
            let is = |names: &[&str]| names.iter().any(|n| ty.eq_ignore_ascii_case(n));
            Some(if is(&["password"]) {
                Kind::Text { masked: true }
            } else if is(&["submit", "button", "reset"]) {
                Kind::Button
            } else if is(&[
                "hidden",
                "checkbox",
                "radio",
                "file",
                "range",
                "color",
                "image",
                "date",
                "datetime-local",
                "month",
                "week",
                "time",
            ]) {
                Kind::Absent
            } else {
                // `text`, `search`, `email`, `url`, `tel`, `number` — and a
                // type nobody has heard of, because HTML says an unrecognized
                // one is a text field and a page that invents `type="wibble"`
                // means a text box.
                Kind::Text { masked: false }
            })
        }
    }
}

/// What the control currently holds: the state beside the tree if a reader has
/// touched it, else what the markup said.
fn value(dom: &Dom, node: NodeId, tag: &str) -> String {
    if let Some(typed) = dom.field_value(node) {
        return typed.to_string();
    }
    if tag == "textarea" {
        // A `<textarea>`'s default value is its text content — RCDATA the
        // tokenizer already decoded — with one newline stripped from the front
        // (HTML §4.10.11), and every other space kept: this is a value, not
        // prose, so nothing collapses it.
        let mut out = String::new();
        push_text(dom, node, &mut out);
        return match out.strip_prefix('\n') {
            Some(rest) => rest.to_string(),
            None => out,
        };
    }
    one_line(dom.attr(node, "value").unwrap_or_default())
}

/// A button's label: a `<button>`'s text content, or an `<input>`'s `value`
/// with HTML's default for the type when the page gave none.
fn label(dom: &Dom, node: NodeId, tag: &str) -> String {
    if tag == "button" {
        let mut out = String::new();
        push_text(dom, node, &mut out);
        // A label is prose, so its whitespace collapses the way prose does.
        return out.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    if let Some(value) = dom.attr(node, "value").filter(|v| !v.is_empty()) {
        return one_line(value);
    }
    let ty = dom.attr(node, "type").unwrap_or("submit").trim();
    if ty.eq_ignore_ascii_case("reset") {
        "Reset".into()
    } else if ty.eq_ignore_ascii_case("button") {
        // A browser really does show an empty button here.
        String::new()
    } else {
        "Submit".into()
    }
}

fn push_text(dom: &Dom, node: NodeId, out: &mut String) {
    for child in dom.children(node) {
        match &dom.node(child).data {
            NodeData::Text(text) => out.push_str(text),
            NodeData::Element { .. } => push_text(dom, child, out),
            _ => {}
        }
    }
}

/// One `*` per source character. Per *character*, not per cell: a mask that
/// shrank a wide glyph to one cell would understate how much was typed.
fn mask(value: &str) -> String {
    std::iter::repeat_n(MASK, value.chars().count()).collect()
}

/// A single-line control shows one line: HTML strips newlines from an
/// `<input>`'s value, and a placeholder that broke a line would break the box.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// A `size`/`cols`/`rows` attribute as a character count. HTML: a value that is
/// not a positive integer is not a value, and the default stands.
fn characters(dom: &Dom, node: NodeId, name: &str, default: i32) -> i32 {
    dom.attr(node, name)
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
        .min(MAX_CHARS)
}

/// One run of cells a control's box draws, in document coordinates.
pub(crate) struct Run {
    pub x: i32,
    pub y: i32,
    pub text: String,
    pub style: Style,
}

/// Everything a control's box puts on screen: its frame, and its text clipped
/// to the cells the page asked for, one row at a time.
///
/// The single source of what a field looks like. Paint emits these as display
/// commands, `--dump-text` rasterises them into rows, and the focus overlay
/// reverses the frame cells they name — three surfaces that must not disagree
/// about where a field is or what is in it.
///
/// The value is **clipped, never wrapped**, and measured with `unicode-width`:
/// a field is exactly where a CJK value would find a `chars().count()`. An
/// over-long value shows its start, which is what a browser shows before the
/// field is focused; the horizontal scrolling that follows a caret is M11.9's.
pub(crate) fn runs(b: &LayoutBox) -> Vec<Run> {
    let BoxKind::Field(paint) = b.kind else {
        return Vec::new();
    };
    let rect = b.dimensions.content;
    let (open, close) = if paint.disabled {
        FRAME_DISABLED
    } else {
        FRAME
    };
    let frame = frame_style(b, paint);
    let interior = interior_style(b, paint);
    let mut out = Vec::new();
    let mut lines = b.text.as_deref().unwrap_or("").split('\n');
    for row in 0..rect.height {
        let y = rect.y + row;
        // The frame sits in the innermost cell of the control's own padding,
        // so it is there exactly when the page left room for it.
        if b.dimensions.padding.left > 0 {
            out.push(Run {
                x: rect.x - 1,
                y,
                text: open.to_string(),
                style: frame,
            });
        }
        // A control the page squeezed to nothing (`width: 0`) still has its
        // frame and no interior — an empty run would be a draw command that
        // draws nothing.
        let line = fit(lines.next().unwrap_or(""), rect.width);
        if !line.is_empty() {
            out.push(Run {
                x: rect.x,
                y,
                text: line,
                style: interior,
            });
        }
        if b.dimensions.padding.right > 0 {
            out.push(Run {
                x: rect.right(),
                y,
                text: close.to_string(),
                style: frame,
            });
        }
    }
    out
}

/// Where the caret sits in this control, in document coordinates — `None` for
/// a button and for a control the reader cannot reach.
///
/// At the **end of the value**, because with no editing there is nowhere else
/// it could be (M11.9 owns everything that moves it). An empty field with a
/// placeholder puts it at the front, where the first character would go.
pub(crate) fn caret(b: &LayoutBox) -> Option<(i32, i32)> {
    let BoxKind::Field(paint) = b.kind else {
        return None;
    };
    if paint.disabled || paint.shows == Shows::Label {
        return None;
    }
    let rect = b.dimensions.content;
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    let (row, col) = match paint.shows {
        Shows::Placeholder => (0, 0),
        _ => {
            let lines: Vec<&str> = b.text.as_deref().unwrap_or("").split('\n').collect();
            let row = (lines.len() as i32 - 1).clamp(0, rect.height - 1);
            (row, lines[row as usize].width() as i32)
        }
    };
    Some((rect.x + col.min(rect.width - 1), rect.y + row))
}

/// The style of the control's own cells: whatever the cascade gave the element,
/// plus what makes it read as a control.
///
/// A text field is **underlined across its whole box**, blanks included, so an
/// empty one still reads as somewhere to type rather than as a gap. A button is
/// not: its brackets say what it is, and underlining a label would make it look
/// like a link. A placeholder is dimmed and italic — two signals, one of which
/// survives a terminal with no colour — because it is the page talking, not the
/// reader's own text.
fn interior_style(b: &LayoutBox, paint: FieldPaint) -> Style {
    let mut style = b.term_style;
    if paint.shows != Shows::Label {
        style.attrs = style.attrs | Attrs::UNDERLINE;
    }
    if paint.shows == Shows::Placeholder {
        style.attrs = style.attrs | Attrs::ITALIC;
        style.fg = Color::Ansi(8);
    }
    if paint.disabled {
        style.fg = Color::Ansi(8);
    }
    style
}

fn frame_style(b: &LayoutBox, paint: FieldPaint) -> Style {
    Style {
        fg: if paint.disabled {
            Color::Ansi(8)
        } else {
            b.term_style.fg
        },
        bg: b.term_style.bg,
        attrs: Attrs::NONE,
    }
}

/// `text` cut to `cells` columns and padded back out to exactly that many.
///
/// Cut by width rather than by characters, and a wide glyph that would straddle
/// the last cell is dropped rather than halved — the padding then fills the cell
/// it left, so every row of a field is exactly as wide as the field.
fn fit(text: &str, cells: i32) -> String {
    if cells <= 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0i32;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0) as i32;
        if used + w > cells {
            break;
        }
        out.push(ch);
        used += w;
    }
    for _ in used..cells {
        out.push(' ');
    }
    out
}

/// Whether control detection runs at all.
///
/// `true` in every build a reader ever sees. Under `cfg(test)` it can be turned
/// off so that [`measure_the_field_work`] can time the pre-M11.8 layout path and
/// this one **in the same process, interleaved** — this machine drifts 5–10%
/// between runs, so a before-commit/after-commit pair is not evidence
/// (CLAUDE.md). Nothing but that measurement touches it.
///
/// [`measure_the_field_work`]: crate::layout
#[cfg(not(test))]
const fn detect() -> bool {
    true
}

#[cfg(test)]
fn detect() -> bool {
    DETECT.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static DETECT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Run `f` with control detection off — the A side of the measurement.
#[cfg(test)]
pub(crate) fn without_detection<T>(f: impl FnOnce() -> T) -> T {
    DETECT.with(|d| d.set(false));
    let out = f();
    DETECT.with(|d| d.set(true));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn node(dom: &Dom, tag: &str) -> NodeId {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&id| matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag))
            .expect("fixture element")
    }

    fn control_of(html_src: &str, tag: &str) -> Option<Control> {
        let dom = html::parse(html_src);
        let id = node(&dom, tag);
        control(&dom, id, tag)
    }

    #[test]
    fn a_text_field_is_size_characters_wide_and_one_row_tall() {
        let c = control_of(r#"<input type=text size=17 value=typed>"#, "input").unwrap();
        assert_eq!((c.cols, c.rows), (17, 1));
        assert_eq!(c.text, "typed");
        assert_eq!(c.shows, Shows::Value);
    }

    #[test]
    fn html_defaults_stand_in_for_a_missing_or_nonsense_attribute() {
        for src in [
            "<input>",
            "<input size=0>",
            "<input size=abc>",
            "<input size=-4>",
        ] {
            assert_eq!(
                control_of(src, "input").unwrap().cols,
                DEFAULT_SIZE,
                "{src}"
            );
        }
        let area = control_of("<textarea></textarea>", "textarea").unwrap();
        assert_eq!((area.cols, area.rows), (DEFAULT_COLS, DEFAULT_ROWS));
    }

    #[test]
    fn a_page_cannot_ask_for_more_cells_than_the_cap() {
        let c = control_of(
            "<textarea cols=99999999 rows=99999999></textarea>",
            "textarea",
        )
        .unwrap();
        assert_eq!((c.cols, c.rows), (MAX_CHARS, MAX_CHARS));
    }

    #[test]
    fn an_unknown_type_is_a_text_field_and_a_recognized_one_is_nothing() {
        let dom = html::parse(
            "<input type=wibble><input type=hidden><input type=checkbox><input type=search>",
        );
        let ids: Vec<NodeId> = (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(
                |&id| matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "input"),
            )
            .collect();
        let drawn: Vec<bool> = ids
            .iter()
            .map(|&id| control(&dom, id, "input").is_some())
            .collect();
        assert_eq!(drawn, [true, false, false, true]);
        assert!(
            ids.iter()
                .skip(1)
                .take(2)
                .all(|&id| generates_no_box(&dom, id, "input"))
        );
    }

    #[test]
    fn a_password_shows_one_mask_cell_per_source_character() {
        let c = control_of("<input type=password value='pä漢'>", "input").unwrap();
        assert_eq!(c.text, "***");
        assert_eq!(c.shows, Shows::Value);
    }

    #[test]
    fn a_placeholder_shows_only_while_the_value_is_empty() {
        let c = control_of("<input placeholder='Search Wikipedia'>", "input").unwrap();
        assert_eq!(
            (c.text.as_str(), c.shows),
            ("Search Wikipedia", Shows::Placeholder)
        );
        let c = control_of("<input placeholder=hint value=typed>", "input").unwrap();
        assert_eq!((c.text.as_str(), c.shows), ("typed", Shows::Value));
    }

    #[test]
    fn a_textarea_keeps_its_whitespace_and_loses_one_leading_newline() {
        let c = control_of("<textarea>\nhello  there\nand more</textarea>", "textarea").unwrap();
        assert_eq!(c.text, "hello  there\nand more");
    }

    #[test]
    fn a_buttons_label_comes_from_its_content_or_the_type_default() {
        assert_eq!(
            control_of("<button> Search  now </button>", "button")
                .unwrap()
                .text,
            "Search now"
        );
        assert_eq!(
            control_of("<input type=submit>", "input").unwrap().text,
            "Submit"
        );
        assert_eq!(
            control_of("<input type=reset>", "input").unwrap().text,
            "Reset"
        );
        assert_eq!(
            control_of("<input type=submit value=Go>", "input")
                .unwrap()
                .text,
            "Go"
        );
        let c = control_of("<button>Search</button>", "button").unwrap();
        assert_eq!((c.cols, c.rows, c.shows), (6, 1, Shows::Label));
    }

    #[test]
    fn the_typed_value_wins_over_the_attribute() {
        // The rule M11.9 depends on: what a reader typed is state beside the
        // tree, and the attribute is only the default it started from.
        let mut dom = html::parse("<input value=default>");
        let id = node(&dom, "input");
        assert_eq!(control(&dom, id, "input").unwrap().text, "default");
        dom.set_field_value(id, "typed");
        assert_eq!(control(&dom, id, "input").unwrap().text, "typed");
        assert_eq!(
            dom.attr(id, "value"),
            Some("default"),
            "typing wrote through to the attribute"
        );
    }

    #[test]
    fn fit_clips_by_cells_and_pads_back_to_the_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 3), "abc");
        // 漢 is two cells: three of them do not fit in five, and the cell the
        // dropped glyph would have half-filled is padded instead.
        assert_eq!(fit("漢漢漢", 5), "漢漢 ");
        assert_eq!(fit("x", 0), "");
    }
}
