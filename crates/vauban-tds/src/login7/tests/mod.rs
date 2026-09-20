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
