//! Integration tests of the binary and `uniqueidentifier` conversions: the three binary
//! styles in both directions, the padding and truncation rules of `binary(n)` and
//! `varbinary(n)`, the textual form of a `uniqueidentifier` and the order of two of them;
//! then the numeric sources, one family at a time, each on its two ends.

use std::cmp::Ordering;

use vauban_types::{
    Collation, Decimal, Len, SqlString, SqlType, TypeInfo, Value, compare, convert,
};

/// The 16 stored bytes of `0E984725-C51C-4BF4-9960-E1C80E27ABA0`: the three first groups
/// little-endian, the two last in reading order.
const GUID: [u8; 16] = [
    0x25, 0x47, 0x98, 0x0E, 0x1C, 0xC5, 0xF4, 0x4B, 0x99, 0x60, 0xE1, 0xC8, 0x0E, 0x27, 0xAB, 0xA0,
];

/// The textual form of [`GUID`], the way SQL Server writes it: upper case, no braces.
const GUID_TEXT: &str = "0E984725-C51C-4BF4-9960-E1C80E27ABA0";

fn ti(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, true)
}

fn text(t: &str) -> Value {
    Value::String(SqlString { text: t.to_owned() })
}

fn varchar(n: u16) -> TypeInfo {
    ti(SqlType::VarChar(Len::Fixed(n)))
}

fn varbinary(n: u16) -> TypeInfo {
    ti(SqlType::VarBinary(Len::Fixed(n)))
}

/// The converted value, when the conversion succeeds.
fn ok(v: &Value, from: &TypeInfo, to: &TypeInfo, style: Option<i32>) -> Value {
    convert(v, from, to, style).expect("conversion expected to succeed")
}

/// The number and the message of a conversion that fails.
fn err(v: &Value, from: &TypeInfo, to: &TypeInfo, style: Option<i32>) -> (u32, String) {
    let e = convert(v, from, to, style).expect_err("conversion expected to fail");
    (e.number, e.message)
}

/// Styles 0, 1 and 2 of a character source, and the two ways of getting them wrong: an
/// odd count of digits, and a style that does not exist.
#[test]
fn string_to_binary_styles() {
    let name = text("Name");
    let binary8 = ti(SqlType::Binary(Len::Fixed(8)));
    let binary4 = ti(SqlType::Binary(Len::Fixed(4)));

    // Style 0: one code page 1252 byte per character, then the `0x00` padding of `binary(8)`.
    assert_eq!(
        ok(&name, &varchar(4), &binary8, Some(0)),
        Value::Bytes(vec![0x4E, 0x61, 0x6D, 0x65, 0, 0, 0, 0])
    );
    // No style at all is style 0.
    assert_eq!(
        ok(&name, &varchar(4), &binary8, None),
        Value::Bytes(vec![0x4E, 0x61, 0x6D, 0x65, 0, 0, 0, 0])
    );
    // Style 1: hexadecimal digits behind the mandatory `0x`.
    assert_eq!(
        ok(&text("0x4E616D65"), &varchar(10), &binary4, Some(1)),
        Value::Bytes(vec![0x4E, 0x61, 0x6D, 0x65])
    );
    // Style 2: the same digits, this time without the prefix.
    assert_eq!(
        ok(&text("4E616D65"), &varchar(8), &binary4, Some(2)),
        Value::Bytes(vec![0x4E, 0x61, 0x6D, 0x65])
    );
    // An `nvarchar` source is UTF-16LE, not the code page: `CONVERT(binary(8), N'Name', 0)`
    // is `0x4E0061006D006500`.
    let nvarchar = ti(SqlType::NVarChar(Len::Fixed(4)));
    assert_eq!(
        ok(&name, &nvarchar, &binary8, Some(0)),
        Value::Bytes(vec![0x4E, 0, 0x61, 0, 0x6D, 0, 0x65, 0])
    );

    // An odd count of digits, a digit that is not one, the prefix where it is forbidden and
    // the prefix missing where it is required: all 8114, and the message names `varbinary`
    // even though the target is a `binary(4)`.
    let converting = "Data type varchar could not be converted to varbinary.".to_owned();
    assert_eq!(
        err(&text("4E616D6"), &varchar(7), &binary4, Some(2)),
        (8114, converting.clone())
    );
    assert_eq!(
        err(&text("ZZ"), &varchar(2), &binary4, Some(2)),
        (8114, converting.clone())
    );
    assert_eq!(
        err(&text("0x4E616D65"), &varchar(10), &binary4, Some(2)),
        (8114, converting.clone())
    );
    assert_eq!(
        err(&text("4E616D65"), &varchar(8), &binary4, Some(1)),
        (8114, converting)
    );

    // Any style other than 0, 1 and 2 is 9809.
    assert_eq!(
        err(&text("41"), &varchar(2), &binary4, Some(3)),
        (
            9809,
            "Style 3 is not defined for converting varchar to varbinary.".to_owned()
        )
    );
}

