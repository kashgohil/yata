mod fetch;

pub use fetch::{
    JsResponse, MAX_FETCH_BYTES, MAX_SCRIPT_BYTES, is_document, spawn_fetch, spawn_image,
    spawn_js_fetch, spawn_script, spawn_stylesheet,
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

#[cfg(test)]
mod tests {
    use super::{normalize_url, percent_decode, resolve_url};

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

/// One generation of fetching. `App` owns the counter and hands out ids; the
/// event loop drops any net message whose id isn't the current generation, so
/// a slow stale fetch can never clobber a newer one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FetchId(pub u64);
