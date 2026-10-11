//! `HashJoin`: a hash join that builds a hash table from one input and probes with the
//! other. `NULL` keys do not match. `Full` answers the internal error 50000.
//!
//! The table is the shared `operator::hash_table`: a key is the sort keys of the key
//! values under the collation of each build key, so `'x'` matches `'X'` under a
//! case-insensitive collation and `1.0` matches `1.00`
//! (`tests/hash_join.rs`, `string_keys_match_under_the_collation_of_the_build_key`). It lives
//! in memory; spilling to disk is not implemented for V1.
//!
//! The joined row is the probe row then the build row, visible values first; the row
//! identifiers each side carries follow them, those of the probe side first, as in a
//! nested-loop join (`tests/row_id.rs`,
//! `the_row_id_of_a_joined_table_crosses_the_joins_the_filter_and_the_sort`).
//!
//! [`HashSemiJoin`] runs the semi-join and the anti-semi-join of a decorrelated `IN`,
//! `NOT IN`, `EXISTS` or `NOT EXISTS` through the same table: the subquery is read once and
//! each outer row looks its candidates up.

use vauban_binder::{BoundExpr, BoundExprKind, CompareOp, LogicalOp, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::{PhysicalJoinKind, PhysicalPlan};
use vauban_types::Value;

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::expr::{as_condition, eval_expr};
use crate::operator::hash_table::{HashTable, KeyEncoder};
use crate::operator::{Operator, build_operator};
use crate::ops::not_implemented;
use crate::row::{JoinSide, Row, join_rows};

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
    /// How many row identifiers the probe rows carry after their visible values.
    probe_tail: usize,
    /// How many row identifiers the build rows carry after their visible values.
    build_tail: usize,

    /// Writes the key of either side, under the collation of each build key.
    encoder: KeyEncoder,

    // Built during open: the build rows, and the positions of those of each key.
    build_table: HashTable<Vec<usize>>,
    build_rows: Vec<Row>,

    // Probe-phase state
    probe_row: Option<Row>,
    /// The entry of the table the probe row's key found, if any.
    bucket: Option<usize>,
    bucket_pos: usize,
    probe_matched: bool,

    exhausted: bool,
}

