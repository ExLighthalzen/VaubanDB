//! Flow of control, variables, session options, transactions and `EXECUTE`.
//!
//! The signatures are the ones `parser::stmt::parse_statement` calls.
//!
//! # Two statements are spelled `SET`
//!
//! `SET @x = 1` assigns a variable and `SET NOCOUNT ON` changes a session option; they are
//! two different nodes of the AST. The token right after `SET` tells them apart with no
//! backtracking at all: a [`TokenKind::Variable`] opens an assignment, anything else opens
//! an option, because no session option starts with `@`.
//!
//! # What is refused on purpose
//!
//! `GOTO` and `BEGIN TRY … END TRY BEGIN CATCH … END CATCH` are **accepted** by SQL Server
//! and refused here by a syntax error: the V1 subset has neither, and the AST variants
//! [`Statement::Goto`] and [`Statement::TryCatch`] stay unproduced: a deliberate
//! difference from SQL Server. The cursor reports the error on the word that is
//! out of the subset -- `GOTO`, and the `TRY` of a `BEGIN TRY` rather than its `BEGIN`.
//!
//! The cursor statements `OPEN`, `FETCH`, `CLOSE` and `DEALLOCATE`, `RAISERROR` and the
//! permission statements are V2 and V4: no rule claims them and the dispatch reports a
//! syntax error on their head word. `DECLARE @c CURSOR` is the exception, and on purpose:
//! the server accepts it, and so does `parse_declare`, as a variable whose type is spelled
//! `CURSOR`.
//!
//! # What the server does
//!
//! The grammar of this file follows what SQL Server does, which is not always what its
//! documentation says (`tests/flow.rs`):
//!
//! - **An empty block is refused.** `BEGIN END` yields `102 near 'END'`, and so do
//!   `BEGIN ; END`, `IF 1 = 1 BEGIN END` and `WHILE 1 = 0 BEGIN END`. A block holds **at
//!   least one** statement, and the error is reported on the `END`.
//! - **`SET` with nothing after it is reported on the `SET`.** `SET ;`, `SELECT 1; SET`
//!   and the one-word batch `SET` yield `156 near the keyword 'SET'`,
//!   where `SET 1` yields `102 near '1'` and `SET NOCOUNT` `102 near 'NOCOUNT'`. The
//!   position in the batch changes nothing: the head word of a rule that
//!   runs out of input is reported the same way first or last (`SELECT 1; DECLARE` and
//!   the one-word batch `DECLARE` both yield `102 near 'DECLARE'`).
//! - **`TRANSACTION` commits the statement to the isolation-level form.** `SET
//!   TRANSACTION`, `SET TRANSACTION ISOLATION` and `SET TRANSACTION ISOLATION LEVEL` are
//!   all syntax errors on their last word, and `SET TRANSACTION ISOLATION LEVEL FOO` is
//!   one on `FOO`: the level is one of the five T-SQL knows, and nothing else.
//! - **`WORK` takes no transaction name.** `COMMIT WORK t1` and `ROLLBACK WORK t1` yield
//!   `102 near 't1'`, where `COMMIT TRANSACTION t1` is accepted.
//! - **`WITH MARK` needs no name to parse.** `BEGIN TRANSACTION WITH MARK 'm'` fails with
//!   3901, a run-time error, not a syntax error: the form parses.
//! - **`PRINT` takes a value, not a predicate.** `PRINT 1 = 1` yields `102 near '='`.
//! - **A bare `RETURN` may be followed by anything.** `RETURN SELECT 1` is accepted as two
//!   statements, so the returned value is optional and read as a value expression only.
//! - **An option name may be several words.** `SET STATISTICS IO ON` is accepted, and so
//!   is `SET A B C ON` (195, a run-time error: `A` is not a recognized option).
//!
//! Two forms the server accepts and this file does not, both deliberate:
//!
//! - `BEGIN TRAN @n`, `COMMIT TRAN @n`: [`Statement::BeginTransaction`] names a
//!   transaction with an `Ident` and has nowhere to put a variable.
//! - `GOTO` and `BEGIN TRY … END CATCH`, refused on purpose (above).
//!
//! And one the server refuses and this file accepts: `SET STATISTICS IO` without a value
//! is `102 near 'IO'` on the server, where an option name is validated. Nothing is
//! validated here (`session` owns the list), so it parses as the
//! option `STATISTICS` with the value `IO`, the same shape as `SET DATEFORMAT ymd`.
//!
//! # Re-serialisation deviations
//!
//! All of them come from the AST not recording which of several spellings was written,
//! and are listed on the `display` module: `BEGIN TRAN` comes back
//! `BEGIN TRANSACTION`, `COMMIT WORK` and `COMMIT TRAN` come back `COMMIT TRANSACTION`,
//! `SAVE TRAN` comes back `SAVE TRANSACTION` and `EXEC` comes back `EXECUTE`. One more is
//! specific to this file: a `WITH MARK` written without a description comes back as `WITH
//! MARK ''`, since [`Statement::BeginTransaction`] has one field for both. The optional
//! `AS` of a `DECLARE` is in the same case: `DECLARE @x AS int` parses and comes back
//! `DECLARE @x int`, [`DeclareItem`] having no field for a word that carries nothing.
//!
//! The grammar follows the T-SQL reference of each statement. No third-party parser is
//! used.

