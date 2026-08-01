//! Layout: DOM + styles + width → box tree (PLAN.md M5).
//!
//! Pure transform. The box tree is the stage's real output; `layout` /
//! `layout_readable` also rasterise it to `Vec<Line>` so the viewport and
//! `--dump-text` keep working until the display-list paint path lands.

mod boxes;
mod dimensions;
mod engine;
mod hit;
mod lines;

pub use boxes::{BoxId, BoxKind, LayoutBox, LayoutTree};
pub use dimensions::{Dimensions, EdgeSizes, Rect};
pub use engine::{Hidden, layout_tree, layout_tree_with, term_color, term_style};
pub use hit::{
    LinkHit, collect_links, dom_links, first_y, hit_test, is_under, link_at, nearest_link,
    visible_links,
};

use crate::dom::Dom;
use crate::image::ImageContext;
use crate::style::Styles;
use crate::term::{Attrs, Color, Style};

/// A run of text sharing one style. Never contains a newline.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One row of laid-out content. Empty lines are blank rows (margin gaps).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

/// Lay the document out, and if honouring `display:none` leaves nothing to
/// read, lay it out again revealing what the page hid.
pub fn layout_readable(dom: &Dom, styles: &Styles, width: u16) -> (Vec<Line>, bool) {
    layout_readable_with(dom, styles, width, &ImageContext::default())
}

/// Like [`layout_readable`] with image metrics (M8).
pub fn layout_readable_with(
    dom: &Dom,
    styles: &Styles,
    width: u16,
    images: &ImageContext,
) -> (Vec<Line>, bool) {
    let (tree, revealed) = layout_document_readable(dom, styles, width, images);
    (lines::from_tree(&tree), revealed)
}

/// The box tree behind [`layout_readable_with`]: the `Hidden::Respect` tree,
/// or the revealed one when respecting `display:none` leaves nothing to read.
/// The `bool` is "this page hid itself and we overrode it".
///
/// The tree, not the lines, is what `F3` and `--dump-boxes` show, so both go
/// through here rather than re-deciding which of the two trees is the page.
pub fn layout_document_readable(
    dom: &Dom,
    styles: &Styles,
    width: u16,
    images: &ImageContext,
) -> (LayoutTree, bool) {
    let tree = layout_tree_with(dom, styles, width, Hidden::Respect, images);
    if lines::from_tree(&tree).iter().any(|l| !l.spans.is_empty()) {
        return (tree, false);
    }
    let revealed = layout_tree_with(dom, styles, width, Hidden::Reveal, images);
    if lines::from_tree(&revealed)
        .iter()
        .any(|l| !l.spans.is_empty())
    {
        (revealed, true)
    } else {
        (tree, false)
    }
}

/// Lay the document out at `width` cells and return display lines.
pub fn layout(dom: &Dom, styles: &Styles, width: u16, hidden: Hidden) -> Vec<Line> {
    lines::from_tree(&layout_tree(dom, styles, width, hidden))
}

/// Full box tree (for paint, F3, benches).
pub fn layout_document(dom: &Dom, styles: &Styles, width: u16, hidden: Hidden) -> LayoutTree {
    layout_tree(dom, styles, width, hidden)
}

/// Full box tree with image context (M8).
pub fn layout_document_with(
    dom: &Dom,
    styles: &Styles,
    width: u16,
    hidden: Hidden,
    images: &ImageContext,
) -> LayoutTree {
    layout_tree_with(dom, styles, width, hidden, images)
}

/// Rasterise a laid-out tree into display lines (viewport scroll range,
/// `--dump-text`). Same tree the display list was painted from.
pub fn lines_from_tree(tree: &LayoutTree) -> Vec<Line> {
    lines::from_tree(tree)
}

