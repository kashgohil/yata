use std::time::Duration;

use crossterm::event::{KeyEvent, MouseEvent};

use crate::css::Stylesheet;
use crate::dom::Dom;
use crate::net::FetchId;

/// Everything the UI thread reacts to arrives as one of these over the single
/// mpsc channel. Producers (input thread, fetch workers) only send; the event
/// loop is the sole receiver.
#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Key(KeyEvent),
    /// SGR mouse (mode 1006). Click → hit-test; move → `:hover` (M6).
    Mouse(MouseEvent),
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
    /// One linked stylesheet, from its own worker (M4.3). `slot` is the
    /// sheet's position in the document's source list, so a sheet that arrives
    /// second but is written first still cascades first — arrival order must
    /// never decide a winner.
    ///
    /// `sheet: None` means the fetch failed or the response was not a success:
    /// a missing stylesheet is a *degraded page*, not an error page, so the
    /// slot resolves to nothing and the rest of the cascade proceeds. Parsing
    /// happens on the worker for the same reason the HTML parse does.
    Stylesheet {
        id: FetchId,
        slot: usize,
        sheet: Option<Stylesheet>,
    },
    /// Terminal failure — bad URL, DNS, connect, TLS, mid-body disconnect.
    NetError {
        id: FetchId,
        url: String,
        reason: String,
    },
}
