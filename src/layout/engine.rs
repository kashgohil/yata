//! Layout engine: DOM + styles + width → positioned box tree (PLAN.md M5).
//!
//! Pure transform. Block boxes stack vertically with margin collapse between
//! adjacent siblings; inline content fills line boxes with unicode-width
//! wrapping. Unit conversion lives on `Length` (M5.1).

use crate::dom::{Dom, NodeData, NodeId};
use crate::image::ImageContext;
use crate::layout::boxes::{BoxId, BoxKind, LayoutBox, LayoutTree};
use crate::layout::dimensions::{Dimensions, EdgeSizes, Rect};
use crate::layout::flex;
use crate::layout::intrinsic::IntrinsicSizer;
use crate::style::values::{
    AlignItems, BoxSizing, Display, FlexBasis, FlexDirection, FontStyle, FontWeight, Gaps, Length,
    TextAlign,
};
use crate::style::{ComputedStyle, Styles};
use crate::term::{Attrs, Color, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    let width = width.max(1) as i32;
    let mut eng = Engine {
        dom,
        styles,
        hidden,
        images,
        boxes: Vec::new(),
        sizer: IntrinsicSizer::new(dom, styles, images, hidden),
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
    });
    let mut y = 0i32;
    let mut prev_mb = 0i32;
    for child in dom.children(dom.root) {
        // `None` height: the page column scrolls, so it has no definite height
        // for a percentage to resolve against. `height: 100%` on a top-level
        // element therefore means "as tall as my content", never "as tall as
        // the terminal" (CSS 2.1 §10.5, and M9.2's definiteness rule).
        if let Some(id) = eng.layout_node(child, 0, width, None, y, &mut prev_mb, false) {
            eng.boxes[root.0 as usize].children.push(id);
            let mb = eng.boxes[id.0 as usize].dimensions.margin_box();
            y = mb.bottom();
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
    let mut tree = LayoutTree {
        boxes: eng.boxes,
        root,
        width,
        height: 0,
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
    tree
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
                        computed: *self.styles.get(id),
                    }],
                    TextAlign::Left,
                    pre,
                )
            }
            NodeData::Element { tag, .. } => {
                if self.is_hidden(id) {
                    return None;
                }
                let mut computed = *self.styles.get(id);
                // Reveal pass: a page's own `display:none` is treated as block so
                // its content can be read. UA-important hiding never reaches here
                // (`is_hidden` still catches it).
                if computed.display == Display::None && self.hidden == Hidden::Reveal {
                    computed.display = Display::Block;
                }
                match tag.as_str() {
                    "br" => self.layout_br(x, containing_width, y, prev_margin_bottom, computed),
                    "hr" => self.layout_hr(x, containing_width, y, prev_margin_bottom, computed),
                    "img" => self.layout_img_block(
                        id,
                        computed,
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
                                computed,
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
                }
            }
        }
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
        let mut dims = resolve_block_dims(&computed, containing_width);
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
        let box_id = self.alloc(LayoutBox {
            kind: if lays_out_as_flex(&computed) {
                BoxKind::Flex
            } else {
                BoxKind::Block
            },
            node: Some(id),
            dimensions: dims,
            children: Vec::new(),
            text: None,
            term_style: Style::default(),
            computed,
            image_src: None,
            image_size_firm: false,
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
        let specified_height =
            definite_v(computed.height, containing_height).map(|h| v_axis.content_from(h));

        let auto_height = self.layout_contents(id, tag, computed, box_id, specified_height, pre);

        // Used height: the specified one if there is one, else the content's
        // (an empty div is zero rows), then the min/max clamps. Children keep
        // the boxes and positions they were given, so a box shorter than its
        // content lets that content overflow and paint past the bottom edge —
        // `overflow: visible` is the initial value, and clipping is M9.3.
        // The flow advances by *this* height, which is what makes `height: 0`
        // collapse a box whose children still exist.
        let content_height = v_axis.clamp(
            specified_height.unwrap_or(auto_height),
            definite_v(computed.min_height, containing_height),
            definite_v(computed.max_height, containing_height),
        );
        self.boxes[box_id.0 as usize].dimensions.content.height = content_height;
        box_id
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
        specified_height: Option<i32>,
        pre: bool,
    ) -> i32 {
        if lays_out_as_flex(&computed) {
            return self.layout_flex_contents(id, computed, box_id, specified_height, pre);
        }
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
        for child in child_ids {
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
                        content_y = mb.bottom();
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

        self.boxes[box_id.0 as usize].children = children;
        (content_y - dims.content.y).max(0)
    }

    /// A flex container's contents: css-flexbox-1 §4 (what the items are),
    /// §9.2 (how big each one wants to be), §9.7 (how the line's space is
    /// divided) and §9.5 (where they go). Returns the container's used content
    /// height, which for a single row is its tallest item.
    ///
    /// Scope, M9.9 so far: `flex-direction: row` and `row-reverse`, `flex-wrap:
    /// nowrap`, both axes — the same directions M9.8 laid out, now expressed in
    /// main/cross terms rather than in `x`/`y` ones. The column directions are
    /// the rest of M9.9 and M9.10 brings wrapping.
    ///
    /// **On the main axis, alignment places and never moves.** Every item is
    /// positioned before its contents are laid out, so a centred item's text is
    /// built at the centred `x` and no subtree has to be translated afterwards:
    /// size the whole line, place the whole line, *then* build the boxes.
    ///
    /// The cross axis cannot be written that way, because a line's height is
    /// not known until its tallest item has been laid out. So it runs last, in
    /// [`align_cross`](Self::align_cross), and it does move boxes — with their
    /// whole subtrees, which is where the classic flex bug (a box that moved
    /// and left its text behind) would live if it lived anywhere here.
    fn layout_flex_contents(
        &mut self,
        id: NodeId,
        computed: ComputedStyle,
        box_id: BoxId,
        specified_height: Option<i32>,
        pre: bool,
    ) -> i32 {
        let content = self.boxes[box_id.0 as usize].dimensions.content;
        let axis = FlexAxis::of(computed.flex_direction, content.width, specified_height);

        let mut items = self.flex_items(id, &axis, pre);
        if items.is_empty() {
            return 0;
        }
        // Order-modified document order (§5.4). Stable, so items that share an
        // `order` keep the order the document gave them. Only the layout tree
        // is reordered: the DOM is untouched, so F1, `/` search and hit-testing
        // still see the document as written — which is what CSS says too.
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

        // §9.2 step 2: the container's inner main size, the number §9.7
        // divides. For a row that is the content width it already has —
        // resolved as any block's is, clamps and `box-sizing` included (M9.2),
        // because a flex container is a perfectly ordinary block-level box from
        // the outside.
        let inner_main = content.width;

        // §9.3 line collection: `nowrap`, so every item is on one line
        // whatever it costs. M9.10 is where that stops being true.
        let sizes = flex::resolve(
            &items.iter().map(|i| i.metrics).collect::<Vec<_>>(),
            inner_main,
            total_gap,
        );

        // The axis flip, and the whole of what a `-reverse` direction costs.
        // Main-start is the container's far edge — its right for `row-reverse`,
        // its bottom for `column-reverse` — so an offset from main-start is
        // subtracted instead of added, and an item's main-start margin is the
        // one on the other side. Everything else — §9.7 above, §9.5 below — is
        // written in main-axis terms and does not know which way the axis
        // points, or even which axis it is.
        let slots: Vec<flex::Slot> = items
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

        // §9.5: hand out what §9.7 could not give away — auto margins first,
        // then `justify-content`.
        let placed = flex::place(&slots, gap, inner_main, computed.justify_content);

        // The container's main-start content edge in tree coordinates, and the
        // far edge a reversed direction counts back from.
        let main_origin = content.x;
        let main_far = main_origin.saturating_add(inner_main);

        let mut children = Vec::with_capacity(items.len());
        for (idx, (item, &main_size)) in items.iter().zip(&sizes).enumerate() {
            let p = placed[idx];
            let outer_main = slots[idx]
                .outer
                .saturating_add(p.auto_start)
                .saturating_add(p.auto_end);
            // Main-axis offset → the physical near edge of the item's margin
            // box: added to the container's near edge, or subtracted from its
            // far one when the axis is reversed. Saturating, because an offset
            // can be enormous — `gap: 1e11em` is a legal thing for a stylesheet
            // to say, and an item shoved off the page is what it asks for,
            // where an overflowing add would be a panic a page could trigger.
            let near = if axis.reverse {
                main_far
                    .saturating_sub(p.main_start)
                    .saturating_sub(outer_main)
            } else {
                main_origin.saturating_add(p.main_start)
            };
            // The auto-margin shares, named for the sides they are painted on
            // rather than for the ends of the main axis.
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
            let child = self.layout_flex_item(
                item,
                place,
                content,
                computed.text_align,
                specified_height,
                pre,
            );
            children.push(child);
        }

        // §9.4 and §9.6: the cross axis, once every item's box exists. It has
        // to be last, and that is a fact about the axis rather than a choice:
        // an item's main size is a number the algorithm computes, but its
        // cross size is *its content's* height, which nothing knows until the
        // content has been laid out.
        let line_cross =
            self.align_cross(&items, &children, &computed, content.y, specified_height);
        self.boxes[box_id.0 as usize].children = children;
        // A single line's cross size is the row's content height. The
        // container's own specified height is not consulted here:
        // `layout_box_at` applies it and the min/max clamps afterwards, exactly
        // as it does for a block.
        line_cross
    }

    /// §9.4 (*cross sizing*) and §9.6 (*cross-axis alignment*) for the one line
    /// a `nowrap` container has: size the line, then move each item into its
    /// place on it.
    ///
    /// Returns the line's cross size.
    ///
    /// **This is the one stage that moves boxes after building them**, and it
    /// is the reason [`shift_subtree`](Self::shift_subtree) exists. The main
    /// axis can place before it builds because it knows every item's width
    /// first; the cross axis cannot know the line's height until the tallest
    /// item has been laid out, and by then the shortest one already has a box.
    /// So an item is built at the line's cross-start edge and then moved down —
    /// *with everything inside it*, which is the whole content of the promise
    /// M9.6 made about text never being left behind.
    fn align_cross(
        &mut self,
        items: &[FlexItem],
        boxes: &[BoxId],
        container: &ComputedStyle,
        // The container's cross-start *content* edge: items are positioned
        // inside its padding and border, never against its border box.
        content_y: i32,
        definite_cross: Option<i32>,
    ) -> i32 {
        let cross_items: Vec<flex::CrossItem> = items
            .iter()
            .zip(boxes)
            .map(|(item, &b)| {
                let c = &item.computed;
                let align = c.align_self.resolve(container.align_items);
                let dims = self.boxes[b.0 as usize].dimensions;
                flex::CrossItem {
                    outer: dims.margin_box().height,
                    // Measured only for the items that will be aligned by it:
                    // finding a baseline means walking the item's subtree for
                    // its first line box, and a row of `align-items: stretch`
                    // cards has no use for the answer.
                    baseline: if align == AlignItems::Baseline {
                        self.item_baseline(b)
                    } else {
                        0
                    },
                    align,
                    auto_start: c.margin.top.is_auto(),
                    auto_end: c.margin.bottom.is_auto(),
                }
            })
            .collect();

        // §9.4 step 7: a container with a definite inner cross size hands it to
        // its line, whatever is on it — that is what makes `align-items:
        // center` inside a `height: 10em` container centre in ten rows rather
        // than in the tallest item. Otherwise the line is as tall as its
        // contents (step 8).
        let line_cross = definite_cross.unwrap_or_else(|| flex::cross_size(&cross_items));
        let placed = flex::cross_place(&cross_items, line_cross);

        for ((item, &b), (p, ci)) in items.iter().zip(boxes).zip(placed.iter().zip(&cross_items)) {
            let c = &item.computed;
            if ci.align == AlignItems::Stretch {
                self.stretch_item(b, c, line_cross, definite_cross);
            }
            let dims = self.boxes[b.0 as usize].dimensions;
            // An `auto` cross margin's share is part of the item's margin box,
            // not just of the line's arithmetic — the same rule §9.5's auto
            // margins follow on the main axis, so that the boxes on a line
            // still tile it exactly.
            let margin_top = if c.margin.top.is_auto() {
                p.auto_start
            } else {
                dims.margin.top
            };
            let top = content_y + p.cross_start + margin_top + dims.border.top + dims.padding.top;
            self.shift_subtree(b, top - dims.content.y);
            let dims = &mut self.boxes[b.0 as usize].dimensions;
            if c.margin.top.is_auto() {
                dims.margin.top = p.auto_start;
            }
            if c.margin.bottom.is_auto() {
                dims.margin.bottom = p.auto_end;
            }
        }
        line_cross
    }

    /// §9.4 step 11: an item with `align-self: stretch` fills its line's cross
    /// size — *if* its own cross size is `auto` and neither cross margin is,
    /// since an item that stated a height or claimed the free space with an
    /// auto margin has already answered the question.
    ///
    /// Only the box grows. Its contents are not laid out again, so this is a
    /// field write rather than a second layout pass: what changes is how far
    /// the item's background and borders reach, which is what pages use
    /// `stretch` for (equal-height cards). Content that genuinely wants the new
    /// height — a nested `height: 100%` — needs the definite-size plumbing
    /// M9.9 brings, and is left to it.
    fn stretch_item(
        &mut self,
        b: BoxId,
        c: &ComputedStyle,
        line_cross: i32,
        definite_cross: Option<i32>,
    ) {
        // A replaced box's cross size came from the image, not from `height:
        // auto`, so it is not the `auto` the spec's condition is about.
        // Stretching one would rescale the picture to fill the row.
        if self.boxes[b.0 as usize].kind == BoxKind::Image
            || !c.height.is_auto()
            || c.margin.top.is_auto()
            || c.margin.bottom.is_auto()
        {
            return;
        }
        let dims = self.boxes[b.0 as usize].dimensions;
        let v_axis = Axis {
            edges: dims.padding.top + dims.padding.bottom + dims.border.top + dims.border.bottom,
            box_sizing: c.box_sizing,
        };
        let target = line_cross - (v_axis.edges + dims.margin.top + dims.margin.bottom);
        // §4.5 on the cross axis: `min-height: auto` on a flex item is its
        // *content* height, so an item stretched into a line shorter than its
        // own text is never squeezed until it clips. That content height is
        // exactly the height the box has right now — `height` is `auto` here,
        // so it is what the contents used.
        let auto_min = dims.content.height;
        let min =
            definite_v(c.min_height, definite_cross).map_or(auto_min, |h| v_axis.content_from(h));
        let max = definite_v(c.max_height, definite_cross).map(|h| v_axis.content_from(h));
        // M9.2's clamp order, max before min, so a minimum bigger than the
        // maximum wins — the automatic one included.
        let used = max.map_or(target, |m| target.min(m)).max(min).max(0);
        self.boxes[b.0 as usize].dimensions.content.height = used;
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

    /// The row of the first line box in this subtree, in paint order — which
    /// for a box that contains text is the row its first line of text is on.
    fn first_line_row(&self, b: BoxId) -> Option<i32> {
        let bx = &self.boxes[b.0 as usize];
        if bx.kind == BoxKind::Line {
            return Some(bx.dimensions.content.y);
        }
        bx.children.iter().find_map(|&c| self.first_line_row(c))
    }

    /// Move a box and everything under it `dy` rows down the page.
    ///
    /// Every rectangle in the tree is absolute, so a subtree moves by adding
    /// the same offset to every box in it — no relative coordinates to keep in
    /// step, and nothing outside the subtree to update. Edges are unaffected:
    /// a margin is a width, not a position.
    fn shift_subtree(&mut self, b: BoxId, dy: i32) {
        if dy == 0 {
            return;
        }
        self.boxes[b.0 as usize].dimensions.content.y += dy;
        // By index, not by iterator: the loop needs `&mut self` for the
        // recursion, and the child list cannot be borrowed across it.
        for i in 0..self.boxes[b.0 as usize].children.len() {
            let child = self.boxes[b.0 as usize].children[i];
            self.shift_subtree(child, dy);
        }
    }

    /// Build one item's box where §9.5 placed it, at the main size §9.7
    /// resolved for it.
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
                let mut dims = resolve_block_dims(&item.computed, container.width);
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
                self.layout_box_at(node, &tag, item.computed, dims, containing_height, pre)
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
    fn flex_items(&mut self, container: NodeId, axis: &FlexAxis, pre: bool) -> Vec<FlexItem> {
        flex_sources(self.dom, container, &|n| self.is_hidden(n), pre)
            .into_iter()
            .map(|source| self.row_item(source, axis))
            .collect()
    }

    /// One item of a **row**: measured, not built. Its main size is a width, so
    /// `intrinsic` can answer without laying anything out, and the box is built
    /// later at the position §9.5 chose for it.
    fn row_item(&mut self, source: FlexItemSource, axis: &FlexAxis) -> FlexItem {
        match source {
            FlexItemSource::Element(node) => {
                let c = *self.styles.get(node);
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
                    computed: c,
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
                    computed: c,
                    order: 0,
                    metrics: item_metrics(&c, axis, content),
                }
            }
        }
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
        let mut dims = resolve_block_dims(&computed, width);
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
                    frags.push(InlineFrag::Piece(Piece {
                        text: text.clone(),
                        cells: text.width() as i32,
                        style: *style,
                        node: None,
                        is_space: false,
                        is_break: false,
                    }));
                }
                InlineItem::Spacer { cells } => {
                    if *cells > 0 {
                        frags.push(InlineFrag::Piece(Piece {
                            text: " ".repeat(*cells as usize),
                            cells: *cells,
                            style: Style::default(),
                            node: None,
                            // Not a soft wrap space: margins must not collapse
                            // or vanish at a line edge the way HTML whitespace does.
                            is_space: false,
                            is_break: false,
                        }));
                    }
                }
                InlineItem::Break => {
                    pending_space = None;
                    frags.push(InlineFrag::Piece(Piece {
                        text: String::new(),
                        cells: 0,
                        style: Style::default(),
                        node: None,
                        is_space: false,
                        is_break: true,
                    }));
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
                        computed: *computed,
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
                            frags.push(InlineFrag::Piece(Piece {
                                text: " ".into(),
                                cells: 1,
                                style: pending_space.unwrap_or(*style),
                                node: None,
                                is_space: true,
                                is_break: false,
                            }));
                        }
                        first = false;
                        pending_space = None;
                        frags.push(InlineFrag::Piece(Piece {
                            text: word.to_string(),
                            cells: word.width() as i32,
                            style: *style,
                            node: Some(*node),
                            is_space: false,
                            is_break: false,
                        }));
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
            if cur.last().is_some_and(|p| p.is_space) {
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
                InlineFrag::Piece(p) => p,
            };
            if piece.is_break {
                if cur.is_empty() {
                    self.emit_empty_line(x, &mut line_y, width, &mut lines);
                } else {
                    self.emit_line(&mut cur, &mut line_y, &mut lines, x, width, align);
                }
                cur_cells = 0;
                continue;
            }
            if piece.is_space {
                if cur.is_empty() {
                    continue; // leading spaces dropped
                }
                cur.push(piece);
                cur_cells += 1;
                continue;
            }

            // Overlong word: hard-break by cells.
            if piece.cells > width {
                if cur.last().is_some_and(|p| p.is_space) {
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
                    cur.push(Piece {
                        text: rest[..end].to_string(),
                        cells: cells as i32,
                        style,
                        node,
                        is_space: false,
                        is_break: false,
                    });
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
                if cur.last().is_some_and(|p| p.is_space) {
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
            computed: *computed,
            image_src: Some(url.clone()),
            image_size_firm: *firm,
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
        while cur.last().is_some_and(|p| p.is_space) {
            cur.pop();
        }
        if cur.is_empty() {
            return;
        }
        let content_cells: i32 = cur.iter().map(|p| p.cells).sum();
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
        });
        // Merge adjacent same-style pieces.
        let mut merged: Vec<Piece> = Vec::new();
        for p in cur.drain(..) {
            match merged.last_mut() {
                Some(last) if last.style == p.style => {
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
            let tid = self.alloc(LayoutBox {
                kind: BoxKind::Text,
                node: p.node,
                dimensions: Dimensions {
                    content: Rect {
                        x: cx,
                        y: *line_y,
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
            });
            cx += p.cells;
            child_ids.push(tid);
        }
        self.boxes[line_id.0 as usize].children = child_ids;
        lines.push(line_id);
        *line_y += 1;
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
                        cur.push(Piece {
                            text: " ".repeat(*cells as usize),
                            cells: *cells,
                            style: Style::default(),
                            node: None,
                            is_space: false,
                            is_break: false,
                        });
                    }
                }
                InlineItem::Marker { text, style } => {
                    cur.push(Piece {
                        text: text.clone(),
                        cells: text.width() as i32,
                        style: *style,
                        node: None,
                        is_space: false,
                        is_break: false,
                    });
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
                            cur.push(Piece {
                                text: before.to_string(),
                                cells: before.width() as i32,
                                style: *style,
                                node: Some(*node),
                                is_space: false,
                                is_break: false,
                            });
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
                        cur.push(Piece {
                            text: rest.to_string(),
                            cells: rest.width() as i32,
                            style: *style,
                            node: Some(*node),
                            is_space: false,
                            is_break: false,
                        });
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
                // Block-level `display:block` img goes through layout_node;
                // default inline img stays in the IFC as an atomic replaced box.
                if tag == "img" {
                    return if is_block_level(self.styles.get(id).display) {
                        ChildMode::Block
                    } else {
                        ChildMode::Inline
                    };
                }
                match self.styles.get(id).display {
                    Display::Inline => ChildMode::Inline,
                    // Reveal: a page-hidden box is walked as block so its
                    // subtree can surface. UA-important none never gets here.
                    // Everything left is block-level.
                    _ => ChildMode::Block,
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
                    computed: *self.styles.get(id),
                });
            }
            NodeData::Element { tag, .. } => {
                if self.is_hidden(id) {
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
                            computed: *self.styles.get(id),
                        });
                    }
                    return;
                }
                let computed = self.styles.get(id);
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
}

impl FlexAxis {
    fn of(direction: FlexDirection, width: i32, definite_height: Option<i32>) -> Self {
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
            width,
            main_base: if vertical {
                definite_height
            } else {
                Some(width)
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
    /// a row's main-axis gap and `row-gap` is a column's. The other one is the
    /// gap between flex lines, and a `nowrap` container has one line, so it
    /// falls between nothing until M9.10.
    fn main_gap(self, gaps: Gaps) -> Length {
        if self.vertical { gaps.row } else { gaps.column }
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
/// `display: flex` is necessary and not sufficient: the two *row* directions
/// are laid out, so a column container keeps stacking its children as a block
/// until the rest of M9.9 lands — which is what it did before flex layout
/// existed, and much closer to what a column means than laying it out sideways
/// would be.
///
/// Both the engine and intrinsic sizing ask this. They must agree, or a flex
/// container's measured width and its laid-out width would come from different
/// algorithms. `row-reverse` needs nothing of its own from the measuring side:
/// a row asks for the sum of its items either way round, and reversing the
/// order of a sum does not change it.
pub(super) fn lays_out_as_flex(computed: &ComputedStyle) -> bool {
    computed.display == Display::Flex
        && matches!(
            computed.flex_direction,
            FlexDirection::Row | FlexDirection::RowReverse
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
}

struct Piece {
    text: String,
    cells: i32,
    style: Style,
    node: Option<NodeId>,
    is_space: bool,
    is_break: bool,
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

fn resolve_block_dims(computed: &ComputedStyle, containing_width: i32) -> Dimensions {
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
        // Fill available: content = containing - margin - border - padding.
        // Already a content-box size whatever `box-sizing` says — `auto` sizes
        // the margin box to the containing block either way.
        (containing_width
            - margin.left
            - margin.right
            - border.left
            - border.right
            - pad.left
            - pad.right)
            .max(0)
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
/// **This is the whole of what `display: flex` means to layout today (M9.5).**
/// The vocabulary — direction, wrap, gaps, the flex factors — cascades and
/// shows up in `F2`, but no box reads it yet, so a flex container is laid out
/// exactly as a block container: children stacked, in document order,
/// `order` ignored. That is what `flex` already did when M4 parsed it straight
/// to `Block`, which is why landing the vocabulary moves no snapshot.
///
/// M9.6 replaces this: `Display::Flex` gets its own arm in `layout_node`, and
/// what remains here is the block/inline question.
pub(super) fn is_block_level(display: Display) -> bool {
    matches!(display, Display::Block | Display::Flex)
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
