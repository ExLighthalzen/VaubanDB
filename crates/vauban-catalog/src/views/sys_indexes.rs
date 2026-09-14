//! `sys.indexes`, `sys.index_columns`, `sys.key_constraints` and `sys.identity_columns`.
//!
//! # The shape of the views
//!
//! The column list of each of the four views — names, order, types — is the one SQL Server
//! 2022 publishes: 23, 9, 15 and 44 columns. The unit tests below freeze the four vectors of
//! names and compare them with the text this file builds
//! (`sys_indexes_columns_match_the_published_list` and its three neighbours). Three columns
//! of `sys.identity_columns` are `sql_variant` in SQL Server; see the bounds below.
//!
//! # Four internal tables for four views
//!
//! A view is a `SELECT` over one denormalised internal table, without a join.
//! [`INDEXES_TABLE`] holds one row per index **and one per heap**, [`INDEX_COLUMNS_TABLE`]
//! one row per key column of an index, [`KEY_CONSTRAINTS_TABLE`] one row per `PRIMARY KEY`
//! or `UNIQUE` constraint and [`IDENTITY_COLUMNS_TABLE`] one row per `IDENTITY` column. The
//! internal tables live in `master` while the four views are per-database, so each
//! definition filters on `database_id = DB_ID()`, as `sys.schemas` (`views/sys_core.rs`)
//! and `sys.objects` (`views/sys_tables.rs`) do.
//!
//! # Read columns and literal columns
//!
//! Same two rules as `views/sys_core.rs` and `views/sys_tables.rs`: a value the catalogue
//! does not store is `CAST(NULL AS …)`, a value SQL Server publishes the same way for each
//! user object is written as that constant. The unit test
//! `the_literal_columns_are_the_published_values` compares each literal with the row it
//! comes from, and `the_columns_written_null_are_the_ones_with_no_datum_behind_them` names
//! the `NULL`s.
//!
//! # What SQL Server publishes
//!
//! - a table without a clustered key carries a row of `index_id` 0, `type` 0, `type_desc`
//!   `HEAP`, `name` `NULL`, and its `data_space_id`, `allow_row_locks` and
//!   `allow_page_locks` are those of a keyed index, so the heap row and an index row of one
//!   table differ on the columns this file stores and on no literal (unit test
//!   `a_heap_row_differs_from_an_index_row_on_six_columns`);
//! - `is_ignored_in_optimization` is 0, `compression_delay` `NULL` and
//!   `suppress_dup_key_messages` 0 on a clustered index, a nonclustered one and a heap;
//! - `column_store_order_ordinal` of `sys.index_columns` is 0 on a clustered key and on a
//!   two-column nonclustered key;
//! - `index_column_id` of `sys.index_columns` follows the `key_ordinal` on a nonclustered
//!   index and the `column_id` order on a clustered one, which [`published_key_columns`]
//!   reports;
//! - `sys.key_constraints` publishes `principal_id` `NULL`, `is_published` 0,
//!   `is_schema_published` 0, `is_enforced` 1, a `create_date` that is not `NULL`, a
//!   `schema_id` equal to `SCHEMA_ID('dbo')` and an `object_id` distinct from
//!   `parent_object_id`, on a named `PRIMARY KEY` and on an unnamed `UNIQUE` of one table;
//! - the index that backs a key constraint carries the **name** of the constraint, for a
//!   name the statement wrote as for one the server made, which is why
//!   [`key_constraint_rows`] publishes the name of the index as the name of the constraint;
//! - `last_value` of an `IDENTITY` that never handed a value out is `NULL`, not seed minus
//!   increment: `CREATE TABLE … (id int IDENTITY(7,3) NOT NULL)` then
//!   `SELECT last_value FROM sys.identity_columns` is `NULL` while `seed_value` is 7 and
//!   `increment_value` 3, no `INSERT` being run;
//! - `system_type_id`, `max_length`, `precision`, `scale`, `is_nullable`, `is_ansi_padded`
//!   and `collation_name` of the seven numeric `IDENTITY` columns [`identity_facts`] serves
//!   (`tinyint`, `smallint`, `int`, `bigint`, `decimal(9,0)`, `decimal(38,0)`,
//!   `numeric(18,0)`), which the unit test `the_identity_type_facts_are_the_published_ones`
//!   restates;
//! - an `IDENTITY` on a `varchar(10)` column is error 2749, state 2. That number is not in
//!   `vauban-errors`, so [`identity_facts`] answers an [`InternalError::Bug`] naming it, as
//!   `index.rs` does for its own eight numbers.
//!
//! # Three bounds this file states
//!
//! 1. `sys.key_constraints.object_id` is `CAST(NULL AS int)`: a key constraint has no
//!    `ObjectId` — [`ConstraintMeta`] carries the [`IndexId`](vauban_storage::IndexId) of
//!    its index and the name lives on that index — where SQL Server publishes an identifier
//!    distinct from `parent_object_id`. Same treatment as the `create_date` of
//!    `views/sys_tables.rs`.
//! 2. `seed_value`, `increment_value` and `last_value` are published as `bigint`, where
//!    SQL Server publishes `sql_variant`: [`SqlType`] has no `sql_variant`, and `bigint` is
//!    the width [`IdentitySpec`](crate::IdentitySpec) stores. `last_value` is
//!    `CAST(NULL AS bigint)`: the counter lives in a table of its own, which `identity.rs`
//!    creates at the first call.
//! 3. `is_system_named` is read from the **shape** of the name ([`is_system_named`]), the
//!    catalogue storing no flag for it: a constraint a client names `PK__` plus the first
//!    eight characters of its table plus `__` plus sixteen hexadecimal digits is published as
//!    system-named (unit test `a_written_name_of_the_generated_shape_is_read_as_generated`).
//!
//! # Execution
//!
//! What this file produces is the shape of the four internal tables, the text of the four
//! definitions and the four functions that turn the `*Meta` of the catalogue into rows;
//! `sys_rows.rs` writes those rows into `storage` at each DDL, and the binder expands the
//! text of a view in place of its name.

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::{DbId, IndexId, Row};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{SYSTEM_DATABASES, SYSTEM_SCHEMAS};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::index::IndexStore;
use crate::meta::{ConstraintMeta, IndexMeta, QualifiedName, TableMeta};

/// Internal table of the indexes and of the heaps, read by `sys.indexes`.
///
/// A `vauban_sys_*` name of our own; what a client reads is the view built over it.
pub(crate) const INDEXES_TABLE: &str = "vauban_sys_indexes";

/// Internal table of the key columns of the indexes, read by `sys.index_columns`.
pub(crate) const INDEX_COLUMNS_TABLE: &str = "vauban_sys_index_columns";

/// Internal table of the `PRIMARY KEY` and `UNIQUE` constraints, read by
/// `sys.key_constraints`.
pub(crate) const KEY_CONSTRAINTS_TABLE: &str = "vauban_sys_key_constraints";

/// Internal table of the `IDENTITY` columns, read by `sys.identity_columns`.
pub(crate) const IDENTITY_COLUMNS_TABLE: &str = "vauban_sys_identity_columns";

/// The schema the four views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The column names of the four views that are T-SQL reserved words and are therefore written
/// as delimited identifiers in the text of a view: `precision`, a column of
/// `sys.identity_columns` as it is one of `sys.columns` (`views/sys_tables.rs`). Unit test
/// `a_reserved_column_name_is_delimited`.
const RESERVED_COLUMN_NAMES: [&str; 1] = ["precision"];

/// `sys.indexes.index_id` and `type` of the heap row of a table without a clustered key:
/// 0 both, `name` `NULL`, `type_desc` [`HEAP_TYPE_DESC`].
const HEAP_INDEX_ID: i32 = 0;

/// `sys.indexes.type` of a heap. See [`HEAP_INDEX_ID`].
const HEAP_TYPE: u8 = 0;

/// `sys.indexes.type_desc` of a heap. See [`HEAP_INDEX_ID`].
const HEAP_TYPE_DESC: &str = "HEAP";

/// `sys.indexes.index_id` of the index that carries the clustered key: 1, the nonclustered
/// indexes taking 2 and up.
const CLUSTERED_INDEX_ID: i32 = 1;

/// `sys.indexes.index_id` of the first nonclustered index of a table.
const FIRST_NONCLUSTERED_INDEX_ID: i32 = 2;

/// `sys.indexes.type` of the index that carries the clustered key.
const CLUSTERED_TYPE: u8 = 1;

/// `sys.indexes.type_desc` that goes with [`CLUSTERED_TYPE`].
const CLUSTERED_TYPE_DESC: &str = "CLUSTERED";

/// `sys.indexes.type` of an index that does not carry the clustered key.
const NONCLUSTERED_TYPE: u8 = 2;

/// `sys.indexes.type_desc` that goes with [`NONCLUSTERED_TYPE`].
const NONCLUSTERED_TYPE_DESC: &str = "NONCLUSTERED";

/// `sys.key_constraints.type` of a `PRIMARY KEY`: the column is a `char(2)`, which `PK`
/// fills.
const PRIMARY_KEY_TYPE: &str = "PK";

/// `sys.key_constraints.type_desc` that goes with [`PRIMARY_KEY_TYPE`].
const PRIMARY_KEY_TYPE_DESC: &str = "PRIMARY_KEY_CONSTRAINT";

/// `sys.key_constraints.type` of a `UNIQUE` constraint.
const UNIQUE_TYPE: &str = "UQ";

/// `sys.key_constraints.type_desc` that goes with [`UNIQUE_TYPE`].
const UNIQUE_TYPE_DESC: &str = "UNIQUE_CONSTRAINT";

/// How many hexadecimal digits close a constraint name the server makes, `index.rs` building
/// `PK__my_table__3BD0198E1CED331D`. Read by [`is_system_named`].
const GENERATED_NAME_DIGITS: usize = 16;

/// How many characters of the table name a generated constraint name carries, between its two
/// pairs of underscores. Read by [`is_system_named`].
const GENERATED_NAME_HEAD: usize = 8;

/// The `schema_id` written for a schema the bootstrap does not know, as `views/sys_tables.rs`
/// writes it on `sys.objects`: `0`, which is not one of the identifiers SQL Server hands out (`dbo` 1,
/// `INFORMATION_SCHEMA` 3, `sys` 4, `bootstrap.rs`).
const UNKNOWN_SCHEMA_ID: i32 = 0;

