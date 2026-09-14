//! Session variables (`@@SPID`, `@@ROWCOUNT`, `@@ERROR`, `@@TRANCOUNT`, `@@SERVERNAME`),
//! `DB_NAME()` and `NEWID()`: the built-ins whose answer comes from the session rather
//! than from their arguments.
//!
//! The functions here read the [`EvalContext`] and nothing else: no catalogue, no
//! conversion. Not one of them is deterministic (Microsoft Learn, "Deterministic and
//! nondeterministic functions"), and not one is an aggregate
//! (`newid_return_type`, `metadata_functions_are_registered`,
//! `niladic_session_functions_return_sysname`,
//! `transaction_state_functions_are_scalar_and_not_deterministic`).
//!
//! `@@VERSION` and `SERVERPROPERTY` look like they belong here but are registered by
//! `vauban-compat`; registering them a second time panics.
//!
//! `SQL_VARIANT_PROPERTY` and `COLLATIONPROPERTY` sit at the end of the file. They answer
//! from the session no more than from a table: they describe the *type* of a value, which
//! no client can read from the TDS metadata.
//!
//! The four niladic functions that read the session, `CURRENT_USER`, `SESSION_USER`,
//! `SYSTEM_USER` and `USER`, are written **without parentheses**. They belong here for the
//! same reason as `@@SPID`: their answer comes from the session. The fifth niladic name,
//! `CURRENT_TIMESTAMP`, is a clock and belongs to `datetime_clock.rs`.
//!
//! `XACT_STATE()`, `@@LOCK_TIMEOUT` and `@@TRANCOUNT` describe the transaction of the
//! session and read their own methods of the context.

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_errors::{SqlError, SqlResult};
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// Result type of `@@SPID`: `smallint`, never `NULL`.
///
/// Microsoft Learn, "@@SPID (Transact-SQL)": the return type is `smallint`, which is also
/// what a client reports for `SELECT @@SPID`.
fn smallint_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::SmallInt, false))
}

/// Result type of `@@ROWCOUNT`, `@@ERROR`, `@@TRANCOUNT` and `@@LOCK_TIMEOUT`: `int`,
/// not nullable.
///
/// `@@LOCK_TIMEOUT` and `@@TRANCOUNT` are `int` and not nullable, where `XACT_STATE()` is
/// nullable; see [`xact_state_type`].
fn int_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, false))
}

/// Result type of `@@SERVERNAME` and `DB_NAME()`: `nvarchar(128)`, nullable.
///
/// 128 is the length of `sysname`, the type of both.
fn name_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

/// Result type of `NEWID()`: `uniqueidentifier`, never `NULL`.
fn guid_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::UniqueIdentifier, false))
}

/// Evaluates `@@SPID`: the identifier of the current session.
fn spid_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::I16(ctx.spid()))
}

/// Evaluates `@@ROWCOUNT`: the number of rows the last statement affected.
///
/// The session counts rows in an `i64` while `@@ROWCOUNT` is an `int`, so the count is
/// **saturated** to `i32::MAX` (and to `i32::MIN`, which no count reaches). This is a
/// VaubanDB choice, not SQL Server's: SQL Server documents `ROWCOUNT_BIG()`, a `bigint`
/// out of the V1 scope, for counts above 2^31-1 rather than saturating.
fn rowcount_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let count = ctx.rowcount();
    let saturated = i32::try_from(count).unwrap_or(if count < 0 { i32::MIN } else { i32::MAX });
    Ok(Value::I32(saturated))
}

/// The `int` value of a session variable, `0` when the session does not know it.
///
/// [`EvalContext::variable`] may answer any [`Value`] variant; `@@ERROR` is an `int` that
/// does not answer `NULL`, so an unexpected variant falls back to `0` instead of raising
/// an error, which would turn a session bug into a query failure
/// (`error_defaults_to_zero`).
fn int_variable(ctx: &dyn EvalContext, name: &str) -> Value {
    match ctx.variable(name) {
        Some(Value::I32(n)) => Value::I32(n),
        _ => Value::I32(0),
    }
}

/// Evaluates `@@ERROR`: the error number of the last statement, `0` when it succeeded.
fn error_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(int_variable(ctx, "@@ERROR"))
}

/// Evaluates `@@TRANCOUNT`: the number of open transactions in the session.
///
/// Reads [`EvalContext::trancount`] and not [`EvalContext::variable`]`("@@TRANCOUNT")`.
/// The method is typed `i32`, so the `Value` variant does not depend on what the session
/// put in the map (`trancount_reads_its_own_method`).
fn trancount_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::I32(ctx.trancount()))
}

/// Builds a `Value::String` from `text`.
fn string(text: &str) -> Value {
    Value::String(SqlString {
        text: text.to_owned(),
    })
}

/// Evaluates `@@SERVERNAME`: the name of the server instance.
fn server_name_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(string(ctx.server_name()))
}

/// Evaluates `DB_NAME()` and `DB_NAME(id)`.
///
/// Without an argument the current database is returned. With one, the database with this
/// identifier, or `NULL` when no database has it, which is also what SQL Server answers
/// for an unknown identifier, and what the default [`EvalContext::database_name`]
/// answers without a catalogue. A `NULL` argument gives `NULL`.
///
/// The argument is declared `int`; the `binder` converts it. A value of any other kind
/// yields `NULL` rather than an error: the conversion, and the error it may raise, belong
/// to `binder` and `types`, not here.
fn db_name_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(argument) = args.values.first() else {
        return Ok(string(ctx.current_database()));
    };
    let id = match *argument {
        Value::I8(n) => Some(i32::from(n)),
        Value::I16(n) => Some(i32::from(n)),
        Value::I32(n) => Some(n),
        Value::I64(n) => i32::try_from(n).ok(),
        _ => None,
    };
    Ok(match id.and_then(|id| ctx.database_name(id)) {
        Some(name) => string(&name),
        None => Value::Null,
    })
}

/// Evaluates `NEWID()`: a fresh random GUID.
fn newid_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::Guid(new_guid()))
}

/// Odd 64-bit constant of SplitMix64, the golden ratio scaled to 2^64.
const SPLITMIX_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

thread_local! {
    /// State of the per-thread generator, seeded once on first use.
    ///
    /// One state per thread: `NEWID()` runs on the blocking pool, and a shared state would
    /// need a lock for nothing. The seeds of two threads differ because
    /// [`RandomState`] draws fresh keys per instance and the thread identifier is mixed in.
    static RANDOM_STATE: Cell<u64> = Cell::new(seed());
}

/// Draws the initial state of the thread's generator from the system.
///
/// [`RandomState`] takes its entropy from the operating system; hashing the current time
/// and the thread identifier with it gives a value that differs between runs, between
/// threads, and between two processes started in the same instant.
fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut hasher = RandomState::new().build_hasher();
    nanos.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    hasher.finish()
}

