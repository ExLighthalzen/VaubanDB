//! The textual form of a physical plan, for the tests of the module and for diagnosis.
//!
//! One line per node; the set operators are the nodes that hold a vector of inputs rather
//! than one or two.
//!
//! # Not a showplan
//!
//! These strings do not go to a client and do not follow the output of
//! `SET SHOWPLAN_TEXT`. The operator names borrow from the showplan vocabulary without
//! matching it; the layout is this crate's own.

use std::fmt::Write;

use crate::physical::{KeyRangeExpr, PhysicalPlan};

/// Writes `plan` as one line per node, each indented by two spaces per level.
///
/// The root is written at indentation zero and the lines are joined by `\n`, with no
/// trailing newline: a `Project` over a `Filter` over a `TableScan` is three lines
/// indented by 0, 2 and 4 spaces (`tests/trivial.rs`, `explain_writes_one_line_per_node`).
///
/// A node that holds a vector of inputs writes each of them one level deeper, as a node
/// that holds one input does (`tests/setop.rs`, `explain_indents_union_inputs`).
#[must_use]
pub fn explain(plan: &PhysicalPlan) -> String {
    let mut out = String::new();
    write_node(plan, 0, &mut out);
    out
}

/// Appends the line of `plan` at `depth`, then the lines of its children.
fn write_node(plan: &PhysicalPlan, depth: usize, out: &mut String) {
    if !out.is_empty() {
        out.push('\n');
    }
    for _ in 0..depth {
        out.push_str("  ");
    }
    // Writing into a `String` cannot fail; the `Result` of `write!` is dropped on purpose.
    let _ = write_label(plan, out);
    for child in children(plan) {
        write_node(child, depth + 1, out);
    }
}

/// Writes the operator name of `plan` and the few details that tell two of them apart.
///
/// The match below is exhaustive, so a variant added to [`PhysicalPlan`] without a line
/// here fails to compile; `tests/setop.rs`, `explain_names_every_variant` reads the name
/// back from the output for each variant.
fn write_label(plan: &PhysicalPlan, out: &mut String) -> std::fmt::Result {
    match plan {
        PhysicalPlan::OneRow => write!(out, "OneRow"),
        PhysicalPlan::Values { rows, .. } => write!(out, "Values(rows={})", rows.len()),
        PhysicalPlan::TableScan { table, alias, .. } => {
            write!(out, "TableScan(table={table}, alias={alias})")
        }
        PhysicalPlan::IndexSeek {
            index,
            range,
            direction,
            ..
        } => write!(
            out,
            "IndexSeek(index={index}, range={}, direction={direction:?})",
            range_kind(range)
        ),
        PhysicalPlan::Filter { .. } => write!(out, "Filter"),
        PhysicalPlan::Project { exprs, .. } => write!(out, "Project(columns={})", exprs.len()),
        PhysicalPlan::Top { .. } => write!(out, "Top"),
        PhysicalPlan::NestedLoopJoin { kind, .. } => write!(out, "NestedLoopJoin({kind:?})"),
        PhysicalPlan::HashJoin { kind, keys, .. } => {
            write!(out, "HashJoin({kind:?}, keys={})", keys.len())
        }
        PhysicalPlan::HashAggregate {
            group_by,
            aggregates,
            ..
        } => write!(
            out,
            "HashAggregate(group_by={}, aggregates={})",
            group_by.len(),
            aggregates.len()
        ),
        PhysicalPlan::StreamAggregate {
            group_by,
            aggregates,
            ..
        } => write!(
            out,
            "StreamAggregate(group_by={}, aggregates={})",
            group_by.len(),
            aggregates.len()
        ),
        PhysicalPlan::Sort { keys, .. } => write!(out, "Sort(keys={})", keys.len()),
        PhysicalPlan::TopN { keys, .. } => write!(out, "TopN(keys={})", keys.len()),
        PhysicalPlan::Distinct(_) => write!(out, "Distinct"),
        PhysicalPlan::SubqueryEval { subplans, .. } => {
            write!(out, "SubqueryEval(subplans={})", subplans.len())
        }
        PhysicalPlan::Union { all, .. } => write!(out, "Union(all={all})"),
        PhysicalPlan::Except { all, .. } => write!(out, "Except(all={all})"),
        PhysicalPlan::Intersect { all, .. } => write!(out, "Intersect(all={all})"),
    }
}

/// How much of an index a seek walks, in one word.
///
/// The bound expressions of the range are not written: they are evaluated at run time and
/// a line of this output is meant to be read, not parsed. Which of the three shapes was
/// chosen is what tells an access path from another (`tests/setop.rs`,
/// `explain_names_every_variant`).
fn range_kind(range: &KeyRangeExpr) -> &'static str {
    match range {
        KeyRangeExpr::Point(_) => "Point",
        KeyRangeExpr::Between(..) => "Between",
        KeyRangeExpr::Full => "Full",
    }
}

/// The children of `plan`, in the order they are written under it.
fn children(plan: &PhysicalPlan) -> Vec<&PhysicalPlan> {
    match plan {
        PhysicalPlan::OneRow
        | PhysicalPlan::Values { .. }
        | PhysicalPlan::TableScan { .. }
        | PhysicalPlan::IndexSeek { .. } => Vec::new(),
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::HashAggregate { input, .. }
        | PhysicalPlan::StreamAggregate { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. } => vec![input],
        PhysicalPlan::Distinct(input) => vec![input],
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => vec![outer, inner],
        PhysicalPlan::HashJoin { build, probe, .. } => vec![build, probe],
        PhysicalPlan::SubqueryEval {
            input, subplans, ..
        } => {
            let mut kids: Vec<&PhysicalPlan> = vec![input];
            kids.extend(subplans.iter().map(|sub| &sub.plan));
            kids
        }
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => inputs.iter().collect(),
    }
}
