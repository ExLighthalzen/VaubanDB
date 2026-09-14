//! The `Operator` trait, the dispatch that builds one operator per node of the physical
//! plan, and the two helpers the hashing operators share.
//!
//! # Volcano
//!
//! An operator is opened once, asked for its rows one at a time until it answers `None`,
//! then closed. The driving loop of a query (`statement.rs`) is the one caller that
//! opens the root; each operator opens, drives and closes its own inputs. A row is
//! produced only when asked for: `Top` over a `Project` over a division by zero asks for
//! nothing once its budget is spent, so `SELECT TOP (0) 1 / 0;` raises nothing
//! (`tests/execute_select.rs`) and `Top 1` over three rows pulls one row and no more
//! (`tests/operator_basics.rs`, `limit_stops_early`).
//!
//! # The lifetime
//!
//! `Operator<'a>` is parameterised by the lifetime of the [`ExecContext`] it runs in: a
//! `TableScan` keeps from `open` to `close` the iterator `storage.scan` hands out, and
//! that iterator borrows the storage the context holds. A `Box<dyn Operator<'a> + 'a>`
//! is therefore the shape of an operator tree.
//!
//! # One file per operator
//!
//! [`build_operator`] holds the match on [`PhysicalPlan`] and nothing else: each arm
//! hands the node to the `build` function of the file that owns the operator, so that
//! writing an operator changes its file and not this one. The arms whose operator is not
//! written yet answer the internal error 50000, with a message that names the node
//! (`tests/operator_basics.rs`, `unserved_variant_is_a_bug`).

use std::hash::{Hash, Hasher};

use vauban_binder::OutputSchema;
use vauban_errors::SqlResult;
use vauban_planner::PhysicalPlan;
use vauban_types::{Collation, TypeInfo, Value, compare};

use crate::context::ExecContext;
use crate::ops;
use crate::row::Row;

/// One node of a running plan: opened once, pulled row by row, closed once.
///
/// `'a` is the lifetime of the context the operator runs in (module documentation).
pub trait Operator<'a> {
    /// Prepares the operator: opens its inputs, evaluates what is evaluated once (the
    /// row count of a `Top`), takes the iterator of a scan. Called once before the first
    /// [`Operator::next`].
    ///
    /// # Errors
    ///
    /// What the operator raises before it produces a row: the 127 of a negative `TOP`,
    /// what `storage.scan` refuses, the internal error 50000 of a context without engine.
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()>;

    /// The next row, or `None` once the operator is exhausted. Calling it again after a
    /// `None` answers `None` again.
    ///
    /// # Errors
    ///
    /// What evaluating a row raises, the line of the failing sub-expression already on
    /// it (`crate::errors::at`). The operator produces nothing more after an error.
    fn next(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>>;

    /// Releases what `open` took. Called once, on an operator that was exhausted,
    /// interrupted or not opened (`ops/limit.rs`, a budget of zero rows leaves the input
    /// unopened).
    fn close(&mut self);

    /// The columns of the rows this operator produces, known before it is opened.
    fn schema(&self) -> &OutputSchema;
}

/// Builds the operator tree of `plan`, without opening it.
///
/// # Errors
///
/// The internal error 50000 for a node whose operator is not written yet: the message
/// names the node (`tests/operator_basics.rs`, `unserved_variant_is_a_bug`).
pub fn build_operator<'a>(plan: &PhysicalPlan) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    match plan {
        PhysicalPlan::OneRow => ops::values::build_one_row(),
        PhysicalPlan::Values { rows, schema } => ops::values::build(rows, schema),
        PhysicalPlan::TableScan {
            table,
            columns,
            schema,
            ..
        } => ops::scan::build(*table, columns, schema),
        PhysicalPlan::IndexSeek { .. } => ops::seek::build(plan),
        PhysicalPlan::Filter { input, predicate } => ops::filter::build(input, predicate),
        PhysicalPlan::Project {
            input,
            exprs,
            schema,
        } => ops::project::build(input, exprs, schema),
        PhysicalPlan::Top { input, top } => ops::limit::build(input, top),
        PhysicalPlan::NestedLoopJoin { .. } => ops::nl_join::build(plan),
        PhysicalPlan::HashJoin { .. } => ops::hash_join::build(plan),
        PhysicalPlan::HashAggregate { .. } | PhysicalPlan::StreamAggregate { .. } => {
            ops::aggregate::build(plan)
        }
        PhysicalPlan::Sort { .. } | PhysicalPlan::TopN { .. } | PhysicalPlan::Distinct(_) => {
            ops::sort::build(plan)
        }
        PhysicalPlan::SubqueryEval { .. } => ops::subquery::build(plan),
        PhysicalPlan::Union { .. }
        | PhysicalPlan::Except { .. }
        | PhysicalPlan::Intersect { .. } => ops::setop::build(plan),
    }
}