/// Advances the thread's generator and returns 64 pseudo-random bits.
///
/// SplitMix64, written here because no random number generator is in the dependency
/// allow-list of the root `Cargo.toml`. It is **not** cryptographic: `NEWID()` needs
/// values that do not repeat, not values nobody can predict.
fn next_u64() -> u64 {
    RANDOM_STATE.with(|state| {
        let mut z = state.get().wrapping_add(SPLITMIX_GAMMA);
        state.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// Builds a random version 4 GUID, in the byte order SQL Server stores it.
///
/// The 122 random bits, the four version bits (`0100`) and the two variant bits (`10`) are
/// laid out as RFC 4122 §4.4 describes, then the first three groups are reversed because
/// [`Value::Guid`] holds them little-endian. The version nibble,
/// byte 6 of the RFC layout, therefore ends up in byte 7 of the returned array, and the
/// variant byte, byte 8, does not move.
fn new_guid() -> [u8; 16] {
    let mut rfc = [0u8; 16];
    rfc[..8].copy_from_slice(&next_u64().to_le_bytes());
    rfc[8..].copy_from_slice(&next_u64().to_le_bytes());
    rfc[6] = (rfc[6] & 0x0F) | 0x40;
    rfc[8] = (rfc[8] & 0x3F) | 0x80;
    to_storage_order(rfc)
}

/// Swaps the three first groups of a GUID between the RFC 4122 order and SQL Server's
/// storage order. The transformation is its own inverse, which the tests use to read the
/// version and variant bits back.
fn to_storage_order(bytes: [u8; 16]) -> [u8; 16] {
    let mut swapped = bytes;
    swapped[0..4].reverse();
    swapped[4..6].reverse();
    swapped[6..8].reverse();
    swapped
}

/// `@@SPID`: Microsoft Learn, "@@SPID (Transact-SQL)".
const SPID_DEF: FunctionDef = FunctionDef {
    name: "@@SPID",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: smallint_type,
    eval: spid_eval,
    aggregate: None,
};

/// `@@ROWCOUNT`: Microsoft Learn, "@@ROWCOUNT (Transact-SQL)". Updated by each statement,
/// hence never deterministic.
const ROWCOUNT_DEF: FunctionDef = FunctionDef {
    name: "@@ROWCOUNT",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: int_type,
    eval: rowcount_eval,
    aggregate: None,
};

/// `@@ERROR`: Microsoft Learn, "@@ERROR (Transact-SQL)".
const ERROR_DEF: FunctionDef = FunctionDef {
    name: "@@ERROR",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: int_type,
    eval: error_eval,
    aggregate: None,
};

/// `@@TRANCOUNT`: Microsoft Learn, "@@TRANCOUNT (Transact-SQL)".
const TRANCOUNT_DEF: FunctionDef = FunctionDef {
    name: "@@TRANCOUNT",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: int_type,
    eval: trancount_eval,
    aggregate: None,
};

/// `@@SERVERNAME`: Microsoft Learn, "@@SERVERNAME (Transact-SQL)".
const SERVER_NAME_DEF: FunctionDef = FunctionDef {
    name: "@@SERVERNAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: server_name_eval,
    aggregate: None,
};

/// `DB_NAME`: Microsoft Learn, "DB_NAME (Transact-SQL)". Zero or one argument.
const DB_NAME_DEF: FunctionDef = FunctionDef {
    name: "DB_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: name_type,
    eval: db_name_eval,
    aggregate: None,
};

/// `NEWID`: Microsoft Learn, "NEWID (Transact-SQL)"; RFC 4122 §4.4 for the layout.
const NEWID_DEF: FunctionDef = FunctionDef {
    name: "NEWID",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: guid_type,
    eval: newid_eval,
    aggregate: None,
};

// --- Metadata of a *value* ---------------------------------------------------------------
//
// `SQL_VARIANT_PROPERTY` and `COLLATIONPROPERTY` are the way a client reads the precision,
// the scale or the collation of an expression: the TDS metadata a driver reports carries
// not one of them. Both answer a `sql_variant` on SQL Server; VaubanDB has no `sql_variant`
// type, so each returns the type the variant *holds*, which a typical use of them casts
// anyway. See the two `*_property_type` functions for the difference.

/// `fIgnoreCase` of the TDS `Collation` rule, `[MS-TDS]` 2.2.5.1.2. The four sensitivity
/// bits and the two binary bits are private to `vauban_types::collation`; they are
/// repeated here — and only here — because `COLLATIONPROPERTY` reports them as one Windows
/// `ComparisonStyle` mask, which is a property of the function and not of the type system.
const FLAG_IGNORE_CASE: u8 = 0x01;
/// `fIgnoreAccent`, `[MS-TDS]` 2.2.5.1.2.
const FLAG_IGNORE_ACCENT: u8 = 0x02;
/// `fIgnoreKana`, `[MS-TDS]` 2.2.5.1.2.
const FLAG_IGNORE_KANA: u8 = 0x04;
/// `fIgnoreWidth`, `[MS-TDS]` 2.2.5.1.2.
const FLAG_IGNORE_WIDTH: u8 = 0x08;
/// `fBinary` (`_BIN`), `[MS-TDS]` 2.2.5.1.2.
const FLAG_BINARY: u8 = 0x10;
/// `fBinary2` (`_BIN2`), `[MS-TDS]` 2.2.5.1.2.
const FLAG_BINARY2: u8 = 0x20;
/// `fUTF8` (`_UTF8`), `[MS-TDS]` 2.2.5.1.2.
const FLAG_UTF8: u8 = 0x40;

/// `NORM_IGNORECASE` of the Windows collation flags, as `COLLATIONPROPERTY(...,
/// 'ComparisonStyle')` reports them.
const STYLE_IGNORE_CASE: i32 = 0x0000_0001;
/// `NORM_IGNORENONSPACE` (accents).
const STYLE_IGNORE_ACCENT: i32 = 0x0000_0002;
/// `NORM_IGNOREKANATYPE`.
const STYLE_IGNORE_KANA: i32 = 0x0001_0000;
/// `NORM_IGNOREWIDTH`.
const STYLE_IGNORE_WIDTH: i32 = 0x0002_0000;

/// Code page of every collation this engine knows but the `_UTF8` ones: 1252, the page of
/// `CP1` and of `Latin1_General`.
const CODE_PAGE_LATIN1: i32 = 1252;
/// Code page of a `_UTF8` collation, `COLLATIONPROPERTY('Latin1_General_100_CI_AS_SC_UTF8',
/// 'CodePage')` → 65001.
const CODE_PAGE_UTF8: i32 = 65001;

/// Result type of `SQL_VARIANT_PROPERTY`: `nvarchar(128)`, nullable.
///
/// SQL Server answers a **`sql_variant`**, which is not a VaubanDB type. What the variant
/// holds depends on the property, as the function says of its own result:
/// `SQL_VARIANT_PROPERTY(SQL_VARIANT_PROPERTY(1, 'BaseType'), 'BaseType')` is `nvarchar`
/// with `MaxLength` 256 and the instance collation, while the same question on
/// `'Precision'` gives `int` with `MaxLength` 4. `nvarchar(128)` is therefore the exact
/// type of the two textual properties, and the numeric ones are rendered into it.
///
/// **Deliberate difference from SQL Server.** A **nested** call shows the wrapper:
/// `SELECT CAST(SQL_VARIANT_PROPERTY(SQL_VARIANT_PROPERTY(CAST(1 AS int), 'Precision'),
/// 'BaseType') AS varchar(30))` with its `MaxLength` answers `int` and `4` on SQL Server,
/// `nvarchar` and `256` here, because the inner call really returns an `int` there and an
/// `nvarchar(128)` here. Three errors SQL Server raises are not raised here: `... + 1` is
/// 257 state 3 there and 245 state 1 here, `... + 'x'` is 402 there and the `nvarchar`
/// `'intx'` here, `LEN(...)` is 8116 there and the `int` 3 here.
///
/// # Errors
///
/// 206, severity 16, state 2, for a `(max)` argument: `SELECT
/// SQL_VARIANT_PROPERTY(CAST('ab' AS varchar(max)), 'BaseType');` is a type clash between
/// `varchar(max)` and `sql_variant` (`variant_property_refuses_the_max_types`): a
/// `sql_variant` cannot hold a large-value type.
fn variant_property_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    if let Some(info) = args.first()
        && is_large_value_type(&info.ty)
    {
        return Err(SqlError::operand_type_clash(
            &info.ty.declaration(),
            "sql_variant",
        ));
    }
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

/// Whether `ty` is one of the `(max)` types a `sql_variant` cannot hold.
fn is_large_value_type(ty: &SqlType) -> bool {
    matches!(
        ty,
        SqlType::VarChar(Len::Max) | SqlType::NVarChar(Len::Max) | SqlType::VarBinary(Len::Max)
    )
}

/// Result type of `COLLATIONPROPERTY`: `int`, always nullable.
///
/// This function answers a **`sql_variant`** too, exactly like `SQL_VARIANT_PROPERTY`; the
/// five properties it reports are integers, so `int` is what the variant holds in each
/// case and no rendering is needed. The wrapper is visible nonetheless, and the same
/// deliberate difference applies: `SELECT
/// LEN(COLLATIONPROPERTY('SQL_Latin1_General_CP1_CI_AS', 'CodePage'));` raises 8116 on
/// SQL Server, where VaubanDB raises nothing and answers the `int` 4.
fn collation_property_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// The property name an argument denotes, folded for comparison, `None` when the argument
/// is not one.
///
/// The name is compared **without case** (`'basetype'` and `'BASETYPE'` both answer
/// `int`) and
/// **without its trailing blanks** (`'BaseType '` answers `int`, `' BaseType'` answers
/// `NULL`): the ordinary comparison rules of a T-SQL string, which pad the shorter operand
/// on the right. An argument that is not a string is not a name either:
/// `SQL_VARIANT_PROPERTY(1, 1)` answers `NULL` and raises nothing.
fn property_name(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.text.trim_end_matches(' ').to_ascii_lowercase()),
        _ => None,
    }
}

/// Evaluates `SQL_VARIANT_PROPERTY(expression, property)`.
///
/// The answer is read from the **declared type** of the expression (`args.types[0]`) for
/// every property but `TotalBytes` and the `MaxLength` of a `decimal`, which depend on the
/// value itself. A `NULL` expression answers `NULL` for **every** property, and the
/// declared type does not save it: `SQL_VARIANT_PROPERTY(CAST(NULL AS int), 'BaseType')` is
/// `NULL`, not `int`. So is an unknown property, an unknown collation, and a property
/// argument that is not a string — this function raises nothing but the 174 of its arity
/// and the 206 of a `(max)` argument.
fn variant_property_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let (Some(value), Some(info)) = (args.values.first(), args.types.first()) else {
        return Ok(Value::Null);
    };
    let Some(property) = property_name(args.values.get(1)) else {
        return Ok(Value::Null);
    };
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let answer = match property.as_str() {
        "basetype" => Some(info.ty.name().to_owned()),
        "precision" => Some(variant_precision(&info.ty).to_string()),
        "scale" => Some(variant_scale(&info.ty).to_string()),
        "totalbytes" => Some(variant_total_bytes(&info.ty, value).to_string()),
        "collation" => variant_collation(info),
        "maxlength" => Some(variant_max_length(&info.ty, value).to_string()),
        _ => None,
    };
    Ok(match answer {
        Some(text) => string(&text),
        None => Value::Null,
    })
}

/// `Precision` of a type: the number of digits, or of characters for the date and time
/// types, that its widest value needs.
///
/// Zero for every type that has no notion of precision — the character, binary and
/// `uniqueidentifier` families. The date and time types count the characters of their
/// rendering: `date` 10 (`yyyy-mm-dd`), `datetime2(0)` 19, `datetimeoffset(0)` 26, plus
/// the decimal point and the fractional digits when the scale is not zero
/// (`datetime2(7)` → 27, `time(7)` → 16). `datetime` reports 23 and `smalldatetime` 16.
fn variant_precision(ty: &SqlType) -> u32 {
    match ty {
        SqlType::Bit => 1,
        SqlType::TinyInt => 3,
        SqlType::SmallInt => 5,
        SqlType::Int => 10,
        SqlType::BigInt | SqlType::Money => 19,
        SqlType::Decimal { precision, .. } | SqlType::Numeric { precision, .. } => {
            u32::from(*precision)
        }
        SqlType::Float => 53,
        SqlType::Real => 24,
        SqlType::SmallMoney => 10,
        SqlType::Date => 10,
        SqlType::DateTime => 23,
        SqlType::SmallDateTime => 16,
        SqlType::Time(scale) => 8 + fraction_digits(*scale),
        SqlType::DateTime2(scale) => 19 + fraction_digits(*scale),
        SqlType::DateTimeOffset(scale) => 26 + fraction_digits(*scale),
        SqlType::Char(_)
        | SqlType::VarChar(_)
        | SqlType::NChar(_)
        | SqlType::NVarChar(_)
        | SqlType::Binary(_)
        | SqlType::VarBinary(_)
        | SqlType::UniqueIdentifier => 0,
    }
}

/// Characters a fractional-seconds scale adds to a rendering: none at scale 0, the decimal
/// point plus the digits otherwise.
fn fraction_digits(scale: u8) -> u32 {
    if scale == 0 { 0 } else { u32::from(scale) + 1 }
}

/// `Scale` of a type: its number of fractional digits, `0` for everything that has none.
///
/// `money` and `smallmoney` report 4, `datetime` 3, and `time(s)`, `datetime2(s)`,
/// `datetimeoffset(s)` their own scale. `float`, `real` and `date` report 0.
fn variant_scale(ty: &SqlType) -> u32 {
    match ty {
        SqlType::Decimal { scale, .. } | SqlType::Numeric { scale, .. } => u32::from(*scale),
        SqlType::Money | SqlType::SmallMoney => 4,
        SqlType::DateTime => 3,
        SqlType::Time(scale) | SqlType::DateTime2(scale) | SqlType::DateTimeOffset(scale) => {
            u32::from(*scale)
        }
        _ => 0,
    }
}

