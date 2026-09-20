use vauban_types::{SqlType, TypeInfo, Value};

use super::encode::encode_row;
use super::page::{Page, PageKind};
use super::temp::TempDir;
use super::version::{HeapTable, VersionHeader};
use super::wal::{CheckpointPayload, WalRecord, WalRecordKind};
use super::*;
use crate::{
    Direction, KeyColumn, KeyRange, Row, RowId, Storage, TableId, TableShape, TxnId, TxnStatus,
};

/// Size in bytes of a file of the instance.
fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .unwrap_or_else(|err| panic!("{} should exist: {err}", path.display()))
        .len()
}

/// The control block of the instance, copied out of its lock.
fn control_of(storage: &DiskStorage) -> Control {
    storage.control().expect("read the control block")
}

#[test]
fn open_creates_ctl_data_wal() {
    let dir = TempDir::created("open-creates");
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");

    let control_path = dir.child(CONTROL_FILE_NAME);
    let data_path = dir.child(DATA_FILE_NAME);
    let wal_path = dir.child(WAL_FILE_NAME);
    assert!(control_path.is_file(), "vauban.ctl");
    assert!(data_path.is_file(), "data");
    assert!(wal_path.is_file(), "wal");

    let control_bytes = fs::read(&control_path).expect("read vauban.ctl");
    assert_eq!(control_bytes.len(), 128);
    assert_eq!(&control_bytes[0..8], b"VAUBANDB");
    assert_eq!(
        u32::from_le_bytes([
            control_bytes[8],
            control_bytes[9],
            control_bytes[10],
            control_bytes[11]
        ]),
        2,
        "format_version"
    );
    assert_eq!(file_size(&data_path), 0, "data holds no page at creation");
    assert_eq!(file_size(&wal_path), 0, "wal is empty at creation");

    assert_eq!(storage.dir, dir.path());
    assert_eq!(storage.data.path(), data_path);
    assert_eq!(storage.options, DiskOptions::default());
    assert_eq!(storage.data.page_count().expect("page count"), 0);
    assert_eq!(control_of(&storage), Control::default());
}

#[test]
fn open_creates_a_missing_directory() {
    let dir = TempDir::reserved("open-mkdir");
    let nested = dir.child("a").join("b");
    assert!(!nested.exists());
    let storage = DiskStorage::open(&nested, DiskOptions::default()).expect("create an instance");
    assert!(nested.is_dir());
    assert!(nested.join(CONTROL_FILE_NAME).is_file());
    assert_eq!(storage.dir, nested);
}

#[test]
fn open_is_idempotent_on_empty() {
    let dir = TempDir::created("open-twice");
    let first = DiskStorage::open(dir.path(), DiskOptions::default()).expect("create");
    let control = control_of(&first);
    drop(first);

    let second = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    assert_eq!(control_of(&second), control);
    assert_eq!(control_of(&second), Control::default());
    assert_eq!(second.data.page_count().expect("page count"), 0);
    assert_eq!(file_size(&dir.child(WAL_FILE_NAME)), 0);
    assert_eq!(file_size(&dir.child(CONTROL_FILE_NAME)), 128);
    drop(second);

    let third = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen again");
    assert_eq!(control_of(&third), control);
}

/// The counters of `vauban.ctl` come back through a close and a reopen.
///
/// The test above cannot show it: a fresh block holds `0`, `1` and `u64::MAX`, the values
/// [`Control::default`] carries, so a `reopen` that threw away the file it read would
/// answer the same. Here the control file of an instance is replaced by one whose eight
/// counters are distinct and none of them is the default, and `open` is asked for them.
///
/// Handing [`Control::default`] to the structure built by [`DiskStorage::reopen`] instead
/// of the block it read turns this test red.
#[test]
fn open_returns_the_counters_of_the_ctl_it_read() {
    use super::page::Lsn;

    let dir = TempDir::created("counters-survive");
    drop(DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance"));

    let moved = Control {
        next_page_id: PageId(7),
        free_head: Some(PageId(9)),
        next_db_id: 12,
        next_table_id: 34,
        next_index_id: 56,
        next_row_id: 78,
        latest_checkpoint_lsn: Lsn(90),
        durable_lsn: Lsn(91),
        meta_root: None,
    };
    assert_ne!(moved, Control::default());
    moved
        .write(&dir.child(CONTROL_FILE_NAME))
        .expect("write the counters of a used instance");

    let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    let read_back = control_of(&reopened);
    assert_eq!(read_back, moved);
    // Field by field as well, so that a failure names the counter that was lost.
    assert_eq!(read_back.next_page_id, PageId(7));
    assert_eq!(read_back.free_head, Some(PageId(9)));
    assert_eq!(read_back.next_db_id, 12);
    assert_eq!(read_back.next_table_id, 34);
    assert_eq!(read_back.next_index_id, 56);
    assert_eq!(read_back.next_row_id, 78);
    assert_eq!(read_back.latest_checkpoint_lsn, Lsn(90));
    assert_eq!(read_back.durable_lsn, Lsn(91));
}

#[test]
fn open_rejects_unknown_version() {
    let dir = TempDir::created("bad-version");
    // The three files are there and the ctl is sealed, so the version is what `open`
    // refuses here.
    let mut bytes = Control::default().to_bytes();
    bytes[8..12].copy_from_slice(&3u32.to_le_bytes());
    control::seal(&mut bytes);
    fs::write(dir.child(CONTROL_FILE_NAME), bytes).expect("write a ctl of version 3");
    fs::write(dir.child(DATA_FILE_NAME), []).expect("write an empty data file");
    fs::write(dir.child(WAL_FILE_NAME), []).expect("write an empty wal");

    let err = DiskStorage::open(dir.path(), DiskOptions::default())
        .expect_err("version 3 is not read by this build");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("format version 3"), "{err}");
}

#[test]
fn read_page_past_end_is_corruption() {
    let dir = TempDir::created("read-past-end");
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    assert_eq!(storage.data.page_count().expect("page count"), 0);
    let err = storage
        .data
        .read_page(PageId(0))
        .expect_err("a fresh instance holds no page");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("page 0"), "{err}");
}

#[test]
fn tls_dir_does_not_block_create() {
    let dir = TempDir::created("with-tls");
    let tls = dir.child("tls");
    fs::create_dir_all(&tls).expect("create tls/");
    fs::write(tls.join("server.pem"), b"not a certificate").expect("write in tls/");

    let storage = DiskStorage::open(dir.path(), DiskOptions::default())
        .expect("tls/ is not an instance, the instance is created");
    assert!(dir.child(CONTROL_FILE_NAME).is_file());
    assert_eq!(control_of(&storage), Control::default());
    // The directory that was already there is left as it was.
    assert_eq!(
        fs::read(tls.join("server.pem")).expect("read the file back"),
        b"not a certificate"
    );
}

#[test]
fn open_rejects_an_instance_whose_data_file_is_missing() {
    let dir = TempDir::created("no-data");
    DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    let data_path = dir.child(DATA_FILE_NAME);
    fs::remove_file(&data_path).expect("remove the data file");

    let err = DiskStorage::open(dir.path(), DiskOptions::default())
        .expect_err("an instance without its data file");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(
        err.to_string().contains(&data_path.display().to_string()),
        "{err}"
    );
}

#[test]
fn open_rejects_an_instance_whose_wal_is_missing() {
    let dir = TempDir::created("no-wal");
    DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    let wal_path = dir.child(WAL_FILE_NAME);
    fs::remove_file(&wal_path).expect("remove the wal");

    let err = DiskStorage::open(dir.path(), DiskOptions::default())
        .expect_err("an instance without its wal");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(
        err.to_string().contains(&wal_path.display().to_string()),
        "{err}"
    );
}

#[test]
fn open_rejects_data_without_a_control_file() {
    let dir = TempDir::created("no-ctl");
    DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    fs::remove_file(dir.child(CONTROL_FILE_NAME)).expect("remove the ctl");

    let err = DiskStorage::open(dir.path(), DiskOptions::default())
        .expect_err("data and wal without a ctl is not an empty directory");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains(CONTROL_FILE_NAME), "{err}");
}