/// A coarse hash of a value, invariant under the collation: two values that
/// [`keys_equal`] finds equal hash the same, and the hash decides the bucket alone.
///
/// The hash reads the family of the value and, inside a family, what the comparison of
/// `types::compare` reads too, coarsened where that comparison ignores something:
///
/// | family | what is hashed |
/// |---|---|
/// | `NULL` | the family alone |
/// | `bit` | the boolean |
/// | integers | the value widened to `i64`, so `I8(3)` and `I64(3)` hash the same |
/// | `decimal` | the family alone: two decimals of different scales may be equal |
/// | `money` | the amount |
/// | floats | the bits of the value widened to `f64`, `-0.0` folded onto `0.0` |
/// | strings | the number of characters once the trailing spaces are gone: the collation ignores those and the case |
/// | bytes | the bytes once the trailing `0x00` are gone: the comparison pads with them |
/// | `uniqueidentifier` | the 16 bytes |
/// | `date`, `time`, `datetime`, `datetime2` | the fields the comparison orders on |
/// | `datetimeoffset` | the UTC instant alone, which is what the comparison reads |
///
/// A degenerate hash keeps the operators correct and makes them slow: the equality of two
/// keys is decided by [`keys_equal`], never by the hash
/// (`operator::tests::equal_keys_hash_the_same`).
#[must_use]
pub fn bucket_hash(value: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match value {
        Value::Null => 0u8.hash(&mut hasher),
        Value::Bit(b) => (1u8, *b).hash(&mut hasher),
        Value::I8(n) => (2u8, i64::from(*n)).hash(&mut hasher),
        Value::I16(n) => (2u8, i64::from(*n)).hash(&mut hasher),
        Value::I32(n) => (2u8, i64::from(*n)).hash(&mut hasher),
        Value::I64(n) => (2u8, *n).hash(&mut hasher),
        Value::Decimal(_) => 3u8.hash(&mut hasher),
        Value::Money(amount) => (4u8, *amount).hash(&mut hasher),
        Value::F32(f) => (5u8, float_bits(f64::from(*f))).hash(&mut hasher),
        Value::F64(f) => (5u8, float_bits(*f)).hash(&mut hasher),
        Value::String(s) => (6u8, s.text.trim_end_matches(' ').chars().count()).hash(&mut hasher),
        Value::Bytes(bytes) => {
            let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
            (7u8, &bytes[..end]).hash(&mut hasher);
        }
        Value::Guid(bytes) => (8u8, bytes).hash(&mut hasher),
        Value::Date(d) => (9u8, d.days).hash(&mut hasher),
        Value::Time(t) => (10u8, t.ticks_100ns).hash(&mut hasher),
        Value::DateTime(dt) => (11u8, dt.days, dt.ticks_300th).hash(&mut hasher),
        Value::DateTime2(dt) => (12u8, dt.date.days, dt.time.ticks_100ns).hash(&mut hasher),
        Value::DateTimeOffset(dto) => {
            (13u8, dto.utc.date.days, dto.utc.time.ticks_100ns).hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// The bits of a float for hashing, with the negative zero folded onto the positive one:
/// the two compare equal and must hash the same.
fn float_bits(f: f64) -> u64 {
    if f == 0.0 {
        0.0f64.to_bits()
    } else {
        f.to_bits()
    }
}

/// Whether two keys of type `ty` are equal, under the collation of `ty` for a string.
///
/// The rule is `types::compare`'s: `NULL` is equal to nothing, `NULL` included
/// (`operator::tests::null_keys_are_never_equal`), trailing spaces and case do not
/// separate two strings, and two values of different families are an internal error,
/// because the caller was to convert both sides to `ty` first.
///
/// # Errors
///
/// The internal error 50000 of `types::compare` for two values of different families.
pub fn keys_equal(a: &Value, b: &Value, ty: &TypeInfo) -> SqlResult<bool> {
    let collation = ty.collation.as_ref().unwrap_or(&Collation::DEFAULT);
    Ok(matches!(
        compare(a, b, collation)?,
        Some(std::cmp::Ordering::Equal)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_types::{SqlString, SqlType};

    fn text(t: &str) -> Value {
        Value::String(SqlString { text: t.to_owned() })
    }

    fn varchar() -> TypeInfo {
        TypeInfo::new(SqlType::VarChar(vauban_types::Len::Fixed(10)), true)
    }

    /// The invariant the hashing operators rely on: what `keys_equal` finds equal hashes
    /// the same, across the widths of a family and across what the collation ignores.
    #[test]
    fn equal_keys_hash_the_same() {
        let int = TypeInfo::new(SqlType::Int, true);
        let pairs: [(Value, Value, TypeInfo); 6] = [
            (Value::I8(3), Value::I64(3), int.clone()),
            (Value::I16(-1), Value::I32(-1), int.clone()),
            (
                Value::F32(1.5),
                Value::F64(1.5),
                TypeInfo::new(SqlType::Float, true),
            ),
            (
                Value::F64(0.0),
                Value::F64(-0.0),
                TypeInfo::new(SqlType::Float, true),
            ),
            (text("abc"), text("ABC  "), varchar()),
            (
                Value::Bytes(vec![1]),
                Value::Bytes(vec![1, 0, 0]),
                TypeInfo::new(SqlType::VarBinary(vauban_types::Len::Fixed(10)), true),
            ),
        ];
        for (a, b, ty) in &pairs {
            assert!(keys_equal(a, b, ty).expect("same family"), "{a:?} = {b:?}");
            assert_eq!(bucket_hash(a), bucket_hash(b), "{a:?} and {b:?}");
        }
    }

    /// The counter-proof: two keys the comparison separates hash apart on the shapes
    /// below, so the hash does read the value and not the family alone.
    #[test]
    fn different_keys_hash_apart_on_these_shapes() {
        let int = TypeInfo::new(SqlType::Int, true);
        assert!(!keys_equal(&Value::I32(1), &Value::I32(2), &int).expect("same family"));
        assert_ne!(bucket_hash(&Value::I32(1)), bucket_hash(&Value::I32(2)));
        assert!(!keys_equal(&text("a"), &text("ab"), &varchar()).expect("same family"));
        assert_ne!(bucket_hash(&text("a")), bucket_hash(&text("ab")));
        assert_ne!(bucket_hash(&Value::I32(1)), bucket_hash(&Value::Null));
    }

    /// `NULL` is equal to nothing, itself included; two `NULL`s still share a bucket.
    #[test]
    fn null_keys_are_never_equal() {
        let int = TypeInfo::new(SqlType::Int, true);
        assert!(!keys_equal(&Value::Null, &Value::Null, &int).expect("NULL compares"));
        assert!(!keys_equal(&Value::Null, &Value::I32(1), &int).expect("NULL compares"));
        assert_eq!(bucket_hash(&Value::Null), bucket_hash(&Value::Null));
    }

    /// Two values of different families are the internal error of `types::compare`, not
    /// `false`.
    #[test]
    fn keys_of_different_families_are_a_bug() {
        let int = TypeInfo::new(SqlType::Int, true);
        let error = keys_equal(&Value::I32(1), &text("1"), &int).unwrap_err();
        assert_eq!(error.number, 50000);
    }
}
