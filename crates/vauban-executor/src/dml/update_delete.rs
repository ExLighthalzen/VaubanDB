//! `UPDATE` and `DELETE`: the rows the input plan produces name the target rows to write,
//! through the row identifier each of them carries after its visible values.
//!
//! # Which row is written
//!
//! The input of the statement is an operator tree like the one of a query: a scan or a seek
//! of the target, under a `Filter`, a join with the sources of a `FROM`, a semi-join of an
//! `IN (SELECT …)` or of an `EXISTS` (`tests/dml_input.rs`). One read of the target in that
//! tree carries the row-identifier binding of its table, so each row of the input ends with
//! the [`RowId`] of the target row it was built from ([`TargetSlot`]). The planner marks the
//! read of a statement over one table, the binder the source the target of a joined
//! statement names; an input whose plan carries no mark gets one here, on each read of the
//! target through the nodes that keep the identifier ([`mark_target_reads`]).
//!
//! A target row the input reaches several times is written once, with the values of the
//! first input row that reached it, and counted once
//! (`tests/dml_input.rs`, `a_target_row_reached_twice_is_written_once`). Which input row
//! comes first depends on the plan: the value an `UPDATE … FROM` writes when a row has
//! several partners is not specified. A row whose identifier is `NULL`, the target side of an
//! outer join padded, is not written.
//!
//! # The spool
//!
//! When the planner set the `spool` flag of the statement, the input is read under a
//! [`Spool`] and every value of the `SET` list is computed before the first write, so that
//! no write changes what the input or a subquery of a value reads
//! (`tests/dml_input.rs`, `an_in_over_the_target_writes_each_row_once`). Without the flag,
//! each row is written as soon as the input hands it over.
//!
//! # A row another transaction changed
//!
//! Each row is locked for writing, then its version is checked against the snapshot of the
//! statement (`TransactionManager::check_write_conflict`). A row changed by a transaction
//! that committed after the snapshot was taken is read again, at `READ COMMITTED`: the
//! input is run once more through a snapshot that also shows the writer of that row
//! ([`Requalifier`]), and the row is written only when the input still reaches it, with the
//! values of the `SET` list computed again over the input row of that run. A row that no
//! longer qualifies is left as it is and not counted (`tests/dml_input.rs`,
//! `a_row_changed_out_of_the_where_is_not_updated_after_the_wait`). One run serves every
//! row the same writer changed. At `SNAPSHOT`, the same change is the conflict 3960.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use vauban_binder::{BoundExpr, BoundObjectExpression, ColumnBinding, TargetExpressions};
use vauban_catalog::{CatalogSnapshot, TableMeta};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::{
    PhysicalDelete, PhysicalJoinKind, PhysicalPlan, PhysicalUpdate, SubPlan, row_id_binding,
    row_id_table,
};
use vauban_storage::{IndexId, RowId, Snapshot, TableId, TxnId};
use vauban_txn::WriteDecision;
use vauban_types::Value;

use crate::context::ExecContext;
use crate::dml::assign::assign_value;
use crate::dml::fk_check::WriteChecks;
use crate::dml::insert::{by_ordinal, fill_computed};
use crate::errors::at;
use crate::expr::eval_expr;
use crate::locking;
use crate::operator::{Operator, build_operator};
use crate::ops::spool::Spool;
use crate::row::{ExecOutcome, Row};

/// Runs one `UPDATE`.
pub(crate) fn execute_update<'a>(
    stmt: &PhysicalUpdate,
    ctx: &mut ExecContext<'a>,
) -> SqlResult<ExecOutcome> {
    write(
        stmt.table,
        &stmt.input,
        stmt.spool,
        Some((&stmt.assignments, &stmt.target)),
        ctx,
    )
}

/// Runs one `DELETE`.
pub(crate) fn execute_delete<'a>(
    stmt: &PhysicalDelete,
    ctx: &mut ExecContext<'a>,
) -> SqlResult<ExecOutcome> {
    write(stmt.table, &stmt.input, stmt.spool, None, ctx)
}

