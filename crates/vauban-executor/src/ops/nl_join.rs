//! `NestedLoopJoin`: runs `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS` and correlated seek.
//!
//! For each row of the outer input, the inner input is **reopened** and walked
//! (`CROSS`, `INNER`, `LEFT`), or the inner is **materialised** once at `open`
//! (`RIGHT`, `FULL`). A materialised inner is bounded by the size of the input — no
//! spill-to-disk in V1.
//!
//! # Correlated seek
//!
//! When the inner sub-tree is an [`IndexSeek`](crate::ops::seek::IndexSeek) whose bounds
//! reference the outer row, the join pushes the outer row into the context before opening
//! the inner, so that `eval_expr` resolves the column references against it.

use vauban_binder::{BoundExpr, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_planner::{PhysicalJoinKind, PhysicalPlan};
use vauban_types::Value;

use crate::context::{CANCEL_CHECK_ROWS, ExecContext};
use crate::expr::{as_condition, eval_expr};
use crate::operator::{Operator, build_operator};
use crate::row::Row;

/// Where the join stands between two `next()` calls.
pub enum Phase<'a> {
    /// Not opened, or exhausted: `next` answers `None`.
    Done,
    /// Waiting for the next outer row.
    NeedOuter,
    /// Walking the inner rows of the current outer row (`INNER`, `CROSS`, `LEFT`).
    /// The inner is open and the outer row is pushed in the context.
    Scanning {
        /// The inner operator, currently open.
        inner: Box<dyn Operator<'a> + 'a>,
        /// The outer row being matched against.
        outer_row: Row,
        /// Whether at least one match was found (for `LEFT`).
        had_match: bool,
    },
    /// Walking the materialised inner rows for the current outer row (`RIGHT`, `FULL`).
    ScanningMaterialised {
        /// The outer row being matched against.
        outer_row: Row,
        /// The next inner row index to try.
        pos: usize,
        /// Whether at least one match was found.
        had_match: bool,
    },
    /// Emitting unmatched outer rows (used in FULL JOIN).
    EmitUnmatchedOuter {
        /// The next outer row index to emit.
        pos: usize,
    },
    /// Emitting unmatched inner rows (RIGHT, FULL).
    EmitUnmatchedInner {
        /// The next inner row index to try.
        pos: usize,
    },
}

/// Runs a nested-loop join of any supported kind.
pub struct NestedLoopJoin<'a> {
    /// The outer (left) operator.
    pub outer: Box<dyn Operator<'a> + 'a>,
    /// The physical plan of the inner (right) side, rebuilt for each outer row in
    /// streamed mode.
    pub inner_plan: PhysicalPlan,
    /// The join kind.
    pub kind: PhysicalJoinKind,
    /// The join predicate, `None` for a cross join.
    pub on: Option<BoundExpr>,
    /// The schema of the output rows (outer columns then inner columns).
    pub schema: OutputSchema,
    /// The number of columns of the outer side.
    pub outer_width: usize,
    /// The number of columns of the inner side.
    pub inner_width: usize,
    /// Where the join stands between two `next()` calls.
    pub phase: Phase<'a>,
    /// Materialised inner rows for RIGHT / FULL.
    pub inner_rows: Vec<Row>,
    /// Whether each inner row had at least one match.
    pub matched: Vec<bool>,
    /// Unmatched outer rows saved for the FULL leftover phase.
    pub unmatched_outer: Vec<Row>,
    /// How many outer rows were pulled since the last cancellation check.
    pub rows_since_cancel: u64,
}

/// Builds the operator of a [`PhysicalPlan::NestedLoopJoin`].
///
/// # Errors
///
/// What building the outer raises.
pub(crate) fn build<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    let PhysicalPlan::NestedLoopJoin {
        outer,
        inner,
        kind,
        on,
        schema,
    } = plan
    else {
        return Err(SqlError::from(InternalError::Bug(
            "NestedLoopJoin: the node is not a NestedLoopJoin".to_owned(),
        )));
    };
    let outer_width = outer.schema().columns.len();
    let inner_width = inner.schema().columns.len();
    Ok(Box::new(NestedLoopJoin {
        outer: build_operator(outer)?,
        inner_plan: (**inner).clone(),
        kind: *kind,
        on: on.clone(),
        schema: schema.clone(),
        outer_width,
        inner_width,
        phase: Phase::Done,
        inner_rows: Vec::new(),
        matched: Vec::new(),
        unmatched_outer: Vec::new(),
        rows_since_cancel: 0,
    }))
}

