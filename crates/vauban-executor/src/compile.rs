//! The checks SQL Server runs while it **compiles** a statement, and the reason a client
//! sees a result set before some errors and not before others.
//!
//! # The question
//!
//! `SELECT SUBSTRING('abc', 1, -1);` answers error 536 and **no** result set at all, while
//! `SELECT 1 / 0;` answers error 8134 preceded by a result set with its columns and no
//! row. A list of numbers on the silent side (536, 127, 1060, 1031, 1062, 174, 195, 4145,
//! 207, 402, 8117) would be the obvious way to reproduce it. It is the wrong one: the
//! number does not decide. (402's shape is `SELECT CAST(1 AS bit) + CAST(1 AS bit);`,
//! `sets = 0`; VaubanDB answers it the same way, its binder refusing the call before there
//! is a plan.)
//!
//! # The rule
//!
//! **The column metadata of a statement goes out when the statement starts running.** An
//! error raised while the batch is still being *compiled* therefore reaches the client
//! before any metadata, and one raised while the plan *runs* reaches it after. The
//! compiler folds the operands it can, and it checks the value of the ones a construct
//! constrains — a `TOP` row count, the length argument of `LEFT`, `RIGHT` and
//! `SUBSTRING` — as soon as that value is known before execution.
//!
//! **Which** operands the compiler folds is neither the same question for the two
//! constructs, nor answered by determinism alone. With `sets` being the number of result
//! sets that reach the client, SQL Server answers:
//!
//! | statement | SQL Server | family |
//! |---|---|---|
//! | `SELECT TOP (0 - 1) 1;` | 127, sev 15, state 1, `sets = 0` | compilation |
//! | `SELECT TOP (@@SPID * 0 - 1) 1;` | 127, sev 15, state 1, **`sets = 1`** | execution |
//! | `SELECT RIGHT('abc', -1);` | 536, sev 16, state 6, `sets = 0` | compilation |
//! | `SELECT RIGHT('abc', @@SPID * 0 - 1);` | 536, sev 16, state **2**, **`sets = 1`** | execution |
//!
//! Those four rows are what **distinguishes** the rule from the list: one number, 127,
//! falls on both sides of the boundary with the same severity, the same state and the same
//! message; and so does 536, whose state alone moves. No list of numbers can produce two
//! different answers for the same number, and `SUBSTRING` and `LEFT` even change number
//! between the two families — 536 while compiling, **537** at run time.
//!
//! Two hypotheses die on the same table: **severity** does not separate the families (127
//! is severity 15 on both sides, 536 severity 16 on both sides), and neither does the
//! presence of a `TOP` (`TOP` sits on both sides too).
//!
//! # What "the compiler can fold it" means
//!
//! It is **not** literalness, and it is **not** determinism either, although determinism
//! gets the first block below right. On the length argument of `SUBSTRING('abc', 1, …)`:
//!
//! | argument | SQL Server |
//! |---|---|
//! | `-1`, `0 - 1`, `CAST(-1 AS int)`, `CONVERT(int, '-1')` | 536, `sets = 0` |
//! | `ABS(-1) * -1`, `LEN('a') - 2`, `DATEDIFF(day, '2020-01-01', '2020-01-01') - 1` | 536, `sets = 0` |
//! | `CASE WHEN 1 = 1 THEN -1 ELSE 1 END`, `COALESCE(-1, 0)`, `IIF(1 = 1, -1, 1)` | 536, `sets = 0` |
//! | `@@SPID * 0 - 1`, `@@ROWCOUNT - 1`, `@@ERROR - 1` | 537, `sets = 1` |
//! | `DATEPART(year, GETDATE()) * 0 - 1`, `CAST(RAND() * 0 AS int) - 1` | 537, `sets = 1` |
//! | `LEN(DB_NAME()) * 0 - 1`, `LEN(@@SERVERNAME) * 0 - 1` | 537, `sets = 1` |
//!
//! The two blocks compute the **same value**, `-1`, so nothing but the moment of the check
//! explains the two answers. [`folds`] is the conservative approximation of that first
//! block: a literal folds, `CASE` follows its foldable conditions to the selected branch,
//! and `COALESCE` follows its foldable arguments to the first non-`NULL` value. Ordinary
//! calls require `FunctionDef::deterministic` and foldable arguments. `DATEPART` and
//! `DATENAME` over a literal remain narrower: `sysfn` marks them non-deterministic because
//! of `DATEFIRST` and `LANGUAGE`. For those residual forms the statement merely gets an
//! empty result set it should not have.
//!
//! # The two checks do not fold at the same moment
//!
//! `TOP` and the length argument share the rule above and nothing else. The length check
//! is the earlier of the two, and it is **sensitive to the type** of the argument, which
//! `TOP` is not. `sets` still being the number of result sets:
//!
//! | argument, written under both constructs | its type | `SUBSTRING('abc', 1, …)` | `TOP (…)` |
//! |---|---|---|---|
//! | `CAST(-1 AS int)` | `int` | 536, `sets = 0` | 127, `sets = 0` |
//! | `CAST(-1 AS smallint)` | `smallint` | 537, **`sets = 1`** | 127, `sets = 0` |
//! | `CAST(-1 AS bigint)` | `bigint` | 537, **`sets = 1`** | 127, `sets = 0` |
//! | `CAST(-1 AS int) + CAST(0 AS bigint)` | `bigint` | 537, **`sets = 1`** | 127, `sets = 0` |
//! | `CAST(-1 AS smallint) + CAST(0 AS smallint)` | `smallint` | 537, **`sets = 1`** | 127, `sets = 0` |
//! | `CAST(-1 AS smallint) * 1` | `int` | 536, `sets = 0` | 127, `sets = 0` |
//! | `CAST(CAST(-1 AS bigint) AS int)` | `int` | 536, `sets = 0` | 127, `sets = 0` |
//! | `CAST(-1 AS decimal(10, 0))`, `-1.0` | `decimal` | 537, **`sets = 1`** | 1060, `sets = 0` |
//!
//! Rows 2, 5 and 6 are what distinguishes "the type of the argument" from "the value the
//! fold computes": the three compute `-1`, and the two typed `int` are the ones checked at
//! compile time. Row 7 kills the reading "no widening under it": the value travels
//! through `bigint` and the check still fires, because the argument's own type is `int`.
//! And the whole right-hand column kills the reading "one fold, one moment": no row of it
//! is sensitive to the type.
//!
//! The type is read on the **argument**, not on the folded value, because that is what a
//! compiler has before it folds, and because the two do not disagree here (`convert` is
//! total from `int` to `int`).
//!
//! # `ISNULL` is the one built-in the length check does not see through
//!
//! Under a length argument, the following direct and wrapped `ISNULL` forms put the check
//! back on the run-time side:
//!
//! | argument | its type | `SUBSTRING('abc', 1, …)` | `TOP (…)` |
//! |---|---|---|---|
//! | `ISNULL(-1, 0)`, `ISNULL(-100000, 0)` | `int` | 537, `sets = 1` | 127, `sets = 0` |
//! | `ISNULL(-1, 0) * 1`, `0 + ISNULL(-1, 0)` | `int` | 537, `sets = 1` | 127, `sets = 0` |
//! | `CAST(ISNULL(-1, 0) AS int)`, `ABS(ISNULL(-1, 0)) * -1` | `int` | 537, `sets = 1` | 127, `sets = 0` |
//! | `COALESCE(-1, 0)` | `int` | 536, `sets = 0` | 127, `sets = 0` |
//!
//! The last row is what makes `ISNULL` a fact and not a family: `COALESCE`, its documented
//! neighbour, is folded. `RIGHT('abc', CASE WHEN 1 = 1 THEN -1 ELSE ISNULL(1, 0) END)`
//! raises 536 without metadata: an `ISNULL` in an unselected branch does not block the
//! length fold.
//!
//! **`NULLIF` is not a second exception, it is the type rule again**, and the column type
//! the server reports is the witness:
//!
//! | argument | type reported by `SELECT` | `SUBSTRING('abc', 1, …)` |
//! |---|---|---|
//! | `NULLIF(-1, 0)` | **`smallint`** | 537, `sets = 1` |
//! | `NULLIF(-100000, 0)` | **`int`** | 536, `sets = 0` |
//! | `NULLIF(-1, 0) * 1` | `int` | 536, `sets = 0` |
//! | `-NULLIF(1, 0)` | `smallint` | 537, `sets = 1` |
//!
//! SQL Server narrows the result type of `NULLIF` to the smallest integral type that holds
//! the literal; `sysfn` gives it the type of its first argument (`nulls.rs`), so `-1` stays
//! `int` here. [`check_length_argument`] therefore refuses to fold through `NULLIF` as
//! well — not because SQL Server refuses, but because our type would make us check a
//! shape it does not, and suppressing a result set the server sends is the worse of the
//! two errors. Lifting that is the business of whoever narrows `NULLIF`'s result type, not
//! of this module.
//!
//! # A fold that fails is not an error, it is a fold that did not happen
//!
//! Folding the checked operand can itself raise. SQL Server then drops the folding and
//! lets the plan run, so the error comes out **after** the metadata, with its ordinary
//! run-time face:
//!
//! | statement | SQL Server |
//! |---|---|
//! | `SELECT TOP (1 / 0) 1;` | 8134, `sets = 1` |
//! | `SELECT TOP (CAST('a' AS int)) 1;` | 245, `sets = 1` |
//! | `SELECT LEFT('abc', 1 / 0);` | 8134, `sets = 1` |
//! | `SELECT LEFT(CAST('abc' AS int), -1);` | **536**, `sets = 0` |
//!
//! The last row is what the check is hung on: the *other* argument fails to fold and the
//! compile-time check still fires, because it reads the length and nothing else. This is
//! why [`fold_integer`] swallows the error of the fold instead of returning it.
//!
//! # Where the checked node sits does not matter
//!
//! Compilation happens before any row is produced, so a check fires wherever its node is
//! written — including where execution would not reach it:
//!
//! | statement | SQL Server |
//! |---|---|
//! | `SELECT SUBSTRING('abc', 1, -1) WHERE 1 = 0;` | 536, `sets = 0` |
//! | `SELECT CASE WHEN 1 = 0 THEN SUBSTRING('abc', 1, -1) ELSE 'z' END;` | 536, `sets = 0` |
//! | `SELECT TOP (0) SUBSTRING('abc', 1, -1);` | 536, `sets = 0` |
//! | `SELECT 1 / 0, SUBSTRING('abc', 1, -1);` | **536**, `sets = 0` |
//! | `SELECT SUBSTRING('abc', 1, @@SPID * 0 - 1) WHERE 1 = 0;` | no error at all, `sets = 1` |
//!
//! The fourth row orders the two phases against each other: a run-time error written
//! *before* the offending node in the select list does not get its turn. The fifth is its
//! mirror: the same defect on an argument the compiler cannot fold is simply not seen,
//! because the row it would have been computed for does not exist.
//!
//! # What this module does **not** decide
//!
//! Whether the statements **before** the failing one keep their result sets. SQL Server
//! compiles the whole batch before running its first statement, so a compilation error
//! silences the batch entirely — `SELECT 1; SELECT SUBSTRING('abc', 1, -1); SELECT 2;`
//! answers no result set, where `session` still sends the row of `SELECT 1`. That
//! frontier is the batch's, not the statement's.
//!
//! Nor does it decide the **number** an error carries. The length check that lands at run
//! time is 537 on SQL Server and 536 here, `sysfn` having one check where SQL Server has
//! two; the type rule above moves more arguments to that side. Lifting it needs a second
//! check in `sysfn` and a state for `RIGHT` in `errors`.

