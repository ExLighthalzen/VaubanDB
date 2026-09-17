//! `IndexSeek`: reads the rows an index serves for a range of keys, over
//! [`Storage::seek`](vauban_storage::Storage::seek).
//!
//! # From the plan to the storage
//!
//! The node carries a [`KeyRangeExpr`], whose bounds are expressions that read no column:
//! a literal, a variable, a conversion or a call over those. The operator evaluates them
//! **once**, when it opens, and turns the result into the [`KeyRange`] the storage takes:
//! `Point` for an equality on the key, `Between` for a prefix or an interval, with the
//! inclusion of each bound kept as the plan wrote it, `Full` for the whole index
//! (`tests/seek.rs`, `seek_point_finds_one_row`, `seek_between_inclusive_and_exclusive`,
//! `seek_full_range_gives_every_row_of_the_index`). The direction of the node is the one
//! handed to the storage (`tests/seek.rs`, `seek_backward_reverses_the_order`).
//!
//! # The type of a bound
//!
//! The storage compares a bound with the key column it applies to and refuses a value of
//! another family, so a bound whose type is not the type of its column is converted to it
//! by `types::convert` before the seek, as the binder converts the operands of a
//! comparison: an `int` bound on a `bigint` key finds the row (`tests/seek.rs`,
//! `seek_bound_is_converted_to_the_key_type`). A bound already of the type of its column
//! is kept as evaluated.
//!
//! The key columns are read from the storage: the index, then the shape of its table,
//! give the type of each column of the key and whether it is descending. The lookup walks
//! the databases, their tables and their indexes, once per operator: the second `open` of
//! the same operator reuses what the first one found.
//!
//! # Two ranges that read no row
//!
//! A bound that evaluates to `NULL` reads no row and calls no seek: `WHERE k = NULL` is
//! unknown, and the unknown is not true, whatever the operator (`tests/seek.rs`,
//! `seek_null_bound_gives_no_row`). A range whose lower bound is above its upper bound in
//! index order reads no row and calls no seek either (`tests/seek.rs`,
//! `seek_empty_range_gives_no_row`).
//!
//! # The columns
//!
//! As for a `TableScan`, the `index` of each [`ColumnBinding`] is the position of the
//! value in the row the storage hands out, and the answer keeps the columns of the node
//! in the order of the node (`tests/seek.rs`, `seek_projects_the_plan_columns`).
//!
//! # One lock per row handed over
//!
//! A row a seek reaches is locked, read and released exactly as a row a scan reaches
//! (`ops/scan.rs`). A lock names a table and a row, never an index, so the table the index
//! belongs to is part of what the first `open` looks up and keeps, next to the key columns.

use std::cmp::Ordering;
use std::ops::Bound;

use vauban_binder::{BoundExpr, ColumnBinding, LockHints, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::{KeyRangeExpr, PhysicalPlan};
use vauban_storage::{Direction, IndexId, KeyColumn, KeyRange, RowIter, Storage, TableId};
use vauban_types::{Collation, TypeInfo, Value, compare, convert};

use crate::context::ExecContext;
use crate::errors::at;
use crate::expr::eval_expr;
use crate::locking::{self, RowVisibility};
use crate::operator::Operator;
use crate::row::Row;

/// The rows `index` serves for `range`, in `direction`, keeping the columns `columns`
/// names, in that order.
struct IndexSeek<'a> {
    index: IndexId,
    range: KeyRangeExpr,
    columns: Vec<ColumnBinding>,
    direction: Direction,
    schema: OutputSchema,
    /// The locking words written on the table reference, read once per row.
    hints: LockHints,
    /// The table the index belongs to and the key columns of the index, read from the
    /// storage by the first `open` and kept for the next ones.
    key: Option<(TableId, Vec<KeyColumnType>)>,
    /// The iterator of `storage.seek`, `Some` between `open` and the end of the rows.
    iter: Option<Box<dyn RowIter + 'a>>,
}

/// One column of the key: its type, and whether the index stores it in reverse.
struct KeyColumnType {
    ty: TypeInfo,
    descending: bool,
}

