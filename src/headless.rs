//! The headless hooks' shared pipeline (PLAN.md M9.1).
//!
//! `--dump-boxes` and the layout goldens in `tests/layout.rs` must show the
//! same boxes, or the goldens pin something no one can see on screen. That
//! means one function, not two call sites that each remember to style the
//! page, run image discovery and pick the same hidden-content rule.
//!
//! Everything downstream of the parse, and nothing upstream of it: the DOM
//! comes from the caller (in `--dump-boxes` it is the fetch worker's own
//! parse, never a second one).

use crate::browser::inspector;
use crate::dom::Dom;
use crate::image::{self, ImageCache, ImageContext};
use crate::layout;
use crate::style;

/// Style → layout → `F3` box lines, as one newline-terminated block of text.
///
/// No network: `<link>` sheets are not fetched (the page is styled by the UA
/// sheet plus its own inline blocks) and no image bytes exist, so images lay
/// out the way the TUI shows them before the first byte arrives — sized from
/// `width`/`height` attrs when the page gives them, placeholder-sized when it
/// does not. Discovery still runs; without it layout drops `<img>` entirely.
pub fn box_dump(dom: &Dom, base_url: Option<&str>, width: u16) -> String {
    let sheets = style::sources::inline_sheets(dom);
    let styles = style::style_tree(dom, &sheets.iter().collect::<Vec<_>>());
    let imgs = image::discover(dom, base_url);
    let img_ctx = ImageContext::from_discovery(&imgs, &mut ImageCache::default());
    let (tree, _revealed) = layout::layout_document_readable(dom, &styles, width, &img_ctx);
    let mut text = inspector::box_lines(dom, &tree).join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn images_are_discovered_so_their_boxes_appear() {
        // Regression: with a default (empty) ImageContext, layout drops every
        // <img>, so the dump silently showed a page with no images in it.
        let dom = html::parse(r#"<img src="pic.png" width="80" height="64" alt="a cat">"#);
        let dump = box_dump(&dom, Some("https://site.test/page"), 40);
        assert!(dump.contains("img"), "no image box in:\n{dump}");
        // 80px/8 = 10 cells wide, 64px/16 = 4 lines tall (PLAN.md's mapping).
        assert!(dump.contains("w=10 h=4"), "wrong image size in:\n{dump}");
    }

    #[test]
    fn dump_ends_with_exactly_one_newline() {
        let dump = box_dump(&html::parse("<p>hi</p>"), None, 40);
        assert!(dump.ends_with("h=1\n"), "{dump:?}");
        assert!(!dump.ends_with("\n\n"), "{dump:?}");
    }
}
