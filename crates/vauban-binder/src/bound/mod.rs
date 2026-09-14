//! The types of the bound plan: statements, relational operators, expressions.
//!
//! Declarations and derivations, and no binding rule.
//!
//! # Derivations
//!
//! The types below derive `Debug` and `Clone`, and not `PartialEq`: a
//! [`BoundExprKind::Function`] holds a `&'static FunctionDef`, and `FunctionDef` derives
//! `Debug, Clone, Copy` and nothing else; [`vauban_types::Value`] has no `Eq` either
//! (`f32`/`f64`). Tests compare the shape of a plan by pattern matching, not by `==`.
//! The two small operator enums, [`CompareOp`] and [`LogicalOp`], do derive the full set.

use vauban_catalog::{AlterTable, ColumnId, IndexDef, QualifiedName, TableDef, TableId};
use vauban_sysfn::FunctionDef;
use vauban_types::{BinaryOp, Collation, TypeInfo, Value};

/// A statement, bound.
///
/// This enum is deliberately **not** `#[non_exhaustive]`: a new variant must break the
/// compilation of the consumers of the workspace.
#[derive(Debug, Clone)]
pub enum BoundStatement {
    /// A `SELECT` and its bound plan.
    Query(Box<LogicalPlan>),
    /// A DDL statement, checked against the catalogue.
    Ddl(DdlStatement),
    /// `USE <database>`: the rest of the batch binds against another database.
    ///
    /// The statement is bound, not executed: switching the current database of the
    /// connection is the business of `session`.
    Use {
        /// Name of the target database, as written and unquoted.
        database: String,
    },
    /// `INSERT`..
    Insert(InsertPlan),
    /// `UPDATE`..
    Update(UpdatePlan),
    /// `DELETE`..
    Delete(DeletePlan),
    /// `SET @x = e`, the assignment of one variable..
    ///
    /// `SELECT @x = e` binds to this variant too: the two spellings assign, and the second
    /// one is not a query (`query.rs`, `assignment`).
    SetVariable {
        /// The name, `@` included, as `DECLARE` wrote it.
        name: String,
        /// The value, already converted to the declared type of the variable.
        value: BoundExpr,
    },
    /// `DECLARE @a int, @b varchar(10) = 'x'`, in the order the declarations were written.
    ///.
    Declare(Vec<BoundDeclaration>),
    /// `IF p stmt [ELSE stmt]`..
    If {
        /// The condition, which satisfies [`BoundExpr::is_predicate`].
        condition: BoundExpr,
        /// The statement run when the condition is true. Named with a trailing `_`:
        /// `then` is not a Rust keyword, but `else_` next to it is, and the pair reads as
        /// one.
        then_: Box<BoundStatement>,
        /// The statement of the `ELSE`, `None` for an `IF` written without that clause.
        else_: Option<Box<BoundStatement>>,
    },
    /// `WHILE p stmt`..
    While {
        /// The condition, which satisfies [`BoundExpr::is_predicate`].
        condition: BoundExpr,
        /// The body of the loop.
        body: Box<BoundStatement>,
    },
    /// `BEGIN … END`, the statements in the order they were written..
    Block(Vec<BoundStatement>),
    /// `BREAK`..
    Break,
    /// `CONTINUE`..
    Continue,
    /// `RETURN [e]`, the expression being `None` for a bare `RETURN`..
    Return(Option<BoundExpr>),
    /// `PRINT e`..
    Print(BoundExpr),
    /// `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`..
    Transaction(TxnStatement),
}

/// One `@x <type> [= e]` of a `DECLARE`, bound.
#[derive(Debug, Clone)]
pub struct BoundDeclaration {
    /// The name, `@` included, as written.
    pub name: String,
    /// The declared type, nullability and collation included.
    pub ty: TypeInfo,
    /// The initial value, already converted to `ty`; `None` for a `DECLARE` written without
    /// `=`, which is the `NULL` a variable starts at.
    pub value: Option<BoundExpr>,
}

