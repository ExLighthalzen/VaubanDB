//! [MS-TDS] 2.2.6.6 RPC Request: procedure name or ProcID, options, typed parameters.
//!
//! # Wire layout ([MS-TDS] 2.2.6.6), every integer little-endian
//!
//! ```text
//! RPCRequest    = ALL_HEADERS RPCReqBatch *((BatchFlag / NoExecFlag) RPCReqBatch)
//! RPCReqBatch   = NameLenProcID OptionFlags *ParameterData
//! NameLenProcID = (US_VARCHAR ProcName) / (ProcIDSwitch ProcID)
//! ProcIDSwitch  = %xFF %xFF
//! ProcID        = USHORT
//! OptionFlags   = USHORT   ; fWithRecomp 0x0001, fNoMetaData 0x0002, fReuseMetaData 0x0004
//! ParameterData = ParamMetaData ParamLenData
//! ParamMetaData = B_VARCHAR StatusFlags TYPE_INFO
//! StatusFlags   = BYTE     ; fByRefValue 0x01, fDefaultValue 0x02, fEncrypted 0x08
//! ParamLenData  = TYPE_VARBYTE
//! BatchFlag     = %x80     ; TDS 7.2 and later
//! NoExecFlag    = %xFF     ; TDS 7.2 and later
//! ```
//!
//! `ProcName` is a US_VARCHAR: two bytes with the number of UTF-16 code units, then the
//! text. When that count is `0xFFFF` (`ProcIDSwitch`) the two next bytes are a `ProcID`
//! instead of a name; the identifiers the spec lists are the `RpcProc::SP_*` constants.
//! `ParamMetaData.Name` is a B_VARCHAR (one length byte) delivered exactly as sent, `@` or
//! not: `tiberius` 0.12.3 (`tests/tiberius_rpc.rs`) names the three parameters of
//! `sp_executesql` `stmt`, `params` and `@P1`; other drivers send the value parameters
//! **without** a name (length 0). `compat` must therefore pair them by position, never by
//! name; an empty name stays empty. A client may send an `nvarchar` parameter with a
//! zeroed collation, which `types::decode` delivers as `Some(Collation { lcid: 0, .. })`.
//!
//! The TYPE_INFO and the value of each parameter are read by [`crate::types::decode_type_info`]
//! and [`crate::types::decode_value`], with one exception handled here: NULLTYPE
//! (`0x1F`, [MS-TDS] 2.2.5.4.1), the untyped NULL of a parameter, whose value takes no byte
//! at all. `SqlType` has no variant for it; the parameter is delivered as `Value::Null` with
//! a nullable `int` TYPE_INFO, the type T-SQL itself gives a bare `NULL` literal (`SELECT NULL`
//! yields an `int` column). `compat` should rely on the declaration string of `sp_executesql`
//! rather than on that placeholder type.
//!
//! # What this version refuses
//!
//! - `fEncrypted` (Always Encrypted): `Unsupported("encrypted RPC parameter")`;
//! - a second RPC in the same message (`BatchFlag` or `NoExecFlag` after the last parameter,
//!   used by `SqlDataAdapter` batch updates): `Unsupported("RPC batch")`. Any byte left after
//!   the last parameter is treated as such a separator, since a parameter name of 128
//!   (`0x80`) or 255 (`0xFF`) characters is not a valid identifier;
//! - table-valued, `xml`, `sql_variant` and legacy `text` / `ntext` / `image` parameters:
//!   `Unsupported` from `types::decode`.
//!
//! # Response to an RPC (for `compat` and `session`)
//!
//! The result sets of the procedure (COLMETADATA / ROW / DONEINPROC…), then a
//! RETURNSTATUS token, then one RETURNVALUE per OUTPUT parameter (`tokens/return_value.rs`),
//! then a DONEPROC token closing the whole request. Both encoders live in `tokens/`.

use vauban_types::{SqlType, TypeInfo, Value};

use crate::ResetConnection;
use crate::error::TdsError;
use crate::headers::decode_all_headers;
use crate::types::{NULLTYPE, decode_type_info, decode_value};

/// `ProcIDSwitch` ([MS-TDS] 2.2.6.6): the `ProcName` length that announces a `ProcID`.
const PROC_ID_SWITCH: u16 = 0xFFFF;
/// `BatchFlag` ([MS-TDS] 2.2.6.6): another RPC follows in the same message (TDS 7.2+).
const BATCH_FLAG: u8 = 0x80;
/// `NoExecFlag` ([MS-TDS] 2.2.6.6): another RPC follows and this one is not executed.
const NO_EXEC_FLAG: u8 = 0xFF;
/// `StatusFlags.fByRefValue` ([MS-TDS] 2.2.6.6): the parameter is OUTPUT.
const STATUS_BY_REF_VALUE: u8 = 0x01;
/// `StatusFlags.fDefaultValue` ([MS-TDS] 2.2.6.6): use the procedure's default value.
const STATUS_DEFAULT_VALUE: u8 = 0x02;
/// `StatusFlags.fEncrypted` ([MS-TDS] 2.2.6.6): the value is encrypted (Always Encrypted).
const STATUS_ENCRYPTED: u8 = 0x08;

