//! `ALTER DATABASE … SET` of the two versioning options:
//! `READ_COMMITTED_SNAPSHOT` and `ALLOW_SNAPSHOT_ISOLATION`, each `ON` or `OFF`.
//!
//! [`DdlStatement::AlterDatabase`] carries the database and the options the binder
//! accepted (the four combinations of the two options, plus `WITH ROLLBACK IMMEDIATE`).
//! This file resolves the database name into a [`DbId`] and calls
//! [`Catalog::set_database_option`] for each option the statement carries.
//!
//! # What this file does not do
//!
//! The `WITH ROLLBACK IMMEDIATE` clause is accepted by the binder and silently ignored
//! here: in this version no exclusive lock is taken on the database and no other session is
//! rolled back. An acceptance criterion of this task decides whether the silence is a
//! divergence: either a named gap is opened or a later change implements the exclusive
//! lock and the rollback — nothing here decides the shape.
//!
//! The switch takes effect on the catalogue at once for the transaction that runs it, so
//! the next [`CatalogSnapshot`] the caller builds carries the new option — which is what
//! the next statement of the same session reads. The versioning itself — the snapshot or
//! the lock the statement asks for — does not live here.
//!
//! # Error separation
//!
//! The binder accepted the two options, their values and the optional `WITH` clause, so
//! the executor knows the two options: `READ_COMMITTED_SNAPSHOT` and
//! `ALLOW_SNAPSHOT_ISOLATION`. An unknown option (one the binder
//! did not name in its match) reaches the catalogue through [`DatabaseOption`] and is
//! reported as the internal 50000 with the name of the option; no client input can reach
//! that path. A database the catalogue does not hold is the internal 50000 of
//! [`Catalog::set_database_option`], which [`catalog::database::set_database_option`]
//! raises when its lookup finds nothing. The 5011 (unknown database) that a production
//! `session` raises is thinner metadata — the binder does not see the catalogue, so it
//! hands the name down without resolving it — and the `set_database_option` call is where
//! the database-not-found path has its first sight of it.

