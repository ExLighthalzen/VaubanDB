//! `sys.database_principals`, `sys.server_principals`, `sys.database_role_members` and
//! `sys.server_role_members`.
//!
//! # The shape of the views
//!
//! The column list of each view — names, order, types — is the one SQL Server 2022
//! publishes: `sys.database_principals` 16 columns, `sys.server_principals` 14,
//! `sys.database_role_members` and `sys.server_role_members` 2 each. The unit test
//! `the_columns_of_the_four_views_are_the_published_ones` freezes the four vectors of names
//! and compares them with the text this file builds.
//!
//! # Two internal tables, one filter column apiece
//!
//! [`PRINCIPALS_TABLE`] carries a database row and a server row side by side, and
//! [`ROLE_MEMBERS_TABLE`] a database membership couple next to a server one; `kind`
//! (respectively `scope`) tells the two apart, and each view's text filters on it — the same
//! technique `views/dm_exec.rs` uses to read `sys.dm_exec_sessions` and `sys.dm_exec_requests`
//! off a single table (module documentation of that file).
//!
//! # Fixed rows, `name` included
//!
//! No `CREATE LOGIN`, `CREATE USER` or `ALTER ROLE … ADD MEMBER` writes here yet: the rows are
//! the ones a fresh installation already carries. `sys.database_principals` holds `public`,
//! `dbo`, `guest`, `INFORMATION_SCHEMA`, `sys` and the eight fixed database roles beneath
//! them (`server_functions.rs` of the `compat` crate names the same nine, `public` included).
//! `sys.server_principals` holds `sa` and the eight fixed server roles beneath `public`. The
//! two membership views hold one couple each: `dbo` in `db_owner`, `sa` in `sysadmin`.
//!
//! `name` is the one column [`PRINCIPALS_TABLE`] does not carry as a value: it is a `CASE` on
//! `principal_id`, built by [`name_case`] from the pairs above and cast to the published
//! `sysname` width, the same technique `sys_core.rs` uses for
//! `sys.databases.snapshot_isolation_state_desc`. `principal_id` already tells every row
//! apart, so a `name` column here would only repeat one of the fixed strings above — `dbo`,
//! `sys` and `INFORMATION_SCHEMA` among them, three names a database of this catalogue
//! already carries as schemas (`bootstrap::SYSTEM_SCHEMAS`).
//!
//! # Columns without a matching fact
//!
//! `create_date`, `modify_date`, `credential_id`, `default_language_lcid` and `tenant_id` are
//! `CAST(NULL AS …)` literals in the view that carries them: nothing in [`PRINCIPALS_TABLE`]
//! backs them. `authentication_type` and
//! `authentication_type_desc` are the literals `0` and `N'NONE'` on a row of
//! `sys.database_principals`, `dbo` included: SQL Server ties `dbo` to the login that owns
//! the database, a link this catalogue does not keep. The `sid` of a row of
//! `sys.database_principals` is `NULL`, unbacked by a column of [`PRINCIPALS_TABLE`] for that
//! kind: SQL Server derives it from the database's own identifier, which this catalogue does
//! not mint. A row of `sys.server_principals` fares better, its `sid` being one well-known byte
//! per fixed principal (`sa` is `0x01`, [`crate::bootstrap::SA_OWNER_SID`]; the others follow
//! their `principal_id`), read straight from a column of the table.
//!
//! # Execution
//!
//! What this file produces is the shape of the two internal tables, the rows they carry and
//! the text of the four definitions, checked by the tests of `views/sys_principals.rs` and
//! `tests/sys_principals.rs`; the binder expands the text in place of the name.

use vauban_storage::Row;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{SA_OWNER_SID, SYSTEM_DATABASES};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::QualifiedName;

/// Internal table behind `sys.database_principals` and `sys.server_principals`, one row per
/// principal, told apart by [`principals_columns::KIND`].
pub(crate) const PRINCIPALS_TABLE: &str = "vauban_sys_principals";

