//! Bounded session checkpoints: ordered URLs and ordinary-page scroll rows.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::browser::app::MAX_TABS;
use crate::browser::bookmarks::{MAX_URL_BYTES, valid_url};

pub const MAX_FILE_BYTES: usize = 256 * 1024;
const HEADER: &[u8] = b"yata-session-v1\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTab {
    pub url: Option<Arc<str>>,
    pub scroll: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub active: usize,
    pub tabs: Arc<[SessionTab]>,
}

impl SessionSnapshot {
    pub fn new(active: usize, tabs: Arc<[SessionTab]>) -> Result<Self, FormatError> {
        let snapshot = Self { active, tabs };
        validate(&snapshot)?;
        Ok(snapshot)
    }
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

pub fn encode(snapshot: &SessionSnapshot) -> Result<Vec<u8>, FormatError> {
    validate(snapshot)?;
    let mut out = Vec::with_capacity(HEADER.len().saturating_add(snapshot.tabs.len() * 64));
    out.extend_from_slice(HEADER);
    out.extend_from_slice(format!("active\t{}\n", snapshot.active).as_bytes());
    for tab in snapshot.tabs.iter() {
        out.extend_from_slice(format!("tab\t{}\t", tab.scroll).as_bytes());
        if let Some(url) = &tab.url {
            escape_into(url, &mut out);
        }
        out.push(b'\n');
        if out.len() > MAX_FILE_BYTES {
            return Err(FormatError::new("session file is too large"));
        }
    }
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<SessionSnapshot, FormatError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(FormatError::new("session file is too large"));
    }
    if !bytes.starts_with(HEADER) {
        return Err(FormatError::new(if bytes.is_empty() {
            "session file is empty"
        } else {
            "unsupported session file header"
        }));
    }
    let body = &bytes[HEADER.len()..];
    if body.is_empty() {
        return Err(FormatError::new("session has no records"));
    }
    if !body.ends_with(b"\n") {
        return Err(FormatError::new("session record is not newline terminated"));
    }

    let body = &body[..body.len() - 1];
    if body.is_empty() {
        return Err(FormatError::new("session has no records"));
    }
    let mut active = None;
    let mut tabs = Vec::new();
    for (line_index, line) in body.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            return Err(FormatError::new("session record is empty"));
        }
        let mut fields = line.split(|byte| *byte == b'\t');
        let kind = fields.next().expect("nonempty line has a record kind");
        match kind {
            b"active" => {
                if line_index != 0 {
                    return Err(FormatError::new("active record is not first"));
                }
                if active.is_some() {
                    return Err(FormatError::new("duplicate active record"));
                }
                let ordinal = fields
                    .next()
                    .ok_or_else(|| FormatError::new("active record is missing a field"))?;
                if fields.next().is_some() {
                    return Err(FormatError::new("active record has extra fields"));
                }
                active =
                    Some(parse_decimal(ordinal, (MAX_TABS - 1) as u64, "active ordinal")? as usize);
            }
            b"tab" => {
                if line_index == 0 {
                    return Err(FormatError::new(
                        "session is missing its first active record",
                    ));
                }
                if tabs.len() == MAX_TABS {
                    return Err(FormatError::new("too many session tabs"));
                }
                let scroll = fields
                    .next()
                    .ok_or_else(|| FormatError::new("tab record is missing its scroll field"))?;
                let url = fields
                    .next()
                    .ok_or_else(|| FormatError::new("tab record is missing its URL field"))?;
                if fields.next().is_some() {
                    return Err(FormatError::new("tab record has extra fields"));
                }
                let scroll = parse_decimal(scroll, i32::MAX as u64, "scroll offset")? as u32;
                let url = unescape(url, MAX_URL_BYTES)?;
                let url = if url.is_empty() {
                    if scroll != 0 {
                        return Err(FormatError::new("blank tab has a nonzero scroll offset"));
                    }
                    None
                } else {
                    if !valid_url(&url) {
                        return Err(FormatError::new("session URL is not normalized HTTP(S)"));
                    }
                    Some(Arc::from(url))
                };
                tabs.push(SessionTab { url, scroll });
            }
            _ => return Err(FormatError::new("unknown session record kind")),
        }
    }
    let active = active.ok_or_else(|| FormatError::new("missing active record"))?;
    SessionSnapshot::new(active, Arc::from(tabs))
}

