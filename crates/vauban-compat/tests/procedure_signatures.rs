//! Arguments of system procedures bound to their declared parameters: names and positions,
//! `DEFAULT`, `OUTPUT`, conversion to the declared type, and the error a wrong call raises.

use vauban_compat::{ProcAction, ProcArg, ProcParam, resolve_system_procedure};
use vauban_errors::SqlResult;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn nvarchar() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), false)
}

fn varchar() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), false)
}

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

/// One argument: name, type, value, `OUTPUT`, `DEFAULT`.
struct Arg(Option<&'static str>, TypeInfo, Value, bool, bool);

fn pos(ty: TypeInfo, value: Value) -> Arg {
    Arg(None, ty, value, false, false)
}

fn named(name: &'static str, ty: TypeInfo, value: Value) -> Arg {
    Arg(Some(name), ty, value, false, false)
}

fn default_arg(name: Option<&'static str>) -> Arg {
    Arg(name, int(), Value::Null, false, true)
}

fn output(Arg(name, ty, value, _, default): Arg) -> Arg {
    Arg(name, ty, value, true, default)
}

fn call(procedure: &str, args: &[Arg]) -> SqlResult<ProcAction> {
    let args: Vec<ProcArg<'_>> = args
        .iter()
        .map(|Arg(name, ty, value, output, default)| ProcArg {
            name: *name,
            ty,
            value,
            output: *output,
            default: *default,
        })
        .collect();
    resolve_system_procedure(procedure, &args).expect("a system procedure")
}

/// The parameters a template procedure binds.
fn template_params(procedure: &str, args: &[Arg]) -> Vec<ProcParam> {
    match call(procedure, args).expect("the call binds") {
        ProcAction::Template { params, .. } => params,
        other => panic!("expected a template, got {other:?}"),
    }
}

/// Number and state of the error the call raises.
fn refused(procedure: &str, args: &[Arg]) -> (u32, u8) {
    let err = call(procedure, args).expect_err("the call is refused");
    (err.number, err.state)
}

#[test]
fn default_leaves_a_required_parameter_missing() {
    assert_eq!(refused("sp_pkeys", &[default_arg(None)]), (201, 4));
    assert_eq!(
        refused("sp_pkeys", &[default_arg(Some("@table_name"))]),
        (201, 4)
    );
    assert_eq!(
        refused("sp_pkeys", &[named("@bogus", int(), Value::I32(1))]),
        (201, 4)
    );
}

#[test]
fn default_gives_an_optional_parameter_its_default() {
    let params = template_params(
        "sp_tables",
        &[default_arg(None), named("@fUsePattern", int(), Value::Null)],
    );
    assert_eq!(params[0].value, Value::Null);
    assert_eq!(params[4].value, Value::Null);
    let params = template_params("sp_tables", &[default_arg(Some("@fUsePattern"))]);
    assert_eq!(params[4].value, Value::Bit(true));
}

#[test]
fn a_parameter_given_twice_is_8143() {
    assert_eq!(
        refused(
            "sp_tables",
            &[
                named("@table_name", nvarchar(), text("x")),
                named("@table_name", nvarchar(), text("y")),
            ],
        ),
        (8143, 1)
    );
    assert_eq!(
        refused(
            "sp_pkeys",
            &[
                pos(nvarchar(), text("a")),
                named("@table_name", nvarchar(), text("b")),
            ],
        ),
        (8143, 1)
    );
}

#[test]
fn too_many_arguments_comes_before_the_duplicate() {
    assert_eq!(
        refused(
            "sp_who",
            &[
                named("@loginame", nvarchar(), text("sa")),
                named("@loginame", nvarchar(), text("sa")),
            ],
        ),
        (8144, 2)
    );
}

#[test]
fn an_argument_is_converted_to_the_declared_type() {
    let params = template_params("sp_who", &[pos(int(), Value::I32(1))]);
    assert_eq!(params[0].value, text("1"));
    assert_eq!(params[0].ty.ty, SqlType::NVarChar(Len::Max));
    let params = template_params("sp_server_info", &[pos(nvarchar(), text("1"))]);
    assert_eq!(params[0].value, Value::I32(1));
    let params = template_params("sp_tables", &[named("@fUsePattern", int(), Value::I32(2))]);
    assert_eq!(params[4].value, Value::Bit(true));
    assert_eq!(
        refused(
            "sp_stored_procedures",
            &[named("@fUsePattern", varchar(), text("x"))]
        ),
        (8114, 5)
    );
}

#[test]
fn output_on_a_parameter_not_declared_output_is_8162() {
    assert_eq!(
        refused("sp_who", &[output(pos(nvarchar(), text("sa")))]),
        (8162, 2)
    );
    assert_eq!(
        refused(
            "sp_pkeys",
            &[
                named("@bogus", int(), Value::I32(1)),
                output(named("@table_name", nvarchar(), text("a"))),
            ],
        ),
        (8162, 2)
    );
}

#[test]
fn arguments_to_sp_databases_are_8146_state_1() {
    assert_eq!(
        refused("sp_databases", &[named("@x", int(), Value::I32(1))]),
        (8146, 1)
    );
    assert_eq!(
        refused("sp_databases", &[pos(int(), Value::I32(1))]),
        (8146, 1)
    );
}

#[test]
fn a_conversion_error_of_an_earlier_parameter_comes_before_8162_of_a_later_one() {
    let bit = TypeInfo::new(SqlType::Bit, true);
    assert_eq!(
        refused(
            "sp_columns_100",
            &[
                pos(nvarchar(), text("t")),
                named("@ODBCVer", varchar(), text("x")),
                output(named("@fUsePattern", bit, Value::Null)),
            ],
        ),
        (8114, 5)
    );
}

#[test]
fn executesql_checks_its_parameters_in_declaration_order() {
    let two = "@a int, @b int";
    assert_eq!(
        executesql_refused(
            two,
            vec![
                output(named("@b", int(), Value::I32(1))),
                named("@a", varchar(), text("x")),
            ],
        ),
        (8114, 5)
    );
    assert_eq!(
        executesql_refused(
            two,
            vec![
                named("@zz", int(), Value::I32(1)),
                output(named("@b", int(), Value::I32(1))),
            ],
        ),
        (8178, 1)
    );
    assert_eq!(
        executesql_refused(two, vec![named("@b", varchar(), text("x"))]),
        (8178, 1)
    );
    assert_eq!(
        executesql_refused(
            two,
            vec![default_arg(None), output(pos(int(), Value::I32(1)))],
        ),
        (8178, 1)
    );
}

#[test]
fn an_unknown_name_after_a_conversion_error_is_not_reported() {
    assert_eq!(
        refused(
            "sp_special_columns",
            &[
                named("@table_name", nvarchar(), text("a")),
                named("@bogus", int(), Value::I32(1)),
                named("@odbcver", varchar(), text("x")),
            ],
        ),
        (8114, 5)
    );
}

fn executesql(declarations: &str, values: Vec<Arg>) -> SqlResult<ProcAction> {
    let mut args = vec![
        pos(nvarchar(), text("SELECT @a")),
        pos(nvarchar(), text(declarations)),
    ];
    args.extend(values);
    call("sp_executesql", &args)
}

fn executesql_refused(declarations: &str, values: Vec<Arg>) -> (u32, u8) {
    let err = executesql(declarations, values).expect_err("the call is refused");
    (err.number, err.state)
}

#[test]
fn executesql_reports_an_unsupplied_parameter_before_a_duplicate() {
    assert_eq!(
        executesql_refused(
            "@a int, @b int",
            vec![
                named("@a", int(), Value::I32(1)),
                named("@a", int(), Value::I32(2)),
            ],
        ),
        (8178, 1)
    );
}

#[test]
fn executesql_reports_a_conversion_error_before_an_unsupplied_parameter() {
    assert_eq!(
        executesql_refused(
            "@a int, @b int",
            vec![
                named("@c", int(), Value::I32(1)),
                named("@a", varchar(), text("x")),
            ],
        ),
        (8114, 5)
    );
}

#[test]
fn executesql_refuses_output_on_a_parameter_not_declared_output() {
    assert_eq!(
        executesql_refused("@a int", vec![output(named("@a", int(), Value::I32(1)))]),
        (8162, 2)
    );
    let action = executesql(
        "@a int OUTPUT",
        vec![output(named("@a", int(), Value::I32(1)))],
    )
    .expect("an OUTPUT parameter takes an OUTPUT argument");
    let ProcAction::ExecuteSql { params, .. } = action else {
        panic!("expected ExecuteSql");
    };
    assert!(params[0].output);
}

#[test]
fn executesql_refuses_an_unknown_type_without_any_argument() {
    assert_eq!(executesql_refused("@a foo", Vec::new()).0, 2715);
}

#[test]
fn executesql_treats_default_as_unsupplied() {
    assert_eq!(
        executesql_refused("@a int", vec![default_arg(Some("@a"))]),
        (8178, 1)
    );
}
