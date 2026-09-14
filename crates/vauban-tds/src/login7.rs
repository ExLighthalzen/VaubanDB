//! [MS-TDS] 2.2.6.4 LOGIN7: login message, password de-obfuscation, feature extensions.
//!
//! Everything here is a pure function of the message payload: no check of the user name,
//! password, TDS version or packet size happens in this crate (`session` decides).

use std::fmt;

use crate::error::TdsError;

/// Decoded LOGIN7 message ([MS-TDS] 2.2.6.4). User name and password are delivered in
/// clear to `session`, which authenticates.
///
/// `Debug` is implemented by hand so that the password never reaches a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct Login7 {
    /// `UserName` field.
    pub username: String,
    /// `Password` field, de-obfuscated ([MS-TDS] 2.2.6.4, password encryption note).
    pub password: String,
    /// `Database` field; `None` when empty.
    pub database: Option<String>,
    /// `AppName` field.
    pub app_name: String,
    /// `HostName` field.
    pub hostname: String,
    /// `ServerName` field.
    pub server_name: String,
    /// `TDSVersion` field (0x74000004 for TDS 7.4).
    pub tds_version: u32,
    /// `PacketSize` field requested by the client.
    pub packet_size: u32,
    /// `ClientLCID` field.
    pub client_lcid: u32,
    /// `fIntSecurity` flag of `OptionFlags2`: the client wants integrated (SSPI) auth.
    /// Also raised when the client ships a non-empty SSPI blob without the flag.
    pub sspi: bool,
    /// `FeatureExt` entries, in order of appearance.
    pub features: Vec<FeatureExt>,
    /// `Language` field; `None` when empty.
    pub language: Option<String>,
    /// `CltIntName` field (client interface name, e.g. `ODBC`).
    pub client_interface_name: String,
    /// `ClientPID` field.
    pub client_pid: u32,
    /// `fReadOnlyIntent` flag of `TypeFlags`.
    pub read_only_intent: bool,
    /// Raw `OptionFlags1`, `OptionFlags2`, `TypeFlags`, `OptionFlags3`, in wire order.
    pub option_flags: [u8; 4],
}

impl fmt::Debug for Login7 {
    /// Every field but the password, which is rendered as `"<redacted>"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login7")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("database", &self.database)
            .field("app_name", &self.app_name)
            .field("hostname", &self.hostname)
            .field("server_name", &self.server_name)
            .field("tds_version", &format_args!("0x{:08X}", self.tds_version))
            .field("packet_size", &self.packet_size)
            .field("client_lcid", &self.client_lcid)
            .field("sspi", &self.sspi)
            .field("features", &self.features)
            .field("language", &self.language)
            .field("client_interface_name", &self.client_interface_name)
            .field("client_pid", &self.client_pid)
            .field("read_only_intent", &self.read_only_intent)
            .field("option_flags", &self.option_flags)
            .finish()
    }
}

/// One FeatureExt entry of a LOGIN7 message ([MS-TDS] 2.2.6.4, FeatureExt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureExt {
    /// `FeatureId` (e.g. [`FeatureExt::SESSIONRECOVERY`], [`FeatureExt::UTF8_SUPPORT`]).
    pub id: u8,
    /// `FeatureData`, opaque to this crate.
    pub data: Vec<u8>,
}

impl FeatureExt {
    /// `SESSIONRECOVERY` feature id ([MS-TDS] 2.2.6.4, FeatureId).
    pub const SESSIONRECOVERY: u8 = 0x01;
    /// `FEDAUTH` feature id ([MS-TDS] 2.2.6.4, FeatureId).
    pub const FEDAUTH: u8 = 0x02;
    /// `COLUMNENCRYPTION` feature id ([MS-TDS] 2.2.6.4, FeatureId).
    pub const COLUMNENCRYPTION: u8 = 0x04;
    /// `GLOBALTRANSACTIONS` feature id ([MS-TDS] 2.2.6.4, FeatureId).
    pub const GLOBALTRANSACTIONS: u8 = 0x05;
    /// `UTF8_SUPPORT` feature id ([MS-TDS] 2.2.6.4, FeatureId).
    pub const UTF8_SUPPORT: u8 = 0x0A;
}

