//! `sys.objects`, `sys.tables` and `sys.columns`.
//!
//! # The shape of the views
//!
//! The column list of each of the three views — names, order, types — is the one SQL Server
//! 2022 publishes: 12, 48 and 40 columns. The unit tests below freeze the three vectors of
//! names and compare them with the text this file builds
//! (`sys_objects_columns_match_the_published_list` and its two neighbours).
//!
//! # Two internal tables for three views
//!
//! A view is a `SELECT` over one denormalised internal table, without a join.
//! [`OBJECTS_TABLE`] holds one row per schema-scoped object and carries `sys.objects` and
//! `sys.tables`, which read the same rows — the second one keeps the user tables
//! (`WHERE type = 'U '`) and publishes 36 more columns; [`COLUMNS_TABLE`] holds one row per
//! column and carries `sys.columns`. The internal tables live in `master` while the three
//! views are per-database, so each definition filters on `database_id = DB_ID()`, as
//! `sys.schemas` does (`views/sys_core.rs`).
//!
//! # Read columns and literal columns
//!
//! [`OBJECTS_TABLE`] holds 9 columns, 7 of which `sys.objects` reads and 8 `sys.tables`;
//! [`COLUMNS_TABLE`] holds 14, of which `sys.columns` reads 13. The other items of a select
//! list are literals, under the two rules of `views/sys_core.rs`: a value the catalogue does
//! not store is `CAST(NULL AS …)`, a value SQL Server publishes the same way for each user
//! table is written as that constant. The unit test
//! `the_columns_written_null_are_the_ones_with_no_datum_behind_them` names the `NULL`s and
//! counts them, `the_literal_columns_are_the_published_values` compares each literal with
//! the row it comes from.
//!
//! `create_date` and `modify_date` read [`OBJECTS_TABLE`]: the instant comes from the
//! transaction that wrote the row, as for `sys.databases.create_date`.
//!
//! # Which objects have a row
//!
//! The rows of the two tables are built from the `*Meta` of the catalogue by [`object_rows`]
//! and [`column_rows`] — a user table gives one row of type `U ` and one row per column
//! (unit test `user_table_has_object_and_column_rows`). The internal `vauban_sys_*` tables of
//! `master` are written at bootstrap with type `S ` / `SYSTEM_TABLE` (unit test
//! `internal_tables_are_system_tables_in_sys_objects`).
//!
//! What that leaves aside: a fresh SQL Server database answers 111 rows to
//! `SELECT COUNT(*) FROM sys.objects` — 72 of type `S `, 36 of type `IT`, 3 of type `SQ`,
//! each with `is_ms_shipped` 1 — and 0 rows to `SELECT COUNT(*) FROM sys.tables`; its
//! shipped views are in `sys.all_objects` (`views/sys_extra.rs`) and not in `sys.objects`.
//! VaubanDB answers 0 to the first query as long as this file writes no row for its own
//! internal tables; the second answer, 0, is the same on both.
//!
//! # Execution
//!
//! What this file produces is the shape of the two internal tables, the text of the three
//! definitions and the two functions that turn a `*Meta` into rows; `sys_rows.rs` writes
//! those rows into `storage` at each DDL, and the binder expands the text of a view in
//! place of its name.

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::{DbId, Row};
use vauban_txn::TxnHandle;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{self, DEFAULT_COLLATION_NAME, SYSTEM_DATABASES, SYSTEM_SCHEMAS};
use crate::catalog::Catalog;
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::{ColumnMeta, QualifiedName, TableMeta};

/// Internal table of the schema-scoped objects, read by `sys.objects` and `sys.tables`.
///
/// A `vauban_sys_*` name of our own; what a client reads is one of the two views built over
/// it.
pub(crate) const OBJECTS_TABLE: &str = "vauban_sys_objects";

/// Internal table of the columns, read by `sys.columns`.
pub(crate) const COLUMNS_TABLE: &str = "vauban_sys_columns";

/// The schema the three views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The column names of the three views that are T-SQL reserved words and are therefore
/// written as delimited identifiers in the text of a view: `precision`, a column of
/// `sys.columns` (unit test
/// `a_reserved_column_name_is_delimited`).
const RESERVED_COLUMN_NAMES: [&str; 1] = ["precision"];

/// `sys.objects.type` of a user table: a `char(2)`, so its second byte is a space.
const USER_TABLE_TYPE: &str = "U ";

/// The `type_desc` that goes with [`USER_TABLE_TYPE`].
const USER_TABLE_TYPE_DESC: &str = "USER_TABLE";

/// `parent_object_id` of an object that is not a constraint: `0`, not `NULL`.
const NO_PARENT: i32 = 0;

/// The [`ObjectId`](crate::ObjectId) of the first internal table of `master`; the next ones
/// count up with the position of each description in [`bootstrap::internal_table_defs`].
pub(crate) const FIRST_INTERNAL_TABLE_OBJECT_ID: i32 = 100_000;

/// `sys.objects.type` of an internal table: a `char(2)`, so its second byte is a space.
const SYSTEM_TABLE_TYPE: &str = "S ";

/// The `type_desc` that goes with [`SYSTEM_TABLE_TYPE`].
const SYSTEM_TABLE_TYPE_DESC: &str = "SYSTEM_TABLE";

/// The `schema_id` written for a schema the bootstrap does not know.
///
/// The three schemas of [`SYSTEM_SCHEMAS`] are the ones an instance holds (`CREATE SCHEMA`
/// is not served), so a table of another schema has no identifier to publish; `0` is not
/// one of the identifiers SQL Server hands out (`dbo` 1, `INFORMATION_SCHEMA` 3, `sys` 4,
/// `bootstrap.rs`), which keeps the two apart (unit test
/// `a_schema_the_bootstrap_does_not_know_has_no_identifier`).
const UNKNOWN_SCHEMA_ID: i32 = 0;

/// Where each column of [`OBJECTS_TABLE`] sits in a [`Row`], as `bootstrap.rs` does for its
/// own tables: a writer of a row addresses it through these constants rather than
/// restating the order (unit test `the_column_order_is_the_one_the_constants_name`).
pub(crate) mod objects_columns {
    /// `database_id int`: the database the object belongs to, the filter of the two views.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the identifier `sys.objects.object_id` publishes.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the object.
    pub(crate) const NAME: usize = 2;
    /// `schema_id int`: the schema the object belongs to.
    pub(crate) const SCHEMA_ID: usize = 3;
    /// `parent_object_id int`: the table of a constraint, `0` for a table.
    pub(crate) const PARENT_OBJECT_ID: usize = 4;
    /// `type char(2)`: `U ` for a user table.
    pub(crate) const TYPE: usize = 5;
    /// `type_desc nvarchar(60)`: `USER_TABLE` for a user table.
    pub(crate) const TYPE_DESC: usize = 6;
    /// `is_ms_shipped bit`: `0` for an object a client created.
    pub(crate) const IS_MS_SHIPPED: usize = 7;
    /// `create_date datetime`: the instant the object was created.
    pub(crate) const CREATE_DATE: usize = 8;
    /// `modify_date datetime`: the instant the object was last changed by DDL.
    pub(crate) const MODIFY_DATE: usize = 9;
    /// `max_column_id_used int`: read by `sys.tables`, not by `sys.objects`.
    pub(crate) const MAX_COLUMN_ID_USED: usize = 10;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 11;
}

/// Where each column of [`COLUMNS_TABLE`] sits in a [`Row`]. Same rule as
/// [`objects_columns`].
pub(crate) mod columns_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table the column belongs to.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the column.
    pub(crate) const NAME: usize = 2;
    /// `column_id int`: the identifier of the column within its table, from `1`.
    pub(crate) const COLUMN_ID: usize = 3;
    /// `system_type_id tinyint`: the identifier of the system type of the column.
    pub(crate) const SYSTEM_TYPE_ID: usize = 4;
    /// `user_type_id int`: equal to `system_type_id` for the 24 types served.
    pub(crate) const USER_TYPE_ID: usize = 5;
    /// `max_length smallint`: bytes, `-1` for a `(max)` type.
    pub(crate) const MAX_LENGTH: usize = 6;
    /// `precision tinyint`.
    pub(crate) const PRECISION: usize = 7;
    /// `scale tinyint`.
    pub(crate) const SCALE: usize = 8;
    /// `collation_name nvarchar(128)`, nullable: `NULL` outside the character types.
    pub(crate) const COLLATION_NAME: usize = 9;
    /// `is_nullable bit`.
    pub(crate) const IS_NULLABLE: usize = 10;
    /// `is_ansi_padded bit`.
    pub(crate) const IS_ANSI_PADDED: usize = 11;
    /// `is_identity bit`.
    pub(crate) const IS_IDENTITY: usize = 12;
    /// `is_computed bit`.
    pub(crate) const IS_COMPUTED: usize = 13;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 14;
}

