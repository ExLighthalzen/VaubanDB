//! `sys.foreign_keys`, `sys.foreign_key_columns`, `sys.check_constraints` and
//! `sys.default_constraints`.
//!
//! # The shape of the views
//!
//! The column list of each of the four views — names, order, types — is the one SQL Server
//! 2022 publishes: 22, 6, 19 and 15 columns. The unit tests below freeze the four vectors of
//! names and compare them with the text this file builds
//! (`sys_foreign_keys_columns_match_the_published_list` and its three neighbours).
//!
//! # Four internal tables, one per view
//!
//! A view is a `SELECT` over one denormalised internal table, without a join. The four views
//! carry four sets of rows — one per foreign key, one per column of a foreign key, one per
//! `CHECK`, one per `DEFAULT` — so each gets its table: [`FOREIGN_KEYS_TABLE`],
//! [`FOREIGN_KEY_COLUMNS_TABLE`], [`CHECK_CONSTRAINTS_TABLE`] and
//! [`DEFAULT_CONSTRAINTS_TABLE`]. The internal tables live in `master` while the four views
//! are per-database, so each definition filters on `database_id = DB_ID()`, as `sys.schemas`
//! (`views/sys_core.rs`) and `sys.objects` (`views/sys_tables.rs`) do. The two constraint
//! tables hold the same eight columns: what separates their views is the literals they
//! write, `C ` against `D ` (unit test `the_two_constraint_tables_share_their_shape`).
//!
//! # Read columns and literal columns
//!
//! The four tables hold 11, 7, 8 and 8 columns, of which the views read 10, 6, 7 and 7 —
//! `database_id` is the filter, not a published column. The other items of a select list are
//! literals, under the two rules of `views/sys_core.rs`: a value the catalogue does not store
//! is `CAST(NULL AS …)`, a value SQL Server publishes the same way on each row is written as
//! that constant. The unit test
//! `the_columns_written_null_are_the_ones_with_no_datum_behind_them` names the `NULL`s and
//! counts them, `the_literal_columns_are_the_published_values` compares each literal with
//! the row it comes from.
//!
//! `type` and `type_desc` are literals here, where `sys.objects` reads them: each of the
//! three views that publishes them serves one kind of constraint, `F `, `C ` or `D `.
//! `create_date` and `modify_date` are `CAST(NULL AS datetime)`, as in
//! `views/sys_tables.rs`: no `*Meta` of the catalogue carries an instant. `key_index_id` is
//! `CAST(NULL AS int)` in the view text, where SQL Server answers the `index_id` of the index
//! that carries the key pointed at (`1` for a `PRIMARY KEY`).
//!
//! # What the catalogue knows of these three kinds of constraint
//!
//! `constraints.rs` stores them: the object of a constraint carries its identifier and its
//! name, and the [`ConstraintMeta`] carries the identifier, the flag `is_system_named`
//! publishes and, for a `CHECK`, the text of its definition. The four builders read those
//! fields, and take the objects beside the tables to write the name a statement wrote (unit
//! tests `foreign_key_row_when_tabledef_has_one`, `check_constraint_row_stores_the_expression`,
//! `a_row_falls_back_on_the_generated_name_without_its_object`).
//!
//! Two values the rows still leave bounded:
//!
//! - the `object_id` of a `DEFAULT` read from [`ColumnMeta::default`] alone is
//!   [`NO_OBJECT_ID`], `0`: a [`TableMeta`] `create_table` built carries a
//!   [`ConstraintMeta::Default`] per column default, one built by hand does not (unit test
//!   `a_column_default_without_its_constraint_has_no_object_id`);
//! - `key_index_id` stays `CAST(NULL AS int)` in the view text. [`ConstraintMeta::ForeignKey`]
//!   holds that index as an `IndexId`, but the number a client reads is the per-table
//!   `index_id` of `views/sys_indexes.rs`; publishing it asks for a column in
//!   [`FOREIGN_KEYS_TABLE`] and for that numbering.
//!
//! `parent_column_id` of a `CHECK` is `0`, the value of a table-level check. SQL Server
//! resolves a check whose predicate names **one** column to that column: on `(a, b, c)`,
//! `CHECK (b > 0)` answers 2, `CHECK (c <> 'z')` answers 3, `CHECK (a < b)` answers 0 and
//! `CHECK (1 = 1)` answers 0. [`ConstraintMeta::Check`] holds its predicate without a
//! column, and that resolution is not done; deliberate difference for now.
//!
//! # Execution
//!
//! The rows of the four functions below are not written into `storage`: the list
//! `sys_rows::rows_of` walks holds twelve internal tables and not these four
//! (`tests/sys_constraints.rs`,
//! `a_created_table_with_a_default_leaves_the_internal_tables_untouched_for_now`). What this
//! file produces is the shape of the four internal tables, the text of the four definitions
//! and the four functions that turn a `*Meta` into rows.

use vauban_errors::{InternalError, SqlResult};
use vauban_parser::{Expr, RefAction};
use vauban_storage::{DbId, Row};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{SYSTEM_DATABASES, SYSTEM_SCHEMAS};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::ids::ColumnId;
use crate::ids::ObjectId;
use crate::meta::{ColumnMeta, ConstraintMeta, ObjectMeta, QualifiedName, TableMeta};

/// Internal table of the foreign keys, read by `sys.foreign_keys`.
///
/// A `vauban_sys_*` name of our own; what a client reads is the view built over it.
pub(crate) const FOREIGN_KEYS_TABLE: &str = "vauban_sys_foreign_keys";

/// Internal table of the columns of the foreign keys, read by `sys.foreign_key_columns`.
pub(crate) const FOREIGN_KEY_COLUMNS_TABLE: &str = "vauban_sys_foreign_key_columns";

/// Internal table of the `CHECK` constraints, read by `sys.check_constraints`.
pub(crate) const CHECK_CONSTRAINTS_TABLE: &str = "vauban_sys_check_constraints";

/// Internal table of the `DEFAULT` constraints, read by `sys.default_constraints`.
pub(crate) const DEFAULT_CONSTRAINTS_TABLE: &str = "vauban_sys_default_constraints";

/// The schema the four views of this file live in.
const VIEW_SCHEMA: &str = "sys";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// The `object_id` written for a constraint no object of the catalogue describes.
///
/// `constraints.rs` gives each `FOREIGN KEY`, `CHECK` and `DEFAULT` of a `CREATE TABLE` an
/// object of its own, whose identifier the [`ConstraintMeta`] carries and the rows below
/// publish.
/// What is left at `0` is the `DEFAULT` of a [`ColumnMeta`] that no [`ConstraintMeta`]
/// covers — a [`TableMeta`] built by hand rather than by `create_table` — and `0` is below
/// the range `table::FIRST_USER_OBJECT_ID` opens (unit test
/// `a_column_default_without_its_constraint_has_no_object_id`).
const NO_OBJECT_ID: i32 = 0;

/// `parent_column_id` of a constraint that belongs to the table rather than to one of its
/// columns, which is what a [`ConstraintMeta::Check`] gives here. See the module
/// documentation.
const TABLE_LEVEL_COLUMN: i32 = 0;

/// The `schema_id` written for a schema the bootstrap does not know, as
/// `views/sys_tables.rs` writes it: `0` is not one of the three identifiers of
/// [`SYSTEM_SCHEMAS`].
const UNKNOWN_SCHEMA_ID: i32 = 0;

/// Where each column of [`FOREIGN_KEYS_TABLE`] sits in a [`Row`], as `bootstrap.rs` and
/// `views/sys_tables.rs` do for their own tables: a writer of a row addresses it
/// through these constants rather than restating the order (unit test
/// `the_column_order_is_the_one_the_constants_name`).
pub(crate) mod foreign_keys_columns {
    /// `database_id int`: the database the constraint belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the identifier of the constraint as an object.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the constraint.
    pub(crate) const NAME: usize = 2;
    /// `schema_id int`: the schema of the table the constraint belongs to.
    pub(crate) const SCHEMA_ID: usize = 3;
    /// `parent_object_id int`: the constrained table.
    pub(crate) const PARENT_OBJECT_ID: usize = 4;
    /// `referenced_object_id int`: the table pointed at.
    pub(crate) const REFERENCED_OBJECT_ID: usize = 5;
    /// `delete_referential_action tinyint`: the code of the `ON DELETE` action.
    pub(crate) const DELETE_REFERENTIAL_ACTION: usize = 6;
    /// `delete_referential_action_desc nvarchar(60)`.
    pub(crate) const DELETE_REFERENTIAL_ACTION_DESC: usize = 7;
    /// `update_referential_action tinyint`: the code of the `ON UPDATE` action.
    pub(crate) const UPDATE_REFERENTIAL_ACTION: usize = 8;
    /// `update_referential_action_desc nvarchar(60)`.
    pub(crate) const UPDATE_REFERENTIAL_ACTION_DESC: usize = 9;
    /// `is_system_named bit`: `1` for a constraint no statement named.
    pub(crate) const IS_SYSTEM_NAMED: usize = 10;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 11;
}

