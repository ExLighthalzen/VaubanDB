//! Server functions read by SSMS and the drivers at connection time:
//! `CONNECTIONPROPERTY`, `SESSIONPROPERTY`, `HAS_DBACCESS`, `ORIGINAL_LOGIN`,
//! `IS_SRVROLEMEMBER`, `IS_MEMBER` and `HAS_PERMS_BY_NAME`.
//!
//! V1 exposes a single sysadmin principal: the answers are those of an administrator,
//! without a real principal or permission catalogue yet.

use vauban_errors::SqlResult;
use vauban_sysfn::{Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind, lookup, register};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value, convert};

/// Fixed server roles of SQL Server ([Learn](https://learn.microsoft.com/sql/relational-databases/security/authentication-access/server-level-roles)).
const FIXED_SERVER_ROLES: &[&str] = &[
    "sysadmin",
    "serveradmin",
    "setupadmin",
    "securityadmin",
    "processadmin",
    "dbcreator",
    "diskadmin",
    "bulkadmin",
];

/// Fixed database roles ([Learn](https://learn.microsoft.com/sql/relational-databases/security/authentication-access/database-level-roles)).
const FIXED_DATABASE_ROLES: &[&str] = &[
    "public",
    "db_owner",
    "db_accessadmin",
    "db_securityadmin",
    "db_ddladmin",
    "db_datareader",
    "db_datawriter",
    "db_denydatareader",
    "db_denydatawriter",
];

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

fn string_arg(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(s)) => Some(s.text.as_str()),
        _ => None,
    }
}

fn is_fixed_server_role(name: &str) -> bool {
    FIXED_SERVER_ROLES
        .iter()
        .any(|role| role.eq_ignore_ascii_case(name))
}

fn is_fixed_database_role(name: &str) -> bool {
    FIXED_DATABASE_ROLES
        .iter()
        .any(|role| role.eq_ignore_ascii_case(name))
}

fn connection_property(name: &str, ctx: &dyn EvalContext) -> Value {
    match name.to_ascii_uppercase().as_str() {
        "NET_TRANSPORT" => text("TCP"),
        "PROTOCOL_TYPE" => text("TSQL"),
        "AUTH_SCHEME" => text("SQL"),
        "LOCAL_NET_ADDRESS" => ctx.local_net_address().map(text).unwrap_or(Value::Null),
        "LOCAL_TCP_PORT" => ctx.local_tcp_port().map(Value::I32).unwrap_or(Value::Null),
        "CLIENT_NET_ADDRESS" => ctx.client_net_address().map(text).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn connection_property_base_type(value: &Value) -> Option<TypeInfo> {
    let ty = match value {
        Value::String(_) => SqlType::NVarChar(Len::Fixed(128)),
        Value::I32(_) => SqlType::Int,
        _ => return None,
    };
    Some(TypeInfo::new(ty, true))
}

fn connection_property_as(
    name: &str,
    ctx: &dyn EvalContext,
    declared: &TypeInfo,
) -> SqlResult<Value> {
    let value = connection_property(name, ctx);
    match connection_property_base_type(&value) {
        Some(base) if base.ty != declared.ty => convert(&value, &base, declared, None),
        Some(_) => Ok(value),
        None => Ok(Value::Null),
    }
}

fn session_property(name: &str, ctx: &dyn EvalContext) -> Value {
    ctx.session_property(name)
        .map(Value::I32)
        .unwrap_or(Value::Null)
}

fn has_dbaccess(name: &str, ctx: &dyn EvalContext) -> Value {
    if ctx.database_id(Some(name)).is_some() {
        Value::I32(1)
    } else {
        Value::Null
    }
}

fn original_login(ctx: &dyn EvalContext) -> Value {
    match ctx.login_name() {
        Some(login) => text(login),
        None => Value::Null,
    }
}

fn is_srvrolemember(args: &[Value], ctx: &dyn EvalContext) -> Value {
    let role = match string_arg(args.first()) {
        Some(name) => name,
        None => return Value::Null,
    };
    if !is_fixed_server_role(role) {
        return Value::Null;
    }
    if args.len() == 2 {
        let login = match string_arg(args.get(1)) {
            Some(name) => name,
            None => return Value::Null,
        };
        match ctx.login_name() {
            Some(current) if current.eq_ignore_ascii_case(login) => Value::I32(1),
            _ => Value::Null,
        }
    } else {
        Value::I32(1)
    }
}

fn is_member(name: &str, ctx: &dyn EvalContext) -> Value {
    let _ = ctx;
    if is_fixed_database_role(name) {
        Value::I32(1)
    } else {
        Value::Null
    }
}

fn has_perms_by_name(entity: Option<&str>, scope: &str, ctx: &dyn EvalContext) -> Value {
    match scope.to_ascii_uppercase().as_str() {
        "SERVER" => Value::Null,
        "DATABASE" => {
            let Some(name) = entity else {
                return Value::Null;
            };
            if ctx.database_id(Some(name)).is_some() {
                Value::I32(1)
            } else {
                Value::Null
            }
        }
        "OBJECT" => {
            let Some(name) = entity else {
                return Value::Null;
            };
            if ctx.object_id(name).is_some() {
                Value::I32(1)
            } else {
                Value::I32(0)
            }
        }
        "SCHEMA" => {
            let Some(name) = entity else {
                return Value::Null;
            };
            if ctx.schema_id(Some(name)).is_some() {
                Value::I32(1)
            } else {
                Value::I32(0)
            }
        }
        _ => Value::Null,
    }
}

fn sql_variant_as_nvarchar_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

fn int_nullable_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, true))
}

