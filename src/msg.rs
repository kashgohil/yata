use crossterm::event::KeyEvent;

/// Everything the UI thread reacts to arrives as one of these over the single
/// mpsc channel. Producers (input thread now, fetch workers from M1.4) only
/// send; the event loop is the sole receiver.
#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Key(KeyEvent),
    Resize(u16, u16),
}
