//! [MS-TDS] 2.2.6.5 PRELOGIN: client request decoding, server response, encryption
//! negotiation.
//!
//! # Wire format ([MS-TDS] 2.2.6.5)
//!
//! A PRELOGIN payload is a list of 5-byte option headers terminated by `TERMINATOR`
//! (0xFF), followed by the option data:
//!
//! ```text
//! PRELOGIN_OPTION = PL_OPTION_TOKEN (1) PL_OFFSET (2, big-endian) PL_OPTION_LENGTH (2, big-endian)
//! PRELOGIN        = *PRELOGIN_OPTION TERMINATOR *PL_OPTION_DATA
//! ```
//!
//! `PL_OFFSET` counts from the start of the payload. Options may appear in any order;
//! a `PL_OPTION_LENGTH` of 0 means "absent" (the server answers THREADID that way).
//!
//! | Token | Name | Data |
//! |---|---|---|
//! | 0x00 | VERSION | `UL_VERSION` (major, minor, build on 2 bytes, big-endian) + `US_SUBBUILD` (2) |
//! | 0x01 | ENCRYPTION | `B_FENCRYPTION` (1): see [`Encryption`] |
//! | 0x02 | INSTOPT | client: `B_INSTVALIDITY`, NUL-terminated instance name; server: 1 byte, 0x00 = valid |
//! | 0x03 | THREADID | `UL_THREADID` (4, little-endian); server: length 0 |
//! | 0x04 | MARS | `B_MARS` (1): 0x00 MARS_OFF, 0x01 MARS_ON |
//! | 0x05 | TRACEID | `GUID_CONNID` (16) + `ACTIVITYID` (16 + 4) = 36 bytes |
//! | 0x06 | FEDAUTHREQUIRED | `B_FEDAUTHREQUIRED` (1) |
//! | 0x07 | NONCEOPT | `NONCE` (32) |
//! | 0xFF | TERMINATOR | none |
//!
//! Byte order of `US_SUBBUILD`: [MS-TDS] states the version is sent in network byte order;
//! the field is kept opaque here (`[u8; 6]`).
//!
//! # Encryption negotiation ([MS-TDS] 2.2.6.5 ENCRYPTION, 3.3.5.1)
//!
//! The server setting derives from the [`EncryptPolicy`]:
//! `Off` → ENCRYPT_NOT_SUP, `Optional` → ENCRYPT_OFF, `Required` → ENCRYPT_REQ. The result
//! is the ENCRYPTION byte of the response plus the [`TlsMode`] the caller applies:
//!
//! | client ↓ / server → | ENCRYPT_NOT_SUP (`Off`) | ENCRYPT_OFF (`Optional`) | ENCRYPT_REQ (`Required`) |
//! |---|---|---|---|
//! | ENCRYPT_OFF | `NotSup`, `None` | `Off`, `LoginOnly` | `Req`, `Full` |
//! | ENCRYPT_ON | `NotSup`, `Refused` | `On`, `Full` | `On`, `Full` |
//! | ENCRYPT_NOT_SUP | `NotSup`, `None` | `NotSup`, `None` | `Req`, `Refused` |
//! | ENCRYPT_REQ | `NotSup`, `Refused` | `On`, `Full` | `On`, `Full` |
//!
//! `LoginOnly`: TLS handshake, LOGIN7 encrypted, then back to clear text. `Refused`: the
//! response is still sent, then the caller closes (the client closes on its side too).
//! The ENCRYPT_REQ row (a client *requiring* encryption) is treated like ENCRYPT_ON; the
//! spec's table lists the client values OFF, ON and NOT_SUP, not REQ.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::TdsError;

/// Server-side encryption policy applied during the PRELOGIN negotiation
/// ([MS-TDS] 2.2.6.5, option ENCRYPTION).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptPolicy {
    /// No TLS at all (server announces ENCRYPT_NOT_SUP); clients that insist on
    /// encryption are refused. `--encrypt off`.
    Off,
    /// Server announces ENCRYPT_OFF: the login alone is encrypted when the client sends
    /// ENCRYPT_OFF, everything is encrypted when it sends ENCRYPT_ON. Default.
    Optional,
    /// Server announces ENCRYPT_REQ: everything is encrypted; clients that cannot encrypt
    /// (ENCRYPT_NOT_SUP) are refused. `--encrypt required`.
    Required,
}

