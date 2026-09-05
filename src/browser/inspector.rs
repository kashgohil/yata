//! The inspector surfaces' tree-to-lines transforms: `F1` turns a parsed `Dom`
//! into one compact, indented line per node, `F2` turns the styled tree into
//! one line per element with its computed values, and `F3` lists layout boxes
//! with x,y,w,h. Pure text — scrolling and drawing are the `App`'s job,
//! through the same `Viewport` machinery the page uses.
//!
//! This is deliberately terser than `html::debug_tree` (which `--dump-dom`
//! prints): the inspector is read on a live terminal, so ids/classes are
//! summarized CSS-style and long text is truncated. Snippet caps are measured
//! in cells with `unicode-width` (CLAUDE.md), never chars or bytes.

use unicode_width::UnicodeWidthChar;

use crate::dom::{Dom, NodeData, NodeId};
use crate::layout::{BoxId, BoxKind, LayoutTree};
use crate::style::Styles;
use crate::style::values::{
    AlignContent, AlignItems, AlignSelf, BoxSizing, ColorValue, Display, Edges, Flex, FlexBasis,
    FlexDirection, FlexWrap, FontStyle, FontWeight, Gaps, JustifyContent, Length, Position,
    TextAlign,
};

/// Cell caps for the variable-length parts of a line. Text gets the most room
/// (it is the content); URLs and comments are context, not content.
const TEXT_CAP: usize = 60;
const ATTR_CAP: usize = 40;

/// Render the whole tree, one node per line, two spaces of indent per depth
/// (the same shape as `debug_tree`, so the two views read alike).
pub fn tree_lines(dom: &Dom) -> Vec<String> {
    let mut out = Vec::new();
    push_node(dom, dom.root, 0, &mut out);
    out
}

fn push_node(dom: &Dom, id: NodeId, depth: usize, out: &mut Vec<String>) {
    // Whitespace-only text is real DOM (M5.0 keeps it, because layout needs it
    // to put a space between inline items) and pure noise in a tree view: on
    // Wikipedia it is thousands of `#text " "` lines between the nodes anyone
    // opened F1 to find. Browsers' element inspectors hide them too.
    // `--dump-dom` still shows them — that one is the parser's own output, and
    // a debugging dump that hides nodes lies.
    if matches!(&dom.node(id).data, NodeData::Text(s) if s.trim().is_empty()) {
        return;
    }
    let mut line = "  ".repeat(depth);
    match &dom.node(id).data {
        NodeData::Document => line.push_str("#document"),
        NodeData::Doctype(s) => {
            line.push_str("<!doctype ");
            line.push_str(&clip(s, ATTR_CAP));
            line.push('>');
        }
        NodeData::Comment(s) => {
            line.push_str("<!-- ");
            line.push_str(&clip(s, ATTR_CAP));
            line.push_str(" -->");
        }
        NodeData::Text(s) => {
            line.push_str("#text \"");
            line.push_str(&clip(s, TEXT_CAP));
            line.push('"');
        }
        NodeData::Element { tag, attrs } => line.push_str(&element_summary(tag, attrs)),
    }
    out.push(line);
    for child in dom.children(id) {
        push_node(dom, child, depth + 1, out);
    }
}

/// The `F2` view: every element with what the cascade computed for it, indented
/// like the `F1` tree so the two read alike.
///
/// PLAN.md §3 describes `F2` as "computed styles for the node under the
/// cursor". There is no cursor until M6 lands hit-testing, so this is the whole
/// document for now and M6 narrows it — the surface, its plumbing and its
/// vocabulary are what this milestone owes.
///
/// Only `display` is always printed. Everything else appears when it differs
/// from the CSS initial value, because Wikipedia has 13 399 elements and a
/// column of identical `color:default · normal` would hide the handful of lines
/// worth reading.
pub fn style_lines(dom: &Dom, styles: &Styles) -> Vec<String> {
    let mut out = Vec::new();
    push_styled(dom, dom.root, 0, styles, &mut out);
    out
}

