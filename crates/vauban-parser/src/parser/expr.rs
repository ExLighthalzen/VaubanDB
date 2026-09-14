//! Expressions and operator precedence.
//!
//! One precedence-climbing loop ([`parse_expr_bp`]) and one table ([`binding_power`]),
//! rather than one function per rank: the operator precedence table of T-SQL is then
//! written line by line and read back line by line.
//!
//! # The precedence of T-SQL is not the one of C
//!
//! From the tightest to the loosest:
//!
//! | Rank | Operators |
//! |---|---|
//! | 1 | `~` |
//! | 2 | `*`, `/`, `%` |
//! | 3 | unary `+`, unary `-`, `+`, `-`, `&`, `^`, \| |
//! | 4 | `=`, `>`, `<`, `>=`, `<=`, `<>`, `!=`, `!>`, `!<` |
//! | 5 | `NOT` |
//! | 6 | `AND` |
//! | 7 | `ALL`, `ANY`, `BETWEEN`, `IN`, `LIKE`, `OR`, `SOME` |
//! | 8 | `=` (assignment, not read here: `query.rs` and `flow.rs`) |
//!
//! Rank 3 is the trap: `&`, `^` and \| sit with `+` and `-`, so `1 | 2 + 3` is
//! `(1 | 2) + 3`, not `1 | (2 + 3)`.
//!
//! # Value position and predicate position
//!
//! T-SQL has two nonterminals where most languages have one: a *search condition* (what
//! `WHERE`, `ON`, `HAVING`, `IF` and `CASE WHEN` read) and a scalar *expression* (what a
//! select item, a function argument or an arithmetic operand reads). A comparison is a
//! search condition, never a value: `SELECT (1 = 1)` is a syntax error while
//! `WHERE (1 = 1)` is not.
//!
//! Both come out of the same loop, told apart by the minimum binding power it starts
//! from: [`parse_expr`] starts at [`MIN_BP_PREDICATE`] and accepts everything,
//! [`parse_value_expr`] starts at [`MIN_BP_VALUE`], which is above the rank-4 operators
//! and therefore refuses them. The parentheses of an [`Expr::Nested`] inherit the
//! position they stand in, which is what makes `NOT (a = 1 AND b = 2)` legal and
//! `1 + (1 = 1)` a syntax error.

use vauban_errors::{InternalError, SqlError, SqlResult};

use crate::ast::expr::{
    BinaryOp, CaseArm, ColumnRef, Expr, FrameBound, FrameUnits, Ident, InList, Literal, ObjectName,
    Over, Quantifier, UnaryOp, WindowFrame,
};
use crate::ast::query::SelectStatement;
use crate::ast::stmt::Statement;
use crate::keyword::Keyword;
use crate::parser::datatype::parse_data_type;
use crate::parser::{Parser, query};
use crate::token::{Op, Punct, TokenKind};

/// The minimum binding power of a **predicate** position: nothing is refused.
const MIN_BP_PREDICATE: u8 = 0;

/// The minimum binding power of a **value** position.
///
/// It is the right binding power of a rank-4 operator, so a comparison, a `NOT`, an
/// `AND`, an `OR` and the suffix predicates (`IS`, `IN`, `LIKE`, `BETWEEN`) all bind too
/// loosely to be read there. The loop simply stops on them and the caller reports the
/// syntax error on the operator, which is what SQL Server does with `SELECT (1 = 1)`.
const MIN_BP_VALUE: u8 = 17;

/// `OR`, rank 7, left-associative.
const BP_OR: (u8, u8) = (10, 11);
/// `AND`, rank 6, left-associative.
const BP_AND: (u8, u8) = (12, 13);
/// `NOT`, rank 5, a prefix: the binding power of its operand.
const BP_NOT: u8 = 14;
/// The comparison operators, rank 4, left-associative. Its left binding power is also
/// the one of the suffix predicates (see [`parse_predicate_suffix`]).
const BP_COMPARISON: (u8, u8) = (16, 17);
/// `+`, `-`, `&`, `^` and `|`, rank 3, left-associative and **all at the same rank**.
const BP_ADDITIVE: (u8, u8) = (18, 19);
/// `*`, `/` and `%`, rank 2, left-associative.
const BP_MULTIPLICATIVE: (u8, u8) = (20, 21);
/// Unary `+`, `-` and `~`, a prefix: the binding power of its operand.
///
/// Above rank 2, hence above rank 3 as well. Rank 1 is where T-SQL puts `~`; unary `+`
/// and `-` are printed at rank 3, next to the binary operators of the same spelling, but
/// binding them there would make `-1 & 2` mean `-(1 & 2)`.
///
/// `SELECT -1 & 2` returns **2** on SQL Server, the value of `(-1) & 2`, where
/// `-(1 & 2)` would be `0` (`precedence_unary` below). The companion `SELECT -2 * 3`
/// tells the two readings apart on nothing: `-(2 * 3)` and `(-2) * 3` are both `-6`, so
/// it is a witness of the value, not a discriminating vector.
const BP_UNARY: u8 = 22;
/// `COLLATE`, a suffix that binds tighter than every operator: `c COLLATE X = 'a'`
/// compares the collated column, it does not collate the comparison.
const BP_COLLATE: u8 = 24;

/// The left and right binding powers of `op`, or `None` when `op` is no operator of the
/// expression grammar.
///
/// A left binding power below the caller's minimum stops the loop; the right one is what
/// the loop reads the right operand with, and being one above the left one is what makes
/// every binary operator of T-SQL left-associative (`8 / 4 / 2` is `(8 / 4) / 2`).
///
/// [`BinaryOp::Concat`] is never produced by the parser (the binder decides between an
/// addition and a concatenation), hence its `None`.
fn binding_power(op: BinaryOp) -> Option<(u8, u8)> {
    Some(match op {
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => BP_MULTIPLICATIVE,
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
            BP_ADDITIVE
        }
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Lt
        | BinaryOp::Le
        | BinaryOp::Gt
        | BinaryOp::Ge
        | BinaryOp::NotLt
        | BinaryOp::NotGt => BP_COMPARISON,
        BinaryOp::And => BP_AND,
        BinaryOp::Or => BP_OR,
        BinaryOp::Concat => return None,
    })
}

/// Parses an expression in **predicate** position: comparisons, `NOT`, `AND`, `OR` and
/// the suffix predicates are all allowed.
///
/// This is the entry point of `WHERE`, `ON`, `HAVING`, `IF`, `WHILE` and the `WHEN` of a
/// searched `CASE`. A select item, a function argument or an operand is a **value** and
/// uses [`parse_value_expr`] instead.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_expr(p: &mut Parser) -> SqlResult<Expr> {
    parse_expr_bp(p, MIN_BP_PREDICATE)
}

/// Parses an expression in **value** position: a scalar expression, never a predicate.
///
/// `1 + (1 = 1)` and `SELECT (1 = 1)` are syntax errors in T-SQL because a comparison is
/// not a value; that refusal is this function, and it is why every value position of the
/// grammar (call argument, `IN` list item, `CAST` operand, `THEN` result, select item)
/// goes through it rather than through [`parse_expr`].
///
/// # Errors
///
/// The syntax error that stopped the parse, the rank-4 operator included.
pub(crate) fn parse_value_expr(p: &mut Parser) -> SqlResult<Expr> {
    parse_expr_bp(p, MIN_BP_VALUE)
}

/// Parses a `,`-separated list of value expressions, at least one.
///
/// Every comma-separated list of expressions of T-SQL is a list of **values**: the
/// arguments of a call, an `IN` list, a `VALUES` row, `GROUP BY`, `PARTITION BY`.
///
/// # Errors
///
/// The syntax error that stopped the parse of one of the items.
pub(crate) fn parse_expr_list(p: &mut Parser) -> SqlResult<Vec<Expr>> {
    let mut items = vec![parse_value_expr(p)?];
    while p.eat_punct(Punct::Comma) {
        items.push(parse_value_expr(p)?);
    }
    Ok(items)
}

/// The precedence-climbing loop: reads a prefix, then every suffix and every binary
/// operator whose left binding power reaches `min_bp`.
///
/// # A predicate is not a left operand either
///
/// Once the loop has built a search condition, only the operators of rank 5 to 7 (`NOT`,
/// `AND`, `OR`) may follow it: a comparison and a suffix predicate both read a **value**
/// on their left, so `a = b = c`, `1 < 2 < 3` and `1 IN (1, 2) IN (1)` are syntax errors
/// (see [`is_predicate`]). Refusing them here rather than through `min_bp` is what keeps
/// `a = 1 AND b = 2` legal: `AND` binds looser than the comparison it follows.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_expr_bp(p: &mut Parser, min_bp: u8) -> SqlResult<Expr> {
    // The one recursive door of the expression grammar: `(`, a call argument, an operand,
    // a `CASE` arm and a subquery re-enter here; the levels are counted there.
    p.nested(|p| parse_expr_bp_inner(p, min_bp))
}

/// The body of [`parse_expr_bp`], one level deeper on the nesting counter.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_expr_bp_inner(p: &mut Parser, min_bp: u8) -> SqlResult<Expr> {
    let start = p.mark();
    let mut left = parse_prefix(p, min_bp)?;
    loop {
        if BP_COLLATE >= min_bp && p.at_keyword(Keyword::Collate) {
            p.advance();
            // A collation is a bare regular name, never a string literal. Its span is read
            // before the name is consumed: errors 447 and 448 are reported on that token
            // and not on the clause (`tests/operator_span.rs`).
            let collation_span = p.peek().span;
            let collation = p.parse_ident()?.value;
            left = Expr::Collate {
                expr: Box::new(left),
                collation,
                collation_span,
                span: p.span_from(start),
            };
            continue;
        }
        if BP_COMPARISON.0 >= min_bp && at_predicate_suffix(p) {
            if is_predicate(&left) {
                return Err(p.error_here());
            }
            left = parse_predicate_suffix(p, left, start)?;
            continue;
        }
        let Some(op) = peek_binary_op(p) else { break };
        let Some((lbp, rbp)) = binding_power(op) else {
            break;
        };
        if lbp < min_bp {
            break;
        }
        if (lbp, rbp) == BP_COMPARISON && is_predicate(&left) {
            return Err(p.error_here());
        }
        // Read before the token is consumed: errors 402 and 8117 are reported on the
        // operator, which no other span of the node locates once a comment sits between
        // the operands (`tests/operator_span.rs`).
        let op_span = p.peek().span;
        p.advance();
        if let Some(quantifier) = eat_quantifier(p, op) {
            p.expect_punct(Punct::LeftParen)?;
            let subquery = parse_subquery(p)?;
            p.expect_punct(Punct::RightParen)?;
            left = Expr::Quantified {
                expr: Box::new(left),
                op,
                quantifier,
                subquery,
                span: p.span_from(start),
            };
            continue;
        }
        let right = parse_expr_bp(p, rbp)?;
        left = Expr::Binary {
            op,
            op_span,
            left: Box::new(left),
            right: Box::new(right),
            span: p.span_from(start),
        };
    }
    Ok(left)
}

/// The binary operator the cursor sits on, or `None`.
///
/// The compound assignment operators (`+=`, `&=`, ...) are not expression operators:
/// they belong to `SET @x += 1`, which `flow.rs` reads.
fn peek_binary_op(p: &Parser) -> Option<BinaryOp> {
    match p.peek().kind {
        TokenKind::Op(op) => match op {
            Op::Plus => Some(BinaryOp::Add),
            Op::Minus => Some(BinaryOp::Sub),
            Op::Star => Some(BinaryOp::Mul),
            Op::Slash => Some(BinaryOp::Div),
            Op::Percent => Some(BinaryOp::Mod),
            Op::Ampersand => Some(BinaryOp::BitAnd),
            Op::Pipe => Some(BinaryOp::BitOr),
            Op::Caret => Some(BinaryOp::BitXor),
            Op::Eq => Some(BinaryOp::Eq),
            Op::Ne | Op::BangEq => Some(BinaryOp::Ne),
            Op::Lt => Some(BinaryOp::Lt),
            Op::Le => Some(BinaryOp::Le),
            Op::Gt => Some(BinaryOp::Gt),
            Op::Ge => Some(BinaryOp::Ge),
            Op::NotLt => Some(BinaryOp::NotLt),
            Op::NotGt => Some(BinaryOp::NotGt),
            _ => None,
        },
        TokenKind::Keyword(Keyword::And) => Some(BinaryOp::And),
        TokenKind::Keyword(Keyword::Or) => Some(BinaryOp::Or),
        _ => None,
    }
}

