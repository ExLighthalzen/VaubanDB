//! Catalog procedures (`sp_tables`, `sp_columns`, …).

use vauban_errors::SqlResult;
use vauban_session::{CallForm, ParamDefault, ProcSignature, SignatureParam, bind_arguments};
use vauban_types::{Len, SqlType};

use crate::procedures::{ProcAction, SystemProc};
use crate::special_procs::ProcArg;

const SP_TABLES_PARAMS: &[SignatureParam<'static>] = &[
    SignatureParam::new("@table_name", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@table_owner", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@table_qualifier", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@table_type", SqlType::VarChar(Len::Fixed(100))),
    SignatureParam::new("@fUsePattern", SqlType::Bit).with_default(ParamDefault::Bit(true)),
];

const SP_COLUMNS_PARAMS: &[SignatureParam<'static>] = &[
    SignatureParam::new("@table_name", SqlType::NVarChar(Len::Max)).required(),
    SignatureParam::new("@table_owner", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@table_qualifier", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@column_name", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@ODBCVer", SqlType::Int).with_default(ParamDefault::Int(2)),
];

const SP_COLUMNS_100_EXTRA: &[SignatureParam<'static>] = &[
    SignatureParam::new("@NameScope", SqlType::Int).with_default(ParamDefault::Int(0)),
    SignatureParam::new("@fUsePattern", SqlType::Bit).with_default(ParamDefault::Bit(true)),
];

const SP_SERVER_INFO_PARAMS: &[SignatureParam<'static>] =
    &[SignatureParam::new("@attribute_id", SqlType::Int)];

const _: &[SystemProc] = crate::who_procs::PROCS;

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
    resolve_template("sp_databases", &[], SP_DATABASES, args)
}

fn resolve_sp_columns(args: &[ProcArg<'_>], columns_100: bool) -> SqlResult<ProcAction> {
    let proc = if columns_100 {
        "sp_columns_100"
    } else {
        "sp_columns"
    };
    let mut specs: Vec<SignatureParam<'static>> = SP_COLUMNS_PARAMS.to_vec();
    if columns_100 {
        specs.extend_from_slice(SP_COLUMNS_100_EXTRA);
    }
    let sql = if columns_100 {
        SP_COLUMNS_100
    } else {
        SP_COLUMNS
    };
    resolve_template(proc, &specs, sql, args)
}

fn resolve_template(
    procedure: &str,
    params: &[SignatureParam<'_>],
    sql: &str,
    args: &[ProcArg<'_>],
) -> SqlResult<ProcAction> {
    let signature = ProcSignature { procedure, params };
    let params = bind_arguments(&signature, CallForm::Procedure, args)?;
    Ok(ProcAction::Template {
        sql: sql.to_owned(),
        params,
    })
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

IF @ODBCVer IS NULL
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
TABLE_QUALIFIER = col_src.TABLE_QUALIFIER,
    TABLE_OWNER = col_src.TABLE_OWNER,
    TABLE_NAME = col_src.TABLE_NAME,
    COLUMN_NAME = col_src.COLUMN_NAME,
    DATA_TYPE = col_src.odbc_data_type,
    TYPE_NAME = col_src.odbc_type_name,
    [PRECISION] = col_src.odbc_precision,
    [LENGTH] = col_src.odbc_length,
    SCALE = col_src.SCALE,
    RADIX = col_src.RADIX,
    NULLABLE = col_src.NULLABLE,
    REMARKS = col_src.REMARKS,
    COLUMN_DEF = col_src.COLUMN_DEF,
    SQL_DATA_TYPE = col_src.odbc_sql_data_type,
    SQL_DATETIME_SUB = col_src.odbc_sql_datetime_sub,
    CHAR_OCTET_LENGTH = col_src.odbc_char_octet_length,
    ORDINAL_POSITION = col_src.ORDINAL_POSITION,
    IS_NULLABLE = col_src.IS_NULLABLE,
    SS_DATA_TYPE = col_src.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(ISNULL(@table_owner, N'dbo') AS nvarchar(128)) AS TABLE_OWNER,
    CAST(OBJECT_NAME(c.object_id) AS nvarchar(128)) AS TABLE_NAME,
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
    END AS smallint) AS odbc_data_type,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS odbc_type_name,
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
    END AS int) AS odbc_precision,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
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
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(46 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS odbc_length,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint') THEN CAST(0 AS smallint) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS odbc_scale,
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
    END AS smallint) AS odbc_sql_data_type,
    CAST(NULL AS smallint) AS odbc_sql_datetime_sub,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS odbc_char_octet_length,
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
    c.object_id AS object_id
FROM sys.all_columns c
WHERE c.is_computed = 0
  AND c.object_id IN (SELECT o.object_id FROM sys.all_objects o WHERE o.type IN (N'U', N'V'))
    ) col_src
    WHERE col_src.object_id = @table_id
      AND (@column_name IS NULL OR col_src.COLUMN_NAME = @column_name);
END
ELSE
BEGIN
    SELECT
TABLE_QUALIFIER = col_src.TABLE_QUALIFIER,
    TABLE_OWNER = col_src.TABLE_OWNER,
    TABLE_NAME = col_src.TABLE_NAME,
    COLUMN_NAME = col_src.COLUMN_NAME,
    DATA_TYPE = col_src.odbc_data_type,
    TYPE_NAME = col_src.odbc_type_name,
    [PRECISION] = col_src.odbc_precision,
    [LENGTH] = col_src.odbc_length,
    SCALE = col_src.SCALE,
    RADIX = col_src.RADIX,
    NULLABLE = col_src.NULLABLE,
    REMARKS = col_src.REMARKS,
    COLUMN_DEF = col_src.COLUMN_DEF,
    SQL_DATA_TYPE = col_src.odbc_sql_data_type,
    SQL_DATETIME_SUB = col_src.odbc_sql_datetime_sub,
    CHAR_OCTET_LENGTH = col_src.odbc_char_octet_length,
    ORDINAL_POSITION = col_src.ORDINAL_POSITION,
    IS_NULLABLE = col_src.IS_NULLABLE,
    SS_DATA_TYPE = col_src.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(ISNULL(@table_owner, N'dbo') AS nvarchar(128)) AS TABLE_OWNER,
    CAST(OBJECT_NAME(c.object_id) AS nvarchar(128)) AS TABLE_NAME,
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
    END AS smallint) AS odbc_data_type,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN N'text'
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'numeric' THEN N'numeric'
        ELSE (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END)
    END AS nvarchar(128)) AS odbc_type_name,
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
    END AS int) AS odbc_precision,
    CAST(CASE
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'int' THEN CAST(4 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bigint' THEN CAST(8 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
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
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'datetime2' THEN CAST(46 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'bit' THEN CAST(1 AS int)
        WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'uniqueidentifier' THEN CAST(16 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS odbc_length,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint') THEN CAST(0 AS smallint) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(10 AS smallint) ELSE CAST(NULL AS smallint) END AS smallint) AS odbc_scale,
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
    END AS smallint) AS odbc_sql_data_type,
    CAST(NULL AS smallint) AS odbc_sql_datetime_sub,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) = N'nvarchar' THEN CAST(c.max_length AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN (CASE c.user_type_id WHEN 56 THEN N'int' WHEN 127 THEN N'bigint' WHEN 106 THEN N'decimal' WHEN 108 THEN N'numeric' WHEN 231 THEN N'nvarchar' WHEN 167 THEN N'varchar' WHEN 165 THEN N'varbinary' WHEN 42 THEN N'datetime2' WHEN 104 THEN N'bit' WHEN 36 THEN N'uniqueidentifier' ELSE N'' END) IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int) ELSE CAST(NULL AS int) END AS int) AS odbc_char_octet_length,
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
    c.object_id AS object_id
FROM sys.all_columns c
WHERE c.is_computed = 0
  AND c.object_id IN (SELECT o.object_id FROM sys.all_objects o WHERE o.type IN (N'U', N'V'))
    ) col_src
    WHERE (@table_id IS NULL OR col_src.object_id = @table_id)
      AND (@table_name IS NULL OR col_src.TABLE_NAME LIKE @table_name)
      AND (@table_owner IS NULL OR col_src.TABLE_OWNER LIKE @table_owner)
      AND (@column_name IS NULL OR col_src.COLUMN_NAME LIKE @column_name);