fn push_styled(dom: &Dom, id: NodeId, depth: usize, styles: &Styles, out: &mut Vec<String>) {
    let mut depth = depth;
    if let NodeData::Element { tag, attrs } = &dom.node(id).data {
        // Text nodes are skipped: they carry their parent's inherited values
        // (M4.2), so a line each would repeat the line above it.
        out.push(format!(
            "{}{} {}",
            "  ".repeat(depth),
            element_summary(tag, attrs),
            summarize(styles.get(id))
        ));
        depth += 1;
    }
    for child in dom.children(id) {
        push_styled(dom, child, depth, styles, out);
    }
}

/// Computed values as one compact clause list: `block · #5c5cff · bold
/// underline`. Initial values are left out (see `style_lines`).
fn summarize(computed: &crate::style::ComputedStyle) -> String {
    let mut parts = vec![match computed.display {
        Display::Block => "block".to_string(),
        Display::Inline => "inline".to_string(),
        Display::InlineBlock => "inline-block".to_string(),
        Display::None => "none".to_string(),
        // A flex container prints its axis with the display keyword — `flex
        // row`, and `flex row wrap` when it wraps — because "which way does
        // this stack, and does it wrap" is the first thing to know about a
        // flex box, and `flex-flow` is how CSS spells the pair. The rest of
        // the flex properties follow the usual "interesting only" rule below.
        // An `inline-flex` box says both halves: it flexes inside and sits on
        // a line outside, and which of the two surprised the reader is exactly
        // what they opened F2 to find out.
        Display::Flex | Display::InlineFlex => {
            let keyword = if computed.display == Display::InlineFlex {
                "inline-flex"
            } else {
                "flex"
            };
            let mut s = format!("{keyword} {}", computed.flex_direction.name());
            if computed.flex_wrap != FlexWrap::default() {
                s.push(' ');
                s.push_str(computed.flex_wrap.name());
            }
            s
        }
    }];
    if let ColorValue::Rgb(r, g, b) = computed.color {
        parts.push(format!("#{r:02x}{g:02x}{b:02x}"));
    }
    if let ColorValue::Rgb(r, g, b) = computed.background_color {
        parts.push(format!("bg #{r:02x}{g:02x}{b:02x}"));
    }
    let mut flags = Vec::new();
    if computed.font_weight == FontWeight::Bold {
        flags.push("bold");
    }
    if computed.font_style == FontStyle::Italic {
        flags.push("italic");
    }
    if computed.underline {
        flags.push("underline");
    }
    if !flags.is_empty() {
        parts.push(flags.join(" "));
    }
    match computed.text_align {
        TextAlign::Left => {}
        TextAlign::Center => parts.push("center".into()),
        TextAlign::Right => parts.push("right".into()),
    }
    // Box model (M5.1): only non-initial values, same "interesting only" rule.
    if let Some(s) = edges_summary("margin", &computed.margin) {
        parts.push(s);
    }
    if let Some(s) = edges_summary("padding", &computed.padding) {
        parts.push(s);
    }
    if let Some(s) = edges_summary("border", &computed.border) {
        parts.push(s);
    }
    if !matches!(computed.width, Length::Auto) {
        parts.push(format!("w {}", length_summary(computed.width)));
    }
    if !matches!(computed.min_width, Length::Auto) {
        parts.push(format!("min-w {}", length_summary(computed.min_width)));
    }
    if !matches!(computed.max_width, Length::Auto) {
        parts.push(format!("max-w {}", length_summary(computed.max_width)));
    }
    // Sizing (M9.2), same "interesting only" rule: `height: auto` and
    // `content-box` are the initial values and stay out of the way.
    if !matches!(computed.height, Length::Auto) {
        parts.push(format!("h {}", length_summary(computed.height)));
    }
    if !matches!(computed.min_height, Length::Auto) {
        parts.push(format!("min-h {}", length_summary(computed.min_height)));
    }
    if !matches!(computed.max_height, Length::Auto) {
        parts.push(format!("max-h {}", length_summary(computed.max_height)));
    }
    if computed.box_sizing == BoxSizing::BorderBox {
        parts.push("border-box".into());
    }
    if computed.position != Position::Static {
        parts.push(match computed.position {
            Position::Relative => "position relative".into(),
            Position::Absolute => "position absolute".into(),
            Position::Static => unreachable!("initial position is omitted"),
        });
    }
    for (name, inset) in [
        ("top", computed.top),
        ("right", computed.right),
        ("bottom", computed.bottom),
        ("left", computed.left),
    ] {
        if !inset.is_auto() {
            parts.push(format!("{name} {}", length_summary(inset)));
        }
    }
    // M9.3: the same clause F3 prints, for the same reason — a page whose
    // content disappeared is read from these two panes together.
    if let Some(o) = overflow_summary(computed) {
        parts.push(format!("overflow {o}"));
    }
    parts.extend(flex_summary(computed));
    parts.join(" · ")
}

