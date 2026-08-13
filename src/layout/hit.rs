//! Hit-testing and link discovery over the layout tree (PLAN.md M6).
//!
//! Pure queries: document cell coordinates in, node/href out. No mutation, no
//! I/O. Click, link hints, Tab focus and `:hover` all share this walk.
//!
//! Content clipped away by `overflow` (M9.3) is not here: a box the reader
//! cannot see is not clickable and gets no link hint. The rule comes from
//! [`Clip`], the same one paint trims the display list with.

use crate::dom::{Dom, NodeData, NodeId};
use crate::layout::boxes::{BoxId, LayoutTree};
use crate::layout::clip::Clip;
use crate::layout::dimensions::Rect;

/// One `<a href>` discovered in the layout tree, with the position of its
/// first content fragment (document coordinates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkHit {
    pub node: NodeId,
    pub href: String,
    /// Content-box origin of the first laid-out fragment of this link.
    pub x: i32,
    pub y: i32,
}

/// Deepest layout box at `(x, y)` that carries a DOM node, then walk up to the
/// nearest ancestor with an `href`. Document cell coordinates (page column
/// origin, not frame origin).
pub fn link_at(tree: &LayoutTree, dom: &Dom, x: i32, y: i32) -> Option<(NodeId, String)> {
    let node = hit_test(tree, x, y)?;
    nearest_link(dom, node)
}

/// Deepest box whose border box contains `(x, y)`, that has a `node`, and that
/// the clips above it leave visible.
pub fn hit_test(tree: &LayoutTree, x: i32, y: i32) -> Option<NodeId> {
    hit_box(tree, tree.root, x, y, Clip::NONE)
}

fn hit_box(tree: &LayoutTree, id: BoxId, x: i32, y: i32, clip: Clip) -> Option<NodeId> {
    let b = tree.get(id);
    // Prefer a descendant: later paint order sits on top, and text fragments
    // live under their inline/block ancestors.
    let mut best = None;
    let inside = clip.inside(b);
    if !inside.is_empty() {
        for &child in &b.children {
            if let Some(n) = hit_box(tree, child, x, y, inside) {
                best = Some(n);
            }
        }
    }
    if best.is_some() {
        return best;
    }
    // The click has to land on a cell this box actually painted: a box hidden
    // by an ancestor's `overflow` is not under the cursor, whatever its
    // geometry says.
    if clip.contains(x, y) && contains(b.dimensions.border_box(), x, y) {
        return b.node;
    }
    None
}

fn contains(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

/// Walk from `node` up to the nearest `<a href>` element. Same predicate as
/// `dom_links` / `:link` so click, Tab, and cascade agree.
pub fn nearest_link(dom: &Dom, node: NodeId) -> Option<(NodeId, String)> {
    let mut current = Some(node);
    while let Some(id) = current {
        if let NodeData::Element { tag, .. } = &dom.node(id).data
            && tag.eq_ignore_ascii_case("a")
            && let Some(href) = dom.attr(id, "href")
        {
            return Some((id, href.to_string()));
        }
        current = dom.node(id).parent;
    }
    None
}

/// Every link that produced at least one *visible* layout box, in document
/// order of first appearance, with the position of that first fragment.
///
/// A link whose first fragment is clipped away is registered at the first
/// fragment that survives, so a hint label always sits on something the reader
/// can see. A link with no surviving fragment is not listed at all.
pub fn collect_links(tree: &LayoutTree, dom: &Dom) -> Vec<LinkHit> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    tree.walk_clipped(&mut |_, b, clip| {
        let Some(node) = b.node else {
            return;
        };
        if !clip.shows(b) {
            return;
        }
        let Some((link, href)) = nearest_link(dom, node) else {
            return;
        };
        if seen.contains(&link) {
            return;
        }
        // Prefer a text fragment's origin when we first meet the link via
        // text; otherwise use this box's content origin.
        seen.push(link);
        out.push(LinkHit {
            node: link,
            href,
            x: b.dimensions.content.x,
            y: b.dimensions.content.y,
        });
    });
    out
}

/// Links whose first fragment lies in the inclusive document y-range
/// `[top, bottom)` — the visible page rows after scroll.
pub fn visible_links(tree: &LayoutTree, dom: &Dom, top: i32, bottom: i32) -> Vec<LinkHit> {
    collect_links(tree, dom)
        .into_iter()
        .filter(|l| l.y >= top && l.y < bottom)
        .collect()
}

/// Whether `node` is `ancestor` or a descendant of it.
pub fn is_under(dom: &Dom, node: NodeId, ancestor: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = dom.node(id).parent;
    }
    false
}

/// Document-order list of every `<a href>` element in the DOM (for Tab cycle).
pub fn dom_links(dom: &Dom) -> Vec<(NodeId, String)> {
    let mut out = Vec::new();
    walk_dom_links(dom, dom.root, &mut out);
    out
}

