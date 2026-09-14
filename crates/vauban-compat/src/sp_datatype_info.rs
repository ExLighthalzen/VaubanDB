//! The rows of `sys.sp_datatype_info_100` that ODBC 18 asks for during login.

use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

/// One result column, without any dependency on the TDS or session layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    /// Column name, as SQL Server names it.
    pub name: String,
    /// SQL type, length, nullability and collation.
    pub ty: TypeInfo,
}

/// The single result set returned by `sp_datatype_info_100`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSetData {
    /// Result metadata.
    pub columns: Vec<ResultColumn>,
    /// Rows.
    pub rows: Vec<Vec<Value>>,
}

#[derive(Clone, Copy)]
enum RawValue {
    Null,
    I16(i16),
    I32(i32),
    Text(&'static str),
}

// The rows SQL Server 2022 answers to `EXEC sys.sp_datatype_info_100 12, @ODBCVer = 4`.
// This table is deliberately a literal, not derived from `vauban-types`; a catalog-backed
// `sys.types` may replace it.
const VARCHAR_ROWS: &[&[RawValue]] = &[&[
    RawValue::Text("varchar"),
    RawValue::I16(12),
    RawValue::I32(8_000),
    RawValue::Text("'"),
    RawValue::Text("'"),
    RawValue::Text("max length"),
    RawValue::I16(1),
    RawValue::I16(0),
    RawValue::I16(3),
    RawValue::Null,
    RawValue::I16(0),
    RawValue::Null,
    RawValue::Text("varchar"),
    RawValue::Null,
    RawValue::Null,
    RawValue::I16(12),
    RawValue::Null,
    RawValue::Null,
    RawValue::Null,
    RawValue::I16(2),
]];

// The rows of `EXEC sys.sp_datatype_info_100 -9, @ODBCVer = 4`.
const WVARCHAR_ROWS: &[&[RawValue]] = &[
    &[
        RawValue::Text("nvarchar"),
        RawValue::I16(-9),
        RawValue::I32(4_000),
        RawValue::Text("N'"),
        RawValue::Text("'"),
        RawValue::Text("max length"),
        RawValue::I16(1),
        RawValue::I16(0),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::I16(0),
        RawValue::Null,
        RawValue::Text("nvarchar"),
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(-9),
        RawValue::Null,
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(0),
    ],
    &[
        RawValue::Text("sysname"),
        RawValue::I16(-9),
        RawValue::I32(128),
        RawValue::Text("N'"),
        RawValue::Text("'"),
        RawValue::Null,
        RawValue::I16(0),
        RawValue::I16(0),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::I16(0),
        RawValue::Null,
        RawValue::Text("sysname"),
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(-9),
        RawValue::Null,
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(18),
    ],
];

// The rows of `EXEC sys.sp_datatype_info_100 -3, @ODBCVer = 4`.
const VARBINARY_ROWS: &[&[RawValue]] = &[&[
    RawValue::Text("varbinary"),
    RawValue::I16(-3),
    RawValue::I32(8_000),
    RawValue::Text("0x"),
    RawValue::Null,
    RawValue::Text("max length"),
    RawValue::I16(1),
    RawValue::I16(0),
    RawValue::I16(2),
    RawValue::Null,
    RawValue::I16(0),
    RawValue::Null,
    RawValue::Text("varbinary"),
    RawValue::Null,
    RawValue::Null,
    RawValue::I16(-3),
    RawValue::Null,
    RawValue::Null,
    RawValue::Null,
    RawValue::I16(4),
]];

// The rows of `EXEC sys.sp_datatype_info_100 93, @ODBCVer = 4`.
const TIMESTAMP_ROWS: &[&[RawValue]] = &[
    &[
        RawValue::Text("datetime2"),
        RawValue::I16(93),
        RawValue::I32(27),
        RawValue::Text("'"),
        RawValue::Text("'"),
        RawValue::Text("scale"),
        RawValue::I16(1),
        RawValue::I16(0),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::I16(0),
        RawValue::Null,
        RawValue::Text("datetime2"),
        RawValue::I16(0),
        RawValue::I16(7),
        RawValue::I16(9),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(0),
    ],
    &[
        RawValue::Text("datetime"),
        RawValue::I16(93),
        RawValue::I32(23),
        RawValue::Text("'"),
        RawValue::Text("'"),
        RawValue::Null,
        RawValue::I16(1),
        RawValue::I16(0),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::I16(0),
        RawValue::Null,
        RawValue::Text("datetime"),
        RawValue::I16(3),
        RawValue::I16(3),
        RawValue::I16(9),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(12),
    ],
    &[
        RawValue::Text("smalldatetime"),
        RawValue::I16(93),
        RawValue::I32(16),
        RawValue::Text("'"),
        RawValue::Text("'"),
        RawValue::Null,
        RawValue::I16(1),
        RawValue::I16(0),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::I16(0),
        RawValue::Null,
        RawValue::Text("smalldatetime"),
        RawValue::I16(0),
        RawValue::I16(0),
        RawValue::I16(9),
        RawValue::I16(3),
        RawValue::Null,
        RawValue::Null,
        RawValue::I16(22),
    ],
];

fn columns() -> Vec<ResultColumn> {
    let nvarchar = |name: &str, nullable| ResultColumn {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), nullable),
    };
    let varchar = |name: &str| {
        let mut ty = TypeInfo::new(SqlType::VarChar(Len::Fixed(32)), true);
        ty.collation = Some(
            Collation::parse("Latin1_General_CI_AI")
                .expect("the collation of the system procedure is supported"),
        );
        ResultColumn {
            name: name.to_owned(),
            ty,
        }
    };
    let smallint = |name: &str, nullable| ResultColumn {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::SmallInt, nullable),
    };
    let int = |name: &str| ResultColumn {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
    };

    vec![
        nvarchar("TYPE_NAME", true),
        smallint("DATA_TYPE", false),
        int("PRECISION"),
        varchar("LITERAL_PREFIX"),
        varchar("LITERAL_SUFFIX"),
        varchar("CREATE_PARAMS"),
        smallint("NULLABLE", true),
        smallint("CASE_SENSITIVE", true),
        smallint("SEARCHABLE", false),
        smallint("UNSIGNED_ATTRIBUTE", true),
        smallint("MONEY", false),
        smallint("AUTO_INCREMENT", true),
        nvarchar("LOCAL_TYPE_NAME", true),
        smallint("MINIMUM_SCALE", true),
        smallint("MAXIMUM_SCALE", true),
        smallint("SQL_DATA_TYPE", false),
        smallint("SQL_DATETIME_SUB", true),
        int("NUM_PREC_RADIX"),
        smallint("INTERVAL_PRECISION", true),
        smallint("USERTYPE", true),
    ]
}

fn rows(raw: &[&[RawValue]]) -> Vec<Vec<Value>> {
    raw.iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    RawValue::Null => Value::Null,
                    RawValue::I16(value) => Value::I16(*value),
                    RawValue::I32(value) => Value::I32(*value),
                    RawValue::Text(value) => Value::String(SqlString {
                        text: (*value).to_owned(),
                    }),
                })
                .collect()
        })
        .collect()
}

/// Answers one ODBC data-type code with the rows SQL Server 2022 returns for it.
///
/// ODBC 18 sends `odbc_ver = 4`. The procedure answers the same shape for its default
/// version, so the argument is accepted and deliberately ignored.
pub fn sp_datatype_info_100(data_type: i32, _odbc_ver: i32) -> ResultSetData {
    let raw = match data_type {
        12 => VARCHAR_ROWS,
        -9 => WVARCHAR_ROWS,
        -3 => VARBINARY_ROWS,
        93 => TIMESTAMP_ROWS,
        _ => &[],
    };
    ResultSetData {
        columns: columns(),
        rows: rows(raw),
    }
}
