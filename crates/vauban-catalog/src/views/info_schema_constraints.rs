//! `INFORMATION_SCHEMA.TABLE_CONSTRAINTS`, `KEY_COLUMN_USAGE`, `REFERENTIAL_CONSTRAINTS` and
//! `VIEWS`.
//!
//! # Three internal tables, and a fourth view without one
//!
//! `TABLE_CONSTRAINTS`, `KEY_COLUMN_USAGE` and `REFERENTIAL_CONSTRAINTS` each read one
//! denormalised internal table, without a join, as `views/info_schema.rs` does for `TABLES`
//! and `COLUMNS`. `VIEWS` adds no table: its rows are the same ones `sys.views`
//! (`views/sys_extra.rs`) reads from [`sys_extra::ALL_OBJECTS_TABLE`], filtered the same way,
//! so its `SystemViewDef` travels with [`REFERENTIAL_CONSTRAINTS_TABLE`] below rather than
//! opening a table of its own.
//!
//! # What a row of `TABLE_CONSTRAINTS` and of `KEY_COLUMN_USAGE` publishes
//!
//! [`TableMeta::constraints`] and the [`IndexStore`] beside it already carry what the two
//! views need: a `PRIMARY KEY` or a `UNIQUE` constraint takes its name from the index it is
//! backed by, as `sys.key_constraints` does (`views/sys_indexes.rs`); a `FOREIGN KEY` or a
//! `CHECK` takes it from the objects `constraints.rs` gave them, the same map
//! `views/sys_constraints.rs` reads for `sys.foreign_keys`, falling back to the name
//! `sys_constraints::generated_constraint_name` would give an unnamed one. `DEFAULT` publishes
//! neither view: SQL Server keeps `TABLE_CONSTRAINTS` and `KEY_COLUMN_USAGE` to the four kinds
//! that carry a key or a predicate. `KEY_COLUMN_USAGE.ORDINAL_POSITION` is the position of a
//! column **in its key**, [`IndexMeta::columns`] order for a `PRIMARY KEY` or a `UNIQUE`
//! constraint and [`ConstraintMeta::ForeignKey::columns`] order for a `FOREIGN KEY` — not the
//! `index_column_id` a clustered key renumbers by `column_id` (`views/sys_indexes.rs`,
//! `published_key_columns`); a `FOREIGN KEY` publishes its own constrained columns here, not
//! the columns of the key it points at.
//!
//! # `REFERENTIAL_CONSTRAINTS`
//!
//! One row per `FOREIGN KEY`. `UNIQUE_CONSTRAINT_NAME` is the name of the index
//! [`ConstraintMeta::ForeignKey::referenced_index`] names, read from the [`IndexStore`] of the
//! **referenced** table — the store holds every index of the catalogue, keyed by table, so
//! this crosses tables without a join in the view text. `UNIQUE_CONSTRAINT_SCHEMA` cannot cross
//! tables the same way: the row is built from the referencing [`TableMeta`] alone, which does
//! not carry the referenced one. VaubanDB has no `CREATE SCHEMA` yet, so every table a
//! statement can create lives in the schema its own `CREATE TABLE` named, and a `FOREIGN KEY`
//! and the table it points at are written the same way; `UNIQUE_CONSTRAINT_SCHEMA` therefore
//! reads the referencing table's own schema, correct while that holds and to revisit once a
//! table can move schemas after creation. `MATCH_OPTION` is the constant `SIMPLE` on every row,
//! `UPDATE_RULE` and `DELETE_RULE` the four values a `FOREIGN KEY` clause writes, spelled with a
//! space (`referential_rule`) rather than the underscore `sys.foreign_keys.*_desc` uses
//! (`views/sys_constraints.rs`, `referential_action_desc`), the two views disagreeing on that
//! point.
//!
//! # `VIEWS`
//!
//! [`sys_extra::ALL_OBJECTS_TABLE`] carries `database_id` `NULL` for a system view, so the
//! filter this file reuses matches it from every database, `master` and one a client creates
//! alike: the same `(database_id = DB_ID() OR database_id IS NULL) AND type = 'V '` `sys.views`
//! filters by. `TABLE_SCHEMA` is `SCHEMA_NAME(schema_id)`, the one column of this file that
//! reads a function rather than a bare column: `sys_extra::ALL_OBJECTS_TABLE` stores a
//! `schema_id`, not a name, for the reason `views/sys_tables.rs` gives. `sys.views` already
//! lists the system views themselves where SQL Server does not — a fresh database's
//! `sys.views` and `INFORMATION_SCHEMA.VIEWS` render no row there — so `VIEWS` inherits the
//! same difference, the choice `views/sys_extra.rs` made when it wrote `system_view_rows`.
//! `VIEW_DEFINITION` is `CAST(NULL AS nvarchar(4000))` on every row: no internal table of this
//! crate keeps the T-SQL text of a system view against its row, `CHECK_OPTION` is the constant
//! `NONE` and `IS_UPDATABLE` the constant `NO`, VaubanDB writing through no view. Deliberate
//! differences for now.
//!
//! # Execution
//!
//! What this file produces is the shape of the three internal tables, the text of the four
//! definitions and the three functions that turn a `TableMeta` slice into rows; `sys_rows.rs`
//! writes those rows into `storage` at each `CREATE TABLE` and `DROP TABLE`, and the binder
//! expands the text of a view in place of its name.

use std::collections::BTreeMap;

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::{DbId, IndexId, Row};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::SYSTEM_DATABASES;
use crate::def::{InternalColumnDef, InternalTableDef, RefAction, SystemViewDef};
use crate::ids::{ColumnId, ObjectId};
use crate::index::IndexStore;
use crate::meta::{ConstraintMeta, IndexMeta, QualifiedName, TableMeta};
use crate::views::{sys_constraints, sys_extra};

