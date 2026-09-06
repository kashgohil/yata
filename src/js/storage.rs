//! `localStorage` and `sessionStorage`, per origin, in memory (M10.11).
//!
//! ## Nothing is written to disk, and that is a decision
//!
//! Two reasons, both worth stating where someone will find them:
//!
//! - CLAUDE.md's UI thread does no disk I/O, and `setItem` is a synchronous
//!   call from inside a script tick. Making it durable would mean either
//!   blocking the loop on a write or inventing a persistence worker with its
//!   own consistency story, for a feature no ladder page needs.
//! - Persistent per-origin storage is a tracking surface. A browser you use
//!   for *reading* does not need to carry an identifier for every site you
//!   have ever visited between runs.
//!
//! A browser's `localStorage` survives restarts and pages assume it does — so
//! this is a real deviation, with a real trigger, deposited for M10.14.
//!
//! ## Scoped per origin
//!
//! Two pages on one host share a store; two hosts never do. That is what makes
//! it storage rather than a global, and it is the milestone's first use of an
//! origin at all.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::rc::Rc;

/// The byte cap per origin, per area. Browsers land around 5 MB and a page
/// that hits it is doing something this browser is not for; the point of the
/// number is that a script cannot grow a map until the PLAN.md §4 budget of
/// 100 MB for a whole page is gone.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Which store — they are separate, as in a browser, so writing to one is not
/// visible in the other.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Area {
    Local,
    Session,
}

/// Every origin's storage for this session, shared with the binding closures.
///
/// Lives in `App`, not in the host: a host is dropped on every navigation and
/// two pages on one origin must see the same data.
#[derive(Clone, Default)]
pub struct Storage {
    local: Rc<RefCell<Stores>>,
    session: Rc<RefCell<Stores>>,
}

type Stores = HashMap<String, BTreeMap<String, String>>;

impl Storage {
    pub fn new() -> Storage {
        Storage::default()
    }

