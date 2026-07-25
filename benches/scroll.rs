//! The M3 fast gate (PLAN.md §6): a scroll step on the Wikipedia fixture must
//! come in under 5 ms. Run: `cargo bench --bench scroll`.
//!
//! What one `j` costs after M3.2, and nothing else: the fixture is fetched,
//! parsed and laid out once, *outside* the measured loop, because the scroll
//! path is forbidden to touch any of those (CLAUDE.md: scrolling never restyles
//! or relayouts). Inside the loop is the whole keypress→screen path — resolve
//! the key, move the offset, paint the visible slice into the frame, diff
//! against the previous frame and serialize the changed cells.
//!
//! The write goes to `io::sink()`. `Renderer::present` diffs and writes in one
//! call and the task forbids restructuring it for a bench, so what is measured
//! is draw + diff + serialize — everything but handing the bytes to the
//! terminal, which is a syscall no benchmark can honestly attribute.
//!
//! Last recorded (M3.3, Apple M4 Pro, 2026-07) — gate < 5 ms:
//!   120×40  28.1 µs · 200×50  44.8 µs
//!
//! Two sizes because the diff is per-cell: the column caps at 90 cells either
//! way, so a wider frame buys no text, only more blank gutter to compare.

use criterion::{Criterion, criterion_group, criterion_main};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::hint::black_box;
use std::io;
use std::time::Duration;

use yata::browser::app::App;
use yata::msg::Msg;
use yata::term::{self, Renderer};

fn key(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

/// A loaded, parsed, laid-out Wikipedia page at `w`×`h`, plus a renderer whose
/// previous frame already holds the first screen — so the very first measured
/// step diffs against a painted screen, like every one after it.
fn page(w: u16, h: u16) -> (App, Renderer) {
    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/en.wikipedia.org.html"
    ))
    .expect("committed fixture must exist");

    let mut app = App::new(w, h);
    let id = app.start_fetch("http://fixture/".into());
    app.update(Msg::Loaded {
        id,
        url: "http://fixture/".into(),
        status: 200,
        body: html.clone().into_bytes(),
        elapsed: Duration::ZERO,
    });
    // Parse and layout both happen here, before the timer starts.
    app.update(Msg::Parsed {
        id,
        dom: yata::html::parse(&html),
        elapsed: Duration::ZERO,
    });

    let caps = term::detect_caps(Some("truecolor"));
    let mut renderer = Renderer::new(w, h, caps);
    app.draw(renderer.frame());
    renderer.present(&mut io::sink()).expect("sink cannot fail");
    (app, renderer)
}

/// One step down. At the last page `j` stops moving and would measure an empty
/// diff, so the offset wraps to the top instead — `gg` is one more scroll step,
/// not a reload, and on this fixture it happens about once every 3600 steps.
fn step(app: &mut App, renderer: &mut Renderer) {
    if !app.update(key('j')).dirty {
        app.update(key('g'));
        app.update(key('g'));
    }
    app.draw(renderer.frame());
    renderer.present(&mut io::sink()).expect("sink cannot fail");
}

fn scroll_wikipedia(c: &mut Criterion) {
    for (w, h) in [(120, 40), (200, 50)] {
        let (mut app, mut renderer) = page(w, h);
        c.bench_function(&format!("scroll step en.wikipedia.org {w}x{h}"), |b| {
            b.iter(|| step(black_box(&mut app), black_box(&mut renderer)))
        });
    }
}

criterion_group!(benches, scroll_wikipedia);
criterion_main!(benches);
