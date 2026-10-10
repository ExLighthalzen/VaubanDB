//! `sp_stored_procedures`: lists the procedures visible in the current database.

use vauban_errors::SqlResult;
use vauban_session::{CallForm, ParamDefault, ProcSignature, SignatureParam, bind_arguments};
use vauban_types::{Len, SqlType};

use crate::procedures::{ProcAction, SystemProc};
use crate::special_procs::ProcArg;

const SP_STORED_PROCEDURES_PARAMS: &[SignatureParam<'static>] = &[
    SignatureParam::new("@sp_name", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@sp_owner", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@sp_qualifier", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@fUsePattern", SqlType::Bit).with_default(ParamDefault::Bit(true)),
];

pub(crate) const PROCS: &[SystemProc] = &[SystemProc {
    name: "sp_stored_procedures",
}];

/// Resolves `sp_stored_procedures`, the single procedure listed in [`PROCS`].
pub(crate) fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    if !PROCS.iter().any(|proc| proc.name == name) {
        return None;
    }
    let signature = ProcSignature {
        procedure: "sp_stored_procedures",
        params: SP_STORED_PROCEDURES_PARAMS,
    };
    let params = match bind_arguments(&signature, CallForm::Procedure, args) {
        Ok(params) => params,
        Err(err) => return Some(Err(err)),
    };
    Some(Ok(ProcAction::Template {
        sql: SP_STORED_PROCEDURES.to_owned(),
        params,
    }))
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