    /// Share local storage with a new tab while giving it a fresh session
    /// namespace. Cloning `Storage` still shares both areas within one tab.
    pub fn fork_tab(&self) -> Storage {
        Storage {
            local: self.local.clone(),
            session: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn stores(&self, area: Area) -> &Rc<RefCell<Stores>> {
        match area {
            Area::Local => &self.local,
            Area::Session => &self.session,
        }
    }

    pub fn get(&self, origin: &str, area: Area, key: &str) -> Option<String> {
        self.stores(area)
            .borrow()
            .get(origin)
            .and_then(|store| store.get(key).cloned())
    }

    /// Write, or report that the origin's quota is full. The caller turns
    /// `false` into the `QuotaExceededError` a browser throws.
    pub fn set(&self, origin: &str, area: Area, key: &str, value: &str) -> bool {
        let mut stores = self.stores(area).borrow_mut();
        let store = stores.entry(origin.to_string()).or_default();

        // Measure what the store *would* weigh, so replacing a large value
        // with a small one is never refused.
        let mut bytes = key.len() + value.len();
        for (existing_key, existing) in store.iter() {
            if existing_key != key {
                bytes += existing_key.len() + existing.len();
            }
        }
        if bytes > MAX_BYTES {
            return false;
        }
        store.insert(key.to_string(), value.to_string());
        true
    }

    pub fn remove(&self, origin: &str, area: Area, key: &str) {
        if let Some(store) = self.stores(area).borrow_mut().get_mut(origin) {
            store.remove(key);
        }
    }

    pub fn clear(&self, origin: &str, area: Area) {
        self.stores(area).borrow_mut().remove(origin);
    }

    pub fn len(&self, origin: &str, area: Area) -> usize {
        self.stores(area)
            .borrow()
            .get(origin)
            .map_or(0, BTreeMap::len)
    }

    /// The `i`th key. Ordered, because `key(i)` is only useful if two calls
    /// with the same `i` agree — a browser's order is unspecified but stable,
    /// and a `BTreeMap` gives us stable for free.
    pub fn key_at(&self, origin: &str, area: Area, index: usize) -> Option<String> {
        self.stores(area)
            .borrow()
            .get(origin)
            .and_then(|store| store.keys().nth(index).cloned())
    }
}

/// The origin of a URL — scheme, host and port — or `None` when there is no
/// meaningful one to derive. Storage is keyed by this, so two pages on one
/// host share and two hosts never do.
///
/// Parsed by `net::` like every other URL in the engine; a second URL parser
/// in `src/js/` would be the review failure this avoids.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_scheme_host_and_port() {
        assert_eq!(
            origin_of("https://example.com/a/b?c#d").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            origin_of("http://example.com:8080/x").as_deref(),
            Some("http://example.com:8080")
        );
        // A different scheme or port is a different origin, which is the whole
        // point of the concept.
        assert_ne!(
            origin_of("https://example.com/"),
            origin_of("http://example.com/")
        );
        assert_ne!(
            origin_of("https://example.com/"),
            origin_of("https://other.example.com/")
        );
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn two_origins_never_see_each_others_data() {
        let storage = Storage::new();
        assert!(storage.set("https://a.test", Area::Local, "k", "from a"));
        assert!(storage.set("https://b.test", Area::Local, "k", "from b"));

        assert_eq!(
            storage.get("https://a.test", Area::Local, "k").as_deref(),
            Some("from a")
        );
        assert_eq!(
            storage.get("https://b.test", Area::Local, "k").as_deref(),
            Some("from b")
        );
        storage.clear("https://a.test", Area::Local);
        assert_eq!(storage.get("https://a.test", Area::Local, "k"), None);
        assert_eq!(
            storage.get("https://b.test", Area::Local, "k").as_deref(),
            Some("from b"),
            "clearing one origin emptied another"
        );
    }

    #[test]
    fn local_and_session_are_separate_areas() {
        let storage = Storage::new();
        storage.set("https://a.test", Area::Local, "k", "local");
        storage.set("https://a.test", Area::Session, "k", "session");
        assert_eq!(
            storage.get("https://a.test", Area::Local, "k").as_deref(),
            Some("local")
        );
        assert_eq!(
            storage.get("https://a.test", Area::Session, "k").as_deref(),
            Some("session")
        );
    }

    #[test]
    fn a_forked_tab_shares_local_and_isolates_session_storage() {
        let first = Storage::new();
        first.set("https://a.test", Area::Local, "shared", "one");
        first.set("https://a.test", Area::Session, "private", "first");
        let second = first.fork_tab();

        assert_eq!(
            second
                .get("https://a.test", Area::Local, "shared")
                .as_deref(),
            Some("one")
        );
        assert_eq!(second.get("https://a.test", Area::Session, "private"), None);
        second.set("https://a.test", Area::Local, "shared", "two");
        second.set("https://a.test", Area::Session, "private", "second");
        assert_eq!(
            first
                .get("https://a.test", Area::Local, "shared")
                .as_deref(),
            Some("two")
        );
        assert_eq!(
            first
                .get("https://a.test", Area::Session, "private")
                .as_deref(),
            Some("first")
        );
    }

    #[test]
    fn the_quota_refuses_a_write_that_would_exceed_it() {
        let storage = Storage::new();
        let big = "x".repeat(MAX_BYTES);
        assert!(!storage.set("https://a.test", Area::Local, "k", &big));
        assert_eq!(storage.len("https://a.test", Area::Local), 0);

        // Just under fits, and replacing it with something smaller is never
        // refused for its own size.
        let fits = "x".repeat(MAX_BYTES - 16);
        assert!(storage.set("https://a.test", Area::Local, "k", &fits));
        assert!(storage.set("https://a.test", Area::Local, "k", "small"));
        // A second key that would push the total over is refused.
        assert!(storage.set("https://a.test", Area::Local, "big", &fits));
        assert!(!storage.set("https://a.test", Area::Local, "more", &fits));
    }

    #[test]
    fn keys_are_enumerable_in_a_stable_order() {
        let storage = Storage::new();
        for key in ["c", "a", "b"] {
            storage.set("https://a.test", Area::Local, key, "v");
        }
        assert_eq!(storage.len("https://a.test", Area::Local), 3);
        let keys: Vec<String> = (0..3)
            .filter_map(|i| storage.key_at("https://a.test", Area::Local, i))
            .collect();
        assert_eq!(keys, ["a", "b", "c"]);
        assert_eq!(storage.key_at("https://a.test", Area::Local, 3), None);
    }

    #[test]
    fn removing_and_clearing_do_what_they_say() {
        let storage = Storage::new();
        storage.set("https://a.test", Area::Local, "k", "v");
        storage.set("https://a.test", Area::Local, "j", "w");
        storage.remove("https://a.test", Area::Local, "k");
        assert_eq!(storage.get("https://a.test", Area::Local, "k"), None);
        assert_eq!(storage.len("https://a.test", Area::Local), 1);
        storage.clear("https://a.test", Area::Local);
        assert_eq!(storage.len("https://a.test", Area::Local), 0);
    }
}
