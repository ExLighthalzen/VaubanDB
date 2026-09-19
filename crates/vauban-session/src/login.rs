//! Login sequence: LOGIN7 handling, negotiation of the TDS version and packet size, the
//! LOGINACK/ENVCHANGE/INFO/DONE response, the refusal sequences and the product version
//! constants. Everything here is a pure function; `server.rs` drives the
//! socket ([MS-TDS] 3.3.5.2).

use std::net::SocketAddr;

use vauban_errors::{InfoMessage, SqlError, SqlResult, message_template};
use vauban_tds::{DoneStatus, EncryptPolicy, EnvChange, Login7, Token};
use vauban_types::Collation;

use crate::registry::ConnectionInfo;
use crate::server::{Engine, ServerConfig};
use crate::state::SessionState;

/// Product version announced in LOGINACK, `@@VERSION` and `SERVERPROPERTY('ProductVersion')`:
/// the product version drivers expect.
///
/// Defined here (and not in `compat`, which depends on `session` and re-exports it), so
/// that the crate has a single definition of it.
pub const PRODUCT_VERSION: &str = "16.0.4275.2";

/// Default text of `SELECT @@VERSION`. Four lines, the first ending with a space, the
/// last three starting with a tab. Product name, copyright and edition are VaubanDB:
/// an alternative, not a Microsoft product.
/// [`PRODUCT_VERSION`] stays the numeric surface the drivers read.
pub const VERSION_BANNER: &str = "VaubanDB (SQL Server compatible) - 16.0.4275.2 (X64) \n\
    \tSep 12 2026 00:00:00 \n\
    \tCopyright (C) 2026 ExYgnizem\n\
    \tVaubanDB (64-bit)";

/// `TDSVersion` of TDS 7.4 ([MS-TDS] 2.2.6.4), the highest the server speaks.
pub(crate) const TDS_VERSION_7_4: u32 = 0x7400_0004;
/// `TDSVersion` of TDS 7.2, the oldest the server accepts: the LOGIN7 layout decoded by
/// `tds` (`cbSSPILong`, 8-byte DONE row counts) starts there.
pub(crate) const TDS_VERSION_MIN: u32 = 0x7200_0002;
/// `ProgName` of LOGINACK ([MS-TDS] 2.2.7.14). Default product name.
pub const PROGRAM_NAME: &str = "VaubanDB";
/// Longest `ProgName` LOGINACK can carry: it is a B_VARCHAR ([MS-TDS] 2.2.5.1), one length
/// byte then that many UTF-16 code units.
const PROGRAM_NAME_MAX_UNITS: usize = 255;
/// `SERVERPROPERTY('Edition')` and the last line of [`VERSION_BANNER`].
pub const EDITION: &str = "VaubanDB (64-bit)";

/// `name` cut to at most [`PROGRAM_NAME_MAX_UNITS`] UTF-16 code units, on a character
/// boundary: a cut between two `char`s cannot leave half of a surrogate pair, which a client
/// decoding UTF-16 could not read.
///
/// A defensive guard, not the rule an operator meets: the `cli` crate refuses a longer
/// `program_name` at startup and names the limit. It exists for a
/// [`ServerConfig`] built without going through `cli`: without it, `put_b_varchar` fails on
/// a longer name, the whole LOGINACK cannot be encoded, and the server closes the connection
/// without sending an error token.
fn program_name_for_login_ack(name: &str) -> &str {
    if name.encode_utf16().count() <= PROGRAM_NAME_MAX_UNITS {
        return name;
    }
    let mut units = 0;
    for (offset, ch) in name.char_indices() {
        let next = units + ch.len_utf16();
        if next > PROGRAM_NAME_MAX_UNITS {
            return &name[..offset];
        }
        units = next;
    }
    name
}

/// Rewrites the last line of [`VERSION_BANNER`] so that `@@VERSION` and
/// `SERVERPROPERTY('Edition')` stay in agreement when only the edition is overridden.
pub fn banner_with_edition(edition: &str) -> String {
    let mut lines: Vec<String> = VERSION_BANNER.split('\n').map(str::to_owned).collect();
    if let Some(last) = lines.last_mut() {
        *last = format!("\t{edition}");
    }
    lines.join("\n")
}

/// Database a login opens when the LOGIN7 names no database, and `old` value of the
/// ENVCHANGE the login response carries whichever database it opens (see
/// [`login_response`]).
pub(crate) const MASTER: &str = "master";

/// Builds the connection metadata written into `vauban_sys_connections` at login.
pub(crate) fn connection_info(
    login: &Login7,
    peer: SocketAddr,
    encrypt: EncryptPolicy,
) -> ConnectionInfo {
    ConnectionInfo {
        client_address: peer.ip().to_string(),
        client_port: peer.port(),
        tds_version: login.tds_version,
        encrypt_option: match encrypt {
            EncryptPolicy::Required => "TRUE".to_owned(),
            EncryptPolicy::Off | EncryptPolicy::Optional => "FALSE".to_owned(),
        },
    }
}