/// The select list of `sys.objects`: `(column, expression)`, in the published order.
///
/// An expression equal to the column name reads [`OBJECTS_TABLE`]; the others are the
/// literals the module documentation explains.
const OBJECTS_VIEW: [(&str, &str); 12] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("create_date", "create_date"),
    ("modify_date", "modify_date"),
    ("is_ms_shipped", "is_ms_shipped"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
];

/// The select list of `sys.tables`: the 12 items of [`OBJECTS_VIEW`] and 36 more.
///
/// The values of the 36 are those SQL Server publishes for a
/// `CREATE TABLE dbo.t (id int NOT NULL PRIMARY KEY)`, except `max_column_id_used`, which
/// is a column of [`OBJECTS_TABLE`].
const TABLES_VIEW: [(&str, &str); 48] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("create_date", "create_date"),
    ("modify_date", "modify_date"),
    ("is_ms_shipped", "is_ms_shipped"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
    ("lob_data_space_id", "CAST(0 AS int)"),
    ("filestream_data_space_id", "CAST(NULL AS int)"),
    ("max_column_id_used", "max_column_id_used"),
    ("lock_on_bulk_load", "CAST(0 AS bit)"),
    ("uses_ansi_nulls", "CAST(1 AS bit)"),
    ("is_replicated", "CAST(0 AS bit)"),
    ("has_replication_filter", "CAST(0 AS bit)"),
    ("is_merge_published", "CAST(0 AS bit)"),
    ("is_sync_tran_subscribed", "CAST(0 AS bit)"),
    ("has_unchecked_assembly_data", "CAST(0 AS bit)"),
    ("text_in_row_limit", "CAST(0 AS int)"),
    ("large_value_types_out_of_row", "CAST(0 AS bit)"),
    ("is_tracked_by_cdc", "CAST(0 AS bit)"),
    ("lock_escalation", "CAST(0 AS tinyint)"),
    ("lock_escalation_desc", "CAST(N'TABLE' AS nvarchar(60))"),
    ("is_filetable", "CAST(0 AS bit)"),
    ("is_memory_optimized", "CAST(0 AS bit)"),
    ("durability", "CAST(0 AS tinyint)"),
    (
        "durability_desc",
        "CAST(N'SCHEMA_AND_DATA' AS nvarchar(60))",
    ),
    ("temporal_type", "CAST(0 AS tinyint)"),
    (
        "temporal_type_desc",
        "CAST(N'NON_TEMPORAL_TABLE' AS nvarchar(60))",
    ),
    ("history_table_id", "CAST(NULL AS int)"),
    ("is_remote_data_archive_enabled", "CAST(0 AS bit)"),
    ("is_external", "CAST(0 AS bit)"),
    ("history_retention_period", "CAST(NULL AS int)"),
    ("history_retention_period_unit", "CAST(NULL AS int)"),
    (
        "history_retention_period_unit_desc",
        "CAST(NULL AS nvarchar(10))",
    ),
    ("is_node", "CAST(0 AS bit)"),
    ("is_edge", "CAST(0 AS bit)"),
    ("data_retention_period", "CAST(-1 AS int)"),
    ("data_retention_period_unit", "CAST(-1 AS int)"),
    (
        "data_retention_period_unit_desc",
        "CAST(N'INFINITE' AS nvarchar(10))",
    ),
    ("ledger_type", "CAST(0 AS tinyint)"),
    (
        "ledger_type_desc",
        "CAST(N'NON_LEDGER_TABLE' AS nvarchar(60))",
    ),
    ("ledger_view_id", "CAST(NULL AS int)"),
    ("is_dropped_ledger_table", "CAST(0 AS bit)"),
];

/// The select list of `sys.columns`: its 40 columns, 13 of which read [`COLUMNS_TABLE`].
///
/// The values of the 27 literals are those SQL Server publishes for the columns of a user
/// table, which agree on each of them.
const COLUMNS_VIEW: [(&str, &str); 40] = [
    ("object_id", "object_id"),
    ("name", "name"),
    ("column_id", "column_id"),
    ("system_type_id", "system_type_id"),
    ("user_type_id", "user_type_id"),
    ("max_length", "max_length"),
    ("precision", "precision"),
    ("scale", "scale"),
    ("collation_name", "collation_name"),
    ("is_nullable", "is_nullable"),
    ("is_ansi_padded", "is_ansi_padded"),
    ("is_rowguidcol", "CAST(0 AS bit)"),
    ("is_identity", "is_identity"),
    ("is_computed", "is_computed"),
    ("is_filestream", "CAST(0 AS bit)"),
    ("is_replicated", "CAST(0 AS bit)"),
    ("is_non_sql_subscribed", "CAST(0 AS bit)"),
    ("is_merge_published", "CAST(0 AS bit)"),
    ("is_dts_replicated", "CAST(0 AS bit)"),
    ("is_xml_document", "CAST(0 AS bit)"),
    ("xml_collection_id", "CAST(0 AS int)"),
    ("default_object_id", "CAST(0 AS int)"),
    ("rule_object_id", "CAST(0 AS int)"),
    ("is_sparse", "CAST(0 AS bit)"),
    ("is_column_set", "CAST(0 AS bit)"),
    ("generated_always_type", "CAST(0 AS tinyint)"),
    (
        "generated_always_type_desc",
        "CAST(N'NOT_APPLICABLE' AS nvarchar(60))",
    ),
    ("encryption_type", "CAST(NULL AS int)"),
    ("encryption_type_desc", "CAST(NULL AS nvarchar(64))"),
    ("encryption_algorithm_name", "CAST(NULL AS nvarchar(128))"),
    ("column_encryption_key_id", "CAST(NULL AS int)"),
    (
        "column_encryption_key_database_name",
        "CAST(NULL AS nvarchar(128))",
    ),
    ("is_hidden", "CAST(0 AS bit)"),
    ("is_masked", "CAST(0 AS bit)"),
    ("graph_type", "CAST(NULL AS int)"),
    ("graph_type_desc", "CAST(NULL AS nvarchar(60))"),
    ("is_data_deletion_filter_column", "CAST(0 AS bit)"),
    ("ledger_view_column_type", "CAST(NULL AS int)"),
    ("ledger_view_column_type_desc", "CAST(NULL AS nvarchar(60))"),
    ("is_dropped_ledger_column", "CAST(0 AS bit)"),
];

/// The internal tables this file describes: the objects and the columns.
///
/// The bootstrap creates them in `master`. Their `rows` are empty: the rows of a user object are
/// built from the `*Meta` of the catalogue by [`object_rows`] and [`column_rows`], and the
/// internal tables themselves are not published (module documentation).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: OBJECTS_TABLE.to_owned(),
            columns: objects_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: [
                views_of(
                    "objects",
                    &definition(&OBJECTS_VIEW, OBJECTS_TABLE, Some("database_id = DB_ID()")),
                ),
                views_of(
                    "tables",
                    &definition(
                        &TABLES_VIEW,
                        OBJECTS_TABLE,
                        Some("database_id = DB_ID()\n   AND type = 'U '"),
                    ),
                ),
            ]
            .concat(),
        },
        InternalTableDef {
            name: COLUMNS_TABLE.to_owned(),
            columns: columns_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "columns",
                &definition(&COLUMNS_VIEW, COLUMNS_TABLE, Some("database_id = DB_ID()")),
            ),
        },
    ]
}

/// The rows of [`OBJECTS_TABLE`] for `tables`: one per table, of type [`USER_TABLE_TYPE`].
///
/// The caller passes the tables of one catalogue — [`TableStore::live`](crate::table::TableStore::live)
/// gives them in increasing [`ObjectId`](crate::ObjectId) order — and gets the rows in the
/// same order.
///
/// # Errors
///
/// [`InternalError::Bug`] when a [`DbId`] does not fit in the `int` the view publishes,
/// which is the check `bootstrap.rs` makes on the same value.
/// Rows of [`OBJECTS_TABLE`] with undated `create_date` and `modify_date` columns.
///
/// Used by `views/sys_extra.rs` for `sys.all_objects`, which this task does not date.
pub(crate) fn object_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    dated_object_rows(tables, &Value::Null, &Value::Null)
}

