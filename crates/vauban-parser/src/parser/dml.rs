//! `INSERT`, `UPDATE`, `DELETE` and `TRUNCATE TABLE`.
//!
//! The signatures are the ones `parser::stmt::parse_statement` calls. `MERGE` is V3 and
//! [`parse_merge`] is deliberately left as a stub.
//!
//! # Where the clauses of a DML statement stand
//!
//! As the T-SQL reference of `INSERT`, `UPDATE`, `DELETE` and the `OUTPUT` clause states
//! them, each shape as SQL Server accepts it:
//!
//! ```text
//! INSERT [TOP (n) [PERCENT]] [INTO] target [WITH (hints)] [(columns)] [OUTPUT …] source
//! UPDATE [TOP (n) [PERCENT]] target [WITH (hints)] SET … [OUTPUT …] [FROM …] [WHERE …]
//! DELETE [TOP (n) [PERCENT]] [FROM] target [WITH (hints)] [OUTPUT …] [FROM …] [WHERE …]
//! TRUNCATE TABLE name
//! ```
//!
//! `OUTPUT` therefore comes **before** the `FROM` of the join sources and before the
//! `WHERE`: `UPDATE t SET a = 1 WHERE b = 2 OUTPUT DELETED.a` is a syntax error on the
//! server (102 near `'OUTPUT'`), and so is `DELETE a FROM t AS a OUTPUT DELETED.id WHERE
//! …`.
//!
//! # The two `FROM` of a `DELETE`
//!
//! The first is decorative and precedes the target, the second opens the list of join
//! sources: `DELETE FROM a FROM t AS a WHERE a.z = 1` is legal and uses both. `Display`
//! always writes the first one, so `DELETE t` re-serialises as `DELETE FROM t`; the AST
//! of the two spellings is the same one.
//!
//! # `TOP` in a DML statement
//!
//! SQL Server **requires** the parentheses here -- `DELETE TOP 10 FROM t` is answered
//! with a 102 near `'10'` -- and refuses `WITH TIES`, which belongs to `SELECT` alone
//! (`DELETE TOP (1) WITH TIES FROM t` is a 156 near `'WITH'`). The `WITH TIES` refusal is
//! kept, since it costs nothing: the words are simply not read. The parenthesis
//! requirement is **not** enforced here: both spellings are
//! accepted here and the binder is the stage that will refuse the bare one.
//!
//! # The target of an `INSERT`
//!
//! [`InsertStatement::target`] is a [`TableRef`], as the target of an `UPDATE` and of a
//! `DELETE`: a named table with its hints, a table variable, or a name given parentheses.
//! Each shape is asserted in `tests/dml.rs` (`insert_targets`, `insert_target_errors`,
//! `insert_function_target`):
//!
//! - `INSERT INTO @t`, `INSERT @t`, `INSERT INTO #t`, `INSERT INTO ##t`, `INSERT INTO
//!   db..t`, `INSERT INTO [srv].[db].[dbo].[t]`, `INSERT INTO t WITH (TABLOCK, HOLDLOCK)`
//!   and `INSERT INTO [@t]` (a bracketed name, not a variable) are accepted.
//! - `INSERT INTO @t WITH (TABLOCK)` is a 156 near `'WITH'`: a variable takes no hint.
//! - `INSERT INTO @t AS x` is a 156 near `'AS'`, `INSERT INTO @t x` a 102 near `'x'`,
//!   `INSERT INTO @t.x` a 102 near `'.'`: a target takes no alias and a variable no part.
//! - `INSERT INTO f() …` is **accepted** by the grammar and refused later, by name
//!   resolution (208, unknown object `f`; 215 when the name is
//!   a table). See [`parse_insert_target`] for how the `(` after the name is told apart
//!   from the column list.
//!
//! # What the AST cannot carry, and is therefore refused
//!
//! - `TRUNCATE TABLE t WITH (PARTITIONS (…))`: [`Statement::Truncate`] carries a name and
//!   nothing else, so the clause is refused.
//!

use std::mem;

