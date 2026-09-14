//! `SELECT` and its clauses.
//!
//! A `SELECT` is read down to the end of its select list, then what wraps it: `INTO`,
//! `WHERE`, `GROUP BY`, `HAVING`, the `ORDER BY` and the `OFFSET`/`FETCH` of
//! the whole query, and the set operators. The contents of a `FROM` are read by
//! `parser/from.rs`; the keyword `FROM` itself is consumed here, so that the error of
//! `SELECT * FROM;` falls on the `;` as SQL Server reports it.
//!
//! # The order of the clauses is not negotiable
//!
//! `SELECT [DISTINCT] [TOP …] <list> [INTO t] [FROM …] [WHERE …] [GROUP BY …]
//! [HAVING …]`, then, on the **whole** query and not on one of its specifications,
//! `[ORDER BY …] [OFFSET … FETCH …]`. Any other order is a syntax error, exactly as in
//! SQL Server: a clause written out of turn is simply not recognised where it stands.
//!
//! # Precedence of the set operators
//!
//! `INTERSECT` binds tighter than `UNION` and `EXCEPT`, which share one rank and
//! associate to the **left**. Hence two loops, [`parse_query_body`] over `UNION`/`EXCEPT`
//! and [`parse_intersect_body`] over `INTERSECT`, the same shape as the two loops of the
//! expression grammar. Parentheses written around a query are kept as a
//! [`QueryBody::Nested`], so that `(SELECT 1) UNION (SELECT 2)` re-serialises with them.
//!
//! # Where a rule stops
//!
//! The `;` is optional in T-SQL, so `SELECT 1 SELECT 2` is a batch of **two** statements.
//! Every rule here therefore stops without consuming the token it does not recognise, and
//! it is `parse_batch` that decides whether that token opens another statement or is a
//! syntax error.
//!
//! # Re-serialisation deviation
//!
//! `SELECT ALL 1` is read as `SELECT 1`: `ALL` is the default of a select list and
//! [`QuerySpec::distinct`] only records `DISTINCT`, so `Display` never writes `ALL` back.
//! The AST of both spellings is the same one, which is what the `parse` -> `Display` ->
//! `parse` loop needs; the deviation is deliberate.
//!
//! The grammar follows the T-SQL reference of `SELECT` and its clauses.

use std::mem;

use vauban_errors::SqlResult;

use crate::ast::expr::{Expr, Ident, Literal, ObjectName};
use crate::ast::query::{
    AliasStyle, OffsetFetch, OrderItem, QueryBody, QuerySpec, SelectItem, SelectStatement, SetOp,
    TableRef, Top,
};
use crate::ast::stmt::Statement;
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::parser::expr::{parse_expr, parse_expr_list, parse_value_expr};
use crate::parser::from;
use crate::span::Span;
use crate::token::{Op, Punct, TokenKind};

/// Parses a `SELECT` statement, starting on its first token.
///
/// The head token is still unconsumed: `SELECT`, `WITH` (a CTE, V2) or `(` (a
/// parenthesised query such as `(SELECT 1) UNION SELECT 2`). The function reads it back
/// itself, because it needs it for the statement span.
///
/// # Errors
///
/// The syntax error that stopped the parse. A `WITH` is one: common table expressions
/// are V2, the AST declares [`SelectStatement::with`] but no rule fills it yet, and
/// `WITH` is a reserved word, so a 156 is reported on it.
pub(crate) fn parse_select_statement(p: &mut Parser) -> SqlResult<Statement> {
    Ok(Statement::Select(Box::new(parse_select(p)?)))
}

/// Parses a query in **expression** position -- a derived table, a subquery of a `WHERE`,
/// a scalar subquery -- and hands it back as the statement node the AST uses everywhere.
///
/// # Errors
///
/// The syntax error that stopped the parse of the query.
pub(crate) fn parse_subquery(p: &mut Parser) -> SqlResult<Box<SelectStatement>> {
    Ok(Box::new(parse_select(p)?))
}

