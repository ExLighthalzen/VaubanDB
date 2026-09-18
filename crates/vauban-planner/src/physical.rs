//! The types of the physical plan: statements, operators, key ranges.
//!
//! Declarations and derivations: no planning rule lives in this file. The variants are
//! declared here and produced by the rule files; a variant no rule produces yet says so in
//! its own documentation.
//!
//! # Derivations
//!
//! Every type derives `Debug` and `Clone`, like the bound plan
//! ([`vauban_binder`], `bound/mod.rs`): a [`BoundExpr`] holds a `&'static FunctionDef` and
//! a [`vauban_types::Value`], neither of which is `Eq`, so no plan type derives
//! `PartialEq`. Tests compare the shape of a plan by pattern matching. The small
//! enumerations that carry no expression — [`PhysicalJoinKind`] — derive the full set.

use std::ops::Bound;

use vauban_binder::BoundDeclaration;
use vauban_binder::{
    AggregateCall, BoundExpr, BoundProjection, BoundTop, ColumnBinding, DdlStatement, JoinKind,
    LockHints, OutputSchema, SortKey, TxnStatement,
};
use vauban_catalog::{TableDef, TableId};
use vauban_storage::{Direction, IndexId};

/// A statement, planned: what the `executor` runs.
///
/// One variant per variant of [`BoundStatement`](vauban_binder::BoundStatement).
/// Deliberately **not** `#[non_exhaustive]`: adding a variant must break the compilation
/// of the consumers rather than pass unnoticed.
///
/// The variants that carry no relational plan — DDL, `USE`, `DECLARE`, the transaction
/// statements — hold the payload the binder built, unchanged: planning has nothing to
/// choose for them, and `plan.rs` moves them across.
#[derive(Debug, Clone)]
pub enum PhysicalStatement {
    /// A `SELECT` and its physical plan.
    Query(PhysicalPlan),
    /// A DDL statement, carried across from the bound statement.
    Ddl(DdlStatement),
    /// `USE <database>`, carried across from the bound statement.
    Use {
        /// Name of the target database, as written and unquoted.
        database: String,
    },
    /// `INSERT`. Not produced yet.
    Insert(PhysicalInsert),
    /// `SELECT … INTO`.
    SelectInto(PhysicalSelectInto),
    /// `UPDATE`. Not produced yet.
    Update(PhysicalUpdate),
    /// `DELETE`. Not produced yet.
    Delete(PhysicalDelete),
    /// `SET @x = e`, the assignment of one variable.
    SetVariable {
        /// The name, `@` included.
        name: String,
        /// The value, as the binder converted it.
        value: BoundExpr,
    },
    /// `DECLARE @a int, @b varchar(10) = 'x'`, in the order the declarations were written.
    Declare(Vec<BoundDeclaration>),
    /// `IF p stmt [ELSE stmt]`, both branches planned.
    If {
        /// The condition, which satisfies
        /// [`BoundExpr::is_predicate`](vauban_binder::BoundExpr::is_predicate).
        condition: BoundExpr,
        /// The statement run when the condition is true, named as in the bound statement.
        then_: Box<PhysicalStatement>,
        /// The statement of the `ELSE`, `None` for an `IF` written without that clause.
        else_: Option<Box<PhysicalStatement>>,
    },
    /// `WHILE p stmt`, body planned.
    While {
        /// The condition, which satisfies
        /// [`BoundExpr::is_predicate`](vauban_binder::BoundExpr::is_predicate).
        condition: BoundExpr,
        /// The body of the loop.
        body: Box<PhysicalStatement>,
    },
    /// `BEGIN … END`, the statements planned in the order they were written.
    Block(Vec<PhysicalStatement>),
    /// `BREAK`.
    Break,
    /// `CONTINUE`.
    Continue,
    /// `RETURN [e]`, the expression being `None` for a bare `RETURN`.
    Return(Option<BoundExpr>),
    /// `PRINT e`.
    Print(BoundExpr),
    /// `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`.
    Transaction(TxnStatement),
}