/// Where each column of [`INDEXES_TABLE`] sits in a [`Row`], as `bootstrap.rs` does for its
/// own tables: a writer of a row addresses it through these constants rather than
/// restating the order (unit test `the_column_order_is_the_one_the_constants_name`).
pub(crate) mod indexes_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table the index is built on.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`, nullable: the name of the index, `NULL` for a heap.
    pub(crate) const NAME: usize = 2;
    /// `index_id int`: 0 for a heap, 1 for the clustered index, 2 and up after that.
    pub(crate) const INDEX_ID: usize = 3;
    /// `type tinyint`: 0 heap, 1 clustered, 2 nonclustered.
    pub(crate) const TYPE: usize = 4;
    /// `type_desc nvarchar(60)`: `HEAP`, `CLUSTERED` or `NONCLUSTERED`.
    pub(crate) const TYPE_DESC: usize = 5;
    /// `is_unique bit`.
    pub(crate) const IS_UNIQUE: usize = 6;
    /// `is_primary_key bit`.
    pub(crate) const IS_PRIMARY_KEY: usize = 7;
    /// `is_unique_constraint bit`.
    pub(crate) const IS_UNIQUE_CONSTRAINT: usize = 8;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 9;
}

/// Where each column of [`INDEX_COLUMNS_TABLE`] sits in a [`Row`]. Same rule as
/// [`indexes_columns`].
pub(crate) mod index_columns_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table the index is built on.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `index_id int`: the index, numbered as [`super::indexes_columns::INDEX_ID`] is.
    pub(crate) const INDEX_ID: usize = 2;
    /// `index_column_id int`: the position of the column in the index, from 1.
    pub(crate) const INDEX_COLUMN_ID: usize = 3;
    /// `column_id int`: the column of the table, as `sys.columns` numbers it.
    pub(crate) const COLUMN_ID: usize = 4;
    /// `key_ordinal tinyint`: the position of the column in the key, from 1.
    pub(crate) const KEY_ORDINAL: usize = 5;
    /// `is_descending_key bit`.
    pub(crate) const IS_DESCENDING_KEY: usize = 6;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 7;
}

/// Where each column of [`KEY_CONSTRAINTS_TABLE`] sits in a [`Row`]. Same rule as
/// [`indexes_columns`].
pub(crate) mod key_constraints_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `name nvarchar(128)`: the name of the constraint, which is that of its index.
    pub(crate) const NAME: usize = 1;
    /// `schema_id int`: the schema of the table the constraint is declared on.
    pub(crate) const SCHEMA_ID: usize = 2;
    /// `parent_object_id int`: the table the constraint is declared on.
    pub(crate) const PARENT_OBJECT_ID: usize = 3;
    /// `type char(2)`: `PK` or `UQ`.
    pub(crate) const TYPE: usize = 4;
    /// `type_desc nvarchar(60)`: `PRIMARY_KEY_CONSTRAINT` or `UNIQUE_CONSTRAINT`.
    pub(crate) const TYPE_DESC: usize = 5;
    /// `unique_index_id int`: the `index_id` of the index that enforces the constraint.
    pub(crate) const UNIQUE_INDEX_ID: usize = 6;
    /// `is_system_named bit`: see [`super::is_system_named`].
    pub(crate) const IS_SYSTEM_NAMED: usize = 7;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 8;
}

/// Where each column of [`IDENTITY_COLUMNS_TABLE`] sits in a [`Row`]. Same rule as
/// [`indexes_columns`].
pub(crate) mod identity_columns_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table the column belongs to.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the column.
    pub(crate) const NAME: usize = 2;
    /// `column_id int`: the identifier of the column within its table, from 1.
    pub(crate) const COLUMN_ID: usize = 3;
    /// `system_type_id tinyint`.
    pub(crate) const SYSTEM_TYPE_ID: usize = 4;
    /// `user_type_id int`: equal to `system_type_id` on the seven types served.
    pub(crate) const USER_TYPE_ID: usize = 5;
    /// `max_length smallint`: bytes.
    pub(crate) const MAX_LENGTH: usize = 6;
    /// `precision tinyint`.
    pub(crate) const PRECISION: usize = 7;
    /// `scale tinyint`.
    pub(crate) const SCALE: usize = 8;
    /// `is_nullable bit`.
    pub(crate) const IS_NULLABLE: usize = 9;
    /// `seed_value bigint`: the first value handed out.
    pub(crate) const SEED_VALUE: usize = 10;
    /// `increment_value bigint`: what the next value adds to the last one.
    pub(crate) const INCREMENT_VALUE: usize = 11;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 12;
}

/// The select list of `sys.indexes`: `(column, expression)`, in the published order.
///
/// An expression equal to the column name reads [`INDEXES_TABLE`]; the others are the literals
/// the module documentation explains.
const INDEXES_VIEW: [(&str, &str); 23] = [
    ("object_id", "object_id"),
    ("name", "name"),
    ("index_id", "index_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("is_unique", "is_unique"),
    ("data_space_id", "CAST(1 AS int)"),
    ("ignore_dup_key", "CAST(0 AS bit)"),
    ("is_primary_key", "is_primary_key"),
    ("is_unique_constraint", "is_unique_constraint"),
    ("fill_factor", "CAST(0 AS tinyint)"),
    ("is_padded", "CAST(0 AS bit)"),
    ("is_disabled", "CAST(0 AS bit)"),
    ("is_hypothetical", "CAST(0 AS bit)"),
    ("is_ignored_in_optimization", "CAST(0 AS bit)"),
    ("allow_row_locks", "CAST(1 AS bit)"),
    ("allow_page_locks", "CAST(1 AS bit)"),
    ("has_filter", "CAST(0 AS bit)"),
    ("filter_definition", "CAST(NULL AS nvarchar(max))"),
    ("compression_delay", "CAST(NULL AS int)"),
    ("suppress_dup_key_messages", "CAST(0 AS bit)"),
    ("auto_created", "CAST(0 AS bit)"),
    ("optimize_for_sequential_key", "CAST(0 AS bit)"),
];

/// The select list of `sys.index_columns`: its 9 columns, 6 of which read
/// [`INDEX_COLUMNS_TABLE`].
///
/// `is_included_column` is a constant 0 because an `INCLUDE` list is not served:
/// [`IndexMeta`] has no field for an included column.
const INDEX_COLUMNS_VIEW: [(&str, &str); 9] = [
    ("object_id", "object_id"),
    ("index_id", "index_id"),
    ("index_column_id", "index_column_id"),
    ("column_id", "column_id"),
    ("key_ordinal", "key_ordinal"),
    ("partition_ordinal", "CAST(0 AS tinyint)"),
    ("is_descending_key", "is_descending_key"),
    ("is_included_column", "CAST(0 AS bit)"),
    ("column_store_order_ordinal", "CAST(0 AS tinyint)"),
];

/// The select list of `sys.key_constraints`: its 15 columns, 7 of which read
/// [`KEY_CONSTRAINTS_TABLE`].
///
/// `object_id` is `NULL` for the reason the module documentation gives (bound 1).
const KEY_CONSTRAINTS_VIEW: [(&str, &str); 15] = [
    ("name", "name"),
    ("object_id", "CAST(NULL AS int)"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("is_ms_shipped", "CAST(0 AS bit)"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
    ("unique_index_id", "unique_index_id"),
    ("is_system_named", "is_system_named"),
    ("is_enforced", "CAST(1 AS bit)"),
];

/// The select list of `sys.identity_columns`: its 44 columns, 11 of which read
/// [`IDENTITY_COLUMNS_TABLE`].
///
/// `collation_name` is `NULL` because an `IDENTITY` column carries one of the numeric types of
/// [`identity_facts`], which have no collation. `last_value` is `NULL` for the reason the
/// module documentation gives (bound 2). The 33 literals are the published values (unit test
/// `the_literal_columns_are_the_published_values`).
const IDENTITY_COLUMNS_VIEW: [(&str, &str); 44] = [
    ("object_id", "object_id"),
    ("name", "name"),
    ("column_id", "column_id"),
    ("system_type_id", "system_type_id"),
    ("user_type_id", "user_type_id"),
    ("max_length", "max_length"),
    ("precision", "precision"),
    ("scale", "scale"),
    ("collation_name", "CAST(NULL AS nvarchar(128))"),
    ("is_nullable", "is_nullable"),
    ("is_ansi_padded", "CAST(0 AS bit)"),
    ("is_rowguidcol", "CAST(0 AS bit)"),
    ("is_identity", "CAST(1 AS bit)"),
    ("is_filestream", "CAST(0 AS bit)"),
    ("is_replicated", "CAST(0 AS bit)"),
    ("is_non_sql_subscribed", "CAST(0 AS bit)"),
    ("is_merge_published", "CAST(0 AS bit)"),
    ("is_dts_replicated", "CAST(0 AS bit)"),
    ("is_xml_document", "CAST(0 AS bit)"),
    ("xml_collection_id", "CAST(0 AS int)"),
    ("default_object_id", "CAST(0 AS int)"),
    ("rule_object_id", "CAST(0 AS int)"),
    ("seed_value", "seed_value"),
    ("increment_value", "increment_value"),
    ("last_value", "CAST(NULL AS bigint)"),
    ("is_not_for_replication", "CAST(0 AS bit)"),
    ("is_computed", "CAST(0 AS bit)"),
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

/// The internal tables this file describes: the indexes, their columns, the key constraints
/// and the identity columns.
///
/// The bootstrap creates them in `master`. Their `rows` are empty: the rows of a user object
/// are built from the `*Meta` of the catalogue by [`index_rows`], [`index_column_rows`],
/// [`key_constraint_rows`] and [`identity_column_rows`] (module documentation, section
/// "Execution").
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        internal_table(
            INDEXES_TABLE,
            indexes_table_columns(),
            "indexes",
            &INDEXES_VIEW,
        ),
        internal_table(
            INDEX_COLUMNS_TABLE,
            index_columns_table_columns(),
            "index_columns",
            &INDEX_COLUMNS_VIEW,
        ),
        internal_table(
            KEY_CONSTRAINTS_TABLE,
            key_constraints_table_columns(),
            "key_constraints",
            &KEY_CONSTRAINTS_VIEW,
        ),
        internal_table(
            IDENTITY_COLUMNS_TABLE,
            identity_columns_table_columns(),
            "identity_columns",
            &IDENTITY_COLUMNS_VIEW,
        ),
    ]
}

/// One internal table with the single view built over it, in the four system databases.
fn internal_table(
    name: &str,
    columns: Vec<InternalColumnDef>,
    view: &str,
    items: &[(&str, &str)],
) -> InternalTableDef {
    InternalTableDef {
        name: name.to_owned(),
        columns,
        clustered_key: None,
        rows: Vec::new(),
        views: views_of(view, &definition(items, name)),
    }
}

/// One index of a table with the `index_id` the views publish for it.
///
/// The pair the row builders work from: [`numbered_indexes`] assigns the numbers once and the
/// builders read them, so `sys.indexes`, `sys.index_columns` and `sys.key_constraints` agree
/// on the `index_id` of one index (unit test `the_three_index_views_agree_on_the_index_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NumberedIndex<'a> {
    /// `index_id` as the views publish it: 1 for the clustered index, 2 and up after that.
    index_id: i32,
    /// What the catalogue knows about the index.
    meta: &'a IndexMeta,
}

/// The indexes of `table` with their `index_id`.
///
/// The clustered index takes [`CLUSTERED_INDEX_ID`] and the others take
/// [`FIRST_NONCLUSTERED_INDEX_ID`] and up in increasing [`IndexId`] order, which is the
/// numbering SQL Server shows (the clustered `PRIMARY KEY` at 1, the `CREATE INDEX` that
/// follows at 2). [`IndexStore::of_table`] already answers in that order.
fn numbered_indexes<'a>(table: &TableMeta, indexes: &'a IndexStore) -> Vec<NumberedIndex<'a>> {
    let mut next = FIRST_NONCLUSTERED_INDEX_ID;
    indexes
        .of_table(table.id)
        .into_iter()
        .map(|meta| {
            let index_id = if meta.clustered {
                CLUSTERED_INDEX_ID
            } else {
                let id = next;
                next += 1;
                id
            };
            NumberedIndex { index_id, meta }
        })
        .collect()
}