/// Decoded RPC message ([MS-TDS] 2.2.6.6).
#[derive(Debug, Clone, PartialEq)]
pub struct Rpc {
    /// `ProcName` or `ProcID`.
    pub proc: RpcProc,
    /// `OptionFlags` (fWithRecomp, fNoMetaData, fReuseMetaData).
    pub options: u16,
    /// `ParameterData` entries, in order.
    pub params: Vec<RpcParam>,
    /// `TransactionDescriptor` of the ALL_HEADERS block ([MS-TDS] 2.2.5.3.1); 0 when absent.
    pub transaction_descriptor: u64,
    /// RESETCONNECTION or RESETCONNECTIONSKIPTRAN from the first packet's status
    /// ([MS-TDS] 2.2.3.1.2).
    pub reset: ResetConnection,
}

/// Target of an RPC: a procedure name or a well-known `ProcID`
/// ([MS-TDS] 2.2.6.6, ProcIDSwitch: 10 = sp_executesql, 11 = sp_prepare, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcProc {
    /// `ProcName` as US_VARCHAR.
    Name(String),
    /// `ProcID` (0xFFFF switch followed by the identifier).
    Id(u16),
}

/// The `ProcID` values of [MS-TDS] 2.2.6.6 and the procedure each one stands for.
const WELL_KNOWN_PROC_IDS: [(u16, &str); 15] = [
    (1, "sp_cursor"),
    (2, "sp_cursoropen"),
    (3, "sp_cursorprepare"),
    (4, "sp_cursorexecute"),
    (5, "sp_cursorprepexec"),
    (6, "sp_cursorunprepare"),
    (7, "sp_cursorfetch"),
    (8, "sp_cursoroption"),
    (9, "sp_cursorclose"),
    (10, "sp_executesql"),
    (11, "sp_prepare"),
    (12, "sp_execute"),
    (13, "sp_prepexec"),
    (14, "sp_prepexecrpc"),
    (15, "sp_unprepare"),
];

impl RpcProc {
    /// `ProcID` 1: `sp_cursor`.
    pub const SP_CURSOR: Self = Self::Id(1);
    /// `ProcID` 2: `sp_cursoropen`.
    pub const SP_CURSOROPEN: Self = Self::Id(2);
    /// `ProcID` 3: `sp_cursorprepare`.
    pub const SP_CURSORPREPARE: Self = Self::Id(3);
    /// `ProcID` 4: `sp_cursorexecute`.
    pub const SP_CURSOREXECUTE: Self = Self::Id(4);
    /// `ProcID` 5: `sp_cursorprepexec`.
    pub const SP_CURSORPREPEXEC: Self = Self::Id(5);
    /// `ProcID` 6: `sp_cursorunprepare`.
    pub const SP_CURSORUNPREPARE: Self = Self::Id(6);
    /// `ProcID` 7: `sp_cursorfetch`.
    pub const SP_CURSORFETCH: Self = Self::Id(7);
    /// `ProcID` 8: `sp_cursoroption`.
    pub const SP_CURSOROPTION: Self = Self::Id(8);
    /// `ProcID` 9: `sp_cursorclose`.
    pub const SP_CURSORCLOSE: Self = Self::Id(9);
    /// `ProcID` 10: `sp_executesql`.
    pub const SP_EXECUTESQL: Self = Self::Id(10);
    /// `ProcID` 11: `sp_prepare`.
    pub const SP_PREPARE: Self = Self::Id(11);
    /// `ProcID` 12: `sp_execute`.
    pub const SP_EXECUTE: Self = Self::Id(12);
    /// `ProcID` 13: `sp_prepexec`.
    pub const SP_PREPEXEC: Self = Self::Id(13);
    /// `ProcID` 14: `sp_prepexecrpc`.
    pub const SP_PREPEXECRPC: Self = Self::Id(14);
    /// `ProcID` 15: `sp_unprepare`.
    pub const SP_UNPREPARE: Self = Self::Id(15);

    /// The procedure name a `ProcID` of [MS-TDS] 2.2.6.6 stands for (`Id(10)` →
    /// `"sp_executesql"`); `None` for an unknown identifier and for `Name(_)`, whose text
    /// is not resolved here.
    pub fn well_known_name(&self) -> Option<&'static str> {
        match self {
            Self::Id(id) => WELL_KNOWN_PROC_IDS
                .iter()
                .find(|(known, _)| known == id)
                .map(|(_, name)| *name),
            Self::Name(_) => None,
        }
    }
}

