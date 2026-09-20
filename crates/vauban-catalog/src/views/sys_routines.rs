//! `sys.procedures`, `sql_modules` and `sys.parameters`.
//!
//! # The shape of the views
//!
//! The column list of each of the three views — names, order, types — is the one SQL Server
//! 2022 publishes: `sys.procedures` 16 columns, `sql_modules` 12, `sys.parameters` 21. The
//! unit test `the_columns_of_the_three_views_are_the_published_ones` freezes the three vectors
//! of names and compares them with the text this file builds.
//!
//! # Three internal tables, one filter column and the published ones
//!
//! A view is a `SELECT` over one denormalised internal table, without a join (unit test
//! `no_definition_holds_a_join`). No `CREATE PROCEDURE` is served, so the three tables carry
//! no row: `database_id`, the filter of the view, and the published columns, declared with
//! the nullability the unit test above freezes and read bare by the view rather than written
//! as a literal, so a `NOT NULL` `bit` or `datetime` column keeps its declared nullability
//! once read through the view (section "Why a real column and not a literal"). No writer
//! inserts a row here.
//!
//! | Internal table | View |
//! |---|---|
//! | [`PROCEDURES_TABLE`] | `sys.procedures` |
//! | [`SQL_MODULES_TABLE`] | `sql_modules` |
//! | [`PARAMETERS_TABLE`] | `sys.parameters` |
//!
//! # Why a real column and not a literal
//!
//! A `CAST(0 AS bit)` or `CAST(0 AS datetime)` written directly in a `SELECT` answers the
//! right value and the right published type on its own; once the same expression sits in the
//! text of a system view and is read back through it, the column comes back with the nullable
//! TDS token instead of the fixed one a `NOT NULL` declaration would carry. A bare read of a
//! declared column does not have that problem, so this file declares the published columns
//! on their table rather than writing them as literals.
//!
//! # What these views do not publish
//!
//! `sys.parameters.default_value` is `nvarchar(max)` nullable here, where SQL Server publishes
//! a `sql_variant`: the module `types` does not serve that type yet. No row ever carries the
//! column either way.
//!
//! # Execution
//!
//! What this file produces is the shape of the three internal tables and the text of the
//! three definitions; the tables stay empty, so `sys_rows.rs` has no row to write here.

use vauban_types::{Len, SqlType, TypeInfo};

use crate::bootstrap::SYSTEM_DATABASES;
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::QualifiedName;

/// Internal table behind `sys.procedures`: one `database_id` column and the 16 published
/// ones (module documentation).
pub(crate) const PROCEDURES_TABLE: &str = "vauban_sys_procedures";

/// Internal table behind `sql_modules`: one `database_id` column and the 12 published ones
/// (module documentation).
pub(crate) const SQL_MODULES_TABLE: &str = "vauban_sys_sql_modules";

/// Internal table behind `sys.parameters`: one `database_id` column and the 21 published
/// ones (module documentation).
pub(crate) const PARAMETERS_TABLE: &str = "vauban_sys_parameters";

/// The schema the three views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The column name of the three views that is a T-SQL reserved word and is therefore written
/// as a delimited identifier in the text of a view: `precision`, a column of
/// `sys.parameters`, as `views/sys_extra.rs` delimits it in `sys.all_columns` (unit test
/// `a_reserved_column_name_is_delimited`).
const RESERVED_COLUMN_NAMES: [&str; 1] = ["precision"];

/// The published columns of `sys.procedures`, `(name, type, nullable)` in the published
/// order: the 12 of `sys.objects` and 4 more.
///
/// `type` and `type_desc` are published nullable here, where `sys.objects.type` is not
/// (`views/sys_tables.rs`, unit test
/// `the_columns_written_null_are_the_ones_with_no_datum_behind_them`): a divergence between
/// the two views on SQL Server itself, not one this file introduces.
const PROCEDURES_COLUMNS: [(&str, SqlType, bool); 16] = [
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
    ("is_auto_executed", SqlType::Bit, false),
    ("is_execution_replicated", SqlType::Bit, true),
    ("is_repl_serializable_only", SqlType::Bit, true),
    ("skips_repl_constraints", SqlType::Bit, true),
];

/// The published columns of `sql_modules`, in the published order.
const SQL_MODULES_COLUMNS: [(&str, SqlType, bool); 12] = [
    ("object_id", SqlType::Int, false),
    ("definition", SqlType::NVarChar(Len::Max), true),
    ("uses_ansi_nulls", SqlType::Bit, true),
    ("uses_quoted_identifier", SqlType::Bit, true),
    ("is_schema_bound", SqlType::Bit, true),
    ("uses_database_collation", SqlType::Bit, true),
    ("is_recompiled", SqlType::Bit, true),
    ("null_on_null_input", SqlType::Bit, true),
    ("execute_as_principal_id", SqlType::Int, true),
    ("uses_native_compilation", SqlType::Bit, true),
    ("inline_type", SqlType::Bit, true),
    ("is_inlineable", SqlType::Bit, true),
];

