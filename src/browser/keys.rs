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
    /// Pre-fill the URL bar with the current page URL (M6 `O`).
    EditUrl,
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    ToggleDom,
    ToggleStyles,
    ToggleBoxes,
    ToggleTiming,
    ToggleConsole,
    Commit,
    Cancel,
    /// Delete the character *before* the caret (`Backspace`).
    DeleteChar,
    /// Delete the character *after* it (`Delete`) — a field has both, because
    /// a caret that can move has something on each side of it (M11.9).
    DeleteCharForward,
    CaretLeft,
    CaretRight,
    CaretToStart,
    CaretToEnd,
    /// Link hints → follow (`f`).
    HintFollow,
    /// Link hints → yank URL (`F`).
    HintYank,
    FocusNext,
    FocusPrev,
    FollowFocus,
    HistoryBack,
    HistoryForward,
    Reload,
    /// Yank the current page URL (`yy`).
    YankUrl,
    /// Open in-page search (`/`).
    OpenSearch,
    /// Jump to the next search match (`n`).
    SearchNext,
    /// Jump to the previous search match (`N`).
    SearchPrev,
    /// Toggle the help overlay (`?`).
    ToggleHelp,
    /// Submit the form the caret is in (`Enter` while typing, M11.10).
    Submit,
    SelectPrev,
    SelectNext,
    SelectFirst,
    SelectLast,
    SelectToggle,
    SelectCommit,
}

/// Which key map is live. `App`'s own mode carries the URL buffer; this is the
/// bare discriminant the table is scoped by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Browse,
    UrlInput,
    /// Same chord set as `UrlInput` (Enter/Esc/Backspace); printable chars type.
    SearchInput,
    /// Typing into a focused form control (M11.9). Entered with `Enter` on a
    /// focused text field, left with `Esc` — see the table's rows for why the
    /// promise that `q` quits is kept by `Ctrl-c` here.
    Field,
    /// Choosing an option without giving ordinary letters to the page.
    Select,
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

