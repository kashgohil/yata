//! Intrinsic widths (PLAN.md M9, task M9.4): how wide a box *wants* to be.
//!
//! Two answers, both in terminal cells measured with `unicode-width`, and both
//! **content-box** widths — a caller that wants a margin box adds the edges
//! itself:
//!
//! - **max-content**: the width at which the box never wraps.
//! - **min-content**: the width of the widest piece that cannot be broken (the
//!   longest word, or the longest line inside `pre`).
//!
//! The engine only knows how to fill an available width, so flexbox cannot be
//! written without this: `flex-basis: auto` on an auto-width item is its
//! max-content width, and `min-width: auto` on a flex item is its min-content
//! width (css-flexbox-1 §4.5). Nothing consumes it yet — M9.6 and M9.8 are the
//! callers.
//!
//! This lives beside `engine` rather than inside it because it answers a
//! question about a subtree *without* laying anything out: no boxes are
//! allocated, no DOM or style value is touched. What it must not do is invent a
//! second engine — break opportunities come from `engine::is_html_space`, the
//! predicate the line breaker splits text runs on, and the sizing arithmetic
//! (`box-sizing`, the min/max clamp order) comes from M9.2's `engine::Axis`.

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

use crate::dom::{Dom, NodeData, NodeId};
use crate::image::ImageContext;
use crate::layout::engine::{
    Axis, FlexItemSource, Hidden, LIST_MARKER, edge_h, flex_sources, is_block_level, is_html_space,
    lays_out_as_flex,
};
use crate::style::Styles;
use crate::style::values::{Display, FlexBasis, Length};

/// The containing width handed to image sizing, and to nothing else.
///
/// An intrinsic measurement has no containing block — the box is being asked
/// how wide it would like to be, before anything has said how much room there
/// is. M8.2's resolution order only ever uses the containing width as a *cap*
/// (and as the width of a placeholder whose real size is still unknown), so an
/// unbounded one yields the image's natural size and the placeholder's own
/// default.
const UNCONSTRAINED: i32 = i32::MAX;

/// A box's two intrinsic widths, in cells, content-box.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Sizes {
    min: i32,
    max: i32,
}

impl Sizes {
    const ZERO: Sizes = Sizes { min: 0, max: 0 };

    fn both(cells: i32) -> Sizes {
        Sizes {
            min: cells,
            max: cells,
        }
    }

    /// This box's sizes seen from its parent: its own horizontal margin,
    /// border and padding are part of how much room it asks the parent for.
    fn grown_by(self, edges: i32) -> Sizes {
        Sizes {
            min: self.min + edges,
            max: self.max + edges,
        }
    }

    fn max_with(self, other: Sizes) -> Sizes {
        Sizes {
            min: self.min.max(other.min),
            max: self.max.max(other.max),
        }
    }
}

/// One layout pass's intrinsic-size scratchpad.
///
/// The memo lives and dies with the pass that created it. Intrinsic widths are
/// a function of DOM + styles + image metrics, every one of which the next pass
/// may have replaced, so a cache that outlived a pass would make this stage
/// something other than a pure transform of its inputs. Inside a pass it is
/// what keeps the work linear: flex asks for the sizes of items whose subtrees
/// nest, and without the memo a deep tree would be walked once per ancestor.
pub struct IntrinsicSizer<'a> {
    dom: &'a Dom,
    styles: &'a Styles,
    images: &'a ImageContext,
    /// The pass's `display:none` mode, carried for one reason: the engine's
    /// reveal pass lays hidden subtrees out anyway (M4's never-blank rescue),
    /// and a sizer that always treated them as nothing would hand flex base
    /// sizes that disagree with the boxes the engine actually builds on every
    /// page rescued that way.
    hidden: Hidden,
    memo: HashMap<NodeId, Sizes>,
    /// Instrumentation, not surface: how many nodes this pass has actually
    /// measured. It exists so the memo's promise can be pinned by a test and
    /// for no other reason, which is why it is not compiled into the module a
    /// caller sees.
    #[cfg(test)]
    measured: usize,
}

