//! Integration tests for `ALTER TABLE ADD` / `DROP COLUMN` and `ADD` / `DROP CONSTRAINT`.
//!
//! These tests parse the SQL text, bind against a catalogue view and execute, which is the
//! path a client takes. The unit tests in `ddl_alter.rs` exercise `execute_alter_table`
//! directly; this file makes sure the parsing, binding and planning produce the shape the
//! executor reads.

use std::sync::Arc;

use vauban_binder::{
    BindContext, ColumnBinding, LockHints, OutputColumn, OutputSchema, SessionOptions, bind,
};
use vauban_catalog::{Catalog, ObjectId};
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, execute_collect};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PhysicalPlan, PhysicalStatement, PlanContext, plan};
use vauban_storage::{MemoryStorage, Row, Storage, TableId};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::Value;

/// A bootstrapped catalogue over an in-memory storage.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        Self {
            storage,
            txn,
            catalog,
        }
    }
}

/// Parses, binds and executes `text` as one statement inside `handle`.
fn run_on(fixture: &Fixture, handle: &TxnHandle, text: &str) -> Result<ExecOutcome, SqlError> {
    let snapshot = fixture.catalog.snapshot(handle);
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    assert_eq!(batch.statements.len(), 1, "one statement per call");
    let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
    bind_ctx.catalog = Some(&snapshot);
    let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
    let planned = plan(
        bound,
        &PlanContext {
            catalog: &NoIndexes,
        },
    )
    .expect("the statement plans");
    let snap = fixture.txn.statement_snapshot(handle);
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
        .with_catalog(&fixture.catalog)
        .with_handle(handle);
    let (outcome, _) = execute_collect(&planned, &mut ctx)?;
    Ok(outcome)
}

/// Parses, binds and executes `text` in a transaction of its own.
fn run(fixture: &Fixture, text: &str) -> Result<ExecOutcome, SqlError> {
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    match run_on(fixture, &handle, text) {
        Ok(outcome) => {
            fixture.txn.commit(handle).expect("the transaction commits");
            Ok(outcome)
        }
        Err(err) => {
            fixture
                .txn
                .rollback(handle)
                .expect("the transaction rolls back");
            Err(err)
        }
    }
}

/// The [`ObjectId`] of `master.dbo.name` in `handle`'s snapshot.
fn table_object(fixture: &Fixture, handle: &TxnHandle, name: &str) -> ObjectId {
    fixture
        .catalog
        .snapshot(handle)
        .resolve_object("master", Some("dbo"), name, "dbo")
        .expect("the table resolves")
        .id
}

/// The storage [`TableId`] of `master.dbo.name` in `handle`'s snapshot.
fn table_storage(fixture: &Fixture, handle: &TxnHandle, name: &str) -> TableId {
    fixture
        .catalog
        .snapshot(handle)
        .table(table_object(fixture, handle, name))
        .expect("the table meta is there")
        .storage_id
}

/// Inserts `(a, b)` pairs into `master.dbo.name` through `storage`.
fn insert_pairs(fixture: &Fixture, handle: &TxnHandle, name: &str, rows: &[(i32, i32)]) {
    let table = table_storage(fixture, handle, name);
    for (a, b) in rows {
        fixture
            .storage
            .insert(handle.id, table, &Row(vec![Value::I32(*a), Value::I32(*b)]))
            .expect("insert");
    }
}

/// Inserts `(a, b, c)` triples into `master.dbo.name`.
fn insert_triples(fixture: &Fixture, handle: &TxnHandle, name: &str, rows: &[(i32, i32, i32)]) {
    let table = table_storage(fixture, handle, name);
    for (a, b, c) in rows {
        fixture
            .storage
            .insert(
                handle.id,
                table,
                &Row(vec![Value::I32(*a), Value::I32(*b), Value::I32(*c)]),
            )
            .expect("insert");
    }
}

/// Inserts one-column rows into `master.dbo.name`.
fn insert_one_col(fixture: &Fixture, handle: &TxnHandle, name: &str, rows: &[i32]) {
    let table = table_storage(fixture, handle, name);
    for value in rows {
        fixture
            .storage
            .insert(handle.id, table, &Row(vec![Value::I32(*value)]))
            .expect("insert");
    }
}