#[test]
fn a_page_written_through_the_instance_survives_a_reopen() {
    use super::page::{Lsn, Page, PageKind};

    let dir = TempDir::created("survives-reopen");
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    // The allocator owns the growth of the file; the test extends it by hand.
    storage.data.extend_to(1).expect("extend to one page");
    let mut page = Page::empty(PageKind::Heap, PageId(0));
    page.set_lsn(Lsn(5));
    page.seal();
    storage
        .data
        .write_page(PageId(0), &page)
        .expect("write page 0");
    drop(storage);

    let reopened = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    assert_eq!(reopened.data.page_count().expect("page count"), 1);
    let read = reopened.data.read_page(PageId(0)).expect("read page 0");
    assert_eq!(read, page);
    assert_eq!(read.lsn(), Lsn(5));
    // The control file was not rewritten by the page write.
    assert_eq!(control_of(&reopened), Control::default());
}

#[test]
fn open_builds_a_pool_of_the_option_size_over_the_data_file() {
    use super::page::{Lsn, Page, PageKind};

    let dir = TempDir::created("pool-of-open");
    let storage =
        DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 2 }).expect("create an instance");
    assert_eq!(storage.pool.capacity(), 2);

    // The allocator owns the growth of the file; the test extends it by hand.
    storage.data.extend_to(1).expect("extend to one page");
    let mut page = Page::empty(PageKind::Heap, PageId(0));
    page.set_lsn(Lsn(0));
    page.seal();
    storage
        .data
        .write_page(PageId(0), &page)
        .expect("write page 0");

    let pin = storage.pool.pin(PageId(0)).expect("pin page 0");
    assert_eq!(
        pin.with_page(|cached| cached.clone())
            .expect("read the cached page"),
        page,
        "the pool of the instance reads through its data file"
    );
}

#[test]
fn open_rejects_a_pool_of_zero_frames_before_creating_anything() {
    let dir = TempDir::created("zero-frames");
    let err = DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 0 })
        .expect_err("a pool of 0 frames is refused");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("0 frames"), "{err}");
    assert!(
        !dir.child(CONTROL_FILE_NAME).exists(),
        "the option is checked before the instance is created"
    );
    assert!(!dir.child(DATA_FILE_NAME).exists());
    assert!(!dir.child(WAL_FILE_NAME).exists());

    // The same directory opens with a usable option.
    DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 1 }).expect("create an instance");
    // And an existing instance is refused the same way.
    let err = DiskStorage::open(dir.path(), DiskOptions { buffer_pages: 0 })
        .expect_err("reopening with 0 frames is refused too");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
}

#[test]
fn disk_options_default_holds_1024_pages() {
    assert_eq!(DiskOptions::default().buffer_pages, 1024);
    assert_eq!(
        DiskOptions { buffer_pages: 2 },
        DiskOptions { buffer_pages: 2 }
    );
}

/// An empty instance in a temporary directory, with the guard that removes it.
fn empty_instance(label: &str) -> (TempDir, DiskStorage) {
    let dir = TempDir::created(label);
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    (dir, storage)
}

/// The kind of each record of the journal, in order.
fn kinds(records: &[WalRecord]) -> Vec<WalRecordKind> {
    records.iter().map(|record| record.kind).collect()
}

#[test]
fn checkpoint_on_empty_is_ok() {
    let (dir, storage) = empty_instance("checkpoint-empty");

    storage
        .checkpoint()
        .expect("checkpoint an instance nothing was written on");

    let records = storage.wal.records().expect("read the journal");
    assert_eq!(kinds(&records), vec![WalRecordKind::Checkpoint]);
    assert_eq!(records[0].lsn, Lsn(1));
    assert_eq!(
        CheckpointPayload::decode(&records[0].payload).expect("decode the payload"),
        CheckpointPayload {
            redo_lsn: Lsn(1),
            next_page_id: PageId(0),
            next_row_id: 1,
        },
        "the payload copies the counters of a fresh control block"
    );
    let control = control_of(&storage);
    assert_eq!(control.latest_checkpoint_lsn, Lsn(1));
    assert_eq!(control.durable_lsn, Lsn(1));
    assert_eq!(
        Control::read(&dir.child(CONTROL_FILE_NAME)).expect("read vauban.ctl"),
        control
    );
    assert_eq!(
        file_size(&dir.child(DATA_FILE_NAME)),
        0,
        "data holds 0 pages, the instance having been written on by nothing"
    );
}

#[test]
fn two_checkpoints_leave_two_records() {
    let (dir, storage) = empty_instance("checkpoint-twice");

    storage.checkpoint().expect("first checkpoint");
    storage.checkpoint().expect("second checkpoint");

    let records = storage.wal.records().expect("read the journal");
    assert_eq!(
        kinds(&records),
        vec![WalRecordKind::Checkpoint, WalRecordKind::Checkpoint],
        "the journal keeps the record of the first checkpoint"
    );
    for (record, lsn) in records.iter().zip([Lsn(1), Lsn(2)]) {
        assert_eq!(record.lsn, lsn);
        assert_eq!(
            CheckpointPayload::decode(&record.payload)
                .expect("decode the payload")
                .redo_lsn,
            lsn,
            "each of the two payloads names its own record"
        );
    }
    let control = Control::read(&dir.child(CONTROL_FILE_NAME)).expect("read vauban.ctl");
    assert_eq!(control.latest_checkpoint_lsn, Lsn(2));
    assert_eq!(control.durable_lsn, Lsn(2));
    assert_eq!(control, control_of(&storage));
}

// --- Checkpoint ---------------------------------------------------------------------

/// The table the checkpoint tests write on.
const TABLE: TableId = TableId(9);

/// A table of one nullable `int` column over a heap page of its own.
fn int_table(storage: &DiskStorage) -> HeapTable<'_> {
    let head = alloc::allocate(storage).expect("allocate the head page");
    let shape = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: None,
    };
    HeapTable::create(storage, TABLE, head, shape).expect("create the table")
}

/// A row of one `int`.
fn row(value: i32) -> Row {
    Row(vec![Value::I32(value)])
}

/// The bytes the first version of the first row of a table occupies on the heap: the
/// 32-byte prefix of [`version::VersionHeader`] then the columns.
fn first_version_of(txn: u64, value: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    VersionHeader {
        row: RowId(1),
        xmin: TxnId(txn),
        xmax: None,
        seq: 1,
    }
    .write_to(&mut bytes);
    encode_row(&row(value), &mut bytes);
    bytes
}

/// Whether the 8 192 bytes of `page` carry `needle`.
fn page_carries(page: &Page, needle: &[u8]) -> bool {
    page.0.windows(needle.len()).any(|window| window == needle)
}

/// The page of `data`, read behind the pool.
fn page_on_data(storage: &DiskStorage, id: PageId) -> Page {
    storage.data.read_page(id).expect("read a page of `data`")
}

#[test]
fn checkpoint_flushes_committed_heap_page() {
    let (_dir, storage) = empty_instance("checkpoint-flush");
    let mut table = int_table(&storage);
    let page = table.heap().first_page();
    table.insert(TxnId(4), &row(7)).expect("insert");
    table.commit(TxnId(4)).expect("commit");
    let version = first_version_of(4, 7);

    // The pool was not flushed by hand: the commit synced the journal, not `data`.
    assert!(
        !page_carries(&page_on_data(&storage, page), &version),
        "the committed row reached `data` before the checkpoint"
    );

    storage.checkpoint().expect("checkpoint");

    assert!(
        page_carries(&page_on_data(&storage, page), &version),
        "the committed row is missing from `data` after the checkpoint"
    );
}

#[test]
fn checkpoint_skips_in_progress() {
    let (_dir, storage) = empty_instance("checkpoint-in-progress");
    let mut running = int_table(&storage);
    let mut committed = int_table(&storage);
    let hidden_page = running.heap().first_page();
    let seen_page = committed.heap().first_page();
    assert_ne!(
        hidden_page, seen_page,
        "the two tables were given heap pages of their own"
    );
    running
        .insert(TxnId(4), &row(41))
        .expect("insert of the running transaction");
    committed
        .insert(TxnId(5), &row(52))
        .expect("insert of the committing transaction");
    committed.commit(TxnId(5)).expect("commit");

    storage.checkpoint().expect("checkpoint");

    assert!(
        page_carries(&page_on_data(&storage, seen_page), &first_version_of(5, 52)),
        "the row of the committed transaction is missing from `data`"
    );
    assert!(
        !page_carries(
            &page_on_data(&storage, hidden_page),
            &first_version_of(4, 41)
        ),
        "the row of the running transaction reached `data`"
    );
    // Transaction 4 is still in progress: its page waits in the pool, flag set.
    assert_eq!(running.status(TxnId(4)), TxnStatus::InProgress);
}

