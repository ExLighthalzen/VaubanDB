//! Crate `vauban-compat`: what clients expect beyond T-SQL.
//!
//! The server functions `@@VERSION`, `SERVERPROPERTY` and `DATABASEPROPERTYEX`, and the
//! ODBC type-info procedure `sp_datatype_info_100`. The version constants
//! ([`PRODUCT_VERSION`], [`VERSION_BANNER`]) are re-exported from `session`, because
//! `compat` depends on it; [`register_functions`] registers the server functions into the
//! `sysfn` registry and the procedure dispatcher into `session`.

mod database_properties;
mod server_properties;
mod sp_datatype_info;
mod version;

use std::sync::Once;

use vauban_errors::SqlResult;
use vauban_sysfn::{Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

pub use sp_datatype_info::{ResultColumn, ResultSetData, sp_datatype_info_100};
pub use version::{PRODUCT_VERSION, VERSION_BANNER};

/// Result of a compatibility procedure, before `session` turns it into TDS tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemProcResult {
    /// Metadata and rows of the single result set.
    pub result_set: ResultSetData,
    /// Stored-procedure return status.
    pub return_status: i32,
}

/// Looks up and executes the system procedure named by an RPC.
///
/// Names are ASCII case-insensitive; brackets and the optional `sys.` prefix are ignored.
/// Unknown names return `None`, allowing `session` to preserve error 2812.
pub fn call_system_procedure(
    name: &str,
    params: &[(Option<&str>, &Value)],
) -> Option<SystemProcResult> {
    let normalized = name.replace(['[', ']'], "").to_ascii_lowercase();
    let normalized = normalized
        .strip_prefix("sys.")
        .unwrap_or(&normalized)
        .to_owned();
    if normalized != "sp_datatype_info_100" {
        return None;
    }

    let integer = |value: &Value| match value {
        Value::I16(value) => Some(i32::from(*value)),
        Value::I32(value) => Some(*value),
        Value::I64(value) => i32::try_from(*value).ok(),
        _ => None,
    };
    let data_type = params.first().and_then(|(_, value)| integer(value))?;
    let odbc_ver = params
        .get(1)
        .and_then(|(_, value)| integer(value))
        .unwrap_or(4);

    Some(SystemProcResult {
        result_set: sp_datatype_info_100(data_type, odbc_ver),
        return_status: 0,
    })
}

fn dispatch_system_procedure(
    name: &str,
    params: &[(Option<&str>, &Value)],
) -> Option<vauban_session::SystemProcedureResult> {
    call_system_procedure(name, params).map(|result| vauban_session::SystemProcedureResult {
        columns: result
            .result_set
            .columns
            .into_iter()
            .map(|column| vauban_session::SystemProcedureColumn {
                name: column.name,
                ty: column.ty,
            })
            .collect(),
        rows: result.result_set.rows,
        return_status: result.return_status,
    })
}

/// Result type of `SELECT @@VERSION`.
///
/// Declared `nvarchar(max)`, nullable. SQL Server types `@@VERSION` as `nvarchar(300)`
/// nullable: the `nvarchar(max)` declaration is a deliberate difference from SQL Server.
fn version_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Max), true))
}

/// Evaluates `@@VERSION`: the banner of the evaluation context
/// ([`EvalContext::version_banner`]) when the operator has overridden it, else the
/// VaubanDB default [`VERSION_BANNER`].
fn version_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::String(SqlString {
        text: ctx.version_banner().unwrap_or(VERSION_BANNER).to_owned(),
    }))
}

/// Result type of `SERVERPROPERTY(name)`.
///
/// SQL Server returns `sql_variant`, which the V1 type system does not represent:
/// `nvarchar(128)` nullable is declared instead, a deliberate difference from SQL Server.
/// The argument list holds the types of the arguments, not their literal text, so this
/// function cannot pick `int` for `'EngineEdition'` and `nvarchar` for `'Edition'`: one
/// declared type serves the whole table.
///
/// The table of properties keeps the base type SQL Server stores in the `sql_variant`
/// (`Value::I32` for an integer property), and [`server_property_eval`] converts the value
/// to the type declared here: a client that reads `SERVERPROPERTY('EngineEdition')` without
/// a cast gets the `nvarchar` `3`. The declared type is the provisional part here, not the
/// value.
fn server_property_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

