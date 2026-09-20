//! `sys.triggers`, `sys.synonyms`, `sys.sequences` and `sys.table_types`.
//!
//! # The shape of the views
//!
//! The column list of each of the four views — names, order, types — is the one SQL Server
//! 2022 publishes: `sys.triggers` 13 columns, `sys.synonyms` 13, `sys.sequences` 26,
//! `sys.table_types` 17. The unit test `the_columns_of_the_four_views_are_the_published_ones`
//! freezes the four vectors of names and compares them with the text this file builds.
//!
//! # Four internal tables, one filter column and the published ones
//!
//! A view is a `SELECT` over one denormalised internal table, without a join (unit test
//! `no_definition_holds_a_join`). No `CREATE TRIGGER`, `CREATE SYNONYM`, `CREATE SEQUENCE` or
//! `CREATE TYPE … AS TABLE` is served, so the four tables carry no row: `database_id`, the
//! filter of the view, and the published columns, declared with the nullability the unit test
//! above freezes and read bare by the view rather than written as a literal, so a `NOT NULL`
//! `bit` or `datetime` column keeps its declared nullability once read through the view
//! (`views/sys_routines.rs`, section "Why a real column and not a literal"; same reasoning
//! here, not restated). No writer inserts a row here.
//!
//! | Internal table | View |
//! |---|---|
//! | [`TRIGGERS_TABLE`] | `sys.triggers` |
//! | [`SYNONYMS_TABLE`] | `sys.synonyms` |
//! | [`SEQUENCES_TABLE`] | `sys.sequences` |
//! | [`TABLE_TYPES_TABLE`] | `sys.table_types` |
//!
//! # What these views do not publish
//!
//! `sys.sequences.start_value`, `increment`, `minimum_value`, `maximum_value`, `current_value`
//! and `last_used_value` are `sql_variant` on SQL Server; the module `types` does not serve
//! that type yet. This file declares them `bigint` instead, `last_used_value` nullable and the
//! other five not, the nullability SQL Server publishes on the `sql_variant` columns they
//! stand in for. No row ever carries them either way.
//!
//! # Execution
//!
//! What this file produces is the shape of the four internal tables and the text of the four
//! definitions; the tables stay empty, so `sys_rows.rs` has no row to write here.

use vauban_types::{Len, SqlType, TypeInfo};

use crate::bootstrap::SYSTEM_DATABASES;
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::QualifiedName;

/// Internal table behind `sys.triggers`: one `database_id` column and the 13 published ones
/// (module documentation).
pub(crate) const TRIGGERS_TABLE: &str = "vauban_sys_triggers";

/// Internal table behind `sys.synonyms`: one `database_id` column and the 13 published ones
/// (module documentation).
pub(crate) const SYNONYMS_TABLE: &str = "vauban_sys_synonyms";

/// Internal table behind `sys.sequences`: one `database_id` column and the 26 published ones
/// (module documentation).
pub(crate) const SEQUENCES_TABLE: &str = "vauban_sys_sequences";

/// Internal table behind `sys.table_types`: one `database_id` column and the 17 published ones
/// (module documentation).
pub(crate) const TABLE_TYPES_TABLE: &str = "vauban_sys_table_types";

/// The schema the four views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The column name of `sys.sequences` and `sys.table_types` that is a T-SQL reserved word and
/// is therefore written as a delimited identifier in the text of a view: `precision`, as
/// `views/sys_routines.rs` delimits it in `sys.parameters`.
const RESERVED_COLUMN_NAMES: [&str; 1] = ["precision"];

/// The published columns of `sys.triggers`, `(name, type, nullable)` in the published order.
const TRIGGERS_COLUMNS: [(&str, SqlType, bool); 13] = [
    ("name", SYSNAME, false),
    ("object_id", SqlType::Int, false),
    ("parent_class", SqlType::TinyInt, false),
    ("parent_class_desc", SqlType::NVarChar(Len::Fixed(60)), true),
    ("parent_id", SqlType::Int, false),
    ("type", SqlType::Char(Len::Fixed(2)), false),
    ("type_desc", SqlType::NVarChar(Len::Fixed(60)), true),
    ("create_date", SqlType::DateTime, false),
    ("modify_date", SqlType::DateTime, false),
    ("is_ms_shipped", SqlType::Bit, false),
    ("is_disabled", SqlType::Bit, false),
    ("is_not_for_replication", SqlType::Bit, false),
    ("is_instead_of_trigger", SqlType::Bit, false),
];

/// The published columns of `sys.synonyms`, in the published order: the 12 of `sys.objects`
/// and `base_object_name`.
///
/// `type` and `type_desc` are published nullable here, where `sys.objects.type` is not
/// (`views/sys_routines.rs`, same divergence on `sys.procedures`, itself on SQL Server, not
/// one this file introduces).
const SYNONYMS_COLUMNS: [(&str, SqlType, bool); 13] = [
    ("name", SYSNAME, false),
    ("object_id", SqlType::Int, false),
    ("principal_id", SqlType::Int, true),
    ("schema_id", SqlType::Int, false),
    ("parent_object_id", SqlType::Int, false),
    ("type", SqlType::Char(Len::Fixed(2)), true),
    ("type_desc", SqlType::NVarChar(Len::Fixed(60)), true),
    ("create_date", SqlType::DateTime, false),
    ("modify_date", SqlType::DateTime, false),
    ("is_ms_shipped", SqlType::Bit, false),
    ("is_published", SqlType::Bit, false),
    ("is_schema_published", SqlType::Bit, false),
    (
        "base_object_name",
        SqlType::NVarChar(Len::Fixed(1035)),
        true,
    ),
];

