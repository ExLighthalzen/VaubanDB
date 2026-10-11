use super::*;
use crate::builtins::check_call;
use crate::context::StaticContext;
use crate::{lookup, register_builtins};

/// The seven definitions this file registers, in registration order.
const DEFS: [&FunctionDef; 7] = [
    &SUSER_SID_DEF,
    &SID_BINARY_DEF,
    &SERVICE_NAME_DEF,
    &MAX_PRECISION_DEF,
    &FILE_PROPERTY_DEF,
    &FULLTEXT_SERVICE_PROPERTY_DEF,
    &FULLTEXT_CATALOG_PROPERTY_DEF,
];

fn ty(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, true)
}

fn nvarchar() -> TypeInfo {
    ty(SqlType::NVarChar(Len::Fixed(128)))
}

fn text(s: &str) -> Value {
    Value::String(SqlString { text: s.to_owned() })
}

/// A context that knows a session login, three principals, two files and a service name.
struct ServerContext {
    login: Option<&'static str>,
}

impl EvalContext for ServerContext {
    fn now_local(&self) -> vauban_types::DateTime2 {
        StaticContext::default().now
    }

    fn now_utc(&self) -> vauban_types::DateTime2 {
        self.now_local()
    }

    fn rowcount(&self) -> i64 {
        0
    }

    fn last_identity(&self) -> Option<vauban_types::Decimal> {
        None
    }

    fn spid(&self) -> i16 {
        0
    }

    fn current_database(&self) -> &str {
        "master"
    }

    fn server_name(&self) -> &str {
        "VAUBAN"
    }

    fn object_id(&self, _name: &str) -> Option<i32> {
        None
    }

    fn object_name(&self, _id: i32) -> Option<String> {
        None
    }

    fn variable(&self, _name: &str) -> Option<Value> {
        None
    }

    fn login_name(&self) -> Option<&str> {
        self.login
    }

    fn server_principal_sid(&self, name: &str) -> Option<Vec<u8>> {
        [("sa", 1u8), ("public", 2), ("sysadmin", 3)]
            .iter()
            .find(|(known, _)| same_name(known, name))
            .map(|(_, sid)| vec![*sid])
    }

    fn database_file(&self, name: &str) -> Option<(i32, u8, bool)> {
        [("master_data", 1, 0u8), ("master_log", 2, 1)]
            .iter()
            .find(|(known, _, _)| same_name(known, name))
            .map(|(_, id, kind)| (*id, *kind, false))
    }

    fn service_name(&self) -> Option<&str> {
        Some("service")
    }
}

const SESSION: ServerContext = ServerContext { login: Some("sa") };

/// Evaluates the registered function `name` on `values` typed `types`, through `check_call`.
fn call_typed(
    name: &str,
    values: &[Value],
    types: &[TypeInfo],
    ctx: &dyn EvalContext,
) -> SqlResult<Value> {
    register_builtins();
    let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
    let result = check_call(def, types)?;
    let args = EvalArgs {
        values,
        types,
        result: &result,
    };
    (def.eval)(&args, ctx)
}

/// Evaluates `name` on string arguments typed `nvarchar(128)`.
fn call_text(name: &str, values: &[&str], ctx: &dyn EvalContext) -> Value {
    let values: Vec<Value> = values.iter().map(|v| text(v)).collect();
    let types: Vec<TypeInfo> = values.iter().map(|_| nvarchar()).collect();
    call_typed(name, &values, &types, ctx).expect("the call must succeed")
}

fn sid(text_form: &str) -> Value {
    call_text("SID_BINARY", &[text_form], &SESSION)
}

fn bytes(hex: &str) -> Value {
    let digits: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    Value::Bytes(digits)
}

