//! Operators, predicates, `CASE` and `COLLATE`: typing and errors.
//!
//! [`bind_expr`] gives a type to the scalar expressions the parser builds out of
//! operators, predicates, `CASE`, `COLLATE` and parentheses; [`bind_condition`] binds the
//! same grammar in a place where SQL Server wants a *condition* and raises error 4145 when
//! it does not get one.
//!
//! # What this module decides, and what it only relays
//!
//! Not one typing rule lives here. `types::binary_op_type` answers "what is the type of
//! `a * b`?" and `types::implicit_result_type` answers "what is the common type of `a`
//! and `b`?"; this module calls them, inserts the [`BoundExprKind::Convert`] nodes their
//! answers imply, and adds the line of the AST node to the errors they raise. Its own
//! decisions are four:
//!
//! - `+` between two **character** operands is a concatenation, so it becomes
//!   [`BinaryOp::Concat`] before `types` sees it (the parser writes `Add` for both);
//! - an **integer literal** engaged with an exact numeric enters the precision table with
//!   its own number of digits — see [`numeric_view`];
//! - `BETWEEN` and the simple `CASE` are **desugared**, so that the executor has one shape
//!   to evaluate instead of three;
//! - the unary operators, which `types` does not type, are typed here, by what SQL Server
//!   answers on each of them.
//!
//! # Explicit conversions
//!
//! After binding, the executor decides no conversion: an implicit one is a
//! `Convert { style: None, try_: false }` node whose `ty` is the target type. A conversion
//! is inserted where the operand type differs from the target, so `1 = 1` carries no
//! conversion and `1 = '1'` carries one, on the right operand
//! (`comparison_inserts_the_conversion`).
//!
//! The target is not the same question on both sides of the fence. A comparison, `IN`,
//! `LIKE` and `CASE` bring their operands to the **common type**, because that is the type
//! at which they compare or return. An arithmetic or bitwise operator brings there every
//! operand of another family too, but spares one group: when the common type is an exact
//! numeric and the operand carries a precision of its own — an `int`, a `money` —, it moves
//! only as far as that precision reads, because the common type would lose digits SQL
//! Server keeps. Outside the exact numerics nothing is spared, an integer no more than the
//! rest: `SELECT CAST(0 AS smallmoney) * CAST(3000000 AS int);` answers an arithmetic
//! overflow for `smallmoney` on the value 3000000 although the product is 0, so the server
//! did move the `int` to the `smallmoney` the pair has in common. [`arith_target`] holds
//! the rule, the line it draws between the two groups, and the queries that fix both.
//!
//! # Depth
//!
//! [`bind_expr`] is the one door into a child node, so it is where `depth.rs` counts the
//! levels of the tree: a text that is flat for the parser — a chain of operators of the
//! same precedence — still builds a comb the binder descends one frame per term.
//!
//! # Wiring to `call.rs`
//!
//! `CAST`, `CONVERT`, a function call, a `@@x` variable and a bare niladic name are typed
//! by `call.rs`: five branches of this file route to `call::bind_cast`,
//! `call::bind_convert`, `call::bind_function`, `call::bind_variable_function` and
//! `call::bind_niladic` — the last one *before* error 207 is raised, see [`bind_column`] —
//! and do nothing else. Not one rule of `call.rs` lives here.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{
    BinaryOp as AstBinaryOp, CaseArm, ColumnRef, Expr, Ident, InList, ObjectName, Span, UnaryOp,
};
use vauban_types::{
    BinaryOp, Collation, Len, SqlType, TypeFamily, TypeInfo, Value, binary_op_type,
    implicit_result_type,
};

use crate::bound::{BoundCaseArm, BoundExpr, BoundExprKind, CompareOp, LogicalOp};
use crate::call::{
    bind_cast, bind_convert, bind_function, bind_niladic, bind_variable_function, is_untyped_null,
};
use crate::context::BindContext;
use crate::depth;
use crate::errors::{line_at, line_of, trivia_len};
use crate::literal::bind_literal;
use crate::star::{Lookup, Source, lookup};
use crate::subquery;

/// The names a bound expression may refer to besides the batch variables.
///
/// Without a `FROM` it is empty: there is no column to resolve, and
/// `BindContext::variables` already holds the batch variables. A `FROM` fills it with its
/// source, and it is threaded through the functions of this module and of `call.rs`, so
/// that an argument of a call sees the columns its enclosing select list sees
/// (`SELECT ABS(b) FROM dbo.t` answers the row).
///
/// Two places bind an expression against the **empty** scope even under a `FROM`:
///
/// - the arguments glued to a name in the `FROM` (`t (id)` answers 207 naming a column of
///   `t` itself);
/// - the row count of a `TOP` (`SELECT TOP (a) b FROM dbo.t` answers **4115** on SQL
///   Server, which `vauban_errors` does not carry: VaubanDB answers 207 there, a
///   deliberate difference).
///
/// The value of a `SELECT @x = e` is bound by `variables.rs`, without a `FROM`, against
/// the batch variables alone; the form with a `FROM` is not bound yet.
///
/// # The shape a join and a subquery need
///
/// - **several sources**, for the `FROM` of a join: `sources` is a list, in the order the
///   references were written, and a qualifier is matched against each of them. A join
///   raises the 209 of an unqualified column two of them carry;
/// - a **parent**, for a correlated subquery: a name the inner query does not carry is
///   looked up in the scope of the query that encloses it. The link owns its parent rather
///   than borrowing it, which keeps `Scope` free of a lifetime parameter and `call.rs`
///   free of a signature change.
// Constructed by the tests below, by `query.rs` and by `call.rs`, which binds the operands
// of a call through [`bind_expr`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    /// The sources a `FROM` puts in scope, in the order they were written.
    sources: Vec<Source>,
    /// The scope of the enclosing query, for a correlated subquery; `None` at the top.
    parent: Option<Box<Scope>>,
}

impl Scope {
    /// The scope of a statement with nothing in it: no `FROM`, or a place a column may not
    /// be named.
    pub(crate) fn empty() -> Self {
        Scope::default()
    }

    /// The scope of a `FROM` over one source.
    pub(crate) fn over(source: Source) -> Self {
        Scope {
            sources: vec![source],
            parent: None,
        }
    }

    /// The scope of a `FROM` over the sources of a join, in written order (`join.rs`).
    pub(crate) fn over_all(sources: Vec<Source>) -> Self {
        Scope {
            sources,
            parent: None,
        }
    }

    /// The same scope, nested inside `parent`: the scope a correlated subquery binds
    /// against.
    pub(crate) fn inside(self, parent: Scope) -> Self {
        Scope {
            sources: self.sources,
            parent: Some(Box::new(parent)),
        }
    }

    /// The sources in scope, in written order: what `star.rs` resolves a column against
    /// and expands a `*` over. Empty without a `FROM`.
    pub(crate) fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// The scope of the enclosing query, `None` at the top.
    pub(crate) fn parent(&self) -> Option<&Scope> {
        self.parent.as_deref()
    }
}

/// Binds a scalar expression, or a predicate, into a typed node of the bound plan.
///
/// The `match` has no `_ =>` arm: a variant added to `parser::Expr` must break this file
/// rather than be bound by accident.
///
/// # Errors
///
/// A user error is the `SqlError` SQL Server raises, with its number and the line of the
/// node: 207 for an unknown column, 137 for an undeclared variable, 206/257/402/8117 for
/// an operand type an operator refuses, 447 for a `COLLATE` on a non-string, 448 for an
/// unknown collation, 4145 for a value where a condition was expected, and whatever
/// `call.rs` raises for a conversion or a call (195, 243, 529, 4121…). What is not bound
/// yet — subqueries, `EXISTS`, quantified comparisons, `NEXT VALUE FOR` — is an internal
/// error 50000, not a user-facing message.
pub(crate) fn bind_expr(e: &Expr, ctx: &BindContext<'_>, scope: &Scope) -> SqlResult<BoundExpr> {
    // The descents of the binder into a child node pass here, so counting the levels
    // here counts the depth of the tree, flat text or not.
    // The guard is bound to a name: `let _ = …` would drop it at once and count nothing.
    let _depth_guard = depth::enter()?;
    match e {
        Expr::InvalidNiladic {
            diagnostic_token,
            diagnostic_number,
            diagnostic_span,
            ..
        } => Err(if *diagnostic_number == 156 {
            SqlError::incorrect_syntax_near_keyword(diagnostic_token, diagnostic_span.line)
        } else {
            SqlError::incorrect_syntax_near(diagnostic_token, diagnostic_span.line)
        }),
        Expr::Literal(literal, span) => bind_literal(literal, span),
        // Parentheses carry no meaning once the tree is built: the AST keeps them for
        // `Display`, the bound plan does not need them.
        Expr::Nested(inner, _) => bind_expr(inner, ctx, scope),
        Expr::Column(column) => bind_column(column, scope),
        Expr::Variable { name, span } => bind_variable(name, span, ctx),
        Expr::Binary {
            op,
            op_span,
            left,
            right,
            span,
        } => bind_binary(*op, op_span, left, right, span, ctx, scope),
        Expr::Unary { op, expr, span } => bind_unary(*op, expr, span, ctx, scope),
        Expr::IsNull {
            expr,
            negated,
            span,
        } => bind_is_null(expr, *negated, span, ctx, scope),
        Expr::In {
            expr,
            list,
            negated,
            span,
        } => bind_in(expr, list, *negated, span, ctx, scope),
        Expr::Like {
            expr,
            pattern,
            escape,
            negated,
            span,
        } => bind_like(expr, pattern, escape.as_deref(), *negated, span, ctx, scope),
        Expr::Between {
            expr,
            low,
            high,
            negated,
            span,
        } => bind_between(expr, low, high, *negated, span, ctx, scope),
        Expr::Case {
            operand,
            arms,
            else_,
            span,
        } => bind_case(operand.as_deref(), arms, else_.as_deref(), span, ctx, scope),
        Expr::Collate {
            expr,
            collation,
            collation_span,
            span,
        } => bind_collate(expr, collation, collation_span, span, ctx, scope),
        // `call.rs`: the target type, the registry lookup and the arity are its business,
        // this file routes and nothing more.
        Expr::Cast { .. } => bind_cast(e, ctx, scope),
        Expr::Convert { .. } => bind_convert(e, ctx, scope),
        Expr::Function { args, .. } => {
            // A niladic syntax diagnostic is reached through the arguments before
            // resolving the enclosing function, preserving earlier variables.
            if args.iter().any(contains_invalid_niladic) {
                for arg in args {
                    bind_expr(arg, ctx, scope)?;
                }
            }
            bind_function(e, ctx, scope)
        }
        // The relational forms, which need a bound `LogicalPlan` inside an expression:
        // `subquery.rs`. This file routes, and binds none of them.
        Expr::Exists(query, _) => subquery::bind_exists(query, ctx, scope),
        Expr::Subquery(query, _) => subquery::bind_scalar(query, ctx, scope),
        // `ALL`, `ANY` and `SOME` are not routed to `subquery.rs`: the refusal names the
        // form.
        Expr::Quantified { .. } => Err(not_yet(
            "bind_expr: ALL/ANY/SOME over a subquery is not implemented yet",
        )),
        Expr::NextValueFor { .. } => {
            Err(not_yet("bind_expr: NEXT VALUE FOR is not implemented yet"))
        }
        // `SELECT @x = e` is a statement-level form: `query.rs` binds it as an
        // assignment, not as a value.
        Expr::Assign { .. } => Err(not_yet(
            "bind_expr: SELECT @x = e is an assignment, not an expression",
        )),
        Expr::Placeholder(..) => Err(not_yet(
            "bind_expr: the ODBC `?` placeholder is not implemented yet",
        )),
    }
}

/// Binds an expression in a place where SQL Server wants a condition: `WHERE`, `HAVING`,
/// `ON`, `WHEN`, `IF`, `AND`, `OR`, `NOT`.
///
/// T-SQL has no boolean type, so the check is on the **variant**
/// ([`BoundExpr::is_predicate`]) and not on the type: `WHERE 1` is refused although `1` is
/// a perfectly good `bit`-compatible value.
///
/// The opposite direction — a predicate used as a value, `SELECT (1 = 1)` — does not reach
/// the binder: the parser rejects it as a syntax error, like SQL Server. There is
/// therefore no branch for it here.
///
/// # Errors
///
/// Error 4145, severity 15, quoting the token that **follows** the expression, which is
/// what SQL Server echoes — see [`near_token`]. A condition that opens on a parenthesised
/// niladic spelling answers 4145 too, without reaching [`bind_expr`], see
/// [`leading_niladic_call`].
pub(crate) fn bind_condition(
    e: &Expr,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    if let Some(opening) = leading_niladic_call(e) {
        return Err(SqlError::non_boolean_expression("(").with_line(opening.line));
    }
    let bound = bind_expr(e, ctx, scope)?;
    if bound.is_predicate() {
        return Ok(bound);
    }
    let span = span_of(e);
    let (token, line) = near_token_at(ctx.text, &span);
    Err(SqlError::non_boolean_expression(token).with_line(line))
}

/// The call parenthesis of a parenthesised niladic spelling that **opens** a condition.
///
/// SQL Server cuts such a condition at the niladic name: `CURRENT_TIMESTAMP` alone is not a
/// predicate, so 4145 quotes the token that follows it, the `(`, exactly as [`near_token_at`]
/// describes for the conditions written by hand: in `SELECT 1 WHERE {x}=1;` for `{x}`
/// written `CURRENT_TIMESTAMP()`, `CURRENT_TIMESTAMP(1)`, `[CURRENT_TIMESTAMP]()` and
/// `[CURRENT_DATE]()`, and in `SELECT CASE WHEN CURRENT_TIMESTAMP()=1 THEN 1 ELSE 0 END;`.
///
/// Four shapes say where the cut stops, and each of them answers differently from 4145
/// near `(`:
///
/// | batch | SQL Server |
/// |---|---|
/// | `SELECT 1 WHERE 1=CURRENT_TIMESTAMP();` | 102 near `)` |
/// | `SELECT 1 WHERE 1=CURRENT_TIMESTAMP(1);` | 102 near `1` |
/// | `SELECT 1 WHERE (CURRENT_TIMESTAMP())=1;` | 102 near `(` |
/// | `SELECT 1 WHERE CURRENT_DATE=1;` | 156 near the keyword `CURRENT_DATE` |
///
/// So the walk follows the left operand of a binary operator — stated on `=` — and stops
/// at anything else, parentheses included; and a spelling that carries no call parenthesis
/// keeps the number of its own token.
///
/// A condition that is not a binary operator is therefore left alone:
/// `SELECT 1 WHERE CURRENT_TIMESTAMP() IS NULL;`, and the same opening followed by
/// `IN (1)`, `LIKE 'a'`, `BETWEEN 1 AND 2` or `COLLATE Latin1_General_CI_AS = 'a'`, answer
/// 102 near `)` here, bare and bracketed alike. The 4145 above is stated on one operator,
/// `=`, in a `WHERE` and in the `WHEN` of a searched `CASE`.
fn leading_niladic_call(e: &Expr) -> Option<Span> {
    let mut current = e;
    loop {
        match current {
            Expr::InvalidNiladic { opening_span, .. } => return *opening_span,
            Expr::Binary { left, .. } => current = left,
            _ => return None,
        }
    }
}

