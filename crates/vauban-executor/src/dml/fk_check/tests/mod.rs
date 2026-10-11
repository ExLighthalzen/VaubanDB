//! What a statement checks, read under the snapshot of its transaction.

use std::sync::Arc;

use vauban_binder::SessionOptions;
use vauban_catalog::{
    AlterTable, Catalog, ColumnDef, ConstraintDef, QualifiedName, SortedColumn, TableDef,
};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo, Value};

use crate::context::CancelToken;
use crate::row::ExecOutcome;

use super::*;

fn name(table: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: table.to_owned(),
    }
}

fn column(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, false),
        default: None,
        identity: None,
        computed: None,
    }
}

/// A bootstrapped catalogue holding `dbo.p (a PRIMARY KEY)` and `dbo.c (a)`, committed.
struct Fixture {
    txn: Arc<TransactionManager>,
    catalog: Catalog,
    parent: ObjectId,
    child: ObjectId,
}

impl Fixture {
    fn new() -> Self {
        vauban_sysfn::register_builtins();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let parent = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: name("p"),
                    columns: vec![column("a")],
                    constraints: vec![ConstraintDef::PrimaryKey {
                        name: Some("pk_p".to_owned()),
                        columns: vec![SortedColumn {
                            column: "a".to_owned(),
                            descending: false,
                        }],
                        clustered: true,
                    }],
                },
            )
            .expect("p is created")
            .id;
        let child = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: name("c"),
                    columns: vec![column("a")],
                    constraints: Vec::new(),
                },
            )
            .expect("c is created")
            .id;
        txn.commit(handle).expect("the tables commit");
        Self {
            txn,
            catalog,
            parent,
            child,
        }
    }

    /// What a `DELETE` of `table` in the transaction `handle` checks.
    fn checks(&self, handle: &TxnHandle, table: ObjectId) -> WriteChecks {
        let snap = self.catalog.snapshot(handle);
        let meta = snap.table(table).expect("the table is in the snapshot");
        let eval = StaticContext::default();
        let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_catalog(&self.catalog);
        WriteChecks::resolve(meta, &snap, &[], "DELETE", true, &ctx)
            .expect("the constraints resolve")
    }
}

fn rule_names(checks: &WriteChecks) -> Vec<&str> {
    checks
        .rules
        .iter()
        .map(|rule| match rule {
            Rule::Check { name, .. } | Rule::ForeignKey { name, .. } => name.as_str(),
        })
        .collect()
}

fn incoming_names(checks: &WriteChecks) -> Vec<&str> {
    checks
        .incoming
        .iter()
        .map(|reference| reference.name.as_str())
        .collect()
}

/// A `FOREIGN KEY` of `c` towards `p` transaction A has added and not committed: the checks
/// of a statement of transaction B leave it out on both tables, those of A hold it on both,
/// by its name.
#[test]
fn a_foreign_key_another_transaction_has_not_committed_is_not_checked() {
    let fixture = Fixture::new();
    let a = fixture.txn.begin(IsolationLevel::ReadCommitted);
    fixture
        .catalog
        .alter_table(
            &a,
            fixture.child,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::ForeignKey {
                    name: Some("fk_c_p".to_owned()),
                    columns: vec!["a".to_owned()],
                    referenced: name("p"),
                    referenced_columns: vec!["a".to_owned()],
                    on_delete: vauban_catalog::RefAction::NoAction,
                    on_update: vauban_catalog::RefAction::NoAction,
                }),
            },
        )
        .expect("A adds the foreign key");
    let b = fixture.txn.begin(IsolationLevel::ReadCommitted);

    assert!(incoming_names(&fixture.checks(&b, fixture.parent)).is_empty());
    assert!(rule_names(&fixture.checks(&b, fixture.child)).is_empty());
    assert_eq!(
        incoming_names(&fixture.checks(&a, fixture.parent)),
        ["fk_c_p"]
    );
    assert_eq!(rule_names(&fixture.checks(&a, fixture.child)), ["fk_c_p"]);

    fixture.txn.rollback(b).expect("B rolls back");
    fixture.txn.rollback(a).expect("A rolls back");
}