/// The flex clauses (M9.5), in the order a page would write them: container
/// first, then what this box asks of the container above it. Same
/// "interesting only" rule as everything else — a document with no flex in it
/// gets not one extra character.
///
/// The direction and wrap of a box that is *not* a flex container still print
/// when a page set them. They do nothing there, and that is exactly why they
/// are worth seeing: `flex-direction: column` on a box whose `display` never
/// became `flex` is a page bug F2 should be able to show.
fn flex_summary(computed: &crate::style::ComputedStyle) -> Vec<String> {
    let mut parts = Vec::new();
    if !matches!(computed.display, Display::Flex | Display::InlineFlex) {
        // Named in full here, unlike the `flex row wrap` clause above: on a box
        // that is not a flex container a bare `column` would read like any
        // other one-word clause, and the point of printing it is that it is
        // out of place.
        if computed.flex_direction != FlexDirection::default() {
            parts.push(format!("flex-direction {}", computed.flex_direction.name()));
        }
        if computed.flex_wrap != FlexWrap::default() {
            parts.push(format!("flex-wrap {}", computed.flex_wrap.name()));
        }
    }
    if computed.justify_content != JustifyContent::default() {
        parts.push(format!("justify {}", computed.justify_content.name()));
    }
    if computed.align_items != AlignItems::default() {
        parts.push(format!("align-items {}", computed.align_items.name()));
    }
    if computed.align_content != AlignContent::default() {
        parts.push(format!("align-content {}", computed.align_content.name()));
    }
    if let Some(g) = gap_summary(&computed.gap) {
        parts.push(g);
    }
    if computed.align_self != AlignSelf::Auto {
        parts.push(format!("align-self {}", computed.align_self.name()));
    }
    // Compared against the initial values rather than against 0/1 literals, so
    // the two spellings of "initial" cannot drift apart.
    let initial = Flex::default();
    if computed.flex.grow != initial.grow {
        parts.push(format!("grow {}", computed.flex.grow));
    }
    if computed.flex.shrink != initial.shrink {
        parts.push(format!("shrink {}", computed.flex.shrink));
    }
    match computed.flex.basis {
        FlexBasis::Auto => {}
        FlexBasis::Content => parts.push("basis content".into()),
        FlexBasis::Size(len) => parts.push(format!("basis {}", length_summary(len))),
    }
    if computed.order != 0 {
        parts.push(format!("order {}", computed.order));
    }
    parts
}

/// `gap 1em`, or `gap 1em 2em` when the two axes differ — row first, like the
/// shorthand. `None` when both are the initial zero.
fn gap_summary(gap: &Gaps) -> Option<String> {
    if *gap == Gaps::default() {
        return None;
    }
    Some(if gap.row == gap.column {
        format!("gap {}", length_summary(gap.row))
    } else {
        format!(
            "gap {} {}",
            length_summary(gap.row),
            length_summary(gap.column)
        )
    })
}

fn length_summary(len: Length) -> String {
    match len {
        Length::Auto => "auto".into(),
        Length::Zero => "0".into(),
        Length::Px(n) => format!("{n}px"),
        Length::Em(n) => format!("{n}em"),
        Length::Percent(n) => format!("{n}%"),
    }
}

