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
    Checkbox(bool),
    Radio(bool),
    Select {
        selected: Option<usize>,
    },
    SelectList {
        multiple: bool,
        first_selected: Option<usize>,
    },
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
    Text {
        masked: bool,
    },
    /// `<button>` and the three button `type`s: the same box with a label in
    /// it and no caret.
    Button,
    Checkbox,
    Radio,
    Select,
    /// Recognized, and deliberately drawn as nothing: `hidden` and specialized
    /// controls whose rendering remains out of scope.
    Absent,
}

/// Is this tag one of the elements that can be a form control at all?
///
/// Cheap enough to ask on the inline path, which is why the decision is split
/// this way: everything else about a control costs a `Dom` lookup.
pub(crate) fn is_control_tag(tag: &str) -> bool {
    detect()
        && (matches!(tag, "input" | "textarea" | "button") || tag == "select" && detect_choices())
}

/// Does this element generate no box at all — `<input type=hidden>`, and the
/// control types this engine recognizes but has not implemented?
///
/// Unknown types do not reach this answer; HTML falls them back to text.
pub(crate) fn generates_no_box(dom: &Dom, node: NodeId, tag: &str) -> bool {
    matches!(kind(dom, node, tag), Some(Kind::Absent))
}

/// Whether this control owns the sparse current-value state. Unlike
/// [`editable_value`], this deliberately includes readonly and disabled text
/// controls: those limit reader input, not a script's `.value` setter.
pub(crate) fn has_live_value(dom: &Dom, node: NodeId, tag: &str) -> bool {
    matches!(kind(dom, node, tag), Some(Kind::Text { .. }))
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
        Kind::Checkbox | Kind::Radio => {
            let checked = checked(dom, node, tag);
            let (text, shows) = match kind {
                Kind::Checkbox => (
                    if checked { "x" } else { " " }.to_string(),
                    Shows::Checkbox(checked),
                ),
                Kind::Radio => (
                    if checked { "*" } else { "o" }.to_string(),
                    Shows::Radio(checked),
                ),
                _ => unreachable!(),
            };
            Some(Control {
                text,
                shows,
                cols: 1,
                rows: 1,
                disabled,
            })
        }
        Kind::Select => {
            let options = options(dom, node);
            let multiple =
                dom.attr(node, "multiple").is_some() || characters(dom, node, "size", 1) > 1;
            let selected = selected_options(dom, node, &options);
            let widest = options
                .iter()
                .map(|option| option.label.width())
                .max()
                .unwrap_or(0);
            let selected_index = selected
                .first()
                .and_then(|id| options.iter().position(|option| option.node == *id));
            let (text, shows, cols, rows) = if multiple {
                let text = options
                    .iter()
                    .map(|option| {
                        let mark = if selected.contains(&option.node) {
                            'x'
                        } else {
                            ' '
                        };
                        format!("[{mark}] {}", option.label)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let rows = characters(
                    dom,
                    node,
                    "size",
                    if dom.attr(node, "multiple").is_some() {
                        4
                    } else {
                        1
                    },
                );
                (
                    text,
                    Shows::SelectList {
                        multiple: dom.attr(node, "multiple").is_some(),
                        first_selected: selected_index,
                    },
                    widest.saturating_add(4).min(MAX_CHARS as usize) as i32,
                    rows,
                )
            } else {
                (
                    options
                        .iter()
                        .map(|option| option.label.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Shows::Select {
                        selected: selected_index,
                    },
                    widest.saturating_add(2).min(MAX_CHARS as usize) as i32,
                    1,
                )
            };
            Some(Control {
                text,
                shows,
                cols,
                rows,
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
        "select" => Some(Kind::Select),
        _ => {
            let ty = dom.attr(node, "type").unwrap_or("text").trim();
            let is = |names: &[&str]| names.iter().any(|n| ty.eq_ignore_ascii_case(n));
            Some(if is(&["password"]) {
                Kind::Text { masked: true }
            } else if is(&["submit", "button", "reset"]) {
                Kind::Button
            } else if is(&["checkbox"]) {
                if detect_choices() {
                    Kind::Checkbox
                } else {
                    Kind::Absent
                }
            } else if is(&["radio"]) {
                if detect_choices() {
                    Kind::Radio
                } else {
                    Kind::Absent
                }
            } else if is(&[
                "hidden",
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

#[derive(Clone, Debug)]
pub(crate) struct OptionItem {
    pub node: NodeId,
    pub label: String,
    pub value: String,
    pub disabled: bool,
}

pub(crate) fn is_checkbox(dom: &Dom, node: NodeId, tag: &str) -> bool {
    matches!(kind(dom, node, tag), Some(Kind::Checkbox))
}

pub(crate) fn is_radio(dom: &Dom, node: NodeId, tag: &str) -> bool {
    matches!(kind(dom, node, tag), Some(Kind::Radio))
}

pub(crate) fn is_select(tag: &str) -> bool {
    tag.eq_ignore_ascii_case("select")
}

fn raw_choice(dom: &Dom, node: NodeId, attr: &str) -> bool {
    dom.choice_state(node)
        .unwrap_or_else(|| dom.attr(node, attr).is_some())
}

pub(crate) fn checked(dom: &Dom, node: NodeId, tag: &str) -> bool {
    if is_checkbox(dom, node, tag) {
        return raw_choice(dom, node, "checked");
    }
    if !is_radio(dom, node, tag) || !raw_choice(dom, node, "checked") {
        return false;
    }
    if dom.attr(node, "name").is_none_or(str::is_empty) {
        return true;
    }
    radio_group(dom, node)
        .into_iter()
        .rev()
        .find(|&candidate| raw_choice(dom, candidate, "checked"))
        == Some(node)
}

fn form_owner(dom: &Dom, node: NodeId) -> Option<NodeId> {
    let mut current = Some(node);
    while let Some(id) = current {
        if let NodeData::Element { tag, .. } = &dom.node(id).data
            && tag.eq_ignore_ascii_case("form")
        {
            return Some(id);
        }
        if id == dom.root {
            return Some(id);
        }
        current = dom.node(id).parent;
    }
    None
}

pub(crate) fn radio_group(dom: &Dom, node: NodeId) -> Vec<NodeId> {
    if !dom.is_connected(node) {
        return Vec::new();
    }
    let Some(name) = dom.attr(node, "name").filter(|name| !name.is_empty()) else {
        return vec![node];
    };
    let owner = form_owner(dom, node);
    fn collect(dom: &Dom, id: NodeId, name: &str, owner: Option<NodeId>, out: &mut Vec<NodeId>) {
        if let NodeData::Element { tag, .. } = &dom.node(id).data
            && is_radio(dom, id, tag)
            && dom.attr(id, "name") == Some(name)
            && form_owner(dom, id) == owner
        {
            out.push(id);
        }
        for child in dom.children(id) {
            collect(dom, child, name, owner, out);
        }
    }
    let mut out = Vec::new();
    collect(dom, dom.root, name, owner, &mut out);
    out
}

pub(crate) fn options(dom: &Dom, select: NodeId) -> Vec<OptionItem> {
    fn collect(dom: &Dom, node: NodeId, inherited_disabled: bool, out: &mut Vec<OptionItem>) {
        for child in dom.children(node) {
            let NodeData::Element { tag, .. } = &dom.node(child).data else {
                continue;
            };
            if tag.eq_ignore_ascii_case("option") {
                let mut text = String::new();
                push_text(dom, child, &mut text);
                let text = collapse_ascii_whitespace(&text);
                out.push(OptionItem {
                    node: child,
                    label: dom.attr(child, "label").unwrap_or(&text).to_string(),
                    value: dom.attr(child, "value").unwrap_or(&text).to_string(),
                    disabled: inherited_disabled || dom.attr(child, "disabled").is_some(),
                });
            } else {
                collect(
                    dom,
                    child,
                    inherited_disabled
                        || tag.eq_ignore_ascii_case("optgroup")
                            && dom.attr(child, "disabled").is_some(),
                    out,
                );
            }
        }
    }
    let mut out = Vec::new();
    collect(dom, select, false, &mut out);
    out
}

fn collapse_ascii_whitespace(text: &str) -> String {
    text.split([' ', '\t', '\n', '\r', '\x0c'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn selected_options(dom: &Dom, select: NodeId, options: &[OptionItem]) -> Vec<NodeId> {
    if dom.attr(select, "multiple").is_some() {
        return options
            .iter()
            .filter(|option| raw_choice(dom, option.node, "selected"))
            .map(|option| option.node)
            .collect();
    }
    options
        .iter()
        .rev()
        .find(|option| raw_choice(dom, option.node, "selected"))
        // A sparse override is HTML's dirty selectedness flag. Once one
        // exists, an all-false single select is a real script-created state,
        // not a cue to restore the clean first-enabled fallback.
        .or_else(|| {
            (!options
                .iter()
                .any(|option| dom.choice_state(option.node).is_some()))
            .then(|| options.iter().find(|option| !option.disabled))
            .flatten()
        })
        .map(|option| vec![option.node])
        .unwrap_or_default()
}

/// What a reader may type into here, and what they would be typing into —
/// `None` for everything that takes no caret: a button, a `disabled` control,
/// a `readonly` one, a type this engine draws as nothing, an element that is
/// not a control.
///
/// `readonly` is here and not in M11.8's focus rule because that is where HTML
/// puts it: a `readonly` field is still reachable and still selectable — it is
/// showing you something — and the only thing it refuses is an edit. That
/// makes it exactly this function's question, unlike `disabled`, which answers
/// two (M11.8 keeps it out of the Tab cycle as well).
///
/// The *unmasked* value, because this is the string an edit is applied to. A
/// password's caret indexes it and not the mask, and the two agree because
/// [`mask`] is one cell per source character; nothing else ever needs the real
/// text, so nothing else is given it.
pub(crate) fn editable_value(dom: &Dom, node: NodeId, tag: &str) -> Option<String> {
    match kind(dom, node, tag)? {
        Kind::Text { .. }
            if dom.attr(node, "disabled").is_none() && dom.attr(node, "readonly").is_none() =>
        {
            Some(value(dom, node, tag))
        }
        _ => None,
    }
}

/// What the control currently holds: the state beside the tree if a reader has
/// touched it, else what the markup said.
///
/// The single answer to that question, which is why M11.10's serializer reaches
/// through here rather than asking `Dom::field_value` itself: **what is sent is
/// exactly what is drawn**, and two derivations of "the value" would be two
/// chances for a form to submit something the reader could not see.
pub(crate) fn value(dom: &Dom, node: NodeId, tag: &str) -> String {
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

/// Everything a control's box puts on screen: its frame, its text, and where
/// its caret sits — one answer, because the caret is a cell of the same window
/// the text is drawn from and deriving the two separately is how they drift
/// apart by a glyph.
pub(crate) struct Painted {
    pub runs: Vec<Run>,
    /// Document coordinates, or `None` for a control that takes no caret: a
    /// button (nothing to type into) and a `disabled` field.
    pub caret: Option<(i32, i32)>,
    /// Content row occupied by a select cursor, when cursor-aware paint was
    /// requested.
    pub cursor_row: Option<i32>,
}

/// The cells of a control the page's own paint draws: the **start** of the
/// value, which is what a browser shows before a field is focused.
///
/// The single source of what a field looks like. Paint emits these as display
/// commands, `--dump-text` rasterises them into rows, and the focus overlay
/// draws the same runs windowed on the caret — surfaces that must not disagree
/// about where a field is or what is in it.
pub(crate) fn runs(b: &LayoutBox) -> Vec<Run> {
    painted_inner(b, None, None).runs
}

/// The same cells, windowed on a caret `chars` characters into the value
/// (M11.9) — what the reader typing into this control sees.
///
/// **The window is UI, not layout.** A control's box is `size` cells wide
/// whatever it holds, so scrolling the value under the caret moves no geometry
/// and belongs nowhere near the tree: the display list keeps the unfocused
/// view, and the focus overlay draws this one over exactly the same cells. A
/// caret in the tree would make layout depend on where a cursor is.
///
/// Everything here is measured with `unicode-width` and never with
/// `chars().count()`: a field is exactly where a CJK value finds that bug, and
/// a caret counted in characters sits half a glyph off in the one language the
/// person who wrote it does not read.
pub(crate) fn painted(b: &LayoutBox, caret: Option<usize>) -> Painted {
    painted_inner(b, caret, None)
}

/// Cursor-aware select paint over the same fixed layout rectangle.
pub(crate) fn painted_select(b: &LayoutBox, cursor: usize) -> Painted {
    painted_inner(b, None, Some(cursor))
}

fn painted_inner(b: &LayoutBox, caret: Option<usize>, select_cursor: Option<usize>) -> Painted {
    let BoxKind::Field(paint) = b.kind else {
        return Painted {
            runs: Vec::new(),
            caret: None,
            cursor_row: None,
        };
    };
    let rect = b.dimensions.content;
    let text = b.text.as_deref().unwrap_or("");
    let lines: Vec<&str> = text.split('\n').collect();
    let (mut first_row, x_off, caret_cell) = window_of(b, paint, &lines, caret);
    let collapsed = match paint.shows {
        Shows::Select { selected } => {
            let at = select_cursor.or(selected);
            Some(format!(
                "{} v",
                at.and_then(|i| lines.get(i)).copied().unwrap_or("")
            ))
        }
        Shows::SelectList { first_selected, .. } => {
            if let Some(cursor) = select_cursor {
                let base = first_selected.unwrap_or(0) as i32;
                let cursor = cursor as i32;
                first_row = if cursor < base {
                    cursor
                } else if cursor >= base + rect.height {
                    cursor - rect.height + 1
                } else {
                    base
                };
            }
            None
        }
        _ => None,
    };
    let cursor_row = select_cursor.map(|cursor| match paint.shows {
        Shows::Select { .. } => 0,
        Shows::SelectList { .. } => cursor as i32 - first_row,
        _ => 0,
    });
    let (open, close) = if paint.disabled {
        FRAME_DISABLED
    } else {
        FRAME
    };
    let frame = frame_style(b, paint);
    let interior = interior_style(b, paint);
    let mut out = Vec::new();
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
        let line = collapsed
            .as_deref()
            .filter(|_| row == 0)
            .or_else(|| {
                if collapsed.is_some() {
                    None
                } else {
                    lines.get((first_row + row) as usize).copied()
                }
            })
            .unwrap_or_default();
        let line = window(line, x_off, rect.width);
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
    Painted {
        runs: out,
        caret: caret_cell,
        cursor_row,
    }
}

/// Just the caret cell, for the overlay that draws a focused-but-not-typed-into
/// control: nothing about its text has moved, so building its runs a second
/// time — every frame, including every scroll step — would be a
/// `<textarea rows=1000>`'s worth of strings for a rectangle already on screen.
pub(crate) fn caret(b: &LayoutBox) -> Option<(i32, i32)> {
    let BoxKind::Field(paint) = b.kind else {
        return None;
    };
    let text = b.text.as_deref().unwrap_or("");
    let lines: Vec<&str> = text.split('\n').collect();
    window_of(b, paint, &lines, None).2
}

/// Which lines and which columns the box shows, and where that puts the caret.
///
/// A caret drags the window after it — past the right edge the value scrolls,
/// and `Home` snaps it back — while a control nobody is typing into always
/// shows its start, which is what a browser shows before a field is focused.
fn window_of(
    b: &LayoutBox,
    paint: FieldPaint,
    lines: &[&str],
    caret: Option<usize>,
) -> (i32, i32, Option<(i32, i32)>) {
    let rect = b.dimensions.content;
    let at = caret_in_value(paint, lines, caret);
    let (first_row, x_off) = match (at, caret) {
        (Some((line, col)), Some(_)) => (
            (line as i32 - rect.height + 1).max(0),
            (col - rect.width + 1).max(0),
        ),
        _ => match paint.shows {
            Shows::SelectList { first_selected, .. } => (first_selected.unwrap_or(0) as i32, 0),
            _ => (0, 0),
        },
    };
    let cell = at
        .filter(|_| rect.width > 0 && rect.height > 0)
        .map(|(line, col)| {
            (
                rect.x + (col - x_off).clamp(0, rect.width - 1),
                rect.y + (line as i32 - first_row).clamp(0, rect.height - 1),
            )
        });
    (first_row, x_off, cell)
}

/// Where the caret is in the value: which line, and how many **cells** into it.
///
/// `None` for a control that takes no caret. `caret` is a character index into
/// the value (M11.9's editing state); without one the caret is at the end of
/// the value, because a control nobody is typing into has nowhere else to put
/// it (M11.8). A placeholder pins it to the front — the value behind the hint
/// is empty, so that is where the first character would go.
fn caret_in_value(paint: FieldPaint, lines: &[&str], caret: Option<usize>) -> Option<(usize, i32)> {
    if paint.disabled || !matches!(paint.shows, Shows::Value | Shows::Placeholder) {
        return None;
    }
    if paint.shows == Shows::Placeholder {
        return Some((0, 0));
    }
    let last = lines.len().saturating_sub(1);
    let Some(mut left) = caret else {
        return Some((last, lines[last].width() as i32));
    };
    for (i, line) in lines.iter().enumerate() {
        let chars = line.chars().count();
        if left <= chars || i == last {
            let upto: String = line.chars().take(left.min(chars)).collect();
            return Some((i, upto.width() as i32));
        }
        // The newline the split consumed is a character the caret can be past.
        left -= chars + 1;
    }
    Some((last, lines[last].width() as i32))
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
    if matches!(paint.shows, Shows::Value | Shows::Placeholder) {
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

/// The `cells` columns of `text` starting `x_off` columns in, padded back out
/// to exactly that many.
///
/// Cut by width rather than by characters at **both** ends, and a wide glyph
/// that would straddle either edge is blanked rather than halved: the cells it
/// still occupies become spaces, so every row of a field is exactly as wide as
/// the field and no half of a 漢 is ever drawn.
fn window(text: &str, x_off: i32, cells: i32) -> String {
    if cells <= 0 {
        return String::new();
    }
    let mut out = String::new();
    // `used` counts the window's filled cells; `x` the line's own columns.
    let (mut used, mut x) = (0i32, 0i32);
    for ch in text.chars() {
        if used >= cells {
            break;
        }
        let w = ch.width().unwrap_or(0) as i32;
        let start = x;
        x += w;
        if x <= x_off {
            continue;
        }
        if start < x_off {
            for _ in 0..(x - x_off).min(cells - used) {
                out.push(' ');
                used += 1;
            }
            continue;
        }
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

#[cfg(not(test))]
const fn detect_choices() -> bool {
    true
}

#[cfg(test)]
fn detect_choices() -> bool {
    DETECT_CHOICES.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static DETECT_CHOICES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Run `f` with only M11.12's checkbox, radio, and select recognition off.
#[cfg(test)]
pub(crate) fn without_choice_detection<T>(f: impl FnOnce() -> T) -> T {
    DETECT_CHOICES.with(|d| d.set(false));
    let out = f();
    DETECT_CHOICES.with(|d| d.set(true));
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
    fn an_unknown_type_is_text_hidden_is_absent_and_checkbox_is_a_choice() {
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
        assert_eq!(drawn, [true, false, true, true]);
        assert!(generates_no_box(&dom, ids[1], "input"));
        assert!(!generates_no_box(&dom, ids[2], "input"));
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
    fn a_window_clips_by_cells_and_pads_back_to_the_width() {
        assert_eq!(window("abc", 0, 5), "abc  ");
        assert_eq!(window("abcdef", 0, 3), "abc");
        // 漢 is two cells: three of them do not fit in five, and the cell the
        // dropped glyph would have half-filled is padded instead.
        assert_eq!(window("漢漢漢", 0, 5), "漢漢 ");
        assert_eq!(window("x", 0, 0), "");
    }

    #[test]
    fn a_window_scrolled_past_half_a_glyph_shows_a_blank_not_half_of_it() {
        // M11.9: the value scrolls under the caret in *cells*. Starting one
        // cell into a 漢 cannot draw half of it, so that cell is blank and the
        // next glyph starts where it really starts.
        assert_eq!(window("漢字abc", 0, 4), "漢字");
        assert_eq!(window("漢字abc", 1, 4), " 字a");
        assert_eq!(window("漢字abc", 2, 4), "字ab");
        assert_eq!(window("漢字abc", 4, 4), "abc ");
        // Past the end of the line: all padding, still exactly as wide.
        assert_eq!(window("ab", 6, 3), "   ");
    }

    /// A control's box, hand-built, so the window logic can be asked about a
    /// caret without going through a whole layout.
    fn field_box(text: &str, cols: i32, rows: i32, shows: Shows) -> LayoutBox {
        let dimensions = crate::layout::Dimensions {
            content: crate::layout::Rect {
                x: 10,
                y: 5,
                width: cols,
                height: rows,
            },
            ..Default::default()
        };
        LayoutBox {
            kind: BoxKind::Field(FieldPaint {
                shows,
                disabled: false,
            }),
            node: None,
            dimensions,
            children: Vec::new(),
            text: Some(text.to_string()),
            term_style: Style::default(),
            computed: crate::style::ComputedStyle::default(),
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        }
    }

    fn shown(b: &LayoutBox, caret: Option<usize>) -> (Vec<String>, Option<(i32, i32)>) {
        let p = painted(b, caret);
        let rows = p
            .runs
            .iter()
            .filter(|r| r.x == b.dimensions.content.x)
            .map(|r| r.text.clone())
            .collect();
        (rows, p.caret)
    }

    #[test]
    fn without_a_caret_a_field_shows_the_start_of_its_value() {
        // M11.8's view, unchanged: paint, `--dump-text` and an unfocused field
        // all show the value's start, and the caret sits at its end.
        let b = field_box("abcdefghij", 6, 1, Shows::Value);
        assert_eq!(shown(&b, None), (vec!["abcdef".into()], Some((15, 5))));
    }

    #[test]
    fn typing_past_the_right_edge_scrolls_the_value_under_the_caret() {
        let b = field_box("abcdefghij", 6, 1, Shows::Value);
        // Caret inside the first window: nothing scrolls.
        assert_eq!(shown(&b, Some(3)), (vec!["abcdef".into()], Some((13, 5))));
        // At the end of a value wider than the box: the last 5 cells and the
        // caret in the 6th, which is where the next character goes.
        assert_eq!(shown(&b, Some(10)), (vec!["fghij ".into()], Some((15, 5))));
        // `Home` snaps back to the start.
        assert_eq!(shown(&b, Some(0)), (vec!["abcdef".into()], Some((10, 5))));
    }

    #[test]
    fn a_cjk_value_scrolls_by_cells_and_the_caret_lands_on_a_whole_glyph() {
        // Six characters, twelve cells, in an eight-cell box. A caret counted
        // in `chars()` would put every one of these numbers somewhere else.
        let b = field_box("漢字漢字漢字", 8, 1, Shows::Value);
        assert_eq!(shown(&b, Some(0)), (vec!["漢字漢字".into()], Some((10, 5))));
        // Four characters in is eight cells: one past the window, so it
        // scrolls by exactly the one cell that puts the caret at the edge.
        assert_eq!(shown(&b, Some(4)), (vec![" 字漢字 ".into()], Some((17, 5))));
        // The end of the value: twelve cells, so the window starts at cell 5,
        // where the third 漢 is already half over — blanked, never halved, and
        // the row is still exactly eight cells wide.
        assert_eq!(shown(&b, Some(6)), (vec![" 字漢字 ".into()], Some((17, 5))));
    }

    #[test]
    fn a_textarea_scrolls_to_the_line_the_caret_is_on() {
        // Three lines in a two-row box. The caret is a character index into
        // the whole value, newlines included.
        let b = field_box("one\ntwo\nthree", 5, 2, Shows::Value);
        assert_eq!(
            shown(&b, Some(1)),
            (vec!["one  ".into(), "two  ".into()], Some((11, 5)))
        );
        // Two characters into the third line (4 + 4 + 2): the window drops the
        // first line so the caret's own line is on screen.
        assert_eq!(
            shown(&b, Some(10)),
            (vec!["two  ".into(), "three".into()], Some((12, 6)))
        );
    }

    #[test]
    fn a_placeholder_keeps_the_caret_at_the_front_and_a_button_has_none() {
        let hint = field_box("Search Wikipedia", 8, 1, Shows::Placeholder);
        assert_eq!(shown(&hint, Some(0)).1, Some((10, 5)));
        let button = field_box("Search", 6, 1, Shows::Label);
        assert_eq!(shown(&button, None).1, None);
    }

    #[test]
    fn an_editable_value_is_the_unmasked_one_and_only_for_what_takes_a_caret() {
        let dom = html::parse(
            "<input value=plain><input type=password value=secret>\
             <input value=off disabled><input value=shown readonly>\
             <button>Search</button><input type=hidden value=h>",
        );
        let mut got = Vec::new();
        for id in (0..dom.node_count() as u32).map(NodeId) {
            if let NodeData::Element { tag, .. } = &dom.node(id).data {
                got.push(editable_value(&dom, id, tag));
            }
        }
        assert_eq!(
            got.into_iter().flatten().collect::<Vec<_>>(),
            // The password's *real* value: an edit applies to the text, not to
            // the stars, and the caret indexes it. The `readonly` field is
            // absent — it has a box and a focus and no caret.
            ["plain".to_string(), "secret".to_string()]
        );
    }

    #[test]
    fn radio_checkedness_is_group_normalized_and_live_state_is_not_an_attribute() {
        let mut dom = html::parse(
            "<form><input id=a type=radio name=x checked>\
             <input id=b type=radio name=x checked></form>",
        );
        let ids = (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(|id| dom.attr(*id, "id").is_some())
            .collect::<Vec<_>>();
        assert!(!checked(&dom, ids[0], "input"));
        assert!(checked(&dom, ids[1], "input"));
        dom.set_choice_state(ids[0], "checked", true);
        dom.set_choice_state(ids[1], "checked", false);
        assert!(checked(&dom, ids[0], "input"));
        assert!(!checked(&dom, ids[1], "input"));
        assert!(dom.attr(ids[1], "checked").is_some());
    }

    #[test]
    fn radio_normalization_follows_the_connected_tree_not_arena_order() {
        let mut dom = html::parse(
            "<form><input id=a type=radio name=x checked>\
             <input id=b type=radio name=x checked></form>",
        );
        let form = node(&dom, "form");
        let group = radio_group(
            &dom,
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&id| dom.attr(id, "id") == Some("a"))
                .unwrap(),
        );
        let (first, second) = (group[0], group[1]);
        dom.append(form, first).unwrap();
        assert!(checked(&dom, first, "input"));
        assert!(!checked(&dom, second, "input"));

        let detached = dom.create_element(
            "input",
            vec![
                ("type".into(), "radio".into()),
                ("name".into(), "x".into()),
                ("checked".into(), String::new()),
            ],
        );
        assert!(!radio_group(&dom, first).contains(&detached));
        assert!(checked(&dom, first, "input"));
    }

    #[test]
    fn radios_outside_forms_group_by_name_while_unnamed_radios_stand_alone() {
        let dom = html::parse(
            "<input id=a type=radio name=x checked><input id=b type=radio name=x checked>\
             <input id=c type=radio checked><input id=d type=radio checked>",
        );
        let id = |wanted| {
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&node| dom.attr(node, "id") == Some(wanted))
                .unwrap()
        };
        assert!(!checked(&dom, id("a"), "input"));
        assert!(checked(&dom, id("b"), "input"));
        assert!(checked(&dom, id("c"), "input"));
        assert!(checked(&dom, id("d"), "input"));
    }

    #[test]
    fn a_select_uses_option_labels_and_normalizes_one_or_many_selections() {
        let dom = html::parse(
            "<select><option selected value=one>First</option>\
             <option selected label=Shown value=two>Second</option></select>\
             <select multiple><option selected>A</option><option selected>B</option></select>",
        );
        let selects = (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(|id| matches!(&dom.node(*id).data, NodeData::Element { tag, .. } if tag == "select"))
            .collect::<Vec<_>>();
        let first = options(&dom, selects[0]);
        assert_eq!(selected_options(&dom, selects[0], &first), [first[1].node]);
        assert_eq!(first[1].label, "Shown");
        assert_eq!(first[1].value, "two");
        let control = control(&dom, selects[0], "select").unwrap();
        assert_eq!(control.shows, Shows::Select { selected: Some(1) });
        let many = options(&dom, selects[1]);
        assert_eq!(selected_options(&dom, selects[1], &many).len(), 2);
    }

    #[test]
    fn select_size_and_unicode_width_follow_cell_defaults_and_bounds() {
        for src in [
            "<select><option>A</option></select>",
            "<select size=0><option>A</option></select>",
            "<select size=-1><option>A</option></select>",
            "<select size=abc><option>A</option></select>",
        ] {
            let c = control_of(src, "select").unwrap();
            assert_eq!((c.cols, c.rows), (3, 1), "{src}");
            assert_eq!(c.shows, Shows::Select { selected: Some(0) }, "{src}");
        }
        let list = control_of("<select size=2><option>A</option></select>", "select").unwrap();
        assert_eq!((list.cols, list.rows), (5, 2));
        assert_eq!(
            list.shows,
            Shows::SelectList {
                multiple: false,
                first_selected: Some(0)
            }
        );
        let many = control_of("<select multiple><option>A</option></select>", "select").unwrap();
        assert_eq!((many.cols, many.rows), (5, 4));
        let wide = control_of(
            "<select><option selected>漢字漢</option></select>",
            "select",
        )
        .unwrap();
        assert_eq!((wide.cols, wide.rows), (8, 1));
    }

    #[test]
    fn select_defaults_handle_disabled_placeholders_and_empty_multiple_choices() {
        let dom = html::parse(
            "<select id=f><option disabled>Skip</option><option id=enabled>Use</option></select>\
             <select id=p><option id=placeholder disabled selected>Choose</option>\
             <option>Use</option></select><select id=m multiple><option>None</option></select>",
        );
        let id = |wanted| {
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&node| dom.attr(node, "id") == Some(wanted))
                .unwrap()
        };
        let fallback = options(&dom, id("f"));
        assert_eq!(selected_options(&dom, id("f"), &fallback), [id("enabled")]);
        let placeholder = options(&dom, id("p"));
        assert_eq!(
            selected_options(&dom, id("p"), &placeholder),
            [id("placeholder")]
        );
        let multiple = options(&dom, id("m"));
        assert!(selected_options(&dom, id("m"), &multiple).is_empty());
    }

    #[test]
    fn select_paint_windows_from_selection_and_then_around_the_cursor() {
        let collapsed = field_box("zero\none\ntwo", 8, 1, Shows::Select { selected: Some(1) });
        assert_eq!(shown(&collapsed, None).0, ["one v   "]);
        assert_eq!(
            painted_select(&collapsed, 2)
                .runs
                .iter()
                .filter(|run| run.x == 10)
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["two v   "]
        );

        let list = field_box(
            "[ ] zero\n[ ] one\n[x] two\n[ ] three\n[ ] four",
            10,
            2,
            Shows::SelectList {
                multiple: true,
                first_selected: Some(2),
            },
        );
        assert_eq!(shown(&list, None).0, ["[x] two   ", "[ ] three "]);
        let cursor = painted_select(&list, 4);
        assert_eq!(cursor.cursor_row, Some(1));
        assert_eq!(
            cursor
                .runs
                .iter()
                .filter(|run| run.x == 10)
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["[ ] three ", "[ ] four  "]
        );

        let wide = field_box("漢字漢", 5, 1, Shows::Select { selected: Some(0) });
        assert_eq!(shown(&wide, None).0, ["漢字 "]);
        let sparse = field_box(
            "[x] one",
            7,
            3,
            Shows::SelectList {
                multiple: true,
                first_selected: Some(0),
            },
        );
        assert_eq!(shown(&sparse, None).0, ["[x] one", "       ", "       "]);
    }

    #[test]
    fn option_text_collapses_only_ascii_space_and_only_optgroup_disables_descendants() {
        let dom = html::parse(
            "<select><div disabled><option>A\t B&nbsp;C</option></div>\
             <optgroup disabled><option>D</option></optgroup></select>",
        );
        let items = options(&dom, node(&dom, "select"));
        assert_eq!(items[0].label, "A B\u{a0}C");
        assert!(!items[0].disabled);
        assert!(items[1].disabled);
    }
}