END
"#####;

const SP_COLUMNS_100: &str = r#####"
DECLARE @full_table_name nvarchar(769);
DECLARE @table_id int;
DECLARE @use_pattern bit;


SELECT @use_pattern = 1;

IF @ODBCVer IS NULL
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
TABLE_QUALIFIER = col_src.TABLE_QUALIFIER,
    TABLE_OWNER = col_src.TABLE_OWNER,
    TABLE_NAME = col_src.TABLE_NAME,
    COLUMN_NAME = col_src.COLUMN_NAME,
    DATA_TYPE = col_src.odbc_data_type,
    TYPE_NAME = col_src.odbc_type_name,
    [PRECISION] = col_src.odbc_precision,
    [LENGTH] = col_src.odbc_length,
    SCALE = col_src.SCALE,
    RADIX = col_src.RADIX,
    NULLABLE = col_src.NULLABLE,
    REMARKS = col_src.REMARKS,
    COLUMN_DEF = col_src.COLUMN_DEF,
    SQL_DATA_TYPE = col_src.odbc_sql_data_type,
    SQL_DATETIME_SUB = col_src.odbc_sql_datetime_sub,
    CHAR_OCTET_LENGTH = col_src.odbc_char_octet_length,
    ORDINAL_POSITION = col_src.ORDINAL_POSITION,
    IS_NULLABLE = col_src.IS_NULLABLE,
    SS_IS_SPARSE = col_src.SS_IS_SPARSE,
    SS_IS_COLUMN_SET = col_src.SS_IS_COLUMN_SET,
    SS_IS_COMPUTED = col_src.SS_IS_COMPUTED,
    SS_IS_IDENTITY = col_src.SS_IS_IDENTITY,
    SS_UDT_CATALOG_NAME = col_src.SS_UDT_CATALOG_NAME,
    SS_UDT_SCHEMA_NAME = col_src.SS_UDT_SCHEMA_NAME,
    SS_UDT_ASSEMBLY_TYPE_NAME = col_src.SS_UDT_ASSEMBLY_TYPE_NAME,
    SS_XML_SCHEMACOLLECTION_CATALOG_NAME = col_src.SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    SS_XML_SCHEMACOLLECTION_SCHEMA_NAME = col_src.SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    SS_XML_SCHEMACOLLECTION_NAME = col_src.SS_XML_SCHEMACOLLECTION_NAME,
    SS_DATA_TYPE = col_src.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(ISNULL(@table_owner, N'dbo') AS nvarchar(128)) AS TABLE_OWNER,
    CAST(OBJECT_NAME(c.object_id) AS nvarchar(128)) AS TABLE_NAME,
    CAST(c.name AS nvarchar(128)) AS COLUMN_NAME,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(93 AS smallint)
            WHEN N'smalldatetime' THEN CAST(93 AS smallint)
            WHEN N'date' THEN CAST(91 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(11 AS smallint)
            WHEN N'smalldatetime' THEN CAST(11 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(93 AS smallint)
            WHEN N'smalldatetime' THEN CAST(93 AS smallint)
            WHEN N'date' THEN CAST(91 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(11 AS smallint)
            WHEN N'smalldatetime' THEN CAST(11 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS odbc_data_type,
    CAST(CASE
        WHEN t.name IN (N'varchar') AND c.max_length = -1 THEN N'varchar'
        WHEN t.name = N'numeric' THEN N'numeric'
        ELSE t.name
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN t.name IN (N'varchar') AND c.max_length = -1 THEN N'varchar'
        WHEN t.name = N'numeric' THEN N'numeric'
        ELSE t.name
    END AS nvarchar(128)) AS odbc_type_name,
    CAST(CASE
        WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(23 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(36 AS int)
        WHEN t.name IN (N'smallint', N'tinyint', N'money', N'smallmoney') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime', N'date', N'time', N'datetimeoffset') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'float' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 53 ELSE 15 END AS int)
        WHEN t.name = N'real' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 24 ELSE 7 END AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(1073741823 AS int)
        WHEN t.name IN (N'xml', N'sql_variant') THEN CAST(0 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [PRECISION],
    CAST(CASE
        WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(23 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(36 AS int)
        WHEN t.name IN (N'smallint', N'tinyint', N'money', N'smallmoney') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime', N'date', N'time', N'datetimeoffset') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'float' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 53 ELSE 15 END AS int)
        WHEN t.name = N'real' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 24 ELSE 7 END AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(1073741823 AS int)
        WHEN t.name IN (N'xml', N'sql_variant') THEN CAST(0 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS odbc_precision,
    CAST(CASE
        WHEN t.name = N'int' THEN CAST(4 AS int)
        WHEN t.name = N'bigint' THEN CAST(8 AS int)
        WHEN t.name IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(16 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(16 AS int)
        WHEN t.name = N'smallint' THEN CAST(2 AS int)
        WHEN t.name = N'tinyint' THEN CAST(1 AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime') THEN CAST(16 AS int)
        WHEN t.name = N'date' THEN CAST(6 AS int)
        WHEN t.name = N'time' THEN CAST(12 AS int)
        WHEN t.name = N'float' THEN CAST(8 AS int)
        WHEN t.name = N'real' THEN CAST(4 AS int)
        WHEN t.name = N'money' THEN CAST(21 AS int)
        WHEN t.name = N'smallmoney' THEN CAST(12 AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'datetimeoffset' THEN CAST(20 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [LENGTH],
    CAST(CASE
        WHEN t.name = N'int' THEN CAST(4 AS int)
        WHEN t.name = N'bigint' THEN CAST(8 AS int)
        WHEN t.name IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(16 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(16 AS int)
        WHEN t.name = N'smallint' THEN CAST(2 AS int)
        WHEN t.name = N'tinyint' THEN CAST(1 AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime') THEN CAST(16 AS int)
        WHEN t.name = N'date' THEN CAST(6 AS int)
        WHEN t.name = N'time' THEN CAST(12 AS int)
        WHEN t.name = N'float' THEN CAST(8 AS int)
        WHEN t.name = N'real' THEN CAST(4 AS int)
        WHEN t.name = N'money' THEN CAST(21 AS int)
        WHEN t.name = N'smallmoney' THEN CAST(12 AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'datetimeoffset' THEN CAST(20 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS odbc_length,
    CAST(CASE WHEN t.name IN (N'int', N'bigint') THEN CAST(0 AS smallint) WHEN t.name IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) WHEN t.name = N'datetime2' THEN CAST(c.scale AS smallint)
        WHEN t.name IN (N'smallint', N'tinyint') THEN CAST(0 AS smallint)
        WHEN t.name IN (N'money', N'smallmoney') THEN CAST(c.scale AS smallint)
        WHEN t.name IN (N'datetime', N'smalldatetime', N'date', N'time', N'datetimeoffset') THEN CAST(c.scale AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric', N'smallint', N'tinyint', N'money', N'smallmoney', N'sql_variant') THEN CAST(10 AS smallint)
        WHEN t.name IN (N'float', N'real') THEN CAST(CASE WHEN @ODBCVer = 3 THEN 2 ELSE 10 END AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS odbc_scale,
    CAST(CASE WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric', N'smallint', N'tinyint', N'money', N'smallmoney', N'sql_variant') THEN CAST(10 AS smallint)
        WHEN t.name IN (N'float', N'real') THEN CAST(CASE WHEN @ODBCVer = 3 THEN 2 ELSE 10 END AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS RADIX,
    CAST(CASE WHEN c.is_nullable = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS NULLABLE,
    CAST(NULL AS varchar(254)) AS REMARKS,
    CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS odbc_sql_data_type,
    CAST(CASE WHEN t.name = N'datetime2' THEN CAST(3 AS smallint)
        WHEN t.name IN (N'datetime', N'smalldatetime') THEN CAST(3 AS smallint)
        WHEN t.name = N'date' THEN CAST(1 AS smallint)
        WHEN t.name IN (N'time', N'datetimeoffset') THEN CAST(0 AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS odbc_sql_datetime_sub,
    CAST(CASE WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int) WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'char', N'nchar', N'binary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int) WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'char', N'nchar', N'binary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(NULL AS int) END AS int) AS odbc_char_octet_length,
    CAST(c.column_id AS int) AS ORDINAL_POSITION,
    CAST(CASE WHEN c.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)) AS IS_NULLABLE,
    CAST(CASE
        WHEN t.name = N'int' AND c.is_nullable = 0 THEN CAST(56 AS tinyint)
        WHEN t.name = N'int' THEN CAST(38 AS tinyint)
        WHEN t.name = N'bigint' THEN CAST(108 AS tinyint)
        WHEN t.name IN (N'decimal', N'numeric') THEN CAST(106 AS tinyint)
        WHEN t.name = N'nvarchar' THEN CAST(39 AS tinyint)
        WHEN t.name = N'varchar' THEN CAST(39 AS tinyint)
        WHEN t.name = N'varbinary' THEN CAST(37 AS tinyint)
        WHEN t.name = N'bit' THEN CAST(50 AS tinyint)
        WHEN t.name = N'uniqueidentifier' THEN CAST(37 AS tinyint)
        WHEN t.name = N'datetime2' THEN CAST(0 AS tinyint)
        WHEN t.name = N'smallint' AND c.is_nullable = 0 THEN CAST(52 AS tinyint)
        WHEN t.name = N'smallint' THEN CAST(38 AS tinyint)
        WHEN t.name = N'tinyint' AND c.is_nullable = 0 THEN CAST(48 AS tinyint)
        WHEN t.name = N'tinyint' THEN CAST(38 AS tinyint)
        WHEN t.name IN (N'char', N'nchar') AND c.is_nullable = 0 THEN CAST(47 AS tinyint)
        WHEN t.name IN (N'char', N'nchar') THEN CAST(39 AS tinyint)
        WHEN t.name = N'sql_variant' THEN CAST(39 AS tinyint)
        WHEN t.name = N'datetime' AND c.is_nullable = 0 THEN CAST(61 AS tinyint)
        WHEN t.name = N'datetime' THEN CAST(111 AS tinyint)
        WHEN t.name = N'smalldatetime' AND c.is_nullable = 0 THEN CAST(58 AS tinyint)
        WHEN t.name = N'smalldatetime' THEN CAST(111 AS tinyint)
        WHEN t.name IN (N'date', N'time', N'datetimeoffset', N'xml') THEN CAST(0 AS tinyint)
        WHEN t.name = N'float' AND c.is_nullable = 0 THEN CAST(62 AS tinyint)
        WHEN t.name = N'float' THEN CAST(109 AS tinyint)
        WHEN t.name = N'real' AND c.is_nullable = 0 THEN CAST(59 AS tinyint)
        WHEN t.name = N'real' THEN CAST(109 AS tinyint)
        WHEN t.name = N'money' AND c.is_nullable = 0 THEN CAST(60 AS tinyint)
        WHEN t.name = N'money' THEN CAST(110 AS tinyint)
        WHEN t.name = N'smallmoney' AND c.is_nullable = 0 THEN CAST(122 AS tinyint)
        WHEN t.name = N'smallmoney' THEN CAST(110 AS tinyint)
        WHEN t.name = N'binary' AND c.is_nullable = 0 THEN CAST(45 AS tinyint)
        WHEN t.name = N'binary' THEN CAST(37 AS tinyint)
        WHEN t.name = N'image' THEN CAST(34 AS tinyint)
        WHEN t.name IN (N'text', N'ntext') THEN CAST(35 AS tinyint)
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
    c.object_id AS object_id
FROM sys.all_columns c
JOIN sys.types t ON t.user_type_id = c.user_type_id
WHERE c.is_computed = 0
  AND c.object_id IN (SELECT o.object_id FROM sys.all_objects o WHERE o.type IN (N'U', N'V'))
    ) col_src
    WHERE col_src.object_id = @table_id
      AND (@column_name IS NULL OR col_src.COLUMN_NAME = @column_name)
      AND @NameScope = 0;
END
ELSE
BEGIN
    SELECT
TABLE_QUALIFIER = col_src.TABLE_QUALIFIER,
    TABLE_OWNER = col_src.TABLE_OWNER,
    TABLE_NAME = col_src.TABLE_NAME,
    COLUMN_NAME = col_src.COLUMN_NAME,
    DATA_TYPE = col_src.odbc_data_type,
    TYPE_NAME = col_src.odbc_type_name,
    [PRECISION] = col_src.odbc_precision,
    [LENGTH] = col_src.odbc_length,
    SCALE = col_src.SCALE,
    RADIX = col_src.RADIX,
    NULLABLE = col_src.NULLABLE,
    REMARKS = col_src.REMARKS,
    COLUMN_DEF = col_src.COLUMN_DEF,
    SQL_DATA_TYPE = col_src.odbc_sql_data_type,
    SQL_DATETIME_SUB = col_src.odbc_sql_datetime_sub,
    CHAR_OCTET_LENGTH = col_src.odbc_char_octet_length,
    ORDINAL_POSITION = col_src.ORDINAL_POSITION,
    IS_NULLABLE = col_src.IS_NULLABLE,
    SS_IS_SPARSE = col_src.SS_IS_SPARSE,
    SS_IS_COLUMN_SET = col_src.SS_IS_COLUMN_SET,
    SS_IS_COMPUTED = col_src.SS_IS_COMPUTED,
    SS_IS_IDENTITY = col_src.SS_IS_IDENTITY,
    SS_UDT_CATALOG_NAME = col_src.SS_UDT_CATALOG_NAME,
    SS_UDT_SCHEMA_NAME = col_src.SS_UDT_SCHEMA_NAME,
    SS_UDT_ASSEMBLY_TYPE_NAME = col_src.SS_UDT_ASSEMBLY_TYPE_NAME,
    SS_XML_SCHEMACOLLECTION_CATALOG_NAME = col_src.SS_XML_SCHEMACOLLECTION_CATALOG_NAME,
    SS_XML_SCHEMACOLLECTION_SCHEMA_NAME = col_src.SS_XML_SCHEMACOLLECTION_SCHEMA_NAME,
    SS_XML_SCHEMACOLLECTION_NAME = col_src.SS_XML_SCHEMACOLLECTION_NAME,
    SS_DATA_TYPE = col_src.SS_DATA_TYPE
    FROM (
SELECT
    CAST(DB_NAME() AS nvarchar(128)) AS TABLE_QUALIFIER,
    CAST(ISNULL(@table_owner, N'dbo') AS nvarchar(128)) AS TABLE_OWNER,
    CAST(OBJECT_NAME(c.object_id) AS nvarchar(128)) AS TABLE_NAME,
    CAST(c.name AS nvarchar(128)) AS COLUMN_NAME,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(93 AS smallint)
            WHEN N'smalldatetime' THEN CAST(93 AS smallint)
            WHEN N'date' THEN CAST(91 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(11 AS smallint)
            WHEN N'smalldatetime' THEN CAST(11 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(93 AS smallint)
            WHEN N'smalldatetime' THEN CAST(93 AS smallint)
            WHEN N'date' THEN CAST(91 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(11 AS smallint)
            WHEN N'smalldatetime' THEN CAST(11 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS odbc_data_type,
    CAST(CASE
        WHEN t.name IN (N'varchar') AND c.max_length = -1 THEN N'varchar'
        WHEN t.name = N'numeric' THEN N'numeric'
        ELSE t.name
    END AS nvarchar(128)) AS TYPE_NAME,
    CAST(CASE
        WHEN t.name IN (N'varchar') AND c.max_length = -1 THEN N'varchar'
        WHEN t.name = N'numeric' THEN N'numeric'
        ELSE t.name
    END AS nvarchar(128)) AS odbc_type_name,
    CAST(CASE
        WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(23 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(36 AS int)
        WHEN t.name IN (N'smallint', N'tinyint', N'money', N'smallmoney') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime', N'date', N'time', N'datetimeoffset') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'float' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 53 ELSE 15 END AS int)
        WHEN t.name = N'real' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 24 ELSE 7 END AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(1073741823 AS int)
        WHEN t.name IN (N'xml', N'sql_variant') THEN CAST(0 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [PRECISION],
    CAST(CASE
        WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(23 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(36 AS int)
        WHEN t.name IN (N'smallint', N'tinyint', N'money', N'smallmoney') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length / 2 AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime', N'date', N'time', N'datetimeoffset') THEN CAST(c.[precision] AS int)
        WHEN t.name = N'float' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 53 ELSE 15 END AS int)
        WHEN t.name = N'real' THEN CAST(CASE WHEN @ODBCVer = 3 THEN 24 ELSE 7 END AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(1073741823 AS int)
        WHEN t.name IN (N'xml', N'sql_variant') THEN CAST(0 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS odbc_precision,
    CAST(CASE
        WHEN t.name = N'int' THEN CAST(4 AS int)
        WHEN t.name = N'bigint' THEN CAST(8 AS int)
        WHEN t.name IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(16 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(16 AS int)
        WHEN t.name = N'smallint' THEN CAST(2 AS int)
        WHEN t.name = N'tinyint' THEN CAST(1 AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime') THEN CAST(16 AS int)
        WHEN t.name = N'date' THEN CAST(6 AS int)
        WHEN t.name = N'time' THEN CAST(12 AS int)
        WHEN t.name = N'float' THEN CAST(8 AS int)
        WHEN t.name = N'real' THEN CAST(4 AS int)
        WHEN t.name = N'money' THEN CAST(21 AS int)
        WHEN t.name = N'smallmoney' THEN CAST(12 AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'datetimeoffset' THEN CAST(20 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS [LENGTH],
    CAST(CASE
        WHEN t.name = N'int' THEN CAST(4 AS int)
        WHEN t.name = N'bigint' THEN CAST(8 AS int)
        WHEN t.name IN (N'decimal', N'numeric') THEN CAST(5 + (c.[precision] + 3) / 2 + (c.[precision] + 9) / 10 AS int)
        WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int)
        WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int)
        WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name = N'datetime2' THEN CAST(16 AS int)
        WHEN t.name = N'bit' THEN CAST(1 AS int)
        WHEN t.name = N'uniqueidentifier' THEN CAST(16 AS int)
        WHEN t.name = N'smallint' THEN CAST(2 AS int)
        WHEN t.name = N'tinyint' THEN CAST(1 AS int)
        WHEN t.name = N'char' THEN CAST(c.max_length AS int)
        WHEN t.name = N'nchar' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'datetime', N'smalldatetime') THEN CAST(16 AS int)
        WHEN t.name = N'date' THEN CAST(6 AS int)
        WHEN t.name = N'time' THEN CAST(12 AS int)
        WHEN t.name = N'float' THEN CAST(8 AS int)
        WHEN t.name = N'real' THEN CAST(4 AS int)
        WHEN t.name = N'money' THEN CAST(21 AS int)
        WHEN t.name = N'smallmoney' THEN CAST(12 AS int)
        WHEN t.name = N'binary' THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'datetimeoffset' THEN CAST(20 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(0 AS int)
    END AS int) AS odbc_length,
    CAST(CASE WHEN t.name IN (N'int', N'bigint') THEN CAST(0 AS smallint) WHEN t.name IN (N'decimal', N'numeric') THEN CAST(c.scale AS smallint) WHEN t.name = N'datetime2' THEN CAST(c.scale AS smallint)
        WHEN t.name IN (N'smallint', N'tinyint') THEN CAST(0 AS smallint)
        WHEN t.name IN (N'money', N'smallmoney') THEN CAST(c.scale AS smallint)
        WHEN t.name IN (N'datetime', N'smalldatetime', N'date', N'time', N'datetimeoffset') THEN CAST(c.scale AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS SCALE,
    CAST(CASE WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric', N'smallint', N'tinyint', N'money', N'smallmoney', N'sql_variant') THEN CAST(10 AS smallint)
        WHEN t.name IN (N'float', N'real') THEN CAST(CASE WHEN @ODBCVer = 3 THEN 2 ELSE 10 END AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS odbc_scale,
    CAST(CASE WHEN t.name IN (N'int', N'bigint', N'decimal', N'numeric', N'smallint', N'tinyint', N'money', N'smallmoney', N'sql_variant') THEN CAST(10 AS smallint)
        WHEN t.name IN (N'float', N'real') THEN CAST(CASE WHEN @ODBCVer = 3 THEN 2 ELSE 10 END AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS RADIX,
    CAST(CASE WHEN c.is_nullable = 1 THEN CAST(1 AS smallint) ELSE CAST(0 AS smallint) END AS smallint) AS NULLABLE,
    CAST(NULL AS varchar(254)) AS REMARKS,
    CAST(NULL AS nvarchar(4000)) AS COLUMN_DEF,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS SQL_DATA_TYPE,
    CAST(CASE WHEN @ODBCVer = 3 THEN
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    ELSE
        CASE t.name
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
            WHEN N'smallint' THEN CAST(5 AS smallint)
            WHEN N'tinyint' THEN CAST(-6 AS smallint)
            WHEN N'char' THEN CAST(1 AS smallint)
            WHEN N'nchar' THEN CAST(-8 AS smallint)
            WHEN N'datetime' THEN CAST(9 AS smallint)
            WHEN N'smalldatetime' THEN CAST(9 AS smallint)
            WHEN N'date' THEN CAST(9 AS smallint)
            WHEN N'time' THEN CAST(-154 AS smallint)
            WHEN N'float' THEN CAST(6 AS smallint)
            WHEN N'real' THEN CAST(7 AS smallint)
            WHEN N'money' THEN CAST(3 AS smallint)
            WHEN N'smallmoney' THEN CAST(3 AS smallint)
            WHEN N'binary' THEN CAST(-2 AS smallint)
            WHEN N'image' THEN CAST(-4 AS smallint)
            WHEN N'ntext' THEN CAST(-10 AS smallint)
            WHEN N'datetimeoffset' THEN CAST(-155 AS smallint)
            WHEN N'xml' THEN CAST(-152 AS smallint)
            WHEN N'sql_variant' THEN CAST(-150 AS smallint)
            ELSE CAST(0 AS smallint)
        END
    END AS smallint) AS odbc_sql_data_type,
    CAST(CASE WHEN t.name = N'datetime2' THEN CAST(3 AS smallint)
        WHEN t.name IN (N'datetime', N'smalldatetime') THEN CAST(3 AS smallint)
        WHEN t.name = N'date' THEN CAST(1 AS smallint)
        WHEN t.name IN (N'time', N'datetimeoffset') THEN CAST(0 AS smallint)
        ELSE CAST(NULL AS smallint) END AS smallint) AS odbc_sql_datetime_sub,
    CAST(CASE WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int) WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'char', N'nchar', N'binary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(NULL AS int) END AS int) AS CHAR_OCTET_LENGTH,
    CAST(CASE WHEN t.name = N'nvarchar' THEN CAST(c.max_length AS int) WHEN t.name = N'varchar' AND c.max_length = -1 THEN CAST(0 AS int)
        WHEN t.name = N'varbinary' AND c.max_length = -1 THEN CAST(2147483647 AS int) WHEN t.name IN (N'varchar', N'varbinary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'char', N'nchar', N'binary') THEN CAST(c.max_length AS int)
        WHEN t.name IN (N'image', N'text') THEN CAST(2147483647 AS int)
        WHEN t.name = N'ntext' THEN CAST(2147483646 AS int)
        WHEN t.name = N'xml' THEN CAST(0 AS int)
        WHEN t.name = N'sql_variant' THEN CAST(8000 AS int)
        ELSE CAST(NULL AS int) END AS int) AS odbc_char_octet_length,
    CAST(c.column_id AS int) AS ORDINAL_POSITION,
    CAST(CASE WHEN c.is_nullable = 1 THEN N'YES' ELSE N'NO' END AS varchar(254)) AS IS_NULLABLE,
    CAST(CASE
        WHEN t.name = N'int' AND c.is_nullable = 0 THEN CAST(56 AS tinyint)
        WHEN t.name = N'int' THEN CAST(38 AS tinyint)
        WHEN t.name = N'bigint' THEN CAST(108 AS tinyint)
        WHEN t.name IN (N'decimal', N'numeric') THEN CAST(106 AS tinyint)
        WHEN t.name = N'nvarchar' THEN CAST(39 AS tinyint)
        WHEN t.name = N'varchar' THEN CAST(39 AS tinyint)
        WHEN t.name = N'varbinary' THEN CAST(37 AS tinyint)
        WHEN t.name = N'bit' THEN CAST(50 AS tinyint)
        WHEN t.name = N'uniqueidentifier' THEN CAST(37 AS tinyint)
        WHEN t.name = N'datetime2' THEN CAST(0 AS tinyint)
        WHEN t.name = N'smallint' AND c.is_nullable = 0 THEN CAST(52 AS tinyint)
        WHEN t.name = N'smallint' THEN CAST(38 AS tinyint)
        WHEN t.name = N'tinyint' AND c.is_nullable = 0 THEN CAST(48 AS tinyint)
        WHEN t.name = N'tinyint' THEN CAST(38 AS tinyint)
        WHEN t.name IN (N'char', N'nchar') AND c.is_nullable = 0 THEN CAST(47 AS tinyint)
        WHEN t.name IN (N'char', N'nchar') THEN CAST(39 AS tinyint)
        WHEN t.name = N'sql_variant' THEN CAST(39 AS tinyint)
        WHEN t.name = N'datetime' AND c.is_nullable = 0 THEN CAST(61 AS tinyint)
        WHEN t.name = N'datetime' THEN CAST(111 AS tinyint)
        WHEN t.name = N'smalldatetime' AND c.is_nullable = 0 THEN CAST(58 AS tinyint)
        WHEN t.name = N'smalldatetime' THEN CAST(111 AS tinyint)
        WHEN t.name IN (N'date', N'time', N'datetimeoffset', N'xml') THEN CAST(0 AS tinyint)
        WHEN t.name = N'float' AND c.is_nullable = 0 THEN CAST(62 AS tinyint)
        WHEN t.name = N'float' THEN CAST(109 AS tinyint)
        WHEN t.name = N'real' AND c.is_nullable = 0 THEN CAST(59 AS tinyint)
        WHEN t.name = N'real' THEN CAST(109 AS tinyint)
        WHEN t.name = N'money' AND c.is_nullable = 0 THEN CAST(60 AS tinyint)
        WHEN t.name = N'money' THEN CAST(110 AS tinyint)
        WHEN t.name = N'smallmoney' AND c.is_nullable = 0 THEN CAST(122 AS tinyint)
        WHEN t.name = N'smallmoney' THEN CAST(110 AS tinyint)
        WHEN t.name = N'binary' AND c.is_nullable = 0 THEN CAST(45 AS tinyint)
        WHEN t.name = N'binary' THEN CAST(37 AS tinyint)
        WHEN t.name = N'image' THEN CAST(34 AS tinyint)
        WHEN t.name IN (N'text', N'ntext') THEN CAST(35 AS tinyint)
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
    c.object_id AS object_id
FROM sys.all_columns c
JOIN sys.types t ON t.user_type_id = c.user_type_id
WHERE c.is_computed = 0
  AND c.object_id IN (SELECT o.object_id FROM sys.all_objects o WHERE o.type IN (N'U', N'V'))
    ) col_src
    WHERE (@table_id IS NULL OR col_src.object_id = @table_id)
      AND (@table_name IS NULL OR col_src.TABLE_NAME LIKE @table_name)
      AND (@table_owner IS NULL OR col_src.TABLE_OWNER LIKE @table_owner)
      AND (@column_name IS NULL OR col_src.COLUMN_NAME LIKE @column_name)
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
    fn a_null_of_another_type_binds_to_text_int_and_bit_parameters() {
        let null = Value::Null;
        let mismatched = int_ty();
        let nv = nvarchar_ty();
        let name = nvarchar("t");
        let action = resolve(
            "sp_columns_100",
            &[
                arg(None, &nv, &name),
                arg(Some("@table_qualifier"), &mismatched, &null),
                arg(Some("@NameScope"), &nv, &null),
                arg(Some("@fUsePattern"), &nv, &null),
            ],
        )
        .unwrap()
        .unwrap();
        let ProcAction::Template { params, .. } = action else {
            panic!("expected Template");
        };
        for (index, name) in [
            (2, "@table_qualifier"),
            (5, "@NameScope"),
            (6, "@fUsePattern"),
        ] {
            assert_eq!(params[index].name, name);
            assert_eq!(params[index].value, Value::Null, "{name}");
        }
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