#[test]
fn checkpoint_record_in_wal_and_ctl() {
    let (dir, storage) = empty_instance("checkpoint-record");
    let mut table = int_table(&storage);
    table.insert(TxnId(4), &row(1)).expect("insert");
    table.commit(TxnId(4)).expect("commit");

    storage.checkpoint().expect("checkpoint");

    let records = storage.wal.records().expect("read the journal");
    assert_eq!(
        kinds(&records),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Commit,
            WalRecordKind::Checkpoint,
        ]
    );
    let record = records.last().expect("the checkpoint record");
    assert_eq!(record.lsn, Lsn(4));
    assert_eq!(record.txn, TxnId(0));
    let payload = CheckpointPayload::decode(&record.payload).expect("decode the payload");
    assert_eq!(
        payload.redo_lsn, record.lsn,
        "the payload names the record itself"
    );

    let control = control_of(&storage);
    assert_eq!(control.latest_checkpoint_lsn, record.lsn);
    assert_eq!(control.durable_lsn, record.lsn);
    assert_eq!(payload.next_page_id, control.next_page_id);
    assert_eq!(payload.next_row_id, control.next_row_id);
    // The head page of the table was handed out by the allocator, so the counter moved.
    assert_eq!(control.next_page_id, PageId(1));
    // The file read on its own by `control.rs` says the same.
    let reread = Control::read(&dir.child(CONTROL_FILE_NAME)).expect("read vauban.ctl");
    assert_eq!(reread, control);
    assert_eq!(reread.latest_checkpoint_lsn, Lsn(4));
}

#[test]
fn a_transaction_astride_the_checkpoint_leaves_its_insert_before_redo_lsn() {
    let (_dir, storage) = empty_instance("checkpoint-astride");
    let mut table = int_table(&storage);
    let page = table.heap().first_page();
    table.insert(TxnId(4), &row(7)).expect("insert");

    storage.checkpoint().expect("checkpoint while 4 runs");
    table.commit(TxnId(4)).expect("commit after the checkpoint");

    let records = storage.wal.records().expect("read the journal");
    assert_eq!(
        kinds(&records),
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Checkpoint,
            WalRecordKind::Commit,
        ]
    );
    let redo = CheckpointPayload::decode(&records[2].payload)
        .expect("decode the payload")
        .redo_lsn;
    assert_eq!(
        (records[1].lsn, redo, records[3].lsn),
        (Lsn(2), Lsn(3), Lsn(4)),
        "the insert is behind the checkpoint record, the commit ahead of it"
    );
    // The write was in progress when the checkpoint flushed the pool, and the commit that
    // follows syncs the journal, not `data`: the page of the row stayed in the pool.
    assert!(
        !page_carries(&page_on_data(&storage, page), &first_version_of(4, 7)),
        "the row of the transaction astride the checkpoint reached `data`"
    );
    // A redo of the records after `redo_lsn` would replay the `Commit` alone, whose
    // payload is empty: where a recovery starts is decided by `recover`, and this LSN
    // does not answer it on its own.
    assert!(records[3].payload.is_empty());
}

// ------------------------------------------------------------------- DDL

/// The instance of `dir`, created or reopened.
fn reopen(dir: &TempDir) -> DiskStorage {
    DiskStorage::open(dir.path(), DiskOptions::default()).expect("open the instance")
}

/// A table of two nullable `int`, the first of them clustered when `clustered` is true.
fn two_ints(clustered: bool) -> TableShape {
    TableShape {
        columns: vec![
            TypeInfo::new(SqlType::Int, true),
            TypeInfo::new(SqlType::Int, true),
        ],
        clustered_key: clustered.then(|| {
            vec![KeyColumn {
                column: 0,
                descending: false,
            }]
        }),
    }
}

/// A unique index on the column `column`, ascending, with no included column.
fn unique_on(column: u16) -> IndexShape {
    IndexShape {
        columns: vec![KeyColumn {
            column,
            descending: false,
        }],
        unique: true,
        included: Vec::new(),
    }
}

/// Creates one database, one table of [`two_ints`] and one index on its first column.
fn create_the_three(storage: &DiskStorage, clustered: bool) -> (DbId, TableId, IndexId) {
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(clustered))
        .expect("create the table");
    let index = storage
        .create_index(table, &unique_on(0))
        .expect("create the index");
    (db, table, index)
}

/// The three lists of the catalogue, as the reopen tests assert them.
type Listed = (
    Vec<(DbId, String)>,
    Vec<(TableId, TableShape)>,
    Vec<(IndexId, IndexShape)>,
);

/// What the three lists of the catalogue answer, for the assertions of the reopen tests.
fn catalogue_of(storage: &DiskStorage, db: DbId, table: TableId) -> Listed {
    (
        storage.databases().expect("databases"),
        storage.tables(db).expect("tables"),
        storage.indexes(table).expect("indexes"),
    )
}

#[test]
fn ddl_survives_reopen() {
    let dir = TempDir::created("ddl-reopen");
    let (db, table, index) = {
        let storage = reopen(&dir);
        let ids = create_the_three(&storage, false);
        storage.checkpoint().expect("checkpoint");
        ids
    };

    let storage = reopen(&dir);
    assert_eq!((db, table, index), (DbId(1), TableId(1), IndexId(1)));
    assert_eq!(
        catalogue_of(&storage, db, table),
        (
            vec![(db, "a".to_string())],
            vec![(table, two_ints(false))],
            vec![(index, unique_on(0))],
        )
    );
    // The identifiers are not handed out a second time.
    let (second_db, second_table, second_index) = create_the_three(&storage, false);
    assert_eq!(
        (second_db, second_table, second_index),
        (DbId(2), TableId(2), IndexId(2))
    );
}

#[test]
fn ddl_survives_reopen_without_checkpoint() {
    let dir = TempDir::created("ddl-reopen-no-checkpoint");
    let (db, table, index, root) = {
        let storage = reopen(&dir);
        let (db, table, index) = create_the_three(&storage, false);
        let root = control_of(&storage)
            .meta_root
            .expect("the root of the chain");
        // No checkpoint and no flush: the page of the catalogue is still in the pool, and
        // `data` holds the page the allocator wrote there.
        assert_eq!(
            page_on_data(&storage, root).kind().expect("the kind"),
            PageKind::Free,
            "the catalogue has not reached `data`"
        );
        (db, table, index, root)
    };

    let storage = reopen(&dir);
    assert_eq!(control_of(&storage).meta_root, Some(root));
    assert_eq!(
        catalogue_of(&storage, db, table),
        (
            vec![(db, "a".to_string())],
            vec![(table, two_ints(false))],
            vec![(index, unique_on(0))],
        ),
        "the DDL records of the journal rebuild the catalogue"
    );
}

#[test]
fn a_checkpointed_catalogue_reads_back_with_an_emptied_journal() {
    let dir = TempDir::created("ddl-pages-alone");
    let (db, table, index) = {
        let storage = reopen(&dir);
        let ids = create_the_three(&storage, false);
        storage.checkpoint().expect("checkpoint");
        ids
    };
    // The counterpart of `ddl_survives_reopen_without_checkpoint`: with the journal gone,
    // what answers is the chain of meta pages the checkpoint wrote. Emptying the file
    // rather than removing it, an instance without a journal being corruption.
    fs::File::create(dir.child(WAL_FILE_NAME)).expect("empty the journal");

    let storage = reopen(&dir);
    assert_eq!(storage.wal.records().expect("records").len(), 0);
    assert_eq!(
        catalogue_of(&storage, db, table),
        (
            vec![(db, "a".to_string())],
            vec![(table, two_ints(false))],
            vec![(index, unique_on(0))],
        )
    );
}

