//! Session listing procedures (`sp_who`, `sp_who2`).

use vauban_errors::SqlResult;
use vauban_session::{CallForm, ProcSignature, SignatureParam, bind_arguments};
use vauban_types::{Len, SqlType};

use crate::procedures::{ProcAction, SystemProc};
use crate::special_procs::ProcArg;

const SP_WHO: &str = r"
SET NOCOUNT ON;
DECLARE @spidlow int = 0, @spidhigh int = 32767, @spid int;

IF @loginame IS NOT NULL AND @loginame IN (N'active', N'ACTIVE')
BEGIN
    SELECT
        CAST(s.session_id AS smallint) AS spid,
        CAST(0 AS smallint) AS ecid,
        CAST(LEFT(
            CASE s.status WHEN N'running' THEN N'runnable' ELSE s.status END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS status,
        s.login_name AS loginame,
        s.host_name AS hostname,
        CONVERT(char(5), ISNULL(r.blocking_session_id, 0)) AS blk,
        DB_NAME(CAST(s.database_id AS int)) AS dbname,
        CAST(LEFT(
            CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS cmd,
        CAST(ISNULL(r.request_id, 0) AS int) AS request_id
    FROM sys.dm_exec_sessions s
    LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
    WHERE s.session_id >= @spidlow
      AND s.session_id <= @spidhigh
      AND CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
          <> N'AWAITING COMMAND';
    RETURN;
END;

IF @loginame IS NOT NULL AND @loginame LIKE N'[0-9]%'
BEGIN
    SET @spid = CONVERT(int, @loginame);
    SELECT
        CAST(s.session_id AS smallint) AS spid,
        CAST(0 AS smallint) AS ecid,
        CAST(LEFT(
            CASE s.status WHEN N'running' THEN N'runnable' ELSE s.status END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS status,
        s.login_name AS loginame,
        s.host_name AS hostname,
        CONVERT(char(5), ISNULL(r.blocking_session_id, 0)) AS blk,
        DB_NAME(CAST(s.database_id AS int)) AS dbname,
        CAST(LEFT(
            CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS cmd,
        CAST(ISNULL(r.request_id, 0) AS int) AS request_id
    FROM sys.dm_exec_sessions s
    LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
    WHERE s.session_id = @spid;
    RETURN;
END;

IF @loginame IS NOT NULL
BEGIN
    SELECT
        CAST(s.session_id AS smallint) AS spid,
        CAST(0 AS smallint) AS ecid,
        CAST(LEFT(
            CASE s.status WHEN N'running' THEN N'runnable' ELSE s.status END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS status,
        s.login_name AS loginame,
        s.host_name AS hostname,
        CONVERT(char(5), ISNULL(r.blocking_session_id, 0)) AS blk,
        DB_NAME(CAST(s.database_id AS int)) AS dbname,
        CAST(LEFT(
            CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS cmd,
        CAST(ISNULL(r.request_id, 0) AS int) AS request_id
    FROM sys.dm_exec_sessions s
    LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
    WHERE s.login_name = @loginame;
    RETURN;
END;

SELECT
    CAST(s.session_id AS smallint) AS spid,
    CAST(0 AS smallint) AS ecid,
    CAST(LEFT(
        CASE s.status WHEN N'running' THEN N'runnable' ELSE s.status END
        + REPLICATE(N' ', 30), 30) AS varchar(30)) AS status,
    s.login_name AS loginame,
    s.host_name AS hostname,
    CONVERT(char(5), ISNULL(r.blocking_session_id, 0)) AS blk,
    DB_NAME(CAST(s.database_id AS int)) AS dbname,
    CAST(LEFT(
        CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
        + REPLICATE(N' ', 30), 30) AS varchar(30)) AS cmd,
    CAST(ISNULL(r.request_id, 0) AS int) AS request_id
FROM sys.dm_exec_sessions s
LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
WHERE s.session_id >= @spidlow AND s.session_id <= @spidhigh;
";

const SP_WHO2: &str = r"
SET NOCOUNT ON;
DECLARE @spidlow int = 0, @spidhigh int = 32767, @spid int;

IF @loginame IS NOT NULL AND @loginame IN (N'active', N'ACTIVE')
BEGIN
    SELECT
        LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
        CAST(LEFT(
            CASE s.status WHEN N'running' THEN N'RUNNABLE' WHEN N'sleeping' THEN N'SLEEPING' ELSE s.status END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS Status,
        s.login_name AS Login,
        CASE
            WHEN s.host_name IS NULL OR s.host_name = N'' THEN N'.'
            ELSE s.host_name
        END AS HostName,
        CASE
            WHEN ISNULL(r.blocking_session_id, 0) = 0 THEN N'  .'
            ELSE LEFT(CAST(r.blocking_session_id AS varchar(11)) + REPLICATE(N' ', 5), 5)
        END AS BlkBy,
        DB_NAME(CAST(s.database_id AS int)) AS DBName,
        CAST(LEFT(
            CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
            + REPLICATE(N' ', 16), 16) AS nvarchar(16)) AS Command,
        CAST(ISNULL(r.cpu_time, 0) AS varchar(10)) AS CPUTime,
        CAST(ISNULL(r.reads, 0) + ISNULL(r.writes, 0) AS varchar(10)) AS DiskIO,
        CAST(
            SUBSTRING(CONVERT(varchar(11), s.last_request_start_time, 111), 6, 5)
            + N' '
            + SUBSTRING(CONVERT(varchar(23), s.last_request_start_time, 113), 13, 8)
            AS varchar(20)) AS LastBatch,
        s.program_name AS ProgramName,
        LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
        LEFT(CAST(ISNULL(r.request_id, 0) AS varchar(11)) + REPLICATE(N' ', 5), 5) AS REQUESTID
    FROM sys.dm_exec_sessions s
    LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
    WHERE s.session_id >= @spidlow
      AND s.session_id <= @spidhigh
      AND CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
          <> N'AWAITING COMMAND';
    RETURN;
END;

IF @loginame IS NOT NULL AND @loginame LIKE N'[0-9]%'
BEGIN
    SET @spid = CONVERT(int, @loginame);
    SELECT
        LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
        CAST(LEFT(
            CASE s.status WHEN N'running' THEN N'RUNNABLE' WHEN N'sleeping' THEN N'SLEEPING' ELSE s.status END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS Status,
        s.login_name AS Login,
        CASE
            WHEN s.host_name IS NULL OR s.host_name = N'' THEN N'.'
            ELSE s.host_name
        END AS HostName,
        CASE
            WHEN ISNULL(r.blocking_session_id, 0) = 0 THEN N'  .'
            ELSE LEFT(CAST(r.blocking_session_id AS varchar(11)) + REPLICATE(N' ', 5), 5)
        END AS BlkBy,
        DB_NAME(CAST(s.database_id AS int)) AS DBName,
        CAST(LEFT(
            CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
            + REPLICATE(N' ', 16), 16) AS nvarchar(16)) AS Command,
        CAST(ISNULL(r.cpu_time, 0) AS varchar(10)) AS CPUTime,
        CAST(ISNULL(r.reads, 0) + ISNULL(r.writes, 0) AS varchar(10)) AS DiskIO,
        CAST(
            SUBSTRING(CONVERT(varchar(11), s.last_request_start_time, 111), 6, 5)
            + N' '
            + SUBSTRING(CONVERT(varchar(23), s.last_request_start_time, 113), 13, 8)
            AS varchar(20)) AS LastBatch,
        s.program_name AS ProgramName,
        LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
        LEFT(CAST(ISNULL(r.request_id, 0) AS varchar(11)) + REPLICATE(N' ', 5), 5) AS REQUESTID
    FROM sys.dm_exec_sessions s
    LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
    WHERE s.session_id = @spid;
    RETURN;
END;

IF @loginame IS NOT NULL
BEGIN
    SELECT
        LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
        CAST(LEFT(
            CASE s.status WHEN N'running' THEN N'RUNNABLE' WHEN N'sleeping' THEN N'SLEEPING' ELSE s.status END
            + REPLICATE(N' ', 30), 30) AS varchar(30)) AS Status,
        s.login_name AS Login,
        CASE
            WHEN s.host_name IS NULL OR s.host_name = N'' THEN N'.'
            ELSE s.host_name
        END AS HostName,
        CASE
            WHEN ISNULL(r.blocking_session_id, 0) = 0 THEN N'  .'
            ELSE LEFT(CAST(r.blocking_session_id AS varchar(11)) + REPLICATE(N' ', 5), 5)
        END AS BlkBy,
        DB_NAME(CAST(s.database_id AS int)) AS DBName,
        CAST(LEFT(
            CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
            + REPLICATE(N' ', 16), 16) AS nvarchar(16)) AS Command,
        CAST(ISNULL(r.cpu_time, 0) AS varchar(10)) AS CPUTime,
        CAST(ISNULL(r.reads, 0) + ISNULL(r.writes, 0) AS varchar(10)) AS DiskIO,
        CAST(
            SUBSTRING(CONVERT(varchar(11), s.last_request_start_time, 111), 6, 5)
            + N' '
            + SUBSTRING(CONVERT(varchar(23), s.last_request_start_time, 113), 13, 8)
            AS varchar(20)) AS LastBatch,
        s.program_name AS ProgramName,
        LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
        LEFT(CAST(ISNULL(r.request_id, 0) AS varchar(11)) + REPLICATE(N' ', 5), 5) AS REQUESTID
    FROM sys.dm_exec_sessions s
    LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
    WHERE s.login_name = @loginame;
    RETURN;
END;

SELECT
    LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
    CAST(LEFT(
        CASE s.status WHEN N'running' THEN N'RUNNABLE' WHEN N'sleeping' THEN N'SLEEPING' ELSE s.status END
        + REPLICATE(N' ', 30), 30) AS varchar(30)) AS Status,
    s.login_name AS Login,
    CASE
        WHEN s.host_name IS NULL OR s.host_name = N'' THEN N'.'
        ELSE s.host_name
    END AS HostName,
    CASE
        WHEN ISNULL(r.blocking_session_id, 0) = 0 THEN N'  .'
        ELSE LEFT(CAST(r.blocking_session_id AS varchar(11)) + REPLICATE(N' ', 5), 5)
    END AS BlkBy,
    DB_NAME(CAST(s.database_id AS int)) AS DBName,
    CAST(LEFT(
        CASE WHEN r.session_id IS NULL THEN N'AWAITING COMMAND' ELSE r.command END
        + REPLICATE(N' ', 16), 16) AS nvarchar(16)) AS Command,
    CAST(ISNULL(r.cpu_time, 0) AS varchar(10)) AS CPUTime,
    CAST(ISNULL(r.reads, 0) + ISNULL(r.writes, 0) AS varchar(10)) AS DiskIO,
    CAST(
        SUBSTRING(CONVERT(varchar(11), s.last_request_start_time, 111), 6, 5)
        + N' '
        + SUBSTRING(CONVERT(varchar(23), s.last_request_start_time, 113), 13, 8)
        AS varchar(20)) AS LastBatch,
    s.program_name AS ProgramName,
    LEFT(CAST(s.session_id AS varchar(11)) + REPLICATE(N' ', 5), 5) AS SPID,
    LEFT(CAST(ISNULL(r.request_id, 0) AS varchar(11)) + REPLICATE(N' ', 5), 5) AS REQUESTID
FROM sys.dm_exec_sessions s
LEFT JOIN sys.dm_exec_requests r ON s.session_id = r.session_id
WHERE s.session_id >= @spidlow AND s.session_id <= @spidhigh;
";

pub(crate) const PROCS: &[SystemProc] = &[
    SystemProc { name: "sp_who" },
    SystemProc { name: "sp_who2" },
];

/// Resolves a session-listing procedure implemented in this module.
pub(crate) fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    match name {
        "sp_who" => Some(resolve_who("sp_who", SP_WHO, args)),
        "sp_who2" => Some(resolve_who("sp_who2", SP_WHO2, args)),
        _ => None,
    }
}

/// The single parameter of `sp_who` and `sp_who2`.
const LOGINAME: &[SignatureParam<'static>] = &[SignatureParam::new(
    "@loginame",
    SqlType::NVarChar(Len::Max),
)];

fn resolve_who(procedure: &str, sql: &str, args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let signature = ProcSignature {
        procedure,
        params: LOGINAME,
    };
    let params = bind_arguments(&signature, CallForm::Procedure, args)?;
    Ok(ProcAction::Template {
        sql: sql.to_owned(),
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{SqlString, SqlType, TypeInfo, Value};

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
    fn sp_who_without_args_returns_template_with_null_loginame() {
        let action = resolve("sp_who", &[]).unwrap().unwrap();
        match action {
            ProcAction::Template { sql, params } => {
                assert!(sql.contains("sys.dm_exec_sessions"));
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "@loginame");
                assert_eq!(params[0].value, Value::Null);
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn sp_who2_rejects_unknown_parameter() {
        let value = nvarchar("sa");
        let ty = nvarchar_ty();
        let err = resolve("sp_who2", &[arg(Some("@nosuch"), &ty, &value)])
            .unwrap()
            .unwrap_err();
        assert_eq!(err.number, 8145);
    }

    #[test]
    fn sp_who_rejects_extra_positional_argument() {
        let first = nvarchar("52");
        let second = nvarchar("sa");
        let ty = nvarchar_ty();
        let err = resolve("sp_who", &[arg(None, &ty, &first), arg(None, &ty, &second)])
            .unwrap()
            .unwrap_err();
        assert_eq!(err.number, 8144);
    }
}
