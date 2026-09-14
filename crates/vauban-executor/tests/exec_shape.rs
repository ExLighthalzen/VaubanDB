//! The shape of the executor: the names `session` compiles against.
//!
//! The `use` below is the contract of the module — `execute`, `eval_expr`, `ExecContext`,
//! `ExecOutcome`, `RowSet` and `Row` — written in the order `rustfmt` imposes.

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundStatement, LogicalPlan, OutputColumn, OutputSchema,
    SessionOptions,
};
use vauban_executor::{ExecContext, ExecOutcome, Row, RowSet, eval_expr, execute};
use vauban_sysfn::StaticContext;
use vauban_types::{SqlType, TypeInfo, Value};

/// A type of the right shape for the tests: `int`, nullable.
fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

/// A bound literal, the simplest expression, built by hand rather than bound.
fn literal() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(1)),
        ty: int(),
        line: 1,
    }
}

/// The entry point of the contract answers rows. What those rows are worth for each
/// `SELECT` — `WHERE`, `TOP`, schemas — is checked by `tests/execute_select.rs`; the plan
/// is built by hand here so that this file keeps depending on nothing but the names
/// `session` compiles against.
#[test]
fn execute_runs_the_plan() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    // `OneRow` alone is the source a `SELECT` without `FROM` sits on: one row, no column.
    let stmt = BoundStatement::Query(Box::new(LogicalPlan::OneRow));
    let outcome = execute(&stmt, &mut ctx).expect("the plan executes");
    match outcome {
        ExecOutcome::Rows(set) => {
            assert!(set.schema.columns.is_empty());
            assert_eq!(set.rows.len(), 1);
            assert!(set.rows[0].is_empty());
        }
        ExecOutcome::NoRows => panic!("a `SELECT` produces rows"),
    }
}

/// The entry point of the contract answers a value. Its semantics — operators,
/// comparisons, three-valued logic — are checked by `tests/eval_expr.rs`.
#[test]
fn eval_expr_evaluates() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let value = eval_expr(&literal(), None, &mut ctx).expect("a literal evaluates");
    assert_eq!(value, Value::I32(1));
}

#[test]
fn exec_context_is_constructible() {
    // The two things a scalar evaluation needs from the session, and nothing else.
    let eval = StaticContext {
        spid: 57,
        database: "master".to_owned(),
        ..StaticContext::default()
    };
    let ctx = ExecContext::scalar(&eval, SessionOptions::default());
    assert_eq!(ctx.eval.spid(), 57);
    assert_eq!(ctx.eval.current_database(), "master");
    assert!(ctx.options.ansi_nulls);
    assert!(!ctx.options.numeric_roundabort);
}

#[test]
fn rowset_carries_its_schema_and_its_rows() {
    let row: Row = vec![Value::I32(1)];
    let rows = RowSet {
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: String::new(),
                ty: int(),
            }],
        },
        rows: vec![row],
    };
    assert_eq!(rows.schema.columns.len(), 1);
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].len(), rows.schema.columns.len());

    // A `SELECT` produces `Rows`; `NoRows` is there for the statements without a result set.
    let outcome = ExecOutcome::Rows(rows);
    match outcome {
        ExecOutcome::Rows(set) => assert_eq!(set.rows[0][0], Value::I32(1)),
        ExecOutcome::NoRows => panic!("a `SELECT` produces rows"),
    }
    assert!(matches!(ExecOutcome::NoRows, ExecOutcome::NoRows));
}

/// The crate carries no `allow(dead_code)`, neither crate-wide nor on an item: what is
/// not used is removed rather than silenced.
#[test]
fn the_crate_has_no_dead_code_allowance() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_files(&root) {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for line in text.lines() {
            assert!(
                !line.contains("allow(dead_code)"),
                "dead_code allowance in {}: {}",
                path.display(),
                line.trim_end()
            );
        }
    }
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}