use vauban_errors::SqlResult;

use crate::ast::expr::Expr;
use crate::ast::stmt::{
    AssignOp, AssignTarget, DeclareItem, DeclareStatement, ExecuteArg, ExecuteStatement,
    ExecuteTarget, SetOptionStatement, SetOptionValue, SetStatement, SetValue, Statement,
    WaitforKind, WaitforStatement,
};
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::parser::datatype::parse_data_type;
use crate::parser::expr::{parse_expr, parse_value_expr};
use crate::parser::query::at_name;
use crate::parser::{ddl_table, query, stmt};
use crate::token::{Op, Punct, TokenKind};

/// Parses a `DECLARE` statement, starting on its `DECLARE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// `DECLARE @c CURSOR` is read as a scalar variable whose type is spelled `CURSOR`, and
/// [`DeclareItem::Cursor`] stays unproduced (V2). The server accepts that statement, so
/// refusing it here would be further from it than letting the binder reject the type.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_declare(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Declare)?;
    let mut items = vec![parse_declare_item(p)?];
    while p.eat_punct(Punct::Comma) {
        items.push(parse_declare_item(p)?);
    }
    Ok(Statement::Declare(Box::new(DeclareStatement {
        items,
        span: p.span_from(start),
    })))
}

/// Reads one item of a `DECLARE`: a scalar variable with its type, or a table variable.
///
/// The `AS` between the name and the type is optional, on both forms:
/// `DECLARE @local_variable [AS] data_type` and `@table_variable_name [AS] TABLE`, and the
/// server accepts `DECLARE @x AS int`, `DECLARE @x AS int = 1, @y AS varchar(10)` and
/// `DECLARE @t AS TABLE (a int)`. The AST has no field for it, so `Display` writes the
/// type back without it; the parsed batches are equal, which is the loop's contract.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_declare_item(p: &mut Parser) -> SqlResult<DeclareItem> {
    let name = parse_variable(p)?;
    p.eat_keyword(Keyword::As);
    if p.eat_keyword(Keyword::Table) {
        // The cursor sits on the `(`; `ddl_table` consumes both parentheses.
        let definition = Box::new(ddl_table::parse_table_definition(p)?);
        return Ok(DeclareItem::TableVariable { name, definition });
    }
    let ty = parse_data_type(p)?;
    let default = if eat_op(p, Op::Eq) {
        Some(Box::new(parse_value_expr(p)?))
    } else {
        None
    };
    Ok(DeclareItem::Variable { name, ty, default })
}