/// Evaluates `keys` against `row`, the row of one side before the join concatenates it:
/// each build key was moved to the positions of the build row when the operator was
/// built ([`orient_hash_keys`]), each probe key reads the probe row, which starts the
/// joined row (`tests/subquery.rs`,
/// `hash_join_inside_a_correlated_scalar_filters_on_its_build_side`). `None` on the
/// first `NULL` key, as a hash join key does.
fn eval_side_keys<'a, 'k>(
    keys: impl Iterator<Item = &'k BoundExpr>,
    row: &Row,
    ctx: &mut ExecContext<'a>,
) -> SqlResult<Option<Vec<Value>>> {
    let mut values = Vec::new();
    for key in keys {
        match eval_expr(key, Some(row), ctx)? {
            Value::Null => return Ok(None),
            v => values.push(v),
        }
    }
    Ok(Some(values))
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
                    let keys = self.keys.iter().map(|(build_key, _)| build_key);
                    let key_values = eval_side_keys(keys, &row, ctx)?;
                    let Some(key_values) = key_values else {
                        continue;
                    };
                    let key = self.encoder.encode(&key_values)?;
                    let (positions, _) = self.build_table.entry(key, || Ok(Vec::new()))?;
                    positions.push(self.build_rows.len());
                    self.build_rows.push(row);
                }
            }
        }
        self.build.close();
        Ok(())
    }

    /// Concatenates a probe row and a build row into the output row, or pads the build
    /// side with `NULL` when `build_row` is `None` ([`join_rows`]).
    fn concat(&self, probe_row: &Row, build_row: Option<&Row>) -> Row {
        join_rows(
            &JoinSide {
                row: Some(probe_row),
                width: self.left_width,
                tail: self.probe_tail,
            },
            &JoinSide {
                row: build_row.map(Vec::as_slice),
                width: self.right_width,
                tail: self.build_tail,
            },
        )
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
                let keys = self.keys.iter().map(|(_, probe_key)| probe_key);
                let values = eval_side_keys(keys, row, ctx)?;
                let Some(values) = values else {
                    // NULL key does not match. For LEFT, emit probe with NULL padding.
                    let row = self.probe_row.as_ref().unwrap();
                    if self.kind == PhysicalJoinKind::Left {
                        let nulled = self.concat(row, None);
                        self.probe_row = None;
                        return Ok(Some(nulled));
                    }
                    // INNER: skip this probe row.
                    self.probe_row = None;
                    continue;
                };
                let key = self.encoder.encode(&values)?;
                self.bucket = self.build_table.find(key);
                self.bucket_pos = 0;
                self.probe_matched = false;
            }

            let row = self.probe_row.as_ref().unwrap();

            // Each build row of the probe key: the keys are equal, the residual decides.
            let positions = self
                .bucket
                .and_then(|entry| self.build_table.get(entry))
                .map_or(&[][..], Vec::as_slice);
            while self.bucket_pos < positions.len() {
                let build_row = &self.build_rows[positions[self.bucket_pos]];
                self.bucket_pos += 1;

                // Apply the residual if present.
                if let Some(residual) = &self.residual {
                    let concat = self.concat(row, Some(build_row));
                    let ok = eval_expr(residual, Some(&concat), ctx)?;
                    if !matches!(ok, Value::Bit(true)) {
                        continue;
                    }
                }

                self.probe_matched = true;
                return Ok(Some(self.concat(row, Some(build_row))));
            }

            // Bucket exhausted for this probe row.
            if self.kind == PhysicalJoinKind::Left && !self.probe_matched {
                let nulled = self.concat(row, None);
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
        // A column of an enclosing query is read from the outer-row stack, whichever
        // side evaluates it: it belongs to neither input.
        BoundExprKind::ColumnRef(binding) if binding.depth > 0 => KeySideRefs {
            probe: false,
            build: false,
        },
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
/// The keys arrive in join coordinates, in either order; the build input is the right
/// operand and the probe input is the left one. A member that references both inputs, or
/// neither, and two members that reference the same input, make no hash key: the equality
/// stays in the residual predicate, checked on the joined row
/// (`tests/hash_join.rs`, `an_equality_between_two_probe_columns_stays_residual`).
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
            if binding.depth == 0 && binding.index >= left_width {
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

    let encoder = KeyEncoder::new(hash_keys.iter().map(|(build_key, _)| &build_key.ty));
    Ok(Box::new(HashJoin {
        build: build_operator(build)?,
        probe: build_operator(probe)?,
        kind: *kind,
        keys: hash_keys,
        residual,
        schema: schema.clone(),
        left_width,
        right_width,
        probe_tail: probe.row_id_count(),
        build_tail: build.row_id_count(),
        encoder,
        build_table: HashTable::new(),
        build_rows: Vec::new(),
        probe_row: None,
        bucket: None,
        bucket_pos: 0,
        probe_matched: false,
        exhausted: false,
    }))
}

/// The semi-join and the anti-semi-join a [`PhysicalPlan::NestedLoopJoin`] of kind
/// [`PhysicalJoinKind::Semi`] or [`PhysicalJoinKind::AntiSemi`] runs: the inner input is
/// read once, into a hash table keyed on the equalities of the `on` that pair an inner
/// expression with an outer one, and each outer row looks its candidates up there.
///
/// The planner builds these two kinds over an inner input that reads nothing of the outer
/// row, so one reading of it serves the outer rows of one `open`. The output row is the
/// outer row as it came, row identifiers included.
///
/// The full `on` is evaluated on each candidate pair, the hash table narrowing the
/// candidates down: a key holds the sort keys of the members, under the collation the
/// comparison applies (that of its left member, else of its right member).
///
/// - `Semi` keeps the outer row when the `on` is true for one inner row. An outer row with a
///   `NULL` key has no candidate (`tests/hash_join.rs`,
///   `semi_join_keeps_the_rows_found_and_drops_a_null_key`).
/// - `AntiSemi` keeps the outer row when the `on` is false for each inner row: one inner
///   row that makes it true or unknown drops the outer row. This is the three-valued
///   logic of `v NOT IN (SELECT w …)`: a `NULL` among the `w` drops each outer row, and a
///   `NULL` `v` is dropped unless the inner input is empty (`tests/hash_join.rs`,
///   `anti_semi_join_follows_not_in_with_a_null_on_either_side`). Without an `on`, the
///   outer row is kept when the inner input is empty, as a `NOT EXISTS` is.
pub(crate) struct HashSemiJoin<'a> {
    outer: Box<dyn Operator<'a> + 'a>,
    inner: Box<dyn Operator<'a> + 'a>,
    kind: PhysicalJoinKind,
    /// The `on` over the concatenated row, evaluated on each candidate pair.
    on: Option<BoundExpr>,
    /// `(inner, outer)` members of the equalities used as keys, each over its own row.
    keys: Vec<(BoundExpr, BoundExpr)>,
    schema: OutputSchema,
    outer_width: usize,
    inner_width: usize,
    outer_tail: usize,
    inner_tail: usize,
    encoder: KeyEncoder,
    /// Whether the inner input has been read into the table.
    built: bool,
    table: HashTable<Vec<usize>>,
    /// The inner rows whose keys hold no `NULL`, indexed by the table.
    rows: Vec<Row>,
    /// The inner rows with a `NULL` key: candidates of each outer row of an `AntiSemi`.
    null_key_rows: Vec<Row>,
    rows_since_cancel: u64,
    exhausted: bool,
}

