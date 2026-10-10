//! Binding the arguments of a call to the parameters a procedure declares.
//!
//! A [`ProcSignature`] lists the parameters of a procedure: name, type, default, `OUTPUT`,
//! `READONLY`. [`bind_arguments`] takes it with the arguments of an `EXECUTE` or of an RPC
//! and answers one [`ProcParam`] per declared parameter, in declaration order, each value
//! converted to the declared type; or the error the call raises.
//!
//! # Matching
//!
//! An argument without a name fills the parameter of its position; a named one fills the
//! parameter that answers to its name, case aside. Once a named argument appears, a
//! positional one is refused with 119. An argument written `DEFAULT` occupies its
//! parameter without supplying it: the parameter takes its default, and a parameter
//! declared without default is missing (`signature/tests/mod.rs`,
//! `default_written_in_the_call_takes_the_default_or_is_201`).
//!
//! # Order of the errors
//!
//! A call can be wrong in several ways at once; the error it raises is the first of these
//! checks that fails (`signature/tests/mod.rs`, `a_procedure_reports_its_errors_in_order`,
//! `a_parameterized_query_reports_its_errors_in_order`):
//!
//! 1. 119, a positional argument after a named one;
//! 2. 8146 or 8144, more arguments than parameters;
//! 3. parameter by parameter, in declaration order: a parameter without default that the
//!    call does not supply (201 for a [`CallForm::Procedure`], 8178 for a
//!    [`CallForm::ParameterizedQuery`]), then 8162 for `OUTPUT` on a parameter not declared
//!    so, then the conversion of its value (8114, 257, 206);
//! 4. 8145 or 8143, whichever argument comes first.
//!

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{SqlString, SqlType, TypeInfo, Value};

use crate::prepared::bound_argument;
use crate::rpc::{ProcArg, ProcParam};

/// The parameters of a procedure, as a call binds its arguments to them.
#[derive(Debug, Clone, Copy)]
pub struct ProcSignature<'s> {
    /// Name of the procedure, as its errors name it.
    pub procedure: &'s str,
    /// Parameters, in declaration order.
    pub params: &'s [SignatureParam<'s>],
}

/// The value a parameter takes when the call does not supply it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamDefault<'s> {
    /// No default: the call must supply the parameter.
    Required,
    /// `NULL`.
    Null,
    /// An `int`.
    Int(i32),
    /// A `bit`.
    Bit(bool),
    /// A character string.
    Text(&'s str),
}

/// One parameter of a [`ProcSignature`].
#[derive(Debug, Clone, Copy)]
pub struct SignatureParam<'s> {
    /// Name, `@` included, as the bound [`ProcParam`] and the errors carry it.
    pub name: &'s str,
    /// Other names the parameter answers to.
    pub aliases: &'s [&'s str],
    /// Declared type; the bound value is converted to it.
    pub ty: SqlType,
    /// Value taken when the call does not supply the parameter.
    pub default: ParamDefault<'s>,
    /// Declared `OUTPUT`.
    pub output: bool,
    /// Declared `READONLY`. Binding does not read it; it travels with the signature for the
    /// procedure that runs.
    pub read_only: bool,
}

impl<'s> SignatureParam<'s> {
    /// A parameter `name` of type `ty`, defaulting to `NULL`, neither `OUTPUT` nor
    /// `READONLY`.
    pub const fn new(name: &'s str, ty: SqlType) -> Self {
        SignatureParam {
            name,
            aliases: &[],
            ty,
            default: ParamDefault::Null,
            output: false,
            read_only: false,
        }
    }

    /// The same parameter, without default.
    pub const fn required(self) -> Self {
        self.with_default(ParamDefault::Required)
    }

    /// The same parameter, with `default`.
    pub const fn with_default(self, default: ParamDefault<'s>) -> Self {
        SignatureParam { default, ..self }
    }

    /// The same parameter, also answering to `aliases`.
    pub const fn with_aliases(self, aliases: &'s [&'s str]) -> Self {
        SignatureParam { aliases, ..self }
    }

    /// The same parameter, declared `OUTPUT`.
    pub const fn output(self) -> Self {
        SignatureParam {
            output: true,
            ..self
        }
    }

    /// Whether an argument written `name =` fills this parameter, case aside.
    pub fn answers_to(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    }

    /// The declared type, nullable.
    fn type_info(&self) -> TypeInfo {
        TypeInfo::new(self.ty, true)
    }

    /// The value of the parameter when the call does not supply it.
    fn default_value(&self) -> Value {
        match self.default {
            ParamDefault::Required | ParamDefault::Null => Value::Null,
            ParamDefault::Int(value) => Value::I32(value),
            ParamDefault::Bit(value) => Value::Bit(value),
            ParamDefault::Text(text) => Value::String(SqlString {
                text: text.to_owned(),
            }),
        }
    }
}

/// What kind of call binds its arguments, which decides the errors it raises and their
/// order (module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallForm<'q> {
    /// An `EXECUTE` of a procedure, or its RPC.
    Procedure,
    /// The values `sp_executesql` binds to its declaration list; the text is the
    /// parameterized query, `(@params)statement`, that 8178 names.
    ParameterizedQuery(&'q str),
}

/// Whether a type carries Unicode text: `nchar` or `nvarchar`.
pub fn is_unicode_text(ty: &TypeInfo) -> bool {
    matches!(ty.ty, SqlType::NVarChar(_) | SqlType::NChar(_))
}

