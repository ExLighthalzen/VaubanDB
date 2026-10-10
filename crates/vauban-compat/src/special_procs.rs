//! Binding arguments of `sp_executesql`, `sp_prepare`, `sp_execute` and related procedures.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{ParseOptions, parse_parameter_declarations};
use vauban_session::{
    CallForm, ParamDefault, ProcSignature, SignatureParam, bind_arguments, is_unicode_text,
};
use vauban_types::{Len, SqlType, Value};

use crate::procedures::{ProcAction, ProcParam};

pub use vauban_session::ProcArg;

/// A fixed parameter of these procedures. Its argument is read under the type the caller
/// gave it, without conversion: an `nvarchar` parameter refuses another type with 214, and
/// an `int` one reads a handle. A named argument that answers to another name belongs to
/// the parameter list the call binds next.
type FixedParam = SignatureParam<'static>;

const TEXT: SqlType = SqlType::NVarChar(Len::Max);

const SP_EXECUTESQL: &[FixedParam] = &[
    SignatureParam::new("@statement", TEXT)
        .required()
        .with_aliases(&["@stmt"]),
    SignatureParam::new("@params", TEXT),
];

const SP_PREPARE: &[FixedParam] = &[
    SignatureParam::new("@handle", SqlType::Int)
        .required()
        .output(),
    SignatureParam::new("@params", TEXT).required(),
    SignatureParam::new("@stmt", TEXT)
        .required()
        .with_aliases(&["@statement"]),
];

const SP_PREPARE_OPTIONS: FixedParam = SignatureParam::new("@options", SqlType::Int);

const SP_EXECUTE: &[FixedParam] = &[SignatureParam::new("@handle", SqlType::Int).required()];

