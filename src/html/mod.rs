//! HTML front end: bytes → tokens → DOM tree.
//!
//! Charset: for now every fetched body is decoded as UTF-8, lossily. That is the
//! one seam where `encoding_rs` would slot in — PLAN.md M2 says pull it only when
//! a ladder page needs a non-UTF-8 decode, and none of them do yet.
//!
// Nothing wires the parser into `App` yet — the F1 inspector and bench (M2.3) are
// the first consumers. The inner allow covers this module and its children.
#![allow(dead_code, unused_imports)]

mod tokenizer;
mod tree_builder;

pub use tokenizer::{Token, tokenize};
pub use tree_builder::{build, debug_tree, parse};

/// Decode a fetched body to a string. Lossy UTF-8 today; the charset-detection
/// seam (see module docs) lives here when a page forces the issue.
pub fn decode_body(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
