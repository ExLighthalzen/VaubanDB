use vauban_types::SqlType;

use super::super::buffer::DurableLsn;
use super::super::temp::TempDir;
use super::super::{DiskOptions, DiskStorage};
use super::*;
use crate::TxnId;

/// An instance in a temporary directory, with the guard that removes it.
fn instance(label: &str) -> (TempDir, DiskStorage) {
    let dir = TempDir::created(label);
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    (dir, storage)
}

/// One instance of each of the 24 variants of [`SqlType`], in declaration order.
///
/// The list is written out rather than derived: what forces a variant added to [`SqlType`]
/// to be handled is the `match` of [`put_sql_type`], which is exhaustive and stops the
/// build; this list is what checks the tag it takes reads back.
fn all_types() -> Vec<SqlType> {
    vec![
        SqlType::Bit,
        SqlType::TinyInt,
        SqlType::SmallInt,
        SqlType::Int,
        SqlType::BigInt,
        SqlType::Decimal {
            precision: 18,
            scale: 4,
        },
        SqlType::Numeric {
            precision: 5,
            scale: 0,
        },
        SqlType::Float,
        SqlType::Real,
        SqlType::Money,
        SqlType::SmallMoney,
        SqlType::Char(Len::Fixed(10)),
        SqlType::VarChar(Len::Max),
        SqlType::NChar(Len::Fixed(4000)),
        SqlType::NVarChar(Len::Max),
        SqlType::Binary(Len::Fixed(1)),
        SqlType::VarBinary(Len::Max),
        SqlType::Date,
        SqlType::Time(7),
        SqlType::DateTime,
        SqlType::SmallDateTime,
        SqlType::DateTime2(3),
        SqlType::DateTimeOffset(0),
        SqlType::UniqueIdentifier,
    ]
}

/// A table of `columns` columns of type `int`, the first `key` of them clustered.
fn shape(columns: usize, key: Option<Vec<u16>>) -> TableShape {
    TableShape {
        columns: (0..columns)
            .map(|_| TypeInfo::new(SqlType::Int, true))
            .collect(),
        clustered_key: key.map(|columns| {
            columns
                .into_iter()
                .map(|column| KeyColumn {
                    column,
                    descending: column % 2 == 1,
                })
                .collect()
        }),
    }
}

/// A catalogue of one database, two tables and two indexes.
fn sample() -> Catalogue {
    let mut catalogue = Catalogue::default();
    catalogue.add_database(DbId(1), "Ålesund");
    catalogue.add_table(TableEntry {
        table: TableId(1),
        db: DbId(1),
        shape: TableShape {
            columns: all_types()
                .into_iter()
                .map(|ty| TypeInfo::new(ty, true))
                .collect(),
            clustered_key: None,
        },
        first_page: PageId(3),
        tree_root: None,
        directory_root: None,
        next_row_id: 1,
    });
    catalogue.add_table(TableEntry {
        table: TableId(2),
        db: DbId(1),
        shape: shape(3, Some(vec![2, 0])),
        first_page: PageId(4),
        tree_root: Some(PageId(5)),
        directory_root: Some(PageId(6)),
        next_row_id: 77,
    });
    catalogue.add_index(IndexEntry {
        index: IndexId(1),
        table: TableId(1),
        shape: IndexShape {
            columns: vec![KeyColumn {
                column: 0,
                descending: true,
            }],
            unique: true,
            included: vec![1, 2],
        },
        root: Some(PageId(7)),
    });
    catalogue.add_index(IndexEntry {
        index: IndexId(2),
        table: TableId(2),
        shape: IndexShape {
            columns: vec![
                KeyColumn {
                    column: 2,
                    descending: false,
                },
                KeyColumn {
                    column: 0,
                    descending: true,
                },
            ],
            unique: false,
            included: Vec::new(),
        },
        root: None,
    });
    catalogue
}

#[test]
fn every_sql_type_reads_back_as_the_column_it_was() {
    let types = all_types();
    assert_eq!(types.len(), 24, "one instance of each variant of SqlType");
    let mut tags = Vec::new();
    for ty in types {
        for nullable in [false, true] {
            let info = TypeInfo::new(ty, nullable);
            let mut bytes = Vec::new();
            put_type_info(&mut bytes, &info);
            tags.push(bytes[0]);
            let mut cursor = Cursor::over(&bytes, "a column");
            assert_eq!(take_type_info(&mut cursor).expect("read back"), info);
            assert_eq!(cursor.at, bytes.len(), "the whole column was read");
        }
    }
    // A tag per variant, and no two variants sharing one: a permutation of two arms of
    // `take_sql_type` is what this catches.
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(tags.len(), 24);
}