use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::ast::expr::{ColumnRef, Expr, Ident, Literal, ObjectName};
use crate::ast::query::{TableHint, TableRef, Top};
use crate::ast::stmt::{
    AssignOp, AssignTarget, Assignment, DeleteStatement, InsertSource, InsertStatement,
    OutputClause, Statement, UpdateStatement,
};
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::parser::expr::{parse_expr, parse_expr_list, parse_value_expr};
use crate::parser::flow;
use crate::parser::query::{parse_from_clause, parse_select_item, parse_subquery};
use crate::span::Span;
use crate::token::{Op, Punct, Token, TokenKind};

/// Parses an `INSERT` statement, starting on its `INSERT` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # `INTO` is optional on the way in, mandatory on the way out
///
/// `INSERT t VALUES (1)` is legal T-SQL and the AST does not record that `INTO` was left
/// out, so `Display` writes it back: the re-serialisation of `INSERT t VALUES (1)` is
/// `INSERT INTO t VALUES (1)`, which parses back to the same AST. The deviation is
/// deliberate.
///
/// # What the `(` after the target opens
///
/// The column list, or the argument list of a function target when what follows the
/// `(` is a value or a `)` ([`parse_insert_target`]); not a parenthesised
/// query: SQL Server answers `INSERT INTO t (SELECT 1)` with an error on `SELECT`, which
/// is what reading a column list there produces (`tests/dml.rs` `dml_errors`). A
/// parenthesised source is written after the column list, as in `INSERT INTO t (a)
/// (SELECT 1)`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_insert(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Insert)?;
    let top = parse_dml_top(p)?;
    // `INTO` is noise: SQL Server accepts `INSERT t VALUES (1)` just the same.
    let _ = p.eat_keyword(Keyword::Into);
    let target = parse_insert_target(p)?;
    let columns = if p.at_punct(Punct::LeftParen) {
        parse_column_list(p)?
    } else {
        Vec::new()
    };
    let output = parse_optional_output(p)?;
    let source = parse_insert_source(p)?;
    Ok(Statement::Insert(Box::new(InsertStatement {
        target,
        columns,
        source,
        top,
        output,
        span: p.span_from(start),
    })))
}

/// Reads where the rows of an `INSERT` come from.
///
/// Four shapes, told apart by the word they start with: `VALUES`, `DEFAULT VALUES`, an
/// `EXEC`/`EXECUTE` call (V2), and anything else, which is a query. A bare `DEFAULT` that
/// no `VALUES` follows is not a source at all and is reported where it stands.
///
/// # Errors
///
/// The syntax error that stopped the parse of the source.
fn parse_insert_source(p: &mut Parser) -> SqlResult<InsertSource> {
    if p.at_keyword(Keyword::Values) {
        return Ok(InsertSource::Values(parse_values_rows(p)?));
    }
    if p.eat_keyword_seq(&[Keyword::Default, Keyword::Values]) {
        return Ok(InsertSource::DefaultValues);
    }
    if p.at_keyword(Keyword::Exec) || p.at_keyword(Keyword::Execute) {
        // (V2) `INSERT INTO t EXEC dbo.p`. The rule lives in `flow.rs`; whatever error it
        // returns is reported unchanged.
        return match flow::parse_execute(p)? {
            Statement::Execute(execute) => Ok(InsertSource::Execute(execute)),
            other => Err(SqlError::from(InternalError::Bug(format!(
                "flow::parse_execute yielded {other:?}"
            )))),
        };
    }
    Ok(InsertSource::Query(parse_subquery(p)?))
}

/// Reads `VALUES (…), (…)`, the cursor on the `VALUES` keyword.
///
/// SQL Server caps a `VALUES` source at 1000 rows (error 10738). That is a count, not a
/// grammar rule: the parser reads as many rows as are written and the binder is the stage
/// that refuses too many.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the rows.
fn parse_values_rows(p: &mut Parser) -> SqlResult<Vec<Vec<Expr>>> {
    p.expect_keyword(Keyword::Values)?;
    let mut rows = vec![parse_values_row(p)?];
    while p.eat_punct(Punct::Comma) {
        rows.push(parse_values_row(p)?);
    }
    Ok(rows)
}

