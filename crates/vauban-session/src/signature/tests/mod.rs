use super::*;
use vauban_types::{Len, SqlString};

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

fn date() -> TypeInfo {
    TypeInfo::new(SqlType::Date, false)
}

/// One argument of a call: its name, type, value, `OUTPUT` and `DEFAULT` marks.
struct Arg {
    name: Option<&'static str>,
    ty: TypeInfo,
    value: Value,
    output: bool,
    default: bool,
}

fn pos(ty: TypeInfo, value: Value) -> Arg {
    Arg {
        name: None,
        ty,
        value,
        output: false,
        default: false,
    }
}

fn named(name: &'static str, ty: TypeInfo, value: Value) -> Arg {
    Arg {
        name: Some(name),
        ..pos(ty, value)
    }
}

fn default_arg(name: Option<&'static str>) -> Arg {
    Arg {
        name,
        default: true,
        ..pos(int(), Value::Null)
    }
}

fn output(arg: Arg) -> Arg {
    Arg {
        output: true,
        ..arg
    }
}

/// Three parameters shaped like those of a key catalogue procedure: a required text, an
/// optional text, an `int` with a default.
const KEYS: &[SignatureParam<'static>] = &[
    SignatureParam::new("@table_name", SqlType::NVarChar(Len::Max)).required(),
    SignatureParam::new("@table_owner", SqlType::NVarChar(Len::Max)),
    SignatureParam::new("@odbcver", SqlType::Int).with_default(ParamDefault::Int(2)),
];

/// Two required parameters, the second `OUTPUT`.
const QUERY: &[SignatureParam<'static>] = &[
    SignatureParam::new("@a", SqlType::Int).required(),
    SignatureParam::new("@b", SqlType::Int).required().output(),
];

