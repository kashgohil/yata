//! Property vocabulary: CSS value text → typed values (M4.2, M5.1 lengths).
//!
//! Every parser here is `&str -> Option<T>`, and `None` means *invalid*, which
//! in CSS means the declaration is dropped and whatever won before it keeps
//! standing. That is the difference between an unknown colour name and black:
//! `color: bananas` must leave the previous winner alone, not paint anything.
//!
//! This is where M4.1's deliberate ignorance ends — the parser handed over raw
//! source text, and this module is the first code that knows `red` is a colour.

/// The display modes this engine implements. M4 had three — a line-breaking
/// block, inline text, or gone — and M9.5 adds the fourth: a flex container,
/// which is block-level and lays its children out along an axis. Until M9.6
/// implements that axis, layout treats it as a block (`engine::is_block_level`,
/// the one place that decision is made).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Display {
    Block,
    #[default]
    Inline,
    Flex,
    None,
}

/// A CSS length as written, before layout resolves it into cells.
///
/// Resolution (PLAN.md §1.4) is axis-aware:
/// - horizontal: `8px ≈ 1 cell`, `1em = 2 cells`
/// - vertical: `16px ≈ 1 line`, `1em = 1 line`
/// - `%` is always of the containing block's **width** (CSS 2.1)
/// - nonzero values round to at least 1 cell so a thin border still draws
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Length {
    /// `width` / `max-width` / `margin` initial for some sides.
    #[default]
    Auto,
    /// Explicit zero — distinct from Auto so `margin: 0` wins over Auto.
    Zero,
    Px(f32),
    Em(f32),
    /// Percentage 0–100 (e.g. `50%` stores `50.0`).
    Percent(f32),
}

impl Length {
    /// Resolve to horizontal cells inside a containing block of `cw` cells.
    pub fn to_cells_h(self, containing_width: i32) -> i32 {
        resolve(self, containing_width, Axis::Horizontal)
    }

    /// Resolve to vertical cells (lines). Percentage still uses width.
    pub fn to_cells_v(self, containing_width: i32) -> i32 {
        resolve(self, containing_width, Axis::Vertical)
    }

    /// Resolve to lines against a containing block's **height** (M9.2).
    ///
    /// This is the rule for `height`/`min-height`/`max-height`, and the only
    /// place a percentage does not mean "of the containing width": CSS 2.1
    /// §10.5 resolves those against the containing block's height, while
    /// percentage `padding`/`margin` keep using the width even vertically —
    /// which is what [`to_cells_v`] is for. The caller decides whether the
    /// containing height is definite at all; an indefinite one makes the whole
    /// property behave as `auto`, so it never reaches here.
    pub fn to_lines(self, containing_height: i32) -> i32 {
        match self {
            // Same rounding rule as every other length: nonzero rounds up to
            // at least one line, so `height: 5%` of a 12-line block is a row
            // the reader can see rather than a box that vanished.
            Length::Percent(p) => {
                let raw = (p / 100.0) * containing_height as f32;
                if raw <= 0.0 {
                    0
                } else {
                    raw.round().max(1.0) as i32
                }
            }
            other => resolve(other, 0, Axis::Vertical),
        }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Length::Auto)
    }
}

#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

fn resolve(len: Length, containing_width: i32, axis: Axis) -> i32 {
    let raw = match len {
        Length::Auto | Length::Zero => 0.0,
        Length::Px(px) => match axis {
            // PLAN.md: 8px ≈ 1 cell width, 16px ≈ 1 line height.
            Axis::Horizontal => px / 8.0,
            Axis::Vertical => px / 16.0,
        },
        Length::Em(em) => match axis {
            // PLAN.md: 1em = 2 cells wide × 1 line tall.
            Axis::Horizontal => em * 2.0,
            Axis::Vertical => em,
        },
        Length::Percent(p) => (p / 100.0) * containing_width as f32,
    };
    if raw <= 0.0 {
        0
    } else {
        // Nonzero → at least one cell so thin borders and small margins show.
        raw.round().max(1.0) as i32
    }
}

/// Four sides of the box model (`margin`, `padding`, `border-width`).
/// Initial value is zero on every side (CSS), never `auto` — `auto` is a
/// legal *written* margin value, not the default.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Edges {
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
}

impl Default for Edges {
    fn default() -> Self {
        Edges::ZERO
    }
}

impl Edges {
    pub const ZERO: Edges = Edges {
        top: Length::Zero,
        right: Length::Zero,
        bottom: Length::Zero,
        left: Length::Zero,
    };

    /// All sides the same length.
    pub fn all(len: Length) -> Edges {
        Edges {
            top: len,
            right: len,
            bottom: len,
            left: len,
        }
    }
}

/// A resolved colour, or the terminal's own. `Default` is the initial value for
/// both `color` and `background-color`: a browser's black-on-white initial pair
/// would paint black text into a black terminal, so "whatever the user's
/// terminal uses" is the only correct starting point.
///
/// These are engine colours, not `term::Color`. Mapping to truecolor or nearest
/// ANSI-256 needs the terminal's capabilities, which a pure stage must not see;
/// M4.4 owns that map, the same seam `layout/mod.rs` documents for its link
/// colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ColorValue {
    #[default]
    Default,
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FontWeight {
    #[default]
    Normal,
    Bold,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// `box-sizing` (M9.2): which box `width`/`height` and their min/max clamps
/// describe. Not inherited — pages apply it globally through the universal
/// selector (`*, *::before, *::after { box-sizing: border-box }`), which this
/// engine already matches, not through inheritance.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

pub fn parse_box_sizing(value: &str) -> Option<BoxSizing> {
    match lower(value).as_str() {
        "content-box" => Some(BoxSizing::ContentBox),
        "border-box" => Some(BoxSizing::BorderBox),
        _ => None,
    }
}

/// `overflow` (M9.3): what a box does with content that does not fit inside it.
///
/// The five CSS values are kept apart even though four of them do the same
/// thing here, because F3 prints what the page asked for and "we clipped a
/// `scroll` box" is a different fact from "we clipped a `hidden` one".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Overflow {
    #[default]
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
}

