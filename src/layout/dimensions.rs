//! Geometry for the layout tree: everything is in terminal cells.
//!
//! Content / padding / border / margin match the CSS box model. Coordinates
//! are absolute within the page column (origin at the top-left of the root
//! content area). Paint and F3 read these; scrolling subtracts an offset.

/// A rectangle in cell coordinates. `x`/`y` are the content-box origin.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn right(self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(self) -> i32 {
        self.y + self.height
    }

    pub fn expanded_by(self, edge: EdgeSizes) -> Rect {
        Rect {
            x: self.x - edge.left,
            y: self.y - edge.top,
            width: self.width + edge.left + edge.right,
            height: self.height + edge.top + edge.bottom,
        }
    }
}

/// Four sides, already resolved to cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EdgeSizes {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl EdgeSizes {
    pub const ZERO: EdgeSizes = EdgeSizes {
        top: 0,
        right: 0,
        bottom: 0,
        left: 0,
    };
}

/// The CSS box model for one layout box, in cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Dimensions {
    pub content: Rect,
    pub padding: EdgeSizes,
    pub border: EdgeSizes,
    pub margin: EdgeSizes,
}

impl Dimensions {
    pub fn padding_box(self) -> Rect {
        self.content.expanded_by(self.padding)
    }

    pub fn border_box(self) -> Rect {
        self.padding_box().expanded_by(self.border)
    }

    pub fn margin_box(self) -> Rect {
        self.border_box().expanded_by(self.margin)
    }

    /// Width of the margin box (used when centering a block in its container).
    pub fn margin_box_width(self) -> i32 {
        self.content.width
            + self.padding.left
            + self.padding.right
            + self.border.left
            + self.border.right
            + self.margin.left
            + self.margin.right
    }
}