#[test]
fn a_root_whose_formatting_was_lost_is_written_again() {
    let dir = TempDir::created("ddl-root-lost");
    let (db, first) = {
        let storage = reopen(&dir);
        let db = storage.create_database("a").expect("create the database");
        let first = storage
            .create_table(db, &two_ints(false))
            .expect("create the table");
        // Dropped without a checkpoint: `data` holds the page the allocator wrote where
        // the root of the chain is, and the catalogue that reaches the reopen comes from
        // the journal.
        (db, first)
    };

    let second = {
        let storage = reopen(&dir);
        let root = control_of(&storage).meta_root.expect("the root");
        assert_eq!(
            page_on_data(&storage, root).kind().expect("the kind"),
            PageKind::Free
        );
        let second = storage
            .create_table(db, &two_ints(true))
            .expect("create a second table");
        storage.checkpoint().expect("checkpoint");
        assert_eq!(
            page_on_data(&storage, root).kind().expect("the kind"),
            PageKind::Meta,
            "the DDL wrote the root again"
        );
        second
    };

    // With the journal emptied, the chain of meta pages answers for the two tables.
    fs::File::create(dir.child(WAL_FILE_NAME)).expect("empty the journal");
    let storage = reopen(&dir);
    assert_eq!(
        storage.tables(db).expect("tables"),
        vec![(first, two_ints(false)), (second, two_ints(true))]
    );
}

#[test]
fn a_ctl_behind_the_journal_takes_the_identifiers_of_the_catalogue() {
    let dir = TempDir::created("ddl-ctl-behind");
    let (db, table) = {
        let storage = reopen(&dir);
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(false))
            .expect("create the table");
        storage.checkpoint().expect("checkpoint");
        (db, table)
    };
    // A `vauban.ctl` whose three object counters are the ones of a fresh instance, its
    // page counter and its root left alone: the order of a DDL statement rules this out,
    // the file being rewritten before the record is appended, so this is what the reopen
    // reads when that file was lost.
    let path = dir.child(CONTROL_FILE_NAME);
    let mut control = Control::read(&path).expect("read the control block");
    control.next_db_id = 1;
    control.next_table_id = 1;
    control.next_index_id = 1;
    control.write(&path).expect("write the control block");

    let storage = reopen(&dir);
    let read_back = control_of(&storage);
    assert_eq!(
        (
            read_back.next_db_id,
            read_back.next_table_id,
            read_back.next_index_id
        ),
        (2, 2, 1),
        "the counters take the largest identifier of the catalogue, plus one"
    );
    assert_eq!(
        storage.create_database("b").expect("create a database"),
        DbId(2)
    );
    assert_eq!(
        storage
            .create_table(db, &two_ints(false))
            .expect("create a table"),
        TableId(2)
    );
    assert_eq!(
        storage.tables(db).expect("tables").first().map(|t| t.0),
        Some(table)
    );
}

#[test]
fn drop_table_frees_its_pages() {
    let (dir, storage) = empty_instance("ddl-drop-table");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(false))
        .expect("create the table");
    let entry_page = storage
        .lock_catalogue()
        .expect("the catalogue")
        .table(table)
        .expect("the table")
        .first_page;
    // 500 rows of two `int` take more than one heap page.
    {
        let mut rows = HeapTable::open(&storage, table, entry_page, two_ints(false));
        for value in 0..500 {
            rows.insert(TxnId(1), &Row(vec![Value::I32(value), Value::Null]))
                .expect("insert");
        }
        rows.commit(TxnId(1)).expect("commit");
    }
    let before = control_of(&storage).next_page_id;
    assert!(before.0 > 3, "the heap took more than one page: {before}");

    storage.drop_table(table).expect("drop the table");
    assert_eq!(storage.tables(db).expect("tables"), Vec::new());
    assert_eq!(storage.indexes(table).unwrap_err().number, 50000);

    // The pages of the heap are in the free list: the next allocation hands one of them
    // back and the file does not grow.
    let handed = alloc::allocate(&storage).expect("allocate");
    assert_eq!(control_of(&storage).next_page_id, before);
    assert!(
        handed.0 < before.0,
        "{handed} is a page the dropped table held"
    );
    drop(dir);
}

#[test]
fn clustered_index_shares_the_tree() {
    let (dir, storage) = empty_instance("ddl-clustered-index");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(true))
        .expect("create the table");
    let before = control_of(&storage).next_page_id;

    let index = storage
        .create_index(table, &unique_on(0))
        .expect("the index of the clustered key");
    assert_eq!(
        control_of(&storage).next_page_id,
        before,
        "an index on the clustered key allocates no tree page"
    );
    assert_eq!(
        storage.indexes(table).expect("indexes"),
        vec![(index, unique_on(0))]
    );
    assert_eq!(
        storage
            .lock_catalogue()
            .expect("the catalogue")
            .index(index)
            .expect("the index")
            .root,
        None,
        "the entry carries no root of its own"
    );
    drop(dir);
}

/// The clustered table `table` of the catalogue, attached as it stands.
fn clustered_of<'a>(storage: &'a DiskStorage, table: TableId) -> ClusteredTable<'a> {
    let entry = storage
        .lock_catalogue()
        .expect("the catalogue")
        .table(table)
        .expect("the table")
        .clone();
    ClusteredTable::open(
        storage,
        table,
        entry.first_page,
        (
            entry.tree_root.expect("the tree"),
            entry.directory_root.expect("the directory"),
        ),
        entry.shape,
    )
    .expect("attach to the clustered table")
}

#[test]
fn a_unique_index_on_the_clustered_key_refuses_a_duplicate() {
    let (dir, storage) = empty_instance("ddl-clustered-duplicate");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(true))
        .expect("create the table");
    {
        let mut rows = clustered_of(&storage, table);
        for second in [11, 12] {
            rows.insert(TxnId(1), &Row(vec![Value::I32(7), Value::I32(second)]))
                .expect("insert a row of key 7");
        }
        rows.commit(TxnId(1)).expect("commit");
    }
    let before = control_of(&storage).next_page_id;

    let err = storage
        .create_index(table, &unique_on(0))
        .expect_err("two live rows of key 7 are a duplicate");
    assert_eq!(err.number, 2601);
    assert!(err.message.contains("I32(7)"), "{}", err.message);
    assert_eq!(storage.indexes(table).expect("indexes"), Vec::new());
    assert_eq!(
        control_of(&storage).next_page_id,
        before,
        "nothing allocated"
    );

    // The same shape without `unique` passes, and shares the tree.
    let permissive = IndexShape {
        columns: vec![KeyColumn {
            column: 0,
            descending: false,
        }],
        unique: false,
        included: Vec::new(),
    };
    let index = storage
        .create_index(table, &permissive)
        .expect("a non-unique index takes the duplicate");
    assert_eq!(
        index,
        IndexId(2),
        "the refused statement kept its identifier"
    );
    assert_eq!(control_of(&storage).next_page_id, before);

    // One row of key 7 deleted: the key is live once, and the unique index passes.
    {
        let mut rows = clustered_of(&storage, table);
        let visible = rows
            .scan(&Snapshot {
                xmin: TxnId(9),
                xmax: TxnId(9),
                active: Vec::new(),
                own: TxnId(2),
            })
            .expect("scan");
        assert_eq!(visible.len(), 2);
        rows.delete(TxnId(2), visible[0].0).expect("delete");
        rows.commit(TxnId(2)).expect("commit");
    }
    assert_eq!(
        storage
            .create_index(table, &unique_on(0))
            .expect("one live row of key 7 is no duplicate"),
        IndexId(3)
    );
    drop(dir);
}

/// A snapshot whose `xmin` and `xmax` are `TxnId(9)`, so a transaction of a smaller
/// identifier reads as settled.
fn settled(own: u64) -> Snapshot {
    Snapshot {
        xmin: TxnId(9),
        xmax: TxnId(9),
        active: Vec::new(),
        own: TxnId(own),
    }
}

/// A row of two `int` columns.
fn pair(a: i32, b: i32) -> Row {
    Row(vec![Value::I32(a), Value::I32(b)])
}

/// An index on the column `column`, ascending, not `unique`, with no included column.
fn index_on(column: u16) -> IndexShape {
    IndexShape {
        unique: false,
        ..unique_on(column)
    }
}

/// A clustered shape of one column of type `ty`, key on that column.
fn keyed_by(ty: SqlType) -> TableShape {
    TableShape {
        columns: vec![TypeInfo::new(ty, true)],
        clustered_key: Some(vec![KeyColumn {
            column: 0,
            descending: false,
        }]),
    }
}

#[test]
fn assert_send_sync() {
    fn takes<T: Storage + Send + Sync + 'static>() {}
    takes::<DiskStorage>();
    let (dir, storage) = empty_instance("trait-send-sync");
    let behind: Box<dyn Storage> = Box::new(storage);
    assert_eq!(behind.databases().expect("databases"), Vec::new());
    drop(dir);
}