/// Compact edge list, or `None` when every side is initial (Auto/zero).
fn edges_summary(name: &str, edges: &Edges) -> Option<String> {
    let interesting = |l: Length| !matches!(l, Length::Auto | Length::Zero);
    if !interesting(edges.top)
        && !interesting(edges.right)
        && !interesting(edges.bottom)
        && !interesting(edges.left)
    {
        return None;
    }
    // Collapse like CSS shorthand when sides pair up.
    if edges.top == edges.right && edges.right == edges.bottom && edges.bottom == edges.left {
        return Some(format!("{name} {}", length_summary(edges.top)));
    }
    if edges.top == edges.bottom && edges.right == edges.left {
        return Some(format!(
            "{name} {} {}",
            length_summary(edges.top),
            length_summary(edges.right)
        ));
    }
    Some(format!(
        "{name} {} {} {} {}",
        length_summary(edges.top),
        length_summary(edges.right),
        length_summary(edges.bottom),
        length_summary(edges.left)
    ))
}

/// The `F3` view: every layout box with its content-box x, y, width, height,
/// indented like the tree. Line and anonymous boxes are labeled; element boxes
/// use the same CSS-flavored summary as F1/F2. Empty anonymous blocks (the
/// whitespace between block tags) are left out — see [`push_box`].
pub fn box_lines(dom: &Dom, tree: &LayoutTree) -> Vec<String> {
    let mut out = Vec::new();
    push_box(dom, tree, tree.root, 0, &mut out);
    out
}

fn push_box(dom: &Dom, tree: &LayoutTree, id: BoxId, depth: usize, out: &mut Vec<String>) {
    let b = tree.get(id);
    // An anonymous block with no children and no height is what the whitespace
    // between two block tags lays out to: it holds nothing and paints nothing,
    // and on a real page there is one of them per source newline. Showing them
    // buries the boxes someone opened F3 to find — the same reason F1 hides
    // whitespace-only text nodes. Anonymous boxes that *contain* something
    // stay: those are real structure.
    if b.kind == BoxKind::AnonymousBlock
        && b.children.is_empty()
        && b.dimensions.content.height == 0
    {
        return;
    }
    // Skip the synthetic document root — it is only a container.
    let skip_label = b.kind == BoxKind::Block && b.node == Some(dom.root);
    if !skip_label {
        let label = match b.kind {
            // A flex container says so, and says which way it flexes and
            // whether it wraps: F3's job is explaining a layout, and "these
            // boxes are side by side because their parent is a flex row" — or
            // "that one is on a second row because the row wraps" — is the
            // explanation. Same clause as F2's, for the same reason.
            BoxKind::Flex => {
                let name = match b.node {
                    Some(nid) => match &dom.node(nid).data {
                        NodeData::Element { tag, attrs } => element_summary(tag, attrs),
                        _ => "flex".into(),
                    },
                    None => "flex".into(),
                };
                let mut label = format!("{name} flex {}", b.computed.flex_direction.name());
                if b.computed.flex_wrap != FlexWrap::default() {
                    label.push(' ');
                    label.push_str(b.computed.flex_wrap.name());
                }
                label
            }
            BoxKind::Block | BoxKind::Inline => {
                if let Some(nid) = b.node {
                    match &dom.node(nid).data {
                        NodeData::Element { tag, attrs } => element_summary(tag, attrs),
                        NodeData::Text(t) => format!("#text \"{}\"", clip(t, 20)),
                        _ => format!("{:?}", b.kind),
                    }
                } else {
                    format!("{:?}", b.kind)
                }
            }
            BoxKind::Table | BoxKind::TableRow | BoxKind::TableCell => {
                let role = match b.kind {
                    BoxKind::Table => "table",
                    BoxKind::TableRow => "table-row",
                    BoxKind::TableCell => "table-cell",
                    _ => unreachable!("matched table role"),
                };
                let name = b.node.and_then(|nid| match &dom.node(nid).data {
                    NodeData::Element { tag, attrs } => Some(element_summary(tag, attrs)),
                    _ => None,
                });
                name.map_or_else(|| role.into(), |name| format!("{role} {name}"))
            }
            BoxKind::AnonymousBlock => "anonymous".into(),
            BoxKind::Line => "line".into(),
            BoxKind::Text => {
                let t = b.text.as_deref().unwrap_or("");
                format!("#text \"{}\"", clip(t, 20))
            }
            // A control names the element it came from, then what it is showing
            // — the two questions F3 gets asked about a field are "how wide did
            // the page make this" and "why is that text in it" (M11.8).
            BoxKind::Field(paint) => {
                let name = match b.node {
                    Some(nid) => match &dom.node(nid).data {
                        NodeData::Element { tag, attrs } => element_summary(tag, attrs),
                        _ => "field".into(),
                    },
                    None => "field".into(),
                };
                let shows = match paint.shows {
                    crate::layout::Shows::Value => "value",
                    crate::layout::Shows::Placeholder => "placeholder",
                    crate::layout::Shows::Label => "label",
                    crate::layout::Shows::Checkbox(true) => "checkbox checked",
                    crate::layout::Shows::Checkbox(false) => "checkbox unchecked",
                    crate::layout::Shows::Radio(true) => "radio checked",
                    crate::layout::Shows::Radio(false) => "radio unchecked",
                    crate::layout::Shows::Select { .. } => "select",
                    crate::layout::Shows::SelectList { multiple: true, .. } => "select multiple",
                    crate::layout::Shows::SelectList {
                        multiple: false, ..
                    } => "select listbox",
                };
                let disabled = if paint.disabled { " disabled" } else { "" };
                format!(
                    "{name} field {shows} \"{}\"{disabled}",
                    clip(b.text.as_deref().unwrap_or(""), 16)
                )
            }
            BoxKind::Image => {
                let alt = b.text.as_deref().unwrap_or("");
                let src = b.image_src.as_deref().unwrap_or("?");
                if alt.is_empty() {
                    format!("img {src}")
                } else {
                    format!("img \"{}\" {}", clip(alt, 16), src)
                }
            }
        };
        let d = b.dimensions.content;
        out.push(format!(
            "{}{}  x={} y={} w={} h={}{}",
            "  ".repeat(depth),
            label,
            d.x,
            d.y,
            d.width,
            d.height,
            overflow_summary(&b.computed).map_or(String::new(), |o| format!(" overflow={o}"))
        ));
    }
    let child_depth = if skip_label { depth } else { depth + 1 };
    for &child in &b.children {
        push_box(dom, tree, child, child_depth, out);
    }
}

