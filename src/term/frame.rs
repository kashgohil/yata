use unicode_width::UnicodeWidthChar;

use super::cell::{CONT, Cell, Style};

/// A width × height grid of cells, row-major. All drawing paints into a
/// `Frame`; the renderer diffs frames to decide what reaches the terminal.
pub struct Frame {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
}

impl Frame {
    pub fn new(w: u16, h: u16) -> Self {
        Frame {
            w,
            h,
            cells: vec![Cell::default(); w as usize * h as usize],
        }
    }

    pub fn width(&self) -> u16 {
        self.w
    }

    pub fn height(&self) -> u16 {
        self.h
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.w as usize + x as usize
    }

    pub fn get(&self, x: u16, y: u16) -> Cell {
        debug_assert!(x < self.w && y < self.h);
        self.cells[self.idx(x, y)]
    }

    /// Write one cell. Zero-width and control characters are ignored. A
    /// double-width character also claims the next cell as a continuation; at
    /// the last column it degrades to a space. Overwriting either half of an
    /// existing wide character blanks the other half, so no orphaned half ever
    /// reaches the diff.
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x >= self.w || y >= self.h {
            return;
        }
        let width = cell.ch.width().unwrap_or(0);
        if width == 0 {
            return;
        }
        self.unsplit_wide(x, y);
        let i = self.idx(x, y);
        if width >= 2 {
            if x + 1 >= self.w {
                self.cells[i] = Cell { ch: ' ', ..cell };
                return;
            }
            self.unsplit_wide(x + 1, y);
            self.cells[i] = cell;
            let cont = self.idx(x + 1, y);
            self.cells[cont] = Cell { ch: CONT, ..cell };
        } else {
            self.cells[i] = cell;
        }
    }

    /// If (x, y) holds half of a wide character, turn both halves into spaces
    /// (styles kept) so a subsequent write can't leave an orphan.
    fn unsplit_wide(&mut self, x: u16, y: u16) {
        let i = self.idx(x, y);
        let c = self.cells[i];
        if c.ch == CONT {
            self.cells[i].ch = ' ';
            let head = self.idx(x - 1, y);
            self.cells[head].ch = ' ';
        } else if c.ch.width() == Some(2) {
            self.cells[i].ch = ' ';
            let cont = self.idx(x + 1, y);
            self.cells[cont].ch = ' ';
        }
    }

    /// Write a string starting at (x, y), clipping at the right edge.
    /// Returns the column after the last cell written.
    pub fn put_str(&mut self, x: u16, y: u16, s: &str, style: Style) -> u16 {
        let mut x = x;
        if y >= self.h {
            return x;
        }
        for ch in s.chars() {
            let width = ch.width().unwrap_or(0) as u16;
            if width == 0 {
                continue;
            }
            if x >= self.w {
                break;
            }
            if width == 2 && x + 1 >= self.w {
                self.set(x, y, Cell::new(' ', style));
                x += 1;
                break;
            }
            self.set(x, y, Cell::new(ch, style));
            x += width;
        }
        x
    }

    pub fn clear(&mut self) {
        self.fill(Cell::default());
    }

    /// Bulk fill, bypassing `set`'s wide-char bookkeeping. Not part of the
    /// public drawing API: a wide or zero-width fill cell would break the
    /// continuation invariant, so callers (clear, renderer invalidation)
    /// must fill with plain narrow cells or an unpaintable sentinel only.
    pub(super) fn fill(&mut self, cell: Cell) {
        self.cells.fill(cell);
    }

    /// Resize the grid; contents are discarded (reset to default cells).
    pub fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
        self.cells.clear();
        self.cells.resize(w as usize * h as usize, Cell::default());
    }

    pub fn copy_from(&mut self, other: &Frame) {
        debug_assert!(self.w == other.w && self.h == other.h);
        self.cells.copy_from_slice(&other.cells);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::cell::Attrs;

    fn cell(ch: char) -> Cell {
        Cell::new(ch, Style::default())
    }

    #[test]
    fn wide_char_claims_two_cells() {
        let mut f = Frame::new(4, 1);
        f.put_str(0, 0, "世x", Style::default());
        assert_eq!(f.get(0, 0).ch, '世');
        assert_eq!(f.get(1, 0).ch, CONT);
        assert_eq!(f.get(2, 0).ch, 'x');
    }

    #[test]
    fn overwriting_continuation_blanks_the_head() {
        let mut f = Frame::new(4, 1);
        f.put_str(0, 0, "世", Style::default());
        f.set(1, 0, cell('x'));
        assert_eq!(f.get(0, 0).ch, ' ');
        assert_eq!(f.get(1, 0).ch, 'x');
    }

    #[test]
    fn overwriting_head_blanks_the_continuation() {
        let mut f = Frame::new(4, 1);
        f.put_str(1, 0, "世", Style::default());
        f.set(1, 0, cell('x'));
        assert_eq!(f.get(1, 0).ch, 'x');
        assert_eq!(f.get(2, 0).ch, ' ');
    }

    #[test]
    fn wide_char_at_last_column_becomes_space() {
        let mut f = Frame::new(3, 1);
        f.put_str(2, 0, "世", Style::default());
        assert_eq!(f.get(2, 0).ch, ' ');
    }

    #[test]
    fn wide_char_overwriting_two_wide_chars_blanks_both() {
        let mut f = Frame::new(4, 1);
        f.put_str(0, 0, "世界", Style::default());
        f.set(1, 0, cell('中'));
        assert_eq!(f.get(0, 0).ch, ' ');
        assert_eq!(f.get(1, 0).ch, '中');
        assert_eq!(f.get(2, 0).ch, CONT);
        assert_eq!(f.get(3, 0).ch, ' ');
    }

    #[test]
    fn zero_width_chars_are_skipped() {
        let mut f = Frame::new(4, 1);
        // U+0300 is a combining accent, width 0.
        let end = f.put_str(0, 0, "a\u{300}b", Style::default());
        assert_eq!(f.get(0, 0).ch, 'a');
        assert_eq!(f.get(1, 0).ch, 'b');
        assert_eq!(end, 2);
    }

    #[test]
    fn put_str_clips_at_right_edge() {
        let mut f = Frame::new(3, 1);
        let end = f.put_str(1, 0, "abcd", Style::default());
        assert_eq!(f.get(1, 0).ch, 'a');
        assert_eq!(f.get(2, 0).ch, 'b');
        assert_eq!(end, 3);
    }

    #[test]
    fn out_of_bounds_writes_are_ignored() {
        let mut f = Frame::new(2, 2);
        f.set(5, 0, cell('x'));
        f.put_str(0, 5, "hi", Style::default());
        assert_eq!(f.get(0, 0).ch, ' ');
    }

    #[test]
    fn style_reaches_the_cell() {
        let mut f = Frame::new(2, 1);
        let style = Style {
            attrs: Attrs::BOLD | Attrs::UNDERLINE,
            ..Style::default()
        };
        f.put_str(0, 0, "a", style);
        assert!(f.get(0, 0).attrs.contains(Attrs::BOLD));
        assert!(f.get(0, 0).attrs.contains(Attrs::UNDERLINE));
        assert!(!f.get(0, 0).attrs.contains(Attrs::ITALIC));
    }
}