impl Overflow {
    /// Everything but `visible` confines content to the padding box.
    ///
    /// `scroll` and `auto` clip too: a terminal has one scroll position, and
    /// inner scrollers are PLAN.md §M11+. The reader gets the same first
    /// screenful a browser shows and the remainder is unreachable — a
    /// deviation, recorded here rather than hidden.
    pub fn clips(self) -> bool {
        !matches!(self, Overflow::Visible)
    }

    /// The keyword, for `F3`.
    pub fn name(self) -> &'static str {
        match self {
            Overflow::Visible => "visible",
            Overflow::Hidden => "hidden",
            Overflow::Clip => "clip",
            Overflow::Scroll => "scroll",
            Overflow::Auto => "auto",
        }
    }
}

pub fn parse_overflow(value: &str) -> Option<Overflow> {
    match lower(value).as_str() {
        "visible" => Some(Overflow::Visible),
        "hidden" => Some(Overflow::Hidden),
        "clip" => Some(Overflow::Clip),
        "scroll" => Some(Overflow::Scroll),
        "auto" => Some(Overflow::Auto),
        _ => None,
    }
}

/// `display`. Values this engine cannot honour are mapped to the nearest one it
/// can rather than dropped: dropping the declaration would leave a page's
/// `display:grid` container as an inline, which collapses it. Every
/// box-generating mode that stacks becomes `Block`.
pub fn parse_display(value: &str) -> Option<Display> {
    match lower(value).as_str() {
        "none" => Some(Display::None),
        // `inline-flex` is an inline-*level* box with a flex *inner* mode. The
        // inner mode is the half that matters for the children, and it is the
        // half M9.6 implements, so it wins for now: an inline-flex box is
        // block-level here. M9.11 (atomic inlines) is where it becomes a real
        // inline-level box that sits on a line with its siblings.
        "flex" | "inline-flex" => Some(Display::Flex),
        "inline" | "inline-block" | "inline-grid" | "contents" => Some(Display::Inline),
        "block" | "flow-root" | "list-item" | "grid" | "table" | "table-row" | "table-cell"
        | "table-row-group" | "table-header-group" | "table-footer-group" | "table-caption"
        | "inline-table" => Some(Display::Block),
        _ => None,
    }
}

// ---- Flexbox vocabulary (M9.5, css-flexbox-1) -----------------------------
//
// Parsing, cascade and initial values only: nothing below is read by layout
// until M9.6. The point of landing it first is that every later flex task has
// its inputs already computed, and that F2 can show a page's flex properties
// before the engine can honour them.

/// `flex-direction`: which axis is the main axis, and which end items start
/// from. `Row` is the initial value, and the only one M9.6 implements — the
/// column directions arrive in M9.9 and the reversals with them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

impl FlexDirection {
    /// The keyword, for `F2`.
    pub fn name(self) -> &'static str {
        match self {
            FlexDirection::Row => "row",
            FlexDirection::RowReverse => "row-reverse",
            FlexDirection::Column => "column",
            FlexDirection::ColumnReverse => "column-reverse",
        }
    }
}

pub fn parse_flex_direction(value: &str) -> Option<FlexDirection> {
    match lower(value).as_str() {
        "row" => Some(FlexDirection::Row),
        "row-reverse" => Some(FlexDirection::RowReverse),
        "column" => Some(FlexDirection::Column),
        "column-reverse" => Some(FlexDirection::ColumnReverse),
        _ => None,
    }
}

/// `flex-wrap`: whether items that do not fit start a new line (M9.10).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FlexWrap {
    #[default]
    NoWrap,
    Wrap,
    WrapReverse,
}

impl FlexWrap {
    pub fn name(self) -> &'static str {
        match self {
            FlexWrap::NoWrap => "nowrap",
            FlexWrap::Wrap => "wrap",
            FlexWrap::WrapReverse => "wrap-reverse",
        }
    }
}

pub fn parse_flex_wrap(value: &str) -> Option<FlexWrap> {
    match lower(value).as_str() {
        "nowrap" => Some(FlexWrap::NoWrap),
        "wrap" => Some(FlexWrap::Wrap),
        "wrap-reverse" => Some(FlexWrap::WrapReverse),
        _ => None,
    }
}

/// `justify-content`: how leftover main-axis space is distributed (M9.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum JustifyContent {
    #[default]
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

impl JustifyContent {
    pub fn name(self) -> &'static str {
        match self {
            JustifyContent::FlexStart => "flex-start",
            JustifyContent::FlexEnd => "flex-end",
            JustifyContent::Center => "center",
            JustifyContent::SpaceBetween => "space-between",
            JustifyContent::SpaceAround => "space-around",
            JustifyContent::SpaceEvenly => "space-evenly",
        }
    }
}

pub fn parse_justify_content(value: &str) -> Option<JustifyContent> {
    match lower(value).as_str() {
        // `start`/`left` and `end`/`right` are the box-alignment spellings of
        // the two flex keywords. In a left-to-right row they mean the same
        // thing, and pages write them, so they are taken rather than dropped.
        // `normal` is this property's own CSS initial value, which on a flex
        // container behaves as `flex-start`.
        "flex-start" | "start" | "left" | "normal" => Some(JustifyContent::FlexStart),
        "flex-end" | "end" | "right" => Some(JustifyContent::FlexEnd),
        "center" => Some(JustifyContent::Center),
        "space-between" => Some(JustifyContent::SpaceBetween),
        "space-around" => Some(JustifyContent::SpaceAround),
        "space-evenly" => Some(JustifyContent::SpaceEvenly),
        _ => None,
    }
}

/// `align-items`: how items are placed on the cross axis of their line (M9.8).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlignItems {
    FlexStart,
    FlexEnd,
    Center,
    #[default]
    Stretch,
    Baseline,
}

impl AlignItems {
    pub fn name(self) -> &'static str {
        match self {
            AlignItems::FlexStart => "flex-start",
            AlignItems::FlexEnd => "flex-end",
            AlignItems::Center => "center",
            AlignItems::Stretch => "stretch",
            AlignItems::Baseline => "baseline",
        }
    }
}

