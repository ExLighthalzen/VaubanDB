use super::*;
use crate::context::StaticContext;
use vauban_types::Decimal;

/// `10^38`, one past the widest `decimal` T-SQL declares.
const TEN_POW_38: i128 = 10i128.pow(38);

fn varchar(length: u16) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), true)
}

fn nvarchar(length: u16) -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Fixed(length)), true)
}

fn char_n(length: u16) -> TypeInfo {
    TypeInfo::new(SqlType::Char(Len::Fixed(length)), true)
}

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

/// Evaluates `def` the way the `executor` does: the result type of the call comes from
/// `return_type`, so a test never has to guess it.
fn eval(def: &FunctionDef, values: &[Value], types: &[TypeInfo]) -> SqlResult<Value> {
    let result = (def.return_type)(types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, &StaticContext::default())
}

/// Evaluates a one-argument string function on a `varchar(50)` argument.
fn eval_string(def: &FunctionDef, value: &str) -> SqlResult<Value> {
    eval(def, &[text(value)], &[varchar(50)])
}

/// Evaluates a two-argument function on a `varchar(50)` and an `int`.
fn eval_string_int(def: &FunctionDef, value: &str, count: i32) -> SqlResult<Value> {
    eval(
        def,
        &[text(value), Value::I32(count)],
        &[varchar(50), int()],
    )
}

/// Evaluates `DATALENGTH` on the `decimal(precision, scale)` value `mantissa / 10^scale`,
/// the way a `CAST` builds it: the mantissa carried by the value is already scaled to the
/// declared scale (`vauban_types::convert`).
fn datalength_of(mantissa: i128, precision: u8, scale: u8) -> SqlResult<Value> {
    let ty = TypeInfo::new(SqlType::Decimal { precision, scale }, true);
    eval(
        &DATALENGTH_DEF,
        &[Value::Decimal(Decimal {
            mantissa,
            precision,
            scale,
        })],
        std::slice::from_ref(&ty),
    )
}

/// Evaluates `SUBSTRING` on a `varchar(50)` and two `int`s.
fn eval_substring(value: &str, start: i32, length: i32) -> SqlResult<Value> {
    eval(
        &SUBSTRING_DEF,
        &[text(value), Value::I32(start), Value::I32(length)],
        &[varchar(50), int(), int()],
    )
}

#[test]
fn len_ignores_trailing_spaces() {
    assert_eq!(eval_string(&LEN_DEF, "abc  "), Ok(Value::I32(3)));
    assert_eq!(eval_string(&LEN_DEF, "  abc"), Ok(Value::I32(5)));
    assert_eq!(eval_string(&LEN_DEF, ""), Ok(Value::I32(0)));
    assert_eq!(eval_string(&LEN_DEF, "   "), Ok(Value::I32(0)));
    assert_eq!(
        eval(&LEN_DEF, &[Value::Null], &[varchar(50)]),
        Ok(Value::Null)
    );
    // Only U+0020 is a trailing space: the tabulation counts.
    assert_eq!(eval_string(&LEN_DEF, "ab\t"), Ok(Value::I32(3)));
}

#[test]
fn len_counts_characters_not_bytes() {
    assert_eq!(
        eval(&LEN_DEF, &[text("é")], &[nvarchar(10)]),
        Ok(Value::I32(1))
    );
    assert_eq!(
        eval(&LEN_DEF, &[text("日本")], &[nvarchar(10)]),
        Ok(Value::I32(2))
    );
}

#[test]
fn len_of_a_non_string_converts_first() {
    // `LEN(12345)` is 5: the argument is rendered before being counted.
    assert_eq!(
        eval(&LEN_DEF, &[Value::I32(12345)], &[int()]),
        Ok(Value::I32(5))
    );
}

#[test]
fn len_return_type_is_bigint_for_max() {
    let int_result = length_return_type(&[varchar(10)]).expect("varchar(10) is accepted");
    assert_eq!(int_result.ty, SqlType::Int);

    for max in [
        SqlType::VarChar(Len::Max),
        SqlType::NVarChar(Len::Max),
        SqlType::VarBinary(Len::Max),
    ] {
        let result =
            length_return_type(&[TypeInfo::new(max, true)]).expect("a (max) argument is accepted");
        assert_eq!(result.ty, SqlType::BigInt, "for {}", max.declaration());
    }
}

