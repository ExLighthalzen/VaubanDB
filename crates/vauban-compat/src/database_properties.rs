//! The table behind `DATABASEPROPERTYEX(database, property)`.
//!
//! The function takes a database name and a property name and returns one `sql_variant`; a
//! name it does not know gives `NULL`. Both arguments are matched without regard to case and
//! without their trailing spaces ([`is_one_of`], [`normalize_property`]).
//!
//! # The values
//!
//! The constants of this module are the values SQL Server 2022 answers for its four system
//! databases and for a freshly created user database. Two of the fourteen properties are
//! **not** one constant across those databases, which is why [`SIMPLE_RECOVERY_DATABASES`]
//! and [`FULLTEXT_DISABLED_DATABASES`] exist: `Recovery` is `SIMPLE` on `master`, `tempdb`
//! and `msdb` and `FULL` on `model` and on a user database, which takes its recovery model
//! from `model`; `IsFulltextEnabled` is `0` on `master`, `tempdb` and `model` and `1` on
//! `msdb` and on a user database. The pair `master` / a user database is the vector that
//! separates the two readings, and the tests `recovery_model_follows_the_database` and
//! `fulltext_follows_the_database` assert it.
//!
//! # What VaubanDB does not carry yet
//!
//! - The database must be **known to the evaluation context**: [`database_property`] asks
//!   [`EvalContext::database_id`] and answers `NULL` when it has no identifier, which is
//!   what SQL Server answers for a database that does not exist. A context whose
//!   `database_id` keeps the trait default `None` therefore answers `NULL` for each of the
//!   fourteen names (`a_context_without_databases_answers_null_for_every_name`).
//! - `COLLATE` on `CREATE DATABASE` is not carried through the catalogue (`collation_name`
//!   stays `NULL` in `sys.databases` for a collation other than
//!   [`vauban_types::Collation::DEFAULT`]), so `Collation` here answers the collation of the
//!   instance, [`COLLATION`], for each of the databases it knows, and not a per-database
//!   collation.
//!
//! # Type of the result
//!
//! SQL Server returns a `sql_variant`, nullable, for a text property such as `'Collation'`
//! as for an integer one such as `'Version'`. The V1 type system does not represent
//! `sql_variant`, so the call declares `nvarchar(128)` nullable, the same strategy
//! `SERVERPROPERTY` follows (`server_properties.rs`). The table below keeps the base type
//! SQL Server stores inside the `sql_variant`, and [`database_property_as`] renders it under
//! the declared type, which is what the executor and the TDS encoder expect from a row.

use vauban_errors::SqlResult;
use vauban_sysfn::EvalContext;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value, convert};

/// `DATABASEPROPERTYEX(<db>, 'Collation')`: the collation of the instance,
/// `SQL_Latin1_General_CP1_CI_AS` (`nvarchar`), for the system databases and for a user
/// database alike. The name of [`vauban_types::Collation::DEFAULT`], tied to it by the test
/// `collation_is_the_name_of_the_default_collation`.
const COLLATION: &str = "SQL_Latin1_General_CP1_CI_AS";

/// `DATABASEPROPERTYEX(<db>, 'Status')`: `ONLINE` (`nvarchar`), `tempdb` included.
const STATUS: &str = "ONLINE";

/// `DATABASEPROPERTYEX(<db>, 'Updateability')`: `READ_WRITE` (`nvarchar`).
const UPDATEABILITY: &str = "READ_WRITE";

/// `DATABASEPROPERTYEX(<db>, 'UserAccess')`: `MULTI_USER` (`nvarchar`).
const USER_ACCESS: &str = "MULTI_USER";

/// `DATABASEPROPERTYEX(<db>, 'Recovery')` of the databases of
/// [`SIMPLE_RECOVERY_DATABASES`].
const RECOVERY_SIMPLE: &str = "SIMPLE";

/// `DATABASEPROPERTYEX(<db>, 'Recovery')` of the other databases: `model`, and a user
/// database, which takes its recovery model from `model`.
const RECOVERY_FULL: &str = "FULL";

/// Databases whose `Recovery` is [`RECOVERY_SIMPLE`]: `master`, `tempdb` and `msdb`,
/// against `FULL` on `model`.
const SIMPLE_RECOVERY_DATABASES: &[&str] = &["master", "tempdb", "msdb"];

