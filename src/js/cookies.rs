//! The cookie jar behind `document.cookie` (M11.6).
//!
//! ## Nothing is written to disk, and that is the same decision twice
//!
//! `js::storage` made this call for `localStorage`; a jar re-makes it for the
//! same two reasons, and one more:
//!
//! - CLAUDE.md's UI thread does no disk I/O, and a cookie write is a
//!   synchronous assignment inside a script tick. Durability would mean either
//!   blocking the loop on a write or inventing a persistence worker with its
//!   own consistency story.
//! - A jar that survives restarts *is* the tracking surface a browser for
//!   reading does not need — the same instinct as `fetch()` refusing
//!   cross-origin requests rather than implementing CORS.
//! - And a cookie is a credential, not just data. A file of them on disk is a
//!   thing to protect; a jar that dies with the process is not.
//!
//! So a "persistent" cookie — one with `Expires` in 2030 — lives until the
//! process exits, and no longer. That is a real deviation with a real trigger
//! (a page that expects to still be logged in tomorrow), deposited for the
//! M11.25 register.
//!
//! ## Scoped per host, not per origin
//!
//! `Storage` is keyed by origin (scheme, host, port). A cookie is not: its
//! scope is a host and a path, which is why `Secure` has to exist at all — it
//! is the flag that keeps an `https` cookie off an `http` page, because the
//! scope itself does not. So this jar is keyed by host, honours `Secure`, and
//! ignores the port, exactly as the web does.
//!
//! What it does **not** honour is `Domain`: every cookie here belongs to
//! exactly one host, and a cookie set by `en.wikipedia.org` is invisible to
//! `wikipedia.org` and to `www.en.wikipedia.org`. There is no public suffix
//! list in this repo — adding one is a dependency and a data file — so a
//! `Domain` implementation could not tell `example.co.uk` from `co.uk`, and
//! would let a page write a cookie for a whole country's registry. And a
//! superdomain cookie is the cross-origin case this milestone refuses on
//! purpose: **cookies never cross an origin**, not on the wire (M11.7), not
//! through `Domain`, not through a subdomain. The cost is a site that logs you
//! in on `example.com` and expects the session on `www.example.com`; that is
//! for the register too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::js::console::{self, Console};
use crate::js::storage::origin_of;

/// The most a single cookie's name and value may weigh. Browsers land at
/// 4 KB and pages are written against that number; the point of having one is
/// that a script cannot grow the jar until PLAN.md §4's 100 MB page budget is
/// gone.
pub const MAX_COOKIE_BYTES: usize = 4096;

/// How many cookies one host may keep. Browsers allow around 50 per host, so
/// 4 KB × 50 bounds a host at ~200 KB.
///
/// A page that reaches the cap has its **write refused**, and the jar evicts
/// nothing — and M11.7 re-made that decision with a server on the other end,
/// where a `Set-Cookie` can be refused too. Eviction is the other defensible
/// answer and browsers pick it, but it hands a page a way to push somebody
/// else's cookie out of the jar by writing 50 of its own, and "somebody else"
/// is now the server's `HttpOnly` session cookie — the one thing the flag
/// exists to keep a script's hands off. Refusing costs the page that filled
/// the jar; evicting would cost the reader their session. The refusal is a
/// console line, never an exception (see `Reject`).
pub const MAX_PER_HOST: usize = 50;

/// Seconds since the Unix epoch. Signed, because a deletion is an expiry in
/// the past and `Max-Age=-1` has to be representable.
pub type Secs = i64;

/// The wall clock, read at the call site and passed *in* to every jar method.
///
/// The jar never asks the time itself: `Expires` is absolute, so a jar that
/// read the clock inside itself would be a jar whose expiry rules could only
/// be tested by sleeping. Two callers read this (the getter and the setter);
/// every test passes its own number.
pub fn now() -> Secs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as Secs)
}

/// `SameSite`, parsed and stored and nothing else. With no cross-origin
/// request carrying cookies at all (M10.12 refuses cross-origin `fetch`, and
/// M11.7 sends cookies same-origin only), there is nothing for it to relax —
/// but a page that sets it must not have its cookie rejected for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SameSite {
    /// The attribute was absent or unrecognised.
    #[default]
    Unspecified,
    Lax,
    Strict,
    None,
}

/// Who is asking the jar for cookies.
///
/// The **only** thing the two readers differ on, and the whole point of
/// `HttpOnly`: the wire gets those cookies, `document.cookie` does not.
/// Expiry, `Path`, `Secure` and the host are identical for both, which is why
/// they are decided once (`in_scope`, and `Jar::live` above it) rather than
/// copied per reader.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reach {
    /// `document.cookie`.
    Script,
    /// A `Cookie:` header on a request this engine is about to make.
    Wire,
}

/// One cookie: what it is called, what it says, and every attribute that
/// decides who may see it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// The one host this cookie belongs to. There is no `Domain` field
    /// because there is no `Domain` behaviour — see the module comment.
    pub host: String,
    /// Always starts with `/`; the default is derived from the page's path.
    pub path: String,
    /// `None` is a session cookie: it lives as long as the process does, which
    /// here is also as long as any cookie lives.
    pub expires: Option<Secs>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

impl Cookie {
    fn expired(&self, now: Secs) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }
}

/// Why a `Set-Cookie`-shaped string did not become a cookie. Every one of
/// these is a no-op plus a console line — a page that writes rubbish to
/// `document.cookie` keeps rendering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reject {
    /// No `=` anywhere, so there is no name-value pair to store.
    NoPair,
    /// `=value`, or a name that is only whitespace.
    EmptyName,
    /// A control character in the name or the value. Rejected here rather than
    /// escaped later: this string goes into a `Cookie:` header (M11.7), and a
    /// newline in it there is header injection.
    ControlCharacter,
    /// Over `MAX_COOKIE_BYTES`.
    TooLarge,
    /// `HttpOnly` is a flag only a server can set — a script setting one would
    /// be a script hiding a cookie from itself.
    HttpOnlyFromScript,
    /// The jar already holds an `HttpOnly` cookie by this name: invisible to
    /// script when read, and unwritable by script too, or the flag would only
    /// protect the value and not the slot.
    HttpOnlyExists,
    /// `Secure` from an `http:` page: the cookie would be readable over a
    /// connection that cannot keep it.
    SecureFromInsecurePage,
    /// `MAX_PER_HOST` reached.
    JarFull,
}

impl Reject {
    /// The half-sentence the console line ends with. The two that quote a cap
    /// read it from the constant rather than repeating the number, because a
    /// message that drifts from the rule it explains is worse than none.
    pub fn message(self) -> String {
        match self {
            Reject::NoPair => "it has no name=value pair".to_string(),
            Reject::EmptyName => "its name is empty".to_string(),
            Reject::ControlCharacter => {
                "its name or value contains a control character".to_string()
            }
            Reject::TooLarge => format!("it is larger than {MAX_COOKIE_BYTES} bytes"),
            Reject::HttpOnlyFromScript => "a script cannot set an HttpOnly cookie".to_string(),
            Reject::HttpOnlyExists => "an HttpOnly cookie of that name already exists".to_string(),
            Reject::SecureFromInsecurePage => {
                "a Secure cookie cannot be set from an http: page".to_string()
            }
            Reject::JarFull => format!("this host already has {MAX_PER_HOST} cookies"),
        }
    }
}

