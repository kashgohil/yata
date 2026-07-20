use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
}

pub struct Binding {
    pub code: KeyCode,
    pub mods: KeyModifiers,
    pub action: Action,
}

/// The single source of truth for key → action mappings (CLAUDE.md). All key
/// handling goes through this table; never match on key events elsewhere.
pub const BINDINGS: &[Binding] = &[
    Binding {
        code: KeyCode::Char('q'),
        mods: KeyModifiers::NONE,
        action: Action::Quit,
    },
    Binding {
        code: KeyCode::Char('c'),
        mods: KeyModifiers::CONTROL,
        action: Action::Quit,
    },
];

pub fn action_for(ev: &KeyEvent) -> Option<Action> {
    if ev.kind != KeyEventKind::Press {
        return None;
    }
    BINDINGS
        .iter()
        .find(|b| b.code == ev.code && b.mods == ev.modifiers)
        .map(|b| b.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn quit_bindings() {
        assert_eq!(
            action_for(&press(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
        assert_eq!(
            action_for(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn unbound_keys_do_nothing() {
        assert_eq!(
            action_for(&press(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            action_for(&press(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn only_press_events_trigger_actions() {
        let mut ev = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Release;
        assert_eq!(action_for(&ev), None);
    }
}
