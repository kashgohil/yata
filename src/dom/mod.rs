//! Arena DOM (PLAN.md §2). Nodes live in a single `Vec<Node>` and refer to each
//! other by `NodeId` index — parent, first/last child, and sibling links only.
//! No `Rc`/`RefCell`, no raw pointers: a tree walk is index arithmetic over the
//! arena, which is what keeps later stages (style, layout) able to hold a plain
//! `&Dom` without borrow gymnastics.
//!
//! This is the shape M2.2's tree builder fills. Since M10.3 it also has a write
//! side — create, insert, move, remove, set — because a scripting engine needs
//! one underneath every DOM mutation binding (M10.5). Two rules make that safe
//! and are enforced here rather than trusted to callers:
//!
//! - **Ids are never reused.** Nothing is ever freed or compacted, so a
//!   `NodeId` means the same node for the life of the document even after that
//!   node leaves the tree. JS holds handles to removed nodes; an id that came
//!   to mean a different node would let a page read or write the wrong element.
//!   The cost is an arena that only grows — see `arena_growth_is_unbounded`.
//! - **The tree stays a tree.** Inserting a node into its own subtree is
//!   refused, because the result would not be a wrong page but a layout walk
//!   that never terminates.

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
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    /// Read by `insert_before` and `unlink`, which have to repair the run from
    /// both ends — the non-test reader the arena was always waiting for.
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub data: NodeData,
}

/// How deeply the tree may nest.
///
/// Not an arbitrary tidiness limit: **style and layout both recurse over the
/// tree**, so a deep enough subtree overflows the native stack — which is a
/// process abort, not an error, and nothing a page or a `catch` can recover
/// from.
///
/// The number comes from measurement, and layout is the binding constraint by
/// a wide margin: style survives past 2,000 levels, but **layout overflows
/// between 200 and 300** on a 2 MB thread (a Rust test thread; the main thread
/// has 8 MB and would reach roughly four times further). 128 sits at half the
/// measured floor on the *smaller* stack.
///
/// Against real pages that is still generous. The ladder's deepest nesting:
/// example.com 7, motherfuckingwebsite 6, danluu 7, Hacker News 15, and
/// Wikipedia — the deepest page in the suite — **62**. A page would have to be
/// twice as deeply nested as Wikipedia before the cap could be noticed.
///
/// The cap lives here, in the arena, because this is the one place every node
/// enters the tree — from the parser and from a script alike. A guard at the
/// script boundary alone would leave a hostile *server* able to do the same
/// thing with markup.
pub const MAX_DEPTH: usize = 128;

/// How many distinct nodes the attribute-change list remembers before it stops
/// trying (M11.3).
///
/// The list exists so a restyle can be narrowed to the subtrees a tick's writes
/// can reach; past a few dozen roots the narrowing has no benefit left to
/// deliver — those subtrees overlap and add up to the document — and an
/// unbounded list is exactly the shape M10.13 exists to refuse. So the cap is
/// not a precision limit: it is the point at which the whole-document answer
/// is both correct and no slower.
pub const MAX_TRACKED_ATTR_CHANGES: usize = 32;

/// Which nodes had an attribute written since the list was last read.
///
/// `TooMany` is not an error: it is the honest answer that the caller should
/// stop narrowing and recompute everything.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AttrChanges {
    /// Every node written to, once each, in the order they were first written.
    Nodes(Vec<NodeId>),
    /// More than [`MAX_TRACKED_ATTR_CHANGES`] distinct nodes were written.
    TooMany,
}

/// Why a tree edit was refused. These are the two DOM exceptions M10.5 turns
/// into JS throws, named after them so the mapping needs no lookup table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomError {
    /// The insert would have put a node inside its own subtree (or moved the
    /// document root), which is not a tree.
    HierarchyRequest,
    /// The reference node is not a child of the parent it was given with.
    NotFound,
    /// The insert would nest deeper than [`MAX_DEPTH`]. A browser has no such
    /// limit; we do, because our style and layout walks recurse.
    TooDeep,
}

/// The arena. `nodes[root.0]` is always the `Document`.
// `PartialEq`/`Debug` exist because a `Dom` travels inside `Msg::Parsed` and
// `Msg` is compared and printed wholesale by tests.
#[derive(PartialEq, Eq, Debug)]
pub struct Dom {
    nodes: Vec<Node>,
    pub root: NodeId,
    /// Bumped by every mutation, so a later stage can ask "did anything
    /// change?" without a deep compare. M10.6's invalidation is the consumer.
    /// It counts edits, not tree shapes: two documents that reached the same
    /// shape by different routes hold different versions, and since it is part
    /// of `Dom` they are therefore not `==`.
    version: u64,
    /// Bumped by the subset of those edits that changed the **shape of the
    /// tree or the text in it**: inserts, moves, removals, `set_text`.
    /// Attribute writes and the creation of a node nothing has inserted yet
    /// do not touch it.
    ///
    /// The distinction is M10.6's whole classification. A structural edit
    /// always needs a relayout, because boxes were added, removed or resized.
    /// An attribute write only *might*, and only through the cascade — so it
    /// can be answered by restyling and comparing computed values, which is
    /// far cheaper than laying the page out to find out.
    structure_version: u64,
    /// *Which* nodes those attribute writes landed on, since the list was last
    /// read (M11.3) — the difference between knowing that the cascade's answer
    /// may have moved and knowing where. Bounded by
    /// [`MAX_TRACKED_ATTR_CHANGES`] and deduplicated, because a script that
    /// writes the same `class` in a loop has changed one node, not a thousand.
    attr_changes: Vec<NodeId>,
    /// The list gave up: more than the cap's worth of distinct nodes. Kept as a
    /// flag rather than by letting the `Vec` grow, so a hostile page cannot buy
    /// memory with attribute writes.
    attr_changes_overflowed: bool,
}