#[test]
fn insert_commit_reopen_get() {
    for clustered in [false, true] {
        let dir = TempDir::created(if clustered {
            "trait-reopen-clustered"
        } else {
            "trait-reopen-heap"
        });
        let (table, ids) = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = storage.create_database("a").expect("create the database");
            let table = storage
                .create_table(db, &two_ints(clustered))
                .expect("create the table");
            // Inserted in an order the clustered key does not follow, so that the order of
            // the `scan` below tells the two stores apart.
            let mut ids = Vec::new();
            for key in [30, 10, 20] {
                ids.push(
                    storage
                        .insert(TxnId(1), table, &pair(key, key + 1))
                        .expect("insert"),
                );
            }
            storage.commit(TxnId(1)).expect("commit");
            // No checkpoint either: the clustered tree is now replayed
            // (`a_clustered_table_reopened_without_a_checkpoint_is_redone`): the two halves
            // of this loop both come back from the journal.
            (table, ids)
        };

        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        for (position, key) in [30, 10, 20].into_iter().enumerate() {
            assert_eq!(
                storage.get(&settled(9), table, ids[position]).expect("get"),
                Some(pair(key, key + 1)),
                "clustered: {clustered}"
            );
        }
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        // A heap answers in `RowId` order, a clustered table in clustered-key order: that
        // is what says the rows came back from the store the shape asked for.
        let expected = if clustered {
            vec![
                (ids[1], pair(10, 11)),
                (ids[2], pair(20, 21)),
                (ids[0], pair(30, 31)),
            ]
        } else {
            vec![
                (ids[0], pair(30, 31)),
                (ids[1], pair(10, 11)),
                (ids[2], pair(20, 21)),
            ]
        };
        assert_eq!(rows, expected, "clustered: {clustered}");
        drop(storage);
        drop(dir);
    }
}

#[test]
fn unique_2601_on_disk() {
    for clustered in [false, true] {
        let (dir, storage) = empty_instance(if clustered {
            "trait-unique-clustered"
        } else {
            "trait-unique-heap"
        });
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(clustered))
            .expect("create the table");
        // On the second column, so that the index is one of its own on both stores.
        let index = storage
            .create_index(table, &unique_on(1))
            .expect("create the index");

        // Two rows whose first column, the clustered key of the clustered half, is
        // inserted in decreasing order: the `scan` below reads back in one order or the
        // other and says which store answered.
        let first = storage
            .insert(TxnId(1), table, &pair(3, 5))
            .expect("insert");
        let second = storage
            .insert(TxnId(1), table, &pair(1, 7))
            .expect("insert");
        storage.commit(TxnId(1)).expect("commit");

        let err = storage
            .insert(TxnId(2), table, &pair(2, 5))
            .expect_err("the second row holds the key of the first");
        assert_eq!(err.number, 2601, "clustered: {clustered}");
        assert!(err.message.contains(&index.to_string()), "{}", err.message);
        // The refused insert left no row: the check runs before the write.
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        let expected = if clustered {
            vec![(second, pair(1, 7)), (first, pair(3, 5))]
        } else {
            vec![(first, pair(3, 5)), (second, pair(1, 7))]
        };
        assert_eq!(rows, expected, "clustered: {clustered}");
        drop(dir);
    }
}

#[test]
fn one_begin_one_end_per_txn_over_two_tables() {
    let (dir, storage) = empty_instance("trait-one-begin");
    let db = storage.create_database("a").expect("create the database");
    let first = storage
        .create_table(db, &two_ints(false))
        .expect("create the first table");
    let second = storage
        .create_table(db, &two_ints(false))
        .expect("create the second table");

    storage
        .insert(TxnId(3), first, &pair(1, 1))
        .expect("insert in the first table");
    storage
        .insert(TxnId(3), second, &pair(2, 2))
        .expect("insert in the second table");
    storage.commit(TxnId(3)).expect("commit");

    let records = storage.wal.records().expect("read the journal");
    let mine: Vec<WalRecordKind> = records
        .iter()
        .filter(|record| record.txn == TxnId(3))
        .map(|record| record.kind)
        .collect();
    assert_eq!(
        mine,
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Insert,
            WalRecordKind::Commit
        ]
    );
    // Both rows are visible to a later snapshot: one status for the two tables.
    for table in [first, second] {
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(rows.len(), 1);
    }
    drop(dir);
}

#[test]
fn stale_version_is_refused_before_uniqueness() {
    let (dir, storage) = empty_instance("trait-stale-first");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(false))
        .expect("create the table");
    storage
        .create_index(table, &unique_on(1))
        .expect("create the index");

    let live = storage
        .insert(TxnId(1), table, &pair(1, 5))
        .expect("insert the row whose key 5 stays live");
    let stale = storage
        .insert(TxnId(1), table, &pair(2, 6))
        .expect("insert the row that goes stale");
    storage.commit(TxnId(1)).expect("commit");
    storage.delete(TxnId(2), table, stale).expect("delete");
    storage.commit(TxnId(2)).expect("commit");

    // The update would break the unique index too: the key 5 is held by a live row.
    let err = storage
        .update(TxnId(3), table, stale, &pair(2, 5))
        .expect_err("a stale row is refused");
    assert_ne!(err.number, 2601, "{}", err.message);
    assert!(err.message.contains("is stale"), "{}", err.message);
    // The same update on a row that is not stale does answer 2601, which is what makes the
    // order of the two checks visible.
    let other = storage
        .insert(TxnId(4), table, &pair(3, 7))
        .expect("insert a third row");
    storage.commit(TxnId(4)).expect("commit");
    let err = storage
        .update(TxnId(5), table, other, &pair(3, 5))
        .expect_err("the key 5 is taken");
    assert_eq!(err.number, 2601, "{}", err.message);
    assert_eq!(
        storage.get(&settled(9), table, live).expect("get"),
        Some(pair(1, 5))
    );
    drop(dir);
}

/// A database, a table of [`two_ints`] and a `unique` index on its second column, on
/// `storage`, with one committed row of key 5 and one transaction already committed.
///
/// `TxnId(1)` holds the live row `(1, 5)`; `TxnId(2)` wrote `(3, 7)` and committed, so it
/// is a finished transaction; `TxnId(5)` has written nothing yet.
fn table_with_a_unique_index(storage: &dyn Storage, clustered: bool) -> TableId {
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(clustered))
        .expect("create the table");
    storage
        .create_index(table, &unique_on(1))
        .expect("create the index");
    storage
        .insert(TxnId(1), table, &pair(1, 5))
        .expect("the row whose key 5 stays live");
    storage.commit(TxnId(1)).expect("commit");
    storage
        .insert(TxnId(2), table, &pair(3, 7))
        .expect("the write of the transaction that then ends");
    storage.commit(TxnId(2)).expect("commit");
    table
}

/// The error of `answer`, as the two implementations are compared on it.
fn failure<T: std::fmt::Debug>(answer: SqlResult<T>) -> SqlError {
    answer.expect_err("the call is refused")
}

#[test]
fn a_finished_txn_is_refused_before_uniqueness() {
    for clustered in [false, true] {
        let (dir, disk) = empty_instance(if clustered {
            "trait-finished-clustered"
        } else {
            "trait-finished-heap"
        });
        let memory = crate::MemoryStorage::new();
        let mut answers = Vec::new();
        for storage in [&disk as &dyn Storage, &memory as &dyn Storage] {
            let table = table_with_a_unique_index(storage, clustered);
            // `TxnId(2)` has committed, and the row would also take the key 5 of a live
            // row: the precondition of the store is what must be reported.
            let err = failure(storage.insert(TxnId(2), table, &pair(9, 5)));
            assert_ne!(err.number, 2601, "clustered: {clustered}, {}", err.message);
            assert!(
                err.message.contains("already Committed"),
                "clustered: {clustered}, {}",
                err.message
            );
            answers.push(err);
        }
        assert_eq!(
            (answers[0].number, &answers[0].message),
            (answers[1].number, &answers[1].message),
            "disk and memory answer the same, clustered: {clustered}"
        );
        drop(dir);
    }
}

