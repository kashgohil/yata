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

pub mod matching;
pub mod values;

use std::sync::OnceLock;

use crate::css::{self, Declaration, Stylesheet};
use crate::dom::{Dom, NodeData, NodeId};
use matching::RuleIndex;
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

impl ComputedStyle {
    /// What a child starts from: the inherited properties of its parent, with
    /// every non-inherited one back at its initial value. `display` and
    /// `background-color` do not inherit — a `<span>` inside a block is not
    /// itself a block, and a paragraph's background does not tint its words.
    ///
    /// `text-decoration` is inherited here, and in CSS it is not: real
    /// decoration propagates to descendants by a separate mechanism that draws
    /// the parent's line *through* the child's box. Inheriting it is the cheap
    /// approximation that puts the underline on a link's inner `<span>`, which
    /// is where a browser also draws one.
    fn inherit(&self) -> ComputedStyle {
        ComputedStyle {
            display: Display::default(),
            background_color: ColorValue::default(),
            color: self.color,
            font_weight: self.font_weight,
            font_style: self.font_style,
            underline: self.underline,
            text_align: self.text_align,
        }
    }
}

/// Computed values for every node, indexed by `NodeId`. Dense rather than a
/// map because the arena is dense: one slot per node, text nodes included, so
/// paint can ask any node what it looks like without a lookup that can miss.
pub struct Styles {
    computed: Vec<ComputedStyle>,
}

impl Styles {
    pub fn get(&self, id: NodeId) -> &ComputedStyle {
        &self.computed[id.0 as usize]
    }
}

/// The user-agent stylesheet: how bare HTML looks before any page says
/// otherwise. Parsed once — it is a constant, and re-parsing it per page would
/// be pure waste in the restyle budget.
pub fn ua_stylesheet() -> &'static Stylesheet {
    static UA: OnceLock<Stylesheet> = OnceLock::new();
    UA.get_or_init(|| css::parse(include_str!("ua.css")))
}

/// Resolve every node's computed values against the UA sheet, the page's
/// sheets in order, and each element's `style=""` attribute.
pub fn style_tree(dom: &Dom, sheets: &[Stylesheet]) -> Styles {
    let ua = RuleIndex::build(std::slice::from_ref(ua_stylesheet()));
    let author = RuleIndex::build(sheets);
    let mut styles = Styles {
        computed: vec![ComputedStyle::default(); dom.node_count()],
    };
    // One pre-order walk: a node's parent is always resolved before it, which
    // is the whole of inheritance. No second pass, no fixpoint.
    resolve(
        dom,
        dom.root,
        &ComputedStyle::default(),
        &ua,
        &author,
        &mut styles,
    );
    styles
}

fn resolve(
    dom: &Dom,
    node: NodeId,
    parent: &ComputedStyle,
    ua: &RuleIndex,
    author: &RuleIndex,
    out: &mut Styles,
) {
    let computed = match &dom.node(node).data {
        NodeData::Element { .. } => cascade(dom, node, parent, ua, author),
        // Text, comments and the document root match no selector; they carry
        // their parent's inherited values so paint can style a text run by
        // asking the text node itself.
        _ => parent.inherit(),
    };
    out.computed[node.0 as usize] = computed;
    for child in dom.children(node) {
        resolve(dom, child, &computed, ua, author, out);
    }
}

/// Cascade origin and importance, low priority to high. The order is CSS's:
/// author rules beat the UA sheet, `!important` inverts the author/UA
/// relationship, and the UA sheet's `!important` is the one thing a page
/// cannot override.
///
/// Inline declarations get their own ranks rather than a huge specificity,
/// because `style=""` beating every selector is a fact about *origin*, not
/// about how specific the author was.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Rank {
    UaNormal,
    AuthorNormal,
    InlineNormal,
    AuthorImportant,
    InlineImportant,
    UaImportant,
}

/// One declaration in the running, with everything the sort needs.
struct Entry<'a> {
    rank: Rank,
    specificity: (u16, u16, u16),
    /// Position within its own origin — the last tie-break, so a later rule of
    /// equal weight wins.
    order: usize,
    declaration: &'a Declaration,
}

