//! Paint: layout tree → display list (PLAN.md M5/M8).
//!
//! Pure transform. The display list is what scrolling re-emits at a new offset
//! — no restyle, no relayout (CLAUDE.md). Commands are in document coordinates;
//! the viewport subtracts the scroll offset when drawing.
//!
//! `overflow` (M9.3) is applied *here*, while the list is built: commands come
//! out already trimmed to the clip they sit under, so `paint_to_frame` stays
//! the same loop it was and the scroll path never grows per-command clip state.

use std::collections::HashMap;
use std::sync::Arc;

use crate::image::{
    DecodedImage, HalfBlockGrid, KittyPlacement, placeholder_grid, raster_halfblocks,
};
use crate::layout::{BoxKind, Clip, LayoutBox, LayoutTree, Rect, term_color};
use crate::style::values::ColorValue;
use crate::term::{Color, Style};

/// One draw operation in document cell coordinates.
#[derive(Clone, Debug, PartialEq)]
pub enum DisplayCommand {
    /// Fill the padding box (content + padding) with a background colour.
    FillRect { rect: Rect, color: Color },
    /// Draw a border around the border box with box-drawing characters.
    Border { rect: Rect },
    /// A run of text at (x, y). Never contains a newline.
    Text {
        x: i32,
        y: i32,
        text: String,
        style: Style,
    },
    /// Replaced image: half-block grid baked in for the scroll path; optional
    /// full-res pixels for Kitty (M8).
    Image {
        rect: Rect,
        grid: HalfBlockGrid,
        pixels: Option<Arc<DecodedImage>>,
    },
}

/// Ordered draw list for one laid-out page.
#[derive(Clone, Debug, Default)]
pub struct DisplayList {
    pub commands: Vec<DisplayCommand>,
    pub width: i32,
    pub height: i32,
}

/// Pixel store for paint: absolute URL → decoded image (or missing).
pub type ImagePixels = HashMap<String, Arc<DecodedImage>>;

/// Walk the layout tree in paint order and emit draw commands.
pub fn paint(tree: &LayoutTree) -> DisplayList {
    paint_with(tree, &ImagePixels::new())
}

/// Paint with an image pixel map (M8). Missing URLs become placeholders.
pub fn paint_with(tree: &LayoutTree, images: &ImagePixels) -> DisplayList {
    let mut list = DisplayList {
        commands: Vec::new(),
        width: tree.width,
        height: tree.height,
    };
    tree.walk_clipped(&mut |_, b, clip| paint_box(b, images, &mut list, clip));
    list
}