/// What the statement writes and where: the parts of an `UPDATE` or a `DELETE` read once.
struct Target<'s> {
    table: TableId,
    meta: &'s TableMeta,
    cat_snap: &'s CatalogSnapshot,
    /// `database.schema.table`, for the messages that name the table.
    name: String,
    /// `UPDATE` or `DELETE`, for the messages that name the statement.
    verb: &'static str,
    /// The `SET` list; `None` for a `DELETE`.
    assignments: Option<&'s [(ColumnBinding, BoundExpr)]>,
    /// The bound expression of each computed column, by ordinal; empty for a `DELETE`.
    computed: Vec<Option<&'s BoundObjectExpression>>,
    /// The constraints each written row is checked against, resolved once.
    checks: WriteChecks,
}

/// The body of both statements; `update` is the `SET` list and the bound expressions of
/// the table for an `UPDATE`, `None` for a `DELETE`.
fn write<'a>(
    table: TableId,
    input: &PhysicalPlan,
    spool: bool,
    update: Option<(&[(ColumnBinding, BoundExpr)], &TargetExpressions)>,
    ctx: &mut ExecContext<'a>,
) -> SqlResult<ExecOutcome> {
    let assignments = update.map(|(assignments, _)| assignments);
    let verb = if assignments.is_some() {
        "UPDATE"
    } else {
        "DELETE"
    };
    let handle = ctx.handle()?;
    let catalog = ctx.catalog()?;
    let cat_snap = catalog.snapshot(handle);
    let meta = cat_snap
        .table_by_storage(table)
        .ok_or_else(|| bug(&format!("{verb}: table not found in the catalogue")))?;
    let name = format!(
        "{}.{}.{}",
        cat_snap
            .database_by_id(meta.database)
            .map_or_else(|| "?".to_owned(), |db| db.name.clone()),
        meta.schema,
        meta.name
    );
    let (checks, computed): (&[BoundObjectExpression], &[BoundObjectExpression]) = match update {
        Some((_, expressions)) => (&expressions.checks, &expressions.computed),
        None => (&[], &[]),
    };
    let target = Target {
        table,
        meta,
        cat_snap: &cat_snap,
        name,
        verb,
        assignments,
        computed: by_ordinal(computed, meta.columns.len()),
        checks: WriteChecks::resolve(meta, &cat_snap, checks, verb, true, ctx)?,
    };

    let (source, set_subplans) = split_assignment_subqueries(input, assignments.is_some());
    let (source, slot) = TargetSlot::locate(source, table, ctx)?;

    let mark = (!set_subplans.is_empty()).then(|| ctx.open_statement_subplans(set_subplans));
    let mut root = Spool::wrap_if(spool, build_operator(&source)?);
    let outcome = drive(&mut *root, &source, slot, spool, &target, ctx);
    root.close();
    if let Some(mark) = mark {
        ctx.unwind_to(mark);
    }
    let count = match outcome? {
        Some(count) => count,
        None => return Ok(ExecOutcome::Cancelled),
    };
    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = i64::try_from(count).unwrap_or(i64::MAX);
    }
    Ok(ExecOutcome::NoRows)
}

/// Reads the input and writes the rows it names; the number of rows written, or `None` when
/// the statement was cancelled.
///
/// The token is read once the input is open, which is when a [`Spool`] has read everything
/// it will hand out: a cancelled spool hands out nothing, and the statement then answers
/// cancelled rather than zero rows (`tests/dml_input.rs`,
/// `a_cancelled_spool_cancels_the_statement`).
fn drive<'a>(
    root: &mut (dyn Operator<'a> + 'a),
    source: &PhysicalPlan,
    slot: TargetSlot,
    spool: bool,
    target: &Target<'_>,
    ctx: &mut ExecContext<'a>,
) -> SqlResult<Option<u64>> {
    root.open(ctx)?;
    if ctx.cancelled() {
        return Ok(None);
    }
    let mut seen = HashSet::new();
    let mut pending = Vec::new();
    let mut requalifier = Requalifier::default();
    let mut count: u64 = 0;
    while let Some(row) = root.next(ctx)? {
        if ctx.cancelled() {
            return Ok(None);
        }
        let Some(id) = slot.row_id(&row)? else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        let values = assigned_values(target, &row, ctx)?;
        if spool {
            pending.push((id, values));
        } else if write_row(id, values, source, slot, target, &mut requalifier, ctx)? {
            count += 1;
        }
    }
    for (id, values) in pending {
        if ctx.cancelled() {
            return Ok(None);
        }
        if write_row(id, values, source, slot, target, &mut requalifier, ctx)? {
            count += 1;
        }
    }
    if ctx.cancelled() {
        return Ok(None);
    }
    Ok(Some(count))
}

