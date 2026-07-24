//! HTML tokenizer: a hand-rolled state machine over the input's chars. It does
//! not track open elements or nesting — that is the tree builder's job (M2.2).
//! The one lookback it keeps is the current raw-text tag name, because
//! `<script>`/`<style>`/`<title>`/`<textarea>` swallow markup until their own
//! end tag and nothing shorter can decide where that run stops.
//!
//! Malformed input never panics: an unterminated tag, comment, or attribute at
//! EOF emits whatever was accumulated and stops. Garbage in, best-effort tokens.

/// One HTML token. Positions and source ranges are intentionally absent — the
/// tree builder consumes shapes, not spans.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Token {
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Text(String),
    Comment(String),
    Doctype(String),
}

/// Tags whose content is raw text: only the matching end tag ends the run, and
/// nothing inside is a tag or an entity.
const RAW_TEXT_TAGS: [&str; 4] = ["script", "style", "title", "textarea"];

/// Tokenize an HTML string. Chosen over an iterator type for simplicity — the
/// whole stream is small relative to a page's layout cost, and the tree builder
/// wants to make several passes anyway.
pub fn tokenize(input: &str) -> Vec<Token> {
    Tokenizer::new(input).run()
}

struct Tokenizer {
    chars: Vec<char>,
    pos: usize,
    tokens: Vec<Token>,
}

impl Tokenizer {
    fn new(input: &str) -> Tokenizer {
        Tokenizer {
            chars: input.chars().collect(),
            pos: 0,
            tokens: Vec::new(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Data state: accumulate text until a real markup start, flushing a decoded
    /// `Text` token before each tag/comment/doctype. A `<` that isn't followed by
    /// something tag-like is a literal text character (bare `<`s exist in the
    /// wild), so it stays in the buffer.
    fn run(mut self) -> Vec<Token> {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c == '<' && self.starts_markup() {
                if !text.is_empty() {
                    self.tokens.push(Token::Text(decode_entities(&text)));
                    text.clear();
                }
                self.consume_markup();
            } else {
                text.push(c);
                self.pos += 1;
            }
        }
        if !text.is_empty() {
            self.tokens.push(Token::Text(decode_entities(&text)));
        }
        self.tokens
    }

    /// Does the `<` at the cursor begin a tag, comment, or doctype? `</`, `<!`,
    /// and `<` + ASCII letter do; anything else is literal text.
    fn starts_markup(&self) -> bool {
        match self.peek_at(1) {
            Some('/') | Some('!') => true,
            Some(c) => c.is_ascii_alphabetic(),
            None => false,
        }
    }

    fn consume_markup(&mut self) {
        // Cursor is on '<'.
        match self.peek_at(1) {
            Some('!') => self.consume_bang(),
            Some('/') => self.consume_end_tag(),
            _ => self.consume_start_tag(),
        }
    }

    /// `<!-- ... -->` comment, or `<!doctype ...>` / other `<!...>` (kept as a
    /// comment). Everything here reads to a terminator and never panics at EOF.
    fn consume_bang(&mut self) {
        if self.peek_at(2) == Some('-') && self.peek_at(3) == Some('-') {
            self.pos += 4; // past "<!--"
            let mut content = String::new();
            loop {
                if self.pos >= self.chars.len() {
                    break; // unterminated comment at EOF
                }
                if self.peek() == Some('-')
                    && self.peek_at(1) == Some('-')
                    && self.peek_at(2) == Some('>')
                {
                    self.pos += 3;
                    break;
                }
                content.push(self.chars[self.pos]);
                self.pos += 1;
            }
            self.tokens.push(Token::Comment(content));
            return;
        }

        // "<!...>" that isn't a comment: read the inner text to '>'.
        self.pos += 2; // past "<!"
        let mut inner = String::new();
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == '>' {
                break;
            }
            inner.push(c);
        }
        let trimmed = inner.trim_start();
        if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("doctype") {
            self.tokens
                .push(Token::Doctype(trimmed[7..].trim().to_string()));
        } else {
            self.tokens.push(Token::Comment(inner));
        }
    }

    fn consume_end_tag(&mut self) {
        self.pos += 2; // past "</"
        let name = self.read_tag_name();
        // Skip any junk (attributes on an end tag are ignored) up to '>'.
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == '>' {
                break;
            }
        }
        self.tokens.push(Token::EndTag { name });
    }