/// The published columns of `sys.sequences`, in the published order: the 12 of `sys.objects`
/// and 14 more.
///
/// `start_value`, `increment`, `minimum_value`, `maximum_value`, `current_value` and
/// `last_used_value` are `bigint` here, not the published `sql_variant` (module
/// documentation).
const SEQUENCES_COLUMNS: [(&str, SqlType, bool); 26] = [
    ("name", SYSNAME, false),
    ("object_id", SqlType::Int, false),
    ("principal_id", SqlType::Int, true),
    ("schema_id", SqlType::Int, false),
    ("parent_object_id", SqlType::Int, false),
    ("type", SqlType::Char(Len::Fixed(2)), true),
    ("type_desc", SqlType::NVarChar(Len::Fixed(60)), true),
    ("create_date", SqlType::DateTime, false),
    ("modify_date", SqlType::DateTime, false),
    ("is_ms_shipped", SqlType::Bit, false),
    ("is_published", SqlType::Bit, false),
    ("is_schema_published", SqlType::Bit, false),
    ("start_value", SqlType::BigInt, false),
    ("increment", SqlType::BigInt, false),
    ("minimum_value", SqlType::BigInt, false),
    ("maximum_value", SqlType::BigInt, false),
    ("is_cycling", SqlType::Bit, true),
    ("is_cached", SqlType::Bit, true),
    ("cache_size", SqlType::Int, true),
    ("system_type_id", SqlType::TinyInt, false),
    ("user_type_id", SqlType::Int, false),
    ("precision", SqlType::TinyInt, false),
    ("scale", SqlType::TinyInt, true),
    ("current_value", SqlType::BigInt, false),
    ("is_exhausted", SqlType::Bit, false),
    ("last_used_value", SqlType::BigInt, true),
];

/// The published columns of `sys.table_types`, in the published order: the columns of
/// `sys.types` and `type_table_object_id`, `is_memory_optimized`.
const TABLE_TYPES_COLUMNS: [(&str, SqlType, bool); 17] = [
    ("name", SYSNAME, false),
    ("system_type_id", SqlType::TinyInt, false),
    ("user_type_id", SqlType::Int, false),
    ("schema_id", SqlType::Int, false),
    ("principal_id", SqlType::Int, true),
    ("max_length", SqlType::SmallInt, false),
    ("precision", SqlType::TinyInt, false),
    ("scale", SqlType::TinyInt, false),
    ("collation_name", SYSNAME, true),
    ("is_nullable", SqlType::Bit, true),
    ("is_user_defined", SqlType::Bit, false),
    ("is_assembly_type", SqlType::Bit, false),
    ("default_object_id", SqlType::Int, false),
    ("rule_object_id", SqlType::Int, false),
    ("is_table_type", SqlType::Bit, false),
    ("type_table_object_id", SqlType::Int, false),
    ("is_memory_optimized", SqlType::Bit, true),
];

/// The internal tables this file describes, in the order of the table of the module
/// documentation.
///
/// The bootstrap creates them in `master`, with no row writer calling into them (module
/// documentation).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: TRIGGERS_TABLE.to_owned(),
            columns: table_columns(&TRIGGERS_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of("triggers", &definition(&TRIGGERS_COLUMNS, TRIGGERS_TABLE)),
        },
        InternalTableDef {
            name: SYNONYMS_TABLE.to_owned(),
            columns: table_columns(&SYNONYMS_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of("synonyms", &definition(&SYNONYMS_COLUMNS, SYNONYMS_TABLE)),
        },
        InternalTableDef {
            name: SEQUENCES_TABLE.to_owned(),
            columns: table_columns(&SEQUENCES_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "sequences",
                &definition(&SEQUENCES_COLUMNS, SEQUENCES_TABLE),
            ),
        },
        InternalTableDef {
            name: TABLE_TYPES_TABLE.to_owned(),
            columns: table_columns(&TABLE_TYPES_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "table_types",
                &definition(&TABLE_TYPES_COLUMNS, TABLE_TYPES_TABLE),
            ),
        },
    ]
}

/// The columns of an internal table of this file: `database_id`, the filter of its view, then
/// the published columns in order.
fn table_columns(published: &[(&str, SqlType, bool)]) -> Vec<InternalColumnDef> {
    let mut columns = vec![column("database_id", SqlType::Int, false)];
    columns.extend(
        published
            .iter()
            .map(|&(name, ty, nullable)| column(name, ty, nullable)),
    );
    columns
}

/// The same view installed in the four system databases, `sys.<name>` in each of them, as
/// `views/sys_routines.rs` does for its own views.
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

