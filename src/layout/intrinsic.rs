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
use crate::layout::engine::{Axis, edge_h, is_html_space};
use crate::style::Styles;
use crate::style::values::{Display, Length};

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
    memo: HashMap<NodeId, Sizes>,
    /// Instrumentation, not surface: how many nodes this pass has actually
    /// measured. It exists so the memo's promise can be pinned by a test and
    /// for no other reason, which is why it is not compiled into the module a
    /// caller sees.
    #[cfg(test)]
    measured: usize,
}

impl<'a> IntrinsicSizer<'a> {
    pub fn new(dom: &'a Dom, styles: &'a Styles, images: &'a ImageContext) -> IntrinsicSizer<'a> {
        IntrinsicSizer {
            dom,
            styles,
            images,
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
        if computed.display == Display::None {
            // Generates no box, so it asks its parent for no width. (The
            // engine's `Hidden::Reveal` pass is a rescue for pages that hide
            // themselves entirely, not a sizing mode; sizing follows the
            // cascade.)
            return Sizes::ZERO;
        }

        // Padding and border on this axis, which is what `border-box` counts
        // as part of a specified width. Zero containing width: see the
        // percentage rule on `definite_h`.
        let axis = Axis {
            edges: edge_h(computed.padding.left, 0)
                + edge_h(computed.padding.right, 0)
                + edge_h(computed.border.left, 0)
                + edge_h(computed.border.right, 0),
            box_sizing: computed.box_sizing,
        };

        let base = if tag == "img" {
            // Replaced: the image box's own cell width (M8.2's resolution
            // order), which is the size the engine gives it too — a CSS
            // `width` on an `<img>` is not something layout honours yet. An
            // image the context does not know generates no box, so it asks for
            // no width.
            Sizes::both(self.image_width(node).unwrap_or(0))
        } else if let Some(specified) = definite_h(computed.width) {
            Sizes::both(axis.content_from(specified))
        } else {
            self.children_sizes(node)
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

    /// A block container's sizes: the max over what each child asks for.
    /// Consecutive inline children form one inline formatting context, whose
    /// pieces sum for max-content and take the max for min-content.
    fn children_sizes(&mut self, node: NodeId) -> Sizes {
        let pre = self.in_pre(node);
        let mut out = Sizes::ZERO;
        let mut run = Run::new(pre);
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
                if self.styles.get(node).display == Display::None {
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
    /// the engine's `child_mode`, except that `display: none` contributes
    /// nothing here rather than being walked for the reveal pass.
    fn child_mode(&self, node: NodeId) -> ChildMode {
        match &self.dom.node(node).data {
            NodeData::Comment(_) | NodeData::Doctype(_) | NodeData::Document => ChildMode::Skip,
            NodeData::Text(_) => ChildMode::Inline,
            NodeData::Element { tag, .. } => {
                let display = self.styles.get(node).display;
                if display == Display::None {
                    return ChildMode::Skip;
                }
                if tag == "br" || tag == "hr" {
                    return ChildMode::Block;
                }
                if tag == "img" {
                    return match display {
                        Display::Block => ChildMode::Block,
                        _ => ChildMode::Inline,
                    };
                }
                match display {
                    Display::Inline => ChildMode::Inline,
                    // `None` left above; block-level is what remains.
                    Display::Block | Display::None => ChildMode::Block,
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
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images);
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
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images);
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
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images);

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
        let mut sizer = IntrinsicSizer::new(&dom, &styles, &images);

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
}
