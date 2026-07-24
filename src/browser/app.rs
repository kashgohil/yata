use std::time::Duration;

use crate::browser::keys::{self, Action, Chord, Resolution};
use crate::browser::statusline;
use crate::browser::timing::{self, Timings};
use crate::browser::viewport::Viewport;
use crate::msg::Msg;
use crate::net::{self, FetchId};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

use crate::term::{Attrs, Cell, Frame, Style};

/// What one message asks of the event loop: exit, redraw, and/or start a fetch.
/// The loop ORs `dirty` across a batch, keeps the last `fetch`, and renders at
/// most once.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Effect {
    pub quit: bool,
    pub dirty: bool,
    /// A committed navigation: the id and (already normalized) URL for the loop
    /// to hand to `net::spawn_fetch`. `App` starts the fetch generation; the
    /// loop owns the worker thread. Keeps `App` pure of the network.
    pub fetch: Option<(FetchId, String)>,
}

/// Where the current fetch stands. `Loaded` retains the raw body for the
/// status-row byte count now, and for M2's parser to consume later; the
/// viewport re-wraps from its own sanitized lines, not from this.
enum Fetch {
    Idle,
    Loading {
        url: String,
        bytes_so_far: u64,
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

/// Input mode. `Browse` reads the body and scrolls; `UrlInput` is the one-line
/// URL bar with its edit buffer (cursor always at the end, no readline moves).
enum Mode {
    Browse,
    UrlInput { buffer: String },
}

/// The UI state. Pure with respect to the terminal: `update` touches only
/// state, `draw` touches only the given frame.
pub struct App {
    size: (u16, u16),
    /// Generation counter behind `FetchId`s; `start_fetch` pre-increments,
    /// so ids start at 1 and id 0 is never live.
    fetch_gen: u64,
    /// The only fetch whose messages matter; anything else is stale.
    current_fetch: Option<FetchId>,
    fetch: Fetch,
    mode: Mode,
    /// The first chord of a two-key sequence, waiting for the second. No timer
    /// backs it (idle CPU 0%): it waits indefinitely until the next key.
    pending: Option<Chord>,
    viewport: Viewport,
    /// Spinner frame index. Progress messages are its clock — there is no
    /// timer in this app — so it animates exactly while bytes are flowing.
    spinner: usize,
    /// Per-stage durations of the last completed pipeline run, fed by message
    /// data (`Loaded::elapsed`) and by the event loop (`record_frame`) — the
    /// app itself never reads the clock.
    timings: Timings,
    /// Whether the `F4` timing overlay is drawn. Independent of the mode: it
    /// stays up while the URL bar is open.
    timing_visible: bool,
}

impl App {
    pub fn new(w: u16, h: u16) -> Self {
        App {
            size: (w, h),
            fetch_gen: 0,
            current_fetch: None,
            fetch: Fetch::Idle,
            mode: Mode::Browse,
            pending: None,
            viewport: Viewport::default(),
            spinner: 0,
            timings: Timings::default(),
            timing_visible: false,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Visible body height: the frame minus the one-row bottom bar.
    fn page(&self) -> u16 {
        self.size.1.saturating_sub(1)
    }

    /// Begin a new fetch generation for `url`: prior fetches become stale and
    /// their messages will be ignored. The caller passes the returned id to
    /// `net::spawn_fetch` — `App` itself never touches the network.
    pub fn start_fetch(&mut self, url: String) -> FetchId {
        self.fetch_gen += 1;
        let id = FetchId(self.fetch_gen);
        self.current_fetch = Some(id);
        self.fetch = Fetch::Loading {
            url,
            bytes_so_far: 0,
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
            Msg::Resize(w, h) => {
                self.size = (w, h);
                // Resize is a wrap point: re-wrap at the new width, keep offset.
                self.viewport.resize(w, self.page());
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
            } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                // Any status has a body worth showing (even a 404); charset is
                // M2's problem, so lossy-decode the raw bytes for now.
                let text = String::from_utf8_lossy(&body).into_owned();
                self.viewport.set_content(&text, self.size.0, self.page());
                self.fetch = Fetch::Loaded { url, status, body };
                // Only an accepted, completed fetch records its duration.
                // `start_fetch` deliberately does not clear it and `NetError`
                // sets nothing: the timing table shows the last *completed*
                // run (PLAN.md §4) until a newer one lands.
                self.timings.fetch = Some(elapsed);
                redraw()
            }
            Msg::NetError { id, url, reason } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                self.fetch = Fetch::Failed { url, reason };
                redraw()
            }
        }
    }

    fn on_key(&mut self, ev: KeyEvent) -> Effect {
        let mode = match self.mode {
            Mode::Browse => keys::Mode::Browse,
            Mode::UrlInput { .. } => keys::Mode::UrlInput,
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
                // (CLAUDE.md): in the URL bar a printable character types into
                // the buffer. `q` is a letter here, not quit. `resolve` only
                // yields `Unbound` for Press events, so no kind check is needed.
                if let Mode::UrlInput { buffer } = &mut self.mode
                    && let KeyCode::Char(c) = ev.code
                    && !ev
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    buffer.push(c);
                    return redraw();
                }
                Effect::default()
            }
        }
    }

