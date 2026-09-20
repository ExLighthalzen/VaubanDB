use super::{SYSNAME_LEN, resolve_data_type, resolve_sysname_cast};
use vauban_parser::{DataType, Span, TypeArg};
use vauban_types::{Len, SqlType};

/// A span the tests do not look at: `resolve_data_type` reports a position,
/// not a line.
fn span() -> Span {
    Span {
        line: 1,
        column: 1,
        offset: 0,
        len: 1,
    }
}

/// The `DataType` the parser produces for `name` and `args`, built by hand
/// from the same two pieces the parser stores (the name as written, the
/// arguments in order).
fn ty(name: &str, args: &[TypeArg]) -> vauban_errors::SqlResult<SqlType> {
    resolve_data_type(
        &DataType {
            name: name.to_owned(),
            args: args.to_vec(),
            span: span(),
        },
        1,
    )
}

/// Same, for the types written without arguments.
fn simple(name: &str) -> vauban_errors::SqlResult<SqlType> {
    ty(name, &[])
}

#[test]
fn resolves_simple_types() {
    assert_eq!(simple("int"), Ok(SqlType::Int));
    assert_eq!(simple("INT"), Ok(SqlType::Int));
    assert_eq!(simple("Int"), Ok(SqlType::Int));
    assert_eq!(simple("bigint"), Ok(SqlType::BigInt));
    assert_eq!(simple("smallint"), Ok(SqlType::SmallInt));
    assert_eq!(simple("tinyint"), Ok(SqlType::TinyInt));
    assert_eq!(simple("bit"), Ok(SqlType::Bit));
    assert_eq!(simple("money"), Ok(SqlType::Money));
    assert_eq!(simple("smallmoney"), Ok(SqlType::SmallMoney));
    assert_eq!(simple("date"), Ok(SqlType::Date));
    assert_eq!(simple("datetime"), Ok(SqlType::DateTime));
    assert_eq!(simple("smalldatetime"), Ok(SqlType::SmallDateTime));
    assert_eq!(simple("uniqueidentifier"), Ok(SqlType::UniqueIdentifier));
    assert_eq!(simple("real"), Ok(SqlType::Real));
    assert_eq!(
        simple("sysname"),
        Ok(SqlType::NVarChar(Len::Fixed(SYSNAME_LEN)))
    );
    assert_eq!(
        simple("SYSNAME"),
        Ok(SqlType::NVarChar(Len::Fixed(SYSNAME_LEN)))
    );
}

#[test]
fn resolves_parameterised_types() {
    assert_eq!(
        ty("varchar", &[TypeArg::Number(10)]),
        Ok(SqlType::VarChar(Len::Fixed(10)))
    );
    assert_eq!(
        ty("varchar", &[TypeArg::Max]),
        Ok(SqlType::VarChar(Len::Max))
    );
    // `nvarchar(MAX)`: the parser normalises the keyword into `TypeArg::Max`,
    // so the binder does not see the spelling.
    assert_eq!(
        ty("nvarchar", &[TypeArg::Max]),
        Ok(SqlType::NVarChar(Len::Max))
    );
    assert_eq!(
        ty("varbinary", &[TypeArg::Max]),
        Ok(SqlType::VarBinary(Len::Max))
    );
    assert_eq!(
        ty("decimal", &[TypeArg::Number(18), TypeArg::Number(2)]),
        Ok(SqlType::Decimal {
            precision: 18,
            scale: 2
        })
    );
    assert_eq!(
        ty("numeric", &[TypeArg::Number(38)]),
        Ok(SqlType::Numeric {
            precision: 38,
            scale: 0
        })
    );
    assert_eq!(
        ty("datetime2", &[TypeArg::Number(3)]),
        Ok(SqlType::DateTime2(3))
    );
    assert_eq!(ty("time", &[TypeArg::Number(7)]), Ok(SqlType::Time(7)));
    assert_eq!(ty("time", &[TypeArg::Number(0)]), Ok(SqlType::Time(0)));
    assert_eq!(
        ty("datetimeoffset", &[TypeArg::Number(2)]),
        Ok(SqlType::DateTimeOffset(2))
    );
    assert_eq!(
        ty("char", &[TypeArg::Number(8000)]),
        Ok(SqlType::Char(Len::Fixed(8000)))
    );
    assert_eq!(
        ty("nchar", &[TypeArg::Number(4000)]),
        Ok(SqlType::NChar(Len::Fixed(4000)))
    );
    assert_eq!(
        ty("binary", &[TypeArg::Number(4)]),
        Ok(SqlType::Binary(Len::Fixed(4)))
    );
}