/// An `INSERT`, bound.
///
/// The three spellings — `VALUES`, `INSERT … SELECT` and `DEFAULT VALUES` — differ by
/// `source` alone: a [`LogicalPlan::Values`] for the first, the bound query for the
/// second, a `Values` of one empty row for the third.
#[derive(Debug, Clone)]
pub struct InsertPlan {
    /// The table written into, as the catalogue holds it in
    /// [`TableMeta::storage_id`](vauban_catalog::TableMeta::storage_id).
    pub table: TableId,
    /// The target columns, in the order the rows of `source` fill them: the written column
    /// list, or the columns of the table for an `INSERT` written without that list.
    pub columns: Vec<ColumnBinding>,
    /// The rows to insert, one column of the schema per entry of `columns`.
    pub source: Box<LogicalPlan>,
}

/// An `UPDATE`, bound.
#[derive(Debug, Clone)]
pub struct UpdatePlan {
    /// The table written into.
    pub table: TableId,
    /// The rows to update: the `Scan` of the table, under the `Filter` of the `WHERE` and,
    /// for an `UPDATE … FROM`, under the `Join`.
    pub input: Box<LogicalPlan>,
    /// One `SET c = e` per entry, in the order they were written: the column assigned and
    /// the value, already converted to the type of that column.
    pub assignments: Vec<(ColumnBinding, BoundExpr)>,
}

/// A `DELETE`, bound.
#[derive(Debug, Clone)]
pub struct DeletePlan {
    /// The table rows are deleted from.
    pub table: TableId,
    /// The rows to delete, built like [`UpdatePlan::input`].
    pub input: Box<LogicalPlan>,
}

/// A transaction statement, bound.
///
/// The name of a transaction is kept as written and unquoted; SQL Server takes it on
/// `BEGIN`, `COMMIT` and `ROLLBACK`, and requires it on `SAVE TRANSACTION`.
#[derive(Debug, Clone)]
pub enum TxnStatement {
    /// `BEGIN TRAN[SACTION] [name [WITH MARK ['text']]]`.
    Begin {
        /// The name written after the keyword, `None` for a bare `BEGIN TRANSACTION`.
        name: Option<String>,
        /// The text of `WITH MARK`, `None` when the clause was not written; the empty
        /// string for a `WITH MARK` written without one.
        mark: Option<String>,
    },
    /// `COMMIT [TRAN[SACTION] [name]]` and `COMMIT WORK`.
    Commit {
        /// The name written after the keyword, `None` for the bare `COMMIT`.
        name: Option<String>,
    },
    /// `ROLLBACK [TRAN[SACTION] [name]]` and `ROLLBACK WORK`.
    ///
    /// The name is that of a transaction or of a savepoint: which one it reaches is
    /// decided at run time, not while the statement is bound.
    Rollback {
        /// The name written after the keyword, `None` for the bare `ROLLBACK`.
        name: Option<String>,
    },
    /// `SAVE TRAN[SACTION] name`, which names a savepoint.
    Save {
        /// The name of the savepoint.
        name: String,
    },
}