#[test]
fn len_of_varbinary_max_is_bigint() {
    let result = length_return_type(&[TypeInfo::new(SqlType::VarBinary(Len::Max), true)])
        .expect("varbinary(max) is accepted");
    assert_eq!(result.ty, SqlType::BigInt);
    // And it counts bytes, not the characters of a `0x01` rendering.
    assert_eq!(
        eval(
            &LEN_DEF,
            &[Value::Bytes(vec![0x01])],
            &[TypeInfo::new(SqlType::VarBinary(Len::Max), true)]
        ),
        Ok(Value::I64(1))
    );
}

/// `SELECT LEN(CAST('12:00' AS time));` answers 16, not 8116: the date and time types
/// convert implicitly to a string, so `LEN` accepts them like the other types.
#[test]
fn len_accepts_time_and_the_other_date_types() {
    for ty in [
        SqlType::Time(7),
        SqlType::Date,
        SqlType::DateTime2(7),
        SqlType::DateTimeOffset(7),
    ] {
        let result = length_return_type(&[TypeInfo::new(ty, true)])
            .unwrap_or_else(|e| panic!("{} must be accepted: {}", ty.declaration(), e.message));
        assert_eq!(result.ty, SqlType::Int);
    }
}

#[test]
fn len_accepts_uniqueidentifier() {
    let result = length_return_type(&[TypeInfo::new(SqlType::UniqueIdentifier, true)])
        .expect("uniqueidentifier is accepted: LEN(NEWID()) is 36");
    assert_eq!(result.ty, SqlType::Int);
    // And the value is the 36 characters of the rendered GUID.
    assert_eq!(
        eval(
            &LEN_DEF,
            &[Value::Guid([0x11; 16])],
            &[TypeInfo::new(SqlType::UniqueIdentifier, true)]
        ),
        Ok(Value::I32(36))
    );
}

#[test]
fn datalength_counts_bytes() {
    // The declared type, and it alone, departs `varchar` from `nvarchar`: the same
    // `Value::String` is counted three ways below.
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("abc ")], &[varchar(10)]),
        Ok(Value::I32(4))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("abc")], &[nvarchar(10)]),
        Ok(Value::I32(6))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("abc")], &[char_n(5)]),
        Ok(Value::I32(5))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[Value::I32(1)], &[int()]),
        Ok(Value::I32(4))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[Value::Null], &[varchar(10)]),
        Ok(Value::Null)
    );
}

/// The same call built by hand, without `return_type`: the proof that nothing but
/// `args.types[0]` is consulted, the values and the result type being identical in the
/// two calls.
#[test]
fn datalength_reads_the_declared_type_only() {
    let values = [text("é")];
    let result = TypeInfo::new(SqlType::Int, true);
    let narrow = [varchar(10)];
    let wide = [nvarchar(10)];
    let ctx = StaticContext::default();
    assert_eq!(
        datalength_eval(
            &EvalArgs {
                values: &values,
                types: &narrow,
                result: &result
            },
            &ctx
        ),
        Ok(Value::I32(1))
    );
    assert_eq!(
        datalength_eval(
            &EvalArgs {
                values: &values,
                types: &wide,
                result: &result
            },
            &ctx
        ),
        Ok(Value::I32(2))
    );
}

#[test]
fn datalength_of_max_is_bigint() {
    let ty = TypeInfo::new(SqlType::VarChar(Len::Max), true);
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("abc")], std::slice::from_ref(&ty)),
        Ok(Value::I64(3))
    );
}