/// Iterative inspection keeps this preflight independent of AST nesting depth.
fn contains_invalid_niladic(expr: &Expr) -> bool {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        match expr {
            Expr::InvalidNiladic { .. } => return true,
            Expr::Nested(inner, _)
            | Expr::Unary { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::Collate { expr: inner, .. } => pending.push(inner),
            Expr::Binary { left, right, .. } => {
                pending.push(left);
                pending.push(right);
            }
            Expr::Function { args, .. } => pending.extend(args),
            Expr::Convert { expr, style, .. } => {
                pending.push(expr);
                if let Some(style) = style {
                    pending.push(style);
                }
            }
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                if let Some(operand) = operand {
                    pending.push(operand);
                }
                for arm in arms {
                    pending.push(&arm.when);
                    pending.push(&arm.then);
                }
                if let Some(otherwise) = else_ {
                    pending.push(otherwise);
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                pending.push(expr);
                pending.push(pattern);
                if let Some(escape) = escape {
                    pending.push(escape);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                pending.push(expr);
                pending.push(low);
                pending.push(high);
            }
            Expr::In { expr, list, .. } => {
                pending.push(expr);
                if let InList::Exprs(items) = list {
                    pending.extend(items);
                }
            }
            Expr::Assign { value, .. } => pending.push(value),
            _ => {}
        }
    }
    false
}

/// Binds a column reference against the source in scope, or refuses it.
///
/// Without a `FROM` the scope is empty and two answers stand: 4104 for a qualified name,
/// 207 for a bare one. With one, the reference is resolved, and the errors keep the same
/// numbers on the same shapes. Four answers, in this order:
///
/// 1. a bare **niladic function** (`CURRENT_TIMESTAMP`, `USER`…) is a call, bound by
///    `call::bind_niladic`, which answers `None` when the name is an ordinary or a
///    delimited one. It comes first, and a column of that very name does not hide it:
///    over a table holding a column `[USER]`, `SELECT USER FROM dbo.t` answers `dbo` typed
///    `nvarchar` and `SELECT [USER] FROM dbo.t` answers the column;
/// 2. a **qualifier that names no source** is error **4104**, severity 16, state 1, on the
///    whole dotted name: `SELECT t.c;`, `SELECT dbo.t.c;` and `SELECT a.b.c.d;` answer
///    4104, the fourth part included — the 117 of a name with too many prefixes concerns a
///    `t.*` wildcard. With a `FROM dbo.t`, `t.a` and `dbo.t.a` resolve and `nosuch.a`,
///    `master.dbo.t.a` and `t.a` under an alias answer that same 4104, while a three-part
///    prefix naming the **current** database binds (`star::Source::matches` decides);
/// 3. a name the source answers to is a [`BoundExprKind::ColumnRef`] over the
///    [`ColumnBinding`](crate::ColumnBinding) the catalogue gave the `Scan`, its `index`
///    unchanged;
/// 4. anything else is error **207** (`SELECT c;`, severity 16, state 1), on the line of
///    the reference — including a name reached through a qualifier that *does* name the
///    source, which SQL Server answers 207 to and not 4104.
///
/// Error **209** is the fourth door: it wants two columns of one name in scope, which one
/// table of the catalogue cannot hold (2705 refuses them at creation) and which a join
/// does (`join.rs`; `tests/bind_join.rs`, `an_ambiguous_unqualified_column_is_209`). The
/// unit test `two_columns_of_the_same_name_in_scope_are_209` builds that scope by hand.
/// Over several sources, `star::lookup` asks the ones the qualifier names, or each of them
/// when there is none.
fn bind_column(column: &ColumnRef, scope: &Scope) -> SqlResult<BoundExpr> {
    let line = line_of(&column.span);
    if column.qualifier.is_none()
        && let Some(bound) = bind_niladic(&column.name, &column.span)?
    {
        return Ok(bound);
    }
    let unbound = || match &column.qualifier {
        Some(qualifier) => {
            SqlError::multi_part_identifier(&dotted(qualifier, &column.name)).with_line(line)
        }
        None => SqlError::invalid_column_name(&column.name.value).with_line(line),
    };
    // Walk the scope chain: try the current scope first, then its parent, and so on.
    let mut current = Some(scope);
    while let Some(sc) = current {
        let Some(found) = lookup(sc.sources(), column.qualifier.as_ref(), &column.name.value)
        else {
            // If a qualifier was written, a failure means the source named by the
            // qualifier does not exist in this scope: try the parent.
            if column.qualifier.is_some() {
                current = sc.parent();
                continue;
            }
            // Without a qualifier, an absent source means the scope is empty or the
            // column is unknown: try the parent.
            current = sc.parent();
            continue;
        };
        return match found {
            // A qualifier that names the source and a column that does not exist is 207,
            // on the column alone: the prefix was bound, the name was not.
            Lookup::One(binding) => Ok(BoundExpr {
                ty: binding.ty.clone(),
                kind: BoundExprKind::ColumnRef(binding.clone()),
                line,
            }),
            Lookup::Absent => {
                Err(SqlError::invalid_column_name(&column.name.value).with_line(line))
            }
            Lookup::Ambiguous => {
                Err(SqlError::ambiguous_column_name(&column.name.value).with_line(line))
            }
        };
    }
    Err(unbound())
}

/// The parts of a qualified column reference joined by dots, as message 4104 prints them:
/// the identifiers themselves, delimiters already stripped by the parser
/// (`SELECT [dbo].[t].[c];` prints `dbo.t.c`).
fn dotted(qualifier: &ObjectName, name: &Ident) -> String {
    [
        qualifier.server.as_ref(),
        qualifier.database.as_ref(),
        qualifier.schema.as_ref(),
        Some(&qualifier.name),
        Some(name),
    ]
    .into_iter()
    .flatten()
    .map(|ident| ident.value.as_str())
    .collect::<Vec<_>>()
    .join(".")
}

/// Binds `@x` against the variables in scope, or hands `@@x` over to the registry.
///
/// # Errors
///
/// Error 137, severity 15, for a variable no `DECLARE` introduced.
fn bind_variable(name: &str, span: &Span, ctx: &BindContext<'_>) -> SqlResult<BoundExpr> {
    if name.starts_with("@@") {
        return bind_variable_function(name, span);
    }
    match ctx.variables.type_of(name) {
        Some(ty) => Ok(BoundExpr {
            kind: BoundExprKind::Variable {
                name: name.to_owned(),
            },
            ty,
            line: line_of(span),
        }),
        None => Err(SqlError::must_declare_scalar_variable(name).with_line(line_of(span))),
    }
}

/// Binds a binary operation, whichever of the three families it belongs to.
///
/// `op_span` is the position of the operator token, which the AST carries;
/// [`bind_arith`] reports 402 and 8117 on its line.
fn bind_binary(
    op: AstBinaryOp,
    op_span: &Span,
    left: &Expr,
    right: &Expr,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    match op {
        AstBinaryOp::Add => bind_arith(BinaryOp::Add, op_span, left, right, span, ctx, scope),
        AstBinaryOp::Sub => bind_arith(BinaryOp::Sub, op_span, left, right, span, ctx, scope),
        AstBinaryOp::Mul => bind_arith(BinaryOp::Mul, op_span, left, right, span, ctx, scope),
        AstBinaryOp::Div => bind_arith(BinaryOp::Div, op_span, left, right, span, ctx, scope),
        AstBinaryOp::Mod => bind_arith(BinaryOp::Mod, op_span, left, right, span, ctx, scope),
        AstBinaryOp::BitAnd => bind_arith(BinaryOp::BitAnd, op_span, left, right, span, ctx, scope),
        AstBinaryOp::BitOr => bind_arith(BinaryOp::BitOr, op_span, left, right, span, ctx, scope),
        AstBinaryOp::BitXor => bind_arith(BinaryOp::BitXor, op_span, left, right, span, ctx, scope),
        AstBinaryOp::Concat => bind_arith(BinaryOp::Concat, op_span, left, right, span, ctx, scope),
        AstBinaryOp::Eq => bind_comparison(CompareOp::Eq, left, right, span, ctx, scope),
        AstBinaryOp::Ne => bind_comparison(CompareOp::Ne, left, right, span, ctx, scope),
        AstBinaryOp::Lt => bind_comparison(CompareOp::Lt, left, right, span, ctx, scope),
        AstBinaryOp::Le => bind_comparison(CompareOp::Le, left, right, span, ctx, scope),
        AstBinaryOp::Gt => bind_comparison(CompareOp::Gt, left, right, span, ctx, scope),
        AstBinaryOp::Ge => bind_comparison(CompareOp::Ge, left, right, span, ctx, scope),
        // `!<` and `!>` are normalised: the bound plan keeps the meaning, not the spelling.
        AstBinaryOp::NotLt => bind_comparison(CompareOp::Ge, left, right, span, ctx, scope),
        AstBinaryOp::NotGt => bind_comparison(CompareOp::Le, left, right, span, ctx, scope),
        AstBinaryOp::And => bind_logical(LogicalOp::And, left, right, span, ctx, scope),
        AstBinaryOp::Or => bind_logical(LogicalOp::Or, left, right, span, ctx, scope),
    }
}

/// Binds `+`, `-`, `*`, `/`, `%`, `&`, `|` and `^`.
///
/// Two decisions before `types::binary_op_type` is called: `+` between two **character**
/// operands is a [`BinaryOp::Concat`] (the parser writes `Add` for both, its README says
/// so), and an integer literal facing an exact numeric enters with its own number of
/// digits ([`numeric_view`]).
///
/// The operands are then converted ([`arith_target`]) as little as `types::eval_binary`
/// allows: it refuses a pair of operands from two different value families, and it is the
/// binder's job to make sure it does not see one — after the binding, the executor decides
/// no conversion.
///
/// That is deliberately **less** than SQL Server converts. `CAST(200 AS tinyint) * CAST(200
/// AS int)` answers `40000`, so the server computes at a width wider than `tinyint`, while
/// `bind_arith` leaves both operands alone and lets `eval_binary` widen them itself.
/// Nothing observable distinguishes the two: a conversion is not visible from outside, its
/// traces are — the value, the type of the result, and the error an operand out of range
/// of its target raises. Each rule below is fixed by one of those traces and by nothing
/// else.
///
/// # The type of the result is not the type of the operands
///
/// `binary_op_type` returns the type of the *result*, which is usually not the target of a
/// conversion: `numeric(2, 1) * numeric(2, 1)` is a `numeric(5, 2)` and converts nothing.
/// Usually, not always — the two coincide in the money and date families, where the result
/// type *is* what the other operand is converted to: `SELECT CAST(0 AS smallmoney) *
/// CAST(3000000 AS int);` is a `smallmoney` and answers an arithmetic overflow for
/// `smallmoney` on the value 3000000, although the product is 0. The two questions are
/// distinct, not disjoint.
///
/// Nor is the *common* type of the two operands that target — not for the operands that
/// carry a precision, and this is not a detail: converting them to it loses digits SQL
/// Server keeps:
///
/// ```text
/// SELECT CAST(12345 AS int) + CAST(0.1 AS decimal(38,38));
///                                    -- 12345.1000000000000000000000000000
/// SELECT CAST(99999999999999999999999999999999999999 AS decimal(38,0))
///      + CAST(0.1 AS decimal(2,1));  -- 99999999999999999999999999999999999999
/// ```
///
/// The common type of the first pair is `decimal(38, 38)`, which holds no `12345` at all,
/// and that of the second `decimal(38, 1)`, which holds no `1e38 - 1`: had SQL Server
/// converted the operands to it, both queries would have answered 8115. It computes
/// exactly instead, and fits the *result* to the result type — the second query does
/// answer 8115 once the sum crosses the cap (`SELECT CAST(999…99 AS decimal(38,0)) +
/// CAST(0.6 AS decimal(2,1));`).
///
/// # Errors
///
/// Those of `binary_op_type`, unchanged but for the line: 206 (type mismatch), 257 (no
/// implicit conversion), 8117 (data type not accepted by the operator). An error that
/// already carries a number is not retranslated into 8117
/// (`error_numbers_are_not_retranslated`).
///
/// # The line is the **operator**'s, not the expression's
///
/// 402 and 8117 are reported on the line of the operator token, which is not where the
/// expression starts: `SELECT` / `CAST(1 AS bit)` / `+` / `CAST(1 AS bit);` answers **4**
/// and not 3. That position is `Expr::Binary::op_span`, written by the lexer's own token:
/// the binder reads it instead of scanning the batch text for the token that follows the
/// left operand, a scan that parts from the lexer on nested block comments
/// (`statement::tests::the_listed_binding_errors_carry_the_line_of_their_own_node`, and
/// `operator_span_is_the_token_between_the_operands` of `parser/tests/operator_span.rs`).
///
/// 206 and 257, which the same two calls raise, are given the same line here and then moved
/// onto the **statement** by `errors::on_the_statement`: the very same `a + b` answers the
/// operator's line for 402 and 8117 and the statement's for 206 and 257 (`crate::errors`).
/// **One call, two families**: the number decides, not the site, which is why
/// `errors::NAMES_THE_STATEMENT` is a table and not a formula. Computing the operator's
/// line once keeps the two calls symmetrical; the table then throws it away for 206 and 257.
fn bind_arith(
    op: BinaryOp,
    op_span: &Span,
    left: &Expr,
    right: &Expr,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let op_line = line_of(op_span);
    let (left_bare, right_bare) = (is_untyped_null(left), is_untyped_null(right));
    let mut left = bind_expr(left, ctx, scope)?;
    let mut right = bind_expr(right, ctx, scope)?;
    if left_bare && !right_bare {
        left.ty = untyped_null_beside(&right.ty);
    } else if right_bare && !left_bare {
        right.ty = untyped_null_beside(&left.ty);
    }
    let op = if op == BinaryOp::Add && is_character(&left.ty) && is_character(&right.ty) {
        BinaryOp::Concat
    } else {
        op
    };
    let (left_ty, right_ty) = (
        numeric_view(&left, &right.ty),
        numeric_view(&right, &left.ty),
    );
    let ty = binary_op_type(op, &left_ty, &right_ty)
        .map_err(|e| refusal_names_the_bare_null(e, op, left_bare, right_bare, &left_ty, &right_ty))
        .map_err(|e| at_line(e, op_line))?;
    // A concatenation converts neither operand: `eval_binary` takes the two strings, or
    // the two byte strings, as they are, and their declared lengths are already summed by
    // `types`.
    let (left, right) = if matches!(ty.ty.family(), TypeFamily::Character | TypeFamily::Binary) {
        (left, right)
    } else {
        let common = implicit_result_type(&left_ty, &right_ty).map_err(|e| at_line(e, op_line))?;
        (
            convert_operand(left, &left_ty, &common),
            convert_operand(right, &right_ty, &common),
        )
    };
    let ty = TypeInfo {
        nullable: arith_is_nullable(op, &left, &right),
        ..ty
    };
    Ok(BoundExpr {
        kind: BoundExprKind::Arith {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        line,
    })
}

/// The nullability SQL Server infers for a binary operator, from the operands **as the
/// operator receives them** — conversions inserted by [`convert_operand`] included.
///
/// The five arithmetic operators are nullable whatever their operands, the four others as
/// soon as one operand is. On the `fNullable` bit of the COLMETADATA token (`@i` a
/// declared `int`, so that `ISNULL(@i, 1)` is a value that is neither constant nor
/// nullable):
///
/// | Query | `fNullable` |
/// |---|---|
/// | `SELECT ISNULL(@i, 1) + 1;` (and `-`, `*`, `/`, `%`) | 1 |
/// | `SELECT 1 + 1;` | 1 — a constant is no exception |
/// | `SET ARITHABORT ON; SELECT 1 + 1;` / `SET ANSI_WARNINGS OFF; …` | 1 — no `SET` option changes it |
/// | `SELECT ISNULL(@i, 1) & 2;` (and `\|`, `^`) | 0 |
/// | `SELECT ISNULL(@s, 'a') + 'x';` | 0 |
/// | `SELECT ISNULL(@s, 'a') + CAST('x' AS varchar(1));` | 1 — carried by the `CAST` |
/// | `SET CONCAT_NULL_YIELDS_NULL OFF; SELECT ISNULL(@s, 'a') + 'x';` | 0 |
/// | `SELECT ISNULL(@b, 0x00) + 0x01;` (`@b` a declared `varbinary(10)`) | 0 |
/// | `SELECT @b + 0x01;` | 1 — carried by `@b` |
///
/// The third row bounds the claim to two options: `ARITHABORT` and `ANSI_WARNINGS`, in
/// the two states each. `NUMERIC_ROUNDABORT` and the others are not claimed.
///
/// The last two rows are the concatenation of two **binary** operands, which the parser
/// and [`bind_arith`] leave as an [`BinaryOp::Add`] — only two character operands are
/// renamed [`BinaryOp::Concat`] — and which `types::binary_op_type` types as a
/// concatenation all the same. It is nullable like the character one, by its operands:
/// `0x00 + 0x01` is not, `CAST(NULL AS varbinary(10)) + 0x01` is. Without this arm, the
/// binder would announce `0x00 + 0x01` as a nullable `BIGVARBINTYPE`, where SQL Server
/// sends `fNullable` 0.
///
/// The operands are read after conversion because a bitwise operator converts them to
/// their common type, and a conversion that may lose a value is nullable
/// ([`implicit_conversion_may_be_null`]).
fn arith_is_nullable(op: BinaryOp, left: &BoundExpr, right: &BoundExpr) -> bool {
    match op {
        BinaryOp::Add if is_binary_concatenation(left, right) => {
            left.ty.nullable || right.ty.nullable
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => true,
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Concat => {
            left.ty.nullable || right.ty.nullable
        }
    }
}

/// Whether a `+` between `left` and `right` is the concatenation of two byte strings,
/// which [`bind_arith`] does not rename [`BinaryOp::Concat`] (see [`arith_is_nullable`]).
fn is_binary_concatenation(left: &BoundExpr, right: &BoundExpr) -> bool {
    left.ty.ty.family() == TypeFamily::Binary && right.ty.ty.family() == TypeFamily::Binary
}

/// The type the bare constant `NULL` enters an operator with, when its sibling operand
/// carries `sibling`.
///
/// The bare `NULL` has no type of its own; `bind_literal` gives it a nullable `int`, which
/// is what `SELECT NULL;` announces, and beside an operand it takes that operand's type
/// instead. On `system_type_name` of `sys.dm_exec_describe_first_result_set`:
///
/// | Query | `system_type_name` | The `NULL` entered as |
/// |---|---|---|
/// | `SELECT CAST(1 AS tinyint) + NULL;` | `tinyint` | `tinyint`, not the `int` of `SELECT NULL;` |
/// | `SELECT CAST(0.5 AS decimal(2,2)) + NULL;` | `decimal(3,2)` | `decimal(2,2)`: `decimal(1,0)` would give `decimal(4,2)` |
/// | `SELECT CAST(1.5 AS decimal(2,1)) * NULL;` | `decimal(5,2)` | `decimal(2,1)`: an `int` would give `decimal(13,1)` |
/// | `SELECT CAST('abc' AS varchar(3)) + NULL;` | `varchar(4)` | `varchar(1)`: a length of 3 answers 4, not 6 |
/// | `SELECT CAST('abc' AS char(3)) + NULL;` | `varchar(4)` | `varchar(1)`: `char(3) + char(1)` is a `char(4)` |
/// | `SELECT CAST('abc' AS nchar(3)) + NULL;` | `nvarchar(4)` | `nvarchar(1)`: `nchar(3) + nchar(1)` is an `nchar(4)` |
/// | `SELECT CAST(0x01 AS binary(8)) + NULL;` | `varbinary(9)` | `varbinary(1)`: `binary(8) + binary(1)` is a `binary(9)` |
/// | `SELECT CAST('abc' AS varchar(max)) + NULL;` | `varchar(max)` | `varchar(1)`, which sums to `max` |
///
/// The `char`, `nchar` and `binary` rows are the ones that distinguish a copy of the
/// sibling's type from a copy of its *family*: the fixed-length forms answer their
/// variable-length twin, and a length of 3 answers 4 and not 6. So the length is 1 and the
/// form is the variable one, while the exact numerics copy the sibling's precision and
/// scale — the `decimal(2,2)` row is where copying the sibling and entering as
/// `decimal(1, 0)` part company.
///
/// `nullable` is `true`; the rest of the type comes from the sibling.
fn untyped_null_beside(sibling: &TypeInfo) -> TypeInfo {
    let ty = match sibling.ty {
        SqlType::Char(_) | SqlType::VarChar(_) => SqlType::VarChar(Len::Fixed(1)),
        SqlType::NChar(_) | SqlType::NVarChar(_) => SqlType::NVarChar(Len::Fixed(1)),
        SqlType::Binary(_) | SqlType::VarBinary(_) => SqlType::VarBinary(Len::Fixed(1)),
        other => other,
    };
    TypeInfo {
        ty,
        nullable: true,
        collation: sibling.collation,
    }
}

/// Puts the name `NULL` back in the refusal of an operator one of whose operands was
/// written as the bare constant `NULL`.
///
/// [`untyped_null_beside`] hands `types::binary_op_type` a pair of identical types, so a
/// refused pair would name that type twice; SQL Server names the written operand `NULL`,
/// and answers **402** where the same pair of typed operands answers 8117:
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT CAST('2020-01-01' AS date) + CAST('2020-01-01' AS date);` | 8117, `date` not accepted by the add operator |
/// | `SELECT CAST('2020-01-01' AS date) + NULL;` | 402, `date` and `NULL` in the add operator |
/// | `SELECT NULL + CAST('2020-01-01' AS date);` | 402, `NULL` and `date` in the add operator |
/// | `SELECT CAST('a' AS varchar(1)) * CAST('b' AS varchar(1));` | 8117, `varchar` not accepted by the multiply operator |
/// | `SELECT CAST('a' AS varchar(1)) * NULL;` | 402, `varchar` and `NULL` in the multiply operator |
/// | `SELECT CAST(1 AS bit) + CAST(1 AS bit);` | 402, `bit` and `bit` in the add operator |
/// | `SELECT CAST(1 AS bit) + NULL;` | 402, `bit` and `NULL` in the add operator |
///
/// Both the 8117 pairs and the 402 pairs come back as 402 once an operand is the bare
/// constant, and so does a refusal that carries another number:
/// [`untyped_null_beside`] hands `binary_op_type` two **equal** types, so what it refuses
/// there is the operator against that type, whichever number it picks to say so. `SELECT
/// CAST('2020-01-01' AS date) * NULL;` is what makes the difference — `types` answers 257
/// for the pair `date`, `date` where SQL Server answers 8117, and the rewrite has to catch
/// it too.
///
/// A pair of bare `NULL`s — `SELECT NULL + NULL;`, an `int` — has nothing to rename, and
/// neither has a pair without one.
fn refusal_names_the_bare_null(
    err: SqlError,
    op: BinaryOp,
    left_bare: bool,
    right_bare: bool,
    left_ty: &TypeInfo,
    right_ty: &TypeInfo,
) -> SqlError {
    if left_bare == right_bare {
        return err;
    }
    let (left, right) = if left_bare {
        ("NULL", right_ty.ty.name())
    } else {
        (left_ty.ty.name(), "NULL")
    };
    SqlError::incompatible_types_for_operator(left, right, operator_in_402(op))
}

/// The word error 402 prints for `op`, which is the operation and not the symbol, except
/// for the three bitwise operators, which print their quoted symbol.
///
/// The eight words are those SQL Server prints for a `date` faced with a bare `NULL`, and
/// the test `every_operator_word_of_402_is_the_one_sql_server_prints` asserts the eight
/// messages. `Concat` is the name
/// [`bind_arith`] gives a `+` between two character operands, a pair `binary_op_type`
/// accepts; it shares the word of `Add`, the symbol the parser read.
fn operator_in_402(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add | BinaryOp::Concat => "add",
        BinaryOp::Sub => "subtract",
        BinaryOp::Mul => "multiply",
        BinaryOp::Div => "divide",
        BinaryOp::Mod => "modulo",
        BinaryOp::BitAnd => "'&'",
        BinaryOp::BitOr => "'|'",
        BinaryOp::BitXor => "'^'",
    }
}

/// `expr` wrapped in the conversion [`arith_target`] asks for, or `expr` unchanged.
///
/// `view` is the type the operand entered the precision table with ([`numeric_view`]) and
/// `common` the common type of the pair.
fn convert_operand(expr: BoundExpr, view: &TypeInfo, common: &TypeInfo) -> BoundExpr {
    match arith_target(&expr.ty, view, common) {
        Some(target) => convert_to(expr, &target),
        None => expr,
    }
}

/// The type an operand of an arithmetic or bitwise operator is converted to, or `None`
/// when it needs no conversion at all — which is the common case.
///
/// [`vauban_types::eval_binary`] dispatches on the **family** of the two values, not
/// on their type: two integers of different widths, two exact numerics of different
/// scales, two `money` and a `real` with a `float` are each a pair it computes as it
/// stands. What it refuses is a pair from two different families, and that is exactly what
/// this function converts away — nothing more, because a conversion inserted beyond that
/// is a chance to lose a digit SQL Server keeps (see [`bind_arith`]).
///
/// So an operand whose family is already the family of `common` is left alone, and the
/// others are converted to:
///
/// - the type they entered the precision table with, when `common` is an exact numeric and
///   the operand carries a precision of its own. That is `view` for an integer literal,
///   which counts its own digits, and otherwise the answer of [`exact_numeric_view`].
///   `SELECT CAST(12345 AS int) + CAST(0.1 AS decimal(38,38));` answers
///   `12345.1000000000000000000000000000`, so the `int` became a `numeric(10, 0)` and not
///   the `decimal(38, 38)` the pair has in common;
/// - `common` otherwise, which covers two groups. The operands of a family that carries no
///   precision at all — `bit`, the characters, the binaries — and the pairs whose common
///   type is not an exact numeric.
///
/// The second bullet is where SQL Server departs from a reading of its precision table by
/// type, and each half of it is fixed by a query:
///
/// - `bit` is listed there as `numeric(1, 0)`, and it does not behave as one: `SELECT
///   CAST(1.5 AS decimal(2,1)) * CAST(1 AS bit);` is a `decimal(5, 2)` — the precision and
///   the scale of `decimal(2,1) * decimal(2,1)` — where a `numeric(1, 0)` operand would
///   have given a `decimal(4, 1)`, which is what the same product with a `tinyint` gives
///   (`decimal(6, 1)`). `SELECT CAST(1 AS bit) + CAST(0.1 AS decimal(38,38));` settles it:
///   an arithmetic overflow converting `tinyint` to `numeric`, because `decimal(38, 38)`
///   holds no 1. Beware `CAST(1 AS bit) + CAST(1.5 AS decimal(2,1))`, which is a
///   `decimal(3, 1)` under **both** readings and discriminates nothing;
/// - a `varchar` against an `int` becomes an `int` (`SELECT '1' + 2;` answers the `int` 3);
/// - an `int` against a `smallmoney` becomes a `smallmoney`, not the `money` a promotion of
///   the pair would have given: `SELECT CAST(0 AS smallmoney) * CAST(3000000 AS int);`
///   answers an arithmetic overflow for `smallmoney` on the value 3000000 although the
///   product is 0 and although a `money` holds 3000000. The operand is what fails, and the
///   message names its target;
/// - an `int` against a `datetime` becomes a `datetime`, and a `smalldatetime` keeps a
///   `smalldatetime`: `SELECT CAST('9999-12-31' AS datetime) + CAST(-100000 AS int);`
///   answers an arithmetic overflow converting the expression to `datetime` although the
///   sum, some day of 9726, is a date `datetime` holds — it is the operand, read as a date
///   of 1626, that leaves the type.
///
/// The bitwise operators follow the same rule: `SQL_VARIANT_PROPERTY` reports `int` for
/// `CAST(200 AS tinyint) & CAST(200 AS int)` and for `CAST(1 AS bit) & CAST(200 AS int)`
/// — the common type in both — and `tinyint` for
/// `CAST(200 AS tinyint) & CAST(200 AS tinyint)`, so a narrow pair stays narrow there too.
fn arith_target(node: &TypeInfo, view: &TypeInfo, common: &TypeInfo) -> Option<TypeInfo> {
    if node.ty.family() == common.ty.family() {
        return None;
    }
    if !common.ty.is_exact_numeric() {
        return Some(common.clone());
    }
    if view.ty.is_exact_numeric() {
        return Some(view.clone());
    }
    // A `bit`, a character or a binary operand has no precision of its own and takes the
    // common type, which is the type SQL Server names when the conversion fails: `SELECT
    // CAST('2' AS varchar(4)) * CAST(0.1 AS decimal(38,38));` answers an arithmetic
    // overflow converting `varchar` to `numeric`, and `SELECT CAST('1.5' AS varchar(4))
    // * CAST(2 AS int);` a conversion failure of the `varchar` value '1.5' to `int`.
    Some(exact_numeric_view(node).unwrap_or_else(|| common.clone()))
}

/// The exact numeric an integer or a `money` enters the precision table as, or `None` for
/// a type that has no such reading.
///
/// The precision and scale of an expression that is not a decimal are those defined for
/// its data type. `types` owns that table and keeps it private, so the binder reads it
/// through [`vauban_types::implicit_result_type`] instead of holding a second copy:
/// `numeric(1, 0)` is the **neutral element** of the widening it applies to two exact
/// numerics — merging it with a `numeric(p, s)` gives that `numeric(p, s)` back — so
/// merging it with another type yields that type seen as an exact numeric, and nothing
/// else. The unit test `an_operand_enters_with_the_precision_of_its_own_type` checks the
/// six answers.
///
/// `bit` is **not** among them, although a `numeric(1, 0)` reading would put it there: it
/// takes the common type of the pair like a character operand, so it belongs to the group
/// this function answers `None` for. The queries are in [`arith_target`].
fn exact_numeric_view(ty: &TypeInfo) -> Option<TypeInfo> {
    if !matches!(ty.ty.family(), TypeFamily::Integer | TypeFamily::Money) {
        return None;
    }
    implicit_result_type(ty, &TypeInfo::new(ONE_DIGIT, false)).ok()
}

/// The weakest exact numeric there is: one integral digit, no fractional one.
const ONE_DIGIT: SqlType = SqlType::Numeric {
    precision: 1,
    scale: 0,
};

/// Binds `=`, `<>`, `<`, `<=`, `>`, `>=`, `!<` and `!>`.
///
/// The two operands are brought to their common type
/// ([`vauban_types::implicit_result_type`]) and the one that is not already of that
/// type is wrapped in a `Convert`: `1 = '1'` converts the *string*, because `int` outranks
/// `varchar` in the precedence of `types`.
fn bind_comparison(
    op: CompareOp,
    left: &Expr,
    right: &Expr,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let left = bind_expr(left, ctx, scope)?;
    let right = bind_expr(right, ctx, scope)?;
    compare_node(op, left, right, line)
}

/// One comparison, with the conversions inserted: the shape `BETWEEN` and the simple
/// `CASE` are desugared into.
fn compare_node(
    op: CompareOp,
    left: BoundExpr,
    right: BoundExpr,
    line: u32,
) -> SqlResult<BoundExpr> {
    let (left_ty, right_ty) = (
        numeric_view(&left, &right.ty),
        numeric_view(&right, &left.ty),
    );
    // The operands are passed in the order the query wrote them: error 206 names them in
    // that order (`SELECT CASE WHEN CAST('2000-01-01' AS date) = 1 THEN 1 ELSE 0 END;`
    // names `date` then `tinyint`).
    let common = implicit_result_type(&left_ty, &right_ty).map_err(|e| at_line(e, line))?;
    let nullable = left.ty.nullable || right.ty.nullable;
    Ok(BoundExpr {
        kind: BoundExprKind::Compare {
            op,
            left: Box::new(convert_to(left, &common)),
            right: Box::new(convert_to(right, &common)),
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line,
    })
}

/// Binds `AND` and `OR`, whose two operands are conditions and not values.
fn bind_logical(
    op: LogicalOp,
    left: &Expr,
    right: &Expr,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let left = bind_condition(left, ctx, scope)?;
    let right = bind_condition(right, ctx, scope)?;
    let nullable = left.ty.nullable || right.ty.nullable;
    Ok(BoundExpr {
        kind: BoundExprKind::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line,
    })
}

/// The name SQL Server prints for a unary operator in message 8117.
///
/// `types::binary_op_type` owns the table for the binary operators; the two unary ones are
/// typed here, so their names are here too, as SQL Server prints them:
///
/// | query | the operator named |
/// |---|---|
/// | `SELECT -CAST('2000-01-01' AS date);` | `minus` |
/// | `SELECT ~CAST(1.5 AS float);` | `'~'` |
const MINUS_OPERATOR: &str = "minus";
/// The name of `~` in message 8117, quoted as SQL Server quotes the symbolic operators
/// (`SELECT ~CAST('abc' AS varchar(10));` names the `'~'` operator).
const BITNOT_OPERATOR: &str = "'~'";

/// Binds `+e`, `-e`, `~e` and `NOT p`.
///
/// The unary `+` is **transparent**, with no check at all: SQL Server answers `abc` to
/// `SELECT +CAST('abc' AS varchar(10));` and `2000-01-01` to
/// `SELECT +CAST('2000-01-01' AS date);`. The bound plan drops it, exactly as it drops
/// parentheses.
///
/// `-e` accepts the numbers and nothing else: `bit`, `date`, `datetime`, `varchar`,
/// `varbinary` and `uniqueidentifier` are each 8117.
/// It keeps the type of its operand, but for `tinyint`, which it widens to `smallint` —
/// `SELECT CAST(SQL_VARIANT_PROPERTY(-CAST(1 AS tinyint), 'BaseType') AS varchar(30));`
/// answers `smallint`, the unsigned type having no negative values.
///
/// `~e` accepts the integers and `bit` and keeps their type (`~tinyint` is a `tinyint`,
/// `~bit` is a `bit`); `numeric`, `float`, `varchar` and `varbinary` are 8117.
///
/// # Nullability
///
/// `-e` is an arithmetic operator and is nullable like one ([`arith_is_nullable`]), with
/// one exception: a **signed literal**, which SQL Server reads as a literal and not as an
/// operation. `~e` keeps the nullability of its operand. On the `fNullable` bit of
/// COLMETADATA:
///
/// | Query | `fNullable` |
/// |---|---|
/// | `SELECT -1;`, `SELECT -(1);`, `SELECT -(-1);`, `SELECT -1.5;` | 0 |
/// | `SELECT -ISNULL(@i, 1);`, `SELECT -@@SPID;` | 1 |
/// | `SELECT -CAST(1 AS int);` | 1 — the operand is |
/// | `SELECT ~1;`, `SELECT ~ISNULL(@i, 1);`, `SELECT ~@@SPID;` | 0 |
/// | `SELECT ~CAST(1 AS int);` | 1 — the operand is |
fn bind_unary(
    op: UnaryOp,
    operand: &Expr,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    if op == UnaryOp::Not {
        let inner = bind_condition(operand, ctx, scope)?;
        let nullable = inner.ty.nullable;
        return Ok(BoundExpr {
            kind: BoundExprKind::Not(Box::new(inner)),
            ty: TypeInfo::new(SqlType::Bit, nullable),
            line,
        });
    }

    let inner = bind_expr(operand, ctx, scope)?;
    match op {
        UnaryOp::Plus => Ok(inner),
        UnaryOp::Minus => {
            let ty = match inner.ty.ty.family() {
                TypeFamily::Integer
                | TypeFamily::ExactNumeric
                | TypeFamily::ApproxNumeric
                | TypeFamily::Money => {
                    let widened = if inner.ty.ty == SqlType::TinyInt {
                        SqlType::SmallInt
                    } else {
                        inner.ty.ty
                    };
                    let nullable = if is_signed_literal(&inner) {
                        inner.ty.nullable
                    } else {
                        true
                    };
                    TypeInfo::new(widened, nullable)
                }
                TypeFamily::Bit
                | TypeFamily::Character
                | TypeFamily::Binary
                | TypeFamily::DateTime
                | TypeFamily::Guid => {
                    return Err(SqlError::invalid_operand_type(
                        inner.ty.ty.error_name(),
                        MINUS_OPERATOR,
                    )
                    .with_line(line));
                }
            };
            Ok(BoundExpr {
                kind: BoundExprKind::Negate(Box::new(inner)),
                ty,
                line,
            })
        }
        UnaryOp::BitNot => match inner.ty.ty.family() {
            TypeFamily::Integer | TypeFamily::Bit => {
                let ty = inner.ty.clone();
                Ok(BoundExpr {
                    kind: BoundExprKind::BitNot(Box::new(inner)),
                    ty,
                    line,
                })
            }
            _ => Err(
                SqlError::invalid_operand_type(inner.ty.ty.error_name(), BITNOT_OPERATOR)
                    .with_line(line),
            ),
        },
        // `NOT` returned above; the compiler cannot see it, the reader can.
        UnaryOp::Not => Err(not_yet("bind_unary: NOT is handled before this match")),
    }
}

/// Binds `e IS NULL` and `e IS NOT NULL`.
///
/// The result is a `bit` that is not `NULL`: the answer is true or false, the operand
/// being `NULL` or not.
fn bind_is_null(
    operand: &Expr,
    negated: bool,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let inner = bind_expr(operand, ctx, scope)?;
    Ok(BoundExpr {
        kind: BoundExprKind::IsNull {
            expr: Box::new(inner),
            negated,
        },
        ty: TypeInfo::new(SqlType::Bit, false),
        line: line_of(span),
    })
}

/// Binds `e IN (v1, v2, …)` and its negation.
///
/// The tested value and the elements of the list are brought to one common type, and the
/// ones that are not already of that type are wrapped in a `Convert`.
///
/// The fold passes each **element first** to `implicit_result_type`, which is the order SQL
/// Server names the operands in when it refuses the pair:
/// `SELECT CASE WHEN 1 IN (1, CAST('0E984725-C51C-4BF4-9960-E1C80E27ABA0' AS uniqueidentifier))
/// THEN 1 ELSE 0 END;` names `uniqueidentifier` then `tinyint` — the list element, then
/// the tested value.
///
/// # Errors
///
/// Error 206 when an element has no common type with the tested value; the internal error
/// 50000 of `subquery.rs` for `IN (SELECT …)`.
fn bind_in(
    operand: &Expr,
    list: &InList,
    negated: bool,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let items = match list {
        InList::Exprs(items) => items,
        // `IN (SELECT …)` is `BoundExprKind::InSubquery`, a variant of its own: the list
        // form below compares against values converted to one common type, which is not
        // the evaluation path a plan takes (`bound/mod.rs`).
        InList::Subquery(query) => {
            return subquery::bind_in_subquery(operand, query, negated, ctx, scope);
        }
    };
    let operand = bind_expr(operand, ctx, scope)?;
    let items = items
        .iter()
        .map(|item| bind_expr(item, ctx, scope))
        .collect::<SqlResult<Vec<_>>>()?;

    let branches: Vec<&BoundExpr> = std::iter::once(&operand).chain(items.iter()).collect();
    let views = numeric_views(&branches);
    // `views` holds at least the tested value, which the fold starts from.
    let mut common = views.first().unwrap_or(&operand.ty).clone();
    for view in views.iter().skip(1) {
        common = implicit_result_type(view, &common).map_err(|e| at_line(e, line))?;
    }

    let nullable = branches.iter().any(|branch| branch.ty.nullable);
    Ok(BoundExpr {
        kind: BoundExprKind::In {
            expr: Box::new(convert_to(operand, &common)),
            list: items
                .into_iter()
                .map(|item| convert_to(item, &common))
                .collect(),
            negated,
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line,
    })
}

/// Binds `e LIKE p [ESCAPE c]` and its negation.
///
/// `LIKE` compares strings, but it does **not** refuse a value that is not one: SQL Server
/// converts it and answers — `SELECT CASE WHEN 1 LIKE 2 THEN 1 ELSE 0 END;` answers `0`,
/// and the same shape over a `date` (`LIKE '2%'`) or a `uniqueidentifier` (`LIKE '0%'`)
/// answers `1`. There is therefore no 8117 here: a non-character operand is wrapped in a
/// `Convert` towards a character type, `nvarchar` as soon as one of the operands is
/// Unicode.
///
/// The match and pattern conversions retain the `max` target used by this binder. For
/// `ESCAPE`, the int, bit, date, decimal, float and GUID forms use a one-character target:
/// `ESCAPE 12` behaves as `'*'` with varchar operands, but raises 8115 with nvarchar
/// operands. Character strings keep their length; the two-byte binary 0x2122 also keeps
/// both bytes (506 with varchar).
fn bind_like(
    operand: &Expr,
    pattern: &Expr,
    escape: Option<&Expr>,
    negated: bool,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let operand = bind_expr(operand, ctx, scope)?;
    let pattern = bind_expr(pattern, ctx, scope)?;
    let escape = escape.map(|e| bind_expr(e, ctx, scope)).transpose()?;

    let unicode = [Some(&operand), Some(&pattern), escape.as_ref()]
        .into_iter()
        .flatten()
        .any(|e| matches!(e.ty.ty, SqlType::NChar(_) | SqlType::NVarChar(_)));
    let target = if unicode {
        SqlType::NVarChar(Len::Max)
    } else {
        SqlType::VarChar(Len::Max)
    };
    let nullable = operand.ty.nullable
        || pattern.ty.nullable
        || escape.as_ref().is_some_and(|e| e.ty.nullable);

    Ok(BoundExpr {
        kind: BoundExprKind::Like {
            expr: Box::new(to_string_operand(operand, target)),
            pattern: Box::new(to_string_operand(pattern, target)),
            escape: escape.map(|e| {
                let escape_target = if matches!(e.ty.ty, SqlType::Binary(_) | SqlType::VarBinary(_))
                {
                    target
                } else if unicode {
                    SqlType::NVarChar(Len::Fixed(1))
                } else {
                    SqlType::VarChar(Len::Fixed(1))
                };
                Box::new(to_string_operand(e, escape_target))
            }),
            negated,
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line,
    })
}

/// Whether `e` is a literal under zero or more unary minuses: `1`, `-1`, `-(-1)`.
///
/// `-(1)` and `-1` are the same node for the parser, which does not fold the sign into
/// the literal, and SQL Server gives both the form of a literal ([`bind_unary`]).
fn is_signed_literal(e: &BoundExpr) -> bool {
    match &e.kind {
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Negate(inner) => is_signed_literal(inner),
        _ => false,
    }
}

/// `expr` if it is already a character string, converted to `target` otherwise.
fn to_string_operand(expr: BoundExpr, target: SqlType) -> BoundExpr {
    if expr.ty.ty.is_string() {
        return expr;
    }
    let ty = TypeInfo::new(target, expr.ty.nullable);
    convert_to(expr, &ty)
}

/// Binds `a BETWEEN low AND high` by **desugaring** it into `a >= low AND a <= high`, and
/// `a NOT BETWEEN low AND high` into `a < low OR a > high`.
///
/// The predicate is defined by that equivalence, `NULL` included, so the desugaring
/// changes no answer and leaves the executor one shape to evaluate instead of three. Same
/// reasoning as the simple `CASE` ([`bind_case`]).
///
/// The tested value is bound **once** and cloned into the two comparisons: the bound tree
/// holds two copies of it. That is invisible for expressions that compute nothing on their
/// own; a non-deterministic operand (`NEWID() BETWEEN …`) would be evaluated twice, which
/// is the one thing to weigh again for those.
fn bind_between(
    operand: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let operand = bind_expr(operand, ctx, scope)?;
    let low = bind_expr(low, ctx, scope)?;
    let high = bind_expr(high, ctx, scope)?;

    let (op, lower, upper) = if negated {
        (LogicalOp::Or, CompareOp::Lt, CompareOp::Gt)
    } else {
        (LogicalOp::And, CompareOp::Ge, CompareOp::Le)
    };
    let left = compare_node(lower, operand.clone(), low, line)?;
    let right = compare_node(upper, operand, high, line)?;
    let nullable = left.ty.nullable || right.ty.nullable;
    Ok(BoundExpr {
        kind: BoundExprKind::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line,
    })
}

/// Binds a `CASE`, simple or searched, into the **searched** shape.
///
/// A simple `CASE` is desugared: `CASE a WHEN b THEN …` becomes
/// `CASE WHEN a = b THEN …`, so `BoundExprKind::Case::operand` is `None` and the executor
/// has one shape to evaluate. The equivalence is the definition of the simple form, and
/// the desugaring buys the comparison rules
/// — common type, inserted conversions, collation — from [`compare_node`] instead of
/// repeating them. Its price is the same as [`bind_between`]'s: the operand is cloned into
/// each arm, so a non-deterministic one would be evaluated once per arm.
///
/// The type of the result is the common type of the `THEN` branches and of the `ELSE`,
/// folded left to right by `implicit_result_type`; each branch that is not already of that
/// type is wrapped in a `Convert`. An **absent** `ELSE` adds no type — it only makes the
/// result nullable, since the `CASE` then answers `NULL` when no arm matches.
///
/// # Nullability
///
/// The result is nullable as soon as one branch is **once converted** to the common type,
/// or when there is no `ELSE`; the conditions play no part. On the `fNullable` bit of
/// COLMETADATA (`@i` a declared `int`, `@n` a declared `decimal(10,2)`, `@@SPID = 0` a
/// condition the server cannot fold):
///
/// | Query | `fNullable` |
/// |---|---|
/// | `CASE WHEN @@SPID = 0 THEN 1 ELSE 2 END` | 0 |
/// | `CASE WHEN @@SPID = 0 THEN 1 END` | 1 — no `ELSE` |
/// | `CASE WHEN @@SPID = 0 THEN CAST(1 AS int) ELSE 2 END` | 1 — a branch is |
/// | `CASE WHEN @i = 0 THEN 1 ELSE 2 END` | 0 — the condition does not count |
/// | `CASE WHEN @@SPID = 0 THEN ISNULL(@i, 1) ELSE ISNULL(@n, 1.5) END` | 0 — `int` fits a `decimal(12,2)` |
/// | `CASE WHEN @@SPID = 0 THEN 1 ELSE 1.5 END` | 1 — an `int` does not fit a `numeric(2,1)` |
/// | `CASE WHEN @@SPID = 0 THEN ISNULL(@i, 1) ELSE 1.5 END` | 0 — it fits a `numeric(11,1)` |
///
/// The last three rows are the conversion of a branch, and they are read from the type
/// the branch **exposes** (`int` for the literal `1`), not from the view it enters the
/// precision table with ([`numeric_view`]): `1` is a `numeric(1, 0)` for the precision of
/// the result and an `int` for the nullability of its conversion.
///
/// SQL Server also folds a `CASE` whose conditions are constant — `CASE WHEN 1 = 1 THEN 1
/// END` answers `fNullable = 0` — and this binder folds nothing: that shape stays nullable
/// here, a difference that the metadata of an empty result alone shows.
///
/// # Errors
///
/// Error 4145 when a `WHEN` of a searched `CASE` is not a condition, error 206 when two
/// branches have no common type.
fn bind_case(
    operand: Option<&Expr>,
    arms: &[CaseArm],
    else_: Option<&Expr>,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let operand = operand.map(|e| bind_expr(e, ctx, scope)).transpose()?;

    let mut bound_arms: Vec<BoundCaseArm> = Vec::with_capacity(arms.len());
    for arm in arms {
        let when = match &operand {
            Some(operand) => {
                let value = bind_expr(&arm.when, ctx, scope)?;
                compare_node(CompareOp::Eq, operand.clone(), value, line)?
            }
            None => bind_condition(&arm.when, ctx, scope)?,
        };
        let then = bind_expr(&arm.then, ctx, scope)?;
        bound_arms.push(BoundCaseArm { when, then });
    }
    let else_ = else_.map(|e| bind_expr(e, ctx, scope)).transpose()?;

    let mut branches: Vec<&BoundExpr> = bound_arms.iter().map(|arm| &arm.then).collect();
    if let Some(else_) = else_.as_ref() {
        branches.push(else_);
    }
    let Some(ty) = common_of(&branches, line)? else {
        return Err(not_yet("bind_case: a CASE without a single WHEN arm"));
    };
    let nullable = else_.is_none()
        || branches.iter().any(|branch| {
            branch.ty.nullable || implicit_conversion_may_be_null(&branch.ty.ty, &ty.ty)
        });
    let ty = TypeInfo {
        nullable,
        ..ty.clone()
    };

    Ok(BoundExpr {
        kind: BoundExprKind::Case {
            operand: None,
            arms: bound_arms
                .into_iter()
                .map(|arm| BoundCaseArm {
                    when: arm.when,
                    then: convert_to(arm.then, &ty),
                })
                .collect(),
            else_: else_.map(|e| Box::new(convert_to(e, &ty))),
        },
        ty,
        line,
    })
}

/// Binds `e COLLATE Latin1_General_CS_AS`.
///
/// The collation name is resolved first — `types::Collation::parse` raises 448 for a name
/// the server does not know — and the operand must then be a character string: SQL Server
/// answers error **447** to `SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;`.
///
/// The node keeps the type of its operand, length included, with the new collation:
/// `SELECT CAST('a' AS varchar(3)) COLLATE Latin1_General_CS_AS;` is still a `varchar(3)`
/// (through `SQL_VARIANT_PROPERTY`).
fn bind_collate(
    operand: &Expr,
    collation: &str,
    collation_span: &Span,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    // Both errors of the clause are reported on the **collation name** and not on the
    // expression: `SELECT` / `1` / `,` / `(1` / `COLLATE` / `Latin1_General_CI_AS)` / `+` /
    // `1;` answers 447 on line **7**, and the same shape with an unknown collation
    // answers 448 on 7 too. The name's span comes from the AST.
    let name_line = line_of(collation_span);
    let collation = Collation::parse(collation).map_err(|e| at_line(e, name_line))?;
    let inner = bind_expr(operand, ctx, scope)?;
    if !inner.ty.ty.is_string() {
        return Err(SqlError::collate_on_non_string(inner.ty.ty.error_name()).with_line(name_line));
    }
    let ty = TypeInfo {
        ty: inner.ty.ty,
        nullable: inner.ty.nullable,
        collation: Some(collation),
    };
    Ok(BoundExpr {
        kind: BoundExprKind::Collate {
            expr: Box::new(inner),
        },
        ty,
        line,
    })
}

/// The common type of a list of branches, folded left to right in the order they were
/// written, or `None` when the list is empty.
fn common_of(branches: &[&BoundExpr], line: u32) -> SqlResult<Option<TypeInfo>> {
    let views = numeric_views(branches);
    let mut common: Option<TypeInfo> = None;
    for view in views {
        common = Some(match common {
            None => view,
            Some(acc) => implicit_result_type(&acc, &view).map_err(|e| at_line(e, line))?,
        });
    }
    Ok(common)
}

/// The types the branches enter the precision table with: [`numeric_view`] applied to each
/// of them against the first exact-numeric branch, when there is one.
fn numeric_views(branches: &[&BoundExpr]) -> Vec<TypeInfo> {
    let exact = branches
        .iter()
        .find(|branch| branch.ty.ty.is_exact_numeric())
        .map(|branch| branch.ty.clone());
    branches
        .iter()
        .map(|branch| match &exact {
            Some(exact) => numeric_view(branch, exact),
            None => branch.ty.clone(),
        })
        .collect()
}

/// The type an operand enters a computation with, which is its own type — except for an
/// **integer literal** facing an exact numeric.
///
/// SQL Server does not give the literal `1` the precision of an `int` when it meets a
/// `numeric`: it counts the digits the user typed. Two pairs, side by side:
///
/// ```text
/// SELECT SQL_VARIANT_PROPERTY(1 + 1.5, 'Precision');                                -- 3
/// SELECT SQL_VARIANT_PROPERTY(CAST(1 AS int) + CAST(1.5 AS numeric(2,1)), 'Precision'); -- 12
/// SELECT SQL_VARIANT_PROPERTY(CASE WHEN 1 = 1 THEN 1 ELSE 2.5 END, 'Precision');    -- 2
/// SELECT SQL_VARIANT_PROPERTY(CASE WHEN 1 = 1 THEN CAST(1 AS int)
///                                  ELSE CAST(2.5 AS numeric(2,1)) END, 'Precision'); -- 11
/// ```
///
/// `1` therefore enters as `numeric(1, 0)` and `CAST(1 AS int)` as `numeric(10, 0)`, both
/// in the arithmetic table (`binary_op_type`) and in the common type (`implicit_result_type`).
/// `types` sees a `TypeInfo` and nothing else, so the substitution belongs here — and here
/// alone: the type the literal node *exposes* stays `int`, which is what a client reads
/// for `SELECT 1`.
///
/// The number of digits is that of the **value**, not of the text: `007` counts as one
/// digit; the padded form is stated on that reading alone.
fn numeric_view(expr: &BoundExpr, other: &TypeInfo) -> TypeInfo {
    if !other.ty.is_exact_numeric() || expr.ty.ty.family() != TypeFamily::Integer {
        return expr.ty.clone();
    }
    let BoundExprKind::Literal(value) = &expr.kind else {
        return expr.ty.clone();
    };
    match integer_digits(value) {
        Some(precision) => TypeInfo::new(
            SqlType::Numeric {
                precision,
                scale: 0,
            },
            expr.ty.nullable,
        ),
        None => expr.ty.clone(),
    }
}

/// The number of decimal digits of an integer value, at least one, or `None` when the value
/// is not an integer.
fn integer_digits(value: &Value) -> Option<u8> {
    let value: i128 = match value {
        Value::I8(v) => i128::from(*v),
        Value::I16(v) => i128::from(*v),
        Value::I32(v) => i128::from(*v),
        Value::I64(v) => i128::from(*v),
        _ => return None,
    };
    let mut rest = value.unsigned_abs();
    let mut digits: u8 = 1;
    while rest >= 10 {
        rest /= 10;
        digits = digits.saturating_add(1);
    }
    Some(digits)
}

/// `expr` wrapped in a `Convert` towards `target`, or `expr` unchanged when it already has
/// that type.
///
/// The conversion is nullable when its source is, or when the pair of types may lose a
/// value ([`implicit_conversion_may_be_null`]); it takes the collation of the target,
/// which is the one `implicit_result_type` computed for the pair.
pub(crate) fn convert_to(expr: BoundExpr, target: &TypeInfo) -> BoundExpr {
    if expr.ty.ty == target.ty {
        return expr;
    }
    let ty = TypeInfo {
        ty: target.ty,
        nullable: expr.ty.nullable || implicit_conversion_may_be_null(&expr.ty.ty, &target.ty),
        collation: target.collation,
    };
    let line = expr.line;
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(expr),
            style: None,
            try_: false,
        },
        ty,
        line,
    }
}

/// Whether an **implicit** conversion from `from` to `to` makes a non-nullable value
/// nullable.
///
/// An explicit `CAST` is nullable whatever its source (`call::bind_conversion`); the
/// conversions the engine inserts on its own — under `CASE`, `COALESCE`, `ISNULL`, the
/// bitwise operators — are nullable when the target may not hold a value of the source,
/// and not otherwise. On the `fNullable` bit of COLMETADATA through
/// `ISNULL(@to, ISNULL(@from, <literal>))`, where `@to` and `@from` are declared variables
/// of the two types: the inner `ISNULL` is a value of type `from` that is neither constant
/// nor nullable, the outer one converts it to `to` and answers `fNullable = 0` if and only
/// if the conversion is lossless. The inner replacement must be a **literal**, which folds:
/// `ISNULL(@sd, GETDATE())` converts the clock to `smalldatetime` and is nullable on its
/// own, which would misread `smalldatetime` → `datetime`. Ninety pairs, of which:
///
/// | From → to | `fNullable` | Rule |
/// |---|---|---|
/// | `tinyint` → `smallint`, `int` → `bigint`, `bit` → `tinyint`… | 0 | integers widen by their digits |
/// | `bigint` → `int`, `int` → `tinyint`, `tinyint` → `bit` | 1 | |
/// | `int` → `decimal(10,0)`, `bigint` → `decimal(19,0)`, `bit` → `decimal(1,0)` | 0 | `p - s ≥` the digits of the integer |
/// | `int` → `decimal(9,0)`, `int` → `decimal(10,2)`, `bigint` → `decimal(18,0)` | 1 | |
/// | `int` → `float`, `smallint` → `real`, `bit` → `real` | 0 | 53 and 24 bits of mantissa |
/// | `bigint` → `float`, `int` → `real` | 1 | |
/// | `int` → `money`, `smallint` → `smallmoney`, `bit` → `money` | 0 | 15 and 6 integral digits |
/// | `bigint` → `money`, `int` → `smallmoney` | 1 | |
/// | `smallmoney` → `money`, `money` → `decimal(19,4)`, `decimal(5,2)` → `money` | 0 | |
/// | `money` → `decimal(18,4)`, `money` → `bigint`, `smallmoney` → `int`, `money` → `float` | 1 | |
/// | `decimal(5,2)` → `decimal(6,3)`, `→ decimal(6,2)`, `→ numeric(6,2)` | 0 | both parts grow or stay |
/// | `decimal(5,2)` → `decimal(5,3)`, `→ decimal(5,1)`, `decimal(12,2)` → `decimal(10,2)` | 1 | |
/// | `real` → `float` | 0 | `float` → `real`, `float` ↔ `decimal` are 1 |
/// | `char(5)` → `char(10)`, `varchar(10)` → `nchar(10)`, `nvarchar(10)` → `varchar(20)`, anything → `(max)` | 0 | at least as many characters **and** bytes |
/// | `char(10)` → `char(5)`, `nvarchar(10)` → `varchar(10)`, `varchar(10)` → `nvarchar(5)`, `varchar(max)` → `varchar(10)` | 1 | |
/// | `varbinary(10)` → `varbinary(20)`, `varbinary(5)` → `binary(10)`, → `(max)` | 0 | at least as many bytes |
/// | `binary(10)` → `binary(5)` | 1 | |
/// | `date` → `datetime2`, `date` → `datetimeoffset`, `time` → `datetime2`, `smalldatetime` → `datetime`, `smalldatetime` → `datetime2` | 0 | |
/// | `datetime2(3)` → `datetime2(7)`, `datetime2` → `datetimeoffset`, `time(3)` → `time(7)` | 0 | the scale grows or stays |
/// | `datetime2(7)` → `datetime2(3)`, `time(7)` → `time(3)` | 1 | |
/// | `date` → `datetime`, `datetime` → `datetime2`, `datetime` → `datetimeoffset` | 1 | `datetime` widens to nothing |
/// | `int` → `varchar(30)`, `bit` → `varchar(10)`, `datetime` → `varchar(30)`, `varchar(10)` → `int` | 1 | a change of family |
///
/// The rules are those that fit the ninety pairs, and a pair they do not cover is
/// **nullable**, which is what the majority of the pairs are and the side `tds` tolerates
/// (a `NULL` in a column announced `NOT NULL` is refused by `tds`, the opposite is not).
pub(crate) fn implicit_conversion_may_be_null(from: &SqlType, to: &SqlType) -> bool {
    !implicit_conversion_is_lossless(from, to)
}

/// The lossless pairs of [`implicit_conversion_may_be_null`], stated by family.
fn implicit_conversion_is_lossless(from: &SqlType, to: &SqlType) -> bool {
    use SqlType::{
        Bit, Date, DateTime, DateTime2, DateTimeOffset, Float, Int, Money, Real, SmallDateTime,
        SmallInt, SmallMoney, Time, TinyInt,
    };
    if from == to {
        return true;
    }
    match (integer_digits_of(from), integer_digits_of(to)) {
        (Some(f), Some(t)) => return f <= t,
        (Some(f), None) => {
            return match to {
                Float => matches!(from, Bit | TinyInt | SmallInt | Int),
                Real => matches!(from, Bit | TinyInt | SmallInt),
                Money => matches!(from, Bit | TinyInt | SmallInt | Int),
                SmallMoney => matches!(from, Bit | TinyInt | SmallInt),
                _ => exact_numeric_parts(to).is_some_and(|(p, s)| p - s >= f),
            };
        }
        (None, _) => {}
    }
    if let Some((p1, s1)) = exact_numeric_parts(from) {
        return match to {
            Money => p1 - s1 <= MONEY_INTEGRAL_DIGITS && s1 <= MONEY_SCALE,
            SmallMoney => p1 - s1 <= SMALLMONEY_INTEGRAL_DIGITS && s1 <= MONEY_SCALE,
            _ => exact_numeric_parts(to).is_some_and(|(p2, s2)| p2 - s2 >= p1 - s1 && s2 >= s1),
        };
    }
    match (from, to) {
        (Money, _) => exact_numeric_parts(to)
            .is_some_and(|(p, s)| p - s >= MONEY_INTEGRAL_DIGITS && s >= MONEY_SCALE),
        (SmallMoney, Money) => true,
        (SmallMoney, _) => exact_numeric_parts(to)
            .is_some_and(|(p, s)| p - s >= SMALLMONEY_INTEGRAL_DIGITS && s >= MONEY_SCALE),
        (Real, Float) => true,
        (Date, DateTime2(_) | DateTimeOffset(_)) => true,
        (SmallDateTime, DateTime | DateTime2(_)) => true,
        (Time(s1), Time(s2) | DateTime2(s2)) => s2 >= s1,
        (DateTime2(s1), DateTime2(s2) | DateTimeOffset(s2)) => s2 >= s1,
        (DateTimeOffset(s1), DateTimeOffset(s2)) => s2 >= s1,
        _ => match (character_capacity(from), character_capacity(to)) {
            (Some(f), Some(t)) => t.holds(&f),
            _ => match (binary_capacity(from), binary_capacity(to)) {
                (Some(_), Some(Len::Max)) => true,
                (Some(Len::Fixed(f)), Some(Len::Fixed(t))) => t >= f,
                _ => false,
            },
        },
    }
}

/// Integral digits a `money` holds (922 337 203 685 477.5807) and its scale.
const MONEY_INTEGRAL_DIGITS: u8 = 15;
/// Integral digits a `smallmoney` holds (214 748.3647).
const SMALLMONEY_INTEGRAL_DIGITS: u8 = 6;
/// Scale of `money` and `smallmoney`.
const MONEY_SCALE: u8 = 4;

/// The number of decimal digits an integer type spans, `bit` counted as one, or `None`
/// for a type that is not an integer.
fn integer_digits_of(ty: &SqlType) -> Option<u8> {
    match ty {
        SqlType::Bit => Some(1),
        SqlType::TinyInt => Some(3),
        SqlType::SmallInt => Some(5),
        SqlType::Int => Some(10),
        SqlType::BigInt => Some(19),
        _ => None,
    }
}

/// `(precision, scale)` of a `decimal` or a `numeric`, `None` for any other type.
fn exact_numeric_parts(ty: &SqlType) -> Option<(u8, u8)> {
    match ty {
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            Some((*precision, *scale))
        }
        _ => None,
    }
}

/// What a character type can hold: its length in characters and its width per character.
struct CharacterCapacity {
    len: Len,
    bytes_per_char: u16,
}

impl CharacterCapacity {
    /// Whether each value of `other` fits here: as many characters **and** as many
    /// bytes, which is what separates `nvarchar(10)` → `varchar(20)` (lossless) from
    /// `nvarchar(10)` → `varchar(10)` and `varchar(10)` → `nvarchar(5)` (both nullable).
    fn holds(&self, other: &CharacterCapacity) -> bool {
        match (self.len, other.len) {
            (Len::Max, _) => true,
            (Len::Fixed(_), Len::Max) => false,
            (Len::Fixed(mine), Len::Fixed(theirs)) => {
                mine >= theirs
                    && u32::from(mine) * u32::from(self.bytes_per_char)
                        >= u32::from(theirs) * u32::from(other.bytes_per_char)
            }
        }
    }
}

/// The capacity of a character type, `None` for any other.
fn character_capacity(ty: &SqlType) -> Option<CharacterCapacity> {
    let (len, bytes_per_char) = match ty {
        SqlType::Char(len) | SqlType::VarChar(len) => (*len, 1),
        SqlType::NChar(len) | SqlType::NVarChar(len) => (*len, 2),
        _ => return None,
    };
    Some(CharacterCapacity {
        len,
        bytes_per_char,
    })
}

/// The length of a binary type, `None` for any other.
fn binary_capacity(ty: &SqlType) -> Option<Len> {
    match ty {
        SqlType::Binary(len) | SqlType::VarBinary(len) => Some(*len),
        _ => None,
    }
}

/// Whether `ty` is a character type, the operands `+` concatenates instead of adding.
fn is_character(ty: &TypeInfo) -> bool {
    ty.ty.family() == TypeFamily::Character
}

/// `err`, given the line of the node that raised it when it carries none.
///
/// The errors of `types` know nothing of the batch: they come back with line 0 and the
/// binder is the last place that can say where they happened. An error raised by a
/// sub-expression already carries its own line and keeps it.
fn at_line(err: SqlError, line: u32) -> SqlError {
    if err.line == 0 {
        err.with_line(line)
    } else {
        err
    }
}

/// The internal error 50000 for a form the binder does not bind, not a message for the
/// client.
fn not_yet(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

/// The position of an expression, whichever variant it is.
fn span_of(e: &Expr) -> Span {
    match e {
        Expr::Literal(_, span)
        | Expr::Nested(_, span)
        | Expr::Variable { span, .. }
        | Expr::Binary { span, .. }
        | Expr::Unary { span, .. }
        | Expr::Function { span, .. }
        | Expr::Case { span, .. }
        | Expr::Cast { span, .. }
        | Expr::Convert { span, .. }
        | Expr::IsNull { span, .. }
        | Expr::In { span, .. }
        | Expr::Like { span, .. }
        | Expr::Between { span, .. }
        | Expr::Exists(_, span)
        | Expr::Subquery(_, span)
        | Expr::Quantified { span, .. }
        | Expr::Collate { span, .. }
        | Expr::Assign { span, .. }
        | Expr::NextValueFor { span, .. }
        | Expr::InvalidNiladic { span, .. }
        | Expr::Placeholder(span) => *span,
        Expr::Column(column) => column.span,
    }
}

/// The text message 4145 quotes — the token that **follows** the expression — and the line
/// that token sits on, which is the line SQL Server reports 4145 on.
///
/// SQL Server does not echo the expression it refused but the token it was looking at when
/// it gave up:
///
/// | query | `near '…'` |
/// |---|---|
/// | `SELECT 1 WHERE 1;` | `;` |
/// | `SELECT 1 WHERE 1 = 1 AND 2;` | `;` |
/// | `SELECT 1 WHERE 1 AND 1 = 1;` | `AND` |
/// | `SELECT CASE WHEN 1 THEN 1 END;` | `THEN` |
/// | `SELECT 1 WHERE 1` (no `;`) | `1` |
///
/// The last line is the end of the batch: there is no following token, and SQL Server falls
/// back on the last token of the expression itself.
///
/// **The line follows the same token**, and not the expression: 4145 is the one binding
/// error whose line is that of a token the user did not write the error about. With the
/// `SELECT` on line 2 — each batch puts the quoted token below the expression, so that
/// "the expression" and "the token" answer differently:
///
/// | batch | expression | quoted token | line of the 4145 |
/// |---|:-:|:-:|:-:|
/// | `SELECT` / `1` / `WHERE` / `1` / `+` / `1;` | 5 | `;` on 7 | **7** |
/// | `SELECT` / `1` / `WHERE` / `1` / `;` | 5 | `;` on 6 | **6** |
/// | `SELECT` / `1` / `WHERE` / `1` / `AND` / `1 = 1;` | 5 | `AND` on 6 | **6** |
/// | `SELECT` / `1` / `WHERE` / `(` / `1` / `)` / `;` | 5 | `;` on 8 | **8** |
/// | `SELECT` / `1` / `,` / `CASE` / `WHEN` / `1` / `THEN 1 END;` | 7 | `THEN` on 8 | **8** |
/// | `SELECT 1 WHERE` / `1 = 1` / `AND` / `1` / `;` | 5 | `;` on 6 | **6** |
///
/// A word is a run of identifier characters; anything else is a single character, which is
/// enough for the tokens that can follow a condition (`;`, `,`, `)`). Whitespace and
/// comments are skipped. A span that falls outside `text` yields the empty slice and the
/// line of the span: a wrong quote is bad, a panic on the query path is worse.
fn near_token_at<'a>(text: &'a str, span: &Span) -> (&'a str, u32) {
    let start = span.offset as usize;
    let end = start.saturating_add(span.len as usize);
    if let Some(rest) = text.get(end..) {
        let skipped = trivia_len(rest);
        if let Some(token) = first_token(&rest[skipped..]) {
            return (token, line_at(text, span, end.saturating_add(skipped)));
        }
    }
    match text.get(start..end).and_then(last_token_at) {
        Some((at, token)) => (token, line_at(text, span, start.saturating_add(at))),
        None => ("", span.line),
    }
}

/// The span of `expr`'s first token, which a `near` quotes for a syntax error.
///
/// A `near` names one token, not the whole expression's text: this is the span that
/// token starts on, and [`token_at`] reads it out of the batch.
pub(crate) fn syntax_span(expr: &Expr) -> Span {
    match expr {
        Expr::Literal(_, span)
        | Expr::Nested(_, span)
        | Expr::Exists(_, span)
        | Expr::Subquery(_, span)
        | Expr::Placeholder(span) => *span,
        Expr::Column(column) => column.span,
        Expr::Variable { span, .. }
        | Expr::Binary { span, .. }
        | Expr::Unary { span, .. }
        | Expr::Function { span, .. }
        | Expr::Case { span, .. }
        | Expr::Cast { span, .. }
        | Expr::Convert { span, .. }
        | Expr::IsNull { span, .. }
        | Expr::In { span, .. }
        | Expr::Like { span, .. }
        | Expr::Between { span, .. }
        | Expr::Quantified { span, .. }
        | Expr::Collate { span, .. }
        | Expr::Assign { span, .. }
        | Expr::NextValueFor { span, .. }
        | Expr::InvalidNiladic { span, .. } => *span,
    }
}

/// The token `span` starts on, and its line. See [`syntax_span`].
pub(crate) fn token_at<'a>(text: &'a str, span: &Span) -> (&'a str, u32) {
    let start = span.offset as usize;
    match text.get(start..).and_then(first_token) {
        Some(token) => (token, line_at(text, span, start)),
        None => ("", span.line),
    }
}

/// The first token of `text`, which must already have its trivia skipped
/// ([`crate::errors::skip_trivia`]), or `None` when there is none.
fn first_token(rest: &str) -> Option<&str> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if !is_word_char(first) {
        return Some(&rest[..first.len_utf8()]);
    }
    let end = chars
        .find(|(_, c)| !is_word_char(*c))
        .map_or(rest.len(), |(i, _)| i);
    Some(&rest[..end])
}