/// `MaxLength` of a value: the bytes its type reserves, as `sys.columns.max_length` counts
/// them — the **declared** length for the character and binary types, doubled for the
/// national ones (`nvarchar(4000)` → 8000).
///
/// One exception, and it is the reason this function takes the value: a `decimal` reports
/// the bytes its **mantissa** needs, not the bytes its precision would reserve.
/// `CAST(1 AS decimal(38,0))` reports 5 and `CAST(99999999999999999999 AS decimal(38,0))`
/// reports 13, where a rule on the precision would report 17 for both.
fn variant_max_length(ty: &SqlType, value: &Value) -> i64 {
    match ty {
        SqlType::Bit | SqlType::TinyInt => 1,
        SqlType::SmallInt => 2,
        SqlType::Date => 3,
        SqlType::Int | SqlType::Real | SqlType::SmallMoney | SqlType::SmallDateTime => 4,
        SqlType::BigInt | SqlType::Float | SqlType::Money | SqlType::DateTime => 8,
        SqlType::UniqueIdentifier => 16,
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => mantissa_bytes(value),
        SqlType::Time(scale) => 3 + fraction_bytes(*scale),
        SqlType::DateTime2(scale) => 6 + fraction_bytes(*scale),
        SqlType::DateTimeOffset(scale) => 8 + fraction_bytes(*scale),
        SqlType::Char(len) | SqlType::VarChar(len) => declared_bytes(len, value),
        SqlType::NChar(len) | SqlType::NVarChar(len) => 2 * declared_bytes(len, value),
        SqlType::Binary(len) | SqlType::VarBinary(len) => declared_bytes(len, value),
    }
}

/// `TotalBytes` of a value: what the whole `sql_variant` cell occupies, header included.
///
/// The header is two bytes plus the properties the variant has to carry: none for a fixed
/// type (`int` → 4 + 2 = 6), one scale byte for `time`, `datetime2` and `datetimeoffset`
/// (`time(3)` → 4 + 3 = 7), a precision and a scale for `decimal` (mantissa + 4), two
/// length bytes for `binary` and `varbinary` (data + 4), and the same plus the collation
/// for the character types (data + 8).
///
/// The **data** is the value's own, not the declared length, for `varchar`, `nvarchar`,
/// `varbinary` and `decimal`: `CAST('abc' AS varchar(200))` totals 11 and `CAST('abc' AS
/// char(10))` totals 18, blank padding included. Trailing spaces count
/// (`CAST('abc   ' AS varchar(10))` totals 14), as they do for `DATALENGTH`.
fn variant_total_bytes(ty: &SqlType, value: &Value) -> i64 {
    match ty {
        SqlType::Decimal { .. } | SqlType::Numeric { .. } => 4 + mantissa_bytes(value),
        SqlType::Char(len) => 8 + declared_bytes(len, value),
        SqlType::VarChar(_) => 8 + character_count(value),
        SqlType::NChar(len) => 8 + 2 * declared_bytes(len, value),
        SqlType::NVarChar(_) => 8 + 2 * character_count(value),
        SqlType::Binary(len) => 4 + declared_bytes(len, value),
        SqlType::VarBinary(_) => 4 + byte_count(value),
        SqlType::Time(_) | SqlType::DateTime2(_) | SqlType::DateTimeOffset(_) => {
            3 + variant_max_length(ty, value)
        }
        _ => 2 + variant_max_length(ty, value),
    }
}

/// Bytes the mantissa of a `decimal` needs: one sign byte and as many 32-bit words as its
/// magnitude spans, which is 5, 9, 13 or 17.
///
/// `4294967295` reports 5 and `4294967296` reports 9; `99999999999999999999` (just past
/// 2^64) reports 13 and `10^38 - 1` reports 17
/// (`variant_property_decimal_lengths_follow_the_value`).
///
/// `pub(crate)` because `DATALENGTH` bills a `decimal` on the same steps and calls this
/// function instead of holding a second copy of the rule; its own vectors are in
/// `strings_core.rs`.
pub(crate) fn mantissa_bytes(value: &Value) -> i64 {
    let magnitude = match value {
        Value::Decimal(d) => d.mantissa.unsigned_abs(),
        _ => 0,
    };
    match magnitude {
        m if m >> 32 == 0 => 5,
        m if m >> 64 == 0 => 9,
        m if m >> 96 == 0 => 13,
        _ => 17,
    }
}

/// Bytes a fractional-seconds scale adds to a `time`, `datetime2` or `datetimeoffset`
/// (Microsoft Learn, "time (Transact-SQL)", section *Storage size*).
fn fraction_bytes(scale: u8) -> i64 {
    match scale {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    }
}

/// The declared length of a sized type. A `(max)` argument never reaches here — 206 is
/// raised while typing the call — so the value's own length is the safe fallback.
fn declared_bytes(len: &Len, value: &Value) -> i64 {
    match len {
        Len::Fixed(n) => i64::from(*n),
        Len::Max => match value {
            Value::Bytes(_) => byte_count(value),
            _ => character_count(value),
        },
    }
}

/// Characters of a string value, `0` for anything else.
fn character_count(value: &Value) -> i64 {
    match value {
        Value::String(s) => i64::try_from(s.text.chars().count()).unwrap_or(i64::MAX),
        _ => 0,
    }
}

/// Bytes of a binary value, `0` for anything else.
fn byte_count(value: &Value) -> i64 {
    match value {
        Value::Bytes(b) => i64::try_from(b.len()).unwrap_or(i64::MAX),
        _ => 0,
    }
}

/// `Collation` of a value: the name of its collation, `None` for every type that has none.
///
/// A non-character type (`int`, `uniqueidentifier`, `binary`, `date`) answers `NULL`,
/// which is exactly what [`TypeInfo::new`] expresses by leaving `collation` empty outside
/// [`SqlType::is_string`] (`variant_property_collation_names_the_character_types_only`).
fn variant_collation(info: &TypeInfo) -> Option<String> {
    if !info.ty.is_string() {
        return None;
    }
    info.collation.as_ref().map(collation_name)
}

/// The name of a collation, the spelling `SQL_VARIANT_PROPERTY(..., 'Collation')` prints.
///
/// The five wire bytes are turned back into a name: a non-zero `SortId` means a legacy
/// `SQL_` collation on code page 1, a version of 2 means the `_100_` family, the binary
/// collations name `_BIN` or `_BIN2` and no sensitivity at all, and the others spell
/// `_CI`/`_CS` then `_AI`/`_AS`, adding `_KS` and `_WS` for the bits they do *not* ignore.
///
/// `Collation::DEFAULT` therefore prints `SQL_Latin1_General_CP1_CI_AS`, which is what the
/// instance answers for any string expression.
///
/// Among the accepted names, the 16 linguistic `_SC_UTF8` names have a wire
/// representation of their own: the corresponding `_UTF8` names without `_SC` do not
/// exist, so `_SC` is restored for those version-100 names.
/// `Latin1_General_100_BIN2_UTF8` is the binary counterexample and has no `_SC`. The 16
/// non-UTF8 `_SC` names share their wire representation with a name without `_SC`; this
/// function keeps the latter spelling, a deliberate difference from SQL Server
/// (`collation_name_restores_the_sixteen_unique_linguistic_utf8_names`).
fn collation_name(collation: &Collation) -> String {
    let mut name = String::new();
    if collation.sort_id == 0 {
        name.push_str("Latin1_General");
        if collation.version == 2 {
            name.push_str("_100");
        }
    } else {
        name.push_str("SQL_Latin1_General_CP1");
    }
    let flags = collation.flags;
    if flags & FLAG_BINARY2 != 0 {
        name.push_str("_BIN2");
    } else if flags & FLAG_BINARY != 0 {
        name.push_str("_BIN");
    } else {
        name.push_str(if flags & FLAG_IGNORE_CASE == 0 {
            "_CS"
        } else {
            "_CI"
        });
        name.push_str(if flags & FLAG_IGNORE_ACCENT == 0 {
            "_AS"
        } else {
            "_AI"
        });
        if flags & FLAG_IGNORE_KANA == 0 {
            name.push_str("_KS");
        }
        if flags & FLAG_IGNORE_WIDTH == 0 {
            name.push_str("_WS");
        }
    }
    if flags & FLAG_UTF8 != 0 {
        if collation.sort_id == 0
            && collation.version == 2
            && flags & (FLAG_BINARY | FLAG_BINARY2) == 0
        {
            name.push_str("_SC");
        }
        name.push_str("_UTF8");
    }
    name
}

/// Evaluates `COLLATIONPROPERTY(collation, property)`.
///
/// `NULL` for an unknown collation, an unknown property, and a `NULL` on either side: this
/// function raises nothing. In particular a name the instance does not have is **not**
/// error 448 — `COLLATIONPROPERTY('Klingon_CI_AS', 'LCID')` answers `NULL` where `'a'
/// COLLATE Klingon_CI_AS` raises 448.
fn collation_property_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    let name = match args.values.first() {
        Some(Value::String(s)) => s.text.as_str(),
        _ => return Ok(Value::Null),
    };
    let Some(property) = property_name(args.values.get(1)) else {
        return Ok(Value::Null);
    };
    let Some(collation) = known_collation(name) else {
        return Ok(Value::Null);
    };
    Ok(match property.as_str() {
        "codepage" => Value::I32(if collation.flags & FLAG_UTF8 == 0 {
            CODE_PAGE_LATIN1
        } else {
            CODE_PAGE_UTF8
        }),
        "lcid" => Value::I32(i32::try_from(collation.lcid).unwrap_or(0)),
        "comparisonstyle" => Value::I32(comparison_style(&collation)),
        "version" => Value::I32(i32::from(collation.version)),
        "sortid" => Value::I32(i32::from(collation.sort_id)),
        _ => Value::Null,
    })
}

/// The collation a name denotes, `None` when the instance does not have it.
///
/// [`Collation::parse`] is the grammar of the names the server has, so no rejection rule
/// of this function's own is needed (`collation_property_knows_only_the_names_the_server_has`).
///
/// What remains is the trailing-blank rule of T-SQL string comparison: trailing blanks are
/// ignored, leading ones are not. `COLLATIONPROPERTY('SQL_Latin1_General_CP1_CI_AS ',
/// 'CodePage')` is 1252 and `COLLATIONPROPERTY(' SQL_Latin1_General_CP1_CI_AS',
/// 'CodePage')` is `NULL`.
fn known_collation(name: &str) -> Option<Collation> {
    Collation::parse(name.trim_end_matches(' ')).ok()
}

/// The Windows `ComparisonStyle` mask of a collation: one bit per comparison the collation
/// **ignores**, and `0` for a binary one, which ignores nothing.
///
/// `SQL_Latin1_General_CP1_CI_AS` reports 196609 (`0x30001`: case, kana type and width),
/// `_CS_AS` 196608, `_CI_AI` 196611, `Latin1_General_CI_AS_KS` 131073,
/// `Latin1_General_CI_AS_WS` 65537, `Latin1_General_CI_AS_KS_WS` 1,
/// `Latin1_General_CS_AS_KS_WS` 0, and both `Latin1_General_BIN` and `_BIN2` report 0.
fn comparison_style(collation: &Collation) -> i32 {
    let flags = collation.flags;
    if flags & (FLAG_BINARY | FLAG_BINARY2) != 0 {
        return 0;
    }
    let mut style = 0;
    for (flag, bit) in [
        (FLAG_IGNORE_CASE, STYLE_IGNORE_CASE),
        (FLAG_IGNORE_ACCENT, STYLE_IGNORE_ACCENT),
        (FLAG_IGNORE_KANA, STYLE_IGNORE_KANA),
        (FLAG_IGNORE_WIDTH, STYLE_IGNORE_WIDTH),
    ] {
        if flags & flag != 0 {
            style |= bit;
        }
    }
    style
}

