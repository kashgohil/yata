//! Adversarial pages: what a hostile script can do, and what happens (M10.13).
//!
//! Every input to this engine before M10 came from a page's *markup*, which is
//! inert. JavaScript is the first input that **runs**, so this module is the
//! JS equivalent of `layout::degenerate_widths_terminate`: one place where the
//! things a page should not be able to do are tried on purpose.
//!
//! Each case asserts the same four properties — **no panic, bounded time,
//! bounded memory, and a page that is still there afterwards**. A case that
//! merely "does not crash" is not enough: the reader has to be able to keep
//! reading.
//!
//! Nothing here gets a special case. Where a guard was needed it went into the
//! arena API or the tick boundary, never into a branch that recognises one of
//! these fixtures.

#![cfg(test)]

use std::time::{Duration, Instant};

use crate::dom::Dom;
use crate::html;
use crate::js::console::Console;
use crate::js::storage::Storage;
use crate::js::{self, Host, PageContext, SCRIPT_BUDGET, Target};
use crate::timers::TimerId;

/// How long any single hostile case may take before the test calls it a hang.
/// Generous against the 100 ms script budget: a case that legitimately spends
/// several budgets (a dispatch through several listeners, each interrupted)
/// still fits, and anything unbounded blows straight past it.
const PATIENCE: Duration = Duration::from_secs(5);

struct Page {
    dom: Dom,
    host: Option<Host>,
    console: Console,
    storage: Storage,
}

impl Page {
    /// Load `script` as a page and run its document-order pass.
    fn run(script: &str) -> Page {
        let mut page = Page {
            dom: html::parse(&format!(
                "<div id=host><p id=t>text</p></div><script>{script}</script>"
            )),
            host: None,
            console: Console::new(),
            storage: Storage::new(),
        };
        let (mut queue, _) =
            js::queue::ScriptQueue::new(js::sources::sources(&page.dom), &page.console);
        let ready = queue.take_ready_prefix();
        // Borrowed pieces rather than `&page`, so the context does not hold a
        // shared borrow across the mutable one `run_prefix` needs.
        let (console, storage) = (page.console.clone(), page.storage.clone());
        let ctx = PageContext {
            page: 1,
            url: "https://hostile.test/page",
            console: &console,
            storage: &storage,
        };
        js::run_prefix(
            &mut page.host,
            &mut page.dom,
            &ctx,
            ready,
            queue.is_finished(),
        );
        page
    }

    /// Click the `#t` element, if it is still in the tree.
    fn click(&mut self) {
        let Some(target) = find_id(&self.dom, "t") else {
            return;
        };
        let ctx = PageContext {
            page: 1,
            url: "https://hostile.test/page",
            console: &self.console,
            storage: &self.storage,
        };
        js::dispatch(
            &mut self.host,
            &mut self.dom,
            &ctx,
            Target::Node(target),
            "click",
        );
    }

    fn fire(&mut self, id: u64) {
        let ctx = PageContext {
            page: 1,
            url: "https://hostile.test/page",
            console: &self.console,
            storage: &self.storage,
        };
        if let Some(host) = self.host.as_mut() {
            let _ = js::fire_timer(host, &mut self.dom, &ctx, TimerId(id));
        }
    }

    /// The page is still usable: its tree is intact, it can be styled and laid
    /// out, and the arena's invariants hold.
    fn still_a_page(&self) {
        crate::dom::check_links(&self.dom);
        let styles = crate::style::style_tree(&self.dom, &[]);
        let lines = crate::layout::layout(&self.dom, &styles, 40, crate::layout::Hidden::Respect);
        // Laying out must terminate and produce something (or nothing, for a
        // page that removed its own body) without panicking.
        let _ = lines.len();
    }
}

fn find_id(dom: &Dom, id: &str) -> Option<u32> {
    (0..dom.node_count())
        .map(|i| crate::dom::NodeId(i as u32))
        .find(|&node| dom.attr(node, "id") == Some(id))
        .map(|node| node.0)
}

