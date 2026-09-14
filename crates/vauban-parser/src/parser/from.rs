//! The `FROM` clause, joins and derived tables.
//!
//! The keyword `FROM` itself is consumed by `query::parse_query_spec`, so that the error
//! of `SELECT * FROM;` falls on the `;`; everything after it is read here.
//!
//! # Shape of the clause
//!
//! A `FROM` is a `,`-separated list of **join trees**. The comma is the historical cross
//! join, so `FROM a, b` and `FROM a CROSS JOIN b` mean the same thing and are kept
//! apart in the AST: the comma yields two [`TableRef`] in the list, `CROSS JOIN` yields
//! one [`TableRef::Join`].
//!
//! Joins are **left-associative**: `a JOIN b ON … JOIN c ON …` is `(a JOIN b) JOIN c`.
//! `CROSS APPLY` and `OUTER APPLY` (V2) sit in the same loop and associate the same way.
//! Every join but `CROSS JOIN` requires its `ON`: T-SQL has no implicit cross join, and a
//! missing `ON` is a syntax error rather than a silently widened result.
//!
//! # Re-serialisation deviation
//!
//! The `OUTER` of `LEFT OUTER JOIN` is optional and [`JoinKind`] does not record it, so
//! `Display` always writes `LEFT JOIN`. The AST of both spellings is the same one, which
//! is what the `parse` -> `Display` -> `parse` loop needs; the deviation is deliberate.
//!
//! # The meanings of a `(` after a table reference
//!
//! The rule depends only on **where** the parenthesis stands, never on what it holds:
//!
//! - after a **derived table**, `(` opens the **column list** of its alias:
//!   `(SELECT 1) d (n)`;
//! - **glued to a name**, before any alias, `(` opens the **arguments of a function**:
//!   `f(a.id) AS x`, and `t (NOLOCK)` just the same;
//! - after the **alias** of a name, `(` opens the deprecated **hint list** written
//!   without `WITH`: `t AS z (NOLOCK)`. `WITH (…)` stands in the same place.
//!
//! The second rule is the one that matters. A parser that looked ahead and guessed, from
//! the shape of what the parentheses held, whether they were a hint list or the arguments
//! of a table-valued function would be wrong: SQL Server makes no such choice. It reads
//! an expression list, and it is its **binder** that, the object turning out not to be a
//! function, re-reads a lone bare identifier as a hint or gives up with error 215. On SQL
//! Server (a batch per line, `t` a table and `f` a function):
//!
//! ```text
//! SELECT * FROM dbo.t (a);   -- 207 (unknown column a), then 215 (parameters supplied
//!                            --     to an object that is not a function)
//! SELECT * FROM dbo.t (1);   -- 215 alone: 1 resolves, dbo.t is still no function
//! SELECT * FROM dbo.t ();    -- 215 too: even an empty list is a parameter list
//! SELECT * FROM dbo.t (NOLOCK);       -- rows: the binder re-reads the lone name
//! SELECT * FROM dbo.t (NOLOCK) AS z;  -- rows: the hint sits before the alias
//! SELECT * FROM dbo.f (NOLOCK);       -- 207 (unknown column NOLOCK): dbo.f *is* a
//!                                     -- function, so no hint is ever looked for
//! SELECT * FROM dbo.f(1) WITH (NOLOCK);  -- 102 near ')': no hints after `nom(…)`
//! ```
//!
//! So `nom(…)` is always a [`TableRef::Function`] here, whatever the parentheses hold, and
//! it carries no hint clause. Raising 215 belongs to the binder; the parser
//! hands it the name and the arguments.
//!
//! Hints proper are read, kept in [`TableHint`] as they were written, and never
//! interpreted: the engine ignores them.
//!
//! # Deviations on the deprecated hint list
//!
//! Written without `WITH`, SQL Server's hint list is far narrower than the one read by
//! [`parse_hint_list`]: exactly one word, with no arguments. VaubanDB accepts the wider
//! list, which only ever means accepting a query SQL Server refuses -- the module never
//! interprets a hint, so nothing downstream depends on it:
//!
//! ```text
//! SELECT * FROM dbo.t AS z (NOLOCK, ROWLOCK); -- 1018 near 'ROWLOCK'
//! SELECT * FROM dbo.t AS z (INDEX(1));        -- 1018 near 'INDEX'
//! SELECT * FROM dbo.t AS z (a);               -- 321 (a is not a table hint)
//! SELECT * FROM dbo.t (INDEX(1));             -- 1018 as well, glued to the name
//! ```
//!
//! Neither 321 nor 1018 is at the catalogue of `errors`, and none of the three texts is
//! refused here: the last one even reads as a call whose argument is the call `INDEX(1)`,
//! a reserved word being a legal function name.
//!
//! The other way round, `SELECT * FROM dbo.t (HOLDLOCK)` returns its rows on SQL Server
//! while VaubanDB answers a syntax error: `HOLDLOCK` is a **reserved** word, so it
//! spells no expression, and the parentheses that follow a name now hold expressions.
//! Reading it as a hint again would need a look-ahead into the parentheses, and storing
//! it as an identifier would break the `parse` -> `Display` -> `parse` loop, `Display`
//! bracketing a reserved word. `NOLOCK`, `ROWLOCK`, `TABLOCK`, `READPAST`,
//! `READCOMMITTED` and the other usual hints are not reserved and go through.
//!
//! The grammar follows the T-SQL reference of the `FROM` clause and of table hints.