/// Consumes the `ALL`, `ANY` or `SOME` that turns a comparison into a quantified
/// predicate, if one is there.
///
/// The word must be followed by `(`, which is the only shape T-SQL gives it, and the
/// operator must be a comparison: `1 + ALL (…)` is not a predicate.
fn eat_quantifier(p: &mut Parser, op: BinaryOp) -> Option<Quantifier> {
    if binding_power(op) != Some(BP_COMPARISON) {
        return None;
    }
    let quantifier = match p.peek().kind {
        TokenKind::Keyword(Keyword::All) => Quantifier::All,
        TokenKind::Keyword(Keyword::Any) => Quantifier::Any,
        TokenKind::Keyword(Keyword::Some) => Quantifier::Some,
        _ => return None,
    };
    if !matches!(p.peek_at(1).kind, TokenKind::Punct(Punct::LeftParen)) {
        return None;
    }
    p.advance();
    Some(quantifier)
}

/// Whether `expr` is a **search condition** rather than a value.
///
/// T-SQL never lets one stand on the left of a comparison or of a suffix predicate, since
/// both read a value there: `a = b = c` is a syntax error, and so are `1 < 2 < 3` and
/// `1 IN (1, 2) IN (1)`. The error the refusal produces
/// (`chained_comparison_is_a_syntax_error` below): `SELECT 1 = 1 = 1` gives a 102 on the
/// second `=`, while
/// `SELECT 1 IN (1) = 1` and `SELECT 1 IS NULL = 1` give a 156 on the reserved word that
/// opened the predicate (`IN`, `IS`), not on the chained `=`.
///
/// The nodes listed are the ones the grammar builds out of a comparison, a logical
/// operator or a suffix predicate. The parentheses of `(1 = 1) = 1` are **not** looked
/// through: what SQL Server answers to a parenthesised search condition used as a value
/// is a separate question.
fn is_predicate(expr: &Expr) -> bool {
    match expr {
        // Rank 4 to 7: the comparisons, `AND` and `OR`. Anything tighter is a value.
        Expr::Binary { op, .. } => {
            binding_power(*op).is_some_and(|(lbp, _)| lbp <= BP_COMPARISON.0)
        }
        Expr::Unary {
            op: UnaryOp::Not, ..
        } => true,
        Expr::IsNull { .. }
        | Expr::In { .. }
        | Expr::Like { .. }
        | Expr::Between { .. }
        | Expr::Quantified { .. }
        | Expr::Exists(..) => true,
        // SQL Server refuses `(1 = 1) = 1` in both positions with 102 near '=', so a
        // parenthesised search condition stays a predicate.
        Expr::Nested(inner, ..) => is_predicate(inner),
        _ => false,
    }
}

/// Whether the cursor sits on a suffix predicate: `IS`, `IN`, `LIKE`, `BETWEEN`, or the
/// `NOT` of `NOT IN`, `NOT LIKE`, `NOT BETWEEN`.
fn at_predicate_suffix(p: &Parser) -> bool {
    match p.peek().kind {
        TokenKind::Keyword(Keyword::Is | Keyword::In | Keyword::Like | Keyword::Between) => true,
        TokenKind::Keyword(Keyword::Not) => matches!(
            p.peek_at(1).kind,
            TokenKind::Keyword(Keyword::In | Keyword::Like | Keyword::Between)
        ),
        _ => false,
    }
}

/// Reads the suffix predicate the cursor sits on and wraps `left` in it.
///
/// The `NOT` of `a NOT IN (…)` sets `negated`; it does **not** become a
/// `Unary(Not, In { negated: false })`, which is what `NOT a IN (…)` produces. The two
/// spellings mean the same thing, and keeping them apart is what lets `Display` write
/// each one back as it was read.
///
/// # Errors
///
/// The syntax error of the token that does not fit the predicate.
fn parse_predicate_suffix(p: &mut Parser, left: Expr, start: usize) -> SqlResult<Expr> {
    if p.eat_keyword(Keyword::Is) {
        let negated = p.eat_keyword(Keyword::Not);
        // `IS DISTINCT FROM` is V4: `DISTINCT` gets the error, as any other word would.
        p.expect_keyword(Keyword::Null)?;
        return Ok(Expr::IsNull {
            expr: Box::new(left),
            negated,
            span: p.span_from(start),
        });
    }
    let negated = p.eat_keyword(Keyword::Not);
    if p.eat_keyword(Keyword::In) {
        p.expect_punct(Punct::LeftParen)?;
        let list = if at_subquery_head(p, 0) {
            InList::Subquery(parse_subquery(p)?)
        } else {
            InList::Exprs(parse_expr_list(p)?)
        };
        p.expect_punct(Punct::RightParen)?;
        return Ok(Expr::In {
            expr: Box::new(left),
            list,
            negated,
            span: p.span_from(start),
        });
    }
    if p.eat_keyword(Keyword::Like) {
        let pattern = parse_value_expr(p)?;
        // `ESCAPE` takes an expression, not only a literal.
        let escape = if p.eat_keyword(Keyword::Escape) {
            Some(Box::new(parse_value_expr(p)?))
        } else {
            None
        };
        return Ok(Expr::Like {
            expr: Box::new(left),
            pattern: Box::new(pattern),
            escape,
            negated,
            span: p.span_from(start),
        });
    }
    if p.eat_keyword(Keyword::Between) {
        // Both bounds are values, so the `AND` that separates them cannot be read as the
        // logical operator: `a BETWEEN 1 AND 2 AND b = 3` is `(a BETWEEN 1 AND 2) AND …`.
        let low = parse_value_expr(p)?;
        p.expect_keyword(Keyword::And)?;
        let high = parse_value_expr(p)?;
        return Ok(Expr::Between {
            expr: Box::new(left),
            low: Box::new(low),
            high: Box::new(high),
            negated,
            span: p.span_from(start),
        });
    }
    Err(p.error_here())
}

/// Reads the prefix operators (`-`, `+`, `~`, `NOT`) and then a primary.
///
/// `NOT` is rank 5 and is therefore refused in a value position, where `min_bp` is above
/// its binding power: `SELECT NOT 1` is a syntax error, and so is `1 + NOT a`.
///
/// # One operator, one recursion, one unit of the nesting guard
///
/// This function reads **one** operator and then goes back through [`parse_expr_bp`] for
/// its operand; it does not loop over the operators itself. That is what makes a chain of
/// them cost the depth counter of `Parser::nested` one unit per operator, the same as a
/// parenthesis, and it is the reason `SELECT - - … 1` answers 191 at 30 instead of
/// overflowing the stack at 250 (the second figure taken on a tree without the guard).
/// **Do not turn this into a `while` loop over the prefix operators**: it would
/// save a handful of frames per batch and would hand back, uncounted, the deepest
/// unauthenticated denial of service this crate has had. `tests/nesting_depth.rs` fails
/// if it happens.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_prefix(p: &mut Parser, min_bp: u8) -> SqlResult<Expr> {
    let start = p.mark();
    let op = match p.peek().kind {
        TokenKind::Op(Op::Minus) => UnaryOp::Minus,
        TokenKind::Op(Op::Plus) => UnaryOp::Plus,
        TokenKind::Op(Op::Tilde) => UnaryOp::BitNot,
        TokenKind::Keyword(Keyword::Not) if min_bp <= BP_NOT => UnaryOp::Not,
        _ => return parse_primary(p, min_bp),
    };
    p.advance();
    let rbp = if op == UnaryOp::Not { BP_NOT } else { BP_UNARY };
    let expr = parse_expr_bp(p, rbp)?;
    Ok(Expr::Unary {
        op,
        expr: Box::new(expr),
        span: p.span_from(start),
    })
}

/// Reads one primary: a literal, a variable, a parenthesised expression or subquery, one
/// of the reserved constructs (`CASE`, `CAST`, `CONVERT`, `EXISTS`, `NEXT VALUE FOR`), a
/// niladic function, a call, or a column reference.
///
/// `min_bp` is only used to give the parentheses the position they stand in.
///
/// # Errors
///
/// The syntax error of the token that starts no expression.
fn parse_primary(p: &mut Parser, min_bp: u8) -> SqlResult<Expr> {
    let start = p.mark();
    if let Some(literal) = parse_literal(p) {
        return Ok(Expr::Literal(literal, p.span_from(start)));
    }
    if matches!(p.peek().kind, TokenKind::Variable) {
        let name = p.advance().text;
        return Ok(Expr::Variable {
            name,
            span: p.span_from(start),
        });
    }
    if p.at_punct(Punct::LeftParen) {
        return parse_parenthesised(p, min_bp);
    }
    if p.at_keyword(Keyword::Case) {
        return parse_case(p);
    }
    if p.at_keyword(Keyword::Cast) || p.at_keyword(Keyword::TryCast) {
        return parse_cast(p);
    }
    if p.at_keyword(Keyword::Convert) || p.at_keyword(Keyword::TryConvert) {
        return parse_convert(p);
    }
    if p.at_keyword(Keyword::Exists) {
        // `EXISTS (…)` yields a search condition, never a value: `SELECT EXISTS (SELECT
        // 1)` is a syntax error while `WHERE EXISTS (SELECT 1)` is not. The test is the
        // one of `parse_parenthesised`: above rank 4, the position is a value. The error
        // is a 156 on the reserved `EXISTS`, not a 102 (`exists_is_not_a_value` below).
        if min_bp > BP_COMPARISON.0 {
            return Err(p.error_here());
        }
        p.advance();
        p.expect_punct(Punct::LeftParen)?;
        let query = parse_subquery(p)?;
        p.expect_punct(Punct::RightParen)?;
        return Ok(Expr::Exists(query, p.span_from(start)));
    }
    if at_next_value_for(p) {
        return parse_next_value_for(p);
    }
    let false_keyword = matches!(
        p.peek().kind,
        TokenKind::Keyword(Keyword::CurrentDate | Keyword::CurrentTime)
    );
    let niladic_name = match &p.peek().kind {
        TokenKind::Keyword(keyword) => NILADIC_FUNCTIONS.contains(keyword),
        TokenKind::Ident {
            value,
            quoted: true,
        } => Keyword::parse(value).is_some_and(|keyword| {
            NILADIC_FUNCTIONS.contains(&keyword)
                || matches!(keyword, Keyword::CurrentDate | Keyword::CurrentTime)
        }),
        _ => false,
    };
    // SELECT CURRENT_TIMESTAMP(SELECT 1) is two statements on SQL Server.
    // Leave the parenthesised query to the statement parser, rather than treating
    // its SELECT as an argument token. The same reader diagnoses (SELECT).
    let mut query_offset = 1;
    while matches!(
        p.peek_at(query_offset).kind,
        TokenKind::Punct(Punct::LeftParen)
    ) {
        query_offset += 1;
    }
    let following_query = query_offset > 1 && at_subquery_head(p, query_offset);
    if niladic_name && following_query && !at_niladic_function(p) {
        return parse_column_ref(p);
    }
    if false_keyword
        || (niladic_name
            && !following_query
            && matches!(p.peek_at(1).kind, TokenKind::Punct(Punct::LeftParen)))
    {
        return parse_invalid_niladic(p, false_keyword);
    }
    if at_niladic_function(p) {
        // A niladic function written without parentheses is kept as a one-part,
        // unquoted column reference; the binder is what tells `CURRENT_TIMESTAMP` from a
        // column named so (there can be none: the word is reserved).
        let name = p.advance().text;
        return Ok(Expr::Column(ColumnRef {
            qualifier: None,
            name: Ident {
                value: name,
                quoted: false,
            },
            span: p.span_from(start),
        }));
    }
    let call = p.mark();
    if let Some(name) = try_function_name(p) {
        if p.at_punct(Punct::LeftParen) {
            return parse_call(p, name, start);
        }
        p.reset(call);
    }
    parse_column_ref(p)
}

/// Changes the niladic diagnostic inside a delimited expression.
///
/// Called for call arguments, `CAST`, `CASE`, parentheses and the count of `TOP`
/// (`tests/niladic.rs`), and for the **value** operand of `CONVERT`, which is given the
/// treatment of `CAST` without a test of its own.
///
/// The style operand of `CONVERT` is not walked, so two operands of the same call answer
/// differently: `SELECT CONVERT(int, CURRENT_TIMESTAMP());` answers 102 near `(` and
/// `SELECT CONVERT(varchar(10), 1, CURRENT_TIMESTAMP());` answers 102 near `)`, `TRY_CONVERT`
/// alike. Nothing here says which one SQL Server would give; the binder half of the
/// mechanism, `contains_invalid_niladic`, does walk `Convert.style`.
pub(crate) fn enclose_niladic_diagnostics(expr: &mut Expr) {
    visit_niladic_diagnostics(expr, &mut |expr| {
        if let Expr::InvalidNiladic {
            diagnostic_token,
            diagnostic_number,
            diagnostic_span,
            opening_span: Some(opening),
            ..
        } = expr
        {
            let false_keyword = *diagnostic_number == 156
                && matches!(
                    Keyword::parse(diagnostic_token),
                    Some(Keyword::CurrentDate | Keyword::CurrentTime)
                );
            if !false_keyword {
                *diagnostic_token = "(".to_owned();
                *diagnostic_number = 102;
                *diagnostic_span = *opening;
            }
        }
    });
}

