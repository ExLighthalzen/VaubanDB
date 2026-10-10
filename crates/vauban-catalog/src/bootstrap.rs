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
//! 2. in `master`, the registry of the internal tables ([`REGISTRY_TABLE`], section below),
//!    then the internal tables: those [`crate::views::internal_tables`] describes, then the
//!    ones the bootstrap owns, [`DATABASES_TABLE`] and [`SCHEMAS_TABLE`] first, then the three
//!    of `table/reload.rs`, dropped when a file of `views/` already describes a table of that
//!    name;
//! 3. the rows each description carries: one per database in [`DATABASES_TABLE`], one per
//!    (database, schema) pair in [`SCHEMAS_TABLE`], which is 4 and 12 rows on a fresh
//!    `MemoryStorage` (unit test `a_fresh_bootstrap_writes_four_and_twelve_rows`);
//! 4. the five tables of [`MASTER_DBO_SHIPPED_TABLES`], ordinary tables of `master.dbo`
//!    created through [`Catalog::create_table`] once the internal tables above exist, so that
//!    `sys.objects`, `sys.columns` and the other views carry them like any other table of the
//!    instance (`master_dbo_shipped_tables`).
//!
//! The table a view reads arrives with that view (`views/mod.rs`). The two tables above sit
//! here rather than in `views/sys_core.rs` because their rows are not constants — they carry
//! the [`DbId`] that `storage` handed out during this bootstrap — and `views/sys_core.rs`
//! gives them the T-SQL text of `sys.databases` and `sys.schemas`.
//!
//! # Order of the descriptions
//!
//! [`crate::views::internal_tables`] offers the tables the files of `views/` describe, the
//! table of objects first (`views/mod.rs`), and the bootstrap appends its own behind them,
//! a file of `views/` adding its tables in front (unit test
//! `the_tables_of_master_are_the_ones_the_files_describe`). That order says which table is
//! created first on a fresh storage and nothing else: an internal table is found by its name
//! in the registry, not by its place among the tables of `master`.
//!
//! # The registry of the internal tables
//!
//! `storage` keeps the shape of a table and not its name, so the bootstrap writes one row per
//! internal table in [`REGISTRY_TABLE`]: its name and the [`TableId`] `storage` handed out.
//! The registry is a table of its own rather than a description among the others: it is the
//! first table the bootstrap creates in `master`, recognised by its shape ([`registry_shape`]),
//! and [`internal_table_id`] reads it to answer for a name. It carries no view and has no
//! entry in `sys.objects`.
//!
//! [`internal_table_id`] has no transaction to read the registry under. It reads the rows of
//! the committed transactions first ([`registry_snapshot`] with `own` `TxnId(0)`), and for a
//! name it does not find there, the rows each open transaction sees: the bootstrap writes
//! the registry and the rows of the tables it names in one transaction, and reaches those
//! tables by name before that transaction commits. The registry is written by the bootstrap
//! alone, so what an open transaction adds to it is a bootstrap in progress.
//!
//! # Restart
//!
//! A second call on the same storage finds a `master` whose first table is the registry. It
//! creates the internal tables a description names and the registry does not, writes their
//! rows and registers them; the tables already registered keep their [`TableId`] and their
//! rows (unit test `a_table_described_in_front_on_reopen_shifts_no_other_table`). With nothing
//! missing, the call creates nothing and gives back the same [`DbId`]s (integration tests
//! `bootstrap_is_idempotent` and `restart_finds_master`, `tests/bootstrap.rs`). It then
//! rebuilds the tables of the instance from the committed rows of the internal tables
//! (`table/reload.rs`). A `master`
//! that holds tables and no registry in first place is refused with
//! [`InternalError::Bug`]: the bootstrap cannot tell which of those tables is which (unit test
//! `a_master_without_registry_is_refused`).
//!
//! # Transactions
//!
//! The rows are written inside a transaction the catalogue opens itself,
//! `begin(IsolationLevel::ReadCommitted)`, committed at the end; a restart with nothing to
//! create skips it. The `create_database` and `create_table` calls sit outside it: DDL is
//! immediate in `storage` and takes no `TxnId` (`Storage` documentation: DDL is not
//! transactional at the storage level). Compensation actions are not registered for them: a
//! bootstrap that fails is fatal to the instance, and the server turns it into a panic at
//! start-up. On the error path the transaction is rolled back rather than left open, so its
//! rows stay invisible to a later reader.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{DbId, Row, Snapshot, Storage, TableId, TableShape, TxnId};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{
    Date, DateTime, DateTime2, Len, SqlString, SqlType, Time, TypeInfo, Value,
    calendar::days_from_civil, convert,
};

use crate::catalog::Catalog;
use crate::def::{ColumnDef, InternalColumnDef, InternalTableDef, TableDef};
use crate::meta::{QualifiedName, SnapshotIsolationState};
use crate::snapshot::{INTERNAL_TABLE_SCHEMA, system_object_id};
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
/// from 16384 up — are principals `views/sys_principals.rs` serves without a schema of their
/// own, so their numbers stay unused here rather than reassigned. Being constants rather than
/// a counter, these identifiers do not depend on the order the rows were written in, so a
/// restart publishes them unchanged (integration test
/// `bootstrap_creates_three_schemas_in_master`).
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

/// The names of the tables [`install`] creates in `master.dbo`, in creation order.
///
/// mssql-jdbc and pyodbc read them through `sp_tables` and `sp_columns_100` rather than
/// through a table a client owns: `views/sys_tables.rs` reads this list to publish
/// `is_ms_shipped` 1 for a table of `dbo` whose name is one of these five, which is what marks
/// them apart from a table a client creates under the same name in another database.
pub(crate) const MASTER_DBO_SHIPPED_TABLES: [&str; 5] = [
    "spt_fallback_db",
    "spt_fallback_dev",
    "spt_fallback_usg",
    "spt_monitor",
    "MSreplication_options",
];

