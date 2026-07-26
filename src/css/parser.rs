//! CSS parser: tokens → `Stylesheet` (M4.1).
//!
//! Error recovery is the job, not a footnote. Real sheets are full of syntax
//! this engine does not implement — attribute selectors, `@media`, `:nth-child`
//! — and the rule everywhere is the CSS one: throw away the smallest thing that
//! is broken and keep parsing. A bad declaration costs its neighbours nothing;
//! a bad selector costs its rule; nothing costs the sheet.
//!
//! `@media` blocks are dropped whole rather than flattened. Applying a media
//! query's rules unconditionally would be worse than ignoring them (a print
//! stylesheet would repaint the screen), and honest media-query evaluation is
//! not in M4. Same for `@supports`; `@import` is skipped without fetching.

use super::tokenizer::{Token, Tokenizer};
use super::{Combinator, Compound, Declaration, PseudoClass, Rule, Selector, Stylesheet};

/// Parse a whole stylesheet. Never panics and never loops: every path consumes
/// a token or reaches `Eof`.
pub fn parse(src: &str) -> Stylesheet {
    let mut p = Parser::new(src);
    let mut rules = Vec::new();
    loop {
        p.skip_ws();
        match p.peek() {
            Token::Eof => return Stylesheet { rules },
            Token::AtKeyword(_) => {
                p.next();
                p.skip_at_rule();
            }
            _ => {
                if let Some(rule) = p.qualified_rule() {
                    rules.push(rule);
                }
            }
        }
    }
}

/// Parse a bare declaration list — the contents of a `style=""` attribute,
/// which has the syntax of a rule's block without the braces. M4.2 calls this
/// for the highest-priority origin in the cascade.
pub fn parse_declarations(src: &str) -> Vec<Declaration> {
    Parser::new(src).declarations(false)
}

/// A token plus its span in the source, so declaration values can be sliced
/// out verbatim instead of re-serialized from tokens.
struct Lexeme {
    tok: Token,
    start: usize,
    end: usize,
}

