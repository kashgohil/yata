use unicode_width::UnicodeWidthStr;

use crate::browser::keys::{self, Action};
use crate::msg::Msg;
use crate::net::FetchId;
use crate::term::{Attrs, Cell, Frame, Style};

/// What one message asks of the event loop: exit, and/or a redraw because the
/// frame no longer matches the state. The loop ORs `dirty` across a batch and
/// renders at most once.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Effect {
    pub quit: bool,
    pub dirty: bool,
}

/// Where the current fetch stands. `Loaded` keeps the raw body for M1.5's
/// viewport to render; nothing renders it yet.
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
}

impl App {
    pub fn new(w: u16, h: u16) -> Self {
        App {
            size: (w, h),
            fetch_gen: 0,
            current_fetch: None,
            fetch: Fetch::Idle,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
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
            // Keys resolve only through the binding table (CLAUDE.md); a key
            // bound to nothing changes nothing, so it must not cause a redraw.
            Msg::Key(ev) => match keys::action_for(&ev) {
                Some(Action::Quit) => Effect {
                    quit: true,
                    dirty: false,
                },
                None => Effect::default(),
            },
            Msg::Resize(w, h) => {
                self.size = (w, h);
                Effect {
                    quit: false,
                    dirty: true,
                }
            }
            // Net messages: a stale id means a fetch that was superseded —
            // its progress, body, and errors must not clobber the current
            // one, so it changes nothing and triggers no redraw.
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
                        Effect {
                            quit: false,
                            dirty: true,
                        }
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
                self.fetch = Fetch::Loaded { url, status, body };
                Effect {
                    quit: false,
                    dirty: true,
                }
            }
            Msg::NetError { id, url, reason } => {
                if Some(id) != self.current_fetch {
                    return Effect::default();
                }
                self.fetch = Fetch::Failed { url, reason };
                Effect {
                    quit: false,
                    dirty: true,
                }
            }
        }
    }

    /// Paint the whole frame: blank page area plus a reversed bottom status
    /// row — a placeholder M1.6 replaces, here so renders and resizes are
    /// visible at all.
    pub fn draw(&self, frame: &mut Frame) {
        frame.clear();
        let Some(y) = frame.height().checked_sub(1) else {
            return;
        };
        let status = Style {
            attrs: Attrs::REVERSE,
            ..Style::default()
        };
        for x in 0..frame.width() {
            frame.set(x, y, Cell::new(' ', status));
        }
        let mut left = String::from("yata");
        if let Some(text) = self.fetch_text() {
            left.push_str("  ");
            left.push_str(&text);
        }
        frame.put_str(1, y, &left, status);
        // Drawn after the left text so the size survives an overlap.
        let size = format!("{}×{}", self.size.0, self.size.1);
        let x = frame.width().saturating_sub(size.width() as u16 + 1);
        frame.put_str(x, y, &size, status);
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

    #[test]
    fn quit_keys_report_quit() {
        let mut app = App::new(80, 24);
        let effect = app.update(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(effect.quit);

        let effect = app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(effect.quit);
    }

    #[test]
    fn unbound_keys_are_not_dirty() {
        let mut app = App::new(80, 24);
        let effect = app.update(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            effect,
            Effect {
                quit: false,
                dirty: false
            }
        );
    }

    #[test]
    fn resize_updates_size_and_requests_redraw() {
        let mut app = App::new(80, 24);
        let effect = app.update(Msg::Resize(120, 40));
        assert_eq!(
            effect,
            Effect {
                quit: false,
                dirty: true
            }
        );
        assert_eq!(app.size(), (120, 40));
    }

    fn row_text(frame: &Frame, y: u16) -> String {
        (0..frame.width()).map(|x| frame.get(x, y).ch).collect()
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
    fn draw_leaves_the_page_area_blank() {
        let app = App::new(20, 6);
        let mut frame = Frame::new(20, 6);
        app.draw(&mut frame);
        for y in 0..frame.height() - 1 {
            for x in 0..frame.width() {
                assert_eq!(frame.get(x, y), Cell::default());
            }
        }
    }

    fn dirty() -> Effect {
        Effect {
            quit: false,
            dirty: true,
        }
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
    fn stale_fetch_messages_are_ignored() {
        let mut app = App::new(80, 24);
        let stale = app.start_fetch("http://old/".into());
        let current = app.start_fetch("http://new/".into());
        assert_ne!(stale, current, "each fetch gets a fresh generation");

        // A slow stale fetch reporting progress, success, or failure must
        // not clobber the current one — no state change, no redraw.
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

        // The current id still lands normally after all that noise.
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
