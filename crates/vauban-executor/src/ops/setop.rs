//! `Union`, `Except` and `Intersect`: the set operators.
//!
//! `UNION [ALL]` streams its operands in order without materialising them. `EXCEPT` and
//! `INTERSECT` materialise the right operand into a hash table keyed by
//! [`rows_equal`](rows_equal), where two `NULL` values compare equal for set membership
//! (`tests/setop.rs`, `nulls_are_equal_for_union_and_intersect`), and stream the left
//! operand with an implicit `DISTINCT` on the answer
//! (`tests/setop.rs`, `except_does_not_repeat_left_duplicates`).

use vauban_binder::OutputSchema;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::PhysicalPlan;
use vauban_types::{TypeInfo, Value};

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::operator::{Operator, bucket_hash, build_operator, keys_equal};
use crate::row::Row;

// -----------------------------------------------------------------------
// UNION ALL
// -----------------------------------------------------------------------

/// `UNION ALL` over its operands: each input is opened only when the previous one is
/// exhausted (`tests/setop.rs`, `union_all_streams_before_opening_the_next_input`).
pub(crate) struct UnionAll<'a> {
    inputs: Vec<Box<dyn Operator<'a> + 'a>>,
    schema: OutputSchema,
    current: usize,
    current_open: bool,
}

impl<'a> UnionAll<'a> {
    fn new(inputs: Vec<Box<dyn Operator<'a> + 'a>>, schema: OutputSchema) -> Self {
        Self {
            inputs,
            schema,
            current: 0,
            current_open: false,
        }
    }
}

impl<'a> Operator<'a> for UnionAll<'a> {
    fn open(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.current = 0;
        self.current_open = false;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        loop {
            if self.current >= self.inputs.len() {
                return Ok(None);
            }
            if !self.current_open {
                self.inputs[self.current].open(ctx)?;
                self.current_open = true;
            }
            match self.inputs[self.current].next(ctx)? {
                Some(row) => return Ok(Some(row)),
                None => {
                    self.inputs[self.current].close();
                    self.current_open = false;
                    self.current += 1;
                }
            }
        }
    }

    fn close(&mut self) {
        if self.current_open && self.current < self.inputs.len() {
            self.inputs[self.current].close();
            self.current_open = false;
        }
        self.current = self.inputs.len();
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

// -----------------------------------------------------------------------
// EXCEPT and INTERSECT
// -----------------------------------------------------------------------

/// `EXCEPT`: rows of the left operand absent from the right, one row per distinct value
/// on the left.
struct Except<'a> {
    left: Box<dyn Operator<'a> + 'a>,
    right: Box<dyn Operator<'a> + 'a>,
    schema: OutputSchema,
    right_rows: RowSet,
    emitted: RowSet,
    state: SideSetOpState,
    left_open: bool,
}

/// `INTERSECT`: rows of the left operand present on the right, one row per distinct value.
struct Intersect<'a> {
    left: Box<dyn Operator<'a> + 'a>,
    right: Box<dyn Operator<'a> + 'a>,
    schema: OutputSchema,
    right_rows: RowSet,
    emitted: RowSet,
    state: SideSetOpState,
    left_open: bool,
}

enum SideSetOpState {
    Pending,
    Streaming,
    Exhausted,
}

impl<'a> Except<'a> {
    fn new(
        left: Box<dyn Operator<'a> + 'a>,
        right: Box<dyn Operator<'a> + 'a>,
        schema: OutputSchema,
    ) -> Self {
        let types: Vec<_> = schema.columns.iter().map(|c| c.ty.clone()).collect();
        Self {
            left,
            right,
            right_rows: RowSet::new(types.clone()),
            emitted: RowSet::new(types),
            schema,
            state: SideSetOpState::Pending,
            left_open: false,
        }
    }
}

impl<'a> Intersect<'a> {
    fn new(
        left: Box<dyn Operator<'a> + 'a>,
        right: Box<dyn Operator<'a> + 'a>,
        schema: OutputSchema,
    ) -> Self {
        let types: Vec<_> = schema.columns.iter().map(|c| c.ty.clone()).collect();
        Self {
            left,
            right,
            right_rows: RowSet::new(types.clone()),
            emitted: RowSet::new(types),
            schema,
            state: SideSetOpState::Pending,
            left_open: false,
        }
    }
}

