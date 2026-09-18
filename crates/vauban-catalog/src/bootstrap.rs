//! Bootstrap of the catalogue: system databases, schemas and internal tables. Fills
//! [`Catalog::bootstrap`](crate::Catalog::bootstrap) and iterates the `internal_tables()` of
//! the files of `views/`.
//!
//! # What a first start creates
//!
//! On a storage where [`Storage::databases`] reports nothing named `master`, in this order:
//!
//! 1. the four system databases of [`SYSTEM_DATABASES`] — `master`, `tempdb`, `model`,
//!    `msdb` — with the collation [`Collation::DEFAULT`](vauban_types::Collation::DEFAULT),
//!    whose name is [`DEFAULT_COLLATION_NAME`];
//! 2. in `master`, the internal tables: those [`crate::views::internal_tables`] describes,
//!    then the two the bootstrap owns, [`DATABASES_TABLE`] and [`SCHEMAS_TABLE`], dropped
//!    when a file of `views/` already describes a table of that name;
//! 3. the rows each description carries: one per database in [`DATABASES_TABLE`], one per
//!    (database, schema) pair in [`SCHEMAS_TABLE`], which is 4 and 12 rows on a fresh
//!    `MemoryStorage` (unit test `a_fresh_bootstrap_writes_four_and_twelve_rows`).
//!
//! The table a view reads arrives with that view (`views/mod.rs`). The two tables above sit
//! here rather than in `views/sys_core.rs` because their rows are not constants — they carry
//! the [`DbId`] that `storage` handed out during this bootstrap — and `views/sys_core.rs`
//! gives them the T-SQL text of `sys.databases` and `sys.schemas`.
//!
//! # Order of the tables in `master`
//!
//! [`crate::views::internal_tables`] offers the tables the files of `views/` describe, the
//! table of objects first (`views/mod.rs`), and the bootstrap appends its own two behind
//! them: [`DATABASES_TABLE`] and [`SCHEMAS_TABLE`] close the list, a file of `views/` adding
//! its tables in front (unit test `the_tables_of_master_are_the_ones_the_files_describe`).
//!
//! # Restart
//!
//! A second call on the same storage creates nothing: [`already_bootstrapped`] finds a
//! database named `master` with at least one table in it, and the call gives back the same
//! [`DbId`]s (integration tests `bootstrap_is_idempotent` and `restart_finds_master`,
//! `tests/bootstrap.rs`).
//!
//! # Transaction
//!
//! The rows are written inside a transaction the catalogue opens itself,
//! `begin(IsolationLevel::ReadCommitted)`, committed at the end. The `create_database` and
//! `create_table` calls sit outside it: DDL is immediate in `storage` and takes no `TxnId`
//! (`Storage` documentation: DDL is not transactional at the storage level). Compensation
//! actions are not registered for them: a bootstrap that fails is fatal to the instance, and
//! the server turns it into a panic at start-up. On the error path the transaction is rolled
//! back rather than left open, so its rows stay invisible to a later reader.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, Row, Storage, TableId, TableShape};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{
    Date, DateTime, DateTime2, Len, SqlString, SqlType, Time, TypeInfo, Value,
    calendar::days_from_civil, convert,
};

use crate::catalog::Catalog;
use crate::def::{InternalColumnDef, InternalTableDef};
use crate::meta::SnapshotIsolationState;
use crate::views;

/// The system databases, in the order the bootstrap creates them.
///
/// That order is the one of `sys.databases.database_id`: `master` 1, `tempdb` 2, `model` 3,
/// `msdb` 4. `MemoryStorage` numbers its databases from 1 in creation order, so a fresh
/// instance hands out those four numbers (integration test
/// `bootstrap_creates_four_system_databases`). `tempdb` is created empty: temporary tables
/// are not implemented yet.
pub(crate) const SYSTEM_DATABASES: [&str; 4] = ["master", "tempdb", "model", "msdb"];

/// The two versioning options of each system database, as
/// `(name, is_read_committed_snapshot_on, snapshot_isolation_state)`.
///
/// `master` 0 / 1 `ON`, `tempdb` 0 / 0 `OFF`, `model` 0 / 0 `OFF`, `msdb` 0 / 1 `ON`.
/// `master` and `msdb` differ from the database a `CREATE DATABASE` makes
/// ([`crate::database::NEW_DATABASE_OPTIONS`], 0 / 0 `OFF`), which is why the bootstrap
/// writes a value per database rather than one for the four (unit test
/// `system_databases_take_their_own_values`).
pub(crate) const SYSTEM_DATABASE_OPTIONS: [(&str, bool, SnapshotIsolationState); 4] = [
    ("master", false, SnapshotIsolationState::On),
    ("tempdb", false, SnapshotIsolationState::Off),
    ("model", false, SnapshotIsolationState::Off),
    ("msdb", false, SnapshotIsolationState::On),
];

