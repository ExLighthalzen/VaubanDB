#![deny(missing_docs)]
//! Crate `vauban-planner`: turns the bound logical plan into the physical plan the
//! `executor` runs.
//!
//! The `binder` says **what** to compute, this crate says **how**: which access path reads
//! a table, which algorithm pairs two inputs, in which order the operators are stacked.
//! It computes no value and reads no row: its whole view of the database is the one
//! question [`PlanCatalog`] answers, which is the single field of [`PlanContext`]
//! (`context.rs`).
//!
//! # What is translated, and what is not
//!
//! The nodes whose translation is one-to-one,
//! [`LogicalPlan::OneRow`](vauban_binder::LogicalPlan::OneRow),
//! `Values`, `Scan`, `Filter`, `Project` and `Limit`, become
//! [`PhysicalPlan::OneRow`], `Values`, [`TableScan`](PhysicalPlan::TableScan), `Filter`,
//! `Project` and [`Top`](PhysicalPlan::Top). A `Filter` over a scan becomes an
//! [`IndexSeek`](PhysicalPlan::IndexSeek) when its predicate covers a prefix of an index
//! of the scanned table (`seek.rs`; `tests/trivial.rs`,
//! `filter_project_over_scan_becomes_a_seek_under_the_project`). The six other nodes of
//! [`LogicalPlan`](vauban_binder::LogicalPlan) — `Join`,
//! `Aggregate`, `Sort`, `Distinct`, `SetOp`, `Subquery` — answer an internal error saying
//! they are not implemented yet (`tests/trivial.rs`, `join_is_not_implemented_yet`,
//! `aggregate_sort_and_distinct_are_not_implemented_yet`,
//! `a_subquery_expression_is_not_implemented_yet`,
//! `a_set_operator_is_not_implemented_yet`).
//!
//! # File plan
//!
//! `physical.rs` declares the variants of [`PhysicalPlan`]; the rule files produce them
//! and declare no type of their own.
//!
//! | File(s) | Holds |
//! |---|---|
//! | `physical.rs` | the types of the physical plan |
//! | `context.rs` | what a rule may read about the database |
//! | `plan.rs` | the translation, one operator per node |
//! | `explain.rs` | the textual form of a plan |
//! | `testing.rs` | the test double of [`PlanCatalog`] |
//! | `seek.rs` | an index seek in place of a filter |
//! | `join.rs` | the join operators |
//! | `aggregate.rs`, `sort.rs` | aggregation, ordering, top-N and `DISTINCT` |
//! | `subquery.rs` | subqueries |
//! | `dml.rs` | `INSERT`, `UPDATE` and `DELETE` |
//! | `setop.rs` | `UNION`, `EXCEPT` and `INTERSECT` |
//!
//! Each rule file holds the entry point `plan.rs` calls. Until its rule is written, that
//! entry point answers for its form: an internal error for a node that has to be built
//! ([`plan`] then fails), `Ok(None)` for a hook that rewrites a node `plan.rs` already
//! builds, as the top-N in place of a sort followed by a top (`sort::try_top_n`) still
//! does. The seek in place of a filter (`seek::try_index_seek`) is written.
//!
//! # Where the context comes from
//!
//! [`PlanContext`] carries one thing, [`PlanCatalog`], keyed on the
//! [`TableId`](vauban_storage::TableId) a bound `Scan` holds. [`StorageIndexes`] is the
//! production implementation, [`NoIndexes`] the one that reports no index, and
//! [`testing::FakeCatalog`] the double the rule tests declare their indexes with. The
//! reasons for this shape are in `context.rs`.

mod aggregate;
mod context;
mod dml;
mod explain;
mod join;
mod physical;
mod plan;
mod seek;
mod setop;
mod sort;
mod subquery;
pub mod testing;

pub use context::{NoIndexes, PlanCatalog, PlanContext, StorageIndexes};
pub use explain::explain;
pub use physical::{
    KeyRangeExpr, PhysicalDelete, PhysicalInsert, PhysicalJoinKind, PhysicalPlan,
    PhysicalStatement, PhysicalUpdate, SubPlan,
};
pub use plan::plan;