#[test]
fn a_collation_reads_back_with_its_four_fields() {
    let info = TypeInfo {
        ty: SqlType::VarChar(Len::Fixed(20)),
        nullable: false,
        collation: Some(Collation {
            lcid: 0x040c,
            flags: 0x1d,
            version: 2,
            sort_id: 52,
        }),
    };
    let mut bytes = Vec::new();
    put_type_info(&mut bytes, &info);
    let read = take_type_info(&mut Cursor::over(&bytes, "a column")).expect("read back");
    assert_eq!(read, info);
    assert_eq!(
        read.collation
            .map(|c| (c.lcid, c.flags, c.version, c.sort_id)),
        Some((0x040c, 0x1d, 2, 52))
    );
    // A column whose collation is `None` reads back the same way.
    let plain = TypeInfo::new(SqlType::Int, true);
    let mut bytes = Vec::new();
    put_type_info(&mut bytes, &plain);
    assert_eq!(
        take_type_info(&mut Cursor::over(&bytes, "a column")).expect("read back"),
        plain
    );
}

#[test]
fn a_catalogue_reads_back_from_its_stream() {
    let catalogue = sample();
    let read = Catalogue::decode(&catalogue.encode()).expect("read the stream back");
    assert_eq!(read, catalogue);
    assert_eq!(read.databases(), vec![(DbId(1), "Ålesund".to_string())]);
    assert_eq!(
        read.tables_of(DbId(1))
            .into_iter()
            .map(|entry| entry.table)
            .collect::<Vec<_>>(),
        vec![TableId(1), TableId(2)]
    );
    assert_eq!(read.indexes_of(TableId(2)).len(), 1);
    assert_eq!(read.index(IndexId(2)).expect("the index").root, None);
    assert_eq!(
        read.table(TableId(2)).expect("the table").tree_root,
        Some(PageId(5))
    );
    assert_eq!(read.largest_ids(), (1, 2, 2));
    assert_eq!(Catalogue::default().largest_ids(), (0, 0, 0));
}

#[test]
fn a_table_entry_opens_with_its_table_and_head_page() {
    let entry = TableEntry {
        table: TableId(9),
        db: DbId(2),
        shape: shape(2, None),
        first_page: PageId(5),
        tree_root: None,
        directory_root: None,
        next_row_id: 1,
    };
    let bytes = entry.encode();
    // The twelve bytes the payload of a `CreateTable` record opens with.
    assert_eq!(&bytes[0..4], &9u32.to_le_bytes());
    assert_eq!(&bytes[4..12], &5u64.to_le_bytes());
    assert_eq!(TableEntry::decode(&bytes).expect("read back"), entry);
}

#[test]
fn a_stream_of_another_version_is_corruption() {
    let mut bytes = sample().encode();
    bytes[0] = 2;
    let err = Catalogue::decode(&bytes).expect_err("version 2 is refused");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("format version 2"), "{err}");
}

#[test]
fn a_table_entry_short_of_its_shape_is_corruption() {
    let bytes = sample().table(TableId(2)).expect("the table").encode();
    for len in [0, 12, bytes.len() - 1] {
        let err = TableEntry::decode(&bytes[..len]).expect_err("a short entry is refused");
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("short of a field"), "{err}");
    }
    assert!(TableEntry::decode(&bytes).is_ok());
}

#[test]
fn an_instance_without_a_root_loads_an_empty_catalogue() {
    let (_dir, storage) = instance("meta-empty");
    assert_eq!(storage.control().expect("the ctl").meta_root, None);
    assert_eq!(
        load(&storage, None).expect("load"),
        Catalogue::default(),
        "an instance that has written no DDL"
    );
}

#[test]
fn a_catalogue_reads_back_from_the_pages_it_was_written_to() {
    let (_dir, storage) = instance("meta-pages");
    let catalogue = sample();
    let root = ensure_root(&storage).expect("allocate the root");
    assert_eq!(storage.control().expect("the ctl").meta_root, Some(root));
    store(&storage, root, &catalogue, Lsn(0)).expect("store");

    // Read back through the pool, then through the file: the pages are written, not kept.
    assert_eq!(load(&storage, Some(root)).expect("load"), catalogue);
    storage.pool.flush_all().expect("flush the pages");
    assert_eq!(load(&storage, Some(root)).expect("load"), catalogue);
}