#[test]
fn server_info_functions_are_registered() {
    register_builtins();
    let expected: [(&str, Arity, SqlType, bool); 7] = [
        (
            "SUSER_SID",
            Arity::Range(0, 2),
            SqlType::VarBinary(Len::Fixed(85)),
            true,
        ),
        (
            "SID_BINARY",
            Arity::Exact(1),
            SqlType::VarBinary(Len::Fixed(85)),
            true,
        ),
        (
            "@@SERVICENAME",
            Arity::Exact(0),
            SqlType::NVarChar(Len::Fixed(128)),
            true,
        ),
        ("@@MAX_PRECISION", Arity::Exact(0), SqlType::TinyInt, false),
        ("FILEPROPERTY", Arity::Exact(2), SqlType::Int, true),
        (
            "FULLTEXTSERVICEPROPERTY",
            Arity::Exact(1),
            SqlType::Int,
            true,
        ),
        (
            "FULLTEXTCATALOGPROPERTY",
            Arity::Exact(2),
            SqlType::Int,
            true,
        ),
    ];
    for ((name, arity, result, nullable), def) in expected.iter().zip(DEFS) {
        assert_eq!(def.name, *name);
        let found = lookup(&name.to_ascii_lowercase()).expect("registered");
        assert_eq!(found.name, *name);
        assert_eq!(found.arity, *arity, "{name}");
        assert_eq!(found.kind, FunctionKind::Scalar, "{name}");
        assert!(!found.deterministic, "{name}");
        assert!(found.aggregate.is_none(), "{name}");
        let args: Vec<TypeInfo> = match arity {
            Arity::Exact(n) | Arity::Range(_, n) | Arity::Variadic(n) => {
                (0..*n).map(|_| nvarchar()).collect()
            }
        };
        let args: Vec<TypeInfo> = if *name == "SUSER_SID" {
            vec![nvarchar(), ty(SqlType::Int)]
        } else {
            args
        };
        let typed = check_call(found, &args).expect("accepted");
        assert_eq!(typed.ty, *result, "{name}");
        assert_eq!(typed.nullable, *nullable, "{name}");
    }

    let suser_sid = lookup("SUSER_SID").expect("registered");
    let error = check_call(suser_sid, &[nvarchar(), ty(SqlType::Int), ty(SqlType::Int)])
        .expect_err("three arguments");
    assert_eq!(error.number, 189);
    let sid_binary = lookup("SID_BINARY").expect("registered");
    let error = check_call(sid_binary, &[]).expect_err("no argument");
    assert_eq!(error.number, 174);
    for (name, count) in [
        ("FILEPROPERTY", 1),
        ("FULLTEXTCATALOGPROPERTY", 1),
        ("FULLTEXTSERVICEPROPERTY", 0),
    ] {
        let def = lookup(name).expect("registered");
        let args: Vec<TypeInfo> = (0..count).map(|_| nvarchar()).collect();
        assert_eq!(
            check_call(def, &args).expect_err(name).number,
            174,
            "{name}"
        );
    }
}

#[test]
fn sid_binary_encodes_the_numeric_form() {
    let cases = [
        ("S-1-5-18", "010100000000000512000000"),
        ("s-1-5-18", "010100000000000512000000"),
        ("S-1-5-32-544", "01020000000000052000000020020000"),
        ("S-1-1-0", "010100000000000100000000"),
        (
            "S-1-5-21-1-2-3-500",
            "010500000000000515000000010000000200000003000000F4010000",
        ),
        ("S-1-281474976710655-1", "0101FFFFFFFFFFFF01000000"),
        ("S-1-0xFFFFFFFFFFFF-1", "0101FFFFFFFFFFFF01000000"),
        ("S-1-5-4294967295", "0101000000000005FFFFFFFF"),
        ("S-1-5-4294967296", "0101000000000005FFFFFFFF"),
        ("S-1-5-18446744073709551616", "0101000000000005FFFFFFFF"),
        ("S-1-5-0xFFFFFFFFF", "0101000000000005FFFFFFFF"),
        ("S-1-5-0x12", "010100000000000512000000"),
        ("S-1-5-0X12", "010100000000000512000000"),
        ("S-1-5-0x0000000000012", "010100000000000512000000"),
        ("S-01-5-18", "010100000000000512000000"),
        ("S-1-05-018", "010100000000000512000000"),
        ("S-1-0X12-18", "010100000000001212000000"),
        ("S-1-5-0x12-0x13", "01020000000000051200000013000000"),
    ];
    for (input, hex) in cases {
        assert_eq!(sid(input), bytes(hex), "{input}");
    }

    let fifteen = format!("S-1-5{}", "-7".repeat(15));
    let Value::Bytes(encoded) = sid(&fifteen) else {
        panic!("fifteen sub-authorities encode");
    };
    assert_eq!(encoded.len(), 68);
    assert_eq!(encoded[1], 15);
    let sixteen = format!("S-1-5{}", "-7".repeat(16));
    assert_eq!(sid(&sixteen), Value::Null);

    for refused in [
        "S-1-5",
        "S-1",
        "S-2-5-18",
        "S-1-5-18 ",
        " S-1-5-18",
        "S-1-5--18",
        "S-1-5-+18",
        "S-1-5-18-",
        "S-1-5-1.8",
        "S-1-5-18a",
        "S-1-5-0x",
        "S-1-5-0xg",
        "S-1-5-00x12",
        "S-1-0x-18",
        "S-1-281474976710656-1",
        "S-1-0x1000000000000-1",
        "sa",
        "",
        "BA",
        "SY",
    ] {
        assert_eq!(sid(refused), Value::Null, "{refused:?}");
    }
}