/// Parses a `SET` statement, starting on its `SET` keyword; it is this function, not the dispatch, that tells `SET @x = 1` from `SET NOCOUNT ON`.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_set(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    // `SET ;`, `SELECT 1; SET` and a batch made of the single word `SET` are
    // three reported on the `SET` itself (156), where `SET 1` is reported on the `1` (102)
    // and `SET NOCOUNT` on the `NOCOUNT` (102). Nothing usable follows means the error is
    // on the head word, so the head word must still be under the cursor when it is built.
    if matches!(
        p.peek_at(1).kind,
        TokenKind::Eof | TokenKind::Punct(Punct::Semicolon)
    ) {
        return Err(p.error_here());
    }
    p.expect_keyword(Keyword::Set)?;
    if matches!(p.peek().kind, TokenKind::Variable) {
        parse_set_variable(p, start)
    } else {
        parse_set_option(p, start)
    }
}

/// Reads `SET @x = expr`, the cursor being just after the `SET`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_set_variable(p: &mut Parser, start: usize) -> SqlResult<Statement> {
    let target = AssignTarget::Variable(parse_variable(p)?);
    let op = parse_assign_op(p)?;
    let value = if at_parenthesised_query(p) {
        p.expect_punct(Punct::LeftParen)?;
        let query = query::parse_subquery(p)?;
        p.expect_punct(Punct::RightParen)?;
        SetValue::Query(query)
    } else {
        SetValue::Expr(parse_value_expr(p)?)
    };
    Ok(Statement::Set(Box::new(SetStatement {
        target,
        op,
        value,
        span: p.span_from(start),
    })))
}

/// Reads a session option, the cursor being just after the `SET`.
///
/// Nothing is validated: an unknown option name is not a syntax error for SQL Server
/// either (`SET FOO ON` yields 195, a run-time error: `FOO` is not a recognized option),
/// and `session` is what knows the list.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_set_option(p: &mut Parser, start: usize) -> SqlResult<Statement> {
    let (names, value) = if p.at_keyword(Keyword::Transaction) {
        parse_isolation_level(p)?
    } else if p.at_keyword(Keyword::IdentityInsert) {
        parse_identity_insert(p)?
    } else {
        parse_named_option(p)?
    };
    // The names of one statement share a single value, and `Display` writes the
    // value of the first entry only, so every entry has to carry the same one.
    let options = names
        .into_iter()
        .map(|name| (name, value.clone()))
        .collect();
    Ok(Statement::SetOption(Box::new(SetOptionStatement {
        options,
        span: p.span_from(start),
    })))
}

/// Reads `TRANSACTION ISOLATION LEVEL <level>`: a three-word option name and a level of
/// one or two words.
///
/// The three words are read as keywords and not through `Parser::parse_ident`, which
/// would refuse `TRANSACTION` and `READ` -- both are reserved. Seeing `TRANSACTION`
/// commits the statement to this form: `SET TRANSACTION`, `SET TRANSACTION
/// ISOLATION` and `SET TRANSACTION ISOLATION LEVEL` are all syntax errors rather than an
/// option named `TRANSACTION`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_isolation_level(p: &mut Parser) -> SqlResult<(Vec<String>, SetOptionValue)> {
    let transaction = p.advance().text;
    let isolation = expect_keyword_text(p, Keyword::Isolation)?;
    let level = expect_keyword_text(p, Keyword::Level)?;
    let name = format!("{transaction} {isolation} {level}");
    // The five isolation levels of T-SQL; anything else
    // is a syntax error, as `SET TRANSACTION ISOLATION LEVEL FOO` is on the server.
    let value = if p.at_keyword(Keyword::Read) {
        let read = p.advance().text;
        let which = match p.peek().kind {
            TokenKind::Keyword(Keyword::Committed | Keyword::Uncommitted) => p.advance().text,
            _ => return Err(p.error_here()),
        };
        format!("{read} {which}")
    } else if p.at_keyword(Keyword::Repeatable) {
        // The lexer reads `REPEATABLE READ` as two keywords, `Repeatable` then `Read`.
        let repeatable = p.advance().text;
        let read = expect_keyword_text(p, Keyword::Read)?;
        format!("{repeatable} {read}")
    } else if matches!(
        p.peek().kind,
        TokenKind::Keyword(Keyword::Snapshot | Keyword::Serializable)
    ) {
        p.advance().text
    } else {
        return Err(p.error_here());
    };
    Ok((vec![name], SetOptionValue::Word(value)))
}

