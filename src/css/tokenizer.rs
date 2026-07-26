//! CSS tokenizer: stylesheet text → tokens (PLAN.md §2 `css/`, M4.1).
//!
//! Same shape as `html/tokenizer.rs`, and the same discipline: it knows syntax
//! and nothing else. `#348` and `#id` are both a `Hash` — which one it *is*
//! depends on where the parser is standing, and the parser is the only thing
//! that knows. No property names, no colours, no units interpreted here.
//!
//! Whitespace is a token rather than something quietly skipped, because in CSS
//! it is a combinator: `div p` and `div>p` differ by exactly one whitespace
//! token. Comments are the opposite — consumed and never emitted, from any
//! position, including between a property and its colon.

/// One CSS token. `Number`/`Dimension`/`Percentage` carry `f64`, so this is
/// `PartialEq` but not `Eq`; nothing needs to hash or sort tokens.
#[derive(Clone, PartialEq, Debug)]
pub enum Token {
    Ident(String),
    /// `@media`, `@import` — the name without the `@`.
    AtKeyword(String),
    /// `#` + name: an id selector or a hex colour, told apart by context.
    Hash(String),
    /// A quoted string, quotes stripped and escapes resolved.
    Str(String),
    Number(f64),
    Dimension {
        value: f64,
        unit: String,
    },
    Percentage(f64),
    /// Any character with no token of its own: `>`, `.`, `*`, `!`, `%`, ...
    Delim(char),
    Colon,
    Semicolon,
    Comma,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    /// A whole run of spaces/tabs/newlines, collapsed into one token.
    Whitespace,
    Eof,
}

