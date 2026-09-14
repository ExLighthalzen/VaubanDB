//! Re-serialisation of the AST into T-SQL.
//!
//! Every node of `ast/` implements [`std::fmt::Display`] here, so that `ast/` holds no
//! logic at all and keeps a single owner. The output is **one line** per statement: this
//! is not a code formatter, it is a re-serialisation whose only purpose is to be parsed
//! again. Keywords are written in UPPERCASE, in their long canonical form, one space
//! between two elements, no space before a `,` or a `)`, one after a `,`
//! (`tests/display.rs`).
//!
//! # Contract
//!
//! For any batch `s` accepted by `parse_batch`,
//! `parse_batch(&parse_batch(s)?.to_string())?` yields a `Batch` **equal** to the first
//! one. This holds because [`Span`](crate::Span) compares equal to any other span and
//! because the AST keeps what distinguishes two spellings of the same thing
//! (`Expr::Nested`, `AliasStyle`, the source text of literals, `Ident.quoted`,
//! `explicit_direction`). The output is **not** equal to the source text.
//!
//! # Assumed deviations
//!
//! Where the AST does not record which of several equivalent spellings the user wrote,
//! `Display` writes one canonical form. Each line below is a re-serialisation deviation,
//! frozen by the `display_normalisations` test of `tests/display.rs`:
//!
//! - `BinaryOp::Ne` is written `<>`, never `!=`.
//! - `JoinKind::Left`/`Right`/`Full` are written `LEFT JOIN`, `RIGHT JOIN`, `FULL JOIN`,
//!   never with the optional `OUTER`.
//! - `Statement::BeginTransaction` is written `BEGIN TRANSACTION`, never `BEGIN TRAN`.
//! - `Statement::Commit` and `Statement::Rollback` are written `COMMIT TRANSACTION` and
//!   `ROLLBACK TRANSACTION`, never with `WORK` nor `TRAN`.
//! - `Statement::Save` is written `SAVE TRANSACTION`, never `SAVE TRAN`.
//! - `ExecuteStatement` with `implicit: false` is written `EXECUTE`, never `EXEC`; with
//!   `implicit: true` nothing at all precedes the name.
//! - `InsertStatement` is written `INSERT INTO t`: the optional `INTO` is always written.
//! - `DropIndexStatement` is written `DROP INDEX <name> ON <table>`, never the older
//!   `DROP INDEX <table>.<name>` form.
//! - `AlterTableAction::Check` with an empty `constraints` list is written
//!   `CONSTRAINT ALL`, since an empty list means `ALL`.
//! - An alias of a table reference is always written with `AS`, since the AST does not
//!   record whether the user wrote it (unlike a select item, which carries an
//!   [`AliasStyle`](crate::AliasStyle)).
//! - An [`Ident`](crate::Ident) that is not `quoted` but whose value is not a valid
//!   regular identifier, or is a reserved keyword, is written between brackets: see
//!   [`is_regular_identifier`]. The name of a function is the one exception:
//!   `LEFT`, `COALESCE` or `CURRENT_TIMESTAMP` are reserved keywords that the grammar
//!   reads as function names, so `Expr::Function` writes its name bare unless the user
//!   quoted it or its spelling is not an identifier at all.
//!
//! The canonical spelling of each statement is the one of the T-SQL reference.

mod ddl;
mod expr;
mod query;
mod stmt;

use std::fmt;

use crate::keyword::Keyword;

/// Writes `items` separated by `separator`, with nothing before the first one and
/// nothing after the last one.
pub(crate) fn separated<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    items: &[T],
    separator: &str,
) -> fmt::Result {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            f.write_str(separator)?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

/// Writes `items` separated by `", "`, the only separator a T-SQL list ever uses.
pub(crate) fn comma_separated<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    items: &[T],
) -> fmt::Result {
    separated(f, items, ", ")
}

/// Writes a parenthesised comma-separated list: `(a, b, c)`.
pub(crate) fn parenthesised_list<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    items: &[T],
) -> fmt::Result {
    f.write_str("(")?;
    comma_separated(f, items)?;
    f.write_str(")")
}

/// Writes a name between brackets, doubling the `]` it contains: `a]b` → `[a]]b]`.
pub(crate) fn write_bracketed(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("[")?;
    // `split` yields one more part than there are separators: writing `]]` between two
    // consecutive parts doubles every `]` of the value.
    for (index, part) in value.split(']').enumerate() {
        if index > 0 {
            f.write_str("]]")?;
        }
        f.write_str(part)?;
    }
    f.write_str("]")
}

/// Writes a character string literal: doubles the `'` of `value`, wraps it in `'…'` and
/// prefixes it with `N` when `unicode` is true. The exact inverse of what the lexer
/// removes.
pub(crate) fn write_string_literal(
    f: &mut fmt::Formatter<'_>,
    value: &str,
    unicode: bool,
) -> fmt::Result {
    if unicode {
        f.write_str("N")?;
    }
    f.write_str("'")?;
    for (index, part) in value.split('\'').enumerate() {
        if index > 0 {
            f.write_str("''")?;
        }
        f.write_str(part)?;
    }
    f.write_str("'")
}

/// Writes a statement block: `BEGIN a; b END`, or `BEGIN END` when it is empty.
///
/// The separator is `"; "` and there is **no** `;` before the closing keyword, which is
/// what T-SQL needs and what `IF … BEGIN … END` relies on. `open` and `close`
/// are the surrounding keywords (`BEGIN`/`END`, `BEGIN TRY`/`END TRY`, …).
pub(crate) fn write_block(
    f: &mut fmt::Formatter<'_>,
    open: &str,
    statements: &[crate::ast::stmt::Statement],
    close: &str,
) -> fmt::Result {
    f.write_str(open)?;
    f.write_str(" ")?;
    if !statements.is_empty() {
        separated(f, statements, "; ")?;
        f.write_str(" ")?;
    }
    f.write_str(close)
}

/// True when `value` is spelled like an identifier, whether or not it is a reserved
/// keyword.
///
/// The shape is the one of a regular T-SQL identifier, restricted to
/// what re-parses identically: the first character is a letter, `_` or `#` (a temporary
/// table), the following ones are letters, digits, `_`, `@`, `$` or `#`. A leading `@` is
/// excluded on purpose: it is the spelling of a variable, not of an identifier.
///
/// This is the whole rule in function-name position, where a reserved keyword is a
/// legitimate name; everywhere else [`is_regular_identifier`] also refuses the reserved
/// keywords.
pub(crate) fn has_identifier_shape(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_' || first == '#') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '@' | '$' | '#'))
}

/// True when `value` can be written as an identifier without brackets.
///
/// That is [`has_identifier_shape`] plus the safety rule: a reserved keyword written bare
/// would not parse back as a name, so `Display` brackets it. Everything else is written
/// between brackets too, which is the only spelling a parser accepts back.
pub(crate) fn is_regular_identifier(value: &str) -> bool {
    has_identifier_shape(value) && !is_reserved_keyword(value)
}

/// True when `value` is one of the reserved keywords of T-SQL, compared without regard to
/// case.
///
/// The single source of truth is [`Keyword::is_reserved`](crate::keyword::Keyword), which
/// holds the reserved keywords of T-SQL.
/// A word that is not a keyword at all, or a keyword T-SQL still accepts as an identifier
/// (`SUM`, `VARCHAR`, `WITHIN`), is not reserved.
fn is_reserved_keyword(value: &str) -> bool {
    Keyword::parse(value).is_some_and(Keyword::is_reserved)
}