/// The name of [`Collation::DEFAULT`](vauban_types::Collation::DEFAULT), written in the
/// `collation_name` column of [`DATABASES_TABLE`].
///
/// `Collation::parse` of this name gives that collation back (unit test
/// `the_default_collation_name_parses_back_to_the_default_collation`), which is what ties
/// the text stored here to the five wire bytes `storage` and `tds` handle.
pub(crate) const DEFAULT_COLLATION_NAME: &str = "SQL_Latin1_General_CP1_CI_AS";

/// The schemas the bootstrap puts in each of the four system databases, as
/// `(name, schema_id, principal_id)`.
///
/// The numbers are those `master.sys.schemas` publishes: `dbo` 1/1, `INFORMATION_SCHEMA`
/// 3/3, `sys` 4/4. The other rows of that view — `guest` (2) and the `db_*` database roles
/// from 16384 up — describe database principals VaubanDB does not serve, so their numbers
/// are left unused rather than reassigned. Being constants rather than a counter, these identifiers do not
/// depend on the order the rows were written in, so a restart publishes them unchanged
/// (integration test `bootstrap_creates_three_schemas_in_master`).
pub(crate) const SYSTEM_SCHEMAS: [(&str, i32, i32); 3] =
    [("dbo", 1, 1), ("INFORMATION_SCHEMA", 3, 3), ("sys", 4, 4)];

/// Internal table of the databases, read by `sys.databases`.
///
/// A name of our own: an internal table is not one of the tables SQL Server holds, and what
/// a client reads is a view built over it.
pub(crate) const DATABASES_TABLE: &str = "vauban_sys_databases";

/// Internal table of the schemas, read by `sys.schemas` and `INFORMATION_SCHEMA.SCHEMATA`.
///
/// One row per (database, schema) pair: `sys.schemas` is a per-database view while the
/// internal tables live in `master`, so the database a schema belongs to is a column here.
pub(crate) const SCHEMAS_TABLE: &str = "vauban_sys_schemas";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// Where each column of [`DATABASES_TABLE`] sits in a [`Row`].
///
/// The order of an internal table is the business of the bootstrap, which declares the
/// columns; the code that writes a row of the table — `database.rs` for a user database —
/// addresses it through these constants instead of restating the order. The bootstrap builds
/// its own rows through them too, and the unit test
/// `the_column_order_of_the_internal_tables_is_the_one_the_constants_name` ties them to the
/// declared column names.
pub(crate) mod databases_columns {
    /// `database_id int`: the `DbId` of the database, published as an `int`.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `name nvarchar(128)`: the name of the database.
    pub(crate) const NAME: usize = 1;
    /// `collation_name nvarchar(128)`, nullable: the name of the collation of the database.
    pub(crate) const COLLATION_NAME: usize = 2;
    /// `is_read_committed_snapshot_on bit`: `READ_COMMITTED_SNAPSHOT` of the database,
    /// written by `set_database_option` and published by `sys.databases` under that name.
    pub(crate) const READ_COMMITTED_SNAPSHOT: usize = 3;
    /// `snapshot_isolation_state tinyint`: the number of
    /// [`SnapshotIsolationState`](crate::meta::SnapshotIsolationState); the view derives
    /// `snapshot_isolation_state_desc` from it (`views/sys_core.rs`).
    pub(crate) const SNAPSHOT_ISOLATION_STATE: usize = 4;
    /// `owner_sid varbinary(85)`: the owner of the database, `0x01` for the bootstrap and
    /// for a database created by `sa`.
    pub(crate) const OWNER_SID: usize = 5;
    /// `create_date datetime`: the instant the database was created.
    pub(crate) const CREATE_DATE: usize = 6;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 7;
}

/// `owner_sid` written for each database: `0x01`.
pub(crate) const SA_OWNER_SID: [u8; 1] = [0x01];

/// Where each column of [`SCHEMAS_TABLE`] sits in a [`Row`]. Same rule as
/// [`databases_columns`].
pub(crate) mod schemas_columns {
    /// `database_id int`: the database the schema belongs to.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `schema_id int`: the identifier `sys.schemas.schema_id` publishes.
    pub(crate) const SCHEMA_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the schema.
    pub(crate) const NAME: usize = 2;
    /// `principal_id int`, nullable: the owner of the schema.
    pub(crate) const PRINCIPAL_ID: usize = 3;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 4;
}