/// Internal table behind `sys.database_role_members` and `sys.server_role_members`, one row
/// per membership couple, told apart by [`role_members_columns::SCOPE`].
pub(crate) const ROLE_MEMBERS_TABLE: &str = "vauban_sys_role_members";

/// Value of [`principals_columns::KIND`] for a row of `sys.database_principals`.
const DATABASE_KIND: &str = "D";

/// Value of [`principals_columns::KIND`] for a row of `sys.server_principals`.
const SERVER_KIND: &str = "S";

const VIEW_SCHEMA: &str = "sys";
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));
const KIND_TYPE: SqlType = SqlType::Char(Len::Fixed(1));

/// `(principal_id, name)` of each row of `sys.database_principals`, in the order
/// [`principal_rows`] writes them; read by [`name_case`] to build the `name` column of the
/// view (module documentation, section "Fixed rows, `name` included").
const DATABASE_PRINCIPAL_NAMES: [(i32, &str); 13] = [
    (0, "public"),
    (1, "dbo"),
    (2, "guest"),
    (3, "INFORMATION_SCHEMA"),
    (4, "sys"),
    (16_384, "db_owner"),
    (16_385, "db_accessadmin"),
    (16_386, "db_securityadmin"),
    (16_387, "db_ddladmin"),
    (16_390, "db_datareader"),
    (16_391, "db_datawriter"),
    (16_392, "db_denydatareader"),
    (16_393, "db_denydatawriter"),
];

/// `(principal_id, name)` of each row of `sys.server_principals`, same rule as
/// [`DATABASE_PRINCIPAL_NAMES`].
const SERVER_PRINCIPAL_NAMES: [(i32, &str); 10] = [
    (1, "sa"),
    (2, "public"),
    (3, "sysadmin"),
    (4, "securityadmin"),
    (5, "serveradmin"),
    (6, "setupadmin"),
    (7, "processadmin"),
    (8, "diskadmin"),
    (9, "dbcreator"),
    (10, "bulkadmin"),
];

/// Where each column of [`PRINCIPALS_TABLE`] sits in a [`Row`]. `name` is not one of them
/// (module documentation).
pub(crate) mod principals_columns {
    /// `D` for a row of `sys.database_principals`, `S` for `sys.server_principals`.
    pub(crate) const KIND: usize = 0;
    pub(crate) const PRINCIPAL_ID: usize = 1;
    pub(crate) const SID: usize = 2;
    pub(crate) const TYPE: usize = 3;
    pub(crate) const TYPE_DESC: usize = 4;
    pub(crate) const IS_FIXED_ROLE: usize = 5;
    pub(crate) const OWNING_PRINCIPAL_ID: usize = 6;
    pub(crate) const DEFAULT_SCHEMA_NAME: usize = 7;
    pub(crate) const DEFAULT_DATABASE_NAME: usize = 8;
    pub(crate) const DEFAULT_LANGUAGE_NAME: usize = 9;
    pub(crate) const IS_DISABLED: usize = 10;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 11;
}

/// Where each column of [`ROLE_MEMBERS_TABLE`] sits in a [`Row`].
pub(crate) mod role_members_columns {
    /// `D` for a row of `sys.database_role_members`, `S` for `sys.server_role_members`.
    pub(crate) const SCOPE: usize = 0;
    pub(crate) const ROLE_PRINCIPAL_ID: usize = 1;
    pub(crate) const MEMBER_PRINCIPAL_ID: usize = 2;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 3;
}

