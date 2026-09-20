use super::*;

/// The hexadecimal reading at the level of the private helper.
#[test]
fn hex_to_bytes_needs_an_even_count_of_hex_digits() {
    assert_eq!(hex_to_bytes("4E616D65"), Some(vec![0x4E, 0x61, 0x6D, 0x65]));
    assert_eq!(hex_to_bytes("4e616d65"), Some(vec![0x4E, 0x61, 0x6D, 0x65]));
    assert_eq!(hex_to_bytes(""), Some(vec![]));
    assert_eq!(hex_to_bytes("4E616D6"), None);
    assert_eq!(hex_to_bytes("ZZ"), None);
    assert_eq!(hex_to_bytes("0x4E"), None);
    // A multi-byte character is read as its bytes, none of which is a digit; the
    // function never indexes inside a character.
    assert_eq!(hex_to_bytes("é"), None);
}

#[test]
fn parse_guid_reads_the_five_groups_in_storage_order() {
    let expected = [
        0x25, 0x47, 0x98, 0x0E, 0x1C, 0xC5, 0xF4, 0x4B, 0x99, 0x60, 0xE1, 0xC8, 0x0E, 0x27, 0xAB,
        0xA0,
    ];
    assert_eq!(
        parse_guid("0E984725-C51C-4BF4-9960-E1C80E27ABA0"),
        Some(expected)
    );
    assert_eq!(
        parse_guid("{0e984725-c51c-4bf4-9960-e1c80e27aba0}"),
        Some(expected)
    );
    assert_eq!(parse_guid("0E984725C51C4BF49960E1C80E27ABA0"), None);
    assert_eq!(parse_guid("{0E984725-C51C-4BF4-9960-E1C80E27ABA0"), None);
    assert_eq!(parse_guid("0E984725-C51C-4BF4-9960-E1C80E27ABA0}"), None);
    assert_eq!(parse_guid(" 0E984725-C51C-4BF4-9960-E1C80E27ABA0"), None);
    assert_eq!(parse_guid("0E984725-C51C-4BF4-9960-E1C80E27ABA0-0"), None);
    assert_eq!(parse_guid("not-a-guid"), None);
    assert_eq!(parse_guid(""), None);
}

#[test]
fn encode_text_follows_the_kind_of_the_source() {
    let ascii = SqlType::VarChar(Len::Fixed(4));
    let national = SqlType::NVarChar(Len::Fixed(4));
    assert_eq!(encode_text("Name", &ascii), vec![0x4E, 0x61, 0x6D, 0x65]);
    assert_eq!(
        encode_text("Name", &national),
        vec![0x4E, 0x00, 0x61, 0x00, 0x6D, 0x00, 0x65, 0x00]
    );
    assert_eq!(encode_text("é", &ascii), vec![0xE9]);
    assert_eq!(encode_text("é", &national), vec![0xE9, 0x00]);
    assert_eq!(encode_text("中", &national), vec![0x2D, 0x4E]);
    // No code page 1252 byte: the `?` substitute.
    assert_eq!(encode_text("中", &ascii), vec![SUBSTITUTE]);
}

