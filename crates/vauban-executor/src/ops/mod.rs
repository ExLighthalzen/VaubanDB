//! The operators, one file each: the [`Operator`](crate::Operator) implementations that
//! [`build_operator`](crate::build_operator) dispatches to.
//!
//! | File | Node(s) of [`PhysicalPlan`](vauban_planner::PhysicalPlan) |
//! |---|---|
//! | `values.rs` | `OneRow`, `Values` |
//! | `scan.rs` | `TableScan` |
//! | `seek.rs` | `IndexSeek` |
//! | `filter.rs` | `Filter` |
//! | `project.rs` | `Project` |
//! | `limit.rs` | `Top` |
//! | `nl_join.rs` | `NestedLoopJoin` |
//! | `hash_join.rs` | `HashJoin` |
//! | `aggregate.rs` | `HashAggregate`, `StreamAggregate` |
//! | `sort.rs` | `Sort`, `TopN`, `Distinct` |
//! | `subquery.rs` | `SubqueryEval` |
//! | `setop.rs` | `Union`, `Except`, `Intersect` |
//!
//! The `build` function of a file whose operator is not written yet answers the internal
//! error 50000 ([`not_implemented`]).

use vauban_errors::{InternalError, SqlError};

pub(crate) mod aggregate;
pub(crate) mod filter;
pub(crate) mod hash_join;
pub mod limit;
pub mod nl_join;
pub(crate) mod project;
pub(crate) mod scan;
pub(crate) mod seek;
pub(crate) mod setop;
pub(crate) mod sort;
pub(crate) mod subquery;
pub(crate) mod values;

/// The internal error 50000 a node answers until its operator is written: the node, as
/// [`build_operator`](crate::build_operator) names it.
pub(crate) fn not_implemented(node: &str) -> SqlError {
    SqlError::from(InternalError::Bug(format!(
        "build_operator: {node} is not implemented yet"
    )))
}