impl<'a> IntrinsicSizer<'a> {
    pub fn new(
        dom: &'a Dom,
        styles: &'a Styles,
        images: &'a ImageContext,
        hidden: Hidden,
    ) -> IntrinsicSizer<'a> {
        IntrinsicSizer {
            dom,
            styles,
            images,
            hidden,
            memo: HashMap::new(),
            #[cfg(test)]
            measured: 0,
        }
    }

    /// Narrowest width this node's content can be squeezed into without a
    /// piece having to be broken. Content-box cells.
    pub fn min_content_width(&mut self, node: NodeId) -> i32 {
        self.sizes(node).min
    }

    /// Width this node's content takes when nothing wraps it. Content-box
    /// cells.
    pub fn max_content_width(&mut self, node: NodeId) -> i32 {
        self.sizes(node).max
    }

    /// The two widths of one *run* of sibling nodes measured as a single
    /// inline formatting context — `(min, max)`, content-box cells.
    ///
    /// This is what an anonymous flex item is (M9.6, §4): a contiguous
    /// sequence of text between two element siblings, which shares one line
    /// box and therefore one measurement. Measuring the nodes separately and
    /// summing would be wrong in both directions — max-content must count the
    /// space that joins them, and min-content must not be the sum at all.
    ///
    /// Not memoized: the key is the run, not a node, and a run is measured
    /// once per layout.
    pub fn run_widths(&mut self, nodes: &[NodeId]) -> (i32, i32) {
        let pre = nodes.first().is_some_and(|&n| self.in_pre(n));
        let mut run = Run::new(pre);
        for &node in nodes {
            self.push_pieces(node, &mut run);
        }
        let sizes = run.finish();
        (sizes.min, sizes.max)
    }

    /// How many nodes this pass has actually measured — the memo's promise
    /// made observable. A node that got measured twice shows up here as two,
    /// which is what the memo test pins.
    #[cfg(test)]
    fn measured_nodes(&self) -> usize {
        self.measured
    }

    fn sizes(&mut self, node: NodeId) -> Sizes {
        if let Some(&cached) = self.memo.get(&node) {
            return cached;
        }
        #[cfg(test)]
        {
            self.measured += 1;
        }
        let sizes = self.measure(node);
        self.memo.insert(node, sizes);
        sizes
    }

    fn measure(&mut self, node: NodeId) -> Sizes {
        let dom = self.dom;
        match &dom.node(node).data {
            NodeData::Comment(_) | NodeData::Doctype(_) => Sizes::ZERO,
            // The engine lays the document's children straight into the page
            // column, so the document measures like the block container it is.
            NodeData::Document => self.children_sizes(node),
            // A text node asked about on its own: one inline run of one item.
            NodeData::Text(_) => {
                let mut run = Run::new(self.in_pre(node));
                self.push_pieces(node, &mut run);
                run.finish()
            }
            NodeData::Element { tag, .. } => self.element_sizes(node, tag),
        }
    }

    fn element_sizes(&mut self, node: NodeId, tag: &str) -> Sizes {
        let computed = *self.styles.get(node);
        if self.is_hidden(node) {
            // Generates no box, so it asks its parent for no width.
            return Sizes::ZERO;
        }

        let axis = axis_h(&computed);

        let base = if tag != "img"
            && let Some(specified) = definite_h(computed.width)
        {
            // A box told how wide to be is that wide, whatever is inside it.
            // (`<img>` is the exception this engine already had: a CSS `width`
            // on one is not something layout honours yet, so measuring it as
            // though it were would disagree with the box that gets built.)
            Sizes::both(axis.content_from(specified))
        } else {
            self.content_sizes(node, tag, &computed)
        };

        // M9.2's clamp, called rather than restated: `max` first, then `min`,
        // so `min-width` wins a conflict with a smaller `max-width`.
        let min = definite_h(computed.min_width);
        let max = definite_h(computed.max_width);
        Sizes {
            min: axis.clamp(base.min, min, max),
            max: axis.clamp(base.max, min, max),
        }
    }

    /// What this element's **content** wants, ignoring any `width` the page
    /// put on the element itself — `(min, max)` content-box cells. Its
    /// descendants keep their own widths; only this box's is set aside.
    ///
    /// Two callers in M9.6, and both need exactly this rather than
    /// [`max_content_width`](Self::max_content_width): `flex-basis: content`
    /// exists to override the main size property (an item that answered "240px,
    /// because that is my `width`" would make the keyword mean nothing), and
    /// §4.5's *content size suggestion* is one half of an automatic minimum
    /// size whose other half is the specified size — the spec takes the smaller
    /// of the two, which it cannot do if the sizer has already folded them
    /// together.
    pub fn content_widths(&mut self, node: NodeId) -> (i32, i32) {
        let sizes = match &self.dom.node(node).data {
            NodeData::Element { tag, .. } => {
                let tag = tag.clone();
                let computed = *self.styles.get(node);
                if self.is_hidden(node) {
                    Sizes::ZERO
                } else {
                    self.content_sizes(node, &tag, &computed)
                }
            }
            // Anything that is not an element has no `width` to set aside.
            _ => self.sizes(node),
        };
        (sizes.min, sizes.max)
    }

    /// The content-based half of [`element_sizes`](Self::element_sizes): what
    /// is inside the box, by whichever rule its formatting context uses.
    fn content_sizes(
        &mut self,
        node: NodeId,
        tag: &str,
        computed: &crate::style::ComputedStyle,
    ) -> Sizes {
        if tag == "img" {
            // Replaced: the image box's own cell width (M8.2's resolution
            // order), which is the size the engine gives it too. An image the
            // context has never heard of generates no box, so it asks for no
            // width.
            Sizes::both(self.image_width(node).unwrap_or(0))
        } else if lays_out_as_flex(computed) {
            self.flex_sizes(node, computed)
        } else {
            self.children_sizes(node)
        }
    }

    /// A flex row's sizes (§9.9): its items sit **side by side**, so both
    /// widths are sums where a block container's are maxima. That difference is
    /// the whole reason this function exists — measuring a flex row like a
    /// block reports the width of its widest item, which is what a two-item
    /// row would be if it were allowed to wrap, and it is not (M9.6 is
    /// `nowrap`).
    ///
    /// Simplification, stated rather than hidden: the spec sizes a flex
    /// container from its items' *contributions*, which scale each item by its
    /// flex fraction (§9.9.1). Here max-content sums the items' max-content
    /// sizes and min-content sums their min-content sizes, which is exact for
    /// the case that matters — at a container width equal to either sum, §9.7
    /// has zero free space to distribute and hands every item exactly the size
    /// that went into the sum.
    fn flex_sizes(&mut self, node: NodeId, computed: &crate::style::ComputedStyle) -> Sizes {
        let pre = self.in_pre(node);
        let sources = flex_sources(self.dom, node, &|n| self.is_hidden(n), pre);
        if sources.is_empty() {
            return Sizes::ZERO;
        }
        // Gaps are part of what the row asks for: a row of three items with a
        // one-cell gap needs two cells nothing will ever draw in. Zero
        // containing width, per this module's percentage rule.
        let gap = edge_h(computed.gap.column, 0).max(0) * (sources.len() as i32 - 1);
        let mut out = Sizes { min: gap, max: gap };
        for source in sources {
            let sizes = match source {
                FlexItemSource::Element(child) => {
                    self.item_sizes(child).grown_by(self.outer_edges(child))
                }
                FlexItemSource::Text(nodes) => {
                    let (min, max) = self.run_widths(&nodes);
                    Sizes { min, max }
                }
            };
            out = Sizes {
                min: out.min + sizes.min,
                max: out.max + sizes.max,
            };
        }
        out
    }

    /// What one element item asks the row for, in place of what
    /// [`sizes`](Self::sizes) would say about it as a plain box.
    ///
    /// The difference is `flex-basis`. §9.2 step 3 makes an item's *flex base
    /// size* what the row is built from, and a definite `flex-basis` overrides
    /// `width` there — so an item with `flex: 0 0 20em` and one word in it asks
    /// for 40 cells, while `sizes` (which only knows about `width`) says one.
    /// Left that way, a nested row measures as wide as its text and then lays
    /// its items out past its own edge and over its next sibling.
    ///
    /// Which of the two sizes the basis decides depends on which way the item
    /// can flex, and that is exact rather than approximate in both directions:
    /// an item that cannot grow can never exceed its basis, and one that cannot
    /// shrink can never go under it. The other side keeps asking what its
    /// content wants — a growable item's real contribution scales by the row's
    /// flex fraction (§9.9.1), which this module states plainly that it does
    /// not model.
    fn item_sizes(&mut self, child: NodeId) -> Sizes {
        let computed = *self.styles.get(child);
        let content = self.sizes(child);
        let Some(basis) = definite_basis(&computed) else {
            return content;
        };
        Sizes {
            // `.max()` rather than the basis outright: a growable item is at
            // least its basis, and how much more is the part not modelled here.
            max: if computed.flex.grow == 0.0 {
                basis
            } else {
                basis.max(content.max)
            },
            // A shrinkable item bottoms out at §4.5's automatic minimum, which
            // is the min-content width `sizes` already reports.
            min: if computed.flex.shrink == 0.0 {
                basis
            } else {
                content.min
            },
        }
    }

    /// A block container's sizes: the max over what each child asks for.
    /// Consecutive inline children form one inline formatting context, whose
    /// pieces sum for max-content and take the max for min-content.
    fn children_sizes(&mut self, node: NodeId) -> Sizes {
        let pre = self.in_pre(node);
        let mut out = Sizes::ZERO;
        let mut run = Run::new(pre);
        // The list marker (M9.6). The engine injects `LIST_MARKER` as the
        // first inline piece of an `<li>` — or, when the item starts with a
        // block, as a line of its own — so seeding the run reproduces both:
        // it joins the first inline run here too, and if a block child closes
        // the run first, it is flushed as a marker-wide segment. Without this
        // an `<li>` measures two cells narrower than it lays out, and as a
        // flex item that means text the algorithm believed would fit wraps.
        if matches!(&self.dom.node(node).data, NodeData::Element { tag, .. } if tag == "li") {
            run.piece(LIST_MARKER.width() as i32);
        }
        let children: Vec<NodeId> = self.dom.children(node).collect();
        for child in children {
            match self.child_mode(child) {
                ChildMode::Skip => {}
                ChildMode::Inline => self.push_pieces(child, &mut run),
                ChildMode::Block => {
                    // A block closes the run before it, exactly as it forces
                    // the engine to flush the anonymous block it was building.
                    let flushed = std::mem::replace(&mut run, Run::new(pre)).finish();
                    let outer = self.outer_edges(child);
                    out = out
                        .max_with(flushed)
                        .max_with(self.sizes(child).grown_by(outer));
                }
            }
        }
        out.max_with(run.finish())
    }

    /// Flatten an inline subtree into the run's pieces. Mirrors the engine's
    /// `push_inline` — including its recursion into *every* child of an inline
    /// element, whatever that child's `display` says, because that is what the
    /// engine really puts on the line.
    ///
    /// The `<li>` marker is missing on purpose: the bullet is decoration the
    /// engine injects while building boxes, and none of M9.4's rules mention
    /// it.
    fn push_pieces(&self, node: NodeId, run: &mut Run) {
        let dom = self.dom;
        match &dom.node(node).data {
            NodeData::Text(text) => {
                if run.pre {
                    // No collapsing and no wrapping: each `\n` closes a line
                    // and everything between two of them is one piece, spaces
                    // included (engine: `layout_pre`).
                    let mut rest = text.as_str();
                    while let Some(nl) = rest.find('\n') {
                        let before = &rest[..nl];
                        if !before.is_empty() {
                            run.piece(before.width() as i32);
                        }
                        run.line_break();
                        rest = &rest[nl + 1..];
                    }
                    if !rest.is_empty() {
                        run.piece(rest.width() as i32);
                    }
                } else {
                    // The line breaker's own split, predicate and all
                    // (engine: `layout_inline`): the words it hands to the
                    // breaker are exactly the pieces it refuses to break, so
                    // measuring them is measuring where lines can actually
                    // break. Whitespace collapses the same way it does there —
                    // any run of it between two words is one cell.
                    if text.starts_with(is_html_space) {
                        run.space();
                    }
                    let mut first = true;
                    for word in text.split(is_html_space).filter(|w| !w.is_empty()) {
                        if !first {
                            run.space();
                        }
                        first = false;
                        run.piece(word.width() as i32);
                    }
                    if text.ends_with(is_html_space) {
                        run.space();
                    }
                }
            }
            NodeData::Element { tag, .. } => {
                if self.is_hidden(node) {
                    return;
                }
                if tag == "br" {
                    run.line_break();
                    return;
                }
                if tag == "img" {
                    // Atomic: the engine gives an inline image a line of its
                    // own, so it neither joins the segment around it nor can
                    // be broken. An image the context does not know (no `src`,
                    // or no image context at all) puts *nothing* on the line
                    // there, so it must not break one here either.
                    if let Some(cells) = self.image_width(node) {
                        run.replaced(cells);
                    }
                    return;
                }
                let computed = self.styles.get(node);
                // Horizontal margin/padding/border on an inline becomes a
                // fixed-width piece on the line (HN's `.hnname`). Zero
                // containing width: see the percentage rule on `definite_h`.
                let lead = edge_h(computed.margin.left, 0)
                    + edge_h(computed.padding.left, 0)
                    + edge_h(computed.border.left, 0);
                let trail = edge_h(computed.margin.right, 0)
                    + edge_h(computed.padding.right, 0)
                    + edge_h(computed.border.right, 0);
                if lead > 0 {
                    run.piece(lead);
                }
                for child in self.dom.children(node) {
                    self.push_pieces(child, run);
                }
                if trail > 0 {
                    run.piece(trail);
                }
            }
            _ => {}
        }
    }

    /// Which formatting context a child of a block container joins. Mirrors
    /// the engine's `child_mode`, `Hidden` mode included — under reveal a
    /// page-hidden child is walked as a block there, so it is measured as one
    /// here.
    fn child_mode(&self, node: NodeId) -> ChildMode {
        match &self.dom.node(node).data {
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => ChildMode::Skip,
            NodeData::Text(_) => ChildMode::Inline,
            NodeData::Element { tag, .. } => {
                let display = self.styles.get(node).display;
                if self.is_hidden(node) {
                    return ChildMode::Skip;
                }
                if tag == "br" || tag == "hr" {
                    return ChildMode::Block;
                }
                if tag == "img" {
                    return if is_block_level(display) {
                        ChildMode::Block
                    } else {
                        ChildMode::Inline
                    };
                }
                match display {
                    Display::Inline => ChildMode::Inline,
                    // Block-level is what remains, and so is a revealed
                    // `display:none` — the engine walks one as a block.
                    _ => ChildMode::Block,
                }
            }
        }
    }

    /// Horizontal margin + border + padding of a block child, in cells. Zero
    /// containing width: see the percentage rule on `definite_h`.
    fn outer_edges(&self, node: NodeId) -> i32 {
        let c = self.styles.get(node);
        edge_h(c.margin.left, 0)
            + edge_h(c.margin.right, 0)
            + edge_h(c.border.left, 0)
            + edge_h(c.border.right, 0)
            + edge_h(c.padding.left, 0)
            + edge_h(c.padding.right, 0)
    }

    /// The replaced box's cell width, or `None` when the image context has
    /// never heard of this node — the case where the engine generates no box
    /// for it at all.
    fn image_width(&self, node: NodeId) -> Option<i32> {
        let img = self.images.by_node.get(&node)?;
        Some(self.images.size_for(img, UNCONSTRAINED).0)
    }

    /// Generates no box at all — the same question `Engine::is_hidden` asks,
    /// and answered from the same two facts, so a measurement and the layout it
    /// predicts cannot disagree about what is on the page. Under
    /// `Hidden::Reveal` a page's own `display:none` is measured (the engine
    /// lays it out as a block); the user-agent sheet's `!important` hiding is
    /// still nothing, because the engine still skips it.
    fn is_hidden(&self, node: NodeId) -> bool {
        let c = self.styles.get(node);
        c.display == Display::None && (self.hidden == Hidden::Respect || c.hidden_by_ua)
    }

    /// Is this node inside a `<pre>`? The engine threads a `pre` flag down
    /// from the block that started one; a measurement can begin anywhere in
    /// the tree, so the same fact is read back off the ancestor chain.
    fn in_pre(&self, node: NodeId) -> bool {
        let dom = self.dom;
        let mut cur = Some(node);
        while let Some(id) = cur {
            if matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "pre") {
                return true;
            }
            cur = dom.node(id).parent;
        }
        false
    }
}

