//! Integration tests for `ALTER DATABASE … SET` of the two versioning options.
//!
//! These tests parse the SQL text, bind it and execute it, which is the path a client
//! takes. The unit tests in `ddl_options.rs` exercise `execute_set_options` directly; this
//! file makes sure the parsing, binding and planning produce the shape `ddl_options.rs`
//! reads.

use std::sync::Arc;

use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_catalog::Catalog;
use vauban_executor::ExecContext;
use vauban_executor::ExecOutcome;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{NoIndexes, PlanContext, plan};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};

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

    fn created_database(&self, name: &str) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.catalog
            .create_database(&handle, name, None)
            .expect("the database is created");
        self.txn.commit(handle).expect("the transaction commits");
    }
}

/// Parses, binds and executes `text` as one statement, inside a transaction of its own.
fn run(fixture: &Fixture, text: &str) -> Result<ExecOutcome, vauban_errors::SqlError> {
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let result = {
        let snapshot = fixture.catalog.snapshot(&handle);
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
        let snap = fixture.txn.statement_snapshot(&handle);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);
        let (outcome, _) = vauban_executor::execute_collect(&planned, &mut ctx)?;
        Ok(outcome)
    };
    match result {
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

/// `ALTER DATABASE vdb SET READ_COMMITTED_SNAPSHOT ON` runs through the parse-bind-plan-execute
/// path and answers [`ExecOutcome::NoRows`].
#[test]
fn rcsi_on_goes_through_the_full_path() {
    let fixture = Fixture::new();
    fixture.created_database("vdb");
    let outcome = run(
        &fixture,
        "ALTER DATABASE vdb SET READ_COMMITTED_SNAPSHOT ON;",
    )
    .expect("RCSI ON binds and executes");
    assert!(matches!(outcome, ExecOutcome::NoRows));
}

/// `ALTER DATABASE vdb SET ALLOW_SNAPSHOT_ISOLATION OFF` through the full path.
#[test]
fn allow_snapshot_isolation_off_goes_through_the_full_path() {
    let fixture = Fixture::new();
    fixture.created_database("vdb");
    let outcome = run(
        &fixture,
        "ALTER DATABASE vdb SET ALLOW_SNAPSHOT_ISOLATION OFF;",
    )
    .expect("ASI OFF binds and executes");
    assert!(matches!(outcome, ExecOutcome::NoRows));
}

/// `WITH ROLLBACK IMMEDIATE` is accepted through the full path.
#[test]
fn with_rollback_immediate_goes_through_the_full_path() {
    let fixture = Fixture::new();
    fixture.created_database("vdb");
    let outcome = run(
        &fixture,
        "ALTER DATABASE vdb SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;",
    )
    .expect("WITH ROLLBACK IMMEDIATE binds and executes");
    assert!(matches!(outcome, ExecOutcome::NoRows));
}
