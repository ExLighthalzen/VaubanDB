use super::*;
use std::collections::HashSet;

#[test]
fn ids_display_as_bare_integers() {
    assert_eq!(DbId(1).to_string(), "1");
    assert_eq!(TableId(7).to_string(), "7");
    assert_eq!(IndexId(42).to_string(), "42");
    assert_eq!(RowId(0).to_string(), "0");
    assert_eq!(TxnId(u64::MAX).to_string(), u64::MAX.to_string());
    assert_eq!(SavepointId(3).to_string(), "3");
    // `Debug` keeps the type name; only `Display` is bare.
    assert_eq!(format!("{:?}", TableId(7)), "TableId(7)");
}

#[test]
fn ids_are_copy_ord_and_hash() {
    fn assert_traits<T: Copy + Ord + std::hash::Hash + std::fmt::Debug>() {}
    assert_traits::<DbId>();
    assert_traits::<TableId>();
    assert_traits::<IndexId>();
    assert_traits::<RowId>();
    assert_traits::<TxnId>();
    assert_traits::<SavepointId>();

    assert!(TxnId(1) < TxnId(2));
    let set: HashSet<RowId> = [RowId(1), RowId(1), RowId(2)].into_iter().collect();
    assert_eq!(set.len(), 2);
}
