//! Help overlay lines generated from the keybinding table (PLAN.md M7).
//!
//! One source of truth: `keys::BINDINGS`. Labels live next to actions so a new
//! binding appears here as soon as its `Action` has a phrase.

use crate::browser::keys::{self, Action, Binding, Chord, Mode};
use crossterm::event::KeyCode;
use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;

/// Short human phrase for an action. Exhaustive so a new `Action` fails to
/// compile until help knows about it.
pub fn action_help(action: Action) -> &'static str {
    match action {
        Action::Quit => "quit",
        Action::ScrollDown => "scroll line down",
        Action::ScrollUp => "scroll line up",
        Action::HalfPageDown => "half-page down",
        Action::HalfPageUp => "half-page up",
        Action::Top => "go to top",
        Action::Bottom => "go to bottom",
        Action::OpenUrl => "open URL",
        Action::EditUrl => "edit current URL",
        Action::ToggleDom => "DOM inspector",
        Action::ToggleStyles => "styles inspector",
        Action::ToggleBoxes => "box inspector",
        Action::ToggleConsole => "javascript console",
        Action::ToggleTiming => "timing overlay",
        Action::Commit => "confirm",
        Action::Cancel => "cancel",
        Action::DeleteChar => "delete character before the caret",
        Action::DeleteCharForward => "delete character after the caret",
        Action::CaretLeft => "caret left",
        Action::CaretRight => "caret right",
        Action::CaretToStart => "caret to start of line",
        Action::CaretToEnd => "caret to end of line",
        Action::HintFollow => "link hints (follow)",
        Action::HintYank => "link hints (yank URL)",
        // Since M11.8 the cycle is links *and* form controls, and since M11.9
        // `Enter` on one of those starts typing rather than going nowhere. The
        // overlay is where a reader finds that out, so it says it.
        Action::FocusNext => "next link or field",
        Action::FocusPrev => "previous link or field",
        // Wikipedia's Search is a `<button>` with no type: `Enter` submits it,
        // and the overlay is where a reader Tabs to Search and finds that out.
        Action::FollowFocus => "follow the focused link / type in the field / submit a button",
        Action::HistoryBack => "history back",
        Action::HistoryForward => "history forward",
        Action::Reload => "reload",
        Action::YankUrl => "yank page URL",
        Action::OpenSearch => "search in page",
        Action::SearchNext => "next search match",
        Action::SearchPrev => "previous search match",
        Action::ToggleHelp => "this help",
        Action::Submit => "submit the form",
        Action::SelectPrev => "previous option",
        Action::SelectNext => "next option",
        Action::SelectFirst => "first option",
        Action::SelectLast => "last option",
        Action::SelectToggle => "toggle option",
        Action::SelectCommit => "choose option and close",
    }
}

/// Full help text: title + Browse bindings + input-mode bindings.
pub fn help_text() -> String {
    let mut lines = vec!["yata — keybindings".into(), String::new()];
    lines.extend(heading("Browse"));
    lines.extend(section_lines(Mode::Browse));
    lines.push(String::new());
    lines.extend(heading("URL bar / search"));
    // UrlInput and SearchInput share the same chord set in the table.
    lines.extend(section_lines(Mode::UrlInput));
    lines.push(String::new());
    lines.extend(heading("Text field — Enter starts typing in it, Esc stops"));
    // The first row is not a binding and cannot be: printable characters are
    // the one sanctioned path outside the table (CLAUDE.md), and a mode that
    // listed everything *except* what typing does would be worse than useless.
    lines.push(format!(
        "  {:<22} {}",
        "(any character)", "insert at the caret"
    ));
    lines.extend(section_lines(Mode::Field));
    lines.push(String::new());
    lines.extend(heading("Select — Enter chooses, Esc stops"));
    lines.extend(section_lines(Mode::Select));
    lines.push(String::new());
    lines.push("Press ? or Esc to close.".into());
    lines.join("\n")
}

/// A section title and its rule, measured in cells so the rule cannot end up a
/// character short of the words above it — which is what happened the moment a
/// third section was hand-underlined.
fn heading(title: &str) -> [String; 2] {
    [title.to_string(), "─".repeat(title.width())]
}

