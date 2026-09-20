use super::*;
use crate::builtins::check_call;
use crate::context::StaticContext;
use crate::{lookup, register_builtins};
use vauban_types::{Date, DateTime, DateTime2, Time};

/// The fourteen definitions this file registers, in registration order.
const DEFS: [&FunctionDef; 14] = [
    &OBJECT_ID_DEF,
    &OBJECT_NAME_DEF,
    &SCHEMA_NAME_DEF,
    &SCHEMA_ID_DEF,
    &DB_ID_DEF,
    &USER_NAME_DEF,
    &SUSER_SNAME_DEF,
    &HOST_NAME_DEF,
    &APP_NAME_DEF,
    &IDENTITY_DEF,
    &SCOPE_IDENTITY_DEF,
    &IDENT_CURRENT_DEF,
    &ISNUMERIC_DEF,
    &ISDATE_DEF,
];

/// A context with a catalogue of one table, `dbo.t` = 42, one schema and one
/// database, plus the three identity values: enough to prove each function reads
/// the context and not a constant.
struct CatalogContext;

impl EvalContext for CatalogContext {
    fn now_local(&self) -> DateTime2 {
        DateTime2 {
            date: Date { days: 0 },
            time: Time { ticks_100ns: 0 },
        }
    }

    fn now_utc(&self) -> DateTime2 {
        self.now_local()
    }

    fn rowcount(&self) -> i64 {
        0
    }

    fn last_identity(&self) -> Option<Decimal> {
        Some(Decimal {
            mantissa: 7,
            precision: 10,
            scale: 0,
        })
    }

    fn spid(&self) -> i16 {
        0
    }

    fn current_database(&self) -> &str {
        "master"
    }

    fn server_name(&self) -> &str {
        ""
    }

    fn object_id(&self, name: &str) -> Option<i32> {
        (name == "dbo.t").then_some(42)
    }

    fn object_name(&self, id: i32) -> Option<String> {
        (id == 42).then(|| "dbo.t".to_owned())
    }

    fn variable(&self, _name: &str) -> Option<Value> {
        None
    }

    fn database_id(&self, name: Option<&str>) -> Option<i32> {
        match name {
            None | Some("master") => Some(1),
            Some(_) => None,
        }
    }

    fn schema_name(&self, id: i32) -> Option<String> {
        (id == 1).then(|| "dbo".to_owned())
    }

    fn schema_id(&self, name: Option<&str>) -> Option<i32> {
        match name {
            None | Some("dbo") => Some(1),
            Some(_) => None,
        }
    }

    fn scope_identity(&self) -> Option<Decimal> {
        Some(Decimal {
            mantissa: 8,
            precision: 10,
            scale: 0,
        })
    }

    fn ident_current(&self, table: &str) -> Option<Decimal> {
        (table == "dbo.t").then_some(Decimal {
            mantissa: 9,
            precision: 10,
            scale: 0,
        })
    }
}

fn text(s: &str) -> Value {
    string(s)
}

/// `varchar(100)`, the type of a string literal argument in these tests.
fn varchar() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(100)), true)
}

/// The declared type a test gives each argument: `varchar(100)` for a string,
/// `datetime` for a datetime, `int` for anything else.
fn type_of(value: &Value) -> TypeInfo {
    match value {
        Value::String(_) => varchar(),
        Value::DateTime(_) => TypeInfo::new(SqlType::DateTime, true),
        _ => TypeInfo::new(SqlType::Int, true),
    }
}

/// Evaluates `def` on `values` declared as `types`, through `check_call` first, as
/// the `binder` then the `executor` would.
fn eval_typed(
    def: &FunctionDef,
    values: &[Value],
    types: &[TypeInfo],
    ctx: &dyn EvalContext,
) -> SqlResult<Value> {
    let result = check_call(def, types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, ctx)
}

fn try_eval(def: &FunctionDef, values: &[Value], ctx: &dyn EvalContext) -> SqlResult<Value> {
    let types: Vec<TypeInfo> = values.iter().map(type_of).collect();
    eval_typed(def, values, &types, ctx)
}

fn eval(def: &FunctionDef, values: &[Value], ctx: &dyn EvalContext) -> Value {
    try_eval(def, values, ctx).expect("eval must succeed")
}

