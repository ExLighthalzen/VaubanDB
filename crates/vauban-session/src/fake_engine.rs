//! What is left of the stand-in for the engine: the `WAITFOR DELAY` fallback.
//!
//! The parser reads `WAITFOR` (`parser`, `WaitforStatement`) but the binder does not bind
//! it, so `batch.rs` falls back here on the internal error the binder answers. The
//! ATTENTION handling needs a request long enough to be interrupted, which is what this
//! statement provides. `WAITFOR` proper (a real statement, with `TIME` and `TIMEOUT`) is
//! not implemented; this file goes away with it.

use std::thread;
use std::time::{Duration, Instant};

use vauban_errors::{InternalError, SqlError, SqlResult, message_template};

use crate::cancel::CancelHandle;
use crate::sink::ResultSink;

/// How long `WAITFOR DELAY` sleeps between two looks at the cancellation flag. The pool
/// thread cannot be killed ([MS-TDS] 2.2.1.7 is answered once the request stopped by
/// itself), so the sleep is cut into slices this long.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Answers one statement the binder does not know, or says that it cannot.
///
/// `Ok(Some(true))`: answered, the batch goes on. `Ok(Some(false))`: answered by an error,
/// the batch stops. `Ok(None)`: **not** a statement of this file, and `batch.rs` sends the
/// binder's internal error to the client instead. An `Err` is an internal error: a closed
/// sink, or the cancellation of the request by an ATTENTION.
///
/// `stmt` is the text the client wrote, cut out of the batch by the span of the statement
/// (`batch::statement_text`): error 148 prints the time string unchanged, so the text and
/// not the AST is what this file reads.
pub(crate) fn answer(
    cancel: &CancelHandle,
    stmt: &str,
    more: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<Option<bool>> {
    let stripped = strip_comments(stmt);
    match waitfor_delay(&stripped) {
        Some(delay) => waitfor(cancel, delay, more, sink).map(Some),
        None => Ok(None),
    }
}

/// `WAITFOR DELAY '<time>'`: sleeps, then a DONE with no
/// count. The sleep is cut into [`CANCEL_POLL_INTERVAL`] slices so that an ATTENTION stops
/// it; the cancellation is reported as an `Err` and no DONE is emitted, the connection task
/// sends the DONE `ATTN` itself.
///
/// A time string the parser rejects gives error 148 and stops the batch, as SQL Server
/// does.
fn waitfor(
    cancel: &CancelHandle,
    delay: &str,
    more: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<bool> {
    let Some(duration) = parse_delay(delay) else {
        sink.error(&incorrect_time_syntax(delay))?;
        sink.done(None, false)?;
        return Ok(false);
    };
    let deadline = Instant::now() + duration;
    loop {
        cancel.check()?;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(CANCEL_POLL_INTERVAL));
    }
    sink.done(None, more)?;
    Ok(true)
}

/// Recognises `WAITFOR DELAY '<time>'` and returns the time string **as the client wrote
/// it**: error 148 prints it unchanged. `None` for anything else, including the `WAITFOR`
/// forms this file does not serve (`TIME`, `TIMEOUT`, `RECEIVE`), which fall back to error 102.
fn waitfor_delay(stripped: &str) -> Option<&str> {
    let statement = stripped.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    let after_waitfor = strip_keyword(statement.trim_start(), "WAITFOR")?;
    let after_delay = strip_keyword(after_waitfor.trim_start(), "DELAY")?;
    let literal = after_delay.trim();
    let inner = literal.strip_prefix('\'')?.strip_suffix('\'')?;
    // A quote inside would be an escaped `''`: not a single literal, leave it to error 102.
    (!inner.contains('\'')).then_some(inner)
}

/// Removes `keyword` from the front of `text`, ignoring case; `None` when `text` does not
/// start with it or when the keyword runs into a longer identifier (`WAITFORDELAY`).
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let (head, rest) = text.split_at_checked(keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    match rest.chars().next() {
        // The characters a regular identifier may continue with.
        Some(c) if c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$') => None,
        _ => Some(rest),
    }
}

/// Reads a `WAITFOR` time string as `hh:mm:ss[.mmm]` and turns it into a duration.
///
/// `None` for anything else. Ranges are those of the `time` type SQL Server parses the
/// string with: hours `0..=23`, minutes and seconds `0..=59`, one to three fractional
/// digits. The rounding of SQL Server's `datetime` to 1/300 s is not reproduced: the
/// fallback needs a delay long enough to be interrupted, nothing finer.
fn parse_delay(delay: &str) -> Option<Duration> {
    let (hours, rest) = delay.split_once(':')?;
    let (minutes, seconds) = rest.split_once(':')?;
    let (seconds, millis) = match seconds.split_once('.') {
        Some((seconds, fraction)) => {
            if fraction.is_empty()
                || fraction.len() > 3
                || !fraction.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            // `.1` is 100 ms, `.12` is 120 ms, `.123` is 123 ms.
            let scale = 10u64.pow(3 - fraction.len() as u32);
            (seconds, parse_field(fraction, 999)? * scale)
        }
        None => (seconds, 0),
    };
    let seconds =
        parse_field(hours, 23)? * 3600 + parse_field(minutes, 59)? * 60 + parse_field(seconds, 59)?;
    Some(Duration::from_secs(seconds) + Duration::from_millis(millis))
}

/// One decimal field of a time string, `0..=max`, digits only (no sign, no space).
fn parse_field(text: &str, max: u64) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|value| *value <= max)
}