/// `PL_OPTION_TOKEN` values ([MS-TDS] 2.2.6.5).
mod token {
    pub(super) const VERSION: u8 = 0x00;
    pub(super) const ENCRYPTION: u8 = 0x01;
    pub(super) const INSTOPT: u8 = 0x02;
    pub(super) const THREADID: u8 = 0x03;
    pub(super) const MARS: u8 = 0x04;
    pub(super) const TRACEID: u8 = 0x05;
    pub(super) const FEDAUTHREQUIRED: u8 = 0x06;
    pub(super) const NONCEOPT: u8 = 0x07;
    pub(super) const TERMINATOR: u8 = 0xFF;
}

/// Size of one option header: `PL_OPTION_TOKEN` (1) + `PL_OFFSET` (2) + `PL_OPTION_LENGTH` (2).
const OPTION_HEADER_LEN: usize = 5;

/// Value of the ENCRYPTION option, `B_FENCRYPTION` ([MS-TDS] 2.2.6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Encryption {
    /// ENCRYPT_OFF: encryption is available but off.
    Off = 0x00,
    /// ENCRYPT_ON: encryption is available and on.
    On = 0x01,
    /// ENCRYPT_NOT_SUP: encryption is not available.
    NotSup = 0x02,
    /// ENCRYPT_REQ: encryption is required.
    Req = 0x03,
}

impl Encryption {
    /// Maps a `B_FENCRYPTION` byte; `None` for values outside the four TDS 7.x ones
    /// (including the TDS 8.0 client-certificate values 0x80..=0x83).
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::Off),
            0x01 => Some(Self::On),
            0x02 => Some(Self::NotSup),
            0x03 => Some(Self::Req),
            _ => None,
        }
    }

    /// The `B_FENCRYPTION` byte of this value.
    fn to_byte(self) -> u8 {
        self as u8
    }
}

/// Fields of a decoded PRELOGIN ([MS-TDS] 2.2.6.5). Produced by [`decode`] from a client
/// request; the decoder accepts a server response as well (THREADID of length 0 gives
/// `thread_id == None`, INSTOPT `0x00` gives an empty `instance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreLogin {
    /// VERSION: `UL_VERSION` (4) + `US_SUBBUILD` (2), kept as sent.
    pub(crate) version: [u8; 6],
    /// ENCRYPTION.
    pub(crate) encryption: Encryption,
    /// INSTOPT: instance name without its NUL terminator; empty when absent.
    pub(crate) instance: String,
    /// THREADID, little-endian; `None` when absent (length 0).
    pub(crate) thread_id: Option<u32>,
    /// MARS: `true` for MARS_ON. Absent counts as MARS_OFF.
    pub(crate) mars: bool,
    /// TRACEID: `GUID_CONNID` + `ACTIVITYID`, kept as sent.
    pub(crate) trace_id: Option<[u8; 36]>,
    /// FEDAUTHREQUIRED: `B_FEDAUTHREQUIRED`, kept as sent.
    pub(crate) fed_auth_required: Option<u8>,
    /// NONCEOPT.
    pub(crate) nonce: Option<[u8; 32]>,
}