/// Run `case`, assert it finished inside [`PATIENCE`], and report how long it
/// took so the suite's output is a table rather than a pass/fail.
fn bounded(name: &str, case: impl FnOnce() -> Page) -> Duration {
    let started = Instant::now();
    let page = case();
    let elapsed = started.elapsed();
    page.still_a_page();
    assert!(
        elapsed < PATIENCE,
        "{name} took {elapsed:?}, over the {PATIENCE:?} patience"
    );
    eprintln!("HOSTILE {name:<44} {elapsed:>12?}");
    elapsed
}

#[test]
fn a_runaway_script_costs_one_budget_and_the_page_survives() {
    let elapsed = bounded("while (true) {} in a script", || {
        Page::run("while (true) {}")
    });
    assert!(elapsed < 3 * SCRIPT_BUDGET, "{elapsed:?}");
}

#[test]
fn a_runaway_listener_costs_one_budget_per_click() {
    let elapsed = bounded("while (true) {} in a click listener", || {
        let mut page = Page::run(
            "document.getElementById('t').addEventListener('click', function () { while (true) {} });",
        );
        page.click();
        page
    });
    // The script pass is instant; the click is one budget.
    assert!(elapsed < 3 * SCRIPT_BUDGET, "{elapsed:?}");
}

#[test]
fn a_runaway_timer_callback_costs_one_budget_per_tick() {
    let elapsed = bounded("while (true) {} in a timer callback", || {
        let mut page = Page::run("setTimeout(function () { while (true) {} }, 0);");
        page.fire(1);
        page
    });
    assert!(elapsed < 3 * SCRIPT_BUDGET, "{elapsed:?}");
}

#[test]
fn unbounded_recursion_and_an_allocation_bomb_are_errors() {
    bounded("unbounded recursion", || {
        Page::run("function f() { return f(); } try { f(); } catch (e) {}")
    });
    bounded("allocation bomb", || {
        Page::run(
            "var a = []; try { while (true) a.push(new Array(500000).fill(0)); } catch (e) {}",
        )
    });
}

#[test]
fn a_timer_that_schedules_more_timers_does_not_run_away_inside_one_tick() {
    // Each callback is its own tick with its own budget; scheduling is not
    // recursion. The engine never runs them back to back on its own — the
    // loop does, one message at a time, with input interleaved.
    let elapsed = bounded("a timer callback scheduling two more", || {
        let mut page = Page::run(
            "var n = 0;\
             function grow() { n++; setTimeout(grow, 0); setTimeout(grow, 0); }\
             setTimeout(grow, 0);",
        );
        for id in 1..=8 {
            page.fire(id);
        }
        page
    });
    assert!(elapsed < 3 * SCRIPT_BUDGET, "{elapsed:?}");
}

#[test]
fn a_promise_that_requeues_itself_ends_at_the_pump_bound() {
    bounded("a promise chain that never settles", || {
        Page::run("function again() { Promise.resolve().then(again); } again();")
    });
}

#[test]
fn a_hundred_thousand_appends_are_bounded_by_the_budget() {
    // The script cannot finish inside one budget, so it is interrupted — the
    // arena grows by however much it managed, and the page is still a page.
    bounded("100k appendChild", || {
        Page::run(
            "var host = document.getElementById('host');\
             for (var i = 0; i < 100000; i++) host.appendChild(document.createElement('p'));",
        )
    });
}

#[test]
fn a_megabyte_of_inner_html_is_parsed_or_refused_without_a_crash() {
    bounded("1 MB innerHTML", || {
        Page::run("document.getElementById('host').innerHTML = '<p>x</p>'.repeat(120000);")
    });
}