/// Reads one whole query: its body, then the clauses that apply to the body as a whole.
///
/// `ORDER BY` and `OFFSET`/`FETCH` are read **here** and not by [`parse_query_spec`],
/// because they act on the query and not on one of its specifications: in `SELECT 1 UNION
/// SELECT 2 ORDER BY 1`, the sort applies to the union.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_select(p: &mut Parser) -> SqlResult<SelectStatement> {
    let start = p.mark();
    if p.at_keyword(Keyword::With) {
        return Err(p.error_here());
    }
    let body = parse_query_body(p)?;
    let order_by = parse_order_by(p)?;
    // T-SQL only allows `OFFSET` after an `ORDER BY`, so without one the word is left
    // where it is and the caller reports it.
    let offset_fetch = if order_by.is_empty() {
        None
    } else {
        parse_offset_fetch(p)?
    };
    Ok(SelectStatement {
        with: None,
        body,
        order_by,
        offset_fetch,
        for_clause: None,
        span: p.span_from(start),
    })
}

/// Reads a query body and the `UNION`/`EXCEPT` operators that join several of them.
///
/// The loop is what makes the two operators associate to the **left**, and calling
/// [`parse_intersect_body`] for each operand is what makes `INTERSECT` bind tighter.
/// `ALL` is only read after `UNION`: T-SQL has neither `EXCEPT ALL` nor `INTERSECT ALL`,
/// and an `ALL` written there is reported where it stands.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the operands.
fn parse_query_body(p: &mut Parser) -> SqlResult<QueryBody> {
    // The one recursive door of the query grammar: a scalar subquery, a derived table and
    // a parenthesised body come back here; the levels are counted there.
    p.nested(parse_query_body_inner)
}

/// The body of [`parse_query_body`], one level deeper on the nesting counter.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the operands.
fn parse_query_body_inner(p: &mut Parser) -> SqlResult<QueryBody> {
    let start = p.mark();
    let mut left = parse_intersect_body(p)?;
    loop {
        let op = if p.at_keyword(Keyword::Union) {
            SetOp::Union
        } else if p.at_keyword(Keyword::Except) {
            SetOp::Except
        } else {
            break;
        };
        p.advance();
        let all = op == SetOp::Union && p.eat_keyword(Keyword::All);
        let right = parse_intersect_body(p)?;
        left = QueryBody::SetOp {
            op,
            all,
            left: Box::new(left),
            right: Box::new(right),
            span: p.span_from(start),
        };
    }
    Ok(left)
}

/// Reads the `INTERSECT` level, the tighter of the two.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the operands.
fn parse_intersect_body(p: &mut Parser) -> SqlResult<QueryBody> {
    let start = p.mark();
    let mut left = parse_query_term(p)?;
    while p.eat_keyword(Keyword::Intersect) {
        let right = parse_query_term(p)?;
        left = QueryBody::SetOp {
            op: SetOp::Intersect,
            all: false,
            left: Box::new(left),
            right: Box::new(right),
            span: p.span_from(start),
        };
    }
    Ok(left)
}

/// Reads one operand of a set operator: a specification, or a parenthesised body.
///
/// The parentheses are kept as a [`QueryBody::Nested`], since `Display` neither adds nor
/// removes one and since they may change what the operators mean: `(SELECT 1 UNION SELECT
/// 2) INTERSECT SELECT 3` is not `SELECT 1 UNION SELECT 2 INTERSECT SELECT 3`.
///
/// # Errors
///
/// The syntax error that stopped the parse, a missing `)` included.
fn parse_query_term(p: &mut Parser) -> SqlResult<QueryBody> {
    if p.at_punct(Punct::LeftParen) {
        let start = p.mark();
        p.advance();
        let body = parse_query_body(p)?;
        p.expect_punct(Punct::RightParen)?;
        return Ok(QueryBody::Nested(Box::new(body), p.span_from(start)));
    }
    Ok(QueryBody::Select(Box::new(parse_query_spec(p)?)))
}

