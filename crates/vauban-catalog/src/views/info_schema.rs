//! `INFORMATION_SCHEMA.TABLES`, `INFORMATION_SCHEMA.COLUMNS` and `INFORMATION_SCHEMA.SCHEMATA`.
//!
//! # The shape of the views
//!
//! The column list of each of the three views — names, order, types — is the one SQL Server
//! 2022 publishes: 4, 23 and 6 columns. The unit tests below freeze the three vectors of
//! names and compare them with the text this file builds
//! (`information_schema_tables_columns_match_the_published_list` and its two neighbours),
//! and the nullability declared on each internal column is the published nullability of the
//! view column it feeds (`the_declared_nullability_is_the_published_one`).
//!
//! # Two internal tables, and the table of the schemas for the third view
//!
//! A view is a `SELECT` over one denormalised internal table, without a join.
//! `INFORMATION_SCHEMA` publishes names where `sys.*` publishes identifiers — `TABLE_SCHEMA`
//! is the name of the schema, `DATA_TYPE` the name of the type — so the rows of
//! `vauban_sys_objects` and `vauban_sys_columns` (`views/sys_tables.rs`) would have to be
//! joined with `vauban_sys_schemas` and `vauban_sys_types` to be read here. [`TABLES_TABLE`]
//! and [`COLUMNS_TABLE`] therefore carry those names themselves.
//!
//! `SCHEMATA` needs no new table: its three columns of substance are a catalogue name, a schema
//! name and an owner, which is what `vauban_sys_schemas` already holds. `SCHEMA_OWNER` reads
//! the `name` column a second time, the owner of a schema being its own name in a fresh
//! database; that column is declared `NOT NULL` while SQL Server publishes the view column as
//! nullable, which is the one divergence of nullability of this file. The definition of
//! `SCHEMATA` travels in the `views` of [`TABLES_TABLE`], which the bootstrap installs like
//! any other (unit test `the_schemata_view_reads_the_table_of_the_schemas`).
//!
//! The three internal tables live in `master` while the views are per-database, so each
//! definition filters on `database_id = DB_ID()`, as the other files of `views/` do. A
//! database a client creates receives the three schemas of `SYSTEM_SCHEMAS` from
//! `database.rs`, so `SCHEMATA` answers there too.
//!
//! # Read columns and literal columns
//!
//! [`TABLES_TABLE`] holds 4 columns, 3 of which `TABLES` reads; [`COLUMNS_TABLE`] holds 16, of
//! which `COLUMNS` reads 15; `SCHEMATA` reads one column of `vauban_sys_schemas`, twice. The
//! other items of a select list are literals, under the two rules of `views/sys_core.rs`: a
//! value the catalogue does not store is `CAST(NULL AS …)`, a value SQL Server publishes the
//! same way on each row is written as that constant. Here they are:
//!
//! - `TABLE_CATALOG` and `CATALOG_NAME` are `DB_NAME()`, whose result type is the published
//!   `nvarchar(128)` nullable (`crates/vauban-sysfn/src/builtins/system.rs`): `TABLE_CATALOG`
//!   equals `DB_NAME()` on the row of a user table, and `CATALOG_NAME` equals it on each row
//!   of `SCHEMATA`;
//! - the seven `CHARACTER_SET_CATALOG`, `CHARACTER_SET_SCHEMA`, `COLLATION_CATALOG`,
//!   `COLLATION_SCHEMA`, `DOMAIN_CATALOG`, `DOMAIN_SCHEMA`, `DOMAIN_NAME` of `COLUMNS` and the
//!   two `DEFAULT_CHARACTER_SET_CATALOG` / `DEFAULT_CHARACTER_SET_SCHEMA` of `SCHEMATA` are
//!   `CAST(NULL AS nvarchar(128))`: SQL Server 2022 publishes `NULL` on each of them, the
//!   seven character columns included;
//! - `DEFAULT_CHARACTER_SET_NAME` is `CAST(N'iso_1' AS nvarchar(128))`, the value each row of
//!   `SCHEMATA` carries.
//!
//! The unit test `the_literal_columns_are_the_published_values` holds those expressions and
//! counts them.
//!
//! # What a row says about the type of a column
//!
//! [`type_facts`] answers the nine values `COLUMNS` publishes about a type, for the 24
//! variants of [`SqlType`], `(max)` for the three types that take it, and the scales 0 and 7
//! of `time` and `datetime2`; the numbers are frozen by
//! `the_type_facts_are_those_of_information_schema`. In short: `CHARACTER_MAXIMUM_LENGTH` is
//! the declared length and `CHARACTER_OCTET_LENGTH` its size in bytes, so `nvarchar(20)`
//! publishes 20 and 40 and `nvarchar(max)` publishes −1 and −1; `NUMERIC_PRECISION_RADIX` is
//! 10 for the exact numerics and 2 for `float` and `real`, which leave `NUMERIC_SCALE` at
//! `NULL`; `DATETIME_PRECISION` is the scale of the type, 3 for `datetime` and 0 for `date`
//! and `smalldatetime`; `bit` and `uniqueidentifier` answer `NULL` on the eight values that
//! follow the name.
//!
//! `IS_NULLABLE` is `YES` / `NO`, not `Y` / `N` (unit test
//! `information_schema_columns_nullable`).
//!
//! `ORDINAL_POSITION` is [`ColumnMeta::ordinal`] plus one and not the `ColumnId` that
//! `sys.columns.column_id` publishes: on a `(a, b, c)` table, `DROP COLUMN b` then `ADD d`
//! leaves the `column_id`s 1, 3, 4 and the `ORDINAL_POSITION`s 1, 2, 3 (unit test
//! `the_ordinal_position_is_renumbered_where_the_column_id_keeps_its_value`).
//!
//! `COLLATION_NAME` is the collation of the database on each character column, where a
//! column declared `COLLATE Latin1_General_BIN` publishes that name in SQL Server:
//! [`Collation`](vauban_types::Collation) carries no name to write, as for `sys.columns` in
//! `views/sys_tables.rs`. Deliberate difference from SQL Server for now.
//!
//! `COLUMN_DEFAULT` is the text of the `DEFAULT` of the column, `NULL` when it has none, and
//! `NULL` on an `IDENTITY` column and on a computed column. This file writes the rendering of
//! [`Expr`](vauban_parser::Expr) inside one pair of parentheses, which differs from the text
//! SQL Server stores: `DEFAULT 0` is `((0))` there and `(0)` here, `DEFAULT (1 + 2)` is
//! `((1)+(2))` there and `((1 + 2))` here, while `DEFAULT 'x'` is `('x')` on both (unit test
//! `the_column_default_is_the_parenthesised_text_of_the_expression`). Normalising the text of
//! a constraint the way SQL Server does is not done; deliberate difference for now.
//!
//! # Execution
//!
//! What this file produces is the shape of the three internal tables, the rows of
//! [`SCHEMATA_TABLE`], the text of the three definitions and the two functions that turn a
//! `*Meta` into rows; `sys_rows.rs` writes those rows into `storage` at each DDL, and the
//! binder expands the text of a view in place of its name.

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::{DbId, Row};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{DEFAULT_COLLATION_NAME, SCHEMAS_TABLE, SYSTEM_DATABASES};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::{ColumnMeta, QualifiedName, TableMeta};

/// Internal table of the tables, read by `INFORMATION_SCHEMA.TABLES`.
///
/// A `vauban_is_*` name of our own, like the `vauban_sys_*` ones; what a client reads is the
/// view built over it.
pub(crate) const TABLES_TABLE: &str = "vauban_is_tables";

/// Internal table of the columns, read by `INFORMATION_SCHEMA.COLUMNS`.
pub(crate) const COLUMNS_TABLE: &str = "vauban_is_columns";

/// The schema the three views of this file live in, created by the bootstrap in each database.
const VIEW_SCHEMA: &str = "INFORMATION_SCHEMA";

