//! Property vocabulary: CSS value text → typed values (M4.2, M5.1 lengths).
//!
//! Every parser here is `&str -> Option<T>`, and `None` means *invalid*, which
//! in CSS means the declaration is dropped and whatever won before it keeps
//! standing. That is the difference between an unknown colour name and black:
//! `color: bananas` must leave the previous winner alone, not paint anything.
//!
//! This is where M4.1's deliberate ignorance ends — the parser handed over raw
//! source text, and this module is the first code that knows `red` is a colour.

/// The three display modes M4 implements. PLAN.md M5 brings the box model;
/// until then a box is either a line-breaking block, inline text, or gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Display {
    Block,
    #[default]
    Inline,
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

/// `display`. Values M4 cannot honour are mapped to the nearest of the three
/// rather than dropped: danluu.com's list items are `display:flex`, and
/// dropping that declaration is harmless while treating it as *invalid* would
/// be too — but treating `flex` as `inline` would collapse the page. Every
/// box-generating mode that stacks becomes `Block`; M9 brings real flex.
pub fn parse_display(value: &str) -> Option<Display> {
    match lower(value).as_str() {
        "none" => Some(Display::None),
        "inline" | "inline-block" | "inline-flex" | "inline-grid" | "contents" => {
            Some(Display::Inline)
        }
        "block" | "flow-root" | "list-item" | "flex" | "grid" | "table" | "table-row"
        | "table-cell" | "table-row-group" | "table-header-group" | "table-footer-group"
        | "table-caption" | "inline-table" => Some(Display::Block),
        _ => None,
    }
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
        // danluu.com's list items; must stack, not collapse into a line.
        assert_eq!(parse_display("flex"), Some(Display::Block));
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
