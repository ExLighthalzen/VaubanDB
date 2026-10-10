use super::*;
use vauban_types::{Decimal, Len, SqlString, SqlType};

fn text(t: &str) -> Value {
    Value::String(SqlString { text: t.to_owned() })
}

fn varchar_under(name: &str) -> TypeInfo {
    let mut ty = TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true);
    ty.collation = Some(Collation::parse(name).expect("a known collation"));
    ty
}

fn decimal(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(Decimal {
        mantissa,
        precision,
        scale,
    })
}

fn key(ty: &TypeInfo, values: &[Value]) -> Vec<u8> {
    let mut encoder = KeyEncoder::new(std::iter::repeat_n(ty, values.len()));
    encoder.encode(values).expect("a key").to_vec()
}

#[test]
fn a_case_insensitive_collation_folds_the_case_into_one_key() {
    let ci = varchar_under("SQL_Latin1_General_CP1_CI_AS");
    assert_eq!(key(&ci, &[text("x")]), key(&ci, &[text("X ")]));
    assert_ne!(key(&ci, &[text("x")]), key(&ci, &[text("y")]));
}

#[test]
fn a_case_sensitive_collation_keeps_the_case_apart() {
    let cs = varchar_under("Latin1_General_CS_AS");
    assert_ne!(key(&cs, &[text("x")]), key(&cs, &[text("X")]));
}

#[test]
fn equal_numbers_of_other_widths_and_scales_share_a_key() {
    let dec = TypeInfo::new(
        SqlType::Decimal {
            precision: 6,
            scale: 2,
        },
        true,
    );
    assert_eq!(
        key(&dec, &[decimal(10, 5, 1)]),
        key(&dec, &[decimal(100, 6, 2)])
    );
    assert_ne!(
        key(&dec, &[decimal(10, 5, 1)]),
        key(&dec, &[decimal(11, 5, 1)])
    );
    let float = TypeInfo::new(SqlType::Float, true);
    assert_eq!(
        key(&float, &[Value::F64(-0.0)]),
        key(&float, &[Value::F64(0.0)])
    );
    let int = TypeInfo::new(SqlType::Int, true);
    assert_eq!(key(&int, &[Value::I8(3)]), key(&int, &[Value::I64(3)]));
}

#[test]
fn trailing_zero_bytes_do_not_separate_two_binaries() {
    let bin = TypeInfo::new(SqlType::VarBinary(Len::Fixed(4)), true);
    assert_eq!(
        key(&bin, &[Value::Bytes(vec![1])]),
        key(&bin, &[Value::Bytes(vec![1, 0])])
    );
}

/// Each column keeps its end, so moving a character from one column to the next makes a
/// different key.
#[test]
fn keys_of_several_columns_do_not_run_into_each_other() {
    let ci = varchar_under("SQL_Latin1_General_CP1_CI_AS");
    assert_ne!(
        key(&ci, &[text("ab"), text("c")]),
        key(&ci, &[text("a"), text("bc")])
    );
}

#[test]
fn null_keys_are_equal_to_each_other_and_to_no_value() {
    let int = TypeInfo::new(SqlType::Int, true);
    assert_eq!(key(&int, &[Value::Null]), key(&int, &[Value::Null]));
    assert_ne!(key(&int, &[Value::Null]), key(&int, &[Value::I32(0)]));
}

#[test]
fn values_come_out_in_first_insertion_order() {
    let ci = varchar_under("SQL_Latin1_General_CP1_CI_AS");
    let mut encoder = KeyEncoder::new([&ci]);
    let mut table: HashTable<Vec<&str>> = HashTable::new();
    for word in ["y", "x", "X", "Y", "z"] {
        let value = text(word);
        let k = encoder.encode([&value]).expect("a key");
        let (members, _) = table.entry(k, || Ok(Vec::new())).expect("an entry");
        members.push(word);
    }
    assert_eq!(
        table.into_values(),
        vec![vec!["y", "Y"], vec!["x", "X"], vec!["z"]]
    );
}

#[test]
fn a_failing_constructor_leaves_the_table_unchanged() {
    let mut table: HashTable<u8> = HashTable::new();
    let error = vauban_errors::SqlError::from(vauban_errors::InternalError::Bug("no".into()));
    assert!(table.entry(b"k", || Err(error)).is_err());
    assert!(table.is_empty());
    assert_eq!(table.find(b"k"), None);
}

#[test]
fn a_key_set_answers_whether_it_saw_a_key() {
    let mut set = KeySet::new();
    assert!(set.insert(b"a"));
    assert!(!set.insert(b"a"));
    assert!(set.contains(b"a"));
    assert!(!set.contains(b"b"));
}
