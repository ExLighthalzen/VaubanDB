//! `Catalog::create_index`, `Catalog::drop_index` and the `PRIMARY KEY` / `UNIQUE`
//! constraints a `CREATE TABLE` carries, seen from outside the crate.
//!
//! These tests know the public API only: they build a [`TableDef`] and an [`IndexDef`] by
//! hand — turning a statement into one is the business of the binder — and read the result
//! through [`TableMeta`](vauban_catalog::TableMeta), through the [`IndexMeta`]
//! `create_index` answers and through `Storage`. The name a
//! constraint takes is read in the unit tests of `src/index.rs`, the store being crate
//! private.
//!
//! The tables are created in `master`, which the bootstrap made.

use std::sync::Arc;

use vauban_catalog::{
    Catalog, ColumnDef, ConstraintDef, ConstraintMeta, IndexDef, IndexMeta, ObjectId,
    QualifiedName, SortedColumn, TableDef, TableMeta,
};
use vauban_storage::{
    DbId, IndexId, IndexShape, KeyColumn, MemoryStorage, Row, Storage, TableId, TableShape,
};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo, Value};

/// A bootstrapped catalogue over a fresh `MemoryStorage`, with its storage and its manager.
fn instance() -> (Catalog, Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (catalog, storage, txn)
}

/// The `DbId` of `master`, the database the bootstrap created first.
fn master(storage: &Arc<dyn Storage>) -> DbId {
    storage
        .databases()
        .expect("databases()")
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("master"))
        .expect("master is there after a bootstrap")
        .0
}

/// An open transaction at `READ COMMITTED`.
fn begin(txn: &Arc<TransactionManager>) -> TxnHandle {
    txn.begin(IsolationLevel::ReadCommitted)
}

/// The shape `storage` holds for `table`.
fn shape_of(storage: &Arc<dyn Storage>, db: DbId, table: TableId) -> TableShape {
    storage
        .tables(db)
        .expect("tables()")
        .into_iter()
        .find(|(id, _)| *id == table)
        .expect("the table is in the database")
        .1
}

/// The indexes `storage` holds for `table`, by increasing identifier.
fn indexes_of(storage: &Arc<dyn Storage>, table: TableId) -> Vec<(IndexId, IndexShape)> {
    storage.indexes(table).expect("indexes()")
}

/// `master.dbo.<name> (a int NOT NULL, b int NOT NULL)` with the constraints given.
fn table(name: &str, constraints: Vec<ConstraintDef>) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
        },
        columns: ["a", "b"]
            .into_iter()
            .map(|column| ColumnDef {
                name: column.to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                default: None,
                identity: None,
                computed: None,
            })
            .collect(),
        constraints,
    }
}

/// The key columns of a constraint or of an index, written as names and directions.
fn sorted(columns: &[(&str, bool)]) -> Vec<SortedColumn> {
    columns
        .iter()
        .map(|(name, descending)| SortedColumn {
            column: (*name).to_owned(),
            descending: *descending,
        })
        .collect()
}

/// `PRIMARY KEY` over the columns given, clustered or not.
fn primary_key(clustered: bool, columns: &[(&str, bool)]) -> ConstraintDef {
    ConstraintDef::PrimaryKey {
        name: None,
        columns: sorted(columns),
        clustered,
    }
}

/// `UNIQUE` over the columns given, clustered or not.
fn unique(clustered: bool, columns: &[(&str, bool)]) -> ConstraintDef {
    ConstraintDef::Unique {
        name: None,
        columns: sorted(columns),
        clustered,
    }
}

/// A non-unique, non-clustered index called `name` over the columns given.
fn index(table: ObjectId, name: &str, columns: &[(&str, bool)]) -> IndexDef {
    IndexDef {
        table,
        name: name.to_owned(),
        columns: sorted(columns),
        unique: false,
        clustered: false,
    }
}