/// Scans `master.dbo.name` and returns the materialised rows.
fn scan_table(fixture: &Fixture, handle: &TxnHandle, name: &str) -> Vec<Vec<Value>> {
    let snapshot = fixture.catalog.snapshot(handle);
    let meta = snapshot
        .table(table_object(fixture, handle, name))
        .expect("the table meta is there");
    let columns: Vec<ColumnBinding> = meta
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| ColumnBinding {
            column: column.id,
            index,
            name: column.name.clone(),
            ty: column.ty.clone(),
        })
        .collect();
    let schema = OutputSchema {
        columns: columns
            .iter()
            .map(|binding| OutputColumn {
                name: binding.name.clone(),
                ty: binding.ty.clone(),
            })
            .collect(),
    };
    let plan = PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns,
        alias: name.to_owned(),
        schema,
        hints: LockHints::default(),
    };
    let snap = fixture.txn.statement_snapshot(handle);
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        fixture.storage.as_ref(),
        &fixture.txn,
        &snap,
    );
    let (_, set) =
        execute_collect(&PhysicalStatement::Query(plan), &mut ctx).expect("the scan runs");
    set.rows
}

/// `ALTER TABLE ADD c int` on a table of two rows: two rows of three columns, the last
/// `NULL`.
#[test]
fn add_nullable_column_keeps_the_rows() {
    let fixture = Fixture::new();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    run_on(&fixture, &handle, "CREATE TABLE dbo.t (a int, b int);").expect("create");
    insert_pairs(&fixture, &handle, "t", &[(1, 2), (3, 4)]);
    let outcome = run_on(&fixture, &handle, "ALTER TABLE dbo.t ADD c int;").expect("alter");
    assert!(matches!(outcome, ExecOutcome::NoRows));
    let rows = scan_table(&fixture, &handle, "t");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 3);
    assert_eq!(rows[0][0], Value::I32(1));
    assert_eq!(rows[0][1], Value::I32(2));
    assert_eq!(rows[0][2], Value::Null);
    assert_eq!(rows[1][2], Value::Null);
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// `ADD c int NOT NULL DEFAULT 7` fills the two existing rows with `7`.
#[test]
fn add_column_with_default_fills_existing_rows() {
    let fixture = Fixture::new();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    run_on(&fixture, &handle, "CREATE TABLE dbo.t (a int, b int);").expect("create");
    insert_pairs(&fixture, &handle, "t", &[(1, 2), (3, 4)]);
    run_on(
        &fixture,
        &handle,
        "ALTER TABLE dbo.t ADD c int NOT NULL DEFAULT 7;",
    )
    .expect("alter");
    let rows = scan_table(&fixture, &handle, "t");
    assert_eq!(rows[0][2], Value::I32(7));
    assert_eq!(rows[1][2], Value::I32(7));
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// `ALTER TABLE dbo.notempty ADD b int NOT NULL;` with one row answers 4901; the table is
/// unchanged after the refusal, and the same instruction on an empty table passes.
#[test]
fn add_not_null_column_without_default_on_a_non_empty_table_is_refused() {
    let fixture = Fixture::new();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    run_on(
        &fixture,
        &handle,
        "CREATE TABLE dbo.notempty (a int NOT NULL);",
    )
    .expect("create");
    insert_one_col(&fixture, &handle, "notempty", &[1]);
    let before = scan_table(&fixture, &handle, "notempty");
    let err = run_on(
        &fixture,
        &handle,
        "ALTER TABLE dbo.notempty ADD b int NOT NULL;",
    )
    .expect_err("NOT NULL without DEFAULT on a non-empty table");
    assert_eq!(err.number, 4901);
    assert_eq!(scan_table(&fixture, &handle, "notempty"), before);

    run_on(
        &fixture,
        &handle,
        "CREATE TABLE dbo.empty_t (a int NOT NULL);",
    )
    .expect("create empty");
    run_on(
        &fixture,
        &handle,
        "ALTER TABLE dbo.empty_t ADD b int NOT NULL;",
    )
    .expect("empty table accepts NOT NULL");
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// `DROP COLUMN b` removes the column from the metadata and from the scan; the other values
/// stay intact.
#[test]
fn drop_column_removes_it() {
    let fixture = Fixture::new();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    run_on(
        &fixture,
        &handle,
        "CREATE TABLE dbo.t (a int, b int, c int);",
    )
    .expect("create");
    insert_triples(&fixture, &handle, "t", &[(1, 2, 7), (3, 4, 8)]);
    run_on(&fixture, &handle, "ALTER TABLE dbo.t DROP COLUMN b;").expect("drop");
    let snapshot = fixture.catalog.snapshot(&handle);
    let meta = snapshot
        .table(table_object(&fixture, &handle, "t"))
        .expect("table");
    assert_eq!(meta.columns.len(), 2);
    assert_eq!(meta.columns[0].name, "a");
    assert_eq!(meta.columns[1].name, "c");
    let rows = scan_table(&fixture, &handle, "t");
    assert_eq!(rows[0], vec![Value::I32(1), Value::I32(7)]);
    assert_eq!(rows[1], vec![Value::I32(3), Value::I32(8)]);
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// `ADD CONSTRAINT … UNIQUE` over duplicate rows answers 1750; without duplicates the
/// constraint is accepted, and a later duplicate `INSERT` answers 2627.
#[test]
fn add_unique_constraint_checks_existing_rows() {
    let fixture = Fixture::new();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    run_on(&fixture, &handle, "CREATE TABLE dbo.dup (a int);").expect("create dup");
    insert_one_col(&fixture, &handle, "dup", &[1, 1]);
    let err = run_on(
        &fixture,
        &handle,
        "ALTER TABLE dbo.dup ADD CONSTRAINT uq_dup UNIQUE (a);",
    )
    .expect_err("duplicate rows refuse UNIQUE");
    assert_eq!(err.number, 1750);

    run_on(&fixture, &handle, "CREATE TABLE dbo.ok (a int);").expect("create ok");
    insert_one_col(&fixture, &handle, "ok", &[1, 2]);
    run_on(
        &fixture,
        &handle,
        "ALTER TABLE dbo.ok ADD CONSTRAINT uq_ok UNIQUE (a);",
    )
    .expect("no duplicate accepts UNIQUE");
    let insert_err = run_on(&fixture, &handle, "INSERT INTO dbo.ok (a) VALUES (1);")
        .expect_err("duplicate INSERT");
    assert_eq!(insert_err.number, 2627);
    fixture
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// A DDL statement answers [`ExecOutcome::NoRows`] with no column schema.
#[test]
fn alter_is_norows() {
    let fixture = Fixture::new();
    run(&fixture, "CREATE TABLE dbo.t (a int);").expect("create");
    let outcome = run(&fixture, "ALTER TABLE dbo.t ADD c int;").expect("alter");
    assert!(matches!(outcome, ExecOutcome::NoRows));
}

/// `ALTER TABLE ADD c` then `rollback` restores the shape and the rows of before.
#[test]
fn alter_rolls_back() {
    let fixture = Fixture::new();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    run_on(&fixture, &handle, "CREATE TABLE dbo.t (a int, b int);").expect("create");
    fixture.txn.commit(handle).expect("commit create");

    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    insert_pairs(&fixture, &handle, "t", &[(1, 2), (3, 4)]);
    fixture.txn.commit(handle).expect("commit the rows");

    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let before = scan_table(&fixture, &handle, "t");
    let object = table_object(&fixture, &handle, "t");
    let storage_before = fixture
        .catalog
        .snapshot(&handle)
        .table(object)
        .expect("table")
        .storage_id;
    run_on(&fixture, &handle, "ALTER TABLE dbo.t ADD c int;").expect("alter");
    fixture.txn.rollback(handle).expect("rollback");

    let reader = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = fixture.catalog.snapshot(&reader);
    let after = snapshot.table(object).expect("table after rollback");
    assert_eq!(after.storage_id, storage_before);
    assert_eq!(after.columns.len(), 2);
    assert_eq!(scan_table(&fixture, &reader, "t"), before);
    fixture
        .txn
        .rollback(reader)
        .expect("the transaction rolls back");
}