/// The registry of the internal tables of `master`: one row per internal table, its name and
/// its [`TableId`] (module documentation, section "The registry of the internal tables").
///
/// The bootstrap finds the table by its place and its shape; its first row names the
/// registry itself, so that [`internal_table_id`] answers for this name too.
pub(crate) const REGISTRY_TABLE: &str = "vauban_sys_internal_tables";

/// Where each column of [`REGISTRY_TABLE`] sits in a [`Row`].
pub(crate) mod registry_columns {
    /// `name nvarchar(128)`: the name of the internal table.
    pub(crate) const NAME: usize = 0;
    /// `table_id bigint`: the [`TableId`](vauban_storage::TableId) `storage` handed out.
    pub(crate) const TABLE_ID: usize = 1;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 2;
}

/// `sys.objects.type` of an internal table, the value `views/sys_tables.rs` writes in its rows
/// of `vauban_sys_objects`.
const SYSTEM_TABLE_TYPE: &str = "S ";

/// What builds the descriptions of the internal tables from the system databases and the
/// instant of the bootstrap: [`internal_tables`], or a list a test puts something in front of.
type Declared<'a> =
    &'a dyn Fn(&[(DbId, &str)], &Value) -> Result<Vec<InternalTableDef>, InternalError>;

/// Internal table of the live sessions, read by the `sys.dm_exec_*` views of `catalog`.
pub(crate) const SESSIONS_TABLE: &str = "vauban_sys_sessions";

/// Internal table of the live connections, one row per session.
pub(crate) const CONNECTIONS_TABLE: &str = "vauban_sys_connections";

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

/// Where each column of [`SESSIONS_TABLE`] sits in a [`Row`].
#[allow(dead_code)]
pub(crate) mod sessions_columns {
    /// `session_id smallint`: the SPID of the connection.
    pub(crate) const SESSION_ID: usize = 0;
    /// `login_name nvarchar(128)`.
    pub(crate) const LOGIN_NAME: usize = 1;
    /// `host_name nvarchar(128)`.
    pub(crate) const HOST_NAME: usize = 2;
    /// `program_name nvarchar(128)`.
    pub(crate) const PROGRAM_NAME: usize = 3;
    /// `database_id int`: the current database of the session.
    pub(crate) const DATABASE_ID: usize = 4;
    /// `login_time datetime`.
    pub(crate) const LOGIN_TIME: usize = 5;
    /// `last_request_time datetime`.
    pub(crate) const LAST_REQUEST_TIME: usize = 6;
    /// `status nvarchar(30)`: `running` or `sleeping`.
    pub(crate) const STATUS: usize = 7;
    /// `last_sql_text_len int`: length of the last batch or RPC text.
    pub(crate) const LAST_SQL_TEXT_LEN: usize = 8;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 9;
}

/// Where each column of [`CONNECTIONS_TABLE`] sits in a [`Row`].
#[allow(dead_code)]
pub(crate) mod connections_columns {
    /// `session_id smallint`: the SPID of the connection.
    pub(crate) const SESSION_ID: usize = 0;
    /// `client_net_address nvarchar(48)`.
    pub(crate) const CLIENT_NET_ADDRESS: usize = 1;
    /// `client_tcp_port int`.
    pub(crate) const CLIENT_TCP_PORT: usize = 2;
    /// `protocol_version int`: the negotiated TDS version.
    pub(crate) const PROTOCOL_VERSION: usize = 3;
    /// `encrypt_option nvarchar(40)`.
    pub(crate) const ENCRYPT_OPTION: usize = 4;
    /// `net_transport nvarchar(40)`.
    pub(crate) const NET_TRANSPORT: usize = 5;
    /// `protocol_type nvarchar(40)`.
    pub(crate) const PROTOCOL_TYPE: usize = 6;
    /// `auth_scheme nvarchar(40)`.
    pub(crate) const AUTH_SCHEME: usize = 7;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 8;
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
    open(storage, txn, &internal_tables)
}

/// The body of [`bootstrap`], with the descriptions of the internal tables taken from
/// `declared` rather than from the code: a test adds a table in front of the list there.
///
/// A storage without `master`, or whose `master` holds no table, is a first start: the
/// registry is created, then the internal tables, then the tables of
/// [`MASTER_DBO_SHIPPED_TABLES`]. Otherwise the registry must be the first table of `master`,
/// and the call creates what the descriptions name and the registry does not
/// ([`install_missing`]).
fn open(
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    declared: Declared<'_>,
) -> Result<Catalog, InternalError> {
    let catalog = Catalog {
        storage,
        txn,
        tables: Default::default(),
    };
    let fresh = match find_database(&catalog, SYSTEM_DATABASES[0])? {
        None => true,
        Some(master) => {
            let tables = catalog.storage.tables(master).map_err(internal)?;
            if tables.is_empty() {
                true
            } else if registry_table_of(&tables).is_some() {
                false
            } else {
                return Err(InternalError::Bug(format!(
                    "bootstrap: master holds {} tables and no registry of internal tables",
                    tables.len()
                )));
            }
        }
    };
    let databases = system_databases(&catalog)?;
    if fresh {
        catalog
            .storage
            .create_table(databases[0].0, &registry_shape())
            .map_err(internal)?;
    }
    install_missing(&catalog, &databases, declared, fresh)?;
    if !fresh {
        crate::table::reload::reload(&catalog)?;
    }
    Ok(catalog)
}

/// The four databases of [`SYSTEM_DATABASES`], with the [`DbId`] each one has in `storage`.
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
fn system_databases(catalog: &Catalog) -> Result<Vec<(DbId, &'static str)>, InternalError> {
    let mut databases = Vec::with_capacity(SYSTEM_DATABASES.len());
    for name in SYSTEM_DATABASES {
        let id = match find_database(catalog, name)? {
            Some(id) => id,
            None => catalog.storage.create_database(name).map_err(internal)?,
        };
        databases.push((id, name));
    }
    Ok(databases)
}