/// Styles 0, 1 and 2 of a binary source rendered as text, including the truncation the two
/// characters of the `0x` prefix cause.
#[test]
fn binary_to_string_styles() {
    let name = Value::Bytes(vec![0x4E, 0x61, 0x6D, 0x65]);
    let source = varbinary(4);
    let char8 = ti(SqlType::Char(Len::Fixed(8)));

    assert_eq!(ok(&name, &source, &char8, Some(0)), text("Name    "));
    assert_eq!(ok(&name, &source, &char8, Some(1)), text("0x4E616D"));
    assert_eq!(ok(&name, &source, &char8, Some(2)), text("4E616D65"));
    // No style reads the bytes as characters, as style 0 does.
    assert_eq!(ok(&name, &source, &varchar(20), None), text("Name"));

    // Style 1 keeps whole bytes only: `varchar(3)` has room for `0x` and half a byte, which
    // is no byte at all, and `varchar(1)` has no room even for the prefix.
    assert_eq!(
        ok(&name, &source, &varchar(40), Some(1)),
        text("0x4E616D65")
    );
    assert_eq!(ok(&name, &source, &varchar(3), Some(1)), text("0x"));
    assert_eq!(ok(&name, &source, &varchar(1), Some(1)), text(""));
    assert_eq!(ok(&name, &source, &varchar(3), Some(2)), text("4E"));
    assert_eq!(ok(&name, &source, &varchar(4), Some(2)), text("4E61"));

    // Style 0 of a national target reads the bytes as UTF-16LE instead of the code page.
    let nvarchar = ti(SqlType::NVarChar(Len::Fixed(20)));
    assert_eq!(ok(&name, &source, &nvarchar, Some(0)), text("慎敭"));
    assert_eq!(ok(&name, &source, &nvarchar, Some(2)), text("4E616D65"));

    // Code page 1252, not ASCII: `0xE9` is `é` and `0x80` is the euro sign.
    let one = varbinary(1);
    assert_eq!(
        ok(&Value::Bytes(vec![0xE9]), &one, &varchar(20), Some(0)),
        text("é")
    );
    assert_eq!(
        ok(&Value::Bytes(vec![0x80]), &one, &varchar(20), Some(0)),
        text("€")
    );

    assert_eq!(
        err(&name, &source, &char8, Some(3)),
        (
            9809,
            "Style 3 is not defined for converting varbinary to varchar.".to_owned()
        )
    );
}

/// `binary(n)` pads on the right, both types truncate on the right, `varbinary(max)` keeps
/// everything.
#[test]
fn binary_padding_and_truncation() {
    let one = varbinary(1);
    let five = varbinary(5);
    let a = Value::Bytes(vec![0x41]);
    let long = Value::Bytes(vec![0x01, 0x02, 0x03, 0x04, 0x05]);

    assert_eq!(
        ok(&a, &one, &ti(SqlType::Binary(Len::Fixed(4))), None),
        Value::Bytes(vec![0x41, 0, 0, 0])
    );
    assert_eq!(
        ok(&long, &five, &varbinary(3), None),
        Value::Bytes(vec![0x01, 0x02, 0x03])
    );
    assert_eq!(
        ok(&long, &five, &ti(SqlType::Binary(Len::Fixed(3))), None),
        Value::Bytes(vec![0x01, 0x02, 0x03])
    );
    assert_eq!(
        ok(&a, &one, &ti(SqlType::VarBinary(Len::Max)), None),
        Value::Bytes(vec![0x41])
    );
}