/// Inserts the session row after a successful login.
pub(crate) fn register_live_session(
    engine: &Engine,
    state: &SessionState,
    connection: &ConnectionInfo,
) -> SqlResult<()> {
    engine.register_session(state, connection)
}
/// `State` of the INFO 5701 of a **login**: 2, whether the LOGIN7 names a database or
/// not. A `USE` sends [`DATABASE_CONTEXT_STATE_USE`] instead, which tells the two senders
/// apart.
pub(crate) const DATABASE_CONTEXT_STATE_LOGIN: u8 = 2;
/// `State` of the INFO 5701 a `USE` statement sends: **1** where a login sends 2, after
/// the ENVCHANGE of the database (unit test
/// `use_existing_database_changes_state_and_sends_5701` of `batch.rs`).
pub(crate) const DATABASE_CONTEXT_STATE_USE: u8 = 1;

/// The INFO 5701 of a login or of a `USE`, with the text of the catalogue.
///
/// One template for the two senders: [`login_response`] calls it with
/// [`DATABASE_CONTEXT_STATE_LOGIN`] and line 1, `batch.rs` with
/// [`DATABASE_CONTEXT_STATE_USE`] and the line the `USE` statement starts on. The
/// severity is 0 for both.
pub(crate) fn changed_database_context(database: &str, state: u8, line: u32) -> InfoMessage {
    catalogue_info(5701, database, state, line)
}

/// The INFO 5703 of a login: the language setting is `language`, severity 0, state 1,
/// line 1, with the text of the catalogue.
fn changed_language_setting(language: &str) -> InfoMessage {
    catalogue_info(5703, language, 1, 1)
}

/// An informational message built from the catalogue entry `number`, whose single
/// `%.*ls` placeholder receives `arg`. The severity is 0, the one the login response and
/// `USE` send, regardless of the severity the catalogue declares for the number.
///
/// A `number` absent from the catalogue is a programming error: the message falls back
/// to the number itself, so nothing panics on a client's path.
fn catalogue_info(number: u32, arg: &str, state: u8, line: u32) -> InfoMessage {
    let message = match message_template(number) {
        Some(def) => def.template.replacen("%.*ls", arg, 1),
        None => number.to_string(),
    };
    InfoMessage {
        number,
        severity: 0,
        state,
        message,
        line,
    }
}
/// Default language of a fresh instance.
const LANGUAGE: &str = "us_english";
/// Smallest `PacketSize` a client may negotiate ([MS-TDS] 2.2.6.4 PacketSize).
const PACKET_SIZE_MIN: u32 = 512;
/// Largest `PacketSize` a client may negotiate ([MS-TDS] 2.2.6.4 PacketSize).
const PACKET_SIZE_MAX: u32 = 32767;
/// State of error 18456 sent to the client: 1 regardless of the cause. The detailed state
/// stays in the server log.
const STATE_18456_CLIENT: u8 = 1;
/// `MajorVer`, `MinorVer`, `BuildNumHi`, `BuildNumLow` of LOGINACK ([MS-TDS] 2.2.7.14),
/// derived from [`PRODUCT_VERSION`] at compile time.
pub(crate) const LOGIN_ACK_VERSION: [u8; 4] = parse_product_version(PRODUCT_VERSION);

/// Splits `major.minor.build.revision` into the four LOGINACK bytes: `major`, `minor`,
/// then the 16-bit `build` most significant byte first. The revision is not announced.
/// A malformed string fails the build (`const` evaluation), never the login.
const fn parse_product_version(version: &str) -> [u8; 4] {
    let bytes = version.as_bytes();
    let mut parts = [0u32; 4];
    let mut part = 0;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'.' {
            part += 1;
            assert!(part < 4, "PRODUCT_VERSION has more than four parts");
        } else {
            assert!(
                byte.is_ascii_digit(),
                "PRODUCT_VERSION is not digits and dots"
            );
            parts[part] = parts[part] * 10 + (byte - b'0') as u32;
        }
        i += 1;
    }
    assert!(part == 3, "PRODUCT_VERSION has fewer than four parts");
    assert!(
        parts[0] <= 0xFF && parts[1] <= 0xFF,
        "major or minor exceeds a byte"
    );
    assert!(parts[2] <= 0xFFFF, "build exceeds 16 bits");
    let [.., build_hi, build_lo] = parts[2].to_be_bytes();
    [parts[0] as u8, parts[1] as u8, build_hi, build_lo]
}

/// TDS version the server announces for the one the client requests: `None` below TDS
/// 7.2 (the caller closes without a response), otherwise the requested version capped at
/// 7.4 ([MS-TDS] 2.2.6.4 TDSVersion, 2.2.7.14).
pub(crate) fn negotiate_tds_version(requested: u32) -> Option<u32> {
    (requested >= TDS_VERSION_MIN).then(|| requested.min(TDS_VERSION_7_4))
}

