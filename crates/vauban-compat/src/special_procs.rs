//! Binding arguments of `sp_executesql`, `sp_prepare`, `sp_execute` and related procedures.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{
    DataType, ParameterDeclaration, ParseOptions, TypeArg, parse_parameter_declarations,
};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::procedures::{ProcAction, ProcParam};

/// One argument of a system procedure call, as the session received it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcArg<'a> {
    /// Parameter name when the call used `@name = value`.
    pub name: Option<&'a str>,
    /// Declared type of the argument.
    pub ty: &'a TypeInfo,
    /// Argument value.
    pub value: &'a Value,
    /// True when the RPC marked the parameter as output.
    pub output: bool,
    /// True when the RPC marked the parameter as default.
    pub default: bool,
}

struct FixedParam {
    canonical: &'static str,
    aliases: &'static [&'static str],
    required: bool,
    unicode: bool,
    output: bool,
    int_handle: bool,
}

const SP_EXECUTESQL: &[FixedParam] = &[
    FixedParam {
        canonical: "@statement",
        aliases: &["@statement", "@stmt"],
        required: true,
        unicode: true,
        output: false,
        int_handle: false,
    },
    FixedParam {
        canonical: "@params",
        aliases: &["@params"],
        required: false,
        unicode: true,
        output: false,
        int_handle: false,
    },
];

const SP_PREPARE: &[FixedParam] = &[
    FixedParam {
        canonical: "@handle",
        aliases: &["@handle"],
        required: true,
        unicode: false,
        output: true,
        int_handle: true,
    },
    FixedParam {
        canonical: "@params",
        aliases: &["@params"],
        required: true,
        unicode: true,
        output: false,
        int_handle: false,
    },
    FixedParam {
        canonical: "@stmt",
        aliases: &["@stmt", "@statement"],
        required: true,
        unicode: true,
        output: false,
        int_handle: false,
    },
];

const SP_PREPARE_OPTIONS: FixedParam = FixedParam {
    canonical: "@options",
    aliases: &["@options"],
    required: false,
    unicode: false,
    output: false,
    int_handle: false,
};

const SP_EXECUTE: &[FixedParam] = &[FixedParam {
    canonical: "@handle",
    aliases: &["@handle"],
    required: true,
    unicode: false,
    output: false,
    int_handle: true,
}];

const SP_UNPREPARE: &[FixedParam] = &[FixedParam {
    canonical: "@handle",
    aliases: &["@handle"],
    required: true,
    unicode: false,
    output: false,
    int_handle: true,
}];

struct BoundCall<'a> {
    fixed_values: Vec<FixedValue>,
    dynamic: Vec<(usize, &'a ProcArg<'a>)>,
    handle_arg: Option<usize>,
}

#[derive(Clone)]
enum FixedValue {
    Text(String),
    Int(i32),
    Missing,
}

pub(crate) fn resolve_sp_executesql(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let bound = bind_call("sp_executesql", SP_EXECUTESQL, args, false)?;
    let statement = text_value(&bound.fixed_values[0])
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_executesql", "@statement"))?;
    let params_text = bound
        .fixed_values
        .get(1)
        .and_then(text_value)
        .unwrap_or_default();
    let dynamic = bind_dynamic_values("sp_executesql", &params_text, &statement, &bound.dynamic)?;
    Ok(ProcAction::ExecuteSql {
        statement,
        params: dynamic,
    })
}