#[test]
fn applies_declaration_defaults() {
    assert_eq!(simple("varchar"), Ok(SqlType::VarChar(Len::Fixed(1))));
    assert_eq!(simple("char"), Ok(SqlType::Char(Len::Fixed(1))));
    assert_eq!(simple("nchar"), Ok(SqlType::NChar(Len::Fixed(1))));
    assert_eq!(simple("nvarchar"), Ok(SqlType::NVarChar(Len::Fixed(1))));
    assert_eq!(simple("binary"), Ok(SqlType::Binary(Len::Fixed(1))));
    assert_eq!(simple("varbinary"), Ok(SqlType::VarBinary(Len::Fixed(1))));
    assert_eq!(
        simple("decimal"),
        Ok(SqlType::Decimal {
            precision: 18,
            scale: 0
        })
    );
    assert_eq!(
        simple("numeric"),
        Ok(SqlType::Numeric {
            precision: 18,
            scale: 0
        })
    );
    assert_eq!(
        ty("decimal", &[TypeArg::Number(5)]),
        Ok(SqlType::Decimal {
            precision: 5,
            scale: 0
        })
    );
    assert_eq!(simple("datetime2"), Ok(SqlType::DateTime2(7)));
    assert_eq!(simple("time"), Ok(SqlType::Time(7)));
    assert_eq!(simple("datetimeoffset"), Ok(SqlType::DateTimeOffset(7)));
    assert_eq!(simple("float"), Ok(SqlType::Float));
    assert_eq!(ty("float", &[TypeArg::Number(1)]), Ok(SqlType::Real));
    assert_eq!(ty("float", &[TypeArg::Number(24)]), Ok(SqlType::Real));
    assert_eq!(ty("float", &[TypeArg::Number(25)]), Ok(SqlType::Float));
    assert_eq!(ty("float", &[TypeArg::Number(53)]), Ok(SqlType::Float));
}

#[test]
fn resolves_iso_synonyms() {
    assert_eq!(
        ty("dec", &[TypeArg::Number(5), TypeArg::Number(2)]),
        Ok(SqlType::Decimal {
            precision: 5,
            scale: 2
        })
    );
    assert_eq!(simple("integer"), Ok(SqlType::Int));
    assert_eq!(simple("double precision"), Ok(SqlType::Float));
    assert_eq!(simple("DOUBLE PRECISION"), Ok(SqlType::Float));
    assert_eq!(
        ty("character", &[TypeArg::Number(10)]),
        Ok(SqlType::Char(Len::Fixed(10)))
    );
    assert_eq!(
        ty("char varying", &[TypeArg::Number(10)]),
        Ok(SqlType::VarChar(Len::Fixed(10)))
    );
    assert_eq!(
        ty("character varying", &[TypeArg::Number(10)]),
        Ok(SqlType::VarChar(Len::Fixed(10)))
    );
    assert_eq!(
        ty("national char", &[TypeArg::Number(2)]),
        Ok(SqlType::NChar(Len::Fixed(2)))
    );
    assert_eq!(
        ty("national character", &[TypeArg::Number(2)]),
        Ok(SqlType::NChar(Len::Fixed(2)))
    );
    assert_eq!(
        ty("national char varying", &[TypeArg::Number(10)]),
        Ok(SqlType::NVarChar(Len::Fixed(10)))
    );
    assert_eq!(
        ty("national character varying", &[TypeArg::Number(10)]),
        Ok(SqlType::NVarChar(Len::Fixed(10)))
    );
    assert_eq!(
        ty("binary varying", &[TypeArg::Number(4)]),
        Ok(SqlType::VarBinary(Len::Fixed(4)))
    );
}

#[test]
fn sysname_with_a_length_is_refused_in_a_declaration() {
    let err = ty("sysname", &[TypeArg::Number(10)]).expect_err("no length in a declaration");
    assert_eq!(err.number, 2715);
}

#[test]
fn sysname_cast_accepts_a_length() {
    assert_eq!(
        resolve_sysname_cast(
            &DataType {
                name: "sysname".to_owned(),
                args: vec![TypeArg::Number(10)],
                span: span(),
            },
            1,
        ),
        Ok(SqlType::NVarChar(Len::Fixed(10)))
    );
    assert_eq!(
        resolve_sysname_cast(
            &DataType {
                name: "sysname".to_owned(),
                args: Vec::new(),
                span: span(),
            },
            1,
        ),
        Ok(SqlType::NVarChar(Len::Fixed(SYSNAME_LEN)))
    );
}