/// Creates the internal tables `declared` describes and the registry does not name, then
/// fills them; on a first start (`fresh`), writes the rows of `sys.objects` for the internal
/// tables and creates the tables of [`MASTER_DBO_SHIPPED_TABLES`] as well.
///
/// One transaction (module documentation, section "Transactions"): each missing table is
/// created and gets its row of [`REGISTRY_TABLE`], then the rows of its description, then its
/// row of `sys.objects`, whose `object_id` is the constant of [`system_object_id`].
fn install_missing(
    catalog: &Catalog,
    databases: &[(DbId, &str)],
    declared: Declared<'_>,
    fresh: bool,
) -> Result<(), InternalError> {
    let master = databases[0].0;
    let registered = registered_internal_tables(catalog)?;
    let missing: Vec<InternalTableDef> = declared(databases, &placeholder_instant())?
        .into_iter()
        .filter(|def| {
            !registered
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(&def.name))
        })
        .collect();
    if missing.is_empty() && !fresh {
        return Ok(());
    }
    in_transaction(catalog, |txn| {
        let instant = transaction_datetime(catalog, txn).map_err(internal)?;
        let registry = registry_table(catalog)?.ok_or_else(|| {
            InternalError::Bug("bootstrap: the registry of internal tables is missing".to_owned())
        })?;
        if fresh {
            catalog
                .storage
                .insert(txn.id, registry, &registry_row(REGISTRY_TABLE, registry))
                .map_err(internal)?;
        }
        let filled: Vec<InternalTableDef> = declared(databases, &instant)?
            .into_iter()
            .filter(|def| missing.iter().any(|new| new.name == def.name))
            .collect();
        for def in &filled {
            let table = catalog
                .storage
                .create_table(master, &shape_of(def))
                .map_err(internal)?;
            catalog
                .storage
                .insert(txn.id, registry, &registry_row(&def.name, table))
                .map_err(internal)?;
            insert_rows(catalog, txn, table, def)?;
        }
        if fresh {
            views::sys_tables::write_internal_object_rows(catalog, txn, master, instant.clone())
                .map_err(internal)?;
        } else {
            insert_internal_object_rows(catalog, txn, master, &filled, &instant)?;
        }
        stamp_internal_object_rows(catalog, txn, master)?;
        if fresh {
            for def in master_dbo_shipped_tables() {
                catalog.create_table(txn, &def).map_err(internal)?;
            }
        }
        Ok(())
    })
}

/// Runs `body` in a transaction, committed when `body` succeeds and rolled back when it
/// fails.
fn in_transaction<T>(
    catalog: &Catalog,
    body: impl FnOnce(&TxnHandle) -> Result<T, InternalError>,
) -> Result<T, InternalError> {
    let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
    match body(&handle) {
        Ok(value) => {
            catalog.txn.commit(handle).map_err(internal)?;
            Ok(value)
        }
        Err(err) => {
            catalog.txn.rollback(handle).map_err(internal)?;
            Err(err)
        }
    }
}

/// Writes the rows of `sys.objects` of internal tables registered on a restart, the ones a
/// first start writes through `views::sys_tables::write_internal_object_rows`.
fn insert_internal_object_rows(
    catalog: &Catalog,
    txn: &TxnHandle,
    master: DbId,
    defs: &[InternalTableDef],
    instant: &Value,
) -> Result<(), InternalError> {
    let rows =
        views::sys_tables::internal_table_object_rows(database_id(master)?, defs, instant, instant)
            .map_err(internal)?;
    let table = objects_table(catalog)?;
    for row in rows {
        catalog
            .storage
            .insert(txn.id, table, &row)
            .map_err(internal)?;
    }
    Ok(())
}

/// Gives each row of an internal table in `vauban_sys_objects` the `object_id` of
/// [`system_object_id`], which the rows of `views/sys_tables.rs` number by their place in
/// the list of descriptions.
///
/// A row is rewritten when it is a system table of `master.dbo` whose name has a constant and
/// whose `object_id` differs from it; the other rows are left as they are (unit test
/// `the_rows_of_sys_objects_carry_the_constant_identifiers`).
fn stamp_internal_object_rows(
    catalog: &Catalog,
    txn: &TxnHandle,
    master: DbId,
) -> Result<(), InternalError> {
    use views::sys_tables::objects_columns;

    let table = objects_table(catalog)?;
    let master = Value::I32(database_id(master)?);
    let dbo = Value::I32(schema_id_of(INTERNAL_TABLE_SCHEMA));
    let snapshot = catalog.txn.statement_snapshot(txn);
    let mut stamped = Vec::new();
    for row in catalog.storage.scan(&snapshot, table).map_err(internal)? {
        let (id, mut values) = row.map_err(internal)?;
        if values.0.get(objects_columns::DATABASE_ID) != Some(&master)
            || values.0.get(objects_columns::SCHEMA_ID) != Some(&dbo)
            || values.0.get(objects_columns::TYPE) != Some(&text(SYSTEM_TABLE_TYPE))
        {
            continue;
        }
        let Some(Value::String(name)) = values.0.get(objects_columns::NAME) else {
            continue;
        };
        let Some(object_id) = system_object_id(INTERNAL_TABLE_SCHEMA, &name.text) else {
            continue;
        };
        let constant = Value::I32(object_id.0);
        if values.0.get(objects_columns::OBJECT_ID) != Some(&constant) {
            values.0[objects_columns::OBJECT_ID] = constant;
            stamped.push((id, values));
        }
    }
    for (id, values) in stamped {
        catalog
            .storage
            .update(txn.id, table, id, &values)
            .map_err(internal)?;
    }
    Ok(())
}

/// The [`TableId`] of `vauban_sys_objects`, an error when the registry does not name it.
fn objects_table(catalog: &Catalog) -> Result<TableId, InternalError> {
    let name = views::sys_tables::OBJECTS_TABLE;
    internal_table_id(catalog, name)?
        .ok_or_else(|| InternalError::Bug(format!("bootstrap: internal table {name} is missing")))
}

/// The `schema_id` [`SYSTEM_SCHEMAS`] gives the schema `name`, `0` for a name it does not
/// list.
fn schema_id_of(name: &str) -> i32 {
    SYSTEM_SCHEMAS
        .iter()
        .find(|(schema, _, _)| schema.eq_ignore_ascii_case(name))
        .map_or(0, |&(_, id, _)| id)
}