enum ChildMode {
    Skip,
    Block,
    Inline,
}

/// A horizontal size property as cells, or `None` for "behaves as `auto`".
///
/// **The percentage rule for this whole module.** An intrinsic measurement has
/// no containing block — that is what makes it intrinsic — so any length that
/// needs one is unresolvable, and each unresolvable length falls back to its
/// own "nothing was specified" behaviour:
///
/// - a *size* (`width`, `min-width`, `max-width`) falls back to `auto`, so the
///   box is measured from its content. This is the one that matters: resolving
///   it against zero instead would report a 0-cell max-content for
///   `width: 50%` and collapse every percentage-sized flex item.
/// - an *edge* (`margin`, `padding`, `border`) falls back to zero cells, which
///   is what `edge_h(len, 0)` returns and what every edge site in this module
///   relies on. Zero is the absence of an edge, so it is also what
///   `box-sizing: border-box` then subtracts for one.
///
/// The two fallbacks differ because zero means different things on either
/// side: a zero *edge* is simply no edge, while a zero *size* is a positive
/// claim that the box is empty. CSS resolves the size case properly with a
/// second pass once the containing block is known; in a cell grid that is not
/// worth its complexity.
/// Padding and border on the inline axis, which is what `border-box` counts as
/// part of a specified width. Zero containing width: see the percentage rule
/// on [`definite_h`].
fn axis_h(computed: &crate::style::ComputedStyle) -> Axis {
    Axis {
        edges: edge_h(computed.padding.left, 0)
            + edge_h(computed.padding.right, 0)
            + edge_h(computed.border.left, 0)
            + edge_h(computed.border.right, 0),
        box_sizing: computed.box_sizing,
    }
}

