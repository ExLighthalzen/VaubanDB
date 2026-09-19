//! Help procedures (`sp_help`, `sp_helpdb`, …).

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::procedures::{ProcAction, ProcParam, SystemProc};
use crate::special_procs::ProcArg;

pub(crate) const PROCS: &[SystemProc] = &[
    SystemProc { name: "sp_help" },
    SystemProc { name: "sp_helpdb" },
    SystemProc {
        name: "sp_spaceused",
    },
];

/// Resolves a help procedure implemented in this module.
pub(crate) fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    match name {
        "sp_help" => Some(resolve_help(args)),
        "sp_helpdb" => Some(resolve_helpdb(args)),
        "sp_spaceused" => Some(resolve_spaceused(args)),
        _ => None,
    }
}

fn resolve_help(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let params = bind_objname("sp_help", args, "@objname")?;
    Ok(ProcAction::Template {
        sql: SP_HELP.to_owned(),
        params,
    })
}

fn resolve_helpdb(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let params = bind_dbname("sp_helpdb", args)?;
    Ok(ProcAction::Template {
        sql: SP_HELPDB.to_owned(),
        params,
    })
}

fn resolve_spaceused(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let params = bind_spaceused(args)?;
    Ok(ProcAction::Template {
        sql: SP_SPACEUSED.to_owned(),
        params,
    })
}