/// Parses one `SELECT` specification: from the `SELECT` keyword down to the `HAVING`.
///
/// The function stops on the first word it does not own -- `ORDER`, `UNION`, `EXCEPT`,
/// `INTERSECT`, a `)` -- because those belong to the query as a whole and are read by
/// [`parse_select`].
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_query_spec(p: &mut Parser) -> SqlResult<QuerySpec> {
    let start = p.mark();
    p.expect_keyword(Keyword::Select)?;
    // `ALL` is the default and the AST does not record it: see the module deviation.
    let distinct = !p.eat_keyword(Keyword::All) && p.eat_keyword(Keyword::Distinct);
    let top = parse_top(p)?;
    let mut items = vec![parse_select_item(p)?];
    while p.eat_punct(Punct::Comma) {
        items.push(parse_select_item(p)?);
    }
    // `SELECT … INTO t FROM …` creates the table `t`. This is the only place `INTO`
    // stands in a `SELECT`; the `INTO` of an `INSERT` and the one of an `OUTPUT` clause
    // are read by their own rules (`dml.rs`).
    let into = if p.eat_keyword(Keyword::Into) {
        Some(p.parse_object_name()?)
    } else {
        None
    };
    // The `FROM` keyword is read here and its contents by `parser/from.rs`, so that the
    // error of `SELECT * FROM;` falls on the `;`, as SQL Server reports it.
    let from = if p.eat_keyword(Keyword::From) {
        parse_from_clause(p)?
    } else {
        Vec::new()
    };
    // `WHERE` needs no `FROM`: `SELECT 1 WHERE 1 = 1` is a legal query that returns one
    // row, or none.
    let where_ = if p.eat_keyword(Keyword::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };
    let group_by = if p.eat_keyword_seq(&[Keyword::Group, Keyword::By]) {
        parse_expr_list(p)?
    } else {
        Vec::new()
    };
    // A `HAVING` without a `GROUP BY` is legal T-SQL, and whether the grouping makes
    // sense at all is the binder's business, never the grammar's.
    let having = if p.eat_keyword(Keyword::Having) {
        Some(parse_expr(p)?)
    } else {
        None
    };
    Ok(QuerySpec {
        distinct,
        top,
        items,
        into,
        from,
        where_,
        group_by,
        having,
        span: p.span_from(start),
    })
}

/// Reads the table references of a `FROM` clause, the keyword itself already consumed.
///
/// The rule lives in `parser/from.rs`; this function is the seam between the two files
/// and its signature is what `parse_query_spec` calls.
///
/// # Errors
///
/// The syntax error that stopped the parse of the clause.
pub(crate) fn parse_from_clause(p: &mut Parser) -> SqlResult<Vec<TableRef>> {
    from::parse_from(p)
}

/// Reads a `TOP` clause when there is one.
///
/// `TOP (expression) [PERCENT] [WITH TIES]`. The
/// parentheses are mandatory in an `INSERT`, `UPDATE` or `DELETE` and optional in a
/// `SELECT`, for backward compatibility; [`Top::parenthesized`] records which form was
/// written so that `Display` restores it.
///
/// Without parentheses, T-SQL admits a **constant** and nothing else, and that restriction is
/// enforced here rather than left to the binder: the select list of `SELECT TOP 5 * FROM
/// t` starts with a `*`, which a full expression would eat as a multiplication. With
/// parentheses, any expression is read, `TOP (@n)` included.
///
/// `WITH TIES` without an `ORDER BY` is the error 1033 of SQL Server; the parser accepts
/// it and leaves that check to the binder, which is the only stage that sees the whole
/// statement.
///
/// # Errors
///
/// The syntax error that stopped the parse of the row count.
fn parse_top(p: &mut Parser) -> SqlResult<Option<Top>> {
    let start = p.mark();
    if !p.eat_keyword(Keyword::Top) {
        return Ok(None);
    }
    let parenthesized = p.at_punct(Punct::LeftParen);
    let expr = if parenthesized {
        p.expect_punct(Punct::LeftParen)?;
        // The parentheses belong to the clause, not to the expression: an `Expr::Nested`
        // here would print `TOP ((5))` on the way back.
        let mut expr = parse_value_expr(p)?;
        super::expr::enclose_niladic_diagnostics(&mut expr);
        p.expect_punct(Punct::RightParen)?;
        expr
    } else {
        parse_number(p)?
    };
    let percent = p.eat_keyword(Keyword::Percent);
    let with_ties = p.eat_keyword_seq(&[Keyword::With, Keyword::Ties]);
    Ok(Some(Top {
        expr,
        percent,
        with_ties,
        parenthesized,
        span: p.span_from(start),
    }))
}

/// Reads one numeric literal, the only thing an unparenthesised `TOP` accepts.
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

