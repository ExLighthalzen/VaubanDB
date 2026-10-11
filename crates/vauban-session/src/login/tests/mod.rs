use std::sync::Arc;

use vauban_tds::EncryptPolicy;

use super::*;
use crate::auth::NoAuth;

const DEFAULT_PACKET_SIZE: u16 = 4096;

fn cfg() -> ServerConfig {
    ServerConfig {
        encrypt: EncryptPolicy::Off,
        tls: None,
        authenticator: Arc::new(NoAuth),
        server_name: "test".into(),
        default_packet_size: DEFAULT_PACKET_SIZE,
        program_name: None,
        version_banner: None,
        edition: None,
    }
}

fn login7(tds_version: u32, packet_size: u32) -> Login7 {
    Login7 {
        username: "sa".into(),
        password: "Secret1!".into(),
        database: None,
        app_name: "app".into(),
        hostname: "host".into(),
        server_name: "server".into(),
        tds_version,
        packet_size,
        client_lcid: 0x0409,
        sspi: false,
        features: Vec::new(),
        language: None,
        client_interface_name: "ODBC".into(),
        client_pid: 1,
        read_only_intent: false,
        option_flags: [0; 4],
    }
}

fn state(packet_size: u16) -> SessionState {
    let mut state = SessionState::new(51);
    state.login = "sa".into();
    state.app_name = "app".into();
    state.hostname = "host".into();
    state.packet_size = packet_size;
    state
}

/// The response for a request of `packet_size` bytes, negotiated as `server.rs` does.
fn response(tds_version: u32, packet_size: u32) -> Vec<Token> {
    let login = login7(tds_version, packet_size);
    let cfg = cfg();
    let state = state(negotiate_packet_size(
        login.packet_size,
        cfg.default_packet_size,
    ));
    login_response(&login, &state, &cfg)
}

#[test]
fn response_has_the_eight_tokens_of_sql_server_2022_in_order() {
    let tokens = response(TDS_VERSION_7_4, 4096);
    assert_eq!(tokens.len(), 8);
    assert_eq!(
        tokens[0],
        Token::EnvChange(EnvChange::Database {
            old: "master".into(),
            new: "master".into(),
        })
    );
    assert_eq!(
        tokens[1],
        Token::Info(InfoMessage {
            number: 5701,
            severity: 0,
            state: 2,
            message: changed_database_context("master", 2, 1).message,
            line: 1,
            procedure: None,
        })
    );
    assert_eq!(
        tokens[2],
        Token::EnvChange(EnvChange::Collation {
            old: None,
            new: Collation::DEFAULT,
        })
    );
    assert_eq!(
        tokens[3],
        Token::EnvChange(EnvChange::Language {
            old: String::new(),
            new: "us_english".into(),
        })
    );
    assert_eq!(
        tokens[4],
        Token::Info(InfoMessage {
            number: 5703,
            severity: 0,
            state: 1,
            message: changed_language_setting("us_english").message,
            line: 1,
            procedure: None,
        })
    );
    assert_eq!(
        tokens[5],
        Token::EnvChange(EnvChange::PacketSize {
            old: 4096,
            new: 4096,
        })
    );
    assert_eq!(
        tokens[6],
        Token::LoginAck {
            tds_version: TDS_VERSION_7_4,
            program_name: PROGRAM_NAME.into(),
            version: [16, 0, 0x10, 0xB3],
        }
    );
    assert_eq!(
        tokens[7],
        Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        }
    );
}

/// A login that opens another database names it in the ENVCHANGE and in the INFO 5701,
/// and keeps `master` as the `old` value of the ENVCHANGE (rustdoc of
/// [`login_response`]).
#[test]
fn login_on_another_database_keeps_master_as_the_old_value() {
    let login = login7(TDS_VERSION_7_4, 4096);
    let mut state = state(4096);
    state.database = "Vauban_Mixed".into();
    let tokens = login_response(&login, &state, &cfg());
    assert_eq!(
        tokens[0],
        Token::EnvChange(EnvChange::Database {
            old: "master".into(),
            new: "Vauban_Mixed".into(),
        })
    );
    let Token::Info(info) = &tokens[1] else {
        panic!("token 1 is the INFO 5701: {:?}", tokens[1]);
    };
    assert_eq!((info.number, info.severity), (5701, 0));
    assert_eq!((info.state, info.line), (DATABASE_CONTEXT_STATE_LOGIN, 1));
    assert!(info.message.contains("'Vauban_Mixed'"), "{}", info.message);
}