/// Walks `expr` and the sub-expressions of the families matched below, deepest-first on
/// the left, handing each [`Expr::InvalidNiladic`] met on the way to `visit`.
///
/// # Why it is written on a worklist
///
/// The walk used to recurse once per node, which a **chain** read in a loop by the
/// expression ladder makes as deep as the client cares to write: the links of
/// `SELECT ABS(1 + 1 + …)` cost nothing to the nesting guard, so the batch is accepted and
/// the walk that follows the parse ran off the stack. Bisected on a 2 MiB stack with the
/// recursive walk: that shape parsed at 3 909 links and aborted the process at 3 910 in a
/// debug build, 26 219 and 26 220 in release; `SELECT CASE WHEN 1 = 1 AND … END` at
/// 3 897/3 898 and 26 221/26 222. The debug threshold of the first shape is 15 649 bytes of
/// text, that of the second 39 010, both the size of an ordinary batch. The tree is now
/// taken apart into a `Vec` on the heap, the method the destructor uses, so the depth of
/// the shape costs heap rather than frames.
///
/// The order is the one the recursive walk had: children are pushed in reverse so that the
/// last-in-first-out worklist pops them left to right, and `nesting_depth.rs`'s
/// `the_niladic_diagnostic_at_the_end_of_a_comb_is_still_enclosed` reads the node at the
/// far end of a 50 000-link chain.
///
/// # The other recursive walks of `src/parser/`, and why they stay as they are
///
/// Grepped for a function whose body names itself (`fn <name>` … `<name>(`), the directory
/// holds three besides this one:
///
/// - `is_predicate`, in this file, recurses through `Expr::Nested` and nothing else; a
///   `Nested` is built by `parse_parenthesised`, which reads its operand through
///   `Parser::nested`, so the chain it walks stops at `MAX_NESTING_DEPTH` = 32 --
///   `nesting_depth.rs`'s `deep_parentheses_are_an_error_and_not_an_abort` answers 191 to
///   2 000 parentheses instead of building the tree.
/// - `shape`, in the `#[cfg(test)]` module of this file, and `descend`, in the one of
///   `mod.rs`: test helpers, off the path a batch takes.
///
/// The parse rules recurse into one another rather than into themselves, and that is what
/// the nesting guard counts. The half of this mechanism that lives in `binder`,
/// `contains_invalid_niladic`, was already written on a worklist. `Display`, `Clone` and
/// `PartialEq` of the AST recurse once per link too (719, 696 and 1 683 links in debug,
/// tried on a 2 MiB stack) and are not covered here.
fn visit_niladic_diagnostics(expr: &mut Expr, visit: &mut impl FnMut(&mut Expr)) {
    let mut work: Vec<&mut Expr> = vec![expr];
    while let Some(expr) = work.pop() {
        match expr {
            Expr::InvalidNiladic { .. } => visit(expr),
            Expr::Nested(inner, _)
            | Expr::Unary { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::Collate { expr: inner, .. } => work.push(inner),
            Expr::Binary { left, right, .. } => {
                work.push(right);
                work.push(left);
            }
            Expr::Function { args, .. } => work.extend(args.iter_mut().rev()),
            Expr::Convert { expr, style, .. } => {
                if let Some(style) = style {
                    work.push(style);
                }
                work.push(expr);
            }
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                if let Some(otherwise) = else_ {
                    work.push(otherwise);
                }
                for arm in arms.iter_mut().rev() {
                    work.push(&mut arm.then);
                    work.push(&mut arm.when);
                }
                if let Some(operand) = operand {
                    work.push(operand);
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                if let Some(escape) = escape {
                    work.push(escape);
                }
                work.push(pattern);
                work.push(expr);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            Expr::In { expr, list, .. } => {
                if let InList::Exprs(items) = list {
                    work.extend(items.iter_mut().rev());
                }
                work.push(expr);
            }
            Expr::Assign { value, .. } => work.push(value),
            _ => {}
        }
    }
}

/// Retains a diagnostic for this family until preceding expressions are bound.
/// No variable lookup or recovery of unrelated syntax occurs here.
fn parse_invalid_niladic(p: &mut Parser, false_keyword: bool) -> SqlResult<Expr> {
    let start = p.mark();
    let first = p.advance();
    let mut spelling = vec![first.text.clone()];
    let mut diagnostic = first;
    let opening_span = if p.at_punct(Punct::LeftParen) {
        Some(p.peek().span)
    } else {
        None
    };
    if p.at_punct(Punct::LeftParen) {
        spelling.push(p.advance().text);
        if !false_keyword {
            // The precise nested shapes are covered by `tests/niladic.rs`.
            let mut offset = 0;
            while matches!(p.peek_at(offset).kind, TokenKind::Punct(Punct::LeftParen)) {
                offset += 1;
            }
            diagnostic = p.peek_at(offset).clone();
        }
        let mut depth = 1usize;
        while depth != 0 {
            if p.at_eof() || p.at_punct(Punct::Semicolon) {
                return Err(p.error_here());
            }
            let token = p.advance();
            match token.kind {
                TokenKind::Punct(Punct::LeftParen) => depth += 1,
                TokenKind::Punct(Punct::RightParen) => depth -= 1,
                _ => {}
            }
            spelling.push(token.text);
        }
    }
    Ok(Expr::InvalidNiladic {
        spelling: spelling.join(" "),
        diagnostic_number: if matches!(diagnostic.kind, TokenKind::Keyword(k) if k.is_reserved()) {
            156
        } else {
            102
        },
        diagnostic_token: match diagnostic.kind {
            TokenKind::Ident {
                value,
                quoted: true,
            }
            | TokenKind::Str { value, .. } => value,
            TokenKind::Binary(_) => diagnostic.text.to_lowercase(),
            _ => diagnostic.text,
        },
        diagnostic_span: diagnostic.span,
        opening_span,
        span: p.span_from(start),
    })
}

/// Reads the literal the cursor sits on, or nothing at all.
///
/// This is where the source text of a token becomes the payload `types::parse_literal`
/// expects: the `$` of a money literal and the `0x` of a binary one are dropped here, not
/// by the lexer.
fn parse_literal(p: &mut Parser) -> Option<Literal> {
    let literal = match &p.peek().kind {
        TokenKind::Integer => Literal::Integer(p.peek().text.clone()),
        TokenKind::Decimal => Literal::Decimal(p.peek().text.clone()),
        TokenKind::Float => Literal::Float(p.peek().text.clone()),
        TokenKind::Money => Literal::Money(without_dollar(&p.peek().text)),
        TokenKind::Binary(digits) => Literal::Binary(digits.clone()),
        TokenKind::Str { value, unicode } => Literal::Str {
            value: value.clone(),
            unicode: *unicode,
        },
        TokenKind::Keyword(Keyword::Null) => Literal::Null,
        TokenKind::Keyword(Keyword::Default) => Literal::Default,
        _ => return None,
    };
    p.advance();
    Some(literal)
}

/// Drops the leading `$` of the source text of a money literal, sign kept: `$-1.50`
/// becomes `-1.50`.
fn without_dollar(text: &str) -> String {
    match text.strip_prefix('$') {
        Some(rest) => rest.to_owned(),
        None => text.to_owned(),
    }
}

/// Whether the token `n` ahead opens a query: `SELECT`, or the `WITH` of a CTE (V2).
fn at_subquery_head(p: &Parser, n: usize) -> bool {
    matches!(
        p.peek_at(n).kind,
        TokenKind::Keyword(Keyword::Select | Keyword::With)
    )
}

/// Reads a `SELECT` in expression position and returns it as a statement.
///
/// The whole `SELECT` grammar lives in `parser/query.rs`.
///
/// # Errors
///
/// The syntax error that stopped the parse of the query.
fn parse_subquery(p: &mut Parser) -> SqlResult<Box<SelectStatement>> {
    match query::parse_select_statement(p)? {
        Statement::Select(select) => Ok(select),
        _ => Err(SqlError::from(InternalError::Bug(
            "query::parse_select_statement returned a statement that is not a SELECT".into(),
        ))),
    }
}

/// Reads `( … )`: a subquery when a query follows, the parentheses the user wrote around
/// an expression otherwise.
///
/// A single `peek_at(1)` decides, so nothing is ever parsed twice. The expression inside
/// keeps the position the parentheses stand in: a predicate under `NOT`, `AND` or `OR`
/// and at the top of a search condition, a value under an operator of rank 3 or above.
///
/// # Errors
///
/// The syntax error that stopped the parse, the missing `)` included.
fn parse_parenthesised(p: &mut Parser, min_bp: u8) -> SqlResult<Expr> {
    let start = p.mark();
    let subquery = at_subquery_head(p, 1);
    p.expect_punct(Punct::LeftParen)?;
    if subquery {
        let query = parse_subquery(p)?;
        p.expect_punct(Punct::RightParen)?;
        return Ok(Expr::Subquery(query, p.span_from(start)));
    }
    let inner_bp = if min_bp > BP_COMPARISON.0 {
        MIN_BP_VALUE
    } else {
        MIN_BP_PREDICATE
    };
    let mut expr = parse_expr_bp(p, inner_bp)?;
    enclose_niladic_diagnostics(&mut expr);
    p.expect_punct(Punct::RightParen)?;
    Ok(Expr::Nested(Box::new(expr), p.span_from(start)))
}

/// How deep `CASE` expressions may nest: the eleventh answers error 125.
///
/// SQL Server gives the `CASE` a limit of its own, two orders of magnitude below the
/// general nesting guard of [`crate::parser::MAX_NESTING_DEPTH`] and with an error of its
/// own, number 125, severity 15 (the `CASE` tests at the end of this file and
/// `tests/nesting_depth.rs`):
///
/// - **A level is a `CASE` inside a `CASE`, wherever it stands.** The five positions were
///   crossed -- the `THEN`, the `ELSE` and the `WHEN` of a searched `CASE`, the operand
///   and the `WHEN` of a simple one -- and the five accept ten and refuse eleven.
/// - **The count follows the syntax, not the plan.** A parenthesis, a call (`ABS`), a
///   `CAST`, a `1 +` or a scalar subquery `(SELECT …)` put between two `CASE`s change
///   nothing: ten pass and eleven answer 125 through each of them. In particular a
///   `CASE` in a subquery in a `CASE` counts on from the outer one, so a chain of one and
///   a chain of ten do not fit through a `(SELECT …)`: a `CASE` nested eleven deep
///   through a subquery answers 125.
/// - **The state names the eleventh `CASE`**: 4 for a searched one, 3 for a simple one,
///   regardless of the ten above it (a simple eleventh under ten searched, a searched
///   twelfth under a simple eleventh, and the reverse: the eleventh decides).
/// - **The check is made on the `CASE` keyword and one token of lookahead**, before its
///   body is read. A syntax error *inside* the eleventh `CASE` (`CASE WHEN 1 + THEN`,
///   `CASE ( END`, `CASE 1 2 END`) or *after* it is not reached: 125 comes first. A token
///   after the eleventh `CASE` that opens neither a `WHEN` nor a value (`END`, `THEN`,
///   `ELSE`, `,`, `)`, `;`, `=`, `*`, `NOT`, `SELECT`, `FROM`) answers the syntax error it
///   answers at depth one, not 125; see [`starts_a_value`]. A syntax error *before* the
///   eleventh `CASE` wins over it.
/// - **The line is the one of the eleventh `CASE` keyword**, not the one of its `END` nor
///   the one of the statement, on a twelve-deep chain written over several lines.
///
/// Siblings do not add up: two chains of ten in one select list, or two chains of nine in
/// the two branches of a `CASE`, pass. The count is a depth, not a total.
///
/// `IIF`, `COALESCE` and `NULLIF` count as a `CASE` level on SQL Server too (each of them
/// answers 125 at eleven, with a state of its own: 2, 2 and 1). They are **not** counted
/// here.
const MAX_CASE_NESTING: u32 = 10;

/// The error 125 an eleventh nested `CASE` answers, on the line of its keyword.
///
/// The number, severity and the two states go with [`MAX_CASE_NESTING`]; `errors` has not
/// catalogued 125, so the error is built here.
fn case_nested_too_deeply(line: u32, searched: bool) -> SqlError {
    let state = if searched { 4 } else { 3 };
    SqlError::new(
        125,
        15,
        state,
        "A CASE expression cannot be nested deeper than level 10.",
    )
    .with_line(line)
}

/// Whether the token the cursor sits on can open a **value** expression, said without
/// consuming anything.
///
/// The one-token lookahead of the `CASE` limit: SQL Server refuses an eleventh `CASE` as
/// soon as it has read the keyword and seen that a `WHEN` or an operand follows, and
/// answers the ordinary syntax error when neither does (`CASE END` at depth eleven is a
/// 156 on `END`, `CASE ( END` a 125). This function mirrors the first token
/// [`parse_prefix`] and [`parse_primary`] accept in a value position; the unit test
/// `starts_a_value_agrees_with_the_grammar` holds the two together on a list of tokens.
/// It is consulted at the limit and not before (`parse_case`): a token it would wrongly
/// refuse goes on to the grammar, which then reads the `CASE` as it would at depth one.
fn starts_a_value(p: &mut Parser) -> bool {
    match &p.peek().kind {
        TokenKind::Integer
        | TokenKind::Decimal
        | TokenKind::Float
        | TokenKind::Money
        | TokenKind::Binary(_)
        | TokenKind::Str { .. }
        | TokenKind::Variable
        | TokenKind::Ident { .. }
        | TokenKind::Punct(Punct::LeftParen)
        | TokenKind::Op(Op::Minus | Op::Plus | Op::Tilde) => true,
        TokenKind::Keyword(keyword) => match keyword {
            Keyword::Null
            | Keyword::Default
            | Keyword::Case
            | Keyword::Cast
            | Keyword::TryCast
            | Keyword::Convert
            | Keyword::TryConvert
            | Keyword::CurrentDate
            | Keyword::CurrentTime => true,
            // `NOT` and `EXISTS` open a predicate and not a value (`parse_prefix`,
            // `parse_primary`; `exists_is_not_a_value`).
            Keyword::Not | Keyword::Exists => false,
            _ if !keyword.is_reserved() || NILADIC_FUNCTIONS.contains(keyword) => true,
            _ if at_next_value_for(p) => true,
            // A reserved word is still a function name when a `(` follows (`LEFT(a, 1)`).
            _ => {
                let start = p.mark();
                let call = try_function_name(p).is_some() && p.at_punct(Punct::LeftParen);
                p.reset(start);
                call
            }
        },
        TokenKind::Punct(_) | TokenKind::Op(_) | TokenKind::Unknown | TokenKind::Eof => false,
    }
}

/// Reads a `CASE`, simple (`CASE c WHEN 1 …`) or searched (`CASE WHEN c = 1 …`).
///
/// The `WHEN` of a simple `CASE` is a value compared to the operand; the `WHEN` of a
/// searched one is a predicate. A `CASE` without `ELSE` keeps `None`: the constant the
/// engine falls back to is not the parser's business.
///
/// # Errors
///
/// The syntax error that stopped the parse; `CASE END`, without a single arm, fails on
/// its `END`. Error 125 on the eleventh nested `CASE`, see [`MAX_CASE_NESTING`].
fn parse_case(p: &mut Parser) -> SqlResult<Expr> {
    let start = p.mark();
    let line = p.peek().span.line;
    p.expect_keyword(Keyword::Case)?;
    let searched = p.at_keyword(Keyword::When);
    if p.case_depth() >= MAX_CASE_NESTING && (searched || starts_a_value(p)) {
        return Err(case_nested_too_deeply(line, searched));
    }
    p.nested_case(|p| parse_case_body(p, start))
}

/// Reads what follows the `CASE` keyword, up to and including its `END`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_case_body(p: &mut Parser, start: usize) -> SqlResult<Expr> {
    let operand = if p.at_keyword(Keyword::When) {
        None
    } else {
        Some(Box::new(parse_value_expr(p)?))
    };
    let mut arms = Vec::new();
    while p.eat_keyword(Keyword::When) {
        let when = if operand.is_some() {
            parse_value_expr(p)?
        } else {
            parse_expr(p)?
        };
        p.expect_keyword(Keyword::Then)?;
        let then = parse_value_expr(p)?;
        arms.push(CaseArm { when, then });
    }
    if arms.is_empty() {
        return Err(p.error_here());
    }
    let else_ = if p.eat_keyword(Keyword::Else) {
        Some(Box::new(parse_value_expr(p)?))
    } else {
        None
    };
    p.expect_keyword(Keyword::End)?;
    let mut expr = Expr::Case {
        operand,
        arms,
        else_,
        span: p.span_from(start),
    };
    enclose_niladic_diagnostics(&mut expr);
    Ok(expr)
}

/// Reads `CAST(e AS t)` or `TRY_CAST(e AS t)`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_cast(p: &mut Parser) -> SqlResult<Expr> {
    let start = p.mark();
    let try_ = p.eat_keyword(Keyword::TryCast);
    if !try_ {
        p.expect_keyword(Keyword::Cast)?;
    }
    p.expect_punct(Punct::LeftParen)?;
    let mut expr = parse_value_expr(p)?;
    enclose_niladic_diagnostics(&mut expr);
    p.expect_keyword(Keyword::As)?;
    let ty = parse_data_type(p)?;
    p.expect_punct(Punct::RightParen)?;
    Ok(Expr::Cast {
        expr: Box::new(expr),
        ty,
        try_,
        span: p.span_from(start),
    })
}

