use super::*;
use crate::builtins::check_call;
use crate::context::StaticContext;

fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

fn varchar(length: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), nullable)
}

fn nvarchar(length: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Fixed(length)), nullable)
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

/// Evaluates `def` on the given values and types, with the result type `check_call`
/// would have computed.
fn eval(def: &FunctionDef, values: &[Value], types: &[TypeInfo]) -> SqlResult<Value> {
    let result = (def.return_type)(types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, &StaticContext::default())
}

#[test]
fn concat_treats_null_as_empty() {
    // CONCAT('a', NULL, 1) is 'a1'.
    assert_eq!(
        eval(
            &CONCAT_DEF,
            &[text("a"), Value::Null, Value::I32(1)],
            &[varchar(1, false), int(true), int(false)],
        ),
        Ok(text("a1"))
    );
    // The same case: CONCAT(NULL, NULL) is the empty string, never NULL.
    assert_eq!(
        eval(
            &CONCAT_DEF,
            &[Value::Null, Value::Null],
            &[int(true), int(true)],
        ),
        Ok(text(""))
    );
    let result = concat_return_type(&[int(true), int(true)]).expect("two arguments are valid");
    assert!(!result.nullable, "CONCAT never answers NULL");
}

#[test]
fn concat_return_type_family() {
    let same = concat_return_type(&[varchar(3, true), varchar(3, true)]).expect("valid arguments");
    assert_eq!(same.ty, SqlType::VarChar(Len::Fixed(6)));

    let wide = concat_return_type(&[varchar(3, true), nvarchar(3, true)]).expect("valid arguments");
    assert_eq!(wide.ty, SqlType::NVarChar(Len::Fixed(6)));

    // Not the precedence rule: `implicit_result_type(int, int)` is `int`, CONCAT is
    // still a `varchar`.
    let numbers = concat_return_type(&[int(true), int(true)]).expect("valid arguments");
    assert_eq!(numbers.ty, SqlType::VarChar(Len::Fixed(24)));
}

/// The declared length rule, with the vectors of the documentation of
/// [`concat_return_type`].
#[test]
fn concat_declared_length_is_the_capped_sum() {
    let sum = concat_return_type(&[varchar(3, true), varchar(5, true)]).expect("valid arguments");
    assert_eq!(sum.ty, SqlType::VarChar(Len::Fixed(8)));

    let capped =
        concat_return_type(&[varchar(4000, true), varchar(5000, true)]).expect("valid arguments");
    assert_eq!(capped.ty, SqlType::VarChar(Len::Fixed(8000)));

    let wide_capped =
        concat_return_type(&[nvarchar(3000, true), nvarchar(3000, true)]).expect("valid arguments");
    assert_eq!(wide_capped.ty, SqlType::NVarChar(Len::Fixed(4000)));

    let unbounded = concat_return_type(&[
        TypeInfo::new(SqlType::VarChar(Len::Max), true),
        varchar(1, true),
    ])
    .expect("valid arguments");
    assert_eq!(unbounded.ty, SqlType::VarChar(Len::Max));

    let mixed =
        concat_return_type(&[varchar(3, true), nvarchar(1, true)]).expect("valid arguments");
    assert_eq!(mixed.ty, SqlType::NVarChar(Len::Fixed(4)));

    let integer = concat_return_type(&[int(true), varchar(5, true)]).expect("valid arguments");
    assert_eq!(integer.ty, SqlType::VarChar(Len::Fixed(17)));
}

#[test]
fn concat_argument_limit() {
    let many = vec![varchar(1, false); 254];
    let result = check_call(&CONCAT_DEF, &many).expect("254 arguments are accepted");
    assert_eq!(result.ty, SqlType::VarChar(Len::Fixed(254)));

    let too_many = vec![varchar(1, false); 255];
    let err = check_call(&CONCAT_DEF, &too_many).expect_err("255 arguments are rejected");
    assert_eq!(err.number, 189);
    assert_eq!(err.severity, 15);
    assert_eq!(
        err.message,
        "The function concat takes between 2 and 254 arguments."
    );
}