/// The two ends at the level of the helper, on vectors that tell them apart: under
/// [`Align::BYTES`] `binary(4)` of `0x0102` is `0x01020000`, under [`Align::NUMBER`]
/// it is `0x00000102`, and `binary(1)` keeps `0x01` on one side and `0x02` on the
/// other. A vector of equal length would answer the same thing on both and would prove
/// nothing.
///
/// The fourth combination, padded at the head and cut at the tail, is
/// [`Align::DECIMAL`], pinned on the bytes of `CAST(1.5 AS decimal(5,2))`,
/// `0x0502000196000000`: `0x050200` at `binary(3)` and four leading zero bytes at
/// `binary(12)`.
#[test]
fn fit_pads_and_truncates_at_the_end_the_alignment_names() {
    let value = || vec![0x01_u8, 0x02];
    let binary = |n: u16| SqlType::Binary(Len::Fixed(n));
    let varbinary = |n: u16| SqlType::VarBinary(Len::Fixed(n));

    assert_eq!(fit(value(), &binary(4), Align::BYTES), Ok(vec![1, 2, 0, 0]));
    assert_eq!(
        fit(value(), &binary(4), Align::NUMBER),
        Ok(vec![0, 0, 1, 2])
    );
    assert_eq!(fit(value(), &binary(1), Align::BYTES), Ok(vec![1]));
    assert_eq!(fit(value(), &binary(1), Align::NUMBER), Ok(vec![2]));
    assert_eq!(fit(value(), &binary(2), Align::BYTES), Ok(value()));
    assert_eq!(fit(value(), &binary(2), Align::NUMBER), Ok(value()));

    // `varbinary(n)` truncates at the same end and never pads, whatever the alignment.
    assert_eq!(fit(value(), &varbinary(4), Align::BYTES), Ok(value()));
    assert_eq!(fit(value(), &varbinary(4), Align::NUMBER), Ok(value()));
    assert_eq!(fit(value(), &varbinary(1), Align::BYTES), Ok(vec![1]));
    assert_eq!(fit(value(), &varbinary(1), Align::NUMBER), Ok(vec![2]));

    // `varbinary(max)` keeps everything, on the three alignments.
    for align in [Align::BYTES, Align::NUMBER, Align::SCALED] {
        assert_eq!(
            fit(value(), &SqlType::VarBinary(Len::Max), align),
            Ok(value())
        );
    }

    // `Align::SCALED` pads at the tail like `Align::BYTES` and parts from it on the
    // narrow targets, which it refuses: the two widths are what tell them apart.
    assert_eq!(
        fit(value(), &binary(4), Align::SCALED),
        Ok(vec![1, 2, 0, 0])
    );
    assert_eq!(fit(value(), &varbinary(4), Align::SCALED), Ok(value()));
    assert_eq!(fit(value(), &binary(2), Align::SCALED), Ok(value()));
    for to in [binary(1), varbinary(1)] {
        let e = fit(value(), &to, Align::SCALED).expect_err("narrower than the value");
        assert_eq!(e.number, 8152);
        assert_eq!(e.severity, 16);
        assert_eq!(e.state, 17);
        assert_eq!(
            e.message,
            "Data too long: the string or binary value would be cut."
        );
        assert!(fit(value(), &to, Align::BYTES).is_ok());
    }

    // A length of zero is refused before the conversion, by SQL Server as by the
    // binder; the function still has to answer something, and it is the empty binary
    // whichever end it would have cut — and 8152 where it would have refused.
    for align in [Align::BYTES, Align::NUMBER] {
        assert_eq!(fit(value(), &binary(0), align), Ok(Vec::<u8>::new()));
        assert_eq!(fit(value(), &varbinary(0), align), Ok(Vec::<u8>::new()));
    }
    assert!(fit(value(), &binary(0), Align::SCALED).is_err());

    // The `decimal` combination: padded at the head like a number, cut at the tail
    // like a byte string. Neither of the two other alignments answers both of these,
    // which is what makes it a vector and not a coincidence.
    let decimal = Align::DECIMAL;
    let d = || vec![0x05_u8, 0x02, 0x00, 0x01, 0x96, 0x00, 0x00, 0x00];
    assert_eq!(fit(d(), &binary(3), decimal), Ok(vec![0x05, 0x02, 0x00]));
    assert_eq!(fit(d(), &varbinary(3), decimal), Ok(vec![0x05, 0x02, 0x00]));
    assert_eq!(
        fit(d(), &binary(12), decimal),
        Ok(vec![
            0, 0, 0, 0, 0x05, 0x02, 0x00, 0x01, 0x96, 0x00, 0x00, 0x00
        ])
    );
    assert_eq!(fit(d(), &varbinary(12), decimal), Ok(d()));
    // `Align::BYTES` agrees on the truncation and differs on the padding,
    // `Align::NUMBER` the other way round: it takes both widths to separate the three,
    // and one width alone would prove nothing.
    assert_eq!(
        fit(d(), &binary(3), Align::BYTES),
        fit(d(), &binary(3), decimal)
    );
    assert_ne!(
        fit(d(), &binary(12), Align::BYTES),
        fit(d(), &binary(12), decimal)
    );
    assert_ne!(
        fit(d(), &binary(3), Align::NUMBER),
        fit(d(), &binary(3), decimal)
    );
    assert_eq!(
        fit(d(), &binary(12), Align::NUMBER),
        fit(d(), &binary(12), decimal)
    );
}