    fn consume_start_tag(&mut self) {
        self.pos += 1; // past '<'
        let name = self.read_tag_name();
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut self_closing = false;

        loop {
            self.skip_whitespace();
            match self.peek() {
                None => break, // EOF mid-tag: emit what we have
                Some('>') => {
                    self.pos += 1;
                    break;
                }
                Some('/') => {
                    // '/>' self-closes; a stray '/' is just skipped.
                    if self.peek_at(1) == Some('>') {
                        self_closing = true;
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
                Some(_) => {
                    let (key, value) = self.read_attribute();
                    if key.is_empty() {
                        continue;
                    }
                    // Duplicate attribute: first occurrence wins (spec behavior).
                    if !attrs.iter().any(|(k, _)| *k == key) {
                        attrs.push((key, value));
                    }
                }
            }
        }

        let raw = !self_closing && RAW_TEXT_TAGS.contains(&name.as_str());
        self.tokens.push(Token::StartTag {
            name: name.clone(),
            attrs,
            self_closing,
        });
        if raw {
            self.consume_raw_text(&name);
        }
    }

    /// Read a lowercased tag name: runs until whitespace, `/`, `>`, or EOF.
    fn read_tag_name(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '/' || c == '>' {
                break;
            }
            name.push(c.to_ascii_lowercase());
            self.pos += 1;
        }
        name
    }

    /// Read one attribute. Name lowercased, terminated by whitespace/`=`/`/`/`>`.
    /// A following `=` introduces a value (double, single, or unquoted); no `=`
    /// means an empty value. Values are entity-decoded.
    fn read_attribute(&mut self) -> (String, String) {
        let mut key = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '=' || c == '/' || c == '>' {
                break;
            }
            key.push(c.to_ascii_lowercase());
            self.pos += 1;
        }

        self.skip_whitespace();
        if self.peek() != Some('=') {
            return (key, String::new());
        }
        self.pos += 1; // past '='
        self.skip_whitespace();

        let value = match self.peek() {
            Some('"') => self.read_quoted_value('"'),
            Some('\'') => self.read_quoted_value('\''),
            _ => self.read_unquoted_value(),
        };
        (key, decode_entities(&value))
    }

    fn read_quoted_value(&mut self, quote: char) -> String {
        self.pos += 1; // past opening quote
        let mut value = String::new();
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == quote {
                break;
            }
            value.push(c);
        }
        value
    }

    fn read_unquoted_value(&mut self) -> String {
        let mut value = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '>' {
                break;
            }
            value.push(c);
            self.pos += 1;
        }
        value
    }

    /// Raw-text run for `<script>`/`<style>`/`<title>`/`<textarea>`: swallow
    /// everything as one undecoded `Text` token until `</name` appears with a
    /// valid terminator, then leave the cursor on that `<` for the normal end-tag
    /// path. No matching end tag before EOF → all the rest is text.
    fn consume_raw_text(&mut self, name: &str) {
        let start = self.pos;
        while self.pos < self.chars.len() {
            if self.peek() == Some('<')
                && self.peek_at(1) == Some('/')
                && self.matches_end_tag(name)
            {
                if self.pos > start {
                    let text: String = self.chars[start..self.pos].iter().collect();
                    self.tokens.push(Token::Text(text));
                }
                return; // cursor on '<'; consume_end_tag handles it next
            }
            self.pos += 1;
        }
        if self.pos > start {
            let text: String = self.chars[start..self.pos].iter().collect();
            self.tokens.push(Token::Text(text));
        }
    }

    /// Is the cursor at `</name` (case-insensitive) followed by whitespace, `/`,
    /// `>`, or EOF — i.e. a real end tag rather than `</something-else`?
    fn matches_end_tag(&self, name: &str) -> bool {
        let mut i = self.pos + 2; // past "</"
        for expected in name.chars() {
            match self.chars.get(i) {
                Some(c) if c.eq_ignore_ascii_case(&expected) => i += 1,
                _ => return false,
            }
        }
        match self.chars.get(i) {
            None => true,
            Some(c) => c.is_whitespace() || *c == '/' || *c == '>',
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }
}