use std::ops::Bound;

use vauban_binder::{BoundExpr, BoundExprKind, BoundTop, SortKey};
use vauban_errors::{SqlError, SqlResult};
use vauban_planner::{KeyRangeExpr, PhysicalPlan, PhysicalStatement};
use vauban_sysfn::FunctionDef;
use vauban_types::{SqlType, TypeInfo, Value, convert};

use crate::context::ExecContext;
use crate::errors::at;
use crate::expr::{as_condition, eval_expr};
use crate::plan::check_budget;

/// The functions whose length argument is checked while the call is compiled: the T-SQL
/// name, the index of the length argument, and the spelling SQL Server puts in the message
/// of 536 (`SqlError::invalid_length_parameter`, lower case).
///
/// The constrained argument is a property of each construct, not of a family: `SUBSTRING`
/// constrains its *third* argument and not its second — `SELECT SUBSTRING('abcdef', -1,
/// 3);` answers `a` and no error at all — and a function with no constrained argument has
/// no compile-time check to run, whatever it raises at run time
/// (`compile::tests::a_statement_without_a_constrained_argument_compiles`).
const LENGTH_ARGUMENT: [(&str, usize, &str); 3] = [
    ("LEFT", 1, "left"),
    ("RIGHT", 1, "right"),
    ("SUBSTRING", 2, "substring"),
];

