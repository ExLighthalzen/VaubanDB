//! `CREATE DATABASE` / `DROP DATABASE` in the catalogue.
//!
//! # What a database is made of
//!
//! A user database is a database in `storage` plus the rows the catalogue writes about it:
//! one in [`DATABASES_TABLE`] and one per schema of [`SYSTEM_SCHEMAS`] in [`SCHEMAS_TABLE`],
//! both tables reached through [`internal_table_id`] and written through the column
//! positions of [`databases_columns`] and [`schemas_columns`], so the column order declared
//! in `bootstrap.rs` is stated in one place. A new database gets the collation
//! `SQL_Latin1_General_CP1_CI_AS` and the schemas `dbo` 1/1, `INFORMATION_SCHEMA` 3/3 and
//! `sys` 4/4: the three schemas the bootstrap writes for a system database are the three
//! written here; `guest` and the `db_*` roles are not written (`bootstrap.rs`,
//! [`SYSTEM_SCHEMAS`]).
//!
//! # Comparison of names
//!
//! Two database names are the same name when [`Collation::DEFAULT`] — the
//! `SQL_Latin1_General_CP1_CI_AS` of `master` — compares them equal, which is what decides
//! 1801 and what `drop_database` looks a name up with. That rule is not an ASCII case fold:
//! after `CREATE DATABASE d_é`, `CREATE DATABASE [D_É]` answers 1801 (case folded
//! beyond ASCII) while `CREATE DATABASE d_e` succeeds (accents kept apart). Both halves
//! are held by the unit test `the_name_is_compared_case_insensitively_and_accent_sensitively`.
//!
//! # Transaction
//!
//! The rows are written and deleted inside the caller's transaction, so they appear or
//! disappear with it; the database of `storage` follows through the two deferred actions of
//! the transaction manager, which is the protocol the `Storage` trait asks its caller for:
//!
//! - `create_database` calls `Storage::create_database` at once — it needs the [`DbId`] to
//!   write its row — and registers [`RollbackAction::DropDatabase`], so a rolled-back create
//!   leaves neither a row nor a database behind (integration test
//!   `create_compensated_on_rollback`);
//! - `drop_database` deletes the rows and registers [`CommitAction::DropDatabase`] instead of
//!   calling `Storage::drop_database`, so the database is readable by the other transactions
//!   until the commit and gone after it (integration test `drop_is_deferred_until_commit`).
//!
//! A name `storage` already carries without a row the caller's transaction can see is error
//! 1801, not a database taken over: that name belongs either to a database bootstrapped
//! outside the catalogue or to a create another transaction has not committed yet, whose row
//! is invisible under MVCC. Refusing it is what keeps the precondition of
//! `Storage::create_database` — no database of that name in `storage` — and what stops a
//! rollback from destroying a database another transaction committed (unit tests
//! `a_name_a_concurrent_transaction_holds_in_storage_is_1801` and
//! `a_database_storage_holds_with_a_table_is_1801_and_kept_as_it_is`).

use std::cmp::Ordering;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, Row, RowId, TableId};
use vauban_txn::{CommitAction, RollbackAction, TxnHandle};
use vauban_types::{Collation, SqlString, Value};

use crate::bootstrap::{
    DATABASES_TABLE, DEFAULT_COLLATION_NAME, SCHEMAS_TABLE, SYSTEM_DATABASES, SYSTEM_SCHEMAS,
    databases_columns, internal_table_id, owner_sid_value, schemas_columns, transaction_datetime,
};
use crate::catalog::Catalog;
use crate::meta::{DatabaseOption, SnapshotIsolationState};

/// The two versioning options a database created by `CREATE DATABASE` starts with:
/// `(is_read_committed_snapshot_on, snapshot_isolation_state)`.
///
/// After `CREATE DATABASE d`, `sys.databases` shows `is_read_committed_snapshot_on` 0 and
/// `snapshot_isolation_state` 0 / `OFF` for `d` (unit test
/// `new_database_defaults_are_off`). The four system databases do not share that couple,
/// `master` and `msdb` carrying `snapshot_isolation_state` 1
/// ([`crate::bootstrap::SYSTEM_DATABASE_OPTIONS`]).
pub(crate) const NEW_DATABASE_OPTIONS: (bool, SnapshotIsolationState) =
    (false, SnapshotIsolationState::Off);

