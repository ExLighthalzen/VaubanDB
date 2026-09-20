//! The lexer: turns the text of a batch into positioned tokens.
//!
//! [`tokenize`] is the only entry point. It never panics and never decides anything about
//! the grammar: a misplaced `FROM` is still a `Keyword(From)` token here, and errors 102
//! and 156 belong to the parser. The single error a lexical pass can raise is the 105,
//! "unclosed quotation mark", built by the catalogue of `vauban-errors`.
//!
//! Rules that are not obvious, as [MS-TSQL] and the T-SQL reference state them:
//!
//! - `/* */` **nests** in T-SQL, unlike most engines, so the scanner counts levels.
//! - A `--` inside a string is not a comment, and a `'` inside a comment opens no string:
//!   one automaton, one pass.
//! - After an identifier, a variable or a `)`, a `.` is always a name separator, never the
//!   start of a number: `t.5` is `t`, `.`, `5`, while `SELECT .5` is a decimal.
//! - Only `$[+|-]<number>` is a money literal; `-$1.50` is a minus sign followed by
//!   `$1.50`, and `a$b` is a plain identifier.
//! - `!=` and `<>` stay two distinct tokens, because errors 102 and 156 print the text the
//!   user wrote; the AST maps both to the same operator.

use vauban_errors::{SqlError, SqlResult};

use crate::keyword::Keyword;
use crate::parser::ParseOptions;
use crate::span::{LineIndex, Span};
use crate::token::{Op, Punct, Token, TokenKind};

/// Splits the text of one batch into tokens, the last of which is always
/// [`TokenKind::Eof`].
///
/// Whitespace and comments produce no token. Every token carries the exact slice of
/// `text` it comes from and the position of its first character.
///
/// # Errors
///
/// Error 105 when a string literal, a `"` delimited name or a `[` delimited name is left
/// open at the end of the text, and error 113 when a `/*` comment is. Any character that
/// starts no T-SQL token becomes a [`TokenKind::Unknown`] token that the parser reports.
pub(crate) fn tokenize(text: &str, opts: &ParseOptions) -> SqlResult<Vec<Token>> {
    Lexer {
        text,
        lines: LineIndex::new(text),
        quoted_identifier: opts.quoted_identifier,
        position: 0,
        tokens: Vec::new(),
    }
    .run()
}

