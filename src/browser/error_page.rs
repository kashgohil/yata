//! Synthetic error pages (PLAN.md M7 / UX §3.7).
//!
//! Network and content failures render as readable pages with a retry hint —
//! never a blank screen and never a panic.

/// Document gate shared with the fetch worker — re-exported so the TUI and the
/// worker cannot drift (review: one predicate, two call sites).
pub use crate::net::is_document;

/// Build the plain-text body shown in the viewport for a failed navigation.
pub fn render(url: &str, reason: &str) -> String {
    format!(
        "Could not load page\n\
         \n\
         URL: {url}\n\
         {reason}\n\
         \n\
         Press r to retry.\n\
         Press o to open another URL.\n"
    )
}

/// Human-readable reason for a non-success HTTP status.
pub fn http_reason(status: u16) -> String {
    let label = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    format!("HTTP {status} {label}")
}

/// Reason string when the status is fine but the type is not a document.
pub fn unsupported_type_reason(content_type: Option<&str>) -> String {
    match content_type {
        Some(ct) if !ct.is_empty() => format!("unsupported content-type: {ct}"),
        _ => "unsupported content-type".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_page_names_url_reason_and_retry() {
        let page = render("https://x.test/", "connection refused");
        assert!(page.contains("https://x.test/"));
        assert!(page.contains("connection refused"));
        assert!(page.contains("Press r to retry."));
    }

    #[test]
    fn only_document_types_pass() {
        assert!(is_document(200, None));
        assert!(is_document(200, Some("text/html; charset=utf-8")));
        assert!(is_document(204, Some("text/html")));
        assert!(!is_document(404, Some("text/html")));
        assert!(!is_document(200, Some("image/png")));
        assert!(!is_document(200, Some("application/json")));
    }

    #[test]
    fn document_predicate_is_shared_with_net() {
        // Re-export of `net::is_document` — same answers, no second implementation.
        let cases: &[(u16, Option<&str>, bool)] = &[
            (200, None, true),
            (200, Some("text/html; charset=utf-8"), true),
            (404, Some("text/html"), false),
            (200, Some("image/png"), false),
            (200, Some("application/json"), false),
            (204, Some("text/plain"), true),
        ];
        for &(status, ct, want) in cases {
            assert_eq!(
                is_document(status, ct),
                crate::net::is_document(status, ct),
                "status={status} ct={ct:?}"
            );
            assert_eq!(is_document(status, ct), want, "status={status} ct={ct:?}");
        }
    }
}
