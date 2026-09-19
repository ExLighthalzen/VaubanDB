//! `HashJoin`: a hash join that builds a hash table from one input and probes with the
//! other. `NULL` keys do not match. `Full` answers the internal error 50000.
//!
//! The hash table lives entirely in memory; spilling to disk is not implemented for V1.

use std::collections::HashMap;

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp, OutputSchema};
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

/// Which join inputs a hash-key expression touches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeySideRefs {
    probe: bool,
    build: bool,
}

impl KeySideRefs {
    fn probe_only(self) -> bool {
        self.probe && !self.build
    }

    fn build_only(self) -> bool {
        self.build && !self.probe
    }

    fn neither(self) -> bool {
        !self.probe && !self.build
    }

    fn both(self) -> bool {
        self.probe && self.build
    }

    fn union(self, other: Self) -> Self {
        Self {
            probe: self.probe || other.probe,
            build: self.build || other.build,
        }
    }
}

/// Walks `expr` and records whether it reads probe columns, build columns, or neither.
fn key_side_refs(expr: &BoundExpr, probe_width: usize) -> KeySideRefs {
    match &expr.kind {
        BoundExprKind::ColumnRef(binding) => {
            if binding.index < probe_width {
                KeySideRefs {
                    probe: true,
                    build: false,
                }
            } else {
                KeySideRefs {
                    probe: false,
                    build: true,
                }
            }
        }
        BoundExprKind::Literal(_) | BoundExprKind::Variable { .. } => KeySideRefs {
            probe: false,
            build: false,
        },
        BoundExprKind::Negate(inner) | BoundExprKind::BitNot(inner) | BoundExprKind::Not(inner) => {
            key_side_refs(inner, probe_width)
        }
        BoundExprKind::IsNull { expr, .. } => key_side_refs(expr, probe_width),
        BoundExprKind::Convert { expr, .. } => key_side_refs(expr, probe_width),
        BoundExprKind::Collate { expr } => key_side_refs(expr, probe_width),
        BoundExprKind::Arith { left, right, .. } => {
            key_side_refs(left, probe_width).union(key_side_refs(right, probe_width))
        }
        BoundExprKind::Compare { left, right, .. } => {
            key_side_refs(left, probe_width).union(key_side_refs(right, probe_width))
        }
        BoundExprKind::Logical { left, right, .. } => {
            key_side_refs(left, probe_width).union(key_side_refs(right, probe_width))
        }
        BoundExprKind::In { expr, list, .. } => list
            .iter()
            .fold(key_side_refs(expr, probe_width), |acc, item| {
                acc.union(key_side_refs(item, probe_width))
            }),
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut refs =
                key_side_refs(expr, probe_width).union(key_side_refs(pattern, probe_width));
            if let Some(escape) = escape {
                refs = refs.union(key_side_refs(escape, probe_width));
            }
            refs
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            let mut refs = operand.as_ref().map_or(
                KeySideRefs {
                    probe: false,
                    build: false,
                },
                |expr| key_side_refs(expr, probe_width),
            );
            for arm in arms {
                refs = refs
                    .union(key_side_refs(&arm.when, probe_width))
                    .union(key_side_refs(&arm.then, probe_width));
            }
            if let Some(else_) = else_ {
                refs = refs.union(key_side_refs(else_, probe_width));
            }
            refs
        }
        BoundExprKind::Function { args, .. } => args.iter().fold(
            KeySideRefs {
                probe: false,
                build: false,
            },
            |acc, arg| acc.union(key_side_refs(arg, probe_width)),
        ),
        BoundExprKind::Exists(_) | BoundExprKind::ScalarSubquery(_) => KeySideRefs {
            probe: false,
            build: false,
        },
        BoundExprKind::InSubquery { expr, .. } => key_side_refs(expr, probe_width),
    }
}

/// Pairs each equality as `(build-side, probe-side)` with indices relative to that row.
///
/// The planner keeps the `ON` order `(left, right)` in join coordinates; the build input
/// is the right operand and the probe input is the left one. A side that references both
/// inputs, or neither, is not a hash key and stays in the residual predicate.
fn orient_hash_keys(
    left: &BoundExpr,
    right: &BoundExpr,
    probe_width: usize,
) -> Option<(BoundExpr, BoundExpr)> {
    let left_refs = key_side_refs(left, probe_width);
    let right_refs = key_side_refs(right, probe_width);

    if left_refs.both() || right_refs.both() || left_refs.neither() || right_refs.neither() {
        return None;
    }

    if left_refs.probe_only() && right_refs.build_only() {
        Some((remap_for_build_side(right, probe_width), left.clone()))
    } else if left_refs.build_only() && right_refs.probe_only() {
        Some((remap_for_build_side(left, probe_width), right.clone()))
    } else if left_refs.probe_only() && right_refs.probe_only() {
        // Unit plans may already carry build-local indices on the left operand.
        Some((remap_for_build_side(left, probe_width), right.clone()))
    } else {
        None
    }
}

