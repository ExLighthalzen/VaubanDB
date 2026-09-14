//! What a DDL of table or of index leaves in the internal tables of `master`: the rows
//! `src/sys_rows.rs` writes.
//!
//! These tests know the public API only, as those of `tests/sys_tables.rs` do: the names of
//! the internal tables are `pub(crate)`, so a test that must reach one finds it by its shape
//! (`table_shaped`). What each row carries column by column is checked by the unit tests of
//! `src/views/*.rs`; what this file checks is that the rows are there after a `COMMIT`, gone
//! after a `DROP` and gone after a `ROLLBACK`.

use std::sync::Arc;

use vauban_catalog::{
    Catalog, ColumnDef, ConstraintDef, IndexDef, ObjectId, QualifiedName, SortedColumn, TableDef,
    TableMeta,
};
use vauban_storage::{DbId, MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlType, TypeInfo, Value};

/// A bootstrapped catalogue over a fresh `MemoryStorage`, with its storage and its manager.
fn instance() -> (Catalog, Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (catalog, storage, txn)
}

/// The `DbId` of `master`, where the internal tables live.
fn master(storage: &Arc<dyn Storage>) -> DbId {
    storage
        .databases()
        .expect("databases()")
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("master"))
        .expect("master is there after a bootstrap")
        .0
}

/// The identifier of the one table of `master` whose shape is `columns`, as
/// `tests/sys_tables.rs` reaches an internal table.
fn table_shaped(storage: &Arc<dyn Storage>, columns: &[TypeInfo]) -> TableId {
    let mut found: Vec<TableId> = storage
        .tables(master(storage))
        .expect("tables(master)")
        .into_iter()
        .filter(|(_, shape)| shape.columns == columns)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(found.len(), 1, "one table of master has that shape");
    found.pop().expect("the shape was found")
}

/// The rows of `table`, read through a transaction of its own.
fn rows(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
) -> Vec<Vec<Value>> {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let read = rows_seen_by(storage, txn, &handle, table);
    txn.commit(handle).expect("commit of the reading txn");
    read
}

/// The rows of `table` a transaction that is already open sees.
fn rows_seen_by(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    handle: &vauban_txn::TxnHandle,
    table: TableId,
) -> Vec<Vec<Value>> {
    let snapshot = txn.statement_snapshot(handle);
    storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect()
}

/// The rows of `table` whose column `position` is the `object_id` of `object`.
fn rows_of_object(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
    position: usize,
    object: ObjectId,
) -> Vec<Vec<Value>> {
    rows(storage, txn, table)
        .into_iter()
        .filter(|row| row.get(position) == Some(&Value::I32(object.0)))
        .collect()
}

/// The `int` at `position` of `row`.
fn int_at(row: &[Value], position: usize) -> i32 {
    match row.get(position) {
        Some(&Value::I32(value)) => value,
        other => panic!("column {position} is not an int: {other:?}"),
    }
}

/// The text at `position` of `row`.
fn text_at(row: &[Value], position: usize) -> String {
    match row.get(position) {
        Some(Value::String(text)) => text.text.clone(),
        other => panic!("column {position} is not a string: {other:?}"),
    }
}

/// A column of a definition, with its type and nothing else on it.
fn column(name: &str, ty: SqlType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
        default: None,
        identity: None,
        computed: None,
    }
}

/// A two-column table `(a int NOT NULL, b int NULL)`, in `database`.
fn two_columns(database: &str, name: &str, constraints: Vec<ConstraintDef>) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: database.to_owned(),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
        },
        columns: vec![
            column("a", SqlType::Int, false),
            column("b", SqlType::NVarChar(Len::Fixed(10)), true),
        ],
        constraints,
    }
}

/// A `PRIMARY KEY CLUSTERED (a)` named `pk_<table>`.
fn primary_key(table: &str) -> ConstraintDef {
    ConstraintDef::PrimaryKey {
        name: Some(format!("pk_{table}")),
        columns: vec![SortedColumn {
            column: "a".to_owned(),
            descending: false,
        }],
        clustered: true,
    }
}

/// Creates `def` in a transaction of its own and commits it.
fn create_committed(catalog: &Catalog, txn: &Arc<TransactionManager>, def: &TableDef) -> TableMeta {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog.create_table(&handle, def).expect("create_table");
    txn.commit(handle).expect("commit");
    meta
}

/// A type of an internal table, not nullable.
fn ty(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, false)
}