pub struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    pub fn new(src: &'a str) -> Tokenizer<'a> {
        Tokenizer { src, pos: 0 }
    }

    /// Byte offset of the next unconsumed character. The parser slices raw
    /// declaration values straight out of the source with this, which is why
    /// values survive tokenization byte-for-byte (`rgb(1, 2, 3)` keeps its
    /// spacing) without the tokenizer having to serialize anything back.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The next token, or `Eof` forever once the source runs out. Every path
    /// through this function either consumes at least one character or returns
    /// `Eof`, which is what stops a malformed sheet from hanging the parser.
    pub fn next_token(&mut self) -> Token {
        loop {
            let Some(c) = self.peek() else {
                return Token::Eof;
            };
            // Comments can appear anywhere a token can, so they are skipped
            // here rather than handled at each call site.
            if c == '/' && self.peek_nth(1) == Some('*') {
                self.skip_comment();
                continue;
            }
            if is_ws(c) {
                while self.peek().is_some_and(is_ws) {
                    self.bump();
                }
                return Token::Whitespace;
            }
            if self.starts_number() {
                return self.numeric();
            }
            if self.starts_ident() {
                return Token::Ident(self.name());
            }
            self.bump();
            return match c {
                '"' | '\'' => Token::Str(self.string(c)),
                '#' => {
                    if self.peek().is_some_and(is_name) {
                        Token::Hash(self.name())
                    } else {
                        Token::Delim('#')
                    }
                }
                '@' => {
                    if self.starts_ident() {
                        Token::AtKeyword(self.name())
                    } else {
                        Token::Delim('@')
                    }
                }
                ':' => Token::Colon,
                ';' => Token::Semicolon,
                ',' => Token::Comma,
                '{' => Token::LBrace,
                '}' => Token::RBrace,
                '(' => Token::LParen,
                ')' => Token::RParen,
                '[' => Token::LBracket,
                ']' => Token::RBracket,
                c => Token::Delim(c),
            };
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Unterminated comments run to EOF — a page that opens `/*` and never
    /// closes it loses its tail, which beats losing the parser.
    fn skip_comment(&mut self) {
        self.bump();
        self.bump();
        while let Some(c) = self.bump() {
            if c == '*' && self.peek() == Some('/') {
                self.bump();
                return;
            }
        }
    }

    /// Does an identifier start here? `-webkit-box` does, `-5px` does not (the
    /// number check runs first), a lone `-` does not.
    fn starts_ident(&self) -> bool {
        match self.peek() {
            Some('\\') => true,
            Some('-') => {
                matches!(self.peek_nth(1), Some(c) if is_name_start(c) || c == '-' || c == '\\')
            }
            Some(c) => is_name_start(c),
            None => false,
        }
    }

    fn starts_number(&self) -> bool {
        match (self.peek(), self.peek_nth(1), self.peek_nth(2)) {
            (Some('+' | '-'), Some('.'), Some(c)) => c.is_ascii_digit(),
            (Some('+' | '-'), Some(c), _) => c.is_ascii_digit(),
            (Some('.'), Some(c), _) => c.is_ascii_digit(),
            (Some(c), _, _) => c.is_ascii_digit(),
            _ => false,
        }
    }

    /// A name (identifier or hash body). A backslash escapes the character
    /// after it verbatim — enough for the `\.` and `\@` real sheets use in
    /// class names; hex escapes (`\26 `) are not decoded, and no ladder page
    /// needs them.
    fn name(&mut self) -> String {
        let mut out = String::new();
        loop {
            match self.peek() {
                Some('\\') => {
                    self.bump();
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                Some(c) if is_name(c) => {
                    self.bump();
                    out.push(c);
                }
                _ => return out,
            }
        }
    }

    /// The opening quote is already consumed. A raw newline ends the string
    /// without consuming it (CSS calls this a bad-string): the newline comes
    /// back as whitespace and the parser recovers at the next `;`, instead of
    /// one stray quote swallowing the rest of the sheet.
    fn string(&mut self, quote: char) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                c if c == quote => {
                    self.bump();
                    return out;
                }
                '\n' | '\r' | '\x0C' => return out,
                '\\' => {
                    self.bump();
                    // A backslash before a newline is a line continuation: it
                    // contributes nothing to the value.
                    match self.bump() {
                        Some('\n') | None => {}
                        Some(c) => out.push(c),
                    }
                }
                c => {
                    self.bump();
                    out.push(c);
                }
            }
        }
        out
    }

    fn numeric(&mut self) -> Token {
        let start = self.pos;
        if matches!(self.peek(), Some('+' | '-')) {
            self.bump();
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') && self.peek_nth(1).is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        // Every byte in the slice came from the digit/sign/dot grammar above,
        // so the parse only fails on absurd exponents; 0.0 is a fine answer for
        // a value nothing in M4 reads numerically.
        let value = self.src[start..self.pos].parse::<f64>().unwrap_or(0.0);
        if self.peek() == Some('%') {
            self.bump();
            Token::Percentage(value)
        } else if self.starts_ident() {
            Token::Dimension {
                value,
                unit: self.name(),
            }
        } else {
            Token::Number(value)
        }
    }
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-'
}

/// Every token in `src`, `Eof` excluded. The parser drives `Tokenizer` directly
/// (it needs byte offsets); this is for tests and for anyone wanting to see the
/// stream, mirroring `html::tokenize`.
pub fn tokenize(src: &str) -> Vec<Token> {
    let mut t = Tokenizer::new(src);
    let mut out = Vec::new();
    loop {
        match t.next_token() {
            Token::Eof => return out,
            tok => out.push(tok),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Token::*;
    use super::*;

    #[test]
    fn whitespace_is_a_token_and_collapses() {
        assert_eq!(
            tokenize("div  \n p"),
            vec![Ident("div".into()), Whitespace, Ident("p".into()),]
        );
    }

    #[test]
    fn comments_vanish_from_any_position() {
        // Mid-declaration is the awkward one: the comment sits between the
        // property and its colon, and between the colon and the value.
        assert_eq!(
            tokenize("color/*x*/:/*y*/red"),
            vec![Ident("color".into()), Colon, Ident("red".into())]
        );
        assert_eq!(tokenize("/* only a comment */"), vec![]);
    }

    #[test]
    fn hash_covers_ids_and_hex_colours_alike() {
        assert_eq!(tokenize("#348"), vec![Hash("348".into())]);
        assert_eq!(tokenize("#main"), vec![Hash("main".into())]);
        // Nothing name-shaped after it: just a delimiter.
        assert_eq!(tokenize("# "), vec![Delim('#'), Whitespace]);
    }

    #[test]
    fn idents_may_lead_with_a_dash() {
        assert_eq!(tokenize("-webkit-box"), vec![Ident("-webkit-box".into())]);
        // A lone dash is not an identifier, and a dash before a digit is a
        // number — otherwise `-5px` would become an ident.
        assert_eq!(tokenize("-"), vec![Delim('-')]);
        assert_eq!(
            tokenize("-5px"),
            vec![Dimension {
                value: -5.0,
                unit: "px".into()
            }]
        );
    }

    #[test]
    fn numbers_dimensions_and_percentages() {
        assert_eq!(tokenize("0"), vec![Number(0.0)]);
        assert_eq!(
            tokenize(".9em"),
            vec![Dimension {
                value: 0.9,
                unit: "em".into()
            }]
        );
        assert_eq!(tokenize("60%"), vec![Percentage(60.0)]);
        // A dot that isn't a number stays a delimiter — this is what keeps
        // `.np` a class selector and `.9em` a length.
        assert_eq!(tokenize(".np"), vec![Delim('.'), Ident("np".into())]);
    }

    #[test]
    fn strings_keep_escapes_and_semicolons() {
        assert_eq!(tokenize(r#""a;b""#), vec![Str("a;b".into())]);
        assert_eq!(tokenize(r#""a\"b""#), vec![Str("a\"b".into())]);
        assert_eq!(tokenize("'single'"), vec![Str("single".into())]);
    }

    #[test]
    fn unterminated_input_stops_instead_of_hanging() {
        // Each of these used to be a plausible infinite loop or panic.
        assert_eq!(tokenize(r#""no end"#), vec![Str("no end".into())]);
        assert_eq!(tokenize("a /* no end"), vec![Ident("a".into()), Whitespace]);
        assert_eq!(tokenize("@"), vec![Delim('@')]);
        assert_eq!(tokenize("\\"), vec![Ident("".into())]);
        // A raw newline ends a bad string without eating the rest of the sheet.
        assert_eq!(
            tokenize("'oops\np{}"),
            vec![
                Str("oops".into()),
                Whitespace,
                Ident("p".into()),
                LBrace,
                RBrace
            ]
        );
    }

    #[test]
    fn punctuation_and_at_keywords() {
        assert_eq!(
            tokenize("@media{a>b}"),
            vec![
                AtKeyword("media".into()),
                LBrace,
                Ident("a".into()),
                Delim('>'),
                Ident("b".into()),
                RBrace
            ]
        );
    }

    #[test]
    fn pos_tracks_the_source_for_raw_value_slicing() {
        let mut t = Tokenizer::new("color:red");
        assert_eq!(t.next_token(), Ident("color".into()));
        assert_eq!(t.pos(), 5);
        assert_eq!(t.next_token(), Colon);
        assert_eq!(t.next_token(), Ident("red".into()));
        assert_eq!(t.pos(), 9);
    }
}
