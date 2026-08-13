mod fetch;

pub use fetch::{
    MAX_SCRIPT_BYTES, is_document, spawn_fetch, spawn_image, spawn_script, spawn_stylesheet,
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

#[cfg(test)]
mod tests {
    use super::{normalize_url, resolve_url};

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