/// The one template, under its two states: 2 for a login, 1 for a `USE` (rustdoc of
/// [`DATABASE_CONTEXT_STATE_LOGIN`] and [`DATABASE_CONTEXT_STATE_USE`]).
#[test]
fn changed_database_context_is_one_template_with_two_states() {
    assert_eq!(DATABASE_CONTEXT_STATE_LOGIN, 2);
    assert_eq!(DATABASE_CONTEXT_STATE_USE, 1);
    let at_login = changed_database_context("master", DATABASE_CONTEXT_STATE_LOGIN, 1);
    let at_use = changed_database_context("master", DATABASE_CONTEXT_STATE_USE, 4);
    assert_eq!(at_login.message, at_use.message);
    assert!(
        at_login.message.contains("'master'"),
        "{}",
        at_login.message
    );
    assert!(!at_login.message.contains("%.*ls"), "{}", at_login.message);
    assert_eq!((at_login.number, at_login.severity), (5701, 0));
    assert_eq!((at_login.state, at_login.line), (2, 1));
    assert_eq!((at_use.state, at_use.line), (1, 4));
}

#[test]
fn login_ack_version_follows_product_version() {
    assert_eq!(PRODUCT_VERSION, "16.0.4275.2");
    assert_eq!(LOGIN_ACK_VERSION, [16, 0, 0x10, 0xB3]);
    assert_eq!(parse_product_version("16.0.4135.4"), [16, 0, 0x10, 0x27]);
    assert_eq!(parse_product_version("15.0.2000.5"), [15, 0, 0x07, 0xD0]);
}

#[test]
fn version_banner_has_four_lines_and_names_the_product_version() {
    let lines: Vec<&str> = VERSION_BANNER.split('\n').collect();
    assert_eq!(lines.len(), 4);
    assert!(
        lines[0].ends_with("(X64) "),
        "first line keeps its trailing space"
    );
    assert!(lines[0].contains(PRODUCT_VERSION));
    assert!(lines[1].starts_with('\t') && lines[1].ends_with("00 "));
    assert!(lines[2].starts_with("\tCopyright (C) 2026 ExYgnizem"));
    assert!(lines[3].starts_with("\tVaubanDB (64-bit)"));
    assert!(VERSION_BANNER.starts_with("VaubanDB"));
    assert!(VERSION_BANNER.contains(EDITION));
    assert!(!VERSION_BANNER.contains("Microsoft SQL Server"));
    assert!(!VERSION_BANNER.contains("Microsoft Corporation"));
    assert_eq!(PROGRAM_NAME, "VaubanDB");
    assert_eq!(EDITION, "VaubanDB (64-bit)");
}

#[test]
fn banner_with_edition_rewrites_the_last_line_only() {
    let rewritten = banner_with_edition("Custom Edition");
    let lines: Vec<&str> = rewritten.split('\n').collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains(PRODUCT_VERSION));
    assert_eq!(lines[3], "\tCustom Edition");
}

#[test]
fn login_ack_uses_configured_program_name() {
    let mut cfg = cfg();
    cfg.program_name = Some("CustomDB".into());
    let login = login7(TDS_VERSION_7_4, 4096);
    let state = state(4096);
    let tokens = login_response(&login, &state, &cfg);
    assert_eq!(
        tokens[6],
        Token::LoginAck {
            tds_version: TDS_VERSION_7_4,
            program_name: "CustomDB".into(),
            version: LOGIN_ACK_VERSION,
        }
    );
}

/// A `program_name` LOGINACK cannot carry is cut, not refused: `cli` is the layer that
/// says no, and a `ServerConfig` built without it must still log a client in.
#[test]
fn login_ack_cuts_a_program_name_longer_than_255_units() {
    let mut cfg = cfg();
    cfg.program_name = Some("x".repeat(300));
    let login = login7(TDS_VERSION_7_4, 4096);
    let state = state(4096);
    let tokens = login_response(&login, &state, &cfg);
    let Token::LoginAck { program_name, .. } = &tokens[6] else {
        panic!("token 6 is the LOGINACK: {:?}", tokens[6]);
    };
    assert_eq!(program_name.encode_utf16().count(), 255);
    assert_eq!(program_name, &"x".repeat(255));
}

/// The cut stays on a character boundary. With one astral character (two UTF-16 code
/// units) straddling the 255th unit, the whole character goes: 254 units out, no unpaired
/// surrogate in them. A cut counted in units alone would have kept its high half.
#[test]
fn the_cut_of_astral_characters_lands_on_a_character_boundary() {
    // U+1F3F0 CASTLE, two UTF-16 code units, repeated so that a unit-by-unit cut at
    // 255 would land in the middle of the 128th one.
    let name = '\u{1F3F0}'.to_string().repeat(200);
    assert_eq!(name.encode_utf16().count(), 400);
    let cut = program_name_for_login_ack(&name);
    assert_eq!(cut.encode_utf16().count(), 254);
    assert_eq!(cut.chars().count(), 127);
    assert!(cut.chars().all(|ch| ch == '\u{1F3F0}'));
    assert!(
        String::from_utf16(&cut.encode_utf16().collect::<Vec<u16>>()).is_ok(),
        "the cut text decodes back from UTF-16"
    );
}