impl<'a> NestedLoopJoin<'a> {
    /// Evaluates `on` on the combined row, or answers `Some(true)` for a cross join.
    fn check_on(
        on: &Option<BoundExpr>,
        combined: &Row,
        ctx: &mut ExecContext<'a>,
    ) -> SqlResult<Option<bool>> {
        match on {
            Some(predicate) => {
                let value = eval_expr(predicate, Some(combined), ctx)?;
                as_condition(&value)
            }
            None => Ok(Some(true)),
        }
    }

    /// Builds the concatenated row and returns it if `on` holds.
    fn pair(
        on: &Option<BoundExpr>,
        outer: &Row,
        inner: &Row,
        ctx: &mut ExecContext<'a>,
    ) -> SqlResult<Option<Row>> {
        let mut combined = Row::with_capacity(outer.len() + inner.len());
        combined.extend_from_slice(outer);
        combined.extend_from_slice(inner);
        match Self::check_on(on, &combined, ctx)? {
            None | Some(false) => Ok(None),
            Some(true) => Ok(Some(combined)),
        }
    }

    /// Pads the missing side with `Value::Null`.
    fn padded(
        outer: Option<&Row>,
        inner: Option<&Row>,
        outer_width: usize,
        inner_width: usize,
    ) -> Row {
        let mut row = Row::with_capacity(outer_width + inner_width);
        match outer {
            Some(r) => row.extend_from_slice(r),
            None => row.resize(outer_width, Value::Null),
        }
        match inner {
            Some(r) => row.extend_from_slice(r),
            None => row.resize(outer_width + inner_width, Value::Null),
        }
        row
    }