/// A table with a clustered `PRIMARY KEY` on `a`, created and committed.
fn table_with_a_key(
    catalog: &Catalog,
    txn: &Arc<TransactionManager>,
    name: &str,
) -> (TableMeta, IndexId) {
    let handle = begin(txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(name, vec![primary_key(true, &[("a", false)])]),
        )
        .expect("create_table");
    txn.commit(handle).expect("commit");
    let clustered = meta.clustered.expect("the primary key is clustered");
    (meta, clustered)
}

/// The same table plus the ordinary index `ix_b` on `b`, which is the one a `DROP INDEX` is
/// allowed to take: the index of a constraint answers 3723
/// (`dropping_the_index_of_a_primary_key_names_3723`).
fn table_with_an_ordinary_index(
    catalog: &Catalog,
    txn: &Arc<TransactionManager>,
    name: &str,
) -> (TableMeta, IndexId) {
    let (meta, _) = table_with_a_key(catalog, txn, name);
    let handle = begin(txn);
    let created = catalog
        .create_index(&handle, &index(meta.id, "ix_b", &[("b", false)]))
        .expect("create_index");
    txn.commit(handle).expect("commit");
    (meta, created.id)
}

/// A `PRIMARY KEY` written without `CLUSTERED` gives the shape its key and an index equal to
/// it, `unique`, and the metadata points at that index.
#[test]
fn primary_key_clustered_sets_shape_and_index() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    // `clustered: true` is what the binder fills for this definition (the binder decides the
    // default, which another `UNIQUE CLUSTERED` constraint of the statement changes);
    // `CREATE TABLE dbo.t (a int PRIMARY KEY, b int);` gives a CLUSTERED index.
    let meta = catalog
        .create_table(
            &handle,
            &table("t", vec![primary_key(true, &[("a", false)])]),
        )
        .expect("create_table");
    txn.commit(handle).expect("commit");

    assert_eq!(
        shape_of(&storage, db, meta.storage_id).clustered_key,
        Some(vec![KeyColumn {
            column: 0,
            descending: false
        }])
    );
    let indexes = indexes_of(&storage, meta.storage_id);
    assert_eq!(indexes.len(), 1, "one index backs the primary key");
    assert_eq!(
        indexes[0].1,
        IndexShape {
            columns: vec![KeyColumn {
                column: 0,
                descending: false
            }],
            unique: true,
            included: Vec::new(),
        }
    );
    assert_eq!(meta.clustered, Some(indexes[0].0));
    assert_eq!(
        meta.constraints,
        vec![ConstraintMeta::PrimaryKey(indexes[0].0)]
    );
}

/// `UNIQUE (b)` leaves the table a heap and adds a unique index on `b`.
#[test]
fn unique_nonclustered_is_an_index() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![unique(false, &[("b", false)])]))
        .expect("create_table");
    txn.commit(handle).expect("commit");

    assert_eq!(shape_of(&storage, db, meta.storage_id).clustered_key, None);
    let indexes = indexes_of(&storage, meta.storage_id);
    assert_eq!(indexes.len(), 1);
    assert_eq!(
        indexes[0].1,
        IndexShape {
            columns: vec![KeyColumn {
                column: 1,
                descending: false
            }],
            unique: true,
            included: Vec::new(),
        }
    );
    assert_eq!(meta.clustered, None, "the table is a heap");
    assert_eq!(meta.constraints, vec![ConstraintMeta::Unique(indexes[0].0)]);
}

/// A `PRIMARY KEY NONCLUSTERED` leaves the shape without a key, as SQL Server leaves the
/// table a `HEAP` (`CREATE TABLE dbo.t (… PRIMARY KEY NONCLUSTERED …)`).
#[test]
fn a_nonclustered_primary_key_leaves_a_heap() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table("t", vec![primary_key(false, &[("a", false)])]),
        )
        .expect("create_table");
    txn.commit(handle).expect("commit");

    assert_eq!(shape_of(&storage, db, meta.storage_id).clustered_key, None);
    assert_eq!(meta.clustered, None);
    let indexes = indexes_of(&storage, meta.storage_id);
    assert_eq!(indexes.len(), 1);
    assert!(indexes[0].1.unique);
    assert_eq!(
        meta.constraints,
        vec![ConstraintMeta::PrimaryKey(indexes[0].0)]
    );
}

