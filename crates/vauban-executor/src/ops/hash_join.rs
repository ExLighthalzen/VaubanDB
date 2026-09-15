//! `HashJoin`: a hash join that builds a hash table from one input and probes with the
//! other. `NULL` keys do not match. `Full` answers the internal error 50000.
//!
//! The hash table lives entirely in memory; spilling to disk is not implemented for V1.

use std::collections::HashMap;

use vauban_binder::{BoundExpr, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::{PhysicalJoinKind, PhysicalPlan};
use vauban_types::Value;

use crate::context::ExecContext;
use crate::expr::eval_expr;
use crate::operator::{Operator, bucket_hash, build_operator, keys_equal};
use crate::ops::not_implemented;
use crate::row::Row;

/// The hash join operator, built from a [`PhysicalPlan::HashJoin`].
pub(crate) struct HashJoin<'a> {
    build: Box<dyn Operator<'a> + 'a>,
    probe: Box<dyn Operator<'a> + 'a>,
    kind: PhysicalJoinKind,
    keys: Vec<(BoundExpr, BoundExpr)>,
    residual: Option<BoundExpr>,
    schema: OutputSchema,
    left_width: usize,
    right_width: usize,

    // Built during open
    build_buckets: HashMap<u64, Vec<usize>>,
    build_rows: Vec<(Vec<Value>, Row)>,

    // Probe-phase state
    probe_row: Option<Row>,
    probe_keys: Vec<Value>,
    bucket: Vec<usize>,
    bucket_pos: usize,
    probe_matched: bool,

    exhausted: bool,
}

/// The combined hash of several key values: the hash of each value folded in with
/// `wrapping_mul(31)` folds each key hash into one bucket value.
fn combined_hash(keys: &[Value]) -> u64 {
    let mut h: u64 = 0;
    for key in keys {
        h = h.wrapping_mul(31).wrapping_add(bucket_hash(key));
    }
    h
}

impl<'a> HashJoin<'a> {
    /// Drains the build operator and fills the hash table.
    /// Rows with a `NULL` key are skipped.
    fn build_table(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.build.open(ctx)?;
        loop {
            if ctx.cancelled() {
                self.build.close();
                return Ok(());
            }
            match self.build.next(ctx)? {
                None => break,
                Some(row) => {
                    let mut key_values = Vec::with_capacity(self.keys.len());
                    let mut has_null = false;
                    for (build_key, _) in &self.keys {
                        let v = eval_expr(build_key, Some(&row), ctx)?;
                        if matches!(v, Value::Null) {
                            has_null = true;
                            break;
                        }
                        key_values.push(v);
                    }
                    if has_null {
                        continue;
                    }
                    let hash = combined_hash(&key_values);
                    self.build_buckets
                        .entry(hash)
                        .or_default()
                        .push(self.build_rows.len());
                    self.build_rows.push((key_values, row));
                }
            }
        }
        self.build.close();
        Ok(())
    }

    /// Concatenates a probe row and a build row into the output row.
    fn concat(&self, probe_row: &Row, build_row: &Row) -> Row {
        let mut out = Vec::with_capacity(self.left_width + build_row.len());
        out.extend_from_slice(probe_row);
        out.extend_from_slice(build_row);
        out
    }

    /// Extends a probe row with `count` NULLs for the build side.
    fn extend_null(&self, probe_row: &Row, count: usize) -> Row {
        let mut out = Vec::with_capacity(self.left_width + count);
        out.extend_from_slice(probe_row);
        out.extend(std::iter::repeat_n(Value::Null, count));
        out
    }
}

