mod browser;
mod dom;
mod html;
mod msg;
mod net;
mod term;

use std::io::{self, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{env, iter, panic, process, thread};

use crossterm::event::{self, Event};
use crossterm::terminal;

use browser::app::{App, Effect};
use msg::Msg;
use term::Renderer;

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let panic_requested = args.iter().any(|a| a == "--panic");
    let dump = args.iter().any(|a| a == "--dump");
    let timing = args.iter().any(|a| a == "--timing");
    // `yata <url>`: the first non-flag argument (`--panic` etc. are flags,
    // not URLs). In the TUI, no argument → no fetch, blank page.
    let url = args.into_iter().find(|a| !a.starts_with("--"));

    // Headless modes are decided and finished here — before the panic hook,
    // `Screen::new`, raw mode, or the input thread exist. `--dump`'s stdout
    // carries body bytes and nothing else, so piping to a file is byte-exact.
    // Exit codes are part of the spec: 0 success · 1 fetch failure · 2 usage.
    if dump || timing {
        if dump && timing {
            process::exit(usage());
        }
        let Some(url) = url else {
            process::exit(usage());
        };
        process::exit(if dump {
            run_dump(&url)
        } else {
            run_timing(&url)
        });
    }

    // Installed before the Screen exists so no panic window is uncovered.
    // Restore first, then report: the default hook's output must land on the
    // normal screen, not vanish with the alternate one.
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = term::restore();
        default_hook(info);
    }));

    let _screen = term::Screen::new()?;

    if panic_requested {
        panic!("deliberate panic via --panic; the terminal should be restored");
    }

    let (w, h) = terminal::size()?;
    let caps = term::detect_caps(env::var("COLORTERM").ok().as_deref());
    let mut renderer = Renderer::new(w, h, caps);
    let mut app = App::new(w, h);

    let (tx, rx) = mpsc::channel();
    if let Some(url) = url {
        // Scheme defaulting for the CLI argument goes through the same helper
        // the URL bar uses. The id makes any previous generation stale; each
        // worker owns its own Sender clone.
        let url = net::normalize_url(&url);
        let id = app.start_fetch(url.clone());
        net::spawn_fetch(id, url, tx.clone());
    }
    // The loop keeps `tx` alive so a URL-bar commit can spawn a fetch (below);
    // the input thread gets its own clone. Because the loop holds a sender,
    // `recv` never ends on its own — input-thread death instead sends
    // `Msg::InputClosed`, which resolves to quit through the normal
    // `update` → `Effect` path (still just `effect.quit`, no extra loop branch).
    spawn_input_thread(tx.clone());

    let mut out = io::stdout();
    render(&mut app, &mut renderer, &mut out)?;

    // Blocking recv is the only wait in the process: idle CPU must be 0%.
    while let Ok(first) = rx.recv() {
        let batch = iter::once(first).chain(iter::from_fn(|| rx.try_recv().ok()));
        let effect = apply_batch(&mut app, batch);
        if effect.quit {
            break;
        }
        // A committed navigation: `App` already started the generation, the
        // loop's only job is to spawn the worker with its own Sender clone.
        if let Some((id, url)) = effect.fetch {
            net::spawn_fetch(id, url, tx.clone());
        }
        if effect.dirty {
            render(&mut app, &mut renderer, &mut out)?;
        }
    }
    Ok(())
}

/// The one usage line. Returns the usage exit code for `main` to exit with.
fn usage() -> i32 {
    eprintln!("usage: yata [--dump | --timing] <url>");
    2
}