/// Two constraints on one table give two indexes, and the constraints come back in the order
/// they were declared.
#[test]
fn two_keys_on_one_table_are_two_indexes() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    primary_key(true, &[("b", true), ("a", false)]),
                    unique(false, &[("a", false)]),
                ],
            ),
        )
        .expect("create_table");
    txn.commit(handle).expect("commit");

    let indexes = indexes_of(&storage, meta.storage_id);
    assert_eq!(indexes.len(), 2);
    assert_eq!(
        indexes[0].1.columns,
        vec![
            KeyColumn {
                column: 1,
                descending: true
            },
            KeyColumn {
                column: 0,
                descending: false
            },
        ],
        "the key keeps the declared order and the DESC of its first column"
    );
    assert_eq!(
        meta.constraints,
        vec![
            ConstraintMeta::PrimaryKey(indexes[0].0),
            ConstraintMeta::Unique(indexes[1].0),
        ]
    );
}

/// `create_index` creates the index at once and answers its metadata.
#[test]
fn create_index_makes_an_index_of_its_definition() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let handle = begin(&txn);
    let created = catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_b".to_owned(),
                columns: sorted(&[("b", true)]),
                unique: true,
                clustered: false,
            },
        )
        .expect("create_index");
    txn.commit(handle).expect("commit");

    assert_eq!(
        created,
        IndexMeta {
            id: created.id,
            name: "ix_b".to_owned(),
            unique: true,
            clustered: false,
            primary_key: false,
            columns: vec![KeyColumn {
                column: 1,
                descending: true
            }],
        }
    );
    let indexes = indexes_of(&storage, meta.storage_id);
    assert_eq!(indexes.len(), 2, "the primary key and ix_b");
    let shape = indexes
        .into_iter()
        .find(|(id, _)| *id == created.id)
        .expect("ix_b is in storage")
        .1;
    assert!(shape.unique);
    assert!(shape.included.is_empty(), "INCLUDE is not served");
}

/// A `DROP INDEX` is deferred to the `COMMIT`, as a `DROP TABLE` is.
#[test]
fn drop_index_deferred() {
    let (catalog, storage, txn) = instance();
    let (meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");

    let handle = begin(&txn);
    catalog.drop_index(&handle, ordinary).expect("drop_index");
    assert!(
        indexes_of(&storage, meta.storage_id)
            .iter()
            .any(|(id, _)| *id == ordinary),
        "the index is readable until the commit"
    );
    txn.commit(handle).expect("commit");
    assert!(
        !indexes_of(&storage, meta.storage_id)
            .iter()
            .any(|(id, _)| *id == ordinary)
    );
}

/// A `CREATE INDEX` undone by a `ROLLBACK` leaves the storage as it was.
#[test]
fn create_index_rollback_drops_it() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let before = indexes_of(&storage, meta.storage_id).len();

    let handle = begin(&txn);
    let created = catalog
        .create_index(&handle, &index(meta.id, "ix_b", &[("b", false)]))
        .expect("create_index");
    assert_eq!(indexes_of(&storage, meta.storage_id).len(), before + 1);
    txn.rollback(handle).expect("rollback");

    let after = indexes_of(&storage, meta.storage_id);
    assert_eq!(after.len(), before);
    assert!(!after.iter().any(|(id, _)| *id == created.id));
}

/// A `DROP INDEX` undone by a `ROLLBACK` leaves the index, and a later `DROP` takes it.
#[test]
fn a_rolled_back_drop_index_leaves_the_index() {
    let (catalog, storage, txn) = instance();
    let (meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");

    let handle = begin(&txn);
    catalog.drop_index(&handle, ordinary).expect("drop_index");
    txn.rollback(handle).expect("rollback");
    assert!(
        indexes_of(&storage, meta.storage_id)
            .iter()
            .any(|(id, _)| *id == ordinary)
    );

    let second = begin(&txn);
    catalog
        .drop_index(&second, ordinary)
        .expect("the mark of the rolled-back drop was cleared");
    txn.commit(second).expect("commit");
    assert!(
        !indexes_of(&storage, meta.storage_id)
            .iter()
            .any(|(id, _)| *id == ordinary)
    );
}

/// A `DROP INDEX` over an identifier the catalogue does not hold answers 3701, the number
/// and the state SQL Server sends for an index over a table that exists.
#[test]
fn dropping_an_unknown_index_is_3701() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let err = catalog
        .drop_index(&handle, IndexId(4_242))
        .expect_err("no such index");
    assert_eq!(err.number, 3701);
    assert_eq!(err.state, 7);
    assert!(err.message.contains("4242"), "{}", err.message);
}