/// Internal table of the `PRIMARY KEY` / `UNIQUE` / `FOREIGN KEY` / `CHECK` constraints, read
/// by `INFORMATION_SCHEMA.TABLE_CONSTRAINTS`.
pub(crate) const TABLE_CONSTRAINTS_TABLE: &str = "vauban_is_table_constraints";

/// Internal table of the key columns of those constraints, read by
/// `INFORMATION_SCHEMA.KEY_COLUMN_USAGE`.
pub(crate) const KEY_COLUMN_USAGE_TABLE: &str = "vauban_is_key_column_usage";

/// Internal table of the `FOREIGN KEY` constraints, read by
/// `INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS`.
pub(crate) const REFERENTIAL_CONSTRAINTS_TABLE: &str = "vauban_is_referential_constraints";

/// The schema the four views of this file live in, created by the bootstrap in each database.
const VIEW_SCHEMA: &str = "INFORMATION_SCHEMA";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// `TABLE_CONSTRAINTS.CONSTRAINT_TYPE` of a `PRIMARY KEY`.
const PRIMARY_KEY_TYPE: &str = "PRIMARY KEY";

/// `TABLE_CONSTRAINTS.CONSTRAINT_TYPE` of a `UNIQUE` constraint.
const UNIQUE_TYPE: &str = "UNIQUE";

/// `TABLE_CONSTRAINTS.CONSTRAINT_TYPE` of a `FOREIGN KEY`.
const FOREIGN_KEY_TYPE: &str = "FOREIGN KEY";

/// `TABLE_CONSTRAINTS.CONSTRAINT_TYPE` of a `CHECK`.
const CHECK_TYPE: &str = "CHECK";

/// The filter a view reads over [`sys_extra::ALL_OBJECTS_TABLE`] needs to see a system view
/// from every database, its own copy of `views/sys_extra.rs`, `ANY_DATABASE_FILTER` (module
/// documentation, section "`VIEWS`").
const ANY_DATABASE_FILTER: &str = "database_id = DB_ID()\n    OR database_id IS NULL";

/// `sys.objects.type` of a view, this file's own copy of `views/sys_extra.rs`, `VIEW_TYPE`.
const VIEW_TYPE: &str = "V ";

/// Where each column of [`TABLE_CONSTRAINTS_TABLE`] sits in a [`Row`], as `bootstrap.rs` does
/// for its own tables: a writer of a row addresses it through these constants rather than
/// restating the order.
pub(crate) mod table_constraints_columns {
    /// `database_id int`: the database the constraint's table belongs to, the filter of the
    /// view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `constraint_schema nvarchar(128)`.
    pub(crate) const CONSTRAINT_SCHEMA: usize = 1;
    /// `constraint_name nvarchar(128)`.
    pub(crate) const CONSTRAINT_NAME: usize = 2;
    /// `table_schema nvarchar(128)`.
    pub(crate) const TABLE_SCHEMA: usize = 3;
    /// `table_name nvarchar(128)`.
    pub(crate) const TABLE_NAME: usize = 4;
    /// `constraint_type varchar(11)`: `PRIMARY KEY`, `UNIQUE`, `FOREIGN KEY` or `CHECK`.
    pub(crate) const CONSTRAINT_TYPE: usize = 5;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 6;
}

/// Where each column of [`KEY_COLUMN_USAGE_TABLE`] sits in a [`Row`]. Same rule as
/// [`table_constraints_columns`].
pub(crate) mod key_column_usage_columns {
    /// `database_id int`: the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `constraint_schema nvarchar(128)`.
    pub(crate) const CONSTRAINT_SCHEMA: usize = 1;
    /// `constraint_name nvarchar(128)`.
    pub(crate) const CONSTRAINT_NAME: usize = 2;
    /// `table_schema nvarchar(128)`.
    pub(crate) const TABLE_SCHEMA: usize = 3;
    /// `table_name nvarchar(128)`.
    pub(crate) const TABLE_NAME: usize = 4;
    /// `column_name nvarchar(128)`.
    pub(crate) const COLUMN_NAME: usize = 5;
    /// `ordinal_position int`: the position of the column in its key, from `1`.
    pub(crate) const ORDINAL_POSITION: usize = 6;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 7;
}

/// Where each column of [`REFERENTIAL_CONSTRAINTS_TABLE`] sits in a [`Row`]. Same rule as
/// [`table_constraints_columns`]; `table_schema` and `table_name` are not read by the view —
/// `REFERENTIAL_CONSTRAINTS` names no such column (module documentation) — and exist only so a
/// `DROP TABLE` can find the rows of the table it constrained (`sys_rows.rs`).
pub(crate) mod referential_constraints_columns {
    /// `database_id int`: the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `table_schema nvarchar(128)`: not read by the view, `sys_rows.rs` only.
    pub(crate) const TABLE_SCHEMA: usize = 1;
    /// `table_name nvarchar(128)`: not read by the view, `sys_rows.rs` only.
    pub(crate) const TABLE_NAME: usize = 2;
    /// `constraint_schema nvarchar(128)`.
    pub(crate) const CONSTRAINT_SCHEMA: usize = 3;
    /// `constraint_name nvarchar(128)`.
    pub(crate) const CONSTRAINT_NAME: usize = 4;
    /// `unique_constraint_schema nvarchar(128)`.
    pub(crate) const UNIQUE_CONSTRAINT_SCHEMA: usize = 5;
    /// `unique_constraint_name nvarchar(128)`.
    pub(crate) const UNIQUE_CONSTRAINT_NAME: usize = 6;
    /// `update_rule varchar(11)`.
    pub(crate) const UPDATE_RULE: usize = 7;
    /// `delete_rule varchar(11)`.
    pub(crate) const DELETE_RULE: usize = 8;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 9;
}

