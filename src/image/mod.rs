//! Images: decode, LRU cache, half-block raster, Kitty graphics (PLAN.md M8).
//!
//! Layout and paint stay pure transforms. This module holds pixel data and the
//! helpers that turn it into terminal output; workers decode off the UI thread
//! and deliver [`DecodedImage`] values through `Msg::Image`.

mod cache;
mod halfblock;
mod kitty;

pub use cache::ImageCache;
pub use halfblock::{HalfBlockGrid, placeholder_grid, raster_halfblocks};
pub use kitty::{
    KittyPlacement, delete_all_images, delete_all_placements, place_sequence, placement_sequence,
    transmit_sequence,
};

use std::sync::Arc;

use crate::dom::{Dom, NodeData, NodeId};
use crate::net;

/// RGBA8 bitmap. Shared via `Arc` so the LRU, page store, and display list can
/// all hold the same pixels without copying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Length is always `width * height * 4` (RGBA).
    pub rgba: Arc<[u8]>,
}

impl DecodedImage {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        debug_assert_eq!(rgba.len(), width as usize * height as usize * 4);
        DecodedImage {
            width,
            height,
            rgba: rgba.into(),
        }
    }

    pub fn byte_size(&self) -> usize {
        self.rgba.len()
    }
}

/// Decode image bytes (any format the `image` crate feature set supports).
pub fn decode(bytes: &[u8]) -> Result<DecodedImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    Ok(DecodedImage::new(w, h, rgba.into_raw()))
}

/// PLAN.md unit table: 8px ≈ 1 cell wide, 16px ≈ 1 line tall.
pub fn px_to_cells_w(px: u32) -> i32 {
    ((px as f64) / 8.0).round().max(1.0) as i32
}

pub fn px_to_cells_h(px: u32) -> i32 {
    ((px as f64) / 16.0).round().max(1.0) as i32
}

/// Parse an HTML length attribute used for image width/height. Bare numbers are
/// CSS pixels; trailing `px` is accepted; percentages are ignored (`None`).
pub fn parse_dim_attr(raw: &str) -> Option<u32> {
    let s = raw.trim();
    if s.is_empty() || s.contains('%') {
        return None;
    }
    let s = s.strip_suffix("px").unwrap_or(s).trim();
    let n: f64 = s.parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some(n.round() as u32)
}

/// One `<img>` discovered in the document, ready for layout and fetch.
#[derive(Clone, Debug)]
pub struct ImgRef {
    pub node: NodeId,
    /// Absolute URL (resolved against the page), or the original src if
    /// resolution failed — fetch will then fail softly.
    pub url: String,
    pub alt: String,
    pub attr_w: Option<u32>,
    pub attr_h: Option<u32>,
}

/// Walk the DOM for `<img src>` elements in document order.
pub fn discover(dom: &Dom, base_url: Option<&str>) -> Vec<ImgRef> {
    let mut out = Vec::new();
    walk(dom, dom.root, base_url, &mut out);
    out
}

fn walk(dom: &Dom, id: NodeId, base: Option<&str>, out: &mut Vec<ImgRef>) {
    if let NodeData::Element { tag, .. } = &dom.node(id).data
        && tag == "img"
    {
        if let Some(src) = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty()) {
            // data: URLs are in-document; we could decode them later. For M8
            // the network path is the product — skip data: to avoid stuffing
            // megabytes into Msg without a fetch id story.
            if !src.starts_with("data:") {
                let url = base
                    .and_then(|b| net::resolve_url(b, src))
                    .unwrap_or_else(|| src.to_string());
                out.push(ImgRef {
                    node: id,
                    url,
                    alt: dom.attr(id, "alt").unwrap_or("").to_string(),
                    attr_w: dom.attr(id, "width").and_then(parse_dim_attr),
                    attr_h: dom.attr(id, "height").and_then(parse_dim_attr),
                });
            }
        }
        return; // void
    }
    for child in dom.children(id) {
        walk(dom, child, base, out);
    }
}

