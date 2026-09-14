//! Queries: `SELECT`, its clauses, set operators and table references.

use crate::ast::expr::{ColumnRef, Expr, Ident, ObjectName};
use crate::span::Span;

/// A complete `SELECT` statement, with the clauses that wrap its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    /// (V2) The `WITH` clause of common table expressions.
    pub with: Option<With>,
    /// The query itself, one specification or a tree of set operators.
    pub body: QueryBody,
    /// `ORDER BY` items, empty when the clause is absent.
    pub order_by: Vec<OrderItem>,
    /// (V3) `OFFSET … FETCH …`, which T-SQL only allows after an `ORDER BY`.
    pub offset_fetch: Option<OffsetFetch>,
    /// (V4) The `FOR XML`/`FOR JSON` clause.
    pub for_clause: Option<ForClause>,
    /// Position of the whole statement.
    pub span: Span,
}

/// The body of a query: one specification, or two joined by a set operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryBody {
    /// A single `SELECT … FROM …`.
    Select(Box<QuerySpec>),
    /// `left UNION [ALL] right`, `EXCEPT`, `INTERSECT`.
    SetOp {
        /// The operator.
        op: SetOp,
        /// True when `ALL` was written.
        all: bool,
        /// The left operand.
        left: Box<QueryBody>,
        /// The right operand.
        right: Box<QueryBody>,
        /// Position of the whole operation.
        span: Span,
    },
    /// Parentheses the user wrote around a query body, kept for `Display`.
    Nested(Box<QueryBody>, Span),
}

/// A set operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`
    Union,
    /// `EXCEPT`
    Except,
    /// `INTERSECT`
    Intersect,
}

/// One `SELECT` specification: the clauses from `SELECT` to `HAVING`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySpec {
    /// True for `SELECT DISTINCT`. `SELECT ALL` is the default and is not kept.
    pub distinct: bool,
    /// The `TOP` clause.
    pub top: Option<Top>,
    /// The select list, never empty.
    pub items: Vec<SelectItem>,
    /// The `INTO t` target.
    pub into: Option<ObjectName>,
    /// The `FROM` clause; empty for a `SELECT` without `FROM`.
    pub from: Vec<TableRef>,
    /// The `WHERE` predicate. Named with a trailing `_`: `where` is a Rust keyword.
    pub where_: Option<Expr>,
    /// `GROUP BY` expressions, empty when the clause is absent.
    pub group_by: Vec<Expr>,
    /// The `HAVING` predicate.
    pub having: Option<Expr>,
    /// Position of the whole specification.
    pub span: Span,
}

/// One item of a select list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    /// `*`
    Wildcard(Span),
    /// `t.*`, `dbo.t.*`.
    QualifiedWildcard(ObjectName),
    /// An expression, with the alias the user wrote, if any.
    Expr {
        /// The expression.
        expr: Expr,
        /// The alias.
        alias: Option<Ident>,
        /// How the alias was written, so that `Display` restores that form.
        alias_style: AliasStyle,
    },
}

/// The three ways of writing an alias in a select list.
///
/// `SELECT c AS a`, `SELECT c a` and `SELECT a = c` mean the same thing and must
/// re-serialise to the form they were written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasStyle {
    /// `expr AS alias`.
    As,
    /// `expr alias`, without `AS`.
    Bare,
    /// `alias = expr`.
    Equals,
}

/// A `TOP` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Top {
    /// The number of rows or the percentage.
    pub expr: Expr,
    /// True for `TOP (n) PERCENT`.
    pub percent: bool,
    /// True for `WITH TIES`.
    pub with_ties: bool,
    /// True when the user parenthesised the value, which `Display` restores.
    pub parenthesized: bool,
    /// Position of the whole clause.
    pub span: Span,
}