#[test]
fn sid_binary_types_its_argument() {
    let refused = [
        SqlType::Int,
        SqlType::Bit,
        SqlType::Float,
        SqlType::Date,
        SqlType::UniqueIdentifier,
    ];
    for t in refused {
        let error =
            call_typed("SID_BINARY", &[Value::Null], &[ty(t)], &SESSION).expect_err("refused");
        assert_eq!(
            (error.number, error.severity, error.state),
            (8116, 16, 1),
            "{t:?}"
        );
    }
    // A binary argument reads as the UTF-16 text it holds.
    let utf16: Vec<u8> = "S-1-5-18"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let binary = call_typed(
        "SID_BINARY",
        &[Value::Bytes(utf16)],
        &[ty(SqlType::VarBinary(Len::Fixed(40)))],
        &SESSION,
    )
    .expect("accepted");
    assert_eq!(binary, bytes("010100000000000512000000"));
    let null = call_typed(
        "SID_BINARY",
        &[Value::Null],
        &[ty(SqlType::VarChar(Len::Fixed(10)))],
        &SESSION,
    )
    .expect("accepted");
    assert_eq!(null, Value::Null);
}

#[test]
fn suser_sid_reads_the_context() {
    let cases = [
        ("sa", Value::Bytes(vec![1])),
        ("SA", Value::Bytes(vec![1])),
        ("sa ", Value::Bytes(vec![1])),
        ("", Value::Bytes(vec![1])),
        ("sysadmin", Value::Bytes(vec![3])),
        ("public", Value::Bytes(vec![2])),
        (" sa", Value::Null),
        (" ", Value::Null),
        ("nosuch", Value::Null),
    ];
    for (name, expected) in cases {
        assert_eq!(
            call_text("SUSER_SID", &[name], &SESSION),
            expected,
            "{name:?}"
        );
    }
    assert_eq!(
        call_typed("SUSER_SID", &[], &[], &SESSION).expect("no argument"),
        Value::Bytes(vec![1])
    );
    let int = ty(SqlType::Int);
    assert_eq!(
        call_typed(
            "SUSER_SID",
            &[Value::Null],
            std::slice::from_ref(&int),
            &SESSION
        )
        .expect("NULL"),
        Value::Null
    );
    assert_eq!(
        call_typed(
            "SUSER_SID",
            &[Value::I32(1)],
            std::slice::from_ref(&int),
            &SESSION
        )
        .expect("1"),
        Value::Null
    );
    for second in [Value::I32(0), Value::I32(2), Value::Null] {
        assert_eq!(
            call_typed(
                "SUSER_SID",
                &[text("sa"), second.clone()],
                &[nvarchar(), int.clone()],
                &SESSION
            )
            .expect("int second argument"),
            Value::Bytes(vec![1]),
            "{second:?}"
        );
    }
    // A login the context does not publish, and no session at all.
    let bob = ServerContext { login: Some("bob") };
    assert_eq!(call_text("SUSER_SID", &[""], &bob), Value::Null);
    let nobody = ServerContext { login: None };
    assert_eq!(
        call_typed("SUSER_SID", &[], &[], &nobody).expect("no session"),
        Value::Null
    );
    assert_eq!(
        call_typed("SUSER_SID", &[], &[], &StaticContext::default()).expect("static"),
        Value::Null
    );
}