/// Decode HTML entities in text or an attribute value. A small named table plus
/// decimal (`&#160;`) and hex (`&#xA0;`) numeric refs; anything unrecognized or
/// unterminated is left exactly as written (real pages carry bare `&`s).
fn decode_entities(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&'
            && let Some((decoded, len)) = parse_entity(&chars, i)
        {
            out.push_str(&decoded);
            i += len;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Try to parse an entity starting at `chars[i]` (which must be `&`). Returns the
/// decoded string and the number of chars consumed (including `&` and `;`), or
/// `None` if it isn't a well-formed, terminated entity we know.
fn parse_entity(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut j = i + 1;
    let first = *chars.get(j)?;

    if first == '#' {
        j += 1;
        let hex = matches!(chars.get(j), Some('x') | Some('X'));
        if hex {
            j += 1;
        }
        let start = j;
        while let Some(&c) = chars.get(j) {
            let ok = if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            };
            if !ok {
                break;
            }
            j += 1;
        }
        if j == start || chars.get(j) != Some(&';') {
            return None;
        }
        let digits: String = chars[start..j].iter().collect();
        let cp = u32::from_str_radix(&digits, if hex { 16 } else { 10 }).ok()?;
        let ch = char::from_u32(cp)?;
        return Some((ch.to_string(), j - i + 1));
    }

    // Named entity: a small, deliberately incomplete table (not the full WHATWG
    // list). Numeric refs cover the long tail.
    let start = j;
    while matches!(chars.get(j), Some(c) if c.is_ascii_alphanumeric()) {
        j += 1;
    }
    if j == start || chars.get(j) != Some(&';') {
        return None;
    }
    let name: String = chars[start..j].iter().collect();
    let decoded = match name.as_str() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        _ => return None,
    };
    Some((decoded.to_string(), j - i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(name: &str, attrs: &[(&str, &str)]) -> Token {
        Token::StartTag {
            name: name.into(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            self_closing: false,
        }
    }

    #[test]
    fn simple_element_with_attr() {
        assert_eq!(
            tokenize(r#"<p class="a">hi</p>"#),
            vec![
                start("p", &[("class", "a")]),
                Token::Text("hi".into()),
                Token::EndTag { name: "p".into() },
            ]
        );
    }

    #[test]
    fn self_closing_and_unquoted() {
        assert_eq!(
            tokenize("<br/>"),
            vec![Token::StartTag {
                name: "br".into(),
                attrs: vec![],
                self_closing: true,
            }]
        );
        assert_eq!(
            tokenize(r#"<img src=x alt="y">"#),
            vec![start("img", &[("src", "x"), ("alt", "y")])]
        );
    }

    #[test]
    fn missing_value_is_empty_string() {
        assert_eq!(
            tokenize("<input disabled>"),
            vec![start("input", &[("disabled", "")])]
        );
    }

    #[test]
    fn duplicate_attribute_first_wins() {
        assert_eq!(
            tokenize(r#"<p id="a" id="b">"#),
            vec![start("p", &[("id", "a")])]
        );
    }

    #[test]
    fn names_are_lowercased() {
        assert_eq!(
            tokenize(r#"<DIV CLASS="X">"#),
            vec![start("div", &[("class", "X")])]
        );
    }

    #[test]
    fn entities_in_text() {
        assert_eq!(
            tokenize("a &amp; b &#60; c &#x3E; d"),
            vec![Token::Text("a & b < c > d".into())]
        );
    }

    #[test]
    fn entity_in_attribute_value() {
        assert_eq!(
            tokenize(r#"<a title="x &amp; y">"#),
            vec![start("a", &[("title", "x & y")])]
        );
    }

    #[test]
    fn nbsp_named_and_numeric() {
        assert_eq!(
            tokenize("&nbsp;&#160;&#xA0;"),
            vec![Token::Text("\u{A0}\u{A0}\u{A0}".into())]
        );
    }

    #[test]
    fn unknown_and_malformed_entities_stay_raw() {
        assert_eq!(
            tokenize("AT&T & &bogus; &#;"),
            vec![Token::Text("AT&T & &bogus; &#;".into())]
        );
    }

    #[test]
    fn script_is_raw_text() {
        assert_eq!(
            tokenize("<script>if (a<b) {}</script>"),
            vec![
                Token::StartTag {
                    name: "script".into(),
                    attrs: vec![],
                    self_closing: false,
                },
                Token::Text("if (a<b) {}".into()),
                Token::EndTag {
                    name: "script".into(),
                },
            ]
        );
    }

    #[test]
    fn style_is_raw_text_no_entity_decode() {
        assert_eq!(
            tokenize("<style>a{content:\"&amp;\"}</style>"),
            vec![
                Token::StartTag {
                    name: "style".into(),
                    attrs: vec![],
                    self_closing: false,
                },
                Token::Text("a{content:\"&amp;\"}".into()),
                Token::EndTag {
                    name: "style".into()
                },
            ]
        );
    }

    #[test]
    fn comment_and_doctype() {
        assert_eq!(tokenize("<!-- x -->"), vec![Token::Comment(" x ".into())]);
        assert_eq!(
            tokenize("<!doctype html>"),
            vec![Token::Doctype("html".into())]
        );
        assert_eq!(
            tokenize("<!DOCTYPE HTML>"),
            vec![Token::Doctype("HTML".into())]
        );
    }

    #[test]
    fn bare_less_than_is_literal_text() {
        assert_eq!(tokenize("a < b"), vec![Token::Text("a < b".into())]);
    }

    #[test]
    fn unterminated_tag_at_eof_does_not_panic() {
        assert_eq!(
            tokenize(r#"<p class="a"#),
            vec![start("p", &[("class", "a")])]
        );
    }

    #[test]
    fn unterminated_comment_at_eof() {
        assert_eq!(tokenize("<!-- oops"), vec![Token::Comment(" oops".into())]);
    }

    #[test]
    fn unterminated_raw_text_at_eof() {
        assert_eq!(
            tokenize("<title>hello"),
            vec![
                Token::StartTag {
                    name: "title".into(),
                    attrs: vec![],
                    self_closing: false,
                },
                Token::Text("hello".into()),
            ]
        );
    }
}