/// Dropping the same index twice in one transaction answers 3701, the second time with the
/// three-part name.
#[test]
fn dropping_the_same_index_twice_is_3701() {
    let (catalog, _storage, txn) = instance();
    let (_meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");
    let handle = begin(&txn);
    catalog.drop_index(&handle, ordinary).expect("drop_index");
    let err = catalog
        .drop_index(&handle, ordinary)
        .expect_err("the index is already claimed");
    assert_eq!(err.number, 3701);
    assert_eq!(err.state, 7, "the table of the index is there");
    assert!(err.message.contains("'dbo.t.ix_b'"), "{}", err.message);
}

/// A committed `DROP TABLE` takes the indexes of the table with it, and the catalogue
/// forgets them.
#[test]
fn dropping_the_table_forgets_its_indexes() {
    let (catalog, _storage, txn) = instance();
    let (meta, clustered) = table_with_a_key(&catalog, &txn, "t");
    let handle = begin(&txn);
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    txn.commit(handle).expect("commit");

    let second = begin(&txn);
    let err = catalog
        .drop_index(&second, clustered)
        .expect_err("the index went with its table");
    assert_eq!(err.number, 3701);
}

/// Two indexes of one name on one table: SQL Server answers 1913, which `vauban-errors` does
/// not carry, so the catalogue answers an internal bug that names the number and creates
/// nothing.
#[test]
fn a_duplicate_index_name_on_a_table_is_refused() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let handle = begin(&txn);
    catalog
        .create_index(&handle, &index(meta.id, "ix_b", &[("b", false)]))
        .expect("create_index");
    let before = indexes_of(&storage, meta.storage_id).len();
    let err = catalog
        .create_index(&handle, &index(meta.id, "IX_B", &[("a", false)]))
        .expect_err("the name is taken");
    assert!(err.message.contains("1913"), "{}", err.message);
    assert_eq!(indexes_of(&storage, meta.storage_id).len(), before);
}

/// `CREATE CLUSTERED INDEX` on a table that exists is not served: the clustered key is a
/// field of the shape and `storage` has no `ALTER`.
#[test]
fn a_clustered_create_index_is_not_served() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let before = indexes_of(&storage, meta.storage_id).len();
    let handle = begin(&txn);
    let err = catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_clustered".to_owned(),
                columns: sorted(&[("b", false)]),
                unique: false,
                clustered: true,
            },
        )
        .expect_err("a clustered index after the fact");
    assert_eq!(
        err.message,
        "Internal error: internal bug: Catalog::create_index CLUSTERED not implemented"
    );
    assert_eq!(indexes_of(&storage, meta.storage_id).len(), before);
}

/// A definition with two `PRIMARY KEY` constraints is refused before the table exists:
/// SQL Server answers 8110, which `vauban-errors` does not carry.
#[test]
fn two_primary_keys_leave_no_table_behind() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let before = storage.tables(db).expect("tables()").len();
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    primary_key(true, &[("a", false)]),
                    primary_key(false, &[("b", false)]),
                ],
            ),
        )
        .expect_err("two primary keys");
    assert!(err.message.contains("8110"), "{}", err.message);
    assert_eq!(storage.tables(db).expect("tables()").len(), before);
}