#[test]
fn suser_sid_types_its_second_argument() {
    for t in [
        SqlType::NVarChar(Len::Fixed(1)),
        SqlType::VarChar(Len::Fixed(4)),
        SqlType::Decimal {
            precision: 2,
            scale: 1,
        },
        SqlType::BigInt,
        SqlType::Bit,
    ] {
        let error = call_typed(
            "SUSER_SID",
            &[text("sa"), Value::Null],
            &[nvarchar(), ty(t)],
            &SESSION,
        )
        .expect_err("refused");
        assert_eq!(
            (error.number, error.severity, error.state),
            (8116, 16, 1),
            "{t:?}"
        );
    }
}

#[test]
fn service_name_and_max_precision() {
    assert_eq!(
        call_typed("@@SERVICENAME", &[], &[], &SESSION).expect("service"),
        text("service")
    );
    assert_eq!(
        call_typed("@@SERVICENAME", &[], &[], &StaticContext::default()).expect("static"),
        Value::Null
    );
    assert_eq!(
        call_typed("@@max_precision", &[], &[], &StaticContext::default()).expect("38"),
        Value::I8(38)
    );
}

#[test]
fn file_property_reads_the_file_facts() {
    let cases = [
        ("master_data", "IsLogFile", Value::I32(0)),
        ("master_data", "IsPrimaryFile", Value::I32(1)),
        ("master_data", "IsReadOnly", Value::I32(0)),
        ("master_data", "SpaceUsed", Value::I32(0)),
        ("master_log", "IsLogFile", Value::I32(1)),
        ("master_log", "IsPrimaryFile", Value::I32(0)),
        ("MASTER_LOG", "islogfile ", Value::I32(1)),
        ("master_log ", "ISLOGFILE", Value::I32(1)),
        (" master_log", "IsLogFile", Value::Null),
        ("master_data", " IsLogFile", Value::Null),
        ("master_data", "Nope", Value::Null),
        ("nosuch", "SpaceUsed", Value::Null),
    ];
    for (file, property, expected) in cases {
        assert_eq!(
            call_text("FILEPROPERTY", &[file, property], &SESSION),
            expected,
            "{file:?} {property:?}"
        );
    }
    let int = ty(SqlType::Int);
    for (values, types) in [
        (
            vec![Value::Null, text("SpaceUsed")],
            vec![int.clone(), nvarchar()],
        ),
        (
            vec![text("master_data"), Value::Null],
            vec![nvarchar(), int.clone()],
        ),
        (
            vec![Value::I32(1), text("SpaceUsed")],
            vec![int.clone(), nvarchar()],
        ),
        (
            vec![text("master_data"), Value::I32(1)],
            vec![nvarchar(), int.clone()],
        ),
    ] {
        assert_eq!(
            call_typed("FILEPROPERTY", &values, &types, &SESSION).expect("accepted"),
            Value::Null,
            "{values:?}"
        );
    }
    assert_eq!(
        call_text(
            "FILEPROPERTY",
            &["master_data", "IsLogFile"],
            &StaticContext::default()
        ),
        Value::Null
    );
}

#[test]
fn full_text_properties() {
    for name in [
        "IsFullTextInstalled",
        "isfulltextinstalled",
        "ISFULLTEXTINSTALLED ",
    ] {
        assert_eq!(
            call_text("FULLTEXTSERVICEPROPERTY", &[name], &SESSION),
            Value::I32(0),
            "{name:?}"
        );
    }
    for name in [
        " IsFullTextInstalled",
        "ResourceUsage",
        "ConnectTimeout",
        "LoadOSResources",
        "VerifySignature",
        "DataTimeout",
        "UpgradeOption",
        "nosuch",
    ] {
        assert_eq!(
            call_text("FULLTEXTSERVICEPROPERTY", &[name], &SESSION),
            Value::Null,
            "{name:?}"
        );
    }
    let int = ty(SqlType::Int);
    assert_eq!(
        call_typed(
            "FULLTEXTSERVICEPROPERTY",
            &[Value::Null],
            std::slice::from_ref(&int),
            &SESSION
        )
        .expect("NULL"),
        Value::Null
    );
    for property in ["ItemCount", "PopulateStatus", "nosuch"] {
        assert_eq!(
            call_text("FULLTEXTCATALOGPROPERTY", &["x", property], &SESSION),
            Value::Null,
            "{property}"
        );
    }
}