fn connection_property_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    match string_arg(args.values.first()) {
        Some(name) => connection_property_as(name, ctx, args.result),
        None => Ok(Value::Null),
    }
}

fn session_property_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    match string_arg(args.values.first()) {
        Some(name) => Ok(session_property(name, ctx)),
        None => Ok(Value::Null),
    }
}

fn has_dbaccess_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    match string_arg(args.values.first()) {
        Some(name) => Ok(has_dbaccess(name, ctx)),
        None => Ok(Value::Null),
    }
}

fn original_login_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(original_login(ctx))
}

fn is_srvrolemember_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(is_srvrolemember(args.values, ctx))
}

fn is_member_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    match string_arg(args.values.first()) {
        Some(name) => Ok(is_member(name, ctx)),
        None => Ok(Value::Null),
    }
}

fn has_perms_by_name_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let entity = match args.values.first() {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.text.as_str()),
        _ => return Ok(Value::Null),
    };
    let scope = match string_arg(args.values.get(1)) {
        Some(name) => name,
        None => return Ok(Value::Null),
    };
    let _permission = match string_arg(args.values.get(2)) {
        Some(name) => name,
        None => return Ok(Value::Null),
    };
    Ok(has_perms_by_name(entity, scope, ctx))
}

const CONNECTION_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "CONNECTIONPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: sql_variant_as_nvarchar_return_type,
    eval: connection_property_eval,
    aggregate: None,
};

const SESSION_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "SESSIONPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: int_nullable_return_type,
    eval: session_property_eval,
    aggregate: None,
};

const HAS_DBACCESS_DEF: FunctionDef = FunctionDef {
    name: "HAS_DBACCESS",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: int_nullable_return_type,
    eval: has_dbaccess_eval,
    aggregate: None,
};

const ORIGINAL_LOGIN_DEF: FunctionDef = FunctionDef {
    name: "ORIGINAL_LOGIN",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: sql_variant_as_nvarchar_return_type,
    eval: original_login_eval,
    aggregate: None,
};

const IS_SRVROLEMEMBER_DEF: FunctionDef = FunctionDef {
    name: "IS_SRVROLEMEMBER",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(1, 2),
    return_type: int_nullable_return_type,
    eval: is_srvrolemember_eval,
    aggregate: None,
};

const IS_MEMBER_DEF: FunctionDef = FunctionDef {
    name: "IS_MEMBER",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: int_nullable_return_type,
    eval: is_member_eval,
    aggregate: None,
};

const HAS_PERMS_BY_NAME_DEF: FunctionDef = FunctionDef {
    name: "HAS_PERMS_BY_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(3),
    return_type: int_nullable_return_type,
    eval: has_perms_by_name_eval,
    aggregate: None,
};

