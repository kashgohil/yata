//! HTML front end: bytes → tokens → DOM tree.
//!
//! Charset: for now every fetched body is decoded as UTF-8, lossily. That is the
//! one seam where `encoding_rs` would slot in — PLAN.md M2 says pull it only when
//! a ladder page needs a non-UTF-8 decode, and none of them do yet.
//!
mod tokenizer;
mod tree_builder;

pub use tokenizer::{Token, tokenize};
pub use tree_builder::{build, debug_tree, parse};

/// Decode a fetched body to a string. Lossy UTF-8 today; the charset-detection
/// seam (see module docs) lives here when a page forces the issue.
pub fn decode_body(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::decode_body;

    #[test]
    fn decode_body_is_lossy_utf8() {
        assert_eq!(decode_body("héllo".as_bytes()), "héllo");
        // An invalid byte becomes U+FFFD rather than panicking — the seam holds
        // until a page forces real charset detection.
        assert_eq!(decode_body(&[0x68, 0xFF, 0x69]), "h\u{FFFD}i");
    }
}