/// Opens the catalogue of `storage`, creating what a first start needs. See
/// [`Catalog::bootstrap`](crate::Catalog::bootstrap) and the module documentation above.
///
/// # Errors
///
/// [`InternalError`] when a call to `storage` or to the transaction manager fails; the
/// message keeps the text of the underlying [`SqlError`], prefixed with `bootstrap:`.
pub(crate) fn bootstrap(
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
) -> Result<Catalog, InternalError> {
    let catalog = Catalog {
        storage,
        txn,
        tables: Default::default(),
    };
    if already_bootstrapped(&catalog)? {
        return Ok(catalog);
    }
    let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
    match install(&catalog, &handle) {
        Ok(()) => catalog.txn.commit(handle).map_err(internal)?,
        Err(err) => {
            catalog.txn.rollback(handle).map_err(internal)?;
            return Err(err);
        }
    }
    Ok(catalog)
}

/// Whether this storage already carries a bootstrapped catalogue.
///
/// A database named `master` in [`Storage::databases`], holding at least one table. The
/// name is compared without regard
/// to case, like the database names a client writes.
fn already_bootstrapped(catalog: &Catalog) -> Result<bool, InternalError> {
    let Some(master) = find_database(catalog, SYSTEM_DATABASES[0])? else {
        return Ok(false);
    };
    let tables = catalog.storage.tables(master).map_err(internal)?;
    Ok(!tables.is_empty())
}

/// Creates the databases, the internal tables and their rows, the rows inside `txn`.
///
/// A database of `SYSTEM_DATABASES` that is already there is reused with its [`DbId`]
/// instead of being created again, which is what makes a half-done bootstrap — databases
/// created, tables missing — finish rather than fail.
///
/// When the stored spelling differs from ours, the row of [`DATABASES_TABLE`] carries the
/// constant of [`SYSTEM_DATABASES`] and not what `storage` kept: a `MSDB` found in storage
/// is published `msdb`, the spelling this product gives its system databases, while
/// `Storage::databases` keeps `MSDB` (unit test
/// `a_database_already_there_is_reused_rather_than_created_again`). The two spellings
/// resolve to the same database, the comparison of names being case-insensitive.
fn install(catalog: &Catalog, txn: &TxnHandle) -> Result<(), InternalError> {
    let mut databases = Vec::with_capacity(SYSTEM_DATABASES.len());
    for name in SYSTEM_DATABASES {
        let id = match find_database(catalog, name)? {
            Some(id) => id,
            None => catalog.storage.create_database(name).map_err(internal)?,
        };
        databases.push((id, name));
    }
    let master = databases[0].0;
    let instant = transaction_datetime(catalog, txn).map_err(internal)?;
    for def in internal_tables(&databases, &instant)? {
        let table = catalog
            .storage
            .create_table(master, &shape_of(&def))
            .map_err(internal)?;
        insert_rows(catalog, txn, table, &def)?;
    }
    views::sys_tables::write_internal_object_rows(catalog, txn, master, instant)
        .map_err(internal)?;
    Ok(())
}

/// Inserts the rows of a description into the table `storage` has just created for it.
///
/// A row whose width differs from the column list is a bug in the file that described the
/// table; it is refused here with the name of that table, which `storage` cannot give since
/// it holds no name (unit test
/// `a_row_of_the_wrong_width_is_refused_before_storage_sees_it`).
fn insert_rows(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: TableId,
    def: &InternalTableDef,
) -> Result<(), InternalError> {
    for row in &def.rows {
        if row.0.len() > def.columns.len() {
            return Err(InternalError::Bug(format!(
                "bootstrap: table {} describes {} columns and carries a row of {} values",
                def.name,
                def.columns.len(),
                row.0.len()
            )));
        }
        let mut values = row.0.clone();
        while values.len() < def.columns.len() {
            values.push(Value::Null);
        }
        catalog
            .storage
            .insert(txn.id, table, &Row(values))
            .map_err(internal)?;
    }
    Ok(())
}

/// The internal tables to create in `master`, in creation order.
///
/// Those of [`crate::views::internal_tables`] first — that list is headed by the table of
/// objects — then the tables of the bootstrap whose name a file of `views/` has not taken.
/// The comparison of the names ignores case, as `SQL_Latin1_General_CP1_CI_AS` does for an
/// identifier.
fn internal_tables(
    databases: &[(DbId, &str)],
    instant: &Value,
) -> Result<Vec<InternalTableDef>, InternalError> {
    let mut tables = views::internal_tables();
    for own in bootstrap_tables(databases, instant)? {
        if !tables
            .iter()
            .any(|table| table.name.eq_ignore_ascii_case(&own.name))
        {
            tables.push(own);
        }
    }
    Ok(tables)
}

