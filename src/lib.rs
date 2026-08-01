//! The yata engine as a library target. The binary (`main.rs`) and the criterion
//! benches (`benches/parse.rs`, `benches/scroll.rs`) are the consumers; a bench
//! cannot import from a bin crate, which is why this split exists at all. The
//! scroll bench drives `browser::app::App` and `term::Renderer` directly, so
//! those stay public for it as much as for the binary.

pub mod browser;
pub mod css;
pub mod dom;
pub mod html;
pub mod image;
pub mod layout;
pub mod msg;
pub mod net;
pub mod paint;
pub mod style;
pub mod term;