/// The rows of [`INDEXES_TABLE`] for `tables`: one per index, plus the heap row of a table
/// without a clustered key.
///
/// The caller passes the tables of one catalogue —
/// [`TableStore::live`](crate::table::TableStore::live) gives them in increasing
/// [`ObjectId`](crate::ObjectId) order — and the [`IndexStore`] that
/// sits beside them, refreshed (`index.rs`). The rows of one table come in increasing
/// `index_id` order, the heap row first (unit tests `heap_table_has_index_id_zero`,
/// `pk_is_index_id_one`).
///
/// # Errors
///
/// [`InternalError::Bug`] when a [`DbId`] does not fit in the `int` the view publishes, which
/// is the check `bootstrap.rs` makes on the same value.
pub(crate) fn index_rows(tables: &[TableMeta], indexes: &IndexStore) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        if table.clustered.is_none() {
            rows.push(heap_row(database, table));
        }
        for numbered in numbered_indexes(table, indexes) {
            rows.push(index_row(database, table, numbered));
        }
    }
    Ok(rows)
}

/// The heap row of a table without a clustered key. See [`index_rows`].
fn heap_row(database: i32, table: &TableMeta) -> Row {
    let mut row = vec![Value::Null; indexes_columns::WIDTH];
    row[indexes_columns::DATABASE_ID] = Value::I32(database);
    row[indexes_columns::OBJECT_ID] = Value::I32(table.id.0);
    row[indexes_columns::NAME] = Value::Null;
    row[indexes_columns::INDEX_ID] = Value::I32(HEAP_INDEX_ID);
    row[indexes_columns::TYPE] = Value::I8(HEAP_TYPE);
    row[indexes_columns::TYPE_DESC] = text(HEAP_TYPE_DESC);
    row[indexes_columns::IS_UNIQUE] = Value::Bit(false);
    row[indexes_columns::IS_PRIMARY_KEY] = Value::Bit(false);
    row[indexes_columns::IS_UNIQUE_CONSTRAINT] = Value::Bit(false);
    Row(row)
}

/// The row of [`INDEXES_TABLE`] of one index. See [`index_rows`].
fn index_row(database: i32, table: &TableMeta, numbered: NumberedIndex<'_>) -> Row {
    let (ty, type_desc) = if numbered.meta.clustered {
        (CLUSTERED_TYPE, CLUSTERED_TYPE_DESC)
    } else {
        (NONCLUSTERED_TYPE, NONCLUSTERED_TYPE_DESC)
    };
    let mut row = vec![Value::Null; indexes_columns::WIDTH];
    row[indexes_columns::DATABASE_ID] = Value::I32(database);
    row[indexes_columns::OBJECT_ID] = Value::I32(table.id.0);
    row[indexes_columns::NAME] = text(&numbered.meta.name);
    row[indexes_columns::INDEX_ID] = Value::I32(numbered.index_id);
    row[indexes_columns::TYPE] = Value::I8(ty);
    row[indexes_columns::TYPE_DESC] = text(type_desc);
    row[indexes_columns::IS_UNIQUE] = Value::Bit(numbered.meta.unique);
    row[indexes_columns::IS_PRIMARY_KEY] = Value::Bit(numbered.meta.primary_key);
    row[indexes_columns::IS_UNIQUE_CONSTRAINT] =
        Value::Bit(backs_a_unique_constraint(table, numbered.meta.id));
    Row(row)
}

/// Whether the index `index` enforces a `UNIQUE` constraint of `table`.
///
/// What tells a `UNIQUE` constraint from a `CREATE UNIQUE INDEX`: both give an [`IndexMeta`]
/// with `unique` set, and the first alone is named by a [`ConstraintMeta::Unique`]
/// (`tests::a_unique_constraint_and_a_unique_index_differ_on_one_column`).
fn backs_a_unique_constraint(table: &TableMeta, index: IndexId) -> bool {
    table
        .constraints
        .iter()
        .any(|constraint| matches!(constraint, ConstraintMeta::Unique(id) if *id == index))
}

/// One key column of an index as `sys.index_columns` publishes it.
///
/// `index_column_id` and `key_ordinal` are **two** numbers, and they part company on a
/// clustered index: see [`published_key_columns`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublishedKeyColumn {
    /// `sys.index_columns.index_column_id`.
    index_column_id: i32,
    /// `sys.index_columns.column_id`, the column as `sys.columns` numbers it.
    column_id: i32,
    /// `sys.index_columns.key_ordinal`, the rank of the column in the key, from 1.
    key_ordinal: u8,
    /// `sys.index_columns.is_descending_key`.
    descending: bool,
}

/// The key columns of `index` as `sys.index_columns` publishes them, in `index_column_id`
/// order.
///
/// # The two numberings
///
/// On a **nonclustered** index, `index_column_id` is the `key_ordinal`: the keys
/// `(c, b DESC)` and `(d, a)` of a table `(a, b, c, d)` read `index_column_id` 1 then 2
/// against `column_id` 3 then 2, and 4 then 1; so does `(c, a DESC)` on a table that already
/// carries a clustered key.
///
/// On a **clustered** index, `index_column_id` is the rank of the column in increasing
/// `column_id` order and not the `key_ordinal`: `PRIMARY KEY CLUSTERED (d, b DESC, c)` on
/// `(a, b, c, d)` reads `index_column_id` 1, 2, 3 against `column_id` 2, 3, 4 and
/// `key_ordinal` 2, 3, 1; the pair `(c, a)` reads `column_id` 1 then 3 with `key_ordinal` 2
/// then 1, and `UNIQUE CLUSTERED (c, b)` reads `column_id` 2 then 3 with `key_ordinal` 2
/// then 1. Frozen by `tests::a_clustered_key_out_of_order_numbers_its_columns_by_column_id`
/// and `tests::a_nonclustered_key_out_of_order_numbers_its_columns_by_key_ordinal`, which
/// fail on each other's rule.
///
/// # Errors
///
/// [`InternalError::Bug`] when a key names a position that is not a column of the table, or
/// when a key holds more columns than the `tinyint` `key_ordinal` publishes.
fn published_key_columns(
    table: &TableMeta,
    index: &IndexMeta,
) -> SqlResult<Vec<PublishedKeyColumn>> {
    let mut keys = Vec::with_capacity(index.columns.len());
    for (position, key) in index.columns.iter().enumerate() {
        let key_ordinal = u8::try_from(position + 1).map_err(|_| {
            InternalError::Bug(format!(
                "sys.index_columns: index {} holds more than 255 key columns, which the tinyint \
                 key_ordinal cannot publish",
                index.name
            ))
        })?;
        let column = table
            .columns
            .iter()
            .find(|column| column.ordinal == key.column)
            .ok_or_else(|| {
                InternalError::Bug(format!(
                    "sys.index_columns: index {} keys on the position {} of table {}, which holds \
                     no column there",
                    index.name, key.column, table.name
                ))
            })?;
        keys.push(PublishedKeyColumn {
            // Replaced below for a clustered index.
            index_column_id: i32::from(key_ordinal),
            column_id: column.id.0,
            key_ordinal,
            descending: key.descending,
        });
    }
    if index.clustered {
        // The rank in increasing `column_id` order. A key lists a column once — `index.rs`
        // answers 1909 otherwise — so the ranks are a permutation of
        // `1..=keys.len()`.
        keys.sort_by_key(|key| key.column_id);
        for (position, key) in keys.iter_mut().enumerate() {
            key.index_column_id = i32::try_from(position + 1).unwrap_or(i32::MAX);
        }
    }
    keys.sort_by_key(|key| key.index_column_id);
    Ok(keys)
}

/// The rows of [`INDEX_COLUMNS_TABLE`] for `tables`: one per key column of each index, in
/// `index_column_id` order within each index.
///
/// A heap has no row here: `sys.index_columns` lists the indexes of a table and no
/// `index_id` 0, which the unit test `a_heap_has_no_index_column_row` states.
///
/// # Errors
///
/// Those of [`index_rows`] and of [`published_key_columns`].
pub(crate) fn index_column_rows(tables: &[TableMeta], indexes: &IndexStore) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for numbered in numbered_indexes(table, indexes) {
            for key in published_key_columns(table, numbered.meta)? {
                let mut row = vec![Value::Null; index_columns_columns::WIDTH];
                row[index_columns_columns::DATABASE_ID] = Value::I32(database);
                row[index_columns_columns::OBJECT_ID] = Value::I32(table.id.0);
                row[index_columns_columns::INDEX_ID] = Value::I32(numbered.index_id);
                row[index_columns_columns::INDEX_COLUMN_ID] = Value::I32(key.index_column_id);
                row[index_columns_columns::COLUMN_ID] = Value::I32(key.column_id);
                row[index_columns_columns::KEY_ORDINAL] = Value::I8(key.key_ordinal);
                row[index_columns_columns::IS_DESCENDING_KEY] = Value::Bit(key.descending);
                rows.push(Row(row));
            }
        }
    }
    Ok(rows)
}

/// The rows of [`KEY_CONSTRAINTS_TABLE`] for `tables`: one per `PRIMARY KEY` or `UNIQUE`
/// constraint, in the declaration order [`TableMeta::constraints`] keeps.
///
/// The name published is that of the index the constraint is backed by, SQL Server giving
/// the index and the constraint one name (module documentation). A `FOREIGN KEY`, a `CHECK`
/// or a `DEFAULT` is skipped: SQL Server keeps this view to the `PK` and `UQ` types, and
/// `views/sys_constraints.rs` owns their views.
///
/// # Errors
///
/// Those of [`index_rows`], plus [`InternalError::Bug`] when a constraint names an index the
/// [`IndexStore`] does not hold.
pub(crate) fn key_constraint_rows(
    tables: &[TableMeta],
    indexes: &IndexStore,
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        let numbered = numbered_indexes(table, indexes);
        for constraint in &table.constraints {
            let (index, primary_key) = match constraint {
                ConstraintMeta::PrimaryKey(id) => (*id, true),
                ConstraintMeta::Unique(id) => (*id, false),
                ConstraintMeta::ForeignKey { .. }
                | ConstraintMeta::Check { .. }
                | ConstraintMeta::Default { .. } => continue,
            };
            let found = numbered
                .iter()
                .find(|candidate| candidate.meta.id == index)
                .ok_or_else(|| {
                    InternalError::Bug(format!(
                        "sys.key_constraints: a constraint of table {} is backed by the index \
                         {index}, which the catalogue does not hold",
                        table.name
                    ))
                })?;
            let (ty, type_desc) = if primary_key {
                (PRIMARY_KEY_TYPE, PRIMARY_KEY_TYPE_DESC)
            } else {
                (UNIQUE_TYPE, UNIQUE_TYPE_DESC)
            };
            let mut row = vec![Value::Null; key_constraints_columns::WIDTH];
            row[key_constraints_columns::DATABASE_ID] = Value::I32(database);
            row[key_constraints_columns::NAME] = text(&found.meta.name);
            row[key_constraints_columns::SCHEMA_ID] = Value::I32(schema_id(&table.schema));
            row[key_constraints_columns::PARENT_OBJECT_ID] = Value::I32(table.id.0);
            row[key_constraints_columns::TYPE] = text(ty);
            row[key_constraints_columns::TYPE_DESC] = text(type_desc);
            row[key_constraints_columns::UNIQUE_INDEX_ID] = Value::I32(found.index_id);
            row[key_constraints_columns::IS_SYSTEM_NAMED] =
                Value::Bit(is_system_named(&found.meta.name, &table.name, primary_key));
            rows.push(Row(row));
        }
    }
    Ok(rows)
}