/// `SQL_VARIANT_PROPERTY`: Microsoft Learn, "SQL_VARIANT_PROPERTY (Transact-SQL)".
/// Deterministic: the answer depends only on the type and the value of its argument.
const SQL_VARIANT_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "SQL_VARIANT_PROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: variant_property_type,
    eval: variant_property_eval,
    aggregate: None,
};

/// `COLLATIONPROPERTY`: Microsoft Learn, "COLLATIONPROPERTY (Transact-SQL)".
const COLLATION_PROPERTY_DEF: FunctionDef = FunctionDef {
    name: "COLLATIONPROPERTY",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(2),
    return_type: collation_property_type,
    eval: collation_property_eval,
    aggregate: None,
};

// --- The niladic functions of the session ------------------------------------------------
//
// `CURRENT_USER`, `SESSION_USER`, `SYSTEM_USER` and `USER` are **niladic**: they are
// written without parentheses, the parser hands them over as a one-part column reference
// and the `binder` recognises them before raising error 207 (`bind_niladic`). The
// registry sees four ordinary functions of arity zero. They read the session and nothing
// else, which is why they live in this file; `USER_NAME()` and `SUSER_SNAME()`, their
// parenthesised cousins, belong to `objects.rs`. The fifth niladic name,
// `CURRENT_TIMESTAMP`, is a clock and lives in `datetime_clock.rs`.
//
// The four are `nvarchar(128)`, that is `sysname`, which [`name_type`] already spells for
// `@@SERVERNAME` and `DB_NAME()`, the type of `USER_NAME()`, `SUSER_SNAME()` and
// `CAST('x' AS nvarchar(128))`. Nullable, unlike `CURRENT_TIMESTAMP`
// (`niladic_session_functions_return_sysname`).
//
// The **values**: `USER`, `CURRENT_USER` and `SESSION_USER` each answer `USER_NAME()`
// (`dbo`), and `SYSTEM_USER` answers `SUSER_SNAME()` (`sa`). Hence the two evaluations
// below, two for four names (`niladic_session_functions_read_the_context`).

/// Evaluates `CURRENT_USER`, `SESSION_USER` and `USER`: the database user of the session.
///
/// `NULL` when the session has no database user: SQL Server answers `dbo` for a login
/// mapped to the owner of the database, and a session without database principals has no
/// such value. Writing `dbo` here would be inventing a value the engine does not hold
/// (`niladic_session_functions_without_a_session_are_null`).
fn user_name_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(match ctx.user_name() {
        Some(name) => string(name),
        None => Value::Null,
    })
}

/// Evaluates `SYSTEM_USER`: the login of the connection, `NULL` when there is none.
///
/// The session does hold this one ([`EvalContext::login_name`] is the LOGIN7 user name),
/// so `SYSTEM_USER` answers a name on VaubanDB as it does on SQL Server.
fn system_user_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(match ctx.login_name() {
        Some(name) => string(name),
        None => Value::Null,
    })
}

/// `CURRENT_USER`: Microsoft Learn, "CURRENT_USER (Transact-SQL)" — the ANSI spelling of
/// `USER_NAME()`.
const CURRENT_USER_DEF: FunctionDef = FunctionDef {
    name: "CURRENT_USER",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: user_name_eval,
    aggregate: None,
};

/// `SESSION_USER`: Microsoft Learn, "SESSION_USER (Transact-SQL)".
const SESSION_USER_DEF: FunctionDef = FunctionDef {
    name: "SESSION_USER",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: user_name_eval,
    aggregate: None,
};

/// `SYSTEM_USER`: Microsoft Learn, "SYSTEM_USER (Transact-SQL)" — the login, not the
/// database user, which is what separates it from the three others.
const SYSTEM_USER_DEF: FunctionDef = FunctionDef {
    name: "SYSTEM_USER",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: system_user_eval,
    aggregate: None,
};

/// `USER`: Microsoft Learn, "USER (Transact-SQL)".
///
/// `USER_NAME()` is another function; `USER` itself refuses parentheses, like the four
/// other niladic names (102 at the parenthesis).
const USER_DEF: FunctionDef = FunctionDef {
    name: "USER",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: user_name_eval,
    aggregate: None,
};

// --- What a client reads about its transaction ---------------------------------------------
//
// `XACT_STATE()`, `@@LOCK_TIMEOUT` and `@@TRANCOUNT` come from three methods of
// `EvalContext` whose defaults are the values of a session outside any explicit
// transaction and without `SET LOCK_TIMEOUT`: `SELECT XACT_STATE();` is 0, `SELECT
// @@LOCK_TIMEOUT;` is -1, `SELECT @@TRANCOUNT;` is 0. The session makes them vary.
//
// On SQL Server, `XACT_STATE()` alone in its `SELECT` list answers 0, while `SELECT
// XACT_STATE() AS xs, @@LOCK_TIMEOUT AS lt, @@TRANCOUNT AS tc;` answers `xs` = 1 with
// `tc` still 0, as do the pairs with `@@SPID`, `@@VERSION`, `DB_NAME()`, `@@SERVERNAME`
// and `@@DATEFIRST`, where the pairs with a literal, `GETDATE()`, `@@ERROR` and
// `@@ROWCOUNT` answer 0. VaubanDB answers the state of the session in each shape.
//
// `@@DEADLOCK_PRIORITY` is **not** registered: it is not a global variable of SQL Server
// 2022, where `SELECT @@DEADLOCK_PRIORITY;` is 137/15/2 (undeclared scalar variable), the
// number a name outside the registry already gets from the binder. `SET
// DEADLOCK_PRIORITY` is a session option.

/// Result type of `XACT_STATE()`: `smallint`, nullable.
///
/// Nullable where `@@LOCK_TIMEOUT` and `@@TRANCOUNT` are not, and where `@@SPID`, a
/// `smallint` too, is not: the flag belongs to the function
/// (`xact_state_return_type_is_a_nullable_smallint`).
fn xact_state_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::SmallInt, true))
}

/// Evaluates `XACT_STATE()`: the state of the transaction the session is in.
fn xact_state_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::I16(ctx.xact_state()))
}

/// Evaluates `@@LOCK_TIMEOUT`: the lock timeout of the session in milliseconds.
fn lock_timeout_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::I32(ctx.lock_timeout()))
}

/// `XACT_STATE`: Microsoft Learn, "XACT_STATE (Transact-SQL)". Written with parentheses,
/// unlike `@@LOCK_TIMEOUT`: telling the two spellings apart is the `parser`'s job.
const XACT_STATE_DEF: FunctionDef = FunctionDef {
    name: "XACT_STATE",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: xact_state_type,
    eval: xact_state_eval,
    aggregate: None,
};

/// `@@LOCK_TIMEOUT`: Microsoft Learn, "@@LOCK_TIMEOUT (Transact-SQL)". Changed by `SET
/// LOCK_TIMEOUT`, hence not deterministic.
const LOCK_TIMEOUT_DEF: FunctionDef = FunctionDef {
    name: "@@LOCK_TIMEOUT",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: int_type,
    eval: lock_timeout_eval,
    aggregate: None,
};

