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
