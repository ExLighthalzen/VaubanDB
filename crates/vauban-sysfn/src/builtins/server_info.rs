//! What a client asks about the server, its logins and its files: `SUSER_SID`, `SID_BINARY`,
//! `@@SERVICENAME`, `@@MAX_PRECISION`, `FILEPROPERTY`, `FULLTEXTSERVICEPROPERTY` and
//! `FULLTEXTCATALOGPROPERTY`.
//!
//! The seven are scalar and non-deterministic (configuration, security and metadata functions)
//! (`server_info_functions_are_registered`). Those that need a fact of the instance read it
//! through the [`EvalContext`]: the SID a login name carries
//! ([`EvalContext::server_principal_sid`]), the file of the current database a name designates
//! ([`EvalContext::database_file`]) and the service name ([`EvalContext::service_name`]). A
//! context without them answers `NULL` (`suser_sid_reads_the_context`,
//! `file_property_reads_the_file_facts`, `service_name_and_max_precision`).
//!
//! `SID_BINARY` reads the numeric form of a security identifier and nothing else
//! (`sid_binary_encodes_the_numeric_form`); the two-letter aliases of the security descriptor
//! language (`BA`, `SY`) answer `NULL` here.
//!
//! VaubanDB installs no full-text search: `FULLTEXTSERVICEPROPERTY('IsFullTextInstalled')` is
//! `0`, and the catalogue holds no full-text catalog for `FULLTEXTCATALOGPROPERTY` to describe.

use std::cmp::Ordering;

use vauban_errors::SqlResult;
use vauban_types::{Collation, Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, convert};

use crate::builtins::args::invalid_argument_type;
use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// The largest precision of `decimal` and `numeric`, answered by `@@MAX_PRECISION`.
const MAX_PRECISION: u8 = 38;

/// The largest number of sub-authorities a security identifier carries.
const MAX_SUB_AUTHORITIES: usize = 15;

/// The largest identifier authority: six bytes.
const MAX_AUTHORITY: u64 = (1 << 48) - 1;

/// The revision of the security identifier format; a text naming another revision answers
/// `NULL` (`sid_binary_encodes_the_numeric_form`).
const SID_REVISION: u64 = 1;

/// Result type of `SUSER_SID` and `SID_BINARY`: `varbinary(85)`, nullable.
///
/// `SID_BINARY` fills at most 68 of the 85 bytes: eight of header, four per sub-authority, fifteen
/// sub-authorities (`sid_binary_encodes_the_numeric_form`).
fn sid_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::VarBinary(Len::Fixed(85)), true))
}

/// Result type of `SUSER_SID`, after the check of its second argument.
///
/// The second argument is accepted when it is an `int`, the bare `NULL` included, and refused
/// with 8116 otherwise: `SUSER_SID(N'sa', N'x')`, `SUSER_SID(N'sa', 1.5)` and
/// `SUSER_SID(N'sa', CAST(NULL AS bigint))` are refused alike (`suser_sid_types_its_second_argument`).
fn suser_sid_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    if let Some(info) = args.get(1)
        && info.ty != SqlType::Int
    {
        return Err(invalid_argument_type(&info.ty, 2, "SUSER_SID"));
    }
    sid_type(args)
}

/// Result type of `SID_BINARY`, after the check of its argument.
///
/// A character or binary argument is accepted; any other type, the bare `NULL` included (typed
/// `int` by the binder), is refused with 8116 (`sid_binary_types_its_argument`).
fn sid_binary_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    if let Some(info) = args.first()
        && !matches!(info.ty.family(), TypeFamily::Character | TypeFamily::Binary)
    {
        return Err(invalid_argument_type(&info.ty, 1, "SID_BINARY"));
    }
    sid_type(args)
}

/// Result type of `@@SERVICENAME`: `nvarchar(128)`, nullable.
fn service_name_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

/// Result type of `@@MAX_PRECISION`: `tinyint`, not nullable.
fn tinyint_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::TinyInt, false))
}