/// Runs the compile-time checks of one physical statement.
///
/// `Ok(())` means the statement reaches execution, which is what makes its column metadata
/// go out; an `Err` here is an error the client sees **without** a result set.
/// [`crate::execute`] calls this first, so a caller that only knows `execute` still gets
/// the checks in the right order; `session` calls it on its own to know where to send the
/// metadata.
///
/// The checks walk the expressions a statement carries, wherever the statement puts
/// them: the nodes of a query, the source of an `INSERT`, the assignments of an
/// `UPDATE`, the condition of an `IF`, the value of a `SET`. A statement whose execution
/// is not written yet still compiles here; it is its execution that answers the internal
/// error 50000.
///
/// Residual folding differences, `DATEPART` over a literal and `NULLIF` under a length
/// argument among them, are described in the module documentation.
///
/// # Errors
///
/// 127 and 1060 for a `TOP` row count whose value is known before execution, 536 for a
/// negative length argument of `LEFT`, `RIGHT` or `SUBSTRING`, and the internal error
/// 50000 for the two `TOP PERCENT` numbers this crate does not raise yet (1031 and 1014)
/// — those two are compilation errors on SQL Server as well, so their **scope** is right
/// here even while their number is not.
pub fn compile(stmt: &PhysicalStatement, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    // No `_ =>` arm, as in `execute`: a variant added to `PhysicalStatement` breaks this
    // file rather than skipping its checks in silence.
    match stmt {
        PhysicalStatement::Query(plan) => compile_plan(plan, ctx),
        // Nothing to check and nothing to fold: a DDL statement carries no expression this
        // crate evaluates, and the checks it does have are the catalogue's, at execution.
        // SQL Server puts them there too: a batch opening with `SELECT 1;` gets its result
        // set before the DDL error — 3701 of a `DROP TABLE`, 226 and 574 of the
        // transaction gate (`ddl.rs`). A `USE` is the same: `session` acts on it after
        // the statement runs.
        PhysicalStatement::Ddl(_) | PhysicalStatement::Use { .. } => Ok(()),
        PhysicalStatement::Insert(insert) => compile_plan(&insert.source, ctx),
        PhysicalStatement::Update(update) => {
            compile_plan(&update.input, ctx)?;
            for (_, expr) in &update.assignments {
                compile_expr(expr, ctx)?;
            }
            Ok(())
        }
        PhysicalStatement::Delete(delete) => compile_plan(&delete.input, ctx),
        PhysicalStatement::SetVariable { value, .. } => compile_expr(value, ctx),
        PhysicalStatement::Declare(declarations) => {
            for declaration in declarations {
                if let Some(value) = &declaration.value {
                    compile_expr(value, ctx)?;
                }
            }
            Ok(())
        }
        PhysicalStatement::If {
            condition,
            then_,
            else_,
        } => {
            compile_expr(condition, ctx)?;
            compile(then_, ctx)?;
            match else_ {
                Some(else_) => compile(else_, ctx),
                None => Ok(()),
            }
        }
        PhysicalStatement::While { condition, body } => {
            compile_expr(condition, ctx)?;
            compile(body, ctx)
        }
        PhysicalStatement::Block(statements) => {
            for statement in statements {
                compile(statement, ctx)?;
            }
            Ok(())
        }
        PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Return(None)
        | PhysicalStatement::Transaction(_) => Ok(()),
        PhysicalStatement::Return(Some(expr)) | PhysicalStatement::Print(expr) => {
            compile_expr(expr, ctx)
        }
    }
}