/// State of 201 when an `EXECUTE` leaves a parameter without default unsupplied.
const MISSING_PARAMETER_STATE: u8 = 4;
/// Binds `args` to the parameters of `signature`, one [`ProcParam`] per parameter in
/// declaration order.
///
/// A supplied value is converted to the declared type ([`ProcParam::bound_value`]); an
/// unsupplied parameter takes its default. The bound parameter is `OUTPUT` when its
/// declaration is.
///
/// # Errors
///
/// The first error of the order the module documentation gives for `form`.
pub fn bind_arguments(
    signature: &ProcSignature<'_>,
    form: CallForm<'_>,
    args: &[ProcArg<'_>],
) -> SqlResult<Vec<ProcParam>> {
    let params = signature.params;
    refuse_positional_after_named(args)?;
    if args.len() > params.len() {
        return Err(too_many_arguments(signature));
    }

    let matched = match_arguments(params, args);
    let mut values: Vec<Option<Value>> = vec![None; params.len()];
    for (slot, param) in params.iter().enumerate() {
        let supplied = matched
            .filled
            .iter()
            .find(|&&(_, filled)| filled == slot)
            .map(|&(arg, _)| &args[arg])
            .filter(|arg| !arg.default);
        let Some(arg) = supplied else {
            if param.default == ParamDefault::Required {
                return Err(missing_parameter(signature.procedure, param.name, form));
            }
            continue;
        };
        refuse_output(param, arg)?;
        values[slot] = Some(bound_argument(arg.value, arg.ty, &param.type_info())?);
    }
    if let Some(misfit) = matched.misfit {
        return Err(misfit.into_error(signature.procedure));
    }

    Ok(params
        .iter()
        .zip(values)
        .map(|(param, value)| ProcParam {
            name: param.name.to_owned(),
            ty: param.type_info(),
            value: value.unwrap_or_else(|| param.default_value()),
            output: param.output,
        })
        .collect())
}

/// 119 at the first positional argument written after a named one.
fn refuse_positional_after_named(args: &[ProcArg<'_>]) -> SqlResult<()> {
    let mut saw_named = false;
    for (index, arg) in args.iter().enumerate() {
        if arg.name.is_some() {
            saw_named = true;
        } else if saw_named {
            return Err(SqlError::positional_after_named((index + 1) as i64));
        }
    }
    Ok(())
}

/// 8146 for a signature without parameter, 8144 otherwise.
fn too_many_arguments(signature: &ProcSignature<'_>) -> SqlError {
    if signature.params.is_empty() {
        SqlError::no_parameter_but_arguments(signature.procedure)
    } else {
        SqlError::too_many_arguments(signature.procedure)
    }
}

/// 201 for a procedure, 8178 for a parameterized query: `name` is not supplied.
fn missing_parameter(procedure: &str, name: &str, form: CallForm<'_>) -> SqlError {
    match form {
        CallForm::Procedure => {
            let mut err = SqlError::procedure_expects_parameter(procedure, name);
            err.state = MISSING_PARAMETER_STATE;
            err
        }
        CallForm::ParameterizedQuery(query) => SqlError::parameter_not_supplied(query, name),
    }
}

/// 8162 when `arg` asks for output from a parameter not declared `OUTPUT`.
fn refuse_output(param: &SignatureParam<'_>, arg: &ProcArg<'_>) -> SqlResult<()> {
    if arg.output && !param.output {
        return Err(SqlError::new(
            8162,
            16,
            2,
            format!(
                "Parameter \"{}\" is not declared OUTPUT, yet its argument asks for output.",
                param.name
            ),
        ));
    }
    Ok(())
}

/// The arguments placed on the parameters they fill.
struct Matched {
    /// `(argument index, parameter index)` of each argument that fills a parameter, in
    /// argument order.
    filled: Vec<(usize, usize)>,
    /// The first argument that fills nothing: an unknown name or a parameter given twice.
    misfit: Option<Misfit>,
}

/// An argument that fills no parameter.
enum Misfit {
    /// Named after no parameter.
    Unknown(String),
    /// Names a parameter an earlier argument already filled.
    Duplicate(String),
}

impl Misfit {
    fn into_error(self, procedure: &str) -> SqlError {
        match self {
            Misfit::Unknown(name) => SqlError::not_a_parameter(&name, procedure),
            Misfit::Duplicate(name) => SqlError::new(
                8143,
                16,
                1,
                format!("Parameter '{name}' was given more than once."),
            ),
        }
    }
}

/// Places each argument on its parameter; called once positional arguments are known to
/// come first and to be no more than the parameters.
fn match_arguments(params: &[SignatureParam<'_>], args: &[ProcArg<'_>]) -> Matched {
    let mut taken = vec![false; params.len()];
    let mut filled = Vec::with_capacity(args.len());
    let mut misfit = None;
    for (index, arg) in args.iter().enumerate() {
        let slot = match arg.name {
            None => index,
            Some(name) => match params.iter().position(|param| param.answers_to(name)) {
                Some(slot) => slot,
                None => {
                    misfit.get_or_insert(Misfit::Unknown(name.to_owned()));
                    continue;
                }
            },
        };
        if taken[slot] {
            misfit.get_or_insert(Misfit::Duplicate(params[slot].name.to_owned()));
            continue;
        }
        taken[slot] = true;
        filled.push((index, slot));
    }
    Matched { filled, misfit }
}

#[cfg(test)]
mod tests;
