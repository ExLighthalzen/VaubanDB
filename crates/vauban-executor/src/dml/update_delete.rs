use std::cmp::Ordering;
use std::ops::Bound;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::{KeyRangeExpr, PhysicalDelete, PhysicalPlan, PhysicalUpdate};
use vauban_storage::{IndexId, KeyRange, RowId, Storage, TableId};
use vauban_txn::WriteDecision;
use vauban_types::{TypeInfo, Value, compare, convert};

use crate::context::ExecContext;
use crate::dml::assign::assign_value;
use crate::errors::at;
use crate::expr::eval_expr;
use crate::locking;
use crate::row::{ExecOutcome, Row};

/// Runs one `UPDATE`.
pub(crate) fn execute_update(
    stmt: &PhysicalUpdate,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let storage = ctx.storage()?;
    let handle = ctx.handle()?;
    let txn_mgr = ctx
        .txn
        .ok_or_else(|| bug("UPDATE needs a transaction manager"))?;
    let catalog = ctx.catalog()?;
    let cat_snap = catalog.snapshot(handle);
    let meta = cat_snap
        .table_by_storage(stmt.table)
        .ok_or_else(|| bug("UPDATE: table not found in the catalogue"))?;

    let rows = collect_input_rows(&stmt.input, stmt.table, ctx)?;
    let materialized = rows;

    let mut count: u64 = 0;
    for (row_id, old_row) in &materialized {
        let mut new_row = old_row.clone();
        for (binding, expr) in &stmt.assignments {
            let expr_value = eval_expr(expr, Some(old_row), ctx)?;
            let col_meta = &meta.columns[binding.index];
            new_row[binding.index] = assign_value(expr_value, &expr.ty, col_meta)?;
        }

        locking::write_lock(ctx, stmt.table, *row_id)?;
        let decision = txn_mgr.check_write_conflict(handle, stmt.table, *row_id)?;
        match decision {
            WriteDecision::Proceed => {
                let stored = vauban_storage::Row(new_row);
                storage
                    .update(handle.id, stmt.table, *row_id, &stored)
                    .map_err(|err| {
                        crate::dml::constraints::translate_unique(
                            err, meta, &cat_snap, &stored.0, "UPDATE",
                        )
                    })?;
                count += 1;
            }
            WriteDecision::Reread(id) => {
                let (_, latest) = storage
                    .latest_version(stmt.table, id)?
                    .ok_or_else(|| bug("UPDATE: row vanished between the read and the re-read"))?;
                let mut re_row = latest.0;
                for (binding, expr) in &stmt.assignments {
                    let expr_value = eval_expr(expr, Some(&re_row), ctx)?;
                    let col_meta = &meta.columns[binding.index];
                    re_row[binding.index] = assign_value(expr_value, &expr.ty, col_meta)?;
                }
                let stored = vauban_storage::Row(re_row);
                storage
                    .update(handle.id, stmt.table, id, &stored)
                    .map_err(|err| {
                        crate::dml::constraints::translate_unique(
                            err, meta, &cat_snap, &stored.0, "UPDATE",
                        )
                    })?;
                count += 1;
            }
            WriteDecision::Conflict => {
                let tbl = format!(
                    "{}.{}.{}",
                    cat_snap
                        .database_by_id(meta.database)
                        .map_or_else(|| "?", |db| db.name.as_str()),
                    meta.schema,
                    meta.name
                );
                return Err(SqlError::snapshot_update_conflict(&tbl, ""));
            }
        }
    }

    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = count as i64;
    }
    Ok(ExecOutcome::NoRows)
}

