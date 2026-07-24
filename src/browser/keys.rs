use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What a resolved key press asks `App` to do. Bindings map keys (or two-key
/// sequences) to one of these; `App` has no other vocabulary for key input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    ScrollDown,
    ScrollUp,
    HalfPageDown,
    HalfPageUp,
    Top,
    Bottom,
    OpenUrl,
    ToggleTiming,
    Commit,
    Cancel,
    DeleteChar,
}

/// Which key map is live. `App`'s own mode carries the URL buffer; this is the
/// bare discriminant the table is scoped by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Browse,
    UrlInput,
}

/// One key plus its modifiers. A binding is one chord, or a two-chord sequence
/// (`gg`); `App` remembers the first chord as a pending prefix between presses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

pub struct Binding {
    pub mode: Mode,
    /// `Some` for the first chord of a two-key sequence; `None` for a plain
    /// single-key binding.
    pub prefix: Option<Chord>,
    pub trigger: Chord,
    pub action: Action,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

const fn chord(code: KeyCode, mods: KeyModifiers) -> Chord {
    Chord { code, mods }
}

const fn browse(prefix: Option<Chord>, trigger: Chord, action: Action) -> Binding {
    Binding {
        mode: Mode::Browse,
        prefix,
        trigger,
        action,
    }
}

const fn input(trigger: Chord, action: Action) -> Binding {
    Binding {
        mode: Mode::UrlInput,
        prefix: None,
        trigger,
        action,
    }
}

/// The single source of truth for key → action mappings (CLAUDE.md); the `?`
/// help overlay (M7) is generated from this table, so bindings stay data.
/// Never match on key events elsewhere — the one sanctioned exception is the
/// printable-character path in `App::update`, documented there.
pub const BINDINGS: &[Binding] = &[
    // Browse.
    browse(None, chord(KeyCode::Char('j'), NONE), Action::ScrollDown),
    browse(None, chord(KeyCode::Down, NONE), Action::ScrollDown),
    browse(None, chord(KeyCode::Char('k'), NONE), Action::ScrollUp),
    browse(None, chord(KeyCode::Up, NONE), Action::ScrollUp),
    browse(None, chord(KeyCode::Char('d'), CTRL), Action::HalfPageDown),
    browse(None, chord(KeyCode::PageDown, NONE), Action::HalfPageDown),
    browse(None, chord(KeyCode::Char('u'), CTRL), Action::HalfPageUp),
    browse(None, chord(KeyCode::PageUp, NONE), Action::HalfPageUp),
    // `gg`: a two-key sequence living in the table, not special-cased in code.
    browse(
        Some(chord(KeyCode::Char('g'), NONE)),
        chord(KeyCode::Char('g'), NONE),
        Action::Top,
    ),
    browse(None, chord(KeyCode::Home, NONE), Action::Top),
    browse(None, chord(KeyCode::Char('G'), NONE), Action::Bottom),
    browse(None, chord(KeyCode::End, NONE), Action::Bottom),
    browse(None, chord(KeyCode::Char('o'), NONE), Action::OpenUrl),
    // `F4` is the timing inspector (PLAN.md §3 `F1`–`F4`); Browse only — in
    // the URL bar it is unbound and ignored.
    browse(None, chord(KeyCode::F(4), NONE), Action::ToggleTiming),
    browse(None, chord(KeyCode::Char('q'), NONE), Action::Quit),
    browse(None, chord(KeyCode::Char('c'), CTRL), Action::Quit),
    // UrlInput. `q` is absent on purpose: it is a letter here and types.
    input(chord(KeyCode::Enter, NONE), Action::Commit),
    input(chord(KeyCode::Esc, NONE), Action::Cancel),
    input(chord(KeyCode::Backspace, NONE), Action::DeleteChar),
    input(chord(KeyCode::Char('c'), CTRL), Action::Quit),
];

/// The outcome of resolving one key press against the table, given the current
/// mode and any pending prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// Fire this action; any pending prefix is consumed.
    Action(Action),
    /// This chord opens a two-key sequence — remember it as the pending prefix.
    Pending(Chord),
    /// Not a `Press` event: leave all state, including a pending prefix, alone.
    Ignore,
    /// Nothing matched; discard any pending prefix (it is now cancelled).
    Unbound,
}

/// Resolve `ev` in `mode` with `pending` as the prefix carried from the prior
/// press. There are no timers (idle CPU 0%): a pending prefix simply waits, and
/// a chord that does not complete the sequence cancels it and is resolved fresh
/// (so `g` then `j` scrolls).
pub fn resolve(mode: Mode, pending: Option<Chord>, ev: &KeyEvent) -> Resolution {
    if ev.kind != KeyEventKind::Press {
        return Resolution::Ignore;
    }
    let c = Chord {
        code: ev.code,
        mods: normalize(ev.code, ev.modifiers),
    };

    // A prefix is waiting: complete the sequence if this chord does; otherwise
    // fall through and resolve the chord on its own, dropping the prefix.
    if let Some(b) = pending.and_then(|prefix| {
        BINDINGS
            .iter()
            .find(|b| b.mode == mode && b.prefix == Some(prefix) && b.trigger == c)
    }) {
        return Resolution::Action(b.action);
    }

    // Does this chord open a two-key sequence? No single-key binding reuses a
    // sequence's first chord, so starting the sequence wins.
    if BINDINGS
        .iter()
        .any(|b| b.mode == mode && b.prefix == Some(c))
    {
        return Resolution::Pending(c);
    }

    match BINDINGS
        .iter()
        .find(|b| b.mode == mode && b.prefix.is_none() && b.trigger == c)
    {
        Some(b) => Resolution::Action(b.action),
        None => Resolution::Unbound,
    }
}

