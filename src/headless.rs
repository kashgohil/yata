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
use crate::js::{self, ScriptRun};
use crate::layout;
use crate::style;

/// The document-order script pass, headless (M10.2).
///
/// **The rule, and it is deliberate: one pass, and no timers — ever.**
/// `--dump-text`, `--dump-boxes` and `--timing` run scripts because a golden
/// that describes a browser nobody uses is worse than no golden: what the
/// dumps show has to be what the TUI shows. But a headless dump has no clock
/// to run against and no user to wait for it, so `setTimeout` and friends
/// (M10.9) must never fire on this path. A dump whose output depended on how
/// long the dump took would be a golden that fails on a loaded machine.
///
/// The host is created and dropped inside this call: nothing headless outlives
/// one page.
pub fn run_scripts(dom: &mut Dom) -> Vec<ScriptRun> {
    let mut host = None;
    // One page, one host, both gone when this returns, so any page generation
    // will do — nothing here outlives the call to hold a stale handle.
    js::run_pass(&mut host, dom, HEADLESS_PAGE)
}

/// The page generation headless runs use. Only its constancy matters.
const HEADLESS_PAGE: u64 = 1;

/// Style → layout → `F3` box lines, as one newline-terminated block of text.
///
/// No network: `<link>` sheets are not fetched (the page is styled by the UA
/// sheet plus its own inline blocks) and no image bytes exist, so images lay
/// out the way the TUI shows them before the first byte arrives — sized from
/// `width`/`height` attrs when the page gives them, placeholder-sized when it
/// does not. Discovery still runs; without it layout drops `<img>` entirely.
pub fn box_dump(dom: &mut Dom, base_url: Option<&str>, width: u16) -> String {
    // Scripts first, and through the shared rule above: the boxes a golden
    // pins must be the boxes a reader would see, which means after the page's
    // own script has had its one pass at the tree.
    run_scripts(dom);
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
        let mut dom = html::parse(r#"<img src="pic.png" width="80" height="64" alt="a cat">"#);
        let dump = box_dump(&mut dom, Some("https://site.test/page"), 40);
        assert!(dump.contains("img"), "no image box in:\n{dump}");
        // 80px/8 = 10 cells wide, 64px/16 = 4 lines tall (PLAN.md's mapping).
        assert!(dump.contains("w=10 h=4"), "wrong image size in:\n{dump}");
    }

    #[test]
    fn a_dump_never_shows_work_a_page_deferred_to_a_timer() {
        // The headless rule: one pass, no timers. A page that schedules work
        // for later must dump exactly as it is *now*, so that the output does
        // not depend on how long the dump took to run.
        //
        // Today `setTimeout` is not bound at all (M10.9), so the script throws
        // and the callback could not run either way. The assertion is written
        // against the rule rather than against that accident: when M10.9 binds
        // timers, the queued callback must still never fire here, and this
        // comparison must still hold.
        let scheduled =
            "<p>now</p><script>setTimeout(function () { document.title = 'later'; }, 0);</script>";
        let plain = "<p>now</p>";

        let with_timer = box_dump(&mut html::parse(scheduled), None, 40);
        let without = box_dump(&mut html::parse(plain), None, 40);
        assert_eq!(
            with_timer, without,
            "a deferred callback reached a headless dump"
        );
        assert!(with_timer.contains("p"), "{with_timer}");
    }

    #[test]
    fn dump_ends_with_exactly_one_newline() {
        let dump = box_dump(&mut html::parse("<p>hi</p>"), None, 40);
        assert!(dump.ends_with("h=1\n"), "{dump:?}");
        assert!(!dump.ends_with("\n\n"), "{dump:?}");
    }
}