/// Registers the functions of this module.
pub(crate) fn register_all() {
    register(SPID_DEF);
    register(ROWCOUNT_DEF);
    register(ERROR_DEF);
    register(TRANCOUNT_DEF);
    register(SERVER_NAME_DEF);
    register(DB_NAME_DEF);
    register(NEWID_DEF);
    register(SQL_VARIANT_PROPERTY_DEF);
    register(COLLATION_PROPERTY_DEF);
    register(CURRENT_USER_DEF);
    register(SESSION_USER_DEF);
    register(SYSTEM_USER_DEF);
    register(USER_DEF);
    register(XACT_STATE_DEF);
    register(LOCK_TIMEOUT_DEF);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::check_call;
    use crate::context::StaticContext;
    use crate::{lookup, register_builtins};
    use std::collections::HashSet;
    use vauban_types::{Date, DateTime, DateTime2, DateTimeOffset, Decimal, Time};

    /// The seven session definitions, in registration order.
    const DEFS: [&FunctionDef; 7] = [
        &SPID_DEF,
        &ROWCOUNT_DEF,
        &ERROR_DEF,
        &TRANCOUNT_DEF,
        &SERVER_NAME_DEF,
        &DB_NAME_DEF,
        &NEWID_DEF,
    ];

    /// An [`EvalContext`] whose `variable` answers `value` for any name: it exercises the
    /// fallback of `@@ERROR` on a variant it does not expect, and serves as the
    /// counter-test of `@@TRANCOUNT`, which does not read `variable`.
    struct VariableContext {
        value: Option<Value>,
    }

    impl EvalContext for VariableContext {
        fn now_local(&self) -> DateTime2 {
            DateTime2 {
                date: Date { days: 0 },
                time: Time { ticks_100ns: 0 },
            }
        }

        fn now_utc(&self) -> DateTime2 {
            self.now_local()
        }

        fn rowcount(&self) -> i64 {
            0
        }

        fn last_identity(&self) -> Option<Decimal> {
            None
        }

        fn spid(&self) -> i16 {
            0
        }

        fn current_database(&self) -> &str {
            ""
        }

        fn server_name(&self) -> &str {
            ""
        }

        fn object_id(&self, _name: &str) -> Option<i32> {
            None
        }

        fn object_name(&self, _id: i32) -> Option<String> {
            None
        }

        fn variable(&self, _name: &str) -> Option<Value> {
            self.value.clone()
        }
    }

    /// Evaluates `def` on `values`, every argument typed `int`, as `check_call` would.
    fn eval(def: &FunctionDef, values: &[Value], ctx: &dyn EvalContext) -> Value {
        let types: Vec<TypeInfo> = values
            .iter()
            .map(|_| TypeInfo::new(SqlType::Int, true))
            .collect();
        let result = (def.return_type)(&types).expect("return_type must succeed");
        let args = EvalArgs {
            values,
            types: &types,
            result: &result,
        };
        (def.eval)(&args, ctx).expect("eval must succeed")
    }

    /// Evaluates the registered function `name` on `values`.
    fn call(name: &str, values: &[Value], ctx: &dyn EvalContext) -> Value {
        register_builtins();
        let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
        eval(def, values, ctx)
    }

    fn text(value: &Value) -> &str {
        match value {
            Value::String(s) => &s.text,
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn session_variables_read_the_context() {
        let ctx = StaticContext {
            spid: 57,
            rowcount: 3,
            database: "master".to_owned(),
            server_name: "VAUBAN".to_owned(),
            ..StaticContext::default()
        };
        assert_eq!(call("@@SPID", &[], &ctx), Value::I16(57));
        assert_eq!(call("@@ROWCOUNT", &[], &ctx), Value::I32(3));
        assert_eq!(text(&call("DB_NAME", &[], &ctx)), "master");
        assert_eq!(text(&call("@@SERVERNAME", &[], &ctx)), "VAUBAN");
    }

    #[test]
    fn rowcount_saturates() {
        let ctx = StaticContext {
            rowcount: i64::MAX,
            ..StaticContext::default()
        };
        assert_eq!(call("@@ROWCOUNT", &[], &ctx), Value::I32(i32::MAX));
    }

    /// `@@ERROR` alone: `@@TRANCOUNT` reads [`EvalContext::trancount`] and is covered by
    /// `trancount_reads_its_own_method`.
    #[test]
    fn error_defaults_to_zero() {
        // `StaticContext::variable` answers `None`: nothing known, so zero.
        let unknown = StaticContext::default();
        assert_eq!(call("@@ERROR", &[], &unknown), Value::I32(0));

        // An unexpected variant is ignored, not reported as an error.
        let unexpected = VariableContext {
            value: Some(string("not an int")),
        };
        assert_eq!(call("@@ERROR", &[], &unexpected), Value::I32(0));
        let args = EvalArgs {
            values: &[],
            types: &[],
            result: &TypeInfo::new(SqlType::Int, false),
        };
        assert!(error_eval(&args, &unexpected).is_ok());

        // A known value is returned as it is.
        let known = VariableContext {
            value: Some(Value::I32(8134)),
        };
        assert_eq!(call("@@ERROR", &[], &known), Value::I32(8134));
    }

    #[test]
    fn db_name_with_unknown_id_is_null() {
        // `StaticContext` knows no database by identifier, like SQL Server for an
        // identifier no database has.
        let ctx = StaticContext {
            database: "master".to_owned(),
            ..StaticContext::default()
        };
        assert_eq!(call("DB_NAME", &[Value::I32(99)], &ctx), Value::Null);
        assert_eq!(call("DB_NAME", &[Value::Null], &ctx), Value::Null);
    }

    #[test]
    fn newid_is_a_v4_guid() {
        let ctx = StaticContext::default();
        let mut seen: HashSet<[u8; 16]> = HashSet::new();
        for _ in 0..1_000 {
            let value = call("NEWID", &[], &ctx);
            let Value::Guid(stored) = value else {
                panic!("NEWID must return a Guid, got {value:?}");
            };
            // Back to the RFC 4122 order: the version nibble is byte 6 there, which is
            // byte 7 of the stored array (first three groups little-endian), and the
            // variant is byte 8 in both orders.
            let rfc = to_storage_order(stored);
            assert_eq!(rfc[6] >> 4, 4, "version bits of {stored:?}");
            assert_eq!(rfc[8] >> 6, 0b10, "variant bits of {stored:?}");
            assert_eq!(rfc[6], stored[7], "version byte is stored at index 7");
            assert_eq!(rfc[8], stored[8], "variant byte does not move");
            seen.insert(stored);
        }
        assert_eq!(seen.len(), 1_000, "1 000 calls must give 1 000 values");
    }

    #[test]
    fn newid_return_type() {
        let info = guid_type(&[]).expect("return_type must succeed");
        assert_eq!(info.ty, SqlType::UniqueIdentifier);
        assert!(!info.nullable);

        for def in DEFS {
            assert!(!def.deterministic, "{} must not be deterministic", def.name);
            assert!(def.aggregate.is_none(), "{} is not an aggregate", def.name);
            assert_eq!(def.kind, FunctionKind::Scalar);
        }
    }

    #[test]
    fn names_are_registered_once() {
        register_builtins();
        // A second call must not panic on the duplicate-name check of `register`.
        register_builtins();
        for name in [
            "@@spid",
            "@@RowCount",
            "db_name",
            "newid",
            // The two transaction names, in the spelling a query may carry.
            "xact_state",
            "@@lock_timeout",
        ] {
            let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(def.name.to_ascii_uppercase(), name.to_ascii_uppercase());
        }
        for def in DEFS {
            assert_eq!(
                lookup(def.name)
                    .unwrap_or_else(|| panic!("{} must be registered", def.name))
                    .name,
                def.name
            );
        }
    }

    #[test]
    fn return_types_follow_the_documented_types() {
        assert_eq!(smallint_type(&[]).expect("smallint").ty, SqlType::SmallInt);
        assert!(!smallint_type(&[]).expect("smallint").nullable);
        assert_eq!(int_type(&[]).expect("int").ty, SqlType::Int);
        assert!(!int_type(&[]).expect("int").nullable);
        let name = name_type(&[]).expect("name");
        assert_eq!(name.ty, SqlType::NVarChar(Len::Fixed(128)));
        assert!(name.nullable);
    }

    #[test]
    fn rowcount_saturates_on_both_ends() {
        for (count, expected) in [
            (0_i64, 0_i32),
            (7, 7),
            (i64::from(i32::MAX), i32::MAX),
            (i64::from(i32::MAX) + 1, i32::MAX),
            (i64::MIN, i32::MIN),
        ] {
            let ctx = StaticContext {
                rowcount: count,
                ..StaticContext::default()
            };
            assert_eq!(call("@@ROWCOUNT", &[], &ctx), Value::I32(expected));
        }
    }

    #[test]
    fn db_name_accepts_any_integer_width() {
        struct OneDatabase;
        impl EvalContext for OneDatabase {
            fn now_local(&self) -> DateTime2 {
                DateTime2 {
                    date: Date { days: 0 },
                    time: Time { ticks_100ns: 0 },
                }
            }
            fn now_utc(&self) -> DateTime2 {
                self.now_local()
            }
            fn rowcount(&self) -> i64 {
                0
            }
            fn last_identity(&self) -> Option<Decimal> {
                None
            }
            fn spid(&self) -> i16 {
                0
            }
            fn current_database(&self) -> &str {
                "vauban"
            }
            fn server_name(&self) -> &str {
                ""
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
            fn database_name(&self, id: i32) -> Option<String> {
                (id == 1).then(|| "master".to_owned())
            }
        }

        let ctx = OneDatabase;
        assert_eq!(text(&call("DB_NAME", &[], &ctx)), "vauban");
        for known in [Value::I8(1), Value::I16(1), Value::I32(1), Value::I64(1)] {
            assert_eq!(text(&call("DB_NAME", &[known], &ctx)), "master");
        }
        // Out of the `int` range, and a variant the binder never produces: `NULL`, no error.
        assert_eq!(call("DB_NAME", &[Value::I64(i64::MAX)], &ctx), Value::Null);
        assert_eq!(call("DB_NAME", &[string("master")], &ctx), Value::Null);
    }

    // --- Metadata of a value ----------------------------------------------------------------

    /// Calls a metadata function on a value of a **declared** type, which is what these two
    /// read: the shared `eval` helper types every argument `int`, which would hide the
    /// whole question.
    fn metadata(def: &FunctionDef, values: &[Value], types: &[TypeInfo]) -> Value {
        let result = (def.return_type)(types).expect("return_type must succeed");
        let args = EvalArgs {
            values,
            types,
            result: &result,
        };
        (def.eval)(&args, &StaticContext::default()).expect("eval must succeed")
    }

    /// `SQL_VARIANT_PROPERTY(value of type `ty`, property)` as text, `None` for `NULL`.
    fn property(ty: SqlType, value: Value, name: &str) -> Option<String> {
        let types = [
            TypeInfo::new(ty, true),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true),
        ];
        match metadata(&SQL_VARIANT_PROPERTY_DEF, &[value, string(name)], &types) {
            Value::Null => None,
            Value::String(s) => Some(s.text),
            other => panic!("SQL_VARIANT_PROPERTY answered {other:?}"),
        }
    }

    /// `COLLATIONPROPERTY(collation, property)` as an `int`, `None` for `NULL`.
    fn collation_property(collation: &str, name: &str) -> Option<i32> {
        let types = [
            TypeInfo::new(SqlType::VarChar(Len::Fixed(128)), true),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true),
        ];
        match metadata(
            &COLLATION_PROPERTY_DEF,
            &[string(collation), string(name)],
            &types,
        ) {
            Value::Null => None,
            Value::I32(n) => Some(n),
            other => panic!("COLLATIONPROPERTY answered {other:?}"),
        }
    }

    /// A `decimal(p, s)` holding `mantissa`.
    fn decimal(precision: u8, scale: u8, mantissa: i128) -> (SqlType, Value) {
        (
            SqlType::Decimal { precision, scale },
            Value::Decimal(Decimal {
                mantissa,
                precision,
                scale,
            }),
        )
    }

    #[test]
    fn metadata_functions_are_registered() {
        register_builtins();
        for name in ["SQL_VARIANT_PROPERTY", "COLLATIONPROPERTY"] {
            let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(def.kind, FunctionKind::Scalar);
            assert!(
                def.deterministic,
                "{name} depends on nothing but its arguments"
            );
            assert!(def.aggregate.is_none());
            // `SELECT SQL_VARIANT_PROPERTY(1);` and `SELECT COLLATIONPROPERTY('x');` both
            // answer 174 `... requires 2 argument(s).`
            assert!(def.arity.accepts(2));
            assert!(!def.arity.accepts(1) && !def.arity.accepts(3));
        }
        // Lookup is case-insensitive, as it is for every built-in.
        assert!(lookup("sql_variant_property").is_some());
        assert!(lookup("CollationProperty").is_some());
    }

    #[test]
    fn variant_property_base_type_is_the_type_name() {
        for (ty, value, expected) in [
            (SqlType::Int, Value::I32(1), "int"),
            (SqlType::TinyInt, Value::I8(1), "tinyint"),
            (SqlType::VarChar(Len::Fixed(10)), string("abc"), "varchar"),
            (SqlType::NVarChar(Len::Fixed(10)), string("abc"), "nvarchar"),
            (SqlType::Char(Len::Fixed(10)), string("abc"), "char"),
            (
                SqlType::Binary(Len::Fixed(4)),
                Value::Bytes(vec![0, 0, 0, 1]),
                "binary",
            ),
            (
                SqlType::VarBinary(Len::Fixed(8)),
                Value::Bytes(vec![0, 0, 0, 1]),
                "varbinary",
            ),
            (
                SqlType::UniqueIdentifier,
                Value::Guid([0; 16]),
                "uniqueidentifier",
            ),
            (SqlType::Date, Value::Date(Date { days: 730_119 }), "date"),
            (
                SqlType::Time(3),
                Value::Time(Time { ticks_100ns: 0 }),
                "time",
            ),
        ] {
            assert_eq!(property(ty, value, "BaseType").as_deref(), Some(expected));
        }
        // `decimal` and `numeric` keep their own names here, unlike in an error message
        // where both are `numeric` (`SqlType::error_name`).
        let (ty, value) = decimal(12, 4, 10_000);
        assert_eq!(property(ty, value, "BaseType").as_deref(), Some("decimal"));
        let numeric = SqlType::Numeric {
            precision: 5,
            scale: 2,
        };
        let value = Value::Decimal(Decimal {
            mantissa: 100,
            precision: 5,
            scale: 2,
        });
        assert_eq!(
            property(numeric, value, "BaseType").as_deref(),
            Some("numeric")
        );
    }

    #[test]
    fn variant_property_precision_and_scale() {
        for (ty, value, precision, scale) in [
            (SqlType::Bit, Value::Bit(true), "1", "0"),
            (SqlType::TinyInt, Value::I8(1), "3", "0"),
            (SqlType::SmallInt, Value::I16(1), "5", "0"),
            (SqlType::Int, Value::I32(1), "10", "0"),
            (SqlType::BigInt, Value::I64(1), "19", "0"),
            (SqlType::Float, Value::F64(1.0), "53", "0"),
            (SqlType::Real, Value::F32(1.0), "24", "0"),
            (SqlType::Money, Value::Money(10_000), "19", "4"),
            (SqlType::SmallMoney, Value::Money(10_000), "10", "4"),
            (SqlType::Date, Value::Date(Date { days: 1 }), "10", "0"),
            (
                SqlType::DateTime,
                Value::DateTime(DateTime {
                    days: 0,
                    ticks_300th: 0,
                }),
                "23",
                "3",
            ),
            (
                SqlType::SmallDateTime,
                Value::DateTime(DateTime {
                    days: 0,
                    ticks_300th: 0,
                }),
                "16",
                "0",
            ),
            (
                SqlType::Time(0),
                Value::Time(Time { ticks_100ns: 0 }),
                "8",
                "0",
            ),
            (
                SqlType::Time(3),
                Value::Time(Time { ticks_100ns: 0 }),
                "12",
                "3",
            ),
            (
                SqlType::Time(7),
                Value::Time(Time { ticks_100ns: 0 }),
                "16",
                "7",
            ),
            (SqlType::VarChar(Len::Fixed(10)), string("abc"), "0", "0"),
            (SqlType::UniqueIdentifier, Value::Guid([0; 16]), "0", "0"),
            (
                SqlType::Binary(Len::Fixed(4)),
                Value::Bytes(vec![1]),
                "0",
                "0",
            ),
        ] {
            assert_eq!(
                (
                    property(ty, value.clone(), "Precision").as_deref(),
                    property(ty, value, "Scale").as_deref()
                ),
                (Some(precision), Some(scale)),
                "{ty:?}"
            );
        }
        // `decimal(p, s)` reports its own declaration.
        let (ty, value) = decimal(12, 4, 10_000);
        assert_eq!(
            property(ty, value.clone(), "Precision").as_deref(),
            Some("12")
        );
        assert_eq!(property(ty, value, "Scale").as_deref(), Some("4"));
        // The date and time types count the characters of their rendering: 19 for
        // `yyyy-mm-dd hh:mm:ss`, 26 with an offset, plus the point and the digits.
        let stamp = Value::DateTime2(DateTime2 {
            date: Date { days: 1 },
            time: Time { ticks_100ns: 0 },
        });
        assert_eq!(
            property(SqlType::DateTime2(0), stamp.clone(), "Precision").as_deref(),
            Some("19")
        );
        assert_eq!(
            property(SqlType::DateTime2(3), stamp.clone(), "Precision").as_deref(),
            Some("23")
        );
        assert_eq!(
            property(SqlType::DateTime2(7), stamp, "Precision").as_deref(),
            Some("27")
        );
        let offset = Value::DateTimeOffset(DateTimeOffset {
            utc: DateTime2 {
                date: Date { days: 1 },
                time: Time { ticks_100ns: 0 },
            },
            offset_minutes: 120,
        });
        assert_eq!(
            property(SqlType::DateTimeOffset(0), offset.clone(), "Precision").as_deref(),
            Some("26")
        );
        assert_eq!(
            property(SqlType::DateTimeOffset(7), offset, "Precision").as_deref(),
            Some("34")
        );
    }

    #[test]
    fn variant_property_max_length_is_the_declared_storage() {
        for (ty, value, expected) in [
            (SqlType::Bit, Value::Bit(true), "1"),
            (SqlType::TinyInt, Value::I8(1), "1"),
            (SqlType::SmallInt, Value::I16(1), "2"),
            (SqlType::Int, Value::I32(1), "4"),
            (SqlType::BigInt, Value::I64(1), "8"),
            (SqlType::Real, Value::F32(1.0), "4"),
            (SqlType::Float, Value::F64(1.0), "8"),
            (SqlType::SmallMoney, Value::Money(1), "4"),
            (SqlType::Money, Value::Money(1), "8"),
            (SqlType::Date, Value::Date(Date { days: 1 }), "3"),
            (
                SqlType::SmallDateTime,
                Value::DateTime(DateTime {
                    days: 0,
                    ticks_300th: 0,
                }),
                "4",
            ),
            (
                SqlType::DateTime,
                Value::DateTime(DateTime {
                    days: 0,
                    ticks_300th: 0,
                }),
                "8",
            ),
            (SqlType::UniqueIdentifier, Value::Guid([0; 16]), "16"),
            (SqlType::Time(0), Value::Time(Time { ticks_100ns: 0 }), "3"),
            (SqlType::Time(3), Value::Time(Time { ticks_100ns: 0 }), "4"),
            (SqlType::Time(7), Value::Time(Time { ticks_100ns: 0 }), "5"),
            // The declared length, not the value's: `char(1)` is 1 and `varchar(8000)`
            // is 8000 whatever it holds; the national types double it.
            (SqlType::Char(Len::Fixed(1)), string("a"), "1"),
            (SqlType::VarChar(Len::Fixed(8000)), string("a"), "8000"),
            (SqlType::VarChar(Len::Fixed(10)), string("abc"), "10"),
            (SqlType::NVarChar(Len::Fixed(4000)), string("a"), "8000"),
            (SqlType::NVarChar(Len::Fixed(10)), string("abc"), "20"),
            (SqlType::NChar(Len::Fixed(4)), string("ab"), "8"),
            (SqlType::Binary(Len::Fixed(1)), Value::Bytes(vec![1]), "1"),
            (
                SqlType::VarBinary(Len::Fixed(8000)),
                Value::Bytes(vec![1]),
                "8000",
            ),
        ] {
            assert_eq!(
                property(ty, value, "MaxLength").as_deref(),
                Some(expected),
                "{ty:?}"
            );
        }
        let stamp = Value::DateTime2(DateTime2 {
            date: Date { days: 1 },
            time: Time { ticks_100ns: 0 },
        });
        for (scale, expected) in [(0, "6"), (3, "7"), (7, "8")] {
            assert_eq!(
                property(SqlType::DateTime2(scale), stamp.clone(), "MaxLength").as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn variant_property_decimal_lengths_follow_the_value() {
        // The four steps of the mantissa, and the vector that distinguishes them from a
        // rule on the *precision*: at one and the same `decimal(38,0)` the answer is 5 for
        // a small value and 17 for a large one, where a rule on the precision would give
        // 17 throughout.
        for (mantissa, max_length, total_bytes) in [
            (0i128, "5", "9"),
            (4_294_967_295, "5", "9"),
            (4_294_967_296, "9", "13"),
            (99_999_999_999_999_999_999, "13", "17"),
            (
                99_999_999_999_999_999_999_999_999_999_999_999_999,
                "17",
                "21",
            ),
        ] {
            let (ty, value) = decimal(38, 0, mantissa);
            assert_eq!(
                property(ty, value.clone(), "MaxLength").as_deref(),
                Some(max_length)
            );
            assert_eq!(
                property(ty, value, "TotalBytes").as_deref(),
                Some(total_bytes)
            );
        }
        // A negative mantissa is counted on its magnitude.
        let (ty, value) = decimal(12, 4, -10_000);
        assert_eq!(property(ty, value, "MaxLength").as_deref(), Some("5"));
        // `decimal(12,4)` holding 99999999.9999: 999999999999 needs a second word.
        let (ty, value) = decimal(12, 4, 999_999_999_999);
        assert_eq!(
            property(ty, value.clone(), "MaxLength").as_deref(),
            Some("9")
        );
        assert_eq!(property(ty, value, "TotalBytes").as_deref(), Some("13"));
    }

    #[test]
    fn variant_property_total_bytes_counts_the_variant_header() {
        for (ty, value, expected) in [
            // Fixed types: the storage plus two header bytes.
            (SqlType::Int, Value::I32(1), "6"),
            (SqlType::Bit, Value::Bit(true), "3"),
            (SqlType::SmallInt, Value::I16(1), "4"),
            (SqlType::BigInt, Value::I64(1), "10"),
            (SqlType::UniqueIdentifier, Value::Guid([0; 16]), "18"),
            (SqlType::Date, Value::Date(Date { days: 1 }), "5"),
            (
                SqlType::DateTime,
                Value::DateTime(DateTime {
                    days: 0,
                    ticks_300th: 0,
                }),
                "10",
            ),
            // A scale byte joins the header for `time` and `datetime2`.
            (SqlType::Time(3), Value::Time(Time { ticks_100ns: 0 }), "7"),
            // Character types: the data plus eight, the declared length for `char`.
            (SqlType::VarChar(Len::Fixed(10)), string("abc"), "11"),
            (SqlType::VarChar(Len::Fixed(200)), string("ab"), "10"),
            (SqlType::VarChar(Len::Fixed(10)), string(""), "8"),
            // Trailing spaces count, as they do for DATALENGTH.
            (SqlType::VarChar(Len::Fixed(10)), string("abc   "), "14"),
            (SqlType::Char(Len::Fixed(10)), string("abc"), "18"),
            (SqlType::Char(Len::Fixed(3)), string("abc"), "11"),
            (SqlType::NVarChar(Len::Fixed(10)), string("abc"), "14"),
            (SqlType::NChar(Len::Fixed(4)), string("ab"), "16"),
            // Binary types: the data plus four, the declared length for `binary`.
            (
                SqlType::Binary(Len::Fixed(4)),
                Value::Bytes(vec![0, 0, 0, 1]),
                "8",
            ),
            (
                SqlType::Binary(Len::Fixed(8)),
                Value::Bytes(vec![0, 0, 0, 1]),
                "12",
            ),
            (
                SqlType::VarBinary(Len::Fixed(8)),
                Value::Bytes(vec![0, 0, 0, 1]),
                "8",
            ),
            (
                SqlType::VarBinary(Len::Fixed(8)),
                Value::Bytes(vec![0; 8]),
                "12",
            ),
        ] {
            assert_eq!(
                property(ty, value, "TotalBytes").as_deref(),
                Some(expected),
                "{ty:?}"
            );
        }
        let stamp = Value::DateTime2(DateTime2 {
            date: Date { days: 1 },
            time: Time { ticks_100ns: 0 },
        });
        for (scale, expected) in [(0, "9"), (3, "10"), (7, "11")] {
            assert_eq!(
                property(SqlType::DateTime2(scale), stamp.clone(), "TotalBytes").as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn variant_property_collation_names_the_character_types_only() {
        assert_eq!(
            property(SqlType::VarChar(Len::Fixed(10)), string("abc"), "Collation").as_deref(),
            Some("SQL_Latin1_General_CP1_CI_AS")
        );
        assert_eq!(
            property(SqlType::NChar(Len::Fixed(5)), string("a"), "Collation").as_deref(),
            Some("SQL_Latin1_General_CP1_CI_AS")
        );
        for (ty, value) in [
            (SqlType::Int, Value::I32(1)),
            (SqlType::UniqueIdentifier, Value::Guid([0; 16])),
            (SqlType::Binary(Len::Fixed(4)), Value::Bytes(vec![1])),
            (SqlType::Date, Value::Date(Date { days: 1 })),
            (SqlType::Bit, Value::Bit(false)),
        ] {
            assert_eq!(property(ty, value, "Collation"), None, "{ty:?}");
        }
    }

    #[test]
    fn collation_name_restores_the_sixteen_unique_linguistic_utf8_names() {
        // Each combination, as SQL_VARIANT_PROPERTY(..., 'Collation') spells it.
        for case in ["CI", "CS"] {
            for accent in ["AI", "AS"] {
                for kana in ["", "_KS"] {
                    for width in ["", "_WS"] {
                        let base = format!("Latin1_General_100_{case}_{accent}{kana}{width}");
                        let utf8 = format!("{base}_SC_UTF8");
                        assert_eq!(collation_name(&Collation::parse(&utf8).unwrap()), utf8);
                        // Non-UTF8 SC is ambiguous: the representation cannot distinguish
                        // the two names. Preserve the base spelling.
                        let plain = Collation::parse(&base).unwrap();
                        let sc = Collation::parse(&format!("{base}_SC")).unwrap();
                        assert_eq!(plain, sc);
                        assert_eq!(collation_name(&sc), base);
                    }
                }
            }
        }
        let binary = "Latin1_General_100_BIN2_UTF8";
        assert_eq!(collation_name(&Collation::parse(binary).unwrap()), binary);
    }

    #[test]
    fn collation_name_round_trips_the_names_the_wire_can_carry() {
        // Not every name the server has: the instance carries 72 `Latin1_General` and
        // `SQL_Latin1_General_CP1` collations, and the fourteen below are the ones the five
        // TDS `Collation` bytes can spell back. `Latin1_General_100_CI_AS_SC` is the
        // counter-example that keeps `every` out of this name: `_SC` has no bit on the wire,
        // so it does not round-trip (see [`collation_name`]).
        for name in [
            "SQL_Latin1_General_CP1_CI_AS",
            "SQL_Latin1_General_CP1_CS_AS",
            "SQL_Latin1_General_CP1_CI_AI",
            "Latin1_General_CI_AS",
            "Latin1_General_CS_AS",
            "Latin1_General_CI_AI",
            "Latin1_General_CI_AS_KS",
            "Latin1_General_CI_AS_WS",
            "Latin1_General_CI_AS_KS_WS",
            "Latin1_General_CS_AS_KS_WS",
            "Latin1_General_BIN",
            "Latin1_General_BIN2",
            "Latin1_General_100_CI_AS",
            "Latin1_General_100_BIN2",
        ] {
            let collation = Collation::parse(name).expect("a collation name");
            assert_eq!(collation_name(&collation), name);
        }
        assert_eq!(
            collation_name(&Collation::DEFAULT),
            "SQL_Latin1_General_CP1_CI_AS"
        );
    }

    #[test]
    fn variant_property_null_value_hides_its_type() {
        // A `NULL` answers `NULL` for each property, and a *declared* type does not save
        // it: `SQL_VARIANT_PROPERTY(CAST(NULL AS int), 'BaseType')` is `NULL`, not `int`.
        for name in [
            "BaseType",
            "Precision",
            "Scale",
            "TotalBytes",
            "Collation",
            "MaxLength",
        ] {
            assert_eq!(property(SqlType::Int, Value::Null, name), None, "{name}");
            assert_eq!(
                property(SqlType::VarChar(Len::Fixed(10)), Value::Null, name),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn variant_property_unknown_property_is_null() {
        for name in ["NoSuchThing", "", " BaseType", "Base Type"] {
            assert_eq!(
                property(SqlType::Int, Value::I32(1), name),
                None,
                "{name:?}"
            );
        }
        // Case folded, trailing blanks ignored, leading ones not.
        for name in ["basetype", "BASETYPE", "BaseType  ", "bAsEtYpE"] {
            assert_eq!(
                property(SqlType::Int, Value::I32(1), name).as_deref(),
                Some("int"),
                "{name:?}"
            );
        }
        // A property argument that is not a string names nothing.
        let types = [
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::Int, true),
        ];
        assert_eq!(
            metadata(
                &SQL_VARIANT_PROPERTY_DEF,
                &[Value::I32(1), Value::I32(1)],
                &types
            ),
            Value::Null
        );
        assert_eq!(
            metadata(
                &SQL_VARIANT_PROPERTY_DEF,
                &[Value::I32(1), Value::Null],
                &types
            ),
            Value::Null
        );
    }

    #[test]
    fn variant_property_refuses_the_max_types() {
        // `SELECT SQL_VARIANT_PROPERTY(CAST('ab' AS varchar(max)), 'BaseType');` is
        // 206/16/2, a type mismatch between `varchar(max)` and `sql_variant`.
        for ty in [
            SqlType::VarChar(Len::Max),
            SqlType::NVarChar(Len::Max),
            SqlType::VarBinary(Len::Max),
        ] {
            let args = [
                TypeInfo::new(ty, true),
                TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true),
            ];
            let err =
                (SQL_VARIANT_PROPERTY_DEF.return_type)(&args).expect_err("a (max) type is refused");
            assert_eq!(err.number, 206);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, 2);
            assert_eq!(
                err.message,
                format!(
                    "Type mismatch: {} cannot be combined with sql_variant.",
                    ty.declaration()
                )
            );
        }
        // A sized string is not refused.
        let args = [
            TypeInfo::new(SqlType::VarChar(Len::Fixed(8000)), true),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true),
        ];
        assert!((SQL_VARIANT_PROPERTY_DEF.return_type)(&args).is_ok());
    }

    #[test]
    fn variant_property_answers_nvarchar_128() {
        let args = [
            TypeInfo::new(SqlType::Int, false),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), false),
        ];
        let info = (SQL_VARIANT_PROPERTY_DEF.return_type)(&args).expect("int is accepted");
        assert_eq!(info.ty, SqlType::NVarChar(Len::Fixed(128)));
        // Nullable even when neither argument is: an unknown property answers `NULL`.
        assert!(info.nullable);
        let info = (COLLATION_PROPERTY_DEF.return_type)(&args).expect("no argument is refused");
        assert_eq!(info.ty, SqlType::Int);
        assert!(info.nullable);
    }

    #[test]
    fn collation_property_reads_the_four_fields() {
        // The default server collation, field by field.
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AS", "CodePage"),
            Some(1252)
        );
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AS", "LCID"),
            Some(1033)
        );
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AS", "ComparisonStyle"),
            Some(196_609)
        );
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AS", "Version"),
            Some(0)
        );
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AS", "SortId"),
            Some(52)
        );
        // The two other SQL collations the server has.
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CS_AS", "SortId"),
            Some(51)
        );
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AI", "SortId"),
            Some(54)
        );
        // Windows collations: no SortId, a version for the `_100_` family, 65001 for UTF-8.
        assert_eq!(
            collation_property("Latin1_General_CI_AS", "SortId"),
            Some(0)
        );
        assert_eq!(
            collation_property("Latin1_General_CI_AS", "Version"),
            Some(0)
        );
        assert_eq!(
            collation_property("Latin1_General_100_CI_AS", "Version"),
            Some(2)
        );
        assert_eq!(
            collation_property("Latin1_General_100_CI_AS", "LCID"),
            Some(1033)
        );
        assert_eq!(
            collation_property("Latin1_General_100_CI_AS_SC_UTF8", "CodePage"),
            Some(65001)
        );
        assert_eq!(
            collation_property("Latin1_General_100_CI_AS_SC_UTF8", "SortId"),
            Some(0)
        );
    }

    #[test]
    fn collation_property_comparison_style_is_a_mask_of_what_is_ignored() {
        for (name, style) in [
            ("SQL_Latin1_General_CP1_CI_AS", 196_609),
            ("SQL_Latin1_General_CP1_CS_AS", 196_608),
            ("SQL_Latin1_General_CP1_CI_AI", 196_611),
            ("Latin1_General_CI_AS", 196_609),
            ("Latin1_General_CS_AS", 196_608),
            ("Latin1_General_CI_AI", 196_611),
            ("Latin1_General_CI_AS_KS", 131_073),
            ("Latin1_General_CI_AS_WS", 65_537),
            ("Latin1_General_CI_AS_KS_WS", 1),
            ("Latin1_General_CS_AS_KS_WS", 0),
            // A binary collation ignores nothing, whatever its name spells.
            ("Latin1_General_BIN", 0),
            ("Latin1_General_BIN2", 0),
            ("Latin1_General_100_BIN2", 0),
        ] {
            assert_eq!(
                collation_property(name, "ComparisonStyle"),
                Some(style),
                "{name}"
            );
        }
    }

    #[test]
    fn collation_property_knows_only_the_names_the_server_has() {
        // Names SQL Server 2022 does not have: `NULL` here, 448 in a COLLATE clause. The
        // grammar refuses them, and both halves are checked here so that a widening of
        // either one shows up.
        for name in [
            "SQL_Latin1_General_CP1_CS_AI",
            "SQL_Latin1_General_CP1_CI_AS_KS",
            "SQL_Latin1_General_CP1_CI_AS_WS",
            "SQL_Latin1_General_CP1_CI_AS_KS_WS",
            "SQL_Latin1_General_CP1_BIN",
            "SQL_Latin1_General_CP1_BIN2",
            "SQL_Latin1_General_CP1_CI_AS_SC",
            "Latin1_General_90_CI_AS",
            "Latin1_General_CI_AS_SC",
            "Latin1_General_BIN_UTF8",
        ] {
            assert_eq!(
                Collation::parse(name).expect_err(name).number,
                448,
                "{name} is refused by the parser"
            );
            for property in ["CodePage", "LCID", "ComparisonStyle", "Version", "SortId"] {
                assert_eq!(
                    collation_property(name, property),
                    None,
                    "{name}/{property}"
                );
            }
        }
        // The Windows spelling of the same five bytes as `SQL_Latin1_General_CP1_BIN`
        // **does** exist, which is why the rejection cannot be structural.
        assert_eq!(
            collation_property("Latin1_General_BIN", "CodePage"),
            Some(1252)
        );
        // And the `_100_` names the server does have keep answering.
        assert_eq!(
            collation_property("Latin1_General_100_CI_AS_SC_UTF8", "CodePage"),
            Some(65001)
        );
        assert_eq!(
            collation_property("Latin1_General_100_BIN2_UTF8", "CodePage"),
            Some(65001)
        );
    }

    #[test]
    fn collation_property_unknown_name_or_property_is_null() {
        // An unknown collation is `NULL`, never error 448.
        for name in [
            "Klingon_CI_AS",
            "",
            "Latin1_General",
            " SQL_Latin1_General_CP1_CI_AS",
        ] {
            assert_eq!(collation_property(name, "CodePage"), None, "{name:?}");
        }
        // Trailing blanks are ignored on both arguments.
        assert_eq!(
            collation_property("SQL_Latin1_General_CP1_CI_AS ", "CodePage"),
            Some(1252)
        );
        assert_eq!(
            collation_property("sql_latin1_general_cp1_ci_as", "codepage"),
            Some(1252)
        );
        for property in ["NoSuchThing", "", " CodePage"] {
            assert_eq!(
                collation_property("SQL_Latin1_General_CP1_CI_AS", property),
                None,
                "{property:?}"
            );
        }
        // A `NULL` on either side, and a non-string argument, answer `NULL`.
        let types = [
            TypeInfo::new(SqlType::VarChar(Len::Fixed(128)), true),
            TypeInfo::new(SqlType::VarChar(Len::Fixed(30)), true),
        ];
        for values in [
            [Value::Null, string("CodePage")],
            [string("SQL_Latin1_General_CP1_CI_AS"), Value::Null],
            [Value::I32(1), string("CodePage")],
        ] {
            assert_eq!(
                metadata(&COLLATION_PROPERTY_DEF, &values, &types),
                Value::Null
            );
        }
    }
    // --- The niladic functions of the session ---------------------------------------------

    /// The four niladic definitions of this file, in registration order.
    const NILADIC_DEFS: [&FunctionDef; 4] = [
        &CURRENT_USER_DEF,
        &SESSION_USER_DEF,
        &SYSTEM_USER_DEF,
        &USER_DEF,
    ];

    /// The four names answer `nvarchar(128)`, nullable, and take no argument.
    ///
    /// Nullability is the half no ordinary `SELECT` shows: the TDS metadata a client sees
    /// carries no such flag. The four are nullable where `CURRENT_TIMESTAMP` is not, so
    /// the flag is a property of each function and not a default.
    #[test]
    fn niladic_session_functions_return_sysname() {
        register_builtins();
        for def in NILADIC_DEFS {
            let info = check_call(def, &[]).expect("no argument");
            assert_eq!(
                info.ty,
                SqlType::NVarChar(Len::Fixed(128)),
                "{} must answer nvarchar(128)",
                def.name
            );
            assert!(info.nullable, "{} must be nullable", def.name);
            assert!(!def.deterministic, "{} reads the session", def.name);
            assert!(def.aggregate.is_none(), "{} is not an aggregate", def.name);
            assert_eq!(def.kind, FunctionKind::Scalar, "{}", def.name);
            assert_eq!(def.arity, Arity::Exact(0), "{}", def.name);
        }
    }

    /// `USER`, `CURRENT_USER` and `SESSION_USER` read the database user; `SYSTEM_USER`
    /// reads the login. The two readings must not be confused: the context below gives
    /// them different values on purpose, which is the vector that separates the two
    /// evaluations — with a context where login and user are equal, any mapping would pass.
    #[test]
    fn niladic_session_functions_read_the_context() {
        let ctx = StaticContext {
            login_name: Some("sa".to_owned()),
            user_name: Some("dbo".to_owned()),
            ..StaticContext::default()
        };
        for name in ["USER", "CURRENT_USER", "SESSION_USER"] {
            assert_eq!(text(&call(name, &[], &ctx)), "dbo", "{name}");
        }
        assert_eq!(text(&call("SYSTEM_USER", &[], &ctx)), "sa");
    }

    /// A session that knows neither name answers `NULL` rather than an invented one.
    ///
    /// Without database principals nothing overrides [`EvalContext::user_name`], so
    /// `SELECT SESSION_USER;` answers `NULL` on VaubanDB where SQL Server answers `dbo`.
    /// This test pins the current answer so that the day `user_name` is wired the change
    /// is visible here.
    #[test]
    fn niladic_session_functions_without_a_session_are_null() {
        let ctx = StaticContext::default();
        for name in ["USER", "CURRENT_USER", "SESSION_USER", "SYSTEM_USER"] {
            assert_eq!(call(name, &[], &ctx), Value::Null, "{name}");
        }
    }

    /// The registry finds them whatever the case, and refuses an argument with 174.
    ///
    /// The lower-case name in the message is `check_call`'s doing and follows the shape
    /// SQL Server uses for its own built-ins; on SQL Server `SESSION_USER(1)` is a syntax
    /// error (102) long before an argument is counted, so 174 is what the registry answers
    /// on its own.
    #[test]
    fn niladic_session_functions_are_registered_case_insensitively() {
        register_builtins();
        for def in NILADIC_DEFS {
            assert_eq!(
                lookup(&def.name.to_lowercase())
                    .unwrap_or_else(|| panic!("{} must be registered", def.name))
                    .name,
                def.name
            );
            let mixed: String = def
                .name
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    }
                })
                .collect();
            assert_eq!(
                lookup(&mixed)
                    .unwrap_or_else(|| panic!("{mixed} must be registered"))
                    .name,
                def.name
            );
            let err = check_call(def, &[TypeInfo::new(SqlType::Int, true)])
                .expect_err("a niladic function takes no argument");
            assert_eq!(err.number, 174, "{}", def.name);
            assert_eq!(
                err.message,
                format!(
                    "The function {} takes exactly 0 argument(s).",
                    def.name.to_lowercase()
                )
            );
        }
    }

    // --- The transaction of the session ---------------------------------------------------

    /// The two transaction definitions, in registration order.
    const TXN_STATE_DEFS: [&FunctionDef; 2] = [&XACT_STATE_DEF, &LOCK_TIMEOUT_DEF];

    /// Both read the session, so neither is deterministic, and neither is an aggregate.
    #[test]
    fn transaction_state_functions_are_scalar_and_not_deterministic() {
        for def in TXN_STATE_DEFS {
            assert!(!def.deterministic, "{} reads the session", def.name);
            assert!(def.aggregate.is_none(), "{} is not an aggregate", def.name);
            assert_eq!(def.kind, FunctionKind::Scalar, "{}", def.name);
            assert_eq!(def.arity, Arity::Exact(0), "{}", def.name);
        }
    }

    /// `XACT_STATE()` answers [`EvalContext::xact_state`], as a `smallint`.
    ///
    /// The context is built at `-1` — the uncommittable state — and not at the default `0`:
    /// a context left at its default would pass whether the function read the method or
    /// returned a constant. `-1` is a value SQL Server answers: `SET XACT_ABORT ON; BEGIN
    /// TRAN; BEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT XACT_STATE(), @@TRANCOUNT;
    /// END CATCH` answers `-1` and `1`.
    #[test]
    fn xact_state_reads_the_context() {
        let doomed = StaticContext {
            xact_state: -1,
            ..StaticContext::default()
        };
        assert_eq!(call("XACT_STATE", &[], &doomed), Value::I16(-1));

        let active = StaticContext {
            xact_state: 1,
            ..StaticContext::default()
        };
        assert_eq!(call("XACT_STATE", &[], &active), Value::I16(1));
    }

    /// `XACT_STATE()` is a nullable `smallint`, where `@@SPID`, a `smallint` too, is not
    /// nullable: the flag belongs to the function.
    #[test]
    fn xact_state_return_type_is_a_nullable_smallint() {
        register_builtins();
        let info = check_call(&XACT_STATE_DEF, &[]).expect("no argument");
        assert_eq!(info.ty, SqlType::SmallInt);
        assert!(info.nullable, "XACT_STATE() is nullable");
        // The pair that separates the two flags.
        let spid = check_call(&SPID_DEF, &[]).expect("no argument");
        assert_eq!(spid.ty, SqlType::SmallInt);
        assert!(!spid.nullable);
    }

    /// `@@LOCK_TIMEOUT` answers [`EvalContext::lock_timeout`], an `int` that is not
    /// nullable.
    ///
    /// `5000` is the value right after `SET LOCK_TIMEOUT 5000`, and `-1` the value of a
    /// connection that ran no such statement; the second is the default of the trait,
    /// checked by `defaults_are_neutral` in `context.rs`.
    #[test]
    fn lock_timeout_reads_the_context() {
        let waiting = StaticContext::default();
        assert_eq!(call("@@LOCK_TIMEOUT", &[], &waiting), Value::I32(-1));

        let five_seconds = StaticContext {
            lock_timeout: 5_000,
            ..StaticContext::default()
        };
        assert_eq!(
            call("@@LOCK_TIMEOUT", &[], &five_seconds),
            Value::I32(5_000)
        );

        let immediate = StaticContext {
            lock_timeout: 0,
            ..StaticContext::default()
        };
        assert_eq!(call("@@LOCK_TIMEOUT", &[], &immediate), Value::I32(0));

        let info = check_call(&LOCK_TIMEOUT_DEF, &[]).expect("no argument");
        assert_eq!(info.ty, SqlType::Int);
        assert!(!info.nullable, "@@LOCK_TIMEOUT is not nullable");
    }

    /// `@@TRANCOUNT` follows [`EvalContext::trancount`] and no longer
    /// `variable("@@TRANCOUNT")`.
    ///
    /// The counter-test is the second block: a context whose `variable` answers `7` for any
    /// name it is given, and whose `trancount` keeps the default `0`, gives `0`; a
    /// reading of the map would give `7`. The first block is the other direction: a
    /// `trancount` of 2 while `variable` answers `None`, which a reading of the map would
    /// turn into `0`.
    #[test]
    fn trancount_reads_its_own_method() {
        let nested = StaticContext {
            trancount: 2,
            ..StaticContext::default()
        };
        assert_eq!(call("@@TRANCOUNT", &[], &nested), Value::I32(2));
        assert_eq!(nested.variable("@@TRANCOUNT"), None);

        let misleading = VariableContext {
            value: Some(Value::I32(7)),
        };
        assert_eq!(misleading.variable("@@TRANCOUNT"), Some(Value::I32(7)));
        assert_eq!(misleading.trancount(), 0, "the default of the trait");
        assert_eq!(call("@@TRANCOUNT", &[], &misleading), Value::I32(0));

        let info = check_call(&TRANCOUNT_DEF, &[]).expect("no argument");
        assert_eq!(info.ty, SqlType::Int);
        assert!(!info.nullable);
    }
}