/// Evaluates the registered function `name` on `values`.
fn call(name: &str, values: &[Value], ctx: &dyn EvalContext) -> Value {
    register_builtins();
    let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
    eval(def, values, ctx)
}

#[test]
fn all_fourteen_are_registered_as_scalar() {
    register_builtins();
    for def in DEFS {
        let found = lookup(def.name).unwrap_or_else(|| panic!("{} missing", def.name));
        assert_eq!(found.name, def.name);
        assert_eq!(found.kind, FunctionKind::Scalar, "{}", def.name);
        assert!(found.aggregate.is_none(), "{}", def.name);
        assert_eq!(
            found.deterministic,
            def.name == "ISNUMERIC",
            "{} determinism",
            def.name
        );
    }
    // Lookup is case-insensitive, `@@` prefix included.
    assert!(lookup("object_id").is_some());
    assert!(lookup("@@identity").is_some());
}

#[test]
fn object_functions_read_the_context() {
    let ctx = CatalogContext;
    assert_eq!(call("OBJECT_ID", &[text("dbo.t")], &ctx), Value::I32(42));
    assert_eq!(call("OBJECT_NAME", &[Value::I32(42)], &ctx), text("dbo.t"));
    assert_eq!(call("SCHEMA_NAME", &[Value::I32(1)], &ctx), text("dbo"));
    assert_eq!(call("DB_ID", &[], &ctx), Value::I16(1));
    assert_eq!(call("DB_ID", &[text("master")], &ctx), Value::I16(1));
    assert_eq!(
        call("IDENT_CURRENT", &[text("dbo.t")], &ctx),
        Value::Decimal(Decimal {
            mantissa: 9,
            precision: 38,
            scale: 0
        })
    );
    assert_eq!(
        call("SCOPE_IDENTITY", &[], &ctx),
        Value::Decimal(Decimal {
            mantissa: 8,
            precision: 38,
            scale: 0
        })
    );
    assert_eq!(
        call("@@IDENTITY", &[], &ctx),
        Value::Decimal(Decimal {
            mantissa: 7,
            precision: 38,
            scale: 0
        })
    );
}

#[test]
fn object_functions_return_null_when_unknown() {
    let ctx = StaticContext::default();
    assert_eq!(call("OBJECT_ID", &[text("dbo.no_such")], &ctx), Value::Null);
    assert_eq!(call("OBJECT_NAME", &[Value::I32(1)], &ctx), Value::Null);
    assert_eq!(call("SCHEMA_NAME", &[Value::I32(99)], &ctx), Value::Null);
    assert_eq!(call("DB_ID", &[text("no_such_db")], &ctx), Value::Null);
    assert_eq!(call("DB_ID", &[], &ctx), Value::Null);
    assert_eq!(call("IDENT_CURRENT", &[text("t")], &ctx), Value::Null);
    assert_eq!(call("SCOPE_IDENTITY", &[], &ctx), Value::Null);
    assert_eq!(call("@@IDENTITY", &[], &ctx), Value::Null);
    // A filled catalogue does not change the answer for what it does not hold.
    let known = CatalogContext;
    assert_eq!(call("OBJECT_ID", &[text("dbo.other")], &known), Value::Null);
    assert_eq!(call("OBJECT_NAME", &[Value::I32(43)], &known), Value::Null);
    assert_eq!(call("SCHEMA_NAME", &[Value::I32(2)], &known), Value::Null);
    assert_eq!(call("DB_ID", &[text("no_such_db")], &known), Value::Null);
    assert_eq!(
        call("IDENT_CURRENT", &[text("dbo.other")], &known),
        Value::Null
    );
}