/// What kind of change an edit was, for `Dom::note_edit`.
#[derive(Clone, Copy)]
enum Edit {
    /// Changed the shape of the tree or the text in it. Always needs layout.
    Structure,
    /// Changed an attribute on this node. Can only reach layout through the
    /// cascade, so M10.6 answers it by restyling and comparing computed
    /// values — and M11.3 restyles only the subtree it can have reached.
    Attribute(NodeId),
    /// Created a node that is in the arena but in no tree. It cannot move a
    /// box until something inserts it — and the insert is a `Structure` edit
    /// of its own — but it does grow `node_count`, which the styled tree is
    /// sized by, so it still counts as an edit.
    Detached,
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
            version: 0,
            structure_version: 0,
            attr_changes: Vec::new(),
            attr_changes_overflowed: false,
        }
    }

    /// Append `data` as the new last child of `parent`, wiring both directions of
    /// every link. Returns the new node's id.
    /// Append `data` as the new last child of `parent`. When `parent` is
    /// already at [`MAX_DEPTH`] the node is attached to the deepest ancestor
    /// that is not — the content is kept, the nesting is not.
    ///
    /// Flattening rather than dropping, because this path is the *parser*: the
    /// alternative is discarding a hostile page's text along with its
    /// structure, and a reader is better served by prose in the wrong place
    /// than by nothing.
    pub fn append_child(&mut self, parent: NodeId, data: NodeData) -> NodeId {
        let parent = self.insertion_point(parent);
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
        self.note_edit(Edit::Structure);
        id
    }

    /// Record an edit against the counters M10.6 classifies by. The single
    /// place `version` is bumped, so a new mutator cannot forget one of them.
    fn note_edit(&mut self, edit: Edit) {
        self.version += 1;
        match edit {
            Edit::Structure => self.structure_version += 1,
            Edit::Attribute(id) => self.note_attr_change(id),
            Edit::Detached => {}
        }
    }

    /// Remember that `id`'s attributes moved, unless the list has already
    /// given up or already knows.
    fn note_attr_change(&mut self, id: NodeId) {
        if self.attr_changes_overflowed || self.attr_changes.contains(&id) {
            return;
        }
        if self.attr_changes.len() == MAX_TRACKED_ATTR_CHANGES {
            self.attr_changes_overflowed = true;
            // Nothing reads the list once it has overflowed, and holding onto
            // the ids would be memory a page bought by writing attributes.
            self.attr_changes.clear();
            self.attr_changes.shrink_to_fit();
            return;
        }
        self.attr_changes.push(id);
    }

    /// The nodes whose attributes changed since this was last called, and clear
    /// the list. Reading is taking: the answer describes one tick, and a list
    /// that survived into the next one would narrow the next restyle against
    /// writes it has already accounted for.
    pub fn take_attr_changes(&mut self) -> AttrChanges {
        let overflowed = std::mem::take(&mut self.attr_changes_overflowed);
        let nodes = std::mem::take(&mut self.attr_changes);
        match overflowed {
            true => AttrChanges::TooMany,
            false => AttrChanges::Nodes(nodes),
        }
    }

    /// Edits made to this document since it was created. See the field docs:
    /// this is a change *signal*, not a description of the tree.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Edits that changed the tree's shape or its text. See the field docs:
    /// this is the half of `version` that always needs a relayout.
    pub fn structure_version(&self) -> u64 {
        self.structure_version
    }

    /// A new element belonging to no tree. It exists in the arena — style will
    /// size its dense `Vec` to include it and `F1` can be asked about it — but
    /// nothing walks to it until it is inserted.
    pub fn create_element(&mut self, tag: &str, attrs: Vec<(String, String)>) -> NodeId {
        self.push_detached(NodeData::Element {
            tag: tag.to_string(),
            attrs,
        })
    }

    /// A new text node belonging to no tree.
    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.push_detached(NodeData::Text(text.to_string()))
    }

    fn push_detached(&mut self, data: NodeData) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data,
        });
        self.note_edit(Edit::Detached);
        id
    }

    /// Make `child` the last child of `parent`, **moving** it if it already had
    /// one — a node is in one place or no place, never aliased into two, which
    /// is what the DOM does and what keeps a walk from visiting it twice.
    pub fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
        self.check_insertable(parent, child)?;
        self.unlink(child);

        let prev = self.nodes[parent.0 as usize].last_child;
        let node = &mut self.nodes[child.0 as usize];
        node.parent = Some(parent);
        node.prev_sibling = prev;
        node.next_sibling = None;
        match prev {
            Some(prev) => self.nodes[prev.0 as usize].next_sibling = Some(child),
            None => self.nodes[parent.0 as usize].first_child = Some(child),
        }
        self.nodes[parent.0 as usize].last_child = Some(child);
        self.note_edit(Edit::Structure);
        Ok(())
    }

    /// Insert `child` into `parent` directly before `reference`, moving it if
    /// it already had a parent. `reference` must be a child of `parent`.
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        child: NodeId,
        reference: NodeId,
    ) -> Result<(), DomError> {
        if self.nodes[reference.0 as usize].parent != Some(parent) {
            return Err(DomError::NotFound);
        }
        // Inserting a node before itself is where it already is. The DOM says
        // so too, and taking it literally would unlink the reference we are
        // about to position against.
        if child == reference {
            return Ok(());
        }
        self.check_insertable(parent, child)?;
        self.unlink(child);

        // Read the neighbour *after* unlinking: `child` may have been the very
        // node sitting before `reference`.
        let prev = self.nodes[reference.0 as usize].prev_sibling;
        let node = &mut self.nodes[child.0 as usize];
        node.parent = Some(parent);
        node.prev_sibling = prev;
        node.next_sibling = Some(reference);
        self.nodes[reference.0 as usize].prev_sibling = Some(child);
        match prev {
            Some(prev) => self.nodes[prev.0 as usize].next_sibling = Some(child),
            None => self.nodes[parent.0 as usize].first_child = Some(child),
        }
        // `last_child` cannot change: `reference` is still after `child`.
        self.note_edit(Edit::Structure);
        Ok(())
    }

    /// Take `child` out of the tree. Its own subtree travels with it and stays
    /// intact, so the caller can put it back. Removing a node that has no
    /// parent — one already detached, or the document root — does nothing, the
    /// same no-op the DOM's `ChildNode.remove()` performs.
    pub fn remove(&mut self, child: NodeId) {
        if self.nodes[child.0 as usize].parent.is_none() {
            return;
        }
        self.unlink(child);
        self.note_edit(Edit::Structure);
    }

    /// Set an attribute, replacing any existing one whose name matches
    /// ASCII-case-insensitively (the same rule `attr` reads by). `false`, and
    /// no change, when `id` is not an element.
    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) -> bool {
        let NodeData::Element { attrs, .. } = &mut self.nodes[id.0 as usize].data else {
            return false;
        };
        match attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some((_, existing)) => {
                existing.clear();
                existing.push_str(value);
            }
            None => attrs.push((name.to_string(), value.to_string())),
        }
        self.note_edit(Edit::Attribute(id));
        true
    }

    /// Remove an attribute by name. `false` when `id` is not an element or had
    /// no such attribute — that is, when the document did not change.
    pub fn remove_attr(&mut self, id: NodeId, name: &str) -> bool {
        let NodeData::Element { attrs, .. } = &mut self.nodes[id.0 as usize].data else {
            return false;
        };
        let Some(at) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) else {
            return false;
        };
        attrs.remove(at);
        self.note_edit(Edit::Attribute(id));
        true
    }

    /// Replace a text node's content. `false`, and no change, when `id` is not
    /// a text node.
    pub fn set_text(&mut self, id: NodeId, text: &str) -> bool {
        let NodeData::Text(existing) = &mut self.nodes[id.0 as usize].data else {
            return false;
        };
        existing.clear();
        existing.push_str(text);
        self.note_edit(Edit::Structure);
        true
    }

    /// Refuse an insert that would not leave a tree behind.
    ///
    /// Four separate conditions, and none implies another:
    ///
    /// - A node cannot go inside itself or its own descendant.
    /// - The document root cannot be inserted anywhere at all. The ancestor
    ///   walk alone misses this, because a **detached** target has no
    ///   ancestors — so `append(orphan, root)` would pass the cycle check and
    ///   hand the document a parent, leaving an arena with no root.
    /// - Only a document or an element can hold children. Text, comments and
    ///   the doctype are leaves; a browser throws `HierarchyRequestError`
    ///   rather than nesting into one. Enforced here rather than left to
    ///   M10.5's bindings because the two halves of the engine disagree about
    ///   such a tree: `F1` prints children under a text node, and layout
    ///   silently drops them, so the reader would be shown content that can
    ///   never render.
    /// - The tree may not nest past [`MAX_DEPTH`], because style and layout
    ///   recurse and a deep enough subtree aborts the process.
    fn check_insertable(&self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
        let holds_children = matches!(
            self.nodes[parent.0 as usize].data,
            NodeData::Document | NodeData::Element { .. }
        );
        if !holds_children || child == self.root || self.is_ancestor_or_self(child, parent) {
            return Err(DomError::HierarchyRequest);
        }
        // A script gets an exception rather than the parser's flattening: it
        // asked for a specific place, and quietly putting the node somewhere
        // else would be a lie about what `appendChild` did.
        if self.depth_of(parent) >= MAX_DEPTH {
            return Err(DomError::TooDeep);
        }
        Ok(())
    }

    /// How far `node` sits from the root, counting to at most `MAX_DEPTH + 1`
    /// — the exact number past the cap does not matter and walking a
    /// pathological chain to find it would be the very cost being avoided.
    fn depth_of(&self, node: NodeId) -> usize {
        let mut depth = 0;
        let mut walk = self.nodes[node.0 as usize].parent;
        while let Some(parent) = walk {
            depth += 1;
            if depth > MAX_DEPTH {
                return depth;
            }
            walk = self.nodes[parent.0 as usize].parent;
        }
        depth
    }

    /// Where a child of `parent` may actually go: `parent` itself, or the
    /// deepest ancestor still inside the cap.
    fn insertion_point(&self, parent: NodeId) -> NodeId {
        if self.depth_of(parent) < MAX_DEPTH {
            return parent;
        }
        let mut walk = parent;
        while self.depth_of(walk) >= MAX_DEPTH {
            match self.nodes[walk.0 as usize].parent {
                Some(up) => walk = up,
                None => break,
            }
        }
        walk
    }

    /// Walks parents from `id` up to the root.
    fn is_ancestor_or_self(&self, ancestor: NodeId, id: NodeId) -> bool {
        let mut walk = Some(id);
        while let Some(current) = walk {
            if current == ancestor {
                return true;
            }
            walk = self.nodes[current.0 as usize].parent;
        }
        false
    }

    /// Detach a node from its parent and siblings, repairing the run it leaves.
    /// Its children are untouched. Callers bump the version — `unlink` is also
    /// half of a move, which must count as one edit, not two.
    fn unlink(&mut self, child: NodeId) {
        let node = &self.nodes[child.0 as usize];
        let (Some(parent), prev, next) = (node.parent, node.prev_sibling, node.next_sibling) else {
            return;
        };

        match prev {
            Some(prev) => self.nodes[prev.0 as usize].next_sibling = next,
            None => self.nodes[parent.0 as usize].first_child = next,
        }
        match next {
            Some(next) => self.nodes[next.0 as usize].prev_sibling = prev,
            None => self.nodes[parent.0 as usize].last_child = prev,
        }

        let node = &mut self.nodes[child.0 as usize];
        node.parent = None;
        node.prev_sibling = None;
        node.next_sibling = None;
    }

    /// How many nodes the arena holds. Every `NodeId` is below this, which is
    /// what lets the styled tree (M4.2) be a dense `Vec` indexed by id rather
    /// than a map.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
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
/// Every arena invariant, checked over the whole `Vec` rather than the
/// nodes a test happens to name. This is what the fuzz drives, and it is
/// the cheapest insurance in M10: a link repaired in one direction only is
/// invisible until something walks the other way, which may be a page.
pub(crate) fn check_links(dom: &Dom) {
    let count = dom.node_count();

    // The arena has a root and it is nobody's child. Everything else is
    // reachable from it or deliberately detached; a root with a parent is
    // a document that has been swallowed by its own contents.
    assert_eq!(dom.node(dom.root).parent, None, "the root gained a parent");

    // How many nodes name each node as their parent, so a child that
    // claims a parent which does not list it cannot hide.
    let mut claimed = vec![0usize; count];
    for i in 0..count {
        if let Some(parent) = dom.node(NodeId(i as u32)).parent {
            claimed[parent.0 as usize] += 1;
        }
    }

    for (i, &claims) in claimed.iter().enumerate() {
        let id = NodeId(i as u32);
        let node = dom.node(id);

        // Walking up terminates — the parent chain holds no cycle.
        let mut steps = 0;
        let mut up = node.parent;
        while let Some(parent) = up {
            steps += 1;
            assert!(steps <= count, "cycle in the parent chain above {id:?}");
            up = dom.node(parent).parent;
        }

        // The child run reads the same forwards and backwards, and every
        // node in it claims this node as its parent.
        let mut walked = 0;
        let mut prev = None;
        let mut child = node.first_child;
        while let Some(current) = child {
            let node = dom.node(current);
            assert_eq!(node.parent, Some(id), "{current:?} does not claim {id:?}");
            assert_eq!(
                node.prev_sibling, prev,
                "prev_sibling broken at {current:?}"
            );
            prev = Some(current);
            child = node.next_sibling;
            walked += 1;
            assert!(walked <= count, "cycle in the sibling run under {id:?}");
        }
        assert_eq!(node.last_child, prev, "last_child wrong on {id:?}");
        assert_eq!(walked, claims, "{id:?} does not list every child of it");

        // A node with no parent is out of the tree entirely: no sibling
        // link may point back into it.
        if node.parent.is_none() {
            assert_eq!(node.prev_sibling, None, "detached {id:?} kept a sibling");
            assert_eq!(node.next_sibling, None, "detached {id:?} kept a sibling");
        }
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
    fn node_count_covers_every_id() {
        let (dom, div, text, comment) = sample();
        // Document + div + text + comment, and every id is a valid index.
        assert_eq!(dom.node_count(), 4);
        for id in [dom.root, div, text, comment] {
            assert!((id.0 as usize) < dom.node_count());
        }
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

    // ---- the write side (M10.3) ----

    #[test]
    fn created_nodes_are_detached() {
        let mut dom = Dom::new_document();
        let el = dom.create_element("div", vec![("id".into(), "new".into())]);
        let text = dom.create_text("hi");

        for id in [el, text] {
            let node = dom.node(id);
            assert_eq!(node.parent, None);
            assert_eq!(node.prev_sibling, None);
            assert_eq!(node.next_sibling, None);
        }
        assert_eq!(dom.attr(el, "id"), Some("new"));
        assert_eq!(dom.children(dom.root).count(), 0);
        check_links(&dom);
    }

    #[test]
    fn a_detached_node_gets_a_default_style() {
        let mut dom = Dom::new_document();
        let attached = dom.append_child(
            dom.root,
            NodeData::Element {
                tag: "div".into(),
                attrs: vec![],
            },
        );
        let detached = dom.create_element("div", vec![]);

        let styles = crate::style::style_tree(&dom, &[]);
        // The dense `Vec` has a slot for it — asking is not an index panic...
        assert_eq!(
            *styles.get(detached),
            crate::style::ComputedStyle::default()
        );
        // ...and the cascade never reached it: the UA sheet's `div{display:block}`
        // applied to the one in the tree and not to this one.
        assert_eq!(
            styles.get(attached).display,
            crate::style::values::Display::Block
        );
    }

    #[test]
    fn a_detached_node_never_reaches_layout_or_f1() {
        let mut dom = crate::html::parse("<p>in the tree</p>");
        let orphan = dom.create_element("p", vec![]);
        let orphan_text = dom.create_text("detached and invisible");
        dom.append(orphan, orphan_text).unwrap();

        let styles = crate::style::style_tree(&dom, &[]);
        let lines = crate::layout::layout(&dom, &styles, 40, crate::layout::Hidden::Respect);
        let rendered = crate::layout::debug_lines(&lines);
        assert!(rendered.contains("in the tree"));
        assert!(
            !rendered.contains("detached"),
            "a node outside the tree was laid out:\n{rendered}"
        );

        // F1 walks from the root, so it cannot reach it either.
        let dumped = crate::html::debug_tree(&dom);
        assert!(!dumped.contains("detached"), "{dumped}");
    }

    #[test]
    fn append_moves_a_node_rather_than_aliasing_it() {
        let (mut dom, div, text, comment) = sample();
        // `text` starts under `div`; move it under `comment`'s parent chain.
        let target = dom.append_child(
            dom.root,
            NodeData::Element {
                tag: "section".into(),
                attrs: vec![],
            },
        );
        dom.append(target, text).unwrap();

        assert_eq!(dom.node(text).parent, Some(target));
        // Gone from the old parent, not duplicated into both.
        assert_eq!(dom.children(div).count(), 0);
        assert_eq!(dom.node(div).first_child, None);
        assert_eq!(dom.node(div).last_child, None);
        assert_eq!(dom.children(target).collect::<Vec<_>>(), vec![text]);
        assert_eq!(dom.node(comment).next_sibling, Some(target));
        check_links(&dom);
    }

    #[test]
    fn insert_before_positions_the_node_and_repairs_both_runs() {
        let mut dom = Dom::new_document();
        let a = dom.append_child(dom.root, NodeData::Text("a".into()));
        let c = dom.append_child(dom.root, NodeData::Text("c".into()));
        let b = dom.create_text("b");

        dom.insert_before(dom.root, b, c).unwrap();
        assert_eq!(dom.children(dom.root).collect::<Vec<_>>(), vec![a, b, c]);
        assert_eq!(dom.node(dom.root).first_child, Some(a));
        assert_eq!(dom.node(dom.root).last_child, Some(c));

        // Insert at the front: `first_child` has to move.
        let front = dom.create_text("front");
        dom.insert_before(dom.root, front, a).unwrap();
        assert_eq!(dom.node(dom.root).first_child, Some(front));
        assert_eq!(
            dom.children(dom.root).collect::<Vec<_>>(),
            vec![front, a, b, c]
        );
        check_links(&dom);
    }

    #[test]
    fn insert_before_a_node_that_is_already_there_is_a_no_op() {
        let mut dom = Dom::new_document();
        let a = dom.append_child(dom.root, NodeData::Text("a".into()));
        let b = dom.append_child(dom.root, NodeData::Text("b".into()));

        // Moving a node before itself must not unlink the very reference it
        // is being positioned against.
        assert_eq!(dom.insert_before(dom.root, b, b), Ok(()));
        assert_eq!(dom.children(dom.root).collect::<Vec<_>>(), vec![a, b]);
        check_links(&dom);
    }

    #[test]
    fn insert_before_a_reference_that_is_not_a_child_is_not_found() {
        let (mut dom, div, text, _) = sample();
        let new = dom.create_text("new");
        // `text` is a child of `div`, not of the root.
        assert_eq!(
            dom.insert_before(dom.root, new, text),
            Err(DomError::NotFound)
        );
        assert_eq!(
            dom.node(new).parent,
            None,
            "a refused insert changed the tree"
        );
        assert_eq!(dom.children(div).collect::<Vec<_>>(), vec![text]);
        check_links(&dom);
    }

    #[test]
    fn a_node_cannot_be_inserted_into_its_own_subtree() {
        // Without this the tree stops being a tree, and the symptom is not a
        // wrong page: it is a layout walk that never terminates.
        let (mut dom, div, text, _) = sample();
        assert_eq!(dom.append(text, div), Err(DomError::HierarchyRequest));
        assert_eq!(dom.append(div, div), Err(DomError::HierarchyRequest));
        assert_eq!(
            dom.append(div, dom.root),
            Err(DomError::HierarchyRequest),
            "the document root is an ancestor of everything and cannot move"
        );
        assert_eq!(
            dom.insert_before(text, div, text),
            Err(DomError::NotFound),
            "the reference is checked first"
        );
        check_links(&dom);
    }

    #[test]
    fn only_documents_and_elements_can_hold_children() {
        // Text, comments and the doctype are leaves. Allowing a child under
        // one produces a tree the engine cannot agree with itself about: `F1`
        // walks children and prints it, layout treats text as a leaf and drops
        // it, and the reader is shown content that can never render.
        let (mut dom, div, text, comment) = sample();
        let orphan = dom.create_element("span", vec![]);

        assert_eq!(dom.append(text, orphan), Err(DomError::HierarchyRequest));
        assert_eq!(dom.append(comment, orphan), Err(DomError::HierarchyRequest));
        assert_eq!(dom.node(orphan).parent, None);
        assert_eq!(dom.children(text).count(), 0);

        // Elements and the document itself still take children, of course.
        assert_eq!(dom.append(div, orphan), Ok(()));
        let second = dom.create_text("t");
        assert_eq!(dom.append(dom.root, second), Ok(()));
        check_links(&dom);
    }

    #[test]
    fn the_document_root_cannot_be_inserted_anywhere() {
        let (mut dom, div, _, _) = sample();
        assert_eq!(dom.append(div, dom.root), Err(DomError::HierarchyRequest));

        // The case the ancestor walk alone misses: an orphan has no ancestors,
        // so nothing reports the root as one of them, and the document would
        // end up as a child of a node outside the tree.
        let orphan = dom.create_element("div", vec![]);
        assert_eq!(
            dom.append(orphan, dom.root),
            Err(DomError::HierarchyRequest)
        );
        assert_eq!(
            dom.insert_before(div, dom.root, div),
            Err(DomError::NotFound)
        );
        assert_eq!(dom.node(dom.root).parent, None);
        check_links(&dom);
    }

    #[test]
    fn remove_detaches_the_node_and_its_subtree_travels_with_it() {
        let (mut dom, div, text, comment) = sample();
        dom.remove(div);

        assert_eq!(dom.node(div).parent, None);
        assert_eq!(dom.children(dom.root).collect::<Vec<_>>(), vec![comment]);
        assert_eq!(dom.node(comment).prev_sibling, None);
        // The subtree is intact, so the caller can put it back.
        assert_eq!(dom.children(div).collect::<Vec<_>>(), vec![text]);
        assert_eq!(dom.node(text).parent, Some(div));

        dom.append(dom.root, div).unwrap();
        assert_eq!(
            dom.children(dom.root).collect::<Vec<_>>(),
            vec![comment, div]
        );
        check_links(&dom);
    }

    #[test]
    fn removing_a_parentless_node_does_nothing() {
        let (mut dom, _, _, _) = sample();
        let before = dom.version();
        dom.remove(dom.root);
        let orphan = dom.create_text("orphan");
        let after_create = dom.version();
        dom.remove(orphan);
        assert_eq!(dom.version(), after_create, "a no-op counted as an edit");
        assert!(before < after_create);
        check_links(&dom);
    }

    #[test]
    fn ids_are_never_reused() {
        // JS holds handles to removed nodes (M10.4). If an id came to mean a
        // different node, a page could read the wrong element through a stale
        // reference.
        let mut dom = Dom::new_document();
        let first = dom.create_text("first");
        dom.append(dom.root, first).unwrap();
        dom.remove(first);

        let second = dom.create_text("second");
        assert_ne!(first, second);
        assert_eq!(dom.node(first).data, NodeData::Text("first".into()));
        assert_eq!(dom.node_count(), 3);
    }

    #[test]
    fn attribute_and_text_writes() {
        let (mut dom, div, text, _) = sample();

        assert!(dom.set_attr(div, "class", "one"));
        assert_eq!(dom.attr(div, "class"), Some("one"));
        // Case-insensitive on the name, like the reader: this replaces rather
        // than adding a second `class`.
        assert!(dom.set_attr(div, "CLASS", "two"));
        assert_eq!(dom.attr(div, "class"), Some("two"));
        assert_eq!(
            dom.node(div).data,
            NodeData::Element {
                tag: "div".into(),
                attrs: vec![("id".into(), "a".into()), ("class".into(), "two".into())],
            }
        );

        assert!(dom.remove_attr(div, "Id"));
        assert_eq!(dom.attr(div, "id"), None);
        assert!(
            !dom.remove_attr(div, "id"),
            "removing a missing attr changed nothing"
        );

        assert!(dom.set_text(text, "goodbye"));
        assert_eq!(dom.node(text).data, NodeData::Text("goodbye".into()));

        // Wrong kind of node: refused, not silently coerced.
        assert!(!dom.set_text(div, "nope"));
        assert!(!dom.set_attr(text, "id", "nope"));
        assert!(!dom.remove_attr(text, "id"));
    }

    #[test]
    fn every_edit_bumps_the_version_and_nothing_else_does() {
        let mut dom = Dom::new_document();
        let el = dom.create_element("div", vec![]);
        let text = dom.create_text("t");

        let mut last = dom.version();
        let mut bumped = |dom: &Dom, what: &str| {
            assert!(dom.version() > last, "{what} did not bump the version");
            last = dom.version();
        };
        dom.append(dom.root, el).unwrap();
        bumped(&dom, "append");
        dom.append(el, text).unwrap();
        bumped(&dom, "append");
        dom.set_attr(el, "id", "x");
        bumped(&dom, "set_attr");
        dom.set_text(text, "u");
        bumped(&dom, "set_text");
        dom.remove_attr(el, "id");
        bumped(&dom, "remove_attr");
        dom.remove(text);
        bumped(&dom, "remove");

        // Reads and refused writes are not edits.
        let quiet = dom.version();
        let _ = dom.children(dom.root).count();
        let _ = dom.attr(el, "id");
        let _ = dom.append(text, dom.root);
        let _ = dom.set_text(el, "not a text node");
        assert_eq!(dom.version(), quiet);
    }

    #[test]
    fn the_attribute_change_list_names_nodes_once_and_empties_when_read() {
        // M11.3's input. A scoped restyle is only as sound as this list: a node
        // written to and *not* reported is a page that silently stops updating.
        let (mut dom, div, text, _) = sample();

        assert_eq!(dom.take_attr_changes(), AttrChanges::Nodes(vec![]));

        dom.set_attr(div, "class", "one");
        dom.set_attr(div, "class", "two");
        dom.set_attr(div, "data-x", "1");
        dom.remove_attr(div, "data-x");
        assert_eq!(
            dom.take_attr_changes(),
            AttrChanges::Nodes(vec![div]),
            "four writes to one node are one node"
        );
        // Reading is taking: the next tick starts empty, or it would restyle
        // subtrees for writes already accounted for.
        assert_eq!(dom.take_attr_changes(), AttrChanges::Nodes(vec![]));

        // Refused writes are not changes, and structural edits are not this
        // list's business — they restyle the document either way.
        dom.set_attr(text, "id", "nope");
        dom.set_text(text, "changed");
        let fresh = dom.create_element("p", vec![("id".into(), "born-with-it".into())]);
        dom.append(div, fresh).unwrap();
        dom.remove(fresh);
        assert_eq!(dom.take_attr_changes(), AttrChanges::Nodes(vec![]));
    }

    #[test]
    fn too_many_attribute_writes_report_too_many_rather_than_growing() {
        // The M10.13 shape: a page that touches everything must not be able to
        // buy memory with attribute writes, and by then the subtrees it dirtied
        // add up to the document anyway.
        let mut dom = Dom::new_document();
        let nodes: Vec<NodeId> = (0..MAX_TRACKED_ATTR_CHANGES + 1)
            .map(|_| {
                let el = dom.create_element("div", vec![]);
                dom.append(dom.root, el).unwrap();
                el
            })
            .collect();

        for &node in &nodes[..MAX_TRACKED_ATTR_CHANGES] {
            dom.set_attr(node, "class", "x");
        }
        let AttrChanges::Nodes(listed) = dom.take_attr_changes() else {
            panic!("the cap itself must still be a list");
        };
        assert_eq!(listed.len(), MAX_TRACKED_ATTR_CHANGES);

        for &node in &nodes {
            dom.set_attr(node, "class", "y");
        }
        assert_eq!(dom.take_attr_changes(), AttrChanges::TooMany);
        // And giving up is also one tick's answer: the next one starts clean.
        assert_eq!(dom.take_attr_changes(), AttrChanges::Nodes(vec![]));
    }

    #[test]
    fn debug_tree_follows_a_mutated_tree() {
        let mut dom = crate::html::parse("<div><b>one</b><i>two</i></div>");
        let body = dom.node(dom.node(dom.root).first_child.unwrap()).last_child;
        let div = dom.children(body.unwrap()).next().unwrap();
        let (b, i) = {
            let mut kids = dom.children(div);
            (kids.next().unwrap(), kids.next().unwrap())
        };

        // Reorder, remove, and add — the dump must describe the tree as it is
        // now, not as it was parsed.
        dom.insert_before(div, i, b).unwrap();
        let fresh = dom.create_element("em", vec![("class".into(), "new".into())]);
        dom.append(div, fresh).unwrap();
        let dropped = dom.create_text("never inserted");
        assert!(dom.node(dropped).parent.is_none());

        let dump = crate::html::debug_tree(&dom);
        let order: Vec<&str> = dump
            .lines()
            .filter(|l| l.trim_start().starts_with('<') && !l.contains("doctype"))
            .map(|l| l.trim())
            .collect();
        assert_eq!(
            order,
            vec![
                "<html>",
                "<head>",
                "<body>",
                "<div>",
                "<i>",
                "<b>",
                "<em class=\"new\">"
            ]
        );
        assert!(!dump.contains("never inserted"));

        dom.remove(div);
        let dump = crate::html::debug_tree(&dom);
        assert!(
            !dump.contains("<div>"),
            "a removed subtree still prints:\n{dump}"
        );
        assert!(!dump.contains("one"));
    }

    /// A three-line LCG, so the sequence is identical on every machine and a
    /// failure can be replayed. Not a crate — the fuzz needs numbers, not a
    /// dependency (CLAUDE.md rule 1).
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn mutation_fuzz_keeps_every_link_consistent() {
        // The arena is capped so `check_links` stays O(small) per operation;
        // the point is the density of edits, not the size of the tree.
        const OPS: usize = 10_000;
        const CAP: usize = 400;

        let mut dom = crate::html::parse("<div><p>seed</p><span>text</span></div>");
        let mut rng = Lcg(0x5EED);

        for _ in 0..OPS {
            let count = dom.node_count();
            let victim = NodeId(rng.below(count) as u32);
            let parent = NodeId(rng.below(count) as u32);

            match rng.below(6) {
                0 if count < CAP => {
                    dom.create_element("div", vec![]);
                }
                1 if count < CAP => {
                    dom.create_text("t");
                }
                2 => {
                    // Refusals are the interesting half: the tree must be
                    // untouched by one, not half-edited.
                    let _ = dom.append(parent, victim);
                }
                3 => {
                    let kids: Vec<NodeId> = dom.children(parent).collect();
                    if !kids.is_empty() {
                        let reference = kids[rng.below(kids.len())];
                        let _ = dom.insert_before(parent, victim, reference);
                    }
                }
                4 => dom.remove(victim),
                _ => {
                    dom.set_attr(victim, "data-x", "1");
                    dom.set_text(victim, "mutated");
                }
            }

            check_links(&dom);
        }

        // Still a walkable document after all of that.
        assert!(dom.node_count() >= CAP.min(dom.node_count()));
        let _ = crate::html::debug_tree(&dom);
        check_links(&dom);
    }

    #[test]
    fn arena_growth_is_unbounded() {
        // Nothing is ever freed, because ids must never be reused. A page that
        // appends and removes in a loop therefore grows the arena forever, and
        // this test exists to keep the number honest rather than to forbid it.
        const PAIRS: usize = 100_000;

        let mut dom = Dom::new_document();
        let host = dom.create_element("div", vec![]);
        dom.append(dom.root, host).unwrap();

        for _ in 0..PAIRS {
            let child = dom.create_text("x");
            dom.append(host, child).unwrap();
            dom.remove(child);
        }

        // Document + host + one abandoned node per pair, none reclaimed.
        assert_eq!(dom.node_count(), 2 + PAIRS);
        assert_eq!(dom.children(host).count(), 0);

        let bytes: usize = dom.nodes.capacity() * std::mem::size_of::<Node>()
            + dom
                .nodes
                .iter()
                .map(|n| match &n.data {
                    NodeData::Text(s) | NodeData::Comment(s) | NodeData::Doctype(s) => s.capacity(),
                    NodeData::Element { tag, attrs } => {
                        tag.capacity()
                            + attrs
                                .iter()
                                .map(|(k, v)| k.capacity() + v.capacity())
                                .sum::<usize>()
                    }
                    NodeData::Document => 0,
                })
                .sum::<usize>();
        eprintln!(
            "ARENA GROWTH: {} nodes, ~{} bytes ({:.1} MB) after {PAIRS} append/remove pairs",
            dom.node_count(),
            bytes,
            bytes as f64 / (1024.0 * 1024.0)
        );
    }
}