fn binary(n: u16) -> TypeInfo {
    ti(SqlType::Binary(Len::Fixed(n)))
}

fn varbinary_max() -> TypeInfo {
    ti(SqlType::VarBinary(Len::Max))
}

/// The bytes of a successful conversion, as upper-case hexadecimal without the `0x`.
fn hex(v: &Value, from: &TypeInfo, to: &TypeInfo) -> String {
    match ok(v, from, to, None) {
        Value::Bytes(bytes) => bytes.iter().map(|b| format!("{b:02X}")).collect(),
        other => panic!("expected bytes, got {other:?}"),
    }
}

/// The sweep applied to each family: `varbinary(max)` for the storage, then `binary(1)`,
/// one below the storage, the storage, one above, 12,
/// and `varbinary(1)`, one below, the storage, one above, 12. `expected` is the storage in
/// hexadecimal; the padding is at the head for the numeric sources this file sweeps, and
/// `keep_head` says which end survives a narrow target, so that a family given the wrong
/// end fails on the width that tells the two apart.
fn sweep(v: &Value, from: &TypeInfo, expected: &str, keep_head: bool) {
    let storage = expected.len() / 2;
    assert_eq!(hex(v, from, &varbinary_max()), expected, "varbinary(max)");
    let cut = |width: usize| -> String {
        if keep_head {
            expected[..width * 2].to_owned()
        } else {
            expected[(storage - width) * 2..].to_owned()
        }
    };
    let pad = |width: usize| -> String { "00".repeat(width - storage) + expected };
    let widths_below: Vec<usize> = [1, storage - 1]
        .into_iter()
        .filter(|w| *w >= 1 && *w < storage)
        .collect();
    for w in widths_below {
        let n = u16::try_from(w).expect("width fits");
        assert_eq!(hex(v, from, &binary(n)), cut(w), "binary({n})");
        assert_eq!(hex(v, from, &varbinary(n)), cut(w), "varbinary({n})");
    }
    let n = u16::try_from(storage).expect("width fits");
    assert_eq!(hex(v, from, &binary(n)), expected, "binary({n})");
    assert_eq!(hex(v, from, &varbinary(n)), expected, "varbinary({n})");
    for w in [storage + 1, 12] {
        let n = u16::try_from(w).expect("width fits");
        assert_eq!(hex(v, from, &binary(n)), pad(w), "binary({n})");
        assert_eq!(hex(v, from, &varbinary(n)), expected, "varbinary({n})");
    }
}

/// Three vectors, then the four integers on the sweep: big-endian two's complement,
/// padded at the head, tail kept.
#[test]
fn integers_to_binary_pad_at_the_head_and_keep_the_tail() {
    let int = ti(SqlType::Int);
    assert_eq!(hex(&Value::I32(258), &int, &binary(2)), "0102");
    assert_eq!(hex(&Value::I32(258), &int, &binary(8)), "0000000000000102");
    assert_eq!(
        hex(&Value::F64(1.5), &ti(SqlType::Float), &binary(4)),
        "00000000"
    );

    sweep(&Value::I32(258), &int, "00000102", false);
    sweep(&Value::I32(-2), &int, "FFFFFFFE", false);
    sweep(
        &Value::I64(258),
        &ti(SqlType::BigInt),
        "0000000000000102",
        false,
    );
    sweep(
        &Value::I64(i64::MIN),
        &ti(SqlType::BigInt),
        "8000000000000000",
        false,
    );
    sweep(&Value::I16(258), &ti(SqlType::SmallInt), "0102", false);
    sweep(&Value::I16(-2), &ti(SqlType::SmallInt), "FFFE", false);
    // A `tinyint` is unsigned: 255 is `0xFF`, and its one byte has no narrower width.
    sweep(&Value::I8(255), &ti(SqlType::TinyInt), "FF", false);
    sweep(&Value::I8(0), &ti(SqlType::TinyInt), "00", false);
}