/// The descriptions of the internal tables of `master`, in the order [`internal_tables`]
/// creates them, with the rows of the two tables of the bootstrap left empty.
///
/// `snapshot.rs` turns each description into a [`TableMeta`](crate::TableMeta) and reads the
/// [`TableId`] of the table from the position of its description, as [`internal_table_id`]
/// does — one call for the whole list rather than one call per table. The rows are empty
/// because they are built from the [`DbId`]s of a bootstrap and `[]` is passed here; a
/// reader of this function wants the names and the columns. The order is the one the unit
/// test `the_tables_of_master_are_the_ones_the_files_describe` states, and the pairing it
/// serves is held by `snapshot.rs`, unit test
/// `the_storage_id_of_an_internal_table_is_the_one_internal_table_id_finds`.
///
/// # Errors
///
/// The error [`internal_tables`] answers with.
pub(crate) fn internal_table_defs() -> Result<Vec<InternalTableDef>, InternalError> {
    let placeholder = Value::DateTime(DateTime {
        days: 0,
        ticks_300th: 0,
    });
    internal_tables(&[], &placeholder)
}

/// The two internal tables the bootstrap owns, carrying the rows it computed for them.
///
/// Their `views` field comes from
/// [`views::sys_core::bootstrap_views`](crate::views::sys_core::bootstrap_views): the T-SQL
/// text of `sys.databases` and `sys.schemas` is the business of that file, while the two
/// tables stay here because their rows carry the [`DbId`]s of this bootstrap (unit test
/// `the_two_tables_of_the_bootstrap_carry_the_views_of_sys_core`).
fn bootstrap_tables(
    databases: &[(DbId, &str)],
    instant: &Value,
) -> Result<Vec<InternalTableDef>, InternalError> {
    let mut database_rows = Vec::with_capacity(databases.len());
    let mut schema_rows = Vec::with_capacity(databases.len() * SYSTEM_SCHEMAS.len());
    for &(id, name) in databases {
        let id = database_id(id)?;
        let mut row = vec![Value::Null; databases_columns::WIDTH];
        row[databases_columns::DATABASE_ID] = Value::I32(id);
        row[databases_columns::NAME] = text(name);
        row[databases_columns::COLLATION_NAME] = text(DEFAULT_COLLATION_NAME);
        let (read_committed_snapshot, snapshot_isolation) = database_options(name);
        row[databases_columns::READ_COMMITTED_SNAPSHOT] = Value::Bit(read_committed_snapshot);
        row[databases_columns::SNAPSHOT_ISOLATION_STATE] = Value::I8(snapshot_isolation.state());
        row[databases_columns::OWNER_SID] = owner_sid_value();
        row[databases_columns::CREATE_DATE] = instant.clone();
        database_rows.push(Row(row));
        for (schema, schema_id, principal_id) in SYSTEM_SCHEMAS {
            let mut row = vec![Value::Null; schemas_columns::WIDTH];
            row[schemas_columns::DATABASE_ID] = Value::I32(id);
            row[schemas_columns::SCHEMA_ID] = Value::I32(schema_id);
            row[schemas_columns::NAME] = text(schema);
            row[schemas_columns::PRINCIPAL_ID] = Value::I32(principal_id);
            schema_rows.push(Row(row));
        }
    }
    Ok(vec![
        InternalTableDef {
            name: DATABASES_TABLE.to_owned(),
            columns: vec![
                column("database_id", SqlType::Int, false),
                column("name", SYSNAME, false),
                // `sys.databases.collation_name` is NULL while a database is being restored;
                // the bootstrap writes the name of the collation the database was created
                // with.
                column("collation_name", SYSNAME, true),
                // The two versioning options, with the types of the columns of
                // `sys.databases` that publish them: `bit` and `tinyint`.
                column("is_read_committed_snapshot_on", SqlType::Bit, false),
                column("snapshot_isolation_state", SqlType::TinyInt, false),
                column("owner_sid", SqlType::VarBinary(Len::Fixed(85)), true),
                column("create_date", SqlType::DateTime, false),
            ],
            clustered_key: None,
            rows: database_rows,
            views: views::sys_core::bootstrap_views(DATABASES_TABLE),
        },
        InternalTableDef {
            name: SCHEMAS_TABLE.to_owned(),
            columns: vec![
                column("database_id", SqlType::Int, false),
                column("schema_id", SqlType::Int, false),
                column("name", SYSNAME, false),
                column("principal_id", SqlType::Int, true),
            ],
            clustered_key: None,
            rows: schema_rows,
            views: views::sys_core::bootstrap_views(SCHEMAS_TABLE),
        },
    ])
}

/// The shape `storage` needs from a description: the types of its columns, in order.
///
/// [`TableShape`] has a type per column and not a name — `storage` has use for the first
/// and not the second — so the names stay in the [`InternalTableDef`] and the catalogue is
/// what knows them. The two tables of the bootstrap are heaps: a scan of a heap comes back
/// in an order the `Storage` trait leaves open, which is why the tests that read them sort
/// before comparing.
fn shape_of(def: &InternalTableDef) -> TableShape {
    TableShape {
        columns: def.columns.iter().map(|column| column.ty.clone()).collect(),
        clustered_key: def.clustered_key.clone(),
    }
}

