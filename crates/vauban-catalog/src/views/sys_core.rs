//! `sys.databases`, `sys.schemas` and `sys.types`.
//!
//! # The shape of the views
//!
//! The column list of each of the three views — names, order, types — is the one SQL Server
//! 2022 publishes. The unit tests below freeze the three vectors of names and compare them
//! with the text this file builds (`sys_databases_column_names_match_the_published_list`
//! and its two neighbours).
//!
//! # One internal table per set of rows
//!
//! A view is a `SELECT` over one denormalised internal table, without a join.
//! [`DATABASES_TABLE`] and [`SCHEMAS_TABLE`] belong to `bootstrap.rs` because their rows
//! carry the [`DbId`](vauban_storage::DbId)s `storage` handed out at start-up; this file
//! hands them their views through [`bootstrap_views`] and owns the third table,
//! [`TYPES_TABLE`], whose 34 rows are constants.
//!
//! # Read columns and literal columns
//!
//! `sys.databases` publishes 89 columns while [`DATABASES_TABLE`] holds five of them —
//! `name`, `database_id`, `collation_name` and the two versioning options
//! `is_read_committed_snapshot_on` and `snapshot_isolation_state`, plus
//! `physical_database_name`, which reads `name` again. One more is **derived** from a read
//! column, `snapshot_isolation_state_desc` being the `CASE` of the four states of
//! [`SnapshotIsolationState`](crate::meta::SnapshotIsolationState) over
//! `snapshot_isolation_state`. The 82 others are literals written in the text of the view
//! (unit test `the_databases_view_reads_six_items_one_is_derived_and_the_rest_are_literals`),
//! under two rules:
//!
//! - a column whose value would need a datum the catalogue does not store is `CAST(NULL AS
//!   …)`. The unit test `the_columns_written_null_are_the_ones_with_no_datum_behind_them`
//!   holds their names and counts them: 11 of the 85, among them `source_database_id`,
//!   `replica_id`, `group_database_id`, `resource_pool_id`, the four `default_…_language_…`
//!   columns, `two_digit_year_cutoff` and the two `bit`s `is_nested_triggers_on` and
//!   `is_transform_noise_words_on`, which SQL Server reads from the configuration of the
//!   instance rather than from the database;
//! - a flag or an option VaubanDB does not serve yet is written with the value of the state
//!   it is in: `0` for a `bit` the database itself carries — the two `bit`s above are `NULL`
//!   instead, being options of the instance — and for a coded pair the number and its text
//!   together (`recovery_model` 3 / `SIMPLE`, `page_verify_option` 0 / `NONE`). Serving one
//!   of those options means writing its value here, as is done for
//!   `is_read_committed_snapshot_on`, `snapshot_isolation_state` and its `_desc`: the three
//!   are read from [`DATABASES_TABLE`] or derived from it, at the published position and
//!   with the published type (unit test `sys_databases_columns_are_unchanged`).
//!
//! `sys.schemas` is a per-database view over a table that holds the schemas of the four
//! system databases at once, so its text filters on `database_id = DB_ID()`; `sys.types`
//! holds the system types, which do not differ from one database to the next, and filters
//! nothing.
//!
//! # Execution
//!
//! What this file produces is the text of the definitions and the rows behind them, checked
//! by the tests of `views/sys_core.rs` and `tests/sys_core.rs`; the binder expands the text
//! in place of the name.

use vauban_storage::Row;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{DATABASES_TABLE, SCHEMAS_TABLE, SYSTEM_DATABASES};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::QualifiedName;

/// Internal table of the system types, read by `sys.types`.
///
/// A `vauban_sys_*` name of our own; what a client reads is the view built over it.
pub(crate) const TYPES_TABLE: &str = "vauban_sys_types";

/// The schema the three views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The column names of the three views that are T-SQL reserved words and are therefore
/// written as delimited identifiers in the text of a view: `precision`, a column of
/// `sys.types` (unit test
/// `a_reserved_column_name_is_delimited`).
const RESERVED_COLUMN_NAMES: [&str; 1] = ["precision"];

/// The `schema_id` of `sys`, which owns the rows of `sys.types` (`bootstrap::SYSTEM_SCHEMAS`;
/// the 34 rows carry `schema_id` 4).
const SYS_SCHEMA_ID: i32 = 4;

/// The name of the collation of a character type, `collation_name` of the rows of
/// `sys.types` that carry one.
const DEFAULT_COLLATION_NAME: &str = "SQL_Latin1_General_CP1_CI_AS";

