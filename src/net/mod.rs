mod fetch;

pub use fetch::spawn_fetch;

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

#[cfg(test)]
mod tests {
    use super::normalize_url;

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
