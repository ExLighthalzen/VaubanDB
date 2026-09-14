//! The contract `call.rs` leans on for `CAST`, `CONVERT` and function calls, checked from
//! outside the crate.
//!
//! # Why the binding tests themselves are not here
//!
//! `bind_cast`, `bind_convert`, `bind_function`, `bind_variable_function` and
//! `bind_niladic` are `pub(crate)`: `pub` is for what the crate exposes. An integration
//! test sees none of them, so the tests of the five functions live next to them, in
//! `src/call.rs`.
//!
//! `vauban-compat` is **not** a development dependency of `vauban-binder`: `@@VERSION`
//! and `SERVERPROPERTY` are therefore absent from the registry these tests see, and no
//! test names them.
//!
//! # What this file does check
//!
//! The three contracts `call.rs` reads as givens, from the outside: the registry types a
//! call and the binder adds nothing, the error catalogue spells the messages, and a type
//! names itself in a message the way message 529 prints it. If any of the three changes,
//! the binder is a crate that has to change with it, and this file says so.

use vauban_errors::SqlError;
use vauban_sysfn::{check_call, lookup, register_builtins};
use vauban_types::{Len, SqlType, TypeInfo};

/// `check_call` answers 174 and 189 with the message the binder passes on untouched.
///
/// The binder adds a line and nothing else: no wording of an arity error is written in
/// `call.rs`, and the lower-cased name is `check_call`'s decision.
#[test]
fn check_call_owns_the_arity_errors() {
    register_builtins();
    let isnull = lookup("ISNULL").expect("ISNULL is registered");
    let error = check_call(isnull, &[]).expect_err("ISNULL takes two arguments");
    assert_eq!(error.number, 174);
    assert_eq!(error.severity, 15);
    assert_eq!(
        error.message,
        "The function isnull takes exactly 2 argument(s)."
    );
    // The binder sets no line itself; the one it adds is the line of the AST node.
    assert_eq!(error.line, 0);

    let charindex = lookup("CHARINDEX").expect("CHARINDEX is registered");
    let error = check_call(charindex, &[]).expect_err("CHARINDEX takes two or three");
    assert_eq!(error.number, 189);
    assert_eq!(error.severity, 15);
}

/// A `@@`-prefixed name is a registry key like any other, matched case-insensitively.
///
/// `bind_variable_function` hands the name over with its `@@`, and relies on both halves of
/// that: the prefix is part of the key, and `@@spid` finds `@@SPID`.
#[test]
fn the_registry_holds_the_global_variables() {
    register_builtins();
    let spid = lookup("@@spid").expect("@@SPID is registered");
    assert_eq!(spid.name, "@@SPID");
    assert_eq!(
        check_call(spid, &[]).expect("@@SPID takes no argument").ty,
        SqlType::SmallInt
    );
    // Without the prefix it is not a name at all.
    assert!(lookup("SPID").is_none());
}

/// The catalogue spells the messages of the binder, arguments included.
///
/// `call.rs` builds none of them by hand: it calls these constructors and adds the line.
#[test]
fn the_catalogue_spells_the_messages_of_the_binder() {
    // SELECT NO_SUCH_FN(1);
    assert_eq!(
        SqlError::not_a_recognized_name("NO_SUCH_FN", "built-in function").message,
        "'NO_SUCH_FN' is not a known built-in function name."
    );
    // SELECT no_such_fn(1); — the name is printed as written, so the constructor must not
    // normalise it.
    assert_eq!(
        SqlError::not_a_recognized_name("no_such_fn", "built-in function").message,
        "'no_such_fn' is not a known built-in function name."
    );

    // SELECT CAST(NEWID() AS int);
    let conversion = SqlError::explicit_conversion_not_allowed("uniqueidentifier", "int");
    assert_eq!(conversion.number, 529);
    assert_eq!(conversion.severity, 16);
    assert_eq!(conversion.state, 1);
    assert_eq!(
        conversion.message,
        "No explicit conversion exists from uniqueidentifier to int."
    );

    // SELECT @@NO_SUCH; — 137, and not the 195 of an unknown function name.
    let variable = SqlError::must_declare_scalar_variable("@@NO_SUCH");
    assert_eq!(variable.number, 137);
    assert_eq!(variable.severity, 15);
    assert_eq!(variable.state, 2);
    assert_eq!(
        variable.message,
        "The scalar variable \"@@NO_SUCH\" is not declared."
    );

    // SELECT PATINDEX('%bc%', NULL); — the type is named `NULL`.
    assert_eq!(
        SqlError::invalid_argument_type("NULL", 2, "patindex").message,
        "Data type NULL is not accepted for argument 2 of the patindex function."
    );
}