/// The select list of `sys.databases`: `(column, expression)`, in the published order.
///
/// An expression equal to the column name reads the internal table; the others are the
/// literals the module documentation explains.
const DATABASES_VIEW: [(&str, &str); 89] = [
    ("name", "name"),
    ("database_id", "database_id"),
    ("source_database_id", "CAST(NULL AS int)"),
    ("owner_sid", "owner_sid"),
    ("create_date", "create_date"),
    ("compatibility_level", "CAST(160 AS tinyint)"),
    ("collation_name", "collation_name"),
    ("user_access", "CAST(0 AS tinyint)"),
    ("user_access_desc", "CAST(N'MULTI_USER' AS nvarchar(60))"),
    ("is_read_only", "CAST(0 AS bit)"),
    ("is_auto_close_on", "CAST(0 AS bit)"),
    ("is_auto_shrink_on", "CAST(0 AS bit)"),
    ("state", "CAST(0 AS tinyint)"),
    ("state_desc", "CAST(N'ONLINE' AS nvarchar(60))"),
    ("is_in_standby", "CAST(0 AS bit)"),
    ("is_cleanly_shutdown", "CAST(0 AS bit)"),
    ("is_supplemental_logging_enabled", "CAST(0 AS bit)"),
    ("snapshot_isolation_state", "snapshot_isolation_state"),
    (
        "snapshot_isolation_state_desc",
        "CAST(CASE snapshot_isolation_state WHEN 0 THEN N'OFF' WHEN 1 THEN N'ON' WHEN 2 THEN N'IN_TRANSITION_TO_ON' WHEN 3 THEN N'IN_TRANSITION_TO_OFF' END AS nvarchar(60))",
    ),
    (
        "is_read_committed_snapshot_on",
        "is_read_committed_snapshot_on",
    ),
    ("recovery_model", "CAST(3 AS tinyint)"),
    ("recovery_model_desc", "CAST(N'SIMPLE' AS nvarchar(60))"),
    ("page_verify_option", "CAST(0 AS tinyint)"),
    ("page_verify_option_desc", "CAST(N'NONE' AS nvarchar(60))"),
    ("is_auto_create_stats_on", "CAST(0 AS bit)"),
    ("is_auto_create_stats_incremental_on", "CAST(0 AS bit)"),
    ("is_auto_update_stats_on", "CAST(0 AS bit)"),
    ("is_auto_update_stats_async_on", "CAST(0 AS bit)"),
    ("is_ansi_null_default_on", "CAST(0 AS bit)"),
    ("is_ansi_nulls_on", "CAST(0 AS bit)"),
    ("is_ansi_padding_on", "CAST(0 AS bit)"),
    ("is_ansi_warnings_on", "CAST(0 AS bit)"),
    ("is_arithabort_on", "CAST(0 AS bit)"),
    ("is_concat_null_yields_null_on", "CAST(0 AS bit)"),
    ("is_numeric_roundabort_on", "CAST(0 AS bit)"),
    ("is_quoted_identifier_on", "CAST(0 AS bit)"),
    ("is_recursive_triggers_on", "CAST(0 AS bit)"),
    ("is_cursor_close_on_commit_on", "CAST(0 AS bit)"),
    ("is_local_cursor_default", "CAST(0 AS bit)"),
    ("is_fulltext_enabled", "CAST(0 AS bit)"),
    ("is_trustworthy_on", "CAST(0 AS bit)"),
    ("is_db_chaining_on", "CAST(0 AS bit)"),
    ("is_parameterization_forced", "CAST(0 AS bit)"),
    ("is_master_key_encrypted_by_server", "CAST(0 AS bit)"),
    ("is_query_store_on", "CAST(0 AS bit)"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_subscribed", "CAST(0 AS bit)"),
    ("is_merge_published", "CAST(0 AS bit)"),
    ("is_distributor", "CAST(0 AS bit)"),
    ("is_sync_with_backup", "CAST(0 AS bit)"),
    (
        "service_broker_guid",
        "CAST('00000000-0000-0000-0000-000000000000' AS uniqueidentifier)",
    ),
    ("is_broker_enabled", "CAST(0 AS bit)"),
    ("log_reuse_wait", "CAST(0 AS tinyint)"),
    ("log_reuse_wait_desc", "CAST(N'NOTHING' AS nvarchar(60))"),
    ("is_date_correlation_on", "CAST(0 AS bit)"),
    ("is_cdc_enabled", "CAST(0 AS bit)"),
    ("is_encrypted", "CAST(0 AS bit)"),
    ("is_honor_broker_priority_on", "CAST(0 AS bit)"),
    ("replica_id", "CAST(NULL AS uniqueidentifier)"),
    ("group_database_id", "CAST(NULL AS uniqueidentifier)"),
    ("resource_pool_id", "CAST(NULL AS int)"),
    ("default_language_lcid", "CAST(NULL AS smallint)"),
    ("default_language_name", "CAST(NULL AS nvarchar(128))"),
    ("default_fulltext_language_lcid", "CAST(NULL AS int)"),
    (
        "default_fulltext_language_name",
        "CAST(NULL AS nvarchar(128))",
    ),
    ("is_nested_triggers_on", "CAST(NULL AS bit)"),
    ("is_transform_noise_words_on", "CAST(NULL AS bit)"),
    ("two_digit_year_cutoff", "CAST(NULL AS smallint)"),
    ("containment", "CAST(0 AS tinyint)"),
    ("containment_desc", "CAST(N'NONE' AS nvarchar(60))"),
    ("target_recovery_time_in_seconds", "CAST(0 AS int)"),
    ("delayed_durability", "CAST(0 AS int)"),
    (
        "delayed_durability_desc",
        "CAST(N'DISABLED' AS nvarchar(60))",
    ),
    (
        "is_memory_optimized_elevate_to_snapshot_on",
        "CAST(0 AS bit)",
    ),
    ("is_federation_member", "CAST(0 AS bit)"),
    ("is_remote_data_archive_enabled", "CAST(0 AS bit)"),
    ("is_mixed_page_allocation_on", "CAST(0 AS bit)"),
    ("is_temporal_history_retention_enabled", "CAST(0 AS bit)"),
    ("catalog_collation_type", "CAST(0 AS int)"),
    (
        "catalog_collation_type_desc",
        "CAST(N'DATABASE_DEFAULT' AS nvarchar(60))",
    ),
    ("physical_database_name", "name"),
    ("is_result_set_caching_on", "CAST(0 AS bit)"),
    ("is_accelerated_database_recovery_on", "CAST(0 AS bit)"),
    ("is_tempdb_spill_to_remote_store", "CAST(0 AS bit)"),
    ("is_stale_page_detection_on", "CAST(0 AS bit)"),
    ("is_memory_optimized_enabled", "CAST(0 AS bit)"),
    ("is_data_retention_enabled", "CAST(0 AS bit)"),
    ("is_ledger_on", "CAST(0 AS bit)"),
    ("is_change_feed_enabled", "CAST(0 AS bit)"),
];

/// The select list of `sys.schemas`. `database_id` of the internal table is the filter of
/// the view, not one of its columns.
const SCHEMAS_VIEW: [(&str, &str); 3] = [
    ("name", "name"),
    ("schema_id", "schema_id"),
    ("principal_id", "principal_id"),
];

/// The select list of `sys.types`: its 15 columns, each one a column of [`TYPES_TABLE`].
const TYPES_VIEW: [(&str, &str); 15] = [
    ("name", "name"),
    ("system_type_id", "system_type_id"),
    ("user_type_id", "user_type_id"),
    ("schema_id", "schema_id"),
    ("principal_id", "principal_id"),
    ("max_length", "max_length"),
    ("precision", "precision"),
    ("scale", "scale"),
    ("collation_name", "collation_name"),
    ("is_nullable", "is_nullable"),
    ("is_user_defined", "is_user_defined"),
    ("is_assembly_type", "is_assembly_type"),
    ("default_object_id", "default_object_id"),
    ("rule_object_id", "rule_object_id"),
    ("is_table_type", "is_table_type"),
];

/// A row of [`TYPES_TABLE`]: `(name, system_type_id, user_type_id, max_length, precision,
/// scale, has_collation, is_nullable, is_assembly_type)`.
///
/// The nine fields that vary from one type to the next; the six that do not are written by
/// [`types_rows`]. The values are those of `sys.types` — the identifiers of the system
/// types are the ones clients and drivers read in `sys.types.user_type_id`.
type SystemType = (&'static str, u8, i32, i16, u8, u8, bool, bool, bool);

/// The 34 system types, in the order of `user_type_id`.
///
/// The list includes the types `vauban-types` does not serve (`sql_variant`, `xml`,
/// `text`, `ntext`, `image`, `timestamp`, `hierarchyid`, `geometry`, `geography`): the view
/// publishes the catalogue of the type system a client expects to join on.
const SYSTEM_TYPES: [SystemType; 34] = [
    ("image", 34, 34, 16, 0, 0, false, true, false),
    ("text", 35, 35, 16, 0, 0, true, true, false),
    ("uniqueidentifier", 36, 36, 16, 0, 0, false, true, false),
    ("date", 40, 40, 3, 10, 0, false, true, false),
    ("time", 41, 41, 5, 16, 7, false, true, false),
    ("datetime2", 42, 42, 8, 27, 7, false, true, false),
    ("datetimeoffset", 43, 43, 10, 34, 7, false, true, false),
    ("tinyint", 48, 48, 1, 3, 0, false, true, false),
    ("smallint", 52, 52, 2, 5, 0, false, true, false),
    ("int", 56, 56, 4, 10, 0, false, true, false),
    ("smalldatetime", 58, 58, 4, 16, 0, false, true, false),
    ("real", 59, 59, 4, 24, 0, false, true, false),
    ("money", 60, 60, 8, 19, 4, false, true, false),
    ("datetime", 61, 61, 8, 23, 3, false, true, false),
    ("float", 62, 62, 8, 53, 0, false, true, false),
    ("sql_variant", 98, 98, 8016, 0, 0, false, true, false),
    ("ntext", 99, 99, 16, 0, 0, true, true, false),
    ("bit", 104, 104, 1, 1, 0, false, true, false),
    ("decimal", 106, 106, 17, 38, 38, false, true, false),
    ("numeric", 108, 108, 17, 38, 38, false, true, false),
    ("smallmoney", 122, 122, 4, 10, 4, false, true, false),
    ("bigint", 127, 127, 8, 19, 0, false, true, false),
    ("hierarchyid", 240, 128, 892, 0, 0, false, true, true),
    ("geometry", 240, 129, -1, 0, 0, false, true, true),
    ("geography", 240, 130, -1, 0, 0, false, true, true),
    ("varbinary", 165, 165, 8000, 0, 0, false, true, false),
    ("varchar", 167, 167, 8000, 0, 0, true, true, false),
    ("binary", 173, 173, 8000, 0, 0, false, true, false),
    ("char", 175, 175, 8000, 0, 0, true, true, false),
    ("timestamp", 189, 189, 8, 0, 0, false, false, false),
    ("nvarchar", 231, 231, 8000, 0, 0, true, true, false),
    ("nchar", 239, 239, 8000, 0, 0, true, true, false),
    ("xml", 241, 241, -1, 0, 0, false, true, false),
    ("sysname", 231, 256, 256, 0, 0, true, false, false),
];

/// The internal tables this file describes: the one of the system types.
///
/// The bootstrap creates them in `master` and inserts their rows; the two tables of
/// `bootstrap.rs` get their views from [`bootstrap_views`] instead.
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![InternalTableDef {
        name: TYPES_TABLE.to_owned(),
        columns: types_columns(),
        clustered_key: None,
        rows: types_rows(),
        views: views_of("types", &definition(&TYPES_VIEW, TYPES_TABLE, None)),
    }]
}

/// The views built on the internal table `table` of `bootstrap.rs`: `sys.databases` for
/// [`DATABASES_TABLE`], `sys.schemas` for [`SCHEMAS_TABLE`], an empty vector for a name this
/// file does not carry a view for (unit test `bootstrap_views_answers_nothing_for_another_table`).
///
/// The name is compared without regard to case, as `SQL_Latin1_General_CP1_CI_AS` compares
/// identifiers.
pub(crate) fn bootstrap_views(table: &str) -> Vec<SystemViewDef> {
    if table.eq_ignore_ascii_case(DATABASES_TABLE) {
        views_of(
            "databases",
            &definition(&DATABASES_VIEW, DATABASES_TABLE, None),
        )
    } else if table.eq_ignore_ascii_case(SCHEMAS_TABLE) {
        views_of(
            "schemas",
            &definition(&SCHEMAS_VIEW, SCHEMAS_TABLE, Some("database_id = DB_ID()")),
        )
    } else {
        Vec::new()
    }
}

/// The same view installed in the four system databases, `sys.<name>` in each of them.
///
/// `sys.databases` reads the same rows from wherever it is called, and `sys.schemas` filters
/// on `DB_ID()`, so the four definitions share their text (unit test
/// `the_views_are_installed_in_the_four_system_databases`). A database created by
/// `CREATE DATABASE` receives no copies yet.
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
/// its filter when it has one.
///
/// One select item per line, which is how `view_definition` hands the text to a client that
/// asks for it. An item whose expression is its own name is written bare, the others as
/// `<expression> AS <name>`, so the name of a column is the last identifier of its item
/// (unit test `sys_databases_column_names_match_the_published_list` reads the text that way).
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

/// The columns of [`TYPES_TABLE`]: the 15 of `sys.types`, with the nullability of each.
fn types_columns() -> Vec<InternalColumnDef> {
    vec![
        column("name", SYSNAME, false),
        column("system_type_id", SqlType::TinyInt, false),
        column("user_type_id", SqlType::Int, false),
        column("schema_id", SqlType::Int, false),
        column("principal_id", SqlType::Int, true),
        column("max_length", SqlType::SmallInt, false),
        column("precision", SqlType::TinyInt, false),
        column("scale", SqlType::TinyInt, false),
        column("collation_name", SYSNAME, true),
        column("is_nullable", SqlType::Bit, true),
        column("is_user_defined", SqlType::Bit, false),
        column("is_assembly_type", SqlType::Bit, false),
        column("default_object_id", SqlType::Int, false),
        column("rule_object_id", SqlType::Int, false),
        column("is_table_type", SqlType::Bit, false),
    ]
}

/// The rows of [`TYPES_TABLE`], one per entry of [`SYSTEM_TYPES`].
///
/// The six columns that carry the same value in the 34 rows are written here:
/// `schema_id` 4, `principal_id` NULL, `is_user_defined` 0, `default_object_id` 0,
/// `rule_object_id` 0, `is_table_type` 0 (unit test
/// `the_constant_columns_of_the_type_rows_are_the_same_in_each_row`).
fn types_rows() -> Vec<Row> {
    SYSTEM_TYPES
        .iter()
        .map(
            |&(
                name,
                system_type_id,
                user_type_id,
                max_length,
                precision,
                scale,
                collation,
                nullable,
                assembly,
            )| {
                Row(vec![
                    text(name),
                    Value::I8(system_type_id),
                    Value::I32(user_type_id),
                    Value::I32(SYS_SCHEMA_ID),
                    Value::Null,
                    Value::I16(max_length),
                    Value::I8(precision),
                    Value::I8(scale),
                    if collation {
                        text(DEFAULT_COLLATION_NAME)
                    } else {
                        Value::Null
                    },
                    Value::Bit(nullable),
                    Value::Bit(false),
                    Value::Bit(assembly),
                    Value::I32(0),
                    Value::I32(0),
                    Value::Bit(false),
                ])
            },
        )
        .collect()
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
    use super::*;
    use vauban_parser::{ParseOptions, Statement, parse_batch};

    use crate::meta::SnapshotIsolationState;

    /// The ordered column names of `sys.databases` in SQL Server 2022. Frozen here so that a
    /// change of the text of the view has to face the list again.
    const PUBLISHED_DATABASES_COLUMNS: [&str; 89] = [
        "name",
        "database_id",
        "source_database_id",
        "owner_sid",
        "create_date",
        "compatibility_level",
        "collation_name",
        "user_access",
        "user_access_desc",
        "is_read_only",
        "is_auto_close_on",
        "is_auto_shrink_on",
        "state",
        "state_desc",
        "is_in_standby",
        "is_cleanly_shutdown",
        "is_supplemental_logging_enabled",
        "snapshot_isolation_state",
        "snapshot_isolation_state_desc",
        "is_read_committed_snapshot_on",
        "recovery_model",
        "recovery_model_desc",
        "page_verify_option",
        "page_verify_option_desc",
        "is_auto_create_stats_on",
        "is_auto_create_stats_incremental_on",
        "is_auto_update_stats_on",
        "is_auto_update_stats_async_on",
        "is_ansi_null_default_on",
        "is_ansi_nulls_on",
        "is_ansi_padding_on",
        "is_ansi_warnings_on",
        "is_arithabort_on",
        "is_concat_null_yields_null_on",
        "is_numeric_roundabort_on",
        "is_quoted_identifier_on",
        "is_recursive_triggers_on",
        "is_cursor_close_on_commit_on",
        "is_local_cursor_default",
        "is_fulltext_enabled",
        "is_trustworthy_on",
        "is_db_chaining_on",
        "is_parameterization_forced",
        "is_master_key_encrypted_by_server",
        "is_query_store_on",
        "is_published",
        "is_subscribed",
        "is_merge_published",
        "is_distributor",
        "is_sync_with_backup",
        "service_broker_guid",
        "is_broker_enabled",
        "log_reuse_wait",
        "log_reuse_wait_desc",
        "is_date_correlation_on",
        "is_cdc_enabled",
        "is_encrypted",
        "is_honor_broker_priority_on",
        "replica_id",
        "group_database_id",
        "resource_pool_id",
        "default_language_lcid",
        "default_language_name",
        "default_fulltext_language_lcid",
        "default_fulltext_language_name",
        "is_nested_triggers_on",
        "is_transform_noise_words_on",
        "two_digit_year_cutoff",
        "containment",
        "containment_desc",
        "target_recovery_time_in_seconds",
        "delayed_durability",
        "delayed_durability_desc",
        "is_memory_optimized_elevate_to_snapshot_on",
        "is_federation_member",
        "is_remote_data_archive_enabled",
        "is_mixed_page_allocation_on",
        "is_temporal_history_retention_enabled",
        "catalog_collation_type",
        "catalog_collation_type_desc",
        "physical_database_name",
        "is_result_set_caching_on",
        "is_accelerated_database_recovery_on",
        "is_tempdb_spill_to_remote_store",
        "is_stale_page_detection_on",
        "is_memory_optimized_enabled",
        "is_data_retention_enabled",
        "is_ledger_on",
        "is_change_feed_enabled",
    ];

    /// The ordered column names of `sys.schemas`.
    const PUBLISHED_SCHEMAS_COLUMNS: [&str; 3] = ["name", "schema_id", "principal_id"];

    /// The ordered column names of `sys.types`.
    const PUBLISHED_TYPES_COLUMNS: [&str; 15] = [
        "name",
        "system_type_id",
        "user_type_id",
        "schema_id",
        "principal_id",
        "max_length",
        "precision",
        "scale",
        "collation_name",
        "is_nullable",
        "is_user_defined",
        "is_assembly_type",
        "default_object_id",
        "rule_object_id",
        "is_table_type",
    ];

    /// The names a client reads in the result of a definition, taken from its text: the
    /// select list split on the commas outside parentheses, then the last identifier of each
    /// item, which is its alias when it has one.
    fn column_names(definition: &str) -> Vec<String> {
        let list = definition
            .strip_prefix("SELECT ")
            .expect("a definition starts with SELECT");
        let list = list
            .split("\n  FROM ")
            .next()
            .expect("split always yields a first piece");
        let mut names = Vec::new();
        let mut item = String::new();
        let mut depth = 0usize;
        for character in list.chars() {
            match character {
                '(' => {
                    depth += 1;
                    item.push(character);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    item.push(character);
                }
                ',' if depth == 0 => {
                    names.push(last_identifier(&item));
                    item.clear();
                }
                _ => item.push(character),
            }
        }
        names.push(last_identifier(&item));
        names
    }

    /// The last identifier of a select item: what follows its last separator, the closing
    /// bracket of a delimited identifier set aside.
    fn last_identifier(item: &str) -> String {
        item.trim()
            .trim_end_matches(']')
            .rsplit(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .next()
            .expect("rsplit always yields a first piece")
            .to_owned()
    }

    /// The definition of the view called `name`, taken where the bootstrap will read it.
    fn definition_of(name: &str) -> String {
        let views = match name {
            "types" => internal_tables().swap_remove(0).views,
            "databases" => bootstrap_views(DATABASES_TABLE),
            _ => bootstrap_views(SCHEMAS_TABLE),
        };
        views[0].definition.clone()
    }

    /// The row of [`TYPES_TABLE`] whose `name` column is `name`, compared without regard to
    /// case as `SQL_Latin1_General_CP1_CI_AS` compares a `sysname`.
    fn type_row(name: &str) -> Vec<Value> {
        types_rows()
            .into_iter()
            .map(|row| row.0)
            .find(|row| matches!(&row[0], Value::String(text) if text.text.eq_ignore_ascii_case(name)))
            .unwrap_or_else(|| panic!("the type `{name}` has a row"))
    }

    #[test]
    fn sys_databases_column_names_match_the_published_list() {
        assert_eq!(
            column_names(&definition_of("databases")),
            PUBLISHED_DATABASES_COLUMNS
        );
    }

    #[test]
    fn the_databases_view_reads_six_items_one_is_derived_and_the_rest_are_literals() {
        // The count the module documentation states: the five columns of the internal table
        // and `physical_database_name`, which reads `name` again.
        let read: Vec<&str> = DATABASES_VIEW
            .iter()
            .filter(|(_, expression)| !expression.starts_with("CAST("))
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            read,
            [
                "name",
                "database_id",
                "owner_sid",
                "create_date",
                "collation_name",
                "snapshot_isolation_state",
                "is_read_committed_snapshot_on",
                "physical_database_name"
            ]
        );
        // The derived item: the text of the state is a `CASE` over the number, so a switch
        // written by `Catalog::set_database_option` moves both columns at once.
        let derived: Vec<&str> = DATABASES_VIEW
            .iter()
            .filter(|(_, expression)| expression.contains("CASE "))
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(derived, ["snapshot_isolation_state_desc"]);
        assert_eq!(DATABASES_VIEW.len() - read.len() - derived.len(), 80);
        // The `CASE` lists the four couples of the enum, so a state added there shows up in
        // the text of the view rather than falling to `NULL`.
        let text = DATABASES_VIEW
            .iter()
            .find(|(name, _)| *name == "snapshot_isolation_state_desc")
            .map(|(_, expression)| *expression)
            .expect("the derived item is in the select list");
        for state in [
            SnapshotIsolationState::Off,
            SnapshotIsolationState::On,
            SnapshotIsolationState::InTransitionToOn,
            SnapshotIsolationState::InTransitionToOff,
        ] {
            let couple = format!("WHEN {} THEN N'{}'", state.state(), state.desc());
            assert!(text.contains(&couple), "{couple} is not in {text}");
        }
    }

    #[test]
    fn sys_databases_columns_are_unchanged() {
        // The published list, unchanged by the three expressions that read the versioning
        // options: same names, same order, same count.
        let names = column_names(&definition_of("databases"));
        assert_eq!(names, PUBLISHED_DATABASES_COLUMNS);
        assert_eq!(names.len(), 89);
        let position = |column: &str| {
            names
                .iter()
                .position(|name| name == column)
                .unwrap_or_else(|| panic!("`{column}` is a column of the view"))
        };
        // The three columns of the versioning options keep their published positions: 18,
        // 19 and 20 of the 89, counted from 1.
        assert_eq!(position("snapshot_isolation_state") + 1, 18);
        assert_eq!(position("snapshot_isolation_state_desc") + 1, 19);
        assert_eq!(position("is_read_committed_snapshot_on") + 1, 20);
        // Their source: two read the internal table and the third is derived from one of
        // them.
        let expression = |column: &str| {
            DATABASES_VIEW
                .iter()
                .find(|(name, _)| *name == column)
                .map(|(_, expression)| *expression)
                .unwrap_or_else(|| panic!("`{column}` is a column of the view"))
        };
        assert_eq!(
            expression("snapshot_isolation_state"),
            "snapshot_isolation_state"
        );
        assert_eq!(
            expression("is_read_committed_snapshot_on"),
            "is_read_committed_snapshot_on"
        );
        assert!(
            expression("snapshot_isolation_state_desc").contains("CASE snapshot_isolation_state"),
            "{}",
            expression("snapshot_isolation_state_desc")
        );
        // The published types are those of the internal columns and of the `CAST` of the
        // derived item: `bit`, `tinyint`, `nvarchar`.
        assert!(
            expression("snapshot_isolation_state_desc").ends_with("AS nvarchar(60))"),
            "{}",
            expression("snapshot_isolation_state_desc")
        );
    }

    #[test]
    fn the_databases_definition_parses_with_its_case() {
        // The text of a view is T-SQL the engine reads, and the derived item is the first
        // `CASE` of a definition of this file: the parser of the workspace is what judges
        // its shape, where the tests above compare its text.
        let definition = definition_of("databases");
        let batch = parse_batch(&definition, &ParseOptions::default())
            .unwrap_or_else(|err| panic!("sys.databases: {}\n{definition}", err.message));
        assert_eq!(batch.statements.len(), 1);
        assert!(matches!(batch.statements[0], Statement::Select(_)));
        // Counter-check held in the test itself: the same text with the `END` of the `CASE`
        // taken out is refused by the same parser, so the assertion above is not blind to a
        // broken `CASE`.
        let broken = definition.replace(" END AS nvarchar(60))", " AS nvarchar(60))");
        assert_ne!(broken, definition);
        assert!(
            parse_batch(&broken, &ParseOptions::default()).is_err(),
            "a `CASE` without its `END` is read by the parser: {broken}"
        );
    }

    #[test]
    fn the_columns_written_null_are_the_ones_with_no_datum_behind_them() {
        // Counted on the select list itself, the module documentation naming the same 13.
        let written_null: Vec<&str> = DATABASES_VIEW
            .iter()
            .filter(|(_, expression)| expression.starts_with("CAST(NULL AS "))
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            written_null,
            [
                "source_database_id",
                "replica_id",
                "group_database_id",
                "resource_pool_id",
                "default_language_lcid",
                "default_language_name",
                "default_fulltext_language_lcid",
                "default_fulltext_language_name",
                "is_nested_triggers_on",
                "is_transform_noise_words_on",
                "two_digit_year_cutoff",
            ]
        );
        assert_eq!(written_null.len(), 11);
        // The two `bit`s written `NULL`: the rule of the documentation writes `0` for the
        // `bit`s a database carries, these two being options of the instance.
        let null_bits: Vec<&str> = DATABASES_VIEW
            .iter()
            .filter(|(_, expression)| *expression == "CAST(NULL AS bit)")
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            null_bits,
            ["is_nested_triggers_on", "is_transform_noise_words_on"]
        );
        // The select lists of the two other views, read column by column below: 0 `NULL`.
        for view in [SCHEMAS_VIEW.as_slice(), TYPES_VIEW.as_slice()] {
            assert!(
                !view
                    .iter()
                    .any(|(_, expression)| expression.contains("NULL")),
                "a NULL slipped into a view that reads its table column by column"
            );
        }
    }

    #[test]
    fn sys_schemas_column_names_match_the_published_list() {
        assert_eq!(
            column_names(&definition_of("schemas")),
            PUBLISHED_SCHEMAS_COLUMNS
        );
    }

    #[test]
    fn sys_types_column_names_match_the_published_list() {
        assert_eq!(
            column_names(&definition_of("types")),
            PUBLISHED_TYPES_COLUMNS
        );
        // The view reads its internal table column by column, so the two lists are the same.
        let stored: Vec<String> = types_columns()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(stored, PUBLISHED_TYPES_COLUMNS);
    }

    #[test]
    fn view_definition_is_a_select() {
        for name in ["databases", "schemas", "types"] {
            let definition = definition_of(name);
            assert!(
                definition.to_uppercase().starts_with("SELECT"),
                "`sys.{name}` does not start with SELECT: {definition}"
            );
            assert!(
                !definition.to_uppercase().contains("JOIN"),
                "`sys.{name}` carries a join: {definition}"
            );
        }
    }

    #[test]
    fn sys_types_contains_int_and_nvarchar() {
        // `system_type_id` and `user_type_id` of the two types.
        assert_eq!(type_row("INT")[1], Value::I8(56));
        assert_eq!(type_row("int")[2], Value::I32(56));
        assert_eq!(type_row("NVarChar")[1], Value::I8(231));
        assert_eq!(type_row("nvarchar")[2], Value::I32(231));
    }

    #[test]
    fn the_type_ids_are_those_of_sys_types() {
        let rows = types_rows();
        assert_eq!(rows.len(), 34);
        // `user_type_id` equals `system_type_id` for 30 of the 34 rows; the four that differ
        // are the three CLR types, which share `system_type_id` 240, and the
        // alias `sysname`, an `nvarchar` of `user_type_id` 256.
        let apart: Vec<String> = rows
            .iter()
            .filter(|row| match (&row.0[1], &row.0[2]) {
                (Value::I8(system), Value::I32(user)) => i32::from(*system) != *user,
                _ => panic!("the two identifiers are a tinyint and an int"),
            })
            .map(|row| match &row.0[0] {
                Value::String(text) => text.text.clone(),
                _ => panic!("the name is an nvarchar"),
            })
            .collect();
        assert_eq!(apart, ["hierarchyid", "geometry", "geography", "sysname"]);
    }

    #[test]
    fn the_constant_columns_of_the_type_rows_are_the_same_in_each_row() {
        let columns = types_columns();
        for row in types_rows() {
            assert_eq!(row.0.len(), columns.len());
            assert_eq!(row.0[3], Value::I32(SYS_SCHEMA_ID), "schema_id");
            assert_eq!(row.0[4], Value::Null, "principal_id");
            assert_eq!(row.0[10], Value::Bit(false), "is_user_defined");
            assert_eq!(row.0[12], Value::I32(0), "default_object_id");
            assert_eq!(row.0[13], Value::I32(0), "rule_object_id");
            assert_eq!(row.0[14], Value::Bit(false), "is_table_type");
        }
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        let types = internal_tables().swap_remove(0);
        assert_eq!(types.name, TYPES_TABLE);
        for (views, view) in [
            (types.views, "types"),
            (bootstrap_views(DATABASES_TABLE), "databases"),
            (bootstrap_views(SCHEMAS_TABLE), "schemas"),
        ] {
            let names: Vec<String> = views
                .iter()
                .map(|installed| {
                    format!(
                        "{}.{}.{}",
                        installed.name.database, installed.name.schema, installed.name.name
                    )
                })
                .collect();
            assert_eq!(
                names,
                SYSTEM_DATABASES
                    .iter()
                    .map(|database| format!("{database}.sys.{view}"))
                    .collect::<Vec<String>>()
            );
            // The four copies share one text.
            assert_eq!(
                views
                    .iter()
                    .filter(|installed| installed.definition == views[0].definition)
                    .count(),
                4
            );
        }
    }

    #[test]
    fn sys_schemas_filters_on_the_current_database() {
        // The internal table holds the schemas of the four system databases, and
        // `sys.schemas` shows those of the database it is read from.
        assert!(
            definition_of("schemas").contains("WHERE database_id = DB_ID()"),
            "{}",
            definition_of("schemas")
        );
        assert!(!definition_of("databases").contains("WHERE"));
    }

    #[test]
    fn a_reserved_column_name_is_delimited() {
        // `precision` is a reserved word of T-SQL and a column of `sys.types`; written bare
        // it ends the select list at a syntax error of the parser of VaubanDB.
        let definition = definition_of("types");
        assert!(definition.contains("[precision]"), "{definition}");
        assert!(!definition.contains("       precision,"), "{definition}");
        // The name a client reads is the published one, brackets left out.
        assert!(column_names(&definition).contains(&"precision".to_owned()));
    }

    #[test]
    fn bootstrap_views_answers_nothing_for_another_table() {
        assert!(bootstrap_views("vauban_sys_columns").is_empty());
        // The name of a table of the bootstrap is matched without regard to case.
        assert_eq!(bootstrap_views("VAUBAN_SYS_SCHEMAS").len(), 4);
    }
}