/// A `FOREIGN KEY` is stored by `constraints.rs` (`tests/constraints.rs`), which reads the
/// table it points at: one that names no table of the catalogue answers the number
/// SQL Server sends, 1767, which `vauban-errors` does not carry.
///
/// The table is created in `storage` before the constraint is read — the
/// key of a table pointing at itself asks for its index, which exists once the table does
/// (`src/constraints.rs`) — so what this leaves behind is the bound
/// `a_failed_key_leaves_a_table_the_catalogue_does_not_name` already states for a key:
/// a table the catalogue does not name, which the `ROLLBACK` drops.
#[test]
fn a_foreign_key_to_an_unknown_table_names_1767() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let before = storage.tables(db).expect("tables()").len();
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![ConstraintDef::ForeignKey {
                    name: None,
                    columns: vec!["a".to_owned()],
                    referenced: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "other".to_owned(),
                    },
                    referenced_columns: vec!["a".to_owned()],
                    on_delete: vauban_parser::RefAction::NoAction,
                    on_update: vauban_parser::RefAction::NoAction,
                }],
            ),
        )
        .expect_err("a foreign key that points at nothing");
    assert!(err.message.contains("1767"), "{}", err.message);
    assert!(err.message.contains("dbo.other"), "{}", err.message);
    assert_eq!(storage.tables(db).expect("tables()").len(), before + 1);
    txn.rollback(handle).expect("rollback");
    assert_eq!(storage.tables(db).expect("tables()").len(), before);
}

/// A `CREATE INDEX` whose compensation cannot be registered drops the index again before
/// answering.
#[test]
fn a_create_index_that_cannot_be_compensated_leaves_nothing() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let before = indexes_of(&storage, meta.storage_id);
    let handle = begin(&txn);
    txn.commit(handle.clone()).expect("commit");
    let err = catalog
        .create_index(&handle, &index(meta.id, "ix_b", &[("b", false)]))
        .expect_err("the transaction is closed");
    assert_eq!(err.number, 50000);
    assert_eq!(indexes_of(&storage, meta.storage_id), before);
}

/// `create_index` over a table the catalogue does not hold is an internal bug: resolving a
/// name happens before the call.
#[test]
fn create_index_on_an_unknown_table_is_a_bug() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let err = catalog
        .create_index(&handle, &index(ObjectId(7), "ix_b", &[("b", false)]))
        .expect_err("no such table");
    assert!(err.message.contains("unknown table 7"), "{}", err.message);
}

/// `create_index` over a column the table does not declare is an internal bug naming 1911.
#[test]
fn create_index_on_an_unknown_column_names_1911() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let before = indexes_of(&storage, meta.storage_id).len();
    let handle = begin(&txn);
    let err = catalog
        .create_index(&handle, &index(meta.id, "ix_nope", &[("nope", false)]))
        .expect_err("no such column");
    assert!(err.message.contains("1911"), "{}", err.message);
    assert_eq!(indexes_of(&storage, meta.storage_id).len(), before);
}

/// The `unique` flag reaches `storage`: two live rows of the same key are refused with 2601,
/// which the executor rephrases into 2627 for a `PRIMARY KEY`.
#[test]
fn the_unique_flag_of_a_primary_key_reaches_storage() {
    let (catalog, storage, txn) = instance();
    let (meta, _) = table_with_a_key(&catalog, &txn, "t");
    let handle = begin(&txn);
    storage
        .insert(
            handle.id,
            meta.storage_id,
            &Row(vec![Value::I32(1), Value::I32(1)]),
        )
        .expect("the first row");
    let err = storage
        .insert(
            handle.id,
            meta.storage_id,
            &Row(vec![Value::I32(1), Value::I32(2)]),
        )
        .expect_err("the key of the primary key is taken");
    assert_eq!(err.number, 2601);
    txn.rollback(handle).expect("rollback");
}

