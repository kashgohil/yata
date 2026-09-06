//! Pure reader-root analysis and projection over the live DOM arena (M11.23).
//!
//! The analyzer keeps node identity: its result is one arena-sized membership
//! vector, never a copied or rewritten document.  Candidate accounting is a
//! single post-order pass with two bounded summaries per node.  The second
//! summary represents being below an `<article>` and is what lets an article's
//! own header/footer survive without admitting page chrome.

use crate::dom::{Dom, NodeData, NodeId};
use crate::style::Styles;
use unicode_width::UnicodeWidthChar;

const CELL_CAP: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub text_cells: u64,
    pub link_cells: u64,
    pub prose_blocks: usize,
    pub included_nodes: usize,
    pub score: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderView {
    pub root: NodeId,
    included: Vec<bool>,
    pub metrics: Metrics,
}

impl ReaderView {
    pub fn includes(&self, node: NodeId) -> bool {
        self.included.get(node.0 as usize).copied().unwrap_or(false)
    }

    pub fn membership(&self) -> &[bool] {
        &self.included
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderError {
    NoProseRoot,
}

#[derive(Clone, Copy, Debug, Default)]
struct Run {
    cells: u64,
    nonblank: bool,
    starts_space: bool,
    ends_space: bool,
}

impl Run {
    fn append(&mut self, rhs: Run) {
        if !rhs.nonblank && rhs.cells == 0 {
            return;
        }
        if self.cells != 0 && rhs.cells != 0 && self.ends_space && rhs.starts_space {
            self.cells = self.cells.saturating_sub(1);
        }
        if self.cells == 0 {
            self.starts_space = rhs.starts_space;
        }
        self.cells = self.cells.saturating_add(rhs.cells).min(CELL_CAP);
        self.ends_space = rhs.ends_space;
        self.nonblank |= rhs.nonblank;
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Summary {
    text: Run,
    link: Run,
    prose_blocks: usize,
    included_nodes: usize,
}

impl Summary {
    fn append(&mut self, rhs: Summary, cap: usize) {
        self.text.append(rhs.text);
        self.link.append(rhs.link);
        self.prose_blocks = self.prose_blocks.saturating_add(rhs.prose_blocks).min(cap);
        self.included_nodes = self
            .included_nodes
            .saturating_add(rhs.included_nodes)
            .min(cap);
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    root: NodeId,
    metrics: Metrics,
    bonus: u64,
    order: usize,
}

/// Select the strongest prose root and build its arena-identity projection.
pub fn analyze(dom: &Dom, styles: &Styles) -> Result<ReaderView, ReaderError> {
    if styles.node_count() != dom.node_count() {
        return Err(ReaderError::NoProseRoot);
    }

    // Pre-order once, then consume in reverse for a non-recursive post-order.
    // The DOM itself caps depth, but the analyzer does not need that promise.
    let mut order = Vec::with_capacity(dom.node_count());
    let mut stack = vec![(dom.root, false, false)];
    let mut under_link = vec![false; dom.node_count()];
    let mut candidate_blocked = vec![false; dom.node_count()];
    while let Some((id, linked, blocked)) = stack.pop() {
        order.push(id);
        let linked = linked || tag(dom, id) == Some("a");
        under_link[id.0 as usize] = linked;
        candidate_blocked[id.0 as usize] = blocked;
        let blocks_descendants = blocked
            || styles.get(id).hidden_by_ua
            || matches!(
                tag(dom, id),
                Some(
                    "nav"
                        | "aside"
                        | "form"
                        | "button"
                        | "input"
                        | "textarea"
                        | "select"
                        | "option"
                )
            )
            || dom.attr(id, "role").is_some_and(pruned_role);
        let children: Vec<_> = dom.children(id).collect();
        for child in children.into_iter().rev() {
            stack.push((child, linked, blocks_descendants));
        }
    }

    // [outside-article, inside-article]. Header/footer are the only nodes for
    // which the answer differs; carrying both avoids rescanning per candidate.
    let mut summaries = vec![[Summary::default(); 2]; dom.node_count()];
    let mut best: Option<Candidate> = None;
    for (document_order, &id) in order.iter().enumerate().rev() {
        for context in [false, true] {
            let summary = summarize_node(dom, styles, &summaries, &under_link, id, context);
            summaries[id.0 as usize][usize::from(context)] = summary;
        }

        let Some(kind) = candidate_kind(dom, id) else {
            continue;
        };
        if candidate_blocked[id.0 as usize] {
            continue;
        }
        let context = kind == CandidateKind::Article;
        let s = summaries[id.0 as usize][usize::from(context)];
        let bonus = kind.bonus();
        if !eligible(kind, s) {
            continue;
        }
        let score = s
            .text
            .cells
            .saturating_sub(s.link.cells)
            .saturating_add(80u64.saturating_mul(s.prose_blocks as u64))
            .saturating_add(bonus);
        let candidate = Candidate {
            root: id,
            metrics: Metrics {
                text_cells: s.text.cells,
                link_cells: s.link.cells,
                prose_blocks: s.prose_blocks,
                included_nodes: s.included_nodes,
                score,
            },
            bonus,
            order: document_order,
        };
        if best.is_none_or(|old| better(candidate, old)) {
            best = Some(candidate);
        }
    }

    let best = best.ok_or(ReaderError::NoProseRoot)?;
    Ok(ReaderView {
        root: best.root,
        included: project(dom, styles, best.root),
        metrics: best.metrics,
    })
}

fn summarize_node(
    dom: &Dom,
    styles: &Styles,
    summaries: &[[Summary; 2]],
    under_link: &[bool],
    id: NodeId,
    article_context: bool,
) -> Summary {
    if pruned(dom, styles, id, article_context) {
        return Summary::default();
    }
    let cap = dom.node_count();
    let mut out = Summary {
        included_nodes: 1,
        ..Summary::default()
    };
    if let NodeData::Text(text) = &dom.node(id).data {
        out.text = normalized(text);
        if under_link[id.0 as usize] {
            out.link = out.text;
        }
    }
    let child_context = article_context || tag(dom, id) == Some("article");
    for child in dom.children(id) {
        out.append(summaries[child.0 as usize][usize::from(child_context)], cap);
    }
    if prose_tag(tag(dom, id)) && out.text.nonblank {
        out.prose_blocks = out.prose_blocks.saturating_add(1).min(cap);
    }
    out
}

fn normalized(text: &str) -> Run {
    let mut out = Run::default();
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.cells = out.cells.saturating_add(1).min(CELL_CAP);
                if out.cells == 1 {
                    out.starts_space = true;
                }
                in_space = true;
            }
        } else if !c.is_control() {
            out.cells = out
                .cells
                .saturating_add(c.width().unwrap_or(0) as u64)
                .min(CELL_CAP);
            out.nonblank = true;
            in_space = false;
        }
    }
    out.ends_space = in_space && out.cells != 0;
    out
}

fn project(dom: &Dom, styles: &Styles, root: NodeId) -> Vec<bool> {
    let mut included = vec![false; dom.node_count()];
    let mut spine = Some(root);
    while let Some(id) = spine {
        included[id.0 as usize] = true;
        spine = dom.node(id).parent;
    }
    let initial_context = tag(dom, root) == Some("article");
    let mut stack = vec![(root, initial_context)];
    while let Some((id, article_context)) = stack.pop() {
        if pruned(dom, styles, id, article_context) {
            continue;
        }
        included[id.0 as usize] = true;
        let child_context = article_context || tag(dom, id) == Some("article");
        let children: Vec<_> = dom.children(id).collect();
        for child in children.into_iter().rev() {
            stack.push((child, child_context));
        }
    }
    included
}

fn pruned(dom: &Dom, styles: &Styles, id: NodeId, article_context: bool) -> bool {
    if styles.get(id).hidden_by_ua {
        return true;
    }
    let Some(tag) = tag(dom, id) else {
        return false;
    };
    if matches!(
        tag,
        "nav" | "aside" | "form" | "button" | "input" | "textarea" | "select" | "option"
    ) {
        return true;
    }
    if matches!(tag, "header" | "footer") && !article_context {
        return true;
    }
    dom.attr(id, "role").is_some_and(pruned_role)
}

fn pruned_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "navigation" | "complementary" | "banner" | "contentinfo" | "dialog"
    )
}

