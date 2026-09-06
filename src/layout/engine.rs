//! Layout engine: DOM + styles + width → positioned box tree (PLAN.md M5).
//!
//! Pure transform. Block boxes stack vertically with margin collapse between
//! adjacent siblings; inline content fills line boxes with unicode-width
//! wrapping. Unit conversion lives on `Length` (M5.1).

use crate::dom::{Dom, NodeData, NodeId};
use crate::image::ImageContext;
use crate::layout::boxes::{
    BoxId, BoxKind, GridBorder, GridLayout, LayoutBox, LayoutTree, StickyConstraint,
};
use crate::layout::field::{self, FieldPaint};
use std::ops::Range;

use crate::layout::dimensions::{Dimensions, EdgeSizes, Rect};
use crate::layout::flex;
use crate::layout::intrinsic::IntrinsicSizer;
use crate::style::values::{
    AlignItems, BoxSizing, Display, FlexBasis, FlexDirection, FlexWrap, FontStyle, FontWeight,
    Gaps, GridMax, GridMin, GridPlacement, GridTrack, Length, Position, TextAlign,
};
use crate::style::{ComputedStyle, Styles};
use crate::term::{Attrs, Color, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn is_out_of_flow(position: Position) -> bool {
    matches!(position, Position::Absolute | Position::Fixed)
}

/// The bullet the engine puts before a list item's content, and the two cells
/// it takes.
///
/// Shared with intrinsic sizing (M9.6) rather than spelled twice: an `<li>`
/// that measures two cells narrower than it lays out is a flex item whose base
/// size is too small and whose automatic minimum size is too small, so text the
/// algorithm believed would fit wraps instead. One `const` is the whole fix.
pub(super) const LIST_MARKER: &str = "• ";

/// Whether `display:none` is honoured on this pass (M4 review never-blank).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hidden {
    Respect,
    Reveal,
}

/// Lay the document out. Returns the box tree; callers that still need lines
/// (dump-text, transition paint) rasterise via `super::lines::from_tree`.
pub fn layout_tree(dom: &Dom, styles: &Styles, width: u16, hidden: Hidden) -> LayoutTree {
    layout_tree_with(dom, styles, width, hidden, &ImageContext::default())
}

/// Layout with image metrics (M8). `images` is a pure snapshot of known sizes
/// and discovered `<img>` nodes — no network, no mutation.
pub fn layout_tree_with(
    dom: &Dom,
    styles: &Styles,
    width: u16,
    hidden: Hidden,
    images: &ImageContext,
) -> LayoutTree {
    layout_tree_with_viewport(dom, styles, width, 1, hidden, images)
}

/// Layout with the bounded terminal page rectangle available to viewport
/// anchored boxes. Normal document flow remains vertically indefinite.
pub fn layout_tree_with_viewport(
    dom: &Dom,
    styles: &Styles,
    width: u16,
    viewport_height: u16,
    hidden: Hidden,
    images: &ImageContext,
) -> LayoutTree {
    let width = width.max(1) as i32;
    let viewport_height = viewport_height.max(1) as i32;
    let mut eng = Engine {
        dom,
        styles,
        hidden,
        images,
        // A visible DOM node usually produces at least one box. Reserving the
        // arena size avoids repeatedly moving the large `LayoutBox` values on
        // real articles; anonymous line boxes can grow beyond this naturally.
        boxes: Vec::with_capacity(dom.node_count()),
        sizer: IntrinsicSizer::new(dom, styles, images, hidden),
        grid_borders: Vec::new(),
        grid_item_depth: 0,
    };
    // Synthetic root: full column width, no margins of its own. Children of
    // the document (html) are laid out into it.
    let root = eng.alloc(LayoutBox {
        kind: BoxKind::Block,
        node: Some(dom.root),
        dimensions: Dimensions {
            content: Rect {
                x: 0,
                y: 0,
                width,
                height: viewport_height,
            },
            ..Dimensions::default()
        },
        children: Vec::new(),
        text: None,
        term_style: Style::default(),
        computed: ComputedStyle::default(),
        image_src: None,
        image_size_firm: false,
        fixed_viewport: false,
        sticky: None,
        grid: None,
    });
    let mut y = 0i32;
    let mut prev_mb = 0i32;
    for child in dom.children(dom.root) {
        // `None` height: the page column scrolls, so it has no definite height
        // for a percentage to resolve against. `height: 100%` on a top-level
        // element therefore means "as tall as my content", never "as tall as
        // the terminal" (CSS 2.1 §10.5, and M9.2's definiteness rule).
        if matches!(dom.node(child).data, NodeData::Element { .. })
            && is_out_of_flow(styles.get(child).position)
        {
            if let Some(id) = eng.layout_positioned(child, root, false) {
                eng.boxes[root.0 as usize].children.push(id);
            }
        } else if let Some(id) = eng.layout_node(child, 0, width, None, y, &mut prev_mb, false) {
            eng.boxes[root.0 as usize].children.push(id);
            let mb = eng.boxes[id.0 as usize].dimensions.margin_box();
            let dy = if styles.get(child).position == Position::Relative {
                relative_delta(styles.get(child).clone(), width, None).1
            } else {
                0
            };
            y = mb.bottom().saturating_sub(dy);
            prev_mb = eng.boxes[id.0 as usize].dimensions.margin.bottom;
        }
    }
    // The document is as tall as the flow *or* as the lowest row anything can
    // paint into, whichever is further down. Those differ once a box is
    // shorter than its content (M9.2's specified heights): with the initial
    // `overflow: visible` the overflowing rows are still on screen, so they
    // have to be inside the scrollable page, or `lines::from_tree` would drop
    // them.
    //
    // Rows a clip swallowed (M9.3) are not: asking the boxes for their
    // *visible* rectangle is what keeps a `height: 0; overflow: hidden` menu
    // from leaving the page scrolling through the blank rows where its content
    // would have been. This needs the finished tree, because a clip is a fact
    // about a box's ancestors.
    // The viewport was temporarily available as the synthetic root's height
    // while fixed descendants resolved their insets. It is not document flow.
    eng.boxes[root.0 as usize].dimensions.content.height = 0;
    let mut tree = LayoutTree {
        boxes: eng.boxes,
        root,
        width,
        height: 0,
        grid_borders: eng.grid_borders,
    };
    let mut lowest = 0;
    tree.walk_clipped(&mut |_, b, clip| {
        let visible = clip.apply(b.dimensions.border_box());
        if visible.height > 0 {
            lowest = lowest.max(visible.bottom());
        }
    });
    let height = y.max(lowest).max(0);
    tree.boxes[root.0 as usize].dimensions.content.height = height;
    tree.height = height;
    attach_sticky_constraints(&mut tree, dom, styles, viewport_height);
    tree
}

fn attach_sticky_constraints(tree: &mut LayoutTree, dom: &Dom, styles: &Styles, viewport_h: i32) {
    for index in 0..tree.boxes.len() {
        let Some(node) = tree.boxes[index].node else {
            continue;
        };
        let style = styles.get(node).clone();
        let Some(inset) = (style.position == Position::Sticky)
            .then(|| inset_v(style.top, tree.width, Some(viewport_h)))
            .flatten()
        else {
            continue;
        };
        let static_rect = tree.boxes[index].dimensions.margin_box();
        if static_rect.height > viewport_h {
            continue;
        }
        let mut ancestor = dom.node(node).parent;
        let mut containing_end = tree.boxes[tree.root.0 as usize].dimensions.content.bottom();
        while let Some(parent) = ancestor {
            if let Some(box_) = tree.boxes.iter().rev().find(|b| b.node == Some(parent)) {
                containing_end = box_.dimensions.padding_box().bottom();
                break;
            }
            ancestor = dom.node(parent).parent;
        }
        let constraint = StickyConstraint {
            static_start: static_rect.y,
            static_end: static_rect.bottom(),
            inset,
            containing_end,
        };
        mark_sticky_subtree(tree, BoxId(index as u32), constraint);
    }
}

fn mark_sticky_subtree(tree: &mut LayoutTree, id: BoxId, constraint: StickyConstraint) {
    tree.boxes[id.0 as usize].sticky = Some(constraint);
    let children = tree.boxes[id.0 as usize].children.clone();
    for child in children {
        mark_sticky_subtree(tree, child, constraint);
    }
}

struct Engine<'a> {
    dom: &'a Dom,
    styles: &'a Styles,
    hidden: Hidden,
    images: &'a ImageContext,
    boxes: Vec<LayoutBox>,
    /// How wide subtrees *want* to be (M9.4), memoized for this pass.
    ///
    /// Flex base sizes and automatic minimum sizes are the only callers, and
    /// they ask about items whose subtrees nest — the memo is what keeps that
    /// linear. It measures without allocating a box, which is why an item's
    /// contents are laid out exactly once per pass and not twice.
    sizer: IntrinsicSizer<'a>,
    grid_borders: Vec<GridBorder>,
    /// A direct grid item has already received its final inline-size. Auto
    /// tables normally shrink-wrap, but doing that here would discard the
    /// resolved grid cell width before table layout sees it.
    grid_item_depth: usize,
}

impl<'a> Engine<'a> {
    fn alloc(&mut self, b: LayoutBox) -> BoxId {
        let id = BoxId(self.boxes.len() as u32);
        self.boxes.push(b);
        id
    }

    fn is_hidden(&self, id: NodeId) -> bool {
        let c = self.styles.get(id);
        c.display == Display::None && (self.hidden == Hidden::Respect || c.hidden_by_ua)
    }

    /// Layout one DOM node as a child of a block container. Returns `None` for
    /// nodes that generate no box (`display:none`, comments, empty whitespace
    /// between blocks).
    #[allow(clippy::too_many_arguments)]
    fn layout_node(
        &mut self,
        id: NodeId,
        x: i32,
        containing_width: i32,
        // The containing block's content height when it is definite (M9.2):
        // what a percentage `height` resolves against, `None` when there is
        // nothing definite to resolve against.
        containing_height: Option<i32>,
        y: i32,
        prev_margin_bottom: &mut i32,
        pre: bool,
    ) -> Option<BoxId> {
        match &self.dom.node(id).data {
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => None,
            NodeData::Text(text) => {
                // Whitespace-only text between blocks does not make a box of
                // its own; inline layout consumes real text runs.
                if text.chars().all(is_html_space) {
                    return None;
                }
                // Bare text as a direct block child becomes an anonymous block
                // containing one IFC — handled by the parent's child walk via
                // the inline collector. Reaching here means a top-level text
                // under the document root; treat as a block of text.
                self.layout_anonymous_block(
                    x,
                    containing_width,
                    y,
                    prev_margin_bottom,
                    &[InlineItem::Text {
                        node: id,
                        text: text.clone(),
                        style: term_style(self.styles.get(id)),
                        computed: self.styles.get(id).clone(),
                    }],
                    TextAlign::Left,
                    pre,
                )
            }
            NodeData::Element { tag, .. } => {
                if self.is_hidden(id) {
                    return None;
                }
                let mut computed = self.styles.get(id).clone();
                // Reveal pass: a page's own `display:none` is treated as block so
                // its content can be read. UA-important hiding never reaches here
                // (`is_hidden` still catches it).
                if computed.display == Display::None && self.hidden == Hidden::Reveal {
                    computed.display = Display::Block;
                }
                // A control this engine draws as nothing generates no box at
                // all — not an empty one (M11.8). Asked before `display`,
                // because `<input type=hidden>` is not hidden by the cascade:
                // it is a control whose rendering is "none of it".
                if field::generates_no_box(self.dom, id, tag) {
                    return None;
                }
                let laid_out = match tag.as_str() {
                    "br" => {
                        self.layout_br(x, containing_width, y, prev_margin_bottom, computed.clone())
                    }
                    "hr" => {
                        self.layout_hr(x, containing_width, y, prev_margin_bottom, computed.clone())
                    }
                    "img" => self.layout_img_block(
                        id,
                        computed.clone(),
                        x,
                        containing_width,
                        y,
                        prev_margin_bottom,
                    ),
                    _ => {
                        if computed.display == Display::None {
                            None
                        } else if is_block_level(computed.display) {
                            self.layout_block(
                                id,
                                tag,
                                computed.clone(),
                                x,
                                containing_width,
                                containing_height,
                                y,
                                prev_margin_bottom,
                                pre || tag == "pre",
                            )
                        } else {
                            // Inline element as a block-container child: wrap.
                            let items =
                                self.collect_inline(id, pre || tag == "pre", containing_width);
                            if items.is_empty() {
                                return None;
                            }
                            self.layout_anonymous_block(
                                x,
                                containing_width,
                                y,
                                prev_margin_bottom,
                                &items,
                                TextAlign::Left,
                                pre || tag == "pre",
                            )
                        }
                    }
                };
                if matches!(computed.position, Position::Relative | Position::Sticky)
                    && let Some(box_id) = laid_out
                {
                    self.apply_relative(box_id, computed, containing_width, containing_height);
                }
                laid_out
            }
        }
    }

    /// Layout one out-of-flow child after its in-flow siblings have chosen
    /// their static geometry.  Ownership remains with the ordinary parent;
    /// only its rectangle is selected from the nearest positioned ancestor.
    fn layout_positioned(&mut self, id: NodeId, normal_parent: BoxId, pre: bool) -> Option<BoxId> {
        let NodeData::Element { tag, .. } = &self.dom.node(id).data else {
            return None;
        };
        if self.is_hidden(id) || field::generates_no_box(self.dom, id, tag) {
            return None;
        }
        let fixed = self.styles.get(id).position == Position::Fixed;
        let (cb, cb_height) = if fixed {
            // The synthetic root's rectangle is the terminal page viewport in
            // this engine: fixed boxes never consult positioned ancestors.
            (
                self.boxes[0].dimensions.content,
                Some(self.boxes[0].dimensions.content.height.max(1)),
            )
        } else {
            self.containing_block(id, normal_parent)
        };
        // A missing inset retains the static position the normal parent would
        // have supplied.  This differs from the containing block whenever a
        // static intermediate ancestor sits between this child and its nearest
        // positioned ancestor.
        let static_origin = self.static_origin(id, normal_parent);
        let style = self.styles.get(id).clone();
        if tag == "img" {
            let img = self.images.by_node.get(&id)?;
            let (intrinsic_w, intrinsic_h, firm) = self.images.size_for(img, cb.width.max(1));
            let width = if style.width.is_auto() {
                intrinsic_w.min(cb.width.max(1)).max(1)
            } else {
                style.width.to_cells_h(cb.width).max(1)
            };
            let left = inset_h(style.left, cb.width);
            let right = inset_h(style.right, cb.width);
            let top = inset_v(style.top, cb.width, cb_height);
            let bottom = cb_height.and_then(|height| inset_v(style.bottom, cb.width, Some(height)));
            let x = left.map(|n| cb.x.saturating_add(n)).unwrap_or_else(|| {
                right
                    .map(|n| cb.right().saturating_sub(n).saturating_sub(width))
                    .unwrap_or(static_origin.x)
            });
            let y = top.map(|n| cb.y.saturating_add(n)).unwrap_or_else(|| {
                bottom
                    .map(|n| cb.bottom().saturating_sub(n).saturating_sub(intrinsic_h))
                    .unwrap_or(static_origin.y)
            });
            let image = self.alloc(LayoutBox {
                kind: BoxKind::Image,
                node: Some(id),
                dimensions: Dimensions {
                    content: Rect {
                        x,
                        y,
                        width,
                        height: intrinsic_h.max(1),
                    },
                    ..Dimensions::default()
                },
                children: Vec::new(),
                text: Some(img.alt.clone()),
                term_style: Style::default(),
                computed: style,
                image_src: Some(img.url.clone()),
                image_size_firm: firm,
                fixed_viewport: fixed,
                sticky: None,
                grid: None,
            });
            return Some(image);
        }
        let intrinsic = field::control(self.dom, id, tag).map(|c| c.cols);
        let mut dims = resolve_block_dims(&style, cb.width.max(1), intrinsic);
        let left = inset_h(style.left, cb.width);
        let right = inset_h(style.right, cb.width);
        if style.width.is_auto()
            && let (Some(left), Some(right)) = (left, right)
        {
            let edges = dims.margin.left
                + dims.margin.right
                + dims.border.left
                + dims.border.right
                + dims.padding.left
                + dims.padding.right;
            dims.content.width = cb
                .width
                .saturating_sub(left)
                .saturating_sub(right)
                .saturating_sub(edges)
                .max(1);
        }
        let outer_w = dims.margin_box_width();
        let margin_x = if let Some(left) = left {
            cb.x.saturating_add(left)
        } else if let Some(right) = right {
            cb.right().saturating_sub(right).saturating_sub(outer_w)
        } else {
            static_origin.x
        };
        dims.content.x = margin_x
            .saturating_add(dims.margin.left)
            .saturating_add(dims.border.left)
            .saturating_add(dims.padding.left);
        dims.content.y =
            cb.y.saturating_add(dims.margin.top)
                .saturating_add(dims.border.top)
                .saturating_add(dims.padding.top);
        let box_id =
            self.layout_box_at(id, tag, style.clone(), dims, cb_height, pre || tag == "pre");
        let top = inset_v(style.top, cb.width, cb_height);
        // An end inset needs an end edge. The scrolling document root has no
        // definite vertical end, so a bottom-only child keeps its static row.
        let bottom = cb_height.and_then(|height| inset_v(style.bottom, cb.width, Some(height)));
        let mut target_y = if let Some(top) = top {
            cb.y.saturating_add(top)
        } else if let Some(bottom) = bottom {
            cb.bottom()
                .saturating_sub(bottom)
                .saturating_sub(self.boxes[box_id.0 as usize].dimensions.margin_box().height)
        } else {
            static_origin.y
        };
        if let (Some(top), Some(bottom), Some(cb_height)) = (top, bottom, cb_height)
            && style.height.is_auto()
        {
            let available = cb_height
                .saturating_sub(top)
                .saturating_sub(bottom)
                .saturating_sub(self.boxes[box_id.0 as usize].dimensions.margin.top)
                .saturating_sub(self.boxes[box_id.0 as usize].dimensions.margin.bottom)
                .saturating_sub(
                    self.boxes[box_id.0 as usize].dimensions.border.top
                        + self.boxes[box_id.0 as usize].dimensions.border.bottom
                        + self.boxes[box_id.0 as usize].dimensions.padding.top
                        + self.boxes[box_id.0 as usize].dimensions.padding.bottom,
                )
                .max(0);
            self.boxes[box_id.0 as usize].dimensions.content.height = available;
            target_y = cb.y.saturating_add(top);
        }
        let current = self.boxes[box_id.0 as usize].dimensions.margin_box();
        self.shift_subtree(
            box_id,
            margin_x.saturating_sub(current.x),
            target_y.saturating_sub(current.y),
        );
        if fixed {
            self.mark_fixed_subtree(box_id);
        }
        Some(box_id)
    }

    fn mark_fixed_subtree(&mut self, id: BoxId) {
        self.boxes[id.0 as usize].fixed_viewport = true;
        let children = self.boxes[id.0 as usize].children.clone();
        for child in children {
            self.mark_fixed_subtree(child);
        }
    }

    /// The ordinary-flow position a positioned child would have started at.
    /// Out-of-flow layout happens after a parent's in-flow children are known,
    /// so their final rectangles are a stable, layout-only source for the
    /// no-inset fallback — no second formatting pass is necessary.
    fn static_origin(&self, id: NodeId, parent: BoxId) -> Rect {
        let parent_box = &self.boxes[parent.0 as usize];
        let Some(parent_node) = parent_box.node else {
            return parent_box.dimensions.content;
        };
        let mut origin = parent_box.dimensions.content;
        for sibling in self.dom.children(parent_node) {
            if sibling == id {
                break;
            }
            if let Some(box_) = self.boxes.iter().rev().find(|b| b.node == Some(sibling)) {
                origin.y = origin.y.max(box_.dimensions.margin_box().bottom());
            }
        }
        origin
    }

    fn containing_block(&self, id: NodeId, fallback: BoxId) -> (Rect, Option<i32>) {
        let mut node = self.dom.node(id).parent;
        while let Some(ancestor) = node {
            if self.styles.get(ancestor).position != Position::Static
                && let Some(b) = self.boxes.iter().rev().find(|b| b.node == Some(ancestor))
            {
                let mut padding = b.dimensions.padding_box();
                // During a parent's content pass its final height has not been
                // committed yet. An explicit px/em height is nevertheless
                // definite for descendants, so recover it here rather than
                // making bottom-only placement wait for a second layout pass.
                let definite = definite_v(b.computed.height, None).map(|h| {
                    Axis {
                        edges: b.dimensions.padding.top
                            + b.dimensions.padding.bottom
                            + b.dimensions.border.top
                            + b.dimensions.border.bottom,
                        box_sizing: b.computed.box_sizing,
                    }
                    .content_from(h)
                    .saturating_add(b.dimensions.padding.top)
                    .saturating_add(b.dimensions.padding.bottom)
                });
                if let Some(height) = definite {
                    padding.height = height;
                }
                return (padding, definite);
            }
            node = self.dom.node(ancestor).parent;
        }
        // The synthetic document root is the initial containing block.
        (
            self.boxes
                .first()
                .map(|b| b.dimensions.content)
                .unwrap_or(self.boxes[fallback.0 as usize].dimensions.content),
            None,
        )
    }

    fn apply_relative(
        &mut self,
        box_id: BoxId,
        style: ComputedStyle,
        width: i32,
        height: Option<i32>,
    ) {
        let (dx, dy) = if style.position == Position::Sticky {
            // `top` is the sticky constraint, not an initial relative shift.
            // Start-edge horizontal sticking is outside terminal scrolling;
            // `left`/`right` and `bottom` retain their M11.17 relative role.
            (
                inset_h(style.left, width)
                    .or_else(|| inset_h(style.right, width).map(|n| -n))
                    .unwrap_or(0),
                inset_v(style.bottom, width, height).map_or(0, |n| -n),
            )
        } else {
            relative_delta(style, width, height)
        };
        self.shift_subtree(box_id, dx, dy);
    }

    // Geometry args are the block-layout signature; grouping them would
    // obscure the pure-transform call sites more than the lint helps.
    #[allow(clippy::too_many_arguments)]
    fn layout_block(
        &mut self,
        id: NodeId,
        tag: &str,
        computed: ComputedStyle,
        containing_x: i32,
        containing_width: i32,
        containing_height: Option<i32>,
        y: i32,
        prev_margin_bottom: &mut i32,
        pre: bool,
    ) -> Option<BoxId> {
        // CSS 2.1 §10.3.4: a block-level **replaced** box with `width: auto` is
        // as wide as it intrinsically is, not as wide as its containing block.
        // A field is `size` characters wide wherever the page puts it, and
        // `display: block` on a form's inputs is a stylesheet away.
        let intrinsic = field::control(self.dom, id, tag).map(|c| c.cols);
        let mut dims = resolve_block_dims(&computed, containing_width, intrinsic);
        // Adjacent-sibling margin collapse: place with max of the two margins.
        // The caller's `prev_margin_bottom` is 0 for the first in-flow child;
        // we still apply this box's own top margin then, except at the very
        // start of the page (y == 0 after undoing the previous reservation),
        // where a leading blank row would only push content down for no
        // reason — M3 never did that.
        let top_margin = dims.margin.top;
        let y_after_prev = y - *prev_margin_bottom;
        let used_top = if y_after_prev == 0 && *prev_margin_bottom == 0 {
            0
        } else {
            top_margin.max(*prev_margin_bottom)
        };
        let y = y_after_prev + used_top;

        dims.margin.top = used_top;
        dims.content.x = containing_x + dims.margin.left + dims.border.left + dims.padding.left;
        dims.content.y = y + dims.border.top + dims.padding.top;
        // Content width already set by resolve; height filled below.
        dims.content.height = 0;

        let box_id = self.layout_box_at(id, tag, computed, dims, containing_height, pre);
        *prev_margin_bottom = self.boxes[box_id.0 as usize].dimensions.margin.bottom;
        Some(box_id)
    }