/// The bound `table.rs` writes on `drop_table` holds for `drop_index`: a `DROP` undone by
/// `ROLLBACK TRANSACTION <savepoint>` keeps its mark, so the next `DROP` answers 3701 while
/// the index survives the `COMMIT`. Current state, not the aim.
#[test]
fn a_savepoint_rollback_leaves_the_drop_mark_in_place() {
    let (catalog, storage, txn) = instance();
    let (meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");

    let dropper = begin(&txn);
    let mark = txn.savepoint(&dropper).expect("savepoint");
    catalog.drop_index(&dropper, ordinary).expect("drop_index");
    txn.rollback_to(&dropper, mark)
        .expect("rollback to the savepoint");
    let err = catalog
        .drop_index(&dropper, ordinary)
        .expect_err("the mark is still there");
    assert_eq!(err.number, 3701);
    txn.commit(dropper).expect("commit");
    assert!(
        indexes_of(&storage, meta.storage_id)
            .iter()
            .any(|(id, _)| *id == ordinary),
        "the deferred drop was forgotten by rollback_to: the index is still in storage"
    );
}

/// A `DROP INDEX` over the index of a `PRIMARY KEY` is refused: dropping it would leave the
/// clustered key of the shape without its index and `TableMeta.clustered` pointing at an
/// identifier `storage` no longer knows. SQL Server refuses it at 3723 state 4, a number
/// `vauban-errors` does not carry.
#[test]
fn dropping_the_index_of_a_primary_key_names_3723() {
    let (catalog, storage, txn) = instance();
    let (meta, clustered) = table_with_a_key(&catalog, &txn, "t");
    let handle = begin(&txn);
    let err = catalog
        .drop_index(&handle, clustered)
        .expect_err("the index backs the primary key");
    assert!(err.message.contains("3723"), "{}", err.message);
    assert!(
        err.message.contains("PRIMARY KEY constraint enforcement"),
        "{}",
        err.message
    );
    txn.commit(handle).expect("commit");
    assert!(
        indexes_of(&storage, meta.storage_id)
            .iter()
            .any(|(id, _)| *id == clustered),
        "the index of the primary key is still there"
    );
}

/// The same refusal for the index of a `UNIQUE` constraint, which SQL Server sends at
/// state 5.
#[test]
fn dropping_the_index_of_a_unique_constraint_names_3723() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![unique(false, &[("b", false)])]))
        .expect("create_table");
    txn.commit(handle).expect("commit");
    let backing = match meta.constraints.as_slice() {
        [ConstraintMeta::Unique(id)] => *id,
        other => panic!("one unique constraint, got {other:?}"),
    };

    let handle = begin(&txn);
    let err = catalog
        .drop_index(&handle, backing)
        .expect_err("the index backs the unique constraint");
    assert!(err.message.contains("3723"), "{}", err.message);
    assert!(
        err.message.contains("UNIQUE KEY constraint enforcement"),
        "{}",
        err.message
    );
}

/// A `DROP INDEX` over an index whose table carries a deferred `DROP TABLE` of the same
/// transaction is refused with 3701 at state 6: the commit actions run in registration order,
/// so the table would take the index with it before the action of this `DROP INDEX` runs.
#[test]
fn dropping_an_index_of_a_table_being_dropped_is_3701_state_6() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let (meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");
    let handle = begin(&txn);
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    let err = catalog
        .drop_index(&handle, ordinary)
        .expect_err("the table of the index is being dropped");
    assert_eq!(err.number, 3701);
    assert_eq!(err.state, 6, "state 7 is for a table that is there");
    assert!(err.message.contains("'dbo.t.ix_b'"), "{}", err.message);
    // The refusal is what keeps the commit whole: the table goes, index included.
    txn.commit(handle).expect("commit");
    assert!(
        !storage
            .tables(db)
            .expect("tables()")
            .iter()
            .any(|(id, _)| *id == meta.storage_id)
    );
}

/// A key that names one column twice is refused, at the `CREATE TABLE` as at the
/// `CREATE INDEX`: SQL Server answers 1909, a number `vauban-errors` does not carry.
#[test]
fn a_key_with_a_repeated_column_names_1909() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let before = storage.tables(db).expect("tables()").len();
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table("t", vec![primary_key(true, &[("a", false), ("a", false)])]),
        )
        .expect_err("the key names a twice");
    assert!(err.message.contains("1909"), "{}", err.message);
    assert_eq!(
        storage.tables(db).expect("tables()").len(),
        before,
        "the refusal comes before the table exists"
    );

    let (meta, _) = table_with_a_key(&catalog, &txn, "u");
    let err = catalog
        .create_index(
            &handle,
            &index(meta.id, "ix_aa", &[("a", false), ("a", true)]),
        )
        .expect_err("ASC then DESC is still one column twice");
    assert!(err.message.contains("1909"), "{}", err.message);
}

