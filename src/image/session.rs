//! Page + session image bookkeeping (M8).
//!
//! Keeps the LRU, the current page's `<img>` list, and Kitty placement state out
//! of `App` so navigation/layout/paint stay thin. Layout still receives a pure
//! [`ImageContext`]; Kitty is a post-present side channel.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::cache::ImageCache;
use super::kitty::{
    KittyPlacement, delete_all_images, delete_all_placements, place_sequence, transmit_sequence,
};
use super::{DecodedImage, ImageContext, ImgRef, discover};
use crate::dom::Dom;
use crate::layout::{BoxKind, LayoutTree};
use crate::net::PageId;
use crate::paint::{DisplayList, ImagePixels, kitty_placements};

/// One browsing session's images: global LRU + current page + Kitty uploads.
#[derive(Debug)]
pub struct ImageSession {
    cache: SharedImageCache,
    page_imgs: Vec<ImgRef>,
    kitty_enabled: bool,
    /// We currently have at least one Kitty placement on screen.
    kitty_active: bool,
    /// Absolute URL → Kitty image id already uploaded (no re-base64 on scroll).
    uploaded: HashMap<String, u32>,
    next_id: u32,
    /// Last emitted placement geometry; identical → emit nothing.
    last_sig: Vec<PlaceSig>,
}

pub type SharedImageCache = Rc<RefCell<ImageCache>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaceSig {
    col: u16,
    row: u16,
    cells_w: u16,
    cells_h: u16,
    image_id: u32,
}

impl ImageSession {
    pub fn new(kitty_enabled: bool) -> Self {
        Self::with_cache(kitty_enabled, Rc::new(RefCell::new(ImageCache::default())))
    }

    pub fn with_cache(kitty_enabled: bool, cache: SharedImageCache) -> Self {
        ImageSession {
            cache,
            page_imgs: Vec::new(),
            kitty_enabled,
            kitty_active: false,
            uploaded: HashMap::new(),
            next_id: 1,
            last_sig: Vec::new(),
        }
    }

    /// Drop page-local bookkeeping (not the LRU or Kitty uploads).
    pub fn clear_page(&mut self) {
        self.page_imgs.clear();
        // Placements are document-relative; force a delete on next frame if any.
        self.last_sig.clear();
    }

    pub fn page_imgs(&self) -> &[ImgRef] {
        &self.page_imgs
    }

    pub fn cache_contains(&self, url: &str) -> bool {
        self.cache.borrow().contains(url)
    }

    /// Discover imgs and return absolute URLs that still need a network fetch.
    pub fn adopt(&mut self, dom: &Dom, base: Option<&str>, id: PageId) -> Vec<(PageId, String)> {
        self.page_imgs = discover(dom, base);
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        for img in &self.page_imgs {
            if !seen.insert(img.url.clone()) {
                continue;
            }
            if self.cache.borrow().contains(&img.url) {
                // Protect current-page URLs under LRU pressure.
                let _ = self.cache.borrow_mut().get(&img.url);
                continue;
            }
            pending.push((id, img.url.clone()));
        }
        pending
    }

    pub fn insert(&mut self, url: String, image: DecodedImage) {
        // New pixels invalidate any prior Kitty upload for this URL.
        self.uploaded.remove(&url);
        self.cache.borrow_mut().insert(url, image);
        self.last_sig.clear();
    }

    pub fn context(&mut self) -> ImageContext {
        ImageContext::from_discovery(&self.page_imgs, &mut self.cache.borrow_mut())
    }

    pub fn pixels(&mut self) -> ImagePixels {
        let mut map = ImagePixels::new();
        for img in &self.page_imgs {
            if let Some(d) = self.cache.borrow_mut().get(&img.url) {
                map.insert(img.url.clone(), d);
            }
        }
        map
    }

