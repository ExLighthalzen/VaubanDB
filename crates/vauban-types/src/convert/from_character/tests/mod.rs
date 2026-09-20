use super::*;
use crate::{Decimal, Len, TypeInfo};

fn varchar() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true)
}

fn conv(s: &str, to: SqlType) -> SqlResult<Value> {
    string_to_numeric(s, &varchar(), &to)
}

fn dec(p: u8, s: u8) -> SqlType {
    SqlType::Decimal {
        precision: p,
        scale: s,
    }
}

/// `int` and its three failures.
#[test]
fn string_to_integer() {
    assert_eq!(conv("  42 ", SqlType::Int), Ok(Value::I32(42)));
    assert_eq!(conv("-42", SqlType::Int), Ok(Value::I32(-42)));
    assert_eq!(conv(" +42 ", SqlType::Int), Ok(Value::I32(42)));
    // The empty string is zero.
    assert_eq!(conv("", SqlType::Int), Ok(Value::I32(0)));
    assert_eq!(conv("   ", SqlType::Int), Ok(Value::I32(0)));

    // The message is exact and echoes the value as written, source type included.
    let e = conv("1.9", SqlType::Int).expect_err("245");
    assert_eq!(e.number, 245);
    assert_eq!(
        e.message,
        "The varchar value '1.9' could not be converted to data type int."
    );
    // Neither a thousands separator nor an exponent is read.
    for text in ["abc", "1,234", "1e3", "1 2"] {
        assert_eq!(
            conv(text, SqlType::Int).expect_err(text).number,
            245,
            "{text}"
        );
    }
    // The spaces of the source are kept in the message.
    let e = conv(" abc ", SqlType::Int).expect_err("245");
    assert_eq!(
        e.message,
        "The varchar value ' abc ' could not be converted to data type int."
    );
    // An `nvarchar` source names itself.
    let nvarchar = TypeInfo::new(SqlType::NVarChar(Len::Fixed(3)), true);
    let e = string_to_numeric("abc", &nvarchar, &SqlType::Int).expect_err("245");
    assert_eq!(
        e.message,
        "The nvarchar value 'abc' could not be converted to data type int."
    );
}

/// 248 state 1 for `int`, 244 states 1 and 2 for the two small integers, and 8115
/// state 2 for `bigint`, the one integer target reported with it.
#[test]
fn string_to_integer_overflow() {
    let e = conv("99999999999", SqlType::Int).expect_err("248");
    assert_eq!(e.number, 248);
    assert_eq!(e.state, 1);
    assert_eq!(
        e.message,
        "The varchar value '99999999999' does not fit in an int column."
    );
    // The state follows the internal column name, 2 for `INT2` and 1 for `INT1`.
    let e = conv("99999", SqlType::SmallInt).expect_err("244");
    assert_eq!(e.number, 244);
    assert_eq!(e.state, 2);
    assert_eq!(
        e.message,
        "The varchar value '99999' does not fit in an INT2 column; a wider integer type is needed."
    );
    let e = conv("300", SqlType::TinyInt).expect_err("244");
    assert_eq!(e.number, 244);
    assert_eq!(e.state, 1);
    assert_eq!(
        e.message,
        "The varchar value '300' does not fit in an INT1 column; a wider integer type is needed."
    );

    let e = conv("99999999999999999999", SqlType::BigInt).expect_err("8115");
    assert_eq!(e.number, 8115);
    assert_eq!(e.state, 2);
    assert_eq!(
        e.message,
        "Converting expression to data type bigint overflowed."
    );

    // The bounds themselves.
    assert_eq!(conv("255", SqlType::TinyInt), Ok(Value::I8(255)));
    assert!(conv("-1", SqlType::TinyInt).is_err());
    assert_eq!(conv("-32768", SqlType::SmallInt), Ok(Value::I16(-32_768)));
    assert_eq!(
        conv("2147483647", SqlType::Int),
        Ok(Value::I32(2_147_483_647))
    );
    assert_eq!(
        conv("-9223372036854775808", SqlType::BigInt),
        Ok(Value::I64(i64::MIN))
    );
}

