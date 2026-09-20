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
mod tests;