/// One RPC parameter ([MS-TDS] 2.2.6.6, ParameterData).
#[derive(Debug, Clone, PartialEq)]
pub struct RpcParam {
    /// `ParamMetaData.Name`, without the leading `@` handling (kept as sent).
    pub name: String,
    /// `StatusFlags.fByRefValue`: OUTPUT parameter.
    pub output: bool,
    /// `StatusFlags.fDefaultValue`: use the procedure's default.
    pub default: bool,
    /// `TYPE_INFO` of the parameter.
    pub ty: TypeInfo,
    /// Decoded value (`Value::Null` for NULL).
    pub value: Value,
}

/// Decodes the payload of an RPC packet ([MS-TDS] 2.2.6.6): ALL_HEADERS, `NameLenProcID`,
/// `OptionFlags`, then the parameters up to the end of the payload.
///
/// A truncated or inconsistent stream fails with `Malformed`; the forms this version does
/// not accept (module documentation) fail with `Unsupported`. Never panics.
pub(crate) fn decode(payload: &[u8]) -> Result<Rpc, TdsError> {
    let (headers, mut rest) = decode_all_headers(payload)?;
    let input = &mut rest;

    let proc = match read_u16_le(input, "RPC NameLenProcID")? {
        PROC_ID_SWITCH => RpcProc::Id(read_u16_le(input, "RPC ProcID")?),
        units => RpcProc::Name(read_utf16le(input, usize::from(units), "RPC ProcName")?),
    };
    let options = read_u16_le(input, "RPC OptionFlags")?;

    let mut params = Vec::new();
    while let Some(&first) = input.first() {
        if first == BATCH_FLAG || first == NO_EXEC_FLAG {
            return Err(TdsError::Unsupported("RPC batch"));
        }
        params.push(decode_param(input)?);
    }

    Ok(Rpc {
        proc,
        options,
        params,
        transaction_descriptor: headers.transaction_descriptor,
        reset: ResetConnection::None,
    })
}

/// Reads one `ParameterData` ([MS-TDS] 2.2.6.6): `ParamMetaData` (name, `StatusFlags`,
/// TYPE_INFO) then the value.
fn decode_param(input: &mut &[u8]) -> Result<RpcParam, TdsError> {
    let name_units = usize::from(read_u8(input, "RPC parameter name length")?);
    let name = read_utf16le(input, name_units, "RPC parameter name")?;
    let status = read_u8(input, "RPC parameter StatusFlags")?;
    if status & STATUS_ENCRYPTED != 0 {
        return Err(TdsError::Unsupported("encrypted RPC parameter"));
    }
    let output = status & STATUS_BY_REF_VALUE != 0;
    let default = status & STATUS_DEFAULT_VALUE != 0;

    // NULLTYPE: no length, no data; see the module documentation for the placeholder type.
    let (ty, value) = if input.first() == Some(&NULLTYPE) {
        *input = &input[1..];
        (TypeInfo::new(SqlType::Int, true), Value::Null)
    } else {
        let ty = decode_type_info(input)?;
        let value = decode_value(&ty, input)?;
        (ty, value)
    };

    Ok(RpcParam {
        name,
        output,
        default,
        ty,
        value,
    })
}

/// Takes the first `len` bytes of `input`, or fails with `Malformed(label)`.
fn take<'a>(input: &mut &'a [u8], len: usize, label: &'static str) -> Result<&'a [u8], TdsError> {
    let (head, tail) = input
        .split_at_checked(len)
        .ok_or(TdsError::Malformed(label))?;
    *input = tail;
    Ok(head)
}

/// Reads one byte.
fn read_u8(input: &mut &[u8], label: &'static str) -> Result<u8, TdsError> {
    Ok(take(input, 1, label)?[0])
}

/// Reads a little-endian `u16`.
fn read_u16_le(input: &mut &[u8], label: &'static str) -> Result<u16, TdsError> {
    let bytes = take(input, 2, label)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Reads `units` UTF-16LE code units as a `String`. An invalid surrogate pair is
/// replaced by U+FFFD, as `batch.rs` does for the text of a SQL_BATCH.
fn read_utf16le(input: &mut &[u8], units: usize, label: &'static str) -> Result<String, TdsError> {
    let bytes = take(input, units * 2, label)?;
    let (pairs, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    Ok(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use vauban_types::{Len, SqlString};

    use super::*;

    /// The 22-byte minimal ALL_HEADERS block: Transaction Descriptor 0, one request.
    const HEADERS: [u8; 22] = [
        0x16, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
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
}