/// The input to read, and the plans of the subqueries of the `SET` list: an `UPDATE` whose
/// values hold a subquery has its input under a `SubqueryEval` that carries those plans, and
/// nothing else puts a `SubqueryEval` at the root of the input of a write statement. The
/// plans are installed for the whole statement instead, so that the input can be read under
/// a spool and the values computed after it.
fn split_assignment_subqueries(input: &PhysicalPlan, update: bool) -> (&PhysicalPlan, &[SubPlan]) {
    match input {
        PhysicalPlan::SubqueryEval {
            input, subplans, ..
        } if update => (input, subplans),
        _ => (input, &[]),
    }
}

/// The values the `SET` list assigns, computed over one row of the input and converted to
/// their columns, in the order of the list; `None` for a `DELETE`.
///
/// # Errors
///
/// What evaluating a value raises; what [`assign_value`] raises, on the line of the value.
fn assigned_values(
    target: &Target<'_>,
    row: &Row,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<Vec<Value>>> {
    let Some(assignments) = target.assignments else {
        return Ok(None);
    };
    let mut values = Vec::with_capacity(assignments.len());
    for (binding, expr) in assignments {
        let value = eval_expr(expr, Some(row), ctx)?;
        let column = target.meta.columns.get(binding.index).ok_or_else(|| {
            bug(&format!(
                "UPDATE: column {} past the columns of the table",
                binding.index
            ))
        })?;
        values.push(
            assign_value(value, &expr.ty, column, &target.name, "UPDATE")
                .map_err(|e| at(e, expr.line))?,
        );
    }
    Ok(Some(values))
}

/// Locks and writes one target row; whether it was written.
///
/// `values` are the values of the `SET` list computed over the input row that reached the
/// row; a row read again after a wait has them computed again over the input row of the
/// new run (module documentation).
///
/// # Errors
///
/// What the lock raises (1222, 1205); 3960 at `SNAPSHOT` for a row changed since the
/// snapshot; the constraint errors of the new row (515, 547, 2627, 2601); the internal error
/// 50000 for a row the storage no longer knows.
fn write_row<'a>(
    id: RowId,
    values: Option<Vec<Value>>,
    source: &PhysicalPlan,
    slot: TargetSlot,
    target: &Target<'_>,
    requalifier: &mut Requalifier,
    ctx: &mut ExecContext<'a>,
) -> SqlResult<bool> {
    let storage = ctx.storage()?;
    let handle = ctx.handle()?;
    let txn = ctx
        .txn
        .ok_or_else(|| bug(&format!("{} needs a transaction manager", target.verb)))?;
    locking::write_lock(ctx, target.table, id)?;
    let (old_row, values) = match txn.check_write_conflict(handle, target.table, id)? {
        WriteDecision::Proceed => {
            let (_, row) = storage.latest_version(target.table, id)?.ok_or_else(|| {
                bug(&format!(
                    "{}: the row vanished between the read and the write",
                    target.verb
                ))
            })?;
            (row.0, values)
        }
        WriteDecision::Reread(id) => {
            let Some((row, input_row)) =
                requalifier.requalify(source, slot, target.table, id, ctx)?
            else {
                return Ok(false);
            };
            let values = assigned_values(target, &input_row, ctx)?;
            (row, values)
        }
        WriteDecision::Conflict => {
            return Err(SqlError::snapshot_update_conflict(&target.name, ""));
        }
    };
    match (target.assignments, values) {
        (Some(assignments), Some(values)) => {
            let mut new_row = old_row.clone();
            for ((binding, _), value) in assignments.iter().zip(values) {
                let slot = new_row.get_mut(binding.index).ok_or_else(|| {
                    bug("UPDATE: an assigned column past the end of the stored row")
                })?;
                *slot = value;
            }
            fill_computed(
                &mut new_row,
                &target.computed,
                &target.meta.columns,
                &target.name,
                "UPDATE",
                ctx,
            )?;
            if target.checks.referenced_columns_changed(&old_row, &new_row) {
                target.checks.check_no_referencing_rows(&old_row, ctx)?;
            }
            target.checks.check_row(&new_row, ctx)?;
            let stored = vauban_storage::Row(new_row);
            storage
                .update(handle.id, target.table, id, &stored)
                .map_err(|err| {
                    crate::dml::constraints::translate_unique(
                        err,
                        target.meta,
                        target.cat_snap,
                        &stored.0,
                        "UPDATE",
                    )
                })?;
        }
        _ => {
            target.checks.check_no_referencing_rows(&old_row, ctx)?;
            storage.delete(handle.id, target.table, id)?;
        }
    }
    Ok(true)
}