/// A DDL statement, bound.
///
/// The payloads are the **minimum** each variant needs to reach the catalogue. A `DROP`
/// carries the names it was written with, not the resolved
/// [`ObjectId`](vauban_catalog::ObjectId): which of them exist, and what `IF EXISTS` does
/// with the others, is decided where the statement runs.
#[derive(Debug, Clone)]
pub enum DdlStatement {
    /// `CREATE DATABASE d [COLLATE c]`..
    CreateDatabase {
        /// Name of the database to create.
        name: String,
        /// Collation written after `COLLATE`, `None` for the collation of the instance.
        collation: Option<Collation>,
    },
    /// `DROP DATABASE [IF EXISTS] d1, d2`..
    DropDatabase {
        /// Names of the databases to drop, in the order they were written.
        names: Vec<String>,
        /// True for `IF EXISTS`.
        if_exists: bool,
    },
    /// `CREATE TABLE t (…)`..
    CreateTable {
        /// The table as [`Catalog::create_table`](vauban_catalog::Catalog::create_table)
        /// takes it.
        def: TableDef,
    },
    /// `DROP TABLE [IF EXISTS] t1, t2`..
    DropTable {
        /// Three-part names of the tables to drop, in the order they were written.
        names: Vec<QualifiedName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
    },
    /// `CREATE INDEX ix ON t (c)`, and the index behind a `PRIMARY KEY` or a `UNIQUE`
    /// constraint..
    CreateIndex {
        /// The index as [`Catalog::create_index`](vauban_catalog::Catalog::create_index)
        /// takes it.
        def: IndexDef,
    },
    /// `DROP INDEX [IF EXISTS] ix ON t`..
    DropIndex {
        /// Name of the index, which is unique within its table and not within the database.
        name: String,
        /// Three-part name of the table the index belongs to.
        table: QualifiedName,
        /// True for `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER DATABASE d SET <option>`..
    AlterDatabase {
        /// Name of the database, as written and unquoted.
        name: String,
        /// The options written after `SET`, in order: the name of the option and its
        /// value, both as written, the value absent for a flag-like option. Which of them
        /// the versioning options are, and what they mean, is the catalogue's business.
        options: Vec<(String, Option<String>)>,
    },
    /// `ALTER TABLE t <action>`..
    AlterTable {
        /// Three-part name of the table altered.
        table: QualifiedName,
        /// The action, as [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table)
        /// takes it. The payload here is the pair the catalogue reads, so growing the
        /// action list does not reopen this file.
        action: AlterTable,
    },
}

/// A node of the bound logical plan: what to compute, never how.
///
/// Choosing an algorithm, an index or a join order belongs to the `planner`; evaluating
/// belongs to the `executor`.
#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// One row, zero column: the implicit source of a `SELECT` without `FROM`.
    OneRow,
    /// Reads the rows of one table (`FROM t`).
    Scan {
        /// The table in `storage`, as the catalogue holds it in
        /// [`TableMeta::storage_id`](vauban_catalog::TableMeta::storage_id).
        table: TableId,
        /// The columns read, in the order the catalogue gives them.
        ///
        /// `columns[i].index` is **not** `i`: it is the 0-based `ordinal` of the column in
        /// the row `storage` hands out, which the drop of a column before it does not
        /// close up (`catalog_view.rs`, `columns_are_indexed_by_ordinal_not_by_identifier`;
        /// `star.rs`, `the_expanded_columns_index_the_row_of_storage`). The two readings
        /// agree on a table no column was ever dropped from, which is why the field says
        /// which one it is.
        columns: Vec<ColumnBinding>,
        /// The name the rest of the query refers to this source by: the alias when one was
        /// written, the object part of the table name otherwise.
        alias: String,
        /// The columns this node produces, in the same order as `columns`.
        ///
        /// The field is here because [`LogicalPlan::schema`] takes no catalogue: it
        /// returns a `&OutputSchema` from the plan alone, so the names and types a `Scan`
        /// publishes are read from the catalogue once, while the statement is bound, and
        /// carried in the node.
        schema: OutputSchema,
        /// The locking hints written on the reference, [`LockHints::default`] when none
        /// were.
        hints: LockHints,
    },
    /// Constant rows (`INSERT … VALUES`).
    Values {
        /// The rows, all of the same width as `schema`.
        rows: Vec<Vec<BoundExpr>>,
        /// The type of each column of the rows.
        schema: OutputSchema,
    },
    /// Keeps the rows of `input` for which `predicate` is true (`WHERE`, `HAVING`).
    ///
    /// `predicate` satisfies [`BoundExpr::is_predicate`]: `bind_condition` raises error
    /// 4145 otherwise.
    Filter {
        /// The rows to filter.
        input: Box<LogicalPlan>,
        /// The condition to keep a row.
        predicate: BoundExpr,
    },
    /// Computes a new row from each row of `input` (the select list).
    Project {
        /// The rows to project.
        input: Box<LogicalPlan>,
        /// One expression and one output name per column.
        exprs: Vec<BoundProjection>,
        /// The type of each projected column, in the same order as `exprs`.
        schema: OutputSchema,
    },
    /// `TOP n [PERCENT] [WITH TIES]` over `input`.
    Limit {
        /// The rows to truncate.
        input: Box<LogicalPlan>,
        /// How many rows to keep.
        top: BoundTop,
    },
    /// Pairs the rows of two inputs: `FROM a JOIN b ON …`, and `FROM a, b`, which is the
    /// same node with [`JoinKind::Cross`]..
    Join {
        /// The left input.
        left: Box<LogicalPlan>,
        /// The right input.
        right: Box<LogicalPlan>,
        /// How the rows are paired.
        kind: JoinKind,
        /// The `ON` predicate, which satisfies [`BoundExpr::is_predicate`]. `None` for a
        /// `CROSS JOIN` and for the comma of `FROM a, b`, the two forms written without an `ON`.
        on: Option<BoundExpr>,
        /// The columns of `left` **then** those of `right`, concatenated in that order.
        ///
        /// A [`BoundExprKind::ColumnRef`] above a `Join` indexes that concatenation, so a
        /// column of `right` has the index it had in `right` plus the width of `left`
        /// (`tests/bound_shape_relational.rs`, `every_relational_plan_variant_reports_its_schema`).
        schema: OutputSchema,
    },
    /// Groups the rows of `input` and computes one row per group: `GROUP BY`, and a select
    /// list holding an aggregate..
    ///
    /// # How the result of an aggregate is referred to
    ///
    /// `schema` holds the `group_by` keys first, then the `aggregates`, each in the order
    /// of its own vector. The `Project` above an `Aggregate` designates one of those
    /// columns with a [`BoundExprKind::ColumnRef`] whose `index` is its position in this
    /// schema, and no expression variant is added for the purpose: `COUNT(*)` in a select
    /// list becomes an entry of `aggregates` and a `ColumnRef` on it.
    /// `tests/bound_shape_relational.rs` pins that order.
    Aggregate {
        /// The rows to group.
        input: Box<LogicalPlan>,
        /// The `GROUP BY` keys, in the order they were written; empty for an aggregate
        /// written without a `GROUP BY`, which computes one row over the whole input.
        group_by: Vec<BoundExpr>,
        /// The aggregates to compute, in the order they were met in the statement.
        aggregates: Vec<AggregateCall>,
        /// The `group_by` keys followed by the `aggregates`, in that order.
        schema: OutputSchema,
    },
    /// Orders the rows of `input` (`ORDER BY`)..
    ///
    /// A `Sort` produces the columns it was handed, so [`LogicalPlan::schema`] delegates
    /// to `input`: a key written on a column the select list drops is `sort.rs`'s business,
    /// not a column of this node.
    Sort {
        /// The rows to order.
        input: Box<LogicalPlan>,
        /// The keys, most significant first.
        keys: Vec<SortKey>,
    },
    /// Removes the duplicate rows of its input (`SELECT DISTINCT`); its schema is that of
    /// the input.
    Distinct(Box<LogicalPlan>),
    /// `UNION [ALL]`, `EXCEPT` and `INTERSECT`..
    SetOp {
        /// Which of the three operators was written.
        op: SetOpKind,
        /// True when `ALL` was written, which keeps the duplicate rows.
        all: bool,
        /// The left operand.
        left: Box<LogicalPlan>,
        /// The right operand.
        right: Box<LogicalPlan>,
        /// One column per column of the operands, which have the same width (205
        /// otherwise): the name comes from the left operand and the type is the common
        /// type of the column of both.
        schema: OutputSchema,
    },
    /// A derived table, `FROM (SELECT …) AS d`..
    ///
    /// The node exists to carry the `alias`, which the rest of the query qualifies its
    /// columns with; a derived table computes nothing of its own.
    Subquery {
        /// The plan of the parenthesised query.
        input: Box<LogicalPlan>,
        /// The alias, which T-SQL requires on a derived table.
        alias: String,
        /// The columns of `input`, renamed by the column list of the derived table when
        /// one was written.
        schema: OutputSchema,
    },
}