/// The name of the schema [`SYSTEM_SCHEMAS`] numbers `id`.
fn schema_name_of(id: i32) -> Option<&'static str> {
    SYSTEM_SCHEMAS
        .iter()
        .find(|&&(_, schema_id, _)| schema_id == id)
        .map(|&(name, _, _)| name)
}

/// The definitions of [`MASTER_DBO_SHIPPED_TABLES`], in the same order: heaps, no constraint,
/// created through the ordinary [`Catalog::create_table`] path so that `sys.objects`,
/// `sys.columns`, `sys.all_objects`, `sys.all_columns` and `INFORMATION_SCHEMA` carry them at
/// once (`sys_rows.rs`, `write`), the way any other table of the instance is published.
///
/// `spt_fallback_db` carries the eight columns clients read through it; the other four carry
/// what `sp_columns_100` answers about them on SQL Server 2022.
fn master_dbo_shipped_tables() -> Vec<TableDef> {
    fn name(table: &str) -> QualifiedName {
        QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: table.to_owned(),
        }
    }
    fn col(name: &str, ty: SqlType, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, nullable),
            default: None,
            identity: None,
            computed: None,
        }
    }
    fn varchar(len: u16) -> SqlType {
        SqlType::VarChar(Len::Fixed(len))
    }
    vec![
        TableDef {
            name: name(MASTER_DBO_SHIPPED_TABLES[0]),
            columns: vec![
                col("xserver_name", varchar(30), false),
                col("xdttm_ins", SqlType::DateTime, false),
                col("xdttm_last_ins_upd", SqlType::DateTime, false),
                col("xfallback_dbid", SqlType::SmallInt, true),
                col("name", varchar(30), false),
                col("dbid", SqlType::SmallInt, false),
                col("status", SqlType::SmallInt, false),
                col("version", SqlType::SmallInt, false),
            ],
            constraints: Vec::new(),
        },
        TableDef {
            name: name(MASTER_DBO_SHIPPED_TABLES[1]),
            columns: vec![
                col("xserver_name", varchar(30), false),
                col("xdttm_ins", SqlType::DateTime, false),
                col("xdttm_last_ins_upd", SqlType::DateTime, false),
                col("xfallback_low", SqlType::Int, true),
                col("xfallback_drive", SqlType::Char(Len::Fixed(2)), true),
                col("low", SqlType::Int, false),
                col("high", SqlType::Int, false),
                col("status", SqlType::SmallInt, false),
                col("name", varchar(30), false),
                col("phyname", varchar(127), false),
            ],
            constraints: Vec::new(),
        },
        TableDef {
            name: name(MASTER_DBO_SHIPPED_TABLES[2]),
            columns: vec![
                col("xserver_name", varchar(30), false),
                col("xdttm_ins", SqlType::DateTime, false),
                col("xdttm_last_ins_upd", SqlType::DateTime, false),
                col("xfallback_vstart", SqlType::Int, true),
                col("dbid", SqlType::SmallInt, false),
                col("segmap", SqlType::Int, false),
                col("lstart", SqlType::Int, false),
                col("sizepg", SqlType::Int, false),
                col("vstart", SqlType::Int, false),
            ],
            constraints: Vec::new(),
        },
        TableDef {
            name: name(MASTER_DBO_SHIPPED_TABLES[3]),
            columns: vec![
                col("lastrun", SqlType::DateTime, false),
                col("cpu_busy", SqlType::Int, false),
                col("io_busy", SqlType::Int, false),
                col("idle", SqlType::Int, false),
                col("pack_received", SqlType::Int, false),
                col("pack_sent", SqlType::Int, false),
                col("connections", SqlType::Int, false),
                col("pack_errors", SqlType::Int, false),
                col("total_read", SqlType::Int, false),
                col("total_write", SqlType::Int, false),
                col("total_errors", SqlType::Int, false),
            ],
            constraints: Vec::new(),
        },
        TableDef {
            name: name(MASTER_DBO_SHIPPED_TABLES[4]),
            columns: vec![
                col("optname", SYSNAME, false),
                col("value", SqlType::Bit, false),
                col("major_version", SqlType::Int, false),
                col("minor_version", SqlType::Int, false),
                col("revision", SqlType::Int, false),
                col("install_failures", SqlType::Int, false),
            ],
            constraints: Vec::new(),
        },
    ]
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
    let own_tables = bootstrap_tables(databases, instant)?
        .into_iter()
        .chain(crate::table::reload::internal_tables());
    for own in own_tables {
        if !tables
            .iter()
            .any(|table| table.name.eq_ignore_ascii_case(&own.name))
        {
            tables.push(own);
        }
    }
    stamp_system_view_rows(&mut tables);
    Ok(tables)
}

/// Gives each row of a system view in the description of `vauban_sys_all_objects` the
/// `object_id` of [`system_object_id`], which `views/sys_extra.rs` numbers by rank.
///
/// The row is found by its `schema_id` and its name; a row whose pair has no constant keeps
/// the number it came with (unit test `the_view_rows_of_sys_all_objects_carry_the_constants`).
fn stamp_system_view_rows(tables: &mut [InternalTableDef]) {
    use views::sys_extra::{ALL_OBJECTS_TABLE, objects_columns};

    for table in tables
        .iter_mut()
        .filter(|table| table.name.eq_ignore_ascii_case(ALL_OBJECTS_TABLE))
    {
        for row in &mut table.rows {
            let Some(&Value::I32(schema_id)) = row.0.get(objects_columns::SCHEMA_ID) else {
                continue;
            };
            let Some(schema) = schema_name_of(schema_id) else {
                continue;
            };
            let Some(Value::String(name)) = row.0.get(objects_columns::NAME) else {
                continue;
            };
            let Some(id) = system_object_id(schema, &name.text) else {
                continue;
            };
            if let Some(slot) = row.0.get_mut(objects_columns::OBJECT_ID) {
                *slot = Value::I32(id.0);
            }
        }
    }
}