pub fn parse_align_items(value: &str) -> Option<AlignItems> {
    match lower_words(value).as_str() {
        "flex-start" | "start" | "self-start" | "left" => Some(AlignItems::FlexStart),
        "flex-end" | "end" | "self-end" | "right" => Some(AlignItems::FlexEnd),
        "center" => Some(AlignItems::Center),
        // `normal` behaves as `stretch` on a flex container (css-align-3 §6.2).
        "stretch" | "normal" => Some(AlignItems::Stretch),
        // A cell grid has one baseline per line — the line itself — so the
        // first/last distinction has nothing to distinguish. Both spellings
        // become the one notion M9.8 implements.
        "baseline" | "first baseline" | "last baseline" => Some(AlignItems::Baseline),
        _ => None,
    }
}

/// `align-self`: one item overriding its container's `align-items`. `Auto`
/// (the initial value) means "whatever the container said"; M9.8 resolves it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlignSelf {
    #[default]
    Auto,
    /// Carries the `align-items` values rather than restating them: the two
    /// properties take the same keywords, and M9.8 resolves `Auto` into
    /// exactly this.
    Items(AlignItems),
}

impl AlignSelf {
    pub fn name(self) -> &'static str {
        match self {
            AlignSelf::Auto => "auto",
            AlignSelf::Items(a) => a.name(),
        }
    }
}

pub fn parse_align_self(value: &str) -> Option<AlignSelf> {
    if lower(value) == "auto" {
        return Some(AlignSelf::Auto);
    }
    parse_align_items(value).map(AlignSelf::Items)
}

/// `align-content`: how leftover cross-axis space is distributed between the
/// lines of a wrapped container (M9.10). Has no effect on a single-line
/// container, which is why it is the last of the alignment properties to
/// matter.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlignContent {
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    #[default]
    Stretch,
}

impl AlignContent {
    pub fn name(self) -> &'static str {
        match self {
            AlignContent::FlexStart => "flex-start",
            AlignContent::FlexEnd => "flex-end",
            AlignContent::Center => "center",
            AlignContent::SpaceBetween => "space-between",
            AlignContent::SpaceAround => "space-around",
            AlignContent::Stretch => "stretch",
        }
    }
}

pub fn parse_align_content(value: &str) -> Option<AlignContent> {
    match lower(value).as_str() {
        "flex-start" | "start" => Some(AlignContent::FlexStart),
        "flex-end" | "end" => Some(AlignContent::FlexEnd),
        "center" => Some(AlignContent::Center),
        "space-between" => Some(AlignContent::SpaceBetween),
        "space-around" => Some(AlignContent::SpaceAround),
        "stretch" | "normal" => Some(AlignContent::Stretch),
        _ => None,
    }
}

/// `row-gap` / `column-gap`: the gutters between flex lines and between the
/// items on a line. Their initial value is zero where every other `Length`
/// field's is `auto`, which is why they live in a type with its own `Default`
/// — the same reason [`Edges`] has one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Gaps {
    pub row: Length,
    pub column: Length,
}

impl Default for Gaps {
    fn default() -> Self {
        Gaps {
            row: Length::Zero,
            column: Length::Zero,
        }
    }
}

/// One gap length. `normal` — the CSS initial value, which pages do write to
/// reset a gap — is zero here: it only means something other than zero in
/// multi-column layout, which this engine does not have.
///
/// Negative gaps are invalid (css-align-3 §8.1), like negative widths and
/// unlike negative margins. A percentage is legal and M9.7 resolves it against
/// the container's own inner size on that axis.
pub fn parse_gap(value: &str) -> Option<Length> {
    if value.trim().eq_ignore_ascii_case("normal") {
        return Some(Length::Zero);
    }
    match parse_length(value)? {
        // `gap: auto` is not CSS, and `Auto` is not a distance.
        Length::Auto => None,
        Length::Px(n) | Length::Em(n) | Length::Percent(n) if n < 0.0 => None,
        len => Some(len),
    }
}

/// `gap: <row> [<column>]` — one value sets both axes.
pub fn parse_gaps(value: &str) -> Option<Gaps> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    match parts.len() {
        1 => {
            let both = parse_gap(parts[0])?;
            Some(Gaps {
                row: both,
                column: both,
            })
        }
        2 => Some(Gaps {
            row: parse_gap(parts[0])?,
            column: parse_gap(parts[1])?,
        }),
        _ => None,
    }
}

/// `flex-basis`: the main size an item starts from, before growing or
/// shrinking. `Auto` defers to the main-axis size property (`width` in a row),
/// and `Content` sizes from the content — which is also what `Auto` falls back
/// to when that property is itself `auto` (css-flexbox-1 §7.2.3).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum FlexBasis {
    #[default]
    Auto,
    Content,
    Size(Length),
}

pub fn parse_flex_basis(value: &str) -> Option<FlexBasis> {
    match lower(value).as_str() {
        "auto" => return Some(FlexBasis::Auto),
        // `max-content` and `fit-content` are distinct sizes in CSS; both
        // collapse to `content` here, which M9.4 measures as the max-content
        // width. `fit-content` is the approximation of the two — it should
        // also be clamped by the available space, and is not.
        "content" | "max-content" | "fit-content" => return Some(FlexBasis::Content),
        _ => {}
    }
    match parse_length(value)? {
        // `auto` is handled above; a bare `Length::Auto` cannot reach here.
        Length::Auto => None,
        Length::Px(n) | Length::Em(n) | Length::Percent(n) if n < 0.0 => None,
        len => Some(FlexBasis::Size(len)),
    }
}

/// `flex-grow` / `flex-shrink` / `flex-basis`, kept together because the `flex`
/// shorthand always writes all three and because their initial values are not
/// each other's: `0 1 auto`, which no derived `Default` would produce.
///
/// The grammar's asymmetry is the thing to remember: a page that writes
/// `flex: 1` gets `1 1 0`, not `1 1 auto` — omitting the basis from the
/// shorthand sets it to zero, while never writing the shorthand at all leaves
/// it `auto`. That difference is most of what makes `flex: 1` behave the way
/// pages expect.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Flex {
    pub grow: f32,
    pub shrink: f32,
    pub basis: FlexBasis,
}