/// How a [`LogicalPlan::Join`] pairs the rows of its two inputs.
///
/// The binder's own enum, deliberately distinct from
/// [`vauban_parser::JoinKind`](vauban_parser::JoinKind): the parser keeps what was written,
/// the bound plan keeps what is computed, and the binder is free to turn a written `RIGHT`
/// into another shape without the plan saying `Right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    /// Keeps the paired rows alone.
    Inner,
    /// Keeps the unpaired rows of the left input too, padded with `NULL` on the right side.
    Left,
    /// Keeps the unpaired rows of the right input too, padded with `NULL` on the left side.
    Right,
    /// Keeps the unpaired rows of both inputs too, padded with `NULL` on the other side.
    Full,
    /// Pairs each row of the left input with each row of the right: `CROSS JOIN`, and the
    /// comma of `FROM a, b`.
    Cross,
}

/// One aggregate computed by a [`LogicalPlan::Aggregate`]..
#[derive(Debug, Clone)]
pub struct AggregateCall {
    /// The definition the `sysfn` registry handed out, whose
    /// [`kind`](vauban_sysfn::FunctionDef::kind) is
    /// [`FunctionKind::Aggregate`](vauban_sysfn::FunctionKind::Aggregate).
    pub def: &'static FunctionDef,
    /// The argument, already converted to the type the aggregate takes. `None` is a call
    /// written with a star — `COUNT(*)` and `COUNT_BIG(*)`, the two of the six aggregates
    /// of `vauban-sysfn` (`builtins/aggregates.rs`) that count rows rather than values —
    /// and it is what tells `COUNT(*)` from `COUNT(c)`, which skips the `NULL` rows.
    pub arg: Option<BoundExpr>,
    /// True for `COUNT(DISTINCT c)` and its siblings.
    pub distinct: bool,
}