/// Reads one parenthesised row of a `VALUES` source.
///
/// `DEFAULT` is a legal item here and the expression grammar already reads it as
/// [`Literal::Default`], so nothing special is done for it.
///
/// # Errors
///
/// The syntax error that stopped the parse of the row, a missing `)` included.
fn parse_values_row(p: &mut Parser) -> SqlResult<Vec<Expr>> {
    p.expect_punct(Punct::LeftParen)?;
    let values = parse_expr_list(p)?;
    p.expect_punct(Punct::RightParen)?;
    Ok(values)
}

/// Parses an `UPDATE` statement, starting on its `UPDATE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_update(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Update)?;
    let top = parse_dml_top(p)?;
    let target = parse_dml_target(p)?;
    p.expect_keyword(Keyword::Set)?;
    let mut assignments = vec![parse_assignment(p)?];
    while p.eat_punct(Punct::Comma) {
        assignments.push(parse_assignment(p)?);
    }
    let output = parse_optional_output(p)?;
    let from = parse_source_from(p)?;
    let where_ = parse_optional_where(p)?;
    Ok(Statement::Update(Box::new(UpdateStatement {
        target,
        top,
        assignments,
        from,
        where_,
        output,
        span: p.span_from(start),
    })))
}

/// Reads one item of a `SET` list: a column or a variable, an operator, a value.
///
/// The nine operators of T-SQL are read here: `=` and the eight compound ones
/// (`+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `^=`, `|=`). The right-hand side is a **value**
/// expression, never a predicate, so `SET a = b = 1` stops on the second `=` and is
/// reported there, exactly as `SELECT a = b = 1` is.
///
/// On SQL Server, `SET @x = c = 2` assigns both targets and `SET c = @x = 2` is refused
/// at the second `=`. The eight compound operators are read in the column position too
/// (`SET @x = c += 2`, etc.); the variable's first operator remains `=` in this form.
///
/// # Errors
///
/// The syntax error that stopped the parse of the item.
fn parse_assignment(p: &mut Parser) -> SqlResult<Assignment> {
    let target = if matches!(p.peek().kind, TokenKind::Variable) {
        // `Token::text` of a variable is its whole name, `@` included.
        AssignTarget::Variable(p.advance().text)
    } else {
        AssignTarget::Column(parse_column_ref(p)?)
    };
    let operator_position = p.mark();
    let Some(op) = eat_assign_op(p) else {
        return Err(p.error_here());
    };
    let mut value = parse_value_expr(p)?;
    // `Expr` implements `Drop`, which forbids moving a field out of one by
    // pattern matching (E0509). The column reference is swapped out for a tombstone
    // instead, and the husk is freed on the next line; the form read, the AST built and the
    // error raised are the ones this function produced before that change.
    if matches!(
        (&target, &value),
        (AssignTarget::Variable(_), Expr::Column(_))
    ) && let Some(column_op) = eat_assign_op(p)
    {
        if op != AssignOp::Set {
            p.reset(operator_position);
            return Err(p.error_here());
        }
        let AssignTarget::Variable(variable) = target else {
            unreachable!("the form matched just above carries a variable target")
        };
        let Expr::Column(column) = &mut value else {
            unreachable!("the form matched just above carries a column reference")
        };
        let column = mem::replace(column, tombstone_column_ref());
        return Ok(Assignment {
            target: AssignTarget::VariableAndColumn { variable, column },
            op: column_op,
            value: parse_value_expr(p)?,
        });
    }
    Ok(Assignment { target, op, value })
}

/// A column reference that holds nothing, left in the place of one taken out of an
/// [`Expr::Column`] by [`parse_assignment`].
///
/// `String::new` allocates nothing, so the tombstone is free.
fn tombstone_column_ref() -> ColumnRef {
    ColumnRef {
        qualifier: None,
        name: Ident {
            value: String::new(),
            quoted: false,
        },
        span: Span::EMPTY,
    }
}

