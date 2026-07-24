//! HTML front end: bytes → tokens (this task) → tree (M2.2).
//!
//! Charset: for now every fetched body is decoded as UTF-8, lossily. That is the
//! one seam where `encoding_rs` would slot in — PLAN.md M2 says pull it only when
//! a ladder page needs a non-UTF-8 decode, and none of them do yet.
//!
// M2.1 lands the tokenizer with no consumer yet — the tree builder (M2.2) is the
// first caller. The inner allow covers this module and `tokenizer` below.
#![allow(dead_code, unused_imports)]

mod tokenizer;

pub use tokenizer::{Token, tokenize};

/// Decode a fetched body to a string. Lossy UTF-8 today; the charset-detection
/// seam (see module docs) lives here when a page forces the issue.
pub fn decode_body(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