/// The descriptions of the internal tables of `master`, in the order [`internal_tables`]
/// creates them, with the rows of the two tables of the bootstrap left empty.
///
/// `snapshot.rs` turns each description into a [`TableMeta`](crate::TableMeta) and reads the
/// [`TableId`] of the table from the registry, by the name of the description
/// ([`registered_internal_tables`]). The rows of the two tables of the bootstrap are empty
/// because they are built from the [`DbId`]s of a bootstrap and `[]` is passed here; a
/// reader of this function wants the names and the columns. The order is the one the unit
/// test `the_tables_of_master_are_the_ones_the_files_describe` states, and the pairing by
/// name is held by `snapshot.rs`, unit test
/// `the_storage_id_of_an_internal_table_is_the_one_internal_table_id_finds`.
///
/// # Errors
///
/// The error [`internal_tables`] answers with.
pub(crate) fn internal_table_defs() -> Result<Vec<InternalTableDef>, InternalError> {
    internal_tables(&[], &placeholder_instant())
}

/// The `datetime` the descriptions are built with when their names and columns are what is read.
fn placeholder_instant() -> Value {
    Value::DateTime(DateTime {
        days: 0,
        ticks_300th: 0,
    })
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
        InternalTableDef {
            name: SESSIONS_TABLE.to_owned(),
            columns: vec![
                column("session_id", SqlType::SmallInt, false),
                column("login_name", SYSNAME, false),
                column("host_name", SYSNAME, false),
                column("program_name", SYSNAME, false),
                column("database_id", SqlType::Int, false),
                column("login_time", SqlType::DateTime, false),
                column("last_request_time", SqlType::DateTime, false),
                column("status", SqlType::NVarChar(Len::Fixed(30)), false),
                column("last_sql_text_len", SqlType::Int, false),
            ],
            clustered_key: None,
            rows: Vec::new(),
            views: Vec::new(),
        },
        InternalTableDef {
            name: CONNECTIONS_TABLE.to_owned(),
            columns: vec![
                column("session_id", SqlType::SmallInt, false),
                column(
                    "client_net_address",
                    SqlType::NVarChar(Len::Fixed(48)),
                    false,
                ),
                column("client_tcp_port", SqlType::Int, false),
                column("protocol_version", SqlType::Int, false),
                column("encrypt_option", SqlType::NVarChar(Len::Fixed(40)), false),
                column("net_transport", SqlType::NVarChar(Len::Fixed(40)), false),
                column("protocol_type", SqlType::NVarChar(Len::Fixed(40)), false),
                column("auth_scheme", SqlType::NVarChar(Len::Fixed(40)), false),
            ],
            clustered_key: None,
            rows: Vec::new(),
            views: Vec::new(),
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

/// The shape of [`REGISTRY_TABLE`]: `name nvarchar(128)`, `table_id bigint`, a heap.
pub(crate) fn registry_shape() -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SYSNAME, false),
            TypeInfo::new(SqlType::BigInt, false),
        ],
        clustered_key: None,
    }
}

/// The row of [`REGISTRY_TABLE`] that says the internal table `name` is `table`.
pub(crate) fn registry_row(name: &str, table: TableId) -> Row {
    let mut row = vec![Value::Null; registry_columns::WIDTH];
    row[registry_columns::NAME] = text(name);
    row[registry_columns::TABLE_ID] = Value::I64(i64::from(table.0));
    Row(row)
}

/// The registry among the tables of `master`: the first one, when it has the shape of
/// [`registry_shape`].
fn registry_table_of(tables: &[(TableId, TableShape)]) -> Option<TableId> {
    tables
        .first()
        .filter(|(_, shape)| *shape == registry_shape())
        .map(|&(id, _)| id)
}

/// The [`TableId`] of [`REGISTRY_TABLE`], `None` when this storage holds no `master` or a
/// `master` without registry.
fn registry_table(catalog: &Catalog) -> Result<Option<TableId>, InternalError> {
    let Some(master) = find_database(catalog, SYSTEM_DATABASES[0])? else {
        return Ok(None);
    };
    let tables = catalog.storage.tables(master).map_err(internal)?;
    Ok(registry_table_of(&tables))
}

/// A snapshot that sees the rows of the transactions committed when it is built, and the
/// rows `own` wrote.
///
/// The transactions [`TransactionManager::active_sessions`] lists, `own` aside, are the ones
/// it does not see; `xmax` is the largest identifier, so a transaction that commits later and
/// was not listed is seen too, which the registry tolerates since its rows are written once
/// and left unchanged afterwards. `TxnId(0)`, an identifier `begin` does not hand out, reads
/// the committed rows alone.
pub(crate) fn registry_snapshot(catalog: &Catalog, own: TxnId) -> Snapshot {
    let mut active: Vec<TxnId> = catalog
        .txn
        .active_sessions()
        .into_iter()
        .map(|info| info.id)
        .filter(|&id| id != own)
        .collect();
    active.sort();
    let xmax = TxnId(u64::MAX);
    Snapshot {
        xmin: active.first().copied().unwrap_or(xmax),
        xmax,
        active,
        own,
    }
}

/// The `(name, TableId)` pairs of the registry `registry` as `snapshot` sees them, in the
/// order of the scan.
///
/// A row whose name is not a string or whose `table_id` does not fit a [`TableId`] is skipped
/// rather than read as another table.
fn registry_rows(
    catalog: &Catalog,
    registry: TableId,
    snapshot: &Snapshot,
) -> Result<Vec<(String, TableId)>, InternalError> {
    let mut registered = Vec::new();
    for row in catalog.storage.scan(snapshot, registry).map_err(internal)? {
        let (_, values) = row.map_err(internal)?;
        let Some(Value::String(name)) = values.0.get(registry_columns::NAME) else {
            continue;
        };
        let Some(&Value::I64(id)) = values.0.get(registry_columns::TABLE_ID) else {
            continue;
        };
        let Ok(id) = u32::try_from(id) else {
            continue;
        };
        registered.push((name.text.clone(), TableId(id)));
    }
    Ok(registered)
}

