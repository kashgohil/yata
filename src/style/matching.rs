//! Selector matching and the rule index (M4.2, PLAN.md §4).
//!
//! Two things live here: whether a selector matches a node, and how to avoid
//! asking that question for rules that obviously cannot match. The second is
//! the index — bucket every selector by its **rightmost** compound, then test
//! only the buckets an element could fall into. Wikipedia's sheets are
//! thousands of rules against tens of thousands of nodes; testing every rule
//! against every node is the quadratic version of this stage.
//!
//! The naive matcher is kept beside the index on purpose. It is the oracle the
//! equivalence test checks the fast path against, and the baseline M4.5 benches
//! — a fast path with nothing to compare against is a fast path nobody can
//! trust.

use std::collections::HashMap;

use crate::css::{Combinator, Compound, Declaration, PseudoClass, Selector, Stylesheet};
use crate::dom::{Dom, NodeData, NodeId};

/// One selector and the declarations it carries. A rule with a selector list
/// becomes one candidate per selector: they match different elements and carry
/// different specificities, but point at the same declarations.
pub struct Candidate<'a> {
    pub selector: &'a Selector,
    pub declarations: &'a [Declaration],
    /// Position in the flattened sheet order — the cascade's last tie-break.
    pub order: usize,
}

/// Selectors bucketed by their rightmost compound. Borrows the stylesheets:
/// cloning every selector per page would show up in the M4 gate's numbers.
pub struct RuleIndex<'a> {
    candidates: Vec<Candidate<'a>>,
    by_id: HashMap<&'a str, Vec<usize>>,
    by_class: HashMap<&'a str, Vec<usize>>,
    by_tag: HashMap<&'a str, Vec<usize>>,
    /// Selectors whose rightmost compound is `*` or a bare pseudo-class: no
    /// key to bucket them under, so every element tests them.
    universal: Vec<usize>,
}

impl<'a> RuleIndex<'a> {
    /// Borrowed sheets, not owned: `App` keeps each page's stylesheets in
    /// document-order slots and restyles every time one arrives, so an index
    /// that took ownership would clone every rule on every arrival.
    pub fn build(sheets: &[&'a Stylesheet]) -> RuleIndex<'a> {
        let mut index = RuleIndex {
            candidates: Vec::new(),
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_tag: HashMap::new(),
            universal: Vec::new(),
        };
        for sheet in sheets.iter().copied() {
            for rule in &sheet.rules {
                for selector in &rule.selectors {
                    let Some((_, rightmost)) = selector.parts.last() else {
                        continue;
                    };
                    let slot = index.candidates.len();
                    index.candidates.push(Candidate {
                        selector,
                        declarations: &rule.declarations,
                        order: slot,
                    });
                    // Most selective key first: an id narrows harder than a
                    // class, a class harder than a tag. A selector lands in
                    // exactly one bucket, which is what keeps a lookup from
                    // returning the same candidate twice.
                    if let Some(id) = &rightmost.id {
                        index.by_id.entry(id).or_default().push(slot);
                    } else if let Some(class) = rightmost.classes.first() {
                        index.by_class.entry(class).or_default().push(slot);
                    } else if let Some(tag) = &rightmost.tag {
                        index.by_tag.entry(tag).or_default().push(slot);
                    } else {
                        index.universal.push(slot);
                    }
                }
            }
        }
        index
    }

    /// Candidates that match `node`, in sheet order. Only the buckets this
    /// element can fall into are tested.
    pub fn matches(&self, dom: &Dom, node: NodeId) -> Vec<&Candidate<'a>> {
        let mut slots = self.universal.clone();
        if let NodeData::Element { tag, .. } = &dom.node(node).data {
            if let Some(bucket) = self.by_tag.get(tag.as_str()) {
                slots.extend_from_slice(bucket);
            }
        } else {
            // Only elements match selectors; text nodes inherit instead.
            return Vec::new();
        }
        if let Some(id) = dom.attr(node, "id")
            && let Some(bucket) = self.by_id.get(id)
        {
            slots.extend_from_slice(bucket);
        }
        for class in dom.attr(node, "class").unwrap_or("").split_whitespace() {
            if let Some(bucket) = self.by_class.get(class) {
                slots.extend_from_slice(bucket);
            }
        }
        // `class="a a"` would otherwise propose the same candidate twice, and a
        // duplicate declaration is a duplicate cascade entry.
        slots.sort_unstable();
        slots.dedup();
        slots
            .into_iter()
            .map(|slot| &self.candidates[slot])
            .filter(|c| matches(dom, node, c.selector))
            .collect()
    }

    /// The same answer, computed by testing every rule: the oracle for the
    /// equivalence test and the baseline for the M4.5 bench.
    pub fn matches_naive(&self, dom: &Dom, node: NodeId) -> Vec<&Candidate<'a>> {
        if !matches!(dom.node(node).data, NodeData::Element { .. }) {
            return Vec::new();
        }
        self.candidates
            .iter()
            .filter(|c| matches(dom, node, c.selector))
            .collect()
    }

