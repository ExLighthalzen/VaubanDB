//! Integration tests of `implicit_result_type`: the common type of two operands.
//!
//! The order of `rank` itself is a unit test of `src/precedence.rs` (the function is
//! `pub(crate)`); what a caller outside the crate sees is only the common type, so that is
//! what this file exercises.

use vauban_types::{Collation, Len, SqlType, TypeInfo, implicit_result_type};

/// A non-nullable operand with the default collation when it is a character type.
fn info(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, false)
}

/// The common type of two non-nullable operands, which must exist.
fn common(a: SqlType, b: SqlType) -> SqlType {
    implicit_result_type(&info(a), &info(b))
        .expect("the pair has a common type")
        .ty
}

fn numeric(precision: u8, scale: u8) -> SqlType {
    SqlType::Numeric { precision, scale }
}

#[test]
fn implicit_result_type_merges_lengths_and_scales() {
    // Character types: the stronger name, the wider length.
    assert_eq!(
        common(
            SqlType::VarChar(Len::Fixed(10)),
            SqlType::NVarChar(Len::Fixed(5))
        ),
        SqlType::NVarChar(Len::Fixed(10))
    );
    assert_eq!(
        common(
            SqlType::Char(Len::Fixed(3)),
            SqlType::VarChar(Len::Fixed(5))
        ),
        SqlType::VarChar(Len::Fixed(5))
    );
    assert_eq!(
        common(SqlType::VarChar(Len::Fixed(10)), SqlType::VarChar(Len::Max)),
        SqlType::VarChar(Len::Max)
    );
    assert_eq!(
        common(
            SqlType::VarBinary(Len::Fixed(4)),
            SqlType::VarBinary(Len::Fixed(8))
        ),
        SqlType::VarBinary(Len::Fixed(8))
    );

    // Exact numerics: s = max(s1, s2), p = min(38, max(p1 - s1, p2 - s2) + s).
    assert_eq!(common(numeric(5, 2), numeric(3, 3)), numeric(6, 3));
    // `int` counts as numeric(10, 0): s = 1, p = max(10, 1) + 1 = 11.
    assert_eq!(common(SqlType::Int, numeric(2, 1)), numeric(11, 1));

    // Two identical types give that type, parameters included.
    assert_eq!(common(numeric(2, 1), numeric(2, 1)), numeric(2, 1));
    assert_eq!(
        common(SqlType::Char(Len::Fixed(3)), SqlType::Char(Len::Fixed(3))),
        SqlType::Char(Len::Fixed(3))
    );
    assert_eq!(common(SqlType::Date, SqlType::Date), SqlType::Date);

    // A `decimal` and a `numeric` share a rank; the pair reads `numeric`.
    let decimal = SqlType::Decimal {
        precision: 5,
        scale: 2,
    };
    assert_eq!(common(decimal, numeric(3, 1)), numeric(5, 2));
    assert_eq!(common(numeric(3, 1), decimal), numeric(5, 2));
    // A lone `decimal` keeps its name.
    assert_eq!(
        common(decimal, SqlType::Int),
        SqlType::Decimal {
            precision: 12,
            scale: 2
        }
    );

    // Two temporal types of the same rank keep the wider fractional-seconds scale.
    assert_eq!(
        common(SqlType::DateTime2(3), SqlType::DateTime2(7)),
        SqlType::DateTime2(7)
    );
}

