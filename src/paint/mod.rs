//! Paint: layout tree → display list (PLAN.md M5/M8).
//!
//! Pure transform. The display list is what scrolling re-emits at a new offset
//! — no restyle, no relayout (CLAUDE.md). Commands are in document coordinates;
//! the viewport subtracts the scroll offset when drawing.

use std::collections::HashMap;
use std::sync::Arc;

use crate::image::{
    DecodedImage, HalfBlockGrid, KittyPlacement, placeholder_grid, raster_halfblocks,
};
use crate::layout::{BoxId, BoxKind, LayoutTree, Rect, term_color};
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
    paint_box(tree, tree.root, images, &mut list);
    list
}

fn paint_box(tree: &LayoutTree, id: BoxId, images: &ImagePixels, list: &mut DisplayList) {
    let b = tree.get(id);
    match b.kind {
        BoxKind::Block => {
            // Background fills the padding box (CSS).
            if let ColorValue::Rgb(r, g, bcol) = b.computed.background_color {
                let rect = b.dimensions.padding_box();
                if rect.width > 0 && rect.height > 0 {
                    list.commands.push(DisplayCommand::FillRect {
                        rect,
                        color: Color::Rgb(r, g, bcol),
                    });
                }
            }
            // Border: any nonzero border edge draws the full rectangle outline.
            let border = b.dimensions.border;
            if border.top > 0 || border.right > 0 || border.bottom > 0 || border.left > 0 {
                let rect = b.dimensions.border_box();
                if rect.width > 0 && rect.height > 0 {
                    list.commands.push(DisplayCommand::Border { rect });
                }
            }
        }
        BoxKind::Text => {
            if let Some(text) = &b.text
                && !text.is_empty()
            {
                list.commands.push(DisplayCommand::Text {
                    x: b.dimensions.content.x,
                    y: b.dimensions.content.y,
                    text: text.clone(),
                    style: b.term_style,
                });
            }
        }
        BoxKind::Image => {
            let rect = b.dimensions.content;
            if rect.width > 0 && rect.height > 0 {
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
                // Prefer alt text overlay on the first row when still loading
                // and alt is non-empty — half-block checker stays underneath
                // for empty alt.
                if pixels.is_none()
                    && let Some(alt) = b.text.as_ref().filter(|t| !t.is_empty())
                {
                    list.commands.push(DisplayCommand::Image {
                        rect,
                        grid,
                        pixels: None,
                    });
                    list.commands.push(DisplayCommand::Text {
                        x: rect.x,
                        y: rect.y,
                        text: truncate_cells(alt, rect.width),
                        style: Style {
                            fg: Color::Rgb(0xc0, 0xc0, 0xc0),
                            bg: Color::Rgb(0x40, 0x40, 0x40),
                            attrs: crate::term::Attrs::NONE,
                        },
                    });
                    for &child in &b.children {
                        paint_box(tree, child, images, list);
                    }
                    return;
                }
                list.commands
                    .push(DisplayCommand::Image { rect, grid, pixels });
            }
        }
        BoxKind::AnonymousBlock | BoxKind::Line | BoxKind::Inline => {}
    }
    for &child in &b.children {
        paint_box(tree, child, images, list);
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