/// One item of an `ORDER BY` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderItem {
    /// The sort expression.
    pub expr: Expr,
    /// True for `DESC`.
    pub desc: bool,
    /// True when `ASC` or `DESC` was written, so that `Display` restores it.
    pub explicit_direction: bool,
    /// The `COLLATE` name, when written.
    pub collate: Option<String>,
}

/// (V3) `OFFSET n ROWS FETCH NEXT m ROWS ONLY`.
///
/// The two flags exist so that `OFFSET 1 ROW FETCH FIRST 1 ROW ONLY` re-serialises
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetFetch {
    /// The number of rows to skip.
    pub offset: Expr,
    /// The number of rows to return, absent when only `OFFSET` was written.
    pub fetch: Option<Expr>,
    /// True when the user wrote `FETCH FIRST` instead of `FETCH NEXT`.
    pub fetch_first: bool,
    /// True when the user wrote `ROW` instead of `ROWS`.
    pub rows_singular: bool,
}

/// A table reference in a `FROM` clause.
///
/// # Telling a hint from an argument
///
/// A name given parentheses is **always** a [`TableRef::Function`], and hints only ever
/// reach [`TableRef::Table`]. The two cases are therefore told apart by the variant alone,
/// with nothing to re-derive:
///
/// | written | parsed as |
/// |---|---|
/// | `FROM t` | `Table { hints: [] }` |
/// | `FROM t WITH (NOLOCK)` | `Table { hints: [NOLOCK] }` |
/// | `FROM t AS z (NOLOCK)` | `Table { hints: [NOLOCK] }` |
/// | `FROM t (NOLOCK)` | `Function { args: [NOLOCK] }` |
/// | `FROM f(1)` | `Function { args: [1] }` |
///
/// This is what SQL Server does, whose parser reads an expression list and leaves the
/// decision to name resolution; see the header of `parser::from` for what SQL Server answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableRef {
    /// A named table, view or synonym.
    Table {
        /// The object name.
        name: ObjectName,
        /// The alias, with or without `AS`.
        alias: Option<Ident>,
        /// Hints, read and kept, never interpreted. They come from a hint list position:
        /// `WITH (…)` after the alias, or the deprecated `(…)` after the alias. Never
        /// from parentheses glued to the name -- those are the `args` of a
        /// [`TableRef::Function`].
        hints: Vec<TableHint>,
        /// Position of the whole reference.
        span: Span,
    },
    /// A derived table `(SELECT …) AS d (c1, c2)`.
    Derived {
        /// The subquery.
        query: Box<SelectStatement>,
        /// The alias, which T-SQL requires here.
        alias: Option<Ident>,
        /// The optional column list of the alias.
        columns: Vec<Ident>,
        /// Position of the whole reference.
        span: Span,
    },
    /// A join between two references.
    Join {
        /// Left side.
        left: Box<TableRef>,
        /// Right side.
        right: Box<TableRef>,
        /// The kind of join.
        kind: JoinKind,
        /// The `ON` predicate, absent for a `CROSS JOIN`.
        on: Option<Expr>,
        /// Position of the whole join.
        span: Span,
    },
    /// (V2) A table variable, `FROM @t`.
    Variable {
        /// The variable name, `@` included.
        name: String,
        /// The alias.
        alias: Option<Ident>,
        /// Position of the whole reference.
        span: Span,
    },
    /// (V2) `CROSS APPLY` / `OUTER APPLY`.
    Apply {
        /// Left side.
        left: Box<TableRef>,
        /// Right side, which may depend on the left one.
        right: Box<TableRef>,
        /// `CROSS` or `OUTER`.
        kind: ApplyKind,
        /// Position of the whole operation.
        span: Span,
    },
    /// A name given parentheses: `f(1)`, but also `t (NOLOCK)` and `t ()`.
    ///
    /// Not a promise that the name is a function -- the parser does not know and does not
    /// guess. The binder resolves it and decides: a table-valued function is
    /// called; anything else means the arguments were meant as a hint, so a lone bare
    /// identifier naming one is re-read as such, and everything else is error 215
    /// *"Parameters supplied for object '…' which is not a function. If the parameters
    /// are intended as a table hint, a WITH keyword is required."*
    ///
    /// No hint clause ever follows one, SQL Server answering 102 to
    /// `FROM dbo.f(1) WITH (NOLOCK)`; hence no `hints` field here.
    Function {
        /// The name given the parentheses.
        name: ObjectName,
        /// The arguments, empty for `t ()`, which SQL Server still calls parameters.
        args: Vec<Expr>,
        /// The alias.
        alias: Option<Ident>,
        /// Position of the whole reference.
        span: Span,
    },
    /// (V2) `PIVOT (…)`, boxed to keep this enumeration small.
    Pivot(Box<PivotSource>),
    /// (V2) `UNPIVOT (…)`, boxed likewise.
    Unpivot(Box<UnpivotSource>),
}