/// Size of the fixed part of a LOGIN7 (TDS 7.2 and later): 36 bytes of scalars, nine
/// `(ib, cch)` pairs, `ClientID`, three more pairs and `cbSSPILong`.
const FIXED_LEN: usize = 94;

/// `fIntSecurity` bit of `OptionFlags2`.
const F_INT_SECURITY: u8 = 0x80;
/// `fReadOnlyIntent` bit of `TypeFlags`.
const F_READ_ONLY_INTENT: u8 = 0x20;
/// `fExtension` bit of `OptionFlags3`: the `Unused` pair becomes `ibExtension/cbExtension`.
const F_EXTENSION: u8 = 0x10;
/// `TERMINATOR` FeatureId closing the FeatureExt block.
const FEATURE_TERMINATOR: u8 = 0xFF;
/// `cbSSPI` sentinel meaning "the real length is in `cbSSPILong`".
const CB_SSPI_LONG_MARKER: u16 = 0xFFFF;

// Byte offsets of the fixed part ([MS-TDS] 2.2.6.4, in wire order).
const OFF_LENGTH: usize = 0;
const OFF_TDS_VERSION: usize = 4;
const OFF_PACKET_SIZE: usize = 8;
const OFF_CLIENT_PID: usize = 16;
const OFF_OPTION_FLAGS: usize = 24;
const OFF_CLIENT_LCID: usize = 32;
const OFF_HOSTNAME: usize = 36;
const OFF_USERNAME: usize = 40;
const OFF_PASSWORD: usize = 44;
const OFF_APP_NAME: usize = 48;
const OFF_SERVER_NAME: usize = 52;
const OFF_EXTENSION: usize = 56;
const OFF_CLT_INT_NAME: usize = 60;
const OFF_LANGUAGE: usize = 64;
const OFF_DATABASE: usize = 68;
const OFF_SSPI: usize = 78;
const OFF_ATCH_DB_FILE: usize = 82;
const OFF_CHANGE_PASSWORD: usize = 86;
const OFF_CB_SSPI_LONG: usize = 90;

