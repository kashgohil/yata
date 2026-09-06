use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::browser::bookmarks::{
    AddResult, Bookmarks, MAX_BOOKMARKS, MAX_URL_BYTES, sanitize_title, valid_url,
};
use crate::browser::error_page;
use crate::browser::form;
use crate::browser::fragment;
use crate::browser::help;
use crate::browser::hints;
use crate::browser::history::History;
use crate::browser::http_cache::{Cache, Key as CacheKey, Plan, Representation, RequestMode};
use crate::browser::inspector;
use crate::browser::keys::{self, Action, Chord, Resolution};
use crate::browser::search::{self, Match as SearchMatch};
use crate::browser::statusline;
use crate::browser::timing::{self, Timings};
use crate::browser::viewport::Viewport;
use crate::css::Stylesheet;
use crate::dom::{AttrChanges, Dom, NodeData, NodeId};
use crate::image::{ImageCache, ImageSession, SharedImageCache};
use crate::js::cookies::Jar;
use crate::js::queue::ScriptQueue;
use crate::js::storage::Storage;
use crate::js::{self, console::Console};
use crate::layout::{self, BoxKind, LayoutTree};
use crate::msg::Msg;
use crate::net::{self, PageId, TabId};
use crate::paint::{self, DisplayList};
use crate::style::sources::{self, Source};
use crate::style::{self, StyleContext, Styles};
use crate::timers::{TimerId, TimerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::term::{Attrs, Cell, Frame, Style};

/// What one message asks of the event loop: exit, redraw, and/or start a fetch.
/// The loop ORs `dirty` across a batch, keeps the last `fetch`, and renders at
/// most once.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Effect {
    pub quit: bool,
    pub dirty: bool,
    /// A committed navigation: the id and the request (already-normalized URL
    /// plus whatever `Cookie:` header the jar decided on) for the loop to hand
    /// to `net::spawn_fetch`. `App` starts the fetch generation; the loop owns
    /// the worker thread. Keeps `App` pure of the network.
    pub fetch: Option<(PageId, net::Request)>,
    /// Stored response bytes to parse on a detached worker. Mutually exclusive
    /// with `fetch` for one navigation.
    pub cached: Option<(PageId, String, Representation, Duration)>,
    /// Additional document work from other tabs in the same coalesced batch.
    /// `fetch`/`cached` retain the newest work for compatibility with a single
    /// page turn; the loop dispatches every entry here as well.
    pub documents: Vec<DocumentWork>,
    /// Linked stylesheets to fetch, as (fetch id, document slot, request).
    /// The loop spawns one worker each, in the same turn — they must not queue
    /// behind one another (PLAN.md M4: fetched in parallel), and the page is
    /// already on screen while they run. Same discipline as `fetch`: `App`
    /// decides, the loop spawns.
    pub sheets: Vec<(PageId, usize, net::Request)>,
    /// Text to put on the system clipboard via OSC 52 (M6 yank). Written by
    /// the event loop, not through the cell buffer.
    pub yank: Option<String>,
    /// Image requests to make (page PageId, request). Parallel workers, same
    /// discipline as stylesheets (M8).
    pub images: Vec<(PageId, net::Request)>,
    /// External scripts to fetch, as (fetch id, document slot, request) — one
    /// worker each, exactly like `sheets`. The slots were allocated in
    /// document order before any fetch started, so arrival order cannot change
    /// execution order (M10.10).
    pub scripts: Vec<(PageId, usize, net::Request)>,
    /// `fetch()` calls this page's script asked for (M10.12), as (page, request
    /// id, method, url, headers, body) for the loop to spawn. Same discipline
    /// as everything else: `App` decides, the loop dispatches.
    pub fetches: Vec<(PageId, js::FetchAsk)>,
    /// Timer work this page's script asked for (M10.9), for the loop to hand
    /// to the timer thread. Same discipline as `fetch` and `sheets`: `App`
    /// decides, the loop dispatches — nothing in `App` or `src/js/` touches a
    /// thread.
    pub timers: Vec<TimerRequest>,
    /// This page wants its script pass (M10.2). The loop sends `Msg::RunScripts`
    /// back to itself, so the pass lands as its own turn *after* this one has
    /// been rendered. Same discipline as `fetch` and `sheets`: `App` decides,
    /// the loop dispatches — which is also what keeps the pass out of the turn
    /// that has to paint.
    pub run_scripts: Option<PageId>,
    /// Tab-set operation resolved by the page's table-driven key handling.
    /// The outer browser owns the collection and applies it after this update.
    pub tab: Option<TabAction>,
    /// An immutable bookmark snapshot for the event loop to submit through
    /// the worker's short-lock handle.
    pub bookmark_save: Option<(u64, Arc<[crate::browser::bookmarks::Bookmark]>)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DocumentWork {
    Fetch(PageId, net::Request),
    Cached(PageId, String, Representation, Duration),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabAction {
    New,
    Close,
    Next,
    Previous,
}

/// Link-hint session (`f` / `F`).
#[derive(Clone, Debug)]
struct HintSession {
    yank: bool,
    buffer: String,
    labels: Vec<(String, layout::LinkHit)>,
}

/// In-page search session after `/` + Enter (M7).
#[derive(Clone, Debug)]
struct SearchSession {
    query: String,
    matches: Vec<SearchMatch>,
    /// Index of the current match when `matches` is non-empty.
    current: usize,
}

/// Resize anchor: a layout fragment at the top of the viewport (UX §3.6).
#[derive(Clone, Copy, Debug)]
struct ScrollAnchor {
    node: NodeId,
    /// Index among this node's text boxes in walk order (mid-paragraph lines).
    text_index: usize,
    /// Document y of that fragment before the resize (fallback for rewrap).
    box_y: i32,
}

/// Where a page that has not been laid out yet should end up once it has.
///
/// Two shapes of one idea, in one slot (M11.4). A history restore knows the
/// offset the reader left; a fragment navigation knows only a name, because
/// the node it points at does not exist until the parse lands — so the
/// fragment is stored as *text* and resolved against the tree the first layout
/// of that generation produces.
///
/// One slot rather than two, because only one of them can win and the winner
/// has to be decided somewhere: `start_fetch` writes the fragment out of the
/// URL, and `navigate_restore` and `reload` overwrite it immediately after,
/// which is what makes `H` return the reader to their own position rather than
/// to whatever the URL's fragment happens to name.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingScroll {
    Offset(usize),
    Fragment(String),
}

/// How many hops one navigation may take before the reader is told the page is
/// not coming (M11.7a).
///
/// Twenty, which is what Chrome and Firefox stop at, so a chain that works in a
/// browser works here. It is a bound on the *chain*, not on time: every hop is
/// a real round trip through the event loop, so a redirect loop costs one
/// request per hop and no busy waiting at all — the loop is asleep in `recv`
/// between them exactly as it is between keystrokes.
/// Public because the headless modes follow the same chain with the same
/// bound: `--dump` of a redirecting URL has to reach the same page the TUI
/// reaches, and two numbers would be two browsers.
pub const MAX_REDIRECTS: u32 = 20;

#[derive(Clone)]
struct SharedCache(Rc<RefCell<Cache>>);

impl SharedCache {
    fn new() -> SharedCache {
        SharedCache(Rc::new(RefCell::new(Cache::default())))
    }

    fn plan(&self, key: &CacheKey, mode: RequestMode, now: Duration) -> Plan {
        self.0.borrow_mut().plan(key, mode, now)
    }

    fn insert(&self, key: CacheKey, response: Representation, now: Duration) -> bool {
        self.0.borrow_mut().insert(key, response, now)
    }

    fn revalidate(
        &self,
        key: &CacheKey,
        metadata: &crate::browser::http_cache::Metadata,
        now: Duration,
    ) -> Option<Representation> {
        self.0.borrow_mut().revalidate(key, metadata, now)
    }

    fn remove(&self, key: &CacheKey) {
        self.0.borrow_mut().remove(key);
    }
}

/// Resources whose lifetime is the browser process rather than a live page.
#[derive(Clone)]
struct BrowserSession {
    cookies: Jar,
    cache: SharedCache,
    cache_epoch: Instant,
    storage: Storage,
    images: SharedImageCache,
}

impl BrowserSession {
    fn new() -> BrowserSession {
        BrowserSession {
            cookies: Jar::new(),
            cache: SharedCache::new(),
            cache_epoch: Instant::now(),
            storage: Storage::new(),
            images: Rc::new(RefCell::new(ImageCache::default())),
        }
    }
}

/// The redirect chain this fetch generation has taken so far (M11.7a).
///
/// It lives on `App`, per generation, cleared by `start_fetch`, rather than
/// travelling in the message: a hop that carried its own count would let two
/// interleaved chains — a stale generation's and the current one's — each
/// believe they were short, and the count is a property of the *navigation*,
/// which is what a generation is.
#[derive(Clone, Copy, Default)]
struct Hops {
    count: u32,
    /// What the hops before the landing page cost, so `timings.fetch` can be
    /// the whole chain.
    elapsed: Duration,
}

/// Where the current fetch stands. `Loaded` retains the raw body for the
/// status-row byte count now, and for M2's parser to consume later; the
/// viewport re-wraps from its own sanitized lines, not from this.
enum Fetch {
    Idle,
    Loading {
        url: String,
        bytes_so_far: u64,
        /// The document request this generation last asked the loop to send
        /// (M11.11). A hop rewrites it in place so `POST → 307 → POST → 303 →
        /// GET` does not resurrect the body after the 303. `Loaded` does not
        /// remember a body: once the page has landed, reload is GET.
        method: net::Method,
    },
    Loaded {
        url: String,
        status: u16,
        body: Vec<u8>,
    },
    Failed {
        url: String,
        reason: String,
    },
}

/// Which surface owns the page area. The inspectors replace the page rather
/// than overlaying it, and only one at a time: `F1`/`F2`/`F3` are one selector,
/// not flags that can all be true. (`F4`'s timing overlay is different — it
/// draws *over* whatever is up, so it stays a separate flag.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Surface {
    Page,
    Dom,
    Styles,
    Boxes,
    /// The JS console (M10.7): what the page's script logged and threw.
    Console,
    /// Keybinding help (M7), scrollable list generated from `keys::BINDINGS`.
    Help,
}

/// Input mode. `Browse` reads the body and scrolls; `UrlInput` / `SearchInput`
/// are one-line prompts (cursor always at the end, no readline moves); `Field`
/// types into the focused form control (M11.9).
enum Mode {
    Browse,
    UrlInput {
        buffer: String,
    },
    SearchInput {
        buffer: String,
    },
    /// Typing into the control named by `focus`.
    ///
    /// **Focus and typing are different states**, and separating them costs
    /// nothing a reader notices. `Tab` past a page whose second focusable is a
    /// search box must not silently swallow `j`, `/` and `q`: a GUI browser can
    /// afford focus-implies-typing because it has a mouse and a chrome, and a
    /// terminal browser whose letters *are* its interface cannot. So `Enter`
    /// starts typing — it already means "activate the focused thing" — and
    /// `Esc` ends it with the field still focused.
    ///
    /// `caret` is a **character** index into the control's value, not a byte
    /// offset and not a cell: bytes would make `Backspace` land inside a
    /// multi-byte character, and cells cannot address the position between two
    /// halves of a wide glyph, which is where a caret must never be. Cells are
    /// derived from it at paint time (`layout::field::painted`).
    ///
    /// The caret lives here rather than on the node because it is UI, like
    /// focus and hover: a caret in the tree would make layout depend on where a
    /// cursor is. It is dropped the moment typing ends, which is the only time
    /// it means anything.
    Field {
        caret: usize,
    },
    /// Choosing from the focused `<select>`; the cursor is UI state, while
    /// selectedness remains sparse state on the DOM (M11.12).
    Select {
        select: NodeId,
        cursor: NodeId,
    },
}

/// The UI state. Pure with respect to the terminal: `update` touches only
/// state, `draw` touches only the given frame.
pub struct App {
    size: (u16, u16),
    /// Stable owner of every generation this page creates. Headless/test
    /// pages use tab zero; live tabs are constructed with a non-zero id.
    tab: TabId,
    /// Generation counter behind `PageId`s; `start_fetch` pre-increments,
    /// so ids start at 1 and id 0 is never live.
    fetch_gen: u64,
    /// The only fetch whose messages matter; anything else is stale.
    current_fetch: Option<PageId>,
    fetch: Fetch,
    mode: Mode,
    /// The first chord of a two-key sequence, waiting for the second. No timer
    /// backs it (idle CPU 0%): it waits indefinitely until the next key.
    pending: Option<Chord>,
    viewport: Viewport,
    /// Spinner frame index. Progress messages are its clock — there is no
    /// timer in this app — so it animates exactly while bytes are flowing.
    spinner: usize,
    /// Per-stage durations of the last completed pipeline run. Fetch and parse
    /// arrive as message data (`Loaded::elapsed`, `Parsed::elapsed`) because
    /// they run on a worker, and the frame time is set by the event loop after
    /// it presents; only layout is timed here, by `relayout`, because that is
    /// the one stage `App` runs itself.
    timings: Timings,
    /// Whether the `F4` timing overlay is drawn. Independent of the mode: it
    /// stays up while the URL bar is open.
    timing_visible: bool,
    /// A navigation a click handler asked for (M10.11), waiting to be folded
    /// into the `Effect` the key or mouse path returns. `dispatch_click` runs
    /// inside those paths and cannot return an `Effect` of its own.
    /// `fetch()` calls a click handler made, carried out the same way.
    pending_click_fetches: Vec<(PageId, js::FetchAsk)>,
    /// Timer work requested by a listener in the reader action currently
    /// resolving. It follows listener fetches out in the returned effect.
    pending_click_timers: Vec<TimerRequest>,
    /// External scripts a click handler inserted (M11.5), and whether one of
    /// its insertions can run now — carried out the same way, because a
    /// bootstrap behind a click is the same shape as one at load.
    pending_click_scripts: Vec<(PageId, usize, net::Request)>,
    pending_click_run: Option<PageId>,
    /// Inserted `<script>` elements owed an `error` because their URL will
    /// never resolve (M11.5), fired one per turn by `run_ready_scripts`.
    ///
    /// A list rather than a dispatch at the point of discovery, and the reason
    /// is the same one that makes an inserted script run in a later turn: a
    /// handler is a script, a script costs a budget, and a turn that fired
    /// every owed `error` would cost as many budgets as the page cared to owe
    /// itself — while an `onerror` that inserts the next unresolvable script
    /// would nest one dispatch inside the last, without the loop reaching
    /// `recv` between them.
    owed_script_errors: Vec<NodeId>,
    /// The page's scripts in document order, with the position execution has
    /// reached (M10.10). External ones are holes until their worker reports;
    /// nothing after a hole runs, because the script that has not arrived may
    /// define what the next one calls.
    script_queue: ScriptQueue,
    /// The generation `script_queue` was built for, so it is built **once** per
    /// page. Rebuilding would re-run every script the page has, and the only
    /// thing preventing a second `Msg::RunScripts` today is that one call site
    /// produces it — a guard that vanishes the moment anything else wants a
    /// script pass.
    script_queue_page: Option<PageId>,
    /// Every origin's `localStorage`/`sessionStorage` for this session
    /// (M10.11). Lives here rather than in the host because a host is dropped
    /// on every navigation and two pages on one origin must see the same data.
    /// In memory only — see `js::storage` for why that is a decision.
    storage: Storage,
    /// The session's cookies (M11.6), here for the same reason `storage` is:
    /// one jar for every page, outliving the host that reads it. In memory
    /// only and host-only; read by `document.cookie` and, since M11.7, by
    /// every request this app makes — see `App::request` and `js::cookies`.
    cookies: Jar,
    /// Session document cache. Page teardown deliberately never clears it.
    cache: SharedCache,
    cache_epoch: Instant,
    cache_key: Option<CacheKey>,
    cache_candidate: Option<CacheKey>,
    document_source: net::DocumentSource,
    /// Everything this page's JavaScript had to say (M10.7): console calls,
    /// uncaught exceptions, scripts skipped for their type — one ordered list,
    /// shown by `F5`. Page-local like the host: cleared on navigation, because
    /// the previous page's complaints are not this page's.
    console: Console,
    /// The `F5` console pane's lines, built the moment it is about to be seen
    /// and scrolled like every other inspector.
    console_view: Viewport,
    console_view_built: bool,
    /// This page generation's JavaScript host, created by its first script
    /// pass and dropped by the next navigation (`start_fetch`). One host per
    /// page generation is the rule `src/js` documents: a page's globals,
    /// closures and — from M10.8 — its listeners and timers cannot outlive it,
    /// because there is nowhere else for them to live. A page with no script
    /// never starts an engine, so `None` is the common case.
    js_host: Option<js::Host>,
    /// The current page's parsed tree, from the fetch worker's `Msg::Parsed`.
    /// `None` between an accepted `Loaded` and its `Parsed` — the old tree
    /// stops matching the shown body the moment a new body lands. Kept whole
    /// (not just as lines) because style/selector work (M4) reads it next.
    dom: Option<Dom>,
    /// The `F1` inspector's scrollable tree text — `dom` rendered to lines,
    /// then scrolled exactly like the page (cached lines → repaint; scrolling
    /// never re-parses or re-renders the tree).
    dom_view: Viewport,
    /// Whether `dom_view` currently holds `dom`'s lines. Rendering a
    /// Wikipedia-sized tree to lines costs ~15 ms, so it is deferred to the
    /// moment the tree is about to be seen (F1 toggled on, or a parse landing
    /// while F1 is open) instead of hitching the event loop on every
    /// navigation — pages load far more often than F1 opens. The flag also
    /// keeps an off/on toggle from rebuilding (and losing the scroll offset).
    dom_view_built: bool,
    /// The `F2` surface's lines: every element with its computed values, built
    /// and scrolled exactly like `dom_view`.
    styles_view: Viewport,
    styles_view_built: bool,
    /// The `F3` surface: layout boxes with x,y,w,h.
    boxes_view: Viewport,
    boxes_view_built: bool,
    /// Which of the page, the DOM tree, styles or boxes is on screen.
    surface: Surface,
    /// The page's author stylesheets, one slot per source in **document
    /// order**: `<style>` blocks filled the moment the tree lands, linked ones
    /// `None` until their worker reports. Indexing by document position is
    /// what stops a sheet that arrives first from cascading first.
    sheets: Vec<Option<Stylesheet>>,
    /// The last layout had to ignore the page's own `display:none` to show
    /// anything at all (see `layout::layout_readable`). Surfaced in the
    /// statusline, because a reader is entitled to know they are being shown
    /// something the page tried to hide.
    revealed: bool,
    /// The styled tree for `dom` and the sheets that have arrived so far.
    /// Recomputed as each one lands — the page renders with what it has and
    /// restyles, rather than blocking on a round trip (UX §3.2).
    styles: Option<Styles>,
    /// Painted form of the last layout tree. Scrolling re-emits this at a new
    /// offset without relayout (PLAN.md M5 display list).
    display_list: DisplayList,
    /// Last layout tree — F3 reads it; rebuilt only on relayout.
    layout_tree: Option<LayoutTree>,
    /// Dimensions represented by `layout_tree`; inactive tabs can accept a
    /// resize without rebuilding until they become visible.
    laid_out_size: Option<(u16, u16)>,
    /// Session history (back/forward) with scroll positions (M6).
    history: History,
    /// Absolute URLs successfully loaded this session — feeds `:visited`.
    visited: HashSet<String>,
    /// Element currently under the pointer (`:hover`), if any.
    hover: Option<NodeId>,
    /// Keyboard-focused link (`Tab` cycle), if any.
    focus: Option<NodeId>,
    /// Value at the start of the current reader edit session. It is UI state:
    /// markup and script writes do not create a reader `change` event.
    field_edit_baseline: Option<(NodeId, String)>,
    /// Active link-hint overlay (`f` / `F`).
    hint: Option<HintSession>,
    /// Brief statusline message (e.g. "yanked"), cleared by the next non-yank
    /// action (scroll, key binding, navigation, …).
    status_msg: Option<String>,
    /// The redirect chain this generation has taken (M11.7a).
    hops: Hops,
    /// Where to scroll after the first layout of a *specific* fetch generation
    /// (history back/forward, reload, a fragment on a cross-document link).
    /// Tied to `PageId` so a resize while the old page is still on screen
    /// cannot consume the restore.
    pending_scroll: Option<(PageId, PendingScroll)>,
    /// Active in-page search (`/` … Enter), if any.
    search: Option<SearchSession>,
    /// Scrollable help overlay content (`?`).
    help_view: Viewport,
    help_view_built: bool,
    /// How many times `relayout` has run. Test-only instrumentation for the
    /// invariant that costs the most if it ever breaks: scrolling relayouts
    /// zero times, a resize exactly once. Hover must not increment it (M6).
    #[cfg(test)]
    layouts: usize,
    /// How many times `restyle` has run. Protects the other half of the same
    /// invariant: scrolling restyles zero times, and a tick whose script only
    /// read the DOM must not restyle either — an event handler that looks at
    /// the page and decides to do nothing should cost nothing (M10.6).
    #[cfg(test)]
    styles_run: usize,
    /// How many *nodes* those restyles resolved (M11.3). `styles_run` counts
    /// passes and so cannot tell a subtree from a document — this is the only
    /// counter that says the scoped path really was scoped, and the difference
    /// between the two on Wikipedia is 25,599 nodes against a handful.
    #[cfg(test)]
    nodes_styled: usize,
    /// How many times the display list has been rebuilt. The cheapest of the
    /// three stages and the one every path ends with, so it is what says
    /// whether a "did nothing" tick really did nothing.
    #[cfg(test)]
    paints: usize,
    /// Turns M11.3's narrowing off, so the measurement can time the old path
    /// and the new one **in the same process, on the same page, interleaved**.
    /// This machine drifts several percent between runs, so a
    /// before-commit/after-commit pair is not evidence. Nothing but
    /// `measure_the_invalidation_*` sets it, and `restyle_scoped` is the only
    /// reader.
    #[cfg(test)]
    full_restyle_only: bool,
    /// Turns M11.5's inserted-script detection off, the same way and for the
    /// same reason: the measurement has to time the old path and the new one
    /// in the same process, interleaved. It reaches all the way into the
    /// bindings — `Host::disarm_script_inserts` — so the A side pays neither
    /// the drain here nor the tag comparison at each insert. Nothing but
    /// `measure_a_tick_that_inserts_no_script` sets it.
    #[cfg(test)]
    no_insert_detection: bool,
    /// Turns M11.9's narrowed keystroke path *on*, the same way M11.3's switch
    /// turns its narrowing off and for the same reason: the two paths have to
    /// be timed in one process, interleaved. The narrow one is the road not
    /// taken — the simple path came in inside the budget — so this is off
    /// everywhere except `measure_a_keystroke_in_a_field` and the test that
    /// keeps the B side honest.
    #[cfg(test)]
    narrow_keystroke: bool,
    /// Images: LRU cache, page discovery, Kitty placement state (M8).
    images: ImageSession,
}

impl App {
    pub fn new(w: u16, h: u16) -> Self {
        Self::with_caps(w, h, false)
    }

    /// Like [`new`] with an explicit Kitty graphics flag (from `term::Caps`).
    pub fn with_caps(w: u16, h: u16, kitty_graphics: bool) -> Self {
        Self::with_tab(w, h, kitty_graphics, TabId(0))
    }

    pub(crate) fn with_tab(w: u16, h: u16, kitty_graphics: bool, tab: TabId) -> Self {
        Self::with_session(w, h, kitty_graphics, tab, &BrowserSession::new())
    }

    fn with_session(
        w: u16,
        h: u16,
        kitty_graphics: bool,
        tab: TabId,
        session: &BrowserSession,
    ) -> Self {
        App {
            size: (w, h),
            tab,
            fetch_gen: 0,
            current_fetch: None,
            fetch: Fetch::Idle,
            mode: Mode::Browse,
            pending: None,
            viewport: Viewport::default(),
            spinner: 0,
            timings: Timings::default(),
            timing_visible: false,
            pending_click_fetches: Vec::new(),
            pending_click_timers: Vec::new(),
            pending_click_scripts: Vec::new(),
            pending_click_run: None,
            owed_script_errors: Vec::new(),
            script_queue: ScriptQueue::default(),
            script_queue_page: None,
            storage: session.storage.fork_tab(),
            cookies: session.cookies.clone(),
            cache: session.cache.clone(),
            cache_epoch: session.cache_epoch,
            cache_key: None,
            cache_candidate: None,
            document_source: net::DocumentSource::Network,
            console: Console::new(),
            console_view: Viewport::default(),
            console_view_built: false,
            js_host: None,
            dom: None,
            dom_view: Viewport::default(),
            dom_view_built: false,
            styles_view: Viewport::default(),
            styles_view_built: false,
            boxes_view: Viewport::default(),
            boxes_view_built: false,
            surface: Surface::Page,
            sheets: Vec::new(),
            styles: None,
            revealed: false,
            display_list: DisplayList::default(),
            layout_tree: None,
            laid_out_size: None,
            history: History::default(),
            visited: HashSet::new(),
            hover: None,
            focus: None,
            field_edit_baseline: None,
            hint: None,
            status_msg: None,
            hops: Hops::default(),
            pending_scroll: None,
            search: None,
            help_view: Viewport::default(),
            help_view_built: false,
            #[cfg(test)]
            layouts: 0,
            #[cfg(test)]
            styles_run: 0,
            #[cfg(test)]
            nodes_styled: 0,
            #[cfg(test)]
            paints: 0,
            #[cfg(test)]
            full_restyle_only: false,
            #[cfg(test)]
            no_insert_detection: false,
            #[cfg(test)]
            narrow_keystroke: false,
            images: ImageSession::with_cache(kitty_graphics, session.images.clone()),
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    fn defer_resize(&mut self, w: u16, h: u16) -> bool {
        self.size = (w, h);
        self.viewport.resize(w, self.page());
        self.dom_view.resize(w, self.page());
        self.styles_view.resize(w, self.page());
        self.boxes_view.resize(w, self.page());
        self.help_view.resize(w, self.page());
        self.layout_tree.is_some() && self.laid_out_size != Some((w, h))
    }

    /// Visible body height: the frame minus the one-row bottom bar.
    fn page(&self) -> u16 {
        self.size.1.saturating_sub(1)
    }

    /// Begin a new fetch generation for `url`: prior fetches become stale and
    /// their messages will be ignored. The caller passes the returned id to
    /// `net::spawn_fetch` — `App` itself never touches the network.
    pub fn start_fetch(&mut self, url: String) -> PageId {
        self.fetch_gen += 1;
        let id = PageId::new(self.tab, self.fetch_gen);
        self.current_fetch = Some(id);
        // One host per page generation: the old page's globals, closures and
        // (from M10.8) listeners go with it. Dropping the host is the whole
        // mechanism — there is nowhere for that state to survive.
        self.js_host = None;
        // The console is page-local for the same reason: the last page's
        // errors on this page's pane would be a lie about this page.
        self.console.clear();
        self.console_view_built = false;
        self.script_queue = ScriptQueue::default();
        self.script_queue_page = None;
        // An `error` owed to the last page's element is owed to nothing now:
        // the host that would have run its handler is gone, and the node id
        // names a tree this generation does not have.
        self.owed_script_errors.clear();
        // A fragment on a cross-document navigation (`href="/other#x"`, and a
        // URL typed or passed on the command line): the node it names does not
        // exist yet, so the *text* is held against this generation and turned
        // into an offset by the first layout the generation produces. This is
        // also the one write of the slot that is unconditional — every caller
        // that wants an offset instead writes it after this returns.
        self.pending_scroll =
            fragment_of(&url).map(|fragment| (id, PendingScroll::Fragment(fragment.to_string())));
        // A chain belongs to the navigation that started it (M11.7a): a page
        // reached through three hops must not count them against the next one.
        self.hops = Hops::default();
        self.cache_key = None;
        self.cache_candidate = None;
        self.document_source = net::DocumentSource::Network;
        self.fetch = Fetch::Loading {
            url,
            bytes_so_far: 0,
            method: net::Method::Get,
        };
        self.spinner = 0;
        id
    }

    /// Record the duration of the frame just presented. A plain setter, not a
    /// `Msg` and not dirty: feeding it back through the channel would make
    /// every frame schedule the next and the loop would never idle. The
    /// statusline therefore shows the *previous* frame's time — honest, since
    /// the current frame's cost isn't known until after it is drawn.
    pub fn record_frame(&mut self, dur: Duration) {
        self.timings.frame = Some(dur);
    }

    /// The last completed pipeline run's timings. `--timing` prints exactly
    /// the rows the `F4` overlay draws, so both read from here.
    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    pub fn update(&mut self, msg: Msg) -> Effect {
        match msg {
            Msg::Key(ev) => self.on_key(ev),
            Msg::Mouse(ev) => self.on_mouse(ev),
            Msg::Resize(w, h) => {
                self.size = (w, h);
                // Anchor: remember which layout fragment sat on the top
                // visible row so relayout can put it back (UX §3.6).
                let anchor = self.top_anchor();
                // Resize is a wrap point: re-wrap at the new width, keep offset.
                self.viewport.resize(w, self.page());
                self.dom_view.resize(w, self.page());
                self.styles_view.resize(w, self.page());
                self.boxes_view.resize(w, self.page());
                self.help_view.resize(w, self.page());
                // ...and the second of exactly two places layout runs, because
                // the column width changed with the frame.
                self.relayout();
                if let Some(anchor) = anchor {
                    self.restore_anchor(anchor);
                }
                // Search match geometry is layout-dependent; re-run the query
                // and keep the current hit on screen when possible.
                self.recompute_search_matches();
                redraw()
            }
            // Terminal input is gone; exit cleanly, the same as the quit key.
            Msg::InputClosed => Effect {
                quit: true,
                ..Effect::default()
            },
            // Net messages: a stale id means a fetch that was superseded — its
            // progress, body, and errors must not clobber the current one, so
            // it changes nothing and triggers no redraw.
            Msg::Loading { id, bytes_so_far } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                match &mut self.fetch {
                    Fetch::Loading {
                        bytes_so_far: bytes,
                        ..
                    } => {
                        *bytes = bytes_so_far;
                        self.spinner = (self.spinner + 1) % SPINNER.len();
                        redraw()
                    }
                    _ => Effect::default(),
                }
            }
            Msg::Loaded {
                id,
                url,
                status,
                body,
                elapsed,
                content_type,
                set_cookie,
                metadata,
            } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                // Only an accepted fetch records its duration (PLAN.md §4) —
                // and it is the **whole chain**, not the last hop of one
                // (M11.7a). A redirect that reported only its landing page
                // would show `--timing` and `F4` a fast fetch with two slow
                // ones hidden inside it, which is the kind of wrong number that
                // is worse than none.
                match self.document_source {
                    net::DocumentSource::Network => {
                        self.timings.fetch = Some(self.hops.elapsed + elapsed);
                        self.timings.cache = Some(timing::CacheOutcome::Network);
                    }
                    net::DocumentSource::CacheHit => {
                        self.timings.fetch = None;
                        self.timings.cache = Some(timing::CacheOutcome::Hit);
                    }
                    net::DocumentSource::Revalidated => {
                        self.timings.fetch = Some(self.hops.elapsed + elapsed);
                        self.timings.cache = Some(timing::CacheOutcome::Revalidated);
                    }
                }
                // The session, before anything else this response does with
                // itself (M11.7). Scoped to the URL the response *came from* —
                // post-redirect, which is what `url` already reports — and not
                // to the one that was asked for.
                //
                // Before the error-page branch on purpose: a 401 or a 302
                // landing on a login page is a response that establishes a
                // session, and a status code is not a reason to throw its
                // cookies away. No pipeline stage runs for this — the jar is a
                // map, and nothing downstream of it is styled or laid out.
                if !set_cookie.is_empty() {
                    js::cookies::apply_set_cookie(
                        &self.cookies,
                        &url,
                        &set_cookie,
                        js::cookies::now(),
                        &self.console,
                    );
                    self.console_view_built = false;
                }
                if status == 304 {
                    let Some(key) = self.cache_candidate.take() else {
                        self.apply_error_page(
                            url,
                            "HTTP 304 without a cached representation".into(),
                        );
                        return redraw();
                    };
                    let response = self.cache.revalidate(&key, &metadata, self.cache_now());
                    if !set_cookie.is_empty() {
                        self.cache.remove(&key);
                    }
                    let Some(response) = response else {
                        self.apply_error_page(
                            url,
                            "HTTP 304 without a cached representation".into(),
                        );
                        return redraw();
                    };
                    self.document_source = net::DocumentSource::Revalidated;
                    return Effect {
                        dirty: true,
                        cached: Some((id, url, response, self.hops.elapsed + elapsed)),
                        ..Effect::default()
                    };
                }
                // Non-document responses become error pages (M7 / UX §3.7).
                if !error_page::is_document(status, content_type.as_deref()) {
                    let reason = if !(200..300).contains(&status) {
                        error_page::http_reason(status)
                    } else {
                        error_page::unsupported_type_reason(content_type.as_deref())
                    };
                    self.apply_error_page(url, reason);
                    return redraw();
                }
                if self.document_source == net::DocumentSource::Network
                    && let Some(key) = self.cache_key.clone()
                {
                    if set_cookie.is_empty() {
                        let _ = self.cache.insert(
                            key,
                            Representation {
                                status,
                                body: body.clone(),
                                content_type: content_type.clone(),
                                metadata,
                            },
                            self.cache_now(),
                        );
                    } else {
                        self.cache.remove(&key);
                    }
                }
                // A redirect drops the fragment — `Location` is joined against
                // the previous URL and a join takes the target's fragment,
                // which a bare path does not have. HTML keeps the original in
                // that case, and so do we: the URL bar, `location.hash` and
                // the row we are about to scroll to have to agree.
                let url = match &self.pending_scroll {
                    Some((pending, PendingScroll::Fragment(fragment)))
                        if *pending == id && !fragment.is_empty() && !url.contains('#') =>
                    {
                        format!("{url}#{fragment}")
                    }
                    _ => url,
                };
                // Document path: show raw body until Parsed lands.
                let text = String::from_utf8_lossy(&body).into_owned();
                self.viewport.set_content(&text, self.size.0, self.page());
                // Session-visited set feeds `:visited` (M6).
                self.visited.insert(url.clone());
                self.fetch = Fetch::Loaded { url, status, body };
                self.clear_page_engine();
                // Interaction state is page-local.
                self.hover = None;
                self.clear_focus();
                self.hint = None;
                self.search = None;
                redraw()
            }
            Msg::Redirect {
                id,
                url,
                to,
                status,
                elapsed,
                set_cookie,
            } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                // **A hop is not a navigation.** The generation does not
                // change, so the fragment the reader clicked still lands, the
                // timers this page scheduled are still its own, and the
                // stylesheet already in flight is still accepted. Minting a new
                // `PageId` here would silently discard all three.
                self.hops.count += 1;
                self.hops.elapsed += elapsed;
                if self.hops.count > MAX_REDIRECTS {
                    self.apply_error_page(to, error_page::redirect_loop_reason(MAX_REDIRECTS));
                    return redraw();
                }
                // The session first, scoped to the URL that *sent* this
                // response — the 302 hands out the cookie, and it belongs to
                // the 302's own host and path, not to wherever the chain ends.
                if !set_cookie.is_empty() {
                    js::cookies::apply_set_cookie(
                        &self.cookies,
                        &url,
                        &set_cookie,
                        js::cookies::now(),
                        &self.console,
                    );
                    self.console_view_built = false;
                }
                // `rewrite_method` — a login's 302 must not keep POST.
                let next_method = match &self.fetch {
                    Fetch::Loading { method, .. } => rewrite_method(status, method),
                    _ => net::Method::Get,
                };
                self.cache_candidate = None;
                if let Fetch::Loading { method, .. } = &mut self.fetch {
                    *method = next_method.clone();
                }
                // Then the page's URL, and only then the next request: the
                // ordering *is* the task. `App::request` measures the jar
                // against `current_url`, so moving it to the target first is
                // what makes the next hop carry the cookies this one just set —
                // and carry the target host's rather than this host's when the
                // hop crosses origins.
                self.set_current_url(to.clone());
                // A hop may name a fragment of its own, and HTML says it wins
                // over the one the reader typed (the typed one is only kept
                // when the target has none — the rule `Loaded` already applies).
                // A write to the slot, not a new one: the generation is the
                // same, so `pending_scroll` is still this navigation's.
                if let Some(fragment) = fragment_of(&to).filter(|f| !f.is_empty()) {
                    self.pending_scroll = Some((id, PendingScroll::Fragment(fragment.to_string())));
                }
                let mut request = self.request(to);
                request.method = next_method;
                self.cache_key = CacheKey::from_request(&request);
                Effect {
                    dirty: true,
                    fetch: Some((id, request)),
                    ..Effect::default()
                }
            }
            Msg::Parsed { id, dom, elapsed } => {
                // Same stale-generation guard as `Loaded`: a slow parse of a
                // superseded page must not clobber the current tree.
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                // Error page already owns the surface — ignore a late parse.
                if matches!(self.fetch, Fetch::Failed { .. }) {
                    return Effect::default();
                }
                self.dom = Some(dom);
                self.timings.parse = Some(elapsed);
                // The page's own CSS: inline blocks are in hand and resolve
                // now, linked ones go out to the loop and land later.
                let sheets = self.adopt_sources(id);
                let images = self.adopt_images(id);
                self.restyle();
                // One of exactly two places layout runs. The page surface
                // switches from the raw body to laid-out lines here, which is
                // also what makes `dom.is_some()` mean "the viewport holds
                // laid-out lines" everywhere below.
                self.relayout();
                // No line building here (see `dom_view_built`) — unless F1 is
                // open right now, in which case the user is watching the tree
                // and it must refresh on this repaint.
                self.dom_view_built = false;
                self.styles_view_built = false;
                self.boxes_view_built = false;
                self.build_visible_inspector();
                Effect {
                    dirty: true,
                    sheets,
                    images,
                    // Ask for the script pass, do not run it here: this turn
                    // owes the user a painted page (UX §3.2), and the pass is
                    // allowed to be slow.
                    run_scripts: Some(id),
                    ..Effect::default()
                }
            }
            Msg::Script { id, slot, source } => {
                // Same stale-generation guard as every other net message: a
                // body requested by a page the user has left must not run.
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                let failed = source.is_none();
                // Read before `fill`, because filling is what stops the slot
                // being pending: `Some(node)` means this body belongs to a
                // script the page inserted, and that element is owed a
                // `load` or an `error` (M11.5).
                let inserted = self.script_queue.element(slot);
                self.script_queue.fill(slot, source);
                if failed {
                    self.console.push(
                        crate::js::console::Level::Warn,
                        None,
                        None,
                        "a <script src> could not be fetched: the rest of the page's \
                         scripts continue",
                    );
                }
                let Some(dom) = self.dom.take() else {
                    return Effect::default();
                };
                let mut effect = self.run_ready_scripts(id, dom);
                // **After** the prefix, never before: `load` means "it ran".
                if let Some(node) = inserted {
                    let event = if failed {
                        js::EventDescriptor::ERROR
                    } else {
                        js::EventDescriptor::LOAD
                    };
                    self.fire_script_event(id, node, event, &mut effect);
                }
                effect
            }
            Msg::JsFetch {
                page,
                request,
                result,
            } => {
                // The same stale-generation guard every producer's message
                // gets: a response for a page the reader has left is dropped,
                // its promise never settles, and the host it belonged to is
                // gone anyway.
                if Some(page) != self.current_fetch {
                    return Effect::default();
                }
                let Some(mut dom) = self.dom.take() else {
                    return Effect::default();
                };
                let url = self.current_url().unwrap_or_default();
                let console = self.console.clone();
                let storage = self.storage.clone();
                let cookies = self.cookies.clone();
                let Some(host) = self.js_host.as_mut() else {
                    self.dom = Some(dom);
                    return Effect::default();
                };

                let before = dom.change_versions();
                let logged_before = console.entries().len();
                js::settle_fetch(
                    host,
                    &mut dom,
                    &js::PageContext {
                        page: page.generation,
                        url: &url,
                        console: &console,
                        storage: &storage,
                        cookies: &cookies,
                    },
                    request,
                    result,
                );
                let after = dom.change_versions();
                self.dom = Some(dom);

                // Settling is a tick: one invalidation cycle, whatever the
                // `.then` chain did inside it.
                let mut effect = self.apply_dom_changes(before, after);
                effect.timers = self.take_timer_requests(page);
                effect.fetches = self.take_fetch_requests(page);
                self.adopt_inserted_scripts(page, &mut effect);
                self.apply_script_navigation(&mut effect);
                if self.console.entries().len() != logged_before {
                    self.console_view_built = false;
                    self.build_visible_inspector();
                    effect.dirty = true;
                }
                effect
            }
            Msg::Timer { page, id } => {
                // The same stale-generation guard every other message uses: a
                // deadline that came up for a page the user has left is not
                // that page's problem any more.
                if Some(page) != self.current_fetch {
                    return Effect::default();
                }
                let Some(mut dom) = self.dom.take() else {
                    return Effect::default();
                };
                let url = self.current_url().unwrap_or_default();
                let console = self.console.clone();
                let storage = self.storage.clone();
                let cookies = self.cookies.clone();
                let Some(host) = self.js_host.as_mut() else {
                    self.dom = Some(dom);
                    return Effect::default();
                };

                let before = dom.change_versions();
                let logged_before = self.console.entries().len();
                let outcome = js::fire_timer(
                    host,
                    &mut dom,
                    &js::PageContext {
                        page: page.generation,
                        url: &url,
                        console: &console,
                        storage: &storage,
                        cookies: &cookies,
                    },
                    id,
                );
                let after = dom.change_versions();
                self.dom = Some(dom);
                let _ = outcome;

                let mut effect = self.apply_dom_changes(before, after);
                effect.timers = self.take_timer_requests(page);
                effect.fetches = self.take_fetch_requests(page);
                self.adopt_inserted_scripts(page, &mut effect);
                self.apply_script_navigation(&mut effect);
                if self.console.entries().len() != logged_before {
                    self.console_view_built = false;
                    self.build_visible_inspector();
                    effect.dirty = true;
                }
                effect
            }
            Msg::RunScripts { id } => {
                // The ordering rule (M10.6): a tick belongs to the generation
                // that scheduled it. Mutations are applied to the current page
                // only, and a generation change cancels the pending cycle —
                // checked here, before any stage runs, because relayouting a
                // page the user has already left is wasted work at best and
                // the wrong page at worst. It is the same `PageId` guard
                // every other message uses.
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                if matches!(self.fetch, Fetch::Failed { .. }) {
                    return Effect::default();
                }
                let Some(dom) = self.dom.take() else {
                    return Effect::default();
                };

                // Built once per page, from the parsed document: every slot
                // exists before any fetch starts. A second pass for the same
                // generation must not rebuild it — that would run every script
                // again — so the queue remembers whose it is.
                let scripts = if self.script_queue_page == Some(id) {
                    Vec::new()
                } else {
                    let (queue, externals) =
                        ScriptQueue::new(js::sources::sources(&dom), &self.console);
                    self.script_queue = queue;
                    self.script_queue_page = Some(id);
                    self.resolve_script_urls(id, externals)
                };

                let mut effect = self.run_ready_scripts(id, dom);
                // The document's externals first, then anything the pass
                // itself inserted (M11.5) — which `run_ready_scripts` has
                // already put in `effect.scripts`, and which an assignment
                // here would silently drop.
                let mut requested = scripts;
                requested.append(&mut effect.scripts);
                effect.scripts = requested;
                effect
            }
            Msg::Image { id, url, result } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                match result {
                    Ok(decoded) => {
                        let need_relayout =
                            self.images.needs_relayout(&url, self.layout_tree.as_ref());
                        self.images.insert(url, decoded);
                        if need_relayout {
                            self.relayout();
                        } else {
                            self.repaint_images();
                        }
                    }
                    Err(_) => {
                        // Soft failure: leave placeholder; no layout change.
                    }
                }
                redraw()
            }
            Msg::Stylesheet { id, slot, sheet } => {
                // Same stale-generation guard as every other net message: a
                // sheet requested by a page the user has already navigated
                // away from must not touch the current one.
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                let Some(entry) = self.sheets.get_mut(slot) else {
                    return Effect::default();
                };
                // A failed fetch resolves the slot to an empty sheet rather
                // than leaving it pending forever: the page is degraded, not
                // broken, and the cascade proceeds with what it has.
                *entry = Some(sheet.unwrap_or_default());
                self.restyle();
                self.styles_view_built = false;
                // Relayout first so F3/F2 rebuild (at end of relayout) sees the
                // new tree and styles — not the pre-sheet geometry.
                self.relayout();
                redraw()
            }
            Msg::NetError { id, url, reason } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                self.apply_error_page(url, reason);
                redraw()
            }
            // These are owned by the outer browser session. Keeping harmless
            // arms here makes accidental page routing unable to mutate a page.
            Msg::BookmarksLoaded(_) | Msg::BookmarksSaved { .. } => Effect::default(),
        }
    }

    fn on_key(&mut self, ev: KeyEvent) -> Effect {
        // Hint mode owns label typing until Esc or a completed label — but
        // quit must always work (PLAN.md §3), so it is checked first.
        if self.hint.is_some() {
            return self.on_hint_key(&ev);
        }
        let mode = match self.mode {
            Mode::Browse => keys::Mode::Browse,
            Mode::UrlInput { .. } => keys::Mode::UrlInput,
            Mode::SearchInput { .. } => keys::Mode::SearchInput,
            Mode::Field { .. } => keys::Mode::Field,
            Mode::Select { .. } => keys::Mode::Select,
        };
        match keys::resolve(mode, self.pending, &ev) {
            // Not a Press event: leave the pending prefix untouched.
            Resolution::Ignore => Effect::default(),
            // A prefix opened; wait for the next key. A pending prefix is not a
            // visible change, so it is not dirty.
            Resolution::Pending(c) => {
                self.pending = Some(c);
                Effect::default()
            }
            Resolution::Action(action) => {
                self.pending = None;
                self.run(action)
            }
            Resolution::Unbound => {
                self.pending = None;
                // The one sanctioned key path outside the binding table
                // (CLAUDE.md): in the URL bar, the search prompt and a form
                // control (M11.9) a printable character types. `q` is a letter
                // in all three, and quit is kept by `Ctrl-c`, which every one
                // of them binds. Widened here rather than copied: a second
                // place that reads `KeyCode::Char` would be a second exception,
                // and the rule allows one. `resolve` only yields `Unbound` for
                // Press events.
                if let KeyCode::Char(c) = ev.code
                    && !ev
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    match &mut self.mode {
                        Mode::UrlInput { buffer } | Mode::SearchInput { buffer } => {
                            buffer.push(c);
                            return redraw();
                        }
                        Mode::Field { .. } => return self.insert_in_field(c),
                        Mode::Browse | Mode::Select { .. } => {}
                    }
                }
                Effect::default()
            }
        }
    }

    fn on_mouse(&mut self, ev: MouseEvent) -> Effect {
        // URL bar, inspectors and a field being typed into: no page
        // hit-testing. Wheel still scrolls whichever surface is active.
        //
        // Typing swallows the click the same way the URL bar does, and for the
        // same reason: leaving a mode is `Esc`, one key, always. A click that
        // blurred the field would be a second way out that only exists for
        // readers who have a mouse — and the caret's key model has to be
        // complete without one.
        match ev.kind {
            MouseEventKind::ScrollDown => moved(self.scroll_target().scroll_down()),
            MouseEventKind::ScrollUp => moved(self.scroll_target().scroll_up()),
            MouseEventKind::Down(MouseButton::Left) => {
                if !matches!(self.mode, Mode::Browse) || self.surface != Surface::Page {
                    return Effect::default();
                }
                self.on_click(ev.column, ev.row)
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if !matches!(self.mode, Mode::Browse) || self.surface != Surface::Page {
                    return Effect::default();
                }
                self.on_hover_move(ev.column, ev.row)
            }
            _ => Effect::default(),
        }
    }

    fn run(&mut self, action: Action) -> Effect {
        // Yank *sets* the flash; every other action clears a stale one so the
        // middle status segment is not stuck on "yanked" forever.
        if !matches!(action, Action::YankUrl) {
            self.status_msg = None;
        }
        match action {
            Action::Quit => Effect {
                quit: true,
                ..Effect::default()
            },
            Action::ScrollDown => moved(self.scroll_target().scroll_down()),
            Action::ScrollUp => moved(self.scroll_target().scroll_up()),
            Action::HalfPageDown => moved(self.scroll_target().half_page_down()),
            Action::HalfPageUp => moved(self.scroll_target().half_page_up()),
            Action::Top => moved(self.scroll_target().scroll_to_top()),
            Action::Bottom => moved(self.scroll_target().scroll_to_bottom()),
            Action::OpenUrl => {
                self.hint = None;
                self.surface = Surface::Page;
                self.mode = Mode::UrlInput {
                    buffer: String::new(),
                };
                redraw()
            }
            Action::EditUrl => {
                self.hint = None;
                self.surface = Surface::Page;
                let buffer = self.current_url().unwrap_or_default();
                self.mode = Mode::UrlInput { buffer };
                redraw()
            }
            Action::NewTab => tab_action(TabAction::New),
            Action::CloseTab => tab_action(TabAction::Close),
            Action::NextTab => tab_action(TabAction::Next),
            Action::PreviousTab => tab_action(TabAction::Previous),
            Action::AddBookmark
            | Action::ToggleBookmarks
            | Action::BookmarkNext
            | Action::BookmarkPrevious
            | Action::BookmarkFirst
            | Action::BookmarkLast
            | Action::BookmarkOpen
            | Action::BookmarkDelete => Effect::default(),
            Action::ToggleDom => self.toggle_surface(Surface::Dom),
            Action::ToggleStyles => self.toggle_surface(Surface::Styles),
            Action::ToggleBoxes => self.toggle_surface(Surface::Boxes),
            Action::ToggleConsole => self.toggle_surface(Surface::Console),
            Action::ToggleTiming => {
                self.timing_visible = !self.timing_visible;
                redraw()
            }
            Action::Commit => self.commit(),
            Action::Cancel => {
                if self.hint.take().is_some() {
                    return redraw();
                }
                if self.surface == Surface::Help {
                    self.surface = Surface::Page;
                    return redraw();
                }
                // URL bar / search: drop the buffer and return to browse.
                // A field keeps its focus and its value — `Esc` leaves the
                // mode, not the control (M11.9), so the reader lands back on
                // the page with `j`, `/` and `q` live and the caret gone.
                // Already in browse with nothing to cancel → not dirty.
                if matches!(
                    self.mode,
                    Mode::UrlInput { .. }
                        | Mode::SearchInput { .. }
                        | Mode::Field { .. }
                        | Mode::Select { .. }
                ) {
                    self.mode = Mode::Browse;
                    return redraw();
                }
                Effect::default()
            }
            Action::DeleteChar => {
                if let Mode::Field { .. } = self.mode {
                    return self.delete_in_field(false);
                }
                if let Mode::UrlInput { buffer } | Mode::SearchInput { buffer } = &mut self.mode {
                    buffer.pop();
                }
                redraw()
            }
            Action::DeleteCharForward => self.delete_in_field(true),
            Action::CaretLeft => self.move_caret(CaretMove::Prev),
            Action::CaretRight => self.move_caret(CaretMove::Next),
            Action::CaretToStart => self.move_caret(CaretMove::LineStart),
            Action::CaretToEnd => self.move_caret(CaretMove::LineEnd),
            Action::HintFollow => self.start_hints(false),
            Action::HintYank => self.start_hints(true),
            Action::FocusNext => self.cycle_focus(1),
            Action::FocusPrev => self.cycle_focus(-1),
            Action::FollowFocus => self.follow_focus(),
            Action::HistoryBack => self.history_go(true),
            Action::HistoryForward => self.history_go(false),
            Action::Reload => self.reload(),
            Action::YankUrl => self.yank_page_url(),
            Action::OpenSearch => {
                self.hint = None;
                // Starting a new find clears the previous session so old
                // highlights do not linger while the user types a new query.
                self.search = None;
                self.status_msg = None;
                self.surface = Surface::Page;
                self.mode = Mode::SearchInput {
                    buffer: String::new(),
                };
                redraw()
            }
            // `Enter` while typing. The form is whatever the caret is in, and
            // nothing else about the page is consulted.
            Action::Submit => match self.focus {
                Some(node) => self.submit_form(node),
                None => Effect::default(),
            },
            Action::SearchNext => self.search_step(1),
            Action::SearchPrev => self.search_step(-1),
            Action::ToggleHelp => self.toggle_surface(Surface::Help),
            Action::SelectPrev => self.move_select(-1, false),
            Action::SelectNext => self.move_select(1, false),
            Action::SelectFirst => self.move_select(1, true),
            Action::SelectLast => self.move_select(-1, true),
            Action::SelectToggle => self.toggle_select_option(),
            Action::SelectCommit => self.commit_select(),
        }
    }

    /// Show `surface`, or go back to the page if it is already showing.
    fn toggle_surface(&mut self, surface: Surface) -> Effect {
        // Hints only paint on the page; leaving the page cancels them so keys
        // do not navigate against an invisible overlay.
        self.hint = None;
        self.surface = if self.surface == surface {
            Surface::Page
        } else {
            surface
        };
        self.build_visible_inspector();
        redraw()
    }

    /// Whether the F1 surface currently owns the page area (and therefore the
    /// scroll keys and the scroll-% readout). With no parsed tree the surface
    /// is a static placeholder — the page keeps the scroll keys so they never
    /// dead-end.
    fn dom_active(&self) -> bool {
        self.surface == Surface::Dom && self.dom.is_some()
    }

    /// The same for `F2`, which needs a styled tree rather than just a parsed
    /// one — though in practice they arrive together.
    fn styles_active(&self) -> bool {
        self.surface == Surface::Styles && self.styles.is_some()
    }

    fn boxes_active(&self) -> bool {
        self.surface == Surface::Boxes && self.layout_tree.is_some()
    }

    fn console_active(&self) -> bool {
        self.surface == Surface::Console
    }

    fn help_active(&self) -> bool {
        self.surface == Surface::Help
    }

    /// Wipe engine state for a new body or an error page. Timing stages other
    /// than fetch are cleared so the table never mixes two runs.
    fn clear_page_engine(&mut self) {
        self.dom = None;
        self.dom_view_built = false;
        self.styles_view_built = false;
        self.boxes_view_built = false;
        self.layout_tree = None;
        self.laid_out_size = None;
        self.display_list = DisplayList::default();
        self.sheets.clear();
        self.styles = None;
        self.timings.parse = None;
        self.timings.style = None;
        self.timings.layout = None;
        self.revealed = false;
        self.search = None;
        // Page bookkeeping only — the LRU survives for back/forward (M8).
        self.images.clear_page();
    }

    /// Show a synthetic error page (M7) and leave the app in `Fetch::Failed`.
    fn apply_error_page(&mut self, url: String, reason: String) {
        let text = error_page::render(&url, &reason);
        self.viewport.set_content(&text, self.size.0, self.page());
        self.fetch = Fetch::Failed { url, reason };
        self.clear_page_engine();
        self.hover = None;
        self.clear_focus();
        self.hint = None;
        self.pending_scroll = None;
        if self.surface != Surface::Help {
            self.surface = Surface::Page;
        }
    }

    /// Layout fragment that intersects the top visible document row.
    /// Prefer text boxes (what the reader sees); fall back to blocks.
    /// Stores the fragment index among that node's text boxes so a mid-
    /// paragraph scroll does not snap to the element's first line after resize.
    fn top_anchor(&self) -> Option<ScrollAnchor> {
        let tree = self.layout_tree.as_ref()?;
        let top = self.viewport.offset() as i32;
        let mut best_text: Option<(NodeId, i32)> = None;
        let mut best_block: Option<(NodeId, i32)> = None;
        tree.walk(tree.root, &mut |_, b| {
            let Some(node) = b.node else {
                return;
            };
            let y = b.dimensions.content.y;
            let h = b.dimensions.content.height.max(1);
            if y + h <= top || y > top {
                return;
            }
            match b.kind {
                // Deeper text fragments overwrite shallower ones (walk order).
                BoxKind::Text => best_text = Some((node, y)),
                // A flex container is as good a scroll anchor as any other
                // block-level box (M9.6).
                BoxKind::Block | BoxKind::Flex => best_block = Some((node, y)),
                _ => {}
            }
        });
        let (node, box_y) = best_text.or(best_block)?;
        // Index among Text boxes for this node (document walk order).
        let mut text_index = 0usize;
        let mut seen = 0usize;
        let mut found = false;
        tree.walk(tree.root, &mut |_, b| {
            if found || b.kind != BoxKind::Text || b.node != Some(node) {
                return;
            }
            if b.dimensions.content.y == box_y {
                text_index = seen;
                found = true;
            }
            seen += 1;
        });
        Some(ScrollAnchor {
            node,
            text_index,
            box_y,
        })
    }

    /// After relayout, restore the anchored fragment to the top of the viewport.
    fn restore_anchor(&mut self, anchor: ScrollAnchor) {
        let Some(tree) = &self.layout_tree else {
            return;
        };
        let mut text_ys: Vec<i32> = Vec::new();
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Text && b.node == Some(anchor.node) {
                text_ys.push(b.dimensions.content.y);
            }
        });
        let y = text_ys
            .get(anchor.text_index)
            .copied()
            .or_else(|| {
                // Fragment count changed (rewrap): nearest y to the old one.
                text_ys
                    .iter()
                    .copied()
                    .min_by_key(|y| (*y - anchor.box_y).unsigned_abs())
            })
            .or_else(|| layout::first_y(tree, anchor.node));
        if let Some(y) = y {
            let _ = self.viewport.scroll_to_offset(y.max(0) as usize);
        }
    }

    fn recompute_search_matches(&mut self) {
        let Some(session) = &self.search else {
            return;
        };
        let query = session.query.clone();
        let current = session.current;
        let Some(tree) = &self.layout_tree else {
            self.search = None;
            return;
        };
        let matches = search::find_matches(tree, &query);
        if matches.is_empty() {
            self.search = Some(SearchSession {
                query,
                matches,
                current: 0,
            });
            return;
        }
        let current = current.min(matches.len() - 1);
        self.search = Some(SearchSession {
            query,
            matches,
            current,
        });
        self.scroll_search_into_view();
    }

    fn commit_search(&mut self, query: String) -> Effect {
        self.mode = Mode::Browse;
        let query = query.trim().to_string();
        if query.is_empty() {
            self.search = None;
            return redraw();
        }
        let Some(tree) = &self.layout_tree else {
            self.search = None;
            self.status_msg = Some("no matches".into());
            return redraw();
        };
        let matches = search::find_matches(tree, &query);
        if matches.is_empty() {
            self.search = Some(SearchSession {
                query,
                matches,
                current: 0,
            });
            self.status_msg = Some("no matches".into());
            return redraw();
        }
        self.search = Some(SearchSession {
            query,
            matches,
            current: 0,
        });
        self.status_msg = None;
        self.scroll_search_into_view();
        redraw()
    }

    fn search_step(&mut self, dir: i32) -> Effect {
        let Some(session) = &mut self.search else {
            return Effect::default();
        };
        if session.matches.is_empty() {
            return Effect::default();
        }
        let n = session.matches.len();
        if dir >= 0 {
            session.current = (session.current + 1) % n;
        } else {
            session.current = (session.current + n - 1) % n;
        }
        self.scroll_search_into_view();
        redraw()
    }

    fn scroll_search_into_view(&mut self) {
        let Some(session) = &self.search else {
            return;
        };
        if session.matches.is_empty() {
            return;
        }
        let y = session.matches[session.current].y as usize;
        let page = self.page() as usize;
        let off = self.viewport.offset();
        if y < off {
            let _ = self.viewport.scroll_to_offset(y);
        } else if y >= off + page {
            let _ = self
                .viewport
                .scroll_to_offset(y.saturating_sub(page.saturating_sub(1)));
        }
    }

    /// Discover `<img>` tags and return absolute URLs that still need a fetch.
    fn adopt_images(&mut self, id: PageId) -> Vec<(PageId, net::Request)> {
        let Some(dom) = &self.dom else {
            return Vec::new();
        };
        let base = match &self.fetch {
            Fetch::Loaded { url, .. } => Some(url.as_str()),
            _ => None,
        };
        // Discovery first, then the jar: `adopt` borrows `self.images`
        // mutably, and `request` reads the whole of `self`.
        let found = self.images.adopt(dom, base, id);
        found
            .into_iter()
            .map(|(id, url)| (id, self.request(url)))
            .collect()
    }

    /// Rebuild the display list from the existing layout tree after an image
    /// lands with a firm size (no geometry change).
    fn repaint_images(&mut self) {
        if let Some(tree) = &self.layout_tree {
            let pixels = self.images.pixels();
            self.display_list = paint::paint_with(tree, &pixels);
            #[cfg(test)]
            {
                self.paints += 1;
            }
        }
    }

    /// Lay the cached tree out at the current column width and hand the lines
    /// to the page surface. Called from exactly two places — a parse landing
    /// and a resize — and never from the scroll path, which only moves an
    /// offset over these lines (CLAUDE.md: scrolling never relayouts).
    ///
    /// Layout stays on the UI thread while fetch and parse do not: it is a
    /// pure transform costing single-digit milliseconds even on Wikipedia, and
    /// its input (the cached tree, the current width) is already here. Moving
    /// it to a worker would buy a frame of latency and cost a round trip.
    fn relayout(&mut self) {
        // Both or neither: `restyle` runs with every tree that lands, so a
        // parsed page always has computed values to lay out with.
        let (Some(dom), Some(styles)) = (&self.dom, &self.styles) else {
            return;
        };
        let started = Instant::now();
        let width = column(self.size.0).width;
        let img_ctx = self.images.context();
        // One layout (or two if we have to reveal a page that hid itself).
        let tree = layout::layout_document_with_viewport(
            dom,
            styles,
            width,
            self.page(),
            layout::Hidden::Respect,
            &img_ctx,
        );
        let mut lines = layout::lines_from_tree(&tree);
        let (tree, revealed) = if lines.iter().any(|l| !l.spans.is_empty()) {
            (tree, false)
        } else {
            let alt = layout::layout_document_with_viewport(
                dom,
                styles,
                width,
                self.page(),
                layout::Hidden::Reveal,
                &img_ctx,
            );
            let alt_lines = layout::lines_from_tree(&alt);
            if alt_lines.iter().any(|l| !l.spans.is_empty()) {
                lines = alt_lines;
                (alt, true)
            } else {
                (tree, false)
            }
        };
        let pixels = self.images.pixels();
        self.display_list = paint::paint_with(&tree, &pixels);
        #[cfg(test)]
        {
            self.paints += 1;
        }
        self.layout_tree = Some(tree);
        self.laid_out_size = Some(self.size);
        self.boxes_view_built = false;
        self.revealed = revealed;
        // The one place `App` reads the clock: fetch and parse are timed by the
        // worker that runs them, but this stage runs here, so it times itself.
        self.timings.layout = Some(started.elapsed());
        #[cfg(test)]
        {
            self.layouts += 1;
        }
        self.viewport.set_lines(lines, self.page());
        // History/reload restore: only apply when this layout belongs to the
        // generation that requested it. A resize while the *old* page is still
        // on screen (fetch Loading, previous DOM live) must not consume it.
        //
        // A fragment is resolved here for the same reason and with the same
        // guard: it is the first moment the node it names has a box. It is
        // consumed here too — resolved once, never retried. A script that
        // appends the target element later does not pull the viewport, because
        // a page that yanks the reader somewhere seconds after they arrived is
        // worse than one that left them at the top; the reader sees the page
        // where it opened, the URL bar showing the fragment, and clicking the
        // link again jumps now that the element exists.
        if matches!(&self.pending_scroll, Some((id, _)) if Some(*id) == self.current_fetch)
            && matches!(self.fetch, Fetch::Loaded { .. })
            && let Some((_, pending)) = self.pending_scroll.take()
        {
            match pending {
                PendingScroll::Offset(scroll) => {
                    let _ = self.viewport.scroll_to_offset(scroll);
                }
                PendingScroll::Fragment(fragment) => self.scroll_to_fragment(&fragment),
            }
        }
        // F3 (and any open inspector) must reflect the new geometry — not a
        // stale cache from before this relayout (resize / stylesheet / parse).
        self.build_visible_inspector();
    }

    /// Take the freshly parsed tree's stylesheet sources: parse the inline
    /// blocks now, and return the linked ones for the loop to fetch. Slots are
    /// allocated for every source up front, so document order survives however
    /// the network reorders the arrivals.
    ///
    /// Inline blocks parse on the UI thread because their bytes are already
    /// here — the round trip to a worker would cost more than the parse. The
    /// measured worst case in the fixtures is Wikipedia's 21 blocks; the
    /// number is in perf.md.
    fn adopt_sources(&mut self, id: PageId) -> Vec<(PageId, usize, net::Request)> {
        let Some(dom) = &self.dom else {
            return Vec::new();
        };
        // Relative hrefs resolve against the page's post-redirect URL, which is
        // exactly what `Fetch::Loaded` holds; the `Loaded` for this generation
        // always precedes its `Parsed` (same worker, in order).
        let base = match &self.fetch {
            Fetch::Loaded { url, .. } => Some(url.clone()),
            _ => None,
        };
        let mut pending = Vec::new();
        self.sheets = sources::sources(dom)
            .into_iter()
            .enumerate()
            .map(|(slot, source)| match source {
                Source::Inline(css) => Some(crate::css::parse(&css)),
                Source::Link(href) => {
                    match base
                        .as_deref()
                        .and_then(|base| net::resolve_url(base, &href))
                    {
                        Some(url) => {
                            pending.push((id, slot, url));
                            None
                        }
                        // Unresolvable href: settle the slot empty instead of
                        // leaving a hole nothing will ever fill.
                        None => Some(Stylesheet::default()),
                    }
                }
            })
            .collect();
        // The jar last, once the slots are settled: `request` reads the whole
        // of `self`, and the assignment above holds `self.sheets`.
        pending
            .into_iter()
            .map(|(id, slot, url)| (id, slot, self.request(url)))
            .collect()
    }

    /// Recompute the styled tree from the tree and whatever sheets have
    /// arrived. Called when a page parses and when each sheet lands — never on
    /// the scroll path (CLAUDE.md: scrolling never restyles). Hover and visited
    /// feed through [`StyleContext`] (M6).
    fn restyle(&mut self) {
        let Some(dom) = &self.dom else {
            self.styles = None;
            self.timings.style = None;
            return;
        };
        let started = Instant::now();
        // Sheets that have not arrived are simply absent from this pass; the
        // next arrival runs it again.
        let sheets: Vec<&Stylesheet> = self.sheets.iter().flatten().collect();
        let base = self.current_url();
        let ctx = StyleContext {
            hover: self.hover,
            visited: &self.visited,
            base_url: base.as_deref(),
        };
        self.styles = Some(style::style_tree_with(dom, &sheets, &ctx));
        // Timed here for the same reason layout is: this stage runs on the UI
        // thread, so it measures itself rather than arriving as message data.
        self.timings.style = Some(started.elapsed());
        #[cfg(test)]
        {
            self.styles_run += 1;
            // A fresh tree starts its count at zero, so this pass *is* its
            // count — every node the walk reached.
            self.nodes_styled += self.styles.as_ref().map_or(0, Styles::nodes_styled);
        }
    }

    /// The narrowed restyle (M11.3): recompute only the subtrees this tick's
    /// attribute writes can have reached, in place, and leave the rest of the
    /// styled tree alone.
    ///
    /// Returns the values the page was **laid out** with, which the caller
    /// compares the new ones against — the scoped pass writes in place, so
    /// that tree has to be copied out of the way before it runs, and this is
    /// the only place that knows the pass is going ahead. `None` means the tick
    /// did not qualify and the caller must run a full pass, which replaces the
    /// tree rather than editing it and so needs no copy at all.
    ///
    /// Three ways it does not qualify, all of them the whole document's fault
    /// rather than a node's:
    ///
    /// - the writes overflowed the arena's list, so there is no scope to narrow
    ///   to (and by then the subtrees would add up to the document anyway);
    /// - nothing has been styled or parsed yet, so there are no parent values
    ///   to resolve against;
    /// - a script created a node, which grew the arena without changing the
    ///   tree's shape — the dense styled `Vec` is a slot short and a full pass
    ///   is what resizes it.
    ///
    /// Correctness rests entirely on [`style::restyle_subtree`]'s argument
    /// about combinators; read it before changing either side.
    #[must_use]
    fn restyle_scoped(&mut self, changes: &AttrChanges) -> Option<Styles> {
        // The A/B switch, and the only thing that reads `full_restyle_only`.
        #[cfg(test)]
        if self.full_restyle_only {
            return None;
        }
        let AttrChanges::Nodes(roots) = changes else {
            return None;
        };
        // An attribute-only tick that wrote to nothing cannot happen: the three
        // `Edit` kinds are structural (handled by the caller), an attribute
        // write (which lands in this list), and creating a detached node (which
        // grows the arena past the size check below). Asserted rather than
        // trusted, because the failure would be silent — a tick that skipped
        // its restyle and looked like one that had nothing to do.
        debug_assert!(
            !roots.is_empty(),
            "an attribute-only tick reported no attribute writes"
        );
        // Both read `&self`, so they cannot outlive the borrow of `styles`.
        let base = self.current_url();
        let hover = self.hover;
        // The tree is the one the last full pass built, so its count already
        // has that pass in it; only the growth belongs to this one.
        #[cfg(test)]
        let styled_before = self.styles.as_ref().map_or(0, Styles::nodes_styled);
        let started = Instant::now();
        let previous;
        {
            let (Some(dom), Some(styles)) = (self.dom.as_ref(), self.styles.as_mut()) else {
                return None;
            };
            if styles.node_count() != dom.node_count() {
                return None;
            }
            // Past every bail: the copy is paid only by a tick that actually
            // narrows, never by one that falls back.
            previous = styles.clone();
            let sheets: Vec<&Stylesheet> = self.sheets.iter().flatten().collect();
            let ctx = StyleContext {
                hover,
                visited: &self.visited,
                base_url: base.as_deref(),
            };
            style::restyle_subtree(dom, &sheets, &ctx, styles, roots);
        }
        self.timings.style = Some(started.elapsed());
        #[cfg(test)]
        {
            self.styles_run += 1;
            self.nodes_styled += self
                .styles
                .as_ref()
                .map_or(0, Styles::nodes_styled)
                .saturating_sub(styled_before);
        }
        Some(previous)
    }

    /// The invalidation cycle for a tick that ran JavaScript (M10.6), given
    /// the arena's `(version, structure_version, live_state_version)` before
    /// and after it.
    ///
    /// Three outcomes, cheapest first, and the classification is the whole
    /// point — the win is in *not running* a stage, not in running it faster:
    ///
    /// - **Nothing changed** → nothing runs. A handler that reads the page and
    ///   decides to do nothing must cost nothing.
    /// - **Attributes only** → restyle *the subtrees the writes can have
    ///   reached* (M11.3), then ask the computed values whether layout would
    ///   even differ. A `class` toggle that only changes a colour takes
    ///   `:hover`'s path: recolour the existing tree and repaint.
    /// - **The tree or its text changed** → restyle and relayout. Boxes were
    ///   added, removed or resized; there is nothing to compare against.
    ///
    /// The middle case is the only narrowing, and it is bounded by
    /// correctness at both ends. The *scope* comes from
    /// [`style::restyle_subtree`], whose argument about combinators is the
    /// thing to read before touching this. The *verdict* comes from comparing
    /// the two `Styles` — O(nodes) of `Copy` structs — where
    /// `ComputedStyle::layout_eq` treats any property it does not explicitly
    /// exempt as layout-relevant, so a wrong answer costs a relayout nobody
    /// needed rather than a page that failed to update. That comparison is why
    /// the styled tree is cloned rather than replaced: the scoped pass writes
    /// in place, and the values the page was laid out with have to survive it.
    fn apply_dom_changes(&mut self, before: (u64, u64, u64), after: (u64, u64, u64)) -> Effect {
        let (edits_before, structure_before, live_before) = before;
        let (edits_after, structure_after, live_after) = after;
        // Read unconditionally, and before the early return: the list describes
        // this tick, and one that survived into the next would narrow a restyle
        // against writes already accounted for.
        let changes = self.dom.as_mut().map(Dom::take_attr_changes);
        if edits_after == edits_before {
            return Effect::default();
        }

        let structural = structure_after != structure_before;
        let live_state = live_after != live_before;
        // A control's box also derives directly from HTML attributes that need
        // not participate in CSS (`checked`, `selected`, `size`, `label`, ...).
        // Any write on this small family therefore invalidates layout even when
        // the scoped style comparison says the computed CSS stayed identical.
        let control_attribute = match (&changes, self.dom.as_ref()) {
            (Some(AttrChanges::TooMany), _) => true,
            (Some(AttrChanges::Nodes(nodes)), Some(dom)) => nodes.iter().any(|&node| {
                matches!(
                    &dom.node(node).data,
                    NodeData::Element { tag, .. }
                        if matches!(tag.as_str(), "input" | "textarea" | "button" | "select" | "option" | "optgroup")
                )
            }),
            _ => false,
        };
        let has_attributes =
            !matches!(changes, Some(AttrChanges::Nodes(ref nodes)) if nodes.is_empty());
        // Sparse current values and choices feed layout directly but cannot
        // move a selector answer. Keep them out of the CSS path entirely.
        // A mixed script tick still goes through the ordinary structural /
        // attribute classification, so one action has one coherent pass.
        if live_state && !structural && !has_attributes {
            self.relayout();
            self.validate_select_mode();
            self.dom_view_built = false;
            self.boxes_view_built = false;
            self.build_visible_inspector();
            return redraw();
        }
        // M11.3: an attribute-only tick recomputes the subtrees its writes can
        // have reached, not the document. Everything downstream is unchanged —
        // the comparison below is what keeps the narrowing honest.
        let scoped = match structural {
            true => None,
            false => changes.as_ref().and_then(|c| self.restyle_scoped(c)),
        };
        // The values the page was laid out with, so the comparison below has
        // something to compare the new ones against. The scoped pass edits the
        // tree in place and so hands back a copy; every other path replaces it,
        // and can move the old one out for free — which is what M10.6 did, and
        // what keeps a tick that falls back paying nothing for the narrowing.
        let previous = match scoped {
            Some(previous) => Some(previous),
            None => {
                let previous = self.styles.take();
                self.restyle();
                previous
            }
        };

        let needs_layout = structural
            || live_state
            || control_attribute
            || match (&previous, &self.styles) {
                (Some(old), Some(new)) => !old.layout_eq(new),
                // No previous styles means nothing has been laid out from
                // them yet; lay out rather than guess.
                _ => true,
            };

        if needs_layout {
            self.relayout();
        } else {
            self.recolour_and_repaint();
        }
        self.validate_select_mode();

        self.dom_view_built = false;
        self.styles_view_built = false;
        self.boxes_view_built = false;
        self.build_visible_inspector();
        redraw()
    }

    /// Restyle + rebuild the display list from the existing layout tree — no
    /// geometry change. Used for `:hover` (PLAN.md M6: restyle + repaint only).
    ///
    /// Deliberately a **full** restyle, and M11.3 left it that way rather than
    /// overlooking it: hover moves *between* two elements, and the subtree
    /// argument in [`style::restyle_subtree`] is about the element being
    /// entered. The one being left has to lose its `:hover` styling in the same
    /// pass, and `hover` is not an attribute write — the arena's change list
    /// knows nothing about it. Narrowing this needs its own reasoning and its
    /// own measurement, so it is its own task.
    fn restyle_and_repaint(&mut self) {
        self.restyle();
        self.recolour_and_repaint();
    }

    /// The half of [`Self::restyle_and_repaint`] after the restyle: push the
    /// new computed values into the existing layout tree and repaint from it,
    /// with no geometry recomputed.
    ///
    /// Split out for M10.6, whose attribute-only path has already restyled by
    /// the time it gets here — and restyling twice would be both wasted work
    /// and a second increment on a counter that exists to catch exactly that.
    fn recolour_and_repaint(&mut self) {
        self.styles_view_built = false;
        if let (Some(tree), Some(styles)) = (self.layout_tree.as_mut(), self.styles.as_ref()) {
            recolour_tree(tree, styles);
            // Rebuild after recolour; need pixels map from cache.
        }
        if let Some(tree) = &self.layout_tree {
            let pixels = self.images.pixels();
            self.display_list = paint::paint_with(tree, &pixels);
            #[cfg(test)]
            {
                self.paints += 1;
            }
        }
        self.build_visible_inspector();
    }

    /// Kitty graphics bytes for the current page view (or `None` to skip).
    /// Mutates session placement state so identical frames emit nothing.
    pub fn kitty_frame(&mut self) -> Option<Vec<u8>> {
        self.kitty_frame_with_offset(0)
    }

    fn kitty_frame_with_offset(&mut self, row_offset: u16) -> Option<Vec<u8>> {
        let left = column(self.size.0).left;
        let scroll = self.viewport.offset() as i32;
        let on_page = self.surface == Surface::Page && self.dom.is_some();
        self.images.kitty_frame(
            &self.display_list,
            (left, row_offset),
            scroll,
            self.page(),
            self.size.0,
            on_page,
        )
    }

    /// Render `dom` into the F1 surface's lines at the current size, if it
    /// isn't already. Called only at the two moments the tree is about to be
    /// shown (see `dom_view_built`); every scroll and repaint in between
    /// reads the cached lines.
    fn build_dom_view(&mut self) {
        if self.dom_view_built {
            return;
        }
        if let Some(dom) = &self.dom {
            let text = inspector::tree_lines(dom).join("\n");
            self.dom_view.set_content(&text, self.size.0, self.page());
            self.dom_view_built = true;
        }
    }

    /// The `F2` equivalent, deferred the same way and for the same reason: a
    /// Wikipedia-sized render costs milliseconds nobody should pay for a
    /// surface they are not looking at.
    fn build_styles_view(&mut self) {
        if self.styles_view_built {
            return;
        }
        if let (Some(dom), Some(styles)) = (&self.dom, &self.styles) {
            let text = inspector::style_lines(dom, styles).join("\n");
            self.styles_view
                .set_content(&text, self.size.0, self.page());
            self.styles_view_built = true;
        }
    }

    fn build_boxes_view(&mut self) {
        if self.boxes_view_built {
            return;
        }
        if let (Some(dom), Some(tree)) = (&self.dom, &self.layout_tree) {
            let text = inspector::box_lines(dom, tree).join("\n");
            self.boxes_view.set_content(&text, self.size.0, self.page());
            self.boxes_view_built = true;
        }
    }

    /// The console pane's lines, built when it is about to be seen and never
    /// on the scroll path — the same deferred-build rule as `F1`–`F3`.
    fn build_console_view(&mut self) {
        if self.console_view_built {
            return;
        }
        let text = self
            .console
            .entries()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        self.console_view
            .set_content(&text, self.size.0, self.page());
        self.console_view_built = true;
    }

    /// Refresh whichever inspector is on screen, and only that one. Called when
    /// a surface opens and when its input changes underneath it (a parse, an
    /// arriving stylesheet).
    fn build_visible_inspector(&mut self) {
        match self.surface {
            Surface::Page => {}
            Surface::Dom => self.build_dom_view(),
            Surface::Styles => self.build_styles_view(),
            Surface::Boxes => self.build_boxes_view(),
            Surface::Console => self.build_console_view(),
            Surface::Help => self.build_help_view(),
        }
    }

    fn build_help_view(&mut self) {
        if self.help_view_built {
            return;
        }
        let text = help::help_text();
        self.help_view.set_content(&text, self.size.0, self.page());
        self.help_view_built = true;
    }

    /// The view the shared scroll keys act on: the same bindings drive the
    /// page and the inspectors, whichever is on screen (the brief's "do not
    /// invent a second scheme").
    fn scroll_target(&mut self) -> &mut Viewport {
        if self.dom_active() {
            &mut self.dom_view
        } else if self.styles_active() {
            &mut self.styles_view
        } else if self.boxes_active() {
            &mut self.boxes_view
        } else if self.console_active() {
            &mut self.console_view
        } else if self.help_active() {
            &mut self.help_view
        } else {
            &mut self.viewport
        }
    }

    /// Commit the URL bar or the search prompt.
    fn commit(&mut self) -> Effect {
        match &self.mode {
            Mode::UrlInput { buffer } => {
                let url = net::normalize_url(buffer);
                self.mode = Mode::Browse;
                self.navigate(url, true)
            }
            Mode::SearchInput { buffer } => {
                let query = buffer.clone();
                self.commit_search(query)
            }
            // A field binds no `Commit`: `Enter` while typing is M11.10's, and
            // it submits the form rather than confirming a buffer.
            Mode::Browse | Mode::Field { .. } | Mode::Select { .. } => Effect::default(),
        }
    }

    /// Turn a URL into a request for the loop to spawn: the URL, plus whatever
    /// `Cookie:` header `cookies::header_for` says it may carry (M11.7).
    ///
    /// **`App`'s only cookie call site**, and every request it produces —
    /// document, stylesheet, script, image — comes through here. The jar is
    /// `Rc<RefCell<…>>` and so `!Send`; a worker could not hold it if it
    /// wanted to. This is where it becomes a plain string.
    ///
    /// The page the same-origin rule is measured against is `current_url`, and
    /// that is correct for a top-level navigation too, because `start_fetch`
    /// has already made the *target* the current URL by the time the `Effect`
    /// is built. A reader typing a URL is not making a cross-origin request,
    /// and a page's own cookies have to survive being navigated to from
    /// somewhere else.
    fn request(&self, url: String) -> net::Request {
        let page = self.current_url().unwrap_or_default();
        let cookie = js::cookies::header_for(&self.cookies, &page, &url, js::cookies::now());
        net::Request {
            url,
            cookie,
            method: net::Method::Get,
        }
    }

    fn cache_now(&self) -> Duration {
        self.cache_epoch.elapsed()
    }

    fn plan_document_request(
        &mut self,
        id: PageId,
        mut request: net::Request,
        mode: RequestMode,
    ) -> Effect {
        let Some(key) = CacheKey::from_request(&request) else {
            self.cache_key = None;
            return Effect {
                dirty: true,
                fetch: Some((id, request)),
                ..Effect::default()
            };
        };
        self.cache_key = Some(key.clone());
        match self.cache.plan(&key, mode, self.cache_now()) {
            Plan::Hit(response) => {
                self.document_source = net::DocumentSource::CacheHit;
                Effect {
                    dirty: true,
                    cached: Some((id, request.url, response, Duration::ZERO)),
                    ..Effect::default()
                }
            }
            Plan::Revalidate { etag } => {
                self.cache_candidate = etag.as_ref().map(|_| key);
                if mode == RequestMode::Reload || etag.is_some() {
                    request.method = net::Method::Conditional {
                        no_cache: mode == RequestMode::Reload,
                        if_none_match: etag,
                    };
                }
                Effect {
                    dirty: true,
                    fetch: Some((id, request)),
                    ..Effect::default()
                }
            }
            Plan::Miss => Effect {
                dirty: true,
                fetch: Some((id, request)),
                ..Effect::default()
            },
        }
    }

    /// Current page URL if one is known (loaded, loading, or failed).
    fn current_url(&self) -> Option<String> {
        match &self.fetch {
            Fetch::Idle => None,
            Fetch::Loading { url, .. } | Fetch::Loaded { url, .. } | Fetch::Failed { url, .. } => {
                Some(url.clone())
            }
        }
    }

    /// Start a navigation. When `push_history` is true and there is a current
    /// page, push it (with scroll) onto the back stack and clear forward.
    fn navigate(&mut self, url: String, push_history: bool) -> Effect {
        self.navigate_with(url, net::Method::Get, push_history)
    }

    /// Initial CLI navigation through the same cache planner as every later
    /// top-level GET. The empty session necessarily misses, but keeping one
    /// entry point prevents startup from becoming a policy exception.
    pub fn start_navigation(&mut self, url: String) -> Effect {
        self.navigate(url, false)
    }

    /// A navigation that may be a POST (M11.11). GET still goes through
    /// [`Self::navigate`]; this is the one path, with a method.
    fn navigate_with(&mut self, url: String, method: net::Method, push_history: bool) -> Effect {
        if let Some(cur) = self.current_url() {
            // Same document (a pure fragment change): no fetch, no new
            // generation — a scroll and a URL. Checked before `push_history`
            // rather than inside it, because `location.replace('#x')` is a
            // fragment jump that happens not to push, and the two callers must
            // reach the same place (M11.4).
            //
            // A fragment jump is a navigation that *has* a fragment. Going to
            // the same document **without** one is a reload (HTML), not a jump
            // to the top: it is what the URL bar does when the reader re-enters
            // the page's own URL, and what `location.reload()` has always been
            // spelled as. Both fall through to the fetch below.
            //
            // **POST never jumps.** A login form that posts to itself — or to
            // `#x` — is a new request with a body, not a scroll of a cached
            // display list.
            if matches!(method, net::Method::Get) && url.contains('#') && same_document(&cur, &url)
            {
                return self.jump_to_fragment(url, push_history);
            }
            if push_history {
                self.history.push(cur, self.viewport.offset());
            }
        }
        self.hover = None;
        self.clear_focus();
        self.hint = None;
        self.search = None;
        self.status_msg = None;
        let id = self.start_fetch(url.clone());
        if let Fetch::Loading { method: slot, .. } = &mut self.fetch {
            *slot = method.clone();
        }
        let mut request = self.request(url);
        request.method = method.clone();
        if matches!(method, net::Method::Post { .. }) {
            Effect {
                dirty: true,
                fetch: Some((id, request)),
                ..Effect::default()
            }
        } else {
            self.plan_document_request(id, request, RequestMode::Ordinary)
        }
    }

    /// A same-document move: the fragment changes, the document does not.
    ///
    /// This is a **scroll** (CLAUDE.md), and the whole point of the path: the
    /// cached display list is repainted at a new offset, nothing upstream of
    /// paint runs, and a citation click on Wikipedia therefore costs
    /// microseconds instead of the 43 ms a restyle of that page costs.
    fn jump_to_fragment(&mut self, url: String, push_history: bool) -> Effect {
        if push_history && let Some(cur) = self.current_url() {
            // The URL changed, so history records it, holding the offset the
            // reader is leaving — that is what makes `H` return them to the
            // paragraph they were reading rather than to the top of the page.
            // The `location.hash` binding has promised this since M10.11.
            self.history.push(cur, self.viewport.offset());
        }
        self.status_msg = None;
        // Hover, focus, hints and the search session all survive: the document
        // they point into is the same document. Only `navigate` proper, which
        // replaces the document, clears them.
        let fragment = fragment_of(&url).unwrap_or_default().to_string();
        self.set_current_url(url);
        self.scroll_to_fragment(&fragment);
        redraw()
    }

    /// Put the node a fragment names at the **top** of the viewport, clamped
    /// by the end of the document.
    ///
    /// Top, not centred, and unconditional rather than search's
    /// scroll-into-view: a fragment jump is a `G`-style move, and a reader who
    /// clicks a citation has to be able to tell that something happened.
    ///
    /// Two ways this does nothing, each a decision rather than a fallthrough:
    ///
    /// - no document laid out yet (an error page, or a body that has not
    ///   parsed) — there is nothing to scroll;
    /// - the fragment names nothing (`fragment::resolve` returns `None`): no
    ///   scroll, no error page, no console line. A link to an id a page
    ///   dropped years ago is not an error the reader can act on, and every
    ///   stale citation on the web would otherwise print a complaint.
    ///
    /// A target that was found but generated no box — `display: none`, a node
    /// in `<head>`, or an inline whose text merged into its neighbour's box —
    /// lands on the nearest laid-out ancestor rather than nowhere. The
    /// reasoning is in `layout::nearest_y`, and it is the third case that
    /// decides it: those anchors are visible on screen, and the layout tree
    /// cannot tell them from the hidden ones.
    fn scroll_to_fragment(&mut self, fragment: &str) {
        let (Some(dom), Some(tree)) = (&self.dom, &self.layout_tree) else {
            return;
        };
        let Some(target) = fragment::resolve(dom, fragment) else {
            return;
        };
        let y = match target {
            fragment::Target::Top => 0,
            fragment::Target::Node(node) => match layout::nearest_y(tree, dom, node) {
                Some(y) => y.max(0) as usize,
                None => return,
            },
        };
        let _ = self.viewport.scroll_to_offset(y);
    }

    /// Replace the URL this page is known by, without touching the fetch
    /// generation: no new `PageId`, no `Fetch` transition, so the sheets,
    /// images and scripts already in flight are not cancelled — the page did
    /// not reload. The `Fetch` variant is the only place a page URL lives
    /// (`current_url` reads it and nothing else), so it is the only place a
    /// fragment can be written.
    ///
    /// The URL it holds is also the base relative hrefs resolve against, and
    /// that stays correct: `net::resolve_url` joins through `reqwest::Url`,
    /// which drops a base's fragment exactly as the URL spec says — pinned by
    /// `a_fragment_in_the_base_never_reaches_the_resolved_url`.
    fn set_current_url(&mut self, url: String) {
        match &mut self.fetch {
            Fetch::Idle => {}
            Fetch::Loading { url: current, .. }
            | Fetch::Loaded { url: current, .. }
            | Fetch::Failed { url: current, .. } => *current = url,
        }
    }

    /// Navigate without pushing history, restoring `scroll` after layout of
    /// *this* fetch generation.
    fn navigate_restore(&mut self, url: String, scroll: usize) -> Effect {
        self.hover = None;
        self.clear_focus();
        self.hint = None;
        self.search = None;
        self.status_msg = None;
        let id = self.start_fetch(url.clone());
        // After `start_fetch`, deliberately: a restored entry's URL may carry a
        // fragment, and the offset the reader left is the more specific answer.
        self.pending_scroll = Some((id, PendingScroll::Offset(scroll)));
        let request = self.request(url);
        self.plan_document_request(id, request, RequestMode::Ordinary)
    }

    fn history_go(&mut self, back: bool) -> Effect {
        let Some(cur) = self.current_url() else {
            return Effect::default();
        };
        let scroll = self.viewport.offset();
        let entry = if back {
            self.history.go_back(cur.clone(), scroll)
        } else {
            self.history.go_forward(cur.clone(), scroll)
        };
        let Some(entry) = entry else {
            return Effect::default();
        };
        // A same-document entry must not fetch. `H` after a citation click
        // would otherwise refetch Wikipedia in order to arrive at the page
        // already on screen — 250 ms and a network round trip to go nowhere,
        // and every script on the page would run again.
        //
        // The check lives here rather than in `Entry`, which gains no field and
        // no second stack. "Same document" is a property of the *pair* — where
        // we are and where the entry points — not of the entry: the same entry
        // is a same-document restore when the reader is on that document and a
        // fetch when they are not (A → A#x → B → back → back exercises both,
        // off one entry each). Storing a flag would be storing an answer to a
        // question that is only asked later, and `same_document` is the same
        // predicate that decided to push the entry in the first place.
        if same_document(&cur, &entry.url) {
            self.set_current_url(entry.url);
            self.status_msg = None;
            // The entry's offset, not the fragment in its URL: the reader may
            // have scrolled after the jump, and where they *were* is what a
            // back button owes them.
            let _ = self.viewport.scroll_to_offset(entry.scroll);
            return redraw();
        }
        self.navigate_restore(entry.url, entry.scroll)
    }

    fn reload(&mut self) -> Effect {
        let Some(url) = self.current_url() else {
            return Effect::default();
        };
        // Reload is not a new history entry; keep scroll via pending restore
        // tied to the new generation. After `start_fetch` for the same reason
        // as `navigate_restore`: `r` on a page whose URL carries a fragment
        // owes the reader their own position, not a second jump to the anchor.
        let scroll = self.viewport.offset();
        self.hover = None;
        self.clear_focus();
        self.hint = None;
        let id = self.start_fetch(url.clone());
        self.pending_scroll = Some((id, PendingScroll::Offset(scroll)));
        let request = self.request(url);
        self.plan_document_request(id, request, RequestMode::Reload)
    }

    fn yank_page_url(&mut self) -> Effect {
        let Some(url) = self.current_url() else {
            return Effect::default();
        };
        self.status_msg = Some("yanked".into());
        Effect {
            dirty: true,
            yank: Some(url),
            ..Effect::default()
        }
    }

    fn start_hints(&mut self, yank: bool) -> Effect {
        // Labels only paint on the page surface; activating against F1–F3
        // would leave an invisible session that steals keys.
        if self.surface != Surface::Page {
            return Effect::default();
        }
        let (Some(dom), Some(tree)) = (&self.dom, &self.layout_tree) else {
            return Effect::default();
        };
        let visible = layout::visible_links_viewport(
            tree,
            dom,
            self.viewport.offset() as i32,
            self.page() as i32,
        );
        if visible.is_empty() {
            return Effect::default();
        }
        let labels = hints::label_links(&visible);
        self.hint = Some(HintSession {
            yank,
            buffer: String::new(),
            labels,
        });
        self.status_msg = None;
        redraw()
    }

    fn on_hint_key(&mut self, ev: &KeyEvent) -> Effect {
        use crossterm::event::KeyEventKind;
        if ev.kind != KeyEventKind::Press {
            return Effect::default();
        }
        // Quit always works, even while typing a label (PLAN.md §3).
        if let Resolution::Action(Action::Quit) = keys::resolve(keys::Mode::Browse, None, ev) {
            self.hint = None;
            return self.run(Action::Quit);
        }
        // Esc cancels — also bound as Cancel in the table, but hint mode
        // intercepts before resolve.
        if matches!(ev.code, KeyCode::Esc) {
            self.hint = None;
            return redraw();
        }
        if matches!(ev.code, KeyCode::Backspace) {
            if let Some(h) = &mut self.hint {
                h.buffer.pop();
            }
            return redraw();
        }
        let KeyCode::Char(c) = ev.code else {
            return Effect::default();
        };
        if ev
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Effect::default();
        }
        let c = c.to_ascii_lowercase();
        if !c.is_ascii_alphabetic() {
            return Effect::default();
        }
        let Some(session) = self.hint.as_mut() else {
            return Effect::default();
        };
        session.buffer.push(c);
        let buffer = session.buffer.clone();
        let matches: Vec<_> = hints::filter_prefix(&session.labels, &buffer)
            .into_iter()
            .cloned()
            .collect();
        if matches.is_empty() {
            // Invalid key: drop the bad character, keep the session (vimium-
            // style — a typo should not force the user to re-open `f`).
            session.buffer.pop();
            return redraw();
        }
        if let Some((_label, link)) = matches.iter().find(|(l, _)| l == &buffer) {
            let href = link.href.clone();
            let yank = session.yank;
            self.hint = None;
            return if yank {
                let url = self.resolve_href(&href).unwrap_or(href);
                self.status_msg = Some("yanked".into());
                Effect {
                    dirty: true,
                    yank: Some(url),
                    ..Effect::default()
                }
            } else if self.dispatch_click(link.node) {
                self.take_click_navigation()
            } else {
                self.follow_href(&href)
            };
        }
        // Partial match — keep filtering.
        redraw()
    }

    /// Drop the focus, and with it any typing it was carrying.
    ///
    /// One function because the two cannot come apart: a caret with nothing
    /// under it would be a statusline saying `[typing]` over a page that has no
    /// field. Every navigation goes through here, including the ones a script
    /// starts while the reader is mid-word.
    fn clear_focus(&mut self) {
        self.focus = None;
        self.field_edit_baseline = None;
        if matches!(self.mode, Mode::Field { .. } | Mode::Select { .. }) {
            self.mode = Mode::Browse;
        }
    }

    /// Repair Select-mode UI state after a script changes the DOM.
    fn validate_select_mode(&mut self) {
        let Mode::Select { select, cursor } = self.mode else {
            return;
        };
        let valid_select = self.dom.as_ref().is_some_and(|dom| {
            dom.is_connected(select)
                && matches!(&dom.node(select).data, NodeData::Element { tag, .. } if layout::field::is_select(tag))
                && dom.attr(select, "disabled").is_none()
        }) && self.layout_tree.as_ref().is_some_and(|tree| {
            tree.boxes
                .iter()
                .any(|b| b.node == Some(select) && matches!(b.kind, BoxKind::Field(_)))
        });
        if !valid_select || self.focus != Some(select) {
            self.clear_focus();
            return;
        }
        let enabled = self
            .dom
            .as_ref()
            .map(|dom| {
                layout::field::options(dom, select)
                    .into_iter()
                    .filter(|option| !option.disabled)
                    .map(|option| option.node)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if enabled.contains(&cursor) {
            return;
        }
        match enabled.first().copied() {
            Some(cursor) => self.mode = Mode::Select { select, cursor },
            None => self.clear_focus(),
        }
    }

    fn cycle_focus(&mut self, dir: i32) -> Effect {
        // `Tab` out of a field leaves typing (see the table's Field rows): the
        // reader lands on the next focusable with every browse binding live,
        // whatever kind of thing it turned out to be.
        let Some(dom) = &self.dom else {
            return Effect::default();
        };
        // Links **and form controls** since M11.8, in document order, and only
        // what the reader can see (M9.3): Tab must not park focus on something
        // an `overflow` clip removed from the page, because `Enter` would then
        // follow a link that is not there. Same predicate the hint labels use.
        // Before layout has run there is no clip to ask about, so the DOM
        // order is all there is.
        let links: Vec<NodeId> = match &self.layout_tree {
            Some(tree) => layout::focusables(tree, dom),
            None => layout::dom_focusables(dom),
        };
        if links.is_empty() {
            return Effect::default();
        }
        let next = match self.focus {
            None => {
                if dir >= 0 {
                    0
                } else {
                    links.len() - 1
                }
            }
            Some(cur) => {
                let idx = links.iter().position(|n| *n == cur).unwrap_or(0);
                if dir >= 0 {
                    (idx + 1) % links.len()
                } else {
                    (idx + links.len() - 1) % links.len()
                }
            }
        };
        self.transition_focus(links[next])
    }

    /// Move reader focus through the DOM event order. This is deliberately one
    /// action even when a pending text edit contributes `change`: listeners
    /// see the synchronous state transitions, while layout waits until all of
    /// them have returned.
    fn transition_focus(&mut self, next: NodeId) -> Effect {
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            self.focus = Some(next);
            self.mode = Mode::Browse;
            self.scroll_focus_into_view();
            return redraw();
        };
        self.transition_focus_in_action(next);
        self.finish_reader_action(id, before, logged_before);
        self.scroll_focus_into_view();
        self.take_click_navigation()
    }

    /// Focus-only portion of a reader action. It deliberately consumes no
    /// DOM version, host queue or navigation request; callers can continue
    /// with click/default-action work before finishing the transaction.
    fn transition_focus_in_action(&mut self, next: NodeId) -> bool {
        let old = self.focus;
        if old == Some(next) {
            return true;
        }
        if let Some((field, baseline)) = self.field_edit_baseline.take()
            && old == Some(field)
            && let Some(dom) = self.dom.as_ref()
            && let NodeData::Element { tag, .. } = &dom.node(field).data
            && layout::field::value(dom, field, tag) != baseline
        {
            self.dispatch_reader_event(field, js::EventDescriptor::CHANGE);
        }
        if let Some(old) = old {
            self.dispatch_reader_event(old, js::EventDescriptor::BLUR);
        }
        // A blur listener can remove or disable the candidate. Do not hunt
        // for a replacement in this action: the old target was genuinely
        // blurred, and focus now has nowhere safe to land.
        if !self
            .dom
            .as_ref()
            .is_some_and(|dom| layout::dom_focusables(dom).contains(&next))
        {
            self.clear_focus();
            return false;
        }
        self.focus = Some(next);
        self.mode = Mode::Browse;
        self.dispatch_reader_event(next, js::EventDescriptor::FOCUS);
        // `focus` itself can invalidate its target. This is a silent safety
        // repair, not a second focus transition and therefore emits no blur.
        if !self
            .dom
            .as_ref()
            .is_some_and(|dom| layout::dom_focusables(dom).contains(&next))
        {
            self.clear_focus();
        }
        self.focus == Some(next)
    }

    fn follow_focus(&mut self) -> Effect {
        // `Enter` means "activate the focused thing", and activating a text box
        // is beginning to type in it (M11.9). A button, a `disabled` field and
        // anything else that takes no caret is not one, so it falls through to
        // the link path exactly as it did before — which for a control means
        // doing nothing, because a control has no `href`.
        //
        // The caret starts at the **end** of the value: `Enter` is not a click,
        // it names no position in the text, and the end is where a reader who
        // just opened a half-filled field wants to carry on.
        if let Some((_, value)) = self.focused_field() {
            self.mode = Mode::Field {
                caret: value.chars().count(),
            };
            return redraw();
        }
        let (Some(focus), Some(dom)) = (self.focus, &self.dom) else {
            return Effect::default();
        };
        let NodeData::Element { tag, .. } = &dom.node(focus).data else {
            return Effect::default();
        };
        if layout::field::is_checkbox(dom, focus, tag) || layout::field::is_radio(dom, focus, tag) {
            return self.activate_choice(focus);
        }
        if layout::field::is_select(tag) {
            return self.start_select(focus);
        }
        // A submit button — Wikipedia's `<button>` with no `type` above all —
        // is the other thing `Enter` can activate, and until M11.10 it fell
        // through to the `href` path and did nothing at all, because a button
        // has no `href`.
        if form::is_submit_button(dom, focus) {
            return self.activate_submit(focus);
        }
        let Some(href) = dom.attr(focus, "href") else {
            return Effect::default();
        };
        let href = href.to_string();
        if self.dispatch_click(focus) {
            return self.take_click_navigation();
        }
        self.follow_href(&href)
    }

    /// The submit-button default is intentionally after its cancelable click.
    /// Re-read the node afterwards: a listener may have disabled, detached or
    /// retagged the button before its native action gets a turn.
    fn activate_submit(&mut self, node: NodeId) -> Effect {
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return Effect::default();
        };
        let prevented = self.dispatch_reader_event(node, js::EventDescriptor::CLICK);
        let still_submits = self.dom.as_ref().is_some_and(|dom| {
            dom.is_connected(node)
                && dom.attr(node, "disabled").is_none()
                && form::is_submit_button(dom, node)
        });
        let submission = if !prevented && still_submits {
            self.submit_form_in_action(node)
        } else {
            None
        };
        self.finish_reader_action(id, before, logged_before);
        self.resolve_submission(submission)
    }

    /// Submit the form `activator` is in — the one function all three ways of
    /// activating a submission reach: `Enter` while typing, `Enter` on a
    /// focused submit button, and a click on one. One function so that a click
    /// and a key cannot come to disagree about what a submission is.
    ///
    /// **A submission is a navigation, and there is only one of those.**
    /// `navigate` already owns everything one needs — history so `H` comes
    /// back, a fresh `PageId` so a stale response cannot land on the wrong
    /// page, `App::request` so the jar attaches the cookies (M11.7), and
    /// redirects through the event loop (M11.7a) so the `Set-Cookie` on the 302
    /// a search or a login answers with is not lost. Nothing here fetches, and
    /// there is no second navigation path to keep in step with the first.
    ///
    /// Three ways this does nothing, each a decision:
    ///
    /// - the control is in no `<form>`: no navigation, no stage, no message.
    ///   A search box outside a form is real — sites submit those with
    ///   JavaScript — and M11.13 is where those start working;
    /// - the form's method is one this engine cannot send. Then the reader is
    ///   *told*, in the statusline's flash segment where "yanked" lives,
    ///   because a key that silently does nothing is indistinguishable from a
    ///   broken keyboard;
    /// - the action does not resolve — a page whose `action` is not a URL.
    ///
    fn submit_form(&mut self, activator: NodeId) -> Effect {
        // There is no native submit event without an owning form. Besides
        // avoiding needless host work, this preserves the field mode exactly:
        // Enter is a no-op for script-owned search boxes outside a form.
        if self
            .dom
            .as_ref()
            .and_then(|dom| form::owner(dom, activator))
            .is_none()
        {
            return Effect::default();
        }
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return Effect::default();
        };
        let submission = self.submit_form_in_action(activator);
        self.finish_reader_action(id, before, logged_before);
        self.resolve_submission(submission)
    }

    /// Submit dispatch plus post-listener serialization, kept inside the
    /// caller's reader action so a submit button's click and submit cannot
    /// create two invalidation cycles.
    fn submit_form_in_action(&mut self, activator: NodeId) -> Option<form::Submit> {
        let form = self
            .dom
            .as_ref()
            .and_then(|dom| form::owner(dom, activator))?;
        // Field-mode Enter commits the reader's pending text edit before the
        // form observes submission. The baseline advances even if `submit` is
        // canceled, so a later Tab cannot report the same commit twice.
        if let Some((field, baseline)) = self.field_edit_baseline.take()
            && field == activator
            && let Some(dom) = self.dom.as_ref()
            && let NodeData::Element { tag, .. } = &dom.node(field).data
            && layout::field::value(dom, field, tag) != baseline
        {
            self.dispatch_reader_event(field, js::EventDescriptor::CHANGE);
        }
        let prevented = self.dispatch_reader_event(form, js::EventDescriptor::SUBMIT);
        if prevented || self.dom.as_ref().is_none_or(|dom| !dom.is_connected(form)) {
            return None;
        }
        let (Some(dom), Some(base)) = (self.dom.as_ref(), self.current_url()) else {
            return None;
        };
        form::submit(dom, &base, activator)
    }

    /// Resolve the native default only after its action's event sequence has
    /// finished. A missing candidate leaves an independently queued script
    /// navigation available.
    fn resolve_submission(&mut self, submission: Option<form::Submit>) -> Effect {
        match submission {
            None => self.take_click_navigation(),
            Some(form::Submit::Unsupported(method)) => {
                // Words for somebody who has never read PLAN.md — and short
                // ones, because this segment is what is left of the row after
                // the URL and the scroll percentage have taken theirs (twenty
                // cells on an 80-column terminal, where "yanked" lives).
                self.status_msg = Some(match method.as_str() {
                    "FILE" => "can't send files".into(),
                    "huge" => "form is too large".into(),
                    other => format!("can't send {other} forms"),
                });
                let mut effect = self.take_click_navigation();
                effect.dirty = true;
                effect
            }
            // Typing ends here, without being asked to: `navigate` clears the
            // focus, and `clear_focus` drops the mode with it. So the browse
            // bindings are live while the new page is still loading — `q`
            // quits, and the reader is not stuck typing into a field that is
            // about to be replaced.
            Some(form::Submit::Get(url)) => {
                let effect = self.navigate(url, true);
                self.with_reader_side_effects(effect)
            }
            Some(form::Submit::Post { url, body }) => {
                let effect = self.navigate_with(url, net::Method::Post { body }, true);
                self.with_reader_side_effects(effect)
            }
        }
    }

    /// The focused control if it is one a reader may type into, with the value
    /// they would be typing into: the state beside the tree if they have
    /// already, else what the markup said.
    ///
    /// `None` for a link, a button, a `disabled` field and a page with nothing
    /// focused — which is what makes `Enter` on a disabled control open no mode
    /// (it cannot be focused either, since M11.8 left it out of the Tab cycle,
    /// but "cannot be reached" and "cannot be typed into" are different claims
    /// and this is the one that matters here).
    fn focused_field(&self) -> Option<(NodeId, String)> {
        let (focus, dom) = (self.focus?, self.dom.as_ref()?);
        let crate::dom::NodeData::Element { tag, .. } = &dom.node(focus).data else {
            return None;
        };
        Some((focus, layout::field::editable_value(dom, focus, tag)?))
    }

    fn activate_choice(&mut self, node: NodeId) -> Effect {
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return Effect::default();
        };
        let changed = self.activate_choice_in_action(node);
        let listener_changed = self.finish_reader_action(id, before, logged_before);
        if changed || listener_changed {
            self.take_click_navigation()
        } else {
            Effect::default()
        }
    }

    /// Checkbox/radio pre-activation and its events, without consuming the
    /// surrounding reader action.
    fn activate_choice_in_action(&mut self, node: NodeId) -> bool {
        let Some(dom) = self.dom.as_ref() else {
            return false;
        };
        let NodeData::Element { tag, .. } = &dom.node(node).data else {
            return false;
        };
        let checkbox = layout::field::is_checkbox(dom, node, tag);
        let radio = layout::field::is_radio(dom, node, tag);
        if !checkbox && !radio {
            return false;
        }
        if dom.attr(node, "disabled").is_some() {
            return false;
        }
        let current = layout::field::checked(dom, node, tag);
        let peers = if radio {
            layout::field::radio_group(dom, node)
        } else {
            vec![node]
        };
        let snapshot = dom.choice_state_snapshot(&peers);
        let states = peers
            .iter()
            .map(|&peer| (peer, if radio { peer == node } else { !current }))
            .collect::<Vec<_>>();
        let Some(dom) = self.dom.as_mut() else {
            return false;
        };
        let changed = dom.set_choice_group(&states, "checked");
        let prevented = self.dispatch_reader_event(node, js::EventDescriptor::CLICK);
        if prevented {
            if let Some(dom) = self.dom.as_mut() {
                dom.restore_choice_state_snapshot(&snapshot);
            }
            false
        } else if changed {
            self.dispatch_reader_event(node, js::EventDescriptor::INPUT);
            self.dispatch_reader_event(node, js::EventDescriptor::CHANGE);
            true
        } else {
            false
        }
    }

    fn start_select(&mut self, node: NodeId) -> Effect {
        let Some(dom) = self.dom.as_ref() else {
            return Effect::default();
        };
        if dom.attr(node, "disabled").is_some() {
            return Effect::default();
        }
        let options = layout::field::options(dom, node);
        let selected = layout::field::selected_options(dom, node, &options);
        let cursor = selected
            .iter()
            .find(|id| options.iter().any(|o| o.node == **id && !o.disabled))
            .copied()
            .or_else(|| options.iter().find(|o| !o.disabled).map(|o| o.node));
        let Some(cursor) = cursor else {
            return Effect::default();
        };
        self.mode = Mode::Select {
            select: node,
            cursor,
        };
        redraw()
    }

    fn move_select(&mut self, direction: i32, edge: bool) -> Effect {
        let Mode::Select { select, cursor } = self.mode else {
            return Effect::default();
        };
        let Some(dom) = self.dom.as_ref() else {
            return Effect::default();
        };
        let enabled = layout::field::options(dom, select)
            .into_iter()
            .filter(|option| !option.disabled)
            .map(|option| option.node)
            .collect::<Vec<_>>();
        let Some(at) = enabled.iter().position(|id| *id == cursor) else {
            return Effect::default();
        };
        let next = if edge {
            if direction < 0 { enabled.len() - 1 } else { 0 }
        } else if direction < 0 {
            at.saturating_sub(1)
        } else {
            (at + 1).min(enabled.len() - 1)
        };
        if next == at {
            return Effect::default();
        }
        self.mode = Mode::Select {
            select,
            cursor: enabled[next],
        };
        redraw()
    }

    fn toggle_select_option(&mut self) -> Effect {
        let (Mode::Select { select, cursor }, Some(dom)) = (&self.mode, self.dom.as_ref()) else {
            return Effect::default();
        };
        if dom.attr(*select, "multiple").is_none() {
            return Effect::default();
        }
        let options = layout::field::options(dom, *select);
        let selected = layout::field::selected_options(dom, *select, &options);
        let value = !selected.contains(cursor);
        let cursor = *cursor;
        let select = *select;
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return Effect::default();
        };
        let changed = self
            .dom
            .as_mut()
            .expect("DOM checked above")
            .set_choice_state(cursor, "selected", value);
        if changed {
            self.dispatch_reader_event(select, js::EventDescriptor::INPUT);
            self.dispatch_reader_event(select, js::EventDescriptor::CHANGE);
        }
        self.finish_reader_action(id, before, logged_before);
        self.take_click_navigation()
    }

    fn commit_select(&mut self) -> Effect {
        let (Mode::Select { select, cursor }, Some(dom)) = (&self.mode, self.dom.as_ref()) else {
            return Effect::default();
        };
        if dom.attr(*select, "multiple").is_some() {
            self.mode = Mode::Browse;
            return redraw();
        }
        let options = layout::field::options(dom, *select);
        let cursor = *cursor;
        let select = *select;
        if layout::field::selected_options(dom, select, &options) == [cursor] {
            self.mode = Mode::Browse;
            return redraw();
        }
        let states = options
            .into_iter()
            .map(|option| (option.node, option.node == cursor))
            .collect::<Vec<_>>();
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            self.mode = Mode::Browse;
            return redraw();
        };
        let changed = self
            .dom
            .as_mut()
            .expect("DOM checked above")
            .set_choice_group(&states, "selected");
        self.mode = Mode::Browse;
        if changed {
            self.dispatch_reader_event(select, js::EventDescriptor::INPUT);
            self.dispatch_reader_event(select, js::EventDescriptor::CHANGE);
        }
        self.finish_reader_action(id, before, logged_before);
        self.take_click_navigation()
    }

    /// Insert a character at the caret — the sanctioned printable-key path's
    /// field arm, and the first production writer `Dom::set_field_value` has
    /// ever had.
    fn insert_in_field(&mut self, ch: char) -> Effect {
        let (Some((node, mut value)), Some(caret)) = (self.focused_field(), self.caret()) else {
            return Effect::default();
        };
        value.insert(byte_at(&value, caret), ch);
        self.mode = Mode::Field { caret: caret + 1 };
        self.write_field(node, &value)
    }

    /// `Backspace` (before the caret) and `Delete` (after it). A caret with
    /// nothing on the side it was asked about is not a change, and not a
    /// redraw either.
    fn delete_in_field(&mut self, forward: bool) -> Effect {
        let (Some((node, mut value)), Some(caret)) = (self.focused_field(), self.caret()) else {
            return Effect::default();
        };
        let target = match forward {
            true => caret,
            false => match caret.checked_sub(1) {
                Some(before) => before,
                None => return Effect::default(),
            },
        };
        if target >= value.chars().count() {
            return Effect::default();
        }
        value.remove(byte_at(&value, target));
        self.mode = Mode::Field { caret: target };
        self.write_field(node, &value)
    }

    /// `←` `→` `Home` `End`. The caret is chrome, like focus and hover: moving
    /// it changes no value, so no stage runs and the overlay draws it in its
    /// new cell on the next frame.
    fn move_caret(&mut self, how: CaretMove) -> Effect {
        let (Some((_, value)), Some(caret)) = (self.focused_field(), self.caret()) else {
            return Effect::default();
        };
        let (start, end) = line_bounds(&value, caret);
        let next = match how {
            CaretMove::Prev => caret.saturating_sub(1),
            CaretMove::Next => (caret + 1).min(value.chars().count()),
            CaretMove::LineStart => start,
            CaretMove::LineEnd => end,
        };
        if next == caret {
            return Effect::default();
        }
        self.mode = Mode::Field { caret: next };
        redraw()
    }

    /// The caret, clamped to the value it indexes.
    ///
    /// Clamped rather than trusted because the value can move underneath it:
    /// a script that writes a `<textarea>`'s content while the reader is in it
    /// leaves the caret past the end, and an index into a string is the last
    /// place to find out.
    fn caret(&self) -> Option<usize> {
        let Mode::Field { caret } = self.mode else {
            return None;
        };
        let (_, value) = self.focused_field()?;
        Some(caret.min(value.chars().count()))
    }

    /// Store what the reader typed and put it on screen.
    ///
    /// **The value is state, not an attribute** (M11.8's rule, and this is what
    /// depends on it): `set_field_value` leaves `value="x"` exactly as the page
    /// wrote it, so `input[value="x"]` keeps matching the markup and a
    /// keystroke never reaches the cascade. It is a *text* edit, though, so the
    /// screen has to follow — see [`Self::after_field_edit`] for what that
    /// costs and why it is not a relayout.
    ///
    fn write_field(&mut self, node: NodeId, value: &str) -> Effect {
        if self.field_edit_baseline.is_none()
            && let Some(dom) = self.dom.as_ref()
            && let NodeData::Element { tag, .. } = &dom.node(node).data
        {
            self.field_edit_baseline = Some((node, layout::field::value(dom, node, tag)));
        }
        if self.js_host.is_none() {
            let Some(dom) = self.dom.as_mut() else {
                return Effect::default();
            };
            let changed = dom.set_field_value(node, value);
            return if changed {
                self.after_field_edit(node)
            } else {
                Effect::default()
            };
        }
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return Effect::default();
        };
        let Some(dom) = self.dom.as_mut() else {
            return Effect::default();
        };
        if !dom.set_field_value(node, value) {
            return Effect::default();
        }
        // The state write precedes dispatch, so the listener observes the
        // character or deletion that caused this event. No keyboard event is
        // synthesized: Yata's key table still owns the key itself.
        self.dispatch_reader_event(node, js::EventDescriptor::INPUT);
        self.finish_reader_action(id, before, logged_before);
        self.take_click_navigation()
    }

    /// What a keystroke costs (M11.9 deliverable 4), and the shape of it is
    /// the point: **relayout, then repaint. No restyle.**
    ///
    /// A keystroke is not a cascade event — the value is state beside the tree
    /// and not an attribute, so no selector's answer can have moved — and
    /// `styles_run` says so in a test.
    ///
    /// It is a *text* edit, so the page is laid out again, and that is the
    /// simple thing rather than the fast thing. It was measured before it was
    /// written, against the narrowing it would obviously want (patch the one
    /// box whose text changed, since a control's box is `size` cells wide
    /// whatever it holds, so no geometry can move):
    ///
    /// ```text
    /// keypress→screen, en.wikipedia.org, 80×24, budget < 10 ms
    ///   relayout + repaint   7.55 ms (7.37–7.89)
    ///   patch + repaint      2.01 ms (1.98–2.09)
    /// ```
    ///
    /// The simple path is **inside the budget with a quarter of it to spare**,
    /// so it ships: a second invalidation path costs a correctness obligation
    /// (the patched tree has to be the tree a relayout would have produced) on
    /// every future change to how a control is built, and 5 ms nobody is
    /// waiting for does not buy that. M12's incremental layout is where this
    /// gets cheaper for every page and not just for fields.
    ///
    /// The narrowing stays as the measurement's B side (see
    /// [`Self::repaint_field`]) so the number can be taken again on a machine
    /// that disagrees, or when a page bigger than Wikipedia's article says the
    /// budget has moved.
    fn after_field_edit(&mut self, node: NodeId) -> Effect {
        // The A/B switch, and the only thing that reads it: the two paths have
        // to be timed in the same process, on the same page, interleaved (this
        // machine drifts 5–10%, so a before-commit/after-commit pair would be
        // measuring the drift).
        #[cfg(test)]
        if self.narrow_keystroke && self.repaint_field(node) {
            return redraw();
        }
        #[cfg(not(test))]
        let _ = node;
        self.relayout();
        redraw()
    }

    /// Push a control's new text into the live layout tree and repaint from it,
    /// without laying the page out — the path
    /// [`Self::after_field_edit`]'s numbers rejected, kept as the B side that
    /// produced them. `false` when there is no such box.
    ///
    /// It has to be a *correct* alternative or the number it produces means
    /// nothing, so: **the patched tree is the tree a fresh layout would
    /// produce**, and
    /// `a_patched_tree_is_the_tree_a_fresh_layout_would_have_built` compares
    /// the two rather than asserting it. That holds because everything layout
    /// derives for a control's box comes from either its attributes and the
    /// cascade (`size`, `cols`, `rows`, `width`) or from the value — and the
    /// value reaches only `text` and `FieldPaint`, both rewritten here from the
    /// same `field::control` that layout calls. Nothing sizes a text field by
    /// what is in it, which is why a button — whose box *is* its label's width
    /// — is not patchable and never needs to be.
    #[cfg(test)]
    fn repaint_field(&mut self, node: NodeId) -> bool {
        let (Some(dom), Some(tree)) = (self.dom.as_ref(), self.layout_tree.as_mut()) else {
            return false;
        };
        let crate::dom::NodeData::Element { tag, .. } = &dom.node(node).data else {
            return false;
        };
        let Some(control) = layout::field::control(dom, node, tag) else {
            return false;
        };
        let mut patched = false;
        for b in &mut tree.boxes {
            if b.node != Some(node) || !matches!(b.kind, BoxKind::Field(_)) {
                continue;
            }
            b.kind = BoxKind::Field(layout::FieldPaint {
                shows: control.shows,
                disabled: control.disabled,
            });
            b.text = Some(control.text.clone());
            patched = true;
        }
        if !patched {
            return false;
        }
        let pixels = self.images.pixels();
        if let Some(tree) = &self.layout_tree {
            self.display_list = paint::paint_with(tree, &pixels);
            // The rasterised lines come from the same tree, and go stale the
            // same way: they are what `--dump-text` prints and what the scroll
            // range is counted from. The row count cannot have changed — no
            // geometry moved — but one row's text did.
            let lines = layout::lines_from_tree(tree);
            self.viewport.set_lines(lines, self.page());
            #[cfg(test)]
            {
                self.paints += 1;
            }
        }
        // F3 names what a control is showing, and typing into an empty field
        // turns a placeholder into a value.
        self.boxes_view_built = false;
        self.build_visible_inspector();
        true
    }

    fn scroll_focus_into_view(&mut self) {
        let (Some(focus), Some(tree)) = (self.focus, &self.layout_tree) else {
            return;
        };
        let Some(y) = layout::first_y(tree, focus) else {
            return;
        };
        let y = y as usize;
        let page = self.page() as usize;
        let off = self.viewport.offset();
        if y < off {
            let _ = self.viewport.scroll_to_offset(y);
        } else if y >= off + page {
            let _ = self
                .viewport
                .scroll_to_offset(y.saturating_sub(page.saturating_sub(1)));
        }
    }

    /// Run whatever the queue can run now, and fold the result into one
    /// `Effect`. Called by the script pass and by every arriving body, so a
    /// prefix that completes late goes through exactly the same path as the
    /// first one.
    fn run_ready_scripts(&mut self, id: PageId, mut dom: Dom) -> Effect {
        // Taken before anything runs, so that an `error` this turn *discovers*
        // is fired by the next one rather than inside this one (M11.5). One
        // per turn: a handler costs a budget, and the reader's keys are served
        // between turns.
        let owed = match self.owed_script_errors.is_empty() {
            true => None,
            false => Some(self.owed_script_errors.remove(0)),
        };
        let ready = self.script_queue.take_ready_prefix();
        // Once per page (M11.5): an inserted script can finish a queue that
        // had already finished, and `DOMContentLoaded`/`load` are not events
        // a page may see twice.
        let finished = self.script_queue.take_finished();

        let started = Instant::now();
        let before = dom.change_versions();
        let logged_before = self.console.entries().len();
        let url = self.current_url().unwrap_or_default();
        let _runs = js::run_prefix(
            &mut self.js_host,
            &mut dom,
            &js::PageContext {
                page: id.generation,
                url: &url,
                console: &self.console,
                storage: &self.storage,
                cookies: &self.cookies,
            },
            ready,
            finished,
        );
        let after = dom.change_versions();
        // The DOM comes straight back: the host borrowed it for the tick and
        // holds nothing now.
        self.dom = Some(dom);

        // **Accumulated**, not replaced: a page's script time is the sum of
        // every prefix that ran, so `F4` shows what the page cost rather than
        // what its last arriving script cost.
        let elapsed = started.elapsed();
        self.timings.script = Some(self.timings.script.unwrap_or_default() + elapsed);

        let mut effect = self.apply_dom_changes(before, after);
        effect.timers = self.take_timer_requests(id);
        effect.fetches = self.take_fetch_requests(id);
        self.adopt_inserted_scripts(id, &mut effect);
        if let Some(node) = owed {
            self.fire_script_event(id, node, js::EventDescriptor::ERROR, &mut effect);
        }
        // Whatever is left over asks for the turn it will be done in: one more
        // inserted body to run, or one more `error` to fire — including the
        // ones the dispatch just above owed us.
        if self.script_queue.has_ready_insertion() || !self.owed_script_errors.is_empty() {
            effect.run_scripts = Some(id);
        }
        self.apply_script_navigation(&mut effect);
        if self.console.entries().len() != logged_before {
            // A script that only logged changed no box, but it did change what
            // the console pane holds and what the statusline says about the
            // page — so the frame is stale even though the pipeline had
            // nothing to do.
            self.console_view_built = false;
            self.build_visible_inspector();
            effect.dirty = true;
        }
        effect
    }

    /// The `<script>` elements the tick that just ended put into the document
    /// (M11.5), folded into `effect`.
    ///
    /// The bindings recorded *candidates* — "this node was inserted", or "a
    /// `src` was written on it". Everything that decides whether one of them
    /// runs happens here, against the tree `App` owns:
    ///
    /// - `sources::connected_script` answers whether it is a `<script>` at
    ///   all, whether it is connected to the document, and whether it sits
    ///   inside a `<template>`/`<noscript>` — the same three answers the
    ///   parsed walk gives, from the same code, so the two paths cannot
    ///   disagree;
    /// - `ScriptQueue::insert` answers where it goes and whether it has
    ///   already run.
    ///
    /// Nothing runs *here*. An inline one becomes a ready slot and the loop is
    /// asked for another turn (`Effect::run_scripts`); an external one becomes
    /// a fetch on a worker through the same `Effect::scripts` a document-order
    /// `<script src>` uses. Re-entering the engine from inside the tick that
    /// inserted the script is exactly the `document.write` re-entrancy bug
    /// M10.2's model exists to make impossible.
    fn adopt_inserted_scripts(&mut self, id: PageId, effect: &mut Effect) {
        // The A side of the interleaved measurement, and its only reader.
        #[cfg(test)]
        if self.no_insert_detection {
            return;
        }
        let candidates = self
            .js_host
            .as_ref()
            .map(js::Host::take_script_inserts)
            .unwrap_or_default();
        if candidates.is_empty() {
            return;
        }
        let Some(dom) = self.dom.as_ref() else {
            return;
        };

        // The tree is read **as the tick left it**, not as it was at the
        // moment of each call. So a page that appends a script and removes it
        // again before the tick ends has inserted nothing, which is what the
        // reader sees too — and is the safer of the two readings, since the
        // page changed its mind before anything could have observed it.
        //
        // Elements whose script will never arrive, so their `error` can be
        // fired once the borrow of the tree is over.
        let mut failed = Vec::new();
        let mut ready = false;
        // The descriptions first, while the tree is borrowed once; the queue
        // and the console are `&mut self` from here on.
        //
        // Numbered by the ones that survive, not by the candidates: a `src`
        // written on a script the page never connects is a candidate and not a
        // script, and a name that skipped a number would be a name that lies
        // about how many scripts the page inserted. The document walk numbers
        // its inline scripts the same way, by the ones that can run.
        let mut surviving = 0;
        let described: Vec<(NodeId, js::sources::Script)> = candidates
            .into_iter()
            .filter_map(|candidate| {
                let node = NodeId(candidate);
                let name = format!("inserted#{}", self.script_queue.inserted() + surviving + 1);
                let script = js::sources::connected_script(dom, node, &name)?;
                surviving += 1;
                Some((node, script))
            })
            .collect();

        for (node, script) in described {
            match self.script_queue.insert(node, script, &self.console) {
                js::queue::Inserted::Ready => ready = true,
                js::queue::Inserted::Fetch(external) => {
                    // The same resolution every other external script gets, so
                    // a dynamic one inherits the `PageId` guard, the
                    // `MAX_SCRIPT_BYTES` cap and the settles-empty behaviour.
                    // An empty answer means the URL will not resolve and the
                    // slot has already settled — which is an `error` for the
                    // element, not a slot that waits forever.
                    let resolved = self.resolve_script_urls(id, vec![external]);
                    match resolved.is_empty() {
                        true => failed.push(node),
                        false => effect.scripts.extend(resolved),
                    }
                }
                js::queue::Inserted::Nothing => {}
            }
        }

        // Only ask for a turn when something can actually happen in it. An
        // insertion that is still fetching is unblocked by its own
        // `Msg::Script`, which goes through `run_ready_scripts` anyway.
        if ready || !failed.is_empty() {
            effect.run_scripts = Some(id);
        }
        // **Owed, not fired.** An `error` handler is a script and costs a
        // budget, so firing every owed one here would put as many budgets in
        // this turn as the page cared to owe itself — and a handler that
        // inserts the next unresolvable script would re-enter this function
        // from inside the dispatch it caused, nesting turns' worth of work
        // into one `update` with the loop nowhere near `recv`. They are fired
        // one per turn by `run_ready_scripts`, which is where an inserted
        // script's body runs too, and for the same reason.
        self.owed_script_errors.extend(failed);
    }

    /// Fire `load` or `error` at an inserted `<script>` element (M11.5
    /// deliverable 8), as its own tick, and fold what its handler did into
    /// `effect`.
    ///
    /// Only inserted scripts get this, and only external ones: a browser fires
    /// neither event for an inline script, and nothing can have registered a
    /// listener on a parsed `<script src>` before the page ran a line.
    fn fire_script_event(
        &mut self,
        id: PageId,
        node: NodeId,
        event: js::EventDescriptor,
        effect: &mut Effect,
    ) {
        if self.js_host.is_none() {
            return;
        }
        let Some(mut dom) = self.dom.take() else {
            return;
        };
        let before = dom.change_versions();
        let logged_before = self.console.entries().len();
        let url = self.current_url().unwrap_or_default();
        js::dispatch(
            &mut self.js_host,
            &mut dom,
            &js::PageContext {
                page: id.generation,
                url: &url,
                console: &self.console,
                storage: &self.storage,
                cookies: &self.cookies,
            },
            js::Target::Node(node.0),
            event,
        );
        let after = dom.change_versions();
        self.dom = Some(dom);

        effect.dirty |= self.apply_dom_changes(before, after).dirty;
        effect.timers.extend(self.take_timer_requests(id));
        effect.fetches.extend(self.take_fetch_requests(id));
        // A `load` handler that inserts the *next* script in a chain is the
        // whole reason the event exists, so its own insertions are adopted.
        self.adopt_inserted_scripts(id, effect);
        self.apply_script_navigation(effect);
        if self.console.entries().len() != logged_before {
            self.console_view_built = false;
            self.build_visible_inspector();
            effect.dirty = true;
        }
    }

    /// Resolve each external script's `src` against the page URL, dropping the
    /// ones that will not resolve — a slot whose URL is unusable settles empty
    /// rather than waiting forever.
    fn resolve_script_urls(
        &mut self,
        id: PageId,
        externals: Vec<crate::js::queue::External>,
    ) -> Vec<(PageId, usize, net::Request)> {
        let base = self.current_url();
        let mut out = Vec::new();
        for external in externals {
            match base
                .as_deref()
                .and_then(|base| net::resolve_url(base, &external.url))
            {
                // The jar's answer, on the same terms every other request
                // gets: a script from another origin — the ladder's
                // `www.google-analytics.com` — carries nothing.
                Some(url) => out.push((id, external.slot, self.request(url))),
                None => {
                    self.console.push(
                        crate::js::console::Level::Warn,
                        Some(external.url.clone()),
                        None,
                        "could not resolve this script's URL",
                    );
                    self.script_queue.fill(external.slot, None);
                }
            }
        }
        out
    }

    /// The `Effect` for a click whose handler cancelled the default action:
    /// a redraw, plus the navigation the handler asked for if it asked for one.
    fn take_click_navigation(&mut self) -> Effect {
        let mut effect = Effect {
            dirty: true,
            fetches: std::mem::take(&mut self.pending_click_fetches),
            timers: std::mem::take(&mut self.pending_click_timers),
            scripts: std::mem::take(&mut self.pending_click_scripts),
            run_scripts: self.pending_click_run.take(),
            ..Effect::default()
        };
        self.apply_script_navigation(&mut effect);
        effect
    }

    /// A native default navigation wins over a script navigation requested by
    /// one of its listeners, but those listeners' fetches and inserted scripts
    /// remain ordinary side effects of the same reader action.
    fn with_reader_side_effects(&mut self, mut effect: Effect) -> Effect {
        if let Some(host) = self.js_host.as_ref() {
            let _ = host.take_navigation();
        }
        effect.fetches.append(&mut self.pending_click_fetches);
        effect.timers.append(&mut self.pending_click_timers);
        effect.scripts.append(&mut self.pending_click_scripts);
        if effect.run_scripts.is_none() {
            effect.run_scripts = self.pending_click_run.take();
        } else {
            self.pending_click_run = None;
        }
        effect
    }

    /// Act on the navigation a tick asked for, if any (M10.11).
    ///
    /// JS never touches the network: the binding recorded a request, and this
    /// turns it into the *same* `Effect::fetch` the URL bar and a link click
    /// produce, through the same `navigate` that already models push-versus-
    /// replace and same-document fragments. At most one per tick — a script
    /// assigning `location` in a loop navigates once, because the queue holds
    /// one request and the last assignment wins.
    fn apply_script_navigation(&mut self, effect: &mut Effect) {
        let Some(request) = self.js_host.as_ref().and_then(js::Host::take_navigation) else {
            return;
        };
        if request.reload {
            let navigation = self.reload();
            effect.fetch = navigation.fetch;
            effect.cached = navigation.cached;
            effect.dirty |= navigation.dirty;
            return;
        }
        let Some(url) = self.resolve_href(&request.url) else {
            self.console.push(
                crate::js::console::Level::Warn,
                None,
                None,
                "a script asked to navigate to a URL that could not be resolved",
            );
            return;
        };
        // `assign` pushes history, `replace` does not — M6's distinction,
        // carried through rather than reinvented. `reload` was handled above
        // because M11.20 gives it forced-validation cache policy. A pure
        // fragment change is a same-document move, and `navigate` is where
        // that is decided for both remaining callers: `location.hash = 'x'`
        // scrolls exactly where a click on `<a href="#x">` scrolls, because
        // it is the same function (M11.4).
        let navigation = self.navigate(url, !request.replace);
        if let Some(fetch) = navigation.fetch {
            effect.fetch = Some(fetch);
            effect.dirty |= navigation.dirty;
        } else {
            // A same-document jump hands the loop no fetch, but the viewport
            // moved, so the frame is owed a repaint.
            effect.dirty |= navigation.dirty;
        }
    }

    /// The `fetch()` calls a tick asked for, tagged with the generation that
    /// asked so a response can be matched back to it.
    fn take_fetch_requests(&mut self, page: PageId) -> Vec<(PageId, js::FetchAsk)> {
        self.js_host
            .as_ref()
            .map(|host| {
                host.take_fetch_requests()
                    .into_iter()
                    .map(|ask| (page, ask))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Turn the timer work a tick asked for into requests the loop can hand to
    /// the timer thread, tagged with the generation that asked.
    fn take_timer_requests(&mut self, page: PageId) -> Vec<TimerRequest> {
        let Some(host) = self.js_host.as_ref() else {
            return Vec::new();
        };
        host.take_timer_requests()
            .into_iter()
            .map(|(id, delay)| match delay {
                Some(delay) => TimerRequest::Schedule {
                    page,
                    id: TimerId(id),
                    delay,
                },
                None => TimerRequest::Cancel {
                    page,
                    id: TimerId(id),
                },
            })
            .collect()
    }

    /// Dispatch a `click` at `node` and report whether a listener cancelled the
    /// default action (M10.8).
    ///
    /// Every way a reader can activate a link goes through here **before** the
    /// navigation: the mouse, `Enter` on a keyboard-focused link, and a hint
    /// follow. A page whose handler works with the mouse but not with `f` has
    /// broken the flagship feature (UX §3.4), so the three paths share this
    /// one call rather than each remembering to make it.
    ///
    /// The dispatch is a tick: the DOM is lent to it, and whatever its
    /// listeners mutated runs one invalidation cycle (M10.6) before this
    /// returns.
    fn dispatch_click(&mut self, node: NodeId) -> bool {
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return false;
        };
        let prevented = self.dispatch_reader_event(node, js::EventDescriptor::CLICK);
        self.finish_reader_action(id, before, logged_before);
        prevented
    }

    /// Snapshot one reader action. Several native steps may dispatch through
    /// the host after this point; one matching `finish_reader_action` consumes
    /// their aggregate DOM edits exactly once.
    fn begin_reader_action(&self) -> Option<(PageId, (u64, u64, u64), usize)> {
        let id = self.current_fetch?;
        let dom = self.dom.as_ref()?;
        Some((id, dom.change_versions(), self.console.entries().len()))
    }

    /// Dispatch one synchronous native event without ending its reader action.
    /// `js::dispatch` still pumps this event's microtasks before returning.
    fn dispatch_reader_event(&mut self, node: NodeId, event: js::EventDescriptor) -> bool {
        // No host means the page ran no script, so it has no listeners: not a
        // reason to start an engine.
        if self.js_host.is_none() {
            return false;
        }
        let Some(id) = self.current_fetch else {
            return false;
        };
        let Some(mut dom) = self.dom.take() else {
            return false;
        };

        let url = self.current_url().unwrap_or_default();
        let prevented = js::dispatch(
            &mut self.js_host,
            &mut dom,
            &js::PageContext {
                page: id.generation,
                url: &url,
                console: &self.console,
                storage: &self.storage,
                cookies: &self.cookies,
            },
            js::Target::Node(node.0),
            event,
        );
        self.dom = Some(dom);

        prevented
    }

    /// End an action started by [`Self::begin_reader_action`].
    fn finish_reader_action(
        &mut self,
        id: PageId,
        before: (u64, u64, u64),
        logged_before: usize,
    ) -> bool {
        let after = self
            .dom
            .as_ref()
            .map(Dom::change_versions)
            .unwrap_or(before);

        let mut effect = self.apply_dom_changes(before, after);
        self.pending_click_fetches = self.take_fetch_requests(id);
        self.pending_click_timers = self.take_timer_requests(id);
        self.adopt_inserted_scripts(id, &mut effect);
        self.pending_click_scripts = std::mem::take(&mut effect.scripts);
        self.pending_click_run = effect.run_scripts.take();
        if self.console.entries().len() != logged_before {
            self.console_view_built = false;
            self.build_visible_inspector();
        }
        effect.dirty
    }

    fn follow_href(&mut self, href: &str) -> Effect {
        let Some(url) = self.resolve_href(href) else {
            return Effect::default();
        };
        self.navigate(url, true)
    }

    fn resolve_href(&self, href: &str) -> Option<String> {
        let base = self.current_url()?;
        net::resolve_url(&base, href)
    }

    /// Frame (col,row) → document cell, or `None` if outside the page column /
    /// status row.
    fn frame_to_doc(&self, col: u16, row: u16) -> Option<(i32, i32)> {
        if row >= self.page() {
            return None;
        }
        let col_info = column(self.size.0);
        if col < col_info.left || col >= col_info.left + col_info.width {
            return None;
        }
        let doc_x = (col - col_info.left) as i32;
        let doc_y = row as i32 + self.viewport.offset() as i32;
        Some((doc_x, doc_y))
    }

    fn on_click(&mut self, col: u16, row: u16) -> Effect {
        let Some((x, _)) = self.frame_to_doc(col, row) else {
            return Effect::default();
        };
        let (Some(dom), Some(tree)) = (&self.dom, &self.layout_tree) else {
            return Effect::default();
        };
        // A click on a form control focuses it (M11.8) — and if it is a submit
        // button, activates it (M11.10), through the same function `Enter`
        // runs. That is not a new power for the mouse: a click on a *link*
        // already navigates, and refusing here would make a button the only
        // thing on a page that looks clickable and is not. Focus first, then
        // activate, so the two rules stay in the order M11.8 wrote them.
        //
        // Still no event dispatched: `click` on a control, `submit` on a form
        // and `preventDefault` cancelling one are all M11.13's.
        //
        // Asked before the link walk so that a control inside an anchor takes
        // the click, which is what the reader aimed at.
        let hit = if layout::has_viewport_adjustment(tree) {
            layout::hit_test_viewport(tree, x, row as i32, self.viewport.offset() as i32)
        } else {
            layout::hit_test(tree, x, self.viewport.offset() as i32 + row as i32)
        };
        if let Some(node) = hit.and_then(|n| layout::nearest_field(dom, n)) {
            let submits = form::is_submit_button(dom, node);
            let choice = match &dom.node(node).data {
                NodeData::Element { tag, .. } => {
                    layout::field::is_checkbox(dom, node, tag)
                        || layout::field::is_radio(dom, node, tag)
                }
                _ => false,
            };
            let moved_focus = self.focus != Some(node);
            // Text fields, selects and non-submit buttons have only focus and
            // click defaults. Keep both inside one reader action so listener
            // mutations cost one invalidation pass.
            if !submits && !choice {
                let Some((id, before, logged_before)) = self.begin_reader_action() else {
                    return Effect::default();
                };
                if moved_focus && !self.transition_focus_in_action(node) {
                    self.finish_reader_action(id, before, logged_before);
                    return self.take_click_navigation();
                }
                self.dispatch_reader_event(node, js::EventDescriptor::CLICK);
                self.finish_reader_action(id, before, logged_before);
                self.scroll_focus_into_view();
                return self.take_click_navigation();
            }
            if submits {
                let Some((id, before, logged_before)) = self.begin_reader_action() else {
                    return Effect::default();
                };
                if moved_focus && !self.transition_focus_in_action(node) {
                    self.finish_reader_action(id, before, logged_before);
                    return self.take_click_navigation();
                }
                let prevented = self.dispatch_reader_event(node, js::EventDescriptor::CLICK);
                let still_submits = self.dom.as_ref().is_some_and(|dom| {
                    dom.is_connected(node)
                        && dom.attr(node, "disabled").is_none()
                        && form::is_submit_button(dom, node)
                });
                let submission = if !prevented && still_submits {
                    self.submit_form_in_action(node)
                } else {
                    None
                };
                self.finish_reader_action(id, before, logged_before);
                self.scroll_focus_into_view();
                return self.resolve_submission(submission);
            }
            if choice {
                let Some((id, before, logged_before)) = self.begin_reader_action() else {
                    return Effect::default();
                };
                if moved_focus && !self.transition_focus_in_action(node) {
                    self.finish_reader_action(id, before, logged_before);
                    return self.take_click_navigation();
                }
                self.activate_choice_in_action(node);
                self.finish_reader_action(id, before, logged_before);
                self.scroll_focus_into_view();
                return self.take_click_navigation();
            }
            unreachable!("non-choice controls returned in the shared action above")
        }
        let link = if layout::has_viewport_adjustment(tree) {
            layout::link_at_viewport(tree, dom, x, row as i32, self.viewport.offset() as i32)
        } else {
            layout::link_at(tree, dom, x, self.viewport.offset() as i32 + row as i32)
        };
        let Some((node, href)) = link else {
            return Effect::default();
        };
        let href = href.to_string();
        let Some((id, before, logged_before)) = self.begin_reader_action() else {
            return Effect::default();
        };
        if self.focus != Some(node) && !self.transition_focus_in_action(node) {
            self.finish_reader_action(id, before, logged_before);
            return self.take_click_navigation();
        }
        let prevented = self.dispatch_reader_event(node, js::EventDescriptor::CLICK);
        self.finish_reader_action(id, before, logged_before);
        self.scroll_focus_into_view();
        if prevented {
            // `preventDefault()`: the page handled the click itself. Whatever
            // its listeners changed has already been drawn — unless one of
            // them navigated, which is a page handling a click by going
            // somewhere else.
            return self.take_click_navigation();
        }
        let effect = self.follow_href(&href);
        self.with_reader_side_effects(effect)
    }

    fn on_hover_move(&mut self, col: u16, row: u16) -> Effect {
        let target = self.frame_to_doc(col, row).and_then(|(x, _)| {
            let (dom, tree) = self.dom.as_ref().zip(self.layout_tree.as_ref())?;
            let hit = if layout::has_viewport_adjustment(tree) {
                layout::hit_test_viewport(tree, x, row as i32, self.viewport.offset() as i32)
            } else {
                layout::hit_test(tree, x, self.viewport.offset() as i32 + row as i32)
            };
            hit.map(|node| {
                // Hover the nearest element; for text, that is the text node —
                // `:hover` on `a:hover` needs the anchor. Walk up to the
                // nearest element... actually CSS :hover matches the element
                // under the pointer and ancestors. Our matching only checks
                // `ctx.hover == Some(node)` on the compound's subject. So set
                // hover to the deepest element (not text).
                let mut id = node;
                if !matches!(dom.node(id).data, crate::dom::NodeData::Element { .. }) {
                    id = dom.node(id).parent.unwrap_or(id);
                }
                // Prefer the link itself when inside one so `a:hover` fires.
                layout::nearest_link(dom, id).map(|(n, _)| n).unwrap_or(id)
            })
        });
        if target == self.hover {
            return Effect::default();
        }
        self.hover = target;
        // Restyle + repaint only — never relayout (PLAN.md M6).
        self.restyle_and_repaint();
        redraw()
    }

    /// Paint the whole frame: the visible body slice into the page area, plus
    /// the bottom row — the URL bar in `UrlInput`, the statusline in `Browse`.
    pub fn draw(&self, frame: &mut Frame) {
        frame.clear();
        match self.surface {
            Surface::Page => self.draw_page(frame),
            Surface::Dom => self.draw_dom(frame),
            Surface::Styles => self.draw_styles(frame),
            Surface::Boxes => self.draw_boxes(frame),
            Surface::Console => self.draw_console(frame),
            Surface::Help => self.draw_help(frame),
        }
        // Over the page area, after the body and before the bottom row.
        if self.timing_visible {
            self.draw_timing(frame);
        }
        let Some(y) = frame.height().checked_sub(1) else {
            return;
        };
        match &self.mode {
            Mode::UrlInput { buffer } => self.draw_prompt(frame, y, "open: ", buffer),
            Mode::SearchInput { buffer } => self.draw_prompt(frame, y, "find: ", buffer),
            // Typing keeps the statusline rather than opening a prompt: the
            // text being typed is in the page, where the caret is, and the row
            // is what says which mode the reader is in.
            Mode::Browse | Mode::Field { .. } | Mode::Select { .. } => self.draw_status(frame, y),
        }
    }

    /// The page: a laid-out document is painted from the cached display list
    /// at the current scroll offset (PLAN.md M5 — scroll never relayouts).
    /// Raw body text — still loading, unparsed, or an error page — still uses
    /// the line path and starts at the left edge.
    fn draw_page(&self, frame: &mut Frame) {
        if self.dom.is_some() {
            let left = column(self.size.0).left;
            let scroll = self.viewport.offset() as i32;
            paint::paint_to_frame(&self.display_list, frame, left, scroll, self.page());
            self.draw_search_highlights(frame, left, scroll);
            self.draw_focus_overlay(frame, left, scroll);
            self.draw_hint_overlay(frame, left, scroll);
            return;
        }
        for (row, line) in self.viewport.visible().iter().enumerate() {
            paint_line(frame, 0, row as u16, line);
        }
    }

    /// Reverse-video every visible search match.
    ///
    /// Paint-time frame overlay (same pattern as focus and link hints), not a
    /// display-list command: PLAN.md M7's "via the display list" is satisfied
    /// by reading layout geometry without restyle/relayout; overlays keep
    /// highlight chrome out of the cached list so scroll re-emit stays cheap.
    fn draw_search_highlights(&self, frame: &mut Frame, left: u16, scroll: i32) {
        let Some(session) = &self.search else {
            return;
        };
        if session.matches.is_empty() {
            return;
        }
        let page_h = self.page() as i32;
        let style = reversed();
        for (i, m) in session.matches.iter().enumerate() {
            let screen_y = self
                .layout_tree
                .as_ref()
                .and_then(|tree| layout::text_screen_y(tree, m.x, m.y, scroll))
                .unwrap_or_else(|| m.y.saturating_sub(scroll));
            if screen_y < 0 || screen_y >= page_h {
                continue;
            }
            // Current match is bold reverse so n/N is easy to track.
            let st = if i == session.current {
                Style {
                    attrs: Attrs::REVERSE | Attrs::BOLD,
                    ..Style::default()
                }
            } else {
                style
            };
            for dx in 0..m.width {
                let sx = left as i32 + m.x + dx;
                if sx < 0 || sx >= frame.width() as i32 {
                    continue;
                }
                let cell = frame.get(sx as u16, screen_y as u16);
                frame.set(sx as u16, screen_y as u16, Cell::new(cell.ch, st));
            }
        }
    }

    fn draw_help(&self, frame: &mut Frame) {
        for (row, line) in self.help_view.visible().iter().enumerate() {
            paint_line(frame, 0, row as u16, line);
        }
    }

    /// Reverse-video the focused link's text fragments (UI chrome, not CSS
    /// `:focus`). Paint-time only — no restyle.
    fn draw_focus_overlay(&self, frame: &mut Frame, left: u16, scroll: i32) {
        let (Some(focus), Some(dom), Some(tree)) =
            (self.focus, self.dom.as_ref(), self.layout_tree.as_ref())
        else {
            return;
        };
        let page_h = self.page() as i32;
        let style = reversed();
        // Clip-aware (M9.3): this overlay *writes glyphs*, so a clip-blind
        // walk would put a focused link's text back on a page that clipped it
        // away — the one surface that can undo the display list's trimming.
        tree.walk_clipped(&mut |_, b, clip| {
            // A focused *control* is marked differently from a focused link,
            // and deliberately so (M11.8): reverse video over a run of text
            // means "Enter follows this", and a field that borrowed the look
            // would be lying about what Enter does. Its frame reverses — two
            // cells, not a sentence — and a text field shows a caret where the
            // next character would go.
            if matches!(b.kind, BoxKind::Field(_)) && b.node == Some(focus) {
                self.draw_focused_field(frame, b, clip, left, scroll);
                return;
            }
            if b.kind != BoxKind::Text {
                return;
            }
            let Some(node) = b.node else {
                return;
            };
            if !layout::is_under(dom, node, focus) {
                return;
            }
            let Some(text) = &b.text else {
                return;
            };
            let (x, y) = (b.dimensions.content.x, b.dimensions.content.y);
            let Some((x, text)) = clip.trim_text(x, y, text) else {
                return;
            };
            let screen_y = layout::box_screen_y(b, scroll);
            if screen_y < 0 || screen_y >= page_h {
                return;
            }
            let screen_x = left as i32 + x;
            if screen_x < 0 {
                return;
            }
            frame.put_str(screen_x as u16, screen_y as u16, &text, style);
        });
    }

    /// The focused control's own chrome: its frame reversed, a caret where the
    /// next character would go (M11.8), and — while the reader is typing into
    /// it — the window of the value that caret is in (M11.9).
    ///
    /// A frame overlay like the focused link's and the search highlight's, for
    /// the same reason: focus is UI chrome, not CSS `:focus`, so moving it must
    /// restyle nothing, relayout nothing and repaint no display list. The
    /// **window** is here rather than in the tree for the same reason again —
    /// which window of a value is on screen is a fact about where a cursor is,
    /// and layout must not depend on that. The display list keeps the view a
    /// page's own paint produces (the start of the value); this draws the
    /// scrolled one over exactly the same cells, from the same
    /// `layout::field` function, so the two cannot disagree about what a field
    /// looks like.
    fn draw_focused_field(
        &self,
        frame: &mut Frame,
        b: &layout::LayoutBox,
        clip: layout::Clip,
        left: u16,
        scroll: i32,
    ) {
        let page_h = self.page() as i32;
        // Redraw only when UI state changes the window: a text caret or select
        // cursor. Otherwise paint already drew these cells.
        let painted = match self.mode {
            Mode::Select { select, cursor } if b.node == Some(select) => self
                .dom
                .as_ref()
                .and_then(|dom| {
                    layout::field::options(dom, select)
                        .iter()
                        .position(|option| option.node == cursor)
                })
                .map(|row| layout::field::painted_select(b, row)),
            _ => self
                .caret()
                .map(|caret| layout::field::painted(b, Some(caret))),
        };
        if let Some(painted) = &painted {
            for run in &painted.runs {
                let Some((x, text)) = clip.trim_text(run.x, run.y, &run.text) else {
                    continue;
                };
                let screen_y = layout::box_screen_y(b, scroll)
                    .saturating_add(run.y.saturating_sub(b.dimensions.content.y));
                let screen_x = left as i32 + x;
                if screen_y < 0 || screen_y >= page_h || screen_x < 0 {
                    continue;
                }
                frame.put_str(screen_x as u16, screen_y as u16, &text, run.style);
            }
        }
        let caret = painted
            .as_ref()
            .and_then(|painted| painted.caret)
            .or_else(|| layout::field::caret(b));
        let mut reverse = |x: i32, y: i32| {
            if !clip.contains(x, y) {
                return;
            }
            let screen_y = layout::box_screen_y(b, scroll)
                .saturating_add(y.saturating_sub(b.dimensions.content.y));
            let screen_x = left as i32 + x;
            if screen_y < 0 || screen_y >= page_h || screen_x < 0 {
                return;
            }
            if screen_x >= frame.width() as i32 {
                return;
            }
            let cell = frame.get(screen_x as u16, screen_y as u16);
            frame.set(
                screen_x as u16,
                screen_y as u16,
                Cell::new(cell.ch, reversed()),
            );
        };
        let rect = b.dimensions.content;
        for row in 0..rect.height {
            let y = rect.y + row;
            if b.dimensions.padding.left > 0 {
                reverse(rect.x - 1, y);
            }
            if b.dimensions.padding.right > 0 {
                reverse(rect.right(), y);
            }
        }
        if let BoxKind::Field(paint) = b.kind
            && matches!(
                paint.shows,
                layout::Shows::Checkbox(_) | layout::Shows::Radio(_)
            )
        {
            reverse(rect.x, rect.y);
        }
        if let Some(row) = painted.as_ref().and_then(|painted| painted.cursor_row) {
            for x in 0..rect.width {
                reverse(rect.x + x, rect.y + row);
            }
        }
        if let Some((x, y)) = caret {
            reverse(x, y);
        }
    }

    /// Link-hint labels on top of the page.
    fn draw_hint_overlay(&self, frame: &mut Frame, left: u16, scroll: i32) {
        let Some(session) = &self.hint else {
            return;
        };
        let page_h = self.page() as i32;
        let style = Style {
            attrs: Attrs::REVERSE | Attrs::BOLD,
            ..Style::default()
        };
        let shown = hints::filter_prefix(&session.labels, &session.buffer);
        for (label, link) in shown {
            let screen_y = link.y - scroll;
            if screen_y < 0 || screen_y >= page_h {
                continue;
            }
            let screen_x = left as i32 + link.x;
            if screen_x < 0 {
                continue;
            }
            frame.put_str(screen_x as u16, screen_y as u16, label, style);
        }
    }

    /// The `F1` surface: the cached tree lines in the page area, or a calm
    /// placeholder while there is nothing parsed yet (fresh start, mid-load,
    /// or after a failure) — never a panic, never a blank that looks broken.
    fn draw_dom(&self, frame: &mut Frame) {
        if self.dom.is_none() {
            frame.put_str(0, 0, "no DOM yet — open a page (o)", Style::default());
            return;
        }
        for (row, line) in self.dom_view.visible().iter().enumerate() {
            paint_line(frame, 0, row as u16, line);
        }
    }

    /// The `F2` surface: one line per element with its computed values, or the
    /// same calm placeholder `F1` shows when there is nothing to inspect.
    fn draw_styles(&self, frame: &mut Frame) {
        if self.styles.is_none() {
            frame.put_str(0, 0, "no styles yet — open a page (o)", Style::default());
            return;
        }
        for (row, line) in self.styles_view.visible().iter().enumerate() {
            paint_line(frame, 0, row as u16, line);
        }
    }

    /// The `F3` surface: layout boxes with content-box geometry.
    fn draw_boxes(&self, frame: &mut Frame) {
        if self.layout_tree.is_none() {
            frame.put_str(0, 0, "no boxes yet — open a page (o)", Style::default());
            return;
        }
        for (row, line) in self.boxes_view.visible().iter().enumerate() {
            paint_line(frame, 0, row as u16, line);
        }
    }

    fn draw_console(&self, frame: &mut Frame) {
        if self.console.is_empty() {
            frame.put_str(0, 0, "no console output on this page", Style::default());
            return;
        }
        for (row, line) in self.console_view.visible().iter().enumerate() {
            paint_line(frame, 0, row as u16, line);
        }
    }

    /// The statusline (PLAN.md §3): URL · fetch progress · scroll % and frame
    /// time. Composition is pure and pre-padded to the row width, so one
    /// `put_str` paints every cell reversed.
    fn draw_status(&self, frame: &mut Frame, y: u16) {
        let row = statusline::compose(
            frame.width() as usize,
            &self.status_left(),
            &self.status_middle(),
            &self.status_right(),
        );
        frame.put_str(0, y, &row, reversed());
    }

    /// The `F4` timing overlay: the `Timings` rows as one reversed box in the
    /// page area's top-right corner. It never touches the bottom row — a
    /// 1-row frame has no page area, so nothing is drawn — and on a frame
    /// narrower than the box it clips at the left edge. No rows (nothing
    /// timed yet) → nothing drawn.
    fn draw_timing(&self, frame: &mut Frame) {
        let rows = self.timings.rows();
        let Some(box_w) = rows.iter().map(|r| r.width()).max() else {
            return;
        };
        let x = (frame.width() as usize).saturating_sub(box_w) as u16;
        let page = frame.height().saturating_sub(1) as usize;
        for (y, row) in rows.iter().enumerate().take(page) {
            // Left-pad each row to the widest row's width — in cells, never
            // chars — so the overlay is a solid rectangle with the `ms`
            // column against the frame edge.
            let mut padded = " ".repeat(box_w - row.width());
            padded.push_str(row);
            frame.put_str(x, y as u16, &padded, reversed());
        }
    }

    /// One-line prompt (`open:` / `find:`) with a cursor cell at the end.
    fn draw_prompt(&self, frame: &mut Frame, y: u16, label: &str, buffer: &str) {
        let style = reversed();
        for x in 0..frame.width() {
            frame.set(x, y, Cell::new(' ', style));
        }
        let mut prompt = String::from(label);
        prompt.push_str(buffer);
        let end = frame.put_str(0, y, &prompt, style);
        frame.set(end, y, Cell::new(CURSOR, style));
    }

    /// Left segment: what page this is — the current fetch's URL, or the app
    /// name before anything has been opened — tagged with the active surface
    /// when it isn't the page itself.
    fn status_left(&self) -> String {
        let base = match &self.fetch {
            Fetch::Idle => "yata".to_string(),
            Fetch::Loading { url, .. } | Fetch::Loaded { url, .. } | Fetch::Failed { url, .. } => {
                url.clone()
            }
        };
        let base = if self.revealed {
            // Short, and only present when it happened: the page rendered
            // blank until its own `display:none` was ignored.
            format!("[unhidden] {base}")
        } else {
            base
        };
        // Typing is a mode a reader can be surprised by — every letter goes
        // into the page instead of doing what it usually does — so the row that
        // always tells them where they are says so, and `Esc` visibly takes it
        // away again (UX §1: every keypress produces a visible change).
        let base = match self.mode {
            Mode::Field { .. } => format!("[typing · Esc] {base}"),
            Mode::Select { .. } => format!("[selecting · Esc] {base}"),
            _ => base,
        };
        match self.surface {
            Surface::Page => base,
            Surface::Dom => format!("[dom] {base}"),
            Surface::Styles => format!("[styles] {base}"),
            Surface::Boxes => format!("[boxes] {base}"),
            Surface::Console => format!("[console] {base}"),
            Surface::Help => format!("[help] {base}"),
        }
    }

    /// Middle segment: where the fetch stands — spinner + progress, the
    /// loaded summary, or the failure reason. A flash message (yank) wins
    /// while present so the user sees confirmation.
    fn status_middle(&self) -> String {
        if let Some(msg) = &self.status_msg {
            return msg.clone();
        }
        if let Some(session) = &self.hint {
            let n = hints::filter_prefix(&session.labels, &session.buffer).len();
            return format!("hints: {} ({n})", session.buffer);
        }
        if let Some(session) = &self.search
            && !session.matches.is_empty()
        {
            return format!("{}/{}", session.current + 1, session.matches.len());
        }
        // A page whose script threw and then rendered nothing is the worst
        // outcome this milestone can produce; this segment is the cure. It
        // sits in the existing middle segment rather than a second row, and it
        // yields to a live fetch — a page still loading has not finished
        // failing yet.
        let errors = self.console.error_count();
        if errors > 0 && matches!(self.fetch, Fetch::Loaded { .. }) {
            return format!(
                "{errors} JS error{} · F5",
                if errors == 1 { "" } else { "s" }
            );
        }
        match &self.fetch {
            Fetch::Idle => String::new(),
            Fetch::Loading { bytes_so_far, .. } => format!(
                "{} loading… {} KB",
                SPINNER[self.spinner],
                kb(*bytes_so_far)
            ),
            Fetch::Loaded { status, body, .. } => {
                format!("{status} · {} KB", kb(body.len() as u64))
            }
            Fetch::Failed { reason, .. } => reason.clone(),
        }
    }

    /// Right segment: `scroll% · frame time`. A part with no value yet is
    /// omitted, not shown as a placeholder. The percentage tracks whichever
    /// surface the scroll keys currently drive.
    fn status_right(&self) -> String {
        let mut parts = Vec::new();
        let view = if self.dom_active() {
            &self.dom_view
        } else if self.styles_active() {
            &self.styles_view
        } else if self.boxes_active() {
            &self.boxes_view
        } else if self.help_active() {
            &self.help_view
        } else {
            &self.viewport
        };
        if let Some(percent) = view.scroll_percent() {
            parts.push(format!("{percent}%"));
        }
        if let Some(dur) = self.timings.frame {
            parts.push(timing::format_ms(dur));
        }
        parts.join(" · ")
    }
}

/// What `←` `→` `Home` `End` ask of the caret (M11.9).
///
/// Vim-flavoured word motions (`w`, `b`, `^`, `$`) are deliberately absent:
/// they need a second modal layer inside an input mode, and no ladder page is
/// waiting for one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CaretMove {
    Prev,
    Next,
    LineStart,
    LineEnd,
}

/// The byte offset of the `chars`-th character, or the end of the string.
///
/// The caret is counted in characters and `String` is indexed in bytes; this is
/// the one place the two meet, so it is the only place a multi-byte character
/// could be cut in half.
fn byte_at(value: &str, chars: usize) -> usize {
    value
        .char_indices()
        .nth(chars)
        .map_or(value.len(), |(at, _)| at)
}

/// The character indices of the ends of the line the caret is on.
///
/// The whole value for an `<input>`, which has one line. A `<textarea>` has
/// several, and there `Home`/`End` mean what they mean in every editor: the
/// ends of *this* line, not of everything in the box.
fn line_bounds(value: &str, caret: usize) -> (usize, usize) {
    let chars: Vec<char> = value.chars().collect();
    let caret = caret.min(chars.len());
    let start = chars[..caret]
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(0, |at| at + 1);
    let end = chars[caret..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |at| caret + at);
    (start, end)
}

/// Widest the text column ever gets (UX §3.5). Past roughly this many cells the
/// eye loses the start of the next line, which is why every book and every
/// readable site stops around here — a maximized terminal must not turn a page
/// into edge-to-edge soup.
const MAX_MEASURE: u16 = 90;

/// A cell of gutter on each side, so text never touches the frame edge. In a
/// terminal too narrow for the cap this is all the margin there is.
const PAGE_MARGIN: u16 = 1;

/// Where the page's text column sits in a frame `width` cells wide.
struct Column {
    left: u16,
    width: u16,
}

/// The column for a terminal width: capped at `MAX_MEASURE`, gutters either
/// side, and centered in whatever is left over — so a wide terminal shows a
/// readable column in the middle rather than a full-width wall. At least one
/// cell survives, because `layout` must always have somewhere to put a
/// character.
fn column(width: u16) -> Column {
    let w = width.saturating_sub(2 * PAGE_MARGIN).clamp(1, MAX_MEASURE);
    Column {
        left: width.saturating_sub(w) / 2,
        width: w,
    }
}

/// Paint one display line from `left`, span by span, each with its own style.
/// The returned end column of each `put_str` is the next span's start, so a
/// wide character's second cell is never written over.
fn paint_line(frame: &mut Frame, left: u16, y: u16, line: &crate::layout::Line) {
    let mut x = left;
    for span in &line.spans {
        x = frame.put_str(x, y, &span.text, span.style);
    }
}

/// Spinner frames, advanced once per accepted progress message.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The cursor cell drawn at the end of the URL buffer.
const CURSOR: char = '▮';

fn reversed() -> Style {
    Style {
        attrs: Attrs::REVERSE,
        ..Style::default()
    }
}

/// A plain redraw effect: no quit, no fetch.
fn redraw() -> Effect {
    Effect {
        dirty: true,
        ..Effect::default()
    }
}

fn tab_action(tab: TabAction) -> Effect {
    Effect {
        dirty: true,
        tab: Some(tab),
        ..Effect::default()
    }
}

pub const MAX_TABS: usize = 16;
pub use crate::browser::bookmarks::MAX_TITLE_BYTES;

struct Tab {
    id: TabId,
    page: App,
    stale_size: bool,
}

#[derive(Debug)]
enum BookmarkPersistence {
    Loading {
        available: bool,
    },
    Ready {
        available: bool,
        submitted: u64,
        acknowledged: u64,
        error: Option<String>,
    },
    ReadOnly(String),
}

/// One terminal browser session: ordered live pages, one active selection and
/// one chrome surface. `App` remains the page state machine; this is the sole
/// owner and router for the bounded tab set.
pub struct Browser {
    size: (u16, u16),
    kitty_graphics: bool,
    session: BrowserSession,
    tabs: Vec<Tab>,
    active: usize,
    next_tab: u64,
    /// Tab whose Kitty placement bookkeeping matches the terminal-global
    /// image state.
    kitty_tab: Option<TabId>,
    kitty_hidden_by_bookmarks: bool,
    bookmarks: Bookmarks,
    bookmark_library: bool,
    bookmark_selected: Option<usize>,
    bookmark_first_visible: usize,
    bookmark_revision: u64,
    bookmark_notice: Option<String>,
    bookmark_persistence: BookmarkPersistence,
}

impl Browser {
    pub fn new(w: u16, h: u16) -> Browser {
        Browser::with_caps(w, h, false)
    }

    pub fn with_caps(w: u16, h: u16, kitty_graphics: bool) -> Browser {
        Self::build(w, h, kitty_graphics, false, false)
    }

    /// Interactive construction begins visibly loading until its worker sends
    /// the session-global load result. Tests and headless callers use
    /// `with_caps`, which performs no persistence setup.
    pub fn with_bookmark_persistence(
        w: u16,
        h: u16,
        kitty_graphics: bool,
        available: bool,
    ) -> Browser {
        Self::build(w, h, kitty_graphics, true, available)
    }

    fn build(w: u16, h: u16, kitty_graphics: bool, loading: bool, available: bool) -> Browser {
        let id = TabId(1);
        let session = BrowserSession::new();
        Browser {
            size: (w, h),
            kitty_graphics,
            tabs: vec![Tab {
                id,
                page: App::with_session(w, h.saturating_sub(1), kitty_graphics, id, &session),
                stale_size: false,
            }],
            session,
            active: 0,
            next_tab: 2,
            kitty_tab: Some(id),
            kitty_hidden_by_bookmarks: false,
            bookmarks: Bookmarks::new(),
            bookmark_library: false,
            bookmark_selected: None,
            bookmark_first_visible: 0,
            bookmark_revision: 0,
            bookmark_notice: None,
            bookmark_persistence: if loading {
                BookmarkPersistence::Loading { available }
            } else {
                BookmarkPersistence::Ready {
                    available: false,
                    submitted: 0,
                    acknowledged: 0,
                    error: None,
                }
            },
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    pub fn start_navigation(&mut self, url: String) -> Effect {
        self.tabs[self.active].page.start_navigation(url)
    }

    pub fn update(&mut self, mut msg: Msg) -> Effect {
        msg = match msg {
            Msg::BookmarksLoaded(result) => return self.bookmarks_loaded(result),
            Msg::BookmarksSaved { revision, result } => {
                return self.bookmarks_saved(revision, result);
            }
            other => other,
        };
        if let Some(page) = msg.page() {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == page.tab) else {
                return Effect::default();
            };
            let old_label = self.tabs[index].page.tab_label();
            let mut effect = self.tabs[index].page.update(msg);
            if index != self.active || self.bookmark_library {
                let page = &self.tabs[index].page;
                self.tabs[index].stale_size =
                    page.layout_tree.is_some() && page.laid_out_size != Some(page.size);
                effect.dirty =
                    old_label != self.tabs[index].page.tab_label() && self.tab_is_visible(index);
            }
            effect.tab = None;
            return effect;
        }

        if let Msg::Key(event) = &msg {
            if self.bookmark_library {
                return self.on_bookmark_key(event);
            }
            if self.tabs[self.active].page.accepts_browser_chrome_keys()
                && let Resolution::Action(action) = keys::resolve(
                    keys::Mode::Browse,
                    self.tabs[self.active].page.pending,
                    event,
                )
                && matches!(action, Action::AddBookmark | Action::ToggleBookmarks)
            {
                self.tabs[self.active].page.pending = None;
                return match action {
                    Action::AddBookmark => self.add_active_bookmark(),
                    Action::ToggleBookmarks => self.open_bookmark_library(),
                    _ => unreachable!(),
                };
            }
        }
        if self.bookmark_library && matches!(msg, Msg::Mouse(_)) {
            return Effect::default();
        }

        match &mut msg {
            Msg::Resize(w, h) => {
                self.size = (*w, *h);
                self.keep_bookmark_visible();
                let page_h = h.saturating_sub(1);
                for (index, tab) in self.tabs.iter_mut().enumerate() {
                    if index != self.active {
                        tab.stale_size = tab.page.defer_resize(*w, page_h);
                    }
                }
                self.tabs[self.active].stale_size = false;
                self.tabs[self.active].page.update(Msg::Resize(*w, page_h))
            }
            Msg::Mouse(mouse) => {
                if mouse.row == 0 {
                    return Effect::default();
                }
                mouse.row -= 1;
                self.update_active(msg)
            }
            _ => self.update_active(msg),
        }
    }

    fn bookmarks_loaded(&mut self, result: Result<Bookmarks, String>) -> Effect {
        let available = match self.bookmark_persistence {
            BookmarkPersistence::Loading { available } => available,
            _ => return Effect::default(),
        };
        match result {
            Ok(bookmarks) => {
                self.bookmarks = bookmarks;
                self.bookmark_selected = (!self.bookmarks.is_empty()).then_some(0);
                self.bookmark_persistence = BookmarkPersistence::Ready {
                    available,
                    submitted: 0,
                    acknowledged: 0,
                    error: None,
                };
            }
            Err(error) => {
                self.bookmarks = Bookmarks::new();
                self.bookmark_selected = None;
                self.bookmark_persistence = BookmarkPersistence::ReadOnly(bounded(error));
            }
        }
        redraw()
    }

    fn bookmarks_saved(&mut self, revision: u64, result: Result<(), String>) -> Effect {
        let BookmarkPersistence::Ready {
            submitted,
            acknowledged,
            error,
            ..
        } = &mut self.bookmark_persistence
        else {
            return Effect::default();
        };
        if revision < *acknowledged {
            return Effect::default();
        }
        *acknowledged = revision;
        if revision == *submitted {
            *error = result.err().map(bounded);
            return redraw();
        }
        Effect::default()
    }

    fn open_bookmark_library(&mut self) -> Effect {
        self.bookmark_library = true;
        self.bookmark_notice = None;
        self.bookmark_selected = (!self.bookmarks.is_empty()).then_some(0);
        self.bookmark_first_visible = 0;
        redraw()
    }

    fn on_bookmark_key(&mut self, event: &KeyEvent) -> Effect {
        let Resolution::Action(action) = keys::resolve(keys::Mode::Bookmarks, None, event) else {
            return Effect::default();
        };
        self.bookmark_notice = None;
        match action {
            Action::Quit => Effect {
                quit: true,
                ..Effect::default()
            },
            Action::ToggleBookmarks | Action::Cancel => {
                self.bookmark_library = false;
                redraw()
            }
            Action::BookmarkNext => self.move_bookmark_selection(1),
            Action::BookmarkPrevious => self.move_bookmark_selection(-1),
            Action::BookmarkFirst => self.set_bookmark_selection(0),
            Action::BookmarkLast => {
                self.set_bookmark_selection(self.bookmarks.len().saturating_sub(1))
            }
            Action::BookmarkOpen => {
                let Some(index) = self.bookmark_selected else {
                    return Effect::default();
                };
                let Some(record) = self.bookmarks.records().get(index) else {
                    return Effect::default();
                };
                let url = record.url.to_string();
                self.bookmark_library = false;
                self.tabs[self.active].page.navigate(url, true)
            }
            Action::BookmarkDelete => self.delete_selected_bookmark(),
            _ => Effect::default(),
        }
    }

    fn move_bookmark_selection(&mut self, delta: isize) -> Effect {
        let Some(selected) = self.bookmark_selected else {
            return Effect::default();
        };
        let last = self.bookmarks.len().saturating_sub(1);
        let next = if delta < 0 {
            selected.saturating_sub(delta.unsigned_abs())
        } else {
            selected.saturating_add(delta as usize).min(last)
        };
        self.set_bookmark_selection(next)
    }

    fn set_bookmark_selection(&mut self, selected: usize) -> Effect {
        if self.bookmarks.is_empty() {
            return Effect::default();
        }
        let selected = selected.min(self.bookmarks.len() - 1);
        if self.bookmark_selected == Some(selected) {
            return Effect::default();
        }
        self.bookmark_selected = Some(selected);
        self.keep_bookmark_visible();
        redraw()
    }

    fn keep_bookmark_visible(&mut self) {
        let Some(selected) = self.bookmark_selected else {
            self.bookmark_first_visible = 0;
            return;
        };
        let rows = self.size.1.saturating_sub(3) as usize;
        if rows == 0 {
            self.bookmark_first_visible = selected;
        } else if selected < self.bookmark_first_visible {
            self.bookmark_first_visible = selected;
        } else if selected >= self.bookmark_first_visible.saturating_add(rows) {
            self.bookmark_first_visible = selected + 1 - rows;
        }
    }

    fn add_active_bookmark(&mut self) -> Effect {
        if matches!(
            self.bookmark_persistence,
            BookmarkPersistence::Loading { .. }
        ) {
            self.tabs[self.active].page.status_msg = Some("bookmarks are still loading".into());
            return redraw();
        }
        if self.bookmark_revision == u64::MAX {
            self.tabs[self.active].page.status_msg =
                Some("bookmark revision space exhausted".into());
            return redraw();
        }
        let Some(url) = self.tabs[self.active].page.current_url() else {
            self.tabs[self.active].page.status_msg = Some("blank page cannot be bookmarked".into());
            return redraw();
        };
        if url.len() > MAX_URL_BYTES {
            self.tabs[self.active].page.status_msg = Some("bookmark URL is too long".into());
            return redraw();
        }
        if !valid_url(&url) {
            self.tabs[self.active].page.status_msg =
                Some("only HTTP(S) pages can be bookmarked".into());
            return redraw();
        }
        let title = self.tabs[self.active].page.tab_label();
        match self.bookmarks.add(Arc::from(url), Arc::from(title)) {
            AddResult::Duplicate => {
                self.tabs[self.active].page.status_msg = Some("already bookmarked".into());
                redraw()
            }
            AddResult::Full => {
                self.tabs[self.active].page.status_msg =
                    Some(format!("bookmark limit ({MAX_BOOKMARKS})"));
                redraw()
            }
            AddResult::Invalid => {
                self.tabs[self.active].page.status_msg =
                    Some("page cannot be represented as a bookmark".into());
                redraw()
            }
            AddResult::Added => {
                self.tabs[self.active].page.status_msg = Some("bookmarked".into());
                self.bookmark_selected = Some(0);
                self.bookmark_first_visible = 0;
                self.bookmark_mutated()
            }
        }
    }

    fn delete_selected_bookmark(&mut self) -> Effect {
        if matches!(
            self.bookmark_persistence,
            BookmarkPersistence::Loading { .. }
        ) {
            self.bookmark_notice = Some("bookmarks are still loading".into());
            return redraw();
        }
        if self.bookmark_revision == u64::MAX {
            self.bookmark_notice = Some("bookmark revision space exhausted".into());
            return redraw();
        }
        let Some(selected) = self.bookmark_selected else {
            return Effect::default();
        };
        if self.bookmarks.remove(selected).is_none() {
            return Effect::default();
        }
        self.bookmark_selected = if self.bookmarks.is_empty() {
            None
        } else {
            Some(selected.min(self.bookmarks.len() - 1))
        };
        self.keep_bookmark_visible();
        self.bookmark_notice = Some("bookmark deleted".into());
        self.bookmark_mutated()
    }

    fn bookmark_mutated(&mut self) -> Effect {
        let Some(revision) = self.bookmark_revision.checked_add(1) else {
            self.bookmark_notice = Some("bookmark revision space exhausted".into());
            return redraw();
        };
        self.bookmark_revision = revision;
        let mut save = None;
        if let BookmarkPersistence::Ready {
            available: true,
            submitted,
            error,
            ..
        } = &mut self.bookmark_persistence
        {
            *submitted = revision;
            *error = None;
            save = Some((revision, self.bookmarks.snapshot()));
        }
        Effect {
            dirty: true,
            bookmark_save: save,
            ..Effect::default()
        }
    }

    fn update_active(&mut self, msg: Msg) -> Effect {
        let mut effect = self.tabs[self.active].page.update(msg);
        if let Some(action) = effect.tab.take() {
            self.apply_tab_action(action, &mut effect);
        }
        effect
    }

    fn apply_tab_action(&mut self, action: TabAction, effect: &mut Effect) {
        match action {
            TabAction::New => {
                self.open_tab();
            }
            TabAction::Close => {
                let closed = self.tabs.remove(self.active).id;
                effect.timers.push(TimerRequest::CancelTab { tab: closed });
                if self.tabs.is_empty() {
                    if !self.open_tab() {
                        return;
                    }
                } else if self.active == self.tabs.len() {
                    self.active -= 1;
                }
                self.activate_current();
            }
            TabAction::Next if self.tabs.len() > 1 => {
                self.active = (self.active + 1) % self.tabs.len();
                self.activate_current();
            }
            TabAction::Previous if self.tabs.len() > 1 => {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
                self.activate_current();
            }
            TabAction::Next | TabAction::Previous => {}
        }
        for tab in &mut self.tabs {
            tab.page.pending = None;
        }
    }

    fn open_tab(&mut self) -> bool {
        if self.tabs.len() == MAX_TABS {
            self.tabs[self.active].page.status_msg = Some(format!("tab limit ({MAX_TABS})"));
            return false;
        }
        let Some(next) = self.next_tab.checked_add(1) else {
            self.tabs[self.active].page.status_msg = Some("tab id space exhausted".into());
            return false;
        };
        let id = TabId(self.next_tab);
        self.next_tab = next;
        let mut page = App::with_session(
            self.size.0,
            self.size.1.saturating_sub(1),
            self.kitty_graphics,
            id,
            &self.session,
        );
        page.mode = Mode::UrlInput {
            buffer: String::new(),
        };
        self.active = (self.active + 1).min(self.tabs.len());
        self.tabs.insert(
            self.active,
            Tab {
                id,
                page,
                stale_size: false,
            },
        );
        true
    }

    fn activate_current(&mut self) {
        if self.tabs[self.active].stale_size {
            self.tabs[self.active].stale_size = false;
            let (w, h) = self.size;
            let _ = self.tabs[self.active]
                .page
                .update(Msg::Resize(w, h.saturating_sub(1)));
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        frame.clear();
        self.draw_tab_strip(frame);
        if self.bookmark_library {
            self.draw_bookmark_library(frame);
            return;
        }
        let mut page = Frame::new(frame.width(), frame.height().saturating_sub(1));
        self.tabs[self.active].page.draw(&mut page);
        frame.blit_rows(&page, 1);
        self.draw_browser_status(frame);
    }

    fn draw_browser_status(&self, frame: &mut Frame) {
        let Some(y) = frame.height().checked_sub(1) else {
            return;
        };
        let page = &self.tabs[self.active].page;
        let page_middle = page.status_middle();
        let bookmark_middle = format!(
            "{} bookmark{} · {}",
            self.bookmarks.len(),
            if self.bookmarks.len() == 1 { "" } else { "s" },
            self.bookmark_status()
        );
        let middle = if page_middle.is_empty() {
            bookmark_middle
        } else {
            format!("{page_middle} · {bookmark_middle}")
        };
        let row = statusline::compose(
            frame.width() as usize,
            &page.status_left(),
            &middle,
            &page.status_right(),
        );
        frame.put_str(0, y, &row, reversed());
    }

    fn draw_bookmark_library(&self, frame: &mut Frame) {
        if frame.height() <= 1 || frame.width() == 0 {
            return;
        }
        frame.put_str(0, 1, "yata — bookmarks", Style::default());
        let hint = help::compact_hint(keys::Mode::Bookmarks);
        let title_width = "yata — bookmarks".width() as u16;
        if title_width.saturating_add(3) < frame.width() {
            frame.put_str(title_width + 3, 1, &hint, Style::default());
        }
        let status_row = frame.height().saturating_sub(1);
        let record_rows = frame.height().saturating_sub(3) as usize;
        if self.bookmarks.is_empty() && record_rows > 0 {
            let empty = if matches!(
                self.bookmark_persistence,
                BookmarkPersistence::Loading { .. }
            ) {
                "loading bookmarks…"
            } else {
                "no bookmarks yet"
            };
            frame.put_str(0, 2, empty, Style::default());
        } else {
            for (row, (index, record)) in self
                .bookmarks
                .records()
                .iter()
                .enumerate()
                .skip(self.bookmark_first_visible)
                .enumerate()
                .take(record_rows)
            {
                let y = 2 + row as u16;
                let style = if self.bookmark_selected == Some(index) {
                    reversed()
                } else {
                    Style::default()
                };
                if self.bookmark_selected == Some(index) {
                    for x in 0..frame.width() {
                        frame.set(x, y, Cell::new(' ', style));
                    }
                }
                let width = frame.width() as usize;
                let line = if width > 3 {
                    let content = width - 3;
                    let title_width = content / 2;
                    let url_width = content - title_width;
                    let mut line = clip_cells(&record.title, title_width);
                    line.push_str(" — ");
                    line.push_str(&clip_cells(&record.url, url_width));
                    line
                } else {
                    clip_cells(&record.title, width)
                };
                frame.put_str(0, y, &line, style);
            }
        }
        if status_row > 1 {
            let left = format!(
                "{} bookmark{}",
                self.bookmarks.len(),
                if self.bookmarks.len() == 1 { "" } else { "s" }
            );
            let middle = self.bookmark_status();
            let row = statusline::compose(frame.width() as usize, &left, &middle, "");
            frame.put_str(0, status_row, &row, reversed());
        }
    }

    fn bookmark_status(&self) -> String {
        if let Some(notice) = &self.bookmark_notice {
            return notice.clone();
        }
        match &self.bookmark_persistence {
            BookmarkPersistence::Loading { .. } => "loading bookmarks".into(),
            BookmarkPersistence::ReadOnly(error) => format!("read-only · {error}"),
            BookmarkPersistence::Ready {
                available: false, ..
            } => "persistence unavailable".into(),
            BookmarkPersistence::Ready {
                error: Some(error), ..
            } => format!("unsaved · {error}"),
            BookmarkPersistence::Ready {
                submitted,
                acknowledged,
                error: None,
                ..
            } if submitted > acknowledged => "saving bookmarks".into(),
            BookmarkPersistence::Ready { .. } => "saved".into(),
        }
    }

    fn draw_tab_strip(&self, frame: &mut Frame) {
        if frame.height() == 0 || frame.width() == 0 {
            return;
        }
        let labels: Vec<String> = self.tabs.iter().map(|tab| tab.page.tab_label()).collect();
        let widths: Vec<usize> = labels.iter().map(|label| label.width() + 2).collect();
        let available = frame.width() as usize;
        let mut start = self.active;
        let mut used = widths[self.active].min(available);
        while start > 0 && used.saturating_add(widths[start - 1]) <= available {
            start -= 1;
            used += widths[start];
        }
        let mut x = 0;
        for (index, label) in labels.iter().enumerate().skip(start) {
            if x >= frame.width() {
                break;
            }
            let style = if index == self.active {
                reversed()
            } else {
                Style::default()
            };
            x = frame.put_str(x, 0, " ", style);
            x = frame.put_str(x, 0, label, style);
            x = frame.put_str(x, 0, " ", style);
        }
    }

    fn tab_is_visible(&self, index: usize) -> bool {
        let labels: Vec<String> = self.tabs.iter().map(|tab| tab.page.tab_label()).collect();
        let widths: Vec<usize> = labels.iter().map(|label| label.width() + 2).collect();
        let available = self.size.0 as usize;
        let mut start = self.active;
        let mut used = widths[self.active].min(available);
        while start > 0 && used.saturating_add(widths[start - 1]) <= available {
            start -= 1;
            used += widths[start];
        }
        if index < start {
            return false;
        }
        let mut drawn = 0;
        for (candidate, width) in widths.iter().enumerate().skip(start) {
            if drawn >= available {
                return false;
            }
            if candidate == index {
                return true;
            }
            drawn = drawn.saturating_add(*width);
        }
        false
    }

    pub fn kitty_frame(&mut self) -> Option<Vec<u8>> {
        if self.bookmark_library {
            if self.kitty_graphics && !self.kitty_hidden_by_bookmarks {
                self.kitty_hidden_by_bookmarks = true;
                for tab in &mut self.tabs {
                    tab.page.images.terminal_reset();
                }
                return Some(crate::image::delete_all_images().to_vec());
            }
            return None;
        }
        if self.kitty_hidden_by_bookmarks {
            self.kitty_hidden_by_bookmarks = false;
            return self.tabs[self.active].page.kitty_frame_with_offset(1);
        }
        let active = self.tabs[self.active].id;
        if self.kitty_graphics && self.kitty_tab != Some(active) {
            self.kitty_tab = Some(active);
            for tab in &mut self.tabs {
                tab.page.images.terminal_reset();
            }
            let mut bytes = crate::image::delete_all_images().to_vec();
            if let Some(next) = self.tabs[self.active].page.kitty_frame_with_offset(1) {
                bytes.extend(next);
            }
            return Some(bytes);
        }
        self.tabs[self.active].page.kitty_frame_with_offset(1)
    }

    pub fn record_frame(&mut self, duration: Duration) {
        self.tabs[self.active].page.record_frame(duration);
    }

    pub fn timings(&self) -> &Timings {
        self.tabs[self.active].page.timings()
    }
}

impl App {
    fn accepts_browser_chrome_keys(&self) -> bool {
        matches!(self.mode, Mode::Browse) && self.hint.is_none()
    }

    fn tab_label(&self) -> String {
        if let Some(dom) = &self.dom {
            for raw in 0..dom.node_count() {
                let id = NodeId(raw as u32);
                if matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "title") {
                    let mut title = String::new();
                    collect_text(dom, id, &mut title);
                    let title = sanitize_title(&title);
                    if !title.is_empty() {
                        return title;
                    }
                }
            }
        }
        let Some(url) = self.current_url() else {
            return "new tab".into();
        };
        reqwest::Url::parse(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| sanitize_title(&url))
    }
}

fn bounded(mut message: String) -> String {
    let at = message
        .char_indices()
        .map(|(at, _)| at)
        .take_while(|at| *at <= 1_024)
        .last()
        .unwrap_or(0);
    if message.len() > 1_024 {
        message.truncate(at);
    }
    message
}

fn clip_cells(value: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut used: usize = 0;
    let mut clipped = false;
    for ch in value.chars() {
        let width = ch.width().unwrap_or(0);
        if used.saturating_add(width) > limit {
            clipped = true;
            break;
        }
        out.push(ch);
        used += width;
    }
    if clipped && limit > 0 {
        while used >= limit {
            let Some(ch) = out.pop() else { break };
            used = used.saturating_sub(ch.width().unwrap_or(0));
        }
        out.push('…');
    }
    out
}

fn collect_text(dom: &Dom, node: NodeId, out: &mut String) {
    for child in dom.children(node) {
        match &dom.node(child).data {
            NodeData::Text(text) => out.push_str(text),
            _ => collect_text(dom, child, out),
        }
    }
}

/// A scroll outcome: dirty exactly when the offset moved, so a scroll at the
/// limit is not a dead redraw.
fn moved(changed: bool) -> Effect {
    Effect {
        dirty: changed,
        ..Effect::default()
    }
}

/// Whole kilobytes, rounded up so any progress at all reads as `1 KB`, not a
/// dishonest `0 KB`.
fn kb(bytes: u64) -> u64 {
    bytes.div_ceil(1024)
}

/// Re-apply computed colours/attrs onto an existing layout tree without
/// changing geometry — the hover path's "repaint without relayout".
fn recolour_tree(tree: &mut LayoutTree, styles: &Styles) {
    for b in &mut tree.boxes {
        let Some(node) = b.node else {
            continue;
        };
        let computed = styles.get(node).clone();
        b.computed = computed.clone();
        if b.kind == BoxKind::Text {
            b.term_style = layout::term_style(&computed);
        }
    }
}

/// Same document for history / navigation: ignore the fragment. Pure `#foo`
/// links must not push history or re-fetch (M6 review).
fn same_document(a: &str, b: &str) -> bool {
    fn strip(s: &str) -> &str {
        s.split_once('#').map(|(u, _)| u).unwrap_or(s)
    }
    strip(a) == strip(b)
}

/// What the next hop's method is, given this hop's status (M11.11).
///
/// 301/302/303 rewrite POST to GET and drop the body; 307/308 keep POST.
/// GET stays GET. Matching browsers, so a login's 302 does not POST the
/// password at the landing page.
fn rewrite_method(status: u16, method: &net::Method) -> net::Method {
    match (status, method) {
        (301..=303, net::Method::Post { .. }) => net::Method::Get,
        (_, net::Method::Conditional { .. }) => net::Method::Get,
        (_, method) => method.clone(),
    }
}

/// The fragment a URL carries, without its `#`. `Some("")` for a bare `#`,
/// which is the top of the document rather than an absent fragment — the two
/// are different navigations and `href="#"` is nine of the Wikipedia fixture's
/// links.
fn fragment_of(url: &str) -> Option<&str> {
    url.split_once('#').map(|(_, fragment)| fragment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::http_cache::Metadata;
    use crate::term::Color;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn key(code: KeyCode, mods: KeyModifiers) -> Msg {
        Msg::Key(KeyEvent::new(code, mods))
    }

    fn cache_metadata(control: &[&str], etag: Option<&str>) -> Metadata {
        Metadata::bounded(
            control.iter().map(|line| line.to_string()).collect(),
            etag.map(str::to_string),
            None,
            Vec::new(),
            false,
        )
    }

    fn load_cacheable(app: &mut App, id: PageId, url: &str, body: &[u8], metadata: Metadata) {
        app.update(Msg::Loaded {
            id,
            url: url.into(),
            status: 200,
            body: body.to_vec(),
            elapsed: Duration::from_millis(2),
            content_type: Some("text/html".into()),
            set_cookie: Vec::new(),
            metadata,
        });
    }

    fn apply_cached_effect(app: &mut App, effect: Effect) {
        let (id, url, response, elapsed) = effect.cached.expect("expected a cache hit");
        let (tx, rx) = mpsc::channel();
        net::spawn_cached(id, url, response, elapsed, tx);
        while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
            let parsed = matches!(msg, Msg::Parsed { .. });
            app.update(msg);
            if parsed {
                break;
            }
        }
    }

    fn settle_document_effect(app: &mut App, effect: Effect) {
        let (tx, rx) = mpsc::channel();
        fn dispatch(effect: Effect, tx: &mpsc::Sender<Msg>) {
            if let Some((id, request)) = effect.fetch {
                net::spawn_fetch(id, request, tx.clone());
            }
            if let Some((id, url, response, elapsed)) = effect.cached {
                net::spawn_cached(id, url, response, elapsed, tx.clone());
            }
        }
        dispatch(effect, &tx);
        loop {
            let msg = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("document worker did not settle");
            let parsed = matches!(msg, Msg::Parsed { .. });
            let next = app.update(msg);
            let run_scripts = next.run_scripts;
            dispatch(next, &tx);
            if parsed {
                if let Some(id) = run_scripts {
                    app.update(Msg::RunScripts { id });
                }
                return;
            }
        }
    }

    #[test]
    fn loopback_a_b_back_skips_a_network_request_and_reload_validates() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = seen.clone();
        thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let request = String::from_utf8(request).unwrap();
                server_seen.lock().unwrap().push(request.clone());
                let response = if request.starts_with("GET /b ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 1\r\nConnection: close\r\n\r\nb".to_string()
                } else if request.contains("if-none-match: \"one\"") {
                    "HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=60\r\nETag: \"one\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: max-age=60\r\nETag: \"one\"\r\nContent-Length: 1\r\nConnection: close\r\n\r\na".to_string()
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let mut app = App::new(40, 8);
        let a = format!("http://{addr}/a");
        let b = format!("http://{addr}/b");
        let first = app.start_navigation(a.clone());
        settle_document_effect(&mut app, first);
        let second = app.navigate(b, true);
        settle_document_effect(&mut app, second);
        let back = app.history_go(true);
        assert!(back.fetch.is_none(), "history hit started a network worker");
        let hit_started = Instant::now();
        settle_document_effect(&mut app, back);
        let hit_elapsed = hit_started.elapsed();
        assert_eq!(seen.lock().unwrap().len(), 2, "back reached the server");

        let reload = app.reload();
        let revalidation_started = Instant::now();
        settle_document_effect(&mut app, reload);
        let revalidation_elapsed = revalidation_started.elapsed();
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("GET /a "), "{}", requests[2]);
        assert!(
            requests[2].contains("if-none-match: \"one\""),
            "{}",
            requests[2]
        );
        assert_eq!(app.timings.cache, Some(timing::CacheOutcome::Revalidated));
        eprintln!(
            "local fresh history pipeline: {:?}; local 304 roundtrip + pipeline: {:?}",
            hit_elapsed, revalidation_elapsed
        );
    }

    #[test]
    #[ignore]
    fn measure_uncached_ladder_load_with_and_without_cache_planning() {
        fn load_once(html: &str, url: &str, planned: bool) -> Duration {
            let mut app = App::new(80, 24);
            let started = Instant::now();
            let id = if planned {
                app.start_navigation(url.into()).fetch.unwrap().0
            } else {
                app.start_fetch(url.into())
            };
            app.update(Msg::Loaded {
                id,
                url: url.into(),
                status: 200,
                body: html.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                content_type: Some("text/html".into()),
                set_cookie: Vec::new(),
                metadata: Default::default(),
            });
            app.update(Msg::Parsed {
                id,
                dom: crate::html::parse(html),
                elapsed: Duration::ZERO,
            });
            started.elapsed()
        }

        for (label, url, html) in [
            (
                "HN",
                "https://news.ycombinator.com/news",
                include_str!("../../tests/fixtures/news.ycombinator.com.html"),
            ),
            (
                "Wikipedia",
                "https://en.wikipedia.org/wiki/Cat",
                include_str!("../../tests/fixtures/en.wikipedia.org.html"),
            ),
        ] {
            let mut baseline = Vec::new();
            let mut planned = Vec::new();
            for round in 0..8 {
                if round % 2 == 0 {
                    baseline.push(load_once(html, url, false));
                    planned.push(load_once(html, url, true));
                } else {
                    planned.push(load_once(html, url, true));
                    baseline.push(load_once(html, url, false));
                }
            }
            baseline.sort_unstable();
            planned.sort_unstable();
            eprintln!(
                "{label} uncached load: baseline {:?}, with cache planning {:?}",
                baseline[baseline.len() / 2],
                planned[planned.len() / 2]
            );
        }
    }

    #[test]
    fn fresh_history_hit_mints_a_generation_rebuilds_and_restores_scroll_without_fetch() {
        let mut app = App::new(40, 6);
        let a = "http://cache.test/a";
        let first = app.start_navigation(a.into());
        let first_id = first.fetch.unwrap().0;
        let body = (0..30)
            .map(|n| format!("<div>row {n}</div>"))
            .collect::<String>();
        load_cacheable(
            &mut app,
            first_id,
            a,
            body.as_bytes(),
            cache_metadata(&["max-age=60"], Some("\"a1\"")),
        );
        app.update(Msg::Parsed {
            id: first_id,
            dom: crate::html::parse(&body),
            elapsed: Duration::ZERO,
        });
        assert!(app.viewport.scroll_to_offset(7));

        let b = app.navigate("http://cache.test/b".into(), true);
        let b_id = b.fetch.unwrap().0;
        load(&mut app, b_id, b"b".to_vec());
        let before = (app.layouts, app.styles_run, app.paints);

        let back = app.history_go(true);
        assert!(back.fetch.is_none(), "fresh A started network work");
        let hit_id = back.cached.as_ref().unwrap().0;
        assert_ne!(hit_id, first_id, "cache reuse resurrected a generation");
        apply_cached_effect(&mut app, back);
        assert_eq!(app.viewport.offset(), 7);
        assert_eq!(
            (app.layouts, app.styles_run, app.paints),
            (before.0 + 1, before.1 + 1, before.2 + 1)
        );
        assert_eq!(app.timings.cache, Some(timing::CacheOutcome::Hit));
        assert_eq!(app.timings.fetch, None);
    }

    #[test]
    fn stale_etag_revalidates_exactly_and_304_reuses_the_stored_representation() {
        let mut app = App::new(40, 8);
        let a = "http://cache.test/a";
        let first = app.start_navigation(a.into());
        let first_id = first.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            first_id,
            a,
            b"old body",
            cache_metadata(&[], Some("W/\"one\"")),
        );

        let b = app.navigate("http://cache.test/b".into(), true);
        let b_id = b.fetch.unwrap().0;
        load(&mut app, b_id, b"b".to_vec());
        let back = app.history_go(true);
        let (id, request) = back.fetch.expect("stale entry did not validate");
        assert_eq!(
            request.method,
            net::Method::Conditional {
                no_cache: false,
                if_none_match: Some("W/\"one\"".into())
            }
        );
        let reuse = app.update(Msg::Loaded {
            id,
            url: a.into(),
            status: 304,
            body: Vec::new(),
            elapsed: Duration::from_millis(3),
            content_type: None,
            set_cookie: Vec::new(),
            metadata: cache_metadata(&["max-age=60"], Some("\"two\"")),
        });
        apply_cached_effect(&mut app, reuse);
        assert!(matches!(&app.fetch, Fetch::Loaded { body, .. } if body == b"old body"));
        assert_eq!(app.timings.cache, Some(timing::CacheOutcome::Revalidated));
        assert_eq!(app.timings.fetch, Some(Duration::from_millis(3)));

        let reload = app.reload();
        let (_, request) = reload.fetch.expect("reload did not validate");
        assert_eq!(
            request.method,
            net::Method::Conditional {
                no_cache: true,
                if_none_match: Some("\"two\"".into()),
            }
        );
    }

    #[test]
    fn conditional_200_replaces_old_bytes_and_unsolicited_304_is_an_error() {
        let mut app = App::new(40, 8);
        let url = "http://cache.test/a";
        let first = app.start_navigation(url.into());
        let first_id = first.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            first_id,
            url,
            b"old",
            cache_metadata(&["max-age=0"], Some("\"one\"")),
        );
        let reload = app.reload();
        let (id, request) = reload.fetch.expect("stale response did not validate");
        let key = CacheKey::from_request(&request).unwrap();
        app.update(Msg::Loaded {
            id,
            url: url.into(),
            status: 200,
            body: b"new".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: Vec::new(),
            metadata: cache_metadata(&["max-age=60"], Some("\"two\"")),
        });
        let Plan::Hit(replaced) = app.cache.plan(&key, RequestMode::Ordinary, app.cache_now())
        else {
            panic!("replacement was not fresh");
        };
        assert_eq!(replaced.body, b"new");
        assert_eq!(replaced.metadata.etag.as_deref(), Some("\"two\""));

        let id = app.start_fetch("http://cache.test/missing".into());
        app.update(Msg::Loaded {
            id,
            url: "http://cache.test/missing".into(),
            status: 304,
            body: Vec::new(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Metadata::default(),
        });
        assert!(
            matches!(&app.fetch, Fetch::Failed { reason, .. } if reason.contains("without a cached representation"))
        );
    }

    #[test]
    fn cached_script_navigation_builds_a_fresh_host_and_fresh_globals() {
        let mut app = App::new(50, 8);
        let url = "http://cache.test/script";
        let html =
            "<script>window.runs = (window.runs || 0) + 1; console.log(window.runs);</script>";
        let first = app.start_navigation(url.into());
        let first_id = first.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            first_id,
            url,
            html.as_bytes(),
            cache_metadata(&["max-age=60"], None),
        );
        let parsed = app.update(Msg::Parsed {
            id: first_id,
            dom: crate::html::parse(html),
            elapsed: Duration::ZERO,
        });
        app.update(Msg::RunScripts {
            id: parsed.run_scripts.unwrap(),
        });
        assert_eq!(app.console.entries().last().unwrap().text, "1");

        let b = app.navigate("http://cache.test/b".into(), true);
        let b_id = b.fetch.unwrap().0;
        load(&mut app, b_id, b"b".to_vec());
        let back = app.history_go(true);
        assert!(
            app.js_host.is_none(),
            "the prior page host survived navigation"
        );
        settle_document_effect(&mut app, back);
        assert_eq!(
            app.console.entries().last().unwrap().text,
            "1",
            "cached navigation resurrected the old global"
        );
    }

    #[test]
    fn stale_unvalidated_response_does_a_full_get_and_failure_never_serves_stale() {
        let mut app = App::new(40, 8);
        let url = "http://cache.test/a";
        let first = app.start_navigation(url.into());
        let first_id = first.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            first_id,
            url,
            b"stale",
            cache_metadata(&["max-age=0"], None),
        );
        let retry = app.navigate(url.into(), false);
        let (id, request) = retry.fetch.expect("stale bytes were served");
        assert_eq!(request.method, net::Method::Get);
        app.update(Msg::NetError {
            id,
            url: url.into(),
            reason: "connection refused".into(),
        });
        assert!(
            matches!(&app.fetch, Fetch::Failed { reason, .. } if reason == "connection refused")
        );
    }

    #[test]
    fn post_and_set_cookie_responses_are_never_inserted() {
        let mut app = App::new(40, 8);
        let url = "http://cache.test/login";
        let post = app.navigate_with(url.into(), net::Method::Post { body: "x=1".into() }, false);
        let id = post.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            id,
            url,
            b"post",
            cache_metadata(&["max-age=60"], None),
        );
        assert!(app.navigate(url.into(), false).fetch.is_some());

        let id = app.current_fetch.unwrap();
        app.update(Msg::Loaded {
            id,
            url: url.into(),
            status: 200,
            body: b"cookie".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: vec!["sid=one; Path=/".into()],
            metadata: cache_metadata(&["max-age=60"], None),
        });
        assert!(app.navigate(url.into(), false).fetch.is_some());
    }

    #[test]
    fn a_logged_out_entry_cannot_satisfy_the_same_url_after_login() {
        let mut app = App::new(40, 8);
        let a = "http://cache.test/a";
        let first = app.start_navigation(a.into());
        let first_id = first.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            first_id,
            a,
            b"logged out",
            cache_metadata(&["max-age=60"], None),
        );

        let b = app.navigate("http://cache.test/login".into(), true);
        let b_id = b.fetch.unwrap().0;
        app.update(Msg::Loaded {
            id: b_id,
            url: "http://cache.test/login".into(),
            status: 200,
            body: b"logged in".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: vec!["sid=private; Path=/".into()],
            metadata: Default::default(),
        });
        let back = app.history_go(true);
        let (_, request) = back.fetch.expect("logged-out bytes crossed cookie state");
        assert_eq!(request.cookie.as_deref(), Some("sid=private"));
        assert_eq!(request.method, net::Method::Get);
    }

    fn ch(c: char) -> Msg {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn set_browser_page(browser: &mut Browser, index: usize, url: &str, title: &str) {
        browser.tabs[index].page.fetch = Fetch::Loaded {
            url: url.into(),
            status: 200,
            body: Vec::new(),
        };
        browser.tabs[index].page.dom = Some(crate::html::parse(&format!(
            "<title>{title}</title><p>page</p>"
        )));
    }

    #[test]
    fn bookmarks_load_mutate_globally_and_open_through_active_history() {
        let mut browser = Browser::with_bookmark_persistence(60, 8, false, true);
        set_browser_page(&mut browser, 0, "https://a.test/#citation", "  A\n title  ");
        let generation_a = browser.tabs[0].page.fetch_gen;
        let refused = browser.update(ch('a'));
        assert!(refused.bookmark_save.is_none());
        assert_eq!(browser.bookmarks.len(), 0);
        assert_eq!(
            browser.tabs[0].page.status_msg.as_deref(),
            Some("bookmarks are still loading")
        );

        browser.update(Msg::BookmarksLoaded(Ok(Bookmarks::new())));
        let save = browser.update(ch('a')).bookmark_save.unwrap();
        assert_eq!(save.0, 1);
        assert_eq!(save.1[0].url.as_ref(), "https://a.test/#citation");
        assert_eq!(save.1[0].title.as_ref(), "A title");
        browser.update(ch('b'));
        assert!(browser.bookmark_library);
        assert_eq!(
            browser.update(ch('t')),
            Effect::default(),
            "tab key leaked through modal chrome"
        );
        browser.update(ch('b'));

        browser.update(ch('t'));
        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        set_browser_page(&mut browser, 1, "https://b.test/", "B");
        browser.update(ch('b'));
        let effect = browser.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!browser.bookmark_library);
        let opened = effect.fetch.unwrap();
        assert_eq!(opened.1.url, "https://a.test/#citation");
        assert!(browser.tabs[1].page.history.can_back());
        assert_eq!(browser.tabs[0].page.fetch_gen, generation_a);
        assert_eq!(browser.bookmarks.len(), 1);
    }

    #[test]
    fn bookmark_selection_scroll_delete_and_stale_save_status_are_bounded() {
        let mut browser = Browser::with_bookmark_persistence(24, 5, false, true);
        browser.update(Msg::BookmarksLoaded(Ok(Bookmarks::new())));
        for n in 0..5 {
            set_browser_page(
                &mut browser,
                0,
                &format!("https://example.test/{n}"),
                &format!("猫 {n}"),
            );
            assert!(browser.update(ch('a')).bookmark_save.is_some());
        }
        browser.update(ch('b'));
        browser.update(ch('G'));
        assert_eq!(browser.bookmark_selected, Some(4));
        assert_eq!(browser.bookmark_first_visible, 3);
        browser.update(ch('d'));
        assert_eq!(browser.bookmark_selected, Some(3));
        assert_eq!(browser.bookmarks.len(), 4);

        let newest = browser.bookmark_revision;
        browser.update(Msg::BookmarksSaved {
            revision: newest - 1,
            result: Ok(()),
        });
        assert_eq!(browser.bookmark_status(), "bookmark deleted");
        browser.bookmark_notice = None;
        assert_eq!(browser.bookmark_status(), "saving bookmarks");
        browser.update(Msg::BookmarksSaved {
            revision: newest,
            result: Err("disk failed".into()),
        });
        assert!(browser.bookmark_status().contains("unsaved"));

        for (w, h) in [(0, 0), (1, 1), (2, 2), (24, 5)] {
            browser.update(Msg::Resize(w, h));
            let mut frame = Frame::new(w, h);
            browser.draw(&mut frame);
            if h > 1 && w > 0 {
                assert!(row_text(&frame, 1).starts_with('y'));
            }
        }
    }

    #[test]
    fn bookmark_and_document_startup_messages_are_independent_in_both_orders() {
        for bookmarks_first in [true, false] {
            let mut browser = Browser::with_bookmark_persistence(40, 8, false, true);
            let effect = browser.start_navigation("https://page.test/".into());
            let page = effect.fetch.unwrap().0;
            let mut loaded = Bookmarks::new();
            loaded.add(Arc::from("https://saved.test/"), Arc::from("Saved"));
            if bookmarks_first {
                browser.update(Msg::BookmarksLoaded(Ok(loaded)));
                browser.update(Msg::Loading {
                    id: page,
                    bytes_so_far: 9,
                });
            } else {
                browser.update(Msg::Loading {
                    id: page,
                    bytes_so_far: 9,
                });
                browser.update(Msg::BookmarksLoaded(Ok(loaded)));
            }
            assert_eq!(
                browser.bookmarks.records()[0].url.as_ref(),
                "https://saved.test/"
            );
            assert_eq!(browser.tabs[0].page.current_fetch, Some(page));
            assert!(matches!(
                browser.tabs[0].page.fetch,
                Fetch::Loading {
                    bytes_so_far: 9,
                    ..
                }
            ));
        }
    }

    #[test]
    fn bookmark_modal_actions_preserve_the_page_and_block_mouse_input() {
        let mut browser = Browser::new(40, 7);
        set_browser_page(&mut browser, 0, "https://page.test/", "Page");
        browser
            .bookmarks
            .add(Arc::from("https://one.test/"), Arc::from("One"));
        browser
            .bookmarks
            .add(Arc::from("https://two.test/"), Arc::from("Two"));
        let page = &mut browser.tabs[0].page;
        page.surface = Surface::Dom;
        page.timing_visible = true;
        page.focus = Some(NodeId(7));
        page.search = Some(SearchSession {
            query: "needle".into(),
            matches: Vec::new(),
            current: 0,
        });
        page.layouts = 11;
        page.styles_run = 12;
        page.paints = 13;
        let before = (
            page.surface,
            page.timing_visible,
            page.focus,
            page.search.as_ref().unwrap().query.clone(),
            page.layouts,
            page.styles_run,
            page.paints,
            page.fetch_gen,
        );

        browser.update(ch('b'));
        browser.update(ch('j'));
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(browser.update(Msg::Mouse(mouse)), Effect::default());
        browser.update(ch('b'));
        let page = &browser.tabs[0].page;
        let after = (
            page.surface,
            page.timing_visible,
            page.focus,
            page.search.as_ref().unwrap().query.clone(),
            page.layouts,
            page.styles_run,
            page.paints,
            page.fetch_gen,
        );
        assert_eq!(after, before);
    }

    #[test]
    fn unavailable_persistence_keeps_mutations_in_memory_without_save_work() {
        let mut browser = Browser::with_bookmark_persistence(40, 7, false, false);
        browser.update(Msg::BookmarksLoaded(Ok(Bookmarks::new())));
        set_browser_page(&mut browser, 0, "https://page.test/", "Page");
        let effect = browser.update(ch('a'));
        assert!(effect.bookmark_save.is_none());
        assert_eq!(browser.bookmarks.len(), 1);
        browser.update(ch('b'));
        assert!(browser.bookmark_status().contains("unavailable"));
    }

    #[test]
    fn tab_operations_are_ordered_wrapping_bounded_and_never_reuse_ids() {
        let mut browser = Browser::new(40, 8);
        let first = browser.tabs[0].id;
        browser.update(ch('t'));
        let second = browser.tabs[1].id;
        assert_ne!(first, second);
        assert_eq!(browser.active, 1);
        assert!(matches!(browser.tabs[1].page.mode, Mode::UrlInput { .. }));

        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        browser.update(ch('g'));
        browser.update(ch('t'));
        assert_eq!(browser.active, 0, "next did not wrap");
        browser.update(ch('g'));
        browser.update(ch('T'));
        assert_eq!(browser.active, 1, "previous did not wrap");

        let closed = browser.tabs[1].id;
        let effect = browser.update(ch('x'));
        assert_eq!(browser.active, 0);
        assert!(matches!(
            effect.timers.as_slice(),
            [TimerRequest::CancelTab { tab }] if *tab == closed
        ));
        browser.update(ch('t'));
        assert!(browser.tabs[1].id.0 > closed.0, "a closed id was reused");
        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));

        while browser.tabs.len() < MAX_TABS {
            browser.update(ch('t'));
            browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        }
        let active = browser.active;
        let ids: Vec<_> = browser.tabs.iter().map(|tab| tab.id).collect();
        browser.update(ch('t'));
        assert_eq!(browser.active, active);
        assert_eq!(
            browser.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            browser.tabs[active].page.status_msg.as_deref(),
            Some("tab limit (16)")
        );
    }

    #[test]
    fn closing_the_only_tab_replaces_it_with_a_new_blank_identity() {
        let mut browser = Browser::new(20, 4);
        let old = browser.tabs[0].id;
        browser.update(ch('x'));
        assert_eq!(browser.tabs.len(), 1);
        assert_ne!(browser.tabs[0].id, old);
        assert!(matches!(browser.tabs[0].page.fetch, Fetch::Idle));
    }

    #[test]
    fn async_messages_route_by_tab_before_the_generation_guard() {
        let mut browser = Browser::new(30, 6);
        let a = browser
            .start_navigation("https://a.test/".into())
            .fetch
            .unwrap()
            .0;
        browser.update(ch('t'));
        let b = browser
            .start_navigation("https://b.test/".into())
            .fetch
            .unwrap()
            .0;
        assert_eq!(
            a.generation, b.generation,
            "the test needs colliding generations"
        );
        assert_ne!(a.tab, b.tab);

        let effect = browser.update(Msg::Loading {
            id: a,
            bytes_so_far: 7,
        });
        assert!(!effect.dirty, "a background progress message repainted");
        assert!(matches!(
            browser.tabs[0].page.fetch,
            Fetch::Loading {
                bytes_so_far: 7,
                ..
            }
        ));
        assert!(matches!(
            browser.tabs[1].page.fetch,
            Fetch::Loading {
                bytes_so_far: 0,
                ..
            }
        ));

        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        browser.update(ch('x'));
        assert_eq!(
            browser.update(Msg::Loading {
                id: b,
                bytes_so_far: 9
            }),
            Effect::default()
        );
    }

    #[test]
    fn interleaved_document_chains_settle_in_their_addressed_tabs() {
        let mut browser = Browser::new(30, 6);
        let a = browser
            .start_navigation("https://a.test/start".into())
            .fetch
            .unwrap()
            .0;
        browser.update(ch('t'));
        let b = browser
            .start_navigation("https://b.test/final".into())
            .fetch
            .unwrap()
            .0;
        assert_eq!(a.generation, b.generation);

        // B finishes first while selected. A then redirects and finishes in
        // the background; neither chain borrows the active index as identity.
        browser.update(Msg::Loaded {
            id: b,
            url: "https://b.test/final".into(),
            status: 200,
            body: b"<p>body b</p>".to_vec(),
            elapsed: Duration::from_millis(2),
            content_type: Some("text/html".into()),
            set_cookie: vec![],
            metadata: Metadata::default(),
        });
        browser.update(Msg::Parsed {
            id: b,
            dom: crate::html::parse("<p>body b</p>"),
            elapsed: Duration::ZERO,
        });
        let hop = browser.update(Msg::Redirect {
            id: a,
            url: "https://a.test/start".into(),
            to: "https://a.test/final".into(),
            status: 302,
            elapsed: Duration::from_millis(3),
            set_cookie: vec![],
        });
        let (hop_id, request) = hop.fetch.expect("A redirect lost its next request");
        assert_eq!(hop_id, a);
        assert_eq!(request.url, "https://a.test/final");
        assert_eq!(request.method, net::Method::Get);
        assert!(!hop.dirty, "a background redirect repainted B");
        browser.update(Msg::Loaded {
            id: a,
            url: "https://a.test/final".into(),
            status: 200,
            body: b"<p>body a</p>".to_vec(),
            elapsed: Duration::from_millis(5),
            content_type: Some("text/html".into()),
            set_cookie: vec![],
            metadata: Metadata::default(),
        });
        browser.update(Msg::Parsed {
            id: a,
            dom: crate::html::parse("<p>body a</p>"),
            elapsed: Duration::ZERO,
        });

        assert!(matches!(
            &browser.tabs[0].page.fetch,
            Fetch::Loaded { url, body, .. }
                if url == "https://a.test/final" && body == b"<p>body a</p>"
        ));
        assert!(matches!(
            &browser.tabs[1].page.fetch,
            Fetch::Loaded { url, body, .. }
                if url == "https://b.test/final" && body == b"<p>body b</p>"
        ));
        assert_eq!(browser.tabs[0].page.hops.count, 1);
        assert_eq!(browser.tabs[1].page.hops.count, 0);
    }

    #[test]
    fn the_strip_is_cell_safe_at_tiny_sizes_and_sanitizes_titles() {
        assert_eq!(
            sanitize_title("  one\n\t two\u{1b}three  "),
            "one two�three"
        );
        assert!(sanitize_title(&"猫".repeat(300)).len() <= MAX_TITLE_BYTES);
        assert!(sanitize_title(&("x".repeat(511) + "\u{1b}")).len() <= MAX_TITLE_BYTES);
        for (w, h) in [(0, 0), (1, 1), (1, 2), (8, 2)] {
            let browser = Browser::new(w, h);
            let mut frame = Frame::new(w, h);
            browser.draw(&mut frame);
        }

        let mut browser = Browser::new(12, 3);
        browser.tabs[0].page.fetch = Fetch::Loaded {
            url: "猫猫猫猫猫猫".into(),
            status: 200,
            body: vec![],
        };
        while browser.tabs.len() < MAX_TABS {
            browser.update(ch('t'));
            browser.tabs[browser.active].page.fetch = Fetch::Loaded {
                url: "猫猫猫猫猫猫".into(),
                status: 200,
                body: vec![],
            };
            browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        }
        let mut frame = Frame::new(12, 3);
        browser.draw(&mut frame);
        assert!(frame.get(0, 0).style().attrs.contains(Attrs::REVERSE));
    }

    #[test]
    fn switching_tabs_resets_terminal_global_kitty_placements_once() {
        let mut browser = Browser::with_caps(20, 4, true);
        assert!(browser.kitty_frame().is_none());
        browser.update(ch('t'));
        assert_eq!(
            browser.kitty_frame().as_deref(),
            Some(crate::image::delete_all_images())
        );
        assert!(browser.kitty_frame().is_none());
    }

    #[test]
    fn cookies_and_document_cache_are_shared_without_sharing_page_state() {
        let mut cookies = Browser::new(40, 8);
        let a = cookies
            .start_navigation("https://same.test/a".into())
            .fetch
            .unwrap()
            .0;
        cookies.update(Msg::Loaded {
            id: a,
            url: "https://same.test/a".into(),
            status: 200,
            body: b"a".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: vec!["sid=shared; Path=/".into()],
            metadata: Metadata::default(),
        });
        cookies.update(ch('t'));
        let request = cookies
            .start_navigation("https://same.test/b".into())
            .fetch
            .unwrap()
            .1;
        assert_eq!(request.cookie.as_deref(), Some("sid=shared"));

        let mut cached = Browser::new(40, 8);
        let first = cached
            .start_navigation("https://cache.test/page".into())
            .fetch
            .unwrap()
            .0;
        cached.update(Msg::Loaded {
            id: first,
            url: "https://cache.test/page".into(),
            status: 200,
            body: b"cached body".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: vec![],
            metadata: Metadata::bounded(vec!["max-age=600".into()], None, None, vec![], false),
        });
        cached.update(ch('t'));
        let hit = cached.start_navigation("https://cache.test/page".into());
        assert!(hit.fetch.is_none());
        let (second, _, representation, _) = hit.cached.expect("cache was private to tab A");
        assert_ne!(first.tab, second.tab);
        assert_eq!(representation.body, b"cached body");
        assert!(
            cached.tabs[1].page.dom.is_none(),
            "a cache hit shared a DOM"
        );
    }

    #[test]
    fn local_and_decoded_images_are_shared_while_session_storage_is_tab_local() {
        use crate::js::storage::Area;
        let mut browser = Browser::new(30, 6);
        let origin = "https://same.test";
        browser.tabs[0]
            .page
            .storage
            .set(origin, Area::Local, "theme", "dark");
        browser.tabs[0]
            .page
            .storage
            .set(origin, Area::Session, "draft", "a");
        browser.tabs[0].page.images.insert(
            "https://same.test/p.png".into(),
            crate::image::DecodedImage {
                width: 1,
                height: 1,
                rgba: std::sync::Arc::from([0, 0, 0, 255]),
            },
        );
        browser.update(ch('t'));
        assert_eq!(
            browser.tabs[1]
                .page
                .storage
                .get(origin, Area::Local, "theme")
                .as_deref(),
            Some("dark")
        );
        assert_eq!(
            browser.tabs[1]
                .page
                .storage
                .get(origin, Area::Session, "draft"),
            None
        );
        assert!(
            browser.tabs[1]
                .page
                .images
                .cache_contains("https://same.test/p.png")
        );
        let image_page = browser
            .start_navigation("https://same.test/image-page".into())
            .fetch
            .unwrap()
            .0;
        browser.update(Msg::Loaded {
            id: image_page,
            url: "https://same.test/image-page".into(),
            status: 200,
            body: b"<img src=/p.png>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: vec![],
            metadata: Metadata::default(),
        });
        let image_hit = browser.update(Msg::Parsed {
            id: image_page,
            dom: crate::html::parse("<img src=/p.png>"),
            elapsed: Duration::ZERO,
        });
        assert!(
            image_hit.images.is_empty(),
            "a decoded hit scheduled a second network/decode worker"
        );
        assert!(browser.tabs[0].page.images.page_imgs().is_empty());
        assert_eq!(browser.tabs[1].page.images.page_imgs().len(), 1);

        browser.tabs[1]
            .page
            .storage
            .set(origin, Area::Session, "draft", "b");
        let _ = browser.tabs[1]
            .page
            .start_navigation("https://same.test/next".into());
        assert_eq!(
            browser.tabs[1]
                .page
                .storage
                .get(origin, Area::Session, "draft")
                .as_deref(),
            Some("b")
        );
    }

    #[test]
    fn switching_preserves_page_state_and_runs_no_pipeline_stage() {
        fn settle(browser: &mut Browser, url: &str, markup: &str) -> PageId {
            let id = browser.start_navigation(url.into()).fetch.unwrap().0;
            browser.update(Msg::Loaded {
                id,
                url: url.into(),
                status: 200,
                body: markup.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                content_type: Some("text/html".into()),
                set_cookie: vec![],
                metadata: Metadata::default(),
            });
            browser.update(Msg::Parsed {
                id,
                dom: crate::html::parse(markup),
                elapsed: Duration::ZERO,
            });
            id
        }

        let mut browser = Browser::new(30, 8);
        let a_markup = format!(
            "<title>A</title><input value=seed>\
             <script>window.tabState = 41; console.log(window.tabState);</script>{}",
            (0..40)
                .map(|line| format!("<p>retained line {line}</p>"))
                .collect::<String>()
        );
        let a_id = settle(&mut browser, "https://a.test/", &a_markup);
        browser.update(Msg::RunScripts { id: a_id });
        browser.update(key(KeyCode::Tab, KeyModifiers::NONE));
        browser.update(key(KeyCode::Enter, KeyModifiers::NONE));
        browser.update(ch('Z'));
        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        for _ in 0..3 {
            browser.update(ch('j'));
        }
        browser.update(ch('/'));
        for c in "line".chars() {
            browser.update(ch(c));
        }
        browser.update(key(KeyCode::Enter, KeyModifiers::NONE));
        browser.tabs[0].page.surface = Surface::Dom;
        browser.tabs[0].page.timing_visible = true;
        browser.tabs[0]
            .page
            .history
            .push("https://a.test/previous".into(), 2);
        let a_offset = browser.tabs[0].page.viewport.offset();
        let a_focus = browser.tabs[0].page.focus;
        let a_field = browser.tabs[0]
            .page
            .focused_field()
            .expect("input was not retained as focused")
            .1;
        let a_search = browser.tabs[0]
            .page
            .search
            .as_ref()
            .expect("settled search")
            .query
            .clone();
        let a_console = browser.tabs[0].page.console.entries();
        assert!(browser.tabs[0].page.js_host.is_some());
        browser.update(ch('t'));
        settle(&mut browser, "https://b.test/", "<title>B</title><p>b</p>");
        let before: Vec<_> = browser
            .tabs
            .iter()
            .map(|tab| (tab.page.styles_run, tab.page.layouts, tab.page.paints))
            .collect();

        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        browser.update(ch('g'));
        let switched = browser.update(ch('t'));
        assert_eq!(browser.active, 0);
        assert!(switched.fetch.is_none() && switched.cached.is_none());
        assert!(switched.documents.is_empty());
        assert_eq!(browser.tabs[0].page.surface, Surface::Dom);
        assert!(browser.tabs[0].page.timing_visible);
        assert_eq!(browser.tabs[0].page.viewport.offset(), a_offset);
        assert_eq!(browser.tabs[0].page.focus, a_focus);
        assert_eq!(browser.tabs[0].page.focused_field().unwrap().1, a_field);
        assert_eq!(a_field, "seedZ");
        assert_eq!(
            browser.tabs[0].page.search.as_ref().unwrap().query,
            a_search
        );
        assert_eq!(a_search, "line");
        assert_eq!(browser.tabs[0].page.console.entries(), a_console);
        assert_eq!(a_console.last().unwrap().text, "41");
        assert!(browser.tabs[0].page.js_host.is_some());
        assert!(browser.tabs[0].page.history.can_back());
        assert_eq!(
            browser
                .tabs
                .iter()
                .map(|tab| (tab.page.styles_run, tab.page.layouts, tab.page.paints))
                .collect::<Vec<_>>(),
            before,
            "an ordinary switch reran a pipeline stage"
        );

        browser.update(Msg::Resize(35, 10));
        assert_eq!(browser.tabs[0].page.layouts, before[0].1 + 1);
        assert_eq!(browser.tabs[1].page.layouts, before[1].1);
        browser.update(ch('g'));
        browser.update(ch('t'));
        assert_eq!(browser.tabs[1].page.layouts, before[1].1 + 1);
        let after_deferred = browser.tabs[1].page.layouts;
        browser.update(ch('g'));
        browser.update(ch('T'));
        browser.update(ch('g'));
        browser.update(ch('t'));
        assert_eq!(browser.tabs[1].page.layouts, after_deferred);
    }

    /// Release-only measurement for M11.21's hot path. The ignored shape
    /// keeps ordinary debug test runs deterministic while leaving the exact
    /// HN/Wikipedia interleaving reproducible with:
    /// `cargo test --release measure_settled_tab_switches -- --ignored --nocapture`.
    #[test]
    #[ignore = "release performance measurement"]
    fn measure_settled_tab_switches() {
        fn settle(browser: &mut Browser, url: &str, markup: &str) {
            let id = browser.start_navigation(url.into()).fetch.unwrap().0;
            browser.update(Msg::Loaded {
                id,
                url: url.into(),
                status: 200,
                body: markup.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                content_type: Some("text/html".into()),
                set_cookie: vec![],
                metadata: Metadata::default(),
            });
            browser.update(Msg::Parsed {
                id,
                dom: crate::html::parse(markup),
                elapsed: Duration::ZERO,
            });
        }

        let hn = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/news.ycombinator.com.html"
        ));
        let wikipedia = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ))
        .replace("<script", "<script type=\"text/x-not-run\"");
        let mut browser = Browser::new(120, 35);
        settle(&mut browser, "https://news.ycombinator.com/", hn);
        browser.update(ch('t'));
        settle(
            &mut browser,
            "https://en.wikipedia.org/wiki/Rust",
            &wikipedia,
        );
        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));

        const ROUNDS: u32 = 200;
        let mut frame = Frame::new(120, 35);
        let started = Instant::now();
        for _ in 0..ROUNDS {
            browser.update(ch('g'));
            let effect = browser.update(ch('t'));
            assert!(effect.dirty);
            browser.draw(&mut frame);
        }
        let elapsed = started.elapsed();
        let mean = elapsed / ROUNDS;
        eprintln!(
            "M11.21 settled HN/Wikipedia switch + cached-frame draw: \
             {ROUNDS} interleaved rounds in {elapsed:?}, mean {mean:?}"
        );
        assert!(
            mean < Duration::from_millis(10),
            "settled tab switch exceeded PLAN.md's 10 ms input budget: {mean:?}"
        );

        while browser.tabs.len() < MAX_TABS {
            browser.update(ch('t'));
            browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        }
        assert_eq!(browser.tabs.len(), MAX_TABS);
        eprintln!(
            "M11.21 16-tab idle: no Browser poll/tick exists; the event loop remains on blocking recv"
        );
    }

    /// Reproducible M11.22 release measurement. The worker has no path, so the
    /// timed submission includes only the UI-side snapshot and short lock,
    /// never filesystem latency.
    #[test]
    #[ignore = "release performance measurement"]
    fn measure_full_bookmark_library_inputs() {
        fn settle(browser: &mut Browser, url: &str, markup: &str) {
            let id = browser.start_navigation(url.into()).fetch.unwrap().0;
            browser.update(Msg::Loaded {
                id,
                url: url.into(),
                status: 200,
                body: markup.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                content_type: Some("text/html".into()),
                set_cookie: vec![],
                metadata: Metadata::default(),
            });
            browser.update(Msg::Parsed {
                id,
                dom: crate::html::parse(markup),
                elapsed: Duration::ZERO,
            });
        }

        let hn = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/news.ycombinator.com.html"
        ));
        let wikipedia = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        let mut browser = Browser::with_bookmark_persistence(120, 35, false, true);
        browser.update(Msg::BookmarksLoaded(Ok(Bookmarks::new())));
        settle(&mut browser, "https://news.ycombinator.com/", hn);
        browser.update(ch('t'));
        settle(&mut browser, "https://en.wikipedia.org/wiki/Cat", wikipedia);
        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        for n in 0..MAX_BOOKMARKS {
            assert_eq!(
                browser.bookmarks.add(
                    Arc::from(format!("https://saved.test/{n}#cat")),
                    Arc::from(format!("保存した猫 {n}"))
                ),
                AddResult::Added
            );
        }
        browser.bookmark_selected = Some(0);
        let counters: Vec<_> = browser
            .tabs
            .iter()
            .map(|tab| (tab.page.layouts, tab.page.styles_run, tab.page.paints))
            .collect();
        let mut frame = Frame::new(120, 35);

        const DRAW_ROUNDS: u32 = 200;
        let draw_started = Instant::now();
        for _ in 0..DRAW_ROUNDS {
            browser.update(ch('b'));
            browser.draw(&mut frame);
            browser.update(ch('j'));
            browser.draw(&mut frame);
            browser.update(ch('b'));
        }
        let draw_mean = draw_started.elapsed() / (DRAW_ROUNDS * 3);

        let (event_tx, event_rx) = mpsc::channel();
        let worker = crate::browser::bookmark_worker::BookmarkWorker::spawn(None, event_tx);
        let _ = event_rx.recv().unwrap();
        const EDIT_ROUNDS: u32 = 200;
        let urls: Vec<String> = (0..EDIT_ROUNDS)
            .map(|n| format!("https://new.test/{n}"))
            .collect();
        let edit_started = Instant::now();
        for url in urls {
            browser.update(ch('b'));
            browser.update(ch('G'));
            let deleted = browser.update(ch('d')).bookmark_save.unwrap();
            worker.submit(deleted.0, deleted.1);
            browser.update(ch('b'));
            browser.tabs[browser.active].page.fetch = Fetch::Loaded {
                url,
                status: 200,
                body: Vec::new(),
            };
            let added = browser.update(ch('a')).bookmark_save.unwrap();
            worker.submit(added.0, added.1);
        }
        let edit_mean = edit_started.elapsed() / (EDIT_ROUNDS * 2);
        worker.shutdown();
        eprintln!(
            "M11.22 1,024-record CJK library: open/move/close + draw mean {draw_mean:?}; add/delete snapshot submission mean {edit_mean:?}"
        );
        assert!(draw_mean < Duration::from_millis(10));
        assert!(edit_mean < Duration::from_millis(10));
        assert_eq!(
            browser
                .tabs
                .iter()
                .map(|tab| (tab.page.layouts, tab.page.styles_run, tab.page.paints))
                .collect::<Vec<_>>(),
            counters
        );
    }

    #[test]
    fn a_background_parse_after_resize_uses_the_new_geometry_once() {
        let mut browser = Browser::new(30, 8);
        browser.update(ch('t'));
        let background = browser
            .start_navigation("https://b.test/".into())
            .fetch
            .unwrap()
            .0;
        browser.update(key(KeyCode::Esc, KeyModifiers::NONE));
        browser.update(ch('g'));
        browser.update(ch('t'));
        assert_eq!(browser.active, 0);
        browser.update(Msg::Resize(44, 12));
        browser.update(Msg::Loaded {
            id: background,
            url: "https://b.test/".into(),
            status: 200,
            body: b"<p>background</p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: vec![],
            metadata: Metadata::default(),
        });
        browser.update(Msg::Parsed {
            id: background,
            dom: crate::html::parse("<p>background</p>"),
            elapsed: Duration::ZERO,
        });
        assert_eq!(browser.tabs[1].page.size, (44, 11));
        assert_eq!(browser.tabs[1].page.layouts, 1);
        assert!(!browser.tabs[1].stale_size);
        browser.update(ch('g'));
        browser.update(ch('t'));
        assert_eq!(browser.tabs[1].page.layouts, 1);
    }

    #[test]
    fn quit_keys_report_quit() {
        let mut app = App::new(80, 24);
        assert!(app.update(ch('q')).quit);

        let effect = app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(effect.quit);
    }

    #[test]
    fn input_closed_reports_quit() {
        let mut app = App::new(80, 24);
        let effect = app.update(Msg::InputClosed);
        assert!(effect.quit);
        assert!(effect.fetch.is_none());
    }

    #[test]
    fn unbound_keys_are_not_dirty() {
        let mut app = App::new(80, 24);
        // 'z' is bound to nothing in Browse; it must not redraw.
        assert_eq!(app.update(ch('z')), Effect::default());
    }

    #[test]
    fn resize_updates_size_and_requests_redraw() {
        let mut app = App::new(80, 24);
        assert_eq!(app.update(Msg::Resize(120, 40)), redraw());
        assert_eq!(app.size(), (120, 40));
    }

    fn row_text(frame: &Frame, y: u16) -> String {
        (0..frame.width()).map(|x| frame.get(x, y).ch).collect()
    }

    // ---- viewport wiring --------------------------------------------------

    fn body(lines: usize) -> Vec<u8> {
        (0..lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }

    fn load(app: &mut App, id: PageId, body: Vec<u8>) -> Effect {
        app.update(Msg::Loaded {
            id,
            url: "http://final/".into(),
            status: 200,
            body,
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        })
    }

    // ---- stylesheet sources (M4.3) ----------------------------------------

    /// Load and parse `html` as one page, returning the `Parsed` effect (which
    /// carries the linked sheets the loop would spawn workers for).
    fn open_page(app: &mut App, html_src: &str) -> (PageId, Effect) {
        let id = app.start_fetch("http://site.test/dir/page".into());
        app.update(Msg::Loaded {
            id,
            url: "http://site.test/dir/page".into(),
            status: 200,
            body: html_src.as_bytes().to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        let effect = app.update(Msg::Parsed {
            id,
            dom: crate::html::parse(html_src),
            elapsed: Duration::ZERO,
        });
        (id, effect)
    }

    /// The computed colour of the first element with `tag`, from the styled
    /// tree `App` holds. Nothing paints from it until M4.4; this is how the
    /// cascade is observed until then.
    fn computed_color(app: &App, tag: &str) -> crate::style::values::ColorValue {
        let dom = app.dom.as_ref().expect("page must be parsed");
        let styles = app.styles.as_ref().expect("page must be styled");
        let mut stack = vec![dom.root];
        let mut best: Option<crate::dom::NodeId> = None;
        while let Some(id) = stack.pop() {
            if matches!(&dom.node(id).data, crate::dom::NodeData::Element { tag: t, .. } if t == tag)
            {
                best = Some(best.map_or(id, |b| if id.0 < b.0 { id } else { b }));
            }
            stack.extend(dom.children(id));
        }
        styles.get(best.expect("tag not in the page")).color
    }

    fn sheet(css: &str) -> Option<crate::css::Stylesheet> {
        Some(crate::css::parse(css))
    }

    const RED: crate::style::values::ColorValue = crate::style::values::ColorValue::Rgb(255, 0, 0);
    const BLUE: crate::style::values::ColorValue = crate::style::values::ColorValue::Rgb(0, 0, 255);

    #[test]
    fn a_parse_asks_the_loop_for_every_linked_sheet_and_shows_the_page_anyway() {
        let mut app = App::new(40, 10);
        let (id, effect) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='a.css'><link rel=stylesheet href='/b.css'></head>\
             <body><p>hello</p></body>",
        );
        // Both links go out in one turn, resolved against the page URL — the
        // loop spawns a worker each, so they run in parallel.
        assert_eq!(
            effect.sheets,
            vec![
                (id, 0, net::Request::bare("http://site.test/dir/a.css")),
                (id, 1, net::Request::bare("http://site.test/b.css")),
            ]
        );
        // And the page is on screen already: nothing waited for a round trip.
        assert!(effect.dirty);
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        assert!(
            (0..9).any(|y| row_text(&frame, y).contains("hello")),
            "the page must render before its stylesheets arrive"
        );
        // Two pending slots, styled with what exists so far (the UA sheet).
        assert_eq!(app.sheets.len(), 2);
        assert!(app.sheets.iter().all(|s| s.is_none()));
        assert!(app.styles.is_some());
    }

    #[test]
    fn sheets_cascade_in_document_order_however_they_arrive() {
        let mut app = App::new(40, 10);
        let (id, _) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='first.css'><link rel=stylesheet href='second.css'>\
             </head><body><p>hello</p></body>",
        );
        // The *second* sheet in the document arrives first. Equal specificity,
        // so the document's later sheet must win — arrival order deciding this
        // would make the page's appearance depend on the network.
        app.update(Msg::Stylesheet {
            id,
            slot: 1,
            sheet: sheet("p { color: blue }"),
        });
        assert_eq!(computed_color(&app, "p"), BLUE);
        app.update(Msg::Stylesheet {
            id,
            slot: 0,
            sheet: sheet("p { color: red }"),
        });
        assert_eq!(computed_color(&app, "p"), BLUE);
    }

    #[test]
    fn an_arriving_sheet_restyles_and_redraws() {
        let mut app = App::new(40, 10);
        let (id, _) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='x.css'></head><body><a href='/y'>link</a></body>",
        );
        // Before: the UA sheet's link colour.
        assert_eq!(
            computed_color(&app, "a"),
            crate::style::values::ColorValue::Rgb(0x5c, 0x5c, 0xff)
        );
        assert_eq!(
            app.update(Msg::Stylesheet {
                id,
                slot: 0,
                sheet: sheet("a:link { color: red }"),
            }),
            redraw()
        );
        assert_eq!(computed_color(&app, "a"), RED);
    }

    #[test]
    fn a_failed_sheet_settles_its_slot_instead_of_hanging_it() {
        let mut app = App::new(40, 10);
        let (id, _) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='gone.css'></head><body><p>hi</p></body>",
        );
        app.update(Msg::Stylesheet {
            id,
            slot: 0,
            sheet: None,
        });
        // Resolved (not pending), empty, and the page still styles.
        assert_eq!(app.sheets, vec![Some(crate::css::Stylesheet::default())]);
        assert!(app.styles.is_some());
    }

    #[test]
    fn a_sheet_from_a_superseded_page_is_ignored() {
        let mut app = App::new(40, 10);
        let (stale, _) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='x.css'></head><body><p>one</p></body>",
        );
        // Navigate: a new generation, and the old page's sheets go with it.
        let (_, effect) = open_page(&mut app, "<body><p>two</p></body>");
        assert!(effect.sheets.is_empty());
        assert!(app.sheets.is_empty(), "the old page's slots must be gone");

        // The stale worker reports late. It must change nothing — and must not
        // panic on a slot the new page does not have.
        assert_eq!(
            app.update(Msg::Stylesheet {
                id: stale,
                slot: 0,
                sheet: sheet("p { color: red }"),
            }),
            Effect::default()
        );
        assert_eq!(
            computed_color(&app, "p"),
            crate::style::values::ColorValue::Default
        );
    }

    #[test]
    fn a_late_sheet_repaints_the_page_it_arrives_for() {
        // "Render unstyled, then restyle" (UX §3.2) is only real if the new
        // values reach the pixels. The page is on screen with UA styling; the
        // sheet lands; the next frame is different.
        let mut app = App::new(40, 10);
        let (id, _) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='x.css'></head><body><p>plain</p></body>",
        );
        // x=1: the readable column is centred, so column zero is gutter.
        let mut before = Frame::new(40, 10);
        app.draw(&mut before);
        assert_eq!(before.get(1, 0).attrs, Attrs::NONE, "unstyled to start");
        let laid_out = app.layouts;

        assert_eq!(
            app.update(Msg::Stylesheet {
                id,
                slot: 0,
                sheet: sheet("p { font-weight: bold; color: #348 }"),
            }),
            redraw()
        );

        let mut after = Frame::new(40, 10);
        app.draw(&mut after);
        assert!(after.get(1, 0).attrs.contains(Attrs::BOLD));
        assert_eq!(after.get(1, 0).fg, Color::Rgb(0x33, 0x44, 0x88));
        // Exactly one relayout for the sheet — not none (the lines are cached,
        // so nothing would change on screen) and not several.
        assert_eq!(app.layouts, laid_out + 1);
    }

    #[test]
    fn inline_style_blocks_need_no_round_trip() {
        let mut app = App::new(40, 10);
        let (_, effect) = open_page(
            &mut app,
            "<head><style>p { color: red }</style></head><body><p>hi</p></body>",
        );
        // Nothing to fetch, and the colour is already applied on this turn.
        assert!(effect.sheets.is_empty());
        assert_eq!(computed_color(&app, "p"), RED);
    }

    #[test]
    fn loaded_body_is_visible_at_the_top_then_scrolls() {
        let mut app = App::new(20, 6); // page area is 5 rows
        let id = app.start_fetch("http://x/".into());
        assert_eq!(load(&mut app, id, body(50)), redraw());

        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).starts_with("line0"));
        assert!(row_text(&frame, 4).starts_with("line4"));

        // One line down shifts every body row by one.
        assert_eq!(app.update(ch('j')), redraw());
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).starts_with("line1"));

        // `gg` returns to the top.
        assert!(!app.update(ch('g')).dirty); // pending, not dirty
        assert_eq!(app.update(ch('g')), redraw());
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).starts_with("line0"));
    }

    #[test]
    fn scroll_at_the_limit_is_not_dirty() {
        let mut app = App::new(20, 6);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(50));
        // Already at the top: scrolling up changes nothing.
        assert_eq!(app.update(ch('k')), Effect::default());
        // Jump to the bottom, then a further down-scroll is a no-op.
        assert!(
            app.update(key(KeyCode::Char('G'), KeyModifiers::NONE))
                .dirty
        );
        assert_eq!(app.update(ch('j')), Effect::default());
    }

    #[test]
    fn g_then_j_cancels_the_prefix_and_scrolls() {
        let mut app = App::new(20, 6);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(50));

        assert_eq!(app.update(ch('g')), Effect::default()); // pending
        assert_eq!(app.update(ch('j')), redraw()); // j resolves fresh, scrolls
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).starts_with("line1"));
    }

    #[test]
    fn invalid_utf8_body_does_not_panic() {
        let mut app = App::new(20, 6);
        let id = app.start_fetch("http://x/".into());
        // Lone continuation bytes are not valid UTF-8.
        assert_eq!(load(&mut app, id, vec![0xff, 0xfe, b'h', b'i']), redraw());
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame); // must not panic
    }

    #[test]
    fn narrower_resize_rewraps_and_keeps_offset_clamped() {
        let mut app = App::new(20, 6);
        let id = app.start_fetch("http://x/".into());
        // Lines wider than 10 cells so a resize to 10 wraps them.
        let long = ["0123456789ABCDEF"; 10].join("\n").into_bytes();
        load(&mut app, id, long);
        app.update(key(KeyCode::Char('G'), KeyModifiers::NONE)); // to bottom

        app.update(Msg::Resize(10, 6));
        let page = app.page() as usize;
        assert!(
            app.viewport.offset() <= app.viewport.line_count().saturating_sub(page),
            "offset left past the re-wrapped content"
        );
        assert!(app.viewport.line_count() > 10, "resize should add lines");
    }

    // ---- URL bar mode -----------------------------------------------------

    #[test]
    fn o_opens_url_bar_and_typed_chars_append_without_quitting() {
        let mut app = App::new(30, 6);
        assert_eq!(app.update(ch('o')), redraw());
        // `q` types here rather than quitting.
        for c in "qux".chars() {
            assert_eq!(app.update(ch(c)), redraw());
        }
        let mut frame = Frame::new(30, 6);
        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.contains("open: qux"), "row was {row:?}");
        assert!(row.contains(CURSOR), "cursor cell missing: {row:?}");
    }

    #[test]
    fn backspace_deletes_the_last_char() {
        let mut app = App::new(30, 6);
        app.update(ch('o'));
        app.update(ch('a'));
        app.update(ch('b'));
        assert_eq!(
            app.update(key(KeyCode::Backspace, KeyModifiers::NONE)),
            redraw()
        );
        let mut frame = Frame::new(30, 6);
        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("open: a"));
        assert!(!row_text(&frame, 5).contains("open: ab"));
    }

    #[test]
    fn esc_cancels_with_no_fetch() {
        let mut app = App::new(30, 6);
        app.update(ch('o'));
        app.update(ch('x'));
        let effect = app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(effect.dirty);
        assert!(effect.fetch.is_none(), "cancel must not fetch");
        // Back in Browse: the status row, not the URL bar.
        let mut frame = Frame::new(30, 6);
        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("yata"));
    }

    #[test]
    fn enter_commits_a_normalized_url_and_shows_loading() {
        let mut app = App::new(40, 6);
        app.update(ch('o'));
        for c in "danluu.com".chars() {
            app.update(ch(c));
        }
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let (id, url) = effect.fetch.expect("commit must return a fetch");
        assert_eq!(url.url, "https://danluu.com", "scheme defaulting applied");
        assert!(effect.dirty);

        // The row now shows the new fetch loading.
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.contains("loading…"), "row was {row:?}");
        assert!(row.contains("https://danluu.com"), "row was {row:?}");

        // A Loaded for that id lands normally (generation is live).
        assert_eq!(load(&mut app, id, body(3)), redraw());
    }

    #[test]
    fn ctrl_c_quits_from_url_input() {
        let mut app = App::new(30, 6);
        app.update(ch('o'));
        assert!(
            app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .quit
        );
    }

    // ---- M1.4 invariants (unchanged behavior) -----------------------------

    fn dirty() -> Effect {
        redraw()
    }

    fn loaded(id: PageId, status: u16, body_len: usize) -> Msg {
        Msg::Loaded {
            id,
            url: "http://final/".into(),
            status,
            body: vec![b'x'; body_len],
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        }
    }

    #[test]
    fn statusline_is_reversed_and_idle_shows_the_app_name_only() {
        let app = App::new(20, 6);
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);

        let bottom = frame.height() - 1;
        for x in 0..frame.width() {
            assert!(
                frame.get(x, bottom).attrs.contains(Attrs::REVERSE),
                "statusline cell {x} must be reversed"
            );
        }
        let text = row_text(&frame, bottom);
        assert!(text.contains("yata"), "statusline was {text:?}");
        // The M1.5 placeholder readouts are gone: no terminal size, and no
        // scroll % or frame time before either has a value.
        assert!(!text.contains('×'), "size readout survived: {text:?}");
        assert!(!text.contains('%'), "made-up scroll %: {text:?}");
        assert!(!text.contains("ms"), "made-up frame time: {text:?}");
    }

    #[test]
    fn draw_leaves_the_page_area_blank_without_content() {
        let app = App::new(20, 6);
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);
        for y in 0..frame.height() - 1 {
            for x in 0..frame.width() {
                assert_eq!(frame.get(x, y), Cell::default());
            }
        }
    }

    #[test]
    fn stale_fetch_messages_are_ignored() {
        let mut app = App::new(80, 24);
        let stale = app.start_fetch("http://old/".into());
        let current = app.start_fetch("http://new/".into());
        assert_ne!(stale, current, "each fetch gets a fresh generation");

        let msgs = [
            Msg::Loading {
                id: stale,
                bytes_so_far: 999,
            },
            loaded(stale, 200, 4096),
            Msg::NetError {
                id: stale,
                url: "http://old/".into(),
                reason: "too late".into(),
            },
        ];
        for msg in msgs {
            assert_eq!(app.update(msg), Effect::default());
        }
        assert!(
            app.timings().fetch.is_none(),
            "a stale Loaded must not record a fetch duration"
        );

        let mut frame = Frame::new(80, 24);
        app.draw(&mut frame);
        let row = row_text(&frame, 23);
        assert!(
            row.contains("loading… 0 KB"),
            "current fetch must still be untouched, row was {row:?}"
        );
        assert!(!row.contains("200"), "stale body leaked into {row:?}");
        assert!(!row.contains("too late"), "stale error leaked into {row:?}");

        assert_eq!(app.update(loaded(current, 200, 4096)), dirty());
        assert!(
            app.timings().fetch.is_some(),
            "the accepted Loaded must record its duration"
        );
    }

    #[test]
    fn status_row_shows_loading_progress_then_loaded_summary() {
        let mut app = App::new(60, 6);
        let id = app.start_fetch("http://x/".into());
        let mut frame = Frame::new(60, 6);

        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.contains("http://x/"), "row was {row:?}");
        assert!(row.contains("loading… 0 KB"), "row was {row:?}");
        assert!(
            row.chars().any(|c| SPINNER.contains(&c)),
            "no spinner glyph in {row:?}"
        );

        assert_eq!(
            app.update(Msg::Loading {
                id,
                bytes_so_far: 12 * 1024,
            }),
            dirty()
        );
        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("loading… 12 KB"));

        assert_eq!(app.update(loaded(id, 200, 54 * 1024)), dirty());
        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.contains("http://final/"), "row was {row:?}");
        assert!(row.contains("200 · 54 KB"), "row was {row:?}");
        assert!(!row.contains("loading"), "row was {row:?}");
    }

    #[test]
    fn status_row_shows_the_error_reason() {
        let mut app = App::new(60, 6);
        let id = app.start_fetch("http://x/".into());
        assert_eq!(
            app.update(Msg::NetError {
                id,
                url: "http://x/".into(),
                reason: "connection refused".into(),
            }),
            dirty()
        );
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.contains("http://x/"), "row was {row:?}");
        assert!(row.contains("connection refused"), "row was {row:?}");
        assert!(!row.contains("loading"), "row was {row:?}");
    }

    #[test]
    fn statusline_spans_the_full_row_after_resize() {
        let mut app = App::new(20, 6);
        app.update(Msg::Resize(19, 5));
        let mut frame = Frame::new(19, 5);
        app.draw(&mut frame);
        for x in 0..frame.width() {
            assert!(
                frame.get(x, 4).attrs.contains(Attrs::REVERSE),
                "cell {x} of the resized statusline must be reversed"
            );
        }
    }

    // ---- M1.6 statusline ---------------------------------------------------

    #[test]
    fn spinner_advances_per_progress_message_and_resets_on_new_fetch() {
        let mut app = App::new(60, 6);
        let progress = |id| Msg::Loading {
            id,
            bytes_so_far: 1024,
        };
        let mut frame = Frame::new(60, 6);

        let id = app.start_fetch("http://x/".into());
        app.update(progress(id));
        app.draw(&mut frame);
        let one = row_text(&frame, 5);
        app.update(progress(id));
        app.draw(&mut frame);
        // Identical byte counts: the glyph is the only thing that may differ.
        assert_ne!(row_text(&frame, 5), one, "spinner did not advance");

        // A new fetch restarts the cycle: one message in, the row matches the
        // first fetch's one-message row exactly.
        let id2 = app.start_fetch("http://x/".into());
        app.update(progress(id2));
        app.draw(&mut frame);
        assert_eq!(row_text(&frame, 5), one, "spinner cycle did not reset");
    }

    #[test]
    fn stale_progress_does_not_advance_the_spinner() {
        let mut app = App::new(60, 6);
        let stale = app.start_fetch("http://old/".into());
        let current = app.start_fetch("http://new/".into());
        app.update(Msg::Loading {
            id: current,
            bytes_so_far: 1024,
        });
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        let before = row_text(&frame, 5);

        assert_eq!(
            app.update(Msg::Loading {
                id: stale,
                bytes_so_far: 1024,
            }),
            Effect::default()
        );
        app.draw(&mut frame);
        assert_eq!(
            row_text(&frame, 5),
            before,
            "a stale message moved the spinner"
        );
    }

    #[test]
    fn frame_time_appears_only_after_a_recording() {
        let mut app = App::new(40, 6);
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(!row_text(&frame, 5).contains("ms"));

        // `record_frame` returns nothing and carries no Effect: it must never
        // be able to request a redraw (that would loop forever). The value
        // simply shows on the next paint.
        app.record_frame(Duration::from_micros(2100));
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 5).contains("2.1 ms"),
            "row was {:?}",
            row_text(&frame, 5)
        );
    }

    #[test]
    fn scroll_percent_tracks_the_viewport() {
        let mut app = App::new(20, 6);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(50)); // page of 5: max offset 45
        let mut frame = Frame::new(20, 6);

        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(
            row.trim_end().ends_with("0%") && !row.contains("100%"),
            "top must read 0%: {row:?}"
        );

        // Half a page down (2 of 45): strictly between the ends, never
        // snapped to 0 or 100.
        app.update(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.trim_end().ends_with("4%"), "row was {row:?}");

        app.update(key(KeyCode::Char('G'), KeyModifiers::NONE));
        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("100%"));

        // One line above the bottom: 44/45 rounds to 98 — between, never 100.
        app.update(ch('k'));
        app.draw(&mut frame);
        let row = row_text(&frame, 5);
        assert!(row.contains("98%"), "row was {row:?}");
    }

    #[test]
    fn byte_counts_round_up_so_progress_never_reads_zero() {
        assert_eq!(kb(0), 0);
        assert_eq!(kb(1), 1, "any progress at all must read 1 KB, not 0 KB");
        assert_eq!(kb(1024), 1);
        assert_eq!(kb(1025), 2);
    }

    #[test]
    fn content_that_fits_reads_100_percent_and_no_content_reads_nothing() {
        let mut app = App::new(40, 6);
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(!row_text(&frame, 5).contains('%'), "no content, no percent");

        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(3));
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 5).contains("100%"),
            "fully visible content reads 100%"
        );
    }

    // ---- M1.7 fetch duration ----------------------------------------------

    #[test]
    fn accepted_loaded_records_the_fetch_duration() {
        let mut app = App::new(40, 6);
        let id = app.start_fetch("http://x/".into());
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 200,
            body: b"hi".to_vec(),
            elapsed: Duration::from_micros(12_300),
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        assert_eq!(app.timings().fetch, Some(Duration::from_micros(12_300)));
    }

    #[test]
    fn start_fetch_keeps_the_last_completed_fetch_duration() {
        let mut app = App::new(40, 6);
        let id = app.start_fetch("http://x/".into());
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 200,
            body: b"hi".to_vec(),
            elapsed: Duration::from_micros(12_300),
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        // The overlay shows the last *completed* run (PLAN.md §4): the old
        // number stands until the new fetch lands.
        app.start_fetch("http://y/".into());
        assert_eq!(app.timings().fetch, Some(Duration::from_micros(12_300)));
    }

    #[test]
    fn net_error_records_no_fetch_duration() {
        let mut app = App::new(40, 6);
        let id = app.start_fetch("http://x/".into());
        app.update(Msg::NetError {
            id,
            url: "http://x/".into(),
            reason: "connection refused".into(),
        });
        assert_eq!(app.timings().fetch, None, "a failed fetch records nothing");

        // After a completed run, a later failure leaves the old value alone.
        let id = app.start_fetch("http://x/".into());
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 200,
            body: b"hi".to_vec(),
            elapsed: Duration::from_micros(12_300),
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        let id = app.start_fetch("http://y/".into());
        app.update(Msg::NetError {
            id,
            url: "http://y/".into(),
            reason: "connection refused".into(),
        });
        assert_eq!(app.timings().fetch, Some(Duration::from_micros(12_300)));
    }

    // ---- M1.7 timing overlay ----------------------------------------------

    fn f4() -> Msg {
        key(KeyCode::F(4), KeyModifiers::NONE)
    }

    /// An app with both stages timed: rows `fetch 12.3 ms` (13 cells, the box
    /// width) and `frame 2.1 ms` (12 cells), over a 50-line body.
    fn timed_app(w: u16, h: u16) -> App {
        let mut app = App::new(w, h);
        let id = app.start_fetch("http://x/".into());
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 200,
            body: body(50),
            elapsed: Duration::from_micros(12_300),
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.record_frame(Duration::from_micros(2_100));
        app
    }

    #[test]
    fn timing_overlay_is_hidden_by_default() {
        let app = timed_app(40, 10);
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        assert!(
            !row_text(&frame, 0).contains("ms"),
            "no overlay before F4: {:?}",
            row_text(&frame, 0)
        );
        assert!(!frame.get(39, 0).attrs.contains(Attrs::REVERSE));
    }

    #[test]
    fn f4_shows_the_timing_rows_top_right_reversed() {
        let mut app = timed_app(40, 10);
        assert_eq!(app.update(f4()), redraw());
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);

        let row0 = row_text(&frame, 0);
        assert!(
            row0.starts_with("line0"),
            "body must stay visible: {row0:?}"
        );
        assert!(row0.ends_with("cache network"), "row was {row0:?}");
        let row1 = row_text(&frame, 1);
        assert!(row1.ends_with("fetch 12.3 ms"), "row was {row1:?}");
        let row2 = row_text(&frame, 2);
        assert!(
            row2.ends_with(" frame 2.1 ms"),
            "rows must pad to the widest row: {row2:?}"
        );
        // The box: 13 cells wide, right-aligned to the frame edge, reversed.
        for y in 0..3 {
            for x in 27..40 {
                assert!(
                    frame.get(x, y).attrs.contains(Attrs::REVERSE),
                    "overlay cell ({x},{y}) must be reversed"
                );
            }
            assert!(!frame.get(26, y).attrs.contains(Attrs::REVERSE));
        }
        // The overlay draws exactly the formatter's rows — one implementation
        // feeds it and `--timing` both.
        let rows = app.timings().rows();
        assert_eq!(&row0[27..], rows[0]);
        assert_eq!(row1[27..].trim_start(), rows[1]);
        assert_eq!(row2[27..].trim_start(), rows[2]);
    }

    #[test]
    fn f4_again_hides_the_overlay_and_restores_the_page() {
        let mut app = timed_app(40, 10);
        let mut before = Frame::new(40, 10);
        app.draw(&mut before);

        assert_eq!(app.update(f4()), redraw());
        let mut shown = Frame::new(40, 10);
        app.draw(&mut shown);
        assert!(
            row_text(&shown, 0).ends_with("cache network"),
            "overlay must show"
        );

        assert_eq!(app.update(f4()), redraw());
        let mut after = Frame::new(40, 10);
        app.draw(&mut after);
        for y in 0..10 {
            for x in 0..40 {
                assert_eq!(
                    after.get(x, y),
                    before.get(x, y),
                    "cell ({x},{y}) not restored after toggling off"
                );
            }
        }
    }

    #[test]
    fn overlay_never_touches_the_bottom_row() {
        for h in [1u16, 2] {
            let mut app = timed_app(40, h);
            let mut plain = Frame::new(40, h);
            app.draw(&mut plain);

            app.update(f4());
            let mut overlaid = Frame::new(40, h);
            app.draw(&mut overlaid);

            let bottom = h - 1;
            for x in 0..40 {
                assert_eq!(
                    overlaid.get(x, bottom),
                    plain.get(x, bottom),
                    "bottom-row cell {x} changed at height {h}"
                );
            }
            if h == 2 {
                // The one page row carries the first timing row; the second
                // row is clipped rather than spilling onto the statusline.
                assert!(row_text(&overlaid, 0).ends_with("cache network"));
            }
        }
    }

    #[test]
    fn narrow_frames_draw_the_overlay_without_panicking() {
        for w in [0u16, 1, 2, 5, 12] {
            let mut app = timed_app(w, 6);
            app.update(f4());
            let mut frame = Frame::new(w, 6);
            app.draw(&mut frame); // must not panic; clipping is acceptable
            if w > 0 {
                assert!(
                    frame.get(0, 0).attrs.contains(Attrs::REVERSE),
                    "a clipped overlay still paints from column 0 at width {w}"
                );
            }
        }
    }

    #[test]
    fn f4_with_nothing_timed_draws_nothing_but_still_toggles() {
        let mut app = App::new(40, 6);
        assert_eq!(app.update(f4()), redraw());
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        for y in 0..5 {
            for x in 0..40 {
                assert_eq!(
                    frame.get(x, y),
                    Cell::default(),
                    "zero rows must draw nothing at ({x},{y})"
                );
            }
        }
        // The toggle still flipped: once something is timed the overlay is
        // already on, with no second F4 needed.
        app.record_frame(Duration::from_micros(2_100));
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).ends_with("frame 2.1 ms"));
    }

    // ---- M2.3 parse + F1 DOM inspector ------------------------------------

    fn f1() -> Msg {
        key(KeyCode::F(1), KeyModifiers::NONE)
    }

    fn f2() -> Msg {
        key(KeyCode::F(2), KeyModifiers::NONE)
    }

    fn f3() -> Msg {
        key(KeyCode::F(3), KeyModifiers::NONE)
    }

    fn parsed(id: PageId, html: &str) -> Msg {
        Msg::Parsed {
            id,
            dom: crate::html::parse(html),
            elapsed: Duration::from_micros(31_700),
        }
    }

    // ---- the script pass (M10.2) ------------------------------------------

    /// A page that has been fetched and parsed, ready for its script pass.
    fn scripted_app(html: &str) -> (App, PageId) {
        let mut app = App::new(40, 10);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, html.as_bytes().to_vec());
        app.update(parsed(id, html));
        (app, id)
    }

    #[test]
    fn the_page_is_painted_before_any_of_its_script_runs() {
        // The ordering guarantee of M10.2: `Parsed` paints and *asks* for the
        // pass; the pass is a separate turn, so a script that spends its whole
        // budget cannot delay first paint (UX §3.2).
        let (mut app, id) = scripted_app("<p>already visible</p><script>1</script>");

        // The page is laid out and drawable before the pass has run at all.
        assert_eq!(app.timings().script, None);
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        assert!(
            (0..10).any(|y| row_text(&frame, y).contains("already visible")),
            "the page was not on screen before the script pass"
        );

        // Now the turn the loop sends itself.
        app.update(Msg::RunScripts { id });
        assert!(app.timings().script.is_some(), "the pass did not run");
    }

    #[test]
    fn a_script_appended_paragraph_is_absent_in_the_first_frame_and_present_in_the_second() {
        // M10.2 deliverable 2, finishable only now that M10.5 can append: the
        // page paints *without* what its script adds, and the script's own
        // turn brings it. Both halves matter — the first is the promise that a
        // slow script cannot delay first paint, the second that the pass is
        // not merely allowed to be slow but actually reaches the screen.
        let (mut app, id) = scripted_app(
            "<p>parsed content</p><script>\
             var added = document.createElement('p');\
             added.textContent = 'appended by script';\
             document.body.appendChild(added);</script>",
        );

        let mut first = Frame::new(40, 10);
        app.draw(&mut first);
        let first_text: String = (0..10).map(|y| row_text(&first, y)).collect();
        assert!(first_text.contains("parsed content"), "{first_text:?}");
        assert!(
            !first_text.contains("appended by script"),
            "the script ran before the page was painted: {first_text:?}"
        );

        assert_eq!(app.update(Msg::RunScripts { id }), redraw());

        let mut second = Frame::new(40, 10);
        app.draw(&mut second);
        let second_text: String = (0..10).map(|y| row_text(&second, y)).collect();
        assert!(
            second_text.contains("appended by script"),
            "the script's paragraph never reached the second frame: {second_text:?}"
        );
        assert!(second_text.contains("parsed content"));
    }

    #[test]
    fn the_dom_is_lent_to_the_tick_and_comes_straight_back() {
        // No `Rc<RefCell<Dom>>`, no second copy: `App` owns the tree again the
        // moment the tick ends, and can lay out immediately.
        let (mut app, id) = scripted_app("<p>body text</p><script>1</script>");
        app.update(Msg::RunScripts { id });

        app.update(Msg::Resize(30, 8));
        let mut frame = Frame::new(30, 8);
        app.draw(&mut frame);
        assert!(
            (0..8).any(|y| row_text(&frame, y).contains("body text")),
            "the tree did not come back from the tick"
        );
    }

    #[test]
    fn scripts_run_once_per_page_and_not_on_anything_else() {
        // The invariant that would rot first, and the one that would put an
        // unbounded amount of work on the resize path if it did.
        let (mut app, id) = scripted_app("<p>x</p><script>1</script>");
        assert_eq!(app.update(Msg::RunScripts { id }).run_scripts, None);

        for (what, msg) in [
            ("resize", Msg::Resize(30, 8)),
            ("scroll", key(KeyCode::Char('j'), KeyModifiers::NONE)),
            ("an inspector toggle", f1()),
            (
                "a stylesheet arriving",
                Msg::Stylesheet {
                    id,
                    slot: 0,
                    sheet: Some(Stylesheet::default()),
                },
            ),
        ] {
            assert_eq!(
                app.update(msg).run_scripts,
                None,
                "{what} asked for another script pass"
            );
        }
    }

    // ---- fetch (M10.12) ---------------------------------------------------

    fn json_response(body: &str) -> Result<crate::net::JsResponse, String> {
        Ok(crate::net::JsResponse {
            status: 200,
            status_text: "OK".into(),
            url: "http://final/data.json".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        })
    }

    #[test]
    fn a_fetch_leaves_as_an_effect_and_settles_as_a_message() {
        let (mut app, id) = scripted_app(
            "<div id=out>loading</div><script>\
             fetch('/data.json').then(function (r) { return r.json(); })\
                                .then(function (d) { console.log('got ' + d.n); });</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let (page, ask) = effect
            .fetches
            .first()
            .expect("the fetch must reach the loop");
        assert_eq!(*page, id);
        assert_eq!(ask.ask.url, "http://final/data.json");
        assert_eq!(ask.method, "GET");
        // Nothing has settled: the promise is still pending.
        assert!(app.console.is_empty());

        app.update(Msg::JsFetch {
            page: id,
            request: ask.request,
            result: json_response("{\"n\": 7}"),
        });
        assert_eq!(
            app.console
                .entries()
                .first()
                .map(|e| e.text.clone())
                .as_deref(),
            Some("got 7")
        );
    }

    #[test]
    fn settling_a_fetch_runs_exactly_one_invalidation_cycle() {
        // Deliverable 5: resolution is a tick like any other, so a `.then`
        // that builds a list costs one relayout however much it appends.
        let (mut app, id) = scripted_app(
            "<div id=out></div><script>\
             fetch('/data.json').then(function (r) { return r.json(); }).then(function (d) {\
               var out = document.getElementById('out');\
               d.items.forEach(function (i) {\
                 var p = document.createElement('p'); p.textContent = i; out.appendChild(p);\
               });\
             });</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let request = effect.fetches[0].1.request;
        let (styled, laid_out, painted) = stages(&app);

        let effect = app.update(Msg::JsFetch {
            page: id,
            request,
            result: json_response("{\"items\": [\"a\", \"b\", \"c\", \"d\", \"e\"]}"),
        });
        assert!(effect.dirty);
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out + 1, painted + 1),
            "settling a fetch must cost one cycle"
        );
        assert!(screen(&mut app, 40, 10).contains('c'));
    }

    #[test]
    fn a_404_resolves_rather_than_rejecting() {
        // The thing pages get wrong constantly, so we must not: the response
        // arrived, so the promise resolves with `ok: false`.
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             fetch('/missing').then(function (r) { console.log('ok=' + r.ok + ' status=' + r.status); },\
                                    function () { console.log('rejected'); });</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let request = effect.fetches[0].1.request;
        app.update(Msg::JsFetch {
            page: id,
            request,
            result: Ok(crate::net::JsResponse {
                status: 404,
                status_text: "Not Found".into(),
                url: "http://final/missing".into(),
                headers: vec![],
                body: "not here".into(),
            }),
        });
        assert_eq!(
            app.console
                .entries()
                .first()
                .map(|e| e.text.clone())
                .as_deref(),
            Some("ok=false status=404")
        );
    }

    #[test]
    fn a_connection_failure_rejects() {
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             fetch('/gone').catch(function (e) { console.log(e.message); });</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let request = effect.fetches[0].1.request;
        app.update(Msg::JsFetch {
            page: id,
            request,
            result: Err("connection refused".into()),
        });
        assert_eq!(
            app.console
                .entries()
                .first()
                .map(|e| e.text.clone())
                .as_deref(),
            Some("fetch failed: connection refused")
        );
    }

    #[test]
    fn a_response_arriving_after_navigation_is_dropped() {
        let (mut app, first) = scripted_app(
            "<p>x</p><script>fetch('/data').then(function () { console.log('settled'); });</script>",
        );
        let effect = app.update(Msg::RunScripts { id: first });
        let request = effect.fetches[0].1.request;

        let second = app.start_fetch("http://elsewhere/".into());
        assert_ne!(first, second);
        assert_eq!(
            app.update(Msg::JsFetch {
                page: first,
                request,
                result: json_response("{}"),
            }),
            Effect::default()
        );
        assert!(app.console.is_empty(), "a stale response settled a promise");
    }

    #[test]
    fn a_cross_origin_fetch_is_refused_where_the_reader_can_see_it() {
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             fetch('https://elsewhere.example/secret').catch(function () {});</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        assert!(
            effect.fetches.is_empty(),
            "a cross-origin request left the app"
        );
        let entry = app
            .console
            .entries()
            .into_iter()
            .next()
            .expect("a console line");
        assert_eq!(entry.level, crate::js::console::Level::Error);
        assert!(
            entry.text.contains("only same-origin requests are allowed"),
            "{}",
            entry.text
        );
    }

    #[test]
    fn a_consumed_body_rejects_on_a_second_read() {
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             fetch('/data').then(function (r) {\
               return r.text().then(function () { return r.text(); });\
             }).catch(function (e) { console.log(e.message); });</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let request = effect.fetches[0].1.request;
        app.update(Msg::JsFetch {
            page: id,
            request,
            result: json_response("{}"),
        });
        assert_eq!(
            app.console
                .entries()
                .first()
                .map(|e| e.text.clone())
                .as_deref(),
            Some("body has already been consumed")
        );
    }

    #[test]
    fn a_page_that_floods_fetch_finds_a_wall() {
        // M10.13 will try exactly this. The cap rejects rather than queueing,
        // so the page finds out and the engine does not grow.
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             for (var i = 0; i < 500; i++) fetch('/x' + i).catch(function () {});</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        assert_eq!(
            effect.fetches.len(),
            crate::js::MAX_IN_FLIGHT,
            "the cap did not hold"
        );
        // And the page is told, in the pane a reader can open. Note what is
        // *not* asserted: a synchronous throw. `fetch` returns a rejected
        // promise rather than throwing, as a browser does, so a page counting
        // refusals must do it in a `.catch`.
        assert!(
            app.console
                .entries()
                .iter()
                .any(|e| e.text.contains("requests in flight")),
            "the refusal was not reported: {:?}",
            app.console.entries().last()
        );
    }

    // ---- location (M10.11) ------------------------------------------------

    #[test]
    fn assigning_location_navigates_through_the_same_path_a_link_does() {
        let (mut app, id) = scripted_app("<p>page</p><script>location.href = '/next';</script>");
        let effect = app.update(Msg::RunScripts { id });
        let (fetch_id, url) = effect.fetch.expect("the script must navigate");
        assert_eq!(url.url, "http://final/next");
        assert_ne!(fetch_id, id, "a navigation starts a new generation");
    }

    #[test]
    fn a_script_assigning_location_in_a_loop_navigates_once() {
        // The named case: one fetch, not a thousand. The queue holds one
        // request and the last assignment wins, the same rule `apply_batch`
        // applies to URL-bar commits.
        let (mut app, id) = scripted_app(
            "<p>page</p><script>\
             for (var i = 0; i < 1000; i++) location.href = '/page' + i;</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let (_, url) = effect.fetch.expect("the script must navigate");
        assert_eq!(url.url, "http://final/page999", "last assignment wins");
    }

    #[test]
    fn assign_pushes_history_and_replace_does_not() {
        let (mut app, id) = scripted_app("<p>a</p><script>location.assign('/b');</script>");
        app.update(Msg::RunScripts { id });
        assert!(app.history.can_back(), "assign must push history");

        let (mut app, id) = scripted_app("<p>a</p><script>location.replace('/b');</script>");
        app.update(Msg::RunScripts { id });
        assert!(!app.history.can_back(), "replace must not push history");
    }

    #[test]
    fn a_fragment_change_does_not_fetch() {
        // `same_document` already decides this for links; a script assigning
        // `location.hash` takes the same path and must reach the same answer.
        let (mut app, id) = scripted_app("<p>page</p><script>location.hash = 'part2';</script>");
        let effect = app.update(Msg::RunScripts { id });
        assert_eq!(effect.fetch, None, "a fragment change fetched");
    }

    #[test]
    fn a_click_handler_can_navigate() {
        // The flagship shape: a script-built button that goes somewhere. It
        // must work through the keyboard path too, not only the mouse.
        let page = "<p><a href='#'>go</a></p><script>\
             document.querySelector('a').addEventListener('click', function (e) {\
               e.preventDefault();\
               location.href = '/from-the-handler';\
             });</script>";

        let (mut app, _) = live_page(80, 12, page);
        let effect = app.update(click_first_link(&app));
        let (_, url) = effect.fetch.expect("the handler's navigation was lost");
        assert_eq!(url.url, "http://final/from-the-handler");

        let (mut app, _) = live_page(80, 12, page);
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            effect.fetch.map(|(_, request)| request.url).as_deref(),
            Some("http://final/from-the-handler"),
            "the keyboard path lost the handler's navigation"
        );
    }

    #[test]
    fn a_timer_can_navigate() {
        let (mut app, id) = scripted_app(
            "<p>page</p><script>setTimeout(function () { location.href = '/later'; }, 5);</script>",
        );
        app.update(Msg::RunScripts { id });
        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert_eq!(
            effect.fetch.map(|(_, request)| request.url).as_deref(),
            Some("http://final/later")
        );
    }

    // ---- external scripts (M10.10) ----------------------------------------

    /// A page loaded from `http://final/` (what `load` reports), parsed, with
    /// its script pass run — so the queue exists and its externals have been
    /// requested.
    fn page_with_scripts(html: &str) -> (App, PageId, Effect) {
        let (mut app, id) = scripted_app(html);
        let effect = app.update(Msg::RunScripts { id });
        (app, id, effect)
    }

    #[test]
    fn a_second_script_pass_for_one_page_runs_nothing_twice() {
        // Only `Msg::Parsed` asks for a pass today, so a duplicate cannot be
        // produced — but the guard against re-running a page's scripts should
        // be structural rather than "nothing happens to send it twice".
        let (mut app, id) = scripted_app(
            "<p>x</p><script>window.runs = (window.runs || 0) + 1;\
             console.log('run ' + window.runs);</script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(Msg::RunScripts { id });
        app.update(Msg::RunScripts { id });

        let runs: Vec<String> = app
            .console
            .entries()
            .iter()
            .map(|e| e.text.clone())
            .collect();
        assert_eq!(runs, ["run 1"], "the page's script ran more than once");
    }

    #[test]
    fn external_scripts_are_requested_in_document_order() {
        let (_, id, effect) = page_with_scripts(
            "<script src='a.js'></script><script>1;</script><script src='b.js'></script>",
        );
        assert_eq!(
            effect.scripts,
            [
                (id, 0, net::Request::bare("http://final/a.js")),
                (id, 2, net::Request::bare("http://final/b.js")),
            ],
            "slots are allocated in document order, before any fetch starts"
        );
    }

    #[test]
    fn execution_order_is_the_document_not_the_network() {
        // The task's named case: `[external, inline, external]` where the
        // second external arrives first must still execute 1, 2, 3.
        let (mut app, id, _) = page_with_scripts(
            "<script src='a.js'></script>\
             <script>order.push('inline'); </script>\
             <script src='b.js'></script>\
             <script>console.log(order.join(','));</script>",
        );
        // Nothing has run: slot 0 is a hole, so even the inline script waits.
        assert!(
            app.console.entries().is_empty(),
            "{:?}",
            app.console.entries()
        );

        // The *second* external lands first. Still nothing may run.
        app.update(Msg::Script {
            id,
            slot: 2,
            source: Some("order.push('b.js');".into()),
        });
        assert!(app.console.entries().is_empty());

        // The first arrives and unblocks all four, in document order.
        app.update(Msg::Script {
            id,
            slot: 0,
            source: Some("var order = ['a.js'];".into()),
        });
        assert_eq!(
            app.console
                .entries()
                .iter()
                .map(|e| e.text.clone())
                .collect::<Vec<_>>(),
            ["a.js,inline,b.js"]
        );
    }

    #[test]
    fn a_hole_that_never_fills_holds_the_rest() {
        // A browser would not run them either: the script that never arrived
        // may be the one that defined everything after it.
        let (mut app, id, _) = page_with_scripts(
            "<script src='never.js'></script><script>console.log('should not run');</script>",
        );
        app.update(Msg::Script {
            id,
            slot: 99,
            source: Some("nowhere".into()),
        });
        assert!(
            app.console
                .entries()
                .iter()
                .all(|e| e.text != "should not run"),
            "{:?}",
            app.console.entries()
        );
    }

    #[test]
    fn a_failed_fetch_is_a_degraded_page_not_an_error_page() {
        let (mut app, id, _) = page_with_scripts(
            "<p>page text</p><script src='gone.js'></script>\
             <script>console.log('after the failure');</script>",
        );
        app.update(Msg::Script {
            id,
            slot: 0,
            source: None,
        });

        let texts: Vec<String> = app
            .console
            .entries()
            .iter()
            .map(|e| e.text.clone())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("could not be fetched")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t == "after the failure"), "{texts:?}");
        // The page itself is untouched — no error page, no lost content.
        assert!(screen(&mut app, 40, 8).contains("page text"));
    }

    #[test]
    fn a_superseded_generations_script_never_runs() {
        let (mut app, first, _) = page_with_scripts("<script src='a.js'></script>");
        let second = app.start_fetch("http://elsewhere/".into());
        assert_ne!(first, second);

        assert_eq!(
            app.update(Msg::Script {
                id: first,
                slot: 0,
                source: Some("console.log('from the old page');".into()),
            }),
            Effect::default()
        );
        assert!(app.console.is_empty(), "a stale generation's script ran");
    }

    #[test]
    fn an_external_script_that_mutates_runs_one_invalidation_cycle() {
        let (mut app, id, _) = page_with_scripts("<div id=out></div><script src='a.js'></script>");
        let (styled, laid_out, painted) = stages(&app);

        app.update(Msg::Script {
            id,
            slot: 0,
            source: Some(
                "var out = document.getElementById('out');\
                 for (var i = 0; i < 50; i++) out.appendChild(document.createElement('p'));"
                    .into(),
            ),
        });
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out + 1, painted + 1),
            "an arriving script's mutations must cost one cycle"
        );
    }

    #[test]
    fn the_script_row_sums_every_prefix_rather_than_the_last() {
        // `F4` should say what the page's script cost, not what its last
        // arriving script cost.
        let (mut app, id, _) =
            page_with_scripts("<script>1;</script><script src='a.js'></script><script>2;</script>");
        let after_pass = app.timings().script.expect("the pass was timed");
        app.update(Msg::Script {
            id,
            slot: 1,
            source: Some("3;".into()),
        });
        let after_arrival = app.timings().script.expect("the arrival was timed");
        assert!(
            after_arrival >= after_pass,
            "the script row went backwards: {after_pass:?} then {after_arrival:?}"
        );
    }

    // ---- scripts a script inserted (M11.5) --------------------------------

    /// Every console line the page has produced, in order.
    fn logged(app: &App) -> Vec<String> {
        app.console
            .entries()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    }

    /// Run turns until the page stops asking for another, and report how many
    /// ran. The loop's job, in a test: `Effect::run_scripts` goes back through
    /// the channel, so a chain of insertions is a chain of turns.
    fn drain_turns(app: &mut App, first: Effect) -> usize {
        let mut effect = first;
        let mut turns = 0;
        while let Some(id) = effect.run_scripts {
            turns += 1;
            assert!(turns < 200, "the loop never stopped asking for a turn");
            effect = app.update(Msg::RunScripts { id });
        }
        turns
    }

    /// The script pass and every turn it asks for after it.
    fn run_turns(app: &mut App, id: PageId) -> usize {
        let first = app.update(Msg::RunScripts { id });
        drain_turns(app, first)
    }

    #[test]
    fn an_inserted_inline_script_runs_in_a_later_turn_and_reaches_the_screen() {
        // The two halves the deviation denied. The insertion tick does *not*
        // run it — re-entering the engine from inside a binding is the
        // `document.write` re-entrancy bug — and the turn after it does, with
        // its DOM changes on screen in the frame after that.
        let (mut app, id) = scripted_app(
            "<p>parsed</p><script>\
             var s = document.createElement('script');\
             s.textContent = \"document.body.appendChild(document.createElement('p'))\
                              .textContent = 'from the inserted script';\";\
             document.body.appendChild(s);</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        assert!(
            !screen(&mut app, 40, 10).contains("from the inserted script"),
            "the inserted script ran inside the tick that inserted it"
        );
        assert_eq!(effect.run_scripts, Some(id), "no later turn was asked for");

        assert_eq!(drain_turns(&mut app, effect), 1);
        let shown = screen(&mut app, 40, 10);
        assert!(shown.contains("from the inserted script"), "{shown}");
        assert!(shown.contains("parsed"), "the page lost its own content");
    }

    #[test]
    fn an_inserted_script_that_only_defines_a_function_runs_no_stage() {
        // The counters, not the appearance (M10.6's rule carried into M11.5):
        // the *insertion* tick is a structural edit and pays for one, but the
        // turn that runs the inserted script must cost nothing when the script
        // changes nothing.
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             var s = document.createElement('script');\
             s.textContent = 'function laterCall() { return 1; }';\
             document.body.appendChild(s);</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        let before = stages(&app);
        assert_eq!(drain_turns(&mut app, effect), 1);
        assert_eq!(
            stages(&app),
            before,
            "a script that only defined a function ran a pipeline stage"
        );
    }

    #[test]
    fn an_inserted_script_runs_although_a_document_slot_is_still_in_flight() {
        // **The ordering decision (deliverable 3), end to end, with a pending
        // slot in the queue at the moment of insertion.** The inserted script
        // was created by code that has already run, so it cannot be waiting on
        // `slow.js`; `inline#3`, which the document wrote *after* `slow.js`,
        // still is.
        let (mut app, id) = scripted_app(
            "<script>\
             var s = document.createElement('script');\
             s.textContent = \"console.log('inserted');\";\
             document.body.appendChild(s);</script>\
             <script src='slow.js'></script>\
             <script>console.log('inline#3');</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        assert_eq!(app.script_queue.pending(), 1, "slot 1 must still be a hole");
        drain_turns(&mut app, effect);
        assert_eq!(logged(&app), ["inserted"]);

        // And the document's own order is untouched: `inline#3` runs only
        // when the hole in front of it fills.
        app.update(Msg::Script {
            id,
            slot: 1,
            source: Some("console.log('slow.js');".into()),
        });
        assert_eq!(logged(&app), ["inserted", "slow.js", "inline#3"]);
    }

    #[test]
    fn the_analytics_loader_fetches_google_analytics_exactly_once() {
        // **The acceptance page.** motherfuckingwebsite.com's fixture, its own
        // Google Analytics loader, unedited: `createElement`,
        // `getElementsByTagName`, `.async`, `.src`, `parentNode.insertBefore`.
        // The URL is protocol-relative, so the base is load-bearing — and no
        // test here touches the network: the worker is never spawned, the
        // `Effect` is the evidence.
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/motherfuckingwebsite.com.html"
        ));
        let mut app = App::new(80, 24);
        let url = "https://motherfuckingwebsite.com/";
        let id = app.start_fetch(url.into());
        app.update(Msg::Loaded {
            id,
            url: url.into(),
            status: 200,
            body: fixture.as_bytes().to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(id, fixture));

        let mut asked = Vec::new();
        let mut effect = app.update(Msg::RunScripts { id });
        loop {
            asked.extend(effect.scripts.iter().map(|(_, _, url)| url.clone()));
            match effect.run_scripts {
                Some(id) => effect = app.update(Msg::RunScripts { id }),
                None => break,
            }
        }
        assert_eq!(
            asked,
            [net::Request::bare(
                "https://www.google-analytics.com/analytics.js"
            )],
            "the loader asked for {asked:?}"
        );

        // Once. A second turn must not re-request it — the element has
        // already started, and the slot is no longer pending.
        app.update(Msg::RunScripts { id });
        assert_eq!(app.update(Msg::RunScripts { id }).scripts, []);
        assert!(screen(&mut app, 80, 24).contains("motherfucking"));
    }

    #[test]
    fn an_inserted_script_whose_url_will_not_resolve_settles_rather_than_waiting() {
        // Deliverable 5's other half, and deliverable 8's error path in one:
        // the slot settles instead of holding the queue, and the element hears
        // about it rather than waiting on an event that never comes.
        let (mut app, id) = scripted_app(
            "<script>\
             var s = document.createElement('script');\
             s.onerror = function () { console.log('error fired'); };\
             s.src = 'http://';\
             document.body.appendChild(s);</script>",
        );
        run_turns(&mut app, id);
        assert!(
            logged(&app).contains(&"error fired".to_string()),
            "{:?}",
            logged(&app)
        );
        assert_eq!(app.script_queue.pending(), 0, "the slot is still a hole");
    }

    #[test]
    fn an_inserted_external_script_runs_then_fires_load_at_its_element() {
        // Deliverable 8, the success path: `load` means "it ran", so the
        // ordering between the two is part of the contract a chaining
        // bootstrap depends on.
        let (mut app, id) = scripted_app(
            "<script>\
             var s = document.createElement('script');\
             s.onload = function () { console.log('load fired'); };\
             s.src = 'lib.js';\
             document.body.appendChild(s);</script>",
        );
        let effect = app.update(Msg::RunScripts { id });
        assert_eq!(
            effect.scripts,
            [(id, 1, net::Request::bare("http://final/lib.js"))]
        );
        assert!(
            logged(&app).is_empty(),
            "load fired before the body arrived"
        );

        app.update(Msg::Script {
            id,
            slot: 1,
            source: Some("console.log('the library ran');".into()),
        });
        assert_eq!(logged(&app), ["the library ran", "load fired"]);
    }

    #[test]
    fn a_failed_fetch_for_an_inserted_script_fires_error_and_is_not_an_error_page() {
        let (mut app, id) = scripted_app(
            "<p>page text</p><script>\
             var s = document.createElement('script');\
             s.onerror = function () { console.log('error fired'); };\
             s.src = 'gone.js';\
             document.body.appendChild(s);</script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(Msg::Script {
            id,
            slot: 1,
            source: None,
        });
        assert!(
            logged(&app).contains(&"error fired".to_string()),
            "{:?}",
            logged(&app)
        );
        // A script whose fetch failed is a console line, not an error page.
        assert!(screen(&mut app, 40, 8).contains("page text"));
    }

    #[test]
    fn dom_content_loaded_and_load_fire_once_although_an_insertion_finishes_the_queue_twice() {
        // The latch in `ScriptQueue::take_finished`. A script inserted by a
        // `load` handler un-finishes a finished queue; without it the page
        // would see both events a second time.
        let (mut app, id) = scripted_app(
            "<script>\
             window.addEventListener('load', function () { console.log('load'); });\
             document.addEventListener('DOMContentLoaded', function () {\
               console.log('dcl');\
               var s = document.createElement('script');\
               s.textContent = \"console.log('inserted');\";\
               document.body.appendChild(s);\
             });</script>",
        );
        run_turns(&mut app, id);
        assert_eq!(logged(&app), ["dcl", "load", "inserted"]);
    }

    // The four that must not run, one test each.

    #[test]
    fn a_script_written_by_inner_html_never_runs() {
        // HTML says so, and it is a security rule rather than a nicety: it is
        // the difference between an XSS that can only write markup and one
        // that can execute. The mechanism is structural — `setInnerHTML` is a
        // different binding from `appendChild` and records nothing — rather
        // than a check that could be forgotten.
        let (mut app, id) = scripted_app(
            "<div id=host></div><script>\
             document.getElementById('host').innerHTML =\
               \"<script>console.log('injected')<\\/script>\";</script>",
        );
        run_turns(&mut app, id);
        assert!(
            !logged(&app).contains(&"injected".to_string()),
            "{:?}",
            logged(&app)
        );
    }

    #[test]
    fn a_script_that_is_never_connected_never_runs() {
        // Created and dropped, and appended into a subtree that is itself
        // detached: neither is in the document, so neither runs — until the
        // subtree is put in, which is the same insertion signal as any other.
        let (mut app, id) = scripted_app(
            "<script>\
             var loose = document.createElement('script');\
             loose.textContent = \"console.log('loose');\";\
             var holder = document.createElement('div');\
             var buried = document.createElement('script');\
             buried.textContent = \"console.log('buried');\";\
             holder.appendChild(buried);\
             window.holder = holder;</script>",
        );
        run_turns(&mut app, id);
        assert!(logged(&app).is_empty(), "{:?}", logged(&app));
    }

    #[test]
    fn a_script_inserted_into_a_template_or_noscript_never_runs() {
        // `js::sources` refuses to descend into these for the parsed document,
        // and the dynamic path reaches the same answer through the same list —
        // it does not hold its own opinion about where a script is inert.
        let (mut app, id) = scripted_app(
            "<template id=t></template><noscript id=n></noscript><script>\
             ['t', 'n'].forEach(function (where) {\
               var s = document.createElement('script');\
               s.textContent = \"console.log('\" + where + \"');\";\
               document.getElementById(where).appendChild(s);\
             });</script>",
        );
        run_turns(&mut app, id);
        assert!(logged(&app).is_empty(), "{:?}", logged(&app));
    }

    #[test]
    fn a_script_that_has_already_run_and_is_then_moved_does_not_run_again() {
        // The "already started" flag. Getting this wrong turns a page that
        // reorders its own DOM into a page that re-runs its analytics — so
        // both populations are checked: the document's own `<script>`, and one
        // the page inserted itself.
        let (mut app, id) = scripted_app(
            "<div id=host></div>\
             <script id=parsed>console.log('parsed script');</script>\
             <script>\
             var host = document.getElementById('host');\
             host.appendChild(document.getElementById('parsed'));\
             var mine = document.createElement('script');\
             mine.textContent = \"console.log('inserted script');\";\
             document.body.appendChild(mine);\
             window.mine = mine;</script>",
        );
        run_turns(&mut app, id);
        assert_eq!(logged(&app), ["parsed script", "inserted script"]);

        // Now move the inserted one, which has run, somewhere else.
        app.update(Msg::Timer {
            page: id,
            id: TimerId(0),
        });
        let (mut app, id) = scripted_app(
            "<div id=host></div><script>\
             var mine = document.createElement('script');\
             mine.textContent = \"console.log('once');\";\
             document.body.appendChild(mine);\
             setTimeout(function () { document.getElementById('host').appendChild(mine); }, 0);\
             </script>",
        );
        run_turns(&mut app, id);
        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        drain_turns(&mut app, effect);
        assert_eq!(logged(&app), ["once"], "a moved script ran twice");
    }

    #[test]
    fn scripts_inserted_in_one_tick_run_one_per_turn() {
        // The bound that keeps a turn worth one execution budget. Each script
        // in a prefix gets its own `js::SCRIPT_BUDGET` inside `Host::eval`, so
        // a turn that ran all five would be five budgets with the loop away
        // from `recv` for every one of them — and five is a number the page
        // chooses at runtime, not one a reader can see in the markup.
        let (mut app, id) = scripted_app(
            "<script>\
             for (var i = 1; i <= 5; i++) {\
               var s = document.createElement('script');\
               s.textContent = \"console.log('ran');\";\
               document.body.appendChild(s);\
             }</script>",
        );
        let mut effect = app.update(Msg::RunScripts { id });
        assert!(logged(&app).is_empty(), "the inserting tick ran one");
        for expected in 1..=5 {
            let id = effect.run_scripts.expect("a turn was not asked for");
            effect = app.update(Msg::RunScripts { id });
            assert_eq!(
                logged(&app).len(),
                expected,
                "a turn ran more than one inserted script"
            );
        }
        assert_eq!(
            effect.run_scripts, None,
            "a turn was asked for with nothing to do"
        );
    }

    #[test]
    fn an_error_is_fired_in_a_later_turn_than_the_one_that_discovered_it() {
        // A handler is a script and costs a budget, so an `error` fired at the
        // point of discovery would let an `onerror` that inserts the next
        // unresolvable script nest one dispatch inside the last — turns' worth
        // of work inside one `update`, with no `recv` between them. One per
        // turn instead, and the chain is a chain of turns like any other.
        let (mut app, id) = scripted_app(
            "<script>\
             window.link = function () {\
               var s = document.createElement('script');\
               s.onerror = function () { console.log('error'); link(); };\
               s.src = 'http://';\
               document.body.appendChild(s);\
             };\
             link();</script>",
        );
        // The handler's own line, not the resolver's report of the bad URL.
        let fired = |app: &App| logged(app).iter().filter(|line| *line == "error").count();

        let mut effect = app.update(Msg::RunScripts { id });
        assert_eq!(
            fired(&app),
            0,
            "the error fired inside the tick that discovered it"
        );
        let mut turns = 0;
        while let Some(id) = effect.run_scripts {
            turns += 1;
            assert!(
                turns <= js::queue::MAX_INSERTED_SCRIPTS + 1,
                "the chain never stopped"
            );
            effect = app.update(Msg::RunScripts { id });
            assert_eq!(fired(&app), turns, "a turn fired more than one error");
        }
        // It stopped where the page bound says, and every link cost one turn.
        assert_eq!(fired(&app), js::queue::MAX_INSERTED_SCRIPTS);
    }

    // ---- timers (M10.9) ---------------------------------------------------

    #[test]
    fn a_script_that_schedules_a_timer_asks_the_loop_for_it() {
        // `App` decides, the loop dispatches: the request reaches the event
        // loop as data on the `Effect`, exactly like a fetch.
        let (mut app, id) =
            scripted_app("<p id=out>x</p><script>setTimeout(function () {}, 25);</script>");
        let effect = app.update(Msg::RunScripts { id });
        assert_eq!(
            effect.timers,
            [TimerRequest::Schedule {
                page: id,
                id: TimerId(1),
                delay: Duration::from_millis(25),
            }]
        );
    }

    #[test]
    fn a_timer_tick_runs_exactly_one_invalidation_cycle() {
        // M10.6's counters at the third entry point into JS. However much the
        // callback mutates, the pipeline runs once.
        let (mut app, id) = scripted_app(
            "<div id=out></div><script>setTimeout(function () {\
               var out = document.getElementById('out');\
               for (var i = 0; i < 100; i++) out.appendChild(document.createElement('p'));\
             }, 5);</script>",
        );
        app.update(Msg::RunScripts { id });
        let (styled, laid_out, painted) = stages(&app);

        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert!(effect.dirty);
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out + 1, painted + 1),
            "one timer tick must cost one cycle whatever it mutated"
        );
    }

    #[test]
    fn a_timer_that_mutates_nothing_costs_nothing() {
        let (mut app, id) = scripted_app(
            "<p id=out>x</p><script>setTimeout(function () {\
               document.querySelectorAll('p').length;\
             }, 5);</script>",
        );
        app.update(Msg::RunScripts { id });
        let before = stages(&app);
        app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert_eq!(stages(&app), before, "a read-only timer ran a stage");
    }

    #[test]
    fn a_timer_for_a_superseded_page_never_runs() {
        // Deliverable 4: navigation drops the host and its callbacks, and a
        // message already in flight is dropped by the `PageId` guard.
        let (mut app, first) = scripted_app(
            "<p id=out>x</p><script>setTimeout(function () {\
               document.getElementById('out').textContent = 'from the old page';\
             }, 10000);</script>",
        );
        app.update(Msg::RunScripts { id: first });
        let before = stages(&app);

        let second = app.start_fetch("http://elsewhere/".into());
        assert_ne!(first, second);
        assert!(app.js_host.is_none(), "navigation kept the old host");

        assert_eq!(
            app.update(Msg::Timer {
                page: first,
                id: TimerId(1)
            }),
            Effect::default()
        );
        assert_eq!(stages(&app), before, "a stale timer ran a pipeline stage");
    }

    #[test]
    fn an_interval_reschedules_itself_after_each_tick() {
        let (mut app, id) = scripted_app(
            "<p id=out>x</p><script>var n = 0;\
             var h = setInterval(function () {\
               n++;\
               document.getElementById('out').textContent = 'tick ' + n;\
               if (n === 2) clearInterval(h);\
             }, 10);</script>",
        );
        app.update(Msg::RunScripts { id });

        // First tick: the page updates and the interval asks to be rearmed.
        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert!(screen(&mut app, 40, 6).contains("tick 1"));
        assert_eq!(
            effect.timers,
            [TimerRequest::Schedule {
                page: id,
                id: TimerId(1),
                delay: Duration::from_millis(10),
            }]
        );

        // Second tick clears it: no rearm, and a cancel for the loop.
        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert!(screen(&mut app, 40, 6).contains("tick 2"));
        assert_eq!(
            effect.timers,
            [TimerRequest::Cancel {
                page: id,
                id: TimerId(1)
            }]
        );

        // And a third message — one already in flight — does nothing.
        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert_eq!(effect, Effect::default());
    }

    // ---- events (M10.8) ---------------------------------------------------

    /// A loaded page whose script has run, ready to be clicked.
    fn live_page(w: u16, h: u16, html: &str) -> (App, PageId) {
        let mut app = App::new(w, h);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, html.as_bytes().to_vec());
        app.update(parsed(id, html));
        app.update(Msg::RunScripts { id });
        (app, id)
    }

    /// A page with one link whose listener cancels the click.
    const CANCELS: &str = "<p><a href='/next'>go</a></p><script>\
         document.querySelector('a').addEventListener('click', function (e) {\
           e.preventDefault();\
           document.querySelector('a').textContent = 'handled';\
         });</script>";

    #[test]
    fn a_mouse_click_dispatches_before_it_navigates() {
        let (mut app, _) = live_page(80, 12, CANCELS);
        let effect = app.update(click_first_link(&app));
        assert_eq!(
            effect.fetch, None,
            "preventDefault() did not cancel the navigation"
        );
        // And the listener's mutation is on screen, in the same turn.
        assert!(screen(&mut app, 80, 12).contains("handled"));
    }

    #[test]
    fn enter_on_a_focused_link_dispatches_before_it_navigates() {
        // UX §3.4: a page whose handler works with the mouse but not with the
        // keyboard has broken the flagship feature.
        let (mut app, _) = live_page(80, 12, CANCELS);
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(effect.fetch, None, "Enter navigated past preventDefault()");
        assert!(screen(&mut app, 80, 12).contains("handled"));
    }

    #[test]
    fn a_hint_follow_dispatches_before_it_navigates() {
        let (mut app, _) = live_page(80, 12, CANCELS);
        app.update(key(KeyCode::Char('f'), KeyModifiers::NONE));
        let label = app
            .hint
            .as_ref()
            .expect("hints opened")
            .labels
            .first()
            .expect("one link")
            .0
            .clone();
        let mut effect = Effect::default();
        for ch in label.chars() {
            effect = app.update(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(effect.fetch, None, "`f` navigated past preventDefault()");
        assert!(screen(&mut app, 80, 12).contains("handled"));
    }

    #[test]
    fn a_click_with_no_prevent_default_still_navigates() {
        // The listener runs, the navigation happens: the dispatch is not a
        // veto by default.
        let (mut app, _) = live_page(
            80,
            12,
            "<p><a href='/next'>go</a></p><script>\
             document.querySelector('a').addEventListener('click', function () {\
               document.querySelector('a').textContent = 'seen';\
             });</script>",
        );
        let effect = app.update(click_first_link(&app));
        let (_, url) = effect.fetch.expect("the click must still navigate");
        assert_eq!(url.url, "http://final/next");
        // The listener's mutation is observable until the new page lands. Note
        // what is *not* asserted: anything the listener logged, because
        // starting a navigation clears the console — it is page-local, and by
        // this point the page it belonged to is on its way out.
        assert!(screen(&mut app, 80, 12).contains("seen"));
    }

    #[test]
    fn a_dispatch_that_mutates_runs_exactly_one_cycle() {
        // M10.6's counters, applied to the other entry point into JS: however
        // many listeners mutate, the pipeline runs once.
        let (mut app, _) = live_page(
            80,
            12,
            "<p><a href='#'>go</a></p><div id=out></div><script>\
             var out = document.getElementById('out');\
             function add() { for (var i = 0; i < 100; i++) out.appendChild(document.createElement('p')); }\
             document.querySelector('a').addEventListener('click', function (e) { e.preventDefault(); add(); });\
             document.querySelector('a').addEventListener('click', add);\
             </script>",
        );
        let (styled, laid_out, painted) = stages(&app);
        app.update(click_first_link(&app));
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out + 1, painted + 1),
            "200 appendChild calls across two listeners must cost one cycle"
        );
    }

    #[test]
    fn a_dispatch_that_mutates_nothing_costs_nothing() {
        let (mut app, _) = live_page(
            80,
            12,
            "<p><a href='#'>go</a></p><script>\
             document.querySelector('a').addEventListener('click', function (e) {\
               e.preventDefault();\
               document.querySelectorAll('p').length;\
             });</script>",
        );
        let before = stages(&app);
        app.update(click_first_link(&app));
        assert_eq!(stages(&app), before, "a read-only listener ran a stage");
    }

    #[test]
    fn a_page_with_no_script_pays_nothing_for_a_click() {
        // No host means no listeners, and finding that out must not start an
        // engine — most pages on the ladder have no script at all.
        let (mut app, _) = live_page(80, 12, "<p><a href='/next'>go</a></p>");
        assert!(app.js_host.is_none());
        let effect = app.update(click_first_link(&app));
        assert!(effect.fetch.is_some());
        assert!(app.js_host.is_none(), "a click started an engine");
    }

    // ---- the console pane (M10.7) -----------------------------------------

    fn f5() -> Msg {
        key(KeyCode::F(5), KeyModifiers::NONE)
    }

    /// Every visible row of the frame, joined — what a reader sees.
    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut frame = Frame::new(w, h);
        app.draw(&mut frame);
        (0..h)
            .map(|y| row_text(&frame, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn f5_shows_what_the_page_logged_and_toggles_back() {
        let (mut app, id) = scripted_app(
            "<p>page</p><script>console.log('hello from the page'); console.warn('careful');</script>",
        );
        app.update(Msg::RunScripts { id });

        assert_eq!(app.update(f5()), redraw());
        let pane = screen(&mut app, 60, 8);
        assert!(pane.contains("log   hello from the page"), "{pane}");
        assert!(pane.contains("warn  careful"), "{pane}");
        // The surface says which one it is, like every other inspector.
        assert!(pane.contains("[console]"), "{pane}");

        // And back to the page.
        assert_eq!(app.update(f5()), redraw());
        let page = screen(&mut app, 60, 8);
        assert!(page.contains("page"), "{page}");
        assert!(!page.contains("hello from the page"), "{page}");
    }

    #[test]
    fn an_empty_console_is_a_placeholder_not_a_panic() {
        let (mut app, id) = scripted_app("<p>page</p><script>1;</script>");
        app.update(Msg::RunScripts { id });
        app.update(f5());
        assert!(
            screen(&mut app, 60, 8).contains("no console output"),
            "{}",
            screen(&mut app, 60, 8)
        );
    }

    #[test]
    fn the_console_clears_on_navigation() {
        // Page-local, like the host and every other per-page thing: the last
        // page's complaints on this page's pane would be a lie about it.
        let (mut app, id) =
            scripted_app("<p>a</p><script>console.log('from the first page');</script>");
        app.update(Msg::RunScripts { id });
        app.update(f5());
        assert!(screen(&mut app, 60, 8).contains("from the first page"));

        let next = app.start_fetch("http://second/".into());
        load(&mut app, next, b"<p>b</p>".to_vec());
        app.update(parsed(next, "<p>b</p>"));
        app.update(Msg::RunScripts { id: next });
        assert!(
            !screen(&mut app, 60, 8).contains("from the first page"),
            "{}",
            screen(&mut app, 60, 8)
        );
    }

    #[test]
    fn scrolling_the_console_never_rebuilds_it() {
        // The same rule as F1–F3: lines are built when the pane is about to be
        // seen, and scrolling is offset arithmetic over them.
        let (mut app, id) = scripted_app(
            "<p>page</p><script>for (var i = 0; i < 200; i++) console.log('line ' + i);</script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(f5());
        assert!(app.console_view_built);

        for _ in 0..30 {
            app.update(key(KeyCode::Char('j'), KeyModifiers::NONE));
            assert!(
                app.console_view_built,
                "scrolling the console rebuilt its lines"
            );
        }
        assert!(app.console_view.offset() > 0, "the pane did not scroll");
    }

    #[test]
    fn a_page_whose_script_threw_says_so_in_the_statusline() {
        // The cure for the worst outcome this milestone can produce: a page
        // that threw and then rendered nothing, silently.
        let (mut app, id) = scripted_app("<p>page</p><script>null.x;</script>");
        app.update(Msg::RunScripts { id });
        let status = screen(&mut app, 60, 8);
        assert!(status.contains("1 JS error · F5"), "{status}");

        // Plural, and only errors count — a warning is not a failure.
        let (mut app, id) = scripted_app(
            "<p>p</p><script>console.warn('w'); null.x; </script><script>undefined.y;</script>",
        );
        app.update(Msg::RunScripts { id });
        assert!(screen(&mut app, 60, 8).contains("2 JS errors · F5"));
    }

    #[test]
    fn a_tick_that_only_logs_still_reaches_the_screen() {
        // It changed no box, so M10.6's classification runs no stage — but the
        // pane and the statusline are stale, and a frame that never redraws
        // would hide the one thing the reader needs.
        let (mut app, id) =
            scripted_app("<p>page</p><script>console.error('something broke');</script>");
        let before = stages(&app);
        let effect = app.update(Msg::RunScripts { id });
        assert!(effect.dirty, "a tick that only logged did not redraw");
        assert_eq!(stages(&app), before, "logging ran a pipeline stage");
        assert!(screen(&mut app, 60, 8).contains("1 JS error · F5"));
    }

    #[test]
    fn the_help_overlay_lists_f5_because_it_comes_from_the_table() {
        // `keys::BINDINGS` is the one source of truth (CLAUDE.md); the overlay
        // is generated from it, so a new binding appears here for free or it
        // was not added to the table at all.
        let mut app = App::new(60, 20);
        app.update(key(KeyCode::Char('?'), KeyModifiers::NONE));
        let overlay = screen(&mut app, 60, 20);
        assert!(overlay.contains("javascript console"), "{overlay}");
        assert!(overlay.contains("F5"), "{overlay}");
    }

    // ---- redirects through the loop (M11.7a) ------------------------------

    /// One hop, as the worker reports it. 302 is the login-shaped default.
    fn hop(id: PageId, from: &str, to: &str, set_cookie: &[&str]) -> Msg {
        hop_at(id, from, to, 302, set_cookie)
    }

    fn hop_at(id: PageId, from: &str, to: &str, status: u16, set_cookie: &[&str]) -> Msg {
        Msg::Redirect {
            id,
            url: from.into(),
            to: to.into(),
            status,
            elapsed: Duration::ZERO,
            set_cookie: set_cookie.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// The request the loop would spawn next, if this effect asks for one.
    fn next_request(effect: &Effect) -> Option<&net::Request> {
        effect.fetch.as_ref().map(|(_, request)| request)
    }

    #[test]
    fn a_hop_sets_the_session_and_the_next_request_carries_it() {
        // **The test this task exists for, and the one that was impossible
        // before it.** A login is a POST-less shape here (M11.11 brings the
        // POST): `/login` answers 302 with `Set-Cookie: sid=…` and
        // `Location: /app`, and the request for `/app` has to carry the cookie
        // the 302 just handed out. Both halves used to fail — the hop's headers
        // never reached `App` at all, and the header the worker did carry was
        // computed once, for the URL the reader typed.
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/login".into());
        let effect = app.update(hop(
            id,
            "http://site.test/login",
            "http://site.test/app",
            &["sid=abc; Path=/"],
        ));

        let request = next_request(&effect).expect("a hop asks for the next request");
        assert_eq!(request.url, "http://site.test/app");
        assert_eq!(
            request.cookie.as_deref(),
            Some("sid=abc"),
            "the hop's own cookie did not reach the request it authorises"
        );
        assert_eq!(
            effect.fetch.as_ref().map(|(id, _)| *id),
            Some(id),
            "a hop is not a navigation"
        );
    }

    #[test]
    fn a_hop_is_the_same_generation_and_the_things_that_would_break_say_so() {
        // Reading `current_fetch` back would prove nothing; these three would
        // actually break if a hop minted an id. The fragment the reader typed
        // is held against the generation, a subresource in flight is dropped
        // unless its id is current, and the URL the page ends up at is the base
        // every relative href resolves against.
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/start#part2".into());
        app.update(hop(
            id,
            "http://site.test/start",
            "http://site.test/final",
            &[],
        ));

        let html = "<link rel=stylesheet href=/s.css>".to_string()
            + &"<p>one</p>".repeat(80)
            + "<p id=part2>target</p>";
        app.update(Msg::Loaded {
            id,
            url: "http://site.test/final".into(),
            status: 200,
            body: html.as_bytes().to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        // The landing page is accepted at all only because the generation did
        // not move: a hop that minted an id would drop its own page as stale.
        let effect = app.update(parsed(id, &html));
        assert_eq!(
            effect.sheets.first().map(|(sheet_id, _, _)| *sheet_id),
            Some(id),
            "the landing page's subresources belong to another generation"
        );
        assert_eq!(
            app.update(Msg::Stylesheet {
                id,
                slot: 0,
                sheet: Some(crate::css::parse("p { color: #c00 }")),
            }),
            redraw(),
            "a subresource of the page that landed was dropped as stale"
        );
        assert!(
            app.viewport.offset() > 0,
            "the fragment the reader typed was lost across the hop"
        );
        // The landing page's URL is the page's URL — with the fragment kept,
        // which is the rule `Loaded` already had for a redirect that drops one.
        assert_eq!(
            app.current_url().as_deref(),
            Some("http://site.test/final#part2")
        );
    }

    #[test]
    fn a_hop_that_names_a_fragment_wins_over_the_one_the_reader_typed() {
        // HTML's rule, and the reason `pending_scroll` is written rather than
        // left alone: the typed fragment survives a hop that has none (the
        // previous test), and loses to one that does.
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/start#typed".into());
        app.update(hop(
            id,
            "http://site.test/start",
            "http://site.test/final#hop",
            &[],
        ));
        let html =
            "<p id=hop>hop</p>".to_string() + &"<p>filler</p>".repeat(80) + "<p id=typed>typed</p>";
        load_at(&mut app, id, "http://site.test/final#hop", &html);
        assert_eq!(
            app.viewport.offset(),
            0,
            "the reader's fragment beat the redirect's"
        );
        assert_eq!(
            app.current_url().as_deref(),
            Some("http://site.test/final#hop")
        );
    }

    #[test]
    fn a_3xx_with_nowhere_to_go_is_an_error_page_and_not_a_hop() {
        // The worker only calls it a redirect when there is a `Location` it can
        // resolve; anything else arrives as an ordinary response, and a 3xx
        // body is not a document. The reader gets a page they can act on
        // instead of a chain into nothing.
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/x".into());
        app.update(Msg::Loaded {
            id,
            url: "http://site.test/x".into(),
            status: 302,
            body: b"<p>moved</p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        let screen = screen(&mut app, 60, 10);
        assert!(screen.contains("HTTP 302"), "{screen}");
        assert!(screen.contains("Press r to retry."), "{screen}");
    }

    #[test]
    fn a_cross_host_hop_carries_the_second_hosts_cookies_and_none_of_the_first() {
        // The new capability, and the reason the loop has to make this
        // decision: the library could only ever *strip* a cookie on a
        // cross-host hop. Asking the jar again can substitute — which is what a
        // browser does, and what an SSO round trip is made of.
        let mut app = App::new(80, 24);
        for (host, name) in [("site.test", "first"), ("idp.test", "second")] {
            app.cookies
                .insert(
                    crate::js::cookies::Cookie {
                        name: name.into(),
                        value: "1".into(),
                        host: host.into(),
                        path: "/".into(),
                        expires: None,
                        secure: false,
                        http_only: false,
                        same_site: crate::js::cookies::SameSite::Unspecified,
                    },
                    crate::js::cookies::now(),
                )
                .expect("the jar is empty");
        }
        let id = app.start_fetch("http://site.test/login".into());
        let effect = app.update(hop(
            id,
            "http://site.test/login",
            "http://idp.test/sso",
            &[],
        ));
        assert_eq!(
            next_request(&effect).and_then(|r| r.cookie.as_deref()),
            Some("second=1"),
            "the hop carried the wrong host's jar"
        );
    }

    #[test]
    fn a_hops_cookie_is_scoped_to_the_hop_not_to_where_the_chain_ends() {
        // The bug the old shape hid: a cookie the 302 set under `/login` was
        // either lost or (had it been kept) scoped to the landing page's path.
        // `Path=/app` from a response sent by `/login` is legal and means what
        // it says.
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/login".into());
        app.update(hop(
            id,
            "http://site.test/login",
            "http://site.test/app/home",
            &["scoped=yes; Path=/app"],
        ));
        // Asked through the function that decides what a request carries,
        // rather than by reading the jar: `Path=/app` means the landing page
        // gets it and a sibling path does not.
        let carried =
            |url: &str| js::cookies::header_for(&app.cookies, url, url, js::cookies::now());
        assert_eq!(
            carried("http://site.test/app/home").as_deref(),
            Some("scoped=yes"),
            "the hop's cookie did not reach the jar at its own scope"
        );
        assert_eq!(
            carried("http://site.test/elsewhere"),
            None,
            "the hop's cookie ignored its own Path"
        );
    }

    #[test]
    fn a_redirect_chain_is_one_history_entry() {
        let mut app = App::new(80, 24);
        // The page the reader came from.
        let first = app.start_fetch("http://site.test/one".into());
        load_at(&mut app, first, "http://site.test/one", "<p>one</p>");
        // …then a link to a URL that bounces twice before landing.
        let effect = app.navigate("http://site.test/start".into(), true);
        let id = effect.fetch.expect("a navigation fetches").0;
        app.update(hop(
            id,
            "http://site.test/start",
            "http://site.test/mid",
            &[],
        ));
        app.update(hop(
            id,
            "http://site.test/mid",
            "http://site.test/land",
            &[],
        ));
        load_at(&mut app, id, "http://site.test/land", "<p>landed</p>");

        // Back goes to where the reader came from, not to either 302.
        let effect = app.update(ch('H'));
        assert_eq!(
            effect.fetch.map(|(_, request)| request.url),
            Some("http://site.test/one".to_string()),
            "a hop pushed itself onto the history stack"
        );
    }

    #[test]
    fn a_redirect_loop_ends_at_the_bound_with_a_page_the_reader_can_act_on() {
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/0".into());
        // Every hop points at the next; nothing ever lands.
        for n in 0..MAX_REDIRECTS {
            let effect = app.update(hop(
                id,
                &format!("http://site.test/{n}"),
                &format!("http://site.test/{}", n + 1),
                &[],
            ));
            assert!(
                effect.fetch.is_some(),
                "hop {n} of {MAX_REDIRECTS} was refused early"
            );
        }
        let effect = app.update(hop(
            id,
            &format!("http://site.test/{MAX_REDIRECTS}"),
            "http://site.test/again",
            &[],
        ));
        assert!(
            effect.fetch.is_none(),
            "the chain kept going past the bound"
        );
        let screen = screen(&mut app, 60, 10);
        assert!(screen.contains("too many redirects"), "{screen}");
        assert!(screen.contains("Press r to retry."), "{screen}");
        // And the next navigation starts from zero hops, because the count
        // belongs to the navigation.
        let id = app.start_fetch("http://site.test/fresh".into());
        assert!(
            app.update(hop(id, "http://site.test/fresh", "http://site.test/x", &[]))
                .fetch
                .is_some(),
            "the last page's hops were counted against this one"
        );
    }

    #[test]
    fn a_hop_runs_no_pipeline_stage_and_the_fetch_row_is_the_whole_chain() {
        let mut app = App::new(80, 24);
        let id = app.start_fetch("http://site.test/start".into());
        let before = stages(&app);
        app.update(Msg::Redirect {
            id,
            url: "http://site.test/start".into(),
            to: "http://site.test/mid".into(),
            status: 302,
            elapsed: Duration::from_millis(30),
            set_cookie: Vec::new(),
        });
        app.update(Msg::Redirect {
            id,
            url: "http://site.test/mid".into(),
            to: "http://site.test/land".into(),
            status: 302,
            elapsed: Duration::from_millis(20),
            set_cookie: Vec::new(),
        });
        assert_eq!(stages(&app), before, "a hop ran a pipeline stage");

        app.update(Msg::Loaded {
            id,
            url: "http://site.test/land".into(),
            status: 200,
            body: b"<p>landed</p>".to_vec(),
            elapsed: Duration::from_millis(10),
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        // 30 + 20 + 10: the whole chain, not the 10 ms last hop. A reader
        // looking at `--timing` or `F4` has to see what the page really cost.
        assert_eq!(app.timings().fetch, Some(Duration::from_millis(60)));
    }

    /// Load a page at a specific URL, the way a landing page arrives.
    fn load_at(app: &mut App, id: PageId, url: &str, html_src: &str) {
        app.update(Msg::Loaded {
            id,
            url: url.into(),
            status: 200,
            body: html_src.as_bytes().to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(id, html_src));
    }

    // ---- form controls (M11.8) --------------------------------------------

    /// A page with both kinds of focusable thing, interleaved: a link, a text
    /// field, a `disabled` field, and a button.
    const FORM_PAGE: &str = "<p><a href=/one>one</a> <input size=6 value=a> \
         <input size=6 disabled> <button>Go</button> <a href=/two>two</a></p>";

    /// Which node `Tab` has parked on, as something a test can read.
    fn focused(app: &App) -> Option<String> {
        let (focus, dom) = (app.focus?, app.dom.as_ref()?);
        Some(match &dom.node(focus).data {
            crate::dom::NodeData::Element { tag, .. } => match dom.attr(focus, "href") {
                Some(href) => format!("{tag} {href}"),
                None => match dom.attr(focus, "value") {
                    Some(value) => format!("{tag} {value}"),
                    None => tag.clone(),
                },
            },
            other => format!("{other:?}"),
        })
    }

    fn field_rect(app: &App, node: NodeId) -> layout::Rect {
        app.layout_tree
            .as_ref()
            .unwrap()
            .boxes
            .iter()
            .find(|b| b.node == Some(node) && matches!(b.kind, BoxKind::Field(_)))
            .unwrap()
            .dimensions
            .content
    }

    fn click_control(app: &mut App, node: NodeId) -> Effect {
        let rect = field_rect(app, node);
        app.update(Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column(app.size.0).left + rect.x as u16,
            row: (rect.y - app.viewport.offset() as i32) as u16,
            modifiers: KeyModifiers::NONE,
        }))
    }

    #[test]
    fn tab_cycles_links_and_fields_together_in_document_order() {
        let (mut app, _) = scripted_app(FORM_PAGE);
        let mut seen = Vec::new();
        for _ in 0..5 {
            app.update(key(KeyCode::Tab, KeyModifiers::NONE));
            seen.push(focused(&app).unwrap());
        }
        // The `disabled` field is not in the cycle: Tab must not park on
        // something the reader cannot use.
        assert_eq!(
            seen,
            [
                "a /one".to_string(),
                "input a".into(),
                "button".into(),
                "a /two".into(),
                "a /one".into(),
            ]
        );
    }

    #[test]
    fn moving_focus_to_a_field_runs_no_pipeline_stage() {
        // M6.3's rule, unchanged by fields being focusable: focus is UI chrome
        // drawn as a frame overlay, so it restyles nothing, relayouts nothing
        // and rebuilds no display list.
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        let before = stages(&app);
        for _ in 0..4 {
            assert_eq!(app.update(key(KeyCode::Tab, KeyModifiers::NONE)), redraw());
        }
        assert_eq!(focused(&app).as_deref(), Some("a /two"));
        assert_eq!(stages(&app), before, "focusing a field ran a stage");
    }

    #[test]
    fn a_click_on_a_field_focuses_it_and_goes_nowhere() {
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        let before = stages(&app);
        // The field's own cells: "one " is 4 wide, then the frame at 4 and the
        // value from 5. Column 6, row 0 — plus the page column's left gutter.
        let left = column(app.size.0).left;
        let effect = app.update(Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: left + 6,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(effect, redraw());
        assert!(effect.fetch.is_none(), "a click on a field navigated");
        assert_eq!(focused(&app).as_deref(), Some("input a"));
        assert_eq!(stages(&app), before);
    }

    #[test]
    fn enter_toggles_a_checkbox_without_changing_its_attribute() {
        let (mut app, id) = scripted_app("<input id=c type=checkbox>");
        app.update(Msg::RunScripts { id });
        let node = by_id(&app, "c");
        app.focus = Some(node);
        let before = stages(&app);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            redraw()
        );
        let dom = app.dom.as_ref().unwrap();
        assert!(layout::field::checked(dom, node, "input"));
        assert_eq!(dom.attr(node, "checked"), None);
        assert_eq!(app.styles_run, before.0, "choice activation restyled");
        app.dom.as_mut().unwrap().set_attr(node, "checked", "");
        app.dom.as_mut().unwrap().remove_attr(node, "checked");
        assert!(layout::field::checked(
            app.dom.as_ref().unwrap(),
            node,
            "input"
        ));
    }

    #[test]
    fn disabled_choices_refuse_stale_focus_and_an_all_disabled_select_cannot_trap() {
        let src = "<input id=c type=checkbox disabled>\
                   <select id=s><option disabled>A</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let checkbox = by_id(&app, "c");
        app.focus = Some(checkbox);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default()
        );
        assert_eq!(app.dom.as_ref().unwrap().choice_state(checkbox), None);

        app.focus = Some(by_id(&app, "s"));
        let before = stages(&app);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default()
        );
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(stages(&app), before);
    }

    #[test]
    fn clicks_activate_a_checkbox_but_only_focus_a_select() {
        let src = "<input id=c type=checkbox><select id=s>\
                   <option id=a selected>A</option><option>B</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (checkbox, select, selected) = (by_id(&app, "c"), by_id(&app, "s"), by_id(&app, "a"));
        click_control(&mut app, checkbox);
        assert!(layout::field::checked(
            app.dom.as_ref().unwrap(),
            checkbox,
            "input"
        ));
        click_control(&mut app, select);
        assert_eq!(app.focus, Some(select));
        assert!(matches!(app.mode, Mode::Browse));
        let dom = app.dom.as_ref().unwrap();
        let options = layout::field::options(dom, select);
        assert_eq!(
            layout::field::selected_options(dom, select, &options),
            [selected]
        );
    }

    #[test]
    fn focus_reverses_a_choice_mark_and_select_mode_draws_its_cursor_label() {
        let src = "<input id=c type=checkbox checked><select id=s>\
                   <option selected>A</option><option>B</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let checkbox = by_id(&app, "c");
        app.focus = Some(checkbox);
        let rect = field_rect(&app, checkbox);
        let mut frame = Frame::new(app.size.0, app.size.1);
        app.draw(&mut frame);
        assert!(
            frame
                .get(column(app.size.0).left + rect.x as u16, rect.y as u16)
                .attrs
                .contains(Attrs::REVERSE)
        );

        let select = by_id(&app, "s");
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        let rect = field_rect(&app, select);
        let mut frame = Frame::new(app.size.0, app.size.1);
        app.draw(&mut frame);
        let x = column(app.size.0).left + rect.x as u16;
        assert_eq!(frame.get(x, rect.y as u16).ch, 'B');
        assert!(frame.get(x, rect.y as u16).attrs.contains(Attrs::REVERSE));
    }

    fn choice_screen(app: &mut App, width: u16, height: u16) -> String {
        let mut frame = Frame::new(width, height);
        app.draw(&mut frame);
        let mut rows = Vec::new();
        for y in 0..height - 1 {
            let last = (0..width).rfind(|&x| {
                frame.get(x, y).ch != ' ' || frame.get(x, y).attrs.contains(Attrs::REVERSE)
            });
            let Some(last) = last else {
                continue;
            };
            rows.push((0..=last).map(|x| frame.get(x, y).ch).collect::<String>());
            rows.push(
                (0..=last)
                    .map(|x| {
                        if frame.get(x, y).attrs.contains(Attrs::REVERSE) {
                            '^'
                        } else {
                            '.'
                        }
                    })
                    .collect::<String>(),
            );
        }
        rows.join("\n")
    }

    #[test]
    fn multiple_select_windows_have_a_committed_before_during_and_after_snapshot() {
        let src = "<style>html,body{margin:0}</style><select id=s multiple size=2>\
                   <option>A</option><option selected>B</option><option>C</option>\
                   <option selected>D</option><option>E</option></select>";
        let (mut app, id) = scripted_app(src);
        app.size = (30, 6);
        app.update(Msg::RunScripts { id });
        app.relayout();
        let select = by_id(&app, "s");
        let before = choice_screen(&mut app, 30, 6);

        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::End, KeyModifiers::NONE));
        let during = choice_screen(&mut app, 30, 6);
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let after_escape = choice_screen(&mut app, 30, 6);

        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Home, KeyModifiers::NONE));
        app.update(key(KeyCode::Char(' '), KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let after_commit = choice_screen(&mut app, 30, 6);
        let actual = format!(
            "before\n{before}\n\nduring\n{during}\n\nafter escape\n{after_escape}\n\nafter commit\n{after_commit}"
        );
        assert_eq!(
            actual,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/snapshots/choice-select-mode.txt"
            ))
            .trim_end()
        );
    }

    #[test]
    fn choosing_a_radio_clears_only_its_group() {
        let src = "<form><input id=a type=radio name=x checked>\
                   <input id=b type=radio name=x><input id=d type=radio name=x>\
                   <input id=e type=radio name=x checked disabled></form>\
                   <form><input id=c type=radio name=x checked></form>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (a, b, c) = (by_id(&app, "a"), by_id(&app, "b"), by_id(&app, "c"));
        app.focus = Some(b);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let dom = app.dom.as_ref().unwrap();
        assert!(!layout::field::checked(dom, a, "input"));
        assert!(layout::field::checked(dom, b, "input"));
        assert!(layout::field::checked(dom, c, "input"));
        assert!(!layout::field::checked(dom, by_id(&app, "e"), "input"));
        let before = stages(&app);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default()
        );
        assert_eq!(stages(&app), before);
        let d = by_id(&app, "d");
        app.dom.as_mut().unwrap().set_attr(d, "checked", "");
        let dom = app.dom.as_ref().unwrap();
        assert!(layout::field::checked(dom, b, "input"));
        assert!(!layout::field::checked(dom, d, "input"));
    }

    #[test]
    fn select_mode_moves_a_cursor_and_enter_commits_the_choice() {
        let src = "<select id=s><option id=a selected>A</option>\
                   <option id=b>B</option><option id=c>C</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (select, second) = (by_id(&app, "s"), by_id(&app, "b"));
        app.focus = Some(select);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            redraw()
        );
        assert!(matches!(app.mode, Mode::Select { .. }));
        assert_eq!(app.update(key(KeyCode::Down, KeyModifiers::NONE)), redraw());
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            redraw()
        );
        assert!(matches!(app.mode, Mode::Browse));
        let dom = app.dom.as_ref().unwrap();
        let options = layout::field::options(dom, select);
        assert_eq!(
            layout::field::selected_options(dom, select, &options),
            [second]
        );
        assert_eq!(dom.attr(second, "selected"), None);
        let third = by_id(&app, "c");
        app.dom.as_mut().unwrap().set_attr(third, "selected", "");
        let dom = app.dom.as_ref().unwrap();
        assert_eq!(
            layout::field::selected_options(dom, select, &options),
            [second]
        );
    }

    #[test]
    fn select_mode_skips_disabled_options_and_esc_discards_a_single_cursor() {
        let src = "<select id=s><option id=a selected>A</option>\
                   <option disabled>B</option><option id=c>C</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (select, first, last) = (by_id(&app, "s"), by_id(&app, "a"), by_id(&app, "c"));
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == last));
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let dom = app.dom.as_ref().unwrap();
        let options = layout::field::options(dom, select);
        assert_eq!(
            layout::field::selected_options(dom, select, &options),
            [first]
        );
    }

    #[test]
    fn select_cursor_movement_skips_disabled_options_and_runs_no_stage() {
        let src = "<select id=s><option id=a>A</option><option disabled>B</option>\
                   <option id=c selected>C</option><option disabled>D</option>\
                   <option id=e>E</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (select, first, middle, last) = (
            by_id(&app, "s"),
            by_id(&app, "a"),
            by_id(&app, "c"),
            by_id(&app, "e"),
        );
        app.focus = Some(select);
        let before = stages(&app);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            redraw()
        );
        assert_eq!(stages(&app), before);

        for (key_code, expected, effect) in [
            (KeyCode::Up, first, redraw()),
            (KeyCode::Up, first, Effect::default()),
            (KeyCode::Down, middle, redraw()),
            (KeyCode::Down, last, redraw()),
            (KeyCode::Down, last, Effect::default()),
            (KeyCode::Home, first, redraw()),
            (KeyCode::Home, first, Effect::default()),
            (KeyCode::End, last, redraw()),
            (KeyCode::End, last, Effect::default()),
        ] {
            assert_eq!(app.update(key(key_code, KeyModifiers::NONE)), effect);
            assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == expected));
        }
        assert_eq!(stages(&app), before);
    }

    #[test]
    fn multiple_select_toggles_immediately_and_keeps_it_on_escape() {
        for exit in [KeyCode::Esc, KeyCode::Enter] {
            let src = "<select id=s multiple><option id=a>A</option><option>B</option></select>";
            let (mut app, id) = scripted_app(src);
            app.update(Msg::RunScripts { id });
            let (select, first) = (by_id(&app, "s"), by_id(&app, "a"));
            app.focus = Some(select);
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            app.update(key(KeyCode::Char(' '), KeyModifiers::NONE));
            let after_toggle = stages(&app);
            app.update(key(exit, KeyModifiers::NONE));
            assert!(matches!(app.mode, Mode::Browse));
            assert_eq!(stages(&app), after_toggle);
            let dom = app.dom.as_ref().unwrap();
            let options = layout::field::options(dom, select);
            assert_eq!(
                layout::field::selected_options(dom, select, &options),
                [first]
            );
        }
    }

    #[test]
    fn select_events_follow_changed_commits_and_multiple_toggles_only() {
        let (mut app, id) = scripted_app(
            "<select id=s><option selected>A</option><option>B</option></select><script>\
             var s = document.getElementById('s');\
             ['input','change'].forEach(function (n) { s.addEventListener(n, function () { console.log(n) }) });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        let select = by_id(&app, "s");
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let entries = app.console.entries();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["input", "change"]
        );

        let (mut app, id) = scripted_app(
            "<select id=s multiple><option>A</option><option>B</option></select><script>\
             var s = document.getElementById('s');\
             ['input','change'].forEach(function (n) { s.addEventListener(n, function () { console.log(n) }) });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        let select = by_id(&app, "s");
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Char(' '), KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let entries = app.console.entries();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["input", "change"]
        );
    }

    #[test]
    fn space_is_a_single_select_noop_and_a_multiple_select_structural_edit() {
        let (mut app, id) = scripted_app(
            "<select id=s><option id=a selected>A</option><option id=b>B</option></select>",
        );
        app.update(Msg::RunScripts { id });
        let (select, first, second) = (by_id(&app, "s"), by_id(&app, "a"), by_id(&app, "b"));
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        let before = stages(&app);
        assert_eq!(
            app.update(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            Effect::default()
        );
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == second));
        let dom = app.dom.as_ref().unwrap();
        let options = layout::field::options(dom, select);
        assert_eq!(
            layout::field::selected_options(dom, select, &options),
            [first]
        );
        assert_eq!(stages(&app), before);

        let (mut app, id) = scripted_app(
            "<select id=s multiple><option selected>A</option><option disabled>B</option>\
             <option id=c>C</option></select>",
        );
        app.update(Msg::RunScripts { id });
        let (select, third) = (by_id(&app, "s"), by_id(&app, "c"));
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        let (styles, layouts, paints) = stages(&app);
        assert_eq!(
            app.update(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            redraw()
        );
        assert_eq!(stages(&app), (styles, layouts + 1, paints + 1));
        assert!(app.dom.as_ref().unwrap().choice_state(third).unwrap());
    }

    #[test]
    fn select_exit_and_quit_keys_keep_their_mode_specific_meaning() {
        let src = "<a href=/before>before</a><select id=s><option>A</option></select>\
                   <a href=/after>after</a>";
        for (exit, expected) in [(KeyCode::Tab, "a /after"), (KeyCode::BackTab, "a /before")] {
            let (mut app, id) = scripted_app(src);
            app.update(Msg::RunScripts { id });
            let select = by_id(&app, "s");
            app.focus = Some(select);
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            let before = stages(&app);
            assert_eq!(app.update(key(exit, KeyModifiers::NONE)), redraw());
            assert!(matches!(app.mode, Mode::Browse));
            assert_eq!(focused(&app).as_deref(), Some(expected));
            assert_eq!(stages(&app), before);
            assert!(app.update(ch('q')).quit);
        }

        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let select = by_id(&app, "s");
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let before = stages(&app);
        assert_eq!(app.update(ch('q')), Effect::default());
        assert!(matches!(app.mode, Mode::Select { .. }));
        let quit = app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(quit.quit);
        assert!(!quit.dirty);
        assert_eq!(stages(&app), before);
    }

    #[test]
    fn dom_mutation_repairs_or_ends_select_mode() {
        let src = "<select id=s><option id=a>A</option><option id=b>B</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (select, first, second) = (by_id(&app, "s"), by_id(&app, "a"), by_id(&app, "b"));
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));

        let before = {
            let dom = app.dom.as_ref().unwrap();
            dom.change_versions()
        };
        app.dom.as_mut().unwrap().set_attr(second, "disabled", "");
        let after = {
            let dom = app.dom.as_ref().unwrap();
            dom.change_versions()
        };
        app.apply_dom_changes(before, after);
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == first));

        let before = after;
        app.dom.as_mut().unwrap().remove(select);
        let after = {
            let dom = app.dom.as_ref().unwrap();
            dom.change_versions()
        };
        app.apply_dom_changes(before, after);
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(app.focus, None);
    }

    #[test]
    fn timer_mutations_repair_or_end_select_mode() {
        let src = "<select id=s><option id=a>A</option><option id=b>B</option>\
                   <option id=c>C</option></select><script>\
                   setTimeout(function () { document.getElementById('b').remove(); }, 0);\
                   setTimeout(function () { document.getElementById('c').setAttribute('disabled', ''); }, 0);\
                   setTimeout(function () { document.getElementById('s').remove(); }, 0);\
                   </script>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (select, first, second, third) = (
            by_id(&app, "s"),
            by_id(&app, "a"),
            by_id(&app, "b"),
            by_id(&app, "c"),
        );
        app.focus = Some(select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == second));

        app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == first));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == third));

        app.update(Msg::Timer {
            page: id,
            id: TimerId(2),
        });
        assert!(matches!(app.mode, Mode::Select { cursor, .. } if cursor == first));
        assert_eq!(app.focus, Some(select));

        app.update(Msg::Timer {
            page: id,
            id: TimerId(3),
        });
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(app.focus, None);
        assert!(!app.dom.as_ref().unwrap().is_connected(select));
    }

    #[test]
    fn clean_choice_attributes_rebuild_glyphs_geometry_and_focusability() {
        let src = "<input id=c type=checkbox><select id=s>\
                   <option id=a>A</option><option id=b label='Long label'>B</option></select>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let (checkbox, select, second) = (by_id(&app, "c"), by_id(&app, "s"), by_id(&app, "b"));
        let before = {
            let dom = app.dom.as_ref().unwrap();
            dom.change_versions()
        };
        {
            let dom = app.dom.as_mut().unwrap();
            dom.set_attr(checkbox, "checked", "");
            dom.set_attr(checkbox, "disabled", "");
            dom.set_attr(second, "selected", "");
            dom.set_attr(second, "label", "A much longer label");
        }
        let after = {
            let dom = app.dom.as_ref().unwrap();
            dom.change_versions()
        };
        assert!(after.0 > before.0);
        let layouts = app.layouts;
        app.apply_dom_changes(before, after);
        assert_eq!(app.layouts, layouts + 1);

        let checkbox_box = app
            .layout_tree
            .as_ref()
            .unwrap()
            .boxes
            .iter()
            .find(|b| b.node == Some(checkbox) && matches!(b.kind, BoxKind::Field(_)))
            .unwrap();
        assert!(
            matches!(
                checkbox_box.kind,
                BoxKind::Field(layout::FieldPaint {
                    shows: layout::Shows::Checkbox(true),
                    disabled: true
                })
            ),
            "{:?}",
            checkbox_box.kind
        );
        assert_eq!(field_rect(&app, select).width, 21);
        let dom = app.dom.as_ref().unwrap();
        assert!(!layout::focusables(app.layout_tree.as_ref().unwrap(), dom).contains(&checkbox));
        let options = layout::field::options(dom, select);
        assert_eq!(
            layout::field::selected_options(dom, select, &options),
            [second]
        );
    }

    #[test]
    fn every_browse_binding_still_works_with_a_field_focused() {
        // The rule M11.8 was most at risk of breaking, and the reason M11.9
        // made typing a *mode* rather than a consequence of focus: a focused
        // field must not swallow the reader's browser. Every one of these keys
        // does what it does on any other page, because focusing a field is not
        // yet typing into one — `Enter` is.
        let tall = format!("{FORM_PAGE}{}", "<p>filler</p>".repeat(40));
        let (mut app, id) = scripted_app(&tall);
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(focused(&app).as_deref(), Some("input a"));

        // `j` scrolls and `gg` goes back to the top, with the field focused
        // throughout.
        app.update(ch('j'));
        assert!(app.viewport.offset() > 0, "j did not scroll");
        app.update(ch('g'));
        app.update(ch('g'));
        assert_eq!(app.viewport.offset(), 0, "gg did not reach the top");
        assert_eq!(
            focused(&app).as_deref(),
            Some("input a"),
            "a key stole focus"
        );

        // And `q` still quits, which is principle §1.5 and not negotiable.
        assert!(app.update(ch('q')).quit);
    }

    // ---- M11.9: typing into a field ---------------------------------------

    /// The caret, if the reader is typing.
    fn caret_of(app: &App) -> Option<usize> {
        match app.mode {
            Mode::Field { caret } => Some(caret),
            _ => None,
        }
    }

    /// What the focused control's *box* holds — the string layout will draw,
    /// which for a password is the mask and never the value.
    fn field_box_text(app: &App) -> Option<String> {
        let (focus, tree) = (app.focus?, app.layout_tree.as_ref()?);
        let mut out = None;
        tree.walk(tree.root, &mut |_, b| {
            if b.node == Some(focus) && matches!(b.kind, BoxKind::Field(_)) {
                out.clone_from(&b.text);
            }
        });
        out
    }

    /// The first `<input>` in the page, as a node.
    fn input_node(app: &App) -> NodeId {
        let dom = app.dom.as_ref().expect("a parsed page");
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&id| {
                matches!(&dom.node(id).data,
                crate::dom::NodeData::Element { tag, .. } if tag == "input")
            })
            .expect("the fixture's field")
    }

    /// `Tab` to the page's text field and `Enter` to start typing.
    fn start_typing(app: &mut App) {
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(focused(app).as_deref(), Some("input a"));
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            redraw()
        );
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            app.update(ch(c));
        }
    }

    #[test]
    fn enter_starts_typing_and_esc_gives_the_page_back() {
        // The mode discipline, end to end. Before `Enter`, `q` quits; after it,
        // `q` is a letter; after `Esc`, `q` quits again and the field is still
        // focused — which is the whole promise: the reader can always leave.
        let tall = format!("{FORM_PAGE}{}", "<p>filler</p>".repeat(40));
        let (mut app, id) = scripted_app(&tall);
        app.update(Msg::RunScripts { id });
        start_typing(&mut app);
        assert_eq!(caret_of(&app), Some(1), "the caret starts past the value");

        // Every browse binding is now a character. `q` above all.
        type_str(&mut app, "qj/g");
        assert_eq!(field_box_text(&app).as_deref(), Some("aqj/g"));
        assert_eq!(app.viewport.offset(), 0, "j scrolled while typing");

        assert_eq!(app.update(key(KeyCode::Esc, KeyModifiers::NONE)), redraw());
        assert!(caret_of(&app).is_none(), "Esc left the caret behind");
        assert_eq!(
            focused(&app).as_deref(),
            Some("input a"),
            "Esc dropped the focus as well as the mode"
        );

        // And the browse bindings are live again, immediately.
        app.update(ch('j'));
        assert!(app.viewport.offset() > 0, "j did not scroll after Esc");
        app.update(ch('g'));
        app.update(ch('g'));
        assert_eq!(app.viewport.offset(), 0, "gg did not reach the top");
        assert!(app.update(ch('/')).dirty);
        assert!(
            matches!(app.mode, Mode::SearchInput { .. }),
            "/ did not open"
        );
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.update(ch('q')).quit, "q did not quit after Esc");
    }

    #[test]
    fn ctrl_c_quits_from_typing_with_a_half_typed_value() {
        // PLAN.md §3: quit always works. In a field `q` is a letter, so the
        // promise is kept by `Ctrl-c` — exactly as it is in the URL bar.
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        start_typing(&mut app);
        type_str(&mut app, "half");
        assert_eq!(field_box_text(&app).as_deref(), Some("ahalf"));
        assert!(
            app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .quit,
            "Ctrl-c did not quit from typing mode"
        );
    }

    #[test]
    fn what_a_reader_typed_never_touches_the_value_attribute() {
        // M11.8's rule, and the reason the store exists: `value` is the
        // *default*, so a stylesheet's `input[value="a"]` must keep matching
        // the markup however much has been typed over it. M11.2 made attribute
        // selectors real, which is what makes this a live wire.
        let (mut app, id) = scripted_app(&format!(
            "<style>input[value=\"a\"] {{ font-weight: bold }}</style>{FORM_PAGE}"
        ));
        app.update(Msg::RunScripts { id });
        let input = input_node(&app);
        start_typing(&mut app);
        type_str(&mut app, "bcd");

        let dom = app.dom.as_ref().unwrap();
        assert_eq!(dom.attr(input, "value"), Some("a"), "typing wrote through");
        assert_eq!(dom.field_value(input), Some("abcd"));

        // Restyled from scratch — the selector is asked again, and still says
        // yes. (Typing itself runs no restyle; this is the assertion that the
        // *next* one would answer the same.)
        app.restyle();
        assert_eq!(
            app.styles.as_ref().unwrap().get(input).font_weight,
            crate::style::values::FontWeight::Bold,
            "input[value=\"a\"] stopped matching what the page wrote"
        );
    }

    #[test]
    fn a_readonly_field_is_focusable_and_takes_no_caret() {
        // HTML's own split, and the reason it lives in `editable_value` rather
        // than in M11.8's focus rule: `readonly` is showing you something, so
        // it stays reachable, and the only thing it refuses is the edit.
        let (mut app, id) = scripted_app("<p><input size=8 value=shown readonly></p>");
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.focus.is_some(), "readonly left the Tab cycle");
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default(),
            "Enter opened a mode on a readonly field"
        );
        assert!(app.update(ch('q')).quit);
    }

    #[test]
    fn a_disabled_field_cannot_be_typed_into() {
        // It is not in the Tab cycle (M11.8) — and `Enter` on it opens no mode
        // either, because "cannot be reached" and "cannot be typed into" are
        // different claims and a click, a script or a later focus rule could
        // make the first one false.
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        let dom = app.dom.as_ref().unwrap();
        let disabled = (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&n| dom.attr(n, "disabled").is_some())
            .expect("the disabled field");
        app.focus = Some(disabled);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default(),
            "Enter opened a mode on a disabled field"
        );
        assert!(caret_of(&app).is_none());
        // And a character is still a browse binding, not a keystroke into it.
        assert!(app.update(ch('q')).quit);
    }

    #[test]
    fn typing_runs_no_restyle_at_all() {
        // **A keystroke is not a cascade event.** The value is state beside the
        // tree rather than an attribute, so no selector's answer can have moved
        // and the cascade must not run — which is the whole reason M11.8 put
        // the value where it did, and the counter is what says it worked.
        //
        // A relayout each, though, deliberately: it is a text edit and the
        // measurement said the simple path fits the budget (see
        // `after_field_edit`).
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        start_typing(&mut app);
        let (styles_run, layouts, paints) = stages(&app);
        type_str(&mut app, "xyz");
        assert_eq!(
            stages(&app),
            (styles_run, layouts + 3, paints + 3),
            "a keystroke restyled, or ran a stage twice"
        );
        // Moving the caret is chrome: it lays out nothing and paints nothing.
        app.update(key(KeyCode::Left, KeyModifiers::NONE));
        app.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(stages(&app), (styles_run, layouts + 3, paints + 3));
    }

    #[test]
    fn the_narrowed_keystroke_path_runs_no_layout() {
        // The B side of the measurement, so its number means something: with
        // the narrowing on, a keystroke lays nothing out and still puts the
        // character on screen.
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        app.narrow_keystroke = true;
        start_typing(&mut app);
        let (styles_run, layouts, paints) = stages(&app);
        type_str(&mut app, "xyz");
        assert_eq!(stages(&app), (styles_run, layouts, paints + 3));
        assert_eq!(field_box_text(&app).as_deref(), Some("axyz"));
    }

    #[test]
    fn a_reader_edit_writes_before_its_input_listener_and_runs_one_cycle() {
        let (mut app, id) = scripted_app(
            "<input id=x value=a><script>\
             document.getElementById('x').addEventListener('input', function (e) { console.log(e.target.value); });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        start_typing(&mut app);
        let (styles, layouts, paints) = stages(&app);
        app.update(key(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(stages(&app), (styles, layouts + 1, paints + 1));
        assert_eq!(
            app.console
                .entries()
                .last()
                .map(|entry| entry.text.as_str()),
            Some("ab")
        );
    }

    #[test]
    fn tab_commits_text_before_blur_and_next_focus() {
        let (mut app, id) = scripted_app(
            "<input id=a><input id=b><script>\
             ['a', 'b'].forEach(function (id) {\
               var e = document.getElementById(id);\
               ['change', 'blur', 'focus'].forEach(function (kind) {\
                 e.addEventListener(kind, function () { console.log(kind + ':' + id); });\
               });\
             });</script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
        let at = app.console.entries().len();
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.console.entries()[at..]
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["change:a", "blur:a", "focus:b"]
        );
    }

    #[test]
    fn the_caret_moves_and_deletes_on_the_side_it_was_asked_about() {
        let (mut app, id) = scripted_app("<p><input size=8 value=abc></p>");
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(caret_of(&app), Some(3));

        // Home / End are the ends of the line; ← and → are one character.
        app.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(caret_of(&app), Some(0));
        app.update(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(caret_of(&app), Some(1));
        // Inserting happens *at* the caret, not at the end.
        type_str(&mut app, "-");
        assert_eq!(field_box_text(&app).as_deref(), Some("a-bc"));
        assert_eq!(caret_of(&app), Some(2));

        // Backspace takes what is before it, Delete what is after.
        app.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(field_box_text(&app).as_deref(), Some("abc"));
        assert_eq!(caret_of(&app), Some(1));
        app.update(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(field_box_text(&app).as_deref(), Some("ac"));
        assert_eq!(caret_of(&app), Some(1));

        // At either end there is nothing to take, and nothing to redraw.
        app.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(
            app.update(key(KeyCode::Backspace, KeyModifiers::NONE)),
            Effect::default(),
            "Backspace at the start invented a change"
        );
        app.update(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(
            app.update(key(KeyCode::Delete, KeyModifiers::NONE)),
            Effect::default(),
            "Delete at the end invented a change"
        );
        assert_eq!(field_box_text(&app).as_deref(), Some("ac"));
    }

    #[test]
    fn a_textarea_types_line_by_line_and_enter_never_inserts_a_newline() {
        // A `<textarea>` is a field like any other, and its value keeps the
        // newlines the markup gave it: `Home`/`End` are the ends of the line
        // the caret is on, not of the whole box.
        //
        // `Enter` submits (M11.10) — in a browser it would insert a newline
        // here, and in the one place a newline is legal this engine spends the
        // key on the submission instead, because it is the only activator a
        // keyboard has. A reader cannot type a line break into a textarea;
        // that is a standing deviation, and this is where it is pinned. In a
        // page with no form there is nothing to submit, so it does nothing.
        let (mut app, id) = scripted_app("<p><textarea cols=8 rows=3>one\ntwo</textarea></p>");
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        assert_eq!(caret_of(&app), Some(7), "the caret starts past the value");

        app.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(caret_of(&app), Some(4), "Home left the caret's own line");
        type_str(&mut app, "X");
        assert_eq!(field_box_text(&app).as_deref(), Some("one\nXtwo"));
        app.update(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(caret_of(&app), Some(8));

        // ← across the line break: the newline is a character the caret moves
        // over, and the value is one string.
        for _ in 0..4 {
            app.update(key(KeyCode::Left, KeyModifiers::NONE));
        }
        assert_eq!(caret_of(&app), Some(4));
        app.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(field_box_text(&app).as_deref(), Some("oneXtwo"));

        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default(),
            "Enter did something in a page with no form to submit"
        );
        assert_eq!(
            field_box_text(&app).as_deref(),
            Some("oneXtwo"),
            "Enter inserted a newline"
        );

        // The same key in a form sends it, newlines and all — as CRLF, which
        // is HTML's rule and `browser::form`'s comment.
        //
        // And the action resolves against the URL the page *arrived* at:
        // `scripted_app` asks for `http://x/` and is answered by
        // `http://final/`, the way a redirect answers (M11.7a). Submitting to
        // the URL the reader typed would send a login form to the host that
        // redirected away from it.
        let (mut app, id) = scripted_app(
            "<form action=/post><textarea name=t cols=8 rows=3>one\ntwo</textarea></form>",
        );
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            effect.fetch.map(|(_, request)| request.url).as_deref(),
            Some("http://final/post?t=one%0D%0Atwo")
        );
    }

    #[test]
    fn tab_out_of_a_field_leaves_typing_behind() {
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        start_typing(&mut app);
        type_str(&mut app, "x");
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(caret_of(&app).is_none(), "Tab kept the reader typing");
        assert_eq!(focused(&app).as_deref(), Some("button"));
        // What was typed is still there — nothing is committed and nothing is
        // discarded; the value has been state since the first keystroke.
        let dom = app.dom.as_ref().unwrap();
        assert_eq!(dom.field_value(input_node(&app)), Some("ax"));
        assert!(app.update(ch('q')).quit, "q did not quit after Tab");
    }

    #[test]
    fn a_patched_tree_is_the_tree_a_fresh_layout_would_have_built() {
        // The narrowing is the measurement's B side, and a B side that produced
        // the *wrong* tree would be no measurement at all: whatever it patches,
        // the result has to be what the shipped path would have laid out.
        let page = "<p><input size=8 value=a> <textarea cols=6 rows=2>hi</textarea></p>";
        let (mut app, id) = scripted_app(page);
        app.update(Msg::RunScripts { id });
        app.narrow_keystroke = true;
        start_typing_at_first_field(&mut app);
        type_str(&mut app, "bc");

        let dom = app.dom.as_ref().unwrap();
        let styles = app.styles.as_ref().unwrap();
        let fresh = layout::layout_document_with(
            dom,
            styles,
            column(app.size.0).width,
            layout::Hidden::Respect,
            &app.images.context(),
        );
        let patched = app.layout_tree.as_ref().unwrap();
        let describe = |tree: &LayoutTree| {
            let mut out = Vec::new();
            tree.walk(tree.root, &mut |_, b| {
                out.push((b.kind, b.node, b.dimensions, b.text.clone()));
            });
            out
        };
        assert_eq!(
            describe(patched),
            describe(&fresh),
            "the patched tree is not the tree a relayout would have produced"
        );
    }

    /// `Tab` once (the first focusable is the field here) and `Enter`.
    fn start_typing_at_first_field(app: &mut App) {
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(caret_of(app).is_some(), "Enter did not start typing");
    }

    #[test]
    fn the_statusline_says_which_mode_the_reader_is_in() {
        // A mode a reader can be surprised by has to be visible (UX §1): every
        // letter goes into the page instead of doing what it usually does.
        let (mut app, id) = scripted_app(FORM_PAGE);
        app.update(Msg::RunScripts { id });
        assert!(!screen(&mut app, 60, 8).contains("[typing"));
        start_typing(&mut app);
        assert!(
            screen(&mut app, 60, 8).contains("[typing · Esc]"),
            "{}",
            screen(&mut app, 60, 8)
        );
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!screen(&mut app, 60, 8).contains("[typing"));
    }

    #[test]
    fn typing_scrolls_the_value_under_the_caret_in_cells() {
        // Deliverable 3, on screen: a value longer than its box shows the
        // window the caret is in, and `Home` snaps back to the start. Measured
        // in cells — the value is CJK, where a caret counted in `chars()` sits
        // half a glyph off.
        let (mut app, id) = scripted_app("<p><input size=6 value=漢字></p>");
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        // The frame's own row, minus the page column's gutter and the nul cells
        // that stand for the second half of a wide glyph.
        let row = |app: &mut App| {
            screen(app, 30, 4)
                .lines()
                .next()
                .unwrap()
                .replace('\0', "")
                .trim()
                .to_string()
        };
        assert_eq!(row(&mut app), "[漢字  ]");
        // Six cells of value in a six-cell box: the caret needs a seventh, so
        // the window scrolls by one cell and the first glyph is half-cut.
        type_str(&mut app, "漢");
        assert_eq!(row(&mut app), "[ 字漢 ]");
        // Home snaps back and shows the start again.
        app.update(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(row(&mut app), "[漢字漢]");
    }

    #[test]
    fn the_page_never_sees_a_key() {
        // DEVIATIONS.md: key events never reach the page — yata's bindings
        // always win. No keyboard event reaches the page; `input` is instead
        // the observable result of an edit (M11.13), and cannot change the
        // keybinding that produced it.
        let (mut app, id) = scripted_app(
            "<p><input id=f size=8 value=a></p><script>\
             var f = document.getElementById('f');\
             ['keydown','keypress','keyup','input','change','focus','blur'].forEach(function (n) {\
               f.addEventListener(n, function () { console.log('page saw ' + n) });\
             });</script>",
        );
        app.update(Msg::RunScripts { id });
        // A listener runs synchronously inside its dispatch, so anything it
        // logged would already be here.
        let logged = app.console.entries().len();
        start_typing_at_first_field(&mut app);
        type_str(&mut app, "xy");
        app.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        app.update(key(KeyCode::Delete, KeyModifiers::NONE));
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let seen = &app.console.entries()[logged..];
        let seen = seen
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            [
                "page saw focus",
                "page saw input",
                "page saw input",
                "page saw input"
            ],
            "only focus transitions and real edits are observable: {seen:?}"
        );
    }

    #[test]
    fn keyboard_and_mouse_choice_activation_dispatches_native_events() {
        let src = "<form id=f><input id=kc type=checkbox>\
                   <input id=kr type=radio name=k><input id=mc type=checkbox>\
                   <input id=mr type=radio name=m>\
                   <select id=ks><option>A</option><option>B</option></select>\
                   <select id=ms><option>A</option></select></form><script>\
                   ['kc','kr','mc','mr','ks','ms'].forEach(function (id) {\
                     var control = document.getElementById(id);\
                     ['click','input','change','focus','blur','submit'].forEach(function (name) {\
                       control.addEventListener(name, function () { console.log(id + ':' + name); });\
                     });\
                   });\
                   document.getElementById('f').addEventListener('submit', function () { console.log('form:submit'); });\
                   </script>";
        let (mut app, id) = scripted_app(src);
        app.update(Msg::RunScripts { id });
        let logged = app.console.entries().len();

        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let mouse_checkbox = by_id(&app, "mc");
        let mouse_radio = by_id(&app, "mr");
        click_control(&mut app, mouse_checkbox);
        click_control(&mut app, mouse_radio);
        let keyboard_select = by_id(&app, "ks");
        app.focus = Some(keyboard_select);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        app.update(key(KeyCode::Down, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let mouse_select = by_id(&app, "ms");
        click_control(&mut app, mouse_select);

        let dom = app.dom.as_ref().unwrap();
        for id in ["kc", "kr", "mc", "mr"] {
            assert!(layout::field::checked(dom, by_id(&app, id), "input"));
        }
        let entries = app.console.entries();
        let seen = entries[logged..]
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            [
                "kc:focus",
                "kc:click",
                "kc:input",
                "kc:change",
                "kc:blur",
                "kr:focus",
                "kr:click",
                "kr:input",
                "kr:change",
                "kr:blur",
                "mc:focus",
                "mc:click",
                "mc:input",
                "mc:change",
                "mc:blur",
                "mr:focus",
                "mr:click",
                "mr:input",
                "mr:change",
                "ks:input",
                "ks:change",
                "ks:blur",
                "ms:focus",
                "ms:click",
            ]
        );
    }

    #[test]
    fn cancelled_choice_click_restores_its_clean_live_state() {
        let (mut app, id) = scripted_app(
            "<input id=c type=checkbox><script>\
             var c = document.getElementById('c');\
             c.addEventListener('click', function (e) { console.log(c.checked); e.preventDefault() });\
             c.addEventListener('input', function () { console.log('input') });\
             c.addEventListener('change', function () { console.log('change') });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));

        let checkbox = by_id(&app, "c");
        let dom = app.dom.as_ref().unwrap();
        assert!(!layout::field::checked(dom, checkbox, "input"));
        assert_eq!(dom.choice_state(checkbox), None);
        let entries = app.console.entries();
        assert!(entries.iter().any(|entry| entry.text == "true"));
        assert!(!entries.iter().any(|entry| entry.text == "input"));
        assert!(!entries.iter().any(|entry| entry.text == "change"));
    }

    #[test]
    fn an_already_checked_radio_dispatches_click_only() {
        let (mut app, id) = scripted_app(
            "<input id=r type=radio name=g checked><script>\
             var r = document.getElementById('r');\
             ['click','input','change'].forEach(function (name) {\
               r.addEventListener(name, function () { console.log(name) });\
             });</script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let entries = app.console.entries();
        let seen = entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(seen, ["click"]);
    }

    #[test]
    fn a_blur_listener_removing_the_next_target_clears_focus_silently() {
        let (mut app, id) = scripted_app(
            "<input id=a><input id=b><script>\
             document.getElementById('a').addEventListener('blur', function () {\
               document.getElementById('b').remove();\
             });\
             document.getElementById('b').addEventListener('focus', function () { console.log('bad focus') });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, None);
        assert!(
            !app.console
                .entries()
                .iter()
                .any(|entry| entry.text == "bad focus"),
            "a detached node received focus"
        );
    }

    // ---- M11.10: `<form>` GET ---------------------------------------------

    /// A page loaded from `url` and parsed, in an app of the given size — the
    /// shape a navigation has, with the URL an `action` resolves against.
    fn page_from(url: &str, html: &str, w: u16, h: u16) -> (App, PageId) {
        let mut app = App::new(w, h);
        let id = load_with_cookies(&mut app, url, html, &[]);
        (app, id)
    }

    /// The URL a submission's `Effect` asks the loop to fetch.
    fn fetched(effect: &Effect) -> Option<&str> {
        effect
            .fetch
            .as_ref()
            .map(|(_, request)| request.url.as_str())
    }

    fn fetched_request(effect: &Effect) -> Option<&net::Request> {
        effect.fetch.as_ref().map(|(_, request)| request)
    }

    /// The first element with this `id`.
    fn by_id(app: &App, id: &str) -> NodeId {
        let dom = app.dom.as_ref().expect("a parsed page");
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap_or_else(|| panic!("the fixture lost #{id}"))
    }

    /// Click the first cell of a node's box, having scrolled it on screen —
    /// the mouse's way of activating something, with the focus left where a
    /// reader's would be (nowhere) so the click has to do the focusing itself.
    fn click_node(app: &mut App, node: NodeId) -> Effect {
        app.focus = Some(node);
        app.scroll_focus_into_view();
        app.focus = None;
        let tree = app.layout_tree.as_ref().expect("a laid-out page");
        let mut at = None;
        tree.walk(tree.root, &mut |_, b| {
            if at.is_none() && b.node == Some(node) && matches!(b.kind, BoxKind::Field(_)) {
                at = Some((b.dimensions.content.x, b.dimensions.content.y));
            }
        });
        let (x, y) = at.expect("that node generated no control box");
        let row = y - app.viewport.offset() as i32;
        let column = column(app.size.0).left + x as u16;
        app.update(Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        }))
    }

    #[test]
    fn hns_search_box_searches() {
        // **The acceptance case**, end to end and asserted on the `Effect`
        // rather than on the network. HN's form is one line and has *no submit
        // button at all*, so `Enter` while typing is the only way a reader can
        // ever send it — which is the key M11.9 left unbound for this task.
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/news.ycombinator.com.html"
        ));
        // Over `http` deliberately: the action is protocol-relative
        // (`//hn.algolia.com/`), so the scheme it inherits is the page's and a
        // test that only ever used `https` could not tell.
        let (mut app, _) = page_from("http://news.ycombinator.com/news", fixture, 80, 30);

        // The search box is the last focusable on the page, so backwards is
        // one press rather than two hundred.
        app.update(key(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(
            app.dom.as_ref().unwrap().attr(app.focus.unwrap(), "name"),
            Some("q"),
            "Shift-Tab did not land on HN's search box"
        );
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        type_str(&mut app, "redirect");
        assert!(caret_of(&app).is_some(), "the reader is not typing");

        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&effect), Some("http://hn.algolia.com/?q=redirect"));
        assert!(
            matches!(
                fetched_request(&effect).map(|r| &r.method),
                Some(net::Method::Get)
            ),
            "HN's search grew a body"
        );

        // Typing is over, and the browse bindings are live while the answer is
        // still in flight — `q` quits a loading page.
        assert!(caret_of(&app).is_none(), "the reader is still typing");
        assert!(matches!(app.fetch, Fetch::Loading { .. }));
        assert!(matches!(app.mode, Mode::Browse));
        assert!(
            app.update(ch('q')).quit,
            "q did not quit after a submission"
        );

        // And `H` goes back to the story list, because a submission is a
        // navigation and history was pushed.
        let back = app.update(ch('H'));
        assert_eq!(fetched(&back), Some("http://news.ycombinator.com/news"));
    }

    #[test]
    fn reader_choices_reach_get_and_post_requests_through_the_same_submission_path() {
        for method in ["", " method=post"] {
            let html = format!(
                "<form{method} action=/save><input id=c type=checkbox name=c value=yes>\
                 <select id=s name=s><option value=a>A</option><option value=b>B</option></select>\
                 <button id=go>Save</button></form>"
            );
            let (mut app, _) = page_from("http://site.test/page", &html, 80, 10);
            let checkbox = by_id(&app, "c");
            app.focus = Some(checkbox);
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            let select = by_id(&app, "s");
            app.focus = Some(select);
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            app.update(key(KeyCode::Down, KeyModifiers::NONE));
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            app.focus = Some(by_id(&app, "go"));
            let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            let (_, request) = effect.fetch.as_ref().expect("choice form did not submit");
            if method.is_empty() {
                assert_eq!(request.url, "http://site.test/save?c=yes&s=b");
                assert_eq!(request.method, net::Method::Get);
            } else {
                assert_eq!(request.url, "http://site.test/save");
                assert_eq!(
                    request.method,
                    net::Method::Post {
                        body: "c=yes&s=b".into()
                    }
                );
            }
        }
    }

    #[test]
    fn wikipedias_search_sends_its_hidden_field_however_it_is_activated() {
        // Tree order and the hidden `title=Special:Search` — the difference
        // between a search that works and one that lands on the wrong page.
        // Then the same form again by *clicking* its `<button>` (which has no
        // `type`, so HTML says it is a submit button, and no `name`, so it
        // contributes nothing): the two activations are one function, so they
        // produce one request.
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        let (mut app, _) = page_from("https://en.wikipedia.org/wiki/Cat", fixture, 80, 30);
        let field = by_id(&app, "searchInput");
        let form = form::owner(app.dom.as_ref().unwrap(), field).expect("#searchform");
        let button = {
            let dom = app.dom.as_ref().unwrap();
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&n| {
                    layout::is_under(dom, n, form)
                        && matches!(&dom.node(n).data,
                            crate::dom::NodeData::Element { tag, .. } if tag == "button")
                })
                .expect("the form's Search button")
        };
        let expected = "https://en.wikipedia.org/w/index.php?search=cat&title=Special%3ASearch";

        app.focus = Some(field);
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        type_str(&mut app, "cat");
        let typed = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&typed), Some(expected));
        assert!(
            matches!(
                fetched_request(&typed).map(|r| &r.method),
                Some(net::Method::Get)
            ),
            "Wikipedia's search grew a body"
        );

        // Back on the article, the same value still in the box, and this time
        // the button is pressed.
        let (mut app, _) = page_from("https://en.wikipedia.org/wiki/Cat", fixture, 80, 30);
        let field = by_id(&app, "searchInput");
        app.dom.as_mut().unwrap().set_field_value(field, "cat");
        let clicked = click_node(&mut app, button);
        assert_eq!(
            fetched(&clicked),
            Some(expected),
            "the click sent something else"
        );
        // The click focused the button before activating it (M11.8's rule,
        // unchanged) — and then the navigation cleared the focus, which is why
        // this is asserted on the request and not on `app.focus`.
        assert!(app.focus.is_none());
    }

    #[test]
    fn enter_on_a_focused_submit_button_submits() {
        // The third activator: `Enter` in Browse on a focused submit button —
        // Wikipedia's `<button>` with no `type`, and `<input type=submit>`.
        // The Wikipedia fixture covers Field-mode `Enter` and a click; the
        // type=button/reset test covers Browse `Enter` only for the *non*-
        // submit case. A later `follow_focus` refactor cannot drop this
        // branch without failing here.
        let html = "<form action=/search>\
            <input name=q value=cat>\
            <button name=go value=1 id=go>Search</button>\
            <input type=submit name=alt value=2 id=alt></form>";

        let (mut app, _) = page_from("http://x/page", html, 60, 8);
        app.focus = Some(by_id(&app, "go"));
        let keyed = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            fetched(&keyed),
            Some("http://x/search?q=cat&go=1"),
            "Enter on the focused <button> did not submit"
        );

        let (mut app, _) = page_from("http://x/page", html, 60, 8);
        app.focus = Some(by_id(&app, "alt"));
        let keyed = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            fetched(&keyed),
            Some("http://x/search?q=cat&alt=2"),
            "Enter on the focused <input type=submit> did not submit"
        );

        // The same button, clicked: one function, one request.
        let (mut app, _) = page_from("http://x/page", html, 60, 8);
        let button = by_id(&app, "go");
        let clicked = click_node(&mut app, button);
        assert_eq!(
            fetched(&clicked),
            Some("http://x/search?q=cat&go=1"),
            "clicking the button sent something else"
        );
    }

    #[test]
    fn a_two_field_button_less_form_submits_on_enter() {
        // **The deviation** (deliverable 2): HTML submits a button-less form on
        // `Enter` only when it has exactly one field that blocks implicit
        // submission, so this form could not be submitted *at all*. In a GUI
        // browser that is survivable — there is nearly always a button, and a
        // mouse to press it. Here `Enter` is the only activator a keyboard has,
        // and the reader pressed it twice, deliberately, in a mode the
        // statusline named.
        let (mut app, _) = page_from(
            "http://x/page",
            "<form action=/search><input name=a value=1><input name=b value=2></form>",
            40,
            10,
        );
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            fetched(&effect),
            Some("http://x/search?a=1&b=2"),
            "both fields, in tree order"
        );
    }

    fn login_form() -> &'static str {
        "<form method=post action=/login>\
         <input name=acct value=pg>\
         <input type=password name=pw value=secret>\
         <button>login</button></form>"
    }

    fn assert_login_post(request: &net::Request) {
        assert_eq!(request.url, "http://site.test/login");
        assert!(
            !request.url.contains("secret") && !request.url.contains("acct="),
            "a password landed in the URL: {}",
            request.url
        );
        match &request.method {
            net::Method::Post { body } => assert_eq!(body, "acct=pg&pw=secret"),
            other => panic!("expected POST, got {other:?}"),
        }
    }

    fn login_page() -> (App, PageId) {
        let mut app = App::new(80, 8);
        let id = load_with_cookies(
            &mut app,
            "http://site.test/page",
            login_form(),
            &["sid=abc; Path=/"],
        );
        app.update(Msg::RunScripts { id });
        (app, id)
    }

    #[test]
    fn a_login_shaped_post_hops_to_get_with_the_cookie() {
        // **The acceptance case.** POST /login with the fields in the body
        // and the jar's `Cookie:` for that URL, then a 302 that hands out a
        // session, then GET /app carrying it — same PageId, no body on the
        // hop, history pointing at the form page.
        let (mut app, _) = login_page();
        let before = stages(&app);
        start_typing_at_first_field(&mut app);

        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let (fetch_id, request) = effect.fetch.as_ref().expect("a POST did not navigate");
        assert_login_post(request);
        assert_eq!(request.cookie.as_deref(), Some("sid=abc"));
        assert_eq!(Some(*fetch_id), app.current_fetch);
        assert_eq!(stages(&app), before, "a POST ran a pipeline stage");
        assert!(app.history.can_back(), "a POST did not push history");
        assert!(
            !app.status_left().contains("secret"),
            "the password is in the statusline: {}",
            app.status_left()
        );
        assert!(caret_of(&app).is_none(), "the reader is still typing");
        assert!(app.update(ch('q')).quit, "q did not quit after a POST");

        // The 302: same generation, GET, cookie from the hop, no body.
        let (mut app, _) = login_page();
        start_typing_at_first_field(&mut app);
        let posted = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let fetch_id = posted.fetch.as_ref().unwrap().0;
        let hop = app.update(hop_at(
            fetch_id,
            "http://site.test/login",
            "http://site.test/app",
            302,
            &["sid=abc; Path=/"],
        ));
        let next = next_request(&hop).expect("the 302 did not re-spawn");
        assert_eq!(next.url, "http://site.test/app");
        assert!(matches!(next.method, net::Method::Get), "{:?}", next.method);
        assert_eq!(next.cookie.as_deref(), Some("sid=abc"));
        assert_eq!(Some(fetch_id), app.current_fetch);

        let back = app.update(ch('H'));
        assert_eq!(fetched(&back), Some("http://site.test/page"));
        assert!(
            matches!(
                fetched_request(&back).map(|r| &r.method),
                Some(net::Method::Get)
            ),
            "H resubmitted the POST"
        );
        assert!(
            fetched(&back).is_some_and(|u| !u.contains("secret")),
            "H carried the password"
        );
    }

    #[test]
    fn enter_and_click_produce_the_same_post() {
        let (mut app, id) = page_from("http://site.test/page", login_form(), 80, 8);
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let keyed = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_login_post(fetched_request(&keyed).unwrap());

        let (mut app, id) = page_from("http://site.test/page", login_form(), 80, 8);
        app.update(Msg::RunScripts { id });
        let button = {
            let dom = app.dom.as_ref().unwrap();
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&n| {
                    matches!(&dom.node(n).data,
                    crate::dom::NodeData::Element { tag, .. } if tag == "button")
                })
                .expect("the login button")
        };
        let clicked = click_node(&mut app, button);
        assert_login_post(fetched_request(&clicked).unwrap());
    }

    #[test]
    fn post_redirects_rewrite_the_method() {
        let post = |status: u16| {
            let (mut app, id) = page_from("http://site.test/page", login_form(), 80, 8);
            app.update(Msg::RunScripts { id });
            start_typing_at_first_field(&mut app);
            let posted = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            let fetch_id = posted.fetch.as_ref().unwrap().0;
            let hop = app.update(hop_at(
                fetch_id,
                "http://site.test/login",
                "http://site.test/app",
                status,
                &[],
            ));
            fetched_request(&hop).unwrap().method.clone()
        };
        assert!(matches!(post(301), net::Method::Get), "301 kept POST");
        assert!(matches!(post(302), net::Method::Get), "302 kept POST");
        assert!(matches!(post(303), net::Method::Get), "303 kept POST");
        match post(307) {
            net::Method::Post { body } => assert_eq!(body, "acct=pg&pw=secret"),
            other => panic!("307 dropped the body: {other:?}"),
        }
        match post(308) {
            net::Method::Post { body } => assert_eq!(body, "acct=pg&pw=secret"),
            other => panic!("308 dropped the body: {other:?}"),
        }

        // In-flight memory updates each hop: 307 keeps POST, 303 then drops it.
        let (mut app, id) = page_from("http://site.test/page", login_form(), 80, 8);
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let posted = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let fetch_id = posted.fetch.as_ref().unwrap().0;
        let mid = app.update(hop_at(
            fetch_id,
            "http://site.test/login",
            "http://site.test/mid",
            307,
            &[],
        ));
        assert!(matches!(
            fetched_request(&mid).map(|r| &r.method),
            Some(net::Method::Post { .. })
        ));
        let land = app.update(hop_at(
            fetch_id,
            "http://site.test/mid",
            "http://site.test/app",
            303,
            &[],
        ));
        assert!(
            matches!(
                fetched_request(&land).map(|r| &r.method),
                Some(net::Method::Get)
            ),
            "the body came back after the 303"
        );
    }

    #[test]
    fn a_post_to_the_document_url_fetches_it_does_not_jump() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form method=post action=#here><input name=q value=v></form>",
            40,
            10,
        );
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let request = fetched_request(&effect).expect("a same-document POST jumped");
        assert!(
            request.url.contains('#') || request.url.ends_with("/page"),
            "{}",
            request.url
        );
        assert!(matches!(&request.method, net::Method::Post { body } if body == "q=v"));
    }

    #[test]
    fn reload_after_a_post_200_is_a_get_with_no_body() {
        let (mut app, id) = page_from("http://site.test/page", login_form(), 80, 8);
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let posted = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let fetch_id = posted.fetch.as_ref().unwrap().0;
        app.update(Msg::Loaded {
            id: fetch_id,
            url: "http://site.test/login".into(),
            status: 200,
            body: b"<p>ok</p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(fetch_id, "<p>ok</p>"));
        let reloaded = app.update(ch('r'));
        let request = fetched_request(&reloaded).expect("reload did nothing");
        assert_eq!(request.url, "http://site.test/login");
        assert!(matches!(request.method, net::Method::Get));
        assert!(!request.url.contains("secret"));
    }

    #[test]
    fn dialog_and_multipart_still_refuse() {
        for (html, needle) in [
            (
                "<form method=dialog action=/x><input name=q value=v></form>",
                "can't send DIALOG",
            ),
            (
                "<form method=post enctype=multipart/form-data action=/x>\
                 <input name=q value=v></form>",
                "can't send files",
            ),
            (
                "<form method=post enctype=text/plain action=/x>\
                 <input name=q value=v></form>",
                "can't send PLAIN",
            ),
        ] {
            let (mut app, id) = page_from("http://x/page", html, 80, 8);
            app.update(Msg::RunScripts { id });
            let before = stages(&app);
            start_typing_at_first_field(&mut app);
            let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            assert!(effect.fetch.is_none(), "{html} navigated");
            assert_eq!(stages(&app), before, "{html} ran a stage");
            assert!(!app.history.can_back(), "{html} pushed history");
            let shown = screen(&mut app, 80, 8);
            assert!(shown.contains(needle), "missing {needle:?} in {shown}");
        }
    }

    #[test]
    fn a_post_body_over_the_cap_does_not_navigate() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form method=post action=/x><input name=q value=v>\
             <input type=hidden name=h></form>",
            80,
            8,
        );
        app.update(Msg::RunScripts { id });
        let hidden = {
            let dom = app.dom.as_ref().unwrap();
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&n| dom.attr(n, "name") == Some("h"))
                .expect("the hidden field")
        };
        app.dom
            .as_mut()
            .unwrap()
            .set_field_value(hidden, &"x".repeat(form::MAX_POST_BODY));
        let before = stages(&app);
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(effect.fetch.is_none(), "an over-cap POST navigated");
        assert_eq!(stages(&app), before);
        assert!(
            screen(&mut app, 80, 8).contains("form is too large"),
            "{}",
            screen(&mut app, 80, 8)
        );
    }

    #[test]
    fn a_button_that_is_not_a_submit_button_submits_nothing_and_resets_nothing() {
        // `type=button` does nothing at all. `type=reset` does not submit —
        // **and does not reset**: restoring the dirty values M11.8 stores is
        // its own small task, no ladder page has a reset button, and a button
        // that quietly does nothing is better than one that clears a form
        // nobody asked it to.
        let (mut app, id) = page_from(
            "http://x/page",
            "<form action=/search><input name=q value=markup>\
             <input type=button value=plain><input type=reset value=clear></form>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        let field = input_node(&app);
        app.dom.as_mut().unwrap().set_field_value(field, "typed");
        let before = stages(&app);

        for id in ["plain", "clear"] {
            let node = {
                let dom = app.dom.as_ref().unwrap();
                (0..dom.node_count() as u32)
                    .map(NodeId)
                    .find(|&n| dom.attr(n, "value") == Some(id))
                    .expect("the button")
            };
            app.focus = Some(node);
            let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            assert!(effect.fetch.is_none(), "{id} submitted");
            assert_eq!(
                app.dom.as_ref().unwrap().field_value(field),
                Some("typed"),
                "{id} changed a value"
            );
        }
        assert_eq!(
            stages(&app),
            before,
            "a button that does nothing ran a stage"
        );
    }

    #[test]
    fn a_click_while_typing_submits_nothing_and_navigates_nowhere() {
        // Where M11.9's rule meets M11.10's for the first time. Leaving a mode
        // is `Esc`, one key, always — so a click is swallowed in `Mode::Field`
        // before any of the submit path is reached, even when it lands on the
        // submit button that would otherwise send the form.
        let (mut app, id) = page_from(
            "http://x/page",
            "<form action=/search><input name=q value=v><button>Go</button></form>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        let button = {
            let dom = app.dom.as_ref().unwrap();
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&n| {
                    matches!(&dom.node(n).data,
                    crate::dom::NodeData::Element { tag, .. } if tag == "button")
                })
                .expect("the button")
        };
        start_typing_at_first_field(&mut app);
        let before = stages(&app);

        let effect = click_node(&mut app, button);
        assert_eq!(
            effect,
            Effect::default(),
            "a click while typing did something"
        );
        assert_eq!(stages(&app), before);

        // `Esc` first, and then the same click sends the form.
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let effect = click_node(&mut app, button);
        assert_eq!(fetched(&effect), Some("http://x/search?q=v"));
    }

    #[test]
    fn a_click_on_a_submit_button_in_no_form_still_focuses_it() {
        // The submission goes nowhere, but the click did something the reader
        // has to see: M11.8's focus frame. A path that returned "nothing
        // happened" here would leave the frame a keypress behind.
        let (mut app, id) = page_from("http://x/page", "<p><button>Go</button></p>", 40, 10);
        app.update(Msg::RunScripts { id });
        let button = {
            let dom = app.dom.as_ref().unwrap();
            (0..dom.node_count() as u32)
                .map(NodeId)
                .find(|&n| {
                    matches!(&dom.node(n).data,
                    crate::dom::NodeData::Element { tag, .. } if tag == "button")
                })
                .expect("the button")
        };
        let effect = click_node(&mut app, button);
        assert_eq!(effect, redraw(), "the focus moved without a redraw");
        assert_eq!(app.focus, Some(button));
    }

    #[test]
    fn enter_in_a_field_with_no_form_does_nothing_at_all() {
        // A search box outside a form is real — sites submit those with
        // JavaScript — and M11.13 is where they start working. Until then:
        // no navigation, no message, no stage.
        let (mut app, id) = page_from("http://x/page", "<p><input name=q value=v></p>", 40, 10);
        app.update(Msg::RunScripts { id });
        let before = stages(&app);
        start_typing_at_first_field(&mut app);
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::default(),
            "a control in no form submitted something"
        );
        assert_eq!(stages(&app), before);
        assert!(
            caret_of(&app).is_some(),
            "the reader was thrown out of the field"
        );
    }

    #[test]
    fn a_submission_is_one_fetch_with_the_page_s_cookies() {
        // Deliverable 8, on the `Effect`: exactly one fetch, the same shape a
        // link click produces, carrying the `Cookie:` header the jar would
        // have built for that URL — because a submission goes through
        // `App::navigate` and `App::request` like everything else, rather than
        // down a second navigation path.
        let mut app = App::new(60, 8);
        let html = "<form action=/search><input name=q value=cat></form><a href=/other>x</a>";
        let id = load_with_cookies(
            &mut app,
            "http://site.test/page",
            html,
            &["sid=abc; Path=/"],
        );
        app.update(Msg::RunScripts { id });
        let before = stages(&app);

        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let (_, request) = effect.fetch.as_ref().expect("no fetch");
        assert_eq!(request.url, "http://site.test/search?q=cat");
        assert_eq!(request.cookie.as_deref(), Some("sid=abc"));
        // One fetch and nothing else: no sheets, no images, no scripts.
        assert!(effect.sheets.is_empty() && effect.images.is_empty() && effect.scripts.is_empty());
        // And no pipeline stage ran here: the pipeline runs when the response
        // lands, exactly as it does for a link.
        assert_eq!(stages(&app), before);
        // The `PageId` guard is the current generation's, so a response from
        // the page we just left cannot land on the search results.
        assert_eq!(
            effect.fetch.as_ref().map(|(fetch, _)| *fetch),
            app.current_fetch
        );
    }

    #[test]
    fn a_submit_listener_can_cancel_field_submission() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form id=f action=/search><input name=q value=v><button id=b>Go</button></form>\
             <script>\
             document.getElementById('f').addEventListener('submit', function (e) {\
               e.preventDefault(); console.log('page saw submit');\
             });\
             document.querySelector('input').addEventListener('change', function () { console.log('page saw change') });\
             document.getElementById('b').addEventListener('click', function () {\
               console.log('page saw click');\
             });</script>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        let logged = app.console.entries().len();

        start_typing_at_first_field(&mut app);
        type_str(&mut app, "x");
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&effect), None, "the canceled form navigated");
        let entries = app.console.entries();
        let seen = entries[logged..]
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(seen, ["page saw change", "page saw submit"]);
    }

    #[test]
    fn a_cancelled_mouse_submit_click_never_dispatches_submit_or_navigates() {
        let (mut app, id) = scripted_app(
            "<form action=/sent><button id=b>Send</button></form><script>\
             var b = document.getElementById('b');\
             b.addEventListener('click', function (e) { console.log('click'); e.preventDefault() });\
             b.parentNode.addEventListener('submit', function () { console.log('submit') });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        let button = by_id(&app, "b");
        let effect = click_node(&mut app, button);
        assert_eq!(fetched(&effect), None);
        let entries = app.console.entries();
        let seen = entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(seen, ["click"]);
    }

    #[test]
    fn submit_listener_mutations_are_serialized_after_the_event() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form id=f action=/old><input id=q name=q value=old></form><script>\
             document.getElementById('f').addEventListener('submit', function () {\
               document.getElementById('q').value = 'new';\
               this.setAttribute('action', '/new');\
             });</script>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&effect), Some("http://x/new?q=new"));
    }

    #[test]
    fn cancelled_submit_keeps_a_listener_requested_navigation() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form action=/native><input name=q value=x></form><script>\
             document.querySelector('form').addEventListener('submit', function (e) {\
               e.preventDefault(); location.href = '/script';\
             });</script>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&effect), Some("http://x/script"));
    }

    #[test]
    fn uncancelled_native_submit_wins_over_listener_navigation() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form action=/native><input name=q value=x></form><script>\
             document.querySelector('form').addEventListener('submit', function () {\
               location.href = '/script';\
             });</script>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&effect), Some("http://x/native?q=x"));
    }

    #[test]
    fn submit_button_click_revalidates_type_disabled_and_connection() {
        for mutation in [
            "b.setAttribute('type', 'button')",
            "b.setAttribute('disabled', '')",
            "b.remove()",
        ] {
            let html = format!(
                "<form action=/native><button id=b>Send</button></form><script>\
                 var b = document.getElementById('b');\
                 b.addEventListener('click', function () {{ {mutation}; }});\
                 </script>"
            );
            let (mut app, id) = page_from("http://x/page", &html, 60, 8);
            app.update(Msg::RunScripts { id });
            let button = by_id(&app, "b");
            let effect = click_node(&mut app, button);
            assert_eq!(
                fetched(&effect),
                None,
                "mutation `{mutation}` left the old submit default active"
            );
        }
    }

    #[test]
    fn submit_button_click_reparents_before_resolving_its_form() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form id=old action=/old><button id=b>Send</button></form>\
             <form id=new action=/new></form><script>\
             var b = document.getElementById('b');\
             b.addEventListener('click', function () { document.getElementById('new').appendChild(b) });\
             </script>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        let button = by_id(&app, "b");
        let effect = click_node(&mut app, button);
        assert_eq!(fetched(&effect), Some("http://x/new?"));
    }

    #[test]
    fn cancelled_submit_keeps_listener_fetches_and_timers() {
        let (mut app, id) = page_from(
            "http://x/page",
            "<form action=/native><input name=q value=x></form><script>\
             document.querySelector('form').addEventListener('submit', function (e) {\
               e.preventDefault(); fetch('/listener'); setTimeout(function () {}, 25);\
             });</script>",
            60,
            8,
        );
        app.update(Msg::RunScripts { id });
        start_typing_at_first_field(&mut app);
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(fetched(&effect), None);
        assert_eq!(effect.fetches.len(), 1);
        assert_eq!(effect.fetches[0].1.ask.url, "http://x/listener");
        assert_eq!(
            effect.timers,
            [TimerRequest::Schedule {
                page: id,
                id: TimerId(1),
                delay: Duration::from_millis(25),
            }]
        );
    }

    #[test]
    fn a_control_click_listener_sends_its_timer_request_with_the_action() {
        let (mut app, id) = scripted_app(
            "<input id=c type=checkbox><script>\
             document.getElementById('c').addEventListener('click', function () { setTimeout(function () {}, 25) });\
             </script>",
        );
        app.update(Msg::RunScripts { id });
        let checkbox = by_id(&app, "c");
        assert!(!app.dispatch_click(checkbox));
        let effect = app.take_click_navigation();
        assert_eq!(
            effect.timers,
            [TimerRequest::Schedule {
                page: id,
                id: TimerId(1),
                delay: Duration::from_millis(25),
            }]
        );
    }

    // ---- invalidation (M10.6) ---------------------------------------------

    /// The three stage counters, as one comparable value. Every M10.6 test
    /// asserts on these rather than on appearance: a stage that ran when it
    /// should not have is invisible on screen and ruinous in a profile.
    fn stages(app: &App) -> (usize, usize, usize) {
        (app.styles_run, app.layouts, app.paints)
    }

    /// A page that has been parsed and had its script pass run, with the
    /// counters read after everything has settled.
    fn settled(html: &str) -> (App, PageId, (usize, usize, usize)) {
        let (mut app, id) = scripted_app(html);
        app.update(Msg::RunScripts { id });
        let counts = stages(&app);
        (app, id, counts)
    }

    #[test]
    fn a_tick_that_mutates_nothing_runs_no_stage_at_all() {
        // Deliverable 4: a handler that only reads must cost nothing. This is
        // the invariant every later M10 task leans on — M10.8's listeners and
        // M10.9's timers fire far more often than a page loads.
        let (mut app, id) = scripted_app(
            "<p class=x>text</p><script>\
             document.querySelectorAll('p').length + document.body.textContent.length;</script>",
        );
        let before = stages(&app);
        assert_eq!(app.update(Msg::RunScripts { id }), Effect::default());
        assert_eq!(
            stages(&app),
            before,
            "a read-only tick ran a stage (styles, layouts, paints)"
        );
    }

    #[test]
    fn building_a_table_uses_the_normal_single_pipeline_pass() {
        let (_app, _id, counts) = settled(
            "<table><tr><td>rank</td><td>title</td></tr><tr><td>1.</td><td>story</td></tr></table>",
        );
        assert_eq!(
            counts,
            (1, 1, 1),
            "table structure must not trigger a table-specific style, layout, or paint pass"
        );
    }

    #[test]
    fn table_resize_relayouts_once_and_scrolling_uses_the_cached_tree() {
        let mut html = String::from("<table>");
        for index in 0..80 {
            html.push_str(&format!(
                "<tr><td>{index}.</td><td>▲</td><td>story title {index} with metadata</td></tr>"
            ));
        }
        html.push_str("</table>");
        let mut app = page(40, 10, &html);
        assert_eq!(app.layouts, 1);
        for _ in 0..80 {
            app.update(ch('j'));
        }
        assert!(app.viewport.offset() > 0);
        assert_eq!(app.layouts, 1, "scroll must not remeasure table columns");
        app.update(Msg::Resize(30, 10));
        assert_eq!(app.layouts, 2, "resize performs one ordinary relayout");
    }

    #[test]
    fn a_script_live_value_write_relayouts_without_restyling() {
        let (mut app, id) = scripted_app(
            "<input id=x value=markup><script>document.getElementById('x').value = 'live';</script>",
        );
        let (styles, layouts, paints) = stages(&app);
        assert_eq!(
            stages(&app),
            (styles, layouts, paints),
            "building the queued script should not itself run it"
        );
        let effect = app.update(Msg::RunScripts { id });
        assert!(effect.dirty);
        assert_eq!(stages(&app), (styles, layouts + 1, paints + 1));
        let field = by_id(&app, "x");
        let dom = app.dom.as_ref().unwrap();
        assert_eq!(dom.field_value(field), Some("live"));
        assert_eq!(dom.attr(field, "value"), Some("markup"));
    }

    #[test]
    fn a_position_style_mutation_restyles_and_relayouts_once() {
        let (mut app, id) = scripted_app(
            "<style>.moved { position: relative; left: 1em }</style><p id=x>text</p>\
             <script>document.getElementById('x').setAttribute('class', 'moved')</script>",
        );
        let before = stages(&app);
        let effect = app.update(Msg::RunScripts { id });
        assert!(effect.dirty);
        assert_eq!(
            stages(&app),
            (before.0 + 1, before.1 + 1, before.2 + 1),
            "position is geometry, not paint-only state"
        );
    }

    #[test]
    fn a_grid_style_mutation_restyles_and_relayouts_once() {
        let (mut app, id) = scripted_app(
            "<style>.grid { display:grid; grid-template-columns:1fr 1fr; gap:1em } \
             .wide { display:grid; grid-template-columns:1fr 2fr; gap:1em }</style>\
             <div id=x class=grid><p>one</p><p>two</p></div>\
             <script>document.getElementById('x').setAttribute('class', 'wide')</script>",
        );
        let before = stages(&app);
        let effect = app.update(Msg::RunScripts { id });
        assert!(effect.dirty);
        assert_eq!(stages(&app), (before.0 + 1, before.1 + 1, before.2 + 1));
    }

    #[test]
    fn a_grid_item_placement_mutation_relayouts_once_at_the_new_cell() {
        let (mut app, id) = scripted_app(
            "<style>body,p{margin:0}.g{display:grid;grid-template-columns:1fr 1fr}.moved{grid-column:2}</style>\
             <div class=g><p id=x>one</p><p>two</p></div>\
             <script>document.getElementById('x').setAttribute('class','moved')</script>",
        );
        let before = stages(&app);
        app.update(Msg::RunScripts { id });
        assert_eq!(stages(&app), (before.0 + 1, before.1 + 1, before.2 + 1));
        let (dom, tree) = (app.dom.as_ref().unwrap(), app.layout_tree.as_ref().unwrap());
        let moved_x = tree
            .boxes
            .iter()
            .find(|b| b.node.is_some_and(|node| dom.attr(node, "id") == Some("x")))
            .unwrap()
            .dimensions
            .margin_box()
            .x;
        assert_eq!(moved_x, 19);
    }

    #[test]
    fn grid_scrolling_reuses_the_cached_layout() {
        let items: String = (0..80).map(|n| format!("<p>row {n}</p>")).collect();
        let mut app = page(
            40,
            8,
            &format!(
                "<style>.g{{display:grid;grid-template-columns:1fr 1fr}}p{{margin:0}}</style><div class=g>{items}</div>"
            ),
        );
        let before = stages(&app);
        for _ in 0..40 {
            app.update(ch('j'));
        }
        assert!(app.viewport.offset() > 0);
        assert_eq!(
            stages(&app),
            before,
            "scrolling must not re-run grid placement"
        );
    }

    #[test]
    fn a_link_in_the_second_grid_track_is_hit_at_its_final_rectangle() {
        let mut app = page(
            60,
            10,
            "<style>body{margin:0}.g{display:grid;grid-template-columns:1fr 1fr}a{display:block}</style><div class=g><a href=/one>one</a><a href=/two>two</a></div>",
        );
        let dom = app.dom.as_ref().unwrap();
        let tree = app.layout_tree.as_ref().unwrap();
        let link = layout::collect_links(tree, dom)
            .into_iter()
            .find(|link| link.href == "/two")
            .expect("grid link missing");
        // The 60-column terminal reserves one gutter cell, leaving a 59-cell
        // page column; the second fractional track begins at its 29th cell.
        assert_eq!((link.x, link.y), (29, 0));
        let left = column(app.size.0).left;
        let effect = app.update(mouse_down((left as i32 + link.x) as u16, link.y as u16));
        assert_eq!(
            effect
                .fetch
                .as_ref()
                .map(|(_, request)| request.url.as_str()),
            Some("http://final/two")
        );
    }

    #[test]
    fn grid_links_and_fields_share_focus_and_hint_geometry() {
        let mut app = page(
            60,
            10,
            "<style>body,p{margin:0}.g{display:grid;grid-template-columns:96px 1fr}</style><div class=g><a href=/side>side</a><a href=/main>main</a><input value=query></div>",
        );
        let before = stages(&app);
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(focused(&app).as_deref(), Some("a /side"));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(focused(&app).as_deref(), Some("a /main"));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(focused(&app).as_deref(), Some("input query"));
        assert_eq!(stages(&app), before, "grid focus reran the pipeline");

        app.update(ch('f'));
        let session = app.hint.as_ref().expect("grid hints must open");
        let hrefs: Vec<_> = session
            .labels
            .iter()
            .map(|(_, link)| link.href.as_str())
            .collect();
        assert!(hrefs.contains(&"/side"), "{hrefs:?}");
        assert!(hrefs.contains(&"/main"), "{hrefs:?}");
        let main = session
            .labels
            .iter()
            .find(|(_, link)| link.href == "/main")
            .unwrap();
        assert!(main.1.x >= 12, "second-track hint at x={}", main.1.x);
    }

    #[test]
    fn search_highlights_text_at_its_final_grid_rectangle() {
        let mut app = page(
            60,
            10,
            "<style>body,p{margin:0}.g{display:grid;grid-template-columns:96px 1fr}</style><div class=g><p>rail</p><p>needle in the article</p></div>",
        );
        let before = stages(&app);
        app.update(ch('/'));
        for c in "needle".chars() {
            app.update(ch(c));
        }
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let found = &app.search.as_ref().expect("grid search missing").matches;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].x >= 12, "grid match at x={}", found[0].x);
        let mut frame = Frame::new(60, 10);
        app.draw(&mut frame);
        assert!(
            (12..59).any(|x| frame.get(x, 0).attrs.contains(Attrs::REVERSE)),
            "search overlay missed the second grid track"
        );
        assert_eq!(stages(&app), before, "grid search reran the pipeline");
    }

    #[test]
    fn decoded_image_pixels_keep_their_grid_track_rectangle() {
        let mut app = App::new(60, 10);
        let (id, _) = open_page(
            &mut app,
            r#"<style>body{margin:0}.g{display:grid;grid-template-columns:1fr 1fr}</style><div class=g><p>left</p><img src=pic.png width=16 height=32 alt=pic></div>"#,
        );
        let decoded = crate::image::DecodedImage::new(2, 2, vec![255; 2 * 2 * 4]);
        app.update(Msg::Image {
            id,
            url: "http://site.test/dir/pic.png".into(),
            result: Ok(decoded),
        });
        let rect = app
            .display_list
            .commands
            .iter()
            .find_map(|command| match command {
                crate::paint::DisplayCommand::Image {
                    rect,
                    pixels: Some(_),
                    ..
                } => Some(*rect),
                _ => None,
            })
            .expect("decoded grid image was not painted");
        assert!(rect.x >= 29, "image did not stay in track two: {rect:?}");
        assert_eq!((rect.width, rect.height), (2, 2));
    }

    #[test]
    fn sticky_grid_heading_uses_cached_static_space_at_several_scroll_offsets() {
        let rows: String = (0..40)
            .map(|n| format!("<p class=row>row {n}</p>"))
            .collect();
        let mut app = page(
            40,
            8,
            &format!(
                "<style>body,p{{margin:0}}.g{{display:grid;grid-template-columns:1fr 1fr}}.head{{position:sticky;top:0;grid-column:2;grid-row:1}}.row{{grid-column:1}}</style><div class=g><p class=head>HEAD</p>{rows}</div>"
            ),
        );
        let laid_out = stages(&app);
        for target in [0, 2, 5] {
            while app.viewport.offset() < target {
                app.update(ch('j'));
            }
            let mut frame = Frame::new(40, 8);
            app.draw(&mut frame);
            assert!(row_text(&frame, 0).contains("HEAD"), "offset {target}");
            assert_eq!(stages(&app), laid_out, "sticky grid scroll relaid out");
        }
    }

    #[test]
    fn grid_resize_relayouts_once_and_recomputes_percentage_and_fr_tracks() {
        fn tracks(app: &App) -> Vec<i32> {
            app.layout_tree
                .as_ref()
                .unwrap()
                .boxes
                .iter()
                .find_map(|b| b.grid.as_ref().map(|g| g.columns.clone()))
                .expect("grid root missing")
        }

        let mut app = page(
            60,
            10,
            "<style>body,p{margin:0}.g{display:grid;grid-template-columns:25% 1fr}</style><div class=g><p>a</p><p>b</p></div>",
        );
        let before_tracks = tracks(&app);
        let before = stages(&app);
        app.update(Msg::Resize(40, 10));
        let after_tracks = tracks(&app);
        assert_eq!(stages(&app), (before.0, before.1 + 1, before.2 + 1));
        assert_ne!(after_tracks, before_tracks);
        assert_eq!(before_tracks.iter().sum::<i32>(), 58);
        assert_eq!(after_tracks.iter().sum::<i32>(), 38);
        assert_eq!(after_tracks, vec![10, 28]);
    }

    #[test]
    fn a_tick_that_only_touches_cookies_runs_no_stage_either() {
        // M11.6 deliverable 9, under M10.6's discipline: `document.cookie` is
        // read by the first inline script of a page, so it is on the load path
        // — and it is a map lookup and a string build, not a stage. A cookie
        // write changes no box either, so neither half of the property may
        // restyle, relayout or repaint.
        let (mut app, id) = scripted_app(
            "<p>text</p><script>\
             document.cookie = 'a=1';\
             document.cookie.length + document.cookie.indexOf('a=1');</script>",
        );
        let before = stages(&app);
        assert_eq!(app.update(Msg::RunScripts { id }), Effect::default());
        assert_eq!(
            stages(&app),
            before,
            "reading or writing a cookie ran a pipeline stage"
        );
    }

    // ---- M11.7: cookies on the wire ---------------------------------------

    /// Load `html` from `url` with the response's `Set-Cookie` lines, and
    /// parse it — the shape a real navigation has, with the cookies where a
    /// real response puts them.
    fn load_with_cookies(app: &mut App, url: &str, html: &str, set_cookie: &[&str]) -> PageId {
        let id = app.start_fetch(url.to_string());
        app.update(Msg::Loaded {
            id,
            url: url.to_string(),
            status: 200,
            body: html.as_bytes().to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: set_cookie.iter().map(|s| s.to_string()).collect(),
            metadata: Default::default(),
        });
        app.update(parsed(id, html));
        id
    }

    #[test]
    fn a_server_set_cookie_reaches_the_jar_and_the_page_sees_its_half() {
        // The round trip in miniature: the response sets two cookies, the
        // page's own script reads back the one it is allowed to, and the wire
        // gets both.
        let mut app = App::new(40, 10);
        let id = load_with_cookies(
            &mut app,
            "http://site.test/app/page",
            "<p>x</p><script>console.log(document.cookie);</script>",
            &["sid=abc; Path=/; HttpOnly", "theme=dark; Path=/"],
        );
        app.update(Msg::RunScripts { id });
        assert!(
            app.console.entries().iter().any(|e| e.text == "theme=dark"),
            "the script saw {:?}",
            app.console.entries()
        );
        assert_eq!(
            js::cookies::header_for(
                &app.cookies,
                "http://site.test/app/page",
                "http://site.test/app/x.js",
                js::cookies::now()
            )
            .as_deref(),
            Some("sid=abc; theme=dark"),
            "the wire did not get the HttpOnly cookie"
        );
    }

    #[test]
    fn a_pages_subresources_carry_its_cookies_and_a_cross_origin_one_does_not() {
        // All four of `App`'s request paths in one page, because the rule is
        // uniform and the way to check a uniform rule is to count call sites:
        // a stylesheet, an image and a script from the page's own origin carry
        // the session; the one from another origin carries nothing.
        let mut app = App::new(40, 10);
        let effect_id = app.start_fetch("http://site.test/app/page".into());
        app.update(Msg::Loaded {
            id: effect_id,
            url: "http://site.test/app/page".into(),
            status: 200,
            body: Vec::new(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: vec!["sid=abc; Path=/".into()],
            metadata: Default::default(),
        });
        let html = "<link rel=stylesheet href=/site.css>\
                    <link rel=stylesheet href=http://cdn.test/other.css>\
                    <img src=/pic.png><img src=http://cdn.test/pic.png>\
                    <script src=/app.js></script>\
                    <script src=https://www.google-analytics.com/analytics.js></script>";
        let effect = app.update(parsed(effect_id, html));

        let cookie_for = |requests: &[(PageId, net::Request)], url: &str| {
            requests
                .iter()
                .find(|(_, request)| request.url == url)
                .unwrap_or_else(|| panic!("no request for {url}"))
                .1
                .cookie
                .clone()
        };
        let sheets: Vec<(PageId, net::Request)> = effect
            .sheets
            .iter()
            .map(|(id, _, request)| (*id, request.clone()))
            .collect();
        assert_eq!(
            cookie_for(&sheets, "http://site.test/site.css").as_deref(),
            Some("sid=abc")
        );
        assert_eq!(cookie_for(&sheets, "http://cdn.test/other.css"), None);
        assert_eq!(
            cookie_for(&effect.images, "http://site.test/pic.png").as_deref(),
            Some("sid=abc")
        );
        assert_eq!(cookie_for(&effect.images, "http://cdn.test/pic.png"), None);

        // Scripts are requested by the pass, not by the parse.
        let effect = app.update(Msg::RunScripts { id: effect_id });
        let scripts: Vec<(PageId, net::Request)> = effect
            .scripts
            .iter()
            .map(|(id, _, request)| (*id, request.clone()))
            .collect();
        assert_eq!(
            cookie_for(&scripts, "http://site.test/app.js").as_deref(),
            Some("sid=abc")
        );
        assert_eq!(
            cookie_for(&scripts, "https://www.google-analytics.com/analytics.js"),
            None,
            "the ladder's tracker was told who the reader is"
        );
    }

    #[test]
    fn a_navigation_carries_the_cookies_of_where_it_is_going() {
        // The page the same-origin rule is measured against is the *target*
        // for a top-level navigation, not the page being left. Getting this
        // backwards would mean a reader could never arrive logged in at a site
        // they reached from somewhere else — which is most of the web.
        let mut app = App::new(40, 10);
        load_with_cookies(
            &mut app,
            "http://b.test/",
            "<p>b</p>",
            &["sid=for-b; Path=/"],
        );
        load_with_cookies(&mut app, "http://a.test/", "<p>a</p>", &[]);
        // On a.test, navigating to b.test: b.test's cookies go out.
        let effect = app.navigate("http://b.test/again".into(), true);
        let (_, request) = effect.fetch.expect("a navigation must fetch");
        assert_eq!(request.url, "http://b.test/again");
        assert_eq!(
            request.cookie.as_deref(),
            Some("sid=for-b"),
            "the target's own cookies were withheld from a top-level navigation"
        );
    }

    #[test]
    fn applying_a_response_cookie_runs_no_pipeline_stage() {
        // Deliverable 7's counter half. The jar is on the load path of every
        // request now; a map insert is not a stage, and the proof is that the
        // same page with and without cookies costs the same three counters.
        let mut plain = App::new(40, 10);
        load_with_cookies(&mut plain, "http://site.test/", "<p>x</p>", &[]);
        let mut with_cookies = App::new(40, 10);
        load_with_cookies(
            &mut with_cookies,
            "http://site.test/",
            "<p>x</p>",
            &["a=1; Path=/", "b=2; HttpOnly", "rubbish"],
        );
        assert_eq!(
            stages(&with_cookies),
            stages(&plain),
            "a Set-Cookie ran a pipeline stage"
        );
        // The malformed one is still reported — a dropped cookie is a console
        // line, never a failed navigation.
        assert!(
            with_cookies
                .console
                .entries()
                .iter()
                .any(|e| e.text.contains("ignored Set-Cookie")),
            "{:?}",
            with_cookies.console.entries()
        );
        assert!(matches!(with_cookies.fetch, Fetch::Loaded { .. }));
    }

    #[test]
    fn fetch_honours_credentials_and_omit_is_the_only_one_that_means_anything() {
        // `credentials` off M10.12's ignored-and-logged list. `include` and
        // `same-origin` are the same answer because the same-origin check
        // upstream has already made a same-origin request the only kind this
        // engine makes at all; `omit` must actually omit.
        let mut app = App::new(40, 10);
        let id = load_with_cookies(
            &mut app,
            "http://site.test/app/page",
            "<script>\
             fetch('/a', {credentials: 'omit'});\
             fetch('/b', {credentials: 'include'});\
             fetch('/c', {credentials: 'same-origin'});\
             fetch('/d');</script>",
            &["sid=abc; Path=/"],
        );
        let effect = app.update(Msg::RunScripts { id });
        let asked: Vec<(&str, Option<&str>)> = effect
            .fetches
            .iter()
            .map(|(_, ask)| (ask.ask.url.as_str(), ask.ask.cookie.as_deref()))
            .collect();
        assert_eq!(
            asked,
            [
                ("http://site.test/a", None),
                ("http://site.test/b", Some("sid=abc")),
                ("http://site.test/c", Some("sid=abc")),
                ("http://site.test/d", Some("sid=abc")),
            ]
        );
        // And it is no longer warned about as ignored, because it is not.
        assert!(
            !app.console
                .entries()
                .iter()
                .any(|e| e.text.contains("`credentials` option is ignored")),
            "{:?}",
            app.console.entries()
        );
    }

    #[test]
    fn a_cookie_survives_the_navigation_that_drops_the_host() {
        // The jar lives in `App`, not in the engine: the second page runs on a
        // host that has never seen the first page's globals and still reads its
        // cookie. Both pages are `http://final/` — one host, one jar.
        let (mut app, first) = scripted_app(
            "<p>one</p><script>globalThis.marker = 'page one';\
             document.cookie = 'who=first; path=/';</script>",
        );
        app.update(Msg::RunScripts { id: first });

        let second = app.start_fetch("http://x/two".into());
        // `typeof marker` is the control: it says the second page really is
        // running on a new engine, so the cookie did not simply survive inside
        // a host nobody dropped.
        let page = "<p>two</p><script>\
             console.log('globals: ' + typeof marker);\
             console.log('read back: ' + document.cookie);</script>";
        load(&mut app, second, page.as_bytes().to_vec());
        app.update(parsed(second, page));
        app.update(Msg::RunScripts { id: second });

        assert_eq!(
            app.console
                .entries()
                .iter()
                .map(|entry| entry.text.clone())
                .collect::<Vec<_>>(),
            ["globals: undefined", "read back: who=first"],
            "the jar was dropped with the page's host"
        );
    }

    #[test]
    fn a_timer_writes_into_the_jar_the_page_pass_wrote_to() {
        // A cookie is not tick-local: what the document-order pass wrote, a
        // callback three turns later reads, and what the callback writes joins
        // it. This is the whole point of the jar hanging off `App`.
        //
        // What it does *not* pin, despite appearances: which jar a later tick's
        // `PageContext` names. The bindings capture the jar when the host is
        // built, so `ctx.cookies` is read at host creation and never again —
        // `a_cookie_survives_the_navigation_that_drops_the_host` is the test
        // that catches a wrong jar there, and it is the only one that can.
        let (mut app, id) = scripted_app(
            "<p>x</p><script>\
             document.cookie = 'from=pass; path=/';\
             setTimeout(function () {\
               document.cookie = 'and=timer; path=/';\
               console.log('the timer sees: ' + document.cookie);\
             }, 0);</script>",
        );
        run_turns(&mut app, id);
        let effect = app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        drain_turns(&mut app, effect);
        assert_eq!(logged(&app), ["the timer sees: from=pass; and=timer"]);
    }

    #[test]
    fn a_superseded_generations_tick_runs_no_stage() {
        // Deliverable 6's ordering rule: mutations belong to the generation
        // that made them, and a generation change cancels the pending cycle.
        // The guard is the same `PageId` check every other message uses, and
        // it must fire *before* any stage runs — a relayout of a page the user
        // has already left is wasted work at best and the wrong page at worst.
        let (mut app, first) = scripted_app(
            "<div id=list></div><script>\
             document.getElementById('list').textContent = 'built';</script>",
        );
        let before = stages(&app);

        // The user navigates before the pass's turn comes up.
        let second = app.start_fetch("http://elsewhere/".into());
        assert_ne!(first, second);

        assert_eq!(app.update(Msg::RunScripts { id: first }), Effect::default());
        assert_eq!(
            stages(&app),
            before,
            "a superseded generation's tick ran a pipeline stage"
        );
    }

    #[test]
    fn a_tree_edit_restyles_relayouts_and_repaints_once() {
        // Deliverable 2, first branch: boxes were added, so there is nothing
        // to compare and everything to redo — but only once.
        let (mut app, id) = scripted_app(
            "<div id=list></div><script>\
             var l = document.getElementById('list');\
             for (var i = 0; i < 50; i++) l.appendChild(document.createElement('p'));</script>",
        );
        let (styled, laid_out, painted) = stages(&app);
        assert_eq!(app.update(Msg::RunScripts { id }), redraw());
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out + 1, painted + 1),
            "a tree edit must cost exactly one of each stage"
        );
    }

    #[test]
    fn an_attribute_write_that_only_changes_paint_skips_layout() {
        // Deliverable 2, the one narrowing: `.tint { color: … }` moves no box,
        // so the page takes `:hover`'s path — restyle, recolour the existing
        // tree, repaint. Getting this wrong is not visible on screen, which is
        // exactly why it is asserted on counters.
        let (mut app, id) = scripted_app(
            "<style>.tint { color: #c00 }</style><p id=box>text</p><script>\
             document.getElementById('box').classList.add('tint');</script>",
        );
        let (styled, laid_out, painted) = stages(&app);
        assert_eq!(app.update(Msg::RunScripts { id }), redraw());
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out, painted + 1),
            "a paint-only class change relayouted"
        );
    }

    #[test]
    fn an_attribute_write_that_moves_a_box_does_relayout() {
        // The other half of the narrowing, and the one that must never be
        // narrowed away: the same shape of write, but the rule it matches
        // changes a property layout reads.
        // Two paragraphs, so hiding one does not blank the page — a page that
        // hides everything is *revealed* again by layout's never-blank rule
        // (M7), which would mask what this test is looking at.
        let (mut app, id) = scripted_app(
            "<style>.gone { display: none }</style>\
             <p id=box>vanishing</p><p>surviving</p><script>\
             document.getElementById('box').classList.add('gone');</script>",
        );
        let (styled, laid_out, painted) = stages(&app);
        assert_eq!(app.update(Msg::RunScripts { id }), redraw());
        assert_eq!(
            stages(&app),
            (styled + 1, laid_out + 1, painted + 1),
            "a class change that hides an element did not relayout"
        );

        // And it really is hidden — the narrowing decides *whether* to lay
        // out, never what the answer is.
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        let text: String = (0..10).map(|y| row_text(&frame, y)).collect();
        assert!(text.contains("surviving"), "{text:?}");
        assert!(!text.contains("vanishing"), "still on screen: {text:?}");
    }

    #[test]
    fn an_attribute_write_no_rule_matches_still_costs_only_a_restyle() {
        // A `data-` attribute no stylesheet mentions: the cascade cannot have
        // changed anything, and the comparison proves it rather than assuming.
        let (mut app, id) = scripted_app(
            "<p id=box>text</p><script>\
             document.getElementById('box').setAttribute('data-seen', '1');</script>",
        );
        let (styled, laid_out, painted) = stages(&app);
        assert_eq!(app.update(Msg::RunScripts { id }), redraw());
        assert_eq!(stages(&app), (styled + 1, laid_out, painted + 1));
    }

    #[test]
    fn scrolling_a_script_built_page_runs_no_stage() {
        // Deliverable 4, and CLAUDE.md's oldest rule: scrolling is cached
        // display list → repaint at a new offset. A page whose content came
        // from a script is no different.
        let (mut app, _id, before) = settled(
            "<div id=list></div><script>\
             var l = document.getElementById('list');\
             for (var i = 0; i < 200; i++) {\
               var p = document.createElement('p');\
               p.textContent = 'row ' + i;\
               l.appendChild(p);\
             }</script>",
        );
        for _ in 0..50 {
            app.update(key(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        assert_eq!(
            stages(&app),
            before,
            "scrolling a script-built page ran a pipeline stage"
        );
    }

    #[test]
    fn hovering_script_built_content_restyles_without_relayout() {
        // Deliverable 4: the `:hover` path is the one M10.6 borrows for
        // paint-only mutations, so it has to keep working on content that JS
        // created rather than the parser.
        let (mut app, _id, before) = settled(
            "<style>a:hover { color: #f00 }</style><div id=list></div><script>\
             var a = document.createElement('a');\
             a.setAttribute('href', 'https://example.com/');\
             a.textContent = 'a script-made link';\
             document.getElementById('list').appendChild(a);</script>",
        );
        let (styled, laid_out, painted) = before;

        // Hover the row the link landed on.
        let moved = app.update(Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: column(40).left + 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));
        if moved.dirty {
            let (s, l, p) = stages(&app);
            assert_eq!(l, laid_out, "hover relayouted script-built content");
            assert_eq!((s, p), (styled + 1, painted + 1));
        }
    }

    #[test]
    fn a_resize_relayouts_once_and_runs_no_script_pass() {
        // Deliverable 4 and M10.2's rule together: a resize is layout work,
        // never an excuse to run the page's script again.
        let (mut app, _id, (styled, laid_out, _painted)) = settled(
            "<div id=list></div><script>\
             document.getElementById('list').textContent = 'built';</script>",
        );

        let effect = app.update(Msg::Resize(30, 8));
        assert_eq!(effect.run_scripts, None, "a resize asked for a script pass");
        assert_eq!(
            app.layouts,
            laid_out + 1,
            "a resize must relayout exactly once"
        );
        assert_eq!(app.styles_run, styled, "a resize restyled");
    }

    // ---- scoped restyle (M11.3) -------------------------------------------

    /// Nodes styled by one tick, and the whole document's size for comparison.
    fn tick_styled(app: &mut App, id: PageId) -> (usize, usize) {
        let before = app.nodes_styled;
        app.update(Msg::RunScripts { id });
        (
            app.nodes_styled - before,
            app.dom.as_ref().unwrap().node_count(),
        )
    }

    #[test]
    fn a_class_toggle_on_a_leaf_styles_a_handful_of_nodes_not_the_document() {
        // The counter M11.3 exists to move. `styles_run` says a restyle
        // happened and cannot say how big it was; on this page the difference
        // between the two answers is 25,599 nodes against three, and 43 ms
        // against nothing worth measuring (perf.md).
        let (mut app, id) = scripted_app(&wikipedia_with_script(
            ".x-tint { color: #c00 }",
            "document.getElementById('x11-3-leaf').className = 'x-tint';",
        ));
        let styled_before = app.styles_run;
        let (styled, document) = tick_styled(&mut app, id);

        assert!(document > 25_000, "the fixture shrank: {document} nodes");
        assert_eq!(
            app.styles_run,
            styled_before + 1,
            "the tick must still restyle exactly once"
        );
        assert_eq!(
            styled, 2,
            "a class toggle on a leaf must style the leaf and its text node, \
             not the document's {document}"
        );
    }

    #[test]
    fn a_structural_edit_still_restyles_the_whole_document() {
        // The narrowing is for attribute writes only, and deliberately so: a
        // new node has no computed values to inherit from and no slot in the
        // dense `Vec`, so there is nothing for a subtree pass to write into.
        let (mut app, id) = scripted_app(
            "<div id=host><p>a</p></div><script>\
             document.getElementById('host').appendChild(document.createElement('p'));</script>",
        );
        let (styled, document) = tick_styled(&mut app, id);
        assert_eq!(styled, document, "a structural edit narrowed its restyle");
    }

    #[test]
    fn a_node_created_beside_an_attribute_write_falls_back_to_a_full_pass() {
        // `createElement` without an insert is an `Edit::Detached`: it bumps
        // the edit count but not the structure count, so the tick still looks
        // attribute-only — while the arena has grown past the styled `Vec` it
        // would be written into. The size check is what catches it.
        let (mut app, id) = scripted_app(
            "<p id=t>text</p><script>\
             document.createElement('span');\
             document.getElementById('t').className = 'x';</script>",
        );
        let (styled, document) = tick_styled(&mut app, id);
        assert_eq!(
            styled,
            document - 1,
            "a tick that grew the arena narrowed its restyle anyway — the one \
             node short of the arena is the detached `span`, which no walk reaches"
        );
    }

    #[test]
    fn more_writes_than_the_arena_tracks_restyle_the_document() {
        // Past the cap the subtrees add up to the document anyway, so the
        // fallback costs nothing and the list stays bounded (M10.13).
        let count = crate::dom::MAX_TRACKED_ATTR_CHANGES + 1;
        let rows: String = (0..count).map(|i| format!("<p id=r{i}>row</p>")).collect();
        let (mut app, id) = scripted_app(&format!(
            "<div>{rows}</div><script>\
             var all = document.querySelectorAll('p');\
             for (var i = 0; i < all.length; i++) all[i].className = 'x';</script>",
        ));
        let (styled, document) = tick_styled(&mut app, id);
        assert_eq!(
            styled, document,
            "{count} attribute writes must fall back to a full pass"
        );
    }

    #[test]
    fn a_scoped_restyle_still_paints_what_it_changed() {
        // The narrowing must not become a page that does not update: the same
        // turn, end to end, with the screen as the witness rather than a
        // counter. `Styles::layout_eq` sees a paint-only change here, so this
        // is the recolour path — and the words still have to move.
        let (mut app, id) = scripted_app(
            "<style>.wide { margin-left: 3em }</style>\
             <p id=t>shifted</p><script>\
             document.getElementById('t').className = 'wide';</script>",
        );
        let indent = |app: &mut App| {
            screen(app, 40, 8)
                .lines()
                .find(|l| l.contains("shifted"))
                .map(|l| l.find("shifted").unwrap())
                .expect("the page never rendered its paragraph")
        };
        let before = indent(&mut app);
        app.update(Msg::RunScripts { id });
        assert_eq!(
            indent(&mut app),
            before + 6,
            "the 3em (= 6 cells) the scoped pass computed never reached the screen"
        );
    }

    /// A Wikipedia-sized page with its own scripts neutralised, plus `script`.
    ///
    /// The fixture's inline scripts are retyped so the source walk skips them
    /// (M10.2 decides what runs by `type`): they are not part of the path a
    /// keypress takes, and leaving them in would measure page load instead.
    /// The elements stay, so the tree, the cascade and the layout are the
    /// article's real size.
    fn wikipedia_with_script(css: &str, script: &str) -> String {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        // Both appended at the end, never prepended: a `<style>` before the
        // fixture's own doctype opens the head early and parses a *different*
        // document, which would make the two paths below incomparable.
        //
        // The paragraph is M11.3's target: an ordinary leaf element deep in a
        // real page, which is the shape a click's `classList.add` has. Two
        // nodes out of 25,599, so the page it is measuring is still the page.
        format!(
            "{}<p id=x11-3-leaf>leaf</p><style>{css}</style><script>{script}</script>",
            fixture.replace("<script", "<script type=\"text/x-not-run\"")
        )
    }

    /// One full turn of the path a click will take once M10.8 dispatches one:
    /// the tick, the invalidation it triggers, the draw, and the present.
    /// Returns the wall clock for all of it.
    fn timed_js_turn(app: &mut App, id: PageId) -> Duration {
        // Built outside the measurement: the event loop keeps one renderer for
        // the life of the process, so its construction is not on the path.
        let mut renderer = crate::term::Renderer::new(80, 24, crate::term::detect_caps_from_env());
        let started = Instant::now();
        app.update(Msg::RunScripts { id });
        // The draw and the renderer's diff + one batched write, into a sink:
        // the same pair the event loop times as `frame`.
        app.draw(renderer.frame());
        let _ = renderer.present(&mut std::io::sink());
        started.elapsed()
    }

    /// The attribute-write turns M11.3 is measured by, as (label, css, script).
    ///
    /// Two writes, not one, and the pair is the whole point: the write on
    /// `<body>` is the one M10.6 measured, and `<body>`'s subtree **is** the
    /// document — so the narrowing has nothing to narrow and the number must
    /// not move. The write on an ordinary leaf is the shape a click has, and is
    /// the case the narrowing exists for. Reporting only the second would be a
    /// narrowed benchmark meeting a budget.
    const ATTRIBUTE_TURNS: [(&str, &str, &str); 4] = [
        (
            "<body> class, paint only ",
            ".x-tint p { color: #c00 }",
            "document.body.classList.add('x-tint');",
        ),
        (
            "<body> class, relayouting",
            ".x-move p { margin-left: 1px }",
            "document.body.classList.add('x-move');",
        ),
        (
            "leaf class,   paint only ",
            ".x-tint { color: #c00 }",
            "document.getElementById('x11-3-leaf').className = 'x-tint';",
        ),
        (
            "leaf class,   relayouting",
            ".x-move { margin-left: 1px }",
            "document.getElementById('x11-3-leaf').className = 'x-move';",
        ),
    ];

    /// Mean and range of a set of samples — the spread is reported because on
    /// this machine it is several percent wide, and a difference smaller than
    /// it is not a difference.
    fn summarize(samples: &[Duration]) -> String {
        let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
        let lo = samples.iter().min().unwrap();
        let hi = samples.iter().max().unwrap();
        format!("{mean:.2?} ({lo:.2?}-{hi:.2?})")
    }

    /// Every turn in [`ATTRIBUTE_TURNS`], with the narrowing off and then on,
    /// alternating within each round on the same page: A/B interleaved, in one
    /// process, because this machine drifts several percent between runs and a
    /// before-commit/after-commit pair would be measuring the drift.
    fn measure_attribute_turns(label: &str, page: impl Fn(&str, &str) -> String) {
        const ROUNDS: usize = 5;
        let mut before = vec![Vec::new(); ATTRIBUTE_TURNS.len()];
        let mut after = vec![Vec::new(); ATTRIBUTE_TURNS.len()];
        let mut nodes = 0;

        // Round 0 is thrown away. The very first turn in the process pays for
        // pages the allocator has not touched and code the CPU has not seen,
        // and on Wikipedia it came in three times the mean — recording it would
        // hand the first case measured a penalty the others do not pay.
        for round in 0..=ROUNDS {
            for (i, (_, css, script)) in ATTRIBUTE_TURNS.iter().enumerate() {
                let source = page(css, script);
                let (mut app, id) = scripted_app(&source);
                app.full_restyle_only = true;
                let full = timed_js_turn(&mut app, id);

                let (mut app, id) = scripted_app(&source);
                let scoped = timed_js_turn(&mut app, id);
                nodes = app.dom.as_ref().map_or(0, |d| d.node_count());

                if round > 0 {
                    before[i].push(full);
                    after[i].push(scoped);
                }
            }
        }

        eprintln!("M11.3 turns on {label} ({nodes} nodes), mean of {ROUNDS} interleaved rounds:");
        for (i, (what, _, _)) in ATTRIBUTE_TURNS.iter().enumerate() {
            eprintln!(
                "  {what}  full {}  ->  scoped {}",
                summarize(&before[i]),
                summarize(&after[i]),
            );
        }
    }

    /// M11.7 deliverable 7. The header build is on the load path of **every**
    /// request, not just the document: a Wikipedia article asks the jar once
    /// per stylesheet and once per image. It is a map lookup, a filter and a
    /// join over at most `MAX_PER_HOST` cookies, so the expected answer is
    /// "nothing measurable" — but this is the first thing M11 has put on every
    /// request, so it is reported rather than asserted.
    ///
    /// A/B interleaved in one process, on the same page, because this machine
    /// drifts 5–10% between runs. The A side is an empty jar and a response
    /// with no `Set-Cookie`, which is exactly the code M11.6 shipped; the B
    /// side is a jar filled to the per-host cap and a response setting two
    /// more, which is the worst case a same-origin server can produce.
    ///
    /// ```text
    /// cargo test --release --lib measure_the_cookie_work -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_the_cookie_work_on_a_page_load() {
        const ROUNDS: usize = 5;
        const URL: &str = "https://en.wikipedia.org/wiki/Cat";
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));

        // One side of one pair: parse outside the clock (the parse is the
        // worker's job and identical either way), then time the two messages
        // the load path is made of. Returns how many subresource requests the
        // page produced, since that is how many times the jar was asked.
        let one_load = |seeded: bool| -> (Duration, usize) {
            let dom = crate::html::parse(fixture);
            let mut app = App::new(80, 24);
            if seeded {
                for i in 0..crate::js::cookies::MAX_PER_HOST {
                    app.cookies
                        .insert(
                            crate::js::cookies::Cookie {
                                name: format!("c{i}"),
                                value: "x".repeat(40),
                                host: "en.wikipedia.org".into(),
                                path: "/".into(),
                                expires: None,
                                secure: false,
                                http_only: i % 2 == 0,
                                same_site: crate::js::cookies::SameSite::Unspecified,
                            },
                            crate::js::cookies::now(),
                        )
                        .expect("the cap is exactly MAX_PER_HOST");
                }
            }
            let set_cookie = match seeded {
                true => vec!["c0=refreshed; Path=/".into(), "sid=abc; HttpOnly".into()],
                false => Vec::new(),
            };
            let id = app.start_fetch(URL.into());
            let started = Instant::now();
            app.update(Msg::Loaded {
                id,
                url: URL.into(),
                status: 200,
                body: fixture.as_bytes().to_vec(),
                elapsed: Duration::ZERO,
                content_type: None,
                set_cookie,
                metadata: Default::default(),
            });
            let effect = app.update(Msg::Parsed {
                id,
                dom,
                elapsed: Duration::ZERO,
            });
            let elapsed = started.elapsed();
            (elapsed, effect.sheets.len() + effect.images.len())
        };

        let (mut without, mut with) = (Vec::new(), Vec::new());
        let mut requests = 0;
        // Round 0 is thrown away, as everywhere else here: the first load in
        // the process pays for pages the allocator has not touched.
        for round in 0..=ROUNDS {
            // Which side goes first alternates, so any residue of running
            // second cancels across rounds instead of landing on one column.
            let (a, b) = match round % 2 == 0 {
                true => {
                    let a = one_load(false);
                    let b = one_load(true);
                    (a, b)
                }
                false => {
                    let b = one_load(true);
                    let a = one_load(false);
                    (a, b)
                }
            };
            requests = b.1;
            if round > 0 {
                without.push(a.0);
                with.push(b.0);
            }
        }
        eprintln!(
            "M11.7 load of en.wikipedia.org ({requests} subresource requests, \
             {} cookies in the jar), mean of {ROUNDS} interleaved rounds:",
            crate::js::cookies::MAX_PER_HOST
        );
        eprintln!(
            "  Loaded + Parsed  empty jar {}  ->  full jar {}",
            summarize(&without),
            summarize(&with),
        );
    }

    /// A measurement, not an assertion — see the note on the M10.6 pair.
    ///
    /// ```text
    /// cargo test --release --lib measure_a_keystroke -- --ignored --nocapture
    /// ```
    ///
    /// M11.9's deliverable 4: what a keystroke costs, both ways, before either
    /// is optimised. The page is Wikipedia — the ladder's worst page that has a
    /// field, and it has the real one (`#searchInput`) rather than one this
    /// test added. 80×24, the size a reader actually has.
    ///
    /// **A** is the simple path, and the one that ships: a keystroke is a text
    /// edit, so relayout and repaint, which is what `Dom::set_field_value`'s
    /// own invalidation rule says. **B** patches the one box whose text changed
    /// and repaints from the tree. Interleaved in one process, alternating
    /// which goes first, because this machine drifts 5–10% between runs
    /// (CLAUDE.md).
    #[test]
    #[ignore]
    fn measure_a_keystroke_in_a_field() {
        const ROUNDS: usize = 5;
        let source = wikipedia_with_script("", "1;");

        // One keystroke, from the key arriving to the bytes leaving: the same
        // pair the event loop times as `frame`. The renderer is built outside
        // the clock — the loop keeps one for the life of the process.
        let one = |narrow: bool| -> (Duration, (usize, usize, usize)) {
            let mut app = App::new(80, 24);
            let id = app.start_fetch("http://x/".into());
            load(&mut app, id, source.as_bytes().to_vec());
            app.update(parsed(id, &source));
            app.update(Msg::RunScripts { id });
            app.narrow_keystroke = narrow;
            let dom = app.dom.as_ref().unwrap();
            app.focus = Some(
                (0..dom.node_count() as u32)
                    .map(NodeId)
                    .find(|&n| dom.attr(n, "id") == Some("searchInput"))
                    .expect("the article's own search field"),
            );
            app.mode = Mode::Field { caret: 0 };
            let mut renderer =
                crate::term::Renderer::new(80, 24, crate::term::detect_caps_from_env());
            // One keystroke thrown away, then the one that counts: the first
            // draw in a process pays for a frame buffer nothing has touched,
            // and the steady state is every keystroke after a field's first.
            app.update(ch('x'));
            app.draw(renderer.frame());
            let _ = renderer.present(&mut std::io::sink());
            let before = stages(&app);
            let started = Instant::now();
            app.update(ch('y'));
            app.draw(renderer.frame());
            let _ = renderer.present(&mut std::io::sink());
            let elapsed = started.elapsed();
            let after = stages(&app);
            (
                elapsed,
                (after.0 - before.0, after.1 - before.1, after.2 - before.2),
            )
        };

        let (mut simple, mut narrow) = (Vec::new(), Vec::new());
        let (mut simple_stages, mut narrow_stages) = ((0, 0, 0), (0, 0, 0));
        for round in 0..=ROUNDS {
            // Which side goes first alternates, so any residue of running
            // second cancels across rounds instead of landing on one column.
            let (a, b) = match round % 2 == 0 {
                true => {
                    let a = one(false);
                    (a, one(true))
                }
                false => {
                    let b = one(true);
                    (one(false), b)
                }
            };
            simple_stages = a.1;
            narrow_stages = b.1;
            if round > 0 {
                simple.push(a.0);
                narrow.push(b.0);
            }
        }
        eprintln!(
            "M11.9 keypress→screen on en.wikipedia.org at 80×24, \
             mean of {ROUNDS} interleaved rounds (budget < 10 ms):"
        );
        eprintln!(
            "  relayout + repaint (ships)  {}  \
             (styles/layouts/paints per keystroke: {simple_stages:?})",
            summarize(&simple),
        );
        eprintln!(
            "  patch + repaint             {}  \
             (styles/layouts/paints per keystroke: {narrow_stages:?})",
            summarize(&narrow),
        );
    }

    /// M11.12's activation measurement, using one of Wikipedia's real dropdown
    /// checkboxes and the same narrowed-box B side as the text-field benchmark.
    ///
    /// ```text
    /// cargo test --release --lib measure_choice_activation -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_choice_activation_on_wikipedia() {
        const ROUNDS: usize = 5;
        let source = wikipedia_with_script("", "1;");
        let one = |narrow: bool| -> (Duration, (usize, usize, usize)) {
            let mut app = App::new(80, 24);
            let id = app.start_fetch("http://x/".into());
            load(&mut app, id, source.as_bytes().to_vec());
            app.update(parsed(id, &source));
            app.update(Msg::RunScripts { id });
            app.narrow_keystroke = narrow;
            let dom = app.dom.as_ref().unwrap();
            app.focus = Some(
                (0..dom.node_count() as u32)
                    .map(NodeId)
                    .find(|&node| {
                        matches!(&dom.node(node).data, NodeData::Element { tag, .. } if tag == "input")
                            && dom.attr(node, "type") == Some("checkbox")
                    })
                    .expect("Wikipedia's dropdown checkbox"),
            );
            let mut renderer =
                crate::term::Renderer::new(80, 24, crate::term::detect_caps_from_env());
            app.draw(renderer.frame());
            let _ = renderer.present(&mut std::io::sink());
            let before = stages(&app);
            let started = Instant::now();
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            app.draw(renderer.frame());
            let _ = renderer.present(&mut std::io::sink());
            let elapsed = started.elapsed();
            let after = stages(&app);
            (
                elapsed,
                (after.0 - before.0, after.1 - before.1, after.2 - before.2),
            )
        };

        let (mut simple, mut narrow) = (Vec::new(), Vec::new());
        let (mut simple_stages, mut narrow_stages) = ((0, 0, 0), (0, 0, 0));
        for round in 0..=ROUNDS {
            let (a, b) = if round % 2 == 0 {
                let a = one(false);
                (a, one(true))
            } else {
                let b = one(true);
                (one(false), b)
            };
            simple_stages = a.1;
            narrow_stages = b.1;
            if round > 0 {
                simple.push(a.0);
                narrow.push(b.0);
            }
        }
        eprintln!(
            "M11.12 checkbox keypress→screen on en.wikipedia.org at 80×24, \
             mean of {ROUNDS} interleaved rounds (budget < 10 ms):"
        );
        eprintln!(
            "  relayout + repaint (ships)  {}  \
             (styles/layouts/paints: {simple_stages:?})",
            summarize(&simple),
        );
        eprintln!(
            "  patch + repaint             {}  \
             (styles/layouts/paints: {narrow_stages:?})",
            summarize(&narrow),
        );
    }

    /// The first element carrying `id="x11-3-leaf"` — the paragraph
    /// `wikipedia_with_script` appends, and the leaf the turns above write to.
    fn probe_leaf(dom: &Dom) -> NodeId {
        (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&id| dom.attr(id, "id") == Some("x11-3-leaf"))
            .expect("the fixture lost its probe leaf")
    }

    /// A measurement, not an assertion: it asserts nothing and prints numbers,
    /// so it is `#[ignore]`d out of the default loop it would otherwise make
    /// ten times slower. Run it the way the numbers in `perf.md` were taken:
    ///
    /// ```text
    /// cargo test --release --lib measure_the_invalidation -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_the_invalidation_paths_on_a_wikipedia_sized_page() {
        // M10.6's deliverables 2, 5 and 7, and M11.3's deliverable 6.
        // Interleaved: this machine drifts several percent between runs of the
        // same thing, so a single before-then-after pair proves nothing.
        const ROUNDS: u32 = 5;
        let mut noop = Duration::ZERO;
        let (mut compare, mut layout_alone) = (Duration::ZERO, Duration::ZERO);
        let (mut restyle_alone, mut scoped_alone, mut clone_alone) =
            (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        let mut nodes = 0;

        for _ in 0..ROUNDS {
            // A tick that changes nothing at all: no stage runs, so this is the
            // floor every other turn is measured against.
            let (mut app, id) = scripted_app(&wikipedia_with_script(
                "",
                "document.querySelectorAll('a').length;",
            ));
            noop += timed_js_turn(&mut app, id);
            nodes = app.dom.as_ref().map_or(0, |d| d.node_count());

            // The stages themselves, so the turns can be read as a breakdown
            // rather than as opaque numbers.
            let (mut app, _) =
                scripted_app(&wikipedia_with_script(".x-tint { color: #c00 }", "1;"));
            let dom = app.dom.as_ref().unwrap();
            let styles = app.styles.as_ref().unwrap();

            let started = Instant::now();
            assert!(styles.layout_eq(styles));
            compare += started.elapsed();

            let started = Instant::now();
            let _ = layout::layout_document_with(
                dom,
                styles,
                column(80).width,
                layout::Hidden::Respect,
                &app.images.context(),
            );
            layout_alone += started.elapsed();

            // The two restyles, back to back on the same page: the document
            // pass, and the subtree pass for a class written on one leaf.
            let leaf = probe_leaf(app.dom.as_ref().unwrap());
            let dom = app.dom.as_mut().unwrap();
            dom.set_attr(leaf, "class", "x-tint");
            let changes = dom.take_attr_changes();

            // What M11.3 added to the path, measured on the same tree the
            // scoped pass is about to copy: `Styles::layout_eq` still decides
            // on layout, so the values the page was laid out with have to
            // survive a pass that writes in place. `scoped_alone` below
            // *includes* this copy — it is part of the stage, not beside it.
            //
            // One copy thrown away first. It is 5 MB, and whichever of the two
            // measurements runs against a cold allocator pays the page faults
            // for all of them — which made these two numbers swap places
            // depending on the order they were written in. Warming it measures
            // the steady state, which is every attribute write after a page's
            // first.
            drop(app.styles.as_ref().unwrap().clone());

            let started = Instant::now();
            let copy = app.styles.as_ref().unwrap().clone();
            clone_alone += started.elapsed();
            assert_eq!(copy.node_count(), app.dom.as_ref().unwrap().node_count());
            drop(copy);

            let started = Instant::now();
            assert!(app.restyle_scoped(&changes).is_some());
            scoped_alone += started.elapsed();

            let started = Instant::now();
            app.restyle();
            restyle_alone += started.elapsed();
        }

        eprintln!(
            "M11.3 stages on {nodes} nodes, mean of {ROUNDS} interleaved rounds:\n  \
             turn: tick that changes nothing   {:?}\n  \
             stage: full restyle               {:?}\n  \
             stage: scoped restyle, one leaf   {:?}\n  \
             stage: Styles::clone              {:?}\n  \
             stage: Styles::layout_eq          {:?}\n  \
             stage: one layout                 {:?}",
            noop / ROUNDS,
            restyle_alone / ROUNDS,
            scoped_alone / ROUNDS,
            clone_alone / ROUNDS,
            compare / ROUNDS,
            layout_alone / ROUNDS,
        );
        measure_attribute_turns("en.wikipedia.org", wikipedia_with_script);
    }

    /// A measurement, not an assertion: it asserts nothing and prints numbers,
    /// so it is `#[ignore]`d out of the default loop it would otherwise make
    /// ten times slower. Run it the way the numbers in `perf.md` were taken:
    ///
    /// ```text
    /// cargo test --release --lib measure_the_invalidation -- --ignored --nocapture
    /// ```
    /// A measurement, not an assertion — see the note on the M10.6 pair.
    ///
    /// ```text
    /// cargo test --release --lib measure_an_interval -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_an_interval_animating_a_page() {
        // M10.9 deliverable 6: a 4 ms interval that toggles a class is the
        // shape of a page animating itself. Two questions — what one tick
        // costs, and whether a keystroke still lands inside its budget while
        // the interval is running.
        const ROUNDS: u32 = 5;
        for (label, fixture) in [
            (
                "danluu",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/danluu.com.html"
                )),
            ),
            (
                "wikipedia",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/en.wikipedia.org.html"
                )),
            ),
        ] {
            let page = format!(
                "{}<style>.beat p {{ color: #c00 }}</style><script>\
                 var on = false;\
                 setInterval(function () {{\
                   on = !on;\
                   if (on) document.body.classList.add('beat');\
                   else document.body.classList.remove('beat');\
                 }}, 4);</script>",
                fixture.replace("<script", "<script type=\"text/x-not-run\"")
            );

            let (mut app, id) = scripted_app(&page);
            app.update(Msg::RunScripts { id });
            let mut renderer =
                crate::term::Renderer::new(80, 24, crate::term::detect_caps_from_env());
            let (mut tick, mut keypress) = (Duration::ZERO, Duration::ZERO);

            for _ in 0..ROUNDS {
                // One interval tick, all the way to the screen.
                let started = Instant::now();
                app.update(Msg::Timer {
                    page: id,
                    id: TimerId(1),
                });
                app.draw(renderer.frame());
                let _ = renderer.present(&mut std::io::sink());
                tick += started.elapsed();

                // A keystroke arriving beside it: scroll, which must touch no
                // pipeline stage at all.
                let started = Instant::now();
                app.update(key(KeyCode::Char('j'), KeyModifiers::NONE));
                app.draw(renderer.frame());
                let _ = renderer.present(&mut std::io::sink());
                keypress += started.elapsed();
            }

            eprintln!(
                "M10.9 {label}: interval tick {:?} · keypress→screen while it runs {:?}",
                tick / ROUNDS,
                keypress / ROUNDS,
            );
        }
    }

    #[test]
    #[ignore]
    fn measure_the_invalidation_paths_on_an_ordinary_page() {
        // The same turns on a page the size most of the web is, so the
        // keypress→screen number can say *where* the budget holds rather than
        // only that the largest page on the ladder blows it.
        const ROUNDS: u32 = 5;
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/danluu.com.html"
        ));
        let page = |css: &str, script: &str| {
            format!(
                "{}<p id=x11-3-leaf>leaf</p><style>{css}</style><script>{script}</script>",
                fixture.replace("<script", "<script type=\"text/x-not-run\"")
            )
        };

        let mut noop = Duration::ZERO;
        for _ in 0..ROUNDS {
            let (mut app, id) = scripted_app(&page("", "document.querySelectorAll('a').length;"));
            noop += timed_js_turn(&mut app, id);
        }
        eprintln!(
            "M11.3 turn: tick that changes nothing (danluu) {:?}",
            noop / ROUNDS
        );
        measure_attribute_turns("danluu.com", page);
    }

    #[test]
    fn a_thousand_mutations_in_one_tick_lay_the_page_out_once() {
        // The coarse signal, and the invariant that makes it worth having:
        // the pass reports "the DOM changed" once, not once per mutation. Get
        // this wrong and a script that builds a list relayouts per element —
        // the single most expensive mistake available in this milestone.
        let (mut app, id) = scripted_app(
            "<div id=list></div><script>\
             var list = document.getElementById('list');\
             for (var i = 0; i < 1000; i++) {\
               var row = document.createElement('p');\
               row.textContent = 'row ' + i;\
               list.appendChild(row);\
             }</script>",
        );
        let laid_out = app.layouts;

        assert_eq!(app.update(Msg::RunScripts { id }), redraw());
        assert_eq!(
            app.layouts,
            laid_out + 1,
            "1000 appendChild calls must relayout once, not 1000 times"
        );

        // And the page really did gain the rows: a tick that changes the tree
        // without changing the screen is the other half of the bug.
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        assert!(
            (0..10).any(|y| row_text(&frame, y).contains("row 0")),
            "the script's rows never reached the screen"
        );
    }

    #[test]
    fn a_tick_that_changes_nothing_does_not_relayout() {
        // The other side of the signal: a page whose script only reads must
        // not pay for a relayout it did not earn.
        let (mut app, id) =
            scripted_app("<p>text</p><script>document.querySelectorAll('p').length;</script>");
        let laid_out = app.layouts;
        assert_eq!(app.update(Msg::RunScripts { id }), Effect::default());
        assert_eq!(app.layouts, laid_out, "a read-only tick relayouted");
    }

    #[test]
    fn a_page_with_no_script_still_reports_what_the_pass_cost() {
        // The pass walked the tree to discover there was nothing to run. F4 is
        // the instrument for what the engine did; hiding that walk would make
        // it a less honest one.
        let (mut app, id) = scripted_app("<p>no script anywhere</p>");
        app.update(Msg::RunScripts { id });
        assert!(app.timings().script.is_some());
    }

    #[test]
    fn a_script_pass_for_a_superseded_page_never_runs() {
        let (mut app, first) = scripted_app("<p>x</p><script>1</script>");
        // The user navigates before the pass's turn comes up.
        let second = app.start_fetch("http://y/".into());
        assert_ne!(first, second);

        assert_eq!(app.update(Msg::RunScripts { id: first }), Effect::default());
        assert_eq!(
            app.timings().script,
            None,
            "a stale generation's scripts ran"
        );
    }

    #[test]
    fn accepted_parsed_records_the_parse_duration_and_f4_shows_it() {
        let mut app = timed_app(40, 10);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(3));
        // A parse repaints *and* asks for the script pass (M10.2) — the pass
        // is a later turn, so this one still paints without waiting for it.
        assert_eq!(
            app.update(parsed(id, "<p>hi</p>")),
            Effect {
                dirty: true,
                run_scripts: Some(id),
                ..Effect::default()
            }
        );
        assert_eq!(app.timings().parse, Some(Duration::from_micros(31_700)));

        app.update(f4());
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        let overlay: String = (0..3).map(|y| row_text(&frame, y)).collect();
        assert!(overlay.contains("parse 31.7 ms"), "overlay was {overlay:?}");
    }

    #[test]
    fn stale_parsed_is_ignored() {
        let mut app = App::new(40, 10);
        let stale = app.start_fetch("http://old/".into());
        let _current = app.start_fetch("http://new/".into());
        assert_eq!(app.update(parsed(stale, "<p>old</p>")), Effect::default());
        assert_eq!(
            app.timings().parse,
            None,
            "a stale Parsed must not record a duration"
        );
        app.update(f1());
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).contains("no DOM yet"),
            "a stale tree must not become the inspector's content"
        );
    }

    // ---- fragment navigation (M11.4) --------------------------------------

    /// A page far taller than the viewport with `id=target` in the middle of
    /// it, and a link to it at the top. `body` goes before the target, so a
    /// jump has somewhere to jump *from* and somewhere to come back to.
    fn anchored_page(w: u16, h: u16) -> App {
        let filler =
            |what: &str| -> String { (0..40).map(|i| format!("<p>{what} {i}</p>")).collect() };
        page(
            w,
            h,
            &format!(
                "<p><a href='#target'>jump</a></p>{}<p id=target>the target</p>{}",
                filler("before"),
                filler("after"),
            ),
        )
    }

    /// The top visible row of a drawn frame — where a fragment jump has to put
    /// its target, and the only assertion the reader can actually make.
    fn top_row(app: &App) -> String {
        let (w, h) = app.size();
        let mut frame = Frame::new(w, h);
        app.draw(&mut frame);
        row_text(&frame, 0).trim().to_string()
    }

    /// Everything on screen, for the cases where the target cannot reach the
    /// top row because the document ends first.
    fn visible_text(app: &App) -> String {
        let (w, h) = app.size();
        let mut frame = Frame::new(w, h);
        app.draw(&mut frame);
        (0..h).map(|y| row_text(&frame, y)).collect()
    }

    #[test]
    fn a_fragment_click_puts_the_target_on_the_top_row_and_runs_no_stage() {
        // Deliverables 2 and 3. The counters are the point: a jump is a
        // scroll, so the cached display list is repainted at a new offset and
        // *nothing* upstream of paint runs. Getting this wrong costs 43 ms on
        // Wikipedia and is invisible on screen, which is exactly why the test
        // asserts on counters rather than on appearance alone.
        let mut app = anchored_page(80, 10);
        assert!(!top_row(&app).contains("the target"));

        // A search session open across the jump. The document it points into
        // is the same document, so it survives — unlike a real navigation,
        // which replaces the document and clears it. `jump_to_fragment` says
        // so in a comment; this is what holds it to that.
        app.update(ch('/'));
        for c in "before".chars() {
            app.update(ch(c));
        }
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let matches = app.search.as_ref().map(|s| s.matches.len());
        assert!(matches.is_some_and(|n| n > 0), "the fixture needs matches");
        app.update(ch('g'));
        app.update(ch('g'));
        let before = stages(&app);

        let effect = app.update(click_first_link(&app));
        assert!(
            effect.dirty,
            "the viewport moved without asking for a frame"
        );
        assert!(effect.fetch.is_none(), "a fragment click fetched");
        assert_eq!(
            stages(&app),
            before,
            "a fragment jump restyled, relayouted or repainted"
        );
        assert_eq!(top_row(&app), "the target");
        assert!(app.viewport.offset() > 0);
        assert_eq!(
            app.search.as_ref().map(|s| s.matches.len()),
            matches,
            "a fragment jump cleared the search session"
        );
    }

    #[test]
    fn a_fragment_jump_changes_the_url_without_touching_the_fetch_generation() {
        // Deliverable 4. A new generation would cancel the sheets, images and
        // scripts still in flight — the page did not reload.
        let mut app = anchored_page(80, 10);
        let (generation, current) = (app.fetch_gen, app.current_fetch);

        app.update(click_first_link(&app));
        assert_eq!(app.current_url().as_deref(), Some("http://final/#target"));
        // …and the reader can see it: the status row reads the same string.
        let mut frame = Frame::new(80, 10);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 9).contains("http://final/#target"),
            "the URL bar still shows the old URL: {:?}",
            row_text(&frame, 9)
        );
        assert_eq!(
            app.fetch_gen, generation,
            "a fragment jump bumped the generation"
        );
        assert_eq!(app.current_fetch, current);
        assert!(matches!(app.fetch, Fetch::Loaded { .. }));

        // And the URL the page is now known by is still a usable base: a
        // stylesheet that stopped loading after a citation click is the
        // expensive version of this bug.
        assert_eq!(
            app.resolve_href("style.css").as_deref(),
            Some("http://final/style.css")
        );
        assert_eq!(
            app.resolve_href("#other").as_deref(),
            Some("http://final/#other")
        );
    }

    #[test]
    fn the_bare_fragment_goes_to_the_top_and_an_unknown_one_goes_nowhere() {
        // Deliverable 1's two named cases. `href="#"` is nine of the Wikipedia
        // fixture's links and means the top of the document; a fragment that
        // matches nothing means *stay put* — no scroll, no error page, no
        // console line — and the URL still changes, because it did.
        let mut app = anchored_page(80, 10);
        app.update(click_first_link(&app));
        let at_target = app.viewport.offset();
        assert!(at_target > 0);

        app.follow_href("#nothing-is-named-this");
        assert_eq!(
            app.viewport.offset(),
            at_target,
            "an unknown fragment moved the viewport"
        );
        assert!(app.console.is_empty(), "an unknown fragment complained");
        assert!(app.dom.is_some(), "an unknown fragment replaced the page");
        assert_eq!(
            app.current_url().as_deref(),
            Some("http://final/#nothing-is-named-this")
        );

        app.follow_href("#");
        assert_eq!(app.viewport.offset(), 0, "a bare # must go to the top");
        assert_eq!(app.current_url().as_deref(), Some("http://final/#"));
    }

    #[test]
    fn the_same_url_without_a_fragment_reloads_rather_than_jumping() {
        // M11.4 review. "Same document" is not the same thing as "a fragment
        // jump": a navigation to the current URL with **no** fragment is a
        // reload (HTML), and the two ways a reader reaches it are the URL bar
        // and a page's own `location.reload()`. Deciding those by
        // `same_document` alone scrolled them to the top of the page and — for
        // the URL bar — pushed a history entry for a navigation that never
        // happened.
        let mut app = anchored_page(80, 10);
        app.follow_href("#target");
        let at_target = app.viewport.offset();
        assert!(at_target > 0);

        // The URL bar, re-entering the page's own URL, fragment and all
        // stripped by the reader.
        app.update(ch('o'));
        for c in "http://final/".chars() {
            app.update(ch(c));
        }
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            effect.fetch.map(|(_, request)| request.url).as_deref(),
            Some("http://final/"),
            "re-entering the current URL did not reload"
        );
    }

    #[test]
    fn location_reload_refetches_the_page() {
        // A script reload reaches the explicit reload policy, not ordinary
        // replace navigation: even this fresh representation must validate.
        let html = "<p>page</p><script>location.reload();</script>";
        let mut app = App::new(40, 10);
        let first = app.start_navigation("http://final/".into());
        let id = first.fetch.unwrap().0;
        load_cacheable(
            &mut app,
            id,
            "http://final/",
            html.as_bytes(),
            cache_metadata(&["max-age=60"], Some("\"one\"")),
        );
        app.update(parsed(id, html));
        let effect = app.update(Msg::RunScripts { id });
        let (again, url) = effect.fetch.expect("location.reload() did not fetch");
        assert_eq!(url.url, "http://final/");
        assert_eq!(
            url.method,
            net::Method::Conditional {
                no_cache: true,
                if_none_match: Some("\"one\"".into()),
            }
        );
        assert_ne!(again, id, "a reload is a new generation");
        assert!(!app.history.can_back(), "a reload is not a history entry");
    }

    #[test]
    fn a_target_near_the_end_is_clamped_by_the_end_of_the_document() {
        // Deliverable 2's other half: top of the viewport, *clamped*. There
        // are not enough rows left to put the last paragraph on the top row,
        // and a browser does not invent them.
        let mut app = page(
            80,
            10,
            &format!(
                "<p><a href='#target'>jump</a></p>{}<p id=target>the target</p>",
                (0..40)
                    .map(|i| format!("<p>line {i}</p>"))
                    .collect::<String>(),
            ),
        );
        app.update(click_first_link(&app));
        let jumped = app.viewport.offset();
        assert!(visible_text(&app).contains("the target"));
        // As far down as `G` goes and no further: the same clamp every scroll
        // gets, rather than a second opinion about where the document ends.
        app.update(ch('G'));
        assert_eq!(jumped, app.viewport.offset());
    }

    #[test]
    fn a_hidden_target_lands_on_the_nearest_laid_out_ancestor() {
        // Deliverable 8, first case: `display: none`. The reader goes to where
        // the element would have been — the wrapper's row — rather than
        // nowhere. `layout::nearest_y` documents why the other answer is not
        // available: a box that was never generated and a box that was merged
        // into its neighbour are indistinguishable from the layout tree, and
        // the second case is an anchor the reader can plainly see.
        let mut app = page(
            80,
            10,
            &format!(
                "<style>#hidden {{ display: none }}</style>\
                 <p><a href='#hidden'>jump</a></p>{}\
                 <div id=wrap><p>wrapper</p><p id=hidden>invisible</p></div>{}",
                (0..40)
                    .map(|i| format!("<p>before {i}</p>"))
                    .collect::<String>(),
                (0..40)
                    .map(|i| format!("<p>after {i}</p>"))
                    .collect::<String>(),
            ),
        );
        let stages_before = stages(&app);

        app.follow_href("#hidden");
        // The wrapper's box, margin row included — the reader is at the top of
        // the block that holds the target, with what came before it off screen.
        let seen = visible_text(&app);
        assert!(seen.contains("wrapper"), "{seen:?}");
        assert!(
            !seen.contains("before 39"),
            "the jump stopped short: {seen:?}"
        );
        assert_eq!(
            stages(&app),
            stages_before,
            "a jump to a hidden target ran a stage"
        );
        assert!(
            !visible_text(&app).contains("invisible"),
            "the target is still hidden — the jump does not reveal it"
        );
        assert_eq!(app.current_url().as_deref(), Some("http://final/#hidden"));
    }

    #[test]
    fn history_comes_back_to_where_the_reader_was_without_fetching() {
        // Deliverable 5, and the reason it is a deliverable: `H` off a
        // fragment jump that refetched would be a network round trip to land
        // on the page already on screen.
        let mut app = anchored_page(80, 10);
        app.update(ch('j'));
        app.update(ch('j'));
        let reading = app.viewport.offset();
        assert!(reading > 0);

        app.follow_href("#target");
        let at_target = app.viewport.offset();
        assert_ne!(at_target, reading);
        let stages_before = stages(&app);

        let back = app.update(ch('H'));
        assert!(back.fetch.is_none(), "H off a fragment jump refetched");
        assert_eq!(
            app.viewport.offset(),
            reading,
            "H did not restore the offset"
        );
        assert_eq!(app.current_url().as_deref(), Some("http://final/"));

        let forward = app.update(ch('L'));
        assert!(forward.fetch.is_none(), "L back to a fragment refetched");
        assert_eq!(
            app.viewport.offset(),
            at_target,
            "L did not return to the citation"
        );
        assert_eq!(app.current_url().as_deref(), Some("http://final/#target"));
        assert_eq!(
            stages(&app),
            stages_before,
            "a same-document history move ran a stage"
        );
    }

    #[test]
    fn a_cross_document_link_with_a_fragment_scrolls_once_its_own_layout_exists() {
        // Deliverable 7. The fragment cannot be resolved when the navigation
        // starts — the node it names does not exist until the parse lands — so
        // it is held as text against *this* generation, and a resize of the
        // page still on screen must not consume it.
        let mut app = page(80, 10, "<p><a href='/other#target'>go</a></p>");
        let effect = app.update(click_first_link(&app));
        let (id, url) = effect.fetch.expect("a cross-document link must fetch");
        assert_eq!(url.url, "http://final/other#target");

        app.update(Msg::Resize(70, 10));
        assert!(
            app.pending_scroll.is_some(),
            "a resize during Loading consumed the fragment"
        );

        // Filler on both sides: a target at the very end of a document cannot
        // reach the top row, because the scroll clamps there.
        let body = format!(
            "{}<p id=target>the target</p>{}",
            (0..40)
                .map(|i| format!("<p>before {i}</p>"))
                .collect::<String>(),
            (0..40)
                .map(|i| format!("<p>after {i}</p>"))
                .collect::<String>(),
        );
        app.update(Msg::Loaded {
            id,
            url: "http://final/other#target".into(),
            status: 200,
            body: body.clone().into_bytes(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(id, &body));
        assert_eq!(top_row(&app), "the target");
        assert!(
            app.pending_scroll.is_none(),
            "the fragment was not consumed"
        );
    }

    #[test]
    fn a_redirect_that_drops_the_fragment_does_not_lose_it() {
        // `Location` is joined against the previous URL and a bare path has no
        // fragment of its own, so reqwest reports one without it. HTML keeps
        // the original — and the URL bar has to agree with the row we scrolled
        // to.
        let mut app = App::new(80, 10);
        let id = app.start_fetch("http://final/old#target".into());
        // Filler on both sides: a target at the very end of a document cannot
        // reach the top row, because the scroll clamps there.
        let body = format!(
            "{}<p id=target>the target</p>{}",
            (0..40)
                .map(|i| format!("<p>before {i}</p>"))
                .collect::<String>(),
            (0..40)
                .map(|i| format!("<p>after {i}</p>"))
                .collect::<String>(),
        );
        app.update(Msg::Loaded {
            id,
            url: "http://final/new".into(),
            status: 200,
            body: body.clone().into_bytes(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        assert_eq!(
            app.current_url().as_deref(),
            Some("http://final/new#target")
        );
        app.update(parsed(id, &body));
        assert_eq!(top_row(&app), "the target");
    }

    #[test]
    fn a_restored_offset_beats_the_fragment_in_the_url_it_restores() {
        // The one-slot decision, stated as a test: a history entry for
        // `/other#target` restores the offset the reader left, not the anchor
        // the URL happens to name. Same rule for `r`.
        let body = format!(
            "{}<p id=target>the target</p>{}",
            (0..40)
                .map(|i| format!("<p>before {i}</p>"))
                .collect::<String>(),
            (0..40)
                .map(|i| format!("<p>after {i}</p>"))
                .collect::<String>(),
        );
        let mut app = App::new(80, 10);
        let id = app.start_fetch("http://final/other#target".into());
        app.update(Msg::Loaded {
            id,
            url: "http://final/other#target".into(),
            status: 200,
            body: body.clone().into_bytes(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(id, &body));
        let at_target = app.viewport.offset();
        assert!(at_target > 0, "the fragment did not land on the first load");

        // The reader scrolls on from the anchor, then leaves and comes back.
        app.update(ch('j'));
        app.update(ch('j'));
        let reading = app.viewport.offset();
        let effect = app.navigate("http://final/elsewhere".into(), true);
        let (next, _) = effect.fetch.expect("a cross-document navigation");
        app.update(Msg::Loaded {
            id: next,
            url: "http://final/elsewhere".into(),
            status: 200,
            body: b"<p>elsewhere</p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(next, "<p>elsewhere</p>"));

        let back = app.update(ch('H'));
        let (restored, url) = back.fetch.expect("a different document must fetch");
        assert_eq!(url.url, "http://final/other#target");
        app.update(Msg::Loaded {
            id: restored,
            url: "http://final/other#target".into(),
            status: 200,
            body: body.clone().into_bytes(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(restored, &body));
        assert_eq!(
            app.viewport.offset(),
            reading,
            "the restore landed on the fragment instead of the reader's position"
        );
    }

    #[test]
    fn a_fragment_is_resolved_once_and_never_retried() {
        // Deliverable 8, second case. A page whose script appends the target
        // later does not get to yank the viewport at some arbitrary moment
        // after the reader has arrived and started reading; the reader sees
        // the page where it opened, with the fragment in the URL bar, and a
        // second click on the link jumps now that the element exists.
        let body = format!(
            "<p>top</p>{}<script>\
             var late = document.createElement('p');\
             late.setAttribute('id', 'late');\
             late.textContent = 'the late target';\
             document.body.appendChild(late);</script>",
            (0..40)
                .map(|i| format!("<p>line {i}</p>"))
                .collect::<String>(),
        );
        let mut app = App::new(80, 10);
        let id = app.start_fetch("http://final/page#late".into());
        app.update(Msg::Loaded {
            id,
            url: "http://final/page#late".into(),
            status: 200,
            body: body.clone().into_bytes(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(parsed(id, &body));
        assert_eq!(app.viewport.offset(), 0, "nothing named `late` existed yet");

        app.update(Msg::RunScripts { id });
        assert_eq!(
            app.viewport.offset(),
            0,
            "the element arriving later pulled the viewport"
        );
        // …and the link works from here. The appended paragraph is the last
        // thing in the document, so the scroll clamps before it reaches the
        // top row — it is on screen, which is what the reader asked for.
        app.follow_href("#late");
        assert!(app.viewport.offset() > 0);
        assert!(
            visible_text(&app).contains("the late target"),
            "a second click did not reach the element the script added"
        );
    }

    #[test]
    fn location_hash_jumps_exactly_where_a_link_jumps() {
        // Deliverable 6: one implementation, two callers. The comment in
        // `js/bindings.rs` that promised a history entry for a fragment change
        // is now true, and `replace` still does not push one.
        let tall = |script: &str| {
            format!(
                "<p>top</p>{}<p id=target>the target</p><script>{script}</script>",
                (0..40)
                    .map(|i| format!("<p>line {i}</p>"))
                    .collect::<String>(),
            )
        };

        let (mut app, id) = scripted_app(&tall("location.hash = 'target';"));
        let stages_before = stages(&app);
        let effect = app.update(Msg::RunScripts { id });
        assert!(effect.fetch.is_none(), "a fragment change fetched");
        assert!(effect.dirty, "the jump did not ask for a frame");
        assert_eq!(app.current_url().as_deref(), Some("http://final/#target"));
        assert_eq!(stages(&app), stages_before, "a script's jump ran a stage");
        let at_target = app.viewport.offset();
        assert!(at_target > 0, "the script's jump did not scroll");
        assert!(
            app.history.can_back(),
            "a fragment change pushed no history"
        );

        // `H` returns to the top of the page the script left.
        app.update(ch('H'));
        assert_eq!(app.viewport.offset(), 0);
        assert_eq!(app.current_url().as_deref(), Some("http://final/"));

        // `replace` reaches the same place without an entry.
        let (mut app, id) = scripted_app(&tall("location.replace('#target');"));
        app.update(Msg::RunScripts { id });
        assert_eq!(app.viewport.offset(), at_target);
        assert!(!app.history.can_back(), "replace pushed a history entry");
    }

    #[test]
    fn location_hash_reads_back_what_a_script_set() {
        // The URL bar and `location.hash` read the same string, so the fix for
        // one is the fix for the other — one tick later. A script cannot see
        // its own assignment *within* the tick that made it: the navigation is
        // a request the tick records and `apply_script_navigation` acts on
        // afterwards, which is the same discipline every other side effect in
        // `src/js` follows.
        let (mut app, id) = scripted_app(
            "<p id=target>x</p><script>\
             location.hash = 'target';\
             setTimeout(function () { console.log('hash=' + location.hash); }, 0);</script>",
        );
        app.update(Msg::RunScripts { id });
        app.update(Msg::Timer {
            page: id,
            id: TimerId(1),
        });
        let logged: Vec<String> = app
            .console
            .entries()
            .iter()
            .map(|e| e.text.clone())
            .collect();
        assert!(
            logged.iter().any(|t| t == "hash=#target"),
            "the next tick read a stale location.hash: {logged:?}"
        );
    }

    /// The Wikipedia article, as the reader meets it: 686 `href="#…"` links
    /// against 269 `cite_note` ids, and a citation click that until now did
    /// nothing at all.
    fn wikipedia_page(w: u16, h: u16) -> App {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        // The fixture's own scripts are neutered for the same reason M11.3's
        // measurement neuters them: this test is about navigation, not about
        // what Wikipedia's analytics does on the way past.
        page(
            w,
            h,
            &fixture.replace("<script", "<script type=\"text/x-not-run\""),
        )
    }

    #[test]
    fn a_wikipedia_citation_click_lands_on_the_citation_and_h_comes_back() {
        // The acceptance round trip, on the ladder's biggest page.
        let mut app = wikipedia_page(80, 24);
        for _ in 0..30 {
            app.update(ch('j'));
        }
        let reading = app.viewport.offset();
        let stages_before = stages(&app);

        let effect = app.follow_href("#cite_note-Linnaeus1758-1");
        assert!(effect.fetch.is_none(), "a citation click fetched Wikipedia");
        assert!(effect.dirty);
        assert_eq!(
            stages(&app),
            stages_before,
            "a citation click restyled or relayouted 25,601 nodes"
        );
        assert_ne!(
            app.viewport.offset(),
            reading,
            "the citation click went nowhere"
        );
        // The `<li>` the citation lives in, at the top of the viewport: its
        // backlinks come first, then the reference text.
        let top = top_row(&app);
        assert!(
            top.contains("Linnaeus"),
            "the citation is not on the top row, which reads {top:?}"
        );
        assert_eq!(
            app.current_url().as_deref(),
            Some("http://final/#cite_note-Linnaeus1758-1")
        );

        let back = app.update(ch('H'));
        assert!(back.fetch.is_none(), "H refetched Wikipedia");
        assert_eq!(
            app.viewport.offset(),
            reading,
            "H did not return the reader"
        );
        assert_eq!(stages(&app), stages_before, "H ran a pipeline stage");
    }

    /// A measurement, not an assertion — the M11.3 shape, A/B interleaved in
    /// one process because this machine drifts several percent between runs.
    ///
    /// ```text
    /// cargo test --release --lib measure_a_fragment_jump -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_a_fragment_jump_on_a_wikipedia_sized_page() {
        // Deliverable 9: a fragment jump is a keypress→screen path (PLAN.md
        // §4: 10 ms). Two costs to separate — the DOM walk that finds the id
        // (25,601 nodes) and the layout walk that finds its row (~3.6k boxes)
        // — measured against the scroll they are bolted onto, since a plain
        // `j` is the same repaint without either walk.
        const ROUNDS: usize = 5;
        let mut jump = Vec::new();
        let mut scroll = Vec::new();
        let mut resolve = Vec::new();
        let mut row = Vec::new();
        let mut nodes = 0;

        // Round 0 is thrown away: the first turn in the process pays for pages
        // the allocator has not touched and code the CPU has not seen.
        for round in 0..=ROUNDS {
            let mut app = wikipedia_page(80, 24);
            let mut renderer =
                crate::term::Renderer::new(80, 24, crate::term::detect_caps_from_env());
            nodes = app.dom.as_ref().map_or(0, |d| d.node_count());

            // A: the jump, all the way to the screen.
            let started = Instant::now();
            app.follow_href("#cite_note-Linnaeus1758-1");
            app.draw(renderer.frame());
            let _ = renderer.present(&mut std::io::sink());
            let jumped = started.elapsed();

            // B: the same repaint without either walk — one scroll step.
            let started = Instant::now();
            app.update(ch('j'));
            app.draw(renderer.frame());
            let _ = renderer.present(&mut std::io::sink());
            let scrolled = started.elapsed();

            // The two walks on their own, so the turn reads as a breakdown.
            let dom = app.dom.as_ref().unwrap();
            let tree = app.layout_tree.as_ref().unwrap();
            let started = Instant::now();
            let target = fragment::resolve(dom, "cite_note-Linnaeus1758-1");
            let resolved = started.elapsed();
            let fragment::Target::Node(node) = target.expect("the fixture lost its citation")
            else {
                unreachable!("a cite_note id is an element, not the top of the document")
            };
            let started = Instant::now();
            assert!(layout::nearest_y(tree, dom, node).is_some());
            let rowed = started.elapsed();

            if round > 0 {
                jump.push(jumped);
                scroll.push(scrolled);
                resolve.push(resolved);
                row.push(rowed);
            }
        }

        eprintln!(
            "M11.4 fragment jump on en.wikipedia.org ({nodes} nodes), \
             mean of {ROUNDS} interleaved rounds:\n  \
             turn:  citation click, to the screen  {}\n  \
             turn:  one scroll step, same repaint  {}\n  \
             stage: fragment -> node (DOM walk)    {}\n  \
             stage: node -> row (layout walk)      {}",
            summarize(&jump),
            summarize(&scroll),
            summarize(&resolve),
            summarize(&row),
        );
    }

    /// Turn M11.5's inserted-script detection off — **both halves**: the drain
    /// after every tick, and the tag comparison the insert bindings make. The
    /// A side of the measurement below, and its only caller.
    #[cfg(test)]
    fn without_insert_detection(app: &mut App) {
        app.no_insert_detection = true;
        if let Some(host) = app.js_host.as_ref() {
            host.disarm_script_inserts();
        }
    }

    /// A measurement, not an assertion — the M11.3 shape, A/B interleaved in
    /// one process because this machine drifts several percent between runs.
    ///
    /// ```text
    /// cargo test --release --lib measure_a_tick_that_inserts_no_script -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_a_tick_that_inserts_no_script() {
        // Deliverable 9. The detection sits on the invalidation path, which is
        // a keypress→screen path (PLAN.md §4: 10 ms) — and the number that
        // matters is the one for pages that never insert a script, because
        // that is every page on the ladder but one.
        //
        // Two groups, because the cost has two halves and they live in
        // different places. The turns are M11.3's, so its table is the
        // baseline; the tick below isolates the per-call half at a scale where
        // it could show up at all.
        const ROUNDS: usize = 5;
        const TURNS: [(&str, &str, &str); 2] = [
            (
                "changes nothing        ",
                "",
                "document.querySelectorAll('a').length;",
            ),
            (
                "leaf class, paint only ",
                ".x-tint { color: #c00 }",
                "document.getElementById('x11-3-leaf').className = 'x-tint';",
            ),
        ];

        // One side of one pair: build the page, run the turn, **drop the app**.
        // The drop is not tidiness — a Wikipedia `App` is several megabytes of
        // DOM, styles and boxes, and leaving the first side's alive while the
        // second runs is a systematic tax on whichever side goes second. It
        // was worth ~5% here, which is the entire size of the effect being
        // looked for.
        let one_turn = |source: &str, detection: bool| -> (Duration, usize) {
            let (mut app, id) = scripted_app(source);
            if !detection {
                without_insert_detection(&mut app);
            }
            let elapsed = timed_js_turn(&mut app, id);
            (elapsed, app.dom.as_ref().map_or(0, |d| d.node_count()))
        };

        let mut before = vec![Vec::new(); TURNS.len()];
        let mut after = vec![Vec::new(); TURNS.len()];
        let mut nodes = 0;
        // Round 0 is thrown away: the first turn in the process pays for pages
        // the allocator has not touched and code the CPU has not seen.
        for round in 0..=ROUNDS {
            for (i, (_, css, script)) in TURNS.iter().enumerate() {
                let source = wikipedia_with_script(css, script);
                // Which side goes first alternates, so any residue of running
                // second cancels across rounds instead of landing on one
                // column.
                let (off, on) = match round % 2 == 0 {
                    true => {
                        let (off, _) = one_turn(&source, false);
                        let (on, count) = one_turn(&source, true);
                        nodes = count;
                        (off, on)
                    }
                    false => {
                        let (on, count) = one_turn(&source, true);
                        let (off, _) = one_turn(&source, false);
                        nodes = count;
                        (off, on)
                    }
                };
                if round > 0 {
                    before[i].push(off);
                    after[i].push(on);
                }
            }
        }
        eprintln!(
            "M11.5 turns on en.wikipedia.org ({nodes} nodes), mean of {ROUNDS} interleaved rounds:"
        );
        for (i, (what, _, _)) in TURNS.iter().enumerate() {
            eprintln!(
                "  tick that {what}  without detection {}  ->  with {}",
                summarize(&before[i]),
                summarize(&after[i]),
            );
        }

        // The other half: the tag comparison each `appendChild` makes. It is
        // paid per call, not per tick, so it is measured on a tick that makes
        // a great many of them — 4,000, which is as many as fit comfortably
        // inside one 100 ms budget, since an interrupted tick would time the
        // budget rather than the work. One call is orders of
        // magnitude below anything this machine can see.
        //
        // Into a **detached** holder on a small page, on purpose. Appending
        // into the document would make the number a measurement of the
        // relayout that follows (at 20,000 nodes it was already the whole
        // 100 ms budget), and the relayout is not what changed. Detached, the
        // binding runs exactly as it does in the document — `record` does not
        // care whether the node is connected; `App` decides that later — so
        // what is left in the number is the calls themselves.
        //
        // The detection is turned off *after* the script pass, so the host
        // exists and the A side runs with the bindings genuinely disarmed.
        const APPENDS: usize = 4_000;
        let page = format!(
            "<p>small page</p><script>setTimeout(function () {{\
               var host = document.createElement('div');\
               for (var i = 0; i < {APPENDS}; i++) host.appendChild(document.createElement('p'));\
             }}, 0);</script>"
        );
        let (mut off, mut on) = (Vec::new(), Vec::new());
        for round in 0..=ROUNDS {
            let order = match round % 2 == 0 {
                true => [false, true],
                false => [true, false],
            };
            for armed in order {
                let (mut app, id) = scripted_app(&page);
                app.update(Msg::RunScripts { id });
                if !armed {
                    without_insert_detection(&mut app);
                }
                let started = Instant::now();
                app.update(Msg::Timer {
                    page: id,
                    id: TimerId(1),
                });
                let elapsed = started.elapsed();
                if round > 0 {
                    if armed { &mut on } else { &mut off }.push(elapsed);
                }
            }
        }
        eprintln!(
            "M11.5 tick of {APPENDS} appendChild, mean of {ROUNDS} interleaved rounds:\n  \
             without detection {}  ->  with {}",
            summarize(&off),
            summarize(&on),
        );
    }

    // ---- F2: the computed-styles surface (M4.5) ---------------------------

    #[test]
    fn a_page_that_hides_itself_still_reaches_the_reader() {
        // End to end: the page says `body { display: none }` and expects a
        // script to undo it. There is no script engine until M10, so the
        // choice is the article or a blank screen (layout::layout_readable).
        // The statusline says so, because showing content a page hid is not
        // something to do silently.
        let mut app = App::new(40, 6);
        open_page(
            &mut app,
            "<head><style>body { display: none }</style></head><body><p>rescued</p></body>",
        );
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(
            (0..5).any(|y| row_text(&frame, y).contains("rescued")),
            "a hidden page must still render: {:?}",
            row_text(&frame, 0)
        );
        assert!(
            row_text(&frame, 5).contains("[unhidden]"),
            "the statusline must say the page was unhidden: {:?}",
            row_text(&frame, 5)
        );
    }

    #[test]
    fn an_ordinary_page_is_never_tagged_as_unhidden() {
        let mut app = App::new(40, 6);
        open_page(&mut app, "<body><p>plain</p></body>");
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(!row_text(&frame, 5).contains("[unhidden]"));
    }

    #[test]
    fn f2_shows_computed_values_and_toggles_back_to_the_page() {
        let mut app = App::new(60, 6);
        let (_, _) = open_page(&mut app, "<body><p>hello</p></body>");

        assert_eq!(app.update(f2()), redraw());
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).starts_with("<html> block"),
            "got {:?}",
            row_text(&frame, 0)
        );
        assert!(
            row_text(&frame, 5).contains("[styles]"),
            "statusline must name the active surface"
        );

        assert_eq!(app.update(f2()), redraw());
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).contains("hello"), "page must come back");
        assert!(!row_text(&frame, 5).contains("[styles]"));
    }

    #[test]
    fn f3_shows_box_geometry_and_toggles_back() {
        let mut app = App::new(60, 6);
        open_page(&mut app, "<body><p>hello</p></body>");
        assert_eq!(app.update(f3()), redraw());
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        let row0 = row_text(&frame, 0);
        assert!(
            row0.contains("w=") || row0.contains('<'),
            "expected box lines, got {row0:?}"
        );
        assert!(row_text(&frame, 5).contains("[boxes]"));
        assert_eq!(app.update(f3()), redraw());
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).contains("hello"), "page must come back");
    }

    #[test]
    fn f3_rebuilds_after_resize() {
        let mut app = App::new(60, 6);
        open_page(&mut app, "<body><p>hello wide content here</p></body>");
        assert_eq!(app.update(f3()), redraw());
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        let before = row_text(&frame, 0);
        assert!(before.contains("w="), "{before:?}");
        // Narrow the frame; layout and F3 must refresh.
        assert_eq!(app.update(Msg::Resize(40, 6)), redraw());
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        let after = row_text(&frame, 0);
        assert!(after.contains("w="), "F3 blank after resize: {after:?}");
        assert!(
            row_text(&frame, 5).contains("[boxes]"),
            "still on F3 surface"
        );
        // Geometry should change with the column (or at least rebuild).
        // Width value in the first box line is content-dependent; just require
        // a non-empty rebuilt line that still looks like box output.
        assert!(!after.trim().is_empty(), "{after:?}");
    }

    #[test]
    fn f2_without_a_page_is_a_placeholder_not_a_panic() {
        let mut app = App::new(40, 6);
        assert_eq!(app.update(f2()), redraw());
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).contains("no styles yet"));
    }

    #[test]
    fn the_inspectors_are_one_surface_not_two_flags() {
        // F1 and F2 replace the page, so they cannot both own it. Opening one
        // from the other switches rather than stacking.
        let mut app = App::new(60, 6);
        open_page(&mut app, "<body><p>hello</p></body>");
        let mut frame = Frame::new(60, 6);

        app.update(f1());
        app.update(f2());
        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("[styles]"));
        assert!(!row_text(&frame, 5).contains("[dom]"));

        app.update(f1());
        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("[dom]"));
        assert!(!row_text(&frame, 5).contains("[styles]"));
    }

    #[test]
    fn the_scroll_keys_drive_whichever_surface_is_up() {
        // One binding table, one set of scroll keys (the UX charter's "do not
        // invent a second scheme"), acting on the surface in front of you.
        let mut app = App::new(60, 6);
        open_page(
            &mut app,
            "<body><div><div><div><div><p>a</p><p>b</p><p>c</p></div></div></div></div></body>",
        );
        app.update(f2());
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        let top = row_text(&frame, 0);

        assert_eq!(app.update(ch('j')), redraw());
        app.draw(&mut frame);
        assert_ne!(row_text(&frame, 0), top, "F2 must scroll");
        // ...and the page underneath did not move.
        app.update(f2());
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).contains('a'));
    }

    #[test]
    fn an_arriving_stylesheet_refreshes_the_open_f2_surface() {
        // The inspector is a product surface (CLAUDE.md rule 4): if it is open
        // when the cascade changes underneath it, it must show the new values,
        // not a cached view of the old ones.
        let mut app = App::new(60, 6);
        let (id, _) = open_page(
            &mut app,
            "<head><link rel=stylesheet href='x.css'></head><body><p>hi</p></body>",
        );
        app.update(f2());
        let mut frame = Frame::new(60, 6);
        app.draw(&mut frame);
        assert!(!row_text(&frame, 3).contains("bold"), "plain to start");

        app.update(Msg::Stylesheet {
            id,
            slot: 0,
            sheet: sheet("p { font-weight: bold }"),
        });
        app.draw(&mut frame);
        let rows: Vec<String> = (0..5).map(|y| row_text(&frame, y)).collect();
        assert!(
            rows.iter().any(|r| r.contains("<p> block · bold")),
            "F2 must show the new computed values, got {rows:?}"
        );
    }

    #[test]
    fn f1_toggles_between_placeholder_and_page() {
        let mut app = App::new(40, 6);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(3));

        assert_eq!(app.update(f1()), redraw());
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).contains("no DOM yet"),
            "no parse yet → calm placeholder, got {:?}",
            row_text(&frame, 0)
        );
        assert!(
            row_text(&frame, 5).contains("[dom]"),
            "statusline must reflect the active surface"
        );

        assert_eq!(app.update(f1()), redraw());
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).starts_with("line0"),
            "toggling off must restore the page"
        );
        assert!(!row_text(&frame, 5).contains("[dom]"));
    }

    #[test]
    fn f1_renders_the_parsed_tree_as_an_indented_grid() {
        let mut app = App::new(40, 10);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(3));
        app.update(parsed(id, "<p>hi</p>"));
        app.update(f1());

        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        let expected = [
            "#document",
            "  <html>",
            "    <head>",
            "    <body>",
            "      <p>",
            "        #text \"hi\"",
        ];
        for (y, want) in expected.iter().enumerate() {
            let row = row_text(&frame, y as u16);
            assert!(row.starts_with(want), "row {y} was {row:?}, want {want:?}");
        }
    }

    #[test]
    fn inspector_scrolls_with_the_page_keys_without_touching_the_page() {
        let mut app = App::new(40, 6); // page area of 5 rows
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(50));
        // A tree taller than the page: 20 paragraphs is 40+ lines.
        let html: String = (0..20).map(|i| format!("<p>p{i}</p>")).collect();
        app.update(parsed(id, &html));
        app.update(f1());
        // The no-re-parse invariant is structural — `App` has no path to the
        // parser; a `Dom` only ever enters via `Msg::Parsed` — and the
        // cached-lines invariant is observable: the line store must not be
        // rebuilt by scrolling (a rebuild would also reset the offset, which
        // the assertions below would catch).
        let lines_before = app.dom_view.line_count();

        // `j` scrolls the tree: the first visible line moves down one.
        assert_eq!(app.update(ch('j')), redraw());
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).starts_with("  <html>"),
            "inspector must scroll, row was {:?}",
            row_text(&frame, 0)
        );
        assert_eq!(
            app.dom_view.line_count(),
            lines_before,
            "scrolling must only move the offset over the cached lines"
        );

        // The page did not move: toggling the inspector off still shows the
        // first laid-out line (the raw body was replaced by the laid-out page
        // when the parse landed).
        app.update(f1());
        app.draw(&mut frame);
        assert_eq!(
            row_text(&frame, 0).trim_end(),
            " p0",
            "the page offset must be untouched by inspector scrolling"
        );

        // And the tree kept its own offset while hidden.
        app.update(f1());
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).starts_with("  <html>"));
    }

    #[test]
    fn a_new_loaded_drops_the_stale_parse_timing_with_the_tree() {
        let mut app = App::new(40, 10);
        let id = app.start_fetch("http://a/".into());
        load(&mut app, id, body(3));
        app.update(parsed(id, "<p>a</p>"));
        assert!(app.timings().parse.is_some());

        // Page B's body lands: fetch is now B's, so A's parse must not sit
        // next to it — a table mixing stages from two runs would lie.
        let id2 = app.start_fetch("http://b/".into());
        load(&mut app, id2, body(3));
        assert_eq!(app.timings().parse, None);
        assert!(
            !app.timings().rows().iter().any(|r| r.starts_with("parse")),
            "rows were {:?}",
            app.timings().rows()
        );

        app.update(parsed(id2, "<p>b</p>"));
        assert!(app.timings().parse.is_some());
    }

    #[test]
    fn parsed_defers_line_building_until_f1_opens() {
        let mut app = App::new(40, 10);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, body(3));
        // F1 closed: Parsed stores the tree but builds no lines — a
        // Wikipedia-sized build (~15 ms) must not ride the load path.
        app.update(parsed(id, "<p>hi</p>"));
        assert_eq!(
            app.dom_view.line_count(),
            0,
            "line building must wait for the surface to open"
        );

        app.update(f1());
        assert!(
            app.dom_view.line_count() > 0,
            "toggling on builds the lines"
        );

        // Off and on again: the cached lines (and the offset with them, see
        // the scroll test) survive — no rebuild per toggle.
        app.update(ch('j'));
        let offset = app.dom_view.offset();
        app.update(f1());
        app.update(f1());
        assert_eq!(app.dom_view.offset(), offset, "a toggle must not rebuild");
    }

    #[test]
    fn a_new_loaded_clears_the_old_tree_until_its_parse_lands() {
        let mut app = App::new(40, 10);
        let id = app.start_fetch("http://a/".into());
        load(&mut app, id, body(3));
        app.update(parsed(id, "<p>old</p>"));
        app.update(f1());

        let id2 = app.start_fetch("http://b/".into());
        load(&mut app, id2, body(3));
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).contains("no DOM yet"),
            "the old page's tree must not pose as the new page's"
        );

        app.update(parsed(id2, "<p>new</p>"));
        app.draw(&mut frame);
        assert!(row_text(&frame, 0).starts_with("#document"));
    }

    // ---- M3.2 laid-out page: column, styles, relayout points --------------

    /// A page that has been fetched and parsed — the state where the viewport
    /// holds laid-out lines rather than raw body text.
    fn page(w: u16, h: u16, html: &str) -> App {
        let mut app = App::new(w, h);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, html.as_bytes().to_vec());
        app.update(parsed(id, html));
        app
    }

    #[test]
    fn the_column_caps_at_ninety_cells_and_centers_what_is_left() {
        // UX §3.5: past ~90 cells prose stops being readable, so a wide
        // terminal gets a centered column, not edge-to-edge text.
        let c = |w| (column(w).left, column(w).width);
        assert_eq!(c(200), (55, 90));
        assert_eq!(c(100), (5, 90));
        // 92 is the widest frame the cap doesn't bite on: gutters only.
        assert_eq!(c(92), (1, 90));
        assert_eq!(c(40), (1, 38));
        // Degenerate frames still leave layout a cell to write in.
        assert_eq!(c(1), (0, 1));
        assert_eq!(c(0), (0, 1));
    }

    #[test]
    fn a_wide_frame_paints_the_column_centered_with_its_styles() {
        let app = page(100, 10, "<h1>Title</h1><p>see <a href=x>docs</a></p>");
        let mut frame = Frame::new(100, 10);
        app.draw(&mut frame);

        // 90-cell column in a 100-cell frame: 5 cells of gutter each side.
        assert_eq!(row_text(&frame, 0).trim_end(), "     Title");
        assert_eq!(row_text(&frame, 1).trim_end(), "", "blank between blocks");
        assert_eq!(row_text(&frame, 2).trim_end(), "     see docs");
        // The gutter is untouched, not painted with spaces in some style.
        assert_eq!(frame.get(4, 0), Cell::default());

        // Attributes, not just characters: the heading is bold…
        assert!(frame.get(5, 0).attrs.contains(Attrs::BOLD), "heading bold");
        assert!(
            !frame.get(5, 2).attrs.contains(Attrs::BOLD),
            "body not bold"
        );
        // …and the link is underlined and colored, starting after "see ".
        let link = frame.get(9, 2);
        assert_eq!(link.ch, 'd');
        assert!(link.attrs.contains(Attrs::UNDERLINE), "link underlined");
        // The UA sheet's link colour, #5c5cff — the RGB of the ANSI 12 M3
        // hardcoded, so the pixels are the same and the source is the cascade.
        assert_eq!(link.fg, Color::Rgb(0x5c, 0x5c, 0xff), "link colored");
        // The space before it belongs to no link: no stray underlined cell.
        assert!(!frame.get(8, 2).attrs.contains(Attrs::UNDERLINE));
    }

    #[test]
    fn a_narrow_frame_uses_the_full_width_without_centering() {
        // Under the cap there is nothing to center: one gutter cell each side
        // and the rest is text.
        let app = page(40, 10, &format!("<p>{}</p>", "wordy ".repeat(20)));
        let mut frame = Frame::new(40, 10);
        app.draw(&mut frame);
        let row = row_text(&frame, 0);
        assert!(row.starts_with(' '), "row was {row:?}");
        assert!(row.starts_with(" wordy"), "row was {row:?}");
        // The column is 38 cells wide at x=1, so the last frame cell is gutter.
        assert_eq!(
            frame.get(39, 0),
            Cell::default(),
            "text ran into the right gutter"
        );
    }

    #[test]
    fn pre_clips_at_the_right_edge_instead_of_wrapping() {
        // <pre> does not wrap (PLAN.md M3), so the overflow is the painter's to
        // drop — no second row, no horizontal scroll until M7.
        let app = page(20, 6, &format!("<pre>{}</pre>", "x".repeat(60)));
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);
        assert_eq!(row_text(&frame, 0), format!(" {}", "x".repeat(19)));
        assert_eq!(row_text(&frame, 1).trim_end(), "", "clipped, not wrapped");
    }

    #[test]
    fn resize_relayouts_once_and_scrolling_never_does() {
        let html: String = (0..50).map(|i| format!("<p>p{i}</p>")).collect();
        let mut app = page(80, 10, &html);
        assert_eq!(app.layouts, 1, "the parse lays the page out once");

        // A burst of scroll keys moves the offset over the cached lines.
        for _ in 0..50 {
            app.update(ch('j'));
        }
        assert!(app.viewport.offset() > 0, "the burst must actually scroll");
        assert_eq!(app.layouts, 1, "scrolling must never relayout");

        app.update(Msg::Resize(60, 10));
        assert_eq!(app.layouts, 2, "a resize relayouts exactly once");
        // …and the reader keeps their place instead of being thrown to the top.
        assert!(app.viewport.offset() > 0, "resize reset the scroll");
    }

    #[test]
    fn scrolling_fixed_and_sticky_output_uses_the_cached_layout() {
        let body: String = (0..30).map(|i| format!("<p>p{i}</p>")).collect();
        let html = format!(
            "<style>.tools{{position:fixed;top:0}} .chapter{{position:sticky;top:0}} p{{margin:0}}</style>\
             <div class=tools>tools</div><div class=chapter>chapter</div>{body}"
        );
        let mut app = page(80, 8, &html);
        let before = stages(&app);
        for _ in 0..10 {
            app.update(ch('j'));
        }
        assert!(app.viewport.offset() > 0, "positioned page did not scroll");
        assert_eq!(
            stages(&app).0,
            before.0,
            "scrolling fixed/sticky output restyled"
        );
        assert_eq!(
            stages(&app).1,
            before.1,
            "scrolling fixed/sticky output relaid out"
        );
    }

    #[test]
    fn resizing_recomputes_fixed_viewport_percentages_once() {
        let mut app = page(
            80,
            8,
            "<style>.tools { position: fixed; top: 50% }</style><div class=tools>fixed</div>",
        );
        let fixed_y = |app: &App| {
            app.layout_tree
                .as_ref()
                .unwrap()
                .boxes
                .iter()
                .find(|b| b.fixed_viewport && b.kind == layout::BoxKind::Block)
                .unwrap()
                .dimensions
                .content
                .y
        };
        let before = fixed_y(&app);
        app.update(Msg::Resize(80, 12));
        assert_eq!(app.layouts, 2, "resize must relayout exactly once");
        assert!(
            fixed_y(&app) > before,
            "fixed percentage ignored new viewport"
        );
    }

    #[test]
    fn a_new_page_starts_at_the_top_even_after_scrolling_the_last_one() {
        // `set_lines` keeps the offset by design, so nothing in layout resets
        // it — the reset rides on the `set_content` that shows the incoming raw
        // body. This pins that load path end to end: drop it and every
        // navigation would open partway down the new page.
        let html: String = (0..50).map(|i| format!("<p>p{i}</p>")).collect();
        let mut app = page(80, 10, &html);
        for _ in 0..30 {
            app.update(ch('j'));
        }
        assert!(app.viewport.offset() > 0, "the burst must actually scroll");

        let id2 = app.start_fetch("http://b/".into());
        load(&mut app, id2, html.as_bytes().to_vec());
        assert_eq!(app.viewport.offset(), 0, "the raw body starts at the top");
        app.update(parsed(id2, &html));
        assert_eq!(app.viewport.offset(), 0, "and so does the laid-out page");
    }

    #[test]
    fn an_unparsed_body_still_renders_as_raw_text_at_the_left_edge() {
        // Between `Loaded` and `Parsed` there is no tree to lay out, so the
        // page is exactly what M1.5 drew: raw text, no column, no styles.
        let mut app = App::new(100, 10);
        let id = app.start_fetch("http://x/".into());
        load(&mut app, id, b"<p>raw</p>".to_vec());
        let mut frame = Frame::new(100, 10);
        app.draw(&mut frame);
        assert_eq!(row_text(&frame, 0).trim_end(), "<p>raw</p>");
        assert_eq!(app.layouts, 0, "nothing to lay out without a tree");
    }

    #[test]
    fn an_empty_document_draws_a_blank_page_and_a_calm_statusline() {
        let app = page(40, 6, "<html><body></body></html>");
        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        for y in 0..5 {
            assert_eq!(row_text(&frame, y).trim_end(), "", "row {y} not blank");
        }
        // No content means no scroll position to report — not "0%".
        assert!(!row_text(&frame, 5).contains('%'));
    }

    #[test]
    fn the_layout_row_joins_the_timing_table_and_leaves_with_a_new_body() {
        let mut app = page(40, 10, "<p>hi</p>");
        assert!(app.timings().layout.is_some());
        let rows = app.timings().rows();
        assert!(
            rows.iter().any(|r| r.starts_with("layout")),
            "rows were {rows:?}"
        );

        // A new body arrives: its fetch time must not sit beside the previous
        // page's layout time (the M2.3 no-stage-mixing rule).
        let id2 = app.start_fetch("http://b/".into());
        load(&mut app, id2, b"<p>b</p>".to_vec());
        assert_eq!(app.timings().layout, None);
    }

    #[test]
    fn overlay_stays_visible_when_the_url_bar_opens() {
        let mut app = timed_app(40, 6);
        app.update(f4());
        app.update(ch('o'));
        // In UrlInput F4 is unbound: ignored, and it types nothing.
        assert_eq!(app.update(f4()), Effect::default());

        let mut frame = Frame::new(40, 6);
        app.draw(&mut frame);
        assert!(
            row_text(&frame, 0).ends_with("cache network"),
            "overlay must stay up under the URL bar"
        );
        let bottom = row_text(&frame, 5);
        assert!(bottom.contains("open:"), "row was {bottom:?}");
        assert!(!bottom.contains("open: F"), "F4 must not type");
    }

    // ---- M6 interaction ---------------------------------------------------

    fn mouse_down(col: u16, row: u16) -> Msg {
        Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn mouse_move(col: u16, row: u16) -> Msg {
        Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Find a document-space click that hits the first link, then map to frame.
    fn click_first_link(app: &App) -> Msg {
        let dom = app.dom.as_ref().unwrap();
        let tree = app.layout_tree.as_ref().unwrap();
        let link = layout::collect_links(tree, dom)
            .into_iter()
            .next()
            .expect("fixture needs a link");
        let left = column(app.size.0).left;
        mouse_down(
            (left as i32 + link.x) as u16,
            (link.y - app.viewport.offset() as i32) as u16,
        )
    }

    #[test]
    fn click_on_a_link_starts_a_fetch_for_the_resolved_url() {
        let mut app = page(80, 12, "<p>see <a href='/docs'>docs</a> here</p>");
        let effect = app.update(click_first_link(&app));
        assert!(effect.dirty);
        let (id, url) = effect.fetch.expect("click must navigate");
        // `page` loads with post-redirect URL `http://final/` (see `load`).
        assert_eq!(url.url, "http://final/docs");
        assert_eq!(id, PageId::headless(2)); // generation after the initial load
    }

    #[test]
    fn click_outside_a_link_is_not_dirty() {
        let mut app = page(80, 12, "<p>no links here at all</p>");
        let left = column(app.size.0).left;
        assert_eq!(app.update(mouse_down(left, 0)), Effect::default());
    }

    #[test]
    fn f_opens_hints_and_typing_the_label_follows() {
        let mut app = page(
            80,
            12,
            "<p><a href='/a'>alpha</a> <a href='/b'>beta</a></p>",
        );
        assert!(app.update(ch('f')).dirty);
        assert!(app.hint.is_some());
        // First visible link is labeled "a" (home-row alphabet).
        let effect = app.update(ch('a'));
        assert_eq!(
            effect.fetch.as_ref().map(|(_, r)| r.url.as_str()),
            Some("http://final/a")
        );
        assert!(app.hint.is_none());
    }

    #[test]
    fn capital_f_yanks_the_hint_url() {
        let mut app = page(80, 12, "<p><a href='/z'>zulu</a></p>");
        app.update(key(KeyCode::Char('F'), KeyModifiers::NONE));
        let effect = app.update(ch('a'));
        assert!(effect.fetch.is_none());
        assert_eq!(effect.yank.as_deref(), Some("http://final/z"));
    }

    #[test]
    fn esc_cancels_hints() {
        let mut app = page(80, 12, "<p><a href='/a'>a</a></p>");
        app.update(ch('f'));
        assert!(app.hint.is_some());
        assert!(app.update(key(KeyCode::Esc, KeyModifiers::NONE)).dirty);
        assert!(app.hint.is_none());
    }

    /// M9.3 review. The focus overlay *writes glyphs*, so walking the tree
    /// clip-blind put a clipped-away link's text back on the page the moment
    /// Tab reached it — undoing the display list's trimming from the one
    /// surface that can. Tab reaching it at all was the other half: `Enter`
    /// would follow a link nobody could see.
    #[test]
    fn tab_neither_reaches_nor_repaints_a_clipped_away_link() {
        let markup = |overflow: &str| {
            format!(
                "<body style='margin:0'><div style='margin:0;max-height:1em;overflow:{overflow}'>\
                 <p style='margin:0'><a href='/shown'>shown</a></p>\
                 <p style='margin:0'><a href='/gone'>SECRET</a></p></div></body>"
            )
        };
        // Control: with nothing clipping, the second row is a real, reachable
        // link — so the assertions below are about the clip, not the markup.
        let mut app = page(40, 8, &markup("visible"));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(visible_text(&app).contains("SECRET"));
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE))
                .fetch
                .as_ref()
                .map(|(_, r)| r.url.as_str()),
            Some("http://final/gone")
        );

        // Clipped: the row paints nothing before or after Tab, and Tab has
        // exactly one link to cycle through however often it is pressed.
        let mut app = page(40, 8, &markup("hidden"));
        assert!(!visible_text(&app).contains("SECRET"));
        for _ in 0..3 {
            app.update(key(KeyCode::Tab, KeyModifiers::NONE));
            assert!(
                !visible_text(&app).contains("SECRET"),
                "the focus overlay repainted clipped-away text"
            );
        }
        assert_eq!(
            app.update(key(KeyCode::Enter, KeyModifiers::NONE))
                .fetch
                .as_ref()
                .map(|(_, r)| r.url.as_str()),
            Some("http://final/shown")
        );
    }

    #[test]
    fn tab_cycles_links_and_enter_follows() {
        let mut app = page(80, 12, "<p><a href='/1'>one</a> <a href='/2'>two</a></p>");
        assert!(app.update(key(KeyCode::Tab, KeyModifiers::NONE)).dirty);
        assert!(app.focus.is_some());
        // Second Tab → second link.
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            effect.fetch.as_ref().map(|(_, r)| r.url.as_str()),
            Some("http://final/2")
        );
    }

    #[test]
    fn history_back_restores_scroll_after_layout() {
        let mut app = page(
            40,
            8,
            "<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p><p>i</p><p>j</p>",
        );
        // Scroll down on page A.
        app.update(ch('j'));
        app.update(ch('j'));
        let scroll_a = app.viewport.offset();
        assert!(scroll_a > 0);

        // Navigate to B via URL bar.
        app.update(ch('o'));
        for c in "http://y/".chars() {
            app.update(ch(c));
        }
        let effect = app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let (id, url) = effect.fetch.unwrap();
        assert_eq!(url.url, "http://y/");
        app.update(Msg::Loaded {
            id,
            url: url.url.clone(),
            status: 200,
            body: b"<p>page b</p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(Msg::Parsed {
            id,
            dom: crate::html::parse("<p>page b</p>"),
            elapsed: Duration::ZERO,
        });

        // Back to A (`http://final/` — the Loaded URL of the first page).
        let effect = app.update(key(KeyCode::Char('H'), KeyModifiers::NONE));
        let (id, url) = effect.fetch.unwrap();
        assert_eq!(url.url, "http://final/");
        app.update(Msg::Loaded {
            id,
            url: url.url.clone(),
            status: 200,
            body:
                b"<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p><p>i</p><p>j</p>"
                    .to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(Msg::Parsed {
            id,
            dom: crate::html::parse(
                "<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p><p>i</p><p>j</p>",
            ),
            elapsed: Duration::ZERO,
        });
        assert_eq!(app.viewport.offset(), scroll_a);
    }

    #[test]
    fn reload_and_edit_url_and_yy() {
        let mut app = page(80, 10, "<p>hi</p>");
        let effect = app.update(ch('r'));
        assert_eq!(
            effect.fetch.as_ref().map(|(_, r)| r.url.as_str()),
            Some("http://final/")
        );

        app.update(key(KeyCode::Char('O'), KeyModifiers::NONE));
        match &app.mode {
            Mode::UrlInput { buffer } => assert_eq!(buffer, "http://final/"),
            _ => panic!("O must open the URL bar"),
        }
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));

        app.update(ch('y')); // pending
        let effect = app.update(ch('y'));
        assert_eq!(effect.yank.as_deref(), Some("http://final/"));
    }

    #[test]
    fn hover_restyles_without_relayout() {
        let mut app = page(80, 12, "<p><a href='/h'>hover me</a></p>");
        let layouts_before = app.layouts;
        let color_before = computed_color(&app, "a");

        let dom = app.dom.as_ref().unwrap();
        let tree = app.layout_tree.as_ref().unwrap();
        let link = layout::collect_links(tree, dom).into_iter().next().unwrap();
        let left = column(app.size.0).left;
        let col = (left as i32 + link.x) as u16;
        let row = (link.y - app.viewport.offset() as i32) as u16;

        let effect = app.update(mouse_move(col, row));
        assert!(effect.dirty);
        assert_eq!(app.layouts, layouts_before, "hover must not relayout");
        assert!(app.hover.is_some());
        let color_after = computed_color(&app, "a");
        assert_ne!(
            color_before, color_after,
            "a:hover should change the link colour"
        );

        // Same target again → not dirty.
        assert_eq!(app.update(mouse_move(col, row)), Effect::default());

        // Move off the page area → clear hover.
        let effect = app.update(mouse_move(0, 0));
        // May or may not hit a node at (0,0); if hover clears, layouts still
        // must not increase.
        assert_eq!(app.layouts, layouts_before);
        let _ = effect;
    }

    #[test]
    fn visited_links_match_after_a_successful_load() {
        // Visit A, then open B which links back to A — cascade must paint
        // :visited, not just record membership in the set.
        let mut app = page(80, 10, "<p><a href='http://visited.test/'>v</a></p>");
        let effect = app.update(click_first_link(&app));
        let (id, url) = effect.fetch.unwrap();
        assert_eq!(url.url, "http://visited.test/");
        app.update(Msg::Loaded {
            id,
            url: url.url.clone(),
            status: 200,
            body: b"<p>there</p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        assert!(app.visited.contains("http://visited.test/"));

        // Page B with a link to the visited URL.
        let id2 = app.start_fetch("http://final/b".into());
        app.update(Msg::Loaded {
            id: id2,
            url: "http://final/b".into(),
            status: 200,
            body: b"<p><a href='http://visited.test/'>back</a></p>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(Msg::Parsed {
            id: id2,
            dom: crate::html::parse("<p><a href='http://visited.test/'>back</a></p>"),
            elapsed: Duration::ZERO,
        });
        // UA a:visited is #af5fff; a:link is #5c5cff.
        let color = computed_color(&app, "a");
        assert_eq!(
            color,
            crate::style::values::ColorValue::Rgb(0xaf, 0x5f, 0xff),
            "visited link must take a:visited colour, got {color:?}"
        );
    }

    #[test]
    fn quit_works_while_hints_are_open() {
        let mut app = page(80, 12, "<p><a href='/a'>a</a></p>");
        app.update(ch('f'));
        assert!(app.hint.is_some());
        let effect = app.update(ch('q'));
        assert!(effect.quit);
        // Ctrl-c too.
        let mut app = page(80, 12, "<p><a href='/a'>a</a></p>");
        app.update(ch('f'));
        let effect = app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(effect.quit);
    }

    #[test]
    fn pending_scroll_survives_resize_while_loading() {
        // History restore must not be consumed by a resize of the *old* page.
        let mut app = page(
            40,
            8,
            "<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p><p>i</p><p>j</p>",
        );
        app.update(ch('j'));
        app.update(ch('j'));
        let scroll_a = app.viewport.offset();
        let effect = app.navigate_restore("http://final/restored".into(), scroll_a);
        let (id, _) = effect.fetch.unwrap();
        // Still on the old DOM (Loading). Resize must not eat pending_scroll.
        app.update(Msg::Resize(50, 8));
        assert!(
            app.pending_scroll.is_some(),
            "resize during Loading must not consume history restore"
        );
        // Now land the restored page.
        app.update(Msg::Loaded {
            id,
            url: "http://final/restored".into(),
            status: 200,
            body:
                b"<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p><p>i</p><p>j</p>"
                    .to_vec(),
            elapsed: Duration::ZERO,
            content_type: None,
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        app.update(Msg::Parsed {
            id,
            dom: crate::html::parse(
                "<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p><p>i</p><p>j</p>",
            ),
            elapsed: Duration::ZERO,
        });
        assert_eq!(app.viewport.offset(), scroll_a);
        assert!(app.pending_scroll.is_none());
    }

    #[test]
    fn yanked_status_clears_on_the_next_action() {
        let mut app = page(80, 10, "<p>hi</p>");
        app.update(ch('y'));
        app.update(ch('y'));
        assert_eq!(app.status_msg.as_deref(), Some("yanked"));
        app.update(ch('j')); // scroll — must clear the flash
        assert!(app.status_msg.is_none());
    }

    #[test]
    fn same_document_fragment_does_not_fetch() {
        let mut app = page(80, 10, "<p><a href='#section'>jump</a></p>");
        let effect = app.update(click_first_link(&app));
        assert!(effect.fetch.is_none(), "pure fragment must not navigate");
        assert!(!effect.dirty || effect.fetch.is_none());
    }

    #[test]
    fn invalid_hint_key_keeps_the_session() {
        let mut app = page(80, 12, "<p><a href='/a'>alpha</a></p>");
        app.update(ch('f'));
        // First link is "a". Typing "z" matches nothing — session stays open.
        app.update(ch('z'));
        assert!(app.hint.is_some());
        assert_eq!(app.hint.as_ref().map(|h| h.buffer.as_str()), Some(""));
    }

    // ---- M7 polish --------------------------------------------------------

    #[test]
    fn net_error_renders_a_retry_page() {
        let mut app = App::new(60, 12);
        let id = app.start_fetch("http://x.test/gone".into());
        assert!(
            app.update(Msg::NetError {
                id,
                url: "http://x.test/gone".into(),
                reason: "connection refused".into(),
            })
            .dirty
        );
        assert!(app.dom.is_none());
        let mut frame = Frame::new(60, 12);
        app.draw(&mut frame);
        let page: String = (0..11)
            .map(|y| row_text(&frame, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(page.contains("http://x.test/gone"), "{page}");
        assert!(page.contains("connection refused"), "{page}");
        assert!(page.contains("Press r to retry."), "{page}");
        // Reload still knows the URL.
        let effect = app.update(ch('r'));
        assert_eq!(
            effect.fetch.as_ref().map(|(_, r)| r.url.as_str()),
            Some("http://x.test/gone")
        );
    }

    #[test]
    fn http_404_is_an_error_page_not_a_document() {
        let mut app = App::new(60, 12);
        let id = app.start_fetch("http://x/".into());
        app.update(Msg::Loaded {
            id,
            url: "http://x/".into(),
            status: 404,
            body: b"<html><body>server 404 page</body></html>".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("text/html".into()),
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        assert!(app.dom.is_none());
        // A late Parsed must not clobber the error page.
        app.update(Msg::Parsed {
            id,
            dom: crate::html::parse("<html><body>server 404 page</body></html>"),
            elapsed: Duration::ZERO,
        });
        assert!(app.dom.is_none());
        let mut frame = Frame::new(60, 12);
        app.draw(&mut frame);
        let page: String = (0..11)
            .map(|y| row_text(&frame, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(page.contains("HTTP 404"), "{page}");
        assert!(page.contains("Press r to retry."), "{page}");
        assert!(!page.contains("server 404 page"), "{page}");
    }

    #[test]
    fn unsupported_content_type_is_an_error_page() {
        let mut app = App::new(60, 12);
        let id = app.start_fetch("http://x/pic.png".into());
        app.update(Msg::Loaded {
            id,
            url: "http://x/pic.png".into(),
            status: 200,
            body: b"\x89PNG".to_vec(),
            elapsed: Duration::ZERO,
            content_type: Some("image/png".into()),
            set_cookie: Vec::new(),
            metadata: Default::default(),
        });
        assert!(app.dom.is_none());
        let mut frame = Frame::new(60, 12);
        app.draw(&mut frame);
        let page: String = (0..11)
            .map(|y| row_text(&frame, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(page.contains("unsupported content-type"), "{page}");
        assert!(page.contains("image/png"), "{page}");
    }

    #[test]
    fn search_finds_highlights_and_steps() {
        let mut app = page(80, 12, "<p>alpha</p><p>beta alpha</p><p>gamma</p>");
        let layouts_before = app.layouts;
        assert!(app.update(ch('/')).dirty);
        match &app.mode {
            Mode::SearchInput { .. } => {}
            _ => panic!("/ must open search"),
        }
        for c in "alpha".chars() {
            app.update(ch(c));
        }
        assert!(app.update(key(KeyCode::Enter, KeyModifiers::NONE)).dirty);
        let session = app.search.as_ref().expect("search session");
        let n = session.matches.len();
        assert!(n >= 2, "{:?}", session.matches);
        assert_eq!(session.current, 0);

        let mut frame = Frame::new(80, 12);
        app.draw(&mut frame);
        // At least one reversed cell on the page (highlight), not the status row.
        let mut reversed_cells = 0;
        for y in 0..11u16 {
            for x in 0..80u16 {
                if frame.get(x, y).attrs.contains(Attrs::REVERSE) {
                    reversed_cells += 1;
                }
            }
        }
        assert!(reversed_cells > 0, "expected search highlights");

        // Status middle is "1/N" — must not rely on the URL containing '/'.
        let row = row_text(&frame, 11);
        assert!(
            row.contains(&format!("1/{n}")),
            "status should show 1/{n}, row was {row:?}"
        );

        app.update(ch('n'));
        assert_eq!(app.search.as_ref().unwrap().current, 1);
        let mut frame = Frame::new(80, 12);
        app.draw(&mut frame);
        let row = row_text(&frame, 11);
        assert!(
            row.contains(&format!("2/{n}")),
            "status should show 2/{n}, row was {row:?}"
        );

        app.update(key(KeyCode::Char('N'), KeyModifiers::NONE));
        assert_eq!(app.search.as_ref().unwrap().current, 0);
        // Wrap backward from 0 → last.
        app.update(key(KeyCode::Char('N'), KeyModifiers::NONE));
        assert_eq!(app.search.as_ref().unwrap().current, n - 1);
        assert_eq!(app.layouts, layouts_before, "search must not relayout");
    }

    #[test]
    fn search_esc_cancels_without_a_session() {
        let mut app = page(80, 10, "<p>hello</p>");
        app.update(ch('/'));
        app.update(ch('h'));
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Browse));
        assert!(app.search.is_none());
    }

    #[test]
    fn opening_search_clears_a_previous_session() {
        let mut app = page(80, 12, "<p>alpha beta alpha</p>");
        app.update(ch('/'));
        for c in "alpha".chars() {
            app.update(ch(c));
        }
        app.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.search.as_ref().unwrap().matches.len() >= 2);

        // `/` again: old highlights/session must go before the user types.
        app.update(ch('/'));
        assert!(app.search.is_none());
        assert!(matches!(app.mode, Mode::SearchInput { .. }));
    }

    #[test]
    fn help_toggles_from_question_mark() {
        let mut app = page(80, 16, "<p>hello world unique_token</p>");
        assert!(app.update(ch('?')).dirty);
        assert_eq!(app.surface, Surface::Help);
        let mut frame = Frame::new(80, 16);
        app.draw(&mut frame);
        let page: String = (0..15)
            .map(|y| row_text(&frame, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(page.contains("yata"), "{page}");
        assert!(page.contains("Browse") || page.contains("scroll"), "{page}");
        // Page body must not be the active content.
        assert!(
            !page.contains("unique_token"),
            "page content under help: {page}"
        );
        app.update(ch('?'));
        assert_eq!(app.surface, Surface::Page);
    }

    #[test]
    fn resize_keeps_the_top_element_anchored() {
        // Distinct markers per paragraph so we can see which one is on top.
        let html: String = (0..40)
            .map(|i| format!("<p>MARKER{i:02} content line</p>"))
            .collect();
        let mut app = page(80, 12, &html);
        // Scroll until MARKER10 is near the top.
        for _ in 0..80 {
            app.update(ch('j'));
            let mut frame = Frame::new(80, 12);
            app.draw(&mut frame);
            let top = row_text(&frame, 0);
            if top.contains("MARKER10") {
                break;
            }
        }
        let mut frame = Frame::new(80, 12);
        app.draw(&mut frame);
        let before = row_text(&frame, 0);
        assert!(
            before.contains("MARKER"),
            "expected a marker on the top row before resize, got {before:?}"
        );
        let marker = before
            .split_whitespace()
            .find(|w| w.starts_with("MARKER"))
            .unwrap()
            .to_string();

        let layouts_before = app.layouts;
        app.update(Msg::Resize(50, 12));
        assert_eq!(app.layouts, layouts_before + 1);
        let mut frame = Frame::new(50, 12);
        app.draw(&mut frame);
        let after = row_text(&frame, 0);
        assert!(
            after.contains(&marker),
            "anchor lost: before top had {marker}, after {after:?}"
        );
    }

    #[test]
    fn resize_mid_paragraph_does_not_snap_to_the_first_line() {
        // One long paragraph wraps to many lines. Scroll into the middle of it,
        // resize, and the top row must still show a mid-paragraph slice — not
        // jump back to the opening words (the old first_y bug).
        let words: String = (0..80).map(|i| format!("w{i:02} ")).collect();
        let html = format!("<p>{words}</p><p>after</p>");
        let mut app = page(40, 8, &html);
        // Scroll a few lines into the wrapped paragraph.
        for _ in 0..4 {
            app.update(ch('j'));
        }
        let mut frame = Frame::new(40, 8);
        app.draw(&mut frame);
        let before = row_text(&frame, 0);
        assert!(
            !before.contains("w00"),
            "test setup: should be mid-paragraph, top was {before:?}"
        );
        assert!(
            before.contains('w'),
            "expected wrapped words on top, got {before:?}"
        );
        // Capture a distinctive token from the top row.
        let token = before
            .split_whitespace()
            .find(|t| t.starts_with('w') && t.len() >= 3)
            .expect("a wNN token on the top row")
            .to_string();

        app.update(Msg::Resize(36, 8));
        let mut frame = Frame::new(36, 8);
        app.draw(&mut frame);
        let after = row_text(&frame, 0);
        // Either the same token or a near neighbour still mid-paragraph —
        // not the paragraph start.
        assert!(
            !after.contains("w00") || after.contains(&token),
            "snapped to start or lost place: before had {token}, after {after:?}"
        );
        // Stronger: the top row should still be paragraph text, not blank.
        assert!(
            after.contains('w'),
            "lost paragraph content after resize: {after:?}"
        );
    }

    // ---- images (M8) ------------------------------------------------------

    #[test]
    fn parse_requests_image_fetches_for_unresolved_srcs() {
        let mut app = App::new(80, 20);
        let (id, effect) = open_page(
            &mut app,
            r#"<p>hi</p><img src="pic.png" width="80" height="48" alt="a">
               <img src="https://cdn.example/x.jpg">"#,
        );
        assert!(
            effect
                .images
                .iter()
                .any(|(i, r)| *i == id && r.url == "http://site.test/dir/pic.png"),
            "{:?}",
            effect.images
        );
        assert!(
            effect
                .images
                .iter()
                .any(|(_, r)| r.url == "https://cdn.example/x.jpg"),
            "{:?}",
            effect.images
        );
        // Layout reserved firm size for the sized image.
        let tree = app.layout_tree.as_ref().unwrap();
        let mut found = false;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Image && b.image_size_firm {
                assert_eq!(b.dimensions.content.width, 10); // 80px / 8
                assert_eq!(b.dimensions.content.height, 3); // 48px / 16
                found = true;
            }
        });
        assert!(found, "expected a firm image box");
    }

    #[test]
    fn firm_size_image_arrival_repaints_without_relayout() {
        let mut app = App::new(80, 20);
        let (id, _) = open_page(
            &mut app,
            r#"<img src="pic.png" width="16" height="16" alt="x">"#,
        );
        let layouts_before = app.layouts;
        let decoded = crate::image::DecodedImage::new(
            2,
            2,
            vec![
                255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
            ],
        );
        assert!(
            app.update(Msg::Image {
                id,
                url: "http://site.test/dir/pic.png".into(),
                result: Ok(decoded),
            })
            .dirty
        );
        assert_eq!(
            app.layouts, layouts_before,
            "firm attrs must not force relayout"
        );
        assert!(app.images.cache_contains("http://site.test/dir/pic.png"));
        // Display list should have half-block image command with pixels.
        assert!(
            app.display_list.commands.iter().any(|c| matches!(
                c,
                crate::paint::DisplayCommand::Image {
                    pixels: Some(_),
                    ..
                }
            )),
            "expected painted image with pixels"
        );
    }

    #[test]
    fn soft_size_image_arrival_relayouts() {
        let mut app = App::new(80, 20);
        let (id, _) = open_page(&mut app, r#"<img src="big.png" alt="x">"#);
        let layouts_before = app.layouts;
        // 160×80 px → 20×5 cells
        let mut rgba = vec![0u8; 160 * 80 * 4];
        for px in rgba.chunks_mut(4) {
            px.copy_from_slice(&[0, 255, 0, 255]);
        }
        let decoded = crate::image::DecodedImage::new(160, 80, rgba);
        app.update(Msg::Image {
            id,
            url: "http://site.test/dir/big.png".into(),
            result: Ok(decoded),
        });
        assert!(
            app.layouts > layouts_before,
            "unknown size must relayout when decode lands"
        );
        let tree = app.layout_tree.as_ref().unwrap();
        let mut h = 0;
        tree.walk(tree.root, &mut |_, b| {
            if b.kind == BoxKind::Image {
                h = b.dimensions.content.height;
            }
        });
        assert_eq!(h, 5, "decoded height should be 5 cells");
    }

    #[test]
    fn failed_image_is_soft_not_an_error_page() {
        let mut app = App::new(80, 20);
        let (id, _) = open_page(&mut app, r#"<p>still here</p><img src="nope.png">"#);
        app.update(Msg::Image {
            id,
            url: "http://site.test/dir/nope.png".into(),
            result: Err("HTTP 404".into()),
        });
        assert!(app.dom.is_some());
        let mut frame = Frame::new(80, 20);
        app.draw(&mut frame);
        let page: String = (0..19)
            .map(|y| row_text(&frame, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(page.contains("still here"), "{page}");
        assert!(!page.contains("Press r to retry"), "{page}");
    }

    #[test]
    fn cached_image_skips_network_on_second_page() {
        let mut app = App::new(80, 20);
        let (id, effect) = open_page(
            &mut app,
            r#"<img src="https://cdn.example/a.png" width="8" height="16">"#,
        );
        assert_eq!(effect.images.len(), 1);
        app.update(Msg::Image {
            id,
            url: "https://cdn.example/a.png".into(),
            result: Ok(crate::image::DecodedImage::new(1, 1, vec![1, 2, 3, 255])),
        });
        // Navigate to another page that reuses the same absolute URL.
        let (_id2, effect2) = open_page(
            &mut app,
            r#"<img src="https://cdn.example/a.png" width="8" height="16">"#,
        );
        assert!(
            effect2.images.is_empty(),
            "cache hit must not re-fetch: {:?}",
            effect2.images
        );
    }

    #[test]
    fn scroll_with_images_does_not_relayout() {
        let mut app = App::new(80, 12);
        let (id, _) = open_page(
            &mut app,
            &format!(
                "{}{}",
                r#"<img src="x.png" width="80" height="160" alt="big">"#,
                (0..30)
                    .map(|i| format!("<p>line{i}</p>"))
                    .collect::<String>()
            ),
        );
        app.update(Msg::Image {
            id,
            url: "http://site.test/dir/x.png".into(),
            result: Ok(crate::image::DecodedImage::new(4, 4, vec![255; 4 * 4 * 4])),
        });
        let layouts_before = app.layouts;
        for _ in 0..20 {
            app.update(ch('j'));
        }
        assert_eq!(app.layouts, layouts_before, "scroll must never relayout");
    }

    #[test]
    fn kitty_frame_is_noop_on_identical_view() {
        let mut app = App::with_caps(80, 24, true);
        let (id, _) = open_page(
            &mut app,
            r#"<img src="pic.png" width="16" height="16" alt="x">"#,
        );
        app.update(Msg::Image {
            id,
            url: "http://site.test/dir/pic.png".into(),
            result: Ok(crate::image::DecodedImage::new(
                2,
                2,
                vec![
                    255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
                ],
            )),
        });
        let first = app.kitty_frame();
        assert!(first.is_some(), "first present should emit Kitty");
        // Same scroll / size / display list → no second write (M8 scroll path).
        assert!(
            app.kitty_frame().is_none(),
            "identical view must not retransmit"
        );
    }

    /// M9.12: every interaction surface, over a flex layout.
    ///
    /// Each of these worked before this module existed, and that is the point:
    /// they worked because hit-testing, hints and search all read the layout
    /// tree rather than the source order, and nothing in M9 was allowed to
    /// make them read something else. Before flex, a link's document `x` was
    /// its indentation and every test could get away with x=0 — a link two
    /// columns to the right is the first thing that tells the two apart.
    ///
    /// The clipped half is the one that has already broken once: M9.3's review
    /// found `Tab` parking on links inside a collapsed menu, from a list that
    /// was built out of the DOM instead of out of the boxes.
    mod flex_interaction {
        use super::*;

        /// A sidebar beside a content column, plus a clipped box whose second
        /// link is cut away. Every length is a whole number of cells.
        const PAGE: &str = r#"<style>
body { margin: 0 } div, p { margin: 0 }
.row { display: flex }
.side { flex: 0 0 96px }
.content { flex: 1 }
.clip { max-height: 16px; overflow: hidden }
</style>
<div class="row">
  <div class="side"><a href="/one">one</a></div>
  <div class="content"><a href="/two">two</a> tail words that carry on far enough to wrap onto a second line</div>
</div>
<div class="clip"><p><a href="/shown">shown</a></p><p><a href="/gone">gone</a></p></div>"#;

        /// The links the layout tree says are on the page, by href.
        fn links(app: &App) -> Vec<(String, i32, i32)> {
            let dom = app.dom.as_ref().unwrap();
            let tree = app.layout_tree.as_ref().unwrap();
            layout::collect_links(tree, dom)
                .into_iter()
                .map(|l| (l.href, l.x, l.y))
                .collect()
        }

        #[test]
        fn a_link_in_the_second_flex_item_is_where_the_layout_says() {
            let app = page(60, 12, PAGE);
            let found = links(&app);
            let two = found
                .iter()
                .find(|(h, _, _)| h == "/two")
                .expect("content link missing");
            // `flex: 0 0 96px` is 12 cells of sidebar, so the content column
            // starts at 12 — the value a pre-flex engine would have called 0.
            assert_eq!((two.1, two.2), (12, 0), "{found:?}");
        }

        #[test]
        fn clicking_a_link_beside_another_flex_item_follows_it() {
            let mut app = page(60, 12, PAGE);
            let found = links(&app);
            let (_, x, y) = found.iter().find(|(h, _, _)| h == "/two").unwrap();
            let left = column(app.size.0).left;
            let effect = app.update(mouse_down((left as i32 + x) as u16, *y as u16));
            assert_eq!(
                effect.fetch.as_ref().map(|(_, r)| r.url.as_str()),
                Some("http://final/two"),
                "click at ({x}, {y}) missed the content column's link"
            );
        }

        #[test]
        fn hints_reach_both_columns_and_skip_the_clipped_link() {
            let mut app = page(60, 12, PAGE);
            assert!(app.update(ch('f')).dirty);
            let session = app.hint.as_ref().expect("hints must open");
            let hrefs: Vec<&str> = session
                .labels
                .iter()
                .map(|(_, l)| l.href.as_str())
                .collect();
            assert!(hrefs.contains(&"/one"), "{hrefs:?}");
            assert!(hrefs.contains(&"/two"), "{hrefs:?}");
            assert!(hrefs.contains(&"/shown"), "{hrefs:?}");
            assert!(
                !hrefs.contains(&"/gone"),
                "a clipped-away link got a hint label: {hrefs:?}"
            );
        }

        #[test]
        fn a_clipped_away_link_is_not_clickable() {
            let mut app = page(60, 12, PAGE);
            // The second paragraph of the clip is at the row `max-height`
            // cut off; clicking where it would have been must hit nothing.
            let dom = app.dom.as_ref().unwrap();
            let tree = app.layout_tree.as_ref().unwrap();
            let hidden_row = layout::collect_links(tree, dom)
                .iter()
                .find(|l| l.href == "/shown")
                .unwrap()
                .y
                + 1;
            let left = column(app.size.0).left;
            let effect = app.update(mouse_down(left, hidden_row as u16));
            assert!(
                effect.fetch.is_none(),
                "a click reached a link the clip removed"
            );
        }

        #[test]
        fn hover_inside_a_flex_item_restyles_without_relayout() {
            let mut app = page(60, 12, PAGE);
            let layouts_before = app.layouts;
            let found = links(&app);
            let (_, x, y) = found.iter().find(|(h, _, _)| h == "/two").unwrap();
            let left = column(app.size.0).left;
            let effect = app.update(mouse_move((left as i32 + x) as u16, *y as u16));
            assert!(effect.dirty, "hover over a flex item did nothing");
            assert!(app.hover.is_some());
            assert_eq!(app.layouts, layouts_before, "hover must not relayout");
        }

        #[test]
        fn search_finds_a_word_on_a_wrapped_flex_items_second_line() {
            let mut app = page(60, 12, PAGE);
            let layouts_before = app.layouts;
            app.update(ch('/'));
            for c in "second".chars() {
                app.update(ch(c));
            }
            app.update(key(KeyCode::Enter, KeyModifiers::NONE));
            let session = app.search.as_ref().expect("search session");
            assert_eq!(session.matches.len(), 1, "{:?}", session.matches);
            // The match is on the content column's second line, which only
            // exists at all because the sidebar narrowed it to 48 cells.
            let m = &session.matches[0];
            assert!(m.x >= 12, "match at x={} is not in the content column", m.x);
            assert!(m.y >= 1, "match at y={} is not on a wrapped line", m.y);
            assert_eq!(app.layouts, layouts_before, "search must not relayout");
        }

        #[test]
        fn resize_keeps_a_flex_page_anchored() {
            let mut app = page(60, 12, PAGE);
            let layouts_before = app.layouts;
            app.update(Msg::Resize(40, 12));
            assert_eq!(app.layouts, layouts_before + 1, "resize must relayout");
            let mut frame = Frame::new(40, 12);
            app.draw(&mut frame);
            // Narrower terminal, same sidebar: the content column shrinks and
            // the page still starts where it did.
            assert!(
                row_text(&frame, 0).contains("one"),
                "{:?}",
                row_text(&frame, 0)
            );
        }

        /// F3 has to explain a flex layout, which means saying that a box *is*
        /// a flex container, which way it runs, and which box swallowed the
        /// content that is missing.
        #[test]
        fn f3_labels_the_flex_container_and_the_clip() {
            let mut app = page(60, 12, PAGE);
            assert_eq!(app.update(f3()), redraw());
            let dom = app.dom.as_ref().unwrap();
            let tree = app.layout_tree.as_ref().unwrap();
            let lines = crate::browser::inspector::box_lines(dom, tree).join("\n");
            assert!(lines.contains("flex row"), "no flex label in F3:\n{lines}");
            assert!(lines.contains("overflow=hidden"), "no clip in F3:\n{lines}");
        }

        /// F1 and F4 are in the same gate and had no reason to change, so this
        /// is the test that says they did not: the DOM surface still lists the
        /// page's elements, and the timing table still has a row per stage that
        /// ran, with `layout` among them on a page whose layout is the point.
        #[test]
        fn f1_and_f4_still_work_on_a_flex_page() {
            let mut app = page(60, 12, PAGE);
            assert_eq!(app.update(f1()), redraw());
            let mut frame = Frame::new(60, 12);
            app.draw(&mut frame);
            let dom_view = (0..11)
                .map(|y| row_text(&frame, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(dom_view.contains("div"), "F1 lost the tree:\n{dom_view}");

            let rows = app.timings().rows();
            assert!(
                rows.iter().any(|r| r.starts_with("layout ")),
                "no layout row in F4: {rows:?}"
            );
            assert!(
                rows.iter()
                    .all(|r| r == "cache network" || r.ends_with(" ms")),
                "F4 row is not a duration: {rows:?}"
            );
        }

        /// F2 has to show the properties M9.5 added, or a page that flexes
        /// unexpectedly has no surface that says why.
        #[test]
        fn f2_shows_the_flex_properties() {
            let mut app = page(60, 12, PAGE);
            assert_eq!(app.update(f2()), redraw());
            let dom = app.dom.as_ref().unwrap();
            let styles = app.styles.as_ref().unwrap();
            let lines = crate::browser::inspector::style_lines(dom, styles).join("\n");
            // The container's axis rides along with the display keyword, and
            // the item properties print only where they differ from initial:
            // `flex: 0 0 96px` is a shrink and a basis, `flex: 1` is a grow.
            assert!(
                lines.contains("flex row"),
                "no flex container in F2:\n{lines}"
            );
            assert!(lines.contains("shrink 0"), "{lines}");
            assert!(lines.contains("basis 96px"), "{lines}");
            assert!(lines.contains("grow 1"), "{lines}");
        }
    }
}