/// Whether `name` has the shape `index.rs` gives a constraint the statement did not name:
/// `PK` or `UQ`, two underscores, the first [`GENERATED_NAME_HEAD`] characters of the table
/// name, two underscores, [`GENERATED_NAME_DIGITS`] upper-case hexadecimal digits.
///
/// The catalogue stores no flag for `sys.key_constraints.is_system_named`, so the column is
/// read from the name; bound 3 of the module documentation. A generated
/// `UQ__my_table__4823FDB2B2946FCA` reads 1 and a written `pk_names` reads 0 (unit tests
/// `a_generated_constraint_name_is_read_as_generated`,
/// `a_written_name_of_the_generated_shape_is_read_as_generated`).
fn is_system_named(name: &str, table: &str, primary_key: bool) -> bool {
    let prefix = if primary_key {
        PRIMARY_KEY_TYPE
    } else {
        UNIQUE_TYPE
    };
    let head: String = table.chars().take(GENERATED_NAME_HEAD).collect();
    let Some(digits) = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix("__"))
        .and_then(|rest| rest.strip_prefix(head.as_str()))
        .and_then(|rest| rest.strip_prefix("__"))
    else {
        return false;
    };
    digits.len() == GENERATED_NAME_DIGITS
        && digits
            .chars()
            .all(|digit| digit.is_ascii_digit() || ('A'..='F').contains(&digit))
}

/// The rows of [`IDENTITY_COLUMNS_TABLE`] for `tables`: one per `IDENTITY` column, in
/// `column_id` order within each table.
///
/// # Errors
///
/// Those of [`index_rows`], plus the error of [`identity_facts`] on the type of the column.
pub(crate) fn identity_column_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for column in &table.columns {
            let Some(identity) = column.identity else {
                continue;
            };
            let facts = identity_facts(&column.name, &column.ty)?;
            let mut row = vec![Value::Null; identity_columns_columns::WIDTH];
            row[identity_columns_columns::DATABASE_ID] = Value::I32(database);
            row[identity_columns_columns::OBJECT_ID] = Value::I32(table.id.0);
            row[identity_columns_columns::NAME] = text(&column.name);
            row[identity_columns_columns::COLUMN_ID] = Value::I32(column.id.0);
            row[identity_columns_columns::SYSTEM_TYPE_ID] = Value::I8(facts.system_type_id);
            row[identity_columns_columns::USER_TYPE_ID] =
                Value::I32(i32::from(facts.system_type_id));
            row[identity_columns_columns::MAX_LENGTH] = Value::I16(facts.max_length);
            row[identity_columns_columns::PRECISION] = Value::I8(facts.precision);
            row[identity_columns_columns::SCALE] = Value::I8(facts.scale);
            row[identity_columns_columns::IS_NULLABLE] = Value::Bit(column.ty.nullable);
            row[identity_columns_columns::SEED_VALUE] = Value::I64(identity.seed);
            row[identity_columns_columns::INCREMENT_VALUE] = Value::I64(identity.increment);
            rows.push(Row(row));
        }
    }
    Ok(rows)
}

/// What `sys.identity_columns` publishes about the type of an `IDENTITY` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdentityFacts {
    /// `sys.identity_columns.system_type_id`, which `user_type_id` equals on the seven
    /// types served.
    system_type_id: u8,
    /// `sys.identity_columns.max_length`, in bytes.
    max_length: i16,
    /// `sys.identity_columns.precision`.
    precision: u8,
    /// `sys.identity_columns.scale`.
    scale: u8,
}

/// What `sys.identity_columns` publishes about `ty`, the column being called `name`.
///
/// The numbers are those of `sys.identity_columns` for the seven columns `tinyint`,
/// `smallint`, `int`, `bigint`, `decimal(9,0)`, `decimal(38,0)` and `numeric(18,0)`, each
/// declared `IDENTITY(1,1) NOT NULL`, frozen by the unit test
/// `the_identity_type_facts_are_the_published_ones`.
///
/// # Bound: a `decimal` or a `numeric` whose scale is not 0
///
/// SQL Server refuses such an `IDENTITY` — `CREATE TABLE … (a decimal(9,2) IDENTITY(1,1)
/// NOT NULL)` is error 2749, state 2 — and this function publishes its facts instead
/// (`decimal(9,2)` reads 106/5/9/2, unit test
/// `a_decimal_identity_with_a_scale_is_published_rather_than_refused`): the scale is no part
/// of what the five families below answer, and no statement turns the column down before
/// the catalogue stores it. Sending 2749 belongs to the validation of an `IDENTITY` clause,
/// not to this file.
///
/// # Errors
///
/// [`InternalError::Bug`] on a type outside the six families 2749 allows: the number is not
/// in `vauban-errors` (module documentation), and no statement refuses the column before the
/// catalogue stores it.
fn identity_facts(name: &str, ty: &TypeInfo) -> SqlResult<IdentityFacts> {
    let (system_type_id, max_length, precision, scale) = match ty.ty {
        SqlType::TinyInt => (48, 1, 3, 0),
        SqlType::SmallInt => (52, 2, 5, 0),
        SqlType::Int => (56, 4, 10, 0),
        SqlType::BigInt => (127, 8, 19, 0),
        SqlType::Decimal { precision, scale } => (106, decimal_length(precision), precision, scale),
        SqlType::Numeric { precision, scale } => (108, decimal_length(precision), precision, scale),
        _ => {
            return Err(InternalError::Bug(format!(
                "sys.identity_columns: column {name} carries an IDENTITY on a type SQL Server \
                 refuses with 2749, which vauban-errors does not catalogue"
            ))
            .into());
        }
    };
    Ok(IdentityFacts {
        system_type_id,
        max_length,
        precision,
        scale,
    })
}

/// `max_length` of a `decimal(p, s)` or a `numeric(p, s)`: 5, 9, 13 or 17 bytes.
///
/// The four steps are those of `views/sys_tables.rs` over the 38 precisions of
/// `sys.columns`; the three precisions of `sys.identity_columns` fall on them —
/// `decimal(9,0)` 5, `numeric(18,0)` 9, `decimal(38,0)` 17 (unit test
/// `the_identity_type_facts_are_the_published_ones`). The two files each hold their own copy
/// of this function.
fn decimal_length(precision: u8) -> i16 {
    match precision {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

/// The `int` a [`DbId`] is published as, the four views being per-database and the internal
/// tables holding the databases side by side.
///
/// # Errors
///
/// [`InternalError::Bug`], as `bootstrap.rs` does on the same value.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "sys.indexes: database id {id} does not fit in an int"
        ))
        .into()
    })
}

/// The `schema_id` of the schema named `name`, [`UNKNOWN_SCHEMA_ID`] when the bootstrap
/// created no schema of that name.
///
/// Compared without regard to case, as `SQL_Latin1_General_CP1_CI_AS` compares identifiers.
/// Same reading as `views/sys_tables.rs`, for the reason [`decimal_length`] gives.
fn schema_id(name: &str) -> i32 {
    SYSTEM_SCHEMAS
        .iter()
        .find(|(schema, _, _)| schema.eq_ignore_ascii_case(name))
        .map_or(UNKNOWN_SCHEMA_ID, |(_, id, _)| *id)
}

/// The columns of [`INDEXES_TABLE`], in the order of [`indexes_columns`].
fn indexes_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        // Nullable: a heap row carries no name.
        column("name", SYSNAME, true),
        column("index_id", SqlType::Int, false),
        column("type", SqlType::TinyInt, false),
        column("type_desc", SqlType::NVarChar(Len::Fixed(60)), false),
        column("is_unique", SqlType::Bit, false),
        column("is_primary_key", SqlType::Bit, false),
        column("is_unique_constraint", SqlType::Bit, false),
    ]
}

/// The columns of [`INDEX_COLUMNS_TABLE`], in the order of [`index_columns_columns`].
fn index_columns_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        column("index_id", SqlType::Int, false),
        column("index_column_id", SqlType::Int, false),
        column("column_id", SqlType::Int, false),
        column("key_ordinal", SqlType::TinyInt, false),
        column("is_descending_key", SqlType::Bit, false),
    ]
}

/// The columns of [`KEY_CONSTRAINTS_TABLE`], in the order of [`key_constraints_columns`].
fn key_constraints_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("name", SYSNAME, false),
        column("schema_id", SqlType::Int, false),
        column("parent_object_id", SqlType::Int, false),
        column("type", SqlType::Char(Len::Fixed(2)), false),
        column("type_desc", SqlType::NVarChar(Len::Fixed(60)), false),
        column("unique_index_id", SqlType::Int, false),
        column("is_system_named", SqlType::Bit, false),
    ]
}

/// The columns of [`IDENTITY_COLUMNS_TABLE`], in the order of [`identity_columns_columns`].
fn identity_columns_table_columns() -> Vec<InternalColumnDef> {
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
        column("is_nullable", SqlType::Bit, false),
        column("seed_value", SqlType::BigInt, false),
        column("increment_value", SqlType::BigInt, false),
    ]
}

/// The same view installed in the four system databases, `sys.<name>` in each of them.
///
/// The four views filter on `DB_ID()`, so the four copies of one view share their text (unit
/// test `the_views_are_installed_in_the_four_system_databases`). A database created by
/// `CREATE DATABASE` receives no copies yet, as for the other files of `views/`.
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

/// The T-SQL text of a view: its select list, the internal table it reads in `master`, and its
/// filter on the current database.
///
/// Same layout as `views/sys_core.rs` and `views/sys_tables.rs`: one select item per line, an
/// item whose expression is its own name written bare, the others as `<expression> AS <name>`,
/// so the name of a column is the last identifier of its item. The three files each hold their
/// own copy of this helper, for the reason [`decimal_length`] gives.
fn definition(items: &[(&str, &str)], table: &str) -> String {
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
    text.push_str("\n WHERE database_id = DB_ID()");
    text
}