fn cascade(
    dom: &Dom,
    node: NodeId,
    parent: &ComputedStyle,
    ua: &RuleIndex,
    author: &RuleIndex,
) -> ComputedStyle {
    let mut entries: Vec<Entry> = Vec::new();
    for (index, normal, important) in [
        (ua, Rank::UaNormal, Rank::UaImportant),
        (author, Rank::AuthorNormal, Rank::AuthorImportant),
    ] {
        for candidate in index.matches(dom, node) {
            let specificity = candidate.selector.specificity();
            for declaration in candidate.declarations {
                entries.push(Entry {
                    rank: if declaration.important {
                        important
                    } else {
                        normal
                    },
                    specificity,
                    order: candidate.order,
                    declaration,
                });
            }
        }
    }

    // `style=""` is parsed per element and per restyle. It is a handful of
    // declarations on the elements that have one at all; caching it would mean
    // holding parsed state between stages, which is what the pipeline forbids.
    let inline = dom
        .attr(node, "style")
        .map(css::parse_declarations)
        .unwrap_or_default();
    for (order, declaration) in inline.iter().enumerate() {
        entries.push(Entry {
            rank: if declaration.important {
                Rank::InlineImportant
            } else {
                Rank::InlineNormal
            },
            specificity: (0, 0, 0),
            order,
            declaration,
        });
    }

    entries.sort_by_key(|e| (e.rank, e.specificity, e.order));

    let mut computed = parent.inherit();
    for entry in &entries {
        apply(&mut computed, entry.declaration);
    }
    computed
}

/// Apply one declaration, if it is a property M4 implements and its value
/// parses. An unparseable value leaves the previous winner standing — that is
/// CSS's rule for invalid values, and it is why `color: bananas` must not
/// resolve to anything.
fn apply(computed: &mut ComputedStyle, declaration: &Declaration) {
    let value = declaration.value.as_str();
    match declaration.name.as_str() {
        "display" => set(&mut computed.display, values::parse_display(value)),
        "color" => set(&mut computed.color, values::parse_color(value)),
        "background-color" => set(&mut computed.background_color, values::parse_color(value)),
        // The `background` shorthand, honoured only when the whole value is a
        // colour (`background:#eee`, which is example.com's). Anything with an
        // image or a position in it is left alone rather than half-applied.
        "background" => set(&mut computed.background_color, values::parse_color(value)),
        "font-weight" => set(&mut computed.font_weight, values::parse_font_weight(value)),
        "font-style" => set(&mut computed.font_style, values::parse_font_style(value)),
        "text-align" => set(&mut computed.text_align, values::parse_text_align(value)),
        "text-decoration" | "text-decoration-line" => {
            set(
                &mut computed.underline,
                values::parse_text_decoration(value),
            );
        }
        _ => {}
    }
}

fn set<T>(slot: &mut T, parsed: Option<T>) {
    if let Some(value) = parsed {
        *slot = value;
    }
}

#[cfg(test)]
mod tests {
    use super::values::*;
    use super::*;
    use crate::html;

    /// Style `html_src` with `css_src` as its only author sheet.
    fn styled(html_src: &str, css_src: &str) -> (Dom, Styles) {
        let dom = html::parse(html_src);
        let sheets = vec![css::parse(css_src)];
        let styles = style_tree(&dom, &sheets);
        (dom, styles)
    }