/// A flex item's hypothetical main size (§9.2 steps 3–4) when `flex-basis`
/// alone decides it — the case [`Sizer::sizes`] cannot see, because it reads
/// `width` and `flex-basis` overrides `width`.
///
/// `None` means "no definite basis of its own": `auto`, `content`, or a
/// percentage, which needs the container's inner main size — the one thing an
/// intrinsic measurement has by definition not got.
fn definite_basis(computed: &crate::style::ComputedStyle) -> Option<i32> {
    let FlexBasis::Size(len) = computed.flex.basis else {
        return None;
    };
    let basis = definite_h(len)?;
    let axis = axis_h(computed);
    Some(axis.clamp(
        axis.content_from(basis),
        definite_h(computed.min_width),
        definite_h(computed.max_width),
    ))
}

fn definite_h(len: Length) -> Option<i32> {
    match len {
        Length::Auto | Length::Percent(_) => None,
        other => Some(other.to_cells_h(0)),
    }
}

/// One inline formatting context being measured.
///
/// Max-content is the widest *segment*: a run of pieces with no forced break
/// in it. Min-content is the widest single *piece*, because a piece is what
/// the line breaker refuses to split — the same units it wraps between. Both
/// are accumulated in one pass over the run.
struct Run {
    /// `pre`: nothing collapses and nothing wraps.
    pre: bool,
    /// Widest unbreakable piece seen.
    min: i32,
    /// Widest completed segment.
    max: i32,
    /// The segment being accumulated.
    cur: i32,
    /// Nothing on this segment yet, so a collapsible space would be dropped
    /// the way the breaker drops one at the start of a line.
    at_start: bool,
    /// A collapsed space is owed before the next piece.
    pending_space: bool,
}

impl Run {
    fn new(pre: bool) -> Run {
        Run {
            pre,
            min: 0,
            max: 0,
            cur: 0,
            at_start: true,
            pending_space: false,
        }
    }

    /// An unbreakable piece of `cells` cells.
    fn piece(&mut self, cells: i32) {
        if self.pending_space {
            self.pending_space = false;
            if !self.at_start {
                self.cur += 1;
            }
        }
        self.cur += cells;
        self.min = self.min.max(cells);
        self.at_start = false;
    }

    /// Collapsible whitespace: at most one cell, and only between pieces.
    fn space(&mut self) {
        self.pending_space = true;
    }

    /// An atomic replaced box, which takes a line to itself.
    fn replaced(&mut self, cells: i32) {
        self.line_break();
        self.piece(cells);
        self.line_break();
    }

    /// End the current segment (`<br>`, a `\n` in `pre`, or the end of the
    /// run). A space owed at a line end is dropped, as the breaker pops a
    /// trailing space before emitting a line.
    fn line_break(&mut self) {
        self.max = self.max.max(self.cur);
        self.cur = 0;
        self.at_start = true;
        self.pending_space = false;
    }

    fn finish(mut self) -> Sizes {
        self.line_break();
        Sizes {
            // Inside `pre` nothing wraps, so the narrowest this content can be
            // is still its widest line.
            min: if self.pre { self.max } else { self.min },
            max: self.max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::layout::{Hidden, layout};
    use crate::style;

    fn styled(html_src: &str, css: &str) -> (Dom, Styles) {
        let dom = html::parse(html_src);
        let sheet = crate::css::parse(css);
        let styles = style::style_tree(&dom, &[&sheet]);
        (dom, styles)
    }

    /// First element with this tag, in document order.
    fn find_tag(dom: &Dom, tag: &str) -> NodeId {
        fn walk(dom: &Dom, id: NodeId, tag: &str) -> Option<NodeId> {
            if matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag) {
                return Some(id);
            }
            dom.children(id).find_map(|c| walk(dom, c, tag))
        }
        walk(dom, dom.root, tag).unwrap_or_else(|| panic!("no <{tag}> in the fixture"))
    }

    /// `(min-content, max-content)` of the first `<tag>` in the document.
    fn sizes_of(html_src: &str, css: &str, tag: &str) -> (i32, i32) {
        let (dom, styles) = styled(html_src, css);
        let node = find_tag(&dom, tag);
        let images = ImageContext::default();
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images, Hidden::Respect);
        (sizer.min_content_width(node), sizer.max_content_width(node))
    }

