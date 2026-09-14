//! Names, literals, data types and expressions.

use crate::ast::query::{OrderItem, SelectStatement};
use crate::span::Span;

/// One part of a name, as the user wrote it.
///
/// The case is **kept as typed**: comparing names without regard to case is the binder's
/// job, never the parser's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    /// The text, **without** delimiters, with `]]` and `""` already unescaped.
    pub value: String,
    /// True when the user wrote `[x]` or `"x"`.
    pub quoted: bool,
}

/// A possibly qualified object name, from `t` to `srv.db.dbo.t`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectName {
    /// Linked server part, the leftmost one.
    pub server: Option<Ident>,
    /// Database part.
    pub database: Option<Ident>,
    /// Schema part.
    pub schema: Option<Ident>,
    /// Object part, the only mandatory one.
    pub name: Ident,
    /// Position of the whole name.
    pub span: Span,
}

/// A reference to a column, with the parts written to its left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    /// The parts left of the column name: `db.dbo.t.c` has qualifier `db.dbo.t`.
    pub qualifier: Option<ObjectName>,
    /// The column name itself.
    pub name: Ident,
    /// Position of the whole reference.
    pub span: Span,
}

/// A literal value, kept in the exact shape `types::parse_literal(kind, text)` expects.
///
/// The parser neither classifies nor evaluates: turning `Integer("3000000000")` into an
/// `int` or a `bigint`, and into a value, belongs to `types`; the mapping from
/// these variants to `types::LiteralKind` belongs to the binder. `Display`
/// restores what was removed here (the `$`, the `0x`, the doubled `'`, the `N`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// `NULL`.
    Null,
    /// `DEFAULT`, in an `INSERT ... VALUES` row or an `UPDATE ... SET c = DEFAULT`.
    Default,
    /// An integer, as **source text** (`007` stays `007`).
    Integer(String),
    /// A fixed-point number, as **source text** (`1.50` stays `1.50`).
    Decimal(String),
    /// A number with an exponent, as **source text** (`1E3` stays `1E3`).
    Float(String),
    /// A money literal, as source text **without the `$`**: `$1.50` → `1.50`,
    /// `$-1.50` → `-1.50`.
    Money(String),
    /// A character string.
    Str {
        /// The value, **already unescaped** (`''` → `'`), without delimiters nor `N`.
        value: String,
        /// True when the literal was written `N'…'`.
        unicode: bool,
    },
    /// A binary literal, as source text **without the `0x` prefix**.
    Binary(String),
}

/// A binary operator.
///
/// `+` always parses to [`BinaryOp::Add`], even between strings: deciding between
/// addition and concatenation needs types, hence the binder. [`BinaryOp::Concat`] is
/// declared for that later use and is never produced by the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// String concatenation, decided by the binder, never produced by the parser.
    Concat,
    /// `=`
    Eq,
    /// `<>` and `!=`, which the parser does not distinguish.
    Ne,
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
    /// `AND`
    And,
    /// `OR`
    Or,
}

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `NOT`
    Not,
    /// `~`
    BitNot,
}

/// One `WHEN … THEN …` arm of a `CASE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseArm {
    /// The `WHEN` part: a value for a simple `CASE`, a predicate for a searched one.
    pub when: Expr,
    /// The `THEN` part.
    pub then: Expr,
}

/// The right-hand side of an `IN` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InList {
    /// `IN (1, 2, 3)`.
    Exprs(Vec<Expr>),
    /// `IN (SELECT …)`.
    Subquery(Box<SelectStatement>),
}

/// The quantifier of a comparison against a subquery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantifier {
    /// `ALL`
    All,
    /// `ANY`
    Any,
    /// `SOME`, a synonym of `ANY` that `Display` must keep as written.
    Some,
}

/// A data type as written, name plus arguments.
///
/// The parser does not normalise and does not resolve: turning a `DataType` into the
/// engine type it denotes is the binder's job, with the `types` crate. Multi-word names
/// (`double precision`, `character varying`) are kept as one name with single spaces; the
/// case is kept as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataType {
    /// The type name, as written.
    pub name: String,
    /// The parenthesised arguments, in order.
    pub args: Vec<TypeArg>,
    /// Position of the whole type.
    pub span: Span,
}