/// A `sysname`.
fn sysname(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), nullable)
}

/// The shape of the internal table `sys.objects` and `sys.tables` read: 9 columns
/// (`tests/sys_tables.rs`).
fn objects_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        sysname(false),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Char(Len::Fixed(2))),
        ty(SqlType::NVarChar(Len::Fixed(60))),
        ty(SqlType::Bit),
        ty(SqlType::Int),
    ]
}

/// The shape of the internal table `sys.columns` reads: 14 columns (`tests/sys_tables.rs`).
fn sys_columns_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        sysname(false),
        ty(SqlType::Int),
        ty(SqlType::TinyInt),
        ty(SqlType::Int),
        ty(SqlType::SmallInt),
        ty(SqlType::TinyInt),
        ty(SqlType::TinyInt),
        sysname(true),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
    ]
}

/// The shape of the internal table `INFORMATION_SCHEMA.TABLES` reads: 4 columns
/// (`tests/info_schema.rs`).
fn is_tables_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        sysname(true),
        sysname(false),
        TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true),
    ]
}

/// The shape of the internal table `INFORMATION_SCHEMA.COLUMNS` reads: 16 columns
/// (`tests/info_schema.rs`).
fn is_columns_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        sysname(true),
        sysname(false),
        sysname(true),
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(4000)), true),
        TypeInfo::new(SqlType::VarChar(Len::Fixed(3)), true),
        sysname(true),
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::TinyInt, true),
        TypeInfo::new(SqlType::SmallInt, true),
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::SmallInt, true),
        sysname(true),
        sysname(true),
    ]
}

/// The shape of the internal table `sys.indexes` reads: 9 columns (`tests/sys_indexes.rs`).
fn indexes_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        sysname(true),
        ty(SqlType::Int),
        ty(SqlType::TinyInt),
        ty(SqlType::NVarChar(Len::Fixed(60))),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
    ]
}

/// The shape of the internal table `sys.index_columns` reads: 7 columns
/// (`tests/sys_indexes.rs`).
fn index_columns_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::TinyInt),
        ty(SqlType::Bit),
    ]
}

/// The shape of the internal table `sys.key_constraints` reads: 8 columns
/// (`tests/sys_indexes.rs`).
fn key_constraints_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        sysname(false),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Char(Len::Fixed(2))),
        ty(SqlType::NVarChar(Len::Fixed(60))),
        ty(SqlType::Int),
        ty(SqlType::Bit),
    ]
}

/// The shape of the internal table `sys.partitions` reads: 5 columns (`tests/sys_extra.rs`).
fn partitions_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::BigInt),
        ty(SqlType::BigInt),
    ]
}

/// The shape of the internal table `sys.allocation_units` reads: 3 columns
/// (`tests/sys_extra.rs`).
fn allocation_units_shape() -> Vec<TypeInfo> {
    vec![ty(SqlType::Int), ty(SqlType::BigInt), ty(SqlType::BigInt)]
}

/// Position of `database_id` in every internal table of this file: the first column
/// (`src/views/*.rs`, the `*_columns` modules).
const DATABASE_ID: usize = 0;

/// Position of `object_id` in the tables that carry one.
const OBJECT_ID: usize = 1;

#[test]
fn create_table_writes_its_object_and_column_rows() {
    let (catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());
    let columns = table_shaped(&storage, &sys_columns_shape());
    let is_tables = table_shaped(&storage, &is_tables_shape());
    let is_columns = table_shaped(&storage, &is_columns_shape());

    let meta = create_committed(&catalog, &txn, &two_columns("master", "t", Vec::new()));

    let object = rows_of_object(&storage, &txn, objects, OBJECT_ID, meta.id);
    assert_eq!(object.len(), 1);
    // `type char(2)` of a user table, second byte a space (`src/views/sys_tables.rs`).
    assert_eq!(text_at(&object[0], 5), "U ");
    assert_eq!(int_at(&object[0], DATABASE_ID), master(&storage).0 as i32);
    assert_eq!(
        rows_of_object(&storage, &txn, columns, OBJECT_ID, meta.id).len(),
        2
    );
    // The two tables of `INFORMATION_SCHEMA` carry the name of the table, not its identifier.
    let named: Vec<Vec<Value>> = rows(&storage, &txn, is_tables)
        .into_iter()
        .filter(|row| text_at(row, 2) == "t")
        .collect();
    assert_eq!(named.len(), 1);
    assert_eq!(text_at(&named[0], 1), "dbo");
    assert_eq!(
        rows(&storage, &txn, is_columns)
            .into_iter()
            .filter(|row| text_at(row, 2) == "t")
            .count(),
        2
    );
}

