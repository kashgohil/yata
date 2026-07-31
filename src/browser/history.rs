//! Session history: back/forward with scroll positions (PLAN.md M6, UX §3.6).

/// One entry in the history stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub url: String,
    pub scroll: usize,
}

/// Back stack + forward stack. The live page is not stored here — `App` holds
/// the current URL and scroll; this only remembers where we came from / can go.
#[derive(Clone, Debug, Default)]
pub struct History {
    back: Vec<Entry>,
    forward: Vec<Entry>,
}

impl History {
    /// About to leave `url` at `scroll` for a brand-new navigation: push onto
    /// back and clear forward (a new branch of the timeline).
    pub fn push(&mut self, url: String, scroll: usize) {
        self.back.push(Entry { url, scroll });
        self.forward.clear();
    }

    /// Pop the back stack; push the *current* page onto forward. Returns the
    /// entry to load, or `None` if there is nowhere to go.
    pub fn go_back(&mut self, current_url: String, current_scroll: usize) -> Option<Entry> {
        let entry = self.back.pop()?;
        self.forward.push(Entry {
            url: current_url,
            scroll: current_scroll,
        });
        Some(entry)
    }

    /// Pop the forward stack; push the current page onto back.
    pub fn go_forward(&mut self, current_url: String, current_scroll: usize) -> Option<Entry> {
        let entry = self.forward.pop()?;
        self.back.push(Entry {
            url: current_url,
            scroll: current_scroll,
        });
        Some(entry)
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_and_forward_restore_urls_and_clear_on_branch() {
        let mut h = History::default();
        h.push("http://a/".into(), 10);
        // Now "at" b; go to c.
        h.push("http://b/".into(), 20);
        // back → b
        let b = h.go_back("http://c/".into(), 30).expect("back to b");
        assert_eq!(b.url, "http://b/");
        assert_eq!(b.scroll, 20);
        // forward → c
        let c = h.go_forward("http://b/".into(), 20).expect("forward to c");
        assert_eq!(c.url, "http://c/");
        assert_eq!(c.scroll, 30);
        // back to b, then navigate to d — forward clears
        let _ = h.go_back("http://c/".into(), 30);
        h.push("http://b/".into(), 20);
        assert!(!h.can_forward());
        assert!(h.can_back());
    }
}
