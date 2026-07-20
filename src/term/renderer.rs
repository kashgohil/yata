use std::io::{self, Write};

use unicode_width::UnicodeWidthChar;

use super::cell::{Attrs, CONT, Cell, Color, Style};
use super::frame::Frame;

/// Marks every prev cell as "unlike anything paintable" so the next present
/// repaints in full. `\u{1}` is a control char, so `Frame::set` can never
/// place it in a paintable frame.
const INVALID: char = '\u{1}';

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub truecolor: bool,
}

/// Capability detection as a pure function of the env value, so it's testable.
/// Call as `detect_caps(std::env::var("COLORTERM").ok().as_deref())`.
pub fn detect_caps(colorterm: Option<&str>) -> Caps {
    Caps {
        truecolor: matches!(colorterm, Some("truecolor") | Some("24bit")),
    }
}

/// Double-buffered renderer: paint into `frame()`, then `present()` diffs
/// against the previous frame and emits only the changed cells as one batched
/// write wrapped in synchronized-output markers (CSI ?2026).
pub struct Renderer {
    prev: Frame,
    next: Frame,
    caps: Caps,
    // Reused across frames; the diff loop must not allocate per present.
    buf: Vec<u8>,
}

impl Renderer {
    /// Assumes the target screen starts blank (a freshly entered alternate
    /// screen); the first present only emits non-blank cells.
    pub fn new(w: u16, h: u16, caps: Caps) -> Self {
        Renderer {
            prev: Frame::new(w, h),
            next: Frame::new(w, h),
            caps,
            buf: Vec::new(),
        }
    }

    pub fn frame(&mut self) -> &mut Frame {
        &mut self.next
    }

    /// Resize both frames and force a full repaint on the next present.
    pub fn resize(&mut self, w: u16, h: u16) {
        self.next.resize(w, h);
        self.prev.resize(w, h);
        self.prev.fill(Cell {
            ch: INVALID,
            ..Cell::default()
        });
    }

    pub fn present(&mut self, out: &mut impl Write) -> io::Result<()> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();

        // Terminal cursor position and SGR state are only known within this
        // frame, so the first emission always sets both.
        let mut cursor: Option<(u16, u16)> = None;
        let mut style: Option<Style> = None;

        for y in 0..self.next.height() {
            let mut x = 0;
            while x < self.next.width() {
                let n = self.next.get(x, y);
                // Continuations are covered by emitting their head.
                if n.ch == CONT {
                    x += 1;
                    continue;
                }
                if n == self.prev.get(x, y) {
                    x += 1;
                    continue;
                }
                if cursor != Some((x, y)) {
                    // CUP is 1-based.
                    write!(buf, "\x1b[{};{}H", y + 1, x + 1).unwrap();
                }
                if style != Some(n.style()) {
                    push_sgr(&mut buf, n.style(), self.caps);
                    style = Some(n.style());
                }
                let mut utf8 = [0u8; 4];
                buf.extend_from_slice(n.ch.encode_utf8(&mut utf8).as_bytes());
                // max(1) guarantees the loop advances even if a zero-width
                // cell ever sneaks past Frame's invariants.
                let width = (n.ch.width().unwrap_or(1) as u16).max(1);
                cursor = Some((x + width, y));
                x += width;
            }
        }

        if !buf.is_empty() {
            out.write_all(b"\x1b[?2026h")?;
            out.write_all(&buf)?;
            out.write_all(b"\x1b[?2026l")?;
            out.flush()?;
        }

        self.buf = buf;
        std::mem::swap(&mut self.prev, &mut self.next);
        // Keep next in sync with the screen so callers may paint incrementally.
        self.next.copy_from(&self.prev);
        Ok(())
    }
}

/// Emit one SGR sequence for the full style. Always starts from a reset so no
/// attribute state leaks between runs or frames.
fn push_sgr(buf: &mut Vec<u8>, style: Style, caps: Caps) {
    buf.extend_from_slice(b"\x1b[0");
    if style.attrs.contains(Attrs::BOLD) {
        buf.extend_from_slice(b";1");
    }
    if style.attrs.contains(Attrs::ITALIC) {
        buf.extend_from_slice(b";3");
    }
    if style.attrs.contains(Attrs::UNDERLINE) {
        buf.extend_from_slice(b";4");
    }
    if style.attrs.contains(Attrs::REVERSE) {
        buf.extend_from_slice(b";7");
    }
    push_color(buf, style.fg, 38, caps);
    push_color(buf, style.bg, 48, caps);
    buf.push(b'm');
}

fn push_color(buf: &mut Vec<u8>, color: Color, base: u8, caps: Caps) {
    match color {
        // The leading reset already selected the default colors.
        Color::Default => {}
        Color::Ansi(n) => write!(buf, ";{base};5;{n}").unwrap(),
        Color::Rgb(r, g, b) => {
            if caps.truecolor {
                write!(buf, ";{base};2;{r};{g};{b}").unwrap();
            } else {
                write!(buf, ";{base};5;{}", nearest_ansi256(r, g, b)).unwrap();
            }
        }
    }
}