/// Emit one box's commands, trimmed to `clip` — the region an ancestor's
/// `overflow` leaves for it. The box's *own* `overflow` is not applied here:
/// it clips this box's content and descendants, which is what the walk hands
/// them (see [`Clip::inside`]).
fn paint_box(b: &LayoutBox, images: &ImagePixels, list: &mut DisplayList, clip: Clip) {
    match b.kind {
        // A flex container paints exactly like a block: a background fills its
        // padding box and a border outlines it, whoever placed what is inside.
        BoxKind::Block
        | BoxKind::Flex
        | BoxKind::Table
        | BoxKind::TableRow
        | BoxKind::TableCell => paint_decorations(b, list, clip),
        // A form control is an ordinary box from the outside — its background
        // and border paint like any other — and then draws its own cells
        // (M11.8). Its frame lives in the padding, which is why the decorations
        // go down first.
        BoxKind::Field(_) => {
            paint_decorations(b, list, clip);
            for run in crate::layout::field::runs(b) {
                if let Some((x, text)) = clip.trim_text(run.x, run.y, &run.text) {
                    list.commands.push(DisplayCommand::Text {
                        x,
                        y: run.y,
                        text,
                        style: run.style,
                    });
                }
            }
        }
        BoxKind::Text => {
            if let Some(text) = &b.text
                && !text.is_empty()
                && let Some((x, text)) =
                    clip.trim_text(b.dimensions.content.x, b.dimensions.content.y, text)
            {
                list.commands.push(DisplayCommand::Text {
                    x,
                    y: b.dimensions.content.y,
                    text,
                    style: b.term_style,
                });
            }
        }
        BoxKind::Image => {
            let rect = b.dimensions.content;
            let visible = clip.apply(rect);
            if visible.width > 0 && visible.height > 0 {
                let (grid, pixels) = match b
                    .image_src
                    .as_ref()
                    .and_then(|u| images.get(u).map(|a| (u, a)))
                {
                    Some((_url, img)) => (
                        raster_halfblocks(img, rect.width, rect.height),
                        Some(Arc::clone(img)),
                    ),
                    None => (placeholder_grid(rect.width, rect.height), None),
                };
                // Whether to overlay alt text is about the *image*, not about
                // the clip: decided before clipping takes the pixels away.
                let alt = (pixels.is_none())
                    .then(|| b.text.as_ref().filter(|t| !t.is_empty()))
                    .flatten();
                let cropped = visible != rect;
                let grid = if cropped {
                    grid.crop(
                        visible.x - rect.x,
                        visible.y - rect.y,
                        visible.width,
                        visible.height,
                    )
                } else {
                    grid
                };
                // Kitty places pixels by cell rectangle: a partially clipped
                // placement would have to crop the pixels to match, which is
                // not worth the protocol gymnastics for a rare case. Such an
                // image falls back to the half-block grid — already cropped
                // above, so it draws exactly the surviving cells.
                let pixels = if cropped { None } else { pixels };
                list.commands.push(DisplayCommand::Image {
                    rect: visible,
                    grid,
                    pixels,
                });
                if let Some(alt) = alt
                    && let Some((x, text)) =
                        clip.trim_text(rect.x, rect.y, &truncate_cells(alt, rect.width))
                {
                    list.commands.push(DisplayCommand::Text {
                        x,
                        y: rect.y,
                        text,
                        style: Style {
                            fg: Color::Rgb(0xc0, 0xc0, 0xc0),
                            bg: Color::Rgb(0x40, 0x40, 0x40),
                            attrs: crate::term::Attrs::NONE,
                        },
                    });
                }
            }
        }
        BoxKind::AnonymousBlock | BoxKind::Line | BoxKind::Inline => {}
    }
}

/// A box's background and border — everything it paints that is not its
/// content. Shared by blocks, flex containers and form controls, because from
/// the outside those differ only in what goes *inside* the padding box.
fn paint_decorations(b: &LayoutBox, list: &mut DisplayList, clip: Clip) {
    // Background fills the padding box (CSS).
    if let ColorValue::Rgb(r, g, bcol) = b.computed.background_color {
        let rect = clip.apply(b.dimensions.padding_box());
        if rect.width > 0 && rect.height > 0 {
            list.commands.push(DisplayCommand::FillRect {
                rect,
                color: Color::Rgb(r, g, bcol),
            });
        }
    }
    // Border: any nonzero border edge draws the full rectangle outline.
    //
    // Deviation, recorded rather than papered over: a border cut by an
    // ancestor's clip is emitted as the *intersected* rectangle, so it closes
    // with corners along the clip edge where a browser would let it run off.
    // `Border` is one command meaning "outline this rect", and the honest fix —
    // open-sided borders — would put clip state into the scroll path, which
    // M9.3 exists to avoid.
    let border = b.dimensions.border;
    if border.top > 0 || border.right > 0 || border.bottom > 0 || border.left > 0 {
        let rect = clip.apply(b.dimensions.border_box());
        if rect.width > 0 && rect.height > 0 {
            list.commands.push(DisplayCommand::Border { rect });
        }
    }
}

fn truncate_cells(s: &str, max: i32) -> String {
    use unicode_width::UnicodeWidthChar;
    if max <= 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut cells = 0i32;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as i32;
        if cells + w > max {
            break;
        }
        out.push(ch);
        cells += w;
    }
    out
}