struct Parser<'a> {
    src: &'a str,
    tokenizer: Tokenizer<'a>,
    peeked: Option<Lexeme>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Parser<'a> {
        Parser {
            src,
            tokenizer: Tokenizer::new(src),
            peeked: None,
        }
    }

    fn next(&mut self) -> Lexeme {
        if let Some(lex) = self.peeked.take() {
            return lex;
        }
        let start = self.tokenizer.pos();
        let tok = self.tokenizer.next_token();
        Lexeme {
            tok,
            start,
            end: self.tokenizer.pos(),
        }
    }

    fn peek(&mut self) -> &Token {
        if self.peeked.is_none() {
            let lex = self.next();
            self.peeked = Some(lex);
        }
        // Just filled if it was empty.
        &self.peeked.as_ref().unwrap().tok
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Token::Whitespace) {
            self.next();
        }
    }

    /// Skip an at-rule whose keyword is already consumed: either to the `;` that
    /// ends a statement (`@import url(x);`) or past the balanced block that ends
    /// a nested one (`@media ... { ... }`).
    fn skip_at_rule(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.next().tok {
                Token::Eof => return,
                Token::Semicolon if depth == 0 => return,
                Token::LBrace => depth += 1,
                Token::RBrace => {
                    depth -= 1;
                    if depth <= 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// A selector list followed by a declaration block. Returns `None` when the
    /// prelude holds syntax this engine cannot evaluate — the whole rule goes,
    /// which is what CSS requires: an invalid selector in a list invalidates the
    /// rule, because keeping the valid half would apply declarations to a set
    /// the author never wrote.
    fn qualified_rule(&mut self) -> Option<Rule> {
        let mut selectors = Vec::new();
        let mut parts: Vec<(Combinator, Compound)> = Vec::new();
        let mut compound: Option<Compound> = None;
        let mut combinator = Combinator::Descendant;
        let mut bad = false;

        loop {
            let lex = self.next();
            match lex.tok {
                // An unterminated prelude means the sheet ended mid-selector:
                // there is no block to skip and nothing to keep.
                Token::Eof => return None,
                // A stray `}` at this level closes nothing; drop what we have
                // and let the caller resume after it.
                Token::RBrace => return None,
                Token::LBrace => {
                    if let Some(c) = compound.take() {
                        parts.push((combinator, c));
                    }
                    if parts.is_empty() {
                        bad = true;
                    } else {
                        selectors.push(Selector {
                            parts: std::mem::take(&mut parts),
                        });
                    }
                    break;
                }
                Token::Whitespace => {
                    // Whitespace only means "descendant" when a compound is
                    // open; leading and pre-`>` whitespace is noise.
                    if let Some(c) = compound.take() {
                        parts.push((combinator, c));
                        combinator = Combinator::Descendant;
                    }
                }
                Token::Delim('>') => {
                    if let Some(c) = compound.take() {
                        parts.push((combinator, c));
                    }
                    combinator = Combinator::Child;
                }
                Token::Comma => {
                    if let Some(c) = compound.take() {
                        parts.push((combinator, c));
                    }
                    combinator = Combinator::Descendant;
                    if parts.is_empty() {
                        bad = true;
                    } else {
                        selectors.push(Selector {
                            parts: std::mem::take(&mut parts),
                        });
                    }
                }
                Token::Ident(name) => {
                    let c = compound.get_or_insert_with(Compound::default);
                    if c.tag.is_some() {
                        bad = true;
                    }
                    c.tag = Some(name.to_ascii_lowercase());
                }
                Token::Hash(name) => {
                    let c = compound.get_or_insert_with(Compound::default);
                    if c.id.is_some() {
                        bad = true;
                    }
                    c.id = Some(name);
                }
                Token::Delim('.') => match self.next().tok {
                    Token::Ident(name) => {
                        compound
                            .get_or_insert_with(Compound::default)
                            .classes
                            .push(name);
                    }
                    _ => bad = true,
                },
                Token::Delim('*') => {
                    compound.get_or_insert_with(Compound::default);
                }
                Token::Colon => {
                    let pseudo = self.pseudo_class();
                    compound
                        .get_or_insert_with(Compound::default)
                        .pseudo
                        .push(pseudo);
                }
                // Attribute selectors, sibling combinators, stray numbers: the
                // rule is unevaluable, but keep scanning to its `{` so recovery
                // resumes at a block boundary rather than mid-selector.
                _ => bad = true,
            }
        }

        if bad {
            self.skip_block();
            return None;
        }
        let declarations = self.declarations(true);
        Some(Rule {
            selectors,
            declarations,
        })
    }

    /// A pseudo-class whose leading `:` is consumed. A second `:` marks a
    /// pseudo-*element* (`::before`), and a `(` a functional pseudo
    /// (`:not(...)`, `:nth-child(2)`) — both are kept as `Unsupported` so the
    /// rule survives as something that can never match.
    fn pseudo_class(&mut self) -> PseudoClass {
        let element = matches!(self.peek(), Token::Colon);
        if element {
            self.next();
        }
        let Token::Ident(name) = self.next().tok else {
            return PseudoClass::Unsupported(String::new());
        };
        let functional = matches!(self.peek(), Token::LParen);
        if functional {
            self.skip_parens();
        }
        if element || functional {
            return PseudoClass::Unsupported(name.to_ascii_lowercase());
        }
        match name.to_ascii_lowercase().as_str() {
            "hover" => PseudoClass::Hover,
            "visited" => PseudoClass::Visited,
            "link" => PseudoClass::Link,
            other => PseudoClass::Unsupported(other.to_string()),
        }
    }

    fn skip_parens(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.next().tok {
                Token::Eof => return,
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth <= 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Skip a `{ }` block whose opening brace is consumed, braces balanced.
    fn skip_block(&mut self) {
        let mut depth = 1i32;
        loop {
            match self.next().tok {
                Token::Eof => return,
                Token::LBrace => depth += 1,
                Token::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Declarations until the closing `}` (`in_block`) or EOF (a `style=""`
    /// attribute). Invalid declarations are dropped individually.
    fn declarations(&mut self, in_block: bool) -> Vec<Declaration> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Token::Eof => return out,
                Token::RBrace if in_block => {
                    self.next();
                    return out;
                }
                Token::Semicolon => {
                    self.next();
                }
                Token::Ident(_) => {
                    if let Some(d) = self.declaration(in_block) {
                        out.push(d);
                    }
                }
                // Junk where a property name belongs: drop through to the next
                // `;` and try again.
                _ => {
                    self.next();
                    self.skip_to_end_of_declaration(in_block);
                }
            }
        }
    }

    /// One `name: value` pair. The name token is known to be an `Ident`.
    fn declaration(&mut self, in_block: bool) -> Option<Declaration> {
        let Token::Ident(name) = self.next().tok else {
            unreachable!("declaration() is only called on an Ident")
        };
        self.skip_ws();
        if !matches!(self.peek(), Token::Colon) {
            self.skip_to_end_of_declaration(in_block);
            return None;
        }
        self.next();

        // The value is the source between the first and last value token, so
        // whatever the author wrote survives verbatim for M4.2 to interpret.
        let mut start = None;
        let mut end = 0;
        let mut important = false;
        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Semicolon => {
                    self.next();
                    break;
                }
                // The block's `}` belongs to the caller's loop.
                Token::RBrace if in_block => break,
                Token::Whitespace => {
                    self.next();
                }
                Token::Delim('!') => {
                    let bang = self.next();
                    self.skip_ws();
                    if matches!(self.peek(), Token::Ident(w) if w.eq_ignore_ascii_case("important"))
                    {
                        self.next();
                        important = true;
                        // `end` already points at the last real value token, so
                        // the flag simply stops the span growing.
                    } else if !important {
                        start.get_or_insert(bang.start);
                        end = bang.end;
                    }
                }
                _ => {
                    let lex = self.next();
                    // Anything after `!important` is junk that CSS would call
                    // invalid; it must not extend the value.
                    if !important {
                        start.get_or_insert(lex.start);
                        end = lex.end;
                    }
                }
            }
        }

        let value = start.map(|s| self.src[s..end].trim()).unwrap_or("");
        if value.is_empty() {
            return None;
        }
        Some(Declaration {
            name: name.to_ascii_lowercase(),
            value: value.to_string(),
            important,
        })
    }

    fn skip_to_end_of_declaration(&mut self, in_block: bool) {
        loop {
            match self.peek() {
                Token::Eof => return,
                Token::RBrace if in_block => return,
                Token::Semicolon => {
                    self.next();
                    return;
                }
                _ => {
                    self.next();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> Rule {
        let sheet = parse(src);
        assert_eq!(sheet.rules.len(), 1, "expected exactly one rule: {src}");
        sheet.rules[0].clone()
    }

    fn compound(tag: Option<&str>, id: Option<&str>, classes: &[&str]) -> Compound {
        Compound {
            tag: tag.map(str::to_string),
            id: id.map(str::to_string),
            classes: classes.iter().map(|c| c.to_string()).collect(),
            pseudo: Vec::new(),
        }
    }

    #[test]
    fn descendant_and_child_combinators() {
        assert_eq!(
            one("div p { color: red }").selectors[0].parts,
            vec![
                (Combinator::Descendant, compound(Some("div"), None, &[])),
                (Combinator::Descendant, compound(Some("p"), None, &[])),
            ]
        );
        // Spelled with and without spaces — the tokenizer's whitespace token is
        // what tells these apart, and both must land on Child.
        for src in ["div > p { color: red }", "div>p { color: red }"] {
            assert_eq!(one(src).selectors[0].parts[1].0, Combinator::Child, "{src}");
        }
    }

    #[test]
    fn compound_selectors_collect_tag_id_and_classes() {
        let rule = one("div.foo#bar.baz { color: red }");
        assert_eq!(rule.selectors[0].parts.len(), 1);
        assert_eq!(
            rule.selectors[0].parts[0].1,
            compound(Some("div"), Some("bar"), &["foo", "baz"])
        );
    }

    #[test]
    fn type_selectors_lowercase_but_class_and_id_keep_case() {
        let rule = one("DIV.Foo#Bar { color: red }");
        assert_eq!(
            rule.selectors[0].parts[0].1,
            compound(Some("div"), Some("Bar"), &["Foo"])
        );
    }

    #[test]
    fn a_selector_list_is_one_rule() {
        let rule = one("h1, h2 { font-weight: bold }");
        assert_eq!(rule.selectors.len(), 2);
        assert_eq!(rule.declarations.len(), 1);
    }

    #[test]
    fn universal_selector_has_an_empty_compound() {
        let rule = one("* { color: red }");
        assert_eq!(rule.selectors[0].parts[0].1, Compound::default());
    }

    #[test]
    fn known_pseudo_classes_and_unsupported_ones() {
        let rule = one("a:link, a:visited, a:hover { color: red }");
        let pseudos: Vec<&PseudoClass> = rule
            .selectors
            .iter()
            .map(|s| &s.parts[0].1.pseudo[0])
            .collect();
        assert_eq!(
            pseudos,
            vec![
                &PseudoClass::Link,
                &PseudoClass::Visited,
                &PseudoClass::Hover
            ]
        );

        // Unsupported ones keep the rule alive but inert — M4.2 never matches
        // a compound holding one. Pseudo-elements and functional pseudos land
        // here too, and `:not(...)`'s argument must not leak into the selector.
        for (src, name) in [
            ("a:frobnicate", "frobnicate"),
            ("p::before", "before"),
            ("p:nth-child(2)", "nth-child"),
        ] {
            let rule = one(&format!("{src} {{ color: red }}"));
            assert_eq!(
                rule.selectors[0].parts[0].1.pseudo,
                vec![PseudoClass::Unsupported(name.into())],
                "{src}"
            );
            assert_eq!(rule.selectors[0].parts.len(), 1, "{src}");
        }
    }

    #[test]
    fn important_is_lifted_out_of_the_value() {
        let rule = one("p { color: red !important; font-weight: bold }");
        assert_eq!(
            rule.declarations,
            vec![
                Declaration {
                    name: "color".into(),
                    value: "red".into(),
                    important: true,
                },
                Declaration {
                    name: "font-weight".into(),
                    value: "bold".into(),
                    important: false,
                },
            ]
        );
    }

    #[test]
    fn values_keep_their_source_spelling() {
        let rule = one("p { margin: 0 0 .9em; font-family: system-ui, sans-serif }");
        assert_eq!(rule.declarations[0].value, "0 0 .9em");
        assert_eq!(rule.declarations[1].value, "system-ui, sans-serif");
    }

    #[test]
    fn property_names_lowercase_and_a_quoted_semicolon_is_not_a_separator() {
        let rule = one(r#"p { COLOR: red; content: "a;b" }"#);
        assert_eq!(rule.declarations[0].name, "color");
        assert_eq!(rule.declarations[1].value, r#""a;b""#);
    }

    #[test]
    fn a_bad_declaration_costs_only_itself() {
        // Empty value, missing colon, and junk where a property belongs.
        let rule = one("p { color:; font-weight: bold }");
        assert_eq!(rule.declarations.len(), 1);
        assert_eq!(rule.declarations[0].name, "font-weight");

        let rule = one("p { color red; font-style: italic }");
        assert_eq!(rule.declarations.len(), 1);

        let rule = one("p { 42; color: red }");
        assert_eq!(rule.declarations.len(), 1);
    }

    #[test]
    fn a_bad_selector_costs_only_its_rule() {
        // Attribute selectors and sibling combinators are out of scope for M4;
        // the rules that use them are dropped whole, and the next rule parses.
        for bad in ["a[href]", "h1 + p", "p ~ span", "p,"] {
            let sheet = parse(&format!("{bad} {{ color: red }} h1 {{ color: blue }}"));
            assert_eq!(sheet.rules.len(), 1, "{bad}");
            assert_eq!(
                sheet.rules[0].selectors[0].parts[0].1.tag.as_deref(),
                Some("h1")
            );
        }
    }

    #[test]
    fn at_rules_are_skipped_whole() {
        let sheet = parse("@media screen { p { color: red } } h1 { color: blue }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations[0].value, "blue");

        let sheet = parse("@import url(x.css); h1 { color: blue }");
        assert_eq!(sheet.rules.len(), 1);

        // Nested blocks inside the skipped at-rule must not end it early.
        let sheet =
            parse("@supports (a:b) { @media print { p { color: red } } } h1 { color: blue }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(
            sheet.rules[0].selectors[0].parts[0].1.tag.as_deref(),
            Some("h1")
        );
    }

    #[test]
    fn malformed_input_terminates_and_keeps_what_it_can() {
        // Each of these is a hang or a panic if recovery is wrong.
        assert_eq!(parse("").rules.len(), 0);
        assert_eq!(parse("{{{{").rules.len(), 0);
        assert_eq!(parse("}}}}").rules.len(), 0);
        assert_eq!(parse("p {").rules.len(), 1);
        assert_eq!(parse("p { color: red").rules[0].declarations.len(), 1);
        assert_eq!(parse("p").rules.len(), 0);
        assert_eq!(parse("@").rules.len(), 0);
        assert_eq!(parse("/* unterminated").rules.len(), 0);
        // A dropped rule in the middle does not desynchronize the sheet.
        let sheet = parse("h1 { color: a } p[x] { color: b } h2 { color: c }");
        assert_eq!(sheet.rules.len(), 2);
    }

    #[test]
    fn declaration_lists_parse_without_braces() {
        // This is the `style=""` attribute path.
        let decls = parse_declarations("color: red; font-weight: bold");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[1].value, "bold");
        assert_eq!(parse_declarations("").len(), 0);
        assert_eq!(parse_declarations("color").len(), 0);
    }
}

/// Ladder proof: parse the CSS the committed fixtures actually ship, reached
/// through the HTML parser rather than a regex — `<style>` content is raw text
/// to the tokenizer, and going through the DOM is what proves the two front
/// ends agree about where a stylesheet begins and ends.
///
/// These pin the sheets the M4 demo gate will be judged on: example.com's four
/// rules are the page's entire appearance.
#[cfg(test)]
mod ladder {
    use super::*;
    use crate::dom::{Dom, NodeData, NodeId};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/",
                $name
            ))
        };
    }

    /// Every `<style>` element's text, in document order.
    fn style_blocks(html: &str) -> Vec<String> {
        let dom = crate::html::parse(html);
        let mut out = Vec::new();
        collect(&dom, dom.root, &mut out);
        out
    }

    fn collect(dom: &Dom, id: NodeId, out: &mut Vec<String>) {
        if matches!(&dom.node(id).data, NodeData::Element { tag, .. } if tag == "style") {
            let mut text = String::new();
            for child in dom.children(id) {
                if let NodeData::Text(t) = &dom.node(child).data {
                    text.push_str(t);
                }
            }
            out.push(text);
            return;
        }
        for child in dom.children(id) {
            collect(dom, child, out);
        }
    }

    fn tag_of(rule: &Rule) -> Option<&str> {
        rule.selectors[0].parts[0].1.tag.as_deref()
    }

    #[test]
    fn example_com_is_four_rules_and_a_link_colour() {
        let blocks = style_blocks(fixture!("example.com.html"));
        assert_eq!(blocks.len(), 1);
        let sheet = parse(&blocks[0]);

        assert_eq!(
            sheet.rules.iter().map(tag_of).collect::<Vec<_>>(),
            vec![Some("body"), Some("h1"), Some("div"), Some("a")]
        );
        // Declarations M4 will not implement (`width`, `font-family`) are still
        // parsed and carried: dropping unknown properties is the cascade's
        // decision, not the parser's.
        assert_eq!(
            sheet.rules[0]
                .declarations
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            vec!["background", "width", "margin", "font-family"]
        );

        // `a:link,a:visited{color:#348}` — one rule, two selectors, and the
        // reason PseudoClass::Link exists at all.
        let link = sheet.rules.last().unwrap();
        assert_eq!(link.selectors.len(), 2);
        assert_eq!(link.selectors[0].parts[0].1.pseudo, vec![PseudoClass::Link]);
        assert_eq!(
            link.selectors[1].parts[0].1.pseudo,
            vec![PseudoClass::Visited]
        );
        assert_eq!(
            link.declarations,
            vec![Declaration {
                name: "color".into(),
                value: "#348".into(),
                important: false,
            }]
        );
    }

    #[test]
    fn danluu_com_keeps_its_homemade_tag_and_class_rules() {
        let blocks = style_blocks(fixture!("danluu.com.html"));
        assert_eq!(blocks.len(), 1);
        let sheet = parse(&blocks[0]);
        assert_eq!(
            sheet.rules.iter().map(tag_of).collect::<Vec<_>>(),
            vec![Some("d"), Some("li"), Some("ul"), None]
        );

        // `d{width:4em}` — danluu's invented <d> tag, which the layout tests
        // already care about.
        assert_eq!(sheet.rules[0].declarations[0].value, "4em");
        // `.np{...}` — a class-only selector with five declarations, one of
        // which (`display:flex`) M4 parses and M9 finally honours.
        let np = sheet.rules.last().unwrap();
        assert_eq!(np.selectors[0].parts[0].1.classes, vec!["np".to_string()]);
        assert_eq!(np.declarations.len(), 5);
        assert_eq!(np.declarations[0].name, "display");
        assert_eq!(np.declarations[0].value, "flex");
    }

    /// The 1.5 MB article: 21 inline sheets of real-world MediaWiki CSS, full
    /// of syntax M4 does not implement. The per-block rule counts are pinned so
    /// a future recovery change shows up as a diff rather than as rules quietly
    /// appearing or vanishing.
    #[test]
    fn wikipedia_parses_every_style_block() {
        let blocks = style_blocks(fixture!("en.wikipedia.org.html"));
        let counts: Vec<usize> = blocks.iter().map(|b| parse(b).rules.len()).collect();
        assert_eq!(
            counts,
            vec![
                3, 8, 3, 16, 24, 4, 5, 8, 2, 2, 5, 5, 9, 15, 7, 0, 13, 17, 1, 12, 1
            ]
        );
        // Block 15 is two `@media` blocks and nothing else: zero rules is the
        // spec'd outcome, not a parse failure. Flattening it would invert a
        // dark-mode image filter on every terminal.
        assert!(blocks[15].starts_with("@media screen{"));
    }
}