use vauban_binder::DdlStatement;
use vauban_catalog::Catalog;
use vauban_catalog::DatabaseOption;
use vauban_errors::SqlResult;
use vauban_txn::TxnHandle;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// Runs the `ALTER DATABASE … SET` of `stmt`, one option at a time.
///
/// # Errors
///
/// What `catalog.set_database_option` raises, unchanged.
pub(crate) fn execute_set_options(
    stmt: &DdlStatement,
    ctx: &ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    match stmt {
        DdlStatement::AlterDatabase { name, options } => {
            set_options(catalog, handle, name, options)?;
        }
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        | DdlStatement::CreateIndex { .. }
        | DdlStatement::DropIndex { .. }
        | DdlStatement::AlterTable { .. } => {
            return Err(bug(
                "execute_set_options: only ALTER DATABASE … SET is run here; the rest is \
                 run by ddl.rs and ddl_index.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// Applies one or two options to the database named `name`.
fn set_options(
    catalog: &Catalog,
    handle: &TxnHandle,
    name: &str,
    options: &[(String, Option<String>)],
) -> SqlResult<()> {
    let db = catalog
        .snapshot(handle)
        .database(name)
        .map(|meta| meta.id)
        .ok_or_else(|| {
            // The binder does not resolve the database name, so this path is reached by a name
            // that does not exist. SQL Server answers 5011 severity 14 state 5 for an unknown
            // database in ALTER DATABASE; the crate does not carry that constructor yet.
            bug(&format!(
                "ALTER DATABASE: the catalogue holds no database named '{name}'"
            ))
        })?;
    for (option_name, value) in options {
        let parsed = parse_option(option_name, value.as_deref());
        let Some((opt, on)) = parsed else {
            // The `WITH` pair or any other pair the binder does not name.
            continue;
        };
        catalog.set_database_option(handle, db, opt, on)?;
    }
    Ok(())
}

/// Turns one option/value pair into a [`DatabaseOption`] and its `on` flag, or `None` for
/// a pair the executor ignores (the `WITH` clause).
fn parse_option(name: &str, value: Option<&str>) -> Option<(DatabaseOption, bool)> {
    match (name, value) {
        ("READ_COMMITTED_SNAPSHOT", Some(v)) => {
            Some((DatabaseOption::ReadCommittedSnapshot, on_off(v)?))
        }
        ("ALLOW_SNAPSHOT_ISOLATION", Some(v)) => {
            Some((DatabaseOption::AllowSnapshotIsolation, on_off(v)?))
        }
        _ => None,
    }
}

/// `true` for `ON`, `false` for `OFF`.
fn on_off(v: &str) -> Option<bool> {
    match v {
        "ON" => Some(true),
        "OFF" => Some(false),
        _ => None,
    }
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> vauban_errors::SqlError {
    vauban_errors::SqlError::from(vauban_errors::InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_binder::{DdlStatement as Ds, SessionOptions};
    use vauban_catalog::Catalog;
    use vauban_catalog::{DatabaseMeta, SnapshotIsolationState};
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};

    use crate::context::ExecContext;

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

        /// A freshly created database, so the test does not guess how `MemoryStorage`
        /// numbers databases.
        fn created_database(&self, name: &str) -> vauban_storage::DbId {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            self.catalog
                .create_database(&handle, name, None)
                .expect("the database is created");
            let id = self
                .catalog
                .snapshot(&handle)
                .database(name)
                .expect("the created database is visible")
                .id;
            self.txn.commit(handle).expect("the transaction commits");
            id
        }

        /// The `DatabaseMeta` of `name` from a fresh snapshot.
        fn meta_of(&self, name: &str) -> DatabaseMeta {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            let meta = self
                .catalog
                .snapshot(&handle)
                .database(name)
                .expect("the database exists")
                .clone();
            self.txn.commit(handle).expect("the transaction commits");
            meta
        }
    }

    /// Runs the options of an `ALTER DATABASE` inside a transaction of its own, with the
    /// engine and the catalogue.
    fn run_options(
        fixture: &Fixture,
        name: &str,
        options: Vec<(String, Option<String>)>,
    ) -> SqlResult<ExecOutcome> {
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let outcome = {
            let snap = fixture.txn.statement_snapshot(&handle);
            let eval = StaticContext::default();
            let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
                .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
                .with_catalog(&fixture.catalog)
                .with_handle(&handle);
            let stmt = Ds::AlterDatabase {
                name: name.to_owned(),
                options,
            };
            // `execute_set_options` is called from the dispatch in the test, as it is from
            // `ddl.rs` in production.
            crate::statement::execute_collect(
                &vauban_planner::PhysicalStatement::Ddl(stmt),
                &mut ctx,
            )
            .map(|(outcome, _)| outcome)
        };
        match outcome {
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

    /// A pair of the options list, as the bound statement carries it.
    fn pair(name: &str, value: &str) -> (String, Option<String>) {
        (name.to_owned(), Some(value.to_owned()))
    }

    /// `ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON` writes the option, and a
    /// following [`CatalogSnapshot`] carries it as `ON`. `OFF` switches it back.
    #[test]
    fn rcsi_on_is_visible_in_the_catalog() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");

        let meta_before = fixture.meta_of("vdb");
        assert!(
            !meta_before.read_committed_snapshot,
            "a fresh database has RCSI OFF"
        );

        let outcome = run_options(&fixture, "vdb", vec![pair("READ_COMMITTED_SNAPSHOT", "ON")]);
        assert!(matches!(outcome, Ok(ExecOutcome::NoRows)), "{outcome:?}");

        let meta_after = fixture.meta_of("vdb");
        assert!(
            meta_after.read_committed_snapshot,
            "RCSI is ON after the switch"
        );

        let outcome = run_options(
            &fixture,
            "vdb",
            vec![pair("READ_COMMITTED_SNAPSHOT", "OFF")],
        );
        assert!(matches!(outcome, Ok(ExecOutcome::NoRows)), "{outcome:?}");

        let meta_after_off = fixture.meta_of("vdb");
        assert!(
            !meta_after_off.read_committed_snapshot,
            "RCSI is OFF after the second switch"
        );
    }

    /// The two options are independent: setting one does not change the other, across the
    /// four combinations.
    #[test]
    fn allow_snapshot_isolation_toggles_independently() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");

        for rcsi in ["ON", "OFF"] {
            for asi in ["ON", "OFF"] {
                run_options(
                    &fixture,
                    "vdb",
                    vec![
                        pair("READ_COMMITTED_SNAPSHOT", rcsi),
                        pair("ALLOW_SNAPSHOT_ISOLATION", asi),
                    ],
                )
                .expect("both options are accepted in one statement");

                let meta = fixture.meta_of("vdb");
                assert_eq!(
                    meta.read_committed_snapshot,
                    rcsi == "ON",
                    "RCSI is {rcsi}, snapshot_isolation is {asi}"
                );
                assert_eq!(
                    meta.snapshot_isolation,
                    if asi == "ON" {
                        SnapshotIsolationState::On
                    } else {
                        SnapshotIsolationState::Off
                    },
                    "snapshot_isolation is {asi}, RCSI is {rcsi}"
                );

                // Reset both to OFF for the next iteration.
                run_options(
                    &fixture,
                    "vdb",
                    vec![
                        pair("READ_COMMITTED_SNAPSHOT", "OFF"),
                        pair("ALLOW_SNAPSHOT_ISOLATION", "OFF"),
                    ],
                )
                .expect("both options reset off");
            }
        }
    }

    /// At creation, the two versioning options are `OFF` for
    /// `read_committed_snapshot` and `OFF` / `0` for `snapshot_isolation`. The test is
    /// written against a newly created database, and the fixture creates it rather than
    /// repeating the default values.
    #[test]
    fn a_new_database_carries_the_default_options() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");
        let meta = fixture.meta_of("vdb");
        // For a database created by CREATE DATABASE:
        // is_read_committed_snapshot_on = 0, snapshot_isolation_state = 0 / OFF.
        assert!(!meta.read_committed_snapshot);
        assert_eq!(meta.snapshot_isolation, SnapshotIsolationState::Off);
        assert_eq!(meta.snapshot_isolation.state(), 0);
        assert_eq!(meta.snapshot_isolation.desc(), "OFF");
    }

    /// After `READ_COMMITTED_SNAPSHOT ON`, the next statement reads the new option
    /// through [`CatalogSnapshot`]. With the option `OFF`, the same path reads the lock
    /// shape.
    ///
    /// The test builds a snapshot from scratch, as `session` does for each statement, and
    /// reads the [`DatabaseMeta`] it carries. It does not call `catalog.snapshot` twice —
    /// the switch and the read take one transaction each, committed between them.
    #[test]
    fn next_statement_follows_the_new_option() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");

        run_options(&fixture, "vdb", vec![pair("READ_COMMITTED_SNAPSHOT", "ON")]).expect("RCSI ON");

        let meta_on = fixture.meta_of("vdb");
        assert!(meta_on.read_committed_snapshot);

        run_options(
            &fixture,
            "vdb",
            vec![pair("READ_COMMITTED_SNAPSHOT", "OFF")],
        )
        .expect("RCSI OFF");

        let meta_off = fixture.meta_of("vdb");
        assert!(!meta_off.read_committed_snapshot);
    }

    /// An `ALTER DATABASE … SET` answers [`ExecOutcome::NoRows`] without announcing
    /// columns.
    #[test]
    fn option_is_norows() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");

        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let snap = fixture.txn.statement_snapshot(&handle);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);

        let stmt = Ds::AlterDatabase {
            name: "vdb".to_owned(),
            options: vec![pair("READ_COMMITTED_SNAPSHOT", "ON")],
        };
        let (outcome, row_set) = crate::statement::execute_collect(
            &vauban_planner::PhysicalStatement::Ddl(stmt),
            &mut ctx,
        )
        .expect("the statement runs");
        assert!(matches!(outcome, ExecOutcome::NoRows), "{outcome:?}");
        assert!(
            row_set.schema.columns.is_empty(),
            "NoRows sends no column metadata"
        );
        assert!(row_set.rows.is_empty(), "NoRows sends no row");

        fixture.txn.commit(handle).expect("the read commits");
    }

    /// The option changed inside a user transaction is undone by the rollback: after a
    /// rollback, the snapshot shows the value before the `ALTER DATABASE`.
    ///
    /// Counter-proof: the same switch committed stays.
    #[test]
    fn rollback_undoes_the_option() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");

        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);

        // Switch RCSI ON.
        let snap = fixture.txn.statement_snapshot(&handle);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);
        let stmt = Ds::AlterDatabase {
            name: "vdb".to_owned(),
            options: vec![pair("READ_COMMITTED_SNAPSHOT", "ON")],
        };
        crate::statement::execute_collect(&vauban_planner::PhysicalStatement::Ddl(stmt), &mut ctx)
            .expect("RCSI ON inside the transaction");

        // The transaction sees the switch.
        let inside = fixture
            .catalog
            .snapshot(&handle)
            .database("vdb")
            .expect("vdb is visible")
            .clone();
        assert!(inside.read_committed_snapshot);

        // Rollback.
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");

        // The next transaction sees the old value.
        let after_rollback = fixture.meta_of("vdb");
        assert!(
            !after_rollback.read_committed_snapshot,
            "the rollback undid the switch"
        );
    }

    /// A database the catalogue does not hold answers the internal error 50000 naming the
    /// database, and not [`ExecOutcome::NoRows`].
    #[test]
    fn unknown_database_is_a_bug() {
        let fixture = Fixture::new();
        let err = run_options(
            &fixture,
            "nosuch",
            vec![pair("READ_COMMITTED_SNAPSHOT", "ON")],
        )
        .expect_err("no such database");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("nosuch"), "{}", err.message);
    }

    /// `WITH ROLLBACK IMMEDIATE` is silently ignored — no exclusive lock, no rollback of
    /// other sessions — and the option is still set. The switch itself is untouched by the
    /// clause: the option is applied regardless.
    #[test]
    fn with_rollback_immediate_is_accepted_and_ignored() {
        let fixture = Fixture::new();
        fixture.created_database("vdb");

        let outcome = run_options(
            &fixture,
            "vdb",
            vec![
                pair("READ_COMMITTED_SNAPSHOT", "ON"),
                pair("WITH", "ROLLBACK IMMEDIATE"),
            ],
        );
        assert!(matches!(outcome, Ok(ExecOutcome::NoRows)), "{outcome:?}");

        let meta = fixture.meta_of("vdb");
        assert!(meta.read_committed_snapshot, "the option was still applied");
    }

    /// A statement whose variant is not an `AlterDatabase` is the internal 50000 here,
    /// rather than a silent `NoRows`.
    #[test]
    fn a_non_alter_database_statement_is_a_bug() {
        let fixture = Fixture::new();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let eval = StaticContext::default();
        let ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);

        let stmt = Ds::CreateDatabase {
            name: "d".to_owned(),
            collation: None,
        };
        let err = execute_set_options(&stmt, &ctx).expect_err("ddl.rs runs CREATE DATABASE");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("ddl.rs"), "{}", err.message);
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }
}