/// Cell size for an image given HTML attrs and optional decoded pixels.
///
/// Returns `(cells_w, cells_h, size_firm)` where `size_firm` means a late
/// decode must **not** force relayout (both attrs present, or we already used
/// decoded dimensions).
pub fn cell_size(
    attr_w: Option<u32>,
    attr_h: Option<u32>,
    decoded: Option<(u32, u32)>,
    containing_width: i32,
) -> (i32, i32, bool) {
    let containing_width = containing_width.max(1);
    match (attr_w, attr_h, decoded) {
        (Some(w), Some(h), _) => (px_to_cells_w(w), px_to_cells_h(h), true),
        (Some(w), None, Some((dw, dh))) => {
            let cw = px_to_cells_w(w);
            let ch = if dw > 0 {
                // Preserve aspect in cell space: h_cells ≈ w_cells * (dh/dw) * (8/16).
                let h_px = (w as f64) * (dh as f64) / (dw as f64);
                px_to_cells_h(h_px.round().max(1.0) as u32)
            } else {
                1
            };
            (cw, ch.max(1), true)
        }
        (None, Some(h), Some((dw, dh))) => {
            let ch = px_to_cells_h(h);
            let cw = if dh > 0 {
                let w_px = (h as f64) * (dw as f64) / (dh as f64);
                px_to_cells_w(w_px.round().max(1.0) as u32)
            } else {
                1
            };
            (cw.min(containing_width).max(1), ch, true)
        }
        (Some(w), None, None) => (px_to_cells_w(w).clamp(1, containing_width), 3, false),
        (None, Some(h), None) => {
            let ch = px_to_cells_h(h);
            (containing_width.clamp(1, 20), ch, false)
        }
        (None, None, Some((dw, dh))) => {
            let cw = px_to_cells_w(dw).clamp(1, containing_width);
            let ch = px_to_cells_h(dh).max(1);
            (cw, ch, true)
        }
        (None, None, None) => {
            let cw = containing_width.clamp(1, 20);
            (cw, 3, false)
        }
    }
}

/// Pure layout input: everything layout needs about images without holding
/// pixel buffers.
#[derive(Clone, Debug, Default)]
pub struct ImageContext {
    /// Absolute URL → decoded pixel size (from cache or page store).
    pub decoded_px: std::collections::HashMap<String, (u32, u32)>,
    /// Node → absolute URL + alt + attrs (from discovery). Empty → no images.
    pub by_node: std::collections::HashMap<NodeId, ImgRef>,
}

impl ImageContext {
    pub fn from_discovery(imgs: &[ImgRef], decoded: &mut ImageCache) -> Self {
        let mut decoded_px = std::collections::HashMap::new();
        let mut by_node = std::collections::HashMap::new();
        for img in imgs {
            if let Some(d) = decoded.get(&img.url) {
                decoded_px.insert(img.url.clone(), (d.width, d.height));
            }
            by_node.insert(img.node, img.clone());
        }
        ImageContext {
            decoded_px,
            by_node,
        }
    }

    pub fn size_for(&self, img: &ImgRef, containing_width: i32) -> (i32, i32, bool) {
        let dec = self.decoded_px.get(&img.url).copied();
        cell_size(img.attr_w, img.attr_h, dec, containing_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn discover_resolves_relative_src() {
        let dom = html::parse(r#"<img src="/pic.png" alt="a"><img src="https://x/y.jpg">"#);
        let imgs = discover(&dom, Some("https://example.com/page"));
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].url, "https://example.com/pic.png");
        assert_eq!(imgs[0].alt, "a");
        assert_eq!(imgs[1].url, "https://x/y.jpg");
    }

    #[test]
    fn discover_skips_empty_and_data() {
        let dom =
            html::parse(r#"<img src=""><img src="data:image/png;base64,xx"><img src="z.png">"#);
        let imgs = discover(&dom, Some("https://ex/"));
        assert_eq!(imgs.len(), 1);
        assert!(imgs[0].url.ends_with("z.png"));
    }

    #[test]
    fn cell_size_from_both_attrs() {
        let (w, h, firm) = cell_size(Some(80), Some(48), None, 90);
        assert_eq!((w, h, firm), (10, 3, true));
    }

    #[test]
    fn cell_size_from_decoded() {
        let (w, h, firm) = cell_size(None, None, Some((160, 80)), 90);
        assert_eq!(w, 20);
        assert_eq!(h, 5);
        assert!(firm);
    }

    #[test]
    fn cell_size_placeholder_is_soft() {
        let (w, h, firm) = cell_size(None, None, None, 90);
        assert_eq!(w, 20);
        assert_eq!(h, 3);
        assert!(!firm);
    }

    #[test]
    fn decode_1x1_png() {
        // Encode with the same crate so the bytes are always a valid PNG.
        let mut png = Vec::new();
        {
            let enc = image::codecs::png::PngEncoder::new(&mut png);
            use image::ImageEncoder;
            enc.write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
        }
        let img = decode(&png).expect("png");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba.len(), 4);
        assert_eq!(&img.rgba[..3], &[255, 0, 0]);
    }
}
