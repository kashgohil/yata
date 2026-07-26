//! CSS front end: stylesheet text → `Stylesheet` (PLAN.md M4).
//!
//! This module is the syntax half of M4 and knows nothing about meaning. It
//! parses `frobnicate: sideways` as readily as `color: red` and hands both on
//! as strings; which properties exist, what `#348` means and which declaration
//! wins are all questions for `style/` (M4.2), where computed values live.
//! Keeping the seam there is what lets this half be finished and tested against
//! real fixture CSS with nothing downstream of it yet.
//!
//! Nothing consumes a `Stylesheet` yet: `<style>` blocks and `<link>` sheets
//! reach the engine in M4.3.

mod parser;
mod tokenizer;

pub use parser::{parse, parse_declarations};
pub use tokenizer::{Token, Tokenizer, tokenize};

/// A parsed stylesheet: rules in source order, which is also cascade order for
/// equal specificity (M4.2).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

/// One `{ }` block and the selectors that open it. A comma-separated list stays
/// **one** rule with several selectors rather than a copy of the block per
/// selector: the cascade needs the specificity of the selector that actually
/// matched, and duplicating the declarations would lose which one that was.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

/// `name: value` — `value` is the raw source text, trimmed, with `!important`
/// lifted into the flag. Values are sliced out of the stylesheet rather than
/// rebuilt from tokens, so they keep their original spelling; the one thing
/// that survives into them uninterpreted is a comment written *inside* a value
/// (`color: red /*x*/ blue`), which M4.2's value parser will see as written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

/// A complex selector: compounds left to right, each with the combinator that
/// joins it to the one before. The first part's combinator is `Descendant` and
/// carries no meaning — matching (M4.2) walks these right to left and stops at
/// the first part.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Selector {
    pub parts: Vec<(Combinator, Compound)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Combinator {
    /// `div p`
    Descendant,
    /// `div > p`
    Child,
}

/// Simple selectors with no combinator between them: `div.foo#bar:hover`. An
/// all-`None`/empty compound is the universal selector `*`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Compound {
    /// Type selector, ASCII-lowercased (HTML type selectors are
    /// case-insensitive; class and id names are not, and keep their case).
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub pseudo: Vec<PseudoClass>,
}

/// PLAN.md M4 asks for `:hover`/`:visited` stubs; `:link` is here because
/// `example.com`'s whole sheet hangs off `a:link,a:visited{color:#348}` and
/// dropping it would drop the page's only colour.
///
/// Matching semantics are M4.2's to implement, but they are fixed here so both
/// halves agree: `Link` matches an `<a href>`, `Visited` never matches until
/// there is history to consult (M6), `Hover` matches the hovered element (M6;
/// nothing hovers yet), and a compound holding an `Unsupported` pseudo **never
/// matches**. Unsupported is inert, never silently promoted to matching
/// everything — `p:nth-child(2)` must not paint every paragraph.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PseudoClass {
    Hover,
    Visited,
    Link,
    Unsupported(String),
}

impl Selector {
    /// CSS specificity as (ids, classes + pseudo-classes, type selectors),
    /// compared lexicographically by the cascade. `*` contributes nothing.
    /// Saturating, because a selector with 65 536 classes is someone's fuzzer,
    /// not a page, and wrapping there would silently invert a comparison.
    pub fn specificity(&self) -> (u16, u16, u16) {
        let mut spec = (0u16, 0u16, 0u16);
        for (_, compound) in &self.parts {
            spec.0 = spec.0.saturating_add(u16::from(compound.id.is_some()));
            spec.1 = spec
                .1
                .saturating_add(clamp_u16(compound.classes.len() + compound.pseudo.len()));
            spec.2 = spec.2.saturating_add(u16::from(compound.tag.is_some()));
        }
        spec
    }
}

fn clamp_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(src: &str) -> Selector {
        let sheet = parse(&format!("{src} {{ color: red }}"));
        sheet.rules[0].selectors[0].clone()
    }

    #[test]
    fn specificity_counts_ids_classes_and_tags() {
        assert_eq!(selector("#a").specificity(), (1, 0, 0));
        assert_eq!(selector(".a.b").specificity(), (0, 2, 0));
        assert_eq!(selector("div p").specificity(), (0, 0, 2));
        assert_eq!(selector("a:hover").specificity(), (0, 1, 1));
        assert_eq!(selector("*").specificity(), (0, 0, 0));
        assert_eq!(selector("ul#nav > li.item a").specificity(), (1, 1, 3));
    }

    #[test]
    fn specificity_orders_the_way_the_cascade_reads_it() {
        // One id beats any pile of classes: the comparison is lexicographic on
        // the tuple, not a sum, which is exactly why the tuple is the type.
        assert!(selector("#a").specificity() > selector(".a.b.c.d").specificity());
        assert!(selector(".a").specificity() > selector("div span p").specificity());
    }
}