/// Runs the compile-time checks of one plan node, its input first.
///
/// Depth first and input first, so that the select list is checked before the `TOP` above
/// it: `SELECT TOP (-1) SUBSTRING('abc', 1, -1);` answers **536** and not 127, the two
/// checks being both compile-time ones (`compile::tests::compilation_comes_before_execution`).
fn compile_plan(plan: &PhysicalPlan, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    match plan {
        PhysicalPlan::OneRow => Ok(()),
        PhysicalPlan::Values { rows, .. } => {
            for row in rows {
                for expr in row {
                    compile_expr(expr, ctx)?;
                }
            }
            Ok(())
        }
        // A scan holds no expression: its columns were resolved against the catalogue
        // while the statement was bound, and its `schema` is already what
        // `PhysicalPlan::schema` answers. Nothing is left to fold or to check here, and
        // the table is opened when the plan runs, not when it compiles.
        PhysicalPlan::TableScan { .. } => Ok(()),
        PhysicalPlan::IndexSeek { range, .. } => compile_range(range, ctx),
        PhysicalPlan::Filter { input, predicate } => {
            compile_plan(input, ctx)?;
            compile_expr(predicate, ctx)
        }
        PhysicalPlan::Project { input, exprs, .. } => {
            compile_plan(input, ctx)?;
            for projection in exprs {
                compile_expr(&projection.expr, ctx)?;
            }
            Ok(())
        }
        PhysicalPlan::Top { input, top } => {
            compile_plan(input, ctx)?;
            compile_expr(&top.expr, ctx)?;
            compile_top(top, ctx)
        }
        PhysicalPlan::NestedLoopJoin {
            outer, inner, on, ..
        } => {
            compile_plan(outer, ctx)?;
            compile_plan(inner, ctx)?;
            match on {
                Some(on) => compile_expr(on, ctx),
                None => Ok(()),
            }
        }
        PhysicalPlan::HashJoin {
            build,
            probe,
            keys,
            residual,
            ..
        } => {
            compile_plan(build, ctx)?;
            compile_plan(probe, ctx)?;
            for (left, right) in keys {
                compile_expr(left, ctx)?;
                compile_expr(right, ctx)?;
            }
            match residual {
                Some(residual) => compile_expr(residual, ctx),
                None => Ok(()),
            }
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
            ..
        }
        | PhysicalPlan::StreamAggregate {
            input,
            group_by,
            aggregates,
            ..
        } => {
            compile_plan(input, ctx)?;
            for key in group_by {
                compile_expr(key, ctx)?;
            }
            for aggregate in aggregates {
                if let Some(arg) = &aggregate.arg {
                    compile_expr(arg, ctx)?;
                }
            }
            Ok(())
        }
        PhysicalPlan::Sort { input, keys } => {
            compile_plan(input, ctx)?;
            compile_keys(keys, ctx)
        }
        PhysicalPlan::TopN { input, keys, top } => {
            compile_plan(input, ctx)?;
            compile_keys(keys, ctx)?;
            compile_expr(&top.expr, ctx)?;
            compile_top(top, ctx)
        }
        PhysicalPlan::Distinct(input) => compile_plan(input, ctx),
        PhysicalPlan::SubqueryEval {
            input, subplans, ..
        } => {
            compile_plan(input, ctx)?;
            for subplan in subplans {
                compile_plan(&subplan.plan, ctx)?;
            }
            Ok(())
        }
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => {
            for input in inputs {
                compile_plan(input, ctx)?;
            }
            Ok(())
        }
    }
}

/// Runs the compile-time checks of the keys of a sort.
fn compile_keys(keys: &[SortKey], ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    for key in keys {
        compile_expr(&key.expr, ctx)?;
    }
    Ok(())
}

/// Runs the compile-time checks of the bounds of an index seek.
fn compile_range(range: &KeyRangeExpr, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    match range {
        KeyRangeExpr::Point(keys) => {
            for key in keys {
                compile_expr(key, ctx)?;
            }
            Ok(())
        }
        KeyRangeExpr::Between(low, high) => {
            compile_bound(low, ctx)?;
            compile_bound(high, ctx)
        }
        KeyRangeExpr::Full => Ok(()),
    }
}

/// Runs the compile-time checks of one bound of an index seek.
fn compile_bound(bound: &Bound<Vec<BoundExpr>>, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    match bound {
        Bound::Included(keys) | Bound::Excluded(keys) => {
            for key in keys {
                compile_expr(key, ctx)?;
            }
            Ok(())
        }
        Bound::Unbounded => Ok(()),
    }
}

/// Checks the row count of a `TOP` when the compiler can know its value.
///
/// The check itself is [`check_budget`], the very one execution runs: the two families
/// differ by **when** the value is known, not by what is considered valid, and writing the
/// rule twice would be a way of getting two rules.
///
/// No gate on the type of the row count, unlike [`check_length_argument`]: `bigint`,
/// `smallint` and `int` fold here — `SELECT TOP (CAST(-1 AS bigint)) 1;` answers 127
/// with no result set (`compile::tests::a_row_count_is_folded_whatever_its_type`) — and
/// a `decimal` or a string folds too, into 1060 rather than 127. The two constructs fold
/// at two different moments in SQL Server, and that is the one reason this function and
/// that one do not share a gate (module documentation).
fn compile_top(top: &BoundTop, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    if !folds(&top.expr, ctx, false) {
        return Ok(());
    }
    let Ok(value) = eval_expr(&top.expr, None, ctx) else {
        // The fold failed: SQL Server drops it and lets the plan raise (module header).
        return Ok(());
    };
    check_budget(top, &value).map(|_| ())
}

/// Runs the compile-time checks of one expression, the node before its children.
fn compile_expr(expr: &BoundExpr, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
    match &expr.kind {
        // A column carries nothing to check at compile time, no more than a literal or a
        // variable.
        BoundExprKind::Literal(_)
        | BoundExprKind::Variable { .. }
        | BoundExprKind::ColumnRef(_) => Ok(()),
        BoundExprKind::Function { def, args } => {
            check_length_argument(def, args, ctx)?;
            for arg in args {
                compile_expr(arg, ctx)?;
            }
            Ok(())
        }
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            compile_expr(left, ctx)?;
            compile_expr(right, ctx)
        }
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => compile_expr(inner, ctx),
        BoundExprKind::In { expr, list, .. } => {
            compile_expr(expr, ctx)?;
            for item in list {
                compile_expr(item, ctx)?;
            }
            Ok(())
        }
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => Ok(()),
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            compile_expr(expr, ctx)?;
            compile_expr(pattern, ctx)?;
            match escape {
                Some(escape) => compile_expr(escape, ctx),
                None => Ok(()),
            }
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            if let Some(operand) = operand {
                compile_expr(operand, ctx)?;
            }
            for arm in arms {
                compile_expr(&arm.when, ctx)?;
                compile_expr(&arm.then, ctx)?;
            }
            match else_ {
                Some(else_) => compile_expr(else_, ctx),
                None => Ok(()),
            }
        }
    }
}

