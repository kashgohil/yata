//! CSS front end: stylesheet text → `Stylesheet` (PLAN.md M4).
//!
//! This module is the syntax half of M4 and knows nothing about meaning. It
//! parses `frobnicate: sideways` as readily as `color: red` and hands both on
//! as strings; which properties exist, what `#348` means and which declaration
//! wins are all questions for `style/` (M4.2), where computed values live.
//! Keeping the seam there is what lets this half be finished and tested against
//! real fixture CSS with nothing downstream of it yet.
//!
//! Nothing consumes a `Stylesheet` yet: `<style>` blocks and `<link>` sheets
//! reach the engine in M4.3.

mod tokenizer;

pub use tokenizer::{Token, Tokenizer, tokenize};