/// One key of a [`LogicalPlan::Sort`]..
#[derive(Debug, Clone)]
pub struct SortKey {
    /// The expression to order by, already resolved: an `ORDER BY` written as an alias or
    /// as a position is resolved by the binder, and the plan keeps the expression.
    pub expr: BoundExpr,
    /// True for `DESC`. `ASC` is the default and is not kept.
    pub desc: bool,
    /// The collation of a `ORDER BY c COLLATE …`, `None` when the clause was not written,
    /// which orders by the collation of the type of `expr`.
    pub collation: Option<Collation>,
}

/// Which set operator a [`LogicalPlan::SetOp`] applies..
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetOpKind {
    /// `UNION`
    Union,
    /// `EXCEPT`
    Except,
    /// `INTERSECT`
    Intersect,
}

/// The locking hints written on a table reference, one flag per word.
///
/// [`LockHints::default`] is the value of a reference written without a hint. `hints.rs`
/// reads the words into this struct and raises 1047 and 1065.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LockHints {
    /// `NOLOCK` and `READUNCOMMITTED`, which read uncommitted rows.
    pub nolock: bool,
    /// `READCOMMITTED` and `READCOMMITTEDLOCK`.
    pub readcommitted: bool,
    /// `REPEATABLEREAD`.
    pub repeatableread: bool,
    /// `SERIALIZABLE` and `HOLDLOCK`, which are the same hint under two words.
    pub serializable: bool,
    /// `SNAPSHOT`.
    pub snapshot: bool,
    /// `UPDLOCK`, an update lock taken while reading.
    pub updlock: bool,
    /// `XLOCK`, an exclusive lock taken while reading.
    pub xlock: bool,
    /// `ROWLOCK`, which asks for row granularity.
    pub rowlock: bool,
    /// `PAGLOCK`, which asks for page granularity.
    pub paglock: bool,
    /// `TABLOCK`, a shared lock on the whole table.
    pub tablock: bool,
    /// `TABLOCKX`, an exclusive lock on the whole table.
    pub tablockx: bool,
    /// `READPAST`, which skips the locked rows instead of waiting.
    pub readpast: bool,
    /// `NOWAIT`, which raises instead of waiting.
    pub nowait: bool,
}

/// The schema of a plan with no column, returned by [`LogicalPlan::schema`] for
/// [`LogicalPlan::OneRow`].
static EMPTY_SCHEMA: OutputSchema = OutputSchema {
    columns: Vec::new(),
};

impl LogicalPlan {
    /// The columns this node produces.
    ///
    /// An operator that does not change the shape of its input (`Filter`, `Limit`,
    /// `Sort`, `Distinct`) delegates to that input; `OneRow` has no column at all. The
    /// order the operators that do build a schema put their columns in is written on each
    /// of them, and pinned by `tests/bound_shape_relational.rs`.
    #[must_use]
    pub fn schema(&self) -> &OutputSchema {
        match self {
            LogicalPlan::OneRow => &EMPTY_SCHEMA,
            LogicalPlan::Scan { schema, .. }
            | LogicalPlan::Values { schema, .. }
            | LogicalPlan::Project { schema, .. }
            | LogicalPlan::Join { schema, .. }
            | LogicalPlan::Aggregate { schema, .. }
            | LogicalPlan::SetOp { schema, .. }
            | LogicalPlan::Subquery { schema, .. } => schema,
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Sort { input, .. } => input.schema(),
            LogicalPlan::Distinct(input) => input.schema(),
        }
    }
}

