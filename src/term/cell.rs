use std::ops::BitOr;

/// Second column of a double-width character. Never emitted; `Frame::set`
/// maintains the invariant that a continuation always sits directly right of
/// its head and shares its style.
pub const CONT: char = '\0';

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    // The renderer emits both variants but nothing paints in color until the
    // CSS cascade (M4); only the renderer's tests construct them until then.
    #[allow(dead_code)]
    Ansi(u8),
    #[allow(dead_code)]
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Attrs(u8);

impl Attrs {
    pub const NONE: Attrs = Attrs(0);
    pub const BOLD: Attrs = Attrs(1);
    pub const ITALIC: Attrs = Attrs(1 << 1);
    pub const UNDERLINE: Attrs = Attrs(1 << 2);
    pub const REVERSE: Attrs = Attrs(1 << 3);

    pub fn contains(self, other: Attrs) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Attrs {
    type Output = Attrs;
    fn bitor(self, rhs: Attrs) -> Attrs {
        Attrs(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::NONE,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Cell {
    pub fn new(ch: char, style: Style) -> Self {
        Cell {
            ch,
            fg: style.fg,
            bg: style.bg,
            attrs: style.attrs,
        }
    }

    pub fn style(&self) -> Style {
        Style {
            fg: self.fg,
            bg: self.bg,
            attrs: self.attrs,
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::new(' ', Style::default())
    }
}