#[test]
fn a_catalogue_larger_than_one_page_spans_the_chain() {
    let (_dir, storage) = instance("meta-chain");
    let mut catalogue = Catalogue::default();
    catalogue.add_database(DbId(1), "wide");
    for table in 1..=200u32 {
        catalogue.add_table(TableEntry {
            table: TableId(table),
            db: DbId(1),
            shape: shape(10, None),
            first_page: PageId(u64::from(table)),
            tree_root: None,
            directory_root: None,
            next_row_id: 1,
        });
    }
    let stream = catalogue.encode();
    assert!(
        stream.len() > CHUNK_CAPACITY,
        "{} bytes, one page carries {CHUNK_CAPACITY}",
        stream.len()
    );
    let root = ensure_root(&storage).expect("allocate the root");
    store(&storage, root, &catalogue, Lsn(0)).expect("store");
    let walk = chain(&storage, root).expect("walk the chain");
    assert!(walk.whole);
    assert_eq!(walk.pages.len(), stream.len().div_ceil(CHUNK_CAPACITY));
    assert_eq!(load(&storage, Some(root)).expect("load"), catalogue);

    // A catalogue that shrinks keeps its chain and empties the pages it no longer fills.
    let small = sample();
    store(&storage, root, &small, Lsn(0)).expect("store the smaller one");
    assert_eq!(
        chain(&storage, root).expect("walk the chain").pages.len(),
        walk.pages.len(),
        "the pages stay chained"
    );
    assert_eq!(load(&storage, Some(root)).expect("load"), small);
}

#[test]
fn a_page_of_the_chain_that_is_not_a_meta_page_is_corruption() {
    let (_dir, storage) = instance("meta-kind");
    let root = ensure_root(&storage).expect("allocate the root");
    store(&storage, root, &sample(), Lsn(0)).expect("store");
    let pin = storage.pool.pin(root).expect("pin the root");
    pin.with_page_mut(|page| page.set_kind(PageKind::Heap))
        .expect("change the kind");
    storage.pool.mark_dirty(root, Lsn(0)).expect("mark dirty");
    drop(pin);

    let err = load(&storage, Some(root)).expect_err("a heap page is refused");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("is a Heap page"), "{err}");
}

#[test]
fn the_ddl_records_rebuild_the_entries_they_named() {
    let catalogue = sample();
    let mut replayed = Catalogue::default();
    let records = [
        (
            WalRecordKind::CreateDatabase,
            create_database_payload(DbId(1), "Ålesund"),
        ),
        (
            WalRecordKind::CreateTable,
            catalogue.table(TableId(1)).expect("table 1").encode(),
        ),
        (
            WalRecordKind::CreateTable,
            catalogue.table(TableId(2)).expect("table 2").encode(),
        ),
        (
            WalRecordKind::CreateIndex,
            catalogue.index(IndexId(1)).expect("index 1").encode(),
        ),
        (
            WalRecordKind::CreateIndex,
            catalogue.index(IndexId(2)).expect("index 2").encode(),
        ),
    ];
    for (kind, payload) in &records {
        assert!(
            replayed
                .apply_ddl(&record(*kind, payload.clone()))
                .expect("apply")
        );
    }
    assert_eq!(replayed, catalogue, "the DDL records rebuild the catalogue");

    // Replaying them over the catalogue they built changes nothing.
    for (kind, payload) in &records {
        replayed
            .apply_ddl(&record(*kind, payload.clone()))
            .expect("apply again");
    }
    assert_eq!(replayed, catalogue);

    // A row record is not DDL, and a drop takes the indexes of its table with it.
    assert!(
        !replayed
            .apply_ddl(&record(WalRecordKind::Insert, Vec::new()))
            .expect("an insert is no DDL")
    );
    replayed
        .apply_ddl(&record(
            WalRecordKind::DropTable,
            identifier_payload(TableId(1).0),
        ))
        .expect("apply the drop");
    assert_eq!(replayed.table(TableId(1)), None);
    assert_eq!(replayed.index(IndexId(1)), None);
    assert_eq!(replayed.indexes_of(TableId(2)).len(), 1);

    // Dropping the database takes the table that was left and its index.
    replayed
        .apply_ddl(&record(
            WalRecordKind::DropDatabase,
            identifier_payload(DbId(1).0),
        ))
        .expect("apply the drop");
    assert_eq!(replayed, Catalogue::default());
}

/// A record of the journal carrying `payload`, numbered as the first one.
fn record(kind: WalRecordKind, payload: Vec<u8>) -> WalRecord {
    WalRecord {
        kind,
        lsn: Lsn(1),
        prev_lsn: Lsn(0),
        txn: TxnId(0),
        payload,
    }
}

/// A catalogue of one database and `tables` heap tables of ten columns, numbered from 1.
fn wide_catalogue(tables: u32) -> Catalogue {
    let mut catalogue = Catalogue::default();
    catalogue.add_database(DbId(1), "wide");
    for table in 1..=tables {
        catalogue.add_table(TableEntry {
            table: TableId(table),
            db: DbId(1),
            shape: shape(10, None),
            first_page: PageId(u64::from(table)),
            tree_root: None,
            directory_root: None,
            next_row_id: 1,
        });
    }
    catalogue
}