/// The headless fetch: the *production* path — `net::normalize_url`, then
/// `net::spawn_fetch`, then a blocking drain of the same channel the event
/// loop `recv`s on (no polling) — flattening the worker's one terminal
/// message into a result. Redirects, decompression, and the one-terminal-
/// message contract all get exercised for free.
fn headless_fetch(url: &str) -> Result<(String, u16, Vec<u8>, Duration), String> {
    let url = net::normalize_url(url);
    let (tx, rx) = mpsc::channel();
    net::spawn_fetch(net::FetchId(1), url, tx);
    loop {
        match rx.recv() {
            Ok(Msg::Loaded {
                url,
                status,
                body,
                elapsed,
                ..
            }) => return Ok((url, status, body, elapsed)),
            Ok(Msg::NetError { url, reason, .. }) => return Err(format!("{url}: {reason}")),
            // Progress messages: keep draining until the terminal one.
            Ok(_) => {}
            // The worker sends exactly one terminal message before dropping
            // its sender; a closed channel without one is a worker bug, but
            // it must surface as an error page/exit, never a hang or panic.
            Err(_) => return Err("fetch worker exited without a result".into()),
        }
    }
}

/// `--dump`: raw body bytes to stdout, verbatim — no lossy decode, no added
/// newline. Any HTTP status dumps its body (curl semantics: a 404 page is
/// still a page). Exit 0, or 1 with the reason on stderr.
fn run_dump(url: &str) -> i32 {
    match headless_fetch(url) {
        Ok((_, _, body, _)) => {
            let mut out = io::stdout();
            if out.write_all(&body).and_then(|()| out.flush()).is_err() {
                return 1;
            }
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// `--timing`: the same headless fetch, then one full first-frame render —
/// the same `draw` + `present` pair the event loop times, into a sink — and
/// the `Timings` table (exactly the `F4` overlay's rows) on stderr. Stdout
/// stays empty.
fn run_timing(url: &str) -> i32 {
    let (final_url, status, body, elapsed) = match headless_fetch(url) {
        Ok(ok) => ok,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    // The normal App + Renderer at the real terminal size when there is one
    // (best-effort — a pipe has none), else 80×24.
    let (w, h) = terminal::size().unwrap_or((80, 24));
    let caps = term::detect_caps(env::var("COLORTERM").ok().as_deref());
    let mut renderer = Renderer::new(w, h, caps);
    let mut app = App::new(w, h);
    let id = app.start_fetch(final_url.clone());
    app.update(Msg::Loaded {
        id,
        url: final_url,
        status,
        body,
        elapsed,
    });

    let started = Instant::now();
    app.draw(renderer.frame());
    // A sink write cannot fail; the Result exists for real terminals.
    let _ = renderer.present(&mut io::sink());
    app.record_frame(started.elapsed());

    for row in app.timings().rows() {
        eprintln!("{row}");
    }
    0
}

/// Input coalescing: apply every already-queued message, then decide **once**
/// whether to redraw, so a flood of events costs one render, not one each.
/// Quit short-circuits — nothing rendered or applied after it matters.
fn apply_batch(app: &mut App, msgs: impl Iterator<Item = Msg>) -> Effect {
    let mut effect = Effect::default();
    for msg in msgs {
        let e = app.update(msg);
        effect.dirty |= e.dirty;
        // Keep only the last fetch of the batch: an earlier commit is already a
        // stale generation, so spawning its worker would be pure waste.
        if e.fetch.is_some() {
            effect.fetch = e.fetch;
        }
        if e.quit {
            effect.quit = true;
            break;
        }
    }
    effect
}

fn render(app: &mut App, renderer: &mut Renderer, out: &mut impl Write) -> io::Result<()> {
    let started = Instant::now();
    // A coalesced batch of resizes syncs the renderer once, at the final size.
    let (w, h) = app.size();
    if (renderer.frame().width(), renderer.frame().height()) != (w, h) {
        renderer.resize(w, h);
    }
    app.draw(renderer.frame());
    renderer.present(out)?;
    // A plain setter, deliberately not a Msg: a message would dirty the app
    // and every frame would schedule the next. The statusline shows this on
    // whatever paint comes next.
    app.record_frame(started.elapsed());
    Ok(())
}

/// Detached producer: blocks in `event::read()`, forwards key and resize
/// events into the channel. Never joined — it sits in `read` at shutdown and
/// process exit reaps it.
fn spawn_input_thread(tx: mpsc::Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(Event::Key(key)) => Msg::Key(key),
                Ok(Event::Resize(w, h)) => Msg::Resize(w, h),
                Ok(_) => continue,
                // Input is gone for good. Signal the loop to quit (if it is
                // already gone the channel is closed, and the failed send is
                // fine), then stop.
                Err(_) => {
                    let _ = tx.send(Msg::InputClosed);
                    return;
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn batch_of_dead_keys_is_one_decision_with_no_redraw() {
        let mut app = App::new(80, 24);
        // 'z' is bound to nothing; a key that does nothing must not redraw, no
        // matter how many arrive in one batch.
        let effect = apply_batch(&mut app, (0..200).map(|_| key('z')));
        assert_eq!(effect, Effect::default());
    }

    #[test]
    fn batch_of_scroll_keys_coalesces_to_one_redraw() {
        let mut app = App::new(80, 6); // 5-row page area
        let id = app.start_fetch("http://x/".into());
        let body = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 200,
            body,
            elapsed: Duration::ZERO,
        });
        // 200 'j' now scroll for real: still one coalesced redraw decision, not
        // 200 renders. (Clamping to the last page is covered by viewport tests.)
        let effect = apply_batch(&mut app, (0..200).map(|_| key('j')));
        assert!(effect.dirty);
        assert!(!effect.quit);
        assert!(effect.fetch.is_none());
    }

    #[test]
    fn batch_of_dirtying_messages_coalesces_to_one_redraw() {
        let mut app = App::new(80, 24);
        // 200 resize wiggles: one redraw decision at the final state, not 200
        // renders.
        let msgs = (0..200).map(|i| Msg::Resize(80, 24 + (i % 2)));
        let effect = apply_batch(&mut app, msgs);
        assert!(effect.dirty && !effect.quit);
        assert_eq!(app.size(), (80, 25), "state reflects the last message");
    }

    #[test]
    fn quit_in_a_batch_reports_quit_and_stops_applying() {
        let mut app = App::new(80, 24);
        // The resize after 'q' must never be applied: quit short-circuits the
        // batch, so state still shows the pre-quit size.
        let msgs = vec![key('j'), key('q'), Msg::Resize(10, 10)];
        assert!(apply_batch(&mut app, msgs.into_iter()).quit);
        assert_eq!(app.size(), (80, 24), "message after quit was applied");
    }

    #[test]
    fn batch_ending_in_input_closed_reports_quit() {
        let mut app = App::new(80, 24);
        // Input-thread death rides the same coalescing path as any quit: no
        // special loop branch, just `effect.quit`.
        let msgs = vec![key('j'), Msg::InputClosed];
        assert!(apply_batch(&mut app, msgs.into_iter()).quit);
    }

    #[test]
    fn empty_batch_does_nothing() {
        let mut app = App::new(80, 24);
        assert_eq!(apply_batch(&mut app, iter::empty()), Effect::default());
    }

    #[test]
    fn batch_keeps_only_the_last_fetch_commit() {
        // Two URL-bar commits in one coalesced batch: the first is already a
        // stale generation by the time the loop sees the effect, so only the
        // second may be spawned (M1.5: `apply_batch` keeps the last fetch).
        let commit = |url: &str| {
            let mut msgs = vec![key('o')];
            msgs.extend(url.chars().map(key));
            msgs.push(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            msgs
        };
        let mut msgs = commit("a.com");
        msgs.extend(commit("b.com"));

        let mut app = App::new(80, 24);
        let effect = apply_batch(&mut app, msgs.into_iter());
        let (id, url) = effect.fetch.expect("a commit must surface a fetch");
        assert_eq!(url, "https://b.com", "an earlier commit leaked through");
        assert_eq!(id, net::FetchId(2), "the id must be the second generation");
    }
}