// The other dispatch methods of the catalogue sit in `catalog.rs`; this one is written here,
// in the file that owns the operation.
impl Catalog {
    /// Switches `option` on or off for the database `db`, inside `txn`.
    ///
    /// The row of the catalogue is rewritten as an ordinary versioned row, so the switch is
    /// visible to `txn` at once, to the others at the commit, and undone by a rollback
    /// without a compensating action (unit tests `set_database_option_changes_the_snapshot_view`
    /// and `set_database_option_rolls_back`). A [`CatalogSnapshot`](crate::CatalogSnapshot)
    /// is rebuilt from the rows at each [`Catalog::snapshot`] call (`snapshot.rs`), so the
    /// write is what a later reader sees; the catalogue holds no cached copy of a
    /// [`DatabaseMeta`](crate::DatabaseMeta) to invalidate.
    ///
    /// Binding `ALTER DATABASE … SET`, executing it and making the options effective belong
    /// to other crates: this method writes the flag and nothing else.
    ///
    /// # Errors
    ///
    /// [`InternalError::Bug`] when no row of the catalogue visible to `txn` carries `db` —
    /// the caller resolved a database the catalogue does not hold — and the error of a
    /// `storage` call otherwise.
    pub fn set_database_option(
        &self,
        txn: &TxnHandle,
        db: DbId,
        option: DatabaseOption,
        on: bool,
    ) -> SqlResult<()> {
        set_database_option(self, txn, db, option, on)
    }
}

/// Creates a database. See [`Catalog::create_database`].
///
/// In order: the name is looked up in the rows of [`DATABASES_TABLE`] that `txn` sees and in
/// the databases of `storage` ([`stored_database`]), either answer being error 1801; then
/// `Storage::create_database` hands out the [`DbId`] and [`RollbackAction::DropDatabase`] is
/// registered on `txn`; then the row of the database and the three rows of its schemas are
/// inserted inside `txn`.
///
/// The second lookup makes `DROP DATABASE d; CREATE DATABASE d;` inside one transaction
/// answer 1801, the drop leaving `d` in `storage` until the commit; the commit runs that
/// deferred drop and the name is free for the next transaction (unit test
/// `a_create_after_a_drop_of_the_same_name_in_one_transaction_is_1801`).
///
/// `collation` is the collation of the database, [`Collation::DEFAULT`] when the caller
/// passes `None`. The `collation_name` column is filled with [`DEFAULT_COLLATION_NAME`] for
/// that collation and left `NULL` for another one: a [`Collation`] is the five wire bytes of
/// `[MS-TDS]` 2.2.5.1.2, and `vauban_types` parses a name into them without spelling one back
/// ([`Collation::parse`] has no inverse), so the text a `COLLATE` clause carried does not
/// reach this function (unit test `a_collation_other_than_the_default_leaves_the_name_null`).
///
/// The row carries the two versioning options of [`NEW_DATABASE_OPTIONS`];
/// `Catalog::set_database_option` is what changes them afterwards.
///
/// A collation *name* that means nothing is error 448, raised before this function by
/// [`Collation::parse`] in the caller that reads the `COLLATE` clause: what arrives here is a
/// parsed [`Collation`]. The other options of `CREATE DATABASE` — `ON (FILENAME = …)`,
/// `LOG ON`, `FOR ATTACH` — are parsed and ignored by the catalogue: no file is created and
/// nothing is written about them.
///
/// # Errors
///
/// 1801 when a database of that name is in the catalogue, and 1801 as well when `storage`
/// carries it without a row `txn` sees (module documentation). The error of a `storage` call
/// otherwise.
pub(crate) fn create_database(
    catalog: &Catalog,
    txn: &TxnHandle,
    name: &str,
    collation: Option<Collation>,
) -> SqlResult<DbId> {
    if database_row(catalog, txn, name)?.is_some() || stored_database(catalog, name)?.is_some() {
        return Err(SqlError::database_already_exists(name));
    }
    let id = catalog.storage.create_database(name)?;
    catalog
        .txn
        .register_on_rollback(txn, RollbackAction::DropDatabase(id))?;
    let published = database_id(id)?;
    let mut row = vec![Value::Null; databases_columns::WIDTH];
    row[databases_columns::DATABASE_ID] = Value::I32(published);
    row[databases_columns::NAME] = text(name);
    row[databases_columns::COLLATION_NAME] = collation_name(collation);
    row[databases_columns::READ_COMMITTED_SNAPSHOT] = Value::Bit(NEW_DATABASE_OPTIONS.0);
    row[databases_columns::SNAPSHOT_ISOLATION_STATE] = Value::I8(NEW_DATABASE_OPTIONS.1.state());
    row[databases_columns::OWNER_SID] = owner_sid_value();
    row[databases_columns::CREATE_DATE] = transaction_datetime(catalog, txn)?;
    catalog
        .storage
        .insert(txn.id, table_of(catalog, DATABASES_TABLE)?, &Row(row))?;
    let schemas = table_of(catalog, SCHEMAS_TABLE)?;
    for (schema, schema_id, principal_id) in SYSTEM_SCHEMAS {
        let mut row = vec![Value::Null; schemas_columns::WIDTH];
        row[schemas_columns::DATABASE_ID] = Value::I32(published);
        row[schemas_columns::SCHEMA_ID] = Value::I32(schema_id);
        row[schemas_columns::NAME] = text(schema);
        row[schemas_columns::PRINCIPAL_ID] = Value::I32(principal_id);
        catalog.storage.insert(txn.id, schemas, &Row(row))?;
    }
    Ok(id)
}

