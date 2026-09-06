mod fetch;

pub use fetch::{
    JsResponse, MAX_FETCH_BYTES, MAX_SCRIPT_BYTES, is_document, spawn_cached, spawn_fetch,
    spawn_image, spawn_js_fetch, spawn_script, spawn_stylesheet,
};

/// Default a bare URL to `https://`. The single place scheme defaulting lives,
/// applied to both the CLI argument and URL-bar input before either reaches the
/// fetch worker. Nothing fancier: no search fallback, no validation — a garbage
/// URL still becomes a `NetError`.
pub fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

/// Resolve `href` against the page's final URL — `<link href="news.css?x">` is
/// relative to wherever the page came from, redirects included. `None` when
/// either side is unparseable, and the caller skips that link.
///
/// `reqwest::Url` does the joining. URL syntax is not a pipeline stage (the
/// engine's identity is parser/cascade/layout/paint, PLAN.md §5), reqwest is
/// already a dependency, and §2 puts URL handling in `net/` precisely here.
pub fn resolve_url(base: &str, href: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    Some(base.join(href.trim()).ok()?.to_string())
}

/// The same URL with its query string **replaced** by `query` (M11.10).
///
/// Replaced, not appended to: a GET form submission discards whatever query the
/// action carried, so `/w/index.php?oldid=5` submitted with `search=cat` is
/// `/w/index.php?search=cat`. An empty data set still leaves the `?`, which is
/// what a browser sends.
///
/// Here rather than in `browser::form` for the reason `resolve_url` is here:
/// URL syntax is not a pipeline stage, and `reqwest::Url` is the one parser
/// this engine uses for it. `None` when the URL is unparseable, and the caller
/// submits nothing.
pub fn set_query(url: &str, query: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(url).ok()?;
    url.set_query(Some(query));
    Some(url.to_string())
}

/// Percent-decode a URL component. Used for fragments (M11.4): a URL escapes
/// the non-ASCII in `#Ausgangs%C3%BCberpr%C3%BCfung`, while `Dom::attr` holds
/// the decoded `id="Ausgangsüberprüfung"`, so one side has to be converted
/// before they can be compared.
///
/// Fails soft, deliberately and in three ways: `%zz` is not an error, it is a
/// literal `%zz` in an anchor name; a `%` at the end of the string is the same;
/// and bytes that do not form UTF-8 leave the whole string as it was. A
/// fragment nobody can decode still gets to match an id spelled the same way,
/// which is more useful than a decoder that reports failures nothing can act
/// on. Not a general URL decoder: `+` is a space in a query string and a plus
/// sign everywhere else, so it is left alone.
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Encode one name or value of an `application/x-www-form-urlencoded` data set
/// (M11.10) — the other half of [`percent_decode`], and deliberately beside it.
///
/// **Not a general URL encoder**, which is why it is its own function rather
/// than a flag on one: a space becomes `+` here and `%20` everywhere else in a
/// URL, and the set of characters left literal is HTML's
/// (`*`, `-`, `.`, `_` and the ASCII alphanumerics), not the URL spec's. Using
/// this on a path would corrupt it; using a path encoder here would send `+`
/// as a literal plus and a space as `%20` — which servers accept, and which
/// makes the query string this engine produces differ from every other
/// browser's for no reason.
///
/// Everything else is percent-encoded from its **UTF-8 bytes** (`猫` →
/// `%E7%8C%AB`), in uppercase hex. Names and values go through the same
/// function: `a=b` typed into a field named `a=b` has to survive being written
/// down beside a `=` that means something.
pub fn form_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b' ' => out.push('+'),
            b'*' | b'-' | b'.' | b'_' => out.push(b as char),
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => out.push(b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{form_urlencode, normalize_url, percent_decode, resolve_url};

    #[test]
    fn hrefs_resolve_against_the_page_url() {
        let page = "https://news.ycombinator.com/news";
        // HN's own link, cache-buster and all.
        assert_eq!(
            resolve_url(page, "news.css?3HzzJW9s7JrtYzwqKDTI").as_deref(),
            Some("https://news.ycombinator.com/news.css?3HzzJW9s7JrtYzwqKDTI")
        );
        // Wikipedia's shape: root-relative, query preserved.
        assert_eq!(
            resolve_url("https://en.wikipedia.org/wiki/Cat", "/w/load.php?lang=en").as_deref(),
            Some("https://en.wikipedia.org/w/load.php?lang=en")
        );
        assert_eq!(
            resolve_url(page, "https://cdn.example/x.css").as_deref(),
            Some("https://cdn.example/x.css")
        );
        // Protocol-relative inherits the page's scheme.
        assert_eq!(
            resolve_url(page, "//cdn.example/x.css").as_deref(),
            Some("https://cdn.example/x.css")
        );
        assert_eq!(
            resolve_url(page, "  spaced.css  ").as_deref(),
            Some("https://news.ycombinator.com/spaced.css")
        );
    }

    #[test]
    fn an_unresolvable_href_is_skipped_not_guessed() {
        assert_eq!(resolve_url("not a url", "x.css"), None);
        assert_eq!(resolve_url("https://example.com/", "http://[bad"), None);
    }

    #[test]
    fn a_fragment_in_the_base_never_reaches_the_resolved_url() {
        // M11.4 deliverable 4: after a citation click the page's URL carries
        // `#cite_note-1`, and that URL is the base every later href resolves
        // against. A stylesheet that stopped loading after a fragment jump is
        // the expensive version of this bug, so the property is pinned rather
        // than assumed.
        let page = "https://en.wikipedia.org/wiki/Cat#cite_note-1";
        assert_eq!(
            resolve_url(page, "/w/load.php?lang=en").as_deref(),
            Some("https://en.wikipedia.org/w/load.php?lang=en")
        );
        assert_eq!(
            resolve_url(page, "Dog").as_deref(),
            Some("https://en.wikipedia.org/wiki/Dog")
        );
        // And a fragment-only href keeps the path while replacing the fragment.
        assert_eq!(
            resolve_url(page, "#cite_note-2").as_deref(),
            Some("https://en.wikipedia.org/wiki/Cat#cite_note-2")
        );
    }

    #[test]
    fn percent_decoding_a_fragment_fails_soft() {
        assert_eq!(percent_decode("cite_note-1"), "cite_note-1");
        // Wikipedia's escaped anchors, which `Dom::attr` holds decoded.
        assert_eq!(
            percent_decode("Ausgangs%C3%BCberpr%C3%BCfung"),
            "Ausgangsüberprüfung"
        );
        assert_eq!(percent_decode("%E7%8C%AB"), "猫");
        // Malformed escapes are literal text, not errors.
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("a%2"), "a%2");
        // A decode that would not be UTF-8 leaves the string as it was.
        assert_eq!(percent_decode("%FF%FE"), "%FF%FE");
        // `+` is a query-string convention, not a fragment one.
        assert_eq!(percent_decode("a+b%20c"), "a+b c");
    }

    #[test]
    fn the_form_encoder_as_a_table_of_cases() {
        // M11.10 deliverable 5, case by case — every one of these is a
        // character a reader can type into HN's search box.
        for (raw, encoded) in [
            ("plain", "plain"),
            ("two words", "two+words"),
            ("a&b", "a%26b"),
            ("a=b", "a%3Db"),
            ("100%", "100%25"),
            ("a+b", "a%2Bb"),
            ("say \"hi\"", "say+%22hi%22"),
            ("猫", "%E7%8C%AB"),
            // The unreserved set HTML keeps literal, and nothing else: `~` is
            // unreserved in a URL and still escaped here.
            ("*-._", "*-._"),
            ("~!", "%7E%21"),
            ("Special:Search", "Special%3ASearch"),
            // What a `<textarea>`'s newline has become by the time it reaches
            // here: CRLF, which HTML requires (see `browser::form`).
            ("one\r\ntwo", "one%0D%0Atwo"),
        ] {
            assert_eq!(form_urlencode(raw), encoded, "encoding {raw:?}");
        }
        // The two functions are **not** inverses, and that is the point of
        // having both: `percent_decode` is a fragment decoder, where `+` is a
        // plus sign, so it undoes the escapes and leaves the spaces encoded.
        // Anything that needs a real round trip needs a query-string decoder,
        // which nothing has asked for yet.
        assert_eq!(percent_decode(&form_urlencode("猫 & mouse")), "猫+&+mouse");
    }

    #[test]
    fn bare_host_gets_https() {
        assert_eq!(normalize_url("danluu.com"), "https://danluu.com");
        assert_eq!(normalize_url("  example.com "), "https://example.com");
    }

    #[test]
    fn explicit_scheme_is_left_alone() {
        assert_eq!(normalize_url("http://x/"), "http://x/");
        assert_eq!(
            normalize_url("https://en.wikipedia.org"),
            "https://en.wikipedia.org"
        );
    }
}

