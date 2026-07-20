use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

// Tracks whether the terminal is in our modified state. The panic hook must be
// able to restore without access to the `Screen` value, and restore must be
// idempotent (Drop and the hook can both fire), so the state lives here.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Guard for the terminal state: raw mode + alternate screen + hidden cursor.
/// Dropping it restores the user's terminal.
pub struct Screen;

impl Screen {
    pub fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // The terminal is modified from this point on; claim it before
        // anything else can fail so the guard's Drop (or the panic hook)
        // always has a restore path — even if setup errors out below.
        ACTIVE.store(true, Ordering::SeqCst);
        let screen = Screen;
        let mut out = io::stdout();
        crossterm::queue!(out, EnterAlternateScreen, cursor::Hide)?;
        out.flush()?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = restore();
    }
}

/// Restore the terminal. Idempotent; safe to call from the panic hook.
/// Returns whether anything was actually restored.
pub fn restore() -> io::Result<bool> {
    restore_to(&mut io::stdout())
}

fn restore_to(out: &mut impl Write) -> io::Result<bool> {
    if !ACTIVE.load(Ordering::SeqCst) {
        return Ok(false);
    }
    // Raw mode may already be off (or stdout may not be a tty under tests);
    // never let that stop the screen sequences from going out.
    let _ = terminal::disable_raw_mode();
    crossterm::queue!(out, LeaveAlternateScreen, cursor::Show)?;
    out.flush()?;
    // Cleared only after the sequences landed: a failed restore must stay
    // retryable (e.g. Drop retrying after a failed panic-hook attempt).
    ACTIVE.store(false, Ordering::SeqCst);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken pipe"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("broken pipe"))
        }
    }

    // One test covers the whole lifecycle: tests run in parallel and ACTIVE
    // is shared state, so the scenarios must run sequentially.
    #[test]
    fn restore_is_retryable_then_idempotent() {
        ACTIVE.store(true, Ordering::SeqCst);

        // A restore that fails mid-write must not consume the claim.
        assert!(restore_to(&mut FailingWriter).is_err());

        let mut retry = Vec::new();
        assert!(
            restore_to(&mut retry).unwrap(),
            "retry after a failed restore must still restore"
        );
        let s = String::from_utf8(retry).unwrap();
        assert!(s.contains("\x1b[?1049l"), "must leave alternate screen");
        assert!(s.contains("\x1b[?25h"), "must show the cursor");

        let mut second = Vec::new();
        assert!(!restore_to(&mut second).unwrap());
        assert!(second.is_empty(), "double restore must emit nothing");
    }
}