    fn find(dom: &Dom, tag: &str) -> NodeId {
        fn walk(dom: &Dom, id: NodeId, tag: &str) -> Option<NodeId> {
            if matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag) {
                return Some(id);
            }
            dom.children(id).find_map(|child| walk(dom, child, tag))
        }
        walk(dom, dom.root, tag).expect("fixture is missing that tag")
    }

    fn color_of(html_src: &str, css_src: &str, tag: &str) -> ColorValue {
        let (dom, styles) = styled(html_src, css_src);
        styles.get(find(&dom, tag)).color
    }

    const RED: ColorValue = ColorValue::Rgb(255, 0, 0);
    const BLUE: ColorValue = ColorValue::Rgb(0, 0, 255);

    #[test]
    fn higher_specificity_wins_regardless_of_order() {
        assert_eq!(
            color_of(
                "<p class='x'>t</p>",
                ".x { color: red } p { color: blue }",
                "p"
            ),
            RED
        );
    }

    #[test]
    fn equal_specificity_gives_it_to_the_later_rule() {
        assert_eq!(
            color_of("<p>t</p>", "p { color: blue } p { color: red }", "p"),
            RED
        );
    }

    #[test]
    fn a_style_attribute_beats_an_id_rule() {
        assert_eq!(
            color_of(
                "<p id='a' style='color: red'>t</p>",
                "#a { color: blue }",
                "p"
            ),
            RED
        );
    }

    #[test]
    fn important_in_a_page_sheet_beats_a_style_attribute() {
        assert_eq!(
            color_of(
                "<p style='color: blue'>t</p>",
                "p { color: red !important }",
                "p"
            ),
            RED
        );
        // ...and an important style attribute takes it back.
        assert_eq!(
            color_of(
                "<p style='color: red !important'>t</p>",
                "p { color: blue !important }",
                "p"
            ),
            RED
        );
    }

    #[test]
    fn the_ua_sheet_loses_to_any_author_rule() {
        // The UA sheet says links are #5c5cff; a page rule of specificity
        // (0,0,1) still wins, because origin outranks specificity.
        assert_eq!(color_of("<a href='/x'>t</a>", "a { color: red }", "a"), RED);
    }

    #[test]
    fn an_invalid_value_drops_only_itself() {
        // `color: bananas` must not resolve to anything — blue stands.
        assert_eq!(
            color_of("<p>t</p>", "p { color: blue } p { color: bananas }", "p"),
            BLUE
        );
    }

    #[test]
    fn inheritance_reaches_a_grandchild_but_backgrounds_do_not() {
        let (dom, styles) = styled(
            "<div><section><em>t</em></section></div>",
            "div { color: red; background-color: blue }",
        );
        let em = styles.get(find(&dom, "em"));
        assert_eq!(em.color, RED);
        assert_eq!(em.font_style, FontStyle::Italic); // from the UA sheet
        assert_eq!(em.background_color, ColorValue::Default);
        // The div keeps its own background; only inheritance was blocked.
        assert_eq!(styles.get(find(&dom, "div")).background_color, BLUE);
    }

    #[test]
    fn a_text_node_carries_its_parents_computed_colour() {
        // Paint styles a text run by asking the text node itself, so text
        // nodes need real values rather than a default slot.
        let (dom, styles) = styled("<p>hello</p>", "p { color: red }");
        let p = find(&dom, "p");
        let text = dom.children(p).next().unwrap();
        assert!(matches!(dom.node(text).data, NodeData::Text(_)));
        assert_eq!(styles.get(text).color, RED);
        // Text is not a block, whatever its parent is.
        assert_eq!(styles.get(text).display, Display::Inline);
        assert_eq!(styles.get(p).display, Display::Block);
    }

    #[test]
    fn display_none_does_not_stop_descendants_computing() {
        // Skipping the subtree is layout's job (M4.4). Style still resolves it,
        // which is what lets `display:none` be flipped later without a restyle
        // of the whole document.
        let (dom, styles) = styled(
            "<div style='display: none'><p>t</p></div>",
            "p { color: red }",
        );
        assert_eq!(styles.get(find(&dom, "div")).display, Display::None);
        assert_eq!(styles.get(find(&dom, "p")).display, Display::Block);
        assert_eq!(styles.get(find(&dom, "p")).color, RED);
    }

    #[test]
    fn the_ua_sheet_parses_with_nothing_dropped() {
        let sheet = ua_stylesheet();
        // One rule per `{` in the file, comments excluded — the sheet documents
        // itself in CSS comments, and one of those contains a brace. Counting
        // rather than pinning a number means this keeps biting as the sheet
        // grows: a rule dropped for syntax the parser dislikes shows up here as
        // an off-by-one instead of as a silently unstyled tag.
        let blocks = strip_comments(include_str!("ua.css")).matches('{').count();
        assert_eq!(sheet.rules.len(), blocks);
        assert!(blocks >= 6);
        // A rule that parsed but lost all its declarations would style nothing.
        assert!(
            sheet
                .rules
                .iter()
                .all(|r| !r.declarations.is_empty() && !r.selectors.is_empty())
        );
    }

    fn strip_comments(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some((before, after)) = rest.split_once("/*") {
            out.push_str(before);
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn bare_html_gets_its_user_agent_styling() {
        let (dom, styles) = styled(
            "<h1>Title</h1><p>t</p><a href='/x'>link</a><script>var x = 1</script>",
            "",
        );
        let h1 = styles.get(find(&dom, "h1"));
        assert_eq!(h1.display, Display::Block);
        assert_eq!(h1.font_weight, FontWeight::Bold);
        // Everything the sheet does not mention stays initial.
        assert_eq!(h1.color, ColorValue::Default);
        assert_eq!(h1.text_align, TextAlign::Left);

        let a = styles.get(find(&dom, "a"));
        assert!(a.underline);
        // ANSI 12's RGB, so M4.4's nearest-256 map lands on the colour M3 draws.
        assert_eq!(a.color, ColorValue::Rgb(0x5c, 0x5c, 0xff));
        assert_eq!(a.display, Display::Inline);

        assert_eq!(styles.get(find(&dom, "script")).display, Display::None);
        assert_eq!(styles.get(find(&dom, "head")).display, Display::None);
    }

    #[test]
    fn a_bare_anchor_is_not_a_link() {
        let (dom, styles) = styled("<a name='top'>t</a>", "");
        assert!(!styles.get(find(&dom, "a")).underline);
    }

    #[test]
    fn junk_in_a_style_attribute_is_survivable() {
        let (dom, styles) = styled(
            "<p style='color; ;; font-weight: bold; color: red'>t</p>",
            "",
        );
        let p = styles.get(find(&dom, "p"));
        assert_eq!(p.font_weight, FontWeight::Bold);
        assert_eq!(p.color, RED);
    }
}

/// Ladder proof: run the real stage over the committed fixtures. Two things
/// are checked here that no hand-built document can check — that the rule
/// index agrees with the naive matcher on pages with thousands of rules, and
/// that the cascade produces the values these pages are supposed to show.
#[cfg(test)]
mod ladder {
    use super::*;
    use crate::html;
    use crate::style::matching::RuleIndex;
    use crate::style::values::{ColorValue, Display, FontWeight};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    /// Every `<style>` block in the page. Reaching them through the DOM is a
    /// stand-in until M4.3 gives stylesheet collection a real home.
    fn author_sheets(dom: &Dom) -> Vec<Stylesheet> {
        fn collect(dom: &Dom, id: NodeId, out: &mut Vec<Stylesheet>) {
            if matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "style") {
                let mut text = String::new();
                for child in dom.children(id) {
                    if let NodeData::Text(t) = &dom.node(child).data {
                        text.push_str(t);
                    }
                }
                out.push(css::parse(&text));
                return;
            }
            for child in dom.children(id) {
                collect(dom, child, out);
            }
        }
        let mut out = Vec::new();
        collect(dom, dom.root, &mut out);
        out
    }

    fn elements(dom: &Dom) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![dom.root];
        while let Some(id) = stack.pop() {
            if matches!(dom.node(id).data, NodeData::Element { .. }) {
                out.push(id);
            }
            stack.extend(dom.children(id));
        }
        out
    }

    fn find(dom: &Dom, tag: &str) -> NodeId {
        elements(dom)
            .into_iter()
            .filter(
                |&id| matches!(&dom.node(id).data, NodeData::Element { tag: t, .. } if t == tag),
            )
            .min_by_key(|id| id.0)
            .expect("fixture is missing that tag")
    }

    /// The index and the naive matcher must agree on every element of the page,
    /// against the UA sheet plus the page's own. A rule index that is fast
    /// because it quietly misses rules is not an optimization, and M4.5's
    /// bench of the two is only meaningful if they compute the same answer.
    fn index_agrees_with_oracle(source: &str) -> Dom {
        let dom = html::parse(source);
        let mut sheets = vec![ua_stylesheet().clone()];
        sheets.extend(author_sheets(&dom));
        let index = RuleIndex::build(&sheets);
        let mut matched = 0;
        for node in elements(&dom) {
            let fast: Vec<usize> = index.matches(&dom, node).iter().map(|c| c.order).collect();
            let naive: Vec<usize> = index
                .matches_naive(&dom, node)
                .iter()
                .map(|c| c.order)
                .collect();
            assert_eq!(fast, naive, "node {node:?}");
            matched += fast.len();
        }
        // A page where nothing matched would pass the comparison above while
        // proving nothing about the buckets.
        assert!(matched > 0, "no rule matched anything");
        dom
    }

    fn styled(source: &str) -> (Dom, Styles) {
        let dom = html::parse(source);
        let sheets = author_sheets(&dom);
        let styles = style_tree(&dom, &sheets);
        (dom, styles)
    }

    #[test]
    fn example_com_shows_its_own_colours() {
        let dom = index_agrees_with_oracle(fixture!("example.com.html"));
        let sheets = author_sheets(&dom);
        let styles = style_tree(&dom, &sheets);
        // `body{background:#eee}` — the shorthand, honoured because the whole
        // value is a colour.
        assert_eq!(
            styles.get(find(&dom, "body")).background_color,
            ColorValue::Rgb(0xee, 0xee, 0xee)
        );
        // `a:link{color:#348}` beats the UA sheet's link colour, and the UA
        // sheet's underline survives because the page never mentions it.
        let a = styles.get(find(&dom, "a"));
        assert_eq!(a.color, ColorValue::Rgb(0x33, 0x44, 0x88));
        assert!(a.underline);
    }

    #[test]
    fn motherfuckingwebsite_com_is_pure_user_agent_styling() {
        let (dom, styles) = styled(fixture!("motherfuckingwebsite.com.html"));
        // The page ships no CSS at all: everything it looks like comes from
        // ua.css, which is the point of having one.
        let h1 = styles.get(find(&dom, "h1"));
        assert_eq!(h1.font_weight, FontWeight::Bold);
        assert_eq!(h1.display, Display::Block);
        assert_eq!(styles.get(find(&dom, "p")).display, Display::Block);
    }

    #[test]
    fn danluu_com_keeps_its_links_and_its_flex_list() {
        let dom = index_agrees_with_oracle(fixture!("danluu.com.html"));
        let styles = style_tree(&dom, &author_sheets(&dom));
        let a = styles.get(find(&dom, "a"));
        assert!(a.underline);
        assert_eq!(a.color, ColorValue::Rgb(0x5c, 0x5c, 0xff));
        // `li{display:flex}` from the page's own sheet: flex is not a mode M4
        // implements, and mapping it to Block is what keeps the list stacked
        // instead of collapsing onto one line.
        assert_eq!(styles.get(find(&dom, "li")).display, Display::Block);
    }

    #[test]
    fn news_ycombinator_com_resolves_without_its_external_sheet() {
        // HN's styling lives in news.css, which M4.3 will fetch; today the page
        // is UA-styled only, and must still come out sane rather than empty.
        let dom = index_agrees_with_oracle(fixture!("news.ycombinator.com.html"));
        let styles = style_tree(&dom, &author_sheets(&dom));
        assert_eq!(styles.get(find(&dom, "table")).display, Display::Block);
        assert!(styles.get(find(&dom, "a")).underline);
    }

    #[test]
    fn en_wikipedia_org_styles_every_node() {
        let dom = index_agrees_with_oracle(fixture!("en.wikipedia.org.html"));
        let styles = style_tree(&dom, &author_sheets(&dom));
        // 1.5 MB of real markup, 21 inline sheets and 222 style attributes:
        // every node gets a slot, and the ones the page styles get its values.
        assert_eq!(styles.computed.len(), dom.node_count());
        assert_eq!(styles.get(find(&dom, "h1")).font_weight, FontWeight::Bold);
        assert_eq!(styles.get(find(&dom, "head")).display, Display::None);
    }
}