/// Exactly 255 units pass through untouched, and so does a shorter name.
#[test]
fn a_program_name_of_255_units_is_not_cut() {
    let name = "x".repeat(255);
    assert_eq!(program_name_for_login_ack(&name), name);
    assert_eq!(program_name_for_login_ack("CustomDB"), "CustomDB");
    assert_eq!(program_name_for_login_ack(PROGRAM_NAME), PROGRAM_NAME);

    // 255 units of two UTF-8 bytes each: the limit is not a byte count.
    let accented = "\u{e9}".repeat(255);
    assert_eq!(accented.len(), 510);
    assert_eq!(program_name_for_login_ack(&accented), accented);
}

#[test]
fn requested_packet_size_in_range_is_granted() {
    let tokens = response(TDS_VERSION_7_4, 8000);
    assert_eq!(
        tokens[5],
        Token::EnvChange(EnvChange::PacketSize {
            old: 4096,
            new: 8000,
        })
    );
}

#[test]
fn requested_packet_size_out_of_range_falls_back_to_the_default() {
    let tokens = response(TDS_VERSION_7_4, 100);
    assert_eq!(
        tokens[5],
        Token::EnvChange(EnvChange::PacketSize {
            old: 4096,
            new: DEFAULT_PACKET_SIZE,
        })
    );
    assert_eq!(negotiate_packet_size(511, 4096), 4096);
    assert_eq!(negotiate_packet_size(512, 4096), 512);
    assert_eq!(negotiate_packet_size(32767, 4096), 32767);
    assert_eq!(negotiate_packet_size(32768, 4096), 4096);
    assert_eq!(negotiate_packet_size(0, 4096), 4096);
}

#[test]
fn tds_version_is_echoed_up_to_7_4() {
    assert_eq!(negotiate_tds_version(0x7400_0004), Some(0x7400_0004));
    assert_eq!(negotiate_tds_version(0x7300_000B), Some(0x7300_000B));
    assert_eq!(negotiate_tds_version(0x7200_0002), Some(0x7200_0002));
    assert_eq!(negotiate_tds_version(0x7500_0000), Some(0x7400_0004));
    assert_eq!(negotiate_tds_version(0x7100_0000), None);
    assert_eq!(negotiate_tds_version(0x7000_0000), None);

    let ack = |version: u32| match &response(version, 4096)[6] {
        Token::LoginAck { tds_version, .. } => *tds_version,
        other => panic!("expected LOGINACK, got {other:?}"),
    };
    assert_eq!(ack(0x7400_0004), 0x7400_0004);
    assert_eq!(ack(0x7300_000B), 0x7300_000B);
}

#[test]
fn failure_response_ends_with_done_error() {
    let tokens = failure_response(vec![
        SqlError::cannot_open_database("nope"),
        SqlError::login_failed("sa"),
    ]);
    assert_eq!(tokens.len(), 3);
    assert!(matches!(&tokens[0], Token::Error(err) if err.number == 4060));
    assert!(matches!(&tokens[1], Token::Error(err) if err.number == 18456));
    assert_eq!(
        tokens[2],
        Token::Done {
            status: DoneStatus::ERROR,
            cur_cmd: 0,
            row_count: None,
        }
    );
}

#[test]
fn sspi_refusal_is_18452_severity_14_state_1() {
    let err = sspi_refused();
    assert_eq!((err.number, err.severity, err.state), (18452, 14, 1));
    assert!(!err.message.contains("%.*ls"), "{}", err.message);
    assert!(!err.message.is_empty());
}

#[test]
fn client_sees_state_1_for_18456_only() {
    let detailed = SqlError {
        state: 8,
        ..SqlError::login_failed("sa")
    };
    assert_eq!(for_client(detailed).state, 1);
    assert_eq!(for_client(SqlError::cannot_open_database("nope")).state, 1);
    let other = SqlError::new(50000, 16, 7, "custom");
    assert_eq!(for_client(other).state, 7);
}

#[test]
fn encrypt_option_follows_the_negotiated_mode() {
    let login = login7(0x7400_0004, 4096);
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    // Encrypted end to end.
    assert_eq!(connection_info(&login, peer, true).encrypt_option, "TRUE");
    // Login only encrypted, or clear text from the start: the connection is not
    // encrypted once the login is over.
    assert_eq!(connection_info(&login, peer, false).encrypt_option, "FALSE");
}