/// Databases whose `IsFulltextEnabled` is `0`: `master`, `tempdb` and `model`, against `1`
/// on `msdb` and on a user database.
const FULLTEXT_DISABLED_DATABASES: &[&str] = &["master", "tempdb", "model"];

/// `DATABASEPROPERTYEX(<db>, 'Version')`: `957` (`int`), the internal database version of
/// SQL Server 2022, for the system databases and for a user database alike. A version
/// number of the announced release, kept as `SERVERPROPERTY('ProductVersion')` is.
const VERSION: i32 = 957;

/// `DATABASEPROPERTYEX(<db>, 'ComparisonStyle')` for a case-insensitive, accent-sensitive
/// comparison: `196609` (`int`), the same number
/// `SERVERPROPERTY('ComparisonStyle')` answers (`server_properties.rs`).
const COMPARISON_STYLE: i32 = 196_609;

/// `DATABASEPROPERTYEX(<db>, 'LCID')`: `1033` (`int`), English (United States).
/// The LCID of [`vauban_types::Collation::DEFAULT`], tied to it by the test
/// `lcid_and_sort_order_are_those_of_the_default_collation`.
const LCID: i32 = 1033;

/// `DATABASEPROPERTYEX(<db>, 'SQLSortOrder')`: `52`, base type **`tinyint`** and not
/// `int`, the third variant [`base_type`] lists, next to `nvarchar` and `int`
/// (`base_type_of_the_three_variants_the_table_produces`). The `SortId` of
/// [`vauban_types::Collation::DEFAULT`], tied to it by the test
/// `lcid_and_sort_order_are_those_of_the_default_collation`.
const SQL_SORT_ORDER: u8 = 52;

/// `DATABASEPROPERTYEX(<db>, 'IsAnsiNullDefault')`: `0` (`int`).
const IS_ANSI_NULL_DEFAULT: i32 = 0;

/// `DATABASEPROPERTYEX(<db>, 'IsAutoClose')`: `0` (`int`).
const IS_AUTO_CLOSE: i32 = 0;

/// `DATABASEPROPERTYEX(<db>, 'IsAutoShrink')`: `0` (`int`).
const IS_AUTO_SHRINK: i32 = 0;

/// `DATABASEPROPERTYEX(<db>, 'IsInStandBy')`: `0` (`int`).
const IS_IN_STANDBY: i32 = 0;

/// The property names [`database_property`] answers, in their documented spelling.
///
/// A test fixture, as in `server_properties.rs`: the `match` of [`database_property`], not
/// this list, is what answers a call. A property added to the `match` is added here, and
/// `property_names_lists_known_properties_only` checks that each name below is known while
/// a misspelt one is not.
#[cfg(test)]
pub(crate) const PROPERTY_NAMES: &[&str] = &[
    "Collation",
    "Status",
    "Updateability",
    "UserAccess",
    "Recovery",
    "Version",
    "IsAutoClose",
    "IsAutoShrink",
    "IsFulltextEnabled",
    "ComparisonStyle",
    "LCID",
    "SQLSortOrder",
    "IsAnsiNullDefault",
    "IsInStandBy",
];

/// Builds a text property value.
fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// Whether `database` is one of `names`, matched ASCII case-insensitively after trailing
/// spaces are dropped, for the two properties whose value depends on which database is named.
///
/// `DATABASEPROPERTYEX('MaStEr', 'Status')` and `DATABASEPROPERTYEX('master ', 'Status')`
/// answer `ONLINE`, as does `DATABASEPROPERTYEX(CAST('master' AS nchar(20)), 'Status')`, so
/// neither the case nor the trailing padding changes which database is named;
/// `DATABASEPROPERTYEX('[master]', 'Status')` answers `NULL`, so brackets are **not**
/// stripped.
///
/// This padding trim is local to the lookup below. Whether the **database** exists is
/// decided by [`EvalContext::database_id`], in the session and not in this crate: the trim
/// that matters for resolving the name lives there, and `Databases::database_id` of the
/// tests below reproduces it so that `'master '` resolves in a test too.
fn is_one_of(database: &str, names: &[&str]) -> bool {
    let database = database.trim_end_matches(' ');
    names.iter().any(|name| name.eq_ignore_ascii_case(database))
}