/// The last token of `text` and the byte it starts at: the fallback of [`near_token_at`] at
/// the end of a batch, where the offset is what gives the token its line.
fn last_token_at(text: &str) -> Option<(usize, &str)> {
    let text = text.trim_end();
    let last = text.chars().next_back()?;
    if !is_word_char(last) {
        let at = text.len() - last.len_utf8();
        return Some((at, &text[at..]));
    }
    let at = text
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    Some((at, &text[at..]))
}

/// Whether `c` can be part of a word token: a letter, a digit, `_`, `@`, `#` or `$`, the
/// characters a T-SQL identifier is made of.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$')
}

#[cfg(test)]
mod tests {
    use super::{
        Scope, bind_condition, bind_expr, exact_numeric_view, implicit_conversion_may_be_null,
        integer_digits, near_token_at,
    };
    use vauban_errors::{SqlError, SqlResult};
    use vauban_parser::{
        BinaryOp as AstBinaryOp, CaseArm, ColumnRef, Expr, Ident, InList, Literal, ObjectName,
        Span, UnaryOp,
    };
    use vauban_types::{Collation, Len, SqlType, TypeInfo, Value};

    use crate::bound::{BoundExpr, BoundExprKind, CompareOp, LogicalOp};
    use crate::call::tests::registry;
    use crate::context::{BindContext, SessionOptions, VariableScope};

