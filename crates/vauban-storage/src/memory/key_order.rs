//! The key comparator of `MemoryStorage`: orders index keys and clustered keys exactly as
//! "Key order" on [`TableShape`] prescribes, and hosts the fallible binary search the
//! sorted structures of the module rely on.
//!
//! Value comparison itself is never reimplemented here: one column is compared by
//! [`vauban_types::compare`] with the collation of the column's [`TypeInfo`]. This module
//! only adds what the storage layer owns: `NULL` first (`NULL` equal to `NULL`), the
//! `descending` reversal, the column-by-column composition and the type/variant check of
//! the keys handed to [`crate::Storage::seek`].

use std::cmp::Ordering;

use vauban_errors::{InternalError, SqlResult};
use vauban_types::{Collation, SqlType, TypeInfo, Value, compare};

use crate::{KeyColumn, Row, TableShape};

/// One column of a key as the comparator needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Column {
    /// Position of the column in the row.
    position: usize,
    /// Reverses the order of the column, `NULL` included.
    descending: bool,
    /// The collation handed to `compare`: the column's, or the default one.
    collation: Collation,
    /// The declared type, used to reject a key value of the wrong variant.
    ty: SqlType,
}

/// The comparator of a composite key (index key or clustered key), built once from the
/// [`KeyColumn`]s and the [`TypeInfo`]s of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyOrder {
    columns: Vec<Column>,
}

/// A caller bug: a violated precondition of the [`crate::Storage`] contract.
fn bug<T>(msg: impl Into<String>) -> SqlResult<T> {
    Err(InternalError::Bug(msg.into()).into())
}

impl KeyOrder {
    /// Builds the comparator of `columns` over a table of shape `shape`.
    ///
    /// Checks the preconditions shared by [`crate::Storage::create_table`] (clustered key)
    /// and [`crate::Storage::create_index`]: `columns` is not empty and every position is
    /// `< shape.columns.len()`. `what` names the key in the error message.
    pub(crate) fn new(what: &str, columns: &[KeyColumn], shape: &TableShape) -> SqlResult<Self> {
        if columns.is_empty() {
            return bug(format!("{what} has no column"));
        }
        let mut out = Vec::with_capacity(columns.len());
        for kc in columns {
            let position = usize::from(kc.column);
            let Some(info) = shape.columns.get(position) else {
                return bug(format!(
                    "{what} column {} out of range ({} columns)",
                    kc.column,
                    shape.columns.len()
                ));
            };
            out.push(Column {
                position,
                descending: kc.descending,
                collation: collation_of(info),
                ty: info.ty,
            });
        }
        Ok(Self { columns: out })
    }

    /// The key of `row`: its values at the key positions, in key order. The caller has
    /// checked the arity of `row`; a missing value is reported as a corruption rather than
    /// a panic.
    pub(crate) fn extract(&self, row: &Row) -> SqlResult<Vec<Value>> {
        self.columns
            .iter()
            .map(|c| match row.0.get(c.position) {
                Some(v) => Ok(v.clone()),
                None => Err(InternalError::Corruption(format!(
                    "row has {} values, key column {} is out of range",
                    row.0.len(),
                    c.position
                ))
                .into()),
            })
            .collect()
    }

    /// Compares two keys or key prefixes column by column, over the `min(a.len(), b.len())`
    /// first columns: a prefix is `Equal` to every key that starts with it. Each column
    /// follows "Key order" on [`TableShape`]: `NULL` first and equal to `NULL`, then
    /// [`compare`] with the column's collation, reversed by `descending`.
    ///
    /// # Errors
    ///
    /// The error of [`compare`] for two values of incompatible families, as is.
    pub(crate) fn compare_prefix(&self, a: &[Value], b: &[Value]) -> SqlResult<Ordering> {
        for (column, (x, y)) in self.columns.iter().zip(a.iter().zip(b.iter())) {
            let ordering = compare_column(x, y, &column.collation)?;
            let ordering = if column.descending {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering != Ordering::Equal {
                return Ok(ordering);
            }
        }
        Ok(Ordering::Equal)
    }

    /// Checks a key or key prefix handed by the caller ([`crate::KeyRange`]): at most as
    /// many values as the key has columns, and each non-`NULL` value of the variant family
    /// the column's type expects.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` on either violation.
    pub(crate) fn check_prefix(&self, prefix: &[Value]) -> SqlResult<()> {
        if prefix.len() > self.columns.len() {
            return bug(format!(
                "key prefix has {} values, the key has {} columns",
                prefix.len(),
                self.columns.len()
            ));
        }
        for (i, (column, value)) in self.columns.iter().zip(prefix.iter()).enumerate() {
            if !variant_matches(value, &column.ty) {
                return bug(format!(
                    "key value {i} ({value:?}) does not match the column type {:?}",
                    column.ty
                ));
            }
        }
        Ok(())
    }
}

/// The collation to hand to [`compare`] for a column: its own, or the default one.
fn collation_of(info: &TypeInfo) -> Collation {
    info.collation.unwrap_or(Collation::DEFAULT)
}

/// Compares two values of one key column, `NULL` first: `NULL == NULL`, `NULL <` anything
/// else, otherwise [`compare`] (which answers `Some` for two non-`NULL` values).
fn compare_column(a: &Value, b: &Value, collation: &Collation) -> SqlResult<Ordering> {
    match (a, b) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(Ordering::Less),
        (_, Value::Null) => Ok(Ordering::Greater),
        _ => match compare(a, b, collation)? {
            Some(ordering) => Ok(ordering),
            None => Err(InternalError::Corruption(format!(
                "compare answered UNKNOWN for two non-NULL values {a:?} and {b:?}"
            ))
            .into()),
        },
    }
}