/// A bootstrapped catalogue over an in-memory storage, written to through SQL text.
struct Engine {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Engine {
    fn new() -> Self {
        vauban_sysfn::register_builtins();
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

    /// Parses, binds, plans and runs `text`, one statement, inside `handle`; the rows of a
    /// query.
    fn run(&self, handle: &TxnHandle, text: &str) -> SqlResult<Vec<Vec<Value>>> {
        let eval = StaticContext::default();
        self.run_with(handle, text, &eval, &CancelToken::new())
            .map(|(_, rows)| rows)
    }

    /// [`Engine::run`] under the session `eval` and the cancellation token `cancel`; the
    /// outcome and the rows of a query.
    fn run_with(
        &self,
        handle: &TxnHandle,
        text: &str,
        eval: &dyn vauban_sysfn::EvalContext,
        cancel: &CancelToken,
    ) -> SqlResult<(ExecOutcome, Vec<Vec<Value>>)> {
        let snapshot = self.catalog.snapshot(handle);
        let batch = vauban_parser::parse_batch(text, &vauban_parser::ParseOptions::default())
            .expect("the text parses");
        assert_eq!(batch.statements.len(), 1, "one statement per call");
        let mut bind_ctx = vauban_binder::BindContext::scalar(text, SessionOptions::default());
        bind_ctx.catalog = Some(&snapshot);
        let bound = vauban_binder::bind(&batch.statements[0], &bind_ctx)?;
        let planned = vauban_planner::plan(
            bound,
            &vauban_planner::PlanContext {
                catalog: &vauban_planner::NoIndexes,
            },
        )?;
        let snap = self.txn.statement_snapshot(handle);
        let mut ctx = ExecContext::scalar(eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_catalog(&self.catalog)
            .with_handle(handle)
            .with_cancel(cancel);
        let (outcome, rows) = crate::execute_collect(&planned, &mut ctx)?;
        Ok((outcome, rows.rows))
    }

    /// A transaction holding `dbo.s (id PRIMARY KEY, parent REFERENCES dbo.s (id))` and the
    /// rows of `insert`, when there is one.
    fn hierarchy(&self, insert: Option<&str>) -> TxnHandle {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.run(
            &handle,
            "CREATE TABLE dbo.s (id int NOT NULL PRIMARY KEY, \
             parent int NULL CONSTRAINT fk_s REFERENCES dbo.s (id));",
        )
        .expect("the table is created");
        if let Some(insert) = insert {
            self.run(&handle, insert).expect("the rows are inserted");
        }
        handle
    }

    /// The `(id, parent)` rows of `dbo.s`, by `id`.
    fn rows(&self, handle: &TxnHandle) -> Vec<(i32, Option<i32>)> {
        self.run(handle, "SELECT id, parent FROM dbo.s ORDER BY id;")
            .expect("the table reads")
            .into_iter()
            .map(|row| match row.as_slice() {
                [Value::I32(id), Value::I32(parent)] => (*id, Some(*parent)),
                [Value::I32(id), Value::Null] => (*id, None),
                other => panic!("unexpected row {other:?}"),
            })
            .collect()
    }
}

/// Asserts that `outcome` is the 547 of `statement` over the constraint kind `kind` of
/// `fk_s`, naming `column`.
fn assert_same_table_547<T: std::fmt::Debug>(
    outcome: SqlResult<T>,
    statement: &str,
    kind: &str,
    column: &str,
) {
    let err = outcome.expect_err("the statement breaks fk_s");
    assert_eq!((err.number, err.severity, err.state), (547, 16, 0));
    assert_eq!(
        err.message,
        format!(
            "{statement} violates the {kind} constraint \"fk_s\" in database \"master\", \
             table \"dbo.s\", column '{column}'."
        )
    );
}

/// A row naming itself as its parent, a row naming a row inserted by an earlier statement,
/// and rows of one `INSERT` naming each other, in either order, are written; a parent no
/// row holds is the 547 of a `FOREIGN KEY SAME TABLE`.
#[test]
fn a_row_may_reference_itself_or_a_row_of_the_same_statement() {
    let engine = Engine::new();
    let handle = engine.hierarchy(None);
    engine
        .run(&handle, "INSERT INTO dbo.s VALUES (1, 1);")
        .expect("a row may be its own parent");
    engine
        .run(&handle, "INSERT INTO dbo.s VALUES (2, 1);")
        .expect("a row of an earlier statement is a parent");
    engine
        .run(&handle, "INSERT INTO dbo.s VALUES (3, 4), (4, 3);")
        .expect("rows of one statement may name each other");
    engine
        .run(&handle, "INSERT INTO dbo.s VALUES (6, 5), (5, NULL);")
        .expect("a child may come before its parent in the statement");
    assert_eq!(
        engine.rows(&handle),
        [
            (1, Some(1)),
            (2, Some(1)),
            (3, Some(4)),
            (4, Some(3)),
            (5, None),
            (6, Some(5))
        ]
    );
    assert_same_table_547(
        engine.run(&handle, "INSERT INTO dbo.s VALUES (7, 8);"),
        "INSERT",
        "FOREIGN KEY SAME TABLE",
        "id",
    );
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// Deleting every row of a hierarchy leaves no orphan and is allowed, a row that is its own
/// parent included; deleting a parent a remaining row references is the 547 of a
/// `SAME TABLE REFERENCE`; deleting a leaf is allowed.
#[test]
fn deleting_a_whole_hierarchy_leaves_no_orphan() {
    let engine = Engine::new();
    let insert = "INSERT INTO dbo.s VALUES (1, 1), (2, 1), (3, 2);";
    let handle = engine.hierarchy(Some(insert));
    engine
        .run(&handle, "DELETE FROM dbo.s;")
        .expect("the whole hierarchy goes at once");
    assert!(engine.rows(&handle).is_empty());
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");

    let handle = engine.hierarchy(Some(insert));
    assert_same_table_547(
        engine.run(&handle, "DELETE FROM dbo.s WHERE id = 1;"),
        "DELETE",
        "SAME TABLE REFERENCE",
        "parent",
    );
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");

    let handle = engine.hierarchy(Some(insert));
    engine
        .run(&handle, "DELETE FROM dbo.s WHERE id = 3;")
        .expect("a leaf goes");
    assert_eq!(engine.rows(&handle), [(1, Some(1)), (2, Some(1))]);
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// Changing the key of a referenced parent is the 547 of a `SAME TABLE REFERENCE`, even when
/// the only row referencing it is the row itself; changing the parent and its children in
/// one statement, or changing an unreferenced key, is allowed; pointing a
/// child at a missing parent is the 547 of a `FOREIGN KEY SAME TABLE`.
#[test]
fn updating_a_referenced_key_reads_the_rows_the_statement_leaves() {
    let engine = Engine::new();
    for (insert, update, rows) in [
        (
            "INSERT INTO dbo.s VALUES (1, 1), (2, 1);",
            "UPDATE dbo.s SET id = id + 10, parent = parent + 10;",
            vec![(11, Some(11)), (12, Some(11))],
        ),
        (
            "INSERT INTO dbo.s VALUES (1, 1), (2, 1), (3, NULL);",
            "UPDATE dbo.s SET id = 30 WHERE id = 3;",
            vec![(1, Some(1)), (2, Some(1)), (30, None)],
        ),
    ] {
        let handle = engine.hierarchy(Some(insert));
        engine
            .run(&handle, update)
            .unwrap_or_else(|err| panic!("{update}: {err:?}"));
        assert_eq!(engine.rows(&handle), rows, "{update}");
        engine
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }
    for (insert, update, kind, column) in [
        (
            "INSERT INTO dbo.s VALUES (1, 1), (2, 1);",
            "UPDATE dbo.s SET id = 10 WHERE id = 1;",
            "SAME TABLE REFERENCE",
            "parent",
        ),
        (
            "INSERT INTO dbo.s VALUES (1, 1), (2, NULL);",
            "UPDATE dbo.s SET id = 10 WHERE id = 1;",
            "SAME TABLE REFERENCE",
            "parent",
        ),
        (
            "INSERT INTO dbo.s VALUES (1, 1), (2, 1);",
            "UPDATE dbo.s SET parent = 5 WHERE id = 2;",
            "FOREIGN KEY SAME TABLE",
            "id",
        ),
    ] {
        let handle = engine.hierarchy(Some(insert));
        assert_same_table_547(engine.run(&handle, update), "UPDATE", kind, column);
        engine
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }
}

/// A session whose `@@SPID` raises a cancellation token on its `at`-th read, so that a
/// statement is cancelled while it reads the row that reads it; `@@SPID` answers 1.
struct CancellingSession {
    inner: StaticContext,
    token: CancelToken,
    reads: std::cell::Cell<usize>,
    at: usize,
}

impl CancellingSession {
    fn new(at: usize) -> Self {
        Self {
            inner: StaticContext::default(),
            token: CancelToken::new(),
            reads: std::cell::Cell::new(0),
            at,
        }
    }
}

impl vauban_sysfn::EvalContext for CancellingSession {
    fn now_local(&self) -> vauban_types::DateTime2 {
        self.inner.now_local()
    }
    fn now_utc(&self) -> vauban_types::DateTime2 {
        self.inner.now_utc()
    }
    fn rowcount(&self) -> i64 {
        self.inner.rowcount()
    }
    fn last_identity(&self) -> Option<vauban_types::Decimal> {
        self.inner.last_identity()
    }
    fn spid(&self) -> i16 {
        let reads = self.reads.get() + 1;
        self.reads.set(reads);
        if reads == self.at {
            self.token.cancel();
        }
        1
    }
    fn current_database(&self) -> &str {
        self.inner.current_database()
    }
    fn server_name(&self) -> &str {
        self.inner.server_name()
    }
    fn object_id(&self, name: &str) -> Option<i32> {
        self.inner.object_id(name)
    }
    fn object_name(&self, id: i32) -> Option<String> {
        self.inner.object_name(id)
    }
    fn variable(&self, name: &str) -> Option<Value> {
        self.inner.variable(name)
    }
}

/// An `INSERT … SELECT` cancelled after it wrote a row whose parent the statement had not
/// written yet does not keep that row unchecked: the key it set aside is checked when the
/// statement stops, and the statement is the 547. Without an orphan, the cancelled
/// statement keeps its rows.
#[test]
fn a_cancelled_insert_leaves_no_row_without_its_parent() {
    let engine = Engine::new();
    let handle = engine.hierarchy(None);
    // The third source row reads `@@SPID`, which raises the token before that row is
    // written: rows 1 and 2 are written, and row 2 names a parent 3 no row holds.
    let insert = "INSERT INTO dbo.s SELECT v.id, v.parent FROM \
                  (VALUES (1, 1), (2, 3), (3, @@SPID)) AS v (id, parent);";
    let session = CancellingSession::new(1);
    let outcome = engine.run_with(&handle, insert, &session, &session.token);
    assert!(session.token.is_cancelled(), "the token was raised");
    assert_same_table_547(outcome, "INSERT", "FOREIGN KEY SAME TABLE", "id");
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");

    let handle = engine.hierarchy(None);
    let insert = "INSERT INTO dbo.s SELECT v.id, v.parent FROM \
                  (VALUES (1, 1), (2, 1), (3, @@SPID)) AS v (id, parent);";
    let session = CancellingSession::new(1);
    let (outcome, _) = engine
        .run_with(&handle, insert, &session, &session.token)
        .expect("no row breaks the foreign key");
    assert!(matches!(outcome, ExecOutcome::Cancelled), "{outcome:?}");
    assert_eq!(engine.rows(&handle), [(1, Some(1)), (2, Some(1))]);
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// A `DELETE` cancelled after it removed a parent a remaining row references does not keep
/// that removal unchecked: the statement is the 547.
#[test]
fn a_cancelled_delete_leaves_no_row_without_its_parent() {
    let engine = Engine::new();
    let handle = engine.hierarchy(Some("INSERT INTO dbo.s VALUES (1, 1), (2, 1), (3, 2);"));
    // The predicate reads `@@SPID` once per row; the third read raises the token, before
    // row 3 is deleted: rows 1 and 2 are gone, and row 3 names the parent 2.
    let session = CancellingSession::new(3);
    let outcome = engine.run_with(
        &handle,
        "DELETE FROM dbo.s WHERE id > @@SPID - 1;",
        &session,
        &session.token,
    );
    assert!(session.token.is_cancelled(), "the token was raised");
    assert_same_table_547(outcome, "DELETE", "SAME TABLE REFERENCE", "parent");
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}

/// A key of several columns names no column in the 547 of a foreign key of a table
/// towards itself.
#[test]
fn a_composite_key_names_no_column() {
    let engine = Engine::new();
    let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
    engine
        .run(
            &handle,
            "CREATE TABLE dbo.z (a int NOT NULL, b int NOT NULL, pa int NULL, pb int NULL, \
             CONSTRAINT pk_z PRIMARY KEY (a, b), \
             CONSTRAINT fk_z FOREIGN KEY (pa, pb) REFERENCES dbo.z (a, b));",
        )
        .expect("the table is created");
    let err = engine
        .run(&handle, "INSERT INTO dbo.z VALUES (1, 1, 2, 2);")
        .expect_err("no row holds (2, 2)");
    assert_eq!((err.number, err.severity, err.state), (547, 16, 0));
    assert_eq!(
        err.message,
        "INSERT violates the FOREIGN KEY SAME TABLE constraint \"fk_z\" in database \
         \"master\", table \"dbo.z\"."
    );
    engine
        .txn
        .rollback(handle)
        .expect("the transaction rolls back");
}