use vauban_errors::SqlResult;

use crate::ast::expr::{Expr, Ident};
use crate::ast::query::{ApplyKind, JoinKind, TableHint, TableRef};
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::parser::expr::{parse_expr, parse_expr_list};
use crate::parser::query::{at_name, parse_subquery};
use crate::token::{Punct, TokenKind};

/// Reads the table references of a `FROM` clause, its keyword already consumed.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the references.
pub(crate) fn parse_from(p: &mut Parser) -> SqlResult<Vec<TableRef>> {
    let mut refs = vec![parse_join_tree(p)?];
    while p.eat_punct(Punct::Comma) {
        refs.push(parse_join_tree(p)?);
    }
    Ok(refs)
}

/// Reads one element of the `,`-separated list: a source and every join hanging off it.
///
/// The loop is what makes the tree lean **left**: each join built becomes the left
/// operand of the next one.
///
/// # Errors
///
/// The syntax error that stopped the parse, a missing `ON` included.
fn parse_join_tree(p: &mut Parser) -> SqlResult<TableRef> {
    let start = p.mark();
    let mut left = parse_table_primary(p)?;
    loop {
        if let Some(kind) = eat_apply_kind(p) {
            let right = parse_table_primary(p)?;
            left = TableRef::Apply {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                span: p.span_from(start),
            };
            continue;
        }
        let Some(kind) = eat_join_kind(p) else { break };
        let right = parse_table_primary(p)?;
        // Every join but `CROSS JOIN` requires its `ON`.
        let on = if kind == JoinKind::Cross {
            None
        } else {
            p.expect_keyword(Keyword::On)?;
            Some(parse_expr(p)?)
        };
        left = TableRef::Join {
            left: Box::new(left),
            right: Box::new(right),
            kind,
            on,
            span: p.span_from(start),
        };
    }
    Ok(left)
}

/// Reads the `CROSS APPLY` or `OUTER APPLY` (V2) the cursor sits on, or nothing at all.
///
/// Tried **before** [`eat_join_kind`], since `CROSS` heads both `CROSS JOIN` and `CROSS
/// APPLY`. Nothing is consumed when the word that follows is not `APPLY`.
fn eat_apply_kind(p: &mut Parser) -> Option<ApplyKind> {
    if p.eat_keyword_seq(&[Keyword::Cross, Keyword::Apply]) {
        return Some(ApplyKind::Cross);
    }
    if p.eat_keyword_seq(&[Keyword::Outer, Keyword::Apply]) {
        return Some(ApplyKind::Outer);
    }
    None
}

/// Reads the join operator the cursor sits on, or nothing at all.
///
/// `INNER` is the default, so a bare `JOIN` is an inner join, and `OUTER` is optional
/// after `LEFT`, `RIGHT` and `FULL`. All or nothing: a `LEFT` that no `JOIN` follows is
/// given back untouched, because it may open something else entirely (`LEFT(a, 1)` is a
/// function call).
fn eat_join_kind(p: &mut Parser) -> Option<JoinKind> {
    let start = p.mark();
    let kind = if p.eat_keyword(Keyword::Inner) {
        JoinKind::Inner
    } else if p.eat_keyword(Keyword::Left) {
        let _ = p.eat_keyword(Keyword::Outer);
        JoinKind::Left
    } else if p.eat_keyword(Keyword::Right) {
        let _ = p.eat_keyword(Keyword::Outer);
        JoinKind::Right
    } else if p.eat_keyword(Keyword::Full) {
        let _ = p.eat_keyword(Keyword::Outer);
        JoinKind::Full
    } else if p.eat_keyword(Keyword::Cross) {
        JoinKind::Cross
    } else if p.at_keyword(Keyword::Join) {
        JoinKind::Inner
    } else {
        return None;
    };
    if p.eat_keyword(Keyword::Join) {
        return Some(kind);
    }
    p.reset(start);
    None
}