/// Reads `CONVERT(t, e [, style])` or `TRY_CONVERT(…)`.
///
/// `CONVERT` is not an ordinary call: its first argument is a **type**, and the type
/// comes first, the other way round from `CAST`. Whether the style is one T-SQL knows is
/// the binder's business.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_convert(p: &mut Parser) -> SqlResult<Expr> {
    let start = p.mark();
    let try_ = p.eat_keyword(Keyword::TryConvert);
    if !try_ {
        p.expect_keyword(Keyword::Convert)?;
    }
    p.expect_punct(Punct::LeftParen)?;
    let ty = parse_data_type(p)?;
    p.expect_punct(Punct::Comma)?;
    let mut expr = parse_value_expr(p)?;
    enclose_niladic_diagnostics(&mut expr);
    let style = if p.eat_punct(Punct::Comma) {
        Some(Box::new(parse_value_expr(p)?))
    } else {
        None
    };
    p.expect_punct(Punct::RightParen)?;
    Ok(Expr::Convert {
        ty,
        expr: Box::new(expr),
        style,
        try_,
        span: p.span_from(start),
    })
}

/// Whether the cursor sits on the `NEXT VALUE FOR` of a sequence (V3).
///
/// `NEXT` and `VALUE` are not reserved words, so the three of them have to be seen
/// before the construct is claimed: `SELECT next` is a column named `next`.
fn at_next_value_for(p: &Parser) -> bool {
    p.at_keyword(Keyword::Next)
        && matches!(p.peek_at(1).kind, TokenKind::Keyword(Keyword::Value))
        && matches!(p.peek_at(2).kind, TokenKind::Keyword(Keyword::For))
}

/// Reads `NEXT VALUE FOR seq [OVER (…)]` (V3).
///
/// The parser accepts it because the AST has it; the binder is what refuses sequences
/// until V3.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_next_value_for(p: &mut Parser) -> SqlResult<Expr> {
    let start = p.mark();
    p.expect_keyword(Keyword::Next)?;
    p.expect_keyword(Keyword::Value)?;
    p.expect_keyword(Keyword::For)?;
    let sequence = p.parse_object_name()?;
    let over = parse_over(p)?;
    Ok(Expr::NextValueFor {
        sequence,
        over,
        span: p.span_from(start),
    })
}

/// The five bare niladic keywords (`tests/niladic.rs`).
///
/// `CURRENT_DATE` and `CURRENT_TIME` are reserved words, but SQL Server 2022
/// rejects them as bare expressions with 156. The binder keeps a separate list of
/// these five function names as strings; it is outside this parser's ownership.
/// Display consumes this keyword list to preserve bare niladic expressions.
pub(crate) const NILADIC_FUNCTIONS: [Keyword; 5] = [
    Keyword::CurrentTimestamp,
    Keyword::CurrentUser,
    Keyword::SessionUser,
    Keyword::SystemUser,
    Keyword::User,
];

/// Whether the cursor sits on one of the five niladic keywords.
fn at_niladic_function(p: &Parser) -> bool {
    let TokenKind::Keyword(keyword) = p.peek().kind else {
        return false;
    };
    NILADIC_FUNCTIONS.contains(&keyword)
}

/// Whether `keyword` can never name a function.
///
/// A reserved word is a legitimate function name in T-SQL -- `LEFT(a, 1)`,
/// `RIGHT(a, 1)`, `CONVERT(…)`, `COALESCE(…)`, `USER` -- so the name of a call is read
/// from the token, not through `Parser::parse_ident`. The exception is the handful of
/// words this very grammar reads as something else: without it, `IN (1, 2)` on its own
/// would parse as a call of a function named `IN`, where SQL Server reports a syntax
/// error.
fn keyword_is_never_a_function_name(keyword: Keyword) -> bool {
    matches!(
        keyword,
        Keyword::CurrentDate
            | Keyword::CurrentTime
            | Keyword::All
            | Keyword::And
            | Keyword::Any
            | Keyword::As
            | Keyword::Between
            | Keyword::By
            | Keyword::Case
            | Keyword::Collate
            | Keyword::Default
            | Keyword::Distinct
            | Keyword::Else
            | Keyword::End
            | Keyword::Escape
            | Keyword::Exists
            | Keyword::From
            | Keyword::Group
            | Keyword::Having
            | Keyword::In
            | Keyword::Is
            | Keyword::Like
            | Keyword::Not
            | Keyword::Null
            | Keyword::Or
            | Keyword::Order
            | Keyword::Over
            | Keyword::Partition
            | Keyword::Select
            | Keyword::Some
            | Keyword::Then
            | Keyword::When
            | Keyword::Where
            | Keyword::With
    )
}

/// Reads one part of a name in function-name position: an identifier, or any keyword
/// that is not one of the words the expression grammar reads as something else.
///
/// Nothing is consumed when the token is neither.
fn read_function_name_part(p: &mut Parser) -> Option<Ident> {
    let ident = match &p.peek().kind {
        TokenKind::Ident { value, quoted } => Ident {
            value: value.clone(),
            quoted: *quoted,
        },
        TokenKind::Keyword(keyword) if !keyword_is_never_a_function_name(*keyword) => Ident {
            value: p.peek().text.clone(),
            quoted: false,
        },
        _ => return None,
    };
    p.advance();
    Some(ident)
}

/// Reads a one- to four-part name in function-name position, or nothing at all.
///
/// The cursor is left where it started when the tokens do not spell a name, so the
/// caller can try to read a column reference instead. The caller is also the one that
/// checks the `(` that makes the name a call.
fn try_function_name(p: &mut Parser) -> Option<ObjectName> {
    let start = p.mark();
    let mut parts: Vec<Option<Ident>> = Vec::new();
    loop {
        // A reserved niladic keyword after a qualifier is not a function name.
        if !parts.is_empty() && at_niladic_function(p) {
            p.reset(start);
            return None;
        }
        let part = read_function_name_part(p);
        if part.is_none() && !p.at_punct(Punct::Dot) {
            p.reset(start);
            return None;
        }
        parts.push(part);
        if parts.len() == 4 || !p.eat_punct(Punct::Dot) {
            break;
        }
    }
    let Some(Some(name)) = parts.pop() else {
        p.reset(start);
        return None;
    };
    let schema = parts.pop().flatten();
    let database = parts.pop().flatten();
    let server = parts.pop().flatten();
    Some(ObjectName {
        server,
        database,
        schema,
        name,
        span: p.span_from(start),
    })
}

/// Reads the argument list of a call whose name has already been read, and the `OVER`
/// clause that may follow it.
///
/// `start` is the mark of the first token of the whole call, which the name consumed.
/// The `*` of `count(*)` is not an expression: alone between the parentheses it sets
/// `star` and leaves `args` empty; anywhere else it is the multiplication operator.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_call(p: &mut Parser, name: ObjectName, start: usize) -> SqlResult<Expr> {
    p.expect_punct(Punct::LeftParen)?;
    let mut star = false;
    let mut distinct = false;
    let mut args = Vec::new();
    if matches!(p.peek().kind, TokenKind::Op(Op::Star))
        && matches!(p.peek_at(1).kind, TokenKind::Punct(Punct::RightParen))
    {
        p.advance();
        star = true;
    } else if !p.at_punct(Punct::RightParen) {
        distinct = p.eat_keyword(Keyword::Distinct);
        args = parse_expr_list(p)?;
        for argument in &mut args {
            enclose_niladic_diagnostics(argument);
        }
    }
    p.expect_punct(Punct::RightParen)?;
    let over = parse_over(p)?;
    Ok(Expr::Function {
        name,
        args,
        star,
        distinct,
        over,
        span: p.span_from(start),
    })
}

