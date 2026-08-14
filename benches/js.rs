//! What M10 actually added (M10.14 deliverable 5).
//!
//! The pipeline benches gained the script pass implicitly — it shows up inside
//! their totals. This one isolates the four costs that did not exist before:
//! starting an engine at all, running a page's scripts, querying the DOM from
//! JavaScript, and one click → mutate → invalidate round trip.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use yata::dom::NodeId;
use yata::js::console::Console;
use yata::js::cookies::Jar;
use yata::js::storage::Storage;
use yata::js::{self, PageContext, Target};
use yata::{html, layout, style};

const WIKIPEDIA: &str = include_str!("../tests/fixtures/en.wikipedia.org.html");

fn context<'a>(console: &'a Console, storage: &'a Storage, cookies: &'a Jar) -> PageContext<'a> {
    PageContext {
        page: 1,
        url: "https://bench.test/page",
        console,
        storage,
        cookies,
    }
}

fn run(page: &str) -> (yata::dom::Dom, Option<js::Host>, Console, Storage, Jar) {
    let mut dom = html::parse(page);
    let (console, storage, cookies) = (Console::new(), Storage::new(), Jar::new());
    let mut host = None;
    let (mut queue, _) = js::queue::ScriptQueue::new(js::sources::sources(&dom), &console);
    let ready = queue.take_ready_prefix();
    let finished = queue.is_finished();
    js::run_prefix(
        &mut host,
        &mut dom,
        &context(&console, &storage, &cookies),
        ready,
        finished,
    );
    (dom, host, console, storage, cookies)
}

fn js_benches(c: &mut Criterion) {
    // Starting an engine: what a page with any script at all pays once, and
    // what a page with none must never pay.
    c.bench_function("engine startup", |b| {
        let (console, storage, cookies) = (Console::new(), Storage::new(), Jar::new());
        b.iter(|| black_box(js::Host::new(&console, &storage, &cookies).expect("starts")));
    });

    // A document-order pass over a script-heavy page: the tick M10.2 added.
    let heavy = "<div id=out></div><script>\
         var out = document.getElementById('out');\
         for (var i = 0; i < 200; i++) {\
           var p = document.createElement('p');\
           p.className = 'row';\
           p.textContent = 'row ' + i;\
           out.appendChild(p);\
         }</script>";
    c.bench_function("script pass, 200 nodes built", |b| {
        b.iter(|| black_box(run(heavy).0.node_count()));
    });

    // `querySelectorAll` on the biggest page in the ladder: the selector
    // matcher, reached from JavaScript rather than from the cascade.
    let wikipedia = format!(
        "{}<script>window.found = document.querySelectorAll('a').length;</script>",
        WIKIPEDIA.replace("<script", "<script type=\"text/x-not-run\"")
    );
    c.bench_function("querySelectorAll('a') on Wikipedia", |b| {
        b.iter(|| black_box(run(&wikipedia).0.node_count()));
    });

    // One click → mutate → invalidate round trip: dispatch, the DOM edit a
    // listener makes, and the restyle and relayout that follow.
    let clickable = "<p id=t>press</p><div id=out></div><script>\
         document.getElementById('t').addEventListener('click', function () {\
           var out = document.getElementById('out');\
           for (var i = 0; i < 20; i++) out.appendChild(document.createElement('p'));\
         });</script>";
    c.bench_function("click, mutate, restyle and relayout", |b| {
        b.iter_batched(
            || run(clickable),
            |(mut dom, mut host, console, storage, cookies)| {
                let target = (0..dom.node_count())
                    .map(|i| NodeId(i as u32))
                    .find(|&n| dom.attr(n, "id") == Some("t"))
                    .expect("the fixture has a target");
                js::dispatch(
                    &mut host,
                    &mut dom,
                    &context(&console, &storage, &cookies),
                    Target::Node(target.0),
                    "click",
                );
                let styles = style::style_tree(&dom, &[]);
                black_box(layout::layout(&dom, &styles, 80, layout::Hidden::Respect).len())
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, js_benches);
criterion_main!(benches);