/// Reads one source: a named table, a derived table, a table variable (V2) or a name
/// given arguments. Joins are read by [`parse_join_tree`], never here.
///
/// A `(` glued to the name always opens **arguments**, never a hint list: see the module
/// header for what SQL Server answers. A [`TableRef::Function`] is therefore not
/// a promise that the name is a function -- only the binder knows, and it is what turns
/// `dbo.t (NOLOCK)` into a hint and `dbo.t (1)` into error 215.
///
/// # Errors
///
/// The syntax error that stopped the parse of the source.
fn parse_table_primary(p: &mut Parser) -> SqlResult<TableRef> {
    let start = p.mark();
    if matches!(p.peek().kind, TokenKind::Variable) {
        // `Token::text` of a variable is its whole name, `@` included.
        let name = p.advance().text;
        let alias = parse_table_alias(p)?;
        return Ok(TableRef::Variable {
            name,
            alias,
            span: p.span_from(start),
        });
    }
    if p.at_punct(Punct::LeftParen) {
        return parse_derived_table(p);
    }
    let name = p.parse_object_name()?;
    if p.at_punct(Punct::LeftParen) {
        // Arguments, always: `f(a.id)`, `t (NOLOCK)` and `t ()` alike. No hint clause
        // follows one, SQL Server answering 102 to `FROM dbo.f(1) WITH (NOLOCK)`.
        let args = parse_function_args(p)?;
        let alias = parse_table_alias(p)?;
        return Ok(TableRef::Function {
            name,
            args,
            alias,
            span: p.span_from(start),
        });
    }
    let alias = parse_table_alias(p)?;
    let hints = parse_table_hints(p)?;
    Ok(TableRef::Table {
        name,
        alias,
        hints,
        span: p.span_from(start),
    })
}

/// Reads a derived table `(SELECT …) AS d (c1, c2)`, the cursor on its `(`.
///
/// The alias is **required**: SQL Server answers "Syntax error near ')'" to a derived
/// table without one. The error is reported on the token that follows the closing
/// parenthesis, which is where the alias should have been.
///
/// # Errors
///
/// The syntax error that stopped the parse of the subquery, of the alias or of its
/// column list.
fn parse_derived_table(p: &mut Parser) -> SqlResult<TableRef> {
    let start = p.mark();
    p.expect_punct(Punct::LeftParen)?;
    let query = parse_subquery(p)?;
    p.expect_punct(Punct::RightParen)?;
    let alias = parse_table_alias(p)?;
    if alias.is_none() {
        return Err(p.error_here());
    }
    let columns = if p.at_punct(Punct::LeftParen) {
        parse_column_alias_list(p)?
    } else {
        Vec::new()
    };
    Ok(TableRef::Derived {
        query,
        alias,
        columns,
        span: p.span_from(start),
    })
}

/// Reads the alias of a table reference, with or without `AS`, when there is one.
///
/// A bare alias is told from the next clause by the same rule as a select item: a
/// **reserved** word is never a bare alias, so `t WHERE …`, `t JOIN …`, `t ON …`,
/// `t GROUP …`, `t ORDER …`, `t UNION …` and `t WITH (…)` all end the reference. Unlike a
/// select item, a table alias is never a character string.
///
/// # Errors
///
/// The syntax error of the token that follows an `AS` and spells no alias.
fn parse_table_alias(p: &mut Parser) -> SqlResult<Option<Ident>> {
    if p.eat_keyword(Keyword::As) {
        return Ok(Some(p.parse_ident()?));
    }
    if !at_name(p, 0) {
        return Ok(None);
    }
    // `at_name` is exactly the case `parse_ident` accepts.
    Ok(p.parse_ident().ok())
}

