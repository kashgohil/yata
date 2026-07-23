use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One blank column at each end of the row.
const MARGIN: usize = 1;
/// Minimum cells between adjacent segments.
const GAP: usize = 2;

/// Compose the statusline (PLAN.md §3): left at the edge, middle centered,
/// right right-aligned, as one string of exactly `width` cells so the caller
/// paints the whole row in a single `put_str`.
///
/// The right segment always wins; on collision the middle gives way first,
/// then the left — each truncated with a trailing `…`, measured in cells
/// (never bytes or chars), and a wide char is never split by the cut.
pub fn compose(width: usize, left: &str, middle: &str, right: &str) -> String {
    if width == 0 {
        return String::new();
    }
    let right = fit(right, width.saturating_sub(2 * MARGIN));
    let right_start = width - MARGIN - right.width();
    // The last column left and middle may touch: the right segment and its
    // gap are reserved (the full row minus a margin when there is no right).
    let bound = if right.is_empty() {
        width - MARGIN
    } else {
        right_start.saturating_sub(GAP)
    };

    // Left first, middle into whatever remains: that ordering is the
    // "middle gives way before left" rule.
    let left = fit(left, bound.saturating_sub(MARGIN));
    let mid_min = if left.is_empty() {
        MARGIN
    } else {
        MARGIN + left.width() + GAP
    };
    let middle = fit(middle, bound.saturating_sub(mid_min));

    let mut row = String::with_capacity(width + 8);
    let mut col = 0;
    place(&mut row, &mut col, MARGIN, &left);
    if !middle.is_empty() {
        // Centered in the free span between left and right, not the full
        // row: asymmetric neighbors put the middle at the gap's midpoint.
        place(
            &mut row,
            &mut col,
            (mid_min + bound - middle.width()) / 2,
            &middle,
        );
    }
    if !right.is_empty() {
        place(&mut row, &mut col, right_start, &right);
    }
    place(&mut row, &mut col, width, "");
    row
}

/// Pad with spaces up to the `target` column, then append `s`.
fn place(row: &mut String, col: &mut usize, target: usize, s: &str) {
    for _ in *col..target {
        row.push(' ');
    }
    *col = target.max(*col) + s.width();
    row.push_str(s);
}

/// `s` if it fits in `max` cells, else the widest prefix of whole characters
/// that leaves room for a trailing `…`.
fn fit(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_is_exactly_the_requested_width() {
        for w in 0..40 {
            let row = compose(w, "left", "middle", "right");
            assert_eq!(row.width(), w, "width {w} produced {row:?}");
        }
    }

    #[test]
    fn right_segment_is_right_aligned_with_a_margin() {
        let row = compose(30, "url", "", "34% · 2.1 ms");
        assert!(row.ends_with("34% · 2.1 ms "), "row was {row:?}");
    }

    #[test]
    fn middle_is_centered_when_space_allows() {
        let row = compose(20, "", "MID", "");
        // Ideal start is (20 - 3) / 2 = 8.
        assert_eq!(&row[8..11], "MID");
        assert_eq!(row.width(), 20);
    }

    #[test]
    fn middle_centers_between_asymmetric_neighbors() {
        // The free span runs from column 13 (left end + gap) to 32 (the last
        // start that clears the right segment): midpoint 22. Full-row
        // centering would put MID at column 18 — this pins the gap rule.
        let row = compose(40, "0123456789", "MID", "9%");
        assert_eq!(&row[22..25], "MID", "row was {row:?}");
    }

    #[test]
    fn long_left_truncates_with_ellipsis_before_touching_right() {
        let row = compose(20, &"x".repeat(40), "", "9%");
        assert!(row.ends_with("9% "), "right must survive: {row:?}");
        assert!(row.contains('…'), "left must show its truncation: {row:?}");
        assert_eq!(row.width(), 20);
    }

    #[test]
    fn middle_gives_way_before_left() {
        // The left fits exactly; there is no room for the middle, which must
        // vanish rather than eat into the left or the right.
        let row = compose(16, "0123456789", "MIDDLE", "9%");
        assert!(row.contains("0123456789"), "left was sacrificed: {row:?}");
        assert!(!row.contains('M'), "middle must give way: {row:?}");
        assert!(row.ends_with("9% "), "row was {row:?}");
    }

    #[test]
    fn middle_truncates_with_ellipsis_when_it_partly_fits() {
        // Left fits whole; six cells remain for the middle, which must show
        // a truncated prefix rather than vanish or push into the right.
        let row = compose(24, "0123456789", "MIDDLEMIDDLE", "9%");
        assert!(row.contains("MIDDL…"), "row was {row:?}");
        assert!(row.ends_with("9% "), "row was {row:?}");
    }

    #[test]
    fn truncation_never_splits_a_wide_char() {
        // Four cells for the left: one two-cell glyph fits before the `…`;
        // the next would straddle the cut, so it must be dropped whole.
        let row = compose(10, "世界世界世界", "", "9%");
        assert_eq!(row.width(), 10, "row was {row:?}");
        assert!(row.contains("世…"), "row was {row:?}");
        assert!(!row.contains('界'), "a glyph leaked past the cut: {row:?}");
    }

    #[test]
    fn tiny_widths_compose_without_panicking() {
        for w in 0..4 {
            let row = compose(w, "https://example.com", "⠸ loading… 12 KB", "34% · 2.1 ms");
            assert_eq!(row.width(), w, "width {w} produced {row:?}");
        }
    }
}