/// One column of a `Project`: the expression and the name it is returned under.
///
/// The name is the one the client sees in the `COLMETADATA` of the result set; it is empty
/// for a column SQL Server leaves unnamed (`SELECT 1`).
#[derive(Debug, Clone)]
pub struct BoundProjection {
    /// The expression to evaluate for each row.
    pub expr: BoundExpr,
    /// The output column name, possibly empty.
    pub name: String,
}

/// A bound `TOP` clause.
#[derive(Debug, Clone)]
pub struct BoundTop {
    /// The number of rows, or the percentage when `percent`.
    pub expr: BoundExpr,
    /// True for `TOP (n) PERCENT`.
    pub percent: bool,
    /// True for `WITH TIES`.
    pub with_ties: bool,
}

/// One column of a source, named once at bind time and read by position afterwards.
///
/// The same type serves a [`LogicalPlan::Scan`], where it lists what the node reads, and a
/// [`BoundExprKind::ColumnRef`], where it says which value of the input row the expression
/// takes. `name` and `ty` are copied out of the catalogue while the statement is bound: the
/// executor evaluates a `ColumnRef` by `index` alone and consults nothing, and the tests
/// have the name and the type to print without a catalogue.
#[derive(Debug, Clone)]
pub struct ColumnBinding {
    /// Identifier of the column within its table (`sys.columns.column_id`), which survives
    /// the drop of a column before it.
    pub column: ColumnId,
    /// 0-based `ordinal` of the value in the row `storage` produces, which the executor
    /// reads to pick a column out of it. Distinct from `column`, which is an identifier, and
    /// distinct from the position the column takes in the output of the node that reads it
    /// (`star.rs`, `the_expanded_columns_index_the_row_of_storage`).
    pub index: usize,
    /// Name of the column, for the `COLMETADATA` of a `SELECT *` and for the messages of
    /// errors 207 and 209.
    pub name: String,
    /// Type of the column, nullability and collation included.
    pub ty: TypeInfo,
}

/// The columns a plan node produces, in order.
#[derive(Debug, Clone)]
pub struct OutputSchema {
    /// The columns, in the order the client receives them.
    pub columns: Vec<OutputColumn>,
}

/// One column of an [`OutputSchema`].
#[derive(Debug, Clone)]
pub struct OutputColumn {
    /// The name sent to the client, possibly empty.
    pub name: String,
    /// The type sent to the client, nullability and collation included.
    pub ty: TypeInfo,
}

/// A bound expression: what it is, what type it has, and where it was written.
#[derive(Debug, Clone)]
pub struct BoundExpr {
    /// The shape of the expression.
    pub kind: BoundExprKind,
    /// The inferred type. `bit` for the predicate variants: T-SQL has no boolean type, so
    /// [`BoundExpr::is_predicate`], not this field, says what a node is.
    pub ty: TypeInfo,
    /// 1-based line of the AST node this comes from, taken from its `parser::Span`.
    ///
    /// A runtime error (8134 divide by zero, 8115 overflow) carries the line of the
    /// expression that raised it. The whole span is not kept: the column and the length serve the `near '…'` of a message,
    /// which the binder resolves while it still holds the AST.
    pub line: u32,
}