pub(crate) fn resolve_sp_prepare(args: &[ProcArg<'_>], execute: bool) -> SqlResult<ProcAction> {
    let proc = if execute { "sp_prepexec" } else { "sp_prepare" };
    let bound = bind_call(proc, SP_PREPARE, args, !execute)?;
    let handle_arg = bound
        .handle_arg
        .ok_or_else(|| SqlError::procedure_expects_parameter(proc, "@handle"))?;
    let params_text = text_value(&bound.fixed_values[1])
        .ok_or_else(|| SqlError::procedure_expects_parameter(proc, "@params"))?;
    let statement = text_value(&bound.fixed_values[2])
        .ok_or_else(|| SqlError::procedure_expects_parameter(proc, "@stmt"))?;
    let declarations = parse_parameter_declarations(&params_text, &ParseOptions::default())?;
    let execute_with = if execute {
        Some(bind_dynamic_values(
            proc,
            &params_text,
            &statement,
            &bound.dynamic,
        )?)
    } else {
        ensure_prepare_tail(proc, &bound.dynamic)?;
        None
    };
    Ok(ProcAction::Prepare {
        statement,
        params: declarations,
        handle_arg,
        execute_with,
    })
}

pub(crate) fn resolve_sp_execute(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let bound = bind_call("sp_execute", SP_EXECUTE, args, false)?;
    let handle = int_value(&bound.fixed_values[0])
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_execute", "@handle"))?;
    let params = bind_execute_params(&bound.dynamic);
    Ok(ProcAction::Execute { handle, params })
}

pub(crate) fn resolve_sp_unprepare(args: &[ProcArg<'_>]) -> SqlResult<ProcAction> {
    let bound = bind_call("sp_unprepare", SP_UNPREPARE, args, false)?;
    if !bound.dynamic.is_empty() {
        return Err(SqlError::too_many_arguments("sp_unprepare"));
    }
    let handle = int_value(&bound.fixed_values[0])
        .ok_or_else(|| SqlError::procedure_expects_parameter("sp_unprepare", "@handle"))?;
    Ok(ProcAction::Unprepare { handle })
}

fn bind_call<'a>(
    proc: &str,
    fixed: &[FixedParam],
    args: &'a [ProcArg<'a>],
    allow_options: bool,
) -> SqlResult<BoundCall<'a>> {
    let mut slots = vec![FixedValue::Missing; fixed.len()];
    let mut dynamic = Vec::new();
    let mut handle_arg = None;
    let mut options: Option<(usize, &ProcArg<'a>)> = None;
    let mut positional = 0usize;
    let mut saw_named = false;

    for (index, arg) in args.iter().enumerate() {
        if let Some(name) = arg.name {
            saw_named = true;
            if let Some(slot) = fixed.iter().position(|spec| name_matches(spec, name)) {
                assign_fixed(proc, &fixed[slot], index, arg, &mut slots[slot])?;
                if fixed[slot].output && fixed[slot].int_handle {
                    handle_arg = Some(index);
                }
            } else if allow_options && name_matches(&SP_PREPARE_OPTIONS, name) {
                options = Some((index, arg));
            } else {
                dynamic.push((index, arg));
            }
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        } else if positional < fixed.len() {
            assign_fixed(proc, &fixed[positional], index, arg, &mut slots[positional])?;
            if fixed[positional].output && fixed[positional].int_handle {
                handle_arg = Some(index);
            }
            positional += 1;
        } else if allow_options && options.is_none() && matches!(arg.ty.ty, SqlType::Int) {
            options = Some((index, arg));
        } else {
            dynamic.push((index, arg));
        }
    }

    for (slot_idx, spec) in fixed.iter().enumerate() {
        if spec.required && matches!(slots[slot_idx], FixedValue::Missing) {
            return Err(SqlError::procedure_expects_parameter(proc, spec.canonical));
        }
    }

    if allow_options
        && let Some((_, arg)) = options
        && !matches!(arg.ty.ty, SqlType::Int)
    {
        return Err(SqlError::procedure_expects_type("@options", "int"));
    }

    Ok(BoundCall {
        fixed_values: slots,
        dynamic,
        handle_arg,
    })
}