#[test]
fn datalength_of_the_fixed_size_types() {
    // Microsoft Learn, "Data types (Transact-SQL)", storage sizes.
    let sizes = [
        (SqlType::Bit, 1),
        (SqlType::TinyInt, 1),
        (SqlType::SmallInt, 2),
        (SqlType::Int, 4),
        (SqlType::BigInt, 8),
        (SqlType::Float, 8),
        (SqlType::Real, 4),
        (SqlType::Money, 8),
        (SqlType::SmallMoney, 4),
        (SqlType::Date, 3),
        (SqlType::Time(0), 3),
        (SqlType::Time(3), 4),
        (SqlType::Time(7), 5),
        (SqlType::DateTime, 8),
        (SqlType::SmallDateTime, 4),
        (SqlType::DateTime2(0), 6),
        (SqlType::DateTime2(7), 8),
        (SqlType::DateTimeOffset(0), 8),
        (SqlType::DateTimeOffset(7), 10),
        (SqlType::UniqueIdentifier, 16),
    ];
    for (ty, expected) in sizes {
        assert_eq!(
            byte_length(&ty, &Value::Bit(true)),
            expected,
            "for {}",
            ty.declaration()
        );
    }
    // The variable-length types are counted on the value, the fixed ones on the type.
    assert_eq!(
        byte_length(&SqlType::Binary(Len::Fixed(8)), &Value::Bytes(vec![1, 2])),
        8
    );
    assert_eq!(
        byte_length(&SqlType::VarBinary(Len::Max), &Value::Bytes(vec![1, 2])),
        2
    );
}

/// A `decimal(p, s)` is billed on its scaled mantissa and not on its declared
/// precision.
///
/// The first three vectors are the ones that **discriminate** the two rules: the three
/// live in a `decimal(38,0)`, where a rule on the precision answers 17 each time and
/// SQL Server answers 5, 9 and 5. The two that follow give the same answer under both
/// rules and are here as the ends of the ladder, not as proof.
#[test]
fn datalength_of_a_decimal_follows_the_mantissa() {
    assert_eq!(datalength_of(4_294_967_295, 38, 0), Ok(Value::I32(5)));
    assert_eq!(datalength_of(4_294_967_296, 38, 0), Ok(Value::I32(9)));
    assert_eq!(datalength_of(1, 38, 0), Ok(Value::I32(5)));
    assert_eq!(datalength_of(1, 9, 0), Ok(Value::I32(5)));
    assert_eq!(datalength_of(TEN_POW_38 - 1, 38, 0), Ok(Value::I32(17)));
}

/// The four steps of the ladder are the powers of two, not the digits of the number:
/// 2^32, 2^64 and 2^96 each cost a 32-bit word more.
#[test]
fn datalength_of_a_decimal_steps_on_the_powers_of_two() {
    let ladder = [
        (u64::MAX as i128, 9),
        (1i128 << 64, 13),
        ((1i128 << 96) - 1, 13),
        (1i128 << 96, 17),
    ];
    for (mantissa, expected) in ladder {
        assert_eq!(
            datalength_of(mantissa, 38, 0),
            Ok(Value::I32(expected)),
            "for a mantissa of {mantissa}"
        );
    }
}

/// The vector that establishes the rule: the **value is constant** at `1` and only the
/// scale moves, so neither the printed value nor the precision can explain the answer —
/// the scaled mantissa can.
#[test]
fn datalength_of_a_decimal_follows_the_scale_at_a_constant_value() {
    assert_eq!(datalength_of(1, 38, 0), Ok(Value::I32(5)));
    assert_eq!(datalength_of(10i128.pow(10), 38, 10), Ok(Value::I32(9)));
    assert_eq!(datalength_of(10i128.pow(20), 38, 20), Ok(Value::I32(13)));
    assert_eq!(datalength_of(10i128.pow(30), 38, 30), Ok(Value::I32(17)));
}