/// Rows of [`OBJECTS_TABLE`] with the instants the caller supplies.
pub(crate) fn dated_object_rows(
    tables: &[TableMeta],
    create_date: &Value,
    modify_date: &Value,
) -> SqlResult<Vec<Row>> {
    tables
        .iter()
        .map(|table| object_row(table, create_date, modify_date))
        .collect()
}

/// The row of [`OBJECTS_TABLE`] of one table. See [`object_rows`].
fn object_row(table: &TableMeta, create_date: &Value, modify_date: &Value) -> SqlResult<Row> {
    let max_column_id_used = table
        .columns
        .iter()
        .map(|column| column.id.0)
        .max()
        .unwrap_or(0);
    Ok(Row(vec![
        Value::I32(database_id(table.database)?),
        Value::I32(table.id.0),
        text(&table.name),
        Value::I32(schema_id(&table.schema)),
        Value::I32(NO_PARENT),
        text(USER_TABLE_TYPE),
        text(USER_TABLE_TYPE_DESC),
        Value::Bit(false),
        create_date.clone(),
        modify_date.clone(),
        Value::I32(max_column_id_used),
    ]))
}

/// The rows of [`OBJECTS_TABLE`] for the internal tables of `master`, one per description.
pub(crate) fn internal_table_object_rows(
    master: i32,
    tables: &[InternalTableDef],
    create_date: &Value,
    modify_date: &Value,
) -> SqlResult<Vec<Row>> {
    tables
        .iter()
        .enumerate()
        .map(|(position, def)| {
            let rank = i32::try_from(position).map_err(|_| {
                InternalError::Bug(format!(
                    "sys.objects: internal table position {position} does not fit in an int"
                ))
            })?;
            let object_id = FIRST_INTERNAL_TABLE_OBJECT_ID
                .checked_add(rank)
                .ok_or_else(|| {
                    InternalError::Bug(format!(
                        "sys.objects: internal table object id for position {position} overflows"
                    ))
                })?;
            Ok(Row(vec![
                Value::I32(master),
                Value::I32(object_id),
                text(&def.name),
                Value::I32(schema_id("dbo")),
                Value::I32(NO_PARENT),
                text(SYSTEM_TABLE_TYPE),
                text(SYSTEM_TABLE_TYPE_DESC),
                Value::Bit(true),
                create_date.clone(),
                modify_date.clone(),
                Value::I32(0),
            ]))
        })
        .collect()
}

/// Writes the rows [`internal_table_object_rows`] builds into [`OBJECTS_TABLE`].
pub(crate) fn write_internal_object_rows(
    catalog: &Catalog,
    txn: &TxnHandle,
    master: DbId,
    instant: Value,
) -> SqlResult<()> {
    let master_id = database_id(master)?;
    let defs = bootstrap::internal_table_defs()?;
    let rows = internal_table_object_rows(master_id, &defs, &instant, &instant)?;
    let Some(table) = bootstrap::internal_table_id(catalog, OBJECTS_TABLE)? else {
        return Err(
            InternalError::Bug(format!("internal table {OBJECTS_TABLE} is missing")).into(),
        );
    };
    for row in rows {
        catalog.storage.insert(txn.id, table, &row)?;
    }
    Ok(())
}

/// The rows of [`COLUMNS_TABLE`] for `tables`: one per column, in `column_id` order within
/// each table.
///
/// # Errors
///
/// Those of [`object_rows`], plus [`InternalError::Bug`] when a declared length does not fit
/// in the `smallint` `sys.columns.max_length` publishes.
pub(crate) fn column_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for column in &table.columns {
            rows.push(column_row(database, table, column)?);
        }
    }
    Ok(rows)
}

/// The row of [`COLUMNS_TABLE`] of one column. See [`column_rows`].
fn column_row(database: i32, table: &TableMeta, column: &ColumnMeta) -> SqlResult<Row> {
    let facts = column_facts(&column.ty)?;
    Ok(Row(vec![
        Value::I32(database),
        Value::I32(table.id.0),
        text(&column.name),
        Value::I32(column.id.0),
        Value::I8(facts.system_type_id),
        Value::I32(i32::from(facts.system_type_id)),
        Value::I16(facts.max_length),
        Value::I8(facts.precision),
        Value::I8(facts.scale),
        if facts.collated {
            text(DEFAULT_COLLATION_NAME)
        } else {
            Value::Null
        },
        Value::Bit(column.ty.nullable),
        Value::Bit(facts.ansi_padded),
        Value::Bit(column.identity.is_some()),
        Value::Bit(column.computed.is_some()),
    ]))
}

/// What `sys.columns` publishes about the type of a column.
///
/// `user_type_id` is not here: it equals `system_type_id` for the 24 types of
/// [`SqlType`](vauban_types::SqlType), and an alias type — `sysname` — is not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColumnFacts {
    /// `sys.columns.system_type_id`.
    system_type_id: u8,
    /// `sys.columns.max_length`, in bytes, `-1` for a `(max)` type.
    max_length: i16,
    /// `sys.columns.precision`.
    precision: u8,
    /// `sys.columns.scale`.
    scale: u8,
    /// Whether the column publishes a `collation_name`.
    collated: bool,
    /// `sys.columns.is_ansi_padded`.
    ansi_padded: bool,
}

/// What `sys.columns` publishes about `ty`.
///
/// The numbers are those of `sys.columns` for the 24 variants of
/// [`SqlType`](vauban_types::SqlType), the 8 scales of `time`, `datetime2` and
/// `datetimeoffset` and the 38 precisions of `decimal`, frozen by the unit test
/// `the_type_facts_are_those_of_sys_columns`.
///
/// # Errors
///
/// [`InternalError::Bug`] when the declared length in bytes does not fit in a `smallint`:
/// `nchar(n)` and `nvarchar(n)` publish `2 * n`, and the T-SQL grammar stops those two at
/// 4 000, so the error names a `TypeInfo` no `CREATE TABLE`
/// builds.
fn column_facts(ty: &TypeInfo) -> SqlResult<ColumnFacts> {
    let collated = ty.ty.is_string();
    let (system_type_id, max_length, precision, scale) = match ty.ty {
        SqlType::Bit => (104, 1, 1, 0),
        SqlType::TinyInt => (48, 1, 3, 0),
        SqlType::SmallInt => (52, 2, 5, 0),
        SqlType::Int => (56, 4, 10, 0),
        SqlType::BigInt => (127, 8, 19, 0),
        SqlType::Decimal { precision, scale } => (106, decimal_length(precision), precision, scale),
        SqlType::Numeric { precision, scale } => (108, decimal_length(precision), precision, scale),
        SqlType::Float => (62, 8, 53, 0),
        SqlType::Real => (59, 4, 24, 0),
        SqlType::Money => (60, 8, 19, 4),
        SqlType::SmallMoney => (122, 4, 10, 4),
        SqlType::Char(len) => (175, byte_length(len, 1)?, 0, 0),
        SqlType::VarChar(len) => (167, byte_length(len, 1)?, 0, 0),
        SqlType::NChar(len) => (239, byte_length(len, 2)?, 0, 0),
        SqlType::NVarChar(len) => (231, byte_length(len, 2)?, 0, 0),
        SqlType::Binary(len) => (173, byte_length(len, 1)?, 0, 0),
        SqlType::VarBinary(len) => (165, byte_length(len, 1)?, 0, 0),
        SqlType::Date => (40, 3, 10, 0),
        SqlType::Time(scale) => (41, time_length(scale), 8 + fraction_digits(scale), scale),
        SqlType::DateTime => (61, 8, 23, 3),
        SqlType::SmallDateTime => (58, 4, 16, 0),
        SqlType::DateTime2(scale) => (
            42,
            3 + time_length(scale),
            19 + fraction_digits(scale),
            scale,
        ),
        SqlType::DateTimeOffset(scale) => (
            43,
            5 + time_length(scale),
            26 + fraction_digits(scale),
            scale,
        ),
        SqlType::UniqueIdentifier => (36, 16, 0, 0),
    };
    Ok(ColumnFacts {
        system_type_id,
        max_length,
        precision,
        scale,
        collated,
        // `is_ansi_padded` is 1 on the character and binary columns and 0 on the others
        // (unit test `the_type_facts_are_those_of_sys_columns`).
        ansi_padded: collated || matches!(ty.ty, SqlType::Binary(_) | SqlType::VarBinary(_)),
    })
}