/// Evaluates `SERVERPROPERTY(name)`. A `NULL` or non-character argument yields `NULL`,
/// like an unknown property name.
///
/// The value is returned under `args.result`, the type
/// [`server_property_return_type`] declared for the call: the row the executor builds and
/// the TDS token the encoder writes both read the declared type, and a value of another
/// variant would make the session drop the connection instead of sending a row.
fn server_property_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    match args.values.first() {
        Some(Value::String(name)) => {
            server_properties::server_property_as(&name.text, ctx, args.result)
        }
        _ => Ok(Value::Null),
    }
}

/// Result type of `DATABASEPROPERTYEX(database, property)`.
///
/// SQL Server returns `sql_variant`, nullable, like `SERVERPROPERTY`: for a text property
/// such as `'Collation'` as for an integer one such as `'Version'`. The V1 type system does
/// not represent `sql_variant`, so `nvarchar(128)` nullable is declared instead, and
/// `database_properties::database_property_as` renders the value under it.
fn database_property_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

/// Evaluates `DATABASEPROPERTYEX(database, property)`. A `NULL` or non-character argument
/// yields `NULL`, as an unknown database name or an unknown property name does.
///
/// The value is returned under `args.result`, the type [`database_property_return_type`]
/// declared for the call, for the reason `server_property_eval` states.
fn database_property_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    match (args.values.first(), args.values.get(1)) {
        (Some(Value::String(database)), Some(Value::String(name))) => {
            database_properties::database_property_as(&database.text, &name.text, ctx, args.result)
        }
        _ => Ok(Value::Null),
    }
}

/// `@@VERSION` is not folded as a constant. On SQL Server,
/// `SELECT ISNULL(CAST(NULL AS nvarchar(4000)), @@VERSION) WHERE 1=0;` has COLMETADATA
/// type 0xE7 and fNullable=1; a deterministic definition would fold that shape and
/// declare it fNullable=0. `SELECT @@VERSION WHERE 1=0;` is nullable as well, which is
/// the binder's `FunctionNullability::Always` classification.
const VERSION_DEF: FunctionDef = FunctionDef {
    name: "@@VERSION",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: version_return_type,
    eval: version_eval,
    aggregate: None,
};

/// `SERVERPROPERTY` is non-deterministic and `FunctionNullability::Always` in the binder.
/// On SQL Server, `SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) WHERE
/// 1=0;` and the same cast with NULL as its argument have fNullable=1, and so does the
/// first expression wrapped in `ISNULL(CAST(NULL AS nvarchar(128)), ...)`, which tells it
/// apart from a folded non-null constant. The uncast `SELECT
/// SERVERPROPERTY('ProductVersion')` and `SELECT SERVERPROPERTY(NULL)` are typed
/// `sql_variant`, nullable, there.
const SERVER_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "SERVERPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: server_property_return_type,
    eval: server_property_eval,
    aggregate: None,
};

/// `DATABASEPROPERTYEX` is non-deterministic and nullable, like `SERVERPROPERTY`: its
/// answer depends on the databases the instance holds, which a second call may not find in
/// the same state. SQL Server types both `SELECT DATABASEPROPERTYEX('master', 'Collation')`
/// and `SELECT DATABASEPROPERTYEX('master', 'Version')` as nullable. Arity `Exact(2)`:
/// `SELECT DATABASEPROPERTYEX('master');` and `SELECT DATABASEPROPERTYEX('master',
/// 'Status', 'x');` both answer error 174, severity 15, state 1, which is what `sysfn`'s
/// `check_call` builds from `Exact(2)`.
const DATABASE_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "DATABASEPROPERTYEX",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(2),
    return_type: database_property_return_type,
    eval: database_property_eval,
    aggregate: None,
};

