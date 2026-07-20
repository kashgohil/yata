mod browser;
mod term;

use std::{env, io, panic};

use crossterm::event::{self, Event};

use browser::keys::{self, Action};

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

    // Direct blocking read; replaced by the Msg event loop in task M1.3.
    loop {
        if let Event::Key(key) = event::read()?
            && keys::action_for(&key) == Some(Action::Quit)
        {
            break;
        }
    }
    Ok(())
}
