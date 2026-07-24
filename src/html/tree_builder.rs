//! Tree builder: a flat `Token` stream → the arena DOM. PLAN.md M2 is explicit
//! that we skip the WHATWG insertion-mode machinery and instead handle only the
//! error recovery the test ladder actually depends on. The goal is "a sane tree
//! a human would draw," not spec conformance.
//!
//! The whole engine is an open-elements stack plus a few small, declared recovery
//! tables. Each table has a comment tying it to the ladder page that motivates it.
//! Adoption-agency / formatting-element reconstruction (`<b><i></b></i>`) is
//! deliberately absent — if the ladder needs it, that is a report, not a feature.

use crate::dom::{Dom, NodeData, NodeId};

use super::tokenizer::{Token, tokenize};

/// Parse a full HTML string into a DOM.
pub fn parse(input: &str) -> Dom {
    build(tokenize(input))
}

/// Assemble a token stream into the arena DOM.
pub fn build(tokens: Vec<Token>) -> Dom {
    let mut b = TreeBuilder::new();
    for token in tokens {
        b.process(token);
    }
    b.finish();
    b.dom
}

/// Void elements never take children and never wait for an end tag (HN, danluu,
/// and every page use `<br>`/`<img>`/`<meta>`/`<link>`/`<hr>`). A self-closing
/// flag on any other start tag is honored the same way.
const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements routed into `<head>` while the head is still open — the metadata a
/// page front-loads (danluu/HN put `<title>`, `<meta>`, `<link>`, `<style>`, and
/// `<script>` before any flow content). Once `<body>` opens they fall through to
/// normal insertion so an inline `<style>` mid-page still lands where it's written.
const HEAD_TAGS: [&str; 7] = [
    "base", "link", "meta", "title", "style", "script", "noscript",
];

/// Block-level starts that implicitly close an open `<p>` — a `<p>` cannot
/// contain them, so `<p>text<div>` and `<p>a<p>b` both close the first `<p>`
/// first (danluu's prose relies on this).
const BLOCK: [&str; 30] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "dd",
    "dt",
    "fieldset",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

/// Inline elements the `<p>`-close scan walks past to find the `<p>` underneath
/// (a `<p><a>link<div>` still closes the `<p>`). Not exhaustive — just the ones
/// that show up between a `<p>` and its block interrupter on the ladder.
const INLINE: [&str; 20] = [
    "a", "abbr", "b", "cite", "code", "em", "font", "i", "kbd", "label", "mark", "q", "s", "small",
    "span", "strong", "sub", "sup", "u", "var",
];

/// Block-only containers whose all-whitespace text children are dropped, so the
/// tree a human reads isn't littered with the newlines between `<li>`s or table
/// rows. Whitespace anywhere else (inside `<p>`, `<pre>`, a cell) is kept as-is —
/// collapsing is layout's job (M3), not the parser's. Pinned by
/// `whitespace_between_block_tags_is_dropped`.
const WS_DROP_PARENTS: [&str; 13] = [
    "html", "head", "body", "ul", "ol", "dl", "table", "thead", "tbody", "tfoot", "tr", "colgroup",
    "select",
];

struct TreeBuilder {
    dom: Dom,
    /// Open elements, bottom-to-top. `<html>` sits at the bottom once created;
    /// the last entry is the current insertion point.
    open: Vec<NodeId>,
    html: Option<NodeId>,
    head: Option<NodeId>,
    body: Option<NodeId>,
    /// Set once `<body>` opens (or `</head>` is seen): the head no longer accepts
    /// metadata, and head tags fall through to normal insertion.
    head_done: bool,
}

impl TreeBuilder {
    fn new() -> TreeBuilder {
        TreeBuilder {
            dom: Dom::new_document(),
            open: Vec::new(),
            html: None,
            head: None,
            body: None,
            head_done: false,
        }
    }