/// What the page a script is running in says about who may see a cookie: its
/// host, its path, and whether the connection was secure.
///
/// Derived in Rust from the URL `net::` produced, never in JS — a page that
/// could name its own host could read anyone's cookies.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Scope {
    pub host: String,
    /// The page's own path, which is what a cookie's `Path` is matched
    /// against. (The *default* path for a cookie that names none is derived
    /// from this; see `default_path`.)
    pub path: String,
    pub secure: bool,
}

impl Scope {
    /// The scope of a page URL, or `None` when there is no host to scope to —
    /// a `file:` page, or a dump with nothing to resolve against. No host, no
    /// cookies: the getter answers `""` and the setter does nothing.
    ///
    /// Parsed by the URL parser the rest of the engine uses; a second one in
    /// `src/js/` would be the review failure this avoids.
    pub fn of(url: &str) -> Option<Scope> {
        let parsed = reqwest::Url::parse(url).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        Some(Scope {
            host,
            path: parsed.path().to_string(),
            secure: parsed.scheme() == "https",
        })
    }
}

/// Every host's cookies for this session, shared with the binding closures.
///
/// Lives in `App` beside `Storage`, not in the host: a host is dropped on
/// every navigation and the jar must not be.
#[derive(Clone, Default)]
pub struct Jar {
    hosts: Rc<RefCell<Inner>>,
}

#[derive(Default)]
struct Inner {
    hosts: HashMap<String, Vec<Entry>>,
    /// Creation order, jar-wide. `document.cookie`'s order is longest path
    /// first and, for equal paths, oldest first — so something has to remember
    /// which cookie is older than which.
    next: u64,
}

struct Entry {
    cookie: Cookie,
    created: u64,
}

impl Jar {
    pub fn new() -> Jar {
        Jar::default()
    }

    /// What `document.cookie` returns: `name=value` pairs joined with `"; "`,
    /// and `""` when there are none — never an exception, because pages call
    /// `.match` on the result and an empty string is load-bearing.
    ///
    /// Left out: `HttpOnly` cookies (the entire point of the flag), expired
    /// ones, ones whose `Path` does not match this page's, `Secure` ones on an
    /// insecure page, and every cookie belonging to another host.
    ///
    /// **The order is part of the API**, because a page parsing this with a
    /// regex depends on it: longest `Path` first, and for equal paths the
    /// oldest first. That is what browsers do and what RFC 6265 §5.4 asks for.
    pub fn read_for_script(&self, scope: &Scope, now: Secs) -> String {
        self.pairs(scope, now, Reach::Script)
    }

    /// Every cookie in scope, as `name=value` joined with `"; "` — the one
    /// serializer both readers use, so the `Cookie:` header a request carries
    /// and the string a script reads cannot come out in different orders.
    fn pairs(&self, scope: &Scope, now: Secs, reach: Reach) -> String {
        // `(path length, creation order, pair)`: the sort key is copied out
        // during the walk because the borrow of the jar ends with it.
        let mut visible: Vec<(usize, u64, String)> = self.live(&scope.host, now, |entries| {
            entries
                .filter(|entry| in_scope(&entry.cookie, scope, reach))
                .map(|entry| {
                    (
                        entry.cookie.path.len(),
                        entry.created,
                        format!("{}={}", entry.cookie.name, entry.cookie.value),
                    )
                })
                .collect()
        });
        visible.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        visible
            .into_iter()
            .map(|(_, _, pair)| pair)
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The one walk over a host's cookies, and the one place expiry is
    /// applied. Every reader goes through it — the getter, the wire, `holds`
    /// and `all` — which is what stops the next one from forgetting that an
    /// expired cookie is *gone*. M11.6's review found exactly that bug: `holds`
    /// kept its own idea of "is there a cookie here" and a long-dead `HttpOnly`
    /// cookie went on refusing a page's write while the getter reported the
    /// slot empty.
    ///
    /// A closure rather than a returned iterator because the entries live
    /// behind a `RefCell`, and the borrow may not outlive the call.
    fn live<R>(&self, host: &str, now: Secs, read: impl FnOnce(Live<'_>) -> R) -> R {
        let inner = self.hosts.borrow();
        read(Live {
            entries: inner
                .hosts
                .get(host)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter(),
            now,
        })
    }

    /// One assignment, one cookie: `document.cookie = "a=1; path=/"` adds or
    /// replaces exactly that cookie and leaves the rest of the jar alone. It
    /// looks like a string property and is not one.
    pub fn write_from_script(
        &self,
        assignment: &str,
        scope: &Scope,
        now: Secs,
    ) -> Result<(), Reject> {
        let cookie = parse(assignment, &scope.host, &scope.path, now)?;
        if cookie.http_only {
            return Err(Reject::HttpOnlyFromScript);
        }
        if cookie.secure && !scope.secure {
            return Err(Reject::SecureFromInsecurePage);
        }
        // The flag protects the *slot*, not just the value: a script that could
        // overwrite an `HttpOnly` cookie could delete the session and let the
        // server mint a fresh one, which is the attack the flag exists to stop.
        if self.holds(&cookie, now, |existing| existing.http_only) {
            return Err(Reject::HttpOnlyExists);
        }
        // And a `Secure` cookie already in the jar is not overwritable from an
        // insecure page either, for the same reason a script there cannot read
        // it: the http page has no business touching it.
        if !scope.secure && self.holds(&cookie, now, |existing| existing.secure) {
            return Err(Reject::SecureFromInsecurePage);
        }
        self.insert(cookie, now)
    }

    /// Put a parsed cookie in the jar — the storage half of the setter, with
    /// none of the rules about who is allowed to ask.
    ///
    /// Replacing keeps the original's creation order, as browsers do: a page
    /// that rewrites a cookie's value has not made it the newest one.
    /// An expiry at or before `now` is a **deletion** — that is how a page
    /// removes a cookie, and there is no other way.
    pub fn insert(&self, cookie: Cookie, now: Secs) -> Result<(), Reject> {
        let mut inner = self.hosts.borrow_mut();
        let created = inner.next;
        let mut used = false;
        let entries = inner.hosts.entry(cookie.host.clone()).or_default();

        // Expired cookies are dropped here rather than by a sweeper: the jar
        // is touched only from a script tick, so this is the only moment the
        // count cap could be wrong.
        entries.retain(|entry| !entry.cookie.expired(now));

        let same = entries
            .iter()
            .position(|entry| entry.cookie.name == cookie.name && entry.cookie.path == cookie.path);
        if cookie.expired(now) {
            if let Some(index) = same {
                entries.remove(index);
            }
            return Ok(());
        }
        match same {
            Some(index) => entries[index].cookie = cookie,
            None => {
                if entries.len() >= MAX_PER_HOST {
                    return Err(Reject::JarFull);
                }
                entries.push(Entry { cookie, created });
                used = true;
            }
        }
        if used {
            inner.next += 1;
        }
        Ok(())
    }

    /// Whether the slot this cookie would take is already held by a live one
    /// matching `predicate`. Only the flags a script is not allowed to walk
    /// over are asked about.
    ///
    /// `now` is not decoration: an **expired** cookie holds nothing. It comes
    /// from `live`, which is the point — this reader asks about a *slot*
    /// (name and exact path) rather than about visibility, so it shares the
    /// expiry rule and nothing else, and it shares it by calling it rather
    /// than by repeating it.
    fn holds(&self, cookie: &Cookie, now: Secs, predicate: impl Fn(&Cookie) -> bool) -> bool {
        self.live(&cookie.host, now, |mut entries| {
            entries.any(|entry| {
                entry.cookie.name == cookie.name
                    && entry.cookie.path == cookie.path
                    && predicate(&entry.cookie)
            })
        })
    }

    /// Every unexpired cookie a host holds, in creation order — including the
    /// `HttpOnly` ones the getter hides, which is why no page-facing path may
    /// reach it. Tests are its only caller: the `Cookie:` header reads through
    /// `pairs` instead, so that it cannot order or filter differently from the
    /// getter it is supposed to differ from in exactly one way.
    #[cfg(test)]
    pub fn all(&self, host: &str, now: Secs) -> Vec<Cookie> {
        self.live(host, now, |entries| {
            entries.map(|entry| entry.cookie.clone()).collect()
        })
    }
}

/// One host's unexpired cookies, in creation order — the only view of the jar
/// there is. A named type rather than an iterator chain so that every reader
/// receives the *same* one: adding a second way in is how the expiry rule got
/// forgotten once already.
struct Live<'a> {
    entries: std::slice::Iter<'a, Entry>,
    now: Secs,
}

impl<'a> Iterator for Live<'a> {
    type Item = &'a Entry;

