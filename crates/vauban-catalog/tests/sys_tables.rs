//! What the bootstrap leaves in `storage` for `views/sys_tables.rs`: the two internal
//! tables the `sys.objects`, `sys.tables` and `sys.columns` views read.
//!
//! These tests know the public API only, as those of `tests/sys_core.rs` do: the names of
//! the internal tables are `pub(crate)`, so a test that must reach one finds it by its
//! shape. The column names, the text of the three definitions and the rows a
//! `TableMeta` gives are checked by the unit tests of `src/views/sys_tables.rs`, which can
//! read what this file cannot.

use std::sync::Arc;

use vauban_catalog::{Catalog, ColumnDef, QualifiedName, TableDef};
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

/// The identifier of the one table of `master` whose shape is `shape`.
///
/// How a test outside the crate reaches an internal table without knowing its name: by the
/// list of column types the view above it publishes. The assertion fails if two tables of
/// `master` share that shape, rather than reading one of them.
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
    let snapshot = txn.statement_snapshot(&handle);
    let read = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    txn.commit(handle).expect("commit of the reading txn");
    read
}

/// The number of rows of `table`, read through a transaction of its own.
fn row_count(storage: &Arc<dyn Storage>, txn: &Arc<TransactionManager>, table: TableId) -> usize {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = txn.statement_snapshot(&handle);
    let rows = storage.scan(&snapshot, table).expect("scan").count();
    txn.commit(handle).expect("commit of the reading txn");
    rows
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

/// The shape of the internal table `sys.objects` and `sys.tables` read.
fn objects_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Char(Len::Fixed(2)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::Int, false),
    ]
}

/// The shape of the internal table `sys.columns` reads: 14 columns.
fn columns_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::TinyInt, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::SmallInt, false),
        TypeInfo::new(SqlType::TinyInt, false),
        TypeInfo::new(SqlType::TinyInt, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::Bit, false),
    ]
}

#[test]
fn the_bootstrap_creates_the_two_internal_tables_of_this_file() {
    let (_catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());
    assert!(row_count(&storage, &txn, objects) > 0);
    let columns = table_shaped(&storage, &columns_shape());
    assert_eq!(row_count(&storage, &txn, columns), 0);
}

#[test]
fn a_created_table_writes_its_object_row_and_its_two_column_rows() {
    // `src/sys_rows.rs` calls `object_rows` / `column_rows` at the end of `create_table`.
    let (catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());
    let columns = table_shaped(&storage, &columns_shape());

    let before_objects = row_count(&storage, &txn, objects);
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: QualifiedName {
                    database: "master".to_owned(),
                    schema: "dbo".to_owned(),
                    name: "t".to_owned(),
                },
                columns: vec![
                    column("a", SqlType::Int, false),
                    column("b", SqlType::NVarChar(Len::Fixed(20)), true),
                ],
                constraints: Vec::new(),
            },
        )
        .expect("create_table");
    txn.commit(handle).expect("commit");

    assert_eq!(meta.columns.len(), 2);
    assert_eq!(row_count(&storage, &txn, objects), before_objects + 1);
    assert_eq!(row_count(&storage, &txn, columns), 2);
}

#[test]
fn schema_id_is_the_resolved_schema() {
    let (catalog, storage, txn) = instance();
    let objects = table_shaped(&storage, &objects_shape());
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_table(
            &handle,
            &TableDef {
                name: QualifiedName {
                    database: "master".to_owned(),
                    schema: "sys".to_owned(),
                    name: "schema_id_probe".to_owned(),
                },
                columns: vec![column("a", SqlType::Int, false)],
                constraints: Vec::new(),
            },
        )
        .expect("create_table");
    txn.commit(handle).expect("commit");
    let read = rows(&storage, &txn, objects);
    let row = read
        .into_iter()
        .find(|row| {
            matches!(
                row.get(2),
                Some(Value::String(text)) if text.text == "schema_id_probe"
            )
        })
        .expect("the object row");
    assert_eq!(row[3], Value::I32(4));
}