fn validate(snapshot: &SessionSnapshot) -> Result<(), FormatError> {
    if snapshot.tabs.is_empty() {
        return Err(FormatError::new("session has no tabs"));
    }
    if snapshot.tabs.len() > MAX_TABS {
        return Err(FormatError::new("too many session tabs"));
    }
    if snapshot.active >= snapshot.tabs.len() {
        return Err(FormatError::new("active ordinal is out of range"));
    }
    for tab in snapshot.tabs.iter() {
        if tab.scroll > i32::MAX as u32 {
            return Err(FormatError::new("scroll offset exceeds its limit"));
        }
        match &tab.url {
            None if tab.scroll != 0 => {
                return Err(FormatError::new("blank tab has a nonzero scroll offset"));
            }
            Some(url) if url.is_empty() || url.len() > MAX_URL_BYTES || !valid_url(url) => {
                return Err(FormatError::new("session URL is not normalized HTTP(S)"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_decimal(input: &[u8], max: u64, field: &str) -> Result<u64, FormatError> {
    if input.is_empty() || input.iter().any(|byte| !byte.is_ascii_digit()) {
        return Err(FormatError::new(format!("malformed {field}")));
    }
    let mut value = 0u64;
    for byte in input {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(byte - b'0')))
            .filter(|value| *value <= max)
            .ok_or_else(|| FormatError::new(format!("{field} exceeds its limit")))?;
    }
    Ok(value)
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

fn unescape(input: &[u8], limit: usize) -> Result<String, FormatError> {
    let mut out = Vec::with_capacity(input.len().min(limit));
    let mut at = 0;
    while at < input.len() {
        let byte = input[at];
        at += 1;
        if byte == b'\\' {
            let escaped = *input
                .get(at)
                .ok_or_else(|| FormatError::new("trailing escape in session URL"))?;
            at += 1;
            out.push(match escaped {
                b'\\' => b'\\',
                b't' => b'\t',
                b'r' => b'\r',
                b'n' => b'\n',
                _ => return Err(FormatError::new("unknown escape in session URL")),
            });
        } else if byte == b'\r' {
            return Err(FormatError::new("unescaped carriage return in session URL"));
        } else {
            out.push(byte);
        }
        if out.len() > limit {
            return Err(FormatError::new("session URL exceeds its size limit"));
        }
    }
    String::from_utf8(out).map_err(|_| FormatError::new("session URL is not UTF-8"))
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
        return Some(root.join("yata/session"));
    }
    absolute_nonempty(home).map(|root| root.join(".local/share/yata/session"))
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

    fn tab(url: Option<&str>, scroll: u32) -> SessionTab {
        SessionTab {
            url: url.map(Arc::from),
            scroll,
        }
    }

    #[test]
    fn codec_round_trips_bounds_and_is_deterministic() {
        let prefix = "https://example.test/";
        let maximum = format!("{prefix}{}", "x".repeat(MAX_URL_BYTES - prefix.len()));
        let tabs: Vec<_> = (0..MAX_TABS)
            .map(|n| {
                if n == 0 {
                    tab(None, 0)
                } else if n == MAX_TABS - 1 {
                    tab(Some(&maximum), i32::MAX as u32)
                } else {
                    tab(Some(&format!("https://example.test/{n}#part")), n as u32)
                }
            })
            .collect();
        for active in 0..MAX_TABS {
            let snapshot = SessionSnapshot::new(active, Arc::from(tabs.clone())).unwrap();
            let encoded = encode(&snapshot).unwrap();
            assert_eq!(encoded, encode(&snapshot).unwrap());
            assert_eq!(decode(&encoded).unwrap(), snapshot);
        }
        let blank = SessionSnapshot::new(0, Arc::from([tab(None, 0)])).unwrap();
        assert_eq!(decode(&encode(&blank).unwrap()).unwrap(), blank);
    }

    #[test]
    fn decoder_rejects_malformed_data_without_partial_acceptance() {
        for bad in [
            b"".as_slice(),
            b"yata-session-v2\nactive\t0\ntab\t0\t\n",
            b"yata-session-v1\n",
            b"yata-session-v1\nactive\t0\ntab\t0\t",
            b"yata-session-v1\ntab\t0\thttps://a.test/\n",
            b"yata-session-v1\ntab\t0\thttps://a.test/\nactive\t0\n",
            b"yata-session-v1\nactive\t0\nactive\t0\ntab\t0\t\n",
            b"yata-session-v1\nactive\t1\ntab\t0\t\n",
            b"yata-session-v1\nactive\t-1\ntab\t0\t\n",
            b"yata-session-v1\nactive\t0x0\ntab\t0\t\n",
            b"yata-session-v1\nactive\t0\ntab\t2147483648\thttps://a.test/\n",
            b"yata-session-v1\nactive\t0\ntab\t1\t\n",
            b"yata-session-v1\nactive\t0\ntab\t0\tftp://a.test/\n",
            b"yata-session-v1\nactive\t0\ntab\t0\thttps://A.test\n",
            b"yata-session-v1\nactive\t0\ntab\t0\thttps://a.test/\\q\n",
            b"yata-session-v1\nactive\t0\ntab\t0\thttps://a.test/\\\n",
            b"yata-session-v1\nactive\t0\ntab\t0\thttps://a.test/\textra\n",
            b"yata-session-v1\nunknown\t0\n",
            b"yata-session-v1\nactive\t0\n\n",
        ] {
            assert!(
                decode(bad).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let invalid_utf8 = [
            HEADER,
            b"active\t0\ntab\t0\thttps://a.test/",
            &[0xff],
            b"\n",
        ]
        .concat();
        assert!(decode(&invalid_utf8).is_err());
        assert!(decode(&vec![b'x'; MAX_FILE_BYTES + 1]).is_err());

        let mut too_many = HEADER.to_vec();
        too_many.extend_from_slice(b"active\t0\n");
        for n in 0..=MAX_TABS {
            too_many.extend_from_slice(format!("tab\t0\thttps://example.test/{n}\n").as_bytes());
        }
        assert!(decode(&too_many).is_err());
    }

    #[test]
    fn model_refuses_invalid_outbound_snapshots() {
        assert!(SessionSnapshot::new(0, Arc::from([])).is_err());
        assert!(SessionSnapshot::new(1, Arc::from([tab(None, 0)])).is_err());
        assert!(SessionSnapshot::new(0, Arc::from([tab(None, 1)])).is_err());
        assert!(SessionSnapshot::new(0, Arc::from([tab(Some("http://A.test"), 0)])).is_err());
        assert!(
            SessionSnapshot::new(
                0,
                Arc::from([tab(Some("https://a.test/"), i32::MAX as u32 + 1)])
            )
            .is_err()
        );
    }

    #[test]
    fn every_truncation_and_single_byte_corruption_is_bounded_and_panic_free() {
        let valid = encode(
            &SessionSnapshot::new(
                1,
                Arc::from([tab(Some("https://a.test/path?x=1#part"), 37), tab(None, 0)]),
            )
            .unwrap(),
        )
        .unwrap();
        for end in 0..valid.len() {
            let _ = decode(&valid[..end]);
        }
        for at in 0..valid.len() {
            for byte in [0, b'\t', b'\n', b'\\', b'9', 0xff] {
                let mut corrupt = valid.clone();
                corrupt[at] = byte;
                let _ = decode(&corrupt);
            }
        }
    }

    #[test]
    fn path_resolution_obeys_precedence_and_absolute_roots() {
        assert_eq!(
            resolve_path(Some("relative/file"), Some("/xdg"), Some("/home")),
            Some(PathBuf::from("relative/file"))
        );
        assert_eq!(
            resolve_path(None, Some("/xdg"), Some("/home")),
            Some(PathBuf::from("/xdg/yata/session"))
        );
        assert_eq!(
            resolve_path(None, Some("relative"), Some("/home")),
            Some(PathBuf::from("/home/.local/share/yata/session"))
        );
        assert_eq!(resolve_path(None, Some(""), Some("relative")), None);
    }

    #[test]
    #[ignore = "release checkpoint codec measurement"]
    fn measure_maximum_session_checkpoint_codec() {
        use std::time::Instant;

        let prefix = "https://example.test/";
        let url = format!("{prefix}{}", "x".repeat(MAX_URL_BYTES - prefix.len()));
        let snapshot = SessionSnapshot::new(
            MAX_TABS - 1,
            Arc::from(
                (0..MAX_TABS)
                    .map(|n| tab(Some(&url), n as u32))
                    .collect::<Vec<_>>(),
            ),
        )
        .unwrap();
        let bytes = encode(&snapshot).unwrap();
        const ROUNDS: u32 = 1_000;
        let started = Instant::now();
        for _ in 0..ROUNDS {
            assert_eq!(decode(&bytes).unwrap(), snapshot);
        }
        eprintln!(
            "M11.24 maximum 16-tab / 8,192-byte URL decode: {:?} mean ({} bytes)",
            started.elapsed() / ROUNDS,
            bytes.len()
        );
    }
}
