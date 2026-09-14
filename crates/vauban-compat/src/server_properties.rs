//! The table behind `SERVERPROPERTY(name)`.
//!
//! The property name is matched without regard to case, and an unknown name gives `NULL`,
//! as on SQL Server. A value comes from the constants of this module (those of a default
//! instance of SQL Server 2022 on Linux, collation `SQL_Latin1_General_CP1_CI_AS`), from the
//! evaluation context (`ServerName`, `MachineName`, and `Edition` when the operator
//! overrides it), from the process (`ProcessID`), or from the VaubanDB identity: `Edition`
//! defaults to [`EDITION`] (`VaubanDB (64-bit)`) and `VaubanDB` answers `1`, two values that
//! describe VaubanDB and are a deliberate difference from SQL Server. Properties that need a
//! capability the engine does not have (`ResourceVersion`, `InstanceDefaultDataPath`)
//! answer `NULL`.
//!
//! In the table, each value carries the base type that SQL Server stores in the
//! `sql_variant`: integer properties are [`Value::I32`], text properties [`Value::String`].
//! The V1 type system does not represent `sql_variant`, so `SERVERPROPERTY` declares
//! `nvarchar(128)` at bind time (`server_property_return_type` in `lib.rs`);
//! [`server_property_as`] renders a value under that declared type, which is what the
//! executor and the TDS encoder expect from a row.

use vauban_errors::SqlResult;
use vauban_sysfn::EvalContext;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value, convert};

use crate::version::{
    EDITION, ENGINE_EDITION, PRODUCT_LEVEL, PRODUCT_UPDATE_LEVEL, PRODUCT_VERSION, version_parts,
};

/// `SERVERPROPERTY('Collation')`: the default collation of the instance.
const COLLATION: &str = "SQL_Latin1_General_CP1_CI_AS";

/// `SERVERPROPERTY('ComparisonStyle')` for a case-insensitive, accent-sensitive Windows
/// comparison (`IgnoreCase | IgnoreKanaType | IgnoreWidth`).
const COMPARISON_STYLE: i32 = 196_609;

/// `SERVERPROPERTY('LCID')`: English (United States).
const LCID: i32 = 1033;

/// `SERVERPROPERTY('SqlCharSetName')` for `SQL_Latin1_General_CP1_CI_AS` (code page 1252).
const SQL_CHAR_SET_NAME: &str = "iso_1";

/// `SERVERPROPERTY('SqlSortOrderName')` for `SQL_Latin1_General_CP1_CI_AS`.
const SQL_SORT_ORDER_NAME: &str = "nocase_iso";

/// `SERVERPROPERTY('PathSeparator')` on Linux.
const PATH_SEPARATOR: &str = "/";

/// `SERVERPROPERTY('FilestreamShareName')`: the name of the default instance (`nvarchar`),
/// which SQL Server on Linux reports even though FILESTREAM is not enabled there.
const FILESTREAM_SHARE_NAME: &str = "MSSQLSERVER";

/// The property names [`server_property`] answers, in their documented spelling.
///
/// The list is what a test can iterate over, since the table itself is a `match`: the tests of
/// this module check that each name below is a known property, and the test
/// `every_property_reads_as_the_declared_type` of `lib.rs` checks that each one reads as the
/// declared type. A property added to the `match` is added here. This list is a test
/// fixture: the `match` itself, not the list, is what answers a call.
#[cfg(test)]
pub(crate) const PROPERTY_NAMES: &[&str] = &[
    "ProductVersion",
    "ProductMajorVersion",
    "ProductMinorVersion",
    "ProductBuild",
    "ProductLevel",
    "ProductUpdateLevel",
    "Edition",
    "EngineEdition",
    "VaubanDB",
    "MachineName",
    "ServerName",
    "InstanceName",
    "InstanceDefaultDataPath",
    "Collation",
    "IsClustered",
    "IsHadrEnabled",
    "IsIntegratedSecurityOnly",
    "IsSingleUser",
    "IsFullTextInstalled",
    "IsXTPSupported",
    "IsPolyBaseInstalled",
    "ProcessID",
    "ComparisonStyle",
    "LCID",
    "SqlCharSetName",
    "SqlSortOrderName",
    "ResourceVersion",
    "ResourceLastUpdateDateTime",
    "FilestreamShareName",
    "HadrManagerStatus",
    "IsAdvancedAnalyticsInstalled",
    "IsBigDataCluster",
    "IsLocalDB",
    "IsExternalAuthenticationOnly",
    "PathSeparator",
    "SuspendedDatabaseCount",
];

