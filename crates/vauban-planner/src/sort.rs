//! Ordering: [`Sort`](PhysicalPlan::Sort), the sort an index makes unnecessary,
//! [`TopN`](PhysicalPlan::TopN) when a `TOP` sits over an `ORDER BY`, and
//! [`Distinct`](PhysicalPlan::Distinct).
//!
//! # The order a node delivers
//!
//! Two rules ask the same question, "in which order do the rows of this input arrive":
//! the one below, which drops a sort the input already answers, and the aggregate rule,
//! which streams a group instead of hashing it (`aggregate.rs`). The answer is a list of
//! keys, most significant first, and it has two shapes: a `Sort` or a `TopN` answers the
//! expressions it ordered on, an [`IndexSeek`](PhysicalPlan::IndexSeek) answers the
//! columns its index is keyed on. A node that orders nothing answers the empty list, and
//! a bare [`TableScan`](PhysicalPlan::TableScan) is one of them: the rows arrive as
//! `storage` hands them out (`tests/aggregate_sort.rs`, `a_table_scan_delivers_no_order`).
//!
//! A seek names its index and not its table, while the key columns are read from the
//! catalogue by table, so the question is asked with the bound node the plan was built
//! from: the table is the one the `Scan` under it carries.
//!
//! # Dropping the sort
//!
//! The `Sort` disappears when the keys asked for are the leading keys of the order the
//! input delivers, expression and direction alike (`tests/aggregate_sort.rs`,
//! `a_sort_the_index_already_delivers_is_dropped`); a key on another column keeps it
//! (`tests/aggregate_sort.rs`, `a_sort_on_another_column_is_kept`).
//!
//! When each key asked for is the opposite of the one delivered, the rows come out in the
//! order wanted by reading the index backwards, and the `Sort` disappears too
//! (`seek::flip_direction`; `tests/aggregate_sort.rs`,
//! `a_descending_sort_flips_the_seek_direction`). One key reversed out of two is neither
//! order, and the `Sort` stays (`tests/aggregate_sort.rs`,
//! `a_partial_reversal_keeps_the_sort`).

use vauban_binder::{BoundExpr, BoundExprKind, BoundTop, LogicalPlan, SortKey};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{Direction, TableId};
use vauban_types::Collation;

use crate::context::PlanContext;
use crate::physical::PhysicalPlan;
use crate::plan::plan_node;
use crate::seek;

/// Plans a [`LogicalPlan::Sort`] into the operator that orders its rows, or into its
/// input when that input delivers the order already.
pub(crate) fn plan_sort(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Sort {
        input: source,
        keys,
    } = plan
    else {
        return Err(bug("plan_sort: expected a Sort plan"));
    };
    let mut input = plan_node(source, ctx)?;
    if keys.is_empty() {
        return Ok(input);
    }
    let (delivered_as_asked, delivered_reversed) = {
        let delivered = delivered_order(&input, source, ctx);
        (
            leads(&delivered, keys, false),
            leads(&delivered, keys, true),
        )
    };
    if delivered_as_asked {
        return Ok(input);
    }
    if delivered_reversed && seek::flip_direction(&mut input) {
        return Ok(input);
    }
    Ok(PhysicalPlan::Sort {
        input: Box::new(input),
        keys: keys.clone(),
    })
}

/// Plans a [`LogicalPlan::Distinct`] into the operator that removes its duplicate rows.
pub(crate) fn plan_distinct(plan: &LogicalPlan, ctx: &PlanContext<'_>) -> SqlResult<PhysicalPlan> {
    let LogicalPlan::Distinct(input) = plan else {
        return Err(bug("plan_distinct: expected a Distinct plan"));
    };
    let input = plan_node(input, ctx)?;
    Ok(PhysicalPlan::Distinct(Box::new(input)))
}

/// Tries to turn a `TOP` over an ordered input into a single [`PhysicalPlan::TopN`].
pub(crate) fn try_top_n(
    top: &BoundTop,
    input: &LogicalPlan,
    ctx: &PlanContext<'_>,
) -> SqlResult<Option<PhysicalPlan>> {
    let LogicalPlan::Sort {
        input: sort_input,
        keys,
    } = input
    else {
        return Ok(None);
    };
    let input = plan_node(sort_input, ctx)?;
    Ok(Some(PhysicalPlan::TopN {
        input: Box::new(input),
        keys: keys.clone(),
        top: top.clone(),
    }))
}

/// Whether the rows `planned` delivers arrive grouped on `group_by`.
///
/// True when the grouping expressions are the leading keys of the order `planned`
/// delivers, in the same positions: the rows that share a key then arrive together, which
/// is what a [`StreamAggregate`](PhysicalPlan::StreamAggregate) reads. The direction of
/// each key is not looked at, since a group is contiguous whichever way its values run.
///
/// `source` is the bound node `planned` was built from, which the order of a seek is read
/// through. An empty `group_by` is false: there is one group and nothing to order on.
pub(crate) fn arrives_grouped_on(
    planned: &PhysicalPlan,
    source: &LogicalPlan,
    group_by: &[BoundExpr],
    ctx: &PlanContext<'_>,
) -> bool {
    if group_by.is_empty() {
        return false;
    }
    let delivered = delivered_order(planned, source, ctx);
    group_by.len() <= delivered.len()
        && delivered
            .iter()
            .zip(group_by)
            .all(|(key, expr)| key.orders_on(expr))
}