/// (V2) The operands of a `PIVOT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PivotSource {
    /// The pivoted reference.
    pub source: TableRef,
    /// The aggregate applied to each group.
    pub aggregate: Expr,
    /// The column whose values become column names.
    pub value_column: ColumnRef,
    /// The values turned into columns.
    pub columns: Vec<Ident>,
    /// The alias of the result.
    pub alias: Option<Ident>,
    /// Position of the whole clause.
    pub span: Span,
}

/// (V2) The operands of an `UNPIVOT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpivotSource {
    /// The unpivoted reference.
    pub source: TableRef,
    /// The column receiving the values.
    pub value_column: Ident,
    /// The column receiving the former column names.
    pub name_column: Ident,
    /// The columns turned into rows.
    pub columns: Vec<Ident>,
    /// The alias of the result.
    pub alias: Option<Ident>,
    /// Position of the whole clause.
    pub span: Span,
}

/// The kind of a join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `INNER JOIN`
    Inner,
    /// `LEFT [OUTER] JOIN`
    Left,
    /// `RIGHT [OUTER] JOIN`
    Right,
    /// `FULL [OUTER] JOIN`
    Full,
    /// `CROSS JOIN`
    Cross,
}

/// The kind of an `APPLY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyKind {
    /// `CROSS APPLY`
    Cross,
    /// `OUTER APPLY`
    Outer,
}

/// One hint of a table hint list: accepted, kept, never interpreted.
///
/// Only a hint list position produces one: `WITH (…)`, or the deprecated `(…)` written
/// after the alias. Parentheses glued to a name are arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableHint {
    /// The hint name, as written (`NOLOCK`, `INDEX`, …).
    pub name: String,
    /// The parenthesised arguments, as written.
    pub args: Vec<String>,
    /// Position of the hint.
    pub span: Span,
}

/// (V2) The `WITH` clause introducing common table expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct With {
    /// T-SQL never writes `WITH RECURSIVE`; the flag exists for the binder, which decides
    /// whether a recursive reference is allowed.
    pub recursive_allowed: bool,
    /// The expressions, in the order they were written.
    pub ctes: Vec<CommonTableExpr>,
    /// Position of the whole clause.
    pub span: Span,
}

/// (V2) One common table expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonTableExpr {
    /// The name it is referred to by.
    pub name: Ident,
    /// The optional column list.
    pub columns: Vec<Ident>,
    /// The query.
    pub query: Box<SelectStatement>,
    /// Position of the whole expression.
    pub span: Span,
}

/// (V4) The `FOR` clause of a `SELECT`. Declared with minimal payloads only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForClause {
    /// `FOR JSON AUTO|PATH`.
    Json {
        /// True for the `RAW`-like mode, false for `PATH`.
        raw: bool,
        /// The `ROOT('…')` path, when written.
        path: Option<String>,
    },
    /// `FOR XML RAW|AUTO|PATH`.
    Xml {
        /// True for `RAW`.
        raw: bool,
        /// The `PATH('…')` argument, when written.
        path: Option<String>,
    },
    /// `FOR BROWSE`.
    Browse,
}