/// Decodes the payload of a LOGIN7 packet ([MS-TDS] 2.2.6.4).
///
/// `payload` is the whole message without packet headers; `Length` must equal its size.
pub(crate) fn decode(payload: &[u8]) -> Result<Login7, TdsError> {
    if payload.len() < FIXED_LEN {
        return Err(TdsError::Malformed("LOGIN7 shorter than its fixed part"));
    }
    const FIXED: &str = "LOGIN7 fixed part truncated";

    let length = u32_at(payload, OFF_LENGTH, FIXED)?;
    if usize::try_from(length).ok() != Some(payload.len()) {
        return Err(TdsError::Malformed(
            "LOGIN7 Length does not match the payload size",
        ));
    }
    let tds_version = u32_at(payload, OFF_TDS_VERSION, FIXED)?;
    let packet_size = u32_at(payload, OFF_PACKET_SIZE, FIXED)?;
    // ClientProgVer, ConnectionID and ClientTimeZone carry nothing the server acts on.
    let client_pid = u32_at(payload, OFF_CLIENT_PID, FIXED)?;
    let option_flags: [u8; 4] = array_at(payload, OFF_OPTION_FLAGS, FIXED)?;
    let client_lcid = u32_at(payload, OFF_CLIENT_LCID, FIXED)?;

    let hostname = string_field(payload, OFF_HOSTNAME, "LOGIN7 HostName out of bounds")?;
    let username = string_field(payload, OFF_USERNAME, "LOGIN7 UserName out of bounds")?;
    let (ib_password, cch_password) = offset_length(payload, OFF_PASSWORD)?;
    let password = deobfuscate_password(field_bytes(
        payload,
        ib_password,
        usize::from(cch_password) * 2,
        "LOGIN7 Password out of bounds",
    )?)?;
    let app_name = string_field(payload, OFF_APP_NAME, "LOGIN7 AppName out of bounds")?;
    let server_name = string_field(payload, OFF_SERVER_NAME, "LOGIN7 ServerName out of bounds")?;
    let client_interface_name =
        string_field(payload, OFF_CLT_INT_NAME, "LOGIN7 CltIntName out of bounds")?;
    let language = non_empty(string_field(
        payload,
        OFF_LANGUAGE,
        "LOGIN7 Language out of bounds",
    )?);
    let database = non_empty(string_field(
        payload,
        OFF_DATABASE,
        "LOGIN7 Database out of bounds",
    )?);
    // ClientID (6 bytes at offset 72) is not used by the server.

    // SSPI: only the length matters here; `session` refuses integrated auth.
    let (ib_sspi, cb_sspi) = offset_length(payload, OFF_SSPI)?;
    let sspi_len = if cb_sspi == CB_SSPI_LONG_MARKER {
        usize::try_from(u32_at(payload, OFF_CB_SSPI_LONG, FIXED)?)
            .map_err(|_| TdsError::Malformed("LOGIN7 cbSSPILong out of bounds"))?
    } else {
        usize::from(cb_sspi)
    };
    field_bytes(payload, ib_sspi, sspi_len, "LOGIN7 SSPI out of bounds")?;

    // AtchDBFile and ChangePassword are read (bounds-checked) and ignored.
    let (ib, cch) = offset_length(payload, OFF_ATCH_DB_FILE)?;
    field_bytes(
        payload,
        ib,
        usize::from(cch) * 2,
        "LOGIN7 AtchDBFile out of bounds",
    )?;
    let (ib, cch) = offset_length(payload, OFF_CHANGE_PASSWORD)?;
    field_bytes(
        payload,
        ib,
        usize::from(cch) * 2,
        "LOGIN7 ChangePassword out of bounds",
    )?;

    let sspi = option_flags[1] & F_INT_SECURITY != 0 || sspi_len > 0;
    let read_only_intent = option_flags[2] & F_READ_ONLY_INTENT != 0;
    let features = if option_flags[3] & F_EXTENSION != 0 {
        decode_feature_ext(payload)?
    } else {
        Vec::new()
    };

    Ok(Login7 {
        username,
        password,
        database,
        app_name,
        hostname,
        server_name,
        tds_version,
        packet_size,
        client_lcid,
        sspi,
        features,
        language,
        client_interface_name,
        client_pid,
        read_only_intent,
        option_flags,
    })
}

/// Reverses the client-side password obfuscation ([MS-TDS] 2.2.6.4, Password field):
/// the client swaps the two nibbles of every byte of the UTF-16LE string, then XORs each
/// byte with 0xA5. Decoding therefore XORs first, then swaps the nibbles back.
fn deobfuscate_password(obfuscated: &[u8]) -> Result<String, TdsError> {
    if !obfuscated.len().is_multiple_of(2) {
        return Err(TdsError::Malformed("LOGIN7 Password has an odd byte count"));
    }
    let units: Vec<u16> = obfuscated
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([deobfuscate_byte(pair[0]), deobfuscate_byte(pair[1])]))
        .collect();
    String::from_utf16(&units)
        .map_err(|_| TdsError::Malformed("LOGIN7 Password is not valid UTF-16"))
}

/// XOR with 0xA5, then swap the high and low nibbles (a 4-bit rotation of a `u8`).
fn deobfuscate_byte(byte: u8) -> u8 {
    (byte ^ 0xA5).rotate_right(4)
}