/// Reads `IDENTITY_INSERT <table> {ON|OFF}`, whose option name carries the table.
///
/// `IDENTITY_INSERT` is a reserved word and is read as a keyword, for the same reason as
/// `TRANSACTION`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_identity_insert(p: &mut Parser) -> SqlResult<(Vec<String>, SetOptionValue)> {
    let word = p.advance().text;
    let table = p.parse_object_name()?;
    let value = match p.peek().kind {
        TokenKind::Keyword(Keyword::On) => {
            p.advance();
            SetOptionValue::On
        }
        TokenKind::Keyword(Keyword::Off) => {
            p.advance();
            SetOptionValue::Off
        }
        _ => return Err(p.error_here()),
    };
    Ok((vec![format!("{word} {table}")], value))
}

/// Reads the ordinary form: one or more option names, then one value for all of them.
///
/// An option name may be several words (`SET STATISTICS IO ON`) and several names may
/// share a value (`SET ANSI_NULLS, ANSI_PADDING ON`), so the words are collected first
/// and the value decides afterwards what the last of them was:
///
/// - `ON` or `OFF` closes the statement and every collected word belongs to the name;
/// - otherwise, with more than one word, the last one is the value (`SET DATEFORMAT ymd`);
/// - otherwise the value is an expression (`SET LOCK_TIMEOUT 5000`).
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_named_option(p: &mut Parser) -> SqlResult<(Vec<String>, SetOptionValue)> {
    let mut names = Vec::new();
    let mut words = vec![parse_option_word(p)?];
    loop {
        if p.eat_punct(Punct::Comma) {
            names.push(words.join(" "));
            words = vec![parse_option_word(p)?];
            continue;
        }
        if at_option_word(p) {
            words.push(parse_option_word(p)?);
            continue;
        }
        break;
    }
    let value = if p.eat_keyword(Keyword::On) {
        SetOptionValue::On
    } else if p.eat_keyword(Keyword::Off) {
        SetOptionValue::Off
    } else if words.len() > 1 {
        // The loop only stops on a token that is no word at all, so the last word read is
        // the value: `SET DATEFORMAT ymd`, `SET LANGUAGE us_english`.
        match words.pop() {
            Some(word) => SetOptionValue::Word(word),
            // A vector of more than one element always pops.
            None => unreachable!("words is not empty"),
        }
    } else {
        SetOptionValue::Value(parse_value_expr(p)?)
    };
    names.push(words.join(" "));
    Ok((names, value))
}

/// Whether the cursor sits on a word an option name or an option value may be made of.
///
/// `ON` and `OFF` are excluded: they are the value, and reading them as a word would eat
/// the end of the statement.
fn at_option_word(p: &Parser) -> bool {
    match &p.peek().kind {
        TokenKind::Ident { quoted, .. } => !quoted,
        TokenKind::Keyword(keyword) => !matches!(keyword, Keyword::On | Keyword::Off),
        _ => false,
    }
}

/// Reads one word of an option name or of an option value, as written.
///
/// A reserved keyword is a legitimate option name (`ROWCOUNT`, `TEXTSIZE`, `STATISTICS`),
/// so this does not go through `Parser::parse_ident`, which refuses them.
///
/// # Errors
///
/// The syntax error of the token the cursor sits on, which stays where it is.
fn parse_option_word(p: &mut Parser) -> SqlResult<String> {
    if at_option_word(p) {
        Ok(p.advance().text)
    } else {
        Err(p.error_here())
    }
}