/// Builds the operator of a [`PhysicalPlan::IndexSeek`].
///
/// # Errors
///
/// The internal error 50000 for a `schema` and a `columns` of different widths, which
/// takes a bug of the planner, and for a node that is not an `IndexSeek`.
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let PhysicalPlan::IndexSeek {
        index,
        range,
        columns,
        direction,
        schema,
        hints,
    } = plan
    else {
        return Err(bug("IndexSeek: the node is not an IndexSeek"));
    };
    if schema.columns.len() != columns.len() {
        return Err(bug(&format!(
            "IndexSeek: the node publishes {} column(s) and reads {}",
            schema.columns.len(),
            columns.len()
        )));
    }
    Ok(Box::new(IndexSeek {
        index: *index,
        range: range.clone(),
        columns: columns.clone(),
        direction: *direction,
        schema: schema.clone(),
        hints: *hints,
        key: None,
        iter: None,
    }))
}

impl<'a> Operator<'a> for IndexSeek<'a> {
    /// Evaluates the bounds, converts them to the type of their key column, and takes
    /// the iterator of `storage.seek`, or leaves the operator without an iterator when
    /// the range reads no row (module documentation).
    ///
    /// # Errors
    ///
    /// What evaluating or converting a bound raises, on the line of the bound. What
    /// [`Storage::seek`] raises, unchanged. The internal error 50000 of a context with no
    /// engine ([`ExecContext::storage`]), of an unknown index, and of a bound longer than
    /// the key.
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.iter = None;
        let storage = ctx.storage()?;
        let snap = locking::snapshot_for_scan(ctx, &self.hints)?;
        if self.key.is_none() {
            self.key = Some(key_columns_of(storage, self.index)?);
        }
        // Filled just above: the default is not reached.
        let key = self.key.as_ref().map(|(_, key)| key.as_slice());
        let Some(range) = evaluate_range(&self.range, key.unwrap_or_default(), ctx)? else {
            return Ok(());
        };
        self.iter = Some(storage.seek(&snap, self.index, &range, self.direction)?);
        Ok(())
    }

    /// The next row of the range, its columns picked by `index`, or `None` once the
    /// token is up.
    ///
    /// # Errors
    ///
    /// An `Err` item of the iterator, which ends the iteration, and the internal error
    /// 50000 for an `index` past the end of the row the storage handed out.
    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        loop {
            let Some(iter) = self.iter.as_mut() else {
                return Ok(None);
            };
            if ctx.cancelled() {
                self.iter = None;
                return Ok(None);
            }
            let (id, mut source) = match iter.next() {
                None => {
                    self.iter = None;
                    return Ok(None);
                }
                Some(Err(err)) => {
                    self.iter = None;
                    return Err(err);
                }
                Some(Ok(pair)) => pair,
            };
            let table = match &self.key {
                Some((table, _)) => *table,
                // `open` fills it before the first row: an operator that produced a row
                // without looking its index up is a bug of this file.
                None => return Err(bug("IndexSeek: the table of the index is not known")),
            };
            match locking::read_lock(ctx, table, id, &self.hints)? {
                RowVisibility::Skip => continue,
                RowVisibility::Latest => {
                    if let Some((_, latest)) = ctx.storage()?.latest_version(table, id)? {
                        source = latest;
                    }
                }
                RowVisibility::Visible => {}
            }
            let mut row = Vec::with_capacity(self.columns.len());
            for binding in &self.columns {
                let value = source.0.get(binding.index).ok_or_else(|| {
                    bug(&format!(
                        "IndexSeek: column `{}` is at index {} of a row of {} value(s)",
                        binding.name,
                        binding.index,
                        source.0.len()
                    ))
                })?;
                row.push(value.clone());
            }
            locking::end_row_read(ctx, table, id)?;
            return Ok(Some(row));
        }
    }

    fn close(&mut self) {
        self.iter = None;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// The table `index` belongs to and the key columns of `index`, found by walking the
/// databases, tables and indexes of the storage: the shape of the index says which columns
/// of its table form the key, the shape of the table says their type. The table comes back
/// with them because a row lock names a table and a row, and the node names an index.
///
/// # Errors
///
/// What the introspection of the storage raises. The internal error 50000 for an index
/// no table of no database holds (`tests/seek.rs`, `seek_on_an_unknown_index_is_a_bug`).
fn key_columns_of(
    storage: &dyn Storage,
    index: IndexId,
) -> SqlResult<(TableId, Vec<KeyColumnType>)> {
    for (db, _) in storage.databases()? {
        for (table, shape) in storage.tables(db)? {
            for (candidate, def) in storage.indexes(table)? {
                if candidate != index {
                    continue;
                }
                let key: Vec<KeyColumnType> = def
                    .columns
                    .iter()
                    .map(|column| key_column_type(column, &shape.columns, index))
                    .collect::<SqlResult<_>>()?;
                return Ok((table, key));
            }
        }
    }
    Err(bug(&format!("IndexSeek: index {index} is unknown")))
}

/// The type of one key column of `index`, read at its position in `columns`, the
/// columns of the table.
///
/// # Errors
///
/// The internal error 50000 for a position past the columns of the table.
fn key_column_type(
    column: &KeyColumn,
    columns: &[TypeInfo],
    index: IndexId,
) -> SqlResult<KeyColumnType> {
    let ty = columns.get(usize::from(column.column)).ok_or_else(|| {
        bug(&format!(
            "IndexSeek: index {index} keys column {} of a table of {} column(s)",
            column.column,
            columns.len()
        ))
    })?;
    Ok(KeyColumnType {
        ty: ty.clone(),
        descending: column.descending,
    })
}

/// The storage range of `range`, its bounds evaluated and converted to the type of their
/// key column; `None` for a range that reads no row, which is one with a `NULL` bound or
/// one whose lower bound is above its upper bound (module documentation).
///
/// # Errors
///
/// What evaluating or converting a bound raises, on the line of the bound; the internal
/// error 50000 for a bound longer than the key.
fn evaluate_range(
    range: &KeyRangeExpr,
    key: &[KeyColumnType],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<KeyRange>> {
    match range {
        KeyRangeExpr::Full => Ok(Some(KeyRange::Full)),
        KeyRangeExpr::Point(exprs) => Ok(evaluate_key(exprs, key, ctx)?.map(KeyRange::Point)),
        KeyRangeExpr::Between(lower, upper) => {
            let Some(lower) = evaluate_bound(lower, key, ctx)? else {
                return Ok(None);
            };
            let Some(upper) = evaluate_bound(upper, key, ctx)? else {
                return Ok(None);
            };
            if is_empty(&lower, &upper, key)? {
                return Ok(None);
            }
            Ok(Some(KeyRange::Between(lower, upper)))
        }
    }
}

/// One side of a `Between`, evaluated: `None` when a value of the side is `NULL`.
///
/// # Errors
///
/// Those of [`evaluate_key`].
fn evaluate_bound(
    bound: &Bound<Vec<BoundExpr>>,
    key: &[KeyColumnType],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<Bound<Vec<Value>>>> {
    Ok(match bound {
        Bound::Unbounded => Some(Bound::Unbounded),
        Bound::Included(exprs) => evaluate_key(exprs, key, ctx)?.map(Bound::Included),
        Bound::Excluded(exprs) => evaluate_key(exprs, key, ctx)?.map(Bound::Excluded),
    })
}

/// The values of a key prefix: each expression evaluated, then converted to the type of
/// its key column when it is not of that type already. `None` as soon as one value is
/// `NULL`.
///
/// # Errors
///
/// What `eval_expr` raises; what `types::convert` raises, on the line of the bound; the
/// internal error 50000 for more expressions than the key has columns.
fn evaluate_key(
    exprs: &[BoundExpr],
    key: &[KeyColumnType],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Option<Vec<Value>>> {
    let mut values = Vec::with_capacity(exprs.len());
    for (position, expr) in exprs.iter().enumerate() {
        let Some(column) = key.get(position) else {
            return Err(bug(&format!(
                "IndexSeek: a bound of {} value(s) on a key of {} column(s)",
                exprs.len(),
                key.len()
            )));
        };
        let value = eval_expr(expr, None, ctx)?;
        let value = if expr.ty.ty == column.ty.ty {
            value
        } else {
            convert(&value, &expr.ty, &column.ty, None).map_err(|e| at(e, expr.line))?
        };
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        values.push(value);
    }
    Ok(Some(values))
}

/// Whether no key lies between `lower` and `upper` in index order.
///
/// The two prefixes are compared over their common length, column by column, a
/// descending column reversed. A lower prefix above the upper one leaves nothing. Two
/// equal prefixes leave nothing when the shorter one, or either one of two prefixes of
/// the same length, is excluded: `Excluded([1])` under `Included([1, 5])` excludes the
/// whole group the upper bound lies in, and `Included([1, 5])` under `Excluded([1, 5])`
/// is the point without itself. An unbounded side leaves something
/// (`seek::tests::empty_ranges_on_these_shapes`).
///
/// # Errors
///
/// The internal error 50000 of `types::compare` for two values of different families.
fn is_empty(
    lower: &Bound<Vec<Value>>,
    upper: &Bound<Vec<Value>>,
    key: &[KeyColumnType],
) -> SqlResult<bool> {
    let (low, low_excluded) = match lower {
        Bound::Unbounded => return Ok(false),
        Bound::Included(values) => (values, false),
        Bound::Excluded(values) => (values, true),
    };
    let (high, high_excluded) = match upper {
        Bound::Unbounded => return Ok(false),
        Bound::Included(values) => (values, false),
        Bound::Excluded(values) => (values, true),
    };
    Ok(match compare_prefixes(low, high, key)? {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            (low.len() <= high.len() && low_excluded) || (low.len() >= high.len() && high_excluded)
        }
    })
}

/// The order of two key prefixes over their common length, in index order.
///
/// # Errors
///
/// The internal error 50000 of `types::compare` for two values of different families.
fn compare_prefixes(a: &[Value], b: &[Value], key: &[KeyColumnType]) -> SqlResult<Ordering> {
    for ((x, y), column) in a.iter().zip(b).zip(key) {
        let collation = column.ty.collation.as_ref().unwrap_or(&Collation::DEFAULT);
        // The values are not `NULL`: `evaluate_key` answered `None` for one.
        let ordering = compare(x, y, collation)?.unwrap_or(Ordering::Equal);
        let ordering = if column.descending {
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

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::SqlType;

    fn int() -> KeyColumnType {
        KeyColumnType {
            ty: TypeInfo::new(SqlType::Int, true),
            descending: false,
        }
    }

    fn values(v: &[i64]) -> Vec<Value> {
        v.iter().map(|n| Value::I64(*n)).collect()
    }

    /// The shapes of a range that reads no row, and their neighbours that read some.
    #[test]
    fn empty_ranges_on_these_shapes() {
        let key = [int(), int()];
        let empty = |low: Bound<Vec<Value>>, high: Bound<Vec<Value>>| {
            is_empty(&low, &high, &key).expect("same family")
        };
        assert!(empty(
            Bound::Included(values(&[4])),
            Bound::Included(values(&[2]))
        ));
        assert!(empty(
            Bound::Excluded(values(&[2])),
            Bound::Included(values(&[2]))
        ));
        assert!(empty(
            Bound::Included(values(&[2])),
            Bound::Excluded(values(&[2]))
        ));
        assert!(empty(
            Bound::Excluded(values(&[1])),
            Bound::Included(values(&[1, 5]))
        ));
        assert!(empty(
            Bound::Included(values(&[1, 5])),
            Bound::Excluded(values(&[1]))
        ));
        assert!(!empty(
            Bound::Included(values(&[2])),
            Bound::Included(values(&[2]))
        ));
        assert!(!empty(
            Bound::Included(values(&[2])),
            Bound::Excluded(values(&[4]))
        ));
        assert!(!empty(
            Bound::Included(values(&[1])),
            Bound::Excluded(values(&[1, 5]))
        ));
        assert!(!empty(
            Bound::Included(values(&[1, 5])),
            Bound::Included(values(&[1]))
        ));
        assert!(!empty(Bound::Unbounded, Bound::Excluded(values(&[1]))));
        assert!(!empty(Bound::Excluded(values(&[9])), Bound::Unbounded));
    }

    /// A descending column reverses the order the bounds are compared in.
    #[test]
    fn a_descending_column_reverses_the_order() {
        let key = [KeyColumnType {
            ty: TypeInfo::new(SqlType::Int, true),
            descending: true,
        }];
        let low = Bound::Included(values(&[4]));
        let high = Bound::Included(values(&[2]));
        assert!(!is_empty(&low, &high, &key).expect("same family"));
        assert!(is_empty(&high, &low, &key).expect("same family"));
    }
}