/// The three stored layouts of 2008, at the level of the helpers, on the bytes of
/// `'2000-01-02 13:05:06.1234567'`.
#[test]
fn the_2008_helpers_write_the_scale_byte_then_the_storage() {
    assert_eq!(date_bytes(Date { days: 730_120 }), [0x08, 0x24, 0x0B]);
    assert_eq!(date_bytes(Date { days: 0 }), [0x00, 0x00, 0x00]);
    assert_eq!(date_bytes(Date { days: 3_652_058 }), [0xDA, 0xB9, 0x37]);

    let hex = |bytes: Vec<u8>| {
        bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join("")
    };
    // The eight scales, and with them the three widths 4, 5 and 6. The ticks are those
    // of `13:05:06.1234567` snapped to each scale, which is the value the conversions
    // store — the engine rounds the fraction while reading the string, so the scale 4
    // holds `.1235` and not `.1234`.
    for (scale, ticks_100ns, expected) in [
        (0, 471_060_000_000_u64, "0002B800"),
        (1, 471_061_000_000, "01153007"),
        (2, 471_061_200_000, "02D4E047"),
        (3, 471_061_230_000, "034BC8CE02"),
        (4, 471_061_235_000, "04F3D2131C"),
        (5, 471_061_234_600, "057A3DC61801"),
        (6, 471_061_234_570, "06C166BEF70A"),
        (7, 471_061_234_567, "07870370AD6D"),
    ] {
        let bytes = scaled_time_bytes(Time { ticks_100ns }, scale);
        assert_eq!(hex(bytes.clone()), expected, "time({scale})");
        assert_eq!(bytes.len(), 1 + time_width(scale));
    }
    assert_eq!(
        scaled_time_bytes(Time { ticks_100ns: 0 }, 7),
        vec![0x07, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        hex(scaled_time_bytes(
            Time {
                ticks_100ns: 863_999_999_999
            },
            7
        )),
        "07FFBF692AC9"
    );
}

/// The two stored layouts, at the level of the helpers.
#[test]
fn the_date_helpers_write_the_stored_layout() {
    assert_eq!(
        datetime_bytes(DateTime {
            days: 1,
            ticks_300th: 12_960_000
        }),
        [0x00, 0x00, 0x00, 0x01, 0x00, 0xC5, 0xC1, 0x00]
    );
    assert_eq!(
        datetime_bytes(DateTime {
            days: -2,
            ticks_300th: 12_960_000
        }),
        [0xFF, 0xFF, 0xFF, 0xFE, 0x00, 0xC5, 0xC1, 0x00]
    );
    assert_eq!(
        smalldatetime_bytes(DateTime {
            days: 65_535,
            ticks_300th: 1_439 * TICKS_300TH_PER_MINUTE
        }),
        [0xFF, 0xFF, 0x05, 0x9F]
    );
    assert_eq!(
        smalldatetime_bytes(DateTime {
            days: 0,
            ticks_300th: 0
        }),
        [0x00, 0x00, 0x00, 0x00]
    );
}

/// The header, the sign byte and the four mantissa widths.
#[test]
fn decimal_bytes_write_the_header_then_the_mantissa_on_four_byte_steps() {
    let hex = |bytes: Vec<u8>| {
        bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join("")
    };
    let decimal = |mantissa: i128, precision: u8, scale: u8| Decimal {
        mantissa,
        precision,
        scale,
    };
    assert_eq!(hex(decimal_bytes(&decimal(150, 5, 2))), "0502000196000000");
    assert_eq!(hex(decimal_bytes(&decimal(-150, 5, 2))), "0502000096000000");
    assert_eq!(hex(decimal_bytes(&decimal(0, 5, 2))), "0502000100000000");
    assert_eq!(
        hex(decimal_bytes(&decimal(15_000, 38, 4))),
        "26040001983A0000"
    );
    assert_eq!(hex(decimal_bytes(&decimal(15, 2, 1))), "020100010F000000");
    // The four widths of the mantissa: a step of four bytes at each power of 2^32.
    assert_eq!(
        hex(decimal_bytes(&decimal((1 << 32) - 1, 20, 0))),
        "14000001FFFFFFFF"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal(1 << 32, 20, 0))),
        "140000010000000001000000"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal(-(1 << 32), 20, 0))),
        "140000000000000001000000"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal((1 << 64) - 1, 20, 0))),
        "14000001FFFFFFFFFFFFFFFF"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal(1 << 64, 20, 0))),
        "14000001000000000000000001000000"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal((1 << 96) - 1, 38, 0))),
        "26000001FFFFFFFFFFFFFFFFFFFFFFFF"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal(1 << 96, 38, 0))),
        "2600000100000000000000000000000001000000"
    );
    let largest = 99_999_999_999_999_999_999_999_999_999_999_999_999_i128;
    assert_eq!(
        hex(decimal_bytes(&decimal(largest, 38, 0))),
        "26000001FFFFFFFF3F228A097AC4865AA84C3B4B"
    );
    assert_eq!(
        hex(decimal_bytes(&decimal(-largest, 38, 0))),
        "26000000FFFFFFFF3F228A097AC4865AA84C3B4B"
    );
}

/// The low half of the amount, on three vectors.
#[test]
fn smallmoney_bytes_keep_the_low_half_of_the_amount() {
    assert_eq!(smallmoney_bytes(15_000), [0x00, 0x00, 0x3A, 0x98]);
    assert_eq!(smallmoney_bytes(-15_000), [0xFF, 0xFF, 0xC5, 0x68]);
    assert_eq!(smallmoney_bytes(2_147_483_647), [0x7F, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn guid_from_bytes_pads_and_truncates() {
    assert_eq!(guid_from_bytes(&[0x41])[0], 0x41);
    assert_eq!(guid_from_bytes(&[0x41])[1..], [0u8; 15]);
    assert_eq!(guid_from_bytes(&[0xFF; 20]), [0xFF; GUID_BYTES]);
    assert_eq!(guid_from_bytes(&[]), [0u8; GUID_BYTES]);
}