/// The [`TableId`] of the internal table called `name`, `None` when this storage holds no
/// such table.
///
/// How the rest of the crate reaches an internal table: `storage` keeps the shape of a table
/// and not its name, so the name lives in the description the bootstrap built and the
/// position of that description in [`internal_tables`] is the position of the table in
/// [`Storage::tables`] of `master` — which comes back sorted by increasing [`TableId`], the
/// order the bootstrap created them in. `database.rs` writes the row of a user database
/// into [`DATABASES_TABLE`] through this function and [`databases_columns`], without
/// restating the order of `bootstrap.rs`.
///
/// The position holds for the tables this bootstrap created in `master`; a table created in
/// `master` by something else before the bootstrap would shift it (unit test
/// `internal_table_id_finds_the_two_tables_of_the_bootstrap`).
///
/// # Errors
///
/// [`InternalError`] when a call to `storage` fails.
#[allow(dead_code)] // kept for the code that writes the row of a user database
pub(crate) fn internal_table_id(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<TableId>, InternalError> {
    let Some(master) = find_database(catalog, SYSTEM_DATABASES[0])? else {
        return Ok(None);
    };
    let Some(position) = internal_table_defs()?
        .iter()
        .position(|def| def.name.eq_ignore_ascii_case(name))
    else {
        return Ok(None);
    };
    let tables = catalog.storage.tables(master).map_err(internal)?;
    Ok(tables.get(position).map(|&(id, _)| id))
}

/// The database of that name, `None` when the name matches nothing.
///
/// Compared without regard to case, as `SQL_Latin1_General_CP1_CI_AS` compares identifiers:
/// `storage` stores the name as it was given and does not fold it
/// ([`Storage::create_database`]).
fn find_database(catalog: &Catalog, name: &str) -> Result<Option<DbId>, InternalError> {
    let databases = catalog.storage.databases().map_err(internal)?;
    Ok(databases
        .into_iter()
        .find(|(_, stored)| stored.eq_ignore_ascii_case(name))
        .map(|(id, _)| id))
}

/// The `int` a [`DbId`] is published as, `sys.databases.database_id` being an `int`.
fn database_id(id: DbId) -> Result<i32, InternalError> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "bootstrap: database id {id} does not fit in an int"
        ))
    })
}

/// The two versioning options of the database called `name`: its entry in
/// [`SYSTEM_DATABASE_OPTIONS`], and [`crate::database::NEW_DATABASE_OPTIONS`] for a name that
/// is not one of the four.
///
/// The names are compared without regard to case, as the bootstrap compares the name of a
/// database elsewhere ([`find_database`]). The fallback is the default of a fresh database,
/// so a name outside the four gets what `CREATE DATABASE` gives (unit test
/// `system_databases_take_their_own_values`).
fn database_options(name: &str) -> (bool, SnapshotIsolationState) {
    SYSTEM_DATABASE_OPTIONS
        .into_iter()
        .find(|(system, _, _)| system.eq_ignore_ascii_case(name))
        .map(|(_, read_committed_snapshot, snapshot_isolation)| {
            (read_committed_snapshot, snapshot_isolation)
        })
        .unwrap_or(crate::database::NEW_DATABASE_OPTIONS)
}