/// Parses an `IF ... [ELSE ...]` statement, starting on its `IF` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The `ELSE` binds to the **nearest** `IF`, and the recursion is what does it: the inner
/// `IF` of `IF a IF b x ELSE y` is parsed as the `then_branch` of the outer one and reads
/// the `ELSE` before the outer one ever looks for it. On the server, `IF 1 = 1 IF
/// 2 = 3 PRINT 'x' ELSE PRINT 'y'` prints `y`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_if(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::If)?;
    let condition = parse_expr(p)?;
    let then_branch = Box::new(stmt::parse_statement(p, false)?);
    let else_branch = if p.eat_keyword(Keyword::Else) {
        Some(Box::new(stmt::parse_statement(p, false)?))
    } else {
        None
    };
    Ok(Statement::If {
        condition,
        then_branch,
        else_branch,
        span: p.span_from(start),
    })
}

/// Parses a `WHILE` statement, starting on its `WHILE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_while(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::While)?;
    let condition = parse_expr(p)?;
    let body = Box::new(stmt::parse_statement(p, false)?);
    Ok(Statement::While {
        condition,
        body,
        span: p.span_from(start),
    })
}

/// Parses a `BEGIN ... END` block, starting on its `BEGIN` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The `;` between two statements is optional and a run of them is one separator, exactly
/// as in `parse_batch`. A block holds **at least one** statement: `BEGIN END`
/// yields `102 near 'END'`, and so the error is reported on the `END`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_block(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Begin)?;
    let mut statements: Vec<Statement> = Vec::new();
    loop {
        while p.eat_punct(Punct::Semicolon) {}
        if p.at_keyword(Keyword::End) {
            if statements.is_empty() {
                return Err(p.error_here());
            }
            break;
        }
        // At the end of the batch no rule claims the token and the dispatch reports the
        // error, so the loop cannot spin.
        statements.push(stmt::parse_statement(p, false)?);
    }
    p.expect_keyword(Keyword::End)?;
    Ok(Statement::Block {
        statements,
        span: p.span_from(start),
    })
}

/// Parses a `BEGIN TRAN[SACTION]` statement, starting on its `BEGIN` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// A transaction named by a variable (`BEGIN TRAN @n`) is refused: the AST names a
/// transaction with an `Ident` and has nowhere to put the variable.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_begin_transaction(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Begin)?;
    expect_transaction_word(p)?;
    let name = if at_name(p, 0) {
        Some(p.parse_ident()?)
    } else {
        None
    };
    let mark = if p.eat_keyword_seq(&[Keyword::With, Keyword::Mark]) {
        // The description is optional: `BEGIN TRAN t1 WITH MARK` is accepted by the
        // server, and an absent description is an empty one.
        Some(eat_string_literal(p).unwrap_or_default())
    } else {
        None
    };
    Ok(Statement::BeginTransaction {
        name,
        mark,
        span: p.span_from(start),
    })
}

/// Parses a `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` statement (V2), starting on its `BEGIN` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// V1 has no structured error handling: the statement is refused, and the refusal names
/// `TRY` rather than the `BEGIN` it follows, because `TRY` is the word that is out of the
/// subset. `TRY` is not reserved, so this is a 102.
///
/// # Errors
///
/// Always: the syntax error on the `TRY`, the cursor left where it was.
pub(crate) fn parse_try_catch(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Begin)?;
    let error = p.error_here();
    p.reset(start);
    Err(error)
}

/// Parses a `COMMIT`, `ROLLBACK` or `SAVE` statement, starting on that keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// A name may follow `TRAN` or `TRANSACTION` and nothing else: `COMMIT WORK t1` and
/// `ROLLBACK WORK t1` both yield `102 near 't1'`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_transaction_control(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    if p.eat_keyword(Keyword::Save) {
        expect_transaction_word(p)?;
        let name = p.parse_ident()?;
        return Ok(Statement::Save {
            name,
            span: p.span_from(start),
        });
    }
    let commit = p.eat_keyword(Keyword::Commit);
    if !commit {
        p.expect_keyword(Keyword::Rollback)?;
    }
    let named = p.eat_keyword(Keyword::Tran) || p.eat_keyword(Keyword::Transaction);
    if !named {
        let _ = p.eat_keyword(Keyword::Work);
    }
    let name = if named && at_name(p, 0) {
        Some(p.parse_ident()?)
    } else {
        None
    };
    let span = p.span_from(start);
    Ok(if commit {
        Statement::Commit { name, span }
    } else {
        Statement::Rollback { name, span }
    })
}

