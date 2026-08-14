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
use crate::js::cookies::Jar;
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
    cookies: Jar,
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
            cookies: Jar::new(),
        };
        let (mut queue, _) =
            js::queue::ScriptQueue::new(js::sources::sources(&page.dom), &page.console);
        let ready = queue.take_ready_prefix();
        // Borrowed pieces rather than `&page`, so the context does not hold a
        // shared borrow across the mutable one `run_prefix` needs.
        let (console, storage, cookies) = (
            page.console.clone(),
            page.storage.clone(),
            page.cookies.clone(),
        );
        let ctx = PageContext {
            page: 1,
            url: "https://hostile.test/page",
            console: &console,
            storage: &storage,
            cookies: &cookies,
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
            cookies: &self.cookies,
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
            cookies: &self.cookies,
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

/// A page whose script appends a script that appends a script, driven through
/// `App` the way the event loop drives it (M11.5 deliverable 7).
///
/// This is the one hostile shape the rest of this module cannot express, and
/// the reason is the point: every case above is bounded *inside one tick*, by
/// M10.13's execution budget. A chain of insertions is bounded by nothing a
/// tick can see — each link asks the loop for a fresh turn, each turn finishes
/// well inside its budget, and the loop happily runs them forever. So the
/// bound has to be a page bound (`js::queue::MAX_INSERTED_SCRIPTS`), and what
/// it has to preserve is PLAN.md §1.5: `q` still quits.
///
/// `App` rather than `Page`: the property under test is about turns of the
/// event loop, and a harness that runs ticks back to back would prove the
/// opposite of what the loop does.
#[cfg(test)]
mod chain {
    use crate::browser::app::{App, Effect};
    use crate::html;
    use crate::js::SCRIPT_BUDGET;
    use crate::js::queue::MAX_INSERTED_SCRIPTS;
    use crate::msg::Msg;
    use crate::net::FetchId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    /// The page: a script whose text is a script that appends a copy of
    /// itself. Nothing here is a loop inside a tick — each generation is one
    /// legal insertion, which is exactly what makes it unbounded without a
    /// page bound.
    const ENDLESS_CHAIN: &str = "<p>still readable</p><script>\
         window.link = function () {\
           var s = document.createElement('script');\
           s.textContent = 'window.depth = (window.depth || 0) + 1; link();';\
           document.body.appendChild(s);\
         };\
         link();</script>";

    fn loaded(html: &str) -> (App, FetchId) {
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://hostile.test/".into());
        app.update(Msg::Loaded {
            id,
            url: "http://hostile.test/".into(),
            status: 200,
            body: html.as_bytes().to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
        });
        app.update(Msg::Parsed {
            id,
            dom: html::parse(html),
            elapsed: Duration::ZERO,
        });
        (app, id)
    }

    fn key(ch: char) -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    #[test]
    fn a_script_that_appends_a_script_forever_stops_and_q_still_quits() {
        let started = Instant::now();
        let (mut app, id) = loaded(ENDLESS_CHAIN);
        let mut effect = app.update(Msg::RunScripts { id });
        let mut turns = 0;

        while let Some(id) = effect.run_scripts {
            turns += 1;
            assert!(
                turns <= MAX_INSERTED_SCRIPTS + 1,
                "the chain asked for {turns} turns; the bound is {MAX_INSERTED_SCRIPTS}"
            );
            // **The loop reaches `recv` between every pair of turns.** Each
            // link is one message that returns, so a key waiting in the
            // channel is served before the next one starts — the chain is
            // never behind more than a single tick, which is the same worst
            // case M10.13 measured for one runaway script.
            let turn = Instant::now();
            effect = app.update(Msg::RunScripts { id });
            assert!(
                turn.elapsed() < 3 * SCRIPT_BUDGET,
                "one link of the chain took {:?}",
                turn.elapsed()
            );
        }

        // It stopped on its own, and it stopped where the bound says.
        assert_eq!(turns, MAX_INSERTED_SCRIPTS);
        assert!(
            app.update(Msg::RunScripts { id }).run_scripts.is_none(),
            "a page past its bound still asked for another turn"
        );
        assert!(app.update(key('q')).quit, "q did not quit");
        assert!(
            started.elapsed() < super::PATIENCE,
            "the chain took {:?}",
            started.elapsed()
        );
        eprintln!(
            "HOSTILE {:<44} {turns} turns",
            "a script appending a script"
        );
    }

    /// The same chain built out of `error` handlers instead of script bodies.
    /// A script whose `src` will not resolve is owed an `error`, its handler
    /// inserts the next one, and every link is a *dispatch* rather than a
    /// script run — so none of it goes through the queue's ready prefix.
    ///
    /// Each handler burns wall-clock **after** inserting, which is the shape
    /// that matters: the insertion is recorded before the budget interrupts
    /// the handler, so the chain keeps its length whatever the burn costs. If
    /// an owed `error` were fired at the point of discovery, the whole chain
    /// would collapse into the one `update` that started it — dispatch nested
    /// inside dispatch, thirty-two budgets deep, with the loop nowhere near
    /// `recv` and `q` waiting behind all of it.
    const ERROR_CHAIN: &str = "<p>still readable</p><script>\
         window.link = function () {\
           var s = document.createElement('script');\
           s.onerror = function () {\
             link();\
             var t = Date.now(); while (Date.now() - t < 20) {}\
           };\
           s.src = 'http://';\
           document.body.appendChild(s);\
         };\
         link();</script>";

    #[test]
    fn a_chain_of_error_handlers_costs_a_turn_each_and_q_still_quits() {
        let (mut app, id) = loaded(ERROR_CHAIN);
        let mut effect = app.update(Msg::RunScripts { id });
        let mut turns = 0;

        while let Some(id) = effect.run_scripts {
            turns += 1;
            assert!(
                turns <= MAX_INSERTED_SCRIPTS + 1,
                "the chain asked for {turns} turns; the bound is {MAX_INSERTED_SCRIPTS}"
            );
            let turn = Instant::now();
            effect = app.update(Msg::RunScripts { id });
            // One handler's worth of work, not the chain's. The burn is 20 ms
            // and the budget is the ceiling; a turn that fired every owed
            // `error` would be seconds rather than milliseconds here.
            assert!(
                turn.elapsed() < 3 * SCRIPT_BUDGET,
                "one link of the chain took {:?}",
                turn.elapsed()
            );
        }

        assert_eq!(turns, MAX_INSERTED_SCRIPTS);
        assert!(app.update(key('q')).quit, "q did not quit");
        eprintln!(
            "HOSTILE {:<44} {turns} turns",
            "a chain of onerror handlers"
        );
    }

    #[test]
    fn q_quits_from_inside_the_chain_before_it_has_finished() {
        // Not just at the end: a reader who wants out mid-chain gets out. The
        // key is served on the turn after whichever tick was running, because
        // the loop is at `recv` and the next `RunScripts` is behind it in the
        // channel.
        let (mut app, id) = loaded(ENDLESS_CHAIN);
        app.update(Msg::RunScripts { id });
        app.update(Msg::RunScripts { id });
        assert_eq!(
            app.update(key('q')),
            Effect {
                quit: true,
                ..Effect::default()
            }
        );
    }
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