/// The select list of `sys.database_principals`: `(column, expression)`, in the published
/// order. `name`'s expression is filled in by [`database_principals_definition`].
const DATABASE_PRINCIPALS_VIEW: [(&str, &str); 16] = [
    ("name", ""),
    ("principal_id", "principal_id"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("default_schema_name", "default_schema_name"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("owning_principal_id", "owning_principal_id"),
    ("sid", "sid"),
    ("is_fixed_role", "is_fixed_role"),
    ("authentication_type", "CAST(0 AS int)"),
    ("authentication_type_desc", "CAST(N'NONE' AS nvarchar(60))"),
    ("default_language_name", "CAST(NULL AS nvarchar(128))"),
    ("default_language_lcid", "CAST(NULL AS int)"),
    ("allow_encrypted_value_modifications", "CAST(0 AS bit)"),
    ("tenant_id", "CAST(NULL AS uniqueidentifier)"),
];
const PUBLISHED_DATABASE_PRINCIPALS_COLUMNS: [&str; 16] = [
    "name",
    "principal_id",
    "type",
    "type_desc",
    "default_schema_name",
    "create_date",
    "modify_date",
    "owning_principal_id",
    "sid",
    "is_fixed_role",
    "authentication_type",
    "authentication_type_desc",
    "default_language_name",
    "default_language_lcid",
    "allow_encrypted_value_modifications",
    "tenant_id",
];

/// The select list of `sys.server_principals`: `(column, expression)`, in the published
/// order. `name`'s expression is filled in by [`server_principals_definition`].
const SERVER_PRINCIPALS_VIEW: [(&str, &str); 14] = [
    ("name", ""),
    ("principal_id", "principal_id"),
    ("sid", "sid"),
    ("type", "type"),
    ("type_desc", "type_desc"),
    ("is_disabled", "is_disabled"),
    ("create_date", "CAST(NULL AS datetime)"),
    ("modify_date", "CAST(NULL AS datetime)"),
    ("default_database_name", "default_database_name"),
    ("default_language_name", "default_language_name"),
    ("credential_id", "CAST(NULL AS int)"),
    ("owning_principal_id", "owning_principal_id"),
    ("is_fixed_role", "is_fixed_role"),
    ("tenant_id", "CAST(NULL AS uniqueidentifier)"),
];
const PUBLISHED_SERVER_PRINCIPALS_COLUMNS: [&str; 14] = [
    "name",
    "principal_id",
    "sid",
    "type",
    "type_desc",
    "is_disabled",
    "create_date",
    "modify_date",
    "default_database_name",
    "default_language_name",
    "credential_id",
    "owning_principal_id",
    "is_fixed_role",
    "tenant_id",
];

/// The select list shared by `sys.database_role_members` and `sys.server_role_members`.
const ROLE_MEMBERS_VIEW: [(&str, &str); 2] = [
    ("role_principal_id", "role_principal_id"),
    ("member_principal_id", "member_principal_id"),
];
const PUBLISHED_ROLE_MEMBERS_COLUMNS: [&str; 2] = ["role_principal_id", "member_principal_id"];

pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: PRINCIPALS_TABLE.to_owned(),
            columns: principals_table_columns(),
            clustered_key: None,
            rows: principal_rows(),
            views: [
                views_of("database_principals", &database_principals_definition()),
                views_of("server_principals", &server_principals_definition()),
            ]
            .concat(),
        },
        InternalTableDef {
            name: ROLE_MEMBERS_TABLE.to_owned(),
            columns: role_members_table_columns(),
            clustered_key: None,
            rows: role_member_rows(),
            views: [
                views_of("database_role_members", &database_role_members_definition()),
                views_of("server_role_members", &server_role_members_definition()),
            ]
            .concat(),
        },
    ]
}

/// A `CAST(CASE principal_id WHEN … THEN N'…' … END AS nvarchar(128))` over `pairs`: the
/// `name` column of a view of this file (module documentation, section "Fixed rows, `name`
/// included").
fn name_case(pairs: &[(i32, &str)]) -> String {
    let mut text = String::from("CAST(CASE principal_id");
    for (id, name) in pairs {
        text.push_str(&format!(" WHEN {id} THEN N'{name}'"));
    }
    text.push_str(" END AS nvarchar(128))");
    text
}

