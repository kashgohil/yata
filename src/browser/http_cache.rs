//! Bounded, private cache for top-level document responses.
//!
//! The cache owns bytes and HTTP metadata only. A hit is copied back into the
//! normal `Loaded -> Parsed` path; no page-engine state lives here.

use std::collections::HashMap;
use std::time::Duration;

use crate::net;

pub const DEFAULT_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_ENTRIES: usize = 128;
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_FIELD_BYTES: usize = 8 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;

/// Selected response fields needed by the cache. Field lines stay separate so
/// quoted commas and repeated directives are never changed by folding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub cache_control: Vec<String>,
    pub etag: Option<String>,
    pub age: Option<String>,
    pub vary: Vec<String>,
    /// A non-UTF-8 or over-limit Vary cannot safely be interpreted.
    pub vary_unusable: bool,
    pub over_limit: bool,
}

impl Metadata {
    pub fn retained_size(&self) -> usize {
        self.cache_control
            .iter()
            .chain(self.vary.iter())
            .map(String::len)
            .fold(0usize, usize::saturating_add)
            .saturating_add(self.etag.as_ref().map_or(0, String::len))
            .saturating_add(self.age.as_ref().map_or(0, String::len))
    }

    pub fn bounded(
        cache_control: Vec<String>,
        etag: Option<String>,
        age: Option<String>,
        vary: Vec<String>,
        vary_unusable: bool,
    ) -> Metadata {
        let field_too_large = cache_control
            .iter()
            .chain(vary.iter())
            .any(|v| v.len() > MAX_FIELD_BYTES)
            || etag.as_ref().is_some_and(|v| v.len() > MAX_FIELD_BYTES)
            || age.as_ref().is_some_and(|v| v.len() > MAX_FIELD_BYTES);
        let mut metadata = Metadata {
            cache_control,
            etag,
            age,
            vary,
            vary_unusable,
            over_limit: field_too_large,
        };
        if metadata.retained_size() > MAX_METADATA_BYTES {
            metadata.over_limit = true;
        }
        metadata
    }