/// The select list of `INFORMATION_SCHEMA.TABLE_CONSTRAINTS`: its 9 columns, 4 of which read
/// [`TABLE_CONSTRAINTS_TABLE`].
const TABLE_CONSTRAINTS_VIEW: [(&str, &str); 9] = [
    ("CONSTRAINT_CATALOG", "DB_NAME()"),
    ("CONSTRAINT_SCHEMA", "constraint_schema"),
    ("CONSTRAINT_NAME", "constraint_name"),
    ("TABLE_CATALOG", "DB_NAME()"),
    ("TABLE_SCHEMA", "table_schema"),
    ("TABLE_NAME", "table_name"),
    ("CONSTRAINT_TYPE", "constraint_type"),
    ("IS_DEFERRABLE", "CAST(N'NO' AS varchar(2))"),
    ("INITIALLY_DEFERRED", "CAST(N'NO' AS varchar(2))"),
];

/// The select list of `INFORMATION_SCHEMA.KEY_COLUMN_USAGE`: its 8 columns, 6 of which read
/// [`KEY_COLUMN_USAGE_TABLE`].
const KEY_COLUMN_USAGE_VIEW: [(&str, &str); 8] = [
    ("CONSTRAINT_CATALOG", "DB_NAME()"),
    ("CONSTRAINT_SCHEMA", "constraint_schema"),
    ("CONSTRAINT_NAME", "constraint_name"),
    ("TABLE_CATALOG", "DB_NAME()"),
    ("TABLE_SCHEMA", "table_schema"),
    ("TABLE_NAME", "table_name"),
    ("COLUMN_NAME", "column_name"),
    ("ORDINAL_POSITION", "ordinal_position"),
];

/// The select list of `INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS`: its 9 columns, 6 of which
/// read [`REFERENTIAL_CONSTRAINTS_TABLE`].
const REFERENTIAL_CONSTRAINTS_VIEW: [(&str, &str); 9] = [
    ("CONSTRAINT_CATALOG", "DB_NAME()"),
    ("CONSTRAINT_SCHEMA", "constraint_schema"),
    ("CONSTRAINT_NAME", "constraint_name"),
    ("UNIQUE_CONSTRAINT_CATALOG", "DB_NAME()"),
    ("UNIQUE_CONSTRAINT_SCHEMA", "unique_constraint_schema"),
    ("UNIQUE_CONSTRAINT_NAME", "unique_constraint_name"),
    ("MATCH_OPTION", "CAST(N'SIMPLE' AS varchar(7))"),
    ("UPDATE_RULE", "update_rule"),
    ("DELETE_RULE", "delete_rule"),
];

/// The select list of `INFORMATION_SCHEMA.VIEWS`: its 6 columns, over
/// [`sys_extra::ALL_OBJECTS_TABLE`] (module documentation, section "`VIEWS`").
const VIEWS_VIEW: [(&str, &str); 6] = [
    ("TABLE_CATALOG", "DB_NAME()"),
    ("TABLE_SCHEMA", "SCHEMA_NAME(schema_id)"),
    ("TABLE_NAME", "name"),
    ("VIEW_DEFINITION", "CAST(NULL AS nvarchar(4000))"),
    ("CHECK_OPTION", "CAST(N'NONE' AS varchar(7))"),
    ("IS_UPDATABLE", "CAST(N'NO' AS varchar(2))"),
];

/// The internal tables this file describes: the three tables and the four view definitions.
///
/// The bootstrap creates the three tables in `master`, with the views [`TABLE_CONSTRAINTS_VIEW`],
/// [`KEY_COLUMN_USAGE_VIEW`] and [`REFERENTIAL_CONSTRAINTS_VIEW`] build. `VIEWS` reads no table
/// of its own, so its definition travels with [`REFERENTIAL_CONSTRAINTS_TABLE`], the same way
/// `SCHEMATA` travels with [`crate::views::info_schema::TABLES_TABLE`].
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: TABLE_CONSTRAINTS_TABLE.to_owned(),
            columns: table_constraints_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "TABLE_CONSTRAINTS",
                &definition(
                    &TABLE_CONSTRAINTS_VIEW,
                    TABLE_CONSTRAINTS_TABLE,
                    Some("database_id = DB_ID()"),
                ),
            ),
        },
        InternalTableDef {
            name: KEY_COLUMN_USAGE_TABLE.to_owned(),
            columns: key_column_usage_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "KEY_COLUMN_USAGE",
                &definition(
                    &KEY_COLUMN_USAGE_VIEW,
                    KEY_COLUMN_USAGE_TABLE,
                    Some("database_id = DB_ID()"),
                ),
            ),
        },
        InternalTableDef {
            name: REFERENTIAL_CONSTRAINTS_TABLE.to_owned(),
            columns: referential_constraints_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: [
                views_of(
                    "REFERENTIAL_CONSTRAINTS",
                    &definition(
                        &REFERENTIAL_CONSTRAINTS_VIEW,
                        REFERENTIAL_CONSTRAINTS_TABLE,
                        Some("database_id = DB_ID()"),
                    ),
                ),
                views_of("VIEWS", &views_definition()),
            ]
            .concat(),
        },
    ]
}

/// The text of `INFORMATION_SCHEMA.VIEWS`, read over [`sys_extra::ALL_OBJECTS_TABLE`] rather
/// than a table of this file (module documentation, section "`VIEWS`").
fn views_definition() -> String {
    definition(
        &VIEWS_VIEW,
        sys_extra::ALL_OBJECTS_TABLE,
        Some(&format!(
            "({ANY_DATABASE_FILTER})\n   AND type = '{VIEW_TYPE}'"
        )),
    )
}