impl<'a> Operator<'a> for HashJoin<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.build_table(ctx)?;
        if ctx.cancelled() {
            self.exhausted = true;
            return Ok(());
        }
        self.probe.open(ctx)?;
        self.exhausted = false;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        if self.exhausted {
            return Ok(None);
        }

        loop {
            // Pull the next probe row if we don't have one.
            if self.probe_row.is_none() {
                if ctx.cancelled() {
                    self.exhausted = true;
                    return Ok(None);
                }
                self.probe_row = self.probe.next(ctx)?;
                if self.probe_row.is_none() {
                    // Probe is exhausted; we are done for INNER and LEFT.
                    self.exhausted = true;
                    return Ok(None);
                }

                let row = self.probe_row.as_ref().unwrap();

                // Evaluate probe keys. A NULL probe key does not match.
                self.probe_keys = Vec::with_capacity(self.keys.len());
                let mut has_null = false;
                for (_, probe_key) in &self.keys {
                    let v = eval_expr(probe_key, Some(row), ctx)?;
                    if matches!(v, Value::Null) {
                        has_null = true;
                        break;
                    }
                    self.probe_keys.push(v);
                }

                if has_null {
                    // NULL key does not match. For LEFT, emit probe with NULL padding.
                    if self.kind == PhysicalJoinKind::Left {
                        let nulled = self.extend_null(row, self.right_width);
                        self.probe_row = None;
                        return Ok(Some(nulled));
                    }
                    // INNER: skip this probe row.
                    self.probe_row = None;
                    continue;
                }

                // Look up the bucket for the combined hash.
                let hash = combined_hash(&self.probe_keys);
                self.bucket = self.build_buckets.get(&hash).cloned().unwrap_or_default();
                self.bucket_pos = 0;
                self.probe_matched = false;
            }

            let row = self.probe_row.as_ref().unwrap();

            // Try each entry in the bucket.
            while self.bucket_pos < self.bucket.len() {
                let build_idx = self.bucket[self.bucket_pos];
                self.bucket_pos += 1;

                let (build_keys, build_row) = &self.build_rows[build_idx];

                // Check each key pair with keys_equal.
                let mut keys_ok = true;
                for (i, (build_key_expr, _)) in self.keys.iter().enumerate() {
                    if !keys_equal(&self.probe_keys[i], &build_keys[i], &build_key_expr.ty)? {
                        keys_ok = false;
                        break;
                    }
                }

                if !keys_ok {
                    continue;
                }

                // Keys match. Apply the residual if present.
                if let Some(residual) = &self.residual {
                    let concat = self.concat(row, build_row);
                    let ok = eval_expr(residual, Some(&concat), ctx)?;
                    if !matches!(ok, Value::Bit(true)) {
                        continue;
                    }
                }

                self.probe_matched = true;
                return Ok(Some(self.concat(row, build_row)));
            }

            // Bucket exhausted for this probe row.
            if self.kind == PhysicalJoinKind::Left && !self.probe_matched {
                let nulled = self.extend_null(row, self.right_width);
                self.probe_row = None;
                return Ok(Some(nulled));
            }

            self.probe_row = None;
        }
    }

    fn close(&mut self) {
        self.probe.close();
        self.build.close();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// Builds the operator of a [`PhysicalPlan::HashJoin`].
///
/// # Errors
///
/// The internal error 50000 for `Full`.
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let PhysicalPlan::HashJoin {
        build,
        probe,
        kind,
        keys,
        residual,
        schema,
    } = plan
    else {
        return Err(not_implemented("HashJoin"));
    };

    if *kind == PhysicalJoinKind::Full {
        return Err(SqlError::from(InternalError::Bug(
            "HashJoin: Full is served by NestedLoopJoin only, choose it in the planner".to_owned(),
        )));
    }

    let right_width = build.schema().columns.len();
    let left_width = schema.columns.len() - right_width;

    Ok(Box::new(HashJoin {
        build: build_operator(build)?,
        probe: build_operator(probe)?,
        kind: *kind,
        keys: keys.clone(),
        residual: residual.clone(),
        schema: schema.clone(),
        left_width,
        right_width,
        build_buckets: HashMap::new(),
        build_rows: Vec::new(),
        probe_row: None,
        probe_keys: Vec::new(),
        bucket: Vec::new(),
        bucket_pos: 0,
        probe_matched: false,
        exhausted: false,
    }))
}
