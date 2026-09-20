//! `sp_stored_procedures`: lists the procedures visible in the current database.

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::procedures::{ProcAction, ProcParam, SystemProc};
use crate::special_procs::ProcArg;

/// Parameter declaration for `sp_stored_procedures`.
#[derive(Debug, Clone, Copy)]
struct ParamSpec {
    canonical: &'static str,
    aliases: &'static [&'static str],
    required: bool,
    unicode: bool,
    bit_param: bool,
    default_null: bool,
    default_bit: Option<bool>,
}

const SP_STORED_PROCEDURES_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@sp_name",
        aliases: &["@sp_name"],
        required: false,
        unicode: true,
        bit_param: false,
        default_null: true,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@sp_owner",
        aliases: &["@sp_owner"],
        required: false,
        unicode: true,
        bit_param: false,
        default_null: true,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@sp_qualifier",
        aliases: &["@sp_qualifier"],
        required: false,
        unicode: true,
        bit_param: false,
        default_null: true,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@fUsePattern",
        aliases: &["@fUsePattern"],
        required: false,
        unicode: false,
        bit_param: true,
        default_null: false,
        default_bit: Some(true),
    },
];

pub(crate) const PROCS: &[SystemProc] = &[SystemProc {
    name: "sp_stored_procedures",
}];

/// Resolves `sp_stored_procedures`, the single procedure listed in [`PROCS`].
pub(crate) fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    if !PROCS.iter().any(|proc| proc.name == name) {
        return None;
    }
    let params =
        match bind_catalog_params("sp_stored_procedures", SP_STORED_PROCEDURES_PARAMS, args) {
            Ok(params) => params,
            Err(err) => return Some(Err(err)),
        };
    Some(Ok(ProcAction::Template {
        sql: SP_STORED_PROCEDURES.to_owned(),
        params,
    }))
}

