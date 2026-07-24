//! The yata engine as a library target. The binary (`main.rs`) and the
//! criterion bench (`benches/parse.rs`) are the two consumers; a bench cannot
//! import from a bin crate, which is why this split exists at all.

pub mod browser;
pub mod dom;
pub mod html;
pub mod msg;
pub mod net;
pub mod term;