/// The type of a `sysname` column, `nvarchar(128)`.
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));

/// `INFORMATION_SCHEMA.TABLES.TABLE_TYPE` of a table.
const BASE_TABLE_TYPE: &str = "BASE TABLE";

/// The `TABLE_TYPE` of a view, which no row of [`TABLES_TABLE`] carries: the catalogue holds
/// no user view, so [`table_rows`] writes [`BASE_TABLE_TYPE`] on each row it builds (unit
/// test `the_table_type_of_a_user_table_is_base_table`).
const VIEW_TABLE_TYPE: &str = "VIEW";

/// `IS_NULLABLE` of a column that accepts `NULL`: the word, not its initial.
const IS_NULLABLE_YES: &str = "YES";

/// `IS_NULLABLE` of a column that does not accept `NULL`. See [`IS_NULLABLE_YES`].
const IS_NULLABLE_NO: &str = "NO";

/// `CHARACTER_SET_NAME` of a `char` or a `varchar` column.
const CHARACTER_SET_ISO_1: &str = "iso_1";

/// `CHARACTER_SET_NAME` of an `nchar` or an `nvarchar` column.
const CHARACTER_SET_UNICODE: &str = "UNICODE";

/// `SCHEMATA.DEFAULT_CHARACTER_SET_NAME`, the same text on each row.
const DEFAULT_CHARACTER_SET_NAME: &str = "iso_1";

/// Where each column of [`TABLES_TABLE`] sits in a [`Row`], as `bootstrap.rs` does for its own
/// tables: a writer of a row addresses it through these constants rather than restating
/// the order (unit test `the_column_order_is_the_one_the_constants_name`).
pub(crate) mod tables_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `table_schema nvarchar(128)`: the name of the schema of the table.
    pub(crate) const TABLE_SCHEMA: usize = 1;
    /// `table_name nvarchar(128)`: the name of the table.
    pub(crate) const TABLE_NAME: usize = 2;
    /// `table_type varchar(10)`: `BASE TABLE` for a table.
    pub(crate) const TABLE_TYPE: usize = 3;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 4;
}

/// Where each column of [`COLUMNS_TABLE`] sits in a [`Row`]. Same rule as [`tables_columns`].
pub(crate) mod columns_columns {
    /// `database_id int`: the database the table belongs to, the filter of the view.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `table_schema nvarchar(128)`: the name of the schema of the table.
    pub(crate) const TABLE_SCHEMA: usize = 1;
    /// `table_name nvarchar(128)`: the name of the table.
    pub(crate) const TABLE_NAME: usize = 2;
    /// `column_name nvarchar(128)`: the name of the column.
    pub(crate) const COLUMN_NAME: usize = 3;
    /// `ordinal_position int`: the place of the column among the live columns of its table,
    /// from `1` — [`ColumnMeta::ordinal`](crate::ColumnMeta::ordinal) plus one, not its
    /// `ColumnId`. SQL Server renumbers it after a `DROP COLUMN` where
    /// `sys.columns.column_id` keeps its value (unit test
    /// `the_ordinal_position_is_renumbered_where_the_column_id_keeps_its_value`).
    pub(crate) const ORDINAL_POSITION: usize = 4;
    /// `column_default nvarchar(4000)`: the text of its `DEFAULT`, `NULL` without one.
    pub(crate) const COLUMN_DEFAULT: usize = 5;
    /// `is_nullable varchar(3)`: `YES` or `NO`.
    pub(crate) const IS_NULLABLE: usize = 6;
    /// `data_type nvarchar(128)`: the bare name of the type, `nvarchar` for `nvarchar(20)`.
    pub(crate) const DATA_TYPE: usize = 7;
    /// `character_maximum_length int`: the declared length, `-1` for a `(max)` type.
    pub(crate) const CHARACTER_MAXIMUM_LENGTH: usize = 8;
    /// `character_octet_length int`: that length in bytes.
    pub(crate) const CHARACTER_OCTET_LENGTH: usize = 9;
    /// `numeric_precision tinyint`.
    pub(crate) const NUMERIC_PRECISION: usize = 10;
    /// `numeric_precision_radix smallint`: 10 or 2.
    pub(crate) const NUMERIC_PRECISION_RADIX: usize = 11;
    /// `numeric_scale int`.
    pub(crate) const NUMERIC_SCALE: usize = 12;
    /// `datetime_precision smallint`: the scale of a date or time type.
    pub(crate) const DATETIME_PRECISION: usize = 13;
    /// `character_set_name nvarchar(128)`: `iso_1` or `UNICODE`, `NULL` outside the character
    /// types.
    pub(crate) const CHARACTER_SET_NAME: usize = 14;
    /// `collation_name nvarchar(128)`: `NULL` outside the character types.
    pub(crate) const COLLATION_NAME: usize = 15;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 16;
}

/// The select list of `INFORMATION_SCHEMA.TABLES`: `(column, expression)`, in the published
/// order.
///
/// An expression equal to the lower-cased name of a column of [`TABLES_TABLE`] reads that
/// column; the others are the literals the module documentation explains.
const TABLES_VIEW: [(&str, &str); 4] = [
    ("TABLE_CATALOG", "DB_NAME()"),
    ("TABLE_SCHEMA", "table_schema"),
    ("TABLE_NAME", "table_name"),
    ("TABLE_TYPE", "table_type"),
];

/// The select list of `INFORMATION_SCHEMA.COLUMNS`: its 23 columns, 15 of which
/// read [`COLUMNS_TABLE`].
const COLUMNS_VIEW: [(&str, &str); 23] = [
    ("TABLE_CATALOG", "DB_NAME()"),
    ("TABLE_SCHEMA", "table_schema"),
    ("TABLE_NAME", "table_name"),
    ("COLUMN_NAME", "column_name"),
    ("ORDINAL_POSITION", "ordinal_position"),
    ("COLUMN_DEFAULT", "column_default"),
    ("IS_NULLABLE", "is_nullable"),
    ("DATA_TYPE", "data_type"),
    ("CHARACTER_MAXIMUM_LENGTH", "character_maximum_length"),
    ("CHARACTER_OCTET_LENGTH", "character_octet_length"),
    ("NUMERIC_PRECISION", "numeric_precision"),
    ("NUMERIC_PRECISION_RADIX", "numeric_precision_radix"),
    ("NUMERIC_SCALE", "numeric_scale"),
    ("DATETIME_PRECISION", "datetime_precision"),
    ("CHARACTER_SET_CATALOG", "CAST(NULL AS nvarchar(128))"),
    ("CHARACTER_SET_SCHEMA", "CAST(NULL AS nvarchar(128))"),
    ("CHARACTER_SET_NAME", "character_set_name"),
    ("COLLATION_CATALOG", "CAST(NULL AS nvarchar(128))"),
    ("COLLATION_SCHEMA", "CAST(NULL AS nvarchar(128))"),
    ("COLLATION_NAME", "collation_name"),
    ("DOMAIN_CATALOG", "CAST(NULL AS nvarchar(128))"),
    ("DOMAIN_SCHEMA", "CAST(NULL AS nvarchar(128))"),
    ("DOMAIN_NAME", "CAST(NULL AS nvarchar(128))"),
];

/// The select list of `INFORMATION_SCHEMA.SCHEMATA`: its 6 columns, 2 of which
/// read the `name` column of `SCHEMAS_TABLE`.
const SCHEMATA_VIEW: [(&str, &str); 6] = [
    ("CATALOG_NAME", "DB_NAME()"),
    ("SCHEMA_NAME", "name"),
    ("SCHEMA_OWNER", "name"),
    (
        "DEFAULT_CHARACTER_SET_CATALOG",
        "CAST(NULL AS nvarchar(128))",
    ),
    (
        "DEFAULT_CHARACTER_SET_SCHEMA",
        "CAST(NULL AS nvarchar(128))",
    ),
    (
        "DEFAULT_CHARACTER_SET_NAME",
        "CAST(N'iso_1' AS nvarchar(128))",
    ),
];