    /// Fields present on a 304 replace stored fields; absent fields retain
    /// their prior values.
    pub fn merge_304(&self, update: &Metadata) -> Metadata {
        Metadata {
            cache_control: if update.cache_control.is_empty() {
                self.cache_control.clone()
            } else {
                update.cache_control.clone()
            },
            etag: update.etag.clone().or_else(|| self.etag.clone()),
            age: update.age.clone().or_else(|| self.age.clone()),
            vary: if update.vary.is_empty() {
                self.vary.clone()
            } else {
                update.vary.clone()
            },
            vary_unusable: update.vary_unusable,
            over_limit: self.over_limit || update.over_limit,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    url: String,
    cookie: Option<String>,
}

impl Key {
    pub fn from_request(request: &net::Request) -> Option<Key> {
        if !matches!(
            request.method,
            net::Method::Get | net::Method::Conditional { .. }
        ) {
            return None;
        }
        let mut url = reqwest::Url::parse(&request.url).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        url.set_fragment(None);
        Some(Key {
            url: url.to_string(),
            cookie: request.cookie.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Representation {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub metadata: Metadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestMode {
    Ordinary,
    Reload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    Hit(Representation),
    Revalidate { etag: Option<String> },
    Miss,
}

#[derive(Clone)]
struct Entry {
    response: Representation,
    received: Duration,
    lifetime: Duration,
    no_cache: bool,
    charge: usize,
    used: u64,
}

pub struct Cache {
    entries: HashMap<Key, Entry>,
    byte_cap: usize,
    entry_cap: usize,
    bytes: usize,
    clock: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(DEFAULT_BYTES, DEFAULT_ENTRIES)
    }
}

impl Cache {
    pub fn new(byte_cap: usize, entry_cap: usize) -> Cache {
        Cache {
            entries: HashMap::new(),
            byte_cap,
            entry_cap,
            bytes: 0,
            clock: 0,
        }
    }

    pub fn plan(&mut self, key: &Key, mode: RequestMode, now: Duration) -> Plan {
        let Some(entry) = self.entries.get_mut(key) else {
            return Plan::Miss;
        };
        self.clock = self.clock.saturating_add(1);
        entry.used = self.clock;
        let fresh = !entry.no_cache && now.saturating_sub(entry.received) < entry.lifetime;
        if mode == RequestMode::Ordinary && fresh {
            Plan::Hit(entry.response.clone())
        } else {
            Plan::Revalidate {
                etag: entry.response.metadata.etag.clone(),
            }
        }
    }

    pub fn insert(&mut self, key: Key, response: Representation, now: Duration) -> bool {
        let Some(policy) = policy(&response.metadata) else {
            self.remove(&key);
            return false;
        };
        let charge = key
            .url
            .len()
            .saturating_add(key.cookie.as_ref().map_or(0, String::len))
            .saturating_add(response.body.len())
            .saturating_add(response.content_type.as_ref().map_or(0, String::len))
            .saturating_add(response.metadata.retained_size());
        if response.status != 200
            || response.body.len() > MAX_BODY_BYTES
            || charge > self.byte_cap
            || self.entry_cap == 0
        {
            self.remove(&key);
            return false;
        }
        self.remove(&key);
        self.clock = self.clock.saturating_add(1);
        self.bytes = self.bytes.saturating_add(charge);
        self.entries.insert(
            key,
            Entry {
                response,
                received: now,
                lifetime: policy.lifetime,
                no_cache: policy.no_cache,
                charge,
                used: self.clock,
            },
        );
        self.evict();
        true
    }

    pub fn revalidate(
        &mut self,
        key: &Key,
        metadata: &Metadata,
        now: Duration,
    ) -> Option<Representation> {
        let old = self.entries.get(key)?.response.clone();
        let mut response = old;
        response.metadata = response.metadata.merge_304(metadata);
        if policy(&response.metadata).is_none() {
            self.remove(key);
            return Some(response);
        }
        self.insert(key.clone(), response.clone(), now);
        Some(response)
    }

    pub fn remove(&mut self, key: &Key) {
        if let Some(old) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(old.charge);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }

    fn evict(&mut self) {
        while self.entries.len() > self.entry_cap || self.bytes > self.byte_cap {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
    }
}

#[derive(Clone, Copy)]
struct Policy {
    lifetime: Duration,
    no_cache: bool,
}

fn policy(metadata: &Metadata) -> Option<Policy> {
    if metadata.over_limit || metadata.vary_unusable || !supported_vary(&metadata.vary) {
        return None;
    }
    let mut no_store = false;
    let mut no_cache = false;
    let mut max_ages = Vec::new();
    let mut malformed_max_age = false;
    let mut has_max_age = false;
    for line in &metadata.cache_control {
        for raw in split_directives(line) {
            let (name, value) = raw
                .split_once('=')
                .map_or((raw, None), |(n, v)| (n, Some(v)));
            match name.trim().to_ascii_lowercase().as_str() {
                "no-store" => no_store = true,
                "no-cache" => no_cache = true,
                "max-age" => {
                    has_max_age = true;
                    match value.and_then(parse_seconds) {
                        Some(value) => max_ages.push(value),
                        None => malformed_max_age = true,
                    }
                }
                "must-revalidate" | "private" | "public" => {}
                _ => {}
            }
        }
    }
    if no_store {
        return None;
    }
    let etag_usable = metadata.etag.as_deref().is_some_and(usable_etag);
    if !has_max_age && !etag_usable {
        return None;
    }
    let duplicate = max_ages.len() > 1;
    let max_age = if malformed_max_age || duplicate {
        Duration::ZERO
    } else {
        max_ages.first().copied().unwrap_or(Duration::ZERO)
    };
    let age = metadata
        .age
        .as_deref()
        .and_then(parse_age)
        .unwrap_or(Duration::ZERO);
    Some(Policy {
        lifetime: max_age.saturating_sub(age),
        no_cache,
    })
}

fn usable_etag(tag: &str) -> bool {
    let tag = tag.strip_prefix("W/").unwrap_or(tag);
    tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"')
}

fn split_directives(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&line[start..]);
    out
}

fn parse_seconds(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds = raw.bytes().fold(0u64, |n, b| {
        n.saturating_mul(10).saturating_add((b - b'0') as u64)
    });
    Some(Duration::from_secs(seconds))
}

fn parse_age(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Duration::from_secs(raw.bytes().fold(0u64, |n, b| {
        n.saturating_mul(10).saturating_add((b - b'0') as u64)
    })))
}

fn supported_vary(lines: &[String]) -> bool {
    lines.iter().all(|line| {
        line.split(',').all(|name| {
            matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "accept" | "accept-encoding" | "host" | "user-agent" | "cookie"
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str, cookie: Option<&str>) -> net::Request {
        let mut request = net::Request::bare(url);
        request.cookie = cookie.map(str::to_string);
        request
    }

    fn response(
        control: &[&str],
        etag: Option<&str>,
        age: Option<&str>,
        body: &[u8],
    ) -> Representation {
        Representation {
            status: 200,
            body: body.to_vec(),
            content_type: Some("text/html".into()),
            metadata: Metadata::bounded(
                control.iter().map(|s| s.to_string()).collect(),
                etag.map(str::to_string),
                age.map(str::to_string),
                Vec::new(),
                false,
            ),
        }
    }

    #[test]
    fn keys_drop_fragments_and_include_cookie_state() {
        assert_eq!(
            Key::from_request(&request("https://x.test/a#one", None)),
            Key::from_request(&request("https://x.test/a#two", None))
        );
        assert_ne!(
            Key::from_request(&request("https://x.test/a", None)),
            Key::from_request(&request("https://x.test/a", Some("sid=1")))
        );
        assert_ne!(
            Key::from_request(&request("https://x.test/a", Some(""))),
            Key::from_request(&request("https://x.test/a", None))
        );
    }

    #[test]
    fn directives_are_case_insensitive_quoted_and_conservative() {
        let now = Duration::from_secs(100);
        for control in [
            vec![" MAX-AGE = \"60\" "],
            vec!["private, max-age=60", "must-revalidate"],
            vec!["public, unknown=x, MaX-aGe=60"],
        ] {
            let mut cache = Cache::new(1024, 4);
            let key = Key::from_request(&request("https://x.test/", None)).unwrap();
            assert!(cache.insert(key.clone(), response(&control, None, None, b"x"), now));
            assert!(matches!(
                cache.plan(&key, RequestMode::Ordinary, now + Duration::from_secs(59)),
                Plan::Hit(_)
            ));
            assert_eq!(
                cache.plan(&key, RequestMode::Ordinary, now + Duration::from_secs(60)),
                Plan::Revalidate { etag: None }
            );
        }

        for control in [
            vec!["max-age=nope"],
            vec!["max-age=60, max-age=61"],
            vec!["max-age=60, max-age"],
        ] {
            let mut cache = Cache::new(1024, 4);
            let key = Key::from_request(&request("https://x.test/", None)).unwrap();
            assert!(cache.insert(key.clone(), response(&control, None, None, b"x"), now));
            assert!(matches!(
                cache.plan(&key, RequestMode::Ordinary, now),
                Plan::Revalidate { .. }
            ));
        }
    }

    #[test]
    fn no_store_dominates_and_no_cache_always_validates() {
        let key = Key::from_request(&request("https://x.test/", None)).unwrap();
        let mut cache = Cache::new(1024, 4);
        assert!(!cache.insert(
            key.clone(),
            response(&["max-age=60, no-store"], Some("\"x\""), None, b"x"),
            Duration::ZERO
        ));
        assert!(cache.insert(
            key.clone(),
            response(&["max-age=60, no-cache"], Some("W/\"x\""), None, b"x"),
            Duration::ZERO
        ));
        assert_eq!(
            cache.plan(&key, RequestMode::Ordinary, Duration::ZERO),
            Plan::Revalidate {
                etag: Some("W/\"x\"".into())
            }
        );
    }

    #[test]
    fn age_reduces_lifetime_and_invalid_age_is_zero() {
        let key = Key::from_request(&request("https://x.test/", None)).unwrap();
        let mut cache = Cache::new(1024, 4);
        assert!(cache.insert(
            key.clone(),
            response(&["max-age=60"], None, Some("20"), b"x"),
            Duration::from_secs(5)
        ));
        assert!(matches!(
            cache.plan(&key, RequestMode::Ordinary, Duration::from_secs(44)),
            Plan::Hit(_)
        ));
        assert!(matches!(
            cache.plan(&key, RequestMode::Ordinary, Duration::from_secs(45)),
            Plan::Revalidate { .. }
        ));
        assert!(cache.insert(
            key.clone(),
            response(&["max-age=60"], None, Some("bad"), b"x"),
            Duration::from_secs(100)
        ));
        assert!(matches!(
            cache.plan(&key, RequestMode::Ordinary, Duration::from_secs(159)),
            Plan::Hit(_)
        ));
    }

    #[test]
    fn etag_only_is_retained_stale_and_reload_forces_validation() {
        let key = Key::from_request(&request("https://x.test/", None)).unwrap();
        let mut cache = Cache::new(1024, 4);
        assert!(cache.insert(
            key.clone(),
            response(&[], Some("\"exact\""), None, b"x"),
            Duration::ZERO
        ));
        assert_eq!(
            cache.plan(&key, RequestMode::Ordinary, Duration::ZERO),
            Plan::Revalidate {
                etag: Some("\"exact\"".into())
            }
        );
        assert!(cache.insert(
            key.clone(),
            response(&["max-age=60"], Some("W/\"weak\""), None, b"x"),
            Duration::ZERO
        ));
        assert_eq!(
            cache.plan(&key, RequestMode::Reload, Duration::from_secs(1)),
            Plan::Revalidate {
                etag: Some("W/\"weak\"".into())
            }
        );
    }

    #[test]
    fn vary_refuses_star_unknown_and_unusable_values() {
        for (vary, unusable) in [
            (vec!["*"], false),
            (vec!["X-Theme"], false),
            (vec!["Cookie, X-Theme"], false),
            (vec!["Cookie"], true),
        ] {
            let key = Key::from_request(&request("https://x.test/", None)).unwrap();
            let mut r = response(&["max-age=60"], None, None, b"x");
            r.metadata.vary = vary.into_iter().map(str::to_string).collect();
            r.metadata.vary_unusable = unusable;
            assert!(!Cache::new(1024, 4).insert(key, r, Duration::ZERO));
        }
    }

    #[test]
    fn replacement_accounting_and_lru_refresh_obey_both_caps() {
        let mut cache = Cache::new(170, 2);
        let a = Key::from_request(&request("https://x.test/a", None)).unwrap();
        let b = Key::from_request(&request("https://x.test/b", None)).unwrap();
        let c = Key::from_request(&request("https://x.test/c", None)).unwrap();
        assert!(cache.insert(
            a.clone(),
            response(&["max-age=60"], None, None, &[b'a'; 20]),
            Duration::ZERO
        ));
        let first_charge = cache.bytes();
        assert!(cache.insert(
            a.clone(),
            response(&["max-age=60"], None, None, &[b'a'; 30]),
            Duration::ZERO
        ));
        assert!(cache.bytes() > first_charge);
        assert_eq!(cache.len(), 1);
        assert!(cache.insert(
            b.clone(),
            response(&["max-age=60"], None, None, b"b"),
            Duration::ZERO
        ));
        assert!(matches!(
            cache.plan(&a, RequestMode::Ordinary, Duration::ZERO),
            Plan::Hit(_)
        ));
        assert!(cache.insert(
            c.clone(),
            response(&["max-age=60"], None, None, b"c"),
            Duration::ZERO
        ));
        assert_eq!(cache.len(), 2);
        assert!(matches!(
            cache.plan(&b, RequestMode::Ordinary, Duration::ZERO),
            Plan::Miss
        ));
        assert!(cache.bytes() <= 170);
    }

    #[test]
    fn oversized_body_is_delivered_but_not_stored() {
        let key = Key::from_request(&request("https://x.test/", None)).unwrap();
        assert!(!Cache::new(MAX_BODY_BYTES + 1024, 2).insert(
            key,
            response(&["max-age=60"], None, None, &vec![0; MAX_BODY_BYTES + 1]),
            Duration::ZERO,
        ));
    }
}
