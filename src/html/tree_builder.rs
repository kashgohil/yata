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

/// Parse `input` as a **fragment**: the nodes it would contribute to a parent,
/// rather than a document. Returns the scratch arena they were built in and
/// the roots to adopt out of it — a caller copies them into the real tree
/// through `Dom`'s write API (M10.3), which is the only way nodes are ever
/// created.
///
/// This is the honest cheap implementation: parse as a document, take
/// `<body>`'s children. A browser's fragment algorithm is *context-sensitive*
/// — it parses with the target element as the insertion context, so a run of
/// `<td>`s parsed into a `<div>` loses its cells and keeps their text. Ours
/// does not know the context, so it keeps whatever the document parser made.
/// `innerHTML` is the only caller and the difference only shows for table
/// parts written outside a table; see the M10.5 deviations.
pub fn parse_fragment(input: &str) -> (Dom, Vec<NodeId>) {
    let dom = parse(input);
    let roots = match find_body(&dom) {
        Some(body) => dom.children(body).collect(),
        None => Vec::new(),
    };
    (dom, roots)
}

fn find_body(dom: &Dom) -> Option<NodeId> {
    let html = dom.children(dom.root).find(
        |&node| matches!(&dom.node(node).data, NodeData::Element { tag, .. } if tag == "html"),
    )?;
    dom.children(html).find(
        |&node| matches!(&dom.node(node).data, NodeData::Element { tag, .. } if tag == "body"),
    )
}

/// Serialize a node's children as HTML — the `innerHTML` getter.
///
/// Escapes what the parser would otherwise read back as markup: `&`, `<` and
/// `>` in text, `&` and `"` in attribute values. Void elements get no closing
/// tag, because emitting `</br>` would parse back as a second element.
pub fn serialize_children(dom: &Dom, node: NodeId) -> String {
    let mut out = String::new();
    for child in dom.children(node) {
        serialize_node(dom, child, &mut out);
    }
    out
}