impl Default for Flex {
    fn default() -> Self {
        Flex {
            grow: 0.0,
            shrink: 1.0,
            basis: FlexBasis::Auto,
        }
    }
}

/// `flex-grow` / `flex-shrink`: a non-negative number. Negative is invalid, and
/// so are the infinities `f32::from_str` would otherwise hand back for
/// `flex-grow: infinity` — a factor that is not a finite number would make
/// M9.6's distribution arithmetic produce `NaN` widths.
pub fn parse_flex_factor(value: &str) -> Option<f32> {
    let n: f32 = lower(value).parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// `flex: <grow> [<shrink>] [<basis>]`, the shorthand pages actually write.
///
/// `auto` and `content` need no special case — they fall out of the grammar as
/// a lone basis, which defaults the two factors to `1 1`. `none` and `initial`
/// do, because neither is a basis.
pub fn parse_flex(value: &str) -> Option<Flex> {
    match lower(value).as_str() {
        "none" => {
            return Some(Flex {
                grow: 0.0,
                shrink: 0.0,
                basis: FlexBasis::Auto,
            });
        }
        "initial" => return Some(Flex::default()),
        _ => {}
    }
    let (mut grow, mut shrink, mut basis) = (None, None, None);
    // Where the grow factor was, so the shrink factor can be required to sit
    // directly after it: the grammar is `<grow> <shrink>? || <basis>`, so the
    // two factors are one component and cannot be split by the basis.
    // `flex: 20px 1 2` is a page writing the components out of order and is
    // valid; `flex: 1 20px 2` splits the pair and is not.
    let mut grow_at = None;
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (i, part) in parts.iter().enumerate() {
        // Order matters within a token too: `0` is both a number and a length,
        // and the grammar reads the leading numbers as the factors.
        if let Some(n) = parse_flex_factor(part) {
            if grow.is_none() {
                grow = Some(n);
                grow_at = Some(i);
                continue;
            }
            if shrink.is_none() && grow_at.is_some_and(|at| at + 1 == i) {
                shrink = Some(n);
                continue;
            }
        }
        if basis.is_none() {
            basis = Some(parse_flex_basis(part)?);
            continue;
        }
        return None;
    }
    Some(Flex {
        // An omitted factor is 1 in the shorthand, whatever the longhand's own
        // initial value is (`flex-grow`'s is 0).
        grow: grow.unwrap_or(1.0),
        shrink: shrink.unwrap_or(1.0),
        basis: basis.unwrap_or(FlexBasis::Size(Length::Zero)),
    })
}

/// `order`: an integer, and only an integer — `order: 1.5` is invalid CSS, so
/// the declaration drops and the previous winner stands. M9.6 sorts items by
/// it before placing them.
pub fn parse_order(value: &str) -> Option<i32> {
    lower(value).parse().ok()
}

/// Lowercase, trimmed, and internal whitespace runs collapsed to one space —
/// what a two-word keyword (`first baseline`) needs before a table lookup.
fn lower_words(value: &str) -> String {
    value
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `color` / `background-color`. Accepts `#rgb`, `#rgba`, `#rrggbb`,
/// `#rrggbbaa`, `rgb()`/`rgba()`, and a small keyword table. Alpha is parsed
/// and discarded — a cell grid has no compositing, so a translucent colour is
/// drawn as its solid self.
pub fn parse_color(value: &str) -> Option<ColorValue> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = lower(value);
    if let Some(args) = lower
        .strip_prefix("rgb(")
        .or_else(|| lower.strip_prefix("rgba("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return parse_rgb_args(args);
    }
    named_color(&lower)
}

fn parse_hex(hex: &str) -> Option<ColorValue> {
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<Vec<u8>>>()?;
    match digits.len() {
        // #rgb / #rgba: each digit doubled, alpha dropped.
        3 | 4 => Some(ColorValue::Rgb(
            digits[0] * 17,
            digits[1] * 17,
            digits[2] * 17,
        )),
        6 | 8 => Some(ColorValue::Rgb(
            digits[0] * 16 + digits[1],
            digits[2] * 16 + digits[3],
            digits[4] * 16 + digits[5],
        )),
        _ => None,
    }
}

/// `rgb(1, 2, 3)`, `rgb(1 2 3)`, and the percentage spelling. A fourth
/// component (alpha) is accepted and ignored.
fn parse_rgb_args(args: &str) -> Option<ColorValue> {
    let parts: Vec<&str> = args
        .split([',', '/', ' ', '\t', '\n'])
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 3 || parts.len() > 4 {
        return None;
    }
    let channel = |s: &str| -> Option<u8> {
        if let Some(pct) = s.strip_suffix('%') {
            let pct: f32 = pct.parse().ok()?;
            Some((pct.clamp(0.0, 100.0) / 100.0 * 255.0).round() as u8)
        } else {
            let n: f32 = s.parse().ok()?;
            Some(n.clamp(0.0, 255.0).round() as u8)
        }
    };
    Some(ColorValue::Rgb(
        channel(parts[0])?,
        channel(parts[1])?,
        channel(parts[2])?,
    ))
}

/// The 16 HTML4 names plus the handful of spellings the ladder pages use. An
/// unknown name is `None` — the full 148-name CSS table can arrive the day a
/// fixture needs it, and until then a wrong guess is worse than no colour.
fn named_color(name: &str) -> Option<ColorValue> {
    let rgb = match name {
        "transparent" => return Some(ColorValue::Default),
        "black" => (0x00, 0x00, 0x00),
        "silver" => (0xc0, 0xc0, 0xc0),
        "gray" | "grey" => (0x80, 0x80, 0x80),
        "white" => (0xff, 0xff, 0xff),
        "maroon" => (0x80, 0x00, 0x00),
        "red" => (0xff, 0x00, 0x00),
        "purple" => (0x80, 0x00, 0x80),
        "fuchsia" | "magenta" => (0xff, 0x00, 0xff),
        "green" => (0x00, 0x80, 0x00),
        "lime" => (0x00, 0xff, 0x00),
        "olive" => (0x80, 0x80, 0x00),
        "yellow" => (0xff, 0xff, 0x00),
        "navy" => (0x00, 0x00, 0x80),
        "blue" => (0x00, 0x00, 0xff),
        "teal" => (0x00, 0x80, 0x80),
        "aqua" | "cyan" => (0x00, 0xff, 0xff),
        "orange" => (0xff, 0xa5, 0x00),
        _ => return None,
    };
    Some(ColorValue::Rgb(rgb.0, rgb.1, rgb.2))
}

/// `font-weight`. The numeric scale collapses at 600, where CSS itself puts
/// the boundary between "normal" and "bold" faces — a terminal has exactly two.
pub fn parse_font_weight(value: &str) -> Option<FontWeight> {
    match lower(value).as_str() {
        "normal" | "lighter" => Some(FontWeight::Normal),
        "bold" | "bolder" => Some(FontWeight::Bold),
        n => match n.parse::<f32>() {
            Ok(n) if n >= 600.0 => Some(FontWeight::Bold),
            Ok(_) => Some(FontWeight::Normal),
            Err(_) => None,
        },
    }
}

pub fn parse_font_style(value: &str) -> Option<FontStyle> {
    match lower(value).as_str() {
        "normal" => Some(FontStyle::Normal),
        "italic" | "oblique" => Some(FontStyle::Italic),
        _ => None,
    }
}

pub fn parse_text_align(value: &str) -> Option<TextAlign> {
    match lower(value).as_str() {
        "left" | "start" | "justify" => Some(TextAlign::Left),
        "center" => Some(TextAlign::Center),
        "right" | "end" => Some(TextAlign::Right),
        _ => None,
    }
}

/// `text-decoration`, reduced to "is it underlined". `Attrs` carries
/// `UNDERLINE` and has no strikethrough or overline, so `line-through` parses
/// to *not underlined* — the declaration is honoured as far as the terminal can
/// honour it, which is not the same as ignoring it. The shorthand's colour and
/// style components (`underline dotted red`) are skipped over.
pub fn parse_text_decoration(value: &str) -> Option<bool> {
    let mut seen = None;
    for word in lower(value).split_whitespace() {
        match word {
            "underline" => seen = Some(true),
            "none" | "line-through" | "overline" | "blink" => seen = seen.or(Some(false)),
            _ => {}
        }
    }
    seen
}

/// One length token: `auto`, `0`, `12px`, `1.5em`, `50%`, bare number as px.
/// Units this engine does not implement (`rem`, `vh`, `ch`, …) are invalid so
/// the cascade leaves the previous winner standing.
pub fn parse_length(value: &str) -> Option<Length> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") {
        return Some(Length::Auto);
    }
    if value == "0" {
        return Some(Length::Zero);
    }
    let lower = lower(value);
    if let Some(rest) = lower.strip_suffix('%') {
        let n: f32 = rest.trim().parse().ok()?;
        return Some(Length::Percent(n));
    }
    if let Some(rest) = lower.strip_suffix("px") {
        let n: f32 = rest.trim().parse().ok()?;
        return Some(if n == 0.0 {
            Length::Zero
        } else {
            Length::Px(n)
        });
    }
    if let Some(rest) = lower.strip_suffix("em") {
        let n: f32 = rest.trim().parse().ok()?;
        return Some(if n == 0.0 {
            Length::Zero
        } else {
            Length::Em(n)
        });
    }
    // Bare number = px (common in minified sheets and `border: 1 solid`).
    if let Ok(n) = lower.parse::<f32>() {
        return Some(if n == 0.0 {
            Length::Zero
        } else {
            Length::Px(n)
        });
    }
    None
}