/// Reads the column list an alias may rename a derived table with: `(c1, c2)`.
///
/// # Errors
///
/// The syntax error of a list item that is not a name, or of a missing `)`.
fn parse_column_alias_list(p: &mut Parser) -> SqlResult<Vec<Ident>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut columns = vec![p.parse_ident()?];
    while p.eat_punct(Punct::Comma) {
        columns.push(p.parse_ident()?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(columns)
}

/// Reads the parenthesised arguments a name is given, `f()` included.
///
/// An empty list is a list all the same: SQL Server answers 215 to `FROM dbo.t ()`, which
/// only makes sense if it saw parameters supplied.
///
/// # Errors
///
/// The syntax error that stopped the parse of an argument, or of a missing `)`.
fn parse_function_args(p: &mut Parser) -> SqlResult<Vec<Expr>> {
    p.expect_punct(Punct::LeftParen)?;
    if p.eat_punct(Punct::RightParen) {
        return Ok(Vec::new());
    }
    let args = parse_expr_list(p)?;
    p.expect_punct(Punct::RightParen)?;
    Ok(args)
}

/// Reads the hints that follow a table reference and its alias, when there are any.
///
/// Two spellings: `WITH (…)`, and the deprecated one without `WITH`. Both stand **after**
/// the alias -- parentheses glued to the name are arguments, and never reach this point.
/// `WITH` is a **reserved** word, so it is never read as a bare alias and is tested here
/// before anything else; a `WITH` that no `(` follows is left where it is, for the caller
/// to report.
///
/// # Errors
///
/// The syntax error that stopped the parse of the hint list.
fn parse_table_hints(p: &mut Parser) -> SqlResult<Vec<TableHint>> {
    if p.at_keyword(Keyword::With)
        && matches!(p.peek_at(1).kind, TokenKind::Punct(Punct::LeftParen))
    {
        p.advance();
        return parse_hint_list(p);
    }
    if p.at_punct(Punct::LeftParen) {
        return parse_hint_list(p);
    }
    Ok(Vec::new())
}

/// Reads `(hint, hint(arg, arg), …)`, the cursor on the `(`.
///
/// Nothing is validated: an unknown hint, an argument that means nothing, a hint that
/// contradicts another are all kept as written. The engine ignores them all.
///
/// # Errors
///
/// The syntax error that stopped the parse of the list.
fn parse_hint_list(p: &mut Parser) -> SqlResult<Vec<TableHint>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut hints = vec![parse_hint(p)?];
    while p.eat_punct(Punct::Comma) {
        hints.push(parse_hint(p)?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(hints)
}

/// Reads one hint: a word, then its parenthesised arguments when it has any.
///
/// The word is taken from the **token text**, not through `Parser::parse_ident`: half the
/// hint names of T-SQL are reserved words (`INDEX`, `HOLDLOCK`, `READCOMMITTED`), and a
/// hint name is not an identifier the binder will ever resolve.
///
/// # Errors
///
/// The syntax error of a token that spells no hint name or no argument.
fn parse_hint(p: &mut Parser) -> SqlResult<TableHint> {
    let start = p.mark();
    if !at_hint_word(p, 0) {
        return Err(p.error_here());
    }
    let name = p.advance().text;
    let mut args = Vec::new();
    if p.eat_punct(Punct::LeftParen) {
        loop {
            if !at_hint_arg(p, 0) {
                return Err(p.error_here());
            }
            args.push(p.advance().text);
            if !p.eat_punct(Punct::Comma) {
                break;
            }
        }
        p.expect_punct(Punct::RightParen)?;
    }
    Ok(TableHint {
        name,
        args,
        span: p.span_from(start),
    })
}

/// Whether the token `n` ahead spells a hint name: a bare identifier or any keyword,
/// reserved or not.
fn at_hint_word(p: &Parser, n: usize) -> bool {
    matches!(
        &p.peek_at(n).kind,
        TokenKind::Ident { quoted: false, .. } | TokenKind::Keyword(_)
    )
}

/// Whether the token `n` ahead spells the argument of a hint: a word or a whole number,
/// which is all `INDEX(1)`, `INDEX(ix)` and `MAXDOP(2)` ever need.
fn at_hint_arg(p: &Parser, n: usize) -> bool {
    at_hint_word(p, n) || matches!(p.peek_at(n).kind, TokenKind::Integer)
}
