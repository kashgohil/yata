//! Layout: DOM + styles + width → box tree (PLAN.md M5).
//!
//! Pure transform. The box tree is the stage's real output; `layout` /
//! `layout_readable` also rasterise it to `Vec<Line>` so the viewport and
//! `--dump-text` keep working until the display-list paint path lands.

mod boxes;
mod clip;
mod dimensions;
mod engine;
pub(crate) mod field;
mod flex;
mod hit;
pub(crate) mod intrinsic;
mod lines;

pub use boxes::{BoxId, BoxKind, LayoutBox, LayoutTree};
pub use clip::Clip;
pub use dimensions::{Dimensions, EdgeSizes, Rect};
pub use engine::{Hidden, layout_tree, layout_tree_with, term_color, term_style};
pub use field::{FieldPaint, Shows};
pub use hit::{
    LinkHit, collect_links, dom_focusables, dom_links, first_y, focusables, hit_test, is_under,
    link_at, nearest_field, nearest_link, nearest_y, visible_links,
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
            // A form is content: a page that is only a login box must not be
            // laid out a second time with everything it hid revealed (M11.8).
            BoxKind::Image | BoxKind::Field(_) => true,
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
    use crate::layout::engine::is_atomic_inline;
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

    /// Every flex item's **content** box under the container with id `r`, as
    /// `(x, y, width, height)` in layout order. The whole box, because a column
    /// puts the interesting numbers on the other axis from a row's.
    fn item_boxes(html_src: &str, css: &str, width: u16) -> Vec<(i32, i32, i32, i32)> {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        let mut out = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                out = b
                    .children
                    .iter()
                    .map(|&c| {
                        let d = tree.get(c).dimensions.content;
                        (d.x, d.y, d.width, d.height)
                    })
                    .collect();
            }
        });
        out
    }

    /// The flex container's own content box — what its items' sizes had to add
    /// up to.
    fn flex_box(html_src: &str, css: &str, width: u16) -> (i32, i32, i32, i32) {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        let mut out = None;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex && out.is_none() {
                let d = b.dimensions.content;
                out = Some((d.x, d.y, d.width, d.height));
            }
        });
        out.expect("no flex container in the tree")
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
        // the container's height. M9.10 gave it something to sit between —
        // `a_wrapping_row_keeps_every_item_and_tiles_its_lines_at_every_width`
        // is where it does — and this row still has one line, so nothing here
        // moved.
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
    fn a_column_stacks_its_items_and_is_as_tall_as_them() {
        // M9.9 opens the gate `lays_out_as_flex` used to keep shut. The items
        // stack down the container's content box, each as wide as it (the
        // initial `align-items: stretch`), and the container is the sum of
        // their heights — which is why a column looks like ordinary block flow
        // until something else in flex is asked for.
        let col = "<div id=r><div>one</div><div>two three four</div></div>";
        let css = "#r { display: flex; flex-direction: column }";
        // At 10 cells "two three" is 9 and " four" would make 14, so the
        // second item is two lines tall.
        assert_eq!(item_boxes(col, css, 10), [(0, 0, 10, 1), (0, 1, 10, 2)]);
        assert_eq!(flex_box(col, css, 10), (0, 0, 10, 3));
    }

    #[test]
    fn flex_grow_does_nothing_in_an_auto_height_column() {
        // The result most likely to be "fixed" into a bug. A column's main size
        // is a height; with `height: auto` the container is exactly as tall as
        // its items, so the free space is *zero* and `flex-grow` has nothing to
        // distribute. `flex: 1` really does nothing here, and a browser agrees.
        let col = "<div id=r><div class=i>a</div><div class=i>b</div></div>";
        let column = "#r { display: flex; flex-direction: column } .i { flex: 1 }";
        assert_eq!(item_boxes(col, column, 20), [(0, 0, 20, 1), (0, 1, 20, 1)]);

        // Give the container a definite height and the same declaration splits
        // it exactly: 96px is 6 lines, 3 each, no row left over.
        let tall = format!("{column} #r {{ height: 96px }}");
        assert_eq!(item_boxes(col, &tall, 20), [(0, 0, 20, 3), (0, 3, 20, 3)]);

        // `min-height` is the other way to put free space back, and the reason
        // a column applies its own clamps before §9.7 rather than leaving them
        // to the clamp every block gets afterwards.
        let floor = format!("{column} #r {{ min-height: 96px }}");
        assert_eq!(item_boxes(col, &floor, 20), [(0, 0, 20, 3), (0, 3, 20, 3)]);

        // ...and `max-height` can make the free space negative on an otherwise
        // auto-height container. Both items are already at §4.5's automatic
        // minimum — the height of their own text — so they overflow the single
        // row the container was capped at rather than shrinking into it.
        let cap = format!("{column} #r {{ max-height: 16px }}");
        assert_eq!(item_boxes(col, &cap, 20), [(0, 0, 20, 1), (0, 1, 20, 1)]);
        assert_eq!(flex_box(col, &cap, 20), (0, 0, 20, 1));
    }

    #[test]
    fn a_column_main_axis_is_measured_in_lines_and_a_cross_axis_in_cells() {
        // The likeliest bug in an axis-generic rewrite is a crossed unit rule.
        // A width is 8px to the cell and takes its percentage from the
        // containing width; a height is 16px to the line and takes its
        // percentage from the containing *height*. The same declaration must
        // mean different numbers in the two directions.
        let one = "<div id=r><div class=i>a</div></div>";
        let column = "#r { display: flex; flex-direction: column } .i { flex: 0 0 64px }";
        assert_eq!(
            item_boxes(one, column, 20),
            [(0, 0, 20, 4)],
            "64px is 4 lines"
        );
        let row = "#r { display: flex } .i { flex: 0 0 64px }";
        assert_eq!(item_boxes(one, row, 20), [(0, 0, 8, 1)], "64px is 8 cells");

        // A percentage basis on a column's main axis is a percentage of the
        // container's height: half of 160px = 10 lines is 5.
        let pct = "#r { display: flex; flex-direction: column; height: 160px }
                   .i { flex: 0 0 50% }";
        assert_eq!(item_boxes(one, pct, 20), [(0, 0, 20, 5)]);

        // Percentage *padding* does not follow: it resolves against the
        // containing block's width on both axes (CSS 2.1 §8.1), so 10% of a
        // 20-cell column is 2 lines of `padding-top` — not 1, which is what
        // 10% of the container's 10-line height would have been. The second
        // item's y is the number that tells them apart.
        let two = "<div id=r><div class=i>a</div><div>b</div></div>";
        let pad = "#r { display: flex; flex-direction: column; height: 160px }
                   .i { padding-top: 10% }";
        assert_eq!(item_boxes(two, pad, 20), [(0, 2, 20, 1), (0, 3, 20, 1)]);
    }

    #[test]
    fn align_items_baseline_degrades_to_flex_start_in_a_column() {
        // A baseline is a *row* in a cell grid, and a column's cross axis is
        // the horizontal one — there is no shared row to stitch items to, so
        // the value has nothing to do. This is a degradation rather than an
        // implementation, and it is pinned so that it stays a decision.
        let col = "<div id=r><div>aaaa</div><div>bb</div></div>";
        let base = "#r { display: flex; flex-direction: column; align-items: baseline }";
        let start = "#r { display: flex; flex-direction: column; align-items: flex-start }";
        assert_eq!(geometry(col, base, 20), geometry(col, start, 20));
        // Not vacuous: neither of those is `stretch`, the initial value, which
        // would have made both items 20 cells wide.
        assert_eq!(
            item_boxes(col, base, 20),
            [(0, 0, 4, 1), (0, 1, 2, 1)],
            "a non-stretching column item shrink-wraps its content"
        );
        let stretch = "#r { display: flex; flex-direction: column }";
        assert_eq!(item_boxes(col, stretch, 20), [(0, 0, 20, 1), (0, 1, 20, 1)]);
    }

    #[test]
    fn auto_margins_take_a_columns_free_space_on_both_axes() {
        // §9.5 step 1 and §9.6 step 1, with the axes swapped: an `auto` margin
        // absorbs the free space before any alignment is consulted. On a
        // column's main axis that is `margin-top`/`margin-bottom` and it pushes
        // an item down the page; on its cross axis it is the horizontal pair
        // and it centres the item across the width.
        let one = "<div id=r><div class=i>x</div></div>";
        let pushed = "#r { display: flex; flex-direction: column; height: 96px }
                      .i { margin-top: auto }";
        let by_alignment = "#r { display: flex; flex-direction: column; height: 96px;
                            justify-content: flex-end }";
        // Nothing here is compared against a number from the implementation:
        // the two ways of saying "put it at the bottom" have to agree.
        assert_eq!(geometry(one, pushed, 20), geometry(one, by_alignment, 20));
        assert_eq!(item_boxes(one, pushed, 20), [(0, 5, 20, 1)]);

        let auto_sides = "#r { display: flex; flex-direction: column }
                          .i { margin-left: auto; margin-right: auto }";
        let centred = "#r { display: flex; flex-direction: column; align-items: center }";
        assert_eq!(
            item_boxes(one, auto_sides, 20),
            item_boxes(one, centred, 20)
        );
        // An auto cross margin stops the item stretching, so it shrink-wraps to
        // its one cell of text and the other 19 go to the margins: 10 before,
        // 9 after, the same "earliest slot first" rule as everywhere else.
        assert_eq!(item_boxes(one, auto_sides, 20), [(10, 0, 1, 1)]);
    }

    #[test]
    fn order_reorders_a_column_the_way_it_reorders_a_row() {
        // A column's boxes are built in document order — building one never
        // depends on its neighbours — and sorted into order-modified document
        // order before the line is placed. The DOM is untouched either way.
        let col = "<div id=r><div class=a>a</div><div class=b>b</div></div>";
        let css = "#r { display: flex; flex-direction: column } .a { order: 1 }";
        assert_eq!(plain(&lines_styled(col, css, 20)), ["b", "a"]);
        let reversed = "#r { display: flex; flex-direction: column-reverse }";
        assert_eq!(plain(&lines_styled(col, reversed, 20)), ["b", "a"]);
    }

    #[test]
    fn moving_a_column_item_moves_everything_inside_it() {
        // The column counterpart of
        // `alignment_moves_everything_inside_the_item_it_moves`. A row can
        // place an item before building it; a column cannot, because an item's
        // main size is a height and nothing measures one — so every item is
        // built against the container's main-start edge and then moved. This is
        // the test that says the move takes the whole subtree with it, which is
        // where the classic flex bug (a box that moved and left its text
        // behind) would live if it lived anywhere.
        let col = "<div id=r><div class=i><b>hi</b> there</div></div>";
        let css = "#r { display: flex; flex-direction: column; height: 80px;
                   justify-content: center }";
        // One 1-line item in 5 lines: 4 free, 2 of them above it.
        assert_eq!(item_boxes(col, css, 20), [(0, 2, 20, 1)]);

        let (dom, styles) = styled_dom(col, css);
        let tree = layout_document(&dom, &styles, 20, Hidden::Respect);
        let mut texts = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text {
                texts.push((b.text.clone().unwrap_or_default(), b.dimensions.content.y));
            }
        });
        assert!(!texts.is_empty(), "the item has text to move");
        for (text, y) in &texts {
            assert_eq!(*y, 2, "{text:?} was left behind at row {y}");
        }
    }

    #[test]
    fn replaced_and_rule_items_keep_their_own_paths_in_a_column() {
        // `<img>`, `<br>` and `<hr>` size themselves from the width they are
        // handed rather than being re-derived as block containers — the same
        // exception a row makes, and for the same reason. On a column's cross
        // axis it matters twice over: `align-items` is `stretch` by default,
        // and stretching an image would rescale the picture to the container's
        // width instead of leaving it at its own.
        let col = "<div id=r><img src=logo.png width=16 height=32><div>after</div></div>";
        let (dom, styles) = styled_dom(col, "#r { display: flex; flex-direction: column }");
        let imgs = crate::image::discover(&dom, Some("https://fixture.test/page"));
        let ctx = ImageContext::from_discovery(&imgs, &mut crate::image::ImageCache::default());
        let tree = layout_document_with(&dom, &styles, 20, Hidden::Respect, &ctx);
        let mut boxes = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Flex {
                boxes = b
                    .children
                    .iter()
                    .map(|&c| {
                        let d = tree.get(c).dimensions.content;
                        (d.x, d.y, d.width, d.height)
                    })
                    .collect();
            }
        });
        // 16 × 32 px is 2 cells by 2 lines. The image keeps both; only the
        // block below it fills the 20-cell width.
        assert_eq!(boxes, [(0, 0, 2, 2), (0, 2, 20, 1)]);

        // An `<hr>` does stretch — it is a rule across the box it was given —
        // and its `margin: 1em 0` from ua.css is a main-axis margin here, so
        // the container is 1 + 1 + 1 lines tall around a 1-line rule.
        let rule = "<div id=r><hr></div>";
        let css = "#r { display: flex; flex-direction: column }";
        assert_eq!(item_boxes(rule, css, 20), [(0, 1, 20, 1)]);
        assert_eq!(flex_box(rule, css, 20), (0, 0, 20, 3));
    }

    #[test]
    fn flex_basis_content_asks_a_column_item_how_tall_its_content_is() {
        // §9.2 step 3: `content` is the keyword that overrides the main size
        // property, so it has to mean the height the item's *content* used and
        // not the height the item ended up with. Those differ exactly when the
        // page states a `height`, which is the case here — and it is why
        // building a column item hands back both numbers.
        let col = "<div id=r><div class=i>a</div></div>";
        let basis = "#r { display: flex; flex-direction: column }
                     .i { flex-basis: content; height: 48px }";
        assert_eq!(item_boxes(col, basis, 10), [(0, 0, 10, 1)]);
        // `flex-basis: auto` defers to `height` instead, and 48px is 3 lines.
        let auto = "#r { display: flex; flex-direction: column } .i { height: 48px }";
        assert_eq!(item_boxes(col, auto, 10), [(0, 0, 10, 3)]);
    }
    #[test]
    fn a_column_item_is_built_exactly_once() {
        // The constraint the whole column path is shaped around. Measuring an
        // item means building it, so the obvious implementation — build to
        // measure, then rebuild at the size §9.7 resolved — doubles the work at
        // every level and is exponential in the nesting depth. §9.7's answer is
        // applied as a field write instead, which is equivalent because content
        // layout here depends on the width a box was given and on nothing else.
        //
        // A rebuild would leave the discarded boxes in the arena, so the tree
        // of a six-deep column nest has to hold exactly as many boxes as the
        // identical block-flow nest — 2^6 times as many, if it did not.
        let mut markup = String::from("<div>leaf</div>");
        for _ in 0..6 {
            markup = format!("<div class=c>{markup}</div>");
        }
        let block = geometry(&markup, ".c { display: block }", 20).len();
        for direction in ["column", "column-reverse"] {
            let css = format!(".c {{ display: flex; flex-direction: {direction} }}");
            assert_eq!(geometry(&markup, &css, 20).len(), block, "{direction}");
        }
    }

    #[test]
    fn a_percentage_height_inside_a_flexed_column_item_stays_indefinite() {
        // **Deferred, and on the record.** §9.7 gives this item a definite main
        // size of 3 lines, but its box was built in order to *measure* that
        // size, so the `height: 100%` inside it resolved against nothing and
        // stayed at its content height. Making it definite means laying the
        // item out a second time, which is the one thing the column path is
        // written to avoid; `stretch_item`'s doc used to promise M9.9 would
        // bring this and now says otherwise.
        let col = "<div id=r><div class=i><div class=fill>x</div></div></div>";
        let css = "#r { display: flex; flex-direction: column; height: 48px }
                   .i { flex: 1 } .fill { height: 100% }";
        assert_eq!(item_boxes(col, css, 20), [(0, 0, 20, 3)], "the item grew");

        let (dom, styles) = styled_dom(col, css);
        let tree = layout_document(&dom, &styles, 20, Hidden::Respect);
        let mut blocks = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Block && b.node.is_some() {
                blocks.push(b.dimensions.content.height);
            }
        });
        assert_eq!(
            blocks.last(),
            Some(&1),
            "the `height: 100%` child is still its content's 1 line"
        );
    }

    // ---- M9.10 wrapping and `align-content` ---------------------------------

    #[test]
    fn a_wrapping_row_keeps_every_item_and_tiles_its_lines_at_every_width() {
        // The invariant sweep for wrapping, over the whole stage. Whatever the
        // terminal width: every item still has a box, no line is empty, items
        // on a line keep their order and their gap, and the container is
        // exactly as tall as its lines and the gaps between them — a cross axis
        // that leaked a row would show up here as a container an inch taller
        // than its own content on some widths and not others.
        let markup = "<div id=r><div>alpha</div><div>beta</div><div>gamma</div>\
                      <div>delta</div><div>epsilon</div><div>zeta</div></div>";
        let css = "#r { display: flex; flex-wrap: wrap; gap: 1em }
                   #r div { flex: 0 0 160px }";
        for width in 20..=120u16 {
            let (dom, styles) = styled_dom(markup, css);
            let tree = layout_document(&dom, &styles, width, Hidden::Respect);
            let mut container = 0;
            let mut boxes = Vec::new();
            tree.walk(tree.root, &mut |_, b| {
                if b.kind == BoxKind::Flex {
                    container = b.dimensions.content.height;
                    boxes = b
                        .children
                        .iter()
                        .map(|&c| tree.get(c).dimensions.margin_box())
                        .collect();
                }
            });
            let label = format!("width {width}: {boxes:?}");
            assert_eq!(boxes.len(), 6, "{label}: an item went missing");
            // Group the items into lines by the row they were placed on, in
            // layout order — which is the order the algorithm collected them.
            let mut rows: Vec<Vec<Rect>> = Vec::new();
            for b in &boxes {
                match rows.last_mut() {
                    Some(row) if row[0].y == b.y => row.push(*b),
                    _ => rows.push(vec![*b]),
                }
            }
            let mut expected_y = 0;
            for (idx, row) in rows.iter().enumerate() {
                // `gap: 1em` is both gaps: 2 cells between items on a line, and
                // one *row* between the lines themselves.
                if idx > 0 {
                    expected_y += 1;
                }
                assert_eq!(row[0].y, expected_y, "{label}: line {idx} is misplaced");
                for pair in row.windows(2) {
                    assert!(pair[1].x - pair[0].right() >= 2, "{label}");
                }
                expected_y += row.iter().map(|b| b.height).max().unwrap_or(0);
            }
            // No line is empty (every group came from an item), and the
            // container is exactly the lines it holds.
            assert_eq!(container, expected_y, "{label}: lines do not tile");
        }
    }

    #[test]
    fn wrapping_moves_everything_inside_the_item_it_moves() {
        // The wrapping counterpart of
        // `moving_a_column_item_moves_everything_inside_it`, and the case M9.10
        // added: a wrapping column places its items *across* after building
        // them, so a subtree now moves sideways as well as down. The classic
        // flex bug — a box that moved and left its text behind — would live
        // here if it lived anywhere.
        let col = "<div id=r><div>one</div><div>two</div><div><b>hi</b> there</div></div>";
        let css = "#r { display: flex; flex-direction: column; flex-wrap: wrap;
                   height: 32px; align-content: flex-start }";
        // Two 1-line items fill the 2-line container, so the third starts a
        // second column at x = 3 — the width of the first column, which is the
        // widest item on it ("one" and "two" are 3 cells each).
        assert_eq!(
            item_boxes(col, css, 40),
            [(0, 0, 3, 1), (0, 1, 3, 1), (3, 0, 8, 1)]
        );

        let (dom, styles) = styled_dom(col, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut texts = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text {
                texts.push((
                    b.text.clone().unwrap_or_default(),
                    b.dimensions.content.x,
                    b.dimensions.content.y,
                ));
            }
        });
        // "hi" and "there" are inside the item that moved, and both went with
        // it: they are on the second column's row 0, not back at x = 0.
        let moved: Vec<_> = texts.iter().filter(|(_, x, _)| *x >= 3).collect();
        assert_eq!(moved.len(), 2, "the second column's text: {texts:?}");
        for (text, x, y) in moved {
            assert!(*x >= 3 && *y == 0, "{text:?} left behind at {x},{y}");
        }
        // ...and it is really on screen there: row 0 reads across both columns.
        assert_eq!(
            plain(&lines_styled(col, css, 40))[0].trim_end(),
            "onehi there"
        );
    }

    #[test]
    fn no_reversed_axis_ever_puts_a_box_before_its_container() {
        // Found reviewing M9.10, fixed across all three properties that reverse
        // an axis. A reversed axis starts at the far edge, so offsets are
        // subtracted — and subtracting an overflowing line's offsets from the
        // container's own edge lands boxes at negative rows and columns. A
        // terminal has no row above 0 and no column left of it: that content is
        // not clipped, it is unreachable, and every stage downstream of layout
        // then has to be careful with a coordinate that should never have
        // existed. `flex-justify-overflow`'s row 3 used to record x = -40.
        //
        // The rule is one line in `from_far_edge`: count back from the far edge
        // of the *content* when the content overflows, which is the container's
        // own edge whenever it fits. This sweep is the rule stated as a
        // property — every direction, every wrap, every width, overflowing on
        // both axes at once.
        let markup = "<div id=r><div>alpha</div><div>beta</div><div>gamma</div>\
                      <div>delta</div></div>";
        for direction in ["row", "row-reverse", "column", "column-reverse"] {
            for wrap in ["nowrap", "wrap", "wrap-reverse"] {
                let css = format!(
                    "#r {{ display: flex; flex-direction: {direction}; flex-wrap: {wrap};
                       height: 2em }}
                     #r div {{ flex: 0 0 240px; height: 2em }}"
                );
                for width in 10..=100u16 {
                    let (dom, styles) = styled_dom(markup, &css);
                    let tree = layout_document(&dom, &styles, width, Hidden::Respect);
                    let mut checked = false;
                    tree.walk(tree.root, &mut |_, b| {
                        if b.kind != BoxKind::Flex {
                            return;
                        }
                        checked = true;
                        let origin = b.dimensions.content;
                        for &c in &b.children {
                            let mb = tree.get(c).dimensions.margin_box();
                            let label =
                                format!("{direction} {wrap} at {width}: {mb:?} in {origin:?}");
                            assert!(mb.x >= origin.x, "{label}: before the left edge");
                            assert!(mb.y >= origin.y, "{label}: above the top edge");
                        }
                    });
                    assert!(checked, "{direction} {wrap} at {width}: no container");
                }
            }
        }
    }

    #[test]
    fn min_height_is_a_definite_cross_size_for_a_row() {
        // Found reviewing M9.10. A container's clamps are part of how much room
        // it has, so a stage that divides its cross axis has to apply them —
        // which is the rule M9.9 already applies to a column's *main* axis, for
        // exactly this reason. Left out, `min-height` bought rows that nothing
        // was allowed to use: the line was as tall as its tallest item and the
        // rest of the container was blank.
        let row = "<div id=r><div>a</div></div>";
        let css = "#r { display: flex; min-height: 6em; align-items: center }
                   #r div { flex: 0 0 160px }";
        // 5 rows to place a 1-row item in, and the odd row goes above it — the
        // same rule every other split in this engine follows.
        assert_eq!(cross(row, css, 40), [(3, 1)]);
        // ...and the container really is 6 rows, as it always was: the clamp
        // `layout_box_at` applies afterwards now re-applies an already-clamped
        // value, which is the point of doing it here.
        assert_eq!(flex_box(row, css, 40).3, 6);

        // `stretch` — the initial value — fills those rows rather than leaving
        // them blank.
        let stretch = "#r { display: flex; min-height: 6em } #r div { flex: 0 0 160px }";
        assert_eq!(cross(row, stretch, 40), [(0, 6)]);

        // `max-height` is the same rule from the other end: a line shorter than
        // its content, with the overflow packed at cross-start.
        let capped = "#r { display: flex; max-height: 1em; align-items: center }
                      #r div { flex: 0 0 160px; height: 4em }";
        assert_eq!(cross(row, capped, 40), [(0, 4)]);

        // And on the wrapping path it is what gives `align-content` anything to
        // do: two lines of one row in six, centred, sit at 2 and 3.
        let two = "<div id=r><div>a</div><div>b</div></div>";
        let wrapped = "#r { display: flex; flex-wrap: wrap; min-height: 6em;
                       align-content: center }
                       #r div { flex: 0 0 640px }";
        assert_eq!(cross(two, wrapped, 80), [(2, 1), (3, 1)]);
    }

    #[test]
    fn a_rule_widens_a_wrapping_columns_line() {
        // **A divergence on the record**, found reviewing M9.10 and left in
        // deliberately. §9.4 step 8 sizes a line from its items' hypothetical
        // cross sizes, and a rule with no content asks for nothing — so a
        // browser lets the other items decide this column's width and stretches
        // the `<hr>` into it. Here the rule asks for the whole content box, and
        // the column comes out 40 cells wide instead of 2.
        //
        // The reason is not flex: `layout_hr` builds the rule's glyphs from the
        // width it is given, and a column item is built before its line exists,
        // so a rule asking for 0 would be a correctly sized box with no rule in
        // it — worse than a column that is too wide. This test is here so that
        // whoever makes a rule re-sizable finds the flex half of the bill.
        let col = "<div id=r><div>aa</div><hr><div>bb</div></div>";
        let css = "#r { display: flex; flex-direction: column; flex-wrap: wrap;
                   height: 32px; align-content: flex-start } hr { margin: 0 }";
        assert_eq!(
            item_boxes(col, css, 40),
            [(0, 0, 40, 1), (0, 1, 40, 1), (40, 0, 2, 1)],
            "the rule decided the first column's width, pushing the second one \
             off the container's right edge"
        );
        // The rule really is drawn, which is the half that must not regress: a
        // column too wide is a layout bug, a rule that vanished is a hole.
        assert_eq!(
            plain(&lines_styled(col, css, 40))[1].trim_end(),
            "─".repeat(40)
        );

        // Without the rule the same three items give the columns their own
        // widths, which is what the fix would restore.
        let plain_col = "<div id=r><div>aa</div><div>x</div><div>bb</div></div>";
        assert_eq!(
            item_boxes(plain_col, css, 40),
            [(0, 0, 2, 1), (0, 1, 2, 1), (2, 0, 2, 1)]
        );
    }

    #[test]
    fn a_wrapped_link_hit_tests_where_it_was_drawn() {
        // The rest of the browser has to agree with the wrap. Two 20-cell links
        // in a 30-cell container: the second one wraps to row 1, and `/2` must
        // answer there rather than where an unwrapped row would have put it.
        let row = "<div id=r><a href=/1>one</a><a href=/2>two</a></div>";
        let css = "#r { display: flex; flex-wrap: wrap } a { flex: 0 0 160px }";
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 30, Hidden::Respect);
        assert_eq!(
            hit::link_at(&tree, &dom, 1, 0).map(|(_, u)| u),
            Some("/1".into())
        );
        assert_eq!(
            hit::link_at(&tree, &dom, 1, 1).map(|(_, u)| u),
            Some("/2".into())
        );
        // The DOM is untouched, as it is for `order` and the reversed
        // directions: wrapping is a layout instruction, not an edit.
        assert_eq!(
            hit::dom_links(&dom)
                .iter()
                .map(|(_, u)| u.as_str())
                .collect::<Vec<_>>(),
            ["/1", "/2"]
        );
    }

    #[test]
    fn baseline_items_still_share_a_row_under_wrap_reverse() {
        // `wrap-reverse` swaps cross-start and cross-end, and the one value
        // that cannot simply be reflected is `baseline`: reflecting each item
        // inside its line would align them by their heights instead of by their
        // text, which is the one thing the value exists to prevent. So a
        // baseline is measured from the item's *cross-start* edge — the bottom,
        // here — and the group ends up flush with the bottom of the line with
        // its baselines still on one row.
        let row = "<div id=r><div class=pad>a</div><div>b</div></div>";
        let css = "#r { display: flex; flex-wrap: wrap-reverse; align-items: baseline }
                   #r div { flex: 0 0 80px } .pad { padding-top: 16px }";
        let (dom, styles) = styled_dom(row, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let mut rows = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text {
                rows.push((b.text.clone().unwrap_or_default(), b.dimensions.content.y));
            }
        });
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].1, rows[1].1, "the baselines split up: {rows:?}");
        // ...and the same fixture the other way up puts them on the same row
        // too, one row higher, because the padded item is the taller one and a
        // reversed line hangs from its far edge.
        let upright = css.replace("wrap-reverse", "wrap");
        assert_eq!(cross(row, &upright, 40), cross(row, css, 40));
    }

    #[test]
    fn an_inline_level_box_stays_on_the_line_beside_its_text() {
        // M9.5 left this test failing on purpose: `inline-flex` cascaded to
        // `Flex`, which is block-level, so a badge a browser leaves in the
        // sentence got a row of its own. M9.11 is where the *outside* half of
        // the keyword starts being read, and both inline-level modes come back
        // onto the line — an `inline-block` that used to flow its contents into
        // the sentence as words, and an `inline-flex` that used to break it.
        let src = "<p>before <span class=b>btn</span> after</p>";
        assert_eq!(
            plain(&lines_styled(src, "span.b { display: inline-block }", 40)),
            ["before btn after", ""]
        );
        assert_eq!(
            plain(&lines_styled(src, "span.b { display: inline-flex }", 40)),
            ["before btn after", ""]
        );
    }

    /// The used content width of the one atomic inline in this document, laid
    /// out in a column `width` cells wide.
    fn atomic_width(html_src: &str, css: &str, width: u16) -> i32 {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        let mut found = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.node
                .is_some_and(|n| is_atomic_inline(styles.get(n).display))
            {
                found.push(b.dimensions.content.width);
            }
        });
        assert_eq!(found.len(), 1, "expected exactly one atomic inline");
        found[0]
    }

    #[test]
    fn shrink_to_fit_stops_at_max_content_and_never_goes_under_min_content() {
        // CSS 2.1 §10.3.9, as a property rather than three numbers: whatever
        // the line has left, the box is somewhere between the width its
        // content cannot go under and the width it would take unwrapped. The
        // interesting half is the *upper* bound — a box that filled the line
        // like a block is what `inline-block` looked like before M9.11, and it
        // is the bound a naive "use the available width" would break.
        let src = "<p>x <span class=b>alpha beta gamma</span></p>";
        let css = "span.b { display: inline-block }";
        // "gamma" is the widest word it cannot break, "alpha beta gamma" is the
        // whole of it on one line.
        let (min_content, max_content) = (5, 16);
        for width in 8..40u16 {
            let used = atomic_width(src, css, width);
            assert!(
                (min_content..=max_content).contains(&used),
                "in {width} cells the box came out {used} wide"
            );
            // "x " takes two of them, and the box takes what is left until
            // there is more than enough.
            let available = width as i32 - 2;
            let want = available.clamp(min_content, max_content);
            assert_eq!(used, want, "in {width} cells");
        }
    }

    #[test]
    fn an_atomic_inline_that_does_not_fit_moves_to_the_next_line_whole() {
        // The line breaker's rule for a box: it fits, or the line breaks
        // before it. Never "part of it fits" — a box has no interior break
        // opportunity a line may use, however much text is inside it.
        let src = "<p>lead <span class=b>alpha beta</span></p>";
        let css = "span.b { display: inline-block; width: 8em }";
        // 16 cells of box after "lead " needs 21; a 20-cell column has to break.
        let rows = plain(&lines_styled(src, css, 20));
        assert_eq!(rows[0].trim_end(), "lead", "{rows:?}");
        assert_eq!(rows[1].trim_end(), "alpha beta", "{rows:?}");
        // One box for the element on the second row, not two fragments split
        // across the break.
        let (dom, styles) = styled_dom(src, css);
        let tree = layout_document(&dom, &styles, 20, Hidden::Respect);
        let mut boxes = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.node
                .is_some_and(|n| is_atomic_inline(styles.get(n).display))
            {
                boxes.push(b.dimensions.content);
            }
        });
        assert_eq!(boxes.len(), 1, "the box was split: {boxes:?}");
        assert_eq!((boxes[0].x, boxes[0].y, boxes[0].width), (0, 1, 16));
    }

    #[test]
    fn text_beside_a_tall_atomic_inline_sits_on_its_last_baseline() {
        // CSS 2.1 §10.8.1. A three-row box whose last line is its third means
        // the sentence beside it is level with that third row — not with the
        // top of the box, which is what "align them by their boxes" would do
        // and is the thing baselines exist to prevent.
        // A 2-cell box takes one word per row, so "a b c" is three rows. The
        // sentence resumes past the box's full 2 cells — the second of which
        // its one-cell last line does not use — because what the line advances
        // by is the box, not the text in it.
        let src = "<p>tag <span class=b>a b c</span> end</p>";
        let css = "span.b { display: inline-block; width: 1em }";
        let rows = plain(&lines_styled(src, css, 40));
        assert_eq!(
            rows.iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            ["    a", "    b", "tag c  end", ""],
            "{rows:?}"
        );
    }

    /// Every box in this document that was generated by an atomic inline,
    /// as `(border box x, y, width, height)`.
    fn atomic_boxes(html_src: &str, css: &str, width: u16) -> Vec<(i32, i32, i32, i32)> {
        let (dom, styles) = styled_dom(html_src, css);
        let tree = layout_document(&dom, &styles, width, Hidden::Respect);
        let mut out = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.node
                .is_some_and(|n| is_atomic_inline(styles.get(n).display))
            {
                let r = b.dimensions.border_box();
                out.push((r.x, r.y, r.width, r.height));
            }
        });
        out
    }

    #[test]
    fn an_atomic_inline_is_shifted_by_text_align_with_the_rest_of_its_line() {
        // A box on a line is a piece of that line and nothing more special:
        // whatever moves the words moves it too. Centring 6 cells of content in
        // 20 leaves 7 either side, so the sentence starts at 7 and the box —
        // "x" with a cell of padding — sits at 10.
        let src = "<p class=c>hi <span class=b>x</span></p>";
        let css = "p.c { text-align: center } span.b { display: inline-block; padding: 0 8px }";
        assert_eq!(atomic_boxes(src, css, 20), [(10, 0, 3, 1)]);
        let rows = plain(&lines_styled(src, css, 20));
        assert_eq!(rows[0].trim_end(), "       hi  x", "{rows:?}");
    }

    #[test]
    fn an_atomic_inline_inside_pre_is_a_box_on_the_preformatted_line() {
        // `<pre>` runs its own line breaker (nothing collapses, nothing wraps),
        // so an atomic inline there needs placing by that one too. The failure
        // this pins is not a misplaced box but a missing one: a `<pre>` that
        // silently dropped the box would still look like a plausible page.
        let src = "<pre>a <span class=b>x</span> b</pre>";
        let css = "span.b { display: inline-block; padding: 0 8px }";
        assert_eq!(atomic_boxes(src, css, 20), [(2, 0, 3, 1)]);
        let rows = plain(&lines_styled(src, css, 20));
        assert_eq!(rows[0].trim_end(), "a  x  b", "{rows:?}");
    }

    #[test]
    fn an_hr_that_cannot_draw_a_rule_still_costs_the_line_only_its_own_box() {
        // The one element whose layout lives in a function the atomic path does
        // not call. `<hr>` inside an inline is markup the parser normally
        // breaks up, and where it survives it becomes an empty box: no rule,
        // but no lost text either, and the rows it takes are its own 1em UA
        // margins rather than a row for a rule that was never drawn.
        let src = "<div>a <span><hr class=b></span> b</div>";
        let css = "span .b { display: inline-block }";
        assert_eq!(atomic_boxes(src, css, 20), [(2, 1, 0, 0)]);
        let rows = plain(&lines_styled(src, css, 20));
        assert_eq!(
            rows.iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            // Two spaces: the box is zero cells wide, and the collapsible
            // space either side of it is still a space.
            ["", "", "a  b"],
            "the text either side of it must survive: {rows:?}"
        );
    }

    #[test]
    fn a_link_inside_an_inline_block_is_clickable_and_searchable() {
        // Hit-testing, link hints and `/` search all walk the layout tree, and
        // an atomic inline puts a whole subtree of boxes under a *line* box for
        // the first time. Nothing about that walk changed, which is exactly why
        // it is worth a test rather than an assumption.
        let src = "<p>see <span class=b><a href='/x'>doc</a></span> now</p>";
        let css = "span.b { display: inline-block; padding: 0 8px }";
        let (dom, styles) = styled_dom(src, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);

        let hits = crate::browser::search::find_matches(&tree, "doc");
        assert_eq!(hits.len(), 1, "the search missed the box: {hits:?}");
        let hit = hits[0].clone();
        assert_eq!(
            link_at(&tree, &dom, hit.x, hit.y).map(|(_, href)| href),
            Some("/x".to_string()),
            "a click on the link's own cells missed it"
        );
        // ...and the link is reachable by Tab / link hints, with the position
        // the hint is drawn at inside the box.
        let links = collect_links(&tree, &dom);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!((links[0].x, links[0].y), (hit.x, hit.y));
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

    /// M9.12: the inputs a real page produces by accident.
    ///
    /// CLAUDE.md is explicit that a panic is a bug, and every one of these is
    /// a shape the flex algorithm can divide by, index into, or recurse
    /// through: a container with nothing to distribute free space to, a gap
    /// larger than the space it divides, a minimum that cannot be satisfied.
    /// Each case asserts the same two things — layout returns, and every box
    /// it produced has a non-negative size — because "what the right boxes
    /// are" is the *other* tests' job and pinning some particular wrong-looking
    /// answer here would just make the file hard to change.
    mod degenerate_flex {
        use super::*;

        /// Every box's content size, which must never go negative: a negative
        /// width is how a subtraction that should have been clamped shows up
        /// three stages later, in paint, as a panic.
        fn sizes(html_src: &str, css: &str, width: u16) -> Vec<(i32, i32)> {
            let (dom, styles) = styled_dom(html_src, css);
            let tree = layout_document(&dom, &styles, width, Hidden::Respect);
            let mut out = Vec::new();
            tree.walk(tree.root, &mut |_, b| {
                let d = b.dimensions.content;
                out.push((d.width, d.height));
            });
            assert!(
                out.iter().all(|&(w, h)| w >= 0 && h >= 0),
                "negative box size: {out:?}"
            );
            out
        }

        const FLEX: &str = ".r { display: flex } body, div, p { margin: 0 }";

        /// [`FLEX`] plus one more rule — the shape every case below wants, and
        /// the reason none of them repeats the container declaration.
        fn css(extra: &str) -> String {
            format!("{FLEX} {extra}")
        }

        #[test]
        fn a_container_with_no_children_lays_out() {
            assert!(!sizes("<div class=r></div>", FLEX, 20).is_empty());
        }

        #[test]
        fn a_container_of_only_whitespace_lays_out() {
            // Whitespace between flex items is trimmed away (M9.6), so this is
            // the case where every item the algorithm generated then measured
            // to nothing.
            assert!(!sizes("<div class=r>   \n  </div>", FLEX, 20).is_empty());
        }

        #[test]
        fn a_display_none_child_is_not_an_item() {
            let css = ".r { display: flex } .gone { display: none } div { margin: 0 }";
            let boxes = item_boxes(
                "<div class=r><div class=gone>x</div><div>y</div></div>",
                css,
                20,
            );
            assert_eq!(boxes.len(), 1, "{boxes:?}");
        }

        #[test]
        fn a_zero_size_item_lays_out() {
            let _ = sizes(
                "<div class=r><div class=z></div><div>y</div></div>",
                &css(".z { width: 0; height: 0 }"),
                20,
            );
        }

        #[test]
        fn a_container_one_cell_wide_lays_out() {
            let _ = sizes(
                "<div class=r><div>alpha</div><div>bravo</div></div>",
                FLEX,
                1,
            );
        }

        #[test]
        fn a_zero_height_clipped_container_lays_out() {
            let _ = sizes(
                "<div class=r><div>alpha</div><div>bravo</div></div>",
                &css(".r { height: 0; overflow: hidden }"),
                20,
            );
        }

        #[test]
        fn a_gap_wider_than_the_container_lays_out() {
            // Free space goes negative before a single item has been placed —
            // the shrink pass divides by a total that must not be zero and must
            // not be signed the way it expects.
            let _ = sizes(
                "<div class=r><div>alpha</div><div>bravo</div></div>",
                &css(".r { gap: 800px }"),
                20,
            );
        }

        #[test]
        fn an_unsatisfiable_min_width_lays_out() {
            let _ = sizes(
                "<div class=r><div>alpha</div><div>bravo</div></div>",
                &css(".r div { min-width: 800px }"),
                20,
            );
        }

        #[test]
        fn wrapping_an_item_wider_than_the_line_lays_out() {
            // §9.3 step 3: an item that does not fit still starts a line, or
            // the loop that fills lines never advances.
            let boxes = item_boxes(
                "<div class=r><div>alpha</div><div>bravo</div></div>",
                &css(".r { flex-wrap: wrap } .r div { flex: 0 0 800px }"),
                20,
            );
            assert_eq!(boxes.len(), 2, "an item was dropped: {boxes:?}");
        }

        #[test]
        fn ten_levels_of_nesting_lay_out() {
            let mut src = String::new();
            for _ in 0..10 {
                src.push_str("<div class=r>");
            }
            src.push_str("deep");
            for _ in 0..10 {
                src.push_str("</div>");
            }
            let out = sizes(&src, FLEX, 20);
            assert!(out.len() >= 10, "{out:?}");
        }
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

    // ---- M11.8: form controls, and the four things they must never do ------

    /// The five lines from the task's own Context, whose `--dump-text` showed
    /// three separate faults: an `<input>` with no box at all, a `<textarea>`
    /// whose value leaked into the page as whitespace-collapsed prose, and a
    /// `<button>` label running straight into it.
    const CONTEXT_FIXTURE: &str = "<p>before</p><form><input type=\"text\" name=\"q\" \
         size=\"17\" value=\"typed\">\n<textarea rows=3 cols=20>hello\ntextarea</textarea>\
         <button>Search</button>\n<input type=hidden name=t value=x></form><p>after</p>";

    #[test]
    fn a_textareas_value_is_a_value_and_never_page_prose() {
        let page = plain(&lines(CONTEXT_FIXTURE, 60)).join("\n");
        // The regression: the collapsed run the IFC used to make of it.
        assert!(
            !page.contains("hello textareaSearch"),
            "the textarea leaked into the page as prose:\n{page}"
        );
        // It is in the field instead, on its own two rows, with the whitespace
        // it was written with — a value, laid out by the box that holds it.
        assert!(page.contains("[hello               ]"), "{page}");
        assert!(page.contains("[textarea            ]"), "{page}");
        // And the rest of the form is there, which is the other half of the
        // Context's three faults: a 17-cell input and a labelled button.
        assert!(page.contains("[typed            ]"), "{page}");
        assert!(page.contains("[Search]"), "{page}");
    }

    #[test]
    fn a_control_this_engine_does_not_draw_occupies_no_cells() {
        // Byte-identical rows, not "looks empty": hidden and still unsupported
        // controls generate no box.
        let bare = plain(&lines("<p>a<span>b</span></p>", 40));
        for src in [
            "<p>a<input type=hidden value=x><span>b</span></p>",
            "<p>a<input type=file><span>b</span></p>",
            "<p>a<input type=range><span>b</span></p>",
        ] {
            assert_eq!(plain(&lines(src, 40)), bare, "{src}");
        }
        assert_eq!(
            plain(&lines(
                "<p>a<input type=checkbox checked><input type=radio><span>b</span></p>",
                40,
            ))[0],
            "a[x][o]b"
        );
    }

    #[test]
    fn a_value_wider_than_the_field_is_clipped_in_cells_and_never_wrapped() {
        // Ten cells of CJK in a field five characters wide. Clipped by *width*
        // — `chars().count()` would keep five glyphs and paint ten cells, which
        // is how a field runs over the text beside it.
        let rows = plain(&lines("<p><input size=5 value='漢字漢字漢'>|</p>", 40));
        assert_eq!(rows[0], "[漢字 ]|", "{rows:?}");
        // One row, not two: a value is clipped, never wrapped.
        assert_eq!(
            rows.iter().filter(|r| r.contains('漢')).count(),
            1,
            "{rows:?}"
        );
        // And the start of the value is what shows, which is what a browser
        // shows before the field is focused (M11.9 owns everything after).
        let rows = plain(&lines("<p><input size=6 value=abcdefghij></p>", 40));
        assert_eq!(rows[0], "[abcdef]", "{rows:?}");
    }

    #[test]
    fn a_field_is_as_wide_as_the_page_asked_wherever_it_is_laid_out() {
        // The same control through the three paths that can build one: an
        // atomic inline (the UA sheet's `inline-block`), a block-level replaced
        // box, and a flex item. A `size=17` field is 17 cells in all three —
        // `width: auto` on a replaced box is its intrinsic width, never its
        // containing block's.
        let field_widths = |css: &str| -> Vec<i32> {
            let (dom, styles) = styled_dom("<div><input size=17></div>", css);
            let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
            let mut out = Vec::new();
            tree.walk(tree.root, &mut |_, b| {
                if matches!(b.kind, BoxKind::Field(_)) {
                    out.push(b.dimensions.content.width);
                }
            });
            out
        };
        assert_eq!(field_widths(""), [17]);
        assert_eq!(field_widths("input { display: block }"), [17]);
        assert_eq!(field_widths("div { display: flex }"), [17]);
        // CSS wins when the page states one, as it does in a browser.
        assert_eq!(field_widths("input { width: 40px }"), [5]);
    }

    #[test]
    fn selects_keep_css_geometry_through_inline_block_and_flex_layout() {
        let html = "<p>L<select id=i><option>A</option></select>R</p>\
                    <select id=b size=2><option>A</option></select>\
                    <div id=r><span>L</span><select id=f><option>A</option></select><span>R</span></div>";
        let css = "html, body, p, div, select { margin: 0; padding: 0 }\
                   #b { display: block; width: 40px; height: 48px }\
                   #r { display: flex }";
        let (dom, styles) = styled_dom(html, css);
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let rect = |wanted: &str| {
            tree.boxes
                .iter()
                .find(|b| {
                    matches!(b.kind, BoxKind::Field(_))
                        && b.node
                            .is_some_and(|node| dom.attr(node, "id") == Some(wanted))
                })
                .unwrap()
                .dimensions
                .content
        };
        assert_eq!(
            (rect("i").x, rect("i").y, rect("i").width, rect("i").height),
            (1, 0, 3, 1)
        );
        assert_eq!(
            (rect("b").x, rect("b").y, rect("b").width, rect("b").height),
            (0, 1, 5, 3)
        );
        assert_eq!(
            (rect("f").x, rect("f").y, rect("f").width, rect("f").height),
            (1, 4, 3, 1)
        );
    }

    #[test]
    fn what_a_reader_typed_is_what_layout_draws() {
        // M11.9's foundation: the value is state beside the tree, so layout
        // reads it and the attribute stays the default it started as.
        let (mut dom, _) = styled_dom("<p><input size=8 value=default></p>", "");
        let input = (0..dom.node_count() as u32)
            .map(crate::dom::NodeId)
            .find(|&n| dom.attr(n, "size") == Some("8"))
            .expect("the field");
        dom.set_field_value(input, "typed");
        let styles = style::style_tree(&dom, &[]);
        let rows = plain(&lines_from_tree(&layout_document(
            &dom,
            &styles,
            40,
            Hidden::Respect,
        )));
        assert_eq!(rows[0], "[typed   ]", "{rows:?}");
    }

    /// A measurement, not an assertion: it asserts nothing and prints numbers,
    /// so it is `#[ignore]`d out of the default loop. Run it the way the
    /// numbers in the PR were taken:
    ///
    /// ```text
    /// cargo test --release --lib measure_the_field_work -- --ignored --nocapture
    /// ```
    ///
    /// M11.8's deliverable 8: a box kind on the layout path is a cost every
    /// page pays, including the four ladder pages that have no fields at all.
    /// Both sides run **in the same process, interleaved**, with control
    /// detection switched off for the A side — this machine drifts 5–10%
    /// between runs, so a before-commit/after-commit pair would be measuring
    /// the drift (CLAUDE.md).
    #[test]
    #[ignore]
    fn measure_the_field_work_on_the_ladder() {
        use std::time::{Duration, Instant};

        const ROUNDS: usize = 5;
        const PAGES: [&str; 4] = [
            "motherfuckingwebsite.com.html",
            "danluu.com.html",
            "news.ycombinator.com.html",
            "en.wikipedia.org.html",
        ];
        let summarize = |samples: &[Duration]| {
            let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
            let (lo, hi) = (samples.iter().min().unwrap(), samples.iter().max().unwrap());
            format!("{mean:.2?} ({lo:.2?}-{hi:.2?})")
        };

        eprintln!("M11.8 layout at 80 cells, mean of {ROUNDS} interleaved rounds:");
        for page in PAGES {
            let src = std::fs::read_to_string(format!(
                "{}/tests/fixtures/{page}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("committed fixture");
            let dom = html::parse(&src);
            let sheets = style::sources::inline_sheets(&dom);
            let refs: Vec<_> = sheets.iter().collect();
            let styles = style::style_tree(&dom, &refs);
            let once = || {
                let started = Instant::now();
                let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
                let elapsed = started.elapsed();
                (elapsed, tree.boxes.len())
            };

            let (mut off, mut on) = (Vec::new(), Vec::new());
            let mut boxes = (0, 0);
            for round in 0..=ROUNDS {
                // Which side goes first alternates, so any residue of running
                // second cancels across rounds instead of landing on a column.
                let (a, b) = if round % 2 == 0 {
                    let a = field::without_detection(once);
                    (a, once())
                } else {
                    let b = once();
                    (field::without_detection(once), b)
                };
                if round > 0 {
                    off.push(a.0);
                    on.push(b.0);
                }
                boxes = (a.1, b.1);
            }
            eprintln!(
                "  {page:<28} detection off {} ({} boxes)  ->  on {} ({} boxes)",
                summarize(&off),
                boxes.0,
                summarize(&on),
                boxes.1,
            );
        }
    }

    /// M11.12's A/B measurement: unlike M11.8's switch above, the baseline
    /// keeps text fields and buttons and disables only checkbox/radio/select.
    ///
    /// ```text
    /// cargo test --release --lib measure_choice_control_work -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_choice_control_work_on_the_ladder_and_flex_bench() {
        use std::time::{Duration, Instant};

        const ROUNDS: usize = 8;
        const PAGES: [&str; 4] = [
            "motherfuckingwebsite.com.html",
            "danluu.com.html",
            "news.ycombinator.com.html",
            "en.wikipedia.org.html",
        ];
        let summarize = |samples: &[Duration]| {
            let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
            let (lo, hi) = (samples.iter().min().unwrap(), samples.iter().max().unwrap());
            format!("{mean:.2?} ({lo:.2?}-{hi:.2?})")
        };

        eprintln!("M11.12 layout at 80 cells, mean of {ROUNDS} interleaved rounds:");
        for page in PAGES {
            let src = std::fs::read_to_string(format!(
                "{}/tests/fixtures/{page}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("committed fixture");
            let dom = html::parse(&src);
            let sheets = style::sources::inline_sheets(&dom);
            let refs: Vec<_> = sheets.iter().collect();
            let styles = style::style_tree(&dom, &refs);
            let once = || {
                let started = Instant::now();
                let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
                (started.elapsed(), tree.boxes.len())
            };

            let (mut before, mut after) = (Vec::new(), Vec::new());
            let mut boxes = (0, 0);
            for round in 0..=ROUNDS {
                let (a, b) = if round % 2 == 0 {
                    let a = field::without_choice_detection(once);
                    (a, once())
                } else {
                    let b = once();
                    (field::without_choice_detection(once), b)
                };
                if round > 0 {
                    before.push(a.0);
                    after.push(b.0);
                }
                boxes = (a.1, b.1);
            }
            eprintln!(
                "  {page:<28} choices off {} ({} boxes)  ->  on {} ({} boxes)",
                summarize(&before),
                boxes.0,
                summarize(&after),
                boxes.1,
            );
        }

        // The M9 layout bench's 300-card nested flex deck, repeated here so
        // both M11.12 sides run interleaved in this one process.
        let mut src = String::from(
            "<!doctype html><html><head><style>body{margin:0}div,p{margin:0}\
             .deck{display:flex;flex-wrap:wrap;gap:8px}\
             .card{display:flex;flex-direction:column;flex:1 1 160px}\
             .head{display:flex;justify-content:space-between}\
             .tag{flex:0 0 48px}</style></head><body><div class=deck>",
        );
        for i in 0..300 {
            src.push_str(&format!(
                "<div class=card><div class=head><span class=tag>t{i}</span>\
                 <span>card title {i}</span></div><p>a line of body text long enough \
                 to need measuring and breaking</p></div>"
            ));
        }
        src.push_str("</div></body></html>");
        let dom = html::parse(&src);
        let sheets = style::sources::inline_sheets(&dom);
        let refs: Vec<_> = sheets.iter().collect();
        let styles = style::style_tree(&dom, &refs);
        let once = || {
            let started = Instant::now();
            let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
            (started.elapsed(), tree.boxes.len())
        };
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let mut boxes = (0, 0);
        for round in 0..=ROUNDS {
            let (a, b) = if round % 2 == 0 {
                let a = field::without_choice_detection(once);
                (a, once())
            } else {
                let b = once();
                (field::without_choice_detection(once), b)
            };
            if round > 0 {
                before.push(a.0);
                after.push(b.0);
            }
            boxes = (a.1, b.1);
        }
        eprintln!(
            "  {:<28} choices off {} ({} boxes)  ->  on {} ({} boxes)",
            "M9 flex deck",
            summarize(&before),
            boxes.0,
            summarize(&after),
            boxes.1,
        );
    }

    #[test]
    fn table_rows_own_cells_and_share_provisional_columns() {
        let (dom, styles) = styled_dom(
            "<table><tr><th>Language</th><th>Year</th></tr><tr><td>Rust</td><td></td><td>extra</td></tr></table>",
            "table, tr, td, th { display: block }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let table = tree
            .boxes
            .iter()
            .position(|b| b.kind == BoxKind::Table)
            .unwrap();
        let rows = &tree.boxes[table].children;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|&row| tree.get(row).kind == BoxKind::TableRow)
        );
        let first = &tree.get(rows[0]).children;
        let second = &tree.get(rows[1]).children;
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 3);
        assert!(
            first
                .iter()
                .chain(second)
                .all(|&cell| tree.get(cell).kind == BoxKind::TableCell)
        );
        assert_eq!(
            tree.get(first[0]).dimensions.content.x,
            tree.get(second[0]).dimensions.content.x
        );
        assert_eq!(
            tree.get(first[1]).dimensions.content.x,
            tree.get(second[1]).dimensions.content.x
        );
        assert!(tree.get(rows[1]).dimensions.content.y > tree.get(rows[0]).dimensions.content.y);
        assert!(
            tree.get(second[1]).dimensions.content.height >= 1,
            "empty cells retain a hit-testable row"
        );
        let dump = crate::browser::inspector::box_lines(&dom, &tree).join("\n");
        assert!(dump.contains("table <table>"), "{dump}");
        assert!(dump.contains("table-row <tr>"), "{dump}");
        assert!(dump.contains("table-cell <td>"), "{dump}");
        assert!(
            tree.boxes.iter().any(|b| {
                b.kind == BoxKind::Text
                    && b.text.as_deref() == Some("Language")
                    && b.term_style.attrs.contains(crate::term::Attrs::BOLD)
            }),
            "header cells retain the UA emphasis"
        );
        assert_eq!(
            plain(&lines::from_tree(&tree)),
            vec!["LanguageYear", "Rust        extra"],
            "dump-text consumes the positioned cell tree in visual row order"
        );
    }

    #[test]
    fn table_columns_keep_rank_and_vote_compact_while_title_spends_the_width() {
        let (dom, styles) = styled_dom(
            "<table><tr><td>1.</td><td>▲</td><td>title lorem ipsum</td></tr>\
             <tr><td></td><td></td><td>42 points by reader</td></tr></table>",
            "table, tr, td { display: block }",
        );
        let tree = layout_document(&dom, &styles, 20, Hidden::Respect);
        let table = tree
            .boxes
            .iter()
            .position(|b| b.kind == BoxKind::Table)
            .unwrap();
        let first = &tree.get(tree.get(BoxId(table as u32)).children[0]).children;
        let widths: Vec<_> = first
            .iter()
            .map(|&cell| tree.get(cell).dimensions.margin_box_width())
            .collect();
        assert_eq!(&widths[..2], &[2, 1]);
        assert_eq!(widths.iter().sum::<i32>(), 20);
        assert!(widths[2] > widths[0] + widths[1]);
    }

    #[test]
    fn table_columns_honour_definite_percent_and_border_box_constraints() {
        let (dom, styles) = styled_dom(
            "<table class=fixed><tr><td class=border>fixed</td><td class=border>other</td></tr></table>\
             <table class=percent><tr><td>left</td><td>right</td></tr></table>",
            "table, tr, td { display: block }\
             table { width: 160px }\
             .fixed { width: 96px }\
             .border { width: 48px; padding: 8px; box-sizing: border-box }\
             .percent td { width: 50% }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let rows: Vec<_> = tree
            .boxes
            .iter()
            .filter(|box_| box_.kind == BoxKind::TableRow)
            .collect();
        let widths = |row: &LayoutBox| {
            row.children
                .iter()
                .map(|&cell| tree.get(cell).dimensions.margin_box_width())
                .collect::<Vec<_>>()
        };
        assert_eq!(widths(rows[0]), vec![6, 6], "border-box includes edges");
        assert_eq!(widths(rows[1]), vec![10, 10], "percentages use table width");
    }

    #[test]
    fn table_min_and_max_widths_request_their_clamped_used_width() {
        let (dom, styles) = styled_dom(
            "<table class=min><tr><td>a</td><td>b</td></tr></table>\
             <table class=max><tr><td>title words</td><td>meta</td></tr></table>",
            "table, tr, td { display: block }\
             .min { min-width: 160px }\
             .max { max-width: 80px }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let tables: Vec<_> = tree
            .boxes
            .iter()
            .filter(|box_| box_.kind == BoxKind::Table)
            .collect();
        assert_eq!(tables[0].dimensions.content.width, 20);
        assert_eq!(tables[1].dimensions.content.width, 10);
    }

    #[test]
    fn decoded_image_in_a_cell_contributes_its_terminal_width() {
        let (dom, styles) = styled_dom(
            "<table><tr><td><img src=logo.png width=80 height=16></td><td>meta</td></tr></table>",
            "table, tr, td { display: block }",
        );
        let images = crate::image::discover(&dom, Some("https://fixture.test/page"));
        let context = crate::image::ImageContext::from_discovery(
            &images,
            &mut crate::image::ImageCache::default(),
        );
        let tree = layout_document_with(&dom, &styles, 20, Hidden::Respect, &context);
        let row = tree
            .boxes
            .iter()
            .find(|box_| box_.kind == BoxKind::TableRow)
            .unwrap();
        assert_eq!(tree.get(row.children[0]).dimensions.margin_box_width(), 10);
        assert_eq!(tree.get(row.children[1]).dimensions.margin_box_width(), 4);
    }

    #[test]
    fn links_and_controls_inside_cells_keep_the_existing_hit_paths() {
        let (dom, styles) = styled_dom(
            "<table><tr><td><a href=/docs>docs</a></td><td><input value=go size=2></td></tr></table>",
            "table, tr, td { display: block }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let links = hit::collect_links(&tree, &dom);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].href, "/docs");
        assert_eq!(
            hit::link_at(&tree, &dom, links[0].x, links[0].y),
            Some((links[0].node, "/docs".into()))
        );
        assert_eq!(hit::focusables(&tree, &dom).len(), 2);
    }

    #[test]
    fn malformed_direct_table_content_remains_visible_beside_rows() {
        let (dom, styles) = styled_dom(
            "<table>loose prose<div>also loose</div><tr><td>cell</td></tr></table>",
            "table, tr, td { display: block }",
        );
        let tree = layout_document(&dom, &styles, 40, Hidden::Respect);
        let text = plain(&lines::from_tree(&tree)).join("");
        for expected in ["looseprose", "alsoloose", "cell"] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn narrow_deep_and_long_tables_stay_cell_bounded() {
        let mut source = String::new();
        for _ in 0..24 {
            source.push_str("<table><tr><td>");
        }
        source.push_str(&"界".repeat(4_096));
        for _ in 0..24 {
            source.push_str("</td></tr></table>");
        }
        let (dom, styles) = styled_dom(&source, "table, tr, td { display: block }");
        let tree = layout_document(&dom, &styles, 1, Hidden::Respect);
        assert_eq!(tree.width, 1);
        assert!(tree.height > 0);
        assert!(
            tree.boxes.len() < 20_000,
            "one-cell table layout expanded without bound"
        );
    }

    #[test]
    fn absurd_table_and_cell_widths_are_capped_without_losing_cells() {
        let (dom, styles) = styled_dom(
            "<table><tr><td class=wide>界界界</td><td class=wide></td></tr></table>",
            "table, tr, td { display: block }\
             table { width: 1e11em }\
             .wide { min-width: 1e11em; padding: 1e11em; box-sizing: border-box }",
        );
        let tree = layout_document(&dom, &styles, 1, Hidden::Respect);
        let row = tree
            .boxes
            .iter()
            .find(|box_| box_.kind == BoxKind::TableRow)
            .unwrap();
        assert_eq!(row.children.len(), 2);
        assert!(
            row.children
                .iter()
                .all(|&cell| tree.get(cell).dimensions.margin_box_width() > 0)
        );
        assert!(tree.boxes.len() < 32, "CSS widths allocated layout boxes");
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
            let tree = layout_document(&dom, &styles, 80, Hidden::Respect);
            let out = lines::from_tree(&tree);
            assert!(out.len() > 10);
            let all = plain(&out).join("\n");
            assert!(
                all.contains("Hacker News") || all.contains("Hacker"),
                "{all}"
            );
            assert!(
                tree.boxes.iter().any(|row| {
                    row.kind == BoxKind::TableRow && row.children.len() >= 3 && {
                        let cells = &row.children;
                        let widths = [
                            tree.get(cells[0]).dimensions.margin_box_width(),
                            tree.get(cells[1]).dimensions.margin_box_width(),
                            tree.get(cells[2]).dimensions.margin_box_width(),
                        ];
                        widths[0] < widths[2] && widths[1] < widths[2]
                    }
                }),
                "HN needs a compact rank/vote pair and a wider reading column"
            );
        }

        #[test]
        fn en_wikipedia_org() {
            let out = check(&fixture("en.wikipedia.org.html"), 100);
            assert!(out.len() > 100);
        }
    }
}
