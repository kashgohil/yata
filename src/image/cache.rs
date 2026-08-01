//! Memory-capped LRU of decoded images (PLAN.md M8).
//!
//! Keyed by absolute URL. Navigation clears the page's in-flight set but keeps
//! this cache so back/forward is instant. [`get`] refreshes recency so current
//! and revisited pages stay under the cap.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use super::DecodedImage;

/// Default cap: ~32 MiB of RGBA. Enough for a Wikipedia article of images
/// without pinning gigabytes after a long session.
pub const DEFAULT_CAP_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ImageCache {
    map: HashMap<String, Arc<DecodedImage>>,
    /// Oldest at the front.
    order: VecDeque<String>,
    bytes: usize,
    cap: usize,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::with_cap(DEFAULT_CAP_BYTES)
    }
}

impl ImageCache {
    pub fn with_cap(cap: usize) -> Self {
        ImageCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap: cap.max(1),
        }
    }

    /// Lookup and mark as most-recently used.
    pub fn get(&mut self, url: &str) -> Option<Arc<DecodedImage>> {
        let img = self.map.get(url).cloned()?;
        self.touch(url);
        Some(img)
    }

    /// Lookup without changing LRU order.
    pub fn peek(&self, url: &str) -> Option<Arc<DecodedImage>> {
        self.map.get(url).cloned()
    }

    pub fn contains(&self, url: &str) -> bool {
        self.map.contains_key(url)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn bytes_used(&self) -> usize {
        self.bytes
    }

    fn touch(&mut self, url: &str) {
        if let Some(pos) = self.order.iter().position(|u| u == url) {
            let u = self.order.remove(pos).expect("pos valid");
            self.order.push_back(u);
        }
    }

    /// Insert or refresh. Returns the shared image. Evicts oldest until under
    /// the byte cap (always keeps the just-inserted entry even if it alone
    /// exceeds the cap).
    pub fn insert(&mut self, url: String, image: DecodedImage) -> Arc<DecodedImage> {
        let arc = Arc::new(image);
        if let Some(old) = self.map.remove(&url) {
            self.bytes = self.bytes.saturating_sub(old.byte_size());
            self.order.retain(|u| u != &url);
        }
        self.bytes += arc.byte_size();
        self.map.insert(url.clone(), Arc::clone(&arc));
        self.order.push_back(url);
        self.evict();
        arc
    }

    fn evict(&mut self) {
        while self.bytes > self.cap && self.order.len() > 1 {
            let Some(old_url) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.map.remove(&old_url) {
                self.bytes = self.bytes.saturating_sub(old.byte_size());
            }
        }
        // Single oversized entry: still keep it (can't show the page otherwise).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny(n: u8) -> DecodedImage {
        DecodedImage::new(1, 1, vec![n, 0, 0, 255])
    }

    #[test]
    fn hit_after_insert() {
        let mut c = ImageCache::with_cap(1024);
        c.insert("https://a/x.png".into(), tiny(1));
        assert!(c.contains("https://a/x.png"));
        assert_eq!(c.get("https://a/x.png").unwrap().rgba[0], 1);
    }

    #[test]
    fn eviction_drops_oldest() {
        let mut c = ImageCache::with_cap(8);
        c.insert("a".into(), tiny(1));
        c.insert("b".into(), tiny(2));
        c.insert("c".into(), tiny(3));
        assert!(!c.contains("a"), "oldest should be gone");
        assert!(c.contains("b"));
        assert!(c.contains("c"));
        assert!(c.bytes_used() <= 8 || c.len() == 1);
    }

    #[test]
    fn reinsert_refreshes_recency() {
        let mut c = ImageCache::with_cap(8);
        c.insert("a".into(), tiny(1));
        c.insert("b".into(), tiny(2));
        c.insert("a".into(), tiny(1));
        c.insert("c".into(), tiny(3));
        assert!(c.contains("a"));
        assert!(!c.contains("b"));
        assert!(c.contains("c"));
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c = ImageCache::with_cap(8);
        c.insert("a".into(), tiny(1));
        c.insert("b".into(), tiny(2));
        // Touch a so b is oldest.
        assert!(c.get("a").is_some());
        c.insert("c".into(), tiny(3));
        assert!(c.contains("a"), "touched a must survive");
        assert!(!c.contains("b"), "untouched b should go");
        assert!(c.contains("c"));
    }
}