/// Names of [`PROPERTY_NAMES`] whose value is [`Value::Null`] with the table below,
/// either because SQL Server itself answers `NULL` (`InstanceName` on a default instance,
/// `IsExternalAuthenticationOnly` outside Azure SQL) or because the capability is missing
/// (`InstanceDefaultDataPath`, `ResourceVersion`, `ResourceLastUpdateDateTime`). Used by the
/// tests below to tell a `NULL` value from a misspelt name.
#[cfg(test)]
const NULL_PROPERTY_NAMES: &[&str] = &[
    "InstanceName",
    "InstanceDefaultDataPath",
    "ResourceVersion",
    "ResourceLastUpdateDateTime",
    "IsExternalAuthenticationOnly",
];

/// Builds a text property value.
fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// Builds a text property value from an owned string.
fn text_owned(s: String) -> Value {
    Value::String(SqlString { text: s })
}

/// Returns the value of the server property `name`, or [`Value::Null`] when the name is
/// unknown, as SQL Server does for an invalid input. `name` is matched ASCII
/// case-insensitively.
pub(crate) fn server_property(name: &str, ctx: &dyn EvalContext) -> Value {
    let (major, minor, build, _revision) = version_parts();
    match name.to_ascii_uppercase().as_str() {
        "PRODUCTVERSION" => text(PRODUCT_VERSION),
        // The three components are nvarchar(128), like ProductVersion.
        "PRODUCTMAJORVERSION" => text_owned(major.to_string()),
        "PRODUCTMINORVERSION" => text_owned(minor.to_string()),
        "PRODUCTBUILD" => text_owned(build.to_string()),
        "PRODUCTLEVEL" => text(PRODUCT_LEVEL),
        "PRODUCTUPDATELEVEL" => text(PRODUCT_UPDATE_LEVEL),
        "EDITION" => text(ctx.edition().unwrap_or(EDITION)),
        "ENGINEEDITION" => Value::I32(ENGINE_EDITION),
        // Property added by VaubanDB, which SQL Server does not know (it answers NULL for
        // the name): a deliberate difference that tells the two products apart.
        "VAUBANDB" => Value::I32(1),
        "MACHINENAME" | "SERVERNAME" => text(ctx.server_name()),
        // Default instance: no instance name.
        "INSTANCENAME" => Value::Null,
        // `/var/opt/mssql/data/` (`nvarchar`) on SQL Server for Linux; NULL here, the
        // engine has no data directory.
        "INSTANCEDEFAULTDATAPATH" => Value::Null,
        "COLLATION" => text(COLLATION),
        "ISCLUSTERED" => Value::I32(0),
        "ISHADRENABLED" => Value::I32(0),
        "ISINTEGRATEDSECURITYONLY" => Value::I32(0),
        "ISSINGLEUSER" => Value::I32(0),
        "ISFULLTEXTINSTALLED" => Value::I32(0),
        // 1 (`int`): a 64-bit SQL Server 2022 supports In-Memory OLTP.
        "ISXTPSUPPORTED" => Value::I32(1),
        "ISPOLYBASEINSTALLED" => Value::I32(0),
        // `std::process::id` is a u32; a pid above i32::MAX cannot be represented.
        "PROCESSID" => i32::try_from(std::process::id()).map_or(Value::Null, Value::I32),
        "COMPARISONSTYLE" => Value::I32(COMPARISON_STYLE),
        "LCID" => Value::I32(LCID),
        "SQLCHARSETNAME" => text(SQL_CHAR_SET_NAME),
        "SQLSORTORDERNAME" => text(SQL_SORT_ORDER_NAME),
        // The version of the resource database (`16.00.4275`, `nvarchar`) on SQL Server;
        // NULL here, there is no resource database.
        "RESOURCEVERSION" => Value::Null,
        // Base type `datetime` on SQL Server; NULL here (no resource database).
        "RESOURCELASTUPDATEDATETIME" => Value::Null,
        // `MSSQLSERVER` (`nvarchar`): the share name of the default instance.
        "FILESTREAMSHARENAME" => text(FILESTREAM_SHARE_NAME),
        // 1 (`int`), "started and running": the HADR manager runs even when
        // `IsHadrEnabled` is 0.
        "HADRMANAGERSTATUS" => Value::I32(1),
        // 1 (`int`): SQL Server 2022 for Linux reports Machine Learning Services as
        // installed.
        "ISADVANCEDANALYTICSINSTALLED" => Value::I32(1),
        "ISBIGDATACLUSTER" => Value::I32(0),
        "ISLOCALDB" => Value::I32(0),
        // NULL: a property of Azure SQL Database and Managed Instance, which an instance
        // of SQL Server does not answer.
        "ISEXTERNALAUTHENTICATIONONLY" => Value::Null,
        "PATHSEPARATOR" => text(PATH_SEPARATOR),
        "SUSPENDEDDATABASECOUNT" => Value::I32(0),
        _ => Value::Null,
    }
}