/// Where each column of [`FOREIGN_KEY_COLUMNS_TABLE`] sits in a [`Row`]. Same rule as
/// [`foreign_keys_columns`].
pub(crate) mod foreign_key_columns_columns {
    /// `database_id int`: the database the constraint belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `constraint_object_id int`: the foreign key this column belongs to.
    pub(crate) const CONSTRAINT_OBJECT_ID: usize = 1;
    /// `constraint_column_id int`: the position of the column in the key, from `1`.
    pub(crate) const CONSTRAINT_COLUMN_ID: usize = 2;
    /// `parent_object_id int`: the constrained table.
    pub(crate) const PARENT_OBJECT_ID: usize = 3;
    /// `parent_column_id int`: the constrained column.
    pub(crate) const PARENT_COLUMN_ID: usize = 4;
    /// `referenced_object_id int`: the table pointed at.
    pub(crate) const REFERENCED_OBJECT_ID: usize = 5;
    /// `referenced_column_id int`: the column pointed at.
    pub(crate) const REFERENCED_COLUMN_ID: usize = 6;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 7;
}

/// Where each column of [`CHECK_CONSTRAINTS_TABLE`] and of [`DEFAULT_CONSTRAINTS_TABLE`]
/// sits in a [`Row`]: the two tables share their shape (module documentation). Same rule as
/// [`foreign_keys_columns`].
pub(crate) mod constraint_columns {
    /// `database_id int`: the database the constraint belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the identifier of the constraint as an object.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `name nvarchar(128)`: the name of the constraint.
    pub(crate) const NAME: usize = 2;
    /// `schema_id int`: the schema of the table the constraint belongs to.
    pub(crate) const SCHEMA_ID: usize = 3;
    /// `parent_object_id int`: the table the constraint belongs to.
    pub(crate) const PARENT_OBJECT_ID: usize = 4;
    /// `parent_column_id int`: the column it belongs to, `0` for a table-level constraint.
    pub(crate) const PARENT_COLUMN_ID: usize = 5;
    /// `definition nvarchar(max)`: the text of the predicate or of the default.
    pub(crate) const DEFINITION: usize = 6;
    /// `is_system_named bit`: `1` for a constraint no statement named.
    pub(crate) const IS_SYSTEM_NAMED: usize = 7;
    /// Width of a row of either table.
    pub(crate) const WIDTH: usize = 8;
}

/// The select list of `sys.foreign_keys`: `(column, expression)`, in the published order.
///
/// An expression equal to the column name reads [`FOREIGN_KEYS_TABLE`]; the 12 others are
/// the literals the module documentation explains. The unit test
/// `the_literal_columns_are_the_published_values` compares 5 of them (`type`, `type_desc`,
/// `is_disabled`, `is_not_for_replication`, `is_not_trusted`) with the row of a foreign key.
const FOREIGN_KEYS_VIEW: [(&str, &str); 22] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "CAST('F ' AS char(2))"),
    (
        "type_desc",
        "CAST(N'FOREIGN_KEY_CONSTRAINT' AS nvarchar(60))",
    ),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("is_ms_shipped", "CAST(0 AS bit)"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
    ("referenced_object_id", "referenced_object_id"),
    ("key_index_id", "CAST(NULL AS int)"),
    ("is_disabled", "CAST(0 AS bit)"),
    ("is_not_for_replication", "CAST(0 AS bit)"),
    ("is_not_trusted", "CAST(0 AS bit)"),
    ("delete_referential_action", "delete_referential_action"),
    (
        "delete_referential_action_desc",
        "delete_referential_action_desc",
    ),
    ("update_referential_action", "update_referential_action"),
    (
        "update_referential_action_desc",
        "update_referential_action_desc",
    ),
    ("is_system_named", "is_system_named"),
];

/// The select list of `sys.foreign_key_columns`: its 6 columns, each read from
/// [`FOREIGN_KEY_COLUMNS_TABLE`]. The list of `the_columns_written_null_are_the_ones_with_no_datum_behind_them`
/// holds no item of this view, and `sys_foreign_key_columns_columns_match_the_published_list`
/// compares the 6 names with the published ones.
const FOREIGN_KEY_COLUMNS_VIEW: [(&str, &str); 6] = [
    ("constraint_object_id", "constraint_object_id"),
    ("constraint_column_id", "constraint_column_id"),
    ("parent_object_id", "parent_object_id"),
    ("parent_column_id", "parent_column_id"),
    ("referenced_object_id", "referenced_object_id"),
    ("referenced_column_id", "referenced_column_id"),
];

/// The select list of `sys.check_constraints`: its 19 columns, 7 of which read
/// [`CHECK_CONSTRAINTS_TABLE`].
///
/// Of the 12 literals, the unit test `the_literal_columns_are_the_published_values` compares
/// 6 (`type`, `type_desc`, `uses_database_collation`, `is_disabled`,
/// `is_not_for_replication`, `is_not_trusted`) with the row of a `CHECK`.
const CHECK_CONSTRAINTS_VIEW: [(&str, &str); 19] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "CAST('C ' AS char(2))"),
    ("type_desc", "CAST(N'CHECK_CONSTRAINT' AS nvarchar(60))"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("is_ms_shipped", "CAST(0 AS bit)"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
    ("is_disabled", "CAST(0 AS bit)"),
    ("is_not_for_replication", "CAST(0 AS bit)"),
    ("is_not_trusted", "CAST(0 AS bit)"),
    ("parent_column_id", "parent_column_id"),
    ("definition", "definition"),
    ("uses_database_collation", "CAST(1 AS bit)"),
    ("is_system_named", "is_system_named"),
];

/// The select list of `sys.default_constraints`: its 15 columns, 7 of which read
/// [`DEFAULT_CONSTRAINTS_TABLE`].
///
/// Of the 8 literals, the unit test `the_literal_columns_are_the_published_values` compares
/// 3 (`type`, `type_desc`, `is_ms_shipped`) with the row of a `DEFAULT`.
const DEFAULT_CONSTRAINTS_VIEW: [(&str, &str); 15] = [
    ("name", "name"),
    ("object_id", "object_id"),
    ("principal_id", "CAST(NULL AS int)"),
    ("schema_id", "schema_id"),
    ("parent_object_id", "parent_object_id"),
    ("type", "CAST('D ' AS char(2))"),
    ("type_desc", "CAST(N'DEFAULT_CONSTRAINT' AS nvarchar(60))"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("is_ms_shipped", "CAST(0 AS bit)"),
    ("is_published", "CAST(0 AS bit)"),
    ("is_schema_published", "CAST(0 AS bit)"),
    ("parent_column_id", "parent_column_id"),
    ("definition", "definition"),
    ("is_system_named", "is_system_named"),
];

/// The internal tables this file describes: one per view.
///
/// The bootstrap creates them in `master`. Their `rows` are empty: the rows of a constraint
/// are built from the `*Meta` of the catalogue by the four functions below (module
/// documentation).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: FOREIGN_KEYS_TABLE.to_owned(),
            columns: foreign_keys_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "foreign_keys",
                &definition(&FOREIGN_KEYS_VIEW, FOREIGN_KEYS_TABLE),
            ),
        },
        InternalTableDef {
            name: FOREIGN_KEY_COLUMNS_TABLE.to_owned(),
            columns: foreign_key_columns_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "foreign_key_columns",
                &definition(&FOREIGN_KEY_COLUMNS_VIEW, FOREIGN_KEY_COLUMNS_TABLE),
            ),
        },
        InternalTableDef {
            name: CHECK_CONSTRAINTS_TABLE.to_owned(),
            columns: constraint_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "check_constraints",
                &definition(&CHECK_CONSTRAINTS_VIEW, CHECK_CONSTRAINTS_TABLE),
            ),
        },
        InternalTableDef {
            name: DEFAULT_CONSTRAINTS_TABLE.to_owned(),
            columns: constraint_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: views_of(
                "default_constraints",
                &definition(&DEFAULT_CONSTRAINTS_VIEW, DEFAULT_CONSTRAINTS_TABLE),
            ),
        },
    ]
}

/// The rows of [`FOREIGN_KEYS_TABLE`] for `tables`: one per [`ConstraintMeta::ForeignKey`].
///
/// The caller passes the tables of one catalogue and gets the rows in that order, the
/// constraints of a table in declaration order (unit test
/// `foreign_key_row_when_tabledef_has_one`). A table that carries no foreign key adds
/// nothing.
///
/// # Errors
///
/// [`InternalError::Bug`] when a [`DbId`] does not fit in the `int` the view publishes,
/// which is the check `bootstrap.rs` makes on the same value.
pub(crate) fn foreign_key_rows(
    tables: &[TableMeta],
    objects: &[ObjectMeta],
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for (position, constraint) in table.constraints.iter().enumerate() {
            let ConstraintMeta::ForeignKey {
                constraint: object,
                system_named,
                columns,
                referenced_table,
                on_delete,
                on_update,
                ..
            } = constraint
            else {
                continue;
            };
            let generated =
                generated_constraint_name("FK", table, single_column(table, columns), position);
            let name = constraint_name(objects, *object).unwrap_or(&generated);
            rows.push(Row(vec![
                Value::I32(database),
                Value::I32(object.0),
                text(name),
                Value::I32(schema_id(&table.schema)),
                Value::I32(table.id.0),
                Value::I32(referenced_table.0),
                Value::I8(referential_action_code(*on_delete)),
                text(referential_action_desc(*on_delete)),
                Value::I8(referential_action_code(*on_update)),
                text(referential_action_desc(*on_update)),
                Value::Bit(*system_named),
            ]));
        }
    }
    Ok(rows)
}

