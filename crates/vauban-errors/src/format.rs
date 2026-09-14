//! Argument substitution for the catalogue's message templates.
//!
//! This module is the one place where a catalogue template becomes a final text.
//! Named constructors (`constructors.rs`) call [`from_catalog`]; no other module of the
//! workspace formats a client-visible message by hand.
//!
//! Only the specifiers that appear in the catalogue are handled (`%.*ls`, `%ls`, `%s`,
//! `%hs`, `%d`, `%ld`, `%I64d`, `%f`, `%S_MSG`, `%%`). Anything else is copied verbatim:
//! this is not a `printf` implementation, and it never panics.

use crate::{InfoMessage, InternalError, SqlError, message_template};

/// One argument to substitute into a message template.
///
/// SQL Server passes `%.*ls` as a length plus a pointer; on the text side this is a single
/// substitution, so a single [`Arg::Str`] consumes it.
///
/// `Eq` is not derived: [`Arg::Float`] holds an `f64`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Arg<'a> {
    /// A string argument, for `%.*ls`, `%ls`, `%s`, `%hs` and `%S_MSG`.
    Str(&'a str),
    /// An integer argument, for `%d`, `%ld` and `%I64d`.
    Int(i64),
    /// A floating-point argument, for `%f` (errors 232 only, so far).
    ///
    /// Printed like a C runtime prints `%f`: six decimals, no exponent, and at most 17
    /// significant digits, the rest padded with zeros: `SELECT CAST(1e40 AS real);` prints
    /// `10000000000000000000000000000000000000000.000000` and
    /// `SELECT CAST(1e39 AS real);` prints `999999999999999940000000000000000000000.000000`
    /// (the exact `f64` is `...939709166371603178586112`, so the 18th significant digit
    /// onwards is zeroed after rounding, not truncated). Rust's `{:.6}` prints the exact
    /// decimal expansion instead, so [`write_float`] does the rounding itself.
    Float(f64),
}

impl Arg<'_> {
    /// Appends the textual form of the argument to `out`.
    fn write_to(self, out: &mut String) {
        match self {
            Arg::Str(s) => out.push_str(s),
            Arg::Int(i) => out.push_str(&i.to_string()),
            Arg::Float(f) => write_float(f, out),
        }
    }
}

/// The number of significant decimal digits SQL Server keeps when printing `%f`.
const FLOAT_SIGNIFICANT_DIGITS: usize = 17;

/// Appends `value` to `out` the way SQL Server prints a `%f` argument.
///
/// Below `1e17` the value has at most 17 integer digits and Rust's `{:.6}` agrees with
/// the C runtime (correct rounding to six decimals); that range is not reachable through
/// error 232, whose values exceed `3.4e38` (`real` overflow), so the branch follows C.
///
/// At or above `1e17` the value is rounded to [`FLOAT_SIGNIFICANT_DIGITS`] significant
/// digits (that is what `{:.16e}` does), zero-padded up to the decimal point, and given a
/// `.000000` fraction: `SELECT CAST(1.2345678901234567e40 AS real);` prints
/// `12345678901234566000000000000000000000000.000000`.
///
/// Does not panic: each step that could fail falls back to `{:.6}`
/// (`tests::format_percent_f_does_not_panic_on_special_values`). Non-finite values are
/// not produced by a catalogued caller and are printed as Rust prints them (`inf`,
/// `NaN`).
fn write_float(value: f64, out: &mut String) {
    if !value.is_finite() || value.abs() < 1e17 {
        out.push_str(&format!("{value:.6}"));
        return;
    }
    let scientific = format!("{:.*e}", FLOAT_SIGNIFICANT_DIGITS - 1, value);
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        out.push_str(&format!("{value:.6}"));
        return;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        out.push_str(&format!("{value:.6}"));
        return;
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let integer_digits = exponent + 1;
    if digits.len() != FLOAT_SIGNIFICANT_DIGITS || integer_digits < FLOAT_SIGNIFICANT_DIGITS as i32
    {
        out.push_str(&format!("{value:.6}"));
        return;
    }
    if value.is_sign_negative() {
        out.push('-');
    }
    out.push_str(&digits);
    for _ in 0..(integer_digits - FLOAT_SIGNIFICANT_DIGITS as i32) {
        out.push('0');
    }
    out.push_str(".000000");
}

