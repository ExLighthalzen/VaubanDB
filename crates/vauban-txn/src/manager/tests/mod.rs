use super::*;
use vauban_storage::MemoryStorage;

/// The documented bound of [`TransactionManager::begin`]: a saturated counter repeats
/// `TxnId(u64::MAX)` instead of wrapping back to an identifier already handed out.
#[test]
fn saturated_counter_repeats_its_bound() {
    let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
    mgr.state.lock().expect("fresh mutex").next_id = u64::MAX;
    let last = mgr.begin(IsolationLevel::ReadCommitted);
    let beyond = mgr.begin(IsolationLevel::ReadCommitted);
    assert_eq!(last.id, TxnId(u64::MAX));
    assert_eq!(beyond.id, TxnId(u64::MAX), "saturation, not wrap-around");
}

/// `xmin` and the horizon fall back on the next identifier once the last transaction
/// closed, so that `Storage::vacuum` may prune what it left behind.
#[test]
fn horizon_and_xmin_fall_back_on_the_next_id() {
    let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
    let txn = mgr.begin(IsolationLevel::ReadCommitted);
    mgr.commit(txn.clone()).expect("commit of an empty txn");
    assert_eq!(mgr.snapshot_horizon(), TxnId(2));
    let snap = mgr.statement_snapshot(&txn);
    assert_eq!(snap.xmin, TxnId(2));
    assert_eq!(snap.xmax, TxnId(2));
}

/// The bound of the second bullet of [`TransactionManager::snapshot_horizon`]: what
/// makes the horizon pass the `xmin` of a snapshot in use is the **oldest** open
/// transaction closing, not any transaction older than the reader.
#[test]
fn horizon_holds_when_a_middle_transaction_closes() {
    let mgr = TransactionManager::new(Arc::new(MemoryStorage::new()));
    let first = mgr.begin(IsolationLevel::ReadCommitted);
    let middle = mgr.begin(IsolationLevel::ReadCommitted);
    let reader = mgr.begin(IsolationLevel::ReadCommitted);
    let held = mgr.statement_snapshot(&reader);
    assert_eq!(held.xmin, TxnId(1));
    mgr.commit(middle).expect("commit the middle transaction");
    assert_eq!(
        mgr.snapshot_horizon(),
        held.xmin,
        "the oldest open transaction still holds the horizon down"
    );
    mgr.commit(first).expect("commit the oldest transaction");
    assert!(
        mgr.snapshot_horizon() > held.xmin,
        "closing the oldest one is what moves the horizon past the snapshot"
    );
}

/// A storage that delegates to a `MemoryStorage` and holds each `commit` back until the
/// test lets it go, as a journal whose sync does not come back would.
struct HeldCommit {
    inner: MemoryStorage,
    /// Sends one message when a `commit` reaches the storage.
    entered: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
    /// Receives one message per `commit` allowed to go on.
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Storage for HeldCommit {
    fn create_database(&self, name: &str) -> SqlResult<DbId> {
        self.inner.create_database(name)
    }
    fn drop_database(&self, db: DbId) -> SqlResult<()> {
        self.inner.drop_database(db)
    }
    fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
        self.inner.databases()
    }
    fn create_table(&self, db: DbId, shape: &vauban_storage::TableShape) -> SqlResult<TableId> {
        self.inner.create_table(db, shape)
    }
    fn drop_table(&self, table: TableId) -> SqlResult<()> {
        self.inner.drop_table(table)
    }
    fn tables(&self, db: DbId) -> SqlResult<Vec<(TableId, vauban_storage::TableShape)>> {
        self.inner.tables(db)
    }
    fn create_index(
        &self,
        table: TableId,
        def: &vauban_storage::IndexShape,
    ) -> SqlResult<vauban_storage::IndexId> {
        self.inner.create_index(table, def)
    }
    fn drop_index(&self, index: vauban_storage::IndexId) -> SqlResult<()> {
        self.inner.drop_index(index)
    }
    fn indexes(
        &self,
        table: TableId,
    ) -> SqlResult<Vec<(vauban_storage::IndexId, vauban_storage::IndexShape)>> {
        self.inner.indexes(table)
    }
    fn insert(&self, txn: TxnId, table: TableId, row: &vauban_storage::Row) -> SqlResult<RowId> {
        self.inner.insert(txn, table, row)
    }
    fn update(
        &self,
        txn: TxnId,
        table: TableId,
        id: RowId,
        row: &vauban_storage::Row,
    ) -> SqlResult<()> {
        self.inner.update(txn, table, id, row)
    }
    fn delete(&self, txn: TxnId, table: TableId, id: RowId) -> SqlResult<()> {
        self.inner.delete(txn, table, id)
    }
    fn get(
        &self,
        snap: &Snapshot,
        table: TableId,
        id: RowId,
    ) -> SqlResult<Option<vauban_storage::Row>> {
        self.inner.get(snap, table, id)
    }
    fn scan(
        &self,
        snap: &Snapshot,
        table: TableId,
    ) -> SqlResult<Box<dyn vauban_storage::RowIter + '_>> {
        self.inner.scan(snap, table)
    }
    fn seek(
        &self,
        snap: &Snapshot,
        index: vauban_storage::IndexId,
        range: &vauban_storage::KeyRange,
        dir: vauban_storage::Direction,
    ) -> SqlResult<Box<dyn vauban_storage::RowIter + '_>> {
        self.inner.seek(snap, index, range, dir)
    }
    fn latest_version(
        &self,
        table: TableId,
        id: RowId,
    ) -> SqlResult<Option<(TxnId, vauban_storage::Row)>> {
        self.inner.latest_version(table, id)
    }
    fn commit(&self, txn: TxnId) -> SqlResult<()> {
        let _ = self.entered.lock().expect("entered").send(());
        let _ = self.release.lock().expect("release").recv();
        self.inner.commit(txn)
    }
    fn rollback(&self, txn: TxnId) -> SqlResult<()> {
        self.inner.rollback(txn)
    }
    fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId> {
        self.inner.savepoint(txn)
    }
    fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()> {
        self.inner.rollback_to(txn, sp)
    }
    fn checkpoint(&self) -> SqlResult<()> {
        self.inner.checkpoint()
    }
    fn vacuum(&self, horizon: TxnId) -> SqlResult<()> {
        self.inner.vacuum(horizon)
    }
}

