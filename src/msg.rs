use crossterm::event::KeyEvent;

use crate::net::FetchId;

/// Everything the UI thread reacts to arrives as one of these over the single
/// mpsc channel. Producers (input thread, fetch workers) only send; the event
/// loop is the sole receiver.
#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Key(KeyEvent),
    Resize(u16, u16),
    /// The input thread's terminal source is gone for good; the app must exit.
    InputClosed,
    /// Progress: the fetch worker has `bytes_so_far` of the body.
    Loading {
        id: FetchId,
        bytes_so_far: u64,
    },
    /// Terminal success: final URL after redirects, HTTP status, raw bytes
    /// (charset handling is M2's problem).
    Loaded {
        id: FetchId,
        url: String,
        status: u16,
        body: Vec<u8>,
    },
    /// Terminal failure — bad URL, DNS, connect, TLS, mid-body disconnect.
    NetError {
        id: FetchId,
        url: String,
        reason: String,
    },
}
