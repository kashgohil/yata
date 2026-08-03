//! Layout engine: DOM + styles + width → positioned box tree (PLAN.md M5).
//!
//! Pure transform. Block boxes stack vertically with margin collapse between
//! adjacent siblings; inline content fills line boxes with unicode-width
//! wrapping. Unit conversion lives on `Length` (M5.1).

use crate::dom::{Dom, NodeData, NodeId};
use crate::image::ImageContext;
use crate::layout::boxes::{BoxId, BoxKind, LayoutBox, LayoutTree};
use crate::layout::dimensions::{Dimensions, EdgeSizes, Rect};
use crate::style::values::{BoxSizing, Display, FontStyle, FontWeight, Length, TextAlign};
use crate::style::{ComputedStyle, Styles};
use crate::term::{Attrs, Color, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

        let box_id = self.alloc(LayoutBox {
            kind: BoxKind::Block,
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
                                text: "• ".into(),
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
                            text: "• ".into(),
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
                    text: "• ".into(),
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

        // Used height: the specified one if there is one, else the content's
        // (an empty div is zero rows), then the min/max clamps. Children keep
        // the boxes and positions they were given, so a box shorter than its
        // content lets that content overflow and paint past the bottom edge —
        // `overflow: visible` is the initial value, and clipping is M9.3.
        // The flow advances by *this* height, which is what makes `height: 0`
        // collapse a box whose children still exist.
        let auto_height = (content_y - dims.content.y).max(0);
        let content_height = v_axis.clamp(
            specified_height.unwrap_or(auto_height),
            definite_v(computed.min_height, containing_height),
            definite_v(computed.max_height, containing_height),
        );
        self.boxes[box_id.0 as usize].dimensions.content.height = content_height;
        self.boxes[box_id.0 as usize].children = children;

        *prev_margin_bottom = self.boxes[box_id.0 as usize].dimensions.margin.bottom;
        Some(box_id)
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

enum ChildMode {
    Skip,
    Block,
    Inline,
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
