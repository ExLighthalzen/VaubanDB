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