/// The sizing properties: `width`/`height` and their `min-`/`max-` clamps.
/// Same tokens as a length, plus `none` (how a page spells "no clamp") for
/// `Auto`.
///
/// Negative values are **invalid**, not zero: CSS 2.1 §10 gives these
/// properties a non-negative range, so `height: -1px` is a dropped
/// declaration and the previous winner stands. Resolving it to zero instead
/// would collapse the box — a page's typo would erase its own content.
/// (`margin` is different: negative margins are legal CSS, and `parse_edges`
/// keeps taking them.)
pub fn parse_width(value: &str) -> Option<Length> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Some(Length::Auto);
    }
    match parse_length(value)? {
        Length::Px(n) | Length::Em(n) | Length::Percent(n) if n < 0.0 => None,
        len => Some(len),
    }
}

/// 1–4 value box shorthand (`margin`, `padding`, `border-width`).
/// CSS order: top, right, bottom, left — with the usual 1/2/3/4 expansion.
pub fn parse_edges(value: &str) -> Option<Edges> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    match parts.len() {
        1 => {
            let a = parse_length(parts[0])?;
            Some(Edges::all(a))
        }
        2 => {
            let v = parse_length(parts[0])?;
            let h = parse_length(parts[1])?;
            Some(Edges {
                top: v,
                right: h,
                bottom: v,
                left: h,
            })
        }
        3 => {
            let top = parse_length(parts[0])?;
            let h = parse_length(parts[1])?;
            let bottom = parse_length(parts[2])?;
            Some(Edges {
                top,
                right: h,
                bottom,
                left: h,
            })
        }
        4 => Some(Edges {
            top: parse_length(parts[0])?,
            right: parse_length(parts[1])?,
            bottom: parse_length(parts[2])?,
            left: parse_length(parts[3])?,
        }),
        _ => None,
    }
}