/// The `overflow` value of a box that clips (M9.3) — `hidden`, or `x/y` when
/// the two axes differ — and `None` for the initial `visible`.
///
/// Content vanishing from the page is the one thing F3 must be able to
/// explain, and "which box swallowed it" is the question being asked. Boxes
/// that clip nothing print nothing, so no existing golden moves.
fn overflow_summary(computed: &crate::style::ComputedStyle) -> Option<String> {
    let (x, y) = (computed.overflow_x, computed.overflow_y);
    if !x.clips() && !y.clips() {
        return None;
    }
    Some(if x == y {
        x.name().to_string()
    } else {
        format!("{}/{}", x.name(), y.name())
    })
}

/// CSS-flavored element summary: `<a#nav.cls href="…">`. `id` and `class`
/// fold into the selector-like head; `href`/`src` (the attributes worth
/// reading in a tree) show truncated values; anything further is elided to a
/// single `…` so a soup of data-attributes can't drown the structure.
fn element_summary(tag: &str, attrs: &[(String, String)]) -> String {
    let mut s = String::from("<");
    s.push_str(tag);
    let mut elided = false;
    for (k, v) in attrs {
        match k.as_str() {
            "id" => {
                s.push('#');
                s.push_str(&clip(v, ATTR_CAP));
            }
            "class" => {
                for class in v.split_whitespace() {
                    s.push('.');
                    s.push_str(&clip(class, ATTR_CAP));
                }
            }
            "href" | "src" => {
                s.push(' ');
                s.push_str(k);
                s.push_str("=\"");
                s.push_str(&clip(v, ATTR_CAP));
                s.push('"');
            }
            _ => elided = true,
        }
    }
    if elided {
        s.push_str(" …");
    }
    s.push('>');
    s
}