/// The rows of [`TABLE_CONSTRAINTS_TABLE`] for `tables`: one per `PRIMARY KEY`, `UNIQUE`,
/// `FOREIGN KEY` or `CHECK` constraint, in the declaration order [`TableMeta::constraints`]
/// keeps. A `DEFAULT` adds nothing (module documentation).
///
/// # Errors
///
/// [`InternalError::Bug`] when a [`DbId`] does not fit in the `int` the view publishes, or when
/// a `PRIMARY KEY` / `UNIQUE` constraint names an index the [`IndexStore`] does not hold.
pub(crate) fn table_constraint_rows(
    tables: &[TableMeta],
    indexes: &IndexStore,
    names: &BTreeMap<ObjectId, String>,
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for (position, constraint) in table.constraints.iter().enumerate() {
            let (name, constraint_type) = match constraint {
                ConstraintMeta::PrimaryKey(id) => (
                    indexed_meta(table, indexes, *id)?.name.clone(),
                    PRIMARY_KEY_TYPE,
                ),
                ConstraintMeta::Unique(id) => {
                    (indexed_meta(table, indexes, *id)?.name.clone(), UNIQUE_TYPE)
                }
                ConstraintMeta::ForeignKey {
                    constraint: object,
                    columns,
                    ..
                } => {
                    let generated = sys_constraints::generated_constraint_name(
                        "FK",
                        table,
                        single_column_name(table, columns),
                        position,
                    );
                    (
                        constraint_display_name(names, *object, &generated),
                        FOREIGN_KEY_TYPE,
                    )
                }
                ConstraintMeta::Check {
                    constraint: object, ..
                } => {
                    let generated =
                        sys_constraints::generated_constraint_name("CK", table, None, position);
                    (
                        constraint_display_name(names, *object, &generated),
                        CHECK_TYPE,
                    )
                }
                ConstraintMeta::Default { .. } => continue,
            };
            let mut row = vec![Value::Null; table_constraints_columns::WIDTH];
            row[table_constraints_columns::DATABASE_ID] = Value::I32(database);
            row[table_constraints_columns::CONSTRAINT_SCHEMA] = text(&table.schema);
            row[table_constraints_columns::CONSTRAINT_NAME] = text(&name);
            row[table_constraints_columns::TABLE_SCHEMA] = text(&table.schema);
            row[table_constraints_columns::TABLE_NAME] = text(&table.name);
            row[table_constraints_columns::CONSTRAINT_TYPE] = text(constraint_type);
            rows.push(Row(row));
        }
    }
    Ok(rows)
}

/// The rows of [`KEY_COLUMN_USAGE_TABLE`] for `tables`: one per column of a `PRIMARY KEY`,
/// `UNIQUE` or `FOREIGN KEY` constraint, in key order within each constraint, in the
/// declaration order of [`TableMeta::constraints`]. `CHECK` and `DEFAULT` add nothing.
///
/// # Errors
///
/// Those of [`table_constraint_rows`], plus [`InternalError::Bug`] when a key names a position,
/// or a `FOREIGN KEY` a column, that the table does not hold.
pub(crate) fn key_column_usage_rows(
    tables: &[TableMeta],
    indexes: &IndexStore,
    names: &BTreeMap<ObjectId, String>,
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for (position, constraint) in table.constraints.iter().enumerate() {
            let (name, columns): (String, Vec<&str>) = match constraint {
                ConstraintMeta::PrimaryKey(id) | ConstraintMeta::Unique(id) => {
                    let meta = indexed_meta(table, indexes, *id)?;
                    (meta.name.clone(), key_column_names(table, meta)?)
                }
                ConstraintMeta::ForeignKey {
                    constraint: object,
                    columns,
                    ..
                } => {
                    let generated = sys_constraints::generated_constraint_name(
                        "FK",
                        table,
                        single_column_name(table, columns),
                        position,
                    );
                    let name = constraint_display_name(names, *object, &generated);
                    let named: SqlResult<Vec<&str>> =
                        columns.iter().map(|id| column_named(table, *id)).collect();
                    (name, named?)
                }
                ConstraintMeta::Check { .. } | ConstraintMeta::Default { .. } => continue,
            };
            for (key_position_in_key, column) in columns.into_iter().enumerate() {
                let mut row = vec![Value::Null; key_column_usage_columns::WIDTH];
                row[key_column_usage_columns::DATABASE_ID] = Value::I32(database);
                row[key_column_usage_columns::CONSTRAINT_SCHEMA] = text(&table.schema);
                row[key_column_usage_columns::CONSTRAINT_NAME] = text(&name);
                row[key_column_usage_columns::TABLE_SCHEMA] = text(&table.schema);
                row[key_column_usage_columns::TABLE_NAME] = text(&table.name);
                row[key_column_usage_columns::COLUMN_NAME] = text(column);
                row[key_column_usage_columns::ORDINAL_POSITION] =
                    Value::I32(key_position(key_position_in_key));
                rows.push(Row(row));
            }
        }
    }
    Ok(rows)
}

/// The rows of [`REFERENTIAL_CONSTRAINTS_TABLE`] for `tables`: one per `FOREIGN KEY`
/// constraint, in the declaration order of [`TableMeta::constraints`].
///
/// # Errors
///
/// Those of [`table_constraint_rows`].
pub(crate) fn referential_constraint_rows(
    tables: &[TableMeta],
    indexes: &IndexStore,
    names: &BTreeMap<ObjectId, String>,
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for (position, constraint) in table.constraints.iter().enumerate() {
            let ConstraintMeta::ForeignKey {
                constraint: object,
                columns,
                referenced_table,
                referenced_index,
                on_delete,
                on_update,
                ..
            } = constraint
            else {
                continue;
            };
            let generated = sys_constraints::generated_constraint_name(
                "FK",
                table,
                single_column_name(table, columns),
                position,
            );
            let name = constraint_display_name(names, *object, &generated);
            let unique_name = indexes
                .of_table(*referenced_table)
                .into_iter()
                .find(|meta| meta.id == *referenced_index)
                .map(|meta| meta.name.as_str());
            let mut row = vec![Value::Null; referential_constraints_columns::WIDTH];
            row[referential_constraints_columns::DATABASE_ID] = Value::I32(database);
            row[referential_constraints_columns::TABLE_SCHEMA] = text(&table.schema);
            row[referential_constraints_columns::TABLE_NAME] = text(&table.name);
            row[referential_constraints_columns::CONSTRAINT_SCHEMA] = text(&table.schema);
            row[referential_constraints_columns::CONSTRAINT_NAME] = text(&name);
            // `UNIQUE_CONSTRAINT_SCHEMA` stands in for the referenced table's own schema
            // (module documentation, section "`REFERENTIAL_CONSTRAINTS`").
            row[referential_constraints_columns::UNIQUE_CONSTRAINT_SCHEMA] = text(&table.schema);
            row[referential_constraints_columns::UNIQUE_CONSTRAINT_NAME] =
                unique_name.map_or(Value::Null, text);
            row[referential_constraints_columns::UPDATE_RULE] = text(referential_rule(*on_update));
            row[referential_constraints_columns::DELETE_RULE] = text(referential_rule(*on_delete));
            rows.push(Row(row));
        }
    }
    Ok(rows)
}

