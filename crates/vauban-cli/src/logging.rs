//! Human-readable log lines for `vauban serve`.
//!
//! Output goes through [`anstream::AutoStream`] so Windows consoles get virtual-terminal
//! processing when they support it, and escape sequences are stripped otherwise. Timestamps
//! use a local UTC offset fixed once at startup ([`install_local_offset`]).

use std::fmt;
use std::io::{self, IsTerminal};
use std::ops::Deref;
use std::sync::OnceLock;

use anstream::AutoStream;
use anstream::ColorChoice;
use time::{OffsetDateTime, UtcOffset};
use tracing::Level;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::fmt::format::{FormatFields, Writer};
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

use crate::config::{Config, LogFormat};

/// Maximum length of a field value kept on the headline; longer values move below.
const LONG_VALUE_THRESHOLD: usize = 48;

/// Left margin for continuation lines (aligned under the message column).
const CONTINUATION_INDENT: &str = "           ";

/// Fields that belong on an indented line rather than the headline.
const INDENTED_FIELD_NAMES: &[&str] = &["app_name", "hostname", "batch", "fingerprint"];

/// Local UTC offset, fixed once before the tokio runtime starts.
static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Detects and stores the local UTC offset. Call once from the main thread before
/// [`crate::init_tracing`] and before any async runtime.
pub(crate) fn install_local_offset() {
    let _ = LOCAL_OFFSET.set(detect_local_offset());
}

fn detect_local_offset() -> UtcOffset {
    UtcOffset::local_offset_at(OffsetDateTime::now_utc()).unwrap_or(UtcOffset::UTC)
}

fn local_offset() -> UtcOffset {
    *LOCAL_OFFSET.get().unwrap_or(&UtcOffset::UTC)
}

/// Whether ANSI styling may be emitted for the text log formats.
pub(crate) fn ansi_enabled(cfg: &Config) -> bool {
    match cfg.log_format {
        LogFormat::Plain | LogFormat::Json => false,
        LogFormat::Text => std::io::stdout().is_terminal(),
    }
}

/// `ColorChoice` passed to [`AnstreamMakeWriter`].
pub(crate) fn color_choice(cfg: &Config) -> ColorChoice {
    match cfg.log_format {
        LogFormat::Plain => ColorChoice::Never,
        LogFormat::Text if std::io::stdout().is_terminal() => ColorChoice::Auto,
        LogFormat::Text => ColorChoice::Never,
        LogFormat::Json => ColorChoice::Never,
    }
}

/// Unicode level markers when the terminal is expected to render them; ASCII otherwise.
pub(crate) fn unicode_markers_enabled() -> bool {
    #[cfg(unix)]
    {
        true
    }
    #[cfg(windows)]
    {
        std::env::var_os("WT_SESSION").is_some()
    }
}

/// Routes log output through [`AutoStream`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct AnstreamMakeWriter {
    /// Colour policy for the underlying stream adapter.
    pub choice: ColorChoice,
}

impl<'a> MakeWriter<'a> for AnstreamMakeWriter {
    type Writer = Box<dyn io::Write + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Box::new(AutoStream::new(std::io::stdout().lock(), self.choice))
    }
}

/// Event formatter installed for `--log-format text` and `--log-format plain`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HumanEventFormat {
    /// When false, level words and timestamps are plain text.
    pub ansi: bool,
    /// When true, level markers use Unicode glyphs instead of ASCII fallbacks.
    pub unicode_markers: bool,
}

impl HumanEventFormat {
    fn write_timestamp(&self, w: &mut Writer<'_>) -> fmt::Result {
        let local = OffsetDateTime::now_utc().to_offset(local_offset());
        write!(
            w,
            "{:02}:{:02}:{:02}",
            local.hour(),
            local.minute(),
            local.second()
        )
    }

    fn write_level(&self, w: &mut Writer<'_>, level: Level) -> fmt::Result {
        let (marker, label, colour) = level_style(level, self.unicode_markers);
        if self.ansi {
            write!(w, "{colour}{marker}{label}{RESET}")
        } else {
            write!(w, "{marker}{label}")
        }
    }