fn lower(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_maps_unimplemented_modes_to_the_nearest_one() {
        assert_eq!(parse_display("block"), Some(Display::Block));
        assert_eq!(parse_display("INLINE"), Some(Display::Inline));
        assert_eq!(parse_display("none"), Some(Display::None));
        // M9.5: `flex` is its own mode now — it stopped being spelled `Block`
        // the moment there was a `Flex` to spell it. `inline-flex` joins it
        // until M9.11 makes inline-level boxes real.
        assert_eq!(parse_display("flex"), Some(Display::Flex));
        assert_eq!(parse_display("INLINE-FLEX"), Some(Display::Flex));
        assert_eq!(parse_display("table-cell"), Some(Display::Block));
        assert_eq!(parse_display("inline-block"), Some(Display::Inline));
        assert_eq!(parse_display("bananas"), None);
    }

    #[test]
    fn colours_from_hex_functions_and_names() {
        assert_eq!(parse_color("#348"), Some(ColorValue::Rgb(0x33, 0x44, 0x88)));
        assert_eq!(parse_color("#5c5cff"), Some(ColorValue::Rgb(92, 92, 255)));
        assert_eq!(parse_color("  #EEE "), Some(ColorValue::Rgb(238, 238, 238)));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(ColorValue::Rgb(1, 2, 3)));
        assert_eq!(parse_color("rgb(1 2 3)"), Some(ColorValue::Rgb(1, 2, 3)));
        // Alpha is parsed and dropped: the cell grid cannot composite.
        assert_eq!(
            parse_color("rgba(1, 2, 3, 0.5)"),
            Some(ColorValue::Rgb(1, 2, 3))
        );
        assert_eq!(
            parse_color("rgb(100%, 0%, 0%)"),
            Some(ColorValue::Rgb(255, 0, 0))
        );
        assert_eq!(parse_color("Red"), Some(ColorValue::Rgb(255, 0, 0)));
        assert_eq!(parse_color("transparent"), Some(ColorValue::Default));
    }

    #[test]
    fn an_unknown_colour_is_invalid_not_black() {
        // The cascade drops the declaration; the previous winner survives. If
        // this returned black, `color: bananas` would repaint the page.
        assert_eq!(parse_color("bananas"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#gg0000"), None);
        assert_eq!(parse_color("rgb(1, 2)"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn font_weight_collapses_the_numeric_scale_at_600() {
        assert_eq!(parse_font_weight("bold"), Some(FontWeight::Bold));
        assert_eq!(parse_font_weight("700"), Some(FontWeight::Bold));
        assert_eq!(parse_font_weight("600"), Some(FontWeight::Bold));
        assert_eq!(parse_font_weight("400"), Some(FontWeight::Normal));
        assert_eq!(parse_font_weight("lighter"), Some(FontWeight::Normal));
        assert_eq!(parse_font_weight("heavy"), None);
    }

    #[test]
    fn font_style_and_text_align() {
        assert_eq!(parse_font_style("oblique"), Some(FontStyle::Italic));
        assert_eq!(parse_font_style("swirly"), None);
        assert_eq!(parse_text_align("justify"), Some(TextAlign::Left));
        assert_eq!(parse_text_align("CENTER"), Some(TextAlign::Center));
        assert_eq!(parse_text_align("end"), Some(TextAlign::Right));
        assert_eq!(parse_text_align("sideways"), None);
    }

    #[test]
    fn text_decoration_is_as_much_as_a_terminal_can_do() {
        assert_eq!(parse_text_decoration("underline"), Some(true));
        assert_eq!(parse_text_decoration("none"), Some(false));
        // The shorthand's extra components are skipped, not fatal.
        assert_eq!(parse_text_decoration("underline dotted red"), Some(true));
        // No strikethrough in Attrs: honoured as far as the terminal goes.
        assert_eq!(parse_text_decoration("line-through"), Some(false));
        assert_eq!(parse_text_decoration("wavy"), None);
    }

    #[test]
    fn lengths_parse_the_tokens_layout_needs() {
        assert_eq!(parse_length("auto"), Some(Length::Auto));
        assert_eq!(parse_length("0"), Some(Length::Zero));
        assert_eq!(parse_length("0px"), Some(Length::Zero));
        assert_eq!(parse_length("16px"), Some(Length::Px(16.0)));
        assert_eq!(parse_length("1em"), Some(Length::Em(1.0)));
        assert_eq!(parse_length("50%"), Some(Length::Percent(50.0)));
        assert_eq!(parse_length("8"), Some(Length::Px(8.0)));
        // Unknown units are invalid, not zero — so the cascade leaves the prior
        // winner standing rather than silently zeroing a margin.
        assert_eq!(parse_length("2rem"), None);
        assert_eq!(parse_length("10vh"), None);
        assert_eq!(parse_width("none"), Some(Length::Auto));
        assert_eq!(parse_width("90%"), Some(Length::Percent(90.0)));
        // Negative sizes are invalid (CSS 2.1 §10), so the declaration drops
        // and whatever won before still stands. Zero would collapse the box.
        assert_eq!(parse_width("-1px"), None);
        assert_eq!(parse_width("-2em"), None);
        assert_eq!(parse_width("-50%"), None);
        assert_eq!(parse_width("0"), Some(Length::Zero));
        // Margins keep taking negatives — those are legal CSS.
        assert_eq!(parse_length("-1px"), Some(Length::Px(-1.0)));
    }

    #[test]
    fn box_sizing_takes_the_two_css_keywords_and_nothing_else() {
        assert_eq!(parse_box_sizing("border-box"), Some(BoxSizing::BorderBox));
        assert_eq!(parse_box_sizing("CONTENT-BOX"), Some(BoxSizing::ContentBox));
        // Invalid, so the cascade keeps the previous winner (never a silent
        // switch to content-box on a page that asked for border-box).
        assert_eq!(parse_box_sizing("padding-box"), None);
        assert_eq!(parse_box_sizing(""), None);
    }

    #[test]
    fn overflow_takes_the_css_keywords_and_nothing_else() {
        assert_eq!(parse_overflow("hidden"), Some(Overflow::Hidden));
        assert_eq!(parse_overflow("VISIBLE"), Some(Overflow::Visible));
        assert_eq!(parse_overflow("clip"), Some(Overflow::Clip));
        assert_eq!(parse_overflow(" auto "), Some(Overflow::Auto));
        // `overflow: ellipsis` is on two of the ladder pages and is not CSS
        // (they mean `text-overflow`): invalid, so the previous winner stands.
        assert_eq!(parse_overflow("ellipsis"), None);
        assert_eq!(parse_overflow(""), None);
        // Only `visible` lets content out of the box.
        assert!(!Overflow::Visible.clips());
        for v in [
            Overflow::Hidden,
            Overflow::Clip,
            Overflow::Scroll,
            Overflow::Auto,
        ] {
            assert!(v.clips(), "{v:?} must clip");
        }
    }

    // ---- Flexbox vocabulary (M9.5) ----------------------------------------

    #[test]
    fn the_flex_keyword_properties_take_their_css_values() {
        assert_eq!(parse_flex_direction("row"), Some(FlexDirection::Row));
        assert_eq!(
            parse_flex_direction("COLUMN-REVERSE"),
            Some(FlexDirection::ColumnReverse)
        );
        assert_eq!(
            parse_flex_direction(" column "),
            Some(FlexDirection::Column)
        );
        assert_eq!(parse_flex_direction("sideways"), None);
        // `column reverse` is two keywords, not the hyphenated one.
        assert_eq!(parse_flex_direction("column reverse"), None);

        assert_eq!(parse_flex_wrap("nowrap"), Some(FlexWrap::NoWrap));
        assert_eq!(parse_flex_wrap("Wrap-Reverse"), Some(FlexWrap::WrapReverse));
        assert_eq!(parse_flex_wrap("no-wrap"), None);

        assert_eq!(
            parse_justify_content("space-between"),
            Some(JustifyContent::SpaceBetween)
        );
        assert_eq!(
            parse_justify_content("SPACE-EVENLY"),
            Some(JustifyContent::SpaceEvenly)
        );
        assert_eq!(parse_justify_content("space-arround"), None);

        assert_eq!(parse_align_items("stretch"), Some(AlignItems::Stretch));
        assert_eq!(parse_align_items("Center"), Some(AlignItems::Center));
        assert_eq!(parse_align_items("middle"), None);

        assert_eq!(parse_align_self("auto"), Some(AlignSelf::Auto));
        assert_eq!(
            parse_align_self("flex-end"),
            Some(AlignSelf::Items(AlignItems::FlexEnd))
        );
        assert_eq!(parse_align_self("space-between"), None);

        assert_eq!(
            parse_align_content("space-around"),
            Some(AlignContent::SpaceAround)
        );
        assert_eq!(parse_align_content("stretch"), Some(AlignContent::Stretch));
        // Not in `align-content`'s value list (css-align-3 gives it
        // `space-evenly`, this engine does not — M9.10 has no use for it):
        // invalid, so the previous winner stands.
        assert_eq!(parse_align_content("space-evenly"), None);
    }

    #[test]
    fn the_alignment_aliases_pages_actually_write() {
        // `start`/`end` are the box-alignment spellings; `left`/`right` are
        // what pages write when they mean the same thing in a row.
        assert_eq!(
            parse_justify_content("start"),
            Some(JustifyContent::FlexStart)
        );
        assert_eq!(
            parse_justify_content("left"),
            Some(JustifyContent::FlexStart)
        );
        assert_eq!(parse_justify_content("end"), Some(JustifyContent::FlexEnd));
        assert_eq!(
            parse_justify_content("right"),
            Some(JustifyContent::FlexEnd)
        );
        assert_eq!(parse_align_items("self-start"), Some(AlignItems::FlexStart));
        // `normal` is the CSS initial value of both, and behaves as stretch on
        // a flex container.
        assert_eq!(parse_align_items("normal"), Some(AlignItems::Stretch));
        assert_eq!(parse_align_content("normal"), Some(AlignContent::Stretch));
        // A terminal line has one baseline, so both spellings land on it.
        assert_eq!(parse_align_items("baseline"), Some(AlignItems::Baseline));
        assert_eq!(
            parse_align_items("first baseline"),
            Some(AlignItems::Baseline)
        );
        assert_eq!(
            parse_align_items("LAST   baseline"),
            Some(AlignItems::Baseline),
            "case and inner whitespace must not matter"
        );
        assert_eq!(parse_align_items("baseline last"), None);
    }

    #[test]
    fn gaps_are_lengths_that_start_at_zero() {
        assert_eq!(parse_gap("1em"), Some(Length::Em(1.0)));
        assert_eq!(parse_gap("0"), Some(Length::Zero));
        assert_eq!(parse_gap("10%"), Some(Length::Percent(10.0)));
        // `normal` is how a page resets a gap; it means zero here.
        assert_eq!(parse_gap("NORMAL"), Some(Length::Zero));
        // Negative gaps are invalid, unlike negative margins.
        assert_eq!(parse_gap("-1em"), None);
        assert_eq!(parse_gap("auto"), None);
        assert_eq!(parse_gap("2rem"), None);

        assert_eq!(
            parse_gaps("1em 2em"),
            Some(Gaps {
                row: Length::Em(1.0),
                column: Length::Em(2.0),
            })
        );
        assert_eq!(
            parse_gaps("8px"),
            Some(Gaps {
                row: Length::Px(8.0),
                column: Length::Px(8.0),
            })
        );
        // One bad component drops the whole shorthand rather than gapping one
        // axis the page never asked to gap.
        assert_eq!(parse_gaps("1em bananas"), None);
        assert_eq!(parse_gaps("1em 2em 3em"), None);
        assert_eq!(parse_gaps(""), None);
        // The initial value is zero on both axes — not `auto`, which is what a
        // derived `Default` on `Length` would have given.
        assert_eq!(
            Gaps::default(),
            Gaps {
                row: Length::Zero,
                column: Length::Zero
            }
        );
    }

    #[test]
    fn flex_factors_are_non_negative_finite_numbers() {
        assert_eq!(parse_flex_factor("0"), Some(0.0));
        assert_eq!(parse_flex_factor("2.5"), Some(2.5));
        assert_eq!(parse_flex_factor(" 3 "), Some(3.0));
        assert_eq!(parse_flex_factor("-1"), None);
        assert_eq!(parse_flex_factor("1px"), None);
        assert_eq!(parse_flex_factor(""), None);
        // `f32::from_str` takes these; a distribution weight of infinity would
        // hand M9.6 a `NaN` width.
        assert_eq!(parse_flex_factor("inf"), None);
        assert_eq!(parse_flex_factor("infinity"), None);
        assert_eq!(parse_flex_factor("NaN"), None);
    }

    #[test]
    fn flex_basis_is_a_length_or_one_of_two_keywords() {
        assert_eq!(parse_flex_basis("auto"), Some(FlexBasis::Auto));
        assert_eq!(parse_flex_basis("Content"), Some(FlexBasis::Content));
        assert_eq!(
            parse_flex_basis("20px"),
            Some(FlexBasis::Size(Length::Px(20.0)))
        );
        assert_eq!(parse_flex_basis("0"), Some(FlexBasis::Size(Length::Zero)));
        assert_eq!(
            parse_flex_basis("50%"),
            Some(FlexBasis::Size(Length::Percent(50.0)))
        );
        assert_eq!(parse_flex_basis("-1px"), None);
        // `none` is `max-width`'s way of saying "no clamp" and is not a basis.
        assert_eq!(parse_flex_basis("none"), None);
    }

    #[test]
    fn the_flex_shorthand_fills_in_what_the_page_left_out() {
        let flex = |s: &str| parse_flex(s).unwrap();
        // The one every page writes, and the asymmetry worth remembering: a
        // bare number leaves the basis at *zero*, not at the longhand's `auto`.
        assert_eq!(
            flex("1"),
            Flex {
                grow: 1.0,
                shrink: 1.0,
                basis: FlexBasis::Size(Length::Zero)
            }
        );
        assert_eq!(
            flex("none"),
            Flex {
                grow: 0.0,
                shrink: 0.0,
                basis: FlexBasis::Auto
            }
        );
        assert_eq!(
            flex("auto"),
            Flex {
                grow: 1.0,
                shrink: 1.0,
                basis: FlexBasis::Auto
            }
        );
        assert_eq!(flex("initial"), Flex::default());
        assert_eq!(Flex::default().grow, 0.0);
        assert_eq!(Flex::default().shrink, 1.0);
        assert_eq!(
            flex("2 3 20px"),
            Flex {
                grow: 2.0,
                shrink: 3.0,
                basis: FlexBasis::Size(Length::Px(20.0))
            }
        );
        // Two numbers are the two factors; a number and a length are grow and
        // basis, whichever order they come in.
        assert_eq!(
            flex("2 0"),
            Flex {
                grow: 2.0,
                shrink: 0.0,
                basis: FlexBasis::Size(Length::Zero)
            }
        );
        assert_eq!(
            flex("1 30%"),
            Flex {
                grow: 1.0,
                shrink: 1.0,
                basis: FlexBasis::Size(Length::Percent(30.0))
            }
        );
        // A lone length: both factors default to 1, which is not either
        // longhand's own initial value.
        assert_eq!(
            flex("30em"),
            Flex {
                grow: 1.0,
                shrink: 1.0,
                basis: FlexBasis::Size(Length::Em(30.0))
            }
        );
        // The components may come in either order, but the two factors are one
        // component: the basis cannot be written between them.
        assert_eq!(
            flex("20px 1 2"),
            Flex {
                grow: 1.0,
                shrink: 2.0,
                basis: FlexBasis::Size(Length::Px(20.0))
            }
        );
        assert_eq!(parse_flex("1 20px 2"), None);
        assert_eq!(parse_flex("1 2 3 4"), None);
        assert_eq!(parse_flex("bananas"), None);
        assert_eq!(parse_flex("1 auto auto"), None);
        assert_eq!(parse_flex(""), None);
        assert_eq!(parse_flex("-1"), None);
    }

    #[test]
    fn order_is_an_integer_and_may_be_negative() {
        assert_eq!(parse_order("0"), Some(0));
        assert_eq!(parse_order("-1"), Some(-1));
        assert_eq!(parse_order(" 12 "), Some(12));
        // CSS says integer; `1.5` is invalid, so the previous winner stands.
        assert_eq!(parse_order("1.5"), None);
        assert_eq!(parse_order("first"), None);
    }

    #[test]
    fn edge_shorthand_expands_1_2_3_4_values() {
        assert_eq!(parse_edges("1em"), Some(Edges::all(Length::Em(1.0))));
        assert_eq!(
            parse_edges("1em 2em"),
            Some(Edges {
                top: Length::Em(1.0),
                right: Length::Em(2.0),
                bottom: Length::Em(1.0),
                left: Length::Em(2.0),
            })
        );
        assert_eq!(
            parse_edges("1px 2px 3px 4px"),
            Some(Edges {
                top: Length::Px(1.0),
                right: Length::Px(2.0),
                bottom: Length::Px(3.0),
                left: Length::Px(4.0),
            })
        );
        assert_eq!(parse_edges(""), None);
    }

    #[test]
    fn cell_conversion_matches_plan_unit_table() {
        // Horizontal: 8px = 1 cell, 1em = 2 cells, 50% of 80 = 40.
        assert_eq!(Length::Px(8.0).to_cells_h(80), 1);
        assert_eq!(Length::Em(1.0).to_cells_h(80), 2);
        assert_eq!(Length::Percent(50.0).to_cells_h(80), 40);
        assert_eq!(Length::Auto.to_cells_h(80), 0);
        // Vertical: 16px = 1 line, 1em = 1 line; % still uses width.
        assert_eq!(Length::Px(16.0).to_cells_v(80), 1);
        assert_eq!(Length::Em(1.0).to_cells_v(80), 1);
        assert_eq!(Length::Percent(50.0).to_cells_v(80), 40);
        // …except for `height` and its clamps (M9.2), where a percentage is
        // of the containing block's *height*. Same rounding rule as the rest:
        // nonzero never disappears.
        assert_eq!(Length::Percent(50.0).to_lines(20), 10);
        assert_eq!(Length::Percent(100.0).to_lines(7), 7);
        assert_eq!(Length::Percent(0.0).to_lines(20), 0);
        assert_eq!(Length::Percent(1.0).to_lines(20), 1);
        // Absolute lengths ignore the containing height entirely.
        assert_eq!(Length::Px(32.0).to_lines(999), 2);
        assert_eq!(Length::Em(3.0).to_lines(0), 3);
        // Nonzero but sub-cell rounds up so thin borders still draw.
        assert_eq!(Length::Px(1.0).to_cells_h(80), 1);
        assert_eq!(Length::Px(5.0).to_cells_h(80), 1);
    }
}