/// Reads the FeatureExt block ([MS-TDS] 2.2.6.4, FeatureExt). `ibExtension` points to a
/// 4-byte `Extension` value in the data section, which is *itself* the offset (from the
/// start of the LOGIN7) of the block: a sequence of `FeatureId` + `FeatureDataLen` +
/// `FeatureData`, closed by the `TERMINATOR` id 0xFF.
fn decode_feature_ext(payload: &[u8]) -> Result<Vec<FeatureExt>, TdsError> {
    let (ib_extension, cb_extension) = offset_length(payload, OFF_EXTENSION)?;
    // The Extension field is a DWORD ([MS-TDS] 2.2.6.4, Data section).
    if cb_extension != 4 {
        return Err(TdsError::Malformed("LOGIN7 cbExtension is not 4"));
    }
    let block_offset = u32_at(
        payload,
        usize::from(ib_extension),
        "LOGIN7 Extension out of bounds",
    )?;
    let mut pos = usize::try_from(block_offset)
        .map_err(|_| TdsError::Malformed("LOGIN7 FeatureExt offset out of bounds"))?;

    let mut features = Vec::new();
    loop {
        let id = *payload
            .get(pos)
            .ok_or(TdsError::Malformed("LOGIN7 FeatureExt without terminator"))?;
        pos += 1;
        if id == FEATURE_TERMINATOR {
            return Ok(features);
        }
        let len = u32_at(payload, pos, "LOGIN7 FeatureDataLen out of bounds")?;
        pos += 4;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| pos.checked_add(len))
            .ok_or(TdsError::Malformed("LOGIN7 FeatureData out of bounds"))?;
        let data = payload
            .get(pos..end)
            .ok_or(TdsError::Malformed("LOGIN7 FeatureData out of bounds"))?;
        pos = end;
        features.push(FeatureExt {
            id,
            data: data.to_vec(),
        });
    }
}

/// Reads an `(ib, cch)` pair of two little-endian `USHORT`s from the fixed part.
fn offset_length(payload: &[u8], at: usize) -> Result<(u16, u16), TdsError> {
    const LABEL: &str = "LOGIN7 fixed part truncated";
    let ib: [u8; 2] = array_at(payload, at, LABEL)?;
    let cch: [u8; 2] = array_at(payload, at + 2, LABEL)?;
    Ok((u16::from_le_bytes(ib), u16::from_le_bytes(cch)))
}

/// Reads a UTF-16LE string field whose `(ib, cch)` pair sits at `pair_at`. `cch` counts
/// UTF-16 code units; an empty field is `""` whatever its `ib`.
fn string_field(
    payload: &[u8],
    pair_at: usize,
    out_of_bounds: &'static str,
) -> Result<String, TdsError> {
    let (ib, cch) = offset_length(payload, pair_at)?;
    let bytes = field_bytes(payload, ib, usize::from(cch) * 2, out_of_bounds)?;
    // `len` is `2 * cch`, so the remainder of `as_chunks` is always empty.
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16(&units).map_err(|_| TdsError::Malformed("LOGIN7 string is not valid UTF-16"))
}

/// Slice of `len` bytes at offset `ib` from the start of the LOGIN7. A zero length yields
/// an empty slice without looking at `ib`.
fn field_bytes<'a>(
    payload: &'a [u8],
    ib: u16,
    len: usize,
    out_of_bounds: &'static str,
) -> Result<&'a [u8], TdsError> {
    if len == 0 {
        return Ok(&[]);
    }
    let start = usize::from(ib);
    start
        .checked_add(len)
        .and_then(|end| payload.get(start..end))
        .ok_or(TdsError::Malformed(out_of_bounds))
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn u32_at(payload: &[u8], at: usize, label: &'static str) -> Result<u32, TdsError> {
    array_at(payload, at, label).map(u32::from_le_bytes)
}

