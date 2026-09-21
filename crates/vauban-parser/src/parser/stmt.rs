//! Statement dispatch: `parse_statement`.
//!
//! One entry point, one routing table, no grammar: [`parse_statement`] looks at the first
//! token or two of a statement and hands the cursor, still untouched, to the function of
//! the module that owns that statement. Each of those functions therefore reads its
//! own head keyword back, because it needs it for its span.
//!
//! Routing is done on [`Keyword`] values, never on the source text: `select`, `Select` and
//! `SELECT` are the same token to the lexer, and the dispatch has nothing to fold.

use vauban_errors::SqlResult;

use crate::ast::stmt::Statement;
use crate::keyword::Keyword;
use crate::parser::{Parser, ddl_db, ddl_table, dml, flow, query};
use crate::token::{Punct, TokenKind};

/// Parses one statement, whatever its kind.
///
/// The cursor sits on the first token of the statement and this function consumes
/// nothing before routing: the owner function of the statement reads its own head
/// keyword.
///
/// `first_in_batch` says whether the statement is the first of its batch. It exists for
/// the **implicit `EXEC`**: SQL Server reads a batch that starts with a bare name as a
/// call of the procedure of that name, so `sp_who` alone is `EXEC sp_who` while `SELECT 1
/// sp_who` is a syntax error. The last row of the table below reads the flag.
///
/// # Errors
///
/// The syntax error of the head token when no rule claims it, or whatever error the
/// owner function returns. A statement whose rule is a stub gives the generic internal
/// error 50000.
pub(crate) fn parse_statement(p: &mut Parser, first_in_batch: bool) -> SqlResult<Statement> {
    // `BEGIN…END`, `IF` and `WHILE` nest statements inside statements. The levels are
    // counted here, the one door the three come back through.
    p.nested(|p| parse_statement_inner(p, first_in_batch))
}

/// The body of [`parse_statement`], one level deeper on the nesting counter.
///
/// # Errors
///
/// The syntax error of the head token when no rule claims it, or whatever error the
/// owner function returns.
fn parse_statement_inner(p: &mut Parser, first_in_batch: bool) -> SqlResult<Statement> {
    match head_keyword(p, 0) {
        // `WITH` heads a CTE (V2) and `(` a parenthesised query, and both are a SELECT
        // statement; the query rule reports whatever they turn out not to be.
        Some(Keyword::Select | Keyword::With) => query::parse_select_statement(p),
        Some(Keyword::Insert) => dml::parse_insert(p),
        Some(Keyword::Update) => dml::parse_update(p),
        Some(Keyword::Delete) => dml::parse_delete(p),
        Some(Keyword::Truncate) => dml::parse_truncate(p),
        Some(Keyword::Merge) => dml::parse_merge(p),
        Some(Keyword::Create) => parse_create(p),
        Some(Keyword::Alter) => parse_alter(p),
        Some(Keyword::Drop) => parse_drop(p),
        Some(Keyword::Use) => ddl_db::parse_use(p),
        Some(Keyword::Declare) => flow::parse_declare(p),
        Some(Keyword::Set) => flow::parse_set(p),
        Some(Keyword::If) => flow::parse_if(p),
        Some(Keyword::While) => flow::parse_while(p),
        Some(Keyword::Begin) => parse_begin(p),
        Some(Keyword::Commit | Keyword::Rollback | Keyword::Save) => {
            flow::parse_transaction_control(p)
        }
        Some(Keyword::Print) => flow::parse_print(p),
        Some(Keyword::Exec | Keyword::Execute) => flow::parse_execute(p),
        Some(
            Keyword::Return
            | Keyword::Break
            | Keyword::Continue
            | Keyword::Goto
            | Keyword::Waitfor
            | Keyword::Throw,
        ) => flow::parse_simple(p),
        _ if p.at_punct(Punct::LeftParen) => query::parse_select_statement(p),
        // A name immediately followed by `:` is a label, regardless of its position in
        // the batch: this is checked ahead of the implicit `EXEC` below, which a bare
        // name with no `:` falls through to instead. A reserved word does not reach
        // here: `at_name` refuses it, so `order:` is claimed by no rule and reports the
        // same syntax error `ORDER` alone gets anywhere a statement is expected
        // (`tests/flow.rs`).
        _ if at_label(p) => flow::parse_label(p),
        // The implicit `EXEC`: a batch whose **first** statement is a bare name
        // calls the procedure of that name (`dbo.p 1` is `EXECUTE dbo.p 1`). Anywhere else
        // a bare name is claimed by no rule and falls through to the syntax error below.
        _ if first_in_batch && query::at_name(p, 0) => flow::parse_implicit_execute(p),
        _ => Err(p.error_here()),
    }
}