/// Whether `value` belongs to the family of variants a column of type `ty` holds. `NULL`
/// fits every column. The families are those of [`compare`]: a key on an `int` column may
/// be given as any integer variant, which is what the executor produces after an implicit
/// conversion.
fn variant_matches(value: &Value, ty: &SqlType) -> bool {
    match value {
        Value::Null => true,
        Value::Bit(_) => matches!(ty, SqlType::Bit),
        Value::I8(_) | Value::I16(_) | Value::I32(_) | Value::I64(_) => matches!(
            ty,
            SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::BigInt
        ),
        Value::Decimal(_) => ty.is_exact_numeric(),
        Value::F32(_) | Value::F64(_) => matches!(ty, SqlType::Float | SqlType::Real),
        Value::Money(_) => matches!(ty, SqlType::Money | SqlType::SmallMoney),
        Value::String(_) => ty.is_string(),
        Value::Bytes(_) => matches!(ty, SqlType::Binary(_) | SqlType::VarBinary(_)),
        Value::Date(_) => matches!(ty, SqlType::Date),
        Value::Time(_) => matches!(ty, SqlType::Time(_)),
        Value::DateTime(_) => matches!(ty, SqlType::DateTime | SqlType::SmallDateTime),
        Value::DateTime2(_) => matches!(ty, SqlType::DateTime2(_)),
        Value::DateTimeOffset(_) => matches!(ty, SqlType::DateTimeOffset(_)),
        Value::Guid(_) => matches!(ty, SqlType::UniqueIdentifier),
    }
}

/// The index of the first element of `items` for which `pred` is `false`, `items.len()` if
/// there is none: [`slice::partition_point`] with a fallible predicate. `pred` must be
/// `true` for a (possibly empty) prefix of `items` and `false` afterwards.
///
/// # Errors
///
/// The first error of `pred`, as is.
pub(crate) fn partition_point<T>(
    items: &[T],
    mut pred: impl FnMut(&T) -> SqlResult<bool>,
) -> SqlResult<usize> {
    let (mut lo, mut hi) = (0, items.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let Some(item) = items.get(mid) else {
            return Err(InternalError::Corruption(format!(
                "binary search index {mid} out of range ({} items)",
                items.len()
            ))
            .into());
        };
        if pred(item)? {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// Sorts `rows` by `order` then by increasing [`crate::RowId`], with a fallible comparator:
/// the standard sort cannot report an error and may panic on an inconsistent order, so the
/// rows are inserted one by one at their binary-search position. Fine for the in-memory
/// engine; the on-disk one reads its clustered tree in order.
pub(crate) fn sort_rows(
    order: &KeyOrder,
    rows: Vec<(crate::RowId, Row)>,
) -> SqlResult<Vec<(crate::RowId, Row)>> {
    let mut sorted: Vec<(Vec<Value>, crate::RowId, Row)> = Vec::with_capacity(rows.len());
    for (id, row) in rows {
        let key = order.extract(&row)?;
        let pos = partition_point(&sorted, |(k, other, _)| {
            Ok(match order.compare_prefix(k, &key)? {
                Ordering::Less => true,
                Ordering::Equal => *other < id,
                Ordering::Greater => false,
            })
        })?;
        sorted.insert(pos, (key, id, row));
    }
    Ok(sorted.into_iter().map(|(_, id, row)| (id, row)).collect())
}