fn walk_dom_links(dom: &Dom, id: NodeId, out: &mut Vec<(NodeId, String)>) {
    if let NodeData::Element { tag, .. } = &dom.node(id).data
        && tag.eq_ignore_ascii_case("a")
        && let Some(href) = dom.attr(id, "href")
    {
        out.push((id, href.to_string()));
    }
    for child in dom.children(id) {
        walk_dom_links(dom, child, out);
    }
}

/// First content-box y of `node` in the layout tree, if any box was generated.
pub fn first_y(tree: &LayoutTree, node: NodeId) -> Option<i32> {
    y_of(tree, |n| n == node)
}

/// The row to scroll to in order to show `node` — what a fragment jump needs
/// (M11.4). In order: the first box in document order belonging to the node or
/// to anything inside it; failing that, the first box of the nearest ancestor
/// that was laid out; failing that, `None`.
///
/// Neither fallback is a nicety, because a great many anchor targets generate
/// no box of their own:
///
/// - a non-atomic inline is laid out as the text inside it, so
///   `<span id="x">heading</span>` is found through its text — an inline
///   element's rendered position *is* its content's position;
/// - and even that text can be absent from the tree, because a line merges
///   adjacent same-styled pieces into one box and the merged box keeps the
///   *first* piece's node. `<p>see <span id="x">this</span></p>` renders as a
///   single text box belonging to the paragraph's first text node, and the
///   only record left of where the span sits is its ancestor.
///
/// So an exact-or-subtree lookup answers `None` for targets the reader can
/// plainly see, and answering `None` means the click does nothing — which the
/// reader cannot tell from a broken link. The ancestor is approximate (the
/// paragraph's first row, not the wrapped line the span landed on) and it is
/// the closest thing on screen to the truth.
///
/// A `display: none` target takes the same path and lands where the element
/// *would* be. That is the accepted cost of the rule: the two cases are
/// indistinguishable from here — a box that was never generated and a box that
/// was merged away look identical in the tree — and moving the reader to the
/// right region beats leaving them where they were with nothing to show for
/// the click.
///
/// Same walk and same coordinate as [`first_y`] and the resize anchor,
/// deliberately: a second way to compute where a box sits is a second way to
/// disagree about it.
pub fn nearest_y(tree: &LayoutTree, dom: &Dom, node: NodeId) -> Option<i32> {
    // Ancestors nearest first, so a smaller index is a better fallback.
    let mut ancestors = Vec::new();
    let mut current = dom.node(node).parent;
    while let Some(id) = current {
        ancestors.push(id);
        current = dom.node(id).parent;
    }

    let mut inside = None;
    let mut fallback: Option<(usize, i32)> = None;
    tree.walk(tree.root, &mut |_, b| {
        if inside.is_some() {
            return;
        }
        let Some(box_node) = b.node else {
            return;
        };
        let y = b.dimensions.content.y;
        if is_under(dom, box_node, node) {
            inside = Some(y);
        } else if let Some(depth) = ancestors.iter().position(|&a| a == box_node)
            && fallback.is_none_or(|(nearest, _)| depth < nearest)
        {
            fallback = Some((depth, y));
        }
    });
    inside.or(fallback.map(|(_, y)| y))
}