fn bind_catalog_params(
    proc: &str,
    specs: &[ParamSpec],
    args: &[ProcArg<'_>],
) -> SqlResult<Vec<ProcParam>> {
    let mut values = vec![None; specs.len()];
    let mut positional = 0usize;
    let mut saw_named = false;

    for (index, arg) in args.iter().enumerate() {
        if let Some(name) = arg.name {
            saw_named = true;
            let Some(slot) = specs.iter().position(|spec| name_matches(spec, name)) else {
                return Err(SqlError::not_a_parameter(name, proc));
            };
            if values[slot].is_some() {
                return Err(SqlError::too_many_arguments(proc));
            }
            values[slot] = Some(bind_one(specs[slot], arg)?);
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        } else if positional >= specs.len() {
            return Err(SqlError::too_many_arguments(proc));
        } else {
            values[positional] = Some(bind_one(specs[positional], arg)?);
            positional += 1;
        }
    }

    let mut out = Vec::with_capacity(specs.len());
    for (spec, slot) in specs.iter().zip(values) {
        let param = match slot {
            Some(param) => param,
            None if spec.required => {
                return Err(SqlError::procedure_expects_parameter(proc, spec.canonical));
            }
            None => default_param(*spec),
        };
        out.push(param);
    }
    Ok(out)
}

fn bind_one(spec: ParamSpec, arg: &ProcArg<'_>) -> SqlResult<ProcParam> {
    // A literal `NULL` carries no type of its own; the checks below skip `arg.ty` for a
    // `NULL` value, so it binds to a `unicode` or `bit_param` slot without a type error
    // (`bind_one_accepts_null`, `catalog_procs.rs`).
    let is_null = matches!(arg.value, Value::Null);
    if spec.unicode && !is_null && !is_unicode_text(arg.ty) {
        return Err(SqlError::procedure_expects_type(
            spec.canonical,
            "ntext/nchar/nvarchar",
        ));
    }
    if spec.bit_param
        && !is_null
        && !matches!(arg.ty.ty, SqlType::Bit | SqlType::Int | SqlType::TinyInt)
    {
        return Err(SqlError::procedure_expects_type(spec.canonical, "bit"));
    }
    Ok(ProcParam {
        name: spec.canonical.to_owned(),
        ty: param_type(spec),
        value: arg.value.clone(),
        output: false,
    })
}

fn default_param(spec: ParamSpec) -> ProcParam {
    let value = if spec.default_null {
        Value::Null
    } else if let Some(bit) = spec.default_bit {
        Value::Bit(bit)
    } else {
        Value::Null
    };
    ProcParam {
        name: spec.canonical.to_owned(),
        ty: param_type(spec),
        value,
        output: false,
    }
}

fn param_type(spec: ParamSpec) -> TypeInfo {
    if spec.bit_param {
        TypeInfo::new(SqlType::Bit, true)
    } else {
        TypeInfo::new(SqlType::NVarChar(Len::Max), true)
    }
}

fn name_matches(spec: &ParamSpec, name: &str) -> bool {
    spec.aliases
        .iter()
        .any(|alias| alias.eq_ignore_ascii_case(name))
}

fn is_unicode_text(ty: &TypeInfo) -> bool {
    matches!(ty.ty, SqlType::NVarChar(_) | SqlType::NChar(_))
}

// `@sp_qualifier` given and different from `DB_NAME()` answers an empty result set with the
// same column shape rather than an error: SQL Server raises error 15250 on that input, which
// this batch cannot reproduce because `RAISERROR` is not yet served inside a catalog
// template. The empty answer matches SQL Server exactly when `@sp_qualifier` is the empty
// string rather than a name that names a different database.
const SP_STORED_PROCEDURES: &str = r#####"
IF @sp_qualifier IS NOT NULL AND DB_NAME() <> @sp_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_OWNER,
        CAST(NULL AS nvarchar(134)) AS PROCEDURE_NAME,
        CAST(NULL AS int) AS NUM_INPUT_PARAMS,
        CAST(NULL AS int) AS NUM_OUTPUT_PARAMS,
        CAST(NULL AS int) AS NUM_RESULT_SETS,
        CAST(NULL AS varchar(254)) AS REMARKS,
        CAST(NULL AS smallint) AS PROCEDURE_TYPE
    WHERE 1 = 0;
    RETURN;
END;

IF @fUsePattern = 0
BEGIN
    SELECT
        PROCEDURE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        PROCEDURE_OWNER = CAST(s.name AS nvarchar(128)),
        PROCEDURE_NAME = CAST(p.name + N';1' AS nvarchar(134)),
        NUM_INPUT_PARAMS = CAST(-1 AS int),
        NUM_OUTPUT_PARAMS = CAST(-1 AS int),
        NUM_RESULT_SETS = CAST(-1 AS int),
        REMARKS = CAST(NULL AS varchar(254)),
        PROCEDURE_TYPE = CAST(2 AS smallint)
    FROM sys.procedures p
    JOIN sys.schemas s ON s.schema_id = p.schema_id
    WHERE (@sp_name IS NULL OR p.name = @sp_name)
      AND (@sp_owner IS NULL OR s.name = @sp_owner)
    ORDER BY PROCEDURE_QUALIFIER, PROCEDURE_OWNER, PROCEDURE_NAME
END;
IF @fUsePattern = 1
BEGIN
    SELECT
        PROCEDURE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        PROCEDURE_OWNER = CAST(s.name AS nvarchar(128)),
        PROCEDURE_NAME = CAST(p.name + N';1' AS nvarchar(134)),
        NUM_INPUT_PARAMS = CAST(-1 AS int),
        NUM_OUTPUT_PARAMS = CAST(-1 AS int),
        NUM_RESULT_SETS = CAST(-1 AS int),
        REMARKS = CAST(NULL AS varchar(254)),
        PROCEDURE_TYPE = CAST(2 AS smallint)
    FROM sys.procedures p
    JOIN sys.schemas s ON s.schema_id = p.schema_id
    WHERE (@sp_name IS NULL OR p.name LIKE @sp_name)
      AND (@sp_owner IS NULL OR s.name LIKE @sp_owner)
    ORDER BY PROCEDURE_QUALIFIER, PROCEDURE_OWNER, PROCEDURE_NAME
END
"#####;