fn bind_with(
    params: &[SignatureParam<'_>],
    form: CallForm<'_>,
    args: &[Arg],
) -> SqlResult<Vec<ProcParam>> {
    let args: Vec<ProcArg<'_>> = args
        .iter()
        .map(|arg| ProcArg {
            name: arg.name,
            ty: &arg.ty,
            value: &arg.value,
            output: arg.output,
            default: arg.default,
        })
        .collect();
    let signature = ProcSignature {
        procedure: if form == CallForm::Procedure {
            "sp_keys"
        } else {
            ""
        },
        params,
    };
    bind_arguments(&signature, form, &args)
}

fn bind(args: &[Arg]) -> SqlResult<Vec<ProcParam>> {
    bind_with(KEYS, CallForm::Procedure, args)
}

fn bind_query(args: &[Arg]) -> SqlResult<Vec<ProcParam>> {
    bind_with(
        QUERY,
        CallForm::ParameterizedQuery("(@a int, @b int OUTPUT)SELECT 1"),
        args,
    )
}

/// Number and state of the error `result` holds.
fn code(result: SqlResult<Vec<ProcParam>>) -> (u32, u8) {
    let err = result.expect_err("the call should be refused");
    (err.number, err.state)
}

#[test]
fn positional_then_named_arguments_fill_their_parameters_in_declaration_order() {
    let bound = bind(&[
        pos(nvarchar(), text("t")),
        named("@ODBCVER", int(), Value::I32(3)),
    ])
    .unwrap();
    let names: Vec<&str> = bound.iter().map(|param| param.name.as_str()).collect();
    assert_eq!(names, ["@table_name", "@table_owner", "@odbcver"]);
    assert_eq!(bound[0].value, text("t"));
    assert_eq!(bound[1].value, Value::Null);
    assert_eq!(bound[2].value, Value::I32(3));
    assert_eq!(bound[2].ty.ty, SqlType::Int);
    assert!(bound[2].ty.nullable);
}

#[test]
fn an_unsupplied_parameter_takes_its_default() {
    let params = [
        SignatureParam::new("@flag", SqlType::Bit).with_default(ParamDefault::Bit(true)),
        SignatureParam::new("@kind", SqlType::VarChar(Len::Fixed(1)))
            .with_default(ParamDefault::Text("R")),
    ];
    let bound = bind_with(&params, CallForm::Procedure, &[]).unwrap();
    assert_eq!(bound[0].value, Value::Bit(true));
    assert_eq!(bound[1].value, text("R"));
}

#[test]
fn default_written_in_the_call_takes_the_default_or_is_201() {
    let bound = bind(&[
        pos(nvarchar(), text("t")),
        default_arg(None),
        default_arg(None),
    ])
    .unwrap();
    assert_eq!(bound[2].value, Value::I32(2));
    assert_eq!(code(bind(&[default_arg(None)])), (201, 4));
    assert_eq!(code(bind(&[default_arg(Some("@table_name"))])), (201, 4));
}

#[test]
fn a_value_is_converted_to_the_declared_type() {
    let bound = bind(&[
        pos(varchar(), text("t")),
        named("@odbcver", nvarchar(), text("3")),
    ])
    .unwrap();
    assert_eq!(bound[0].value, text("t"));
    assert_eq!(bound[2].value, Value::I32(3));
    assert_eq!(
        code(bind(&[
            pos(nvarchar(), text("t")),
            named("@odbcver", nvarchar(), text("x"))
        ])),
        (8114, 5)
    );
    let day = Value::Date(vauban_types::Date { days: 1 });
    assert_eq!(
        code(bind(&[
            pos(nvarchar(), text("t")),
            named("@odbcver", date(), day)
        ])),
        (206, 2)
    );
    let bound = bind(&[
        pos(nvarchar(), text("t")),
        named("@odbcver", date(), Value::Null),
    ]);
    assert_eq!(bound.unwrap()[2].value, Value::Null);
}

#[test]
fn a_missing_parameter_without_default_is_201_state_4() {
    let err = bind(&[named("@table_owner", nvarchar(), text("o"))]).unwrap_err();
    assert_eq!((err.number, err.severity, err.state), (201, 16, 4));
    assert!(err.message.contains("@table_name"), "{}", err.message);
}

#[test]
fn more_arguments_than_parameters_is_8144() {
    let four = [
        pos(nvarchar(), text("a")),
        pos(nvarchar(), text("b")),
        pos(int(), Value::I32(1)),
        pos(int(), Value::I32(2)),
    ];
    assert_eq!(code(bind(&four)), (8144, 2));
    let unknown_names = [
        pos(nvarchar(), text("a")),
        named("@x", int(), Value::I32(1)),
        named("@y", int(), Value::I32(1)),
        named("@z", int(), Value::I32(1)),
    ];
    assert_eq!(code(bind(&unknown_names)), (8144, 2));
}

#[test]
fn arguments_to_a_procedure_without_parameters_are_8146() {
    let one = [pos(int(), Value::I32(1))];
    assert_eq!(code(bind_with(&[], CallForm::Procedure, &one)), (8146, 1));
    assert_eq!(
        code(bind_with(&[], CallForm::ParameterizedQuery(""), &one)),
        (8146, 1)
    );
}

#[test]
fn an_unknown_name_is_8145() {
    let err = bind(&[
        pos(nvarchar(), text("t")),
        named("@bogus", int(), Value::I32(1)),
    ])
    .unwrap_err();
    assert_eq!((err.number, err.state), (8145, 1));
    assert!(err.message.contains("@bogus"), "{}", err.message);
}

#[test]
fn a_parameter_given_twice_is_8143() {
    assert_eq!(
        code(bind(&[
            pos(nvarchar(), text("a")),
            named("@table_name", nvarchar(), text("b")),
        ])),
        (8143, 1)
    );
    assert_eq!(
        code(bind(&[
            named("@table_name", nvarchar(), text("a")),
            default_arg(Some("@table_name")),
        ])),
        (8143, 1)
    );
}

#[test]
fn a_positional_argument_after_a_named_one_is_119() {
    let err = bind(&[
        pos(nvarchar(), text("a")),
        named("@table_owner", nvarchar(), text("o")),
        pos(int(), Value::I32(1)),
    ])
    .unwrap_err();
    assert_eq!((err.number, err.severity, err.state), (119, 15, 1));
    assert!(err.message.contains('3'), "{}", err.message);
}

#[test]
fn output_on_a_parameter_not_declared_output_is_8162() {
    let err = bind(&[output(pos(nvarchar(), text("a")))]).unwrap_err();
    assert_eq!((err.number, err.severity, err.state), (8162, 16, 2));
    let bound =
        bind_query(&[pos(int(), Value::I32(1)), output(pos(int(), Value::I32(2)))]).unwrap();
    assert!(!bound[0].output);
    assert!(bound[1].output);
}

#[test]
fn a_name_matches_whatever_its_case_and_an_alias_matches_too() {
    let params = [SignatureParam::new("@stmt", SqlType::NVarChar(Len::Max))
        .required()
        .with_aliases(&["@statement"])];
    let bound = bind_with(
        &params,
        CallForm::Procedure,
        &[named("@STATEMENT", nvarchar(), text("q"))],
    )
    .unwrap();
    assert_eq!(bound[0].name, "@stmt");
    assert_eq!(bound[0].value, text("q"));
}

#[test]
fn a_procedure_reports_its_errors_in_order() {
    // 119 before 8144.
    let late_positional = [
        named("@table_name", nvarchar(), text("a")),
        pos(int(), Value::I32(1)),
        pos(int(), Value::I32(1)),
        pos(int(), Value::I32(1)),
    ];
    assert_eq!(code(bind(&late_positional)).0, 119);
    // 201 before 8145 and 8143.
    assert_eq!(code(bind(&[named("@bogus", int(), Value::I32(1))])).0, 201);
    assert_eq!(
        code(bind(&[
            named("@table_owner", nvarchar(), text("a")),
            named("@table_owner", nvarchar(), text("b")),
        ]))
        .0,
        201
    );
    // 201 before a conversion error.
    assert_eq!(
        code(bind(&[named("@odbcver", varchar(), text("x"))])).0,
        201
    );
    // Parameter by parameter in declaration order: 8162 of the first parameter before the
    // conversion of the third, whatever the order of the arguments.
    assert_eq!(
        code(bind(&[
            named("@odbcver", varchar(), text("x")),
            output(named("@table_name", nvarchar(), text("a"))),
        ]))
        .0,
        8162
    );
    // The conversion of an earlier parameter before 8162 of a later one.
    assert_eq!(
        code(bind_with(
            PLAIN,
            CallForm::Procedure,
            &[
                named("@a", varchar(), text("x")),
                output(named("@b", int(), Value::I32(1))),
            ],
        )),
        (8114, 5)
    );
    // 8162 before an unknown name written earlier.
    assert_eq!(
        code(bind(&[
            named("@bogus", int(), Value::I32(1)),
            output(named("@table_name", nvarchar(), text("a"))),
        ]))
        .0,
        8162
    );
    // A conversion error before an unknown name written earlier.
    assert_eq!(
        code(bind(&[
            named("@table_name", nvarchar(), text("t")),
            named("@bogus", int(), Value::I32(1)),
            named("@odbcver", varchar(), text("x")),
        ]))
        .0,
        8114
    );
    // The `OUTPUT` of the argument given twice is not read; the duplicate is.
    assert_eq!(
        code(bind(&[
            named("@table_name", nvarchar(), text("a")),
            output(named("@table_name", nvarchar(), text("b"))),
        ]))
        .0,
        8143
    );
    // Between 8145 and 8143, the earlier argument wins.
    assert_eq!(
        code(bind(&[
            named("@table_name", nvarchar(), text("a")),
            named("@bogus", int(), Value::I32(1)),
            named("@table_name", nvarchar(), text("b")),
        ]))
        .0,
        8145
    );
    assert_eq!(
        code(bind(&[
            named("@table_name", nvarchar(), text("a")),
            named("@table_name", nvarchar(), text("b")),
            named("@bogus", int(), Value::I32(1)),
        ]))
        .0,
        8143
    );
}

/// Two required `int` parameters, neither `OUTPUT`.
const PLAIN: &[SignatureParam<'static>] = &[
    SignatureParam::new("@a", SqlType::Int).required(),
    SignatureParam::new("@b", SqlType::Int).required(),
];

fn bind_plain(args: &[Arg]) -> (u32, u8) {
    code(bind_with(
        PLAIN,
        CallForm::ParameterizedQuery("(@a int, @b int)SELECT 1"),
        args,
    ))
}

#[test]
fn a_parameterized_query_reports_its_errors_in_order() {
    // 8144 before 8143 when the duplicate makes one argument too many.
    assert_eq!(
        code(bind_query(&[
            named("@a", int(), Value::I32(1)),
            named("@a", int(), Value::I32(2)),
            named("@b", int(), Value::I32(3)),
        ])),
        (8144, 2)
    );
    // 8178, not 8143 nor 8145, while a parameter is unsupplied.
    let err = bind_query(&[
        named("@a", int(), Value::I32(1)),
        named("@a", int(), Value::I32(2)),
    ])
    .unwrap_err();
    assert_eq!((err.number, err.state), (8178, 1));
    assert!(err.message.contains("@b"), "{}", err.message);
    assert!(
        err.message.contains("(@a int, @b int OUTPUT)SELECT 1"),
        "{}",
        err.message
    );
    // `DEFAULT` leaves its parameter unsupplied.
    assert_eq!(code(bind_query(&[default_arg(Some("@a"))])).0, 8178);
    // Parameter by parameter in declaration order, whatever the order of the arguments.
    // The first parameter fails its conversion before the second refuses `OUTPUT`.
    assert_eq!(
        bind_plain(&[
            output(named("@b", int(), Value::I32(1))),
            named("@a", varchar(), text("x")),
        ]),
        (8114, 5)
    );
    // The first parameter is missing before the second refuses `OUTPUT`.
    assert_eq!(
        bind_plain(&[
            named("@zz", int(), Value::I32(1)),
            output(named("@b", int(), Value::I32(1))),
        ]),
        (8178, 1)
    );
    // The first parameter is missing before the second fails its conversion.
    assert_eq!(bind_plain(&[named("@b", varchar(), text("x"))]), (8178, 1));
    // `DEFAULT` on the first parameter before `OUTPUT` on the second.
    assert_eq!(
        bind_plain(&[default_arg(None), output(pos(int(), Value::I32(1)))]),
        (8178, 1)
    );
    // The first parameter refuses `OUTPUT` before the second fails its conversion.
    assert_eq!(
        bind_plain(&[
            output(named("@a", int(), Value::I32(1))),
            named("@b", varchar(), text("x")),
        ]),
        (8162, 2)
    );
    // 8162 of the second parameter once the first binds.
    assert_eq!(
        bind_plain(&[
            output(named("@b", int(), Value::I32(1))),
            named("@a", int(), Value::I32(1)),
        ]),
        (8162, 2)
    );
}

#[test]
fn a_parameterized_query_reports_8145_and_8143_once_every_parameter_is_supplied() {
    let with_default = [
        SignatureParam::new("@a", SqlType::Int).required(),
        SignatureParam::new("@b", SqlType::Int).with_default(ParamDefault::Int(2)),
    ];
    let form = CallForm::ParameterizedQuery("");
    let unknown = bind_with(
        &with_default,
        form,
        &[
            named("@a", int(), Value::I32(1)),
            named("@c", int(), Value::I32(3)),
        ],
    )
    .unwrap_err();
    assert_eq!((unknown.number, unknown.state), (8145, 1));
    assert_eq!(
        code(bind_with(
            &with_default,
            form,
            &[
                named("@a", int(), Value::I32(1)),
                named("@a", int(), Value::I32(3))
            ],
        )),
        (8143, 1)
    );
}

#[test]
fn unicode_text_is_nchar_or_nvarchar() {
    assert!(is_unicode_text(&nvarchar()));
    assert!(is_unicode_text(&TypeInfo::new(
        SqlType::NChar(Len::Fixed(3)),
        true
    )));
    assert!(!is_unicode_text(&varchar()));
    assert!(!is_unicode_text(&int()));
}