/// The shapes of a `decimal`, a `money` and a `float` source string.
#[test]
fn string_to_decimal_and_money() {
    let decimal = |mantissa, precision, scale| {
        Ok(Value::Decimal(Decimal {
            mantissa,
            precision,
            scale,
        }))
    };
    assert_eq!(conv("1.5", dec(5, 2)), decimal(150, 5, 2));
    // Half away from zero, computed on the digits and not on a `f64`.
    assert_eq!(conv("1.567", dec(5, 2)), decimal(157, 5, 2));
    assert_eq!(conv("  -1.565 ", dec(5, 2)), decimal(-157, 5, 2));
    assert_eq!(conv("+1.5", dec(5, 2)), decimal(150, 5, 2));
    assert_eq!(conv(".5", dec(5, 2)), decimal(50, 5, 2));
    assert_eq!(conv("5.", dec(5, 2)), decimal(500, 5, 2));

    // The money grammar.
    assert_eq!(
        conv("$1,234.50", SqlType::Money),
        Ok(Value::Money(12_345_000))
    );
    assert_eq!(conv("-$1.5", SqlType::Money), Ok(Value::Money(-15_000)));
    assert_eq!(conv("$-1.5", SqlType::Money), Ok(Value::Money(-15_000)));
    assert_eq!(conv("$ 1.5", SqlType::Money), Ok(Value::Money(15_000)));
    // Four decimals, half away from zero.
    assert_eq!(conv("  1.23456 ", SqlType::Money), Ok(Value::Money(12_346)));
    assert_eq!(conv("-1.23455", SqlType::Money), Ok(Value::Money(-12_346)));
    assert_eq!(conv("", SqlType::Money), Ok(Value::Money(0)));

    // The float grammar.
    assert_eq!(conv("1.5", SqlType::Float), Ok(Value::F64(1.5)));
    assert_eq!(conv("1e3", SqlType::Float), Ok(Value::F64(1000.0)));
    assert_eq!(conv("", SqlType::Float), Ok(Value::F64(0.0)));

    // The exponent is refused towards `decimal`.
    let e = conv("1e3", dec(5, 0)).expect_err("8114");
    assert_eq!(e.number, 8114);
    assert_eq!(
        e.message,
        "Data type varchar could not be converted to numeric."
    );
    // The empty string is refused towards `decimal`.
    assert_eq!(conv("", dec(5, 2)).expect_err("8114").number, 8114);
    // 8115 state 8, the state a character source towards `numeric` carries.
    let e = conv("1234.5", dec(5, 2)).expect_err("8115");
    assert_eq!(e.number, 8115);
    assert_eq!(e.state, 8);
    assert_eq!(
        e.message,
        "Converting varchar to data type numeric overflowed."
    );
}

/// What `float` and `money` refuse, and with which number.
#[test]
fn string_to_approximate_and_money_failures() {
    for text in ["1.2.3", "inf", "infinity", "NaN", "abc", "1e"] {
        let e = conv(text, SqlType::Float).expect_err(text);
        assert_eq!(e.number, 8114, "{text}");
        assert_eq!(
            e.message,
            "Data type varchar could not be converted to float."
        );
    }
    let e = conv("1e400", SqlType::Float).expect_err("8115");
    assert_eq!(e.number, 8115);
    assert_eq!(e.state, 2);
    assert_eq!(
        e.message,
        "Converting expression to data type float overflowed."
    );
    let e = conv("1e40", SqlType::Real).expect_err("8115");
    assert_eq!(e.number, 8115);

    // 235, state 0, and a message that names neither the value nor the source type,
    // an `nvarchar` source included.
    let e = conv("abc", SqlType::Money).expect_err("235");
    assert_eq!(e.number, 235);
    assert_eq!(e.state, 0);
    assert_eq!(
        e.message,
        "The character value is not a valid money literal and could not be converted."
    );
    let nvarchar = TypeInfo::new(SqlType::NVarChar(Len::Fixed(3)), true);
    assert_eq!(
        string_to_numeric("abc", &nvarchar, &SqlType::Money)
            .expect_err("235")
            .message,
        "The character value is not a valid money literal and could not be converted."
    );
    let e = conv("99999999999999999999", SqlType::Money).expect_err("8115");
    assert_eq!(e.number, 8115);
    assert_eq!(e.state, 2);
    assert_eq!(
        e.message,
        "Converting expression to data type money overflowed."
    );
    assert!(conv("300000", SqlType::SmallMoney).is_err());
    assert_eq!(
        conv("214748.3647", SqlType::SmallMoney),
        Ok(Value::Money(i64::from(i32::MAX)))
    );
}

