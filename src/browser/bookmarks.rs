//! Bounded bookmark records and their dependency-free on-disk format.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_BOOKMARKS: usize = 1_024;
pub const MAX_URL_BYTES: usize = 8_192;
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const HEADER: &[u8] = b"yata-bookmarks-v1\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub url: Arc<str>,
    pub title: Arc<str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bookmarks {
    records: Vec<Bookmark>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddResult {
    Added,
    Duplicate,
    Full,
}

impl Bookmarks {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn records(&self) -> &[Bookmark] {
        &self.records
    }
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn add(&mut self, url: Arc<str>, title: Arc<str>) -> AddResult {
        if self.records.iter().any(|record| record.url == url) {
            return AddResult::Duplicate;
        }
        if self.records.len() == MAX_BOOKMARKS {
            return AddResult::Full;
        }
        debug_assert!(valid_url(&url));
        debug_assert!(url.len() <= MAX_URL_BYTES);
        debug_assert!(!title.is_empty() && title.len() <= MAX_TITLE_BYTES);
        self.records.insert(0, Bookmark { url, title });
        AddResult::Added
    }

    pub fn remove(&mut self, index: usize) -> Option<Bookmark> {
        (index < self.records.len()).then(|| self.records.remove(index))
    }

    pub fn snapshot(&self) -> Arc<[Bookmark]> {
        Arc::from(self.records.clone())
    }
    fn from_records(records: Vec<Bookmark>) -> Self {
        Self { records }
    }
}

/// The tab strip and bookmark capture share exactly one retained-title rule.
pub fn sanitize_title(raw: &str) -> String {
    fn push(out: &mut String, ch: char) -> bool {
        if out.len() + ch.len_utf8() > MAX_TITLE_BYTES {
            return false;
        }
        out.push(ch);
        true
    }
    let mut title = String::new();
    let mut space = false;
    for ch in raw.chars() {
        if ch.is_ascii_whitespace() {
            space = !title.is_empty();
        } else {
            if space && !push(&mut title, ' ') {
                break;
            }
            space = false;
            let safe = if ch.is_control() { '\u{fffd}' } else { ch };
            if !push(&mut title, safe) {
                break;
            }
        }
    }
    title.trim().to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatError(String);

impl FormatError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for FormatError {}

pub fn encode(records: &[Bookmark]) -> Result<Vec<u8>, FormatError> {
    if records.len() > MAX_BOOKMARKS {
        return Err(FormatError::new("too many bookmarks"));
    }
    let mut out = Vec::with_capacity(HEADER.len().saturating_add(records.len() * 64));
    out.extend_from_slice(HEADER);
    let mut seen = HashSet::with_capacity(records.len());
    for record in records {
        validate_record(&record.url, &record.title)?;
        if !seen.insert(record.url.as_ref()) {
            return Err(FormatError::new("duplicate bookmark URL"));
        }
        escape_into(&record.url, &mut out);
        out.push(b'\t');
        escape_into(&record.title, &mut out);
        out.push(b'\n');
        if out.len() > MAX_FILE_BYTES {
            return Err(FormatError::new("bookmark file is too large"));
        }
    }
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<Bookmarks, FormatError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(FormatError::new("bookmark file is too large"));
    }
    if !bytes.starts_with(HEADER) {
        return Err(FormatError::new(if bytes.is_empty() {
            "bookmark file is empty"
        } else {
            "unsupported bookmark file header"
        }));
    }
    let body = &bytes[HEADER.len()..];
    if !body.is_empty() && !body.ends_with(b"\n") {
        return Err(FormatError::new(
            "bookmark record is not newline terminated",
        ));
    }
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for line in body
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if records.len() == MAX_BOOKMARKS {
            return Err(FormatError::new("too many bookmarks"));
        }
        let mut tabs = line.iter().enumerate().filter(|(_, byte)| **byte == b'\t');
        let Some((tab, _)) = tabs.next() else {
            return Err(FormatError::new("bookmark record is missing a field"));
        };
        if tabs.next().is_some() {
            return Err(FormatError::new("bookmark record has extra fields"));
        }
        let url = unescape(&line[..tab], MAX_URL_BYTES, "URL")?;
        let title = unescape(&line[tab + 1..], MAX_TITLE_BYTES, "title")?;
        validate_record(&url, &title)?;
        if !seen.insert(url.clone()) {
            return Err(FormatError::new("duplicate bookmark URL"));
        }
        records.push(Bookmark {
            url: Arc::from(url),
            title: Arc::from(title),
        });
    }
    Ok(Bookmarks::from_records(records))
}

fn validate_record(url: &str, title: &str) -> Result<(), FormatError> {
    if url.is_empty() || url.len() > MAX_URL_BYTES {
        return Err(FormatError::new("bookmark URL exceeds its size limit"));
    }
    if title.is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err(FormatError::new("bookmark title exceeds its size limit"));
    }
    if !valid_url(url) {
        return Err(FormatError::new("bookmark URL is not normalized HTTP(S)"));
    }
    Ok(())
}

pub fn valid_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.as_str() == url
}

fn escape_into(value: &str, out: &mut Vec<u8>) {
    for byte in value.bytes() {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\n' => out.extend_from_slice(b"\\n"),
            byte => out.push(byte),
        }
    }
}

