use unicode_width::UnicodeWidthStr;

use crate::browser::keys::{self, Action, Chord, Resolution};
use crate::browser::viewport::Viewport;
use crate::msg::Msg;
use crate::net::{self, FetchId};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

/// Where the current fetch stands. `Loaded` keeps the raw body so a re-wrap on
/// resize can rebuild the viewport from it (via `String::from_utf8_lossy`).
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
        id
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
            } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                // Any status has a body worth showing (even a 404); charset is
                // M2's problem, so lossy-decode the raw bytes for now.
                let text = String::from_utf8_lossy(&body).into_owned();
                self.viewport.set_content(&text, self.size.0, self.page());
                self.fetch = Fetch::Loaded { url, status, body };
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
    /// the bottom row — the URL bar in `UrlInput`, the status placeholder in
    /// `Browse`.
    pub fn draw(&self, frame: &mut Frame) {
        frame.clear();
        for (row, line) in self.viewport.visible().iter().enumerate() {
            frame.put_str(0, row as u16, line, Style::default());
        }
        let Some(y) = frame.height().checked_sub(1) else {
            return;
        };
        match &self.mode {
            Mode::UrlInput { buffer } => self.draw_url_bar(frame, y, buffer),
            Mode::Browse => self.draw_status(frame, y),
        }
    }

    /// The reversed bottom status row — a placeholder M1.6 replaces, here so
    /// renders and resizes are visible at all.
    fn draw_status(&self, frame: &mut Frame, y: u16) {
        let style = reversed();
        for x in 0..frame.width() {
            frame.set(x, y, Cell::new(' ', style));
        }
        let mut left = String::from("yata");
        if let Some(text) = self.fetch_text() {
            left.push_str("  ");
            left.push_str(&text);
        }
        frame.put_str(1, y, &left, style);
        // Drawn after the left text so the size survives an overlap.
        let size = format!("{}×{}", self.size.0, self.size.1);
        let x = frame.width().saturating_sub(size.width() as u16 + 1);
        frame.put_str(x, y, &size, style);
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

    /// Placeholder-row summary of the fetch (M1.6 replaces this with a real
    /// statusline): `loading… N KB`, `status · N KB`, or the error reason.
    fn fetch_text(&self) -> Option<String> {
        match &self.fetch {
            Fetch::Idle => None,
            Fetch::Loading { url, bytes_so_far } => {
                Some(format!("{url}  loading… {} KB", kb(*bytes_so_far)))
            }
            Fetch::Loaded { url, status, body } => {
                Some(format!("{url}  {status} · {} KB", kb(body.len() as u64)))
            }
            Fetch::Failed { url, reason } => Some(format!("{url}  {reason}")),
        }
    }
}

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
        }
    }

    #[test]
    fn draw_paints_reversed_status_row_with_size_at_bottom() {
        let app = App::new(20, 6);
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);

        let bottom = frame.height() - 1;
        for x in 0..frame.width() {
            assert!(
                frame.get(x, bottom).attrs.contains(Attrs::REVERSE),
                "status row cell {x} must be reversed"
            );
        }
        let text = row_text(&frame, bottom);
        assert!(text.contains("yata"), "status row was {text:?}");
        assert!(text.contains("20×6"), "status row was {text:?}");
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
    }

    #[test]
    fn status_row_shows_loading_progress_then_loaded_summary() {
        let mut app = App::new(60, 6);
        let id = app.start_fetch("http://x/".into());
        let mut frame = Frame::new(60, 6);

        app.draw(&mut frame);
        assert!(row_text(&frame, 5).contains("loading… 0 KB"));

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
        assert!(row.contains("connection refused"), "row was {row:?}");
        assert!(!row.contains("loading"), "row was {row:?}");
    }

    #[test]
    fn draw_shows_the_size_from_state_after_resize() {
        let mut app = App::new(20, 6);
        app.update(Msg::Resize(19, 5));
        let mut frame = Frame::new(19, 5);
        app.draw(&mut frame);
        assert!(row_text(&frame, 4).contains("19×5"));
    }
}