/// Parses a `PRINT` statement, starting on its `PRINT` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// What follows is a **value**, not a predicate: `PRINT 1 = 1` yields
/// `102 near '='`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_print(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Print)?;
    let expr = parse_value_expr(p)?;
    Ok(Statement::Print {
        expr,
        span: p.span_from(start),
    })
}

/// Parses an `EXEC[UTE]` statement, starting on its `EXEC` or `EXECUTE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_execute(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    if !p.eat_keyword(Keyword::Exec) && !p.eat_keyword(Keyword::Execute) {
        return Err(p.error_here());
    }
    if p.at_punct(Punct::LeftParen) {
        p.expect_punct(Punct::LeftParen)?;
        // The T-SQL inside the string is **not** parsed here: it is a string like any
        // other, built by any concatenation of values.
        let text = parse_value_expr(p)?;
        p.expect_punct(Punct::RightParen)?;
        return Ok(Statement::Execute(Box::new(ExecuteStatement {
            target: ExecuteTarget::Literal(Box::new(text)),
            args: Vec::new(),
            return_into: None,
            implicit: false,
            span: p.span_from(start),
        })));
    }
    parse_execute_call(p, start, false)
}

/// Parses the implicit `EXEC` of a batch that starts with a bare name: `sp_who` alone is
/// `EXECUTE sp_who`, and `dbo.p 1` is `EXECUTE dbo.p 1`.
///
/// The word `EXECUTE` may be left out when the statement is the **first** of its batch.
/// `stmt::parse_statement` is the one that knows, through its `first_in_batch` flag, and
/// calls this function; anywhere else a bare name is a syntax error: `dbo.p 1` yields
/// 2812 (unknown stored procedure `dbo.p`), while `SELECT 1; dbo.p 1` yields `102 near
/// 'dbo'`.
///
/// What opens the form is a bare **name**, not any first word: a batch made of a single
/// reserved keyword is a syntax error on the server too, exactly the one it gives that
/// keyword in the middle of a batch. One word per batch: `SELEC` and `GO2`
/// yield 2812, where `SELECT`, `CREATE`, `DECLARE`, `IF`, `PRINT` and `EXEC` yield
/// `102 near '<the word>'` and `SET` yields `156 near the keyword 'SET'` --
/// the same numbers and the same words as after a `SELECT 1;`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_implicit_execute(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    parse_execute_call(p, start, true)
}

/// Reads the call itself -- the optional return variable, the target and the arguments --
/// the cursor being on the target (or on the `@rc` that precedes it).
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_execute_call(p: &mut Parser, start: usize, implicit: bool) -> SqlResult<Statement> {
    let return_into = if !implicit && at_named_variable(p) {
        let name = parse_variable(p)?;
        eat_op(p, Op::Eq);
        Some(name)
    } else {
        None
    };
    let target = if matches!(p.peek().kind, TokenKind::Variable) {
        ExecuteTarget::Variable(parse_variable(p)?)
    } else {
        ExecuteTarget::Procedure(p.parse_object_name()?)
    };
    let args = parse_execute_args(p)?;
    Ok(Statement::Execute(Box::new(ExecuteStatement {
        target,
        args,
        return_into,
        implicit,
        span: p.span_from(start),
    })))
}