    fn run(&mut self, action: Action) -> Effect {
        match action {
            Action::Quit => Effect {
                quit: true,
                ..Effect::default()
            },
            Action::ScrollDown => moved(self.viewport.scroll_down()),
            Action::ScrollUp => moved(self.viewport.scroll_up()),
            Action::HalfPageDown => moved(self.viewport.half_page_down()),
            Action::HalfPageUp => moved(self.viewport.half_page_up()),
            Action::Top => moved(self.viewport.scroll_to_top()),
            Action::Bottom => moved(self.viewport.scroll_to_bottom()),
            Action::OpenUrl => {
                self.mode = Mode::UrlInput {
                    buffer: String::new(),
                };
                redraw()
            }
            Action::ToggleTiming => {
                self.timing_visible = !self.timing_visible;
                redraw()
            }
            Action::Commit => self.commit(),
            Action::Cancel => {
                // Cancel drops the buffer with no fetch; the bar reverts to the
                // status row, so a repaint is due.
                self.mode = Mode::Browse;
                redraw()
            }
            Action::DeleteChar => {
                if let Mode::UrlInput { buffer } = &mut self.mode {
                    buffer.pop();
                }
                redraw()
            }
        }
    }

    /// Commit the URL bar: normalize the input, leave the bar, and start a
    /// fetch generation. The returned `Effect::fetch` tells the loop to spawn
    /// the worker — `App` never spawns.
    fn commit(&mut self) -> Effect {
        let Mode::UrlInput { buffer } = &self.mode else {
            return Effect::default();
        };
        let url = net::normalize_url(buffer);
        self.mode = Mode::Browse;
        let id = self.start_fetch(url.clone());
        Effect {
            quit: false,
            dirty: true,
            fetch: Some((id, url)),
        }
    }

    /// Paint the whole frame: the visible body slice into the page area, plus
    /// the bottom row — the URL bar in `UrlInput`, the statusline in `Browse`.
    pub fn draw(&self, frame: &mut Frame) {
        frame.clear();
        for (row, line) in self.viewport.visible().iter().enumerate() {
            frame.put_str(0, row as u16, line, Style::default());
        }
        // Over the page area, after the body and before the bottom row.
        if self.timing_visible {
            self.draw_timing(frame);
        }
        let Some(y) = frame.height().checked_sub(1) else {
            return;
        };
        match &self.mode {
            Mode::UrlInput { buffer } => self.draw_url_bar(frame, y, buffer),
            Mode::Browse => self.draw_status(frame, y),
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

    /// The URL bar: `open: <buffer>` with a cursor cell at the end (the cursor
    /// is always at the end — no readline editing beyond `Backspace`).
    fn draw_url_bar(&self, frame: &mut Frame, y: u16, buffer: &str) {
        let style = reversed();
        for x in 0..frame.width() {
            frame.set(x, y, Cell::new(' ', style));
        }
        let mut prompt = String::from("open: ");
        prompt.push_str(buffer);
        let end = frame.put_str(0, y, &prompt, style);
        frame.set(end, y, Cell::new(CURSOR, style));
    }

    /// Left segment: what page this is — the current fetch's URL, or the app
    /// name before anything has been opened.
    fn status_left(&self) -> String {
        match &self.fetch {
            Fetch::Idle => "yata".into(),
            Fetch::Loading { url, .. } | Fetch::Loaded { url, .. } | Fetch::Failed { url, .. } => {
                url.clone()
            }
        }
    }

    /// Middle segment: where the fetch stands — spinner + progress, the
    /// loaded summary, or the failure reason.
    fn status_middle(&self) -> String {
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
    /// omitted, not shown as a placeholder.
    fn status_right(&self) -> String {
        let mut parts = Vec::new();
        if let Some(percent) = self.viewport.scroll_percent() {
            parts.push(format!("{percent}%"));
        }
        if let Some(dur) = self.timings.frame {
            parts.push(timing::format_ms(dur));
        }
        parts.join(" · ")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> Msg {
        Msg::Key(KeyEvent::new(code, mods))
    }

    fn ch(c: char) -> Msg {
        key(KeyCode::Char(c), KeyModifiers::NONE)
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

    fn load(app: &mut App, id: FetchId, body: Vec<u8>) -> Effect {
        app.update(Msg::Loaded {
            id,
            url: "http://final/".into(),
            status: 200,
            body,
            elapsed: Duration::ZERO,
        })
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
        assert_eq!(url, "https://danluu.com", "scheme defaulting applied");
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

    fn loaded(id: FetchId, status: u16, body_len: usize) -> Msg {
        Msg::Loaded {
            id,
            url: "http://final/".into(),
            status,
            body: vec![b'x'; body_len],
            elapsed: Duration::ZERO,
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
        assert!(row0.ends_with("fetch 12.3 ms"), "row was {row0:?}");
        let row1 = row_text(&frame, 1);
        assert!(
            row1.ends_with(" frame 2.1 ms"),
            "rows must pad to the widest row: {row1:?}"
        );
        // The box: 13 cells wide, right-aligned to the frame edge, reversed.
        for y in 0..2 {
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
    }

    #[test]
    fn f4_again_hides_the_overlay_and_restores_the_page() {
        let mut app = timed_app(40, 10);
        let mut before = Frame::new(40, 10);
        app.draw(&mut before);

        assert_eq!(app.update(f4()), redraw());
        let mut shown = Frame::new(40, 10);
        app.draw(&mut shown);
        assert!(row_text(&shown, 0).ends_with("ms"), "overlay must show");

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
                assert!(row_text(&overlaid, 0).ends_with("fetch 12.3 ms"));
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
            row_text(&frame, 0).ends_with("fetch 12.3 ms"),
            "overlay must stay up under the URL bar"
        );
        let bottom = row_text(&frame, 5);
        assert!(bottom.contains("open:"), "row was {bottom:?}");
        assert!(!bottom.contains("open: F"), "F4 must not type");
    }
}
