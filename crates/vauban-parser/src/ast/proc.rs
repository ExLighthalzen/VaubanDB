//! Programmability: procedures, functions, views, triggers, sequences and `MERGE`.
//!
//! Everything in this file is (V2) or (V3): no V1 grammar rule builds these nodes. They
//! are declared now so that `binder`, `planner` and `executor` can be written against a
//! stable type.

use crate::ast::ddl::TableDefinition;
use crate::ast::expr::{DataType, Expr, Ident, ObjectName};
use crate::ast::query::{SelectStatement, TableRef};
use crate::ast::stmt::{Assignment, OutputClause, Statement};
use crate::span::Span;

/// (V2) One parameter of a procedure or of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcParam {
    /// The name, `@` included.
    pub name: String,
    /// The declared type.
    pub ty: DataType,
    /// The `= expr` default value.
    pub default: Option<Expr>,
    /// True for `OUTPUT` (or `OUT`).
    pub output: bool,
    /// True for `READONLY`.
    pub readonly: bool,
}

/// (V2) `CREATE [OR ALTER] PROCEDURE p @a int AS …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateProcedureStatement {
    /// The procedure name.
    pub name: ObjectName,
    /// The parameters, in order.
    pub params: Vec<ProcParam>,
    /// The body.
    pub body: Vec<Statement>,
    /// True for `CREATE OR ALTER`.
    pub or_alter: bool,
    /// The `WITH` options, kept as written (`RECOMPILE`, `ENCRYPTION`, …).
    pub with_options: Vec<String>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) `CREATE [OR ALTER] FUNCTION f (…) RETURNS … AS …`, scalar or table-valued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateFunctionStatement {
    /// The function name.
    pub name: ObjectName,
    /// The parameters, in order.
    pub params: Vec<ProcParam>,
    /// What the function returns.
    pub returns: FunctionReturns,
    /// The body.
    pub body: FunctionBody,
    /// True for `CREATE OR ALTER`.
    pub or_alter: bool,
    /// The `WITH` options, kept as written.
    pub with_options: Vec<String>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) What a function returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionReturns {
    /// A scalar value of the given type.
    Scalar(DataType),
    /// An inline table, `RETURNS TABLE`.
    Table,
    /// A multi-statement table variable, `RETURNS @t TABLE (…)`.
    TableVariable {
        /// The name of the returned variable, `@` included.
        name: String,
        /// Its columns and constraints.
        definition: Box<TableDefinition>,
    },
}

/// (V2) The body of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionBody {
    /// `BEGIN … END`, for scalar and multi-statement functions.
    Statements(Vec<Statement>),
    /// `RETURN (SELECT …)`, for an inline table-valued function.
    Query(Box<SelectStatement>),
}

/// (V2) `CREATE [OR ALTER] VIEW v AS SELECT …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateViewStatement {
    /// The view name.
    pub name: ObjectName,
    /// The optional column list.
    pub columns: Vec<Ident>,
    /// The query.
    pub query: Box<SelectStatement>,
    /// True for `CREATE OR ALTER`.
    pub or_alter: bool,
    /// The `WITH` options, kept as written (`SCHEMABINDING`, …).
    pub with_options: Vec<String>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) `CREATE [OR ALTER] TRIGGER tr ON t AFTER INSERT AS …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTriggerStatement {
    /// The trigger name.
    pub name: ObjectName,
    /// The table or view it is attached to.
    pub table: ObjectName,
    /// When it fires.
    pub timing: TriggerTiming,
    /// The events it fires on.
    pub events: Vec<TriggerEvent>,
    /// The body.
    pub body: Vec<Statement>,
    /// True for `CREATE OR ALTER`.
    pub or_alter: bool,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) When a trigger fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    /// `AFTER`
    After,
    /// `FOR`, a synonym of `AFTER` that `Display` must keep as written.
    For,
    /// `INSTEAD OF`
    InsteadOf,
}

/// (V2) What a trigger fires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    /// `INSERT`
    Insert,
    /// `UPDATE`
    Update,
    /// `DELETE`
    Delete,
}

/// (V3) `CREATE SEQUENCE s AS int START WITH 1 INCREMENT BY 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSequenceStatement {
    /// The sequence name.
    pub name: ObjectName,
    /// The `AS <type>` clause, when written.
    pub ty: Option<DataType>,
    /// The `START WITH` value.
    pub start_with: Option<i64>,
    /// The `INCREMENT BY` value.
    pub increment_by: Option<i64>,
    /// The `MINVALUE`, absent for `NO MINVALUE` and when nothing was written.
    pub min_value: Option<i64>,
    /// The `MAXVALUE`, absent for `NO MAXVALUE` and when nothing was written.
    pub max_value: Option<i64>,
    /// True for `CYCLE`.
    pub cycle: bool,
    /// The caching clause.
    pub cache: SequenceCache,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V3) The caching clause of a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceCache {
    /// Nothing written: the server default applies.
    Unspecified,
    /// `NO CACHE`
    NoCache,
    /// `CACHE n`
    Size(i64),
}

/// (V3) `MERGE t USING s ON … WHEN …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeStatement {
    /// The table rows are merged into.
    pub target: TableRef,
    /// The source of the rows.
    pub source: TableRef,
    /// The join predicate.
    pub on: Expr,
    /// The `WHEN …` clauses, in order.
    pub clauses: Vec<MergeClause>,
    /// The `OUTPUT` clause.
    pub output: Option<OutputClause>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V3) One `WHEN …` clause of a `MERGE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeClause {
    /// `WHEN MATCHED [AND cond] THEN UPDATE SET …`.
    MatchedUpdate {
        /// The extra `AND` condition.
        condition: Option<Expr>,
        /// The `SET` list.
        assignments: Vec<Assignment>,
    },
    /// `WHEN MATCHED [AND cond] THEN DELETE`.
    MatchedDelete {
        /// The extra `AND` condition.
        condition: Option<Expr>,
    },
    /// `WHEN NOT MATCHED [BY TARGET] [AND cond] THEN INSERT …`.
    NotMatchedInsert {
        /// The extra `AND` condition.
        condition: Option<Expr>,
        /// The column list, empty when the user wrote none.
        columns: Vec<Ident>,
        /// The inserted values, absent for `INSERT DEFAULT VALUES`.
        values: Option<Vec<Expr>>,
    },
    /// `WHEN NOT MATCHED BY SOURCE [AND cond] THEN DELETE`.
    NotMatchedBySourceDelete {
        /// The extra `AND` condition.
        condition: Option<Expr>,
    },
    /// `WHEN NOT MATCHED BY SOURCE [AND cond] THEN UPDATE SET …`.
    NotMatchedBySourceUpdate {
        /// The extra `AND` condition.
        condition: Option<Expr>,
        /// The `SET` list.
        assignments: Vec<Assignment>,
    },
}
