//! `HashAggregate` and `StreamAggregate`: grouping and the aggregate functions.

use std::collections::HashMap;

use vauban_binder::{AggregateCall, BoundExpr, OutputSchema};
use vauban_errors::{InfoMessage, InternalError, SqlResult};
use vauban_planner::PhysicalPlan;
use vauban_sysfn::AggregateState;
use vauban_types::{Collation, SqlType, TypeInfo, Value, compare};

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::expr::eval_expr;
use crate::operator::{Operator, bucket_hash, build_operator};
use crate::row::Row;

/// Whether two grouping keys are equal: NULL equals NULL (unlike `=` semantics).
fn group_keys_equal(a: &[Value], b: &[Value], group_by: &[BoundExpr]) -> SqlResult<bool> {
    if a.len() != b.len() {
        return Ok(false);
    }
    for ((ak, bk), gb) in a.iter().zip(b.iter()).zip(group_by.iter()) {
        match (ak, bk) {
            (Value::Null, Value::Null) => {}
            (Value::Null, _) | (_, Value::Null) => return Ok(false),
            _ => {
                let collation = gb.ty.collation.as_ref().unwrap_or(&Collation::DEFAULT);
                if compare(ak, bk, collation)? != Some(std::cmp::Ordering::Equal) {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Hash of a grouping key.
fn group_hash(key: &[Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    if key.is_empty() {
        return 0;
    }
    let mut h = 0u64;
    for v in key {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match v {
            Value::Null => 0u8.hash(&mut hasher),
            other => bucket_hash(other).hash(&mut hasher),
        }
        h = h.wrapping_mul(31).wrapping_add(hasher.finish());
    }
    h
}

fn eval_keys(
    group_by: &[BoundExpr],
    row: &Row,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<Vec<Value>> {
    group_by
        .iter()
        .map(|expr| eval_expr(expr, Some(row), ctx))
        .collect()
}

/// A placeholder state that panics if step/finish is called (used as a dummy after
/// swapping out a real state in `Group::finish`).
struct UnreachableState;

impl AggregateState for UnreachableState {
    fn step(&mut self, _v: &Value) -> SqlResult<()> {
        panic!("UnreachableState::step called")
    }
    fn finish(self: Box<Self>) -> SqlResult<Value> {
        panic!("UnreachableState::finish called")
    }
}

struct Group {
    key: Vec<Value>,
    states: Vec<Box<dyn AggregateState>>,
    distinct_seen: Vec<Vec<Value>>,
}

impl Group {
    fn new(key: Vec<Value>, aggregates: &[AggregateCall]) -> SqlResult<Self> {
        let states = aggregates
            .iter()
            .map(|call| {
                let factory = call.def.aggregate.ok_or_else(|| {
                    InternalError::Bug(format!("{} is not an aggregate", call.def.name))
                })?;
                let arg_ty = match &call.arg {
                    Some(expr) => expr.ty.clone(),
                    None => TypeInfo::new(SqlType::Int, false),
                };
                factory(&arg_ty)
            })
            .collect::<SqlResult<Vec<_>>>()?;
        let distinct_seen = aggregates.iter().map(|_| Vec::new()).collect();
        Ok(Group {
            key,
            states,
            distinct_seen,
        })
    }

    fn accumulate(
        &mut self,
        row: &Row,
        aggregates: &[AggregateCall],
        ctx: &mut ExecContext<'_>,
        null_flag: &mut bool,
    ) -> SqlResult<()> {
        for (i, call) in aggregates.iter().enumerate() {
            let value = match &call.arg {
                Some(expr) => eval_expr(expr, Some(row), ctx)?,
                None => Value::I32(1),
            };
            if call.distinct {
                if self.distinct_seen[i].contains(&value) {
                    continue;
                }
                self.distinct_seen[i].push(value.clone());
            }
            self.states[i].step(&value)?;
            if !*null_flag && self.states[i].null_eliminated() {
                *null_flag = true;
            }
        }
        Ok(())
    }

    fn finish(&mut self, aggregates: &[AggregateCall], null_flag: &mut bool) -> SqlResult<Row> {
        let mut row = std::mem::take(&mut self.key);
        for (i, _call) in aggregates.iter().enumerate() {
            let state = std::mem::replace(&mut self.states[i], Box::new(UnreachableState));
            if !*null_flag && state.null_eliminated() {
                *null_flag = true;
            }
            let value = state.finish()?;
            row.push(value);
        }
        Ok(row)
    }
}

// -----------------------------------------------------------------------
// HashAggregate — consume the input in open, finish groups, emit rows in next
// -----------------------------------------------------------------------

enum HashState {
    Pending,
    Ready { rows: Vec<Row>, index: usize },
    Exhausted,
}

struct HashAggregate<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    group_by: Vec<BoundExpr>,
    aggregates: Vec<AggregateCall>,
    schema: OutputSchema,
    state: HashState,
    input_open: bool,
}

fn hash_bucket<'a>(
    buckets: &'a mut HashMap<u64, Vec<Group>>,
    key: Vec<Value>,
    hash: u64,
    aggregates: &[AggregateCall],
    group_by: &[BoundExpr],
) -> SqlResult<&'a mut Group> {
    let bucket = buckets.entry(hash).or_default();
    if key.is_empty() && group_by.is_empty() {
        if bucket.is_empty() {
            bucket.push(Group::new(key, aggregates)?);
        }
        return Ok(&mut bucket[0]);
    }
    let pos = bucket
        .iter()
        .position(|g| group_keys_equal(&g.key, &key, group_by).unwrap_or(false));
    if let Some(idx) = pos {
        Ok(&mut bucket[idx])
    } else {
        bucket.push(Group::new(key, aggregates)?);
        Ok(bucket.last_mut().unwrap())
    }
}

impl<'a> Operator<'a> for HashAggregate<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)?;
        self.input_open = true;

        let mut buckets: HashMap<u64, Vec<Group>> = HashMap::new();
        let mut null_flag = false;

        while let Some(row) = self.input.next(ctx)? {
            let key = eval_keys(&self.group_by, &row, ctx)?;
            let hash = group_hash(&key);
            let group = hash_bucket(&mut buckets, key, hash, &self.aggregates, &self.group_by)?;
            group.accumulate(&row, &self.aggregates, ctx, &mut null_flag)?;

            if (row.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.state = HashState::Exhausted;
                return Ok(());
            }
        }

        let mut rows: Vec<Row> = Vec::new();
        if self.group_by.is_empty() && buckets.is_empty() {
            // No GROUP BY, empty input → one row with COUNT=0, others NULL.
            let mut group = Group::new(Vec::new(), &self.aggregates)?;
            let row = group.finish(&self.aggregates, &mut null_flag)?;
            rows.push(row);
        }

        let mut bucket_list: Vec<_> = buckets.into_iter().collect();
        bucket_list.sort_by_key(|(h, _)| *h);
        for (_hash, mut bucket) in bucket_list {
            for group in &mut bucket {
                let row = group.finish(&self.aggregates, &mut null_flag)?;
                rows.push(row);
            }
        }

        if null_flag {
            ctx.emit_info(InfoMessage::null_eliminated());
        }

        self.state = HashState::Ready { rows, index: 0 };
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match &mut self.state {
            HashState::Pending => {
                Err(InternalError::Bug("HashAggregate::next called before open".into()).into())
            }
            HashState::Exhausted => Ok(None),
            HashState::Ready { rows, index } => {
                if *index < rows.len() {
                    let row = std::mem::take(&mut rows[*index]);
                    *index += 1;
                    Ok(Some(row))
                } else {
                    self.state = HashState::Exhausted;
                    Ok(None)
                }
            }
        }
    }

    fn close(&mut self) {
        if self.input_open {
            self.input.close();
            self.input_open = false;
        }
        self.state = HashState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

// -----------------------------------------------------------------------
// StreamAggregate — emite one group per next() call
// -----------------------------------------------------------------------

/// A row peeked from the input that starts a new group: the row and its key.
struct PeekedGroup {
    row: Row,
    key: Vec<Value>,
}

enum StreamState {
    Started,
    Peeked(PeekedGroup),
    Exhausted,
}

struct StreamAggregate<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    group_by: Vec<BoundExpr>,
    aggregates: Vec<AggregateCall>,
    schema: OutputSchema,
    state: StreamState,
    input_open: bool,
    null_emitted: bool,
    null_flag: bool,
}

impl<'a> StreamAggregate<'a> {
    /// Accumulates rows of one group starting with `first_row`/`first_key`.
    /// When a key change is detected or the input is exhausted, finishes the group
    /// and returns its output row. If the input has a next row belonging to a different
    /// group, it is saved in `self.state`.
    fn emit_one_group(
        &mut self,
        mut group: Group,
        first_row: Row,
        ctx: &mut ExecContext<'a>,
    ) -> SqlResult<Row> {
        group.accumulate(&first_row, &self.aggregates, ctx, &mut self.null_flag)?;

        loop {
            let Some(next_row) = self.input.next(ctx)? else {
                self.state = StreamState::Exhausted;
                return group.finish(&self.aggregates, &mut self.null_flag);
            };
            let next_key = eval_keys(&self.group_by, &next_row, ctx)?;

            if !self.group_by.is_empty()
                && !group_keys_equal(&group.key, &next_key, &self.group_by)?
            {
                // Key change: save the peeked row for the next group.
                self.state = StreamState::Peeked(PeekedGroup {
                    row: next_row,
                    key: next_key,
                });
                return group.finish(&self.aggregates, &mut self.null_flag);
            }
            group.accumulate(&next_row, &self.aggregates, ctx, &mut self.null_flag)?;
        }
    }
}

impl<'a> Operator<'a> for StreamAggregate<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.input.open(ctx)?;
        self.input_open = true;
        self.state = StreamState::Started;
        self.null_emitted = false;
        self.null_flag = false;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match &mut self.state {
            StreamState::Exhausted => return Ok(None),
            StreamState::Started => {}
            StreamState::Peeked(_) => {}
        }

        // Take ownership out of self.state.
        let (row, key) = match std::mem::replace(&mut self.state, StreamState::Exhausted) {
            StreamState::Peeked(p) => (p.row, p.key),
            StreamState::Started => {
                let Some(first_row) = self.input.next(ctx)? else {
                    if self.group_by.is_empty() {
                        let mut group = Group::new(Vec::new(), &self.aggregates)?;
                        let row = group.finish(&self.aggregates, &mut self.null_flag)?;
                        if self.null_flag && !self.null_emitted {
                            ctx.emit_info(InfoMessage::null_eliminated());
                            self.null_emitted = true;
                        }
                        return Ok(Some(row));
                    }
                    return Ok(None);
                };
                let key = eval_keys(&self.group_by, &first_row, ctx)?;
                let group = Group::new(key, &self.aggregates)?;
                let row = self.emit_one_group(group, first_row, ctx)?;
                if self.null_flag && !self.null_emitted {
                    ctx.emit_info(InfoMessage::null_eliminated());
                    self.null_emitted = true;
                }
                return Ok(Some(row));
            }
            StreamState::Exhausted => return Ok(None),
        };

        // We have a peeked row that belongs to a new group.
        let group = Group::new(key, &self.aggregates)?;
        let result_row = self.emit_one_group(group, row, ctx)?;
        if self.null_flag && !self.null_emitted {
            ctx.emit_info(InfoMessage::null_eliminated());
            self.null_emitted = true;
        }
        Ok(Some(result_row))
    }

    fn close(&mut self) {
        if self.input_open {
            self.input.close();
            self.input_open = false;
        }
        self.state = StreamState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// Builds the operator of a [`PhysicalPlan::HashAggregate`] or [`PhysicalPlan::StreamAggregate`].
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    match plan {
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => Ok(Box::new(HashAggregate {
            input: build_operator(input)?,
            group_by: group_by.clone(),
            aggregates: aggregates.clone(),
            schema: schema.clone(),
            state: HashState::Pending,
            input_open: false,
        })),
        PhysicalPlan::StreamAggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => Ok(Box::new(StreamAggregate {
            input: build_operator(input)?,
            group_by: group_by.clone(),
            aggregates: aggregates.clone(),
            schema: schema.clone(),
            state: StreamState::Started,
            input_open: false,
            null_emitted: false,
            null_flag: false,
        })),
        _ => Err(
            InternalError::Bug("aggregate::build called for a non-aggregate node".into()).into(),
        ),
    }
}
