//! Style resolution: DOM + stylesheets → computed values (PLAN.md §2, M4.2).
//!
//! The semantics half of M4. `css/` decided what the author *wrote*; this
//! decides what each node *is*: which rules match it, which declaration wins,
//! and what it inherits from its parent. Input is a `&Dom` and the page's
//! stylesheets, output is one `ComputedStyle` per `NodeId` — a pure transform,
//! like every other stage.
//!
//! Nothing renders differently yet: layout and paint keep M3's hardcoded
//! styling until M4.4 rewires them onto these values.

pub mod matching;
pub mod sources;
pub mod values;

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::css::{self, Declaration, Stylesheet};
use crate::dom::{Dom, NodeData, NodeId};
use matching::RuleIndex;
use values::{
    AlignContent, AlignItems, AlignSelf, BoxSizing, ColorValue, Display, Edges, Flex,
    FlexDirection, FlexWrap, FontStyle, FontWeight, Gaps, GridAutoFlow, GridPlacement, GridTracks,
    JustifyContent, Length, Overflow, Position, TextAlign,
};

/// Dynamic matching inputs for the cascade (M6): which node is hovered, which
/// absolute URLs the user has visited, and the base URL for resolving `href`s
/// in `:link` / `:visited`. Empty/default is what headless dumps and pure
/// cascade unit tests use — every link is unvisited, nothing hovers.
///
/// `visited` is borrowed so a hover restyle does not clone the set every move.
#[derive(Clone, Copy, Debug)]
pub struct StyleContext<'a> {
    pub hover: Option<NodeId>,
    pub visited: &'a HashSet<String>,
    pub base_url: Option<&'a str>,
}

impl Default for StyleContext<'static> {
    fn default() -> Self {
        static EMPTY: OnceLock<HashSet<String>> = OnceLock::new();
        StyleContext {
            hover: None,
            visited: EMPTY.get_or_init(HashSet::new),
            base_url: None,
        }
    }
}

/// What a node looks like once the cascade and inheritance have run. `Default`
/// is the CSS initial value of every property, which is also what a node with
/// no matching rule and no parent gets.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ComputedStyle {
    pub display: Display,
    /// Positioning is non-inherited. Insets are kept as authored lengths so
    /// layout can resolve each against the right containing-block axis.
    pub position: Position,
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
    pub color: ColorValue,
    pub background_color: ColorValue,
    pub font_weight: FontWeight,
    pub font_style: FontStyle,
    /// `text-decoration`, as much of it as a cell grid has: underlined or not.
    pub underline: bool,
    pub text_align: TextAlign,
    /// Box model (M5.1). None of these inherit — a span does not pad itself
    /// because its parent paragraph has padding.
    pub margin: Edges,
    pub padding: Edges,
    /// Border widths only; colour/style of borders arrive with paint (M5).
    pub border: Edges,
    pub width: Length,
    pub max_width: Length,
    /// Sizing added in M9.2 (CSS 2.1 §10.4–10.7). `height` is `Auto` — content
    /// height — until a page says otherwise; the min/max pairs clamp whatever
    /// the used value works out to, with `min` winning over `max`.
    pub min_width: Length,
    pub height: Length,
    pub min_height: Length,
    pub max_height: Length,
    /// Which box `width`/`height` and the clamps above describe (M9.2).
    pub box_sizing: BoxSizing,
    /// `overflow-x` / `overflow-y` (M9.3): whether content that does not fit
    /// this box is clipped to its padding box. Two axes, because
    /// `overflow-x: hidden` alone must not clip vertically. Not inherited —
    /// a paragraph inside a clipped menu is not itself a clipping box.
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    /// Flex *container* properties (M9.5): what a `display:flex` box does with
    /// the children it flexes. None of them inherit — a `<span>` inside a flex
    /// container is not itself a flex container, and its own children are laid
    /// out by whatever formatting context it establishes.
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub align_content: AlignContent,
    /// `row-gap` / `column-gap`. Also the `gap` shorthand, which is why they
    /// share a struct.
    pub gap: Gaps,
    /// Flex *item* properties: what this box asks of the container flexing it.
    /// Set on the child, read by the parent — so they sit on every node, not
    /// only on flex children, and mean nothing until a flex container reads
    /// them (M9.6).
    pub flex: Flex,
    pub align_self: AlignSelf,
    /// `order`: the sequence items are placed in, lowest first (M9.6). It does
    /// not reorder the DOM — hit-testing, focus and the inspectors keep seeing
    /// document order, which is what CSS says too.
    pub order: i32,
    pub grid_template_columns: GridTracks,
    pub grid_template_rows: GridTracks,
    pub grid_column: (GridPlacement, GridPlacement),
    pub grid_row: (GridPlacement, GridPlacement),
    pub grid_auto_flow: GridAutoFlow,
    /// The winning `display` came from the user-agent sheet's `!important` —
    /// this element holds code, metadata or inert markup and is never prose
    /// (see `ua.css`). Layout's never-blank fallback honours this even when it
    /// is ignoring every other `display:none`: a page hidden behind a script
    /// should be revealed, the script itself never.
    pub hidden_by_ua: bool,
}

impl ComputedStyle {
    /// Whether these two agree on everything **layout** reads (M10.6).
    ///
    /// Implemented by blanking the paint-only properties and comparing the
    /// rest wholesale, rather than by listing the layout ones. That is
    /// deliberate: the list below is the *exception* list, so a property added
    /// to this struct and forgotten here counts as layout-relevant by default.
    /// The cost of being wrong in that direction is a relayout nobody needed;
    /// the cost of being wrong the other way is a page that does not update.
    /// M10.6's rule is correctness first, and this is where it is enforced.
    ///
    /// What is exempt: the terminal draws these into cell attributes, and
    /// `recolour_tree` refreshes them on an existing layout tree. None of them
    /// can move a box — a bold cell is exactly as wide as a plain one, which
    /// is a property of a grid that a proportional renderer would not share.
    pub fn layout_eq(&self, other: &ComputedStyle) -> bool {
        fn without_paint(mut style: ComputedStyle) -> ComputedStyle {
            style.color = ColorValue::default();
            style.background_color = ColorValue::default();
            style.font_weight = FontWeight::default();
            style.font_style = FontStyle::default();
            style.underline = false;
            style
        }
        without_paint(self.clone()) == without_paint(other.clone())
    }

    /// What a child starts from: the inherited properties of its parent, with
    /// every non-inherited one back at its initial value. `display` and
    /// `background-color` do not inherit — a `<span>` inside a block is not
    /// itself a block, and a paragraph's background does not tint its words.
    ///
    /// `text-decoration` is inherited here, and in CSS it is not: real
    /// decoration propagates to descendants by a separate mechanism that draws
    /// the parent's line *through* the child's box. Inheriting it is the cheap
    /// approximation that puts the underline on a link's inner `<span>`, which
    /// is where a browser also draws one.
    fn inherit(&self) -> ComputedStyle {
        ComputedStyle {
            display: Display::default(),
            position: Position::default(),
            top: Length::Auto,
            right: Length::Auto,
            bottom: Length::Auto,
            left: Length::Auto,
            background_color: ColorValue::default(),
            color: self.color,
            font_weight: self.font_weight,
            font_style: self.font_style,
            underline: self.underline,
            text_align: self.text_align,
            // Box model does not inherit.
            margin: Edges::default(),
            padding: Edges::default(),
            border: Edges::default(),
            width: Length::Auto,
            max_width: Length::Auto,
            min_width: Length::Auto,
            height: Length::Auto,
            min_height: Length::Auto,
            max_height: Length::Auto,
            // `box-sizing` is not inherited either (CSS): pages set it on
            // everything through `*`, which the cascade already handles.
            box_sizing: BoxSizing::default(),
            // Nor does `overflow` (M9.3): the box that clips is the one the
            // page put it on.
            overflow_x: Overflow::default(),
            overflow_y: Overflow::default(),
            // Nor does any of the flex vocabulary (M9.5). A container's
            // `justify-content` describes how *it* places *its* children, and
            // an item's `flex: 1` is a request to *its* parent; inheriting
            // either would apply it a level too deep.
            flex_direction: FlexDirection::default(),
            flex_wrap: FlexWrap::default(),
            justify_content: JustifyContent::default(),
            align_items: AlignItems::default(),
            align_content: AlignContent::default(),
            gap: Gaps::default(),
            flex: Flex::default(),
            align_self: AlignSelf::default(),
            order: 0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_column: (GridPlacement::Auto, GridPlacement::Auto),
            grid_row: (GridPlacement::Auto, GridPlacement::Auto),
            grid_auto_flow: GridAutoFlow::Row,
            // Not inherited: a child of a hidden `<script>` is hidden because
            // its ancestor's subtree is skipped, not because it inherited a
            // verdict about itself.
            hidden_by_ua: false,
        }
    }
}

/// Computed values for every node, indexed by `NodeId`. Dense rather than a
/// map because the arena is dense: one slot per node, text nodes included, so
/// paint can ask any node what it looks like without a lookup that can miss.
///
/// `Clone` exists for M11.3: the invalidation cycle keeps the values the page
/// was laid out with so it can compare them against the new ones, and a scoped
/// restyle writes in place rather than producing a second tree to compare.
#[derive(Clone)]
pub struct Styles {
    computed: Vec<ComputedStyle>,
    /// How many nodes have had their values resolved into this tree — one per
    /// node for a full pass, one per subtree node for a scoped one (M11.3).
    /// Test-only, like M10.6's three counters and for the same reason:
    /// `styles_run` counts restyles and cannot tell a subtree from a document,
    /// so nothing else can say whether the narrowing actually narrowed.
    #[cfg(test)]
    nodes_styled: usize,
}