/// The page records of the journal of `storage`.
fn page_records(storage: &DiskStorage) -> usize {
    storage
        .wal
        .records()
        .expect("read the journal")
        .iter()
        .filter(|record| record.kind == WalRecordKind::PageWrite)
        .count()
}

#[test]
fn a_chain_whose_pages_missed_data_reads_back_from_the_journal() {
    let dir = TempDir::created("meta-torn-chain");
    let (root, pages, larger) = {
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        let root = ensure_root(&storage).expect("allocate the root");
        store(&storage, root, &wide_catalogue(150), Lsn(0)).expect("store");
        storage.checkpoint().expect("the chain reaches data");
        // A larger catalogue: the chain grows by a page, and its first page alone reaches
        // `data`; the others stay in the pool the drop throws away.
        let larger = wide_catalogue(230);
        store(&storage, root, &larger, Lsn(0)).expect("store the larger one");
        let pages = chain(&storage, root).expect("walk the chain").pages;
        assert!(pages.len() >= 3, "{} pages", pages.len());
        storage.pool.flush(pages[0]).expect("flush the first page");
        (root, pages, larger)
    };
    let storage = DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen");
    // `data` holds the first page of the larger chain beside the others of the smaller one.
    let on_data = storage
        .data
        .read_page(pages[0])
        .expect("read the first page");
    let behind = storage
        .data
        .read_page(pages[1])
        .expect("read the second page");
    assert!(
        behind.lsn() < on_data.lsn(),
        "the second page is the older one"
    );
    assert_eq!(load(&storage, Some(root)).expect("load"), larger);
    assert_eq!(
        storage
            .lock_catalogue()
            .expect("the catalogue")
            .tables_of(DbId(1))
            .len(),
        230
    );
}

#[test]
fn a_chain_cut_by_a_free_page_is_an_error() {
    let (_dir, storage) = instance("meta-cut-chain");
    let root = ensure_root(&storage).expect("allocate the root");
    store(&storage, root, &wide_catalogue(230), Lsn(0)).expect("store");
    let pages = chain(&storage, root).expect("walk the chain").pages;
    // The second page reads back as a free page past the record that wrote it, which the
    // redo of the catalogue pages therefore leaves as it stands.
    let pin = storage.pool.pin(pages[1]).expect("pin");
    pin.with_page_mut(|page| {
        page.set_kind(PageKind::Free);
        page.set_lsn(Lsn(u64::MAX - 1));
    })
    .expect("free the page");
    drop(pin);
    let err = load(&storage, Some(root)).expect_err("a chain cut by a free page");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("a free page"), "{err}");
}

#[test]
fn a_catalogue_of_the_same_length_is_rewritten_in_place_without_a_sync() {
    let (_dir, storage) = instance("meta-same-length");
    let root = ensure_root(&storage).expect("allocate the root");
    let first = wide_catalogue(230);
    store(&storage, root, &first, Lsn(0)).expect("store");
    let records = page_records(&storage);
    let durable = storage.wal.durable_lsn();
    assert!(
        durable >= storage.wal.last_lsn().expect("last"),
        "another length is synced"
    );
    // The counter of a table moves: eight bytes in place, one record, no sync, the pages left
    // at the number the caller gives.
    let mut moved = first.clone();
    let entry = moved.table(TableId(17)).cloned().expect("table 17");
    moved.add_table(TableEntry {
        next_row_id: 9_000,
        ..entry
    });
    assert_eq!(moved.encode().len(), first.encode().len());
    store(&storage, root, &moved, durable).expect("store the counter");
    assert_eq!(page_records(&storage), records + 1, "one record");
    assert_eq!(storage.wal.durable_lsn(), durable, "and no sync");
    let pages = chain(&storage, root).expect("walk the chain").pages;
    for id in &pages {
        let lsn = storage
            .pool
            .pin(*id)
            .expect("pin")
            .with_page(Page::lsn)
            .expect("read");
        assert!(
            lsn <= durable,
            "page {id} carries {lsn}, past the durable {durable}"
        );
    }
    storage
        .pool
        .flush_all()
        .expect("the pages may leave the pool");
    assert_eq!(load(&storage, Some(root)).expect("load"), moved);
    // A table more: another length, one more record, synced.
    store(&storage, root, &wide_catalogue(231), Lsn(0)).expect("store a table more");
    assert_eq!(page_records(&storage), records + 2);
    assert!(storage.wal.durable_lsn() >= storage.wal.last_lsn().expect("last"));
}