/// The shape of a bound expression.
///
/// Three families of operators, where the AST has one: [`BoundExprKind::Arith`] carries a
/// [`vauban_types::BinaryOp`] (the executor calls `types::eval_binary` with it),
/// [`BoundExprKind::Compare`] a [`CompareOp`] and [`BoundExprKind::Logical`] a
/// [`LogicalOp`]. `BETWEEN` is desugared into `Logical`/`Compare`, and an implicit
/// conversion is materialised as a [`BoundExprKind::Convert`] node.
#[derive(Debug, Clone)]
pub enum BoundExprKind {
    /// A literal value.
    Literal(Value),
    /// A column of the input row of the node that holds the expression.
    ColumnRef(ColumnBinding),
    /// A local variable `@x`. `@@x` is a function, not a variable.
    Variable {
        /// The name, `@` included.
        name: String,
    },
    /// An arithmetic, bitwise or concatenation operation.
    Arith {
        /// The operator, `Concat` included: the binder decided between `+` and `||`.
        op: BinaryOp,
        /// Left operand.
        left: Box<BoundExpr>,
        /// Right operand.
        right: Box<BoundExpr>,
    },
    /// `- expr`.
    Negate(Box<BoundExpr>),
    /// `~ expr`.
    BitNot(Box<BoundExpr>),
    /// A comparison. Predicate.
    Compare {
        /// The normalised operator.
        op: CompareOp,
        /// Left operand.
        left: Box<BoundExpr>,
        /// Right operand.
        right: Box<BoundExpr>,
    },
    /// `AND` or `OR`. Predicate.
    Logical {
        /// The operator.
        op: LogicalOp,
        /// Left operand, a predicate.
        left: Box<BoundExpr>,
        /// Right operand, a predicate.
        right: Box<BoundExpr>,
    },
    /// `NOT p`. Predicate.
    Not(Box<BoundExpr>),
    /// `e IS [NOT] NULL`. Predicate.
    IsNull {
        /// The tested value.
        expr: Box<BoundExpr>,
        /// True for `IS NOT NULL`.
        negated: bool,
    },
    /// `e [NOT] IN (v1, v2, …)`. Predicate.
    In {
        /// The tested value.
        expr: Box<BoundExpr>,
        /// The values compared against, already converted to the common type.
        list: Vec<BoundExpr>,
        /// True for `NOT IN`.
        negated: bool,
    },
    /// `e [NOT] LIKE p [ESCAPE c]`. Predicate.
    Like {
        /// The tested value.
        expr: Box<BoundExpr>,
        /// The pattern.
        pattern: Box<BoundExpr>,
        /// The `ESCAPE` character, when written.
        escape: Option<Box<BoundExpr>>,
        /// True for `NOT LIKE`.
        negated: bool,
    },
    /// A `CASE`, simple when `operand` is set, searched otherwise.
    Case {
        /// The value a simple `CASE` compares each `WHEN` against.
        operand: Option<Box<BoundExpr>>,
        /// The `WHEN … THEN …` arms, at least one.
        arms: Vec<BoundCaseArm>,
        /// The `ELSE` result; `NULL` of the result type when the user wrote none.
        else_: Option<Box<BoundExpr>>,
    },
    /// A conversion, written by the user (`CAST`, `CONVERT`, `TRY_*`) or **inserted by the
    /// binder** for an implicit one. The target type is the `ty` of the node.
    Convert {
        /// The value to convert.
        expr: Box<BoundExpr>,
        /// The `CONVERT` style argument, when written.
        style: Option<i32>,
        /// True for `TRY_CAST` and `TRY_CONVERT`, which yield `NULL` instead of raising.
        try_: bool,
    },
    /// A call of a built-in function.
    ///
    /// The variant carries no extra field on purpose: the `EvalArgs` the executor builds
    /// is exactly `values` from evaluating `args`, `types` from `args[i].ty`, and `result`
    /// from the `ty` of this very node — the one `check_call` computed at bind time. This
    /// is the contract the executor relies on.
    Function {
        /// The definition the `sysfn` registry handed out.
        def: &'static FunctionDef,
        /// The arguments, in call order.
        args: Vec<BoundExpr>,
    },
    /// `e COLLATE Latin1_General_CI_AS`: the collation is in the `ty` of the node, so the
    /// variant only marks that the user asked for it.
    Collate {
        /// The value whose collation changes.
        expr: Box<BoundExpr>,
    },
    /// `[NOT] EXISTS (SELECT …)`. Predicate..
    ///
    /// `NOT EXISTS` is [`BoundExprKind::Not`] over this variant, as the parser builds it.
    Exists(Box<LogicalPlan>),
    /// A scalar subquery, `(SELECT …)` in the place of a value..
    ///
    /// The `ty` of the node is the type of the single column of the plan; a plan of
    /// another width answers 116 at bind time.
    ScalarSubquery(Box<LogicalPlan>),
    /// `e [NOT] IN (SELECT …)`. Predicate..
    ///
    /// A variant of its own, and **not** a third field of [`BoundExprKind::In`]: the list
    /// form compares against values the binder converted to one common type, which is the
    /// evaluation path the executor has for it, while this one runs a plan. Folding the two
    /// would make one variant carry two ways of evaluating it.
    InSubquery {
        /// The tested value.
        expr: Box<BoundExpr>,
        /// The plan of the subquery, of one column (116 otherwise).
        plan: Box<LogicalPlan>,
        /// True for `NOT IN`.
        negated: bool,
    },
}