/// Reads the argument list of an `EXECUTE`, which may be empty.
///
/// There is no keyword to close a call, so what tells "no argument" from "a bad argument"
/// is a try and a fallback: when the first argument does not parse, the cursor goes back
/// and the call has none. That is what makes `EXEC p SELECT 1` two statements, as it is on
/// the server.
///
/// # Errors
///
/// The syntax error of an argument that follows a `,`, which is not optional.
fn parse_execute_args(p: &mut Parser) -> SqlResult<Vec<ExecuteArg>> {
    let start = p.mark();
    let first = match parse_execute_arg(p) {
        Ok(arg) => arg,
        Err(_) => {
            p.reset(start);
            return Ok(Vec::new());
        }
    };
    let mut args = vec![first];
    while p.eat_punct(Punct::Comma) {
        args.push(parse_execute_arg(p)?);
    }
    Ok(args)
}

/// Reads one argument: `[@name =] value [OUTPUT|OUT]`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_execute_arg(p: &mut Parser) -> SqlResult<ExecuteArg> {
    let name = if at_named_variable(p) {
        let name = parse_variable(p)?;
        eat_op(p, Op::Eq);
        Some(name)
    } else {
        None
    };
    let value = parse_value_expr(p)?;
    let output = p.eat_keyword(Keyword::Output) || p.eat_keyword(Keyword::Out);
    Ok(ExecuteArg {
        name,
        value,
        output,
    })
}

/// Whether the cursor sits on a `@x =`, the shape of a named argument and of the return
/// variable of an `EXECUTE`.
fn at_named_variable(p: &Parser) -> bool {
    matches!(p.peek().kind, TokenKind::Variable)
        && matches!(p.peek_at(1).kind, TokenKind::Op(Op::Eq))
}

/// Parses one of the keyword-led statements with no clause of their own: `RETURN`, `BREAK`, `CONTINUE`, `GOTO`, `WAITFOR` and `THROW`.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse, and always one for `GOTO`, which is out of
/// the V1 subset.
pub(crate) fn parse_simple(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    match p.peek().kind {
        TokenKind::Keyword(Keyword::Break) => {
            p.advance();
            Ok(Statement::Break(p.span_from(start)))
        }
        TokenKind::Keyword(Keyword::Continue) => {
            p.advance();
            Ok(Statement::Continue(p.span_from(start)))
        }
        TokenKind::Keyword(Keyword::Return) => {
            p.advance();
            Ok(Statement::Return {
                value: parse_optional_value(p),
                span: p.span_from(start),
            })
        }
        TokenKind::Keyword(Keyword::Waitfor) => parse_waitfor(p, start),
        TokenKind::Keyword(Keyword::Throw) => parse_throw(p, start),
        // `GOTO` is a reserved word SQL Server accepts and V1 does not: the refusal is
        // reported on it, a deliberate difference.
        _ => Err(p.error_here()),
    }
}

/// Reads `WAITFOR {DELAY|TIME} expr` (V2), the cursor being on its `WAITFOR`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_waitfor(p: &mut Parser, start: usize) -> SqlResult<Statement> {
    p.expect_keyword(Keyword::Waitfor)?;
    let kind = if p.eat_keyword(Keyword::Delay) {
        WaitforKind::Delay
    } else {
        p.expect_keyword(Keyword::Time)?;
        WaitforKind::Time
    };
    let value = parse_value_expr(p)?;
    Ok(Statement::Waitfor(Box::new(WaitforStatement {
        kind,
        value,
        span: p.span_from(start),
    })))
}

/// Reads `THROW [number, message, state]` (V3), the cursor being on its `THROW`.
///
/// The three parts come together, or the statement is bare.
///
/// # Errors
///
/// The syntax error that stopped the parse of the three parts, once the first one was
/// read.
fn parse_throw(p: &mut Parser, start: usize) -> SqlResult<Statement> {
    p.expect_keyword(Keyword::Throw)?;
    let mark = p.mark();
    let (number, message, state) = match parse_value_expr(p) {
        Ok(number) => {
            p.expect_punct(Punct::Comma)?;
            let message = parse_value_expr(p)?;
            p.expect_punct(Punct::Comma)?;
            let state = parse_value_expr(p)?;
            (
                Some(Box::new(number)),
                Some(Box::new(message)),
                Some(Box::new(state)),
            )
        }
        Err(_) => {
            p.reset(mark);
            (None, None, None)
        }
    };
    Ok(Statement::Throw {
        number,
        message,
        state,
        span: p.span_from(start),
    })
}