/// Lines as text with style markers for tests: `[b]bold[/]`, `[u #5c5cff]…`.
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
            } else if color.is_empty() {
                out.push('[');
                out.push_str(&markers);
                out.push(']');
                out.push_str(&span.text);
                out.push_str("[/]");
            } else if markers.is_empty() {
                out.push('[');
                out.push_str(&color);
                out.push(']');
                out.push_str(&span.text);
                out.push_str("[/]");
            } else {
                out.push('[');
                out.push_str(&markers);
                out.push(' ');
                out.push_str(&color);
                out.push(']');
                out.push_str(&span.text);
                out.push_str("[/]");
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::style;
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    fn styled_dom(html: &str, css: &str) -> (crate::dom::Dom, Styles) {
        let dom = html::parse(html);
        let sheet = crate::css::parse(css);
        let styles = style::style_tree(&dom, &[&sheet]);
        (dom, styles)
    }

    fn lines(html: &str, width: u16) -> Vec<Line> {
        let (dom, styles) = styled_dom(html, "");
        layout(&dom, &styles, width, Hidden::Respect)
    }

    fn lines_styled(html: &str, css: &str, width: u16) -> Vec<Line> {
        let (dom, styles) = styled_dom(html, css);
        layout(&dom, &styles, width, Hidden::Respect)
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn cells(line: &Line) -> usize {
        line.spans.iter().map(|s| s.text.width()).sum()
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text).collect()
    }

    #[test]
    fn paragraphs_stack_with_a_blank_line_from_margins() {
        // UA margin 1em top+bottom on p → adjacent collapse to one blank row.
        let out = plain(&lines("<p>one</p><p>two</p>", 40));
        assert!(out.iter().any(|l| l.contains("one")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("two")), "{out:?}");
        let one = out.iter().position(|l| l.contains("one")).unwrap();
        let two = out.iter().position(|l| l.contains("two")).unwrap();
        assert!(
            two > one + 1,
            "expected a blank between paragraphs: {out:?}"
        );
        assert!(
            out[one + 1..two].iter().any(|l| l.trim().is_empty()),
            "gap missing: {out:?}"
        );
    }

    #[test]
    fn words_wrap_at_width() {
        let out = lines("<p>one two three four five</p>", 10);
        for line in &out {
            if text(line).trim().is_empty() {
                continue;
            }
            assert!(
                cells(line) <= 10,
                "line {:?} is {} cells",
                text(line),
                cells(line)
            );
        }
        let joined: String = out.iter().map(text).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("one"));
        assert!(joined.contains("five"));
    }

    #[test]
    fn overlong_word_hard_breaks() {
        let out = lines("<p>abcdefghijklmnopqrstuvwxyz</p>", 10);
        let content: Vec<_> = out
            .iter()
            .map(text)
            .filter(|t| !t.trim().is_empty())
            .collect();
        assert!(content.len() >= 3, "{content:?}");
        for t in &content {
            assert!(t.width() <= 10, "{t}");
        }
    }

    #[test]
    fn cjk_wraps_by_cell_width() {
        // Five 2-cell chars at width 9 → 8-cell line then 2-cell line.
        let out = lines(&format!("<p>{}</p>", "世".repeat(5)), 9);
        let content: Vec<_> = out
            .iter()
            .map(text)
            .filter(|t| !t.trim().is_empty())
            .collect();
        assert!(content.iter().all(|t| t.width() <= 9), "{content:?}");
        assert!(
            content
                .iter()
                .all(|t| t.chars().all(|c| c.width() != Some(1) || c == ' '))
        );
    }

    #[test]
    fn degenerate_widths_terminate() {
        let _ = lines("<ul><li>a<ul><li>b</li></ul></li></ul>", 0);
        let _ = lines("<ul><li>a<ul><li>b</li></ul></li></ul>", 1);
    }

    #[test]
    fn display_none_hides_subtree() {
        let out = lines_styled(
            "<p>vis</p><p class=ad>hidden</p>",
            ".ad { display: none }",
            40,
        );
        let all = plain(&out).join("\n");
        assert!(all.contains("vis"), "{all}");
        assert!(!all.contains("hidden"), "{all}");
    }

    #[test]
    fn display_block_on_span_breaks_the_line() {
        let out = plain(&lines_styled(
            "a<span>b</span>c",
            "span { display: block }",
            20,
        ));
        // a, then b on its own block, then c — not "abc" on one line.
        let joined = out.join("|");
        assert!(
            !joined.replace('|', "").contains("abc")
                || out.iter().filter(|l| !l.trim().is_empty()).count() >= 2,
            "{out:?}"
        );
    }

    #[test]
    fn display_inline_on_p_flows() {
        let out = plain(&lines_styled(
            "<p>a</p><p>b</p>",
            "p { display: inline; margin: 0 }",
            20,
        ));
        let content: Vec<_> = out.into_iter().filter(|l| !l.trim().is_empty()).collect();
        // Both on one line when inline with no margin.
        assert_eq!(content.len(), 1, "{content:?}");
        assert!(
            content[0].contains('a') && content[0].contains('b'),
            "{content:?}"
        );
    }

    #[test]
    fn width_and_max_width_constrain_content() {
        let (dom, styles) = styled_dom(
            "<div>hello world this is long</div>",
            "div { max-width: 80px; margin: 0 }",
        );
        let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
        // Find the div box.
        let mut found = false;
        tree.walk(tree.root, &mut |_id, b| {
            if b.kind == BoxKind::Block
                && let Some(n) = b.node
                && matches!(&dom.node(n).data, crate::dom::NodeData::Element { tag, .. } if tag == "div")
            {
                // 80px / 8 = 10 cells max content width.
                assert!(
                    b.dimensions.content.width <= 10,
                    "width {}",
                    b.dimensions.content.width
                );
                found = true;
            }
        });
        assert!(found, "div box missing");
        let _ = (dom, styles);
    }

    #[test]
    fn text_align_center_shifts_content() {
        let out = lines_styled("<p>hi</p>", "p { text-align: center; margin: 0 }", 20);
        let line = out.iter().find(|l| text(l).contains("hi")).unwrap();
        let t = text(line);
        let pad = t.find("hi").unwrap();
        // Content is 2 cells; pad ≈ (20 - 2) / 2 = 9.
        assert!(
            (8..=10).contains(&pad),
            "expected ~9 cells of pad, got {pad}: {t:?}"
        );
    }

    #[test]
    fn block_horizontal_margins_resolve_to_cells() {
        let (dom, styles) = styled_dom(
            "<div>x</div>",
            "div { margin-left: 16px; margin-right: 16px; margin-top: 0; margin-bottom: 0 }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        tree.walk(tree.root, &mut |_id, b| {
            if b.kind == BoxKind::Block && b.dimensions.margin.left > 0 {
                assert_eq!(b.dimensions.margin.left, 2); // 16px / 8
                assert_eq!(b.dimensions.margin.right, 2);
            }
        });
    }

    #[test]
    fn anonymous_block_keeps_previous_block_margin() {
        // `<p>one</p>two` — the paragraph's margin-bottom must not vanish when
        // the following text becomes an anonymous block.
        let out = plain(&lines_styled(
            "<div><p>one</p>two</div>",
            "p { margin-top: 0; margin-bottom: 1em } div { margin: 0 }",
            40,
        ));
        let one = out.iter().position(|l| l.contains("one")).unwrap();
        let two = out.iter().position(|l| l.contains("two")).unwrap();
        assert!(
            two > one + 1,
            "expected a blank row between block and loose text: {out:?}"
        );
    }

    #[test]
    fn inline_margin_right_separates_without_dom_whitespace() {
        // HN header: `.hnname { margin-right: 5px }` with no text node between.
        let out = plain(&lines_styled(
            "<b class=hnname>Hacker News</b><a href=n>new</a>",
            ".hnname { margin-right: 5px } a { margin: 0 }",
            40,
        ));
        let all = out.join("");
        assert!(
            !all.contains("Hacker Newsnew"),
            "inline margin-right must separate: {all:?}"
        );
        assert!(
            all.contains("Hacker News") && all.contains("new"),
            "{all:?}"
        );
        // At least one cell of gap.
        let pos = all.find("Hacker News").unwrap() + "Hacker News".len();
        assert_eq!(&all[pos..pos + 1], " ", "{all:?}");
    }

    #[test]
    fn nested_br_inside_inline_forces_a_line_break() {
        let out = plain(&lines_styled(
            "<p style='margin:0'><span>a<br>b</span></p>",
            "",
            40,
        ));
        let content: Vec<_> = out.into_iter().filter(|l| !l.trim().is_empty()).collect();
        assert!(
            content.iter().any(|l| l.contains('a')) && content.iter().any(|l| l.contains('b')),
            "{content:?}"
        );
        assert!(content.len() >= 2, "expected two rows, got {content:?}");
        assert!(
            !content.iter().any(|l| l.contains("ab")),
            "br must not glue: {content:?}"
        );
    }

    #[test]
    fn list_item_with_block_child_still_gets_a_bullet() {
        let out = plain(&lines_styled(
            "<ul style='margin:0;padding-left:2em'><li><p style='margin:0'>item</p></li></ul>",
            "",
            40,
        ));
        let all = out.join("\n");
        assert!(all.contains('•'), "missing bullet: {all:?}");
        assert!(all.contains("item"), "{all:?}");
    }

    #[test]
    fn pre_preserves_mixed_styles() {
        let out = lines_styled("<pre>a<b>b</b>c</pre>", "", 40);
        let dbg = debug_lines(&out);
        assert!(dbg.contains('a') && dbg.contains('c'), "{dbg}");
        // The bold `b` must keep BOLD in its span, not collapse to one style.
        assert!(
            dbg.contains("[b]b[/]") || dbg.contains("[b]"),
            "bold inside pre lost: {dbg}"
        );
    }

    #[test]
    fn inline_list_items_keep_a_space_between_them() {
        // M5.0 reason: whitespace text nodes + inline layout → "a b".
        let out = lines_styled(
            "<ul><li>a</li>\n<li>b</li></ul>",
            "li { display: inline } ul { margin: 0; padding: 0 }",
            40,
        );
        let all = plain(&out).join("");
        assert!(
            all.contains("a b") || all.contains("a  b"),
            "expected space between inline items: {all:?}"
        );
    }

    #[test]
    fn layout_readable_reveals_a_blank_page() {
        let (dom, styles) = styled_dom("<body style='display:none'><p>hello</p></body>", "");
        // Confirm cascade first: body really is display:none from the attribute.
        fn find_tag(dom: &crate::dom::Dom, tag: &str) -> crate::dom::NodeId {
            fn walk(
                dom: &crate::dom::Dom,
                id: crate::dom::NodeId,
                tag: &str,
            ) -> Option<crate::dom::NodeId> {
                if matches!(&dom.node(id).data, crate::dom::NodeData::Element { tag: t, .. } if t == tag)
                {
                    return Some(id);
                }
                dom.children(id).find_map(|c| walk(dom, c, tag))
            }
            walk(dom, dom.root, tag).unwrap()
        }
        assert_eq!(
            styles.get(find_tag(&dom, "body")).display,
            crate::style::values::Display::None
        );
        let respect = layout(&dom, &styles, 40, Hidden::Respect);
        let reveal = layout(&dom, &styles, 40, Hidden::Reveal);
        assert!(
            plain(&respect).join("").trim().is_empty(),
            "respect should be blank: {:?}",
            plain(&respect)
        );
        assert!(
            plain(&reveal).join("\n").contains("hello"),
            "reveal should show text: {:?}",
            plain(&reveal)
        );
        let (lines, revealed) = layout_readable(&dom, &styles, 40);
        assert!(revealed, "layout_readable must report the fallback");
        assert!(
            plain(&lines).join("\n").contains("hello"),
            "{:?}",
            plain(&lines)
        );
    }

    #[test]
    fn box_tree_has_positions_and_sizes() {
        let (dom, styles) = styled_dom("<p>hi</p>", "");
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        assert!(tree.height > 0);
        let mut saw_text = false;
        tree.walk(tree.root, &mut |_id, b| {
            if b.kind == BoxKind::Text {
                saw_text = true;
                assert!(b.dimensions.content.width > 0);
            }
        });
        assert!(saw_text);
    }

    // ---- M9.2 sizing: the cases a golden cannot show -----------------------

    /// Every rect in the tree, for the invariants that must hold everywhere.
    fn all_rects(tree: &LayoutTree) -> Vec<Rect> {
        let mut out = Vec::new();
        tree.walk(tree.root, &mut |_, b| out.push(b.dimensions.content));
        out
    }

    #[test]
    fn a_border_box_narrower_than_its_own_padding_floors_at_zero() {
        // 16px = 2 cells of declared width, but padding alone is 2 cells a side
        // and the border another 1. `border-box` subtracts more than there is;
        // the answer is an empty content box, not a negative one, and layout
        // must survive laying text out into it.
        let (dom, styles) = styled_dom(
            "<div>squeezed</div>",
            "div { box-sizing: border-box; width: 16px; padding: 16px; border: 8px solid }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let div = all_rects(&tree)
            .into_iter()
            .find(|r| r.x == 3)
            .expect("the content box sits inside border 1 + padding 2");
        assert_eq!(div.width, 0, "content width must floor at zero");
        for r in all_rects(&tree) {
            assert!(
                r.width >= 0 && r.height >= 0,
                "negative rect in the tree: {r:?}"
            );
        }
    }

    #[test]
    fn content_overflowing_a_short_box_still_rasterises() {
        // `height: 0` with three lines inside: the box takes no rows in the
        // flow, but `overflow: visible` means those lines are still on the
        // page. If the document height only counted the flow, `from_tree`
        // would drop them (they sit below its last row).
        let (dom, styles) = styled_dom(
            "<div class=zero>alpha<br>beta<br>gamma</div>",
            "div { margin: 0; height: 0 }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let out = plain(&lines_from_tree(&tree));
        let all = out.join("\n");
        for word in ["alpha", "beta", "gamma"] {
            assert!(all.contains(word), "overflowing row lost: {out:?}");
        }
    }

    #[test]
    fn a_specified_height_is_the_used_height_whatever_the_content_does() {
        // Three lines of content in a 1-line box: the *next* sibling starts on
        // row 1, not row 3. This is the property flex will depend on.
        let (dom, styles) = styled_dom(
            "<div class=short>alpha<br>beta<br>gamma</div><div class=after>after</div>",
            "div { margin: 0 } .short { height: 1em }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut after_y = None;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text && b.text.as_deref() == Some("after") {
                after_y = Some(b.dimensions.content.y);
            }
        });
        assert_eq!(after_y, Some(1), "the flow must advance by the used height");
    }

    #[test]
    fn img_attrs_become_cell_size() {
        let dom = html::parse(r#"<img src="a.png" width="80" height="48" alt="pic">"#);
        let styles = style::style_tree(&dom, &[]);
        let imgs = crate::image::discover(&dom, Some("https://ex/"));
        let mut cache = crate::image::ImageCache::default();
        let ctx = crate::image::ImageContext::from_discovery(&imgs, &mut cache);
        let tree = layout_document_with(&dom, &styles, 40, Hidden::Respect, &ctx);
        let mut found = false;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Image {
                assert_eq!(b.dimensions.content.width, 10);
                assert_eq!(b.dimensions.content.height, 3);
                assert!(b.image_size_firm);
                assert_eq!(b.image_src.as_deref(), Some("https://ex/a.png"));
                found = true;
            }
        });
        assert!(found);
    }

    #[test]
    fn links_carry_underline_and_colour() {
        let out = lines("<p><a href='/x'>docs</a></p>", 40);
        let dbg = debug_lines(&out);
        assert!(dbg.contains("docs"), "{dbg}");
        assert!(dbg.contains('u') || dbg.contains("#5c5cff"), "{dbg}");
    }

    #[test]
    fn pre_preserves_newlines() {
        let out = lines("<pre>a\nb\nc</pre>", 40);
        let content: Vec<_> = plain(&out)
            .into_iter()
            .filter(|l| !l.trim().is_empty() || l.is_empty())
            .collect();
        let joined = plain(&out).join("\n");
        assert!(
            joined.contains('a') && joined.contains('b') && joined.contains('c'),
            "{joined}"
        );
        // At least three non-empty lines or explicit newlines between.
        let non_empty = content.iter().filter(|l| !l.is_empty()).count();
        assert!(non_empty >= 3, "{content:?}");
    }

    mod ladder {
        use super::*;
        use std::fs;

        fn fixture(name: &str) -> String {
            fs::read_to_string(format!(
                "{}/tests/fixtures/{name}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        }

        fn check(html: &str, min_lines: usize) -> Vec<Line> {
            let dom = html::parse(html);
            let sheets = style::sources::inline_sheets(&dom);
            let sheet_refs: Vec<_> = sheets.iter().collect();
            let styles = style::style_tree(&dom, &sheet_refs);
            let out = layout(&dom, &styles, 80, Hidden::Respect);
            assert!(
                out.len() >= min_lines,
                "expected ≥{min_lines} lines, got {}",
                out.len()
            );
            out
        }

        #[test]
        fn example_com() {
            let out = check(&fixture("example.com.html"), 2);
            let all = plain(&out).join("\n");
            assert!(all.to_lowercase().contains("example"), "{all}");
        }

        #[test]
        fn motherfuckingwebsite_com() {
            let out = check(&fixture("motherfuckingwebsite.com.html"), 10);
            let all = plain(&out).join("\n");
            assert!(all.to_lowercase().contains("motherfucking"), "{all}");
        }

        #[test]
        fn danluu_com() {
            let out = check(&fixture("danluu.com.html"), 20);
            assert!(out.iter().any(|l| cells(l) > 0));
        }

        #[test]
        fn news_ycombinator_com() {
            let html = fixture("news.ycombinator.com.html");
            let css = fixture("news.ycombinator.com.news.css");
            let dom = html::parse(&html);
            let page = crate::css::parse(&css);
            let inline = style::sources::inline_sheets(&dom);
            let mut sheets: Vec<&crate::css::Stylesheet> = inline.iter().collect();
            sheets.push(&page);
            let styles = style::style_tree(&dom, &sheets);
            let out = layout(&dom, &styles, 80, Hidden::Respect);
            assert!(out.len() > 10);
            let all = plain(&out).join("\n");
            assert!(
                all.contains("Hacker News") || all.contains("Hacker"),
                "{all}"
            );
        }

        #[test]
        fn en_wikipedia_org() {
            let out = check(&fixture("en.wikipedia.org.html"), 100);
            assert!(out.len() > 100);
        }
    }
}