fn array_at<const N: usize>(
    payload: &[u8],
    at: usize,
    label: &'static str,
) -> Result<[u8; N], TdsError> {
    at.checked_add(N)
        .and_then(|end| payload.get(at..end))
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(TdsError::Malformed(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Client-side obfuscation ([MS-TDS] 2.2.6.4): swap nibbles, then XOR 0xA5, on every
    /// byte of the UTF-16LE encoding.
    fn obfuscate(password: &str) -> Vec<u8> {
        utf16le(password)
            .into_iter()
            .map(|b| b.rotate_left(4) ^ 0xA5)
            .collect()
    }

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// What `build_login7` puts on the wire. Strings are in clear; the password gets
    /// obfuscated by the builder.
    struct Spec {
        tds_version: u32,
        packet_size: u32,
        client_pid: u32,
        option_flags: [u8; 4],
        client_lcid: u32,
        hostname: &'static str,
        username: &'static str,
        password: &'static str,
        app_name: &'static str,
        server_name: &'static str,
        /// `Some(block)`: an `Extension` DWORD is emitted and points to `block`, appended
        /// after every other field. The caller sets `fExtension` itself.
        feature_ext: Option<Vec<u8>>,
        clt_int_name: &'static str,
        language: &'static str,
        database: &'static str,
        sspi: Vec<u8>,
        /// Force `cbSSPI = 0xFFFF` and put the SSPI length in `cbSSPILong`.
        sspi_long: bool,
    }

    impl Default for Spec {
        fn default() -> Self {
            Self {
                tds_version: 0x7400_0004,
                packet_size: 4096,
                client_pid: 1234,
                option_flags: [0, 0, 0, 0],
                client_lcid: 0x0409,
                hostname: "host",
                username: "sa",
                password: "sa",
                app_name: "app",
                server_name: "srv",
                feature_ext: None,
                clt_int_name: "ODBC",
                language: "",
                database: "master",
                sspi: Vec::new(),
                sspi_long: false,
            }
        }
    }

    /// Appends an `(ib, count)` pair to `fixed` and the bytes to `data`. `ib` is absolute
    /// (data section starts at `FIXED_LEN`). Returns the absolute offset of the bytes.
    fn push_field(fixed: &mut Vec<u8>, data: &mut Vec<u8>, bytes: &[u8], count: u16) -> usize {
        let ib = FIXED_LEN + data.len();
        fixed.extend_from_slice(&u16::try_from(ib).unwrap().to_le_bytes());
        fixed.extend_from_slice(&count.to_le_bytes());
        data.extend_from_slice(bytes);
        ib
    }

    fn push_string(fixed: &mut Vec<u8>, data: &mut Vec<u8>, s: &str) {
        let bytes = utf16le(s);
        let cch = u16::try_from(s.encode_utf16().count()).unwrap();
        push_field(fixed, data, &bytes, cch);
    }

    /// Builds a well-formed LOGIN7 payload from `spec` ([MS-TDS] 2.2.6.4 layout).
    fn build_login7(spec: &Spec) -> Vec<u8> {
        let mut fixed = Vec::with_capacity(FIXED_LEN);
        let mut data = Vec::new();

        fixed.extend_from_slice(&0u32.to_le_bytes()); // Length, patched below
        fixed.extend_from_slice(&spec.tds_version.to_le_bytes());
        fixed.extend_from_slice(&spec.packet_size.to_le_bytes());
        fixed.extend_from_slice(&0x0700_0000u32.to_le_bytes()); // ClientProgVer
        fixed.extend_from_slice(&spec.client_pid.to_le_bytes());
        fixed.extend_from_slice(&0u32.to_le_bytes()); // ConnectionID
        fixed.extend_from_slice(&spec.option_flags);
        fixed.extend_from_slice(&0i32.to_le_bytes()); // ClientTimeZone
        fixed.extend_from_slice(&spec.client_lcid.to_le_bytes());

        push_string(&mut fixed, &mut data, spec.hostname);
        push_string(&mut fixed, &mut data, spec.username);
        let password = obfuscate(spec.password);
        let cch_password = u16::try_from(spec.password.encode_utf16().count()).unwrap();
        push_field(&mut fixed, &mut data, &password, cch_password);
        push_string(&mut fixed, &mut data, spec.app_name);
        push_string(&mut fixed, &mut data, spec.server_name);
        let extension_at = match &spec.feature_ext {
            Some(_) => Some(push_field(&mut fixed, &mut data, &[0; 4], 4)),
            None => {
                fixed.extend_from_slice(&[0, 0, 0, 0]); // ibUnused, cbUnused
                None
            }
        };
        push_string(&mut fixed, &mut data, spec.clt_int_name);
        push_string(&mut fixed, &mut data, spec.language);
        push_string(&mut fixed, &mut data, spec.database);
        fixed.extend_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]); // ClientID

        let sspi_len = u16::try_from(spec.sspi.len()).unwrap();
        let cb_sspi = if spec.sspi_long { 0xFFFF } else { sspi_len };
        push_field(&mut fixed, &mut data, &spec.sspi, cb_sspi);
        fixed.extend_from_slice(&[0, 0, 0, 0]); // AtchDBFile
        fixed.extend_from_slice(&[0, 0, 0, 0]); // ChangePassword
        let cb_sspi_long: u32 = if spec.sspi_long {
            spec.sspi.len() as u32
        } else {
            0
        };
        fixed.extend_from_slice(&cb_sspi_long.to_le_bytes());
        assert_eq!(fixed.len(), FIXED_LEN);

        if let (Some(block), Some(extension_at)) = (&spec.feature_ext, extension_at) {
            let block_offset = u32::try_from(FIXED_LEN + data.len()).unwrap();
            data.extend_from_slice(block);
            let at = extension_at - FIXED_LEN;
            data[at..at + 4].copy_from_slice(&block_offset.to_le_bytes());
        }

        let mut payload = fixed;
        payload.extend_from_slice(&data);
        let length = u32::try_from(payload.len()).unwrap();
        payload[0..4].copy_from_slice(&length.to_le_bytes());
        payload
    }

    fn expect_malformed(result: Result<Login7, TdsError>) -> &'static str {
        match result {
            Err(TdsError::Malformed(label)) => label,
            Err(other) => panic!("expected Malformed, got {other:?}"),
            Ok(login) => panic!("expected Malformed, got {login:?}"),
        }
    }

    #[test]
    fn password_deobfuscation_vector() {
        assert_eq!(
            deobfuscate_password(&[0x92, 0xA5, 0xB3, 0xA5]).unwrap(),
            "sa"
        );
        assert_eq!(deobfuscate_byte(0xA5), 0x00);
        assert_eq!(deobfuscate_password(&[0xA5, 0xA5]).unwrap(), "\0");
        assert_eq!(deobfuscate_password(&[]).unwrap(), "");
    }

    #[test]
    fn password_roundtrip() {
        let clear = "Pa$$w0rd-éà";
        let wire = obfuscate(clear);
        assert_ne!(wire, utf16le(clear));
        assert_eq!(deobfuscate_password(&wire).unwrap(), clear);
    }

    #[test]
    fn password_rejects_invalid_utf16() {
        // A lone high surrogate (0xD800) is not valid UTF-16.
        let wire: Vec<u8> = [0x00u8, 0xD8]
            .iter()
            .map(|b| b.rotate_left(4) ^ 0xA5)
            .collect();
        assert!(matches!(
            deobfuscate_password(&wire),
            Err(TdsError::Malformed(_))
        ));
        assert!(matches!(
            deobfuscate_password(&[0x92]),
            Err(TdsError::Malformed(_))
        ));
    }

    #[test]
    fn decode_minimal_login7() {
        let payload = build_login7(&Spec::default());
        assert_eq!(&payload[4..8], &[0x04, 0x00, 0x00, 0x74]);
        let login = decode(&payload).unwrap();

        assert_eq!(login.tds_version, 0x7400_0004);
        assert_eq!(login.packet_size, 4096);
        assert_eq!(login.client_pid, 1234);
        assert_eq!(login.client_lcid, 0x0409);
        assert_eq!(login.hostname, "host");
        assert_eq!(login.username, "sa");
        assert_eq!(login.password, "sa");
        assert_eq!(login.app_name, "app");
        assert_eq!(login.server_name, "srv");
        assert_eq!(login.client_interface_name, "ODBC");
        assert_eq!(login.database, Some("master".to_owned()));
        assert_eq!(login.language, None);
        assert!(!login.sspi);
        assert!(!login.read_only_intent);
        assert!(login.features.is_empty());
        assert_eq!(login.option_flags, [0, 0, 0, 0]);
    }

    #[test]
    fn decode_sspi_flag() {
        let payload = build_login7(&Spec {
            option_flags: [0, F_INT_SECURITY, 0, 0],
            password: "",
            sspi: vec![0x60, 0x01, 0x02],
            ..Spec::default()
        });
        let login = decode(&payload).unwrap();
        assert!(login.sspi);
        assert_eq!(login.password, "");
    }

    #[test]
    fn decode_sspi_long_length() {
        let payload = build_login7(&Spec {
            option_flags: [0, F_INT_SECURITY, 0, 0],
            password: "",
            sspi: vec![0x60; 8],
            sspi_long: true,
            ..Spec::default()
        });
        assert_eq!(&payload[OFF_SSPI + 2..OFF_SSPI + 4], &[0xFF, 0xFF]);
        assert!(decode(&payload).unwrap().sspi);

        // cbSSPILong beyond the payload.
        let mut broken = payload.clone();
        broken[OFF_CB_SSPI_LONG..OFF_CB_SSPI_LONG + 4].copy_from_slice(&9999u32.to_le_bytes());
        assert_eq!(
            expect_malformed(decode(&broken)),
            "LOGIN7 SSPI out of bounds"
        );
    }

    #[test]
    fn decode_feature_ext() {
        let payload = build_login7(&Spec {
            option_flags: [0, 0, 0, F_EXTENSION],
            feature_ext: Some(vec![0x0A, 0x01, 0x00, 0x00, 0x00, 0x01, 0xFF]),
            ..Spec::default()
        });
        let login = decode(&payload).unwrap();
        assert_eq!(
            login.features,
            vec![FeatureExt {
                id: FeatureExt::UTF8_SUPPORT,
                data: vec![1]
            }]
        );

        // Same block without its 0xFF terminator.
        let payload = build_login7(&Spec {
            option_flags: [0, 0, 0, F_EXTENSION],
            feature_ext: Some(vec![0x0A, 0x01, 0x00, 0x00, 0x00, 0x01]),
            ..Spec::default()
        });
        assert_eq!(
            expect_malformed(decode(&payload)),
            "LOGIN7 FeatureExt without terminator"
        );
    }

    #[test]
    fn decode_feature_ext_several_entries_and_edge_cases() {
        // Two entries, one of them empty, unknown id kept as is.
        let block = vec![
            0x01, 0x00, 0x00, 0x00, 0x00, // SESSIONRECOVERY, no data
            0x7E, 0x02, 0x00, 0x00, 0x00, 0xAA, 0xBB, // unknown id, 2 bytes
            0xFF,
        ];
        let payload = build_login7(&Spec {
            option_flags: [0, 0, 0, F_EXTENSION],
            feature_ext: Some(block),
            ..Spec::default()
        });
        let login = decode(&payload).unwrap();
        assert_eq!(
            login.features,
            vec![
                FeatureExt {
                    id: FeatureExt::SESSIONRECOVERY,
                    data: vec![]
                },
                FeatureExt {
                    id: 0x7E,
                    data: vec![0xAA, 0xBB]
                },
            ]
        );

        // Only the terminator: no feature.
        let payload = build_login7(&Spec {
            option_flags: [0, 0, 0, F_EXTENSION],
            feature_ext: Some(vec![0xFF]),
            ..Spec::default()
        });
        assert!(decode(&payload).unwrap().features.is_empty());

        // FeatureDataLen larger than what remains.
        let payload = build_login7(&Spec {
            option_flags: [0, 0, 0, F_EXTENSION],
            feature_ext: Some(vec![0x0A, 0x10, 0x00, 0x00, 0x00, 0x01, 0xFF]),
            ..Spec::default()
        });
        assert_eq!(
            expect_malformed(decode(&payload)),
            "LOGIN7 FeatureData out of bounds"
        );

        // fExtension raised but no Extension DWORD (cbExtension = 0).
        let payload = build_login7(&Spec {
            option_flags: [0, 0, 0, F_EXTENSION],
            ..Spec::default()
        });
        assert_eq!(
            expect_malformed(decode(&payload)),
            "LOGIN7 cbExtension is not 4"
        );

        // A block present on the wire but fExtension clear: ignored.
        let payload = build_login7(&Spec {
            feature_ext: Some(vec![0x0A, 0x01, 0x00, 0x00, 0x00, 0x01, 0xFF]),
            ..Spec::default()
        });
        assert!(decode(&payload).unwrap().features.is_empty());
    }

    #[test]
    fn decode_rejects_out_of_bounds() {
        let valid = build_login7(&Spec::default());

        // ibUserName pushed past the end of the payload.
        let mut broken = valid.clone();
        broken[OFF_USERNAME..OFF_USERNAME + 2].copy_from_slice(&0xFFF0u16.to_le_bytes());
        assert_eq!(
            expect_malformed(decode(&broken)),
            "LOGIN7 UserName out of bounds"
        );

        // cchUserName so large that ib + 2 * cch overflows the payload.
        let mut broken = valid.clone();
        broken[OFF_USERNAME + 2..OFF_USERNAME + 4].copy_from_slice(&0x8000u16.to_le_bytes());
        assert_eq!(
            expect_malformed(decode(&broken)),
            "LOGIN7 UserName out of bounds"
        );

        // Length field disagrees with the payload size.
        let mut broken = valid.clone();
        let wrong = u32::try_from(valid.len()).unwrap() + 1;
        broken[0..4].copy_from_slice(&wrong.to_le_bytes());
        assert_eq!(
            expect_malformed(decode(&broken)),
            "LOGIN7 Length does not match the payload size"
        );

        // Payload shorter than the fixed part.
        assert_eq!(
            expect_malformed(decode(&valid[..FIXED_LEN - 1])),
            "LOGIN7 shorter than its fixed part"
        );
        assert!(matches!(decode(&[]), Err(TdsError::Malformed(_))));
    }

    #[test]
    fn decode_rejects_invalid_utf16_identifier() {
        let payload = build_login7(&Spec::default());
        let login = decode(&payload).unwrap();
        assert_eq!(login.username, "sa");
        // Overwrite the first code unit of UserName with a lone low surrogate.
        let ib = usize::from(u16::from_le_bytes([
            payload[OFF_USERNAME],
            payload[OFF_USERNAME + 1],
        ]));
        let mut broken = payload.clone();
        broken[ib..ib + 2].copy_from_slice(&0xDC00u16.to_le_bytes());
        assert_eq!(
            expect_malformed(decode(&broken)),
            "LOGIN7 string is not valid UTF-16"
        );
    }

    #[test]
    fn decode_read_only_intent() {
        let payload = build_login7(&Spec {
            option_flags: [0xE0, 0x03, F_READ_ONLY_INTENT, 0],
            ..Spec::default()
        });
        let login = decode(&payload).unwrap();
        assert!(login.read_only_intent);
        assert_eq!(login.option_flags, [0xE0, 0x03, 0x20, 0x00]);
    }

    #[test]
    fn empty_strings_are_empty_not_error() {
        let payload = build_login7(&Spec {
            hostname: "",
            app_name: "",
            server_name: "",
            clt_int_name: "",
            language: "",
            database: "",
            ..Spec::default()
        });
        let login = decode(&payload).unwrap();
        assert_eq!(login.hostname, "");
        assert_eq!(login.app_name, "");
        assert_eq!(login.server_name, "");
        assert_eq!(login.client_interface_name, "");
        assert_eq!(login.database, None);
        assert_eq!(login.language, None);
    }

    #[test]
    fn decode_language_when_present() {
        let payload = build_login7(&Spec {
            language: "us_english",
            ..Spec::default()
        });
        assert_eq!(
            decode(&payload).unwrap().language,
            Some("us_english".to_owned())
        );
    }

    #[test]
    fn debug_never_prints_password() {
        let payload = build_login7(&Spec {
            username: "alice",
            password: "Sup3r-Secret!",
            ..Spec::default()
        });
        let login = decode(&payload).unwrap();
        assert_eq!(login.password, "Sup3r-Secret!");

        let debug = format!("{login:?}");
        assert!(!debug.contains("Sup3r"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("alice"), "{debug}");
        assert!(debug.contains("0x74000004"), "{debug}");
    }
}