/// Paint a display list into a frame, showing the slice starting at `scroll_y`
/// for `page_h` rows, with content origin at (`origin_x`, 0) on the frame.
pub fn paint_to_frame(
    list: &DisplayList,
    frame: &mut crate::term::Frame,
    origin_x: u16,
    scroll_y: i32,
    page_h: u16,
) {
    let page_h = page_h as i32;
    let visible = |y: i32, h: i32| y + h > scroll_y && y < scroll_y + page_h;

    for cmd in &list.commands {
        match cmd {
            DisplayCommand::FillRect { rect, color } => {
                if !visible(rect.y, rect.height) {
                    continue;
                }
                let style = Style {
                    fg: Color::Default,
                    bg: *color,
                    attrs: crate::term::Attrs::NONE,
                };
                for row in rect.y..rect.y + rect.height {
                    let screen_y = row - scroll_y;
                    if screen_y < 0 || screen_y >= page_h {
                        continue;
                    }
                    for col in 0..rect.width {
                        let screen_x = origin_x as i32 + rect.x + col;
                        if screen_x < 0 || screen_x >= frame.width() as i32 {
                            continue;
                        }
                        frame.set(
                            screen_x as u16,
                            screen_y as u16,
                            crate::term::Cell::new(' ', style),
                        );
                    }
                }
            }
            DisplayCommand::Border { rect } => {
                if !visible(rect.y, rect.height) {
                    continue;
                }
                draw_border(frame, origin_x, scroll_y, page_h, *rect);
            }
            DisplayCommand::Text { x, y, text, style } => {
                if !visible(*y, 1) {
                    continue;
                }
                let screen_y = *y - scroll_y;
                if screen_y < 0 || screen_y >= page_h {
                    continue;
                }
                let screen_x = origin_x as i32 + *x;
                let text_cells = unicode_width::UnicodeWidthStr::width(text.as_str()) as i32;
                if screen_x >= frame.width() as i32 || screen_x + text_cells <= 0 {
                    continue;
                }
                // Draw char by char so a background fill already under this
                // row keeps its bg (text style usually has bg:Default).
                let mut cx = screen_x;
                for ch in text.chars() {
                    let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as i32;
                    if w == 0 {
                        continue;
                    }
                    if cx >= 0 && cx < frame.width() as i32 {
                        let existing = frame.get(cx as u16, screen_y as u16);
                        let mut st = *style;
                        if st.bg == Color::Default {
                            st.bg = existing.bg;
                        }
                        frame.set(cx as u16, screen_y as u16, crate::term::Cell::new(ch, st));
                    }
                    cx += w;
                    if cx >= frame.width() as i32 {
                        break;
                    }
                }
            }
            DisplayCommand::Image { rect, grid, .. } => {
                if !visible(rect.y, rect.height) {
                    continue;
                }
                for row in 0..grid.height {
                    let doc_y = rect.y + row;
                    let screen_y = doc_y - scroll_y;
                    if screen_y < 0 || screen_y >= page_h {
                        continue;
                    }
                    for col in 0..grid.width {
                        let screen_x = origin_x as i32 + rect.x + col;
                        if screen_x < 0 || screen_x >= frame.width() as i32 {
                            continue;
                        }
                        let Some((fg, bg)) = grid.cell(col, row) else {
                            continue;
                        };
                        // Transparent half → leave existing cell.
                        if fg == Color::Default && bg == Color::Default {
                            continue;
                        }
                        let style = Style {
                            fg: if fg == Color::Default {
                                Color::Rgb(0, 0, 0)
                            } else {
                                fg
                            },
                            bg: if bg == Color::Default {
                                Color::Rgb(0, 0, 0)
                            } else {
                                bg
                            },
                            attrs: crate::term::Attrs::NONE,
                        };
                        frame.set(
                            screen_x as u16,
                            screen_y as u16,
                            crate::term::Cell::new('▀', style),
                        );
                    }
                }
            }
        }
    }
}

