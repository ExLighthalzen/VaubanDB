//! In-memory representation of SQL Server values, `NULL` included.

/// SQL Server `decimal(p, s)` / `numeric(p, s)`: the value is `mantissa / 10^scale`.
///
/// No range validation happens here (`precision` up to 38 digits fits in an `i128`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    /// Signed unscaled value: the digits of the number without the decimal point.
    pub mantissa: i128,
    /// Total number of digits, `1..=38`.
    pub precision: u8,
    /// Number of digits to the right of the decimal point.
    pub scale: u8,
}

/// SQL Server character value (`char`, `varchar`, `nchar`, `nvarchar`).
///
/// Held as Unicode text; the `tds` module transcodes it to UTF-16LE or to code page 1252
/// according to the type of the column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SqlString {
    /// The Unicode text.
    pub text: String,
}

/// SQL Server `date`: days since 0001-01-01 in the proleptic Gregorian calendar
/// (2000-01-01 is day `730_119`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Date {
    /// Days since 0001-01-01.
    pub days: i32,
}

/// SQL Server `time(s)`: time elapsed since midnight in 100 ns units, the finest
/// precision of `time(7)`. The declared scale lives in [`SqlType::Time`](crate::SqlType::Time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Time {
    /// 100-nanosecond ticks since midnight.
    pub ticks_100ns: u64,
}

/// SQL Server `datetime` (and `smalldatetime`, with whole minutes): days since
/// 1900-01-01 and 1/300 s ticks since midnight, the storage layout of `datetime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateTime {
    /// Days since 1900-01-01 (negative before).
    pub days: i32,
    /// Ticks of 1/300 s since midnight.
    pub ticks_300th: u32,
}

/// SQL Server `datetime2(s)`: a [`Date`] and a [`Time`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateTime2 {
    /// The calendar date.
    pub date: Date,
    /// The time of day.
    pub time: Time,
}

/// SQL Server `datetimeoffset(s)`: an instant in UTC plus the offset of the original
/// time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateTimeOffset {
    /// The instant, expressed in UTC.
    pub utc: DateTime2,
    /// Offset from UTC in minutes, `-840..=840`.
    pub offset_minutes: i16,
}

/// A SQL Server value in memory.
///
/// Equality is **structural** (handy in tests); it is not SQL comparison, which is
/// three-valued and collation-aware (`compare`). No `Eq` because of `f32`/`f64`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL Server `NULL`, whatever the type.
    Null,
    /// SQL Server `bit`.
    Bit(bool),
    /// SQL Server `tinyint` (unsigned, `0..=255`; the name `I8` is kept from the module plan).
    I8(u8),
    /// SQL Server `smallint`.
    I16(i16),
    /// SQL Server `int`.
    I32(i32),
    /// SQL Server `bigint`.
    I64(i64),
    /// SQL Server `decimal` / `numeric`.
    Decimal(Decimal),
    /// SQL Server `float`.
    F64(f64),
    /// SQL Server `real`.
    F32(f32),
    /// SQL Server `money` and `smallmoney`: the amount multiplied by 10 000 (four decimals).
    Money(i64),
    /// SQL Server `char`, `varchar`, `nchar`, `nvarchar`.
    String(SqlString),
    /// SQL Server `binary`, `varbinary`.
    Bytes(Vec<u8>),
    /// SQL Server `date`.
    Date(Date),
    /// SQL Server `time`.
    Time(Time),
    /// SQL Server `datetime` and `smalldatetime`.
    DateTime(DateTime),
    /// SQL Server `datetime2`.
    DateTime2(DateTime2),
    /// SQL Server `datetimeoffset`.
    DateTimeOffset(DateTimeOffset),
    /// SQL Server `uniqueidentifier`: the 16 bytes in the order SQL Server stores and
    /// transmits them (the first three groups little-endian). Textual display is a
    /// conversion (`convert::binary`).
    Guid([u8; 16]),
}