/// `bit` reads the integer grammar.
#[test]
fn string_to_bit_follows_the_integer_grammar() {
    assert_eq!(conv("0", SqlType::Bit), Ok(Value::Bit(false)));
    assert_eq!(conv("2", SqlType::Bit), Ok(Value::Bit(true)));
    assert_eq!(conv("-3", SqlType::Bit), Ok(Value::Bit(true)));
    assert_eq!(conv("", SqlType::Bit), Ok(Value::Bit(false)));
    for text in ["1.9", "abc"] {
        let e = conv(text, SqlType::Bit).expect_err(text);
        assert_eq!(e.number, 245, "{text}");
        assert_eq!(
            e.message,
            format!("The varchar value '{text}' could not be converted to data type bit.")
        );
    }
}

/// The preconditions: the source is a character type, the target a numeric one.
/// The dates, GUIDs and binaries belong to other modules.
#[test]
fn broken_preconditions_are_internal_errors() {
    let int = TypeInfo::new(SqlType::Int, false);
    assert_eq!(
        string_to_numeric("1", &int, &SqlType::Int)
            .expect_err("bug")
            .number,
        50_000
    );
    for target in [
        SqlType::Date,
        SqlType::UniqueIdentifier,
        SqlType::VarBinary(Len::Fixed(4)),
        SqlType::VarChar(Len::Fixed(4)),
    ] {
        assert_eq!(
            conv("1", target).expect_err("bug").number,
            50_000,
            "{target:?}"
        );
    }
}

/// A sign with no digit behind it, crossed over the five families.
///
/// `'-'`, `'+'`, `'- '` and `' -'` are `0` towards `bit`, `tinyint`, `smallint`,
/// `int` and `bigint`, `0.0000` towards `money` and `smallmoney`, 8115 towards
/// `numeric`/`decimal` and 8114 towards `float` and `real`. `'--'`, `'-+'`, `'+-'`,
/// `'-.'`, `'-,'`, `'-$'` and `'$-'` are refused by the integer targets: the lone
/// sign is the degenerate case of `[+|-] spaces* digits*`, see
/// [`spaces_fall_between_the_sign_and_the_digits`].
#[test]
fn a_lone_sign_is_zero_for_the_integers_and_for_money() {
    for text in ["-", "+", "- ", " -"] {
        assert_eq!(conv(text, SqlType::Bit), Ok(Value::Bit(false)), "{text:?}");
        assert_eq!(conv(text, SqlType::TinyInt), Ok(Value::I8(0)), "{text:?}");
        assert_eq!(conv(text, SqlType::SmallInt), Ok(Value::I16(0)), "{text:?}");
        assert_eq!(conv(text, SqlType::Int), Ok(Value::I32(0)), "{text:?}");
        assert_eq!(conv(text, SqlType::BigInt), Ok(Value::I64(0)), "{text:?}");
        assert_eq!(conv(text, SqlType::Money), Ok(Value::Money(0)), "{text:?}");
        assert_eq!(
            conv(text, SqlType::SmallMoney),
            Ok(Value::Money(0)),
            "{text:?}"
        );
        // `float` and `numeric` refuse the same four strings: the rule does not cross
        // the family boundary. SQL Server answers 8115 state 6 towards `numeric`,
        // which `to_exact_numeric` documents as an unaligned gap.
        for target in [dec(10, 2), SqlType::Float, SqlType::Real] {
            let e = conv(text, target).expect_err("8114");
            assert_eq!(e.number, 8114, "{text:?} {target:?}");
        }
    }
    // The counter-vectors: a second character brings 245 back on these nine strings.
    for text in ["--", "-+", "+-", "-.", "-,", "-$", "$-", "--1", "1-"] {
        for target in [SqlType::Bit, SqlType::Int, SqlType::SmallInt] {
            assert_eq!(
                conv(text, target).expect_err(text).number,
                245,
                "{text:?} {target:?}"
            );
        }
    }
}

