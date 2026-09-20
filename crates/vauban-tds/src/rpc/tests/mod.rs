use vauban_types::{Len, SqlString};

use super::*;

/// The 22-byte minimal ALL_HEADERS block: Transaction Descriptor 0, one request.
const HEADERS: [u8; 22] = [
    0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
];

/// Parses `"FF FF 0A 00"` into bytes.
fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

/// UTF-16LE bytes of `s`.
fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// `Value::String` of `s`.
fn s(text: &str) -> Value {
    Value::String(SqlString { text: text.into() })
}

/// The minimal ALL_HEADERS followed by `body`.
fn message(body: &[u8]) -> Vec<u8> {
    let mut out = HEADERS.to_vec();
    out.extend_from_slice(body);
    out
}

/// `sp_executesql` by ProcID, no options, one unnamed `nvarchar(4000)` parameter
/// holding `SELECT 1` (the first parameter every driver sends).
fn sp_executesql_select_1() -> Vec<u8> {
    let mut body = hex("FF FF 0A 00 00 00");
    body.extend_from_slice(&hex("00 00 E7 40 1F 09 04 D0 00 34 10 00"));
    body.extend_from_slice(&utf16le("SELECT 1"));
    body
}

/// `sp_help` by name, no options, one OUTPUT `int` parameter `@p` holding NULL.
fn sp_help_output_param() -> Vec<u8> {
    let mut body = hex("07 00");
    body.extend_from_slice(&utf16le("sp_help"));
    body.extend_from_slice(&hex("00 00"));
    body.extend_from_slice(&hex("02 40 00 70 00 01 26 04 00"));
    body
}

#[test]
fn decode_sp_executesql_by_id() {
    let rpc = decode(&message(&sp_executesql_select_1())).unwrap();
    assert_eq!(rpc.proc, RpcProc::Id(10));
    assert_eq!(rpc.options, 0);
    assert_eq!(rpc.transaction_descriptor, 0);
    assert_eq!(rpc.params.len(), 1);
    let param = &rpc.params[0];
    assert_eq!(param.name, "");
    assert!(!param.output);
    assert!(!param.default);
    assert_eq!(param.ty.ty, SqlType::NVarChar(Len::Fixed(4000)));
    assert!(param.ty.nullable);
    assert_eq!(param.value, s("SELECT 1"));
}

#[test]
fn decode_named_proc_with_output_param() {
    let rpc = decode(&message(&sp_help_output_param())).unwrap();
    assert_eq!(rpc.proc, RpcProc::Name("sp_help".into()));
    assert_eq!(rpc.options, 0);
    assert_eq!(rpc.params.len(), 1);
    let param = &rpc.params[0];
    assert_eq!(param.name, "@p");
    assert!(param.output);
    assert!(!param.default);
    assert_eq!(param.ty.ty, SqlType::Int);
    assert_eq!(param.value, Value::Null);
}

#[test]
fn decode_option_flags() {
    let mut body = sp_executesql_select_1();
    body[4] = 0x01; // fWithRecomp
    assert_eq!(decode(&message(&body)).unwrap().options, 1);

    // Delivered raw, reserved bits included.
    body[4] = 0x07;
    body[5] = 0x80;
    assert_eq!(decode(&message(&body)).unwrap().options, 0x8007);
}

#[test]
fn decode_default_flag() {
    let mut body = sp_help_output_param();
    body[2 + 14 + 2 + 1 + 4] = 0x02; // StatusFlags of "@p"
    let rpc = decode(&message(&body)).unwrap();
    assert!(rpc.params[0].default);
    assert!(!rpc.params[0].output);

    // Both bits at once.
    body[2 + 14 + 2 + 1 + 4] = 0x03;
    let rpc = decode(&message(&body)).unwrap();
    assert!(rpc.params[0].default);
    assert!(rpc.params[0].output);
}

#[test]
fn decode_encrypted_param_unsupported() {
    let mut body = sp_help_output_param();
    body[2 + 14 + 2 + 1 + 4] = 0x08; // fEncrypted
    assert!(matches!(
        decode(&message(&body)),
        Err(TdsError::Unsupported(_))
    ));
    // Combined with other bits too.
    body[2 + 14 + 2 + 1 + 4] = 0x09;
    assert!(matches!(
        decode(&message(&body)),
        Err(TdsError::Unsupported(_))
    ));
}

#[test]
fn decode_rpc_batch_unsupported() {
    let mut body = sp_executesql_select_1();
    body.push(BATCH_FLAG);
    body.extend_from_slice(&sp_help_output_param());
    assert!(matches!(
        decode(&message(&body)),
        Err(TdsError::Unsupported("RPC batch"))
    ));

    // NoExecFlag is the other separator of [MS-TDS] 2.2.6.6.
    let mut body = sp_executesql_select_1();
    body.push(NO_EXEC_FLAG);
    body.extend_from_slice(&sp_help_output_param());
    assert!(matches!(
        decode(&message(&body)),
        Err(TdsError::Unsupported("RPC batch"))
    ));

    // A lone separator with nothing behind it is refused the same way.
    let mut body = sp_executesql_select_1();
    body.push(BATCH_FLAG);
    assert!(matches!(
        decode(&message(&body)),
        Err(TdsError::Unsupported("RPC batch"))
    ));
}