/// Whether the cursor sits on the head of a label: a name (`query::at_name`), then a
/// `:` with **no gap** -- no space, comment or newline -- between the two. SQL Server
/// reports the name alone, and no gap at all, when a space or a newline sits before the
/// `:` (`tests/flow.rs`): the name is then left unclaimed by any rule, exactly as an
/// ordinary bare name is anywhere else a statement is expected.
///
/// A delimited or quoted name reaches here too, and is refused further in by
/// `flow::parse_label_name` (`tests/flow.rs`): this decides the *shape* of a label, not
/// whether the name itself is legal.
fn at_label(p: &Parser) -> bool {
    if !query::at_name(p, 0) {
        return false;
    }
    let name = p.peek_at(0);
    let colon = p.peek_at(1);
    matches!(colon.kind, TokenKind::Punct(Punct::Colon))
        && name.span.offset.saturating_add(name.span.len) == colon.span.offset
}

/// The keyword `n` tokens ahead, or `None` when that token is not a keyword.
fn head_keyword(p: &Parser, n: usize) -> Option<Keyword> {
    match p.peek_at(n).kind {
        TokenKind::Keyword(keyword) => Some(keyword),
        _ => None,
    }
}

/// `CREATE`: the object word decides, and an index hides behind up to two adjectives.
fn parse_create(p: &mut Parser) -> SqlResult<Statement> {
    if at_create_index(p) {
        return ddl_db::parse_create_index(p);
    }
    match head_keyword(p, 1) {
        Some(Keyword::Table) => ddl_table::parse_create_table(p),
        Some(Keyword::Database) => ddl_db::parse_create_database(p),
        _ if at_programmability_object(p, 1) => ddl_db::parse_unsupported_ddl(p),
        _ => Err(p.error_here()),
    }
}

/// `ALTER`: only tables and databases are supported; the rest is refused by name.
fn parse_alter(p: &mut Parser) -> SqlResult<Statement> {
    match head_keyword(p, 1) {
        Some(Keyword::Table) => ddl_table::parse_alter_table(p),
        Some(Keyword::Database) => ddl_db::parse_alter_database(p),
        _ if at_programmability_object(p, 1) => ddl_db::parse_unsupported_ddl(p),
        _ => Err(p.error_here()),
    }
}

/// `DROP`: tables, databases and indexes; the rest is refused by name.
fn parse_drop(p: &mut Parser) -> SqlResult<Statement> {
    match head_keyword(p, 1) {
        Some(Keyword::Table) => ddl_table::parse_drop_table(p),
        Some(Keyword::Database) => ddl_db::parse_drop_database(p),
        Some(Keyword::Index) => ddl_db::parse_drop_index(p),
        _ if at_programmability_object(p, 1) => ddl_db::parse_unsupported_ddl(p),
        _ => Err(p.error_here()),
    }
}

/// `BEGIN` heads three unrelated statements, told apart by the word that follows.
fn parse_begin(p: &mut Parser) -> SqlResult<Statement> {
    match head_keyword(p, 1) {
        Some(Keyword::Tran | Keyword::Transaction) => flow::parse_begin_transaction(p),
        Some(Keyword::Try) => flow::parse_try_catch(p),
        // `BEGIN` on its own opens a block, whatever follows it.
        _ => flow::parse_block(p),
    }
}

/// Whether the `CREATE` the cursor sits on is a `CREATE INDEX`.
///
/// `UNIQUE` and `CLUSTERED`/`NONCLUSTERED` are optional and, in that order, come between
/// `CREATE` and `INDEX`, so the word to test is one, two or three tokens ahead.
fn at_create_index(p: &Parser) -> bool {
    let mut n = 1;
    if head_keyword(p, n) == Some(Keyword::Unique) {
        n += 1;
    }
    if matches!(
        head_keyword(p, n),
        Some(Keyword::Clustered | Keyword::NonClustered)
    ) {
        n += 1;
    }
    head_keyword(p, n) == Some(Keyword::Index)
}

/// Whether the token `n` ahead names a programmability object, which the engine does not
/// create yet (V2 or V3) and which `ddl_db::parse_unsupported_ddl` refuses by name.
///
/// Eight of the eleven words are in [`Keyword`] and are matched as keywords. The three
/// left -- `TYPE`, `LOGIN` and `ROLE` -- are not T-SQL keywords at all, so the lexer
/// makes plain identifiers of them and the only way to recognise them is their text,
/// compared without regard for ASCII case, exactly as `Keyword::parse` compares words. A
/// delimited name (`CREATE [type] x`) is a real name and is not one of them.
fn at_programmability_object(p: &Parser, n: usize) -> bool {
    if let Some(keyword) = head_keyword(p, n) {
        return matches!(
            keyword,
            Keyword::Procedure
                | Keyword::Proc
                | Keyword::Function
                | Keyword::View
                | Keyword::Trigger
                | Keyword::Schema
                | Keyword::Sequence
                | Keyword::User
        );
    }
    matches!(
        &p.peek_at(n).kind,
        TokenKind::Ident { value, quoted: false }
            if ["TYPE", "LOGIN", "ROLE"]
                .iter()
                .any(|word| value.eq_ignore_ascii_case(word))
    )
}

#[cfg(test)]
mod tests;
