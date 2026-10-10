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

use vauban_binder::{
    AggregateCall, BoundExecute, BoundExpr, BoundProjection, BoundTop, ColumnBinding, DdlStatement,
    DeferredStatement, JoinKind, LockHints, OutputSchema, SortKey, SubqueryId, TxnStatement,
};
use vauban_catalog::{ColumnId, TableDef, TableId};
use vauban_storage::{Direction, IndexId};
use vauban_types::{SqlType, TypeInfo};

/// A statement, planned: what the `executor` runs.
///
/// One variant per variant of [`BoundStatement`](vauban_binder::BoundStatement).
/// Deliberately **not** `#[non_exhaustive]`: adding a variant must break the compilation
/// of the consumers rather than pass unnoticed.
///
/// The variants that carry no relational plan — DDL, `USE`, the transaction statements —
/// hold the payload the binder built, unchanged: planning has nothing to choose for them,
/// and `plan.rs` moves them across. A `DECLARE`, a `SET`, the condition of an `IF` or of a
/// `WHILE` and a `RETURN` carry, beside their expression, the plan of each subquery it
/// holds ([`SubPlan`], [`StatementExpr`]), since an expression evaluated outside a plan has
/// no operator to hang one on.
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
        /// One plan per subquery `value` holds, in the order
        /// [`SubqueryEval`](PhysicalPlan::SubqueryEval) lists them; empty when the value
        /// holds none.
        subplans: Vec<SubPlan>,
    },
    /// `SELECT @x = a, @y = b FROM t …`, the assignment of variables from the rows of a
    /// plan.
    ///
    /// The statement produces no result set: the rows of `input` are walked and each one
    /// overwrites the variables, so the last row walked leaves its values behind. The
    /// select list of that statement is the `Project` the binder detached from the query
    /// ([`BoundStatement::SelectAssign`](vauban_binder::BoundStatement::SelectAssign)),
    /// which is why the expressions travel beside the plan rather than on top of it.
    SelectAssign {
        /// The rows to assign from, under a [`PhysicalPlan::SubqueryEval`] when an
        /// assigned value holds a subquery.
        input: PhysicalPlan,
        /// One `@x = e` per entry, in written order: the name as the assignment wrote it,
        /// `@` included, and the value as the binder converted it.
        assignments: Vec<(String, BoundExpr)>,
    },
    /// `DECLARE @a int, @b varchar(10) = 'x'`, in the order the declarations were written.
    Declare(Vec<PhysicalDeclaration>),
    /// `IF p stmt [ELSE stmt]`, both branches planned.
    If {
        /// The condition, which satisfies
        /// [`BoundExpr::is_predicate`](vauban_binder::BoundExpr::is_predicate), with the
        /// plans of the subqueries it holds.
        condition: StatementExpr,
        /// The statement run when the condition is true, named as in the bound statement.
        then_: Box<PhysicalStatement>,
        /// The statement of the `ELSE`, `None` for an `IF` written without that clause.
        else_: Option<Box<PhysicalStatement>>,
    },
    /// `WHILE p stmt`, body planned.
    While {
        /// The condition, which satisfies
        /// [`BoundExpr::is_predicate`](vauban_binder::BoundExpr::is_predicate), with the
        /// plans of the subqueries it holds.
        condition: StatementExpr,
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
    Return(Option<StatementExpr>),
    /// `GOTO label`, the name carried across from the bound statement: there is nothing to
    /// plan, since where the jump lands is a position in a list of statements neither this
    /// crate nor the executor holds.
    Goto {
        /// The label named, as the `GOTO` wrote it.
        label: String,
    },
    /// `label:`, carried across from the bound statement. Runs nothing.
    Label {
        /// The name declared, as the label wrote it.
        name: String,
    },
    /// `PRINT e`.
    Print(BoundExpr),
    /// `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`.
    Transaction(TxnStatement),
    /// `EXECUTE`, carried across from the bound statement.
    Execute(BoundExecute),
    /// `SET <option>`, carried across from the bound statement: there is nothing to plan,
    /// the session applies it to its options when it runs.
    SetOption(Box<vauban_binder::SetOptionStatement>),
    /// A statement of a branch or of a loop body left unbound, carried across from the
    /// bound statement: there is nothing to plan until it is bound, which happens when the
    /// control of flow reaches it.
    Deferred(Box<DeferredStatement>),
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
        /// `storage` produces, then the row-identifier bindings ([`row_id_binding`]) the
        /// plan asked for, which have no entry in `schema`.
        columns: Vec<ColumnBinding>,
        /// The name the rest of the query refers to this source by.
        alias: String,
        /// The columns this node produces, in the same order as `columns`, row identifiers
        /// left out.
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
        /// The columns read, then the row-identifier bindings, as for a
        /// [`PhysicalPlan::TableScan`].
        columns: Vec<ColumnBinding>,
        /// Forward or backward relative to the index order.
        direction: Direction,
        /// The columns this node produces, in the same order as `columns`, row identifiers
        /// left out.
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
        /// The plans of the subqueries, one per [`SubPlan::id`], found by that identity.
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

/// One variable of a [`PhysicalStatement::Declare`], with the plans of the subqueries its
/// initial value holds.
///
/// The three fields the binder wrote are carried across unchanged; `subplans` is what
/// planning adds.
#[derive(Debug, Clone)]
pub struct PhysicalDeclaration {
    /// The name, `@` included, as written.
    pub name: String,
    /// The declared type, nullability and collation included.
    pub ty: TypeInfo,
    /// The initial value, already converted to `ty`; `None` for a `DECLARE` written
    /// without `=`, which is the `NULL` a variable starts at.
    pub value: Option<BoundExpr>,
    /// One plan per subquery `value` holds, in the order
    /// [`SubqueryEval`](PhysicalPlan::SubqueryEval) lists them; empty for a value that
    /// holds none, and for a declaration written without `=`.
    pub subplans: Vec<SubPlan>,
}

/// An expression a statement evaluates outside any plan — the condition of an `IF` or of a
/// `WHILE`, the value of a `RETURN` — and the plans of the subqueries it holds.
///
/// The bound expression is carried across unchanged; `subplans` is what planning adds, one
/// plan per subquery written directly in `expr`, in the order
/// [`SubqueryEval`](PhysicalPlan::SubqueryEval) lists them. A subquery nested in one of
/// those is planned inside the plan of its parent, as it is in a query.
#[derive(Debug, Clone)]
pub struct StatementExpr {
    /// The expression, as the binder bound it.
    pub expr: BoundExpr,
    /// One plan per subquery `expr` holds, empty for `IF 1 = 1`
    /// (`tests/subquery.rs`, `an_if_condition_without_a_subquery_carries_no_subplan`).
    pub subplans: Vec<SubPlan>,
}

impl From<BoundExpr> for StatementExpr {
    /// An expression planned with no subquery plan, which is what an expression that holds
    /// no subquery is planned into.
    fn from(expr: BoundExpr) -> Self {
        Self {
            expr,
            subplans: Vec::new(),
        }
    }
}

/// One subquery of a [`PhysicalPlan::SubqueryEval`], or of an expression a statement
/// evaluates outside any plan ([`PhysicalDeclaration`], [`StatementExpr`],
/// [`PhysicalStatement::SetVariable`]).
#[derive(Debug, Clone)]
pub struct SubPlan {
    /// The identity of the bound node this plan was built from; the executor finds the
    /// plan of a node by it.
    pub id: SubqueryId,
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

/// Name of a row-identifier binding. No scope of the binder holds it, so a query cannot
/// name the column.
pub const ROW_ID_NAME: &str = "%rowid";

/// The binding that asks a [`PhysicalPlan::TableScan`] or a [`PhysicalPlan::IndexSeek`] of
/// `table` to emit, after the columns of its `schema`, the
/// [`RowId`](vauban_storage::RowId) of each row it reads.
///
/// # The row identifier in the row stream
///
/// A plan asks for row identifiers so that a write statement can reach, from a row of its
/// input, the row of its target table that produced it. The identifier is an invisible
/// column: it has no entry in any `schema`, and no expression of the plan reads it, so the
/// positions the binder resolved are left as they were. Each row carries the identifiers
/// after its visible values, as a tail:
///
/// - a scan or a seek appends one value per row-identifier binding of its `columns`;
/// - `Filter`, `Top`, `Sort`, `TopN` and `SubqueryEval` hand the row over, tail included;
/// - `Project` appends the tail of its input after the values it computes;
/// - a join puts the visible values of both sides first, then the tail of the left side,
///   then that of the right; a side padded with `NULL` has a `NULL` per identifier;
/// - `Distinct`, the aggregates and the set operators build rows of their own, without
///   a tail.
///
/// [`PhysicalPlan::row_id_tables`] lists the table each value of the tail belongs to, in
/// that order (`tests/row_id.rs`, `the_tail_follows_the_join_order`).
///
/// The binding is recognised by its `column`, which holds the table, negated and moved one
/// down so that it stays below 0, the identifier the binder gives a computed column, and
/// below the identifiers of the columns of a table, which start at 1 (`tests/row_id.rs`,
/// `the_binding_names_its_table`). Its `index` is 0, a position inside each row of a table
/// with a column: a reader that picks the values of `columns` by `index` without telling
/// this binding apart still reads inside the row.
///
/// `None` for a table whose identifier does not fit in a column identifier.
#[must_use]
pub fn row_id_binding(table: TableId) -> Option<ColumnBinding> {
    let table = i32::try_from(table.0).ok()?;
    Some(ColumnBinding {
        column: ColumnId(-table - 1),
        index: 0,
        name: ROW_ID_NAME.to_owned(),
        ty: TypeInfo::new(SqlType::BigInt, false),
        depth: 0,
    })
}

/// The table a row-identifier binding names, `None` for the binding of a column
/// (`tests/row_id.rs`, `the_binding_names_its_table`).
#[must_use]
pub fn row_id_table(binding: &ColumnBinding) -> Option<TableId> {
    if binding.column.0 >= 0 {
        return None;
    }
    u32::try_from(-(i64::from(binding.column.0) + 1))
        .ok()
        .map(TableId)
}

impl PhysicalPlan {
    /// The table of each row identifier the rows of this node carry after their visible
    /// values, in the order of the tail ([`row_id_binding`]). Empty for a plan nothing
    /// asked a row identifier of.
    #[must_use]
    pub fn row_id_tables(&self) -> Vec<TableId> {
        let mut tables = Vec::new();
        self.visit_row_ids(&mut |table| tables.push(table));
        tables
    }

    /// How many row identifiers the rows of this node carry after their visible values:
    /// the length of [`PhysicalPlan::row_id_tables`], without building it.
    #[must_use]
    pub fn row_id_count(&self) -> usize {
        let mut count = 0;
        self.visit_row_ids(&mut |_| count += 1);
        count
    }

    /// Calls `f` with the table of each value of the tail, in the order of the tail.
    fn visit_row_ids(&self, f: &mut impl FnMut(TableId)) {
        match self {
            PhysicalPlan::TableScan { columns, .. } | PhysicalPlan::IndexSeek { columns, .. } => {
                columns.iter().filter_map(row_id_table).for_each(f);
            }
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Project { input, .. }
            | PhysicalPlan::Top { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. }
            | PhysicalPlan::SubqueryEval { input, .. } => input.visit_row_ids(f),
            PhysicalPlan::NestedLoopJoin {
                outer, inner, kind, ..
            } => {
                outer.visit_row_ids(f);
                if !matches!(kind, PhysicalJoinKind::Semi | PhysicalJoinKind::AntiSemi) {
                    inner.visit_row_ids(f);
                }
            }
            PhysicalPlan::HashJoin { build, probe, .. } => {
                probe.visit_row_ids(f);
                build.visit_row_ids(f);
            }
            PhysicalPlan::OneRow
            | PhysicalPlan::Values { .. }
            | PhysicalPlan::HashAggregate { .. }
            | PhysicalPlan::StreamAggregate { .. }
            | PhysicalPlan::Distinct(_)
            | PhysicalPlan::Union { .. }
            | PhysicalPlan::Except { .. }
            | PhysicalPlan::Intersect { .. } => {}
        }
    }
}