/// Reads the assignment operator the cursor sits on, or nothing at all.
fn eat_assign_op(p: &mut Parser) -> Option<AssignOp> {
    let TokenKind::Op(op) = p.peek().kind else {
        return None;
    };
    let assign = match op {
        Op::Eq => AssignOp::Set,
        Op::PlusEq => AssignOp::AddAssign,
        Op::MinusEq => AssignOp::SubAssign,
        Op::StarEq => AssignOp::MulAssign,
        Op::SlashEq => AssignOp::DivAssign,
        Op::PercentEq => AssignOp::ModAssign,
        Op::AmpersandEq => AssignOp::BitAndAssign,
        Op::PipeEq => AssignOp::BitOrAssign,
        Op::CaretEq => AssignOp::BitXorAssign,
        _ => return None,
    };
    p.advance();
    Some(assign)
}

/// Parses a `DELETE` statement, starting on its `DELETE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_delete(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Delete)?;
    let top = parse_dml_top(p)?;
    // The decorative `FROM` of the target; the one of the join sources is read below.
    let _ = p.eat_keyword(Keyword::From);
    let target = parse_dml_target(p)?;
    let output = parse_optional_output(p)?;
    let from = parse_source_from(p)?;
    let where_ = parse_optional_where(p)?;
    Ok(Statement::Delete(Box::new(DeleteStatement {
        target,
        top,
        from,
        where_,
        output,
        span: p.span_from(start),
    })))
}

/// Parses a `TRUNCATE TABLE` statement, starting on its `TRUNCATE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// `TABLE` is **not** optional: SQL Server answers `TRUNCATE t` with a 102 near `'t'`,
/// which is what failing on the word that is not `TABLE` produces here.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_truncate(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Truncate)?;
    p.expect_keyword(Keyword::Table)?;
    let table = p.parse_object_name()?;
    Ok(Statement::Truncate {
        table,
        span: p.span_from(start),
    })
}

/// Parses a `MERGE` statement (V3), starting on its `MERGE` keyword.
///
/// **A stub**, on purpose: `MERGE` is V3 and its grammar is not written. The consequence
/// is client-visible and assumed -- a client sending a `MERGE` gets the internal error
/// 50000 where SQL Server answers a syntax error 102.
///
/// # Errors
///
/// The generic internal error 50000 on each call, until the V3 grammar is written.
pub(crate) fn parse_merge(p: &mut Parser) -> SqlResult<Statement> {
    let _ = p;
    Err(SqlError::from(InternalError::Bug(
        "dml::parse_merge not implemented".into(),
    )))
}

/// Reads the `OUTPUT` clause of a DML statement, when there is one.
///
/// The items are read as a **projection list**: `INSERTED.` and `DELETED.` are ordinary
/// qualifiers, so `INSERTED.id`, `DELETED.*` and `INSERTED.a + 1 AS n` all go through
/// [`parse_select_item`] and need no rule of their own. Whether a qualifier is one of the
/// two magic tables is the binder's business.
///
/// `INTO` names a table, a temporary table or a **table variable**, read by
/// [`parse_output_target`] into a [`TableRef`].
///
/// # Errors
///
/// The syntax error that stopped the parse of the clause.
pub(crate) fn parse_output_clause(p: &mut Parser) -> SqlResult<OutputClause> {
    p.expect_keyword(Keyword::Output)?;
    let mut items = vec![parse_select_item(p)?];
    while p.eat_punct(Punct::Comma) {
        items.push(parse_select_item(p)?);
    }
    let mut into = None;
    let mut into_columns = Vec::new();
    if p.eat_keyword(Keyword::Into) {
        into = Some(parse_output_target(p)?);
        if p.at_punct(Punct::LeftParen) {
            into_columns = parse_column_list(p)?;
        }
    }
    Ok(OutputClause {
        items,
        into,
        into_columns,
    })
}

