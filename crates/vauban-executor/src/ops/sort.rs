//! `Sort`, `TopN` (including `TOP … WITH TIES` and `TOP … PERCENT`), and `Distinct`:
//! ordering, ordered top, and deduplication.
//!
//! # Stability
//!
//! The sort is **stable** (`slice::sort_by`): rows of equal sort key keep their input
//! order. This is an implementation choice, not a property of SQL Server, which does not
//! guarantee the order of rows whose keys are equal.

use std::collections::VecDeque;

use vauban_binder::{BoundTop, OutputSchema, SortKey};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::PhysicalPlan;
use vauban_types::{Collation, Value, compare};

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::expr::eval_expr;
use crate::operator::{Operator, bucket_hash, build_operator, keys_equal};
use crate::plan::{RowBudget, eval_budget};
use crate::row::Row;

// -----------------------------------------------------------------------
// Sort
// -----------------------------------------------------------------------

/// Orders the rows of `input` by `keys`, using a stable sort.
struct Sort<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    keys: Vec<SortKey>,
    state: SortState,
    input_open: bool,
}

enum SortState {
    Pending,
    Streaming { rows: VecDeque<Row> },
    Exhausted,
}

impl<'a> Sort<'a> {
    fn new(input: Box<dyn Operator<'a> + 'a>, keys: Vec<SortKey>) -> Self {
        Self {
            input,
            keys,
            state: SortState::Pending,
            input_open: false,
        }
    }
}

impl<'a> Operator<'a> for Sort<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)?;
        self.input_open = true;
        let mut rows: Vec<Row> = Vec::new();
        while let Some(row) = self.input.next(ctx)? {
            rows.push(row);
            if (rows.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.state = SortState::Exhausted;
                return Ok(());
            }
        }
        if !self.keys.is_empty() {
            // Pre-evaluate sort key values for each row.
            let key_values: Vec<Vec<Value>> = rows
                .iter()
                .map(|row| eval_keys(&self.keys, row, ctx))
                .collect::<SqlResult<Vec<_>>>()?;
            let collations = key_collations(&self.keys);
            let mut indices: Vec<usize> = (0..rows.len()).collect();
            indices.sort_by(|&a, &b| {
                compare_key_values(&key_values[a], &key_values[b], &collations, &self.keys)
            });
            rows = indices
                .into_iter()
                .map(|i| std::mem::take(&mut rows[i]))
                .collect();
        }
        self.state = SortState::Streaming {
            rows: rows.into_iter().collect(),
        };
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match &mut self.state {
            SortState::Pending => Err(bug("Sort::next called before open")),
            SortState::Exhausted => Ok(None),
            SortState::Streaming { rows } => Ok(rows.pop_front()),
        }
    }

    fn close(&mut self) {
        if self.input_open {
            self.input.close();
            self.input_open = false;
        }
        self.state = SortState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}

// -----------------------------------------------------------------------
// TopN
// -----------------------------------------------------------------------

struct TopN<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    keys: Vec<SortKey>,
    top: BoundTop,
    state: TopNState,
    input_open: bool,
}

enum TopNState {
    Pending,
    Buffered { rows: VecDeque<Row> },
    Exhausted,
}

impl<'a> TopN<'a> {
    fn new(input: Box<dyn Operator<'a> + 'a>, keys: Vec<SortKey>, top: BoundTop) -> Self {
        Self {
            input,
            keys,
            top,
            state: TopNState::Pending,
            input_open: false,
        }
    }
}

