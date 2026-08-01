//! Unicode half-block rasterization: two vertical pixels per cell via `▀`.

use crate::term::Color;

use super::DecodedImage;

/// Precomputed half-block grid for one image box (`w` columns × `h` rows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HalfBlockGrid {
    pub width: i32,
    pub height: i32,
    /// Row-major: `height` rows of `width` cells, each (fg, bg) for `▀`.
    pub cells: Vec<(Color, Color)>,
}

impl HalfBlockGrid {
    pub fn cell(&self, x: i32, y: i32) -> Option<(Color, Color)> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        self.cells.get((y * self.width + x) as usize).copied()
    }

    /// The `width` × `height` sub-grid starting at (`x`, `y`) — an image
    /// cropped by a clip (M9.3) keeps the cells it still covers rather than
    /// being rescaled into the surviving rectangle, which would squash it.
    /// Cells outside the source are transparent (`Default`), so a crop asking
    /// for more than there is degrades instead of panicking.
    pub fn crop(&self, x: i32, y: i32, width: i32, height: i32) -> HalfBlockGrid {
        let width = width.max(0);
        let height = height.max(0);
        let mut cells = Vec::with_capacity((width * height) as usize);
        for row in 0..height {
            for col in 0..width {
                cells.push(
                    self.cell(x + col, y + row)
                        .unwrap_or((Color::Default, Color::Default)),
                );
            }
        }
        HalfBlockGrid {
            width,
            height,
            cells,
        }
    }
}

/// Scale `img` into a `cells_w` × `cells_h` half-block grid (nearest neighbour).
/// Each cell samples two vertical source bands (upper → fg, lower → bg).
pub fn raster_halfblocks(img: &DecodedImage, cells_w: i32, cells_h: i32) -> HalfBlockGrid {
    let cells_w = cells_w.max(1);
    let cells_h = cells_h.max(1);
    let mut cells = Vec::with_capacity((cells_w * cells_h) as usize);

    let src_w = img.width as f64;
    let src_h = img.height as f64;
    // Half-blocks: cell row y covers source rows [2y, 2y+1] in a virtual
    //  cells_w × (cells_h*2) pixel grid.
    let virt_h = (cells_h * 2) as f64;

    for cy in 0..cells_h {
        for cx in 0..cells_w {
            let sx = ((cx as f64 + 0.5) * src_w / cells_w as f64) as u32;
            let sy0 = ((cy as f64 * 2.0 + 0.5) * src_h / virt_h) as u32;
            let sy1 = ((cy as f64 * 2.0 + 1.5) * src_h / virt_h) as u32;
            let fg = sample(img, sx.min(img.width - 1), sy0.min(img.height - 1));
            let bg = sample(img, sx.min(img.width - 1), sy1.min(img.height - 1));
            cells.push((fg, bg));
        }
    }

    HalfBlockGrid {
        width: cells_w,
        height: cells_h,
        cells,
    }
}

fn sample(img: &DecodedImage, x: u32, y: u32) -> Color {
    let i = ((y * img.width + x) * 4) as usize;
    let r = img.rgba[i];
    let g = img.rgba[i + 1];
    let b = img.rgba[i + 2];
    let a = img.rgba[i + 3];
    if a < 128 {
        // Transparent → default so the page background shows through.
        Color::Default
    } else {
        Color::Rgb(r, g, b)
    }
}

/// Dim placeholder fill for a reserved image rect (not yet loaded / failed).
pub fn placeholder_grid(cells_w: i32, cells_h: i32) -> HalfBlockGrid {
    let cells_w = cells_w.max(1);
    let cells_h = cells_h.max(1);
    let dim = Color::Rgb(0x40, 0x40, 0x40);
    let mid = Color::Rgb(0x60, 0x60, 0x60);
    let cells = (0..cells_w * cells_h)
        .map(|i| {
            if (i / cells_w + i % cells_w) % 2 == 0 {
                (dim, mid)
            } else {
                (mid, dim)
            }
        })
        .collect();
    HalfBlockGrid {
        width: cells_w,
        height: cells_h,
        cells,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> DecodedImage {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
        DecodedImage {
            width: w,
            height: h,
            rgba: Arc::from(rgba),
        }
    }

    #[test]
    fn solid_red_halfblocks() {
        let img = solid(2, 2, 255, 0, 0);
        let grid = raster_halfblocks(&img, 1, 1);
        assert_eq!(grid.width, 1);
        assert_eq!(grid.height, 1);
        let (fg, bg) = grid.cell(0, 0).unwrap();
        assert_eq!(fg, Color::Rgb(255, 0, 0));
        assert_eq!(bg, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn crop_keeps_the_cells_it_still_covers() {
        // A clipped image (M9.3) keeps its scale and loses the cells outside
        // the clip — it is not re-rastered into the surviving rectangle.
        let img = solid(8, 8, 0, 0, 255);
        let grid = raster_halfblocks(&img, 4, 4);
        let cropped = grid.crop(1, 2, 2, 2);
        assert_eq!((cropped.width, cropped.height), (2, 2));
        assert_eq!(cropped.cell(0, 0), grid.cell(1, 2));
        assert_eq!(cropped.cell(1, 1), grid.cell(2, 3));
        // Asking past the edge degrades to transparent rather than panicking.
        let past = grid.crop(3, 3, 3, 3);
        assert_eq!(past.cell(2, 2), Some((Color::Default, Color::Default)));
    }

    #[test]
    fn grid_dimensions_match_request() {
        let img = solid(100, 50, 0, 255, 0);
        let grid = raster_halfblocks(&img, 10, 3);
        assert_eq!(grid.cells.len(), 30);
        assert_eq!((grid.width, grid.height), (10, 3));
    }
}