/// Reads the `OUTPUT … INTO` target: a table variable, or a name of up to four parts.
///
/// Neither an alias, nor a hint list, nor a function follows it. On SQL Server,
/// `INTO @o AS x` and `INTO u AS x` are a 156 near
/// `'AS'`, `INTO @o WITH (TABLOCK)` and `INTO u WITH (TABLOCK)` a 156 near `'WITH'`,
/// `INTO @o.x` a 102 near `'.'`, `INTO f() (a)` a 102 near `')'`. Nothing is read after
/// the name here, so each of those words is reported where it stands; asserted in
/// `tests/dml.rs` `output_into_targets`.
///
/// # Errors
///
/// The syntax error of a token that spells neither.
fn parse_output_target(p: &mut Parser) -> SqlResult<TableRef> {
    let start = p.mark();
    if matches!(p.peek().kind, TokenKind::Variable) {
        // `Token::text` of a variable is its whole name, `@` included.
        let name = p.advance().text;
        return Ok(TableRef::Variable {
            name,
            alias: None,
            span: p.span_from(start),
        });
    }
    let name = p.parse_object_name()?;
    Ok(TableRef::Table {
        name,
        alias: None,
        hints: Vec::new(),
        span: p.span_from(start),
    })
}

/// Reads the `OUTPUT` clause when the cursor sits on its keyword.
///
/// # Errors
///
/// The syntax error that stopped the parse of the clause.
fn parse_optional_output(p: &mut Parser) -> SqlResult<Option<OutputClause>> {
    if !p.at_keyword(Keyword::Output) {
        return Ok(None);
    }
    Ok(Some(parse_output_clause(p)?))
}

/// Reads the `FROM` of the join sources of an `UPDATE` or a `DELETE`, when there is one.
///
/// The keyword is consumed here and its contents by `parser/from.rs`, through the same
/// seam `SELECT` uses, so that `UPDATE t SET a = 1 FROM;` reports on the `;`.
///
/// # Errors
///
/// The syntax error that stopped the parse of the clause.
fn parse_source_from(p: &mut Parser) -> SqlResult<Vec<TableRef>> {
    if !p.eat_keyword(Keyword::From) {
        return Ok(Vec::new());
    }
    parse_from_clause(p)
}

/// Reads the `WHERE` of a DML statement, when there is one.
///
/// # Errors
///
/// The syntax error that stopped the parse of the predicate.
fn parse_optional_where(p: &mut Parser) -> SqlResult<Option<Expr>> {
    if !p.eat_keyword(Keyword::Where) {
        return Ok(None);
    }
    Ok(Some(parse_expr(p)?))
}

/// Reads the target of an `UPDATE` or a `DELETE`: a name or a table variable, and its
/// hints.
///
/// A [`TableRef`] is built rather than an [`ObjectName`] because the target may be an
/// **alias** declared in the `FROM` of the join sources (`UPDATE a SET … FROM t AS a`),
/// which the binder resolves against that clause. It is never a join tree nor a derived
/// table, and it carries **no alias of its own**: SQL Server answers `UPDATE t AS a SET a
/// = 1` with a 156 near `'AS'` and `UPDATE t a SET a = 1` with a 102 near `'a'`, so no
/// alias is read here and the word that follows the target is reported where it stands.
///
/// # Errors
///
/// The syntax error that stopped the parse of the target.
fn parse_dml_target(p: &mut Parser) -> SqlResult<TableRef> {
    let start = p.mark();
    if matches!(p.peek().kind, TokenKind::Variable) {
        // `Token::text` of a variable is its whole name, `@` included.
        let name = p.advance().text;
        return Ok(TableRef::Variable {
            name,
            alias: None,
            span: p.span_from(start),
        });
    }
    let name = p.parse_object_name()?;
    let hints = parse_with_hints(p)?;
    Ok(TableRef::Table {
        name,
        alias: None,
        hints,
        span: p.span_from(start),
    })
}