fn y_of(tree: &LayoutTree, mut want: impl FnMut(NodeId) -> bool) -> Option<i32> {
    let mut found = None;
    tree.walk(tree.root, &mut |_, b| {
        if found.is_none()
            && let Some(node) = b.node
            && want(node)
        {
            found = Some(b.dimensions.content.y);
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::layout::{self, Hidden};
    use crate::style;

    fn laid(html: &str) -> (crate::dom::Dom, LayoutTree) {
        let dom = html::parse(html);
        let styles = style::style_tree(&dom, &[]);
        let tree = layout::layout_document(&dom, &styles, 40, Hidden::Respect);
        (dom, tree)
    }

    #[test]
    fn click_on_link_text_finds_the_link() {
        let (dom, tree) = laid("<p>see <a href=/docs>docs</a> here</p>");
        let links = collect_links(&tree, &dom);
        assert_eq!(links.len(), 1);
        let link = &links[0];
        // Hit the first character of the link's text fragment.
        let got = link_at(&tree, &dom, link.x, link.y);
        assert_eq!(got, Some((link.node, "/docs".into())));
    }

    #[test]
    fn click_on_nested_span_inside_link_still_finds_link() {
        let (dom, tree) = laid("<a href=x><span>inner</span></a>");
        let links = collect_links(&tree, &dom);
        assert_eq!(links.len(), 1);
        let link = &links[0];
        let got = link_at(&tree, &dom, link.x, link.y);
        assert_eq!(got.map(|(_, h)| h).as_deref(), Some("x"));
    }

    fn node_with_id(dom: &crate::dom::Dom, id: &str) -> NodeId {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .expect("fixture id")
    }

    #[test]
    fn an_inline_target_is_found_through_the_text_inside_it() {
        // M11.4: `<span id=x>` generates no box of its own, so an exact-match
        // lookup answers `None` for one of the commonest anchor targets on the
        // web. Its rendered position is its text's position.
        let (dom, tree) = laid("<p>first</p><p><span id=x>marked</span></p>");
        let span = node_with_id(&dom, "x");
        assert_eq!(first_y(&tree, span), None, "an inline generated a box");
        assert_eq!(
            nearest_y(&tree, &dom, span),
            first_y(&tree, dom.node(span).parent.unwrap()),
            "the inline's own text is on the paragraph's row"
        );
        // …and it is the second paragraph's row, not the first's.
        assert!(nearest_y(&tree, &dom, span).unwrap() > 0);
    }

    #[test]
    fn a_target_whose_text_was_merged_away_falls_back_to_its_ancestor() {
        // The line builder merges adjacent same-styled pieces and the merged
        // box keeps the *first* piece's node, so nothing in the tree belongs
        // to this span at all — not even its text.
        let (dom, tree) = laid("<p>first</p><p>see <span id=x>this</span></p>");
        let span = node_with_id(&dom, "x");
        let paragraph = dom.node(span).parent.unwrap();
        assert_eq!(nearest_y(&tree, &dom, span), first_y(&tree, paragraph));
        assert!(nearest_y(&tree, &dom, span).unwrap() > 0);
    }

    #[test]
    fn a_target_with_no_box_lands_where_it_would_have_been() {
        // `display: none` is the same shape as the merge above from here, and
        // takes the same answer: the nearest ancestor that was laid out.
        let (dom, tree) = laid(
            "<p>first</p><div id=wrap><p style='display:none'><span id=x>gone</span></p></div>",
        );
        let span = node_with_id(&dom, "x");
        assert_eq!(
            nearest_y(&tree, &dom, span),
            first_y(&tree, node_with_id(&dom, "wrap")),
            "a hidden target did not fall back to its laid-out ancestor"
        );
    }

    #[test]
    fn a_target_in_the_head_lands_at_the_top_of_the_document() {
        // Nothing in `<head>` is laid out, so the walk keeps climbing and the
        // first ancestor with a box is the document itself. The rule does not
        // stop early for this case: the reader asked to go somewhere, and the
        // top is where the nearest laid-out ancestor is.
        let (dom, tree) = laid("<head><meta id=x></head><body><p>text</p></body>");
        assert_eq!(nearest_y(&tree, &dom, node_with_id(&dom, "x")), Some(0));
    }

    #[test]
    fn click_outside_any_link_returns_none() {
        let (dom, tree) = laid("<p>no links here</p>");
        // Somewhere on the first line of text — not a link.
        assert_eq!(link_at(&tree, &dom, 0, 0), None);
    }

    #[test]
    fn nearest_link_walks_up_from_text() {
        let dom = html::parse("<a href=y>hi</a>");
        // Find the text node under the anchor.
        let a = dom_links(&dom)[0].0;
        let text = dom.children(a).next().expect("text child");
        assert_eq!(
            nearest_link(&dom, text).map(|(_, h)| h).as_deref(),
            Some("y")
        );
    }

    /// The same page twice: clipping the second row away, and not (M9.3).
    /// Without the control, a test that finds one link proves nothing about
    /// the clip — the page might have generated one link all along.
    fn two_rows(overflow: &str) -> (crate::dom::Dom, LayoutTree) {
        laid(&format!(
            "<body style='margin:0'><div style='margin:0;max-height:1em;overflow:{overflow}'>\
             <p style='margin:0'>visible <a href=/a>first</a></p>\
             <p style='margin:0'>hidden <a href=/b>second</a></p></div></body>"
        ))
    }

    #[test]
    fn clipped_away_content_is_not_hit_testable() {
        let (dom, tree) = two_rows("visible");
        let control = link_at(&tree, &dom, 8, 1);
        assert_eq!(
            control.map(|(_, h)| h).as_deref(),
            Some("/b"),
            "the second link is at (8, 1) when nothing clips it"
        );

        let (dom, tree) = two_rows("hidden");
        // The box is still in the tree at row 1; it just reaches no cell.
        assert_eq!(hit_test(&tree, 8, 1), None);
        assert_eq!(link_at(&tree, &dom, 8, 1), None);
    }

    #[test]
    fn a_clipped_away_link_gets_no_hint() {
        let (dom, tree) = two_rows("visible");
        assert_eq!(hrefs(&collect_links(&tree, &dom)), ["/a", "/b"]);
        let (dom, tree) = two_rows("hidden");
        assert_eq!(hrefs(&collect_links(&tree, &dom)), ["/a"]);
    }

    fn hrefs(links: &[LinkHit]) -> Vec<&str> {
        links.iter().map(|l| l.href.as_str()).collect()
    }

    #[test]
    fn visible_links_filters_by_y_range() {
        let (dom, tree) =
            laid("<p><a href=1>one</a></p><p><a href=2>two</a></p><p><a href=3>three</a></p>");
        let all = collect_links(&tree, &dom);
        assert_eq!(all.len(), 3);
        // Only the first link's y.
        let top = all[0].y;
        let vis = visible_links(&tree, &dom, top, top + 1);
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].href, "1");
    }
}