impl<'a> Operator<'a> for Except<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.right.open(ctx)?;
        let mut count = 0u64;
        while let Some(row) = self.right.next(ctx)? {
            self.right_rows.insert(row);
            count += 1;
            if count.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.right.close();
                self.state = SideSetOpState::Exhausted;
                return Ok(());
            }
        }
        self.right.close();
        self.state = SideSetOpState::Streaming;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match self.state {
            SideSetOpState::Pending => Err(bug("Except::next called before open")),
            SideSetOpState::Exhausted => Ok(None),
            SideSetOpState::Streaming => {
                if !self.left_open {
                    self.left.open(ctx)?;
                    self.left_open = true;
                }
                let mut count = 0u64;
                while let Some(row) = self.left.next(ctx)? {
                    if !self.right_rows.contains(&row) && self.emitted.insert(row.clone()) {
                        return Ok(Some(row));
                    }
                    count += 1;
                    if count.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                        self.state = SideSetOpState::Exhausted;
                        return Ok(None);
                    }
                }
                self.state = SideSetOpState::Exhausted;
                Ok(None)
            }
        }
    }

    fn close(&mut self) {
        if self.left_open {
            self.left.close();
            self.left_open = false;
        }
        self.state = SideSetOpState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

impl<'a> Operator<'a> for Intersect<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.right.open(ctx)?;
        let mut count = 0u64;
        while let Some(row) = self.right.next(ctx)? {
            self.right_rows.insert(row);
            count += 1;
            if count.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                self.right.close();
                self.state = SideSetOpState::Exhausted;
                return Ok(());
            }
        }
        self.right.close();
        self.state = SideSetOpState::Streaming;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match self.state {
            SideSetOpState::Pending => Err(bug("Intersect::next called before open")),
            SideSetOpState::Exhausted => Ok(None),
            SideSetOpState::Streaming => {
                if !self.left_open {
                    self.left.open(ctx)?;
                    self.left_open = true;
                }
                let mut count = 0u64;
                while let Some(row) = self.left.next(ctx)? {
                    if self.right_rows.contains(&row) && self.emitted.insert(row.clone()) {
                        return Ok(Some(row));
                    }
                    count += 1;
                    if count.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                        self.state = SideSetOpState::Exhausted;
                        return Ok(None);
                    }
                }
                self.state = SideSetOpState::Exhausted;
                Ok(None)
            }
        }
    }

    fn close(&mut self) {
        if self.left_open {
            self.left.close();
            self.left_open = false;
        }
        self.state = SideSetOpState::Exhausted;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

// -----------------------------------------------------------------------
// Row set
// -----------------------------------------------------------------------

/// A set of rows compared with set semantics: two `NULL` values are equal.
struct RowSet {
    buckets: Vec<Vec<Row>>,
    types: Vec<TypeInfo>,
}

impl RowSet {
    fn new(types: Vec<TypeInfo>) -> Self {
        Self {
            buckets: Vec::new(),
            types,
        }
    }

    fn insert(&mut self, row: Row) -> bool {
        if self.contains(&row) {
            return false;
        }
        let hash = row_hash(&row, &self.types);
        let bucket_idx = (hash as usize) % 1024;
        if bucket_idx >= self.buckets.len() {
            self.buckets.resize(bucket_idx + 1, Vec::new());
        }
        self.buckets[bucket_idx].push(row);
        true
    }

    fn contains(&self, row: &Row) -> bool {
        let hash = row_hash(row, &self.types);
        let bucket_idx = (hash as usize) % 1024;
        if bucket_idx >= self.buckets.len() {
            return false;
        }
        self.buckets[bucket_idx]
            .iter()
            .any(|seen| rows_equal(seen, row, &self.types))
    }
}

/// Whether two rows are equal on each column for set membership.
///
/// Two `NULL` values are equal here, which differs from [`keys_equal`] alone.
fn rows_equal(a: &Row, b: &Row, types: &[TypeInfo]) -> bool {
    for (i, ty) in types.iter().enumerate() {
        let Some(va) = a.get(i) else {
            return false;
        };
        let Some(vb) = b.get(i) else {
            return false;
        };
        if matches!(va, Value::Null) && matches!(vb, Value::Null) {
            continue;
        }
        if !keys_equal(va, vb, ty).unwrap_or(false) {
            return false;
        }
    }
    true
}

fn row_hash(row: &Row, _types: &[TypeInfo]) -> u64 {
    let mut h: u64 = 0;
    for val in row.iter() {
        h = h.wrapping_mul(31).wrapping_add(bucket_hash(val));
    }
    h
}

// -----------------------------------------------------------------------
// Build
// -----------------------------------------------------------------------

/// Builds the operator for a [`PhysicalPlan::Union`], [`PhysicalPlan::Except`] or
/// [`PhysicalPlan::Intersect`].
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    match plan {
        PhysicalPlan::Union { inputs, schema, .. } => {
            let built: SqlResult<Vec<_>> = inputs.iter().map(build_operator).collect();
            Ok(Box::new(UnionAll::new(built?, schema.clone())))
        }
        PhysicalPlan::Except { inputs, schema, .. } => {
            let (left, right) = binary_inputs(inputs)?;
            Ok(Box::new(Except::new(
                build_operator(left)?,
                build_operator(right)?,
                schema.clone(),
            )))
        }
        PhysicalPlan::Intersect { inputs, schema, .. } => {
            let (left, right) = binary_inputs(inputs)?;
            Ok(Box::new(Intersect::new(
                build_operator(left)?,
                build_operator(right)?,
                schema.clone(),
            )))
        }
        _ => Err(SqlError::from(InternalError::Bug(
            "setop::build: expected Union, Except or Intersect".to_owned(),
        ))),
    }
}