    fn process(&mut self, token: Token) {
        match token {
            Token::Doctype(s) => {
                // Kept as a node under the document (before <html>); nothing reads
                // it yet, but the F1 tree should show it.
                self.dom.append_child(self.dom.root, NodeData::Doctype(s));
            }
            Token::Comment(s) => {
                let parent = self.insertion_parent();
                self.dom.append_child(parent, NodeData::Comment(s));
            }
            Token::Text(s) => self.insert_text(s),
            Token::StartTag {
                name,
                attrs,
                self_closing,
            } => self.insert_start(name, attrs, self_closing),
            Token::EndTag { name } => self.insert_end(&name),
        }
    }

    /// Current insertion point: the top of the open stack, or the document when
    /// nothing is open yet.
    fn insertion_parent(&self) -> NodeId {
        self.open.last().copied().unwrap_or(self.dom.root)
    }

    fn tag_of(&self, id: NodeId) -> &str {
        match &self.dom.node(id).data {
            NodeData::Element { tag, .. } => tag,
            _ => "",
        }
    }

    /// Is the insertion point one of the structural spine nodes (document / html
    /// / head)? Real flow content arriving here means it's time to open `<body>`.
    fn at_structural_root(&self) -> bool {
        let p = self.insertion_parent();
        p == self.dom.root || Some(p) == self.html || Some(p) == self.head
    }

    fn ensure_html(&mut self) -> NodeId {
        if let Some(h) = self.html {
            return h;
        }
        let h = self.dom.append_child(
            self.dom.root,
            NodeData::Element {
                tag: "html".into(),
                attrs: Vec::new(),
            },
        );
        self.open.push(h);
        self.html = Some(h);
        h
    }

    fn ensure_head(&mut self) -> NodeId {
        self.ensure_html();
        if let Some(h) = self.head {
            return h;
        }
        let html = self.html.unwrap();
        let h = self.dom.append_child(
            html,
            NodeData::Element {
                tag: "head".into(),
                attrs: Vec::new(),
            },
        );
        // Push the head so metadata inserts under it; it is popped when body opens.
        if !self.head_done {
            self.open.push(h);
        }
        self.head = Some(h);
        h
    }

    /// Open `<body>` if it isn't already: synthesize an (empty) head so the spine
    /// is complete, pop everything back down to `<html>`, then create and enter
    /// the body. Idempotent — a later `<body>` tag or stray flow content is a
    /// no-op once the body exists.
    fn open_body(&mut self) {
        if self.body.is_some() {
            return;
        }
        self.ensure_head();
        let html = self.html.unwrap();
        while let Some(&top) = self.open.last() {
            if top == html {
                break;
            }
            self.open.pop();
        }
        self.head_done = true;
        let body = self.dom.append_child(
            html,
            NodeData::Element {
                tag: "body".into(),
                attrs: Vec::new(),
            },
        );
        self.open.push(body);
        self.body = Some(body);
    }

    fn insert_text(&mut self, s: String) {
        if s.trim().is_empty() {
            // All-whitespace: drop it in the structural spine and in block-only
            // containers (see WS_DROP_PARENTS); keep it everywhere else.
            if self.at_structural_root() {
                return;
            }
            let parent = self.insertion_parent();
            if WS_DROP_PARENTS.contains(&self.tag_of(parent)) {
                return;
            }
        } else if self.at_structural_root() {
            // Real text before/around the spine belongs in flow.
            self.open_body();
        }
        let parent = self.insertion_parent();
        self.dom.append_child(parent, NodeData::Text(s));
    }

    fn insert_start(&mut self, name: String, attrs: Vec<(String, String)>, self_closing: bool) {
        match name.as_str() {
            "html" => {
                self.ensure_html();
                return;
            }
            "head" => {
                self.ensure_head();
                return;
            }
            "body" => {
                self.open_body();
                return;
            }
            _ => {}
        }

        // Recovery: close whatever this start tag implicitly ends.
        self.apply_implied_close(&name);

        // Placement: head metadata into <head> while it's open, otherwise flow.
        if !self.head_done && HEAD_TAGS.contains(&name.as_str()) {
            self.ensure_head();
        } else if self.at_structural_root() {
            self.open_body();
        }

        let parent = self.insertion_parent();
        let id = self.dom.append_child(
            parent,
            NodeData::Element {
                tag: name.clone(),
                attrs,
            },
        );
        if !self_closing && !VOID.contains(&name.as_str()) {
            self.open.push(id);
        }
    }