/// `max_length` of a `decimal(p, s)` or a `numeric(p, s)`: 5, 9, 13 or 17 bytes.
///
/// The four steps over the 38 precisions: 1 to 9 give 5, 10 to 19 give 9, 20 to 28 give
/// 13, 29 to 38 give 17.
fn decimal_length(precision: u8) -> i16 {
    match precision {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

/// `max_length` of a `time(s)`: 3, 4 or 5 bytes over the 8 scales.
///
/// `datetime2(s)` adds the 3 bytes of its date and `datetimeoffset(s)` 5 (its date and its
/// offset): 6/7/8 and 8/9/10.
fn time_length(scale: u8) -> i16 {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

/// What a fractional-seconds scale adds to the `precision` of a `time`, a `datetime2` or a
/// `datetimeoffset`: the digits and the decimal point, nothing at scale 0.
///
/// `time(0)` is 8 and `time(7)` 16, `datetime2(0)` 19 and `datetime2(7)` 27,
/// `datetimeoffset(0)` 26 and `datetimeoffset(7)` 34.
fn fraction_digits(scale: u8) -> u8 {
    if scale == 0 { 0 } else { scale + 1 }
}

/// `max_length` of a character or binary type: the declared length times `bytes_per_unit`,
/// `-1` for `(max)` (`varchar(max)`, `nvarchar(max)` and `varbinary(max)`).
///
/// # Errors
///
/// [`InternalError::Bug`] when the product does not fit in the `smallint` the view
/// publishes. See [`column_facts`].
fn byte_length(len: Len, bytes_per_unit: i32) -> SqlResult<i16> {
    match len {
        Len::Max => Ok(-1),
        Len::Fixed(n) => i16::try_from(i32::from(n) * bytes_per_unit).map_err(|_| {
            InternalError::Bug(format!(
                "sys.columns: a declared length of {n} unit(s) of {bytes_per_unit} byte(s) does \
                 not fit in the smallint max_length publishes"
            ))
            .into()
        }),
    }
}

/// The `int` a [`DbId`] is published as, `sys.objects` being per-database and the internal
/// tables holding the databases side by side.
///
/// # Errors
///
/// [`InternalError::Bug`], as `bootstrap.rs` does on the same value.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "sys.objects: database id {id} does not fit in an int"
        ))
        .into()
    })
}

/// The `schema_id` of the schema named `name`, [`UNKNOWN_SCHEMA_ID`] when the bootstrap
/// created no schema of that name.
///
/// Compared without regard to case, as `SQL_Latin1_General_CP1_CI_AS` compares identifiers.
fn schema_id(name: &str) -> i32 {
    let name = if name.is_empty() { "dbo" } else { name };
    SYSTEM_SCHEMAS
        .iter()
        .find(|(schema, _, _)| schema.eq_ignore_ascii_case(name))
        .map_or(UNKNOWN_SCHEMA_ID, |(_, id, _)| *id)
}

/// The columns of [`OBJECTS_TABLE`], in the order of [`objects_columns`].
fn objects_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        column("name", SYSNAME, false),
        column("schema_id", SqlType::Int, false),
        column("parent_object_id", SqlType::Int, false),
        column("type", SqlType::Char(Len::Fixed(2)), false),
        column("type_desc", SqlType::NVarChar(Len::Fixed(60)), false),
        column("is_ms_shipped", SqlType::Bit, false),
        column("create_date", SqlType::DateTime, false),
        column("modify_date", SqlType::DateTime, false),
        column("max_column_id_used", SqlType::Int, false),
    ]
}

/// The columns of [`COLUMNS_TABLE`], in the order of [`columns_columns`].
fn columns_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        column("name", SYSNAME, false),
        column("column_id", SqlType::Int, false),
        column("system_type_id", SqlType::TinyInt, false),
        column("user_type_id", SqlType::Int, false),
        column("max_length", SqlType::SmallInt, false),
        column("precision", SqlType::TinyInt, false),
        column("scale", SqlType::TinyInt, false),
        column("collation_name", SYSNAME, true),
        column("is_nullable", SqlType::Bit, false),
        column("is_ansi_padded", SqlType::Bit, false),
        column("is_identity", SqlType::Bit, false),
        column("is_computed", SqlType::Bit, false),
    ]
}

/// The same view installed in the four system databases, `sys.<name>` in each of them.
///
/// The three views filter on `DB_ID()`, so the four definitions share their text (unit test
/// `the_views_are_installed_in_the_four_system_databases`). A database created by
/// `CREATE DATABASE` receives no copies yet, as for `views/sys_core.rs`.
fn views_of(name: &str, definition: &str) -> Vec<SystemViewDef> {
    SYSTEM_DATABASES
        .iter()
        .map(|database| SystemViewDef {
            name: QualifiedName {
                database: (*database).to_owned(),
                schema: VIEW_SCHEMA.to_owned(),
                name: name.to_owned(),
            },
            definition: definition.to_owned(),
        })
        .collect()
}

/// The T-SQL text of a view: its select list, the internal table it reads in `master`, and
/// its filter.
///
/// Same layout as `views/sys_core.rs`, which owns the other three views: one select item per
/// line, an item whose expression is its own name written bare, the others as
/// `<expression> AS <name>`, so the name of a column is the last identifier of its item. The
/// two files each hold their own copy of this helper.
fn definition(items: &[(&str, &str)], table: &str, filter: Option<&str>) -> String {
    let mut text = String::from("SELECT ");
    for (index, (name, expression)) in items.iter().enumerate() {
        if index > 0 {
            text.push_str(",\n       ");
        }
        if expression != name {
            text.push_str(expression);
            text.push_str(" AS ");
        }
        text.push_str(&identifier(name));
    }
    text.push_str("\n  FROM master.dbo.");
    text.push_str(table);
    if let Some(filter) = filter {
        text.push_str("\n WHERE ");
        text.push_str(filter);
    }
    text
}