/// `SELECT OBJECT_ID(NULL), OBJECT_NAME(NULL), SCHEMA_NAME(NULL), DB_ID(NULL),
/// IDENT_CURRENT(NULL), USER_NAME(NULL), SUSER_SNAME(NULL);` is a row of `NULL`s on
/// SQL Server. `SCHEMA_NAME(NULL)`, `DB_ID(NULL)` and `USER_NAME(NULL)` are the
/// vectors that distinguish "propagated" from "ignored": without an argument the same
/// functions answer `dbo`, `1` and `dbo`.
#[test]
fn null_argument_gives_null() {
    let ctx = StaticContext {
        login_name: Some("sa".to_owned()),
        ..StaticContext::default()
    };
    assert_eq!(
        call("OBJECT_ID", &[Value::Null], &CatalogContext),
        Value::Null
    );
    assert_eq!(
        call("OBJECT_NAME", &[Value::Null], &CatalogContext),
        Value::Null
    );
    assert_eq!(
        call("SCHEMA_NAME", &[Value::Null], &CatalogContext),
        Value::Null
    );
    assert_eq!(call("DB_ID", &[Value::Null], &CatalogContext), Value::Null);
    assert_eq!(
        call("IDENT_CURRENT", &[Value::Null], &CatalogContext),
        Value::Null
    );
    assert_eq!(call("USER_NAME", &[Value::Null], &ctx), Value::Null);
    assert_eq!(call("SUSER_SNAME", &[Value::Null], &ctx), Value::Null);
    // The same calls without the `NULL` do answer.
    assert_eq!(call("SCHEMA_NAME", &[], &CatalogContext), text("dbo"));
    assert_eq!(call("DB_ID", &[], &CatalogContext), Value::I16(1));
    assert_eq!(call("USER_NAME", &[], &ctx), text("dbo"));
    assert_eq!(call("SUSER_SNAME", &[], &ctx), text("sa"));
}

#[test]
fn object_id_accepts_and_ignores_the_type_argument() {
    let ctx = CatalogContext;
    let one = call("OBJECT_ID", &[text("dbo.t")], &ctx);
    let two = call("OBJECT_ID", &[text("dbo.t"), text("U")], &ctx);
    assert_eq!(one, Value::I32(42));
    assert_eq!(two, one);
    // `OBJECT_ID('sys.objects', NULL)` is `NULL` on SQL Server where `'V'` finds it.
    assert_eq!(
        call("OBJECT_ID", &[text("dbo.t"), Value::Null], &ctx),
        Value::Null
    );
    // Same rule for the database identifier of `OBJECT_NAME`.
    assert_eq!(
        call("OBJECT_NAME", &[Value::I32(42), Value::I32(1)], &ctx),
        text("dbo.t")
    );
    assert_eq!(
        call("OBJECT_NAME", &[Value::I32(42), Value::Null], &ctx),
        Value::Null
    );

    let one_arg = [varchar()];
    let two_args = [varchar(), varchar()];
    let three_args = [varchar(), varchar(), varchar()];
    assert!(check_call(&OBJECT_ID_DEF, &one_arg).is_ok());
    assert!(check_call(&OBJECT_ID_DEF, &two_args).is_ok());
    let err = check_call(&OBJECT_ID_DEF, &three_args).expect_err("three arguments");
    assert_eq!(err.number, 189);
    assert_eq!(
        err.message,
        "The function object_id takes between 1 and 2 arguments."
    );
    let err = check_call(&OBJECT_ID_DEF, &[]).expect_err("no argument");
    assert_eq!(err.number, 189);
    assert!(check_call(&OBJECT_NAME_DEF, &two_args).is_ok());
    assert_eq!(
        check_call(&OBJECT_NAME_DEF, &three_args)
            .expect_err("three arguments")
            .number,
        189
    );
}

#[test]
fn schema_name_without_argument_is_dbo() {
    assert_eq!(
        call("SCHEMA_NAME", &[], &StaticContext::default()),
        text("dbo")
    );
    assert_eq!(call("SCHEMA_NAME", &[], &CatalogContext), text("dbo"));
}

/// `SCHEMA_ID()` and `SCHEMA_ID('dbo')` are the identifier the context holds for the
/// default schema: `1` in [`CatalogContext`], for both forms. The two calls go through
/// the context and read no constant: `StaticContext`, which resolves no schema, answers
/// `NULL` to the same two forms.
#[test]
fn schema_id_dbo_is_some() {
    let ctx = CatalogContext;
    assert_eq!(call("SCHEMA_ID", &[], &ctx), Value::I32(1));
    assert_eq!(call("SCHEMA_ID", &[text("dbo")], &ctx), Value::I32(1));
    let empty = StaticContext::default();
    assert_eq!(call("SCHEMA_ID", &[], &empty), Value::Null);
    assert_eq!(call("SCHEMA_ID", &[text("dbo")], &empty), Value::Null);
}