/// A node of the physical plan: how the rows are produced.
///
/// The bound plan says what to compute ([`LogicalPlan`](vauban_binder::LogicalPlan)); this
/// one says with which operator. An executor is meant to build one operator per variant,
/// which is why the whole list is declared here and the enum is not `#[non_exhaustive]`.
#[derive(Debug, Clone)]
pub enum PhysicalPlan {
    /// One row, zero column: the source of a `SELECT` without `FROM`.
    OneRow,
    /// Constant rows.
    Values {
        /// The rows, of the width of `schema`.
        rows: Vec<Vec<BoundExpr>>,
        /// The type of each column of the rows.
        schema: OutputSchema,
    },
    /// Reads the rows of one table, in the order `storage` hands them out.
    ///
    /// `hints` are the lock hints the reference carried
    /// ([`LogicalPlan::Scan`](vauban_binder::LogicalPlan::Scan)), copied unchanged: this
    /// node decides nothing about them, and whoever takes a lock reads them
    /// ([`LockHints`](vauban_binder::LockHints)).
    TableScan {
        /// The table in `storage`.
        table: TableId,
        /// The columns read, each carrying the 0-based `index` of its value in the row
        /// `storage` produces.
        columns: Vec<ColumnBinding>,
        /// The name the rest of the query refers to this source by.
        alias: String,
        /// The columns this node produces, in the same order as `columns`.
        schema: OutputSchema,
        /// The lock hints written on the reference this scan comes from,
        /// [`LockHints::default`] for a reference written without a hint.
        hints: LockHints,
    },
    /// Reads the rows an index serves for a range of keys.
    ///
    /// `index` is what [`Storage::seek`](vauban_storage::Storage::seek) takes, and it
    /// identifies the table on its own: the trait keys an index by `IndexId` alone.
    IndexSeek {
        /// The index to walk.
        index: IndexId,
        /// The keys served, with bounds still to evaluate.
        range: KeyRangeExpr,
        /// The columns read, as for a [`PhysicalPlan::TableScan`].
        columns: Vec<ColumnBinding>,
        /// Forward or backward relative to the index order.
        direction: Direction,
        /// The columns this node produces, in the same order as `columns`.
        schema: OutputSchema,
        /// The lock hints of the scan this seek replaces, copied unchanged.
        hints: LockHints,
    },
    /// Keeps the rows of `input` for which `predicate` is true.
    Filter {
        /// The rows to filter.
        input: Box<PhysicalPlan>,
        /// The condition to keep a row, which satisfies
        /// [`BoundExpr::is_predicate`](vauban_binder::BoundExpr::is_predicate).
        predicate: BoundExpr,
    },
    /// Computes a new row from each row of `input`.
    Project {
        /// The rows to project.
        input: Box<PhysicalPlan>,
        /// One expression and one output name per column, as the binder built them.
        exprs: Vec<BoundProjection>,
        /// The type of each projected column, in the same order as `exprs`.
        schema: OutputSchema,
    },
    /// `TOP n [PERCENT] [WITH TIES]` over `input`, without a sort of its own.
    Top {
        /// The rows to truncate.
        input: Box<PhysicalPlan>,
        /// How many rows to keep.
        top: BoundTop,
    },
    /// Pairs the rows of two inputs by walking the inner one for each row of the outer.
    /// Not produced yet.
    NestedLoopJoin {
        /// The input walked once.
        outer: Box<PhysicalPlan>,
        /// The input walked for each row of `outer`.
        inner: Box<PhysicalPlan>,
        /// How the rows are paired.
        kind: PhysicalJoinKind,
        /// The join predicate, `None` for a cross join.
        on: Option<BoundExpr>,
        /// The columns of `outer` then those of `inner`, concatenated in that order, as
        /// [`LogicalPlan::Join`](vauban_binder::LogicalPlan::Join) does.
        schema: OutputSchema,
    },
    /// Pairs the rows of two inputs through a hash table built on one of them. Not
    /// produced yet.
    HashJoin {
        /// The input the hash table is built from.
        build: Box<PhysicalPlan>,
        /// The input probed against that table.
        probe: Box<PhysicalPlan>,
        /// How the rows are paired.
        kind: PhysicalJoinKind,
        /// The equality keys, the `build` side first in each pair.
        keys: Vec<(BoundExpr, BoundExpr)>,
        /// What is left of the join predicate once the equalities are taken out, applied
        /// to the paired rows; `None` when the predicate was equalities alone.
        residual: Option<BoundExpr>,
        /// The columns of the left operand of the bound `Join` then those of the right, in
        /// that order, whichever side was built from.
        schema: OutputSchema,
    },
    /// Groups the rows of `input` through a hash table, in whichever order they arrive.
    HashAggregate {
        /// The rows to group.
        input: Box<PhysicalPlan>,
        /// The grouping keys, empty for an aggregate over the whole input.
        group_by: Vec<BoundExpr>,
        /// The aggregates to compute.
        aggregates: Vec<AggregateCall>,
        /// The `group_by` keys followed by the `aggregates`, as
        /// [`LogicalPlan::Aggregate`](vauban_binder::LogicalPlan::Aggregate) orders them.
        schema: OutputSchema,
    },
    /// Groups the rows of an input already ordered on `group_by`, one group at a time.
    StreamAggregate {
        /// The rows to group, ordered on the grouping keys.
        input: Box<PhysicalPlan>,
        /// The grouping keys, empty for an aggregate over the whole input.
        group_by: Vec<BoundExpr>,
        /// The aggregates to compute.
        aggregates: Vec<AggregateCall>,
        /// The `group_by` keys followed by the `aggregates`, as for
        /// [`PhysicalPlan::HashAggregate`].
        schema: OutputSchema,
    },
    /// Orders the rows of `input`.
    ///
    /// Built for an `ORDER BY` whose keys the input does not deliver already; `sort.rs`
    /// says which ones it does.
    Sort {
        /// The rows to order.
        input: Box<PhysicalPlan>,
        /// The keys, most significant first.
        keys: Vec<SortKey>,
    },
    /// Orders the rows of `input` and keeps the first ones, without sorting the whole
    /// input.
    TopN {
        /// The rows to order.
        input: Box<PhysicalPlan>,
        /// The keys, most significant first.
        keys: Vec<SortKey>,
        /// How many rows to keep.
        top: BoundTop,
    },
    /// Removes the duplicate rows of its input.
    Distinct(Box<PhysicalPlan>),
    /// Evaluates the subqueries an expression of `input` holds, once per row for a
    /// correlated one. Not produced yet.
    SubqueryEval {
        /// The rows the expressions are evaluated over.
        input: Box<PhysicalPlan>,
        /// The plans of the subqueries, in the order the expressions name them.
        subplans: Vec<SubPlan>,
        /// The columns of `input`, plus one per subquery result.
        schema: OutputSchema,
    },
    /// `UNION [ALL]` of its operands, in the order they were written. Not produced yet.
    Union {
        /// The operands, at least two.
        inputs: Vec<PhysicalPlan>,
        /// True when `ALL` was written, which keeps the duplicate rows.
        all: bool,
        /// One column per column of the operands, as
        /// [`LogicalPlan::SetOp`](vauban_binder::LogicalPlan::SetOp) builds it.
        schema: OutputSchema,
    },
    /// `EXCEPT` of its operands, applied left to right. Not produced yet.
    Except {
        /// The operands, at least two, in the order they were written.
        inputs: Vec<PhysicalPlan>,
        /// The `all` flag the bound `SetOp` carries.
        all: bool,
        /// One column per column of the operands.
        schema: OutputSchema,
    },
    /// `INTERSECT` of its operands, applied left to right. Not produced yet.
    Intersect {
        /// The operands, at least two, in the order they were written.
        inputs: Vec<PhysicalPlan>,
        /// The `all` flag the bound `SetOp` carries.
        all: bool,
        /// One column per column of the operands.
        schema: OutputSchema,
    },
}