    fn next(&mut self) -> Option<&'a Entry> {
        let now = self.now;
        self.entries.find(|entry| !entry.cookie.expired(now))
    }
}

/// Whether a live cookie is *this* reader's to see: the second half of the
/// filter, applied after `Jar::live` has ruled out the expired ones.
///
/// `Reach` is the only parameter, and the only way the wire differs from
/// `document.cookie`. Everything else here is shared on purpose.
fn in_scope(cookie: &Cookie, scope: &Scope, reach: Reach) -> bool {
    (reach == Reach::Wire || !cookie.http_only)
        && (!cookie.secure || scope.secure)
        && path_matches(&cookie.path, &scope.path)
}

/// The `Cookie:` header value for one request, or `None` when it carries
/// none. **The one function every request in this engine asks**, and the
/// reason it is one: there are five request paths, the rule is the same on all
/// five, and `if same_origin` written out five times is five chances to be
/// wrong.
///
/// `None` in three cases, which are one case: the request is not the page's
/// own origin, either side has no origin to compare (a `file:` page, a URL
/// that will not parse), or the jar holds nothing that matches. A request
/// carries cookies only when its origin *is* the page's — the ladder makes a
/// genuinely cross-origin request (M11.5's second rung pulls a script from
/// `www.google-analytics.com`), and it gets nothing.
///
/// It differs from `document.cookie` in exactly one way, which is the point of
/// `HttpOnly`: **the wire gets the cookies the script cannot see.** Expiry,
/// `Path`, `Secure` and the host are the same rules, from the same code.
///
/// The jar never leaves the UI thread — it is `Rc<RefCell<…>>` and so `!Send`,
/// which is not discipline but a compile error. This is where it becomes a
/// plain `String` a worker can hold.
pub fn header_for(jar: &Jar, page_url: &str, request_url: &str, now: Secs) -> Option<String> {
    if origin_of(request_url)? != origin_of(page_url)? {
        return None;
    }
    // The *request's* scope, not the page's: a stylesheet at `/static/x.css`
    // is matched against `/static`, and a page at `/docs/page` is not.
    let scope = Scope::of(request_url)?;
    let pairs = jar.pairs(&scope, now, Reach::Wire);
    (!pairs.is_empty()).then_some(pairs)
}

/// Apply a response's `Set-Cookie` lines to the jar, scoped to the URL the
/// response actually came from (post-redirect).
///
/// The server's half of the setter, and it is `Jar::insert` rather than
/// `write_from_script` for one reason: a server may set `HttpOnly` and
/// `Secure`, and a script may not. Everything a script is refused for — the
/// caps, the control characters, the missing `=` — still applies, because
/// those are properties of the cookie and not of who asked.
///
/// A malformed line is a dropped cookie and a console line, never a failed
/// navigation. A response that tries to set more than `MAX_PER_HOST` hits
/// M11.6's refusal, deliberately re-made with a server on the other end: the
/// alternative is eviction, and eviction would let a page push a server's
/// `HttpOnly` session cookie out of the jar by writing fifty of its own —
/// exactly what the flag exists to prevent. A refused cookie costs the page
/// that filled the jar; an evicted one costs the reader their session.
///
/// Takes the `Console` rather than returning messages: both callers (`App` and
/// the headless dumps) would otherwise format the same line, and a warning
/// that drifts between the TUI and `--dump-js` is a warning that lies about
/// one of them.
pub fn apply_set_cookie(jar: &Jar, url: &str, lines: &[String], now: Secs, console: &Console) {
    // No host, no cookies — the same answer `Scope::of` gives the getter.
    let Some(scope) = Scope::of(url) else {
        return;
    };
    for line in lines {
        if let Err(reject) =
            parse(line, &scope.host, &scope.path, now).and_then(|cookie| jar.insert(cookie, now))
        {
            console.push(
                console::Level::Warn,
                None,
                None,
                &format!("ignored Set-Cookie: {line:?}: {}", reject.message()),
            );
        }
    }
}

/// RFC 6265 §5.1.4: a cookie's `Path` matches a request path when it is the
/// same, or a prefix ending at a `/` boundary. `/foo` matches `/foo` and
/// `/foo/bar` but never `/foobar`.
fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path[cookie_path.len()..].starts_with('/')
}

/// RFC 6265 §5.1.4's default-path: the page's directory. `/a/b` defaults to
/// `/a`, `/a` and `/` both default to `/`.
fn default_path(page_path: &str) -> String {
    if !page_path.starts_with('/') {
        return "/".to_string();
    }
    match page_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(cut) => page_path[..cut].to_string(),
    }
}