/// Registers the seven server functions of this module when their names are free.
pub(crate) fn register_server_functions() {
    for def in [
        CONNECTION_PROPERTY_DEF,
        SESSION_PROPERTY_DEF,
        HAS_DBACCESS_DEF,
        ORIGINAL_LOGIN_DEF,
        IS_SRVROLEMEMBER_DEF,
        IS_MEMBER_DEF,
        HAS_PERMS_BY_NAME_DEF,
    ] {
        if lookup(def.name).is_none() {
            register(def);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_sysfn::StaticContext;

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
    fn register_server_functions_makes_all_names_visible() {
        crate::register_functions();
        for name in [
            "CONNECTIONPROPERTY",
            "SESSIONPROPERTY",
            "HAS_DBACCESS",
            "ORIGINAL_LOGIN",
            "IS_SRVROLEMEMBER",
            "IS_MEMBER",
            "HAS_PERMS_BY_NAME",
        ] {
            assert!(lookup(name).is_some(), "{name} must be registered");
        }
    }

    #[test]
    fn connection_property_constants_and_null_endpoints() {
        crate::register_functions();
        let def = lookup("CONNECTIONPROPERTY").expect("registered");
        let ctx = StaticContext::default();
        let arg = |name: &str| {
            Value::String(SqlString {
                text: name.to_owned(),
            })
        };
        assert_eq!(eval(def, &[arg("net_transport")], &ctx), text("TCP"));
        assert_eq!(eval(def, &[arg("protocol_type")], &ctx), text("TSQL"));
        assert_eq!(eval(def, &[arg("auth_scheme")], &ctx), text("SQL"));
        assert_eq!(eval(def, &[arg("local_net_address")], &ctx), Value::Null);
        assert_eq!(eval(def, &[arg("local_tcp_port")], &ctx), Value::Null);
        assert_eq!(eval(def, &[arg("client_net_address")], &ctx), Value::Null);
    }

    #[test]
    fn session_property_reads_default_connection_options() {
        crate::register_functions();
        let def = lookup("SESSIONPROPERTY").expect("registered");
        let ctx = StaticContext::default();
        let arg = |name: &str| {
            Value::String(SqlString {
                text: name.to_owned(),
            })
        };
        assert_eq!(eval(def, &[arg("ANSI_NULLS")], &ctx), Value::I32(1));
        assert_eq!(eval(def, &[arg("ARITHABORT")], &ctx), Value::I32(0));
        assert_eq!(eval(def, &[arg("nosuch")], &ctx), Value::Null);
    }

    #[test]
    fn original_login_reads_the_context_login() {
        crate::register_functions();
        let def = lookup("ORIGINAL_LOGIN").expect("registered");
        let ctx = StaticContext {
            login_name: Some("sa".to_owned()),
            ..StaticContext::default()
        };
        assert_eq!(eval(def, &[], &ctx), text("sa"));
    }

    #[test]
    fn is_srvrolemember_sysadmin_and_unknown_role() {
        crate::register_functions();
        let def = lookup("IS_SRVROLEMEMBER").expect("registered");
        let ctx = StaticContext {
            login_name: Some("sa".to_owned()),
            ..StaticContext::default()
        };
        let arg = |name: &str| {
            Value::String(SqlString {
                text: name.to_owned(),
            })
        };
        assert_eq!(eval(def, &[arg("sysadmin")], &ctx), Value::I32(1));
        assert_eq!(eval(def, &[arg("nosuch")], &ctx), Value::Null);
        assert_eq!(
            eval(def, &[arg("sysadmin"), arg("nosuch")], &ctx),
            Value::Null
        );
    }

    #[test]
    fn is_member_db_owner_and_unknown() {
        crate::register_functions();
        let def = lookup("IS_MEMBER").expect("registered");
        let ctx = StaticContext::default();
        let arg = |name: &str| {
            Value::String(SqlString {
                text: name.to_owned(),
            })
        };
        assert_eq!(eval(def, &[arg("db_owner")], &ctx), Value::I32(1));
        assert_eq!(eval(def, &[arg("nosuch")], &ctx), Value::Null);
    }
}