/// A column of an internal table; [`TypeInfo::new`] gives a character type the default
/// collation.
fn column(name: &str, ty: SqlType, nullable: bool) -> InternalColumnDef {
    InternalColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

/// The `nvarchar` value of a piece of text.
fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

/// The `varbinary(85)` value `sys.databases.owner_sid` carries for a database created here.
pub(crate) fn owner_sid_value() -> Value {
    Value::Bytes(SA_OWNER_SID.to_vec())
}

/// The `datetime` of `txn`, taken from when the transaction began.
///
/// Two objects created in the same transaction carry the same instant.
///
/// # Errors
///
/// [`InternalError::Bug`] when `txn` is not open on this manager.
pub(crate) fn transaction_datetime(catalog: &Catalog, txn: &TxnHandle) -> SqlResult<Value> {
    catalog
        .txn
        .active_sessions()
        .into_iter()
        .find(|info| info.id == txn.id)
        .map(|info| Value::DateTime(system_time_to_datetime(info.began_at)))
        .ok_or_else(|| {
            InternalError::Bug(format!(
                "Catalog: transaction {} is not open on this manager",
                txn.id
            ))
            .into()
        })
}

/// Maps a [`SystemTime`] to the `datetime` the internal tables store.
fn system_time_to_datetime(time: SystemTime) -> DateTime {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_PER_DAY: u64 = 86_400;
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let ticks_100ns = elapsed
        .as_secs()
        .saturating_mul(TICKS_PER_SECOND)
        .saturating_add(u64::from(elapsed.subsec_nanos()) / 100);
    let dt2 = DateTime2 {
        date: Date {
            days: days_from_civil(1970, 1, 1)
                .saturating_add((elapsed.as_secs() / SECONDS_PER_DAY) as i32),
        },
        time: Time {
            ticks_100ns: (elapsed.as_secs() % SECONDS_PER_DAY) * TICKS_PER_SECOND
                + ticks_100ns % TICKS_PER_SECOND,
        },
    };
    match convert(
        &Value::DateTime2(dt2),
        &TypeInfo::new(SqlType::DateTime2(7), false),
        &TypeInfo::new(SqlType::DateTime, false),
        None,
    ) {
        Ok(Value::DateTime(dt)) => dt,
        _ => DateTime {
            days: 0,
            ticks_300th: 0,
        },
    }
}

/// Turns the error of a `storage` or `txn` call into the one `bootstrap` answers with.
///
/// Those two layers hand back a [`SqlError`] built from an [`InternalError`] (number 50000)
/// while the signature of the bootstrap wants the internal form, so the message is kept and
/// the variant becomes [`InternalError::Bug`].
fn internal(err: SqlError) -> InternalError {
    InternalError::Bug(format!("bootstrap: {}", err.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use vauban_storage::MemoryStorage;
    use vauban_types::Collation;

    /// A catalogue bootstrapped on a fresh `MemoryStorage`.
    fn bootstrapped() -> Catalog {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        bootstrap(storage, txn).expect("bootstrap of a fresh storage")
    }

    /// The rows of the internal table of that name, sorted by their debug form.
    ///
    /// `Storage::tables` sorts by increasing `TableId`, which is the order `internal_tables`
    /// created them in, so the position of the name in that list is the position of the
    /// table. `bootstrap_tables(&[])` describes the same tables with no row, which is enough
    /// to read the position.
    fn rows_of(catalog: &Catalog, name: &str) -> Vec<Vec<Value>> {
        let master = find_database(catalog, "master").unwrap().unwrap();
        let position = internal_table_defs()
            .unwrap()
            .iter()
            .position(|def| def.name == name)
            .expect("the name is one of the internal tables");
        let table = catalog.storage.tables(master).unwrap()[position].0;
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = catalog.txn.statement_snapshot(&handle);
        let mut rows: Vec<Vec<Value>> = catalog
            .storage
            .scan(&snapshot, table)
            .unwrap()
            .map(|row| row.unwrap().1.0)
            .collect();
        catalog.txn.commit(handle).unwrap();
        rows.sort_by_key(|row| format!("{row:?}"));
        rows
    }

    #[test]
    fn the_default_collation_name_parses_back_to_the_default_collation() {
        assert_eq!(
            Collation::parse(DEFAULT_COLLATION_NAME).unwrap(),
            Collation::DEFAULT
        );
    }

    #[test]
    fn a_fresh_bootstrap_writes_four_and_twelve_rows() {
        let catalog = bootstrapped();
        assert_eq!(rows_of(&catalog, DATABASES_TABLE).len(), 4);
        assert_eq!(rows_of(&catalog, SCHEMAS_TABLE).len(), 12);
    }

    #[test]
    fn the_database_rows_carry_the_id_the_name_and_the_collation() {
        let catalog = bootstrapped();
        let collation = text(DEFAULT_COLLATION_NAME);
        let rows = rows_of(&catalog, DATABASES_TABLE);
        assert_eq!(rows.len(), 4);
        for (row, (id, name, read_committed, snapshot)) in rows.iter().zip([
            (1, "master", false, 1_u8),
            (2, "tempdb", false, 0_u8),
            (3, "model", false, 0_u8),
            (4, "msdb", false, 1_u8),
        ]) {
            assert_eq!(row[databases_columns::DATABASE_ID], Value::I32(id));
            assert_eq!(row[databases_columns::NAME], text(name));
            assert_eq!(row[databases_columns::COLLATION_NAME], collation);
            assert_eq!(
                row[databases_columns::READ_COMMITTED_SNAPSHOT],
                Value::Bit(read_committed)
            );
            assert_eq!(
                row[databases_columns::SNAPSHOT_ISOLATION_STATE],
                Value::I8(snapshot)
            );
            assert_eq!(row[databases_columns::OWNER_SID], owner_sid_value());
            assert!(matches!(
                row[databases_columns::CREATE_DATE],
                Value::DateTime(_)
            ));
        }
    }

    #[test]
    fn the_schema_rows_of_master_carry_the_published_ids() {
        let catalog = bootstrapped();
        let master: Vec<Vec<Value>> = rows_of(&catalog, SCHEMAS_TABLE)
            .into_iter()
            .filter(|row| row[0] == Value::I32(1))
            .collect();
        assert_eq!(
            master,
            vec![
                vec![Value::I32(1), Value::I32(1), text("dbo"), Value::I32(1)],
                vec![
                    Value::I32(1),
                    Value::I32(3),
                    text("INFORMATION_SCHEMA"),
                    Value::I32(3)
                ],
                vec![Value::I32(1), Value::I32(4), text("sys"), Value::I32(4)],
            ]
        );
    }

    #[test]
    fn the_four_databases_hold_the_three_schemas() {
        let catalog = bootstrapped();
        let pairs: BTreeSet<(i32, String)> = rows_of(&catalog, SCHEMAS_TABLE)
            .into_iter()
            .map(|row| {
                let Value::I32(database) = row[0] else {
                    panic!("database_id is an int");
                };
                let Value::String(name) = &row[2] else {
                    panic!("name is an nvarchar");
                };
                (database, name.text.clone())
            })
            .collect();
        let expected: BTreeSet<(i32, String)> = (1..=4)
            .flat_map(|database| SYSTEM_SCHEMAS.map(|(name, _, _)| (database, name.to_owned())))
            .collect();
        assert_eq!(pairs, expected);
    }

    #[test]
    fn the_tables_of_master_are_the_ones_the_files_describe() {
        // The tables of the files of `views/` come first, then the two tables of the
        // bootstrap; `views/mod.rs` puts the table of objects first.
        let mut expected: Vec<String> = views::internal_tables()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert!(
            expected.contains(&views::sys_core::TYPES_TABLE.to_owned()),
            "{expected:?}"
        );
        expected.extend([DATABASES_TABLE.to_owned(), SCHEMAS_TABLE.to_owned()]);
        let names: Vec<String> = internal_table_defs()
            .unwrap()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn the_two_tables_of_the_bootstrap_carry_the_views_of_sys_core() {
        let tables = internal_table_defs().unwrap();
        for (table, view) in [(DATABASES_TABLE, "databases"), (SCHEMAS_TABLE, "schemas")] {
            let described = tables
                .iter()
                .find(|def| def.name == table)
                .expect("the table is described");
            let names: Vec<&str> = described
                .views
                .iter()
                .map(|installed| installed.name.name.as_str())
                .collect();
            // One view per system database, `sys.<view>` in each of them.
            assert_eq!(names, vec![view; SYSTEM_DATABASES.len()]);
        }
    }

    #[test]
    fn a_row_of_the_wrong_width_is_refused_before_storage_sees_it() {
        let catalog = bootstrapped();
        let master = find_database(&catalog, "master").unwrap().unwrap();
        let def = InternalTableDef {
            name: "vauban_sys_wrong".to_owned(),
            columns: vec![column("only_one", SqlType::Int, false)],
            clustered_key: None,
            rows: vec![Row(vec![Value::I32(1), Value::I32(2)])],
            views: Vec::new(),
        };
        let table = catalog
            .storage
            .create_table(master, &shape_of(&def))
            .unwrap();
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let err = insert_rows(&catalog, &handle, table, &def).unwrap_err();
        catalog.txn.rollback(handle).unwrap();
        assert!(
            matches!(&err, InternalError::Bug(message)
                if message.contains("describes 1 columns and carries a row of 2 values")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_column_order_of_the_internal_tables_is_the_one_the_constants_name() {
        let tables = internal_table_defs().unwrap();
        let databases = tables
            .iter()
            .find(|def| def.name == DATABASES_TABLE)
            .expect("the table of the databases is described");
        let names: Vec<&str> = databases
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names.len(), databases_columns::WIDTH);
        assert_eq!(names[databases_columns::DATABASE_ID], "database_id");
        assert_eq!(names[databases_columns::NAME], "name");
        assert_eq!(names[databases_columns::COLLATION_NAME], "collation_name");
        assert_eq!(
            names[databases_columns::READ_COMMITTED_SNAPSHOT],
            "is_read_committed_snapshot_on"
        );
        assert_eq!(
            names[databases_columns::SNAPSHOT_ISOLATION_STATE],
            "snapshot_isolation_state"
        );

        let schemas = tables
            .iter()
            .find(|def| def.name == SCHEMAS_TABLE)
            .expect("the table of the schemas is described");
        let names: Vec<&str> = schemas
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names.len(), schemas_columns::WIDTH);
        assert_eq!(names[schemas_columns::DATABASE_ID], "database_id");
        assert_eq!(names[schemas_columns::SCHEMA_ID], "schema_id");
        assert_eq!(names[schemas_columns::NAME], "name");
        assert_eq!(names[schemas_columns::PRINCIPAL_ID], "principal_id");
    }

    #[test]
    fn internal_table_id_finds_the_two_tables_of_the_bootstrap() {
        let catalog = bootstrapped();
        let master = find_database(&catalog, "master").unwrap().unwrap();
        let created: Vec<TableId> = catalog
            .storage
            .tables(master)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        // One table created per description, in the order of the list, so the position of a
        // name in the list is the position of its `TableId`; the positions are read from the
        // list rather than written down, a file of `views/` adding its tables in front.
        let names: Vec<String> = internal_table_defs()
            .unwrap()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert_eq!(created.len(), names.len());
        let position = |name: &str| {
            names
                .iter()
                .position(|described| described == name)
                .expect("the table is described")
        };
        for name in [views::sys_core::TYPES_TABLE, DATABASES_TABLE, SCHEMAS_TABLE] {
            assert_eq!(
                internal_table_id(&catalog, name).unwrap(),
                Some(created[position(name)])
            );
        }
        // The name is matched without regard to case, and a name the bootstrap does not
        // describe answers `None` rather than a wrong table.
        assert_eq!(
            internal_table_id(&catalog, "VAUBAN_SYS_SCHEMAS").unwrap(),
            Some(created[position(SCHEMAS_TABLE)])
        );
        assert_eq!(
            internal_table_id(&catalog, "vauban_sys_not_described").unwrap(),
            None
        );
    }

    #[test]
    fn internal_table_id_answers_none_without_a_bootstrap() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog {
            storage,
            txn,
            tables: Default::default(),
        };
        assert_eq!(internal_table_id(&catalog, DATABASES_TABLE).unwrap(), None);
    }

    /// A row of [`DATABASES_TABLE`] read through the constants of [`databases_columns`],
    /// the way `database.rs` reads and writes one.
    fn database_row(catalog: &Catalog, name: &str) -> Option<Vec<Value>> {
        rows_of(catalog, DATABASES_TABLE)
            .into_iter()
            .find(|row| row[databases_columns::NAME] == text(name))
    }

    #[test]
    fn system_databases_take_their_own_values() {
        // One couple per database: `master` 0 / 1 `ON`, `tempdb` 0 / 0 `OFF`, `model`
        // 0 / 0 `OFF`, `msdb` 0 / 1 `ON`.
        let expected = [
            ("master", false, SnapshotIsolationState::On),
            ("tempdb", false, SnapshotIsolationState::Off),
            ("model", false, SnapshotIsolationState::Off),
            ("msdb", false, SnapshotIsolationState::On),
        ];
        assert_eq!(
            expected, SYSTEM_DATABASE_OPTIONS,
            "the constant of this file"
        );
        let catalog = bootstrapped();
        for (name, read_committed_snapshot, snapshot_isolation) in expected {
            let row = database_row(&catalog, name).expect("the row of the system database");
            assert_eq!(
                row[databases_columns::READ_COMMITTED_SNAPSHOT],
                Value::Bit(read_committed_snapshot),
                "is_read_committed_snapshot_on of {name}"
            );
            assert_eq!(
                row[databases_columns::SNAPSHOT_ISOLATION_STATE],
                Value::I8(snapshot_isolation.state()),
                "snapshot_isolation_state of {name}"
            );
        }
        // `master` and `msdb` are the two that differ from `tempdb`, `model` and a fresh
        // database: a single value for the four would have written 0 for them.
        let of = |name: &str| database_options(name).1;
        assert_eq!(of("master"), SnapshotIsolationState::On);
        assert_eq!(of("MSDB"), SnapshotIsolationState::On);
        assert_eq!(of("tempdb"), SnapshotIsolationState::Off);
        assert_eq!(of("model"), SnapshotIsolationState::Off);
        // A name outside the four falls back on the default of `CREATE DATABASE`.
        assert_eq!(database_options("d"), crate::database::NEW_DATABASE_OPTIONS);
    }

    #[test]
    fn a_database_already_there_is_reused_rather_than_created_again() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        // A `master` created before the bootstrap, with the case a client might have used.
        let existing = storage.create_database("MASTER").unwrap();
        let catalog = bootstrap(Arc::clone(&storage), txn).expect("bootstrap");
        assert_eq!(find_database(&catalog, "master").unwrap(), Some(existing));
        let names: Vec<String> = storage
            .databases()
            .unwrap()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(names, vec!["MASTER", "tempdb", "model", "msdb"]);
        // Published with the spelling of `SYSTEM_DATABASES`, not the one storage kept.
        let published = database_row(&catalog, "master").expect("the row of master");
        assert_eq!(published[databases_columns::NAME], text("master"));
        assert_eq!(
            published[databases_columns::DATABASE_ID],
            Value::I32(database_id(existing).unwrap())
        );
        assert!(database_row(&catalog, "MASTER").is_none());
    }
}
