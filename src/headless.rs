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
    // A session of its own, empty and gone when this returns: a dump's cookies
    // begin empty and die with the call, so a golden cannot depend on what the
    // last dump wrote.
    let console = Console::new();
    let cookies = crate::js::cookies::Jar::new();
    let (runs, pending) = run_scripts_from(dom, url, false, &cookies, &console);
    (runs, console, pending)
}

/// The pass **with** external `<script src>` fetched, against the caller's
/// session. See below for why only `--dump-js` does this.
///
/// The jar is the caller's, and that is M11.7a's half of it: the headless
/// modes follow a redirect chain themselves, so the cookies a hop set are
/// already in the jar — and were already on the wire for the hop after it —
/// before any script asks `document.cookie`. `--dump-js` is what M11.25's
/// ladder sweep reads, so a page whose session was established by a 302 the
/// reader never saw has to behave here exactly as it does in the TUI.
pub fn run_scripts_fetching(
    dom: &mut Dom,
    url: &str,
    cookies: &crate::js::cookies::Jar,
    console: &Console,
) -> (Vec<ScriptRun>, usize) {
    run_scripts_from(dom, Some(url), true, cookies, console)
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
    cookies: &crate::js::cookies::Jar,
    console: &Console,
) -> (Vec<ScriptRun>, usize) {
    let mut host = None;
    // One page, one session: storage is created and dropped with this call.
    let storage = crate::js::storage::Storage::new();
    // One page, one host, both gone when this returns, so any page generation
    // will do — nothing here outlives the call to hold a stale handle.
    // The queue, headless: external scripts are never fetched here (no worker
    // and no network on this path), so their slots stay holes and everything
    // after one waits — the same rule the TUI follows, with the arrivals that
    // would unblock it simply never coming.
    let (mut queue, externals) = js::queue::ScriptQueue::new(js::sources::sources(dom), console);

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
                // The jar's answer, on the same terms the TUI's document-order
                // `<script src>` gets — including nothing at all when the
                // script comes from another origin.
                let request = script_request(cookies, base_url, url);
                net::spawn_script(FetchId(1), external.slot, request, tx.clone());
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
    // The `load`/`error` an inserted script's element is owed: once its body
    // has actually run, or once its URL turns out to be unusable (M11.5).
    let mut owed_events: Vec<(crate::dom::NodeId, js::EventDescriptor)> = Vec::new();
    loop {
        // One per round, taken before anything runs, which is exactly what
        // `App::run_ready_scripts` does with a turn — an event owed by *this*
        // round is fired by the next one, and a handler never nests inside the
        // dispatch that caused it.
        let owed = match owed_events.is_empty() {
            true => None,
            false => Some(owed_events.remove(0)),
        };
        // Scripts a previous tick inserted (M11.5) join the queue before this
        // round's prefix is taken, which is what makes them run in a *later*
        // turn — the same rule the TUI follows, with this loop standing in for
        // the event loop. The page bound in `js::queue` is what stops a script
        // that appends a script from making this loop endless.
        owed_events.extend(adopt_inserted_scripts(
            host.as_ref(),
            dom,
            &mut queue,
            console,
            cookies,
            base_url.filter(|_| fetch_externals),
            &tx,
            &mut in_flight,
        ));

        let ready = queue.take_ready_prefix();
        let finished = queue.take_finished();
        let mut ran = !ready.is_empty() || finished;
        if ran {
            runs.extend(js::run_prefix(
                &mut host,
                dom,
                &js::PageContext {
                    page: HEADLESS_PAGE,
                    url: base_url.unwrap_or_default(),
                    console,
                    storage: &storage,
                    cookies,
                },
                ready,
                finished,
            ));
        }

        // An inserted script's `load` or `error` (M11.5), fired **after** the
        // prefix that ran its body — `load` means "it ran". It is owed from
        // the previous round, because that is when the body arrived and this
        // is when it executed. `--dump-js` is what M11.25's ladder sweep
        // reads, so a page that chains on `onload` has to behave here the way
        // it behaves in the TUI.
        if let Some((node, event)) = owed {
            js::dispatch(
                &mut host,
                dom,
                &js::PageContext {
                    page: HEADLESS_PAGE,
                    url: base_url.unwrap_or_default(),
                    console,
                    storage: &storage,
                    cookies,
                },
                js::Target::Node(node.0),
                event,
            );
            // The handler may have inserted the next link in a chain, which
            // the top of the next round adopts.
            ran = true;
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
                    // The binding already asked the jar, because
                    // `credentials` is its option to read (M11.7).
                    ask.ask,
                    ask.method,
                    ask.headers,
                    ask.body,
                    tx.clone(),
                );
                in_flight += 1;
            }
        }

        // An owed event is work left to do, so it keeps the loop alive even
        // when this round ran nothing: the round that discovers an unusable
        // URL fires nothing itself.
        if in_flight == 0 && !ran && owed_events.is_empty() {
            break;
        }
        if settled >= MAX_HEADLESS_FETCHES && queue.is_finished() {
            break;
        }
        // A tick that ran but asked for nothing over the wire may still have
        // inserted a script; go round again rather than blocking on a `recv`
        // no worker will answer.
        if in_flight == 0 {
            continue;
        }
        // Block for the next answer. Every worker always replies, so this
        // cannot wait longer than the requests themselves take.
        match rx.recv() {
            Ok(Msg::Script { slot, source, .. }) => {
                let failed = source.is_none();
                if failed {
                    console.push(
                        js::console::Level::Warn,
                        None,
                        None,
                        "a <script src> could not be fetched",
                    );
                }
                // Only an *inserted* slot names an element; a document-order
                // `<script src>` has none, because nothing could have put a
                // listener on it before the page ran.
                owed_events.extend(queue.element(slot).map(|node| {
                    (
                        node,
                        if failed {
                            js::EventDescriptor::ERROR
                        } else {
                            js::EventDescriptor::LOAD
                        },
                    )
                }));
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
                            console,
                            storage: &storage,
                            cookies,
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
    (runs, pending)
}

/// The headless half of `App::adopt_inserted_scripts` (M11.5): the `<script>`
/// elements the last tick put into the document join the queue, and the
/// external ones go out to a worker exactly where the document's own do.
///
/// The decisions themselves are not duplicated — `connected_script` and
/// `ScriptQueue::insert` are the same two calls `App` makes, in the same
/// order. What differs is only where the worker is spawned from, which is what
/// differs between the two paths for every other subresource too.
#[allow(clippy::too_many_arguments)]
fn adopt_inserted_scripts(
    host: Option<&js::Host>,
    dom: &Dom,
    queue: &mut js::queue::ScriptQueue,
    console: &Console,
    cookies: &crate::js::cookies::Jar,
    base_url: Option<&str>,
    tx: &mpsc::Sender<Msg>,
    in_flight: &mut usize,
) -> Vec<(crate::dom::NodeId, js::EventDescriptor)> {
    let mut owed = Vec::new();
    for candidate in host.map(js::Host::take_script_inserts).unwrap_or_default() {
        let node = crate::dom::NodeId(candidate);
        let name = format!("inserted#{}", queue.inserted() + 1);
        let Some(script) = js::sources::connected_script(dom, node, &name) else {
            continue;
        };
        if let js::queue::Inserted::Fetch(external) = queue.insert(node, script, console) {
            match base_url.and_then(|base| net::resolve_url(base, &external.url)) {
                Some(url) => {
                    let request = script_request(cookies, base_url, url);
                    net::spawn_script(FetchId(1), external.slot, request, tx.clone());
                    *in_flight += 1;
                }
                None => {
                    console.push(
                        js::console::Level::Warn,
                        Some(external.url.clone()),
                        None,
                        "this inserted script's URL was not fetched",
                    );
                    queue.fill(external.slot, None);
                    // Only when we *tried*: a `base_url` here means this path
                    // fetches, so a URL that will not resolve against it is
                    // one that can never arrive, and its element is owed an
                    // `error` exactly as it would be in the TUI. When the
                    // caller passed no base, nothing was attempted — inventing
                    // an `error` would put a console line in the dump that the
                    // TUI never produces, which is the same lie in reverse.
                    if base_url.is_some() {
                        owed.push((node, js::EventDescriptor::ERROR));
                    }
                }
            }
        }
    }
    owed
}

/// The headless half of `App::request` (M11.7): a script URL plus the
/// `Cookie:` header the jar decided it may carry, from the same one function.
///
/// `base_url` is `None` on the paths that never fetch, and then there is
/// nothing to compare an origin against — which `header_for` already answers
/// with `None`, so the empty string is honest rather than a special case.
fn script_request(
    cookies: &crate::js::cookies::Jar,
    base_url: Option<&str>,
    url: String,
) -> net::Request {
    let cookie = crate::js::cookies::header_for(
        cookies,
        base_url.unwrap_or_default(),
        &url,
        crate::js::cookies::now(),
    );
    net::Request {
        url,
        cookie,
        method: net::Method::Get,
    }
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
    box_dump_with_viewport(dom, base_url, width, 1)
}

/// [`box_dump`] with an explicit page height for fixed/sticky layout goldens.
pub fn box_dump_with_viewport(
    dom: &mut Dom,
    base_url: Option<&str>,
    width: u16,
    viewport_height: u16,
) -> String {
    // Scripts first, and through the shared rule above: the boxes a golden
    // pins must be the boxes a reader would see, which means after the page's
    // own script has had its one pass at the tree.
    let _ = run_scripts(dom, base_url);
    let sheets = style::sources::inline_sheets(dom);
    let styles = style::style_tree(dom, &sheets.iter().collect::<Vec<_>>());
    let imgs = image::discover(dom, base_url);
    let img_ctx = ImageContext::from_discovery(&imgs, &mut ImageCache::default());
    let (tree, _revealed) = layout::layout_document_readable_with_viewport(
        dom,
        &styles,
        width,
        viewport_height,
        &img_ctx,
    );
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
    fn a_script_a_script_inserted_reaches_a_headless_dump() {
        // M11.5: the dumps have to show what the TUI shows, so the loop above
        // adopts insertions between rounds the way the event loop adopts them
        // between turns.
        let mut dom = html::parse(
            "<p>parsed</p><script>\
             var s = document.createElement('script');\
             s.textContent = \"document.body.appendChild(document.createElement('p'))\
                              .textContent = 'inserted';\";\
             document.body.appendChild(s);</script>",
        );
        let dump = box_dump(&mut dom, None, 40);
        assert!(dump.contains("\"inserted\""), "{dump}");
    }

    #[test]
    fn a_chain_of_insertions_terminates_a_headless_dump() {
        // The same bound the TUI has (`js::queue::MAX_INSERTED_SCRIPTS`), doing
        // the same job in a loop that has no reader to rescue it: without one,
        // a `--dump-text` of a page like this never returns.
        let mut dom = html::parse(
            "<p>parsed</p><script>\
             window.link = function () {\
               var s = document.createElement('script');\
               s.textContent = 'link();';\
               document.body.appendChild(s);\
             };\
             link();</script>",
        );
        let (_, console, _) = run_scripts(&mut dom, None);
        assert!(
            console
                .entries()
                .iter()
                .any(|e| e.text.contains("as many as this browser will run")),
            "the chain stopped for some reason other than the bound: {:?}",
            console.entries()
        );
    }

    #[test]
    fn an_inserted_script_whose_url_will_not_resolve_fires_error_in_a_dump() {
        // The TUI fires `error` at an element whose script can never arrive
        // (`App::adopt_inserted_scripts`), and `--dump-js` is what M11.25's
        // ladder sweep reads — so a page that chains on `onerror` must not
        // look like a page that hangs here. `http://` resolves against
        // nothing, so no worker is spawned and no test touches the network.
        let mut dom = html::parse(
            "<p>parsed</p><script>\
             var s = document.createElement('script');\
             s.onerror = function () { console.log('error fired'); };\
             s.src = 'http://';\
             document.body.appendChild(s);</script>",
        );
        let cookies = crate::js::cookies::Jar::new();
        let console = Console::new();
        run_scripts_fetching(&mut dom, "https://fixture.test/page", &cookies, &console);
        assert!(
            console
                .entries()
                .iter()
                .any(|e| e.text.contains("error fired")),
            "{:?}",
            console.entries()
        );
    }

    #[test]
    fn dump_ends_with_exactly_one_newline() {
        let dump = box_dump(&mut html::parse("<p>hi</p>"), None, 40);
        assert!(dump.ends_with("h=1\n"), "{dump:?}");
        assert!(!dump.ends_with("\n\n"), "{dump:?}");
    }
}
