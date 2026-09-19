//! Join of two system views: `dm_exec_sessions` and `dm_exec_requests`.

use std::sync::Arc;

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, LogicalPlan, NoVariables,
    SessionOptions, bind,
};
use vauban_catalog::Catalog;
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, execute_collect};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{PlanContext, StorageIndexes, plan};
use vauban_storage::{MemoryStorage, Row as StorageRow};
use vauban_sysfn::{StaticContext, register_builtins};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{DateTime, SqlString, SqlType, TypeInfo, Value};

struct Fixture {
    catalog: Catalog,
    storage: Arc<dyn vauban_storage::Storage>,
    txn: Arc<TransactionManager>,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn vauban_storage::Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog =
            Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
        let fixture = Self {
            catalog,
            storage,
            txn,
        };
        fixture.seed_session(52);
        fixture
    }

    fn seed_session(&self, spid: i16) {
        let master = self
            .storage
            .databases()
            .expect("databases")
            .into_iter()
            .find(|(_, name)| name.eq_ignore_ascii_case("master"))
            .map(|(id, _)| id)
            .expect("master");
        let smallint = TypeInfo::new(SqlType::SmallInt, false);
        let table = self
            .storage
            .tables(master)
            .expect("tables")
            .into_iter()
            .find(|(_, shape)| shape.columns.len() == 9 && shape.columns.first() == Some(&smallint))
            .map(|(id, _)| id)
            .expect("sessions table");
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let now = Value::DateTime(DateTime {
            days: 25_567,
            ticks_300th: 0,
        });
        let text = |value: &str| {
            Value::String(SqlString {
                text: value.to_owned(),
            })
        };
        let row = StorageRow(vec![
            Value::I16(spid),
            text("sa"),
            text("host"),
            text("test"),
            Value::I32(1),
            now.clone(),
            now,
            text("sleeping"),
            Value::I32(0),
        ]);
        self.storage
            .insert(handle.id, table, &row)
            .expect("insert session");
        self.txn.commit(handle).expect("commit");
    }

    fn run(&self, sql: &str) -> Result<usize, SqlError> {
        register_builtins();
        let batch = parse_batch(sql, &ParseOptions::default())?;
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let snap = self.catalog.snapshot(&handle);
        let bind_ctx = BindContext {
            text: sql,
            catalog: Some(&snap),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        };
        let eval = StaticContext::default();
        let stmt_snap = self.txn.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            self.txn.as_ref(),
            &stmt_snap,
        );
        let bound = bind(&batch.statements[0], &bind_ctx)?;
        let indexes = StorageIndexes(self.storage.as_ref());
        let physical = plan(bound, &PlanContext { catalog: &indexes })?;
        let (outcome, set) = execute_collect(&physical, &mut ctx)?;
        assert!(matches!(outcome, ExecOutcome::Rows(_)));
        Ok(set.rows.len())
    }
}

fn join_on_indices(sql: &str) -> (usize, usize) {
    register_builtins();
    let f = Fixture::new();
    let batch = parse_batch(sql, &ParseOptions::default()).expect("parse");
    let handle = f.txn.begin(IsolationLevel::ReadCommitted);
    let snap = f.catalog.snapshot(&handle);
    let bind_ctx = BindContext {
        text: sql,
        catalog: Some(&snap),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    let bound = bind(&batch.statements[0], &bind_ctx).expect("bind");
    let BoundStatement::Query(plan) = bound else {
        panic!("expected a query");
    };
    let on = join_on(&plan).expect("expected a join with ON");
    let BoundExprKind::Compare { left, right, .. } = &on.kind else {
        panic!("expected ON to be a comparison");
    };
    let index_of = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::ColumnRef(binding) => binding.index,
        other => panic!("not a column ref: {other:?}"),
    };
    (index_of(left), index_of(right))
}

fn join_on(plan: &LogicalPlan) -> Option<&BoundExpr> {
    match plan {
        LogicalPlan::Join { on, .. } => on.as_ref(),
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct(input) => join_on(input),
        _ => None,
    }
}

/// Before `orient_hash_keys`, the planner hands `(s.session_id @ 0, r.session_id @ 52)` to
/// `HashJoin`; evaluating the probe key on a 52-column row raised 50000 on this shape.
#[test]
fn sessions_left_join_requests_returns_a_row() {
    let join_only = "SELECT 1 FROM sys.dm_exec_sessions s \
                     LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id";
    let (left, right) = join_on_indices(join_only);
    assert_eq!(left, 0, "s.session_id indexes the left side");
    assert_eq!(right, 52, "r.session_id indexes past the left width");
    let sql = "SELECT s.session_id, r.command FROM sys.dm_exec_sessions s \
               LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id";
    let count = Fixture::new().run(sql).expect("join should succeed");
    assert!(count >= 1, "expected at least one row, got {count}");
}

#[test]
fn join_predicate_reads_session_id_from_each_side() {
    let sql = "SELECT 1 FROM sys.dm_exec_sessions s \
               LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id";
    Fixture::new()
        .run(sql)
        .expect("ON predicate should not raise 50000");
}