/// The rows of [`FOREIGN_KEY_COLUMNS_TABLE`] for `tables`: one per column of a
/// [`ConstraintMeta::ForeignKey`], in key order.
///
/// `constraint_column_id` numbers the columns of one key from `1` (positions 1 and 2 over
/// the columns 2 and 3 of a child table). A key whose `referenced_columns` is shorter than
/// its `columns` stops on the
/// shorter of the two, the two lists being parallel by construction
/// ([`ConstraintMeta::ForeignKey`]).
///
/// # Errors
///
/// Those of [`foreign_key_rows`].
pub(crate) fn foreign_key_column_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for constraint in &table.constraints {
            let ConstraintMeta::ForeignKey {
                constraint: object,
                columns,
                referenced_table,
                referenced_columns,
                ..
            } = constraint
            else {
                continue;
            };
            for (position, (column, referenced)) in
                columns.iter().zip(referenced_columns.iter()).enumerate()
            {
                rows.push(Row(vec![
                    Value::I32(database),
                    Value::I32(object.0),
                    Value::I32(key_position(position)),
                    Value::I32(table.id.0),
                    Value::I32(column.0),
                    Value::I32(referenced_table.0),
                    Value::I32(referenced.0),
                ]));
            }
        }
    }
    Ok(rows)
}

/// The rows of [`CHECK_CONSTRAINTS_TABLE`] for `tables`: one per [`ConstraintMeta::Check`].
///
/// `definition` is the predicate the constraint was given, written by the `Display` of
/// [`Expr`] between parentheses (unit test `check_constraint_row_stores_the_expression`).
/// `parent_column_id` is [`TABLE_LEVEL_COLUMN`]: see the module documentation.
///
/// # Errors
///
/// Those of [`foreign_key_rows`].
pub(crate) fn check_constraint_rows(
    tables: &[TableMeta],
    objects: &[ObjectMeta],
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for (position, constraint) in table.constraints.iter().enumerate() {
            let ConstraintMeta::Check {
                constraint: object,
                system_named,
                definition,
                ..
            } = constraint
            else {
                continue;
            };
            let generated = generated_constraint_name("CK", table, None, position);
            rows.push(constraint_row(
                database,
                table,
                *object,
                constraint_name(objects, *object).unwrap_or(&generated),
                TABLE_LEVEL_COLUMN,
                definition,
                *system_named,
            ));
        }
    }
    Ok(rows)
}

/// The rows of [`DEFAULT_CONSTRAINTS_TABLE`] for `tables`: one per column that carries a
/// default.
///
/// The source is [`ColumnMeta::default`], which `create_table` fills from the `DEFAULT`
/// clause of a column (unit test `default_constraint_row_for_column_default`), then the
/// [`ConstraintMeta::Default`] constraints whose column the first pass did not cover — T-SQL
/// allows one default per column, and the catalogue would otherwise hold the same expression
/// twice (unit test `a_default_is_published_once_per_column`). `parent_column_id` is the
/// `column_id` of the column, its 1-based position (`parent_column_id` 2 for the second
/// column of a table).
///
/// # Errors
///
/// Those of [`foreign_key_rows`].
pub(crate) fn default_constraint_rows(
    tables: &[TableMeta],
    objects: &[ObjectMeta],
) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        let mut published: Vec<ColumnId> = Vec::new();
        for (position, constraint) in table.constraints.iter().enumerate() {
            let ConstraintMeta::Default {
                constraint: object,
                system_named,
                column,
                expr,
            } = constraint
            else {
                continue;
            };
            published.push(*column);
            let generated =
                generated_constraint_name("DF", table, column_name(table, *column), position);
            rows.push(constraint_row(
                database,
                table,
                *object,
                constraint_name(objects, *object).unwrap_or(&generated),
                column.0,
                &definition_text(expr),
                *system_named,
            ));
        }
        for (position, column) in table.columns.iter().enumerate() {
            let Some(expr) = &column.default else {
                continue;
            };
            if published.contains(&column.id) {
                continue;
            }
            published.push(column.id);
            rows.push(constraint_row(
                database,
                table,
                ObjectId(NO_OBJECT_ID),
                &generated_constraint_name(
                    "DF",
                    table,
                    Some(&column.name),
                    table.constraints.len() + position,
                ),
                column.id.0,
                &definition_text(expr),
                true,
            ));
        }
    }
    Ok(rows)
}

/// One row of [`CHECK_CONSTRAINTS_TABLE`] or of [`DEFAULT_CONSTRAINTS_TABLE`], the two
/// tables sharing their shape (module documentation).
fn constraint_row(
    database: i32,
    table: &TableMeta,
    object: ObjectId,
    name: &str,
    parent_column_id: i32,
    definition: &str,
    system_named: bool,
) -> Row {
    Row(vec![
        Value::I32(database),
        Value::I32(object.0),
        text(name),
        Value::I32(schema_id(&table.schema)),
        Value::I32(table.id.0),
        Value::I32(parent_column_id),
        text(definition),
        Value::Bit(system_named),
    ])
}

/// The name of the constraint object `id` among `objects`, `None` when the slice holds no
/// object of that identifier — a [`TableMeta`] read without the store that carries its
/// constraints (unit test `a_row_falls_back_on_the_generated_name_without_its_object`).
fn constraint_name(objects: &[ObjectMeta], id: ObjectId) -> Option<&str> {
    objects
        .iter()
        .find(|object| object.id == id)
        .map(|object| object.name.name.as_str())
}

/// The name of the single column a constraint holds, `None` when it holds another number of
/// them.
///
/// A `FOREIGN KEY` over two columns is named without a column group —
/// `FK__tbl014_cc__38996AB5` for `FOREIGN KEY (first_col, second_col)`, where a one-column
/// key gives `FK__ab__a__38996AB5` (`constraints.rs`, module documentation).
fn single_column<'a>(table: &'a TableMeta, columns: &[ColumnId]) -> Option<&'a str> {
    match columns {
        [only] => column_name(table, *only),
        _ => None,
    }
}

/// The text `definition` publishes for `expr`: what `Display` writes, between parentheses.
///
/// SQL Server normalises the text it stores — `CHECK (amount >= 0)` comes back as
/// `([amount]>=(0))` and `DEFAULT 'x'` as `('x')` — where this writes the expression the
/// caller gave, re-serialised by `vauban-parser` (unit test
/// `check_constraint_row_stores_the_expression`). The outer parentheses are the part of that
/// normalisation this file keeps; the brackets around an identifier and the parentheses
/// around a literal are a deliberate difference for now.
fn definition_text(expr: &Expr) -> String {
    format!("({expr})")
}

/// The code `delete_referential_action` and `update_referential_action` publish.
///
/// `NO ACTION` 0, `CASCADE` 1, `SET NULL` 2, `SET DEFAULT` 3; a clause left out is
/// `NO ACTION`. Frozen by the unit test `the_referential_action_codes_are_the_published_ones`.
fn referential_action_code(action: RefAction) -> u8 {
    match action {
        RefAction::NoAction => 0,
        RefAction::Cascade => 1,
        RefAction::SetNull => 2,
        RefAction::SetDefault => 3,
    }
}

/// The `*_desc` that goes with [`referential_action_code`].
fn referential_action_desc(action: RefAction) -> &'static str {
    match action {
        RefAction::NoAction => "NO_ACTION",
        RefAction::Cascade => "CASCADE",
        RefAction::SetNull => "SET_NULL",
        RefAction::SetDefault => "SET_DEFAULT",
    }
}

/// How many characters the table head and the column head of a generated name hold together
/// when the constraint belongs to a column.
///
/// See [`generated_constraint_name`]: the 6 names of the table there with a column are 26 or
/// 30 characters long, and their two heads hold 14 together.
const NAME_BUDGET_WITH_COLUMN: usize = 14;

/// How many characters the table head of a generated name holds when the constraint belongs
/// to the table rather than to a column.
///
/// See [`generated_constraint_name`]: the 7 table-level names of the table there grow with
/// the table name up to 16 characters of head — 26, 27, 28, 29, 30, 30 and 30 characters over
/// tables of 12, 13, 14, 15, 16, 17 and 38 — which [`NAME_BUDGET_WITH_COLUMN`] would cut two
/// characters shorter (unit test `a_generated_name_follows_the_published_shape`).
const NAME_BUDGET_WITHOUT_COLUMN: usize = 16;

/// How many of the [`NAME_BUDGET_WITH_COLUMN`] characters the column head is sure to get.
///
/// See [`generated_constraint_name`]: `tbl011_gen` is cut to 9 so that `amount` keeps 5,
/// while `tbl011_c`, which is shorter, leaves 6 to `parent_id`.
const COLUMN_FLOOR: usize = 5;