#[test]
fn decode_truncated_is_malformed() {
    // Length of the message once `OptionFlags` is read: a prefix that stops exactly
    // there is a complete RPC with no parameter, the only well-formed proper prefix.
    for (body, no_param_len) in [
        (sp_executesql_select_1(), HEADERS.len() + 6),
        (sp_help_output_param(), HEADERS.len() + 2 + 14 + 2),
    ] {
        let full = message(&body);
        assert!(decode(&full[..no_param_len]).unwrap().params.is_empty());
        // Every other prefix shorter than the full message, the ALL_HEADERS alone
        // (a message with no NameLenProcID) included.
        for len in (0..full.len()).filter(|&len| len != no_param_len) {
            assert!(
                matches!(decode(&full[..len]), Err(TdsError::Malformed(_))),
                "prefix of {len} bytes"
            );
        }
    }
}

#[test]
fn decode_no_params() {
    // `sp_who` by name, no option, no parameter.
    let mut body = hex("06 00");
    body.extend_from_slice(&utf16le("sp_who"));
    body.extend_from_slice(&hex("00 00"));
    let rpc = decode(&message(&body)).unwrap();
    assert_eq!(rpc.proc, RpcProc::Name("sp_who".into()));
    assert!(rpc.params.is_empty());
}

#[test]
fn decode_three_params_as_drivers_send_sp_executesql() {
    // Text, declaration, then an unnamed `int` value (INTNTYPE 4, 42).
    let mut body = hex("FF FF 0A 00 00 00");
    body.extend_from_slice(&hex("00 00 E7 40 1F 09 04 D0 00 34 14 00"));
    body.extend_from_slice(&utf16le("SELECT @P1"));
    body.extend_from_slice(&hex("00 00 E7 40 1F 09 04 D0 00 34 0E 00"));
    body.extend_from_slice(&utf16le("@P1 int"));
    body.extend_from_slice(&hex("00 00 26 04 04 2A 00 00 00"));
    let rpc = decode(&message(&body)).unwrap();
    assert_eq!(rpc.proc, RpcProc::SP_EXECUTESQL);
    assert_eq!(rpc.params.len(), 3);
    assert_eq!(rpc.params[0].value, s("SELECT @P1"));
    assert_eq!(rpc.params[1].value, s("@P1 int"));
    assert_eq!(rpc.params[2].name, "");
    assert_eq!(rpc.params[2].ty, TypeInfo::new(SqlType::Int, true));
    assert_eq!(rpc.params[2].value, Value::I32(42));
}

#[test]
fn decode_nulltype_param() {
    // `@p` with NULLTYPE: no length byte, no data, and the next parameter follows.
    let mut body = hex("FF FF 0A 00 00 00");
    body.extend_from_slice(&hex("02 40 00 70 00 00 1F"));
    body.extend_from_slice(&hex("00 00 26 04 04 01 00 00 00"));
    let rpc = decode(&message(&body)).unwrap();
    assert_eq!(rpc.params.len(), 2);
    assert_eq!(rpc.params[0].name, "@p");
    assert_eq!(rpc.params[0].ty, TypeInfo::new(SqlType::Int, true));
    assert_eq!(rpc.params[0].value, Value::Null);
    assert_eq!(rpc.params[1].value, Value::I32(1));
}

#[test]
fn decode_transaction_descriptor() {
    let mut payload = HEADERS.to_vec();
    payload[10..18].copy_from_slice(&0x0123_4567_89AB_CDEFu64.to_le_bytes());
    payload.extend_from_slice(&sp_executesql_select_1());
    let rpc = decode(&payload).unwrap();
    assert_eq!(rpc.transaction_descriptor, 0x0123_4567_89AB_CDEF);
}

#[test]
fn decode_unsupported_param_type_propagates() {
    // `xml` (0xF1) is refused by `types::decode`.
    let mut body = hex("FF FF 0A 00 00 00");
    body.extend_from_slice(&hex("00 00 F1 00"));
    assert!(matches!(
        decode(&message(&body)),
        Err(TdsError::Unsupported(_))
    ));
}

#[test]
fn decode_without_all_headers_is_malformed() {
    assert!(matches!(
        decode(&sp_executesql_select_1()),
        Err(TdsError::Malformed(_))
    ));
    assert!(matches!(decode(&[]), Err(TdsError::Malformed(_))));
}

#[test]
fn well_known_names() {
    assert_eq!(RpcProc::Id(10).well_known_name(), Some("sp_executesql"));
    assert_eq!(RpcProc::Id(11).well_known_name(), Some("sp_prepare"));
    assert_eq!(RpcProc::Id(12).well_known_name(), Some("sp_execute"));
    assert_eq!(RpcProc::Id(13).well_known_name(), Some("sp_prepexec"));
    assert_eq!(RpcProc::Id(14).well_known_name(), Some("sp_prepexecrpc"));
    assert_eq!(RpcProc::Id(15).well_known_name(), Some("sp_unprepare"));
    assert_eq!(RpcProc::Id(1).well_known_name(), Some("sp_cursor"));
    assert_eq!(RpcProc::Id(9).well_known_name(), Some("sp_cursorclose"));
    assert_eq!(RpcProc::Id(0).well_known_name(), None);
    assert_eq!(RpcProc::Id(99).well_known_name(), None);
    assert_eq!(
        RpcProc::Name("sp_executesql".into()).well_known_name(),
        None
    );

    assert_eq!(RpcProc::SP_EXECUTESQL, RpcProc::Id(10));
    assert_eq!(RpcProc::SP_UNPREPARE, RpcProc::Id(15));
    assert_eq!(RpcProc::SP_CURSOR, RpcProc::Id(1));
    assert_eq!(
        RpcProc::SP_CURSOROPEN.well_known_name(),
        Some("sp_cursoropen")
    );
}
