//! The layout tree: boxes with positions, distinct from the DOM (PLAN.md M5).
//!
//! Built by `engine` as a pure transform of DOM + styles + width. Each box
//! carries its own geometry; paint walks this tree into a display list, and F3
//! draws outlines from the same coordinates. Anonymous boxes exist so mixed
//! block/inline children of a block still form a legal CSS box tree.

use crate::dom::NodeId;
use crate::layout::dimensions::Dimensions;
use crate::style::ComputedStyle;
use crate::term::Style;

/// Index into `LayoutTree::boxes`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoxId(pub u32);

/// What kind of box this is — drives paint and the F3 label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoxKind {
    /// A block-level element box.
    Block,
    /// Anonymous block wrapping consecutive inlines inside a block container.
    AnonymousBlock,
    /// One line box inside an inline formatting context.
    Line,
    /// A fragment of text (may be one of several for a single text node that
    /// wrapped across lines).
    Text,
    /// An inline-level element that contributed no text of its own (used so
    /// F3 / hit-testing can still name the element). Rarely painted alone.
    Inline,
}

/// One node of the layout tree.
#[derive(Clone, Debug)]
pub struct LayoutBox {
    pub kind: BoxKind,
    /// DOM node this box was generated for, if any. Anonymous and line boxes
    /// have `None`; text fragments point at their text node.
    pub node: Option<NodeId>,
    pub dimensions: Dimensions,
    pub children: Vec<BoxId>,
    /// For `BoxKind::Text`: the characters drawn in this fragment.
    pub text: Option<String>,
    /// Terminal style for text fragments (cascade → attrs + colour).
    pub term_style: Style,
    /// Computed style for the generating element — backgrounds, borders, and
    /// F3. Anonymous/line/text boxes carry `Default`.
    pub computed: ComputedStyle,
}

/// The laid-out page: an arena of boxes plus the root and the content height.
#[derive(Clone, Debug)]
pub struct LayoutTree {
    pub boxes: Vec<LayoutBox>,
    pub root: BoxId,
    /// Content width the tree was laid out at (the column width).
    pub width: i32,
    /// Total height of the root margin box — the scroll range.
    pub height: i32,
}

impl LayoutTree {
    pub fn get(&self, id: BoxId) -> &LayoutBox {
        &self.boxes[id.0 as usize]
    }

    /// Depth-first walk in paint order.
    pub fn walk(&self, id: BoxId, f: &mut dyn FnMut(BoxId, &LayoutBox)) {
        let b = self.get(id);
        f(id, b);
        for &child in &b.children {
            self.walk(child, f);
        }
    }
}
