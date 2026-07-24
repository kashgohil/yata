use std::time::Duration;

use crossterm::event::KeyEvent;

use crate::dom::Dom;
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
    /// (charset handling is M2's problem), and the whole request's duration
    /// (client build → last body byte), measured on the worker so the app
    /// stays pure of `Instant::now()`.
    Loaded {
        id: FetchId,
        url: String,
        status: u16,
        body: Vec<u8>,
        elapsed: Duration,
    },
    /// The parsed tree for a `Loaded` body, sent by the same worker right
    /// after it. Parsing happens off the UI thread (CLAUDE.md: the UI thread
    /// never blocks, not even on a slow parse); the duration is measured on
    /// the worker for the same reason `Loaded::elapsed` is.
    Parsed {
        id: FetchId,
        dom: Dom,
        elapsed: Duration,
    },
    /// Terminal failure — bad URL, DNS, connect, TLS, mid-body disconnect.
    NetError {
        id: FetchId,
        url: String,
        reason: String,
    },
}
