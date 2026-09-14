//! What the bootstrap leaves in `storage` for `views/sys_indexes.rs`: the four internal
//! tables the `sys.indexes`, `sys.index_columns`, `sys.key_constraints` and
//! `sys.identity_columns` views read.
//!
//! These tests know the public API only, as those of `tests/sys_tables.rs` do: the names of
//! the internal tables are `pub(crate)`, so a test that must reach one finds it by its shape.
//! The column names, the text of the four definitions and the rows a `TableMeta` gives are
//! checked by the unit tests of `src/views/sys_indexes.rs`, which can read what this file
//! cannot.

use std::sync::Arc;

use vauban_catalog::{Catalog, ColumnDef, ConstraintDef, IndexDef, QualifiedName, SortedColumn};
use vauban_catalog::{IdentitySpec, TableDef};
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

/// The identifier of the one table of `master` whose shape is `columns`.
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

/// The number of rows of `table`, read through a transaction of its own.
fn row_count(storage: &Arc<dyn Storage>, txn: &Arc<TransactionManager>, table: TableId) -> usize {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = txn.statement_snapshot(&handle);
    let rows = storage.scan(&snapshot, table).expect("scan").count();
    txn.commit(handle).expect("commit of the reading txn");
    rows
}

/// A not-null column of a definition, with its type and its `IDENTITY` property.
fn column(name: &str, ty: SqlType, identity: Option<IdentitySpec>) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, false),
        default: None,
        identity,
        computed: None,
    }
}

/// A type of an internal table, never nullable except where the caller says so.
fn ty(ty: SqlType) -> TypeInfo {
    TypeInfo::new(ty, false)
}

/// The shape of the internal table `sys.indexes` reads: 9 columns, the name being nullable
/// because a heap row carries none.
fn indexes_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true),
        ty(SqlType::Int),
        ty(SqlType::TinyInt),
        ty(SqlType::NVarChar(Len::Fixed(60))),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
        ty(SqlType::Bit),
    ]
}

/// The shape of the internal table `sys.index_columns` reads: 7 columns.
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

/// The shape of the internal table `sys.key_constraints` reads: 8 columns.
fn key_constraints_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::NVarChar(Len::Fixed(128))),
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::Char(Len::Fixed(2))),
        ty(SqlType::NVarChar(Len::Fixed(60))),
        ty(SqlType::Int),
        ty(SqlType::Bit),
    ]
}

/// The shape of the internal table `sys.identity_columns` reads: 12 columns, the seed and the
/// increment being `bigint` where SQL Server publishes `sql_variant`
/// (`src/views/sys_indexes.rs`, bound 2).
fn identity_columns_shape() -> Vec<TypeInfo> {
    vec![
        ty(SqlType::Int),
        ty(SqlType::Int),
        ty(SqlType::NVarChar(Len::Fixed(128))),
        ty(SqlType::Int),
        ty(SqlType::TinyInt),
        ty(SqlType::Int),
        ty(SqlType::SmallInt),
        ty(SqlType::TinyInt),
        ty(SqlType::TinyInt),
        ty(SqlType::Bit),
        ty(SqlType::BigInt),
        ty(SqlType::BigInt),
    ]
}

/// The four shapes, in the order `internal_tables()` describes them.
fn shapes() -> Vec<Vec<TypeInfo>> {
    vec![
        indexes_shape(),
        index_columns_shape(),
        key_constraints_shape(),
        identity_columns_shape(),
    ]
}

#[test]
fn the_bootstrap_creates_the_four_internal_tables_of_this_file() {
    let (_catalog, storage, txn) = instance();
    for shape in shapes() {
        let table = table_shaped(&storage, &shape);
        // No row at bootstrap: the rows of the four tables are those of the objects a client
        // creates, and a fresh instance holds none (unit test
        // `the_internal_tables_of_the_catalogue_have_no_row_in_these_views`).
        assert_eq!(row_count(&storage, &txn, table), 0);
    }
}

#[test]
fn the_four_internal_tables_are_four_distinct_tables_of_master() {
    let (_catalog, storage, _txn) = instance();
    let mut found: Vec<TableId> = shapes()
        .iter()
        .map(|shape| table_shaped(&storage, shape))
        .collect();
    assert_eq!(found.len(), 4);
    found.sort();
    found.dedup();
    assert_eq!(found.len(), 4, "two shapes read the same table");
}

#[test]
fn a_created_table_with_a_key_and_an_identity_writes_two_two_one_and_one_rows() {
    // 2, 2, 1 and 1 rows: the clustered `PRIMARY KEY` at `index_id` 1 and the index on `b` at 2, no
    // heap row, one key column each, one key constraint and one identity column. The unit test
    // `a_clustered_key_an_index_and_an_identity_give_two_two_one_and_one_rows` of
    // `src/views/sys_indexes.rs` asserts the same four counts on the row builders; this one
    // reads them through `storage`, after `create_table` and `create_index`.
    let (catalog, storage, txn) = instance();
    let tables: Vec<TableId> = shapes()
        .iter()
        .map(|shape| table_shaped(&storage, shape))
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
                    column("a", SqlType::Int, None),
                    column("b", SqlType::Int, Some(IdentitySpec::default())),
                ],
                constraints: vec![ConstraintDef::PrimaryKey {
                    name: Some("pk_t".to_owned()),
                    columns: vec![SortedColumn {
                        column: "a".to_owned(),
                        descending: false,
                    }],
                    clustered: true,
                }],
            },
        )
        .expect("create_table");
    catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_t_b".to_owned(),
                columns: vec![SortedColumn {
                    column: "b".to_owned(),
                    descending: true,
                }],
                unique: false,
                clustered: false,
            },
        )
        .expect("create_index");
    txn.commit(handle).expect("commit");

    assert_eq!(meta.columns.len(), 2);
    assert_eq!(meta.constraints.len(), 1);
    assert!(meta.clustered.is_some(), "the PRIMARY KEY is clustered");
    let counts: Vec<usize> = tables
        .into_iter()
        .map(|table| row_count(&storage, &txn, table))
        .collect();
    assert_eq!(counts, vec![2, 2, 1, 1]);
}