/// The internal tables this file describes: the tables and the columns.
///
/// The bootstrap creates them in `master`, with their views. Their `rows` are empty: the rows of a
/// user table are built from the `*Meta` of the catalogue by [`table_rows`] and [`column_rows`].
/// The third view, `SCHEMATA`, reads the table of the schemas of `bootstrap.rs` and travels with
/// [`TABLES_TABLE`] (module documentation).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: TABLES_TABLE.to_owned(),
            columns: tables_table_columns(),
            clustered_key: None,
            rows: Vec::new(),
            views: [
                views_of(
                    "TABLES",
                    &definition(&TABLES_VIEW, TABLES_TABLE, Some("database_id = DB_ID()")),
                ),
                views_of(
                    "SCHEMATA",
                    &definition(&SCHEMATA_VIEW, SCHEMAS_TABLE, Some("database_id = DB_ID()")),
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
                "COLUMNS",
                &definition(&COLUMNS_VIEW, COLUMNS_TABLE, Some("database_id = DB_ID()")),
            ),
        },
    ]
}

/// The rows of [`TABLES_TABLE`]: one per table, of type [`BASE_TABLE_TYPE`].
///
/// The caller passes the tables of one catalogue — [`TableStore::live`](crate::table::TableStore::live)
/// gives them in increasing [`ObjectId`](crate::ObjectId) order — and gets the rows in the same
/// order.
///
/// # Errors
///
/// [`InternalError::Bug`] when a [`DbId`] does not fit in the `int` the internal table holds,
/// which is the check `bootstrap.rs` makes on the same value.
pub(crate) fn table_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    tables
        .iter()
        .map(|table| {
            Ok(Row(vec![
                Value::I32(database_id(table.database)?),
                text(&table.schema),
                text(&table.name),
                text(BASE_TABLE_TYPE),
            ]))
        })
        .collect()
}

/// The rows of [`COLUMNS_TABLE`]: one per column, in `ordinal_position` order within each table.
///
/// # Errors
///
/// Those of [`table_rows`].
pub(crate) fn column_rows(tables: &[TableMeta]) -> SqlResult<Vec<Row>> {
    let mut rows = Vec::new();
    for table in tables {
        let database = database_id(table.database)?;
        for column in &table.columns {
            rows.push(column_row(database, table, column));
        }
    }
    Ok(rows)
}

/// The row of [`COLUMNS_TABLE`] of one column. See [`column_rows`].
fn column_row(database: i32, table: &TableMeta, column: &ColumnMeta) -> Row {
    let facts = type_facts(&column.ty.ty);
    Row(vec![
        Value::I32(database),
        text(&table.schema),
        text(&table.name),
        text(&column.name),
        Value::I32(i32::from(column.ordinal) + 1),
        column_default(column),
        text(if column.ty.nullable {
            IS_NULLABLE_YES
        } else {
            IS_NULLABLE_NO
        }),
        text(facts.data_type),
        number(facts.character_maximum_length.map(Value::I32)),
        number(facts.character_octet_length.map(Value::I32)),
        number(facts.numeric_precision.map(Value::I8)),
        number(facts.numeric_precision_radix.map(Value::I16)),
        number(facts.numeric_scale.map(Value::I32)),
        number(facts.datetime_precision.map(Value::I16)),
        facts.character_set_name.map_or(Value::Null, text),
        facts.collation_name.map_or(Value::Null, text),
    ])
}

/// `COLUMN_DEFAULT` of a column: the text of its `DEFAULT` inside one pair of parentheses,
/// `NULL` for a column declared without a `DEFAULT` clause (unit test
/// `the_column_default_is_the_parenthesised_text_of_the_expression`). See the module
/// documentation for the text SQL Server stores instead.
fn column_default(column: &ColumnMeta) -> Value {
    match &column.default {
        Some(expr) => text(&format!("({expr})")),
        None => Value::Null,
    }
}

/// What `INFORMATION_SCHEMA.COLUMNS` publishes about the type of a column.
///
/// Nine values, each `None` where the view publishes `NULL`. See [`type_facts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypeFacts {
    /// `DATA_TYPE`: the bare name of the type, [`SqlType::name`].
    data_type: &'static str,
    /// `CHARACTER_MAXIMUM_LENGTH`: the declared length, `-1` for a `(max)` type.
    character_maximum_length: Option<i32>,
    /// `CHARACTER_OCTET_LENGTH`: that length in bytes, `-1` for a `(max)` type.
    character_octet_length: Option<i32>,
    /// `NUMERIC_PRECISION`.
    numeric_precision: Option<u8>,
    /// `NUMERIC_PRECISION_RADIX`: 10 for the exact numerics, 2 for `float` and `real`.
    numeric_precision_radix: Option<i16>,
    /// `NUMERIC_SCALE`.
    numeric_scale: Option<i32>,
    /// `DATETIME_PRECISION`: the scale of a date or time type.
    datetime_precision: Option<i16>,
    /// `CHARACTER_SET_NAME`: [`CHARACTER_SET_ISO_1`] or [`CHARACTER_SET_UNICODE`].
    character_set_name: Option<&'static str>,
    /// `COLLATION_NAME`: the collation of a character column.
    collation_name: Option<&'static str>,
}

/// What `INFORMATION_SCHEMA.COLUMNS` publishes about `ty`.
///
/// The unit test `the_type_facts_are_those_of_information_schema` restates the published
/// rows, type by type.
fn type_facts(ty: &SqlType) -> TypeFacts {
    let mut facts = TypeFacts {
        data_type: ty.name(),
        character_maximum_length: None,
        character_octet_length: None,
        numeric_precision: None,
        numeric_precision_radix: None,
        numeric_scale: None,
        datetime_precision: None,
        character_set_name: None,
        collation_name: None,
    };
    match *ty {
        // `bit` and `uniqueidentifier` publish the name alone.
        SqlType::Bit | SqlType::UniqueIdentifier => {}
        SqlType::TinyInt => facts.exact(3, 0),
        SqlType::SmallInt => facts.exact(5, 0),
        SqlType::Int => facts.exact(10, 0),
        SqlType::BigInt => facts.exact(19, 0),
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            facts.exact(precision, i32::from(scale));
        }
        SqlType::Float => facts.approximate(53),
        SqlType::Real => facts.approximate(24),
        SqlType::Money => facts.exact(19, 4),
        SqlType::SmallMoney => facts.exact(10, 4),
        SqlType::Char(len) | SqlType::VarChar(len) => facts.character(len, 1, CHARACTER_SET_ISO_1),
        SqlType::NChar(len) | SqlType::NVarChar(len) => {
            facts.character(len, 2, CHARACTER_SET_UNICODE);
        }
        SqlType::Binary(len) | SqlType::VarBinary(len) => facts.binary(len),
        SqlType::Date | SqlType::SmallDateTime => facts.moment(0),
        SqlType::DateTime => facts.moment(3),
        SqlType::Time(scale) | SqlType::DateTime2(scale) | SqlType::DateTimeOffset(scale) => {
            facts.moment(i16::from(scale));
        }
    }
    facts
}

impl TypeFacts {
    /// An exact numeric: its precision, radix 10, its scale.
    fn exact(&mut self, precision: u8, scale: i32) {
        self.numeric_precision = Some(precision);
        self.numeric_precision_radix = Some(10);
        self.numeric_scale = Some(scale);
    }

    /// An approximate numeric: its precision in bits, radix 2, and no scale.
    fn approximate(&mut self, precision: u8) {
        self.numeric_precision = Some(precision);
        self.numeric_precision_radix = Some(2);
    }

    /// A character type: its length, its length in bytes, its character set and its collation.
    fn character(&mut self, len: Len, bytes_per_unit: i32, character_set: &'static str) {
        self.binary(len);
        self.character_octet_length = self.character_octet_length.map(|length| {
            if length < 0 {
                length
            } else {
                length * bytes_per_unit
            }
        });
        self.character_set_name = Some(character_set);
        self.collation_name = Some(DEFAULT_COLLATION_NAME);
    }