    /// Pulls and returns the next outer row, updating the cancellation counter.
    fn next_outer(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        match self.outer.next(ctx)? {
            Some(row) => {
                self.rows_since_cancel += 1;
                if self.rows_since_cancel.is_multiple_of(CANCEL_CHECK_ROWS) && ctx.cancelled() {
                    self.phase = Phase::Done;
                    return Ok(None);
                }
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    /// Checks whether the outer row had a match and handles the transition:
    /// - LEFT / FULL: saves unmatched outer rows for later emission.
    /// - No-op for RIGHT / INNER / CROSS.
    fn note_outer_done(
        kind: PhysicalJoinKind,
        outer_row: &Row,
        had_match: bool,
        unmatched_outer: &mut Vec<Row>,
    ) {
        if !had_match && (kind == PhysicalJoinKind::Left || kind == PhysicalJoinKind::Full) {
            unmatched_outer.push(outer_row.clone());
        }
    }
}

impl<'a> Operator<'a> for NestedLoopJoin<'a> {
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.outer.open(ctx)?;
        self.rows_since_cancel = 0;
        self.inner_rows.clear();
        self.matched.clear();
        self.unmatched_outer.clear();

        match self.kind {
            PhysicalJoinKind::Right | PhysicalJoinKind::Full => {
                let mut inner = build_operator(&self.inner_plan)?;
                inner.open(ctx)?;
                while let Some(row) = inner.next(ctx)? {
                    self.inner_rows.push(row);
                    if (self.inner_rows.len() as u64).is_multiple_of(CANCEL_CHECK_ROWS)
                        && ctx.cancelled()
                    {
                        inner.close();
                        self.phase = Phase::Done;
                        return Ok(());
                    }
                }
                inner.close();
                self.matched = vec![false; self.inner_rows.len()];
            }
            PhysicalJoinKind::Cross
            | PhysicalJoinKind::Inner
            | PhysicalJoinKind::Left
            | PhysicalJoinKind::Semi
            | PhysicalJoinKind::AntiSemi => {}
        }

        self.phase = Phase::NeedOuter;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        loop {
            match &mut self.phase {
                Phase::Done => return Ok(None),
                Phase::EmitUnmatchedInner { pos } => {
                    while *pos < self.inner_rows.len() {
                        let p = *pos;
                        *pos += 1;
                        if !self.matched[p] {
                            return Ok(Some(Self::padded(
                                None,
                                Some(&self.inner_rows[p]),
                                self.outer_width,
                                self.inner_width,
                            )));
                        }
                    }
                    self.phase = Phase::Done;
                    return Ok(None);
                }
                Phase::EmitUnmatchedOuter { pos } => {
                    if *pos < self.unmatched_outer.len() {
                        let row = Self::padded(
                            Some(&self.unmatched_outer[*pos]),
                            None,
                            self.outer_width,
                            self.inner_width,
                        );
                        *pos += 1;
                        return Ok(Some(row));
                    }
                    // Done with unmatched outer: move to unmatched inner.
                    if self.kind == PhysicalJoinKind::Full {
                        self.phase = Phase::EmitUnmatchedInner { pos: 0 };
                        continue;
                    }
                    self.phase = Phase::Done;
                    return Ok(None);
                }
                Phase::Scanning {
                    inner,
                    outer_row,
                    had_match,
                } => {
                    match inner.next(ctx)? {
                        Some(inner_row) => {
                            let holds = match self.kind {
                                PhysicalJoinKind::Cross => true,
                                PhysicalJoinKind::Semi | PhysicalJoinKind::AntiSemi => {
                                    Self::pair(&self.on, outer_row, &inner_row, ctx)?.is_some()
                                }
                                _ => Self::pair(&self.on, outer_row, &inner_row, ctx)?.is_some(),
                            };
                            if holds {
                                *had_match = true;
                                if self.kind == PhysicalJoinKind::Semi {
                                    inner.close();
                                    let _ = ctx.pop_outer();
                                    let row = outer_row.clone();
                                    self.phase = Phase::NeedOuter;
                                    return Ok(Some(row));
                                }
                                if self.kind == PhysicalJoinKind::AntiSemi {
                                    inner.close();
                                    let _ = ctx.pop_outer();
                                    self.phase = Phase::NeedOuter;
                                    continue;
                                }
                                if let Some(combined) =
                                    Self::pair(&self.on, outer_row, &inner_row, ctx)?
                                {
                                    return Ok(Some(combined));
                                }
                            }
                            continue;
                        }
                        None => {
                            // Inner exhausted.
                            inner.close();
                            let _ = ctx.pop_outer();
                            let matched = *had_match;
                            Self::note_outer_done(
                                self.kind,
                                outer_row,
                                matched,
                                &mut self.unmatched_outer,
                            );
                            if self.kind == PhysicalJoinKind::AntiSemi && !matched {
                                let row = outer_row.clone();
                                self.phase = Phase::NeedOuter;
                                return Ok(Some(row));
                            }
                            // LEFT: emit padded row if no match.
                            if self.kind == PhysicalJoinKind::Left && !matched {
                                let padded = Self::padded(
                                    Some(outer_row),
                                    None,
                                    self.outer_width,
                                    self.inner_width,
                                );
                                self.phase = Phase::NeedOuter;
                                return Ok(Some(padded));
                            }
                            self.phase = Phase::NeedOuter;
                            continue;
                        }
                    }
                }
                Phase::ScanningMaterialised {
                    outer_row,
                    pos,
                    had_match,
                } => {
                    while *pos < self.inner_rows.len() {
                        let p = *pos;
                        *pos += 1;
                        if let Some(combined) =
                            Self::pair(&self.on, outer_row, &self.inner_rows[p], ctx)?
                        {
                            self.matched[p] = true;
                            *had_match = true;
                            return Ok(Some(combined));
                        }
                    }
                    // This outer row has no more inner rows to check.
                    let matched = *had_match;
                    Self::note_outer_done(self.kind, outer_row, matched, &mut self.unmatched_outer);
                    outer_row.clear();
                    self.phase = Phase::NeedOuter;
                    continue;
                }
                Phase::NeedOuter => {
                    match self.next_outer(ctx)? {
                        Some(row) => {
                            match self.kind {
                                PhysicalJoinKind::Right | PhysicalJoinKind::Full => {
                                    self.phase = Phase::ScanningMaterialised {
                                        outer_row: row,
                                        pos: 0,
                                        had_match: false,
                                    };
                                }
                                PhysicalJoinKind::Semi
                                | PhysicalJoinKind::AntiSemi
                                | PhysicalJoinKind::Cross
                                | PhysicalJoinKind::Inner
                                | PhysicalJoinKind::Left => {
                                    ctx.push_outer(row.clone());
                                    let mut inner = build_operator(&self.inner_plan)?;
                                    inner.open(ctx)?;
                                    self.phase = Phase::Scanning {
                                        inner,
                                        outer_row: row,
                                        had_match: false,
                                    };
                                }
                            }
                            continue;
                        }
                        None => {
                            // Outer exhausted.
                            if self.kind == PhysicalJoinKind::Right
                                || self.kind == PhysicalJoinKind::Full
                            {
                                if !self.unmatched_outer.is_empty()
                                    && self.kind == PhysicalJoinKind::Full
                                {
                                    self.phase = Phase::EmitUnmatchedOuter { pos: 0 };
                                } else {
                                    self.phase = Phase::EmitUnmatchedInner { pos: 0 };
                                }
                                continue;
                            }
                            self.phase = Phase::Done;
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }

    fn close(&mut self) {
        self.outer.close();
        if let Phase::Scanning { inner, .. } = &mut self.phase {
            inner.close();
        }
        self.phase = Phase::Done;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}