/// The property name as the `match` of [`database_property`] reads it: upper-cased and
/// stripped of its trailing spaces.
///
/// `DATABASEPROPERTYEX('master', 'Status ')`, the same with two trailing spaces,
/// `'Recovery '`, `'Version '`, `'collation '` and `CAST('Status' AS nchar(20))` answer
/// `ONLINE`, `SIMPLE`, `957` and `SQL_Latin1_General_CP1_CI_AS`: a `char`/`nchar` parameter
/// of a client arrives in that padded shape. A **leading** space is kept:
/// `DATABASEPROPERTYEX('master', ' Status')` answers `NULL`, and so do `'Status' + CHAR(9)`
/// and `'Status' + CHAR(10)`, so the trim is on the end of the name and takes the space
/// alone.
fn normalize_property(name: &str) -> String {
    name.trim_end_matches(' ').to_ascii_uppercase()
}

/// Returns the value of the property `name` of the database `database`, or [`Value::Null`]
/// when the evaluation context does not know the database or when the property name is
/// unknown, as SQL Server does for either.
///
/// Both names are matched ASCII case-insensitively.
pub(crate) fn database_property(database: &str, name: &str, ctx: &dyn EvalContext) -> Value {
    // A database the context cannot identify is a database that does not exist, and
    // `DATABASEPROPERTYEX('no_such_db', 'Status')` is NULL.
    if ctx.database_id(Some(database)).is_none() {
        return Value::Null;
    }
    match normalize_property(name).as_str() {
        "COLLATION" => text(COLLATION),
        "STATUS" => text(STATUS),
        "UPDATEABILITY" => text(UPDATEABILITY),
        "USERACCESS" => text(USER_ACCESS),
        "RECOVERY" => {
            if is_one_of(database, SIMPLE_RECOVERY_DATABASES) {
                text(RECOVERY_SIMPLE)
            } else {
                text(RECOVERY_FULL)
            }
        }
        "VERSION" => Value::I32(VERSION),
        "ISAUTOCLOSE" => Value::I32(IS_AUTO_CLOSE),
        "ISAUTOSHRINK" => Value::I32(IS_AUTO_SHRINK),
        "ISFULLTEXTENABLED" => {
            if is_one_of(database, FULLTEXT_DISABLED_DATABASES) {
                Value::I32(0)
            } else {
                Value::I32(1)
            }
        }
        "COMPARISONSTYLE" => Value::I32(COMPARISON_STYLE),
        "LCID" => Value::I32(LCID),
        // `tinyint` on SQL Server, hence `Value::I8`.
        "SQLSORTORDER" => Value::I8(SQL_SORT_ORDER),
        "ISANSINULLDEFAULT" => Value::I32(IS_ANSI_NULL_DEFAULT),
        "ISINSTANDBY" => Value::I32(IS_IN_STANDBY),
        _ => Value::Null,
    }
}

/// Base type of a property value: the type SQL Server stores inside the `sql_variant`, and
/// the source type [`database_property_as`] reads to convert the value to the declared type.
///
/// The table produces [`Value::String`], [`Value::I32`], [`Value::I8`] and [`Value::Null`];
/// another variant gets its base type here. Until it does, [`database_property_as`] answers
/// `NULL` for it and `every_property_reads_as_the_declared_type` in `lib.rs` fails on that
/// property, which is where the omission is meant to be caught.
fn base_type(value: &Value) -> Option<TypeInfo> {
    let ty = match value {
        // The declared type of `DATABASEPROPERTYEX`; a text property is already under it.
        Value::String(_) => SqlType::NVarChar(Len::Fixed(128)),
        Value::I32(_) => SqlType::Int,
        Value::I8(_) => SqlType::TinyInt,
        _ => return None,
    };
    Some(TypeInfo::new(ty, true))
}

