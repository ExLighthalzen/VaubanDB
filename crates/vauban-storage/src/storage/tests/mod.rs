use vauban_errors::InternalError;

use super::*;

/// A do-nothing implementation: proves that the trait is implementable without
/// `unsafe` and that every method can fail with a caller bug.
struct Nop;

fn bug<T>() -> SqlResult<T> {
    Err(InternalError::Bug("nop".into()).into())
}

impl Storage for Nop {
    fn create_database(&self, _name: &str) -> SqlResult<DbId> {
        bug()
    }
    fn drop_database(&self, _db: DbId) -> SqlResult<()> {
        bug()
    }
    fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
        bug()
    }
    fn create_table(&self, _db: DbId, _shape: &TableShape) -> SqlResult<TableId> {
        bug()
    }
    fn drop_table(&self, _table: TableId) -> SqlResult<()> {
        bug()
    }
    fn tables(&self, _db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
        bug()
    }
    fn create_index(&self, _table: TableId, _def: &IndexShape) -> SqlResult<IndexId> {
        bug()
    }
    fn drop_index(&self, _index: IndexId) -> SqlResult<()> {
        bug()
    }
    fn indexes(&self, _table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
        bug()
    }
    fn insert(&self, _txn: TxnId, _table: TableId, _row: &Row) -> SqlResult<RowId> {
        bug()
    }
    fn update(&self, _txn: TxnId, _table: TableId, _id: RowId, _row: &Row) -> SqlResult<()> {
        bug()
    }
    fn delete(&self, _txn: TxnId, _table: TableId, _id: RowId) -> SqlResult<()> {
        bug()
    }
    fn get(&self, _snap: &Snapshot, _table: TableId, _id: RowId) -> SqlResult<Option<Row>> {
        bug()
    }
    fn scan(&self, _snap: &Snapshot, _table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
        bug()
    }
    fn seek(
        &self,
        _snap: &Snapshot,
        _index: IndexId,
        _range: &KeyRange,
        _dir: Direction,
    ) -> SqlResult<Box<dyn RowIter + '_>> {
        bug()
    }
    fn latest_version(&self, _table: TableId, _id: RowId) -> SqlResult<Option<(TxnId, Row)>> {
        bug()
    }
    fn commit(&self, _txn: TxnId) -> SqlResult<()> {
        bug()
    }
    fn rollback(&self, _txn: TxnId) -> SqlResult<()> {
        bug()
    }
    fn savepoint(&self, _txn: TxnId) -> SqlResult<SavepointId> {
        bug()
    }
    fn rollback_to(&self, _txn: TxnId, _sp: SavepointId) -> SqlResult<()> {
        bug()
    }
    fn checkpoint(&self) -> SqlResult<()> {
        bug()
    }
    fn vacuum(&self, _horizon: TxnId) -> SqlResult<()> {
        bug()
    }
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn boxed_storage_is_send_sync() {
    assert_send_sync::<Box<dyn Storage>>();
    assert_send_sync::<Nop>();
}

#[test]
fn nop_is_usable_through_dyn_storage() {
    let storage: Box<dyn Storage> = Box::new(Nop);
    let err = storage.databases().unwrap_err();
    assert_eq!(err.number, 50000);
    assert_eq!(err.message, "Internal error: internal bug: nop");
    assert!(storage.checkpoint().is_err());
    assert!(storage.vacuum(TxnId(1)).is_err());
}