/// The name of a column as the text of a view writes it: bracketed when it is a reserved
/// word of T-SQL, bare otherwise.
fn identifier(name: &str) -> String {
    if RESERVED_COLUMN_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
    {
        format!("[{name}]")
    } else {
        name.to_owned()
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};

    use super::*;
    use crate::bootstrap;
    use crate::catalog::Catalog;
    use crate::def::{ColumnDef, TableDef};
    use crate::meta::IdentitySpec;
    use crate::table;

    /// The ordered column names of `sys.objects` in SQL Server 2022. Frozen here so that a
    /// change of the text of the view has to face the list again.
    const PUBLISHED_OBJECTS_COLUMNS: [&str; 12] = [
        "name",
        "object_id",
        "principal_id",
        "schema_id",
        "parent_object_id",
        "type",
        "type_desc",
        "create_date",
        "modify_date",
        "is_ms_shipped",
        "is_published",
        "is_schema_published",
    ];

    /// The ordered column names of `sys.tables` in SQL Server 2022.
    const PUBLISHED_TABLES_COLUMNS: [&str; 48] = [
        "name",
        "object_id",
        "principal_id",
        "schema_id",
        "parent_object_id",
        "type",
        "type_desc",
        "create_date",
        "modify_date",
        "is_ms_shipped",
        "is_published",
        "is_schema_published",
        "lob_data_space_id",
        "filestream_data_space_id",
        "max_column_id_used",
        "lock_on_bulk_load",
        "uses_ansi_nulls",
        "is_replicated",
        "has_replication_filter",
        "is_merge_published",
        "is_sync_tran_subscribed",
        "has_unchecked_assembly_data",
        "text_in_row_limit",
        "large_value_types_out_of_row",
        "is_tracked_by_cdc",
        "lock_escalation",
        "lock_escalation_desc",
        "is_filetable",
        "is_memory_optimized",
        "durability",
        "durability_desc",
        "temporal_type",
        "temporal_type_desc",
        "history_table_id",
        "is_remote_data_archive_enabled",
        "is_external",
        "history_retention_period",
        "history_retention_period_unit",
        "history_retention_period_unit_desc",
        "is_node",
        "is_edge",
        "data_retention_period",
        "data_retention_period_unit",
        "data_retention_period_unit_desc",
        "ledger_type",
        "ledger_type_desc",
        "ledger_view_id",
        "is_dropped_ledger_table",
    ];

    /// The ordered column names of `sys.columns` in SQL Server 2022.
    const PUBLISHED_COLUMNS_COLUMNS: [&str; 40] = [
        "object_id",
        "name",
        "column_id",
        "system_type_id",
        "user_type_id",
        "max_length",
        "precision",
        "scale",
        "collation_name",
        "is_nullable",
        "is_ansi_padded",
        "is_rowguidcol",
        "is_identity",
        "is_computed",
        "is_filestream",
        "is_replicated",
        "is_non_sql_subscribed",
        "is_merge_published",
        "is_dts_replicated",
        "is_xml_document",
        "xml_collection_id",
        "default_object_id",
        "rule_object_id",
        "is_sparse",
        "is_column_set",
        "generated_always_type",
        "generated_always_type_desc",
        "encryption_type",
        "encryption_type_desc",
        "encryption_algorithm_name",
        "column_encryption_key_id",
        "column_encryption_key_database_name",
        "is_hidden",
        "is_masked",
        "graph_type",
        "graph_type_desc",
        "is_data_deletion_filter_column",
        "ledger_view_column_type",
        "ledger_view_column_type_desc",
        "is_dropped_ledger_column",
    ];

    /// The row `sys.objects` publishes for a user table
    /// (`CREATE TABLE dbo.objects_target (id int NOT NULL)`).
    const EXPECTED_OBJECTS_ROW: [(&str, Option<&str>); 12] = [
        ("name", Some("objects_target")),
        ("object_id", Some("901578250")),
        ("principal_id", None),
        ("schema_id", Some("1")),
        ("parent_object_id", Some("0")),
        ("type", Some("U ")),
        ("type_desc", Some("USER_TABLE")),
        ("create_date", Some("2026-01-01T00:00:00.000")),
        ("modify_date", Some("2026-01-01T00:00:00.000")),
        ("is_ms_shipped", Some("0")),
        ("is_published", Some("0")),
        ("is_schema_published", Some("0")),
    ];

    /// The row `sys.tables` publishes for a user table
    /// (`CREATE TABLE dbo.tables_target (id int NOT NULL PRIMARY KEY)`).
    const EXPECTED_TABLES_ROW: [(&str, Option<&str>); 48] = [
        ("name", Some("tables_target")),
        ("object_id", Some("901578250")),
        ("principal_id", None),
        ("schema_id", Some("1")),
        ("parent_object_id", Some("0")),
        ("type", Some("U ")),
        ("type_desc", Some("USER_TABLE")),
        ("create_date", Some("2026-01-01T00:00:00.000")),
        ("modify_date", Some("2026-01-01T00:00:00.000")),
        ("is_ms_shipped", Some("0")),
        ("is_published", Some("0")),
        ("is_schema_published", Some("0")),
        ("lob_data_space_id", Some("0")),
        ("filestream_data_space_id", None),
        ("max_column_id_used", Some("1")),
        ("lock_on_bulk_load", Some("0")),
        ("uses_ansi_nulls", Some("1")),
        ("is_replicated", Some("0")),
        ("has_replication_filter", Some("0")),
        ("is_merge_published", Some("0")),
        ("is_sync_tran_subscribed", Some("0")),
        ("has_unchecked_assembly_data", Some("0")),
        ("text_in_row_limit", Some("0")),
        ("large_value_types_out_of_row", Some("0")),
        ("is_tracked_by_cdc", Some("0")),
        ("lock_escalation", Some("0")),
        ("lock_escalation_desc", Some("TABLE")),
        ("is_filetable", Some("0")),
        ("is_memory_optimized", Some("0")),
        ("durability", Some("0")),
        ("durability_desc", Some("SCHEMA_AND_DATA")),
        ("temporal_type", Some("0")),
        ("temporal_type_desc", Some("NON_TEMPORAL_TABLE")),
        ("history_table_id", None),
        ("is_remote_data_archive_enabled", Some("0")),
        ("is_external", Some("0")),
        ("history_retention_period", None),
        ("history_retention_period_unit", None),
        ("history_retention_period_unit_desc", None),
        ("is_node", Some("0")),
        ("is_edge", Some("0")),
        ("data_retention_period", Some("-1")),
        ("data_retention_period_unit", Some("-1")),
        ("data_retention_period_unit_desc", Some("INFINITE")),
        ("ledger_type", Some("0")),
        ("ledger_type_desc", Some("NON_LEDGER_TABLE")),
        ("ledger_view_id", None),
        ("is_dropped_ledger_table", Some("0")),
    ];

    /// The first of the five rows `sys.columns` publishes for the table of
    /// `EXPECTED_COLUMN_ROWS` (column `id int IDENTITY(1,1) NOT NULL`); the five agree on
    /// each of the 27 literal columns, which is what
    /// `the_literal_columns_are_the_published_values` compares them with.
    const EXPECTED_COLUMNS_ROW: [(&str, Option<&str>); 40] = [
        ("object_id", Some("901578250")),
        ("name", Some("id")),
        ("column_id", Some("1")),
        ("system_type_id", Some("56")),
        ("user_type_id", Some("56")),
        ("max_length", Some("4")),
        ("precision", Some("10")),
        ("scale", Some("0")),
        ("collation_name", None),
        ("is_nullable", Some("0")),
        ("is_ansi_padded", Some("0")),
        ("is_rowguidcol", Some("0")),
        ("is_identity", Some("1")),
        ("is_computed", Some("0")),
        ("is_filestream", Some("0")),
        ("is_replicated", Some("0")),
        ("is_non_sql_subscribed", Some("0")),
        ("is_merge_published", Some("0")),
        ("is_dts_replicated", Some("0")),
        ("is_xml_document", Some("0")),
        ("xml_collection_id", Some("0")),
        ("default_object_id", Some("0")),
        ("rule_object_id", Some("0")),
        ("is_sparse", Some("0")),
        ("is_column_set", Some("0")),
        ("generated_always_type", Some("0")),
        ("generated_always_type_desc", Some("NOT_APPLICABLE")),
        ("encryption_type", None),
        ("encryption_type_desc", None),
        ("encryption_algorithm_name", None),
        ("column_encryption_key_id", None),
        ("column_encryption_key_database_name", None),
        ("is_hidden", Some("0")),
        ("is_masked", Some("0")),
        ("graph_type", None),
        ("graph_type_desc", None),
        ("is_data_deletion_filter_column", Some("0")),
        ("ledger_view_column_type", None),
        ("ledger_view_column_type_desc", None),
        ("is_dropped_ledger_column", Some("0")),
    ];

    /// The 12 fields `column_rows` builds, for the five columns of the table
    /// `dbo.columns_target` of `the_column_rows_of_a_five_column_table_carry_its_values`.
    const EXPECTED_COLUMN_ROWS: [&[(&str, Option<&str>)]; 5] = [
        &[
            ("name", Some("id")),
            ("column_id", Some("1")),
            ("system_type_id", Some("56")),
            ("user_type_id", Some("56")),
            ("max_length", Some("4")),
            ("precision", Some("10")),
            ("scale", Some("0")),
            ("collation_name", None),
            ("is_nullable", Some("0")),
            ("is_ansi_padded", Some("0")),
            ("is_identity", Some("1")),
            ("is_computed", Some("0")),
        ],
        &[
            ("name", Some("label")),
            ("column_id", Some("2")),
            ("system_type_id", Some("167")),
            ("user_type_id", Some("167")),
            ("max_length", Some("30")),
            ("precision", Some("0")),
            ("scale", Some("0")),
            ("collation_name", Some("SQL_Latin1_General_CP1_CI_AS")),
            ("is_nullable", Some("1")),
            ("is_ansi_padded", Some("1")),
            ("is_identity", Some("0")),
            ("is_computed", Some("0")),
        ],
        &[
            ("name", Some("amount")),
            ("column_id", Some("3")),
            ("system_type_id", Some("106")),
            ("user_type_id", Some("106")),
            ("max_length", Some("5")),
            ("precision", Some("9")),
            ("scale", Some("2")),
            ("collation_name", None),
            ("is_nullable", Some("1")),
            ("is_ansi_padded", Some("0")),
            ("is_identity", Some("0")),
            ("is_computed", Some("0")),
        ],
        &[
            ("name", Some("note")),
            ("column_id", Some("4")),
            ("system_type_id", Some("231")),
            ("user_type_id", Some("231")),
            ("max_length", Some("-1")),
            ("precision", Some("0")),
            ("scale", Some("0")),
            ("collation_name", Some("SQL_Latin1_General_CP1_CI_AS")),
            ("is_nullable", Some("1")),
            ("is_ansi_padded", Some("1")),
            ("is_identity", Some("0")),
            ("is_computed", Some("0")),
        ],
        &[
            ("name", Some("made_on")),
            ("column_id", Some("5")),
            ("system_type_id", Some("42")),
            ("user_type_id", Some("42")),
            ("max_length", Some("7")),
            ("precision", Some("23")),
            ("scale", Some("3")),
            ("collation_name", None),
            ("is_nullable", Some("1")),
            ("is_ansi_padded", Some("0")),
            ("is_identity", Some("0")),
            ("is_computed", Some("0")),
        ],
    ];

    /// What `sys.columns` publishes for each type: `(type, system_type_id, max_length,
    /// precision, scale, collated, ansi_padded)`.
    const EXPECTED_TYPE_FACTS: [(SqlType, u8, i16, u8, u8, bool, bool); 57] = [
        (SqlType::Bit, 104, 1, 1, 0, false, false),
        (SqlType::TinyInt, 48, 1, 3, 0, false, false),
        (SqlType::SmallInt, 52, 2, 5, 0, false, false),
        (SqlType::Int, 56, 4, 10, 0, false, false),
        (SqlType::BigInt, 127, 8, 19, 0, false, false),
        (
            SqlType::Decimal {
                precision: 1,
                scale: 0,
            },
            106,
            5,
            1,
            0,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 9,
                scale: 2,
            },
            106,
            5,
            9,
            2,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 10,
                scale: 0,
            },
            106,
            9,
            10,
            0,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 19,
                scale: 4,
            },
            106,
            9,
            19,
            4,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 20,
                scale: 0,
            },
            106,
            13,
            20,
            0,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 28,
                scale: 7,
            },
            106,
            13,
            28,
            7,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 29,
                scale: 0,
            },
            106,
            17,
            29,
            0,
            false,
            false,
        ),
        (
            SqlType::Decimal {
                precision: 38,
                scale: 38,
            },
            106,
            17,
            38,
            38,
            false,
            false,
        ),
        (
            SqlType::Numeric {
                precision: 5,
                scale: 2,
            },
            108,
            5,
            5,
            2,
            false,
            false,
        ),
        (
            SqlType::Numeric {
                precision: 38,
                scale: 0,
            },
            108,
            17,
            38,
            0,
            false,
            false,
        ),
        (SqlType::Float, 62, 8, 53, 0, false, false),
        (SqlType::Real, 59, 4, 24, 0, false, false),
        (SqlType::Money, 60, 8, 19, 4, false, false),
        (SqlType::SmallMoney, 122, 4, 10, 4, false, false),
        (SqlType::Char(Len::Fixed(10)), 175, 10, 0, 0, true, true),
        (SqlType::Char(Len::Fixed(1)), 175, 1, 0, 0, true, true),
        (SqlType::VarChar(Len::Fixed(30)), 167, 30, 0, 0, true, true),
        (SqlType::VarChar(Len::Max), 167, -1, 0, 0, true, true),
        (SqlType::NChar(Len::Fixed(10)), 239, 20, 0, 0, true, true),
        (SqlType::NVarChar(Len::Fixed(10)), 231, 20, 0, 0, true, true),
        (SqlType::NVarChar(Len::Max), 231, -1, 0, 0, true, true),
        (SqlType::Binary(Len::Fixed(8)), 173, 8, 0, 0, false, true),
        (
            SqlType::VarBinary(Len::Fixed(20)),
            165,
            20,
            0,
            0,
            false,
            true,
        ),
        (SqlType::VarBinary(Len::Max), 165, -1, 0, 0, false, true),
        (SqlType::Date, 40, 3, 10, 0, false, false),
        (SqlType::Time(0), 41, 3, 8, 0, false, false),
        (SqlType::Time(2), 41, 3, 11, 2, false, false),
        (SqlType::Time(3), 41, 4, 12, 3, false, false),
        (SqlType::Time(4), 41, 4, 13, 4, false, false),
        (SqlType::Time(7), 41, 5, 16, 7, false, false),
        (SqlType::DateTime, 61, 8, 23, 3, false, false),
        (SqlType::SmallDateTime, 58, 4, 16, 0, false, false),
        (SqlType::DateTime2(0), 42, 6, 19, 0, false, false),
        (SqlType::DateTime2(2), 42, 6, 22, 2, false, false),
        (SqlType::DateTime2(3), 42, 7, 23, 3, false, false),
        (SqlType::DateTime2(5), 42, 8, 25, 5, false, false),
        (SqlType::DateTime2(7), 42, 8, 27, 7, false, false),
        (SqlType::DateTimeOffset(0), 43, 8, 26, 0, false, false),
        (SqlType::DateTimeOffset(3), 43, 9, 30, 3, false, false),
        (SqlType::DateTimeOffset(7), 43, 10, 34, 7, false, false),
        (SqlType::UniqueIdentifier, 36, 16, 0, 0, false, false),
        (SqlType::Time(1), 41, 3, 10, 1, false, false),
        (SqlType::Time(5), 41, 5, 14, 5, false, false),
        (SqlType::Time(6), 41, 5, 15, 6, false, false),
        (SqlType::DateTime2(1), 42, 6, 21, 1, false, false),
        (SqlType::DateTime2(4), 42, 7, 24, 4, false, false),
        (SqlType::DateTime2(6), 42, 8, 26, 6, false, false),
        (SqlType::DateTimeOffset(1), 43, 8, 28, 1, false, false),
        (SqlType::DateTimeOffset(2), 43, 8, 29, 2, false, false),
        (SqlType::DateTimeOffset(4), 43, 9, 31, 4, false, false),
        (SqlType::DateTimeOffset(5), 43, 10, 32, 5, false, false),
        (SqlType::DateTimeOffset(6), 43, 10, 33, 6, false, false),
    ];

    /// `max_length` of a `decimal(p, 0)` for each precision `p`.
    const EXPECTED_DECIMAL_LENGTHS: [(u8, i16); 38] = [
        (1, 5),
        (2, 5),
        (3, 5),
        (4, 5),
        (5, 5),
        (6, 5),
        (7, 5),
        (8, 5),
        (9, 5),
        (10, 9),
        (11, 9),
        (12, 9),
        (13, 9),
        (14, 9),
        (15, 9),
        (16, 9),
        (17, 9),
        (18, 9),
        (19, 9),
        (20, 13),
        (21, 13),
        (22, 13),
        (23, 13),
        (24, 13),
        (25, 13),
        (26, 13),
        (27, 13),
        (28, 13),
        (29, 17),
        (30, 17),
        (31, 17),
        (32, 17),
        (33, 17),
        (34, 17),
        (35, 17),
        (36, 17),
        (37, 17),
        (38, 17),
    ];

    /// The two view texts [`OBJECTS_TABLE`] carries, `sys.objects` then `sys.tables`, and
    /// the one of [`COLUMNS_TABLE`], read back from what [`internal_tables`] describes.
    fn definitions() -> Vec<(String, String)> {
        internal_tables()
            .into_iter()
            .flat_map(|table| table.views)
            .filter(|view| view.name.database == "master")
            .map(|view| (view.name.name, view.definition))
            .collect()
    }

    /// The text of the view `sys.<name>`, as [`definitions`] reads it.
    fn definition_of(name: &str) -> String {
        definitions()
            .into_iter()
            .find(|(view, _)| view == name)
            .unwrap_or_else(|| panic!("sys.{name} is described here"))
            .1
    }

    /// The column names the text of a view publishes: the last identifier of each select
    /// item, brackets removed. Same reading as `views/sys_core.rs`.
    fn published_columns(definition: &str) -> Vec<String> {
        let select = definition
            .split("\n  FROM ")
            .next()
            .expect("the text has a FROM clause")
            .strip_prefix("SELECT ")
            .expect("the text starts with SELECT");
        select
            .split(",\n       ")
            .map(|item| {
                let name = item.rsplit(" AS ").next().unwrap_or(item);
                name.trim_matches(['[', ']']).to_owned()
            })
            .collect()
    }

    /// The constant a literal select item writes, `None` for an item that reads its table.
    ///
    /// `CAST(0 AS bit)` gives `Some("0")`, `CAST(N'TABLE' AS nvarchar(60))` gives
    /// `Some("TABLE")`, `CAST(NULL AS int)` gives `Some("NULL")`.
    fn constant_of(expression: &str, name: &str) -> Option<String> {
        if expression == name {
            return None;
        }
        let inner = expression
            .strip_prefix("CAST(")
            .and_then(|rest| rest.rsplit_once(" AS "))
            .expect("a literal item is a CAST")
            .0;
        Some(inner.strip_prefix("N'").map_or_else(
            || inner.trim_matches('\'').to_owned(),
            |text| text.trim_end_matches('\'').to_owned(),
        ))
    }

    /// The rows of the internal table called `name`.
    fn rows_of(catalog: &Catalog, name: &str) -> Vec<Row> {
        let table = bootstrap::internal_table_id(catalog, name)
            .expect("internal_table_id")
            .expect("the table");
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = catalog.txn.statement_snapshot(&handle);
        let rows: Vec<Row> = catalog
            .storage
            .scan(&snapshot, table)
            .expect("scan")
            .map(|row| row.expect("row").1)
            .collect();
        catalog.txn.commit(handle).expect("commit");
        rows
    }

    /// A catalogue bootstrapped on a fresh `MemoryStorage`.
    fn bootstrapped() -> Catalog {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        Catalog::bootstrap(storage, txn).expect("bootstrap of a fresh storage")
    }

    /// A column of a [`TableDef`], with its type and its `IDENTITY` property.
    fn column_def(name: &str, ty: SqlType, nullable: bool, identity: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, nullable),
            default: None,
            identity: identity.then(IdentitySpec::default),
            computed: None,
        }
    }

    /// `master.dbo.<name>` with those columns.
    fn table_def(name: &str, columns: Vec<ColumnDef>) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns,
            constraints: Vec::new(),
        }
    }

    /// The tables `catalog` holds, once `def` has been created and committed.
    fn created(catalog: &Catalog, def: &TableDef) -> Vec<TableMeta> {
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        catalog.create_table(&handle, def).expect("create_table");
        catalog.txn.commit(handle).expect("commit");
        let store = table::store(catalog);
        store.live().cloned().collect()
    }

    /// A stored value as text: digits for a number, `1` or `0` for a `bit`, the text of a
    /// string, `None` for `NULL`.
    fn rendered(value: &Value) -> Option<String> {
        match value {
            Value::Null => None,
            Value::Bit(bit) => Some(i32::from(*bit).to_string()),
            Value::I8(number) => Some(number.to_string()),
            Value::I16(number) => Some(number.to_string()),
            Value::I32(number) => Some(number.to_string()),
            Value::String(string) => Some(string.text.clone()),
            other => panic!("no internal row carries {other:?}"),
        }
    }

    /// The value of the column `name` of a row of [`COLUMNS_TABLE`].
    fn column_field(row: &Row, name: &str) -> Option<String> {
        let position = columns_table_columns()
            .iter()
            .position(|column| column.name == name)
            .unwrap_or_else(|| panic!("{name} is a column of {COLUMNS_TABLE}"));
        rendered(&row.0[position])
    }

    #[test]
    fn sys_objects_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("objects")),
            PUBLISHED_OBJECTS_COLUMNS
        );
    }

    #[test]
    fn sys_tables_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("tables")),
            PUBLISHED_TABLES_COLUMNS
        );
    }

    #[test]
    fn sys_columns_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("columns")),
            PUBLISHED_COLUMNS_COLUMNS
        );
    }

    #[test]
    fn view_definition_is_a_select_without_a_join() {
        for (name, definition) in definitions() {
            assert!(
                definition.starts_with("SELECT "),
                "sys.{name}: {definition}"
            );
            assert!(
                !definition.to_ascii_uppercase().contains("JOIN"),
                "sys.{name} reads one internal table: {definition}"
            );
            assert!(
                definition.contains("\n  FROM master.dbo.vauban_sys_"),
                "sys.{name} reads an internal table of master: {definition}"
            );
            assert!(
                definition.contains("\n WHERE database_id = DB_ID()"),
                "sys.{name} is a per-database view: {definition}"
            );
        }
    }

    #[test]
    fn sys_tables_keeps_the_user_tables_of_the_objects_table() {
        // The two views read the same internal table; `sys.tables` adds the filter on the
        // `type` of a user table, so a row of another type is out of it.
        let tables = definition_of("tables");
        assert!(tables.ends_with("\n   AND type = 'U '"), "{tables}");
        assert!(!definition_of("objects").contains("AND type"));
        assert_eq!(USER_TABLE_TYPE, "U ");
    }

    #[test]
    fn a_reserved_column_name_is_delimited() {
        // `precision` is a reserved word of T-SQL, `scale` is not; the reading of
        // `published_columns` strips the brackets again.
        let columns = definition_of("columns");
        assert!(columns.contains("[precision],"), "{columns}");
        assert!(columns.contains("scale,"), "{columns}");
        assert_eq!(identifier("precision"), "[precision]");
        assert_eq!(identifier("scale"), "scale");
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        for table in internal_tables() {
            for name in ["objects", "tables", "columns"] {
                let installed: Vec<String> = table
                    .views
                    .iter()
                    .filter(|view| view.name.name == name)
                    .map(|view| view.name.database.clone())
                    .collect();
                if installed.is_empty() {
                    continue;
                }
                assert_eq!(installed, SYSTEM_DATABASES, "sys.{name}");
            }
            for view in &table.views {
                assert_eq!(view.name.schema, VIEW_SCHEMA);
            }
        }
        assert_eq!(
            internal_tables()
                .iter()
                .map(|table| table.views.len())
                .sum::<usize>(),
            12,
            "three views in four databases"
        );
    }

    #[test]
    fn the_column_order_is_the_one_the_constants_name() {
        let objects = objects_table_columns();
        assert_eq!(objects.len(), objects_columns::WIDTH);
        assert_eq!(objects[objects_columns::DATABASE_ID].name, "database_id");
        assert_eq!(objects[objects_columns::OBJECT_ID].name, "object_id");
        assert_eq!(objects[objects_columns::NAME].name, "name");
        assert_eq!(objects[objects_columns::SCHEMA_ID].name, "schema_id");
        assert_eq!(
            objects[objects_columns::PARENT_OBJECT_ID].name,
            "parent_object_id"
        );
        assert_eq!(objects[objects_columns::TYPE].name, "type");
        assert_eq!(objects[objects_columns::TYPE_DESC].name, "type_desc");
        assert_eq!(
            objects[objects_columns::IS_MS_SHIPPED].name,
            "is_ms_shipped"
        );
        assert_eq!(
            objects[objects_columns::MAX_COLUMN_ID_USED].name,
            "max_column_id_used"
        );
        let columns = columns_table_columns();
        assert_eq!(columns.len(), columns_columns::WIDTH);
        assert_eq!(columns[columns_columns::DATABASE_ID].name, "database_id");
        assert_eq!(columns[columns_columns::OBJECT_ID].name, "object_id");
        assert_eq!(columns[columns_columns::NAME].name, "name");
        assert_eq!(columns[columns_columns::COLUMN_ID].name, "column_id");
        assert_eq!(
            columns[columns_columns::SYSTEM_TYPE_ID].name,
            "system_type_id"
        );
        assert_eq!(columns[columns_columns::USER_TYPE_ID].name, "user_type_id");
        assert_eq!(columns[columns_columns::MAX_LENGTH].name, "max_length");
        assert_eq!(columns[columns_columns::PRECISION].name, "precision");
        assert_eq!(columns[columns_columns::SCALE].name, "scale");
        assert_eq!(
            columns[columns_columns::COLLATION_NAME].name,
            "collation_name"
        );
        assert_eq!(columns[columns_columns::IS_NULLABLE].name, "is_nullable");
        assert_eq!(
            columns[columns_columns::IS_ANSI_PADDED].name,
            "is_ansi_padded"
        );
        assert_eq!(columns[columns_columns::IS_IDENTITY].name, "is_identity");
        assert_eq!(columns[columns_columns::IS_COMPUTED].name, "is_computed");
    }

    #[test]
    fn the_columns_written_null_are_the_ones_with_no_datum_behind_them() {
        // `principal_id` (principals are not served), `create_date` and `modify_date` (the
        // catalogue holds no instant, as for `sys.databases`), then the features not served:
        // filestream, temporal history, ledger, encryption, graph.
        let nulls = |items: &[(&str, &str)]| -> Vec<String> {
            items
                .iter()
                .filter(|(_, expression)| expression.starts_with("CAST(NULL"))
                .map(|(name, _)| (*name).to_owned())
                .collect()
        };
        assert_eq!(nulls(&OBJECTS_VIEW), ["principal_id"]);
        assert_eq!(
            nulls(&TABLES_VIEW),
            [
                "principal_id",
                "filestream_data_space_id",
                "history_table_id",
                "history_retention_period",
                "history_retention_period_unit",
                "history_retention_period_unit_desc",
                "ledger_view_id",
            ]
        );
        assert_eq!(
            nulls(&COLUMNS_VIEW),
            [
                "encryption_type",
                "encryption_type_desc",
                "encryption_algorithm_name",
                "column_encryption_key_id",
                "column_encryption_key_database_name",
                "graph_type",
                "graph_type_desc",
                "ledger_view_column_type",
                "ledger_view_column_type_desc",
            ]
        );
    }

    #[test]
    fn the_literal_columns_are_the_published_values() {
        // The two instants are the gap the module documentation states: the catalogue writes
        // `NULL` where SQL Server writes the moment of the `CREATE`.
        let undated = ["create_date", "modify_date"];
        let mut compared = 0;
        for (items, published) in [
            (&OBJECTS_VIEW[..], &EXPECTED_OBJECTS_ROW[..]),
            (&TABLES_VIEW[..], &EXPECTED_TABLES_ROW[..]),
            (&COLUMNS_VIEW[..], &EXPECTED_COLUMNS_ROW[..]),
        ] {
            for (name, expression) in items {
                let Some(constant) = constant_of(expression, name) else {
                    continue;
                };
                if undated.contains(name) {
                    assert_eq!(constant, "NULL");
                    continue;
                }
                let (_, value) = published
                    .iter()
                    .find(|(column, _)| column == name)
                    .unwrap_or_else(|| panic!("{name} is a column of the view"));
                let written = (constant != "NULL").then_some(constant.as_str());
                assert_eq!(written, *value, "{name}");
                compared += 1;
            }
        }
        // 5 literals of `sys.objects` outside the two instants, 40 of `sys.tables`, 27 of
        // `sys.columns`.
        assert_eq!(compared, 5 - 2 + 40 - 2 + 27);
    }

    #[test]
    fn the_type_facts_are_those_of_sys_columns() {
        for (ty, system_type_id, max_length, precision, scale, collated, padded) in
            EXPECTED_TYPE_FACTS
        {
            let facts = column_facts(&TypeInfo::new(ty, true)).expect("a declared type");
            assert_eq!(
                facts,
                ColumnFacts {
                    system_type_id,
                    max_length,
                    precision,
                    scale,
                    collated,
                    ansi_padded: padded,
                },
                "{}",
                ty.declaration()
            );
        }
        for (precision, max_length) in EXPECTED_DECIMAL_LENGTHS {
            assert_eq!(
                decimal_length(precision),
                max_length,
                "decimal({precision})"
            );
        }
    }

    #[test]
    fn a_length_wider_than_a_smallint_is_a_bug_rather_than_a_wrapped_number() {
        // No `CREATE TABLE` builds this type — `nvarchar(n)` stops at 4 000 — and the
        // conversion answers an error rather than a negative `max_length`, which is the
        // value `(max)` has.
        let wide = TypeInfo::new(SqlType::NVarChar(Len::Fixed(20_000)), true);
        let error = column_facts(&wide).expect_err("40 000 bytes do not fit in a smallint");
        assert!(format!("{error}").contains("max_length"), "{error}");
    }

    #[test]
    fn user_table_has_object_and_column_rows() {
        let catalog = bootstrapped();
        let tables = created(
            &catalog,
            &table_def(
                "t",
                vec![
                    column_def("a", SqlType::Int, false, true),
                    column_def("b", SqlType::NVarChar(Len::Fixed(20)), true, false),
                ],
            ),
        );
        assert_eq!(tables.len(), 1);
        let object_id = tables[0].id.0;

        let objects = object_rows(&tables).expect("the rows of the objects table");
        assert_eq!(objects.len(), 1);
        let row = &objects[0].0;
        assert_eq!(row.len(), objects_columns::WIDTH);
        assert_eq!(rendered(&row[objects_columns::NAME]).as_deref(), Some("t"));
        assert_eq!(
            rendered(&row[objects_columns::TYPE]).as_deref(),
            Some(USER_TABLE_TYPE)
        );
        assert_eq!(
            rendered(&row[objects_columns::TYPE_DESC]).as_deref(),
            Some(USER_TABLE_TYPE_DESC)
        );
        // `is_ms_shipped` 0 and `parent_object_id` 0 are the values of a user table.
        assert_eq!(
            rendered(&row[objects_columns::IS_MS_SHIPPED]).as_deref(),
            Some("0")
        );
        assert_eq!(
            rendered(&row[objects_columns::PARENT_OBJECT_ID]).as_deref(),
            Some("0")
        );
        // `dbo` is schema 1 (`bootstrap::SYSTEM_SCHEMAS`), `master` database 1.
        assert_eq!(
            rendered(&row[objects_columns::SCHEMA_ID]).as_deref(),
            Some("1")
        );
        assert_eq!(
            rendered(&row[objects_columns::DATABASE_ID]).as_deref(),
            Some("1")
        );
        assert_eq!(
            rendered(&row[objects_columns::MAX_COLUMN_ID_USED]).as_deref(),
            Some("2")
        );

        let columns = column_rows(&tables).expect("the rows of the columns table");
        assert_eq!(columns.len(), 2);
        assert_eq!(column_field(&columns[0], "name").as_deref(), Some("a"));
        assert_eq!(column_field(&columns[0], "column_id").as_deref(), Some("1"));
        assert_eq!(
            column_field(&columns[0], "is_identity").as_deref(),
            Some("1")
        );
        assert_eq!(column_field(&columns[1], "name").as_deref(), Some("b"));
        assert_eq!(column_field(&columns[1], "column_id").as_deref(), Some("2"));
        assert_eq!(
            column_field(&columns[1], "max_length").as_deref(),
            Some("40")
        );
        assert_eq!(
            column_field(&columns[1], "collation_name").as_deref(),
            Some(DEFAULT_COLLATION_NAME)
        );

        // The identifier is the same in the three views: the two of them read the same row
        // of `OBJECTS_TABLE`, and the rows of `COLUMNS_TABLE` carry it again.
        assert_eq!(
            rendered(&row[objects_columns::OBJECT_ID]),
            Some(object_id.to_string())
        );
        for column in &columns {
            assert_eq!(
                column_field(column, "object_id"),
                Some(object_id.to_string())
            );
        }
        for view in ["objects", "tables"] {
            assert!(definition_of(view).contains(&format!("\n  FROM master.dbo.{OBJECTS_TABLE}")));
        }
        assert!(definition_of("columns").contains(&format!("\n  FROM master.dbo.{COLUMNS_TABLE}")));
    }

    #[test]
    fn the_column_rows_of_a_five_column_table_carry_its_values() {
        // The five columns of `dbo.columns_target`, in declaration order.
        let catalog = bootstrapped();
        let tables = created(
            &catalog,
            &table_def(
                "columns_target",
                vec![
                    column_def("id", SqlType::Int, false, true),
                    column_def("label", SqlType::VarChar(Len::Fixed(30)), true, false),
                    column_def(
                        "amount",
                        SqlType::Decimal {
                            precision: 9,
                            scale: 2,
                        },
                        true,
                        false,
                    ),
                    column_def("note", SqlType::NVarChar(Len::Max), true, false),
                    column_def("made_on", SqlType::DateTime2(3), true, false),
                ],
            ),
        );
        let rows = column_rows(&tables).expect("the rows of the columns table");
        assert_eq!(rows.len(), EXPECTED_COLUMN_ROWS.len());
        for (row, expected) in rows.iter().zip(EXPECTED_COLUMN_ROWS) {
            for (name, value) in expected {
                assert_eq!(column_field(row, name).as_deref(), *value, "{name}");
            }
        }
    }

    #[test]
    fn internal_tables_are_system_tables_in_sys_objects() {
        let catalog = bootstrapped();
        let objects = rows_of(&catalog, OBJECTS_TABLE);
        assert!(!objects.is_empty());
        for row in &objects {
            assert_eq!(
                rendered(&row.0[objects_columns::TYPE]).as_deref(),
                Some(SYSTEM_TABLE_TYPE)
            );
            assert_eq!(
                rendered(&row.0[objects_columns::TYPE_DESC]).as_deref(),
                Some(SYSTEM_TABLE_TYPE_DESC)
            );
            assert_eq!(row.0[objects_columns::IS_MS_SHIPPED], Value::Bit(true));
        }
        let tables = created(
            &catalog,
            &table_def("t", vec![column_def("a", SqlType::Int, false, false)]),
        );
        let user = object_rows(&tables)
            .expect("the rows of the objects table")
            .into_iter()
            .map(|row| rendered(&row.0[objects_columns::NAME]))
            .collect::<Vec<_>>();
        assert_eq!(user, vec![Some("t".to_owned())]);
    }

    #[test]
    fn a_schema_the_bootstrap_does_not_know_has_no_identifier() {
        assert_eq!(schema_id("dbo"), 1);
        assert_eq!(schema_id("DBO"), 1);
        assert_eq!(schema_id("sys"), 4);
        assert_eq!(schema_id("INFORMATION_SCHEMA"), 3);
        assert_eq!(schema_id("sales"), UNKNOWN_SCHEMA_ID);
        assert!(
            SYSTEM_SCHEMAS
                .iter()
                .all(|(_, id, _)| *id != UNKNOWN_SCHEMA_ID)
        );
    }
}
