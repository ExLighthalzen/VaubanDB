//! `sp_describe_first_result_set` and `sp_describe_undeclared_parameters`: the shape of a
//! T-SQL batch, without running it.
//!
//! Both take the batch as `[ @tsql = ] N'…'`, an ordinary T-SQL text and not the name of a
//! procedure. A driver calls the first one behind `PreparedStatement.getMetaData()` and the
//! second one behind `getParameterMetaData()`.
//!
//! This file owns the names, the argument binding and the fixed shape of the two result
//! sets; the session fills the rows from the statement it binds.
//!
//! # Result-set shapes
//!
//! `sp_describe_first_result_set` answers one row per column of the first result set of the
//! batch, `sp_describe_undeclared_parameters` one row per parameter the batch uses without
//! declaring it. Their column lists are fixed and are the two tables below, `sysname` being
//! `nvarchar(128)`.
//!
//! # Arguments
//!
//! An argument reaches its slot by its position in the call; a `@name =` written in front of
//! it leaves that position unchanged (`tests/describe_procs.rs`,
//! `a_name_does_not_move_its_argument`).
//! The first slot is the batch, and it is read from a Unicode string: another type, `NULL`
//! included, is refused (`tests/describe_procs.rs`, `a_tsql_that_is_not_unicode_text_is_214`).
//!
//! `@params` and `@browse_information_mode` are bound and refused rather than ignored: a
//! caller that asks for a declaration list or for browse information would otherwise read an
//! answer that does not hold it. Their neutral values (`NULL`, the empty declaration list,
//! browse mode `0`) are accepted, because a driver sends them to reach the argument after.

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::procedures::ProcAction;
use crate::sp_datatype_info::ResultColumn;
use crate::special_procs::ProcArg;

/// Which of the two descriptions a call asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeKind {
    /// Columns of the first result set of the batch.
    FirstResultSet,
    /// Parameters the batch uses without declaring them.
    UndeclaredParameters,
}

/// Name of the procedure that describes the first result set.
const FIRST_RESULT_SET: &str = "sp_describe_first_result_set";
/// Name of the procedure that describes the undeclared parameters.
const UNDECLARED_PARAMETERS: &str = "sp_describe_undeclared_parameters";

/// Slots of `sp_describe_first_result_set`, in call order: the batch, the declaration list,
/// then the browse mode.
const FIRST_RESULT_SET_ARITY: usize = 3;
/// Slots of `sp_describe_undeclared_parameters`, in call order: the batch, then the
/// declaration list.
const UNDECLARED_PARAMETERS_ARITY: usize = 2;

/// Type name reported when `@tsql` is missing or not a string.
const TSQL_TYPE: &str = "nvarchar(max)";

/// State of error 201 on the path where the call is missing `@tsql`.
const MISSING_TSQL_STATE: u8 = 20;
/// State of error 214 on the path where `@tsql` is `NULL` or not a string.
const WRONG_TSQL_TYPE_STATE: u8 = 21;
/// State of error 8144 on the path where the call carries an argument the procedure does
/// not serve.
const EXTRA_ARGUMENT_STATE: u8 = 23;

/// Error 201 with the state of the missing-`@tsql` path.
fn missing_tsql(proc_name: &str) -> SqlError {
    let mut err = SqlError::procedure_expects_parameter(proc_name, "@tsql");
    err.state = MISSING_TSQL_STATE;
    err
}

/// Error 214 with the state of the wrong-`@tsql`-type path.
fn wrong_tsql_type() -> SqlError {
    let mut err = SqlError::procedure_expects_type("@tsql", TSQL_TYPE);
    err.state = WRONG_TSQL_TYPE_STATE;
    err
}

/// Error 8144 with the state of the extra-argument path.
fn extra_argument(proc_name: &str) -> SqlError {
    let mut err = SqlError::too_many_arguments(proc_name);
    err.state = EXTRA_ARGUMENT_STATE;
    err
}