/// Reads a column reference of one to four parts, `c` through `db.dbo.t.c`.
///
/// The parts fill from the right, the last one being the column: `dbo.t.c` is schema
/// `dbo`, table `t`, column `c`. A part left empty (`db..t.c`) is legal and yields
/// `None`, but the part right before the column cannot be empty -- `a..c` names no
/// table -- and is a syntax error.
///
/// # Errors
///
/// The syntax error of the token that does not spell a name.
fn parse_column_ref(p: &mut Parser) -> SqlResult<Expr> {
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
    Ok(Expr::Column(ColumnRef {
        qualifier,
        name: column,
        span: p.span_from(start),
    }))
}

/// Reads the `OVER (…)` clause of a window function (V2), when there is one.
///
/// Whether the function may have one at all is the binder's business; the parser only
/// records what was written.
///
/// # Errors
///
/// The syntax error that stopped the parse of the clause.
fn parse_over(p: &mut Parser) -> SqlResult<Option<Box<Over>>> {
    if !p.at_keyword(Keyword::Over) {
        return Ok(None);
    }
    let start = p.mark();
    p.advance();
    p.expect_punct(Punct::LeftParen)?;
    let partition_by = if p.eat_keyword_seq(&[Keyword::Partition, Keyword::By]) {
        parse_expr_list(p)?
    } else {
        Vec::new()
    };
    let mut order_by = Vec::new();
    if p.eat_keyword_seq(&[Keyword::Order, Keyword::By]) {
        order_by.push(query::parse_order_item(p)?);
        while p.eat_punct(Punct::Comma) {
            order_by.push(query::parse_order_item(p)?);
        }
    }
    let frame = parse_window_frame(p)?;
    p.expect_punct(Punct::RightParen)?;
    Ok(Some(Box::new(Over {
        partition_by,
        order_by,
        frame,
        span: p.span_from(start),
    })))
}

/// Reads the `ROWS`/`RANGE` frame of an `OVER` clause, when there is one.
///
/// # Errors
///
/// The syntax error that stopped the parse of the frame.
fn parse_window_frame(p: &mut Parser) -> SqlResult<Option<WindowFrame>> {
    let units = if p.eat_keyword(Keyword::Rows) {
        FrameUnits::Rows
    } else if p.eat_keyword(Keyword::Range) {
        FrameUnits::Range
    } else {
        return Ok(None);
    };
    let between = p.eat_keyword(Keyword::Between);
    let start = parse_frame_bound(p)?;
    let end = if between {
        p.expect_keyword(Keyword::And)?;
        Some(parse_frame_bound(p)?)
    } else {
        None
    };
    Ok(Some(WindowFrame { units, start, end }))
}

/// Reads one bound of a window frame.
///
/// `ROW` is no keyword of T-SQL (only `ROWS` is), so the `ROW` of `CURRENT ROW` arrives
/// as a plain identifier and is compared by text, without regard to case, exactly as
/// `Keyword::parse` compares a word.
///
/// # Errors
///
/// The syntax error of the token that spells no bound.
fn parse_frame_bound(p: &mut Parser) -> SqlResult<FrameBound> {
    if p.eat_keyword(Keyword::Unbounded) {
        if p.eat_keyword(Keyword::Preceding) {
            return Ok(FrameBound::UnboundedPreceding);
        }
        p.expect_keyword(Keyword::Following)?;
        return Ok(FrameBound::UnboundedFollowing);
    }
    if p.at_keyword(Keyword::Current) && at_word(p, 1, "ROW") {
        p.advance();
        p.advance();
        return Ok(FrameBound::CurrentRow);
    }
    let expr = Box::new(parse_value_expr(p)?);
    if p.eat_keyword(Keyword::Preceding) {
        return Ok(FrameBound::Preceding(expr));
    }
    p.expect_keyword(Keyword::Following)?;
    Ok(FrameBound::Following(expr))
}