fn section_lines(mode: Mode) -> Vec<String> {
    // Group chords by action, preserving first-seen order via BTreeMap of
    // formatted action order index.
    let mut groups: BTreeMap<usize, (Action, Vec<String>)> = BTreeMap::new();
    let mut order: Vec<Action> = Vec::new();
    for b in keys::BINDINGS.iter().filter(|b| b.mode == mode) {
        if !order.contains(&b.action) {
            order.push(b.action);
        }
        let idx = order.iter().position(|a| *a == b.action).unwrap();
        groups
            .entry(idx)
            .or_insert_with(|| (b.action, Vec::new()))
            .1
            .push(format_binding(b));
    }
    let mut out = Vec::new();
    for i in 0..order.len() {
        let Some((action, chords)) = groups.get(&i) else {
            continue;
        };
        let keys = chords.join(" / ");
        out.push(format!("  {:<22} {}", keys, action_help(*action)));
    }
    out
}

fn format_binding(b: &Binding) -> String {
    match b.prefix {
        Some(prefix) => format!("{}{}", format_chord(prefix), format_chord(b.trigger)),
        None => format_chord(b.trigger),
    }
}

fn format_chord(c: Chord) -> String {
    let mut s = String::new();
    if c.mods.contains(crossterm::event::KeyModifiers::CONTROL) {
        s.push_str("Ctrl-");
    }
    if c.mods.contains(crossterm::event::KeyModifiers::ALT) {
        s.push_str("Alt-");
    }
    // SHIFT on non-char keys (e.g. Shift-Tab, Shift-Backspace).
    if c.mods.contains(crossterm::event::KeyModifiers::SHIFT) && !matches!(c.code, KeyCode::Char(_))
    {
        s.push_str("Shift-");
    }
    s.push_str(&format_code(c.code));
    s
}

fn format_code(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "Shift-Tab".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_mentions_flagship_bindings() {
        let text = help_text();
        for needle in [
            "f ",
            "follow",
            "?",
            "/",
            "q ",
            "gg",
            "scroll",
            "search",
            // M11.10: Browse `Enter` also submits a focused submit button.
            "submit a button",
        ] {
            assert!(
                text.to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
                    || text.contains(needle.trim()),
                "help missing {needle:?}:\n{text}"
            );
        }
        // Link hints and quit must be present as concrete keys.
        assert!(text.contains("link hints") || text.contains("follow"));
        assert!(text.contains('q') || text.contains("quit"));
    }

    #[test]
    fn help_lists_the_field_mode_because_it_is_generated_from_the_table() {
        // M11.9: the mode's rows appear here without anything being written
        // twice — adding a row to `BINDINGS` is what puts it on this page.
        let text = help_text();
        assert!(text.contains("Text field"), "{text}");
        for needle in [
            "insert at the caret",
            "delete character before the caret",
            "delete character after the caret",
            "caret to start of line",
            "Ctrl-c",
            // M11.10's row, and the reason it is here without anyone editing
            // this file's list: `Enter` became a `Binding`, so the overlay
            // grew a line.
            "submit the form",
        ] {
            assert!(text.contains(needle), "help missing {needle:?}:\n{text}");
        }
        // Every Field binding in the table is on the page, keys and phrase.
        for b in keys::BINDINGS.iter().filter(|b| b.mode == Mode::Field) {
            assert!(
                text.contains(&format_binding(b)),
                "help missing the chord for {:?}:\n{text}",
                b.action
            );
            assert!(text.contains(action_help(b.action)), "{text}");
        }
    }

    #[test]
    fn help_lists_every_select_binding_from_the_table() {
        let text = help_text();
        assert!(text.contains("Select"), "{text}");
        for b in keys::BINDINGS.iter().filter(|b| b.mode == Mode::Select) {
            assert!(text.contains(&format_binding(b)), "{:?}\n{text}", b.action);
            assert!(text.contains(action_help(b.action)), "{text}");
        }
    }
}