    /// How many (selector, declarations) pairs the index holds — the number
    /// the naive matcher tests per element, and the number M4.5's bench divides
    /// by to show what the buckets save.
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }
}

/// Does `selector` match `node`? Evaluated right to left — the rightmost
/// compound is the cheapest thing to reject on, which is the whole reason
/// selectors are indexed by it.
pub fn matches(dom: &Dom, node: NodeId, selector: &Selector) -> bool {
    matches_parts(dom, node, &selector.parts)
}

/// `parts`' last compound must match `node`, and the rest must match some chain
/// of ancestors.
///
/// The ancestor search backtracks, and it has to: greedy nearest-match is wrong
/// for a mixed chain. `a > b span` against `<a><b><b><span>` matches only by
/// choosing the *outer* `b` — the nearest `b` fails the child combinator, and a
/// matcher that stops there reports no match. Depth is selector length, not
/// document depth, so the recursion is a handful of frames.
fn matches_parts(dom: &Dom, node: NodeId, parts: &[(Combinator, Compound)]) -> bool {
    let Some((combinator, rightmost)) = parts.last() else {
        return false;
    };
    if !compound_matches(dom, node, rightmost) {
        return false;
    }
    let rest = &parts[..parts.len() - 1];
    if rest.is_empty() {
        return true;
    }
    match combinator {
        Combinator::Child => {
            parent_element(dom, node).is_some_and(|parent| matches_parts(dom, parent, rest))
        }
        Combinator::Descendant => {
            let mut ancestor = parent_element(dom, node);
            while let Some(id) = ancestor {
                if matches_parts(dom, id, rest) {
                    return true;
                }
                ancestor = parent_element(dom, id);
            }
            false
        }
    }
}

fn compound_matches(dom: &Dom, node: NodeId, compound: &Compound) -> bool {
    let NodeData::Element { tag, .. } = &dom.node(node).data else {
        return false;
    };
    // Exact comparison, and it must stay exact: `RuleIndex` buckets selectors
    // by tag under a plain `HashMap` lookup, so a case-insensitive compare here
    // would make the index and the naive matcher disagree on an uppercase tag —
    // silently, since the fast path would simply never propose the rule.
    // Both sides are lowercase by construction: the HTML tokenizer lowercases
    // tag names (M2.1) and the CSS parser lowercases type selectors (M4.1).
    if let Some(want) = &compound.tag
        && tag != want
    {
        return false;
    }
    if let Some(want) = &compound.id
        && dom.attr(node, "id") != Some(want.as_str())
    {
        return false;
    }
    if !compound.classes.is_empty() {
        // `class="athing submission"` is two classes, not one string.
        let classes = dom.attr(node, "class").unwrap_or("");
        for want in &compound.classes {
            if !classes.split_whitespace().any(|have| have == want) {
                return false;
            }
        }
    }
    compound.pseudo.iter().all(|pseudo| match pseudo {
        // An unvisited link, which is every link: history arrives in M6, and
        // until then `:visited` matching nothing is the honest answer.
        PseudoClass::Link => tag.eq_ignore_ascii_case("a") && dom.attr(node, "href").is_some(),
        // Nothing hovers until M6 wires the mouse; `Unsupported` is inert by
        // definition (M4.1). All three must match nothing rather than
        // everything — a pseudo we cannot evaluate is not a pseudo we ignore.
        PseudoClass::Visited | PseudoClass::Hover | PseudoClass::Unsupported(_) => false,
    })
}