/// The name of a constraint this file publishes, [`ConstraintMeta`] having no field for one
/// (module documentation, unit test `a_generated_name_is_flagged_as_system_named`).
///
/// The shape is the one SQL Server generates: the two letters of the kind, two underscores, the head
/// of the table name, two underscores, the head of the column name when the constraint
/// belongs to one, two underscores, eight upper-case hexadecimal digits. A constraint that
/// belongs to a column holds [`NAME_BUDGET_WITH_COLUMN`] characters of head, the column
/// keeping [`COLUMN_FLOOR`] of them when the table name is long enough to take the rest; a
/// constraint that belongs to the table holds [`NAME_BUDGET_WITHOUT_COLUMN`] characters of
/// table name, two more than the other branch, which is what the second half of the table
/// below shows. The 14 names of the table are 26 to 30 characters long:
///
/// | Declaration | Generated name |
/// |---|---|
/// | `dbo.tbl011_gen (… amount int NULL CHECK (amount >= 0) …)` | `CK__tbl011_ge__amoun__37A5467C` |
/// | `dbo.tbl011_gen (… label varchar(30) NULL DEFAULT 'x' …)` | `DF__tbl011_ge__label__38996AB5` |
/// | `dbo.tbl011_c (… parent_id int NOT NULL REFERENCES dbo.tbl011_p (id))` | `FK__tbl011_c__parent__398D8EEE` |
/// | `dbo.abc (xyzabcdefghijklmnopqrst int NULL CHECK (xyzabcdefghijklmnopqrst > 0))` | `CK__abc__xyzabcdefgh__36B12243` |
/// | `dbo.tbl011_longer_name (ab int NULL CHECK (ab > 0))` | `CK__tbl011_longe__ab__38996AB5` |
/// | `dbo.tbl011_x (zz int NULL DEFAULT (0))` | `DF__tbl011_x__zz__3A81B327` |
/// | `dbo.tbl011_nocol (a int NULL, b int NULL, CHECK (a < b))` | `CK__tbl011_nocol__3C69FB99` |
/// | `dbo.t012_abcdefg (…, CHECK (a < b))` | `CK__t012_abcdefg__36B12243` |
/// | `dbo.t013_abcdefgh (…, CHECK (a < b))` | `CK__t013_abcdefgh__38996AB5` |
/// | `dbo.t014_abcdefghi (…, CHECK (a < b))` | `CK__t014_abcdefghi__3A81B327` |
/// | `dbo.t015_abcdefghij (…, CHECK (a < b))` | `CK__t015_abcdefghij__3C69FB99` |
/// | `dbo.t016_abcdefghijk (…, CHECK (a < b))` | `CK__t016_abcdefghijk__3E52440B` |
/// | `dbo.t017_abcdefghijkl (…, CHECK (a < b))` | `CK__t017_abcdefghijk__403A8C7D` |
/// | `dbo.t038_abcdefghijklmnopqrstuvwxyzabcdefg (…, CHECK (a < b))` | `CK__t038_abcdefghijk__4222D4EF` |
///
/// The eight digits are ours, a hash of the identifier of the table, of the position of the
/// constraint, of the kind and of the name of the table, so that two constraints of one table
/// take two names and a name does not move between two runs (unit tests
/// `a_generated_name_follows_the_published_shape`,
/// `two_constraints_of_one_table_take_two_names`); SQL Server draws them from the identifier
/// it gave the constraint. This follows `index.rs`, which generates the name of an unnamed
/// `PRIMARY KEY` the same way.
pub(crate) fn generated_constraint_name(
    kind: &str,
    table: &TableMeta,
    column: Option<&str>,
    position: usize,
) -> String {
    let table_budget = match column {
        Some(column) => NAME_BUDGET_WITH_COLUMN - column.chars().count().min(COLUMN_FLOOR),
        None => NAME_BUDGET_WITHOUT_COLUMN,
    };
    let head: String = table.name.chars().take(table_budget).collect();
    let tail: String = match column {
        Some(column) => {
            let kept: String = column
                .chars()
                .take(NAME_BUDGET_WITH_COLUMN - head.chars().count())
                .collect();
            format!("__{kept}")
        }
        None => String::new(),
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in table
        .id
        .0
        .to_be_bytes()
        .into_iter()
        .chain((position as u64).to_be_bytes())
        .chain(kind.as_bytes().iter().copied())
        .chain(table.name.as_bytes().iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let digits = ((hash >> 32) as u32) ^ (hash as u32);
    format!("{kind}__{head}{tail}__{digits:08X}")
}

/// The `constraint_column_id` of the column at `position` in its key: the position, from `1`.
///
/// A key of more columns than an `int` counts is not reachable — SQL Server refuses a
/// `CREATE TABLE` of more than 1024 columns with 1702 (`table.rs`) — so the value saturates
/// rather than carrying an error up the four row builders.
fn key_position(position: usize) -> i32 {
    i32::try_from(position.saturating_add(1)).unwrap_or(i32::MAX)
}

/// The name of the column of `table` whose identifier is `id`, `None` when the table holds
/// no such column.
fn column_name(table: &TableMeta, id: ColumnId) -> Option<&str> {
    table
        .columns
        .iter()
        .find(|column: &&ColumnMeta| column.id == id)
        .map(|column| column.name.as_str())
}

/// The `int` a [`DbId`] is published as, the four views being per-database while the internal
/// tables hold the databases side by side.
///
/// # Errors
///
/// [`InternalError::Bug`], as `bootstrap.rs` and `views/sys_tables.rs` do on the same value.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "sys.foreign_keys: database id {id} does not fit in an int"
        ))
        .into()
    })
}

/// The `schema_id` of the schema named `name`, [`UNKNOWN_SCHEMA_ID`] when the bootstrap
/// created no schema of that name.
///
/// Compared without regard to case, as `SQL_Latin1_General_CP1_CI_AS` compares identifiers;
/// same rule as `views/sys_tables.rs`, whose rustdoc explains the value.
fn schema_id(name: &str) -> i32 {
    SYSTEM_SCHEMAS
        .iter()
        .find(|(schema, _, _)| schema.eq_ignore_ascii_case(name))
        .map_or(UNKNOWN_SCHEMA_ID, |(_, id, _)| *id)
}

/// The columns of [`FOREIGN_KEYS_TABLE`], in the order of [`foreign_keys_columns`].
fn foreign_keys_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        column("name", SYSNAME, false),
        column("schema_id", SqlType::Int, false),
        column("parent_object_id", SqlType::Int, false),
        column("referenced_object_id", SqlType::Int, false),
        column("delete_referential_action", SqlType::TinyInt, false),
        column(
            "delete_referential_action_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
        column("update_referential_action", SqlType::TinyInt, false),
        column(
            "update_referential_action_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
        column("is_system_named", SqlType::Bit, false),
    ]
}

/// The columns of [`FOREIGN_KEY_COLUMNS_TABLE`], in the order of
/// [`foreign_key_columns_columns`].
fn foreign_key_columns_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("constraint_object_id", SqlType::Int, false),
        column("constraint_column_id", SqlType::Int, false),
        column("parent_object_id", SqlType::Int, false),
        column("parent_column_id", SqlType::Int, false),
        column("referenced_object_id", SqlType::Int, false),
        column("referenced_column_id", SqlType::Int, false),
    ]
}

/// The columns of [`CHECK_CONSTRAINTS_TABLE`] and of [`DEFAULT_CONSTRAINTS_TABLE`], in the
/// order of [`constraint_columns`].
fn constraint_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("object_id", SqlType::Int, false),
        column("name", SYSNAME, false),
        column("schema_id", SqlType::Int, false),
        column("parent_object_id", SqlType::Int, false),
        column("parent_column_id", SqlType::Int, false),
        column("definition", SqlType::NVarChar(Len::Max), false),
        column("is_system_named", SqlType::Bit, false),
    ]
}

/// The same view installed in the four system databases, `sys.<name>` in each of them.
///
/// The four views filter on `DB_ID()`, so the four definitions share their text (unit test
/// `the_views_are_installed_in_the_four_system_databases`). A database created by
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