impl<'a> Operator<'a> for TopN<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        if self.top.with_ties && self.keys.is_empty() {
            return Err(SqlError::from(InternalError::Bug(
                "TopN WITH TIES without ORDER BY (1062 is raised by binder)".to_owned(),
            )));
        }

        let budget = eval_budget(&self.top, ctx)?;
        if budget.is_zero() {
            self.state = TopNState::Exhausted;
            return Ok(());
        }

        self.input.open(ctx)?;
        self.input_open = true;

        let mut rows: Vec<Row> = Vec::new();
        while let Some(row) = self.input.next(ctx)? {
            rows.push(row);
            if (rows.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.state = TopNState::Exhausted;
                return Ok(());
            }
        }

        if rows.is_empty() {
            self.state = TopNState::Exhausted;
            return Ok(());
        }

        // Sort the whole input.
        if !self.keys.is_empty() {
            let key_values: Vec<Vec<Value>> = rows
                .iter()
                .map(|row| eval_keys(&self.keys, row, ctx))
                .collect::<SqlResult<Vec<_>>>()?;
            let collations = key_collations(&self.keys);
            let mut indices: Vec<usize> = (0..rows.len()).collect();
            indices.sort_by(|&a, &b| {
                compare_key_values(&key_values[a], &key_values[b], &collations, &self.keys)
            });
            rows = indices
                .into_iter()
                .map(|i| std::mem::take(&mut rows[i]))
                .collect();
        }

        let collations = key_collations(&self.keys);

        let kept = match budget {
            RowBudget::Rows(count) => {
                let n = usize::try_from(count).unwrap_or(usize::MAX).min(rows.len());
                if self.top.with_ties {
                    let key_v: Vec<Vec<Value>> = rows
                        .iter()
                        .map(|row| eval_keys(&self.keys, row, ctx))
                        .collect::<SqlResult<Vec<_>>>()?;
                    let boundary = n.saturating_sub(1);
                    let mut end = n;
                    while end < rows.len()
                        && equal_key_values(&key_v[boundary], &key_v[end], &collations)
                    {
                        end += 1;
                    }
                    end
                } else {
                    n
                }
            }
            RowBudget::Percent(p) => RowBudget::Percent(p).rows_of(rows.len()).min(rows.len()),
        };

        rows.truncate(kept);
        self.state = TopNState::Buffered {
            rows: rows.into_iter().collect(),
        };
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match &mut self.state {
            TopNState::Pending => Err(bug("TopN::next called before open")),
            TopNState::Exhausted => Ok(None),
            TopNState::Buffered { rows } => Ok(rows.pop_front()),
        }
    }

    fn close(&mut self) {
        if self.input_open {
            self.input.close();
            self.input_open = false;
        }
        self.state = TopNState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}

// -----------------------------------------------------------------------
// Distinct
// -----------------------------------------------------------------------

struct Distinct<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    state: DistinctState,
    input_open: bool,
}

enum DistinctState {
    Pending,
    Buffered { rows: VecDeque<Row> },
    Exhausted,
}

impl<'a> Distinct<'a> {
    fn new(input: Box<dyn Operator<'a> + 'a>) -> Self {
        Self {
            input,
            state: DistinctState::Pending,
            input_open: false,
        }
    }
}

impl<'a> Operator<'a> for Distinct<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)?;
        self.input_open = true;

        let schema = self.input.schema();
        let types: Vec<_> = schema.columns.iter().map(|c| c.ty.clone()).collect();
        let mut seen_buckets: Vec<Vec<Row>> = Vec::new();
        let mut rows: Vec<Row> = Vec::new();

        while let Some(row) = self.input.next(ctx)? {
            let hash = row_hash(&row, &types);
            let bucket_idx = (hash as usize) % 1024;
            if bucket_idx >= seen_buckets.len() {
                seen_buckets.resize(bucket_idx + 1, Vec::new());
            }
            let bucket = &seen_buckets[bucket_idx];
            let is_dup = bucket.iter().any(|seen| rows_equal(seen, &row, &types));
            if !is_dup {
                seen_buckets[bucket_idx].push(row.clone());
                rows.push(row);
            }

            if (rows.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.state = DistinctState::Exhausted;
                return Ok(());
            }
        }

        self.state = DistinctState::Buffered {
            rows: rows.into_iter().collect(),
        };
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match &mut self.state {
            DistinctState::Pending => Err(bug("Distinct::next called before open")),
            DistinctState::Exhausted => Ok(None),
            DistinctState::Buffered { rows } => Ok(rows.pop_front()),
        }
    }

    fn close(&mut self) {
        if self.input_open {
            self.input.close();
            self.input_open = false;
        }
        self.state = DistinctState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        self.input.schema()
    }
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Evaluates the sort keys against a row.
fn eval_keys(keys: &[SortKey], row: &Row, ctx: &mut ExecContext<'_>) -> SqlResult<Vec<Value>> {
    keys.iter()
        .map(|key| eval_expr(&key.expr, Some(row), ctx))
        .collect()
}

