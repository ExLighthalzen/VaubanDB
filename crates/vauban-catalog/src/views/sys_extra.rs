//! `sys.all_objects`, `sys.all_columns`, `sys.views`, `sys.partitions`,
//! `sys.allocation_units`, `sys.database_files` and `sys.master_files`.
//!
//! # The shape of the views
//!
//! The column list of each of the seven views — names, order, types — is the one SQL Server
//! 2022 publishes: `sys.all_objects` 12 columns, `sys.all_columns` 40, `sys.views` 23,
//! `sys.partitions` 11, `sys.allocation_units` 8, `sys.database_files` 30 and
//! `sys.master_files` 32. The unit test `the_columns_of_the_seven_views_are_the_published_ones`
//! freezes the seven vectors of names and compares them with the text this file builds.
//!
//! # Five internal tables for seven views
//!
//! A view is a `SELECT` over one denormalised internal table, without a join (unit test
//! `no_definition_holds_a_join`).
//!
//! | Internal table | Views |
//! |---|---|
//! | [`ALL_OBJECTS_TABLE`] | `sys.all_objects`, `sys.views` |
//! | [`ALL_COLUMNS_TABLE`] | `sys.all_columns` |
//! | [`PARTITIONS_TABLE`] | `sys.partitions` |
//! | [`ALLOCATION_UNITS_TABLE`] | `sys.allocation_units` |
//! | [`FILES_TABLE`] | `sys.database_files`, `sys.master_files` |
//!
//! [`ALL_OBJECTS_TABLE`] and [`ALL_COLUMNS_TABLE`] are the two tables of `views/sys_tables.rs`
//! again, column for column, with one change: their `database_id` is nullable, a `NULL`
//! meaning "in every database", which is how a system view installed in the four system
//! databases holds one row instead of four. The two copies exist because a view reads **one**
//! table: the rows `sys_rows.rs` writes in the table of `views/sys_tables.rs` it writes in
//! this one too, the two shapes being equal (unit test
//! `the_two_copies_are_the_tables_of_sys_tables_with_a_nullable_database_id`), and
//! [`user_object_rows`] / [`user_column_rows`] call the row builders of that file rather than
//! restating its type facts.
//!
//! # Which rows each table holds
//!
//! - [`ALL_OBJECTS_TABLE`] carries, from the bootstrap, one row per **system view** the files
//!   of `views/` install — `sys.tables`, `sys.databases`, the seven views of this file… —
//!   with `type` `V `, `is_ms_shipped` 1 and `database_id` `NULL` ([`system_view_rows`]). That
//!   is what `sys.views` publishes: VaubanDB serves no `CREATE VIEW`, so a user view has no
//!   row, and the filter `type = 'V '` keeps a user table out (unit test
//!   `sys_views_lists_sys_tables_not_user_views`).
//! - [`FILES_TABLE`] carries, from the bootstrap, the two files of each of the four system
//!   databases — eight rows, `database_id` 1 to 4 ([`file_rows`], integration test
//!   `master_has_database_files_rows`). Those identifiers are the ones a first bootstrap hands
//!   out, `master` 1, `tempdb` 2, `model` 3, `msdb` 4 (`bootstrap.rs`, integration test
//!   `bootstrap_creates_four_system_databases`); on a storage where a system database was
//!   already there under another identifier, `storage` reuses that one and these rows name
//!   another database (integration test
//!   `the_file_rows_name_the_databases_of_the_databases_table` reads the two tables together).
//! - the three other tables hold no row at the bootstrap. The rows of a user object are built
//!   from the `*Meta` of the catalogue by [`user_object_rows`], [`user_column_rows`],
//!   [`partition_rows`] and [`allocation_unit_rows`], as `views/sys_tables.rs` does, and
//!   `sys_rows.rs` writes them into `storage` at each DDL.
//!
//! # Identifiers and plausible constants
//!
//! - `object_id` of a system view: [`FIRST_SYSTEM_VIEW_OBJECT_ID`] and downwards, in the order
//!   the files of `views/` describe the views. Negative, as `ids.rs` says SQL Server numbers
//!   its own objects. The numbers of this file are its own, not those SQL Server gives its
//!   shipped views (unit test `the_system_views_have_their_own_negative_ids`).
//! - `partition_id` = `hobt_id` = [`partition_id`], which packs the database, the object and
//!   the index into one `bigint`, so two partitions of one instance do not share one (unit
//!   test `the_partition_id_is_unique_per_database_object_and_index`). `container_id` of an
//!   allocation unit is that `partition_id` and its `allocation_unit_id` is that
//!   `partition_id` plus [`ALLOCATION_UNIT_STEP`]: in SQL Server, an allocation unit has
//!   `container_id` equal to the `hobt_id` of its partition and an `allocation_unit_id` that
//!   differs from the `partition_id`.
//! - `type` 1 / `type_desc` `IN_ROW_DATA`, `data_space_id` 1 and `total_pages` / `used_pages`
//!   / `data_pages` 0: the row SQL Server publishes for a table without any `INSERT`
//!   (`index_id` 1, `IN_ROW_DATA`, `0`, `0`, `0`). Two rows inserted move them to 9 / 2 / 1
//!   there; VaubanDB keeps the three at 0 and `rows` at 0. Deliberate difference for now.
//! - files: `file_id` 1 is the data file and 2 the log file, named `<database>_data` and
//!   `<database>_log`, `growth` 8192, `max_size` −1 for the data file and 268435456 for the
//!   log, `is_percent_growth` 0, `data_space_id` 1 for the data file and 0 for the log,
//!   `state` 0 / `ONLINE`: the numbers SQL Server publishes for a `CREATE DATABASE`. Three
//!   values are ours: the two logical names, `<database>_data` and `<database>_log` where
//!   SQL Server names the data file after its database (the same suffix for the four system
//!   databases, whose files SQL Server calls `master` / `mastlog`, `is_percent_growth` 1);
//!   [`FILE_SIZE`], 1 024 pages of 8 KB, the `size` of SQL Server following its `model`
//!   database; and the `physical_name`, [`DATA_DIRECTORY`] and the two extensions of
//!   VaubanDB's own file format. The `FILENAME` clause of a `CREATE DATABASE` is ignored.
//!   The three are frozen by the unit test `the_file_rows_are_those_of_a_new_database`.
//!
//! # What these views do not publish
//!
//! The parser refuses `precision` as a bare column name where SQL Server reads it; hence the
//! `[precision]` of [`RESERVED_COLUMN_NAMES`] in the text of `sys.all_columns`. On a fresh
//! database SQL Server publishes thousands of rows in `sys.all_objects` (its shipped views
//! among them), `sys.all_columns`, `sys.partitions` and `sys.allocation_units`, 2 in
//! `sys.database_files`, and **0** in `sys.views` — a shipped view is not in `sys.views`
//! there. VaubanDB publishes the system views in `sys.views`, publishes user objects alone
//! in `sys.all_columns`, `sys.partitions` and `sys.allocation_units`, and keeps its own
//! internal tables out of `sys.all_objects` (the rule `views/sys_tables.rs` follows for
//! `sys.objects`). Deliberate differences for now.

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::{DbId, Row};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{DATABASES_TABLE, SCHEMAS_TABLE, SYSTEM_DATABASES, SYSTEM_SCHEMAS};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::{QualifiedName, TableMeta};
use crate::views::{info_schema, sys_constraints, sys_core, sys_indexes, sys_tables};

/// Internal table of the objects of a database, system views included, read by
/// `sys.all_objects` and `sys.views`.
///
/// A `vauban_sys_*` name of our own.
pub(crate) const ALL_OBJECTS_TABLE: &str = "vauban_sys_all_objects";

/// Internal table of the columns of those objects, read by `sys.all_columns`.
pub(crate) const ALL_COLUMNS_TABLE: &str = "vauban_sys_all_columns";

/// Internal table of the partitions, read by `sys.partitions`.
pub(crate) const PARTITIONS_TABLE: &str = "vauban_sys_partitions";

/// Internal table of the allocation units, read by `sys.allocation_units`.
pub(crate) const ALLOCATION_UNITS_TABLE: &str = "vauban_sys_allocation_units";

/// Internal table of the logical files, read by `sys.database_files` and `sys.master_files`.
pub(crate) const FILES_TABLE: &str = "vauban_sys_files";