/// Drops a database. See [`Catalog::drop_database`].
///
/// The row of the database and the rows of its schemas are deleted inside `txn`, so the
/// database is gone for `txn` at once and for the other transactions at the commit. The
/// database itself is taken out of `storage` by [`CommitAction::DropDatabase`], registered
/// here instead of calling `Storage::drop_database`, which takes effect immediately: that is
/// what makes the statement transactional at the SQL level (module documentation, integration
/// test `drop_is_deferred_until_commit`).
///
/// # Errors
///
/// 3701 when the name is no database of the catalogue. For one of the four databases of
/// [`SYSTEM_DATABASES`], 3708 severity 16 state 4 ([`SqlError::cannot_drop_system_database`]),
/// the name printed as it was written (integration test `drop_master_is_refused`). The error
/// of a `storage` call otherwise.
pub(crate) fn drop_database(catalog: &Catalog, txn: &TxnHandle, name: &str) -> SqlResult<()> {
    if SYSTEM_DATABASES
        .iter()
        .any(|system| same_name(system, name))
    {
        return Err(SqlError::cannot_drop_system_database(name));
    }
    let Some((row, id)) = database_row(catalog, txn, name)? else {
        return Err(SqlError::cannot_drop("drop", "database", name));
    };
    catalog
        .storage
        .delete(txn.id, table_of(catalog, DATABASES_TABLE)?, row)?;
    let schemas = table_of(catalog, SCHEMAS_TABLE)?;
    for (row, values) in rows_of(catalog, txn, SCHEMAS_TABLE)? {
        if values.get(schemas_columns::DATABASE_ID) == Some(&Value::I32(id)) {
            catalog.storage.delete(txn.id, schemas, row)?;
        }
    }
    crate::sys_rows::remove_database(catalog, txn, db_id(id)?)?;
    catalog
        .txn
        .register_on_commit(txn, CommitAction::DropDatabase(db_id(id)?))?;
    Ok(())
}

/// Switches a versioning option of a database. See [`Catalog::set_database_option`].
///
/// The row of [`DATABASES_TABLE`] that carries `db` is read in the statement snapshot of
/// `txn` and written back by `Storage::update`, which makes a new version of it under `txn`:
/// the DDL of an option needs no deferred action and no compensation, unlike the
/// `CREATE`/`DROP DATABASE` above.
///
/// `on` is written as a `bit` for [`DatabaseOption::ReadCommittedSnapshot`] and as the number
/// of [`SnapshotIsolationState::On`] or [`SnapshotIsolationState::Off`] for
/// [`DatabaseOption::AllowSnapshotIsolation`]. The two transitional states are represented by
/// the enum and are not written here.
fn set_database_option(
    catalog: &Catalog,
    txn: &TxnHandle,
    db: DbId,
    option: DatabaseOption,
    on: bool,
) -> SqlResult<()> {
    let published = database_id(db)?;
    let Some((row, mut values)) =
        rows_of(catalog, txn, DATABASES_TABLE)?
            .into_iter()
            .find(|(_, values)| {
                values.get(databases_columns::DATABASE_ID) == Some(&Value::I32(published))
            })
    else {
        return Err(bug(&format!(
            "{DATABASES_TABLE} holds no row this transaction sees for database {published}"
        )));
    };
    if values.len() != databases_columns::WIDTH {
        return Err(bug(&format!(
            "{DATABASES_TABLE} holds a row of {} values instead of {}",
            values.len(),
            databases_columns::WIDTH
        )));
    }
    match option {
        DatabaseOption::ReadCommittedSnapshot => {
            values[databases_columns::READ_COMMITTED_SNAPSHOT] = Value::Bit(on);
        }
        DatabaseOption::AllowSnapshotIsolation => {
            let state = if on {
                SnapshotIsolationState::On
            } else {
                SnapshotIsolationState::Off
            };
            values[databases_columns::SNAPSHOT_ISOLATION_STATE] = Value::I8(state.state());
        }
    }
    catalog.storage.update(
        txn.id,
        table_of(catalog, DATABASES_TABLE)?,
        row,
        &Row(values),
    )?;
    Ok(())
}