/// One byte, padded at the head.
#[test]
fn bit_to_binary_is_one_byte_padded_at_the_head() {
    let bit = ti(SqlType::Bit);
    sweep(&Value::Bit(true), &bit, "01", false);
    sweep(&Value::Bit(false), &bit, "00", false);
    assert_eq!(hex(&Value::Bit(true), &bit, &binary(2)), "0001");
}

/// The IEEE 754 bytes, big-endian, sign bit included; a narrow target keeps the tail and
/// loses the exponent in silence.
#[test]
fn float_and_real_to_binary_write_their_ieee_bytes() {
    let float = ti(SqlType::Float);
    let real = ti(SqlType::Real);
    sweep(&Value::F64(1.5), &float, "3FF8000000000000", false);
    sweep(&Value::F64(-1.5), &float, "BFF8000000000000", false);
    sweep(&Value::F64(0.0), &float, "0000000000000000", false);
    sweep(&Value::F64(1e308), &float, "7FE1CCF385EBC8A0", false);
    sweep(&Value::F32(1.5), &real, "3FC00000", false);
    sweep(&Value::F32(-1.5), &real, "BFC00000", false);
    sweep(&Value::F32(3.4e38), &real, "7F7FC99E", false);
    // The negative zero keeps its sign bit: `CAST(0 AS float) * -1` is `0x80…` on
    // SQL Server, and only the literal `-0.0` — an exact zero first — is `0x00…`.
    assert_eq!(
        hex(&Value::F64(-0.0), &float, &varbinary_max()),
        "8000000000000000"
    );
    assert_eq!(hex(&Value::F32(-0.0), &real, &varbinary_max()), "80000000");
}

/// The amount in ten-thousandths, big-endian on eight or four bytes.
#[test]
fn money_and_smallmoney_to_binary_write_the_amount_big_endian() {
    let money = ti(SqlType::Money);
    let smallmoney = ti(SqlType::SmallMoney);
    sweep(&Value::Money(15_000), &money, "0000000000003A98", false);
    sweep(&Value::Money(-15_000), &money, "FFFFFFFFFFFFC568", false);
    sweep(&Value::Money(i64::MAX), &money, "7FFFFFFFFFFFFFFF", false);
    sweep(&Value::Money(i64::MIN), &money, "8000000000000000", false);
    sweep(&Value::Money(15_000), &smallmoney, "00003A98", false);
    sweep(&Value::Money(-15_000), &smallmoney, "FFFFC568", false);
    sweep(&Value::Money(2_147_483_647), &smallmoney, "7FFFFFFF", false);
}

fn decimal(mantissa: i128, precision: u8, scale: u8) -> (Value, TypeInfo) {
    (
        Value::Decimal(Decimal {
            mantissa,
            precision,
            scale,
        }),
        ti(SqlType::Decimal { precision, scale }),
    )
}