#[test]
fn char_ascii_roundtrip() {
    assert_eq!(
        eval(&CHAR_DEF, &[Value::I32(65)], &[int(false)]),
        Ok(text("A"))
    );
    assert_eq!(
        eval(&ASCII_DEF, &[text("A")], &[varchar(1, false)]),
        Ok(Value::I32(65))
    );
    // Outside 0..=255 the answer is NULL, not an error.
    assert_eq!(
        eval(&CHAR_DEF, &[Value::I32(-1)], &[int(false)]),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&CHAR_DEF, &[Value::I32(256)], &[int(false)]),
        Ok(Value::Null)
    );
    // The empty string has no first character.
    assert_eq!(
        eval(&ASCII_DEF, &[text("")], &[varchar(1, false)]),
        Ok(Value::Null)
    );
    for def in [&CHAR_DEF, &ASCII_DEF] {
        assert_eq!(eval(def, &[Value::Null], &[int(true)]), Ok(Value::Null));
    }
}

/// The vectors where code page 1252 and Latin-1 disagree, which is what proves the
/// round trip goes through the code page and not through Unicode.
#[test]
fn char_ascii_use_the_code_page_of_the_collation() {
    for (code, expected) in [(128, "€"), (0x9C, "œ"), (233, "é")] {
        assert_eq!(
            eval(&CHAR_DEF, &[Value::I32(code)], &[int(false)]),
            Ok(text(expected)),
            "CHAR({code})"
        );
    }
    for (input, ty, expected) in [
        ("€", varchar(1, false), 128),
        ("€", nvarchar(1, false), 128),
        ("é", varchar(1, false), 233),
        // A character the page has no byte for: the question mark the conversion to
        // varchar substitutes.
        ("日", nvarchar(1, false), 63),
    ] {
        assert_eq!(
            eval(&ASCII_DEF, &[text(input)], &[ty]),
            Ok(Value::I32(expected)),
            "ASCII({input:?})"
        );
    }
    // The five bytes code page 1252 leaves unassigned in its published layout stand
    // for the C1 control character of the same value, and the round trip holds on
    // them like on the other bytes.
    for code in [0x81_i32, 0x8D, 0x8F, 0x90, 0x9D] {
        let scalar = u32::try_from(code).expect("a positive code");
        let control = char::from_u32(scalar).expect("a C1 control character");
        let as_text = text(&control.to_string());
        assert_eq!(
            eval(&CHAR_DEF, &[Value::I32(code)], &[int(false)]),
            Ok(as_text.clone()),
            "CHAR({code})"
        );
        assert_eq!(
            eval(&ASCII_DEF, &[as_text], &[nvarchar(1, false)]),
            Ok(Value::I32(code)),
            "ASCII(NCHAR({code}))"
        );
    }
    // Only the first character is read.
    assert_eq!(
        eval(&ASCII_DEF, &[text("Ab")], &[varchar(2, false)]),
        Ok(Value::I32(65))
    );
}

#[test]
fn nchar_unicode_roundtrip() {
    assert_eq!(
        eval(&NCHAR_DEF, &[Value::I32(233)], &[int(false)]),
        Ok(text("é"))
    );
    assert_eq!(
        eval(&UNICODE_DEF, &[text("é")], &[nvarchar(1, false)]),
        Ok(Value::I32(233))
    );
    // Out of the domain of a non-SC collation.
    assert_eq!(
        eval(&NCHAR_DEF, &[Value::I32(65536)], &[int(false)]),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&NCHAR_DEF, &[Value::I32(-1)], &[int(false)]),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&NCHAR_DEF, &[Value::I32(0)], &[int(false)]),
        Ok(text("\u{0}"))
    );
    // The same case: the empty string has no first character, several characters read
    // only the first one.
    assert_eq!(
        eval(&UNICODE_DEF, &[text("")], &[nvarchar(1, false)]),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&UNICODE_DEF, &[text("ab")], &[nvarchar(2, false)]),
        Ok(Value::I32(97))
    );
    // A lone surrogate cannot live in a Rust string: the documented limit.
    assert_eq!(
        eval(&NCHAR_DEF, &[Value::I32(0xD83D)], &[int(false)]),
        Ok(Value::Null)
    );
    // Outside the basic multilingual plane, UNICODE answers the high surrogate.
    assert_eq!(
        eval(&UNICODE_DEF, &[text("😀")], &[nvarchar(2, false)]),
        Ok(Value::I32(0xD83D))
    );
    for def in [&NCHAR_DEF, &UNICODE_DEF] {
        assert_eq!(eval(def, &[Value::Null], &[int(true)]), Ok(Value::Null));
    }
}