/// Reads one item of a select list.
///
/// The decisions are taken in this order: `*`, then `t.*`, then `alias = expression`,
/// then an assignment, then an expression and its optional alias. What tells the two
/// forms of `x = 1` apart is the kind of the token on the left: a name or a character
/// string is an **alias** (`SELECT n = 1`, `SELECT 'my label' = 1`, both accepted by SQL
/// Server), a variable is an **assignment** and yields an [`Expr::Assign`].
///
/// An assignment takes **no** alias: SQL Server answers a 102 near `'n'` to `SELECT @x =
/// 1 n`, so the word is left where it is and the caller reports it.
///
/// Mixing assignments and ordinary items in one select list (`SELECT @x = 1, 2`) is
/// accepted here: SQL Server refuses it with the error 141, which needs the whole list
/// and belongs to the binder, not to the grammar.
///
/// # Errors
///
/// The syntax error that stopped the parse of the item.
pub(crate) fn parse_select_item(p: &mut Parser) -> SqlResult<SelectItem> {
    if at_star(p, 0) {
        let span = p.advance().span;
        return Ok(SelectItem::Wildcard(span));
    }
    if at_qualified_wildcard(p) {
        return parse_qualified_wildcard(p);
    }
    if at_alias_before_equals(p) {
        // `at_alias_before_equals` is exactly the case `try_alias_name` accepts.
        let alias = try_alias_name(p);
        p.advance();
        return Ok(SelectItem::Expr {
            expr: parse_value_expr(p)?,
            alias,
            alias_style: AliasStyle::Equals,
        });
    }
    if matches!(p.peek().kind, TokenKind::Variable) && at_assignment_sign(p, 1) {
        return Ok(SelectItem::Expr {
            expr: parse_assignment(p)?,
            alias: None,
            alias_style: AliasStyle::Bare,
        });
    }
    let expr = parse_value_expr(p)?;
    let (alias, alias_style) = parse_alias(p)?;
    Ok(SelectItem::Expr {
        expr,
        alias,
        alias_style,
    })
}

/// Whether the item starts with the alias of an `alias = expression`.
///
/// The alias may be a name or a character string, the two spellings SQL Server treats as
/// an identifier here; a variable on the left of an `=` is an assignment instead.
fn at_alias_before_equals(p: &Parser) -> bool {
    let named = at_name(p, 0) || matches!(p.peek().kind, TokenKind::Str { .. });
    named && at_assignment_sign(p, 1)
}

/// Reads `@x = expression`, the form that makes a `SELECT` assign a variable.
///
/// # Errors
///
/// The syntax error that stopped the parse of the assigned expression.
fn parse_assignment(p: &mut Parser) -> SqlResult<Expr> {
    let start = p.mark();
    // `Token::text` of a variable is its whole name, `@` included, which is what
    // `Expr::Assign` and `Display` both want.
    let target = p.advance().text;
    p.advance();
    let value = Box::new(parse_value_expr(p)?);
    Ok(Expr::Assign {
        target,
        value,
        span: p.span_from(start),
    })
}

/// Reads the alias that follows an expression, with or without `AS`, when there is one.
///
/// A bare alias (`SELECT 1 n`) is told from the next statement of the batch by the kind
/// of the token: an identifier or a non-reserved word is an alias, a reserved word never
/// is, so `SELECT 1 SELECT 2` stops on the second `SELECT`. When no alias is written the
/// style is [`AliasStyle::Bare`], which `Display` never reads since there is no name to
/// write.
///
/// # Errors
///
/// The syntax error of the token that follows an `AS` and spells no alias.
fn parse_alias(p: &mut Parser) -> SqlResult<(Option<Ident>, AliasStyle)> {
    if p.eat_keyword(Keyword::As) {
        return match try_alias_name(p) {
            Some(alias) => Ok((Some(alias), AliasStyle::As)),
            None => Err(p.error_here()),
        };
    }
    Ok(match try_alias_name(p) {
        Some(alias) => (Some(alias), AliasStyle::Bare),
        None => (None, AliasStyle::Bare),
    })
}

/// Reads the name of an alias, when the cursor sits on one.
///
/// An alias may be written as a **character string** (`SELECT 1 'n'`, `SELECT 1 AS 'n'`),
/// which SQL Server accepts and treats as a delimited identifier. The AST only records
/// that the name was quoted, so `Display` writes it back between brackets: the
/// re-serialisation of `SELECT 1 'n'` is `SELECT 1 [n]`, a deliberate deviation.
fn try_alias_name(p: &mut Parser) -> Option<Ident> {
    if let TokenKind::Str { value, .. } = &p.peek().kind {
        let alias = Ident {
            value: value.clone(),
            quoted: true,
        };
        p.advance();
        return Some(alias);
    }
    if !at_name(p, 0) {
        return None;
    }
    // `at_name` is exactly the case `parse_ident` accepts, so the error is unreachable.
    p.parse_ident().ok()
}

