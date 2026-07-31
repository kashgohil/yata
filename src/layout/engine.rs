//! Layout engine: DOM + styles + width → positioned box tree (PLAN.md M5).
//!
//! Pure transform. Block boxes stack vertically with margin collapse between
//! adjacent siblings; inline content fills line boxes with unicode-width
//! wrapping. Unit conversion lives on `Length` (M5.1).

use crate::dom::{Dom, NodeData, NodeId};
use crate::layout::boxes::{BoxId, BoxKind, LayoutBox, LayoutTree};
use crate::layout::dimensions::{Dimensions, EdgeSizes, Rect};
use crate::style::values::{Display, FontStyle, FontWeight, TextAlign};
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
    let width = width.max(1) as i32;
    let mut eng = Engine {
        dom,
        styles,
        hidden,
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
    });
    let mut y = 0i32;
    let mut prev_mb = 0i32;
    for child in dom.children(dom.root) {
        if let Some(id) = eng.layout_node(child, 0, width, y, &mut prev_mb, false) {
            eng.boxes[root.0 as usize].children.push(id);
            let mb = eng.boxes[id.0 as usize].dimensions.margin_box();
            y = mb.bottom();
            prev_mb = eng.boxes[id.0 as usize].dimensions.margin.bottom;
        }
    }
    eng.boxes[root.0 as usize].dimensions.content.height = y.max(0);
    let height = y.max(0);
    LayoutTree {
        boxes: eng.boxes,
        root,
        width,
        height,
    }
}