/// Reads the optional value of a `RETURN`, or nothing when what follows starts no value.
///
/// A bare `RETURN` may be followed by anything, the end of the batch, an `END` or another
/// statement, and none of those parses as a value: a failed attempt is what says there is
/// no value, and it costs nothing since it consumes nothing.
fn parse_optional_value(p: &mut Parser) -> Option<Expr> {
    let start = p.mark();
    match parse_value_expr(p) {
        Ok(value) => Some(value),
        Err(_) => {
            p.reset(start);
            None
        }
    }
}

/// Reads the `TRAN` or `TRANSACTION` word every transaction statement but `COMMIT` and
/// `ROLLBACK` requires.
///
/// # Errors
///
/// The syntax error of the token the cursor sits on, which stays where it is.
fn expect_transaction_word(p: &mut Parser) -> SqlResult<()> {
    if p.eat_keyword(Keyword::Tran) || p.eat_keyword(Keyword::Transaction) {
        Ok(())
    } else {
        Err(p.error_here())
    }
}

/// Consumes the keyword `k` and returns the text it was written with.
///
/// # Errors
///
/// The syntax error of the token the cursor sits on, which stays where it is.
fn expect_keyword_text(p: &mut Parser, k: Keyword) -> SqlResult<String> {
    if p.at_keyword(k) {
        Ok(p.advance().text)
    } else {
        Err(p.error_here())
    }
}

/// Consumes a `@x` or `@@x` token and returns its name, the `@` signs included.
///
/// # Errors
///
/// The syntax error of the token the cursor sits on, which stays where it is.
fn parse_variable(p: &mut Parser) -> SqlResult<String> {
    if matches!(p.peek().kind, TokenKind::Variable) {
        Ok(p.advance().text)
    } else {
        Err(p.error_here())
    }
}

/// Consumes the assignment operator of a `SET @x = 1` or of a `SET @x += 1`.
///
/// # Errors
///
/// The syntax error of the token the cursor sits on, which stays where it is.
fn parse_assign_op(p: &mut Parser) -> SqlResult<AssignOp> {
    let op = match p.peek().kind {
        TokenKind::Op(Op::Eq) => AssignOp::Set,
        TokenKind::Op(Op::PlusEq) => AssignOp::AddAssign,
        TokenKind::Op(Op::MinusEq) => AssignOp::SubAssign,
        TokenKind::Op(Op::StarEq) => AssignOp::MulAssign,
        TokenKind::Op(Op::SlashEq) => AssignOp::DivAssign,
        TokenKind::Op(Op::PercentEq) => AssignOp::ModAssign,
        TokenKind::Op(Op::AmpersandEq) => AssignOp::BitAndAssign,
        TokenKind::Op(Op::PipeEq) => AssignOp::BitOrAssign,
        TokenKind::Op(Op::CaretEq) => AssignOp::BitXorAssign,
        _ => return Err(p.error_here()),
    };
    p.advance();
    Ok(op)
}

/// Whether the cursor sits on the `(` of a parenthesised query, as in
/// `SET @x = (SELECT 1)`.
fn at_parenthesised_query(p: &Parser) -> bool {
    p.at_punct(Punct::LeftParen) && matches!(p.peek_at(1).kind, TokenKind::Keyword(Keyword::Select))
}

/// Consumes the operator `op` if it is there, and says whether it was.
fn eat_op(p: &mut Parser, op: Op) -> bool {
    let found = matches!(p.peek().kind, TokenKind::Op(found) if found == op);
    if found {
        p.advance();
    }
    found
}

/// Consumes a character string literal if the cursor sits on one, and returns its value.
fn eat_string_literal(p: &mut Parser) -> Option<String> {
    let value = match &p.peek().kind {
        TokenKind::Str { value, .. } => value.clone(),
        _ => return None,
    };
    p.advance();
    Some(value)
}