    /// Non-blank rendered rows and their cell widths at a given column width.
    fn rows(html_src: &str, css: &str, width: u16) -> Vec<String> {
        let (dom, styles) = styled(html_src, css);
        layout(&dom, &styles, width, Hidden::Respect)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            })
            .filter(|t| !t.trim().is_empty())
            .collect()
    }

    #[test]
    fn a_text_run_measures_its_words_and_its_widest_word() {
        // "hello world" is 11 cells unwrapped; the widest thing that cannot be
        // broken is "hello", 5. Neither number depends on how much room there
        // is — that is what makes them intrinsic, and why the API takes no
        // width at all.
        assert_eq!(
            sizes_of("<div>hello world</div>", "div { margin: 0 }", "div"),
            (5, 11)
        );
    }

    #[test]
    fn the_intrinsic_widths_are_widths_the_line_breaker_agrees_with() {
        // The reason break opportunities must come from the breaker's own
        // helper: max-content is exactly the width at which the run stops
        // wrapping, and min-content is a width every line still fits into.
        let src = "<div>hello world</div>";
        let css = "div { margin: 0 }";
        let (min, max) = sizes_of(src, css, "div");

        assert_eq!(
            rows(src, css, max as u16).len(),
            1,
            "max-content must not wrap"
        );
        assert_eq!(
            rows(src, css, max as u16 - 1).len(),
            2,
            "one cell narrower must wrap"
        );
        for row in rows(src, css, min as u16) {
            assert!(
                row.width() as i32 <= min,
                "min-content {min} cannot hold {row:?}"
            );
        }
    }

    #[test]
    fn a_block_takes_the_max_of_its_children_including_their_padding() {
        // Outer holds a padded block ("wide!" = 5 cells + 1 cell of padding a
        // side = 7) and a wider-looking bare one ("hello" = 5). The padded
        // child wins because its padding is part of what it asks for.
        let src = "<div class=outer><div class=pad>wide!</div><div>hello</div></div>";
        let css = "div { margin: 0 } .pad { padding-left: 8px; padding-right: 8px }";
        let (dom, styles) = styled(src, css);
        let outer = find_tag(&dom, "div");
        let images = ImageContext::default();
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images, Hidden::Respect);
        assert_eq!(sizer.max_content_width(outer), 7);
        // min-content likewise: the padded child's word (5) plus its padding.
        assert_eq!(sizer.min_content_width(outer), 7);
    }

    #[test]
    fn pre_measures_its_widest_line_and_collapses_nothing() {
        // "a  b" keeps both spaces (4 cells); "xy" is 2. Nothing in a `pre`
        // wraps, so min-content is that same widest line, not its widest word.
        let (min, max) = sizes_of("<pre>a  b\nxy</pre>", "", "pre");
        assert_eq!((min, max), (4, 4));
    }

    #[test]
    fn a_specified_width_is_both_sizes_and_the_clamps_apply() {
        // 1em = 2 cells (PLAN.md unit table).
        assert_eq!(
            sizes_of("<div>hello world</div>", "div { width: 10em }", "div"),
            (20, 20)
        );
        assert_eq!(
            sizes_of(
                "<div>hello world</div>",
                "div { width: 10em; max-width: 4em }",
                "div"
            ),
            (8, 8),
            "max-width clamps the intrinsic result"
        );
        assert_eq!(
            sizes_of(
                "<div>hello world</div>",
                "div { width: 10em; min-width: 12em; max-width: 4em }",
                "div"
            ),
            (24, 24),
            "min-width beats a smaller max-width (M9.2's clamp order)"
        );
    }

    #[test]
    fn a_rows_width_comes_from_its_items_flex_basis_not_their_text() {
        // §9.2 step 3: `flex-basis` is what an item's flex base size comes
        // from, and it overrides the main size property. Measuring a row by
        // its items' text instead reports 2 cells for a row that lays out 20,
        // and the items then draw past their own parent and over its sibling.
        let row = "<div id=r><div class=i>a</div><div class=i>b</div></div>";
        let rigid = "#r { display: flex } .i { flex: 0 0 80px }";
        assert_eq!(sizes_of(row, rigid, "div"), (20, 20));

        // Which of the two sizes the basis decides is not a guess: an item
        // that cannot grow can never be wider than its basis, and one that
        // cannot shrink can never be narrower. Free one direction and only
        // that direction goes back to asking the content.
        let can_shrink = "#r { display: flex } .i { flex: 0 1 80px }";
        assert_eq!(
            sizes_of(row, can_shrink, "div"),
            (2, 20),
            "shrinkable items bottom out at §4.5's automatic minimum"
        );

        // `flex: 1` is `1 1 0`, and a growable item's real contribution scales
        // by the row's flex fraction (§9.9.1) — which this module says plainly
        // that it does not model. It must therefore keep asking what its
        // content wants rather than reporting a basis of zero, or every
        // `flex: 1` row in the wild would measure as nothing.
        let growable = "#r { display: flex } .i { flex: 1 }";
        assert_eq!(sizes_of(row, growable, "div"), (2, 2));
    }

    #[test]
    fn border_box_subtracts_padding_and_border_as_m9_2_does() {
        // 10em = 20 cells declared; padding 8px and border 8px a side are 4
        // cells of edges, so the content box is 16 — and that is exactly the
        // content width the engine gives the same box.
        let css = "div { margin: 0; width: 10em; padding: 8px; border: 8px solid; box-sizing: border-box }";
        assert_eq!(sizes_of("<div>hi</div>", css, "div"), (16, 16));

        let (dom, styles) = styled("<div>hi</div>", css);
        let div = find_tag(&dom, "div");
        let tree = crate::layout::layout_document(&dom, &styles, 80, Hidden::Respect);
        let mut laid_out = None;
        tree.walk(tree.root, &mut |_, b| {
            if b.node == Some(div) {
                laid_out = Some(b.dimensions.content.width);
            }
        });
        assert_eq!(
            laid_out,
            Some(16),
            "intrinsic sizing must not disagree with M9.2"
        );

        // The same declaration under `content-box` keeps all 20 cells.
        let content_box = "div { margin: 0; width: 10em; padding: 8px; border: 8px solid }";
        assert_eq!(sizes_of("<div>hi</div>", content_box, "div"), (20, 20));
    }

    #[test]
    fn an_image_measures_its_replaced_box() {
        // 80px wide at 8px per cell = 10 cells, both sizes (M8.2's order).
        let dom = html::parse(r#"<div><img src="a.png" width="80" height="48" alt="pic"></div>"#);
        let styles = style::style_tree(&dom, &[]);
        let imgs = crate::image::discover(&dom, Some("https://ex/"));
        let mut cache = crate::image::ImageCache::default();
        let images = crate::image::ImageContext::from_discovery(&imgs, &mut cache);
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images, Hidden::Respect);

        let img = find_tag(&dom, "img");
        assert_eq!(sizer.min_content_width(img), 10);
        assert_eq!(sizer.max_content_width(img), 10);
        // And through its parent: an inline image is atomic, so it is both the
        // widest piece and a segment of its own.
        let div = find_tag(&dom, "div");
        assert_eq!(sizer.min_content_width(div), 10);
        assert_eq!(sizer.max_content_width(div), 10);
    }

    #[test]
    fn wide_glyphs_are_measured_in_cells_not_chars() {
        // 世界 is two characters and four cells. Counting characters would say
        // min 2 / max 5; counting cells says min 4 / max 7.
        assert_eq!(
            sizes_of("<div>世界 hi</div>", "div { margin: 0 }", "div"),
            (4, 7)
        );
    }

    #[test]
    fn each_node_is_measured_once_per_pass() {
        // A deep chain: without the memo, measuring every node in it walks
        // each subtree once per ancestor — the O(n²) intrinsic pass that blows
        // the Wikipedia budget.
        const DEPTH: usize = 120;
        let mut src = String::new();
        for _ in 0..DEPTH {
            src.push_str("<div>");
        }
        src.push_str("hello world");
        for _ in 0..DEPTH {
            src.push_str("</div>");
        }
        let (dom, styles) = styled(&src, "div { margin: 0 }");
        let images = ImageContext::default();
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images, Hidden::Respect);

        // Measuring the outermost div measures the whole chain, once each.
        let outer = find_tag(&dom, "div");
        assert_eq!(sizer.max_content_width(outer), 11);
        assert_eq!(sizer.measured_nodes(), DEPTH);

        // The other size, and every node in the chain again: all memo hits.
        assert_eq!(sizer.min_content_width(outer), 5);
        let mut node = outer;
        loop {
            assert_eq!(sizer.max_content_width(node), 11);
            assert_eq!(sizer.min_content_width(node), 5);
            match dom.node(node).first_child {
                Some(child) if matches!(&dom.node(child).data, NodeData::Element { .. }) => {
                    node = child
                }
                _ => break,
            }
        }
        assert_eq!(
            sizer.measured_nodes(),
            DEPTH,
            "a node was measured more than once"
        );
    }

    #[test]
    fn display_none_children_contribute_nothing() {
        let src =
            "<div class=outer><div class=gone>a very wide hidden thing</div><div>hi</div></div>";
        let css = "div { margin: 0 } .gone { display: none }";
        assert_eq!(sizes_of(src, css, "div"), (2, 2));
    }

    /// The agreement test above, widened to the committed ladder pages.
    ///
    /// `push_pieces` reproduces `layout_inline`'s collapsing rules by hand, so
    /// the only thing keeping the two from drifting is a test that asks the
    /// breaker what it really does. One hand-written `<div>` cannot do that: it
    /// has no nested inlines, no inline margins, no `<br>`, no `pre`. These
    /// pages have all of it.
    ///
    /// Two framings of the same question, because no single one reaches every
    /// element:
    ///
    /// - **Narrowed** (`narrowing_agrees`): size the page so the element's
    ///   content box is *exactly* its max-content width. Nothing inside may
    ///   wrap — it must produce the same line boxes as it does with room to
    ///   spare — and one cell narrower something must. At min-content, no row
    ///   may stick out past the box. This needs the element's width to follow
    ///   the page's, which is not true everywhere: HN's `#hnmain` carries a
    ///   `min-width`, so nothing inside it can ever be narrowed to its
    ///   intrinsic size.
    /// - **Roomy** (`widest_row_agrees`): with room to spare, the widest row
    ///   the breaker produces inside the element must be *exactly* its
    ///   max-content width. That is the definition, checked directly, and it
    ///   needs no control over the width at all — but it only reads cleanly
    ///   for an element whose content is all inline, where every line box is
    ///   its own and no nested block can hide edges from the sum.
    ///
    /// Between them, every page below gets real coverage; each element is put
    /// through whichever framings it admits, and a page where none of them
    /// reached anything fails rather than passing quietly.
    mod ladder_agreement {
        use super::*;
        use crate::layout::{BoxId, BoxKind, LayoutTree, layout_document_with};
        use std::fs;

        /// How many elements of one page to measure. Each costs a handful of
        /// full layouts (the search for the page width that hands the element
        /// its intrinsic width), so this is a sample, spread across the
        /// document, not a sweep.
        const SAMPLE: usize = 6;

        /// Refuse to pass vacuously: if the filters below ever skip a whole
        /// page, the test has stopped testing and should say so.
        const MIN_CHECKED: usize = 3;

        fn fixture(name: &str) -> String {
            fs::read_to_string(format!(
                "{}/tests/fixtures/{name}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        }

        /// One document under test, with the image metrics *both* sides must
        /// be given: the engine and the sizer have to be looking at the same
        /// images, or they would disagree about `<img>` for a reason that has
        /// nothing to do with line breaking.
        struct Page<'a> {
            label: &'a str,
            dom: &'a Dom,
            styles: &'a Styles,
            images: &'a ImageContext,
        }

        impl Page<'_> {
            fn lay_out(&self, width: i32) -> LayoutTree {
                layout_document_with(
                    self.dom,
                    self.styles,
                    width.clamp(1, u16::MAX as i32) as u16,
                    Hidden::Respect,
                    self.images,
                )
            }

            fn content_width(&self, node: NodeId, width: i32) -> Option<i32> {
                let tree = self.lay_out(width);
                content_width(&tree, node)
            }

            /// The narrowest page width that gives `node` a content box of
            /// exactly `want` cells, or `None` if no width in range does.
            ///
            /// An element's available width is the page width minus whatever
            /// its ancestors take out of it, and that inset is not always a
            /// constant (a percentage-width ancestor scales with the page).
            /// Searching for the width and then *verifying* the box really
            /// came out at `want` is what makes this work regardless, and what
            /// makes an element it cannot hit skip rather than fail.
            fn page_width_giving(&self, node: NodeId, want: i32) -> Option<i32> {
                let (mut lo, mut hi) = (1i32, want + 200);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    match self.content_width(node, mid) {
                        Some(w) if w >= want => hi = mid,
                        _ => lo = mid + 1,
                    }
                }
                (self.content_width(node, lo) == Some(want)).then_some(lo)
            }
        }

        /// The block box generated for `node`, if it generated one.
        fn box_of(tree: &LayoutTree, node: NodeId) -> Option<BoxId> {
            let mut found = None;
            tree.walk(tree.root, &mut |id, b| {
                // A flex container's box is a `Flex`, not a `Block` (M9.6),
                // and it is still the box this element generated.
                if found.is_none()
                    && b.node == Some(node)
                    && matches!(b.kind, BoxKind::Block | BoxKind::Flex)
                {
                    found = Some(id);
                }
            });
            found
        }

        fn content_width(tree: &LayoutTree, node: NodeId) -> Option<i32> {
            box_of(tree, node).map(|id| tree.get(id).dimensions.content.width)
        }

        /// The widest row under `root`, as cells of content.
        ///
        /// Summing a line box's children rather than measuring how far right
        /// they reach is deliberate: `text-align` shifts a line's contents
        /// without making the line any wider, and HN wraps its whole page in
        /// `<center>`. A replaced image is a row of its own — the engine gives
        /// it one — so it counts as one here.
        fn widest_row(tree: &LayoutTree, root: BoxId) -> i32 {
            let mut out = 0;
            tree.walk(root, &mut |_, b| match b.kind {
                BoxKind::Line => {
                    let cells: i32 = b
                        .children
                        .iter()
                        .map(|&c| tree.get(c).dimensions.content.width)
                        .sum();
                    out = out.max(cells);
                }
                BoxKind::Image => out = out.max(b.dimensions.content.width),
                _ => {}
            });
            out
        }

        /// Line boxes under `root` — the count that goes up when text wraps.
        fn line_boxes(tree: &LayoutTree, root: BoxId) -> usize {
            let mut n = 0;
            tree.walk(root, &mut |_, b| {
                if b.kind == BoxKind::Line {
                    n += 1;
                }
            });
            n
        }

        /// How far past the left edge of `root`'s content box any text under it
        /// reaches.
        fn text_extent(tree: &LayoutTree, root: BoxId) -> i32 {
            let left = tree.get(root).dimensions.content.x;
            let mut out = 0;
            tree.walk(root, &mut |_, b| {
                if b.kind == BoxKind::Text {
                    out = out.max(b.dimensions.content.right() - left);
                }
            });
            out
        }

        fn tag_of(dom: &Dom, node: NodeId) -> &str {
            match &dom.node(node).data {
                NodeData::Element { tag, .. } => tag,
                _ => "?",
            }
        }

        /// Does any node in this subtree (including `node`) satisfy `f`?
        fn subtree_any(dom: &Dom, node: NodeId, f: &dyn Fn(NodeId) -> bool) -> bool {
            f(node) || dom.children(node).any(|c| subtree_any(dom, c, f))
        }

        impl Page<'_> {
            /// The elements of this page the three assertions can be asked of.
            ///
            /// Two exclusions, each because the assertion would be meaningless
            /// rather than because it would be inconvenient:
            ///
            /// - nothing to wrap: no text anywhere inside.
            /// - a specified `width` / `min-width` / `max-width` inside: such a box
            ///   is the width it was told to be no matter how much room there is,
            ///   so no page width narrows it and "one cell narrower and it wraps"
            ///   has nothing to observe. The unit tests above cover specified
            ///   widths directly.
            fn measurable_blocks(&self) -> Vec<NodeId> {
                let (dom, styles) = (self.dom, self.styles);
                let mut out = Vec::new();
                let mut stack = vec![dom.root];
                while let Some(node) = stack.pop() {
                    stack.extend(dom.children(node));
                    if !matches!(&dom.node(node).data, NodeData::Element { .. })
                        || !is_block_level(styles.get(node).display)
                    {
                        continue;
                    }
                    let has_text = subtree_any(dom, node, &|n| {
                        matches!(&dom.node(n).data, NodeData::Text(t) if !t.trim().is_empty())
                            && styles.get(n).display != Display::None
                    });
                    if !has_text {
                        continue;
                    }
                    // List items are *in* now (M9.6): the marker they lay out
                    // with is the marker they measure with, so the third of
                    // danluu.com this filter used to cost is back under the
                    // three assertions below.
                    let sized = subtree_any(dom, node, &|n| {
                        let c = styles.get(n);
                        !c.width.is_auto() || !c.min_width.is_auto() || !c.max_width.is_auto()
                    });
                    if sized {
                        continue;
                    }
                    out.push(node);
                }
                out.sort_by_key(|n| n.0);
                out
            }

            /// Put a sample of this page's blocks through both framings, and
            /// refuse to pass if the filters left nothing to measure.
            fn check(&self) {
                let mut sizer =
                    IntrinsicSizer::new(self.dom, self.styles, self.images, Hidden::Respect);
                let candidates = self.measurable_blocks();
                let step = (candidates.len() / SAMPLE).max(1);
                let mut checked = 0;
                for node in candidates.into_iter().step_by(step).take(SAMPLE) {
                    let max = sizer.max_content_width(node);
                    let min = sizer.min_content_width(node);
                    if max <= 0 || min <= 0 {
                        continue;
                    }
                    let what = format!(
                        "{}: <{}> #{} (max {max}, min {min})",
                        self.label,
                        tag_of(self.dom, node),
                        node.0
                    );
                    let mut reached = self.widest_row_agrees(node, max, &what);
                    reached |= self.narrowing_agrees(node, max, min, &what);
                    if reached {
                        checked += 1;
                    }
                }
                assert!(
                    checked >= MIN_CHECKED,
                    "{}: only {checked} elements were measurable — the filters \
                     have eaten the test",
                    self.label
                );
            }

            /// Roomy framing: given more width than it wants, the breaker's
            /// widest row inside this element is exactly its max-content width.
            ///
            /// Only asked of elements whose content is all inline — a nested
            /// block brings its own margins, padding and (for `<hr>`) a
            /// full-width rule, none of which a sum over rows can see.
            fn widest_row_agrees(&self, node: NodeId, max: i32, what: &str) -> bool {
                let has_block_child = self.dom.children(node).any(|c| {
                    subtree_any(self.dom, c, &|n| is_block_element(self.dom, self.styles, n))
                });
                if has_block_child {
                    return false;
                }
                // A flex container is not one inline formatting context either
                // (M9.6): its items are blockified and sit side by side, so the
                // width it wants is a sum *across* boxes and no single line box
                // ever holds it. The narrowed framing below still measures
                // these — and on danluu.com it is the only thing that does.
                if lays_out_as_flex(self.styles.get(node)) {
                    return false;
                }
                let tree = self.lay_out(max + 200);
                let Some(id) = box_of(&tree, node) else {
                    return false;
                };
                // Room to spare is the premise; without it nothing is shown.
                if tree.get(id).dimensions.content.width < max {
                    return false;
                }
                assert_eq!(
                    widest_row(&tree, id),
                    max,
                    "{what}: the breaker's widest unwrapped row is not max-content"
                );
                true
            }

            /// Narrowed framing: at exactly max-content nothing wraps, one cell
            /// under it something does, and at min-content every row still fits.
            fn narrowing_agrees(&self, node: NodeId, max: i32, min: i32, what: &str) -> bool {
                let Some(page) = self.page_width_giving(node, max) else {
                    return false;
                };
                // With room to spare, and at exactly max-content: the same
                // lines, because nothing wrapped at either width.
                let roomy = self.lay_out(page + 40);
                let at_max = self.lay_out(page);
                let (Some(roomy_box), Some(max_box)) =
                    (box_of(&roomy, node), box_of(&at_max, node))
                else {
                    return false;
                };
                assert_eq!(
                    line_boxes(&at_max, max_box),
                    line_boxes(&roomy, roomy_box),
                    "{what}: content wrapped at its own max-content width"
                );

                // One cell narrower, something must give — unless this content
                // has no break in it at all (`min == max`: a `pre` block, whose
                // lines never wrap however narrow the box gets). Then there is
                // nothing the engine could do differently and nothing to
                // assert.
                let at_narrow = self.lay_out(page - 1);
                if min < max
                    && let Some(narrow_box) = box_of(&at_narrow, node)
                {
                    assert!(
                        line_boxes(&at_narrow, narrow_box) > line_boxes(&at_max, max_box),
                        "{what}: nothing wrapped one cell below max-content, so \
                         max-content is wider than the run really needs"
                    );
                }

                // At min-content, every row the breaker produces fits.
                if let Some(page) = self.page_width_giving(node, min) {
                    let tree = self.lay_out(page);
                    if let Some(min_box) = box_of(&tree, node) {
                        assert!(
                            text_extent(&tree, min_box) <= min,
                            "{what}: a row overflows min-content by {} cells",
                            text_extent(&tree, min_box) - min
                        );
                    }
                }
                true
            }
        }

        /// Block-*level*, not `display:block` — a flex container is still one
        /// of these, and until M9.6 it is laid out as a block container too
        /// (`engine::is_block_level`). Asking the narrower question here would
        /// quietly drop danluu.com's `li{display:flex}` and `.np` from the
        /// suite the moment M9.5 gave `flex` its own value.
        fn is_block_element(dom: &Dom, styles: &Styles, node: NodeId) -> bool {
            matches!(&dom.node(node).data, NodeData::Element { .. })
                && is_block_level(styles.get(node).display)
        }

        fn check(name: &str, extra_css: Option<&str>) {
            let source = fixture(name);
            let dom = html::parse(&source);
            let inline = style::sources::inline_sheets(&dom);
            let page = extra_css.map(|css| crate::css::parse(&fixture(css)));
            let mut sheets: Vec<&crate::css::Stylesheet> = inline.iter().collect();
            if let Some(page) = &page {
                sheets.push(page);
            }
            let styles = style::style_tree(&dom, &sheets);
            Page {
                label: name,
                dom: &dom,
                styles: &styles,
                images: &ImageContext::default(),
            }
            .check();
        }

        #[test]
        fn example_com() {
            check("example.com.html", None);
        }

        #[test]
        fn motherfuckingwebsite_com() {
            check("motherfuckingwebsite.com.html", None);
        }

        #[test]
        fn danluu_com() {
            check("danluu.com.html", None);
        }

        #[test]
        fn news_ycombinator_com() {
            check(
                "news.ycombinator.com.html",
                Some("news.ycombinator.com.news.css"),
            );
        }

        #[test]
        fn shapes_the_ladder_pages_do_not_have() {
            // No page on the ladder contains a `<pre>`, and the inline
            // margin/padding pieces only appear on HN. Rather than add a
            // fixture, one document written for the purpose puts every shape
            // `push_pieces` handles through the same two framings:
            //
            // - runs of collapsible whitespace, including at the very start and
            //   end of a block, where the breaker drops them entirely;
            // - an inline with horizontal margin *and* padding nested inside a
            //   text run;
            // - forced breaks, both as a direct child of the block and nested
            //   inside an inline — different paths on both sides;
            // - a `pre` whose widest line is several pieces, so that "no
            //   collapsing, no wrapping" is pinned for min-content as well as
            //   max-content;
            // - loose text on both sides of a block child, which the engine
            //   splits into two anonymous blocks and the sizer into two runs;
            // - wide glyphs.
            let (dom, styles) = styled(
                concat!(
                    "<div class=doc>",
                    "<div class=para>a  bb   ccc dddd</div>",
                    "<div class=ws>   spaced out   </div>",
                    "<div class=nested>lead <b class=tight>bold</b> tail</div>",
                    "<div class=brs>alpha<br>beta gamma<br>d</div>",
                    "<div class=nbr>x <span>alpha<br>beta gamma</span> y</div>",
                    "<pre>xx  <b>yy</b> zzzz\nshort\nq</pre>",
                    "<div class=mixed>loose words before",
                    "<div class=inner>inner block</div>and words after</div>",
                    "<div class=cjk>世界 hi こんにちは</div>",
                    "</div>",
                ),
                "div, pre { margin: 0 } .tight { margin-left: 8px; padding-right: 16px }",
            );
            Page {
                label: "synthetic shapes",
                dom: &dom,
                styles: &styles,
                images: &ImageContext::default(),
            }
            .check();
        }

        #[test]
        fn an_inline_image_is_atomic_on_both_sides() {
            // The one shape that needs an image context: an inline `<img>` is
            // a row of its own, so it ends the segment around it rather than
            // widening it. Both sides are handed the same context — the engine
            // would otherwise not place an image at all.
            let dom = html::parse(concat!(
                "<div class=doc><div class=img>",
                r#"text before <img src="a.png" width="48" height="16"> text after"#,
                "</div></div>",
            ));
            let sheet = crate::css::parse("div { margin: 0 }");
            let styles = style::style_tree(&dom, &[&sheet]);
            let imgs = crate::image::discover(&dom, Some("https://fixture.test/page"));
            let mut cache = crate::image::ImageCache::default();
            let images = ImageContext::from_discovery(&imgs, &mut cache);
            Page {
                label: "inline image",
                dom: &dom,
                styles: &styles,
                images: &images,
            }
            .check();
        }
    }
}