    /// Relayout only when a laid-out box for `url` still has a soft size.
    pub fn needs_relayout(&self, url: &str, tree: Option<&LayoutTree>) -> bool {
        let Some(tree) = tree else {
            return false;
        };
        let mut soft = false;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Image && b.image_src.as_deref() == Some(url) && !b.image_size_firm
            {
                soft = true;
            }
        });
        soft
    }

    /// Kitty bytes after the cell present, or `None` when nothing to write.
    ///
    /// Scroll path: if placement geometry is unchanged, returns `None`. When
    /// geometry changes, re-places already-uploaded images (`a=p`) without
    /// re-encoding RGBA. Full transmit happens once per URL per upload.
    /// Partially off-screen images are omitted (half-blocks already paint them).
    pub fn kitty_frame(
        &mut self,
        list: &DisplayList,
        origin: (u16, u16),
        scroll_y: i32,
        page_h: u16,
        frame_w: u16,
        on_page_surface: bool,
    ) -> Option<Vec<u8>> {
        if !self.kitty_enabled {
            return None;
        }
        if !on_page_surface {
            return self.clear_kitty_screen();
        }

        let placements = kitty_placements(list, origin.0, scroll_y, page_h, frame_w, 1);
        if placements.is_empty() {
            return self.clear_kitty_screen();
        }

        // Assign stable Kitty ids per URL; build signature.
        let mut planned: Vec<(KittyPlacement, u32, bool /* need_upload */)> = Vec::new();
        let mut sig = Vec::with_capacity(placements.len());
        for mut p in placements {
            p.row = p.row.saturating_add(origin.1);
            let url_key = image_key(&p.image);
            let (image_id, need_upload) = match self.uploaded.get(&url_key) {
                Some(&id) => (id, false),
                None => {
                    let id = self.alloc_id();
                    (id, true)
                }
            };
            p.id = image_id;
            sig.push(PlaceSig {
                col: p.col,
                row: p.row,
                cells_w: p.cells_w,
                cells_h: p.cells_h,
                image_id,
            });
            planned.push((p, image_id, need_upload));
        }

        if sig == self.last_sig {
            return None;
        }

        let mut buf = Vec::new();
        // Drop old placements but keep uploaded bitmaps when possible.
        if self.kitty_active {
            buf.extend_from_slice(delete_all_placements());
        }

        for (p, image_id, need_upload) in &planned {
            if *need_upload {
                buf.extend_from_slice(&transmit_sequence(*image_id, &p.image));
                // Key by a stable id derived from pixel buffer pointer + size.
                self.uploaded.insert(image_key(&p.image), *image_id);
            }
            buf.extend_from_slice(&place_sequence(p));
        }

        self.last_sig = sig;
        self.kitty_active = true;
        Some(buf)
    }

    fn clear_kitty_screen(&mut self) -> Option<Vec<u8>> {
        if !self.kitty_active && self.last_sig.is_empty() {
            return None;
        }
        self.kitty_active = false;
        self.last_sig.clear();
        // Free terminal-side image memory on full clear (leave page / no imgs).
        Some(delete_all_images().to_vec())
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

/// Stable key for an uploaded bitmap: address of the Arc payload + dimensions.
/// Same Arc (cache hit) → same key → no re-upload.
fn image_key(img: &Arc<DecodedImage>) -> String {
    format!(
        "{:p}:{}x{}",
        Arc::as_ptr(img) as *const u8,
        img.width,
        img.height
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::layout;
    use crate::paint;
    use crate::style;

    #[test]
    fn kitty_frame_is_none_when_idle() {
        let mut s = ImageSession::new(true);
        let list = DisplayList::default();
        assert!(s.kitty_frame(&list, (0, 0), 0, 20, 80, true).is_none());
    }

    #[test]
    fn kitty_frame_noop_on_identical_geometry() {
        let mut s = ImageSession::new(true);
        let dom = html::parse(r#"<img src="https://ex/a.png" width="16" height="16">"#);
        let styles = style::style_tree(&dom, &[]);
        s.adopt(&dom, Some("https://ex/"), PageId::headless(1));
        s.insert(
            "https://ex/a.png".into(),
            DecodedImage::new(
                2,
                2,
                vec![
                    255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
                ],
            ),
        );
        let ctx = s.context();
        let tree = layout::layout_document_with(&dom, &styles, 40, layout::Hidden::Respect, &ctx);
        let list = paint::paint_with(&tree, &s.pixels());
        let first = s.kitty_frame(&list, (0, 0), 0, 20, 80, true);
        assert!(first.is_some());
        let second = s.kitty_frame(&list, (0, 0), 0, 20, 80, true);
        assert!(second.is_none(), "identical frame must not retransmit");
    }

    #[test]
    fn second_frame_reuses_upload_without_payload() {
        let mut s = ImageSession::new(true);
        let dom = html::parse(r#"<img src="https://ex/a.png" width="16" height="32">"#);
        let styles = style::style_tree(&dom, &[]);
        s.adopt(&dom, Some("https://ex/"), PageId::headless(1));
        s.insert(
            "https://ex/a.png".into(),
            DecodedImage::new(2, 2, vec![255; 16]),
        );
        let ctx = s.context();
        let tree = layout::layout_document_with(&dom, &styles, 40, layout::Hidden::Respect, &ctx);
        let list = paint::paint_with(&tree, &s.pixels());
        let first = s.kitty_frame(&list, (0, 0), 0, 20, 80, true).unwrap();
        let s1 = String::from_utf8_lossy(&first);
        assert!(
            s1.contains("a=t") || s1.contains("a=T"),
            "first frame transmits: {s1}"
        );
        // Shift origin → placement geometry changes while still fully on-screen.
        // Must re-place without re-encoding RGBA.
        let second = s.kitty_frame(&list, (1, 0), 0, 20, 80, true).unwrap();
        let s2 = String::from_utf8_lossy(&second);
        assert!(s2.contains("a=p"), "geometry change should re-place: {s2}");
        assert!(!s2.contains("f=32"), "must not retransmit pixels: {s2}");
    }
}