/// Packet size granted for the one the client requests: the request when it lies in
/// `512..=32767`, `default` otherwise ([MS-TDS] 2.2.6.4 PacketSize).
pub(crate) fn negotiate_packet_size(requested: u32, default: u16) -> u16 {
    if (PACKET_SIZE_MIN..=PACKET_SIZE_MAX).contains(&requested) {
        // Bounded above by 32767: the conversion cannot fail.
        u16::try_from(requested).unwrap_or(default)
    } else {
        default
    }
}

/// The response to an accepted LOGIN7, in the order SQL Server sends it:
///
/// 1. ENVCHANGE database ([`MASTER`] -> `state.database`),
/// 2. INFO 5701, state 2, line 1 ([`changed_database_context`]),
/// 3. ENVCHANGE collation (`Collation::DEFAULT`, no old value),
/// 4. ENVCHANGE language (`""` -> `us_english`),
/// 5. INFO 5703, state 1, line 1 ([`changed_language_setting`]),
/// 6. ENVCHANGE packet size (`cfg.default_packet_size` -> `state.packet_size`),
/// 7. LOGINACK (`min(login.tds_version, 7.4)`, `cfg.program_name` or [`PROGRAM_NAME`] cut
///    by [`program_name_for_login_ack`], [`LOGIN_ACK_VERSION`]),
/// 8. DONE final.
///
/// tiberius accepts this sequence in the three encryption modes
/// (`crates/vauban-tds/tests/tiberius_select1.rs`). `state.packet_size` is the negotiated
/// size: the caller applies it with `set_packet_size` **after** flushing this response, so
/// that the response itself still travels at the previous size.
///
/// # The `old` value of the first ENVCHANGE when the login opens another database
///
/// It stays [`MASTER`]: a login that opens `d` announces `master` -> `d`, not `d` -> `d`
/// (unit test `login_on_another_database_keeps_master_as_the_old_value`).
pub(crate) fn login_response(
    login: &Login7,
    state: &SessionState,
    cfg: &ServerConfig,
) -> Vec<Token> {
    vec![
        Token::EnvChange(EnvChange::Database {
            old: MASTER.into(),
            new: state.database.clone(),
        }),
        Token::Info(changed_database_context(
            &state.database,
            DATABASE_CONTEXT_STATE_LOGIN,
            1,
        )),
        Token::EnvChange(EnvChange::Collation {
            old: None,
            new: Collation::DEFAULT,
        }),
        Token::EnvChange(EnvChange::Language {
            old: String::new(),
            new: LANGUAGE.into(),
        }),
        Token::Info(changed_language_setting(LANGUAGE)),
        Token::EnvChange(EnvChange::PacketSize {
            old: cfg.default_packet_size,
            new: state.packet_size,
        }),
        Token::LoginAck {
            tds_version: login.tds_version.min(TDS_VERSION_7_4),
            program_name: program_name_for_login_ack(
                cfg.program_name.as_deref().unwrap_or(PROGRAM_NAME),
            )
            .into(),
            version: LOGIN_ACK_VERSION,
        },
        Token::Done {
            status: DoneStatus::FINAL,
            cur_cmd: 0,
            row_count: None,
        },
    ]
}

/// The response to a refused LOGIN7: the errors, then a DONE carrying `ERROR`. The caller
/// flushes it and closes the connection.
///
/// # A LOGIN7 that names a database the instance does not hold
///
/// VaubanDB refuses it: 4060, then 18456, then this DONE `ERROR`, and the connection closes
/// (`server.rs`, `check_login`; unit test `login_to_unknown_database_is_4060_then_18456`).
///
/// Deliberate difference from SQL Server, which answers the error 4063 (severity 11,
/// state 1) followed by the sequence of an accepted login, and opens the session in
/// `master`: VaubanDB has neither roles nor a default database per login to fall back to,
/// and 4063 is absent from the catalogue of `vauban-errors`.
pub(crate) fn failure_response(errors: Vec<SqlError>) -> Vec<Token> {
    let mut tokens: Vec<Token> = errors.into_iter().map(Token::Error).collect();
    tokens.push(Token::Done {
        status: DoneStatus::ERROR,
        cur_cmd: 0,
        row_count: None,
    });
    tokens
}

/// Error 18452, severity 14, state 1: the client asked for integrated (SSPI) authentication,
/// which VaubanDB does not offer. The text is the catalogue's; its trailing `%.*ls`
/// (server-side detail) is left empty. `errors` has no named constructor for it.
pub(crate) fn sspi_refused() -> SqlError {
    match message_template(18452) {
        Some(def) => SqlError::new(
            def.number,
            def.severity,
            1,
            def.template.replacen("%.*ls", "", 1),
        ),
        None => SqlError::new(18452, 14, 1, "18452"),
    }
}

/// The refusal as the client must see it: a 18456 loses its server-side state
/// (5 or 8, see `Authenticator`) for [`STATE_18456_CLIENT`]; any other error is unchanged.
pub(crate) fn for_client(err: SqlError) -> SqlError {
    if err.number == 18456 {
        SqlError {
            state: STATE_18456_CLIENT,
            ..err
        }
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
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
}
