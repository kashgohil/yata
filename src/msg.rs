use std::time::Duration;

use crossterm::event::{KeyEvent, MouseEvent};

use crate::css::Stylesheet;
use crate::dom::Dom;
use crate::image::DecodedImage;
use crate::net::{JsResponse, PageId};
use crate::timers::TimerId;

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
        id: PageId,
        bytes_so_far: u64,
    },
    /// Terminal success: final URL after redirects, HTTP status, raw bytes
    /// (charset handling is M2's problem), and the whole request's duration
    /// (client build → last body byte), measured on the worker so the app
    /// stays pure of `Instant::now()`.
    Loaded {
        id: PageId,
        url: String,
        status: u16,
        body: Vec<u8>,
        elapsed: Duration,
        /// Raw `Content-Type` header value, if any. The TUI uses this to refuse
        /// non-document responses (M7); `--dump` ignores it.
        content_type: Option<String>,
        /// The response's `Set-Cookie` lines, verbatim and unfolded (M11.7).
        /// `App` parses them on the UI thread — the jar is `!Send` and no
        /// worker can hold one.
        ///
        /// **Only the document carries these.** `Stylesheet`, `Script` and
        /// `Image` have no such field, and the asymmetry with the `Cookie:`
        /// header (which every request sends) is the decision: a subresource
        /// *sends* cookies because a server may require them to serve the
        /// bytes, but a session is established by the document path, and a
        /// subresource setting one is a quirk rather than a concept. With no
        /// field to carry it, "a stylesheet cannot start a session" is
        /// structural rather than a rule somebody has to remember.
        set_cookie: Vec<String>,
        /// Selected cache fields, with repeated field lines kept separate.
        metadata: crate::browser::http_cache::Metadata,
    },
    /// One hop of a redirect chain (M11.7a): the response said to go
    /// somewhere else, and that is all the worker did about it.
    ///
    /// **A hop is a thing that happened, so it is a message.** Every other
    /// decision in this browser is made by the event loop on data a worker sent
    /// it (PLAN.md §2); a worker that quietly made a second request the loop
    /// never asked for was the one place that was not true, and it is why a
    /// `Set-Cookie` on a 302 used to be lost and why the hop after it was sent
    /// the cookies of the URL the reader typed. `App` applies these lines,
    /// moves the page URL and asks the jar again — in that order, which is the
    /// whole point.
    ///
    /// `url` is the URL that produced this response (what the `Set-Cookie`
    /// lines are scoped to); `to` is where it points, already resolved against
    /// `url` through `net::resolve_url`. A 3xx with no usable `Location` is not
    /// a redirect at all and arrives as an ordinary `Loaded`, which the error
    /// page path already refuses.
    ///
    /// Only the **document** path produces these — see `net::fetch::client`.
    ///
    /// `status` is the 3xx that produced the hop (M11.11): 301/302/303 rewrite
    /// a POST to GET, 307/308 keep it. Without the status the loop cannot pick
    /// a row, and a login's 302 would POST the password at `/app`.
    Redirect {
        id: PageId,
        url: String,
        to: String,
        status: u16,
        elapsed: Duration,
        /// This hop's own `Set-Cookie` lines, for the same reason `Loaded`
        /// carries them: the 302 that hands out a session cookie and points at
        /// the landing page is what a login *is*.
        set_cookie: Vec<String>,
    },
    /// The parsed tree for a `Loaded` body, sent by the same worker right
    /// after it. Parsing happens off the UI thread (CLAUDE.md: the UI thread
    /// never blocks, not even on a slow parse); the duration is measured on
    /// the worker for the same reason `Loaded::elapsed` is.
    Parsed {
        id: PageId,
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
        id: PageId,
        slot: usize,
        sheet: Option<Stylesheet>,
    },
    /// Terminal failure — bad URL, DNS, connect, TLS, mid-body disconnect.
    NetError {
        id: PageId,
        url: String,
        reason: String,
    },
    /// One `<img>` decode finished (or failed). Soft failure: a broken image
    /// never becomes an error page. `id` is the page generation that requested
    /// the fetch so a late arrival after navigation is ignored.
    Image {
        id: PageId,
        url: String,
        result: Result<DecodedImage, String>,
    },
    /// One `<script src>`'s body, from its own worker (M10.10). `slot` is the
    /// script's position in the document-order queue, so a script that arrives
    /// second but is written first still runs first.
    ///
    /// `source: None` means the slot will never run — a failed fetch, a
    /// non-success status, or a body past `net::MAX_SCRIPT_BYTES`. Like a
    /// missing stylesheet that is a *degraded page*, not an error page: the
    /// slot settles empty and the rest of the queue proceeds.
    Script {
        id: PageId,
        slot: usize,
        source: Option<String>,
    },
    /// One `fetch()` finished (M10.12), from its own worker. `request` is the
    /// id the page's promise is waiting on; `page` is the generation that
    /// asked, so a response for a page the reader has left is dropped and its
    /// promise simply never settles — which is what a browser does when it
    /// tears down a document too.
    ///
    /// `Err` means the request never completed. A 404 is `Ok` with
    /// `ok: false`, because that is what `fetch` resolves to.
    JsFetch {
        page: PageId,
        request: u64,
        result: Result<JsResponse, String>,
    },
    /// A timer's deadline came up (M10.9), sent by the timer thread — one
    /// more producer on the one channel, which is the whole of what PLAN.md
    /// M10 predicted the M1 architecture would absorb. `page` is the
    /// generation that scheduled it, so a message for a page the user has left
    /// is dropped by the same guard every other message uses.
    Timer {
        page: PageId,
        id: TimerId,
    },
    /// Run this page's `<script>` elements in document order (M10.2).
    ///
    /// The loop sends this to itself after a `Msg::Parsed` turn has been
    /// rendered, so the page is on screen before any script runs and a script
    /// that spends its whole budget cannot delay first paint. Everything in
    /// this app is a message (PLAN.md §2); a self-scheduled pass is an
    /// instance of that rule, not an exception to it. `id` is the page
    /// generation, so a pass queued for a page the user has navigated away
    /// from is dropped by the same guard every other message uses.
    RunScripts {
        id: PageId,
    },
}