    /// A binary type: its length, in units and in bytes, `-1` for `(max)`.
    fn binary(&mut self, len: Len) {
        let length = match len {
            Len::Max => -1,
            Len::Fixed(n) => i32::from(n),
        };
        self.character_maximum_length = Some(length);
        self.character_octet_length = Some(length);
    }

    /// A date or time type: its `DATETIME_PRECISION`.
    fn moment(&mut self, precision: i16) {
        self.datetime_precision = Some(precision);
    }
}

/// The `int` a [`DbId`] is published as, the two per-database views reading internal tables that
/// hold the databases side by side.
///
/// # Errors
///
/// [`InternalError::Bug`], as `bootstrap.rs` does on the same value.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "INFORMATION_SCHEMA: database id {id} does not fit in an int"
        ))
        .into()
    })
}

/// The columns of [`TABLES_TABLE`], in the order of [`tables_columns`].
///
/// The nullability declared on a column is the published one of the view column it feeds
/// (`the_declared_nullability_is_the_published_one`).
fn tables_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("table_schema", SYSNAME, true),
        column("table_name", SYSNAME, false),
        column("table_type", SqlType::VarChar(Len::Fixed(10)), true),
    ]
}

/// The columns of [`COLUMNS_TABLE`], in the order of [`columns_columns`]. Same rule on
/// nullability as [`tables_table_columns`].
fn columns_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("database_id", SqlType::Int, false),
        column("table_schema", SYSNAME, true),
        column("table_name", SYSNAME, false),
        column("column_name", SYSNAME, true),
        column("ordinal_position", SqlType::Int, true),
        column("column_default", SqlType::NVarChar(Len::Fixed(4000)), true),
        column("is_nullable", SqlType::VarChar(Len::Fixed(3)), true),
        column("data_type", SYSNAME, true),
        column("character_maximum_length", SqlType::Int, true),
        column("character_octet_length", SqlType::Int, true),
        column("numeric_precision", SqlType::TinyInt, true),
        column("numeric_precision_radix", SqlType::SmallInt, true),
        column("numeric_scale", SqlType::Int, true),
        column("datetime_precision", SqlType::SmallInt, true),
        column("character_set_name", SYSNAME, true),
        column("collation_name", SYSNAME, true),
    ]
}

/// The same view installed in the four system databases, `INFORMATION_SCHEMA.<name>` in each of
/// them.
///
/// The three views filter on `DB_ID()`, so the four definitions share their text (unit test
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

/// The T-SQL text of a view: its select list, the internal table it reads in `master`, and its
/// filter.
///
/// Same layout as `views/sys_core.rs` and `views/sys_tables.rs`: one select item per line, an
/// item whose expression is its own name written bare, the others as `<expression> AS <name>`,
/// so the name of a column is the last identifier of its item. The column names of these three
/// views are upper case while an internal column is lower case, so each item is written with
/// its `AS` (unit test `every_select_item_names_its_column`). The three files each hold their
/// own copy of this helper.
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