#[test]
fn the_rows_carry_the_database_id_of_the_table() {
    let (catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let created = catalog
        .create_database(&handle, "userdb", None)
        .expect("create_database");
    txn.commit(handle).expect("commit of the create database");
    let meta = create_committed(&catalog, &txn, &two_columns("userdb", "t", Vec::new()));

    let object = rows_of_object(&storage, &txn, objects, OBJECT_ID, meta.id);
    assert_eq!(object.len(), 1);
    // The `WHERE database_id = DB_ID()` of the views filters on this column, so the row of a
    // table of `userdb` carries the identifier of `userdb` and not that of `master`.
    assert_eq!(int_at(&object[0], DATABASE_ID), created.0 as i32);
    assert_ne!(created, master(&storage));
}

#[test]
fn a_primary_key_writes_its_index_and_key_constraint_rows() {
    let (catalog, storage, txn) = instance();
    let indexes = table_shaped(&storage, &indexes_shape());
    let index_columns = table_shaped(&storage, &index_columns_shape());
    let key_constraints = table_shaped(&storage, &key_constraints_shape());
    let partitions = table_shaped(&storage, &partitions_shape());

    let def = two_columns("master", "t_pk", vec![primary_key("t_pk")]);
    let meta = create_committed(&catalog, &txn, &def);

    let index = rows_of_object(&storage, &txn, indexes, OBJECT_ID, meta.id);
    assert_eq!(index.len(), 1, "the clustered key, and no heap row");
    // `index_id` at position 3, `type_desc` at position 5 (`src/views/sys_indexes.rs`).
    assert_eq!(int_at(&index[0], 3), 1);
    assert_eq!(text_at(&index[0], 5), "CLUSTERED");
    let keys = rows_of_object(&storage, &txn, index_columns, OBJECT_ID, meta.id);
    assert_eq!(keys.len(), 1, "one key column, `a`");
    assert_eq!(int_at(&keys[0], 2), 1, "the index_id of the key");
    // A key constraint names its table through `parent_object_id`, at position 3.
    let constraint = rows_of_object(&storage, &txn, key_constraints, 3, meta.id);
    assert_eq!(constraint.len(), 1);
    assert_eq!(text_at(&constraint[0], 1), "pk_t_pk");
    assert_eq!(text_at(&constraint[0], 4), "PK");
    let partition = rows_of_object(&storage, &txn, partitions, OBJECT_ID, meta.id);
    assert_eq!(partition.len(), 1);
    assert_eq!(int_at(&partition[0], 2), 1, "the partition of the key");
}

#[test]
fn drop_table_removes_every_row_of_the_table() {
    let (catalog, storage, txn) = instance();
    let watched = watched_tables(&storage);
    let before: Vec<usize> = watched
        .iter()
        .map(|&table| rows(&storage, &txn, table).len())
        .collect();

    let def = two_columns("master", "t_drop", vec![primary_key("t_drop")]);
    let meta = create_committed(&catalog, &txn, &def);
    let written: Vec<usize> = watched
        .iter()
        .zip(&before)
        .map(|(&table, count)| rows(&storage, &txn, table).len() - count)
        .collect();
    // One object row, two column rows, one `INFORMATION_SCHEMA.TABLES` row, two
    // `INFORMATION_SCHEMA.COLUMNS` rows, one index row, one index column row, one key
    // constraint row, one partition row and one allocation unit row.
    assert_eq!(written, vec![1, 2, 1, 2, 1, 1, 1, 1, 1]);

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    txn.commit(handle).expect("commit of the drop");

    let after: Vec<usize> = watched
        .iter()
        .map(|&table| rows(&storage, &txn, table).len())
        .collect();
    assert_eq!(after, before, "the drop left the tables as it found them");
}

#[test]
fn a_rolled_back_create_table_leaves_no_row() {
    let (catalog, storage, txn) = instance();
    let watched = watched_tables(&storage);
    let before: Vec<usize> = watched
        .iter()
        .map(|&table| rows(&storage, &txn, table).len())
        .collect();

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let def = two_columns("master", "t_back", vec![primary_key("t_back")]);
    catalog.create_table(&handle, &def).expect("create_table");
    txn.rollback(handle).expect("rollback");

    let after: Vec<usize> = watched
        .iter()
        .map(|&table| rows(&storage, &txn, table).len())
        .collect();
    assert_eq!(after, before, "the rows are versioned by the transaction");
}

#[test]
fn a_table_created_and_dropped_in_one_transaction_leaves_no_row() {
    let (catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let def = two_columns("master", "t_both", Vec::new());
    let meta = catalog.create_table(&handle, &def).expect("create_table");
    // The statement snapshot of the drop sees the rows the same transaction inserted, which is
    // what lets the two statements meet.
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    txn.commit(handle).expect("commit");

    assert_eq!(
        rows_of_object(&storage, &txn, objects, OBJECT_ID, meta.id).len(),
        0
    );
}

#[test]
fn another_transaction_reads_the_rows_until_the_commit_of_the_drop() {
    let (catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());
    let meta = create_committed(&catalog, &txn, &two_columns("master", "t_mvcc", Vec::new()));

    let reader = txn.begin(IsolationLevel::ReadCommitted);
    let dropper = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_table(&dropper, meta.id).expect("drop_table");
    let seen = rows_seen_by(&storage, &txn, &reader, objects)
        .into_iter()
        .filter(|row| row.get(OBJECT_ID) == Some(&Value::I32(meta.id.0)))
        .count();
    assert_eq!(seen, 1, "the delete is not committed yet");
    txn.commit(dropper).expect("commit of the drop");
    txn.commit(reader).expect("commit of the reader");

    assert_eq!(
        rows_of_object(&storage, &txn, objects, OBJECT_ID, meta.id).len(),
        0
    );
}

#[test]
fn create_index_and_drop_index_maintain_sys_indexes() {
    let (catalog, storage, txn) = instance();
    let indexes = table_shaped(&storage, &indexes_shape());
    let index_columns = table_shaped(&storage, &index_columns_shape());
    let meta = create_committed(&catalog, &txn, &two_columns("master", "t_ix", Vec::new()));

    // A heap: one row, `index_id` 0, and no index column row.
    let heap = rows_of_object(&storage, &txn, indexes, OBJECT_ID, meta.id);
    assert_eq!(heap.len(), 1);
    assert_eq!(int_at(&heap[0], 3), 0);
    assert_eq!(
        rows_of_object(&storage, &txn, index_columns, OBJECT_ID, meta.id).len(),
        0
    );

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let index = catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_t_b".to_owned(),
                columns: vec![SortedColumn {
                    column: "b".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        )
        .expect("create_index");
    txn.commit(handle).expect("commit of the create index");

    let mut written = rows_of_object(&storage, &txn, indexes, OBJECT_ID, meta.id);
    written.sort_by_key(|row| int_at(row, 3));
    assert_eq!(written.len(), 2, "the heap row and the index row");
    assert_eq!(int_at(&written[0], 3), 0);
    assert_eq!(int_at(&written[1], 3), 2);
    assert_eq!(text_at(&written[1], 2), "ix_t_b");
    assert_eq!(
        rows_of_object(&storage, &txn, index_columns, OBJECT_ID, meta.id).len(),
        1
    );

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_index(&handle, index.id).expect("drop_index");
    txn.commit(handle).expect("commit of the drop index");

    let left = rows_of_object(&storage, &txn, indexes, OBJECT_ID, meta.id);
    assert_eq!(left.len(), 1, "the heap row is what is left");
    assert_eq!(int_at(&left[0], 3), 0);
    assert_eq!(
        rows_of_object(&storage, &txn, index_columns, OBJECT_ID, meta.id).len(),
        0
    );
}

/// The nine internal tables a `CREATE TABLE` with a `PRIMARY KEY` writes into, in the order
/// the counts of `drop_table_removes_every_row_of_the_table` list them.
fn watched_tables(storage: &Arc<dyn Storage>) -> Vec<TableId> {
    vec![
        table_shaped(storage, &objects_shape()),
        table_shaped(storage, &sys_columns_shape()),
        table_shaped(storage, &is_tables_shape()),
        table_shaped(storage, &is_columns_shape()),
        table_shaped(storage, &indexes_shape()),
        table_shaped(storage, &index_columns_shape()),
        table_shaped(storage, &key_constraints_shape()),
        table_shaped(storage, &partitions_shape()),
        table_shaped(storage, &allocation_units_shape()),
    ]
}