    /// The recovery table. Each rule pops the elements a new start tag implicitly
    /// closes, scoped so nested lists/tables don't over-close.
    fn apply_implied_close(&mut self, name: &str) {
        if BLOCK.contains(&name) {
            self.close_p();
        }
        match name {
            // <li>a<li>b — the second <li> closes the first, but a nested <ul>/<ol>
            // is a fresh scope (Hacker News comment trees, Wikipedia contents).
            "li" => self.close_scoped("li", &["ul", "ol"]),
            // <dt>/<dd> close each other, bounded by the enclosing <dl>.
            "dt" | "dd" => self.close_scoped_any(&["dt", "dd"], &["dl"]),
            // Table cells and rows close their siblings (HN's layout is nested
            // tables); a cell stops at its row, a row at its table.
            "td" | "th" => self.close_scoped_any(&["td", "th"], &["tr", "table"]),
            "tr" => self.close_scoped("tr", &["table"]),
            _ => {}
        }
    }

    /// Pop an open `<p>`, walking past inline elements to reach it and stopping at
    /// any block boundary (so we never reach across a container).
    fn close_p(&mut self) {
        for i in (0..self.open.len()).rev() {
            let tag = self.tag_of(self.open[i]);
            if tag == "p" {
                self.open.truncate(i);
                return;
            }
            if !INLINE.contains(&tag) {
                return;
            }
        }
    }

    fn close_scoped(&mut self, target: &str, boundary: &[&str]) {
        self.close_scoped_any(&[target], boundary);
    }

    /// Pop down to and including the nearest open element whose tag is in
    /// `targets`, but give up if a `boundary` tag is hit first (a new scope).
    fn close_scoped_any(&mut self, targets: &[&str], boundary: &[&str]) {
        for i in (0..self.open.len()).rev() {
            let tag = self.tag_of(self.open[i]);
            if targets.contains(&tag) {
                self.open.truncate(i);
                return;
            }
            if boundary.contains(&tag) {
                return;
            }
        }
    }

    /// At EOF every still-open element is closed implicitly. A page that created
    /// a spine but never any flow content (e.g. only `<script>` in `<head>`) still
    /// gets an empty `<body>`, so the tree a human draws always has one.
    fn finish(&mut self) {
        if self.html.is_some() && self.body.is_none() {
            self.open_body();
        }
        self.open.clear();
    }

    fn insert_end(&mut self, name: &str) {
        match name {
            // Structural end tags don't tear the spine down; trailing content
            // still belongs in the body, and EOF closes everything anyway.
            "body" | "html" => {}
            "head" => {
                if let Some(head) = self.head {
                    if let Some(i) = self.open.iter().rposition(|&id| id == head) {
                        self.open.truncate(i);
                    }
                    self.head_done = true;
                }
            }
            _ => {
                // Pop to the nearest matching open element; a stray end tag with
                // no match is ignored (not an error) — real pages have them.
                if let Some(i) = self.open.iter().rposition(|&id| self.tag_of(id) == name) {
                    self.open.truncate(i);
                }
            }
        }
    }
}

/// Render a DOM as an indented tree, one node per line, two spaces per depth.
/// Small enough for snapshot tests to read; M2.3's F1 view reuses this shape.
pub fn debug_tree(dom: &Dom) -> String {
    let mut out = String::new();
    write_node(dom, dom.root, 0, &mut out);
    out
}