const fn input(mode: Mode, trigger: Chord, action: Action) -> Binding {
    Binding {
        mode,
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
    browse(None, chord(KeyCode::Char('O'), NONE), Action::EditUrl),
    browse(None, chord(KeyCode::Char('t'), NONE), Action::NewTab),
    browse(None, chord(KeyCode::Char('x'), NONE), Action::CloseTab),
    browse(
        Some(chord(KeyCode::Char('g'), NONE)),
        chord(KeyCode::Char('t'), NONE),
        Action::NextTab,
    ),
    browse(
        Some(chord(KeyCode::Char('g'), NONE)),
        chord(KeyCode::Char('T'), NONE),
        Action::PreviousTab,
    ),
    // `F1`–`F4` are the DOM, styles, boxes and timing inspectors
    // (PLAN.md §3 `F1`–`F4`); Browse only — in the URL bar they are unbound.
    browse(None, chord(KeyCode::F(1), NONE), Action::ToggleDom),
    browse(None, chord(KeyCode::F(2), NONE), Action::ToggleStyles),
    browse(None, chord(KeyCode::F(3), NONE), Action::ToggleBoxes),
    browse(None, chord(KeyCode::F(4), NONE), Action::ToggleTiming),
    browse(None, chord(KeyCode::F(5), NONE), Action::ToggleConsole),
    // M6 interaction.
    browse(None, chord(KeyCode::Char('f'), NONE), Action::HintFollow),
    browse(None, chord(KeyCode::Char('F'), NONE), Action::HintYank),
    browse(None, chord(KeyCode::Tab, NONE), Action::FocusNext),
    browse(None, chord(KeyCode::BackTab, NONE), Action::FocusPrev),
    browse(None, chord(KeyCode::Enter, NONE), Action::FollowFocus),
    browse(None, chord(KeyCode::Char('H'), NONE), Action::HistoryBack),
    browse(
        None,
        chord(KeyCode::Char('L'), NONE),
        Action::HistoryForward,
    ),
    browse(None, chord(KeyCode::Backspace, NONE), Action::HistoryBack),
    // Shift-Backspace: some terminals report Backspace+SHIFT.
    browse(
        None,
        chord(KeyCode::Backspace, KeyModifiers::SHIFT),
        Action::HistoryForward,
    ),
    browse(None, chord(KeyCode::Char('r'), NONE), Action::Reload),
    // `yy`: yank page URL (same two-key shape as `gg`).
    browse(
        Some(chord(KeyCode::Char('y'), NONE)),
        chord(KeyCode::Char('y'), NONE),
        Action::YankUrl,
    ),
    // M7 polish.
    browse(None, chord(KeyCode::Char('/'), NONE), Action::OpenSearch),
    browse(None, chord(KeyCode::Char('n'), NONE), Action::SearchNext),
    browse(None, chord(KeyCode::Char('N'), NONE), Action::SearchPrev),
    browse(None, chord(KeyCode::Char('?'), NONE), Action::ToggleHelp),
    browse(None, chord(KeyCode::Esc, NONE), Action::Cancel),
    browse(None, chord(KeyCode::Char('q'), NONE), Action::Quit),
    browse(None, chord(KeyCode::Char('c'), CTRL), Action::Quit),
    // UrlInput / SearchInput. `q` is absent on purpose: it is a letter here.
    input(Mode::UrlInput, chord(KeyCode::Enter, NONE), Action::Commit),
    input(Mode::UrlInput, chord(KeyCode::Esc, NONE), Action::Cancel),
    input(
        Mode::UrlInput,
        chord(KeyCode::Backspace, NONE),
        Action::DeleteChar,
    ),
    input(
        Mode::UrlInput,
        chord(KeyCode::Char('c'), CTRL),
        Action::Quit,
    ),
    input(
        Mode::SearchInput,
        chord(KeyCode::Enter, NONE),
        Action::Commit,
    ),
    input(Mode::SearchInput, chord(KeyCode::Esc, NONE), Action::Cancel),
    input(
        Mode::SearchInput,
        chord(KeyCode::Backspace, NONE),
        Action::DeleteChar,
    ),
    input(
        Mode::SearchInput,
        chord(KeyCode::Char('c'), CTRL),
        Action::Quit,
    ),
    // Field (M11.9): typing into the focused control. A third input *mode*,
    // not a second exception — the printable-character path in `App::on_key`
    // is widened to it, and nothing else in the app reads `KeyCode::Char`.
    //
    // `q` is absent for the reason it is absent above: it is a letter here.
    // PLAN.md §3 promises `q` "always works" — and in the URL bar that promise
    // has always been kept by `Ctrl-c`, which is bound in every mode and is the
    // one chord a reader can press without first knowing where they are. `q`
    // quits the moment they are not inside a field, which `Esc` makes true in
    // one key, and the statusline says which of the two they are in.
    //
    // `Enter` submits the form the caret is in (M11.10) — the key M11.9 left
    // unbound on purpose, spent on the thing it was reserved for. It is the
    // *only* activator a keyboard has for HN's search box, which has no submit
    // button at all, and it submits whatever form the control is in rather
    // than only HTML's single-field case: see `App::submit_form` for why the rule
    // HTML has here solves a problem a modal browser does not have.
    //
    // Typing ends when the submission starts, because a navigation clears the
    // focus and the mode with it (`App::clear_focus`).
    input(Mode::Field, chord(KeyCode::Enter, NONE), Action::Submit),
    input(Mode::Field, chord(KeyCode::Esc, NONE), Action::Cancel),
    input(Mode::Field, chord(KeyCode::Char('c'), CTRL), Action::Quit),
    input(
        Mode::Field,
        chord(KeyCode::Backspace, NONE),
        Action::DeleteChar,
    ),
    input(
        Mode::Field,
        chord(KeyCode::Delete, NONE),
        Action::DeleteCharForward,
    ),
    input(Mode::Field, chord(KeyCode::Left, NONE), Action::CaretLeft),
    input(Mode::Field, chord(KeyCode::Right, NONE), Action::CaretRight),
    input(
        Mode::Field,
        chord(KeyCode::Home, NONE),
        Action::CaretToStart,
    ),
    input(Mode::Field, chord(KeyCode::End, NONE), Action::CaretToEnd),
    // `Tab` moves to the next focusable and **leaves typing**, which is the
    // browser behaviour minus the half a browser cannot have: there, focus
    // implies typing, so tabbing between two fields keeps the keyboard. Here
    // the two are separate states, and the alternative — staying in typing
    // mode when the next focusable happens to be another field — would make
    // whether `q` quits depend on what the page put next in the document. One
    // exit rule instead: you leave a field the same way whatever you land on,
    // and `Enter` starts typing again in one key. Nothing is committed either
    // way; the value has been state since the first keystroke.
    input(Mode::Field, chord(KeyCode::Tab, NONE), Action::FocusNext),
    input(
        Mode::Field,
        chord(KeyCode::BackTab, NONE),
        Action::FocusPrev,
    ),
    // Select (M11.12): cursor movement is table-driven like every other mode.
    input(Mode::Select, chord(KeyCode::Up, NONE), Action::SelectPrev),
    input(Mode::Select, chord(KeyCode::Down, NONE), Action::SelectNext),
    input(
        Mode::Select,
        chord(KeyCode::Home, NONE),
        Action::SelectFirst,
    ),
    input(Mode::Select, chord(KeyCode::End, NONE), Action::SelectLast),
    input(
        Mode::Select,
        chord(KeyCode::Char(' '), NONE),
        Action::SelectToggle,
    ),
    input(
        Mode::Select,
        chord(KeyCode::Enter, NONE),
        Action::SelectCommit,
    ),
    input(Mode::Select, chord(KeyCode::Esc, NONE), Action::Cancel),
    input(Mode::Select, chord(KeyCode::Tab, NONE), Action::FocusNext),
    input(
        Mode::Select,
        chord(KeyCode::BackTab, NONE),
        Action::FocusPrev,
    ),
    input(Mode::Select, chord(KeyCode::Char('c'), CTRL), Action::Quit),
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
    fn f1_toggles_the_dom_inspector_in_browse_only() {
        assert_eq!(
            browse_key(KeyCode::F(1), NONE),
            Resolution::Action(Action::ToggleDom)
        );
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::F(1), NONE)),
            Resolution::Unbound
        );
    }

    #[test]
    fn f3_toggles_the_box_inspector_in_browse_only() {
        assert_eq!(
            browse_key(KeyCode::F(3), NONE),
            Resolution::Action(Action::ToggleBoxes)
        );
        assert_eq!(
            resolve(Mode::UrlInput, None, &press(KeyCode::F(3), NONE)),
            Resolution::Unbound
        );
    }

    #[test]
    fn unbound_keys_do_nothing() {
        assert_eq!(browse_key(KeyCode::Char('z'), NONE), Resolution::Unbound);
        assert_eq!(browse_key(KeyCode::Char('q'), CTRL), Resolution::Unbound);
    }

    #[test]
    fn m6_interaction_bindings() {
        assert_eq!(
            browse_key(KeyCode::Char('f'), NONE),
            Resolution::Action(Action::HintFollow)
        );
        assert_eq!(
            browse_key(KeyCode::Char('F'), NONE),
            Resolution::Action(Action::HintYank)
        );
        assert_eq!(
            browse_key(KeyCode::Tab, NONE),
            Resolution::Action(Action::FocusNext)
        );
        assert_eq!(
            browse_key(KeyCode::BackTab, NONE),
            Resolution::Action(Action::FocusPrev)
        );
        assert_eq!(
            browse_key(KeyCode::Enter, NONE),
            Resolution::Action(Action::FollowFocus)
        );
        assert_eq!(
            browse_key(KeyCode::Char('H'), NONE),
            Resolution::Action(Action::HistoryBack)
        );
        assert_eq!(
            browse_key(KeyCode::Char('L'), NONE),
            Resolution::Action(Action::HistoryForward)
        );
        assert_eq!(
            browse_key(KeyCode::Backspace, NONE),
            Resolution::Action(Action::HistoryBack)
        );
        assert_eq!(
            browse_key(KeyCode::Backspace, KeyModifiers::SHIFT),
            Resolution::Action(Action::HistoryForward)
        );
        assert_eq!(
            browse_key(KeyCode::Char('r'), NONE),
            Resolution::Action(Action::Reload)
        );
        assert_eq!(
            browse_key(KeyCode::Char('O'), NONE),
            Resolution::Action(Action::EditUrl)
        );
        let y = chord(KeyCode::Char('y'), NONE);
        assert_eq!(
            resolve(Mode::Browse, None, &press(KeyCode::Char('y'), NONE)),
            Resolution::Pending(y)
        );
        assert_eq!(
            resolve(Mode::Browse, Some(y), &press(KeyCode::Char('y'), NONE)),
            Resolution::Action(Action::YankUrl)
        );
    }

    #[test]
    fn non_press_events_are_ignored_and_keep_pending() {
        let mut ev = press(KeyCode::Char('g'), NONE);
        ev.kind = KeyEventKind::Release;
        // A release must not cancel a pending prefix.
        let g = chord(KeyCode::Char('g'), NONE);
        assert_eq!(resolve(Mode::Browse, Some(g), &ev), Resolution::Ignore);
    }

    #[test]
    fn a_field_is_a_third_mode_and_letters_type_in_it() {
        // The table is the whole of the mode (M11.9): `q` and `j` resolve to
        // nothing here, which is what sends them down `App::on_key`'s
        // printable-character path — the one sanctioned exception, widened
        // rather than copied.
        for c in ['q', 'j', 'o', '/', 'G'] {
            assert_eq!(
                resolve(Mode::Field, None, &press(KeyCode::Char(c), NONE)),
                Resolution::Unbound,
                "{c} is a letter in a field"
            );
        }
        // Quit is kept by Ctrl-c, in this mode as in the URL bar.
        assert_eq!(
            resolve(Mode::Field, None, &press(KeyCode::Char('c'), CTRL)),
            Resolution::Action(Action::Quit)
        );
        for (code, action) in [
            (KeyCode::Esc, Action::Cancel),
            (KeyCode::Backspace, Action::DeleteChar),
            (KeyCode::Delete, Action::DeleteCharForward),
            (KeyCode::Left, Action::CaretLeft),
            (KeyCode::Right, Action::CaretRight),
            (KeyCode::Home, Action::CaretToStart),
            (KeyCode::End, Action::CaretToEnd),
            (KeyCode::Tab, Action::FocusNext),
            (KeyCode::BackTab, Action::FocusPrev),
        ] {
            assert_eq!(
                resolve(Mode::Field, None, &press(code, NONE)),
                Resolution::Action(action),
                "{code:?}"
            );
        }
    }

    #[test]
    fn enter_in_a_field_submits_the_form() {
        // The key M11.9 left unbound, spent by M11.10 on the thing it was
        // reserved for. It is the only activator a keyboard has for a form
        // with no submit button, which is exactly what HN's search is.
        assert_eq!(
            resolve(Mode::Field, None, &press(KeyCode::Enter, NONE)),
            Resolution::Action(Action::Submit)
        );
        // And nowhere else: submitting is a thing you do from inside a field,
        // never from the URL bar or the search prompt, where `Enter` commits.
        for mode in [Mode::UrlInput, Mode::SearchInput] {
            assert_eq!(
                resolve(mode, None, &press(KeyCode::Enter, NONE)),
                Resolution::Action(Action::Commit)
            );
        }
        // And in Browse it still activates the focused thing, which is what
        // starts the typing in the first place.
        assert_eq!(
            browse_key(KeyCode::Enter, NONE),
            Resolution::Action(Action::FollowFocus)
        );
    }

    #[test]
    fn select_keys_are_one_complete_table_driven_mode() {
        for (code, action) in [
            (KeyCode::Up, Action::SelectPrev),
            (KeyCode::Down, Action::SelectNext),
            (KeyCode::Home, Action::SelectFirst),
            (KeyCode::End, Action::SelectLast),
            (KeyCode::Char(' '), Action::SelectToggle),
            (KeyCode::Enter, Action::SelectCommit),
            (KeyCode::Esc, Action::Cancel),
            (KeyCode::Tab, Action::FocusNext),
            (KeyCode::BackTab, Action::FocusPrev),
        ] {
            assert_eq!(
                resolve(Mode::Select, None, &press(code, NONE)),
                Resolution::Action(action),
                "{code:?}"
            );
        }
        assert_eq!(
            resolve(Mode::Select, None, &press(KeyCode::Char('c'), CTRL)),
            Resolution::Action(Action::Quit)
        );
        assert_eq!(
            resolve(Mode::Select, None, &press(KeyCode::Char('q'), NONE)),
            Resolution::Unbound
        );
    }

    #[test]
    fn m7_search_and_help_bindings() {
        assert_eq!(
            browse_key(KeyCode::Char('/'), NONE),
            Resolution::Action(Action::OpenSearch)
        );
        assert_eq!(
            browse_key(KeyCode::Char('n'), NONE),
            Resolution::Action(Action::SearchNext)
        );
        assert_eq!(
            browse_key(KeyCode::Char('N'), NONE),
            Resolution::Action(Action::SearchPrev)
        );
        assert_eq!(
            browse_key(KeyCode::Char('?'), NONE),
            Resolution::Action(Action::ToggleHelp)
        );
        assert_eq!(
            resolve(Mode::SearchInput, None, &press(KeyCode::Enter, NONE)),
            Resolution::Action(Action::Commit)
        );
        assert_eq!(
            resolve(Mode::SearchInput, None, &press(KeyCode::Esc, NONE)),
            Resolution::Action(Action::Cancel)
        );
    }
}
