//! Running `CREATE INDEX` and `DROP INDEX`.
//!
//! The two variants of [`DdlStatement`] this file serves call one method of
//! [`Catalog`] inside the transaction the caller opened
//! ([`ExecContext::handle`](crate::ExecContext)) and answer
//! [`ExecOutcome::NoRows`], like the database and table DDL of `ddl.rs`: no COLMETADATA
//! and no row (`create_index_is_visible_on_snapshot`,
//! `drop_index_unknown_is_3701_in_two_states`). A standalone index only; the index of a
//! `PRIMARY KEY` or of a `UNIQUE` written inside a `CREATE TABLE` is created by
//! `catalog.create_table`, which `ddl.rs` already calls.
//!
//! # `CREATE INDEX`: where each answer comes from
//!
//! `CREATE INDEX` re-checks nothing here: the duplicate key column (1909) is refused at
//! binding time, and the unknown column (1911), the taken name (1913), the second
//! clustered index (1902) and the unknown table come from `catalog.create_index`. The
//! catalogue raises the internal 50000 naming 1911 and 1913 rather than the two SQL Server
//! errors, which is what a client sees
//! (`create_index_twice_names_1913`, `create_index_on_an_unknown_column_names_1911`); the
//! divergence is the catalogue's, not this file's. A second divergence is on `drop_index`:
//! SQL Server frees the name at once, the deferred `DROP` of the catalogue keeps it taken
//! until the `COMMIT`.
//!
//! This file adds one thing the catalogue cannot do: turning the name and table a
//! `DROP INDEX` was written with into the [`IndexId`](vauban_storage::IndexId) that
//! [`Catalog::drop_index`] takes.
//!
//! # `DROP INDEX`: two states of 3701, and `IF EXISTS`
//!
//! The two states are what this file exists for: the state follows whether the table was
//! found, **7** when it was and the index was not, **6** when it was not. The number of
//! parts written is **not** what the state follows: a three-part name whose table is there
//! answers 7 next to a two-part 7, and a three-part name whose database is not there
//! answers 6 next to a two-part 6, which is the pair that separates "the state follows the
//! table" from "the state follows the shape of the name"
//! (`a_three_part_name_answers_the_state_of_its_table`,
//! `drop_index_unknown_is_3701_in_two_states`). The two `IF EXISTS` forms answer
//! [`ExecOutcome::NoRows`] where the two bare ones answer an error
//! (`locate_reports_the_table_before_the_index`).
//!
//! [`SqlError::cannot_drop`] keys its state on the kind and gives 7 for `index`, so state 6
//! is written here, as `catalog/index.rs` writes it for the neighbouring case of a table
//! carrying a deferred `DROP`. The comparison of the index name written in [`locate`]
//! ignores ASCII case, as `catalog/index.rs` does when it refuses a duplicate index name:
//! `drop_index_of_a_created_index_reaches_the_catalogue` drops `ix_a` written `IX_A`,
//! which a case-sensitive comparison would miss.
//!
//! # Deliberate difference: the name printed in the 3701
//!
//! The `%.*ls` of 3701 carries the table and the index joined by a dot:
//! `'dbo.t.ix_nope'` and `'dbo.nosuch.ix_nope'`. SQL Server prints the table **as
//! written** (`'master.dbo.t.ix_nope'` for a three-part name); the binder hands over a
//! resolved three-part [`QualifiedName`] and drops what the client wrote, so this file
//! prints `<schema>.<table>.<index>` for the two-part and the three-part forms alike. That
//! is the difference `ddl.rs` already carries for the 3701 of a `DROP TABLE`, same cause.
//!
//! # How a `DROP INDEX` reaches an index by its name
//!
//! [`CatalogSnapshot::indexes_of`](vauban_catalog::CatalogSnapshot::indexes_of) is the way
//! from a name to an [`IndexId`](vauban_storage::IndexId): the `pub` items of
//! `vauban-catalog` offer no second one —
//! [`TableMeta`](vauban_catalog::TableMeta) lists the clustered index and the constraints
//! but not the indexes, and the `IndexShape` of `storage` carries no name.
//! `create_index_is_visible_on_snapshot` reads the `IndexMeta` of the index it created out
//! of the snapshot, and `drop_index_of_a_created_index_reaches_the_catalogue` drops that
//! index by its name. What a name that reaches no index answers is the 3701 of the section
//! above.