fn bind_objname(proc: &str, args: &[ProcArg<'_>], canonical: &str) -> SqlResult<Vec<ProcParam>> {
    let ty = TypeInfo::new(SqlType::NVarChar(Len::Fixed(776)), true);
    let mut value = Value::Null;
    let mut positional = 0usize;
    let mut saw_named = false;

    for (index, arg) in args.iter().enumerate() {
        if let Some(name) = arg.name {
            saw_named = true;
            if name.eq_ignore_ascii_case(canonical) {
                if value != Value::Null {
                    return Err(SqlError::too_many_arguments(proc));
                }
                value = bind_unicode(arg)?;
            } else {
                return Err(SqlError::not_a_parameter(name, proc));
            }
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        } else if positional == 0 {
            value = bind_unicode(arg)?;
            positional += 1;
        } else {
            return Err(SqlError::too_many_arguments(proc));
        }
    }

    Ok(vec![ProcParam {
        name: canonical.to_owned(),
        ty,
        value,
        output: false,
    }])
}

fn bind_dbname(proc: &str, args: &[ProcArg<'_>]) -> SqlResult<Vec<ProcParam>> {
    let ty = TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true);
    let mut dbname = Value::Null;
    let mut positional = 0usize;
    let mut saw_named = false;

    for (index, arg) in args.iter().enumerate() {
        if let Some(name) = arg.name {
            saw_named = true;
            if name.eq_ignore_ascii_case("@dbname") {
                if dbname != Value::Null {
                    return Err(SqlError::too_many_arguments(proc));
                }
                dbname = bind_unicode(arg)?;
            } else {
                return Err(SqlError::not_a_parameter(name, proc));
            }
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        } else if positional == 0 {
            dbname = bind_unicode(arg)?;
            positional += 1;
        } else {
            return Err(SqlError::too_many_arguments(proc));
        }
    }

    if dbname == Value::Null {
        return Err(SqlError::procedure_expects_parameter(proc, "@dbname"));
    }

    Ok(vec![ProcParam {
        name: "@dbname".to_owned(),
        ty,
        value: dbname,
        output: false,
    }])
}

fn bind_spaceused(args: &[ProcArg<'_>]) -> SqlResult<Vec<ProcParam>> {
    let obj_ty = TypeInfo::new(SqlType::NVarChar(Len::Fixed(776)), true);
    let update_ty = TypeInfo::new(SqlType::VarChar(Len::Fixed(5)), true);
    let mut objname = Value::Null;
    let mut updateusage = Value::Null;
    let mut positional = 0usize;
    let mut saw_named = false;

    for (index, arg) in args.iter().enumerate() {
        if let Some(name) = arg.name {
            saw_named = true;
            if name.eq_ignore_ascii_case("@objname") {
                if objname != Value::Null {
                    return Err(SqlError::too_many_arguments("sp_spaceused"));
                }
                objname = bind_unicode(arg)?;
            } else if name.eq_ignore_ascii_case("@updateusage") {
                if updateusage != Value::Null {
                    return Err(SqlError::too_many_arguments("sp_spaceused"));
                }
                updateusage = arg.value.clone();
            } else {
                return Err(SqlError::not_a_parameter(name, "sp_spaceused"));
            }
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        } else if positional == 0 {
            objname = bind_unicode(arg)?;
            positional += 1;
        } else {
            return Err(SqlError::too_many_arguments("sp_spaceused"));
        }
    }

    Ok(vec![
        ProcParam {
            name: "@objname".to_owned(),
            ty: obj_ty,
            value: objname,
            output: false,
        },
        ProcParam {
            name: "@updateusage".to_owned(),
            ty: update_ty,
            value: updateusage,
            output: false,
        },
    ])
}

fn bind_unicode(arg: &ProcArg<'_>) -> SqlResult<Value> {
    if !matches!(arg.ty.ty, SqlType::NVarChar(_) | SqlType::NChar(_)) {
        return Err(SqlError::procedure_expects_type(
            arg.name.unwrap_or("@objname"),
            "ntext/nchar/nvarchar",
        ));
    }
    Ok(arg.value.clone())
}

const SP_HELP: &str = r#####"SET NOCOUNT ON;

IF @objname IS NULL
BEGIN
    SELECT
        Name = CAST(o.name AS nvarchar(128)),
        Owner = CAST(USER_NAME(s.principal_id) AS nvarchar(128)),
        Object_type = CAST(
            CASE o.type
                WHEN N'U ' THEN N'user table'
                WHEN N'V ' THEN N'view'
                WHEN N'P ' THEN N'stored procedure'
                WHEN N'PK' THEN N'primary key cns'
                WHEN N'F ' THEN N'foreign key cns'
                WHEN N'D ' THEN N'default cns'
                WHEN N'UQ' THEN N'unique cns'
                ELSE o.type
            END AS nvarchar(31))
    FROM sys.all_objects o
    INNER JOIN sys.schemas s ON o.schema_id = s.schema_id
    ORDER BY o.name;

    SELECT
        Type_name = CAST(NULL AS nvarchar(128)),
        Storage_type = CAST(NULL AS nvarchar(128)),
        Length = CAST(NULL AS smallint),
        Prec = CAST(NULL AS int),
        Scale = CAST(NULL AS int),
        Nullable = CAST(NULL AS varchar(35)),
        Default_name = CAST(NULL AS nvarchar(128)),
        Rule_name = CAST(NULL AS nvarchar(128)),
        Collation = CAST(NULL AS nvarchar(128))
    WHERE 1 = 0;

    RETURN;
END;

SELECT
    Name = CAST(o.name AS nvarchar(128)),
    Owner = CAST(USER_NAME(s.principal_id) AS nvarchar(128)),
    Type = CAST(
        CASE o.type
            WHEN N'U ' THEN N'user table'
            WHEN N'V ' THEN N'view'
            WHEN N'P ' THEN N'stored procedure'
            ELSE o.type
        END AS nvarchar(31)),
    Created_datetime = o.create_date
FROM sys.all_objects o
INNER JOIN sys.schemas s ON o.schema_id = s.schema_id
WHERE o.object_id = OBJECT_ID(@objname);

    SELECT
        Column_name = CAST(c.name AS nvarchar(128)),
        Type = CAST(
            CASE WHEN c.is_computed = 1 THEN N'numeric' ELSE t.name END AS nvarchar(128)),
        Computed = CAST(CASE WHEN c.is_computed = 1 THEN N'yes' ELSE N'no' END AS varchar(35)),
        Length = CAST(c.max_length AS int),
        Prec = CAST(
            CASE
                WHEN c.is_computed = 1
                    THEN LEFT(CAST(13 AS varchar(5)) + REPLICATE(N' ', 5), 5)
                WHEN t.name IN (N'decimal', N'numeric', N'float', N'real', N'datetime', N'datetime2', N'time', N'datetimeoffset')
                    THEN LEFT(CAST(c.[precision] AS varchar(5)) + REPLICATE(N' ', 5), 5)
                WHEN t.name IN (N'int', N'bigint', N'smallint', N'tinyint', N'money', N'smallmoney', N'bit')
                    THEN LEFT(CAST(
                        CASE t.name
                            WHEN N'int' THEN 10
                            WHEN N'bigint' THEN 19
                            WHEN N'smallint' THEN 5
                            WHEN N'tinyint' THEN 3
                            WHEN N'money' THEN 19
                            WHEN N'smallmoney' THEN 10
                            WHEN N'bit' THEN 1
                        END AS varchar(5)) + REPLICATE(N' ', 5), 5)
                ELSE REPLICATE(N' ', 5)
            END AS char(5)),
        Scale = CAST(
            CASE
                WHEN c.is_computed = 1
                    THEN LEFT(CAST(4 AS varchar(5)) + REPLICATE(N' ', 5), 5)
                WHEN t.name IN (N'decimal', N'numeric', N'datetime', N'datetime2', N'time', N'datetimeoffset', N'money', N'smallmoney')
                    THEN LEFT(CAST(c.scale AS varchar(5)) + REPLICATE(N' ', 5), 5)
                WHEN t.name IN (N'int', N'bigint', N'smallint', N'tinyint', N'float', N'real', N'bit')
                    THEN LEFT(CAST(0 AS varchar(5)) + REPLICATE(N' ', 5), 5)
                ELSE REPLICATE(N' ', 5)
            END AS char(5)),
        Nullable = CAST(CASE WHEN c.is_nullable = 1 THEN N'yes' ELSE N'no' END AS varchar(35)),
        TrimTrailingBlanks = CAST(
            CASE WHEN t.name IN (N'char', N'varchar') THEN N'no' ELSE N'(n/a)' END AS varchar(35)),
        FixedLenNullInSource = CAST(
            CASE WHEN t.name IN (N'char', N'varchar') THEN N'no' ELSE N'(n/a)' END AS varchar(35)),
        Collation = CAST(c.collation_name AS nvarchar(128))
    FROM sys.all_columns c
    INNER JOIN sys.types t ON c.user_type_id = t.user_type_id
    WHERE c.object_id = OBJECT_ID(@objname)
    ORDER BY c.column_id;

    SELECT
        [Identity] = CAST(COALESCE(MAX(ic.name), N'No identity column defined.') AS nvarchar(128)),
        Seed = MAX(ic.seed_value),
        Increment = MAX(ic.increment_value),
        [Not For Replication] = MAX(CASE WHEN ic.object_id IS NULL THEN CAST(NULL AS int) ELSE CAST(0 AS int) END)
    FROM sys.identity_columns ic
    WHERE ic.object_id = OBJECT_ID(@objname);

    SELECT [RowGuidCol] = CAST(N'No rowguidcol column defined.' AS nvarchar(128));

    SELECT Data_located_on_filegroup = CAST(N'PRIMARY' AS nvarchar(128));

    SELECT
        index_name = CAST(i.name AS nvarchar(128)),
        index_description = CAST(
            CASE
                WHEN i.is_primary_key = 1 THEN N'clustered, unique, primary key located on PRIMARY'
                WHEN i.is_unique = 1 THEN N'nonclustered, unique located on PRIMARY'
                ELSE N'nonclustered located on PRIMARY'
            END AS varchar(210)),
        index_keys = CAST(
            (
                SELECT
                    ISNULL(MAX(CASE WHEN ic.key_ordinal = 1 THEN c.name END), N'') +
                    CASE
                        WHEN MAX(CASE WHEN ic.key_ordinal = 2 THEN c.name END) IS NOT NULL
                            THEN N', ' + MAX(CASE WHEN ic.key_ordinal = 2 THEN c.name END)
                        ELSE N''
                    END +
                    CASE
                        WHEN MAX(CASE WHEN ic.key_ordinal = 3 THEN c.name END) IS NOT NULL
                            THEN N', ' + MAX(CASE WHEN ic.key_ordinal = 3 THEN c.name END)
                        ELSE N''
                    END
                FROM sys.index_columns ic
                INNER JOIN sys.all_columns c
                    ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                WHERE ic.object_id = i.object_id
                  AND ic.index_id = i.index_id
                  AND ic.key_ordinal > 0
            ) AS nvarchar(2078))
    FROM sys.indexes i
    WHERE i.object_id = OBJECT_ID(@objname)
      AND i.index_id > 0
    ORDER BY i.name;

    SELECT
        constraint_type = CAST(src.constraint_type AS nvarchar(146)),
        constraint_name = CAST(src.constraint_name AS nvarchar(128)),
        delete_action = CAST(src.delete_action AS varchar(9)),
        update_action = CAST(src.update_action AS varchar(9)),
        status_enabled = CAST(src.status_enabled AS varchar(8)),
        status_for_replication = CAST(src.status_for_replication AS varchar(19)),
        constraint_keys = CAST(src.constraint_keys AS nvarchar(2078))
    FROM (
        SELECT
            constraint_type = N'DEFAULT on column ' + c.name,
            constraint_name = dc.name,
            delete_action = N'(n/a)',
            update_action = N'(n/a)',
            status_enabled = N'(n/a)',
            status_for_replication = N'(n/a)',
            constraint_keys = dc.definition,
            sort_key = dc.name
        FROM sys.default_constraints dc
        INNER JOIN sys.all_columns c
            ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id
        WHERE dc.parent_object_id = OBJECT_ID(@objname)
        UNION ALL
        SELECT
            constraint_type = N'PRIMARY KEY (clustered)',
            constraint_name = kc.name,
            delete_action = N'(n/a)',
            update_action = N'(n/a)',
            status_enabled = N'(n/a)',
            status_for_replication = N'(n/a)',
            constraint_keys = (
                SELECT
                    ISNULL(MAX(CASE WHEN ic.key_ordinal = 1 THEN c.name END), N'') +
                    CASE
                        WHEN MAX(CASE WHEN ic.key_ordinal = 2 THEN c.name END) IS NOT NULL
                            THEN N', ' + MAX(CASE WHEN ic.key_ordinal = 2 THEN c.name END)
                        ELSE N''
                    END +
                    CASE
                        WHEN MAX(CASE WHEN ic.key_ordinal = 3 THEN c.name END) IS NOT NULL
                            THEN N', ' + MAX(CASE WHEN ic.key_ordinal = 3 THEN c.name END)
                        ELSE N''
                    END
                FROM sys.indexes i
                INNER JOIN sys.index_columns ic
                    ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                INNER JOIN sys.all_columns c
                    ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                WHERE i.object_id = kc.parent_object_id
                  AND i.is_primary_key = 1
                  AND ic.key_ordinal > 0
            ),
            sort_key = kc.name
        FROM sys.key_constraints kc
        WHERE kc.parent_object_id = OBJECT_ID(@objname)
          AND kc.type = N'PK'
        UNION ALL
        SELECT
            constraint_type = N'FOREIGN KEY',
            constraint_name = fk.name,
            delete_action = CAST(
                CASE fk.delete_referential_action_desc
                    WHEN N'NO_ACTION' THEN N'No Action'
                    WHEN N'CASCADE' THEN N'Cascade'
                    WHEN N'SET_NULL' THEN N'Set Null'
                    WHEN N'SET_DEFAULT' THEN N'Set Default'
                    ELSE fk.delete_referential_action_desc
                END AS varchar(9)),
            update_action = CAST(
                CASE fk.update_referential_action_desc
                    WHEN N'NO_ACTION' THEN N'No Action'
                    WHEN N'CASCADE' THEN N'Cascade'
                    WHEN N'SET_NULL' THEN N'Set Null'
                    WHEN N'SET_DEFAULT' THEN N'Set Default'
                    ELSE fk.update_referential_action_desc
                END AS varchar(9)),
            status_enabled = N'Enabled',
            status_for_replication = N'Is_For_Replication',
            constraint_keys = (
                SELECT
                    ISNULL(MAX(CASE WHEN fkc.constraint_column_id = 1 THEN c.name END), N'') +
                    CASE
                        WHEN MAX(CASE WHEN fkc.constraint_column_id = 2 THEN c.name END) IS NOT NULL
                            THEN N', ' + MAX(CASE WHEN fkc.constraint_column_id = 2 THEN c.name END)
                        ELSE N''
                    END
                FROM sys.foreign_key_columns fkc
                INNER JOIN sys.all_columns c
                    ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id
                WHERE fkc.constraint_object_id = fk.object_id
            ),
            sort_key = fk.name
        FROM sys.foreign_keys fk
        WHERE fk.parent_object_id = OBJECT_ID(@objname)
        UNION ALL
        SELECT
            constraint_type = N' ',
            constraint_name = N' ',
            delete_action = N' ',
            update_action = N' ',
            status_enabled = N' ',
            status_for_replication = N' ',
            constraint_keys = N'REFERENCES ' + DB_NAME() + N'.' + SCHEMA_NAME(ref.schema_id) + N'.' + ref.name
                + N' (' + (
                    SELECT
                        ISNULL(MAX(CASE WHEN fkc.constraint_column_id = 1 THEN rc.name END), N'') +
                        CASE
                            WHEN MAX(CASE WHEN fkc.constraint_column_id = 2 THEN rc.name END) IS NOT NULL
                                THEN N', ' + MAX(CASE WHEN fkc.constraint_column_id = 2 THEN rc.name END)
                            ELSE N''
                        END
                    FROM sys.foreign_key_columns fkc
                    INNER JOIN sys.all_columns rc
                        ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
                    WHERE fkc.constraint_object_id = fk.object_id
                ) + N')',
            sort_key = fk.name + N'z'
        FROM sys.foreign_keys fk
        INNER JOIN sys.all_objects ref ON ref.object_id = fk.referenced_object_id
        WHERE fk.parent_object_id = OBJECT_ID(@objname)
    ) AS src
    ORDER BY src.sort_key;

    SELECT [Table is referenced by foreign key] = CAST(
        DB_NAME() + N'.' + SCHEMA_NAME(parent.schema_id) + N'.' + parent.name + N': ' + fk.name
        AS nvarchar(516))
    FROM sys.foreign_keys fk
    INNER JOIN sys.all_objects parent ON parent.object_id = fk.parent_object_id
    WHERE fk.referenced_object_id = OBJECT_ID(@objname)
    ORDER BY fk.name;"#####;

const SP_HELPDB: &str = r#####"SET NOCOUNT ON;

DECLARE @dbid int = DB_ID(@dbname);

SELECT
    name = CAST(d.name AS nvarchar(128)),
    db_size = CAST(
        LEFT(
            CAST(
                CAST(
                    (SELECT SUM(CAST(mf.size AS bigint)) * 8.0 / 1024.0
                     FROM sys.master_files mf
                     WHERE mf.database_id = d.database_id) AS decimal(10, 2)) AS varchar(20)),
            12) + N' MB' AS nvarchar(18)),
    owner = CAST(SUSER_SNAME(d.owner_sid) AS nvarchar(128)),
    dbid = CAST(d.database_id AS smallint),
    created = CAST(CONVERT(varchar(11), d.create_date, 106) AS nvarchar(11)),
    status = CAST(
        N'Status=ONLINE, Updateability=READ_WRITE, UserAccess=MULTI_USER, Recovery=' +
        CASE d.recovery_model_desc
            WHEN N'FULL' THEN N'FULL'
            WHEN N'SIMPLE' THEN N'SIMPLE'
            ELSE d.recovery_model_desc
        END +
        N', Version=957, Collation=' + CAST(d.collation_name AS nvarchar(128)) +
        N', SQLSortOrder=52, IsAutoCreateStatistics, IsAutoUpdateStatistics' AS nvarchar(4000)),
    compatibility_level = CAST(d.compatibility_level AS tinyint)
FROM sys.databases d
WHERE d.name = @dbname;

SELECT
    name = CAST(mf.name AS nvarchar(128)),
    fileid = CAST(mf.file_id AS smallint),
    filename = CAST(mf.physical_name AS nvarchar(260)),
    filegroup = CAST(
        CASE WHEN mf.type_desc = N'LOG' THEN CAST(NULL AS nvarchar(128)) ELSE N'PRIMARY' END
        AS nvarchar(128)),
    size = CAST(
        LEFT(CAST(CAST(mf.size * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20) AS nvarchar(20)),
    maxsize = CAST(N'Unlimited' AS nvarchar(20)),
    growth = CAST(N'10%' AS nvarchar(20)),
    usage = CAST(
        CASE mf.type_desc WHEN N'LOG' THEN N'log only' ELSE N'data only' END AS varchar(14))
FROM sys.master_files mf
WHERE mf.database_id = @dbid
ORDER BY mf.file_id;"#####;

const SP_SPACEUSED: &str = r#####"SET NOCOUNT ON;

IF @objname IS NULL
BEGIN
    SELECT
        database_name = CAST(DB_NAME() AS nvarchar(128)),
        database_size = CAST(
            LEFT(
                CAST(
                    CAST(
                        (SELECT SUM(CAST(mf.size AS bigint)) * 8.0 / 1024.0
                         FROM sys.master_files mf
                         WHERE mf.database_id = DB_ID()) AS decimal(10, 2)) AS varchar(20)),
                10) + N' MB' AS varchar(18)),
        [unallocated space] = CAST(
            LEFT(
                CAST(
                    CAST(
                        (SELECT SUM(CAST(mf.size AS bigint)) * 8.0 / 1024.0
                         FROM sys.master_files mf
                         WHERE mf.database_id = DB_ID()) -
                        (SELECT SUM(CAST(au.total_pages AS bigint)) * 8.0 / 1024.0
                         FROM sys.allocation_units au
                         INNER JOIN sys.partitions p ON p.partition_id = au.container_id
                         WHERE p.index_id IN (0, 1)) AS decimal(10, 2)) AS varchar(20)),
                10) + N' MB' AS varchar(18))
    ;

    SELECT
        reserved = CAST(
            LEFT(CAST(CAST(ISNULL(SUM(au.total_pages), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
            AS varchar(18)),
        data = CAST(
            LEFT(CAST(CAST(ISNULL(SUM(CASE WHEN au.type IN (1, 3) THEN au.used_pages ELSE 0 END), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
            AS varchar(18)),
        index_size = CAST(
            LEFT(CAST(CAST(ISNULL(SUM(CASE WHEN au.type IN (2) THEN au.used_pages ELSE 0 END), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
            AS varchar(18)),
        unused = CAST(
            LEFT(CAST(CAST(ISNULL(SUM(au.total_pages - au.used_pages), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
            AS varchar(18))
    FROM sys.allocation_units au
    INNER JOIN sys.partitions p ON p.partition_id = au.container_id
    WHERE p.index_id IN (0, 1);

    RETURN;
END;

SELECT
    name = CAST(o.name AS nvarchar(128)),
    rows = CAST(
        LEFT(CAST(ISNULL(SUM(p.rows), 0) AS varchar(11)) + REPLICATE(N' ', 20), 20) AS char(20)),
    reserved = CAST(
        LEFT(CAST(CAST(ISNULL(SUM(au.total_pages), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
        AS varchar(18)),
    data = CAST(
        LEFT(CAST(CAST(ISNULL(SUM(CASE WHEN au.type IN (1, 3) THEN au.used_pages ELSE 0 END), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
        AS varchar(18)),
    index_size = CAST(
        LEFT(CAST(CAST(ISNULL(SUM(CASE WHEN au.type IN (2) THEN au.used_pages ELSE 0 END), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
        AS varchar(18)),
    unused = CAST(
        LEFT(CAST(CAST(ISNULL(SUM(au.total_pages - au.used_pages), 0) * 8.0 AS decimal(10, 0)) AS varchar(20)) + N' KB', 20)
        AS varchar(18))
FROM sys.all_objects o
LEFT JOIN sys.partitions p ON p.object_id = o.object_id
LEFT JOIN sys.allocation_units au ON au.container_id = p.partition_id
WHERE o.object_id = OBJECT_ID(@objname)
GROUP BY o.name;"#####;

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{SqlString, SqlType};

    fn nvarchar(value: &str) -> Value {
        Value::String(SqlString {
            text: value.to_owned(),
        })
    }

    fn nvarchar_ty() -> TypeInfo {
        TypeInfo::new(SqlType::NVarChar(Len::Max), true)
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
    fn sp_help_without_args_returns_template_with_null_objname() {
        let action = resolve("sp_help", &[]).unwrap().unwrap();
        match action {
            ProcAction::Template { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "@objname");
                assert_eq!(params[0].value, Value::Null);
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn sp_helpdb_requires_dbname() {
        let err = resolve("sp_helpdb", &[]).unwrap().unwrap_err();
        assert_eq!(err.number, 201);
    }

    #[test]
    fn sp_spaceused_accepts_named_updateusage() {
        let name = nvarchar("dbo.t");
        let nv = nvarchar_ty();
        let update = Value::String(SqlString {
            text: "true".to_owned(),
        });
        let update_ty = TypeInfo::new(SqlType::VarChar(Len::Fixed(5)), true);
        let action = resolve(
            "sp_spaceused",
            &[
                arg(Some("@objname"), &nv, &name),
                arg(Some("@updateusage"), &update_ty, &update),
            ],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(action, ProcAction::Template { .. }));
    }
}
