//! Unit tests of the logging setup: text and JSON layout, ANSI suppressed off a
//! terminal, and the peer scrubber that keeps long or sensitive values out of the
//! startup log line.

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