fn ensure_prepare_tail(proc: &str, dynamic: &[(usize, &ProcArg<'_>)]) -> SqlResult<()> {
    match dynamic.len() {
        0 => Ok(()),
        1 => {
            let (_, arg) = dynamic[0];
            if !matches!(arg.ty.ty, SqlType::Int) {
                Err(SqlError::procedure_expects_type("@options", "int"))
            } else {
                Ok(())
            }
        }
        _ => Err(SqlError::too_many_arguments(proc)),
    }
}

fn assign_fixed(
    proc: &str,
    spec: &FixedParam,
    _index: usize,
    arg: &ProcArg<'_>,
    slot: &mut FixedValue,
) -> SqlResult<()> {
    if !matches!(slot, FixedValue::Missing) {
        return Err(SqlError::too_many_arguments(proc));
    }
    if spec.unicode {
        if !is_unicode_text(arg.ty) {
            return Err(SqlError::procedure_expects_type(
                spec.canonical,
                "ntext/nchar/nvarchar",
            ));
        }
        let Some(text) = read_text(arg.value) else {
            return Err(SqlError::procedure_expects_type(
                spec.canonical,
                "ntext/nchar/nvarchar",
            ));
        };
        *slot = FixedValue::Text(text);
    } else if spec.int_handle {
        if !matches!(arg.ty.ty, SqlType::Int) {
            return Err(SqlError::procedure_expects_type(spec.canonical, "int"));
        }
        *slot = FixedValue::Int(read_int(arg.value).unwrap_or(0));
    }
    Ok(())
}

fn bind_dynamic_values(
    proc: &str,
    params_text: &str,
    statement: &str,
    dynamic: &[(usize, &ProcArg<'_>)],
) -> SqlResult<Vec<ProcParam>> {
    let declarations = parse_parameter_declarations(params_text, &ParseOptions::default())?;

    if dynamic.is_empty() {
        if let Some(decl) = declarations.first() {
            let query = parameterized_query(params_text, statement);
            return Err(SqlError::parameter_not_supplied(&query, &decl.name));
        }
        return Ok(Vec::new());
    }

    if declarations.is_empty() {
        return Err(SqlError::no_parameter_but_arguments(""));
    }

    let mut bound = vec![None; declarations.len()];
    let mut positional = 0usize;
    let mut saw_named = false;

    for (index, arg) in dynamic {
        if let Some(name) = arg.name {
            saw_named = true;
            let Some(decl_idx) = declarations
                .iter()
                .position(|decl| eq_ignore_case(&decl.name, name))
            else {
                if let Some(decl) = first_unbound(&declarations, &bound) {
                    let query = parameterized_query(params_text, statement);
                    return Err(SqlError::parameter_not_supplied(&query, &decl.name));
                }
                return Err(SqlError::too_many_arguments(proc));
            };
            if bound[decl_idx].is_some() {
                return Err(SqlError::too_many_arguments(proc));
            }
            bound[decl_idx] = Some(make_proc_param(&declarations[decl_idx], arg));
        } else if saw_named {
            return Err(SqlError::positional_after_named((*index + 1) as i64));
        } else if positional >= declarations.len() {
            return Err(SqlError::too_many_arguments(proc));
        } else {
            bound[positional] = Some(make_proc_param(&declarations[positional], arg));
            positional += 1;
        }
    }

    for (decl, slot) in declarations.iter().zip(bound.iter()) {
        if slot.is_none() {
            let query = parameterized_query(params_text, statement);
            return Err(SqlError::parameter_not_supplied(&query, &decl.name));
        }
    }

    Ok(bound.into_iter().flatten().collect())
}

fn bind_execute_params(dynamic: &[(usize, &ProcArg<'_>)]) -> Vec<ProcParam> {
    dynamic
        .iter()
        .enumerate()
        .map(|(index, (_, arg))| ProcParam {
            name: arg
                .name
                .map(str::to_owned)
                .unwrap_or_else(|| format!("@P{}", index + 1)),
            ty: arg.ty.clone(),
            value: arg.value.clone(),
            output: arg.output,
        })
        .collect()
}

fn make_proc_param(decl: &ParameterDeclaration, arg: &ProcArg<'_>) -> ProcParam {
    ProcParam {
        name: decl.name.clone(),
        ty: declaration_type(&decl.ty),
        value: arg.value.clone(),
        output: decl.output || arg.output,
    }
}

fn declaration_type(decl: &DataType) -> TypeInfo {
    resolve_decl_type(decl).unwrap_or_else(|_| TypeInfo::new(SqlType::Int, true))
}

fn resolve_decl_type(decl: &DataType) -> SqlResult<TypeInfo> {
    let name = decl.name.to_ascii_lowercase();
    let sql_type = match name.as_str() {
        "int" | "integer" => SqlType::Int,
        "bigint" => SqlType::BigInt,
        "smallint" => SqlType::SmallInt,
        "tinyint" => SqlType::TinyInt,
        "bit" => SqlType::Bit,
        "nvarchar" | "national char varying" | "national character varying" => {
            sized_sql_type(decl, SqlType::NVarChar, 4_000, true)?
        }
        "varchar" | "char varying" | "character varying" => {
            sized_sql_type(decl, SqlType::VarChar, 8_000, true)?
        }
        "nchar" | "national char" | "national character" => {
            sized_sql_type(decl, SqlType::NChar, 4_000, false)?
        }
        "char" | "character" => sized_sql_type(decl, SqlType::Char, 8_000, false)?,
        "varbinary" | "binary varying" => sized_sql_type(decl, SqlType::VarBinary, 8_000, true)?,
        "binary" => sized_sql_type(decl, SqlType::Binary, 8_000, false)?,
        "decimal" | "dec" | "numeric" => exact_numeric(decl)?,
        "float" | "double precision" => SqlType::Float,
        "real" => SqlType::Real,
        "money" => SqlType::Money,
        "smallmoney" => SqlType::SmallMoney,
        "date" => SqlType::Date,
        "datetime" => SqlType::DateTime,
        "smalldatetime" => SqlType::SmallDateTime,
        "datetime2" => SqlType::DateTime2(7),
        "time" => SqlType::Time(7),
        "uniqueidentifier" => SqlType::UniqueIdentifier,
        _ => return Err(SqlError::cannot_find_data_type(1, &decl.name)),
    };
    Ok(TypeInfo::new(sql_type, true))
}

fn sized_sql_type(
    decl: &DataType,
    make: fn(Len) -> SqlType,
    max: i64,
    allow_max: bool,
) -> SqlResult<SqlType> {
    let len = match decl.args.as_slice() {
        [] => Len::Fixed(1),
        [TypeArg::Number(n)] if (1..=max).contains(n) => Len::Fixed(
            u16::try_from(*n).map_err(|_| SqlError::cannot_find_data_type(1, &decl.name))?,
        ),
        [TypeArg::Max] if allow_max => Len::Max,
        _ => return Err(SqlError::cannot_find_data_type(1, &decl.name)),
    };
    Ok(make(len))
}

fn exact_numeric(decl: &DataType) -> SqlResult<SqlType> {
    let (precision, scale) = match decl.args.as_slice() {
        [] => (18, 0),
        [TypeArg::Number(p)] => (*p, 0),
        [TypeArg::Number(p), TypeArg::Number(s)] => (*p, *s),
        _ => return Err(SqlError::cannot_find_data_type(1, &decl.name)),
    };
    if !(1..=38).contains(&precision) || !(0..=precision).contains(&scale) {
        return Err(SqlError::cannot_find_data_type(1, &decl.name));
    }
    Ok(SqlType::Decimal {
        precision: u8::try_from(precision)
            .map_err(|_| SqlError::cannot_find_data_type(1, &decl.name))?,
        scale: u8::try_from(scale).map_err(|_| SqlError::cannot_find_data_type(1, &decl.name))?,
    })
}

fn first_unbound<'a>(
    declarations: &'a [ParameterDeclaration],
    bound: &[Option<ProcParam>],
) -> Option<&'a ParameterDeclaration> {
    declarations
        .iter()
        .zip(bound.iter())
        .find_map(|(decl, slot)| slot.is_none().then_some(decl))
}

fn parameterized_query(params_text: &str, statement: &str) -> String {
    if params_text.is_empty() {
        statement.to_owned()
    } else {
        format!("({params_text}){statement}")
    }
}

fn name_matches(spec: &FixedParam, name: &str) -> bool {
    spec.aliases.iter().any(|alias| eq_ignore_case(alias, name))
}

fn eq_ignore_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn is_unicode_text(ty: &TypeInfo) -> bool {
    matches!(ty.ty, SqlType::NVarChar(_) | SqlType::NChar(_))
}

fn text_value(value: &FixedValue) -> Option<String> {
    match value {
        FixedValue::Text(text) => Some(text.clone()),
        FixedValue::Missing => None,
        FixedValue::Int(_) => None,
    }
}

fn int_value(value: &FixedValue) -> Option<i32> {
    match value {
        FixedValue::Int(v) => Some(*v),
        _ => None,
    }
}

fn read_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.text.clone()),
        Value::Null => None,
        _ => None,
    }
}

fn read_int(value: &Value) -> Option<i32> {
    match value {
        Value::I32(v) => Some(*v),
        Value::I16(v) => Some(i32::from(*v)),
        Value::I64(v) => i32::try_from(*v).ok(),
        Value::Null => Some(0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::SqlString;

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

    fn arg<'a>(
        name: Option<&'a str>,
        ty: &'a TypeInfo,
        value: &'a Value,
        output: bool,
    ) -> ProcArg<'a> {
        ProcArg {
            name,
            ty,
            value,
            output,
            default: false,
        }
    }

    #[test]
    fn empty_params_with_extra_value_is_8146() {
        let err = resolve_sp_executesql(&[
            arg(None, &nvarchar_ty(), &nvarchar("SELECT 1"), false),
            arg(None, &nvarchar_ty(), &nvarchar(""), false),
            arg(None, &int_ty(), &int(1), false),
        ])
        .unwrap_err();
        assert_eq!(err.number, 8146);
    }

    #[test]
    fn positional_after_named_is_119() {
        let err = resolve_sp_executesql(&[
            arg(Some("@stmt"), &nvarchar_ty(), &nvarchar("SELECT 1"), false),
            arg(None, &int_ty(), &int(1), false),
        ])
        .unwrap_err();
        assert_eq!(err.number, 119);
    }
}