/// The header then the little-endian mantissa, padded at the head like a number and cut
/// at the tail like a byte string: the family whose two ends part.
#[test]
fn decimal_to_binary_pads_at_the_head_and_keeps_the_head() {
    let (v, from) = decimal(150, 5, 2);
    // The two ends: `binary(3)` keeps the header, `binary(12)`
    // puts four zero bytes in front. Under the integers' pair `binary(3)` would answer
    // `0x960000` and under the byte strings' pair `binary(12)` would end in zero bytes.
    assert_eq!(hex(&v, &from, &binary(3)), "050200");
    assert_eq!(hex(&v, &from, &binary(12)), "000000000502000196000000");
    sweep(&v, &from, "0502000196000000", true);

    let (v, from) = decimal(-150, 5, 2);
    sweep(&v, &from, "0502000096000000", true);
    // A `numeric` writes the same bytes as a `decimal`.
    let numeric = ti(SqlType::Numeric {
        precision: 5,
        scale: 2,
    });
    assert_eq!(
        hex(&decimal(150, 5, 2).0, &numeric, &varbinary_max()),
        "0502000196000000"
    );
    // Eight bytes at the precision 38 too: the width follows the value, not the
    // precision, so `binary(17)` pads nine bytes in front.
    let (v, from) = decimal(15_000, 38, 4);
    sweep(&v, &from, "26040001983A0000", true);
    assert_eq!(
        hex(&v, &from, &binary(17)),
        "00000000000000000026040001983A0000"
    );
    let (v, from) = decimal(0, 5, 2);
    sweep(&v, &from, "0502000100000000", true);
    let (v, from) = decimal(0, 38, 0);
    sweep(&v, &from, "2600000100000000", true);
    let (v, from) = decimal(1_234_567, 10, 4);
    sweep(&v, &from, "0A04000187D61200", true);
    let (v, from) = decimal(15, 2, 1);
    sweep(&v, &from, "020100010F000000", true);

    // The four mantissa widths, and the sweep on the twelve-byte one in both signs.
    let cases: [(i128, u8, &str); 9] = [
        ((1 << 32) - 1, 20, "14000001FFFFFFFF"),
        (1 << 32, 20, "140000010000000001000000"),
        (-(1 << 32), 20, "140000000000000001000000"),
        ((1 << 64) - 1, 20, "14000001FFFFFFFFFFFFFFFF"),
        (1 << 64, 20, "14000001000000000000000001000000"),
        ((1 << 96) - 1, 38, "26000001FFFFFFFFFFFFFFFFFFFFFFFF"),
        (1 << 96, 38, "2600000100000000000000000000000001000000"),
        (
            99_999_999_999_999_999_999_999_999_999_999_999_999,
            38,
            "26000001FFFFFFFF3F228A097AC4865AA84C3B4B",
        ),
        (
            -99_999_999_999_999_999_999_999_999_999_999_999_999,
            38,
            "26000000FFFFFFFF3F228A097AC4865AA84C3B4B",
        ),
    ];
    for (mantissa, precision, expected) in cases {
        let (v, from) = decimal(mantissa, precision, 0);
        assert_eq!(hex(&v, &from, &varbinary_max()), expected, "{mantissa}");
    }
    let (v, from) = decimal(1 << 32, 20, 0);
    assert_eq!(hex(&v, &from, &binary(1)), "14");
    assert_eq!(hex(&v, &from, &binary(11)), "1400000100000000010000");
    assert_eq!(hex(&v, &from, &binary(13)), "00140000010000000001000000");
    let (v, from) = decimal(-(1 << 32), 20, 0);
    assert_eq!(hex(&v, &from, &binary(5)), "1400000000");
    assert_eq!(
        hex(&v, &from, &binary(16)),
        "00000000140000000000000001000000"
    );
    let (v, from) = decimal(12_345_678_901_234_567_890, 20, 0);
    assert_eq!(hex(&v, &from, &varbinary_max()), "14000001D20A1FEB8CA954AB");
}

/// A style number is ignored on a numeric source, as on a binary one: no 9809.
#[test]
fn number_to_binary_ignores_the_style() {
    for style in [Some(1), Some(2), Some(3), Some(126)] {
        assert_eq!(
            ok(&Value::I32(258), &ti(SqlType::Int), &binary(4), style),
            Value::Bytes(vec![0, 0, 1, 2]),
            "style {style:?}"
        );
    }
    let (v, from) = decimal(150, 5, 2);
    assert_eq!(
        ok(&v, &from, &binary(8), Some(1)),
        Value::Bytes(vec![0x05, 0x02, 0x00, 0x01, 0x96, 0, 0, 0])
    );
    assert_eq!(
        ok(&Value::F32(1.5), &ti(SqlType::Real), &binary(4), Some(3)),
        Value::Bytes(vec![0x3F, 0xC0, 0, 0])
    );
    assert_eq!(
        ok(&Value::Bit(true), &ti(SqlType::Bit), &binary(1), Some(1)),
        Value::Bytes(vec![0x01])
    );
}