/// Collect Kitty placements for **fully** visible image commands.
///
/// Partially scrolled images are omitted so we never paint over the status
/// line or leave a clipped Kitty rect that does not match half-blocks. The
/// cell path still draws the visible band.
pub fn kitty_placements(
    list: &DisplayList,
    origin_x: u16,
    scroll_y: i32,
    page_h: u16,
    frame_w: u16,
    id_base: u32,
) -> Vec<KittyPlacement> {
    let page_h = page_h as i32;
    let frame_w = frame_w as i32;
    let mut out = Vec::new();
    let mut id = id_base.max(1);
    for cmd in &list.commands {
        let DisplayCommand::Image {
            rect,
            pixels: Some(pixels),
            ..
        } = cmd
        else {
            continue;
        };
        if rect.width <= 0 || rect.height <= 0 {
            continue;
        }
        let screen_y = rect.y - scroll_y;
        let screen_x = origin_x as i32 + rect.x;
        // Fully inside the page viewport (not status line) and the frame.
        if screen_y < 0 || screen_y + rect.height > page_h {
            continue;
        }
        if screen_x < 0 || screen_x + rect.width > frame_w {
            continue;
        }
        out.push(KittyPlacement {
            col: (screen_x + 1) as u16, // 1-based CUP
            row: (screen_y + 1) as u16,
            cells_w: rect.width as u16,
            cells_h: rect.height as u16,
            image: Arc::clone(pixels),
            id,
        });
        id = id.wrapping_add(1).max(1);
    }
    out
}

/// Box-drawing border for a rectangle in document coords.
fn draw_border(
    frame: &mut crate::term::Frame,
    origin_x: u16,
    scroll_y: i32,
    page_h: i32,
    rect: Rect,
) {
    let style = Style::default();
    let put = |frame: &mut crate::term::Frame, dx: i32, dy: i32, ch: char| {
        let sy = dy - scroll_y;
        if sy < 0 || sy >= page_h {
            return;
        }
        let sx = origin_x as i32 + dx;
        if sx < 0 || sx >= frame.width() as i32 {
            return;
        }
        frame.set(sx as u16, sy as u16, crate::term::Cell::new(ch, style));
    };

    if rect.width <= 0 || rect.height <= 0 {
        return;
    }
    if rect.width == 1 && rect.height == 1 {
        put(frame, rect.x, rect.y, '□');
        return;
    }
    if rect.height == 1 {
        for c in 0..rect.width {
            let ch = if c == 0 {
                '╶'
            } else if c == rect.width - 1 {
                '╴'
            } else {
                '─'
            };
            put(frame, rect.x + c, rect.y, ch);
        }
        return;
    }
    if rect.width == 1 {
        for r in 0..rect.height {
            let ch = if r == 0 {
                '╷'
            } else if r == rect.height - 1 {
                '╵'
            } else {
                '│'
            };
            put(frame, rect.x, rect.y + r, ch);
        }
        return;
    }

    // Corners and edges.
    put(frame, rect.x, rect.y, '┌');
    put(frame, rect.x + rect.width - 1, rect.y, '┐');
    put(frame, rect.x, rect.y + rect.height - 1, '└');
    put(
        frame,
        rect.x + rect.width - 1,
        rect.y + rect.height - 1,
        '┘',
    );
    for c in 1..rect.width - 1 {
        put(frame, rect.x + c, rect.y, '─');
        put(frame, rect.x + c, rect.y + rect.height - 1, '─');
    }
    for r in 1..rect.height - 1 {
        put(frame, rect.x, rect.y + r, '│');
        put(frame, rect.x + rect.width - 1, rect.y + r, '│');
    }
}

