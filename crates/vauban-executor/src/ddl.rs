//! Running the DDL of databases and tables: `CREATE`/`DROP DATABASE`, `CREATE`/`DROP TABLE`.
//!
//! Each variant of [`DdlStatement`] this file serves calls one method of
//! [`Catalog`](vauban_catalog::Catalog) inside the transaction the caller opened
//! ([`ExecContext::handle`]) and answers [`ExecOutcome::NoRows`]: a DDL statement sends no
//! COLMETADATA and no row, so a client sees a `DONE` and nothing else. `USE` is not here —
//! it changes no metadata and `statement.rs` answers it directly. `CREATE INDEX` and
//! `DROP INDEX` are bound to the same `Ddl` match and routed to `ddl_index.rs`
//! (`ddl::tests::index_statements_reach_ddl_index`).
//!
//! # What this file checks and what it propagates
//!
//! Nothing is re-checked here. 1801 (`CREATE DATABASE` of a name already taken), 2714 (a
//! table name already taken), 3708 (`DROP DATABASE master`) and the 3701 of a `DROP
//! DATABASE` come from `catalog`, already built with their number, severity and state.
//! One of them arrives earlier when the session hands the binder a view of the catalogue:
//! 2714 is then raised at binding time; the tests of this file bind without a view, so it
//! is the catalogue that answers them (`ddl::tests::create_table_twice_is_2714`). This
//! file adds two things the catalogue cannot see: the transaction gate below, and the
//! resolution of the names a `DROP TABLE` was written with.
//!
//! # `IF EXISTS`, and a list of names
//!
//! `DROP DATABASE d1, d2;` and `DROP TABLE t1, t2;` are executed **name by name, left to
//! right**, and the first failure ends the statement with the names after it untouched:
//! `DROP DATABASE d1, nosuch, d2;` answers 3701 for the second name after `d1` has been
//! dropped, and leaves `d2` alone (`ddl::tests::a_list_stops_at_the_first_failure`).
//! `IF EXISTS` swallows the 3701 of a name that is not there and goes on to the next one.
//! It swallows 3701 and nothing else: `DROP DATABASE IF EXISTS master;` still answers the
//! 3708 of a system database
//! (`ddl::tests::drop_system_database_is_3708_even_with_if_exists`).
//!
//! # The transaction gate (226 and 574)
//!
//! `CREATE DATABASE` inside a user transaction answers **226** severity 16 state 5, and
//! `DROP DATABASE` answers **574** severity 16 state 0 — two numbers, not one. `CREATE
//! TABLE` inside a transaction is accepted, so the gate is about those two statements and
//! not about DDL inside a transaction.
//!
//! Both errors are built by [`in_a_user_transaction`], which answers `false` for now: the
//! session opens one transaction per statement, `BEGIN TRANSACTION` is not executed and
//! `@@TRANCOUNT` stays 0. The two numbers are frozen by
//! `ddl::tests::create_database_in_transaction_is_226`, and the gate has one function to
//! fill for them to reach a client.
//!
//! # How a `DROP TABLE` finds its table
//!
//! [`Catalog::drop_table`](vauban_catalog::Catalog::drop_table) takes an
//! [`ObjectId`](vauban_catalog::ObjectId) and the bound statement carries names, so the
//! executor resolves them through [`Catalog::snapshot`](vauban_catalog::Catalog::snapshot)
//! and [`CatalogSnapshot::resolve_object`](vauban_catalog::CatalogSnapshot::resolve_object):
//! a table the session created is dropped
//! (`ddl::tests::drop_table_of_a_created_table_reaches_the_catalogue`) and 3701 is left to
//! the names that resolve to nothing (`ddl::tests::drop_table_unknown_is_3701`).
//!
//! # Deliberate difference: the name printed in the 3701 of a `DROP TABLE`
//!
//! SQL Server prints the name **as written**, delimiters removed, whether that is one, two
//! or three parts: `'nosuch'`, `'dbo.nosuch'` (for `dbo.nosuch` and for `[dbo].[nosuch]`
//! alike) and `'master.dbo.nosuch'`. The binder hands the executor a resolved three-part
//! [`QualifiedName`] and drops what the client wrote, so this file prints `schema.name`:
//! that is the text of the two-part form, and it differs from the bare form and from the
//! three-part one. Keeping the written text asks the binder for a field it does not carry.