/// [`DATABASE_PRINCIPALS_VIEW`], its `name` expression filled in.
fn database_principals_select_list() -> Vec<(&'static str, String)> {
    let mut items: Vec<(&'static str, String)> = DATABASE_PRINCIPALS_VIEW
        .iter()
        .map(|&(name, expr)| (name, expr.to_owned()))
        .collect();
    items[0].1 = name_case(&DATABASE_PRINCIPAL_NAMES);
    items
}

/// [`SERVER_PRINCIPALS_VIEW`], its `name` expression filled in.
fn server_principals_select_list() -> Vec<(&'static str, String)> {
    let mut items: Vec<(&'static str, String)> = SERVER_PRINCIPALS_VIEW
        .iter()
        .map(|&(name, expr)| (name, expr.to_owned()))
        .collect();
    items[0].1 = name_case(&SERVER_PRINCIPAL_NAMES);
    items
}

fn database_principals_definition() -> String {
    definition(
        &database_principals_select_list(),
        PRINCIPALS_TABLE,
        &format!("kind = '{DATABASE_KIND}'"),
    )
}

fn server_principals_definition() -> String {
    definition(
        &server_principals_select_list(),
        PRINCIPALS_TABLE,
        &format!("kind = '{SERVER_KIND}'"),
    )
}

fn database_role_members_definition() -> String {
    definition_of_static(
        &ROLE_MEMBERS_VIEW,
        ROLE_MEMBERS_TABLE,
        &format!("scope = '{DATABASE_KIND}'"),
    )
}

fn server_role_members_definition() -> String {
    definition_of_static(
        &ROLE_MEMBERS_VIEW,
        ROLE_MEMBERS_TABLE,
        &format!("scope = '{SERVER_KIND}'"),
    )
}

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

fn definition(items: &[(&str, String)], table: &str, filter: &str) -> String {
    let mut text = String::from("SELECT ");
    for (index, (name, expression)) in items.iter().enumerate() {
        if index > 0 {
            text.push_str(",\n       ");
        }
        if expression.as_str() != *name {
            text.push_str(expression);
            text.push_str(" AS ");
        }
        text.push_str(name);
    }
    text.push_str("\n  FROM master.dbo.");
    text.push_str(table);
    text.push_str("\n WHERE ");
    text.push_str(filter);
    text
}

/// [`definition`], for a select list whose expressions are `'static`.
fn definition_of_static(items: &[(&str, &str)], table: &str, filter: &str) -> String {
    let owned: Vec<(&str, String)> = items
        .iter()
        .map(|&(name, expr)| (name, expr.to_owned()))
        .collect();
    definition(&owned, table, filter)
}

fn column(name: &str, ty: SqlType, nullable: bool) -> InternalColumnDef {
    InternalColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

fn principals_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("kind", KIND_TYPE, false),
        column("principal_id", SqlType::Int, false),
        column("sid", SqlType::VarBinary(Len::Fixed(85)), true),
        column("type", SqlType::Char(Len::Fixed(1)), false),
        column("type_desc", SqlType::NVarChar(Len::Fixed(60)), false),
        column("is_fixed_role", SqlType::Bit, false),
        column("owning_principal_id", SqlType::Int, true),
        column("default_schema_name", SYSNAME, true),
        column("default_database_name", SYSNAME, true),
        column("default_language_name", SYSNAME, true),
        column("is_disabled", SqlType::Bit, true),
    ]
}

fn role_members_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("scope", KIND_TYPE, false),
        column("role_principal_id", SqlType::Int, false),
        column("member_principal_id", SqlType::Int, false),
    ]
}

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

fn opt_text(s: Option<&str>) -> Value {
    s.map_or(Value::Null, text)
}

fn opt_i32(v: Option<i32>) -> Value {
    v.map_or(Value::Null, Value::I32)
}