/// Reads the target of an `INSERT`: what [`parse_dml_target`] reads, plus a name given
/// parentheses, `f()` or `dbo.f(1, @x)`, which is a [`TableRef::Function`].
///
/// # The `(` after the name: arguments or column list
///
/// One batch per shape, asserted in `tests/dml.rs` `insert_function_target`. The list is the
/// **argument list** when it is empty or when its first item is a value -- a literal,
/// a negative number, a string, `NULL`, `DEFAULT`, a local variable: `f()`, `f(1)`,
/// `f(-1)`, `f(-1.5)`, `f('a')`, `f(N'a')`, `f(1.5)`, `f(0x01)`, `f($1)`, `f(NULL)`,
/// `f(DEFAULT)`, `f(@x)` and `f(1, 'a')` are accepted, each followed by an optional
/// column list. It is the **column list** when its first item is an identifier or a
/// global variable: `f(a) (c)`, `f([a]) (c)`, `f(x.a) (c)` and `f(a, b) (c)` are a 102
/// near `'c'`, `f(a, 1)` a 102 near `'1'`, `f(@@ROWCOUNT) (c)` a 102 near `'@@ROWCOUNT'`.
///
/// An argument is one value and nothing more: `f(1 + 1)` is a 102 near `'+'`,
/// `f(@x + 1)` a 102 near `'+'`, `f(-1 + 1)` a 102 near `'+'`, `f((1))` and
/// `f((SELECT 1))` a 102 near `'('`, `f(*)` a 102 near `'*'`, and a value followed by
/// an identifier (`f(1, a)`, `f(@x, a)`) a 102 near `'a'`. A `+` opens no value
/// (`f(+1)` is a 102 near `'+'`), and a `-` is followed by a number and nothing else:
/// `f(-a)` is a 102 near `'a'`, `f(-@x)` a 102 near `'@x'`. No hint follows the
/// parentheses (`f() WITH (TABLOCK)` and `f(1) WITH (TABLOCK)` are a 156 near `'WITH'`),
/// which is what [`TableRef::Function`] says already.
///
/// # Errors
///
/// The syntax error that stopped the parse of the target.
fn parse_insert_target(p: &mut Parser) -> SqlResult<TableRef> {
    let start = p.mark();
    if matches!(p.peek().kind, TokenKind::Variable) {
        return parse_dml_target(p);
    }
    let name = p.parse_object_name()?;
    if at_function_args(p) {
        let args = parse_function_args(p)?;
        return Ok(TableRef::Function {
            name,
            args,
            alias: None,
            span: p.span_from(start),
        });
    }
    let hints = parse_with_hints(p)?;
    Ok(TableRef::Table {
        name,
        alias: None,
        hints,
        span: p.span_from(start),
    })
}

/// Whether the cursor sits on a `(` that opens the argument list of a function target
/// rather than the column list of the `INSERT`: `()`, or `(` followed by a value.
///
/// Anything else after the `(` -- an identifier, a reserved word, a nested `(`, a `*` --
/// leaves the list to the column-list reader, which reports it where it stands.
fn at_function_args(p: &Parser) -> bool {
    if !p.at_punct(Punct::LeftParen) {
        return false;
    }
    let next = p.peek_at(1);
    matches!(next.kind, TokenKind::Punct(Punct::RightParen)) || at_value(next)
}

/// Whether `token` opens one value of a function-target argument.
///
/// A `Variable` token is a value when it is a local one: `@@ROWCOUNT` and the other
/// global ones are refused by SQL Server where they stand (`tests/dml.rs`
/// `insert_function_target`).
fn at_value(token: &Token) -> bool {
    match token.kind {
        TokenKind::Integer
        | TokenKind::Decimal
        | TokenKind::Float
        | TokenKind::Money
        | TokenKind::Binary(_)
        | TokenKind::Str { .. }
        | TokenKind::Keyword(Keyword::Null)
        | TokenKind::Keyword(Keyword::Default)
        | TokenKind::Op(Op::Minus) => true,
        TokenKind::Variable => !token.text.starts_with("@@"),
        _ => false,
    }
}

/// Whether `kind` is a number, the one thing a `-` may precede in a function-target
/// argument.
fn at_number(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Integer | TokenKind::Decimal | TokenKind::Float
    )
}