/// Runs one `DELETE`.
pub(crate) fn execute_delete(
    stmt: &PhysicalDelete,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let storage = ctx.storage()?;
    let handle = ctx.handle()?;
    let txn_mgr = ctx
        .txn
        .ok_or_else(|| bug("DELETE needs a transaction manager"))?;
    let catalog = ctx.catalog()?;
    let cat_snap = catalog.snapshot(handle);
    let meta = cat_snap
        .table_by_storage(stmt.table)
        .ok_or_else(|| bug("DELETE: table not found in the catalogue"))?;

    let rows = collect_input_rows(&stmt.input, stmt.table, ctx)?;
    let materialized = rows;

    let mut count: u64 = 0;
    for (row_id, _) in &materialized {
        locking::write_lock(ctx, stmt.table, *row_id)?;
        let decision = txn_mgr.check_write_conflict(handle, stmt.table, *row_id)?;
        match decision {
            WriteDecision::Proceed => {
                storage.delete(handle.id, stmt.table, *row_id)?;
                count += 1;
            }
            WriteDecision::Reread(_) => {
                storage.delete(handle.id, stmt.table, *row_id)?;
                count += 1;
            }
            WriteDecision::Conflict => {
                let tbl = format!(
                    "{}.{}.{}",
                    cat_snap
                        .database_by_id(meta.database)
                        .map_or_else(|| "?", |db| db.name.as_str()),
                    meta.schema,
                    meta.name
                );
                return Err(SqlError::snapshot_update_conflict(&tbl, ""));
            }
        }
    }

    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = count as i64;
    }
    Ok(ExecOutcome::NoRows)
}

