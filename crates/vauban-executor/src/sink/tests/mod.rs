use super::*;

use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PlanContext, plan};
use vauban_sysfn::StaticContext;
use vauban_types::SqlType;

use crate::{ExecContext, ExecOutcome, execute};

/// `CollectSink` refuses a row that comes before the columns, and keeps the rest.
#[test]
fn a_row_before_the_columns_is_refused() {
    let mut sink = CollectSink::new();
    let refused = sink.row(&[Value::I32(1)]).unwrap_err();
    assert_eq!(refused.number, 50000);
    assert!(sink.rows.is_empty());
    assert!(sink.events.is_empty());
    sink.columns(&[]).expect("the columns are taken");
    sink.row(&[Value::I32(1)]).expect("a row after the columns");
    assert_eq!(sink.rows, vec![vec![Value::I32(1)]]);
    assert_eq!(
        sink.events,
        vec![
            SinkEvent::Columns(Vec::new()),
            SinkEvent::Row(vec![Value::I32(1)]),
        ]
    );
}

/// A column keeps its nullability apart from its type, and goes back to the bound shape
/// with the nullability folded into the type.
#[test]
fn a_column_round_trips_through_the_bound_schema() {
    let output = OutputColumn {
        name: "a".to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
    };
    let column = ResultColumn::from_output(&output);
    assert_eq!(column.name, "a");
    assert!(column.nullable);
    let mut not_null = column.clone();
    not_null.nullable = false;
    assert!(!not_null.to_output().ty.nullable);
    assert_eq!(column.to_output().ty, output.ty);
}

/// The ends of statements, the errors and the environment changes are kept in the order
/// they were received.
#[test]
fn done_errors_and_environment_changes_are_kept_in_order() {
    let mut sink = CollectSink::new();
    let change = EnvironmentChange::Database {
        old: "master".to_owned(),
        new: "tempdb".to_owned(),
    };
    let error = SqlError::from(InternalError::Bug("x".to_owned()));
    sink.env_change(&change).unwrap();
    sink.error(&error).unwrap();
    sink.done(None).unwrap();
    sink.env_change(&EnvironmentChange::TransactionBegun(7))
        .unwrap();
    assert_eq!(
        sink.events,
        vec![
            SinkEvent::EnvChange(change),
            SinkEvent::Error(error.clone()),
            SinkEvent::Done(None),
            SinkEvent::EnvChange(EnvironmentChange::TransactionBegun(7)),
        ]
    );
    assert_eq!(sink.errors(), vec![&error]);
}

/// Runs each statement of `text` into `sink` the way a driver of a batch does: the
/// results of the statement, the error it answered if any, then its end. A scalar
/// context: no storage, no catalogue, no session.
fn run_batch(text: &str, sink: &mut CollectSink) {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the batch parses");
    let eval = StaticContext::default();
    for statement in &batch.statements {
        let bind_ctx = BindContext::scalar(text, SessionOptions::default());
        let bound = bind(statement, &bind_ctx).expect("the statement binds");
        let physical = plan(
            bound,
            &PlanContext {
                catalog: &NoIndexes,
            },
        )
        .expect("the statement plans");
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        match execute(&physical, &mut ctx, sink) {
            Ok(ExecOutcome::Rows(count)) => sink.done(Some(count)).unwrap(),
            Ok(_) => sink.done(None).unwrap(),
            Err(error) => {
                sink.error(&error).unwrap();
                sink.done(None).unwrap();
            }
        }
    }
}

/// A batch of four statements runs into the in-memory sink with no session: a result
/// set, a message, a run-time error after the columns of its statement, and a second
/// result set, each statement closed by its own end.
#[test]
fn a_batch_runs_into_the_collect_sink_without_a_session() {
    let mut sink = CollectSink::new();
    run_batch(
        "SELECT 1 AS a; PRINT 'hello'; SELECT 1 / 0; SELECT 2 AS b;",
        &mut sink,
    );
    let shape: Vec<String> = sink
        .events
        .iter()
        .map(|event| match event {
            SinkEvent::Columns(columns) => format!(
                "columns {}",
                columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            SinkEvent::Row(row) => format!("row {row:?}"),
            SinkEvent::Done(count) => format!("done {count:?}"),
            SinkEvent::Info(info) => format!("info {}", info.message),
            SinkEvent::Error(error) => format!("error {}", error.number),
            SinkEvent::EnvChange(change) => format!("env {change:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            "columns a",
            "row [I32(1)]",
            "done Some(1)",
            "info hello",
            "done None",
            "columns ",
            "error 8134",
            "done None",
            "columns b",
            "row [I32(2)]",
            "done Some(1)",
        ]
    );
    assert_eq!(sink.rows, vec![vec![Value::I32(1)], vec![Value::I32(2)]]);
}