/// Decodes the payload of a PRELOGIN message ([MS-TDS] 2.2.6.5).
///
/// Unknown option tokens are skipped. An option header whose data runs past the payload,
/// a missing TERMINATOR, a fixed-size option of the wrong length, or an ENCRYPTION or MARS
/// byte outside its defined values give `TdsError::Malformed`. VERSION and ENCRYPTION are
/// required (the clients this crate is tested with send both).
/// When an option is repeated, the last occurrence wins.
pub(crate) fn decode(payload: &[u8]) -> Result<PreLogin, TdsError> {
    let mut version = None;
    let mut encryption = None;
    let mut instance = String::new();
    let mut thread_id = None;
    let mut mars = false;
    let mut trace_id = None;
    let mut fed_auth_required = None;
    let mut nonce = None;

    let mut pos = 0usize;
    loop {
        let &tok = payload
            .get(pos)
            .ok_or(TdsError::Malformed("PRELOGIN: missing TERMINATOR"))?;
        if tok == token::TERMINATOR {
            break;
        }
        let header = payload
            .get(pos..pos + OPTION_HEADER_LEN)
            .ok_or(TdsError::Malformed("PRELOGIN: truncated option header"))?;
        let offset = usize::from(u16::from_be_bytes([header[1], header[2]]));
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        pos += OPTION_HEADER_LEN;

        if length == 0 {
            // Length 0 means "absent", whatever the offset says.
            continue;
        }
        let data = payload
            .get(offset..offset + length)
            .ok_or(TdsError::Malformed("PRELOGIN: option data out of bounds"))?;

        match tok {
            token::VERSION => {
                version = Some(fixed::<6>(data, "PRELOGIN: VERSION is not 6 bytes")?);
            }
            token::ENCRYPTION => {
                let byte = single(data, "PRELOGIN: ENCRYPTION is not 1 byte")?;
                encryption = Some(
                    Encryption::from_byte(byte)
                        .ok_or(TdsError::Malformed("PRELOGIN: unknown ENCRYPTION value"))?,
                );
            }
            token::INSTOPT => instance = decode_instance(data),
            token::THREADID => {
                thread_id = Some(u32::from_le_bytes(fixed::<4>(
                    data,
                    "PRELOGIN: THREADID is not 4 bytes",
                )?));
            }
            token::MARS => {
                mars = match single(data, "PRELOGIN: MARS is not 1 byte")? {
                    0x00 => false,
                    0x01 => true,
                    _ => return Err(TdsError::Malformed("PRELOGIN: unknown MARS value")),
                };
            }
            token::TRACEID => {
                trace_id = Some(fixed::<36>(data, "PRELOGIN: TRACEID is not 36 bytes")?);
            }
            token::FEDAUTHREQUIRED => {
                fed_auth_required = Some(single(data, "PRELOGIN: FEDAUTHREQUIRED is not 1 byte")?);
            }
            token::NONCEOPT => {
                nonce = Some(fixed::<32>(data, "PRELOGIN: NONCEOPT is not 32 bytes")?);
            }
            // Tokens this version does not know: ignored, the rest is decoded.
            _ => {}
        }
    }

    Ok(PreLogin {
        version: version.ok_or(TdsError::Malformed("PRELOGIN: missing VERSION"))?,
        encryption: encryption.ok_or(TdsError::Malformed("PRELOGIN: missing ENCRYPTION"))?,
        instance,
        thread_id,
        mars,
        trace_id,
        fed_auth_required,
        nonce,
    })
}

/// Copies a fixed-size option; `Malformed(label)` when the length differs.
fn fixed<const N: usize>(data: &[u8], label: &'static str) -> Result<[u8; N], TdsError> {
    data.try_into().map_err(|_| TdsError::Malformed(label))
}

/// Reads a one-byte option; `Malformed(label)` when the length differs.
fn single(data: &[u8], label: &'static str) -> Result<u8, TdsError> {
    match data {
        [byte] => Ok(*byte),
        _ => Err(TdsError::Malformed(label)),
    }
}

/// INSTOPT data: the bytes up to the first NUL (or all of them if there is none). The
/// V1 has no named instance, so the name is informational and decoded lossily.
fn decode_instance(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Server PRELOGIN response ([MS-TDS] 2.2.6.5). [`PreLoginResponse::encode`] always emits
/// VERSION, ENCRYPTION, INSTOPT (one byte 0x00: instance valid), THREADID (length 0) and
/// MARS (0x00, MARS_OFF), then FEDAUTHREQUIRED when the client
/// sent it, then TERMINATOR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreLoginResponse {
    /// VERSION announced by the server, as `UL_VERSION` + `US_SUBBUILD`; chosen by the caller.
    pub(crate) version: [u8; 6],
    /// ENCRYPTION byte from [`negotiate`].
    pub(crate) encryption: Encryption,
    /// FEDAUTHREQUIRED value to echo (0x00 in V1), `Some` only if the client sent the option.
    pub(crate) fed_auth_required: Option<u8>,
}