    #[test]
    fn invalid_niladic_binding_preserves_variable_order() {
        registry();
        for name in [
            "CURRENT_TIMESTAMP()",
            "CURRENT_USER()",
            "SESSION_USER()",
            "SYSTEM_USER()",
            "USER()",
            "CURRENT_DATE",
            "CURRENT_TIME",
            "[CURRENT_TIMESTAMP]()",
            "[CURRENT_DATE](1)",
        ] {
            let number = if matches!(name, "CURRENT_DATE" | "CURRENT_TIME") {
                156
            } else {
                102
            };
            for (variable, before, expected) in [
                ("@missing", true, 137),
                ("@missing", false, number),
                ("@int", true, number),
                ("@int", false, number),
            ] {
                let sql = if before {
                    format!("SELECT {variable}, {name};")
                } else {
                    format!("SELECT {name}, {variable};")
                };
                let batch =
                    vauban_parser::parse_batch(&sql, &vauban_parser::ParseOptions::default())
                        .unwrap();
                let mut ctx = BindContext::scalar(&sql, SessionOptions::default());
                ctx.variables = &Vars;
                let error = crate::bind(&batch.statements[0], &ctx).unwrap_err();
                assert_eq!(error.number, expected, "{sql}");
                assert_eq!(
                    (error.severity, error.state),
                    (15, if expected == 137 { 2 } else { 1 }),
                    "{sql}"
                );
            }
        }
    }

