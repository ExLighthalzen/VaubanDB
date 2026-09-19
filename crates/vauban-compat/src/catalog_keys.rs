//! Key catalog procedures (`sp_pkeys`, `sp_fkeys`, …).

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::procedures::{ProcAction, ProcParam, SystemProc};
use crate::special_procs::ProcArg;

/// Parameter declaration for a catalog procedure.
#[derive(Debug, Clone, Copy)]
struct ParamSpec {
    canonical: &'static str,
    aliases: &'static [&'static str],
    required: bool,
    unicode: bool,
    int_param: bool,
    bit_param: bool,
    char_param: bool,
    default_null: bool,
    default_int: Option<i32>,
    default_bit: Option<bool>,
    default_char: Option<char>,
}

const SP_PKEYS_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@table_name",
        aliases: &["@table_name"],
        required: true,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: false,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@table_owner",
        aliases: &["@table_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@table_qualifier",
        aliases: &["@table_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
];

const SP_FKEYS_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@pktable_name",
        aliases: &["@pktable_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@pkcolumn_name",
        aliases: &["@pkcolumn_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@fktable_name",
        aliases: &["@fktable_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@fkcolumn_name",
        aliases: &["@fkcolumn_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@pktable_qualifier",
        aliases: &["@pktable_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@fktable_qualifier",
        aliases: &["@fktable_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@pktable_owner",
        aliases: &["@pktable_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@fktable_owner",
        aliases: &["@fktable_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
];

const SP_STATISTICS_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@table_name",
        aliases: &["@table_name"],
        required: true,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: false,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@table_owner",
        aliases: &["@table_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@table_qualifier",
        aliases: &["@table_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@index_name",
        aliases: &["@index_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@is_unique",
        aliases: &["@is_unique"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@accuracy",
        aliases: &["@accuracy"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: false,
        char_param: true,
        default_null: false,
        default_int: None,
        default_bit: None,
        default_char: Some('Q'),
    },
];

const SP_SPECIAL_COLUMNS_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@table_name",
        aliases: &["@table_name"],
        required: true,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: false,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@table_owner",
        aliases: &["@table_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@table_qualifier",
        aliases: &["@table_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@col_type",
        aliases: &["@col_type"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: false,
        char_param: true,
        default_null: false,
        default_int: None,
        default_bit: None,
        default_char: Some('R'),
    },
    ParamSpec {
        canonical: "@scope",
        aliases: &["@scope"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: false,
        char_param: true,
        default_null: false,
        default_int: None,
        default_bit: None,
        default_char: Some('C'),
    },
    ParamSpec {
        canonical: "@nullable",
        aliases: &["@nullable"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@odbcver",
        aliases: &["@odbcver"],
        required: false,
        unicode: false,
        int_param: true,
        bit_param: false,
        char_param: false,
        default_null: false,
        default_int: Some(2),
        default_bit: None,
        default_char: None,
    },
];

const SP_SPROC_COLUMNS_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@procedure_name",
        aliases: &["@procedure_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@procedure_owner",
        aliases: &["@procedure_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@procedure_qualifier",
        aliases: &["@procedure_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@column_name",
        aliases: &["@column_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        char_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@ODBCVer",
        aliases: &["@ODBCVer"],
        required: false,
        unicode: false,
        int_param: true,
        bit_param: false,
        char_param: false,
        default_null: false,
        default_int: Some(2),
        default_bit: None,
        default_char: None,
    },
    ParamSpec {
        canonical: "@proc_type",
        aliases: &["@proc_type"],
        required: false,
        unicode: false,
        int_param: true,
        bit_param: false,
        char_param: false,
        default_null: false,
        default_int: Some(0),
        default_bit: None,
        default_char: None,
    },
];

pub(crate) const PROCS: &[SystemProc] = &[
    SystemProc { name: "sp_pkeys" },
    SystemProc {
        name: "sp_pkeys_100",
    },
    SystemProc { name: "sp_fkeys" },
    SystemProc {
        name: "sp_fkeys_100",
    },
    SystemProc {
        name: "sp_statistics",
    },
    SystemProc {
        name: "sp_statistics_100",
    },
    SystemProc {
        name: "sp_special_columns",
    },
    SystemProc {
        name: "sp_special_columns_100",
    },
    SystemProc {
        name: "sp_sproc_columns",
    },
    SystemProc {
        name: "sp_sproc_columns_100",
    },
];

/// Resolves a catalog procedure implemented in this module.
pub(crate) fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    if !PROCS.iter().any(|proc| proc.name == name) {
        return None;
    }
    match name {
        "sp_pkeys" | "sp_pkeys_100" => Some(resolve_template(
            "sp_pkeys",
            SP_PKEYS_PARAMS,
            SP_PKEYS,
            args,
        )),
        "sp_fkeys" | "sp_fkeys_100" => Some(resolve_sp_fkeys(args)),
        "sp_statistics" | "sp_statistics_100" => Some(resolve_template(
            "sp_statistics",
            SP_STATISTICS_PARAMS,
            SP_STATISTICS,
            args,
        )),
        "sp_special_columns" | "sp_special_columns_100" => Some(resolve_sp_special_columns(args)),
        "sp_sproc_columns" => Some(resolve_sp_sproc_columns(args, false)),
        "sp_sproc_columns_100" => Some(resolve_sp_sproc_columns(args, true)),
        _ => None,
    }
}

fn resolve_sp_fkeys(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let params = bind_catalog_params("sp_fkeys", SP_FKEYS_PARAMS, args)?;
    let pktable = params
        .iter()
        .find(|p| p.name == "@pktable_name")
        .map(|p| !matches!(p.value, Value::Null))
        .unwrap_or(false);
    let fktable = params
        .iter()
        .find(|p| p.name == "@fktable_name")
        .map(|p| !matches!(p.value, Value::Null))
        .unwrap_or(false);
    if !pktable && !fktable {
        return Err(SqlError::new(
            15252,
            16,
            1,
            "The primary or foreign key table name must be given.",
        )
        .with_line(0));
    }
    Ok(ProcAction::Template {
        sql: SP_FKEYS.to_owned(),
        params,
    })
}

fn resolve_sp_special_columns(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    for arg in args {
        if let Some(name) = arg.name
            && name.eq_ignore_ascii_case("@colType")
        {
            return Err(SqlError::not_a_parameter(name, "sp_special_columns"));
        }
    }
    resolve_template(
        "sp_special_columns",
        SP_SPECIAL_COLUMNS_PARAMS,
        SP_SPECIAL_COLUMNS,
        args,
    )
}

fn resolve_sp_sproc_columns(args: &[ProcArg<'_>], columns_100: bool) -> SqlResult<ProcAction> {
    let proc = if columns_100 {
        "sp_sproc_columns_100"
    } else {
        "sp_sproc_columns"
    };
    let sql = if columns_100 {
        SP_SPROC_COLUMNS_100
    } else {
        SP_SPROC_COLUMNS
    };
    resolve_template(proc, SP_SPROC_COLUMNS_PARAMS, sql, args)
}

fn resolve_template(
    proc: &str,
    specs: &[ParamSpec],
    sql: &str,
    args: &[ProcArg<'_>],
) -> SqlResult<ProcAction> {
    let params = bind_catalog_params(proc, specs, args)?;
    Ok(ProcAction::Template {
        sql: sql.to_owned(),
        params,
    })
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
            None => default_param(*spec)?,
        };
        out.push(param);
    }
    Ok(out)
}

fn bind_one(spec: ParamSpec, arg: &ProcArg<'_>) -> SqlResult<ProcParam> {
    if spec.unicode && !is_unicode_text(arg.ty) {
        return Err(SqlError::procedure_expects_type(
            spec.canonical,
            "ntext/nchar/nvarchar",
        ));
    }
    if spec.int_param
        && !matches!(
            arg.ty.ty,
            SqlType::Int | SqlType::SmallInt | SqlType::TinyInt
        )
    {
        return Err(SqlError::procedure_expects_type(spec.canonical, "int"));
    }
    if spec.bit_param && !matches!(arg.ty.ty, SqlType::Bit | SqlType::Int | SqlType::TinyInt) {
        return Err(SqlError::procedure_expects_type(spec.canonical, "bit"));
    }
    Ok(ProcParam {
        name: spec.canonical.to_owned(),
        ty: param_type(spec),
        value: arg.value.clone(),
        output: false,
    })
}

fn default_param(spec: ParamSpec) -> SqlResult<ProcParam> {
    let value = if spec.default_null {
        Value::Null
    } else if let Some(bit) = spec.default_bit {
        Value::Bit(bit)
    } else if let Some(int) = spec.default_int {
        Value::I32(int)
    } else if let Some(ch) = spec.default_char {
        Value::String(SqlString {
            text: ch.to_string(),
        })
    } else {
        Value::Null
    };
    Ok(ProcParam {
        name: spec.canonical.to_owned(),
        ty: param_type(spec),
        value,
        output: false,
    })
}

fn param_type(spec: ParamSpec) -> TypeInfo {
    if spec.bit_param {
        TypeInfo::new(SqlType::Bit, true)
    } else if spec.int_param {
        TypeInfo::new(SqlType::Int, true)
    } else if spec.char_param {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(1)), true)
    } else if spec.unicode {
        TypeInfo::new(SqlType::NVarChar(Len::Max), true)
    } else {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(254)), true)
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

const SP_PKEYS: &str = r#####"
DECLARE @qual_name nvarchar(769);
DECLARE @object_id int;
DECLARE @owner nvarchar(128);

IF @table_qualifier IS NOT NULL AND @table_qualifier <> '' AND DB_NAME() <> @table_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS KEY_SEQ,
        CAST(NULL AS nvarchar(128)) AS PK_NAME
    WHERE 1 = 0;
    RETURN;
END;

SET @owner = COALESCE(@table_owner, SCHEMA_NAME(SCHEMA_ID()));

SELECT @qual_name = QUOTENAME(@owner) + N'.' + QUOTENAME(@table_name);
SELECT @object_id = OBJECT_ID(@qual_name);

IF @object_id IS NULL
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS KEY_SEQ,
        CAST(NULL AS nvarchar(128)) AS PK_NAME
    WHERE 1 = 0;
    RETURN;
END;

SELECT
    TABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
    TABLE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
    TABLE_NAME = CAST(o.name AS nvarchar(128)),
    COLUMN_NAME = CAST(c.name AS nvarchar(128)),
    KEY_SEQ = CAST(ic.key_ordinal AS smallint),
    PK_NAME = CAST(kc.name AS nvarchar(128))
FROM sys.all_objects o
INNER JOIN sys.key_constraints kc
    ON kc.parent_object_id = o.object_id AND kc.type = N'PK'
INNER JOIN sys.index_columns ic
    ON ic.object_id = o.object_id AND ic.index_id = kc.unique_index_id AND ic.key_ordinal > 0
INNER JOIN sys.all_columns c
    ON c.object_id = ic.object_id AND c.column_id = ic.column_id
WHERE o.object_id = @object_id
ORDER BY KEY_SEQ;
"#####;

const SP_FKEYS: &str = r#####"
DECLARE @pk_owner nvarchar(128);
DECLARE @fk_owner nvarchar(128);

IF (@pktable_qualifier IS NOT NULL AND @pktable_qualifier <> '' AND DB_NAME() <> @pktable_qualifier)
    OR (@fktable_qualifier IS NOT NULL AND @fktable_qualifier <> '' AND DB_NAME() <> @fktable_qualifier)
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS PKTABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS PKTABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS PKTABLE_NAME,
        CAST(NULL AS nvarchar(128)) AS PKCOLUMN_NAME,
        CAST(NULL AS nvarchar(128)) AS FKTABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS FKTABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS FKTABLE_NAME,
        CAST(NULL AS nvarchar(128)) AS FKCOLUMN_NAME,
        CAST(NULL AS smallint) AS KEY_SEQ,
        CAST(NULL AS smallint) AS UPDATE_RULE,
        CAST(NULL AS smallint) AS DELETE_RULE,
        CAST(NULL AS nvarchar(128)) AS FK_NAME,
        CAST(NULL AS nvarchar(128)) AS PK_NAME,
        CAST(NULL AS smallint) AS DEFERRABILITY
    WHERE 1 = 0;
    RETURN;
END;

SET @pk_owner = COALESCE(@pktable_owner, N'%');
SET @fk_owner = COALESCE(@fktable_owner, N'%');

SELECT
    PKTABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
    PKTABLE_OWNER = CAST(SCHEMA_NAME(ro.schema_id) AS nvarchar(128)),
    PKTABLE_NAME = CAST(ro.name AS nvarchar(128)),
    PKCOLUMN_NAME = CAST(rc.name AS nvarchar(128)),
    FKTABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
    FKTABLE_OWNER = CAST(SCHEMA_NAME(fo.schema_id) AS nvarchar(128)),
    FKTABLE_NAME = CAST(fo.name AS nvarchar(128)),
    FKCOLUMN_NAME = CAST(fc.name AS nvarchar(128)),
    KEY_SEQ = CAST(fkc.constraint_column_id AS smallint),
    UPDATE_RULE = CAST(
        CASE fk.update_referential_action
            WHEN 0 THEN 1
            WHEN 1 THEN 0
            WHEN 2 THEN 2
            WHEN 3 THEN 3
            ELSE 1
        END AS smallint),
    DELETE_RULE = CAST(
        CASE fk.delete_referential_action
            WHEN 0 THEN 1
            WHEN 1 THEN 0
            WHEN 2 THEN 2
            WHEN 3 THEN 3
            ELSE 1
        END AS smallint),
    FK_NAME = CAST(fk.name AS nvarchar(128)),
    PK_NAME = CAST(pk.name AS nvarchar(128)),
    DEFERRABILITY = CAST(7 AS smallint)
FROM sys.foreign_keys fk
INNER JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
INNER JOIN sys.all_objects fo ON fo.object_id = fk.parent_object_id
INNER JOIN sys.all_objects ro ON ro.object_id = fk.referenced_object_id
INNER JOIN sys.all_columns fc
    ON fc.object_id = fkc.parent_object_id AND fc.column_id = fkc.parent_column_id
INNER JOIN sys.all_columns rc
    ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
INNER JOIN sys.key_constraints pk
    ON pk.parent_object_id = ro.object_id AND pk.type = N'PK'
WHERE (@pktable_name IS NULL OR (ro.name = @pktable_name AND SCHEMA_NAME(ro.schema_id) LIKE @pk_owner))
  AND (@fktable_name IS NULL OR (fo.name = @fktable_name AND SCHEMA_NAME(fo.schema_id) LIKE @fk_owner))
  AND (@pkcolumn_name IS NULL OR rc.name = @pkcolumn_name)
  AND (@fkcolumn_name IS NULL OR fc.name = @fkcolumn_name)
ORDER BY FKTABLE_QUALIFIER, FKTABLE_OWNER, FKTABLE_NAME, KEY_SEQ;
"#####;

const SP_STATISTICS: &str = r#####"
DECLARE @qual_name nvarchar(769);
DECLARE @object_id int;
DECLARE @owner nvarchar(128);

IF @table_qualifier IS NOT NULL AND @table_qualifier <> '' AND DB_NAME() <> @table_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS smallint) AS NON_UNIQUE,
        CAST(NULL AS nvarchar(128)) AS INDEX_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS INDEX_NAME,
        CAST(NULL AS smallint) AS TYPE,
        CAST(NULL AS smallint) AS SEQ_IN_INDEX,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS char(1)) AS COLLATION,
        CAST(NULL AS int) AS CARDINALITY,
        CAST(NULL AS int) AS PAGES,
        CAST(NULL AS varchar(128)) AS FILTER_CONDITION
    WHERE 1 = 0;
    RETURN;
END;

SET @owner = COALESCE(@table_owner, SCHEMA_NAME(SCHEMA_ID()));

SELECT @qual_name = QUOTENAME(@owner) + N'.' + QUOTENAME(@table_name);
SELECT @object_id = OBJECT_ID(@qual_name);

IF @object_id IS NULL
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS smallint) AS NON_UNIQUE,
        CAST(NULL AS nvarchar(128)) AS INDEX_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS INDEX_NAME,
        CAST(NULL AS smallint) AS TYPE,
        CAST(NULL AS smallint) AS SEQ_IN_INDEX,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS char(1)) AS COLLATION,
        CAST(NULL AS int) AS CARDINALITY,
        CAST(NULL AS int) AS PAGES,
        CAST(NULL AS varchar(128)) AS FILTER_CONDITION
    WHERE 1 = 0;
    RETURN;
END;

SELECT
    stats.TABLE_QUALIFIER,
    stats.TABLE_OWNER,
    stats.TABLE_NAME,
    stats.NON_UNIQUE,
    stats.INDEX_QUALIFIER,
    stats.INDEX_NAME,
    stats.TYPE,
    stats.SEQ_IN_INDEX,
    stats.COLUMN_NAME,
    stats.COLLATION,
    stats.CARDINALITY,
    stats.PAGES,
    stats.FILTER_CONDITION
FROM (
    SELECT
        TABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        TABLE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
        TABLE_NAME = CAST(o.name AS nvarchar(128)),
        NON_UNIQUE = CAST(NULL AS smallint),
        INDEX_QUALIFIER = CAST(NULL AS nvarchar(128)),
        INDEX_NAME = CAST(NULL AS nvarchar(128)),
        TYPE = CAST(0 AS smallint),
        SEQ_IN_INDEX = CAST(NULL AS smallint),
        COLUMN_NAME = CAST(NULL AS nvarchar(128)),
        COLLATION = CAST(NULL AS char(1)),
        CARDINALITY = CAST(0 AS int),
        PAGES = CAST(0 AS int),
        FILTER_CONDITION = CAST(NULL AS varchar(128)),
        sort_non_unique = CAST(-1 AS smallint),
        sort_type = CAST(-1 AS smallint),
        sort_name = CAST(N'' AS nvarchar(128))
    FROM sys.all_objects o
    WHERE o.object_id = @object_id
    UNION ALL
    SELECT
        TABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        TABLE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
        TABLE_NAME = CAST(o.name AS nvarchar(128)),
        NON_UNIQUE = CAST(CASE WHEN i.is_unique = 1 THEN 0 ELSE 1 END AS smallint),
        INDEX_QUALIFIER = CAST(o.name AS nvarchar(128)),
        INDEX_NAME = CAST(i.name AS nvarchar(128)),
        TYPE = CAST(CASE WHEN i.type = 1 THEN 1 ELSE 3 END AS smallint),
        SEQ_IN_INDEX = CAST(ic.key_ordinal AS smallint),
        COLUMN_NAME = CAST(c.name AS nvarchar(128)),
        COLLATION = CAST(N'A' AS char(1)),
        CARDINALITY = CAST(CASE WHEN i.type = 1 THEN 0 ELSE NULL END AS int),
        PAGES = CAST(CASE WHEN i.type = 1 THEN 0 ELSE NULL END AS int),
        FILTER_CONDITION = CAST(NULL AS varchar(128)),
        sort_non_unique = CAST(CASE WHEN i.is_unique = 1 THEN 0 ELSE 1 END AS smallint),
        sort_type = CAST(CASE WHEN i.type = 1 THEN 1 ELSE 3 END AS smallint),
        sort_name = CAST(i.name AS nvarchar(128))
    FROM sys.all_objects o
    INNER JOIN sys.indexes i ON i.object_id = o.object_id AND i.index_id > 0
    INNER JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0
    INNER JOIN sys.all_columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
    WHERE o.object_id = @object_id
      AND (@index_name IS NULL OR i.name = @index_name)
      AND (@is_unique IS NULL OR @is_unique <> N'Y' OR i.is_unique = 1)
) AS stats
ORDER BY stats.sort_non_unique, stats.sort_type, stats.sort_name, stats.SEQ_IN_INDEX;
"#####;

const SP_SPECIAL_COLUMNS: &str = r#####"
DECLARE @qual_name nvarchar(769);
DECLARE @object_id int;
DECLARE @owner nvarchar(128);

IF @table_qualifier IS NOT NULL AND @table_qualifier <> '' AND DB_NAME() <> @table_qualifier
BEGIN
    SELECT
        CAST(NULL AS smallint) AS SCOPE,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS [PRECISION],
        CAST(NULL AS int) AS LENGTH,
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS PSEUDO_COLUMN
    WHERE 1 = 0;
    RETURN;
END;

IF @col_type = N'V'
BEGIN
    SELECT
        CAST(NULL AS smallint) AS SCOPE,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS [PRECISION],
        CAST(NULL AS int) AS LENGTH,
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS PSEUDO_COLUMN
    WHERE 1 = 0;
    RETURN;
END;

SET @owner = COALESCE(@table_owner, SCHEMA_NAME(SCHEMA_ID()));

SELECT @qual_name = QUOTENAME(@owner) + N'.' + QUOTENAME(@table_name);
SELECT @object_id = OBJECT_ID(@qual_name);

IF @object_id IS NULL
BEGIN
    SELECT
        CAST(NULL AS smallint) AS SCOPE,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS [PRECISION],
        CAST(NULL AS int) AS LENGTH,
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS PSEUDO_COLUMN
    WHERE 1 = 0;
    RETURN;
END;

SELECT
    SCOPE = CAST(1 AS smallint),
    COLUMN_NAME = CAST(c.name AS nvarchar(128)),
    DATA_TYPE = CAST(
        CASE t.name
            WHEN N'int' THEN 4
            WHEN N'bigint' THEN -5
            WHEN N'smallint' THEN 5
            WHEN N'tinyint' THEN -6
            WHEN N'bit' THEN -7
            WHEN N'decimal' THEN 3
            WHEN N'numeric' THEN 2
            WHEN N'nvarchar' THEN -9
            WHEN N'varchar' THEN 12
            WHEN N'varbinary' THEN -3
            WHEN N'datetime2' THEN 93
            WHEN N'uniqueidentifier' THEN -11
            ELSE 0
        END AS smallint),
    TYPE_NAME = CAST(
        CASE WHEN c.is_identity = 1 THEN t.name + N' identity' ELSE t.name END AS nvarchar(128)),
    [PRECISION] = CAST(
        CASE
            WHEN t.name IN (N'nvarchar', N'varchar', N'varbinary') AND c.max_length = -1 THEN 2147483647
            WHEN t.name IN (N'nvarchar', N'nchar') THEN c.max_length / 2
            WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN c.max_length
            WHEN t.name IN (N'decimal', N'numeric') THEN c.[precision]
            WHEN t.name = N'int' THEN 10
            WHEN t.name = N'bigint' THEN 19
            WHEN t.name = N'smallint' THEN 5
            WHEN t.name = N'tinyint' THEN 3
            WHEN t.name = N'bit' THEN 1
            WHEN t.name = N'uniqueidentifier' THEN 36
            ELSE NULL
        END AS int),
    LENGTH = CAST(
        CASE
            WHEN t.name IN (N'nvarchar', N'varchar', N'varbinary') AND c.max_length = -1 THEN 2147483647
            WHEN t.name IN (N'nvarchar', N'nchar') THEN c.max_length
            WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN c.max_length
            WHEN t.name = N'int' THEN 4
            WHEN t.name = N'bigint' THEN 8
            WHEN t.name = N'smallint' THEN 2
            WHEN t.name = N'tinyint' THEN 1
            WHEN t.name = N'bit' THEN 1
            WHEN t.name = N'uniqueidentifier' THEN 16
            WHEN t.name IN (N'decimal', N'numeric') THEN c.[precision] + 2
            ELSE NULL
        END AS int),
    SCALE = CAST(c.scale AS smallint),
    PSEUDO_COLUMN = CAST(1 AS smallint)
FROM sys.all_objects o
INNER JOIN sys.key_constraints kc
    ON kc.parent_object_id = o.object_id AND kc.type = N'PK'
INNER JOIN sys.index_columns ic
    ON ic.object_id = o.object_id AND ic.index_id = kc.unique_index_id AND ic.key_ordinal > 0
INNER JOIN sys.all_columns c
    ON c.object_id = ic.object_id AND c.column_id = ic.column_id
INNER JOIN sys.types t ON t.user_type_id = c.user_type_id
WHERE o.object_id = @object_id
  AND (@nullable IS NULL OR @nullable <> N'O' OR c.is_nullable = 0)
ORDER BY ic.key_ordinal;
"#####;

const SP_SPROC_COLUMNS: &str = r#####"
DECLARE @owner nvarchar(128);
DECLARE @proc_name nvarchar(128);

IF @procedure_qualifier IS NOT NULL AND @procedure_qualifier <> '' AND DB_NAME() <> @procedure_qualifier
BEGIN
    
    SELECT
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_OWNER,
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_NAME,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS COLUMN_TYPE,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS [PRECISION],
        CAST(NULL AS int) AS LENGTH,
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS RADIX,
        CAST(NULL AS smallint) AS NULLABLE,
        CAST(NULL AS varchar(254)) AS REMARKS,
        CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
        CAST(NULL AS smallint) AS SQL_DATA_TYPE,
        CAST(NULL AS smallint) AS SQL_DATETIME_SUB,
        CAST(NULL AS smallint) AS FDATATYPE,
        CAST(NULL AS varchar(254)) AS CHARACTER_SET_CAT,
        CAST(NULL AS int) AS ORDINAL_POSITION,
        CAST(NULL AS varchar(254)) AS IS_NULLABLE,
        CAST(NULL AS tinyint) AS SS_DATA_TYPE
    WHERE 1 = 0;

    RETURN;
END;

SET @owner = COALESCE(@procedure_owner, N'%');
SET @proc_name = COALESCE(@procedure_name, N'%');

SELECT *
FROM (
    SELECT
        PROCEDURE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        PROCEDURE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
        PROCEDURE_NAME = CAST(o.name + N';1' AS nvarchar(134)),
        COLUMN_NAME = CAST(N'@RETURN_VALUE' AS nvarchar(128)),
        COLUMN_TYPE = CAST(5 AS smallint),
        DATA_TYPE = CAST(4 AS smallint),
        TYPE_NAME = CAST(N'int' AS nvarchar(128)),
        [PRECISION] = CAST(10 AS int),
        LENGTH = CAST(4 AS int),
        SCALE = CAST(0 AS smallint),
        RADIX = CAST(10 AS smallint),
        NULLABLE = CAST(0 AS smallint),
        REMARKS = CAST(NULL AS varchar(254)),
        COLUMN_DEF = CAST(NULL AS nvarchar(4000)),
        SQL_DATA_TYPE = CAST(4 AS smallint),
        SQL_DATETIME_SUB = CAST(NULL AS smallint),
        FDATATYPE = CAST(0 AS smallint),
        CHARACTER_SET_CAT = CAST(NULL AS varchar(254)),
        ORDINAL_POSITION = CAST(0 AS int),
        IS_NULLABLE = CAST(N'NO' AS varchar(254)),
        SS_DATA_TYPE = CAST(56 AS tinyint)
        
    FROM sys.all_objects o
    WHERE o.type = N'P '
      AND o.name LIKE @proc_name
      AND SCHEMA_NAME(o.schema_id) LIKE @owner
      AND (@column_name IS NULL OR N'@RETURN_VALUE' LIKE @column_name)
    UNION ALL
    SELECT
        PROCEDURE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        PROCEDURE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
        PROCEDURE_NAME = CAST(o.name + N';1' AS nvarchar(134)),
        COLUMN_NAME = CAST(p.name AS nvarchar(128)),
        COLUMN_TYPE = CAST(CASE WHEN p.is_output = 1 THEN 2 ELSE 1 END AS smallint),
        DATA_TYPE = CASE t.name
    WHEN N'int' THEN CAST(4 AS smallint)
    WHEN N'bigint' THEN CAST(-5 AS smallint)
    WHEN N'decimal' THEN CAST(3 AS smallint)
    WHEN N'numeric' THEN CAST(2 AS smallint)
    WHEN N'nvarchar' THEN CAST(-9 AS smallint)
    WHEN N'varchar' THEN CAST(12 AS smallint)
    WHEN N'varbinary' THEN CAST(-3 AS smallint)
    WHEN N'datetime2' THEN CAST(93 AS smallint)
    WHEN N'bit' THEN CAST(-7 AS smallint)
    WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
    ELSE CAST(0 AS smallint)
END,
        TYPE_NAME = CAST(t.name AS nvarchar(128)),
        [PRECISION] = CAST(
            CASE
                WHEN t.name IN (N'nvarchar', N'varchar', N'varbinary') AND p.max_length = -1 THEN 2147483647
                WHEN t.name IN (N'nvarchar', N'nchar') THEN p.max_length / 2
                WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN p.max_length
                WHEN t.name IN (N'decimal', N'numeric') THEN p.[precision]
                WHEN t.name = N'int' THEN 10
                WHEN t.name = N'bigint' THEN 19
                WHEN t.name = N'bit' THEN 1
                WHEN t.name = N'uniqueidentifier' THEN 36
                ELSE NULL
            END AS int),
        LENGTH = CAST(
            CASE
                WHEN t.name IN (N'nvarchar', N'varchar', N'varbinary') AND p.max_length = -1 THEN 2147483647
                WHEN t.name IN (N'nvarchar', N'nchar') THEN p.max_length
                WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN p.max_length
                WHEN t.name = N'int' THEN 4
                WHEN t.name = N'bigint' THEN 8
                WHEN t.name = N'bit' THEN 1
                WHEN t.name = N'uniqueidentifier' THEN 16
                WHEN t.name IN (N'decimal', N'numeric') THEN p.[precision] + 2
                ELSE NULL
            END AS int),
        SCALE = CAST(p.scale AS smallint),
        RADIX = CAST(10 AS smallint),
        NULLABLE = CAST(CASE WHEN p.is_nullable = 1 THEN 1 ELSE 0 END AS smallint),
        REMARKS = CAST(NULL AS varchar(254)),
        COLUMN_DEF = CAST(NULL AS nvarchar(4000)),
        SQL_DATA_TYPE = CASE t.name
    WHEN N'int' THEN CAST(4 AS smallint)
    WHEN N'bigint' THEN CAST(-5 AS smallint)
    WHEN N'decimal' THEN CAST(3 AS smallint)
    WHEN N'numeric' THEN CAST(2 AS smallint)
    WHEN N'nvarchar' THEN CAST(-9 AS smallint)
    WHEN N'varchar' THEN CAST(12 AS smallint)
    WHEN N'varbinary' THEN CAST(-3 AS smallint)
    WHEN N'datetime2' THEN CAST(93 AS smallint)
    WHEN N'bit' THEN CAST(-7 AS smallint)
    WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
    ELSE CAST(0 AS smallint)
END,
        SQL_DATETIME_SUB = CAST(NULL AS smallint),
        FDATATYPE = CAST(0 AS smallint),
        CHARACTER_SET_CAT = CAST(NULL AS varchar(254)),
        ORDINAL_POSITION = CAST(p.parameter_id AS int),
        IS_NULLABLE = CAST(CASE WHEN p.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)),
        SS_DATA_TYPE = CAST(
            CASE t.name
                WHEN N'int' THEN 56
                WHEN N'nvarchar' THEN 39
                WHEN N'decimal' THEN 106
                WHEN N'bit' THEN 50
                ELSE 0
            END AS tinyint)
        
    FROM sys.all_objects o
    INNER JOIN sys.parameters p ON p.object_id = o.object_id
    INNER JOIN sys.types t ON t.user_type_id = p.user_type_id
    WHERE o.type = N'P '
      AND o.name LIKE @proc_name
      AND SCHEMA_NAME(o.schema_id) LIKE @owner
      AND (@column_name IS NULL OR p.name LIKE @column_name)
) AS cols
WHERE @ODBCVer <> 4 OR COLUMN_NAME = N'@RETURN_VALUE'
ORDER BY ORDINAL_POSITION;
"#####;

const SP_SPROC_COLUMNS_100: &str = r#####"
DECLARE @owner nvarchar(128);
DECLARE @proc_name nvarchar(128);

IF @procedure_qualifier IS NOT NULL AND @procedure_qualifier <> '' AND DB_NAME() <> @procedure_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_OWNER,
        CAST(NULL AS nvarchar(128)) AS PROCEDURE_NAME,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS COLUMN_TYPE,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS [PRECISION],
        CAST(NULL AS int) AS LENGTH,
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS RADIX,
        CAST(NULL AS smallint) AS NULLABLE,
        CAST(NULL AS varchar(254)) AS REMARKS,
        CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
        CAST(NULL AS smallint) AS SQL_DATA_TYPE,
        CAST(NULL AS smallint) AS SQL_DATETIME_SUB,
        CAST(NULL AS smallint) AS FDATATYPE,
        CAST(NULL AS varchar(254)) AS CHARACTER_SET_CAT,
        CAST(NULL AS int) AS ORDINAL_POSITION,
        CAST(NULL AS varchar(254)) AS IS_NULLABLE,
        CAST(NULL AS tinyint) AS SS_DATA_TYPE
    WHERE 1 = 0;

    RETURN;
END;

SET @owner = COALESCE(@procedure_owner, N'%');
SET @proc_name = COALESCE(@procedure_name, N'%');

SELECT *
FROM (
    SELECT
        PROCEDURE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        PROCEDURE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
        PROCEDURE_NAME = CAST(o.name + N';1' AS nvarchar(134)),
        COLUMN_NAME = CAST(N'@RETURN_VALUE' AS nvarchar(128)),
        COLUMN_TYPE = CAST(5 AS smallint),
        DATA_TYPE = CAST(4 AS smallint),
        TYPE_NAME = CAST(N'int' AS nvarchar(128)),
        [PRECISION] = CAST(10 AS int),
        LENGTH = CAST(4 AS int),
        SCALE = CAST(0 AS smallint),
        RADIX = CAST(10 AS smallint),
        NULLABLE = CAST(0 AS smallint),
        REMARKS = CAST(NULL AS varchar(254)),
        COLUMN_DEF = CAST(NULL AS nvarchar(4000)),
        SQL_DATA_TYPE = CAST(4 AS smallint),
        SQL_DATETIME_SUB = CAST(NULL AS smallint),
        FDATATYPE = CAST(0 AS smallint),
        CHARACTER_SET_CAT = CAST(NULL AS varchar(254)),
        ORDINAL_POSITION = CAST(0 AS int),
        IS_NULLABLE = CAST(N'NO' AS varchar(254)),
        SS_DATA_TYPE = CAST(56 AS tinyint)
        ,
        SS_XML_SCHEMACOLLECTION_CATALOG_NAME = CAST(NULL AS nvarchar(128)),
        SS_XML_SCHEMACOLLECTION_SCHEMA_NAME = CAST(NULL AS nvarchar(128)),
        SS_XML_SCHEMACOLLECTION_NAME = CAST(NULL AS nvarchar(128)),
        SS_UDT_CATALOG_NAME = CAST(NULL AS nvarchar(128)),
        SS_UDT_SCHEMA_NAME = CAST(NULL AS nvarchar(128)),
        SS_UDT_ASSEMBLY_TYPE_NAME = CAST(NULL AS nvarchar(128))
    FROM sys.all_objects o
    WHERE o.type = N'P '
      AND o.name LIKE @proc_name
      AND SCHEMA_NAME(o.schema_id) LIKE @owner
      AND (@column_name IS NULL OR N'@RETURN_VALUE' LIKE @column_name)
    UNION ALL
    SELECT
        PROCEDURE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        PROCEDURE_OWNER = CAST(SCHEMA_NAME(o.schema_id) AS nvarchar(128)),
        PROCEDURE_NAME = CAST(o.name + N';1' AS nvarchar(134)),
        COLUMN_NAME = CAST(p.name AS nvarchar(128)),
        COLUMN_TYPE = CAST(CASE WHEN p.is_output = 1 THEN 2 ELSE 1 END AS smallint),
        DATA_TYPE = CASE t.name
    WHEN N'int' THEN CAST(4 AS smallint)
    WHEN N'bigint' THEN CAST(-5 AS smallint)
    WHEN N'decimal' THEN CAST(3 AS smallint)
    WHEN N'numeric' THEN CAST(2 AS smallint)
    WHEN N'nvarchar' THEN CAST(-9 AS smallint)
    WHEN N'varchar' THEN CAST(12 AS smallint)
    WHEN N'varbinary' THEN CAST(-3 AS smallint)
    WHEN N'datetime2' THEN CAST(93 AS smallint)
    WHEN N'bit' THEN CAST(-7 AS smallint)
    WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
    ELSE CAST(0 AS smallint)
END,
        TYPE_NAME = CAST(t.name AS nvarchar(128)),
        [PRECISION] = CAST(
            CASE
                WHEN t.name IN (N'nvarchar', N'varchar', N'varbinary') AND p.max_length = -1 THEN 2147483647
                WHEN t.name IN (N'nvarchar', N'nchar') THEN p.max_length / 2
                WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN p.max_length
                WHEN t.name IN (N'decimal', N'numeric') THEN p.[precision]
                WHEN t.name = N'int' THEN 10
                WHEN t.name = N'bigint' THEN 19
                WHEN t.name = N'bit' THEN 1
                WHEN t.name = N'uniqueidentifier' THEN 36
                ELSE NULL
            END AS int),
        LENGTH = CAST(
            CASE
                WHEN t.name IN (N'nvarchar', N'varchar', N'varbinary') AND p.max_length = -1 THEN 2147483647
                WHEN t.name IN (N'nvarchar', N'nchar') THEN p.max_length
                WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN p.max_length
                WHEN t.name = N'int' THEN 4
                WHEN t.name = N'bigint' THEN 8
                WHEN t.name = N'bit' THEN 1
                WHEN t.name = N'uniqueidentifier' THEN 16
                WHEN t.name IN (N'decimal', N'numeric') THEN p.[precision] + 2
                ELSE NULL
            END AS int),
        SCALE = CAST(p.scale AS smallint),
        RADIX = CAST(10 AS smallint),
        NULLABLE = CAST(CASE WHEN p.is_nullable = 1 THEN 1 ELSE 0 END AS smallint),
        REMARKS = CAST(NULL AS varchar(254)),
        COLUMN_DEF = CAST(NULL AS nvarchar(4000)),
        SQL_DATA_TYPE = CASE t.name
    WHEN N'int' THEN CAST(4 AS smallint)
    WHEN N'bigint' THEN CAST(-5 AS smallint)
    WHEN N'decimal' THEN CAST(3 AS smallint)
    WHEN N'numeric' THEN CAST(2 AS smallint)
    WHEN N'nvarchar' THEN CAST(-9 AS smallint)
    WHEN N'varchar' THEN CAST(12 AS smallint)
    WHEN N'varbinary' THEN CAST(-3 AS smallint)
    WHEN N'datetime2' THEN CAST(93 AS smallint)
    WHEN N'bit' THEN CAST(-7 AS smallint)
    WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
    ELSE CAST(0 AS smallint)
END,
        SQL_DATETIME_SUB = CAST(NULL AS smallint),
        FDATATYPE = CAST(0 AS smallint),
        CHARACTER_SET_CAT = CAST(NULL AS varchar(254)),
        ORDINAL_POSITION = CAST(p.parameter_id AS int),
        IS_NULLABLE = CAST(CASE WHEN p.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)),
        SS_DATA_TYPE = CAST(
            CASE t.name
                WHEN N'int' THEN 56
                WHEN N'nvarchar' THEN 39
                WHEN N'decimal' THEN 106
                WHEN N'bit' THEN 50
                ELSE 0
            END AS tinyint)
        ,
        SS_XML_SCHEMACOLLECTION_CATALOG_NAME = CAST(NULL AS nvarchar(128)),
        SS_XML_SCHEMACOLLECTION_SCHEMA_NAME = CAST(NULL AS nvarchar(128)),
        SS_XML_SCHEMACOLLECTION_NAME = CAST(NULL AS nvarchar(128)),
        SS_UDT_CATALOG_NAME = CAST(NULL AS nvarchar(128)),
        SS_UDT_SCHEMA_NAME = CAST(NULL AS nvarchar(128)),
        SS_UDT_ASSEMBLY_TYPE_NAME = CAST(NULL AS nvarchar(128))
    FROM sys.all_objects o
    INNER JOIN sys.parameters p ON p.object_id = o.object_id
    INNER JOIN sys.types t ON t.user_type_id = p.user_type_id
    WHERE o.type = N'P '
      AND o.name LIKE @proc_name
      AND SCHEMA_NAME(o.schema_id) LIKE @owner
      AND (@column_name IS NULL OR p.name LIKE @column_name)
) AS cols
WHERE @ODBCVer <> 4 OR COLUMN_NAME = N'@RETURN_VALUE'
ORDER BY ORDINAL_POSITION;
"#####;

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{Len, SqlType};

    fn nvarchar(value: &str) -> Value {
        Value::String(SqlString {
            text: value.to_owned(),
        })
    }

    fn nvarchar_ty() -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Max), false)
    }

    fn arg<'a>(name: Option<&'a str>, ty: &'a TypeInfo, value: &'a Value) -> ProcArg<'a> {
        ProcArg {
            name,
            ty,
            value,
            output: false,
            default: false,
        }
    }

    #[test]
    fn sp_pkeys_requires_table_name() {
        let err = resolve("sp_pkeys", &[]).unwrap().unwrap_err();
        assert_eq!(err.number, 201);
    }

    #[test]
    fn sp_pkeys_resolves_to_template() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let action = resolve("sp_pkeys", &[arg(Some("@table_name"), &nv, &name)])
            .unwrap()
            .unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
    }

    #[test]
    fn sp_fkeys_without_table_names_returns_15252() {
        let err = resolve("sp_fkeys", &[]).unwrap().unwrap_err();
        assert_eq!(err.number, 15252);
    }

    #[test]
    fn sp_statistics_100_is_an_alias() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let action = resolve("sp_statistics_100", &[arg(Some("@table_name"), &nv, &name)])
            .unwrap()
            .unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
    }

    #[test]
    fn sp_pkeys_100_is_an_alias() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let action = resolve("sp_pkeys_100", &[arg(Some("@table_name"), &nv, &name)])
            .unwrap()
            .unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
    }

    #[test]
    fn sp_statistics_binds_accuracy_default() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let action = resolve("sp_statistics", &[arg(Some("@table_name"), &nv, &name)])
            .unwrap()
            .unwrap();
        match action {
            ProcAction::Template { params, .. } => {
                let accuracy = params
                    .iter()
                    .find(|p| p.name == "@accuracy")
                    .expect("@accuracy");
                assert_eq!(
                    accuracy.value,
                    Value::String(SqlString {
                        text: "Q".to_owned()
                    })
                );
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn sp_sproc_columns_100_resolves() {
        let action = resolve("sp_sproc_columns_100", &[]).unwrap().unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
    }

    #[test]
    fn sp_special_columns_rejects_coltype_alias() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let err = resolve(
            "sp_special_columns",
            &[
                arg(Some("@table_name"), &nv, &name),
                arg(
                    Some("@colType"),
                    &TypeInfo::new(SqlType::VarChar(Len::Fixed(1)), false),
                    &Value::String(SqlString { text: "V".into() }),
                ),
            ],
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(err.number, 8145);
    }
}