/// The grammar is `[+|-] spaces* digits*`, not a lone sign.
///
/// `'- 1'` is `-1`, `'+ 1'` is `1`,
/// `'- 0'` is `0` towards `bit`, `smallint`, `int` and `bigint`, and `'- 1'` towards
/// `tinyint` is the 244 of `-1`; `numeric` reads the same spaces (`'- 1.5'` is
/// `-1.50`). The frontier: the tab is not a space (`'-\t1'` is 245 and 8114), the
/// sign comes once (`'- -1'`, `'- +1'`) and first (`'1 -'`), and `float` refuses
/// the space after the sign (`'- 1'` is 8114).
#[test]
fn spaces_fall_between_the_sign_and_the_digits() {
    for (text, value) in [
        ("- 1", -1),
        ("+ 1", 1),
        ("- 0", 0),
        ("-  12", -12),
        ("- 00", 0),
    ] {
        assert_eq!(conv(text, SqlType::Int), Ok(Value::I32(value)), "{text:?}");
        assert_eq!(
            conv(text, SqlType::BigInt),
            Ok(Value::I64(value.into())),
            "{text:?}"
        );
        assert_eq!(
            conv(text, SqlType::SmallInt),
            Ok(Value::I16(value as i16)),
            "{text:?}"
        );
        assert_eq!(
            conv(text, SqlType::Bit),
            Ok(Value::Bit(value != 0)),
            "{text:?}"
        );
        assert_eq!(
            conv(text, dec(10, 2)),
            Ok(Value::Decimal(Decimal {
                mantissa: i128::from(value) * 100,
                precision: 10,
                scale: 2,
            })),
            "{text:?}"
        );
    }
    assert_eq!(conv("+ 1", SqlType::TinyInt), Ok(Value::I8(1)));
    let e = conv("- 1", SqlType::TinyInt).expect_err("244");
    assert_eq!((e.number, e.state), (244, 1));
    assert_eq!(
        conv("- 1.5", dec(10, 2)),
        Ok(Value::Decimal(Decimal {
            mantissa: -150,
            precision: 10,
            scale: 2,
        }))
    );
    // The frontier, on the integers and on `numeric` alike.
    for text in ["-\t1", "- -1", "- +1", "1 -", "+ 1e2", "- 1.5"] {
        for target in [
            SqlType::Bit,
            SqlType::Int,
            SqlType::SmallInt,
            SqlType::TinyInt,
        ] {
            assert_eq!(
                conv(text, target).expect_err(text).number,
                245,
                "{text:?} {target:?}"
            );
        }
    }
    for text in ["-\t1", "- -1", "- +1", "1 -", "+ 1e2"] {
        assert_eq!(
            conv(text, dec(10, 2)).expect_err(text).number,
            8114,
            "{text:?}"
        );
    }
    // `float` and `real` read neither the lone sign nor the space after a sign.
    for text in ["- 1", "+ 1", "- 0", "- 1.5"] {
        for target in [SqlType::Float, SqlType::Real] {
            assert_eq!(
                conv(text, target).expect_err(text).number,
                8114,
                "{text:?} {target:?}"
            );
        }
    }
}