/// The bytes written here read back through the binary-to-number conversion, at the
/// storage width, a wider one and a narrower one.
///
/// `float` and `real` have no way back (529), and a `decimal` reads back in SQL Server but
/// not here, a deliberate difference: 8114 is asserted so that the day it changes, this
/// test says so.
#[test]
fn number_to_binary_round_trip() {
    let round_trip = |v: &Value, from: &TypeInfo, width: u16| {
        let bytes = ok(v, from, &binary(width), None);
        ok(&bytes, &binary(width), from, None)
    };
    let int = ti(SqlType::Int);
    for width in [4, 8, 2] {
        assert_eq!(round_trip(&Value::I32(258), &int, width), Value::I32(258));
    }
    assert_eq!(round_trip(&Value::I32(-2), &int, 4), Value::I32(-2));
    assert_eq!(
        round_trip(&Value::I64(258), &ti(SqlType::BigInt), 8),
        Value::I64(258)
    );
    assert_eq!(
        round_trip(&Value::I16(258), &ti(SqlType::SmallInt), 2),
        Value::I16(258)
    );
    assert_eq!(
        round_trip(&Value::I8(255), &ti(SqlType::TinyInt), 1),
        Value::I8(255)
    );
    for width in [1, 12] {
        assert_eq!(
            round_trip(&Value::Bit(true), &ti(SqlType::Bit), width),
            Value::Bit(true)
        );
    }
    assert_eq!(
        round_trip(&Value::Money(15_000), &ti(SqlType::Money), 8),
        Value::Money(15_000)
    );
    assert_eq!(
        round_trip(&Value::Money(-15_000), &ti(SqlType::SmallMoney), 4),
        Value::Money(-15_000)
    );
    // A `money` cut to four bytes comes back unsigned, `429495.2296`, the way the other
    // direction reads a short byte string.
    assert_eq!(
        round_trip(&Value::Money(-15_000), &ti(SqlType::Money), 4),
        Value::Money(4_294_952_296)
    );

    let (v, from) = decimal(150, 5, 2);
    let bytes = ok(&v, &from, &binary(8), None);
    assert_eq!(err(&bytes, &binary(8), &from, None).0, 8114);
    let bytes = ok(&Value::F64(1.5), &ti(SqlType::Float), &binary(8), None);
    assert_eq!(err(&bytes, &binary(8), &ti(SqlType::Float), None).0, 8114);
}

/// `NULL` stays `NULL` towards a width narrower than the storage.
#[test]
fn null_number_to_binary_stays_null() {
    assert_eq!(
        ok(&Value::Null, &ti(SqlType::Int), &binary(2), None),
        Value::Null
    );
    assert_eq!(
        ok(&Value::Null, &decimal(0, 5, 2).1, &binary(1), None),
        Value::Null
    );
    assert_eq!(
        ok(&Value::Null, &ti(SqlType::Float), &varbinary(1), None),
        Value::Null
    );
}

