use unicode_width::UnicodeWidthStr;

use crate::browser::keys::{self, Action};
use crate::msg::Msg;
use crate::term::{Attrs, Cell, Frame, Style};

/// What one message asks of the event loop: exit, and/or a redraw because the
/// frame no longer matches the state. The loop ORs `dirty` across a batch and
/// renders at most once.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Effect {
    pub quit: bool,
    pub dirty: bool,
}

/// The UI state. Pure with respect to the terminal: `update` touches only
/// state, `draw` touches only the given frame.
pub struct App {
    size: (u16, u16),
}

impl App {
    pub fn new(w: u16, h: u16) -> Self {
        App { size: (w, h) }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
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
        frame.put_str(1, y, "yata", status);
        let size = format!("{}×{}", self.size.0, self.size.1);
        let x = frame.width().saturating_sub(size.width() as u16 + 1);
        frame.put_str(x, y, &size, status);
    }
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

    #[test]
    fn draw_shows_the_size_from_state_after_resize() {
        let mut app = App::new(20, 6);
        app.update(Msg::Resize(19, 5));
        let mut frame = Frame::new(19, 5);
        app.draw(&mut frame);
        assert!(row_text(&frame, 4).contains("19×5"));
    }
}
