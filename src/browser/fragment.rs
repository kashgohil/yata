//! What `#x` in a URL points at (PLAN.md M6 navigation, M11.4).
//!
//! One resolver, two callers: a link to `#x` and `location.hash = 'x'` take
//! this same path, because a script must not be able to reach a place a link
//! cannot.
//!
//! Pure query: fragment text and DOM in, a target out. It knows nothing about
//! layout — where the node *is* on screen is the layout tree's answer
//! (`layout::nearest_y`), and keeping the two apart is what stops this from
//! becoming a second way to compute box positions.

use crate::dom::{Dom, NodeData, NodeId};
use crate::net;

/// Where a fragment points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The top of the document: the empty fragment, and `#top` when nothing
    /// is named that. Not a failed lookup — HTML says so, and the Wikipedia
    /// fixture alone has nine `href="#"` links that mean exactly this.
    Top,
    /// The element the fragment names.
    Node(NodeId),
}

/// Resolve a fragment (the text *after* `#`) against the document, following
/// HTML's "find a potential indicated element":
///
/// 1. the first element in document order whose `id` matches **exactly** —
///    ids are case-sensitive, unlike almost everything else in HTML;
/// 2. failing that, the first `<a name="…">` — the legacy fallback, still on
///    old pages and in old bookmarks, which no ladder fixture uses;
/// 3. failing that, `#top` (ASCII case-insensitive) and the empty fragment
///    mean the top of the document;
/// 4. failing that, `None` — the fragment names nothing that is here.
///
/// The fragment is percent-decoded first: a URL escapes non-ASCII anchors
/// (`#%E7%8C%AB`) while `Dom::attr` holds them decoded, so the comparison has
/// to happen on one side or the other and the decoded side is the one a page
/// author wrote.
///
/// `None` is not an error. The caller does nothing with it — no scroll, no
/// error page, no console line — because a link to an id that a page dropped
/// years ago is not something the reader did wrong, and every stale citation
/// on the web would otherwise print a complaint.
pub fn resolve(dom: &Dom, fragment: &str) -> Option<Target> {
    if fragment.is_empty() {
        return Some(Target::Top);
    }
    let wanted = net::percent_decode(fragment);
    // One walk for both rules: `id` anywhere in the document beats `<a name>`
    // anywhere in it, so the name candidate is carried along and only used
    // once the walk has run out of ids.
    let mut named = None;
    if let Some(node) = scan(dom, dom.root, &wanted, &mut named) {
        return Some(Target::Node(node));
    }
    if let Some(node) = named {
        return Some(Target::Node(node));
    }
    // `top` is a fallback, not an override: a page with `id="top"` wins it.
    fragment.eq_ignore_ascii_case("top").then_some(Target::Top)
}

/// First `id` match in document order, recording the first `<a name>` match
/// on the way past it.
fn scan(dom: &Dom, node: NodeId, wanted: &str, named: &mut Option<NodeId>) -> Option<NodeId> {
    for child in dom.children(node) {
        if dom.attr(child, "id") == Some(wanted) {
            return Some(child);
        }
        if named.is_none()
            && let NodeData::Element { tag, .. } = &dom.node(child).data
            && tag.eq_ignore_ascii_case("a")
            && dom.attr(child, "name") == Some(wanted)
        {
            *named = Some(child);
        }
        if let Some(found) = scan(dom, child, wanted, named) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn id_of(dom: &Dom, target: Option<Target>) -> Option<String> {
        match target? {
            Target::Top => Some("<top>".into()),
            Target::Node(node) => Some(
                dom.attr(node, "id")
                    .or_else(|| dom.attr(node, "name"))
                    .unwrap_or("<unnamed>")
                    .to_string(),
            ),
        }
    }

    #[test]
    fn an_id_matches_exactly_and_in_document_order() {
        let dom = html::parse("<p id=alpha>a</p><p id=Alpha>b</p><p id=alpha>c</p>");
        assert_eq!(
            resolve(&dom, "alpha").map(|t| match t {
                Target::Node(n) => n,
                Target::Top => unreachable!(),
            }),
            // First of the two `alpha`s — the DOM's rule for duplicate ids,
            // which real pages do have.
            Some(first_with(&dom, "alpha"))
        );
        // Case-sensitive: `Alpha` is a different id, not the same one.
        assert_eq!(
            id_of(&dom, resolve(&dom, "Alpha")).as_deref(),
            Some("Alpha")
        );
        assert!(resolve(&dom, "ALPHA").is_none());
    }

    fn first_with(dom: &Dom, id: &str) -> NodeId {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .expect("fixture id")
    }

    #[test]
    fn an_a_name_anchor_is_the_fallback_when_no_id_matches() {
        // HTML's legacy anchor. No ladder fixture uses it, so this test is the
        // only thing keeping it honest.
        let dom = html::parse("<p>before</p><a name=notes>notes</a><p>after</p>");
        assert_eq!(
            id_of(&dom, resolve(&dom, "notes")).as_deref(),
            Some("notes")
        );
        // Only `<a>` — `name` on a `<meta>` or an `<input>` is a different
        // attribute with a different meaning.
        let dom = html::parse("<meta name=notes><input name=notes>");
        assert!(resolve(&dom, "notes").is_none());
    }

    #[test]
    fn an_id_anywhere_beats_a_name_anywhere() {
        // The name comes first in document order and still loses: the two
        // rules are ordered, not interleaved.
        let dom = html::parse("<a name=x>anchor</a><p id=x>element</p>");
        assert_eq!(id_of(&dom, resolve(&dom, "x")).as_deref(), Some("x"));
        assert_eq!(
            resolve(&dom, "x"),
            Some(Target::Node(first_with(&dom, "x"))),
            "the element with id=x, not the <a name=x> before it"
        );
    }

    #[test]
    fn the_empty_fragment_and_top_are_the_top_of_the_document() {
        let dom = html::parse("<p id=elsewhere>x</p>");
        assert_eq!(resolve(&dom, ""), Some(Target::Top));
        assert_eq!(resolve(&dom, "top"), Some(Target::Top));
        assert_eq!(resolve(&dom, "TOP"), Some(Target::Top));
        // …unless the page names something `top`, which wins.
        let dom = html::parse("<p>a</p><p id=top>real</p>");
        assert_eq!(
            resolve(&dom, "top"),
            Some(Target::Node(first_with(&dom, "top")))
        );
    }

    #[test]
    fn a_fragment_that_names_nothing_resolves_to_nothing() {
        let dom = html::parse("<p id=here>x</p>");
        assert_eq!(resolve(&dom, "gone"), None);
    }

    #[test]
    fn a_percent_escaped_fragment_matches_the_decoded_id() {
        let dom = html::parse("<h2 id=\"Ausgangsüberprüfung\">x</h2><h2 id=\"猫\">y</h2>");
        assert_eq!(
            id_of(&dom, resolve(&dom, "Ausgangs%C3%BCberpr%C3%BCfung")).as_deref(),
            Some("Ausgangsüberprüfung")
        );
        assert_eq!(
            id_of(&dom, resolve(&dom, "%E7%8C%AB")).as_deref(),
            Some("猫")
        );
        // An id that really does contain a percent sign still matches, because
        // the decode fails soft rather than erroring.
        let dom = html::parse("<p id=\"100%zz\">x</p>");
        assert_eq!(
            id_of(&dom, resolve(&dom, "100%zz")).as_deref(),
            Some("100%zz")
        );
    }
}