/// One subquery of a [`PhysicalPlan::SubqueryEval`]. Not produced yet.
#[derive(Debug, Clone)]
pub struct SubPlan {
    /// The plan of the subquery.
    pub plan: PhysicalPlan,
    /// True when the plan reads a column of the outer row, which makes it run once per
    /// row instead of once.
    pub correlated: bool,
}

/// How a physical join pairs the rows of its two inputs.
///
/// The five kinds of [`JoinKind`] plus the two a decorrelated subquery produces: an
/// `EXISTS` becomes [`PhysicalJoinKind::Semi`] and a `NOT EXISTS`
/// [`PhysicalJoinKind::AntiSemi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalJoinKind {
    /// Keeps the paired rows alone.
    Inner,
    /// Keeps the unpaired rows of the left input too, padded with `NULL`.
    Left,
    /// Keeps the unpaired rows of the right input too, padded with `NULL`.
    Right,
    /// Keeps the unpaired rows of both inputs, padded with `NULL` on the other side.
    Full,
    /// Pairs each row of the left input with each row of the right.
    Cross,
    /// Keeps one copy of each left row that has at least one match on the right.
    Semi,
    /// Keeps the left rows that have no match on the right.
    AntiSemi,
}

impl From<JoinKind> for PhysicalJoinKind {
    /// Maps the five bound kinds onto the five of the same name; `Semi` and `AntiSemi`
    /// have no bound counterpart and come from a decorrelated subquery alone.
    fn from(kind: JoinKind) -> Self {
        match kind {
            JoinKind::Inner => PhysicalJoinKind::Inner,
            JoinKind::Left => PhysicalJoinKind::Left,
            JoinKind::Right => PhysicalJoinKind::Right,
            JoinKind::Full => PhysicalJoinKind::Full,
            JoinKind::Cross => PhysicalJoinKind::Cross,
        }
    }
}