    /// Build the box for an element whose edges and content **width** are
    /// already decided and whose position is already chosen, fill it with its
    /// contents, and give it its used height (CSS 2.1 §10.5–10.7).
    ///
    /// Two callers, which is the point: a block child, whose width came from
    /// filling its containing block and whose position came from margin
    /// collapse, and a flex item (M9.6), whose width came from §9.7 and whose
    /// position came from the flex line. Everything after "how wide, and
    /// where" is the same for both, so it is written once.
    fn layout_box_at(
        &mut self,
        id: NodeId,
        tag: &str,
        computed: ComputedStyle,
        dims: Dimensions,
        containing_height: Option<i32>,
        pre: bool,
    ) -> BoxId {
        self.layout_box_at_measured(id, tag, computed, dims, containing_height, pre)
            .0
    }

    /// [`layout_box_at`](Self::layout_box_at), and also the height this box's
    /// **contents** used — which is not its used height whenever a `height`,
    /// `min-height` or `max-height` overrode them.
    ///
    /// One caller wants the second number: a column flex container's items
    /// (M9.9). §9.2's flex base size and §4.5's content size suggestion are both
    /// defined in terms of what an item's content wants, and on a vertical main
    /// axis that is a height — which `intrinsic` cannot measure, because it
    /// measures widths only. Building the item is the measurement, so the
    /// number has to come back out of the build.
    fn layout_box_at_measured(
        &mut self,
        id: NodeId,
        tag: &str,
        computed: ComputedStyle,
        dims: Dimensions,
        containing_height: Option<i32>,
        pre: bool,
    ) -> (BoxId, i32) {
        // A form control (M11.8): its box carries what it shows and no
        // children, so the value of a `<textarea>` stops being prose the moment
        // the box exists. Derived once, here, because this is the one place
        // both halves of it — the kind and the height — are needed.
        let control = field::control(self.dom, id, tag);
        let box_id = self.alloc(LayoutBox {
            kind: match &control {
                Some(c) => BoxKind::Field(FieldPaint {
                    shows: c.shows,
                    disabled: c.disabled,
                }),
                None if tag == "table" => BoxKind::Table,
                None if tag == "tr" => BoxKind::TableRow,
                None if matches!(tag, "td" | "th") => BoxKind::TableCell,
                None if lays_out_as_flex(&computed) => BoxKind::Flex,
                None if lays_out_as_grid(&computed) => BoxKind::Grid,
                None => BoxKind::Block,
            },
            node: Some(id),
            dimensions: dims,
            children: Vec::new(),
            text: control.as_ref().map(|c| c.text.clone()),
            // A control draws its own text, so it needs the cascade's colours
            // and attributes on the box; every other box here draws none.
            term_style: match &control {
                Some(_) => term_style(&computed),
                None => Style::default(),
            },
            computed: computed.clone(),
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });

        // The vertical axis (CSS 2.1 §10.5–10.7). A specified height is known
        // *before* the children are laid out, and that is exactly what makes
        // it definite: it is what percentage heights inside this box resolve
        // against. An auto height is only known afterwards, and stays
        // indefinite for the subtree.
        let v_axis = Axis {
            edges: dims.padding.top + dims.padding.bottom + dims.border.top + dims.border.bottom,
            box_sizing: computed.box_sizing,
        };
        let heights = BlockHeight {
            specified: definite_v(computed.height, containing_height)
                .map(|h| v_axis.content_from(h)),
            min: definite_v(computed.min_height, containing_height),
            max: definite_v(computed.max_height, containing_height),
        };

        let auto_height = match &control {
            // `rows` lines, and no children walked: a control's contents are
            // its value, which the box already carries.
            Some(c) => c.rows,
            None if tag == "table" => {
                self.layout_table_contents(id, computed, box_id, heights, pre)
            }
            None => self.layout_contents(id, tag, computed, box_id, heights, pre),
        };

        // Used height: the specified one if there is one, else the content's
        // (an empty div is zero rows), then the min/max clamps. Children keep
        // the boxes and positions they were given, so a box shorter than its
        // content lets that content overflow and paint past the bottom edge —
        // `overflow: visible` is the initial value, and clipping is M9.3.
        // The flow advances by *this* height, which is what makes `height: 0`
        // collapse a box whose children still exist.
        let content_height = v_axis.clamp(
            heights.specified.unwrap_or(auto_height),
            heights.min,
            heights.max,
        );
        self.boxes[box_id.0 as usize].dimensions.content.height = content_height;
        (box_id, auto_height)
    }

    /// Lay this element's children into its (already positioned) box, and
    /// return the content height they used.
    ///
    /// **The formatting-context fork.** A block container stacks its children
    /// and wraps runs of inlines in anonymous blocks; a flex container runs
    /// css-flexbox-1 §9 over them. Which one an element is, is the only thing
    /// `display: flex` decides — everything outside this function treats the
    /// two identically, which is why a flex item can be a block, a block child
    /// can be a flex container, and neither case needed a second code path.
    fn layout_contents(
        &mut self,
        id: NodeId,
        tag: &str,
        computed: ComputedStyle,
        box_id: BoxId,
        heights: BlockHeight,
        pre: bool,
    ) -> i32 {
        if lays_out_as_flex(&computed) {
            return self.layout_flex_contents(id, computed, box_id, heights, pre);
        }
        if lays_out_as_grid(&computed) {
            return self.layout_grid_contents(id, computed, box_id, heights, pre);
        }
        let specified_height = heights.specified;
        let dims = self.boxes[box_id.0 as usize].dimensions;

        // List marker / tag-driven extras before children.
        let bullet = tag == "li";
        let content_x = dims.content.x;
        let content_w = dims.content.width;
        let mut content_y = dims.content.y;
        let mut child_prev_mb = 0i32;
        let mut children = Vec::new();

        // Partition children into runs of blocks vs inlines.
        let mut inline_run: Vec<InlineItem> = Vec::new();
        let flush_inlines = |eng: &mut Engine<'a>,
                             run: &mut Vec<InlineItem>,
                             children: &mut Vec<BoxId>,
                             y: &mut i32,
                             prev_mb: &mut i32| {
            if run.is_empty() {
                return;
            }
            let items = std::mem::take(run);
            if let Some(anon) = eng.layout_anonymous_block(
                content_x,
                content_w,
                *y,
                prev_mb,
                &items,
                computed.text_align,
                pre,
            ) {
                let mb = eng.boxes[anon.0 as usize].dimensions.margin_box();
                *y = mb.bottom();
                *prev_mb = eng.boxes[anon.0 as usize].dimensions.margin.bottom;
                children.push(anon);
            }
        };

        // Leading bullet for list items: inject a marker as the first inline
        // of the first inline run, or as its own line if the first child is a
        // block. Hang-indent is padding-left from ua.css on ul/ol.
        let mut marker_pending = bullet;

        let child_ids: Vec<NodeId> = self.dom.children(id).collect();
        let mut absolute_children = Vec::new();
        for child in child_ids {
            if matches!(self.dom.node(child).data, NodeData::Element { .. })
                && is_out_of_flow(self.styles.get(child).position)
            {
                absolute_children.push(child);
                continue;
            }
            match self.child_mode(child, pre) {
                ChildMode::Skip => {}
                ChildMode::Block => {
                    flush_inlines(
                        self,
                        &mut inline_run,
                        &mut children,
                        &mut content_y,
                        &mut child_prev_mb,
                    );
                    // `<li><p>…</p></li>`: bullet before the first block child,
                    // not dropped when the item has no leading text.
                    if marker_pending {
                        if let Some(anon) = self.layout_anonymous_block(
                            content_x,
                            content_w,
                            content_y,
                            &mut child_prev_mb,
                            &[InlineItem::Marker {
                                text: LIST_MARKER.into(),
                                style: Style::default(),
                            }],
                            computed.text_align,
                            pre,
                        ) {
                            let mb = self.boxes[anon.0 as usize].dimensions.margin_box();
                            content_y = mb.bottom();
                            child_prev_mb = self.boxes[anon.0 as usize].dimensions.margin.bottom;
                            children.push(anon);
                        }
                        marker_pending = false;
                    }
                    if let Some(cid) = self.layout_node(
                        child,
                        content_x,
                        content_w,
                        specified_height,
                        content_y,
                        &mut child_prev_mb,
                        pre,
                    ) {
                        let mb = self.boxes[cid.0 as usize].dimensions.margin_box();
                        let dy = if self.styles.get(child).position == Position::Relative {
                            relative_delta(
                                self.styles.get(child).clone(),
                                content_w,
                                specified_height,
                            )
                            .1
                        } else {
                            0
                        };
                        content_y = mb.bottom().saturating_sub(dy);
                        child_prev_mb = self.boxes[cid.0 as usize].dimensions.margin.bottom;
                        children.push(cid);
                    }
                }
                ChildMode::Inline => {
                    if marker_pending {
                        inline_run.push(InlineItem::Marker {
                            text: LIST_MARKER.into(),
                            style: Style::default(),
                        });
                        marker_pending = false;
                    }
                    absolute_children.extend(self.inline_absolute_descendants(child));
                    self.push_inline(child, pre, content_w, &mut inline_run);
                }
            }
        }
        flush_inlines(
            self,
            &mut inline_run,
            &mut children,
            &mut content_y,
            &mut child_prev_mb,
        );

        // They remain children in the normal tree, but never update the flow
        // cursor or its collapsed-margin state.
        let has_absolute_children = !absolute_children.is_empty();
        for child in absolute_children {
            if let Some(abs) = self.layout_positioned(child, box_id, pre) {
                children.push(abs);
            }
        }

        // Empty <li> still gets a bullet line.
        if marker_pending
            && let Some(anon) = self.layout_anonymous_block(
                content_x,
                content_w,
                content_y,
                &mut child_prev_mb,
                &[InlineItem::Marker {
                    text: LIST_MARKER.into(),
                    style: Style::default(),
                }],
                computed.text_align,
                pre,
            )
        {
            let mb = self.boxes[anon.0 as usize].dimensions.margin_box();
            content_y = mb.bottom();
            children.push(anon);
        }

        if has_absolute_children {
            self.order_children_by_dom(id, &mut children);
        }
        self.boxes[box_id.0 as usize].children = children;
        (content_y - dims.content.y).max(0)
    }