/// The `UPDATE_RULE` / `DELETE_RULE` text of `action`, spelled with a space (module
/// documentation, section "`REFERENTIAL_CONSTRAINTS`").
fn referential_rule(action: RefAction) -> &'static str {
    match action {
        RefAction::NoAction => "NO ACTION",
        RefAction::Cascade => "CASCADE",
        RefAction::SetNull => "SET NULL",
        RefAction::SetDefault => "SET DEFAULT",
    }
}

/// The name published for the constraint object `object`: the one `names` carries, the
/// generated one otherwise (`sys_constraints::foreign_key_rows`, same fallback).
fn constraint_display_name(
    names: &BTreeMap<ObjectId, String>,
    object: ObjectId,
    generated: &str,
) -> String {
    names
        .get(&object)
        .cloned()
        .unwrap_or_else(|| generated.to_owned())
}

/// The index of `table` whose identifier is `id`.
///
/// # Errors
///
/// [`InternalError::Bug`] when the [`IndexStore`] holds no such index of `table`, as
/// `views/sys_indexes.rs`, `key_constraint_rows` reports on the same case.
fn indexed_meta<'a>(
    table: &TableMeta,
    indexes: &'a IndexStore,
    id: IndexId,
) -> SqlResult<&'a IndexMeta> {
    indexes
        .of_table(table.id)
        .into_iter()
        .find(|meta| meta.id == id)
        .ok_or_else(|| {
            InternalError::Bug(format!(
                "INFORMATION_SCHEMA.TABLE_CONSTRAINTS: a constraint of table {} is backed by \
                 the index {id}, which the catalogue does not hold",
                table.name
            ))
            .into()
        })
}

/// The column names of `table` that carry the key `meta`, in key order.
///
/// # Errors
///
/// [`InternalError::Bug`] when a key column names a position `table` does not hold.
fn key_column_names<'a>(table: &'a TableMeta, meta: &IndexMeta) -> SqlResult<Vec<&'a str>> {
    meta.columns
        .iter()
        .map(|key| {
            table
                .columns
                .iter()
                .find(|column| column.ordinal == key.column)
                .map(|column| column.name.as_str())
                .ok_or_else(|| {
                    InternalError::Bug(format!(
                        "INFORMATION_SCHEMA.KEY_COLUMN_USAGE: a key of table {} names the \
                         position {}, which holds no column there",
                        table.name, key.column
                    ))
                    .into()
                })
        })
        .collect()
}

/// The name of the column of `table` whose identifier is `id`.
///
/// # Errors
///
/// [`InternalError::Bug`] when `table` holds no such column.
fn column_named(table: &TableMeta, id: ColumnId) -> SqlResult<&str> {
    table
        .columns
        .iter()
        .find(|column| column.id == id)
        .map(|column| column.name.as_str())
        .ok_or_else(|| {
            InternalError::Bug(format!(
                "INFORMATION_SCHEMA.KEY_COLUMN_USAGE: a FOREIGN KEY of table {} names the \
                 column {id}, which the table does not hold",
                table.name
            ))
            .into()
        })
}

/// The name of the single column `columns` holds, `None` when it holds another number of
/// them or names one the table does not have — the same shape
/// `sys_constraints::foreign_key_rows` reads for its own generated name.
fn single_column_name<'a>(table: &'a TableMeta, columns: &[ColumnId]) -> Option<&'a str> {
    match columns {
        [only] => column_named(table, *only).ok(),
        _ => None,
    }
}

/// The `ORDINAL_POSITION`/`constraint_column_id` of the column at `position` in its key: the
/// position, from `1`, saturating rather than erroring on a key wider than an `int` counts, as
/// `sys_constraints::key_position` does.
fn key_position(position: usize) -> i32 {
    i32::try_from(position.saturating_add(1)).unwrap_or(i32::MAX)
}

/// The `int` a [`DbId`] is published as, the four views being per-database while the internal
/// tables hold the databases side by side.
///
/// # Errors
///
/// [`InternalError::Bug`], as `bootstrap.rs` and the other files of `views/` report on the
/// same value.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "INFORMATION_SCHEMA: database id {id} does not fit in an int"
        ))
        .into()
    })
}

/// The columns of [`TABLE_CONSTRAINTS_TABLE`], in the order of [`table_constraints_columns`].
///
/// The nullability declared on a column is the published one of the view column it feeds, as
/// `views/info_schema.rs` does for [`crate::views::info_schema::TABLES_TABLE`].
fn table_constraints_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("constraint_schema", SYSNAME, true),
        column("constraint_name", SYSNAME, false),
        column("table_schema", SYSNAME, true),
        column("table_name", SYSNAME, true),
        column("constraint_type", SqlType::VarChar(Len::Fixed(11)), true),
    ]
}