/// While `storage` holds a commit back, `begin`, `statement_snapshot` and `active_sessions`
/// answer, the committing transaction stays in `active` and its row stays hidden, and a
/// second `commit` of its handle is refused; once `storage` lets the commit go, the next
/// snapshot shows the row.
#[test]
fn begin_and_snapshot_do_not_wait_for_a_commit_in_storage() {
    use std::sync::mpsc::channel;
    use std::time::Duration;
    use vauban_storage::testsuite::{int_table_shape, row};

    let (entered_tx, entered) = channel();
    let (release, release_rx) = channel();
    let storage = Arc::new(HeldCommit {
        inner: MemoryStorage::new(),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    let db = storage.create_database("txn").expect("create_database");
    let table = storage
        .create_table(db, &int_table_shape(1))
        .expect("create_table");
    let mgr = Arc::new(TransactionManager::new(
        Arc::clone(&storage) as Arc<dyn Storage>
    ));

    let writer = mgr.begin(IsolationLevel::ReadCommitted);
    let id = storage
        .insert(writer.id, table, &row(&[7]))
        .expect("insert");
    let committer = {
        let mgr = Arc::clone(&mgr);
        let writer = writer.clone();
        std::thread::spawn(move || mgr.commit(writer))
    };
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the commit reaches storage");

    // `begin` and `statement_snapshot` run on a thread of their own, so that a manager that
    // waits for the commit fails the test instead of hanging it.
    let (answer_tx, answer) = channel();
    {
        let mgr = Arc::clone(&mgr);
        std::thread::spawn(move || {
            let reader = mgr.begin(IsolationLevel::ReadCommitted);
            let snap = mgr.statement_snapshot(&reader);
            let open: Vec<TxnId> = mgr.active_sessions().iter().map(|i| i.id).collect();
            let _ = answer_tx.send((reader, snap, open));
        });
    }
    let (reader, snap, open) = answer
        .recv_timeout(Duration::from_secs(2))
        .expect("begin and statement_snapshot answer while the commit is held");
    assert_eq!(
        snap.active,
        vec![writer.id],
        "the committing transaction is in active"
    );
    assert_eq!(open, vec![writer.id, reader.id]);
    assert_eq!(storage.get(&snap, table, id).expect("get"), None);
    let err = mgr
        .commit(writer.clone())
        .expect_err("a second commit while the first is in storage");
    assert_eq!(err.number, 50000, "message was {:?}", err.message);

    release.send(()).expect("let the commit go");
    committer
        .join()
        .expect("the committing thread")
        .expect("commit");
    let after = mgr.statement_snapshot(&reader);
    assert!(after.active.is_empty());
    assert_eq!(
        storage.get(&after, table, id).expect("get"),
        Some(row(&[7]))
    );
    assert_eq!(
        storage.get(&snap, table, id).expect("get"),
        None,
        "the snapshot taken during the commit keeps hiding the row"
    );
}
