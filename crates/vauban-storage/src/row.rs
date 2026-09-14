//! Rows as the storage layer exchanges them with the rest of the engine.

use vauban_errors::SqlResult;
use vauban_types::Value;

use crate::RowId;

/// The content of one row version: one [`Value`] per column of the table, in
/// [`crate::TableShape::columns`] order.
///
/// The storage layer checks the arity (`0.len() == columns.len()`) and nothing else: the
/// executor is responsible for the values matching the column types. Not `Eq`: [`Value`]
/// is not.
#[derive(Debug, Clone, PartialEq)]
pub struct Row(pub Vec<Value>);

/// An iterator over the rows produced by [`crate::Storage::scan`] and
/// [`crate::Storage::seek`]: the [`RowId`] of each visible logical row and the content of
/// its visible version.
///
/// Any iterator with the right `Item` type is a `RowIter` (blanket impl below), so
/// implementations may return any concrete iterator boxed. An error item (I/O failure on
/// disk) ends the iteration: the caller must not pull further items after an `Err`.
pub trait RowIter: Iterator<Item = SqlResult<(RowId, Row)>> {}

impl<I: Iterator<Item = SqlResult<(RowId, Row)>>> RowIter for I {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_iterator_of_rows_is_a_row_iter() {
        let rows = vec![Ok((RowId(1), Row(vec![Value::Null])))];
        let mut iter: Box<dyn RowIter> = Box::new(rows.into_iter());
        let (id, row) = iter.next().unwrap().unwrap();
        assert_eq!(id, RowId(1));
        assert_eq!(row, Row(vec![Value::Null]));
        assert!(iter.next().is_none());
    }
}