fn tag(dom: &Dom, id: NodeId) -> Option<&str> {
    match &dom.node(id).data {
        NodeData::Element { tag, .. } => Some(tag),
        _ => None,
    }
}

fn prose_tag(tag: Option<&str>) -> bool {
    matches!(
        tag,
        Some(
            "p" | "blockquote"
                | "pre"
                | "dd"
                | "figcaption"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
        )
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Article,
    Main,
    Section,
    Generic,
}

impl CandidateKind {
    fn bonus(self) -> u64 {
        match self {
            CandidateKind::Article => 400,
            CandidateKind::Main => 300,
            CandidateKind::Section => 100,
            CandidateKind::Generic => 0,
        }
    }
}

fn candidate_kind(dom: &Dom, id: NodeId) -> Option<CandidateKind> {
    match tag(dom, id)? {
        "article" => Some(CandidateKind::Article),
        "main" => Some(CandidateKind::Main),
        "section" => Some(CandidateKind::Section),
        "div" | "body" => Some(CandidateKind::Generic),
        _ => None,
    }
}

fn eligible(kind: CandidateKind, s: Summary) -> bool {
    match kind {
        CandidateKind::Article | CandidateKind::Main => s.text.cells >= 40 || s.prose_blocks >= 1,
        CandidateKind::Section | CandidateKind::Generic => {
            s.text.cells >= 200
                && s.prose_blocks >= 2
                && s.link.cells.saturating_mul(2) <= s.text.cells
        }
    }
}

fn better(new: Candidate, old: Candidate) -> bool {
    (
        new.metrics.score,
        new.bonus,
        std::cmp::Reverse(new.metrics.included_nodes),
        std::cmp::Reverse(new.order),
    ) > (
        old.metrics.score,
        old.bonus,
        std::cmp::Reverse(old.metrics.included_nodes),
        std::cmp::Reverse(old.order),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{html, style};

    fn analyze_html(input: &str) -> Result<(Dom, ReaderView), ReaderError> {
        let dom = html::parse(input);
        let styles = style::style_tree(&dom, &[]);
        let result = analyze(&dom, &styles)?;
        Ok((dom, result))
    }

    fn id(dom: &Dom, value: &str) -> NodeId {
        (0..dom.node_count())
            .map(|n| NodeId(n as u32))
            .find(|&node| dom.attr(node, "id") == Some(value))
            .unwrap()
    }

    #[test]
    fn semantic_short_article_qualifies_and_exact_score_is_stable() {
        let (dom, view) = analyze_html("<article id=a><h1>Hi</h1><p>Small.</p></article>").unwrap();
        assert_eq!(view.root, id(&dom, "a"));
        assert_eq!(view.metrics.text_cells, 8);
        assert_eq!(view.metrics.link_cells, 0);
        assert_eq!(view.metrics.prose_blocks, 2);
        assert_eq!(view.metrics.score, 568);
    }

    #[test]
    fn prunes_chrome_controls_roles_and_ua_inert_content() {
        let (dom, view) = analyze_html(
            "<header id=page>chrome</header><article id=a><header id=own><h1>Title</h1></header>\
             <p>Shown prose.</p><aside id=aside><p>aside</p></aside><form id=form><p>form</p></form>\
             <div role=navigation id=role><p>role</p></div><script id=script>source</script>\
             <footer id=own-foot>notes</footer></article><footer id=page-foot>chrome</footer>",
        )
        .unwrap();
        for kept in ["a", "own", "own-foot"] {
            assert!(view.includes(id(&dom, kept)), "{kept} was pruned");
        }
        for cut in ["page", "aside", "form", "role", "script", "page-foot"] {
            assert!(!view.includes(id(&dom, cut)), "{cut} entered projection");
        }
    }

    #[test]
    fn author_hidden_prose_is_still_eligible() {
        let dom = html::parse(
            "<style>article{display:none}</style><article id=a><p>secret</p></article>",
        );
        let sheet = crate::css::parse("article{display:none}");
        let styles = style::style_tree(&dom, &[&sheet]);
        let view = analyze(&dom, &styles).unwrap();
        assert_eq!(view.root, id(&dom, "a"));
    }

    #[test]
    fn a_link_directory_is_refused() {
        let links = "<p><a href=x>abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz</a></p>";
        let input = format!("<div>{links}{links}</div>");
        let dom = html::parse(&input);
        let styles = style::style_tree(&dom, &[]);
        assert_eq!(analyze(&dom, &styles), Err(ReaderError::NoProseRoot));
    }

    #[test]
    fn score_then_semantics_specificity_and_document_order_break_ties() {
        let candidate = |root, score, bonus, nodes, order| Candidate {
            root: NodeId(root),
            metrics: Metrics {
                score,
                included_nodes: nodes,
                ..Metrics::default()
            },
            bonus,
            order,
        };
        assert!(better(
            candidate(1, 11, 0, 20, 9),
            candidate(2, 10, 400, 1, 0)
        ));
        assert!(better(
            candidate(1, 10, 400, 20, 9),
            candidate(2, 10, 300, 1, 0)
        ));
        assert!(better(candidate(1, 10, 0, 2, 9), candidate(2, 10, 0, 3, 0)));
        assert!(better(candidate(1, 10, 0, 2, 3), candidate(2, 10, 0, 2, 4)));

        // Equal nested generic candidates: the more specific one wins.
        let text = "x".repeat(200);
        let input = format!("<div id=outer><div id=early><p>{text}</p><p>y</p></div></div>");
        let (dom, view) = analyze_html(&input).unwrap();
        assert_eq!(view.root, id(&dom, "early"));
    }

    #[test]
    fn article_below_pruned_subtree_is_not_a_candidate() {
        let dom = html::parse("<nav><article><p>hidden</p></article></nav>");
        let styles = style::style_tree(&dom, &[]);
        assert_eq!(analyze(&dom, &styles), Err(ReaderError::NoProseRoot));
    }

    #[test]
    fn unknown_elements_are_neutral_containers() {
        let (dom, view) =
            analyze_html("<article id=a><x-card><p>words</p></x-card></article>").unwrap();
        assert!(view.includes(id(&dom, "a")));
        let custom = (0..dom.node_count())
            .map(|n| NodeId(n as u32))
            .find(|&n| tag(&dom, n) == Some("x-card"))
            .unwrap();
        assert!(view.includes(custom));
    }

    #[test]
    fn large_arena_is_deterministic_and_saturates() {
        let mut dom = Dom::new_document();
        let article = dom.append_child(
            dom.root,
            NodeData::Element {
                tag: "article".into(),
                attrs: vec![],
            },
        );
        for _ in 0..25_000 {
            dom.append_child(article, NodeData::Text("x".repeat(100).into()));
        }
        let styles = style::style_tree(&dom, &[]);
        let a = analyze(&dom, &styles).unwrap();
        let b = analyze(&dom, &styles).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.metrics.text_cells, CELL_CAP);
        assert_eq!(a.membership().len(), dom.node_count());
    }
}