/// The row of [`DATABASES_TABLE`] that names `name`, as `(row, database_id)`; `None` when no
/// row visible to `txn` carries that name.
fn database_row(catalog: &Catalog, txn: &TxnHandle, name: &str) -> SqlResult<Option<(RowId, i32)>> {
    for (row, values) in rows_of(catalog, txn, DATABASES_TABLE)? {
        let Some(Value::String(stored)) = values.get(databases_columns::NAME) else {
            return Err(bug(&format!(
                "{DATABASES_TABLE} holds a row whose name is not a string"
            )));
        };
        if !same_name(&stored.text, name) {
            continue;
        }
        let Some(&Value::I32(id)) = values.get(databases_columns::DATABASE_ID) else {
            return Err(bug(&format!(
                "{DATABASES_TABLE} holds a row whose database_id is not an int"
            )));
        };
        return Ok(Some((row, id)));
    }
    Ok(None)
}

/// The rows of the internal table called `name` that `txn` sees, with their [`RowId`].
///
/// The snapshot is taken at the call, as a statement of `txn` would take it, so the rows
/// `txn` wrote itself are there (`TransactionManager::statement_snapshot`).
fn rows_of(catalog: &Catalog, txn: &TxnHandle, name: &str) -> SqlResult<Vec<(RowId, Vec<Value>)>> {
    let table = table_of(catalog, name)?;
    let snapshot = catalog.txn.statement_snapshot(txn);
    let mut rows = Vec::new();
    for row in catalog.storage.scan(&snapshot, table)? {
        let (id, values) = row?;
        rows.push((id, values.0));
    }
    Ok(rows)
}

/// The [`TableId`] of the internal table called `name`.
///
/// # Errors
///
/// [`InternalError::Bug`] when the storage carries no such table, which means the catalogue
/// was not bootstrapped: [`Catalog::bootstrap`] creates the internal tables (`bootstrap.rs`)
/// and is the constructor of [`Catalog`] this crate exposes (unit test
/// `a_table_the_bootstrap_does_not_describe_is_an_internal_bug`).
fn table_of(catalog: &Catalog, name: &str) -> SqlResult<TableId> {
    internal_table_id(catalog, name)?
        .ok_or_else(|| bug(&format!("internal table {name} is not in this storage")))
}

/// The [`DbId`] `storage` holds for that name, `None` for a name `Storage::databases` does
/// not carry.
///
/// Read from `Storage::databases`, which lists what the DDL applied so far left in place,
/// with no regard for transactions: that is the list `Storage::create_database` asks the
/// caller to check against, its precondition being that no database of the same name is
/// there. [`create_database`] turns a name found here into 1801 instead of handing the
/// database over, a database of `storage` with no visible row being either older than the
/// catalogue or the uncommitted work of another transaction.
fn stored_database(catalog: &Catalog, name: &str) -> SqlResult<Option<DbId>> {
    Ok(catalog
        .storage
        .databases()?
        .into_iter()
        .find(|(_, stored)| same_name(stored, name))
        .map(|(id, _)| id))
}

/// Whether two database names are the same name, under the collation of `master`.
///
/// [`Collation::DEFAULT`] is `SQL_Latin1_General_CP1_CI_AS`: case-insensitive and
/// accent-sensitive, beyond ASCII as well as on it: `D_É` is the name `d_é` and
/// `d_e` is another name, the two shapes the unit test
/// `the_name_is_compared_case_insensitively_and_accent_sensitively` holds (module
/// documentation).
fn same_name(left: &str, right: &str) -> bool {
    Collation::DEFAULT.compare(left, right) == Ordering::Equal
}

/// The `int` a [`DbId`] is published as, `sys.databases.database_id` being an `int`.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| bug(&format!("database id {id} does not fit in an int")))
}

/// The [`DbId`] behind a `database_id` read from a row of [`DATABASES_TABLE`], the way back
/// from [`database_id`]: that column carries the identifier `storage` handed out, so a
/// [`CommitAction`] built from it names the database of the row (unit test
/// `the_two_readings_of_a_database_id_are_the_way_there_and_back`).
///
/// # Errors
///
/// [`InternalError::Bug`] for a negative number, which no [`database_id`] writes.
fn db_id(published: i32) -> SqlResult<DbId> {
    u32::try_from(published)
        .map(DbId)
        .map_err(|_| bug(&format!("database id {published} is negative")))
}