/// A name the context resolves nothing for is `NULL`
/// (`SCHEMA_ID('no_such_schema')`), and so is a `NULL` argument where the same call
/// without an argument answers `1`: the vector that separates "argument propagated"
/// from "argument ignored". A number is converted to a name rather than refused, and
/// no schema of the context is named `1`.
#[test]
fn schema_id_unknown_is_null() {
    let ctx = CatalogContext;
    assert_eq!(
        call("SCHEMA_ID", &[text("no_such_schema")], &ctx),
        Value::Null
    );
    assert_eq!(call("SCHEMA_ID", &[Value::Null], &ctx), Value::Null);
    assert_eq!(call("SCHEMA_ID", &[Value::I32(1)], &ctx), Value::Null);
    assert_eq!(call("SCHEMA_ID", &[], &ctx), Value::I32(1));
}

#[test]
fn environment_functions_read_the_context() {
    let ctx = StaticContext {
        host_name: Some("APP01".to_owned()),
        app_name: Some("sqlcmd".to_owned()),
        login_name: Some("sa".to_owned()),
        user_name: Some("dbo".to_owned()),
        ..StaticContext::default()
    };
    assert_eq!(call("HOST_NAME", &[], &ctx), text("APP01"));
    assert_eq!(call("APP_NAME", &[], &ctx), text("sqlcmd"));
    assert_eq!(call("SUSER_SNAME", &[], &ctx), text("sa"));
    assert_eq!(call("USER_NAME", &[], &ctx), text("dbo"));
    // A non-`NULL` argument is accepted and ignored.
    assert_eq!(call("USER_NAME", &[Value::I32(1)], &ctx), text("dbo"));
    assert_eq!(call("SUSER_SNAME", &[Value::I32(1)], &ctx), text("sa"));

    let empty = StaticContext::default();
    assert_eq!(call("HOST_NAME", &[], &empty), Value::Null);
    assert_eq!(call("APP_NAME", &[], &empty), Value::Null);
    assert_eq!(call("SUSER_SNAME", &[], &empty), Value::Null);
    assert_eq!(call("USER_NAME", &[], &empty), text("dbo"));

    // A user the context names that is not `dbo` is the one answered: `dbo` is a
    // fallback, not a constant.
    let other = StaticContext {
        user_name: Some("guest".to_owned()),
        ..StaticContext::default()
    };
    assert_eq!(call("USER_NAME", &[], &other), text("guest"));
}

#[test]
fn arities_follow_sql_server() {
    for def in [
        &HOST_NAME_DEF,
        &APP_NAME_DEF,
        &IDENTITY_DEF,
        &SCOPE_IDENTITY_DEF,
    ] {
        let err = check_call(def, &[varchar()]).expect_err("must refuse an argument");
        assert_eq!(err.number, 174, "{}", def.name);
    }
    let err = check_call(&IDENT_CURRENT_DEF, &[]).expect_err("must require the table");
    assert_eq!(err.number, 174);
    assert_eq!(
        err.message,
        "The function ident_current takes exactly 1 argument(s)."
    );
    for def in [&ISNUMERIC_DEF, &ISDATE_DEF] {
        assert_eq!(check_call(def, &[]).expect_err("no argument").number, 174);
        assert_eq!(
            check_call(def, &[varchar(), varchar()])
                .expect_err("two arguments")
                .number,
            174
        );
    }
    for def in [
        &SCHEMA_NAME_DEF,
        &SCHEMA_ID_DEF,
        &DB_ID_DEF,
        &USER_NAME_DEF,
        &SUSER_SNAME_DEF,
    ] {
        assert!(check_call(def, &[]).is_ok(), "{}", def.name);
        assert!(check_call(def, &[varchar()]).is_ok(), "{}", def.name);
        let err = check_call(def, &[varchar(), varchar()]).expect_err("two arguments");
        assert_eq!(err.number, 189, "{}", def.name);
    }
    // The 189 of a range arity; 174 is the number of an exact arity, which `SCHEMA_ID`
    // does not have.
    let err = check_call(&SCHEMA_ID_DEF, &[varchar(), varchar()]).expect_err("two arguments");
    assert_eq!(
        err.message,
        "The function schema_id takes between 0 and 1 arguments."
    );
}