/// The schema the seven views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The column names of the seven views that are T-SQL reserved words and are therefore
/// written as delimited identifiers in the text of a view: `rows`, a column of
/// `sys.partitions`, and `precision`, a column of `sys.all_columns` (unit test
/// `a_reserved_column_name_is_delimited`).
const RESERVED_COLUMN_NAMES: [&str; 2] = ["precision", "rows"];

/// `type` and `type_desc` of a system view, as `sys.all_objects` publishes them: the `type`
/// is a `char(2)`, so its second byte is a space.
const VIEW_TYPE: &str = "V ";

/// The `type_desc` that goes with [`VIEW_TYPE`].
const VIEW_TYPE_DESC: &str = "VIEW";

/// `parent_object_id` of an object that is not a constraint: `0`, not `NULL`.
const NO_PARENT: i32 = 0;

/// The `schema_id` written for a schema the bootstrap does not know, as `views/sys_tables.rs`
/// writes it: `0`, which is none of the identifiers of [`SYSTEM_SCHEMAS`].
const UNKNOWN_SCHEMA_ID: i32 = 0;

/// The `object_id` of the first system view, the next ones going down by one.
///
/// Negative, so that no identifier of a user object collides with one: `table.rs` numbers a
/// user object from `FIRST_USER_OBJECT_ID` upwards (unit test
/// `the_system_views_have_their_own_negative_ids`).
const FIRST_SYSTEM_VIEW_OBJECT_ID: i32 = -1;

/// What separates the `allocation_unit_id` of an allocation unit from the `partition_id` of
/// the partition it holds, the two differing in SQL Server (module documentation).
const ALLOCATION_UNIT_STEP: i64 = 1;

/// `index_id` of a heap: `0`.
const HEAP_INDEX_ID: i32 = 0;

/// `index_id` of a clustered index: `1`. The number `views/sys_indexes.rs` publishes for the
/// same index in `sys.indexes`.
const CLUSTERED_INDEX_ID: i32 = 1;

/// The directory the `physical_name` of a logical file is written in.
///
/// A name of ours, with no disk behind it: `storage` keeps its tables in memory, and
/// VaubanDB has its own file format rather than a `.mdf` (unit test
/// `the_file_rows_are_those_of_a_new_database`).
const DATA_DIRECTORY: &str = "vauban/data/";

/// Extension of the data file of a database.
const DATA_EXTENSION: &str = ".vdat";

/// Extension of the log file of a database.
const LOG_EXTENSION: &str = ".vlog";

/// Suffix of the logical name of the log file of a database, as SQL Server names it (`d` /
/// `d_log`).
const LOG_NAME_SUFFIX: &str = "_log";

/// Suffix of the logical name of the data file of a database.
///
/// Ours: SQL Server names that file after its database for a database a client created and
/// gives its own names to the files of a system database (`master` / `mastlog`); one suffix
/// serves the two here (unit test `the_file_rows_are_those_of_a_new_database`).
const DATA_NAME_SUFFIX: &str = "_data";

/// `size` of a file at creation, in 8-KB pages: 1 024, that is 8 MB.
///
/// A value of ours; the `size` SQL Server answers follows its `model` database.
const FILE_SIZE: i32 = 1024;

/// `growth` of a file, in 8-KB pages (`is_percent_growth` 0).
const FILE_GROWTH: i32 = 8192;

/// `max_size` of a data file: `-1`, no bound.
const DATA_MAX_SIZE: i32 = -1;

/// `max_size` of a log file in 8-KB pages (2 TB).
const LOG_MAX_SIZE: i32 = 268_435_456;

/// Where each column of [`ALL_OBJECTS_TABLE`] sits in a [`Row`], as `views/sys_tables.rs`
/// does for its own tables: a writer of a row addresses it through these constants
/// rather than restating the order (unit test
/// `the_column_order_is_the_one_the_constants_name`).
pub(crate) mod objects_columns {
    /// `database_id int NULL`: the database the object belongs to, `NULL` for an object the
    /// four system databases share.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the identifier `sys.all_objects.object_id` publishes.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the object.
    pub(crate) const NAME: usize = 2;
    /// `schema_id int`: the schema the object belongs to.
    pub(crate) const SCHEMA_ID: usize = 3;
    /// `parent_object_id int`: the table of a constraint, `0` for a table or a view.
    pub(crate) const PARENT_OBJECT_ID: usize = 4;
    /// `type char(2)`: `V ` for a view, `U ` for a user table.
    pub(crate) const TYPE: usize = 5;
    /// `type_desc nvarchar(60)`: `VIEW` for a view.
    pub(crate) const TYPE_DESC: usize = 6;
    /// `is_ms_shipped bit`: `1` for a system view, `0` for an object a client created.
    pub(crate) const IS_MS_SHIPPED: usize = 7;
    /// `create_date datetime`: kept in sync with `views/sys_tables.rs`.
    pub(crate) const CREATE_DATE: usize = 8;
    /// `modify_date datetime`: kept in sync with `views/sys_tables.rs`.
    pub(crate) const MODIFY_DATE: usize = 9;
    /// `max_column_id_used int`: no view of this file reads it; the column is here so that a
    /// row of the table of `views/sys_tables.rs` is a row of this one.
    pub(crate) const MAX_COLUMN_ID_USED: usize = 10;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 11;
}

/// Where each column of [`PARTITIONS_TABLE`] sits in a [`Row`]. Same rule as
/// [`objects_columns`].
pub(crate) mod partitions_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table the partition belongs to.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `index_id int`: `0` for a heap, `1` for the clustered index.
    pub(crate) const INDEX_ID: usize = 2;
    /// `partition_id bigint`: the identifier the view publishes as `partition_id` and as
    /// `hobt_id`.
    pub(crate) const PARTITION_ID: usize = 3;
    /// `row_count bigint`: the `rows` column of the view, `0` before any `INSERT`.
    pub(crate) const ROW_COUNT: usize = 4;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 5;
}

/// Where each column of [`ALLOCATION_UNITS_TABLE`] sits in a [`Row`]. Same rule as
/// [`objects_columns`].
pub(crate) mod allocation_units_columns {
    /// `database_id int`: the database the unit belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `allocation_unit_id bigint`.
    pub(crate) const ALLOCATION_UNIT_ID: usize = 1;
    /// `container_id bigint`: the `partition_id` of the partition the unit holds.
    pub(crate) const CONTAINER_ID: usize = 2;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 3;
}

/// Where each column of [`FILES_TABLE`] sits in a [`Row`]. Same rule as [`objects_columns`].
pub(crate) mod files_columns {
    /// `database_id int`: the database the file belongs to, the filter of
    /// `sys.database_files` and a column of `sys.master_files`.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `file_id int`: `1` for the data file, `2` for the log file.
    pub(crate) const FILE_ID: usize = 1;
    /// `type tinyint`: `0` for `ROWS`, `1` for `LOG`.
    pub(crate) const TYPE: usize = 2;
    /// `type_desc nvarchar(60)`: `ROWS` or `LOG`.
    pub(crate) const TYPE_DESC: usize = 3;
    /// `name nvarchar(128)`: logical name of the file.
    pub(crate) const NAME: usize = 4;
    /// `physical_name nvarchar(260)`: the path of the file, ours (module documentation).
    pub(crate) const PHYSICAL_NAME: usize = 5;
    /// `data_space_id int`: `1` for the data file, `0` for the log file.
    pub(crate) const DATA_SPACE_ID: usize = 6;
    /// `size int`: the size in 8-KB pages.
    pub(crate) const SIZE: usize = 7;
    /// `max_size int`: `-1` for a data file that grows without a bound.
    pub(crate) const MAX_SIZE: usize = 8;
    /// `growth int`: pages added at each growth, `is_percent_growth` being 0.
    pub(crate) const GROWTH: usize = 9;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 10;
}

/// The select list of `sys.all_objects`: `(column, expression)`, in the published order.
///
/// The 12 items of `sys.objects` (`views/sys_tables.rs`), read over the copy of that table
/// this file describes; an expression equal to the column name reads [`ALL_OBJECTS_TABLE`].
const ALL_OBJECTS_VIEW: [(&str, &str); 12] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("is_ms_shipped", "is_ms_shipped"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
];