/// The money grammar spelled without a single digit is `0.0000`.
///
/// Over the thirty-six ordered pairs of `$ + - . ,` and the space, the twelve refusals
/// below are 235 and the twenty-four other pairs are `0.0000`. `'$ - .'`, `'$-,.'` and
/// `',,'` extend the same grammar past two characters.
#[test]
fn money_reads_a_grammar_without_digits_as_zero() {
    let symbols = ['$', '+', '-', '.', ',', ' '];
    // The twelve pairs that are 235; the other twenty-four of the thirty-six are
    // `0.0000`.
    let refused = [
        "$$", "++", "+-", "-+", "--", ".$", ".+", ".-", "..", ",$", ",+", ",-",
    ];
    assert_eq!(refused.len(), 12);
    for a in symbols {
        for b in symbols {
            let text = format!("{a}{b}");
            let got = conv(&text, SqlType::Money);
            if refused.contains(&text.as_str()) {
                assert_eq!(got.expect_err(&text).number, 235, "{text:?}");
            } else {
                assert_eq!(got, Ok(Value::Money(0)), "{text:?}");
            }
        }
    }
    for text in [
        "$", "+", "-", ".", ",", "$-.", "-$.", "$-,.", "$ - .", ",,", ",.,",
    ] {
        assert_eq!(conv(text, SqlType::Money), Ok(Value::Money(0)), "{text:?}");
    }
    for text in ["$.-", ".-$", "+-.,$", "$$$", "1$", "1-", "$1$", "1.2.3"] {
        assert_eq!(
            conv(text, SqlType::Money).expect_err(text).number,
            235,
            "{text:?}"
        );
    }
    // A comma falls on either side of the point, and the digits still count.
    for (text, units) in [
        ("1.2,3", 12_300),
        ("1,.2", 12_000),
        ("1.,2", 12_000),
        ("1.2,", 12_000),
        (".,1", 1_000),
        ("$1", 10_000),
        ("$ 1", 10_000),
        ("-$1", -10_000),
    ] {
        assert_eq!(
            conv(text, SqlType::Money),
            Ok(Value::Money(units)),
            "{text:?}"
        );
    }
}

/// `d` and `D` spell the exponent of a `float` as `e` and `E` do.
///
/// Each form is asserted against its `e` twin, which is the counter-vector: a rule
/// that read `d` as something else would break the pairing. `'1f5'` and `'1g5'` show
/// that the letter added is `d`.
#[test]
fn the_d_exponent_reads_as_the_e_exponent() {
    for (with_d, with_e) in [
        ("1d5", "1e5"),
        ("1D5", "1E5"),
        ("1.d5", "1.e5"),
        ("1d+5", "1e+5"),
        ("1d-5", "1e-5"),
        ("1d05", "1e05"),
        (" 1d5 ", " 1e5 "),
        ("+1d5", "+1e5"),
        ("-1d5", "-1e5"),
        (".5d2", ".5e2"),
        ("1.5d2", "1.5e2"),
        ("1d0", "1e0"),
    ] {
        assert_eq!(
            conv(with_d, SqlType::Float),
            conv(with_e, SqlType::Float),
            "{with_d:?}"
        );
        assert_eq!(
            conv(with_d, SqlType::Real),
            conv(with_e, SqlType::Real),
            "{with_d:?}"
        );
    }
    for (with_d, with_e) in [
        ("1d", "1e"),
        ("d5", "e5"),
        ("1d.5", "1e.5"),
        ("1d+", "1e+"),
        ("1d5.2", "1e5.2"),
        ("1 d5", "1 e5"),
        ("1d 5", "1e 5"),
        ("5d", "5e"),
        ("1d5d2", "1e5e2"),
    ] {
        assert_eq!(conv(with_d, SqlType::Float).expect_err(with_d).number, 8114);
        assert_eq!(conv(with_e, SqlType::Float).expect_err(with_e).number, 8114);
    }
    for text in ["1de5", "1ed5", "1d5e2", "1f5", "1g5", "1dd5"] {
        assert_eq!(
            conv(text, SqlType::Float).expect_err(text).number,
            8114,
            "{text:?}"
        );
    }
    // The letter stops at the approximate families: no other grammar reads it.
    assert_eq!(conv("1d5", SqlType::Int).expect_err("245").number, 245);
    assert_eq!(conv("1d5", dec(10, 2)).expect_err("8114").number, 8114);
    assert_eq!(conv("1d5", SqlType::Money).expect_err("235").number, 235);
}
