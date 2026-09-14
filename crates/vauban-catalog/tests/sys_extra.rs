//! What the bootstrap leaves in `storage` for `views/sys_extra.rs`: the five internal
//! tables the `sys.all_objects`, `sys.all_columns`, `sys.views`, `sys.partitions`,
//! `sys.allocation_units`, `sys.database_files` and `sys.master_files` views read.
//!
//! These tests know the public API only, as those of `tests/sys_tables.rs` do: the names of the
//! internal tables are `pub(crate)`, so a test that must reach one finds it by its shape. The
//! column names, the text of the seven definitions and the rows a `*Meta` gives are checked by
//! the unit tests of `src/views/sys_extra.rs`, which can read what this file cannot.

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

/// The identifier of the one table of `master` whose shape is `columns`.
///
/// How a test outside the crate reaches an internal table without knowing its name, as
/// `tests/sys_tables.rs` does: by the list of column types of the table. The assertion fails if
/// two tables of `master` share that shape, rather than reading one of them.
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
    let read: Vec<Vec<Value>> = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    txn.commit(handle).expect("commit of the reading txn");
    read
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

/// The shape of the copy of the objects table `sys.all_objects` and `sys.views` read: the 9
/// columns of the table of `views/sys_tables.rs` with a nullable `database_id`.
fn all_objects_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Char(Len::Fixed(2)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::Int, false),
    ]
}

/// The shape of the copy of the columns table `sys.all_columns` reads: 14 columns.
fn all_columns_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, true),
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

/// The shape of the table `sys.partitions` reads: 5 columns.
fn partitions_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
    ]
}

/// The shape of the table `sys.allocation_units` reads: 3 columns.
fn allocation_units_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::BigInt, false),
        TypeInfo::new(SqlType::BigInt, false),
    ]
}

/// The shape of the table `sys.database_files` and `sys.master_files` read: 10 columns.
fn files_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::TinyInt, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(260)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
    ]
}

/// The texts a row carries.
fn texts(row: &[Value]) -> Vec<String> {
    row.iter()
        .filter_map(|value| match value {
            Value::String(stored) => Some(stored.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_bootstrap_creates_the_five_internal_tables_of_this_file() {
    let (_catalog, storage, txn) = instance();
    // The copy of the objects table carries the system views and the table of the files the two
    // files of each of the four system databases; the three others hold the rows of a user
    // object, written at each DDL (unit test
    // `the_objects_and_the_files_tables_are_the_two_the_bootstrap_fills`).
    let objects = rows(&storage, &txn, table_shaped(&storage, &all_objects_shape()));
    assert!(!objects.is_empty());
    assert_eq!(
        rows(&storage, &txn, table_shaped(&storage, &files_shape())).len(),
        8
    );
    for shape in [
        all_columns_shape(),
        partitions_shape(),
        allocation_units_shape(),
    ] {
        let table = table_shaped(&storage, &shape);
        assert_eq!(rows(&storage, &txn, table).len(), 0);
    }
}

#[test]
fn master_has_database_files_rows() {
    let (_catalog, storage, txn) = instance();
    // Read from the table the bootstrap wrote, not from the function that built the rows:
    // `sys.database_files` filters this table on `database_id = DB_ID()`, and `master` is
    // database 1.
    let files = rows(&storage, &txn, table_shaped(&storage, &files_shape()));
    let of_master: Vec<&Vec<Value>> = files.iter().filter(|row| row[0] == Value::I32(1)).collect();
    assert_eq!(of_master.len(), 2, "a data file and a log file: {files:?}");
    let data = of_master[0];
    assert_eq!(data[1], Value::I32(1), "file_id of the data file");
    assert_eq!(data[2], Value::I8(0), "type of the data file");
    assert!(texts(data).contains(&"ROWS".to_owned()), "{data:?}");
    assert!(texts(data).contains(&"master_data".to_owned()), "{data:?}");
    let log = of_master[1];
    assert_eq!(log[1], Value::I32(2), "file_id of the log file");
    assert!(texts(log).contains(&"LOG".to_owned()), "{log:?}");
    assert!(texts(log).contains(&"master_log".to_owned()), "{log:?}");
}

#[test]
fn the_file_rows_name_the_databases_of_the_databases_table() {
    let (_catalog, storage, txn) = instance();
    // The `database_id` of a file row is the identifier the bootstrap handed out: the bound the
    // module documentation of `views/sys_extra.rs` states, checked against the table
    // `sys.databases` reads — the one that holds the row of `msdb`.
    let mut databases: Vec<(Value, String)> = Vec::new();
    for (table, _) in storage.tables(master(&storage)).expect("tables(master)") {
        for row in rows(&storage, &txn, table) {
            if texts(&row).contains(&"msdb".to_owned()) {
                databases.push((row[0].clone(), texts(&row)[0].clone()));
            }
        }
    }
    assert_eq!(databases.len(), 1, "one row names msdb: {databases:?}");
    let files = rows(&storage, &txn, table_shaped(&storage, &files_shape()));
    let of_msdb: Vec<&Vec<Value>> = files
        .iter()
        .filter(|row| row[0] == databases[0].0)
        .collect();
    assert_eq!(of_msdb.len(), 2, "msdb has its two files: {files:?}");
    for row in of_msdb {
        assert!(
            texts(row)
                .iter()
                .any(|text| text.starts_with(&databases[0].1)),
            "a file of msdb is named after it: {row:?}"
        );
    }
}

#[test]
fn the_objects_copy_carries_the_system_views_of_the_catalogue() {
    let (_catalog, storage, txn) = instance();
    let objects = rows(&storage, &txn, table_shaped(&storage, &all_objects_shape()));
    let names: Vec<String> = objects.iter().flat_map(|row| texts(row)).collect();
    // `sys.tables` of `views/sys_tables.rs`, `sys.databases` of `views/sys_core.rs` and the
    // seven views of `views/sys_extra.rs`; each row is a view, so `sys.views` shows these and
    // no user object.
    for view in [
        "tables",
        "databases",
        "all_objects",
        "views",
        "all_columns",
        "partitions",
        "allocation_units",
        "database_files",
        "master_files",
    ] {
        assert!(names.contains(&view.to_owned()), "{names:?}");
    }
    for row in &objects {
        assert!(texts(row).contains(&"VIEW".to_owned()), "{row:?}");
    }
}

#[test]
fn a_created_table_writes_its_rows_in_four_of_the_five_tables() {
    // `create_table` writes one object row, two column rows, one partition row and one
    // allocation unit row, and no row
    // in the table of the files, which `create_database` owns.
    let (catalog, storage, txn) = instance();
    let counted: Vec<(TableId, usize)> = [
        all_objects_shape(),
        all_columns_shape(),
        partitions_shape(),
        allocation_units_shape(),
        files_shape(),
    ]
    .into_iter()
    .map(|shape| {
        let table = table_shaped(&storage, &shape);
        (table, rows(&storage, &txn, table).len())
    })
    .collect();

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
    let written: Vec<usize> = counted
        .into_iter()
        .map(|(table, before)| rows(&storage, &txn, table).len() - before)
        .collect();
    assert_eq!(written, vec![1, 2, 1, 1, 0]);
}