/// The select list of `sys.views`: the 12 items of [`ALL_OBJECTS_VIEW`] and 11 more.
///
/// The values of the 11 are those `sys.views` publishes for a `CREATE VIEW`, compared literal
/// by literal by the unit test `the_literal_columns_are_the_published_values`.
const VIEWS_VIEW: [(&str, &str); 23] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("is_ms_shipped", "is_ms_shipped"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
    ("is_replicated", "CAST(0 AS bit)"),
    ("has_replication_filter", "CAST(0 AS bit)"),
    ("has_opaque_metadata", "CAST(0 AS bit)"),
    ("has_unchecked_assembly_data", "CAST(0 AS bit)"),
    ("with_check_option", "CAST(0 AS bit)"),
    ("is_date_correlation_view", "CAST(0 AS bit)"),
    ("is_tracked_by_cdc", "CAST(0 AS bit)"),
    ("has_snapshot", "CAST(0 AS bit)"),
    ("ledger_view_type", "CAST(0 AS tinyint)"),
    (
        "ledger_view_type_desc",
        "CAST(N'NON_LEDGER_VIEW' AS nvarchar(60))",
    ),
    ("is_dropped_ledger_view", "CAST(0 AS bit)"),
];

/// The select list of `sys.all_columns`: the 40 items of `sys.columns`
/// (`views/sys_tables.rs`), read over the copy of that table this file describes.
const ALL_COLUMNS_VIEW: [(&str, &str); 40] = [
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

/// The select list of `sys.partitions`: 11 items, 4 of which read [`PARTITIONS_TABLE`].
///
/// `hobt_id` reads the `partition_id` column a second time, the two being equal in SQL Server
/// (module documentation); `rows` is the `row_count` column, renamed because `ROWS` is a
/// reserved word (`[rows]`). The literals are the values SQL Server publishes for a table
/// without any `INSERT`.
const PARTITIONS_VIEW: [(&str, &str); 11] = [
    ("partition_id", "partition_id"),
    ("object_id", "object_id"),
    ("index_id", "index_id"),
    ("partition_number", "CAST(1 AS int)"),
    ("hobt_id", "partition_id"),
    ("rows", "row_count"),
    ("filestream_filegroup_id", "CAST(0 AS smallint)"),
    ("data_compression", "CAST(0 AS tinyint)"),
    ("data_compression_desc", "CAST(N'NONE' AS nvarchar(60))"),
    ("xml_compression", "CAST(0 AS bit)"),
    ("xml_compression_desc", "CAST('OFF' AS varchar(3))"),
];

/// The select list of `sys.allocation_units`: 8 items, 2 of which read
/// [`ALLOCATION_UNITS_TABLE`].
///
/// The six literals are the values SQL Server publishes for a table without any `INSERT`
/// (module documentation).
const ALLOCATION_UNITS_VIEW: [(&str, &str); 8] = [
    ("allocation_unit_id", "allocation_unit_id"),
    ("type", "CAST(1 AS tinyint)"),
    ("type_desc", "CAST(N'IN_ROW_DATA' AS nvarchar(60))"),
    ("container_id", "container_id"),
    ("data_space_id", "CAST(1 AS int)"),
    ("total_pages", "CAST(0 AS bigint)"),
    ("used_pages", "CAST(0 AS bigint)"),
    ("data_pages", "CAST(0 AS bigint)"),
];

/// The select list of `sys.database_files`: 30 items, 9 of which read [`FILES_TABLE`].
///
/// The LSN columns are a `numeric(25, 0)` and the two description columns an `nvarchar(60)`:
/// the declared types of the view. VaubanDB writes no log sequence number, so the eight of
/// them are `NULL`, which their nullability allows.
const DATABASE_FILES_VIEW: [(&str, &str); 30] = [
    ("file_id", "file_id"),
    ("file_guid", "CAST(NULL AS uniqueidentifier)"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("data_space_id", "data_space_id"),
    ("name", "name"),
    ("physical_name", "physical_name"),
    ("state", "CAST(0 AS tinyint)"),
    ("state_desc", "CAST(N'ONLINE' AS nvarchar(60))"),
    ("size", "size"),
    ("max_size", "max_size"),
    ("growth", "growth"),
    ("is_media_read_only", "CAST(0 AS bit)"),
    ("is_read_only", "CAST(0 AS bit)"),
    ("is_sparse", "CAST(0 AS bit)"),
    ("is_percent_growth", "CAST(0 AS bit)"),
    ("is_name_reserved", "CAST(0 AS bit)"),
    ("is_persistent_log_buffer", "CAST(0 AS bit)"),
    ("create_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("drop_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("read_only_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("read_write_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("differential_base_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("differential_base_guid", "CAST(NULL AS uniqueidentifier)"),
    ("differential_base_time", "CAST(NULL AS datetime)"),
    ("redo_start_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("redo_start_fork_guid", "CAST(NULL AS uniqueidentifier)"),
    ("redo_target_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("redo_target_fork_guid", "CAST(NULL AS uniqueidentifier)"),
    ("backup_lsn", "CAST(NULL AS numeric(25, 0))"),
];

/// The select list of `sys.master_files`: the 30 items of [`DATABASE_FILES_VIEW`], a
/// `database_id` in front and a `credential_id` behind.
///
/// `database_id` is the column of [`FILES_TABLE`], read without a filter: `sys.master_files` is
/// instance-wide where `sys.database_files` keeps the current database (unit test
/// `master_files_reads_every_database_where_database_files_filters`).
const MASTER_FILES_VIEW: [(&str, &str); 32] = [
    ("database_id", "database_id"),
    ("file_id", "file_id"),
    ("file_guid", "CAST(NULL AS uniqueidentifier)"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("data_space_id", "data_space_id"),
    ("name", "name"),
    ("physical_name", "physical_name"),
    ("state", "CAST(0 AS tinyint)"),
    ("state_desc", "CAST(N'ONLINE' AS nvarchar(60))"),
    ("size", "size"),
    ("max_size", "max_size"),
    ("growth", "growth"),
    ("is_media_read_only", "CAST(0 AS bit)"),
    ("is_read_only", "CAST(0 AS bit)"),
    ("is_sparse", "CAST(0 AS bit)"),
    ("is_percent_growth", "CAST(0 AS bit)"),
    ("is_name_reserved", "CAST(0 AS bit)"),
    ("is_persistent_log_buffer", "CAST(0 AS bit)"),
    ("create_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("drop_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("read_only_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("read_write_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("differential_base_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("differential_base_guid", "CAST(NULL AS uniqueidentifier)"),
    ("differential_base_time", "CAST(NULL AS datetime)"),
    ("redo_start_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("redo_start_fork_guid", "CAST(NULL AS uniqueidentifier)"),
    ("redo_target_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("redo_target_fork_guid", "CAST(NULL AS uniqueidentifier)"),
    ("backup_lsn", "CAST(NULL AS numeric(25, 0))"),
    ("credential_id", "CAST(NULL AS int)"),
];

/// The filter of a view read over [`ALL_OBJECTS_TABLE`] or [`ALL_COLUMNS_TABLE`]: the objects
/// of the current database, and those whose `database_id` is `NULL`.
const ANY_DATABASE_FILTER: &str = "database_id = DB_ID()\n    OR database_id IS NULL";

/// The internal tables this file describes, in the order of the table of the module
/// documentation.
///
/// The bootstrap creates them in `master`. [`ALL_OBJECTS_TABLE`] is the one of the five that
/// carries rows — the system views, which `sys.views` publishes; the rows of a user object are
/// built by the functions below (module documentation).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    let own = own_views();
    vec![
        InternalTableDef {
            name: ALL_OBJECTS_TABLE.to_owned(),
            columns: objects_table_columns(),
            clustered_key: None,
            rows: system_view_rows(&installed_system_views(&own)),
            views: [
                views_of("all_objects", &all_objects_definition()),
                views_of("views", &views_definition()),
            ]
            .concat(),
        },
        InternalTableDef {
            name: ALL_COLUMNS_TABLE.to_owned(),
            columns: columns_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of("all_columns", &all_columns_definition()),
        },
        InternalTableDef {
            name: PARTITIONS_TABLE.to_owned(),
            columns: partitions_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of("partitions", &partitions_definition()),
        },
        InternalTableDef {
            name: ALLOCATION_UNITS_TABLE.to_owned(),
            columns: allocation_units_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of("allocation_units", &allocation_units_definition()),
        },
        InternalTableDef {
            name: FILES_TABLE.to_owned(),
            columns: files_table_columns(),
            clustered_key: None,
            rows: system_database_file_rows(),
            views: [
                views_of("database_files", &database_files_definition()),
                views_of("master_files", &master_files_definition()),
            ]
            .concat(),
        },
    ]
}

/// The views this file installs, which [`installed_system_views`] needs before
/// [`internal_tables`] can build its rows: the seven of the table of the module documentation.
///
/// Written apart from [`internal_tables`] so that the rows of [`ALL_OBJECTS_TABLE`] can name
/// the views of this file without that function calling itself.
fn own_views() -> Vec<SystemViewDef> {
    [
        views_of("all_objects", &all_objects_definition()),
        views_of("views", &views_definition()),
        views_of("all_columns", &all_columns_definition()),
        views_of("partitions", &partitions_definition()),
        views_of("allocation_units", &allocation_units_definition()),
        views_of("database_files", &database_files_definition()),
        views_of("master_files", &master_files_definition()),
    ]
    .concat()
}

/// The distinct system views the files of `views/` install, in the order of
/// [`crate::views::internal_tables`], each one named once.
///
/// A view is installed in each of the four system databases under the same name, so the list
/// keeps the first copy of each `(schema, name)` pair: a row of [`ALL_OBJECTS_TABLE`] whose
/// `database_id` is `NULL` stands for the four (module documentation). The other five files
/// are read through their own `internal_tables()`, which this one does not call on itself —
/// `own` is what [`own_views`] built — and `sys.databases` / `sys.schemas` through
/// [`crate::views::sys_core::bootstrap_views`], those two views being carried by the tables
/// `bootstrap.rs` owns rather than by a table of a file of `views/` (integration test
/// `the_objects_copy_carries_the_system_views_of_the_catalogue`, which reads `databases`).
fn installed_system_views(own: &[SystemViewDef]) -> Vec<QualifiedName> {
    let described = [
        sys_tables::internal_tables(),
        sys_core::internal_tables(),
        sys_indexes::internal_tables(),
        info_schema::internal_tables(),
        sys_constraints::internal_tables(),
    ];
    let of_the_bootstrap = [
        sys_core::bootstrap_views(DATABASES_TABLE),
        sys_core::bootstrap_views(SCHEMAS_TABLE),
    ];
    let installed = described
        .iter()
        .flatten()
        .flat_map(|table| table.views.iter())
        .chain(of_the_bootstrap.iter().flatten())
        .chain(own.iter());
    let mut names: Vec<QualifiedName> = Vec::new();
    for view in installed {
        if !names.iter().any(|kept| {
            kept.schema.eq_ignore_ascii_case(&view.name.schema)
                && kept.name.eq_ignore_ascii_case(&view.name.name)
        }) {
            names.push(view.name.clone());
        }
    }
    names
}

/// The rows of [`ALL_OBJECTS_TABLE`] the bootstrap writes: one per system view, numbered from
/// [`FIRST_SYSTEM_VIEW_OBJECT_ID`] downwards in the order of `views`.
fn system_view_rows(views: &[QualifiedName]) -> Vec<Row> {
    views
        .iter()
        .enumerate()
        .map(|(rank, view)| {
            let mut row = vec![Value::Null; objects_columns::WIDTH];
            // `database_id` stays `Null`: the view is installed in the four system databases.
            row[objects_columns::OBJECT_ID] =
                Value::I32(FIRST_SYSTEM_VIEW_OBJECT_ID - i32::try_from(rank).unwrap_or(0));
            row[objects_columns::NAME] = text(&view.name);
            row[objects_columns::SCHEMA_ID] = Value::I32(schema_id(&view.schema));
            row[objects_columns::PARENT_OBJECT_ID] = Value::I32(NO_PARENT);
            row[objects_columns::TYPE] = text(VIEW_TYPE);
            row[objects_columns::TYPE_DESC] = text(VIEW_TYPE_DESC);
            row[objects_columns::IS_MS_SHIPPED] = Value::Bit(true);
            row[objects_columns::CREATE_DATE] = Value::Null;
            row[objects_columns::MODIFY_DATE] = Value::Null;
            row[objects_columns::MAX_COLUMN_ID_USED] = Value::I32(0);
            Row(row)
        })
        .collect()
}

/// The rows of [`FILES_TABLE`] the bootstrap writes: the two files of each of the four system
/// databases, under the identifiers a first bootstrap hands out (module documentation).
fn system_database_file_rows() -> Vec<Row> {
    SYSTEM_DATABASES
        .iter()
        .enumerate()
        .flat_map(|(rank, name)| file_rows(i32::try_from(rank).unwrap_or(0) + 1, name))
        .collect()
}

/// The rows of [`ALL_OBJECTS_TABLE`] of the user objects of a catalogue: those of the table of
/// `views/sys_tables.rs`, the two shapes being equal (module documentation).
///
/// # Errors
///
/// Those of [`crate::views::sys_tables::object_rows`].
pub(crate) fn user_object_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    sys_tables::object_rows(tables)
}

/// The rows of [`ALL_COLUMNS_TABLE`] of the columns of the user tables of a catalogue: those
/// of the table of `views/sys_tables.rs`.
///
/// # Errors
///
/// Those of [`crate::views::sys_tables::column_rows`].
pub(crate) fn user_column_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    sys_tables::column_rows(tables)
}

/// The rows of [`PARTITIONS_TABLE`] for `tables`: one per table, its heap or its clustered
/// index.
///
/// A table has one partition — `CREATE PARTITION FUNCTION` is not served — so
/// `partition_number` is the constant `1` of the view and the `index_id` is [`HEAP_INDEX_ID`]
/// or [`CLUSTERED_INDEX_ID`], the numbers `views/sys_indexes.rs` gives the same index in
/// `sys.indexes` (unit test `user_table_has_one_partition`). A nonclustered index has no row
/// here, where `sys.partitions` gives one partition per index in SQL Server: the heap and
/// the clustered index alone are published (`user_table_has_one_partition`).
///
/// # Errors
///
/// [`InternalError::Bug`] when a [`DbId`] does not fit in the `int` the view publishes, as
/// `views/sys_tables.rs` reports on the same value.
pub(crate) fn partition_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    tables
        .iter()
        .map(|table| {
            let database = database_id(table.database)?;
            let index = index_id(table);
            let mut row = vec![Value::Null; partitions_columns::WIDTH];
            row[partitions_columns::DATABASE_ID] = Value::I32(database);
            row[partitions_columns::OBJECT_ID] = Value::I32(table.id.0);
            row[partitions_columns::INDEX_ID] = Value::I32(index);
            row[partitions_columns::PARTITION_ID] =
                Value::I64(partition_id(database, table.id.0, index));
            row[partitions_columns::ROW_COUNT] = Value::I64(0);
            Ok(Row(row))
        })
        .collect()
}

/// The rows of [`ALLOCATION_UNITS_TABLE`] for `tables`: one per partition of
/// [`partition_rows`], holding it.
///
/// # Errors
///
/// Those of [`partition_rows`].
pub(crate) fn allocation_unit_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    tables
        .iter()
        .map(|table| {
            let database = database_id(table.database)?;
            let container = partition_id(database, table.id.0, index_id(table));
            let mut row = vec![Value::Null; allocation_units_columns::WIDTH];
            row[allocation_units_columns::DATABASE_ID] = Value::I32(database);
            row[allocation_units_columns::ALLOCATION_UNIT_ID] =
                Value::I64(container.saturating_add(ALLOCATION_UNIT_STEP));
            row[allocation_units_columns::CONTAINER_ID] = Value::I64(container);
            Ok(Row(row))
        })
        .collect()
}

/// What one row of [`FILES_TABLE`] carries that the other rows do not: the four values that
/// tell a data file from a log file.
struct FileKind {
    /// `file_id`: 1 for the data file, 2 for the log file.
    file_id: i32,
    /// `type` and `type_desc`: 0 / `ROWS`, or 1 / `LOG`.
    file_type: u8,
    /// Extension of the `physical_name`.
    extension: &'static str,
    /// `data_space_id`: 1 for the data file, 0 for the log file.
    data_space_id: i32,
    /// `max_size`.
    max_size: i32,
}

/// The data file of a database, as SQL Server reports it for a created database.
const DATA_FILE: FileKind = FileKind {
    file_id: 1,
    file_type: 0,
    extension: DATA_EXTENSION,
    data_space_id: 1,
    max_size: DATA_MAX_SIZE,
};

/// The log file of a database, as SQL Server reports it for a created database.
const LOG_FILE: FileKind = FileKind {
    file_id: 2,
    file_type: 1,
    extension: LOG_EXTENSION,
    data_space_id: 0,
    max_size: LOG_MAX_SIZE,
};

/// The rows of [`FILES_TABLE`] of the database `name`, of identifier `database`: its data file
/// and its log file, in `file_id` order.
///
/// The numbers are those the module documentation lists, the two logical names are ours. The
/// bootstrap calls this one for each of the four system databases
/// ([`system_database_file_rows`]) (integration test `master_has_database_files_rows`).
pub(crate) fn file_rows(database: i32, name: &str) -> Vec<Row> {
    vec![
        file_row(database, &format!("{name}{DATA_NAME_SUFFIX}"), &DATA_FILE),
        file_row(database, &format!("{name}{LOG_NAME_SUFFIX}"), &LOG_FILE),
    ]
}

/// One row of [`FILES_TABLE`], of logical name `name`. See [`file_rows`].
fn file_row(database: i32, name: &str, kind: &FileKind) -> Row {
    let mut row = vec![Value::Null; files_columns::WIDTH];
    row[files_columns::DATABASE_ID] = Value::I32(database);
    row[files_columns::FILE_ID] = Value::I32(kind.file_id);
    row[files_columns::TYPE] = Value::I8(kind.file_type);
    row[files_columns::TYPE_DESC] = text(if kind.file_type == 0 { "ROWS" } else { "LOG" });
    row[files_columns::NAME] = text(name);
    row[files_columns::PHYSICAL_NAME] = text(&format!("{DATA_DIRECTORY}{name}{}", kind.extension));
    row[files_columns::DATA_SPACE_ID] = Value::I32(kind.data_space_id);
    row[files_columns::SIZE] = Value::I32(FILE_SIZE);
    row[files_columns::MAX_SIZE] = Value::I32(kind.max_size);
    row[files_columns::GROWTH] = Value::I32(FILE_GROWTH);
    Row(row)
}

/// The `index_id` of the one partition of `table`: [`CLUSTERED_INDEX_ID`] when the table has a
/// clustered index, [`HEAP_INDEX_ID`] otherwise.
fn index_id(table: &TableMeta) -> i32 {
    if table.clustered.is_some() {
        CLUSTERED_INDEX_ID
    } else {
        HEAP_INDEX_ID
    }
}

/// The `partition_id` — and `hobt_id`, and `container_id` of the allocation unit — of the
/// partition `index` of the object `object` of the database `database`.
///
/// The three numbers are packed into one `bigint`, the index in the low byte and the object
/// above it, so that two partitions of one instance do not share an identifier (unit test
/// `the_partition_id_is_unique_per_database_object_and_index`). A value of ours: SQL Server
/// hands out identifiers of its own, of which the relation `container_id = hobt_id` is kept
/// (module documentation).
fn partition_id(database: i32, object: i32, index: i32) -> i64 {
    (i64::from(database) << 40) | (i64::from(object) << 8) | i64::from(index)
}

/// The `int` a [`DbId`] is published as, as `views/sys_tables.rs` publishes it.
///
/// # Errors
///
/// [`InternalError::Bug`], as `bootstrap.rs` does on the same value.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "sys.partitions: database id {id} does not fit in an int"
        ))
        .into()
    })
}

/// The `schema_id` of the schema named `name`, [`UNKNOWN_SCHEMA_ID`] when the bootstrap
/// created no schema of that name.
///
/// Compared without regard to case, as `SQL_Latin1_General_CP1_CI_AS` compares identifiers.
fn schema_id(name: &str) -> i32 {
    SYSTEM_SCHEMAS
        .iter()
        .find(|(schema, _, _)| schema.eq_ignore_ascii_case(name))
        .map_or(UNKNOWN_SCHEMA_ID, |(_, id, _)| *id)
}

/// The columns of [`ALL_OBJECTS_TABLE`], in the order of [`objects_columns`]: those of the
/// table of `views/sys_tables.rs` with a nullable `database_id`.
fn objects_table_columns() -> Vec<InternalColumnDef> {
    copy_of(sys_tables::OBJECTS_TABLE)
}

/// The columns of [`ALL_COLUMNS_TABLE`]: those of the columns table of `views/sys_tables.rs` with a
/// nullable `database_id`.
fn columns_table_columns() -> Vec<InternalColumnDef> {
    copy_of(sys_tables::COLUMNS_TABLE)
}

/// The columns of the table of `views/sys_tables.rs` called `name`, with a nullable
/// `database_id`.
///
/// Read from that file rather than restated here, so that the two tables do not drift apart
/// (unit test `the_two_copies_are_the_tables_of_sys_tables_with_a_nullable_database_id`). An
/// empty vector would mean that file describes no such table, which the unit test named above
/// refuses as well.
fn copy_of(name: &str) -> Vec<InternalColumnDef> {
    let mut columns: Vec<InternalColumnDef> = sys_tables::internal_tables()
        .into_iter()
        .find(|table| table.name == name)
        .map(|table| table.columns)
        .unwrap_or_default();
    for column in &mut columns {
        if column.name == "database_id" {
            column.ty.nullable = true;
        }
    }
    columns
}

/// The columns of [`PARTITIONS_TABLE`], in the order of [`partitions_columns`].
fn partitions_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        column("index_id", SqlType::Int, false),
        column("partition_id", SqlType::BigInt, false),
        column("row_count", SqlType::BigInt, false),
    ]
}

/// The columns of [`ALLOCATION_UNITS_TABLE`], in the order of [`allocation_units_columns`].
fn allocation_units_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("allocation_unit_id", SqlType::BigInt, false),
        column("container_id", SqlType::BigInt, false),
    ]
}