fn eq_expr(left: BoundExpr, right: BoundExpr, template: &BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: template.ty.clone(),
        line: template.line,
    }
}

fn and_exprs(left: BoundExpr, right: BoundExpr, template: &BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Logical {
            op: LogicalOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: template.ty.clone(),
        line: template.line,
    }
}

fn merge_residual(
    residual: Option<BoundExpr>,
    extra: impl IntoIterator<Item = BoundExpr>,
    template: &BoundExpr,
) -> Option<BoundExpr> {
    extra.into_iter().fold(residual, |acc, expr| {
        Some(match acc {
            None => expr,
            Some(existing) => and_exprs(existing, expr, template),
        })
    })
}

fn remap_for_build_side(expr: &BoundExpr, left_width: usize) -> BoundExpr {
    BoundExpr {
        kind: remap_for_build_side_kind(expr.kind.clone(), left_width),
        ty: expr.ty.clone(),
        line: expr.line,
    }
}

fn remap_for_build_side_kind(kind: BoundExprKind, left_width: usize) -> BoundExprKind {
    match kind {
        BoundExprKind::ColumnRef(mut binding) => {
            if binding.index >= left_width {
                binding.index -= left_width;
            }
            BoundExprKind::ColumnRef(binding)
        }
        BoundExprKind::Negate(inner) => {
            BoundExprKind::Negate(Box::new(remap_for_build_side(&inner, left_width)))
        }
        BoundExprKind::BitNot(inner) => {
            BoundExprKind::BitNot(Box::new(remap_for_build_side(&inner, left_width)))
        }
        BoundExprKind::Not(inner) => {
            BoundExprKind::Not(Box::new(remap_for_build_side(&inner, left_width)))
        }
        BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
            expr: Box::new(remap_for_build_side(&expr, left_width)),
            negated,
        },
        BoundExprKind::Convert { expr, style, try_ } => BoundExprKind::Convert {
            expr: Box::new(remap_for_build_side(&expr, left_width)),
            style,
            try_,
        },
        BoundExprKind::Collate { expr } => BoundExprKind::Collate {
            expr: Box::new(remap_for_build_side(&expr, left_width)),
        },
        BoundExprKind::Arith { op, left, right } => BoundExprKind::Arith {
            op,
            left: Box::new(remap_for_build_side(&left, left_width)),
            right: Box::new(remap_for_build_side(&right, left_width)),
        },
        BoundExprKind::Compare { op, left, right } => BoundExprKind::Compare {
            op,
            left: Box::new(remap_for_build_side(&left, left_width)),
            right: Box::new(remap_for_build_side(&right, left_width)),
        },
        BoundExprKind::Logical { op, left, right } => BoundExprKind::Logical {
            op,
            left: Box::new(remap_for_build_side(&left, left_width)),
            right: Box::new(remap_for_build_side(&right, left_width)),
        },
        BoundExprKind::In {
            expr,
            list,
            negated,
        } => BoundExprKind::In {
            expr: Box::new(remap_for_build_side(&expr, left_width)),
            list: list
                .into_iter()
                .map(|item| remap_for_build_side(&item, left_width))
                .collect(),
            negated,
        },
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            negated,
        } => BoundExprKind::Like {
            expr: Box::new(remap_for_build_side(&expr, left_width)),
            pattern: Box::new(remap_for_build_side(&pattern, left_width)),
            escape: escape.map(|item| Box::new(remap_for_build_side(&item, left_width))),
            negated,
        },
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => BoundExprKind::Case {
            operand: operand.map(|item| Box::new(remap_for_build_side(&item, left_width))),
            arms: arms
                .into_iter()
                .map(|arm| vauban_binder::BoundCaseArm {
                    when: remap_for_build_side(&arm.when, left_width),
                    then: remap_for_build_side(&arm.then, left_width),
                })
                .collect(),
            else_: else_.map(|item| Box::new(remap_for_build_side(&item, left_width))),
        },
        BoundExprKind::Function { def, args } => BoundExprKind::Function {
            def,
            args: args
                .into_iter()
                .map(|arg| remap_for_build_side(&arg, left_width))
                .collect(),
        },
        other => other,
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
    let probe_width = probe.schema().columns.len();
    let left_width = probe_width;

    let template = keys
        .first()
        .map(|(left, _)| left)
        .or(residual.as_ref())
        .expect("HashJoin carries at least one ON conjunct");

    let mut hash_keys = Vec::with_capacity(keys.len());
    let mut extra_residual = Vec::new();
    for (left, right) in keys {
        if let Some(pair) = orient_hash_keys(left, right, probe_width) {
            hash_keys.push(pair);
        } else {
            extra_residual.push(eq_expr(left.clone(), right.clone(), template));
        }
    }
    let residual = merge_residual(residual.clone(), extra_residual, template);

    Ok(Box::new(HashJoin {
        build: build_operator(build)?,
        probe: build_operator(probe)?,
        kind: *kind,
        keys: hash_keys,
        residual,
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