/// Built-ins the length check does not fold through, at any depth of the expression that
/// contains them (`compile::tests::a_length_that_calls_isnull_or_nullif_is_left_to_execution`).
///
/// `ISNULL` follows SQL Server: `SELECT SUBSTRING('abc', 1, ISNULL(-1, 0));` answers 537
/// with a result set where `COALESCE(-1, 0)`, of the same type and the same value, answers
/// 536 with no result set. `NULLIF` is a precaution: SQL Server does fold it, but after narrowing
/// its result type below `int`, which `sysfn` does not do — see the module documentation.
const LENGTH_OPAQUE: [&str; 2] = ["ISNULL", "NULLIF"];

/// Checks the length argument of a call, when the called function constrains one and the
/// compiler can know its value.
///
/// The threshold is the one `sysfn` applies at run time — a length below zero is refused —
/// and the two are meant to stay the same rule read at two moments. The number differs by
/// the moment: a folded constant is 536 here, while a length first seen at run time is 537
/// for `LEFT` and `SUBSTRING` and 536 for `RIGHT`. `NULL` is not a negative length:
/// `SELECT SUBSTRING('abc', 1, NULL);` answers `NULL`, and [`fold_integer`] answers
/// `None` for it (`compile::tests::a_null_length_is_not_a_negative_length`).
///
/// Two gates come before the fold, and neither has an equivalent on the `TOP` side
/// ([`compile_top`]): the argument must be typed `int`, and no [`LENGTH_OPAQUE`] built-in
/// may be encountered on the path selected while folding it. The module documentation
/// carries the tables behind both.
fn check_length_argument(
    def: &'static FunctionDef,
    args: &[BoundExpr],
    ctx: &mut ExecContext<'_>,
) -> SqlResult<()> {
    let Some((_, index, spelling)) = LENGTH_ARGUMENT
        .iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(def.name))
    else {
        return Ok(());
    };
    // Fewer arguments than the entry expects cannot happen — `check_call` has already
    // refused the call with 174 — and is not this function's error to raise.
    let Some(arg) = args.get(*index) else {
        return Ok(());
    };
    if arg.ty.ty != SqlType::Int {
        return Ok(());
    }
    match fold_integer(arg, ctx) {
        Some(length) if length < 0 => {
            Err(at(SqlError::invalid_length_parameter(spelling), arg.line))
        }
        _ => Ok(()),
    }
}

/// The value of `expr` as an integer, when the compiler can compute it before execution.
///
/// `None` in the three cases where there is nothing to check: the expression does not fold
/// (a required operand is not known before execution), folding it raises — SQL Server
/// then drops the fold and lets the plan raise instead (module header) — or the value
/// is `NULL`.
fn fold_integer(expr: &BoundExpr, ctx: &mut ExecContext<'_>) -> Option<i64> {
    if !folds(expr, ctx, true) {
        return None;
    }
    let value = eval_expr(expr, None, ctx).ok()?;
    // The same conversion `sysfn` applies to a length argument (`strings_core`,
    // `integer_arg`): the argument is not always an `int`, `SELECT LEFT('abc', '-1');`
    // being a legal call.
    let target = TypeInfo::new(SqlType::BigInt, true);
    match convert(&value, &expr.ty, &target, None).ok()? {
        Value::I64(length) => Some(length),
        _ => None,
    }
}

/// Whether the expression can be folded for a row-count or length check.
///
/// `CASE` conditions are evaluated in order; false and unknown skip their `THEN`, and true
/// selects it without inspecting later arms. A condition that cannot fold, or raises,
/// leaves the expression to execution. A simple `CASE` has already been desugared by the
/// binder. `COALESCE` and `ISNULL` select their first non-`NULL` argument under `TOP`;
/// `ISNULL` and `NULLIF` remain opaque under a length check. Ordinary calls keep the
/// registry's determinism gate; a per-argument frontier for `DATEPART` and `DATENAME` is
/// not written.
fn folds(expr: &BoundExpr, ctx: &mut ExecContext<'_>, length: bool) -> bool {
    match &expr.kind {
        BoundExprKind::Literal(_) => true,
        // A column has no value at compile time, no more than a variable.
        BoundExprKind::Variable { .. } | BoundExprKind::ColumnRef(_) => false,
        BoundExprKind::Function { def, args } => {
            if !def.deterministic
                || (length
                    && LENGTH_OPAQUE
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(def.name)))
            {
                return false;
            }
            if matches!(def.name, "COALESCE" | "ISNULL") {
                for arg in args {
                    if !folds(arg, ctx, length) {
                        return false;
                    }
                    match eval_expr(arg, None, ctx) {
                        Ok(Value::Null) => {}
                        Ok(_) => return true,
                        Err(_) => return false,
                    }
                }
                true
            } else {
                args.iter().all(|arg| folds(arg, ctx, length))
            }
        }
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => {
            folds(left, ctx, length) && folds(right, ctx, length)
        }
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => folds(inner, ctx, length),
        BoundExprKind::In { expr, list, .. } => {
            folds(expr, ctx, length) && list.iter().all(|item| folds(item, ctx, length))
        }
        // A subquery runs a plan; it has no value the compiler may read.
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => false,
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            folds(expr, ctx, length)
                && folds(pattern, ctx, length)
                && escape
                    .as_ref()
                    .is_none_or(|escape| folds(escape, ctx, length))
        }
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            if operand.is_some() {
                // The binder desugars simple CASE; do not fold an unexpected shape.
                return false;
            }
            for arm in arms {
                if !folds(&arm.when, ctx, length) {
                    return false;
                }
                match eval_expr(&arm.when, None, ctx).and_then(|value| as_condition(&value)) {
                    Ok(Some(true)) => return folds(&arm.then, ctx, length),
                    Ok(Some(false) | None) => {}
                    Err(_) => return false,
                }
            }
            else_.as_ref().is_none_or(|else_| folds(else_, ctx, length))
        }
    }
}