/// The T-SQL text of a view: its select list, the internal table it reads in `master`, and
/// the filter that makes it per-database.
///
/// Same layout as `views/sys_core.rs` and `views/sys_tables.rs`: one select item per line, an
/// item whose expression is its own name written bare, the others as
/// `<expression> AS <name>`. The three files each hold their own copy of this helper. The 62
/// column names of the four views are written bare, where `sys.columns`
/// has to bracket its `precision` (unit test `each_definition_parses_as_one_select`, which
/// sends the four texts through `vauban_parser::parse_batch`).
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
        text.push_str(name);
    }
    text.push_str("\n  FROM master.dbo.");
    text.push_str(table);
    text.push_str("\n WHERE database_id = DB_ID()");
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
    use vauban_parser::{
        BinaryOp, ColumnRef, Ident, Literal, ParseOptions, Span, Statement, parse_batch,
    };
    use vauban_storage::{DbId, TableId};

    use crate::ids::ObjectId;

    use super::*;

    /// The ordered column names of `sys.foreign_keys` in SQL Server 2022. Frozen here so
    /// that a change of the text of the view has to face the list again.
    const PUBLISHED_FOREIGN_KEYS_COLUMNS: [&str; 22] = [
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
        "referenced_object_id",
        "key_index_id",
        "is_disabled",
        "is_not_for_replication",
        "is_not_trusted",
        "delete_referential_action",
        "delete_referential_action_desc",
        "update_referential_action",
        "update_referential_action_desc",
        "is_system_named",
    ];

    /// The ordered column names of `sys.foreign_key_columns`.
    const PUBLISHED_FOREIGN_KEY_COLUMNS_COLUMNS: [&str; 6] = [
        "constraint_object_id",
        "constraint_column_id",
        "parent_object_id",
        "parent_column_id",
        "referenced_object_id",
        "referenced_column_id",
    ];

    /// The ordered column names of `sys.check_constraints`.
    const PUBLISHED_CHECK_CONSTRAINTS_COLUMNS: [&str; 19] = [
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
        "is_disabled",
        "is_not_for_replication",
        "is_not_trusted",
        "parent_column_id",
        "definition",
        "uses_database_collation",
        "is_system_named",
    ];

    /// The ordered column names of `sys.default_constraints`.
    const PUBLISHED_DEFAULT_CONSTRAINTS_COLUMNS: [&str; 15] = [
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
        "parent_column_id",
        "definition",
        "is_system_named",
    ];

    /// The row `sys.foreign_keys` publishes for a foreign key declared `ON DELETE CASCADE`.
    const PUBLISHED_FOREIGN_KEY_ROW: [(&str, &str); 11] = [
        ("name", "fk_child"),
        ("type", "F "),
        ("type_desc", "FOREIGN_KEY_CONSTRAINT"),
        ("is_disabled", "0"),
        ("is_not_for_replication", "0"),
        ("is_not_trusted", "0"),
        ("delete_referential_action", "1"),
        ("delete_referential_action_desc", "CASCADE"),
        ("update_referential_action", "0"),
        ("update_referential_action_desc", "NO_ACTION"),
        ("is_system_named", "0"),
    ];

    /// The row `sys.check_constraints` publishes for `CHECK (amount >= 0)` on the second
    /// column of a table.
    const PUBLISHED_CHECK_ROW: [(&str, &str); 10] = [
        ("name", "ck_check_amount"),
        ("type", "C "),
        ("type_desc", "CHECK_CONSTRAINT"),
        ("parent_column_id", "2"),
        ("definition", "([amount]>=(0))"),
        ("uses_database_collation", "1"),
        ("is_disabled", "0"),
        ("is_not_for_replication", "0"),
        ("is_not_trusted", "0"),
        ("is_system_named", "0"),
    ];

    /// The row `sys.default_constraints` publishes for `DEFAULT 0` on the second column of a
    /// table.
    const PUBLISHED_DEFAULT_ROW: [(&str, &str); 7] = [
        ("name", "df_default_amount"),
        ("type", "D "),
        ("type_desc", "DEFAULT_CONSTRAINT"),
        ("parent_column_id", "2"),
        ("definition", "((0))"),
        ("is_system_named", "0"),
        ("is_ms_shipped", "0"),
    ];

    /// The names SQL Server generates for a constraint no statement named (table of
    /// [`generated_constraint_name`]): `(kind, table, column, name)`.
    ///
    /// The last 8 are table-level `CHECK` constraints, over tables of 12, 12, 13, 14, 15, 16,
    /// 17 and 38 characters: they are the vectors that tell [`NAME_BUDGET_WITHOUT_COLUMN`]
    /// from [`NAME_BUDGET_WITH_COLUMN`], the first six growing with the table name and the
    /// last two stopping at 16 characters of head.
    const GENERATED_NAMES: [(&str, &str, Option<&str>, &str); 14] = [
        (
            "CK",
            "tbl011_gen",
            Some("amount"),
            "CK__tbl011_ge__amoun__37A5467C",
        ),
        (
            "DF",
            "tbl011_gen",
            Some("label"),
            "DF__tbl011_ge__label__38996AB5",
        ),
        (
            "FK",
            "tbl011_c",
            Some("parent_id"),
            "FK__tbl011_c__parent__398D8EEE",
        ),
        (
            "CK",
            "abc",
            Some("xyzabcdefghijklmnopqrst"),
            "CK__abc__xyzabcdefgh__36B12243",
        ),
        (
            "CK",
            "tbl011_longer_name",
            Some("ab"),
            "CK__tbl011_longe__ab__38996AB5",
        ),
        ("DF", "tbl011_x", Some("zz"), "DF__tbl011_x__zz__3A81B327"),
        ("CK", "tbl011_nocol", None, "CK__tbl011_nocol__3C69FB99"),
        ("CK", "t012_abcdefg", None, "CK__t012_abcdefg__36B12243"),
        ("CK", "t013_abcdefgh", None, "CK__t013_abcdefgh__38996AB5"),
        ("CK", "t014_abcdefghi", None, "CK__t014_abcdefghi__3A81B327"),
        (
            "CK",
            "t015_abcdefghij",
            None,
            "CK__t015_abcdefghij__3C69FB99",
        ),
        (
            "CK",
            "t016_abcdefghijk",
            None,
            "CK__t016_abcdefghijk__3E52440B",
        ),
        (
            "CK",
            "t017_abcdefghijkl",
            None,
            "CK__t017_abcdefghijk__403A8C7D",
        ),
        (
            "CK",
            "t038_abcdefghijklmnopqrstuvwxyzabcdefg",
            None,
            "CK__t038_abcdefghijk__4222D4EF",
        ),
    ];

    /// A table of `dbo` with the columns named, in the database of identifier 1.
    fn table(name: &str, columns: &[&str]) -> TableMeta {
        TableMeta {
            id: ObjectId(1_000_000),
            storage_id: TableId(7),
            database: DbId(1),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
            columns: columns
                .iter()
                .enumerate()
                .map(|(position, column)| meta_column(position, column, None))
                .collect(),
            clustered: None,
            constraints: Vec::new(),
        }
    }

    /// A column of a [`TableMeta`], at `position` from `0`, with the default given.
    /// A constraint object of `table` called `name`, as `constraints.rs` stores it.
    fn constraint_object(id: i32, table: &TableMeta, name: &str) -> ObjectMeta {
        ObjectMeta {
            id: ObjectId(id),
            kind: crate::meta::ObjectKind::Constraint,
            name: QualifiedName {
                database: "master".to_owned(),
                schema: table.schema.clone(),
                name: name.to_owned(),
            },
            database: table.database,
            parent: Some(table.id),
            definition: None,
        }
    }

    fn meta_column(position: usize, name: &str, default: Option<Expr>) -> ColumnMeta {
        let ordinal = u16::try_from(position).expect("a test table is narrow");
        ColumnMeta {
            id: ColumnId(i32::from(ordinal) + 1),
            name: name.to_owned(),
            ty: TypeInfo::new(SqlType::Int, true),
            ordinal,
            default,
            identity: None,
            computed: None,
        }
    }

    /// The integer literal `value` as an [`Expr`].
    fn integer(value: &str) -> Expr {
        Expr::Literal(Literal::Integer(value.to_owned()), Span::EMPTY)
    }

    /// The definitions of the four views, by the name a client writes after `sys.`.
    fn definitions() -> Vec<(String, String)> {
        internal_tables()
            .into_iter()
            .flat_map(|table| table.views)
            .filter(|view| view.name.database == "master")
            .map(|view| (view.name.name, view.definition))
            .collect()
    }

    /// The definition of `sys.<name>`.
    fn definition_of(name: &str) -> String {
        definitions()
            .into_iter()
            .find(|(view, _)| view == name)
            .expect("the four views of this file are described")
            .1
    }

    /// The select items of the text of `sys.<name>`, as `definition` wrote them.
    fn select_items(name: &str) -> Vec<String> {
        let text = definition_of(name);
        let list = text
            .split_once("\n  FROM ")
            .expect("a definition reads a table")
            .0
            .trim_start_matches("SELECT ")
            .to_owned();
        list.split(",\n       ")
            .map(ToOwned::to_owned)
            .collect::<Vec<String>>()
    }

    /// The column names the text of `sys.<name>` publishes: the last word of each select
    /// item.
    fn published_columns(name: &str) -> Vec<String> {
        select_items(name)
            .into_iter()
            .map(|item| {
                item.rsplit(' ')
                    .next()
                    .expect("an item has at least one word")
                    .to_owned()
            })
            .collect()
    }

    /// The select item the text of `sys.<view>` writes for the column `column`.
    fn expression_of(view: &str, column: &str) -> String {
        select_items(view)
            .into_iter()
            .find(|item| item.rsplit(' ').next() == Some(column))
            .unwrap_or_else(|| panic!("sys.{view} publishes {column}"))
    }

    /// The value inside the `CAST(… AS …)` of the literal `sys.<view>` writes for `column`,
    /// `None` when the item reads the internal table instead.
    ///
    /// The `N` of an `nvarchar` literal and the quotes are stripped so that the result reads
    /// like a published value.
    fn bare_literal(view: &str, column: &str) -> Option<String> {
        let item = expression_of(view, column);
        let inside = item.strip_prefix("CAST(")?;
        let value = inside.split_once(" AS ")?.0;
        Some(value.trim_start_matches('N').trim_matches('\'').to_owned())
    }

    #[test]
    fn sys_foreign_keys_columns_match_the_published_list() {
        assert_eq!(
            published_columns("foreign_keys"),
            PUBLISHED_FOREIGN_KEYS_COLUMNS
        );
    }

    #[test]
    fn sys_foreign_key_columns_columns_match_the_published_list() {
        assert_eq!(
            published_columns("foreign_key_columns"),
            PUBLISHED_FOREIGN_KEY_COLUMNS_COLUMNS
        );
    }

    #[test]
    fn sys_check_constraints_columns_match_the_published_list() {
        assert_eq!(
            published_columns("check_constraints"),
            PUBLISHED_CHECK_CONSTRAINTS_COLUMNS
        );
    }

    #[test]
    fn sys_default_constraints_columns_match_the_published_list() {
        assert_eq!(
            published_columns("default_constraints"),
            PUBLISHED_DEFAULT_CONSTRAINTS_COLUMNS
        );
    }

    #[test]
    fn the_literal_columns_are_the_published_values() {
        // The 22 constants of the three views that publish any, each one against the row it
        // comes from. `name`, `parent_column_id`, `definition` and `is_system_named` are read
        // from the internal table, so their published values are checked by the row tests
        // below instead.
        let written: [(&str, &str, &str); 22] = [
            ("foreign_keys", "type", "CAST('F ' AS char(2))"),
            (
                "foreign_keys",
                "type_desc",
                "CAST(N'FOREIGN_KEY_CONSTRAINT' AS nvarchar(60))",
            ),
            ("foreign_keys", "is_disabled", "CAST(0 AS bit)"),
            ("foreign_keys", "is_not_for_replication", "CAST(0 AS bit)"),
            ("foreign_keys", "is_not_trusted", "CAST(0 AS bit)"),
            ("foreign_keys", "is_ms_shipped", "CAST(0 AS bit)"),
            ("foreign_keys", "is_published", "CAST(0 AS bit)"),
            ("foreign_keys", "is_schema_published", "CAST(0 AS bit)"),
            ("check_constraints", "type", "CAST('C ' AS char(2))"),
            (
                "check_constraints",
                "type_desc",
                "CAST(N'CHECK_CONSTRAINT' AS nvarchar(60))",
            ),
            (
                "check_constraints",
                "uses_database_collation",
                "CAST(1 AS bit)",
            ),
            ("check_constraints", "is_disabled", "CAST(0 AS bit)"),
            (
                "check_constraints",
                "is_not_for_replication",
                "CAST(0 AS bit)",
            ),
            ("check_constraints", "is_not_trusted", "CAST(0 AS bit)"),
            ("check_constraints", "is_ms_shipped", "CAST(0 AS bit)"),
            ("check_constraints", "is_published", "CAST(0 AS bit)"),
            ("check_constraints", "is_schema_published", "CAST(0 AS bit)"),
            ("default_constraints", "type", "CAST('D ' AS char(2))"),
            (
                "default_constraints",
                "type_desc",
                "CAST(N'DEFAULT_CONSTRAINT' AS nvarchar(60))",
            ),
            ("default_constraints", "is_ms_shipped", "CAST(0 AS bit)"),
            ("default_constraints", "is_published", "CAST(0 AS bit)"),
            (
                "default_constraints",
                "is_schema_published",
                "CAST(0 AS bit)",
            ),
        ];
        for (view, column, expression) in written {
            assert_eq!(
                expression_of(view, column),
                format!("{expression} AS {column}"),
                "sys.{view}.{column}"
            );
        }

        // The same literals read back as values, against the three published rows: a column
        // this file writes as a constant agrees with the row.
        let mut compared = 0;
        for (view, published) in [
            ("foreign_keys", PUBLISHED_FOREIGN_KEY_ROW.to_vec()),
            ("check_constraints", PUBLISHED_CHECK_ROW.to_vec()),
            ("default_constraints", PUBLISHED_DEFAULT_ROW.to_vec()),
        ] {
            for (column, value) in published {
                if let Some(literal) = bare_literal(view, column) {
                    assert_eq!(literal, value, "sys.{view}.{column}");
                    compared += 1;
                }
            }
        }
        // 5 of the 11 columns of the foreign key row, 6 of the 10 of the check row and 3 of
        // the 7 of the default row: the others are read from the internal table.
        assert_eq!(compared, 14);
    }

    #[test]
    fn the_columns_written_null_are_the_ones_with_no_datum_behind_them() {
        // `principal_id` (principals are not served), `create_date` and `modify_date` (no `*Meta`
        // carries an instant) and `key_index_id` (`ConstraintMeta::ForeignKey` holds no
        // index), which is 4 items of `sys.foreign_keys` and 3 of each of the two others.
        // The list below holds no item of `sys.foreign_key_columns`, whose 6 columns are
        // read.
        let mut nulls: Vec<(String, String)> = Vec::new();
        for (view, _) in definitions() {
            for column in published_columns(&view) {
                if expression_of(&view, &column).starts_with("CAST(NULL AS ") {
                    nulls.push((view.clone(), column));
                }
            }
        }
        nulls.sort();
        nulls.dedup();
        assert_eq!(
            nulls,
            vec![
                ("check_constraints".to_owned(), "create_date".to_owned()),
                ("check_constraints".to_owned(), "modify_date".to_owned()),
                ("check_constraints".to_owned(), "principal_id".to_owned()),
                ("default_constraints".to_owned(), "create_date".to_owned()),
                ("default_constraints".to_owned(), "modify_date".to_owned()),
                ("default_constraints".to_owned(), "principal_id".to_owned()),
                ("foreign_keys".to_owned(), "create_date".to_owned()),
                ("foreign_keys".to_owned(), "key_index_id".to_owned()),
                ("foreign_keys".to_owned(), "modify_date".to_owned()),
                ("foreign_keys".to_owned(), "principal_id".to_owned()),
            ]
        );
    }

    #[test]
    fn the_column_order_is_the_one_the_constants_name() {
        let tables = internal_tables();
        assert_eq!(tables.len(), 4);
        let names = |position: usize| -> Vec<String> {
            tables[position]
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect()
        };
        assert_eq!(names(0).len(), foreign_keys_columns::WIDTH);
        assert_eq!(names(0)[foreign_keys_columns::DATABASE_ID], "database_id");
        assert_eq!(names(0)[foreign_keys_columns::OBJECT_ID], "object_id");
        assert_eq!(names(0)[foreign_keys_columns::NAME], "name");
        assert_eq!(names(0)[foreign_keys_columns::SCHEMA_ID], "schema_id");
        assert_eq!(
            names(0)[foreign_keys_columns::PARENT_OBJECT_ID],
            "parent_object_id"
        );
        assert_eq!(
            names(0)[foreign_keys_columns::REFERENCED_OBJECT_ID],
            "referenced_object_id"
        );
        assert_eq!(
            names(0)[foreign_keys_columns::DELETE_REFERENTIAL_ACTION],
            "delete_referential_action"
        );
        assert_eq!(
            names(0)[foreign_keys_columns::DELETE_REFERENTIAL_ACTION_DESC],
            "delete_referential_action_desc"
        );
        assert_eq!(
            names(0)[foreign_keys_columns::UPDATE_REFERENTIAL_ACTION],
            "update_referential_action"
        );
        assert_eq!(
            names(0)[foreign_keys_columns::UPDATE_REFERENTIAL_ACTION_DESC],
            "update_referential_action_desc"
        );
        assert_eq!(
            names(0)[foreign_keys_columns::IS_SYSTEM_NAMED],
            "is_system_named"
        );

        assert_eq!(names(1).len(), foreign_key_columns_columns::WIDTH);
        assert_eq!(
            names(1)[foreign_key_columns_columns::DATABASE_ID],
            "database_id"
        );
        assert_eq!(
            names(1)[foreign_key_columns_columns::CONSTRAINT_OBJECT_ID],
            "constraint_object_id"
        );
        assert_eq!(
            names(1)[foreign_key_columns_columns::CONSTRAINT_COLUMN_ID],
            "constraint_column_id"
        );
        assert_eq!(
            names(1)[foreign_key_columns_columns::PARENT_OBJECT_ID],
            "parent_object_id"
        );
        assert_eq!(
            names(1)[foreign_key_columns_columns::PARENT_COLUMN_ID],
            "parent_column_id"
        );
        assert_eq!(
            names(1)[foreign_key_columns_columns::REFERENCED_OBJECT_ID],
            "referenced_object_id"
        );
        assert_eq!(
            names(1)[foreign_key_columns_columns::REFERENCED_COLUMN_ID],
            "referenced_column_id"
        );

        for position in [2, 3] {
            assert_eq!(names(position).len(), constraint_columns::WIDTH);
            assert_eq!(
                names(position)[constraint_columns::DATABASE_ID],
                "database_id"
            );
            assert_eq!(names(position)[constraint_columns::OBJECT_ID], "object_id");
            assert_eq!(names(position)[constraint_columns::NAME], "name");
            assert_eq!(names(position)[constraint_columns::SCHEMA_ID], "schema_id");
            assert_eq!(
                names(position)[constraint_columns::PARENT_OBJECT_ID],
                "parent_object_id"
            );
            assert_eq!(
                names(position)[constraint_columns::PARENT_COLUMN_ID],
                "parent_column_id"
            );
            assert_eq!(
                names(position)[constraint_columns::DEFINITION],
                "definition"
            );
            assert_eq!(
                names(position)[constraint_columns::IS_SYSTEM_NAMED],
                "is_system_named"
            );
        }
    }

    #[test]
    fn the_two_constraint_tables_share_their_shape() {
        let tables = internal_tables();
        assert_eq!(tables[2].name, CHECK_CONSTRAINTS_TABLE);
        assert_eq!(tables[3].name, DEFAULT_CONSTRAINTS_TABLE);
        assert_eq!(tables[2].columns, tables[3].columns);
        // What separates the two views is the literal `type` they write.
        assert_ne!(
            expression_of("check_constraints", "type"),
            expression_of("default_constraints", "type")
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
                definition.ends_with("\n WHERE database_id = DB_ID()"),
                "sys.{name} is a per-database view: {definition}"
            );
        }
    }

    #[test]
    fn each_definition_parses_as_one_select() {
        // The four texts go through the parser: a column name that had to be bracketed, as
        // the `precision` of `sys.columns` is in `views/sys_tables.rs`, would be refused
        // here: a select list holding a bare `precision` is a syntax error of that parser.
        for (name, definition) in definitions() {
            let batch = parse_batch(&definition, &ParseOptions::default())
                .unwrap_or_else(|err| panic!("sys.{name}: {}", err.message));
            assert_eq!(batch.statements.len(), 1, "sys.{name}");
            assert!(
                matches!(batch.statements[0], Statement::Select(_)),
                "sys.{name} is a SELECT"
            );
        }
    }

    #[test]
    fn each_view_reads_its_own_internal_table() {
        // The two constraint tables share their shape, so this test is what ties a view to
        // the table that carries it: `sys.default_constraints` reading the table of the
        // `CHECK` constraints fails this test alone.
        let named: [(&str, &str); 4] = [
            ("foreign_keys", FOREIGN_KEYS_TABLE),
            ("foreign_key_columns", FOREIGN_KEY_COLUMNS_TABLE),
            ("check_constraints", CHECK_CONSTRAINTS_TABLE),
            ("default_constraints", DEFAULT_CONSTRAINTS_TABLE),
        ];
        for (view, internal) in named {
            let definition = definition_of(view);
            assert!(
                definition.contains(&format!("\n  FROM master.dbo.{internal}\n")),
                "sys.{view} reads {internal}: {definition}"
            );
        }
        // Same couples read the other way round: the `SystemViewDef`s an `InternalTableDef`
        // carries read that very table, in the four system databases.
        let mut couples = 0;
        for table in internal_tables() {
            for view in &table.views {
                assert!(
                    view.definition
                        .contains(&format!("\n  FROM master.dbo.{}\n", table.name)),
                    "sys.{} is carried by {} and reads {}",
                    view.name.name,
                    table.name,
                    view.definition
                );
                couples += 1;
            }
        }
        assert_eq!(couples, 4 * SYSTEM_DATABASES.len());
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        for table in internal_tables() {
            let databases: Vec<String> = table
                .views
                .iter()
                .map(|view| view.name.database.clone())
                .collect();
            assert_eq!(databases, SYSTEM_DATABASES.map(str::to_owned).to_vec());
            let schemas: Vec<&str> = table
                .views
                .iter()
                .map(|view| view.name.schema.as_str())
                .collect();
            assert_eq!(schemas, vec![VIEW_SCHEMA; SYSTEM_DATABASES.len()]);
            // One text for the four copies: the filter on `DB_ID()` does the separating.
            let texts: Vec<&str> = table
                .views
                .iter()
                .map(|view| view.definition.as_str())
                .collect();
            assert_eq!(texts, vec![texts[0]; SYSTEM_DATABASES.len()]);
        }
    }

    #[test]
    fn views_exist_and_are_empty_without_fk() {
        // The four tables are described with their views and no row, and a table that carries
        // no constraint of these three kinds gives no row to any of the four functions.
        let tables = internal_tables();
        assert_eq!(tables.len(), 4);
        for table in &tables {
            assert!(table.rows.is_empty(), "{}", table.name);
            assert_eq!(table.views.len(), SYSTEM_DATABASES.len(), "{}", table.name);
        }
        let plain = [table("tbl011_plain", &["id", "label"])];
        assert_eq!(
            foreign_key_rows(&plain, &[]).expect("rows"),
            Vec::<Row>::new()
        );
        assert_eq!(
            foreign_key_column_rows(&plain).expect("rows"),
            Vec::<Row>::new()
        );
        assert_eq!(
            check_constraint_rows(&plain, &[]).expect("rows"),
            Vec::<Row>::new()
        );
        assert_eq!(
            default_constraint_rows(&plain, &[]).expect("rows"),
            Vec::<Row>::new()
        );
    }

    #[test]
    fn foreign_key_row_when_tabledef_has_one() {
        let mut child = table("tbl011_child", &["id", "parent_left", "parent_right"]);
        child.constraints.push(ConstraintMeta::ForeignKey {
            constraint: ObjectId(1_000_500),
            system_named: true,
            columns: vec![ColumnId(2), ColumnId(3)],
            referenced_table: ObjectId(1_000_042),
            referenced_columns: vec![ColumnId(1), ColumnId(2)],
            referenced_index: vauban_storage::IndexId(4),
            on_delete: RefAction::Cascade,
            on_update: RefAction::NoAction,
        });
        let tables = [child];

        let rows = foreign_key_rows(&tables, &[]).expect("rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0].0;
        assert_eq!(row.len(), foreign_keys_columns::WIDTH);
        assert_eq!(
            row[foreign_keys_columns::PARENT_OBJECT_ID],
            Value::I32(1_000_000)
        );
        assert_eq!(
            row[foreign_keys_columns::REFERENCED_OBJECT_ID],
            Value::I32(1_000_042)
        );
        assert_eq!(
            row[foreign_keys_columns::DELETE_REFERENTIAL_ACTION],
            Value::I8(1)
        );
        assert_eq!(
            row[foreign_keys_columns::DELETE_REFERENTIAL_ACTION_DESC],
            text("CASCADE")
        );
        assert_eq!(
            row[foreign_keys_columns::UPDATE_REFERENTIAL_ACTION],
            Value::I8(0)
        );
        assert_eq!(
            row[foreign_keys_columns::UPDATE_REFERENTIAL_ACTION_DESC],
            text("NO_ACTION")
        );
        assert_eq!(row[foreign_keys_columns::SCHEMA_ID], Value::I32(1));
        assert_eq!(row[foreign_keys_columns::IS_SYSTEM_NAMED], Value::Bit(true));
        assert_eq!(
            row[foreign_keys_columns::OBJECT_ID],
            Value::I32(1_000_500),
            "the object `constraints.rs` gave the constraint"
        );
        // A key over two columns is named without a column group, where a one-column key is
        // named after its column (`constraints.rs`, module documentation).
        assert_eq!(
            row[foreign_keys_columns::NAME],
            text(&generated_constraint_name("FK", &tables[0], None, 0))
        );

        // Two columns in the key, numbered from 1, each one with the column it points at
        // (1/2/1 then 2/3/2).
        let columns = foreign_key_column_rows(&tables).expect("rows");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].0.len(), foreign_key_columns_columns::WIDTH);
        let triples: Vec<(Value, Value, Value)> = columns
            .iter()
            .map(|row| {
                (
                    row.0[foreign_key_columns_columns::CONSTRAINT_COLUMN_ID].clone(),
                    row.0[foreign_key_columns_columns::PARENT_COLUMN_ID].clone(),
                    row.0[foreign_key_columns_columns::REFERENCED_COLUMN_ID].clone(),
                )
            })
            .collect();
        assert_eq!(
            triples,
            vec![
                (Value::I32(1), Value::I32(2), Value::I32(1)),
                (Value::I32(2), Value::I32(3), Value::I32(2)),
            ]
        );
    }

    #[test]
    fn check_constraint_row_stores_the_expression() {
        let mut target = table("tbl011_check", &["id", "amount"]);
        let expr = Expr::Binary {
            op: BinaryOp::Ge,
            op_span: Span::EMPTY,
            left: Box::new(Expr::Column(ColumnRef {
                qualifier: None,
                name: Ident {
                    value: "amount".to_owned(),
                    quoted: false,
                },
                span: Span::EMPTY,
            })),
            right: Box::new(integer("0")),
            span: Span::EMPTY,
        };
        target.constraints.push(ConstraintMeta::Check {
            constraint: ObjectId(1_000_501),
            system_named: false,
            expr: expr.clone(),
            definition: definition_text(&expr),
        });
        let named = [constraint_object(1_000_501, &target, "ck_tbl011_check")];
        let rows = check_constraint_rows(&[target], &named).expect("rows");

        assert_eq!(rows.len(), 1);
        let row = &rows[0].0;
        assert_eq!(row.len(), constraint_columns::WIDTH);
        assert_eq!(
            row[constraint_columns::PARENT_OBJECT_ID],
            Value::I32(1_000_000)
        );
        // The text stored is the expression given, re-serialised between parentheses;
        // SQL Server writes `([amount]>=(0))` for the same declaration.
        assert_eq!(row[constraint_columns::DEFINITION], text("(amount >= 0)"));
        assert_eq!(definition_text(&expr), "(amount >= 0)");
        // `0`, not `TABLE_LEVEL_COLUMN`, so that the assertion is not the constant compared
        // with itself: SQL Server answers 0 on `CHECK (a < b)` and `CHECK (1 = 1)` and on
        // the 7 table-level checks of the name table.
        assert_eq!(row[constraint_columns::PARENT_COLUMN_ID], Value::I32(0));
        assert_eq!(TABLE_LEVEL_COLUMN, 0);
        // The object of `constraints.rs` carries the identifier and the written name, and
        // `is_system_named` follows the constraint.
        assert_eq!(row[constraint_columns::OBJECT_ID], Value::I32(1_000_501));
        assert_eq!(row[constraint_columns::NAME], text("ck_tbl011_check"));
        assert_eq!(row[constraint_columns::IS_SYSTEM_NAMED], Value::Bit(false));
    }

    #[test]
    fn default_constraint_row_for_column_default() {
        // A column with a `DEFAULT`: one row, the table as `parent_object_id` and the
        // `column_id` of the column — its 1-based position — as `parent_column_id`.
        let mut target = table("tbl011_default", &["id", "amount"]);
        target.columns[1] = meta_column(1, "amount", Some(integer("0")));
        let rows = default_constraint_rows(&[target], &[]).expect("rows");

        assert_eq!(rows.len(), 1);
        let row = &rows[0].0;
        assert_eq!(row.len(), constraint_columns::WIDTH);
        assert_eq!(
            row[constraint_columns::PARENT_OBJECT_ID],
            Value::I32(1_000_000)
        );
        assert_eq!(row[constraint_columns::PARENT_COLUMN_ID], Value::I32(2));
        assert_eq!(row[constraint_columns::DEFINITION], text("(0)"));
        assert_eq!(row[constraint_columns::SCHEMA_ID], Value::I32(1));
    }

    #[test]
    fn a_default_is_published_once_per_column() {
        // A `ColumnMeta::default` and a `ConstraintMeta::Default` on the same column are the
        // same default: one row. A `ConstraintMeta::Default` on another column adds one.
        let mut target = table("tbl011_twice", &["id", "amount"]);
        target.columns[1] = meta_column(1, "amount", Some(integer("0")));
        target.constraints.push(ConstraintMeta::Default {
            constraint: ObjectId(1_000_502),
            system_named: true,
            column: ColumnId(2),
            expr: integer("0"),
        });
        target.constraints.push(ConstraintMeta::Default {
            constraint: ObjectId(1_000_503),
            system_named: true,
            column: ColumnId(1),
            expr: integer("7"),
        });
        let rows = default_constraint_rows(&[target], &[]).expect("rows");

        let columns: Vec<Value> = rows
            .iter()
            .map(|row| row.0[constraint_columns::PARENT_COLUMN_ID].clone())
            .collect();
        assert_eq!(columns, vec![Value::I32(2), Value::I32(1)]);
        assert_eq!(rows[1].0[constraint_columns::DEFINITION], text("(7)"));
    }

    #[test]
    fn the_referential_action_codes_are_the_published_ones() {
        let published: [(RefAction, u8, &str); 4] = [
            (RefAction::NoAction, 0, "NO_ACTION"),
            (RefAction::Cascade, 1, "CASCADE"),
            (RefAction::SetNull, 2, "SET_NULL"),
            (RefAction::SetDefault, 3, "SET_DEFAULT"),
        ];
        for (action, code, desc) in published {
            assert_eq!(referential_action_code(action), code, "{action:?}");
            assert_eq!(referential_action_desc(action), desc, "{action:?}");
        }
    }

    #[test]
    fn a_generated_name_follows_the_published_shape() {
        // The names of `GENERATED_NAMES`, compared on everything but the eight hexadecimal
        // digits, which are ours.
        let head = |name: &str| -> String {
            name.rsplit_once("__")
                .expect("a generated name ends with its digits")
                .0
                .to_owned()
        };
        for (kind, table_name, column, published) in GENERATED_NAMES {
            let meta = table(table_name, &[]);
            let built = generated_constraint_name(kind, &meta, column, 0);
            assert_eq!(head(&built), head(published), "{published}");
            assert_eq!(built.len(), published.len(), "{published}");
            assert!(
                built
                    .rsplit("__")
                    .next()
                    .expect("digits")
                    .chars()
                    .all(|digit| digit.is_ascii_hexdigit() && !digit.is_ascii_lowercase()),
                "{built}"
            );
        }
    }

    #[test]
    fn two_constraints_of_one_table_take_two_names() {
        let meta = table("tbl011_two", &["a", "b"]);
        let first = generated_constraint_name("CK", &meta, None, 0);
        let second = generated_constraint_name("CK", &meta, None, 1);
        assert_ne!(first, second);
        // Same inputs, same name: the digits do not move between two runs.
        assert_eq!(first, generated_constraint_name("CK", &meta, None, 0));
        // The kind is part of the digits, so a `DF` at the same position differs.
        assert_ne!(first, generated_constraint_name("DF", &meta, None, 0));
    }

    #[test]
    fn a_generated_name_is_flagged_as_system_named() {
        let mut target = table("tbl011_named", &["id"]);
        target.columns[0] = meta_column(0, "id", Some(integer("1")));
        let rows = default_constraint_rows(&[target], &[]).expect("rows");
        assert_eq!(
            rows[0].0[constraint_columns::IS_SYSTEM_NAMED],
            Value::Bit(true)
        );
        let Value::String(name) = &rows[0].0[constraint_columns::NAME] else {
            panic!("the name is text");
        };
        assert!(name.text.starts_with("DF__tbl011_named__id__"), "{name:?}");
    }

    #[test]
    fn a_column_default_without_its_constraint_has_no_object_id() {
        // `0` is below the range `table::FIRST_USER_OBJECT_ID` opens, so a client that
        // compares identifiers does not take a constraint for another object. A `TableMeta`
        // `create_table` built carries a `ConstraintMeta::Default` per column default
        // (`constraints.rs`); one built by hand does not, and the row falls back on `0`.
        assert_eq!(NO_OBJECT_ID, 0);
        const { assert!(NO_OBJECT_ID < crate::table::FIRST_USER_OBJECT_ID) };
        let mut target = table("tbl011_noid", &["id"]);
        target.columns[0] = meta_column(0, "id", Some(integer("1")));
        let rows = default_constraint_rows(&[target], &[]).expect("rows");
        assert_eq!(rows[0].0[constraint_columns::OBJECT_ID], Value::I32(0));
    }

    #[test]
    fn a_row_falls_back_on_the_generated_name_without_its_object() {
        // The same constraint read with and without the object that names it: the written
        // name when the object is there, the generated shape when it is not.
        let mut target = table("tbl011_fallback", &["id"]);
        target.constraints.push(ConstraintMeta::Check {
            constraint: ObjectId(1_000_504),
            system_named: false,
            expr: integer("1"),
            definition: "(1)".to_owned(),
        });
        let named = [constraint_object(1_000_504, &target, "ck_written")];
        let with = check_constraint_rows(std::slice::from_ref(&target), &named).expect("rows");
        assert_eq!(with[0].0[constraint_columns::NAME], text("ck_written"));
        let without = check_constraint_rows(&[target], &[]).expect("rows");
        let Value::String(name) = &without[0].0[constraint_columns::NAME] else {
            panic!("the name is text");
        };
        assert!(name.text.starts_with("CK__tbl011_fallback__"), "{name:?}");
    }

    #[test]
    fn a_schema_the_bootstrap_does_not_know_has_no_identifier() {
        let mut target = table("tbl011_other", &["id"]);
        target.schema = "app".to_owned();
        target.constraints.push(ConstraintMeta::Check {
            constraint: ObjectId(1_000_505),
            system_named: true,
            expr: integer("1"),
            definition: "(1)".to_owned(),
        });
        let rows = check_constraint_rows(&[target], &[]).expect("rows");
        assert_eq!(
            rows[0].0[constraint_columns::SCHEMA_ID],
            Value::I32(UNKNOWN_SCHEMA_ID)
        );
        assert_eq!(schema_id("sys"), 4);
        assert_eq!(schema_id("DBO"), 1);
    }

    #[test]
    fn the_position_of_a_key_column_counts_from_one() {
        assert_eq!(key_position(0), 1);
        assert_eq!(key_position(15), 16);
        assert_eq!(key_position(usize::MAX), i32::MAX);
    }
}