impl Styles {
    pub fn get(&self, id: NodeId) -> &ComputedStyle {
        &self.computed[id.0 as usize]
    }

    /// How many nodes this tree has room for. Equal to the arena's
    /// `node_count` when it was built — and *not* equal any more once a script
    /// has created a node, which is what M11.3's scoped path checks before it
    /// writes into a `Vec` that may be a slot short.
    pub fn node_count(&self) -> usize {
        self.computed.len()
    }

    /// Nodes resolved into this tree so far. See the field.
    #[cfg(test)]
    pub fn nodes_styled(&self) -> usize {
        self.nodes_styled
    }

    /// Whether every node still computes to the same values **layout reads**
    /// (M10.6). `false` means the page has to be laid out again; `true` means
    /// whatever changed is paint-only, and refreshing the existing layout
    /// tree's colours is enough — the path `:hover` has always taken.
    ///
    /// Two trees of different sizes are never equal: a node was created, so
    /// the styled tree grew and the comparison has nothing to align.
    pub fn layout_eq(&self, other: &Styles) -> bool {
        self.computed.len() == other.computed.len()
            && self
                .computed
                .iter()
                .zip(&other.computed)
                .all(|(a, b)| a.layout_eq(b))
    }
}

/// The user-agent stylesheet: how bare HTML looks before any page says
/// otherwise. Parsed once — it is a constant, and re-parsing it per page would
/// be pure waste in the restyle budget.
pub fn ua_stylesheet() -> &'static Stylesheet {
    static UA: OnceLock<Stylesheet> = OnceLock::new();
    UA.get_or_init(|| css::parse(include_str!("ua.css")))
}

/// Resolve every node's computed values against the UA sheet, the page's
/// sheets in order, and each element's `style=""` attribute. Uses an empty
/// [`StyleContext`] — no hover, no visited set.
pub fn style_tree(dom: &Dom, sheets: &[&Stylesheet]) -> Styles {
    style_tree_with(dom, sheets, &StyleContext::default())
}

/// Like [`style_tree`], but with hover / visited matching (M6).
pub fn style_tree_with(dom: &Dom, sheets: &[&Stylesheet], ctx: &StyleContext<'_>) -> Styles {
    let ua = RuleIndex::build(&[ua_stylesheet()]);
    let author = RuleIndex::build(sheets);
    let mut styles = Styles {
        computed: vec![ComputedStyle::default(); dom.node_count()],
        #[cfg(test)]
        nodes_styled: 0,
    };
    // One pre-order walk: a node's parent is always resolved before it, which
    // is the whole of inheritance. No second pass, no fixpoint.
    resolve(
        dom,
        dom.root,
        &ComputedStyle::default(),
        &ua,
        &author,
        ctx,
        &mut styles,
    );
    styles
}

/// Resolve the live arena with UA rules and interaction state only, then make
/// nodes outside a reader projection impossible for layout to reveal.
///
/// The projection is presentation metadata, not a DOM rewrite.  Marking its
/// excluded roots with the same `hidden_by_ua` boundary used for inert markup
/// means [`crate::layout::Hidden::Reveal`] can recover author-hidden prose
/// without recovering page chrome or siblings of the projection spine.
pub fn style_reader_tree_with(dom: &Dom, included: &[bool], ctx: &StyleContext<'_>) -> Styles {
    debug_assert_eq!(included.len(), dom.node_count());
    let ua = RuleIndex::build(&[ua_stylesheet()]);
    let author = RuleIndex::build(&[]);
    let mut excluded = ComputedStyle::default();
    excluded.display = Display::None;
    excluded.hidden_by_ua = true;
    let mut styles = Styles {
        computed: vec![excluded; dom.node_count()],
        #[cfg(test)]
        nodes_styled: 0,
    };
    if included.get(dom.root.0 as usize).copied().unwrap_or(false) {
        resolve_projected(
            dom,
            dom.root,
            &ComputedStyle::default(),
            &ua,
            &author,
            ctx,
            included,
            &mut styles,
        );
    }
    styles
}

#[allow(clippy::too_many_arguments)]
fn resolve_projected(
    dom: &Dom,
    node: NodeId,
    parent: &ComputedStyle,
    ua: &RuleIndex<'_>,
    author: &RuleIndex<'_>,
    ctx: &StyleContext<'_>,
    included: &[bool],
    out: &mut Styles,
) {
    if !included.get(node.0 as usize).copied().unwrap_or(false) {
        return;
    }
    #[cfg(test)]
    {
        out.nodes_styled += 1;
    }
    let computed = match &dom.node(node).data {
        NodeData::Element { .. } => cascade(dom, node, parent, ua, author, ctx),
        _ => parent.inherit(),
    };
    out.computed[node.0 as usize] = computed.clone();
    for child in dom.children(node) {
        resolve_projected(dom, child, &computed, ua, author, ctx, included, out);
    }
}

/// Recompute `roots` and everything under them, in place, leaving every other
/// node's computed values exactly as they were (M11.3). The same pre-order
/// walk [`style_tree_with`] runs, started somewhere other than the document
/// root and seeded from the parent values already in `styles`.
///
/// # Why a subtree is the whole answer, and when it stops being
///
/// An attribute write on `N` can change the computed values of `N` and of `N`'s
/// descendants, and of **nothing else** — but only because of what this
/// engine's selectors can express:
///
/// - Descendant and child combinators look *up* the tree ([`css::Combinator`]
///   has exactly those two), so only nodes under `N` can newly match, or stop
///   matching, because of something about `N`.
/// - Inheritance flows down, so a change to `N`'s own values reaches its
///   subtree and no further.
/// - **Sibling combinators (`+`, `~`) do not exist**, and they are the
///   construct that would break this: `N + p` makes `N`'s *next sibling*
///   depend on `N`. Neither does `:has()`, which would make its ancestors
///   depend on it.
///
/// **The day a sibling combinator is added to [`css::Combinator`], this
/// narrowing is wrong** and the caller in `App::apply_dom_changes` has to go
/// back to a full pass — or this function has to grow the sibling's subtree
/// into its walk. That is not a nicety: the failure mode is a page that
/// silently does not update, which is the one bug M10.6's classification was
/// built to make impossible. Whoever implements `+` starts here.
///
/// Roots are handled in any order and may nest: the final write to any node
/// comes from the pass of the last root that is an ancestor-or-self of it, and
/// that root's parent is always resolved by then. Roots covered by another
/// root are skipped rather than styled twice for the same answer.
///
/// `styles` must be the tree that was resolved from this `dom`, at this size —
/// a script that created a node grew the arena past it, and the caller checks
/// [`Styles::node_count`] before calling.
pub fn restyle_subtree(
    dom: &Dom,
    sheets: &[&Stylesheet],
    ctx: &StyleContext<'_>,
    styles: &mut Styles,
    roots: &[NodeId],
) {
    debug_assert_eq!(styles.node_count(), dom.node_count());
    // Built once for the whole call rather than once per root: on Wikipedia's
    // sheets the index costs more than a small subtree does.
    let ua = RuleIndex::build(&[ua_stylesheet()]);
    let author = RuleIndex::build(sheets);
    for &root in roots {
        if roots
            .iter()
            .any(|&other| other != root && is_ancestor(dom, other, root))
        {
            continue;
        }
        // A detached node is never reached by a full pass, so its slot holds
        // the initial values; styling it here would make the scoped answer
        // differ from the full one for a node nothing can see. It can only
        // become visible through an insert, which is a structural edit and
        // restyles the document anyway.
        let Some(parent) = inherited_from(dom, styles, root) else {
            continue;
        };
        resolve(dom, root, &parent, &ua, &author, ctx, styles);
    }
}

/// The already-computed values `root` inherits from, or `None` when `root` is
/// not in the document.
fn inherited_from(dom: &Dom, styles: &Styles, root: NodeId) -> Option<ComputedStyle> {
    if root == dom.root {
        return Some(ComputedStyle::default());
    }
    let parent = dom.node(root).parent?;
    is_ancestor(dom, dom.root, root).then(|| styles.get(parent).clone())
}

/// Whether `ancestor` is a strict ancestor of `id`.
fn is_ancestor(dom: &Dom, ancestor: NodeId, id: NodeId) -> bool {
    let mut walk = dom.node(id).parent;
    while let Some(current) = walk {
        if current == ancestor {
            return true;
        }
        walk = dom.node(current).parent;
    }
    false
}

fn resolve(
    dom: &Dom,
    node: NodeId,
    parent: &ComputedStyle,
    ua: &RuleIndex,
    author: &RuleIndex,
    ctx: &StyleContext<'_>,
    out: &mut Styles,
) {
    #[cfg(test)]
    {
        out.nodes_styled += 1;
    }
    let computed = match &dom.node(node).data {
        NodeData::Element { .. } => cascade(dom, node, parent, ua, author, ctx),
        // Text, comments and the document root match no selector; they carry
        // their parent's inherited values so paint can style a text run by
        // asking the text node itself.
        _ => parent.inherit(),
    };
    out.computed[node.0 as usize] = computed.clone();
    for child in dom.children(node) {
        resolve(dom, child, &computed, ua, author, ctx, out);
    }
}