    fn write_target(&self, w: &mut Writer<'_>, target: &str, level: Level) -> fmt::Result {
        match level {
            Level::TRACE | Level::DEBUG => {
                if self.ansi {
                    write!(w, " {DIM}({target}){RESET}")?;
                } else {
                    write!(w, " ({target})")?;
                }
            }
            Level::INFO | Level::WARN | Level::ERROR => {}
        }
        Ok(())
    }
}

impl<S, N> FormatEvent<S, N> for HumanEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let level = *meta.level();

        self.write_timestamp(&mut writer)?;
        writer.write_str(" ")?;
        self.write_level(&mut writer, level)?;
        writer.write_str("  ")?;

        if let Some(context) = connection_context(ctx) {
            write!(writer, "{context}  ")?;
        }

        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let message = visitor.message.as_deref().unwrap_or_default();
        writer.write_str(message)?;

        let (short, long) = partition_fields(visitor.fields);
        if !short.is_empty() {
            writer.write_str("  ")?;
            write_aligned_fields(&mut writer, &short)?;
        }

        self.write_target(&mut writer, meta.target(), level)?;

        for (name, value) in long {
            writer.write_str("\n")?;
            writer.write_str(CONTINUATION_INDENT)?;
            write!(writer, "{name}=")?;
            write_field_value(&mut writer, &value)?;
        }

        writer.write_str("\n")
    }
}

/// Field formatter used for span field caching; values stay unstyled.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HumanFields;

impl HumanFields {
    /// Constructs the field formatter.
    pub(crate) fn new() -> Self {
        Self
    }
}