/// Stable identity of a live tab. Zero is reserved for headless tools, which
/// deliberately have no tab set or chrome.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TabId(pub u64);

/// One document generation inside one stable tab.
///
/// Both facts travel together through every worker and timer path. A vector
/// index can change when a tab closes, and a generation number can collide in
/// two tabs, so neither fact is sufficient on its own.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PageId {
    pub tab: TabId,
    pub generation: u64,
}

impl PageId {
    pub const fn new(tab: TabId, generation: u64) -> PageId {
        PageId { tab, generation }
    }

    /// Identity used by single-navigation headless modes.
    pub const fn headless(generation: u64) -> PageId {
        PageId::new(TabId(0), generation)
    }
}

/// One request a worker is to make: where to, the `Cookie:` header the jar
/// decided it may carry (M11.7), and — for a document — the method.
///
/// A type rather than a second parameter on five functions, because the
/// pairing is the whole point. The jar is `Rc<RefCell<…>>` and therefore
/// `!Send`, so a worker *cannot* hold one — not by discipline, but because it
/// would not compile. Everything that crosses the channel is this: a URL and a
/// string somebody already decided on, on the UI thread, through
/// `cookies::header_for`. "Did this request ask the jar?" is then a question a
/// reviewer answers by reading a signature rather than by counting call sites.
///
/// The method is an enum whose POST variant *is* the body, so a GET cannot
/// carry one (M11.11). A password has no business riding along on a link
/// click, a reload, or a 303 hop by accident.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Request {
    pub url: String,
    /// The `Cookie:` header value, or `None` for a request that carries none —
    /// cross-origin, or a jar with nothing that matches.
    pub cookie: Option<String>,
    pub method: Method,
}

/// What kind of document request this is (M11.11). Subresources are always
/// [`Get`]: they have no caller that would set a body.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Method {
    #[default]
    Get,
    /// A document GET whose cache directives were selected by the UI cache.
    Conditional {
        no_cache: bool,
        if_none_match: Option<String>,
    },
    /// `application/x-www-form-urlencoded` body. The Content-Type is implied;
    /// the worker writes it, so a GET cannot grow one by forgetting a branch.
    Post { body: String },
}

impl Request {
    /// A request that carries no cookies, because there is no jar to ask: the
    /// headless document fetch (whose jar does not exist until the document
    /// has arrived) and the tests that are not about cookies.
    pub fn bare(url: impl Into<String>) -> Request {
        Request {
            url: url.into(),
            cookie: None,
            method: Method::Get,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentSource {
    Network,
    CacheHit,
    Revalidated,
}