/// The columns of [`FILES_TABLE`], in the order of [`files_columns`].
///
/// `physical_name` is an `nvarchar(260)` and `type_desc` an `nvarchar(60)`, the declared types
/// of the columns of `sys.database_files` (`max_length` 520 and 120).
fn files_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("file_id", SqlType::Int, false),
        column("type", SqlType::TinyInt, false),
        column("type_desc", SqlType::NVarChar(Len::Fixed(60)), false),
        column("name", SYSNAME, false),
        column("physical_name", SqlType::NVarChar(Len::Fixed(260)), false),
        column("data_space_id", SqlType::Int, false),
        column("size", SqlType::Int, false),
        column("max_size", SqlType::Int, false),
        column("growth", SqlType::Int, false),
    ]
}

/// The text of `sys.all_objects`.
fn all_objects_definition() -> String {
    definition(
        &ALL_OBJECTS_VIEW,
        ALL_OBJECTS_TABLE,
        Some(ANY_DATABASE_FILTER),
    )
}

/// The text of `sys.views`: the objects of [`ALL_OBJECTS_TABLE`] whose type is a view.
fn views_definition() -> String {
    definition(
        &VIEWS_VIEW,
        ALL_OBJECTS_TABLE,
        Some(&format!(
            "({ANY_DATABASE_FILTER})\n   AND type = '{VIEW_TYPE}'"
        )),
    )
}