/// The published columns of `sys.parameters`, in the published order.
///
/// `default_value` is `nvarchar(max)`, not the published `sql_variant` (module documentation).
const PARAMETERS_COLUMNS: [(&str, SqlType, bool); 21] = [
    ("object_id", SqlType::Int, false),
    ("name", SYSNAME, true),
    ("parameter_id", SqlType::Int, false),
    ("system_type_id", SqlType::TinyInt, false),
    ("user_type_id", SqlType::Int, false),
    ("max_length", SqlType::SmallInt, false),
    ("precision", SqlType::TinyInt, false),
    ("scale", SqlType::TinyInt, false),
    ("is_output", SqlType::Bit, false),
    ("is_cursor_ref", SqlType::Bit, false),
    ("has_default_value", SqlType::Bit, false),
    ("is_xml_document", SqlType::Bit, false),
    ("default_value", SqlType::NVarChar(Len::Max), true),
    ("xml_collection_id", SqlType::Int, false),
    ("is_readonly", SqlType::Bit, false),
    ("is_nullable", SqlType::Bit, true),
    ("encryption_type", SqlType::Int, true),
    (
        "encryption_type_desc",
        SqlType::NVarChar(Len::Fixed(64)),
        true,
    ),
    (
        "encryption_algorithm_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    ("column_encryption_key_id", SqlType::Int, true),
    (
        "column_encryption_key_database_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
];

/// The internal tables this file describes, in the order of the table of the module
/// documentation.
///
/// The bootstrap creates them in `master`, with no row writer calling into them (module
/// documentation).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: PROCEDURES_TABLE.to_owned(),
            columns: table_columns(&PROCEDURES_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "procedures",
                &definition(&PROCEDURES_COLUMNS, PROCEDURES_TABLE),
            ),
        },
        InternalTableDef {
            name: SQL_MODULES_TABLE.to_owned(),
            columns: table_columns(&SQL_MODULES_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "sql_modules",
                &definition(&SQL_MODULES_COLUMNS, SQL_MODULES_TABLE),
            ),
        },
        InternalTableDef {
            name: PARAMETERS_TABLE.to_owned(),
            columns: table_columns(&PARAMETERS_COLUMNS),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "parameters",
                &definition(&PARAMETERS_COLUMNS, PARAMETERS_TABLE),
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
/// `views/sys_extra.rs` does for its own views (unit test
/// `the_views_are_installed_in_the_four_system_databases`).
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
/// documentation, section "Why a real column and not a literal"), filtered on `database_id`.
/// Same layout as `views/sys_extra.rs`, which holds its own copy of this helper.
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

    /// The ordered column names of `sys.procedures` in SQL Server 2022.
    const PUBLISHED_PROCEDURES_COLUMNS: [&str; 16] = [
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
        "is_auto_executed",
        "is_execution_replicated",
        "is_repl_serializable_only",
        "skips_repl_constraints",
    ];
    /// The ordered column names of `sql_modules`.
    const PUBLISHED_SQL_MODULES_COLUMNS: [&str; 12] = [
        "object_id",
        "definition",
        "uses_ansi_nulls",
        "uses_quoted_identifier",
        "is_schema_bound",
        "uses_database_collation",
        "is_recompiled",
        "null_on_null_input",
        "execute_as_principal_id",
        "uses_native_compilation",
        "inline_type",
        "is_inlineable",
    ];
    /// The ordered column names of `sys.parameters`.
    const PUBLISHED_PARAMETERS_COLUMNS: [&str; 21] = [
        "object_id",
        "name",
        "parameter_id",
        "system_type_id",
        "user_type_id",
        "max_length",
        "precision",
        "scale",
        "is_output",
        "is_cursor_ref",
        "has_default_value",
        "is_xml_document",
        "default_value",
        "xml_collection_id",
        "is_readonly",
        "is_nullable",
        "encryption_type",
        "encryption_type_desc",
        "encryption_algorithm_name",
        "column_encryption_key_id",
        "column_encryption_key_database_name",
    ];

    fn names<'a>(published: &'a [(&'a str, SqlType, bool)]) -> Vec<&'a str> {
        published.iter().map(|&(name, _, _)| name).collect()
    }

    #[test]
    fn the_columns_of_the_three_views_are_the_published_ones() {
        assert_eq!(names(&PROCEDURES_COLUMNS), PUBLISHED_PROCEDURES_COLUMNS);
        assert_eq!(names(&SQL_MODULES_COLUMNS), PUBLISHED_SQL_MODULES_COLUMNS);
        assert_eq!(names(&PARAMETERS_COLUMNS), PUBLISHED_PARAMETERS_COLUMNS);
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
        assert!(definition(&PARAMETERS_COLUMNS, PARAMETERS_TABLE).contains("[precision]"));
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        let views = views_of("procedures", "SELECT 1");
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
            &PROCEDURES_COLUMNS[..],
            &SQL_MODULES_COLUMNS[..],
            &PARAMETERS_COLUMNS[..],
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
    fn the_three_views_are_installed_and_read_in_master() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snapshot = catalog.snapshot(&handle);
        for name in ["procedures", "sql_modules", "parameters"] {
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
