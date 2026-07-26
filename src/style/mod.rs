//! Style resolution: DOM + stylesheets → computed values (PLAN.md §2, M4.2).
//!
//! The semantics half of M4. `css/` decided what the author *wrote*; this
//! decides what each node *is*: which rules match it, which declaration wins,
//! and what it inherits from its parent. Input is a `&Dom` and the page's
//! stylesheets, output is one `ComputedStyle` per `NodeId` — a pure transform,
//! like every other stage.
//!
//! Nothing renders differently yet: layout and paint keep M3's hardcoded
//! styling until M4.4 rewires them onto these values.

pub mod values;

use values::{ColorValue, Display, FontStyle, FontWeight, TextAlign};

/// What a node looks like once the cascade and inheritance have run. `Default`
/// is the CSS initial value of every property, which is also what a node with
/// no matching rule and no parent gets.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: ColorValue,
    pub background_color: ColorValue,
    pub font_weight: FontWeight,
    pub font_style: FontStyle,
    /// `text-decoration`, as much of it as a cell grid has: underlined or not.
    pub underline: bool,
    pub text_align: TextAlign,
}
