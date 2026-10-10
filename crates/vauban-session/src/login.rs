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
        procedure: None,
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
mod tests;
