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
use crate::net;
use crate::style::StyleContext;

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
    pub fn matches(&self, dom: &Dom, node: NodeId, ctx: &StyleContext<'_>) -> Vec<&Candidate<'a>> {
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
            .filter(|c| matches(dom, node, c.selector, ctx))
            .collect()
    }

    /// The same answer, computed by testing every rule: the oracle for the
    /// equivalence test and the baseline for the M4.5 bench.
    pub fn matches_naive(
        &self,
        dom: &Dom,
        node: NodeId,
        ctx: &StyleContext<'_>,
    ) -> Vec<&Candidate<'a>> {
        if !matches!(dom.node(node).data, NodeData::Element { .. }) {
            return Vec::new();
        }
        self.candidates
            .iter()
            .filter(|c| matches(dom, node, c.selector, ctx))
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
/// One `[attr…]` test against a node (M11.2).
///
/// The empty-value cases are the ones always got wrong: `[a^=""]`, `[a$=""]`
/// and `[a*=""]` match **nothing**, where a naive `starts_with("")` would
/// match everything — the difference between hiding one element and hiding the
/// page.
fn attribute_matches(dom: &Dom, node: NodeId, test: &crate::css::AttributeTest) -> bool {
    use crate::css::AttributeMatch;

    // `Dom::attr` already compares names ASCII-case-insensitively, which is
    // the HTML rule; values keep their case.
    let Some(actual) = dom.attr(node, &test.name) else {
        return false;
    };
    let Some((operator, want)) = &test.match_ else {
        return true;
    };
    match operator {
        AttributeMatch::Exact => actual == want,
        AttributeMatch::Word => actual.split_whitespace().any(|word| word == want),
        // `|=` is the language-subtag rule: `en` matches `en` and `en-GB`.
        AttributeMatch::Hyphen => {
            actual == want
                || actual
                    .strip_prefix(want.as_str())
                    .is_some_and(|rest| rest.starts_with('-'))
        }
        AttributeMatch::Prefix => !want.is_empty() && actual.starts_with(want.as_str()),
        AttributeMatch::Suffix => !want.is_empty() && actual.ends_with(want.as_str()),
        AttributeMatch::Substring => !want.is_empty() && actual.contains(want.as_str()),
    }
}

pub fn matches(dom: &Dom, node: NodeId, selector: &Selector, ctx: &StyleContext<'_>) -> bool {
    matches_parts(dom, node, &selector.parts, ctx)
}

/// `parts`' last compound must match `node`, and the rest must match some chain
/// of ancestors.
///
/// The ancestor search backtracks, and it has to: greedy nearest-match is wrong
/// for a mixed chain. `a > b span` against `<a><b><b><span>` matches only by
/// choosing the *outer* `b` — the nearest `b` fails the child combinator, and a
/// matcher that stops there reports no match. Depth is selector length, not
/// document depth, so the recursion is a handful of frames.
fn matches_parts(
    dom: &Dom,
    node: NodeId,
    parts: &[(Combinator, Compound)],
    ctx: &StyleContext<'_>,
) -> bool {
    let Some((combinator, rightmost)) = parts.last() else {
        return false;
    };
    if !compound_matches(dom, node, rightmost, ctx) {
        return false;
    }
    let rest = &parts[..parts.len() - 1];
    if rest.is_empty() {
        return true;
    }
    match combinator {
        Combinator::Child => {
            parent_element(dom, node).is_some_and(|parent| matches_parts(dom, parent, rest, ctx))
        }
        Combinator::Descendant => {
            let mut ancestor = parent_element(dom, node);
            while let Some(id) = ancestor {
                if matches_parts(dom, id, rest, ctx) {
                    return true;
                }
                ancestor = parent_element(dom, id);
            }
            false
        }
    }
}

fn compound_matches(dom: &Dom, node: NodeId, compound: &Compound, ctx: &StyleContext<'_>) -> bool {
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
    for test in &compound.attributes {
        if !attribute_matches(dom, node, test) {
            return false;
        }
    }
    compound.pseudo.iter().all(|pseudo| match pseudo {
        PseudoClass::Link => is_link(dom, tag, node) && !is_visited(dom, node, ctx),
        PseudoClass::Visited => is_link(dom, tag, node) && is_visited(dom, node, ctx),
        // CSS: the hovered element *and its ancestors* match `:hover`.
        PseudoClass::Hover => ctx.hover.is_some_and(|h| is_self_or_ancestor(dom, h, node)),
        // Unsupported is inert by definition (M4.1): never match.
        PseudoClass::Unsupported(_) => false,
    })
}

fn is_link(dom: &Dom, tag: &str, node: NodeId) -> bool {
    tag.eq_ignore_ascii_case("a") && dom.attr(node, "href").is_some()
}

fn is_visited(dom: &Dom, node: NodeId, ctx: &StyleContext<'_>) -> bool {
    let Some(href) = dom.attr(node, "href") else {
        return false;
    };
    let absolute = match ctx.base_url {
        Some(base) => net::resolve_url(base, href).unwrap_or_else(|| href.to_string()),
        None => href.to_string(),
    };
    ctx.visited.contains(&absolute)
}

/// Whether `descendant` is `ancestor` or nested under it (for `:hover`).
fn is_self_or_ancestor(dom: &Dom, descendant: NodeId, ancestor: NodeId) -> bool {
    let mut current = Some(descendant);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = dom.node(id).parent;
    }
    false
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
mod attribute_selectors {
    use super::matches;
    use crate::style::StyleContext;
    use crate::{css, html};

    /// Does `selector` match the element with `id=t` in `page`?
    fn hits(page: &str, selector: &str) -> bool {
        let dom = html::parse(page);
        let sheet = css::parse(&format!("{selector}{{}}"));
        let Some(rule) = sheet.rules.first() else {
            panic!("{selector} did not parse");
        };
        let target = (0..dom.node_count())
            .map(|i| crate::dom::NodeId(i as u32))
            .find(|&n| dom.attr(n, "id") == Some("t"))
            .expect("the fixture has a target");
        matches(&dom, target, &rule.selectors[0], &StyleContext::default())
    }

    const PAGE: &str = r#"<a id=t href="https://example.com/docs/a.html" class="one two"
        lang="en-GB" data-empty="" title="a b c">x</a>"#;

    #[test]
    fn presence_and_exact_value() {
        assert!(hits(PAGE, "[href]"));
        assert!(hits(PAGE, "a[href]"));
        assert!(!hits(PAGE, "[nope]"));
        assert!(hits(PAGE, "[lang=en-GB]"));
        assert!(hits(PAGE, r#"[lang="en-GB"]"#));
        assert!(!hits(PAGE, "[lang=en]"));
        // Names are ASCII-case-insensitive; values are not.
        assert!(hits(PAGE, "[HREF]"));
        assert!(!hits(PAGE, "[lang=EN-GB]"));
    }

    #[test]
    fn the_word_and_hyphen_operators_follow_css() {
        assert!(hits(PAGE, "[title~=b]"));
        assert!(!hits(PAGE, "[title~=a1]"));
        // `|=` is the language-subtag rule: the value, or the value then `-`.
        assert!(hits(PAGE, "[lang|=en]"));
        assert!(hits(PAGE, "[lang|=en-GB]"));
        assert!(!hits(PAGE, "[lang|=e]"));
    }

    #[test]
    fn substring_operators_and_the_empty_value_that_matches_nothing() {
        // A bare value must be a CSS identifier; anything with a dot or a
        // slash in it has to be quoted, which is CSS's rule and not ours.
        assert!(hits(PAGE, "[href^=https]"));
        // Quoted, because a bare CSS value must be an identifier and
        // `http://` is not one — the parser is right to refuse it unquoted.
        assert!(!hits(PAGE, r#"[href^="http://"]"#));
        assert!(hits(PAGE, r#"[href$=".html"]"#));
        assert!(!hits(PAGE, r#"[href$=".htm"]"#));
        assert!(hits(PAGE, r#"[href*="/docs/"]"#));
        assert!(!hits(PAGE, r#"[href*="/missing/"]"#));

        // The case always got wrong: an empty value matches **nothing**, where
        // `starts_with("")` would match everything — the difference between
        // hiding one element and hiding the page.
        assert!(!hits(PAGE, r#"[href^=""]"#));
        assert!(!hits(PAGE, r#"[href$=""]"#));
        assert!(!hits(PAGE, r#"[href*=""]"#));
        // Presence still matches an attribute whose value *is* empty.
        assert!(hits(PAGE, "[data-empty]"));
        assert!(hits(PAGE, r#"[data-empty=""]"#));
    }

    #[test]
    fn several_tests_on_one_compound_must_all_match() {
        assert!(hits(PAGE, "a[href][lang|=en].one"));
        assert!(!hits(PAGE, "a[href][lang|=fr]"));
    }

    #[test]
    fn an_attribute_selector_weighs_the_same_as_a_class() {
        let spec = |s: &str| css::parse(&format!("{s}{{}}")).rules[0].selectors[0].specificity();
        assert_eq!(spec("[href]"), spec(".cls"));
        assert_eq!(spec("a[href]"), spec("a.cls"));
        assert!(spec("#id") > spec("[href]"));
        assert!(spec("[href]") > spec("a"));
    }
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
        matches(
            &dom,
            node,
            &sheet.rules[0].selectors[0],
            &StyleContext::default(),
        )
    }

    fn empty_ctx() -> StyleContext<'static> {
        StyleContext::default()
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

        let ctx = empty_ctx();
        for tag in ["div", "p", "span"] {
            let node = find(&dom, tag);
            let fast: Vec<usize> = index
                .matches(&dom, node, &ctx)
                .iter()
                .map(|c| c.order)
                .collect();
            let naive: Vec<usize> = index
                .matches_naive(&dom, node, &ctx)
                .iter()
                .map(|c| c.order)
                .collect();
            assert_eq!(fast, naive, "{tag}");
        }
        // And the buckets really are doing work: the <span> never tests the
        // eight rules that cannot match it.
        let span = find(&dom, "span");
        assert_eq!(index.matches(&dom, span, &ctx).len(), 2);
    }

    #[test]
    fn a_duplicated_class_proposes_a_candidate_once() {
        let dom = html::parse("<p class='a a'>x</p>");
        let sheets = [parse(".a { color: red }")];
        let index = RuleIndex::build(&sheets.iter().collect::<Vec<_>>());
        assert_eq!(index.matches(&dom, find(&dom, "p"), &empty_ctx()).len(), 1);
    }

    #[test]
    fn hover_and_visited_match_through_context() {
        let dom = html::parse("<div><a href='https://x/'>t</a></div>");
        let a = find(&dom, "a");
        let div = find(&dom, "div");
        let mut visited = std::collections::HashSet::new();
        visited.insert("https://x/".into());
        let ctx = StyleContext {
            hover: Some(a),
            visited: &visited,
            base_url: Some("https://example.com/"),
        };
        let hover_sheet = parse("a:hover { color: red }");
        let div_hover = parse("div:hover { color: red }");
        let visited_sheet = parse("a:visited { color: red }");
        let link_sheet = parse("a:link { color: red }");
        assert!(matches(&dom, a, &hover_sheet.rules[0].selectors[0], &ctx));
        // Ancestors of the hover target also match `:hover`.
        assert!(matches(&dom, div, &div_hover.rules[0].selectors[0], &ctx));
        assert!(matches(&dom, a, &visited_sheet.rules[0].selectors[0], &ctx));
        // Visited is not :link.
        assert!(!matches(&dom, a, &link_sheet.rules[0].selectors[0], &ctx));
    }
}