/// Reads `t.*`, `dbo.t.*` or `db..t.*`, the cursor sitting on the first part.
///
/// The parts fill from the right, as [`Parser::parse_object_name`] fills them, and an
/// empty part yields `None`. The span of the name covers the `.*` too, which no message
/// prints and no comparison sees.
///
/// # Errors
///
/// The syntax error of a fifth part, or of a qualifier whose last part is empty (`a..*`
/// names no table).
fn parse_qualified_wildcard(p: &mut Parser) -> SqlResult<SelectItem> {
    let start = p.mark();
    let mut parts: Vec<Option<Ident>> = vec![Some(p.parse_ident()?)];
    loop {
        p.expect_punct(Punct::Dot)?;
        if at_star(p, 0) {
            if parts.last().is_some_and(Option::is_none) {
                return Err(p.error_here());
            }
            p.advance();
            break;
        }
        if parts.len() == 4 {
            // The token that would open a fifth part is what SQL Server reports on.
            return Err(p.error_here());
        }
        if p.at_punct(Punct::Dot) {
            parts.push(None);
            continue;
        }
        parts.push(Some(p.parse_ident()?));
    }
    let name = match parts.pop() {
        Some(Some(name)) => name,
        // The loop refuses to stop on an empty part, so there is always a name here.
        _ => return Err(p.error_here()),
    };
    let schema = parts.pop().flatten();
    let database = parts.pop().flatten();
    let server = parts.pop().flatten();
    Ok(SelectItem::QualifiedWildcard(ObjectName {
        server,
        database,
        schema,
        name,
        span: p.span_from(start),
    }))
}

/// Reads the `ORDER BY` of a query, when there is one.
///
/// The clause sorts the **whole** query, so it hangs from [`SelectStatement`] and not
/// from a [`QuerySpec`]: in `SELECT 1 UNION SELECT 2 ORDER BY 1` it applies to the union,
/// and a specification stops before it.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the items.
fn parse_order_by(p: &mut Parser) -> SqlResult<Vec<OrderItem>> {
    if !p.eat_keyword_seq(&[Keyword::Order, Keyword::By]) {
        return Ok(Vec::new());
    }
    let mut items = vec![parse_order_item(p)?];
    while p.eat_punct(Punct::Comma) {
        items.push(parse_order_item(p)?);
    }
    Ok(items)
}

/// Reads one item of an `ORDER BY`: an expression, its collation and its direction.
///
/// The same rule serves the `ORDER BY` of a query and the one of an `OVER` clause
/// (`parser/expr.rs`), which are the same grammar.
///
/// `ORDER BY 1` sorts by the **position** of a column, and the AST records it as the
/// integer literal it is written as: turning a position into a column is the binder's
/// business, not the grammar's.
///
/// # `COLLATE` is read twice over
///
/// `COLLATE` is a suffix operator of the expression grammar, so `a COLLATE c` comes back
/// from [`parse_value_expr`] as an [`Expr::Collate`]. A collation written at the top of a
/// sort item is moved into [`OrderItem::collate`], the field the AST declares for it, so
/// that the binder finds it where the clause puts it; `Display` writes it back between
/// the expression and the direction, which is where T-SQL spells it. A collation that is
/// not at the top (`a COLLATE c + b`) belongs to the expression and stays there.
///
/// # Errors
///
/// The syntax error that stopped the parse of the expression.
pub(crate) fn parse_order_item(p: &mut Parser) -> SqlResult<OrderItem> {
    // `Expr` implements `Drop`, which forbids moving a field out of one by
    // pattern matching (E0509). The operand and the collation of a top-level `COLLATE` are
    // swapped out for tombstones instead, and the husk is freed on the line that reads
    // `unwrap_or`; the item built is the one this function produced before that change.
    let mut value = parse_value_expr(p)?;
    let mut collate = None;
    let mut collated_operand = None;
    if let Expr::Collate {
        expr, collation, ..
    } = &mut value
    {
        collate = Some(mem::take(collation));
        collated_operand = Some(mem::replace(&mut **expr, Expr::Placeholder(Span::EMPTY)));
    }
    let expr = collated_operand.unwrap_or(value);
    let desc = p.at_keyword(Keyword::Desc);
    let explicit_direction = p.eat_keyword(Keyword::Desc) || p.eat_keyword(Keyword::Asc);
    Ok(OrderItem {
        expr,
        desc,
        explicit_direction,
        collate,
    })
}