use vauban_binder::DdlStatement;
use vauban_catalog::{Catalog, ObjectId, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_txn::TxnHandle;
use vauban_types::Collation;

use crate::context::ExecContext;
use crate::ddl_index::execute_index_ddl;
use crate::row::ExecOutcome;

/// Error 3701, the number a `DROP` of something that is not there answers.
///
/// The `IF EXISTS` of a `DROP` swallows this one and nothing else (module documentation).
const CANNOT_DROP: u32 = 3701;

/// Runs one bound DDL statement and answers [`ExecOutcome::NoRows`].
///
/// # Errors
///
/// What the catalogue raises, unchanged — 1801, 2714, 3701, 3708 — plus the 3701 this file
/// raises for a `DROP TABLE` whose name resolves to nothing, the 226 and 574 of the
/// transaction gate, what `ddl_index.rs` raises for the two index statements, and the
/// internal error 50000 when the context carries no catalogue or no transaction handle
/// ([`ExecContext::catalog`], [`ExecContext::handle`]).
pub(crate) fn execute_ddl(stmt: &DdlStatement, ctx: &ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    match stmt {
        DdlStatement::CreateDatabase { name, collation } => {
            create_database(catalog, handle, ctx, name, *collation)?;
        }
        DdlStatement::DropDatabase { names, if_exists } => {
            drop_databases(catalog, handle, ctx, names, *if_exists)?;
        }
        DdlStatement::CreateTable { def } => {
            catalog.create_table(handle, def)?;
        }
        DdlStatement::DropTable { names, if_exists } => {
            drop_tables(catalog, handle, names, *if_exists)?;
        }
        // The two `ALTER` statements are bound but not executed yet.
        DdlStatement::AlterDatabase { .. } => {
            return Err(SqlError::from(InternalError::Bug(
                "execute_ddl: ALTER DATABASE … SET is not implemented yet".to_owned(),
            )));
        }
        DdlStatement::AlterTable { .. } => {
            return Err(SqlError::from(InternalError::Bug(
                "execute_ddl: ALTER TABLE is not implemented yet".to_owned(),
            )));
        }
        // The two index variants are routed to `ddl_index.rs`, which answers the same
        // `NoRows` and takes the same catalogue and handle.
        DdlStatement::CreateIndex { .. } | DdlStatement::DropIndex { .. } => {
            return execute_index_ddl(stmt, catalog, handle);
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// `CREATE DATABASE d [COLLATE c]`: the gate of 226, then the catalogue.
fn create_database(
    catalog: &Catalog,
    handle: &TxnHandle,
    ctx: &ExecContext<'_>,
    name: &str,
    collation: Option<Collation>,
) -> SqlResult<()> {
    if in_a_user_transaction(ctx) {
        return Err(SqlError::statement_not_allowed_in_transaction(
            "CREATE DATABASE",
        ));
    }
    catalog.create_database(handle, name, collation)?;
    Ok(())
}

/// `DROP DATABASE [IF EXISTS] d1, d2`: the gate of 574, then one call per name.
fn drop_databases(
    catalog: &Catalog,
    handle: &TxnHandle,
    ctx: &ExecContext<'_>,
    names: &[String],
    if_exists: bool,
) -> SqlResult<()> {
    if in_a_user_transaction(ctx) {
        return Err(SqlError::drop_database_in_transaction());
    }
    for name in names {
        match catalog.drop_database(handle, name) {
            Ok(()) => {}
            Err(err) if if_exists && err.number == CANNOT_DROP => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// `DROP TABLE [IF EXISTS] t1, t2`: one resolution and one call per name.
///
/// No transaction gate: `CREATE TABLE` and `DROP TABLE` take part in a user transaction on
/// SQL Server (module documentation).
fn drop_tables(
    catalog: &Catalog,
    handle: &TxnHandle,
    names: &[QualifiedName],
    if_exists: bool,
) -> SqlResult<()> {
    for name in names {
        let resolved = catalog
            .snapshot(handle)
            .resolve_object(&name.database, Some(&name.schema), &name.name, &name.schema)
            .map(|object| object.id);
        let outcome = match resolved {
            Some(id) => drop_one_table(catalog, handle, id),
            None => Err(SqlError::cannot_drop("drop", "table", &printed(name))),
        };
        match outcome {
            Ok(()) => {}
            Err(err) if if_exists && err.number == CANNOT_DROP => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Drops the table of identifier `id`. Split out so that the borrow of the snapshot ends
/// before the call, the snapshot being built per name.
fn drop_one_table(catalog: &Catalog, handle: &TxnHandle, id: ObjectId) -> SqlResult<()> {
    catalog.drop_table(handle, id)
}

/// The name a 3701 of `DROP TABLE` prints: `schema.name`, the deliberate difference of the
/// module documentation.
fn printed(name: &QualifiedName) -> String {
    format!("{}.{}", name.schema, name.name)
}

/// Whether the statement runs inside a transaction the client opened with
/// `BEGIN TRANSACTION`.
///
/// Answers `false` for now, for the reason written in the module documentation: the
/// handle in the context is the one `session` opened for this statement alone, and
/// `BEGIN TRANSACTION` is not executed. Reading the trancount of the session here is what
/// makes the 226 and 574 above reach a client.
fn in_a_user_transaction(ctx: &ExecContext<'_>) -> bool {
    let _ = ctx;
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_catalog::{ColumnId, IndexDef, SortedColumn, TableDef};
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_planner::{NoIndexes, PhysicalPlan, PhysicalStatement, PlanContext, plan};
    use vauban_storage::{DbId, MemoryStorage, Storage, TableId};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo};

    /// A bootstrapped catalogue over an in-memory storage, and the manager its transactions
    /// come from.
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

        /// `master`, as `storage` numbers it: the database the bound statements of these
        /// tests name (`BindContext::scalar`).
        fn master(&self) -> DbId {
            self.storage
                .databases()
                .expect("the databases are listed")
                .into_iter()
                .find(|(_, name)| name == "master")
                .expect("the bootstrap created master")
                .0
        }

        /// The tables `storage` holds in `master`, by identifier.
        fn tables(&self) -> Vec<TableId> {
            self.storage
                .tables(self.master())
                .expect("master is known")
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        }
    }

    /// Parses, binds and executes `text` as one statement, in a transaction of its own.
    ///
    /// The shape `session` builds: one handle per statement, the catalogue and the engine
    /// in the context, and the transaction committed when the statement succeeded.
    fn run(fixture: &Fixture, text: &str) -> SqlResult<ExecOutcome> {
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

    /// The same, inside a transaction the caller owns: neither committed nor rolled back
    /// here, so a test can look at what the statement did before either happens.
    fn run_on(fixture: &Fixture, handle: &TxnHandle, text: &str) -> SqlResult<ExecOutcome> {
        let bound = bound(text);
        let snap = fixture.txn.statement_snapshot(handle);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
            .with_catalog(&fixture.catalog)
            .with_handle(handle);
        crate::statement::execute_collect(&bound, &mut ctx).map(|(outcome, _)| outcome)
    }

    /// The single statement of `text`, bound against the scalar context (`master`, `dbo`)
    /// and planned.
    fn bound(text: &str) -> PhysicalStatement {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        assert_eq!(batch.statements.len(), 1, "one statement per call");
        let bind_ctx = BindContext::scalar(text, SessionOptions::default());
        let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
        plan(
            bound,
            &PlanContext {
                catalog: &NoIndexes,
            },
        )
        .expect("the statement plans")
    }

    /// A `TableDef` of one `int` column named `name` in `master.dbo`, built without the
    /// parser for the tests that need a second table.
    fn table_def(name: &str) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns: vec![vauban_catalog::ColumnDef {
                name: "a".to_owned(),
                ty: TypeInfo::new(SqlType::Int, true),
                default: None,
                identity: None,
                computed: None,
            }],
            constraints: Vec::new(),
        }
    }

    /// A `CREATE TABLE` bound and executed creates the table, and a `Scan` of it answers
    /// zero row and the schema of its columns.
    ///
    /// The identifier of the created table is read as the one `storage` did not hold before
    /// the statement, so the test does not guess how `MemoryStorage` numbers its tables.
    #[test]
    fn create_table_then_scan_is_empty() {
        use vauban_binder::{ColumnBinding, OutputColumn, OutputSchema};

        let fixture = Fixture::new();
        let before = fixture.tables();
        let outcome = run(&fixture, "CREATE TABLE dbo.t (a int);").expect("the table is created");
        assert!(
            matches!(outcome, ExecOutcome::NoRows),
            "a DDL statement answers NoRows"
        );
        let after = fixture.tables();
        let created: Vec<TableId> = after
            .iter()
            .filter(|id| !before.contains(id))
            .copied()
            .collect();
        assert_eq!(created.len(), 1, "the statement created one table");

        let ty = TypeInfo::new(SqlType::Int, true);
        let plan = PhysicalPlan::TableScan {
            table: created[0],
            columns: vec![ColumnBinding {
                column: ColumnId(1),
                index: 0,
                name: "a".to_owned(),
                ty: ty.clone(),
            }],
            alias: "t".to_owned(),
            schema: OutputSchema {
                columns: vec![OutputColumn {
                    name: "a".to_owned(),
                    ty,
                }],
            },
        };
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let snap = fixture.txn.statement_snapshot(&handle);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            fixture.storage.as_ref(),
            &fixture.txn,
            &snap,
        );
        let (_, set) = crate::statement::execute_collect(&PhysicalStatement::Query(plan), &mut ctx)
            .expect("the scan runs");
        assert_eq!(set.rows.len(), 0, "a table just created holds no row");
        assert_eq!(set.schema.columns.len(), 1);
        assert_eq!(set.schema.columns[0].name, "a");
    }

    /// The counter-proof of the test above: the same statement twice answers 2714, the
    /// number the catalogue builds, so the first call reached the catalogue and not
    /// `storage` alone.
    #[test]
    fn create_table_twice_is_2714() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int);").expect("the table is created");
        let after_first = fixture.tables();
        let err = run(&fixture, "CREATE TABLE dbo.t (a int);").expect_err("the name is taken");
        assert_eq!(err.number, 2714);
        assert_eq!(
            fixture.tables(),
            after_first,
            "the refused statement created nothing in storage"
        );
    }

    /// `DROP TABLE` of a name that resolves to nothing answers 3701, severity 11, state 5,
    /// and the message prints `schema.name`.
    #[test]
    fn drop_table_unknown_is_3701() {
        let fixture = Fixture::new();
        let err = run(&fixture, "DROP TABLE dbo.nosuch;").expect_err("no such table");
        assert_eq!(err.number, 3701);
        assert_eq!(err.severity, 11);
        assert_eq!(err.state, 5);
        assert!(err.message.contains("'dbo.nosuch'"), "{}", err.message);
    }

    /// `DROP TABLE IF EXISTS` of the same name answers `NoRows`: the 3701 of
    /// `drop_table_unknown_is_3701` is swallowed, which is what makes the pair a vector for
    /// `if_exists`.
    #[test]
    fn drop_table_if_exists_unknown_is_norows() {
        let fixture = Fixture::new();
        let outcome = run(&fixture, "DROP TABLE IF EXISTS dbo.nosuch;").expect("IF EXISTS");
        assert!(matches!(outcome, ExecOutcome::NoRows));
    }

    /// A table this catalogue created resolves, and the `DROP TABLE` of it reaches the
    /// catalogue.
    ///
    /// The counter-proof is `drop_table_unknown_is_3701`, which keeps 3701 for `dbo.nosuch`:
    /// the two together say the resolution answers on the name it was given, rather than
    /// accepting the two names alike.
    #[test]
    fn drop_table_of_a_created_table_reaches_the_catalogue() {
        let fixture = Fixture::new();
        let before = fixture.tables();
        run(&fixture, "CREATE TABLE dbo.t (a int);").expect("the table is created");
        assert_eq!(fixture.tables().len(), before.len() + 1);
        let outcome = run(&fixture, "DROP TABLE dbo.t;").expect("the table resolves");
        assert!(matches!(outcome, ExecOutcome::NoRows));
        assert_eq!(
            fixture.tables(),
            before,
            "the committed DROP took the table out of storage"
        );
    }

    /// `CREATE DATABASE` reaches the catalogue: the database is in `storage` afterwards, and
    /// a second statement of the same name answers 1801.
    #[test]
    fn create_database_then_1801() {
        let fixture = Fixture::new();
        let outcome = run(&fixture, "CREATE DATABASE d;").expect("the database is created");
        assert!(matches!(outcome, ExecOutcome::NoRows));
        let names: Vec<String> = fixture
            .storage
            .databases()
            .expect("the databases are listed")
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert!(names.iter().any(|name| name == "d"), "{names:?}");
        let err = run(&fixture, "CREATE DATABASE d;").expect_err("the name is taken");
        assert_eq!(err.number, 1801);
    }

    /// `DROP DATABASE master` answers the 3708 of the catalogue, and `IF EXISTS` does not
    /// swallow it — 3701 is the number `IF EXISTS` swallows, and the four assertions below
    /// check 3708 surviving it, 3708 without it, and the 3701 pair of a missing name.
    #[test]
    fn drop_system_database_is_3708_even_with_if_exists() {
        let fixture = Fixture::new();
        let plain = run(&fixture, "DROP DATABASE master;").expect_err("master is a system one");
        assert_eq!(plain.number, 3708);
        let guarded =
            run(&fixture, "DROP DATABASE IF EXISTS master;").expect_err("IF EXISTS is not 3708");
        assert_eq!(guarded.number, 3708);
        let absent = run(&fixture, "DROP DATABASE IF EXISTS nosuch;").expect("IF EXISTS");
        assert!(matches!(absent, ExecOutcome::NoRows));
        let err = run(&fixture, "DROP DATABASE nosuch;").expect_err("no such database");
        assert_eq!(err.number, 3701);
    }

    /// A list of names is executed left to right and stops at the first failure: after
    /// `DROP DATABASE d1, nosuch, d2;`, `d1` is dropped and `d2` is not.
    ///
    /// Read inside the transaction of the statement, by asking the catalogue to drop each
    /// of the two again: `d1` answers 3701 because the failed statement had already dropped
    /// it, `d2` answers `Ok` because the statement stopped before reaching it. The
    /// counter-proof is that pair — under a right-to-left order, or under a statement that
    /// checked the three names before touching one of them, the two answers would be
    /// swapped or equal.
    ///
    /// What the caller does with that transaction is not decided here: SQL Server keeps the
    /// first drop after the error, while a caller that rolls the statement back gets `d1`
    /// again — statement atomicity is not this file's.
    #[test]
    fn a_list_stops_at_the_first_failure() {
        let fixture = Fixture::new();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        run_on(&fixture, &handle, "CREATE DATABASE d1;").expect("d1");
        run_on(&fixture, &handle, "CREATE DATABASE d2;").expect("d2");
        let err = run_on(&fixture, &handle, "DROP DATABASE d1, nosuch, d2;")
            .expect_err("nosuch is not there");
        assert_eq!(err.number, 3701);
        assert_eq!(
            fixture
                .catalog
                .drop_database(&handle, "d1")
                .expect_err("d1 was dropped by the statement")
                .number,
            3701
        );
        fixture
            .catalog
            .drop_database(&handle, "d2")
            .expect("the statement stopped before d2");
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }

    /// The two errors of the transaction gate, frozen by number, severity and state, and
    /// the state of the gate.
    ///
    /// The gate itself answers `false` here, so the two numbers do not reach a client yet:
    /// the last two assertions are that counter-proof.
    #[test]
    fn create_database_in_transaction_is_226() {
        let refused = SqlError::statement_not_allowed_in_transaction("CREATE DATABASE");
        assert_eq!(refused.number, 226);
        assert_eq!(refused.severity, 16);
        assert_eq!(refused.state, 5);
        assert!(
            refused.message.contains("CREATE DATABASE"),
            "{}",
            refused.message
        );
        // `DROP DATABASE` is not a filling of 226: it answers 574.
        let dropped = SqlError::drop_database_in_transaction();
        assert_eq!(dropped.number, 574);
        assert_eq!(dropped.severity, 16);
        assert_eq!(dropped.state, 0);

        let fixture = Fixture::new();
        let eval = StaticContext::default();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);
        assert!(
            !in_a_user_transaction(&ctx),
            "one transaction per statement, no user transaction"
        );
        assert!(
            run(&fixture, "CREATE DATABASE d;").is_ok(),
            "the gate is shut, so the statement runs"
        );
    }

    /// Each of the two index statements reaching this file is routed to `ddl_index.rs` and
    /// answers its [`ExecOutcome::NoRows`], rather than an internal 50000.
    ///
    /// Both arms are exercised, `CreateIndex` and `DropIndex`: the variant decides nothing
    /// else in `execute_ddl`, so a test of one arm would say nothing about the other. The two
    /// are built by hand — a `CREATE INDEX` cannot bind without a catalogue view — over a
    /// table `catalog.create_table` has just made. That the routing is real and not a
    /// second copy of the dispatch is
    /// `ddl_index::tests::a_statement_that_is_not_an_index_one_is_a_bug`, which refuses the
    /// four variants this file keeps.
    #[test]
    fn index_statements_reach_ddl_index() {
        let fixture = Fixture::new();
        let eval = StaticContext::default();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let table = fixture
            .catalog
            .create_table(&handle, &table_def("t"))
            .expect("the table is created");
        let ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);

        let created = DdlStatement::CreateIndex {
            def: IndexDef {
                table: table.id,
                name: "ix".to_owned(),
                columns: vec![SortedColumn {
                    column: "a".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        };
        assert!(matches!(
            execute_ddl(&created, &ctx).expect("the index is created"),
            ExecOutcome::NoRows
        ));
        let dropped = DdlStatement::DropIndex {
            name: "ix".to_owned(),
            table: table_def("t").name,
            if_exists: true,
        };
        assert!(matches!(
            execute_ddl(&dropped, &ctx).expect("IF EXISTS swallows the 3701"),
            ExecOutcome::NoRows
        ));
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }

    /// Without a catalogue or without a handle, a DDL statement is the internal 50000 of
    /// the context, as a `Scan` without storage is.
    #[test]
    fn a_context_without_a_handle_is_a_bug() {
        let fixture = Fixture::new();
        let eval = StaticContext::default();
        let stmt = DdlStatement::CreateTable {
            def: table_def("t"),
        };
        let no_catalog = ExecContext::scalar(&eval, SessionOptions::default());
        assert_eq!(
            execute_ddl(&stmt, &no_catalog)
                .expect_err("no catalogue")
                .number,
            50000
        );
        let no_handle =
            ExecContext::scalar(&eval, SessionOptions::default()).with_catalog(&fixture.catalog);
        assert_eq!(
            execute_ddl(&stmt, &no_handle)
                .expect_err("no handle")
                .number,
            50000
        );
    }
}