/// The text of `sys.all_columns`.
fn all_columns_definition() -> String {
    definition(
        &ALL_COLUMNS_VIEW,
        ALL_COLUMNS_TABLE,
        Some(ANY_DATABASE_FILTER),
    )
}

/// The text of `sys.partitions`.
fn partitions_definition() -> String {
    definition(
        &PARTITIONS_VIEW,
        PARTITIONS_TABLE,
        Some("database_id = DB_ID()"),
    )
}

/// The text of `sys.allocation_units`.
fn allocation_units_definition() -> String {
    definition(
        &ALLOCATION_UNITS_VIEW,
        ALLOCATION_UNITS_TABLE,
        Some("database_id = DB_ID()"),
    )
}

/// The text of `sys.database_files`: the files of the database the client is reading from.
fn database_files_definition() -> String {
    definition(
        &DATABASE_FILES_VIEW,
        FILES_TABLE,
        Some("database_id = DB_ID()"),
    )
}

/// The text of `sys.master_files`: the files of the databases of the instance, without a
/// filter.
fn master_files_definition() -> String {
    definition(&MASTER_FILES_VIEW, FILES_TABLE, None)
}

/// The same view installed in the four system databases, `sys.<name>` in each of them.
///
/// The seven views filter on the current database or on nothing, so the four definitions share
/// their text (unit test `the_views_are_installed_in_the_four_system_databases`). A database
/// created by `CREATE DATABASE` receives no copies yet, as for the other files of `views/`.
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
/// filter.
///
/// Same layout as `views/sys_core.rs` and `views/sys_tables.rs`, which own the other views:
/// one select item per line, an item whose expression is its own name written bare, the others
/// as `<expression> AS <name>`, so the name of a column is the last identifier of its item.
/// The three files each hold their own copy of this helper.
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
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};

    use super::*;
    use crate::catalog::Catalog;
    use crate::def::{ColumnDef, ConstraintDef, SortedColumn, TableDef};

    /// The ordered column names of `sys.all_objects` in SQL Server 2022.
    ///
    /// Frozen here so that a change of the text of a view has to face the list again.
    const PUBLISHED_ALL_OBJECTS_COLUMNS: [&str; 12] = [
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
    /// The ordered column names of `sys.all_columns`.
    const PUBLISHED_ALL_COLUMNS_COLUMNS: [&str; 40] = [
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
    /// The ordered column names of `sys.views`.
    const PUBLISHED_VIEWS_COLUMNS: [&str; 23] = [
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
        "is_replicated",
        "has_replication_filter",
        "has_opaque_metadata",
        "has_unchecked_assembly_data",
        "with_check_option",
        "is_date_correlation_view",
        "is_tracked_by_cdc",
        "has_snapshot",
        "ledger_view_type",
        "ledger_view_type_desc",
        "is_dropped_ledger_view",
    ];
    /// The ordered column names of `sys.partitions`.
    const PUBLISHED_PARTITIONS_COLUMNS: [&str; 11] = [
        "partition_id",
        "object_id",
        "index_id",
        "partition_number",
        "hobt_id",
        "rows",
        "filestream_filegroup_id",
        "data_compression",
        "data_compression_desc",
        "xml_compression",
        "xml_compression_desc",
    ];
    /// The ordered column names of `sys.allocation_units`.
    const PUBLISHED_ALLOCATION_UNITS_COLUMNS: [&str; 8] = [
        "allocation_unit_id",
        "type",
        "type_desc",
        "container_id",
        "data_space_id",
        "total_pages",
        "used_pages",
        "data_pages",
    ];
    /// The ordered column names of `sys.database_files`.
    const PUBLISHED_DATABASE_FILES_COLUMNS: [&str; 30] = [
        "file_id",
        "file_guid",
        "type",
        "type_desc",
        "data_space_id",
        "name",
        "physical_name",
        "state",
        "state_desc",
        "size",
        "max_size",
        "growth",
        "is_media_read_only",
        "is_read_only",
        "is_sparse",
        "is_percent_growth",
        "is_name_reserved",
        "is_persistent_log_buffer",
        "create_lsn",
        "drop_lsn",
        "read_only_lsn",
        "read_write_lsn",
        "differential_base_lsn",
        "differential_base_guid",
        "differential_base_time",
        "redo_start_lsn",
        "redo_start_fork_guid",
        "redo_target_lsn",
        "redo_target_fork_guid",
        "backup_lsn",
    ];
    /// The ordered column names of `sys.master_files`.
    const PUBLISHED_MASTER_FILES_COLUMNS: [&str; 32] = [
        "database_id",
        "file_id",
        "file_guid",
        "type",
        "type_desc",
        "data_space_id",
        "name",
        "physical_name",
        "state",
        "state_desc",
        "size",
        "max_size",
        "growth",
        "is_media_read_only",
        "is_read_only",
        "is_sparse",
        "is_percent_growth",
        "is_name_reserved",
        "is_persistent_log_buffer",
        "create_lsn",
        "drop_lsn",
        "read_only_lsn",
        "read_write_lsn",
        "differential_base_lsn",
        "differential_base_guid",
        "differential_base_time",
        "redo_start_lsn",
        "redo_start_fork_guid",
        "redo_target_lsn",
        "redo_target_fork_guid",
        "backup_lsn",
        "credential_id",
    ];
    /// The row `sys.views` publishes for a user view (`CREATE VIEW dbo.views_target …`).
    ///
    /// Frozen here so that a change of the text of a view has to face the row again.
    const PUBLISHED_VIEWS_ROW: [(&str, Option<&str>); 23] = [
        ("name", Some("views_target")),
        ("object_id", Some("917578307")),
        ("principal_id", None),
        ("schema_id", Some("1")),
        ("parent_object_id", Some("0")),
        ("type", Some("V ")),
        ("type_desc", Some("VIEW")),
        ("create_date", Some("2026-01-01T00:00:00.000")),
        ("modify_date", Some("2026-01-01T00:00:00.000")),
        ("is_ms_shipped", Some("0")),
        ("is_published", Some("0")),
        ("is_schema_published", Some("0")),
        ("is_replicated", Some("0")),
        ("has_replication_filter", Some("0")),
        ("has_opaque_metadata", Some("0")),
        ("has_unchecked_assembly_data", Some("0")),
        ("with_check_option", Some("0")),
        ("is_date_correlation_view", Some("0")),
        ("is_tracked_by_cdc", Some("0")),
        ("has_snapshot", Some("0")),
        ("ledger_view_type", Some("0")),
        ("ledger_view_type_desc", Some("NON_LEDGER_VIEW")),
        ("is_dropped_ledger_view", Some("0")),
    ];

    /// The seven views of this file, as `(name, definition, published column names)`.
    fn views_and_published_columns() -> Vec<(&'static str, String, Vec<&'static str>)> {
        vec![
            (
                "all_objects",
                all_objects_definition(),
                PUBLISHED_ALL_OBJECTS_COLUMNS.to_vec(),
            ),
            (
                "all_columns",
                all_columns_definition(),
                PUBLISHED_ALL_COLUMNS_COLUMNS.to_vec(),
            ),
            (
                "views",
                views_definition(),
                PUBLISHED_VIEWS_COLUMNS.to_vec(),
            ),
            (
                "partitions",
                partitions_definition(),
                PUBLISHED_PARTITIONS_COLUMNS.to_vec(),
            ),
            (
                "allocation_units",
                allocation_units_definition(),
                PUBLISHED_ALLOCATION_UNITS_COLUMNS.to_vec(),
            ),
            (
                "database_files",
                database_files_definition(),
                PUBLISHED_DATABASE_FILES_COLUMNS.to_vec(),
            ),
            (
                "master_files",
                master_files_definition(),
                PUBLISHED_MASTER_FILES_COLUMNS.to_vec(),
            ),
        ]
    }

    /// The column names the text of a view publishes: the last identifier of each select item,
    /// brackets removed.
    fn published_columns(definition: &str) -> Vec<String> {
        definition
            .lines()
            .take_while(|line| !line.starts_with("  FROM"))
            .map(|line| {
                let item = line
                    .trim_start_matches("SELECT ")
                    .trim()
                    .trim_end_matches(',');
                let name = item.rsplit(' ').next().unwrap_or(item);
                name.trim_matches(|c| c == '[' || c == ']').to_owned()
            })
            .collect()
    }

    /// A bootstrapped catalogue over a fresh `MemoryStorage`.
    fn instance() -> (Catalog, Arc<TransactionManager>) {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog =
            Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
        (catalog, txn)
    }

    /// A column of a definition, with its type and nothing else on it.
    fn column_def(name: &str, ty: SqlType) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, false),
            default: None,
            identity: None,
            computed: None,
        }
    }

    /// A table of `master`, a heap or with a clustered primary key, created through the
    /// catalogue.
    fn user_table(name: &str, clustered_key: bool) -> TableMeta {
        let (catalog, txn) = instance();
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let constraints = if clustered_key {
            vec![ConstraintDef::PrimaryKey {
                name: Some(format!("pk_{name}")),
                columns: vec![SortedColumn {
                    column: "id".to_owned(),
                    descending: false,
                }],
                clustered: true,
            }]
        } else {
            Vec::new()
        };
        let meta = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: name.to_owned(),
                    },
                    columns: vec![
                        column_def("id", SqlType::Int),
                        column_def("label", SqlType::NVarChar(Len::Fixed(30))),
                    ],
                    constraints,
                },
            )
            .expect("create_table");
        txn.commit(handle).expect("commit");
        meta
    }

    /// The text of a value, empty for anything else than a string.
    fn text_of(value: &Value) -> String {
        match value {
            Value::String(stored) => stored.text.clone(),
            _ => String::new(),
        }
    }

    /// The table this file describes under `name`.
    fn described(name: &str) -> InternalTableDef {
        internal_tables()
            .into_iter()
            .find(|table| table.name == name)
            .expect("the table is described")
    }

    #[test]
    fn the_columns_of_the_seven_views_are_the_published_ones() {
        for (name, definition, published) in views_and_published_columns() {
            assert_eq!(
                published_columns(&definition),
                published,
                "the select list of sys.{name} is the published one"
            );
        }
    }

    #[test]
    fn no_definition_holds_a_join() {
        for (name, definition, _) in views_and_published_columns() {
            assert!(
                !definition.to_uppercase().contains("JOIN"),
                "sys.{name} reads one table"
            );
            assert_eq!(
                definition.matches("  FROM ").count(),
                1,
                "sys.{name} has one FROM"
            );
        }
    }

    #[test]
    fn a_reserved_column_name_is_delimited() {
        // `rows` and `precision` are the two reserved words among the 156 column names of the
        // seven views.
        assert!(partitions_definition().contains("row_count AS [rows]"));
        assert!(all_columns_definition().contains("[precision]"));
        assert_eq!(identifier("rows"), "[rows]");
        assert_eq!(identifier("partition_id"), "partition_id");
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        for table in internal_tables() {
            for view in table.views.chunks(SYSTEM_DATABASES.len()) {
                let databases: Vec<&str> = view
                    .iter()
                    .map(|installed| installed.name.database.as_str())
                    .collect();
                assert_eq!(databases, SYSTEM_DATABASES, "{:?}", table.name);
                for installed in view {
                    assert_eq!(installed.name.schema, VIEW_SCHEMA);
                    assert_eq!(installed.definition, view[0].definition);
                }
            }
        }
        // The seven views of the file, four copies each.
        let installed: usize = internal_tables()
            .iter()
            .map(|table| table.views.len())
            .sum();
        assert_eq!(installed, 7 * SYSTEM_DATABASES.len());
    }

    #[test]
    fn the_two_copies_are_the_tables_of_sys_tables_with_a_nullable_database_id() {
        for (mine, theirs) in [
            (ALL_OBJECTS_TABLE, sys_tables::OBJECTS_TABLE),
            (ALL_COLUMNS_TABLE, sys_tables::COLUMNS_TABLE),
        ] {
            let copy = described(mine).columns;
            let source = sys_tables::internal_tables()
                .into_iter()
                .find(|table| table.name == theirs)
                .expect("the table of views/sys_tables.rs is described")
                .columns;
            assert!(!copy.is_empty());
            assert_eq!(copy.len(), source.len(), "{mine} is as wide as {theirs}");
            for (copied, original) in copy.iter().zip(source.iter()) {
                assert_eq!(copied.name, original.name);
                assert_eq!(copied.ty.ty, original.ty.ty);
                let nullable = copied.name == "database_id" || original.ty.nullable;
                assert_eq!(copied.ty.nullable, nullable, "{}", copied.name);
            }
        }
    }

    #[test]
    fn the_column_order_is_the_one_the_constants_name() {
        let objects = described(ALL_OBJECTS_TABLE).columns;
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

        let partitions = described(PARTITIONS_TABLE).columns;
        assert_eq!(partitions.len(), partitions_columns::WIDTH);
        assert_eq!(
            partitions[partitions_columns::PARTITION_ID].name,
            "partition_id"
        );
        assert_eq!(partitions[partitions_columns::ROW_COUNT].name, "row_count");

        let units = described(ALLOCATION_UNITS_TABLE).columns;
        assert_eq!(units.len(), allocation_units_columns::WIDTH);
        assert_eq!(
            units[allocation_units_columns::ALLOCATION_UNIT_ID].name,
            "allocation_unit_id"
        );
        assert_eq!(
            units[allocation_units_columns::CONTAINER_ID].name,
            "container_id"
        );

        let files = described(FILES_TABLE).columns;
        assert_eq!(files.len(), files_columns::WIDTH);
        assert_eq!(files[files_columns::DATABASE_ID].name, "database_id");
        assert_eq!(files[files_columns::FILE_ID].name, "file_id");
        assert_eq!(files[files_columns::PHYSICAL_NAME].name, "physical_name");
        assert_eq!(files[files_columns::GROWTH].name, "growth");
    }

    #[test]
    fn sys_views_lists_sys_tables_not_user_views() {
        let rows = described(ALL_OBJECTS_TABLE).rows;
        let names: Vec<String> = rows
            .iter()
            .map(|row| text_of(&row.0[objects_columns::NAME]))
            .collect();
        // The view `sys.tables` of `views/sys_tables.rs` and the seven of this file have a row.
        for expected in [
            "tables",
            "databases",
            "schemas",
            "all_objects",
            "views",
            "all_columns",
            "partitions",
            "allocation_units",
            "database_files",
            "master_files",
        ] {
            assert!(names.contains(&expected.to_owned()), "{names:?}");
        }
        // Each row is a shipped view, so `sys.views` publishes no user object; a user table is
        // of type `U `, which the filter `type = 'V '` of the view leaves out.
        for row in &rows {
            assert_eq!(text_of(&row.0[objects_columns::TYPE]), VIEW_TYPE);
            assert_eq!(row.0[objects_columns::IS_MS_SHIPPED], Value::Bit(true));
            assert_eq!(row.0[objects_columns::DATABASE_ID], Value::Null);
        }
        let table = user_table("t_not_a_view", false);
        let user = user_object_rows(&[table]).expect("object rows");
        assert_eq!(text_of(&user[0].0[objects_columns::TYPE]), "U ");
        assert!(views_definition().contains("AND type = 'V '"));
    }

    #[test]
    fn all_objects_contains_user_table_and_sys_view() {
        let table = user_table("t_all_objects_target", false);
        let mut rows = described(ALL_OBJECTS_TABLE).rows;
        rows.extend(user_object_rows(&[table]).expect("object rows"));
        let names: Vec<String> = rows
            .iter()
            .map(|row| text_of(&row.0[objects_columns::NAME]))
            .collect();
        assert!(
            names.contains(&"t_all_objects_target".to_owned()),
            "{names:?}"
        );
        assert!(names.contains(&"tables".to_owned()), "{names:?}");
        // The user row carries the identifier of its database, the row of a system view does
        // not, and the filter of the view reads both.
        let user = rows
            .iter()
            .find(|row| text_of(&row.0[objects_columns::NAME]) == "t_all_objects_target")
            .expect("the user row is there");
        assert_eq!(user.0[objects_columns::DATABASE_ID], Value::I32(1));
        assert!(all_objects_definition().contains("database_id = DB_ID()"));
        assert!(all_objects_definition().contains("OR database_id IS NULL"));
    }

    #[test]
    fn the_system_views_have_their_own_negative_ids() {
        let rows = described(ALL_OBJECTS_TABLE).rows;
        let ids: Vec<i32> = rows
            .iter()
            .map(|row| match row.0[objects_columns::OBJECT_ID] {
                Value::I32(id) => id,
                _ => 0,
            })
            .collect();
        assert_eq!(ids[0], FIRST_SYSTEM_VIEW_OBJECT_ID);
        for (rank, id) in ids.iter().enumerate() {
            assert!(*id < 0, "{ids:?}");
            assert_eq!(
                *id,
                FIRST_SYSTEM_VIEW_OBJECT_ID - i32::try_from(rank).expect("a rank fits in an int")
            );
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "two views share an id: {ids:?}");
        // Six identifiers SQL Server hands out to views this file installs are outside the
        // range this one uses.
        for other in [-213, -216, -385, -391, -400, -448] {
            assert!(!ids.contains(&other), "{ids:?}");
        }
    }

    #[test]
    fn user_table_has_one_partition() {
        // The two numbers SQL Server answers for the same pair of tables, written here rather
        // than read from the two constants, so that a change of a constant faces them.
        assert_eq!((HEAP_INDEX_ID, CLUSTERED_INDEX_ID), (0, 1));
        for (name, clustered, expected) in [("t_heap", false, 0), ("t_clustered", true, 1)] {
            let table = user_table(name, clustered);
            let rows = partition_rows(std::slice::from_ref(&table)).expect("partition rows");
            assert_eq!(rows.len(), 1, "{name} has one partition");
            let row = &rows[0].0;
            assert_eq!(row[partitions_columns::INDEX_ID], Value::I32(expected));
            assert_eq!(row[partitions_columns::OBJECT_ID], Value::I32(table.id.0));
            // `rows` is 0 before any `INSERT`, which is the bound of this file.
            assert_eq!(row[partitions_columns::ROW_COUNT], Value::I64(0));
            assert_eq!(
                row[partitions_columns::PARTITION_ID],
                Value::I64(partition_id(1, table.id.0, expected))
            );
            // The view reads that one column twice, as `partition_id` and as `hobt_id`.
            assert!(partitions_definition().contains("partition_id AS hobt_id"));
        }
    }

    #[test]
    fn the_partition_id_is_unique_per_database_object_and_index() {
        let ids = [
            partition_id(1, 1_000_000, 0),
            partition_id(1, 1_000_000, 1),
            partition_id(1, 1_000_001, 0),
            partition_id(2, 1_000_000, 0),
        ];
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "{ids:?}");
        assert!(ids.iter().all(|id| *id > 0), "{ids:?}");
    }

    #[test]
    fn allocation_units_follow_their_partition() {
        let table = user_table("t_units", true);
        let partitions = partition_rows(std::slice::from_ref(&table)).expect("partition rows");
        let units = allocation_unit_rows(std::slice::from_ref(&table)).expect("unit rows");
        assert_eq!(units.len(), partitions.len());
        let partition = &partitions[0].0[partitions_columns::PARTITION_ID];
        let unit = &units[0].0;
        // `container_id` is the `hobt_id` of the partition, as in SQL Server, and the
        // identifier of the unit is not that of the partition, as in SQL Server.
        assert_eq!(&unit[allocation_units_columns::CONTAINER_ID], partition);
        assert_ne!(
            &unit[allocation_units_columns::ALLOCATION_UNIT_ID],
            partition
        );
        assert!(allocation_units_definition().contains("CAST(N'IN_ROW_DATA' AS nvarchar(60))"));
        assert!(allocation_units_definition().contains("CAST(0 AS bigint) AS total_pages"));
    }

    #[test]
    fn the_bootstrap_rows_cover_the_four_system_databases() {
        // Two rows per system database, `database_id` 1 to 4 in the order of
        // `SYSTEM_DATABASES`; what a bootstrapped storage then holds is read by the
        // integration test `master_has_database_files_rows`.
        let rows = described(FILES_TABLE).rows;
        assert_eq!(rows.len(), 2 * SYSTEM_DATABASES.len());
        let pairs: Vec<(Value, String)> = rows
            .iter()
            .map(|row| {
                (
                    row.0[files_columns::DATABASE_ID].clone(),
                    text_of(&row.0[files_columns::NAME]),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                (Value::I32(1), "master_data".to_owned()),
                (Value::I32(1), "master_log".to_owned()),
                (Value::I32(2), "tempdb_data".to_owned()),
                (Value::I32(2), "tempdb_log".to_owned()),
                (Value::I32(3), "model_data".to_owned()),
                (Value::I32(3), "model_log".to_owned()),
                (Value::I32(4), "msdb_data".to_owned()),
                (Value::I32(4), "msdb_log".to_owned()),
            ]
        );
    }

    #[test]
    fn the_file_rows_are_those_of_a_new_database() {
        // The numbers SQL Server publishes for a new database: a data file of 1024 pages
        // growing by 8192 without a maximum, a log file of 1024 pages growing by 8192 up to
        // 268435456, `data_space_id` 1 and 0.
        let rows = file_rows(7, "d");
        let data = &rows[0].0;
        assert_eq!(data[files_columns::DATABASE_ID], Value::I32(7));
        assert_eq!(data[files_columns::FILE_ID], Value::I32(1));
        assert_eq!(data[files_columns::TYPE], Value::I8(0));
        assert_eq!(text_of(&data[files_columns::TYPE_DESC]), "ROWS");
        // The two logical names and the size are ours, the rest is published: SQL Server
        // answers `d` for that data file and a `size` that follows its `model`.
        assert_eq!(text_of(&data[files_columns::NAME]), "d_data");
        assert_eq!(
            text_of(&data[files_columns::PHYSICAL_NAME]),
            "vauban/data/d_data.vdat"
        );
        assert_eq!(data[files_columns::DATA_SPACE_ID], Value::I32(1));
        assert_eq!(data[files_columns::SIZE], Value::I32(1024));
        assert_eq!(data[files_columns::MAX_SIZE], Value::I32(-1));
        assert_eq!(data[files_columns::GROWTH], Value::I32(8192));
        let log = &rows[1].0;
        assert_eq!(log[files_columns::FILE_ID], Value::I32(2));
        assert_eq!(log[files_columns::TYPE], Value::I8(1));
        assert_eq!(text_of(&log[files_columns::TYPE_DESC]), "LOG");
        assert_eq!(text_of(&log[files_columns::NAME]), "d_log");
        assert_eq!(
            text_of(&log[files_columns::PHYSICAL_NAME]),
            "vauban/data/d_log.vlog"
        );
        assert_eq!(log[files_columns::DATA_SPACE_ID], Value::I32(0));
        assert_eq!(log[files_columns::MAX_SIZE], Value::I32(268_435_456));
        // `is_percent_growth` is the literal 0 of the view, the growth being in pages.
        assert!(database_files_definition().contains("CAST(0 AS bit) AS is_percent_growth"));
        // No disk is touched: the path is a name.
        assert!(text_of(&data[files_columns::PHYSICAL_NAME]).starts_with(DATA_DIRECTORY));
    }

    #[test]
    fn master_files_reads_every_database_where_database_files_filters() {
        let text = master_files_definition();
        assert!(text.starts_with("SELECT database_id,"));
        // Instance-wide: no `WHERE`, where `sys.database_files` keeps the current database.
        assert!(!text.contains("WHERE"));
        assert!(database_files_definition().contains("WHERE database_id = DB_ID()"));
    }

    #[test]
    fn the_literal_columns_are_the_published_values() {
        // The literals of `sys.views`, compared with the published row of a user view: each
        // one carries the published value, `create_date` and `modify_date` apart (`NULL`
        // here, an instant there, as for `sys.objects`).
        let published: BTreeMap<&str, Option<&str>> = PUBLISHED_VIEWS_ROW.into_iter().collect();
        let mut compared = 0;
        for (name, expression) in VIEWS_VIEW {
            let Some(literal) = expression.strip_prefix("CAST(") else {
                continue;
            };
            if name == "create_date" || name == "modify_date" {
                continue;
            }
            let value = literal.split(" AS ").next().unwrap_or(literal);
            let expected = match published.get(name) {
                Some(Some(text)) => (*text).to_owned(),
                _ => "NULL".to_owned(),
            };
            // `N'TEXT'` gives `TEXT` and `NULL` stays `NULL`: the `N` of a national literal
            // is the one before a quote.
            let written = match value.strip_prefix("N'") {
                Some(quoted) => quoted.trim_end_matches('\'').to_owned(),
                None => value.trim_matches('\'').to_owned(),
            };
            assert_eq!(written, expected, "{name}");
            compared += 1;
        }
        // The 14 literals of the select list, `create_date` and `modify_date` apart.
        assert_eq!(compared, 14);
    }

    #[test]
    fn the_described_tables_are_five_and_named_apart() {
        let names: Vec<String> = internal_tables()
            .into_iter()
            .map(|table| table.name)
            .collect();
        assert_eq!(
            names,
            vec![
                ALL_OBJECTS_TABLE,
                ALL_COLUMNS_TABLE,
                PARTITIONS_TABLE,
                ALLOCATION_UNITS_TABLE,
                FILES_TABLE
            ]
        );
        // A name another file of `views/` describes is not in this list.
        let others: Vec<String> = [
            sys_tables::internal_tables(),
            sys_core::internal_tables(),
            sys_indexes::internal_tables(),
            info_schema::internal_tables(),
            sys_constraints::internal_tables(),
        ]
        .into_iter()
        .flatten()
        .map(|table| table.name)
        .collect();
        for name in &names {
            assert!(!others.contains(name), "{name} is described twice");
        }
    }

    #[test]
    fn the_objects_and_the_files_tables_are_the_two_the_bootstrap_fills() {
        // The three others hold the rows of a user object, written into `storage` at each
        // DDL (module documentation).
        for table in internal_tables() {
            let filled = table.name == ALL_OBJECTS_TABLE || table.name == FILES_TABLE;
            assert_eq!(!table.rows.is_empty(), filled, "{}", table.name);
            for row in &table.rows {
                assert_eq!(row.0.len(), table.columns.len(), "{}", table.name);
            }
        }
    }
}