/// The `(name, TableId)` pairs the committed rows of the registry hold, in the order of the
/// scan; an empty list for a storage without registry.
///
/// # Errors
///
/// [`InternalError`] when a call to `storage` fails.
pub(crate) fn registered_internal_tables(
    catalog: &Catalog,
) -> Result<Vec<(String, TableId)>, InternalError> {
    let Some(registry) = registry_table(catalog)? else {
        return Ok(Vec::new());
    };
    registry_rows(catalog, registry, &registry_snapshot(catalog, TxnId(0)))
}

/// The [`TableId`] of the internal table called `name`, `None` when the registry names no
/// such table.
///
/// How the rest of the crate reaches an internal table: `storage` keeps the shape of a table
/// and not its name, so the name is read from [`REGISTRY_TABLE`], committed rows first, then
/// the rows of each open transaction (module documentation, section "The registry of the
/// internal tables"). The name is matched without regard to case. `database.rs` writes the row of a user database into [`DATABASES_TABLE`] through
/// this function and [`databases_columns`] (unit test
/// `internal_table_id_finds_the_two_tables_of_the_bootstrap`).
///
/// # Errors
///
/// [`InternalError`] when a call to `storage` fails.
pub(crate) fn internal_table_id(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<TableId>, InternalError> {
    let Some(registry) = registry_table(catalog)? else {
        return Ok(None);
    };
    let readers = std::iter::once(TxnId(0)).chain(
        catalog
            .txn
            .active_sessions()
            .into_iter()
            .map(|info| info.id),
    );
    for own in readers {
        let found = registry_rows(catalog, registry, &registry_snapshot(catalog, own))?
            .into_iter()
            .find(|(registered, _)| registered.eq_ignore_ascii_case(name))
            .map(|(_, id)| id);
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
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

    /// The rows of the internal table of that name, sorted by their debug form; the table is
    /// the one the registry names.
    fn rows_of(catalog: &Catalog, name: &str) -> Vec<Vec<Value>> {
        let table = internal_table_id(catalog, name)
            .unwrap()
            .expect("the name is one of the internal tables");
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
        // The tables of the files of `views/` come first, then the tables of the bootstrap
        // and the three of `table/reload.rs`; `views/mod.rs` puts the table of objects first.
        let mut expected: Vec<String> = views::internal_tables()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert!(
            expected.contains(&views::sys_core::TYPES_TABLE.to_owned()),
            "{expected:?}"
        );
        expected.extend([
            DATABASES_TABLE.to_owned(),
            SCHEMAS_TABLE.to_owned(),
            SESSIONS_TABLE.to_owned(),
            CONNECTIONS_TABLE.to_owned(),
            crate::table::reload::STORAGE_TABLE.to_owned(),
            crate::table::reload::COLUMNS_TABLE.to_owned(),
            crate::table::reload::HIGH_WATER_TABLE.to_owned(),
        ]);
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

        let sessions = tables
            .iter()
            .find(|def| def.name == SESSIONS_TABLE)
            .expect("the table of the sessions is described");
        let names: Vec<&str> = sessions
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names.len(), sessions_columns::WIDTH);
        assert_eq!(names[sessions_columns::SESSION_ID], "session_id");
        assert_eq!(names[sessions_columns::LOGIN_NAME], "login_name");
        assert_eq!(names[sessions_columns::STATUS], "status");

        let connections = tables
            .iter()
            .find(|def| def.name == CONNECTIONS_TABLE)
            .expect("the table of the connections is described");
        let names: Vec<&str> = connections
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names.len(), connections_columns::WIDTH);
        assert_eq!(names[connections_columns::SESSION_ID], "session_id");
        assert_eq!(
            names[connections_columns::CLIENT_NET_ADDRESS],
            "client_net_address"
        );
    }

    #[test]
    fn internal_table_id_finds_the_two_tables_of_the_bootstrap() {
        let catalog = bootstrapped();
        let master = find_database(&catalog, "master").unwrap().unwrap();
        let created: Vec<(TableId, TableShape)> = catalog.storage.tables(master).unwrap();
        // The registry, one table per description, then the five tables of
        // `MASTER_DBO_SHIPPED_TABLES`, outside the list.
        let described = internal_table_defs().unwrap();
        assert_eq!(
            created.len(),
            1 + described.len() + MASTER_DBO_SHIPPED_TABLES.len()
        );
        assert_eq!(created[0].1, registry_shape());
        assert_eq!(
            internal_table_id(&catalog, REGISTRY_TABLE).unwrap(),
            Some(created[0].0),
            "the registry names itself"
        );
        // Each description is found by its name, on a table of its own shape.
        let mut found = BTreeSet::new();
        for def in &described {
            let table = internal_table_id(&catalog, &def.name)
                .unwrap()
                .unwrap_or_else(|| panic!("{} is registered", def.name));
            let shape = created
                .iter()
                .find(|(id, _)| *id == table)
                .map(|(_, shape)| shape.clone());
            assert_eq!(shape, Some(shape_of(def)), "{}", def.name);
            assert!(found.insert(table), "{} shares a table", def.name);
        }
        // The name is matched without regard to case, and a name the bootstrap does not
        // describe answers `None` rather than a wrong table.
        assert_eq!(
            internal_table_id(&catalog, "VAUBAN_SYS_SCHEMAS").unwrap(),
            internal_table_id(&catalog, SCHEMAS_TABLE).unwrap()
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

    #[test]
    fn the_five_shipped_tables_are_defined_in_dbo_of_master() {
        let defs = master_dbo_shipped_tables();
        assert_eq!(defs.len(), MASTER_DBO_SHIPPED_TABLES.len());
        for (def, name) in defs.iter().zip(MASTER_DBO_SHIPPED_TABLES) {
            assert_eq!(def.name.database, "master");
            assert_eq!(def.name.schema, "dbo");
            assert_eq!(def.name.name, name);
            assert!(def.constraints.is_empty(), "{name} is a heap");
            assert!(!def.columns.is_empty(), "{name} has columns");
        }
        // The column counts `sp_columns_100` answers for each of the five tables.
        let widths: Vec<usize> = defs.iter().map(|def| def.columns.len()).collect();
        assert_eq!(widths, vec![8, 10, 9, 11, 6]);
    }

    #[test]
    fn the_five_shipped_tables_exist_in_master_after_a_bootstrap() {
        let catalog = bootstrapped();
        let master = find_database(&catalog, "master").unwrap().unwrap();
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        for name in MASTER_DBO_SHIPPED_TABLES {
            let found = catalog
                .snapshot(&handle)
                .resolve_object("master", Some("dbo"), name, "dbo")
                .is_some();
            assert!(found, "{name} is resolved in the catalogue");
        }
        catalog.txn.commit(handle).unwrap();
        // The names are ordinary tables of `master`, on top of the registry and the internal
        // tables.
        let tables = catalog.storage.tables(master).unwrap();
        assert_eq!(
            tables.len(),
            1 + internal_table_defs().unwrap().len() + MASTER_DBO_SHIPPED_TABLES.len()
        );
    }

    /// A directory of its own under the temporary directory, removed when dropped.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "vauban-catalog-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create the directory of the instance");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The on-disk storage of `dir` and a transaction manager over it.
    fn on_disk(dir: &TempDir) -> (Arc<dyn Storage>, Arc<TransactionManager>) {
        let storage: Arc<dyn Storage> = Arc::new(
            vauban_storage::DiskStorage::open(&dir.0, vauban_storage::DiskOptions::default())
                .expect("open the instance"),
        );
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        (storage, txn)
    }

    /// The rows of `table`, in the order of their debug form.
    fn scanned(catalog: &Catalog, table: TableId) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = catalog
            .storage
            .scan(&registry_snapshot(catalog, TxnId(0)), table)
            .unwrap()
            .map(|row| row.unwrap().1.0)
            .collect();
        rows.sort_by_key(|row| format!("{row:?}"));
        rows
    }

    /// The rows of a description, padded to its width as [`insert_rows`] writes them, in the
    /// order of their debug form.
    fn declared_rows(def: &InternalTableDef) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = def
            .rows
            .iter()
            .map(|row| {
                let mut values = row.0.clone();
                values.resize(def.columns.len(), Value::Null);
                values
            })
            .collect();
        rows.sort_by_key(|row| format!("{row:?}"));
        rows
    }

    /// A description put in front of the others on a reopen: one column, one row that says
    /// which table it is.
    fn described_in_front() -> InternalTableDef {
        InternalTableDef {
            name: "vauban_sys_described_in_front".to_owned(),
            columns: vec![column("marker", SYSNAME, false)],
            clustered_key: None,
            rows: vec![Row(vec![text("the table described in front")])],
            views: Vec::new(),
        }
    }

    /// [`internal_tables`] with [`described_in_front`] at the head of the list.
    fn with_a_table_in_front(
        databases: &[(DbId, &str)],
        instant: &Value,
    ) -> Result<Vec<InternalTableDef>, InternalError> {
        let mut tables = vec![described_in_front()];
        tables.extend(internal_tables(databases, instant)?);
        Ok(tables)
    }

    #[test]
    fn a_table_described_in_front_on_reopen_shifts_no_other_table() {
        let dir = TempDir::new("in-front");
        let registered_before = {
            let (storage, txn) = on_disk(&dir);
            let catalog = bootstrap(storage, txn).expect("first start");
            registered_internal_tables(&catalog).unwrap()
        };
        let (storage, txn) = on_disk(&dir);
        let catalog = open(storage, txn, &with_a_table_in_front).expect("reopen");

        // The tables registered before keep their `TableId`, and the new one is added.
        let registered_after = registered_internal_tables(&catalog).unwrap();
        for pair in &registered_before {
            assert!(registered_after.contains(pair), "{pair:?} moved");
        }
        assert_eq!(registered_after.len(), registered_before.len() + 1);

        // Each internal table reads its own rows: the rows of its description are among the
        // rows it holds, for each description whose rows carry no instant of the bootstrap.
        // The tables of `sys.objects` and `sys.all_objects` also hold the rows a start writes
        // for the objects it creates.
        let master = find_database(&catalog, "master").unwrap().unwrap();
        let shapes = catalog.storage.tables(master).unwrap();
        let declared = with_a_table_in_front(
            &SYSTEM_DATABASES
                .iter()
                .map(|&name| (find_database(&catalog, name).unwrap().unwrap(), name))
                .collect::<Vec<_>>(),
            &placeholder_instant(),
        )
        .unwrap();
        let mut compared = 0;
        for def in &declared {
            let table = internal_table_id(&catalog, &def.name)
                .unwrap()
                .unwrap_or_else(|| panic!("{} is registered", def.name));
            let shape = shapes
                .iter()
                .find(|(id, _)| *id == table)
                .map(|(_, shape)| shape.clone());
            assert_eq!(shape, Some(shape_of(def)), "{}", def.name);
            let rows = scanned(&catalog, table);
            let dated = def
                .rows
                .iter()
                .flat_map(|row| row.0.iter())
                .any(|value| matches!(value, Value::DateTime(_)));
            if !dated && !def.rows.is_empty() {
                for row in declared_rows(def) {
                    assert!(rows.contains(&row), "{}: {row:?} is missing", def.name);
                }
                compared += 1;
            }
        }
        assert!(compared >= 5, "{compared} tables compared");
        assert_eq!(
            scanned(
                &catalog,
                internal_table_id(&catalog, &described_in_front().name)
                    .unwrap()
                    .unwrap()
            ),
            vec![vec![text("the table described in front")]]
        );
        // The names the table of the databases holds are the four system databases.
        let names: BTreeSet<String> = rows_of(&catalog, DATABASES_TABLE)
            .into_iter()
            .map(|row| format!("{:?}", row[databases_columns::NAME]))
            .collect();
        let expected: BTreeSet<String> = SYSTEM_DATABASES
            .iter()
            .map(|name| format!("{:?}", text(name)))
            .collect();
        assert_eq!(names, expected);

        // A third start with the list of the code creates nothing.
        let count = catalog.storage.tables(master).unwrap().len();
        drop(catalog);
        let (storage, txn) = on_disk(&dir);
        let catalog = bootstrap(storage, txn).expect("third start");
        assert_eq!(catalog.storage.tables(master).unwrap().len(), count);
    }

    #[test]
    fn object_id_of_sys_tables_holds_across_a_restart() {
        let dir = TempDir::new("object-id");
        let resolve = |catalog: &Catalog, schema: &str, name: &str| {
            let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
            let id = catalog
                .snapshot(&handle)
                .resolve_object("master", Some(schema), name, "dbo")
                .map(|object| object.id);
            catalog.txn.commit(handle).unwrap();
            id
        };
        let before = {
            let (storage, txn) = on_disk(&dir);
            let catalog = bootstrap(storage, txn).expect("first start");
            (
                resolve(&catalog, "sys", "tables"),
                resolve(&catalog, "dbo", DATABASES_TABLE),
            )
        };
        let (storage, txn) = on_disk(&dir);
        let catalog = open(storage, txn, &with_a_table_in_front).expect("reopen");
        let after = (
            resolve(&catalog, "sys", "tables"),
            resolve(&catalog, "dbo", DATABASES_TABLE),
        );
        assert_eq!(after, before);
        assert_eq!(after.0, system_object_id("sys", "tables"));
        assert_eq!(after.1, system_object_id("dbo", DATABASES_TABLE));
        assert!(after.0.is_some() && after.1.is_some());
    }

    #[test]
    fn a_master_without_registry_is_refused() {
        // A `master` whose first table is not the registry: the bootstrap cannot tell its
        // tables apart and stops, rather than reading one table as another.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let master = storage.create_database("master").unwrap();
        let shape = TableShape {
            columns: vec![TypeInfo::new(SqlType::Int, false)],
            clustered_key: None,
        };
        storage.create_table(master, &shape).unwrap();
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let Err(err) = bootstrap(storage, txn) else {
            panic!("a master without registry is accepted");
        };
        assert!(
            matches!(&err, InternalError::Bug(message)
                if message.contains("no registry of internal tables")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_rows_of_sys_objects_carry_the_constant_identifiers() {
        use views::sys_tables::objects_columns;

        let catalog = bootstrapped();
        let rows = rows_of(&catalog, views::sys_tables::OBJECTS_TABLE);
        let mut checked = 0;
        for row in &rows {
            if row[objects_columns::TYPE] != text(SYSTEM_TABLE_TYPE) {
                continue;
            }
            let Value::String(name) = &row[objects_columns::NAME] else {
                panic!("name is an nvarchar");
            };
            let expected = system_object_id(INTERNAL_TABLE_SCHEMA, &name.text)
                .unwrap_or_else(|| panic!("{} has a constant", name.text));
            assert_eq!(row[objects_columns::OBJECT_ID], Value::I32(expected.0));
            checked += 1;
        }
        assert_eq!(checked, internal_table_defs().unwrap().len());

        // A row numbered otherwise is rewritten to its constant.
        let table = objects_table(&catalog).unwrap();
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = catalog.txn.statement_snapshot(&handle);
        let (id, mut values) = catalog
            .storage
            .scan(&snapshot, table)
            .unwrap()
            .map(|row| row.unwrap())
            .find(|(_, values)| values.0[objects_columns::NAME] == text(DATABASES_TABLE))
            .expect("the row of the table of the databases");
        values.0[objects_columns::OBJECT_ID] = Value::I32(100_000);
        catalog
            .storage
            .update(handle.id, table, id, &values)
            .unwrap();
        let master = find_database(&catalog, "master").unwrap().unwrap();
        stamp_internal_object_rows(&catalog, &handle, master).unwrap();
        let snapshot = catalog.txn.statement_snapshot(&handle);
        let stamped = catalog
            .storage
            .get(&snapshot, table, id)
            .unwrap()
            .expect("the row");
        catalog.txn.rollback(handle).unwrap();
        assert_eq!(
            Some(stamped.0[objects_columns::OBJECT_ID].clone()),
            system_object_id(INTERNAL_TABLE_SCHEMA, DATABASES_TABLE).map(|id| Value::I32(id.0))
        );
    }

    #[test]
    fn the_view_rows_of_sys_all_objects_carry_the_constants() {
        use views::sys_extra::objects_columns;

        let mut tables = internal_table_defs().unwrap();
        let all_objects = |tables: &[InternalTableDef]| -> InternalTableDef {
            tables
                .iter()
                .find(|def| def.name == views::sys_extra::ALL_OBJECTS_TABLE)
                .cloned()
                .expect("the table of sys.all_objects is described")
        };
        let described = all_objects(&tables);
        assert!(!described.rows.is_empty());
        for row in &described.rows {
            let Value::I32(schema_id) = row.0[objects_columns::SCHEMA_ID] else {
                panic!("schema_id is an int");
            };
            let Value::String(name) = &row.0[objects_columns::NAME] else {
                panic!("name is an nvarchar");
            };
            let schema = schema_name_of(schema_id).expect("a schema of the bootstrap");
            assert_eq!(
                Some(row.0[objects_columns::OBJECT_ID].clone()),
                system_object_id(schema, &name.text).map(|id| Value::I32(id.0)),
                "{schema}.{}",
                name.text
            );
        }
        // A row numbered otherwise is given its constant; a row of a pair the table does not
        // list keeps its number.
        let position = tables
            .iter()
            .position(|def| def.name == views::sys_extra::ALL_OBJECTS_TABLE)
            .unwrap();
        tables[position].rows[0].0[objects_columns::OBJECT_ID] = Value::I32(-1_000);
        let mut unlisted = tables[position].rows[0].clone();
        unlisted.0[objects_columns::NAME] = text("not_a_system_view");
        unlisted.0[objects_columns::OBJECT_ID] = Value::I32(-2_000);
        tables[position].rows.push(unlisted);
        stamp_system_view_rows(&mut tables);
        let stamped = all_objects(&tables);
        assert_eq!(stamped.rows[0], described.rows[0]);
        assert_eq!(
            stamped
                .rows
                .last()
                .map(|row| row.0[objects_columns::OBJECT_ID].clone()),
            Some(Value::I32(-2_000))
        );
    }
}