/// Reads the parenthesised argument list of a function target, the cursor on the `(`.
///
/// # Errors
///
/// The syntax error of an item that is not one value, or of a missing `)`.
fn parse_function_args(p: &mut Parser) -> SqlResult<Vec<Expr>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut args = Vec::new();
    if !p.at_punct(Punct::RightParen) {
        args.push(parse_function_arg(p)?);
        while p.eat_punct(Punct::Comma) {
            args.push(parse_function_arg(p)?);
        }
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(args)
}

/// Reads one argument of a function target: a literal, a local variable, or a negative
/// number, through the expression grammar, then checks that nothing more was read.
///
/// The expression grammar is what turns the token into the [`Expr`] the AST expects (a
/// string deredoubled, a money literal without its `$`), and it is the same grammar that
/// would swallow `1 + 1`; so the shape of what it read is checked afterwards, and when
/// it is more than one value the cursor is put back on the token that follows that value
/// (`+` in `f(1 + 1)`), which is where SQL Server reports. A `-` that no number follows
/// is reported on that following token (`a` in `f(-a)`, `)` in `f(-)`), before the
/// expression grammar gets to read it.
///
/// # Errors
///
/// The syntax error of the token that is not a value, or of the token that follows one.
fn parse_function_arg(p: &mut Parser) -> SqlResult<Expr> {
    let start = p.mark();
    if !at_value(p.peek()) {
        return Err(p.error_here());
    }
    let signed = matches!(p.peek().kind, TokenKind::Op(Op::Minus));
    if signed {
        p.advance();
        if !at_number(&p.peek().kind) {
            return Err(p.error_here());
        }
        p.reset(start);
    }
    let expr = parse_value_expr(p)?;
    let one_value = match &expr {
        Expr::Literal(..) | Expr::Variable { .. } => true,
        Expr::Unary { expr, .. } => matches!(**expr, Expr::Literal(..)),
        _ => false,
    };
    if one_value {
        return Ok(expr);
    }
    // More than one value was read: report on the token after the first value.
    p.reset(start);
    p.advance();
    if signed {
        p.advance();
    }
    Err(p.error_here())
}