    /// The error `sql` answers with, bound through the whole statement so that the order
    /// the binder walks the tree in is the one under test.
    fn statement_error(sql: &str) -> SqlError {
        registry();
        let batch =
            vauban_parser::parse_batch(sql, &vauban_parser::ParseOptions::default()).unwrap();
        let mut ctx = BindContext::scalar(sql, SessionOptions::default());
        ctx.variables = &Vars;
        crate::bind(&batch.statements[0], &ctx).unwrap_err()
    }

    /// A condition that opens on a parenthesised niladic spelling answers 4145 quoting the
    /// call parenthesis, and the shapes that stop that walk keep their own number. Each of
    /// the nine batches below compares with `=`.
    #[test]
    fn a_niladic_call_that_opens_a_condition_is_4145() {
        let quoting_the_parenthesis = SqlError::non_boolean_expression("(").message;
        for sql in [
            "SELECT 1 WHERE CURRENT_TIMESTAMP()=1;",
            "SELECT 1 WHERE CURRENT_TIMESTAMP(1)=1;",
            "SELECT 1 WHERE [CURRENT_TIMESTAMP]()=1;",
            "SELECT 1 WHERE [CURRENT_DATE]()=1;",
            "SELECT CASE WHEN CURRENT_TIMESTAMP()=1 THEN 1 ELSE 0 END;",
        ] {
            let error = statement_error(sql);
            assert_eq!(
                (error.number, error.severity, error.state, error.line),
                (4145, 15, 1, 1),
                "{sql}"
            );
            assert_eq!(error.message, quoting_the_parenthesis, "{sql}");
        }

        // The discriminating side: the spelling on the right of the comparison, the same
        // spelling inside parentheses, and the two names written without a call, which
        // SQL Server answers differently from 4145.
        for (sql, number, near) in [
            ("SELECT 1 WHERE 1=CURRENT_TIMESTAMP();", 102, "')'"),
            ("SELECT 1 WHERE 1=CURRENT_TIMESTAMP(1);", 102, "'1'"),
            ("SELECT 1 WHERE (CURRENT_TIMESTAMP())=1;", 102, "'('"),
            ("SELECT 1 WHERE CURRENT_DATE=1;", 156, "'CURRENT_DATE'"),
        ] {
            let error = statement_error(sql);
            assert_eq!((error.number, error.line), (number, 1), "{sql}");
            assert!(error.message.contains(near), "{sql}: {}", error.message);
        }
    }

    /// An unresolved call around an invalid niladic spelling: the answer is about the
    /// argument, and about an undeclared variable written before it, not about the name of
    /// the enclosing function, which does not exist either.
    #[test]
    fn an_argument_is_diagnosed_before_the_enclosing_call() {
        for (sql, number, named) in [
            ("SELECT unknown(CURRENT_TIMESTAMP());", 102, "'('"),
            ("SELECT unknown(CURRENT_DATE);", 156, "'CURRENT_DATE'"),
            (
                "SELECT unknown(@missing,CURRENT_TIMESTAMP());",
                137,
                "\"@missing\"",
            ),
            (
                "SELECT ISNULL(@missing,CURRENT_TIMESTAMP());",
                137,
                "\"@missing\"",
            ),
        ] {
            let error = statement_error(sql);
            assert_eq!((error.number, error.line), (number, 1), "{sql}");
            assert!(error.message.contains(named), "{sql}: {}", error.message);
        }

        // The preflight leaves the ordinary path alone: without such a spelling among its
        // arguments, an unresolved call still raises the 195 of the registry. A control of
        // the four batches above.
        let error = statement_error("SELECT unknown(1);");
        assert_eq!(error.number, 195, "{}", error.message);
    }

    /// The tests build the AST node by hand: what is under test is the binding of a
    /// shape, and the shape is the parser's contract, written down in `parser::ast::expr`.
    ///
    /// A span on line 1, offset 0: the line alone survives into a `BoundExpr`.
    fn any_span() -> Span {
        Span {
            line: 1,
            column: 1,
            offset: 0,
            len: 0,
        }
    }

    /// An integer literal, as the lexer would classify it.
    fn int(text: &str) -> Expr {
        Expr::Literal(Literal::Integer(text.to_owned()), any_span())
    }

    /// A fixed-point literal (`1.5` is a `numeric(2, 1)`).
    fn dec(text: &str) -> Expr {
        Expr::Literal(Literal::Decimal(text.to_owned()), any_span())
    }

    /// A non-Unicode string literal (`'a'` is a `varchar(1)`).
    fn text(value: &str) -> Expr {
        Expr::Literal(
            Literal::Str {
                value: value.to_owned(),
                unicode: false,
            },
            any_span(),
        )
    }

    /// A binary literal (`0x00` is a non-nullable `varbinary(1)`).
    fn hex(text: &str) -> Expr {
        Expr::Literal(Literal::Binary(text.to_owned()), any_span())
    }

    /// The untyped `NULL`, which `literal.rs` types `int`, nullable.
    fn null() -> Expr {
        Expr::Literal(Literal::Null, any_span())
    }

    /// A variable reference, resolved by [`Vars`].
    fn var(name: &str) -> Expr {
        Expr::Variable {
            name: name.to_owned(),
            span: any_span(),
        }
    }