// Keep term_color import used for potential future border colours.
#[allow(dead_code)]
fn _color(c: ColorValue) -> Color {
    term_color(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::image::{DecodedImage, ImageContext, ImgRef, cell_size};
    use crate::layout::{self, Hidden};
    use crate::style;
    use crate::term::Frame;
    use std::sync::Arc;

    fn tree(html: &str, css: &str, width: u16) -> layout::LayoutTree {
        let dom = html::parse(html);
        let sheet = crate::css::parse(css);
        let styles = style::style_tree(&dom, &[&sheet]);
        layout::layout_document(&dom, &styles, width, Hidden::Respect)
    }

    /// Every text command as `(x, y, text)`, in paint order.
    fn texts(list: &DisplayList) -> Vec<(i32, i32, String)> {
        list.commands
            .iter()
            .filter_map(|c| match c {
                DisplayCommand::Text { x, y, text, .. } => Some((*x, *y, text.clone())),
                _ => None,
            })
            .collect()
    }

    /// `(row, text)` per text command — what the reader can actually read.
    fn rows(list: &DisplayList) -> Vec<(i32, String)> {
        texts(list).into_iter().map(|(_, y, t)| (y, t)).collect()
    }

    #[test]
    fn text_commands_carry_the_glyphs() {
        let t = tree("<p>hi</p>", "", 40);
        let list = paint(&t);
        let texts: Vec<_> = list
            .commands
            .iter()
            .filter_map(|c| match c {
                DisplayCommand::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.contains("hi")), "{texts:?}");
    }

    #[test]
    fn background_emits_a_fill() {
        let t = tree(
            "<div>x</div>",
            "div { background-color: #eee; margin: 0; padding: 1em }",
            40,
        );
        let list = paint(&t);
        assert!(
            list.commands
                .iter()
                .any(|c| matches!(c, DisplayCommand::FillRect { .. })),
            "{list:?}"
        );
    }

    #[test]
    fn border_emits_box_drawing() {
        let t = tree(
            "<div>x</div>",
            "div { border: 1px solid black; margin: 0; padding: 0 }",
            40,
        );
        let list = paint(&t);
        assert!(
            list.commands
                .iter()
                .any(|c| matches!(c, DisplayCommand::Border { .. })),
            "{list:?}"
        );
        let mut frame = Frame::new(40, 10);
        paint_to_frame(&list, &mut frame, 0, 0, 10);
        // Top-left corner of the border somewhere on the first rows.
        let mut found = false;
        for y in 0..10u16 {
            for x in 0..40u16 {
                if frame.get(x, y).ch == '┌' {
                    found = true;
                }
            }
        }
        assert!(found, "expected a ┌ corner on screen");
    }

    #[test]
    fn scroll_offset_moves_content_up() {
        let t = tree("<p>one</p><p>two</p><p>three</p>", "p { margin: 0 }", 40);
        let list = paint(&t);
        let mut a = Frame::new(40, 2);
        paint_to_frame(&list, &mut a, 0, 0, 2);
        let mut b = Frame::new(40, 2);
        paint_to_frame(&list, &mut b, 0, 1, 2);
        // Scrolling by one row changes what is on screen.
        let row = |f: &Frame, y| (0..f.width()).map(|x| f.get(x, y).ch).collect::<String>();
        assert_ne!(row(&a, 0), row(&b, 0));
    }

    // ---- M9.3: overflow clipping, applied while the list is built ----

    #[test]
    fn a_zero_height_hidden_box_paints_nothing_and_takes_no_room() {
        // `height: 0; overflow: hidden` — the collapsed menu the whole
        // property exists for. The three paragraphs still have boxes; none of
        // them reaches a cell, and the next sibling starts where they are.
        let t = tree(
            "<div class=menu><p>one</p><p>two</p><p>three</p></div><p class=after>after</p>",
            "body,div,p{margin:0} .menu{height:0;overflow:hidden}",
            40,
        );
        assert_eq!(rows(&paint(&t)), [(0, "after".to_string())]);
    }

    #[test]
    fn max_height_hidden_keeps_exactly_the_rows_that_fit() {
        let t = tree(
            "<div class=card><p>one</p><p>two</p><p>three</p></div>",
            "body,div,p{margin:0} .card{max-height:2em;overflow:hidden}",
            40,
        );
        assert_eq!(
            rows(&paint(&t)),
            [(0, "one".to_string()), (1, "two".to_string())]
        );
    }

    #[test]
    fn a_horizontal_clip_truncates_by_cells_and_drops_a_straddling_wide_glyph() {
        // 10em = 20 cells, and `<pre>` does not wrap, so the run is wider than
        // the padding box. 漢 would occupy cells 19–20; cell 20 is outside, so
        // the glyph goes rather than being drawn as half of itself.
        let t = tree(
            "<div class=side><pre>0123456789abcdefghi漢字</pre></div>",
            "body,div,pre{margin:0} .side{width:10em;overflow:hidden}",
            40,
        );
        let texts = texts(&paint(&t));
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert_eq!(texts[0].0, 0, "the run still starts at the left edge");
        assert_eq!(texts[0].2, "0123456789abcdefghi");
    }

    #[test]
    fn a_clip_starting_mid_run_trims_from_the_left_and_moves_the_origin() {
        let clip = Clip::ranges(Some((5, 9)), None);
        assert_eq!(
            clip.trim_text(0, 0, "0123456789"),
            Some((5, "5678".to_string()))
        );
        // A wide glyph straddling the left edge is dropped with the cells it
        // cannot fit into: 漢 spans 4–5, so the run starts at 6.
        assert_eq!(clip.trim_text(4, 0, "漢ab"), Some((6, "ab".to_string())));
        // A row the clip excludes emits nothing at all.
        assert_eq!(Clip::ranges(None, Some((0, 2))).trim_text(0, 2, "x"), None);
    }

    #[test]
    fn one_clipped_axis_leaves_the_other_alone() {
        let css = "body,div,pre{margin:0}
                   .x{width:10em;height:1em;overflow-x:hidden}
                   .y{width:10em;height:1em;overflow-y:hidden}";
        // Two rows of 36 and 15 cells in a box 20 cells wide and 1 line tall.
        let pre = "<pre>0123456789abcdefghijklmnopqrstuvwxyz\nsecond row here</pre>";

        // Horizontal only: the second row still paints past the bottom edge,
        // and both rows are cut to the 20-cell padding box.
        let t = tree(&format!("<div class=x>{pre}</div>"), css, 40);
        let got: Vec<(i32, usize)> = texts(&paint(&t))
            .iter()
            .map(|(_, y, s)| (*y, s.chars().count()))
            .collect();
        assert_eq!(got, [(0, 20), (1, 15)]);

        // Vertical only: one row survives, and it keeps every one of its 36
        // cells even though the box is 20 wide.
        let t = tree(&format!("<div class=y>{pre}</div>"), css, 40);
        let got = texts(&paint(&t));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].2, "0123456789abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn nested_clips_intersect() {
        // Outer keeps 3 rows, inner keeps 20 cells: a child sticking out of
        // both is cut to the inner rectangle of the two.
        let t = tree(
            "<div class=outer><div class=inner><pre>0123456789abcdefghijklmnopqrstuvwxyz\n\
             0123456789abcdefghijklmnopqrstuvwxyz\n0123456789abcdefghijklmnopqrstuvwxyz\n\
             0123456789abcdefghijklmnopqrstuvwxyz\n0123456789abcdefghijklmnopqrstuvwxyz\
             </pre></div></div>",
            "body,div,pre{margin:0} .outer{height:3em;overflow:hidden}
             .inner{width:10em;height:5em;overflow:hidden}",
            40,
        );
        let got: Vec<(i32, usize)> = texts(&paint(&t))
            .iter()
            .map(|(_, y, s)| (*y, s.chars().count()))
            .collect();
        assert_eq!(got, [(0, 20), (1, 20), (2, 20)]);
    }

    #[test]
    fn a_clipped_background_and_border_are_trimmed_rather_than_dropped() {
        let rects = |list: &DisplayList| -> Vec<Rect> {
            list.commands
                .iter()
                .filter_map(|c| match c {
                    DisplayCommand::FillRect { rect, .. } | DisplayCommand::Border { rect } => {
                        Some(*rect)
                    }
                    _ => None,
                })
                .collect()
        };
        // A background four rows tall inside a box that shows two: the fill
        // covers the two surviving rows, not none of them and not four.
        let t = tree(
            "<div class=card><div class=fill><p>a</p><p>b</p><p>c</p><p>d</p></div></div>",
            "body,div,p{margin:0} .card{max-height:2em;overflow:hidden}
             .fill{background:#eee}",
            40,
        );
        assert_eq!(
            rects(&paint(&t)),
            [Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 2
            }]
        );

        // The recorded deviation, pinned so it cannot change unnoticed: a
        // border cut by the clip is emitted as the intersected rectangle, so
        // it closes along the clip edge instead of running off it.
        let t = tree(
            "<div class=card><div class=edged><p>a</p><p>b</p><p>c</p><p>d</p></div></div>",
            "body,div,p{margin:0} .card{max-height:2em;overflow:hidden}
             .edged{border:1px solid black}",
            40,
        );
        assert_eq!(
            rects(&paint(&t)),
            [Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 2
            }]
        );
    }

    #[test]
    fn a_clipped_image_crops_its_grid_and_falls_back_from_kitty() {
        // 32×64 px = 4 cells wide, 4 rows tall (PLAN.md unit table).
        let src = r#"<img src="https://ex/a.png" width="32" height="64" alt="">"#;
        let render = |html: &str| {
            let dom = html::parse(html);
            let styles = style::style_tree(&dom, &[]);
            let mut ctx = ImageContext::default();
            for img in crate::image::discover(&dom, Some("https://ex/")) {
                ctx.by_node.insert(img.node, img);
            }
            let tree = layout::layout_document_with(&dom, &styles, 40, Hidden::Respect, &ctx);
            let mut pixels = ImagePixels::new();
            pixels.insert(
                "https://ex/a.png".into(),
                Arc::new(DecodedImage::new(2, 2, [255, 0, 0, 255].repeat(4))),
            );
            paint_with(&tree, &pixels)
        };
        let image_of = |list: &DisplayList| {
            list.commands
                .iter()
                .find_map(|c| match c {
                    DisplayCommand::Image { rect, grid, pixels } => {
                        Some((*rect, grid.clone(), pixels.is_some()))
                    }
                    _ => None,
                })
                .expect("an image command")
        };

        // Unclipped: full rectangle, full grid, and Kitty gets a placement.
        let list = render(&format!("<div style='margin:0'>{src}</div>"));
        let (rect, grid, has_pixels) = image_of(&list);
        assert_eq!((rect.width, rect.height), (4, 4));
        assert_eq!((grid.width, grid.height), (4, 4));
        assert!(has_pixels);
        assert_eq!(kitty_placements(&list, 0, 0, 24, 80, 1).len(), 1);

        // Clipped to one row: the grid is cropped to the cells that survive,
        // and the placement is dropped — half-blocks draw the visible band.
        let list = render(&format!(
            "<div style='margin:0;height:1em;overflow:hidden'>{src}</div>"
        ));
        let (rect, grid, has_pixels) = image_of(&list);
        assert_eq!((rect.width, rect.height), (4, 1));
        assert_eq!((grid.width, grid.height), (4, 1));
        assert!(!has_pixels, "a cropped image must not go through Kitty");
        assert!(kitty_placements(&list, 0, 0, 24, 80, 1).is_empty());
    }

    #[test]
    fn image_box_paints_halfblocks() {
        let dom = html::parse(r#"<img src="https://ex/a.png" width="16" height="16" alt="x">"#);
        let styles = style::style_tree(&dom, &[]);
        let imgs = crate::image::discover(&dom, Some("https://ex/"));
        let mut ctx = ImageContext::default();
        for img in &imgs {
            ctx.by_node.insert(img.node, img.clone());
        }
        let tree = layout::layout_document_with(&dom, &styles, 40, Hidden::Respect, &ctx);
        let mut pixels = ImagePixels::new();
        // 2×2 red
        let img = Arc::new(DecodedImage::new(
            2,
            2,
            vec![
                255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
            ],
        ));
        pixels.insert("https://ex/a.png".into(), img);
        let list = paint_with(&tree, &pixels);
        assert!(
            list.commands
                .iter()
                .any(|c| matches!(c, DisplayCommand::Image { .. })),
            "{list:?}"
        );
        let mut frame = Frame::new(40, 20);
        paint_to_frame(&list, &mut frame, 0, 0, 20);
        let mut saw = false;
        for y in 0..20u16 {
            for x in 0..40u16 {
                if frame.get(x, y).ch == '▀' {
                    saw = true;
                }
            }
        }
        assert!(saw, "expected half-block cells");
        let _ = cell_size;
        let _ = ImgRef {
            node: crate::dom::NodeId(0),
            url: String::new(),
            alt: String::new(),
            attr_w: None,
            attr_h: None,
        };
    }
}