/// Result type of `FILEPROPERTY`, `FULLTEXTSERVICEPROPERTY` and `FULLTEXTCATALOGPROPERTY`:
/// `int`, nullable.
fn property_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// The `index`-th argument read as text: `Ok(None)` when it is absent or `NULL`.
///
/// A non-character argument is converted to `nvarchar(max)` the way an implicit conversion
/// would: `SUSER_SID(1)` looks the name `1` up, and a `varbinary` holding UTF-16 text reads as
/// that text.
fn text_argument(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<String>> {
    let Some(value) = args.values.get(index) else {
        return Ok(None);
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = TypeInfo::new(SqlType::NVarChar(Len::Max), true);
    match convert(value, &args.types[index], &target, None)? {
        Value::String(s) => Ok(Some(s.text)),
        _ => Ok(None),
    }
}

/// Whether `a` and `b` name the same thing under the default collation: case and trailing
/// blanks ignored, a leading blank significant.
fn same_name(a: &str, b: &str) -> bool {
    Collation::DEFAULT.compare(a, b) == Ordering::Equal
}

/// Evaluates `SUSER_SID([login [, param2]])`.
///
/// Without an argument, or with the empty string, the SID of the login of the session; with a
/// name, the SID the context publishes for it; `NULL` for a `NULL` name, a name the context does
/// not know, or a context without a session. `param2` is typed by [`suser_sid_type`] and its
/// value ignored: it selects a check against the Windows domain that a login of SQL Server
/// authentication does not need (`suser_sid_reads_the_context`).
fn suser_sid_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let name = if args.values.is_empty() {
        None
    } else {
        match text_argument(args, 0)? {
            Some(name) => Some(name),
            None => return Ok(Value::Null),
        }
    };
    let name = match name {
        Some(name) if !name.is_empty() => name,
        _ => match ctx.login_name() {
            Some(login) => login.to_owned(),
            None => return Ok(Value::Null),
        },
    };
    Ok(ctx
        .server_principal_sid(&name)
        .map_or(Value::Null, Value::Bytes))
}

/// One number of the numeric form of a security identifier: decimal digits, or `0x` / `0X`
/// followed by hexadecimal digits, leading zeros allowed. `None` for anything else, the empty
/// string included. A number too large saturates to `u64::MAX`.
fn sid_number(text: &str) -> Option<u64> {
    let (digits, radix) = match text.get(..2) {
        Some("0x" | "0X") => (&text[2..], 16),
        _ => (text, 10),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for c in digits.chars() {
        let digit = c.to_digit(radix)?;
        value = value
            .saturating_mul(u64::from(radix))
            .saturating_add(u64::from(digit));
    }
    Some(value)
}

/// The bytes of the security identifier `text` writes in its numeric form
/// `S-1-<authority>-<sub-authority>…`, `None` when `text` is not that form.
///
/// The layout: the revision (1), the number of sub-authorities, the authority on six bytes most
/// significant first, then each sub-authority on four bytes least significant first. The
/// authority must fit in six bytes; a sub-authority too large saturates to `0xFFFFFFFF`. One
/// to fifteen sub-authorities (`sid_binary_encodes_the_numeric_form`).
fn sid_from_text(text: &str) -> Option<Vec<u8>> {
    let rest = text
        .strip_prefix("S-")
        .or_else(|| text.strip_prefix("s-"))?;
    let parts: Vec<&str> = rest.split('-').collect();
    let (revision, authority, subs) = match parts.as_slice() {
        [revision, authority, subs @ ..] => (*revision, *authority, subs),
        _ => return None,
    };
    if subs.is_empty() || subs.len() > MAX_SUB_AUTHORITIES {
        return None;
    }
    if sid_number(revision)? != SID_REVISION {
        return None;
    }
    let authority = sid_number(authority)?;
    if authority > MAX_AUTHORITY {
        return None;
    }
    let mut bytes = Vec::with_capacity(8 + 4 * subs.len());
    bytes.push(1);
    bytes.push(u8::try_from(subs.len()).ok()?);
    bytes.extend_from_slice(&authority.to_be_bytes()[2..]);
    for sub in subs {
        let value = u32::try_from(sid_number(sub)?).unwrap_or(u32::MAX);
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Some(bytes)
}

/// Evaluates `SID_BINARY(sddl)`: the bytes of the numeric form, `NULL` for any other text.
fn sid_binary_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(text_argument(args, 0)?
        .and_then(|text| sid_from_text(&text))
        .map_or(Value::Null, Value::Bytes))
}

/// Evaluates `@@SERVICENAME`: the service name is the program name the context announces,
/// `NULL` for a context without one.
fn service_name_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(ctx.service_name().map_or(Value::Null, |name| {
        Value::String(SqlString {
            text: name.to_owned(),
        })
    }))
}

/// Evaluates `@@MAX_PRECISION`: 38.
fn max_precision_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::I8(MAX_PRECISION))
}