/// The `8-4-4-4-12` form, with or without braces, in either case; anything else is 8169.
#[test]
fn string_to_guid() {
    let guid = ti(SqlType::UniqueIdentifier);
    assert_eq!(
        ok(&text(GUID_TEXT), &varchar(36), &guid, None),
        Value::Guid(GUID)
    );
    assert_eq!(
        ok(&text(&GUID_TEXT.to_lowercase()), &varchar(36), &guid, None),
        Value::Guid(GUID)
    );
    assert_eq!(
        ok(
            &text(&format!("{{{GUID_TEXT}}}")),
            &varchar(38),
            &guid,
            None
        ),
        Value::Guid(GUID)
    );

    let failed = "The character string could not be converted to uniqueidentifier.".to_owned();
    assert_eq!(
        err(&text("not-a-guid"), &varchar(10), &guid, None),
        (8169, failed.clone())
    );
    // No dash, one brace, a group too short, a surrounding space: 8169 all the same.
    for wrong in [
        "0E984725C51C4BF49960E1C80E27ABA0",
        "{0E984725-C51C-4BF4-9960-E1C80E27ABA0",
        "0E984725-C51C-4BF4-9960-E1C80E27ABA",
        "  0E984725-C51C-4BF4-9960-E1C80E27ABA0  ",
        "",
    ] {
        assert_eq!(
            err(&text(wrong), &varchar(40), &guid, None),
            (8169, failed.clone()),
            "{wrong}"
        );
    }
}

/// A `uniqueidentifier` towards text and towards bytes, and back from bytes.
#[test]
fn guid_to_string_and_binary() {
    let guid = ti(SqlType::UniqueIdentifier);
    let value = Value::Guid(GUID);

    assert_eq!(ok(&value, &guid, &varchar(40), None), text(GUID_TEXT));
    assert_eq!(
        ok(&value, &guid, &ti(SqlType::Binary(Len::Fixed(16))), None),
        Value::Bytes(GUID.to_vec())
    );
    assert_eq!(
        ok(
            &Value::Bytes(GUID.to_vec()),
            &ti(SqlType::Binary(Len::Fixed(16))),
            &guid,
            None
        ),
        Value::Guid(GUID)
    );

    // There is **no** error at all: `SELECT CAST(CAST(0x41 AS binary(15)) AS
    // uniqueidentifier);` yields `00000041-0000-0000-0000-000000000000`: the bytes are
    // padded on the
    // right with `0x00`, exactly as a `binary(16)` target would pad them. The 8114 the
    // acceptance criterion expected does not exist.
    let mut short = vec![0x41];
    short.resize(15, 0);
    let mut padded = [0u8; 16];
    padded[0] = 0x41;
    assert_eq!(
        ok(
            &Value::Bytes(short),
            &ti(SqlType::Binary(Len::Fixed(15))),
            &guid,
            None
        ),
        Value::Guid(padded)
    );

    // A source that is neither a string, nor bytes, nor a guid: 529, not 8114.
    assert_eq!(
        err(&Value::I32(1), &ti(SqlType::Int), &guid, None),
        (
            529,
            "No explicit conversion exists from int to uniqueidentifier.".to_owned()
        )
    );
}

/// The order of two `uniqueidentifier` values, which is not the order of their stored
/// bytes: `0` then `1` to the two `CASE WHEN … < … THEN 1 ELSE 0 END` below.
#[test]
fn guid_comparison_order() {
    let guid = ti(SqlType::UniqueIdentifier);
    let as_guid = |t: &str| ok(&text(t), &varchar(36), &guid, None);
    let order = |a: &str, b: &str| {
        compare(&as_guid(a), &as_guid(b), &Collation::DEFAULT).expect("two guids")
    };

    // The last group weighs more than the first one, which the ordinal order gets backwards.
    assert_eq!(
        order(
            "00000000-0000-0000-0000-000000000001",
            "01000000-0000-0000-0000-000000000000"
        ),
        Some(Ordering::Greater)
    );
    // The fourth group weighs more than the fifth one... in the other direction: the fifth
    // is read first, and it is `000000000000` against `000000000001`.
    assert_eq!(
        order(
            "00000000-0000-0000-0001-000000000000",
            "00000000-0000-0000-0000-000000000001"
        ),
        Some(Ordering::Less)
    );
    // Inside the first group the stored bytes are read left to right, so the little-endian
    // storage reverses the numeric order.
    assert_eq!(
        order(
            "01000000-0000-0000-0000-000000000000",
            "00000100-0000-0000-0000-000000000000"
        ),
        Some(Ordering::Less)
    );
    assert_eq!(order(GUID_TEXT, GUID_TEXT), Some(Ordering::Equal));
}