/// Registers the server functions of this crate (`@@VERSION`, `SERVERPROPERTY`,
/// `DATABASEPROPERTYEX`) in the `sysfn` registry.
///
/// Idempotent: the registration happens once per process, later calls are no-ops. Meant
/// to be called at start-up, before any query is bound.
///
/// # Panics
///
/// When another crate already registered one of these names: a programming error
/// detected at initialisation, as `sysfn::register` documents.
pub fn register_functions() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        vauban_sysfn::register(VERSION_DEF);
        vauban_sysfn::register(SERVER_PROPERTY_DEF);
        vauban_sysfn::register(DATABASE_PROPERTY_DEF);
        vauban_session::register_system_procedure_dispatcher(dispatch_system_procedure);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_sysfn::{StaticContext, lookup};

    fn text(value: &Value) -> &str {
        match value {
            Value::String(s) => &s.text,
            other => panic!("expected a string, got {other:?}"),
        }
    }

    /// Calls `def.eval` with an [`EvalArgs`] built for `values`: every argument is typed
    /// `nvarchar(20)` and the result type is the one the definition declares.
    fn eval(def: &FunctionDef, values: &[Value], ctx: &dyn EvalContext) -> Value {
        let types: Vec<TypeInfo> = values
            .iter()
            .map(|_| TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false))
            .collect();
        let result = (def.return_type)(&types).expect("return_type must succeed");
        let args = EvalArgs {
            values,
            types: &types,
            result: &result,
        };
        (def.eval)(&args, ctx).expect("eval must succeed")
    }

    #[test]
    fn register_functions_makes_both_names_visible() {
        register_functions();
        let version = lookup("@@version").expect("@@VERSION must be registered");
        assert_eq!(version.name, "@@VERSION");
        assert_eq!(version.kind, FunctionKind::Scalar);
        assert!(!version.deterministic);
        assert_eq!(version.arity, Arity::Exact(0));

        let property = lookup("ServerProperty").expect("SERVERPROPERTY must be registered");
        assert_eq!(property.name, "SERVERPROPERTY");
        assert_eq!(property.kind, FunctionKind::Scalar);
        assert!(!property.deterministic);
        assert_eq!(property.arity, Arity::Exact(1));
    }

    #[test]
    fn register_functions_twice_does_not_panic() {
        register_functions();
        register_functions();
        assert!(lookup("@@VERSION").is_some());
        assert!(lookup("SERVERPROPERTY").is_some());
        assert!(lookup("DATABASEPROPERTYEX").is_some());
    }

    #[test]
    fn register_functions_makes_databasepropertyex_visible() {
        register_functions();
        let def = lookup("databasepropertyex").expect("DATABASEPROPERTYEX must be registered");
        assert_eq!(def.name, "DATABASEPROPERTYEX");
        assert_eq!(def.kind, FunctionKind::Scalar);
        assert!(!def.deterministic);
        assert_eq!(def.arity, Arity::Exact(2));

        let arg_types = [
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false),
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false),
        ];
        let ty = (def.return_type)(&arg_types).expect("return_type must succeed");
        assert_eq!(ty.ty, SqlType::NVarChar(Len::Fixed(128)));
        assert!(ty.nullable);
    }

    /// `StaticContext::database_id` keeps the trait default `None`: the call answers `NULL`
    /// for a database name the context cannot identify, which is also what SQL Server answers
    /// for a database that does not exist.
    #[test]
    fn database_property_eval_reads_null_without_a_database_id() {
        register_functions();
        let def = lookup("DATABASEPROPERTYEX").expect("DATABASEPROPERTYEX must be registered");
        let ctx = StaticContext::default();
        let arg = |name: &str| {
            Value::String(SqlString {
                text: name.to_owned(),
            })
        };
        for name in ["Collation", "Status", "Version", "NoSuchProperty"] {
            assert_eq!(
                eval(def, &[arg("master"), arg(name)], &ctx),
                Value::Null,
                "{name}"
            );
        }
        // Four more shapes that give NULL: a NULL first argument, a NULL second one, a
        // non-string first one, and a call of arity 1 then of arity 0.
        assert_eq!(eval(def, &[Value::Null, arg("Status")], &ctx), Value::Null);
        assert_eq!(eval(def, &[arg("master"), Value::Null], &ctx), Value::Null);
        assert_eq!(
            eval(def, &[Value::I32(1), arg("Status")], &ctx),
            Value::Null
        );
        assert_eq!(eval(def, &[arg("master")], &ctx), Value::Null);
        assert_eq!(eval(def, &[], &ctx), Value::Null);
    }

    #[test]
    fn version_eval_returns_the_banner() {
        register_functions();
        let def = lookup("@@VERSION").expect("@@VERSION must be registered");
        let ctx = StaticContext::default();
        let value = eval(def, &[], &ctx);
        assert_eq!(text(&value), VERSION_BANNER);

        let ty = (def.return_type)(&[]).expect("return_type must succeed");
        assert_eq!(ty.ty, SqlType::NVarChar(Len::Max));
        assert!(ty.nullable);
    }

    #[test]
    fn version_eval_uses_the_context_banner_when_present() {
        register_functions();
        let def = lookup("@@VERSION").expect("@@VERSION must be registered");
        let ctx = StaticContext {
            version_banner: Some("CustomBanner".into()),
            ..StaticContext::default()
        };
        let value = eval(def, &[], &ctx);
        assert_eq!(text(&value), "CustomBanner");
    }

    #[test]
    fn server_property_eval_uses_the_table_and_the_context() {
        register_functions();
        let def = lookup("SERVERPROPERTY").expect("SERVERPROPERTY must be registered");
        let ctx = StaticContext {
            server_name: "VAUBAN".to_owned(),
            ..StaticContext::default()
        };
        let arg = |name: &str| {
            Value::String(SqlString {
                text: name.to_owned(),
            })
        };

        let value = eval(def, &[arg("ProductVersion")], &ctx);
        assert_eq!(text(&value), PRODUCT_VERSION);
        let value = eval(def, &[arg("servername")], &ctx);
        assert_eq!(text(&value), "VAUBAN");
        // An integer property reads under the declared `nvarchar(128)`.
        let value = eval(def, &[arg("EngineEdition")], &ctx);
        assert_eq!(text(&value), "3");
        let value = eval(def, &[arg("VaubanDB")], &ctx);
        assert_eq!(text(&value), "1");

        // NULL, a non-string argument and a missing argument all give NULL.
        assert_eq!(eval(def, &[Value::Null], &ctx), Value::Null);
        assert_eq!(eval(def, &[Value::I32(1)], &ctx), Value::Null);
        assert_eq!(eval(def, &[], &ctx), Value::Null);

        let ty = (def.return_type)(&[TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false)])
            .expect("return_type must succeed");
        assert_eq!(ty.ty, SqlType::NVarChar(Len::Fixed(128)));
        assert!(ty.nullable);
    }

    /// Walks the whole property table: a value of a variant the declared type does not
    /// describe would close the connection without an error token, because the TDS encoder
    /// refuses a row whose value and column type disagree.
    #[test]
    fn every_property_reads_as_the_declared_type() {
        register_functions();
        let def = lookup("SERVERPROPERTY").expect("SERVERPROPERTY must be registered");
        let ctx = StaticContext {
            server_name: "VAUBAN".to_owned(),
            ..StaticContext::default()
        };
        let arg_types = [TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false)];
        let declared = (def.return_type)(&arg_types).expect("return_type must succeed");
        assert_eq!(declared.ty, SqlType::NVarChar(Len::Fixed(128)));
        assert!(declared.nullable);

        let mut non_null = 0;
        for name in server_properties::PROPERTY_NAMES {
            let value = eval(
                def,
                &[Value::String(SqlString {
                    text: (*name).to_owned(),
                })],
                &ctx,
            );
            match value {
                // The declared type is nullable: a property may answer `NULL`.
                Value::Null => {}
                Value::String(_) => non_null += 1,
                other => panic!("{name} reads as {other:?}, not as {:?}", declared.ty),
            }
        }
        // 36 names in the table, of which 5 answer NULL (`NULL_PROPERTY_NAMES` in
        // `server_properties.rs`): 31 values carry text.
        assert_eq!(non_null, 31);
    }
}
