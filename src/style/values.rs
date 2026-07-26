//! Property vocabulary: CSS value text → typed values (M4.2).
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
}