#[test]
fn quotename_doubles_the_closing_bracket() {
    let one = [varchar(130, true)];
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text("abc")], &one),
        Ok(text("[abc]"))
    );
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text("a]b")], &one),
        Ok(text("[a]]b]"))
    );
    let two = [varchar(130, true), varchar(1, true)];
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text("abc"), text("\"")], &two),
        Ok(text("\"abc\""))
    );
    // 128 characters pass, 129 answer NULL.
    let long = "a".repeat(128);
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text(&long)], &one),
        Ok(text(&format!("[{long}]")))
    );
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text(&"a".repeat(129))], &one),
        Ok(Value::Null)
    );
    // An unknown delimiter, a NULL delimiter and a NULL string all answer NULL.
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text("abc"), text("#")], &two),
        Ok(Value::Null)
    );
    assert_eq!(
        eval(&QUOTENAME_DEF, &[text("abc"), Value::Null], &two),
        Ok(Value::Null)
    );
    assert_eq!(eval(&QUOTENAME_DEF, &[Value::Null], &one), Ok(Value::Null));
}

/// Each documented delimiter, with its answer.
#[test]
fn quotename_accepts_every_documented_delimiter() {
    let two = [varchar(130, true), varchar(1, true)];
    for (input, quote, expected) in [
        ("a>b", "<", "<a>>b>"),
        ("a]b", "]", "[a]]b]"),
        ("a'b", "'", "'a''b'"),
        ("a`b", "`", "`a``b`"),
        ("a}b", "{", "{a}}b}"),
        ("a)b", "(", "(a))b)"),
    ] {
        assert_eq!(
            eval(&QUOTENAME_DEF, &[text(input), text(quote)], &two),
            Ok(text(expected)),
            "QUOTENAME({input:?}, {quote:?})"
        );
    }
    // The result fills its declared length: 128 closing brackets give 258 characters.
    let brackets = "]".repeat(128);
    let quoted = eval(&QUOTENAME_DEF, &[text(&brackets)], &[varchar(130, true)])
        .expect("128 characters are accepted");
    assert_eq!(text_of(&quoted).expect("a string").chars().count(), 258);
}

#[test]
fn return_types() {
    assert_eq!(
        char_return_type(&[int(false)]).expect("one argument").ty,
        SqlType::Char(Len::Fixed(1))
    );
    assert_eq!(
        ascii_return_type(&[varchar(1, false)])
            .expect("one argument")
            .ty,
        SqlType::Int
    );
    assert_eq!(
        nchar_return_type(&[int(false)]).expect("one argument").ty,
        SqlType::NChar(Len::Fixed(1))
    );
    assert_eq!(
        unicode_return_type(&[nvarchar(1, false)])
            .expect("one argument")
            .ty,
        SqlType::Int
    );
    assert_eq!(
        quotename_return_type(&[varchar(10, true)])
            .expect("one argument")
            .ty,
        SqlType::NVarChar(Len::Fixed(QUOTENAME_RESULT_LENGTH))
    );
    // Every one of them may answer NULL, out of an out-of-range code, an empty string
    // or an unknown delimiter.
    for result in [
        char_return_type(&[int(false)]),
        ascii_return_type(&[varchar(1, false)]),
        nchar_return_type(&[int(false)]),
        unicode_return_type(&[nvarchar(1, false)]),
        quotename_return_type(&[varchar(10, false)]),
    ] {
        assert!(result.expect("one argument").nullable);
    }
}

#[test]
fn the_functions_are_registered_as_scalars() {
    crate::builtins::register_builtins();
    for name in ["concat", "CHAR", "Ascii", "NCHAR", "Unicode", "QuoteName"] {
        let def = crate::lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
        assert_eq!(def.kind, FunctionKind::Scalar);
        assert!(def.aggregate.is_none());
        assert!(def.deterministic);
    }
}