#[test]
fn unknown_type_is_2715() {
    let err = simple("foo").expect_err("foo is not a type");
    assert_eq!(err.number, 2715);
    assert_eq!(err.severity, 16);
    assert_eq!(
        err.message,
        "Column, parameter or variable #1: unknown data type foo."
    );

    // A qualified name arrives in one piece and is echoed as written.
    let err = simple("dbo.MonType").expect_err("user types are not resolved");
    assert_eq!(err.number, 2715);
    assert_eq!(
        err.message,
        "Column, parameter or variable #1: unknown data type dbo.MonType."
    );

    // A type SQL Server has and VaubanDB does not: same number, a deliberate
    // difference.
    for name in [
        "sql_variant",
        "xml",
        "text",
        "ntext",
        "image",
        "hierarchyid",
        "geometry",
        "geography",
        "timestamp",
        "rowversion",
        "table",
        "cursor",
    ] {
        let err = simple(name).expect_err("not a type VaubanDB serves");
        assert_eq!(err.number, 2715, "{name}");
        assert_eq!(
            err.message,
            format!("Column, parameter or variable #1: unknown data type {name}.")
        );
    }
}

#[test]
fn position_is_the_rank_in_the_statement() {
    let err = resolve_data_type(
        &DataType {
            name: "foo".to_owned(),
            args: Vec::new(),
            span: span(),
        },
        3,
    )
    .expect_err("foo is not a type");
    assert_eq!(
        err.message,
        "Column, parameter or variable #3: unknown data type foo."
    );
}

#[test]
fn out_of_bounds_parameters_are_refused() {
    // The number is the 2715 placeholder of `out_of_range`, whose rustdoc
    // lists what SQL Server really answers:
    // 131 for the two lengths, 2717 for `decimal(39,2)`, 192 for
    // `decimal(5,6)`, 1002 for `time(8)` and a syntax error 156 for
    // `char(max)`. Those numbers have no catalogue entry yet.
    let vectors: [(&str, Vec<TypeArg>); 6] = [
        ("varchar", vec![TypeArg::Number(9000)]),
        ("nvarchar", vec![TypeArg::Number(5000)]),
        ("decimal", vec![TypeArg::Number(39), TypeArg::Number(2)]),
        ("decimal", vec![TypeArg::Number(5), TypeArg::Number(6)]),
        ("time", vec![TypeArg::Number(8)]),
        ("char", vec![TypeArg::Max]),
    ];
    for (name, args) in vectors {
        let err = ty(name, &args).expect_err("out of bounds");
        assert_eq!(err.number, 2715, "{name}");
    }

    // The other bounds of the same family, refused the same way.
    assert!(ty("char", &[TypeArg::Number(8001)]).is_err());
    assert!(ty("binary", &[TypeArg::Number(0)]).is_err());
    assert!(ty("nchar", &[TypeArg::Number(4001)]).is_err());
    assert!(ty("varbinary", &[TypeArg::Number(-1)]).is_err());
    assert!(ty("nchar", &[TypeArg::Max]).is_err());
    assert!(ty("binary", &[TypeArg::Max]).is_err());
    assert!(ty("decimal", &[TypeArg::Number(0)]).is_err());
    assert!(ty("float", &[TypeArg::Number(0)]).is_err());
    assert!(ty("float", &[TypeArg::Number(54)]).is_err());
    assert!(ty("datetime2", &[TypeArg::Number(8)]).is_err());
    assert!(ty("datetimeoffset", &[TypeArg::Number(-1)]).is_err());
    assert!(ty("int", &[TypeArg::Number(10)]).is_err());
    assert!(ty("varchar", &[TypeArg::Ident("max".to_owned())]).is_err());
    assert!(ty("decimal", &[TypeArg::Max]).is_err());
}

/// The numbers SQL Server answers for the six shapes above. Ignored: 131, 192,
/// 1002 and 2717 are neither in the errors catalogue nor given a constructor.
/// The assertions are written down so that whoever adds them has the test
/// ready.
#[test]
#[ignore = "numbers 131, 192, 1002 and 2717 are missing from the errors catalogue"]
fn out_of_bounds_parameters_use_the_numbers_of_sql_server() {
    assert_eq!(
        ty("varchar", &[TypeArg::Number(9000)])
            .expect_err("out of bounds")
            .number,
        131
    );
    assert_eq!(
        ty("nvarchar", &[TypeArg::Number(5000)])
            .expect_err("out of bounds")
            .number,
        131
    );
    assert_eq!(
        ty("decimal", &[TypeArg::Number(39), TypeArg::Number(2)])
            .expect_err("out of bounds")
            .number,
        2717
    );
    assert_eq!(
        ty("decimal", &[TypeArg::Number(5), TypeArg::Number(6)])
            .expect_err("out of bounds")
            .number,
        192
    );
    assert_eq!(
        ty("time", &[TypeArg::Number(8)])
            .expect_err("out of bounds")
            .number,
        1002
    );
}