    /// Explicit, row-major terminal grid.  It resolves each child exactly once
    /// at its final column span; the small occupancy map dies with layout.
    fn layout_grid_contents(
        &mut self,
        id: NodeId,
        computed: ComputedStyle,
        box_id: BoxId,
        heights: BlockHeight,
        pre: bool,
    ) -> i32 {
        let dims = self.boxes[box_id.0 as usize].dimensions;
        let mut columns = if computed.grid_template_columns.as_slice().is_empty() {
            vec![GridTrack::Auto]
        } else {
            computed.grid_template_columns.as_slice().to_vec()
        };
        columns.truncate(256);
        let col_gap = computed.gap.column.to_cells_h(dims.content.width).max(0);
        let row_gap = computed.gap.row.to_cells_v(dims.content.width).max(0);
        let child_ids: Vec<_> = self
            .dom
            .children(id)
            // A non-whitespace text run is an anonymous grid item.  It still
            // becomes an ordinary anonymous block below, so no later stage
            // needs a grid-only text representation.
            .filter(|&n| match &self.dom.node(n).data {
                NodeData::Element { .. } => true,
                NodeData::Text(text) => text.chars().any(|c| !is_html_space(c)),
                NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => false,
            })
            .collect();
        let mut occupied: Vec<Vec<bool>> = Vec::new();
        let mut placed = Vec::new();
        for child in child_ids.iter().copied() {
            let style = self.styles.get(child).clone();
            if is_out_of_flow(style.position) {
                continue;
            }
            let (mut col, mut cspan) = grid_axis(style.grid_column, columns.len());
            let (mut row, mut rspan) = grid_axis(style.grid_row, 256);
            if let Some(start) = col {
                cspan = cspan.min(columns.len().saturating_sub(start)).max(1);
            }
            if let Some(start) = row {
                rspan = rspan.min(256usize.saturating_sub(start)).max(1);
            }
            if col.is_none() || row.is_none() {
                let mut found = None;
                for r in 0..256usize {
                    for c in 0..columns.len() {
                        let rr = row.unwrap_or(r);
                        let cc = col.unwrap_or(c);
                        if cc + cspan <= columns.len() && grid_free(&occupied, rr, cc, rspan, cspan)
                        {
                            found = Some((rr, cc));
                            break;
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
                let (r, c) = found.unwrap_or((255, 0));
                row.get_or_insert(r);
                col.get_or_insert(c);
            }
            let (row, col) = (row.unwrap_or(0), col.unwrap_or(0));
            grid_reserve(&mut occupied, row, col, rspan, cspan);
            placed.push((child, row, col, rspan, cspan));
        }
        let column_axis = GridTrackAxis::Columns {
            width: dims.content.width,
        };
        let mut widths = resolve_grid_tracks(&columns, column_axis, col_gap);
        // Auto columns use the intrinsic minimum of one-cell items. Spanning
        // items keep their final span width but do not run a second sizing pass.
        for &(child, _, col, _, span) in &placed {
            if span == 1
                && matches!(
                    columns[col],
                    GridTrack::Auto | GridTrack::MinMax(GridMin::Auto, _)
                )
            {
                widths[col] = widths[col].max(self.sizer.min_content_width(child));
            }
        }
        widths = fit_grid_tracks(&columns, widths, column_axis, col_gap);
        let row_count = placed
            .iter()
            .map(|p| p.1 + p.3)
            .max()
            .unwrap_or(0)
            .max(computed.grid_template_rows.as_slice().len())
            .clamp(1, 256);
        let mut rows = computed.grid_template_rows.as_slice().to_vec();
        rows.resize(row_count, GridTrack::Auto);
        let row_axis = GridTrackAxis::Rows {
            width: dims.content.width,
            height: heights.specified,
        };
        let mut row_heights = resolve_grid_tracks(&rows, row_axis, row_gap);
        let mut children = Vec::new();
        for &(child, row, col, rspan, cspan) in &placed {
            let x = dims
                .content
                .x
                .saturating_add(grid_offset(&widths, col, col_gap));
            let w = grid_span(&widths, col, cspan, col_gap).max(1);
            let mut margin = 0;
            let direct_table = matches!(
                &self.dom.node(child).data,
                NodeData::Element { tag, .. } if tag == "table"
            );
            if direct_table {
                self.grid_item_depth = self.grid_item_depth.saturating_add(1);
            }
            let laid_out = if is_atomic_inline(self.styles.get(child).display) {
                self.layout_grid_item_root(child, x, w, pre)
            } else {
                self.layout_node(child, x, w, None, 0, &mut margin, pre)
            };
            if direct_table {
                self.grid_item_depth = self.grid_item_depth.saturating_sub(1);
            }
            if let Some(cid) = laid_out {
                let h = self.boxes[cid.0 as usize]
                    .dimensions
                    .margin_box()
                    .height
                    .max(0);
                if rspan == 1 && grid_row_accepts_content(rows[row], heights.specified.is_some()) {
                    row_heights[row] = row_heights[row].max(h);
                }
                children.push((child, cid, row, col, rspan, cspan));
            }
        }
        row_heights = fit_grid_tracks(&rows, row_heights, row_axis, row_gap);
        for &(_, cid, row, col, rspan, cspan) in &children {
            let target_x = dims
                .content
                .x
                .saturating_add(grid_offset(&widths, col, col_gap));
            let target_y = dims
                .content
                .y
                .saturating_add(grid_offset(&row_heights, row, row_gap));
            let current = self.boxes[cid.0 as usize].dimensions.margin_box();
            self.shift_subtree(
                cid,
                target_x.saturating_sub(current.x),
                target_y.saturating_sub(current.y),
            );
            let _ = (rspan, cspan);
        }
        // Keep DOM order for paint even when explicit cells are visually reordered.
        let mut ids: Vec<_> = children.into_iter().map(|(_, cid, ..)| cid).collect();
        for child in self.dom.children(id) {
            if is_out_of_flow(self.styles.get(child).position)
                && let Some(cid) = self.layout_positioned(child, box_id, pre)
            {
                ids.push(cid);
            }
        }
        self.order_children_by_dom(id, &mut ids);
        self.boxes[box_id.0 as usize].children = ids;
        self.boxes[box_id.0 as usize].grid = Some(GridLayout {
            columns: widths,
            rows: row_heights.clone(),
        });
        row_heights
            .iter()
            .fold(0i32, |n, h| n.saturating_add(*h))
            .saturating_add(row_gap.saturating_mul(row_heights.len().saturating_sub(1) as i32))
    }

    /// Grid items are block formatting roots for their *inside*. The outer
    /// inline half of an atomic inline only matters while it sits in an inline
    /// formatting context; here it owns a cell, so build its own root rather
    /// than flattening it into the anonymous text item path.
    fn layout_grid_item_root(
        &mut self,
        id: NodeId,
        x: i32,
        width: i32,
        pre: bool,
    ) -> Option<BoxId> {
        let NodeData::Element { tag, .. } = &self.dom.node(id).data else {
            return None;
        };
        if self.is_hidden(id) {
            return None;
        }
        let computed = self.styles.get(id).clone();
        let mut dims = resolve_block_dims(&computed, width, None);
        dims.content.x = x
            .saturating_add(dims.margin.left)
            .saturating_add(dims.border.left)
            .saturating_add(dims.padding.left);
        dims.content.y = dims.border.top.saturating_add(dims.padding.top);
        dims.content.height = 0;
        let box_id = self.layout_box_at(id, tag, computed.clone(), dims, None, pre || tag == "pre");
        if matches!(computed.position, Position::Relative | Position::Sticky) {
            self.apply_relative(box_id, computed, width, None);
        }
        Some(box_id)
    }

    /// Automatic table layout with a bounded, short-lived occupancy grid.
    /// Cells are laid out once after their final column widths are known; row
    /// spans only move and enlarge that already-built subtree.
    fn layout_table_contents(
        &mut self,
        id: NodeId,
        computed: ComputedStyle,
        box_id: BoxId,
        heights: BlockHeight,
        pre: bool,
    ) -> i32 {
        let (rows, loose_children) = self.table_rows(id);
        if rows.is_empty() {
            // Invalid or prose-only tables still read as they did before this
            // milestone.  In particular, do not manufacture a cell for them.
            return self.layout_contents(id, "table", computed, box_id, heights, pre);
        }

        let placements = self.table_placements(&rows);
        let columns = placements
            .iter()
            .map(|p| p.col + p.colspan)
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return self.layout_contents(id, "table", computed, box_id, heights, pre);
        }
        // `resolve_block_dims` has already resolved the table's own width and
        // min/max clamps. For an auto table this is the available content
        // width; for a definite or clamped table it is the requested width.
        let requested = self.boxes[box_id.0 as usize]
            .dimensions
            .content
            .width
            .max(1);
        let mut constraints = vec![TableColumn::EMPTY; columns];
        let contributions: Vec<_> = placements
            .iter()
            .map(|placement| self.table_cell_contribution(placement.cell, requested))
            .collect();
        // All floors settle before any ceilings: a later one-column minimum
        // must participate in a preceding colspan's max constraint.
        for (placement, contribution) in placements.iter().zip(&contributions) {
            if placement.colspan == 1 {
                let column = &mut constraints[placement.col];
                column.min = column.min.max(contribution.min);
            } else {
                raise_track_sum(
                    &mut constraints,
                    placement.col,
                    placement.colspan,
                    contribution.min,
                    true,
                );
            }
        }
        for (placement, contribution) in placements.iter().zip(&contributions) {
            if placement.colspan == 1 {
                let column = &mut constraints[placement.col];
                column.max = column.max.max(contribution.max).max(column.min);
            } else {
                raise_track_sum(
                    &mut constraints,
                    placement.col,
                    placement.colspan,
                    contribution.max,
                    false,
                );
            }
        }
        let min_table = constraints
            .iter()
            .fold(0i32, |sum, column| sum.saturating_add(column.min));
        let max_table = constraints
            .iter()
            .fold(0i32, |sum, column| sum.saturating_add(column.max));
        // Auto tables shrink-wrap between their intrinsic bounds *before*
        // their own min/max clamp applies. `resolve_block_dims` starts a block
        // at its containing width, so using that value directly here would
        // make `min-width: 20` turn a two-cell auto table into a 40-cell one.
        let target = if computed.width.is_auto() && self.grid_item_depth == 0 {
            let dims = self.boxes[box_id.0 as usize].dimensions;
            let axis = Axis {
                edges: dims
                    .padding
                    .left
                    .saturating_add(dims.padding.right)
                    .saturating_add(dims.border.left)
                    .saturating_add(dims.border.right),
                box_sizing: computed.box_sizing,
            };
            let mut target = requested.min(max_table).max(min_table);
            if !computed.max_width.is_auto() {
                target = target.min(axis.content_from(computed.max_width.to_cells_h(requested)));
            }
            if !computed.min_width.is_auto() {
                target = target.max(axis.content_from(computed.min_width.to_cells_h(requested)));
            }
            target
        } else {
            requested
        };
        let widths = fit_table_columns(&constraints, target);
        let table_width = widths
            .iter()
            .fold(0i32, |sum, width| sum.saturating_add(*width));
        // A table whose minima exceed the containing block deliberately
        // overflows. Existing horizontal clipping handles it; shrinking a
        // present column to zero would lose its cell instead.
        self.boxes[box_id.0 as usize].dimensions.content.width = table_width;
        let table = self.boxes[box_id.0 as usize].dimensions.content;
        let mut y = table.y;
        let mut row_boxes = Vec::with_capacity(rows.len() + loose_children.len());
        let mut previous_margin = 0;
        // Parser repair and anonymous table objects are deliberately deferred,
        // but a direct unexpected child must not disappear merely because its
        // siblings formed rows. Keep it on the existing block path before the
        // derived row subtree.
        for child in loose_children {
            if matches!(self.dom.node(child).data, NodeData::Element { .. })
                && is_out_of_flow(self.styles.get(child).position)
            {
                if let Some(abs) = self.layout_positioned(child, box_id, pre) {
                    row_boxes.push(abs);
                }
                continue;
            }
            if let Some(child_box) = self.layout_node(
                child,
                table.x,
                table_width,
                heights.specified,
                y,
                &mut previous_margin,
                pre,
            ) {
                let margin_box = self.boxes[child_box.0 as usize].dimensions.margin_box();
                y = margin_box.bottom();
                previous_margin = self.boxes[child_box.0 as usize].dimensions.margin.bottom;
                row_boxes.push(child_box);
            }
        }

        let mut row_ids = Vec::with_capacity(rows.len());
        for (row, _) in &rows {
            let row_style = self.styles.get(*row).clone();
            let row_id = self.alloc(LayoutBox {
                kind: BoxKind::TableRow,
                node: Some(*row),
                dimensions: Dimensions {
                    content: Rect {
                        x: table.x,
                        y,
                        width: table_width,
                        height: 0,
                    },
                    ..Dimensions::default()
                },
                children: Vec::new(),
                text: None,
                term_style: Style::default(),
                computed: row_style,
                image_src: None,
                image_size_firm: false,
                fixed_viewport: false,
                sticky: None,
                grid: None,
            });
            row_ids.push(row_id);
        }

        let mut cell_boxes = Vec::with_capacity(placements.len());
        let mut cell_heights = Vec::with_capacity(placements.len());
        for placement in &placements {
            let cell_style = self.styles.get(placement.cell).clone();
            let x = table.x.saturating_add(
                widths[..placement.col]
                    .iter()
                    .fold(0i32, |n, w| n.saturating_add(*w)),
            );
            let slot = widths[placement.col..placement.col + placement.colspan]
                .iter()
                .fold(0i32, |n, w| n.saturating_add(*w));
            let mut dims = resolve_block_dims(&cell_style, slot, None);
            // A table slot fixes the cell's outer horizontal budget. CSS
            // widths and auto margins cannot make it drift into a neighbour
            // before M11.15's real column algorithm has a say.
            dims.margin.left = 0;
            dims.margin.right = 0;
            dims.content.width = (slot
                - dims.padding.left
                - dims.padding.right
                - dims.border.left
                - dims.border.right)
                .max(0);
            dims.content.x = x + dims.border.left + dims.padding.left;
            dims.content.y = y + dims.border.top + dims.padding.top;
            dims.content.height = 0;
            let tag = self.tag(placement.cell).to_owned();
            let cell_id = self.layout_box_at(
                placement.cell,
                &tag,
                cell_style,
                dims,
                heights.specified,
                pre,
            );
            let cell_height = self.boxes[cell_id.0 as usize]
                .dimensions
                .margin_box()
                .height
                .max(1);
            self.boxes[cell_id.0 as usize].dimensions.content.height = self.boxes
                [cell_id.0 as usize]
                .dimensions
                .content
                .height
                .max(1);
            cell_boxes.push(cell_id);
            cell_heights.push(cell_height);
        }
        let mut row_heights = vec![1i32; rows.len()];
        for (placement, &height) in placements.iter().zip(&cell_heights) {
            if placement.rowspan == 1 {
                row_heights[placement.row] = row_heights[placement.row].max(height);
            } else {
                raise_row_sum(&mut row_heights, placement.row, placement.rowspan, height);
            }
        }
        let mut row_y = Vec::with_capacity(rows.len());
        for (index, row_id) in row_ids.iter().enumerate() {
            row_y.push(y);
            self.boxes[row_id.0 as usize].dimensions.content.y = y;
            self.boxes[row_id.0 as usize].dimensions.content.height = row_heights[index];
            row_boxes.push(*row_id);
            y = y.saturating_add(row_heights[index]);
        }
        for (index, placement) in placements.iter().enumerate() {
            let cell_id = cell_boxes[index];
            let final_y = row_y[placement.row];
            let final_outer_h = row_heights[placement.row..placement.row + placement.rowspan]
                .iter()
                .fold(0i32, |n, h| n.saturating_add(*h));
            let old = self.boxes[cell_id.0 as usize].dimensions.content;
            let dims = self.boxes[cell_id.0 as usize].dimensions;
            let content_h = final_outer_h
                .saturating_sub(dims.padding.top)
                .saturating_sub(dims.padding.bottom)
                .saturating_sub(dims.border.top)
                .saturating_sub(dims.border.bottom)
                .max(1);
            if placement.rowspan > 1 {
                self.boxes[cell_id.0 as usize].dimensions.content.height = content_h;
            }
            self.shift_subtree(
                cell_id,
                0,
                final_y
                    .saturating_add(dims.border.top)
                    .saturating_add(dims.padding.top)
                    .saturating_sub(old.y),
            );
            self.boxes[row_ids[placement.row].0 as usize]
                .children
                .push(cell_id);
        }
        // A positioned table cell remains owned by its row, but it is not a
        // grid track participant. Build it only after rows have their final
        // rectangles so its containing block is available without a second
        // table pass.
        for (row_index, (row, _)) in rows.iter().enumerate() {
            for cell in self.dom.children(*row) {
                if is_out_of_flow(self.styles.get(cell).position)
                    && matches!(&self.dom.node(cell).data, NodeData::Element { tag, .. } if matches!(tag.as_str(), "td" | "th"))
                    && let Some(abs) = self.layout_positioned(cell, row_ids[row_index], pre)
                {
                    self.boxes[row_ids[row_index].0 as usize].children.push(abs);
                }
            }
        }
        self.resolve_table_borders(
            TableGrid {
                placements: &placements,
                rows: &rows,
                widths: &widths,
                row_y: &row_y,
                row_heights: &row_heights,
            },
            box_id,
            table,
            computed,
        );
        self.boxes[box_id.0 as usize].children = row_boxes;
        (y - table.y).max(0)
    }

    /// One cell's outer horizontal intrinsic contribution. Its normal box is
    /// still built only once below, at the column width ultimately assigned.
    fn table_cell_contribution(&mut self, cell: NodeId, containing_width: i32) -> TableColumn {
        let computed = self.styles.get(cell).clone();
        let padding = computed
            .padding
            .left
            .to_cells_h(containing_width)
            .saturating_add(computed.padding.right.to_cells_h(containing_width));
        let border = computed
            .border
            .left
            .to_cells_h(containing_width)
            .saturating_add(computed.border.right.to_cells_h(containing_width));
        let axis = Axis {
            edges: padding.saturating_add(border),
            box_sizing: computed.box_sizing,
        };
        // The intrinsic sizer owns all subtree measurement (including fields,
        // images, Unicode and nested tables). Resolve percentages here, where
        // a table content width is finally available.
        let (mut min, mut max) = (
            self.sizer.min_content_width(cell),
            self.sizer.max_content_width(cell),
        );
        if !computed.width.is_auto() {
            let width = axis.content_from(computed.width.to_cells_h(containing_width));
            min = width;
            max = width;
        }
        let resolve =
            |length: Length| (!length.is_auto()).then(|| length.to_cells_h(containing_width));
        min = axis.clamp(
            min,
            resolve(computed.min_width),
            resolve(computed.max_width),
        );
        max = axis.clamp(
            max,
            resolve(computed.min_width),
            resolve(computed.max_width),
        );
        TableColumn {
            min: min.saturating_add(axis.edges).max(1),
            max: max.saturating_add(axis.edges).max(1),
            used: 1,
        }
        .normalized()
    }

    /// Turn parser supplied rows into finite rectangles. The grid is local to
    /// this call and its track cap is deliberately independent of attributes:
    /// hostile `rowspan=...` cannot turn into an allocation request.
    fn table_placements(&self, rows: &[(NodeId, Vec<NodeId>)]) -> Vec<TablePlacement> {
        const MAX_TRACKS: usize = 1024;
        let mut occupied = vec![vec![false; MAX_TRACKS]; rows.len()];
        let mut out = Vec::new();
        for (row, (_, cells)) in rows.iter().enumerate() {
            let mut cursor = 0usize;
            for &cell in cells {
                while cursor < MAX_TRACKS && occupied[row][cursor] {
                    cursor += 1;
                }
                if cursor == MAX_TRACKS {
                    break;
                }
                let colspan = table_span(self.dom.attr(cell, "colspan"), MAX_TRACKS - cursor);
                let rowspan = table_span(self.dom.attr(cell, "rowspan"), rows.len() - row);
                for occupied_row in occupied.iter_mut().skip(row).take(rowspan) {
                    for slot in occupied_row.iter_mut().skip(cursor).take(colspan) {
                        *slot = true;
                    }
                }
                out.push(TablePlacement {
                    cell,
                    row,
                    col: cursor,
                    rowspan,
                    colspan,
                });
                cursor += colspan;
            }
        }
        out
    }

    fn resolve_table_borders(
        &mut self,
        grid: TableGrid<'_>,
        table_owner: BoxId,
        table: Rect,
        table_style: ComputedStyle,
    ) {
        let row_count = grid.row_heights.len();
        let cols = grid.widths.len();
        let mut owner = vec![vec![None; cols]; row_count];
        for (index, p) in grid.placements.iter().enumerate() {
            for owner_row in owner.iter_mut().skip(p.row).take(p.rowspan) {
                for slot in owner_row.iter_mut().skip(p.col).take(p.colspan) {
                    *slot = Some(index);
                }
            }
        }
        let edge = |style: ComputedStyle, side: usize, basis: i32| -> i32 {
            let lengths = [
                style.border.top,
                style.border.right,
                style.border.bottom,
                style.border.left,
            ];
            if !lengths[side].is_auto() {
                lengths[side].to_cells_h(basis).max(0)
            } else {
                0
            }
        };
        let mut horizontal = vec![vec![0i32; cols]; row_count + 1];
        let mut vertical = vec![vec![0i32; cols + 1]; row_count];
        for r in 0..=row_count {
            for c in 0..cols {
                if r == 0 {
                    horizontal[r][c] =
                        horizontal[r][c].max(edge(table_style.clone(), 0, table.width));
                }
                if r == row_count {
                    horizontal[r][c] =
                        horizontal[r][c].max(edge(table_style.clone(), 2, table.width));
                }
                if r > 0 {
                    horizontal[r][c] = horizontal[r][c].max(edge(
                        self.styles.get(grid.rows[r - 1].0).clone(),
                        2,
                        table.width,
                    ));
                }
                if r < row_count {
                    horizontal[r][c] = horizontal[r][c].max(edge(
                        self.styles.get(grid.rows[r].0).clone(),
                        0,
                        table.width,
                    ));
                }
                let above = if r > 0 { owner[r - 1][c] } else { None };
                let below = if r < row_count { owner[r][c] } else { None };
                if above != below {
                    if let Some(i) = above {
                        horizontal[r][c] = horizontal[r][c].max(edge(
                            self.styles.get(grid.placements[i].cell).clone(),
                            2,
                            table.width,
                        ));
                    }
                    if let Some(i) = below {
                        horizontal[r][c] = horizontal[r][c].max(edge(
                            self.styles.get(grid.placements[i].cell).clone(),
                            0,
                            table.width,
                        ));
                    }
                }
            }
        }
        for r in 0..row_count {
            for c in 0..=cols {
                if c == 0 {
                    vertical[r][c] = vertical[r][c].max(edge(table_style.clone(), 3, table.width));
                }
                if c == cols {
                    vertical[r][c] = vertical[r][c].max(edge(table_style.clone(), 1, table.width));
                }
                let left = if c > 0 { owner[r][c - 1] } else { None };
                let right = if c < cols { owner[r][c] } else { None };
                if left != right {
                    if let Some(i) = left {
                        vertical[r][c] = vertical[r][c].max(edge(
                            self.styles.get(grid.placements[i].cell).clone(),
                            1,
                            table.width,
                        ));
                    }
                    if let Some(i) = right {
                        vertical[r][c] = vertical[r][c].max(edge(
                            self.styles.get(grid.placements[i].cell).clone(),
                            3,
                            table.width,
                        ));
                    }
                }
            }
        }
        let xs: Vec<i32> = grid
            .widths
            .iter()
            .scan(table.x, |x, width| {
                let at = *x;
                *x = x.saturating_add(*width);
                Some(at)
            })
            .collect();
        for (r, horizontal_row) in horizontal.iter().enumerate() {
            for (c, &thickness) in horizontal_row.iter().enumerate() {
                if thickness > 0 {
                    let y = if r == row_count {
                        grid.row_y[r - 1]
                            .saturating_add(grid.row_heights[r - 1])
                            .saturating_sub(1)
                    } else {
                        grid.row_y[r]
                    };
                    self.grid_borders.push(GridBorder {
                        owner: table_owner,
                        x: xs[c],
                        y,
                        length: grid.widths[c],
                        horizontal: true,
                        thickness,
                    });
                }
            }
        }
        for (r, vertical_row) in vertical.iter().enumerate() {
            for (c, &thickness) in vertical_row.iter().enumerate() {
                if thickness > 0 {
                    let x = if c == cols {
                        table.x.saturating_add(table.width).saturating_sub(1)
                    } else {
                        xs[c]
                    };
                    self.grid_borders.push(GridBorder {
                        owner: table_owner,
                        x,
                        y: grid.row_y[r],
                        length: grid.row_heights[r],
                        horizontal: false,
                        thickness,
                    });
                }
            }
        }
    }

    fn tag(&self, id: NodeId) -> &str {
        match &self.dom.node(id).data {
            NodeData::Element { tag, .. } => tag,
            _ => "",
        }
    }

    /// Find parser-supplied rows through harmless grouping elements, but never
    /// cross into a nested table or into a cell. Anonymous table repair is a
    /// later task; this traversal only derives roles already present in DOM.
    fn table_rows(&self, table: NodeId) -> (Vec<(NodeId, Vec<NodeId>)>, Vec<NodeId>) {
        fn visit(eng: &Engine<'_>, node: NodeId, out: &mut Vec<(NodeId, Vec<NodeId>)>) {
            for child in eng.dom.children(node) {
                let NodeData::Element { tag, .. } = &eng.dom.node(child).data else {
                    continue;
                };
                if eng.is_hidden(child)
                    || is_out_of_flow(eng.styles.get(child).position)
                    || tag == "table"
                    || matches!(tag.as_str(), "td" | "th")
                {
                    continue;
                }
                if tag == "tr" {
                    let cells = eng.dom.children(child).filter(|&cell| {
                        !eng.is_hidden(cell)
                            && eng.styles.get(cell).position != Position::Absolute
                            && matches!(&eng.dom.node(cell).data, NodeData::Element { tag, .. } if matches!(tag.as_str(), "td" | "th"))
                    }).collect();
                    out.push((child, cells));
                } else {
                    visit(eng, child, out);
                }
            }
        }
        let mut rows = Vec::new();
        visit(self, table, &mut rows);
        let loose = self
            .dom
            .children(table)
            .filter(|&child| match &self.dom.node(child).data {
                NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => false,
                NodeData::Text(text) => !text.chars().all(is_html_space),
                NodeData::Element { tag, .. } => {
                    !self.is_hidden(child)
                        && (is_out_of_flow(self.styles.get(child).position)
                            || !matches!(tag.as_str(), "tr" | "thead" | "tbody" | "tfoot"))
                }
            })
            .collect();
        (rows, loose)
    }

    /// A flex container's contents: css-flexbox-1 §4 (what the items are),
    /// §9.2 (how big each one wants to be), §9.3 (which of them share a line),
    /// §9.7 (how each line's space is divided) and §9.5 (where they go).
    /// Returns the container's used content height — for a row the cross size
    /// its lines used, for a column its inner main size.
    ///
    /// Scope, M9.10: all four `flex-direction`s, all three `flex-wrap`s, both
    /// axes.
    ///
    /// **The two directions run their passes in opposite orders, and that is
    /// forced rather than chosen.** A row's main size is a *width*, and
    /// `intrinsic` can measure a width without laying anything out — so a row
    /// sizes the whole line, places the whole line, and only then builds each
    /// item's box at its final position: on the main axis, alignment places and
    /// never moves. Its cross axis is a height, which nothing knows until the
    /// items exist, so that runs last in [`align_cross`](Self::align_cross) and
    /// does move boxes.
    ///
    /// A column inverts the main half. Its main size is a height and there is
    /// no height sizer, so an item has to be *built to be measured*: item
    /// generation builds each box, §9.7 and §9.5 run on the heights that came
    /// back, and [`place_column_item`](Self::place_column_item) moves each
    /// subtree to where the line put it.
    ///
    /// **What wrapping took back (M9.10).** A `nowrap` column's cross axis was
    /// the easy one: its single line is exactly as wide as the container's
    /// content box, so no item's cross placement depended on any other's and
    /// M9.9 settled both while building each item. A wrapping line is only as
    /// wide as the items *on it*, and which line an item lands on is not known
    /// until line collection has run — which for a column needs the items built
    /// first, because their main sizes are heights. So the shortcut is gone and
    /// both directions now run the same order: build, collect, flex and place
    /// the main axis line by line, then size and place the cross axis in
    /// [`align_cross`](Self::align_cross). Every item still gets built exactly
    /// once; what a column pays is that its boxes move on both axes instead of
    /// one.
    fn layout_flex_contents(
        &mut self,
        id: NodeId,
        computed: ComputedStyle,
        box_id: BoxId,
        heights: BlockHeight,
        pre: bool,
    ) -> i32 {
        let content = self.boxes[box_id.0 as usize].dimensions.content;
        let axis = FlexAxis::of(
            computed.flex_direction,
            computed.flex_wrap,
            content.width,
            heights,
        );

        let mut items = self.flex_items(id, &axis, &computed, content, heights.specified, pre);
        if items.is_empty() {
            return 0;
        }
        // Order-modified document order (§5.4). Stable, so items that share an
        // `order` keep the order the document gave them. Only the layout tree
        // is reordered: the DOM is untouched, so F1, `/` search and hit-testing
        // still see the document as written — which is what CSS says too. A
        // column's boxes are already built by now, but building one never
        // depended on its neighbours, so sorting them here is still enough.
        items.sort_by_key(|item| item.order);

        // §8.3: the gap between two items on the main axis — `column-gap` for a
        // row, `row-gap` for a column, because a gap is named for what it sits
        // between. A percentage resolves against the container's own inner size
        // on that axis.
        let gap = axis.main_distance(axis.main_gap(computed.gap));
        // Saturating: a page is free to write `gap: 99999em`, and a layout
        // stage that panicked on one would be a browser a page can crash.
        // The row comes out empty instead, which is what such a gap means.
        let total_gap = gap.saturating_mul(items.len() as i32 - 1);

        // §9.2 step 2: the container's inner main size, the number §9.3 wraps
        // against and §9.7 divides.
        //
        // A **row**'s is the content width it already has — resolved as any
        // block's is, clamps and `box-sizing` included (M9.2), because a flex
        // container is a perfectly ordinary block-level box from the outside.
        //
        // A **column**'s is a height, and there is no height sizer: a definite
        // `height` is the number, and `height: auto` makes the container
        // exactly as tall as its items. That second case is worth stating
        // plainly, because from the outside it looks like a bug — an
        // auto-height column has **no free space**, so `flex-grow` has nothing
        // to hand out and `justify-content` has nothing to move, and `flex: 1`
        // on its items does nothing at all. A browser behaves the same way.
        // `min-height` is what puts free space back, which is why the clamps
        // are applied here and not left to `layout_box_at`: the container's own
        // free space depends on them. That later clamp then re-applies to an
        // already-clamped value and is a no-op, which keeps the rule at one
        // site rather than splitting it across two.
        //
        // It is also what leaves a `max-height` in the number §9.3 wraps
        // against, which is §9.2 step 2's rule for a container whose main size
        // is indefinite — see [`FlexAxis::of`], where the decision not to wrap
        // an auto-height column is written down.
        let inner_main = if axis.vertical {
            let dims = self.boxes[box_id.0 as usize].dimensions;
            let v_axis = Axis {
                edges: dims.padding.top
                    + dims.padding.bottom
                    + dims.border.top
                    + dims.border.bottom,
                box_sizing: computed.box_sizing,
            };
            let content_main = items.iter().fold(total_gap, |acc, item| {
                acc.saturating_add(item.metrics.hypothetical)
                    .saturating_add(item.metrics.outer_edges)
            });
            v_axis.clamp(
                heights.specified.unwrap_or(content_main),
                heights.min,
                heights.max,
            )
        } else {
            content.width
        };

        // §9.3 step 5: which items share a line. A `nowrap` container gets the
        // one line it always got; a wrapping one is cut at the last item that
        // fits, by the *hypothetical* sizes measured above — before §9.7 has
        // grown or shrunk anything, because where the items wrap is what
        // decides how much room each has to grow into.
        let metrics: Vec<flex::Item> = items.iter().map(|i| i.metrics).collect();
        let lines = flex::collect_lines(&metrics, inner_main, gap, axis.wraps);

        // The container's near content edge on the main axis, in tree
        // coordinates — its left for a row, its top for a column, whichever end
        // of the axis main-start turns out to be.
        let main_origin = if axis.vertical { content.y } else { content.x };

        // §9.7 and §9.5, once per line. Every line divides the container's
        // whole inner main size — lines do not share it, they take turns at it
        // — so nothing here needs to know how many other lines there are.
        let mut children = Vec::with_capacity(items.len());
        for line in &lines {
            let line_items = &items[line.clone()];
            let sizes = flex::resolve(
                &metrics[line.clone()],
                inner_main,
                gap.saturating_mul(line.len() as i32 - 1),
            );

            // The axis flip, and the whole of what a `-reverse` direction
            // costs. Main-start is the container's far edge — its right for
            // `row-reverse`, its bottom for `column-reverse` — so an offset
            // from main-start is subtracted instead of added, and an item's
            // main-start margin is the one on the other side. Everything else —
            // §9.7 above, §9.5 below — is written in main-axis terms and does
            // not know which way the axis points, or even which axis it is.
            let slots: Vec<flex::Slot> = line_items
                .iter()
                .zip(&sizes)
                .map(|(item, &main_size)| {
                    let (start, end) = axis.main_margins(&item.computed);
                    flex::Slot {
                        outer: main_size + item.metrics.outer_edges,
                        auto_start: start.is_auto(),
                        auto_end: end.is_auto(),
                    }
                })
                .collect();

            // §9.5: hand out what §9.7 could not give away — auto margins
            // first, then `justify-content`.
            let placed = flex::place(&slots, gap, inner_main, computed.justify_content);

            // What this line really takes, which is more than the container's
            // inner main size exactly when it overflows — the number a reversed
            // direction has to count back from instead of the container's own
            // edge. Auto margins are not in it because a line with free space
            // for them to take is a line that fits.
            let line_used = slots
                .iter()
                .fold(gap.saturating_mul(line.len() as i32 - 1), |acc, slot| {
                    acc.saturating_add(slot.outer)
                });

            for (idx, (item, &main_size)) in line_items.iter().zip(&sizes).enumerate() {
                let p = placed[idx];
                let outer_main = slots[idx]
                    .outer
                    .saturating_add(p.auto_start)
                    .saturating_add(p.auto_end);
                // Main-axis offset → the physical near edge of the item's
                // margin box: added to the container's near edge, or counted
                // back from the far one when the axis is reversed
                // ([`from_far_edge`]). Saturating throughout, because an offset
                // can be enormous — `gap: 1e11em` is a legal thing for a
                // stylesheet to say, and an item shoved off the page is what it
                // asks for, where an overflowing add would be a panic a page
                // could trigger.
                let near = if axis.reverse {
                    from_far_edge(main_origin, inner_main, line_used, p.main_start, outer_main)
                } else {
                    main_origin.saturating_add(p.main_start)
                };
                // The auto-margin shares, named for the sides they are painted
                // on rather than for the ends of the main axis.
                let (auto_near, auto_far) = if axis.reverse {
                    (p.auto_end, p.auto_start)
                } else {
                    (p.auto_start, p.auto_end)
                };
                let place = ItemPlacement {
                    near,
                    main_size,
                    auto_near,
                    auto_far,
                };
                // The one fork the pass inversion costs. A column's box already
                // exists — building it is how its main size was measured — so
                // it moves to its place; a row's is built there in the first
                // place.
                let child = match item.built {
                    Some(b) => {
                        self.place_column_item(b, item, &axis, place);
                        b
                    }
                    None => self.layout_flex_item(
                        item,
                        place,
                        content,
                        computed.text_align,
                        heights.specified,
                        pre,
                    ),
                };
                children.push(child);
            }
        }

        // §9.4 and §9.6: size every line on the cross axis and stack them.
        // This can only be asked now, whichever direction the container runs:
        // for a row an item's cross size is *its content's* height, which
        // nothing knows until the content has been laid out, and for a column a
        // line's cross size is the widest item on it, which nothing knows until
        // line collection has said which items those are.
        let cross = self.align_cross(&items, &children, &lines, &axis, box_id, heights);

        // The container's used content height, whichever axis produced it: a
        // row's lines stack down the page, while a column's cross axis runs
        // across it and the height is the inner main size §9.7 just divided.
        //
        // Either way the container's own `height` is not consulted here:
        // `layout_box_at` applies it and the min/max clamps afterwards, exactly
        // as it does for a block.
        let height = if axis.vertical { inner_main } else { cross };
        for child in self.dom.children(id) {
            if matches!(self.dom.node(child).data, NodeData::Element { .. })
                && is_out_of_flow(self.styles.get(child).position)
                && let Some(abs) = self.layout_positioned(child, box_id, pre)
            {
                children.push(abs);
            }
        }
        self.boxes[box_id.0 as usize].children = children;
        height
    }

    /// Move one already-built column item to the main-axis position §9.5 gave
    /// it, and record the main size §9.7 resolved for it.
    ///
    /// The counterpart of [`layout_flex_item`](Self::layout_flex_item), and the
    /// half of the pass inversion that costs something: a column's items exist
    /// before the line's arithmetic can run, so they arrive here at a
    /// provisional position and move — with their whole subtrees, which is what
    /// [`shift_subtree`](Self::shift_subtree) is for.
    ///
    /// **The used main size is a field write, not a second layout pass.** That
    /// is not only cheaper than rebuilding the item at its flexed height —
    /// rebuilding to measure and then again to place is exponential in nesting
    /// depth — it is equivalent: content layout in this engine depends on the
    /// width a box is given and on nothing else, so an item rebuilt at a
    /// different *height* would produce identical children.
    fn place_column_item(
        &mut self,
        b: BoxId,
        item: &FlexItem,
        axis: &FlexAxis,
        place: ItemPlacement,
    ) {
        let c = &item.computed;
        // Read off the style rather than off the built box: a replaced box
        // keeps none of its own edges (`layout_img_block` ignores them), and
        // what the line reserved for this item is what the style says.
        let margin_top = if c.margin.top.is_auto() {
            place.auto_near
        } else {
            edge_v(c.margin.top, axis.width)
        };
        let content_y = place
            .near
            .saturating_add(margin_top)
            .saturating_add(edge_v(c.border.top, axis.width))
            .saturating_add(edge_v(c.padding.top, axis.width));
        let dy = content_y - self.boxes[b.0 as usize].dimensions.content.y;
        self.shift_subtree(b, 0, dy);
        let dims = &mut self.boxes[b.0 as usize].dimensions;
        // An `auto` main-axis margin's share is part of the item's margin box,
        // not just of the line's arithmetic — the same rule the row path
        // follows, so that the boxes in a column still tile it exactly.
        if c.margin.top.is_auto() {
            dims.margin.top = place.auto_near;
        }
        if c.margin.bottom.is_auto() {
            dims.margin.bottom = place.auto_far;
        }
        dims.content.height = place.main_size;
    }

    /// §9.4 (*cross sizing*), §9.6 step 15 (*`align-content`*) and §9.6
    /// (*cross-axis alignment*): size every line, stack the lines, and move
    /// each item into its place on its own line.
    ///
    /// Returns the cross size the lines used, which for a row is the
    /// container's used content height.
    ///
    /// **Both directions, since M9.10.** The cross axis is where the two
    /// directions stopped differing: whichever it is, a line's cross size
    /// depends on every item on it, so no item's cross placement can be settled
    /// while the items are still being built. An item is therefore built at the
    /// container's cross-start content edge and moved afterwards, *with
    /// everything inside it* — which is the whole content of the promise M9.6
    /// made about text never being left behind, and the reason
    /// [`shift_subtree`](Self::shift_subtree) exists.
    ///
    /// What still differs is what "cross size" means. A row's is a height, and
    /// growing one is a field write. A column's is a width, and growing one
    /// would leave the text inside wrapped at the old width, so a column item
    /// is *built* at the size it will keep and only ever widened into space its
    /// text was never going to reach — see
    /// [`stretch_item`](Self::stretch_item).
    fn align_cross(
        &mut self,
        items: &[FlexItem],
        boxes: &[BoxId],
        lines: &[Range<usize>],
        axis: &FlexAxis,
        box_id: BoxId,
        heights: BlockHeight,
    ) -> i32 {
        // Read off the container's own box rather than taking them as
        // arguments: its content rect (items sit inside its padding and border,
        // never against its border box), its edges, and its style.
        let dims = self.boxes[box_id.0 as usize].dimensions;
        let container = self.boxes[box_id.0 as usize].computed.clone();
        let content = dims.content;
        let cross_items: Vec<flex::CrossItem> = items
            .iter()
            .zip(boxes)
            .map(|(item, &b)| {
                let c = &item.computed;
                let align = cross_align(c, axis, &container);
                let dims = self.boxes[b.0 as usize].dimensions;
                let outer = if axis.vertical {
                    dims.margin_box().width
                } else {
                    dims.margin_box().height
                };
                let (start, end) = axis.cross_margins(c);
                flex::CrossItem {
                    outer,
                    // Measured only for the items that will be aligned by it:
                    // finding a baseline means walking the item's subtree for
                    // its first line box, and a row of `align-items: stretch`
                    // cards has no use for the answer.
                    //
                    // A baseline is a distance from the item's *cross-start*
                    // edge, so under `wrap-reverse` — where cross-start is the
                    // bottom — it is measured from the bottom. That keeps the
                    // baselines coincident after the flip, which is the one
                    // property the value exists for: reflecting each item
                    // individually would align them by their heights instead.
                    baseline: if align == AlignItems::Baseline {
                        let from_top = self.item_baseline(b);
                        if axis.cross_reverse {
                            outer - from_top
                        } else {
                            from_top
                        }
                    } else {
                        0
                    },
                    align,
                    auto_start: start.is_auto(),
                    auto_end: end.is_auto(),
                }
            })
            .collect();

        let gap = axis.cross_distance(axis.cross_gap(container.gap));

        // §9.4 step 8: every line is as big as the items on it.
        let mut line_cross: Vec<i32> = lines
            .iter()
            .map(|line| flex::cross_size(&cross_items[line.clone()]))
            .collect();

        // The container's own inner cross size, which is the number
        // `align-content` divides. A column's is its content width, definite
        // before anything was laid out. A row's is its `height` when it has
        // one, its lines' total when it does not — and **either way through its
        // own `min-height` and `max-height`**, which is what makes those
        // definite too.
        //
        // That last clause is the rule M9.9 already applies to a column's main
        // axis, for exactly the reason it applies here: a container's free
        // space depends on its clamps, so a stage that leaves them to the clamp
        // every block gets afterwards divides the wrong number. Left out, a
        // `min-height: 20em` row centred its items in the tallest of them and
        // then sat in twenty rows of blank space. `layout_box_at`'s later clamp
        // re-applies to an already-clamped value and does nothing, which is the
        // point: one rule, applied at one site.
        let cross_axis = Axis {
            edges: dims.padding.top + dims.padding.bottom + dims.border.top + dims.border.bottom,
            box_sizing: container.box_sizing,
        };
        let inner_cross = if axis.vertical {
            content.width
        } else {
            cross_axis.clamp(
                heights
                    .specified
                    .unwrap_or_else(|| flex::used_cross(&line_cross, gap)),
                heights.min,
                heights.max,
            )
        };

        // §9.4 step 7: a **single-line** container — `nowrap`, and the spec
        // means the property rather than "happened to fit on one line" — hands
        // its inner cross size to its one line. That is what makes
        // `align-items: center` inside a `height: 10em` container centre in ten
        // rows rather than in the tallest item; when nothing made the container
        // definite the two numbers are equal anyway and this changes nothing.
        // A wrapping container keeps its content-sized lines, and
        // `align-content` decides where the leftover goes instead.
        if !axis.wraps {
            line_cross[0] = inner_cross;
        }
        let placed_lines =
            flex::align_lines(&line_cross, gap, inner_cross, container.align_content);

        // The container's near content edge on the cross axis, and how much of
        // that axis the lines really took — more than the container's inner
        // cross size exactly when they overflow it, which is what
        // `wrap-reverse` has to count back from ([`from_far_edge`]).
        let cross_origin = if axis.vertical { content.x } else { content.y };
        let occupied = placed_lines
            .last()
            .map_or(0, |line| line.cross_start.saturating_add(line.cross));

        for (line, placed_line) in lines.iter().zip(&placed_lines) {
            let placed = flex::cross_place(&cross_items[line.clone()], placed_line.cross);
            // This line's *near* edge in tree coordinates — its top for a row,
            // its left for a column, whichever end of the cross axis
            // cross-start turns out to be.
            let line_near = if axis.cross_reverse {
                from_far_edge(
                    cross_origin,
                    inner_cross,
                    occupied,
                    placed_line.cross_start,
                    placed_line.cross,
                )
            } else {
                cross_origin.saturating_add(placed_line.cross_start)
            };
            for (idx, item) in line.clone().enumerate() {
                let (b, p, ci) = (boxes[item], placed[idx], cross_items[item]);
                let c = &items[item].computed;
                if ci.align == AlignItems::Stretch {
                    // A percentage `min-height` on an item still resolves
                    // against the container's *specified* height and nothing
                    // else: a height that only became a number because of the
                    // container's own `min-height` is not one a percentage
                    // inside it may resolve against (CSS 2.1 §10.5).
                    self.stretch_item(b, c, axis, placed_line.cross, axis.cross_base);
                }
                // Read the box back: `stretch_item` may just have changed the
                // very size the offsets below are measured against, and a
                // stretched item that kept its old outer size would sit a
                // reversed line's worth of cells off the bottom.
                let dims = self.boxes[b.0 as usize].dimensions;
                let outer = if axis.vertical {
                    dims.margin_box().width
                } else {
                    dims.margin_box().height
                };
                let span = outer
                    .saturating_add(p.auto_start)
                    .saturating_add(p.auto_end);
                // Cross-axis offset → the physical near edge of the item's
                // margin box, and the auto-margin shares named for the sides
                // they are painted on. The same arithmetic the main axis does,
                // one level down: the line is this item's container, and an
                // item taller than its line hangs off the line's far end rather
                // than off the top of the page.
                let near = if axis.cross_reverse {
                    from_far_edge(line_near, placed_line.cross, span, p.cross_start, span)
                } else {
                    line_near.saturating_add(p.cross_start)
                };
                let (auto_near, auto_far) = if axis.cross_reverse {
                    (p.auto_end, p.auto_start)
                } else {
                    (p.auto_start, p.auto_end)
                };
                // The item's own near-side edges, the ones between its margin
                // box and its content box on this axis.
                let (near_margin, far_margin, near_edges, used_margin) = if axis.vertical {
                    (
                        c.margin.left,
                        c.margin.right,
                        dims.border.left + dims.padding.left,
                        dims.margin.left,
                    )
                } else {
                    (
                        c.margin.top,
                        c.margin.bottom,
                        dims.border.top + dims.padding.top,
                        dims.margin.top,
                    )
                };
                let used_margin = if near_margin.is_auto() {
                    auto_near
                } else {
                    used_margin
                };
                let start = near + used_margin + near_edges;
                if axis.vertical {
                    self.shift_subtree(b, start - dims.content.x, 0);
                } else {
                    self.shift_subtree(b, 0, start - dims.content.y);
                }
                // An `auto` cross margin's share is part of the item's margin
                // box, not just of the line's arithmetic — the same rule §9.5's
                // auto margins follow on the main axis, so that the boxes on a
                // line still tile it exactly.
                let dims = &mut self.boxes[b.0 as usize].dimensions;
                let (near_slot, far_slot) = if axis.vertical {
                    (&mut dims.margin.left, &mut dims.margin.right)
                } else {
                    (&mut dims.margin.top, &mut dims.margin.bottom)
                };
                if near_margin.is_auto() {
                    *near_slot = auto_near;
                }
                if far_margin.is_auto() {
                    *far_slot = auto_far;
                }
            }
        }
        inner_cross
    }

    /// §9.4 step 11: an item with `align-self: stretch` fills its line's cross
    /// size — *if* its own cross size is `auto` and neither cross margin is,
    /// since an item that stated a size or claimed the free space with an auto
    /// margin has already answered the question.
    ///
    /// **A field write on both axes, but they are not the same bargain.** A row
    /// item's cross size is a height, and growing a box's height changes
    /// nothing inside it: its contents keep the positions they were given, and
    /// what changes is how far its background and borders reach, which is what
    /// pages use `stretch` for (equal-height cards). A column item's cross size
    /// is a width, and widening a box does *not* re-wrap the text inside it —
    /// so a column item is built at the width it will keep, and this only ever
    /// widens it into space that text was never going to reach.
    ///
    /// That is safe rather than lucky, and the reason is worth stating: a
    /// column item that stretches is one whose `width` is `auto`, which
    /// [`column_cross_size`](Self::column_cross_size) built at its fit-content
    /// width — its text's own width, unwrapped — whenever that fits the
    /// container. An item whose text *did* have to wrap was built at the
    /// container's full content width, which no line can exceed, so it is never
    /// the one being widened here.
    ///
    /// Content that genuinely wants the new size — a nested `height: 100%` —
    /// still does not get it, on either axis. M9.9's note on that: an item's
    /// used main size in a column is only known *after* the item was built to
    /// measure it, so a percentage height inside it resolved against nothing
    /// and stays indefinite. Making it definite means a second layout pass over
    /// the item, which is the one thing this path is written to avoid.
    fn stretch_item(
        &mut self,
        b: BoxId,
        c: &ComputedStyle,
        axis: &FlexAxis,
        line_cross: i32,
        definite_cross: Option<i32>,
    ) {
        let (size, min, max) = if axis.vertical {
            (c.width, c.min_width, c.max_width)
        } else {
            (c.height, c.min_height, c.max_height)
        };
        let (near_margin, far_margin) = axis.cross_margins(c);
        // A replaced box's cross size came from the image, not from an `auto`
        // size, so it is not the `auto` the spec's condition is about.
        // Stretching one would rescale the picture to fill the line.
        if self.boxes[b.0 as usize].kind == BoxKind::Image
            || !size.is_auto()
            || near_margin.is_auto()
            || far_margin.is_auto()
        {
            return;
        }
        let dims = self.boxes[b.0 as usize].dimensions;
        let (edges, margins, content_size) = if axis.vertical {
            (
                dims.padding.left + dims.padding.right + dims.border.left + dims.border.right,
                dims.margin.left + dims.margin.right,
                dims.content.width,
            )
        } else {
            (
                dims.padding.top + dims.padding.bottom + dims.border.top + dims.border.bottom,
                dims.margin.top + dims.margin.bottom,
                dims.content.height,
            )
        };
        let box_axis = Axis {
            edges,
            box_sizing: c.box_sizing,
        };
        let target = line_cross - (edges + margins);
        // §4.5 on the cross axis: an `auto` minimum on a flex item is its
        // *content* size, so an item stretched into a line shorter than its own
        // text is never squeezed until it clips. That content size is exactly
        // the size the box has right now — the cross size is `auto` here, so it
        // is what the contents used.
        let resolve = |len: Length| {
            if axis.vertical {
                (!len.is_auto()).then(|| len.to_cells_h(axis.width))
            } else {
                definite_v(len, definite_cross)
            }
        };
        let min = resolve(min).map_or(content_size, |v| box_axis.content_from(v));
        let max = resolve(max).map(|v| box_axis.content_from(v));
        // M9.2's clamp order, max before min, so a minimum bigger than the
        // maximum wins — the automatic one included.
        let used = max.map_or(target, |m| target.min(m)).max(min).max(0);
        let dims = &mut self.boxes[b.0 as usize].dimensions;
        if axis.vertical {
            dims.content.width = used;
        } else {
            dims.content.height = used;
        }
    }

    /// The row this item's baseline sits on, as a distance from its cross-start
    /// margin edge.
    ///
    /// A cell grid makes this the easy part of flex rather than the hard one:
    /// every line box is exactly one row tall, so an item's baseline *is* the
    /// row of its first line box — margin + border + padding, plus wherever
    /// that line ended up inside the content box.
    ///
    /// An item with no line box at all (an empty div, an image) has nothing to
    /// align to, so it synthesises one from its cross-end **border** edge, as
    /// css-flexbox-1 §8.3 says: the box hangs off the baseline the way a
    /// letter sits on a rule.
    fn item_baseline(&self, b: BoxId) -> i32 {
        let dims = self.boxes[b.0 as usize].dimensions;
        let margin_edge = dims.margin_box().y;
        match self.first_line_row(b) {
            Some(row) => row - margin_edge,
            None => dims.border_box().bottom() - margin_edge,
        }
    }

    /// The baseline row of the first line box in this subtree, in paint order —
    /// which for a box that contains text is the row its first line of text is
    /// on.
    fn first_line_row(&self, b: BoxId) -> Option<i32> {
        let bx = &self.boxes[b.0 as usize];
        if bx.kind == BoxKind::Line {
            return Some(self.line_baseline_row(b));
        }
        if let Some(row) = field_baseline_rows(bx) {
            return Some(row.0);
        }
        bx.children.iter().find_map(|&c| self.first_line_row(c))
    }

    /// The same question asked of the **last** line box: what an atomic
    /// inline's own baseline is (CSS 2.1 §10.8.1).
    fn last_line_row(&self, b: BoxId) -> Option<i32> {
        let bx = &self.boxes[b.0 as usize];
        if bx.kind == BoxKind::Line {
            return Some(self.line_baseline_row(b));
        }
        if let Some(row) = field_baseline_rows(bx) {
            return Some(row.1);
        }
        bx.children
            .iter()
            .rev()
            .find_map(|&c| self.last_line_row(c))
    }

    /// Which of a line box's rows carries its baseline.
    ///
    /// A line box was one row tall until M9.11, so its top row *was* its
    /// baseline; a line with an atomic inline on it can be several, and the
    /// baseline is the row its text sits on. Reading it back off the text
    /// rather than storing it keeps the answer in one place — the line box
    /// puts every piece on the page, and a second copy of "which row" could
    /// disagree with where they went. A line with no text on it takes the
    /// baseline of whatever box is on it instead, and an empty one is its own
    /// first row.
    fn line_baseline_row(&self, line: BoxId) -> i32 {
        let bx = &self.boxes[line.0 as usize];
        for &c in &bx.children {
            let child = &self.boxes[c.0 as usize];
            if child.kind == BoxKind::Text {
                return child.dimensions.content.y;
            }
        }
        bx.children
            .iter()
            .find_map(|&c| self.last_line_row(c))
            .unwrap_or(bx.dimensions.content.y)
    }

    /// Move a box and everything under it `dx` cells across the page and `dy`
    /// rows down it.
    ///
    /// Every rectangle in the tree is absolute, so a subtree moves by adding
    /// the same offset to every box in it — no relative coordinates to keep in
    /// step, and nothing outside the subtree to update. Edges are unaffected:
    /// a margin is a width, not a position.
    ///
    /// Both axes since M9.10: a wrapping column's items are placed on the cross
    /// axis after they are built, so they move sideways for the same reason a
    /// row's move down.
    fn shift_subtree(&mut self, b: BoxId, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        let content = &mut self.boxes[b.0 as usize].dimensions.content;
        content.x += dx;
        content.y += dy;
        // By index, not by iterator: the loop needs `&mut self` for the
        // recursion, and the child list cannot be borrowed across it.
        for i in 0..self.boxes[b.0 as usize].children.len() {
            let child = self.boxes[b.0 as usize].children[i];
            self.shift_subtree(child, dx, dy);
        }
    }

    /// Build one **row** item's box where §9.5 placed it, at the main size §9.7
    /// resolved for it.
    ///
    /// Row-only, and that is the whole shape of the row path: its main size is
    /// a width, which `intrinsic` measured before anything was built, so the
    /// box can be built at its final main-axis position and never has to move.
    /// A column has no such option and uses
    /// [`place_column_item`](Self::place_column_item) instead.
    fn layout_flex_item(
        &mut self,
        item: &FlexItem,
        place: ItemPlacement,
        container: Rect,
        align: TextAlign,
        containing_height: Option<i32>,
        pre: bool,
    ) -> BoxId {
        let ItemPlacement {
            near: left,
            main_size,
            auto_near: auto_left,
            auto_far: auto_right,
        } = place;
        match item.source {
            FlexItemSource::Element(node) => {
                let tag = match &self.dom.node(node).data {
                    NodeData::Element { tag, .. } => tag.clone(),
                    _ => String::new(),
                };
                let mut dims = resolve_block_dims(&item.computed, container.width, None);
                // An `auto` margin on a flex item absorbs the line's free space
                // rather than centring the box in its containing block (§9.5
                // step 1) — a different rule with a different answer, so the
                // block one is overwritten with the share `flex::place`
                // computed. Zero when there was no free space to take.
                if item.computed.margin.left.is_auto() {
                    dims.margin.left = auto_left;
                }
                if item.computed.margin.right.is_auto() {
                    dims.margin.right = auto_right;
                }
                // What this item costs the line, whichever path builds its box.
                // `outer_edges` is the same six lengths `dims` resolved, summed
                // against the same containing width, so an item occupies
                // exactly the outer size §9.7 divided the line into and §9.5
                // placed — the invariant the whole line's arithmetic rests on.
                // The auto-margin share is deliberately *not* in it: that is
                // the line's space, sitting beside the box rather than in it.
                let lead = dims.margin.left + dims.border.left + dims.padding.left;
                let own_outer = main_size + item.metrics.outer_edges;

                // Replaced and line-generating children keep their own layout
                // paths rather than being re-derived here: an `<img>` item is
                // its image, not a block container that happens to be
                // `main_size` wide. `<br>` and `<hr>` size themselves from the
                // width they are handed and take their own edges back out of
                // it, so their own outer size is what they should be given —
                // starting past the auto margin, never across it. Hand one the
                // auto share as well and it fills the very cells §9.5 reserved
                // to push it along.
                if matches!(tag.as_str(), "img" | "br" | "hr") {
                    let mut prev_mb = 0;
                    if let Some(child) = self.layout_node(
                        node,
                        left + auto_left,
                        own_outer,
                        containing_height,
                        container.y,
                        &mut prev_mb,
                        pre,
                    ) {
                        // The cells §9.5 granted this item's auto margins are
                        // part of its margin box, not just of the line's
                        // arithmetic: an item whose box did not record them
                        // would sit correctly and still leave the row's boxes
                        // failing to tile it, which is the invariant M9.8's
                        // cross-axis work will be read against. They are the
                        // one thing the inner layout could not know — it was
                        // handed this item's own outer size and nothing else.
                        let built = &mut self.boxes[child.0 as usize].dimensions;
                        built.margin.left += auto_left;
                        built.margin.right += auto_right;
                        if tag == "img" {
                            // `layout_img_block` floors an image at its
                            // intrinsic width and ignores its margins: right
                            // for a block, wrong for a flex item, whose used
                            // main size *is* the size §9.7 resolved for it,
                            // stretched or squeezed. Left alone, the item
                            // advanced the cursor by its intrinsic width and
                            // the line silently kept free space the algorithm
                            // had already given away.
                            built.content.x = left + lead;
                            built.content.width = main_size;
                        }
                        return child;
                    }
                    // One that generates no box at all (an `<img>` the image
                    // context never heard of) falls through and gets an empty
                    // one: every item owes the tree a box, or F3 and
                    // hit-testing would stop matching the DOM.
                }
                // The flexed size *is* the used main size: it already went
                // through this item's own min/max clamps inside §9.7, so the
                // width `resolve_block_dims` computed from `width`/`auto` is
                // replaced rather than clamped again.
                dims.content.width = main_size;
                dims.content.x = left + lead;
                dims.content.y = container.y + dims.margin.top + dims.border.top + dims.padding.top;
                dims.content.height = 0;
                self.layout_box_at(
                    node,
                    &tag,
                    item.computed.clone(),
                    dims,
                    containing_height,
                    pre,
                )
            }
            FlexItemSource::Text(ref nodes) => {
                // An anonymous flex item: one inline formatting context over a
                // contiguous run of text, with no styles of its own (§4).
                let mut run = Vec::new();
                for &node in nodes {
                    self.push_inline(node, pre, main_size, &mut run);
                }
                let mut prev_mb = 0;
                self.layout_anonymous_block(
                    left,
                    main_size,
                    container.y,
                    &mut prev_mb,
                    &run,
                    // An anonymous box has no style of its own, so the
                    // alignment it uses is the container's.
                    align,
                    pre,
                )
                .expect("an anonymous flex item always has inline content")
            }
        }
    }

    /// §4: turn the container's children into flex items, and measure each one
    /// enough to hand §9.7 its inputs.
    ///
    /// For a **column** this also builds every item's box, because there is no
    /// other way to measure a height — see
    /// [`column_item`](Self::column_item). Building one never depends on its
    /// neighbours, so document order is still a valid order to do it in.
    fn flex_items(
        &mut self,
        container: NodeId,
        axis: &FlexAxis,
        style: &ComputedStyle,
        content: Rect,
        definite_height: Option<i32>,
        pre: bool,
    ) -> Vec<FlexItem> {
        flex_sources(self.dom, container, &|n| self.is_hidden(n), pre)
            .into_iter()
            .filter(|source| match source {
                FlexItemSource::Element(node) => {
                    self.styles.get(*node).position != Position::Absolute
                }
                FlexItemSource::Text(_) => true,
            })
            .map(|source| {
                if axis.vertical {
                    self.column_item(source, axis, style, content, definite_height, pre)
                } else {
                    self.row_item(source, axis)
                }
            })
            .collect()
    }

    /// One item of a **row**: measured, not built. Its main size is a width, so
    /// `intrinsic` can answer without laying anything out, and the box is built
    /// later at the position §9.5 chose for it.
    fn row_item(&mut self, source: FlexItemSource, axis: &FlexAxis) -> FlexItem {
        match source {
            FlexItemSource::Element(node) => {
                let c = self.styles.get(node).clone();
                // Measuring an inline subtree is the expensive thing flex added
                // to the layout path, so it is asked at most once per item and
                // only when §9.2's base size or §4.5's automatic minimum
                // actually wants the answer.
                let content = if wants_content_size(&c, axis) {
                    self.sizer.content_widths(node)
                } else {
                    (0, 0)
                };
                FlexItem {
                    source: FlexItemSource::Element(node),
                    order: c.order,
                    metrics: item_metrics(&c, axis, content),
                    computed: c.clone(),
                    built: None,
                }
            }
            FlexItemSource::Text(nodes) => {
                // An anonymous item has no style, so every flex property is at
                // its initial value: `flex: 0 1 auto`, and a basis of `auto` on
                // a box with no `width` is its max-content size (§9.2 step 3
                // B/E), which is what the initial style makes `item_metrics`
                // compute.
                let content = self.sizer.run_widths(&nodes);
                let c = ComputedStyle::default();
                FlexItem {
                    source: FlexItemSource::Text(nodes),
                    computed: c.clone(),
                    order: 0,
                    metrics: item_metrics(&c, axis, content),
                    built: None,
                }
            }
        }
    }

    /// One item of a **column**: resolve its cross size, build it at the
    /// container's cross-start edge, and read back the main size it used.
    ///
    /// This is the pass inversion `flex-direction: column` forces. A column's
    /// main size is a height, `intrinsic` measures widths only, and there is no
    /// height sizer — so a `height: auto` item's flex base size can only be
    /// learned by building the item and asking how tall it came out. The item
    /// is built exactly once: §9.7's answer is applied afterwards as a field
    /// write, never as a rebuild.
    ///
    /// Its cross **size** is settled here, because it is a width and a box has
    /// to be built at the width it will keep. Its cross **position** is not:
    /// under `flex-wrap: wrap` a line is only as wide as the items on it, and
    /// which items those are is not known until every item has been built. So
    /// the box is built against the container's cross-start content edge and
    /// [`align_cross`](Self::align_cross) moves it — which is what M9.10 took
    /// back from M9.9, where a column's single line was always the full content
    /// box and no item ever moved sideways.
    fn column_item(
        &mut self,
        source: FlexItemSource,
        axis: &FlexAxis,
        style: &ComputedStyle,
        content: Rect,
        definite_height: Option<i32>,
        pre: bool,
    ) -> FlexItem {
        match source {
            FlexItemSource::Element(node) => {
                let c = self.styles.get(node).clone();
                let tag = match &self.dom.node(node).data {
                    NodeData::Element { tag, .. } => tag.clone(),
                    _ => String::new(),
                };
                let align = cross_align(&c, axis, style);

                let mut dims = resolve_block_dims(&c, content.width, None);
                // `resolve_block_dims` centres an `auto` cross margin in the
                // containing block; a flex item's takes the *line's* free space
                // instead (§9.6 step 1), a different rule with a different
                // answer. Zero them first, so the item's outer cross size is
                // measured the way §9.6 measures it — and leave them zero:
                // `align_cross` is what hands out the shares, once it knows
                // which line the item is on.
                if c.margin.left.is_auto() {
                    dims.margin.left = 0;
                }
                if c.margin.right.is_auto() {
                    dims.margin.right = 0;
                }
                let h_edges =
                    dims.padding.left + dims.padding.right + dims.border.left + dims.border.right;
                // The margins that are the item's own, with `auto` counted as
                // nothing — what this item costs the line's cross axis before
                // §9.6 hands out anything.
                let fixed_margins = dims.margin.left + dims.margin.right;
                let avail = (content.width - fixed_margins - h_edges).max(0);
                let cross = match tag.as_str() {
                    // A replaced item's cross size is its image's, never a
                    // stretched one — the same exception `stretch_item` makes
                    // on a row, where stretching an image would rescale the
                    // picture to fill the line.
                    "img" => self.sizer.content_widths(node).1,
                    // A `<br>` is a break and an `<hr>` a rule across the box
                    // they were handed: no content to shrink-wrap and nothing
                    // to stretch, so they take the whole content box.
                    //
                    // **A known divergence in a wrapping column, and the reason
                    // it is not fixed here.** §9.4 step 8 would size the line
                    // from this item's *hypothetical* cross size, which for a
                    // rule with no content is 0 — so a browser lets the other
                    // items decide the column's width and stretches the rule
                    // into it. This engine cannot: `layout_hr` generates the
                    // rule's glyphs from the width it is built at, and a column
                    // item is built before its line exists. Built at 0 and
                    // widened afterwards, an `<hr>` is a correctly sized box
                    // containing no rule at all, which is worse than a column
                    // that came out too wide. The cost is `<hr>` in a wrapping
                    // column widening its line to the container, pinned by
                    // `a_rule_widens_a_wrapping_columns_line`; the fix is a
                    // rule that can be re-sized after it is built, which is a
                    // change to the replaced-box path rather than to flex.
                    "br" | "hr" => avail,
                    _ => self.column_cross_size(node, &c, align, axis, avail, h_edges),
                };
                dims.content.width = cross;

                // Both axes are provisional: §9.5 cannot run until every item's
                // height is known and §9.6 cannot run until line collection has
                // said which items share a line, so the box is built against
                // the container's two content-start edges and moved from there
                // — `place_column_item` on the main axis, `align_cross` on the
                // cross one.
                dims.content.x =
                    content.x + dims.margin.left + dims.border.left + dims.padding.left;
                dims.content.y = content.y + dims.margin.top + dims.border.top + dims.padding.top;
                dims.content.height = 0;

                let built = if matches!(tag.as_str(), "img" | "br" | "hr") {
                    // These keep their own layout paths rather than being
                    // re-derived as block containers, exactly as they do on a
                    // row: they size themselves from the width they are handed
                    // and take their own edges back out of it, so what they are
                    // given is their own outer cross size.
                    let mut prev_mb = 0;
                    let own_outer = cross + h_edges + fixed_margins;
                    let content_x = dims.content.x;
                    self.layout_node(
                        node,
                        content.x,
                        own_outer,
                        definite_height,
                        content.y,
                        &mut prev_mb,
                        pre,
                    )
                    .map(|b| {
                        let d = &mut self.boxes[b.0 as usize].dimensions;
                        if tag == "img" {
                            // `layout_img_block` ignores an image's own margins
                            // and floors it at its intrinsic width: right for a
                            // block, wrong for a flex item, whose used cross
                            // size is the one resolved above.
                            d.content.x = content_x;
                            d.content.width = cross;
                        }
                        let h = d.content.height;
                        (b, h)
                    })
                } else {
                    None
                };
                // One that generates no box at all (an `<img>` the image
                // context never heard of) falls through to the block path and
                // gets an empty one: every item owes the tree a box, or F3 and
                // hit-testing would stop matching the DOM.
                let (b, content_main) = built.unwrap_or_else(|| {
                    self.layout_box_at_measured(node, &tag, c.clone(), dims, definite_height, pre)
                });

                FlexItem {
                    source: FlexItemSource::Element(node),
                    order: c.order,
                    // The content height stands in for both content sizes: a
                    // block at a fixed width has one, and min-content and
                    // max-content on this axis are the same number.
                    metrics: item_metrics(&c, axis, (content_main, content_main)),
                    computed: c.clone(),
                    built: Some(b),
                }
            }
            FlexItemSource::Text(nodes) => {
                // An anonymous flex item: one inline formatting context over a
                // contiguous run of text, with no style of its own (§4), so its
                // alignment and its text alignment are the container's.
                let align = column_cross_align(style.align_items);
                let (min_content, max_content) = self.sizer.run_widths(&nodes);
                let cross = if align == AlignItems::Stretch && !axis.wraps {
                    content.width
                } else {
                    shrink_to_fit(min_content, max_content, content.width)
                };
                let mut run = Vec::new();
                for &node in &nodes {
                    self.push_inline(node, pre, cross, &mut run);
                }
                let mut prev_mb = 0;
                let b = self
                    .layout_anonymous_block(
                        content.x,
                        cross,
                        content.y,
                        &mut prev_mb,
                        &run,
                        style.text_align,
                        pre,
                    )
                    .expect("an anonymous flex item always has inline content");
                let content_main = self.boxes[b.0 as usize].dimensions.content.height;
                let c = ComputedStyle::default();
                FlexItem {
                    source: FlexItemSource::Text(nodes),
                    computed: c.clone(),
                    order: 0,
                    metrics: item_metrics(&c, axis, (content_main, content_main)),
                    built: Some(b),
                }
            }
        }
    }

    /// A column item's cross size: its used **width**, in content-box cells.
    ///
    /// A stated `width` wins. Otherwise `stretch` — the initial `align-items`,
    /// and the reason `flex-direction: column` looks like ordinary block flow —
    /// fills the line, and every other alignment shrink-wraps the item around
    /// its content. Either way the item's own `min-width`/`max-width` clamp the
    /// result, in M9.2's order.
    ///
    /// **This has to happen before the box is built**: a box laid out at one
    /// width and then given another has its text wrapped at the wrong one, and
    /// re-wrapping means building the item twice.
    ///
    /// Which is why a **wrapping** column stretches nothing here. Its line is
    /// only as wide as the items on it, and that is not known until every item
    /// has been built, so a stretching item is built at the same fit-content
    /// width `flex-start` would have given it — §9.4 step 8's *hypothetical
    /// cross size*, which is exactly what the line is then sized from — and
    /// [`stretch_item`](Self::stretch_item) widens the box into its line
    /// afterwards. That widening cannot change any wrapping: an item built
    /// narrower than its line was built at its own text's width.
    fn column_cross_size(
        &mut self,
        node: NodeId,
        c: &ComputedStyle,
        align: AlignItems,
        axis: &FlexAxis,
        avail: i32,
        h_edges: i32,
    ) -> i32 {
        let h_axis = Axis {
            edges: h_edges,
            box_sizing: c.box_sizing,
        };
        let tentative = if !c.width.is_auto() {
            h_axis.content_from(c.width.to_cells_h(axis.width))
        } else if align == AlignItems::Stretch
            && !axis.wraps
            && !c.margin.left.is_auto()
            && !c.margin.right.is_auto()
        {
            // §9.4 step 11's condition, on this axis: an item that stated a
            // cross size or claimed the line's free space with an auto margin
            // has already answered the question.
            avail
        } else {
            let (min_content, max_content) = self.sizer.content_widths(node);
            shrink_to_fit(min_content, max_content, avail)
        };
        let resolve = |len: Length| (!len.is_auto()).then(|| len.to_cells_h(axis.width));
        h_axis.clamp(tentative, resolve(c.min_width), resolve(c.max_width))
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_anonymous_block(
        &mut self,
        x: i32,
        width: i32,
        y: i32,
        prev_margin_bottom: &mut i32,
        items: &[InlineItem],
        align: TextAlign,
        pre: bool,
    ) -> Option<BoxId> {
        if items.is_empty() {
            return None;
        }
        // Same adjacent-sibling collapse as real blocks: the previous sibling
        // already advanced `y` through its bottom margin; pull back and re-apply
        // max(anon_top=0, prev_bottom). Zeroing prev (the old path) ate the
        // gap between `<p>one</p>two`.
        let y_after_prev = y - *prev_margin_bottom;
        let used_top = if y_after_prev == 0 && *prev_margin_bottom == 0 {
            0
        } else {
            *prev_margin_bottom
        };
        let y = y_after_prev + used_top;
        *prev_margin_bottom = 0;

        let box_id = self.alloc(LayoutBox {
            kind: BoxKind::AnonymousBlock,
            node: None,
            dimensions: Dimensions {
                content: Rect {
                    x,
                    y,
                    width,
                    height: 0,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: None,
            term_style: Style::default(),
            computed: ComputedStyle::default(),
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });

        let line_ids = if pre {
            self.layout_pre(items, x, y, width)
        } else {
            self.layout_inline(items, x, y, width, align)
        };
        let height = line_ids
            .last()
            .map(|&id| {
                let d = &self.boxes[id.0 as usize].dimensions;
                d.content.y + d.content.height - y
            })
            .unwrap_or(0);
        self.boxes[box_id.0 as usize].dimensions.content.height = height;
        self.boxes[box_id.0 as usize].children = line_ids;
        Some(box_id)
    }

    /// Block-container child `<img>`: wrap as a single replaced box.
    fn layout_img_block(
        &mut self,
        id: NodeId,
        computed: ComputedStyle,
        x: i32,
        containing_width: i32,
        y: i32,
        prev_margin_bottom: &mut i32,
    ) -> Option<BoxId> {
        let Some(img) = self.images.by_node.get(&id) else {
            // No src / not discovered — nothing to show.
            return None;
        };
        let (cw, ch, firm) = self.images.size_for(img, containing_width);
        let y = y - *prev_margin_bottom;
        *prev_margin_bottom = 0;
        let w = cw.min(containing_width).max(1);
        let h = ch.max(1);
        Some(self.alloc(LayoutBox {
            kind: BoxKind::Image,
            node: Some(id),
            dimensions: Dimensions {
                content: Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: Some(img.alt.clone()),
            term_style: Style::default(),
            computed,
            image_src: Some(img.url.clone()),
            image_size_firm: firm,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        }))
    }

    fn layout_br(
        &mut self,
        x: i32,
        width: i32,
        y: i32,
        prev_margin_bottom: &mut i32,
        computed: ComputedStyle,
    ) -> Option<BoxId> {
        let y = y - *prev_margin_bottom;
        *prev_margin_bottom = 0;
        // A line box one cell tall with no text — forces a visual break.
        let line = self.alloc(LayoutBox {
            kind: BoxKind::Line,
            node: None,
            dimensions: Dimensions {
                content: Rect {
                    x,
                    y,
                    width,
                    height: 1,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: None,
            term_style: Style::default(),
            computed,
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });
        Some(line)
    }

    fn layout_hr(
        &mut self,
        x: i32,
        width: i32,
        y: i32,
        prev_margin_bottom: &mut i32,
        computed: ComputedStyle,
    ) -> Option<BoxId> {
        let mut dims = resolve_block_dims(&computed, width, None);
        let top = dims.margin.top.max(*prev_margin_bottom);
        let y = y - *prev_margin_bottom + top;
        dims.margin.top = top;
        dims.content.x = x + dims.margin.left + dims.border.left + dims.padding.left;
        dims.content.y = y + dims.border.top + dims.padding.top;
        dims.content.width = (width
            - dims.margin.left
            - dims.margin.right
            - dims.border.left
            - dims.border.right
            - dims.padding.left
            - dims.padding.right)
            .max(0);
        dims.content.height = 1;

        let text = if dims.content.width > 0 {
            "─".repeat(dims.content.width as usize)
        } else {
            String::new()
        };
        let text_id = self.alloc(LayoutBox {
            kind: BoxKind::Text,
            node: None,
            dimensions: Dimensions {
                content: Rect {
                    x: dims.content.x,
                    y: dims.content.y,
                    width: dims.content.width,
                    height: 1,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: Some(text),
            term_style: Style::default(),
            computed: ComputedStyle::default(),
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });
        let box_id = self.alloc(LayoutBox {
            kind: BoxKind::Block,
            node: None,
            dimensions: dims,
            children: vec![text_id],
            text: None,
            term_style: Style::default(),
            computed,
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });
        *prev_margin_bottom = dims.margin.bottom;
        Some(box_id)
    }

    /// Inline formatting context: wrap `items` into line boxes.
    fn layout_inline(
        &mut self,
        items: &[InlineItem],
        x: i32,
        y: i32,
        width: i32,
        align: TextAlign,
    ) -> Vec<BoxId> {
        let width = width.max(1);
        // Flatten to words / breaks / atomic images. Images flush the current
        // text line and occupy `cells_h` rows of their own (M8).
        let mut frags: Vec<InlineFrag> = Vec::new();
        let mut pending_space: Option<Style> = None;

        for item in items {
            match item {
                InlineItem::Marker { text, style } => {
                    frags.push(InlineFrag::Piece(Piece::word(
                        text.clone(),
                        text.width() as i32,
                        *style,
                        None,
                    )));
                }
                InlineItem::Spacer { cells } => {
                    if *cells > 0 {
                        // A `Word`, not a space: margins must not collapse or
                        // vanish at a line edge the way HTML whitespace does.
                        frags.push(InlineFrag::Piece(Piece::word(
                            " ".repeat(*cells as usize),
                            *cells,
                            Style::default(),
                            None,
                        )));
                    }
                }
                InlineItem::Break => {
                    pending_space = None;
                    frags.push(InlineFrag::Piece(Piece::line_break()));
                }
                // An atomic inline is one piece of the line, so the space
                // before it is a real break opportunity — unlike an image,
                // which takes rows of its own and drops the space with them.
                InlineItem::Atomic { node } => {
                    if let Some(style) = pending_space.take() {
                        frags.push(InlineFrag::Piece(Piece::space(style)));
                    }
                    frags.push(InlineFrag::Atomic { node: *node });
                }
                InlineItem::Image {
                    node,
                    url,
                    alt,
                    cells_w,
                    cells_h,
                    firm,
                    computed,
                } => {
                    pending_space = None;
                    frags.push(InlineFrag::Image {
                        node: *node,
                        url: url.clone(),
                        alt: alt.clone(),
                        cells_w: *cells_w,
                        cells_h: *cells_h,
                        firm: *firm,
                        computed: computed.clone(),
                    });
                }
                InlineItem::Text {
                    node, text, style, ..
                } => {
                    if text.starts_with(is_html_space) {
                        pending_space = Some(*style);
                    }
                    let mut first = true;
                    for word in text.split(is_html_space).filter(|w| !w.is_empty()) {
                        if !first || pending_space.is_some() {
                            frags.push(InlineFrag::Piece(Piece::space(
                                pending_space.unwrap_or(*style),
                            )));
                        }
                        first = false;
                        pending_space = None;
                        frags.push(InlineFrag::Piece(Piece::word(
                            word.to_string(),
                            word.width() as i32,
                            *style,
                            Some(*node),
                        )));
                    }
                    if text.ends_with(is_html_space) {
                        pending_space = Some(*style);
                    }
                }
            }
        }

        let mut lines: Vec<BoxId> = Vec::new();
        let mut line_y = y;
        let mut cur: Vec<Piece> = Vec::new();
        let mut cur_cells = 0i32;

        let flush_cur = |eng: &mut Engine<'a>,
                         cur: &mut Vec<Piece>,
                         line_y: &mut i32,
                         lines: &mut Vec<BoxId>,
                         cur_cells: &mut i32| {
            if cur.is_empty() {
                return;
            }
            if cur.last().is_some_and(|p| p.is_space()) {
                cur.pop();
            }
            if !cur.is_empty() {
                eng.emit_line(cur, line_y, lines, x, width, align);
            }
            *cur_cells = 0;
        };

        for frag in frags {
            let piece = match frag {
                InlineFrag::Image {
                    node,
                    url,
                    alt,
                    cells_w,
                    cells_h,
                    firm,
                    computed,
                } => {
                    flush_cur(self, &mut cur, &mut line_y, &mut lines, &mut cur_cells);
                    self.emit_image(
                        &mut line_y,
                        &mut lines,
                        x,
                        width,
                        &InlineItem::Image {
                            node,
                            url,
                            alt,
                            cells_w,
                            cells_h,
                            firm,
                            computed,
                        },
                    );
                    continue;
                }
                // An atomic inline is sized against the room left on *this*
                // line (CSS 2.1 §10.3.9), so it can only be built here — and
                // if what comes back does not fit, the line breaks before it
                // and it is sized again against a whole one. Never split: a
                // box that had to wrap internally already did so, inside
                // itself, at the width it was given.
                InlineFrag::Atomic { node } => {
                    let mut dims = self.atomic_dims(node, width, width - cur_cells);
                    if !cur.is_empty() && cur_cells + dims.margin_box_width() > width {
                        if cur.last().is_some_and(|p| p.is_space()) {
                            cur.pop();
                        }
                        self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
                        cur_cells = 0;
                        dims = self.atomic_dims(node, width, width);
                    }
                    // `false`: an atomic inside a `<pre>` reaches the breaker
                    // through `layout_pre`, never through here.
                    self.build_atomic(node, dims, x + cur_cells, line_y, width, false)
                }
                InlineFrag::Piece(p) => p,
            };
            if matches!(piece.kind, PieceKind::Break) {
                if cur.is_empty() {
                    self.emit_empty_line(x, &mut line_y, width, &mut lines);
                } else {
                    self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
                }
                cur_cells = 0;
                continue;
            }
            if piece.is_space() {
                if cur.is_empty() {
                    continue; // leading spaces dropped
                }
                cur.push(piece);
                cur_cells += 1;
                continue;
            }
            if matches!(piece.kind, PieceKind::Atomic(_)) {
                // Already placed on the line it belongs to, above. It can still
                // be wider than the line — a box whose min-content width does
                // not fit overflows rather than being broken — and that is the
                // same overflow a too-long unbreakable word has, not a case of
                // its own.
                cur_cells += piece.cells;
                cur.push(piece);
                continue;
            }

            // Overlong word: hard-break by cells.
            if piece.cells > width {
                if cur.last().is_some_and(|p| p.is_space()) {
                    cur.pop();
                }
                if !cur.is_empty() {
                    self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
                }
                let style = piece.style;
                let node = piece.node;
                let mut rest = piece.text.as_str();
                while !rest.is_empty() {
                    let limit = width.max(1) as usize;
                    let mut cells = 0usize;
                    let mut end = 0;
                    for (i, ch) in rest.char_indices() {
                        let w = ch.width().unwrap_or(0);
                        if end > 0 && cells + w > limit {
                            break;
                        }
                        cells += w;
                        end = i + ch.len_utf8();
                    }
                    cur.push(Piece::word(
                        rest[..end].to_string(),
                        cells as i32,
                        style,
                        node,
                    ));
                    rest = &rest[end..];
                    if !rest.is_empty() {
                        self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
                    }
                }
                cur_cells = cur.iter().map(|p| p.cells).sum();
                continue;
            }

            // Word that does not fit: wrap (consume trailing space).
            if !cur.is_empty() && cur_cells + piece.cells > width {
                if cur.last().is_some_and(|p| p.is_space()) {
                    cur.pop();
                }
                self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
            }
            cur.push(piece);
            cur_cells = cur.iter().map(|p| p.cells).sum();
        }
        self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
        lines
    }

    /// How wide an atomic inline is, and what its edges are, given `available`
    /// cells left on the line for its **margin box** (M9.11).
    ///
    /// CSS 2.1 §10.3.9: an inline-block with a `width` is that wide, and one
    /// without is *shrink-to-fit* —
    /// `min(max(min-content, available), max-content)`. Then M9.2's `min-width`
    /// / `max-width` clamps and `box-sizing`, which is why the block resolver
    /// does the first pass: everything but the `auto` branch is identical, and
    /// two copies of the clamp order is how the two drift apart.
    ///
    /// No box is built here. Sizing and building are separate because the
    /// breaker has to know how wide the box *would* be before it can decide
    /// which line it goes on — and if it goes on the next one, the answer
    /// changes, because `available` did.
    ///
    /// **Where this diverges from a browser, deliberately.** CSS 2.1 reads
    /// "available width" as the *containing block's* width, so a browser sizes
    /// an inline-block the same wherever it lands: a box that does not fit
    /// moves to the next line at the width it already had. Here `available` is
    /// what is left of the current line (M9.11's spec), so a box with room for
    /// only some of its content squeezes into that room and wraps inside
    /// itself rather than leaving the tail of a line empty — worth more on an
    /// 80-cell terminal than the browser's rule is. The cost is that the same
    /// box can come out two widths on two runs at different column widths.
    /// `tests/fixtures/layout/spec/inline-block.boxes` pins both readings
    /// apart: its third paragraph is the one that would change.
    fn atomic_dims(&mut self, node: NodeId, containing_width: i32, available: i32) -> Dimensions {
        let computed = self.styles.get(node).clone();
        let mut dims = resolve_block_dims(&computed, containing_width, None);
        // An `auto` margin on an inline-level box is zero (CSS 2.1 §10.3.9),
        // not the free space of its containing block: a badge in a sentence
        // does not centre itself in the paragraph.
        if computed.margin.left.is_auto() {
            dims.margin.left = 0;
        }
        if computed.margin.right.is_auto() {
            dims.margin.right = 0;
        }
        if computed.width.is_auto() {
            let edges = dims.margin_box_width() - dims.content.width;
            let (min_content, max_content) = self.sizer.content_widths(node);
            let axis = Axis {
                edges: dims.padding.left
                    + dims.padding.right
                    + dims.border.left
                    + dims.border.right,
                box_sizing: computed.box_sizing,
            };
            let resolve = |len: Length| (!len.is_auto()).then(|| len.to_cells_h(containing_width));
            dims.content.width = axis.clamp(
                shrink_to_fit(min_content, max_content, available - edges),
                resolve(computed.min_width),
                resolve(computed.max_width),
            );
        }
        dims
    }

    /// Build an atomic inline's box at `x`, `y` — its margin-box origin — and
    /// return the piece the line places.
    ///
    /// Provisional coordinates: the line does not know its baseline row until
    /// every piece on it is sized, so [`emit_line`](Self::emit_line) moves this
    /// subtree once it does. Building it here rather than there is what lets
    /// the box report the two numbers the line needs — how many rows it is, and
    /// which of them carries its baseline — since both are answers only its own
    /// contents can give.
    fn build_atomic(
        &mut self,
        node: NodeId,
        mut dims: Dimensions,
        x: i32,
        y: i32,
        _containing_width: i32,
        pre: bool,
    ) -> Piece {
        let computed = self.styles.get(node).clone();
        let tag = match &self.dom.node(node).data {
            NodeData::Element { tag, .. } => tag.clone(),
            _ => String::new(),
        };
        let cells = dims.margin_box_width();
        dims.content.x = x + dims.margin.left + dims.border.left + dims.padding.left;
        dims.content.y = y + dims.margin.top + dims.border.top + dims.padding.top;
        dims.content.height = 0;
        // `None`: a line box has no definite height of its own, so a percentage
        // height inside an atomic inline behaves as `auto` — M9.2's rule for
        // any box whose containing block is sized by its content.
        let box_id = self.layout_box_at(node, &tag, computed, dims, None, pre);
        Piece::atomic(box_id, cells)
    }

    /// How many rows an atomic inline's margin box occupies, and which of them
    /// carries its baseline, counted from its top.
    ///
    /// CSS 2.1 §10.8.1: that baseline is the baseline of the box's **last**
    /// line box — the last row of text in it, which is what makes a two-line
    /// badge line up with the sentence by its second line and not its first.
    /// A box with no line box at all (an empty one, or one holding only
    /// another box) takes its bottom margin edge instead, one row past its
    /// last: it then sits entirely above the text beside it, the way a letter
    /// sits on a rule.
    fn atomic_rows(&self, box_id: BoxId) -> (i32, i32) {
        let margin_box = self.boxes[box_id.0 as usize].dimensions.margin_box();
        let baseline = match self.last_line_row(box_id) {
            Some(row) => row - margin_box.y,
            None => margin_box.height,
        };
        (margin_box.height, baseline)
    }

    /// Place a replaced image: one multi-row `BoxKind::Image` on its own
    /// line-box-equivalent span. Flushes vertical space by advancing `line_y`.
    fn emit_image(
        &mut self,
        line_y: &mut i32,
        lines: &mut Vec<BoxId>,
        x: i32,
        containing_width: i32,
        img: &InlineItem,
    ) {
        let InlineItem::Image {
            node,
            url,
            alt,
            cells_w,
            cells_h,
            firm,
            computed,
        } = img
        else {
            return;
        };
        let w = (*cells_w).clamp(1, containing_width);
        let h = (*cells_h).max(1);
        let img_id = self.alloc(LayoutBox {
            kind: BoxKind::Image,
            node: Some(*node),
            dimensions: Dimensions {
                content: Rect {
                    x,
                    y: *line_y,
                    width: w,
                    height: h,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: Some(alt.clone()),
            term_style: Style::default(),
            computed: computed.clone(),
            image_src: Some(url.clone()),
            image_size_firm: *firm,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });
        lines.push(img_id);
        *line_y += h;
    }

    fn emit_line(
        &mut self,
        cur: &mut Vec<Piece>,
        line_y: &mut i32,
        lines: &mut Vec<BoxId>,
        x: i32,
        width: i32,
        align: TextAlign,
    ) {
        if cur.is_empty() {
            return;
        }
        while cur.last().is_some_and(|p| p.is_space()) {
            cur.pop();
        }
        if cur.is_empty() {
            return;
        }
        // How wide the line's content is, and how many rows it needs either
        // side of its baseline. One pass, because this is the inner loop of
        // every inline formatting context on the page — and for a line of
        // plain text both depths are zero, so the arithmetic below collapses
        // to "one row, text on it", exactly as it was before M9.11.
        let mut content_cells = 0;
        let mut above = 0;
        let mut below = 0;
        for p in cur.iter() {
            content_cells += p.cells;
            if let PieceKind::Atomic(box_id) = p.kind {
                let (height, baseline) = self.atomic_rows(box_id);
                above = above.max(baseline);
                below = below.max((height - 1 - baseline).max(0));
            }
        }
        let height = above + 1 + below;
        // The row everything on this line is aligned on: text sits on it, and
        // an atomic inline hangs `baseline` rows above it.
        let baseline_row = *line_y + above;
        let shift = match align {
            TextAlign::Left => 0,
            TextAlign::Center => ((width - content_cells) / 2).max(0),
            TextAlign::Right => (width - content_cells).max(0),
        };
        let line_id = self.alloc(LayoutBox {
            kind: BoxKind::Line,
            node: None,
            dimensions: Dimensions {
                content: Rect {
                    x,
                    y: *line_y,
                    width,
                    height,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: None,
            term_style: Style::default(),
            computed: ComputedStyle::default(),
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });
        // Merge adjacent same-style pieces. An atomic inline is a box, not
        // characters, so it merges with nothing on either side.
        let mut merged: Vec<Piece> = Vec::new();
        for p in cur.drain(..) {
            match merged.last_mut() {
                Some(last) if !p.is_atomic() && !last.is_atomic() && last.style == p.style => {
                    last.text.push_str(&p.text);
                    last.cells += p.cells;
                    if last.node.is_none() {
                        last.node = p.node;
                    }
                }
                _ => merged.push(p),
            }
        }
        let mut cx = x + shift;
        let mut child_ids = Vec::new();
        for p in merged {
            let id = match p.kind {
                // Built before the line knew where it would go, so it moves
                // now: across by the text-align shift and whatever preceded
                // it, and down to hang off the shared baseline row.
                PieceKind::Atomic(box_id) => {
                    let placed = self.boxes[box_id.0 as usize].dimensions.margin_box();
                    let (_, baseline) = self.atomic_rows(box_id);
                    self.shift_subtree(box_id, cx - placed.x, (baseline_row - baseline) - placed.y);
                    let computed = self.boxes[box_id.0 as usize].computed.clone();
                    if matches!(computed.position, Position::Relative | Position::Sticky) {
                        self.apply_relative(box_id, computed, width, None);
                    }
                    box_id
                }
                _ => self.alloc(LayoutBox {
                    kind: BoxKind::Text,
                    node: p.node,
                    dimensions: Dimensions {
                        content: Rect {
                            x: cx,
                            y: baseline_row,
                            width: p.cells,
                            height: 1,
                        },
                        ..Dimensions::default()
                    },
                    children: Vec::new(),
                    text: Some(p.text),
                    term_style: p.style,
                    computed: ComputedStyle::default(),
                    image_src: None,
                    image_size_firm: false,
                    fixed_viewport: false,
                    sticky: None,
                    grid: None,
                }),
            };
            cx += p.cells;
            child_ids.push(id);
        }
        self.boxes[line_id.0 as usize].children = child_ids;
        lines.push(line_id);
        *line_y += height;
    }

    fn layout_pre(&mut self, items: &[InlineItem], x: i32, y: i32, width: i32) -> Vec<BoxId> {
        // Preserve newlines and per-span styles. A `\n` or Break closes the line.
        let mut lines = Vec::new();
        let mut line_y = y;
        let mut cur: Vec<Piece> = Vec::new();

        for item in items {
            match item {
                InlineItem::Break => {
                    if cur.is_empty() {
                        self.emit_empty_line(x, &mut line_y, width, &mut lines);
                    } else {
                        self.emit_line(
                            &mut cur,
                            &mut line_y,
                            &mut lines,
                            x,
                            width,
                            TextAlign::Left,
                        );
                    }
                }
                InlineItem::Spacer { cells } => {
                    if *cells > 0 {
                        cur.push(Piece::word(
                            " ".repeat(*cells as usize),
                            *cells,
                            Style::default(),
                            None,
                        ));
                    }
                }
                InlineItem::Marker { text, style } => {
                    cur.push(Piece::word(text.clone(), text.width() as i32, *style, None));
                }
                // Nothing wraps in a `<pre>`, so an atomic inline here is
                // simply sized against what is left of the line and placed:
                // there is no second line for it to move to.
                InlineItem::Atomic { node } => {
                    let placed: i32 = cur.iter().map(|p| p.cells).sum();
                    let dims = self.atomic_dims(*node, width, width - placed);
                    let piece = self.build_atomic(*node, dims, x + placed, line_y, width, true);
                    cur.push(piece);
                }
                img @ InlineItem::Image { .. } => {
                    if !cur.is_empty() {
                        self.emit_line(
                            &mut cur,
                            &mut line_y,
                            &mut lines,
                            x,
                            width,
                            TextAlign::Left,
                        );
                    }
                    self.emit_image(&mut line_y, &mut lines, x, width, img);
                }
                InlineItem::Text {
                    node, text, style, ..
                } => {
                    let mut rest = text.as_str();
                    while let Some(nl) = rest.find('\n') {
                        let before = &rest[..nl];
                        if !before.is_empty() {
                            cur.push(Piece::word(
                                before.to_string(),
                                before.width() as i32,
                                *style,
                                Some(*node),
                            ));
                        }
                        if cur.is_empty() {
                            self.emit_empty_line(x, &mut line_y, width, &mut lines);
                        } else {
                            self.emit_line(
                                &mut cur,
                                &mut line_y,
                                &mut lines,
                                x,
                                width,
                                TextAlign::Left,
                            );
                        }
                        rest = &rest[nl + 1..];
                    }
                    if !rest.is_empty() {
                        cur.push(Piece::word(
                            rest.to_string(),
                            rest.width() as i32,
                            *style,
                            Some(*node),
                        ));
                    }
                }
            }
        }
        if !cur.is_empty() {
            self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, TextAlign::Left);
        } else if lines.is_empty() {
            self.emit_empty_line(x, &mut line_y, width, &mut lines);
        }
        lines
    }

    fn emit_empty_line(&mut self, x: i32, line_y: &mut i32, width: i32, lines: &mut Vec<BoxId>) {
        let line_id = self.alloc(LayoutBox {
            kind: BoxKind::Line,
            node: None,
            dimensions: Dimensions {
                content: Rect {
                    x,
                    y: *line_y,
                    width,
                    height: 1,
                },
                ..Dimensions::default()
            },
            children: Vec::new(),
            text: None,
            term_style: Style::default(),
            computed: ComputedStyle::default(),
            image_src: None,
            image_size_firm: false,
            fixed_viewport: false,
            sticky: None,
            grid: None,
        });
        lines.push(line_id);
        *line_y += 1;
    }

    fn child_mode(&self, id: NodeId, _pre: bool) -> ChildMode {
        match &self.dom.node(id).data {
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => ChildMode::Skip,
            NodeData::Text(_) => ChildMode::Inline,
            NodeData::Element { tag, .. } => {
                if self.is_hidden(id) {
                    return ChildMode::Skip;
                }
                if tag == "br" || tag == "hr" {
                    return ChildMode::Block;
                }
                // A control drawn as nothing joins no formatting context at all
                // (M11.8) — it is not an empty inline, it is absent.
                if field::generates_no_box(self.dom, id, tag) {
                    return ChildMode::Skip;
                }
                // Block-level `display:block` img goes through layout_node;
                // default inline img stays in the IFC as an atomic replaced box.
                if tag == "img" {
                    return if is_block_level(self.styles.get(id).display) {
                        ChildMode::Block
                    } else {
                        ChildMode::Inline
                    };
                }
                let display = self.styles.get(id).display;
                // An atomic inline joins the line for the same reason a plain
                // inline does — it is inline-*level* — and differs only in
                // what the inline formatting context then does with it.
                if display == Display::Inline || is_atomic_inline(display) {
                    ChildMode::Inline
                } else {
                    // Reveal: a page-hidden box is walked as block so its
                    // subtree can surface. UA-important none never gets here.
                    // Everything left is block-level.
                    ChildMode::Block
                }
            }
        }
    }

    fn push_inline(&self, id: NodeId, pre: bool, containing_width: i32, out: &mut Vec<InlineItem>) {
        match &self.dom.node(id).data {
            NodeData::Text(text) => {
                out.push(InlineItem::Text {
                    node: id,
                    text: text.clone(),
                    style: term_style(self.styles.get(id)),
                    computed: self.styles.get(id).clone(),
                });
            }
            NodeData::Element { tag, .. } => {
                if self.is_hidden(id) {
                    return;
                }
                // An out-of-flow inline neither leaves a text fragment nor a
                // line-sized atomic item. Its block parent schedules it after
                // normal flow has been laid out.
                if is_out_of_flow(self.styles.get(id).position) {
                    return;
                }
                if tag == "br" {
                    // Nested `<br>` inside an inline (e.g. `<span>a<br>b</span>`).
                    out.push(InlineItem::Break);
                    return;
                }
                if tag == "img" {
                    if let Some(img) = self.images.by_node.get(&id) {
                        let (cw, ch, firm) = self.images.size_for(img, containing_width);
                        out.push(InlineItem::Image {
                            node: id,
                            url: img.url.clone(),
                            alt: img.alt.clone(),
                            cells_w: cw,
                            cells_h: ch,
                            firm,
                            computed: self.styles.get(id).clone(),
                        });
                    }
                    return;
                }
                // A form control is a *replaced* element, so it goes on the line
                // whole whatever `display` the page gave it (M11.8) — the UA
                // sheet says `inline-block`, and a page that says `inline`
                // still gets a box rather than its `<textarea>`'s value flowing
                // into the paragraph as prose.
                if field::is_control_tag(tag) {
                    if !field::generates_no_box(self.dom, id, tag) {
                        out.push(InlineItem::Atomic { node: id });
                    }
                    return;
                }
                let computed = self.styles.get(id);
                if matches!(computed.position, Position::Relative | Position::Sticky) {
                    // A relative inline needs a real subtree to move after it
                    // has claimed its ordinary line space. Atomic placement is
                    // the engine's existing representation of that invariant.
                    out.push(InlineItem::Atomic { node: id });
                    return;
                }
                // An atomic inline goes on the line whole, edges and all: its
                // margins and padding belong to its own box rather than
                // becoming spacers, and its contents are laid out inside it
                // rather than flowing into this line (M9.11).
                //
                // `<br>` and `<img>` are already gone by here, which leaves one
                // element whose real layout lives in a function this path does
                // not call: an `<hr>` nested inside an inline element (invalid
                // markup the parser usually breaks up first) gets an empty
                // atomic box instead of a rule. It still costs the line its 1em
                // UA margins, which is what a browser does with the box too —
                // only the rule inside it is missing.
                if is_atomic_inline(computed.display) {
                    out.push(InlineItem::Atomic { node: id });
                    return;
                }
                // Horizontal margin/padding on inlines (HN `.hnname { margin-right }`).
                let lead = edge_h(computed.margin.left, containing_width)
                    + edge_h(computed.padding.left, containing_width)
                    + edge_h(computed.border.left, containing_width);
                let trail = edge_h(computed.margin.right, containing_width)
                    + edge_h(computed.padding.right, containing_width)
                    + edge_h(computed.border.right, containing_width);
                if lead > 0 {
                    out.push(InlineItem::Spacer { cells: lead });
                }
                let pre = pre || tag == "pre";
                for child in self.dom.children(id) {
                    self.push_inline(child, pre, containing_width, out);
                }
                if trail > 0 {
                    out.push(InlineItem::Spacer { cells: trail });
                }
            }
            _ => {}
        }
    }

    fn collect_inline(&self, id: NodeId, pre: bool, containing_width: i32) -> Vec<InlineItem> {
        let mut out = Vec::new();
        self.push_inline(id, pre, containing_width, &mut out);
        out
    }

    /// Absolute descendants under ordinary inline wrappers have no inline box
    /// to own a placement pass. Their nearest block formatting owner schedules
    /// them; an atomic inline/block descendant stops the walk because its own
    /// box construction will schedule its children instead.
    fn inline_absolute_descendants(&self, id: NodeId) -> Vec<NodeId> {
        fn walk(eng: &Engine<'_>, node: NodeId, out: &mut Vec<NodeId>) {
            for child in eng.dom.children(node) {
                let NodeData::Element { .. } = &eng.dom.node(child).data else {
                    continue;
                };
                let style = eng.styles.get(child);
                if is_out_of_flow(style.position) {
                    out.push(child);
                } else if style.display == Display::Inline {
                    walk(eng, child, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, id, &mut out);
        out
    }

    /// Deferred out-of-flow children must be restored to source order before
    /// paint/hit testing observes the ordinary child list.
    fn order_children_by_dom(&self, owner: NodeId, children: &mut [BoxId]) {
        fn first_direct(eng: &Engine<'_>, owner: NodeId, box_id: BoxId) -> Option<NodeId> {
            let b = &eng.boxes[box_id.0 as usize];
            if let Some(mut node) = b.node {
                while let Some(parent) = eng.dom.node(node).parent {
                    if parent == owner {
                        return Some(node);
                    }
                    node = parent;
                }
            }
            b.children
                .iter()
                .find_map(|&child| first_direct(eng, owner, child))
        }
        let direct: Vec<NodeId> = self.dom.children(owner).collect();
        children.sort_by_key(|&box_id| {
            first_direct(self, owner, box_id)
                .and_then(|node| direct.iter().position(|&candidate| candidate == node))
                .unwrap_or(usize::MAX)
        });
    }
}

/// The rows a form control's own text sits on — its first and its last — or
/// `None` for every other box (M11.8).
///
/// A control has no line boxes of its own, and CSS 2.1 §10.8.1's fallback for a
/// box without one is its bottom margin edge: taken literally, a field would
/// hang entirely above the sentence naming it and `Search: [        ]` would
/// print the label a row below the box. A browser lines an input up with the
/// text beside it by the row its *value* is on, and here that row is a fact the
/// box already carries.
fn field_baseline_rows(b: &LayoutBox) -> Option<(i32, i32)> {
    let BoxKind::Field(_) = b.kind else {
        return None;
    };
    let content = b.dimensions.content;
    Some((content.y, content.y + (content.height - 1).max(0)))
}

/// Horizontal length → cells; `auto` is zero for margin edges.
pub(super) fn edge_h(len: crate::style::values::Length, containing_width: i32) -> i32 {
    if len.is_auto() {
        0
    } else {
        len.to_cells_h(containing_width)
    }
}

/// Vertical length → lines; `auto` is zero for margin edges.
///
/// A percentage still resolves against the containing block's **width** — CSS
/// 2.1's rule for padding and margins on both axes, which is why this is
/// `to_cells_v` and not `to_lines`. `to_lines` is for `height` and its clamps,
/// and getting the two crossed is the likeliest bug on a vertical main axis.
pub(super) fn edge_v(len: crate::style::values::Length, containing_width: i32) -> i32 {
    if len.is_auto() {
        0
    } else {
        len.to_cells_v(containing_width)
    }
}

/// A physical horizontal inset. `auto` remains absent so the caller can apply
/// start-over-end precedence rather than confusing it with zero.
fn inset_h(len: Length, containing_width: i32) -> Option<i32> {
    (!len.is_auto()).then(|| len.to_cells_h(containing_width))
}

/// A vertical inset. Percentages need a definite containing-block height;
/// absolute pixel/em values do not. This mirrors the height definiteness rule
/// rather than using the margin/padding percentage convention.
fn inset_v(len: Length, containing_width: i32, containing_height: Option<i32>) -> Option<i32> {
    match len {
        Length::Auto => None,
        Length::Percent(_) => containing_height.map(|height| len.to_lines(height)),
        _ => Some(len.to_cells_v(containing_width)),
    }
}

fn relative_delta(style: ComputedStyle, width: i32, height: Option<i32>) -> (i32, i32) {
    let dx = inset_h(style.left, width)
        .or_else(|| inset_h(style.right, width).map(|n| -n))
        .unwrap_or(0);
    let dy = inset_v(style.top, width, height)
        .or_else(|| inset_v(style.bottom, width, height).map(|n| -n))
        .unwrap_or(0);
    (dx, dy)
}

/// CSS 2.1 §10.3.5's shrink-to-fit width, the one a float uses:
/// `min(max(min-content, available), max-content)`.
fn shrink_to_fit(min_content: i32, max_content: i32, available: i32) -> i32 {
    min_content
        .max(available)
        .min(max_content.max(min_content))
        .max(0)
}

/// The physical near edge of something placed `offset` cells from a
/// **reversed** axis's start edge: `row-reverse` and `column-reverse` on the
/// main axis, `wrap-reverse` on the cross one.
///
/// A reversed axis starts at the far edge, so offsets are subtracted rather
/// than added. The subtlety is *which* far edge, and getting it wrong is a bug
/// with teeth. It is the container's while the content fits — and the content's
/// own when it does not. Counting back from the container's edge through
/// content that overflows it puts the overflow *before* the near edge, at a
/// negative row or column: a terminal has no row above 0 and no column left of
/// it, so that content is not merely off-screen, it is unreachable by any
/// amount of scrolling. Measuring from the content's own far edge instead
/// leaves the near end exactly where a forward axis would have left it, with
/// the overflow running off the far end, which is the end a reader can still
/// get to. That is the same "safe" reasoning §9.5 and §9.6 already apply
/// *within* a line, finally applied to the mapping around it.
///
/// `occupied` is how much room the content really takes, which is what makes
/// the guarantee provable: everything placed on the axis fits inside
/// `offset + size <= occupied`, so the result is never less than `near`.
fn from_far_edge(near: i32, available: i32, occupied: i32, offset: i32, size: i32) -> i32 {
    near.saturating_add(available.max(occupied))
        .saturating_sub(offset)
        .saturating_sub(size)
}

/// `align-self` resolved against the container's `align-items`, and then
/// against what this container's cross axis can actually honour.
fn cross_align(c: &ComputedStyle, axis: &FlexAxis, container: &ComputedStyle) -> AlignItems {
    let align = c.align_self.resolve(container.align_items);
    if axis.vertical {
        column_cross_align(align)
    } else {
        align
    }
}

/// `align-items` as a **column**'s cross axis can honour it.
///
/// Five of the six values are the same question turned sideways. `baseline` is
/// not: a baseline is a *row* in a cell grid, and a column's cross axis is the
/// horizontal one, so there is no shared row to stitch items to and nothing for
/// the value to do. It degrades to `flex-start`.
///
/// This one really is a degradation rather than an implementation of the value,
/// which is why it is a named function with this comment on it rather than a
/// silent arm somewhere: a reader who finds `align-items: baseline` doing
/// nothing in a column should find the decision, not a bug.
fn column_cross_align(align: AlignItems) -> AlignItems {
    match align {
        AlignItems::Baseline => AlignItems::FlexStart,
        other => other,
    }
}

/// Does §9.2's base size or §4.5's automatic minimum need to know how big this
/// item's *content* wants to be on the main axis?
///
/// Asked so the measurement happens at most once per item, and only when one of
/// the two actually reads it — measuring an inline subtree is the expensive
/// thing flex added to the layout path.
fn wants_content_size(c: &ComputedStyle, axis: &FlexAxis) -> bool {
    matches!(c.flex.basis, FlexBasis::Content)
        || (matches!(c.flex.basis, FlexBasis::Auto)
            && axis.main_definite(axis.main_size(c)).is_none())
        || (axis.main_min(c).is_auto() && !axis.main_overflow_clips(c))
}

/// §9.2 steps 3 and 4 and §4.5, from the two content sizes the axis was able to
/// supply — `(min-content, max-content)` for a row, and the height the item's
/// content used, twice, for a column.
///
/// Pure arithmetic over one style, so both directions reach the same rules by
/// the same route: the only thing that differs between them is which property
/// each "main-axis" question resolves to, and that is [`FlexAxis`]'s job.
fn item_metrics(c: &ComputedStyle, axis: &FlexAxis, content: (i32, i32)) -> flex::Item {
    let box_axis = Axis {
        edges: axis.main_box_edges(c),
        box_sizing: c.box_sizing,
    };
    let (margin_start, margin_end) = axis.main_margins(c);
    let outer_edges = box_axis.edges + axis.edge(margin_start) + axis.edge(margin_end);
    let specified = axis
        .main_definite(axis.main_size(c))
        .map(|v| box_axis.content_from(v));

    // §9.2 step 3: the flex base size.
    //
    // `content` is the max-content size outright; a length or percentage
    // resolves on the main axis; `auto` defers to the main-axis size property,
    // and to the content size when that is `auto` too. A percentage against an
    // indefinite main size — a `height: auto` column — behaves as `auto` and
    // lands in the same place. This is the step that needs M9.4 for a row: the
    // engine can fill an available width, but only intrinsic sizing can say how
    // wide content *wants* to be.
    let base = match c.flex.basis {
        FlexBasis::Content => content.1,
        FlexBasis::Size(len) => match axis.main_definite(len) {
            Some(cells) => box_axis.content_from(cells),
            None => content.1,
        },
        FlexBasis::Auto => specified.unwrap_or(content.1),
    };

    let max = axis
        .main_definite(axis.main_max(c))
        .map(|v| box_axis.content_from(v));
    let min = if axis.main_min(c).is_auto() {
        // §4.5, the automatic minimum size — the rule that stops a flex row
        // from shredding a word one cell at a time, and a column from squeezing
        // an item shorter than its own text. A scroll container opts out of it
        // (a clipped box is allowed to be smaller than its content; that is
        // what clipping is for).
        if axis.main_overflow_clips(c) {
            0
        } else {
            // Never larger than the size the item was explicitly given, or than
            // its own maximum: an automatic minimum that outgrew either would
            // be inventing a size the page never asked for.
            let mut min = content.0;
            if let Some(specified) = specified {
                min = min.min(specified);
            }
            if let Some(max) = max {
                min = min.min(max);
            }
            min
        }
    } else {
        axis.main_definite(axis.main_min(c))
            .map_or(0, |v| box_axis.content_from(v))
    };

    // §9.2 step 4: the hypothetical main size is the base size clamped by the
    // item's own min/max — max first, then min, the order M9.2 pinned.
    let mut hypothetical = base;
    if let Some(max) = max {
        hypothetical = hypothetical.min(max);
    }
    hypothetical = hypothetical.max(min).max(0);

    flex::Item {
        base,
        hypothetical,
        min,
        max,
        grow: c.flex.grow,
        shrink: c.flex.shrink,
        outer_edges,
    }
}

/// Which physical direction a flex container's main axis runs in, and what
/// lengths on either axis resolve against (M9.9).
///
/// `flex-direction: column` is not a second algorithm — it is the same one with
/// main and cross swapped — and this is the swap. Every "main axis" question
/// the engine asks (which size property, which margins, which padding and
/// border, which gap, which unit rule, which `overflow`) is asked here, which is
/// what lets `layout_flex_contents` and its helpers be written once in
/// main/cross terms instead of twice in `x`/`y` ones.
///
/// **Deliberately not called `Axis`.** This module already has one — a
/// box-model axis, carrying padding+border and `box-sizing` for a single
/// dimension — and `style::values` has a private one for a length's units. A
/// third meaning of the same word in the same crate is a trap for a reader.
#[derive(Clone, Copy)]
struct FlexAxis {
    /// The main axis runs down the page: `column` or `column-reverse`.
    vertical: bool,
    /// Main-start is the container's far edge — its right for `row-reverse`,
    /// its bottom for `column-reverse`.
    reverse: bool,
    /// Items that do not fit start a new line: `flex-wrap: wrap` or
    /// `wrap-reverse` (M9.10).
    wraps: bool,
    /// Cross-start is the container's far edge — its bottom for a row,
    /// its right for a column: `flex-wrap: wrap-reverse`. Lines stack back
    /// towards the near edge and every cross-axis offset is subtracted rather
    /// than added, which is the same mapping `reverse` is on the main axis.
    cross_reverse: bool,
    /// The container's inner **width**. Percentage padding and margins resolve
    /// against it on *both* axes (CSS 2.1 §8.1), and for a column it is the
    /// inner cross size outright.
    width: i32,
    /// What a percentage on the **main** axis resolves against, or `None` when
    /// the container's main size is indefinite. A row's is always `Some` — a
    /// main size that is a width is settled before anything is laid out — while
    /// a `height: auto` column has no number for a percentage to be a
    /// percentage of, so every such length behaves as `auto` (M9.2).
    main_base: Option<i32>,
    /// The same question on the cross axis, and the same asymmetry seen from
    /// the other side: a column's cross size is a width and always definite, a
    /// row's is its `height` and often is not.
    ///
    /// **Not the same number as the container's used inner cross size**, which
    /// `align_cross` computes and which its own `min-height`/`max-height` can
    /// decide. This one is what a *percentage inside the container* resolves
    /// against, and a height that only became a number because of a clamp is
    /// not one a percentage may resolve against (CSS 2.1 §10.5).
    cross_base: Option<i32>,
}

impl FlexAxis {
    fn of(direction: FlexDirection, wrap: FlexWrap, width: i32, heights: BlockHeight) -> Self {
        let vertical = matches!(
            direction,
            FlexDirection::Column | FlexDirection::ColumnReverse
        );
        FlexAxis {
            vertical,
            reverse: matches!(
                direction,
                FlexDirection::RowReverse | FlexDirection::ColumnReverse
            ),
            // **An auto-height column does not wrap, whatever `flex-wrap`
            // says**, and it is the question every reader of the wrapping code
            // asks. Wrapping needs an edge to wrap at: a row always has one,
            // because its main size is the width it was given, but a column's
            // is its height, and a column that is as tall as its own items has
            // no height it could overflow. A browser does the same. `height`
            // puts the edge there, and so does `max-height` — §9.2 step 2 says
            // to measure against the max main size when the main size itself is
            // indefinite, which is exactly what the clamp in
            // `layout_flex_contents` leaves in the inner main size.
            wraps: wrap != FlexWrap::NoWrap
                && (!vertical || heights.specified.is_some() || heights.max.is_some()),
            cross_reverse: wrap == FlexWrap::WrapReverse,
            width,
            main_base: if vertical {
                heights.specified
            } else {
                Some(width)
            },
            cross_base: if vertical {
                Some(width)
            } else {
                heights.specified
            },
        }
    }

    fn main_size(self, c: &ComputedStyle) -> Length {
        if self.vertical { c.height } else { c.width }
    }

    fn main_min(self, c: &ComputedStyle) -> Length {
        if self.vertical {
            c.min_height
        } else {
            c.min_width
        }
    }

    fn main_max(self, c: &ComputedStyle) -> Length {
        if self.vertical {
            c.max_height
        } else {
            c.max_width
        }
    }

    /// The main-axis margins in `(main-start, main-end)` order, already through
    /// the direction's flip — a `row-reverse` item's main-start margin is its
    /// `margin-right`, and a `column-reverse` item's is its `margin-bottom`.
    fn main_margins(self, c: &ComputedStyle) -> (Length, Length) {
        let (start, end) = if self.vertical {
            (c.margin.top, c.margin.bottom)
        } else {
            (c.margin.left, c.margin.right)
        };
        if self.reverse {
            (end, start)
        } else {
            (start, end)
        }
    }

    /// The cross-axis margins in `(cross-start, cross-end)` order, already
    /// through `wrap-reverse`'s flip — under which a row item's cross-start
    /// margin is its `margin-bottom`, so `margin-bottom: auto` is what pushes
    /// it to the top of its line.
    fn cross_margins(self, c: &ComputedStyle) -> (Length, Length) {
        let (start, end) = if self.vertical {
            (c.margin.left, c.margin.right)
        } else {
            (c.margin.top, c.margin.bottom)
        };
        if self.cross_reverse {
            (end, start)
        } else {
            (start, end)
        }
    }

    /// Padding + border on the main axis: what `box-sizing: border-box` counts
    /// as part of a main size.
    fn main_box_edges(self, c: &ComputedStyle) -> i32 {
        if self.vertical {
            self.edge(c.padding.top)
                + self.edge(c.padding.bottom)
                + self.edge(c.border.top)
                + self.edge(c.border.bottom)
        } else {
            self.edge(c.padding.left)
                + self.edge(c.padding.right)
                + self.edge(c.border.left)
                + self.edge(c.border.right)
        }
    }

    /// One main-axis edge — margin, border or padding — in cells; `auto` is
    /// zero. Percentages use the container's *width* whichever axis this is,
    /// which is CSS 2.1's rule and not a shortcut.
    fn edge(self, len: Length) -> i32 {
        if self.vertical {
            edge_v(len, self.width)
        } else {
            edge_h(len, self.width)
        }
    }

    /// A main-axis **size** property as cells, or `None` for "behaves as
    /// `auto`" — which a percentage does when the container's main size is
    /// indefinite.
    ///
    /// The unit rule is the thing that must not get crossed: a width is 8px to
    /// the cell and takes its percentage from the containing width, a height is
    /// 16px to the line and takes its percentage from the containing *height*.
    fn main_definite(self, len: Length) -> Option<i32> {
        if self.vertical {
            definite_v(len, self.main_base)
        } else if len.is_auto() {
            None
        } else {
            Some(len.to_cells_h(self.width))
        }
    }

    /// A main-axis distance that is never `auto`: a gap. An indefinite main
    /// size makes a percentage gap zero, the same thing it does to a percentage
    /// height.
    fn main_distance(self, len: Length) -> i32 {
        self.main_definite(len).unwrap_or(0).max(0)
    }

    /// The gap that falls *between the items on a line*. A gap is named for
    /// what it sits between, so `column-gap` — the gutter between columns — is
    /// a row's main-axis gap and `row-gap` is a column's.
    fn main_gap(self, gaps: Gaps) -> Length {
        if self.vertical { gaps.row } else { gaps.column }
    }

    /// The other one: the gap *between flex lines*, which a `nowrap` container
    /// has nothing to put between and a wrapping one does (M9.10).
    fn cross_gap(self, gaps: Gaps) -> Length {
        if self.vertical { gaps.column } else { gaps.row }
    }

    /// A cross-axis distance that is never `auto`. The mirror of
    /// [`main_distance`](Self::main_distance), unit rule included: a cross axis
    /// that is a width is 8px to the cell and takes its percentage from the
    /// container's width, one that is a height is 16px to the line and takes
    /// its percentage from a definite `height` — or is zero when there is none.
    fn cross_distance(self, len: Length) -> i32 {
        if self.vertical {
            edge_h(len, self.width)
        } else {
            definite_v(len, self.cross_base).unwrap_or(0)
        }
        .max(0)
    }

    /// Whether this item opts out of §4.5's automatic minimum size by clipping
    /// on the main axis.
    fn main_overflow_clips(self, c: &ComputedStyle) -> bool {
        if self.vertical {
            c.overflow_y.clips()
        } else {
            c.overflow_x.clips()
        }
    }
}

/// The vertical size properties `layout_box_at` resolved for one box, handed to
/// its contents.
///
/// A block container reads only `specified` — the definite height its
/// percentage-height children resolve against. A **column** flex container
/// reads all three, because its free space is what is left of its own height
/// after its items, so it has to apply its own `min-height`/`max-height` before
/// §9.7 runs rather than leaving them to the clamp every block gets afterwards.
/// That clamp then re-applies to an already-clamped value and does nothing,
/// which is the point: one rule, applied at one site.
#[derive(Clone, Copy)]
struct BlockHeight {
    /// `height` when definite, already a content-box size.
    specified: Option<i32>,
    /// `min-height` / `max-height` when definite, as *specified* values:
    /// [`Axis::clamp`] is what puts them through `box-sizing`.
    min: Option<i32>,
    max: Option<i32>,
}

enum ChildMode {
    Skip,
    Block,
    Inline,
}

/// One flex item while it is being sized and placed (M9.6).
///
/// `metrics` is everything §9.7 needs and nothing it does not — the algorithm
/// never sees a DOM node — while `source` is how the item becomes boxes once
/// its size is known.
struct FlexItem {
    source: FlexItemSource,
    /// The item's own computed style, or the initial values for an anonymous
    /// item (which has no element to have a style).
    computed: ComputedStyle,
    /// `order`, lifted out so the sort does not have to reach through a style.
    order: i32,
    metrics: flex::Item,
    /// The box, when measuring the item is what built it — a column, always
    /// (M9.9). `None` for a row, whose items are built once §9.5 has said where
    /// they go. Which of the two it is, is the whole of what the direction
    /// changes after §9.2.
    built: Option<BoxId>,
}

/// One item's place on the line, in the physical terms box-building needs:
/// §9.5's main-axis offsets already mapped through the container's direction.
///
/// "Near" and "far" rather than start and end, because those are the ones a
/// box's coordinates are in: the near edge is the item's left in a row and its
/// top in a column, and under a `-reverse` direction it is the main-*end* one.
#[derive(Clone, Copy)]
struct ItemPlacement {
    /// The item's margin box's near edge on the main axis, in the tree's
    /// coordinates: its `x` for a row, its `y` for a column.
    near: i32,
    /// The used main size §9.7 resolved, content-box.
    main_size: i32,
    /// The cells this item's `auto` main-axis margins absorbed, on the near and
    /// far sides — `margin-left`/`margin-right` for a row, `margin-top`/
    /// `margin-bottom` for a column.
    auto_near: i32,
    auto_far: i32,
}

pub(super) enum FlexItemSource {
    Element(NodeId),
    /// A contiguous run of text between two element children: one anonymous
    /// item wrapping one inline formatting context (§4).
    Text(Vec<NodeId>),
}

/// Does this box lay its children out by css-flexbox-1 §9, here and now?
///
/// Since M9.9, `display: flex` is the whole question: all four
/// `flex-direction`s run the algorithm, and the direction only decides which
/// axis is which ([`FlexAxis`]).
///
/// Both the engine and intrinsic sizing ask this. They must agree, or a flex
/// container's measured width and its laid-out width would come from different
/// algorithms. The measuring side does have to know the direction, though, even
/// though it does not read this predicate for it: a row's items sit side by
/// side so its width is a *sum*, while a column's stack so its width is a
/// *max*. Reversal changes neither — reversing the order of a sum or a maximum
/// does not change it.
pub(super) fn lays_out_as_flex(computed: &ComputedStyle) -> bool {
    matches!(computed.display, Display::Flex | Display::InlineFlex)
}

pub(super) fn lays_out_as_grid(computed: &ComputedStyle) -> bool {
    matches!(computed.display, Display::Grid | Display::InlineGrid)
}

fn grid_axis(pair: (GridPlacement, GridPlacement), limit: usize) -> (Option<usize>, usize) {
    let (start, span) = match pair {
        (GridPlacement::Line(a), GridPlacement::Line(b)) if b > a => {
            (Some(a.saturating_sub(1)), b - a)
        }
        (GridPlacement::Line(a), GridPlacement::Span(n)) => (Some(a.saturating_sub(1)), n),
        (GridPlacement::Line(a), _) => (Some(a.saturating_sub(1)), 1),
        (GridPlacement::Span(n), GridPlacement::Line(b)) => {
            (Some(b.saturating_sub(1).saturating_sub(n)), n)
        }
        (GridPlacement::Auto, GridPlacement::Line(b)) => (Some(b.saturating_sub(2)), 1),
        (GridPlacement::Span(n), _) | (_, GridPlacement::Span(n)) => (None, n),
        _ => (None, 1),
    };
    let start = start.map(|n| n.min(limit.saturating_sub(1)));
    let span = span.min(limit.max(1));
    (start, span)
}
fn grid_free(cells: &[Vec<bool>], row: usize, col: usize, rs: usize, cs: usize) -> bool {
    (row..row.saturating_add(rs).min(256)).all(|r| {
        (col..col.saturating_add(cs)).all(|c| {
            !cells
                .get(r)
                .and_then(|x| x.get(c))
                .copied()
                .unwrap_or(false)
        })
    })
}
fn grid_reserve(cells: &mut Vec<Vec<bool>>, row: usize, col: usize, rs: usize, cs: usize) {
    let last = row.saturating_add(rs).min(256);
    while cells.len() < last {
        cells.push(Vec::new());
    }
    for occupied in cells.iter_mut().take(last).skip(row) {
        if occupied.len() < col.saturating_add(cs) {
            occupied.resize(col.saturating_add(cs), false);
        }
        for cell in occupied.iter_mut().take(col.saturating_add(cs)).skip(col) {
            *cell = true;
        }
    }
}
fn grid_offset(tracks: &[i32], at: usize, gap: i32) -> i32 {
    tracks
        .iter()
        .take(at)
        .fold(0i32, |n, x| n.saturating_add(*x).saturating_add(gap))
}
fn grid_span(tracks: &[i32], at: usize, span: usize, gap: i32) -> i32 {
    tracks
        .iter()
        .skip(at)
        .take(span)
        .fold(0i32, |n, x| n.saturating_add(*x))
        .saturating_add(gap.saturating_mul(span.saturating_sub(1) as i32))
}
#[derive(Clone, Copy)]
enum GridTrackAxis {
    Columns { width: i32 },
    Rows { width: i32, height: Option<i32> },
}

impl GridTrackAxis {
    fn available(self) -> i32 {
        match self {
            Self::Columns { width } => width,
            Self::Rows { height, .. } => height.unwrap_or(0),
        }
    }

    fn length(self, value: Length) -> i32 {
        match self {
            Self::Columns { width } => value.to_cells_h(width),
            Self::Rows {
                height: Some(height),
                ..
            } if matches!(value, Length::Percent(_)) => value.to_lines(height),
            Self::Rows { height: None, .. } if matches!(value, Length::Percent(_)) => 0,
            Self::Rows { width, .. } => value.to_cells_v(width),
        }
    }
}

fn grid_row_accepts_content(track: GridTrack, definite_height: bool) -> bool {
    matches!(track, GridTrack::Auto | GridTrack::MinMax(GridMin::Auto, _))
        || (!definite_height
            && matches!(
                track,
                GridTrack::Fr(_)
                    | GridTrack::Fixed(Length::Percent(_))
                    | GridTrack::MinMax(_, GridMax::Fr(_))
            ))
}

fn resolve_grid_tracks(tracks: &[GridTrack], axis: GridTrackAxis, gap: i32) -> Vec<i32> {
    let available = axis.available();
    let free = available
        .saturating_sub(gap.saturating_mul(tracks.len().saturating_sub(1) as i32))
        .max(0);
    tracks
        .iter()
        .map(|t| match t {
            GridTrack::Fixed(l) => axis.length(*l).max(0),
            GridTrack::Auto | GridTrack::Fr(_) => 0,
            GridTrack::MinMax(GridMin::Fixed(l), _) => axis.length(*l).max(0),
            GridTrack::MinMax(_, _) => 0,
        })
        .map(|n| if available > 0 { n.min(free) } else { n })
        .collect()
}
fn fit_grid_tracks(
    tracks: &[GridTrack],
    mut used: Vec<i32>,
    axis: GridTrackAxis,
    gap: i32,
) -> Vec<i32> {
    let available = axis.available();
    let target = available
        .saturating_sub(gap.saturating_mul(used.len().saturating_sub(1) as i32))
        .max(0);
    let sum = used.iter().fold(0i32, |n, x| n.saturating_add(*x));
    let free = target.saturating_sub(sum).max(0);
    let factor: f32 = tracks
        .iter()
        .map(|t| match t {
            GridTrack::Fr(n) => *n,
            GridTrack::MinMax(_, GridMax::Fr(n)) => *n,
            _ => 0.,
        })
        .sum();
    if factor > 0. {
        let mut left = free;
        for (i, t) in tracks.iter().enumerate() {
            let f = match t {
                GridTrack::Fr(n) => *n,
                GridTrack::MinMax(_, GridMax::Fr(n)) => *n,
                _ => 0.,
            };
            let add = if f == 0. {
                0
            } else {
                ((free as f32 * f / factor).round() as i32).min(left)
            };
            used[i] = used[i].saturating_add(add);
            left = left.saturating_sub(add);
        }
    }
    used.into_iter()
        .zip(tracks)
        .map(|(used, track)| match track {
            GridTrack::MinMax(min, GridMax::Fixed(max)) => {
                let floor = match min {
                    GridMin::Fixed(value) => axis.length(*value).max(0),
                    GridMin::Auto => 0,
                };
                used.min(axis.length(*max).max(floor)).max(floor)
            }
            _ => used.max(0),
        })
        .collect()
}

/// Does this flex container's main axis run down the page? Intrinsic sizing
/// needs the answer to know whether to sum its items' widths or take the
/// largest (M9.9); the engine gets it from [`FlexAxis`].
pub(super) fn is_column(computed: &ComputedStyle) -> bool {
    matches!(
        computed.flex_direction,
        FlexDirection::Column | FlexDirection::ColumnReverse
    )
}

/// §4 item generation: which of a flex container's children become items, and
/// which text becomes an anonymous one.
///
/// Every in-flow element child is one item, blockified — an inline child
/// becomes a block-level item, which is why a `<span>` in a flex row gets a box
/// of its own instead of joining a line. Each contiguous run of text between
/// elements becomes one anonymous item; a run that is only whitespace generates
/// nothing, which is what keeps the newlines between two `<div>`s from becoming
/// a third item.
///
/// Shared with intrinsic sizing (M9.4's lesson, applied): a measurement that
/// disagreed with the engine about *what the items are* would be wrong before
/// any arithmetic started.
pub(super) fn flex_sources(
    dom: &Dom,
    container: NodeId,
    hidden: &dyn Fn(NodeId) -> bool,
    pre: bool,
) -> Vec<FlexItemSource> {
    let mut out = Vec::new();
    let mut run: Vec<NodeId> = Vec::new();
    let flush = |run: &mut Vec<NodeId>, out: &mut Vec<FlexItemSource>| {
        let nodes = std::mem::take(run);
        let has_content = nodes.iter().any(|&n| match &dom.node(n).data {
            NodeData::Text(t) => pre || !t.chars().all(is_html_space),
            _ => false,
        });
        if has_content {
            out.push(FlexItemSource::Text(nodes));
        }
    };
    for child in dom.children(container) {
        match &dom.node(child).data {
            // Comments and doctypes are not boxes and not text: they do not
            // interrupt a run either.
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => {}
            NodeData::Text(_) => run.push(child),
            NodeData::Element { .. } => {
                // A hidden child generates nothing, so it cannot be an item —
                // and it does not split the text on either side of it into two,
                // because there is no box between them to do the splitting.
                if hidden(child) {
                    continue;
                }
                flush(&mut run, &mut out);
                out.push(FlexItemSource::Element(child));
            }
        }
    }
    flush(&mut run, &mut out);
    out
}

#[derive(Clone)]
enum InlineItem {
    Text {
        node: NodeId,
        text: String,
        style: Style,
        #[allow(dead_code)]
        computed: ComputedStyle,
    },
    Marker {
        text: String,
        style: Style,
    },
    /// Fixed-width gap from inline horizontal margin/padding/border.
    Spacer {
        cells: i32,
    },
    /// Forced line break from `<br>` nested in an inline.
    Break,
    /// Atomic replaced `<img>` (M8).
    Image {
        node: NodeId,
        url: String,
        alt: String,
        cells_w: i32,
        cells_h: i32,
        firm: bool,
        computed: ComputedStyle,
    },
    /// An atomic inline: `inline-block` or `inline-flex` (M9.11). Only the node
    /// travels, because nothing about the box can be decided here — its width
    /// depends on how much of the line is left when the breaker reaches it.
    Atomic {
        node: NodeId,
    },
}

/// Intermediate fragment while building an IFC (text pieces + atomic images).
///
/// The `Image` variant is the wide one: it carries a whole `ComputedStyle`
/// (204 bytes since M9.5 put flexbox's vocabulary in there), against 48 for a
/// text piece. Boxing it, which is what the lint asks for, would trade a
/// transient per-IFC `Vec` — freed as soon as the block's lines are built —
/// for a heap allocation on every inline image. Not worth it at this size;
/// worth revisiting if `ComputedStyle` keeps growing.
#[allow(clippy::large_enum_variant)]
enum InlineFrag {
    Piece(Piece),
    Image {
        node: NodeId,
        url: String,
        alt: String,
        cells_w: i32,
        cells_h: i32,
        firm: bool,
        computed: ComputedStyle,
    },
    /// An atomic inline, still unsized: it becomes a [`Piece`] only once the
    /// breaker knows how much room is left on the line (M9.11).
    Atomic {
        node: NodeId,
    },
}

/// One unit the line breaker places: it fits on the current line, or the line
/// breaks before it.
struct Piece {
    text: String,
    /// Cells this piece occupies on the line — for an atomic inline, its whole
    /// *margin* box, which is what the next piece has to start after.
    cells: i32,
    style: Style,
    node: Option<NodeId>,
    kind: PieceKind,
}

/// What a piece is. Three of the four are text the line draws itself; the
/// fourth is a box that was built before the line existed.
#[derive(Clone, Copy)]
enum PieceKind {
    /// A word, a list marker, or a fixed-width spacer from an inline's
    /// horizontal edges: drawn as text, never split except by the overlong-word
    /// path.
    Word,
    /// Collapsible HTML whitespace: dropped at either end of a line.
    Space,
    /// A forced break (`<br>`).
    Break,
    /// An atomic inline (M9.11): a box already built and sized, which the line
    /// places whole and never breaks into. How many rows it needs is read back
    /// off the box ([`Engine::atomic_rows`]) rather than carried here — a
    /// second copy could disagree with the box, and this struct is the inner
    /// loop of every inline formatting context on the page, so it stays the
    /// size it was before atomic inlines existed.
    Atomic(BoxId),
}

impl Piece {
    fn word(text: String, cells: i32, style: Style, node: Option<NodeId>) -> Piece {
        Piece {
            text,
            cells,
            style,
            node,
            kind: PieceKind::Word,
        }
    }

    fn space(style: Style) -> Piece {
        Piece {
            text: " ".into(),
            cells: 1,
            style,
            node: None,
            kind: PieceKind::Space,
        }
    }

    fn line_break() -> Piece {
        Piece {
            text: String::new(),
            cells: 0,
            style: Style::default(),
            node: None,
            kind: PieceKind::Break,
        }
    }

    /// `node` stays `None`: the box carries the DOM node, and a piece's own
    /// node is only ever read to build a text box or to merge one, neither of
    /// which an atomic does.
    fn atomic(box_id: BoxId, cells: i32) -> Piece {
        Piece {
            text: String::new(),
            cells,
            style: Style::default(),
            node: None,
            kind: PieceKind::Atomic(box_id),
        }
    }

    fn is_space(&self) -> bool {
        matches!(self.kind, PieceKind::Space)
    }

    fn is_atomic(&self) -> bool {
        matches!(self.kind, PieceKind::Atomic(_))
    }
}

/// Resolve horizontal box model for a block in a containing block of width `cw`.
/// One axis of CSS 2.1 §10.4/§10.7 sizing. Width and height share it so the
/// clamp order and the `box-sizing` arithmetic cannot drift apart between the
/// two axes — the bug that makes `min-width` and `min-height` disagree.
#[derive(Clone, Copy)]
pub(super) struct Axis {
    /// Padding + border on this axis, in cells: what `border-box` counts as
    /// part of the specified size and `content-box` does not.
    pub(super) edges: i32,
    pub(super) box_sizing: BoxSizing,
}

impl Axis {
    /// A specified value (already in cells) as a **content-box** size.
    /// Degenerate `border-box` boxes — padding and border wider than the
    /// declared width — floor at zero rather than going negative.
    pub(super) fn content_from(self, specified: i32) -> i32 {
        match self.box_sizing {
            BoxSizing::ContentBox => specified.max(0),
            BoxSizing::BorderBox => (specified - self.edges).max(0),
        }
    }

    /// CSS 2.1 §10.4: clamp by `max`, then by `min`. The order is the rule —
    /// applying `min` last is what makes it win a conflict with a smaller
    /// `max`, which is the behaviour pages rely on.
    pub(super) fn clamp(self, size: i32, min: Option<i32>, max: Option<i32>) -> i32 {
        let mut size = size;
        if let Some(max) = max {
            size = size.min(self.content_from(max));
        }
        if let Some(min) = min {
            size = size.max(self.content_from(min));
        }
        size.max(0)
    }
}

/// A vertical size property as a number layout can use, or `None` for "behave
/// as `auto`".
///
/// A percentage needs a containing block whose height is **definite** — one
/// whose own used height came from a specified length, transitively. The page
/// column is not: it scrolls, so its height is whatever the content turns out
/// to be. That is why `height: 100%` on a top-level element means "as tall as
/// my content" here and not "as tall as the terminal".
fn definite_v(len: Length, containing_height: Option<i32>) -> Option<i32> {
    match len {
        Length::Auto => None,
        Length::Percent(_) => containing_height.map(|h| len.to_lines(h)),
        other => Some(other.to_lines(0)),
    }
}

/// The edges and used content width of a block-level box.
///
/// `intrinsic` is how wide the box is when its `width` is `auto` and it is a
/// **replaced** one — a form control's `size` characters (M11.8). `None` is the
/// ordinary rule, "fill what the containing block leaves"; the clamps and
/// `box-sizing` below apply to either answer, which is the reason this is a
/// parameter rather than an overwrite at the call site.
fn resolve_block_dims(
    computed: &ComputedStyle,
    containing_width: i32,
    intrinsic: Option<i32>,
) -> Dimensions {
    let pad = EdgeSizes {
        top: computed.padding.top.to_cells_v(containing_width),
        right: computed.padding.right.to_cells_h(containing_width),
        bottom: computed.padding.bottom.to_cells_v(containing_width),
        left: computed.padding.left.to_cells_h(containing_width),
    };
    let border = EdgeSizes {
        top: computed.border.top.to_cells_v(containing_width),
        right: computed.border.right.to_cells_h(containing_width),
        bottom: computed.border.bottom.to_cells_v(containing_width),
        left: computed.border.left.to_cells_h(containing_width),
    };
    let mut margin = EdgeSizes {
        top: if computed.margin.top.is_auto() {
            0
        } else {
            computed.margin.top.to_cells_v(containing_width)
        },
        right: if computed.margin.right.is_auto() {
            0
        } else {
            computed.margin.right.to_cells_h(containing_width)
        },
        bottom: if computed.margin.bottom.is_auto() {
            0
        } else {
            computed.margin.bottom.to_cells_v(containing_width)
        },
        left: if computed.margin.left.is_auto() {
            0
        } else {
            computed.margin.left.to_cells_h(containing_width)
        },
    };

    let under =
        |w: i32| w + pad.left + pad.right + border.left + border.right + margin.left + margin.right;

    // Content width (CSS 2.1 §10.4): the specified width, then max, then min.
    let axis = Axis {
        edges: pad.left + pad.right + border.left + border.right,
        box_sizing: computed.box_sizing,
    };
    let tentative = if computed.width.is_auto() {
        match intrinsic {
            // A replaced box is as wide as it is, wherever it lands.
            Some(cells) => cells.max(0),
            // Fill available: content = containing - margin - border - padding.
            // Already a content-box size whatever `box-sizing` says — `auto`
            // sizes the margin box to the containing block either way.
            None => (containing_width
                - margin.left
                - margin.right
                - border.left
                - border.right
                - pad.left
                - pad.right)
                .max(0),
        }
    } else {
        axis.content_from(computed.width.to_cells_h(containing_width))
    };
    let resolve_h = |len: Length| (!len.is_auto()).then(|| len.to_cells_h(containing_width));
    let width = axis.clamp(
        tentative,
        resolve_h(computed.min_width),
        resolve_h(computed.max_width),
    );

    // Auto margins centre against the *final* width, clamps included — an
    // element narrowed by `max-width` or widened by `min-width` still sits in
    // the middle of what is left.
    let used = under(width);
    if used < containing_width {
        let free = containing_width - used;
        let left_auto = computed.margin.left.is_auto();
        let right_auto = computed.margin.right.is_auto();
        match (left_auto, right_auto) {
            (true, true) => {
                margin.left = free / 2;
                margin.right = free - margin.left;
            }
            (true, false) => margin.left = free,
            (false, true) => margin.right = free,
            (false, false) => {}
        }
    }

    Dimensions {
        content: Rect {
            x: 0,
            y: 0,
            width,
            height: 0,
        },
        padding: pad,
        border,
        margin,
    }
}

pub fn term_style(computed: &ComputedStyle) -> Style {
    let mut attrs = Attrs::NONE;
    if computed.font_weight == FontWeight::Bold {
        attrs = attrs | Attrs::BOLD;
    }
    if computed.font_style == FontStyle::Italic {
        attrs = attrs | Attrs::ITALIC;
    }
    if computed.underline {
        attrs = attrs | Attrs::UNDERLINE;
    }
    Style {
        fg: term_color(computed.color),
        bg: Color::Default,
        attrs,
    }
}

pub fn term_color(color: crate::style::values::ColorValue) -> Color {
    use crate::style::values::ColorValue;
    const TOO_DARK: f32 = 0.20;
    const TOO_LIGHT: f32 = 0.85;
    match color {
        ColorValue::Default => Color::Default,
        ColorValue::Rgb(r, g, b) => {
            let luma =
                (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
            if !(TOO_DARK..=TOO_LIGHT).contains(&luma) {
                Color::Default
            } else {
                Color::Rgb(r, g, b)
            }
        }
    }
}

/// Does this `display` generate a block-level box — one that stacks, rather
/// than joining the line beside it?
///
/// `Display::Flex` answers yes for the same reason `Display::Block` does: a
/// flex container is an ordinary block-level box *from the outside*, and this
/// question is only about the outside. What it does with its children —
/// css-flexbox-1 §9 rather than a stack of blocks — is
/// [`lays_out_as_flex`]'s, and the two are deliberately separate: it is what
/// lets a flex container be a block's child, and a block a flex item, with no
/// second code path on either side.
pub(super) fn is_block_level(display: Display) -> bool {
    matches!(display, Display::Block | Display::Flex | Display::Grid)
}

/// The bounded local state of one no-span table column. It is intentionally
/// not stored on layout boxes: final cell geometry is all later stages need.
#[derive(Clone, Copy, Debug)]
struct TableColumn {
    min: i32,
    max: i32,
    used: i32,
}

#[derive(Clone, Copy, Debug)]
struct TablePlacement {
    cell: NodeId,
    row: usize,
    col: usize,
    rowspan: usize,
    colspan: usize,
}

struct TableGrid<'a> {
    placements: &'a [TablePlacement],
    rows: &'a [(NodeId, Vec<NodeId>)],
    widths: &'a [i32],
    row_y: &'a [i32],
    row_heights: &'a [i32],
}

/// HTML spans are positive base-ten integers only. The caller supplies the
/// already finite space left in its grid, so parsing cannot cause an oversized
/// multiplication, index, or allocation.
fn table_span(raw: Option<&str>, limit: usize) -> usize {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|&value| value > 0)
        .map(|value| (value.min(limit as u64)) as usize)
        .filter(|&value| value > 0)
        .unwrap_or(1)
}

fn raise_track_sum(columns: &mut [TableColumn], start: usize, span: usize, wanted: i32, min: bool) {
    let end = start.saturating_add(span).min(columns.len());
    let have = columns[start..end].iter().fold(0i32, |sum, c| {
        sum.saturating_add(if min { c.min } else { c.max })
    });
    let deficit = wanted.saturating_sub(have);
    let count = (end - start) as i32;
    if deficit == 0 || count == 0 {
        return;
    }
    let each = deficit / count;
    let remainder = deficit % count;
    for (offset, column) in columns[start..end].iter_mut().enumerate() {
        let add = each.saturating_add((offset < remainder as usize) as i32);
        if min {
            column.min = column.min.saturating_add(add);
            column.max = column.max.max(column.min);
        } else {
            column.max = column.max.saturating_add(add).max(column.min);
        }
    }
}

fn raise_row_sum(rows: &mut [i32], start: usize, span: usize, wanted: i32) {
    let end = start.saturating_add(span).min(rows.len());
    let have = rows[start..end]
        .iter()
        .fold(0i32, |sum, row| sum.saturating_add(*row));
    let deficit = wanted.saturating_sub(have);
    let count = (end - start) as i32;
    if deficit == 0 || count == 0 {
        return;
    }
    let each = deficit / count;
    let remainder = deficit % count;
    for (offset, row) in rows[start..end].iter_mut().enumerate() {
        *row = row
            .saturating_add(each)
            .saturating_add((offset < remainder as usize) as i32);
    }
}

impl TableColumn {
    const EMPTY: TableColumn = TableColumn {
        min: 1,
        max: 1,
        used: 1,
    };

    fn normalized(self) -> TableColumn {
        let min = self.min.max(1);
        TableColumn {
            min,
            max: self.max.max(min),
            used: self.used.max(min),
        }
    }
}

/// Resolve no-span automatic-table columns. The integer quotient is assigned
/// first, then any rounding cells go to earlier document columns, which makes
/// repeated layout byte-identical.
fn fit_table_columns(columns: &[TableColumn], requested: i32) -> Vec<i32> {
    let mut columns: Vec<TableColumn> = columns
        .iter()
        .copied()
        .map(TableColumn::normalized)
        .collect();
    let sum = |f: fn(TableColumn) -> i32| {
        columns
            .iter()
            .fold(0i32, |total, column| total.saturating_add(f(*column)))
    };
    let min_table = sum(|column| column.min);
    let max_table = sum(|column| column.max);
    let target = requested.max(min_table);

    if target >= max_table {
        let surplus = target.saturating_sub(max_table);
        let count = columns.len() as i32;
        let each = surplus / count;
        let remainder = surplus % count;
        for (index, column) in columns.iter_mut().enumerate() {
            column.used = column
                .max
                .saturating_add(each)
                .saturating_add((index < remainder as usize) as i32);
        }
    } else {
        let free = target.saturating_sub(min_table);
        let range = max_table.saturating_sub(min_table);
        let mut used_total = 0i32;
        for column in &mut columns {
            let expandable = column.max.saturating_sub(column.min);
            let share = ((i64::from(expandable) * i64::from(free)) / i64::from(range)) as i32;
            column.used = column.min.saturating_add(share);
            used_total = used_total.saturating_add(column.used);
        }
        let mut remainder = target.saturating_sub(used_total);
        for column in &mut columns {
            if remainder == 0 {
                break;
            }
            if column.used < column.max {
                column.used += 1;
                remainder -= 1;
            }
        }
    }
    columns.into_iter().map(|column| column.used).collect()
}

/// Does this `display` generate an **atomic inline** — a box that joins the
/// line beside it, sized and placed as one unbreakable piece rather than
/// flowing its contents into that line as words (M9.11)?
///
/// This is the other half of the keyword [`is_block_level`] reads, and the two
/// are exhaustive over the box-generating modes: a box is block-level, atomic
/// inline, or plain inline. What runs *inside* an atomic one is still
/// [`lays_out_as_flex`]'s question — that separation is what lets
/// `inline-block` and `inline-flex` share every line of placement code and
/// differ only in which formatting context fills the box.
pub(super) fn is_atomic_inline(display: Display) -> bool {
    matches!(
        display,
        Display::InlineBlock | Display::InlineFlex | Display::InlineGrid
    )
}

/// Where a line may be broken: HTML whitespace, and nothing else.
///
/// The line breaker splits text runs on this predicate, so it is also what
/// decides a run's break opportunities — which is why intrinsic sizing (M9.4)
/// measures with it rather than its own idea of whitespace. If the two
/// disagreed, a flex item's min-content width would be a width its own text
/// cannot actually wrap into, and the boxes flex computes would not match what
/// gets painted.
pub(super) fn is_html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

#[cfg(test)]
mod table_tests {
    use super::{TableColumn, fit_table_columns, raise_row_sum, raise_track_sum, table_span};

    fn columns(bounds: &[(i32, i32)]) -> Vec<TableColumn> {
        bounds
            .iter()
            .map(|&(min, max)| TableColumn {
                min,
                max,
                used: min,
            })
            .collect()
    }

    #[test]
    fn columns_spend_width_between_intrinsic_bounds_in_document_order() {
        let bounds = columns(&[(2, 2), (1, 1), (5, 17)]);
        assert_eq!(fit_table_columns(&bounds, 20), vec![2, 1, 17]);
        // Six free cells over unequal ranges leaves one rounding cell, and it
        // belongs to the first expandable document column.
        assert_eq!(
            fit_table_columns(&columns(&[(2, 5), (1, 6)]), 8),
            vec![4, 4]
        );
    }

    #[test]
    fn columns_keep_minima_and_share_explicit_surplus() {
        let bounds = columns(&[(2, 2), (1, 1), (5, 8)]);
        assert_eq!(fit_table_columns(&bounds, 1), vec![2, 1, 5]);
        assert_eq!(fit_table_columns(&bounds, 17), vec![4, 3, 10]);
    }

    #[test]
    fn spans_are_positive_bounded_integers_and_deficits_keep_document_order() {
        assert_eq!(table_span(None, 4), 1);
        assert_eq!(table_span(Some("0"), 4), 1);
        assert_eq!(table_span(Some("-2"), 4), 1);
        assert_eq!(table_span(Some("wat"), 4), 1);
        assert_eq!(table_span(Some("999999999999999999999"), 4), 1);
        assert_eq!(table_span(Some("9"), 4), 4);
        let mut tracks = columns(&[(1, 1), (1, 1), (1, 1)]);
        raise_track_sum(&mut tracks, 0, 3, 8, true);
        assert_eq!(
            tracks.iter().map(|t| t.min).collect::<Vec<_>>(),
            vec![3, 3, 2]
        );
        let mut rows = vec![1, 1];
        raise_row_sum(&mut rows, 0, 2, 5);
        assert_eq!(rows, vec![3, 2]);
    }
}