/// Cascade origin and importance, low priority to high. The order is CSS's:
/// author rules beat the UA sheet, `!important` inverts the author/UA
/// relationship, and the UA sheet's `!important` is the one thing a page
/// cannot override.
///
/// Inline declarations get their own ranks rather than a huge specificity,
/// because `style=""` beating every selector is a fact about *origin*, not
/// about how specific the author was.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Rank {
    UaNormal,
    AuthorNormal,
    InlineNormal,
    AuthorImportant,
    InlineImportant,
    UaImportant,
}

/// One declaration in the running, with everything the sort needs.
struct Entry<'a> {
    rank: Rank,
    specificity: (u16, u16, u16),
    /// Position within its own origin — the last tie-break, so a later rule of
    /// equal weight wins.
    order: usize,
    declaration: &'a Declaration,
}

fn cascade(
    dom: &Dom,
    node: NodeId,
    parent: &ComputedStyle,
    ua: &RuleIndex,
    author: &RuleIndex,
    ctx: &StyleContext<'_>,
) -> ComputedStyle {
    let mut entries: Vec<Entry> = Vec::new();
    for (index, normal, important) in [
        (ua, Rank::UaNormal, Rank::UaImportant),
        (author, Rank::AuthorNormal, Rank::AuthorImportant),
    ] {
        for candidate in index.matches(dom, node, ctx) {
            let specificity = candidate.selector.specificity();
            for declaration in candidate.declarations {
                entries.push(Entry {
                    rank: if declaration.important {
                        important
                    } else {
                        normal
                    },
                    specificity,
                    order: candidate.order,
                    declaration,
                });
            }
        }
    }

    // `style=""` is parsed per element and per restyle. It is a handful of
    // declarations on the elements that have one at all; caching it would mean
    // holding parsed state between stages, which is what the pipeline forbids.
    let inline = dom
        .attr(node, "style")
        .map(css::parse_declarations)
        .unwrap_or_default();
    for (order, declaration) in inline.iter().enumerate() {
        entries.push(Entry {
            rank: if declaration.important {
                Rank::InlineImportant
            } else {
                Rank::InlineNormal
            },
            specificity: (0, 0, 0),
            order,
            declaration,
        });
    }

    entries.sort_by_key(|e| (e.rank, e.specificity, e.order));

    let mut computed = parent.inherit();
    for entry in &entries {
        let applied = apply(&mut computed, entry.declaration);
        // Entries are in ascending cascade order, so the last `display` to
        // apply is the winner and its rank is the one that matters.
        if applied && entry.declaration.name == "display" {
            computed.hidden_by_ua = entry.rank == Rank::UaImportant;
        }
    }
    computed
}

/// Apply one declaration, if it is a property M4 implements and its value
/// parses; `true` when it actually changed something. An unparseable value
/// leaves the previous winner standing — that is CSS's rule for invalid values,
/// and it is why `color: bananas` must not resolve to anything.
fn apply(computed: &mut ComputedStyle, declaration: &Declaration) -> bool {
    let value = declaration.value.as_str();
    match declaration.name.as_str() {
        "display" => set(&mut computed.display, values::parse_display(value)),
        "position" => set(&mut computed.position, values::parse_position(value)),
        "top" => set(&mut computed.top, values::parse_length(value)),
        "right" => set(&mut computed.right, values::parse_length(value)),
        "bottom" => set(&mut computed.bottom, values::parse_length(value)),
        "left" => set(&mut computed.left, values::parse_length(value)),
        "color" => set(&mut computed.color, values::parse_color(value)),
        "background-color" => set(&mut computed.background_color, values::parse_color(value)),
        // The `background` shorthand, honoured for the two spellings that
        // resolve to a colour: a bare colour (`background:#eee`, which is
        // example.com's) and `none`, which resets to the initial value the way
        // the shorthand is defined to. Anything with an image or a position in
        // it is left alone rather than half-applied.
        "background" if value.trim().eq_ignore_ascii_case("none") => {
            computed.background_color = ColorValue::default();
            true
        }
        "background" => set(&mut computed.background_color, values::parse_color(value)),
        "font-weight" => set(&mut computed.font_weight, values::parse_font_weight(value)),
        "font-style" => set(&mut computed.font_style, values::parse_font_style(value)),
        "text-align" => set(&mut computed.text_align, values::parse_text_align(value)),
        "margin" => set(&mut computed.margin, values::parse_edges(value)),
        "margin-top" => set(&mut computed.margin.top, values::parse_length(value)),
        "margin-right" => set(&mut computed.margin.right, values::parse_length(value)),
        "margin-bottom" => set(&mut computed.margin.bottom, values::parse_length(value)),
        "margin-left" => set(&mut computed.margin.left, values::parse_length(value)),
        "padding" => set(&mut computed.padding, values::parse_edges(value)),
        "padding-top" => set(&mut computed.padding.top, values::parse_length(value)),
        "padding-right" => set(&mut computed.padding.right, values::parse_length(value)),
        "padding-bottom" => set(&mut computed.padding.bottom, values::parse_length(value)),
        "padding-left" => set(&mut computed.padding.left, values::parse_length(value)),
        "border-width" => set(&mut computed.border, values::parse_edges(value)),
        "border-top-width" => set(&mut computed.border.top, values::parse_length(value)),
        "border-right-width" => set(&mut computed.border.right, values::parse_length(value)),
        "border-bottom-width" => set(&mut computed.border.bottom, values::parse_length(value)),
        "border-left-width" => set(&mut computed.border.left, values::parse_length(value)),
        // `border: 1px solid red` — take the first length token; style/colour
        // are ignored until paint needs them. Invalid if no length appears.
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            apply_border_shorthand(computed, declaration.name.as_str(), value)
        }
        "width" => set(&mut computed.width, values::parse_width(value)),
        "max-width" => set(&mut computed.max_width, values::parse_width(value)),
        // M9.2. `parse_width` is the right parser for all of these: it maps
        // `none` (the initial `max-*` value as pages write it) to `Auto`,
        // which is how layout spells "no clamp".
        "min-width" => set(&mut computed.min_width, values::parse_width(value)),
        "height" => set(&mut computed.height, values::parse_width(value)),
        "min-height" => set(&mut computed.min_height, values::parse_width(value)),
        "max-height" => set(&mut computed.max_height, values::parse_width(value)),
        "box-sizing" => set(&mut computed.box_sizing, values::parse_box_sizing(value)),
        // M9.3. The shorthand takes one or two values (`overflow: hidden auto`
        // is x then y); either component being invalid drops the whole
        // declaration, so a half-applied `overflow` cannot clip one axis by
        // accident.
        "overflow" => apply_overflow_shorthand(computed, value),
        "overflow-x" => set(&mut computed.overflow_x, values::parse_overflow(value)),
        "overflow-y" => set(&mut computed.overflow_y, values::parse_overflow(value)),
        // M9.5's vocabulary. The shorthands expand here rather than in the
        // parser so a longhand later in the cascade still overrides the piece
        // of them it names — `flex: none; flex-grow: 3` is grow 3.
        "flex-direction" => set(
            &mut computed.flex_direction,
            values::parse_flex_direction(value),
        ),
        "flex-wrap" => set(&mut computed.flex_wrap, values::parse_flex_wrap(value)),
        "flex-flow" => apply_flex_flow(computed, value),
        "justify-content" => set(
            &mut computed.justify_content,
            values::parse_justify_content(value),
        ),
        "align-items" => set(&mut computed.align_items, values::parse_align_items(value)),
        "align-self" => set(&mut computed.align_self, values::parse_align_self(value)),
        "align-content" => set(
            &mut computed.align_content,
            values::parse_align_content(value),
        ),
        "gap" => set(&mut computed.gap, values::parse_gaps(value)),
        "row-gap" => set(&mut computed.gap.row, values::parse_gap(value)),
        "column-gap" => set(&mut computed.gap.column, values::parse_gap(value)),
        "flex" => set(&mut computed.flex, values::parse_flex(value)),
        "flex-grow" => set(&mut computed.flex.grow, values::parse_flex_factor(value)),
        "flex-shrink" => set(&mut computed.flex.shrink, values::parse_flex_factor(value)),
        "flex-basis" => set(&mut computed.flex.basis, values::parse_flex_basis(value)),
        "order" => set(&mut computed.order, values::parse_order(value)),
        "grid-template-columns" => set(
            &mut computed.grid_template_columns,
            values::parse_grid_tracks(value),
        ),
        "grid-template-rows" => set(
            &mut computed.grid_template_rows,
            values::parse_grid_tracks(value),
        ),
        "grid-column" => set(&mut computed.grid_column, values::parse_grid_area(value)),
        "grid-row" => set(&mut computed.grid_row, values::parse_grid_area(value)),
        "grid-column-start" => set(
            &mut computed.grid_column.0,
            values::parse_grid_placement(value),
        ),
        "grid-column-end" => set(
            &mut computed.grid_column.1,
            values::parse_grid_placement(value),
        ),
        "grid-row-start" => set(
            &mut computed.grid_row.0,
            values::parse_grid_placement(value),
        ),
        "grid-row-end" => set(
            &mut computed.grid_row.1,
            values::parse_grid_placement(value),
        ),
        "grid-auto-flow" => set(
            &mut computed.grid_auto_flow,
            values::parse_grid_auto_flow(value),
        ),
        "text-decoration" | "text-decoration-line" => set(
            &mut computed.underline,
            values::parse_text_decoration(value),
        ),
        _ => false,
    }
}