fn unescape(input: &[u8], limit: usize, field: &str) -> Result<String, FormatError> {
    let mut out = Vec::with_capacity(input.len().min(limit));
    let mut at = 0;
    while at < input.len() {
        let byte = input[at];
        at += 1;
        if byte == b'\\' {
            let Some(escaped) = input.get(at) else {
                return Err(FormatError::new(format!(
                    "trailing escape in bookmark {field}"
                )));
            };
            at += 1;
            out.push(match escaped {
                b'\\' => b'\\',
                b't' => b'\t',
                b'r' => b'\r',
                b'n' => b'\n',
                _ => {
                    return Err(FormatError::new(format!(
                        "unknown escape in bookmark {field}"
                    )));
                }
            });
        } else {
            out.push(byte);
        }
        if out.len() > limit {
            return Err(FormatError::new(format!(
                "bookmark {field} exceeds its size limit"
            )));
        }
    }
    String::from_utf8(out).map_err(|_| FormatError::new(format!("bookmark {field} is not UTF-8")))
}

pub fn resolve_path(
    override_path: Option<&str>,
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(path) = override_path.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if let Some(root) = absolute_nonempty(xdg_data_home) {
        return Some(root.join("yata/bookmarks"));
    }
    absolute_nonempty(home).map(|root| root.join(".local/share/yata/bookmarks"))
}

fn absolute_nonempty(value: Option<&str>) -> Option<&Path> {
    value
        .filter(|value| !value.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bookmark(url: &str, title: &str) -> Bookmark {
        Bookmark {
            url: Arc::from(url),
            title: Arc::from(title),
        }
    }

    #[test]
    fn ordered_model_is_newest_first_and_duplicates_do_not_move() {
        let mut bookmarks = Bookmarks::new();
        assert_eq!(
            bookmarks.add(Arc::from("https://a.test/"), Arc::from("A")),
            AddResult::Added
        );
        assert_eq!(
            bookmarks.add(Arc::from("https://b.test/#part"), Arc::from("B")),
            AddResult::Added
        );
        assert_eq!(
            bookmarks.add(Arc::from("https://a.test/"), Arc::from("new title")),
            AddResult::Duplicate
        );
        assert_eq!(
            bookmarks.records(),
            &[
                bookmark("https://b.test/#part", "B"),
                bookmark("https://a.test/", "A")
            ]
        );
        assert_eq!(bookmarks.remove(0).unwrap().title.as_ref(), "B");
        assert_eq!(bookmarks.records()[0].title.as_ref(), "A");
    }

    #[test]
    fn codec_round_trips_escaped_and_unicode_records_deterministically() {
        let records = vec![
            bookmark(
                "https://example.test/a?x=1#%E7%8C%AB",
                "slashes / \\ tab\t CR\r LF\n 猫 😀",
            ),
            bookmark("http://example.test/", "plain"),
        ];
        let once = encode(&records).unwrap();
        assert_eq!(once, encode(&records).unwrap());
        assert_eq!(decode(&once).unwrap().records(), records);
    }

    #[test]
    fn decoder_rejects_malformed_data_without_partial_acceptance() {
        for bad in [
            b"".as_slice(),
            b"yata-bookmarks-v2\n",
            b"yata-bookmarks-v1\nhttps://a.test/\ttitle",
            b"yata-bookmarks-v1\nhttps://a.test/\n",
            b"yata-bookmarks-v1\nhttps://a.test/\ttitle\textra\n",
            b"yata-bookmarks-v1\nhttps://a.test/\tbad\\q\n",
            b"yata-bookmarks-v1\nhttps://a.test/\tbad\\\n",
            b"yata-bookmarks-v1\nftp://a.test/\ttitle\n",
            b"yata-bookmarks-v1\nhttps://A.test\ttitle\n",
            b"yata-bookmarks-v1\nhttps://a.test/\tone\nhttps://a.test/\ttwo\n",
        ] {
            assert!(
                decode(bad).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let invalid = [HEADER, b"https://a.test/\t", &[0xff], b"\n"].concat();
        assert!(decode(&invalid).is_err());
    }

    #[test]
    fn byte_limits_are_exact_and_character_safe() {
        let title = "x".repeat(MAX_TITLE_BYTES);
        assert!(encode(&[bookmark("https://a.test/", &title)]).is_ok());
        assert!(encode(&[bookmark("https://a.test/", &(title + "x"))]).is_err());
        assert!(sanitize_title(&"猫".repeat(300)).len() <= MAX_TITLE_BYTES);
        assert!(decode(&vec![b'x'; MAX_FILE_BYTES + 1]).is_err());
    }

    #[test]
    fn collection_refuses_the_1025th_record() {
        let mut bookmarks = Bookmarks::new();
        for n in 0..MAX_BOOKMARKS {
            assert_eq!(
                bookmarks.add(
                    Arc::from(format!("https://example.test/{n}")),
                    Arc::from("title")
                ),
                AddResult::Added
            );
        }
        assert_eq!(
            bookmarks.add(Arc::from("https://example.test/full"), Arc::from("title")),
            AddResult::Full
        );
    }

    #[test]
    fn path_resolution_obeys_precedence_and_absolute_roots() {
        assert_eq!(
            resolve_path(Some("relative/file"), Some("/xdg"), Some("/home")),
            Some(PathBuf::from("relative/file"))
        );
        assert_eq!(
            resolve_path(None, Some("/xdg"), Some("/home")),
            Some(PathBuf::from("/xdg/yata/bookmarks"))
        );
        assert_eq!(
            resolve_path(None, Some("relative"), Some("/home")),
            Some(PathBuf::from("/home/.local/share/yata/bookmarks"))
        );
        assert_eq!(resolve_path(None, Some(""), Some("relative")), None);
    }
}