const SP_UNPREPARE: &[FixedParam] = &[SignatureParam::new("@handle", SqlType::Int).required()];

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
    let dynamic = bind_dynamic_values(&params_text, &statement, &bound.dynamic)?;
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
            if let Some(slot) = fixed.iter().position(|spec| spec.answers_to(name)) {
                assign_fixed(proc, &fixed[slot], index, arg, &mut slots[slot])?;
                if fixed[slot].output {
                    handle_arg = Some(index);
                }
            } else if allow_options && SP_PREPARE_OPTIONS.answers_to(name) {
                options = Some((index, arg));
            } else {
                dynamic.push((index, arg));
            }
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        } else if positional < fixed.len() {
            assign_fixed(proc, &fixed[positional], index, arg, &mut slots[positional])?;
            if fixed[positional].output {
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
        if spec.default == ParamDefault::Required && matches!(slots[slot_idx], FixedValue::Missing)
        {
            return Err(SqlError::procedure_expects_parameter(proc, spec.name));
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
    if spec.ty == TEXT {
        if !is_unicode_text(arg.ty) {
            return Err(SqlError::procedure_expects_type(
                spec.name,
                "ntext/nchar/nvarchar",
            ));
        }
        let Some(text) = read_text(arg.value) else {
            return Err(SqlError::procedure_expects_type(
                spec.name,
                "ntext/nchar/nvarchar",
            ));
        };
        *slot = FixedValue::Text(text);
    } else if spec.ty == SqlType::Int {
        if !matches!(arg.ty.ty, SqlType::Int) {
            return Err(SqlError::procedure_expects_type(spec.name, "int"));
        }
        *slot = FixedValue::Int(read_int(arg.value).unwrap_or(0));
    }
    Ok(())
}

/// Binds the arguments that follow the fixed ones to the declaration list `params_text`,
/// each value converted to its declared type.
fn bind_dynamic_values(
    params_text: &str,
    statement: &str,
    dynamic: &[(usize, &ProcArg<'_>)],
) -> SqlResult<Vec<ProcParam>> {
    let declarations = parse_parameter_declarations(params_text, &ParseOptions::default())?;
    let mut params = Vec::with_capacity(declarations.len());
    for (index, decl) in declarations.iter().enumerate() {
        let ty = ProcParam::declared_type(&decl.ty, index as u32 + 1)?;
        let param = SignatureParam::new(&decl.name, ty.ty).required();
        params.push(if decl.output { param.output() } else { param });
    }
    let signature = ProcSignature {
        procedure: "",
        params: &params,
    };
    let query = parameterized_query(params_text, statement);
    let args: Vec<ProcArg<'_>> = dynamic.iter().map(|(_, arg)| **arg).collect();
    bind_arguments(&signature, CallForm::ParameterizedQuery(&query), &args)
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

fn parameterized_query(params_text: &str, statement: &str) -> String {
    if params_text.is_empty() {
        statement.to_owned()
    } else {
        format!("({params_text}){statement}")
    }
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
    use vauban_types::{SqlString, TypeInfo};

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
    fn sysname_param_resolves_to_nvarchar128() {
        let stmt = nvarchar("SELECT @p AS p");
        let params = nvarchar("@p sysname");
        let value = nvarchar("x");
        let sysname_ty = TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false);
        let action = resolve_sp_executesql(&[
            arg(None, &nvarchar_ty(), &stmt, false),
            arg(None, &nvarchar_ty(), &params, false),
            arg(Some("@p"), &sysname_ty, &value, false),
        ])
        .expect("ok");
        match action {
            ProcAction::ExecuteSql { statement, params } => {
                assert_eq!(statement, "SELECT @p AS p");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "@p");
                assert_eq!(params[0].ty.ty, SqlType::NVarChar(Len::Fixed(128)));
                assert_eq!(params[0].value, nvarchar("x"));
            }
            other => panic!("expected ExecuteSql, got {other:?}"),
        }
    }

    #[test]
    fn datetimeoffset_param_is_recognized() {
        let stmt = nvarchar("SELECT @p AS p");
        let params = nvarchar("@p datetimeoffset");
        let value = Value::Null;
        let dto_ty = TypeInfo::new(SqlType::DateTimeOffset(7), true);
        let action = resolve_sp_executesql(&[
            arg(None, &nvarchar_ty(), &stmt, false),
            arg(None, &nvarchar_ty(), &params, false),
            arg(Some("@p"), &dto_ty, &value, false),
        ])
        .expect("ok");
        match action {
            ProcAction::ExecuteSql { params, .. } => {
                assert_eq!(params[0].ty.ty, SqlType::DateTimeOffset(7));
            }
            other => panic!("expected ExecuteSql, got {other:?}"),
        }
    }

    #[test]
    fn unknown_param_type_is_refused_not_converted_to_int() {
        let stmt = nvarchar("SELECT @p AS p");
        let params = nvarchar("@p notarealtype");
        let value = int(1);
        let err = resolve_sp_executesql(&[
            arg(None, &nvarchar_ty(), &stmt, false),
            arg(None, &nvarchar_ty(), &params, false),
            arg(Some("@p"), &int_ty(), &value, false),
        ])
        .unwrap_err();
        assert_eq!(err.number, 2715);
        assert!(err.message.contains("notarealtype"));
    }

    #[test]
    fn unknown_param_type_names_its_rank_among_declarations() {
        let stmt = nvarchar("SELECT @p1, @p2");
        let params = nvarchar("@p1 int, @p2 notarealtype");
        let v1 = int(1);
        let v2 = int(2);
        let err = resolve_sp_executesql(&[
            arg(None, &nvarchar_ty(), &stmt, false),
            arg(None, &nvarchar_ty(), &params, false),
            arg(Some("@p1"), &int_ty(), &v1, false),
            arg(Some("@p2"), &int_ty(), &v2, false),
        ])
        .unwrap_err();
        assert_eq!(err.number, 2715);
        assert_eq!(
            err.message,
            "Column, parameter or variable #2: unknown data type notarealtype."
        );
    }

    /// The declared type of one `@p`, as `sp_executesql` binds it to a `NULL`.
    fn declared(declaration: &str) -> SqlResult<TypeInfo> {
        let stmt = nvarchar("SELECT @p AS p");
        let params = nvarchar(declaration);
        let value = Value::Null;
        match resolve_sp_executesql(&[
            arg(None, &nvarchar_ty(), &stmt, false),
            arg(None, &nvarchar_ty(), &params, false),
            arg(Some("@p"), &int_ty(), &value, false),
        ])? {
            ProcAction::ExecuteSql { params, .. } => Ok(params[0].ty.clone()),
            other => panic!("expected ExecuteSql, got {other:?}"),
        }
    }

    #[test]
    fn declared_param_types_keep_their_written_shape() {
        let shapes = [
            (
                "@p numeric(5,2)",
                SqlType::Numeric {
                    precision: 5,
                    scale: 2,
                },
            ),
            (
                "@p decimal",
                SqlType::Decimal {
                    precision: 18,
                    scale: 0,
                },
            ),
            ("@p float(10)", SqlType::Real),
            ("@p float(53)", SqlType::Float),
            ("@p datetime2(3)", SqlType::DateTime2(3)),
            ("@p time(0)", SqlType::Time(0)),
            ("@p datetimeoffset(2)", SqlType::DateTimeOffset(2)),
            ("@p varchar", SqlType::VarChar(Len::Fixed(1))),
            ("@p nvarchar(max)", SqlType::NVarChar(Len::Max)),
        ];
        for (declaration, expected) in shapes {
            let ty = declared(declaration).expect(declaration);
            assert_eq!(ty.ty, expected, "{declaration}");
            assert!(ty.nullable, "{declaration}");
        }
    }

    #[test]
    fn length_on_a_type_without_one_is_2716() {
        for declaration in ["@p int(10)", "@p sysname(10)"] {
            let err = declared(declaration).unwrap_err();
            assert_eq!(err.number, 2716, "{declaration}");
            assert_eq!(err.severity, 16, "{declaration}");
        }
        assert_eq!(declared("@p int(10)").unwrap_err().state, 1);
        assert_eq!(declared("@p sysname(10)").unwrap_err().state, 3);
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