    fn binary(op: AstBinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            op_span: any_span(),
            left: Box::new(left),
            right: Box::new(right),
            span: any_span(),
        }
    }

    fn unary(op: UnaryOp, expr: Expr) -> Expr {
        Expr::Unary {
            op,
            expr: Box::new(expr),
            span: any_span(),
        }
    }

    /// A one-part column reference on `line`.
    fn column(name: &str, line: u32) -> Expr {
        Expr::Column(ColumnRef {
            qualifier: None,
            name: Ident {
                value: name.to_owned(),
                quoted: false,
            },
            span: Span {
                line,
                column: 8,
                offset: 7,
                len: name.len() as u32,
            },
        })
    }

    /// The variables the tests declare, so that an operand can have a type no literal has
    /// (`bit`, `uniqueidentifier`, `datetime`, `tinyint`).
    struct Vars;

    impl VariableScope for Vars {
        fn type_of(&self, name: &str) -> Option<TypeInfo> {
            let ty = match name {
                "@bit" => SqlType::Bit,
                "@guid" => SqlType::UniqueIdentifier,
                "@datetime" => SqlType::DateTime,
                "@tiny" => SqlType::TinyInt,
                "@int" => SqlType::Int,
                "@float" => SqlType::Float,
                "@varchar" => SqlType::VarChar(Len::Fixed(10)),
                "@big" => SqlType::BigInt,
                "@small" => SqlType::SmallInt,
                "@real" => SqlType::Real,
                "@money" => SqlType::Money,
                "@smallmoney" => SqlType::SmallMoney,
                "@dec" => SqlType::Decimal {
                    precision: 10,
                    scale: 2,
                },
                // The type whose common type with an `int` holds no `int` at all: it is
                // what `arith_target` is stated on.
                "@tiny_scale" => SqlType::Decimal {
                    precision: 38,
                    scale: 38,
                },
                // The nullable ones, as a `DECLARE`d variable is on the server: the
                // nullability tests are written over them.
                "@nint" => return Some(TypeInfo::new(SqlType::Int, true)),
                "@ndec" => {
                    return Some(TypeInfo::new(
                        SqlType::Decimal {
                            precision: 10,
                            scale: 2,
                        },
                        true,
                    ));
                }
                "@nvarchar" => {
                    return Some(TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true));
                }
                "@nbin" => {
                    return Some(TypeInfo::new(SqlType::VarBinary(Len::Fixed(10)), true));
                }
                _ => return None,
            };
            Some(TypeInfo::new(ty, false))
        }
    }

    static VARS: Vars = Vars;

    /// A binding context over `text`, with the variables above in scope.
    fn ctx(text: &str) -> BindContext<'_> {
        BindContext {
            text,
            catalog: None,
            database: "master",
            default_schema: "dbo",
            variables: &VARS,
            options: SessionOptions::default(),
        }
    }

    /// The bound form of an expression, or the error it raised.
    ///
    /// The registry is filled first: a function call, a `@@x` variable and a bare niladic
    /// name go through `sysfn::lookup`, and a test must not depend on whether another test
    /// filled it.
    fn bind(e: &Expr) -> SqlResult<BoundExpr> {
        registry();
        bind_expr(e, &ctx("SELECT 1"), &Scope::empty())
    }

    /// The bound form of an expression the test expects to bind.
    fn b(e: &Expr) -> BoundExpr {
        bind(e).expect("binds")
    }

    /// The error an expression the test expects to fail raised.
    fn err(e: &Expr) -> SqlError {
        bind(e).expect_err("fails to bind")
    }

    /// The `ty` of the left and right operands of a binary bound node.
    fn operands(e: &BoundExpr) -> (&BoundExpr, &BoundExpr) {
        match &e.kind {
            BoundExprKind::Arith { left, right, .. }
            | BoundExprKind::Compare { left, right, .. }
            | BoundExprKind::Logical { left, right, .. } => (left, right),
            other => panic!("expected a binary node, got {other:?}"),
        }
    }

    /// Whether a node is a binder-inserted conversion towards `ty`.
    fn is_convert_to(e: &BoundExpr, ty: SqlType) -> bool {
        matches!(
            &e.kind,
            BoundExprKind::Convert {
                style: None,
                try_: false,
                ..
            }
        ) && e.ty.ty == ty
    }

    #[test]
    fn arithmetic_types() {
        assert_eq!(
            b(&binary(AstBinaryOp::Add, int("1"), int("1"))).ty.ty,
            SqlType::Int
        );
        // `1 + 1.5` is a numeric(3, 1) and **not** a numeric(12, 1): the integer literal
        // enters the precision table with its own digit count (precision 3, scale 1,
        // against the 12 of `CAST(1 AS int) + CAST(1.5 AS numeric(2,1))`).
        assert_eq!(
            b(&binary(AstBinaryOp::Add, int("1"), dec("1.5"))).ty.ty,
            SqlType::Numeric {
                precision: 3,
                scale: 1
            }
        );
        // Two integers divide as integers.
        assert_eq!(
            b(&binary(AstBinaryOp::Div, int("1"), int("2"))).ty.ty,
            SqlType::Int
        );
        assert!(
            b(&binary(AstBinaryOp::Div, dec("1.0"), int("2")))
                .ty
                .ty
                .is_exact_numeric()
        );
        // An `int` typed by a variable keeps the precision of its type: numeric(12, 1).
        assert_eq!(
            b(&binary(AstBinaryOp::Add, var("@int"), dec("1.5"))).ty.ty,
            SqlType::Numeric {
                precision: 12,
                scale: 1
            }
        );
    }

    /// The `Convert` nodes `bind_arith` inserts on its operands, and the ones it does not,
    /// each fixed by what SQL Server answers to the query named next to it.
    #[test]
    fn arith_inserts_the_conversion() {
        // `SELECT 1.5 * 2;` answers `3.0`: the
        // integer is converted, the `numeric(2, 1)` is not, and the product is a
        // `numeric(4, 1)` — a type neither operand has.
        let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), int("2")));
        let (left, right) = operands(&mul);
        assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
        assert!(
            is_convert_to(
                right,
                SqlType::Numeric {
                    precision: 1,
                    scale: 0
                }
            ),
            "{right:?}"
        );
        assert_eq!(
            mul.ty.ty,
            SqlType::Numeric {
                precision: 4,
                scale: 1
            }
        );

        // `SELECT 1.5 * 2.5;`: two exact numerics convert nothing at all, whatever their
        // precision and scale. `SELECT CAST(1.5 AS decimal(5,2)) * CAST(1.5 AS
        // decimal(5,2));` is a `decimal(11,4)` without
        // either operand moving.
        let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), dec("2.5")));
        let (left, right) = operands(&mul);
        assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
        assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");

        // An `int` that is not a literal enters with the precision of its own type, and
        // that is the type it is converted to: `numeric(10, 0)`, neither the common type
        // of the pair (`numeric(11, 1)`) nor the type of the result (`numeric(12, 1)`).
        let add = b(&binary(AstBinaryOp::Add, var("@int"), dec("1.5")));
        let (left, right) = operands(&add);
        assert!(
            is_convert_to(
                left,
                SqlType::Numeric {
                    precision: 10,
                    scale: 0
                }
            ),
            "{left:?}"
        );
        assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");

        // What rules out the common type: `SELECT CAST(12345 AS int) + CAST(0.1
        // AS decimal(38,38));` answers `12345.1000000000000000000000000000`, so the `int`
        // is not converted to the
        // `decimal(38, 38)` the pair has in common — which holds no `12345`.
        let add = b(&binary(AstBinaryOp::Add, var("@int"), var("@tiny_scale")));
        let (left, _) = operands(&add);
        assert!(
            is_convert_to(
                left,
                SqlType::Numeric {
                    precision: 10,
                    scale: 0
                }
            ),
            "{left:?}"
        );

        // Two integers convert nothing: `eval_binary` widens both to `i64` on its own, and
        // `SELECT CAST(200 AS tinyint) * CAST(200 AS tinyint);` overflows a `tinyint`
        // while `CAST(200 AS tinyint) * CAST(200 AS int)` answers `40000` — the result
        // type alone makes the difference.
        for pair in [("@tiny", "@tiny"), ("@tiny", "@int"), ("@int", "@big")] {
            let mul = b(&binary(AstBinaryOp::Mul, var(pair.0), var(pair.1)));
            let (left, right) = operands(&mul);
            assert!(
                matches!(left.kind, BoundExprKind::Variable { .. }),
                "{left:?}"
            );
            assert!(
                matches!(right.kind, BoundExprKind::Variable { .. }),
                "{right:?}"
            );
        }

        // `money` against an `int` stays in the money family and the `int` joins it. What
        // proves the *operand* is what moves, and towards the narrow type:
        // `SELECT CAST(0 AS smallmoney) * CAST(3000000 AS int);` answers an arithmetic
        // overflow for `smallmoney` on the value 3000000 although the product is 0 and
        // although a `money` holds 3000000 — so neither the result nor a promotion of the
        // pair explains the failure, the conversion of the operand alone does.
        let add = b(&binary(AstBinaryOp::Add, var("@smallmoney"), var("@int")));
        let (left, right) = operands(&add);
        assert!(
            matches!(left.kind, BoundExprKind::Variable { .. }),
            "{left:?}"
        );
        assert!(is_convert_to(right, SqlType::SmallMoney), "{right:?}");

        // A `money` against a `decimal` leaves the money family instead, as
        // `numeric(19, 4)`: `SELECT CAST(1.5 AS money) + CAST(2.25 AS decimal(5,2));` is a
        // `decimal(20,4)`, which is that reading of `money`
        // put through the precision table.
        let add = b(&binary(AstBinaryOp::Add, var("@money"), dec("2.25")));
        let (left, right) = operands(&add);
        assert!(
            is_convert_to(
                left,
                SqlType::Numeric {
                    precision: 19,
                    scale: 4
                }
            ),
            "{left:?}"
        );
        assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");

        // A `bit` carries no precision of its own either, and takes the common type like
        // a character operand. `SELECT CAST(1.5 AS decimal(2,1)) * CAST(1 AS bit);` is a
        // `decimal(5, 2)`, which is `decimal(2,1) * decimal(2,1)`; a `numeric(1, 0)`
        // reading would have given a `decimal(4, 1)`.
        let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), var("@bit")));
        let (left, right) = operands(&mul);
        assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
        assert!(
            is_convert_to(
                right,
                SqlType::Numeric {
                    precision: 2,
                    scale: 1
                }
            ),
            "{right:?}"
        );

        // The witness that makes that answer readable: the same product with a `tinyint`,
        // which does carry a precision, is a `decimal(6, 1)`. Two queries of the same
        // shape, two different answers.
        let mul = b(&binary(AstBinaryOp::Mul, dec("1.5"), var("@tiny")));
        let (_, right) = operands(&mul);
        assert!(
            is_convert_to(
                right,
                SqlType::Numeric {
                    precision: 3,
                    scale: 0
                }
            ),
            "{right:?}"
        );

        // And what settles it: the common type of a `bit` and a `decimal(38, 38)` holds
        // no 1, so converting the operand to it overflows — which is what SQL Server
        // answers, an arithmetic overflow converting `tinyint` to `numeric`. Under the
        // `numeric(1, 0)` reading the sum would have been a value.
        let add = b(&binary(AstBinaryOp::Add, var("@bit"), var("@tiny_scale")));
        let (left, _) = operands(&add);
        assert!(
            is_convert_to(
                left,
                SqlType::Decimal {
                    precision: 38,
                    scale: 38
                }
            ),
            "{left:?}"
        );

        // A character operand has no precision of its own and takes the common type:
        // `SELECT '1' + 2;` answers the `int` 3.
        let add = b(&binary(AstBinaryOp::Add, text("1"), var("@int")));
        let (left, right) = operands(&add);
        assert!(is_convert_to(left, SqlType::Int), "{left:?}");
        assert!(
            matches!(right.kind, BoundExprKind::Variable { .. }),
            "{right:?}"
        );

        // Two approximate operands convert nothing (`real * float` is a `float`), and an
        // exact operand facing one becomes a `float`.
        let mul = b(&binary(AstBinaryOp::Mul, var("@real"), var("@float")));
        let (left, right) = operands(&mul);
        assert!(
            matches!(left.kind, BoundExprKind::Variable { .. }),
            "{left:?}"
        );
        assert!(
            matches!(right.kind, BoundExprKind::Variable { .. }),
            "{right:?}"
        );
        let mul = b(&binary(AstBinaryOp::Mul, var("@float"), int("2")));
        let (_, right) = operands(&mul);
        assert!(is_convert_to(right, SqlType::Float), "{right:?}");

        // `datetime + 1` is a `datetime` and the number joins it (`Jan  2 2000 12:00AM`).
        // Same proof as for the money family that the operand is converted:
        // `SELECT CAST('9999-12-31' AS datetime) + CAST(-100000 AS int);` answers an
        // arithmetic overflow converting the expression to `datetime` although the sum
        // falls in 9726, a date `datetime` holds — it is `-100000`, read as a date of
        // 1626, that leaves the type.
        let add = b(&binary(AstBinaryOp::Add, var("@datetime"), int("1")));
        let (left, right) = operands(&add);
        assert!(
            matches!(left.kind, BoundExprKind::Variable { .. }),
            "{left:?}"
        );
        assert!(is_convert_to(right, SqlType::DateTime), "{right:?}");

        // The bitwise operators follow the same rule: `CAST(1 AS bit) & CAST(200 AS int)`
        // is an `int`, so the `bit` is the operand that moves.
        let and = b(&binary(AstBinaryOp::BitAnd, var("@bit"), var("@int")));
        let (left, right) = operands(&and);
        assert!(is_convert_to(left, SqlType::Int), "{left:?}");
        assert!(
            matches!(right.kind, BoundExprKind::Variable { .. }),
            "{right:?}"
        );

        // A concatenation converts nothing: the two strings reach `eval_binary` as they
        // are (`SELECT 'a' + 'b';`).
        let concat = b(&binary(AstBinaryOp::Add, text("a"), text("b")));
        let (left, right) = operands(&concat);
        assert!(matches!(left.kind, BoundExprKind::Literal(_)), "{left:?}");
        assert!(matches!(right.kind, BoundExprKind::Literal(_)), "{right:?}");
    }

    /// The precondition of [`vauban_types::eval_binary`], checked over the shapes above:
    /// "a pair of operands from two different families is a broken precondition".
    ///
    /// The check is on the **family** and not on the type, because that is what
    /// `eval_binary` dispatches on and because SQL Server does not equalise the types: the
    /// operands of `CAST(12345 AS int) + CAST(0.1 AS decimal(38,38))` end up
    /// `numeric(10, 0)` and `decimal(38, 38)`, and equalising them would answer 8115 where
    /// SQL Server answers a value (see [`arith_target`]).
    #[test]
    fn every_arithmetic_pair_reaches_eval_binary_in_one_family() {
        let numbers = [
            "@bit",
            "@tiny",
            "@small",
            "@int",
            "@big",
            "@smallmoney",
            "@money",
            "@real",
            "@float",
            "@tiny_scale",
            "@varchar",
        ];
        let operators = [
            AstBinaryOp::Add,
            AstBinaryOp::Sub,
            AstBinaryOp::Mul,
            AstBinaryOp::Div,
            AstBinaryOp::Mod,
            AstBinaryOp::BitAnd,
            AstBinaryOp::BitOr,
            AstBinaryOp::BitXor,
        ];
        let mut checked = 0_u32;
        for op in operators {
            for left in numbers {
                for right in numbers {
                    // A refused pair (`bit * bit`, `float % float`, a bitwise operator on a
                    // `decimal`…) does not reach the executor.
                    let Ok(bound) = bind(&binary(op, var(left), var(right))) else {
                        continue;
                    };
                    let (a, b) = operands(&bound);
                    assert_eq!(
                        a.ty.ty.family(),
                        b.ty.ty.family(),
                        "{left} {op:?} {right}: {a:?} against {b:?}"
                    );
                    checked += 1;
                }
            }
        }
        // The literals and the temporal operands, which no variable of `Vars` spells.
        for (op, left, right) in [
            (AstBinaryOp::Mul, dec("1.5"), int("2")),
            (AstBinaryOp::Mul, int("2"), dec("1.5")),
            (AstBinaryOp::Add, int("1"), int("1")),
            (AstBinaryOp::Add, dec("1.5"), dec("2.5")),
            (AstBinaryOp::Add, text("a"), text("b")),
            (AstBinaryOp::Add, var("@datetime"), int("1")),
            (AstBinaryOp::Sub, var("@datetime"), var("@datetime")),
            (AstBinaryOp::Add, text("1"), int("2")),
        ] {
            let bound = b(&binary(op, left, right));
            let (a, b) = operands(&bound);
            assert_eq!(a.ty.ty.family(), b.ty.ty.family(), "{a:?} against {b:?}");
            checked += 1;
        }
        // A guard against a loop that silently binds nothing.
        assert!(checked > 100, "only {checked} pairs bound");
    }

    /// [`exact_numeric_view`] answers the precision and scale defined for the data type of
    /// the expression — **minus `bit`**, which a `numeric(1, 0)` reading would include and
    /// which SQL Server types otherwise.
    ///
    /// Each of the six is fixed by a query: `CAST(1.5 AS decimal(2,1)) + CAST(1 AS t)`
    /// reports the precision the reading of `t` predicts — 5 for a `tinyint`, 7 for a
    /// `smallint`, 12 for an `int`, 21 for a `bigint`, 11 and scale 4 for a `smallmoney`,
    /// 20 and scale 4 for a `money`. The `bit`, whose 3 and 1 agree with both readings,
    /// settles nothing.
    #[test]
    fn an_operand_enters_with_the_precision_of_its_own_type() {
        for (ty, precision, scale) in [
            (SqlType::TinyInt, 3, 0),
            (SqlType::SmallInt, 5, 0),
            (SqlType::Int, 10, 0),
            (SqlType::BigInt, 19, 0),
            (SqlType::SmallMoney, 10, 4),
            (SqlType::Money, 19, 4),
        ] {
            let view = exact_numeric_view(&TypeInfo::new(ty, false))
                .unwrap_or_else(|| panic!("{ty:?} has an exact-numeric reading"));
            assert_eq!(view.ty, SqlType::Numeric { precision, scale }, "{ty:?}");
        }
        // The types that have none: they are converted to the common type instead. `bit`
        // leads the list: `CAST(1.5 AS decimal(2,1)) * CAST(1 AS bit)` is a
        // `decimal(5, 2)`, which is `decimal(2,1) * decimal(2,1)`, and
        // `CAST(1 AS bit) + CAST(0.1 AS decimal(38,38))` overflows.
        for ty in [
            SqlType::Bit,
            SqlType::Float,
            SqlType::Real,
            SqlType::VarChar(Len::Fixed(10)),
            SqlType::DateTime,
            SqlType::UniqueIdentifier,
        ] {
            assert!(
                exact_numeric_view(&TypeInfo::new(ty, false)).is_none(),
                "{ty:?}"
            );
        }
    }

    #[test]
    fn concat_is_not_add() {
        let concat = b(&binary(AstBinaryOp::Add, text("a"), text("b")));
        assert!(matches!(
            concat.kind,
            BoundExprKind::Arith {
                op: vauban_types::BinaryOp::Concat,
                ..
            }
        ));
        assert_eq!(concat.ty.ty, SqlType::VarChar(Len::Fixed(2)));

        // One character operand is not enough: `types` converts the string to the number.
        let add = b(&binary(AstBinaryOp::Add, int("1"), text("a")));
        assert!(matches!(
            add.kind,
            BoundExprKind::Arith {
                op: vauban_types::BinaryOp::Add,
                ..
            }
        ));
        assert_eq!(add.ty.ty, SqlType::Int);
    }

    #[test]
    fn invalid_operand_is_8117() {
        // `SELECT CAST(1 AS bit) * CAST(1 AS bit);` answers 8117 naming `bit` and the
        // multiply operator. The binder relays the error of `types` and adds the line.
        let e = err(&binary(AstBinaryOp::Mul, var("@bit"), var("@bit")));
        assert_eq!(e.number, 8117);
        assert_eq!(e.severity, 16);
        assert_eq!(
            e.message,
            SqlError::invalid_operand_type("bit", "multiply").message
        );
    }

    #[test]
    fn error_numbers_are_not_retranslated() {
        // 206: `SELECT CAST('0E98…' AS uniqueidentifier) + 1;` (severity 16, state 2).
        // Not 8117.
        let clash = err(&binary(AstBinaryOp::Add, var("@guid"), int("1")));
        assert_eq!(clash.number, 206);
        assert_eq!(
            clash.message,
            SqlError::operand_type_clash("uniqueidentifier", "int").message
        );

        // 257: a date with `*`, `/` or `%`.
        let implicit = err(&binary(AstBinaryOp::Mul, var("@datetime"), int("1")));
        assert_eq!(implicit.number, 257);
        assert_eq!(
            implicit.message,
            SqlError::implicit_conversion_not_allowed("datetime", "int").message
        );

        // 402 on SQL Server (`bit` and `bit` in the add operator), which the `errors`
        // crate cannot spell yet, so `types` raises 8117 there. Either way the binder
        // passes the number on untouched.
        let pair = err(&binary(AstBinaryOp::Add, var("@bit"), var("@bit")));
        assert!(matches!(pair.number, 402 | 8117), "{pair:?}");
    }

    #[test]
    fn comparison_is_a_predicate() {
        let eq = b(&binary(AstBinaryOp::Eq, int("1"), int("1")));
        assert!(eq.is_predicate());
        assert!(matches!(
            eq.kind,
            BoundExprKind::Compare {
                op: CompareOp::Eq,
                ..
            }
        ));
        assert_eq!(eq.ty.ty, SqlType::Bit);

        // `!<` and `!>` are normalised into their meaning.
        assert!(matches!(
            b(&binary(AstBinaryOp::NotLt, int("1"), int("2"))).kind,
            BoundExprKind::Compare {
                op: CompareOp::Ge,
                ..
            }
        ));
        assert!(matches!(
            b(&binary(AstBinaryOp::NotGt, int("1"), int("2"))).kind,
            BoundExprKind::Compare {
                op: CompareOp::Le,
                ..
            }
        ));
    }

    #[test]
    fn comparison_inserts_the_conversion() {
        let cmp = b(&binary(AstBinaryOp::Eq, int("1"), text("1")));
        let (left, right) = operands(&cmp);
        // `int` outranks `varchar`, so the string is converted and not the number.
        assert!(matches!(left.kind, BoundExprKind::Literal(Value::I32(1))));
        assert!(is_convert_to(right, SqlType::Int), "{right:?}");
    }

    #[test]
    fn logical_operators() {
        let and = b(&binary(
            AstBinaryOp::And,
            binary(AstBinaryOp::Eq, int("1"), int("1")),
            binary(AstBinaryOp::Eq, int("2"), int("2")),
        ));
        assert!(matches!(
            and.kind,
            BoundExprKind::Logical {
                op: LogicalOp::And,
                ..
            }
        ));
        assert_eq!(and.ty.ty, SqlType::Bit);

        let not = b(&unary(
            UnaryOp::Not,
            binary(AstBinaryOp::Eq, int("1"), int("1")),
        ));
        assert!(matches!(not.kind, BoundExprKind::Not(_)));

        // An operand of `AND` that is not a condition is 4145, like a `WHERE`.
        assert_eq!(
            err(&binary(
                AstBinaryOp::And,
                int("1"),
                binary(AstBinaryOp::Eq, int("1"), int("1")),
            ))
            .number,
            4145
        );
    }

    #[test]
    fn non_boolean_condition_is_4145() {
        // `SELECT 1 WHERE 1` (no semicolon) answers `near '1'`, and the same batch with a
        // semicolon answers `near ';'`: SQL Server echoes the token that **follows** the
        // expression, and falls back on its last token at the end of the batch.
        let source = "SELECT 1 WHERE 1";
        let one = Expr::Literal(
            Literal::Integer("1".to_owned()),
            Span {
                line: 1,
                column: 16,
                offset: 15,
                len: 1,
            },
        );
        let e =
            bind_condition(&one, &ctx(source), &Scope::empty()).expect_err("1 is not a condition");
        assert_eq!(e.number, 4145);
        assert_eq!(e.severity, 15);
        assert_eq!(e.message, SqlError::non_boolean_expression("1").message);
        assert_eq!(e.line, 1);

        let e = bind_condition(&one, &ctx("SELECT 1 WHERE 1;"), &Scope::empty())
            .expect_err("1 is not a condition");
        assert!(e.message.contains("near ';'"), "{}", e.message);
    }

    #[test]
    fn between_is_desugared() {
        let between = |negated| Expr::Between {
            expr: Box::new(int("1")),
            low: Box::new(int("0")),
            high: Box::new(int("2")),
            negated,
            span: any_span(),
        };

        let plain = b(&between(false));
        let (left, right) = operands(&plain);
        assert!(matches!(
            plain.kind,
            BoundExprKind::Logical {
                op: LogicalOp::And,
                ..
            }
        ));
        assert!(matches!(
            left.kind,
            BoundExprKind::Compare {
                op: CompareOp::Ge,
                ..
            }
        ));
        assert!(matches!(
            right.kind,
            BoundExprKind::Compare {
                op: CompareOp::Le,
                ..
            }
        ));

        let negated = b(&between(true));
        let (left, right) = operands(&negated);
        assert!(matches!(
            negated.kind,
            BoundExprKind::Logical {
                op: LogicalOp::Or,
                ..
            }
        ));
        assert!(matches!(
            left.kind,
            BoundExprKind::Compare {
                op: CompareOp::Lt,
                ..
            }
        ));
        assert!(matches!(
            right.kind,
            BoundExprKind::Compare {
                op: CompareOp::Gt,
                ..
            }
        ));
    }

    #[test]
    fn case_result_type() {
        let searched = |else_: Option<Expr>| Expr::Case {
            operand: None,
            arms: vec![CaseArm {
                when: binary(AstBinaryOp::Eq, int("1"), int("1")),
                then: int("1"),
            }],
            else_: else_.map(Box::new),
            span: any_span(),
        };

        // `SELECT SQL_VARIANT_PROPERTY(CASE WHEN 1 = 1 THEN 1 ELSE 2.5 END, 'Precision');`
        // answers 2, and 'Scale' answers 1: the common type is numeric(2, 1), the type of
        // the `ELSE` branch, because the integer literal enters with one digit. The `THEN`
        // branch alone therefore carries a conversion — `CAST(1 AS int)` would put one on
        // each (numeric(11, 1)).
        let case = b(&searched(Some(dec("2.5"))));
        assert_eq!(
            case.ty.ty,
            SqlType::Numeric {
                precision: 2,
                scale: 1
            }
        );
        let BoundExprKind::Case {
            operand,
            arms,
            else_,
        } = &case.kind
        else {
            panic!("expected a CASE, got {:?}", case.kind);
        };
        assert!(operand.is_none(), "the simple CASE is desugared");
        assert!(
            is_convert_to(
                &arms[0].then,
                SqlType::Numeric {
                    precision: 2,
                    scale: 1
                }
            ),
            "{:?}",
            arms[0].then
        );
        let else_ = else_.as_ref().expect("ELSE 2.5");
        assert!(matches!(
            else_.kind,
            BoundExprKind::Literal(Value::Decimal(_))
        ));

        // No `ELSE`: the `CASE` answers NULL when no arm matches.
        let without_else = b(&searched(None));
        assert_eq!(without_else.ty.ty, SqlType::Int);
        assert!(without_else.ty.nullable);
        assert!(!b(&searched(Some(int("2")))).ty.nullable);
    }

    #[test]
    fn simple_case_is_desugared_into_a_searched_one() {
        let simple = Expr::Case {
            operand: Some(Box::new(int("1"))),
            arms: vec![CaseArm {
                when: int("1"),
                then: text("a"),
            }],
            else_: None,
            span: any_span(),
        };
        let case = b(&simple);
        assert_eq!(case.ty.ty, SqlType::VarChar(Len::Fixed(1)));
        let BoundExprKind::Case { operand, arms, .. } = &case.kind else {
            panic!("expected a CASE, got {:?}", case.kind);
        };
        assert!(operand.is_none(), "the operand moved into each WHEN");
        assert!(arms[0].when.is_predicate());
        assert!(matches!(
            arms[0].when.kind,
            BoundExprKind::Compare {
                op: CompareOp::Eq,
                ..
            }
        ));
    }

    #[test]
    fn is_null_type() {
        let is_null = b(&Expr::IsNull {
            expr: Box::new(null()),
            negated: false,
            span: any_span(),
        });
        assert_eq!(is_null.ty.ty, SqlType::Bit);
        assert!(!is_null.ty.nullable);
        assert!(is_null.is_predicate());

        let is_not_null = b(&Expr::IsNull {
            expr: Box::new(null()),
            negated: true,
            span: any_span(),
        });
        assert!(matches!(
            is_not_null.kind,
            BoundExprKind::IsNull { negated: true, .. }
        ));
    }

    #[test]
    fn in_list_types() {
        let in_list = |items: Vec<Expr>| Expr::In {
            expr: Box::new(int("1")),
            list: InList::Exprs(items),
            negated: false,
            span: any_span(),
        };

        let plain = b(&in_list(vec![int("1"), int("2"), int("3")]));
        let BoundExprKind::In { list, .. } = &plain.kind else {
            panic!("expected an IN, got {:?}", plain.kind);
        };
        assert_eq!(list.len(), 3);
        assert!(plain.is_predicate());

        // `SELECT CASE WHEN 1 IN (1, '2') THEN 1 ELSE 0 END;` answers 1: the string is
        // converted to the common type, `int`.
        let mixed = b(&in_list(vec![int("1"), text("2")]));
        let BoundExprKind::In { list, .. } = &mixed.kind else {
            panic!("expected an IN, got {:?}", mixed.kind);
        };
        assert!(is_convert_to(&list[1], SqlType::Int), "{:?}", list[1]);

        // The tested value is converted too when the list decides the common type, and it
        // enters the precision table with its digit count like anywhere else.
        let decimals = b(&in_list(vec![dec("1.5")]));
        let BoundExprKind::In { expr, .. } = &decimals.kind else {
            panic!("expected an IN, got {:?}", decimals.kind);
        };
        assert!(
            is_convert_to(
                expr,
                SqlType::Numeric {
                    precision: 2,
                    scale: 1
                }
            ),
            "{expr:?}"
        );

        // `SELECT CASE WHEN 1 IN (1, CAST('0E98…' AS uniqueidentifier)) …` answers 206
        // naming `uniqueidentifier` then `tinyint`: the element is named first, which is
        // the order the fold passes the operands in.
        let clash = err(&in_list(vec![int("1"), var("@guid")]));
        assert_eq!(clash.number, 206);
        assert_eq!(
            clash.message,
            SqlError::operand_type_clash("uniqueidentifier", "int").message
        );
    }

    #[test]
    fn like_converts_instead_of_refusing() {
        let like = |expr: Expr, pattern: Expr| Expr::Like {
            expr: Box::new(expr),
            pattern: Box::new(pattern),
            escape: None,
            negated: false,
            span: any_span(),
        };

        let strings = b(&like(text("abc"), text("a%")));
        assert!(strings.is_predicate());
        let BoundExprKind::Like { expr, pattern, .. } = &strings.kind else {
            panic!("expected a LIKE, got {:?}", strings.kind);
        };
        // Two character operands: nothing to convert.
        assert!(matches!(expr.kind, BoundExprKind::Literal(_)));
        assert!(matches!(pattern.kind, BoundExprKind::Literal(_)));

        // `SELECT CASE WHEN 1 LIKE 2 THEN 1 ELSE 0 END;` answers 0 — no 8117: SQL Server
        // converts both operands to a string.
        let numbers = b(&like(int("1"), int("2")));
        let BoundExprKind::Like { expr, pattern, .. } = &numbers.kind else {
            panic!("expected a LIKE, got {:?}", numbers.kind);
        };
        assert!(is_convert_to(expr, SqlType::VarChar(Len::Max)), "{expr:?}");
        assert!(
            is_convert_to(pattern, SqlType::VarChar(Len::Max)),
            "{pattern:?}"
        );
    }

    #[test]
    fn collate_changes_the_collation() {
        let collate = |expr: Expr, name: &str| Expr::Collate {
            expr: Box::new(expr),
            collation: name.to_owned(),
            collation_span: any_span(),
            span: any_span(),
        };

        let bound = b(&collate(text("a"), "Latin1_General_CS_AS"));
        assert_eq!(bound.ty.ty, SqlType::VarChar(Len::Fixed(1)));
        assert_eq!(
            bound.ty.collation,
            Some(Collation::parse("Latin1_General_CS_AS").expect("a known collation"))
        );
        assert!(matches!(bound.kind, BoundExprKind::Collate { .. }));

        assert_eq!(err(&collate(text("a"), "Klingon_CI_AS")).number, 448);

        // `SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;` answers error 447 naming
        // `int`.
        let on_int = err(&collate(var("@int"), "Latin1_General_CI_AS"));
        assert_eq!(on_int.number, 447);
        assert_eq!(
            on_int.message,
            SqlError::collate_on_non_string("int").message
        );
    }

    #[test]
    fn unary_operators() {
        // `+` is transparent, on any type: `SELECT +CAST('abc' AS varchar(10));` answers
        // `abc`.
        let plus = b(&unary(UnaryOp::Plus, var("@varchar")));
        assert_eq!(plus.ty.ty, SqlType::VarChar(Len::Fixed(10)));
        assert!(matches!(plus.kind, BoundExprKind::Variable { .. }));

        // `-tinyint` is a `smallint`, `-int` an `int`.
        let negate = b(&unary(UnaryOp::Minus, var("@tiny")));
        assert_eq!(negate.ty.ty, SqlType::SmallInt);
        assert!(matches!(negate.kind, BoundExprKind::Negate(_)));
        assert_eq!(b(&unary(UnaryOp::Minus, int("1"))).ty.ty, SqlType::Int);

        // `SELECT -CAST(1 AS bit);` answers 8117 naming `bit` and the `minus` operator —
        // the word is `minus`, not `unary minus`.
        let bad = err(&unary(UnaryOp::Minus, var("@bit")));
        assert_eq!(bad.number, 8117);
        assert_eq!(
            bad.message,
            SqlError::invalid_operand_type("bit", "minus").message
        );

        // `~` keeps the type of an integer or of a `bit` (`~tinyint` is a `tinyint`,
        // `~bit` is a `bit`).
        assert_eq!(
            b(&unary(UnaryOp::BitNot, var("@tiny"))).ty.ty,
            SqlType::TinyInt
        );
        assert_eq!(b(&unary(UnaryOp::BitNot, var("@bit"))).ty.ty, SqlType::Bit);
        let bad = err(&unary(UnaryOp::BitNot, var("@float")));
        assert_eq!(bad.number, 8117);
        assert_eq!(
            bad.message,
            SqlError::invalid_operand_type("float", "'~'").message
        );
    }

    #[test]
    fn column_without_from_is_207() {
        let e = err(&column("c", 1));
        assert_eq!(e.number, 207);
        assert_eq!(e.severity, 16);
        assert_eq!(e.message, SqlError::invalid_column_name("c").message);

        // `SELECT t.c;` answers 4104 naming `"t.c"`, severity 16 state 1.
        let qualified = Expr::Column(ColumnRef {
            qualifier: Some(ObjectName {
                server: None,
                database: None,
                schema: None,
                name: Ident {
                    value: "t".to_owned(),
                    quoted: false,
                },
                span: any_span(),
            }),
            name: Ident {
                value: "c".to_owned(),
                quoted: false,
            },
            span: any_span(),
        });
        let error = err(&qualified);
        assert_eq!((error.number, error.severity, error.state), (4104, 16, 1));
        assert!(error.message.contains("\"t.c\""), "{}", error.message);
    }

    #[test]
    fn niladic_functions_are_not_columns() {
        // `CURRENT_TIMESTAMP` reaches the binder as a column reference and must not become
        // error 207: `call::bind_niladic` binds it as a call.
        assert!(matches!(
            b(&column("CURRENT_TIMESTAMP", 1)).kind,
            BoundExprKind::Function { .. }
        ));
        // A name that *looks* niladic and is not stays a column, so that the user reads
        // 207 and not a message about a function (`call::bind_niladic`).
        // `CURRENT_CATALOG` is the SQL:2003 niladic function SQL Server does not have:
        // `SELECT CURRENT_CATALOG;` answers 207. Two spellings, for the case-insensitivity
        // of the lookup.
        for name in ["current_catalog", "CURRENT_CATALOG"] {
            assert_eq!(err(&column(name, 1)).number, 207, "{name}");
        }
        // A delimited name is a column, whatever it spells.
        let quoted = Expr::Column(ColumnRef {
            qualifier: None,
            name: Ident {
                value: "USER".to_owned(),
                quoted: true,
            },
            span: any_span(),
        });
        assert_eq!(err(&quoted).number, 207);
    }

    #[test]
    fn errors_carry_the_line() {
        // "SELECT 1;\n\nSELECT c": the column is on line 3.
        let e = err(&column("c", 3));
        assert_eq!(e.number, 207);
        assert_eq!(e.line, 3);
        // An error raised by `types` gets the line of the operator node.
        let clash = binary(AstBinaryOp::Add, var("@guid"), int("1"));
        let Expr::Binary { span, .. } = &clash else {
            panic!("built as a binary node");
        };
        assert_eq!(span.line, 1);
        assert_eq!(err(&clash).line, 1);
    }

    #[test]
    fn unknown_variable_is_137() {
        let e = err(&var("@x"));
        assert_eq!(e.number, 137);
        assert_eq!(e.severity, 15);
        assert_eq!(
            e.message,
            SqlError::must_declare_scalar_variable("@x").message
        );
        // `@@x` is a function of the registry, not a variable: `call::bind_variable_function`
        // binds it, case-insensitively.
        assert!(matches!(
            b(&var("@@rowcount")).kind,
            BoundExprKind::Function { .. }
        ));
        // A `@@x` the registry does not know is 137 too, state 2, with the name as written.
        let global = err(&var("@@NO_SUCH"));
        assert_eq!((global.number, global.severity, global.state), (137, 15, 2));
        assert!(
            global.message.contains("\"@@NO_SUCH\""),
            "{}",
            global.message
        );
    }

    #[test]
    fn parentheses_are_dropped() {
        let nested = Expr::Nested(
            Box::new(binary(AstBinaryOp::Add, int("1"), int("1"))),
            any_span(),
        );
        assert!(matches!(b(&nested).kind, BoundExprKind::Arith { .. }));
    }

    #[test]
    fn near_token_reads_what_follows_the_expression() {
        let span = |offset: u32, len: u32| Span {
            line: 1,
            column: offset + 1,
            offset,
            len,
        };
        assert_eq!(near_token_at("SELECT 1 WHERE 1;", &span(15, 1)).0, ";");
        assert_eq!(
            near_token_at("SELECT 1 WHERE 1 AND 1 = 1", &span(15, 1)).0,
            "AND"
        );
        assert_eq!(near_token_at("IF 1 SELECT 1;", &span(3, 1)).0, "SELECT");
        assert_eq!(near_token_at("SELECT 1 WHERE 1", &span(15, 1)).0, "1");
        assert_eq!(near_token_at("SELECT 1 WHERE 1 + 1", &span(15, 5)).0, "1");
        // Comments between the expression and the next token are skipped.
        assert_eq!(near_token_at("... 1 /* c */ -- x\n;", &span(4, 1)).0, ";");
        // And a **nested** block comment is skipped whole: `still` is a word inside a
        // comment, not the token 4145 quotes. The batch
        // `SELECT` / `1` / `WHERE` / `1` / `/* outer /* inner` / `*/ still outer */` / `;`
        // answers `near ';'` on line 8, where the first `*/` would have given
        // `near 'still'` on line 7.
        assert_eq!(
            near_token_at("... 1 /* outer /* inner */ still outer */ ;", &span(4, 1)).0,
            ";"
        );
        // A span outside the text is the empty slice, never a panic.
        assert_eq!(near_token_at("SELECT 1", &span(99, 3)).0, "");
    }

    /// The line 4145 carries is the quoted token's; each batch here puts that token on a
    /// line the expression does not start on.
    #[test]
    fn near_token_carries_the_line_of_the_token_it_quotes() {
        // `SELECT\n1\nWHERE\n1\n+\n1;`: the expression `1\n+\n1` starts on line 4, the `;`
        // that follows it is on line 6.
        let text = "SELECT\n1\nWHERE\n1\n+\n1;";
        let at = text.rfind("1\n+").expect("the condition");
        let condition = Span {
            line: 4,
            column: 1,
            offset: at as u32,
            len: (text.len() - at - 1) as u32,
        };
        assert_eq!(near_token_at(text, &condition), (";", 6));

        // The `AND` on its own line, three lines below the expression it follows.
        let text = "SELECT 1 WHERE\n1\n\n\nAND 1 = 1;";
        let operand = Span {
            line: 2,
            column: 1,
            offset: 15,
            len: 1,
        };
        assert_eq!(near_token_at(text, &operand), ("AND", 5));

        // End of batch: the fall-back is the last token of the expression itself, and the
        // line is that token's, not the expression's.
        let text = "SELECT 1 WHERE\n1\n+\n2";
        let condition = Span {
            line: 2,
            column: 1,
            offset: 15,
            len: (text.len() - 15) as u32,
        };
        assert_eq!(near_token_at(text, &condition), ("2", 4));
    }

    #[test]
    fn an_integer_literal_counts_its_digits() {
        assert_eq!(integer_digits(&Value::I32(1)), Some(1));
        assert_eq!(integer_digits(&Value::I32(-999)), Some(3));
        assert_eq!(integer_digits(&Value::I32(0)), Some(1));
        assert_eq!(integer_digits(&Value::I64(i64::MIN)), Some(19));
        assert_eq!(integer_digits(&Value::I8(255)), Some(3));
        assert_eq!(integer_digits(&Value::Null), None);
    }

    /// `CAST(<expr> AS <name>)`, without type arguments.
    fn cast(expr: Expr, name: &str) -> Expr {
        Expr::Cast {
            expr: Box::new(expr),
            ty: vauban_parser::DataType {
                name: name.to_owned(),
                args: Vec::new(),
                span: any_span(),
            },
            try_: false,
            span: any_span(),
        }
    }

    /// `CASE WHEN @@SPID = 0 THEN <then> [ELSE <else_>] END`: a condition the server
    /// cannot fold, so that the `CASE` is typed and not simplified.
    fn case(then: Expr, else_: Option<Expr>) -> Expr {
        Expr::Case {
            operand: None,
            arms: vec![CaseArm {
                when: binary(AstBinaryOp::Eq, var("@@SPID"), int("0")),
                then,
            }],
            else_: else_.map(Box::new),
            span: any_span(),
        }
    }

    /// The shapes of `arith_is_nullable` (`fNullable` of COLMETADATA): `@int` and
    /// `@varchar` are non-nullable here, which is what `ISNULL(@i, 1)` is there.
    #[test]
    fn arithmetic_is_nullable_and_the_bitwise_and_concatenation_are_not() {
        for op in [
            AstBinaryOp::Add,
            AstBinaryOp::Sub,
            AstBinaryOp::Mul,
            AstBinaryOp::Div,
            AstBinaryOp::Mod,
        ] {
            assert!(b(&binary(op, var("@int"), int("1"))).ty.nullable, "{op:?}");
            assert!(b(&binary(op, int("1"), int("1"))).ty.nullable, "{op:?}");
        }
        for op in [AstBinaryOp::BitAnd, AstBinaryOp::BitOr, AstBinaryOp::BitXor] {
            assert!(!b(&binary(op, var("@int"), int("2"))).ty.nullable, "{op:?}");
            assert!(b(&binary(op, var("@nint"), int("2"))).ty.nullable, "{op:?}");
            // `~CAST(1 AS int) & 1`-like: an explicit conversion carries its nullability.
            assert!(
                b(&binary(op, cast(int("1"), "int"), int("2"))).ty.nullable,
                "{op:?}"
            );
        }
        // `@varchar + 'x'` → 0, `@varchar + CAST('x' AS varchar(1))` → 1, `'a' + NULL` → 1.
        assert!(
            !b(&binary(AstBinaryOp::Add, var("@varchar"), text("x")))
                .ty
                .nullable
        );
        assert!(
            b(&binary(
                AstBinaryOp::Add,
                var("@varchar"),
                cast(text("x"), "varchar")
            ))
            .ty
            .nullable
        );
        assert!(b(&binary(AstBinaryOp::Add, text("a"), null())).ty.nullable);
        // The binary concatenation stays an `Add` and follows its operands like the
        // character one: `0x00 + 0x01` → 0, `@nbin + 0x01` → 1,
        // `CAST(NULL AS varbinary(10)) + 0x01` → 1.
        let bin = b(&binary(AstBinaryOp::Add, hex("00"), hex("01")));
        assert_eq!(bin.ty.ty, SqlType::VarBinary(Len::Fixed(2)));
        assert!(!bin.ty.nullable);
        assert!(
            b(&binary(AstBinaryOp::Add, var("@nbin"), hex("01")))
                .ty
                .nullable
        );
        assert!(
            b(&binary(
                AstBinaryOp::Add,
                cast(null(), "varbinary"),
                hex("01")
            ))
            .ty
            .nullable
        );
        // A bitwise operator reads its operands once converted: `@tiny & @int` widens
        // the `tinyint` without loss, `@big & @int` narrows nothing either (both to
        // `bigint`), and the result follows the operands.
        assert!(
            !b(&binary(AstBinaryOp::BitAnd, var("@tiny"), var("@int")))
                .ty
                .nullable
        );
        assert!(
            !b(&binary(AstBinaryOp::BitAnd, var("@big"), var("@int")))
                .ty
                .nullable
        );
        // `@datetime + 1` → 1: arithmetic on a date is arithmetic.
        assert!(
            b(&binary(AstBinaryOp::Add, var("@datetime"), int("1")))
                .ty
                .nullable
        );
    }

    /// The shapes of `bind_unary` on nullability.
    #[test]
    fn negation_is_nullable_but_for_a_signed_literal() {
        let neg = |e: Expr| unary(UnaryOp::Minus, e);
        let not = |e: Expr| unary(UnaryOp::BitNot, e);
        assert!(!b(&neg(int("1"))).ty.nullable);
        assert!(!b(&neg(neg(int("1")))).ty.nullable);
        assert!(!b(&neg(dec("1.5"))).ty.nullable);
        assert!(!b(&neg(unary(UnaryOp::Plus, int("1")))).ty.nullable);
        assert!(b(&neg(var("@int"))).ty.nullable);
        assert!(b(&neg(var("@@SPID"))).ty.nullable);
        assert!(b(&neg(cast(int("1"), "int"))).ty.nullable);
        assert!(
            b(&neg(binary(AstBinaryOp::BitAnd, int("1"), int("1"))))
                .ty
                .nullable
        );
        assert!(!b(&not(int("1"))).ty.nullable);
        assert!(!b(&not(var("@int"))).ty.nullable);
        assert!(!b(&not(var("@@SPID"))).ty.nullable);
        assert!(b(&not(var("@nint"))).ty.nullable);
        assert!(b(&not(cast(int("1"), "int"))).ty.nullable);
    }

    /// The shapes of `bind_case` on nullability.
    #[test]
    fn case_is_nullable_by_its_branches_once_converted() {
        assert!(!b(&case(int("1"), Some(int("2")))).ty.nullable);
        assert!(b(&case(int("1"), None)).ty.nullable);
        assert!(b(&case(cast(int("1"), "int"), Some(int("2")))).ty.nullable);
        assert!(b(&case(int("1"), Some(cast(int("2"), "int")))).ty.nullable);
        assert!(b(&case(int("1"), Some(null()))).ty.nullable);
        assert!(b(&case(var("@nint"), Some(int("2")))).ty.nullable);
        // The condition does not count: `CASE WHEN @nint = 0 THEN 1 ELSE 2 END` → 0.
        let nullable_condition = Expr::Case {
            operand: None,
            arms: vec![CaseArm {
                when: binary(AstBinaryOp::Eq, var("@nint"), int("0")),
                then: int("1"),
            }],
            else_: Some(Box::new(int("2"))),
            span: any_span(),
        };
        assert!(!b(&nullable_condition).ty.nullable);
        // The conversion of a branch counts, read from the type the branch exposes:
        // `THEN @int ELSE @dec` fits (`int` → `decimal(12,2)`), `THEN @tiny ELSE @int`
        // too, the literal `1` facing `1.5` does not (`int` → `numeric(2,1)`) and `@int`
        // facing `1.5` does (`numeric(11,1)`).
        let case_int_dec = b(&case(var("@int"), Some(var("@dec"))));
        assert_eq!(
            case_int_dec.ty.ty,
            SqlType::Decimal {
                precision: 12,
                scale: 2
            }
        );
        assert!(!case_int_dec.ty.nullable);
        assert!(!b(&case(var("@tiny"), Some(var("@int")))).ty.nullable);
        assert!(b(&case(int("1"), Some(dec("1.5")))).ty.nullable);
        assert!(!b(&case(var("@int"), Some(dec("1.5")))).ty.nullable);
    }

    /// The table of `implicit_conversion_may_be_null`, one row per rule and its
    /// counter-example.
    #[test]
    fn implicit_conversions_that_may_lose_a_value_are_nullable() {
        use SqlType::{
            BigInt, Binary, Bit, Char, Date, DateTime, DateTime2, DateTimeOffset, Decimal, Float,
            Int, Money, NChar, NVarChar, Numeric, Real, SmallDateTime, SmallInt, SmallMoney, Time,
            TinyInt, VarBinary, VarChar,
        };
        let dec = |precision: u8, scale: u8| Decimal { precision, scale };
        let num = |precision: u8, scale: u8| Numeric { precision, scale };
        let lossless: &[(SqlType, SqlType)] = &[
            (Int, Int),
            (TinyInt, SmallInt),
            (TinyInt, Int),
            (TinyInt, BigInt),
            (SmallInt, Int),
            (Int, BigInt),
            (Bit, TinyInt),
            (Bit, Int),
            (Int, dec(10, 0)),
            (Int, dec(12, 2)),
            (BigInt, dec(19, 0)),
            (SmallInt, dec(5, 0)),
            (TinyInt, dec(3, 0)),
            (Bit, dec(1, 0)),
            (Int, Float),
            (SmallInt, Real),
            (Bit, Real),
            (Int, Money),
            (SmallInt, SmallMoney),
            (Bit, Money),
            (SmallMoney, Money),
            (Money, dec(19, 4)),
            (SmallMoney, dec(10, 4)),
            (dec(5, 2), Money),
            (dec(5, 2), dec(6, 3)),
            (dec(5, 2), dec(6, 2)),
            (dec(5, 2), num(6, 2)),
            (num(5, 2), dec(6, 2)),
            (Real, Float),
            (Char(Len::Fixed(5)), Char(Len::Fixed(10))),
            (Char(Len::Fixed(10)), VarChar(Len::Fixed(10))),
            (VarChar(Len::Fixed(5)), Char(Len::Fixed(10))),
            (VarChar(Len::Fixed(10)), NChar(Len::Fixed(10))),
            (NChar(Len::Fixed(10)), NVarChar(Len::Fixed(10))),
            (NVarChar(Len::Fixed(10)), VarChar(Len::Fixed(20))),
            (NVarChar(Len::Fixed(10)), NVarChar(Len::Max)),
            (VarChar(Len::Max), NVarChar(Len::Max)),
            (NVarChar(Len::Max), VarChar(Len::Max)),
            (VarBinary(Len::Fixed(10)), VarBinary(Len::Fixed(20))),
            (VarBinary(Len::Fixed(5)), Binary(Len::Fixed(10))),
            (VarBinary(Len::Fixed(10)), VarBinary(Len::Max)),
            (Date, DateTime2(7)),
            (Date, DateTimeOffset(7)),
            (Time(7), DateTime2(7)),
            (SmallDateTime, DateTime),
            (SmallDateTime, DateTime2(7)),
            (DateTime2(3), DateTime2(7)),
            (DateTime2(7), DateTimeOffset(7)),
            (Time(3), Time(7)),
            (DateTimeOffset(3), DateTimeOffset(7)),
        ];
        for (from, to) in lossless {
            assert!(
                !implicit_conversion_may_be_null(from, to),
                "{from:?} → {to:?} is lossless"
            );
        }
        let nullable: &[(SqlType, SqlType)] = &[
            (BigInt, Int),
            (Int, TinyInt),
            (TinyInt, Bit),
            (Int, dec(9, 0)),
            (Int, dec(10, 2)),
            (BigInt, dec(18, 0)),
            (SmallInt, dec(4, 0)),
            (TinyInt, dec(2, 0)),
            (BigInt, Float),
            (Int, Real),
            (BigInt, Money),
            (Int, SmallMoney),
            (Money, dec(18, 4)),
            (Money, BigInt),
            (SmallMoney, Int),
            (Money, Float),
            (dec(5, 2), dec(5, 3)),
            (dec(5, 2), dec(5, 1)),
            (dec(12, 2), dec(10, 2)),
            (Float, Real),
            (Float, dec(10, 2)),
            (dec(10, 2), Float),
            (Char(Len::Fixed(10)), Char(Len::Fixed(5))),
            (NVarChar(Len::Fixed(10)), VarChar(Len::Fixed(10))),
            (VarChar(Len::Fixed(10)), NVarChar(Len::Fixed(5))),
            (VarChar(Len::Max), VarChar(Len::Fixed(10))),
            (Binary(Len::Fixed(10)), Binary(Len::Fixed(5))),
            (Date, DateTime),
            (DateTime, SmallDateTime),
            (DateTime, DateTime2(7)),
            (DateTime, DateTimeOffset(7)),
            (DateTime2(7), DateTime2(3)),
            (Time(7), Time(3)),
            (DateTime2(7), DateTime),
            (Int, VarChar(Len::Fixed(30))),
            (Bit, VarChar(Len::Fixed(10))),
            (DateTime, VarChar(Len::Fixed(30))),
            (VarChar(Len::Fixed(10)), Int),
        ];
        for (from, to) in nullable {
            assert!(
                implicit_conversion_may_be_null(from, to),
                "{from:?} → {to:?} may lose a value"
            );
        }
    }

    /// `CAST(<expr> AS <name>(<args>))`, whose arguments are numbers.
    fn cast_args(expr: Expr, name: &str, args: &[i64]) -> Expr {
        Expr::Cast {
            expr: Box::new(expr),
            ty: vauban_parser::DataType {
                name: name.to_owned(),
                args: args
                    .iter()
                    .map(|a| vauban_parser::TypeArg::Number(*a))
                    .collect(),
                span: any_span(),
            },
            try_: false,
            span: any_span(),
        }
    }

    /// Beside an operand, the bare `NULL` takes that operand's type instead of the `int`
    /// `bind_literal` gives it alone. Each line is a row of the table of
    /// [`super::untyped_null_beside`].
    #[test]
    fn the_bare_null_takes_the_type_of_its_sibling() {
        let vectors: Vec<(Expr, SqlType)> = vec![
            // `SELECT 'a' + NULL;` is a concatenation, not an integer addition that would
            // answer 245.
            (text("a"), SqlType::VarChar(Len::Fixed(2))),
            (int("1"), SqlType::Int),
            (
                dec("1.5"),
                SqlType::Numeric {
                    precision: 3,
                    scale: 1,
                },
            ),
            (hex("01"), SqlType::VarBinary(Len::Fixed(2))),
            (cast(int("1"), "tinyint"), SqlType::TinyInt),
            (cast(int("1"), "bigint"), SqlType::BigInt),
            (cast(int("1"), "float"), SqlType::Float),
            (cast(int("1"), "money"), SqlType::Money),
            (
                cast_args(text("abc"), "varchar", &[3]),
                SqlType::VarChar(Len::Fixed(4)),
            ),
            (
                cast_args(text("abc"), "char", &[3]),
                SqlType::VarChar(Len::Fixed(4)),
            ),
            (
                cast_args(text("abc"), "nchar", &[3]),
                SqlType::NVarChar(Len::Fixed(4)),
            ),
            (
                cast_args(hex("01"), "binary", &[8]),
                SqlType::VarBinary(Len::Fixed(9)),
            ),
            (
                cast_args(dec("0.5"), "decimal", &[2, 2]),
                SqlType::Decimal {
                    precision: 3,
                    scale: 2,
                },
            ),
            (
                cast_args(dec("1.5"), "decimal", &[5, 2]),
                SqlType::Decimal {
                    precision: 6,
                    scale: 2,
                },
            ),
        ];
        for (sibling, expected) in vectors {
            let left = b(&binary(AstBinaryOp::Add, sibling.clone(), null()));
            assert_eq!(left.ty.ty, expected, "{sibling:?} + NULL");
            let right = b(&binary(AstBinaryOp::Add, null(), sibling.clone()));
            assert_eq!(right.ty.ty, expected, "NULL + {sibling:?}");
            assert!(left.ty.nullable, "{sibling:?} + NULL is nullable");
        }
    }

    /// The counter-proof of the rule above: a bare `NULL` on **both** sides keeps the `int`
    /// of `SELECT NULL;`, and so do the operators that have no sibling to read.
    #[test]
    fn two_bare_nulls_stay_an_int() {
        for op in [
            AstBinaryOp::Add,
            AstBinaryOp::Sub,
            AstBinaryOp::Mul,
            AstBinaryOp::Div,
            AstBinaryOp::Mod,
            AstBinaryOp::BitAnd,
            AstBinaryOp::BitOr,
            AstBinaryOp::BitXor,
        ] {
            assert_eq!(b(&binary(op, null(), null())).ty.ty, SqlType::Int, "{op:?}");
        }
        assert_eq!(b(&unary(UnaryOp::Minus, null())).ty.ty, SqlType::Int);
        assert_eq!(b(&unary(UnaryOp::BitNot, null())).ty.ty, SqlType::Int);
    }

    /// `CAST(1 AS tinyint) + NULL` is a `tinyint` and `CAST(1.5 AS decimal(2,1)) * NULL` a
    /// `decimal(5,2)`: neither is what typing the bare `NULL` `int` would give, an `int`
    /// and a `decimal(13,1)`.
    #[test]
    fn the_bare_null_is_not_an_int_beside_a_narrower_sibling() {
        let tiny = b(&binary(AstBinaryOp::Add, cast(int("1"), "tinyint"), null()));
        assert_eq!(tiny.ty.ty, SqlType::TinyInt);
        let product = b(&binary(
            AstBinaryOp::Mul,
            cast_args(dec("1.5"), "decimal", &[2, 1]),
            null(),
        ));
        assert_eq!(
            product.ty.ty,
            SqlType::Decimal {
                precision: 5,
                scale: 2
            }
        );
    }

    /// A pair the operator refuses names the written operand `NULL`, and answers 402 where
    /// the same pair of typed operands answers 8117
    /// ([`super::refusal_names_the_bare_null`]).
    #[test]
    fn a_refused_bare_null_is_named_null_in_402() {
        let date = || cast(text("2020-01-01"), "date");
        let typed = err(&binary(AstBinaryOp::Add, date(), date()));
        assert_eq!(typed.number, 8117);
        assert_eq!(
            typed.message,
            SqlError::invalid_operand_type("date", "add").message
        );
        let right_null = err(&binary(AstBinaryOp::Add, date(), null()));
        assert_eq!(right_null.number, 402);
        assert_eq!(
            right_null.message,
            SqlError::incompatible_types_for_operator("date", "NULL", "add").message
        );
        let left_null = err(&binary(AstBinaryOp::Add, null(), date()));
        assert_eq!(left_null.number, 402);
        assert_eq!(
            left_null.message,
            SqlError::incompatible_types_for_operator("NULL", "date", "add").message
        );
        let bit = || cast(int("1"), "bit");
        assert_eq!(
            err(&binary(AstBinaryOp::Add, bit(), null())).message,
            SqlError::incompatible_types_for_operator("bit", "NULL", "add").message
        );
        assert_eq!(
            err(&binary(AstBinaryOp::Mul, text("a"), null())).message,
            SqlError::incompatible_types_for_operator("varchar", "NULL", "multiply").message
        );
    }

    /// The eight words of the table of [`super::operator_in_402`], each asserted on the
    /// message: `SELECT CAST('2020-01-01' AS date) <op> NULL;` answers 402 on SQL Server
    /// for the eight operators, and names the operand `NULL`.
    ///
    /// Counter-proof of the same table: typing the bare `NULL` `int` would answer 206
    /// naming `date` and `int`, which no assertion below accepts.
    #[test]
    fn every_operator_word_of_402_is_the_one_sql_server_prints() {
        let date = || cast(text("2020-01-01"), "date");
        let words = [
            (AstBinaryOp::Add, "add"),
            (AstBinaryOp::Sub, "subtract"),
            (AstBinaryOp::Mul, "multiply"),
            (AstBinaryOp::Div, "divide"),
            (AstBinaryOp::Mod, "modulo"),
            (AstBinaryOp::BitAnd, "'&'"),
            (AstBinaryOp::BitOr, "'|'"),
            (AstBinaryOp::BitXor, "'^'"),
        ];
        for (op, word) in words {
            let right = err(&binary(op, date(), null()));
            assert_eq!(right.number, 402, "{op:?}");
            assert_eq!(
                right.message,
                SqlError::incompatible_types_for_operator("date", "NULL", word).message
            );
            let left = err(&binary(op, null(), date()));
            assert_eq!(
                left.message,
                SqlError::incompatible_types_for_operator("NULL", "date", word).message
            );
        }
    }
}