/// A number the view publishes, [`Value::Null`] where it publishes `NULL`.
fn number(value: Option<Value>) -> Value {
    value.unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_parser::{Expr, Literal, Span};
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};

    use super::*;
    use crate::bootstrap::{SYSTEM_SCHEMAS, internal_table_id};
    use crate::catalog::Catalog;
    use crate::def::{ColumnDef, TableDef};
    use crate::ids::ColumnId;
    use crate::meta::IdentitySpec;
    use crate::table;

    /// The ordered column names of `INFORMATION_SCHEMA.TABLES` in SQL Server 2022. Frozen
    /// here so that a change of the text of the view has to face the list again.
    const PUBLISHED_TABLES_COLUMNS: [&str; 4] =
        ["TABLE_CATALOG", "TABLE_SCHEMA", "TABLE_NAME", "TABLE_TYPE"];

    /// The ordered column names of `INFORMATION_SCHEMA.COLUMNS`.
    const PUBLISHED_COLUMNS_COLUMNS: [&str; 23] = [
        "TABLE_CATALOG",
        "TABLE_SCHEMA",
        "TABLE_NAME",
        "COLUMN_NAME",
        "ORDINAL_POSITION",
        "COLUMN_DEFAULT",
        "IS_NULLABLE",
        "DATA_TYPE",
        "CHARACTER_MAXIMUM_LENGTH",
        "CHARACTER_OCTET_LENGTH",
        "NUMERIC_PRECISION",
        "NUMERIC_PRECISION_RADIX",
        "NUMERIC_SCALE",
        "DATETIME_PRECISION",
        "CHARACTER_SET_CATALOG",
        "CHARACTER_SET_SCHEMA",
        "CHARACTER_SET_NAME",
        "COLLATION_CATALOG",
        "COLLATION_SCHEMA",
        "COLLATION_NAME",
        "DOMAIN_CATALOG",
        "DOMAIN_SCHEMA",
        "DOMAIN_NAME",
    ];

    /// The ordered column names of `INFORMATION_SCHEMA.SCHEMATA`.
    const PUBLISHED_SCHEMATA_COLUMNS: [&str; 6] = [
        "CATALOG_NAME",
        "SCHEMA_NAME",
        "SCHEMA_OWNER",
        "DEFAULT_CHARACTER_SET_CATALOG",
        "DEFAULT_CHARACTER_SET_SCHEMA",
        "DEFAULT_CHARACTER_SET_NAME",
    ];

    /// `(view, internal column, nullable)` as `sys.dm_exec_describe_first_result_set` publishes
    /// it (column `is_nullable`), for the 18 columns the two tables of this file are read
    /// through: a view column and the internal column it reads share their nullability.
    /// `SCHEMATA` reads a table of `bootstrap.rs`, whose `name` column is `NOT NULL` against
    /// the nullable `SCHEMA_OWNER` (module documentation).
    const PUBLISHED_NULLABILITY: [(&str, &str, bool); 18] = [
        ("TABLES", "table_schema", true),
        ("TABLES", "table_name", false),
        ("TABLES", "table_type", true),
        ("COLUMNS", "table_schema", true),
        ("COLUMNS", "table_name", false),
        ("COLUMNS", "column_name", true),
        ("COLUMNS", "ordinal_position", true),
        ("COLUMNS", "column_default", true),
        ("COLUMNS", "is_nullable", true),
        ("COLUMNS", "data_type", true),
        ("COLUMNS", "character_maximum_length", true),
        ("COLUMNS", "character_octet_length", true),
        ("COLUMNS", "numeric_precision", true),
        ("COLUMNS", "numeric_precision_radix", true),
        ("COLUMNS", "numeric_scale", true),
        ("COLUMNS", "datetime_precision", true),
        ("COLUMNS", "character_set_name", true),
        ("COLUMNS", "collation_name", true),
    ];

    /// One row per type: the type, then the nine values of [`TypeFacts`] `COLUMNS` publishes
    /// for a column of that type.
    #[allow(clippy::type_complexity)] // one tuple per row, read by a single test
    const EXPECTED_TYPE_FACTS: [(
        SqlType,
        &str,
        Option<i32>,
        Option<i32>,
        Option<u8>,
        Option<i16>,
        Option<i32>,
        Option<i16>,
        Option<&str>,
        Option<&str>,
    ); 29] = [
        (
            SqlType::Bit,
            "bit",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            SqlType::TinyInt,
            "tinyint",
            None,
            None,
            Some(3),
            Some(10),
            Some(0),
            None,
            None,
            None,
        ),
        (
            SqlType::SmallInt,
            "smallint",
            None,
            None,
            Some(5),
            Some(10),
            Some(0),
            None,
            None,
            None,
        ),
        (
            SqlType::Int,
            "int",
            None,
            None,
            Some(10),
            Some(10),
            Some(0),
            None,
            None,
            None,
        ),
        (
            SqlType::BigInt,
            "bigint",
            None,
            None,
            Some(19),
            Some(10),
            Some(0),
            None,
            None,
            None,
        ),
        (
            SqlType::Decimal {
                precision: 9,
                scale: 2,
            },
            "decimal",
            None,
            None,
            Some(9),
            Some(10),
            Some(2),
            None,
            None,
            None,
        ),
        (
            SqlType::Numeric {
                precision: 18,
                scale: 4,
            },
            "numeric",
            None,
            None,
            Some(18),
            Some(10),
            Some(4),
            None,
            None,
            None,
        ),
        (
            SqlType::Float,
            "float",
            None,
            None,
            Some(53),
            Some(2),
            None,
            None,
            None,
            None,
        ),
        (
            SqlType::Real,
            "real",
            None,
            None,
            Some(24),
            Some(2),
            None,
            None,
            None,
            None,
        ),
        (
            SqlType::Money,
            "money",
            None,
            None,
            Some(19),
            Some(10),
            Some(4),
            None,
            None,
            None,
        ),
        (
            SqlType::SmallMoney,
            "smallmoney",
            None,
            None,
            Some(10),
            Some(10),
            Some(4),
            None,
            None,
            None,
        ),
        (
            SqlType::Char(Len::Fixed(10)),
            "char",
            Some(10),
            Some(10),
            None,
            None,
            None,
            None,
            Some("iso_1"),
            Some(DEFAULT_COLLATION_NAME),
        ),
        (
            SqlType::VarChar(Len::Fixed(30)),
            "varchar",
            Some(30),
            Some(30),
            None,
            None,
            None,
            None,
            Some("iso_1"),
            Some(DEFAULT_COLLATION_NAME),
        ),
        (
            SqlType::VarChar(Len::Max),
            "varchar",
            Some(-1),
            Some(-1),
            None,
            None,
            None,
            None,
            Some("iso_1"),
            Some(DEFAULT_COLLATION_NAME),
        ),
        (
            SqlType::NChar(Len::Fixed(10)),
            "nchar",
            Some(10),
            Some(20),
            None,
            None,
            None,
            None,
            Some("UNICODE"),
            Some(DEFAULT_COLLATION_NAME),
        ),
        (
            SqlType::NVarChar(Len::Fixed(20)),
            "nvarchar",
            Some(20),
            Some(40),
            None,
            None,
            None,
            None,
            Some("UNICODE"),
            Some(DEFAULT_COLLATION_NAME),
        ),
        (
            SqlType::NVarChar(Len::Max),
            "nvarchar",
            Some(-1),
            Some(-1),
            None,
            None,
            None,
            None,
            Some("UNICODE"),
            Some(DEFAULT_COLLATION_NAME),
        ),
        (
            SqlType::Binary(Len::Fixed(8)),
            "binary",
            Some(8),
            Some(8),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            SqlType::VarBinary(Len::Fixed(16)),
            "varbinary",
            Some(16),
            Some(16),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            SqlType::VarBinary(Len::Max),
            "varbinary",
            Some(-1),
            Some(-1),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            SqlType::Date,
            "date",
            None,
            None,
            None,
            None,
            None,
            Some(0),
            None,
            None,
        ),
        (
            SqlType::Time(0),
            "time",
            None,
            None,
            None,
            None,
            None,
            Some(0),
            None,
            None,
        ),
        (
            SqlType::Time(7),
            "time",
            None,
            None,
            None,
            None,
            None,
            Some(7),
            None,
            None,
        ),
        (
            SqlType::DateTime,
            "datetime",
            None,
            None,
            None,
            None,
            None,
            Some(3),
            None,
            None,
        ),
        (
            SqlType::SmallDateTime,
            "smalldatetime",
            None,
            None,
            None,
            None,
            None,
            Some(0),
            None,
            None,
        ),
        (
            SqlType::DateTime2(0),
            "datetime2",
            None,
            None,
            None,
            None,
            None,
            Some(0),
            None,
            None,
        ),
        (
            SqlType::DateTime2(7),
            "datetime2",
            None,
            None,
            None,
            None,
            None,
            Some(7),
            None,
            None,
        ),
        (
            SqlType::DateTimeOffset(3),
            "datetimeoffset",
            None,
            None,
            None,
            None,
            None,
            Some(3),
            None,
            None,
        ),
        (
            SqlType::UniqueIdentifier,
            "uniqueidentifier",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    ];

    /// The view texts this file describes, as `(name, definition)` of the copies installed in
    /// `master`.
    fn definitions() -> Vec<(String, String)> {
        internal_tables()
            .into_iter()
            .flat_map(|table| table.views)
            .filter(|view| view.name.database == "master")
            .map(|view| (view.name.name, view.definition))
            .collect()
    }

    /// The text of the view `INFORMATION_SCHEMA.<name>`, as [`definitions`] reads it.
    fn definition_of(name: &str) -> String {
        definitions()
            .into_iter()
            .find(|(view, _)| view == name)
            .unwrap_or_else(|| panic!("INFORMATION_SCHEMA.{name} is described here"))
            .1
    }

    /// The select items of a view text, as `(expression, name)`.
    fn select_items(definition: &str) -> Vec<(String, String)> {
        let select = definition
            .split("\n  FROM ")
            .next()
            .expect("the text has a FROM clause")
            .strip_prefix("SELECT ")
            .expect("the text starts with SELECT");
        select
            .split(",\n       ")
            .map(|item| {
                let (expression, name) = item
                    .rsplit_once(" AS ")
                    .unwrap_or_else(|| panic!("{item} names its column"));
                (expression.to_owned(), name.to_owned())
            })
            .collect()
    }

    /// The column names the text of a view publishes, in order.
    fn published_columns(definition: &str) -> Vec<String> {
        select_items(definition)
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    /// The internal table of that name, as [`internal_tables`] describes it.
    fn internal_table(name: &str) -> InternalTableDef {
        internal_tables()
            .into_iter()
            .find(|table| table.name == name)
            .unwrap_or_else(|| panic!("{name} is described here"))
    }

    /// The declared columns of the internal table the view `view` reads; for `SCHEMATA`, the
    /// four columns `bootstrap.rs` declares on its table of the schemas.
    fn columns_of(view: &str) -> Vec<InternalColumnDef> {
        match view {
            "TABLES" => tables_table_columns(),
            "COLUMNS" => columns_table_columns(),
            "SCHEMATA" => vec![
                column("database_id", SqlType::Int, false),
                column("schema_id", SqlType::Int, false),
                column("name", SYSNAME, false),
                column("principal_id", SqlType::Int, true),
            ],
            other => panic!("{other} is not a view of this file"),
        }
    }

    /// A catalogue bootstrapped on a fresh `MemoryStorage`.
    fn bootstrapped() -> Catalog {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        Catalog::bootstrap(storage, txn).expect("bootstrap of a fresh storage")
    }

    /// A column of a [`TableDef`], with its type, its `DEFAULT` and its `IDENTITY` property.
    fn column_def(
        name: &str,
        ty: SqlType,
        nullable: bool,
        default: Option<Expr>,
        identity: bool,
    ) -> ColumnDef {
        ColumnDef {
            name: name.to_owned(),
            ty: TypeInfo::new(ty, nullable),
            default,
            identity: identity.then(IdentitySpec::default),
            computed: None,
        }
    }

    /// The tables `catalog` holds once `master.dbo.<name>` has been created and committed.
    fn created(catalog: &Catalog, name: &str, columns: Vec<ColumnDef>) -> Vec<TableMeta> {
        let def = TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns,
            constraints: Vec::new(),
        };
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        catalog.create_table(&handle, &def).expect("create_table");
        catalog.txn.commit(handle).expect("commit");
        let store = table::store(catalog);
        store.live().cloned().collect()
    }

    /// The text of a value, `None` for `NULL`.
    fn string(value: &Value) -> Option<String> {
        match value {
            Value::Null => None,
            Value::String(string) => Some(string.text.clone()),
            other => panic!("{other:?} is not a string"),
        }
    }

    /// The value of the column `name` of a row of [`COLUMNS_TABLE`], as text: digits for a
    /// number, the text of a string, `None` for `NULL`.
    fn column_field(row: &Row, name: &str) -> Option<String> {
        let position = columns_table_columns()
            .iter()
            .position(|column| column.name == name)
            .unwrap_or_else(|| panic!("{name} is a column of {COLUMNS_TABLE}"));
        match &row.0[position] {
            Value::Null => None,
            Value::I8(number) => Some(number.to_string()),
            Value::I16(number) => Some(number.to_string()),
            Value::I32(number) => Some(number.to_string()),
            Value::String(string) => Some(string.text.clone()),
            other => panic!("no row of {COLUMNS_TABLE} carries {other:?}"),
        }
    }

    #[test]
    fn information_schema_tables_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("TABLES")),
            PUBLISHED_TABLES_COLUMNS
        );
    }

    #[test]
    fn information_schema_columns_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("COLUMNS")),
            PUBLISHED_COLUMNS_COLUMNS
        );
    }

    #[test]
    fn information_schema_schemata_columns_match_the_published_list() {
        assert_eq!(
            published_columns(&definition_of("SCHEMATA")),
            PUBLISHED_SCHEMATA_COLUMNS
        );
    }

    #[test]
    fn view_definition_is_a_select_without_a_join() {
        for (name, definition) in definitions() {
            assert!(
                definition.starts_with("SELECT "),
                "INFORMATION_SCHEMA.{name}: {definition}"
            );
            assert!(
                !definition.to_ascii_uppercase().contains("JOIN"),
                "INFORMATION_SCHEMA.{name} reads one internal table: {definition}"
            );
            let reads = if name == "SCHEMATA" {
                format!("\n  FROM master.dbo.{SCHEMAS_TABLE}")
            } else {
                "\n  FROM master.dbo.vauban_is_".to_owned()
            };
            assert!(
                definition.contains(&reads),
                "INFORMATION_SCHEMA.{name} reads an internal table of master: {definition}"
            );
        }
    }

    #[test]
    fn the_three_views_filter_on_the_current_database() {
        // The internal tables live in `master` and hold the rows of the four databases side by
        // side, as those of the other files of `views/` do.
        for view in ["TABLES", "COLUMNS", "SCHEMATA"] {
            let definition = definition_of(view);
            assert!(
                definition.ends_with("\n WHERE database_id = DB_ID()"),
                "{definition}"
            );
        }
    }

    #[test]
    fn every_select_item_names_its_column() {
        // The column names are published in upper case and an internal column is lower
        // case, so each item of the three texts carries its `AS`.
        for (name, definition) in definitions() {
            for (expression, column) in select_items(&definition) {
                assert_ne!(expression, column, "INFORMATION_SCHEMA.{name}");
                assert!(
                    column.chars().all(|letter| !letter.is_lowercase()),
                    "INFORMATION_SCHEMA.{name} publishes {column}"
                );
            }
        }
    }

    #[test]
    fn the_column_order_is_the_one_the_constants_name() {
        let tables: Vec<String> = tables_table_columns()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(tables.len(), tables_columns::WIDTH);
        assert_eq!(tables[tables_columns::DATABASE_ID], "database_id");
        assert_eq!(tables[tables_columns::TABLE_SCHEMA], "table_schema");
        assert_eq!(tables[tables_columns::TABLE_NAME], "table_name");
        assert_eq!(tables[tables_columns::TABLE_TYPE], "table_type");

        let columns: Vec<String> = columns_table_columns()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(columns.len(), columns_columns::WIDTH);
        assert_eq!(columns[columns_columns::DATABASE_ID], "database_id");
        assert_eq!(columns[columns_columns::TABLE_SCHEMA], "table_schema");
        assert_eq!(columns[columns_columns::TABLE_NAME], "table_name");
        assert_eq!(columns[columns_columns::COLUMN_NAME], "column_name");
        assert_eq!(
            columns[columns_columns::ORDINAL_POSITION],
            "ordinal_position"
        );
        assert_eq!(columns[columns_columns::COLUMN_DEFAULT], "column_default");
        assert_eq!(columns[columns_columns::IS_NULLABLE], "is_nullable");
        assert_eq!(columns[columns_columns::DATA_TYPE], "data_type");
        assert_eq!(
            columns[columns_columns::CHARACTER_MAXIMUM_LENGTH],
            "character_maximum_length"
        );
        assert_eq!(
            columns[columns_columns::CHARACTER_OCTET_LENGTH],
            "character_octet_length"
        );
        assert_eq!(
            columns[columns_columns::NUMERIC_PRECISION],
            "numeric_precision"
        );
        assert_eq!(
            columns[columns_columns::NUMERIC_PRECISION_RADIX],
            "numeric_precision_radix"
        );
        assert_eq!(columns[columns_columns::NUMERIC_SCALE], "numeric_scale");
        assert_eq!(
            columns[columns_columns::DATETIME_PRECISION],
            "datetime_precision"
        );
        assert_eq!(
            columns[columns_columns::CHARACTER_SET_NAME],
            "character_set_name"
        );
        assert_eq!(columns[columns_columns::COLLATION_NAME], "collation_name");
    }

    #[test]
    fn the_declared_nullability_is_the_published_one() {
        for (view, column, nullable) in PUBLISHED_NULLABILITY {
            let declared = columns_of(view)
                .into_iter()
                .find(|candidate| candidate.name == column)
                .unwrap_or_else(|| panic!("{column} is a column of the table of {view}"));
            assert_eq!(
                declared.ty.nullable, nullable,
                "INFORMATION_SCHEMA.{view}.{column}"
            );
        }
        // `database_id` is the filter of the two per-database views and is published by neither,
        // so nothing is published about it; it is declared `NOT NULL`.
        for view in ["TABLES", "COLUMNS"] {
            let database_id = columns_of(view)
                .into_iter()
                .find(|column| column.name == "database_id")
                .expect("the table of a per-database view carries the filter");
            assert!(!database_id.ty.nullable);
            assert!(!published_columns(&definition_of(view)).contains(&"database_id".to_owned()));
        }
    }

    #[test]
    fn the_literal_columns_are_the_published_values() {
        // The 13 items that read no internal column, sorted by view then column: the three
        // `DB_NAME()` (`TABLE_CATALOG` in two views, `CATALOG_NAME` in the third), the nine
        // `NULL`s and the `iso_1` of `SCHEMATA`. Their values are the ones the module
        // documentation lists.
        let mut literals: Vec<(String, String, String)> = Vec::new();
        for (view, definition) in definitions() {
            let read: Vec<String> = columns_of(&view)
                .into_iter()
                .map(|column| column.name)
                .collect();
            for (expression, column) in select_items(&definition) {
                if !read.contains(&expression) {
                    literals.push((view.clone(), column, expression));
                }
            }
        }
        literals.sort();
        let named = |wanted: &str| -> Vec<(String, String)> {
            literals
                .iter()
                .filter(|(_, _, expression)| expression == wanted)
                .map(|(view, column, _)| (view.clone(), column.clone()))
                .collect()
        };
        assert_eq!(
            named("DB_NAME()"),
            [
                ("COLUMNS", "TABLE_CATALOG"),
                ("SCHEMATA", "CATALOG_NAME"),
                ("TABLES", "TABLE_CATALOG"),
            ]
            .map(|(view, column)| (view.to_owned(), column.to_owned()))
        );
        assert_eq!(
            named("CAST(NULL AS nvarchar(128))"),
            [
                ("COLUMNS", "CHARACTER_SET_CATALOG"),
                ("COLUMNS", "CHARACTER_SET_SCHEMA"),
                ("COLUMNS", "COLLATION_CATALOG"),
                ("COLUMNS", "COLLATION_SCHEMA"),
                ("COLUMNS", "DOMAIN_CATALOG"),
                ("COLUMNS", "DOMAIN_NAME"),
                ("COLUMNS", "DOMAIN_SCHEMA"),
                ("SCHEMATA", "DEFAULT_CHARACTER_SET_CATALOG"),
                ("SCHEMATA", "DEFAULT_CHARACTER_SET_SCHEMA"),
            ]
            .map(|(view, column)| (view.to_owned(), column.to_owned()))
        );
        assert_eq!(
            named("CAST(N'iso_1' AS nvarchar(128))"),
            [(
                "SCHEMATA".to_owned(),
                "DEFAULT_CHARACTER_SET_NAME".to_owned()
            )]
        );
        assert_eq!(literals.len(), 13);
        assert_eq!(DEFAULT_CHARACTER_SET_NAME, "iso_1");
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        let mut copies: Vec<(String, String, String, String)> = Vec::new();
        for table in internal_tables() {
            for view in table.views {
                copies.push((
                    view.name.name,
                    view.name.database,
                    view.name.schema,
                    view.definition,
                ));
            }
        }
        for wanted in ["TABLES", "COLUMNS", "SCHEMATA"] {
            let installed: Vec<&(String, String, String, String)> = copies
                .iter()
                .filter(|(name, _, _, _)| name == wanted)
                .collect();
            assert_eq!(
                installed
                    .iter()
                    .map(|(_, database, schema, _)| (database.clone(), schema.clone()))
                    .collect::<Vec<(String, String)>>(),
                SYSTEM_DATABASES
                    .iter()
                    .map(|database| ((*database).to_owned(), VIEW_SCHEMA.to_owned()))
                    .collect::<Vec<(String, String)>>(),
                "INFORMATION_SCHEMA.{wanted}"
            );
            let texts: Vec<&String> = installed
                .iter()
                .map(|(_, _, _, definition)| definition)
                .collect();
            assert!(
                texts.windows(2).all(|pair| pair[0] == pair[1]),
                "the four copies of {wanted} share their text: {texts:?}"
            );
        }
        assert_eq!(copies.len(), 3 * SYSTEM_DATABASES.len());
    }

    #[test]
    fn the_schemata_view_reads_the_table_of_the_schemas() {
        // The view travels with `TABLES_TABLE` and reads the table `bootstrap.rs` filled, whose
        // rows are the three schemas of `SYSTEM_SCHEMAS` per database. `SCHEMA_NAME` and
        // `SCHEMA_OWNER` read its `name` column, the owner of a schema being its own name in
        // a fresh database.
        let schemata = definition_of("SCHEMATA");
        assert!(
            schemata.contains(&format!("\n  FROM master.dbo.{SCHEMAS_TABLE}")),
            "{schemata}"
        );
        assert_eq!(
            select_items(&schemata)
                .into_iter()
                .filter(|(expression, _)| expression == "name")
                .map(|(_, column)| column)
                .collect::<Vec<String>>(),
            ["SCHEMA_NAME", "SCHEMA_OWNER"]
        );
        assert!(
            internal_tables()
                .iter()
                .all(|table| table.name != "vauban_is_schemata")
        );

        // The rows the view reads, read from `storage` after a bootstrap: three schemas in each
        // of the four system databases.
        let catalog = bootstrapped();
        let table = internal_table_id(&catalog, SCHEMAS_TABLE)
            .expect("internal_table_id")
            .expect("the bootstrap created the table of the schemas");
        let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = catalog.txn.statement_snapshot(&handle);
        let mut names: Vec<String> = catalog
            .storage
            .scan(&snapshot, table)
            .expect("scan")
            .map(|row| match &row.expect("row").1.0[2] {
                Value::String(name) => name.text.clone(),
                other => panic!("the name column carries {other:?}"),
            })
            .collect();
        catalog.txn.commit(handle).expect("commit");
        names.sort();
        names.dedup();
        let mut expected: Vec<String> = SYSTEM_SCHEMAS
            .iter()
            .map(|(schema, _, _)| (*schema).to_owned())
            .collect();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn the_two_internal_tables_hold_no_row_at_bootstrap() {
        // The rows of a table and of its columns are built from the `*Meta` of the catalogue,
        // and a fresh instance holds no user table. SQL Server answers 0 to `SELECT COUNT(*)
        // FROM INFORMATION_SCHEMA.TABLES` and to the same query on `COLUMNS` in a fresh
        // database, which is what these two empty vectors publish.
        for name in [TABLES_TABLE, COLUMNS_TABLE] {
            assert!(internal_table(name).rows.is_empty());
        }
    }

    #[test]
    fn information_schema_tables_lists_user_table() {
        let catalog = bootstrapped();
        let tables = created(
            &catalog,
            "t",
            vec![column_def("a", SqlType::Int, false, None, false)],
        );
        let rows = table_rows(&tables).expect("table_rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            string(&row.0[tables_columns::TABLE_NAME]),
            Some("t".to_owned())
        );
        assert_eq!(
            string(&row.0[tables_columns::TABLE_SCHEMA]),
            Some("dbo".to_owned())
        );
        assert_eq!(
            string(&row.0[tables_columns::TABLE_TYPE]),
            Some("BASE TABLE".to_owned())
        );
        // `master` is database 1 on a fresh instance (`bootstrap.rs`).
        assert_eq!(row.0[tables_columns::DATABASE_ID], Value::I32(1));
        assert_eq!(row.0.len(), tables_columns::WIDTH);
    }

    #[test]
    fn information_schema_columns_nullable() {
        // `YES` and `NO`, the two published words (`id int NOT NULL` gives `NO`,
        // `label varchar(30) NULL` gives `YES`), not `Y` and `N`.
        let catalog = bootstrapped();
        let tables = created(
            &catalog,
            "t",
            vec![
                column_def("id", SqlType::Int, false, None, false),
                column_def("label", SqlType::VarChar(Len::Fixed(30)), true, None, false),
            ],
        );
        let rows = column_rows(&tables).expect("column_rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(column_field(&rows[0], "is_nullable"), Some("NO".to_owned()));
        assert_eq!(
            column_field(&rows[1], "is_nullable"),
            Some("YES".to_owned())
        );
        assert_eq!(IS_NULLABLE_YES, "YES");
        assert_eq!(IS_NULLABLE_NO, "NO");
    }

    #[test]
    fn the_column_rows_of_a_five_column_table_carry_its_values() {
        // A five-column table and the 13 values `COLUMNS` publishes for each of its columns,
        // `TABLE_CATALOG` aside (the view reads `DB_NAME()`).
        let catalog = bootstrapped();
        let tables = created(
            &catalog,
            "columns_target",
            vec![
                column_def("id", SqlType::Int, false, None, false),
                column_def("label", SqlType::VarChar(Len::Fixed(30)), true, None, false),
                column_def(
                    "amount",
                    SqlType::Decimal {
                        precision: 9,
                        scale: 2,
                    },
                    true,
                    None,
                    false,
                ),
                column_def("note", SqlType::NVarChar(Len::Max), true, None, false),
                column_def("made_on", SqlType::DateTime2(3), true, None, false),
            ],
        );
        let rows = column_rows(&tables).expect("column_rows");
        let published: [[Option<&str>; 13]; 5] = [
            [
                Some("dbo"),
                Some("columns_target"),
                Some("id"),
                Some("1"),
                None,
                Some("NO"),
                Some("int"),
                None,
                None,
                Some("10"),
                Some("10"),
                Some("0"),
                None,
            ],
            [
                Some("dbo"),
                Some("columns_target"),
                Some("label"),
                Some("2"),
                None,
                Some("YES"),
                Some("varchar"),
                Some("30"),
                Some("30"),
                None,
                None,
                None,
                None,
            ],
            [
                Some("dbo"),
                Some("columns_target"),
                Some("amount"),
                Some("3"),
                None,
                Some("YES"),
                Some("decimal"),
                None,
                None,
                Some("9"),
                Some("10"),
                Some("2"),
                None,
            ],
            [
                Some("dbo"),
                Some("columns_target"),
                Some("note"),
                Some("4"),
                None,
                Some("YES"),
                Some("nvarchar"),
                Some("-1"),
                Some("-1"),
                None,
                None,
                None,
                None,
            ],
            [
                Some("dbo"),
                Some("columns_target"),
                Some("made_on"),
                Some("5"),
                None,
                Some("YES"),
                Some("datetime2"),
                None,
                None,
                None,
                None,
                None,
                Some("3"),
            ],
        ];
        let read = [
            "table_schema",
            "table_name",
            "column_name",
            "ordinal_position",
            "column_default",
            "is_nullable",
            "data_type",
            "character_maximum_length",
            "character_octet_length",
            "numeric_precision",
            "numeric_precision_radix",
            "numeric_scale",
            "datetime_precision",
        ];
        assert_eq!(rows.len(), published.len());
        for (row, expected) in rows.iter().zip(published) {
            for (name, value) in read.iter().zip(expected) {
                assert_eq!(
                    column_field(row, name).as_deref(),
                    value,
                    "{name} of {:?}",
                    column_field(row, "column_name")
                );
            }
        }
        // The two character-set columns, with a value on the `varchar` and `nvarchar` rows
        // and `NULL` on the `int` row.
        assert_eq!(
            column_field(&rows[1], "character_set_name"),
            Some("iso_1".to_owned())
        );
        assert_eq!(
            column_field(&rows[3], "character_set_name"),
            Some("UNICODE".to_owned())
        );
        assert_eq!(
            column_field(&rows[1], "collation_name").as_deref(),
            Some(DEFAULT_COLLATION_NAME)
        );
        assert_eq!(column_field(&rows[0], "collation_name"), None);
    }

    #[test]
    fn the_type_facts_are_those_of_information_schema() {
        for (
            ty,
            data_type,
            character_maximum_length,
            character_octet_length,
            numeric_precision,
            numeric_precision_radix,
            numeric_scale,
            datetime_precision,
            character_set_name,
            collation_name,
        ) in EXPECTED_TYPE_FACTS
        {
            assert_eq!(
                type_facts(&ty),
                TypeFacts {
                    data_type,
                    character_maximum_length,
                    character_octet_length,
                    numeric_precision,
                    numeric_precision_radix,
                    numeric_scale,
                    datetime_precision,
                    character_set_name,
                    collation_name,
                },
                "{}",
                ty.declaration()
            );
        }
        assert_eq!(EXPECTED_TYPE_FACTS.len(), 29);
    }

    #[test]
    fn the_table_type_of_a_user_table_is_base_table() {
        // `TABLES` publishes `BASE TABLE` on a table and `VIEW` on a view; the catalogue holds
        // no user view, so each row built here is a `BASE TABLE`
        // (`information_schema_tables_lists_user_table`).
        assert_eq!(BASE_TABLE_TYPE, "BASE TABLE");
        assert_eq!(VIEW_TABLE_TYPE, "VIEW");
        let catalog = bootstrapped();
        let tables = created(
            &catalog,
            "t",
            vec![column_def("a", SqlType::Int, false, None, false)],
        );
        let types: Vec<Option<String>> = table_rows(&tables)
            .expect("table_rows")
            .iter()
            .map(|row| string(&row.0[tables_columns::TABLE_TYPE]))
            .collect();
        assert_eq!(types, vec![Some(BASE_TABLE_TYPE.to_owned())]);
    }

    #[test]
    fn the_column_default_is_the_parenthesised_text_of_the_expression() {
        // In SQL Server, `DEFAULT 0` is stored `((0))`, `DEFAULT 'x'` is `('x')`,
        // `DEFAULT (1 + 2)` is `((1)+(2))`, and an `IDENTITY` column publishes `NULL`. This
        // file writes one pair of parentheses around the text of the expression, so the first
        // and the third differ.
        let catalog = bootstrapped();
        let zero = Expr::Literal(Literal::Integer("0".to_owned()), Span::EMPTY);
        let tables = created(
            &catalog,
            "t",
            vec![
                column_def("a", SqlType::Int, false, Some(zero), false),
                column_def("b", SqlType::Int, false, None, true),
                column_def("c", SqlType::Int, true, None, false),
            ],
        );
        let rows = column_rows(&tables).expect("column_rows");
        assert_eq!(
            column_field(&rows[0], "column_default"),
            Some("(0)".to_owned())
        );
        assert_eq!(column_field(&rows[1], "column_default"), None);
        assert_eq!(column_field(&rows[2], "column_default"), None);
    }

    #[test]
    fn a_table_of_another_schema_publishes_that_schema() {
        // `TABLE_SCHEMA` is a name and not an identifier, so a schema the bootstrap does not
        // know needs no number here, where `sys.objects` writes `0` (`views/sys_tables.rs`).
        let catalog = bootstrapped();
        let mut tables = created(
            &catalog,
            "t",
            vec![column_def("a", SqlType::Int, false, None, false)],
        );
        tables[0].schema = "other".to_owned();
        let rows = table_rows(&tables).expect("table_rows");
        assert_eq!(
            string(&rows[0].0[tables_columns::TABLE_SCHEMA]),
            Some("other".to_owned())
        );
    }

    #[test]
    fn the_ordinal_position_is_renumbered_where_the_column_id_keeps_its_value() {
        // On `dbo.t (a, b, c)`, `DROP COLUMN b` then `ADD d` leaves `sys.columns.column_id`
        // 1, 3, 4 and `ORDINAL_POSITION` 1, 2, 3. The
        // `TableMeta` below carries that hole — `ColumnMeta::ordinal` is the position in the row
        // of `storage`, `ColumnMeta::id` the identifier that survives a drop (`meta.rs`).
        let catalog = bootstrapped();
        let mut tables = created(
            &catalog,
            "t",
            vec![
                column_def("a", SqlType::Int, true, None, false),
                column_def("b", SqlType::Int, true, None, false),
                column_def("c", SqlType::Int, true, None, false),
            ],
        );
        let columns = &mut tables[0].columns;
        columns.remove(1);
        columns.push(ColumnMeta {
            id: ColumnId(4),
            name: "d".to_owned(),
            ty: TypeInfo::new(SqlType::Int, true),
            ordinal: 2,
            default: None,
            identity: None,
            computed: None,
        });
        columns[1].ordinal = 1;
        assert_eq!(
            columns
                .iter()
                .map(|column| (column.name.clone(), column.id.0))
                .collect::<Vec<(String, i32)>>(),
            [("a", 1), ("c", 3), ("d", 4)].map(|(name, id)| (name.to_owned(), id))
        );

        let rows = column_rows(&tables).expect("column_rows");
        assert_eq!(
            rows.iter()
                .map(|row| (
                    column_field(row, "column_name"),
                    column_field(row, "ordinal_position")
                ))
                .collect::<Vec<(Option<String>, Option<String>)>>(),
            [("a", "1"), ("c", "2"), ("d", "3")]
                .map(|(name, position)| (Some(name.to_owned()), Some(position.to_owned())))
        );
    }

    #[test]
    fn the_internal_tables_are_not_published_as_user_tables() {
        // The two `vauban_is_*` tables and the `vauban_sys_*` ones are out of
        // `INFORMATION_SCHEMA.TABLES`: its rows are those of the tables the store of `table.rs`
        // holds, which a bootstrap leaves empty.
        let catalog = bootstrapped();
        let store = table::store(&catalog);
        let tables: Vec<TableMeta> = store.live().cloned().collect();
        drop(store);
        assert!(tables.is_empty());
        assert!(table_rows(&tables).expect("table_rows").is_empty());
        assert!(column_rows(&tables).expect("column_rows").is_empty());
    }
}
