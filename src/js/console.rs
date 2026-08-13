//! The page's console: one ordered list of everything JavaScript had to say
//! (M10.7).
//!
//! Not a debugging leftover. CLAUDE.md rule 4 makes the inspectors product
//! surfaces, and this is the fifth: it is the only place a reader can find out
//! *why* a page's script did nothing, which is the most common thing that goes
//! wrong for the rest of this milestone.
//!
//! **One list, in order.** Console calls, uncaught exceptions from the
//! document-order pass, scripts skipped for their `type`, and — as they land —
//! listener and timer exceptions (M10.8, M10.9) and `fetch` failures (M10.12)
//! all go here. The interleaving is the information: "it logged twice and then
//! threw" is a different story from "it threw and then logged twice".
//!
//! **Bounded and page-local.** A ring buffer with a fixed cap, cleared on
//! navigation. A `setInterval` logging every 4 ms is something M10.13 will
//! deliberately try; it must cost a bounded amount of memory.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;

/// How many entries a page may keep. Oldest are dropped first: the tail of a
/// runaway logger is more useful than its head, and the head is what a page
/// spamming the console would otherwise pin in memory forever.
pub const MAX_ENTRIES: usize = 500;

/// How long one entry's text may be. A page logging a 10 MB string must not
/// put 10 MB in the pane; the JS formatter clips to this too, so the string
/// never crosses the boundary at full size, and this is the backstop that
/// holds whatever the formatter does.
pub const MAX_TEXT: usize = 1024;

/// Console levels, plus the engine's own voice. Everything the pane shows is
/// one of these, and the level is what the statusline counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

impl Level {
    /// The tag `--dump-js` and the pane print. Fixed width so the pane's text
    /// column lines up without a table layout.
    pub fn tag(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Log => "log  ",
            Level::Info => "info ",
            Level::Warn => "warn ",
            Level::Error => "error",
        }
    }
}

/// One thing that happened, with where it happened when that is known.
#[derive(Clone, PartialEq, Debug)]
pub struct Entry {
    pub level: Level,
    /// The script the entry came from (`inline#2`, a URL), if any. Console
    /// calls have none today — a browser gets one from a stack walk we do not
    /// do — but an uncaught exception does.
    pub source: Option<String>,
    pub line: Option<u32>,
    pub text: String,
}

impl fmt::Display for Entry {
    /// The one rendering, shared by the `F5` pane and `--dump-js`, so a test
    /// that greps the dump is asserting what a reader sees:
    ///
    /// ```text
    /// log   hello
    /// error inline#1:3: cannot read property 'x' of null
    /// warn  <script type=module> was skipped
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.level.tag())?;
        if let Some(source) = &self.source {
            match self.line {
                Some(line) => write!(f, "{source}:{line}: ")?,
                None => write!(f, "{source}: ")?,
            }
        }
        f.write_str(&self.text)
    }
}

/// The page's console, shared between `App` and the binding closures that
/// append to it. `Rc` because the closures outlive any one tick and the buffer
/// has to outlive them both; page-local because `App` drops it on navigation.
#[derive(Clone, Default)]
pub struct Console {
    entries: Rc<RefCell<VecDeque<Entry>>>,
}

impl Console {
    pub fn new() -> Console {
        Console::default()
    }

    /// Append one entry, dropping the oldest if the buffer is full and
    /// clipping text to [`MAX_TEXT`].
    pub fn push(&self, level: Level, source: Option<String>, line: Option<u32>, text: &str) {
        let mut entries = self.entries.borrow_mut();
        if entries.len() == MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(Entry {
            level,
            source,
            line,
            text: clip(text),
        });
    }

    /// Everything logged so far, oldest first. Cloned rather than borrowed:
    /// the pane is built rarely and a `Ref` escaping here would be one more
    /// way for a binding to find the buffer already borrowed.
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.borrow().iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// How many entries are errors — what the statusline tells the reader
    /// about, because a page whose script threw and rendered nothing is the
    /// worst outcome this milestone can produce.
    pub fn error_count(&self) -> usize {
        self.entries
            .borrow()
            .iter()
            .filter(|entry| entry.level == Level::Error)
            .count()
    }

    /// Drop everything. Navigation calls this; nothing else should.
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }
}

/// Clip to [`MAX_TEXT`] on a character boundary, marking that it was clipped.
fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_string();
    }
    let mut end = MAX_TEXT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_render_with_their_level_and_source() {
        let console = Console::new();
        console.push(Level::Log, None, None, "hello");
        console.push(
            Level::Error,
            Some("inline#1".into()),
            Some(3),
            "cannot read property 'x' of null",
        );
        console.push(Level::Warn, Some("lib.js".into()), None, "skipped");

        let rendered: Vec<String> = console.entries().iter().map(Entry::to_string).collect();
        assert_eq!(
            rendered,
            [
                "log   hello",
                "error inline#1:3: cannot read property 'x' of null",
                "warn  lib.js: skipped",
            ]
        );
    }

    #[test]
    fn the_ring_buffer_drops_the_oldest_at_the_cap() {
        // M10.13 will point a `setInterval` at this. The tail is what a reader
        // needs; the head is what a runaway logger would otherwise pin.
        let console = Console::new();
        for i in 0..MAX_ENTRIES + 10 {
            console.push(Level::Log, None, None, &i.to_string());
        }
        let entries = console.entries();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().unwrap().text, "10");
        assert_eq!(entries.last().unwrap().text, (MAX_ENTRIES + 9).to_string());
    }

    #[test]
    fn a_long_message_is_clipped_and_says_so() {
        let console = Console::new();
        console.push(Level::Log, None, None, &"x".repeat(10 * 1024 * 1024));
        let entry = console.entries().pop().unwrap();
        assert!(entry.text.len() < MAX_TEXT + 64, "{}", entry.text.len());
        assert!(entry.text.ends_with("(10485760 bytes)"), "{}", entry.text);
    }

    #[test]
    fn clipping_never_splits_a_character() {
        let console = Console::new();
        // A multi-byte character straddling the cap: clipping to a byte index
        // inside it would panic on the slice.
        console.push(Level::Log, None, None, &"é".repeat(MAX_TEXT));
        let entry = console.entries().pop().unwrap();
        assert!(entry.text.starts_with('é'));
    }

    #[test]
    fn errors_are_counted_and_clearing_empties_the_list() {
        let console = Console::new();
        console.push(Level::Log, None, None, "a");
        console.push(Level::Error, None, None, "b");
        console.push(Level::Error, None, None, "c");
        assert_eq!(console.error_count(), 2);
        assert!(!console.is_empty());

        console.clear();
        assert!(console.is_empty());
        assert_eq!(console.error_count(), 0);
    }

    #[test]
    fn a_clone_shares_one_buffer() {
        // The binding closures hold a clone; `App` holds the original. They
        // must be the same list or the pane would show half the story.
        let console = Console::new();
        let held_by_bindings = console.clone();
        held_by_bindings.push(Level::Log, None, None, "from a binding");
        assert_eq!(console.entries().len(), 1);
    }
}