#[test]
fn a_row_of_the_wrong_arity_is_refused_before_uniqueness() {
    for clustered in [false, true] {
        let (dir, disk) = empty_instance(if clustered {
            "trait-arity-clustered"
        } else {
            "trait-arity-heap"
        });
        let memory = crate::MemoryStorage::new();
        let mut answers = Vec::new();
        for storage in [&disk as &dyn Storage, &memory as &dyn Storage] {
            let table = table_with_a_unique_index(storage, clustered);
            // One value where the shape asks for two, and that value holds the key 5 of a
            // live row: the arity is what must be reported, as an `InternalError::Bug`.
            let err = failure(storage.insert(TxnId(5), table, &Row(vec![Value::I32(5)])));
            assert_eq!(err.number, 50000, "clustered: {clustered}, {}", err.message);
            assert!(
                err.message.contains("row has 1 values"),
                "clustered: {clustered}, {}",
                err.message
            );
            assert!(
                !err.message.contains("data corruption"),
                "clustered: {clustered}, {}",
                err.message
            );
            answers.push(err);
        }
        assert_eq!(
            (answers[0].number, &answers[0].message),
            (answers[1].number, &answers[1].message),
            "disk and memory answer the same, clustered: {clustered}"
        );
        drop(dir);
    }
}

#[test]
fn a_failed_store_of_leaves_the_state_of_the_table_in_place() {
    let (dir, storage) = empty_instance("trait-store-of-error");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(true))
        .expect("create the clustered table");
    assert_eq!(
        storage
            .insert(TxnId(1), table, &pair(1, 1))
            .expect("insert"),
        RowId(1)
    );

    // The catalogue loses the roots of the tree, so `store_of` fails after `with_rows_of`
    // has taken the state of the table out of its cell.
    let entry = storage
        .lock_catalogue()
        .expect("the catalogue")
        .table(table)
        .expect("the entry")
        .clone();
    let broken = TableEntry {
        tree_root: None,
        ..entry.clone()
    };
    storage
        .lock_catalogue()
        .expect("the catalogue")
        .add_table(broken);
    let err = storage
        .insert(TxnId(1), table, &pair(2, 2))
        .expect_err("the entry carries no tree");
    assert!(err.message.contains("no tree"), "{}", err.message);

    // The roots come back: the state of the table must be the one the first insert left,
    // not a `TableState::default()`.
    storage
        .lock_catalogue()
        .expect("the catalogue")
        .add_table(entry);
    assert_eq!(
        storage
            .insert(TxnId(1), table, &pair(3, 3))
            .expect("insert"),
        RowId(2),
        "the counter of the table survived the failed call"
    );
    storage.rollback(TxnId(1)).expect("rollback");
    for id in [RowId(1), RowId(2)] {
        assert_eq!(
            storage.latest_version(table, id).expect("latest_version"),
            None,
            "the undo log of the transaction survived the failed call"
        );
    }
    drop(dir);
}

#[test]
fn clustered_key_over_900_bytes_is_1946() {
    let (dir, storage) = empty_instance("trait-key-width");
    let db = storage.create_database("a").expect("create the database");

    let err = storage
        .create_table(db, &keyed_by(SqlType::Char(Len::Fixed(901))))
        .expect_err("a key of 901 declared bytes is refused");
    assert_eq!(err.number, 1946);
    assert_eq!(err.severity, 16);
    assert!(err.message.contains("901"), "{}", err.message);
    assert!(err.message.contains("900"), "{}", err.message);
    // The same shape one byte shorter is created, so 900 is the bound and not a guess.
    storage
        .create_table(db, &keyed_by(SqlType::Char(Len::Fixed(900))))
        .expect("a key of 900 declared bytes is taken");
    // `nchar` counts two bytes per character: 451 characters are 902 bytes.
    let err = storage
        .create_table(db, &keyed_by(SqlType::NChar(Len::Fixed(451))))
        .expect_err("451 characters of nchar are 902 bytes");
    assert_eq!(err.number, 1946);
    assert!(err.message.contains("902"), "{}", err.message);
    assert_eq!(storage.tables(db).expect("tables").len(), 1);
    drop(dir);
}

#[test]
fn a_max_column_in_a_clustered_key_is_1946() {
    let (dir, storage) = empty_instance("trait-key-max");
    let db = storage.create_database("a").expect("create the database");

    let err = storage
        .create_table(db, &keyed_by(SqlType::VarChar(Len::Max)))
        .expect_err("a max column has no declared width");
    assert_eq!(err.number, 1946);
    assert!(err.message.contains("unbounded"), "{}", err.message);
    assert_eq!(storage.tables(db).expect("tables"), Vec::new());
    drop(dir);
}

#[test]
fn index_seek_on_clustered_table() {
    let (dir, storage) = empty_instance("trait-index-clustered");
    let db = storage.create_database("a").expect("create the database");
    let table = storage
        .create_table(db, &two_ints(true))
        .expect("create the clustered table");
    // A shape other than the clustered key, kept up to date at each write.
    let index = storage
        .create_index(table, &index_on(1))
        .expect("a non-clustered index on a clustered table");

    let mut ids = Vec::new();
    for (key, indexed) in [(30, 5), (10, 7), (20, 5)] {
        ids.push(
            storage
                .insert(TxnId(1), table, &pair(key, indexed))
                .expect("insert"),
        );
    }
    storage.commit(TxnId(1)).expect("commit");

    let found: Vec<_> = storage
        .seek(
            &settled(9),
            index,
            &KeyRange::Point(vec![Value::I32(5)]),
            Direction::Forward,
        )
        .expect("seek")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows");
    assert_eq!(
        found,
        vec![(ids[0], pair(30, 5)), (ids[2], pair(20, 5))],
        "the two rows of key 5, by increasing RowId"
    );
    // The table answering the `seek` is the clustered one: its `scan` is in clustered-key
    // order, where a heap would answer in the insertion order 30, 10, 20.
    let rows: Vec<_> = storage
        .scan(&settled(9), table)
        .expect("scan")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows");
    assert_eq!(
        rows,
        vec![
            (ids[1], pair(10, 7)),
            (ids[2], pair(20, 5)),
            (ids[0], pair(30, 5)),
        ]
    );

    // The maintenance follows an update: the row leaves the key 5 for the key 9.
    storage
        .update(TxnId(2), table, ids[0], &pair(30, 9))
        .expect("update");
    storage.commit(TxnId(2)).expect("commit");
    let found: Vec<_> = storage
        .seek(
            &settled(9),
            index,
            &KeyRange::Point(vec![Value::I32(5)]),
            Direction::Forward,
        )
        .expect("seek")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows");
    assert_eq!(found, vec![(ids[2], pair(20, 5))]);
    drop(dir);
}

#[test]
fn a_clustered_table_reopened_without_a_checkpoint_is_redone() {
    let dir = TempDir::created("trait-reopen-no-checkpoint");
    let (heap, clustered, id) = {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        let db = storage.create_database("a").expect("create the database");
        let heap = storage
            .create_table(db, &two_ints(false))
            .expect("create the heap");
        let clustered = storage
            .create_table(db, &two_ints(true))
            .expect("create the clustered table");
        storage
            .insert(TxnId(1), heap, &pair(7, 11))
            .expect("insert in the heap");
        let id = storage
            .insert(TxnId(1), clustered, &pair(7, 11))
            .expect("insert in the clustered table");
        storage.commit(TxnId(1)).expect("commit");
        (heap, clustered, id)
    };

    // No checkpoint: the rows of the heap and of the clustered table come back from the
    // journal, and the tree of the clustered table is replayed on the pages
    // [`ClusteredTable::redo_open`] formats as empty leaves.
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    let rows: Vec<_> = storage
        .scan(&settled(9), heap)
        .expect("scan the heap")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows of the heap");
    assert_eq!(rows.len(), 1, "the heap comes back from the journal");
    let rows: Vec<_> = storage
        .scan(&settled(9), clustered)
        .expect("scan the clustered table")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows of the clustered table");
    assert_eq!(
        rows,
        vec![(id, pair(7, 11))],
        "the tree is replayed from the journal"
    );
    assert_eq!(
        storage.get(&settled(9), clustered, id).expect("get"),
        Some(pair(7, 11))
    );
    drop(storage);
    drop(dir);
}