/// The columns of [`KEY_COLUMN_USAGE_TABLE`], in the order of [`key_column_usage_columns`].
/// Same rule on nullability as [`table_constraints_table_columns`].
fn key_column_usage_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("constraint_schema", SYSNAME, true),
        column("constraint_name", SYSNAME, false),
        column("table_schema", SYSNAME, true),
        column("table_name", SYSNAME, false),
        column("column_name", SYSNAME, true),
        column("ordinal_position", SqlType::Int, false),
    ]
}

/// The columns of [`REFERENTIAL_CONSTRAINTS_TABLE`], in the order of
/// [`referential_constraints_columns`]. `table_schema` and `table_name` are not read by the
/// view and are declared `NOT NULL`, the value `sys_rows.rs` always writes there.
fn referential_constraints_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("table_schema", SYSNAME, false),
        column("table_name", SYSNAME, false),
        column("constraint_schema", SYSNAME, true),
        column("constraint_name", SYSNAME, false),
        column("unique_constraint_schema", SYSNAME, true),
        column("unique_constraint_name", SYSNAME, true),
        column("update_rule", SqlType::VarChar(Len::Fixed(11)), true),
        column("delete_rule", SqlType::VarChar(Len::Fixed(11)), true),
    ]
}

/// The same view installed in the four system databases, `INFORMATION_SCHEMA.<name>` in each of
/// them, this file's own copy of `views/info_schema.rs`, `views_of`.
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