use vauban_binder::DdlStatement;
use vauban_catalog::{Catalog, IndexDef, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::IndexId;
use vauban_txn::TxnHandle;

use crate::row::ExecOutcome;

/// Error 3701, the number a `DROP` of something that is not there answers.
///
/// The `IF EXISTS` of a `DROP INDEX` swallows this one, in both of its states (module
/// documentation).
const CANNOT_DROP: u32 = 3701;

/// The state of the 3701 a `DROP INDEX` answers when its **table** is not there.
///
/// [`SqlError::cannot_drop`] keys the state on the kind and gives 7 for `index`, the state
/// of a table that is there without the index; 6 is the state of the missing table
/// (module documentation).
const INDEX_OF_A_MISSING_TABLE_3701_STATE: u8 = 6;

/// Runs the `CREATE INDEX` or the `DROP INDEX` of `stmt` and answers
/// [`ExecOutcome::NoRows`].
///
/// The single entry point `ddl.rs` calls from the two index arms of its `Ddl` match; the
/// other four variants of [`DdlStatement`] belong to `ddl.rs` and are the internal 50000
/// here (`a_statement_that_is_not_an_index_one_is_a_bug`), so a variant routed to the wrong
/// file is reported rather than run as something else.
///
/// # Errors
///
/// What `catalog.create_index` and `catalog.drop_index` raise, unchanged — including the
/// internal 50000 the catalogue raises in place of the SQL Server numbers it does not
/// build (1909, 1911, 1913, 3723) — plus the 3701 this file raises for a `DROP INDEX` whose
/// table or index resolves to nothing, and the internal 50000 for a statement that is not
/// one of the two index ones.
pub(crate) fn execute_index_ddl(
    stmt: &DdlStatement,
    catalog: &Catalog,
    handle: &TxnHandle,
) -> SqlResult<ExecOutcome> {
    match stmt {
        DdlStatement::CreateIndex { def } => create_index(catalog, handle, def)?,
        DdlStatement::DropIndex {
            name,
            table,
            if_exists,
        } => drop_index(catalog, handle, name, table, *if_exists)?,
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        // The two `ALTER` statements reach `ddl.rs`, which answers for them; they are
        // listed here for the same reason as the four above.
        | DdlStatement::AlterDatabase { .. }
        | DdlStatement::TruncateTable { .. }
        | DdlStatement::AlterTable { .. } => {
            return Err(bug(
                "execute_index_ddl: only CREATE INDEX and DROP INDEX are run here; the \
                 database and table DDL is run by ddl.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// `CREATE [UNIQUE] [CLUSTERED] INDEX ix ON t (c1, c2 DESC)`: one call, nothing re-checked.
///
/// [`IndexDef::table`] is the [`ObjectId`](vauban_catalog::ObjectId) the binder resolved,
/// so no name is turned into an identifier here.
fn create_index(catalog: &Catalog, handle: &TxnHandle, def: &IndexDef) -> SqlResult<()> {
    catalog.create_index(handle, def)?;
    Ok(())
}

/// `DROP INDEX [IF EXISTS] ix ON t`: the name resolved into an identifier, then one call.
fn drop_index(
    catalog: &Catalog,
    handle: &TxnHandle,
    name: &str,
    table: &QualifiedName,
    if_exists: bool,
) -> SqlResult<()> {
    let outcome =
        locate(catalog, handle, name, table).and_then(|index| catalog.drop_index(handle, index));
    match outcome {
        Ok(()) => Ok(()),
        Err(err) if if_exists && err.number == CANNOT_DROP => Ok(()),
        Err(err) => Err(err),
    }
}

/// The identifier of the index called `name` on `table`.
///
/// # Errors
///
/// 3701 state 6 when `table` resolves to nothing, 3701 state 7 when it resolves and carries
/// no index of that name (module documentation). The comparison of the index name ignores
/// ASCII case, as `catalog/index.rs` does when it refuses a duplicate name.
fn locate(
    catalog: &Catalog,
    handle: &TxnHandle,
    name: &str,
    table: &QualifiedName,
) -> SqlResult<IndexId> {
    let printed = printed(table, name);
    let snapshot = catalog.snapshot(handle);
    let Some(object) = snapshot.resolve_object(
        &table.database,
        Some(&table.schema),
        &table.name,
        &table.schema,
    ) else {
        let mut err = SqlError::cannot_drop("drop", "index", &printed);
        err.state = INDEX_OF_A_MISSING_TABLE_3701_STATE;
        return Err(err);
    };
    snapshot
        .indexes_of(object.id)
        .iter()
        .find(|index| index.name.eq_ignore_ascii_case(name))
        .map(|index| index.id)
        .ok_or_else(|| SqlError::cannot_drop("drop", "index", &printed))
}

/// The name a 3701 of `DROP INDEX` prints: `<schema>.<table>.<index>`, the deliberate
/// difference of the module documentation.
fn printed(table: &QualifiedName, name: &str) -> String {
    format!("{}.{}.{}", table.schema, table.name, name)
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_catalog::{CatalogSnapshot, IndexMeta, ObjectId, SortedColumn, TableMeta};
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_planner::{NoIndexes, PhysicalStatement, PlanContext, plan};
    use vauban_storage::{IndexShape, MemoryStorage, Storage, TableId};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};

    use crate::context::ExecContext;

    /// A bootstrapped catalogue over an in-memory storage, and the manager its transactions
    /// come from. The fixture of `ddl.rs`, which is `#[cfg(test)]` in its own module.
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

        /// The table `master.dbo.<name>` as the catalogue holds it.
        fn table(&self, name: &str) -> TableMeta {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            let snapshot = self.catalog.snapshot(&handle);
            let object = snapshot
                .resolve_object("master", Some("dbo"), name, "dbo")
                .unwrap_or_else(|| unreachable!("the test created master.dbo.{name}"));
            let meta = snapshot
                .table(object.id)
                .unwrap_or_else(|| unreachable!("master.dbo.{name} is a table"))
                .clone();
            self.txn.commit(handle).expect("the read commits");
            meta
        }

        /// The indexes `storage` holds on `table`, which is where the catalogue created them.
        fn indexes(&self, table: TableId) -> Vec<(vauban_storage::IndexId, IndexShape)> {
            self.storage.indexes(table).expect("the table is known")
        }
    }

    /// Parses, binds and runs `text` as one statement, in a transaction of its own, with
    /// the catalogue handed to the binder as well: `CREATE INDEX` cannot bind without it,
    /// [`IndexDef::table`] being an identifier the catalogue hands out.
    fn run(fixture: &Fixture, text: &str) -> SqlResult<ExecOutcome> {
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let outcome = {
            let snapshot = fixture.catalog.snapshot(&handle);
            let bound = bound(text, &snapshot);
            let snap = fixture.txn.statement_snapshot(&handle);
            let eval = StaticContext::default();
            let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
                .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
                .with_catalog(&fixture.catalog)
                .with_handle(&handle);
            crate::statement::execute_collect(&bound, &mut ctx).map(|(outcome, _)| outcome)
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

    /// The single statement of `text`, bound against `master`, `dbo` and `snapshot`, and
    /// planned.
    fn bound(text: &str, snapshot: &CatalogSnapshot) -> PhysicalStatement {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        assert_eq!(batch.statements.len(), 1, "one statement per call");
        let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
        bind_ctx.catalog = Some(snapshot);
        let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
        plan(
            bound,
            &PlanContext {
                catalog: &NoIndexes,
            },
        )
        .expect("the statement plans")
    }

    /// `CREATE INDEX`, checked on both sides of the engine.
    ///
    /// `CREATE INDEX ix_a ON dbo.t (a);` answers [`ExecOutcome::NoRows`] and leaves one more
    /// index on the table than the statement found, keyed on the column written and
    /// non-unique — read in `storage`, which is where `catalog.create_index` puts it and what
    /// `catalog/index.rs` calls the reference on existence — **and** the snapshot taken after
    /// it carries the [`IndexMeta`](vauban_catalog::IndexMeta) of `ix_a`, same name, same
    /// identifier, same flags. That last part is what makes the name of this test true of
    /// the snapshot and not of `storage` alone.
    #[test]
    fn create_index_is_visible_on_snapshot() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");
        let table = fixture.table("t");
        let before = fixture.indexes(table.storage_id);

        let outcome = run(&fixture, "CREATE INDEX ix_a ON dbo.t (a);").expect("the index");
        assert!(
            matches!(outcome, ExecOutcome::NoRows),
            "a DDL statement answers NoRows"
        );
        let after = fixture.indexes(table.storage_id);
        assert_eq!(after.len(), before.len() + 1, "one index was created");
        let created = after
            .iter()
            .find(|(id, _)| !before.iter().any(|(before, _)| before == id))
            .map(|(_, shape)| shape)
            .expect("the new index");
        assert_eq!(created.columns.len(), 1, "one key column was written");
        assert_eq!(
            created.columns[0].column, 0,
            "the key is `a`, the first column of the row"
        );
        assert!(!created.unique, "`CREATE INDEX` without UNIQUE");

        let created_id = after
            .iter()
            .find(|(id, _)| !before.iter().any(|(before, _)| before == id))
            .map(|(id, _)| *id)
            .expect("the new index has an identifier");
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = fixture.catalog.snapshot(&handle);
        let on_the_snapshot: Vec<&IndexMeta> = snapshot
            .indexes_of(table.id)
            .iter()
            .filter(|index| index.id == created_id)
            .collect();
        assert_eq!(
            on_the_snapshot.len(),
            1,
            "the snapshot carries the index that was just created"
        );
        assert_eq!(on_the_snapshot[0].name, "ix_a");
        assert!(!on_the_snapshot[0].unique, "`CREATE INDEX` without UNIQUE");
        assert!(!on_the_snapshot[0].clustered);
        assert!(!on_the_snapshot[0].primary_key);
        drop(snapshot);
        fixture.txn.commit(handle).expect("the read commits");
    }

    /// The counter-proof of the test above: the same statement twice answers the internal
    /// 50000 naming 1913, which `catalog.create_index` builds, so the first call reached the
    /// catalogue and not `storage` alone.
    #[test]
    fn create_index_twice_names_1913() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");
        run(&fixture, "CREATE INDEX ix_a ON dbo.t (a);").expect("the index is created");
        let table = fixture.table("t");
        let after_first = fixture.indexes(table.storage_id).len();

        let err = run(&fixture, "CREATE INDEX ix_a ON dbo.t (b);").expect_err("the name is taken");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("1913"), "{}", err.message);
        assert_eq!(
            fixture.indexes(table.storage_id).len(),
            after_first,
            "the refused statement created nothing in storage"
        );
    }

    /// An unknown column is the number `catalog.create_index` names, not a silent success:
    /// the second half of the pair that shows this file re-checks nothing of its own.
    #[test]
    fn create_index_on_an_unknown_column_names_1911() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");
        let err = run(&fixture, "CREATE INDEX ix_c ON dbo.t (nosuch);").expect_err("no column");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("1911"), "{}", err.message);
    }

    /// The two states of 3701, and the `IF EXISTS` that swallows both.
    ///
    /// The state is the vector — 7 when the table was found and the index was not, 6 when it
    /// was not found — asserted here on the two-part shape the binder built, the two names
    /// below being the `dbo.t` and `dbo.nosuch` of a batch bound in `master`. The three-part
    /// shape is `a_three_part_name_answers_the_state_of_its_table`; nothing here says what a
    /// name of four parts would answer, the binder refusing it at binding time with 117.
    /// The two halves cannot be read as one rule about a missing index, and the two
    /// `IF EXISTS` forms answer [`ExecOutcome::NoRows`] where the two bare ones answer an
    /// error.
    #[test]
    fn drop_index_unknown_is_3701_in_two_states() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");

        let on_a_live_table = run(&fixture, "DROP INDEX ix_nope ON dbo.t;").expect_err("no index");
        assert_eq!(on_a_live_table.number, 3701);
        assert_eq!(on_a_live_table.severity, 11);
        assert_eq!(on_a_live_table.state, 7);
        assert!(
            on_a_live_table.message.contains("'dbo.t.ix_nope'"),
            "{}",
            on_a_live_table.message
        );

        let on_a_missing_table =
            run(&fixture, "DROP INDEX ix_nope ON dbo.nosuch;").expect_err("no table");
        assert_eq!(on_a_missing_table.number, 3701);
        assert_eq!(on_a_missing_table.severity, 11);
        assert_eq!(on_a_missing_table.state, 6);
        assert!(
            on_a_missing_table.message.contains("'dbo.nosuch.ix_nope'"),
            "{}",
            on_a_missing_table.message
        );

        for text in [
            "DROP INDEX IF EXISTS ix_nope ON dbo.t;",
            "DROP INDEX IF EXISTS ix_nope ON dbo.nosuch;",
        ] {
            let outcome = run(&fixture, text)
                .unwrap_or_else(|err| unreachable!("{text} answers no error, got {}", err.message));
            assert!(matches!(outcome, ExecOutcome::NoRows), "{text}");
        }
    }

    /// What this crate answers on a **three-part** name, next to the two-part one: the state
    /// follows the table, and the printed name does not follow what was written.
    ///
    /// Both shapes are covered — `master.dbo.t`, whose table is there, and `nosuchdb.dbo.t`,
    /// whose database is not — and both answer the state of their table (7 and 6).
    ///
    /// What differs from SQL Server is the printed name, which this test freezes rather than
    /// closes: SQL Server prints the name **as written** (`'master.dbo.t.ix_nope'`), and
    /// this file prints `<schema>.<table>.<index>`, the same text as for the two-part form. A
    /// [`QualifiedName`] does not say whether a database part was written, so the two shapes
    /// are one value by the time the executor sees them — the first two assertions below
    /// state that identity, which is what makes the divergence a binder-shaped one and not a
    /// choice made here.
    #[test]
    fn a_three_part_name_answers_the_state_of_its_table() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");

        let three = run(&fixture, "DROP INDEX ix_nope ON master.dbo.t;").expect_err("no index");
        let two = run(&fixture, "DROP INDEX ix_nope ON dbo.t;").expect_err("no index");
        assert_eq!(three.state, two.state, "one value reaches the executor");
        assert_eq!(three.message, two.message, "and one message comes back");
        assert_eq!(three.number, 3701);
        assert_eq!(three.state, 7, "the table of the three-part name is there");
        assert!(
            three.message.contains("'dbo.t.ix_nope'"),
            "SQL Server prints 'master.dbo.t.ix_nope' here: {}",
            three.message
        );

        let unknown_database =
            run(&fixture, "DROP INDEX ix_nope ON nosuchdb.dbo.t;").expect_err("no database");
        assert_eq!(unknown_database.number, 3701);
        assert_eq!(
            unknown_database.state, 6,
            "the table of that three-part name is not there"
        );
    }

    /// The other half of [`locate`]: an index this session created is reached by its name and
    /// dropped. The `DROP INDEX` answers [`ExecOutcome::NoRows`], `storage` holds one index
    /// fewer and the next snapshot does not carry it.
    ///
    /// The name is written `IX_A` where the index is `ix_a`, which is the vector for the
    /// ASCII-case-insensitive comparison of [`locate`]: a case-sensitive comparison answers
    /// 3701 here. The counter-proof that the lookup answers on the name and not on the table
    /// is `drop_index_unknown_is_3701_in_two_states`, whose `ix_nope` on the same shape of
    /// table is still 3701 state 7.
    #[test]
    fn drop_index_of_a_created_index_reaches_the_catalogue() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");
        run(&fixture, "CREATE INDEX ix_a ON dbo.t (a);").expect("the index is created");
        let table = fixture.table("t");
        let created = fixture.indexes(table.storage_id).len();

        let outcome = run(&fixture, "DROP INDEX IX_A ON dbo.t;").expect("the index resolves");
        assert!(matches!(outcome, ExecOutcome::NoRows));
        assert_eq!(
            fixture.indexes(table.storage_id).len(),
            created - 1,
            "the committed DROP took the index out of storage"
        );
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        assert!(
            fixture
                .catalog
                .snapshot(&handle)
                .indexes_of(table.id)
                .iter()
                .all(|index| index.name != "ix_a"),
            "the dropped index is out of the next snapshot"
        );
        fixture.txn.commit(handle).expect("the read commits");
    }

    /// A variant `ddl.rs` owns is the internal 50000 here, rather than a silent `NoRows`:
    /// the routing of the two files is stated on a value and not only on the shape of the
    /// match.
    #[test]
    fn a_statement_that_is_not_an_index_one_is_a_bug() {
        let fixture = Fixture::new();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let stmt = DdlStatement::CreateDatabase {
            name: "d".to_owned(),
            collation: None,
        };
        let err = execute_index_ddl(&stmt, &fixture.catalog, &handle)
            .expect_err("ddl.rs runs CREATE DATABASE");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("ddl.rs"), "{}", err.message);
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }

    /// `locate` reports the table before the index: a `DROP INDEX` whose table resolves to
    /// nothing never reaches
    /// [`CatalogSnapshot::indexes_of`](vauban_catalog::CatalogSnapshot::indexes_of).
    ///
    /// Stated on the identifier a bare `IndexDef` would carry: the two calls below differ
    /// only by the table, and answer the two states of the module documentation.
    #[test]
    fn locate_reports_the_table_before_the_index() {
        let fixture = Fixture::new();
        run(&fixture, "CREATE TABLE dbo.t (a int, b int);").expect("the table is created");
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let live = QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "t".to_owned(),
        };
        let missing = QualifiedName {
            name: "nosuch".to_owned(),
            ..live.clone()
        };
        assert_eq!(
            locate(&fixture.catalog, &handle, "ix", &live)
                .expect_err("no such index")
                .state,
            7
        );
        assert_eq!(
            locate(&fixture.catalog, &handle, "ix", &missing)
                .expect_err("no such table")
                .state,
            6
        );
        fixture.txn.commit(handle).expect("the read commits");
    }

    /// A `CREATE INDEX` built by hand, without the binder: the identifier of the table is
    /// what `catalog.create_index` reads, so a table that is not there is the internal 50000
    /// the catalogue names, and not a 3701 invented here.
    #[test]
    fn create_index_on_an_unknown_object_is_the_bug_of_the_catalogue() {
        let fixture = Fixture::new();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let stmt = DdlStatement::CreateIndex {
            def: IndexDef {
                table: ObjectId(9_999),
                name: "ix".to_owned(),
                columns: vec![SortedColumn {
                    column: "a".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        };
        let err = execute_index_ddl(&stmt, &fixture.catalog, &handle).expect_err("no such table");
        assert_eq!(err.number, 50000);
        assert!(err.message.contains("create_index"), "{}", err.message);
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }
}