#[test]
fn a_reopen_hands_out_row_ids_past_the_ones_the_journal_names() {
    for clustered in [false, true] {
        let dir = TempDir::created(if clustered {
            "trait-resume-clustered"
        } else {
            "trait-resume-heap"
        });
        let table = {
            let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
            let db = storage.create_database("a").expect("create the database");
            let table = storage
                .create_table(db, &two_ints(clustered))
                .expect("create the table");
            for key in [1, 2, 3] {
                assert_eq!(
                    storage
                        .insert(TxnId(1), table, &pair(key, key))
                        .expect("insert"),
                    RowId(u64::try_from(key).expect("a small key"))
                );
            }
            storage.commit(TxnId(1)).expect("commit");
            if clustered {
                storage.checkpoint().expect("checkpoint");
            }
            table
        };

        // The state of the table is rebuilt from what the recovery of the `open` found:
        // the fourth row takes `RowId(4)`, not the `RowId(1)` a counter starting over
        // would hand out a second time.
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
        assert_eq!(
            storage
                .insert(TxnId(2), table, &pair(4, 4))
                .expect("insert after the reopen"),
            RowId(4),
            "clustered: {clustered}"
        );
        storage.commit(TxnId(2)).expect("commit");
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(rows.len(), 4, "clustered: {clustered}");
        // A transaction the journal has already ended may not write again: the register of
        // the instance took the winners of the recovery.
        let err = storage
            .insert(TxnId(1), table, &pair(5, 5))
            .expect_err("TxnId(1) is a winner of the recovery");
        assert_eq!(err.number, 50000, "{}", err.message);
        assert!(err.message.contains("Committed"), "{}", err.message);
        drop(storage);
        drop(dir);
    }
}

#[test]
fn savepoint_rollback_to_partial() {
    let dir = TempDir::created("trait-savepoint");
    let (db, table, first_two) = {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        let db = storage.create_database("a").expect("create the database");
        let table = storage
            .create_table(db, &two_ints(false))
            .expect("create the table");
        let a = storage
            .insert(TxnId(1), table, &pair(1, 1))
            .expect("insert a");
        let b = storage
            .insert(TxnId(1), table, &pair(2, 2))
            .expect("insert b");
        let sp = storage.savepoint(TxnId(1)).expect("savepoint");
        let c = storage
            .insert(TxnId(1), table, &pair(3, 3))
            .expect("insert c");
        assert_eq!(c, RowId(3));
        storage
            .rollback_to(TxnId(1), sp)
            .expect("rollback to savepoint");
        storage.commit(TxnId(1)).expect("commit");
        (db, table, vec![a, b])
    };
    let _ = db;

    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    let rows: Vec<_> = storage
        .scan(&settled(9), table)
        .expect("scan")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], (first_two[0], pair(1, 1)));
    assert_eq!(rows[1], (first_two[1], pair(2, 2)));
    assert!(
        storage
            .wal
            .records()
            .expect("journal")
            .iter()
            .any(|r| r.kind == WalRecordKind::Savepoint),
        "the savepoint is journalled"
    );
    assert!(
        storage
            .wal
            .records()
            .expect("journal")
            .iter()
            .any(|r| r.kind == WalRecordKind::RollbackTo),
        "the rollback to savepoint is journalled"
    );
    drop(storage);
    drop(dir);
}

#[test]
fn vacuum_removes_dead_keeps_horizon() {
    let (dir, storage) = empty_instance("trait-vacuum-heap");
    let db = storage.create_database("a").expect("database");
    let table = storage.create_table(db, &two_ints(false)).expect("table");
    let a = storage.insert(TxnId(1), table, &pair(1, 1)).expect("a");
    let b = storage.insert(TxnId(1), table, &pair(2, 2)).expect("b");
    storage.commit(TxnId(1)).expect("commit 1");
    storage.delete(TxnId(2), table, a).expect("delete a");
    storage.commit(TxnId(2)).expect("commit 2");
    // A snapshot taken while TxnId(3) still runs: `xmax` does not settle 3, `active`
    // holds it, so 3's own write of b is not visible to it.
    let held = Snapshot {
        xmin: TxnId(1),
        xmax: TxnId(4),
        active: vec![TxnId(3)],
        own: TxnId(2),
    };
    storage
        .update(TxnId(3), table, b, &pair(20, 2))
        .expect("update b");
    storage.commit(TxnId(3)).expect("commit 3");

    storage.vacuum(TxnId(3)).expect("vacuum at horizon 3");
    assert_eq!(
        storage.get(&held, table, b).expect("old b visible"),
        Some(pair(2, 2)),
        "the version a snapshot still holds is kept"
    );
    storage.vacuum(TxnId(4)).expect("vacuum at horizon 4");
    assert_eq!(storage.get(&settled(9), table, a).expect("a gone"), None);
    assert_eq!(
        storage.get(&settled(9), table, b).expect("new b"),
        Some(pair(20, 2))
    );
    let c = storage
        .insert(TxnId(5), table, &pair(3, 3))
        .expect("row ids are not reused");
    assert_eq!(c, RowId(3));
    drop(storage);
    drop(dir);
}

#[test]
fn vacuum_drops_index_entries_of_dead_versions() {
    let (dir, storage) = empty_instance("trait-vacuum-index");
    let db = storage.create_database("a").expect("database");
    let table = storage.create_table(db, &two_ints(false)).expect("table");
    // Indexed on column 0, the one the update moves from 2 to 20.
    let index = storage.create_index(table, &index_on(0)).expect("index");
    let b = storage
        .insert(TxnId(1), table, &pair(2, 2))
        .expect("b at key 2");
    storage.commit(TxnId(1)).expect("commit");
    storage
        .update(TxnId(2), table, b, &pair(20, 2))
        .expect("move b to 20");
    storage.commit(TxnId(2)).expect("commit");
    // A snapshot taken while TxnId(2) still runs: it reads b under its old key.
    let older = Snapshot {
        xmin: TxnId(1),
        xmax: TxnId(3),
        active: vec![TxnId(2)],
        own: TxnId(1),
    };
    let before: Vec<_> = storage
        .seek(
            &older,
            index,
            &KeyRange::Point(vec![vauban_types::Value::I32(2)]),
            Direction::Forward,
        )
        .expect("seek the old key")
        .collect::<SqlResult<Vec<_>>>()
        .expect("rows");
    assert_eq!(before, vec![(b, pair(2, 2))]);
    storage.vacuum(TxnId(3)).expect("vacuum");
    let after: Vec<_> = storage
        .seek(
            &settled(9),
            index,
            &KeyRange::Point(vec![vauban_types::Value::I32(2)]),
            Direction::Forward,
        )
        .expect("seek")
        .collect::<SqlResult<Vec<_>>>()
        .expect("rows");
    assert!(
        after.is_empty(),
        "the index lost the dead version: {after:?}"
    );
    drop(storage);
    drop(dir);
}

#[test]
fn indexes_are_rebuilt_at_open() {
    let dir = TempDir::created("trait-index-reopen");
    let (table, index, ids) = {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        let db = storage.create_database("a").expect("database");
        let table = storage.create_table(db, &two_ints(false)).expect("table");
        let index = storage.create_index(table, &index_on(0)).expect("index");
        let mut ids = Vec::new();
        for key in [1i32, 2, 3] {
            ids.push(
                storage
                    .insert(TxnId(1), table, &pair(key, 0))
                    .expect("insert"),
            );
        }
        storage.commit(TxnId(1)).expect("commit");
        (table, index, ids)
    };
    let _ = table;
    // No checkpoint: the tree pages of the index stayed in the pool and are gone; the row
    // pages themselves come back from the heap and the journal.
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    let rows: Vec<_> = storage
        .seek(
            &settled(9),
            index,
            &KeyRange::Point(vec![vauban_types::Value::I32(2)]),
            Direction::Forward,
        )
        .expect("seek")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows");
    assert_eq!(rows, vec![(ids[1], pair(2, 0))], "the index is back");
    drop(storage);
    drop(dir);
}