/// A shifted character already encodes the shift in its value (`G` vs `g`) and
/// terminals disagree on whether they *also* report `SHIFT`; drop it for
/// character keys so a binding matches either way. Other keys keep their mods.
fn normalize(code: KeyCode, mods: KeyModifiers) -> KeyModifiers {
    match code {
        KeyCode::Char(_) => mods.difference(KeyModifiers::SHIFT),
        _ => mods,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn browse_key(code: KeyCode, mods: KeyModifiers) -> Resolution {
        resolve(Mode::Browse, None, &press(code, mods))
    }

    #[test]
    fn quit_bindings() {
        assert_eq!(
            browse_key(KeyCode::Char('q'), NONE),
            Resolution::Action(Action::Quit)
        );
        assert_eq!(
            browse_key(KeyCode::Char('c'), CTRL),
            Resolution::Action(Action::Quit)
        );
    }

    #[test]
    fn scroll_bindings() {
        assert_eq!(
            browse_key(KeyCode::Char('j'), NONE),
            Resolution::Action(Action::ScrollDown)
        );
        assert_eq!(
            browse_key(KeyCode::Down, NONE),
            Resolution::Action(Action::ScrollDown)
        );
        assert_eq!(
            browse_key(KeyCode::Char('d'), CTRL),
            Resolution::Action(Action::HalfPageDown)
        );
        assert_eq!(
            browse_key(KeyCode::End, NONE),
            Resolution::Action(Action::Bottom)
        );
    }

    #[test]
    fn capital_g_matches_with_or_without_shift() {
        // Terminals differ on reporting SHIFT for uppercase letters; both must
        // reach Bottom.
        assert_eq!(
            browse_key(KeyCode::Char('G'), NONE),
            Resolution::Action(Action::Bottom)
        );
        assert_eq!(
            browse_key(KeyCode::Char('G'), KeyModifiers::SHIFT),
            Resolution::Action(Action::Bottom)
        );
    }

    #[test]
    fn gg_is_a_two_key_sequence() {
        let g = chord(KeyCode::Char('g'), NONE);
        // First `g` opens the sequence rather than acting.
        assert_eq!(
            resolve(Mode::Browse, None, &press(KeyCode::Char('g'), NONE)),
            Resolution::Pending(g)
        );
        // Second `g` completes it.
        assert_eq!(
            resolve(Mode::Browse, Some(g), &press(KeyCode::Char('g'), NONE)),
            Resolution::Action(Action::Top)
        );
    }

    #[test]
    fn prefix_then_nonmatch_resolves_the_second_key_fresh() {
        let g = chord(KeyCode::Char('g'), NONE);
        // `g` then `j`: the sequence fails, `j` resolves on its own.
        assert_eq!(
            resolve(Mode::Browse, Some(g), &press(KeyCode::Char('j'), NONE)),
            Resolution::Action(Action::ScrollDown)
        );
    }

    #[test]
    fn bindings_are_mode_scoped() {
        // `o` opens the URL bar in Browse but is just a letter in UrlInput.
        assert_eq!(
            resolve(Mode::Browse, None, &press(KeyCode::Char('o'), NONE)),
            Resolution::Action(Action::OpenUrl)
        );
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::Char('o'), NONE)),
            Resolution::Unbound
        );
        // `q` quits in Browse, types in UrlInput.
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::Char('q'), NONE)),
            Resolution::Unbound
        );
        // Ctrl-c quits from either mode.
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::Char('c'), CTRL)),
            Resolution::Action(Action::Quit)
        );
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::Enter, NONE)),
            Resolution::Action(Action::Commit)
        );
    }

    #[test]
    fn f4_toggles_timing_in_browse_only() {
        assert_eq!(
            browse_key(KeyCode::F(4), NONE),
            Resolution::Action(Action::ToggleTiming)
        );
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::F(4), NONE)),
            Resolution::Unbound
        );
    }

    #[test]
    fn unbound_keys_do_nothing() {
        assert_eq!(browse_key(KeyCode::Char('x'), NONE), Resolution::Unbound);
        assert_eq!(browse_key(KeyCode::Char('q'), CTRL), Resolution::Unbound);
    }

    #[test]
    fn non_press_events_are_ignored_and_keep_pending() {
        let mut ev = press(KeyCode::Char('g'), NONE);
        ev.kind = KeyEventKind::Release;
        // A release must not cancel a pending prefix.
        let g = chord(KeyCode::Char('g'), NONE);
        assert_eq!(resolve(Mode::Browse, Some(g), &ev), Resolution::Ignore);
    }
}
