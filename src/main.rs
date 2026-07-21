mod browser;
mod msg;
mod term;

use std::io::{self, Write};
use std::sync::mpsc;
use std::{env, iter, panic, thread};

use crossterm::event::{self, Event};
use crossterm::terminal;

use browser::app::{App, Effect};
use msg::Msg;
use term::Renderer;

fn main() -> io::Result<()> {
    let panic_requested = env::args().any(|a| a == "--panic");

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
    spawn_input_thread(tx);

    let mut out = io::stdout();
    render(&app, &mut renderer, &mut out)?;

    // Blocking recv is the only wait in the process: idle CPU must be 0%.
    while let Ok(first) = rx.recv() {
        let batch = iter::once(first).chain(iter::from_fn(|| rx.try_recv().ok()));
        let effect = apply_batch(&mut app, batch);
        if effect.quit {
            break;
        }
        if effect.dirty {
            render(&app, &mut renderer, &mut out)?;
        }
    }
    Ok(())
}

/// Input coalescing: apply every already-queued message, then decide **once**
/// whether to redraw, so a flood of events costs one render, not one each.
/// Quit short-circuits — nothing rendered or applied after it matters.
fn apply_batch(app: &mut App, msgs: impl Iterator<Item = Msg>) -> Effect {
    let mut effect = Effect::default();
    for msg in msgs {
        let e = app.update(msg);
        effect.dirty |= e.dirty;
        if e.quit {
            effect.quit = true;
            break;
        }
    }
    effect
}

fn render(app: &App, renderer: &mut Renderer, out: &mut impl Write) -> io::Result<()> {
    // A coalesced batch of resizes syncs the renderer once, at the final size.
    let (w, h) = app.size();
    if (renderer.frame().width(), renderer.frame().height()) != (w, h) {
        renderer.resize(w, h);
    }
    app.draw(renderer.frame());
    renderer.present(out)
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
                // Input is gone for good; dropping the sender closes the
                // channel and the event loop exits via recv's Err.
                Err(_) => return,
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
    fn batch_of_scrollish_keys_is_one_decision_with_no_redraw() {
        let mut app = App::new(80, 24);
        // 'j' is unbound until M1.5 gives it scroll; a key that does nothing
        // must not redraw, no matter how many arrive in one batch.
        let effect = apply_batch(&mut app, (0..200).map(|_| key('j')));
        assert_eq!(
            effect,
            Effect {
                quit: false,
                dirty: false
            }
        );
    }

    #[test]
    fn batch_of_dirtying_messages_coalesces_to_one_redraw() {
        let mut app = App::new(80, 24);
        // 200 resize wiggles: one redraw decision at the final state, not 200
        // renders. This is the shape a scroll-key flood takes from M1.5 on.
        let msgs = (0..200).map(|i| Msg::Resize(80, 24 + (i % 2)));
        let effect = apply_batch(&mut app, msgs);
        assert_eq!(
            effect,
            Effect {
                quit: false,
                dirty: true
            }
        );
        assert_eq!(app.size(), (80, 25), "state reflects the last message");
    }

    #[test]
    fn quit_in_a_batch_reports_quit() {
        let mut app = App::new(80, 24);
        let msgs = vec![key('j'), key('q'), key('j')];
        assert!(apply_batch(&mut app, msgs.into_iter()).quit);
    }

    #[test]
    fn empty_batch_does_nothing() {
        let mut app = App::new(80, 24);
        assert_eq!(apply_batch(&mut app, iter::empty()), Effect::default());
    }
}