/// Whether the token `n` ahead is the bare word `word`, whatever case it is written in.
fn at_word(p: &Parser, n: usize, word: &str) -> bool {
    matches!(
        &p.peek_at(n).kind,
        TokenKind::Ident { value, quoted: false } if value.eq_ignore_ascii_case(word)
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_CASE_NESTING, parse_expr, parse_expr_list, parse_value_expr, starts_a_value};
    use crate::ast::expr::{Expr, InList, Literal};
    use crate::parser::{ParseOptions, Parser};
    use vauban_errors::{SqlError, SqlResult};

    /// Builds a cursor over `text` with the default options.
    fn cursor(text: &str) -> Parser<'static> {
        static OPTIONS: ParseOptions = ParseOptions {
            quoted_identifier: true,
        };
        match Parser::new(text, &OPTIONS) {
            Ok(parser) => parser,
            Err(error) => unreachable!("the test text lexes: {error:?}"),
        }
    }

    /// Parses `text` as one expression in predicate position and checks that the whole
    /// text was read.
    fn e(text: &str) -> Expr {
        match try_e(text) {
            Ok(expr) => expr,
            Err(error) => unreachable!("{text} should parse: {error:?}"),
        }
    }

    /// Same as [`e`], without the panic: for the tests that expect an error.
    ///
    /// A text that parses but leaves tokens behind is an error too, and the error is the
    /// one its first leftover token would give inside a statement: that is what happens
    /// to `1 = 1` read in value position, where the loop stops on the `=`.
    fn try_e(text: &str) -> SqlResult<Expr> {
        let mut p = cursor(text);
        let expr = parse_expr(&mut p)?;
        if p.at_eof() {
            Ok(expr)
        } else {
            Err(p.error_here())
        }
    }

    /// Parses `text` as one expression in **value** position, as a select item or a call
    /// argument does, and checks that the whole text was read.
    fn try_v(text: &str) -> SqlResult<Expr> {
        let mut p = cursor(text);
        let expr = parse_value_expr(&mut p)?;
        if p.at_eof() {
            Ok(expr)
        } else {
            Err(p.error_here())
        }
    }

    /// Checks the `parse` -> `Display` -> `parse` loop on `text` and returns what
    /// `Display` wrote.
    fn roundtrip(text: &str) -> String {
        let first = e(text);
        let printed = first.to_string();
        assert_eq!(first, e(&printed), "{text} was printed as {printed}");
        printed
    }

    /// The error of a text that must not parse.
    fn error(text: &str) -> SqlError {
        match try_e(text) {
            Ok(expr) => unreachable!("{text} should not parse, got {expr:?}"),
            Err(error) => error,
        }
    }

    /// A parenthesised rendering of the tree, so that a precedence test reads like the
    /// vector it checks: `1 + 2 * 3` becomes `Add(1, Mul(2, 3))`.
    ///
    /// `Display` cannot be used for that: it adds no parenthesis, so `Add(1, Mul(2, 3))`
    /// and `Mul(Add(1, 2), 3)` both print `1 + 2 * 3`.
    fn shape(expr: &Expr) -> String {
        match expr {
            Expr::Literal(literal, _) => literal.to_string(),
            Expr::Column(column) => column.to_string(),
            Expr::Variable { name, .. } => name.clone(),
            Expr::Binary {
                op, left, right, ..
            } => format!("{op:?}({}, {})", shape(left), shape(right)),
            Expr::Unary { op, expr, .. } => format!("{op:?}({})", shape(expr)),
            Expr::Nested(inner, _) => format!("Nested({})", shape(inner)),
            Expr::IsNull { expr, negated, .. } => {
                let name = if *negated { "IsNotNull" } else { "IsNull" };
                format!("{name}({})", shape(expr))
            }
            Expr::In {
                expr,
                list,
                negated,
                ..
            } => {
                let name = if *negated { "NotIn" } else { "In" };
                format!("{name}({}, {list})", shape(expr))
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                ..
            } => {
                let name = if *negated { "NotLike" } else { "Like" };
                format!("{name}({}, {pattern})", shape(expr))
            }
            Expr::Between {
                expr,
                low,
                high,
                negated,
                ..
            } => {
                let name = if *negated { "NotBetween" } else { "Between" };
                format!("{name}({}, {low}, {high})", shape(expr))
            }
            Expr::Collate {
                expr, collation, ..
            } => format!("Collate({}, {collation})", shape(expr)),
            other => other.to_string(),
        }
    }

    #[test]
    fn precedence_arithmetic() {
        assert_eq!(shape(&e("1 + 2 * 3")), "Add(1, Mul(2, 3))");
        assert_eq!(shape(&e("2 * 3 + 1")), "Add(Mul(2, 3), 1)");
        // Left-associative.
        assert_eq!(shape(&e("8 / 4 / 2")), "Div(Div(8, 4), 2)");
        assert_eq!(shape(&e("10 % 3 * 2")), "Mul(Mod(10, 3), 2)");
    }

    /// In T-SQL, `&`, `^`, `|`, `+` and `-` are at rank 3, unlike C where the bitwise
    /// operators are far looser.
    #[test]
    fn precedence_bitwise_equals_additive() {
        assert_eq!(shape(&e("1 | 2 + 3")), "Add(BitOr(1, 2), 3)");
        assert_eq!(shape(&e("1 & 2 * 3")), "BitAnd(1, Mul(2, 3))");
        assert_eq!(shape(&e("1 + 2 | 3")), "BitOr(Add(1, 2), 3)");
        assert_eq!(shape(&e("1 ^ 2 & 3")), "BitAnd(BitXor(1, 2), 3)");
    }

    /// A unary `-` binds tighter than the binary operators of ranks 2 and 3. The two
    /// readings cannot be told apart on `*` (`-1 * 2` is `-2` either way), but they can on
    /// a rank-3 operator: `-1 & 2` is `2` under this rule and `0` under the other, and the
    /// `2` is what SQL Server returns.
    #[test]
    fn precedence_unary() {
        assert_eq!(shape(&e("~1 + 2")), "Add(BitNot(1), 2)");
        assert_eq!(shape(&e("-1 * 2")), "Mul(Minus(1), 2)");
        assert_eq!(shape(&e("-1 & 2")), "BitAnd(Minus(1), 2)");
        assert_eq!(
            shape(&e("NOT a = 1 AND b = 2")),
            "And(Not(Eq(a, 1)), Eq(b, 2))"
        );
        assert_eq!(
            shape(&e("NOT (a = 1 AND b = 2)")),
            "Not(Nested(And(Eq(a, 1), Eq(b, 2))))"
        );
    }

    /// The suffix predicates hang off rank 4, tighter than `NOT`, so `NOT a IN (…)`
    /// negates the whole predicate. `a NOT IN (…)` means the same thing and keeps a
    /// different tree, which is what lets `Display` write each one back as it was read.
    #[test]
    fn not_binds_looser_than_predicates() {
        assert_eq!(shape(&e("NOT a IN (1, 2)")), "Not(In(a, 1, 2))");
        assert_eq!(shape(&e("a NOT IN (1, 2)")), "NotIn(a, 1, 2)");
        assert_eq!(shape(&e("NOT a IS NULL")), "Not(IsNull(a))");
        assert_eq!(roundtrip("NOT a IN (1, 2)"), "NOT a IN (1, 2)");
        assert_eq!(roundtrip("a NOT IN (1, 2)"), "a NOT IN (1, 2)");
    }

    #[test]
    fn precedence_logical() {
        assert_eq!(
            shape(&e("a = 1 OR b = 2 AND c = 3")),
            "Or(Eq(a, 1), And(Eq(b, 2), Eq(c, 3)))"
        );
        assert_eq!(
            shape(&e("a = 1 AND b = 2 OR c = 3")),
            "Or(And(Eq(a, 1), Eq(b, 2)), Eq(c, 3))"
        );
    }

    #[test]
    fn precedence_comparison_below_arithmetic() {
        assert_eq!(shape(&e("1 + 1 = 2")), "Eq(Add(1, 1), 2)");
        assert_eq!(shape(&e("1 <> 2 * 3")), "Ne(1, Mul(2, 3))");
        // `!=` and `<>` are the same operator; `Display` writes `<>`.
        assert_eq!(shape(&e("1 != 2")), "Ne(1, 2)");
    }

    #[test]
    fn literals() {
        let literal = |text: &str| match &e(text) {
            Expr::Literal(literal, _) => literal.clone(),
            other => unreachable!("{text} is a literal, got {other:?}"),
        };
        assert_eq!(literal("1"), Literal::Integer("1".to_owned()));
        assert_eq!(literal("1.50"), Literal::Decimal("1.50".to_owned()));
        assert_eq!(literal("1.5E-2"), Literal::Float("1.5E-2".to_owned()));
        // The `0x` and the `$` are dropped here, not by the lexer.
        assert_eq!(literal("0x00FF"), Literal::Binary("00FF".to_owned()));
        assert_eq!(literal("$1.50"), Literal::Money("1.50".to_owned()));
        assert_eq!(literal("$-1.50"), Literal::Money("-1.50".to_owned()));
        assert_eq!(
            literal("'a''b'"),
            Literal::Str {
                value: "a'b".to_owned(),
                unicode: false,
            }
        );
        assert_eq!(
            literal("N'é'"),
            Literal::Str {
                value: "é".to_owned(),
                unicode: true,
            }
        );
        assert_eq!(literal("NULL"), Literal::Null);
        assert_eq!(literal("DEFAULT"), Literal::Default);
        // The source text is kept as written: `1.50` does not become `1.5`.
        for text in [
            "1", "1.50", "1.5E-2", "0x00FF", "$1.50", "$-1.50", "'a''b'", "N'é'", "NULL",
        ] {
            assert_eq!(roundtrip(text), text);
        }
    }

    /// The lexer only makes a `Money` token of `$[-|+]number`, so the `-` of `-$1.50` is
    /// the unary operator.
    #[test]
    fn unary_minus_before_money() {
        assert_eq!(shape(&e("-$1.50")), "Minus($1.50)");
        assert_eq!(roundtrip("-$1.50"), "-$1.50");
    }

    #[test]
    fn column_refs() {
        let parts = |text: &str| match &e(text) {
            Expr::Column(column) => (
                column.qualifier.as_ref().map(|q| q.to_string()),
                column.name.value.clone(),
                column.name.quoted,
            ),
            other => unreachable!("{text} is a column, got {other:?}"),
        };
        assert_eq!(parts("c"), (None, "c".to_owned(), false));
        assert_eq!(parts("t.c"), (Some("t".to_owned()), "c".to_owned(), false));
        assert_eq!(
            parts("dbo.t.c"),
            (Some("dbo.t".to_owned()), "c".to_owned(), false)
        );
        assert_eq!(
            parts("db.dbo.t.c"),
            (Some("db.dbo.t".to_owned()), "c".to_owned(), false)
        );
        assert_eq!(
            parts("[my db].dbo.[my t].[my c]"),
            (
                Some("[my db].dbo.[my t]".to_owned()),
                "my c".to_owned(),
                true
            )
        );
        for text in [
            "c",
            "t.c",
            "dbo.t.c",
            "db.dbo.t.c",
            "[my db].dbo.[my t].[my c]",
        ] {
            assert_eq!(roundtrip(text), text);
        }
        // The part right before the column cannot be empty: `a..c` names no table.
        assert_eq!(error("a..c").number, 102);
    }

    #[test]
    fn variables() {
        let name = |text: &str| match &e(text) {
            Expr::Variable { name, .. } => name.clone(),
            other => unreachable!("{text} is a variable, got {other:?}"),
        };
        assert_eq!(name("@x"), "@x");
        assert_eq!(name("@@ROWCOUNT"), "@@ROWCOUNT");
        assert_eq!(roundtrip("@x + 1"), "@x + 1");
    }

    #[test]
    fn functions() {
        // The name is read part by part rather than through `ObjectName::to_string`,
        // which brackets a reserved word: writing a function name bare is the business of
        // `Expr::Function`'s own `Display`, checked by the `roundtrip` below.
        let call = |text: &str| match &e(text) {
            Expr::Function {
                name,
                args,
                star,
                distinct,
                ..
            } => {
                let mut printed = String::new();
                if let Some(schema) = &name.schema {
                    printed.push_str(&schema.value);
                    printed.push('.');
                }
                printed.push_str(&name.name.value);
                (printed, args.len(), *star, *distinct)
            }
            other => unreachable!("{text} is a call, got {other:?}"),
        };
        assert_eq!(call("GETDATE()"), ("GETDATE".to_owned(), 0, false, false));
        assert_eq!(call("ISNULL(a, 0)"), ("ISNULL".to_owned(), 2, false, false));
        assert_eq!(call("COUNT(*)"), ("COUNT".to_owned(), 0, true, false));
        assert_eq!(
            call("COUNT(DISTINCT c)"),
            ("COUNT".to_owned(), 1, false, true)
        );
        assert_eq!(call("dbo.f(1)"), ("dbo.f".to_owned(), 1, false, false));
        // `LEFT` is reserved and is still a function name: the name of a call does not
        // go through `parse_ident`.
        assert_eq!(call("LEFT(a, 3)"), ("LEFT".to_owned(), 2, false, false));
        assert_eq!(call("RIGHT(a, 3)"), ("RIGHT".to_owned(), 2, false, false));
        assert_eq!(
            call("COALESCE(a, b)"),
            ("COALESCE".to_owned(), 2, false, false)
        );
        assert_eq!(call("NULLIF(a, b)"), ("NULLIF".to_owned(), 2, false, false));
        for text in [
            "GETDATE()",
            "ISNULL(a, 0)",
            "COUNT(*)",
            "COUNT(DISTINCT c)",
            "dbo.f(1)",
            "LEFT(a, 3)",
            "COALESCE(a, b)",
            "NULLIF(a, b)",
        ] {
            assert_eq!(roundtrip(text), text);
        }

        // The niladic functions written without parentheses: a one-part unquoted column
        // reference, which the binder recognises as a function.
        for text in [
            "CURRENT_TIMESTAMP",
            "CURRENT_USER",
            "SESSION_USER",
            "SYSTEM_USER",
            "USER",
        ] {
            match &e(text) {
                Expr::Column(column) => {
                    assert!(column.qualifier.is_none(), "{text} has no qualifier");
                    assert_eq!(column.name.value, text);
                    assert!(!column.name.quoted, "{text} is not quoted");
                }
                other => unreachable!("{text} is a niladic function, got {other:?}"),
            }
        }
    }

    /// A niladic function survives the `parse` -> `Display` -> `parse` loop: it is an
    /// `Expr::Column` whose name is a reserved word, and `display::ColumnRef` writes it
    /// bare instead of applying the safety rule that would bracket it (same mechanism as
    /// the function-name exception).
    ///
    /// The brackets the user wrote are a different tree and stay: `[CURRENT_TIMESTAMP]`
    /// is a quoted name, which the binder reads as a column, not as a function.
    #[test]
    fn niladic_roundtrips() {
        for text in [
            "CURRENT_TIMESTAMP",
            "CURRENT_USER",
            "SESSION_USER",
            "SYSTEM_USER",
            "USER",
        ] {
            assert_eq!(roundtrip(text), text);
        }
        assert_eq!(roundtrip("[CURRENT_TIMESTAMP]"), "[CURRENT_TIMESTAMP]");
        assert_ne!(e("CURRENT_TIMESTAMP"), e("[CURRENT_TIMESTAMP]"));
        // The exception is spelled on the name, not on the case it was written in.
        assert_eq!(roundtrip("current_timestamp"), "current_timestamp");
    }

    #[test]
    fn case_expressions() {
        let searched = "CASE WHEN 1 = 1 THEN 'a' WHEN 2 = 2 THEN 'b' ELSE 'c' END";
        match &e(searched) {
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                assert!(operand.is_none());
                assert_eq!(arms.len(), 2);
                assert!(else_.is_some());
            }
            other => unreachable!("{searched} is a CASE, got {other:?}"),
        }
        let simple = "CASE c WHEN 1 THEN 'a' END";
        match &e(simple) {
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                assert!(operand.is_some());
                assert_eq!(arms.len(), 1);
                assert!(else_.is_none());
            }
            other => unreachable!("{simple} is a CASE, got {other:?}"),
        }
        assert_eq!(roundtrip(searched), searched);
        assert_eq!(roundtrip(simple), simple);
        // `END` is reserved, so a 156 is reported on it.
        assert_eq!(error("CASE END").number, 156);
    }

    #[test]
    fn cast_and_convert() {
        match &e("CAST('1' AS int)") {
            Expr::Cast { ty, try_, .. } => {
                assert_eq!(ty.name, "int");
                assert!(!try_);
            }
            other => unreachable!("a CAST, got {other:?}"),
        }
        match &e("TRY_CAST(c AS decimal(18, 2))") {
            Expr::Cast { ty, try_, .. } => {
                assert_eq!(ty.name, "decimal");
                assert_eq!(ty.args.len(), 2);
                assert!(*try_);
            }
            other => unreachable!("a TRY_CAST, got {other:?}"),
        }
        // `CONVERT` writes the type first, the other way round from `CAST`.
        match &e("CONVERT(varchar(10), GETDATE(), 120)") {
            Expr::Convert {
                ty,
                expr,
                style,
                try_,
                ..
            } => {
                assert_eq!(ty.name, "varchar");
                assert_eq!(expr.to_string(), "GETDATE()");
                assert_eq!(
                    style.as_ref().map(|s| s.to_string()),
                    Some("120".to_owned())
                );
                assert!(!try_);
            }
            other => unreachable!("a CONVERT, got {other:?}"),
        }
        match &e("CONVERT(int, c)") {
            Expr::Convert { style, .. } => assert!(style.is_none()),
            other => unreachable!("a CONVERT, got {other:?}"),
        }
        match &e("TRY_CONVERT(varchar(max), c)") {
            Expr::Convert { ty, try_, .. } => {
                assert_eq!(ty.args.len(), 1);
                assert!(*try_);
            }
            other => unreachable!("a TRY_CONVERT, got {other:?}"),
        }
        for text in [
            "CAST('1' AS int)",
            "TRY_CAST(c AS decimal(18, 2))",
            "CONVERT(varchar(10), GETDATE(), 120)",
            "CONVERT(int, c)",
            "TRY_CONVERT(varchar(MAX), c)",
        ] {
            assert_eq!(roundtrip(text), text);
        }
    }

    #[test]
    fn predicates() {
        assert_eq!(shape(&e("c IS NULL")), "IsNull(c)");
        assert_eq!(shape(&e("c IS NOT NULL")), "IsNotNull(c)");
        assert_eq!(shape(&e("c IN (1, 2)")), "In(c, 1, 2)");
        assert_eq!(shape(&e("c LIKE 'a%'")), "Like(c, 'a%')");
        assert_eq!(shape(&e("c BETWEEN 1 AND 2")), "Between(c, 1, 2)");
        assert_eq!(shape(&e("c NOT BETWEEN 1 AND 2")), "NotBetween(c, 1, 2)");
        assert_eq!(
            shape(&e("c COLLATE Latin1_General_CI_AS")),
            "Collate(c, Latin1_General_CI_AS)"
        );
        match &e("c NOT LIKE 'a%' ESCAPE '!'") {
            Expr::Like {
                escape, negated, ..
            } => {
                assert!(*negated);
                assert_eq!(
                    escape.as_ref().map(|x| x.to_string()),
                    Some("'!'".to_owned())
                );
            }
            other => unreachable!("a LIKE, got {other:?}"),
        }
        match &e("c IN (1, 2)") {
            Expr::In { list, .. } => {
                assert!(matches!(list, InList::Exprs(items) if items.len() == 2))
            }
            other => unreachable!("an IN, got {other:?}"),
        }
        for text in [
            "c IS NULL",
            "c IS NOT NULL",
            "c IN (1, 2)",
            "c LIKE 'a%'",
            "c NOT LIKE 'a%' ESCAPE '!'",
            "c BETWEEN 1 AND 2",
            "c NOT BETWEEN 1 AND 2",
            "c COLLATE Latin1_General_CI_AS",
        ] {
            assert_eq!(roundtrip(text), text);
        }
        // `COLLATE` binds tighter than every operator.
        assert_eq!(
            shape(&e("c COLLATE Latin1_General_CI_AS = 'a'")),
            "Eq(Collate(c, Latin1_General_CI_AS), 'a')"
        );
    }

    /// The predicates that hold a subquery. What is checked here is the predicate; the shape
    /// of the query it holds is the business of `tests/select.rs`.
    #[test]
    fn subquery_predicates() {
        use crate::ast::expr::{BinaryOp, Quantifier};

        match &e("c NOT IN (SELECT 1)") {
            Expr::In { list, negated, .. } => {
                assert!(*negated);
                assert!(matches!(list, InList::Subquery(_)), "a subquery IN list");
            }
            other => unreachable!("a NOT IN, got {other:?}"),
        }
        match e("EXISTS (SELECT 1)") {
            Expr::Exists(..) => {}
            other => unreachable!("an EXISTS, got {other:?}"),
        }
        match e("(SELECT 1)") {
            Expr::Subquery(..) => {}
            other => unreachable!("a scalar subquery, got {other:?}"),
        }
        let quantified = [
            ("c > ALL (SELECT 1)", BinaryOp::Gt, Quantifier::All),
            ("c = ANY (SELECT 1)", BinaryOp::Eq, Quantifier::Any),
            // `SOME` is a synonym of `ANY` that the AST keeps as written.
            ("c = SOME (SELECT 1)", BinaryOp::Eq, Quantifier::Some),
        ];
        for (text, expected_op, expected_quantifier) in quantified {
            match e(text) {
                Expr::Quantified { op, quantifier, .. } => {
                    assert_eq!(op, expected_op, "{text}");
                    assert_eq!(quantifier, expected_quantifier, "{text}");
                }
                other => unreachable!("{text} is a quantified comparison, got {other:?}"),
            }
        }
        for text in [
            "c NOT IN (SELECT 1)",
            "EXISTS (SELECT 1)",
            "c > ALL (SELECT 1)",
            "c = ANY (SELECT 1)",
            "c = SOME (SELECT 1)",
            "(SELECT 1)",
        ] {
            assert_eq!(roundtrip(text), text);
        }
    }

    #[test]
    fn between_binds_tighter_than_and() {
        assert_eq!(
            shape(&e("a BETWEEN 1 AND 2 AND b = 3")),
            "And(Between(a, 1, 2), Eq(b, 3))"
        );
    }

    /// `(1 + 2)` is an `Expr::Nested` and `((1))` two of them, since `Display` neither
    /// adds nor removes a parenthesis. `(SELECT …)` is an `Expr::Subquery`, told apart by
    /// a single `peek_at(1)`; it is checked by `subquery_predicates`.
    #[test]
    fn subqueries() {
        assert_eq!(shape(&e("(1 + 2)")), "Nested(Add(1, 2))");
        assert_eq!(shape(&e("((1))")), "Nested(Nested(1))");
        assert_eq!(roundtrip("((1))"), "((1))");
        assert_eq!(roundtrip("(1 + 2) * 3"), "(1 + 2) * 3");
    }

    #[test]
    fn over_clause() {
        let partitioned = "ROW_NUMBER() OVER (PARTITION BY a, b ORDER BY c DESC)";
        match &e(partitioned) {
            Expr::Function { over, .. } => match over {
                Some(over) => {
                    assert_eq!(over.partition_by.len(), 2);
                    assert_eq!(over.order_by.len(), 1);
                    assert!(over.order_by[0].desc);
                    assert!(over.frame.is_none());
                }
                None => unreachable!("{partitioned} has an OVER clause"),
            },
            other => unreachable!("{partitioned} is a call, got {other:?}"),
        }
        let framed = "SUM(x) OVER (ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)";
        match &e(framed) {
            Expr::Function { over, .. } => match over {
                Some(over) => {
                    assert!(over.partition_by.is_empty());
                    assert_eq!(over.order_by.len(), 1);
                    assert!(over.frame.is_some());
                }
                None => unreachable!("{framed} has an OVER clause"),
            },
            other => unreachable!("{framed} is a call, got {other:?}"),
        }
        assert_eq!(roundtrip(partitioned), partitioned);
        assert_eq!(roundtrip(framed), framed);
        assert_eq!(
            roundtrip("SUM(x) OVER (ORDER BY d ROWS 3 PRECEDING)"),
            "SUM(x) OVER (ORDER BY d ROWS 3 PRECEDING)"
        );
    }

    /// `NEXT VALUE FOR` is V3: the parser accepts it, the binder refuses it.
    #[test]
    fn next_value_for() {
        match &e("NEXT VALUE FOR dbo.s") {
            Expr::NextValueFor { sequence, over, .. } => {
                assert_eq!(sequence.to_string(), "dbo.s");
                assert!(over.is_none());
            }
            other => unreachable!("a NEXT VALUE FOR, got {other:?}"),
        }
        assert_eq!(roundtrip("NEXT VALUE FOR dbo.s"), "NEXT VALUE FOR dbo.s");
        // `NEXT` alone is not reserved and stays a column name.
        assert_eq!(shape(&e("next")), "next");
    }

    #[test]
    fn expr_list_reads_values() {
        let mut p = cursor("1, 2, 3");
        let list = match parse_expr_list(&mut p) {
            Ok(list) => list,
            Err(error) => unreachable!("a list of literals: {error:?}"),
        };
        assert_eq!(list.len(), 3);
        assert!(p.at_eof());
    }

    #[test]
    fn expr_errors() {
        // The last token is the operator `+`; which token the message names at the end
        // of a batch is `syntax_error.rs`'s business.
        let incomplete = error("1 +");
        assert_eq!(incomplete.number, 102);
        assert_eq!(incomplete.line, 1);
        // `CAST(1 AS)`: the type is missing and the `)` gets the error.
        let no_type = error("CAST(1 AS)");
        assert_eq!(no_type.number, 102);
        assert_eq!(no_type.message, "Syntax error near ')'.");
        assert_eq!(no_type.line, 1);
        // `END` and `IN` are reserved: these are 156.
        for text in ["CASE END", "IN (1, 2)"] {
            let reserved = error(text);
            assert_eq!(reserved.number, 156, "{text}: {reserved:?}");
            assert_eq!(reserved.line, 1);
        }
        // The line of the offending token, not the line of the batch.
        let second_line = error("1 +\n*");
        assert_eq!(second_line.number, 102);
        assert_eq!(second_line.line, 2);
    }

    /// A predicate is not a scalar value: `SELECT (1 = 1)` is a syntax error in T-SQL,
    /// and so is a comparison used as an operand. The number and the token the message
    /// names are the ones SQL Server answers: 102, `Syntax error near '='.`.
    ///
    /// The refusal is the business of the **value** entry point, since the same
    /// parentheses hold a predicate in `WHERE (a = 1 AND b = 2)`, which must keep
    /// parsing; the vector `e("(1 = 1)")` is written here on
    /// [`parse_value_expr`] for that reason, and the search-condition reading is asserted
    /// right after it.
    #[test]
    fn predicate_is_not_a_value() {
        // The offending token is the `=` of the two first, and the reserved `NOT` of
        // the third: 102 and 156.
        for (text, number) in [("(1 = 1)", 102), ("1 + (1 = 1)", 102), ("NOT 1", 156)] {
            match try_v(text) {
                Ok(expr) => unreachable!("{text} is no value, got {expr:?}"),
                Err(error) => assert_eq!(error.number, number, "{text}: {error:?}"),
            }
        }
        let in_operand = error("1 + (1 = 1)");
        assert_eq!(in_operand.number, 102, "{in_operand:?}");
        // In predicate position the same parentheses hold a search condition, which is
        // what `WHERE (a = 1 AND b = 2)` needs.
        assert_eq!(shape(&e("(1 = 1)")), "Nested(Eq(1, 1))");
    }

    /// A value position also refuses the words of rank 5 to 7, and a prefix `NOT` with
    /// them.
    #[test]
    fn value_position_refuses_logical_operators() {
        for text in ["1 = 1", "a AND b", "a OR b", "a IS NULL", "a IN (1)"] {
            assert!(try_v(text).is_err(), "{text} is no value");
        }
        // What a value position does accept.
        for text in [
            "1 + 2",
            "-1",
            "~1",
            "f(1)",
            "(1 + 2)",
            "CASE WHEN a = 1 THEN 2 END",
        ] {
            assert!(try_v(text).is_ok(), "{text} is a value");
        }
    }

    /// A unary operator applies to the primary that follows it, and `NOT` may be
    /// repeated.
    #[test]
    fn prefix_operators_stack() {
        assert_eq!(shape(&e("- -1")), "Minus(Minus(1))");
        assert_eq!(shape(&e("NOT NOT a = 1")), "Not(Not(Eq(a, 1)))");
        assert_eq!(shape(&e("+1 + 2")), "Add(Plus(1), 2)");
    }

    /// A comparison yields a search condition, and a search condition is not a value on
    /// the left of an operator any more than it is on its right: `a = b = c` is a syntax
    /// error in T-SQL, not `Eq(Eq(a, b), c)` as in C. Same refusal for a chained rank-4
    /// operator and for a chained suffix predicate.
    ///
    /// A reserved word such as `IN` is a 156, hence the check on the message rather than
    /// on the number alone. The two vectors marked below answer a 102 through [`try_e`], which
    /// enters the grammar in **search-condition** position and stops on the leftover `=`;
    /// what a client sees is the **value** position of a select item, and there
    /// `parse_batch` answers the same 156 as SQL Server.
    #[test]
    fn chained_comparison_is_a_syntax_error() {
        let chained_eq = error("a = b = c");
        assert_eq!(chained_eq.number, 102);
        assert_eq!(chained_eq.message, "Syntax error near '='.");
        let chained_lt = error("1 < 2 < 3");
        assert_eq!(chained_lt.number, 102);
        assert_eq!(chained_lt.message, "Syntax error near '<'.");
        // The number follows the token the message names: the reserved `IN` of the
        // second predicate gives a 156, the chained `=` a 102. On SQL Server,
        // `SELECT 1 IN (1) IN (1)` answers 156 and `SELECT 1 = 1 = 1` a 102. The first
        // vector below matches that; the next two do not.
        for (text, number) in [
            ("1 IN (1) IN (1)", 156),
            // Search-condition entry point: the predicate parses and the leftover `=`
            // gives the 102. In value position `SELECT 1 IN (1) = 1` answers 156 `near
            // the keyword 'IN'`, which is what SQL Server answers too.
            ("1 IN (1) = 1", 102),
            // Same pair: `SELECT 1 IS NULL = 1` answers 156 `near the keyword 'IS'` on
            // both sides.
            ("a IS NULL = 1", 102),
        ] {
            let chained = error(text);
            assert_eq!(chained.number, number, "{text}: {chained:?}");
        }
        let chained_in = error("1 IN (1) IN (1)");
        assert!(
            chained_in.message.contains("'IN'"),
            "the second IN gets the error, got {}",
            chained_in.message
        );
        // What stays legal: the words of rank 5 to 7 do follow a search condition.
        for text in [
            "a = 1 AND b = 2",
            "a = 1 OR b = 2",
            "NOT a = 1",
            "a BETWEEN 1 AND 2 AND b = 3",
            "a IS NULL AND b IS NOT NULL",
        ] {
            assert!(try_e(text).is_ok(), "{text} is a search condition");
        }
    }

    /// `EXISTS (…)` is a search condition, never a value: `SELECT EXISTS (SELECT 1)` is a
    /// syntax error in T-SQL while `WHERE EXISTS (SELECT 1)` is not. The error itself is
    /// a 156 on the reserved `EXISTS`, as on SQL Server.
    ///
    /// In predicate position the parse must reach the `SELECT` inside: what is asserted is
    /// that the refusal did **not** happen.
    #[test]
    fn exists_is_not_a_value() {
        let as_value = match try_v("EXISTS (SELECT 1)") {
            Ok(expr) => unreachable!("EXISTS is no value, got {expr:?}"),
            Err(error) => error,
        };
        assert_eq!(as_value.number, 156, "{as_value:?}");
        assert!(
            as_value.message.contains("'EXISTS'"),
            "the EXISTS gets the error, got {}",
            as_value.message
        );
        // An operand is a value too, `1 + EXISTS (…)` included.
        let as_operand = error("1 + EXISTS (SELECT 1)");
        assert_eq!(as_operand.number, 156, "{as_operand:?}");
        if let Err(condition) = try_e("EXISTS (SELECT 1)") {
            assert_eq!(
                condition.number, 50000,
                "in predicate position EXISTS reaches the SELECT, got {condition:?}"
            );
        }
        if let Err(negated) = try_e("NOT EXISTS (SELECT 1)") {
            assert_eq!(negated.number, 50000, "NOT EXISTS too, got {negated:?}");
        }
    }

    // ---- The `CASE` limit ----
    //
    // The shapes below follow SQL Server; the corners stated on this parser alone are
    // named where they are asserted.

    /// The five positions a `CASE` can stand in inside another `CASE`, each as a format
    /// with `{}` for the inner expression.
    const CASE_POSITIONS: [(&str, &str, u8); 5] = [
        ("THEN", "CASE WHEN 1 = 1 THEN {} ELSE 0 END", 4),
        ("ELSE", "CASE WHEN 1 = 1 THEN 0 ELSE {} END", 4),
        ("searched WHEN", "CASE WHEN {} = 1 THEN 1 ELSE 0 END", 4),
        ("operand", "CASE {} WHEN 1 THEN 1 ELSE 0 END", 3),
        ("simple WHEN", "CASE 1 WHEN {} THEN 1 ELSE 0 END", 3),
    ];

    /// Wraps `inner` in `levels` copies of `shape`, innermost first.
    fn nest(shape: &str, levels: u32, inner: &str) -> String {
        let mut text = inner.to_owned();
        for _ in 0..levels {
            text = shape.replacen("{}", &text, 1);
        }
        text
    }

    /// The error of a whole batch, which is what a subquery needs.
    fn batch_error(text: &str) -> SqlError {
        match crate::parse_batch(text, &ParseOptions::default()) {
            Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
            Err(error) => error,
        }
    }

    /// Checks that `error` is the 125 of the eleventh `CASE`, with `state` and `line`.
    fn assert_is_125(error: &SqlError, state: u8, line: u32, what: &str) {
        assert_eq!(
            (error.number, error.severity, error.state, error.line),
            (125, 15, state, line),
            "{what}: {error:?}"
        );
        assert_eq!(
            error.message, "A CASE expression cannot be nested deeper than level 10.",
            "{what}"
        );
    }

    /// Ten levels pass and eleven answer 125 in the five positions, with the state of the
    /// eleventh: 4 in the three positions of a searched `CASE`, 3 in the two of a simple
    /// one.
    #[test]
    fn ten_nested_cases_pass_and_eleven_answer_125_in_the_five_positions() {
        assert_eq!(MAX_CASE_NESTING, 10);
        for (name, shape, state) in CASE_POSITIONS {
            let ten = nest(shape, 10, "1");
            assert!(try_v(&ten).is_ok(), "ten in the {name}: {:?}", try_v(&ten));
            let eleven = nest(shape, 11, "1");
            assert_is_125(&error(&eleven), state, 1, &format!("eleven in the {name}"));
            let twenty = nest(shape, 20, "1");
            assert_is_125(&error(&twenty), state, 1, &format!("twenty in the {name}"));
        }
    }

    /// A parenthesis, a call, a `CAST`, an operator or a scalar subquery between two
    /// `CASE`s neither resets nor adds a level: ten pass, eleven answer 125.
    #[test]
    fn the_case_count_follows_the_syntax_through_what_stands_between_two_cases() {
        let through = [
            ("parenthesis", "CASE WHEN 1 = 1 THEN ({}) ELSE 0 END"),
            ("call", "CASE WHEN 1 = 1 THEN ABS({}) ELSE 0 END"),
            ("CAST", "CASE WHEN 1 = 1 THEN CAST({} AS int) ELSE 0 END"),
            ("operator", "CASE WHEN 1 = 1 THEN 1 + {} ELSE 0 END"),
            ("subquery", "CASE WHEN 1 = 1 THEN (SELECT {}) ELSE 0 END"),
        ];
        for (name, shape) in through {
            let ten = format!("SELECT {}", nest(shape, 10, "1"));
            let parsed = crate::parse_batch(&ten, &ParseOptions::default());
            assert!(parsed.is_ok(), "ten through a {name}: {parsed:?}");
            let eleven = format!("SELECT {}", nest(shape, 11, "1"));
            assert_is_125(
                &batch_error(&eleven),
                4,
                1,
                &format!("eleven through a {name}"),
            );
        }
        // One CASE, a subquery, then ten: the count goes on through the subquery.
        let one_over_ten = format!(
            "SELECT CASE WHEN 1 = 1 THEN (SELECT {}) ELSE 0 END",
            nest(CASE_POSITIONS[0].1, 10, "1")
        );
        assert_is_125(
            &batch_error(&one_over_ten),
            4,
            1,
            "one CASE over a subquery of ten",
        );
    }

    /// The state is the one of the eleventh `CASE`, counted from the outside: not the
    /// outermost's, not the innermost's.
    #[test]
    fn the_state_of_125_names_the_eleventh_case() {
        let searched = CASE_POSITIONS[0].1;
        let simple = CASE_POSITIONS[3].1;
        // Ten searched over a simple eleventh: 3.
        let inner_simple = nest(searched, 10, &nest(simple, 1, "1"));
        assert_is_125(
            &error(&inner_simple),
            3,
            1,
            "simple eleventh under ten searched",
        );
        // A simple outermost over ten searched: the eleventh is searched, 4.
        let outer_simple = nest(simple, 1, &nest(searched, 10, "1"));
        assert_is_125(
            &error(&outer_simple),
            4,
            1,
            "simple outermost, searched eleventh",
        );
        // Twelve deep, simple eleventh, searched twelfth: 3 -- the innermost does not decide.
        let simple_eleventh = nest(searched, 10, &nest(simple, 1, &nest(searched, 1, "1")));
        assert_is_125(
            &error(&simple_eleventh),
            3,
            1,
            "simple eleventh, searched twelfth",
        );
        // Twelve deep, searched eleventh, simple twelfth: 4.
        let simple_twelfth = nest(searched, 11, &nest(simple, 1, "1"));
        assert_is_125(
            &error(&simple_twelfth),
            4,
            1,
            "searched eleventh, simple twelfth",
        );
    }

    /// The line of the error is the one of the eleventh `CASE` keyword, not the one of
    /// its `WHEN`, of its `END` or of the statement.
    #[test]
    fn error_125_is_on_the_line_of_the_eleventh_case_keyword() {
        let mut lines = vec!["SELECT".to_owned()];
        lines.extend((0..12).map(|_| "CASE WHEN 1 = 1 THEN".to_owned()));
        lines.push("1".to_owned());
        lines.extend((0..12).map(|_| "ELSE 0 END".to_owned()));
        let text = lines.join("\n");
        // `SELECT` is line 1, the first `CASE` line 2, the eleventh line 12.
        assert_is_125(&batch_error(&text), 4, 12, "one CASE per line");
        // The keyword alone on its line: the `WHEN` on the next one does not move it.
        let keyword_alone = format!(
            "SELECT {}",
            nest(
                CASE_POSITIONS[0].1,
                10,
                "CASE\nWHEN 1 = 1 THEN 1\nELSE 0 END"
            )
        );
        assert_is_125(
            &batch_error(&keyword_alone),
            4,
            1,
            "keyword on line 1, WHEN on line 2",
        );
        let simple_alone = format!(
            "SELECT 1,\n{}",
            nest(CASE_POSITIONS[0].1, 10, "CASE\n1 WHEN 1 THEN 1\nELSE 0 END")
        );
        assert_is_125(
            &batch_error(&simple_alone),
            3,
            2,
            "simple keyword on line 2",
        );
    }

    /// The eleventh `CASE` is refused on its keyword and one token of lookahead: what is
    /// inside it or after it is not read, what is before it has already been.
    #[test]
    fn the_eleventh_case_is_refused_on_its_keyword_and_one_token() {
        let ten = |inner: &str| nest(CASE_POSITIONS[0].1, 10, inner);
        // A syntax error inside the eleventh, in the searched and in the simple form.
        for (inner, state) in [
            ("CASE WHEN 1 + THEN 1 END", 4),
            ("CASE WHEN END", 4),
            ("CASE WHEN 1 = 1 THEN END", 4),
            ("CASE 1 + WHEN 1 THEN 1 END", 3),
            ("CASE ( END", 3),
            ("CASE 1 2 END", 3),
            ("CASE - END", 3),
            ("CASE + 1 WHEN 1 THEN 1 END", 3),
            ("CASE CASE END END", 3),
            ("CASE WHEN CASE END THEN 1 END", 4),
        ] {
            assert_is_125(&error(&ten(inner)), state, 1, inner);
        }
        // A syntax error after the eleventh: 125 first.
        let trailing = format!("SELECT {} + ;", ten("CASE WHEN 1 = 1 THEN 1 END"));
        assert_is_125(&batch_error(&trailing), 4, 1, "trailing syntax error");
        // A syntax error before it wins.
        let leading = format!("SELECT 1 +, {}", ten("CASE WHEN 1 = 1 THEN 1 END"));
        let before = batch_error(&leading);
        assert_eq!(before.number, 102, "{before:?}");
        // A token that opens neither a WHEN nor a value: the error of depth one.
        for inner in [
            "CASE END",
            "CASE THEN 1 END",
            "CASE ELSE 1 END",
            "CASE , 1 END",
            "CASE ) END",
            "CASE ; END",
            "CASE = 1 END",
            "CASE SELECT 1 END",
            "CASE FROM END",
            "CASE * END",
            "CASE NOT 1 = 1 THEN 1 END",
            "CASE EXISTS (SELECT 1) WHEN 1 THEN 1 END",
        ] {
            let shallow = error(inner);
            let deep = error(&ten(inner));
            assert_eq!(
                shallow, deep,
                "{inner}: depth eleven answers what depth one does"
            );
            assert!(matches!(shallow.number, 102 | 156), "{inner}: {shallow:?}");
        }
    }

    /// The count is a depth, not a total: siblings do not add up.
    #[test]
    fn sibling_cases_do_not_add_up() {
        let searched = CASE_POSITIONS[0].1;
        let ten = nest(searched, 10, "1");
        let siblings = format!("SELECT {ten}, {ten}");
        let parsed = crate::parse_batch(&siblings, &ParseOptions::default());
        assert!(parsed.is_ok(), "two chains of ten side by side: {parsed:?}");
        let nine = nest(searched, 9, "1");
        let both_branches = format!("CASE WHEN 1 = 1 THEN {nine} ELSE {nine} END");
        assert!(
            try_v(&both_branches).is_ok(),
            "nine in both branches: {:?}",
            try_v(&both_branches)
        );
        // Two chains of ten in the two branches of an eleventh: still a depth of eleven.
        let over_both = format!("CASE WHEN 1 = 1 THEN {ten} ELSE {ten} END");
        assert_is_125(&error(&over_both), 4, 1, "ten in both branches of one");
    }

    /// A `CASE` refused at the limit, or failed inside, leaks no level on the counter.
    #[test]
    fn a_refused_case_leaks_no_level() {
        let mut p = cursor(&nest(CASE_POSITIONS[0].1, 11, "1"));
        assert!(parse_value_expr(&mut p).is_err());
        assert_eq!(p.case_depth(), 0, "after the 125");
        let mut p = cursor("CASE WHEN 1 = 1 THEN CASE WHEN 1 + THEN 1 END END");
        assert!(parse_value_expr(&mut p).is_err());
        assert_eq!(p.case_depth(), 0, "after a syntax error inside");
    }

    /// `starts_a_value` says what the grammar does: it answers `true` exactly when the
    /// operand parse of a simple `CASE` gets past its first token.
    ///
    /// The grammar's verdict is read on `CASE <operand> WHEN 1 THEN 1 END` at depth one:
    /// a parse, or an error that names another token than the first, means the first
    /// token was accepted. The operands are chosen so that their first token is not
    /// written a second time in the text.
    #[test]
    fn starts_a_value_agrees_with_the_grammar() {
        let operands = [
            "1",
            "1.5",
            "1e3",
            "$1",
            "0x00",
            "'a'",
            "N'a'",
            "@x",
            "@@rowcount",
            "col",
            "[col]",
            "dbo.col",
            "(1)",
            "( END",
            "1 +",
            "- 1",
            "+ 1",
            "~ 1",
            "- END",
            "NULL",
            "DEFAULT",
            "CASE 2 WHEN 2 THEN 3 END",
            "CAST(2 AS int)",
            "TRY_CAST(2 AS int)",
            "CONVERT(int, 2)",
            "TRY_CONVERT(int, 2)",
            "CURRENT_TIMESTAMP",
            "CURRENT_DATE",
            "USER",
            "NEXT VALUE FOR s",
            "LEFT('a', 2)",
            "COALESCE(2, 3)",
            "LEFT",
            "IN",
            "EXISTS (SELECT 2)",
            "NOT 2 = 2",
            "SELECT",
            "FROM",
            "END",
            "THEN",
            "ELSE",
            ",",
            ")",
            ";",
            "=",
            "*",
            "/",
            "\\",
            "{",
        ];
        let mut accepted = 0;
        for operand in operands {
            let mut p = cursor(operand);
            let first = p.peek().text.clone();
            let lookahead = starts_a_value(&mut p);
            assert_eq!(p.mark(), 0, "{operand}: the lookahead consumes nothing");
            let grammar = match try_v(&format!("CASE {operand} WHEN 1 THEN 1 END")) {
                Ok(_) => true,
                Err(error) => !error.message.contains(&format!("'{first}'")),
            };
            assert_eq!(
                lookahead, grammar,
                "{operand}: lookahead against the grammar"
            );
            accepted += usize::from(lookahead);
        }
        assert_eq!(
            accepted,
            32,
            "starters among the {} operands",
            operands.len()
        );
    }
}
