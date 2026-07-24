//! Arena DOM (PLAN.md §2). Nodes live in a single `Vec<Node>` and refer to each
//! other by `NodeId` index — parent, first/last child, and sibling links only.
//! No `Rc`/`RefCell`, no raw pointers: a tree walk is index arithmetic over the
//! arena, which is what keeps later stages (style, layout) able to hold a plain
//! `&Dom` without borrow gymnastics.
//!
//! This is the shape M2.2's tree builder fills; here it only knows how to be
//! constructed and traversed.
//!
// M2.1 lands these types with no consumer yet — the tree builder (M2.2) and the
// F1 inspector (M2.3) are the first callers. Until then the arena is exercised
// only by its own tests, so silence dead-code noise crate-wide for the module.
#![allow(dead_code)]

/// Index into `Dom::nodes`. `u32` is plenty — a Wikipedia article is well under
/// the 4-billion node ceiling and half the width of a pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct NodeId(pub u32);

/// The payload of a node. The `Document` variant is the arena root and appears
/// exactly once; everything else is produced by the tree builder from tokens.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NodeData {
    Document,
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
    Comment(String),
    Doctype(String),
}

/// A node and its links. All links are `Option<NodeId>`: the root has no parent,
/// leaves no children, ends of a sibling run no neighbour on that side.
#[derive(Clone, Debug)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub data: NodeData,
}

/// The arena. `nodes[root.0]` is always the `Document`.
pub struct Dom {
    nodes: Vec<Node>,
    pub root: NodeId,
}

impl Dom {
    /// A fresh document holding only its root node.
    pub fn new_document() -> Dom {
        let root = Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data: NodeData::Document,
        };
        Dom {
            nodes: vec![root],
            root: NodeId(0),
        }
    }

    /// Append `data` as the new last child of `parent`, wiring both directions of
    /// every link. Returns the new node's id.
    pub fn append_child(&mut self, parent: NodeId, data: NodeData) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let prev = self.nodes[parent.0 as usize].last_child;
        self.nodes.push(Node {
            parent: Some(parent),
            first_child: None,
            last_child: None,
            prev_sibling: prev,
            next_sibling: None,
            data,
        });
        match prev {
            Some(prev) => self.nodes[prev.0 as usize].next_sibling = Some(id),
            None => self.nodes[parent.0 as usize].first_child = Some(id),
        }
        self.nodes[parent.0 as usize].last_child = Some(id);
        id
    }

    /// Borrow a node by id.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// Iterate a node's children in document order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: self.nodes[id.0 as usize].first_child,
        }
    }

    /// Look up an attribute on an element by name, ASCII-case-insensitively (HTML
    /// attribute names are case-insensitive). `None` on non-elements or a miss.
    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id.0 as usize].data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str()),
            _ => None,
        }
    }
}

/// Child iterator: walks `next_sibling` from a node's first child.
pub struct Children<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let id = self.next?;
        self.next = self.dom.nodes[id.0 as usize].next_sibling;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // <div id="a">hello</div> built by hand: div under the document, text under
    // the div, plus a sibling comment on the div to pin the sibling links.
    fn sample() -> (Dom, NodeId, NodeId, NodeId) {
        let mut dom = Dom::new_document();
        let div = dom.append_child(
            dom.root,
            NodeData::Element {
                tag: "div".into(),
                attrs: vec![("id".into(), "a".into())],
            },
        );
        let text = dom.append_child(div, NodeData::Text("hello".into()));
        let comment = dom.append_child(dom.root, NodeData::Comment("c".into()));
        (dom, div, text, comment)
    }

    #[test]
    fn parent_child_links() {
        let (dom, div, text, _) = sample();
        assert_eq!(dom.node(div).parent, Some(dom.root));
        assert_eq!(dom.node(div).first_child, Some(text));
        assert_eq!(dom.node(div).last_child, Some(text));
        assert_eq!(dom.node(text).parent, Some(div));
        assert_eq!(dom.node(text).first_child, None);
    }

    #[test]
    fn sibling_links_both_ways() {
        let (dom, div, _, comment) = sample();
        assert_eq!(dom.node(div).next_sibling, Some(comment));
        assert_eq!(dom.node(div).prev_sibling, None);
        assert_eq!(dom.node(comment).prev_sibling, Some(div));
        assert_eq!(dom.node(comment).next_sibling, None);
    }

    #[test]
    fn children_iterates_in_order() {
        let (dom, div, comment, _) = {
            let (dom, div, _text, comment) = sample();
            (dom, div, comment, ())
        };
        let kids: Vec<NodeId> = dom.children(dom.root).collect();
        assert_eq!(kids, vec![div, comment]);
    }

    #[test]
    fn attr_is_case_insensitive() {
        let (dom, div, _, _) = sample();
        assert_eq!(dom.attr(div, "id"), Some("a"));
        assert_eq!(dom.attr(div, "ID"), Some("a"));
        assert_eq!(dom.attr(div, "Id"), Some("a"));
        assert_eq!(dom.attr(div, "class"), None);
    }

    #[test]
    fn attr_on_non_element_is_none() {
        let (dom, _, text, _) = sample();
        assert_eq!(dom.attr(text, "id"), None);
    }
}