#[test]
fn identity_return_type_is_numeric_38_0() {
    let expected = TypeInfo::new(
        SqlType::Numeric {
            precision: 38,
            scale: 0,
        },
        true,
    );
    assert_eq!(check_call(&IDENTITY_DEF, &[]).unwrap(), expected);
    assert_eq!(check_call(&SCOPE_IDENTITY_DEF, &[]).unwrap(), expected);
    assert_eq!(
        check_call(&IDENT_CURRENT_DEF, &[varchar()]).unwrap(),
        expected
    );
}

#[test]
fn return_types_are_exact() {
    let int = TypeInfo::new(SqlType::Int, true);
    let smallint = TypeInfo::new(SqlType::SmallInt, true);
    let name = TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true);
    let predicate = TypeInfo::new(SqlType::Int, false);
    assert_eq!(check_call(&OBJECT_ID_DEF, &[varchar()]).unwrap(), int);
    // `int` here where `DB_ID` is a `smallint`.
    assert_eq!(check_call(&SCHEMA_ID_DEF, &[]).unwrap(), int);
    assert_eq!(check_call(&SCHEMA_ID_DEF, &[varchar()]).unwrap(), int);
    // `smallint`, against the `int` of Microsoft Learn: see `smallint_type`.
    assert_eq!(check_call(&DB_ID_DEF, &[]).unwrap(), smallint);
    assert_eq!(check_call(&DB_ID_DEF, &[varchar()]).unwrap(), smallint);
    for def in [
        &OBJECT_NAME_DEF,
        &SCHEMA_NAME_DEF,
        &USER_NAME_DEF,
        &SUSER_SNAME_DEF,
    ] {
        assert_eq!(
            check_call(def, std::slice::from_ref(&int)).unwrap(),
            name,
            "{}",
            def.name
        );
    }
    assert_eq!(check_call(&HOST_NAME_DEF, &[]).unwrap(), name);
    assert_eq!(check_call(&APP_NAME_DEF, &[]).unwrap(), name);
    assert_eq!(check_call(&ISNUMERIC_DEF, &[varchar()]).unwrap(), predicate);
    assert_eq!(check_call(&ISDATE_DEF, &[varchar()]).unwrap(), predicate);
}

/// An `int` argument where a name is expected is converted, not refused:
/// `SELECT OBJECT_ID(1);` is `NULL` on SQL Server. A name where an `int` is expected is
/// converted too, with the conversion's own error: `SELECT OBJECT_NAME('abc');` is 245
/// and `SELECT OBJECT_NAME(CAST(99999999999 AS bigint));` is 8115.
#[test]
fn arguments_are_converted_like_sql_server_does() {
    let ctx = CatalogContext;
    assert_eq!(call("OBJECT_ID", &[Value::I32(1)], &ctx), Value::Null);
    assert_eq!(call("OBJECT_NAME", &[text("42")], &ctx), text("dbo.t"));
    let err = try_eval(&OBJECT_NAME_DEF, &[text("abc")], &ctx).expect_err("245");
    assert_eq!(err.number, 245);
    let bigint = [TypeInfo::new(SqlType::BigInt, true)];
    let err = eval_typed(
        &OBJECT_NAME_DEF,
        &[Value::I64(99_999_999_999)],
        &bigint,
        &ctx,
    )
    .expect_err("an overflow");
    // SQL Server raises 8115 here; the number is the one `types` gives a `bigint` that
    // does not fit an `int` (220 today), and is not this function's to choose.
    assert!(matches!(err.number, 220 | 8115), "{}", err.message);
    // `SCHEMA_NAME(1.5)` answers what `SCHEMA_NAME(1)` answers.
    let decimal = Value::Decimal(Decimal {
        mantissa: 15,
        precision: 2,
        scale: 1,
    });
    let types = [TypeInfo::new(
        SqlType::Decimal {
            precision: 2,
            scale: 1,
        },
        true,
    )];
    assert_eq!(
        eval_typed(&SCHEMA_NAME_DEF, &[decimal], &types, &ctx).unwrap(),
        text("dbo")
    );
}