/// Base type of a property value: the type SQL Server stores inside the `sql_variant`, and
/// the source type [`server_property_as`] reads to convert the value to the declared type.
///
/// The table produces [`Value::String`], [`Value::I32`] and [`Value::Null`]; another variant
/// gets its base type here. Until it does, [`server_property_as`] answers `NULL` for it and
/// the test `every_property_reads_as_the_declared_type` of `lib.rs` fails on that property,
/// which is where the omission is meant to be caught.
fn base_type(value: &Value) -> Option<TypeInfo> {
    let ty = match value {
        // The declared type of `SERVERPROPERTY`; a text property is already under it.
        Value::String(_) => SqlType::NVarChar(Len::Fixed(128)),
        Value::I32(_) => SqlType::Int,
        _ => return None,
    };
    Some(TypeInfo::new(ty, true))
}

/// Returns the value of the server property `name` under `declared`, the type the call was
/// bound with (`nvarchar(128)` nullable, see the module documentation).
///
/// An integer property is converted to `declared` exactly as `CAST` would: `EngineEdition`
/// reads as the string `3`, `VaubanDB` as the string `1`. A text property and `NULL` are
/// returned unchanged. A form that casts the call back to a number (`CONVERT(int, …)`) reads
/// the digits and gets the integer again.
pub(crate) fn server_property_as(
    name: &str,
    ctx: &dyn EvalContext,
    declared: &TypeInfo,
) -> SqlResult<Value> {
    let value = server_property(name, ctx);
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
    use vauban_sysfn::StaticContext;

    fn ctx() -> StaticContext {
        StaticContext {
            server_name: "VAUBAN".to_owned(),
            ..StaticContext::default()
        }
    }

    fn as_text(value: Value) -> String {
        match value {
            Value::String(s) => s.text,
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn product_version_matches_the_constant() {
        assert_eq!(
            as_text(server_property("productversion", &ctx())),
            PRODUCT_VERSION
        );
        assert_eq!(
            as_text(server_property("PRODUCTVERSION", &ctx())),
            PRODUCT_VERSION
        );
    }

    #[test]
    fn version_components_agree_with_product_version() {
        let major = as_text(server_property("ProductMajorVersion", &ctx()));
        let minor = as_text(server_property("ProductMinorVersion", &ctx()));
        let build = as_text(server_property("ProductBuild", &ctx()));
        assert_eq!(major, "16");
        assert_eq!(minor, "0");
        assert!(PRODUCT_VERSION.starts_with(&format!("{major}.{minor}.{build}.")));
        assert_eq!(as_text(server_property("ProductLevel", &ctx())), "RTM");
        assert_eq!(
            as_text(server_property("ProductUpdateLevel", &ctx())),
            PRODUCT_UPDATE_LEVEL
        );
    }

    #[test]
    fn edition_and_engine_edition() {
        assert_eq!(as_text(server_property("Edition", &ctx())), EDITION);
        assert_eq!(server_property("EngineEdition", &ctx()), Value::I32(3));
        assert_eq!(server_property("VaubanDB", &ctx()), Value::I32(1));
        assert_eq!(server_property("vaubandb", &ctx()), Value::I32(1));
    }

    #[test]
    fn edition_override_is_read_from_the_context() {
        let ctx = StaticContext {
            edition: Some("Custom Edition".into()),
            ..ctx()
        };
        assert_eq!(as_text(server_property("Edition", &ctx)), "Custom Edition");
        assert_eq!(server_property("VaubanDB", &ctx), Value::I32(1));
    }

    #[test]
    fn names_come_from_the_context() {
        assert_eq!(as_text(server_property("ServerName", &ctx())), "VAUBAN");
        assert_eq!(as_text(server_property("MachineName", &ctx())), "VAUBAN");
        assert_eq!(server_property("InstanceName", &ctx()), Value::Null);
    }

    #[test]
    fn unknown_property_is_null() {
        assert_eq!(server_property("NoSuchProperty", &ctx()), Value::Null);
        assert_eq!(server_property("", &ctx()), Value::Null);
    }

    #[test]
    fn integer_flags_are_i32() {
        for name in [
            "IsClustered",
            "IsHadrEnabled",
            "IsIntegratedSecurityOnly",
            "IsSingleUser",
            "IsFullTextInstalled",
            "IsPolyBaseInstalled",
            "IsBigDataCluster",
            "IsLocalDB",
            "SuspendedDatabaseCount",
        ] {
            assert_eq!(server_property(name, &ctx()), Value::I32(0), "{name}");
        }
        // The three flags at 1 in the table.
        for name in [
            "IsXTPSupported",
            "HadrManagerStatus",
            "IsAdvancedAnalyticsInstalled",
        ] {
            assert_eq!(server_property(name, &ctx()), Value::I32(1), "{name}");
        }
        // An Azure SQL property: NULL.
        assert_eq!(
            server_property("IsExternalAuthenticationOnly", &ctx()),
            Value::Null
        );
        assert_eq!(
            server_property("ComparisonStyle", &ctx()),
            Value::I32(196_609)
        );
        assert_eq!(server_property("LCID", &ctx()), Value::I32(1033));
    }

    #[test]
    fn collation_related_properties() {
        assert_eq!(
            as_text(server_property("Collation", &ctx())),
            "SQL_Latin1_General_CP1_CI_AS"
        );
        assert_eq!(as_text(server_property("SqlCharSetName", &ctx())), "iso_1");
        assert_eq!(
            as_text(server_property("SqlSortOrderName", &ctx())),
            "nocase_iso"
        );
        assert_eq!(as_text(server_property("PathSeparator", &ctx())), "/");
    }

    #[test]
    fn filestream_share_name_is_the_default_instance_name() {
        assert_eq!(
            as_text(server_property("FilestreamShareName", &ctx())),
            "MSSQLSERVER"
        );
    }

    #[test]
    fn process_id_is_the_current_process() {
        let expected = i32::try_from(std::process::id()).expect("pid fits in i32");
        assert_eq!(server_property("ProcessID", &ctx()), Value::I32(expected));
    }

    /// The declared type of `SERVERPROPERTY`, as `server_property_return_type` builds it in
    /// `lib.rs`.
    fn declared() -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true)
    }

    #[test]
    fn property_names_lists_known_properties_only() {
        for name in PROPERTY_NAMES {
            let value = server_property(name, &ctx());
            if NULL_PROPERTY_NAMES.contains(name) {
                assert_eq!(value, Value::Null, "{name}");
            } else {
                assert_ne!(value, Value::Null, "{name} is not a property of the table");
            }
        }
        // A misspelt name is indistinguishable from an unknown one: the check above is what
        // keeps the list in step with the `match`.
        assert_eq!(server_property("EngineEditionn", &ctx()), Value::Null);
    }

    #[test]
    fn an_integer_property_reads_as_the_declared_type() {
        // The table keeps the base type SQL Server stores in the `sql_variant`...
        assert_eq!(server_property("EngineEdition", &ctx()), Value::I32(3));
        // ...and the call reads it under the declared `nvarchar(128)`.
        assert_eq!(
            server_property_as("EngineEdition", &ctx(), &declared()).expect("no conversion error"),
            text("3")
        );
        assert_eq!(
            server_property_as("VaubanDB", &ctx(), &declared()).expect("no conversion error"),
            text("1")
        );
        assert_eq!(
            server_property_as("IsClustered", &ctx(), &declared()).expect("no conversion error"),
            text("0")
        );
        assert_eq!(
            server_property_as("ComparisonStyle", &ctx(), &declared())
                .expect("no conversion error"),
            text("196609")
        );
    }

    #[test]
    fn a_text_property_and_an_unknown_name_are_unchanged() {
        assert_eq!(
            server_property_as("ProductVersion", &ctx(), &declared()).expect("no conversion error"),
            text(PRODUCT_VERSION)
        );
        assert_eq!(
            server_property_as("NoSuchProperty", &ctx(), &declared()).expect("no conversion error"),
            Value::Null
        );
    }

    #[test]
    fn base_type_of_the_two_variants_the_table_produces() {
        assert_eq!(base_type(&Value::I32(3)).map(|t| t.ty), Some(SqlType::Int));
        assert_eq!(
            base_type(&text("x")).map(|t| t.ty),
            Some(SqlType::NVarChar(Len::Fixed(128)))
        );
        assert_eq!(base_type(&Value::Null), None);
    }

    #[test]
    fn properties_without_a_backing_capability_are_null() {
        for name in [
            "InstanceDefaultDataPath",
            "ResourceVersion",
            "ResourceLastUpdateDateTime",
        ] {
            assert_eq!(server_property(name, &ctx()), Value::Null, "{name}");
        }
    }
}