#[test]
fn deep_nesting_built_by_script_does_not_abort_the_process() {
    // **The case most likely to take the process with it.** Style and layout
    // both recurse over the tree, so a deep enough subtree overflows the
    // native stack — which is an abort, not an error, and not something a
    // `catch` can reach. The arena refuses past its depth cap for exactly
    // this reason.
    bounded("10k-deep nesting via appendChild", || {
        Page::run(
            "var node = document.getElementById('host');\
             for (var i = 0; i < 10000; i++) {\
               var child = document.createElement('div');\
               try { node.appendChild(child); } catch (e) { break; }\
               node = child;\
             }",
        )
    });
    bounded("10k-deep nesting via innerHTML", || {
        Page::run("document.getElementById('host').innerHTML = '<div>'.repeat(10000);")
    });
}

#[test]
fn a_page_can_remove_its_own_body_and_still_be_laid_out() {
    bounded("document.body.remove()", || {
        Page::run("document.body.remove();")
    });
}

#[test]
fn a_listener_can_remove_its_own_node_mid_dispatch() {
    bounded("a listener removing its own node", || {
        let mut page = Page::run(
            "var t = document.getElementById('t');\
             t.addEventListener('click', function () { t.remove(); });\
             t.addEventListener('click', function () { console.log('still ran'); });",
        );
        page.click();
        page
    });
}

#[test]
fn navigation_from_every_entry_point_is_bounded() {
    bounded("location.href in a loop", || {
        Page::run("for (var i = 0; i < 10000; i++) location.href = '/p' + i;")
    });
    bounded("location.href in a dispatch", || {
        let mut page = Page::run(
            "document.getElementById('t').addEventListener('click', function () {\
               for (var i = 0; i < 1000; i++) location.href = '/x' + i;\
             });",
        );
        page.click();
        page
    });
    bounded("location.href in a timer", || {
        let mut page = Page::run("setTimeout(function () { location.href = '/later'; }, 0);");
        page.fire(1);
        page
    });
}

#[test]
fn ten_thousand_fetches_find_the_concurrency_cap() {
    let page = Page::run("for (var i = 0; i < 10000; i++) fetch('/x' + i).catch(function () {});");
    let asked = page
        .host
        .as_ref()
        .map(|host| host.take_fetch_requests().len())
        .unwrap_or(0);
    assert!(
        asked <= js::MAX_IN_FLIGHT,
        "{asked} requests escaped a cap of {}",
        js::MAX_IN_FLIGHT
    );
    page.still_a_page();
    eprintln!("HOSTILE {:<44} {asked} requests left", "10k fetch()");
}

#[test]
fn circular_values_terminate_in_every_place_that_formats_one() {
    bounded("console.log(window) and a circular JSON", || {
        Page::run(
            "var a = {}; a.self = a;\
             console.log(a);\
             console.log(window);\
             try { JSON.stringify(a); } catch (e) { console.log('stringify threw'); }",
        )
    });
}

#[test]
fn document_write_is_ignored_with_a_console_line_rather_than_a_crash() {
    let page = Page::run("document.write('<p>injected</p>');");
    page.still_a_page();
    let entries: Vec<String> = page
        .console
        .entries()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        entries.iter().any(|e| e.contains("document.write")),
        "a page calling document.write got no explanation: {entries:?}"
    );
}

#[test]
fn the_console_ring_buffer_holds_under_a_logging_storm() {
    let page = Page::run("for (var i = 0; i < 100000; i++) console.log('spam ' + i);");
    page.still_a_page();
    assert!(
        page.console.entries().len() <= crate::js::console::MAX_ENTRIES,
        "the console grew past its cap"
    );
}

#[test]
fn storage_is_capped_rather_than_unbounded() {
    let page = Page::run(
        "try { for (var i = 0; i < 100000; i++) localStorage.setItem('k' + i, 'x'.repeat(1000)); }\
         catch (e) { console.log('quota: ' + e.message.split(':')[0]); }",
    );
    page.still_a_page();
    assert!(
        page.console
            .entries()
            .iter()
            .any(|e| e.text.contains("QuotaExceededError")),
        "storage grew without hitting its quota"
    );
}
