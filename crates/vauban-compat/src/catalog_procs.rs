//! Catalog procedures (`sp_tables`, `sp_columns`, …).

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Len, SqlType, TypeInfo, Value};

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
    default_null: bool,
    default_int: Option<i32>,
    default_bit: Option<bool>,
}

const SP_TABLES_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@table_name",
        aliases: &["@table_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@table_owner",
        aliases: &["@table_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@table_qualifier",
        aliases: &["@table_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@table_type",
        aliases: &["@table_type"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@fUsePattern",
        aliases: &["@fUsePattern"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: true,
        default_null: false,
        default_int: None,
        default_bit: Some(true),
    },
];

const SP_COLUMNS_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@table_name",
        aliases: &["@table_name"],
        required: true,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: false,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@table_owner",
        aliases: &["@table_owner"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@table_qualifier",
        aliases: &["@table_qualifier"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@column_name",
        aliases: &["@column_name"],
        required: false,
        unicode: true,
        int_param: false,
        bit_param: false,
        default_null: true,
        default_int: None,
        default_bit: None,
    },
    ParamSpec {
        canonical: "@ODBCVer",
        aliases: &["@ODBCVer"],
        required: false,
        unicode: false,
        int_param: true,
        bit_param: false,
        default_null: false,
        default_int: Some(2),
        default_bit: None,
    },
];

const SP_COLUMNS_100_EXTRA: &[ParamSpec] = &[
    ParamSpec {
        canonical: "@NameScope",
        aliases: &["@NameScope"],
        required: false,
        unicode: false,
        int_param: true,
        bit_param: false,
        default_null: false,
        default_int: Some(0),
        default_bit: None,
    },
    ParamSpec {
        canonical: "@fUsePattern",
        aliases: &["@fUsePattern"],
        required: false,
        unicode: false,
        int_param: false,
        bit_param: true,
        default_null: false,
        default_int: None,
        default_bit: Some(true),
    },
];

const SP_SERVER_INFO_PARAMS: &[ParamSpec] = &[ParamSpec {
    canonical: "@attribute_id",
    aliases: &["@attribute_id"],
    required: false,
    unicode: false,
    int_param: true,
    bit_param: false,
    default_null: true,
    default_int: None,
    default_bit: None,
}];

#[allow(dead_code)]
pub(crate) const PROCS: &[SystemProc] = &[
    SystemProc { name: "sp_tables" },
    SystemProc { name: "sp_columns" },
    SystemProc {
        name: "sp_columns_100",
    },
    SystemProc {
        name: "sp_databases",
    },
    SystemProc {
        name: "sp_server_info",
    },
];

/// Resolves a catalog procedure implemented in this module.
pub(crate) fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    match name {
        "sp_tables" => Some(resolve_template(
            "sp_tables",
            SP_TABLES_PARAMS,
            SP_TABLES,
            args,
        )),
        "sp_columns" => Some(resolve_sp_columns(args, false)),
        "sp_columns_100" => Some(resolve_sp_columns(args, true)),
        "sp_databases" => Some(resolve_sp_databases(args)),
        "sp_server_info" => Some(resolve_template(
            "sp_server_info",
            SP_SERVER_INFO_PARAMS,
            SP_SERVER_INFO,
            args,
        )),
        _ => None,
    }
}

fn resolve_sp_databases(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    if !args.is_empty() {
        return Err(SqlError::new(
            8146,
            16,
            1,
            "Procedure sp_databases has no parameters and arguments were supplied.",
        )
        .with_line(0));
    }
    Ok(ProcAction::Template {
        sql: SP_DATABASES.to_owned(),
        params: Vec::new(),
    })
}

fn resolve_sp_columns(args: &[ProcArg<'_>], columns_100: bool) -> SqlResult<ProcAction> {
    let proc = if columns_100 {
        "sp_columns_100"
    } else {
        "sp_columns"
    };
    let mut specs: Vec<ParamSpec> = SP_COLUMNS_PARAMS.to_vec();
    if columns_100 {
        specs.extend_from_slice(SP_COLUMNS_100_EXTRA);
    }
    for arg in args {
        if let Some(name) = arg.name
            && name.eq_ignore_ascii_case("@fUsePattern")
            && !columns_100
        {
            return Err(SqlError::not_a_parameter("@fUsePattern", "sp_columns"));
        }
    }
    let sql = if columns_100 {
        SP_COLUMNS_100
    } else {
        SP_COLUMNS
    };
    resolve_template(proc, &specs, sql, args)
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
    } else if spec.canonical == "@table_type" {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(100)), true)
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

const SP_TABLES: &str = r#####"
DECLARE @type1 varchar(3);
DECLARE @qual_name nvarchar(517);
DECLARE @table_id int;
DECLARE @token_system_table varchar(20);
DECLARE @token_view varchar(20);
DECLARE @token_table varchar(20);

SELECT @token_system_table = 'SYSTEM TABLE';
SELECT @token_view = 'VIEW';
SELECT @token_table = 'TABLE';

IF @table_type IS NULL
    SELECT @type1 = 'SUV';
IF @table_type IS NOT NULL
BEGIN
    SELECT @type1 = '';
    IF @table_type LIKE '%' + @token_system_table + '%'
        SELECT @type1 = @type1 + 'S';
    IF @table_type LIKE '%' + @token_view + '%'
        SELECT @type1 = @type1 + 'V';
    IF @table_type LIKE '%' + @token_table + '%' AND @table_type NOT LIKE '%' + @token_system_table + '%'
        SELECT @type1 = @type1 + 'U';
END;

IF @table_qualifier IS NOT NULL AND @table_qualifier <> '' AND DB_NAME() <> @table_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS varchar(32)) AS TABLE_TYPE,
        CAST(NULL AS varchar(254)) AS REMARKS
    WHERE 1 = 0;
    RETURN;
END;

IF @table_name IS NOT NULL AND @table_owner IS NULL
    AND CHARINDEX('%', @table_name) = 0 AND CHARINDEX('_', @table_name) = 0
BEGIN
    IF EXISTS (
        SELECT 1 FROM sys.all_objects o
        WHERE o.schema_id = SCHEMA_ID()
          AND o.name = @table_name
          AND o.type IN ('U', 'V', 'S')
    )
        SELECT @table_owner = N'dbo';
END;

SELECT @qual_name = ISNULL(QUOTENAME(@table_owner), '') + '.' + QUOTENAME(@table_name);
SELECT @table_id = OBJECT_ID(@qual_name);

IF @fUsePattern = 1
    AND CHARINDEX('%', ISNULL(@table_name, '')) = 0
    AND CHARINDEX('_', ISNULL(@table_name, '')) = 0
    AND CHARINDEX('%', ISNULL(@table_owner, '')) = 0
    AND CHARINDEX('_', ISNULL(@table_owner, '')) = 0
    AND @table_id IS NOT NULL
    SELECT @fUsePattern = 0;

IF @fUsePattern = 0
BEGIN
    SELECT
        TABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        TABLE_OWNER = CAST(CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END AS nvarchar(128)),
        TABLE_NAME = CAST(o.name AS nvarchar(128)),
        TABLE_TYPE = CAST(
            CASE o.type
                WHEN 'S' THEN 'SYSTEM TABLE'
                WHEN 'U' THEN 'TABLE'
                WHEN 'V' THEN 'VIEW'
            END AS varchar(32)),
        REMARKS = CAST(NULL AS varchar(254))
    FROM sys.all_objects o
    WHERE o.type IN ('U', 'V', 'S')
      AND CHARINDEX(SUBSTRING(o.type, 1, 1), @type1) <> 0
      AND (@table_id IS NULL OR o.object_id = @table_id)
      AND (@table_name IS NULL OR o.name = @table_name)
      AND (@table_owner IS NULL OR CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END = @table_owner)
END;
IF @fUsePattern = 1
BEGIN
    SELECT
        TABLE_QUALIFIER = CAST(DB_NAME() AS nvarchar(128)),
        TABLE_OWNER = CAST(CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END AS nvarchar(128)),
        TABLE_NAME = CAST(o.name AS nvarchar(128)),
        TABLE_TYPE = CAST(
            CASE o.type
                WHEN 'S' THEN 'SYSTEM TABLE'
                WHEN 'U' THEN 'TABLE'
                WHEN 'V' THEN 'VIEW'
            END AS varchar(32)),
        REMARKS = CAST(NULL AS varchar(254))
    FROM sys.all_objects o
    WHERE o.type IN ('U', 'V', 'S')
      AND CHARINDEX(SUBSTRING(o.type, 1, 1), @type1) <> 0
      AND (@table_name IS NULL OR o.name LIKE @table_name)
      AND (@table_owner IS NULL OR CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END LIKE @table_owner)
END
"#####;

const SP_COLUMNS: &str = r#####"
DECLARE @full_table_name nvarchar(769);
DECLARE @table_id int;
DECLARE @use_pattern bit;

SELECT @use_pattern = 1;

IF @ODBCVer IS NULL OR @ODBCVer <> 3
    SELECT @ODBCVer = 2;

IF @table_qualifier IS NOT NULL AND DB_NAME() <> @table_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS "PRECISION",
        CAST(NULL AS int) AS "LENGTH",
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS RADIX,
        CAST(NULL AS smallint) AS NULLABLE,
        CAST(NULL AS varchar(254)) AS REMARKS,
        CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
        CAST(NULL AS smallint) AS SQL_DATA_TYPE,
        CAST(NULL AS smallint) AS SQL_DATETIME_SUB,
        CAST(NULL AS int) AS CHAR_OCTET_LENGTH,
        CAST(NULL AS int) AS ORDINAL_POSITION,
        CAST(NULL AS varchar(254)) AS IS_NULLABLE,
        CAST(NULL AS tinyint) AS SS_DATA_TYPE
    WHERE 1 = 0;
    RETURN;
END;

IF @table_name = '%'
    SELECT @table_name = NULL;
IF @table_owner = '%'
    SELECT @table_owner = NULL;
IF @table_qualifier = '%'
    SELECT @table_qualifier = NULL;
IF @column_name = '%'
    SELECT @column_name = NULL;

IF @table_owner = ''
    SELECT @table_owner = ' ';

IF @table_name IS NOT NULL AND @table_owner IS NULL
    SELECT @table_owner = N'dbo';

SELECT @full_table_name = ISNULL(QUOTENAME(ISNULL(@table_owner, N'dbo')), '') + '.' + ISNULL(QUOTENAME(@table_name), '');
SELECT @table_id = OBJECT_ID(@full_table_name);

IF @use_pattern = 1
    AND CHARINDEX('%', ISNULL(@full_table_name, '')) = 0
    AND CHARINDEX('_', ISNULL(@full_table_name, '')) = 0
    AND CHARINDEX('[', ISNULL(@table_name, '')) = 0
    AND CHARINDEX('[', ISNULL(@table_owner, '')) = 0
    AND CHARINDEX('%', ISNULL(@column_name, '')) = 0
    AND CHARINDEX('_', ISNULL(@column_name, '')) = 0
    AND @table_id IS NOT NULL AND @table_id <> 0
    SELECT @use_pattern = 0;

IF @use_pattern = 0
BEGIN
    SELECT
TABLE_QUALIFIER = s_cov.TABLE_QUALIFIER,
    TABLE_OWNER = s_cov.TABLE_OWNER,
    TABLE_NAME = s_cov.TABLE_NAME,
    COLUMN_NAME = s_cov.COLUMN_NAME,
    DATA_TYPE = s_cov.DATA_TYPE_28,
    TYPE_NAME = s_cov.TYPE_NAME_28,
    [PRECISION] = s_cov.PRECISION_28,
    [LENGTH] = s_cov.LENGTH_28,
    SCALE = s_cov.SCALE_90,
    RADIX = s_cov.RADIX,
    NULLABLE = s_cov.NULLABLE,
    REMARKS = s_cov.REMARKS,
    COLUMN_DEF = s_cov.COLUMN_DEF,
    SQL_DATA_TYPE = s_cov.[SQL_DATA_TYPE_28],
    SQL_DATETIME_SUB = s_cov.[SQL_DATETIME_SUB_90],
    CHAR_OCTET_LENGTH = s_cov.CHAR_OCTET_LENGTH_28,
    ORDINAL_POSITION = s_cov.ORDINAL_POSITION,
    IS_NULLABLE = s_cov.IS_NULLABLE,
    SS_DATA_TYPE = s_cov.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END AS nvarchar(128)) AS TABLE_OWNER,
    CAST(o.name AS nvarchar(128)) AS TABLE_NAME,
    CAST(c.name AS nvarchar(128)) AS COLUMN_NAME,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(93 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(11 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [PRECISION],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS PRECISION_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(16 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [LENGTH],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(46 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS LENGTH_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(c.scale AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS RADIX,
    CAST(CASE WHEN c.is_nullable = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS NULLABLE,
    CAST(NULL AS varchar(254)) AS REMARKS,
    CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH_28,
    CAST(c.column_id AS int) AS ORDINAL_POSITION,
    CAST(CASE WHEN c.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)) AS IS_NULLABLE,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' AND c.is_nullable = 0 THEN CAST(56 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(38 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(108 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(106 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'text') THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'varbinary' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(50 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(0 AS tinyint)
        ELSE CAST(0 AS tinyint)
    END AS tinyint) AS SS_DATA_TYPE,
    CAST(0 AS smallint) AS SS_IS_SPARSE,
    CAST(0 AS smallint) AS SS_IS_COLUMN_SET,
    CAST(CASE WHEN c.is_computed = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_COMPUTED,
    CAST(CASE WHEN c.is_identity = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_IDENTITY,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_ASSEMBLY_TYPE_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_NAME,
    c.object_id AS object_id,
    o.schema_id AS SCHEMA_ID,
    o.type AS OBJECT_TYPE,
    @ODBCVer AS ODBCVER
FROM sys.all_objects o
INNER JOIN sys.all_columns c ON c.object_id = o.object_id
WHERE o.type IN (N'U', N'V') AND c.is_computed = 0
    ) s_cov
    WHERE s_cov.object_id = @table_id
      AND (@column_name IS NULL OR s_cov.COLUMN_NAME = @column_name)
      AND s_cov.ODBCVER = @ODBCVer
      AND s_cov.OBJECT_TYPE <> 'TT';
END
ELSE
BEGIN
    SELECT
TABLE_QUALIFIER = s_cov.TABLE_QUALIFIER,
    TABLE_OWNER = s_cov.TABLE_OWNER,
    TABLE_NAME = s_cov.TABLE_NAME,
    COLUMN_NAME = s_cov.COLUMN_NAME,
    DATA_TYPE = s_cov.DATA_TYPE_28,
    TYPE_NAME = s_cov.TYPE_NAME_28,
    [PRECISION] = s_cov.PRECISION_28,
    [LENGTH] = s_cov.LENGTH_28,
    SCALE = s_cov.SCALE_90,
    RADIX = s_cov.RADIX,
    NULLABLE = s_cov.NULLABLE,
    REMARKS = s_cov.REMARKS,
    COLUMN_DEF = s_cov.COLUMN_DEF,
    SQL_DATA_TYPE = s_cov.[SQL_DATA_TYPE_28],
    SQL_DATETIME_SUB = s_cov.[SQL_DATETIME_SUB_90],
    CHAR_OCTET_LENGTH = s_cov.CHAR_OCTET_LENGTH_28,
    ORDINAL_POSITION = s_cov.ORDINAL_POSITION,
    IS_NULLABLE = s_cov.IS_NULLABLE,
    SS_DATA_TYPE = s_cov.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END AS nvarchar(128)) AS TABLE_OWNER,
    CAST(o.name AS nvarchar(128)) AS TABLE_NAME,
    CAST(c.name AS nvarchar(128)) AS COLUMN_NAME,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(93 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(11 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [PRECISION],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS PRECISION_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(16 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [LENGTH],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(46 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS LENGTH_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(c.scale AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS RADIX,
    CAST(CASE WHEN c.is_nullable = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS NULLABLE,
    CAST(NULL AS varchar(254)) AS REMARKS,
    CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH_28,
    CAST(c.column_id AS int) AS ORDINAL_POSITION,
    CAST(CASE WHEN c.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)) AS IS_NULLABLE,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' AND c.is_nullable = 0 THEN CAST(56 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(38 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(108 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(106 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'text') THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'varbinary' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(50 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(0 AS tinyint)
        ELSE CAST(0 AS tinyint)
    END AS tinyint) AS SS_DATA_TYPE,
    CAST(0 AS smallint) AS SS_IS_SPARSE,
    CAST(0 AS smallint) AS SS_IS_COLUMN_SET,
    CAST(CASE WHEN c.is_computed = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_COMPUTED,
    CAST(CASE WHEN c.is_identity = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_IDENTITY,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_ASSEMBLY_TYPE_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_NAME,
    c.object_id AS object_id,
    o.schema_id AS SCHEMA_ID,
    o.type AS OBJECT_TYPE,
    @ODBCVer AS ODBCVER
FROM sys.all_objects o
INNER JOIN sys.all_columns c ON c.object_id = o.object_id
WHERE o.type IN (N'U', N'V') AND c.is_computed = 0
    ) s_cov
    WHERE s_cov.ODBCVER = @ODBCVer
      AND s_cov.OBJECT_TYPE <> 'TT'
      AND (@table_name IS NULL OR s_cov.TABLE_NAME LIKE @table_name)
      AND (@table_owner IS NULL OR SCHEMA_NAME(s_cov.SCHEMA_ID) LIKE @table_owner)
      AND (@column_name IS NULL OR s_cov.COLUMN_NAME LIKE @column_name);
END
"#####;

const SP_COLUMNS_100: &str = r#####"
DECLARE @full_table_name nvarchar(769);
DECLARE @table_id int;
DECLARE @use_pattern bit;


SELECT @use_pattern = 1;

IF @ODBCVer IS NULL OR @ODBCVer <> 3
    SELECT @ODBCVer = 2;

IF @table_qualifier IS NOT NULL AND DB_NAME() <> @table_qualifier
BEGIN
    SELECT
        CAST(NULL AS nvarchar(128)) AS TABLE_QUALIFIER,
        CAST(NULL AS nvarchar(128)) AS TABLE_OWNER,
        CAST(NULL AS nvarchar(128)) AS TABLE_NAME,
        CAST(NULL AS nvarchar(128)) AS COLUMN_NAME,
        CAST(NULL AS smallint) AS DATA_TYPE,
        CAST(NULL AS nvarchar(128)) AS TYPE_NAME,
        CAST(NULL AS int) AS "PRECISION",
        CAST(NULL AS int) AS "LENGTH",
        CAST(NULL AS smallint) AS SCALE,
        CAST(NULL AS smallint) AS RADIX,
        CAST(NULL AS smallint) AS NULLABLE,
        CAST(NULL AS varchar(254)) AS REMARKS,
        CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
        CAST(NULL AS smallint) AS SQL_DATA_TYPE,
        CAST(NULL AS smallint) AS SQL_DATETIME_SUB,
        CAST(NULL AS int) AS CHAR_OCTET_LENGTH,
        CAST(NULL AS int) AS ORDINAL_POSITION,
        CAST(NULL AS varchar(254)) AS IS_NULLABLE,
        CAST(NULL AS tinyint) AS SS_DATA_TYPE
    WHERE 1 = 0;
    RETURN;
END;

IF @table_name = '%'
    SELECT @table_name = NULL;
IF @table_owner = '%'
    SELECT @table_owner = NULL;
IF @table_qualifier = '%'
    SELECT @table_qualifier = NULL;
IF @column_name = '%'
    SELECT @column_name = NULL;

IF @table_owner = ''
    SELECT @table_owner = ' ';

IF @table_name IS NOT NULL AND @table_owner IS NULL
    SELECT @table_owner = N'dbo';

SELECT @full_table_name = ISNULL(QUOTENAME(ISNULL(@table_owner, N'dbo')), '') + '.' + ISNULL(QUOTENAME(@table_name), '');
SELECT @table_id = OBJECT_ID(@full_table_name);

IF @use_pattern = 1
    AND CHARINDEX('%', ISNULL(@full_table_name, '')) = 0
    AND CHARINDEX('_', ISNULL(@full_table_name, '')) = 0
    AND CHARINDEX('[', ISNULL(@table_name, '')) = 0
    AND CHARINDEX('[', ISNULL(@table_owner, '')) = 0
    AND CHARINDEX('%', ISNULL(@column_name, '')) = 0
    AND CHARINDEX('_', ISNULL(@column_name, '')) = 0
    AND @table_id IS NOT NULL AND @table_id <> 0
    SELECT @use_pattern = 0;

IF @use_pattern = 0
BEGIN
    SELECT
TABLE_QUALIFIER = s_cov.TABLE_QUALIFIER,
    TABLE_OWNER = s_cov.TABLE_OWNER,
    TABLE_NAME = s_cov.TABLE_NAME,
    COLUMN_NAME = s_cov.COLUMN_NAME,
    DATA_TYPE = s_cov.DATA_TYPE_28,
    TYPE_NAME = s_cov.TYPE_NAME_28,
    [PRECISION] = s_cov.PRECISION_28,
    [LENGTH] = s_cov.LENGTH_28,
    SCALE = s_cov.SCALE,
    RADIX = s_cov.RADIX,
    NULLABLE = s_cov.NULLABLE,
    REMARKS = s_cov.REMARKS,
    COLUMN_DEF = s_cov.COLUMN_DEF,
    SQL_DATA_TYPE = s_cov.[SQL_DATA_TYPE],
    SQL_DATETIME_SUB = s_cov.[SQL_DATETIME_SUB],
    CHAR_OCTET_LENGTH = s_cov.CHAR_OCTET_LENGTH,
    ORDINAL_POSITION = s_cov.ORDINAL_POSITION,
    IS_NULLABLE = s_cov.IS_NULLABLE,
    SS_IS_SPARSE = s_cov.SS_IS_SPARSE,
    SS_IS_COLUMN_SET = s_cov.SS_IS_COLUMN_SET,
    SS_IS_COMPUTED = s_cov.SS_IS_COMPUTED,
    SS_IS_IDENTITY = s_cov.SS_IS_IDENTITY,
    SS_UDT_CATALOG_NAME = s_cov.SS_UDT_CATALOG_NAME,
    SS_UDT_SCHEMA_NAME = s_cov.SS_UDT_SCHEMA_NAME,
    SS_UDT_ASSEMBLY_TYPE_NAME = s_cov.SS_UDT_ASSEMBLY_TYPE_NAME,
    SS_XML_SCHEMACOLLECTION_CATALOG_NAME = s_cov.SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    SS_XML_SCHEMACOLLECTION_SCHEMA_NAME = s_cov.SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    SS_XML_SCHEMACOLLECTION_NAME = s_cov.SS_XML_SCHEMACOLLECTION_NAME,
    SS_DATA_TYPE = s_cov.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END AS nvarchar(128)) AS TABLE_OWNER,
    CAST(o.name AS nvarchar(128)) AS TABLE_NAME,
    CAST(c.name AS nvarchar(128)) AS COLUMN_NAME,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(93 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(11 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [PRECISION],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS PRECISION_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(16 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [LENGTH],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(46 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS LENGTH_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(c.scale AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS RADIX,
    CAST(CASE WHEN c.is_nullable = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS NULLABLE,
    CAST(NULL AS varchar(254)) AS REMARKS,
    CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH_28,
    CAST(c.column_id AS int) AS ORDINAL_POSITION,
    CAST(CASE WHEN c.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)) AS IS_NULLABLE,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' AND c.is_nullable = 0 THEN CAST(56 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(38 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(108 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(106 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'text') THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'varbinary' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(50 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(0 AS tinyint)
        ELSE CAST(0 AS tinyint)
    END AS tinyint) AS SS_DATA_TYPE,
    CAST(0 AS smallint) AS SS_IS_SPARSE,
    CAST(0 AS smallint) AS SS_IS_COLUMN_SET,
    CAST(CASE WHEN c.is_computed = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_COMPUTED,
    CAST(CASE WHEN c.is_identity = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_IDENTITY,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_ASSEMBLY_TYPE_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_NAME,
    c.object_id AS object_id,
    o.schema_id AS SCHEMA_ID,
    o.type AS OBJECT_TYPE,
    @ODBCVer AS ODBCVER
FROM sys.all_objects o
INNER JOIN sys.all_columns c ON c.object_id = o.object_id
WHERE o.type IN (N'U', N'V') AND c.is_computed = 0
    ) s_cov
    WHERE s_cov.object_id = @table_id
      AND (@column_name IS NULL OR s_cov.COLUMN_NAME = @column_name)
      AND s_cov.ODBCVER = @ODBCVer
      AND s_cov.OBJECT_TYPE <> 'TT'
      AND @NameScope = 0;
END
ELSE
BEGIN
    SELECT
TABLE_QUALIFIER = s_cov.TABLE_QUALIFIER,
    TABLE_OWNER = s_cov.TABLE_OWNER,
    TABLE_NAME = s_cov.TABLE_NAME,
    COLUMN_NAME = s_cov.COLUMN_NAME,
    DATA_TYPE = s_cov.DATA_TYPE_28,
    TYPE_NAME = s_cov.TYPE_NAME_28,
    [PRECISION] = s_cov.PRECISION_28,
    [LENGTH] = s_cov.LENGTH_28,
    SCALE = s_cov.SCALE,
    RADIX = s_cov.RADIX,
    NULLABLE = s_cov.NULLABLE,
    REMARKS = s_cov.REMARKS,
    COLUMN_DEF = s_cov.COLUMN_DEF,
    SQL_DATA_TYPE = s_cov.[SQL_DATA_TYPE],
    SQL_DATETIME_SUB = s_cov.[SQL_DATETIME_SUB],
    CHAR_OCTET_LENGTH = s_cov.CHAR_OCTET_LENGTH,
    ORDINAL_POSITION = s_cov.ORDINAL_POSITION,
    IS_NULLABLE = s_cov.IS_NULLABLE,
    SS_IS_SPARSE = s_cov.SS_IS_SPARSE,
    SS_IS_COLUMN_SET = s_cov.SS_IS_COLUMN_SET,
    SS_IS_COMPUTED = s_cov.SS_IS_COMPUTED,
    SS_IS_IDENTITY = s_cov.SS_IS_IDENTITY,
    SS_UDT_CATALOG_NAME = s_cov.SS_UDT_CATALOG_NAME,
    SS_UDT_SCHEMA_NAME = s_cov.SS_UDT_SCHEMA_NAME,
    SS_UDT_ASSEMBLY_TYPE_NAME = s_cov.SS_UDT_ASSEMBLY_TYPE_NAME,
    SS_XML_SCHEMACOLLECTION_CATALOG_NAME = s_cov.SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    SS_XML_SCHEMACOLLECTION_SCHEMA_NAME = s_cov.SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    SS_XML_SCHEMACOLLECTION_NAME = s_cov.SS_XML_SCHEMACOLLECTION_NAME,
    SS_DATA_TYPE = s_cov.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(CASE WHEN o.schema_id = 4 THEN N'sys' ELSE N'dbo' END AS nvarchar(128)) AS TABLE_OWNER,
    CAST(o.name AS nvarchar(128)) AS TABLE_NAME,
    CAST(c.name AS nvarchar(128)) AS COLUMN_NAME,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(93 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(11 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [PRECISION],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(23 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(36 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS PRECISION_28,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(16 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [LENGTH],
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 1) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(46 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS LENGTH_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(c.scale AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS RADIX,
    CAST(CASE WHEN c.is_nullable = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS NULLABLE,
    CAST(NULL AS varchar(254)) AS REMARKS,
    CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(12 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
            WHEN N'int' THEN CAST(4 AS smallint)
            WHEN N'bigint' THEN CAST(-5 AS smallint)
            WHEN N'decimal' THEN CAST(3 AS smallint)
            WHEN N'numeric' THEN CAST(2 AS smallint)
            WHEN N'nvarchar' THEN CAST(-9 AS smallint)
            WHEN N'varchar' THEN CAST(-1 AS smallint)
            WHEN N'text' THEN CAST(-1 AS smallint)
            WHEN N'varbinary' THEN CAST(-3 AS smallint)
            WHEN N'datetime2' THEN CAST(-9 AS smallint)
            WHEN N'bit' THEN CAST(-7 AS smallint)
            WHEN N'uniqueidentifier' THEN CAST(-11 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE_28,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' AND @ODBCVer = 3 THEN CAST(3 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SQL_DATETIME_SUB_90,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH_28,
    CAST(c.column_id AS int) AS ORDINAL_POSITION,
    CAST(CASE WHEN c.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)) AS IS_NULLABLE,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' AND c.is_nullable = 0 THEN CAST(56 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(38 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(108 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(106 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'text') THEN CAST(39 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'varbinary' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(50 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(37 AS tinyint)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(0 AS tinyint)
        ELSE CAST(0 AS tinyint)
    END AS tinyint) AS SS_DATA_TYPE,
    CAST(0 AS smallint) AS SS_IS_SPARSE,
    CAST(0 AS smallint) AS SS_IS_COLUMN_SET,
    CAST(CASE WHEN c.is_computed = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_COMPUTED,
    CAST(CASE WHEN c.is_identity = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS SS_IS_IDENTITY,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_UDT_ASSEMBLY_TYPE_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    CAST(NULL AS nvarchar(128)) AS SS_XML_SCHEMACOLLECTION_NAME,
    c.object_id AS object_id,
    o.schema_id AS SCHEMA_ID,
    o.type AS OBJECT_TYPE,
    @ODBCVer AS ODBCVER
FROM sys.all_objects o
INNER JOIN sys.all_columns c ON c.object_id = o.object_id
WHERE o.type IN (N'U', N'V') AND c.is_computed = 0
    ) s_cov
    WHERE s_cov.ODBCVER = @ODBCVer
      AND s_cov.OBJECT_TYPE <> 'TT'
      AND (@table_name IS NULL OR s_cov.TABLE_NAME LIKE @table_name)
      AND (@table_owner IS NULL OR SCHEMA_NAME(s_cov.SCHEMA_ID) LIKE @table_owner)
      AND (@column_name IS NULL OR s_cov.COLUMN_NAME LIKE @column_name)
      AND @NameScope = 0;
END
"#####;

const SP_DATABASES: &str = r#####"
SET NOCOUNT ON;

SELECT
    DATABASE_NAME = CAST(DB_NAME(s_mf.database_id) AS nvarchar(128)),
    DATABASE_SIZE = CAST(
        CASE
            WHEN SUM(CAST(s_mf.size AS bigint)) >= 268435456 THEN NULL
            ELSE SUM(CAST(s_mf.size AS bigint)) * 8
        END AS int),
    REMARKS = CAST(NULL AS varchar(254))
FROM sys.master_files s_mf
WHERE s_mf.state = 0
  AND HAS_DBACCESS(DB_NAME(s_mf.database_id)) = 1
GROUP BY s_mf.database_id
"#####;

const SP_SERVER_INFO: &str = r#####"

SELECT
    CAST(attribute_id AS int) AS attribute_id,
    CAST(attribute_name AS varchar(60)) AS attribute_name,
    CAST(attribute_value AS varchar(255)) AS attribute_value
FROM (
    SELECT CAST(1 AS int) AS attribute_id, CAST('DBMS_NAME' AS varchar(60)) AS attribute_name, CAST(N'VaubanDB' AS varchar(255)) AS attribute_value
    UNION ALL SELECT 2, 'DBMS_VER', CAST(N'VaubanDB (SQL Server compatible) - ' + CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS varchar(255))
    UNION ALL SELECT 10, 'OWNER_TERM', 'owner'
    UNION ALL SELECT 11, 'TABLE_TERM', 'table'
    UNION ALL SELECT 12, 'MAX_OWNER_NAME_LENGTH', '128'
    UNION ALL SELECT 13, 'TABLE_LENGTH', '128'
    UNION ALL SELECT 14, 'MAX_QUAL_LENGTH', '128'
    UNION ALL SELECT 15, 'COLUMN_LENGTH', '128'
    UNION ALL SELECT 16, 'IDENTIFIER_CASE', CASE WHEN N'a' <> N'A' THEN 'SENSITIVE' ELSE 'MIXED' END
    UNION ALL SELECT 17, 'TX_ISOLATION', '2'
    UNION ALL SELECT 18, 'COLLATION_SEQ', CAST(
        N'charset=' + ISNULL(CAST(SERVERPROPERTY('SqlCharSetName') AS nvarchar(255)), N'iso_1') +
        CASE WHEN ISNULL(CAST(SERVERPROPERTY('SqlSortOrder') AS int), 52) = 0
        THEN N' collation=' + ISNULL(CAST(SERVERPROPERTY('Collation') AS nvarchar(255)), N' ')
        ELSE N' sort_order=' + ISNULL(CAST(SERVERPROPERTY('SqlSortOrderName') AS nvarchar(64)), N'nocase_iso') +
             N' charset_num=' + RTRIM(CAST(ISNULL(CAST(SERVERPROPERTY('SqlCharSet') AS int), 1) AS char(4))) +
             N' sort_order_num=' + RTRIM(CAST(ISNULL(CAST(SERVERPROPERTY('SqlSortOrder') AS int), 52) AS char(4)))
        END AS varchar(255))
    UNION ALL SELECT 19, 'SAVEPOINT_SUPPORT', 'Y'
    UNION ALL SELECT 20, 'MULTI_RESULT_SETS', 'Y'
    UNION ALL SELECT 22, 'ACCESSIBLE_TABLES', 'Y'
    UNION ALL SELECT 100, 'USERID_LENGTH', '128'
    UNION ALL SELECT 101, 'QUALIFIER_TERM', 'database'
    UNION ALL SELECT 102, 'NAMED_TRANSACTIONS', 'Y'
    UNION ALL SELECT 103, 'SPROC_AS_LANGUAGE', 'Y'
    UNION ALL SELECT 104, 'ACCESSIBLE_SPROC', 'Y'
    UNION ALL SELECT 105, 'MAX_INDEX_COLS', '16'
    UNION ALL SELECT 106, 'RENAME_TABLE', 'Y'
    UNION ALL SELECT 107, 'RENAME_COLUMN', 'Y'
    UNION ALL SELECT 108, 'DROP_COLUMN', 'Y'
    UNION ALL SELECT 109, 'INCREASE_COLUMN_LENGTH', 'Y'
    UNION ALL SELECT 110, 'DDL_IN_TRANSACTION', 'Y'
    UNION ALL SELECT 111, 'DESCENDING_INDEXES', 'Y'
    UNION ALL SELECT 112, 'SP_RENAME', 'Y'
    UNION ALL SELECT 113, 'REMOTE_SPROC', 'Y'
    UNION ALL SELECT 500, 'SYS_SPROC_VERSION', LEFT(REPLACE(CAST(SERVERPROPERTY('ProductVersion') AS varchar(20)), N'.0.', N'.00.'), 10)
) AS t
WHERE @attribute_id IS NULL OR @attribute_id = attribute_id

"#####;

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

    fn nvarchar(value: &str) -> Value {
        Value::String(SqlString {
            text: value.to_owned(),
        })
    }

    fn int(value: i32) -> Value {
        Value::I32(value)
    }

    fn nvarchar_ty() -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Max), false)
    }

    fn int_ty() -> TypeInfo {
        TypeInfo::new(SqlType::Int, false)
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
    fn sp_tables_returns_template_with_defaults() {
        let action = resolve("sp_tables", &[]).unwrap().unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
        if let ProcAction::Template { params, .. } = action {
            assert_eq!(params.len(), 5);
            assert_eq!(params[0].name, "@table_name");
            assert_eq!(params[0].value, Value::Null);
            assert_eq!(params[4].name, "@fUsePattern");
            assert_eq!(params[4].value, Value::Bit(true));
        }
    }

    #[test]
    fn sp_columns_rejects_fusepattern() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let err = resolve(
            "sp_columns",
            &[
                arg(Some("@table_name"), &nv, &name),
                arg(Some("@fUsePattern"), &int_ty(), &int(0)),
            ],
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(err.number, 8145);
    }

    #[test]
    fn sp_columns_100_aliases_to_template() {
        let name = nvarchar("t");
        let nv = nvarchar_ty();
        let action = resolve("sp_columns_100", &[arg(Some("@table_name"), &nv, &name)])
            .unwrap()
            .unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
    }

    #[test]
    fn sp_databases_rejects_arguments() {
        let err = resolve("sp_databases", &[arg(None, &int_ty(), &int(1))])
            .unwrap()
            .unwrap_err();
        assert_eq!(err.number, 8146);
    }

    #[test]
    fn sp_server_info_optional_attribute_id() {
        let action = resolve("sp_server_info", &[]).unwrap().unwrap();
        if let ProcAction::Template { params, .. } = action {
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "@attribute_id");
            assert_eq!(params[0].value, Value::Null);
        } else {
            panic!("expected Template");
        }
    }
}