/// Builds the operator of a semi- or anti-semi [`PhysicalPlan::NestedLoopJoin`].
///
/// # Errors
///
/// What building either input raises.
pub(crate) fn build_semi<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let PhysicalPlan::NestedLoopJoin {
        outer,
        inner,
        kind,
        on,
        schema,
    } = plan
    else {
        return Err(not_implemented("semi join"));
    };
    let outer_width = outer.schema().columns.len();
    let mut keys = Vec::new();
    let mut key_types = Vec::new();
    if let Some(on) = on {
        let mut conjuncts = Vec::new();
        split_and(on, &mut conjuncts);
        for conjunct in conjuncts {
            let BoundExprKind::Compare {
                op: CompareOp::Eq,
                left,
                right,
            } = &conjunct.kind
            else {
                continue;
            };
            let Some(pair) = orient_hash_keys(left, right, outer_width) else {
                continue;
            };
            let mut ty = pair.0.ty.clone();
            ty.collation = left.ty.collation.or(right.ty.collation);
            key_types.push(ty);
            keys.push(pair);
        }
    }
    Ok(Box::new(HashSemiJoin {
        outer: build_operator(outer)?,
        inner: build_operator(inner)?,
        kind: *kind,
        on: on.clone(),
        keys,
        schema: schema.clone(),
        outer_width,
        inner_width: inner.schema().columns.len(),
        outer_tail: outer.row_id_count(),
        inner_tail: inner.row_id_count(),
        encoder: KeyEncoder::new(key_types.iter()),
        built: false,
        table: HashTable::new(),
        rows: Vec::new(),
        null_key_rows: Vec::new(),
        rows_since_cancel: 0,
        exhausted: false,
    }))
}

/// Appends the top-level conjunctions of `expr` to `out`.
fn split_and<'e>(expr: &'e BoundExpr, out: &mut Vec<&'e BoundExpr>) {
    match &expr.kind {
        BoundExprKind::Logical {
            op: LogicalOp::And,
            left,
            right,
        } => {
            split_and(left, out);
            split_and(right, out);
        }
        _ => out.push(expr),
    }
}