/// The sign leaves the answer alone, on `-1` and `-4294967296` in a `decimal(38,0)`,
/// and a value below 1 is billed on its mantissa too: `0.4294967295` and
/// `0.4294967296` in a `decimal(38,10)` are 5 and 9, though both print as a zero and a
/// fraction.
#[test]
fn datalength_of_a_decimal_ignores_the_sign_and_the_decimal_point() {
    assert_eq!(datalength_of(0, 38, 0), Ok(Value::I32(5)));
    assert_eq!(datalength_of(-1, 38, 0), Ok(Value::I32(5)));
    assert_eq!(datalength_of(-4_294_967_296, 38, 0), Ok(Value::I32(9)));
    assert_eq!(datalength_of(4_294_967_295, 38, 10), Ok(Value::I32(5)));
    assert_eq!(datalength_of(4_294_967_296, 38, 10), Ok(Value::I32(9)));
}

/// `numeric` answers as `decimal` does, and a `NULL` of either stays `NULL`: the branch
/// reads the mantissa of the value, and a `NULL` has no mantissa.
#[test]
fn datalength_of_a_numeric_matches_the_decimal_rule() {
    let ty = TypeInfo::new(
        SqlType::Numeric {
            precision: 20,
            scale: 0,
        },
        true,
    );
    assert_eq!(
        eval(
            &DATALENGTH_DEF,
            &[Value::Decimal(Decimal {
                mantissa: 1,
                precision: 20,
                scale: 0,
            })],
            std::slice::from_ref(&ty),
        ),
        Ok(Value::I32(5))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[Value::Null], std::slice::from_ref(&ty)),
        Ok(Value::Null)
    );
    let decimal_38_0 = TypeInfo::new(
        SqlType::Decimal {
            precision: 38,
            scale: 0,
        },
        true,
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[Value::Null], &[decimal_38_0]),
        Ok(Value::Null)
    );
}

/// `DATALENGTH` counts UTF-16 code units, so a character outside the BMP is charged
/// twice in the two variable-length families.
#[test]
fn datalength_counts_utf16_code_units() {
    // U+1D11E: one character, two code units.
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("𝄞")], &[nvarchar(10)]),
        Ok(Value::I32(4))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("𝄞")], &[varchar(10)]),
        Ok(Value::I32(2))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("a𝄞")], &[nvarchar(10)]),
        Ok(Value::I32(6))
    );
    // A character of the BMP is one unit in the two families, whether code page 1252
    // can represent it or not.
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("日本")], &[nvarchar(10)]),
        Ok(Value::I32(4))
    );
    assert_eq!(
        eval(&DATALENGTH_DEF, &[text("日本")], &[varchar(10)]),
        Ok(Value::I32(2))
    );
}

#[test]
fn left_and_right_basics() {
    assert_eq!(eval_string_int(&LEFT_DEF, "abcdef", 3), Ok(text("abc")));
    assert_eq!(eval_string_int(&RIGHT_DEF, "abcdef", 3), Ok(text("def")));
    assert_eq!(eval_string_int(&LEFT_DEF, "abc", 10), Ok(text("abc")));
    assert_eq!(eval_string_int(&RIGHT_DEF, "abc", 10), Ok(text("abc")));
    assert_eq!(eval_string_int(&LEFT_DEF, "abc", 0), Ok(text("")));
    assert_eq!(eval_string_int(&RIGHT_DEF, "abc", 0), Ok(text("")));
    // A `NULL` string, or a `NULL` length, gives `NULL`.
    assert_eq!(
        eval(
            &LEFT_DEF,
            &[Value::Null, Value::I32(2)],
            &[varchar(50), int()]
        ),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(
            &LEFT_DEF,
            &[text("abc"), Value::Null],
            &[varchar(50), int()]
        ),
        Ok(Value::Null)
    );
}

#[test]
fn runtime_negative_lengths_have_their_errors() {
    let source = text("abc");
    for (def, number, message) in [
        (
            &LEFT_DEF,
            537,
            "The length given to LEFT or SUBSTRING is not valid.",
        ),
        (
            &RIGHT_DEF,
            536,
            "The length given to the RIGHT function is not valid.",
        ),
        (
            &SUBSTRING_DEF,
            537,
            "The length given to LEFT or SUBSTRING is not valid.",
        ),
    ] {
        let (values, types) = if def.name == "SUBSTRING" {
            (
                vec![source.clone(), Value::I32(1), Value::I32(-1)],
                vec![varchar(3), int(), int()],
            )
        } else {
            (
                vec![source.clone(), Value::I32(-1)],
                vec![varchar(3), int()],
            )
        };
        let err = eval(def, &values, &types).expect_err("negative runtime length");
        assert_eq!(
            (err.number, err.severity, err.state, err.message.as_str()),
            (number, 16, 2, message)
        );
    }
}