/// The T-SQL text of a view, this file's own copy of `views/info_schema.rs`, `definition`.
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
        text.push_str(name);
    }
    text.push_str("\n  FROM master.dbo.");
    text.push_str(table);
    if let Some(filter) = filter {
        text.push_str("\n WHERE ");
        text.push_str(filter);
    }
    text
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

    use vauban_storage::{MemoryStorage, Storage, TableId};
    use vauban_txn::{IsolationLevel, TransactionManager};

    use super::*;
    use crate::catalog::Catalog;
    use crate::def::{ColumnDef, ConstraintDef, SortedColumn, TableDef};
    use crate::{index, table};

    /// The ordered column names of `INFORMATION_SCHEMA.TABLE_CONSTRAINTS` in SQL Server 2022.
    const PUBLISHED_TABLE_CONSTRAINTS_COLUMNS: [&str; 9] = [
        "CONSTRAINT_CATALOG",
        "CONSTRAINT_SCHEMA",
        "CONSTRAINT_NAME",
        "TABLE_CATALOG",
        "TABLE_SCHEMA",
        "TABLE_NAME",
        "CONSTRAINT_TYPE",
        "IS_DEFERRABLE",
        "INITIALLY_DEFERRED",
    ];

    /// The ordered column names of `INFORMATION_SCHEMA.KEY_COLUMN_USAGE`.
    const PUBLISHED_KEY_COLUMN_USAGE_COLUMNS: [&str; 8] = [
        "CONSTRAINT_CATALOG",
        "CONSTRAINT_SCHEMA",
        "CONSTRAINT_NAME",
        "TABLE_CATALOG",
        "TABLE_SCHEMA",
        "TABLE_NAME",
        "COLUMN_NAME",
        "ORDINAL_POSITION",
    ];

    /// The ordered column names of `INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS`.
    const PUBLISHED_REFERENTIAL_CONSTRAINTS_COLUMNS: [&str; 9] = [
        "CONSTRAINT_CATALOG",
        "CONSTRAINT_SCHEMA",
        "CONSTRAINT_NAME",
        "UNIQUE_CONSTRAINT_CATALOG",
        "UNIQUE_CONSTRAINT_SCHEMA",
        "UNIQUE_CONSTRAINT_NAME",
        "MATCH_OPTION",
        "UPDATE_RULE",
        "DELETE_RULE",
    ];

    /// The ordered column names of `INFORMATION_SCHEMA.VIEWS`.
    const PUBLISHED_VIEWS_COLUMNS: [&str; 6] = [
        "TABLE_CATALOG",
        "TABLE_SCHEMA",
        "TABLE_NAME",
        "VIEW_DEFINITION",
        "CHECK_OPTION",
        "IS_UPDATABLE",
    ];

    /// The column names a `SELECT` text of `views/` publishes, in order.
    fn published_columns(definition: &str) -> Vec<String> {
        definition
            .split("\n  FROM ")
            .next()
            .expect("a FROM clause")
            .trim_start_matches("SELECT ")
            .split(",\n       ")
            .map(|item| {
                item.rsplit(' ')
                    .next()
                    .unwrap_or_else(|| panic!("{item} names its column"))
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_column_lists_match_the_published_ones() {
        assert_eq!(
            published_columns(&definition(
                &TABLE_CONSTRAINTS_VIEW,
                TABLE_CONSTRAINTS_TABLE,
                None
            )),
            PUBLISHED_TABLE_CONSTRAINTS_COLUMNS
        );
        assert_eq!(
            published_columns(&definition(
                &KEY_COLUMN_USAGE_VIEW,
                KEY_COLUMN_USAGE_TABLE,
                None
            )),
            PUBLISHED_KEY_COLUMN_USAGE_COLUMNS
        );
        assert_eq!(
            published_columns(&definition(
                &REFERENTIAL_CONSTRAINTS_VIEW,
                REFERENTIAL_CONSTRAINTS_TABLE,
                None
            )),
            PUBLISHED_REFERENTIAL_CONSTRAINTS_COLUMNS
        );
        assert_eq!(
            published_columns(&views_definition()),
            PUBLISHED_VIEWS_COLUMNS
        );
    }

    #[test]
    fn no_definition_holds_a_join() {
        for definition in [
            definition(&TABLE_CONSTRAINTS_VIEW, TABLE_CONSTRAINTS_TABLE, None),
            definition(&KEY_COLUMN_USAGE_VIEW, KEY_COLUMN_USAGE_TABLE, None),
            definition(
                &REFERENTIAL_CONSTRAINTS_VIEW,
                REFERENTIAL_CONSTRAINTS_TABLE,
                None,
            ),
            views_definition(),
        ] {
            assert!(
                !definition.to_ascii_uppercase().contains("JOIN"),
                "{definition}"
            );
        }
    }

    #[test]
    fn views_reads_the_same_filter_as_sys_views() {
        // `views/sys_extra.rs` keeps `ANY_DATABASE_FILTER` private, so this checks the two
        // copies by shape rather than by identity: both let a `database_id IS NULL` row
        // through, which is what a fresh database needs to see a system view (module
        // documentation, section "`VIEWS`").
        let definition = views_definition();
        assert!(definition.contains("database_id IS NULL"), "{definition}");
        assert!(definition.contains("type = 'V '"), "{definition}");
    }

    /// A bootstrapped catalogue over a fresh `MemoryStorage`, with its transaction manager.
    fn instance() -> (Catalog, Arc<TransactionManager>) {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&txn)).expect("bootstrap");
        (catalog, txn)
    }

    /// Runs `with` against the refreshed [`IndexStore`] of `catalog`'s table store, as
    /// `views/sys_indexes.rs`, `published` refreshes both stores before reading either.
    fn with_index_store<T>(catalog: &Catalog, with: impl FnOnce(&IndexStore) -> T) -> T {
        let mut store = table::store(catalog);
        table::refresh(catalog, &mut store).expect("refresh of the tables");
        index::refresh(catalog, &mut store).expect("refresh of the indexes");
        with(&store.indexes)
    }

    /// The stored text of the integer literal `value`, the shape `tests/sys_constraints.rs`
    /// gives a `CHECK` predicate.
    fn integer(value: &str) -> String {
        format!("(({value}))")
    }

    /// The names `sys_rows::write` stashes at `CREATE TABLE`, rebuilt from
    /// `Catalog::constraints_of` after the fact: what `table_constraint_rows` and its
    /// neighbours read as `names` once the stash `apply_table_constraints` filled has already
    /// been drained by the real write.
    fn real_constraint_names(
        catalog: &Catalog,
        tables: &[TableMeta],
    ) -> BTreeMap<ObjectId, String> {
        let mut names = BTreeMap::new();
        for table in tables {
            let objects = catalog.constraints_of(table.id).expect("constraints_of");
            names.extend(crate::sys_rows::constraint_names_of(&objects));
        }
        names
    }

    /// A column of a definition, with its type and nothing else on it.
    fn col(name: &str, ty: SqlType, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, nullable),
            default: None,
            identity: None,
            computed: None,
        }
    }

    /// A table `(a int NOT NULL, b int NOT NULL, u int NOT NULL)` with a two-column clustered
    /// `PRIMARY KEY (a, b)` and a `UNIQUE (u)`, in `database`.
    fn parent_def(database: &str, name: &str) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: database.to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns: vec![
                col("a", SqlType::Int, false),
                col("b", SqlType::Int, false),
                col("u", SqlType::Int, false),
            ],
            constraints: vec![
                ConstraintDef::PrimaryKey {
                    name: Some(format!("pk_{name}")),
                    columns: vec![
                        SortedColumn {
                            column: "a".to_owned(),
                            descending: false,
                        },
                        SortedColumn {
                            column: "b".to_owned(),
                            descending: false,
                        },
                    ],
                    clustered: true,
                },
                ConstraintDef::Unique {
                    name: Some(format!("uq_{name}")),
                    columns: vec![SortedColumn {
                        column: "u".to_owned(),
                        descending: false,
                    }],
                    clustered: false,
                },
            ],
        }
    }

    /// A table `(id int NOT NULL, fa int NOT NULL, fb int NOT NULL)` with a `PRIMARY KEY (id)`
    /// and a named `FOREIGN KEY (fa, fb)` pointing at `parent`'s `PRIMARY KEY`, in `database`.
    fn child_def(database: &str, name: &str, parent: &str) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: database.to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns: vec![
                col("id", SqlType::Int, false),
                col("fa", SqlType::Int, false),
                col("fb", SqlType::Int, false),
            ],
            constraints: vec![
                ConstraintDef::PrimaryKey {
                    name: Some(format!("pk_{name}")),
                    columns: vec![SortedColumn {
                        column: "id".to_owned(),
                        descending: false,
                    }],
                    clustered: true,
                },
                ConstraintDef::ForeignKey {
                    name: Some(format!("fk_{name}")),
                    columns: vec!["fa".to_owned(), "fb".to_owned()],
                    referenced: QualifiedName {
                        database: database.to_owned(),
                        schema: "dbo".to_owned(),
                        name: parent.to_owned(),
                    },
                    referenced_columns: vec!["a".to_owned(), "b".to_owned()],
                    on_delete: RefAction::Cascade,
                    on_update: RefAction::NoAction,
                },
            ],
        }
    }

    #[test]
    fn a_primary_key_unique_and_foreign_key_publish_the_three_views() {
        let (catalog, txn) = instance();
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let parent = catalog
            .create_table(&handle, &parent_def("master", "p035"))
            .expect("create_table parent");
        let child = catalog
            .create_table(&handle, &child_def("master", "c035", "p035"))
            .expect("create_table child");
        txn.commit(handle).expect("commit");

        let names = real_constraint_names(&catalog, &[parent.clone(), child.clone()]);
        let (table_rows, key_rows, ref_rows) = with_index_store(&catalog, |index_store| {
            (
                table_constraint_rows(&[parent.clone(), child.clone()], index_store, &names)
                    .expect("table_constraint_rows"),
                key_column_usage_rows(&[parent.clone(), child.clone()], index_store, &names)
                    .expect("key_column_usage_rows"),
                referential_constraint_rows(&[parent.clone(), child.clone()], index_store, &names)
                    .expect("referential_constraint_rows"),
            )
        });

        let types: Vec<&Value> = table_rows
            .iter()
            .map(|row| &row.0[table_constraints_columns::CONSTRAINT_TYPE])
            .collect();
        assert_eq!(types.len(), 4, "{types:?}"); // parent PK, parent UQ, child PK, child FK
        assert!(types.contains(&&text(PRIMARY_KEY_TYPE)));
        assert!(types.contains(&&text(UNIQUE_TYPE)));
        assert!(types.contains(&&text(FOREIGN_KEY_TYPE)));

        let fk_key_rows: Vec<&Row> = key_rows
            .iter()
            .filter(|row| row.0[key_column_usage_columns::CONSTRAINT_NAME] == text("fk_c035"))
            .collect();
        assert_eq!(fk_key_rows.len(), 2, "{fk_key_rows:?}");
        assert_eq!(
            fk_key_rows[0].0[key_column_usage_columns::COLUMN_NAME],
            text("fa")
        );
        assert_eq!(
            fk_key_rows[0].0[key_column_usage_columns::ORDINAL_POSITION],
            Value::I32(1)
        );
        assert_eq!(
            fk_key_rows[1].0[key_column_usage_columns::COLUMN_NAME],
            text("fb")
        );
        assert_eq!(
            fk_key_rows[1].0[key_column_usage_columns::ORDINAL_POSITION],
            Value::I32(2)
        );

        assert_eq!(ref_rows.len(), 1, "{ref_rows:?}");
        let referential = &ref_rows[0].0;
        assert_eq!(
            referential[referential_constraints_columns::CONSTRAINT_NAME],
            text("fk_c035")
        );
        assert_eq!(
            referential[referential_constraints_columns::UNIQUE_CONSTRAINT_NAME],
            text("pk_p035")
        );
        assert_eq!(
            referential[referential_constraints_columns::UPDATE_RULE],
            text("NO ACTION")
        );
        assert_eq!(
            referential[referential_constraints_columns::DELETE_RULE],
            text("CASCADE")
        );
    }

    #[test]
    fn a_check_or_a_default_publish_table_constraints_alone() {
        let (catalog, txn) = instance();
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let mut def = parent_def("master", "p035b");
        def.name.name = "t035c".to_owned();
        def.constraints = vec![ConstraintDef::Check {
            name: Some("ck_t035c".to_owned()),
            expr: integer("1"),
        }];
        let table = catalog.create_table(&handle, &def).expect("create_table");
        txn.commit(handle).expect("commit");

        let names = real_constraint_names(&catalog, std::slice::from_ref(&table));
        let (table_rows, key_rows) = with_index_store(&catalog, |index_store| {
            (
                table_constraint_rows(std::slice::from_ref(&table), index_store, &names)
                    .expect("table_constraint_rows"),
                key_column_usage_rows(std::slice::from_ref(&table), index_store, &names)
                    .expect("key_column_usage_rows"),
            )
        });
        assert_eq!(table_rows.len(), 1, "{table_rows:?}");
        assert_eq!(
            table_rows[0].0[table_constraints_columns::CONSTRAINT_TYPE],
            text(CHECK_TYPE)
        );
        assert_eq!(
            table_rows[0].0[table_constraints_columns::CONSTRAINT_NAME],
            text("ck_t035c")
        );
        assert!(key_rows.is_empty(), "{key_rows:?}");
    }

    #[test]
    fn an_unnamed_foreign_key_falls_back_on_the_generated_name() {
        let (catalog, txn) = instance();
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        catalog
            .create_table(&handle, &parent_def("master", "p035u"))
            .expect("create_table parent");
        let mut def = child_def("master", "c035u", "p035u");
        let ConstraintDef::ForeignKey { name, .. } = &mut def.constraints[1] else {
            panic!("the second constraint is the foreign key");
        };
        name.take();
        let child = catalog.create_table(&handle, &def).expect("create_table");
        txn.commit(handle).expect("commit");

        let names = BTreeMap::new();
        let table_rows = with_index_store(&catalog, |index_store| {
            table_constraint_rows(&[child], index_store, &names).expect("table_constraint_rows")
        });
        let fk_row = table_rows
            .iter()
            .find(|row| row.0[table_constraints_columns::CONSTRAINT_TYPE] == text(FOREIGN_KEY_TYPE))
            .expect("a FOREIGN KEY row");
        let name = &fk_row.0[table_constraints_columns::CONSTRAINT_NAME];
        assert!(
            matches!(name, Value::String(s) if s.text.starts_with("FK__c035u")),
            "{name:?}"
        );
    }

    #[test]
    fn column_named_rejects_an_unknown_column() {
        let table = TableMeta {
            id: ObjectId(1),
            storage_id: TableId(1),
            database: DbId(1),
            schema: "dbo".to_owned(),
            name: "t".to_owned(),
            columns: Vec::new(),
            clustered: None,
            constraints: Vec::new(),
        };
        let err = column_named(&table, ColumnId(1)).expect_err("no such column");
        assert!(err.message.contains("does not hold"), "{}", err.message);
    }

    #[test]
    fn indexed_meta_rejects_an_unknown_index() {
        let (catalog, txn) = instance();
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let table = catalog
            .create_table(&handle, &parent_def("master", "p035i"))
            .expect("create_table");
        txn.commit(handle).expect("commit");
        let err = with_index_store(&catalog, |index_store| {
            indexed_meta(&table, index_store, IndexId(999_999)).expect_err("no such index")
        });
        assert!(err.message.contains("does not hold"), "{}", err.message);
    }

    #[test]
    fn a_table_of_a_database_beyond_an_int_is_a_bug() {
        let err = database_id(DbId(u32::MAX)).expect_err("2^32 - 1 does not fit in an int");
        assert!(
            err.message.contains("does not fit in an int"),
            "{}",
            err.message
        );
    }
}