fn binary_inputs(inputs: &[PhysicalPlan]) -> SqlResult<(&PhysicalPlan, &PhysicalPlan)> {
    let [left, right] = inputs else {
        return Err(SqlError::from(InternalError::Bug(format!(
            "setop: expected two operands, got {}",
            inputs.len()
        ))));
    };
    Ok((left, right))
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use vauban_binder::{BoundExpr, BoundExprKind, OutputColumn, OutputSchema, SessionOptions};
    use vauban_sysfn::StaticContext;
    use vauban_types::{SqlType, TypeInfo, Value};

    use super::*;

    fn int_t() -> TypeInfo {
        TypeInfo::new(SqlType::Int, true)
    }

    fn values_plan(vals: &[Value]) -> PhysicalPlan {
        let rows: Vec<Vec<BoundExpr>> = vals
            .iter()
            .map(|v| {
                vec![BoundExpr {
                    kind: BoundExprKind::Literal(v.clone()),
                    ty: int_t(),
                    line: 1,
                }]
            })
            .collect();
        PhysicalPlan::Values {
            rows,
            schema: OutputSchema {
                columns: vec![OutputColumn {
                    name: "v".to_owned(),
                    ty: int_t(),
                }],
            },
        }
    }

    struct FlaggedValues<'a> {
        inner: Box<dyn Operator<'a> + 'a>,
        opened: Rc<Cell<bool>>,
    }

    impl<'a> Operator<'a> for FlaggedValues<'a> {
        fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
            self.opened.set(true);
            self.inner.open(ctx)
        }

        fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
            self.inner.next(ctx)
        }

        fn close(&mut self) {
            self.inner.close();
        }

        fn schema(&self) -> &OutputSchema {
            self.inner.schema()
        }
    }

    #[test]
    fn union_all_streams_before_opening_the_next_input() {
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());

        let right_opened = Rc::new(Cell::new(false));
        let left = build_operator(&values_plan(&[Value::I32(1)])).expect("left builds");
        let right = FlaggedValues {
            inner: build_operator(&values_plan(&[Value::I32(2)])).expect("right builds"),
            opened: Rc::clone(&right_opened),
        };
        let mut union = UnionAll::new(
            vec![left, Box::new(right)],
            OutputSchema {
                columns: vec![OutputColumn {
                    name: "v".to_owned(),
                    ty: int_t(),
                }],
            },
        );

        union.open(&mut ctx).expect("opens");
        assert!(!right_opened.get());
        let first = union.next(&mut ctx).expect("a row").expect("the first row");
        assert_eq!(first, vec![Value::I32(1)]);
        assert!(!right_opened.get());
        let second = union
            .next(&mut ctx)
            .expect("a row")
            .expect("the second row");
        assert_eq!(second, vec![Value::I32(2)]);
        assert!(right_opened.get());
        assert!(union.next(&mut ctx).expect("a row").is_none());
        union.close();
    }
}