/// Where the identifier of the target row sits in a row of the input: the position of its
/// value in the tail of identifiers the row ends with
/// ([`PhysicalPlan::row_id_tables`]), and the length of that tail.
#[derive(Clone, Copy, Debug)]
struct TargetSlot {
    /// How many identifiers each row ends with.
    tail: usize,
    /// The position of the target's among them.
    index: usize,
}

impl TargetSlot {
    /// The plan to run, marked here when it carries no identifier of `table`, and where the
    /// identifier of the target sits in its rows.
    ///
    /// # Errors
    ///
    /// The internal error 50000 when the input reads no row of `table`, or carries the
    /// identifier of `table` twice: which of two reads of the table is the target is then not
    /// said by the plan.
    fn locate(
        plan: &PhysicalPlan,
        table: TableId,
        ctx: &ExecContext<'_>,
    ) -> SqlResult<(PhysicalPlan, Self)> {
        let mut plan = plan.clone();
        if !plan.row_id_tables().contains(&table) {
            let indexes: Vec<IndexId> = ctx
                .storage()?
                .indexes(table)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            mark_target_reads(&mut plan, table, &indexes)?;
        }
        let tables = plan.row_id_tables();
        let mut positions = tables
            .iter()
            .enumerate()
            .filter(|(_, read)| **read == table)
            .map(|(index, _)| index);
        let slot = match (positions.next(), positions.next()) {
            (Some(index), None) => Self {
                tail: tables.len(),
                index,
            },
            (None, _) => {
                return Err(bug(
                    "UPDATE/DELETE: the input plan reads no row of the target table",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(bug(
                    "UPDATE/DELETE: the input plan reads the target table twice and does not \
                     say which read is the target",
                ));
            }
        };
        Ok((plan, slot))
    }

    /// The identifier of the target row `row` was built from; `None` for the `NULL` of a
    /// target side an outer join padded.
    ///
    /// # Errors
    ///
    /// The internal error 50000 for a row narrower than its tail, or a tail value that is
    /// not an identifier: the width of the row is not the one the plan announced.
    fn row_id(self, row: &Row) -> SqlResult<Option<RowId>> {
        let start = row
            .len()
            .checked_sub(self.tail)
            .ok_or_else(|| bug("UPDATE/DELETE: an input row narrower than its identifiers"))?;
        match row.get(start + self.index) {
            Some(Value::I64(value)) => Ok(Some(RowId(u64::from_ne_bytes(value.to_ne_bytes())))),
            Some(Value::Null) => Ok(None),
            _ => Err(bug(
                "UPDATE/DELETE: the value at the place of the row identifier is not one",
            )),
        }
    }
}

/// Puts the row-identifier binding of `table` on each scan of `table`, and each seek of one
/// of its `indexes`, that `plan` hands the rows of to its root: through the nodes that keep
/// the identifiers of their input, the inner side of a semi-join or an anti-semi-join left
/// out. A plan built by the planner arrives marked; this serves a plan built by hand.
///
/// # Errors
///
/// The internal error 50000 for a table whose identifier does not fit in a column
/// identifier.
fn mark_target_reads(
    plan: &mut PhysicalPlan,
    table: TableId,
    indexes: &[IndexId],
) -> SqlResult<()> {
    let columns = match plan {
        PhysicalPlan::TableScan {
            table: read,
            columns,
            ..
        } if *read == table => columns,
        PhysicalPlan::IndexSeek { index, columns, .. } if indexes.contains(index) => columns,
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => {
            return mark_target_reads(input, table, indexes);
        }
        PhysicalPlan::NestedLoopJoin {
            outer, inner, kind, ..
        } => {
            mark_target_reads(outer, table, indexes)?;
            if !matches!(kind, PhysicalJoinKind::Semi | PhysicalJoinKind::AntiSemi) {
                mark_target_reads(inner, table, indexes)?;
            }
            return Ok(());
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            mark_target_reads(probe, table, indexes)?;
            return mark_target_reads(build, table, indexes);
        }
        _ => return Ok(()),
    };
    if columns
        .iter()
        .all(|binding| row_id_table(binding) != Some(table))
    {
        let binding = row_id_binding(table).ok_or_else(|| {
            bug("UPDATE/DELETE: the table identifier does not fit in a column identifier")
        })?;
        columns.push(binding);
    }
    Ok(())
}

/// The runs of the input made for the rows read again after a wait, one per transaction
/// whose change caused the wait.
#[derive(Default)]
struct Requalifier {
    /// For each writer: the snapshot that shows its change, and the first input row of the
    /// run made through it for each target row that run reached.
    runs: HashMap<TxnId, (Snapshot, HashMap<RowId, Row>)>,
}

impl Requalifier {
    /// The stored row `id` as the transaction that last wrote it left it, and the input row
    /// that reaches it now; `None` when the input no longer reaches it, or when that
    /// transaction deleted it.
    ///
    /// # Errors
    ///
    /// What running the input raises.
    fn requalify<'a>(
        &mut self,
        source: &PhysicalPlan,
        slot: TargetSlot,
        table: TableId,
        id: RowId,
        ctx: &mut ExecContext<'a>,
    ) -> SqlResult<Option<(Vec<Value>, Row)>> {
        let storage = ctx.storage()?;
        let Some((writer, _)) = storage.latest_version(table, id)? else {
            return Ok(None);
        };
        let (snap, rows) = match self.runs.entry(writer) {
            Entry::Occupied(run) => run.into_mut(),
            Entry::Vacant(slot_of_writer) => {
                let snap = showing(ctx.snapshot()?, writer);
                let rows = rerun(source, slot, &snap, ctx)?;
                slot_of_writer.insert((snap, rows))
            }
        };
        let Some(input_row) = rows.get(&id) else {
            return Ok(None);
        };
        Ok(storage
            .get(snap, table, id)?
            .map(|row| (row.0, input_row.clone())))
    }
}

/// `base`, with the change of `writer` shown: `writer` leaves the transactions it hides,
/// and the ones that started between `base` and `writer` join them.
fn showing(base: &Snapshot, writer: TxnId) -> Snapshot {
    let mut snap = base.clone();
    if writer >= snap.xmax {
        snap.active.extend((snap.xmax.0..writer.0).map(TxnId));
        snap.xmax = TxnId(writer.0.saturating_add(1));
    }
    snap.active.retain(|active| *active != writer);
    snap.active.sort_unstable();
    snap
}

/// Runs `source` through `snap` in a context of its own, and answers the first input row
/// that reaches each target row.
///
/// # Errors
///
/// What opening or reading the input raises.
fn rerun(
    source: &PhysicalPlan,
    slot: TargetSlot,
    snap: &Snapshot,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<HashMap<RowId, Row>> {
    let mut run = ExecContext::scalar(ctx.eval, ctx.options);
    run.storage = ctx.storage;
    run.txn = ctx.txn;
    run.catalog = ctx.catalog;
    run.handle = ctx.handle;
    run.cancel = ctx.cancel;
    run.snap = Some(snap);
    run.session = ctx.session.as_deref_mut();
    let mut root = build_operator(source)?;
    let rows = read_by_target(&mut *root, slot, &mut run);
    root.close();
    rows
}

/// The first row of `root` for each target row it reaches.
fn read_by_target<'r>(
    root: &mut (dyn Operator<'r> + 'r),
    slot: TargetSlot,
    ctx: &mut ExecContext<'r>,
) -> SqlResult<HashMap<RowId, Row>> {
    root.open(ctx)?;
    let mut rows = HashMap::new();
    while let Some(row) = root.next(ctx)? {
        if let Some(id) = slot.row_id(&row)? {
            rows.entry(id).or_insert(row);
        }
    }
    Ok(rows)
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests;