#[cfg(test)]
mod tests {
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_errors::SqlError;
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_planner::{NoIndexes, PlanContext, plan};
    use vauban_sysfn::{StaticContext, register_builtins};

    use super::*;
    use crate::statement::execute_collect;

    /// Runs the compile-time checks of `text`, going through the whole chain: a hand-built
    /// `BoundExpr` would say nothing about what a client's text folds to.
    fn compiled(text: &str) -> SqlResult<()> {
        with_chain(text, compile)
    }

    /// Runs `text` in full, compile-time checks included, and answers how many rows it
    /// produced.
    fn executed(text: &str) -> SqlResult<usize> {
        with_chain(text, |stmt, ctx| {
            execute_collect(stmt, ctx).map(|(_, set)| set.rows.len())
        })
    }

    /// Parses, binds, plans and hands the single statement of `text` to `run`.
    fn with_chain<T>(
        text: &str,
        run: impl FnOnce(&PhysicalStatement, &mut ExecContext<'_>) -> SqlResult<T>,
    ) -> SqlResult<T> {
        // The registry is global and idempotent, like in the test files of the crate.
        register_builtins();
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let bind_ctx = BindContext::scalar(text, SessionOptions::default());
        let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
        let physical = plan(
            bound,
            &PlanContext {
                catalog: &NoIndexes,
            },
        )
        .expect("the statement plans");
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        run(&physical, &mut ctx)
    }

    /// A DDL statement and a `USE` have nothing to compile: `compile` answers `Ok(())`
    /// for them in a context that has neither engine nor catalogue, so the checks of the
    /// catalogue are not silently moved here (`ddl.rs` runs them at execution).
    #[test]
    fn ddl_and_use_compile_without_checks() {
        for text in [
            "CREATE TABLE dbo.t (a int);",
            "DROP TABLE dbo.t;",
            "USE master;",
        ] {
            compiled(text).expect("nothing to check at compile time");
        }
    }

    /// The number of the error `text` raises while it is compiled.
    fn compile_error(text: &str) -> SqlError {
        compiled(text).expect_err("the statement does not compile")
    }

    #[test]
    fn conditional_folding_follows_the_selected_branch() {
        for expression in [
            "CASE WHEN 1 = 1 THEN -1 ELSE @@SPID END",
            "CASE WHEN 1 = 0 THEN @@SPID ELSE -1 END",
            "CASE 1 WHEN 1 THEN -1 ELSE @@SPID END",
            "CASE 2 WHEN 1 THEN @@SPID ELSE -1 END",
            "CASE WHEN 1 = 1 THEN -1 WHEN @@SPID > 0 THEN 1 ELSE 1 END",
            "CASE WHEN NULL = 1 THEN @@SPID ELSE -1 END",
            "CASE WHEN 1 = 1 THEN -1 ELSE 1 / 0 END",
            "CASE WHEN 1 = 1 THEN -1 ELSE CAST('bad' AS int) END",
            "CASE WHEN 1 = 1 THEN -1 ELSE ISNULL(1, 0) END",
            "CASE WHEN 1 = 1 THEN CASE WHEN 1 = 0 THEN @@SPID ELSE -1 END ELSE @@SPID END",
            "CASE WHEN CASE WHEN 1 = 1 THEN 1 ELSE @@SPID END = 1 THEN -1 ELSE @@SPID END",
        ] {
            let length = format!("SELECT RIGHT('abc', {expression})");
            assert_eq!(compile_error(&length).number, 536, "{length}");
            let top = format!("SELECT TOP ({expression}) 1");
            assert_eq!(compile_error(&top).number, 127, "{top}");
        }
    }

    #[test]
    fn unresolved_or_failing_conditions_do_not_fold() {
        for expression in [
            "CASE WHEN @@SPID = 0 THEN 1 ELSE -1 END",
            "CASE @@SPID WHEN 0 THEN 1 ELSE -1 END",
            "CASE WHEN 1 / 0 = 1 THEN -1 ELSE @@SPID END",
            "CASE WHEN 1 = 1 THEN @@SPID * 0 - 1 ELSE -1 END",
        ] {
            assert!(compiled(&format!("SELECT RIGHT('abc', {expression})")).is_ok());
            assert!(compiled(&format!("SELECT TOP ({expression}) 1")).is_ok());
        }
    }

    #[test]
    fn coalesce_selects_its_first_known_non_null_argument() {
        for expression in [
            "COALESCE(-1, @@SPID)",
            "COALESCE(CAST(NULL AS int), -1, @@SPID)",
            "COALESCE(-1, 1 / 0)",
            "COALESCE(-1, ISNULL(1, 0))",
        ] {
            assert_eq!(
                compile_error(&format!("SELECT RIGHT('abc', {expression})")).number,
                536
            );
            assert_eq!(
                compile_error(&format!("SELECT TOP ({expression}) 1")).number,
                127
            );
        }
    }

    #[test]
    fn selected_isnull_keeps_the_length_gate() {
        for expression in [
            "ISNULL(-1, @@SPID)",
            "ISNULL(CAST(NULL AS int), -1)",
            "ISNULL(-1, 1 / 0)",
            "CASE WHEN 1 = 1 THEN ISNULL(-1, 0) ELSE @@SPID END",
        ] {
            assert!(compiled(&format!("SELECT RIGHT('abc', {expression})")).is_ok());
            assert_eq!(
                compile_error(&format!("SELECT TOP ({expression}) 1")).number,
                127
            );
        }
    }

    #[test]
    fn conditional_folds_keep_null_and_selected_errors() {
        let absent = "CASE WHEN 1 = 0 THEN @@SPID END";
        assert!(compiled(&format!("SELECT RIGHT('abc', {absent})")).is_ok());
        assert_eq!(
            compile_error(&format!("SELECT TOP ({absent}) 1")).number,
            1060
        );
        let failing = "CASE WHEN 1 = 1 THEN 1 / 0 ELSE @@SPID END";
        for text in [
            format!("SELECT RIGHT('abc', {failing})"),
            format!("SELECT TOP ({failing}) 1"),
        ] {
            assert!(compiled(&text).is_ok());
            assert_eq!(
                executed(&text)
                    .expect_err("the selected expression raises")
                    .number,
                8134
            );
        }
        assert_eq!(
            executed("SELECT CASE WHEN 1 = 1 THEN 1 ELSE 1 / 0 END"),
            Ok(1)
        );
    }

    #[test]
    fn a_constant_length_is_checked_before_the_plan_runs() {
        // SQL Server sends no result set for these three.
        assert_eq!(compile_error("SELECT SUBSTRING('abc', 1, -1)").number, 536);
        assert_eq!(compile_error("SELECT LEFT('abc', -1)").number, 536);
        assert_eq!(compile_error("SELECT RIGHT('abc', -1)").number, 536);
        // The state follows the function, as `errors` builds it (6 for `left` and
        // `right`, 8 for `substring`), and the compile-time check keeps that.
        assert_eq!(compile_error("SELECT LEFT('abc', -1)").state, 6);
        assert_eq!(compile_error("SELECT SUBSTRING('abc', 1, -1)").state, 8);
    }

    #[test]
    fn a_length_the_compiler_computes_is_not_only_a_literal() {
        // Each of these folds on SQL Server and answers 536 with no result set (module
        // header, second table).
        for text in [
            "SELECT SUBSTRING('abc', 1, 0 - 1)",
            "SELECT SUBSTRING('abc', 1, CAST(-1 AS int))",
            "SELECT SUBSTRING('abc', 1, ABS(-1) * -1)",
            "SELECT SUBSTRING('abc', 1, LEN('a') - 2)",
            "SELECT SUBSTRING('abc', 1, CASE WHEN 1 = 1 THEN -1 ELSE 1 END)",
            // Typed `int` although the value travels through a wider type, and folded on
            // SQL Server for that reason (module header, third table, rows 6 and 7).
            "SELECT SUBSTRING('abc', 1, CAST(-1 AS smallint) * 1)",
            "SELECT SUBSTRING('abc', 1, CAST(CAST(-1 AS bigint) AS int))",
            "SELECT SUBSTRING('abc', 1, COALESCE(-1, 0))",
        ] {
            assert_eq!(compile_error(text).number, 536, "{text}");
        }
    }

    #[test]
    fn a_length_not_typed_int_is_left_to_execution() {
        // The gate that has no equivalent on the `TOP` side: SQL Server checks a length
        // argument at compile time when the argument is typed `int`, and sends its result
        // set for the other integral or numeric types — with 537, not 536 (module header,
        // third table). Without the gate each of these would compute -1 at compile time
        // and suppress a result set the server sends.
        for text in [
            "SELECT SUBSTRING('abc', 1, CAST(-1 AS bigint))",
            "SELECT SUBSTRING('abc', 1, CAST(-1 AS smallint))",
            "SELECT SUBSTRING('abc', 1, CAST(-1 AS decimal(10, 0)))",
            "SELECT SUBSTRING('abc', 1, -1.0)",
            "SELECT SUBSTRING('abc', 1, CAST(-1 AS int) + CAST(0 AS bigint))",
            "SELECT SUBSTRING('abc', 1, CONVERT(bigint, '-1'))",
            "SELECT LEFT('abc', CAST(-1 AS bigint))",
            "SELECT RIGHT('abc', CAST(-1 AS bigint))",
        ] {
            assert!(compiled(text).is_ok(), "{text}");
            // The check still exists, one phase later: what moved is the moment, not the
            // threshold. The number is 536 here and 537 there.
            assert!(executed(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_length_that_calls_isnull_or_nullif_is_left_to_execution() {
        // `ISNULL` follows SQL Server — `SELECT SUBSTRING('abc', 1, ISNULL(-1, 0));` answers
        // 537 with a result set where `COALESCE(-1, 0)`, same type and same value, answers
        // 536 with no result set — and it is opaque at any depth, the root included. `NULLIF` is
        // there because SQL Server narrows its result type below `int` and `sysfn` does
        // not (module header).
        for text in [
            "SELECT SUBSTRING('abc', 1, ISNULL(-1, 0))",
            "SELECT SUBSTRING('abc', 1, ISNULL(-1, 0) * 1)",
            "SELECT SUBSTRING('abc', 1, 0 + ISNULL(-1, 0))",
            "SELECT SUBSTRING('abc', 1, CAST(ISNULL(-1, 0) AS int))",
            "SELECT SUBSTRING('abc', 1, ABS(ISNULL(-1, 0)) * -1)",
            "SELECT LEFT('abc', ISNULL(-1, 0))",
            "SELECT RIGHT('abc', ISNULL(-1, 0))",
            "SELECT SUBSTRING('abc', 1, NULLIF(-1, 0))",
            "SELECT SUBSTRING('abc', 1, -NULLIF(1, 0))",
        ] {
            assert!(compiled(text).is_ok(), "{text}");
            assert!(executed(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_row_count_is_folded_whatever_its_type() {
        // The mirror of `a_length_not_typed_int_is_left_to_execution`, and what makes the
        // two gates two facts rather than a symmetry: the very same arguments are checked
        // at compile time under a `TOP`. `ISNULL` is transparent here too.
        for text in [
            "SELECT TOP (CAST(-1 AS bigint)) 1",
            "SELECT TOP (CAST(-1 AS smallint)) 1",
            "SELECT TOP (ISNULL(-1, 0)) 1",
            "SELECT TOP (NULLIF(-1, 0)) 1",
        ] {
            assert_eq!(compile_error(text).number, 127, "{text}");
        }
        // A row count that is not integral answers 1060 — with no result set on both
        // sides, `binder` refusing the clause before there is a plan to compile, which is
        // why these two are not `compile_error`.
        for text in [
            "SELECT TOP (CAST(-1 AS decimal(10, 0))) 1",
            "SELECT TOP (-1.0) 1",
        ] {
            let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
            let bind_ctx = BindContext::scalar(text, SessionOptions::default());
            let err = bind(&batch.statements[0], &bind_ctx).expect_err("the clause is refused");
            assert_eq!(err.number, 1060, "{text}");
        }
    }

    #[test]
    fn a_length_that_does_not_fold_is_left_to_execution() {
        // `@@SPID` is non-deterministic, so the compiler cannot know the length: SQL
        // Server sends the result set and raises 537 at run time. This test also pins
        // that compilation lets the expression through.
        let text = "SELECT SUBSTRING('abc', 1, @@SPID * 0 - 1)";
        assert!(compiled(text).is_ok());
        assert_eq!(
            executed(text).expect_err("the plan raises it").number,
            537,
            "the check still exists at run time"
        );
    }

    #[test]
    fn a_null_length_is_not_a_negative_length() {
        // `SELECT SUBSTRING('abc', 1, NULL);` answers `NULL` and no error at all.
        assert!(compiled("SELECT SUBSTRING('abc', 1, NULL)").is_ok());
        assert_eq!(executed("SELECT SUBSTRING('abc', 1, NULL)"), Ok(1));
    }

    #[test]
    fn a_constant_row_count_is_checked_before_the_plan_runs() {
        assert_eq!(compile_error("SELECT TOP (-1) 1").number, 127);
        assert_eq!(compile_error("SELECT TOP (0 - 1) 1").number, 127);
        assert_eq!(
            compile_error("SELECT TOP (CAST(NULL AS int)) 1").number,
            1060
        );
    }

    #[test]
    fn a_row_count_that_does_not_fold_is_left_to_execution() {
        // The vector that separates the rule from a list of numbers: **the same 127**,
        // once before the metadata and once after.
        let text = "SELECT TOP (@@SPID * 0 - 1) 1";
        assert!(compiled(text).is_ok());
        assert_eq!(executed(text).expect_err("the plan raises it").number, 127);
    }

    #[test]
    fn a_fold_that_raises_is_not_a_compilation_error() {
        // SQL Server drops the fold and lets the plan raise, result set included.
        for text in [
            "SELECT TOP (1 / 0) 1",
            "SELECT LEFT('abc', 1 / 0)",
            "SELECT SUBSTRING('abc', 1, CAST('a' AS int))",
        ] {
            assert!(compiled(text).is_ok(), "{text}");
            assert!(executed(text).is_err(), "{text}");
        }
        // The other argument failing to fold does not spare the length its check.
        assert_eq!(
            compile_error("SELECT LEFT(CAST('abc' AS int), -1)").number,
            536
        );
    }

    #[test]
    fn the_check_fires_where_execution_would_never_reach() {
        // No row is ever computed, and the error comes out all the same.
        assert_eq!(
            compile_error("SELECT SUBSTRING('abc', 1, -1) WHERE 1 = 0").number,
            536
        );
        assert_eq!(
            compile_error("SELECT CASE WHEN 1 = 0 THEN SUBSTRING('abc', 1, -1) ELSE 'z' END")
                .number,
            536
        );
        assert_eq!(
            compile_error("SELECT TOP (0) SUBSTRING('abc', 1, -1)").number,
            536
        );
    }

    #[test]
    fn compilation_comes_before_execution() {
        // A run-time error written first in the select list does not get its turn.
        assert_eq!(
            compile_error("SELECT 1 / 0, SUBSTRING('abc', 1, -1)").number,
            536
        );
        // And between two compile-time checks, the select list answers before the `TOP`.
        assert_eq!(
            compile_error("SELECT TOP (-1) SUBSTRING('abc', 1, -1)").number,
            536
        );
    }

    #[test]
    fn a_statement_without_a_constrained_argument_compiles() {
        for text in [
            "SELECT 1",
            "SELECT 1 / 0",
            "SELECT SUBSTRING('abc', -5, 3)",
            "SELECT TOP (1) LEFT('abc', 2)",
            "SELECT 1 WHERE 'a' LIKE 'a'",
        ] {
            assert!(compiled(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn the_registry_still_names_the_functions_this_module_checks() {
        // `LENGTH_ARGUMENT` matches on the name `sysfn` publishes: a renamed built-in, or
        // one whose length argument moves, must break here rather than stop being checked
        // in silence.
        register_builtins();
        for (name, index, spelling) in LENGTH_ARGUMENT {
            let def = vauban_sysfn::lookup(name).unwrap_or_else(|| panic!("{name} is registered"));
            assert!(
                def.arity.accepts(index + 1),
                "{name} has an argument {index}"
            );
            assert!(def.deterministic, "{name} folds when its arguments do");
            assert_eq!(spelling, name.to_ascii_lowercase(), "message spelling");
        }
        // Same reason for `LENGTH_OPAQUE`: a built-in that stops being registered under
        // that name would stop being opaque without a word, and the length check would go
        // back to suppressing result sets the server sends.
        for name in LENGTH_OPAQUE {
            let def = vauban_sysfn::lookup(name).unwrap_or_else(|| panic!("{name} is registered"));
            assert!(
                def.deterministic,
                "{name} is opaque to the fold although `folds` accepts it, which is the \
                 whole point of the list"
            );
        }
    }
}