fn serialize_node(dom: &Dom, node: NodeId, out: &mut String) {
    match &dom.node(node).data {
        NodeData::Text(text) => escape_text(text, out),
        NodeData::Comment(text) => {
            out.push_str("<!--");
            out.push_str(text);
            out.push_str("-->");
        }
        NodeData::Doctype(name) => {
            out.push_str("<!doctype ");
            out.push_str(name);
            out.push('>');
        }
        NodeData::Element { tag, attrs } => {
            out.push('<');
            out.push_str(tag);
            for (name, value) in attrs {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape_attribute(value, out);
                out.push('"');
            }
            out.push('>');
            if VOID.contains(&tag.as_str()) {
                return;
            }
            if super::tokenizer::keeps_text_verbatim(tag) {
                // `<script>` and `<style>` hold raw text: the tokenizer never
                // decodes entities inside them, so escaping here would compound
                // on the next parse — a `>` in a stylesheet would become `&gt;`
                // and then `&amp;gt;`. Their children are text by construction.
                for child in dom.children(node) {
                    if let NodeData::Text(text) = &dom.node(child).data {
                        out.push_str(text);
                    }
                }
            } else {
                for child in dom.children(node) {
                    serialize_node(dom, child, out);
                }
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        // The document node has no serialization of its own.
        NodeData::Document => {
            for child in dom.children(node) {
                serialize_node(dom, child, out);
            }
        }
    }
}

fn escape_text(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

fn escape_attribute(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
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

    // The structural constructors take the start tag's attrs so `<html lang>` /
    // `<body class>` survive (example.com, Wikipedia). Attrs apply only on first
    // creation; an implied open passes an empty vec, and a duplicate structural
    // tag is a no-op (first wins — merging is out of scope).
    fn ensure_html(&mut self, attrs: Vec<(String, String)>) -> NodeId {
        if let Some(h) = self.html {
            return h;
        }
        let h = self.dom.append_child(
            self.dom.root,
            NodeData::Element {
                tag: "html".into(),
                attrs,
            },
        );
        self.open.push(h);
        self.html = Some(h);
        h
    }

    fn ensure_head(&mut self, attrs: Vec<(String, String)>) -> NodeId {
        let html = self.ensure_html(Vec::new());
        if let Some(h) = self.head {
            return h;
        }
        let h = self.dom.append_child(
            html,
            NodeData::Element {
                tag: "head".into(),
                attrs,
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
    fn open_body(&mut self, attrs: Vec<(String, String)>) {
        if self.body.is_some() {
            return;
        }
        let html = self.ensure_html(Vec::new());
        self.ensure_head(Vec::new());
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
                attrs,
            },
        );
        self.open.push(body);
        self.body = Some(body);
    }

    fn insert_text(&mut self, s: String) {
        if s.trim().is_empty() {
            // Whitespace before the document has a spine is ignored, as the
            // spec's early insertion modes do. Everywhere else it is kept —
            // including between `<li>`s and table rows, which this builder used
            // to drop.
            //
            // Dropping was invisible while every `<li>` was a block (a block
            // break swallows the space anyway) and became wrong the moment the
            // cascade could say `display:inline`: Wikipedia's navbox hlists
            // rendered as `AnatomyGeneticsDwarf cat`, with no space node left
            // to collapse. Browsers keep the node and let layout decide; so do
            // we now. The `F1` inspector hides them, which is what the old rule
            // was really for.
            if self.at_structural_root() {
                return;
            }
        } else if self.at_structural_root() {
            // Real text before/around the spine belongs in flow.
            self.open_body(Vec::new());
        }
        let parent = self.insertion_parent();
        self.dom.append_child(parent, NodeData::Text(s));
    }

    fn insert_start(&mut self, name: String, attrs: Vec<(String, String)>, self_closing: bool) {
        match name.as_str() {
            "html" => {
                self.ensure_html(attrs);
                return;
            }
            "head" => {
                self.ensure_head(attrs);
                return;
            }
            "body" => {
                self.open_body(attrs);
                return;
            }
            _ => {}
        }

        // Recovery: close whatever this start tag implicitly ends.
        self.apply_implied_close(&name);

        // Placement: head metadata into <head> while it's open, otherwise flow.
        if !self.head_done && HEAD_TAGS.contains(&name.as_str()) {
            self.ensure_head(Vec::new());
        } else if self.at_structural_root() {
            self.open_body(Vec::new());
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
            self.open_body(Vec::new());
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
mod serialization {
    use super::*;

    /// Serialize a parsed document's `<body>` contents — what `innerHTML`
    /// returns for the body, and the shape the round-trip tests use.
    fn body_html(source: &str) -> String {
        let dom = parse(source);
        let body = find_body(&dom).expect("every parse synthesizes a body");
        serialize_children(&dom, body)
    }

    #[test]
    fn text_and_attributes_are_escaped_so_they_parse_back_as_data() {
        assert_eq!(
            body_html("<p>a &lt; b &amp; c</p>"),
            "<p>a &lt; b &amp; c</p>"
        );
        assert_eq!(
            body_html(r#"<a href="?x=1&amp;y=2" title="say &quot;hi&quot;">l</a>"#),
            r#"<a href="?x=1&amp;y=2" title="say &quot;hi&quot;">l</a>"#
        );
    }

    #[test]
    fn a_void_element_gets_no_closing_tag() {
        // `</br>` would parse back as a second element.
        assert_eq!(body_html("<p>a<br>b</p>"), "<p>a<br>b</p>");
        assert_eq!(body_html(r#"<img src="x.png">"#), r#"<img src="x.png">"#);
    }

    #[test]
    fn raw_text_elements_are_written_verbatim() {
        // The bug the ladder round-trip caught: `<style>` and `<script>` hold
        // text the tokenizer never decodes, so escaping it here compounds —
        // `>` would come back as `&gt;`, then `&amp;gt;`, growing on every
        // pass. A `<title>` is RCDATA and *is* escaped, because its entities
        // do get decoded.
        // The `<p>` opens the body: `<style>` and `<script>` written before
        // one are routed into `<head>` instead (see `HEAD_TAGS`).
        assert_eq!(
            body_html("<p></p><style>.a > .b { color: red }</style>"),
            "<p></p><style>.a > .b { color: red }</style>"
        );
        assert_eq!(
            body_html("<p></p><script>if (a < b && c > d) {}</script>"),
            "<p></p><script>if (a < b && c > d) {}</script>"
        );
        // A `<title>` is RCDATA, so its entities *are* decoded and escaping is
        // the right inverse for it.
        assert_eq!(
            body_html("<p></p><title>AT&amp;T</title>"),
            "<p></p><title>AT&amp;T</title>"
        );
    }

    #[test]
    fn parsing_a_serialization_gives_the_same_document() {
        for source in [
            "<p>plain</p>",
            "<div class='a b'><ul><li>one</li><li>two</li></ul></div>",
            "<p>before</p><style>.x > .y{content:'&'}</style><p>after</p>",
            "<p>a<br>b</p><!-- note --><p title='q&quot;q'>c</p>",
        ] {
            let once = body_html(source);
            let twice = body_html(&once);
            assert_eq!(once, twice, "serializing {source:?} is not stable");
        }
    }

    #[test]
    fn a_fragment_is_the_body_children_of_a_document_parse() {
        let (dom, roots) = parse_fragment("<p>one</p><p>two</p>");
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|&node| matches!(
            &dom.node(node).data,
            NodeData::Element { tag, .. } if tag == "p"
        )));
        assert_eq!(parse_fragment("").1.len(), 0);
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
    fn whitespace_between_block_tags_is_kept() {
        // The newline between `<ul>` and its first `<li>` survives as a text
        // child, the way a browser keeps it. Collapsing it is layout's job —
        // and if the page makes those items inline, that node is the space
        // between them (M5.0).
        assert_eq!(
            tree("<ul>\n  <li>a\n  <li>b\n</ul>"),
            "\
#document
  <html>
    <head>
    <body>
      <ul>
        #text \"\\n  \"
        <li>
          #text \"a\\n  \"
        <li>
          #text \"b\\n\"
"
        );
    }

    #[test]
    fn structural_element_attrs_are_kept() {
        // <html lang> / <body class> must survive — example.com and Wikipedia
        // both carry them, and the cascade (M4) will need them.
        assert_eq!(
            tree(r#"<html lang="en"><body class="doc">hi"#),
            "\
#document
  <html lang=\"en\">
    <head>
    <body class=\"doc\">
      #text \"hi\"
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

/// Ladder smoke test: parse each committed fixture without panicking and assert
/// a couple of structural invariants per page. These pin the recovery that real
/// pages depend on — notably danluu.com, which ships no `<body>` tag at all, so
/// "exactly one body" is a test of synthesis, not of the source.
#[cfg(test)]
mod ladder {
    use super::*;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    /// Every node in the tree, in no particular order.
    fn all_nodes(dom: &Dom) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![dom.root];
        while let Some(id) = stack.pop() {
            out.push(id);
            for child in dom.children(id) {
                stack.push(child);
            }
        }
        out
    }

    fn count_tag(dom: &Dom, tag: &str) -> usize {
        all_nodes(dom)
            .iter()
            .filter(
                |&&id| matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag),
            )
            .count()
    }

    /// Elements of `tag` carrying `class_token` as one of their space-separated
    /// classes (HN rows are `class="athing submission"`, so equality won't do).
    fn count_tag_with_class(dom: &Dom, tag: &str, class_token: &str) -> usize {
        all_nodes(dom)
            .iter()
            .filter(|&&id| {
                matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag)
                    && dom
                        .attr(id, "class")
                        .is_some_and(|c| c.split_whitespace().any(|w| w == class_token))
            })
            .count()
    }

    #[test]
    fn example_com() {
        let dom = parse(fixture!("example.com.html"));
        assert_eq!(count_tag(&dom, "body"), 1);
        assert!(count_tag(&dom, "h1") >= 1);
    }

    #[test]
    fn motherfuckingwebsite_com() {
        let dom = parse(fixture!("motherfuckingwebsite.com.html"));
        assert_eq!(count_tag(&dom, "body"), 1);
        assert!(count_tag(&dom, "p") > 1);
    }

    #[test]
    fn danluu_com_body_is_synthesized() {
        // danluu.com's source has no <body> tag; the builder must invent exactly
        // one and hang the page's links off it.
        let src = fixture!("danluu.com.html");
        assert!(!src.to_ascii_lowercase().contains("<body"));
        let dom = parse(src);
        assert_eq!(count_tag(&dom, "body"), 1);
        assert!(count_tag(&dom, "a") > 10);
    }

    #[test]
    fn hacker_news_story_rows() {
        let dom = parse(fixture!("news.ycombinator.com.html"));
        assert_eq!(count_tag(&dom, "body"), 1);
        // A full HN front page is 30 stories, each a <tr class="athing …">.
        assert_eq!(count_tag_with_class(&dom, "tr", "athing"), 30);
    }

    #[test]
    fn wikipedia_article() {
        let dom = parse(fixture!("en.wikipedia.org.html"));
        assert_eq!(count_tag(&dom, "body"), 1);
        assert!(count_tag(&dom, "p") > 20);
        // The <title>'s text child survives raw-text handling.
        let title = all_nodes(&dom)
            .into_iter()
            .find(
                |&id| matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "title"),
            )
            .expect("a <title> element");
        let text = dom.children(title).next().expect("title text");
        assert_eq!(
            dom.node(text).data,
            NodeData::Text("Cat - Wikipedia".into())
        );
    }
}
