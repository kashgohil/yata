//! The layout tree: boxes with positions, distinct from the DOM (PLAN.md M5).
//!
//! Built by `engine` as a pure transform of DOM + styles + width. Each box
//! carries its own geometry; paint walks this tree into a display list, and F3
//! draws outlines from the same coordinates. Anonymous boxes exist so mixed
//! block/inline children of a block still form a legal CSS box tree.

use crate::dom::NodeId;
use crate::layout::clip::Clip;
use crate::layout::dimensions::Dimensions;
use crate::layout::field::FieldPaint;
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
    /// A flex container (M9.6). Block-level and painted exactly like `Block` —
    /// backgrounds, borders and clipping know nothing about flex. The kind
    /// exists because what is *inside* it was placed by a different algorithm,
    /// and F3 has to be able to say so.
    Flex,
    /// A table formatting root. Its children are [`TableRow`](Self::TableRow)
    /// boxes rather than ordinary block-flow siblings.
    Table,
    /// One row owned by a [`Table`](Self::Table).
    TableRow,
    /// One cell owned by a [`TableRow`](Self::TableRow). Its children use the
    /// normal block/inline formatting machinery.
    TableCell,
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
    /// Replaced `<img>`: fixed cell rectangle, painted as half-blocks / Kitty.
    Image,
    /// A form control (M11.8): a fixed cell rectangle carrying the text it
    /// shows and no children, sized in *characters* by `size` / `cols` / `rows`.
    ///
    /// The payload is what the box draws rather than what it holds — a
    /// `password`'s value is masked before it ever reaches a box, and a
    /// `<textarea>`'s content is a value here rather than the prose it looks
    /// like in the DOM.
    Field(FieldPaint),
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
    /// For `BoxKind::Image`: alt text (placeholder / dump-text).
    pub text: Option<String>,
    /// Terminal style for text fragments (cascade → attrs + colour).
    pub term_style: Style,
    /// Computed style for the generating element — backgrounds, borders, and
    /// F3. Anonymous/line/text boxes carry `Default`.
    pub computed: ComputedStyle,
    /// Absolute image URL for `BoxKind::Image` (paint looks up pixels).
    pub image_src: Option<String>,
    /// When true, a late decode must not force relayout (attrs or known size).
    pub image_size_firm: bool,
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
    /// Resolved table grid rules. These are final layout output: paint never
    /// has to infer spans or inspect the DOM.
    pub grid_borders: Vec<GridBorder>,
}

/// One horizontal or vertical rule in a resolved table grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GridBorder {
    /// The table that owns this rule, used only to carry its normal ancestor
    /// clip into paint. Paint still never discovers the grid from the DOM.
    pub owner: BoxId,
    pub x: i32,
    pub y: i32,
    pub length: i32,
    pub horizontal: bool,
    /// The winning adjacent resolved border width, in terminal cells.
    pub thickness: i32,
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

    /// Depth-first walk in paint order, carrying the [`Clip`] each box's own
    /// output is confined to (M9.3). Paint, hit-testing and `/` search all go
    /// through this so they cannot disagree about what the reader can see.
    ///
    /// Subtrees under a collapsed clip are skipped entirely: a clip only ever
    /// narrows, so nothing inside one could reach the screen.
    pub fn walk_clipped(&self, f: &mut dyn FnMut(BoxId, &LayoutBox, Clip)) {
        self.walk_clipped_from(self.root, Clip::NONE, f);
    }

    fn walk_clipped_from(&self, id: BoxId, clip: Clip, f: &mut dyn FnMut(BoxId, &LayoutBox, Clip)) {
        let b = self.get(id);
        f(id, b, clip);
        let inside = clip.inside(b);
        if inside.is_empty() {
            return;
        }
        for &child in &b.children {
            self.walk_clipped_from(child, inside, f);
        }
    }
}