/// One `Set-Cookie`-shaped string in, one cookie or one rejection out.
///
/// Written once here and used by both setters — a script's assignment and a
/// response's `Set-Cookie` (`apply_set_cookie`) — which is why it takes a host
/// and a path rather than reaching for a page: the two callers differ only in
/// what they are allowed to do with the result.
///
/// `Path`, `Expires`, `Max-Age`, `Secure`, `HttpOnly`, `SameSite` and `Domain`
/// in any order and any case; unknown attributes are skipped rather than
/// fatal, because the attribute list grows and a page must not break for
/// naming something we have not heard of. `Max-Age` beats `Expires`
/// (RFC 6265 §5.2.2), and `Domain` is parsed only so that ignoring it is
/// deliberate.
pub fn parse(header: &str, host: &str, page_path: &str, now: Secs) -> Result<Cookie, Reject> {
    let (pair, attributes) = match header.find(';') {
        Some(cut) => (&header[..cut], &header[cut + 1..]),
        None => (header, ""),
    };
    let (name, value) = pair.split_once('=').ok_or(Reject::NoPair)?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return Err(Reject::EmptyName);
    }
    if name.len() + value.len() > MAX_COOKIE_BYTES {
        return Err(Reject::TooLarge);
    }
    if [name, value]
        .iter()
        .any(|part| part.contains(char::is_control))
    {
        return Err(Reject::ControlCharacter);
    }

    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.to_string(),
        host: host.to_ascii_lowercase(),
        path: default_path(page_path),
        expires: None,
        secure: false,
        http_only: false,
        same_site: SameSite::Unspecified,
    };

    // `Max-Age` wins over `Expires` however they are ordered, so it is tracked
    // separately and applied last.
    let mut max_age: Option<Secs> = None;
    for attribute in attributes.split(';') {
        let (key, argument) = match attribute.split_once('=') {
            Some((key, argument)) => (key.trim(), argument.trim()),
            None => (attribute.trim(), ""),
        };
        // A repeated attribute is last-wins, which falls out of assigning.
        match key.to_ascii_lowercase().as_str() {
            "path" if argument.starts_with('/') => cookie.path = argument.to_string(),
            // A `Path` that is not absolute is not a path; the default stands.
            "path" => {}
            "expires" => cookie.expires = parse_expires(argument).or(cookie.expires),
            "max-age" => max_age = parse_max_age(argument).or(max_age),
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "samesite" => {
                cookie.same_site = match argument.to_ascii_lowercase().as_str() {
                    "lax" => SameSite::Lax,
                    "strict" => SameSite::Strict,
                    "none" => SameSite::None,
                    _ => SameSite::Unspecified,
                }
            }
            // Parsed, and *ignored*: see the module comment. Not silently —
            // the attribute is recognised, so nothing here mistakes it for a
            // typo, and the cookie stays host-only.
            "domain" => {}
            _ => {}
        }
    }
    if let Some(age) = max_age {
        // A zero or negative `Max-Age` is an expiry in the past, which is how
        // deletion works. `saturating_add` because a page may name a number
        // that would overflow the epoch.
        cookie.expires = Some(if age <= 0 {
            now - 1
        } else {
            now.saturating_add(age)
        });
    }
    Ok(cookie)
}

/// `Max-Age`, in seconds. RFC 6265 §5.2.2: a value that does not start with a
/// digit or `-` is not a `Max-Age` at all.
fn parse_max_age(argument: &str) -> Option<Secs> {
    let mut chars = argument.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() || c == '-' => {}
        _ => return None,
    }
    argument.parse::<Secs>().ok()
}

/// `Expires`, by hand, per RFC 6265 §5.1.1 — the three date shapes the web
/// actually uses (`Sun, 06 Nov 1994 08:49:37 GMT`, `Sunday, 06-Nov-94
/// 08:49:37 GMT`, `Sun Nov  6 08:49:37 1994`) fall out of one tokenizer, and
/// `chrono`/`time` are not on CLAUDE.md's allowed list for a reason: a date
/// parser is not a pipeline stage worth importing.
///
/// Unparseable returns `None`, which leaves the cookie a session cookie rather
/// than rejecting it — a browser does the same, and a bad `Expires` is not the
/// page's fault to lose a cookie over.
fn parse_expires(argument: &str) -> Option<Secs> {
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);

    // The RFC's delimiter set is "everything except digits, letters and `:`",
    // which is what makes one tokenizer cover all three shapes: `06-Nov-94`
    // and `06 Nov 1994` tokenize identically.
    for token in argument.split(|c: char| !c.is_ascii_alphanumeric() && c != ':') {
        if token.is_empty() {
            continue;
        }
        // The RFC's order matters: a token is tried as a time, then a day,
        // then a month, then a year, and the first field it fits claims it.
        // That is what makes `06 Nov 1994` and `Nov 6 1994` agree.
        if let (None, Some(parsed)) = (time, parse_time(token)) {
            time = Some(parsed);
            continue;
        }
        if let (None, Some(parsed)) = (day, leading_number(token, 1, 2)) {
            day = Some(parsed);
            continue;
        }
        if let (None, Some(parsed)) = (month, parse_month(token)) {
            month = Some(parsed);
            continue;
        }
        if let (None, Some(parsed)) = (year, leading_number(token, 2, 4)) {
            year = Some(parsed);
            continue;
        }
    }

    let ((hour, minute, second), day, month, mut year) = (time?, day?, month?, year?);
    // Two-digit years, as the RFC maps them: 70–99 are the 1900s and 0–69 are
    // the 2000s, so `69` is 2069 rather than the year the cookie meant.
    if (70..=99).contains(&year) {
        year += 1900;
    } else if (0..=69).contains(&year) {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// `h:m:s`, one or two digits each, with anything after the seconds ignored —
/// the RFC's time production.
fn parse_time(token: &str) -> Option<(Secs, Secs, Secs)> {
    let mut parts = token.split(':');
    let hour = leading_number(parts.next()?, 1, 2)?;
    let minute = leading_number(parts.next()?, 1, 2)?;
    let second = leading_number(parts.next()?, 1, 2)?;
    if parts.next().is_some() {
        return None;
    }
    Some((hour, minute, second))
}

/// The number a token *starts* with, when it has between `min` and `max`
/// leading digits. `1994abc` is a year; `abc` is not.
fn leading_number(token: &str, min: usize, max: usize) -> Option<Secs> {
    let digits: String = token.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < min || digits.len() > max {
        return None;
    }
    digits.parse().ok()
}

/// A month from the first three letters of its name, case-insensitively.
fn parse_month(token: &str) -> Option<Secs> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let head: String = token.chars().take(3).flat_map(char::to_lowercase).collect();
    if head.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|month| *month == head)
        .map(|index| index as Secs + 1)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Integer arithmetic only, and the only calendar maths