/// One argument of a data type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeArg {
    /// A number: the `10` and the `2` of `decimal(10, 2)`.
    Number(i64),
    /// The `MAX` of `varchar(max)`.
    Max,
    /// A word, such as the collation-like arguments some types accept.
    Ident(String),
}

/// (V2) The `OVER` clause of a window function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Over {
    /// `PARTITION BY` expressions, empty when the clause is absent.
    pub partition_by: Vec<Expr>,
    /// `ORDER BY` items, empty when the clause is absent.
    pub order_by: Vec<OrderItem>,
    /// `ROWS`/`RANGE` frame, when written.
    pub frame: Option<WindowFrame>,
    /// Position of the `OVER (…)` clause.
    pub span: Span,
}

/// (V2) The frame of a window: `ROWS BETWEEN … AND …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFrame {
    /// `ROWS` or `RANGE`.
    pub units: FrameUnits,
    /// The lower bound, the only mandatory one.
    pub start: FrameBound,
    /// The upper bound, present only in the `BETWEEN … AND …` form.
    pub end: Option<FrameBound>,
}

/// (V2) The unit a window frame counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameUnits {
    /// `ROWS`
    Rows,
    /// `RANGE`
    Range,
}

/// (V2) One bound of a window frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING`
    UnboundedPreceding,
    /// `<n> PRECEDING`
    Preceding(Box<Expr>),
    /// `CURRENT ROW`
    CurrentRow,
    /// `<n> FOLLOWING`
    Following(Box<Expr>),
    /// `UNBOUNDED FOLLOWING`
    UnboundedFollowing,
}