impl<'a> HashSemiJoin<'a> {
    /// Reads the inner input into the table when the first outer row after an `open`
    /// arrives: an outer input without rows leaves the inner one unread.
    /// Without keys nor `on`, the first inner row settles each outer row, and the reading
    /// stops there.
    fn build_table(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.built = true;
        let existence_only = self.keys.is_empty() && self.on.is_none();
        let inner = &mut self.inner;
        inner.open(ctx)?;
        let mut read: u64 = 0;
        while let Some(row) = inner.next(ctx)? {
            read += 1;
            if read.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                inner.close();
                self.exhausted = true;
                return Ok(());
            }
            let values = eval_side_keys(self.keys.iter().map(|(key, _)| key), &row, ctx)?;
            let Some(values) = values else {
                self.null_key_rows.push(row);
                continue;
            };
            let key = self.encoder.encode(&values)?;
            let (positions, _) = self.table.entry(key, || Ok(Vec::new()))?;
            positions.push(self.rows.len());
            self.rows.push(row);
            if existence_only {
                break;
            }
        }
        inner.close();
        Ok(())
    }

    /// The truth of the `on` for `outer` paired with `inner`; true without an `on`.
    fn holds(
        &self,
        outer: &Row,
        inner: &Row,
        ctx: &mut ExecContext<'a>,
    ) -> SqlResult<Option<bool>> {
        let Some(on) = &self.on else {
            return Ok(Some(true));
        };
        let joined = join_rows(
            &JoinSide {
                row: Some(outer),
                width: self.outer_width,
                tail: self.outer_tail,
            },
            &JoinSide {
                row: Some(inner),
                width: self.inner_width,
                tail: self.inner_tail,
            },
        );
        as_condition(&eval_expr(on, Some(&joined), ctx)?)
    }

    /// Whether `outer` passes the join.
    fn keeps(&mut self, outer: &Row, ctx: &mut ExecContext<'a>) -> SqlResult<bool> {
        let values = eval_side_keys(self.keys.iter().map(|(_, key)| key), outer, ctx)?;
        let bucket: Vec<usize> = match &values {
            Some(values) => {
                let key = self.encoder.encode(values)?;
                self.table
                    .find(key)
                    .and_then(|entry| self.table.get(entry))
                    .cloned()
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        if self.kind == PhysicalJoinKind::Semi {
            // A NULL key makes the equality unknown, so the `on` is not true.
            for position in bucket {
                if self.holds(outer, &self.rows[position], ctx)? == Some(true) {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        // AntiSemi: the candidates are the rows the key finds and those with a NULL key,
        // or each inner row when the outer key is NULL; any other row has a key that
        // differs, which makes the `on` false.
        let candidates: Vec<&Row> = if values.is_some() {
            bucket
                .iter()
                .map(|&position| &self.rows[position])
                .chain(self.null_key_rows.iter())
                .collect()
        } else {
            self.rows.iter().chain(self.null_key_rows.iter()).collect()
        };
        for inner in candidates {
            if self.holds(outer, inner, ctx)? != Some(false) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl<'a> Operator<'a> for HashSemiJoin<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.outer.open(ctx)?;
        self.rows_since_cancel = 0;
        self.exhausted = false;
        self.built = false;
        self.table = HashTable::new();
        self.rows.clear();
        self.null_key_rows.clear();
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        while !self.exhausted {
            let Some(row) = self.outer.next(ctx)? else {
                self.exhausted = true;
                break;
            };
            self.rows_since_cancel += 1;
            if self.rows_since_cancel.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.exhausted = true;
                break;
            }
            if !self.built {
                self.build_table(ctx)?;
                if self.exhausted {
                    break;
                }
            }
            if self.keeps(&row, ctx)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn close(&mut self) {
        self.outer.close();
        self.inner.close();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}