/// `overflow: <a>` sets both axes; `overflow: <a> <b>` sets x then y.
fn apply_overflow_shorthand(computed: &mut ComputedStyle, value: &str) -> bool {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let (x, y) = match parts.len() {
        1 => {
            let a = values::parse_overflow(parts[0]);
            (a, a)
        }
        2 => (
            values::parse_overflow(parts[0]),
            values::parse_overflow(parts[1]),
        ),
        _ => return false,
    };
    let (Some(x), Some(y)) = (x, y) else {
        return false;
    };
    computed.overflow_x = x;
    computed.overflow_y = y;
    true
}

/// `flex-flow: <direction> || <wrap>` — either component, in either order,
/// with the one that is absent reset to its initial value (that is what a
/// shorthand does, and it is why `flex-flow: wrap` un-reverses a direction an
/// earlier rule set). A token that is neither drops the whole declaration.
fn apply_flex_flow(computed: &mut ComputedStyle, value: &str) -> bool {
    let (mut direction, mut wrap) = (None, None);
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.is_empty() || parts.len() > 2 {
        return false;
    }
    for part in parts {
        if direction.is_none()
            && let Some(d) = values::parse_flex_direction(part)
        {
            direction = Some(d);
            continue;
        }
        if wrap.is_none()
            && let Some(w) = values::parse_flex_wrap(part)
        {
            wrap = Some(w);
            continue;
        }
        return false;
    }
    computed.flex_direction = direction.unwrap_or_default();
    computed.flex_wrap = wrap.unwrap_or_default();
    true
}

/// Pull the first parseable length out of a `border` / `border-*` shorthand
/// and apply it as a width. Style and colour tokens are skipped — paint will
/// own those later; until then a border is "how many cells thick".
fn apply_border_shorthand(computed: &mut ComputedStyle, name: &str, value: &str) -> bool {
    let mut width = None;
    for token in value.split_whitespace() {
        if let Some(len) = values::parse_length(token) {
            width = Some(len);
            break;
        }
    }
    let Some(width) = width else {
        return false;
    };
    match name {
        "border" => {
            computed.border = Edges::all(width);
            true
        }
        "border-top" => set(&mut computed.border.top, Some(width)),
        "border-right" => set(&mut computed.border.right, Some(width)),
        "border-bottom" => set(&mut computed.border.bottom, Some(width)),
        "border-left" => set(&mut computed.border.left, Some(width)),
        _ => false,
    }
}