impl PreLoginResponse {
    /// Serialises the response payload: option headers, TERMINATOR, then the data in the
    /// same order as the headers.
    pub(crate) fn encode(&self) -> Bytes {
        /// INSTOPT response: `B_INSTVALIDITY` 0x00, the instance name is valid.
        const INSTANCE_VALID: [u8; 1] = [0x00];
        /// MARS response: MARS_OFF.
        const MARS_OFF: [u8; 1] = [0x00];
        /// THREADID response: length 0.
        const NO_DATA: [u8; 0] = [];

        let encryption = [self.encryption.to_byte()];
        let fed_auth = self.fed_auth_required.map(|value| [value]);
        // (token, data) in wire order.
        let mut options: Vec<(u8, &[u8])> = vec![
            (token::VERSION, self.version.as_slice()),
            (token::ENCRYPTION, encryption.as_slice()),
            (token::INSTOPT, INSTANCE_VALID.as_slice()),
            (token::THREADID, NO_DATA.as_slice()),
            (token::MARS, MARS_OFF.as_slice()),
        ];
        if let Some(fed_auth) = &fed_auth {
            options.push((token::FEDAUTHREQUIRED, fed_auth.as_slice()));
        }

        let header_len = options.len() * OPTION_HEADER_LEN + 1;
        let data_len: usize = options.iter().map(|(_, data)| data.len()).sum();
        let mut buf = BytesMut::with_capacity(header_len + data_len);

        // Option lengths are at most 6 bytes and there are at most 6 options: offsets fit
        // in a u16 by construction, `unwrap_or` only silences the conversion.
        let mut offset = u16::try_from(header_len).unwrap_or(u16::MAX);
        for (tok, data) in &options {
            let length = u16::try_from(data.len()).unwrap_or(u16::MAX);
            buf.put_u8(*tok);
            buf.put_u16(offset);
            buf.put_u16(length);
            offset = offset.saturating_add(length);
        }
        buf.put_u8(token::TERMINATOR);
        for (_, data) in &options {
            buf.put_slice(data);
        }
        buf.freeze()
    }
}

/// What the caller does with the connection once the PRELOGIN response is sent
/// ([MS-TDS] 2.2.6.5 ENCRYPTION, 3.3.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TlsMode {
    /// No TLS: everything in clear text.
    None,
    /// TLS handshake, LOGIN7 encrypted, then back to clear text (ENCRYPT_OFF).
    LoginOnly,
    /// TLS handshake, then the whole connection stays encrypted.
    Full,
    /// Client and server disagree: send the response, then close.
    Refused,
}