/// One key of the order a node delivers: what the rows are ordered on, and which way the
/// values run.
struct DeliveredKey<'a> {
    /// What the rows are ordered on.
    by: OrderedBy<'a>,
    /// True when the values run from the highest to the lowest.
    desc: bool,
    /// The collation a `Sort` or a `TopN` ordered on, `None` when the key used the
    /// collation of its expression.
    collation: Option<Collation>,
}

/// What a delivered key orders the rows on.
enum OrderedBy<'a> {
    /// The expression a `Sort` or a `TopN` ordered the rows on.
    Expr(&'a BoundExpr),
    /// The 0-based position, in the row, of a column an index is keyed on.
    Column(usize),
}

impl DeliveredKey<'_> {
    /// Whether this key orders the rows on `expr`, whichever way the values run.
    fn orders_on(&self, expr: &BoundExpr) -> bool {
        match self.by {
            OrderedBy::Expr(delivered) => exprs_equal(delivered, expr),
            OrderedBy::Column(position) => {
                matches!(&expr.kind, BoundExprKind::ColumnRef(binding) if binding.index == position)
            }
        }
    }

    /// Whether this key answers `required`, read the other way round when `reversed`.
    fn answers(&self, required: &SortKey, reversed: bool) -> bool {
        if (self.desc != reversed) != required.desc {
            return false;
        }
        if required.collation != self.collation {
            return false;
        }
        match self.by {
            OrderedBy::Expr(_) => self.orders_on(&required.expr),
            // An index keys a column under the collation that column carries, so a key
            // written `COLLATE …` asks for an order the index does not deliver.
            OrderedBy::Column(_) => required.collation.is_none() && self.orders_on(&required.expr),
        }
    }
}

/// The order `planned` delivers, most significant key first.
///
/// `source` is the bound node `planned` was built from, which the table of a seek is read
/// from. The empty vector for a node that delivers no order of its own.
fn delivered_order<'a>(
    planned: &'a PhysicalPlan,
    source: &LogicalPlan,
    ctx: &PlanContext<'_>,
) -> Vec<DeliveredKey<'a>> {
    match planned {
        PhysicalPlan::Sort { keys, .. } | PhysicalPlan::TopN { keys, .. } => keys
            .iter()
            .map(|key| DeliveredKey {
                by: OrderedBy::Expr(&key.expr),
                desc: key.desc,
                collation: key.collation,
            })
            .collect(),
        PhysicalPlan::IndexSeek {
            index, direction, ..
        } => {
            let Some(table) = scanned_table(source) else {
                return Vec::new();
            };
            let backwards = matches!(direction, Direction::Backward);
            seek::index_key(*index, table, ctx)
                .iter()
                .map(|key| DeliveredKey {
                    by: OrderedBy::Column(usize::from(key.column)),
                    desc: key.descending != backwards,
                    collation: None,
                })
                .collect()
        }
        PhysicalPlan::Filter { input, .. } | PhysicalPlan::Top { input, .. } => {
            delivered_order(input, source, ctx)
        }
        _ => Vec::new(),
    }
}

/// The table a bound node reads, when a chain of nodes with one input each leads to a
/// single [`Scan`](LogicalPlan::Scan).
///
/// `None` where that chain stops on a node that reads no table or two of them: the
/// identifier is wanted to look an index up, and another table would answer another
/// index of the same identifier.
fn scanned_table(plan: &LogicalPlan) -> Option<TableId> {
    match plan {
        LogicalPlan::Scan { table, .. } => Some(*table),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Subquery { input, .. } => scanned_table(input),
        LogicalPlan::Distinct(input) => scanned_table(input),
        LogicalPlan::OneRow
        | LogicalPlan::Values { .. }
        | LogicalPlan::Join { .. }
        | LogicalPlan::SetOp { .. } => None,
    }
}

/// Whether the keys `required` asks for are the leading keys of `delivered`, each one
/// read the other way round when `reversed`.
fn leads(delivered: &[DeliveredKey<'_>], required: &[SortKey], reversed: bool) -> bool {
    required.len() <= delivered.len()
        && delivered
            .iter()
            .zip(required)
            .all(|(key, want)| key.answers(want, reversed))
}

/// Whether two bound expressions are structurally identical.
fn exprs_equal(a: &BoundExpr, b: &BoundExpr) -> bool {
    match (&a.kind, &b.kind) {
        (BoundExprKind::Literal(va), BoundExprKind::Literal(vb)) => va == vb,
        (BoundExprKind::ColumnRef(ca), BoundExprKind::ColumnRef(cb)) => ca.index == cb.index,
        (
            BoundExprKind::Arith {
                op: oa,
                left: la,
                right: ra,
            },
            BoundExprKind::Arith {
                op: ob,
                left: lb,
                right: rb,
            },
        ) => oa == ob && exprs_equal(la, lb) && exprs_equal(ra, rb),
        _ => false,
    }
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