impl<'writer> FormatFields<'writer> for HumanFields {
    fn format_fields<R>(&self, mut writer: Writer<'writer>, fields: R) -> fmt::Result
    where
        R: tracing_subscriber::field::RecordFields,
    {
        let mut visitor = EventVisitor::default();
        fields.record(&mut visitor);
        let (short, long) = partition_fields(visitor.fields);
        write_aligned_fields(&mut writer, &short)?;
        for (name, value) in long {
            writer.write_str("\n")?;
            writer.write_str(CONTINUATION_INDENT)?;
            write!(writer, "{name}=")?;
            write_field_value(&mut writer, &value)?;
        }
        Ok(())
    }
}

fn level_style(level: Level, unicode: bool) -> (&'static str, &'static str, &'static str) {
    if unicode {
        match level {
            Level::ERROR => ("✗ ", "ERROR", RED),
            Level::WARN => ("⚠ ", "WARN", YELLOW),
            Level::INFO => ("● ", "INFO", GREEN),
            Level::DEBUG => ("· ", "DEBUG", CYAN),
            Level::TRACE => ("  ", "TRACE", DIM),
        }
    } else {
        match level {
            Level::ERROR => ("!! ", "ERROR", RED),
            Level::WARN => ("! ", "WARN", YELLOW),
            Level::INFO => ("- ", "INFO", GREEN),
            Level::DEBUG => (". ", "DEBUG", CYAN),
            Level::TRACE => ("  ", "TRACE", DIM),
        }
    }
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        record_field(self, field, &format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        record_field(self, field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        record_field(self, field, if value { "true" } else { "false" });
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        record_field(self, field, &value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        record_field(self, field, &value.to_string());
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        record_field(self, field, &value.to_string());
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        record_field(self, field, &value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        record_field(self, field, &value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        record_field(self, field, &value.to_string());
    }
}

fn record_field(visitor: &mut EventVisitor, field: &Field, raw: &str) {
    let value = strip_quotes(raw);
    if field.name() == "message" {
        visitor.message = Some(value);
        return;
    }
    visitor.fields.push((field.name().to_owned(), value));
}

fn strip_quotes(raw: &str) -> String {
    if raw.len() >= 2 {
        let bytes = raw.as_bytes();
        if (bytes[0] == b'"' && bytes[raw.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[raw.len() - 1] == b'\'')
        {
            return raw[1..raw.len() - 1].to_owned();
        }
    }
    raw.to_owned()
}

type FieldList = Vec<(String, String)>;

fn partition_fields(fields: FieldList) -> (FieldList, FieldList) {
    let mut short = Vec::new();
    let mut long = Vec::new();
    for (name, value) in fields {
        if should_indent_field(&name, &value) {
            long.push((name, value));
        } else {
            short.push((name, value));
        }
    }
    (short, long)
}

fn should_indent_field(name: &str, value: &str) -> bool {
    INDENTED_FIELD_NAMES.contains(&name)
        || value.contains('\n')
        || value.len() > LONG_VALUE_THRESHOLD
}

fn write_aligned_fields(writer: &mut Writer<'_>, fields: &[(String, String)]) -> fmt::Result {
    if fields.is_empty() {
        return Ok(());
    }
    let width = fields.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            writer.write_str("  ")?;
        }
        write!(writer, "{name:>width$}=")?;
        write_inline_value(writer, value)?;
    }
    Ok(())
}

fn write_inline_value(writer: &mut Writer<'_>, value: &str) -> fmt::Result {
    if value.contains(' ') {
        write!(writer, "\"{}\"", escape_quotes(value))
    } else {
        writer.write_str(value)
    }
}

fn write_field_value(writer: &mut Writer<'_>, value: &str) -> fmt::Result {
    if value.contains('\n') {
        for (index, line) in value.lines().enumerate() {
            if index > 0 {
                writer.write_str("\n")?;
                writer.write_str(CONTINUATION_INDENT)?;
                writer.write_str("  ")?;
            }
            writer.write_str(line)?;
        }
        Ok(())
    } else if value.contains(' ') {
        write!(writer, "\"{}\"", escape_quotes(value))
    } else {
        writer.write_str(value)
    }
}

fn escape_quotes(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn connection_context<S, N>(ctx: &FmtContext<'_, S, N>) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    let scope = ctx.event_scope()?;
    for span in scope.from_root() {
        if span.metadata().name() != "connection" {
            continue;
        }
        let mut spid = None;
        let mut peer = None;
        let extensions = span.extensions();
        if let Some(fields) = extensions.get::<FormattedFields<N>>() {
            parse_connection_fields(fields.deref(), &mut spid, &mut peer);
        }
        let mut parts = Vec::new();
        if let Some(spid) = spid {
            parts.push(format!("spid {spid}"));
        }
        if let Some(peer) = peer {
            parts.push(format!("peer {peer}"));
        }
        if parts.is_empty() {
            return None;
        }
        return Some(parts.join(" "));
    }
    None
}

fn parse_connection_fields(raw: &str, spid: &mut Option<i16>, peer: &mut Option<String>) {
    for token in raw.split_whitespace() {
        if let Some(value) = token.strip_prefix("spid=") {
            if let Ok(parsed) = value.parse::<i16>() {
                *spid = Some(parsed);
            }
        } else if let Some(value) = token.strip_prefix("peer=") {
            *peer = Some(value.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use tracing::{info, info_span};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rendered(event: impl FnOnce(), ansi: bool, unicode: bool) -> String {
        install_local_offset();
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || LogBuffer(Arc::clone(&buffer))
        };
        let _guard = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(ansi)
                    .with_target(false)
                    .without_time()
                    .event_format(HumanEventFormat {
                        ansi,
                        unicode_markers: unicode,
                    })
                    .fmt_fields(HumanFields::new())
                    .with_writer(make_writer),
            )
            .set_default();

        event();
        String::from_utf8(buffer.lock().unwrap().clone()).expect("utf-8 log line")
    }

    #[test]
    fn plain_format_is_stable_without_ansi() {
        let line = rendered(|| info!(port = 56789_u16, "vauban listening"), false, false);
        assert!(
            !line.contains('\x1b'),
            "plain output must not contain escapes: {line:?}"
        );
        assert!(line.contains("vauban listening"), "{line}");
        assert!(line.contains("port=56789"), "{line}");
        assert!(line.contains("- INFO"), "{line}");
    }

    #[test]
    fn spaced_inline_values_are_quoted() {
        let output = rendered(|| info!(label = "has space", "startup"), false, false);
        assert!(output.contains("label=\"has space\""), "{output}");
    }

    #[test]
    fn long_value_moves_to_an_indented_line() {
        let long = "x".repeat(LONG_VALUE_THRESHOLD + 1);
        let output = rendered(
            || info!(batch = long.as_str(), "batch received"),
            false,
            false,
        );
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 2, "{output}");
        assert!(lines[0].contains("batch received"), "{output}");
        assert!(lines[1].starts_with(CONTINUATION_INDENT), "{output}");
        assert!(lines[1].contains("batch="), "{output}");
    }

    #[test]
    fn multiline_value_is_indented() {
        let output = rendered(
            || info!(batch = "SELECT 1\nSELECT 2", "batch received"),
            false,
            false,
        );
        assert!(output.contains("SELECT 1"), "{output}");
        assert!(output.contains("SELECT 2"), "{output}");
        assert!(output.matches('\n').count() >= 2, "{output}");
    }

    #[test]
    fn connection_context_puts_client_identity_on_the_next_line() {
        let output = rendered(
            || {
                let span = info_span!("connection", spid = 51_i16, peer = "127.0.0.1:65135");
                let _enter = span.enter();
                info!(
                    login = "sa",
                    app_name = "Framework Microsoft SqlClient Data Provider",
                    hostname = "DESKTOP-4LFH3L9",
                    tds_version = "0x74000004",
                    packet_size = 4096_u16,
                    "login succeeded"
                );
            },
            false,
            false,
        );
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[0].contains("spid 51"), "{output}");
        assert!(lines[0].contains("login succeeded"), "{output}");
        assert!(lines[0].contains("login=sa"), "{output}");
        assert!(
            !lines[0].contains("Framework Microsoft"),
            "client name must not be on the headline: {output}"
        );
        assert!(
            lines.iter().any(|line| line.contains("app_name=")),
            "{output}"
        );
        assert!(
            output.contains("app_name=\"Framework Microsoft SqlClient Data Provider\""),
            "{output}"
        );
        assert!(!output.contains('{'), "{output}");
    }

    #[test]
    fn ansi_disabled_produces_no_escape_sequences() {
        let line = rendered(|| info!(port = 1_u16, "vauban listening"), false, false);
        assert!(!line.contains('\x1b'), "{line:?}");
    }

    #[test]
    fn ascii_level_markers_when_unicode_disabled() {
        let line = rendered(|| tracing::warn!("check"), false, false);
        assert!(line.contains("! WARN"), "{line}");
        assert!(line.is_ascii(), "{line:?}");
    }

    #[test]
    fn unicode_level_markers_when_enabled() {
        let line = rendered(|| tracing::warn!("check"), false, true);
        assert!(line.contains('⚠'), "{line}");
    }

    #[test]
    fn plain_and_json_disable_ansi() {
        use crate::config::{Config, LogFormat, ServeArgs};

        let mut cfg = Config::assemble(&ServeArgs::default(), None, None);
        cfg.log_format = LogFormat::Plain;
        assert!(!super::ansi_enabled(&cfg));
        cfg.log_format = LogFormat::Json;
        assert!(!super::ansi_enabled(&cfg));
    }

    #[test]
    fn local_offset_install_succeeds() {
        install_local_offset();
        let offset = local_offset();
        let _ = offset.whole_hours();
    }

    #[test]
    fn timestamp_matches_local_offset_clock() {
        install_local_offset();
        let line = rendered(|| info!("tick"), false, false);
        let offset = local_offset();
        let local = OffsetDateTime::now_utc().to_offset(offset);
        let expected = format!(
            "{:02}:{:02}:{:02}",
            local.hour(),
            local.minute(),
            local.second()
        );
        assert!(
            line.starts_with(&expected),
            "expected local prefix {expected}, got {line:?}"
        );
    }

    #[test]
    fn partition_fields_splits_on_length_and_newlines() {
        let (short, long) = partition_fields(vec![
            ("a".into(), "1".into()),
            ("batch".into(), "line1\nline2".into()),
            ("cert".into(), "z".repeat(LONG_VALUE_THRESHOLD + 5)),
            (
                "app_name".into(),
                "Framework Microsoft SqlClient Data Provider".into(),
            ),
        ]);
        assert_eq!(short.len(), 1);
        assert_eq!(long.len(), 3);
    }
}
