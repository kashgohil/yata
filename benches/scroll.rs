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
//!   120×40  28.5 µs · 200×50  44.4 µs
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

    // Parsed before the body is handed over, so the fixture is read once and
    // moved rather than copied.
    let dom = yata::html::parse(&html);

    let mut app = App::new(w, h);
    let id = app.start_fetch("http://fixture/".into());
    app.update(Msg::Loaded {
        id,
        url: "http://fixture/".into(),
        status: 200,
        body: html.into_bytes(),
        elapsed: Duration::ZERO,
        content_type: None,
        set_cookie: Vec::new(),
    });
    // Layout happens here, before the timer starts.
    app.update(Msg::Parsed {
        id,
        dom,
        elapsed: Duration::ZERO,
    });

    let caps = term::detect_caps(Some("truecolor"));
    let mut renderer = Renderer::new(w, h, caps);
    app.draw(renderer.frame());
    renderer.present(&mut io::sink()).expect("sink cannot fail");
    (app, renderer)
}

/// Whether a scroll step goes down or up. At either end of the page the key
/// stops moving and would measure an empty diff, so the walk turns around
/// instead of jumping: every measured iteration is then exactly one line step,
/// with no whole-page jump averaged in among them.
struct Walk {
    down: bool,
}

fn step(app: &mut App, renderer: &mut Renderer, walk: &mut Walk) {
    if !app.update(key(if walk.down { 'j' } else { 'k' })).dirty {
        walk.down = !walk.down;
        app.update(key(if walk.down { 'j' } else { 'k' }));
    }
    app.draw(renderer.frame());
    renderer.present(&mut io::sink()).expect("sink cannot fail");
}

fn scroll_wikipedia(c: &mut Criterion) {
    for (w, h) in [(120, 40), (200, 50)] {
        let (mut app, mut renderer) = page(w, h);
        let mut walk = Walk { down: true };
        c.bench_function(&format!("scroll step en.wikipedia.org {w}x{h}"), |b| {
            b.iter(|| {
                step(
                    black_box(&mut app),
                    black_box(&mut renderer),
                    black_box(&mut walk),
                )
            })
        });
    }
}

criterion_group!(benches, scroll_wikipedia);
criterion_main!(benches);