/// The `sid` of a row of `sys.server_principals`: one well-known byte per fixed principal
/// (module documentation, section "Columns without a matching fact"). `sa`'s is
/// [`SA_OWNER_SID`] by name, the same the bootstrap writes as `sys.databases.owner_sid`.
fn sid_byte(principal_id: i32) -> Value {
    if principal_id == 1 {
        return Value::Bytes(SA_OWNER_SID.to_vec());
    }
    Value::Bytes(vec![u8::try_from(principal_id).unwrap_or(0xFF)])
}

/// A row of `sys.database_principals`: `public` (0) through the eight fixed database roles.
/// `name` is not a field here (module documentation, section "Fixed rows, `name` included");
/// `principal_id` is the key [`DATABASE_PRINCIPAL_NAMES`] reads back.
#[allow(clippy::too_many_arguments)]
fn database_principal_row(
    principal_id: i32,
    type_: &str,
    type_desc: &str,
    is_fixed_role: bool,
    owning_principal_id: Option<i32>,
    default_schema_name: Option<&str>,
) -> Row {
    let mut row = vec![Value::Null; principals_columns::WIDTH];
    row[principals_columns::KIND] = text(DATABASE_KIND);
    row[principals_columns::PRINCIPAL_ID] = Value::I32(principal_id);
    row[principals_columns::SID] = Value::Null;
    row[principals_columns::TYPE] = text(type_);
    row[principals_columns::TYPE_DESC] = text(type_desc);
    row[principals_columns::IS_FIXED_ROLE] = Value::Bit(is_fixed_role);
    row[principals_columns::OWNING_PRINCIPAL_ID] = opt_i32(owning_principal_id);
    row[principals_columns::DEFAULT_SCHEMA_NAME] = opt_text(default_schema_name);
    row[principals_columns::DEFAULT_DATABASE_NAME] = Value::Null;
    row[principals_columns::DEFAULT_LANGUAGE_NAME] = Value::Null;
    row[principals_columns::IS_DISABLED] = Value::Null;
    Row(row)
}

/// A row of `sys.server_principals`: `sa` (1) through the eight fixed server roles. `name` is
/// not a field here, same reason as [`database_principal_row`].
#[allow(clippy::too_many_arguments)]
fn server_principal_row(
    principal_id: i32,
    type_: &str,
    type_desc: &str,
    is_fixed_role: bool,
    is_disabled: bool,
    owning_principal_id: Option<i32>,
    default_database_name: Option<&str>,
    default_language_name: Option<&str>,
) -> Row {
    let mut row = vec![Value::Null; principals_columns::WIDTH];
    row[principals_columns::KIND] = text(SERVER_KIND);
    row[principals_columns::PRINCIPAL_ID] = Value::I32(principal_id);
    row[principals_columns::SID] = sid_byte(principal_id);
    row[principals_columns::TYPE] = text(type_);
    row[principals_columns::TYPE_DESC] = text(type_desc);
    row[principals_columns::IS_FIXED_ROLE] = Value::Bit(is_fixed_role);
    row[principals_columns::OWNING_PRINCIPAL_ID] = opt_i32(owning_principal_id);
    row[principals_columns::DEFAULT_SCHEMA_NAME] = Value::Null;
    row[principals_columns::DEFAULT_DATABASE_NAME] = opt_text(default_database_name);
    row[principals_columns::DEFAULT_LANGUAGE_NAME] = opt_text(default_language_name);
    row[principals_columns::IS_DISABLED] = Value::Bit(is_disabled);
    Row(row)
}