/// `true` when the value parsed and was applied.
fn set<T>(slot: &mut T, parsed: Option<T>) -> bool {
    match parsed {
        Some(value) => {
            *slot = value;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::values::*;
    use super::*;
    use crate::html;

    /// Style `html_src` with `css_src` as its only author sheet.
    fn styled(html_src: &str, css_src: &str) -> (Dom, Styles) {
        let dom = html::parse(html_src);
        let sheets = [css::parse(css_src)];
        let styles = style_tree(&dom, &sheets.iter().collect::<Vec<_>>());
        (dom, styles)
    }

    fn find(dom: &Dom, tag: &str) -> NodeId {
        fn walk(dom: &Dom, id: NodeId, tag: &str) -> Option<NodeId> {
            if matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag) {
                return Some(id);
            }
            dom.children(id).find_map(|child| walk(dom, child, tag))
        }
        walk(dom, dom.root, tag).expect("fixture is missing that tag")
    }

    fn color_of(html_src: &str, css_src: &str, tag: &str) -> ColorValue {
        let (dom, styles) = styled(html_src, css_src);
        styles.get(find(&dom, tag)).color
    }

    const RED: ColorValue = ColorValue::Rgb(255, 0, 0);
    const BLUE: ColorValue = ColorValue::Rgb(0, 0, 255);

    #[test]
    fn higher_specificity_wins_regardless_of_order() {
        assert_eq!(
            color_of(
                "<p class='x'>t</p>",
                ".x { color: red } p { color: blue }",
                "p"
            ),
            RED
        );
    }

    #[test]
    fn equal_specificity_gives_it_to_the_later_rule() {
        assert_eq!(
            color_of("<p>t</p>", "p { color: blue } p { color: red }", "p"),
            RED
        );
    }

    #[test]
    fn a_style_attribute_beats_an_id_rule() {
        assert_eq!(
            color_of(
                "<p id='a' style='color: red'>t</p>",
                "#a { color: blue }",
                "p"
            ),
            RED
        );
    }

    #[test]
    fn important_in_a_page_sheet_beats_a_style_attribute() {
        assert_eq!(
            color_of(
                "<p style='color: blue'>t</p>",
                "p { color: red !important }",
                "p"
            ),
            RED
        );
        // ...and an important style attribute takes it back.
        assert_eq!(
            color_of(
                "<p style='color: red !important'>t</p>",
                "p { color: blue !important }",
                "p"
            ),
            RED
        );
    }

    #[test]
    fn the_ua_sheet_loses_to_any_author_rule() {
        // The UA sheet says links are #5c5cff; a page rule of specificity
        // (0,0,1) still wins, because origin outranks specificity.
        assert_eq!(color_of("<a href='/x'>t</a>", "a { color: red }", "a"), RED);
    }

    #[test]
    fn an_invalid_value_drops_only_itself() {
        // `color: bananas` must not resolve to anything — blue stands.
        assert_eq!(
            color_of("<p>t</p>", "p { color: blue } p { color: bananas }", "p"),
            BLUE
        );
    }

    #[test]
    fn inheritance_reaches_a_grandchild_but_backgrounds_do_not() {
        let (dom, styles) = styled(
            "<div><section><em>t</em></section></div>",
            "div { color: red; background-color: blue }",
        );
        let em = styles.get(find(&dom, "em"));
        assert_eq!(em.color, RED);
        assert_eq!(em.font_style, FontStyle::Italic); // from the UA sheet
        assert_eq!(em.background_color, ColorValue::Default);
        // The div keeps its own background; only inheritance was blocked.
        assert_eq!(styles.get(find(&dom, "div")).background_color, BLUE);
    }

    #[test]
    fn a_text_node_carries_its_parents_computed_colour() {
        // Paint styles a text run by asking the text node itself, so text
        // nodes need real values rather than a default slot.
        let (dom, styles) = styled("<p>hello</p>", "p { color: red }");
        let p = find(&dom, "p");
        let text = dom.children(p).next().unwrap();
        assert!(matches!(dom.node(text).data, NodeData::Text(_)));
        assert_eq!(styles.get(text).color, RED);
        // Text is not a block, whatever its parent is.
        assert_eq!(styles.get(text).display, Display::Inline);
        assert_eq!(styles.get(p).display, Display::Block);
    }

    #[test]
    fn display_none_does_not_stop_descendants_computing() {
        // Skipping the subtree is layout's job (M4.4). Style still resolves it,
        // which is what lets `display:none` be flipped later without a restyle
        // of the whole document.
        let (dom, styles) = styled(
            "<div style='display: none'><p>t</p></div>",
            "p { color: red }",
        );
        assert_eq!(styles.get(find(&dom, "div")).display, Display::None);
        assert_eq!(styles.get(find(&dom, "p")).display, Display::Block);
        assert_eq!(styles.get(find(&dom, "p")).color, RED);
    }

    #[test]
    fn the_ua_sheet_parses_with_nothing_dropped() {
        let sheet = ua_stylesheet();
        // One rule per `{` in the file, comments excluded — the sheet documents
        // itself in CSS comments, and one of those contains a brace. Counting
        // rather than pinning a number means this keeps biting as the sheet
        // grows: a rule dropped for syntax the parser dislikes shows up here as
        // an off-by-one instead of as a silently unstyled tag.
        let blocks = strip_comments(include_str!("ua.css")).matches('{').count();
        assert_eq!(sheet.rules.len(), blocks);
        assert!(blocks >= 6);
        // A rule that parsed but lost all its declarations would style nothing.
        assert!(
            sheet
                .rules
                .iter()
                .all(|r| !r.declarations.is_empty() && !r.selectors.is_empty())
        );
    }

    fn strip_comments(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some((before, after)) = rest.split_once("/*") {
            out.push_str(before);
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn bare_html_gets_its_user_agent_styling() {
        let (dom, styles) = styled(
            "<h1>Title</h1><p>t</p><a href='/x'>link</a><script>var x = 1</script>",
            "",
        );
        let h1 = styles.get(find(&dom, "h1"));
        assert_eq!(h1.display, Display::Block);
        assert_eq!(h1.font_weight, FontWeight::Bold);
        // Everything the sheet does not mention stays initial.
        assert_eq!(h1.color, ColorValue::Default);
        assert_eq!(h1.text_align, TextAlign::Left);
        // M5.1: the UA sheet's vertical rhythm lands as real margins.
        assert_eq!(h1.margin.top, Length::Em(1.0));
        assert_eq!(h1.margin.bottom, Length::Em(1.0));
        assert_eq!(h1.margin.left, Length::Zero);
        assert_eq!(styles.get(find(&dom, "p")).margin.top, Length::Em(1.0));

        let a = styles.get(find(&dom, "a"));
        assert!(a.underline);
        // ANSI 12's RGB, so M4.4's nearest-256 map lands on the colour M3 draws.
        assert_eq!(a.color, ColorValue::Rgb(0x5c, 0x5c, 0xff));
        assert_eq!(a.display, Display::Inline);

        assert_eq!(styles.get(find(&dom, "script")).display, Display::None);
        assert_eq!(styles.get(find(&dom, "head")).display, Display::None);
    }

    #[test]
    fn sizing_properties_cascade_and_do_not_inherit() {
        // M9.2's vocabulary. `box-sizing` is the one pages set on everything
        // at once — through `*`, not through inheritance, which is why the
        // child below must come back `content-box` while the `*` rule that
        // matches it makes it `border-box`.
        let (dom, styles) = styled(
            "<div><p>t</p></div>",
            "div { height: 20em; min-width: 10em; min-height: 2em; max-height: 30em;
                   box-sizing: border-box }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.height, Length::Em(20.0));
        assert_eq!(div.min_width, Length::Em(10.0));
        assert_eq!(div.min_height, Length::Em(2.0));
        assert_eq!(div.max_height, Length::Em(30.0));
        assert_eq!(div.box_sizing, BoxSizing::BorderBox);

        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.height, Length::Auto);
        assert_eq!(p.min_height, Length::Auto);
        assert_eq!(
            p.box_sizing,
            BoxSizing::ContentBox,
            "box-sizing must not inherit"
        );

        // The way real pages turn it on, and the reason it must not inherit:
        // the universal selector already reaches every element.
        let (dom, styles) = styled(
            "<div><p>t</p></div>",
            "*, *::before, *::after { box-sizing: border-box }",
        );
        for tag in ["div", "p"] {
            assert_eq!(
                styles.get(find(&dom, tag)).box_sizing,
                BoxSizing::BorderBox,
                "`*` must reach <{tag}>"
            );
        }

        // `max-height: none` is how a page spells "no clamp"; layout spells
        // the same thing `Auto`.
        let (dom, styles) = styled("<div>t</div>", "div { max-height: none }");
        assert_eq!(styles.get(find(&dom, "div")).max_height, Length::Auto);
    }

    #[test]
    fn positioning_properties_cascade_and_do_not_inherit() {
        let (dom, styles) = styled(
            "<div><p>t</p></div>",
            "div { position: relative; top: 2em; right: 25%; bottom: auto; left: 3px }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.position, Position::Relative);
        assert_eq!(div.top, Length::Em(2.0));
        assert_eq!(div.right, Length::Percent(25.0));
        assert_eq!(div.left, Length::Px(3.0));
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.position, Position::Static);
        assert_eq!(p.top, Length::Auto);

        // An unsupported value is invalid, so the prior winner survives.
        let (dom, styles) = styled("<div>t</div>", "div { position: fixed; position: fixedly }");
        assert_eq!(styles.get(find(&dom, "div")).position, Position::Fixed);
    }

    #[test]
    fn grid_properties_do_not_inherit_and_invalid_values_keep_the_winner() {
        let (dom, styles) = styled(
            "<div><p>t</p></div>",
            "div { display: grid; grid-template-columns: 10px 1fr; grid-template-rows: auto 2em; grid-column: 2 / span 1; grid-row: 1 / 2; grid-auto-flow: row; gap: 1em }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.display, Display::Grid);
        assert_eq!(div.grid_template_columns.as_slice().len(), 2);
        assert_eq!(div.grid_template_rows.as_slice().len(), 2);
        assert_eq!(
            div.grid_column,
            (GridPlacement::Line(2), GridPlacement::Span(1))
        );
        assert_eq!(
            div.grid_row,
            (GridPlacement::Line(1), GridPlacement::Line(2))
        );
        assert_eq!(div.grid_auto_flow, GridAutoFlow::Row);
        let child = styles.get(find(&dom, "p"));
        assert!(child.grid_template_columns.is_empty());
        assert_eq!(
            child.grid_column,
            (GridPlacement::Auto, GridPlacement::Auto)
        );
        assert_eq!(child.gap, Gaps::default());

        let (dom, styles) = styled(
            "<div>t</div>",
            "div { grid-template-columns: 1fr; grid-template-columns: repeat(0, 1fr); grid-column-start: 2; grid-column-start: -1 }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.grid_template_columns.as_slice().len(), 1);
        assert_eq!(div.grid_column.0, GridPlacement::Line(2));
    }

    #[test]
    fn overflow_is_two_axes_and_does_not_inherit() {
        let (dom, styles) = styled("<div><p>t</p></div>", "div { overflow: hidden }");
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.overflow_x, Overflow::Hidden);
        assert_eq!(div.overflow_y, Overflow::Hidden);
        // A paragraph inside a clipped menu is not itself a clipping box.
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.overflow_x, Overflow::Visible);
        assert_eq!(p.overflow_y, Overflow::Visible);

        // Two values are x then y.
        let (dom, styles) = styled("<div>t</div>", "div { overflow: hidden auto }");
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.overflow_x, Overflow::Hidden);
        assert_eq!(div.overflow_y, Overflow::Auto);

        // Longhands stand on their own, and one bad component drops the whole
        // shorthand rather than clipping an axis the page never asked to clip.
        let (dom, styles) = styled(
            "<div>t</div>",
            "div { overflow-y: scroll } div { overflow: hidden bananas }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.overflow_x, Overflow::Visible);
        assert_eq!(div.overflow_y, Overflow::Scroll);
    }

    #[test]
    fn the_flex_vocabulary_cascades_and_does_not_inherit() {
        let (dom, styles) = styled(
            "<div><p>t</p></div>",
            "div { display: flex; flex-direction: column; flex-wrap: wrap-reverse;
                   justify-content: space-between; align-items: center;
                   align-content: flex-end; gap: 1em 2em;
                   flex: 2 3 20px; align-self: flex-start; order: -1 }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.display, Display::Flex);
        assert_eq!(div.flex_direction, FlexDirection::Column);
        assert_eq!(div.flex_wrap, FlexWrap::WrapReverse);
        assert_eq!(div.justify_content, JustifyContent::SpaceBetween);
        assert_eq!(div.align_items, AlignItems::Center);
        assert_eq!(div.align_content, AlignContent::FlexEnd);
        assert_eq!(div.gap.row, Length::Em(1.0));
        assert_eq!(div.gap.column, Length::Em(2.0));
        assert_eq!(div.flex.grow, 2.0);
        assert_eq!(div.flex.shrink, 3.0);
        assert_eq!(div.flex.basis, FlexBasis::Size(Length::Px(20.0)));
        assert_eq!(div.align_self, AlignSelf::Items(AlignItems::FlexStart));
        assert_eq!(div.order, -1);

        // A child of a flex container is not itself one, and asks nothing of
        // its own children: every one of these is back at its initial value.
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.display, Display::Block);
        assert_eq!(p.flex_direction, FlexDirection::Row);
        assert_eq!(p.flex_wrap, FlexWrap::NoWrap);
        assert_eq!(p.justify_content, JustifyContent::FlexStart);
        assert_eq!(p.align_items, AlignItems::Stretch);
        assert_eq!(p.align_content, AlignContent::Stretch);
        assert_eq!(p.gap, Gaps::default());
        assert_eq!(p.flex, Flex::default());
        assert_eq!(p.align_self, AlignSelf::Auto);
        assert_eq!(p.order, 0);
    }

    #[test]
    fn a_longhand_after_a_shorthand_still_wins() {
        // The shorthands expand at cascade time, in cascade order, so a
        // longhand that comes later overrides the piece of them it names —
        // and one that comes earlier does not.
        let (dom, styles) = styled(
            "<div>t</div>",
            "div { flex: none; flex-grow: 3 }
             div { flex-basis: 4em; flex: 1 }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.flex.grow, 1.0, "the second `flex` reset grow");
        assert_eq!(div.flex.shrink, 1.0);
        assert_eq!(
            div.flex.basis,
            FlexBasis::Size(Length::Zero),
            "`flex: 1` overrides the flex-basis written before it"
        );

        // The other order, on one element, for each shorthand.
        let (dom, styles) = styled(
            "<div>t</div>",
            "div { flex: none; flex-grow: 3;
                   flex-flow: column wrap; flex-direction: row-reverse;
                   gap: 1em; row-gap: 3em; column-gap: 4em }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.flex.grow, 3.0);
        assert_eq!(div.flex.shrink, 0.0, "the rest of `flex: none` survives");
        assert_eq!(div.flex_direction, FlexDirection::RowReverse);
        assert_eq!(div.flex_wrap, FlexWrap::Wrap, "from `flex-flow`");
        assert_eq!(div.gap.row, Length::Em(3.0));
        assert_eq!(div.gap.column, Length::Em(4.0));
    }

    #[test]
    fn flex_flow_resets_the_component_it_omits() {
        // That is what a shorthand does: `flex-flow: wrap` says nothing about
        // the direction, so the direction goes back to its initial value
        // rather than keeping what an earlier rule set.
        let (dom, styles) = styled(
            "<div>t</div>",
            "div { flex-direction: column } div { flex-flow: wrap }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.flex_direction, FlexDirection::Row);
        assert_eq!(div.flex_wrap, FlexWrap::Wrap);

        // Either order of the two components, and a junk token drops the whole
        // declaration rather than half-applying it.
        let (dom, styles) = styled(
            "<div>t</div>",
            "div { flex-flow: wrap-reverse column-reverse }
             div { flex-flow: row bananas }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.flex_direction, FlexDirection::ColumnReverse);
        assert_eq!(div.flex_wrap, FlexWrap::WrapReverse);
    }

    #[test]
    fn an_invalid_flex_value_leaves_the_previous_winner_standing() {
        let (dom, styles) = styled(
            "<div>t</div>",
            "div { flex-grow: 2; justify-content: center; gap: 1em; order: 4 }
             div { flex-grow: -1; justify-content: middle; gap: -1em; order: 1.5 }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.flex.grow, 2.0);
        assert_eq!(div.justify_content, JustifyContent::Center);
        assert_eq!(div.gap.row, Length::Em(1.0));
        assert_eq!(div.order, 4);
    }

    #[test]
    fn box_model_properties_cascade_and_do_not_inherit() {
        let (dom, styles) = styled(
            "<div><p>t</p></div>",
            "div { margin: 1em; padding: 8px; border: 1px solid red; width: 50%; max-width: 40em }",
        );
        let div = styles.get(find(&dom, "div"));
        assert_eq!(div.margin, Edges::all(Length::Em(1.0)));
        assert_eq!(div.padding, Edges::all(Length::Px(8.0)));
        assert_eq!(div.border, Edges::all(Length::Px(1.0)));
        assert_eq!(div.width, Length::Percent(50.0));
        assert_eq!(div.max_width, Length::Em(40.0));
        // Children start at zero — box props never inherit.
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.padding, Edges::ZERO);
        assert_eq!(p.border, Edges::ZERO);
        assert_eq!(p.width, Length::Auto);
        // `p` still gets its own UA margin, not the div's.
        assert_eq!(p.margin.top, Length::Em(1.0));
        assert_ne!(p.margin, div.margin);

        // Longhands and an invalid token: only the bad declaration drops.
        let (dom, styles) = styled(
            "<p>t</p>",
            "p { margin-left: 2em; margin-right: bananas; width: 10px }",
        );
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.margin.left, Length::Em(2.0));
        assert_eq!(p.margin.right, Length::Zero); // bananas dropped
        assert_eq!(p.width, Length::Px(10.0));
    }

    #[test]
    fn a_page_cannot_unhide_the_elements_that_hold_code() {
        // The UA sheet's `!important` — the top cascade rank, and the reason it
        // exists. A browser lets `script { display: block }` print the source;
        // a reader-first browser must not, whether the page means it or the
        // engine mis-parsed a selector into it.
        let (dom, styles) = styled(
            "<body><script>var x = 1</script><p>text</p></body>",
            "script, head, style { display: block !important }",
        );
        assert_eq!(styles.get(find(&dom, "script")).display, Display::None);
        assert_eq!(styles.get(find(&dom, "head")).display, Display::None);
        // Everything else the UA sheet says stays overridable, as it should be.
        let (dom, styles) = styled("<p>t</p>", "p { display: inline }");
        assert_eq!(styles.get(find(&dom, "p")).display, Display::Inline);
    }

    #[test]
    fn noscript_content_is_hidden_now_that_scripts_run() {
        // The flip, M10.2. This test said the opposite until scripts ran: a
        // browser hides <noscript> *because* it runs scripts, and the only
        // reason yata showed it was that it did not. Showing it now would put
        // "please enable JavaScript" on a page whose script has already run.
        let (dom, styles) = styled("<noscript><p>No JS? Fine.</p></noscript>", "");
        assert_eq!(styles.get(find(&dom, "noscript")).display, Display::None);
        // And it is not overridable, like the rest of that rule: a page cannot
        // style its own fallback back into view.
        let (dom, styles) = styled(
            "<noscript><p>No JS? Fine.</p></noscript>",
            "noscript { display: block }",
        );
        assert_eq!(styles.get(find(&dom, "noscript")).display, Display::None);
    }

    #[test]
    fn the_background_shorthand_can_reset_as_well_as_set() {
        // `background: none` is the shorthand resetting to its initial value.
        // Dropping it as "not a colour" left the old colour standing, which is
        // the one thing a reset must not do.
        let (dom, styles) = styled(
            "<p>x</p>",
            "p { background-color: red } p { background: none }",
        );
        assert_eq!(
            styles.get(find(&dom, "p")).background_color,
            ColorValue::Default
        );
    }

    #[test]
    fn a_bare_anchor_is_not_a_link() {
        let (dom, styles) = styled("<a name='top'>t</a>", "");
        assert!(!styles.get(find(&dom, "a")).underline);
    }

    #[test]
    fn junk_in_a_style_attribute_is_survivable() {
        let (dom, styles) = styled(
            "<p style='color; ;; font-weight: bold; color: red'>t</p>",
            "",
        );
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.font_weight, FontWeight::Bold);
        assert_eq!(p.color, RED);
    }
}