/// One column of a describe result set: name, type, and whether it may be `NULL`.
struct Column(&'static str, ColumnType, bool);

/// The column types the two result sets use.
#[derive(Clone, Copy)]
enum ColumnType {
    /// `bit`.
    Bit,
    /// `tinyint`.
    TinyInt,
    /// `smallint`.
    SmallInt,
    /// `int`.
    Int,
    /// `sysname`, that is `nvarchar(128)`.
    SysName,
    /// `nvarchar(4000)`.
    LongText,
}

impl ColumnType {
    /// The type as the session sends it in COLMETADATA.
    fn type_info(self, nullable: bool) -> TypeInfo {
        let ty = match self {
            ColumnType::Bit => SqlType::Bit,
            ColumnType::TinyInt => SqlType::TinyInt,
            ColumnType::SmallInt => SqlType::SmallInt,
            ColumnType::Int => SqlType::Int,
            ColumnType::SysName => SqlType::NVarChar(Len::Fixed(128)),
            ColumnType::LongText => SqlType::NVarChar(Len::Fixed(4_000)),
        };
        TypeInfo::new(ty, nullable)
    }
}

use ColumnType::{Bit, Int, LongText, SmallInt, SysName, TinyInt};

/// Columns of `sp_describe_first_result_set`, in the order the caller receives them.
const FIRST_RESULT_SET_COLUMNS: &[Column] = &[
    Column("is_hidden", Bit, false),
    Column("column_ordinal", Int, false),
    Column("name", SysName, true),
    Column("is_nullable", Bit, false),
    Column("system_type_id", Int, false),
    Column("system_type_name", SysName, true),
    Column("max_length", SmallInt, false),
    Column("precision", TinyInt, false),
    Column("scale", TinyInt, false),
    Column("collation_name", SysName, true),
    Column("user_type_id", Int, true),
    Column("user_type_database", SysName, true),
    Column("user_type_schema", SysName, true),
    Column("user_type_name", SysName, true),
    Column("assembly_qualified_type_name", LongText, true),
    Column("xml_collection_id", Int, true),
    Column("xml_collection_database", SysName, true),
    Column("xml_collection_schema", SysName, true),
    Column("xml_collection_name", SysName, true),
    Column("is_xml_document", Bit, false),
    Column("is_case_sensitive", Bit, false),
    Column("is_fixed_length_clr_type", Bit, false),
    Column("source_server", SysName, true),
    Column("source_database", SysName, true),
    Column("source_schema", SysName, true),
    Column("source_table", SysName, true),
    Column("source_column", SysName, true),
    Column("is_identity_column", Bit, true),
    Column("is_part_of_unique_key", Bit, true),
    Column("is_updateable", Bit, true),
    Column("is_computed_column", Bit, true),
    Column("is_sparse_column_set", Bit, true),
    Column("ordinal_in_order_by_list", SmallInt, true),
    Column("order_by_is_descending", Bit, true),
    Column("order_by_list_length", SmallInt, true),
    Column("tds_type_id", Int, false),
    Column("tds_length", Int, false),
    Column("tds_collation_id", Int, true),
    Column("tds_collation_sort_id", TinyInt, true),
];

/// Columns of `sp_describe_undeclared_parameters`, in the order the caller receives them.
const UNDECLARED_PARAMETERS_COLUMNS: &[Column] = &[
    Column("parameter_ordinal", Int, false),
    Column("name", SysName, false),
    Column("suggested_system_type_id", Int, false),
    Column("suggested_system_type_name", SysName, true),
    Column("suggested_max_length", SmallInt, false),
    Column("suggested_precision", TinyInt, false),
    Column("suggested_scale", TinyInt, false),
    Column("suggested_user_type_id", Int, true),
    Column("suggested_user_type_database", SysName, true),
    Column("suggested_user_type_schema", SysName, true),
    Column("suggested_user_type_name", SysName, true),
    Column("suggested_assembly_qualified_type_name", LongText, true),
    Column("suggested_xml_collection_id", Int, true),
    Column("suggested_xml_collection_database", SysName, true),
    Column("suggested_xml_collection_schema", SysName, true),
    Column("suggested_xml_collection_name", SysName, true),
    Column("suggested_is_xml_document", Bit, false),
    Column("suggested_is_case_sensitive", Bit, false),
    Column("suggested_is_fixed_length_clr_type", Bit, false),
    Column("suggested_is_input", Bit, false),
    Column("suggested_is_output", Bit, false),
    Column("formal_parameter_name", SysName, true),
    Column("suggested_tds_type_id", Int, false),
    Column("suggested_tds_length", Int, false),
];

/// The result-set metadata of `kind`, in column order.
pub fn describe_columns(kind: DescribeKind) -> Vec<ResultColumn> {
    let columns = match kind {
        DescribeKind::FirstResultSet => FIRST_RESULT_SET_COLUMNS,
        DescribeKind::UndeclaredParameters => UNDECLARED_PARAMETERS_COLUMNS,
    };
    columns
        .iter()
        .map(|Column(name, ty, nullable)| ResultColumn {
            name: (*name).to_owned(),
            ty: ty.type_info(*nullable),
        })
        .collect()
}

/// Resolves a call to one of the two describe procedures, or `None` for another name.
pub fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<SqlResult<ProcAction>> {
    let kind = match name {
        FIRST_RESULT_SET => DescribeKind::FirstResultSet,
        UNDECLARED_PARAMETERS => DescribeKind::UndeclaredParameters,
        _ => return None,
    };
    Some(resolve_describe(name, kind, args))
}

fn resolve_describe(
    proc_name: &str,
    kind: DescribeKind,
    args: &[ProcArg<'_>],
) -> SqlResult<ProcAction> {
    let slots = match kind {
        DescribeKind::FirstResultSet => FIRST_RESULT_SET_ARITY,
        DescribeKind::UndeclaredParameters => UNDECLARED_PARAMETERS_ARITY,
    };
    let bound = bind_arguments(proc_name, args, slots)?;

    let statement = match bound[0] {
        None => return Err(missing_tsql(proc_name)),
        Some(arg) => match (arg.value, is_unicode_text(arg.ty)) {
            (Value::String(text), true) => text.text.clone(),
            _ => return Err(wrong_tsql_type()),
        },
    };

    if bound[1].is_some_and(|arg| declares_parameters(arg.value)) {
        return Err(extra_argument(proc_name));
    }
    let browse = bound.get(2).copied().flatten();
    if browse.is_some_and(|arg| browse_information_asked(arg.value)) {
        return Err(extra_argument(proc_name));
    }

    Ok(ProcAction::Describe {
        statement,
        kind,
        columns: describe_columns(kind),
    })
}

/// `true` when `@params` carries a declaration list rather than its neutral value.
fn declares_parameters(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.text.trim().is_empty(),
        _ => true,
    }
}

/// `true` when `@browse_information_mode` asks for browse information.
fn browse_information_asked(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::I8(mode) => *mode != 0,
        Value::I16(mode) => *mode != 0,
        Value::I32(mode) => *mode != 0,
        Value::I64(mode) => *mode != 0,
        _ => true,
    }
}

/// Places each argument in the slot of its position in the call. The `@name =` an argument
/// carries selects nothing: the first argument fills the first slot whichever name it was
/// written with, and a batch passed as `@params = N'…'` lands in the slot of the batch
/// (`tests/describe_procs.rs`, `a_name_does_not_move_its_argument`). An argument past the
/// last slot is refused rather than dropped; an argument written `DEFAULT` leaves its slot
/// empty.
fn bind_arguments<'a, 'b>(
    proc_name: &str,
    args: &'b [ProcArg<'a>],
    slots: usize,
) -> SqlResult<Vec<Option<&'b ProcArg<'a>>>> {
    if args.len() > slots {
        return Err(extra_argument(proc_name));
    }
    let mut bound: Vec<Option<&ProcArg<'_>>> = vec![None; slots];
    for (index, arg) in args.iter().enumerate() {
        if !arg.default {
            bound[index] = Some(arg);
        }
    }
    Ok(bound)
}

/// Whether a type carries Unicode text, which is what the batch slot is read from.
fn is_unicode_text(ty: &TypeInfo) -> bool {
    matches!(ty.ty, SqlType::NVarChar(_) | SqlType::NChar(_))
}