/// Trim, collapse whitespace runs (raw text keeps its newlines and tabs; a
/// tree line must not), and truncate at `cap` cells, appending `…` when
/// anything was cut.
fn clip(s: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    let mut in_ws = false;
    for ch in s.trim().chars() {
        let ch = if ch.is_whitespace() {
            if in_ws {
                continue;
            }
            in_ws = true;
            ' '
        } else {
            in_ws = false;
            ch
        };
        let w = ch.width().unwrap_or(0);
        if width + w > cap {
            // The ellipsis lives *inside* the cap: drop kept chars until it
            // fits, so a clipped result is never wider than `cap` cells.
            while width + 1 > cap {
                let Some(dropped) = out.pop() else { break };
                width -= dropped.width().unwrap_or(0);
            }
            out.push('…');
            return out;
        }
        out.push(ch);
        width += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn lines_are_indented_one_node_each() {
        let dom = parse("<p>hi</p>");
        assert_eq!(
            tree_lines(&dom),
            [
                "#document",
                "  <html>",
                "    <head>",
                "    <body>",
                "      <p>",
                "        #text \"hi\"",
            ]
        );
    }

    #[test]
    fn whitespace_only_text_is_hidden_from_the_tree_view() {
        // The nodes exist (M5.0) — layout needs them — but a tree view full of
        // `#text " "` hides the structure it is there to show.
        let dom = parse("<ul>\n  <li>a\n  <li>b\n</ul>");
        let lines = tree_lines(&dom);
        assert!(
            !lines
                .iter()
                .any(|l| l.trim() == r#"#text """# || l.contains("#text \"\\n")),
            "whitespace nodes must not be listed: {lines:?}"
        );
        // ...and text with content still is, whitespace and all.
        assert!(
            lines.iter().any(|l| l.contains(r#"#text "a""#)),
            "{lines:?}"
        );
    }

    #[test]
    fn id_and_class_fold_into_a_selector_like_summary() {
        let dom = parse(r#"<div id="main" class="header wide"></div>"#);
        assert!(
            tree_lines(&dom).contains(&"      <div#main.header.wide>".to_string()),
            "got {:?}",
            tree_lines(&dom)
        );
    }

    #[test]
    fn href_shows_truncated_and_other_attrs_elide() {
        let long = format!("https://example.com/{}", "x".repeat(80));
        let dom = parse(&format!(r#"<a href="{long}" data-x="1">go</a>"#));
        let lines = tree_lines(&dom);
        let a = lines.iter().find(|l| l.contains("<a ")).unwrap();
        assert!(a.contains(r#"href="https://example.com/"#), "line: {a}");
        assert!(a.contains('…'), "long href must truncate: {a}");
        assert!(!a.contains("data-x"), "other attrs elide: {a}");
        assert!(a.trim_start().ends_with(" …>"), "elision marker: {a}");
    }

    #[test]
    fn text_snippets_collapse_whitespace_and_truncate_by_cells() {
        let dom = parse("<p>a\n\t b</p>");
        assert!(
            tree_lines(&dom).contains(&"        #text \"a b\"".to_string()),
            "got {:?}",
            tree_lines(&dom)
        );

        // 40 wide chars are 80 cells: the cap must bite by cells, not chars,
        // and the ellipsis must fit inside the cap, not ride past it.
        let dom = parse(&format!("<p>{}</p>", "世".repeat(40)));
        let lines = tree_lines(&dom);
        let text = lines.iter().find(|l| l.contains("#text")).unwrap();
        assert!(text.ends_with("…\""), "wide text must truncate: {text}");
        let snippet: String = text
            .chars()
            .skip_while(|&c| c != '"')
            .filter(|&c| c != '"')
            .collect();
        assert!(
            UnicodeWidthStr::width(snippet.as_str()) <= TEXT_CAP,
            "clipped snippet is {} cells, over the {TEXT_CAP}-cell cap: {text}",
            UnicodeWidthStr::width(snippet.as_str())
        );
        // 29 whole wide chars (58 cells) + the 1-cell ellipsis ≤ 60.
        assert_eq!(snippet.chars().filter(|&c| c == '世').count(), 29);
    }

    // ---- F2: computed styles (M4.5) ---------------------------------------

    fn styled(html: &str, css: &str) -> Vec<String> {
        let dom = parse(html);
        let sheet = crate::css::parse(css);
        let styles = crate::style::style_tree(&dom, &[&sheet]);
        style_lines(&dom, &styles)
    }

    #[test]
    fn one_line_per_element_indented_like_the_tree() {
        // Text nodes get no line: they carry their parent's inherited values,
        // so a line each would just repeat the line above.
        // `p` carries the UA vertical margin (M5.1); that is real computed
        // style, so F2 shows it under the same "only if interesting" rule.
        assert_eq!(
            styled("<p>hi</p>", ""),
            [
                "<html> block",
                "  <head> none",
                "  <body> block",
                "    <p> block · margin 1em 0",
            ]
        );
    }

    #[test]
    fn display_always_shows_and_initial_values_stay_quiet() {
        // A plain inline element says `inline` and nothing else — 13 399
        // elements of `color:default · normal` would bury the lines worth
        // reading.
        let lines = styled("<span>x</span>", "");
        let span = lines.iter().find(|l| l.contains("<span>")).unwrap();
        assert_eq!(span.trim(), "<span> inline");
    }

    #[test]
    fn computed_values_appear_as_they_differ_from_the_initial() {
        let lines = styled(
            "<a href='/x'>link</a>",
            "a { background-color: #eee; text-align: center }",
        );
        let a = lines.iter().find(|l| l.contains("<a ")).unwrap();
        // UA link colour and underline, plus what the page added.
        assert!(a.contains("inline"), "{a}");
        assert!(a.contains("#5c5cff"), "{a}");
        assert!(a.contains("bg #eeeeee"), "{a}");
        assert!(a.contains("underline"), "{a}");
        assert!(a.contains("center"), "{a}");

        let lines = styled("<h1>t</h1>", "h1 { font-style: italic }");
        let h1 = lines.iter().find(|l| l.contains("<h1>")).unwrap();
        assert!(h1.contains("block"), "{h1}");
        assert!(h1.contains("bold italic"), "{h1}");
    }

    #[test]
    fn hidden_elements_are_listed_as_hidden_not_omitted() {
        // The inspector's job is to explain the page, and "why is this not on
        // screen" is the question it most needs to answer.
        let lines = styled("<p class=ad>gone</p>", ".ad { display: none }");
        let p = lines.iter().find(|l| l.contains("<p.ad>")).unwrap();
        assert!(p.contains("none"), "{p}");
        // <script> and <head> too, from the UA sheet.
        assert!(lines.iter().any(|l| l.trim() == "<head> none"));
    }

    #[test]
    fn box_model_values_show_when_set() {
        let lines = styled(
            "<div>x</div>",
            "div { margin: 1em; width: 50%; max-width: 40em; padding-left: 8px }",
        );
        let div = lines.iter().find(|l| l.contains("<div>")).unwrap();
        assert!(div.contains("margin 1em"), "{div}");
        assert!(div.contains("padding"), "{div}");
        assert!(div.contains("w 50%"), "{div}");
        assert!(div.contains("max-w 40em"), "{div}");
    }

    #[test]
    fn sizing_values_show_when_set() {
        let lines = styled(
            "<div>x</div><p>y</p>",
            "div { height: 3em; min-width: 10em; min-height: 2em; max-height: 30em;
                   box-sizing: border-box }",
        );
        let div = lines.iter().find(|l| l.contains("<div>")).unwrap();
        assert!(div.contains("h 3em"), "{div}");
        assert!(div.contains("min-w 10em"), "{div}");
        assert!(div.contains("min-h 2em"), "{div}");
        assert!(div.contains("max-h 30em"), "{div}");
        assert!(div.contains("border-box"), "{div}");
        // The initial values stay out of the way: an untouched element shows
        // none of this (F2 would be unreadable on Wikipedia otherwise).
        let p = lines.iter().find(|l| l.contains("<p>")).unwrap();
        assert!(!p.contains("h "), "{p}");
        assert!(!p.contains("border-box"), "{p}");
    }

    #[test]
    fn positioned_values_show_when_set() {
        let lines = styled(
            "<div>x</div>",
            "div { position: absolute; top: 1em; right: 25%; left: 0 }",
        );
        let div = lines.iter().find(|l| l.contains("<div>")).unwrap();
        assert!(div.contains("position absolute"), "{div}");
        assert!(div.contains("top 1em"), "{div}");
        assert!(div.contains("right 25%"), "{div}");
        assert!(div.contains("left 0"), "{div}");
    }

    #[test]
    fn flex_values_show_when_set() {
        let lines = styled(
            "<div><p>x</p></div><span>y</span>",
            "div { display: flex; gap: 1em; justify-content: space-between;
                   align-items: center }
             p { flex: 1; order: -1; align-self: flex-end }",
        );
        // The axis rides with the display keyword: a flex container's first
        // fact is which way it stacks.
        let div = lines.iter().find(|l| l.contains("<div>")).unwrap();
        assert!(div.contains("flex row"), "{div}");
        assert!(div.contains("gap 1em"), "{div}");
        assert!(div.contains("justify space-between"), "{div}");
        assert!(div.contains("align-items center"), "{div}");

        // `flex: 1` is 1/1/0, so all three parts show.
        let p = lines.iter().find(|l| l.contains("<p>")).unwrap();
        assert!(p.contains("grow 1"), "{p}");
        assert!(p.contains("basis 0"), "{p}");
        assert!(p.contains("order -1"), "{p}");
        assert!(p.contains("align-self flex-end"), "{p}");
        assert!(!p.contains("shrink"), "shrink 1 is initial: {p}");

        // Nothing flex-related was said about the span, so nothing is shown —
        // the same reason F2 stays readable on Wikipedia.
        let span = lines.iter().find(|l| l.contains("<span>")).unwrap();
        assert_eq!(span.trim(), "<span> inline");

        // Wrap joins the axis, the way `flex-flow` writes the pair.
        let lines = styled(
            "<div>x</div>",
            "div { display: flex; flex-direction: column; flex-wrap: wrap; gap: 1em 2em }",
        );
        let div = lines.iter().find(|l| l.contains("<div>")).unwrap();
        assert!(div.contains("flex column wrap"), "{div}");
        assert!(div.contains("gap 1em 2em"), "{div}");

        // Flex properties on a box that never became a flex container still
        // show: that combination is a page bug, and F2 exists to explain it.
        let lines = styled("<div>x</div>", "div { flex-direction: column }");
        let div = lines.iter().find(|l| l.contains("<div>")).unwrap();
        assert_eq!(div.trim(), "<div> block · flex-direction column");
    }

    #[test]
    fn comments_and_doctype_are_marked() {
        let dom = parse("<!doctype html><!-- note --><p>x</p>");
        let lines = tree_lines(&dom);
        assert!(lines.iter().any(|l| l.contains("<!doctype html>")));
        assert!(lines.iter().any(|l| l.contains("<!-- note -->")));
    }

    // ---- F3: layout boxes (M5) --------------------------------------------

    #[test]
    fn box_lines_show_geometry() {
        let dom = parse("<p>hi</p>");
        let styles = crate::style::style_tree(&dom, &[]);
        let tree =
            crate::layout::layout_document(&dom, &styles, 40, crate::layout::Hidden::Respect);
        let lines = box_lines(&dom, &tree);
        assert!(
            lines.iter().any(|l| l.contains("<p>") && l.contains("w=")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("#text") && l.contains("hi")),
            "{lines:?}"
        );
    }

    #[test]
    fn empty_anonymous_boxes_are_hidden_but_filled_ones_stay() {
        // The newlines between these block tags each lay out to an empty
        // anonymous block. They are noise; the anonymous block wrapping the
        // paragraph's text is structure.
        let dom = parse("<div>a</div>\n<div>b</div>\n");
        let styles = crate::style::style_tree(&dom, &[]);
        let tree =
            crate::layout::layout_document(&dom, &styles, 40, crate::layout::Hidden::Respect);
        let lines = box_lines(&dom, &tree);
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("anonymous") && l.contains("h=0")),
            "empty anonymous boxes leaked into F3: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("anonymous")),
            "the anonymous block holding inline content must stay: {lines:?}"
        );
    }
}