/// (V3) Reads `OFFSET n ROWS [FETCH NEXT m ROWS ONLY]`, when it is there.
///
/// `ROW` and `ROWS` are
/// interchangeable, and so are `FIRST` and `NEXT`. [`OffsetFetch::rows_singular`] and
/// [`OffsetFetch::fetch_first`] record which one was written, so that the clause
/// re-serialises in the form it was read in. One flag serves both halves of the clause:
/// `OFFSET 1 ROWS FETCH NEXT 1 ROW ONLY` is written back with `ROWS` on both sides, and
/// the AST of what is written back is the same one.
///
/// # Errors
///
/// The syntax error that stopped the parse of the clause.
fn parse_offset_fetch(p: &mut Parser) -> SqlResult<Option<OffsetFetch>> {
    if !p.eat_keyword(Keyword::Offset) {
        return Ok(None);
    }
    let offset = parse_value_expr(p)?;
    let rows_singular = eat_rows(p)?;
    let mut fetch = None;
    let mut fetch_first = false;
    if p.eat_keyword(Keyword::Fetch) {
        fetch_first = p.eat_keyword(Keyword::First);
        if !fetch_first {
            p.expect_keyword(Keyword::Next)?;
        }
        fetch = Some(parse_value_expr(p)?);
        // The plural of the `FETCH` is read and dropped: the AST keeps one flag.
        eat_rows(p)?;
        p.expect_keyword(Keyword::Only)?;
    }
    Ok(Some(OffsetFetch {
        offset,
        fetch,
        fetch_first,
        rows_singular,
    }))
}

/// Reads the `ROW` or `ROWS` of an `OFFSET`/`FETCH`, and says whether it was the singular.
///
/// `ROWS` is a keyword of the lexer; `ROW` is not a T-SQL keyword at all, so it arrives
/// as a plain identifier and is compared as text, without regard for ASCII case, exactly
/// as `Keyword::parse` compares words.
///
/// # Errors
///
/// The syntax error of a token that is neither.
fn eat_rows(p: &mut Parser) -> SqlResult<bool> {
    if p.eat_keyword(Keyword::Rows) {
        return Ok(false);
    }
    if at_word(p, "ROW") {
        p.advance();
        return Ok(true);
    }
    Err(p.error_here())
}

/// Whether the cursor sits on the bare word `word`, whatever case it is written in.
fn at_word(p: &Parser, word: &str) -> bool {
    matches!(
        &p.peek().kind,
        TokenKind::Ident { value, quoted: false } if value.eq_ignore_ascii_case(word)
    )
}

/// Whether the token `n` ahead is the `*` of a wildcard, which the lexer reads as the
/// multiplication sign.
fn at_star(p: &Parser, n: usize) -> bool {
    matches!(p.peek_at(n).kind, TokenKind::Op(Op::Star))
}

/// Whether the token `n` ahead can be read as a name: an identifier, delimited or not, or
/// a keyword T-SQL does not reserve.
///
/// Shared with `parser/from.rs`: the rule that tells a bare alias from the next clause is
/// the same for a select item and for a table reference.
pub(crate) fn at_name(p: &Parser, n: usize) -> bool {
    match &p.peek_at(n).kind {
        TokenKind::Ident { .. } => true,
        TokenKind::Keyword(keyword) => !keyword.is_reserved(),
        _ => false,
    }
}

/// Whether the token `n` ahead is the `=` of `alias = expression` or of `@x =
/// expression`.
///
/// A second `=` right after it is not an assignment but the start of an expression the
/// grammar has no rule for, and reporting the error on it is clearer than reporting it
/// inside an alias that was never written.
fn at_assignment_sign(p: &Parser, n: usize) -> bool {
    matches!(p.peek_at(n).kind, TokenKind::Op(Op::Eq))
        && !matches!(p.peek_at(n.saturating_add(1)).kind, TokenKind::Op(Op::Eq))
}

/// Whether the select list starts a `t.*`: one or more name parts, then `.`, then `*`.
///
/// The whole shape is read with `peek_at`, so nothing is parsed twice and the cursor
/// never has to be reset. An empty part is allowed in the middle (`db..t.*`), exactly as
/// [`Parser::parse_object_name`] allows it.
fn at_qualified_wildcard(p: &Parser) -> bool {
    if !at_name(p, 0) {
        return false;
    }
    let mut n = 1;
    loop {
        if !matches!(p.peek_at(n).kind, TokenKind::Punct(Punct::Dot)) {
            return false;
        }
        n += 1;
        if at_star(p, n) {
            return true;
        }
        if matches!(p.peek_at(n).kind, TokenKind::Punct(Punct::Dot)) {
            continue;
        }
        if !at_name(p, n) {
            return false;
        }
        n += 1;
    }
}