impl Msg {
    /// Stable page address for worker/timer messages. Terminal input is
    /// global and is therefore routed to the active tab instead.
    pub fn page(&self) -> Option<PageId> {
        match self {
            Msg::Loading { id, .. }
            | Msg::Loaded { id, .. }
            | Msg::Redirect { id, .. }
            | Msg::Parsed { id, .. }
            | Msg::Stylesheet { id, .. }
            | Msg::NetError { id, .. }
            | Msg::Image { id, .. }
            | Msg::Script { id, .. }
            | Msg::RunScripts { id } => Some(*id),
            Msg::JsFetch { page, .. } | Msg::Timer { page, .. } => Some(*page),
            Msg::Key(_) | Msg::Mouse(_) | Msg::Resize(_, _) | Msg::InputClosed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{PageId, TabId};

    #[test]
    fn every_page_originated_message_exposes_its_complete_address() {
        let page = PageId::new(TabId(7), 3);
        let messages = vec![
            Msg::Loading {
                id: page,
                bytes_so_far: 1,
            },
            Msg::Loaded {
                id: page,
                url: "https://x.test/".into(),
                status: 200,
                body: vec![],
                elapsed: Duration::ZERO,
                content_type: None,
                set_cookie: vec![],
                metadata: Default::default(),
            },
            Msg::Redirect {
                id: page,
                url: "https://x.test/".into(),
                to: "https://x.test/y".into(),
                status: 302,
                elapsed: Duration::ZERO,
                set_cookie: vec![],
            },
            Msg::Parsed {
                id: page,
                dom: Dom::new_document(),
                elapsed: Duration::ZERO,
            },
            Msg::Stylesheet {
                id: page,
                slot: 0,
                sheet: None,
            },
            Msg::NetError {
                id: page,
                url: "https://x.test/".into(),
                reason: "failed".into(),
            },
            Msg::Image {
                id: page,
                url: "https://x.test/i.png".into(),
                result: Err("failed".into()),
            },
            Msg::Script {
                id: page,
                slot: 0,
                source: None,
            },
            Msg::JsFetch {
                page,
                request: 1,
                result: Err("failed".into()),
            },
            Msg::Timer {
                page,
                id: TimerId(1),
            },
            Msg::RunScripts { id: page },
        ];
        assert!(messages.iter().all(|message| message.page() == Some(page)));
    }
}