/// this repo needs — which is why it is nine lines here rather than a crate.
fn days_from_civil(year: Secs, month: Secs, day: Secs) -> Secs {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_position = (month + 9) % 12;
    let day_of_year = (153 * month_position + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "now" every test measures from: 2026-01-01T00:00:00Z. Nothing
    /// here reads the clock, so nothing here sleeps.
    const NOW: Secs = 1_767_225_600;

    fn scope(url: &str) -> Scope {
        Scope::of(url).expect("a page URL with a host")
    }

    /// The parse, with the fixture page's host and path.
    fn parsed(header: &str) -> Result<Cookie, Reject> {
        parse(header, "a.test", "/docs/page", NOW)
    }

    #[test]
    fn the_epoch_arithmetic_matches_known_dates() {
        // The calendar maths is the one part of this file that cannot be
        // eyeballed, so it is pinned against dates with published values.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2026, 1, 1) * 86_400, NOW);
        // A leap day, and the day after a century that is not a leap year.
        assert_eq!(
            days_from_civil(2024, 2, 29) + 1,
            days_from_civil(2024, 3, 1)
        );
        assert_eq!(
            days_from_civil(1900, 2, 28) + 1,
            days_from_civil(1900, 3, 1)
        );
    }

    #[test]
    fn the_three_date_shapes_all_parse_to_the_same_instant() {
        // Sun, 06 Nov 1994 08:49:37 GMT — the RFC's own example, in each form.
        let expected = Some(784_111_777);
        assert_eq!(parse_expires("Sun, 06 Nov 1994 08:49:37 GMT"), expected);
        assert_eq!(parse_expires("Sunday, 06-Nov-94 08:49:37 GMT"), expected);
        assert_eq!(parse_expires("Sun Nov  6 08:49:37 1994"), expected);
        // Case and extra rubbish around the tokens do not matter.
        assert_eq!(parse_expires("sun, 06 nov 1994 08:49:37 gmt"), expected);
        // Two-digit years map as the RFC says, and the boundary is the
        // surprising part: 70 is 1970, but 69 is *2069*.
        assert_eq!(
            parse_expires("Thu, 01 Jan 70 00:00:00 GMT"),
            Some(days_from_civil(1970, 1, 1) * 86_400)
        );
        assert_eq!(
            parse_expires("Tue, 01 Jan 69 00:00:00 GMT"),
            Some(days_from_civil(2069, 1, 1) * 86_400)
        );
    }

    #[test]
    fn a_date_that_is_not_a_date_is_none_and_not_a_panic() {
        for bad in [
            "",
            "tomorrow",
            "Sun, 06 Nov 1994",           // no time
            "08:49:37 GMT",               // no date
            "Sun, 32 Nov 1994 08:49:37",  // no such day
            "Sun, 06 Nov 1994 24:00:00",  // no such hour
            "Sun, 06 Nov 1594 08:49:37",  // before the RFC's floor
            "Sun, 06 Nov 199999 08:49:3", // year too long
            "Sun, 06 Foo 1994 08:49:37",  // no such month
            "0:0:0:0 06 Nov 1994",        // four fields is not a time
        ] {
            assert_eq!(parse_expires(bad), None, "{bad:?} parsed as a date");
        }
    }

    #[test]
    fn a_plain_pair_takes_the_pages_directory_as_its_path() {
        let cookie = parsed("a=1").expect("a plain pair is a cookie");
        assert_eq!((cookie.name.as_str(), cookie.value.as_str()), ("a", "1"));
        assert_eq!(cookie.path, "/docs", "the default path is the directory");
        assert_eq!(cookie.host, "a.test");
        assert_eq!(cookie.expires, None, "no expiry is a session cookie");
        assert!(!cookie.secure && !cookie.http_only);
        assert_eq!(cookie.same_site, SameSite::Unspecified);
        // A page at the root, and a page with no path at all.
        assert_eq!(parse("a=1", "a.test", "/", NOW).unwrap().path, "/");
        assert_eq!(parse("a=1", "a.test", "", NOW).unwrap().path, "/");
    }

    #[test]
    fn attributes_parse_in_any_order_and_any_case() {
        let cookie = parsed(
            "a=1; HTTPONLY; samesite=Lax; Path=/x; secure; Expires=Sun, 06 Nov 1994 08:49:37 GMT",
        )
        .expect("every attribute is optional and unordered");
        assert_eq!(cookie.path, "/x");
        assert!(cookie.secure && cookie.http_only);
        assert_eq!(cookie.same_site, SameSite::Lax);
        assert_eq!(cookie.expires, Some(784_111_777));
        // An unknown attribute is skipped, not fatal — the list grows.
        let cookie = parsed("a=1; Priority=High; Partitioned; nonsense=x").expect("still a cookie");
        assert_eq!(cookie.value, "1");
        // A relative `Path` is not a path; the default stands.
        assert_eq!(parsed("a=1; Path=x").unwrap().path, "/docs");
        // A repeated attribute is last-wins.
        assert_eq!(parsed("a=1; Path=/one; Path=/two").unwrap().path, "/two");
        // `SameSite` with a value nobody defined is Unspecified, not a reject.
        assert_eq!(
            parsed("a=1; SameSite=sideways").unwrap().same_site,
            SameSite::Unspecified
        );
    }

    #[test]
    fn max_age_beats_expires_however_they_are_ordered() {
        let far = "Expires=Sun, 06 Nov 2094 08:49:37 GMT";
        assert_eq!(
            parsed(&format!("a=1; Max-Age=60; {far}")).unwrap().expires,
            Some(NOW + 60)
        );
        assert_eq!(
            parsed(&format!("a=1; {far}; Max-Age=60")).unwrap().expires,
            Some(NOW + 60)
        );
        // Zero and negative are an expiry in the past — how deletion works.
        for age in ["0", "-1", "-99999"] {
            let cookie = parsed(&format!("a=1; Max-Age={age}")).unwrap();
            assert!(cookie.expired(NOW), "Max-Age={age} was not already expired");
        }
        // A `Max-Age` that is not a number leaves `Expires` alone.
        let cookie = parsed(&format!("a=1; {far}; Max-Age=soon")).unwrap();
        assert_eq!(
            cookie.expires,
            parse_expires("Sun, 06 Nov 2094 08:49:37 GMT")
        );
        // And an `Expires` that is not a date leaves a session cookie.
        assert_eq!(parsed("a=1; Expires=tomorrow").unwrap().expires, None);
    }

    #[test]
    fn the_hostile_inputs_each_get_an_answer() {
        // No `=` at all, in three shapes.
        assert_eq!(parsed(""), Err(Reject::NoPair));
        assert_eq!(parsed("justaname"), Err(Reject::NoPair));
        assert_eq!(parsed(";;;;;;"), Err(Reject::NoPair));
        // An empty name, with and without whitespace.
        assert_eq!(parsed("=value"), Err(Reject::EmptyName));
        assert_eq!(parsed("   =value"), Err(Reject::EmptyName));
        // An empty value is a *cookie*, and a common way to blank one out.
        assert_eq!(parsed("a=").unwrap().value, "");
        // A leading `$` was reserved in the Netscape draft and is an ordinary
        // name today; browsers store it, so we do.
        assert_eq!(parsed("$Version=1").unwrap().name, "$Version");
        // A comma is legal in practice (folding `Set-Cookie` on commas is the
        // mistake this avoids), a newline is not: it is header injection once
        // this reaches a `Cookie:` header (M11.7).
        assert_eq!(parsed("a=1,2").unwrap().value, "1,2");
        assert_eq!(
            parsed("a=1\r\nSet-Cookie: b=2"),
            Err(Reject::ControlCharacter)
        );
        assert_eq!(parsed("a=1\n2"), Err(Reject::ControlCharacter));
        assert_eq!(parsed("a=\u{7f}"), Err(Reject::ControlCharacter));
        assert_eq!(parsed("a\u{1}b=1"), Err(Reject::ControlCharacter));
        // Whitespace *around* a name or value is trimmed rather than rejected,
        // which is what browsers do and what `a = 1` in a page's script means.
        assert_eq!(parsed("a\t= 1 ").unwrap().name, "a");
        assert_eq!(parsed("a\t= 1 ").unwrap().value, "1");
        // A value nobody could want, refused before it is stored.
        let huge = format!("a={}", "x".repeat(100 * 1024));
        assert_eq!(parsed(&huge), Err(Reject::TooLarge));
        // And the boundary either side of the cap.
        assert!(parsed(&format!("a={}", "x".repeat(MAX_COOKIE_BYTES - 1))).is_ok());
        assert_eq!(
            parsed(&format!("a={}", "x".repeat(MAX_COOKIE_BYTES))),
            Err(Reject::TooLarge)
        );
    }

    #[test]
    fn a_path_matches_at_a_slash_boundary_and_nowhere_else() {
        assert!(path_matches("/", "/anything/at/all"));
        assert!(path_matches("/foo", "/foo"));
        assert!(path_matches("/foo", "/foo/bar"));
        assert!(path_matches("/foo/", "/foo/bar"));
        assert!(!path_matches("/foo", "/foobar"));
        assert!(!path_matches("/foo/bar", "/foo"));
    }

    #[test]
    fn one_assignment_replaces_one_cookie_and_leaves_the_rest() {
        // The property that is not a string: assigning does not overwrite the
        // whole jar, however much `document.cookie = "a=1"` looks like it.
        let jar = Jar::new();
        let page = scope("https://a.test/docs/page");
        for header in ["a=1", "b=2", "c=3"] {
            jar.write_from_script(header, &page, NOW).unwrap();
        }
        assert_eq!(jar.read_for_script(&page, NOW), "a=1; b=2; c=3");
        jar.write_from_script("b=changed", &page, NOW).unwrap();
        assert_eq!(
            jar.read_for_script(&page, NOW),
            "a=1; b=changed; c=3",
            "a replacement moved or dropped a cookie"
        );
        // Deletion is an expiry in the past, and nothing else is.
        jar.write_from_script("b=; Max-Age=0", &page, NOW).unwrap();
        assert_eq!(jar.read_for_script(&page, NOW), "a=1; c=3");
    }

    #[test]
    fn a_cookie_expires_without_anything_sleeping() {
        // The deterministic-clock test: `now` is an argument, so time passes
        // by arithmetic.
        let jar = Jar::new();
        let page = scope("https://a.test/");
        jar.write_from_script("s=session", &page, NOW).unwrap();
        jar.write_from_script("a=1; Max-Age=60", &page, NOW)
            .unwrap();
        jar.write_from_script("b=2; Expires=Sun, 06 Nov 2094 08:49:37 GMT", &page, NOW)
            .unwrap();

        assert_eq!(jar.read_for_script(&page, NOW), "s=session; a=1; b=2");
        assert_eq!(jar.read_for_script(&page, NOW + 59), "s=session; a=1; b=2");
        // A second past the deadline, the short one is gone and the others are
        // not. A session cookie has no deadline to pass.
        assert_eq!(jar.read_for_script(&page, NOW + 61), "s=session; b=2");
        assert_eq!(jar.read_for_script(&page, NOW + 60), "s=session; b=2");
        // Far enough forward and only the session cookie is left.
        let much_later = days_from_civil(2095, 1, 1) * 86_400;
        assert_eq!(jar.read_for_script(&page, much_later), "s=session");
        assert_eq!(jar.all("a.test", much_later).len(), 1);
    }

    #[test]
    fn a_cookie_never_crosses_a_host_not_even_to_a_subdomain() {
        // Deliverable 2, as a test rather than a policy.
        let jar = Jar::new();
        let a = scope("https://a.test/");
        jar.write_from_script("who=from a", &a, NOW).unwrap();
        // Another host cannot see it.
        assert_eq!(jar.read_for_script(&scope("https://b.test/"), NOW), "");
        // Nor can a subdomain of it, nor its parent.
        assert_eq!(jar.read_for_script(&scope("https://sub.a.test/"), NOW), "");
        assert_eq!(jar.read_for_script(&scope("https://test/"), NOW), "");
        // And `Domain` is no way in: a page cannot widen its own cookie.
        jar.write_from_script("wide=1; Domain=.test", &a, NOW)
            .unwrap();
        jar.write_from_script("wider=1; Domain=a.test", &a, NOW)
            .unwrap();
        assert_eq!(jar.read_for_script(&scope("https://sub.a.test/"), NOW), "");
        assert_eq!(jar.read_for_script(&scope("https://b.test/"), NOW), "");
        assert_eq!(jar.read_for_script(&a, NOW), "who=from a; wide=1; wider=1");
        // A different scheme on the same host *is* the same jar — a cookie's
        // scope is a host, which is why `Secure` exists.
        assert_eq!(
            jar.read_for_script(&scope("http://a.test/"), NOW),
            "who=from a; wide=1; wider=1"
        );
    }

    #[test]
    fn an_http_only_cookie_is_invisible_to_script_both_ways() {
        // Only a server can set one (M11.7); a script can neither read it nor
        // overwrite it, or the flag would protect the value and not the slot.
        let jar = Jar::new();
        let page = scope("https://a.test/");
        jar.insert(
            Cookie {
                name: "session".into(),
                value: "secret".into(),
                host: "a.test".into(),
                path: "/".into(),
                expires: None,
                secure: false,
                http_only: true,
                same_site: SameSite::Unspecified,
            },
            NOW,
        )
        .unwrap();
        jar.write_from_script("visible=1", &page, NOW).unwrap();

        assert_eq!(jar.read_for_script(&page, NOW), "visible=1");
        assert_eq!(
            jar.write_from_script("session=stolen", &page, NOW),
            Err(Reject::HttpOnlyExists)
        );
        assert_eq!(
            jar.write_from_script("session=; Max-Age=0", &page, NOW),
            Err(Reject::HttpOnlyExists),
            "a script deleted an HttpOnly cookie"
        );
        // The real one is untouched, and a script cannot set one either.
        assert_eq!(jar.all("a.test", NOW)[0].value, "secret");
        assert_eq!(
            jar.write_from_script("mine=1; HttpOnly", &page, NOW),
            Err(Reject::HttpOnlyFromScript)
        );
    }

    #[test]
    fn an_expired_protected_cookie_stops_protecting_a_slot_it_no_longer_holds() {
        // The getter filters by expiry and the guard on the setter must agree:
        // an `HttpOnly` cookie that has run out is *gone*, so a script may take
        // the name. Two answers to "is there a cookie here" is one too many —
        // otherwise a server's long-dead session cookie refuses the page's own
        // write for the rest of the session, invisibly.
        let jar = Jar::new();
        let page = scope("https://a.test/");
        for (name, secure, http_only) in [("session", false, true), ("locked", true, false)] {
            jar.insert(
                Cookie {
                    name: name.into(),
                    value: "server's".into(),
                    host: "a.test".into(),
                    path: "/".into(),
                    expires: Some(NOW + 60),
                    secure,
                    http_only,
                    same_site: SameSite::Unspecified,
                },
                NOW,
            )
            .unwrap();
        }
        // While they live, they hold their slots.
        assert_eq!(
            jar.write_from_script("session=mine", &page, NOW),
            Err(Reject::HttpOnlyExists)
        );
        assert_eq!(
            jar.write_from_script("locked=mine", &scope("http://a.test/"), NOW),
            Err(Reject::SecureFromInsecurePage)
        );
        // Once expired they hold nothing, and the getter agrees.
        jar.write_from_script("session=mine", &page, NOW + 61)
            .unwrap();
        jar.write_from_script("locked=mine", &scope("http://a.test/"), NOW + 61)
            .unwrap();
        assert_eq!(
            jar.read_for_script(&page, NOW + 61),
            "session=mine; locked=mine"
        );
    }

    #[test]
    fn a_secure_cookie_is_neither_set_nor_read_over_http() {
        let jar = Jar::new();
        let secure = scope("https://a.test/");
        let insecure = scope("http://a.test/");
        jar.write_from_script("s=1; Secure", &secure, NOW).unwrap();
        assert_eq!(jar.read_for_script(&secure, NOW), "s=1");
        assert_eq!(jar.read_for_script(&insecure, NOW), "");
        // An http page can neither create one nor walk over the one there.
        assert_eq!(
            jar.write_from_script("t=1; Secure", &insecure, NOW),
            Err(Reject::SecureFromInsecurePage)
        );
        assert_eq!(
            jar.write_from_script("s=stolen", &insecure, NOW),
            Err(Reject::SecureFromInsecurePage)
        );
        assert_eq!(jar.read_for_script(&secure, NOW), "s=1");
    }

    #[test]
    fn the_getter_orders_by_path_length_then_age() {
        // A page parsing `document.cookie` with a regex depends on this, so it
        // is pinned rather than left to the map.
        let jar = Jar::new();
        let page = scope("https://a.test/a/b/c");
        for header in [
            "root=1; Path=/",
            "deep=1; Path=/a/b",
            "mid=1; Path=/a",
            "deeper=1; Path=/a/b/c",
            "root2=1; Path=/",
        ] {
            jar.write_from_script(header, &page, NOW).unwrap();
        }
        // A cookie for a path this page is not under is not there at all.
        jar.write_from_script("elsewhere=1; Path=/other", &page, NOW)
            .unwrap();
        assert_eq!(
            jar.read_for_script(&page, NOW),
            "deeper=1; deep=1; mid=1; root=1; root2=1"
        );
        // Rewriting a value does not make it the newest cookie.
        jar.write_from_script("root=2; Path=/", &page, NOW).unwrap();
        assert_eq!(
            jar.read_for_script(&page, NOW),
            "deeper=1; deep=1; mid=1; root=2; root2=1"
        );
        // A page higher up the tree sees only what matches it.
        assert_eq!(
            jar.read_for_script(&scope("https://a.test/a"), NOW),
            "mid=1; root=2; root2=1"
        );
    }

    #[test]
    fn a_page_cannot_grow_the_jar_past_the_cap() {
        let jar = Jar::new();
        let page = scope("https://a.test/");
        for i in 0..MAX_PER_HOST {
            jar.write_from_script(&format!("c{i}=1"), &page, NOW)
                .unwrap();
        }
        assert_eq!(jar.all("a.test", NOW).len(), MAX_PER_HOST);
        assert_eq!(
            jar.write_from_script("one_more=1", &page, NOW),
            Err(Reject::JarFull),
            "the cap did not hold"
        );
        // Replacing an existing cookie is not growth, so it still works — and
        // so does deleting one to make room.
        jar.write_from_script("c0=2", &page, NOW).unwrap();
        jar.write_from_script("c0=; Max-Age=0", &page, NOW).unwrap();
        jar.write_from_script("one_more=1", &page, NOW).unwrap();
        assert_eq!(jar.all("a.test", NOW).len(), MAX_PER_HOST);
        // Another host has its own budget.
        let b = scope("https://b.test/");
        jar.write_from_script("only=1", &b, NOW).unwrap();
        assert_eq!(jar.all("b.test", NOW).len(), 1);
    }

    // ---- M11.7: the wire ---------------------------------------------------

    /// A jar holding one `HttpOnly` cookie and one ordinary one, both live.
    fn seeded(host: &str) -> Jar {
        let jar = Jar::new();
        for (name, http_only) in [("session", true), ("theme", false)] {
            jar.insert(
                Cookie {
                    name: name.into(),
                    value: "v".into(),
                    host: host.into(),
                    path: "/".into(),
                    expires: None,
                    secure: false,
                    http_only,
                    same_site: SameSite::Unspecified,
                },
                NOW,
            )
            .unwrap();
        }
        jar
    }

    #[test]
    fn the_wire_gets_the_http_only_cookies_a_script_cannot_see() {
        // The one way the two readers differ, and the whole point of the flag.
        let jar = seeded("a.test");
        let page = "https://a.test/";
        assert_eq!(
            header_for(&jar, page, "https://a.test/app.js", NOW).as_deref(),
            Some("session=v; theme=v")
        );
        assert_eq!(
            jar.read_for_script(&scope(page), NOW),
            "theme=v",
            "an HttpOnly cookie became visible to a script"
        );
        // The order the two readers produce is the same order, from the same
        // code: a page parsing `document.cookie` with a regex and a server
        // parsing the header see the jar the same way.
        assert_eq!(
            header_for(&jar, page, page, NOW).as_deref(),
            Some("session=v; theme=v")
        );
    }

    #[test]
    fn a_cross_origin_request_carries_no_cookie_header() {
        // The ladder's own case, not a hypothetical: M11.5's second rung
        // injects a script from `www.google-analytics.com`, and it gets
        // nothing — the tracker cannot be told who the reader is.
        let jar = seeded("www.google-analytics.com");
        jar.insert(
            Cookie {
                name: "id".into(),
                value: "who".into(),
                host: "motherfuckingwebsite.com".into(),
                path: "/".into(),
                expires: None,
                secure: false,
                http_only: false,
                same_site: SameSite::Unspecified,
            },
            NOW,
        )
        .unwrap();
        let page = "http://motherfuckingwebsite.com/";
        assert_eq!(
            header_for(
                &jar,
                page,
                "https://www.google-analytics.com/analytics.js",
                NOW
            ),
            None,
            "a cross-origin subresource carried cookies"
        );
        // The page's own subresource still does.
        assert_eq!(
            header_for(&jar, page, "http://motherfuckingwebsite.com/x.css", NOW).as_deref(),
            Some("id=who")
        );
        // And "origin" is scheme, host *and* port, exactly as `fetch()` reads
        // it — a jar keyed by host does not make http and https one origin.
        let jar = seeded("a.test");
        assert_eq!(
            header_for(&jar, "https://a.test/", "http://a.test/x", NOW),
            None
        );
        assert_eq!(
            header_for(&jar, "http://a.test/", "http://a.test:8080/x", NOW),
            None
        );
        // Neither does a subdomain, on the wire or anywhere else.
        assert_eq!(
            header_for(&jar, "https://a.test/", "https://sub.a.test/x", NOW),
            None
        );
    }

    #[test]
    fn a_secure_cookie_is_not_sent_over_http() {
        let jar = Jar::new();
        jar.write_from_script("s=1; Secure", &scope("https://a.test/"), NOW)
            .unwrap();
        jar.write_from_script("plain=1", &scope("https://a.test/"), NOW)
            .unwrap();
        assert_eq!(
            header_for(&jar, "https://a.test/", "https://a.test/x", NOW).as_deref(),
            Some("s=1; plain=1")
        );
        assert_eq!(
            header_for(&jar, "http://a.test/", "http://a.test/x", NOW).as_deref(),
            Some("plain=1"),
            "a Secure cookie went out over http"
        );
    }

    #[test]
    fn the_header_is_matched_against_the_request_path_not_the_pages() {
        // A stylesheet at `/static/x.css` is a different path from the page at
        // `/docs/page`, and `Path` is matched against the *request*.
        let jar = Jar::new();
        let page = scope("https://a.test/docs/page");
        jar.write_from_script("everywhere=1; Path=/", &page, NOW)
            .unwrap();
        jar.write_from_script("docs=1; Path=/docs", &page, NOW)
            .unwrap();
        jar.write_from_script("static=1; Path=/static", &page, NOW)
            .unwrap();
        assert_eq!(
            header_for(
                &jar,
                "https://a.test/docs/page",
                "https://a.test/static/x.css",
                NOW
            )
            .as_deref(),
            Some("static=1; everywhere=1")
        );
        // Expiry is the same rule as the getter's, applied in the same place.
        jar.write_from_script("gone=1; Path=/; Max-Age=60", &page, NOW)
            .unwrap();
        assert_eq!(
            header_for(&jar, "https://a.test/docs/page", "https://a.test/", NOW).as_deref(),
            Some("everywhere=1; gone=1")
        );
        assert_eq!(
            header_for(
                &jar,
                "https://a.test/docs/page",
                "https://a.test/",
                NOW + 61
            )
            .as_deref(),
            Some("everywhere=1")
        );
    }

    #[test]
    fn an_empty_answer_is_none_rather_than_an_empty_header() {
        // A `Cookie:` header with nothing in it is a header a server has to
        // parse for no reason; `None` means the worker sends none at all.
        let jar = Jar::new();
        assert_eq!(
            header_for(&jar, "https://a.test/", "https://a.test/x", NOW),
            None
        );
        // No host on either side is the same answer, not a panic.
        assert_eq!(header_for(&jar, "", "https://a.test/x", NOW), None);
        assert_eq!(
            header_for(&jar, "file:///tmp/p.html", "file:///tmp/x.css", NOW),
            None
        );
        assert_eq!(header_for(&jar, "https://a.test/", "not a url", NOW), None);
    }

    #[test]
    fn a_server_sets_what_a_script_may_not_and_a_script_still_cannot_see_it() {
        let jar = Jar::new();
        let console = Console::new();
        apply_set_cookie(
            &jar,
            "https://a.test/login",
            &[
                "sid=abc; Path=/; HttpOnly; Secure".to_string(),
                "theme=dark; Path=/".to_string(),
            ],
            NOW,
            &console,
        );
        // Both on the wire...
        assert_eq!(
            header_for(&jar, "https://a.test/login", "https://a.test/app.js", NOW).as_deref(),
            Some("sid=abc; theme=dark")
        );
        // ...one of them to the page.
        assert_eq!(
            jar.read_for_script(&scope("https://a.test/x"), NOW),
            "theme=dark"
        );
        assert!(console.entries().is_empty(), "{:?}", console.entries());
    }

    #[test]
    fn a_set_cookie_is_scoped_to_the_url_the_response_came_from() {
        // Post-redirect: the host and the default path both come from where
        // the bytes actually arrived, never from what was asked for.
        let jar = Jar::new();
        apply_set_cookie(
            &jar,
            "https://b.test/app/page",
            &["where=here".to_string()],
            NOW,
            &Console::new(),
        );
        assert_eq!(jar.all("b.test", NOW).len(), 1);
        assert_eq!(jar.all("b.test", NOW)[0].path, "/app");
        assert!(jar.all("a.test", NOW).is_empty());
    }

    #[test]
    fn a_malformed_set_cookie_is_a_dropped_cookie_and_a_console_line() {
        let jar = Jar::new();
        let console = Console::new();
        apply_set_cookie(
            &jar,
            "https://a.test/",
            &[
                "nonsense".to_string(),
                "ok=1".to_string(),
                // Header injection, refused before anything can put this in a
                // `Cookie:` header. (Leading whitespace *is* trimmed, so the
                // break has to be where a real one would be — inside the
                // value, after the byte that made it a value.)
                "bad=1\r\nSet-Cookie: sneaky=2".to_string(),
            ],
            NOW,
            &console,
        );
        assert_eq!(jar.read_for_script(&scope("https://a.test/"), NOW), "ok=1");
        let entries = console.entries();
        let lines: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("no name=value pair"), "{lines:?}");
        assert!(lines[1].contains("control character"), "{lines:?}");
        // And a response with no host to scope to changes nothing.
        let jar = Jar::new();
        apply_set_cookie(
            &jar,
            "not a url",
            &["a=1".to_string()],
            NOW,
            &Console::new(),
        );
        assert!(jar.all("a.test", NOW).is_empty());
    }

    #[test]
    fn a_response_that_would_overfill_the_jar_is_refused_not_evicted() {
        // The M11.6 decision, re-made with a server on the other end: a page
        // that fills its own jar loses the server's next cookie, and that is
        // the cheaper failure. Eviction would let the page choose *which*
        // cookie to lose, and it would choose the session.
        let jar = Jar::new();
        let page = scope("https://a.test/");
        for i in 0..MAX_PER_HOST {
            jar.write_from_script(&format!("c{i}=1"), &page, NOW)
                .unwrap();
        }
        let console = Console::new();
        apply_set_cookie(
            &jar,
            "https://a.test/",
            &["sid=abc; HttpOnly".to_string()],
            NOW,
            &console,
        );
        assert!(
            header_for(&jar, "https://a.test/", "https://a.test/x", NOW)
                .is_none_or(|header| !header.contains("sid=")),
            "the cap did not hold against a server"
        );
        assert!(
            console.entries()[0].text.contains("50 cookies"),
            "{:?}",
            console.entries()
        );
    }

    #[test]
    fn nothing_a_cookie_knows_outlives_the_process() {
        // "Nothing reaches the disk", pinned the only way it can be: a jar is
        // reachable exactly one way — `Jar::new`, which takes no path — and a
        // new one is empty however "persistent" the cookies in the last one
        // claimed to be. There is nowhere a previous run's cookies could come
        // from because there is no constructor that could load them.
        let jar = Jar::new();
        let page = scope("https://a.test/");
        jar.write_from_script("keep=1; Expires=Sun, 06 Nov 2094 08:49:37 GMT", &page, NOW)
            .unwrap();
        apply_set_cookie(
            &jar,
            "https://a.test/",
            &["sid=abc; Expires=Sun, 06 Nov 2094 08:49:37 GMT; HttpOnly".to_string()],
            NOW,
            &Console::new(),
        );
        assert_eq!(jar.all("a.test", NOW).len(), 2);

        let next_session = Jar::new();
        assert_eq!(next_session.read_for_script(&page, NOW), "");
        assert_eq!(
            header_for(&next_session, "https://a.test/", "https://a.test/x", NOW),
            None
        );
    }

    #[test]
    fn a_page_with_no_host_has_no_cookies_rather_than_an_error() {
        // A dump with nothing to resolve against, and a `file:` page: there is
        // no host to scope a jar to, so the answer is "none", not a panic.
        assert_eq!(Scope::of(""), None);
        assert_eq!(Scope::of("not a url"), None);
        assert_eq!(Scope::of("file:///tmp/page.html"), None);
        // A host is lower-cased on the way in, so case cannot split a jar.
        assert_eq!(scope("https://A.TEST/x").host, "a.test");
        let jar = Jar::new();
        jar.write_from_script("a=1", &scope("https://A.Test/"), NOW)
            .unwrap();
        assert_eq!(jar.read_for_script(&scope("https://a.test/"), NOW), "a=1");
    }
}