struct Engine<'a> {
    dom: &'a Dom,
    styles: &'a Styles,
    hidden: Hidden,
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
    fn layout_node(
        &mut self,
        id: NodeId,
        x: i32,
        containing_width: i32,
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
                    _ => match computed.display {
                        Display::None => None,
                        Display::Block => self.layout_block(
                            id,
                            tag,
                            computed,
                            x,
                            containing_width,
                            y,
                            prev_margin_bottom,
                            pre || tag == "pre",
                        ),
                        Display::Inline => {
                            // Inline element as a block-container child: wrap.
                            let items = self.collect_inline(id, pre || tag == "pre");
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
                    },
                }
            }
        }
    }

    fn layout_block(
        &mut self,
        id: NodeId,
        tag: &str,
        computed: ComputedStyle,
        containing_x: i32,
        containing_width: i32,
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
        });

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
                    marker_pending = false;
                    if let Some(cid) = self.layout_node(
                        child,
                        content_x,
                        content_w,
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
                    self.push_inline(child, pre, &mut inline_run);
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
                children.push(anon);
            }
        }

        // Empty blocks (div with no content) get zero height — fine.
        let content_height = (content_y - dims.content.y).max(0);
        self.boxes[box_id.0 as usize].dimensions.content.height = content_height;
        self.boxes[box_id.0 as usize].children = children;

        *prev_margin_bottom = self.boxes[box_id.0 as usize].dimensions.margin.bottom;
        Some(box_id)
    }

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
        // Anonymous blocks have no margin of their own.
        let y = y - *prev_margin_bottom;
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
        });
        let box_id = self.alloc(LayoutBox {
            kind: BoxKind::Block,
            node: None,
            dimensions: dims,
            children: vec![text_id],
            text: None,
            term_style: Style::default(),
            computed,
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
        // Flatten to words and break opportunities (collapsed spaces).
        let mut pieces: Vec<Piece> = Vec::new();
        let mut pending_space: Option<Style> = None;

        for item in items {
            match item {
                InlineItem::Marker { text, style } => {
                    pieces.push(Piece {
                        text: text.clone(),
                        cells: text.width() as i32,
                        style: *style,
                        node: None,
                        is_space: false,
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
                            pieces.push(Piece {
                                text: " ".into(),
                                cells: 1,
                                style: pending_space.unwrap_or(*style),
                                node: None,
                                is_space: true,
                            });
                        }
                        first = false;
                        pending_space = None;
                        pieces.push(Piece {
                            text: word.to_string(),
                            cells: word.width() as i32,
                            style: *style,
                            node: Some(*node),
                            is_space: false,
                        });
                    }
                    if text.ends_with(is_html_space) {
                        pending_space = Some(*style);
                    }
                }
                InlineItem::ElementStart { .. } | InlineItem::ElementEnd => {}
            }
        }

        let mut lines: Vec<BoxId> = Vec::new();
        let mut line_y = y;
        let mut cur: Vec<Piece> = Vec::new();
        let mut cur_cells = 0i32;

        for piece in pieces {
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
            });
            cx += p.cells;
            child_ids.push(tid);
        }
        self.boxes[line_id.0 as usize].children = child_ids;
        lines.push(line_id);
        *line_y += 1;
    }

    fn layout_pre(&mut self, items: &[InlineItem], x: i32, y: i32, width: i32) -> Vec<BoxId> {
        // Concatenate all text preserving newlines; each source line → one line box.
        let mut buf = String::new();
        let mut style = Style::default();
        let mut node = None;
        for item in items {
            if let InlineItem::Text {
                node: n,
                text,
                style: s,
                ..
            } = item
            {
                buf.push_str(text);
                style = *s;
                node = Some(*n);
            }
        }
        let mut lines = Vec::new();
        let mut line_y = y;
        for seg in buf.split('\n') {
            let cells = seg.width() as i32;
            let line_id = self.alloc(LayoutBox {
                kind: BoxKind::Line,
                node: None,
                dimensions: Dimensions {
                    content: Rect {
                        x,
                        y: line_y,
                        width,
                        height: 1,
                    },
                    ..Dimensions::default()
                },
                children: Vec::new(),
                text: None,
                term_style: Style::default(),
                computed: ComputedStyle::default(),
            });
            if !seg.is_empty() {
                let tid = self.alloc(LayoutBox {
                    kind: BoxKind::Text,
                    node,
                    dimensions: Dimensions {
                        content: Rect {
                            x,
                            y: line_y,
                            width: cells,
                            height: 1,
                        },
                        ..Dimensions::default()
                    },
                    children: Vec::new(),
                    text: Some(seg.to_string()),
                    term_style: style,
                    computed: ComputedStyle::default(),
                });
                self.boxes[line_id.0 as usize].children.push(tid);
            }
            lines.push(line_id);
            line_y += 1;
        }
        lines
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
                match self.styles.get(id).display {
                    // Reveal: a page-hidden box is walked as block so its
                    // subtree can surface. UA-important none never gets here.
                    Display::None => ChildMode::Block,
                    Display::Block => ChildMode::Block,
                    Display::Inline => ChildMode::Inline,
                }
            }
        }
    }

    fn push_inline(&self, id: NodeId, pre: bool, out: &mut Vec<InlineItem>) {
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
                    // br inside inline run: represent as a newline in pre sense
                    // by splitting — handled as block-level in child_mode.
                    return;
                }
                let pre = pre || tag == "pre";
                for child in self.dom.children(id) {
                    self.push_inline(child, pre, out);
                }
            }
            _ => {}
        }
    }

    fn collect_inline(&self, id: NodeId, pre: bool) -> Vec<InlineItem> {
        let mut out = Vec::new();
        self.push_inline(id, pre, &mut out);
        out
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
    #[allow(dead_code)]
    ElementStart {
        node: NodeId,
    },
    #[allow(dead_code)]
    ElementEnd,
}

struct Piece {
    text: String,
    cells: i32,
    style: Style,
    node: Option<NodeId>,
    is_space: bool,
}

/// Resolve horizontal box model for a block in a containing block of width `cw`.
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

    // Content width.
    let mut width = if computed.width.is_auto() {
        // Fill available: content = containing - margin - border - padding.
        (containing_width
            - margin.left
            - margin.right
            - border.left
            - border.right
            - pad.left
            - pad.right)
            .max(0)
    } else {
        computed.width.to_cells_h(containing_width)
    };

    if !computed.max_width.is_auto() {
        let max_w = computed.max_width.to_cells_h(containing_width);
        if width > max_w {
            width = max_w;
        }
    }

    // If width was specified and margins are auto, centre the block.
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

fn is_html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}