#[test]
fn runtime_unicode_length_states_follow_the_result_family() {
    for (def, number, state) in [
        (&LEFT_DEF, 537, 3),
        (&RIGHT_DEF, 536, 4),
        (&SUBSTRING_DEF, 537, 3),
    ] {
        for ty in [
            SqlType::NChar(Len::Fixed(3)),
            SqlType::NVarChar(Len::Fixed(3)),
        ] {
            let wide = TypeInfo::new(ty, true);
            let (values, types) = if def.name == "SUBSTRING" {
                (
                    vec![text("abc"), Value::I32(1), Value::I32(-1)],
                    vec![wide, int(), int()],
                )
            } else {
                (vec![text("abc"), Value::I32(-1)], vec![wide, int()])
            };
            let error = eval(def, &values, &types).expect_err("negative Unicode length");
            assert_eq!(
                (error.number, error.severity, error.state),
                (number, 16, state)
            );
        }
    }
}

#[test]
fn runtime_null_string_masks_negative_length() {
    for def in [&LEFT_DEF, &RIGHT_DEF] {
        assert_eq!(
            eval(def, &[Value::Null, Value::I32(-1)], &[varchar(3), int()]),
            Ok(Value::Null)
        );
    }
    assert_eq!(
        eval(
            &SUBSTRING_DEF,
            &[Value::Null, Value::I32(1), Value::I32(-1)],
            &[varchar(3), int(), int()]
        ),
        Ok(Value::Null)
    );
}

#[test]
fn runtime_null_start_masks_negative_length() {
    assert_eq!(
        eval(
            &SUBSTRING_DEF,
            &[text("abc"), Value::Null, Value::I32(-1)],
            &[varchar(3), int(), int()]
        ),
        Ok(Value::Null)
    );
}

#[test]
fn substring_clamps_start() {
    assert_eq!(eval_substring("abcdef", 2, 3), Ok(text("bcd")));
    assert_eq!(eval_substring("abcdef", 0, 3), Ok(text("ab")));
    assert_eq!(eval_substring("abcdef", -1, 3), Ok(text("a")));
    assert_eq!(eval_substring("abcdef", -5, 3), Ok(text("")));
    assert_eq!(eval_substring("abcdef", 10, 3), Ok(text("")));
    assert_eq!(eval_substring("abcdef", 2, 100), Ok(text("bcdef")));
    assert_eq!(eval_substring("abcdef", 1, 0), Ok(text("")));
}

#[test]
fn substring_rejects_a_non_string_argument() {
    let err = substring_return_type(&[int(), int(), int()])
        .expect_err("an int argument is rejected by SUBSTRING");
    assert_eq!(err.number, 8116);
    assert_eq!(
        err.message,
        "Data type int is not accepted for argument 1 of the substring function."
    );
    // The same type is accepted by LEN and by LEFT, which convert instead.
    assert!(length_return_type(&[int()]).is_ok());
    assert!(left_right_return_type(&[int(), int()]).is_ok());
}