/// The scanning state: a byte cursor over the batch text, always on a character boundary.
struct Lexer<'a> {
    text: &'a str,
    lines: LineIndex<'a>,
    quoted_identifier: bool,
    position: usize,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    /// Scans the whole text, one token per turn.
    fn run(mut self) -> SqlResult<Vec<Token>> {
        loop {
            self.skip_trivia()?;
            let start = self.position;
            let Some(first) = self.peek() else {
                self.push(TokenKind::Eof, start);
                return Ok(self.tokens);
            };
            let kind = self.scan(first)?;
            self.push(kind, start);
        }
    }

    /// Dispatches on the first character of a token. On return the cursor sits just after
    /// the token.
    fn scan(&mut self, first: char) -> SqlResult<TokenKind> {
        match first {
            '\'' => self.string_literal(false),
            // `N'x'` is a Unicode string, but a lone `N` is an identifier.
            'N' | 'n' if self.peek_second() == Some('\'') => {
                self.position += 1;
                self.string_literal(true)
            }
            '"' if self.quoted_identifier => self.double_quoted_name(),
            '"' => self.double_quoted_string(),
            '[' => self.bracketed_name(),
            '@' => Ok(self.variable()),
            '$' => Ok(self.money_or_unknown()),
            '.' if self.dot_starts_a_number() => Ok(self.number()),
            _ if first.is_ascii_digit() => Ok(self.number()),
            // `#t` and `##t` are names too, and `#` is a name character everywhere else.
            _ if is_name_start(first) => Ok(self.word()),
            _ => Ok(self.operator(first)),
        }
    }

    // -- trivia ----------------------------------------------------------------------

    /// Skips whitespace, `--` comments and nested `/* */` comments, in any order.
    fn skip_trivia(&mut self) -> SqlResult<()> {
        loop {
            match self.peek() {
                Some(character) if is_whitespace(character) => self.position += 1,
                Some('-') if self.peek_second() == Some('-') => self.skip_line_comment(),
                Some('/') if self.peek_second() == Some('*') => self.skip_block_comment()?,
                _ => return Ok(()),
            }
        }
    }

    /// Skips `-- …` up to and including the end of the line, or to the end of the text.
    fn skip_line_comment(&mut self) {
        while let Some(character) = self.bump() {
            if character == '\n' {
                return;
            }
        }
    }

    /// Skips `/* … */`, counting levels because T-SQL block comments nest.
    ///
    /// A comment left open at the end of the batch is error **113**, carried on the
    /// **last line of the batch** -- not on the line the `/*`
    /// sits on, and not on the last line that holds text: a batch that ends with a line
    /// ending has one more line after it, and that empty line is the one reported.
    ///
    /// Five shapes, each 113/15/1, `<n>` being the line reported: `SELECT 1 /* a` on two
    /// lines -> 2, `/* a` then `SELECT 1` -> 3, `SELECT 1` then `/* a` then `/* b */`
    /// -> 4, `SELECT 1` then `/* a */` then `/* b` then a line ending -> 5, `SELECT 1`
    /// then `/* a` then two empty lines -> 6 (`lex_comments` below and
    /// `tests/block_comment.rs`).
    fn skip_block_comment(&mut self) -> SqlResult<()> {
        self.position += 2;
        let mut depth = 1usize;
        while depth > 0 {
            let rest = self.rest();
            if rest.is_empty() {
                let (line, _) = self.lines.position(offset_of(self.text.len()));
                return Err(SqlError::missing_end_comment_mark(line));
            }
            if rest.starts_with("/*") {
                depth += 1;
                self.position += 2;
            } else if rest.starts_with("*/") {
                depth -= 1;
                self.position += 2;
            } else {
                let _ = self.bump();
            }
        }
        Ok(())
    }

    // -- strings and names -----------------------------------------------------------

    /// Scans `'…'` (or the `'…'` part of `N'…'`), where `''` stands for one `'`.
    fn string_literal(&mut self, unicode: bool) -> SqlResult<TokenKind> {
        let quote = self.position;
        self.position += 1;
        let mut value = String::new();
        loop {
            match self.bump() {
                None => return Err(self.unclosed_text(&value, quote)),
                Some('\'') if self.peek() == Some('\'') => {
                    self.position += 1;
                    value.push('\'');
                }
                Some('\'') => return Ok(TokenKind::Str { value, unicode }),
                Some(character) => value.push(character),
            }
        }
    }

    /// Scans `"…"` under `QUOTED_IDENTIFIER OFF`, where the literal is a string and `""`
    /// stands for one `"`.
    ///
    /// `SELECT "a""b"` returns one unnamed `varchar` column holding `a"b`, so it is one
    /// string with a doubled delimiter -- two strings would have named the column `b` and
    /// returned `a` (`lex_double_quotes_off_are_strings` below).
    fn double_quoted_string(&mut self) -> SqlResult<TokenKind> {
        let quote = self.position;
        self.position += 1;
        let mut value = String::new();
        loop {
            match self.bump() {
                // Same code path as the `"` name of QUOTED_IDENTIFIER ON, whose value
                // the 105 prints; the rule is stated on that ON shape.
                None => return Err(self.unclosed_text(&value, quote)),
                Some('"') if self.peek() == Some('"') => {
                    self.position += 1;
                    value.push('"');
                }
                Some('"') => {
                    return Ok(TokenKind::Str {
                        value,
                        unicode: false,
                    });
                }
                Some(character) => value.push(character),
            }
        }
    }

    /// Scans `"…"` under `QUOTED_IDENTIFIER ON`, where `""` stands for one `"`.
    ///
    /// Left open, it reports its value like a `[` name does: `SELECT "a""b` -> `'a"b'`.
    fn double_quoted_name(&mut self) -> SqlResult<TokenKind> {
        let quote = self.position;
        self.position += 1;
        let mut value = String::new();
        loop {
            match self.bump() {
                None => return Err(self.unclosed_text(&value, quote)),
                Some('"') if self.peek() == Some('"') => {
                    self.position += 1;
                    value.push('"');
                }
                Some('"') => {
                    return Ok(TokenKind::Ident {
                        value,
                        quoted: true,
                    });
                }
                Some(character) => value.push(character),
            }
        }
    }

    /// Scans `[…]`, where `]]` stands for one `]`.
    fn bracketed_name(&mut self) -> SqlResult<TokenKind> {
        let bracket = self.position;
        self.position += 1;
        let mut value = String::new();
        loop {
            match self.bump() {
                // The 105 prints the **value** scanned so far: no opening `[`, and a
                // doubled `]]` reduced to one (`SELECT [a` -> `'a'`, not `'[a'`;
                // `SELECT [a]]b` -> `'a]b'`).
                None => return Err(self.unclosed_text(&value, bracket)),
                Some(']') if self.peek() == Some(']') => {
                    self.position += 1;
                    value.push(']');
                }
                Some(']') => {
                    return Ok(TokenKind::Ident {
                        value,
                        quoted: true,
                    });
                }
                Some(character) => value.push(character),
            }
        }
    }

    /// Builds error 105 from the catalogue on the **unescaped value** the scan has built
    /// so far, blanks included.
    ///
    /// `opening` is the offset of the delimiter that was never closed, whose line the
    /// error carries. The three delimited families print their value, not their source
    /// slice: `SELECT [a]]b` answers `'a]b'`, `SELECT 1 x [a]]b]]c` answers `'a]b]c'`,
    /// `SELECT "a""b` answers `'a"b'`, and a character string does the same --
    /// `SELECT 1 x 'a''b` answers `'a'b'`, `SELECT 'a''` answers `'a''`, the argument
    /// being `a'`. Blanks are kept: `SELECT 1 x 'a  ` answers `'a  '` and `SELECT 1 x 'a`
    /// followed by a line ending answers `'a\n'`. Neither the raw slice nor a trimmed end
    /// would fit those shapes (`lex_unclosed_string_is_105` below).
    fn unclosed_text(&self, text: &str, opening: usize) -> SqlError {
        let (line, _) = self.lines.position(offset_of(opening));
        SqlError::unclosed_quotation_mark(text, line)
    }

    /// Scans `@x` or `@@x`, the `@` signs included in the token text.
    fn variable(&mut self) -> TokenKind {
        self.position += 1;
        if self.peek() == Some('@') {
            self.position += 1;
        }
        self.take_name_characters();
        TokenKind::Variable
    }

    /// Scans a regular name, which may be a keyword, a temporary table name (`#t`, `##t`)
    /// or a plain identifier.
    fn word(&mut self) -> TokenKind {
        let start = self.position;
        self.take_name_characters();
        let word = self.text.get(start..self.position).unwrap_or("");
        match Keyword::parse(word) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Ident {
                value: word.to_owned(),
                quoted: false,
            },
        }
    }

    /// Advances over every character a name may contain.
    fn take_name_characters(&mut self) {
        while let Some(character) = self.peek() {
            if is_name_character(character) {
                self.position += character.len_utf8();
            } else {
                return;
            }
        }
    }

    // -- numbers ---------------------------------------------------------------------

    /// Whether the `.` under the cursor opens a decimal rather than separating names.
    fn dot_starts_a_number(&self) -> bool {
        let digit_follows = self.peek_second().is_some_and(|c| c.is_ascii_digit());
        let after_a_name = matches!(
            self.tokens.last().map(|token| &token.kind),
            Some(
                TokenKind::Ident { .. } | TokenKind::Variable | TokenKind::Punct(Punct::RightParen)
            )
        );
        digit_follows && !after_a_name
    }

    /// Scans `0x…`, an integer, a decimal or a float. The cursor is on a digit or on the
    /// `.` of a `.5`.
    fn number(&mut self) -> TokenKind {
        let rest = self.rest();
        if rest.starts_with("0x") || rest.starts_with("0X") {
            self.position += 2;
            let digits = self.position;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.position += 1;
            }
            let digits = self.text.get(digits..self.position).unwrap_or("");
            // 0xABC and 0xabc yield 0x0ABC, 0x0 yields 0x00, and 0x remains empty
            // (`tests/hex_literals.rs`).
            // Pad the payload; the token text and span retain the source spelling.
            let payload = if digits.len().is_multiple_of(2) {
                digits.to_owned()
            } else {
                let mut padded = String::with_capacity(digits.len() + 1);
                padded.push('0');
                padded.push_str(digits);
                padded
            };
            return TokenKind::Binary(payload);
        }
        let mut kind = TokenKind::Integer;
        self.take_digits();
        if self.peek() == Some('.') {
            kind = TokenKind::Decimal;
            self.position += 1;
            self.take_digits();
        }
        if self.take_exponent() {
            kind = TokenKind::Float;
        }
        kind
    }

    /// Scans `$[+|-]<number>`, the only shape of a money literal. Any other `$` is an
    /// unknown character, because `$` may not start an identifier.
    fn money_or_unknown(&mut self) -> TokenKind {
        let mut characters = self.rest().chars();
        let _dollar = characters.next();
        let mut sign = 0;
        let mut next = characters.next();
        if matches!(next, Some('+' | '-')) {
            sign = 1;
            next = characters.next();
        }
        let starts_a_number = next.is_some_and(|c| c.is_ascii_digit())
            || (next == Some('.') && characters.next().is_some_and(|c| c.is_ascii_digit()));
        if !starts_a_number {
            self.position += 1;
            return TokenKind::Unknown;
        }
        self.position += 1 + sign;
        self.take_digits();
        if self.peek() == Some('.') {
            self.position += 1;
            self.take_digits();
        }
        TokenKind::Money
    }

    /// Advances over a run of decimal digits, possibly empty.
    fn take_digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.position += 1;
        }
    }

    /// Advances over `e12`, `E+12` or `e-12` and answers whether there was one. A lone
    /// `e` is left alone: `1e` is the integer `1` followed by the identifier `e`.
    fn take_exponent(&mut self) -> bool {
        if !matches!(self.peek(), Some('e' | 'E')) {
            return false;
        }
        let mut characters = self.rest().chars();
        let _marker = characters.next();
        let mut width = 1;
        let mut next = characters.next();
        if matches!(next, Some('+' | '-')) {
            width = 2;
            next = characters.next();
        }
        if !next.is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        self.position += width;
        self.take_digits();
        true
    }

    // -- operators -------------------------------------------------------------------

    /// Scans punctuation and operators, longest match first. Anything else is one
    /// [`TokenKind::Unknown`] character.
    fn operator(&mut self, first: char) -> TokenKind {
        self.position += first.len_utf8();
        let operator = match first {
            '(' => return TokenKind::Punct(Punct::LeftParen),
            ')' => return TokenKind::Punct(Punct::RightParen),
            ',' => return TokenKind::Punct(Punct::Comma),
            ';' => return TokenKind::Punct(Punct::Semicolon),
            '.' => return TokenKind::Punct(Punct::Dot),
            '?' => return TokenKind::Punct(Punct::Question),
            ':' if self.eat(':') => return TokenKind::Punct(Punct::DoubleColon),
            ':' => return TokenKind::Punct(Punct::Colon),
            '~' => Op::Tilde,
            '=' => Op::Eq,
            '+' if self.eat('=') => Op::PlusEq,
            '+' => Op::Plus,
            '-' if self.eat('=') => Op::MinusEq,
            '-' => Op::Minus,
            '*' if self.eat('=') => Op::StarEq,
            '*' => Op::Star,
            '/' if self.eat('=') => Op::SlashEq,
            '/' => Op::Slash,
            '%' if self.eat('=') => Op::PercentEq,
            '%' => Op::Percent,
            '&' if self.eat('=') => Op::AmpersandEq,
            '&' => Op::Ampersand,
            '|' if self.eat('=') => Op::PipeEq,
            '|' => Op::Pipe,
            '^' if self.eat('=') => Op::CaretEq,
            '^' => Op::Caret,
            '<' if self.eat('>') => Op::Ne,
            '<' if self.eat('=') => Op::Le,
            '<' => Op::Lt,
            '>' if self.eat('=') => Op::Ge,
            '>' => Op::Gt,
            '!' if self.eat('=') => Op::BangEq,
            '!' if self.eat('<') => Op::NotLt,
            '!' if self.eat('>') => Op::NotGt,
            _ => return TokenKind::Unknown,
        };
        TokenKind::Op(operator)
    }

    // -- cursor ----------------------------------------------------------------------

    /// The text that is left to scan.
    fn rest(&self) -> &'a str {
        self.text.get(self.position..).unwrap_or("")
    }

    /// The character under the cursor.
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// The character just after the one under the cursor.
    fn peek_second(&self) -> Option<char> {
        self.rest().chars().nth(1)
    }

    /// Consumes the character under the cursor.
    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        Some(character)
    }

    /// Consumes the character under the cursor when it is `expected`.
    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.position += expected.len_utf8();
            true
        } else {
            false
        }
    }

    /// Appends the token that spans from `start` to the cursor.
    fn push(&mut self, kind: TokenKind, start: usize) {
        let text = self.text.get(start..self.position).unwrap_or("");
        let (line, column) = self.lines.position(offset_of(start));
        self.tokens.push(Token {
            kind,
            text: text.to_owned(),
            span: Span {
                line,
                column,
                offset: offset_of(start),
                len: offset_of(text.len()),
            },
        });
    }
}

/// Narrows a byte offset to the `u32` a [`Span`] holds, saturating rather than wrapping
/// on a batch larger than 4 GiB, which no client sends.
fn offset_of(position: usize) -> u32 {
    u32::try_from(position).unwrap_or(u32::MAX)
}

/// The characters T-SQL treats as whitespace. Deliberately ASCII only: a no-break space
/// is not whitespace for SQL Server, and must become an [`TokenKind::Unknown`] token.
fn is_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}')
}

/// Whether a regular name may start with this character, as the T-SQL rules on
/// identifiers state. `@` and `#` are handled before this is called.
fn is_name_start(character: char) -> bool {
    character.is_alphabetic() || character == '_' || character == '#'
}

/// Whether a regular name may contain this character. `$` is allowed inside a name but
/// not at its start, which is what makes `a$b` an identifier and `$1` a money literal.
fn is_name_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '@' | '#' | '$')
}

#[cfg(test)]
mod tests;
