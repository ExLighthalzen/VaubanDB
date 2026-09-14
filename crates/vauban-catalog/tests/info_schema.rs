//! What the bootstrap leaves in `storage` for `views/info_schema.rs`: the two internal
//! tables the `INFORMATION_SCHEMA.TABLES` and `INFORMATION_SCHEMA.COLUMNS` views read.
//!
//! These tests know the public API only, as those of `tests/sys_tables.rs` do: the names of the
//! internal tables are `pub(crate)`, so a test that must reach one finds it by its shape. The
//! column names, the text of the three definitions and the rows a `TableMeta` gives are checked
//! by the unit tests of `src/views/info_schema.rs`, which can read what this file cannot.
//!
//! `INFORMATION_SCHEMA.SCHEMATA` has no table of its own — it reads the table of the schemas the
//! bootstrap fills, which `tests/bootstrap.rs` covers.

use std::sync::Arc;

use vauban_catalog::{Catalog, ColumnDef, QualifiedName, TableDef};
use vauban_storage::{DbId, MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlType, TypeInfo};

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
/// How a test outside the crate reaches an internal table without knowing its name: by the list
/// of column types the view above it publishes. The assertion fails if two tables of `master`
/// share that shape, rather than reading one of them.
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

/// A `sysname`, the type of the names these two tables hold.
fn sysname(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), nullable)
}

/// The shape of the internal table `INFORMATION_SCHEMA.TABLES` reads: 4 columns.
fn tables_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        sysname(true),
        sysname(false),
        TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true),
    ]
}

/// The shape of the internal table `INFORMATION_SCHEMA.COLUMNS` reads: 16 columns.
fn columns_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
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

#[test]
fn the_bootstrap_creates_the_two_internal_tables_of_this_file() {
    let (_catalog, storage, txn) = instance();
    for shape in [tables_shape(), columns_shape()] {
        let table = table_shaped(&storage, &shape);
        // No row at bootstrap: the rows of the two tables are those of the tables a client
        // creates, and a fresh instance holds none (unit test
        // `the_two_internal_tables_hold_no_row_at_bootstrap`). SQL Server answers 0 to
        // `SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES` on a fresh database as well.
        assert_eq!(row_count(&storage, &txn, table), 0);
    }
}

#[test]
fn a_created_table_writes_its_table_row_and_its_two_column_rows() {
    // `src/sys_rows.rs` calls `table_rows` / `column_rows` at the end of `create_table`.
    let (catalog, storage, txn) = instance();
    let tables = table_shaped(&storage, &tables_shape());
    let columns = table_shaped(&storage, &columns_shape());

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
    assert_eq!(row_count(&storage, &txn, tables), 1);
    assert_eq!(row_count(&storage, &txn, columns), 2);
}