/// The value of the `collation_name` column for that collation: the name of the default
/// collation, or `NULL` for another one (see [`create_database`]).
fn collation_name(collation: Option<Collation>) -> Value {
    let collation = collation.unwrap_or(Collation::DEFAULT);
    if collation == Collation::DEFAULT {
        text(DEFAULT_COLLATION_NAME)
    } else {
        Value::Null
    }
}

/// The `nvarchar` value of a piece of text.
fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

/// An internal bug of this file, prefixed the way `bootstrap.rs` prefixes its own.
fn bug(message: &str) -> SqlError {
    InternalError::Bug(format!("database: {message}")).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_storage::{MemoryStorage, Storage, TableShape};
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo};

    /// A catalogue bootstrapped on a fresh `MemoryStorage`.
    fn bootstrapped() -> Catalog {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        Catalog::bootstrap(storage, txn).expect("bootstrap of a fresh storage")
    }

    /// The names of the databases `storage` carries, in the order `Storage::databases`
    /// lists them.
    fn stored_names(catalog: &Catalog) -> Vec<String> {
        catalog
            .storage
            .databases()
            .expect("databases")
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    /// Runs `body` in a transaction of `catalog` and commits it.
    fn committed<T>(catalog: &Catalog, body: impl FnOnce(&TxnHandle) -> T) -> T {
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let out = body(&handle);
        catalog.txn.commit(handle).expect("commit");
        out
    }

    #[test]
    fn the_name_is_compared_case_insensitively_and_accent_sensitively() {
        // The two statements of the module documentation: `D_É` is the name `d_é`
        // (1801) while `d_e` is another name (created).
        assert!(same_name("d_é", "D_É"));
        assert!(!same_name("d_é", "d_e"));
        // What separates this rule from an ASCII case fold: `eq_ignore_ascii_case` keeps `É`
        // and `é` apart, where the collation of `master` joins them.
        assert!(!"d_é".eq_ignore_ascii_case("D_É"));
        assert!(same_name("d", "D"));
    }

    #[test]
    fn a_new_database_writes_its_row_and_its_three_schemas() {
        let catalog = bootstrapped();
        let id = committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("create_database")
        });
        let published = database_id(id).unwrap();
        committed(&catalog, |txn| {
            let databases = rows_of(&catalog, txn, DATABASES_TABLE).unwrap();
            assert_eq!(databases.len(), 5, "the four system databases and `d`");
            let row = databases
                .into_iter()
                .map(|(_, values)| values)
                .find(|values| values[databases_columns::NAME] == text("d"))
                .expect("the row of `d`");
            assert_eq!(row[databases_columns::DATABASE_ID], Value::I32(published));
            assert_eq!(
                row[databases_columns::COLLATION_NAME],
                text(DEFAULT_COLLATION_NAME)
            );
            let schemas: Vec<Vec<Value>> = rows_of(&catalog, txn, SCHEMAS_TABLE)
                .unwrap()
                .into_iter()
                .map(|(_, values)| values)
                .filter(|values| values[schemas_columns::DATABASE_ID] == Value::I32(published))
                .collect();
            let mut names: Vec<Value> = schemas
                .iter()
                .map(|values| values[schemas_columns::NAME].clone())
                .collect();
            names.sort_by_key(|name| format!("{name:?}"));
            assert_eq!(
                names,
                vec![text("INFORMATION_SCHEMA"), text("dbo"), text("sys")]
            );
            for values in schemas {
                let Value::String(name) = &values[schemas_columns::NAME] else {
                    panic!("the name of a schema is an nvarchar");
                };
                let (_, schema_id, principal_id) = SYSTEM_SCHEMAS
                    .into_iter()
                    .find(|(schema, _, _)| *schema == name.text)
                    .expect("the schema is one of the three");
                assert_eq!(values[schemas_columns::SCHEMA_ID], Value::I32(schema_id));
                assert_eq!(
                    values[schemas_columns::PRINCIPAL_ID],
                    Value::I32(principal_id)
                );
            }
        });
    }

    #[test]
    fn a_collation_other_than_the_default_leaves_the_name_null() {
        assert_eq!(collation_name(None), text(DEFAULT_COLLATION_NAME));
        assert_eq!(
            collation_name(Some(Collation::DEFAULT)),
            text(DEFAULT_COLLATION_NAME)
        );
        let other = Collation::parse("Latin1_General_CS_AS").expect("a name of the 72");
        assert_ne!(other, Collation::DEFAULT);
        assert_eq!(collation_name(Some(other)), Value::Null);
    }

    #[test]
    fn a_dropped_database_takes_its_schema_rows_with_it() {
        let catalog = bootstrapped();
        committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("create_database");
            create_database(&catalog, txn, "e", None).expect("create_database");
        });
        committed(&catalog, |txn| {
            assert_eq!(rows_of(&catalog, txn, SCHEMAS_TABLE).unwrap().len(), 18);
            drop_database(&catalog, txn, "D").expect("drop_database");
            assert_eq!(rows_of(&catalog, txn, DATABASES_TABLE).unwrap().len(), 5);
            assert_eq!(rows_of(&catalog, txn, SCHEMAS_TABLE).unwrap().len(), 15);
        });
        committed(&catalog, |txn| {
            let names: Vec<Value> = rows_of(&catalog, txn, DATABASES_TABLE)
                .unwrap()
                .into_iter()
                .map(|(_, values)| values[databases_columns::NAME].clone())
                .collect();
            assert!(!names.contains(&text("d")), "the row of `d` is gone");
            assert!(names.contains(&text("e")), "the row of `e` is still there");
        });
    }

    #[test]
    fn the_rows_of_a_create_are_invisible_to_another_transaction_until_the_commit() {
        let catalog = bootstrapped();
        let writer = catalog.txn.begin(IsolationLevel::ReadCommitted);
        create_database(&catalog, &writer, "d", None).expect("create_database");
        let reader = catalog.txn.begin(IsolationLevel::ReadCommitted);
        assert!(database_row(&catalog, &reader, "d").unwrap().is_none());
        assert!(database_row(&catalog, &writer, "d").unwrap().is_some());
        catalog.txn.commit(writer).expect("commit");
        assert!(database_row(&catalog, &reader, "d").unwrap().is_some());
        catalog.txn.commit(reader).expect("commit");
    }

    #[test]
    fn the_two_readings_of_a_database_id_are_the_way_there_and_back() {
        for id in [1_u32, 4, 5, 4_000_000_000] {
            let published = database_id(DbId(id));
            match published {
                Ok(published) => assert_eq!(db_id(published).unwrap(), DbId(id)),
                // Above `i32::MAX` the column cannot carry the identifier; `MemoryStorage`
                // numbers from 1, so the shape is refused rather than truncated.
                Err(err) => assert!(err.message.contains("does not fit in an int")),
            }
        }
        assert!(db_id(-1).unwrap_err().message.contains("is negative"));
    }

    #[test]
    fn a_name_a_concurrent_transaction_holds_in_storage_is_1801() {
        let catalog = bootstrapped();
        // The first transaction creates `d`: `storage` carries the name at once, while the
        // row stays invisible to the second transaction under MVCC.
        let first = catalog.txn.begin(IsolationLevel::ReadCommitted);
        create_database(&catalog, &first, "d", None).expect("create_database");
        let second = catalog.txn.begin(IsolationLevel::ReadCommitted);
        assert!(database_row(&catalog, &second, "d").unwrap().is_none());
        let err = create_database(&catalog, &second, "D", None)
            .expect_err("the name is held by the first transaction");
        assert_eq!(err.number, 1801);
        assert_eq!(err.state, 3);
        assert!(err.message.contains("'D'"), "{}", err.message);
        // The second wrote nothing, so the rollback of the first destroys its own database
        // and no committed one, and it leaves the name free.
        catalog.txn.commit(second).expect("commit");
        catalog.txn.rollback(first).expect("rollback");
        assert_eq!(
            stored_names(&catalog),
            ["master", "tempdb", "model", "msdb"]
        );
        committed(&catalog, |txn| {
            assert!(database_row(&catalog, txn, "d").unwrap().is_none());
            create_database(&catalog, txn, "d", None).expect("the name is free again");
        });
    }

    #[test]
    fn a_database_storage_holds_with_a_table_is_1801_and_kept_as_it_is() {
        let catalog = bootstrapped();
        // A database of `storage` the catalogue has no row for, carrying a table: taking it
        // over would hand out a `CREATE DATABASE` whose database is not empty.
        let existing = catalog
            .storage
            .create_database("d")
            .expect("create_database");
        let shape = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, false)],
            clustered_key: None,
        };
        catalog
            .storage
            .create_table(existing, &shape)
            .expect("create_table");
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let err = create_database(&catalog, &handle, "d", None).expect_err("the name is taken");
        assert_eq!(err.number, 1801);
        catalog.txn.commit(handle).expect("commit");
        // The database of `storage` is the one that was there, table included, and the
        // catalogue holds no row for it.
        assert_eq!(catalog.storage.tables(existing).unwrap().len(), 1);
        assert_eq!(
            stored_names(&catalog),
            ["master", "tempdb", "model", "msdb", "d"]
        );
        committed(&catalog, |txn| {
            assert!(database_row(&catalog, txn, "d").unwrap().is_none());
        });
    }

    #[test]
    fn a_create_after_a_drop_of_the_same_name_in_one_transaction_is_1801() {
        let catalog = bootstrapped();
        committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("create_database")
        });
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        drop_database(&catalog, &handle, "d").expect("drop_database");
        let err = create_database(&catalog, &handle, "d", None)
            .expect_err("`storage` carries `d` until the commit runs the deferred drop");
        assert_eq!(err.number, 1801);
        // The commit runs that drop, and the name is free for the next transaction.
        catalog.txn.commit(handle).expect("commit");
        assert_eq!(
            stored_names(&catalog),
            ["master", "tempdb", "model", "msdb"]
        );
        committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("the name is free again");
        });
    }

    /// The row of `d` as [`DATABASES_TABLE`] holds it for `txn`.
    fn row_of(catalog: &Catalog, txn: &TxnHandle, name: &str) -> Vec<Value> {
        rows_of(catalog, txn, DATABASES_TABLE)
            .expect("the rows of the internal table")
            .into_iter()
            .map(|(_, values)| values)
            .find(|values| values[databases_columns::NAME] == text(name))
            .unwrap_or_else(|| panic!("the row of `{name}`"))
    }

    /// The [`DatabaseMeta`](crate::DatabaseMeta) of `name` in the snapshot of `txn`.
    fn meta_of(catalog: &Catalog, txn: &TxnHandle, name: &str) -> crate::DatabaseMeta {
        catalog
            .snapshot(txn)
            .database(name)
            .cloned()
            .unwrap_or_else(|| panic!("the metadata of `{name}`"))
    }

    #[test]
    fn new_database_defaults_are_off() {
        // After `CREATE DATABASE d`, `sys.databases` shows `is_read_committed_snapshot_on` 0
        // and `snapshot_isolation_state` 0 / `OFF` for `d`.
        assert_eq!(NEW_DATABASE_OPTIONS, (false, SnapshotIsolationState::Off));
        let catalog = bootstrapped();
        committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("create_database");
            let meta = meta_of(&catalog, txn, "d");
            assert!(
                !meta.read_committed_snapshot,
                "is_read_committed_snapshot_on"
            );
            assert_eq!(meta.snapshot_isolation, SnapshotIsolationState::Off);
            assert_eq!(meta.snapshot_isolation.state(), 0);
            assert_eq!(meta.snapshot_isolation.desc(), "OFF");
            // The row the view reads carries the same couple.
            let row = row_of(&catalog, txn, "d");
            assert_eq!(
                row[databases_columns::READ_COMMITTED_SNAPSHOT],
                Value::Bit(false)
            );
            assert_eq!(
                row[databases_columns::SNAPSHOT_ISOLATION_STATE],
                Value::I8(0)
            );
        });
    }

    #[test]
    fn snapshot_isolation_state_desc_has_four_labels() {
        // 0 / `OFF` and 1 / `ON` are the two states a database rests in. 2 and 3 are the
        // transitional ones: a database shows them while a switch waits for another session.
        let couples = [
            (0_u8, "OFF", SnapshotIsolationState::Off),
            (1, "ON", SnapshotIsolationState::On),
            (
                2,
                "IN_TRANSITION_TO_ON",
                SnapshotIsolationState::InTransitionToOn,
            ),
            (
                3,
                "IN_TRANSITION_TO_OFF",
                SnapshotIsolationState::InTransitionToOff,
            ),
        ];
        for (state, desc, value) in couples {
            assert_eq!(value.state(), state, "state of {value:?}");
            assert_eq!(value.desc(), desc, "desc of {value:?}");
            assert_eq!(SnapshotIsolationState::from_state(state), Some(value));
        }
        // A number outside the four is not a state: the reader of a row falls back rather
        // than inventing a label (`snapshot.rs`).
        assert_eq!(SnapshotIsolationState::from_state(4), None);
        assert_eq!(SnapshotIsolationState::from_state(255), None);
    }

    #[test]
    fn set_database_option_changes_the_snapshot_view() {
        let catalog = bootstrapped();
        let id = committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("create_database")
        });
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        assert!(!meta_of(&catalog, &handle, "d").read_committed_snapshot);
        catalog
            .set_database_option(&handle, id, DatabaseOption::ReadCommittedSnapshot, true)
            .expect("set_database_option");
        // The metadata of the same transaction, and the internal row behind `sys.databases`.
        let meta = meta_of(&catalog, &handle, "d");
        assert!(meta.read_committed_snapshot);
        assert_eq!(meta.snapshot_isolation, SnapshotIsolationState::Off);
        let row = row_of(&catalog, &handle, "d");
        assert_eq!(
            row[databases_columns::READ_COMMITTED_SNAPSHOT],
            Value::Bit(true)
        );
        assert_eq!(
            row[databases_columns::SNAPSHOT_ISOLATION_STATE],
            Value::I8(0),
            "the other option is left where it was"
        );
        // The second option is written on top of the first without undoing it.
        catalog
            .set_database_option(&handle, id, DatabaseOption::AllowSnapshotIsolation, true)
            .expect("set_database_option");
        let meta = meta_of(&catalog, &handle, "d");
        assert!(meta.read_committed_snapshot);
        assert_eq!(meta.snapshot_isolation, SnapshotIsolationState::On);
        assert_eq!(
            row_of(&catalog, &handle, "d")[databases_columns::SNAPSHOT_ISOLATION_STATE],
            Value::I8(1)
        );
        // A versioned row: another transaction reads the values of before until the commit.
        let reader = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let before = meta_of(&catalog, &reader, "d");
        assert!(!before.read_committed_snapshot);
        assert_eq!(before.snapshot_isolation, SnapshotIsolationState::Off);
        catalog.txn.commit(handle).expect("commit");
        catalog.txn.commit(reader).expect("commit");
        committed(&catalog, |txn| {
            let after = meta_of(&catalog, txn, "d");
            assert!(after.read_committed_snapshot);
            assert_eq!(after.snapshot_isolation, SnapshotIsolationState::On);
            // The other databases are untouched: `master` keeps its couple.
            let master = meta_of(&catalog, txn, "master");
            assert!(!master.read_committed_snapshot);
            assert_eq!(master.snapshot_isolation, SnapshotIsolationState::On);
        });
    }

    #[test]
    fn set_database_option_rolls_back() {
        let catalog = bootstrapped();
        let id = committed(&catalog, |txn| {
            create_database(&catalog, txn, "d", None).expect("create_database")
        });
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        for option in [
            DatabaseOption::ReadCommittedSnapshot,
            DatabaseOption::AllowSnapshotIsolation,
        ] {
            catalog
                .set_database_option(&handle, id, option, true)
                .expect("set_database_option");
        }
        // Switched for the transaction that wrote it, so the rollback below has something to
        // undo.
        let switched = meta_of(&catalog, &handle, "d");
        assert!(switched.read_committed_snapshot);
        assert_eq!(switched.snapshot_isolation, SnapshotIsolationState::On);
        catalog.txn.rollback(handle).expect("rollback");
        committed(&catalog, |txn| {
            let meta = meta_of(&catalog, txn, "d");
            assert!(!meta.read_committed_snapshot, "the value of before");
            assert_eq!(meta.snapshot_isolation, NEW_DATABASE_OPTIONS.1);
            let row = row_of(&catalog, txn, "d");
            assert_eq!(
                row[databases_columns::READ_COMMITTED_SNAPSHOT],
                Value::Bit(false)
            );
            assert_eq!(
                row[databases_columns::SNAPSHOT_ISOLATION_STATE],
                Value::I8(0)
            );
        });
    }

    #[test]
    fn set_database_option_on_a_database_the_catalogue_does_not_hold_is_an_internal_bug() {
        let catalog = bootstrapped();
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let err = catalog
            .set_database_option(
                &handle,
                DbId(4_000),
                DatabaseOption::ReadCommittedSnapshot,
                true,
            )
            .expect_err("no row carries that identifier");
        assert!(
            err.message.contains("holds no row this transaction sees"),
            "{}",
            err.message
        );
        catalog.txn.rollback(handle).expect("rollback");
    }

    #[test]
    fn a_table_the_bootstrap_does_not_describe_is_an_internal_bug() {
        let catalog = bootstrapped();
        // A name no file of `views/` describes.
        let err = table_of(&catalog, "vauban_sys_nowhere").unwrap_err();
        assert_eq!(
            err.message,
            "Internal error: internal bug: database: internal table vauban_sys_nowhere is not in this storage"
        );
    }
}