/// Returns the value of the property `name` of `database` under `declared`, the type the
/// call was bound with (`nvarchar(128)` nullable, see the module documentation).
///
/// An integer property is converted to `declared` exactly as `CAST` would: `Version` reads
/// as the string `957`, `SQLSortOrder` as the string `52`. A text property and `NULL` are
/// returned unchanged.
pub(crate) fn database_property_as(
    database: &str,
    name: &str,
    ctx: &dyn EvalContext,
    declared: &TypeInfo,
) -> SqlResult<Value> {
    let value = database_property(database, name, ctx);
    match base_type(&value) {
        Some(base) if base.ty != declared.ty => convert(&value, &base, declared, None),
        Some(_) => Ok(value),
        // `Value::Null`, and a variant the table does not produce today until `base_type`
        // lists it: `NULL` is a legal value under the declared type.
        None => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{Collation, Date, DateTime2, Decimal, Time};

    /// An [`EvalContext`] that knows a fixed list of databases, so that `database_id`
    /// answers something: `StaticContext` has no field for it and the trait default is
    /// `None`, which would make the tests below read `NULL`.
    struct Databases(&'static [&'static str]);

    /// The four databases `catalog` bootstraps, plus one user database.
    const KNOWN: Databases = Databases(&["master", "tempdb", "model", "msdb", "shop"]);

    impl EvalContext for Databases {
        fn now_local(&self) -> DateTime2 {
            DateTime2 {
                date: Date { days: 0 },
                time: Time { ticks_100ns: 0 },
            }
        }

        fn now_utc(&self) -> DateTime2 {
            self.now_local()
        }

        fn rowcount(&self) -> i64 {
            0
        }

        fn last_identity(&self) -> Option<Decimal> {
            None
        }

        fn spid(&self) -> i16 {
            51
        }

        fn current_database(&self) -> &str {
            "master"
        }

        fn server_name(&self) -> &str {
            "VAUBAN"
        }

        fn object_id(&self, _name: &str) -> Option<i32> {
            None
        }

        fn object_name(&self, _id: i32) -> Option<String> {
            None
        }

        fn variable(&self, _name: &str) -> Option<Value> {
            None
        }

        /// The position in the list, one-based, as `sys.databases.database_id` would be.
        fn database_id(&self, name: Option<&str>) -> Option<i32> {
            let name = name.unwrap_or_else(|| self.current_database());
            let name = name.trim_end_matches(' ');
            self.0
                .iter()
                .position(|known| known.eq_ignore_ascii_case(name))
                .and_then(|index| i32::try_from(index + 1).ok())
        }
    }

    fn as_text(value: Value) -> String {
        match value {
            Value::String(s) => s.text,
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn property(database: &str, name: &str) -> Value {
        database_property(database, name, &KNOWN)
    }

    /// The declared type of `DATABASEPROPERTYEX`, as `database_property_return_type` builds
    /// it in `lib.rs`.
    fn declared() -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true)
    }

    #[test]
    fn master_collation_is_the_instance_collation() {
        assert_eq!(
            as_text(property("master", "Collation")),
            "SQL_Latin1_General_CP1_CI_AS"
        );
        // The property name is matched without regard to case.
        assert_eq!(
            as_text(property("master", "collation")),
            "SQL_Latin1_General_CP1_CI_AS"
        );
        assert_eq!(
            as_text(property("MASTER", "COLLATION")),
            "SQL_Latin1_General_CP1_CI_AS"
        );
        // The instance collation for each database the context knows: `COLLATE` is not
        // carried through the catalogue yet, see the module documentation.
        for database in ["tempdb", "model", "msdb", "shop"] {
            assert_eq!(as_text(property(database, "Collation")), COLLATION);
        }
    }

    #[test]
    fn collation_is_the_name_of_the_default_collation() {
        // The vector that ties the name to `Collation::DEFAULT`: a name of another
        // collation would parse to other bytes.
        assert_eq!(
            Collation::parse(COLLATION).expect("the collation name is a known collation"),
            Collation::DEFAULT
        );
    }

    #[test]
    fn lcid_and_sort_order_are_those_of_the_default_collation() {
        assert_eq!(i64::from(Collation::DEFAULT.lcid), i64::from(LCID));
        assert_eq!(Collation::DEFAULT.sort_id, SQL_SORT_ORDER);
        assert_eq!(property("master", "LCID"), Value::I32(1033));
        // `tinyint` and not `int`: the base type of `SQLSortOrder`.
        assert_eq!(property("master", "SQLSortOrder"), Value::I8(52));
    }

    #[test]
    fn unknown_database_is_null() {
        for property_name in PROPERTY_NAMES {
            assert_eq!(property("no_such_db", property_name), Value::Null);
        }
        assert_eq!(property("", "Status"), Value::Null);
        // Brackets are part of the name looked up.
        assert_eq!(property("[master]", "Status"), Value::Null);
    }

    #[test]
    fn unknown_property_is_null() {
        assert_eq!(property("master", "NoSuchProperty"), Value::Null);
        assert_eq!(property("master", ""), Value::Null);
        // A leading space is part of the property name: ' Status' answers NULL where
        // 'Status' answers ONLINE.
        assert_eq!(property("master", " Status"), Value::Null);
        assert_eq!(as_text(property("master", "Status")), "ONLINE");
    }

    /// Trailing spaces on the **property** name, the shape a `char`/`nchar` parameter of a
    /// client arrives in: `DATABASEPROPERTYEX('master', 'Status ')`, the same with two
    /// spaces, `'Recovery '`, `'Version '`, `'collation '` and `CAST('Status' AS nchar(20))`
    /// answer `ONLINE`, `ONLINE`, `SIMPLE`, `957`, `SQL_Latin1_General_CP1_CI_AS` and
    /// `ONLINE`.
    #[test]
    fn a_property_name_padded_with_spaces_reads_as_the_unpadded_one() {
        assert_eq!(as_text(property("master", "Status ")), "ONLINE");
        assert_eq!(as_text(property("master", "Status  ")), "ONLINE");
        assert_eq!(as_text(property("master", "Recovery ")), "SIMPLE");
        assert_eq!(property("master", "Version "), Value::I32(957));
        assert_eq!(
            as_text(property("master", "collation ")),
            "SQL_Latin1_General_CP1_CI_AS"
        );
        // `CAST('Status' AS nchar(20))`, as the executor would hand it over: the name padded
        // to the declared width.
        assert_eq!(
            as_text(property("master", &format!("{:<20}", "Status"))),
            "ONLINE"
        );
        // A padded database name resolves the same way; here the trim that resolves it
        // lives in `database_id` (see `is_one_of`).
        assert_eq!(as_text(property("master ", "Status ")), "ONLINE");
        // A leading space stays part of the name: NULL.
        assert_eq!(property("master", " Status"), Value::Null);
        // A trailing tab is not a space: `'Status' + CHAR(9)` and `'Status' + CHAR(10)`
        // answer NULL; the space is the one character the trim drops.
        assert_eq!(property("master", "Status\t"), Value::Null);
        assert_eq!(property("master", "Status\n"), Value::Null);
    }

    #[test]
    fn status_updateability_and_user_access_are_the_same_for_every_known_database() {
        for database in ["master", "tempdb", "model", "msdb", "shop"] {
            assert_eq!(
                as_text(property(database, "Status")),
                "ONLINE",
                "{database}"
            );
            assert_eq!(
                as_text(property(database, "Updateability")),
                "READ_WRITE",
                "{database}"
            );
            assert_eq!(
                as_text(property(database, "UserAccess")),
                "MULTI_USER",
                "{database}"
            );
            assert_eq!(property(database, "Version"), Value::I32(957), "{database}");
        }
    }

    #[test]
    fn recovery_model_follows_the_database() {
        // The vector a single constant cannot satisfy: `master` SIMPLE against a user
        // database FULL.
        for database in ["master", "tempdb", "msdb"] {
            assert_eq!(
                as_text(property(database, "Recovery")),
                "SIMPLE",
                "{database}"
            );
        }
        for database in ["model", "shop"] {
            assert_eq!(
                as_text(property(database, "Recovery")),
                "FULL",
                "{database}"
            );
        }
        // Case and a trailing space do not change which database is named.
        assert_eq!(as_text(property("MaStEr", "Recovery")), "SIMPLE");
        assert_eq!(as_text(property("master ", "Recovery")), "SIMPLE");
    }

    #[test]
    fn fulltext_follows_the_database() {
        // The second vector a single constant cannot satisfy: `master` 0 against `msdb` and
        // a user database 1.
        for database in ["master", "tempdb", "model"] {
            assert_eq!(
                property(database, "IsFulltextEnabled"),
                Value::I32(0),
                "{database}"
            );
        }
        for database in ["msdb", "shop"] {
            assert_eq!(
                property(database, "IsFulltextEnabled"),
                Value::I32(1),
                "{database}"
            );
        }
    }

    #[test]
    fn integer_flags_at_zero() {
        for name in [
            "IsAutoClose",
            "IsAutoShrink",
            "IsAnsiNullDefault",
            "IsInStandBy",
        ] {
            assert_eq!(property("master", name), Value::I32(0), "{name}");
        }
        assert_eq!(property("master", "ComparisonStyle"), Value::I32(196_609));
    }

    #[test]
    fn property_names_lists_known_properties_only() {
        for name in PROPERTY_NAMES {
            assert_ne!(
                property("master", name),
                Value::Null,
                "{name} is not a property of the table"
            );
        }
        // A misspelt name is indistinguishable from an unknown one: the check above is what
        // keeps the list in step with the `match`.
        assert_eq!(property("master", "Collationn"), Value::Null);
    }

    #[test]
    fn an_integer_property_reads_as_the_declared_type() {
        // The table keeps the base type SQL Server stores in the `sql_variant`...
        assert_eq!(property("master", "Version"), Value::I32(957));
        // ...and the call reads it under the declared `nvarchar(128)`.
        assert_eq!(
            database_property_as("master", "Version", &KNOWN, &declared())
                .expect("no conversion error"),
            text("957")
        );
        assert_eq!(
            database_property_as("master", "SQLSortOrder", &KNOWN, &declared())
                .expect("no conversion error"),
            text("52")
        );
        assert_eq!(
            database_property_as("master", "IsAutoClose", &KNOWN, &declared())
                .expect("no conversion error"),
            text("0")
        );
    }

    #[test]
    fn a_text_property_and_an_unknown_name_are_unchanged() {
        assert_eq!(
            database_property_as("master", "Status", &KNOWN, &declared())
                .expect("no conversion error"),
            text("ONLINE")
        );
        assert_eq!(
            database_property_as("master", "NoSuchProperty", &KNOWN, &declared())
                .expect("no conversion error"),
            Value::Null
        );
        assert_eq!(
            database_property_as("no_such_db", "Status", &KNOWN, &declared())
                .expect("no conversion error"),
            Value::Null
        );
    }

    /// Walks the whole table under the declared type: a value of a variant the declared
    /// type does not describe would close the connection without an error token, because
    /// the TDS encoder refuses a row whose value and column type disagree.
    #[test]
    fn every_property_reads_as_the_declared_type() {
        let mut text_values = 0;
        for name in PROPERTY_NAMES {
            let value = database_property_as("master", name, &KNOWN, &declared())
                .expect("no conversion error");
            match value {
                Value::String(_) => text_values += 1,
                other => panic!("{name} reads as {other:?}, not as {:?}", declared().ty),
            }
        }
        // The fourteen names of `PROPERTY_NAMES`, each answering a value for `master`.
        assert_eq!(text_values, 14);
        assert_eq!(PROPERTY_NAMES.len(), 14);
    }

    #[test]
    fn base_type_of_the_three_variants_the_table_produces() {
        assert_eq!(
            base_type(&Value::I32(957)).map(|t| t.ty),
            Some(SqlType::Int)
        );
        assert_eq!(
            base_type(&Value::I8(52)).map(|t| t.ty),
            Some(SqlType::TinyInt)
        );
        assert_eq!(
            base_type(&text("x")).map(|t| t.ty),
            Some(SqlType::NVarChar(Len::Fixed(128)))
        );
        assert_eq!(base_type(&Value::Null), None);
    }

    /// A context that identifies no database reads `NULL` for each of the fourteen names of
    /// `PROPERTY_NAMES`: the shape of a context whose `database_id` keeps the trait default
    /// `None`.
    #[test]
    fn a_context_without_databases_answers_null_for_every_name() {
        let none = Databases(&[]);
        for name in PROPERTY_NAMES {
            assert_eq!(
                database_property("master", name, &none),
                Value::Null,
                "{name}"
            );
        }
    }
}