#[test]
fn a_loser_xmax_on_data_is_cleared() {
    let dir = TempDir::created("trait-loser-xmax");
    let (table, id) = {
        let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("open");
        let db = storage.create_database("a").expect("database");
        let table = storage.create_table(db, &two_ints(false)).expect("table");
        let id = storage
            .insert(TxnId(1), table, &pair(1, 1))
            .expect("insert");
        storage.commit(TxnId(1)).expect("commit 1");
        // TxnId(2) deletes the row; its page is shared with a later committed write of
        // TxnId(3) that reaches `data`, which carries the loser's `xmax`.
        storage.delete(TxnId(2), table, id).expect("delete");
        storage
            .insert(TxnId(3), table, &pair(9, 9))
            .expect("second write");
        storage.commit(TxnId(3)).expect("commit 3");
        storage.checkpoint().expect("checkpoint");
        storage.rollback(TxnId(2)).expect("rollback 2");
        // The rollback replays the undo and clears the xmax on `data`'s copy too; a build
        // that left it there is what this test would catch through the reopen below.
        (table, id)
    };
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    assert_eq!(
        storage.get(&settled(9), table, id).expect("get"),
        Some(pair(1, 1)),
        "the row of a rolled-back delete is back"
    );
    storage
        .update(TxnId(4), table, id, &pair(2, 2))
        .expect("update a row whose loser xmax is cleared");
    storage.commit(TxnId(4)).expect("commit");
    drop(storage);
    drop(dir);
}

#[test]
fn rollback_writes_one_abort_and_takes_back_both_tables() {
    let (dir, storage) = empty_instance("trait-rollback");
    let db = storage.create_database("a").expect("create the database");
    let heap = storage
        .create_table(db, &two_ints(false))
        .expect("create the heap");
    let clustered = storage
        .create_table(db, &two_ints(true))
        .expect("create the clustered table");
    let index = storage
        .create_index(heap, &index_on(1))
        .expect("create the index");

    storage
        .insert(TxnId(3), heap, &pair(1, 5))
        .expect("insert in the heap");
    storage
        .insert(TxnId(3), clustered, &pair(2, 5))
        .expect("insert in the clustered table");
    storage.rollback(TxnId(3)).expect("rollback");

    let mine: Vec<WalRecordKind> = storage
        .wal
        .records()
        .expect("read the journal")
        .iter()
        .filter(|record| record.txn == TxnId(3))
        .map(|record| record.kind)
        .collect();
    assert_eq!(
        mine,
        vec![
            WalRecordKind::Begin,
            WalRecordKind::Insert,
            WalRecordKind::Insert,
            WalRecordKind::Abort
        ]
    );
    for table in [heap, clustered] {
        let rows: Vec<_> = storage
            .scan(&settled(9), table)
            .expect("scan")
            .collect::<SqlResult<Vec<_>>>()
            .expect("the rows");
        assert_eq!(rows, Vec::new());
    }
    // The versions themselves are gone, not merely hidden: `latest_version` reads the
    // directory of the store and no longer finds the row.
    for (table, id) in [(heap, RowId(1)), (clustered, RowId(2))] {
        assert_eq!(
            storage.latest_version(table, id).expect("latest_version"),
            None
        );
    }
    // The index of the heap lost the entry of the undone insert too.
    let found: Vec<_> = storage
        .seek(
            &settled(9),
            index,
            &KeyRange::Point(vec![Value::I32(5)]),
            Direction::Forward,
        )
        .expect("seek")
        .collect::<SqlResult<Vec<_>>>()
        .expect("the rows");
    assert_eq!(found, Vec::new());
    drop(dir);
}

#[test]
fn a_second_open_of_the_same_directory_is_refused() {
    let (dir, storage) = empty_instance("ddl-second-open");
    let db = storage.create_database("a").expect("create the database");

    let err = DiskStorage::open(dir.path(), DiskOptions::default())
        .expect_err("a second instance on one directory is refused");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("already open"), "{err}");

    // The first instance goes on writing.
    let table = storage
        .create_table(db, &two_ints(false))
        .expect("the first instance writes on");
    assert_eq!(storage.tables(db).expect("tables").len(), 1);
    drop(storage);

    // Once it is dropped, the directory opens again, with what it held.
    let reopened = reopen(&dir);
    assert_eq!(
        reopened.tables(db).expect("tables"),
        vec![(table, two_ints(false))]
    );
}

#[test]
fn drop_database_takes_its_tables_and_indexes() {
    let (dir, storage) = empty_instance("ddl-drop-database");
    let (db, table, _index) = create_the_three(&storage, false);
    let other = storage.create_database("b").expect("create the database");
    let kept = storage
        .create_table(other, &two_ints(false))
        .expect("create the table");

    storage.drop_database(db).expect("drop the database");
    assert_eq!(
        storage.databases().expect("databases"),
        vec![(other, "b".to_string())]
    );
    assert_eq!(
        storage.tables(other).expect("tables"),
        vec![(kept, two_ints(false))]
    );
    // The dropped database, its table and the index of that table have left the
    // catalogue.
    for message in [
        storage.tables(db).unwrap_err().message,
        storage.indexes(table).unwrap_err().message,
        storage.drop_table(table).unwrap_err().message,
        storage.drop_database(db).unwrap_err().message,
    ] {
        assert!(message.contains("unknown to instance"), "{message}");
    }
    drop(dir);
}

#[test]
fn drop_index_frees_its_tree_and_leaves_the_rows() {
    let (dir, storage) = empty_instance("ddl-drop-index");
    let (db, table, index) = create_the_three(&storage, false);
    let before = control_of(&storage).next_page_id;
    assert_eq!(
        storage.indexes(table).expect("indexes"),
        vec![(index, unique_on(0))]
    );

    storage.drop_index(index).expect("drop the index");
    assert_eq!(storage.indexes(table).expect("indexes"), Vec::new());
    assert_eq!(
        storage.tables(db).expect("tables"),
        vec![(table, two_ints(false))],
        "the table is untouched"
    );
    // The page of the tree went back to the free list.
    assert_eq!(control_of(&storage).next_page_id, before);
    assert!(control_of(&storage).free_head.is_some());
    assert!(
        storage.drop_index(index).is_err(),
        "the index is unknown now"
    );
    drop(dir);
}

#[test]
fn an_identifier_is_not_handed_out_twice_after_a_refused_statement() {
    let (dir, storage) = empty_instance("ddl-identifiers");
    let db = storage.create_database("a").expect("create the database");
    assert_eq!(db, DbId(1));
    // A shape this build refuses, checked before an identifier is taken.
    let empty = TableShape {
        columns: Vec::new(),
        clustered_key: None,
    };
    assert!(storage.create_table(db, &empty).is_err());
    assert_eq!(control_of(&storage).next_table_id, 1, "no identifier taken");

    let table = storage
        .create_table(db, &two_ints(false))
        .expect("create the table");
    assert_eq!(table, TableId(1));
    // An index shape naming a column outside the table is refused before the identifier.
    assert!(storage.create_index(table, &unique_on(9)).is_err());
    assert_eq!(control_of(&storage).next_index_id, 1);
    // A duplicate key takes the identifier and creates nothing (2601).
    let entry_page = storage
        .lock_catalogue()
        .expect("the catalogue")
        .table(table)
        .expect("the table")
        .first_page;
    {
        let mut rows = HeapTable::open(&storage, table, entry_page, two_ints(false));
        for _ in 0..2 {
            rows.insert(TxnId(1), &Row(vec![Value::I32(7), Value::Null]))
                .expect("insert");
        }
        rows.commit(TxnId(1)).expect("commit");
    }
    assert_eq!(
        storage
            .create_index(table, &unique_on(0))
            .unwrap_err()
            .number,
        2601
    );
    assert_eq!(storage.indexes(table).expect("indexes"), Vec::new());
    assert_eq!(
        storage
            .create_index(
                table,
                &IndexShape {
                    columns: vec![KeyColumn {
                        column: 0,
                        descending: false,
                    }],
                    unique: false,
                    included: Vec::new(),
                }
            )
            .expect("a non-unique index passes"),
        IndexId(2),
        "the identifier the refused statement took is not handed out again"
    );
    drop(dir);
}

#[test]
fn a_table_of_an_unknown_database_is_refused() {
    let (dir, storage) = empty_instance("ddl-unknown-db");
    let err = storage
        .create_table(DbId(4), &two_ints(false))
        .expect_err("an unknown database is refused");
    assert!(
        err.message.contains("database 4 is unknown"),
        "{}",
        err.message
    );
    assert_eq!(control_of(&storage).next_table_id, 1);

    let db = storage.create_database("a").expect("create the database");
    let clustered = TableShape {
        columns: vec![TypeInfo::new(SqlType::Int, true)],
        clustered_key: Some(vec![KeyColumn {
            column: 3,
            descending: false,
        }]),
    };
    let err = storage
        .create_table(db, &clustered)
        .expect_err("a key outside the shape is refused");
    assert!(err.message.contains("column 3"), "{}", err.message);
    drop(dir);
}