/// The specifiers that consume exactly one argument. None of them is a prefix of another,
/// so the match order does not matter.
///
/// `%hs` is the "narrow string" twin of `%ls`; error 244 is the only catalogue entry that
/// uses it (`SELECT CAST('300' AS tinyint);` prints `INT1` there), and it consumes one
/// [`Arg::Str`] like the others.
const ARGUMENT_SPECIFIERS: &[&str] = &[
    "%.*ls", "%ls", "%s", "%hs", "%d", "%ld", "%I64d", "%f", "%S_MSG",
];

/// Substitutes `args`, in order, into the specifiers of `template`.
///
/// Rules:
/// - `%%` produces a literal `%` and consumes no argument;
/// - each specifier of [`ARGUMENT_SPECIFIERS`] consumes the next argument, whatever its
///   variant (an [`Arg::Int`] given to `%ls` is printed in decimal, an [`Arg::Str`] given to
///   `%d` is printed as-is);
/// - an argument in excess is ignored;
/// - a missing argument leaves the specifier untouched in the output;
/// - a `%` followed by anything else is copied verbatim.
///
/// Never panics.
pub(crate) fn format_message(template: &str, args: &[Arg<'_>]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut args = args.iter();
    let mut rest = template;
    while let Some(pos) = rest.find('%') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix("%%") {
            out.push('%');
            rest = after;
        } else if let Some(spec) = ARGUMENT_SPECIFIERS
            .iter()
            .find(|spec| tail.starts_with(**spec))
        {
            match args.next() {
                Some(arg) => arg.write_to(&mut out),
                None => out.push_str(spec),
            }
            rest = &tail[spec.len()..];
        } else {
            // `%` is a single ASCII byte: slicing after it stays on a char boundary.
            out.push('%');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Builds a [`SqlError`] from the catalogue entry `number`, with `state` and `args`
/// substituted into the template.
///
/// `severity` is the catalogue's default severity; `line` is `0` and `procedure` is
/// `None` (use [`SqlError::with_line`] and [`SqlError::with_procedure`]).
///
/// A `number` absent from the catalogue is a programming error, not a client error: the
/// result is `SqlError::from(InternalError::Bug(..))` (number 50000), never a panic.
pub(crate) fn from_catalog(number: u32, state: u8, args: &[Arg<'_>]) -> SqlError {
    match message_template(number) {
        Some(def) => SqlError::new(
            def.number,
            def.severity,
            state,
            format_message(def.template, args),
        ),
        None => SqlError::from(InternalError::Bug(format!(
            "error {number} is not in the catalog"
        ))),
    }
}

/// Builds a [`SqlError`] from the catalogue entry `number` like [`from_catalog`], but with
/// `severity` instead of the catalogue's default.
///
/// The catalogue holds a default severity, and for a few numbers the server sends
/// another one: 2717 is catalogued 15 and sent 16, 192, 1001 and 1002 are catalogued 16
/// and sent 15, and 131 is sent 16 for its `convert specification` filling and 15 for the
/// other two. The sent value wins there, and there alone: the catalogue keeps its
/// default.
///
/// A `number` absent from the catalogue keeps [`from_catalog`]'s answer untouched, the
/// internal bug 50000 with its own severity.
pub(crate) fn from_catalog_with_severity(
    number: u32,
    severity: u8,
    state: u8,
    args: &[Arg<'_>],
) -> SqlError {
    let mut error = from_catalog(number, state, args);
    if error.number == number {
        error.severity = severity;
    }
    error
}

/// Builds an [`InfoMessage`] from the catalogue entry `number`, with `state` and `args`
/// substituted into the template.
///
/// The informational twin of [`from_catalog`]: same substitution, same "the catalogue owns
/// the text" rule, but the result carries no failure. `severity` is the catalogue's default
/// severity, which the caller must have checked to be `<= 10`; `line` is `0`.
///
/// A `number` absent from the catalogue is a programming error: the message falls back to
/// the number itself, severity 10, so nothing panics on a client's path.
pub(crate) fn from_catalog_info(number: u32, state: u8, args: &[Arg<'_>]) -> InfoMessage {
    match message_template(number) {
        Some(def) => InfoMessage {
            number: def.number,
            severity: def.severity,
            state,
            message: format_message(def.template, args),
            line: 0,
        },
        None => InfoMessage {
            number,
            severity: 10,
            state,
            message: format!("Internal error: message {number} is not in the catalog"),
            line: 0,
        },
    }
}

#[cfg(test)]
mod tests {
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
}