/// Collects `(RowId, full_storage_row)` pairs from `plan`, which locates rows of
/// `table`. The plan is the `input` of an `UPDATE` or `DELETE`: a `TableScan`, an
/// `IndexSeek`, or a `Filter` over either.
fn collect_input_rows(
    plan: &PhysicalPlan,
    table: TableId,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Vec<(RowId, Vec<Value>)>> {
    match plan {
        PhysicalPlan::TableScan {
            table: t,
            columns: _,
            alias: _,
            schema: _,
            hints: _,
        } => {
            if *t != table {
                return Err(bug("UPDATE/DELETE: TableScan reads a different table"));
            }
            let storage = ctx.storage()?;
            let snap = ctx.snapshot()?;
            let iter = storage.scan(snap, *t)?;
            let mut result = Vec::new();
            for item in iter {
                let (row_id, storage_row) = item?;
                result.push((row_id, storage_row.0));
            }
            Ok(result)
        }
        PhysicalPlan::Filter { input, predicate } => {
            let base = collect_input_rows(input, table, ctx)?;
            let (leaf_cols, _) = leaf_columns(input);
            let mut result = Vec::new();
            for (row_id, full_row) in base {
                let projected: Row = leaf_cols
                    .iter()
                    .map(|cb| full_row[cb.index].clone())
                    .collect();
                let value = eval_expr(predicate, Some(&projected), ctx)?;
                if matches!(value, Value::Bit(true)) {
                    result.push((row_id, full_row));
                }
            }
            Ok(result)
        }
        PhysicalPlan::IndexSeek {
            index,
            range,
            columns: _,
            direction,
            schema: _,
            hints: _,
        } => {
            let storage = ctx.storage()?;
            let snap = ctx.snapshot()?;
            let key = key_columns_of(storage, *index)?;
            let Some(seek_range) = evaluate_seek_range(range, &key, ctx)? else {
                return Ok(Vec::new());
            };
            let iter = storage.seek(snap, *index, &seek_range, *direction)?;
            let mut result = Vec::new();
            for item in iter {
                let (row_id, storage_row) = item?;
                result.push((row_id, storage_row.0));
            }
            Ok(result)
        }
        _ => Err(bug("UPDATE/DELETE: unsupported input plan variant")),
    }
}

/// The columns the leaf scan or seek of `plan` produces, and the table it reads.
fn leaf_columns(plan: &PhysicalPlan) -> (Vec<vauban_binder::ColumnBinding>, TableId) {
    match plan {
        PhysicalPlan::TableScan { table, columns, .. } => (columns.clone(), *table),
        PhysicalPlan::IndexSeek { columns, .. } => (columns.clone(), TableId(u32::MAX)),
        PhysicalPlan::Filter { input, .. } => leaf_columns(input),
        PhysicalPlan::Project { input, .. } => leaf_columns(input),
        PhysicalPlan::Top { input, .. } => leaf_columns(input),
        _ => (Vec::new(), TableId(u32::MAX)),
    }
}

/// One column of an index key.
struct KeyCol {
    ty: TypeInfo,
    descending: bool,
}

/// The key columns of `index`, found by walking the databases, tables and indexes of
/// the storage.
fn key_columns_of(storage: &dyn Storage, index: IndexId) -> SqlResult<Vec<KeyCol>> {
    for (db, _) in storage.databases()? {
        for (tid, shape) in storage.tables(db)? {
            for (candidate, def) in storage.indexes(tid)? {
                if candidate != index {
                    continue;
                }
                return def
                    .columns
                    .iter()
                    .map(|kc| {
                        let ty = shape
                            .columns
                            .get(usize::from(kc.column))
                            .ok_or_else(|| {
                                bug(&format!(
                                    "key column {} past the {} columns of table {tid}",
                                    kc.column,
                                    shape.columns.len()
                                ))
                            })?
                            .clone();
                        Ok(KeyCol {
                            ty,
                            descending: kc.descending,
                        })
                    })
                    .collect();
            }
        }
    }
    Err(bug(&format!("index {index} is unknown")))
}

/// Evaluates a `KeyRangeExpr` into a `KeyRange` the storage can seek.
fn evaluate_seek_range(
    range: &KeyRangeExpr,
    key: &[KeyCol],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<KeyRange>> {
    match range {
        KeyRangeExpr::Full => Ok(Some(KeyRange::Full)),
        KeyRangeExpr::Point(exprs) => {
            let Some(values) = evaluate_key(exprs, key, ctx)? else {
                return Ok(None);
            };
            Ok(Some(KeyRange::Point(values)))
        }
        KeyRangeExpr::Between(lower, upper) => {
            let Some(lower) = evaluate_bound(lower, key, ctx)? else {
                return Ok(None);
            };
            let Some(upper) = evaluate_bound(upper, key, ctx)? else {
                return Ok(None);
            };
            if is_empty_range(&lower, &upper, key)? {
                return Ok(None);
            }
            Ok(Some(KeyRange::Between(lower, upper)))
        }
    }
}

fn evaluate_bound(
    bound: &Bound<Vec<vauban_binder::BoundExpr>>,
    key: &[KeyCol],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<Bound<Vec<Value>>>> {
    Ok(match bound {
        Bound::Unbounded => Some(Bound::Unbounded),
        Bound::Included(exprs) => evaluate_key(exprs, key, ctx)?.map(Bound::Included),
        Bound::Excluded(exprs) => evaluate_key(exprs, key, ctx)?.map(Bound::Excluded),
    })
}

fn evaluate_key(
    exprs: &[vauban_binder::BoundExpr],
    key: &[KeyCol],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<Vec<Value>>> {
    let mut values = Vec::with_capacity(exprs.len());
    for (pos, expr) in exprs.iter().enumerate() {
        let Some(col) = key.get(pos) else {
            return Err(bug(&format!(
                "a bound of {} values on a key of {} column(s)",
                exprs.len(),
                key.len()
            )));
        };
        let value = eval_expr(expr, None, ctx)?;
        let value = if expr.ty.ty == col.ty.ty {
            value
        } else {
            convert(&value, &expr.ty, &col.ty, None).map_err(|e| at(e, expr.line))?
        };
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        values.push(value);
    }
    Ok(Some(values))
}

fn is_empty_range(
    lower: &Bound<Vec<Value>>,
    upper: &Bound<Vec<Value>>,
    key: &[KeyCol],
) -> SqlResult<bool> {
    let (low, low_excluded) = match lower {
        Bound::Unbounded => return Ok(false),
        Bound::Included(v) => (v, false),
        Bound::Excluded(v) => (v, true),
    };
    let (high, high_excluded) = match upper {
        Bound::Unbounded => return Ok(false),
        Bound::Included(v) => (v, false),
        Bound::Excluded(v) => (v, true),
    };
    Ok(match compare_prefixes(low, high, key)? {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            (low.len() <= high.len() && low_excluded) || (low.len() >= high.len() && high_excluded)
        }
    })
}

fn compare_prefixes(a: &[Value], b: &[Value], key: &[KeyCol]) -> SqlResult<Ordering> {
    for ((x, y), col) in a.iter().zip(b).zip(key) {
        let collation = col
            .ty
            .collation
            .as_ref()
            .unwrap_or(&vauban_types::Collation::DEFAULT);
        let ordering = compare(x, y, collation)?.unwrap_or(Ordering::Equal);
        let ordering = if col.descending {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