/// The keys a [`PhysicalPlan::IndexSeek`] serves, with bounds still to evaluate.
///
/// The shape of [`KeyRange`](vauban_storage::KeyRange), one level earlier: where the
/// storage range holds [`Value`](vauban_types::Value)s, this one holds the expressions
/// the executor evaluates into them before calling
/// [`Storage::seek`](vauban_storage::Storage::seek). A bound of `p` columns is a prefix of
/// the index key, with the same meaning as in `storage`.
#[derive(Debug, Clone)]
pub enum KeyRangeExpr {
    /// The keys whose first columns equal the given expressions.
    Point(Vec<BoundExpr>),
    /// The keys between two bounds, in index order.
    Between(Bound<Vec<BoundExpr>>, Bound<Vec<BoundExpr>>),
    /// The whole index.
    Full,
}

/// A `SELECT … INTO`, planned.
#[derive(Debug, Clone)]
pub struct PhysicalSelectInto {
    /// The table to create.
    pub def: TableDef,
    /// The rows to write into it.
    pub source: PhysicalPlan,
    /// True when the source must be read entirely before the first row is written.
    pub spool: bool,
}

/// An `INSERT`, planned. Not produced yet.
#[derive(Debug, Clone)]
pub struct PhysicalInsert {
    /// The table written into.
    pub table: TableId,
    /// The target columns, in the order the rows of `source` fill them.
    pub columns: Vec<ColumnBinding>,
    /// The rows to insert.
    pub source: PhysicalPlan,
    /// True when the rows must be read entirely before the first one is written: the
    /// Halloween protection the planner decides and the executor applies.
    pub spool: bool,
}

/// An `UPDATE`, planned. Not produced yet.
#[derive(Debug, Clone)]
pub struct PhysicalUpdate {
    /// The table written into.
    pub table: TableId,
    /// The rows to update, and how they are reached.
    pub input: PhysicalPlan,
    /// One `SET c = e` per entry, in the order they were written.
    pub assignments: Vec<(ColumnBinding, BoundExpr)>,
    /// True when the rows must be read entirely before the first one is written.
    pub spool: bool,
}

/// A `DELETE`, planned. Not produced yet.
#[derive(Debug, Clone)]
pub struct PhysicalDelete {
    /// The table rows are deleted from.
    pub table: TableId,
    /// The rows to delete, and how they are reached.
    pub input: PhysicalPlan,
    /// True when the rows must be read entirely before the first one is deleted.
    pub spool: bool,
}

/// The schema of a plan with no column, returned by [`PhysicalPlan::schema`] for
/// [`PhysicalPlan::OneRow`], as `LogicalPlan::schema` does for its own.
static EMPTY_SCHEMA: OutputSchema = OutputSchema {
    columns: Vec::new(),
};

impl PhysicalPlan {
    /// The columns this node produces.
    ///
    /// An operator that does not change the shape of its input (`Filter`, `Top`, `Sort`,
    /// `TopN`, `Distinct`) delegates to that input; the operators that build a shape carry
    /// their own `schema`; `OneRow` has no column
    /// (`tests/trivial.rs`, `schema_follows_the_node`).
    #[must_use]
    pub fn schema(&self) -> &OutputSchema {
        match self {
            PhysicalPlan::OneRow => &EMPTY_SCHEMA,
            PhysicalPlan::Values { schema, .. }
            | PhysicalPlan::TableScan { schema, .. }
            | PhysicalPlan::IndexSeek { schema, .. }
            | PhysicalPlan::Project { schema, .. }
            | PhysicalPlan::NestedLoopJoin { schema, .. }
            | PhysicalPlan::HashJoin { schema, .. }
            | PhysicalPlan::HashAggregate { schema, .. }
            | PhysicalPlan::StreamAggregate { schema, .. }
            | PhysicalPlan::SubqueryEval { schema, .. }
            | PhysicalPlan::Union { schema, .. }
            | PhysicalPlan::Except { schema, .. }
            | PhysicalPlan::Intersect { schema, .. } => schema,
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Top { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. } => input.schema(),
            PhysicalPlan::Distinct(input) => input.schema(),
        }
    }
}