/// An expression.
///
/// Parentheses the user wrote are kept as [`Expr::Nested`] so that `Display` re-serialises
/// them without adding or removing any: the `parse` → `Display` → `parse` loop depends on
/// it. Recursive fields are boxed, which keeps the variants small enough for clippy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A literal value.
    Literal(Literal, Span),
    /// A column reference.
    Column(ColumnRef),
    /// A local variable `@x` or a global one `@@x`.
    Variable {
        /// The name, `@` or `@@` **included**, so that `Display` is exact.
        name: String,
        /// Position of the variable.
        span: Span,
    },
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Position of the **operator token**, which errors 402 and 8117 are reported on
        /// (`tests/operator_span.rs`).
        /// It is not deducible from the three other spans: a block comment between the two
        /// operands puts the operator on the line of neither of them.
        op_span: Span,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
        /// Position of the whole operation.
        span: Span,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
        /// Position of the whole operation.
        span: Span,
    },
    /// Parentheses written by the user, kept as such.
    Nested(Box<Expr>, Span),
    /// A function call, from `abs(@x)` to `count(distinct c) over (…)`.
    Function {
        /// The function name, possibly qualified.
        name: ObjectName,
        /// The arguments, empty for `count(*)`.
        args: Vec<Expr>,
        /// True for the `*` of `count(*)`.
        star: bool,
        /// True when the argument list starts with `DISTINCT`.
        distinct: bool,
        /// (V2) The `OVER` clause, boxed to keep [`Expr`] small.
        over: Option<Box<Over>>,
        /// Position of the whole call.
        span: Span,
    },
    /// A `CASE`, simple when `operand` is set, searched otherwise.
    Case {
        /// The value compared by a simple `CASE`.
        operand: Option<Box<Expr>>,
        /// The `WHEN … THEN …` arms, at least one.
        arms: Vec<CaseArm>,
        /// The `ELSE` result, when written.
        else_: Option<Box<Expr>>,
        /// Position of the whole `CASE`.
        span: Span,
    },
    /// `CAST(e AS t)` or `TRY_CAST(e AS t)`.
    Cast {
        /// The value to convert.
        expr: Box<Expr>,
        /// The target type.
        ty: DataType,
        /// True for `TRY_CAST`.
        try_: bool,
        /// Position of the whole call.
        span: Span,
    },
    /// `CONVERT(t, e [, style])` or `TRY_CONVERT(…)`.
    Convert {
        /// The target type, written first in T-SQL.
        ty: DataType,
        /// The value to convert.
        expr: Box<Expr>,
        /// The optional style argument.
        style: Option<Box<Expr>>,
        /// True for `TRY_CONVERT`.
        try_: bool,
        /// Position of the whole call.
        span: Span,
    },
    /// `e IS NULL` or `e IS NOT NULL`.
    IsNull {
        /// The tested value.
        expr: Box<Expr>,
        /// True for `IS NOT NULL`.
        negated: bool,
        /// Position of the whole predicate.
        span: Span,
    },
    /// `e IN (…)` or `e NOT IN (…)`.
    In {
        /// The tested value.
        expr: Box<Expr>,
        /// The list or the subquery.
        list: InList,
        /// True for `NOT IN`.
        negated: bool,
        /// Position of the whole predicate.
        span: Span,
    },
    /// `e LIKE p [ESCAPE c]`.
    Like {
        /// The tested value.
        expr: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// The `ESCAPE` character, when written.
        escape: Option<Box<Expr>>,
        /// True for `NOT LIKE`.
        negated: bool,
        /// Position of the whole predicate.
        span: Span,
    },
    /// `e BETWEEN low AND high`.
    Between {
        /// The tested value.
        expr: Box<Expr>,
        /// Lower bound.
        low: Box<Expr>,
        /// Upper bound.
        high: Box<Expr>,
        /// True for `NOT BETWEEN`.
        negated: bool,
        /// Position of the whole predicate.
        span: Span,
    },
    /// `EXISTS (SELECT …)`.
    Exists(Box<SelectStatement>, Span),
    /// A scalar subquery `(SELECT …)`.
    Subquery(Box<SelectStatement>, Span),
    /// `e > ALL (SELECT …)`, `e = ANY (SELECT …)`.
    Quantified {
        /// The left operand.
        expr: Box<Expr>,
        /// The comparison operator.
        op: BinaryOp,
        /// `ALL`, `ANY` or `SOME`.
        quantifier: Quantifier,
        /// The subquery compared against.
        subquery: Box<SelectStatement>,
        /// Position of the whole predicate.
        span: Span,
    },
    /// `e COLLATE Latin1_General_CI_AS`.
    Collate {
        /// The value.
        expr: Box<Expr>,
        /// The collation name, as written.
        collation: String,
        /// Position of the **collation name token**, which errors 447 and 448 are reported
        /// on (`tests/operator_span.rs`).
        collation_span: Span,
        /// Position of the whole expression.
        span: Span,
    },
    /// `SELECT @x = e`: an assignment used as a select item.
    Assign {
        /// The target variable, `@` included.
        target: String,
        /// The assigned expression.
        value: Box<Expr>,
        /// Position of the whole assignment.
        span: Span,
    },
    /// (V3) `NEXT VALUE FOR seq [OVER (…)]`.
    NextValueFor {
        /// The sequence name.
        sequence: ObjectName,
        /// The `OVER` clause, allowed here by T-SQL.
        over: Option<Box<Over>>,
        /// Position of the whole expression.
        span: Span,
    },
    /// An invalid niladic spelling whose diagnostic is deferred until binding.
    ///
    /// Deferring it is what orders two errors of one select list **on the comma form**: an
    /// undeclared variable written before the comma keeps its own 137, the spelling written
    /// first answers its own number. On the four spellings:
    /// `SELECT @v, CURRENT_TIMESTAMP();` answers 137 against 102 for
    /// `SELECT CURRENT_TIMESTAMP(), @v;`, and likewise for `CURRENT_DATE` (137 against
    /// 156), `[CURRENT_TIMESTAMP]()` and `[CURRENT_DATE]()` (137 against 102).
    ///
    /// The order is stated on that comma form only, and it is not the order the pipeline
    /// produces in every shape where a variable is written first: the assignment
    /// `SELECT @x = CURRENT_TIMESTAMP();` answers 102 near `)` here, because
    /// `vauban-binder`'s `query.rs` binds the value of an assignment before checking that
    /// its target is declared, and `SELECT TOP (@missing) CURRENT_TIMESTAMP();` answers 102
    /// near `)` too, though its variable is written first as well. Neither shape is
    /// covered by a test, so no order is stated for them.
    InvalidNiladic {
        /// Token spelling retained for structural display roundtrips.
        spelling: String,
        /// The offending token as written.
        diagnostic_token: String,
        /// The number this token answers with, 102 or 156.
        diagnostic_number: u16,
        /// Location of the offending token.
        diagnostic_span: Span,
        /// Opening call parenthesis, when present, for an enclosing expression.
        opening_span: Option<Span>,
        /// Location of the entire invalid expression.
        span: Span,
    },
    /// (V2) The `?` parameter placeholder of ODBC. Never produced in V1.
    Placeholder(Span),
}