/// Error 148 from the catalogue, with the time string substituted for its single `%.*ls`.
/// Neither the text nor the severity is written here; state 1 and line 1 (this fallback
/// reads the statement text, not a parsed line).
///
/// The substitution is done here because `errors` exposes no named constructor for 148.
fn incorrect_time_syntax(delay: &str) -> SqlError {
    match message_template(148) {
        Some(def) => SqlError::new(
            def.number,
            def.severity,
            1,
            def.template.replacen("%.*ls", delay, 1),
        )
        .with_line(1),
        None => InternalError::Bug("error 148 missing from the catalogue".into()).into(),
    }
}

/// Removes `-- …` line comments and `/* … */` block comments (nested, as T-SQL allows),
/// each replaced by a space so that the words around them stay apart.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        if depth > 0 {
            match (c, chars.peek()) {
                ('*', Some('/')) => {
                    chars.next();
                    depth -= 1;
                    if depth == 0 {
                        out.push(' ');
                    }
                }
                ('/', Some('*')) => {
                    chars.next();
                    depth += 1;
                }
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('/', Some('*')) => {
                chars.next();
                depth = 1;
            }
            ('-', Some('-')) => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_out_of_range_or_out_of_shape_are_refused() {
        for delay in [
            "abc",
            "",
            "1",
            "00:00",
            "24:00:00",
            "00:60:00",
            "00:00:60",
            "00:00:00.",
            "00:00:00.1234",
            "00:00:00.abc",
            "-1:00:00",
            "0x:00:00",
            " 00:00:00",
            "00:00:00 ",
        ] {
            assert_eq!(parse_delay(delay), None, "delay {delay:?}");
        }
    }

    #[test]
    fn well_formed_delays_give_their_duration() {
        assert_eq!(parse_delay("00:00:00"), Some(Duration::ZERO));
        assert_eq!(parse_delay("00:00:10"), Some(Duration::from_secs(10)));
        assert_eq!(parse_delay("23:59:59"), Some(Duration::from_secs(86399)));
        assert_eq!(parse_delay("00:00:00.1"), Some(Duration::from_millis(100)));
        assert_eq!(parse_delay("00:00:00.12"), Some(Duration::from_millis(120)));
        assert_eq!(
            parse_delay("00:00:01.023"),
            Some(Duration::from_millis(1023))
        );
    }

    #[test]
    fn only_the_delay_form_of_waitfor_is_served() {
        assert_eq!(waitfor_delay("WAITFOR DELAY '00:00:01'"), Some("00:00:01"));
        assert_eq!(waitfor_delay("waitfor delay 'abc';"), Some("abc"));
        assert_eq!(waitfor_delay("WAITFOR TIME '22:00'"), None);
        assert_eq!(waitfor_delay("WAITFOR DELAY'00:00:01'"), Some("00:00:01"));
        assert_eq!(waitfor_delay("WAITFOR DELAY 00:00:01"), None);
        assert_eq!(waitfor_delay("WAITFORDELAY '00:00:01'"), None);
        assert_eq!(waitfor_delay("WAITFOR DELAYS '00:00:01'"), None);
        assert_eq!(waitfor_delay("SELECT 1"), None);
    }

    #[test]
    fn a_statement_this_file_does_not_know_is_none() {
        // The signal `batch.rs` reads to send the binder's internal error instead: the
        // fake engine answers nothing at all, it does not invent an error 102 any more.
        struct Silent;
        impl ResultSink for Silent {
            fn columns(&mut self, _cols: &[vauban_tds::ColumnMeta]) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn row(&mut self, _row: &[vauban_types::Value]) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn info(&mut self, _msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn error(&mut self, _err: &SqlError) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn env_change(&mut self, _change: &vauban_tds::EnvChange) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn return_value(
                &mut self,
                _name: &str,
                _ty: &vauban_types::TypeInfo,
                _value: &vauban_types::Value,
            ) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
            fn return_status(&mut self, _status: i32) -> SqlResult<()> {
                unreachable!("nothing is sent for a statement this file does not know")
            }
        }
        let cancel = CancelHandle::new();
        for text in ["SELECT 1", "SELECT @@VERSION", "WAITFOR TIME '22:00'"] {
            assert_eq!(
                answer(&cancel, text, false, &mut Silent).expect("no internal error"),
                None,
                "statement {text:?}"
            );
        }
    }

    #[test]
    fn strip_comments_keeps_words_apart() {
        assert_eq!(strip_comments("a--x\nb"), "a b");
        assert_eq!(strip_comments("a/*x*/b"), "a b");
        assert_eq!(strip_comments("a/* /* x */ */b"), "a b");
        assert_eq!(strip_comments("a -- unterminated"), "a  ");
        assert_eq!(strip_comments("a /* unterminated"), "a ");
    }
}