/// Ladder proof: run the real stage over the committed fixtures. Two things
/// are checked here that no hand-built document can check — that the rule
/// index agrees with the naive matcher on pages with thousands of rules, and
/// that the cascade produces the values these pages are supposed to show.
#[cfg(test)]
mod ladder {
    use super::*;
    use crate::html;
    use crate::style::matching::RuleIndex;
    use crate::style::values::{ColorValue, Display, FontWeight};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    /// Every `<style>` block in the page. Reaching them through the DOM is a
    /// stand-in until M4.3 gives stylesheet collection a real home.
    fn author_sheets(dom: &Dom) -> Vec<Stylesheet> {
        fn collect(dom: &Dom, id: NodeId, out: &mut Vec<Stylesheet>) {
            if matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "style") {
                let mut text = String::new();
                for child in dom.children(id) {
                    if let NodeData::Text(t) = &dom.node(child).data {
                        text.push_str(t);
                    }
                }
                out.push(css::parse(&text));
                return;
            }
            for child in dom.children(id) {
                collect(dom, child, out);
            }
        }
        let mut out = Vec::new();
        collect(dom, dom.root, &mut out);
        out
    }

    fn elements(dom: &Dom) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![dom.root];
        while let Some(id) = stack.pop() {
            if matches!(dom.node(id).data, NodeData::Element { .. }) {
                out.push(id);
            }
            stack.extend(dom.children(id));
        }
        out
    }

    fn find(dom: &Dom, tag: &str) -> NodeId {
        elements(dom)
            .into_iter()
            .filter(
                |&id| matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag),
            )
            .min_by_key(|id| id.0)
            .expect("fixture is missing that tag")
    }

    /// The index and the naive matcher must agree on every element of the page,
    /// against the UA sheet plus the page's own. A rule index that is fast
    /// because it quietly misses rules is not an optimization, and M4.5's
    /// bench of the two is only meaningful if they compute the same answer.
    fn index_agrees_with_oracle(source: &str) -> Dom {
        let dom = html::parse(source);
        let mut sheets = vec![ua_stylesheet()];
        let author = author_sheets(&dom);
        sheets.extend(author.iter());
        let index = RuleIndex::build(&sheets);
        let ctx = StyleContext::default();
        let mut matched = 0;
        for node in elements(&dom) {
            let fast: Vec<usize> = index
                .matches(&dom, node, &ctx)
                .iter()
                .map(|c| c.order)
                .collect();
            let naive: Vec<usize> = index
                .matches_naive(&dom, node, &ctx)
                .iter()
                .map(|c| c.order)
                .collect();
            assert_eq!(fast, naive, "node {node:?}");
            matched += fast.len();
        }
        // A page where nothing matched would pass the comparison above while
        // proving nothing about the buckets.
        assert!(matched > 0, "no rule matched anything");
        dom
    }

    fn styled(source: &str) -> (Dom, Styles) {
        let dom = html::parse(source);
        let sheets = author_sheets(&dom);
        let styles = style_tree(&dom, &sheets.iter().collect::<Vec<_>>());
        (dom, styles)
    }

    #[test]
    fn example_com_shows_its_own_colours() {
        let dom = index_agrees_with_oracle(fixture!("example.com.html"));
        let sheets = author_sheets(&dom);
        let styles = style_tree(&dom, &sheets.iter().collect::<Vec<_>>());
        // `body{background:#eee}` — the shorthand, honoured because the whole
        // value is a colour.
        assert_eq!(
            styles.get(find(&dom, "body")).background_color,
            ColorValue::Rgb(0xee, 0xee, 0xee)
        );
        // `a:link{color:#348}` beats the UA sheet's link colour, and the UA
        // sheet's underline survives because the page never mentions it.
        let a = styles.get(find(&dom, "a"));
        assert_eq!(a.color, ColorValue::Rgb(0x33, 0x44, 0x88));
        assert!(a.underline);
    }

    #[test]
    fn motherfuckingwebsite_com_is_pure_user_agent_styling() {
        let (dom, styles) = styled(fixture!("motherfuckingwebsite.com.html"));
        // The page ships no CSS at all: everything it looks like comes from
        // ua.css, which is the point of having one.
        let h1 = styles.get(find(&dom, "h1"));
        assert_eq!(h1.font_weight, FontWeight::Bold);
        assert_eq!(h1.display, Display::Block);
        assert_eq!(styles.get(find(&dom, "p")).display, Display::Block);
    }

    #[test]
    fn danluu_com_keeps_its_links_and_its_flex_list() {
        let dom = index_agrees_with_oracle(fixture!("danluu.com.html"));
        let styles = style_tree(&dom, &author_sheets(&dom).iter().collect::<Vec<_>>());
        let a = styles.get(find(&dom, "a"));
        assert!(a.underline);
        assert_eq!(a.color, ColorValue::Rgb(0x5c, 0x5c, 0xff));
        // `li{display:flex}` from the page's own sheet. Until M9.5 this
        // cascaded to `Block`, because that was the nearest mode that stacked;
        // now it cascades to what the page actually wrote, and layout is the
        // stage that treats it as a block container (`engine::is_block_level`)
        // until M9.6. The list stays stacked either way — that is what makes
        // this a vocabulary change and not a rendering one.
        assert_eq!(styles.get(find(&dom, "li")).display, Display::Flex);
        // ...and the page's `.np` nav, which is where its flex *properties*
        // are: `flex-direction: row; justify-content: space-between`.
        let np = elements(&dom)
            .into_iter()
            .find(|&id| dom.attr(id, "class").is_some_and(|c| c == "np"))
            .expect("danluu.com has a .np nav");
        let np = styles.get(np);
        assert_eq!(np.display, Display::Flex);
        assert_eq!(np.flex_direction, values::FlexDirection::Row);
        assert_eq!(
            np.justify_content,
            values::JustifyContent::SpaceBetween,
            "the one real justify-content on the ladder"
        );
    }

    #[test]
    fn news_ycombinator_com_resolves_without_its_external_sheet() {
        // HN's styling lives in news.css, which M4.3 will fetch; today the page
        // is UA-styled only, and must still come out sane rather than empty.
        let dom = index_agrees_with_oracle(fixture!("news.ycombinator.com.html"));
        let styles = style_tree(&dom, &author_sheets(&dom).iter().collect::<Vec<_>>());
        assert_eq!(styles.get(find(&dom, "table")).display, Display::Block);
        assert!(styles.get(find(&dom, "a")).underline);
    }

    #[test]
    fn en_wikipedia_org_styles_every_node() {
        let dom = index_agrees_with_oracle(fixture!("en.wikipedia.org.html"));
        let styles = style_tree(&dom, &author_sheets(&dom).iter().collect::<Vec<_>>());
        // 1.5 MB of real markup, 21 inline sheets and 222 style attributes:
        // every node gets a slot, and the ones the page styles get its values.
        assert_eq!(styles.computed.len(), dom.node_count());
        assert_eq!(styles.get(find(&dom, "h1")).font_weight, FontWeight::Bold);
        assert_eq!(styles.get(find(&dom, "head")).display, Display::None);
    }

    /// The first `<a href>` inside an element carrying `class`, in document
    /// order. HN styles links differently depending on where they sit, which
    /// is the point of the test below.
    fn first_link_under_class(dom: &Dom, class: &str) -> NodeId {
        fn walk(dom: &Dom, id: NodeId, class: &str, inside: bool) -> Option<NodeId> {
            let inside = inside
                || dom
                    .attr(id, "class")
                    .is_some_and(|c| c.split_whitespace().any(|t| t == class));
            if inside
                && matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "a")
                && dom.attr(id, "href").is_some()
            {
                return Some(id);
            }
            dom.children(id)
                .find_map(|child| walk(dom, child, class, inside))
        }
        walk(dom, dom.root, class, false).expect("no link under that class")
    }

    /// The whole chain, offline: HN's markup plus HN's real stylesheet, the
    /// one the page links and M4.3 fetches. Everything before this test styled
    /// pages from inline blocks only.
    #[test]
    fn news_ycombinator_com_styled_by_its_own_linked_sheet() {
        let dom = html::parse(fixture!("news.ycombinator.com.html"));
        // Exactly what a worker would deliver for the page's one <link>.
        let sheet = css::parse(fixture!("news.ycombinator.com.news.css"));
        let styles = style_tree(&dom, &[&sheet]);

        // `a:link { color:#000000; text-decoration:none }` — the page beating
        // the UA sheet on both properties. HN's links really are black and
        // undecorated, and this is the first fixture where an author sheet
        // *removes* UA styling rather than adding to it.
        let story = styles.get(first_link_under_class(&dom, "titleline"));
        assert_eq!(story.color, ColorValue::Rgb(0, 0, 0));
        assert!(!story.underline, "news.css turns the UA underline off");

        // `.subtext a:link { color:#828282 }` beats the bare `a:link` above on
        // specificity — (0,2,1) against (0,1,1) — so two links on the same page
        // come out different colours. Real-page proof that the cascade orders
        // by specificity and not by source position.
        let subtext = styles.get(first_link_under_class(&dom, "subtext"));
        assert_eq!(subtext.color, ColorValue::Rgb(0x82, 0x82, 0x82));

        // `body { color:#828282 }` inherits down to a cell with no rule of its
        // own, through markup the page never styles directly.
        assert_eq!(
            styles.get(find(&dom, "body")).color,
            ColorValue::Rgb(0x82, 0x82, 0x82)
        );

        // The sheet is real-world CSS: 12 @media blocks and attribute
        // selectors that M4 drops on purpose. It must still deliver rules.
        assert!(
            sheet.rules.len() > 20,
            "recovery dropped too much: {} rules",
            sheet.rules.len()
        );
    }
}

