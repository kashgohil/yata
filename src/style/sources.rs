//! Where a page's CSS comes from: `<style>` blocks and `<link
//! rel=stylesheet>` hrefs, in document order (M4.3).
//!
//! A pure DOM walk — it reads the tree and returns descriptions, it does not
//! parse CSS, resolve URLs or fetch anything. `App` decides what to do with
//! each source; `net/` does the fetching.
//!
//! One ordered list rather than two, because cascade order *is* document
//! order: a `<style>` written after a `<link>` beats it on ties, and two lists
//! would lose the interleaving that decides who wins.

use crate::dom::{Dom, NodeData, NodeId};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// The text of a `<style>` element, already in hand.
    Inline(String),
    /// A `<link rel=stylesheet>`'s href, exactly as the page wrote it —
    /// resolving it against the page URL is `net::resolve_url`'s job.
    Link(String),
}

/// Every stylesheet the document asks for, in the order it asks.
pub fn sources(dom: &Dom) -> Vec<Source> {
    let mut out = Vec::new();
    collect(dom, dom.root, &mut out);
    out
}

fn collect(dom: &Dom, node: NodeId, out: &mut Vec<Source>) {
    if let NodeData::Element { tag, .. } = &dom.node(node).data {
        match tag.as_str() {
            "style" => {
                // `<style>` content is raw text to the tokenizer, so this is
                // one text child in practice; concatenating is what makes it
                // not depend on that.
                let mut css = String::new();
                for child in dom.children(node) {
                    if let NodeData::Text(text) = &dom.node(child).data {
                        css.push_str(text);
                    }
                }
                out.push(Source::Inline(css));
                return;
            }
            "link" => {
                if is_stylesheet(dom, node)
                    && let Some(href) = dom.attr(node, "href").filter(|h| !h.trim().is_empty())
                {
                    out.push(Source::Link(href.to_string()));
                }
                return;
            }
            _ => {}
        }
    }
    for child in dom.children(node) {
        collect(dom, child, out);
    }
}

/// `rel` is a space-separated token list, ASCII-case-insensitive.
/// `rel="alternate stylesheet"` is deliberately *not* a stylesheet: those are
/// opt-in alternates a browser offers but does not apply.
fn is_stylesheet(dom: &Dom, node: NodeId) -> bool {
    let Some(rel) = dom.attr(node, "rel") else {
        return false;
    };
    let tokens: Vec<&str> = rel.split_whitespace().collect();
    tokens.iter().any(|t| t.eq_ignore_ascii_case("stylesheet"))
        && !tokens.iter().any(|t| t.eq_ignore_ascii_case("alternate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn of(html_src: &str) -> Vec<Source> {
        sources(&html::parse(html_src))
    }

    fn link(href: &str) -> Source {
        Source::Link(href.to_string())
    }

    fn inline(css: &str) -> Source {
        Source::Inline(css.to_string())
    }

    #[test]
    fn sources_come_out_in_document_order() {
        // The interleaving is the point: the second inline block cascades
        // after the link, and a two-list API would lose that.
        assert_eq!(
            of("<head><style>a{}</style><link rel=stylesheet href=x.css><style>b{}</style></head>"),
            vec![inline("a{}"), link("x.css"), inline("b{}")]
        );
    }

    #[test]
    fn rel_is_a_case_insensitive_token_list() {
        assert_eq!(
            of("<link REL='STYLESHEET' href=x.css>"),
            vec![link("x.css")]
        );
        assert_eq!(
            of("<link rel='preload stylesheet' href=x.css>"),
            vec![link("x.css")]
        );
        // Alternates are offered, not applied.
        assert_eq!(of("<link rel='alternate stylesheet' href=x.css>"), vec![]);
        assert_eq!(of("<link rel=icon href=favicon.ico>"), vec![]);
        assert_eq!(of("<link href=x.css>"), vec![]);
    }

    #[test]
    fn a_link_without_a_usable_href_is_skipped() {
        assert_eq!(of("<link rel=stylesheet>"), vec![]);
        assert_eq!(of("<link rel=stylesheet href=''>"), vec![]);
        assert_eq!(of("<link rel=stylesheet href='   '>"), vec![]);
    }

    #[test]
    fn an_empty_style_block_is_still_a_source() {
        // It contributes no rules, but dropping it here would mean the slot
        // numbering no longer matches document order.
        assert_eq!(of("<style></style>"), vec![inline("")]);
    }

    #[test]
    fn the_hn_fixture_asks_for_exactly_one_sheet() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/news.ycombinator.com.html"
        ));
        assert_eq!(
            sources(&html::parse(fixture)),
            vec![link("news.css?3HzzJW9s7JrtYzwqKDTI")]
        );
    }

    #[test]
    fn wikipedia_asks_for_its_inline_blocks_and_its_load_php_sheets() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        let found = sources(&html::parse(fixture));
        let links = found
            .iter()
            .filter(|s| matches!(s, Source::Link(_)))
            .count();
        let inlines = found.len() - links;
        // 21 inline blocks (the count M4.1's parser test pins) and the
        // load.php sheets the page links.
        assert_eq!(inlines, 21);
        assert!(links >= 2, "expected the load.php stylesheets, got {links}");
        // The first thing the document asks for is a linked sheet, before any
        // of its inline blocks — order this walk has to preserve.
        assert!(matches!(found[0], Source::Link(_)));
    }
}