/// Applies the negotiation table (module documentation) to the client's ENCRYPTION value
/// and the server's policy. Returns the ENCRYPTION byte to answer and the [`TlsMode`].
pub(crate) fn negotiate(client: Encryption, policy: EncryptPolicy) -> (Encryption, TlsMode) {
    match (policy, client) {
        // Server ENCRYPT_NOT_SUP: nothing is ever encrypted.
        (EncryptPolicy::Off, Encryption::Off | Encryption::NotSup) => {
            (Encryption::NotSup, TlsMode::None)
        }
        (EncryptPolicy::Off, Encryption::On | Encryption::Req) => {
            (Encryption::NotSup, TlsMode::Refused)
        }
        // Server ENCRYPT_OFF: the client decides.
        (EncryptPolicy::Optional, Encryption::Off) => (Encryption::Off, TlsMode::LoginOnly),
        (EncryptPolicy::Optional, Encryption::On | Encryption::Req) => {
            (Encryption::On, TlsMode::Full)
        }
        (EncryptPolicy::Optional, Encryption::NotSup) => (Encryption::NotSup, TlsMode::None),
        // Server ENCRYPT_REQ: everything is encrypted or the client is refused.
        (EncryptPolicy::Required, Encryption::Off) => (Encryption::Req, TlsMode::Full),
        (EncryptPolicy::Required, Encryption::On | Encryption::Req) => {
            (Encryption::On, TlsMode::Full)
        }
        (EncryptPolicy::Required, Encryption::NotSup) => (Encryption::Req, TlsMode::Refused),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client request (39 bytes): VERSION 15.0.2000.0, ENCRYPT_OFF,
    /// empty INSTOPT, THREADID 0x12345678, MARS_OFF.
    const CLIENT_PRELOGIN: [u8; 39] = [
        0x00, 0x00, 0x1A, 0x00, 0x06, // VERSION @26, 6
        0x01, 0x00, 0x20, 0x00, 0x01, // ENCRYPTION @32, 1
        0x02, 0x00, 0x21, 0x00, 0x01, // INSTOPT @33, 1
        0x03, 0x00, 0x22, 0x00, 0x04, // THREADID @34, 4
        0x04, 0x00, 0x26, 0x00, 0x01, // MARS @38, 1
        0xFF, // TERMINATOR
        0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00, // VERSION data
        0x00, // ENCRYPTION data
        0x00, // INSTOPT data
        0x78, 0x56, 0x34, 0x12, // THREADID data
        0x00, // MARS data
    ];

    fn expected_client() -> PreLogin {
        PreLogin {
            version: [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00],
            encryption: Encryption::Off,
            instance: String::new(),
            thread_id: Some(0x1234_5678),
            mars: false,
            trace_id: None,
            fed_auth_required: None,
            nonce: None,
        }
    }

    fn malformed(result: Result<PreLogin, TdsError>) -> &'static str {
        match result {
            Err(TdsError::Malformed(label)) => label,
            other => panic!("expected TdsError::Malformed, got {other:?}"),
        }
    }

    #[test]
    fn decode_client_prelogin() {
        let prelogin = decode(&CLIENT_PRELOGIN).unwrap();
        assert_eq!(prelogin, expected_client());
    }

    #[test]
    fn decode_ignores_unknown_option() {
        // Same as CLIENT_PRELOGIN with an extra option 0x0B of 2 bytes appended.
        let mut payload = vec![
            0x00, 0x00, 0x1F, 0x00, 0x06, // VERSION @31, 6
            0x01, 0x00, 0x25, 0x00, 0x01, // ENCRYPTION @37, 1
            0x02, 0x00, 0x26, 0x00, 0x01, // INSTOPT @38, 1
            0x03, 0x00, 0x27, 0x00, 0x04, // THREADID @39, 4
            0x04, 0x00, 0x2B, 0x00, 0x01, // MARS @43, 1
            0x0B, 0x00, 0x2C, 0x00, 0x02, // unknown token @44, 2
            0xFF,
        ];
        payload.extend_from_slice(&[0x0F, 0x00, 0x07, 0xD0, 0x00, 0x00]);
        payload.extend_from_slice(&[0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00]);
        payload.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(payload.len(), 46);

        let prelogin = decode(&payload).unwrap();
        assert_eq!(prelogin, expected_client());
    }

    #[test]
    fn decode_rejects_out_of_bounds() {
        // MARS says 2 bytes at offset 38: one byte past the end of the 39-byte payload.
        let mut too_long = CLIENT_PRELOGIN;
        too_long[24] = 0x02;
        assert_eq!(
            malformed(decode(&too_long)),
            "PRELOGIN: option data out of bounds"
        );

        // Offset beyond the payload.
        let mut far_offset = CLIENT_PRELOGIN;
        far_offset[1] = 0x10;
        assert_eq!(
            malformed(decode(&far_offset)),
            "PRELOGIN: option data out of bounds"
        );

        // No TERMINATOR: a single THREADID header of length 0, then nothing.
        let headers_only = [0x03, 0x00, 0x06, 0x00, 0x00];
        assert_eq!(
            malformed(decode(&headers_only)),
            "PRELOGIN: missing TERMINATOR"
        );

        // No TERMINATOR after the five client headers: the first option (VERSION) already
        // points past the 25-byte payload, which is reported first.
        assert_eq!(
            malformed(decode(&CLIENT_PRELOGIN[..25])),
            "PRELOGIN: option data out of bounds"
        );

        // A THREADID header of length 0, then a second header cut after 3 of its 5 bytes.
        let truncated_header = [0x03, 0x00, 0x08, 0x00, 0x00, 0x05, 0x00, 0x00];
        assert_eq!(
            malformed(decode(&truncated_header)),
            "PRELOGIN: truncated option header"
        );

        assert_eq!(malformed(decode(&[])), "PRELOGIN: missing TERMINATOR");
    }

    #[test]
    fn decode_rejects_wrong_sizes_and_values() {
        // VERSION of 5 bytes.
        let mut short_version = CLIENT_PRELOGIN;
        short_version[4] = 0x05;
        assert_eq!(
            malformed(decode(&short_version)),
            "PRELOGIN: VERSION is not 6 bytes"
        );

        // ENCRYPTION 0x80 (TDS 8.0 client certificate): not a TDS 7.4 value.
        let mut bad_encryption = CLIENT_PRELOGIN;
        bad_encryption[32] = 0x80;
        assert_eq!(
            malformed(decode(&bad_encryption)),
            "PRELOGIN: unknown ENCRYPTION value"
        );

        // MARS 0x02.
        let mut bad_mars = CLIENT_PRELOGIN;
        bad_mars[38] = 0x02;
        assert_eq!(malformed(decode(&bad_mars)), "PRELOGIN: unknown MARS value");

        // VERSION with length 0 counts as absent.
        let mut no_version = CLIENT_PRELOGIN;
        no_version[4] = 0x00;
        assert_eq!(malformed(decode(&no_version)), "PRELOGIN: missing VERSION");

        let mut no_encryption = CLIENT_PRELOGIN;
        no_encryption[9] = 0x00;
        assert_eq!(
            malformed(decode(&no_encryption)),
            "PRELOGIN: missing ENCRYPTION"
        );
    }

    #[test]
    fn decode_all_options_in_any_order() {
        // Headers in reverse token order, data laid out in yet another order.
        let mut payload: Vec<u8> = Vec::new();
        let header_len = 8 * OPTION_HEADER_LEN + 1;
        let mut offsets = std::collections::HashMap::new();
        let mut data: Vec<u8> = Vec::new();
        let mut place = |tok: u8, bytes: &[u8], data: &mut Vec<u8>| {
            offsets.insert(tok, header_len + data.len());
            data.extend_from_slice(bytes);
        };
        let nonce: [u8; 32] = std::array::from_fn(|i| 0xC0 + i as u8);
        let trace: [u8; 36] = std::array::from_fn(|i| 0x40 + i as u8);
        place(token::NONCEOPT, &nonce, &mut data);
        place(token::INSTOPT, b"SQLEXPRESS\0", &mut data);
        place(
            token::VERSION,
            &[0x0F, 0x00, 0x07, 0xD0, 0x00, 0x01],
            &mut data,
        );
        place(token::MARS, &[0x01], &mut data);
        place(token::FEDAUTHREQUIRED, &[0x01], &mut data);
        place(token::ENCRYPTION, &[0x01], &mut data);
        place(token::TRACEID, &trace, &mut data);
        place(token::THREADID, &[0x01, 0x00, 0x00, 0x00], &mut data);

        let lengths = [
            (token::NONCEOPT, 32u16),
            (token::TRACEID, 36),
            (token::FEDAUTHREQUIRED, 1),
            (token::MARS, 1),
            (token::THREADID, 4),
            (token::INSTOPT, 11),
            (token::ENCRYPTION, 1),
            (token::VERSION, 6),
        ];
        for (tok, len) in lengths {
            payload.push(tok);
            payload.extend_from_slice(&(offsets[&tok] as u16).to_be_bytes());
            payload.extend_from_slice(&len.to_be_bytes());
        }
        payload.push(token::TERMINATOR);
        payload.extend_from_slice(&data);

        let prelogin = decode(&payload).unwrap();
        assert_eq!(
            prelogin,
            PreLogin {
                version: [0x0F, 0x00, 0x07, 0xD0, 0x00, 0x01],
                encryption: Encryption::On,
                instance: "SQLEXPRESS".to_owned(),
                thread_id: Some(1),
                mars: true,
                trace_id: Some(trace),
                fed_auth_required: Some(0x01),
                nonce: Some(nonce),
            }
        );
    }

    #[test]
    fn decode_instance_without_nul_is_lenient() {
        assert_eq!(decode_instance(b"MSSQL"), "MSSQL");
        assert_eq!(decode_instance(b"A\0B"), "A");
        assert_eq!(decode_instance(b"\0"), "");
    }

    #[test]
    fn encode_response_bytes() {
        let response = PreLoginResponse {
            version: [0x10, 0x00, 0x03, 0xE8, 0x00, 0x06],
            encryption: Encryption::Off,
            fed_auth_required: None,
        };
        let expected: [u8; 35] = [
            0x00, 0x00, 0x1A, 0x00, 0x06, // VERSION @26, 6
            0x01, 0x00, 0x20, 0x00, 0x01, // ENCRYPTION @32, 1
            0x02, 0x00, 0x21, 0x00, 0x01, // INSTOPT @33, 1
            0x03, 0x00, 0x22, 0x00, 0x00, // THREADID @34, 0
            0x04, 0x00, 0x22, 0x00, 0x01, // MARS @34, 1
            0xFF, // TERMINATOR
            0x10, 0x00, 0x03, 0xE8, 0x00, 0x06, // VERSION data
            0x00, // ENCRYPTION data
            0x00, // INSTOPT data
            0x00, // MARS data
        ];
        assert_eq!(response.encode().as_ref(), &expected);
    }

    #[test]
    fn encode_response_with_fed_auth_required() {
        let response = PreLoginResponse {
            version: [0x10, 0x00, 0x03, 0xE8, 0x00, 0x06],
            encryption: Encryption::Req,
            fed_auth_required: Some(0x00),
        };
        let expected: [u8; 41] = [
            0x00, 0x00, 0x1F, 0x00, 0x06, // VERSION @31, 6
            0x01, 0x00, 0x25, 0x00, 0x01, // ENCRYPTION @37, 1
            0x02, 0x00, 0x26, 0x00, 0x01, // INSTOPT @38, 1
            0x03, 0x00, 0x27, 0x00, 0x00, // THREADID @39, 0
            0x04, 0x00, 0x27, 0x00, 0x01, // MARS @39, 1
            0x06, 0x00, 0x28, 0x00, 0x01, // FEDAUTHREQUIRED @40, 1
            0xFF, // TERMINATOR
            0x10, 0x00, 0x03, 0xE8, 0x00, 0x06, // VERSION data
            0x03, // ENCRYPTION data
            0x00, // INSTOPT data
            0x00, // MARS data
            0x00, // FEDAUTHREQUIRED data
        ];
        assert_eq!(response.encode().as_ref(), &expected);
    }

    #[test]
    fn response_roundtrip() {
        for fed_auth_required in [None, Some(0x00)] {
            for encryption in [
                Encryption::Off,
                Encryption::On,
                Encryption::NotSup,
                Encryption::Req,
            ] {
                let response = PreLoginResponse {
                    version: [0x10, 0x00, 0x03, 0xE8, 0x00, 0x06],
                    encryption,
                    fed_auth_required,
                };
                let decoded = decode(&response.encode()).unwrap();
                assert_eq!(
                    decoded,
                    PreLogin {
                        version: response.version,
                        encryption,
                        instance: String::new(),
                        thread_id: None,
                        mars: false,
                        trace_id: None,
                        fed_auth_required,
                        nonce: None,
                    }
                );
            }
        }
    }

    #[test]
    fn negotiate_matrix() {
        use EncryptPolicy as P;
        use Encryption::{NotSup, Off, On, Req};

        let cases = [
            // (client, policy) -> (response, mode)
            ((Off, P::Off), (NotSup, TlsMode::None)),
            ((Off, P::Optional), (Off, TlsMode::LoginOnly)),
            ((Off, P::Required), (Req, TlsMode::Full)),
            ((On, P::Off), (NotSup, TlsMode::Refused)),
            ((On, P::Optional), (On, TlsMode::Full)),
            ((On, P::Required), (On, TlsMode::Full)),
            ((NotSup, P::Off), (NotSup, TlsMode::None)),
            ((NotSup, P::Optional), (NotSup, TlsMode::None)),
            ((NotSup, P::Required), (Req, TlsMode::Refused)),
            ((Req, P::Off), (NotSup, TlsMode::Refused)),
            ((Req, P::Optional), (On, TlsMode::Full)),
            ((Req, P::Required), (On, TlsMode::Full)),
        ];
        assert_eq!(cases.len(), 12);
        for ((client, policy), expected) in cases {
            assert_eq!(
                negotiate(client, policy),
                expected,
                "client {client:?}, policy {policy:?}"
            );
        }
    }

    #[test]
    fn encryption_byte_mapping() {
        for byte in 0x00..=0x03 {
            assert_eq!(Encryption::from_byte(byte).unwrap().to_byte(), byte);
        }
        assert_eq!(Encryption::from_byte(0x04), None);
        assert_eq!(Encryption::from_byte(0x80), None);
        assert_eq!(Encryption::from_byte(0xFF), None);
    }
}