/// The T-SQL text of a view: the published columns of `table`, read bare (module
/// documentation, section "Four internal tables, one filter column and the published ones"),
/// filtered on `database_id`. Same layout as `views/sys_routines.rs`, which holds its own copy
/// of this helper.
fn definition(published: &[(&str, SqlType, bool)], table: &str) -> String {
    let mut text = String::from("SELECT ");
    for (index, &(name, _, _)) in published.iter().enumerate() {
        if index > 0 {
            text.push_str(",\n       ");
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};

    use super::*;
    use crate::catalog::Catalog;

    /// The ordered column names of `sys.triggers` in SQL Server 2022.
    const PUBLISHED_TRIGGERS_COLUMNS: [&str; 13] = [
        "name",
        "object_id",
        "parent_class",
        "parent_class_desc",
        "parent_id",
        "type",
        "type_desc",
        "create_date",
        "modify_date",
        "is_ms_shipped",
        "is_disabled",
        "is_not_for_replication",
        "is_instead_of_trigger",
    ];
    /// The ordered column names of `sys.synonyms`.
    const PUBLISHED_SYNONYMS_COLUMNS: [&str; 13] = [
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
        "base_object_name",
    ];
    /// The ordered column names of `sys.sequences`.
    const PUBLISHED_SEQUENCES_COLUMNS: [&str; 26] = [
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
        "start_value",
        "increment",
        "minimum_value",
        "maximum_value",
        "is_cycling",
        "is_cached",
        "cache_size",
        "system_type_id",
        "user_type_id",
        "precision",
        "scale",
        "current_value",
        "is_exhausted",
        "last_used_value",
    ];
    /// The ordered column names of `sys.table_types`.
    const PUBLISHED_TABLE_TYPES_COLUMNS: [&str; 17] = [
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
        "type_table_object_id",
        "is_memory_optimized",
    ];

    fn names<'a>(published: &'a [(&'a str, SqlType, bool)]) -> Vec<&'a str> {
        published.iter().map(|&(name, _, _)| name).collect()
    }

    #[test]
    fn the_columns_of_the_four_views_are_the_published_ones() {
        assert_eq!(names(&TRIGGERS_COLUMNS), PUBLISHED_TRIGGERS_COLUMNS);
        assert_eq!(names(&SYNONYMS_COLUMNS), PUBLISHED_SYNONYMS_COLUMNS);
        assert_eq!(names(&SEQUENCES_COLUMNS), PUBLISHED_SEQUENCES_COLUMNS);
        assert_eq!(names(&TABLE_TYPES_COLUMNS), PUBLISHED_TABLE_TYPES_COLUMNS);
    }

    #[test]
    fn no_definition_holds_a_join() {
        for table in internal_tables() {
            for view in &table.views {
                assert!(
                    !view.definition.to_ascii_uppercase().contains("JOIN"),
                    "{}: {}",
                    view.name.name,
                    view.definition
                );
            }
        }
    }

    #[test]
    fn a_reserved_column_name_is_delimited() {
        assert_eq!(identifier("precision"), "[precision]");
        assert_eq!(identifier("scale"), "scale");
        assert!(definition(&SEQUENCES_COLUMNS, SEQUENCES_TABLE).contains("[precision]"));
        assert!(definition(&TABLE_TYPES_COLUMNS, TABLE_TYPES_TABLE).contains("[precision]"));
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        let views = views_of("triggers", "SELECT 1");
        let databases: Vec<&str> = views
            .iter()
            .map(|view| view.name.database.as_str())
            .collect();
        assert_eq!(databases, SYSTEM_DATABASES.to_vec());
        assert!(views.iter().all(|view| view.definition == "SELECT 1"));
    }

    #[test]
    fn each_internal_table_carries_database_id_and_the_published_columns_and_no_row() {
        for (table, published) in internal_tables().into_iter().zip([
            &TRIGGERS_COLUMNS[..],
            &SYNONYMS_COLUMNS[..],
            &SEQUENCES_COLUMNS[..],
            &TABLE_TYPES_COLUMNS[..],
        ]) {
            assert_eq!(table.columns.len(), published.len() + 1, "{}", table.name);
            assert_eq!(table.columns[0].name, "database_id");
            assert!(!table.columns[0].ty.nullable, "{}", table.name);
            for (declared, &(name, _, nullable)) in table.columns[1..].iter().zip(published) {
                assert_eq!(declared.name, name, "{}", table.name);
                assert_eq!(declared.ty.nullable, nullable, "{}: {name}", table.name);
            }
            assert!(table.rows.is_empty(), "{}", table.name);
        }
    }

    #[test]
    fn the_four_views_are_installed_and_read_in_master() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snapshot = catalog.snapshot(&handle);
        for name in ["triggers", "synonyms", "sequences", "table_types"] {
            let object = snapshot
                .resolve_object("master", Some("sys"), name, "dbo")
                .unwrap_or_else(|| panic!("sys.{name} resolves"));
            assert!(
                snapshot.view_definition(object.id).is_some(),
                "sys.{name} carries a definition"
            );
        }
    }
}
