//! Layout: DOM + styles + width → box tree (PLAN.md M5).
//!
//! Pure transform. The box tree is the stage's real output; `layout` /
//! `layout_readable` also rasterise it to `Vec<Line>` so the viewport and
//! `--dump-text` keep working until the display-list paint path lands.

mod boxes;
mod clip;
mod dimensions;
mod engine;
mod flex;
mod hit;
pub(crate) mod intrinsic;
mod lines;

pub use boxes::{BoxId, BoxKind, LayoutBox, LayoutTree};
pub use clip::Clip;
pub use dimensions::{Dimensions, EdgeSizes, Rect};
pub use engine::{Hidden, layout_tree, layout_tree_with, term_color, term_style};
pub use hit::{
    LinkHit, collect_links, dom_links, first_y, hit_test, is_under, link_at, nearest_link,
    visible_links,
};
pub use intrinsic::IntrinsicSizer;

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
    if has_visible_content(&tree) {
        return (tree, false);
    }
    let revealed = layout_tree_with(dom, styles, width, Hidden::Reveal, images);
    if has_visible_content(&revealed) {
        (revealed, true)
    } else {
        (tree, false)
    }
}

/// Does this tree put anything on screen? The reveal fallback's question,
/// asked of the boxes rather than of a rasterised copy of them: a caller that
/// wants the lines builds them once afterwards, and one that only wants the
/// tree (`F3`, `--dump-boxes`, the layout goldens) never builds them at all.
fn has_visible_content(tree: &LayoutTree) -> bool {
    // A tree with no rows cannot show anything, whatever boxes it holds —
    // `lines::from_tree` says the same by returning nothing.
    if tree.height <= 0 {
        return false;
    }
    let mut visible = false;
    tree.walk(tree.root, &mut |_, b| {
        visible |= match b.kind {
            BoxKind::Text => b.text.as_deref().is_some_and(|t| !t.is_empty()),
            BoxKind::Image => true,
            _ => false,
        };
    });
    visible
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

    /// Every box's kind and content-box geometry, in allocation order — what
    /// two documents must share to be laying out identically.
    fn geometry(html_src: &str, css: &str, width: u16) -> Vec<(BoxKind, i32, i32, i32, i32)> {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        tree.boxes
            .iter()
            .map(|b| {
                let d = b.dimensions.content;
                (b.kind, d.x, d.y, d.width, d.height)
            })
            .collect()
    }

    /// Every flex item's content box under the container with id `r`, as
    /// `(x, width)` in layout order — which for a flex container is
    /// order-modified document order.
    fn items(html_src: &str, css: &str, width: u16) -> Vec<(i32, i32)> {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        let mut out = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                out = b
                    .children
                    .iter()
                    .map(|&c| {
                        let d = tree.get(c).dimensions;
                        (d.margin_box().x, d.content.width)
                    })
                    .collect();
            }
        });
        out
    }

    #[test]
    fn flex_items_sit_side_by_side_at_their_own_widths() {
        // The row basics: three items 10 cells wide in an 80-cell container.
        // `flex-grow` is 0 initially, so nobody takes the 50 cells left over —
        // where that room goes is M9.7's question, not this task's.
        let row = "<div id=r><div class=i>a</div><div class=i>b</div><div class=i>c</div></div>";
        let css = "#r { display: flex } .i { flex-basis: 80px }";
        assert_eq!(items(row, css, 80), [(0, 10), (10, 10), (20, 10)]);

        // Same row, same y: side-by-side is the whole point.
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
        let mut ys = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text {
                ys.push(b.dimensions.content.y);
            }
        });
        assert_eq!(ys, [0, 0, 0], "items share the line's rows");
    }

    #[test]
    fn a_row_is_as_tall_as_its_tallest_item() {
        // Cross sizing is M9.8's, but a container has to be tall enough to
        // hold what is in it or the page scrolls past its own content.
        let row = "<div id=r><div>one</div><div>two words here that wrap</div></div>";
        let (dom, styles) = styled_dom(row, "#r { display: flex } #r div { flex-basis: 100% }");
        let tree = layout_document(&dom, &styles, 20, Hidden::Respect);
        let mut flex_h = 0;
        let mut tallest_item = 0;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                flex_h = b.dimensions.content.height;
                tallest_item = b
                    .children
                    .iter()
                    .map(|&c| tree.get(c).dimensions.margin_box().height)
                    .max()
                    .unwrap_or(0);
            }
        });
        assert!(tallest_item > 1, "the second item must have wrapped");
        assert_eq!(flex_h, tallest_item);
    }

    #[test]
    fn flex_basis_reads_every_spelling_the_page_can_write() {
        let row = "<div id=r><div class=a>aa</div><div class=b>bb</div></div>";
        // A length, and a percentage of the container's definite inner size.
        assert_eq!(
            items(
                row,
                "#r{display:flex} .a{flex-basis:160px} .b{flex-basis:25%}",
                80
            ),
            [(0, 20), (20, 20)]
        );
        // `auto` defers to the main-axis size property...
        assert_eq!(
            items(row, "#r{display:flex} .a{width:240px} .b{width:80px}", 80),
            [(0, 30), (30, 10)]
        );
        // ...and to max-content when there is no width either, which is the
        // case that needs M9.4. Both items are two cells of text.
        assert_eq!(items(row, "#r{display:flex}", 80), [(0, 2), (2, 2)]);
        // `content` is max-content outright, whatever `width` says.
        assert_eq!(
            items(
                row,
                "#r{display:flex} div{width:240px;flex-basis:content}",
                80
            ),
            [(0, 2), (2, 2)]
        );
    }

    #[test]
    fn gaps_take_their_cells_before_the_items_do() {
        // Two growing items and a 2-cell gap in 20 cells: 18 to divide, 9 each,
        // and the second item starts one gap past the first.
        let row = "<div id=r><div>a</div><div>b</div></div>";
        assert_eq!(
            items(row, "#r { display: flex; gap: 1em } #r div { flex: 1 }", 20),
            [(0, 9), (11, 9)]
        );
    }

    #[test]
    fn row_gap_is_the_cross_axis_gap_and_a_row_has_one_line() {
        // `gap: <row> <column>`: the row gap goes *between lines*, and a
        // `nowrap` row has a single line, so it must not reach the main axis or
        // the container's height. M9.10 is where it starts doing something.
        let row = "<div id=r><div>a</div><div>b</div></div>";
        let sizing = "#r div { flex: 0 0 80px }";
        let both = format!("#r {{ display: flex; gap: 4em 1em }} {sizing}");
        let column_only = format!("#r {{ display: flex; gap: 0 1em }} {sizing}");
        assert_eq!(items(row, &both, 40), items(row, &column_only, 40));

        let (dom, styles) = styled_dom(row, &both);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut height = 0;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                height = b.dimensions.content.height;
            }
        });
        assert_eq!(height, 1, "a four-line row gap on a one-line row");
    }

    #[test]
    fn order_moves_an_item_visually_and_leaves_the_document_alone() {
        let row = "<div id=r><a href=/1>one</a><a class=second href=/2>two</a></div>";
        let css = "#r { display: flex } a { flex-basis: 25% }";
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        // Document order, no `order`: /1 is on the left.
        assert_eq!(
            hit::link_at(&tree, &dom, 1, 0).map(|(_, u)| u),
            Some("/1".into())
        );

        let css = "#r { display: flex } a { flex-basis: 25% } .second { order: -1 }";
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        // ...and with `order: -1` the second link is the one on the left.
        assert_eq!(
            hit::link_at(&tree, &dom, 1, 0).map(|(_, u)| u),
            Some("/2".into())
        );
        // The DOM never moved: `order` is a layout instruction, not an edit.
        assert_eq!(
            hit::dom_links(&dom)
                .iter()
                .map(|(_, u)| u.as_str())
                .collect::<Vec<_>>(),
            ["/1", "/2"]
        );
        // ...and hit-testing still finds each link where it was drawn, which is
        // what `/` search, link hints and the focus ring all walk.
        assert_eq!(
            hit::link_at(&tree, &dom, 11, 0).map(|(_, u)| u),
            Some("/1".into())
        );
    }

    #[test]
    fn a_flex_item_can_itself_be_a_flex_container() {
        // The recursion the algorithm needs: an item is laid out at its
        // resolved width by whatever formatting context its own `display` says,
        // so a nested row divides *its* width the same way.
        let markup = "<div id=r><div id=n><div>a</div><div>b</div></div><div>c</div></div>";
        let css = "#r { display: flex } #n { display: flex } #r > div { flex: 1 }
                   #n > div { flex: 1 }";
        let (dom, styles) = styled_dom(markup, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut rows = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                rows.push((
                    b.dimensions.content.width,
                    b.children
                        .iter()
                        .map(|&c| tree.get(c).dimensions.content.width)
                        .collect::<Vec<_>>(),
                ));
            }
        });
        // The outer row splits 40 into 20/20; the nested one splits its own 20
        // into 10/10.
        assert_eq!(rows, [(40, vec![20, 20]), (20, vec![10, 10])]);
    }

    /// Every flex item in `tree`, with the width of its box and how far the
    /// text inside it actually reaches. The two agreeing is what it means for
    /// a measurement to have predicted the boxes it produced.
    fn item_text_extents(tree: &LayoutTree) -> Vec<(i32, i32)> {
        let mut out = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind != BoxKind::Flex {
                return;
            }
            for &item in &b.children {
                let box_width = tree.get(item).dimensions.content.width;
                let left = tree.get(item).dimensions.content.x;
                let mut reach = 0;
                tree.walk(item, &mut |_, t| {
                    if t.kind == BoxKind::Text {
                        reach = reach.max(t.dimensions.content.right() - left);
                    }
                });
                out.push((box_width, reach));
            }
        });
        out
    }

    #[test]
    fn a_list_item_flex_item_is_sized_with_the_marker_it_lays_out_with() {
        // M9.4's first known divergence, closed here. An `<li>` inside a flex
        // *container* is a flex item that is still a list item inside, so the
        // engine injects the two-cell marker while building its boxes. If the
        // measurement behind its flex base size did not count those two cells,
        // the box would come out two cells short and the item's last word would
        // drop to a second line. Nothing here is compared against a number from
        // the implementation: the box is compared against the text in it.
        let markup = "<ul id=r><li>alpha beta</li><li>gamma delta</li></ul>";
        let (dom, styles) = styled_dom(markup, "#r { display: flex }");
        let tree = layout_document(&dom, &styles, 60, Hidden::Respect);
        let extents = item_text_extents(&tree);
        assert_eq!(extents.len(), 2, "two items: {extents:?}");
        for (width, reach) in extents {
            assert!(
                reach <= width,
                "text reaches {reach} in a {width}-cell item"
            );
        }
        // ...and the marker really is there, so the test above is measuring the
        // case it claims to (a flex *item* keeps its marker; a flex *container*
        // that is itself an `<li>` does not — see the test below).
        let rows = plain(&lines_styled(markup, "#r { display: flex }", 60));
        assert!(rows[0].contains("• alpha beta"), "{rows:?}");
        assert_eq!(rows.len(), 2, "nothing wrapped: {rows:?}");
    }

    #[test]
    fn a_revealed_page_measures_the_flex_items_it_lays_out() {
        // M9.4's second known divergence, closed here. A page that hides
        // itself gets laid out again with `display:none` ignored (M4's
        // never-blank rescue). Intrinsic sizing has to be told, or every flex
        // item on such a page is measured as nothing while the engine builds a
        // real box for it — items would collapse to zero and their text would
        // spill across the row.
        let markup = "<body style='display:none'>\
                      <div id=r><div>alpha</div><div>beta</div></div></body>";
        let (dom, styles) = styled_dom(markup, "#r { display: flex }");
        let (tree, revealed) =
            layout_document_readable(&dom, &styles, 40, &ImageContext::default());
        assert!(revealed, "the fixture must be rescued by the reveal pass");
        let extents = item_text_extents(&tree);
        assert_eq!(extents.len(), 2, "two items: {extents:?}");
        for (width, reach) in &extents {
            assert!(*width > 0, "a revealed item collapsed to nothing");
            assert!(
                reach <= width,
                "text reaches {reach} in a {width}-cell item"
            );
        }
        // Both words are on the row, side by side, with the second past the
        // first — which is only true if the first item was sized from content
        // the sizer was willing to look at.
        assert_eq!(plain(&lines_from_tree(&tree))[0].trim_end(), "alphabeta");
    }

    #[test]
    fn a_flex_list_item_loses_its_marker_the_way_a_browser_drops_it() {
        // `display: flex` replaces `display: list-item`, so the marker a list
        // item would have generated is not generated — which is what a browser
        // shows for danluu.com's `li{display:flex}` link list, and what this
        // engine now shows too. The bullet is emitted by the block path only.
        assert_eq!(
            plain(&lines_styled(
                "<ul><li>first</li></ul>",
                "li{display:block}",
                40
            )),
            ["    • first", ""]
        );
        assert_eq!(
            plain(&lines_styled(
                "<ul><li>first</li></ul>",
                "li{display:flex}",
                40
            )),
            ["    first", ""]
        );
    }

    #[test]
    fn text_between_items_becomes_one_anonymous_item_and_whitespace_none() {
        // §4: element children are items; contiguous text between them is one
        // anonymous item; whitespace-only runs generate nothing, which is why
        // the newlines in the markup below do not become items of their own.
        let markup = "<div id=r>\n  lead text\n  <b>bee</b>\n  tail\n</div>";
        let (dom, styles) = styled_dom(markup, "#r { display: flex }");
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut kinds = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                kinds = b.children.iter().map(|&c| tree.get(c).kind).collect();
            }
        });
        assert_eq!(
            kinds,
            [
                BoxKind::AnonymousBlock,
                BoxKind::Block,
                BoxKind::AnonymousBlock
            ],
            "lead text, <b>, tail — and nothing for the newlines between them"
        );
        // The `<b>` was blockified: it is an item with a box, not a word on a
        // line shared with the text either side of it. So the whitespace
        // *between* items is gone — each anonymous item is its own inline
        // formatting context and trims its own edges, and the items are then
        // packed against each other. That is the flexbox gotcha every page
        // author meets: spaces between inline-blocks show, spaces between flex
        // items do not.
        let text: Vec<String> = plain(&lines_styled(markup, "#r { display: flex }", 40));
        assert_eq!(text[0].trim_end(), "lead textbeetail");
    }

    #[test]
    fn an_items_own_edges_come_out_of_the_line_before_it_is_divided() {
        // Free space is measured against *outer* sizes: an item's margin,
        // border and padding are not room the algorithm may hand to anyone,
        // including itself. In 40 cells with one item carrying 4 cells of
        // margin and 2 of padding, the 34 that are left split 17/17 — and the
        // second item starts past all six of the first item's edge cells.
        let row = "<div id=r><div class=a>a</div><div class=b>b</div></div>";
        let css = "#r { display: flex } #r div { flex: 1 }
                   .a { margin: 0 1em; padding: 0 8px }";
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut boxes = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                boxes = b
                    .children
                    .iter()
                    .map(|&c| {
                        let d = tree.get(c).dimensions;
                        (
                            d.margin_box().x,
                            d.content.x,
                            d.content.width,
                            d.margin_box().width,
                        )
                    })
                    .collect();
            }
        });
        assert_eq!(boxes, [(0, 3, 17, 23), (23, 23, 17, 17)]);
        // ...and the row is still filled exactly: 23 + 17 = 40.
        assert_eq!(boxes.iter().map(|b| b.3).sum::<i32>(), 40);
    }

    #[test]
    fn a_border_box_basis_counts_the_edges_it_says_it_does() {
        // `box-sizing` applies to `flex-basis` the same way it applies to
        // `width` (M9.2's arithmetic, called rather than restated): a 20-cell
        // border-box basis on an item with 2 cells of padding is 18 cells of
        // content, and a content-box one is 20.
        let row = "<div id=r><div class=a>a</div></div>";
        let css = "#r { display: flex } .a { flex-basis: 160px; padding: 0 8px }";
        assert_eq!(items(row, css, 40), [(0, 20)]);
        let css = format!("{css} .a {{ box-sizing: border-box }}");
        assert_eq!(items(row, &css, 40), [(0, 18)]);
    }

    #[test]
    fn a_grown_row_fills_its_container_exactly_at_every_width() {
        // The integer-cell invariant, swept over the whole stage rather than
        // over `flex::resolve` alone: whatever the terminal width, the items
        // plus their gaps are exactly the container's inner size, no item is
        // negative, and no item is lost. Rounding that leaked a cell would show
        // up here as a one-cell hole at the end of the row on some widths and
        // not others — the kind of bug a single fixture never catches.
        let markup = "<div id=r><div class=a>alpha</div><div class=b>b</div>\
                      <div class=c>gamma</div></div>";
        let css = "#r { display: flex; gap: 1em } .a { flex: 1 } .b { flex: 2 }
                   .c { flex: 1 }";
        for width in 20..=120u16 {
            let (dom, styles) = styled_dom(markup, css);
            let tree = layout_document(&dom, &styles, width, Hidden::Respect);
            let mut checked = false;
            tree.walk(tree.root, &mut |_, b| {
                if b.kind != BoxKind::Flex {
                    return;
                }
                checked = true;
                let widths: Vec<i32> = b
                    .children
                    .iter()
                    .map(|&c| tree.get(c).dimensions.margin_box().width)
                    .collect();
                assert_eq!(widths.len(), 3, "width {width}: an item went missing");
                assert!(
                    widths.iter().all(|&w| w >= 0),
                    "width {width}: negative item {widths:?}"
                );
                // Two gaps of 2 cells between three items.
                assert_eq!(
                    widths.iter().sum::<i32>() + 4,
                    b.dimensions.content.width,
                    "width {width}: items {widths:?} do not fill the row"
                );
            });
            assert!(checked, "width {width}: no flex container in the tree");
        }
    }

    #[test]
    fn alignment_moves_everything_inside_the_item_it_moves() {
        // The classic flex bug is a centred item whose text stayed at the old
        // x. On the main axis it cannot happen in this engine's shape — an item
        // is *placed* before its contents are laid out, so there is no second
        // position to forget to update — and this is the test that says so
        // rather than leaving it to a reader of the code. The cross axis does
        // move built boxes and is pinned separately, in
        // `cross_alignment_moves_everything_inside_the_item_it_moves`.
        let row = "<div id=r><div class=i><b>hi</b> there</div></div>";
        let css = "#r { display: flex; justify-content: center } .i { flex: 0 0 160px }";
        // One 20-cell item in 80 cells: 60 free, 30 of it before the item.
        assert_eq!(items(row, css, 80), [(30, 20)]);

        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
        let mut texts = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text {
                texts.push((
                    b.text.clone().unwrap_or_default(),
                    b.dimensions.content.x,
                    b.dimensions.content.right(),
                ));
            }
        });
        assert!(!texts.is_empty(), "the item has text to move");
        for (text, left, right) in &texts {
            assert!(
                *left >= 30 && *right <= 50,
                "{text:?} at {left}..{right} is outside the centred item"
            );
        }
        assert_eq!(texts[0].1, 30, "the line starts at the item's content edge");
        // ...and it is really on screen where the boxes say: the rasterised row
        // has 30 blank cells before the text.
        let row_text = &plain(&lines_styled(row, css, 80))[0];
        assert!(row_text.starts_with(&" ".repeat(30)), "{row_text:?}");
        assert_eq!(row_text.trim_end(), format!("{}hi there", " ".repeat(30)));
    }

    #[test]
    fn an_hr_item_is_pushed_by_its_auto_margin_instead_of_filling_it() {
        // M9.7 review. `<hr>` and `<br>` are handed a width and size themselves
        // from it, taking their own edges back out — which is why they are not
        // re-derived like a block. The auto-margin share is not one of their
        // edges: it is the line's space, sitting *beside* the box. Handed it as
        // well, an `<hr>` stretched across the very cells §9.5 had reserved to
        // push it right, and the row silently lost the margin it had granted.
        let row = "<div id=r><hr></div>";
        let pushed = "#r { display: flex } hr { flex: 0 0 80px; margin: 0; margin-left: auto }";
        // Nothing here is compared against a number from the implementation:
        // the same intent said the other way — `justify-content: flex-end` on a
        // row with one item — has to produce the same boxes, and the two ways
        // of pushing an item to main-end agreeing is the assertion.
        let by_alignment =
            "#r { display: flex; justify-content: flex-end } hr { flex: 0 0 80px; margin: 0 }";
        assert_eq!(geometry(row, pushed, 40), geometry(row, by_alignment, 40));
        // ...and it really is 10 cells at 30, not 40 cells at 0. The margin box
        // starts at 0 and spans the whole 40, because the granted cells land on
        // the *box* as well as on the line: a row whose items do not tile it is
        // a row that has lost track of its own free space.
        assert_eq!(items(row, pushed, 40), [(0, 10)]);
        let (dom, styles) = styled_dom(row, pushed);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut tiled = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                tiled = b
                    .children
                    .iter()
                    .map(|&c| tree.get(c).dimensions.margin_box().width)
                    .collect();
            }
        });
        assert_eq!(tiled, [40]);

        // The same for a replaced item, which reaches the line by the same path
        // and was already correct — pinned so the two stay together.
        let img = "<div id=r><img src=logo.png width=80 height=16></div>";
        let (dom, styles) = styled_dom(img, "#r { display: flex } img { margin-left: auto }");
        let imgs = crate::image::discover(&dom, Some("https://fixture.test/page"));
        let ctx = ImageContext::from_discovery(&imgs, &mut crate::image::ImageCache::default());
        let tree = layout_document_with(&dom, &styles, 40, Hidden::Respect, &ctx);
        let mut boxes = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Image {
                boxes.push((b.dimensions.content.x, b.dimensions.content.width));
            }
        });
        assert_eq!(boxes, [(30, 10)], "the image is pushed, not stretched");
    }

    #[test]
    fn row_reverse_starts_at_the_right_edge_and_hit_tests_where_it_drew() {
        // Main-start is the container's right edge, so the *first* item in
        // document order is the rightmost one and `flex-start` leaves its free
        // space on the left. Two 10-cell links in 40 cells: /1 at 30, /2 at 20,
        // and 20 cells spare at main-end.
        let row = "<div id=r><a href=/1>one</a><a href=/2>two</a></div>";
        let css = "#r { display: flex; flex-direction: row-reverse } a { flex: 0 0 80px }";
        assert_eq!(items(row, css, 40), [(30, 10), (20, 10)]);

        // The rest of the browser has to agree with that. Hit-testing is
        // geometric, so a link answers where it was drawn...
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        assert_eq!(
            hit::link_at(&tree, &dom, 30, 0).map(|(_, u)| u),
            Some("/1".into())
        );
        assert_eq!(
            hit::link_at(&tree, &dom, 20, 0).map(|(_, u)| u),
            Some("/2".into())
        );
        // ...and the DOM is untouched: a reversed axis is a layout instruction,
        // not an edit, so `/` search and F1 still see the document as written.
        assert_eq!(
            hit::dom_links(&dom)
                .iter()
                .map(|(_, u)| u.as_str())
                .collect::<Vec<_>>(),
            ["/1", "/2"]
        );
    }

    #[test]
    fn an_aligned_row_neither_overlaps_nor_overflows_at_any_width() {
        // The placement half of the integer-cell invariant, swept through the
        // whole stage. Three items that cannot flex, so every width leaves real
        // free space for `justify-content` to round: whatever it does with it,
        // the items stay in main-axis order, keep their gap, and stay inside
        // the container whenever there is room for them.
        let markup = "<div id=r><div>a</div><div>b</div><div>c</div></div>";
        for justify in [
            "flex-start",
            "flex-end",
            "center",
            "space-between",
            "space-around",
            "space-evenly",
        ] {
            let css = format!(
                "#r {{ display: flex; gap: 1em; justify-content: {justify} }}
                 #r div {{ flex: 0 0 80px }}"
            );
            for width in 20..=120u16 {
                let (dom, styles) = styled_dom(markup, &css);
                let tree = layout_document(&dom, &styles, width, Hidden::Respect);
                let mut boxes = Vec::new();
                tree.walk(tree.root, &mut |_, b| {
                    if b.kind == BoxKind::Flex {
                        boxes = b
                            .children
                            .iter()
                            .map(|&c| {
                                let mb = tree.get(c).dimensions.margin_box();
                                (mb.x, mb.width)
                            })
                            .collect();
                    }
                });
                let label = format!("{justify} at {width}: {boxes:?}");
                assert_eq!(boxes.len(), 3, "{label}");
                for pair in boxes.windows(2) {
                    // Two cells of gap between every adjacent pair, never less.
                    assert!(pair[1].0 - (pair[0].0 + pair[0].1) >= 2, "{label}");
                }
                // 3 items of 10 cells and 2 gaps of 2 need 34 cells. Narrower
                // than that the row overflows the end edge from main-start,
                // which is the overflow fallback and is pinned above.
                if width >= 34 {
                    assert!(boxes[0].0 >= 0, "{label}");
                    let end = boxes[2].0 + boxes[2].1;
                    assert!(end <= i32::from(width), "{label}");
                } else {
                    assert_eq!(boxes[0].0, 0, "{label}: overflow must pack at 0");
                }
            }
        }
    }

    // ---- M9.8 cross sizing and alignment -----------------------------------

    /// Every flex item's `(margin-box y, content height)` under the container
    /// with id `r`, in layout order — the cross-axis counterpart of [`items`].
    fn cross(html_src: &str, css: &str, width: u16) -> Vec<(i32, i32)> {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        let mut out = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                out = b
                    .children
                    .iter()
                    .map(|&c| {
                        let d = tree.get(c).dimensions;
                        (d.margin_box().y, d.content.height)
                    })
                    .collect();
            }
        });
        out
    }

    #[test]
    fn a_definite_container_height_is_the_lines_cross_size() {
        // §9.4 step 7, and the case no golden built out of content can show:
        // when the container states a height, *that* is the line the items are
        // aligned in — not the tallest item's height. A 10-row container
        // centres a 1-row item at 5 and a 3-row item at 4, and both would sit
        // at 0 and 0 if the line were only as tall as its contents.
        let row = "<div id=r><div class=a>one</div><div class=b>two words here</div></div>";
        let css = "#r { display: flex; height: 10em; align-items: center }
                   #r div { flex: 0 0 32px }";
        assert_eq!(cross(row, css, 40), [(5, 1), (4, 3)]);
        // The container really is 10 rows: `layout_box_at` applies the
        // specified height exactly as it does for a block, and the flex code
        // does not get a second say in it.
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut container = 0;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                container = b.dimensions.content.height;
            }
        });
        assert_eq!(container, 10);
    }

    #[test]
    fn stretching_never_squeezes_an_item_below_its_own_text() {
        // §4.5 on the cross axis: `min-height: auto` on a flex item is its
        // content height. A container 2 rows tall stretches its items to 2 —
        // and an item with 5 rows of text in it keeps all 5 and overflows,
        // rather than having its box end three rows above its own last line.
        let row = "<div id=r><div>a b c d e</div></div>";
        let css = "#r { display: flex; height: 2em } #r div { flex: 0 0 16px }";
        assert_eq!(cross(row, css, 40), [(0, 5)]);
        // An explicit `min-height` replaces the automatic one, and a page that
        // states a floor of 1 row has said the text may be clipped: the item
        // stretches to the line's 2 rows and stops there, three rows short of
        // its own last line.
        let with_min = format!("{css} #r div {{ min-height: 1em }}");
        assert_eq!(cross(row, &with_min, 40), [(0, 2)]);
    }

    #[test]
    fn cross_alignment_moves_everything_inside_the_item_it_moves() {
        // The cross axis is the one place this stage moves a box *after*
        // building it: a line's height is not known until its tallest item has
        // been laid out, so a short item is built at the line's top edge and
        // dropped into place afterwards. That is the shape the main axis was
        // written to avoid, so the thing it risks — a box that moved and left
        // its text behind — is pinned here instead.
        let row = "<div id=r><div class=a><b>hi</b> there</div><div class=b>x y z w v</div></div>";
        let css = "#r { display: flex; align-items: flex-end }
                   .a { flex: 0 0 160px } .b { flex: 0 0 16px }";
        // The second item is five 1-cell words in a 2-cell box, so the line is
        // 5 rows and the one-row first item drops 4.
        assert_eq!(cross(row, css, 40), [(4, 1), (0, 5)]);

        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut texts = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text {
                texts.push((b.text.clone().unwrap_or_default(), b.dimensions.content.y));
            }
        });
        assert_eq!(
            texts,
            [
                ("hi".into(), 4),
                (" there".into(), 4),
                ("x".into(), 0),
                ("y".into(), 1),
                ("z".into(), 2),
                ("w".into(), 3),
                ("v".into(), 4),
            ]
        );
        // ...and it is on screen where the boxes say: the moved item's text is
        // on the row the alignment put it on, not on the row it was built at.
        assert_eq!(
            plain(&lines_styled(row, css, 40))[4],
            "hi there            v"
        );
    }

    #[test]
    fn a_cross_axis_length_no_layout_can_hold_lays_out_instead_of_crashing() {
        // The cross-axis half of the rule below, because this task added
        // arithmetic on a new axis: a line's cross size, an item's free space
        // in it, an auto margin's share of that. Every one of these
        // declarations is legal CSS a page may serve, and every one of them
        // has to come out as boxes rather than as an overflow panic.
        for css in [
            "#r { display: flex; height: 1e11em; align-items: flex-end }",
            "#r { display: flex; align-items: center } #r div { height: 1e11em }",
            "#r { display: flex; align-items: baseline } #r div { padding-top: 1e11em }",
            "#r { display: flex } #r div { margin-top: auto; margin-bottom: 1e11em }",
            "#r { display: flex; height: 1e11em } #r div { max-height: 1e11em }",
        ] {
            let (dom, styles) = styled_dom("<div id=r><div>a</div><div>b</div></div>", css);
            let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
            assert!(tree.height >= 0, "css: {css}");
        }
    }

    #[test]
    fn a_length_no_layout_can_hold_lays_out_instead_of_crashing() {
        // A stylesheet is untrusted input, and `gap: 1e11em` is a legal thing
        // for one to say. Layout has to answer it with boxes — principle §1.5:
        // what reaches the reader is a page, and a panic is not one. The
        // answer here is "everything after the first item is off the right
        // edge", which is what such a gap asks for.
        //
        // The cap that makes it safe lives in `Length::to_cells_*`, so the
        // same declaration on a plain block is covered by the same fix: these
        // three used to overflow at the same `+` inside inline layout.
        let row = "<div id=r><div>a</div><div>b</div></div>";
        let items = items(row, "#r { display: flex; gap: 1e11em }", 80);
        assert_eq!(items.len(), 2, "both items still got boxes");
        assert!(
            items[1].0 > 80,
            "the second item is off the line: {items:?}"
        );

        for css in [
            "p { margin-left: 1e11em }",
            "p { padding-left: 1e11em }",
            "p { width: 1e11em }",
        ] {
            let rows = lines_styled("<p>hello</p>", css, 80);
            assert!(!rows.is_empty(), "css: {css}");
        }
    }

    #[test]
    fn a_column_flex_container_still_stacks_until_m9_9() {
        // M9.7 implements both *row* directions. A column container keeps the
        // block layout it had before flex existed, which is far closer to what
        // a column means than laying it out sideways would be.
        // `engine::lays_out_as_flex` is the one predicate, and intrinsic sizing
        // asks it too.
        let markup = "<div id=r><div>one</div><div>two</div></div>";
        let block = geometry(markup, "#r { display: block }", 20);
        for css in [
            "#r { display: flex; flex-direction: column }",
            "#r { display: flex; flex-direction: column-reverse }",
        ] {
            assert_eq!(geometry(markup, css, 20), block, "css: {css}");
        }
        // ...and both row directions really do something different, so the
        // comparison above is not vacuous.
        assert_ne!(geometry(markup, "#r { display: flex }", 20), block);
        assert_ne!(
            geometry(
                markup,
                "#r { display: flex; flex-direction: row-reverse }",
                20
            ),
            block
        );
    }

    #[test]
    fn an_inline_flex_span_breaks_its_line_until_m9_11() {
        // The one place M9.5 does change what a page looks like, pinned so
        // M9.11 has to come here and say so. `inline-flex` is an inline-level
        // box with a flex inner mode; this engine takes the inner half and
        // makes the box block-level, so a `<span>` that a browser leaves on
        // the line gets a row of its own. No ladder fixture uses `inline-flex`,
        // which is exactly why this test exists rather than a snapshot.
        let src = "<p>before <span class=b>btn</span> after</p>";
        assert_eq!(
            plain(&lines_styled(src, "span.b { display: inline-block }", 40)),
            ["before btn after", ""]
        );
        assert_eq!(
            plain(&lines_styled(src, "span.b { display: inline-flex }", 40)),
            ["before", "btn", "after", ""],
            "when M9.11 gives inline-level boxes a line to sit on, this becomes \
             one row again"
        );
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
    fn a_page_of_only_images_counts_as_readable() {
        // The reveal fallback asks "does this tree show anything?". A page
        // whose whole body is one image has no text box at all — answering
        // from text alone would declare it blank and then "reveal" a page
        // that was never hidden.
        let dom = html::parse(r#"<body><img src="a.png" width="80" height="48" alt=""></body>"#);
        let styles = style::style_tree(&dom, &[]);
        let imgs = crate::image::discover(&dom, Some("https://ex/"));
        let mut cache = crate::image::ImageCache::default();
        let ctx = crate::image::ImageContext::from_discovery(&imgs, &mut cache);
        let (_tree, revealed) = layout_document_readable(&dom, &styles, 40, &ctx);
        assert!(!revealed, "an image-only page is not a blank page");
    }

    #[test]
    fn a_negative_height_is_ignored_not_a_collapsed_box() {
        // CSS 2.1 §10: the sizing properties are non-negative, so `-50px` is
        // an invalid declaration. Resolving it to zero instead would let a
        // page's typo erase its own content.
        let (dom, styles) = styled_dom(
            "<div>visible text</div>",
            "div { margin: 0; height: -50px; width: -10px }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let out = plain(&lines_from_tree(&tree));
        assert!(
            out.iter().any(|l| l.contains("visible text")),
            "content vanished: {out:?}"
        );
        let div = tree
            .get(tree.root)
            .children
            .first()
            .map(|&c| tree.get(c).dimensions.content)
            .expect("the div must still generate a box");
        assert_eq!(div.height, 1, "height fell back to content height");
        assert_eq!(div.width, 40, "width fell back to auto");
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
    fn a_clipped_away_row_does_not_extend_the_page() {
        // The other half of the rule above (M9.3): overflowing rows belong to
        // the scrollable page because they are *visible*. Clip them and the
        // page must shrink back, or a collapsed menu leaves the reader
        // scrolling through blank rows where its content would have been.
        let source = "<div class=zero>alpha<br>beta<br>gamma</div><div class=after>after</div>";
        let (dom, styles) = styled_dom(source, "div { margin: 0 } .zero { height: 0 }");
        let visible = layout_document(&dom, &styles, 40, Hidden::Respect);
        // Five rows: each `<br>` is a line box of its own between the words.
        assert_eq!(visible.height, 5, "the overflowing rows are on screen");

        let (dom, styles) = styled_dom(
            source,
            "div { margin: 0 } .zero { height: 0; overflow: hidden }",
        );
        let clipped = layout_document(&dom, &styles, 40, Hidden::Respect);
        assert_eq!(clipped.height, 1, "only `after` is left to scroll through");
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