fn write_node(dom: &Dom, id: NodeId, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    match &dom.node(id).data {
        NodeData::Document => out.push_str("#document"),
        NodeData::Doctype(s) => out.push_str(&format!("<!doctype {s}>")),
        NodeData::Comment(s) => out.push_str(&format!("<!--{s}-->")),
        NodeData::Text(s) => out.push_str(&format!("#text {:?}", s)),
        NodeData::Element { tag, attrs } => {
            out.push('<');
            out.push_str(tag);
            for (k, v) in attrs {
                out.push_str(&format!(" {k}=\"{v}\""));
            }
            out.push('>');
        }
    }
    out.push('\n');
    for child in dom.children(id) {
        write_node(dom, child, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(html: &str) -> String {
        debug_tree(&parse(html))
    }

    #[test]
    fn spine_is_synthesized_when_absent() {
        assert_eq!(
            tree("hi"),
            "\
#document
  <html>
    <head>
    <body>
      #text \"hi\"
"
        );
    }

    #[test]
    fn head_and_body_placement() {
        // <title> lands in head, flow text opens body.
        assert_eq!(
            tree("<title>T</title>hi"),
            "\
#document
  <html>
    <head>
      <title>
        #text \"T\"
    <body>
      #text \"hi\"
"
        );
    }

    #[test]
    fn sibling_paragraphs_not_nested() {
        assert_eq!(
            tree("<p>one<p>two"),
            "\
#document
  <html>
    <head>
    <body>
      <p>
        #text \"one\"
      <p>
        #text \"two\"
"
        );
    }

    #[test]
    fn list_items_are_siblings() {
        assert_eq!(
            tree("<ul><li>a<li>b</ul>"),
            "\
#document
  <html>
    <head>
    <body>
      <ul>
        <li>
          #text \"a\"
        <li>
          #text \"b\"
"
        );
    }

    #[test]
    fn nested_lists_keep_their_nesting() {
        // The inner <li> must attach to the inner <ul>, not close the outer one.
        assert_eq!(
            tree("<ul><li>a<ul><li>b</ul></ul>"),
            "\
#document
  <html>
    <head>
    <body>
      <ul>
        <li>
          #text \"a\"
          <ul>
            <li>
              #text \"b\"
"
        );
    }

    #[test]
    fn void_elements_adopt_no_children() {
        assert_eq!(
            tree("<br>after"),
            "\
#document
  <html>
    <head>
    <body>
      <br>
      #text \"after\"
"
        );
        assert_eq!(
            tree("<img src=x>after"),
            "\
#document
  <html>
    <head>
    <body>
      <img src=\"x\">
      #text \"after\"
"
        );
    }

    #[test]
    fn script_is_one_element_with_one_text_child() {
        assert_eq!(
            tree("<script>if (a<b){}</script>"),
            "\
#document
  <html>
    <head>
      <script>
        #text \"if (a<b){}\"
    <body>
"
        );
    }

    #[test]
    fn stray_end_tag_is_ignored() {
        assert_eq!(
            tree("</div>hi"),
            "\
#document
  <html>
    <head>
    <body>
      #text \"hi\"
"
        );
    }

    #[test]
    fn unclosed_tags_close_at_eof() {
        assert_eq!(
            tree("<div><span>x"),
            "\
#document
  <html>
    <head>
    <body>
      <div>
        <span>
          #text \"x\"
"
        );
    }

    #[test]
    fn whitespace_between_block_tags_is_dropped() {
        // Newlines between <li>s don't survive as text children of <ul>.
        assert_eq!(
            tree("<ul>\n  <li>a\n  <li>b\n</ul>"),
            "\
#document
  <html>
    <head>
    <body>
      <ul>
        <li>
          #text \"a\\n  \"
        <li>
          #text \"b\\n\"
"
        );
    }

    #[test]
    fn doctype_and_comment_become_nodes() {
        assert_eq!(
            tree("<!doctype html><!-- c -->hi"),
            "\
#document
  <!doctype html>
  <!-- c -->
  <html>
    <head>
    <body>
      #text \"hi\"
"
        );
    }
}