/// The fourteen types `SUBSTRING` refuses as a start or as a length, and the six it
/// takes. The counter-test is the second half: `bit` and `money` are numbers and are
/// refused, `decimal` is not an integer and is taken, so "a number" and "an integer"
/// both fail to draw this line.
#[test]
fn substring_rejects_a_start_or_a_length_of_the_wrong_type() {
    let refused: [(SqlType, &str); 14] = [
        (SqlType::Bit, "bit"),
        (SqlType::Float, "float"),
        (SqlType::Real, "real"),
        (SqlType::Money, "money"),
        (SqlType::SmallMoney, "smallmoney"),
        (SqlType::Char(Len::Fixed(1)), "char"),
        (SqlType::VarChar(Len::Fixed(5)), "varchar"),
        (SqlType::NChar(Len::Fixed(1)), "nchar"),
        (SqlType::NVarChar(Len::Fixed(5)), "nvarchar"),
        (SqlType::Date, "date"),
        (SqlType::DateTime, "datetime"),
        (SqlType::Time(7), "time"),
        (SqlType::UniqueIdentifier, "uniqueidentifier"),
        (SqlType::Binary(Len::Fixed(1)), "binary"),
    ];
    for (ty, name) in refused {
        let wrong = TypeInfo::new(ty, true);
        for position in [2usize, 3] {
            let mut args = [varchar(10), int(), int()];
            args[position - 1] = wrong.clone();
            let Err(err) = substring_return_type(&args) else {
                panic!("{name} must be refused at position {position}");
            };
            assert_eq!(err.number, 8116);
            assert_eq!(
                err.message,
                format!(
                    "Data type {name} is not accepted for argument {position} of the substring function."
                )
            );
        }
    }
}

/// The six types `SUBSTRING` takes as a start or as a length, and the message of the
/// fourteen it refuses.
#[test]
fn substring_offset_types_follow_the_engine() {
    for ty in [
        SqlType::TinyInt,
        SqlType::SmallInt,
        SqlType::Int,
        SqlType::BigInt,
        SqlType::Decimal {
            precision: 5,
            scale: 1,
        },
        SqlType::Numeric {
            precision: 5,
            scale: 0,
        },
    ] {
        let accepted = TypeInfo::new(ty, true);
        assert!(
            substring_return_type(&[varchar(10), accepted.clone(), accepted]).is_ok(),
            "{ty:?} is accepted as a start and as a length"
        );
    }

    let float = TypeInfo::new(SqlType::Float, true);
    let err = substring_return_type(&[varchar(10), int(), float.clone()])
        .expect_err("a float length is 8116");
    assert_eq!(err.number, 8116);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "Data type float is not accepted for argument 3 of the substring function."
    );

    // The start is named with its own position, and is read before the length.
    let err = substring_return_type(&[varchar(10), float.clone(), float])
        .expect_err("a float start is 8116");
    assert_eq!(
        err.message,
        "Data type float is not accepted for argument 2 of the substring function."
    );

    // Argument 1 is read before both of them.
    let err = substring_return_type(&[int(), int(), TypeInfo::new(SqlType::Float, true)])
        .expect_err("an int text argument is 8116");
    assert_eq!(
        err.message,
        "Data type int is not accepted for argument 1 of the substring function."
    );

    // LEFT and RIGHT keep taking what SUBSTRING refuses: SQL Server fails them at run
    // time instead.
    assert!(left_right_return_type(&[varchar(10), TypeInfo::new(SqlType::Float, true)]).is_ok());
    assert!(left_right_return_type(&[varchar(10), varchar(5)]).is_ok());
}

#[test]
fn substring_of_binary_cuts_bytes() {
    let ty = TypeInfo::new(SqlType::VarBinary(Len::Fixed(10)), true);
    let types = [ty.clone(), int(), int()];
    assert_eq!(
        eval(
            &SUBSTRING_DEF,
            &[Value::Bytes(vec![1, 2, 3, 4]), Value::I32(2), Value::I32(2)],
            &types
        ),
        Ok(Value::Bytes(vec![2, 3]))
    );
    // And the result stays binary.
    assert_eq!(
        substring_return_type(&types)
            .expect("binary is accepted")
            .ty,
        ty.ty
    );
}

#[test]
fn upper_lower_fold_one_char_at_a_time() {
    assert_eq!(eval_string(&UPPER_DEF, "abcé"), Ok(text("ABCÉ")));
    assert_eq!(eval_string(&LOWER_DEF, "ABCÉ"), Ok(text("abcé")));
    // `ß` upper-cases to two characters in Unicode, to itself in SQL Server.
    assert_eq!(eval_string(&UPPER_DEF, "straße"), Ok(text("STRAßE")));
    assert_eq!(
        eval(&UPPER_DEF, &[Value::Null], &[varchar(50)]),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&LOWER_DEF, &[Value::Null], &[varchar(50)]),
        Ok(Value::Null)
    );
}