/// Collations of the sort keys. Defaults to `DEFAULT` when a key carries `None`.
fn key_collations(keys: &[SortKey]) -> Vec<Collation> {
    keys.iter()
        .map(|k| k.collation.unwrap_or(Collation::DEFAULT))
        .collect()
}

/// Compares two vectors of key values.
fn compare_key_values(
    a: &[Value],
    b: &[Value],
    collations: &[Collation],
    keys: &[SortKey],
) -> std::cmp::Ordering {
    for (i, key) in keys.iter().enumerate() {
        let collation = &collations[i];
        let va = &a[i];
        let vb = &b[i];
        match compare(va, vb, collation) {
            Ok(Some(ord)) => {
                if ord != std::cmp::Ordering::Equal {
                    return if key.desc { ord.reverse() } else { ord };
                }
            }
            // One or both are NULL.
            Ok(None) => {
                let a_null = matches!(va, Value::Null);
                let b_null = matches!(vb, Value::Null);
                match (a_null, b_null) {
                    (true, true) => continue,
                    (true, false) => {
                        return if key.desc {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        };
                    }
                    (false, true) => {
                        return if key.desc {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                    }
                    (false, false) => unreachable!(),
                }
            }
            Err(_) => return std::cmp::Ordering::Equal,
        }
    }
    std::cmp::Ordering::Equal
}

/// Whether two key value vectors are equal.
fn equal_key_values(a: &[Value], b: &[Value], collations: &[Collation]) -> bool {
    for (i, collation) in collations.iter().enumerate() {
        match compare(&a[i], &b[i], collation) {
            Ok(Some(ord)) => {
                if ord != std::cmp::Ordering::Equal {
                    return false;
                }
            }
            Ok(None) => return false,
            Err(_) => return false,
        }
    }
    true
}

/// Whether two rows are equal on each column, using [`keys_equal`].
///
/// For `DISTINCT`, two `NULL` values are equal, which differs from [`keys_equal`]
/// (`NULL ≠ NULL` in comparison semantics).
fn rows_equal(a: &Row, b: &Row, types: &[vauban_types::TypeInfo]) -> bool {
    for (i, ty) in types.iter().enumerate() {
        let Some(va) = a.get(i) else { return false };
        let Some(vb) = b.get(i) else { return false };
        if matches!(va, Value::Null) && matches!(vb, Value::Null) {
            continue;
        }
        if !keys_equal(va, vb, ty).unwrap_or(false) {
            return false;
        }
    }
    true
}

/// Computes a hash of the row for bucketing in Distinct, using [`bucket_hash`].
fn row_hash(row: &Row, _types: &[vauban_types::TypeInfo]) -> u64 {
    let mut h: u64 = 0;
    for val in row.iter() {
        h = h.wrapping_mul(31).wrapping_add(bucket_hash(val));
    }
    h
}

// -----------------------------------------------------------------------
// Build
// -----------------------------------------------------------------------

/// Builds the operator for a [`PhysicalPlan::Sort`], [`PhysicalPlan::TopN`], or
/// [`PhysicalPlan::Distinct`].
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    match plan {
        PhysicalPlan::Sort { input, keys } => {
            Ok(Box::new(Sort::new(build_operator(input)?, keys.clone())))
        }
        PhysicalPlan::TopN { input, keys, top } => Ok(Box::new(TopN::new(
            build_operator(input)?,
            keys.clone(),
            top.clone(),
        ))),
        PhysicalPlan::Distinct(input) => Ok(Box::new(Distinct::new(build_operator(input)?))),
        _ => Err(SqlError::from(InternalError::Bug(
            "sort::build: expected Sort, TopN or Distinct".to_owned(),
        ))),
    }
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