/// Two constraints of one name in one `CREATE TABLE`: SQL Server answers 8168, not the 1913
/// of two `CREATE INDEX` of one name.
#[test]
fn two_constraints_of_one_name_name_8168() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    ConstraintDef::PrimaryKey {
                        name: Some("c1".to_owned()),
                        columns: sorted(&[("a", false)]),
                        clustered: true,
                    },
                    ConstraintDef::Unique {
                        name: Some("C1".to_owned()),
                        columns: sorted(&[("b", false)]),
                        clustered: false,
                    },
                ],
            ),
        )
        .expect_err("two constraints of one name");
    assert!(err.message.contains("8168"), "{}", err.message);
    assert!(
        !err.message.contains("1913"),
        "1913 is for two CREATE INDEX: {}",
        err.message
    );
}

/// A table whose keys failed is left in `storage` and is not named by the catalogue: a second
/// `CREATE TABLE` of that name in the same transaction creates a namesake, where SQL Server
/// answers 2714. Current state of the bound written on `apply_table_keys`, not the aim.
#[test]
fn a_failed_key_leaves_a_table_the_catalogue_does_not_name() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let before = storage.tables(db).expect("tables()").len();
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    ConstraintDef::PrimaryKey {
                        name: Some("c1".to_owned()),
                        columns: sorted(&[("a", false)]),
                        clustered: true,
                    },
                    ConstraintDef::Unique {
                        name: Some("c1".to_owned()),
                        columns: sorted(&[("b", false)]),
                        clustered: false,
                    },
                ],
            ),
        )
        .expect_err("the second constraint takes the name of the first");
    assert!(err.message.contains("8168"), "{}", err.message);
    assert_eq!(
        storage.tables(db).expect("tables()").len(),
        before + 1,
        "the table was created before the keys were applied"
    );
    let second = catalog
        .create_table(&handle, &table("t", Vec::new()))
        .expect("the catalogue does not name the first table any more");
    assert_eq!(
        storage.tables(db).expect("tables()").len(),
        before + 2,
        "two tables named master.dbo.t in storage"
    );
    assert_eq!(second.name, "t");
    txn.rollback(handle).expect("rollback");
    assert_eq!(
        storage.tables(db).expect("tables()").len(),
        before,
        "both go with the transaction"
    );
}

/// The name of an index dropped in the transaction stays taken until the `COMMIT`, the index
/// being still in `storage`, where SQL Server frees it at once. Current state of the bound
/// written on `drop_index`, not the aim.
#[test]
fn a_name_dropped_in_the_transaction_is_not_free_before_the_commit() {
    let (catalog, _storage, txn) = instance();
    let (meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");
    let handle = begin(&txn);
    catalog.drop_index(&handle, ordinary).expect("drop_index");
    let err = catalog
        .create_index(&handle, &index(meta.id, "ix_b", &[("a", false)]))
        .expect_err("the name is held by the deferred drop");
    assert!(err.message.contains("1913"), "{}", err.message);
    txn.commit(handle).expect("commit");
    let second = begin(&txn);
    catalog
        .create_index(&second, &index(meta.id, "ix_b", &[("a", false)]))
        .expect("after the commit the name is free");
    txn.commit(second).expect("commit");
}

/// The refusal of the point above is not a refusal of the pair: `DROP INDEX` then
/// `DROP TABLE` in one transaction is accepted, the action of the index running before the one
/// of the table, and the `COMMIT` takes both.
#[test]
fn an_index_dropped_before_its_table_is_accepted() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let (meta, ordinary) = table_with_an_ordinary_index(&catalog, &txn, "t");
    let handle = begin(&txn);
    catalog.drop_index(&handle, ordinary).expect("drop_index");
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    txn.commit(handle).expect("commit");
    assert!(
        !storage
            .tables(db)
            .expect("tables()")
            .iter()
            .any(|(id, _)| *id == meta.storage_id)
    );
}