#[test]
fn ltrim_rtrim_only_remove_spaces() {
    assert_eq!(eval_string(&LTRIM_DEF, "  ab "), Ok(text("ab ")));
    assert_eq!(eval_string(&RTRIM_DEF, " ab  "), Ok(text(" ab")));
    assert_eq!(eval_string(&RTRIM_DEF, "ab\t"), Ok(text("ab\t")));
    assert_eq!(eval_string(&LTRIM_DEF, "\tab"), Ok(text("\tab")));
    assert_eq!(eval_string(&LTRIM_DEF, "   "), Ok(text("")));
    assert_eq!(eval_string(&RTRIM_DEF, "   "), Ok(text("")));
    assert_eq!(
        eval(&LTRIM_DEF, &[Value::Null], &[varchar(50)]),
        Ok(Value::Null)
    );
}

#[test]
fn return_type_keeps_the_string_family() {
    let one_argument = [&UPPER_DEF, &LOWER_DEF, &LTRIM_DEF, &RTRIM_DEF];
    for def in one_argument {
        let narrow = (def.return_type)(&[varchar(10)]).expect("varchar is accepted");
        assert_eq!(narrow.ty, SqlType::VarChar(Len::Fixed(10)), "{}", def.name);
        let wide = (def.return_type)(&[nvarchar(10)]).expect("nvarchar is accepted");
        assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(10)), "{}", def.name);
    }

    for def in [&LEFT_DEF, &RIGHT_DEF] {
        let narrow = (def.return_type)(&[varchar(10), int()]).expect("varchar is accepted");
        assert_eq!(narrow.ty, SqlType::VarChar(Len::Fixed(10)), "{}", def.name);
        let wide = (def.return_type)(&[nvarchar(10), int()]).expect("nvarchar is accepted");
        assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(10)), "{}", def.name);
    }

    let narrow = substring_return_type(&[varchar(10), int(), int()]).expect("varchar is accepted");
    assert_eq!(narrow.ty, SqlType::VarChar(Len::Fixed(10)));
    let wide = substring_return_type(&[nvarchar(10), int(), int()]).expect("nvarchar is accepted");
    assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(10)));

    // A `char(n)` gives a `varchar(n)`.
    let from_char = (UPPER_DEF.return_type)(&[char_n(5)]).expect("char is accepted");
    assert_eq!(from_char.ty, SqlType::VarChar(Len::Fixed(5)));
    // A non-character argument gives a `varchar` too.
    let from_int = (LEFT_DEF.return_type)(&[int(), int()]).expect("int is accepted");
    assert_eq!(from_int.ty, SqlType::VarChar(Len::Fixed(WIDEST_VARCHAR)));
}

#[test]
fn return_type_keeps_the_collation_of_the_argument() {
    let collation = Collation::parse("Latin1_General_BIN2").expect("a supported collation");
    let argument = TypeInfo {
        ty: SqlType::VarChar(Len::Fixed(10)),
        nullable: true,
        collation: Some(collation),
    };
    for result in [
        (UPPER_DEF.return_type)(std::slice::from_ref(&argument)),
        (LEFT_DEF.return_type)(&[argument.clone(), int()]),
        substring_return_type(&[argument.clone(), int(), int()]),
    ] {
        let result = result.expect("varchar is accepted");
        assert_eq!(result.collation, Some(collation));
    }
}

#[test]
fn the_nine_functions_are_registered_as_scalars() {
    crate::builtins::register_builtins();
    for name in [
        "len",
        "DATALENGTH",
        "Left",
        "RIGHT",
        "substring",
        "UPPER",
        "lower",
        "LTRIM",
        "rtrim",
    ] {
        let def = crate::lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
        assert_eq!(def.kind, FunctionKind::Scalar);
        assert!(def.aggregate.is_none());
        assert!(def.deterministic);
    }
}
