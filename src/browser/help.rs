//! Help overlay lines generated from the keybinding table (PLAN.md M7).
//!
//! One source of truth: `keys::BINDINGS`. Labels live next to actions so a new
//! binding appears here as soon as its `Action` has a phrase.

use crate::browser::keys::{self, Action, Binding, Chord, Mode};
use crossterm::event::KeyCode;
use std::collections::BTreeMap;

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
        Action::ToggleTiming => "timing overlay",
        Action::Commit => "confirm",
        Action::Cancel => "cancel",
        Action::DeleteChar => "delete character",
        Action::HintFollow => "link hints (follow)",
        Action::HintYank => "link hints (yank URL)",
        Action::FocusNext => "next link",
        Action::FocusPrev => "previous link",
        Action::FollowFocus => "follow focused link",
        Action::HistoryBack => "history back",
        Action::HistoryForward => "history forward",
        Action::Reload => "reload",
        Action::YankUrl => "yank page URL",
        Action::OpenSearch => "search in page",
        Action::SearchNext => "next search match",
        Action::SearchPrev => "previous search match",
        Action::ToggleHelp => "this help",
    }
}

/// Full help text: title + Browse bindings + input-mode bindings.
pub fn help_text() -> String {
    let mut lines = vec![
        "yata — keybindings".into(),
        String::new(),
        "Browse".into(),
        String::from("──────"),
    ];
    lines.extend(section_lines(Mode::Browse));
    lines.push(String::new());
    lines.push("URL bar / search".into());
    lines.push(String::from("────────────────"));
    // UrlInput and SearchInput share the same chord set in the table.
    lines.extend(section_lines(Mode::UrlInput));
    lines.push(String::new());
    lines.push("Press ? or Esc to close.".into());
    lines.join("\n")
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
        for needle in ["f ", "follow", "?", "/", "q ", "gg", "scroll", "search"] {
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
}