/// A comparison operator, **normalised**.
///
/// The AST keeps the form the user wrote, the bound plan keeps the meaning: `!<` becomes
/// [`CompareOp::Ge`] and `!>` becomes [`CompareOp::Le`], `!=` and `<>` are the same
/// [`CompareOp::Ne`]. The executor therefore never sees a spelling, only an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompareOp {
    /// `=`
    Eq,
    /// `<>`, `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`, `!>`
    Le,
    /// `>`
    Gt,
    /// `>=`, `!<`
    Ge,
}

/// A logical connective. `NOT` is [`BoundExprKind::Not`], not an operator here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalOp {
    /// `AND`
    And,
    /// `OR`
    Or,
}

/// One `WHEN … THEN …` arm of a bound `CASE`.
#[derive(Debug, Clone)]
pub struct BoundCaseArm {
    /// The `WHEN` part: a predicate for a searched `CASE`, a value for a simple one.
    pub when: BoundExpr,
    /// The `THEN` part, already converted to the result type of the `CASE`.
    pub then: BoundExpr,
}

impl BoundExpr {
    /// `true` for the variants that only make sense in a boolean context: `Compare`,
    /// `Logical`, `Not`, `IsNull`, `In`, `Like`, and the two relational predicates
    /// `Exists` and `InSubquery`.
    ///
    /// [`BoundExprKind::ScalarSubquery`] is **not** one of them: `(SELECT 1)` is a value,
    /// and `WHERE (SELECT 1)` answers 4145 as `WHERE 1` does.
    ///
    /// T-SQL has no boolean type, so a predicate is recognised by its variant and not by
    /// its type. `expr.rs` uses this to raise error 4145 where a condition was expected;
    /// the other direction — a predicate used as a value, `SELECT (1 = 1)` — does not reach
    /// the binder, the parser rejects it as a syntax error, like SQL Server.
    #[must_use]
    pub fn is_predicate(&self) -> bool {
        matches!(
            self.kind,
            BoundExprKind::Compare { .. }
                | BoundExprKind::Logical { .. }
                | BoundExprKind::Not(_)
                | BoundExprKind::IsNull { .. }
                | BoundExprKind::In { .. }
                | BoundExprKind::Like { .. }
                | BoundExprKind::Exists(_)
                | BoundExprKind::InSubquery { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ColumnBinding, LogicalPlan, OutputColumn, OutputSchema};
    use vauban_catalog::{ColumnId, TableId};
    use vauban_types::{SqlType, TypeInfo};

    /// `LogicalPlan::schema` reads the `schema` field of a `Scan` and consults nothing else:
    /// it takes no catalogue, so what a `Scan` publishes is what was written into the node
    /// when the statement was bound.
    #[test]
    fn scan_schema_is_the_declared_schema() {
        let declared = OutputSchema {
            columns: vec![
                OutputColumn {
                    name: "id".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, false),
                },
                OutputColumn {
                    name: "label".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, true),
                },
            ],
        };
        let scan = LogicalPlan::Scan {
            table: TableId(7),
            columns: vec![
                ColumnBinding {
                    column: ColumnId(1),
                    index: 0,
                    name: "id".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, false),
                },
                ColumnBinding {
                    column: ColumnId(4),
                    index: 1,
                    name: "label".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, true),
                },
            ],
            alias: "t".to_owned(),
            schema: declared.clone(),
            hints: super::LockHints::default(),
        };

        let schema = scan.schema();
        assert_eq!(schema.columns.len(), declared.columns.len());
        assert_eq!(schema.columns[0].name, "id");
        assert_eq!(schema.columns[0].ty, TypeInfo::new(SqlType::Int, false));
        assert_eq!(schema.columns[1].name, "label");
        assert_eq!(schema.columns[1].ty, TypeInfo::new(SqlType::Int, true));

        // A node that does not change the shape of its input delegates to it, so the same
        // schema comes back through a `Filter` above the `Scan`.
        let limited = LogicalPlan::Limit {
            input: Box::new(scan),
            top: super::BoundTop {
                expr: super::BoundExpr {
                    kind: super::BoundExprKind::Literal(vauban_types::Value::I32(1)),
                    ty: TypeInfo::new(SqlType::Int, false),
                    line: 1,
                },
                percent: false,
                with_ties: false,
            },
        };
        assert_eq!(limited.schema().columns[1].name, "label");
    }
}