/// The source and target spelling survives the real binding path.
#[test]
fn message_529_preserves_exact_numeric_spelling() {
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_parser::{ParseOptions, parse_batch};

    register_builtins();
    for numeric in ["decimal", "numeric"] {
        for (temporal, value) in [
            ("date", "2020-01-01"),
            ("time(3)", "12:00:00"),
            ("datetime2(3)", "2020-01-01"),
            ("datetimeoffset(3)", "2020-01-01"),
        ] {
            let name = temporal.split('(').next().unwrap();
            for (sql, from, to) in [
                (
                    format!("SELECT CAST(CAST(1 AS {numeric}(18,6)) AS {temporal});"),
                    numeric,
                    name,
                ),
                (
                    format!("SELECT CAST(CAST('{value}' AS {temporal}) AS {numeric}(18,6));"),
                    name,
                    numeric,
                ),
            ] {
                let parsed = parse_batch(&sql, &ParseOptions::default()).unwrap();
                let ctx = BindContext::scalar(&sql, SessionOptions::default());
                let error = bind(&parsed.statements[0], &ctx).unwrap_err();
                assert_eq!(
                    error,
                    SqlError::explicit_conversion_not_allowed(from, to).with_line(1),
                    "{sql}"
                );
            }
        }
    }
}

/// A conversion keeps the collation of its source when both sides are character types, and
/// `TypeInfo::new` is what decides that a non-character type carries none.
#[test]
fn only_a_character_type_carries_a_collation() {
    assert!(
        TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), false)
            .collation
            .is_some()
    );
    assert!(TypeInfo::new(SqlType::Int, false).collation.is_none());
    assert!(
        TypeInfo::new(SqlType::VarBinary(Len::Fixed(30)), false)
            .collation
            .is_none()
    );
}

/// The `NULL` written in a conversion and a NULL *valued* `int` take two paths.
///
/// The two refused sources are what separates the rule from a rule on the value:
/// `CAST(NULL AS int)` and `CASE WHEN 1=1 THEN NULL ELSE 1 END` both evaluate to a NULL of
/// type `int`, like the bare constant, and both answer 529 on the five targets a
/// conversion refuses from `int`. A rule written on the *value* would let them through.
/// The two witness targets below answer a typed NULL for the five sources alike.
#[test]
fn untyped_null_conversions_preserve_the_target_type() {
    use vauban_binder::{BindContext, BoundStatement, SessionOptions, bind};
    use vauban_parser::{ParseOptions, parse_batch};

    register_builtins();
    /// The three written sources the five refused targets accept.
    const WRITTEN_NULL: [&str; 3] = ["NULL", "((NULL))", "+NULL"];
    /// Two written sources whose value is a NULL of type `int`, refused on those targets.
    const TYPED_NULL: [&str; 2] = ["CAST(NULL AS int)", "CASE WHEN 1=1 THEN NULL ELSE 1 END"];

    let refused_from_int = [
        ("date", SqlType::Date),
        ("time(7)", SqlType::Time(7)),
        ("datetime2(7)", SqlType::DateTime2(7)),
        ("datetimeoffset(7)", SqlType::DateTimeOffset(7)),
        ("uniqueidentifier", SqlType::UniqueIdentifier),
    ];
    // The two targets a conversion from `int` already allowed: the guard changes nothing
    // there, so the five sources above answer the target type.
    let witnesses = [
        ("int", SqlType::Int),
        ("varchar(20)", SqlType::VarChar(Len::Fixed(20))),
    ];

    let bind_one = |source: &str, target: &str, mode: &str| {
        let sql = if mode.ends_with("CAST") {
            format!("SELECT {mode}({source} AS {target});")
        } else {
            format!("SELECT {mode}({target}, {source});")
        };
        let parsed = parse_batch(&sql, &ParseOptions::default()).expect("the corpus parses");
        let ctx = BindContext::scalar(&sql, SessionOptions::default());
        (sql.clone(), bind(&parsed.statements[0], &ctx))
    };
    let typed_null = |sql: &str, result: Result<BoundStatement, SqlError>, expected: &SqlType| {
        // The pattern is not irrefutable: `BoundStatement` has other variants than
        // `Query`, and the statements of this file are `SELECT`s.
        let BoundStatement::Query(plan) = result.expect(sql) else {
            panic!("expected a bound query: {sql}");
        };
        assert_eq!(&plan.schema().columns[0].ty.ty, expected, "{sql}");
        assert!(plan.schema().columns[0].ty.nullable, "{sql}");
    };

    for mode in ["CAST", "TRY_CAST", "CONVERT", "TRY_CONVERT"] {
        for (target, expected) in &refused_from_int {
            for source in WRITTEN_NULL {
                let (sql, result) = bind_one(source, target, mode);
                typed_null(&sql, result, expected);
            }
            for source in TYPED_NULL {
                let (sql, result) = bind_one(source, target, mode);
                assert_eq!(
                    result.expect_err(&sql),
                    SqlError::explicit_conversion_not_allowed("int", expected.name()).with_line(1),
                    "{sql}"
                );
            }
        }
        for (target, expected) in &witnesses {
            for source in WRITTEN_NULL.iter().chain(TYPED_NULL.iter()) {
                let (sql, result) = bind_one(source, target, mode);
                typed_null(&sql, result, expected);
            }
        }
    }
}
