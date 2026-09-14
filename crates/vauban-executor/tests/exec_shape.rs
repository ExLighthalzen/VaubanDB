//! The shape of the executor: the names `session` compiles against.
//!
//! The `use` below is the contract of the module — `execute`, `execute_collect`,
//! `eval_expr`, `ExecContext`, `ExecOutcome`, `RowSink`, `CollectSink`, `CancelToken`,
//! `ExecSession`, `RowSet` and `Row` — written in the order `rustfmt` imposes.

use vauban_binder::{BoundExpr, BoundExprKind, OutputColumn, OutputSchema, SessionOptions};
use vauban_executor::{
    CancelToken, CollectSink, ExecContext, ExecOutcome, ExecSession, Row, RowSet, RowSink,
    eval_expr, execute, execute_collect,
};
use vauban_planner::{PhysicalPlan, PhysicalStatement};
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

/// The entry point of the contract hands its rows to the sink and answers how many. What
/// those rows are worth for each `SELECT` — `WHERE`, `TOP`, schemas — is checked by
/// `tests/execute_select.rs`; the plan is built by hand here so that this file keeps
/// depending on nothing but the names `session` compiles against.
#[test]
fn execute_runs_the_plan() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    // `OneRow` alone is the source a `SELECT` without `FROM` sits on: one row, no column.
    let stmt = PhysicalStatement::Query(PhysicalPlan::OneRow);
    let mut sink = CollectSink::new();
    let outcome = execute(&stmt, &mut ctx, &mut sink).expect("the plan executes");
    match outcome {
        ExecOutcome::Rows(count) => {
            assert_eq!(count, 1);
            let schema = sink.schema.expect("the columns were announced");
            assert!(schema.columns.is_empty());
            assert_eq!(sink.rows.len(), 1);
            assert!(sink.rows[0].is_empty());
        }
        other => panic!("a `SELECT` produces rows, not {other:?}"),
    }
}

/// The collecting entry point answers the same thing as a materialised `RowSet`.
#[test]
fn execute_collect_materialises() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let stmt = PhysicalStatement::Query(PhysicalPlan::OneRow);
    let (outcome, set) = execute_collect(&stmt, &mut ctx).expect("the plan executes");
    assert!(matches!(outcome, ExecOutcome::Rows(1)));
    assert!(set.schema.columns.is_empty());
    assert_eq!(set.rows, vec![Row::new()]);
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
    // The two things a scalar evaluation needs from the session, and nothing else; the
    // token and the session state are added by their builders.
    let eval = StaticContext {
        spid: 57,
        database: "master".to_owned(),
        ..StaticContext::default()
    };
    let token = CancelToken::new();
    let mut session = ExecSession::default();
    let ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_cancel(&token)
        .with_session(&mut session);
    assert_eq!(ctx.eval.spid(), 57);
    assert_eq!(ctx.eval.current_database(), "master");
    assert!(ctx.options.ansi_nulls);
    assert!(!ctx.options.numeric_roundabort);
    assert!(!ctx.cancelled());
    assert!(ctx.session.is_some());
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

    // A `SELECT` produces `Rows`; `NoRows` is there for the statements without a result
    // set; `Cancelled` is not an error.
    let outcome = ExecOutcome::Rows(rows.rows.len() as u64);
    assert!(matches!(outcome, ExecOutcome::Rows(1)));
    assert!(matches!(ExecOutcome::NoRows, ExecOutcome::NoRows));
    assert!(matches!(ExecOutcome::Cancelled, ExecOutcome::Cancelled));
}

/// A sink of `session`'s shape: the three methods, each answering `Ok(())`.
#[test]
fn row_sink_is_implementable() {
    struct Counting(usize);
    impl RowSink for Counting {
        fn columns(&mut self, _schema: &OutputSchema) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
        fn row(&mut self, _row: &[Value]) -> vauban_errors::SqlResult<()> {
            self.0 += 1;
            Ok(())
        }
        fn info(&mut self, _message: &vauban_errors::InfoMessage) -> vauban_errors::SqlResult<()> {
            Ok(())
        }
    }
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let mut sink = Counting(0);
    let outcome = execute(
        &PhysicalStatement::Query(PhysicalPlan::OneRow),
        &mut ctx,
        &mut sink,
    )
    .expect("the plan executes");
    assert!(matches!(outcome, ExecOutcome::Rows(1)));
    assert_eq!(sink.0, 1);
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