/// The name of a column as the text of a view writes it: bracketed when it is a reserved word
/// of T-SQL, bare otherwise.
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

    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};

    use super::*;
    use crate::catalog::Catalog;
    use crate::def::{ColumnDef, ConstraintDef, IndexDef, SortedColumn, TableDef};
    use crate::ids::ObjectId;
    use crate::meta::IdentitySpec;
    use crate::{index, table};

    /// The ordered column names of `sys.indexes` in SQL Server 2022. Frozen here so that a
    /// change of the text of the view has to face the list again.
    const PUBLISHED_INDEXES_COLUMNS: [&str; 23] = [
        "object_id",
        "name",
        "index_id",
        "type",
        "type_desc",
        "is_unique",
        "data_space_id",
        "ignore_dup_key",
        "is_primary_key",
        "is_unique_constraint",
        "fill_factor",
        "is_padded",
        "is_disabled",
        "is_hypothetical",
        "is_ignored_in_optimization",
        "allow_row_locks",
        "allow_page_locks",
        "has_filter",
        "filter_definition",
        "compression_delay",
        "suppress_dup_key_messages",
        "auto_created",
        "optimize_for_sequential_key",
    ];

    /// The ordered column names of `sys.index_columns`.
    const PUBLISHED_INDEX_COLUMNS_COLUMNS: [&str; 9] = [
        "object_id",
        "index_id",
        "index_column_id",
        "column_id",
        "key_ordinal",
        "partition_ordinal",
        "is_descending_key",
        "is_included_column",
        "column_store_order_ordinal",
    ];

    /// The ordered column names of `sys.key_constraints`.
    const PUBLISHED_KEY_CONSTRAINTS_COLUMNS: [&str; 15] = [
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
        "unique_index_id",
        "is_system_named",
        "is_enforced",
    ];

    /// The ordered column names of `sys.identity_columns`, the three `sql_variant` columns
    /// included.
    const PUBLISHED_IDENTITY_COLUMNS_COLUMNS: [&str; 44] = [
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
        "is_filestream",
        "is_replicated",
        "is_non_sql_subscribed",
        "is_merge_published",
        "is_dts_replicated",
        "is_xml_document",
        "xml_collection_id",
        "default_object_id",
        "rule_object_id",
        "seed_value",
        "increment_value",
        "last_value",
        "is_not_for_replication",
        "is_computed",
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

    /// `(column, value)` of the 15 literals of `sys.indexes`, as the rows of a clustered
    /// index, a nonclustered index and a heap publish them, which agree on each of them.
    /// `None` is a `NULL`.
    const EXPECTED_INDEXES_LITERALS: [(&str, Option<&str>); 15] = [
        ("data_space_id", Some("1")),
        ("ignore_dup_key", Some("0")),
        ("fill_factor", Some("0")),
        ("is_padded", Some("0")),
        ("is_disabled", Some("0")),
        ("is_hypothetical", Some("0")),
        ("is_ignored_in_optimization", Some("0")),
        ("allow_row_locks", Some("1")),
        ("allow_page_locks", Some("1")),
        ("has_filter", Some("0")),
        ("filter_definition", None),
        ("compression_delay", None),
        ("suppress_dup_key_messages", Some("0")),
        ("auto_created", Some("0")),
        ("optimize_for_sequential_key", Some("0")),
    ];

    /// `(column, value)` of the 3 literals of `sys.index_columns`, as the rows of a key
    /// column publish them — except `is_included_column`, which SQL Server writes 1 on an
    /// `INCLUDE` column and which this file writes 0 for the reason [`INDEX_COLUMNS_VIEW`]
    /// gives.
    const EXPECTED_INDEX_COLUMNS_LITERALS: [(&str, Option<&str>); 3] = [
        ("partition_ordinal", Some("0")),
        ("is_included_column", Some("0")),
        ("column_store_order_ordinal", Some("0")),
    ];

    /// `(column, value)` of the 8 literals of `sys.key_constraints`, as the rows of a
    /// `PRIMARY KEY` and of a `UNIQUE` publish them — except `object_id`, `create_date` and
    /// `modify_date`, which SQL Server fills and which this file writes `NULL` for the reason
    /// bound 1 gives.
    const EXPECTED_KEY_CONSTRAINTS_LITERALS: [(&str, Option<&str>); 8] = [
        ("object_id", None),
        ("principal_id", None),
        ("create_date", None),
        ("modify_date", None),
        ("is_ms_shipped", Some("0")),
        ("is_published", Some("0")),
        ("is_schema_published", Some("0")),
        ("is_enforced", Some("1")),
    ];

    /// `(column, value)` of the 33 literals of `sys.identity_columns`, as the row of an
    /// `int IDENTITY` column publishes them — except `last_value`, which SQL Server fills
    /// after an `INSERT` and which this file writes `NULL` for the reason bound 2 gives.
    const EXPECTED_IDENTITY_COLUMNS_LITERALS: [(&str, Option<&str>); 33] = [
        ("collation_name", None),
        ("is_ansi_padded", Some("0")),
        ("is_rowguidcol", Some("0")),
        ("is_identity", Some("1")),
        ("is_filestream", Some("0")),
        ("is_replicated", Some("0")),
        ("is_non_sql_subscribed", Some("0")),
        ("is_merge_published", Some("0")),
        ("is_dts_replicated", Some("0")),
        ("is_xml_document", Some("0")),
        ("xml_collection_id", Some("0")),
        ("default_object_id", Some("0")),
        ("rule_object_id", Some("0")),
        ("last_value", None),
        ("is_not_for_replication", Some("0")),
        ("is_computed", Some("0")),
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

    /// `(type, system_type_id, max_length, precision, scale)` of the seven `IDENTITY` column
    /// types (see [`identity_facts`]).
    const EXPECTED_IDENTITY_FACTS: [(SqlType, u8, i16, u8, u8); 7] = [
        (SqlType::TinyInt, 48, 1, 3, 0),
        (SqlType::SmallInt, 52, 2, 5, 0),
        (SqlType::Int, 56, 4, 10, 0),
        (SqlType::BigInt, 127, 8, 19, 0),
        (
            SqlType::Decimal {
                precision: 9,
                scale: 0,
            },
            106,
            5,
            9,
            0,
        ),
        (
            SqlType::Decimal {
                precision: 38,
                scale: 0,
            },
            106,
            17,
            38,
            0,
        ),
        (
            SqlType::Numeric {
                precision: 18,
                scale: 0,
            },
            108,
            9,
            18,
            0,
        ),
    ];

    /// The select list of one view and the `(column, value)` pairs of its literals: what
    /// `the_literal_columns_are_the_published_values` walks.
    type ExpectedLiterals<'a> = (&'a [(&'a str, &'a str)], &'a [(&'a str, Option<&'a str>)]);

    /// The four view texts this file describes, read back from what [`internal_tables`]
    /// describes, for the `master` copy of each.
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

    /// The column names the text of a view publishes: the last identifier of each select item,
    /// brackets removed. Same reading as `views/sys_tables.rs`.
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

    /// The constant the literal select item of `items` named `name` writes, `None` for a
    /// `NULL`; the outer `None` means the item reads its internal table.
    ///
    /// `CAST(0 AS bit)` gives `Some(Some("0"))`, `CAST(N'NOT_APPLICABLE' AS nvarchar(60))`
    /// gives `Some(Some("NOT_APPLICABLE"))`, `CAST(NULL AS int)` gives `Some(None)`.
    fn literal_of(items: &[(&str, &str)], name: &str) -> Option<Option<String>> {
        let (_, expression) = items
            .iter()
            .find(|(column, _)| *column == name)
            .unwrap_or_else(|| panic!("{name} is a column of the view"));
        if *expression == name {
            return None;
        }
        let inner = expression
            .strip_prefix("CAST(")
            .and_then(|rest| rest.rsplit_once(" AS "))
            .expect("a literal item is a CAST")
            .0;
        if inner == "NULL" {
            return Some(None);
        }
        Some(Some(inner.strip_prefix("N'").map_or_else(
            || inner.trim_matches('\'').to_owned(),
            |literal| literal.trim_end_matches('\'').to_owned(),
        )))
    }

    /// A catalogue bootstrapped on a fresh `MemoryStorage`.
    fn bootstrapped() -> Catalog {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        Catalog::bootstrap(storage, txn).expect("bootstrap of a fresh storage")
    }

    /// A column of a [`TableDef`], with its type and its `IDENTITY` property.
    fn column_def(name: &str, ty: SqlType, identity: Option<IdentitySpec>) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, false),
            default: None,
            identity,
            computed: None,
        }
    }

    /// `master.dbo.<name>` with those columns and those constraints.
    fn table_def(name: &str, columns: Vec<ColumnDef>, constraints: Vec<ConstraintDef>) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns,
            constraints,
        }
    }

    /// A `PRIMARY KEY` or `UNIQUE` constraint on the columns named, a `-` prefix meaning
    /// `DESC`: `key_of(None, &["d", "-b", "c"], true, true)` is
    /// `PRIMARY KEY CLUSTERED (d, b DESC, c)`.
    fn key_of(
        name: Option<&str>,
        columns: &[&str],
        primary_key: bool,
        clustered: bool,
    ) -> ConstraintDef {
        let columns = columns
            .iter()
            .map(|column| SortedColumn {
                column: column.trim_start_matches('-').to_owned(),
                descending: column.starts_with('-'),
            })
            .collect();
        let name = name.map(std::borrow::ToOwned::to_owned);
        if primary_key {
            ConstraintDef::PrimaryKey {
                name,
                columns,
                clustered,
            }
        } else {
            ConstraintDef::Unique {
                name,
                columns,
                clustered,
            }
        }
    }

    /// A `PRIMARY KEY` or `UNIQUE` constraint on one ascending column.
    fn key_on(
        name: Option<&str>,
        column: &str,
        primary_key: bool,
        clustered: bool,
    ) -> ConstraintDef {
        let columns = vec![SortedColumn {
            column: column.to_owned(),
            descending: false,
        }];
        let name = name.map(std::borrow::ToOwned::to_owned);
        if primary_key {
            ConstraintDef::PrimaryKey {
                name,
                columns,
                clustered,
            }
        } else {
            ConstraintDef::Unique {
                name,
                columns,
                clustered,
            }
        }
    }

    /// The four kinds of row one catalogue gives, once each `def` and then each `IndexDef` has
    /// been created and committed.
    struct Published {
        /// The tables of the catalogue, in [`ObjectId`] order.
        tables: Vec<TableMeta>,
        /// The rows of [`INDEXES_TABLE`].
        indexes: Vec<Row>,
        /// The rows of [`INDEX_COLUMNS_TABLE`].
        index_columns: Vec<Row>,
        /// The rows of [`KEY_CONSTRAINTS_TABLE`].
        keys: Vec<Row>,
        /// The rows of [`IDENTITY_COLUMNS_TABLE`].
        identity: Vec<Row>,
    }

    /// What `catalog` publishes now, the stores having been refreshed as `index.rs` asks.
    fn published(catalog: &Catalog) -> Published {
        let mut store = table::store(catalog);
        table::refresh(catalog, &mut store).expect("refresh of the tables");
        index::refresh(catalog, &mut store).expect("refresh of the indexes");
        let tables: Vec<TableMeta> = store.live().cloned().collect();
        Published {
            indexes: index_rows(&tables, &store.indexes).expect("index rows"),
            index_columns: index_column_rows(&tables, &store.indexes).expect("index column rows"),
            keys: key_constraint_rows(&tables, &store.indexes).expect("key constraint rows"),
            identity: identity_column_rows(&tables).expect("identity rows"),
            tables,
        }
    }

    /// Creates `defs` then `indexes` in one committed transaction and answers [`published`].
    fn created(catalog: &Catalog, defs: &[TableDef], indexes: &[IndexDef]) -> Published {
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        for def in defs {
            catalog.create_table(&handle, def).expect("create_table");
        }
        for def in indexes {
            catalog.create_index(&handle, def).expect("create_index");
        }
        catalog.txn.commit(handle).expect("commit");
        published(catalog)
    }

    /// The identifier of the single table of `tables`.
    fn only_table(tables: &[TableMeta]) -> ObjectId {
        assert_eq!(tables.len(), 1, "{tables:?}");
        tables[0].id
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
            Value::I64(number) => Some(number.to_string()),
            Value::String(string) => Some(string.text.clone()),
            other => panic!("no internal row carries {other:?}"),
        }
    }

    /// The value of the column at `position` of `row`, as [`rendered`] writes it.
    fn field(row: &Row, position: usize) -> Option<String> {
        rendered(&row.0[position])
    }

    /// The values of the column at `position` of each row.
    fn column_of(rows: &[Row], position: usize) -> Vec<Option<String>> {
        rows.iter().map(|row| field(row, position)).collect()
    }

    /// A vector of `Some(text)`, to read an expectation without a pile of `to_owned`.
    fn some(values: &[&str]) -> Vec<Option<String>> {
        values
            .iter()
            .map(|value| Some((*value).to_owned()))
            .collect()
    }

    #[test]
    fn sys_indexes_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("indexes")),
            PUBLISHED_INDEXES_COLUMNS
        );
    }

    #[test]
    fn sys_index_columns_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("index_columns")),
            PUBLISHED_INDEX_COLUMNS_COLUMNS
        );
    }

    #[test]
    fn sys_key_constraints_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("key_constraints")),
            PUBLISHED_KEY_CONSTRAINTS_COLUMNS
        );
    }

    #[test]
    fn sys_identity_columns_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("identity_columns")),
            PUBLISHED_IDENTITY_COLUMNS_COLUMNS
        );
    }

    #[test]
    fn view_definition_is_a_select_without_a_join() {
        let described = definitions();
        assert_eq!(described.len(), 4, "{described:?}");
        for (name, definition) in described {
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
                definition.ends_with("\n WHERE database_id = DB_ID()"),
                "sys.{name} is a per-database view: {definition}"
            );
        }
    }

    #[test]
    fn the_four_definitions_parse() {
        // The text of a view is T-SQL the engine reads, so the parser of the workspace is
        // the first judge of it. The counter-check of this test is
        // `a_reserved_column_name_is_delimited`, which shows the same text refused once the
        // brackets of `precision` are gone.
        for (name, definition) in definitions() {
            let batch = parse_batch(&definition, &ParseOptions::default());
            assert!(batch.is_ok(), "sys.{name}: {batch:?}\n{definition}");
        }
    }

    #[test]
    fn a_reserved_column_name_is_delimited() {
        let identity = definition_of("identity_columns");
        assert!(identity.contains("[precision]"), "{identity}");
        let bare = identity.replace("[precision]", "precision");
        let refused = parse_batch(&bare, &ParseOptions::default());
        assert!(
            refused.is_err(),
            "a bare `precision` in the select list is read by the parser: {bare}"
        );
        assert_eq!(RESERVED_COLUMN_NAMES, ["precision"]);
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        let mut installed: Vec<(String, String)> = internal_tables()
            .into_iter()
            .flat_map(|table| table.views)
            .map(|view| (view.name.database, view.name.name))
            .collect();
        installed.sort();
        let mut expected: Vec<(String, String)> = SYSTEM_DATABASES
            .iter()
            .flat_map(|database| {
                [
                    "indexes",
                    "index_columns",
                    "key_constraints",
                    "identity_columns",
                ]
                .into_iter()
                .map(move |view| ((*database).to_owned(), view.to_owned()))
            })
            .collect();
        expected.sort();
        assert_eq!(installed, expected);
        for view in internal_tables().into_iter().flat_map(|table| table.views) {
            assert_eq!(view.name.schema, VIEW_SCHEMA);
        }
    }

    #[test]
    fn the_column_order_is_the_one_the_constants_name() {
        let described: Vec<(&str, Vec<InternalColumnDef>, usize)> = vec![
            (
                INDEXES_TABLE,
                indexes_table_columns(),
                indexes_columns::WIDTH,
            ),
            (
                INDEX_COLUMNS_TABLE,
                index_columns_table_columns(),
                index_columns_columns::WIDTH,
            ),
            (
                KEY_CONSTRAINTS_TABLE,
                key_constraints_table_columns(),
                key_constraints_columns::WIDTH,
            ),
            (
                IDENTITY_COLUMNS_TABLE,
                identity_columns_table_columns(),
                identity_columns_columns::WIDTH,
            ),
        ];
        for (name, columns, width) in &described {
            assert_eq!(columns.len(), *width, "{name}");
        }
        // One position read per table; the row builders address the other positions through
        // the same constants.
        assert_eq!(indexes_table_columns()[indexes_columns::NAME].name, "name");
        assert_eq!(
            indexes_table_columns()[indexes_columns::IS_UNIQUE_CONSTRAINT].name,
            "is_unique_constraint"
        );
        assert_eq!(
            index_columns_table_columns()[index_columns_columns::KEY_ORDINAL].name,
            "key_ordinal"
        );
        assert_eq!(
            key_constraints_table_columns()[key_constraints_columns::UNIQUE_INDEX_ID].name,
            "unique_index_id"
        );
        assert_eq!(
            identity_columns_table_columns()[identity_columns_columns::INCREMENT_VALUE].name,
            "increment_value"
        );
        // Each stored column is read by its view under its own name, `database_id` excepted:
        // the filter reads that one and the select list does not.
        for (columns, items) in [
            (indexes_table_columns(), &INDEXES_VIEW[..]),
            (index_columns_table_columns(), &INDEX_COLUMNS_VIEW[..]),
            (key_constraints_table_columns(), &KEY_CONSTRAINTS_VIEW[..]),
            (identity_columns_table_columns(), &IDENTITY_COLUMNS_VIEW[..]),
        ] {
            for column in &columns {
                if column.name == "database_id" {
                    continue;
                }
                assert!(
                    items.iter().any(
                        |(name, expression)| *name == column.name && *expression == column.name
                    ),
                    "the column {} is stored and not read",
                    column.name
                );
            }
        }
    }

    #[test]
    fn the_literal_columns_are_the_published_values() {
        let lists: [ExpectedLiterals<'_>; 4] = [
            (&INDEXES_VIEW, &EXPECTED_INDEXES_LITERALS),
            (&INDEX_COLUMNS_VIEW, &EXPECTED_INDEX_COLUMNS_LITERALS),
            (&KEY_CONSTRAINTS_VIEW, &EXPECTED_KEY_CONSTRAINTS_LITERALS),
            (&IDENTITY_COLUMNS_VIEW, &EXPECTED_IDENTITY_COLUMNS_LITERALS),
        ];
        let mut compared = 0;
        for (items, expected) in lists {
            for (name, value) in expected {
                assert_eq!(
                    literal_of(items, name),
                    Some(value.map(std::borrow::ToOwned::to_owned)),
                    "the literal {name} differs from the published row"
                );
                compared += 1;
            }
            // Each item that is not read from the internal table is in the list above.
            let literals = items
                .iter()
                .filter(|(name, expression)| expression != name)
                .count();
            assert_eq!(literals, expected.len(), "a literal is not accounted for");
        }
        assert_eq!(compared, 15 + 3 + 8 + 33);
    }

    #[test]
    fn the_columns_written_null_are_the_ones_with_no_datum_behind_them() {
        let mut nulls: Vec<&str> = [
            &INDEXES_VIEW[..],
            &INDEX_COLUMNS_VIEW[..],
            &KEY_CONSTRAINTS_VIEW[..],
            &IDENTITY_COLUMNS_VIEW[..],
        ]
        .concat()
        .into_iter()
        .filter(|(_, expression)| expression.starts_with("CAST(NULL AS "))
        .map(|(name, _)| name)
        .collect();
        nulls.sort_unstable();
        assert_eq!(
            nulls,
            [
                "collation_name",
                "column_encryption_key_database_name",
                "column_encryption_key_id",
                "compression_delay",
                "create_date",
                "encryption_algorithm_name",
                "encryption_type",
                "encryption_type_desc",
                "filter_definition",
                "graph_type",
                "graph_type_desc",
                "last_value",
                "ledger_view_column_type",
                "ledger_view_column_type_desc",
                "modify_date",
                "object_id",
                "principal_id",
            ]
        );
    }

    #[test]
    fn the_identity_type_facts_are_the_published_ones() {
        for (ty, system_type_id, max_length, precision, scale) in EXPECTED_IDENTITY_FACTS {
            assert_eq!(
                identity_facts("a", &TypeInfo::new(ty, false)).expect("a numeric IDENTITY"),
                IdentityFacts {
                    system_type_id,
                    max_length,
                    precision,
                    scale,
                },
                "{ty:?}"
            );
        }
        assert_eq!(EXPECTED_IDENTITY_FACTS.len(), 7);
    }

    #[test]
    fn an_identity_on_a_type_sql_server_refuses_is_a_bug_naming_2749() {
        let err = identity_facts("a", &TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), false))
            .expect_err("a varchar IDENTITY");
        assert!(err.message.contains("2749"), "{}", err.message);
    }

    #[test]
    fn heap_table_has_index_id_zero() {
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_heap",
                vec![column_def("a", SqlType::Int, None)],
                Vec::new(),
            )],
            &[],
        );
        assert!(read.tables[0].clustered.is_none());
        assert_eq!(read.indexes.len(), 1, "{:?}", read.indexes);
        let row = &read.indexes[0];
        assert_eq!(field(row, indexes_columns::INDEX_ID).as_deref(), Some("0"));
        assert_eq!(field(row, indexes_columns::TYPE).as_deref(), Some("0"));
        assert_eq!(
            field(row, indexes_columns::TYPE_DESC).as_deref(),
            Some(HEAP_TYPE_DESC)
        );
        assert_eq!(field(row, indexes_columns::NAME), None);
        assert_eq!(field(row, indexes_columns::IS_UNIQUE).as_deref(), Some("0"));
        assert_eq!(
            field(row, indexes_columns::IS_PRIMARY_KEY).as_deref(),
            Some("0")
        );
        // A heap carries no key column, no key constraint and no identity column.
        assert!(read.index_columns.is_empty(), "{:?}", read.index_columns);
        assert!(read.keys.is_empty(), "{:?}", read.keys);
        assert!(read.identity.is_empty(), "{:?}", read.identity);
    }

    #[test]
    fn a_heap_has_no_index_column_row() {
        // Stated apart from `heap_table_has_index_id_zero`: a table that carries both a heap
        // row and an index has index column rows for the index, not for the heap.
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_uq",
                vec![column_def("a", SqlType::Int, None)],
                vec![key_on(Some("uq_uq"), "a", false, false)],
            )],
            &[],
        );
        assert!(read.tables[0].clustered.is_none());
        assert_eq!(
            column_of(&read.indexes, indexes_columns::INDEX_ID),
            some(&["0", "2"])
        );
        assert_eq!(
            column_of(&read.index_columns, index_columns_columns::INDEX_ID),
            some(&["2"])
        );
    }

    #[test]
    fn pk_is_index_id_one() {
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_pk",
                vec![
                    column_def("a", SqlType::Int, None),
                    column_def("b", SqlType::Int, None),
                ],
                vec![key_on(Some("pk_pk"), "a", true, true)],
            )],
            &[],
        );
        assert!(read.tables[0].clustered.is_some());
        // No heap row: the table carries a clustered key.
        assert_eq!(read.indexes.len(), 1, "{:?}", read.indexes);
        let row = &read.indexes[0];
        assert_eq!(field(row, indexes_columns::INDEX_ID).as_deref(), Some("1"));
        assert_eq!(field(row, indexes_columns::TYPE).as_deref(), Some("1"));
        assert_eq!(
            field(row, indexes_columns::TYPE_DESC).as_deref(),
            Some(CLUSTERED_TYPE_DESC)
        );
        assert_eq!(field(row, indexes_columns::NAME).as_deref(), Some("pk_pk"));
        assert_eq!(
            field(row, indexes_columns::IS_PRIMARY_KEY).as_deref(),
            Some("1")
        );
        assert_eq!(field(row, indexes_columns::IS_UNIQUE).as_deref(), Some("1"));
        assert_eq!(
            field(row, indexes_columns::IS_UNIQUE_CONSTRAINT).as_deref(),
            Some("0")
        );
        // One key column: `key_ordinal` 1, ascending, the `column_id` of the column `a`.
        assert_eq!(read.index_columns.len(), 1, "{:?}", read.index_columns);
        let key = &read.index_columns[0];
        assert_eq!(
            field(key, index_columns_columns::INDEX_ID).as_deref(),
            Some("1")
        );
        assert_eq!(
            field(key, index_columns_columns::INDEX_COLUMN_ID).as_deref(),
            Some("1")
        );
        assert_eq!(
            field(key, index_columns_columns::KEY_ORDINAL).as_deref(),
            Some("1")
        );
        assert_eq!(
            field(key, index_columns_columns::IS_DESCENDING_KEY).as_deref(),
            Some("0")
        );
        assert_eq!(
            field(key, index_columns_columns::COLUMN_ID).as_deref(),
            Some("1")
        );
        // The constraint row names the same index.
        assert_eq!(read.keys.len(), 1, "{:?}", read.keys);
        let constraint = &read.keys[0];
        assert_eq!(
            field(constraint, key_constraints_columns::TYPE).as_deref(),
            Some(PRIMARY_KEY_TYPE)
        );
        assert_eq!(
            field(constraint, key_constraints_columns::TYPE_DESC).as_deref(),
            Some(PRIMARY_KEY_TYPE_DESC)
        );
        assert_eq!(
            field(constraint, key_constraints_columns::UNIQUE_INDEX_ID).as_deref(),
            Some("1")
        );
        assert_eq!(
            field(constraint, key_constraints_columns::IS_SYSTEM_NAMED).as_deref(),
            Some("0")
        );
        assert_eq!(
            field(constraint, key_constraints_columns::NAME).as_deref(),
            Some("pk_pk")
        );
    }

    #[test]
    fn a_descending_key_column_is_published_descending() {
        // The composite key `(b DESC, a)`: `b` at `key_ordinal` 1 with `is_descending_key`
        // 1, then `a`.
        let catalog = bootstrapped();
        let table = only_table(
            &created(
                &catalog,
                &[table_def(
                    "t_desc",
                    vec![
                        column_def("a", SqlType::Int, None),
                        column_def("b", SqlType::Int, None),
                    ],
                    Vec::new(),
                )],
                &[],
            )
            .tables,
        );
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        catalog
            .create_index(
                &handle,
                &IndexDef {
                    table,
                    name: "ix_desc".to_owned(),
                    columns: vec![
                        SortedColumn {
                            column: "b".to_owned(),
                            descending: true,
                        },
                        SortedColumn {
                            column: "a".to_owned(),
                            descending: false,
                        },
                    ],
                    unique: false,
                    clustered: false,
                },
            )
            .expect("create_index");
        catalog.txn.commit(handle).expect("commit");
        let read = published(&catalog);
        assert_eq!(read.index_columns.len(), 2, "{:?}", read.index_columns);
        assert_eq!(
            column_of(&read.index_columns, index_columns_columns::KEY_ORDINAL),
            some(&["1", "2"])
        );
        assert_eq!(
            column_of(&read.index_columns, index_columns_columns::COLUMN_ID),
            some(&["2", "1"])
        );
        assert_eq!(
            column_of(
                &read.index_columns,
                index_columns_columns::IS_DESCENDING_KEY
            ),
            some(&["1", "0"])
        );
    }

    #[test]
    fn a_unique_constraint_and_a_unique_index_differ_on_one_column() {
        let catalog = bootstrapped();
        let table = only_table(
            &created(
                &catalog,
                &[table_def(
                    "t_mix",
                    vec![
                        column_def("a", SqlType::Int, None),
                        column_def("b", SqlType::Int, None),
                    ],
                    vec![key_on(Some("uq_mix"), "a", false, false)],
                )],
                &[],
            )
            .tables,
        );
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        catalog
            .create_index(
                &handle,
                &IndexDef {
                    table,
                    name: "ix_mix".to_owned(),
                    columns: vec![SortedColumn {
                        column: "b".to_owned(),
                        descending: false,
                    }],
                    unique: true,
                    clustered: false,
                },
            )
            .expect("create_index");
        catalog.txn.commit(handle).expect("commit");
        let read = published(&catalog);
        // Heap row, the `UNIQUE` constraint at 2, the `CREATE UNIQUE INDEX` at 3: the last two
        // are `is_unique` 1, and the constraint alone is `is_unique_constraint` 1.
        assert_eq!(
            column_of(&read.indexes, indexes_columns::INDEX_ID),
            some(&["0", "2", "3"])
        );
        assert_eq!(
            column_of(&read.indexes, indexes_columns::IS_UNIQUE),
            some(&["0", "1", "1"])
        );
        assert_eq!(
            column_of(&read.indexes, indexes_columns::IS_UNIQUE_CONSTRAINT),
            some(&["0", "1", "0"])
        );
        // One row of `sys.key_constraints`, the `UNIQUE` constraint, of type `UQ`.
        assert_eq!(read.keys.len(), 1, "{:?}", read.keys);
        assert_eq!(
            field(&read.keys[0], key_constraints_columns::TYPE).as_deref(),
            Some(UNIQUE_TYPE)
        );
        assert_eq!(
            field(&read.keys[0], key_constraints_columns::TYPE_DESC).as_deref(),
            Some(UNIQUE_TYPE_DESC)
        );
        assert_eq!(
            field(&read.keys[0], key_constraints_columns::UNIQUE_INDEX_ID).as_deref(),
            Some("2")
        );
    }

    #[test]
    fn the_three_index_views_agree_on_the_index_id() {
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_two",
                vec![
                    column_def("a", SqlType::Int, None),
                    column_def("b", SqlType::Int, None),
                ],
                vec![
                    key_on(Some("pk_two"), "a", true, true),
                    key_on(Some("uq_two"), "b", false, false),
                ],
            )],
            &[],
        );
        let numbers = some(&["1", "2"]);
        assert_eq!(
            column_of(&read.indexes, indexes_columns::INDEX_ID),
            numbers,
            "the clustered PRIMARY KEY is 1 and the UNIQUE that follows it is 2"
        );
        assert_eq!(
            column_of(&read.index_columns, index_columns_columns::INDEX_ID),
            numbers
        );
        assert_eq!(
            column_of(&read.keys, key_constraints_columns::UNIQUE_INDEX_ID),
            numbers
        );
        assert_eq!(
            column_of(&read.keys, key_constraints_columns::TYPE),
            some(&[PRIMARY_KEY_TYPE, UNIQUE_TYPE])
        );
    }

    #[test]
    fn a_generated_constraint_name_is_read_as_generated() {
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_gen",
                vec![column_def("a", SqlType::Int, None)],
                vec![key_on(None, "a", true, true)],
            )],
            &[],
        );
        assert_eq!(read.keys.len(), 1, "{:?}", read.keys);
        let row = &read.keys[0];
        let name = field(row, key_constraints_columns::NAME).expect("a name");
        assert!(name.starts_with("PK__t_gen__"), "{name}");
        assert_eq!(
            field(row, key_constraints_columns::IS_SYSTEM_NAMED).as_deref(),
            Some("1"),
            "{name}"
        );
        assert_eq!(
            field(row, key_constraints_columns::SCHEMA_ID).as_deref(),
            Some("1"),
            "dbo is schema 1 in bootstrap.rs"
        );
        assert_eq!(
            field(row, key_constraints_columns::PARENT_OBJECT_ID),
            Some(read.tables[0].id.0.to_string())
        );
    }

    #[test]
    fn a_written_name_of_the_generated_shape_is_read_as_generated() {
        // Bound 3 of the module documentation, written as a test rather than as a sentence:
        // the column is read from the shape of the name, so a name a client writes that way is
        // published as system-named.
        assert!(is_system_named(
            "PK__abcdefgh__0123456789ABCDEF",
            "abcdefgh",
            true
        ));
        assert!(is_system_named(
            "UQ__abcdefgh__0123456789ABCDEF",
            "abcdefghijk",
            false
        ));
        // What the shape turns down: a written name, the other prefix, a short tail, a lower
        // case digit, another table.
        assert!(!is_system_named("pk_abcdefgh", "abcdefgh", true));
        assert!(!is_system_named(
            "UQ__abcdefgh__0123456789ABCDEF",
            "abcdefgh",
            true
        ));
        assert!(!is_system_named(
            "PK__abcdefgh__0123456789ABCDE",
            "abcdefgh",
            true
        ));
        assert!(!is_system_named(
            "PK__abcdefgh__0123456789abcdef",
            "abcdefgh",
            true
        ));
        assert!(!is_system_named(
            "PK__abcdefgh__0123456789ABCDEF",
            "zzzzzzzz",
            true
        ));
    }

    #[test]
    fn identity_column_row_matches_spec() {
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_id",
                vec![
                    column_def(
                        "id",
                        SqlType::Int,
                        Some(IdentitySpec {
                            seed: 7,
                            increment: 3,
                        }),
                    ),
                    column_def("label", SqlType::Int, None),
                ],
                Vec::new(),
            )],
            &[],
        );
        // One row, for the `IDENTITY` column and not for its neighbour.
        assert_eq!(read.identity.len(), 1, "{:?}", read.identity);
        let row = &read.identity[0];
        assert_eq!(
            field(row, identity_columns_columns::NAME).as_deref(),
            Some("id")
        );
        assert_eq!(
            field(row, identity_columns_columns::OBJECT_ID),
            Some(read.tables[0].id.0.to_string())
        );
        assert_eq!(
            field(row, identity_columns_columns::COLUMN_ID).as_deref(),
            Some("1")
        );
        assert_eq!(
            field(row, identity_columns_columns::SEED_VALUE).as_deref(),
            Some("7")
        );
        assert_eq!(
            field(row, identity_columns_columns::INCREMENT_VALUE).as_deref(),
            Some("3")
        );
        // The type facts of an `int`, as published on an `int IDENTITY(7,3)`.
        assert_eq!(
            field(row, identity_columns_columns::SYSTEM_TYPE_ID).as_deref(),
            Some("56")
        );
        assert_eq!(
            field(row, identity_columns_columns::USER_TYPE_ID).as_deref(),
            Some("56")
        );
        assert_eq!(
            field(row, identity_columns_columns::MAX_LENGTH).as_deref(),
            Some("4")
        );
        assert_eq!(
            field(row, identity_columns_columns::PRECISION).as_deref(),
            Some("10")
        );
        assert_eq!(
            field(row, identity_columns_columns::SCALE).as_deref(),
            Some("0")
        );
        assert_eq!(
            field(row, identity_columns_columns::IS_NULLABLE).as_deref(),
            Some("0")
        );
    }

    #[test]
    fn a_bare_identity_is_seed_one_increment_one() {
        let catalog = bootstrapped();
        let read = created(
            &catalog,
            &[table_def(
                "t_bare",
                vec![column_def(
                    "id",
                    SqlType::BigInt,
                    Some(IdentitySpec::default()),
                )],
                Vec::new(),
            )],
            &[],
        );
        assert_eq!(read.identity.len(), 1, "{:?}", read.identity);
        let row = &read.identity[0];
        assert_eq!(
            field(row, identity_columns_columns::SEED_VALUE).as_deref(),
            Some("1")
        );
        assert_eq!(
            field(row, identity_columns_columns::INCREMENT_VALUE).as_deref(),
            Some("1")
        );
        // A `bigint IDENTITY`, whose facts are the published ones of that type.
        assert_eq!(
            field(row, identity_columns_columns::SYSTEM_TYPE_ID).as_deref(),
            Some("127")
        );
        assert_eq!(
            field(row, identity_columns_columns::MAX_LENGTH).as_deref(),
            Some("8")
        );
    }

    #[test]
    fn a_heap_row_differs_from_an_index_row_on_six_columns() {
        // The heap row and an index row agree on the 15 literals of `sys.indexes`, so the
        // two rows differ on the columns this file stores and on no
        // other: 6 of the 9 here, `database_id` and `object_id` being shared and
        // `is_unique_constraint` reading 0 on both.
        let heap = created(
            &bootstrapped(),
            &[table_def(
                "t_h",
                vec![column_def("a", SqlType::Int, None)],
                Vec::new(),
            )],
            &[],
        );
        let keyed = created(
            &bootstrapped(),
            &[table_def(
                "t_h",
                vec![column_def("a", SqlType::Int, None)],
                vec![key_on(Some("pk_h"), "a", true, true)],
            )],
            &[],
        );
        assert_eq!(heap.indexes.len(), 1);
        assert_eq!(keyed.indexes.len(), 1);
        let differing: Vec<usize> = (0..indexes_columns::WIDTH)
            .filter(|position| heap.indexes[0].0[*position] != keyed.indexes[0].0[*position])
            .collect();
        assert_eq!(
            differing,
            vec![
                indexes_columns::NAME,
                indexes_columns::INDEX_ID,
                indexes_columns::TYPE,
                indexes_columns::TYPE_DESC,
                indexes_columns::IS_UNIQUE,
                indexes_columns::IS_PRIMARY_KEY,
            ]
        );
        assert_eq!(EXPECTED_INDEXES_LITERALS.len(), 15);
    }

    /// `(index_column_id, column_id, key_ordinal, is_descending_key)` of one row of
    /// [`INDEX_COLUMNS_TABLE`], as `sys.index_columns` publishes them.
    type ReadKeyColumn = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    /// The four numbers above, for each row of [`INDEX_COLUMNS_TABLE`].
    fn key_columns(rows: &[Row]) -> Vec<ReadKeyColumn> {
        rows.iter()
            .map(|row| {
                (
                    field(row, index_columns_columns::INDEX_COLUMN_ID),
                    field(row, index_columns_columns::COLUMN_ID),
                    field(row, index_columns_columns::KEY_ORDINAL),
                    field(row, index_columns_columns::IS_DESCENDING_KEY),
                )
            })
            .collect()
    }

    /// The four columns `a`, `b`, `c`, `d`, which take `column_id` 1 to 4.
    fn four_columns() -> Vec<ColumnDef> {
        ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| column_def(name, SqlType::Int, None))
            .collect()
    }

    #[test]
    fn a_clustered_key_out_of_order_numbers_its_columns_by_column_id() {
        // `PRIMARY KEY CLUSTERED (d, b DESC, c)` on `(a, b, c, d)`:
        // `index_column_id` 1, 2, 3 against `column_id` 2, 3, 4 and `key_ordinal` 2, 3, 1.
        let read = created(
            &bootstrapped(),
            &[table_def(
                "t_cl",
                four_columns(),
                vec![key_of(Some("pk_cl"), &["d", "-b", "c"], true, true)],
            )],
            &[],
        );
        assert_eq!(
            key_columns(&read.index_columns),
            vec![
                (
                    Some("1".to_owned()),
                    Some("2".to_owned()),
                    Some("2".to_owned()),
                    Some("1".to_owned())
                ),
                (
                    Some("2".to_owned()),
                    Some("3".to_owned()),
                    Some("3".to_owned()),
                    Some("0".to_owned())
                ),
                (
                    Some("3".to_owned()),
                    Some("4".to_owned()),
                    Some("1".to_owned()),
                    Some("0".to_owned())
                ),
            ]
        );
        // The same on two other clustered keys: `(c, a)` and
        // `UNIQUE CLUSTERED (c, b)` read `key_ordinal` 2 then 1.
        for (name, key, columns) in [
            (
                "t_pair",
                key_of(Some("pk_pair"), &["c", "a"], true, true),
                ["1", "3"],
            ),
            (
                "t_ucl",
                key_of(Some("uq_ucl"), &["c", "b"], false, true),
                ["2", "3"],
            ),
        ] {
            let read = created(
                &bootstrapped(),
                &[table_def(name, four_columns(), vec![key])],
                &[],
            );
            assert_eq!(
                key_columns(&read.index_columns),
                vec![
                    (
                        Some("1".to_owned()),
                        Some(columns[0].to_owned()),
                        Some("2".to_owned()),
                        Some("0".to_owned())
                    ),
                    (
                        Some("2".to_owned()),
                        Some(columns[1].to_owned()),
                        Some("1".to_owned()),
                        Some("0".to_owned())
                    ),
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn a_nonclustered_key_out_of_order_numbers_its_columns_by_key_ordinal() {
        // The counter-check of the test above: on a nonclustered index the two numbers
        // agree, on `(c, b DESC)` and `(d, a)`.
        let catalog = bootstrapped();
        let table = only_table(
            &created(
                &catalog,
                &[table_def("t_nc", four_columns(), Vec::new())],
                &[],
            )
            .tables,
        );
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        for (name, columns) in [
            ("ix_nc_cb", vec![("c", false), ("b", true)]),
            ("ix_nc_da", vec![("d", false), ("a", false)]),
        ] {
            catalog
                .create_index(
                    &handle,
                    &IndexDef {
                        table,
                        name: name.to_owned(),
                        columns: columns
                            .into_iter()
                            .map(|(column, descending)| SortedColumn {
                                column: column.to_owned(),
                                descending,
                            })
                            .collect(),
                        unique: false,
                        clustered: false,
                    },
                )
                .expect("create_index");
        }
        catalog.txn.commit(handle).expect("commit");
        let read = published(&catalog);
        assert_eq!(
            column_of(&read.index_columns, index_columns_columns::INDEX_ID),
            some(&["2", "2", "3", "3"])
        );
        assert_eq!(
            key_columns(&read.index_columns),
            vec![
                (
                    Some("1".to_owned()),
                    Some("3".to_owned()),
                    Some("1".to_owned()),
                    Some("0".to_owned())
                ),
                (
                    Some("2".to_owned()),
                    Some("2".to_owned()),
                    Some("2".to_owned()),
                    Some("1".to_owned())
                ),
                (
                    Some("1".to_owned()),
                    Some("4".to_owned()),
                    Some("1".to_owned()),
                    Some("0".to_owned())
                ),
                (
                    Some("2".to_owned()),
                    Some("1".to_owned()),
                    Some("2".to_owned()),
                    Some("0".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn a_decimal_identity_with_a_scale_is_published_rather_than_refused() {
        // The bound of [`identity_facts`]: SQL Server answers 2749 on this shape and the
        // catalogue stores it, so the view publishes its facts.
        assert_eq!(
            identity_facts(
                "a",
                &TypeInfo::new(
                    SqlType::Decimal {
                        precision: 9,
                        scale: 2
                    },
                    false
                )
            )
            .expect("the catalogue does not refuse it"),
            IdentityFacts {
                system_type_id: 106,
                max_length: 5,
                precision: 9,
                scale: 2,
            }
        );
    }

    #[test]
    fn a_clustered_key_an_index_and_an_identity_give_two_two_one_and_one_rows() {
        // The table of `tests/sys_indexes.rs`, whose comment names these four counts: a
        // clustered `PRIMARY KEY` on `a`, a nonclustered index on `b`, and `b` an `IDENTITY`.
        // No heap row, the table carrying a clustered key.
        let catalog = bootstrapped();
        let table = only_table(
            &created(
                &catalog,
                &[table_def(
                    "t_all",
                    vec![
                        column_def("a", SqlType::Int, None),
                        column_def("b", SqlType::Int, Some(IdentitySpec::default())),
                    ],
                    vec![key_of(Some("pk_all"), &["a"], true, true)],
                )],
                &[],
            )
            .tables,
        );
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        catalog
            .create_index(
                &handle,
                &IndexDef {
                    table,
                    name: "ix_all_b".to_owned(),
                    columns: vec![SortedColumn {
                        column: "b".to_owned(),
                        descending: true,
                    }],
                    unique: false,
                    clustered: false,
                },
            )
            .expect("create_index");
        catalog.txn.commit(handle).expect("commit");
        let read = published(&catalog);
        assert_eq!(
            (
                read.indexes.len(),
                read.index_columns.len(),
                read.keys.len(),
                read.identity.len()
            ),
            (2, 2, 1, 1)
        );
        assert_eq!(
            column_of(&read.indexes, indexes_columns::INDEX_ID),
            some(&["1", "2"]),
            "no heap row: the table carries a clustered key"
        );
    }

    #[test]
    fn the_internal_tables_carry_no_row_at_bootstrap() {
        for table in internal_tables() {
            assert!(table.rows.is_empty(), "{}", table.name);
            assert!(table.clustered_key.is_none(), "{}", table.name);
        }
        let names: Vec<String> = internal_tables()
            .into_iter()
            .map(|table| table.name)
            .collect();
        assert_eq!(
            names,
            [
                INDEXES_TABLE,
                INDEX_COLUMNS_TABLE,
                KEY_CONSTRAINTS_TABLE,
                IDENTITY_COLUMNS_TABLE
            ]
        );
    }

    #[test]
    fn the_internal_tables_of_the_catalogue_have_no_row_in_these_views() {
        // The `vauban_sys_*` tables are heaps without an `IDENTITY` column, and `views/sys_tables.rs`
        // keeps them out of what `sys.tables` publishes; this file reads the tables it is
        // given, which are those of the store of `table.rs` — a fresh catalogue holds none.
        let catalog = bootstrapped();
        let read = published(&catalog);
        assert!(read.tables.is_empty(), "{:?}", read.tables);
        assert!(read.indexes.is_empty(), "{:?}", read.indexes);
        assert!(read.index_columns.is_empty(), "{:?}", read.index_columns);
        assert!(read.keys.is_empty(), "{:?}", read.keys);
        assert!(read.identity.is_empty(), "{:?}", read.identity);
    }
}