/// Nearest color in the xterm-256 palette: 6×6×6 cube (16–231) for chromatic
/// colors, 24-step gray ramp (232–255) for grays.
fn nearest_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16; // cube black
        }
        if r >= 248 {
            return 231; // cube white
        }
        return 232 + (r - 8) / 10;
    }
    // Invert the cube's level values (0, 95, 135, 175, 215, 255).
    let level = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRUE: Caps = Caps { truecolor: true };

    fn present(r: &mut Renderer) -> String {
        let mut out = Vec::new();
        r.present(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn wrapped(inner: &str) -> String {
        format!("\x1b[?2026h{inner}\x1b[?2026l")
    }

    #[test]
    fn unchanged_repaint_writes_zero_bytes() {
        let mut r = Renderer::new(4, 2, TRUE);
        assert!(present(&mut r).is_empty(), "blank first frame");

        r.frame().put_str(0, 0, "hi", Style::default());
        assert!(!present(&mut r).is_empty());

        r.frame().put_str(0, 0, "hi", Style::default());
        assert!(
            present(&mut r).is_empty(),
            "repainting identical content must write nothing"
        );
    }

    #[test]
    fn single_cell_change_is_one_synchronized_write() {
        let mut r = Renderer::new(4, 2, TRUE);
        r.frame().set(1, 0, Cell::new('A', Style::default()));
        assert_eq!(present(&mut r), wrapped("\x1b[1;2H\x1b[0mA"));
    }

    #[test]
    fn same_style_run_coalesces_to_one_move_and_one_sgr() {
        let mut r = Renderer::new(4, 2, TRUE);
        r.frame().put_str(0, 1, "abc", Style::default());
        assert_eq!(present(&mut r), wrapped("\x1b[2;1H\x1b[0mabc"));
    }

    #[test]
    fn adjacent_style_change_emits_sgr_but_no_cursor_move() {
        let mut r = Renderer::new(4, 1, TRUE);
        r.frame().set(0, 0, Cell::new('a', Style::default()));
        let bold = Style {
            attrs: Attrs::BOLD,
            ..Style::default()
        };
        r.frame().set(1, 0, Cell::new('b', bold));
        assert_eq!(present(&mut r), wrapped("\x1b[1;1H\x1b[0ma\x1b[0;1mb"));
    }

    #[test]
    fn wide_char_is_emitted_once_and_advances_two_columns() {
        let mut r = Renderer::new(4, 1, TRUE);
        r.frame().put_str(0, 0, "世x", Style::default());
        // 'x' lands at column 2; the cursor got there by writing 世, so no
        // second cursor move appears.
        assert_eq!(present(&mut r), wrapped("\x1b[1;1H\x1b[0m世x"));
    }

    #[test]
    fn overwriting_half_a_wide_char_repaints_both_cells() {
        let mut r = Renderer::new(4, 1, TRUE);
        r.frame().put_str(0, 0, "世", Style::default());
        present(&mut r);

        // Overwriting the continuation blanks the head; the emitted bytes
        // must repaint both columns, not leave half a glyph on screen.
        r.frame().set(1, 0, Cell::new('x', Style::default()));
        assert_eq!(present(&mut r), wrapped("\x1b[1;1H\x1b[0m x"));
    }

    #[test]
    fn resize_forces_a_full_repaint() {
        let mut r = Renderer::new(4, 2, TRUE);
        r.frame().put_str(0, 0, "hi", Style::default());
        present(&mut r);

        r.resize(4, 2);
        r.frame().put_str(0, 0, "hi", Style::default());
        assert_eq!(
            present(&mut r),
            wrapped("\x1b[1;1H\x1b[0mhi  \x1b[2;1H    ")
        );
    }

    #[test]
    fn rgb_uses_truecolor_when_available() {
        let mut r = Renderer::new(2, 1, TRUE);
        let style = Style {
            fg: Color::Rgb(1, 2, 3),
            ..Style::default()
        };
        r.frame().set(0, 0, Cell::new('a', style));
        assert_eq!(present(&mut r), wrapped("\x1b[1;1H\x1b[0;38;2;1;2;3ma"));
    }

    #[test]
    fn rgb_degrades_to_nearest_ansi256() {
        let mut r = Renderer::new(2, 1, Caps { truecolor: false });
        let style = Style {
            fg: Color::Rgb(255, 0, 0),
            bg: Color::Ansi(17),
            ..Style::default()
        };
        r.frame().set(0, 0, Cell::new('a', style));
        assert_eq!(
            present(&mut r),
            wrapped("\x1b[1;1H\x1b[0;38;5;196;48;5;17ma")
        );
    }

    #[test]
    fn nearest_ansi256_known_colors() {
        assert_eq!(nearest_ansi256(255, 0, 0), 196); // pure red
        assert_eq!(nearest_ansi256(255, 255, 255), 231); // white
        assert_eq!(nearest_ansi256(0, 0, 0), 16); // black
        assert_eq!(nearest_ansi256(128, 128, 128), 244); // mid gray
        assert_eq!(nearest_ansi256(0, 95, 175), 25); // exact cube point
    }

    #[test]
    fn detect_caps_reads_colorterm() {
        assert!(detect_caps(Some("truecolor")).truecolor);
        assert!(detect_caps(Some("24bit")).truecolor);
        assert!(!detect_caps(Some("256color")).truecolor);
        assert!(!detect_caps(None).truecolor);
    }
}
