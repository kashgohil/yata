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
use std::sync::mpsc;

use crate::js::console::Console;
use crate::js::{self, ScriptRun};
use crate::layout;
use crate::msg::Msg;
use crate::net::{self, FetchId};
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
pub fn run_scripts(dom: &mut Dom, url: Option<&str>) -> (Vec<ScriptRun>, Console, usize) {
    run_scripts_from(dom, url, false)
}

/// The pass **with** external `<script src>` fetched. See below for why only
/// `--dump-js` does this.
pub fn run_scripts_fetching(dom: &mut Dom, url: &str) -> (Vec<ScriptRun>, Console, usize) {
    run_scripts_from(dom, Some(url), true)
}

/// The same pass, but **fetching** `<script src>` from `base_url`.
///
/// Only `--dump-js` uses this, and the split is deliberate. No other headless
/// path fetches a subresource — `<link>` stylesheets are not fetched either
/// (M4.3) — and the layout goldens run against a fake base (`fixture.test`),
/// so a fetching `box_dump` would have tests opening connections, which
/// CLAUDE.md forbids. `--dump-js` is the tool for inspecting what a page's
/// JavaScript actually does, is always pointed at a real URL, and is what
/// M10.14's ladder sweep reads; without fetching it would report that the
/// ladder runs almost no script, which is an artefact rather than a finding.
///
/// Still no timers, on either path.
fn run_scripts_from(
    dom: &mut Dom,
    base_url: Option<&str>,
    fetch_externals: bool,
) -> (Vec<ScriptRun>, Console, usize) {
    let mut host = None;
    let console = Console::new();
    // One page, one session: storage is created and dropped with this call.
    let storage = crate::js::storage::Storage::new();
    // One page, one host, both gone when this returns, so any page generation
    // will do — nothing here outlives the call to hold a stale handle.
    // The queue, headless: external scripts are never fetched here (no worker
    // and no network on this path), so their slots stay holes and everything
    // after one waits — the same rule the TUI follows, with the arrivals that
    // would unblock it simply never coming.
    let (mut queue, externals) = js::queue::ScriptQueue::new(js::sources::sources(dom), &console);

    // Fetch what we were given a base for; settle the rest as unfetchable so
    // the queue can drain instead of waiting forever on a hole.
    let (tx, rx) = mpsc::channel();
    let mut in_flight = 0;
    for external in externals {
        match base_url
            .filter(|_| fetch_externals)
            .and_then(|base| net::resolve_url(base, &external.url))
        {
            Some(url) => {
                net::spawn_script(FetchId(1), external.slot, url, tx.clone());
                in_flight += 1;
            }
            None => {
                console.push(
                    js::console::Level::Warn,
                    Some(external.url.clone()),
                    None,
                    if fetch_externals {
                        "could not resolve this script's URL"
                    } else {
                        "external scripts are not fetched on this headless path"
                    },
                );
                queue.fill(external.slot, None);
            }
        }
    }
    // `tx` stays alive: a tick may still ask for `fetch()`, and the loop below
    // needs a sender to hand those workers. `rx.recv()` therefore never ends on
    // its own — the loop leaves when nothing is in flight, which it counts.

    let mut runs = Vec::new();
    let mut settled = 0usize;
    let mut scripts_done = false;
    loop {
        if !scripts_done {
            let ready = queue.take_ready_prefix();
            let finished = queue.is_finished();
            if !ready.is_empty() || finished {
                runs.extend(js::run_prefix(
                    &mut host,
                    dom,
                    &js::PageContext {
                        page: HEADLESS_PAGE,
                        url: base_url.unwrap_or_default(),
                        console: &console,
                        storage: &storage,
                    },
                    ready,
                    finished,
                ));
            }
            scripts_done = finished;
        }

        // A tick may have asked for `fetch()`. On the fetching path we perform
        // them, so a page that renders from data actually renders; otherwise
        // they are simply never answered, and the promise never settles.
        if fetch_externals {
            for ask in host
                .as_ref()
                .map(js::Host::take_fetch_requests)
                .unwrap_or_default()
            {
                net::spawn_js_fetch(
                    FetchId(1),
                    ask.request,
                    ask.url,
                    ask.method,
                    ask.headers,
                    ask.body,
                    tx.clone(),
                );
                in_flight += 1;
            }
        }

        if in_flight == 0 || (scripts_done && settled >= MAX_HEADLESS_FETCHES) {
            break;
        }
        // Block for the next answer. Every worker always replies, so this
        // cannot wait longer than the requests themselves take.
        match rx.recv() {
            Ok(Msg::Script { slot, source, .. }) => {
                if source.is_none() {
                    console.push(
                        js::console::Level::Warn,
                        None,
                        None,
                        "a <script src> could not be fetched",
                    );
                }
                queue.fill(slot, source);
                in_flight -= 1;
            }
            Ok(Msg::JsFetch {
                request, result, ..
            }) => {
                in_flight -= 1;
                settled += 1;
                if let Some(engine) = host.as_mut() {
                    js::settle_fetch(
                        engine,
                        dom,
                        &js::PageContext {
                            page: HEADLESS_PAGE,
                            url: base_url.unwrap_or_default(),
                            console: &console,
                            storage: &storage,
                        },
                        request,
                        result,
                    );
                }
            }
            _ => break,
        }
    }
    // Timers are never *run* here — the rule above — but the count is
    // reported, so "the page scheduled work" is distinguishable from "the page
    // did nothing".
    let pending = host.as_mut().map_or(0, js::Host::pending_timers);
    (runs, console, pending)
}

/// How many `fetch()` calls one headless run will answer. A page that fetches
/// in a loop must not make a dump run forever; the count is generous enough
/// that an honest page rendering from data finishes.
const MAX_HEADLESS_FETCHES: usize = 64;

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
    let _ = run_scripts(dom, base_url);
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