/// Nearest ancestor that is an element. Text and comment nodes are skipped, and
/// the `Document` root terminates the walk.
fn parent_element(dom: &Dom, node: NodeId) -> Option<NodeId> {
    let mut current = dom.node(node).parent;
    while let Some(id) = current {
        if matches!(dom.node(id).data, NodeData::Element { .. }) {
            return Some(id);
        }
        current = dom.node(id).parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::parse;
    use crate::html;

    /// First element with the given tag, in document order.
    fn find(dom: &Dom, tag: &str) -> NodeId {
        fn walk(dom: &Dom, id: NodeId, tag: &str) -> Option<NodeId> {
            if matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag) {
                return Some(id);
            }
            dom.children(id).find_map(|child| walk(dom, child, tag))
        }
        walk(dom, dom.root, tag).expect("fixture is missing that tag")
    }

    /// Does `selector` match the first `tag` element of `html`?
    fn hit(html_src: &str, tag: &str, selector: &str) -> bool {
        let dom = html::parse(html_src);
        let sheet = parse(&format!("{selector} {{ color: red }}"));
        let node = find(&dom, tag);
        matches(&dom, node, &sheet.rules[0].selectors[0])
    }

    #[test]
    fn simple_and_compound_selectors() {
        let doc = "<p id='lead' class='intro big'>x</p>";
        assert!(hit(doc, "p", "p"));
        assert!(hit(doc, "p", "*"));
        assert!(hit(doc, "p", "#lead"));
        assert!(hit(doc, "p", ".intro"));
        // Both classes are required, and they are tokens: `.big` must match
        // `class="intro big"`, which string equality would miss.
        assert!(hit(doc, "p", "p.intro.big#lead"));
        assert!(!hit(doc, "p", ".intro.missing"));
        assert!(!hit(doc, "p", "#other"));
        assert!(!hit(doc, "p", "div"));
    }

    #[test]
    fn tag_case_is_normalized_before_matching_ever_sees_it() {
        // The parsers lowercase both sides — the tokenizer the tags, the CSS
        // parser the type selectors — which is what lets the index bucket by
        // tag with an exact hash lookup. This pins the invariant end to end.
        assert!(hit("<DIV><P>x</P></DIV>", "p", "div p"));
        assert!(hit("<div><p>x</p></div>", "p", "DIV P"));
    }

    #[test]
    fn descendant_and_child_differ() {
        let doc = "<div><section><p>x</p></section></div>";
        assert!(hit(doc, "p", "div p"));
        assert!(hit(doc, "p", "section > p"));
        // A grandchild is not a child.
        assert!(!hit(doc, "p", "div > p"));
    }

    #[test]
    fn the_ancestor_search_backtracks() {
        // `a > b span` against <a><b><b><span>: the nearest <b> ancestor fails
        // the child combinator, and only the outer one works. A greedy
        // right-to-left walk reports no match here.
        let doc = "<a href='x'><b><b><span>t</span></b></b></a>";
        assert!(hit(doc, "span", "a > b span"));
        // Same shape, no <a>: still no match, so the test above is not just
        // "anything matches".
        assert!(!hit(
            "<div><b><b><span>t</span></b></b></div>",
            "span",
            "a > b span"
        ));
    }

    #[test]
    fn link_matches_an_anchor_with_an_href() {
        assert!(hit("<a href='/x'>t</a>", "a", "a:link"));
        // A bare <a> is an anchor, not a link.
        assert!(!hit("<a name='top'>t</a>", "a", "a:link"));
    }

    #[test]
    fn pseudo_classes_we_cannot_evaluate_match_nothing() {
        // The failure mode being pinned: an unknown pseudo quietly matching
        // everything would paint every paragraph.
        let doc = "<a href='/x'>t</a>";
        assert!(!hit(doc, "a", "a:visited"));
        assert!(!hit(doc, "a", "a:hover"));
        assert!(!hit(doc, "a", "a:nth-child(2)"));
        assert!(!hit(doc, "a", "a::before"));
    }

    #[test]
    fn the_index_proposes_what_the_naive_matcher_finds() {
        let dom = html::parse("<div id='main'><p class='a b'>x</p><span>y</span></div>");
        let sheets = [parse(
            "p { color: 1 } .a { color: 2 } .b { color: 3 } #main p { color: 4 } \
             * { color: 5 } span { color: 6 } #main { color: 7 } div > p { color: 8 } \
             .missing { color: 9 } h1 { color: 10 }",
        )];
        let index = RuleIndex::build(&sheets.iter().collect::<Vec<_>>());
        assert_eq!(index.candidate_count(), 10);

        for tag in ["div", "p", "span"] {
            let node = find(&dom, tag);
            let fast: Vec<usize> = index.matches(&dom, node).iter().map(|c| c.order).collect();
            let naive: Vec<usize> = index
                .matches_naive(&dom, node)
                .iter()
                .map(|c| c.order)
                .collect();
            assert_eq!(fast, naive, "{tag}");
        }
        // And the buckets really are doing work: the <span> never tests the
        // eight rules that cannot match it.
        let span = find(&dom, "span");
        assert_eq!(index.matches(&dom, span).len(), 2);
    }

    #[test]
    fn a_duplicated_class_proposes_a_candidate_once() {
        let dom = html::parse("<p class='a a'>x</p>");
        let sheets = [parse(".a { color: red }")];
        let index = RuleIndex::build(&sheets.iter().collect::<Vec<_>>());
        assert_eq!(index.matches(&dom, find(&dom, "p")).len(), 1);
    }
}