#[test]
fn implicit_result_type_refuses_impossible_pairs() {
    // 206, state 2, naming the two types (the literal `1` is a `tinyint`, hence the type
    // named in the message).
    let guid = implicit_result_type(&info(SqlType::UniqueIdentifier), &info(SqlType::Int))
        .expect_err("uniqueidentifier and int have no common type");
    assert_eq!(guid.number, 206);
    assert_eq!(guid.severity, 16);
    assert_eq!(guid.state, 2);
    // The operands are named in the order the caller passed them, as in the query.
    assert_eq!(
        guid.message,
        "Type mismatch: uniqueidentifier cannot be combined with int."
    );

    let date = implicit_result_type(&info(SqlType::Date), &info(SqlType::TinyInt))
        .expect_err("date and tinyint have no common type");
    assert_eq!(date.number, 206);
    assert_eq!(
        date.message,
        "Type mismatch: date cannot be combined with tinyint."
    );

    // The order of the operands does not change the verdict.
    assert!(implicit_result_type(&info(SqlType::Int), &info(SqlType::UniqueIdentifier)).is_err());
    assert!(implicit_result_type(&info(SqlType::Int), &info(SqlType::Date)).is_err());

    // Neither does the exact type on each side.
    assert!(implicit_result_type(&info(SqlType::Time(7)), &info(SqlType::Float)).is_err());
    assert!(
        implicit_result_type(&info(SqlType::DateTimeOffset(7)), &info(SqlType::Money)).is_err()
    );
    assert!(implicit_result_type(&info(SqlType::UniqueIdentifier), &info(SqlType::Date)).is_err());

    // `datetime` and `smalldatetime` do convert to numbers: `GETDATE() + 1` is legal.
    assert_eq!(common(SqlType::DateTime, SqlType::Int), SqlType::DateTime);
    assert_eq!(
        common(SqlType::SmallDateTime, SqlType::Int),
        SqlType::SmallDateTime
    );
    // A character operand reconciles with every type.
    assert_eq!(
        common(SqlType::VarChar(Len::Fixed(36)), SqlType::UniqueIdentifier),
        SqlType::UniqueIdentifier
    );
    assert_eq!(
        common(SqlType::VarChar(Len::Fixed(10)), SqlType::Date),
        SqlType::Date
    );
}

#[test]
fn nullability_and_collation_propagate() {
    let nullable = TypeInfo::new(SqlType::Int, true);
    let not_null = TypeInfo::new(SqlType::Int, false);

    assert!(
        implicit_result_type(&nullable, &not_null)
            .expect("two ints have a common type")
            .nullable
    );
    assert!(
        implicit_result_type(&not_null, &nullable)
            .expect("two ints have a common type")
            .nullable
    );
    assert!(
        !implicit_result_type(&not_null, &not_null)
            .expect("two ints have a common type")
            .nullable
    );
    assert!(
        implicit_result_type(&nullable, &nullable)
            .expect("two ints have a common type")
            .nullable
    );

    // A character result carries the collation of the left operand when both have one.
    let case_sensitive =
        Collation::parse("SQL_Latin1_General_CP1_CS_AS").expect("a collation the server knows");
    assert_ne!(case_sensitive, Collation::DEFAULT);
    let left = TypeInfo {
        ty: SqlType::VarChar(Len::Fixed(10)),
        nullable: false,
        collation: Some(case_sensitive),
    };
    let right = TypeInfo::new(SqlType::NVarChar(Len::Fixed(5)), false);
    assert_eq!(right.collation, Some(Collation::DEFAULT));

    let merged = implicit_result_type(&left, &right).expect("two strings have a common type");
    assert_eq!(merged.ty, SqlType::NVarChar(Len::Fixed(10)));
    assert_eq!(merged.collation, Some(case_sensitive));

    let merged = implicit_result_type(&right, &left).expect("two strings have a common type");
    assert_eq!(merged.collation, Some(Collation::DEFAULT));

    // A non-character result carries no collation, even when an operand is a string.
    let merged = implicit_result_type(&left, &TypeInfo::new(SqlType::Int, false))
        .expect("varchar and int have a common type");
    assert_eq!(merged.ty, SqlType::Int);
    assert_eq!(merged.collation, None);

    // The collation of the only character operand wins, left or right.
    let merged = implicit_result_type(&TypeInfo::new(SqlType::Binary(Len::Fixed(4)), false), &left)
        .expect("binary and varchar have a common type");
    assert_eq!(merged.ty, SqlType::VarChar(Len::Fixed(10)));
    assert_eq!(merged.collation, Some(case_sensitive));
}