fn principal_rows() -> Vec<Row> {
    vec![
        database_principal_row(0, "R", "DATABASE_ROLE", false, Some(1), None),
        database_principal_row(1, "S", "SQL_USER", false, None, Some("dbo")),
        database_principal_row(2, "S", "SQL_USER", false, None, Some("guest")),
        database_principal_row(3, "S", "SQL_USER", false, None, None),
        database_principal_row(4, "S", "SQL_USER", false, None, None),
        database_principal_row(16_384, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_385, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_386, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_387, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_390, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_391, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_392, "R", "DATABASE_ROLE", true, Some(1), None),
        database_principal_row(16_393, "R", "DATABASE_ROLE", true, Some(1), None),
        server_principal_row(
            1,
            "S",
            "SQL_LOGIN",
            false,
            false,
            None,
            Some("master"),
            Some("us_english"),
        ),
        server_principal_row(2, "R", "SERVER_ROLE", false, false, Some(1), None, None),
        server_principal_row(3, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(4, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(5, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(6, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(7, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(8, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(9, "R", "SERVER_ROLE", true, false, Some(1), None, None),
        server_principal_row(10, "R", "SERVER_ROLE", true, false, Some(1), None, None),
    ]
}

fn role_member_row(scope: &str, role_principal_id: i32, member_principal_id: i32) -> Row {
    let mut row = vec![Value::Null; role_members_columns::WIDTH];
    row[role_members_columns::SCOPE] = text(scope);
    row[role_members_columns::ROLE_PRINCIPAL_ID] = Value::I32(role_principal_id);
    row[role_members_columns::MEMBER_PRINCIPAL_ID] = Value::I32(member_principal_id);
    Row(row)
}

/// `dbo` (1) in `db_owner` (16384), `sa` (1) in `sysadmin` (3).
fn role_member_rows() -> Vec<Row> {
    vec![
        role_member_row(DATABASE_KIND, 16_384, 1),
        role_member_row(SERVER_KIND, 3, 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns_of(text: &str) -> Vec<String> {
        let select_list = text
            .strip_prefix("SELECT ")
            .and_then(|rest| rest.split("\n  FROM").next())
            .expect("a definition starts with SELECT and has a FROM clause");
        // A `CASE` expression carries its own commas (`WHEN … THEN N'…'` pairs do not), but a
        // parenthesised argument list could; a plain split on `,` at top level is enough here
        // (test `the_columns_of_the_four_views_are_the_published_ones` below, on the actual
        // definitions this file builds). The `name` column is first, so this does not need to
        // look inside it.
        select_list
            .split(',')
            .map(|piece| {
                piece
                    .rsplit("AS ")
                    .next()
                    .unwrap_or(piece)
                    .split_whitespace()
                    .last()
                    .unwrap_or(piece)
                    .trim()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_columns_of_the_four_views_are_the_published_ones() {
        assert_eq!(
            columns_of(&database_principals_definition()),
            PUBLISHED_DATABASE_PRINCIPALS_COLUMNS
        );
        assert_eq!(
            columns_of(&server_principals_definition()),
            PUBLISHED_SERVER_PRINCIPALS_COLUMNS
        );
        assert_eq!(
            columns_of(&database_role_members_definition()),
            PUBLISHED_ROLE_MEMBERS_COLUMNS
        );
        assert_eq!(
            columns_of(&server_role_members_definition()),
            PUBLISHED_ROLE_MEMBERS_COLUMNS
        );
    }

    #[test]
    fn no_definition_holds_a_join() {
        for text in [
            database_principals_definition(),
            server_principals_definition(),
            database_role_members_definition(),
            server_role_members_definition(),
        ] {
            assert!(!text.to_ascii_uppercase().contains("JOIN"), "{text}");
        }
    }

    #[test]
    fn the_four_definitions_parse() {
        use vauban_parser::{ParseOptions, parse_batch};

        for (name, definition) in [
            ("database_principals", database_principals_definition()),
            ("server_principals", server_principals_definition()),
            ("database_role_members", database_role_members_definition()),
            ("server_role_members", server_role_members_definition()),
        ] {
            let batch = parse_batch(&definition, &ParseOptions::default())
                .unwrap_or_else(|err| panic!("sys.{name}: {}", err.message));
            assert_eq!(batch.statements.len(), 1, "sys.{name}");
        }
    }

    #[test]
    fn each_view_is_installed_in_the_four_system_databases() {
        for table in internal_tables() {
            let names: Vec<&str> = table
                .views
                .iter()
                .map(|view| view.name.database.as_str())
                .collect();
            assert_eq!(
                names.len(),
                SYSTEM_DATABASES.len() * (table.views.len() / SYSTEM_DATABASES.len()),
                "{names:?}"
            );
        }
    }

    #[test]
    fn one_membership_couple_per_scope() {
        let rows = role_member_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0[role_members_columns::SCOPE], text(DATABASE_KIND));
        assert_eq!(
            rows[0].0[role_members_columns::ROLE_PRINCIPAL_ID],
            Value::I32(16_384)
        );
        assert_eq!(
            rows[0].0[role_members_columns::MEMBER_PRINCIPAL_ID],
            Value::I32(1)
        );
        assert_eq!(rows[1].0[role_members_columns::SCOPE], text(SERVER_KIND));
        assert_eq!(
            rows[1].0[role_members_columns::ROLE_PRINCIPAL_ID],
            Value::I32(3)
        );
        assert_eq!(
            rows[1].0[role_members_columns::MEMBER_PRINCIPAL_ID],
            Value::I32(1)
        );
    }

    #[test]
    fn sa_sid_is_the_bootstrap_owner_sid() {
        let rows = principal_rows();
        let sa = rows
            .iter()
            .find(|row| {
                row.0[principals_columns::KIND] == text(SERVER_KIND)
                    && row.0[principals_columns::PRINCIPAL_ID] == Value::I32(1)
            })
            .expect("sa is a row of the table");
        assert_eq!(
            sa.0[principals_columns::SID],
            Value::Bytes(SA_OWNER_SID.to_vec())
        );
    }

    #[test]
    fn nine_database_roles_and_nine_server_roles_are_fixed_or_public() {
        let rows = principal_rows();
        let database_roles = rows
            .iter()
            .filter(|row| row.0[principals_columns::KIND] == text(DATABASE_KIND))
            .filter(|row| row.0[principals_columns::TYPE] == text("R"))
            .count();
        let server_roles = rows
            .iter()
            .filter(|row| row.0[principals_columns::KIND] == text(SERVER_KIND))
            .filter(|row| row.0[principals_columns::TYPE] == text("R"))
            .count();
        assert_eq!(database_roles, 9, "public plus the eight fixed roles");
        assert_eq!(server_roles, 9, "public plus the eight fixed roles");
    }

    #[test]
    fn every_principal_id_of_a_row_has_a_name_in_its_case() {
        let rows = principal_rows();
        for row in &rows {
            let id = match row.0[principals_columns::PRINCIPAL_ID] {
                Value::I32(id) => id,
                ref other => panic!("{other:?}"),
            };
            let names = if row.0[principals_columns::KIND] == text(DATABASE_KIND) {
                &DATABASE_PRINCIPAL_NAMES[..]
            } else {
                &SERVER_PRINCIPAL_NAMES[..]
            };
            assert!(
                names.iter().any(|&(named_id, _)| named_id == id),
                "principal_id {id} has no name"
            );
        }
    }

    #[test]
    fn no_row_of_the_principals_table_carries_the_text_of_a_system_schema() {
        // `bootstrap.rs`'s `schemas_table` finds the internal table of the schemas by the
        // presence of `dbo`, `sys` and `INFORMATION_SCHEMA` among its rows; this table must
        // not carry all three, `name` not being one of its columns (module documentation).
        let rows = principal_rows();
        let schema_names = ["dbo", "sys", "INFORMATION_SCHEMA"];
        let carried = schema_names
            .iter()
            .filter(|schema| {
                rows.iter()
                    .any(|row| row.0.iter().any(|value| *value == text(schema)))
            })
            .count();
        assert!(carried < schema_names.len(), "{carried}");
    }
}
