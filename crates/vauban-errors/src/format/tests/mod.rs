use super::*;

#[test]
fn format_substitutes_percent_dot_star_ls() {
    assert_eq!(
        format_message("Invalid object name '%.*ls'.", &[Arg::Str("dbo.t")]),
        "Invalid object name 'dbo.t'."
    );
}

#[test]
fn format_substitutes_each_string_specifier_once() {
    assert_eq!(
        format_message(
            "a=%.*ls b=%ls c=%s d=%S_MSG e=%hs",
            &[
                Arg::Str("1"),
                Arg::Str("2"),
                Arg::Str("3"),
                Arg::Str("4"),
                Arg::Str("5")
            ]
        ),
        "a=1 b=2 c=3 d=4 e=5"
    );
}

/// `%hs` is not `%h` followed by `s`, and `%s` does not steal its argument: error 244
/// carries `%ls`, `%.*ls` and `%hs` in that order.
#[test]
fn format_substitutes_percent_hs_of_error_244() {
    assert_eq!(
        format_message(
            "The conversion of the %ls value '%.*ls' overflowed an %hs column. Use a larger integer column.",
            &[Arg::Str("varchar"), Arg::Str("300"), Arg::Str("INT1")]
        ),
        "The conversion of the varchar value '300' overflowed an INT1 column. Use a larger integer column."
    );
}

#[test]
fn format_handles_percent_d_and_percent_percent() {
    assert_eq!(
        format_message(
            "Process ID %d used 100%% of %ld / %I64d.",
            &[Arg::Int(57), Arg::Int(-3), Arg::Int(1 << 40)]
        ),
        "Process ID 57 used 100% of -3 / 1099511627776."
    );
}

#[test]
fn format_percent_percent_consumes_no_argument() {
    assert_eq!(format_message("%%%ls", &[Arg::Str("x")]), "%x");
}

#[test]
fn format_never_panics_on_missing_argument() {
    assert_eq!(
        format_message("Column '%.*ls.%.*ls' is invalid.", &[Arg::Str("t")]),
        "Column 't.%.*ls' is invalid."
    );
    assert_eq!(format_message("%ls %d %%", &[]), "%ls %d %");
}

#[test]
fn format_ignores_extra_arguments() {
    assert_eq!(
        format_message(
            "Divide by zero error encountered.",
            &[Arg::Str("unused"), Arg::Int(1)]
        ),
        "Divide by zero error encountered."
    );
}

#[test]
fn format_leaves_unknown_specifiers_and_trailing_percent_verbatim() {
    assert_eq!(format_message("%x %.5f %", &[Arg::Str("a")]), "%x %.5f %");
}

#[test]
fn format_prints_int_for_string_specifier_and_string_for_int_specifier() {
    assert_eq!(
        format_message("%ls-%d", &[Arg::Int(7), Arg::Str("seven")]),
        "7-seven"
    );
}

#[test]
fn format_preserves_non_ascii_text_around_specifiers() {
    assert_eq!(format_message("é%lsé", &[Arg::Str("ü")]), "éüé");
}

#[test]
fn from_catalog_uses_template_severity_and_given_state() {
    let err = from_catalog(208, 1, &[Arg::Str("dbo.t")]);
    assert_eq!(err.number, 208);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "Unknown object name 'dbo.t'.");
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);
}

/// A severity given by the caller replaces the catalogue's default, and nothing else
/// of the entry moves.
#[test]
fn from_catalog_with_severity_overrides_only_the_severity() {
    let published = from_catalog(
        2717,
        2,
        &[
            Arg::Int(5000),
            Arg::Str("parameter"),
            Arg::Str("@v"),
            Arg::Int(4000),
        ],
    );
    let sent = from_catalog_with_severity(
        2717,
        16,
        2,
        &[
            Arg::Int(5000),
            Arg::Str("parameter"),
            Arg::Str("@v"),
            Arg::Int(4000),
        ],
    );
    assert_eq!(published.severity, 15);
    assert_eq!(sent.severity, 16);
    assert_eq!(sent.number, published.number);
    assert_eq!(sent.state, published.state);
    assert_eq!(sent.message, published.message);
    assert_eq!(sent.line, 0);
    assert_eq!(sent.procedure, None);
}

/// An unknown number keeps the internal bug of [`from_catalog`], severity included:
/// the override applies to the catalogue's entry, not to the fallback.
#[test]
fn from_catalog_with_severity_unknown_number_is_internal_bug() {
    let err = from_catalog_with_severity(999_999, 15, 1, &[]);
    assert_eq!(err.number, 50000);
    assert_eq!(err.severity, 16);
}

/// `%f` renders as a C runtime does (see [`write_float`]).
#[test]
fn format_handles_percent_f() {
    assert_eq!(
        format_message("value = %f.", &[Arg::Float(300.0)]),
        "value = 300.000000."
    );
    assert_eq!(format_message("%f", &[]), "%f");
    assert_eq!(format_message("%f %f", &[Arg::Float(-0.5)]), "-0.500000 %f");
}

/// The value error 232 prints for `SELECT CAST(<literal> AS real);`.
#[test]
fn format_percent_f_matches_real_overflow_values() {
    for (value, expected) in [
        (1e40, "10000000000000000000000000000000000000000.000000"),
        (
            1.2345678901234567e40,
            "12345678901234566000000000000000000000000.000000",
        ),
        (-3.5e38, "-350000000000000000000000000000000000000.000000"),
        (1.5e39, "1500000000000000000000000000000000000000.000000"),
        (
            123456789.5e33,
            "123456789500000000000000000000000000000000.000000",
        ),
        (1e39, "999999999999999940000000000000000000000.000000"),
        (
            3.4028236e38,
            "340282359999999990000000000000000000000.000000",
        ),
    ] {
        assert_eq!(
            format_message("%f", &[Arg::Float(value)]),
            expected,
            "rendering of {value:e}"
        );
    }
}

#[test]
fn format_percent_f_does_not_panic_on_special_values() {
    assert_eq!(format_message("%f", &[Arg::Float(0.0)]), "0.000000");
    assert_eq!(format_message("%f", &[Arg::Float(f64::INFINITY)]), "inf");
    assert_eq!(format_message("%f", &[Arg::Float(f64::NAN)]), "NaN");
    let max = format_message("%f", &[Arg::Float(f64::MAX)]);
    assert!(max.starts_with("17976931348623157"));
    assert!(max.ends_with("0.000000"));
    assert_eq!(max.len(), 309 + ".000000".len());
}

#[test]
fn from_catalog_info_uses_template_severity_and_given_state() {
    let info = from_catalog_info(5701, 1, &[Arg::Str("db")]);
    assert_eq!(info.number, 5701);
    assert_eq!(info.severity, 10);
    assert_eq!(info.state, 1);
    assert_eq!(info.message, "Database context is now 'db'.");
    assert_eq!(info.line, 0);
}

#[test]
fn from_catalog_info_unknown_number_does_not_panic() {
    let info = from_catalog_info(999_999, 1, &[]);
    assert_eq!(info.number, 999_999);
    assert_eq!(info.severity, 10);
    assert!(info.message.contains("999999"));
}

#[test]
fn from_catalog_unknown_number_is_internal_bug() {
    let err = from_catalog(999_999, 1, &[]);
    assert_eq!(err.number, 50000);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "Internal error: internal bug: error 999999 is not in the catalog"
    );
}