/// M11.3's equivalence oracle: a scoped restyle must produce **byte-identical**
/// computed values to a full one, on every page of the ladder and under a
/// seeded fuzz. This is the deliverable the narrowing lives or dies by — a
/// subtree pass that gets one node wrong is a page that silently stops
/// updating, which is exactly the failure M10.6's classification was built to
/// rule out.
#[cfg(test)]
mod scoped {
    use super::*;
    use crate::dom::AttrChanges;
    use crate::html;
    use std::collections::HashSet;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    /// Rules that *react* to the writes below, so the comparison has something
    /// to disagree about: a scoped pass that wrote nothing at all would match a
    /// full one on a page whose sheets never mention the attributes touched.
    ///
    /// Deliberately every shape the subtree argument rests on — inherited and
    /// non-inherited properties, descendant and child combinators, and M11.2's
    /// attribute selectors.
    const PROBE: &str = "
        .x-probe { color: #c00; margin-left: 3px; display: flex }
        .x-probe p { font-weight: bold }
        .x-probe > * { padding-left: 1px }
        body .x-probe span { text-align: right }
        [data-probe] { border-left-width: 2px }
        [data-probe='deep'] em { font-style: normal }
    ";

    fn sheets_for(dom: &Dom) -> Vec<Stylesheet> {
        fn collect(dom: &Dom, id: NodeId, out: &mut Vec<Stylesheet>) {
            if matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "style") {
                let mut text = String::new();
                for child in dom.children(id) {
                    if let NodeData::Text(t) = &dom.node(child).data {
                        text.push_str(t);
                    }
                }
                out.push(css::parse(&text));
                return;
            }
            for child in dom.children(id) {
                collect(dom, child, out);
            }
        }
        let mut out = Vec::new();
        collect(dom, dom.root, &mut out);
        out.push(css::parse(PROBE));
        out
    }

    fn elements(dom: &Dom) -> Vec<NodeId> {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .filter(|&id| matches!(dom.node(id).data, NodeData::Element { .. }))
            .collect()
    }

    fn depth(dom: &Dom, id: NodeId) -> usize {
        let mut depth = 0;
        let mut walk = dom.node(id).parent;
        while let Some(up) = walk {
            depth += 1;
            walk = dom.node(up).parent;
        }
        depth
    }

    /// The four nodes the task names: a shallow one, a deep one, one with
    /// children and one without. They exercise the two halves of the argument
    /// separately — a leaf can only change itself, an ancestor changes a whole
    /// subtree through both inheritance and its descendants' selectors.
    fn sampled_victims(dom: &Dom) -> Vec<NodeId> {
        let els = elements(dom);
        let mut victims = vec![
            *els.iter().min_by_key(|&&id| depth(dom, id)).unwrap(),
            *els.iter().max_by_key(|&&id| depth(dom, id)).unwrap(),
            *els.iter()
                .max_by_key(|&&id| dom.children(id).count())
                .unwrap(),
            *els.iter()
                .find(|&&id| dom.children(id).next().is_none())
                .unwrap(),
        ];
        victims.dedup();
        victims
    }

    /// Every node, compared value by value. `Styles` holds `ComputedStyle`s
    /// that are `PartialEq` in full, so this is the byte-identical claim and
    /// not a claim about the properties someone remembered to check.
    fn assert_identical(scoped: &Styles, full: &Styles, dom: &Dom, what: &str) {
        assert_eq!(scoped.node_count(), full.node_count(), "{what}: tree size");
        for id in (0..full.node_count() as u32).map(NodeId) {
            assert_eq!(
                scoped.get(id),
                full.get(id),
                "{what}: node {} (<{}>) diverged",
                id.0,
                match &dom.node(id).data {
                    NodeData::Element { tag, .. } => tag.as_str(),
                    _ => "non-element",
                }
            );
        }
    }

    /// Apply `write` to `dom`, then compare a scoped restyle of `current`
    /// against a full pass. Returns the full pass, which becomes the next
    /// comparison's starting point — so the writes compound rather than each
    /// starting from a clean page.
    fn compare_after(
        dom: &mut Dom,
        current: &Styles,
        ctx: &StyleContext<'_>,
        what: &str,
    ) -> Styles {
        let sheets = sheets_for(dom);
        let refs: Vec<&Stylesheet> = sheets.iter().collect();

        let AttrChanges::Nodes(roots) = dom.take_attr_changes() else {
            panic!("{what}: the write overflowed the arena's list");
        };
        let mut scoped = current.clone();
        restyle_subtree(dom, &refs, ctx, &mut scoped, &roots);

        let full = style_tree_with(dom, &refs, ctx);
        assert_identical(&scoped, &full, dom, what);
        full
    }