#[test]
fn isnumeric_edge_cases() {
    let ctx = StaticContext::default();
    let one = Value::I32(1);
    let zero = Value::I32(0);
    let mut mismatches = Vec::new();
    for (input, expected) in [
        ("123", &one),
        ("$5", &one),
        ("1e5", &one),
        ("1,000", &one),
        ("12a", &zero),
        ("", &zero),
        (" ", &zero),
        ("  ", &zero),
        // The rest is what the `money` and `float` readings cover.
        ("1E5", &one),
        ("1e+5", &one),
        ("1e-5", &one),
        ("1.5e5", &one),
        ("1e05", &one),
        ("1e308", &one),
        ("-1e308", &one),
        ("1e309", &zero),
        ("1e400", &zero),
        ("$1,000.50", &one),
        ("-$5", &one),
        ("$-5", &one),
        ("+$5", &one),
        ("$+5", &one),
        ("$ 5", &one),
        (".5", &one),
        ("5.", &one),
        ("+.5", &one),
        ("-.5", &one),
        ("01", &one),
        ("-0", &one),
        ("1,0", &one),
        ("1,00,0", &one),
        (",1", &one),
        ("1,", &one),
        (" 1 ", &one),
        (" 5", &one),
        ("5 ", &one),
        ("\t", &one),
        ("\n", &one),
        ("\t1", &one),
        ("99999999999999999999999999999999999999999", &one),
        ("922337203685477.5808", &one),
        ("1e", &zero),
        ("e5", &zero),
        ("e", &zero),
        ("0x10", &zero),
        ("1.5.5", &zero),
        ("1..", &zero),
        ("5$", &zero),
        ("$$5", &zero),
        ("$1e5", &zero),
        ("1e5$", &zero),
        ("1,000e5", &zero),
        ("++1", &zero),
        ("--1", &zero),
        ("1-", &zero),
        ("1+", &zero),
        ("1 000", &zero),
        ("1 e5", &zero),
        ("(5)", &zero),
        ("1_000", &zero),
        ("x1", &zero),
        ("\0", &zero),
    ] {
        let got = call("ISNUMERIC", &[text(input)], &ctx);
        if got != *expected {
            mismatches.push(format!(
                "ISNUMERIC({input:?}) = {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    assert_eq!(call("ISNUMERIC", &[Value::Null], &ctx), zero);
    assert_eq!(call("ISNUMERIC", &[Value::I32(1)], &ctx), one);

    // A typed `NULL` is `0` whatever the type, a number is `1` whatever the family.
    for (value, ty) in [
        (Value::Bit(true), SqlType::Bit),
        (Value::I8(1), SqlType::TinyInt),
        (Value::I16(1), SqlType::SmallInt),
        (Value::I64(1), SqlType::BigInt),
        (Value::F64(1.5), SqlType::Float),
        (Value::F32(1.5), SqlType::Real),
        (Value::Money(10_000), SqlType::Money),
        (Value::Money(10_000), SqlType::SmallMoney),
        (
            Value::Decimal(Decimal {
                mantissa: 1,
                precision: 38,
                scale: 10,
            }),
            SqlType::Numeric {
                precision: 38,
                scale: 10,
            },
        ),
    ] {
        let types = [TypeInfo::new(ty, true)];
        assert_eq!(
            eval_typed(&ISNUMERIC_DEF, &[value], &types, &ctx).unwrap(),
            one,
            "{ty:?}"
        );
        assert_eq!(
            eval_typed(&ISNUMERIC_DEF, &[Value::Null], &types, &ctx).unwrap(),
            zero,
            "NULL as {ty:?}"
        );
    }
    // `datetime`, a GUID and a binary are `0`.
    let datetime = Value::DateTime(DateTime {
        days: 0,
        ticks_300th: 0,
    });
    assert_eq!(call("ISNUMERIC", &[datetime], &ctx), zero);
    let guid = [TypeInfo::new(SqlType::UniqueIdentifier, true)];
    assert_eq!(
        eval_typed(&ISNUMERIC_DEF, &[Value::Guid([0; 16])], &guid, &ctx).unwrap(),
        zero
    );
    let binary = [TypeInfo::new(SqlType::VarBinary(Len::Fixed(4)), true)];
    assert_eq!(
        eval_typed(&ISNUMERIC_DEF, &[Value::Bytes(vec![1])], &binary, &ctx).unwrap(),
        zero
    );
    // An `nvarchar` reads the same way as a `varchar`.
    let nvarchar = [TypeInfo::new(SqlType::NVarChar(Len::Max), true)];
    assert_eq!(
        eval_typed(&ISNUMERIC_DEF, &[text("$5")], &nvarchar, &ctx).unwrap(),
        one
    );
    assert_eq!(
        eval_typed(&ISNUMERIC_DEF, &[text("")], &nvarchar, &ctx).unwrap(),
        zero
    );
}

/// The four date types are refused, `NULL` included, for both predicates, by
/// `check_call` and by the evaluation alike; `datetime` and `smalldatetime` are not.
#[test]
fn predicates_refuse_the_date_only_types() {
    let ctx = StaticContext::default();
    for (ty, name) in [
        (SqlType::Date, "date"),
        (SqlType::Time(7), "time"),
        (SqlType::DateTime2(7), "datetime2"),
        (SqlType::DateTimeOffset(7), "datetimeoffset"),
    ] {
        let types = [TypeInfo::new(ty, true)];
        for (def, function) in [(&ISDATE_DEF, "isdate"), (&ISNUMERIC_DEF, "isnumeric")] {
            let expected = format!(
                "Data type {name} is not accepted for argument 1 of the {function} function."
            );
            let err = check_call(def, &types).expect_err("8116 expected");
            assert_eq!(err.number, 8116);
            assert_eq!(err.message, expected);
            let args = EvalArgs {
                values: &[Value::Null],
                types: &types,
                result: &TypeInfo::new(SqlType::Int, false),
            };
            let err = (def.eval)(&args, &ctx).expect_err("8116 expected from eval");
            assert_eq!(err.number, 8116);
            assert_eq!(err.message, expected);
        }
    }
    for ty in [SqlType::DateTime, SqlType::SmallDateTime] {
        let types = [TypeInfo::new(ty, true)];
        assert!(check_call(&ISDATE_DEF, &types).is_ok());
        assert!(check_call(&ISNUMERIC_DEF, &types).is_ok());
    }
}

/// The money grammar spelled without a digit, and the `1d5` float spelling.
///
/// Each line carries the `CAST` that shows the shape belongs to `types`. `'--'` and
/// `'1f5'` are the counter-vectors: SQL Server refuses them in both `CAST(... AS
/// money)` and `CAST(... AS float)`, so `ISNUMERIC` answers `0`, and a rule looser
/// than the one `types` applies would turn them into `1`.
#[test]
fn isnumeric_follows_the_cast_on_the_money_and_float_shapes() {
    let ctx = StaticContext::default();
    let one = Value::I32(1);
    let zero = Value::I32(0);
    let mut mismatches = Vec::new();
    for (input, expected) in [
        // `CAST('$' AS money)`, `'+'`, `'-'`, `'.'`, `','`, `'-,'`, `'$-.'` are all
        // `0.0000` on SQL Server.
        ("$", &one),
        ("-", &one),
        ("+", &one),
        (".", &one),
        (",", &one),
        ("-,", &one),
        ("$-.", &one),
        // `CAST('1d5' AS float)` is `100000` on SQL Server.
        ("1d5", &one),
        // The counter-vectors: 235 towards `money` and 8114 towards `float`.
        ("--", &zero),
        ("$$", &zero),
        (".-$", &zero),
        ("1f5", &zero),
        ("1d5e2", &zero),
    ] {
        let got = call("ISNUMERIC", &[text(input)], &ctx);
        if got != *expected {
            mismatches.push(format!(
                "ISNUMERIC({input:?}) = {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// The `datetime` shapes `CAST` refuses: an offset or a `Z` after a space, more than
/// three fractional digits, a tabulation at either end.
///
/// Each line carries the `CAST` that shows the shape belongs to `types`.
#[test]
fn isdate_follows_the_cast_on_the_datetime_shapes() {
    let ctx = StaticContext::default();
    let zero = Value::I32(0);
    let mut mismatches = Vec::new();
    for (input, expected) in [
        // `CAST('2020-01-01 12:00:00+02:00' AS datetime)` and the same with a `T` are
        // 241 on SQL Server: an offset is refused when the target is `datetime`.
        ("2020-01-01 12:00:00+02:00", &zero),
        ("2020-01-01T12:00:00+02:00", &zero),
        // `CAST('2020-01-01 12:00:00Z' AS datetime)` is 241 where the same with a
        // `T` is read: a `Z` after a space is not a zone designator.
        ("2020-01-01 12:00:00Z", &zero),
        // `CAST('2020-01-01 12:00:00.1234' AS datetime)` is 241: more than three
        // fractional digits are refused when the target is `datetime`.
        ("2020-01-01 12:00:00.1234", &zero),
        ("2020-01-01 12:00:00.1234567", &zero),
        // `CAST(CHAR(9) + '2020-01-01' AS datetime)`, `CAST('2020-01-01' + CHAR(9) AS
        // datetime)` and `CAST(CHAR(9) AS datetime)` are 241: a tabulation at either
        // end is not trimmed.
        ("\t2020-01-01", &zero),
        ("2020-01-01\t", &zero),
        ("\t", &zero),
    ] {
        let got = call("ISDATE", &[text(input)], &ctx);
        if got != *expected {
            mismatches.push(format!(
                "ISDATE({input:?}) = {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn isdate_edge_cases() {
    let ctx = StaticContext::default();
    let one = Value::I32(1);
    let zero = Value::I32(0);
    let mut mismatches = Vec::new();
    for (input, expected) in [
        ("2020-01-01", &one),
        ("2020-13-01", &zero),
        ("abc", &zero),
        ("", &zero),
        (" ", &zero),
        // The rest is what the `datetime` reading covers.
        ("12:00", &one),
        ("2020-01-01 12:00:00", &one),
        ("1/2/2020", &one),
        ("30/1/2020", &zero),
        ("20200101", &one),
        ("9999-12-31", &one),
        ("10000-01-01", &zero),
        ("1753-01-01", &one),
        ("1752-12-31", &zero),
        ("0001-01-01", &zero),
        (" 2020-01-01 ", &one),
        ("2020", &one),
        ("12", &zero),
        ("12:00:00.123", &one),
        ("2020-01-01T12:00:00", &one),
        ("2020-01-01T12:00:00Z", &one),
        ("2020-02-30", &zero),
        ("jan 1 2020", &one),
        ("1 jan 2020", &one),
        ("2020/1/1", &one),
        ("01-02-2020", &one),
        ("2020.01.01", &one),
        ("9999-12-31 23:59:59.997", &one),
        ("9999-12-31 23:59:59.999", &zero),
        ("2020-01-01 24:00:00", &zero),
        ("2079-06-06 23:59:59", &one),
    ] {
        let got = call("ISDATE", &[text(input)], &ctx);
        if got != *expected {
            mismatches.push(format!(
                "ISDATE({input:?}) = {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    assert_eq!(call("ISDATE", &[Value::Null], &ctx), zero);

    // A `datetime` value is `1`, a `NULL` of that type `0`.
    let datetime = Value::DateTime(DateTime {
        days: 0,
        ticks_300th: 0,
    });
    assert_eq!(call("ISDATE", &[datetime], &ctx), one);
    let datetime_type = [TypeInfo::new(SqlType::DateTime, true)];
    assert_eq!(
        eval_typed(&ISDATE_DEF, &[Value::Null], &datetime_type, &ctx).unwrap(),
        zero
    );
    // A number is `0` without a conversion attempt, although `CAST(43000 AS datetime)`
    // is a date.
    assert_eq!(call("ISDATE", &[Value::I32(43_000)], &ctx), zero);
    for (value, ty) in [
        (Value::F64(1.5), SqlType::Float),
        (Value::Money(10_000), SqlType::Money),
        (Value::Bit(true), SqlType::Bit),
        (Value::I64(1), SqlType::BigInt),
        (Value::Guid([0; 16]), SqlType::UniqueIdentifier),
        (Value::Bytes(vec![0]), SqlType::VarBinary(Len::Fixed(4))),
    ] {
        let types = [TypeInfo::new(ty, true)];
        assert_eq!(
            eval_typed(&ISDATE_DEF, &[value], &types, &ctx).unwrap(),
            zero,
            "{ty:?}"
        );
    }

    // A `time(7)` argument is error 8116.
    let time = Value::Time(Time { ticks_100ns: 0 });
    let time_type = [TypeInfo::new(SqlType::Time(7), true)];
    let err = eval_typed(&ISDATE_DEF, &[time], &time_type, &ctx).expect_err("8116");
    assert_eq!(err.number, 8116);
    assert_eq!(
        err.message,
        "Data type time is not accepted for argument 1 of the isdate function."
    );
}
