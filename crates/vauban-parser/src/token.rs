//! The tokens the lexer produces, as `lexer.rs` builds them; the cursor of the parser
//! consumes them.
//!
//! [`Token::text`] is **always** the exact slice of batch text the token comes from,
//! delimiters and escapes included: it is what errors 102 and 156 print, and what
//! `Display` reuses. Payloads only carry what that slice cannot give for free: the
//! unescaped value of an identifier or of a string, and the digits of a binary literal
//! without their `0x`. The numeric kinds carry nothing, because their source text *is*
//! their value: turning `Token.text` into a [`crate::Literal`] (dropping the `$` of a
//! money literal, for instance) belongs to the parser.

use crate::keyword::Keyword;
use crate::span::Span;

/// A lexical token and the slice of batch text it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    /// What the token is.
    pub kind: TokenKind,
    /// The exact slice of batch text, as written.
    pub text: String,
    /// Where it starts and how long it is.
    pub span: Span,
}

/// The kind of a token, with the payload the parser needs.
///
/// Comments and whitespace are not tokens: the lexer drops them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// A word that matched a keyword of [`Keyword`]. Whether it is reserved (and hence
    /// unusable as an identifier here) is decided by `Keyword::is_reserved`.
    Keyword(Keyword),
    /// A regular identifier (`t`), a temporary table name (`#t`, `##t`), or a delimited
    /// identifier (`[t]`, `"t"`). `value` has no delimiters left and its `]]` and `""`
    /// are already unescaped; `quoted` says whether the user wrote delimiters.
    Ident {
        /// Identifier text, without delimiters.
        value: String,
        /// True when the user wrote `[ ]` or `" "`.
        quoted: bool,
    },
    /// A local variable `@x` or a global one `@@x`. The `@` signs are part of
    /// [`Token::text`], which is the whole name.
    Variable,
    /// An integer literal. Its digits are in [`Token::text`].
    Integer,
    /// A fixed-point literal (`1.5`, `.5`, `1.`), as written in [`Token::text`].
    Decimal,
    /// A floating-point literal with an exponent (`1e3`), as written in [`Token::text`].
    Float,
    /// A money literal. [`Token::text`] keeps the `$` and the sign that follows it
    /// (`$-1.50`); the parser drops the `$` when it builds the literal.
    Money,
    /// A binary literal; the payload holds its hexadecimal digits **without** the `0x`
    /// prefix, which [`Token::text`] still has.
    Binary(String),
    /// A character string literal; the payload is already unescaped (`''` -> `'`) and has
    /// no delimiters, while [`Token::text`] keeps them and the `N` prefix.
    Str {
        /// The string value.
        value: String,
        /// True when the literal had an `N` prefix.
        unicode: bool,
    },
    /// A punctuation sign.
    Punct(Punct),
    /// An operator.
    Op(Op),
    /// A character that starts no token of T-SQL (`\`, `!` alone, `{`, an emoji, ...).
    /// The lexer never fails on it: it hands it to the parser, which reports the error
    /// 102 that SQL Server reports on it too.
    Unknown,
    /// The end of the batch text.
    Eof,
}

/// Punctuation signs that structure a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Punct {
    /// `(`
    LeftParen,
    /// `)`
    RightParen,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `.`
    Dot,
    /// `:`, used by a `GOTO` label.
    Colon,
    /// `::`, the scope resolution sign of `sys::fn_x` and `::fn_x`.
    DoubleColon,
    /// `?`, the ODBC parameter placeholder.
    Question,
}

/// Operators, arithmetic, bitwise, comparison and compound assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`, also the `SELECT *` star.
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `&`
    Ampersand,
    /// `|`
    Pipe,
    /// `^`
    Caret,
    /// `~`
    Tilde,
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `!=`, the other spelling of `<>`.
    BangEq,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `!<`
    NotLt,
    /// `!>`
    NotGt,
    /// `+=`
    PlusEq,
    /// `-=`
    MinusEq,
    /// `*=`
    StarEq,
    /// `/=`
    SlashEq,
    /// `%=`
    PercentEq,
    /// `&=`
    AmpersandEq,
    /// `|=`
    PipeEq,
    /// `^=`
    CaretEq,
}