    /// The ladder proof: on a real page, an attribute write on each sampled
    /// node — set, then unset — must leave the scoped tree identical to a full
    /// one. Unset matters as much as set: a subtree that gained a rule and
    /// never gave it back is the other half of the bug.
    fn ladder_page(source: &str, label: &str, pick: impl Fn(&Dom) -> Vec<NodeId>) {
        let mut dom = html::parse(source);
        let sheets = sheets_for(&dom);
        let mut current = style_tree_with(
            &dom,
            &sheets.iter().collect::<Vec<_>>(),
            &StyleContext::default(),
        );
        drop(sheets);

        for victim in pick(&dom) {
            let tag = match &dom.node(victim).data {
                NodeData::Element { tag, .. } => tag.clone(),
                _ => unreachable!("victims are elements"),
            };
            let at = format!("{label} <{tag}> #{}", victim.0);

            dom.set_attr(victim, "class", "x-probe");
            let ctx = StyleContext::default();
            current = compare_after(&mut dom, &current, &ctx, &format!("{at}: class added"));

            dom.set_attr(victim, "data-probe", "deep");
            current = compare_after(&mut dom, &current, &ctx, &format!("{at}: attribute added"));

            dom.remove_attr(victim, "class");
            current = compare_after(&mut dom, &current, &ctx, &format!("{at}: class removed"));

            dom.remove_attr(victim, "data-probe");
            current = compare_after(
                &mut dom,
                &current,
                &ctx,
                &format!("{at}: attribute removed"),
            );
        }
    }

    #[test]
    fn example_com_scoped_restyle_equals_a_full_one() {
        ladder_page(fixture!("example.com.html"), "example.com", sampled_victims);
    }

    #[test]
    fn motherfuckingwebsite_com_scoped_restyle_equals_a_full_one() {
        ladder_page(
            fixture!("motherfuckingwebsite.com.html"),
            "motherfuckingwebsite.com",
            sampled_victims,
        );
    }

    #[test]
    fn danluu_com_scoped_restyle_equals_a_full_one() {
        ladder_page(fixture!("danluu.com.html"), "danluu.com", sampled_victims);
    }

    #[test]
    fn news_ycombinator_com_scoped_restyle_equals_a_full_one() {
        ladder_page(
            fixture!("news.ycombinator.com.html"),
            "news.ycombinator.com",
            sampled_victims,
        );
    }

    #[test]
    fn en_wikipedia_org_scoped_restyle_equals_a_full_one() {
        // The shallow and deep victims only. Every comparison costs a *full*
        // restyle of 25,599 nodes, which is a second of an unoptimized build
        // each; the two victims this page has that no other does are its
        // extremes of depth, and the children/leaf pair is covered four times
        // over above. Trading the redundant half for a `cargo test` that stays
        // in its current wall clock.
        ladder_page(
            fixture!("en.wikipedia.org.html"),
            "en.wikipedia.org",
            |dom| sampled_victims(dom)[..2].to_vec(),
        );
    }

    /// The same generator `dom::mutation_fuzz` uses.
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
    fn mutation_fuzz_keeps_a_scoped_restyle_equal_to_a_full_one() {
        // Shaped like `dom::mutation_fuzz`: random attribute writes on random
        // nodes, comparing scoped against full after each. The cheapest
        // insurance in M11 — the sampled victims above test the cases someone
        // thought of, and this tests the ones nobody did.
        //
        // Several writes per round on purpose: one root is the easy case, and
        // the interesting ones are two roots where one is inside the other's
        // subtree, and a write that lands on a node no walk reaches.
        const ROUNDS: usize = 150;

        let mut dom = html::parse(fixture!("motherfuckingwebsite.com.html"));
        // A node the tree does not contain: a full pass never reaches it, so
        // its slot must stay at the initial values however often it is written.
        let detached = dom.create_element("div", vec![]);
        let sheets = sheets_for(&dom);
        let mut current = style_tree_with(
            &dom,
            &sheets.iter().collect::<Vec<_>>(),
            &StyleContext::default(),
        );
        drop(sheets);
        dom.take_attr_changes();

        let all = elements(&dom);
        let mut rng = Lcg(0x0113);
        for round in 0..ROUNDS {
            for _ in 0..1 + rng.below(3) {
                let victim = match rng.below(16) {
                    0 => detached,
                    _ => all[rng.below(all.len())],
                };
                match rng.below(4) {
                    0 => dom.set_attr(victim, "class", "x-probe"),
                    1 => dom.set_attr(victim, "data-probe", "deep"),
                    2 => dom.remove_attr(victim, "class"),
                    _ => dom.remove_attr(victim, "data-probe"),
                };
            }
            current = compare_after(
                &mut dom,
                &current,
                &StyleContext::default(),
                &format!("fuzz round {round}"),
            );
        }
    }

    #[test]
    fn the_dynamic_matching_inputs_survive_a_scoped_pass() {
        // `:hover`, `:link` and `:visited` are the three matching inputs that
        // do not come from the DOM, and `href` is the attribute two of them
        // read — one a script can write, and one the oracle above never writes
        // because it runs with an empty `StyleContext`. This is the only place
        // they are compared scoped against full.
        //
        // The subtree argument still holds for all three, and this is what
        // says so rather than only arguing it: `:link`/`:visited` read the
        // node's own `href`, and `:hover` reads neither attributes nor
        // anything an attribute write can move.
        let mut dom = html::parse(
            "<nav><a href='/seen'>one</a><a href='/fresh'>two</a></nav>\
             <main><a href='/seen'><span>three</span></a></main>",
        );
        let links: Vec<NodeId> = elements(&dom)
            .into_iter()
            .filter(|&id| matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "a"))
            .collect();
        assert_eq!(links.len(), 3);

        let visited: HashSet<String> = ["http://example.com/seen".to_string()].into();
        let ctx = StyleContext {
            // The hovered node is *outside* the subtrees written to below, so
            // a scoped pass that quietly dropped it would show up here.
            hover: Some(links[0]),
            visited: &visited,
            base_url: Some("http://example.com/page"),
        };

        let sheets = sheets_for(&dom);
        let mut current = style_tree_with(&dom, &sheets.iter().collect::<Vec<_>>(), &ctx);
        drop(sheets);
        dom.take_attr_changes();

        // Each write flips what the node itself matches — unvisited to
        // visited, link to not-a-link, and back.
        for (victim, name, value) in [
            (links[1], "href", "/seen"),
            (links[2], "href", "/fresh"),
            (links[1], "href", "/fresh"),
            (links[2], "class", "x-probe"),
        ] {
            dom.set_attr(victim, name, value);
            current = compare_after(
                &mut dom,
                &current,
                &ctx,
                &format!("set {name} on #{}", victim.0),
            );
        }

        // And removing `href` entirely: the node stops being a link at all,
        // which the UA sheet styles and a descendant rule may depend on.
        dom.remove_attr(links[2], "href");
        compare_after(&mut dom, &current, &ctx, "href removed");
    }

    #[test]
    fn a_scoped_pass_styles_the_subtree_and_not_the_document() {
        // The scope itself, at the stage rather than through `App`: the walk
        // must reach the subtree's nodes and stop.
        let mut dom = html::parse("<div id=a><p><em>deep</em></p></div><div id=b><p>x</p></div>");
        let sheets = [css::parse(PROBE)];
        let refs: Vec<&Stylesheet> = sheets.iter().collect();
        let ctx = StyleContext::default();
        let mut styles = style_tree_with(&dom, &refs, &ctx);
        let whole_document = styles.nodes_styled();
        assert_eq!(whole_document, dom.node_count());

        let em = elements(&dom)
            .into_iter()
            .find(|&id| matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "em"))
            .unwrap();
        dom.set_attr(em, "class", "x-probe");
        let AttrChanges::Nodes(roots) = dom.take_attr_changes() else {
            unreachable!()
        };
        restyle_subtree(&dom, &refs, &ctx, &mut styles, &roots);
        assert_eq!(
            styles.nodes_styled() - whole_document,
            2,
            "restyling <em> must reach <em> and its text node, and nothing else"
        );
    }

    #[test]
    fn a_root_inside_another_roots_subtree_is_not_styled_twice() {
        // Overlapping roots are the case a per-node loop gets quadratic on:
        // <body> and something under it both written to in one tick.
        let mut dom = html::parse("<div id=outer><p id=inner>text</p></div>");
        let sheets = [css::parse(PROBE)];
        let refs: Vec<&Stylesheet> = sheets.iter().collect();
        let ctx = StyleContext::default();
        let mut styles = style_tree_with(&dom, &refs, &ctx);
        let before = styles.nodes_styled();

        let by_id = |dom: &Dom, want: &str| {
            elements(dom)
                .into_iter()
                .find(|&id| dom.attr(id, "id") == Some(want))
                .unwrap()
        };
        let (outer, inner) = (by_id(&dom, "outer"), by_id(&dom, "inner"));
        dom.set_attr(inner, "class", "x-probe");
        dom.set_attr(outer, "class", "x-probe");
        let AttrChanges::Nodes(roots) = dom.take_attr_changes() else {
            unreachable!()
        };
        assert_eq!(roots, vec![inner, outer], "both, in write order");

        restyle_subtree(&dom, &refs, &ctx, &mut styles, &roots);
        assert_eq!(
            styles.nodes_styled() - before,
            3,
            "<div>, <p> and the text once each — the inner root is covered by \
             the outer one"
        );
        // ...and covered means *correct*, not merely skipped: the inner node's
        // values still come out identical to a full pass.
        assert_identical(
            &styles,
            &style_tree_with(&dom, &refs, &ctx),
            &dom,
            "overlapping roots",
        );
    }
}