/// Reads the `WITH (hint, …)` of a DML target, when there is one.
///
/// Only the `WITH` spelling is read. The older one without it, which `parser/from.rs`
/// accepts in a `FROM`, cannot be used here: after the target of an `INSERT` a `(` opens
/// the **column list**, and there is nothing to tell the two apart.
///
/// Nothing is validated -- an unknown hint, an argument that means nothing, two hints
/// that contradict each other are kept as written -- and the engine ignores them.
///
/// # Errors
///
/// The syntax error that stopped the parse of the list.
fn parse_with_hints(p: &mut Parser) -> SqlResult<Vec<TableHint>> {
    if !(p.at_keyword(Keyword::With)
        && matches!(p.peek_at(1).kind, TokenKind::Punct(Punct::LeftParen)))
    {
        return Ok(Vec::new());
    }
    p.advance();
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
/// The word is taken from the **token text** and not through `Parser::parse_ident`: half
/// the hint names of T-SQL are reserved words (`INDEX`, `HOLDLOCK`, `READCOMMITTED`), and
/// a hint name is not an identifier the binder will ever resolve.
///
/// # Errors
///
/// The syntax error of a token that spells no hint name or no argument.
fn parse_hint(p: &mut Parser) -> SqlResult<TableHint> {
    let start = p.mark();
    if !at_hint_word(p) {
        return Err(p.error_here());
    }
    let name = p.advance().text;
    let mut args = Vec::new();
    if p.eat_punct(Punct::LeftParen) {
        loop {
            if !(at_hint_word(p) || matches!(p.peek().kind, TokenKind::Integer)) {
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

/// Whether the cursor sits on something that spells a hint name: a bare identifier or any
/// keyword, reserved or not.
fn at_hint_word(p: &Parser) -> bool {
    matches!(
        &p.peek().kind,
        TokenKind::Ident { quoted: false, .. } | TokenKind::Keyword(_)
    )
}

/// Reads a parenthesised list of column names: the one of an `INSERT` target, and the one
/// of an `OUTPUT … INTO` target.
///
/// # Errors
///
/// The syntax error of a list item that is not a name, or of a missing `)`.
fn parse_column_list(p: &mut Parser) -> SqlResult<Vec<Ident>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut columns = vec![p.parse_ident()?];
    while p.eat_punct(Punct::Comma) {
        columns.push(p.parse_ident()?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(columns)
}

/// Reads a column reference in assignment position, `c` or `t.c` or `db.dbo.t.c`.
///
/// The same shape [`crate::parser::expr`] reads in an expression, restricted to what an
/// assignment target may be: `parse_object_name` fills the parts from the right, and the
/// last one is the column.
///
/// # Errors
///
/// The syntax error of a token that does not spell a name. The cursor is left where it
/// started.
fn parse_column_ref(p: &mut Parser) -> SqlResult<ColumnRef> {
    let start = p.mark();
    let name = p.parse_object_name()?;
    let column = name.name;
    let qualifier = match (name.server, name.database, name.schema) {
        (None, None, None) => None,
        (server, database, Some(table)) => Some(ObjectName {
            server: None,
            database: server,
            schema: database,
            name: table,
            span: p.span_from(start),
        }),
        _ => {
            p.reset(start);
            return Err(p.error_here());
        }
    };
    Ok(ColumnRef {
        qualifier,
        name: column,
        span: p.span_from(start),
    })
}

/// Reads the `TOP` clause of a DML statement, when there is one.
///
/// `TOP (expression) [PERCENT]`, and nothing else: `WITH TIES` belongs to `SELECT`, and
/// leaving its two words unread is what turns `DELETE TOP (1) WITH TIES FROM t` into the
/// error SQL Server reports on `WITH`.
///
/// The parentheses are **required** by SQL Server in a DML statement and are optional
/// here, on purpose: that check is left to the binder, which is the stage that
/// sees the whole statement. Without them only a numeric literal is read, as in a
/// `SELECT`, so that the `*` of a following expression is never eaten as a
/// multiplication.
///
/// The rule is written here rather than shared with `parser/query.rs`, whose `parse_top`
/// is private to that file; the two differ anyway, by `WITH TIES`.
///
/// # Errors
///
/// The syntax error that stopped the parse of the row count.
fn parse_dml_top(p: &mut Parser) -> SqlResult<Option<Top>> {
    let start = p.mark();
    if !p.eat_keyword(Keyword::Top) {
        return Ok(None);
    }
    let parenthesized = p.at_punct(Punct::LeftParen);
    let expr = if parenthesized {
        p.expect_punct(Punct::LeftParen)?;
        // The parentheses belong to the clause, not to the expression: an `Expr::Nested`
        // here would print `TOP ((5))` on the way back.
        let expr = parse_value_expr(p)?;
        p.expect_punct(Punct::RightParen)?;
        expr
    } else {
        parse_number(p)?
    };
    let percent = p.eat_keyword(Keyword::Percent);
    Ok(Some(Top {
        expr,
        percent,
        with_ties: false,
        parenthesized,
        span: p.span_from(start),
    }))
}

/// Reads one numeric literal, the only thing an unparenthesised `TOP` accepts.
///
/// Word for word the `parse_number` of `parser/query.rs`, and deliberately so, for the
/// reason given on [`parse_dml_top`] just above: that one is private to its file. Six
/// duplicated lines.
///
/// # Errors
///
/// The syntax error of the token that is not a number.
fn parse_number(p: &mut Parser) -> SqlResult<Expr> {
    let literal = match p.peek().kind {
        TokenKind::Integer => Literal::Integer(p.peek().text.clone()),
        TokenKind::Decimal => Literal::Decimal(p.peek().text.clone()),
        TokenKind::Float => Literal::Float(p.peek().text.clone()),
        _ => return Err(p.error_here()),
    };
    let span = p.advance().span;
    Ok(Expr::Literal(literal, span))
}