/// Evaluates `FILEPROPERTY(file, property)` on a file of the current database.
///
/// `IsLogFile`, `IsPrimaryFile` (the file of `file_id` 1) and `IsReadOnly` answer `1` or `0`;
/// `SpaceUsed` answers `0`, the engine not counting the pages a file holds. `NULL` for a file
/// the context does not know, another property, or a `NULL` argument. Both names compare under
/// the default collation (`file_property_reads_the_file_facts`).
fn file_property_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(file), Some(property)) = (text_argument(args, 0)?, text_argument(args, 1)?) else {
        return Ok(Value::Null);
    };
    let Some((file_id, file_type, is_read_only)) = ctx.database_file(&file) else {
        return Ok(Value::Null);
    };
    let answer = if same_name(&property, "IsLogFile") {
        file_type == 1
    } else if same_name(&property, "IsPrimaryFile") {
        file_id == 1
    } else if same_name(&property, "IsReadOnly") {
        is_read_only
    } else if same_name(&property, "SpaceUsed") {
        false
    } else {
        return Ok(Value::Null);
    };
    Ok(Value::I32(i32::from(answer)))
}

/// Evaluates `FULLTEXTSERVICEPROPERTY(property)`: `0` for `IsFullTextInstalled`, `NULL` for any
/// other name, the full-text service not being installed (`full_text_properties`).
fn fulltext_service_property_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(match text_argument(args, 0)? {
        Some(property) if same_name(&property, "IsFullTextInstalled") => Value::I32(0),
        _ => Value::Null,
    })
}

/// Evaluates `FULLTEXTCATALOGPROPERTY(catalog, property)`: `NULL`, no full-text catalog
/// existing to describe (`full_text_properties`).
fn fulltext_catalog_property_eval(
    _args: &EvalArgs<'_>,
    _ctx: &dyn EvalContext,
) -> SqlResult<Value> {
    Ok(Value::Null)
}

/// `SUSER_SID`: zero to two arguments.
const SUSER_SID_DEF: FunctionDef = FunctionDef {
    name: "SUSER_SID",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 2),
    return_type: suser_sid_type,
    eval: suser_sid_eval,
    aggregate: None,
};

/// `SID_BINARY`: one argument.
const SID_BINARY_DEF: FunctionDef = FunctionDef {
    name: "SID_BINARY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: sid_binary_type,
    eval: sid_binary_eval,
    aggregate: None,
};

/// `@@SERVICENAME`.
const SERVICE_NAME_DEF: FunctionDef = FunctionDef {
    name: "@@SERVICENAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: service_name_type,
    eval: service_name_eval,
    aggregate: None,
};

/// `@@MAX_PRECISION`.
const MAX_PRECISION_DEF: FunctionDef = FunctionDef {
    name: "@@MAX_PRECISION",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: tinyint_type,
    eval: max_precision_eval,
    aggregate: None,
};

/// `FILEPROPERTY`: two arguments.
const FILE_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "FILEPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(2),
    return_type: property_type,
    eval: file_property_eval,
    aggregate: None,
};

/// `FULLTEXTSERVICEPROPERTY`: one argument.
const FULLTEXT_SERVICE_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "FULLTEXTSERVICEPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: property_type,
    eval: fulltext_service_property_eval,
    aggregate: None,
};

/// `FULLTEXTCATALOGPROPERTY`: two arguments.
const FULLTEXT_CATALOG_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "FULLTEXTCATALOGPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(2),
    return_type: property_type,
    eval: fulltext_catalog_property_eval,
    aggregate: None,
};

/// Registers the functions of this module.
pub(crate) fn register_all() {
    register(SUSER_SID_DEF);
    register(SID_BINARY_DEF);
    register(SERVICE_NAME_DEF);
    register(MAX_PRECISION_DEF);
    register(FILE_PROPERTY_DEF);
    register(FULLTEXT_SERVICE_PROPERTY_DEF);
    register(FULLTEXT_CATALOG_PROPERTY_DEF);
}

#[cfg(test)]
mod tests;
