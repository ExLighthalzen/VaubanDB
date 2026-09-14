//! `Catalog::create_table` and `Catalog::drop_table` seen from outside the crate.
//!
//! These tests know the public API only: they build a [`TableDef`] by hand — turning a
//! `CREATE TABLE` statement into one is the business of the binder — and read the result
//! through [`TableMeta`] and through `Storage`.
//!
//! The tables are created in `master`, which the bootstrap made, and in a database created
//! straight through `Storage::create_database`.

use std::collections::BTreeSet;
use std::sync::Arc;

use vauban_catalog::{Catalog, ColumnDef, IdentitySpec, ObjectId, QualifiedName, TableDef};
use vauban_parser::{Expr, Literal, Span};
use vauban_storage::{DbId, MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{Len, SqlType, TypeInfo};

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

/// The identifiers of the tables of `db`.
fn table_ids(storage: &Arc<dyn Storage>, db: DbId) -> BTreeSet<TableId> {
    storage
        .tables(db)
        .expect("tables()")
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// The shape of `table` in `db`, or `None` when the table is gone.
fn columns_of(storage: &Arc<dyn Storage>, db: DbId, table: TableId) -> Option<usize> {
    storage
        .tables(db)
        .expect("tables()")
        .into_iter()
        .find(|(id, _)| *id == table)
        .map(|(_, shape)| shape.columns.len())
}

/// An open transaction at `READ COMMITTED`.
fn begin(txn: &Arc<TransactionManager>) -> TxnHandle {
    txn.begin(IsolationLevel::ReadCommitted)
}

/// A column with its type and nothing else on it.
fn column(name: &str, ty: SqlType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
        default: None,
        identity: None,
        computed: None,
    }
}

/// `master.dbo.<name>` with the columns given.
fn table(name: &str, columns: Vec<ColumnDef>) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
        },
        columns,
        constraints: Vec::new(),
    }
}

/// `dbo.t (a int NOT NULL, b nvarchar(20) NULL)`, the table the tests are written on.
fn two_columns(name: &str) -> TableDef {
    table(
        name,
        vec![
            column("a", SqlType::Int, false),
            column("b", SqlType::NVarChar(Len::Fixed(20)), true),
        ],
    )
}

/// The shape `storage` holds has one column per column of the definition, in that order.
#[test]
fn create_table_shape_matches_columns() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit");

    assert_eq!(columns_of(&storage, db, meta.storage_id), Some(2));
    let shape = storage
        .tables(db)
        .expect("tables()")
        .into_iter()
        .find(|(id, _)| *id == meta.storage_id)
        .expect("the table is in master")
        .1;
    assert_eq!(shape.columns[0], TypeInfo::new(SqlType::Int, false));
    assert_eq!(
        shape.columns[1],
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), true)
    );
    assert_eq!(shape.clustered_key, None, "no key was declared");
    assert_eq!(meta.columns.len(), 2);
    assert_eq!(meta.database, db);
    assert_eq!(meta.schema, "dbo");
    assert_eq!(meta.name, "t");
    assert!(meta.constraints.is_empty());
    assert_eq!(meta.clustered, None);

    // The table is empty: a scan of its storage identifier reads no row.
    let reader = begin(&txn);
    let snapshot = txn.statement_snapshot(&reader);
    let rows = storage
        .scan(&snapshot, meta.storage_id)
        .expect("scan")
        .count();
    txn.commit(reader).expect("commit of the reader");
    assert_eq!(rows, 0);
}

/// Two creations get two identifiers, and an identifier freed by a committed `DROP` is not
/// handed out again — the behaviour of SQL Server, quoted in `table.rs`.
#[test]
fn create_table_assigns_stable_object_id() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let first = catalog
        .create_table(&handle, &two_columns("t1"))
        .expect("create t1");
    let second = catalog
        .create_table(&handle, &two_columns("t2"))
        .expect("create t2");
    txn.commit(handle).expect("commit");
    assert_ne!(first.id, second.id);
    assert!(first.id < second.id, "the counter moves forward");

    let dropper = begin(&txn);
    catalog.drop_table(&dropper, first.id).expect("drop t1");
    txn.commit(dropper).expect("commit of the drop");
    assert!(!table_ids(&storage, db).contains(&first.storage_id));

    let again = begin(&txn);
    let third = catalog
        .create_table(&again, &two_columns("t1"))
        .expect("create t1 again");
    txn.commit(again).expect("commit");
    let ids: BTreeSet<ObjectId> = [first.id, second.id, third.id].into_iter().collect();
    assert_eq!(
        ids.len(),
        3,
        "the identifier of t1 was not handed out again"
    );
    assert!(second.id < third.id);
}

/// The object identifier and the storage identifier are two different numbers for one table.
#[test]
fn the_object_id_is_not_the_table_id() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit");
    assert_ne!(i64::from(meta.id.0), i64::from(meta.storage_id.0));
    let _ = storage;
}

/// `DROP TABLE` waits for the `COMMIT`: the table is still in `storage` after the call and
/// gone after the commit.
#[test]
fn drop_table_deferred() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit of the create");

    let dropper = begin(&txn);
    catalog.drop_table(&dropper, meta.id).expect("drop_table");
    assert!(
        table_ids(&storage, db).contains(&meta.storage_id),
        "before the commit, storage still lists the table"
    );
    let snapshot = txn.statement_snapshot(&dropper);
    assert_eq!(
        storage
            .scan(&snapshot, meta.storage_id)
            .expect("scan")
            .count(),
        0,
        "the table is still readable, and empty"
    );
    txn.commit(dropper).expect("commit of the drop");
    assert!(!table_ids(&storage, db).contains(&meta.storage_id));
}

/// A `ROLLBACK` undoes the creation: the compensation registered by `create_table` drops the
/// storage table.
#[test]
fn create_table_rollback_drops_storage() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let before = table_ids(&storage, db);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    assert!(table_ids(&storage, db).contains(&meta.storage_id));
    txn.rollback(handle).expect("rollback");
    assert!(!table_ids(&storage, db).contains(&meta.storage_id));
    assert_eq!(table_ids(&storage, db), before, "master is as it was");
}

/// A name freed by a rolled-back `CREATE` can be taken again, and the identifier of the
/// second table is a new one.
#[test]
fn a_name_freed_by_a_rollback_can_be_taken_again() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let first = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.rollback(handle).expect("rollback");

    let again = begin(&txn);
    let second = catalog
        .create_table(&again, &two_columns("t"))
        .expect("the name is free again");
    txn.commit(again).expect("commit");
    assert_ne!(first.id, second.id);
    assert_ne!(first.storage_id, second.storage_id);
}

/// A `DROP` that is rolled back leaves the table in place, and droppable again.
#[test]
fn a_rolled_back_drop_leaves_the_table_droppable() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit of the create");

    let dropper = begin(&txn);
    catalog.drop_table(&dropper, meta.id).expect("drop_table");
    txn.rollback(dropper).expect("rollback of the drop");
    assert!(table_ids(&storage, db).contains(&meta.storage_id));

    let second = begin(&txn);
    catalog
        .drop_table(&second, meta.id)
        .expect("the table is droppable again");
    txn.commit(second).expect("commit of the second drop");
    assert!(!table_ids(&storage, db).contains(&meta.storage_id));
}

/// `NOT NULL`, `IDENTITY(10, 2)`, `DEFAULT` and a computed expression land on the
/// `ColumnMeta`, stored and not evaluated.
#[test]
fn not_null_and_identity_are_on_column_meta() {
    let (catalog, _storage, txn) = instance();
    let mut def = table(
        "t",
        vec![
            column("id", SqlType::Int, false),
            column("b", SqlType::NVarChar(Len::Fixed(20)), true),
        ],
    );
    def.columns[0].identity = Some(IdentitySpec {
        seed: 10,
        increment: 2,
    });
    let zero = Expr::Literal(Literal::Integer("0".to_owned()), Span::EMPTY);
    def.columns[1].default = Some(zero.clone());

    let handle = begin(&txn);
    let meta = catalog.create_table(&handle, &def).expect("create_table");
    txn.commit(handle).expect("commit");

    assert!(!meta.columns[0].ty.nullable);
    assert_eq!(
        meta.columns[0].identity,
        Some(IdentitySpec {
            seed: 10,
            increment: 2
        })
    );
    assert_eq!(meta.columns[0].default, None);
    assert!(meta.columns[1].ty.nullable);
    assert_eq!(meta.columns[1].identity, None);
    assert_eq!(meta.columns[1].default, Some(zero));
}

/// A bare `IDENTITY` is `IDENTITY(1, 1)`, and a computed column is stored as its expression.
#[test]
fn a_bare_identity_and_a_computed_column_are_stored_as_written() {
    let (catalog, _storage, txn) = instance();
    let expr = Expr::Literal(Literal::Integer("42".to_owned()), Span::EMPTY);
    let mut def = table(
        "t",
        vec![
            column("id", SqlType::Int, false),
            column("c", SqlType::Int, true),
        ],
    );
    def.columns[0].identity = Some(IdentitySpec::default());
    def.columns[1].computed = Some(expr.clone());

    let handle = begin(&txn);
    let meta = catalog.create_table(&handle, &def).expect("create_table");
    txn.commit(handle).expect("commit");
    assert_eq!(
        meta.columns[0].identity,
        Some(IdentitySpec {
            seed: 1,
            increment: 1
        })
    );
    assert_eq!(meta.columns[1].computed, Some(expr));
}

/// `ColumnMeta::id` is one-based, `ColumnMeta::ordinal` is the position in the row, from `0`.
#[test]
fn column_id_is_one_based_and_ordinal_is_the_row_position() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit");
    assert_eq!(meta.columns[0].id.0, 1);
    assert_eq!(meta.columns[0].ordinal, 0);
    assert_eq!(meta.columns[0].name, "a");
    assert_eq!(meta.columns[1].id.0, 2);
    assert_eq!(meta.columns[1].ordinal, 1);
    assert_eq!(meta.columns[1].name, "b");
}

/// A second table of the same name in the same database answers 2714.
#[test]
fn a_second_table_of_the_same_name_is_2714() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    let err = catalog
        .create_table(&handle, &two_columns("T"))
        .expect_err("the name is taken, case-insensitively");
    txn.commit(handle).expect("commit");
    assert_eq!(err.number, 2714);
    assert_eq!(err.state, 6);
    assert!(err.message.contains("'T'"), "{}", err.message);
}

/// The same name in another database is another table.
#[test]
fn the_same_name_in_another_database_is_another_table() {
    let (catalog, storage, txn) = instance();
    let other = storage
        .create_database("other_db")
        .expect("create_database");
    let handle = begin(&txn);
    let first = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create in master");
    let mut def = two_columns("t");
    def.name.database = "other_db".to_owned();
    let second = catalog
        .create_table(&handle, &def)
        .expect("create in the other database");
    txn.commit(handle).expect("commit");
    assert_ne!(first.id, second.id);
    assert_eq!(second.database, other);
    assert!(table_ids(&storage, other).contains(&second.storage_id));
}

/// Dropping an identifier the catalogue does not hold answers 3701.
#[test]
fn dropping_an_unknown_table_is_3701() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let err = catalog
        .drop_table(&handle, ObjectId(4_242))
        .expect_err("no such object");
    txn.commit(handle).expect("commit");
    assert_eq!(err.number, 3701);
    assert!(err.message.contains("'4242'"), "{}", err.message);
}

/// Dropping twice in one transaction answers 3701 the second time, with the name of the
/// table.
#[test]
fn dropping_the_same_table_twice_is_3701() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit of the create");

    let dropper = begin(&txn);
    catalog.drop_table(&dropper, meta.id).expect("first drop");
    let err = catalog
        .drop_table(&dropper, meta.id)
        .expect_err("the drop is already registered");
    txn.commit(dropper).expect("commit of the drop");
    assert_eq!(err.number, 3701);
    assert!(err.message.contains("'t'"), "{}", err.message);
}

/// Two `IDENTITY` columns are an internal bug here: 2744 is raised before the catalogue.
#[test]
fn two_identity_columns_are_a_bug() {
    let (catalog, _storage, txn) = instance();
    let mut def = two_columns("t");
    def.columns[0].identity = Some(IdentitySpec::default());
    def.columns[1].identity = Some(IdentitySpec::default());
    let handle = begin(&txn);
    let err = catalog
        .create_table(&handle, &def)
        .expect_err("one identity column at most");
    txn.commit(handle).expect("commit");
    assert_eq!(err.number, 50000, "an internal bug, not 2744");
    assert!(
        err.message.contains("2 identity columns"),
        "message: {}",
        err.message
    );
}

/// A definition without a column is an internal bug: `storage` asks for a non-empty shape.
#[test]
fn a_table_without_a_column_is_a_bug() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let err = catalog
        .create_table(&handle, &table("t", Vec::new()))
        .expect_err("no column");
    txn.commit(handle).expect("commit");
    assert_eq!(err.number, 50000);
    assert!(err.message.contains("has no column"), "{}", err.message);
}

/// A creation whose compensation cannot be registered — here because the transaction is
/// closed — leaves no table behind in `storage`.
#[test]
fn a_create_that_cannot_be_compensated_leaves_nothing() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let before = table_ids(&storage, db);
    let handle = begin(&txn);
    txn.commit(handle.clone()).expect("commit");
    let err = catalog
        .create_table(&handle, &two_columns("t"))
        .expect_err("the transaction is closed");
    assert_eq!(err.number, 50000);
    assert_eq!(table_ids(&storage, db), before);
}

/// `SAVE TRANSACTION s; DROP TABLE t; ROLLBACK TRANSACTION s;` — the state this version
/// leaves, frozen so that the change which lifts it sees this test change.
///
/// `rollback_to` forgets the deferred drop, so the table lives on in `storage` and survives
/// the `COMMIT`; the catalogue keeps its mark, so it answers 3701 on a table that is still
/// there. Lifting the mark asks `vauban_txn` for something its public API does not give:
/// whether an action registered earlier is still in the log (rustdoc of `drop_table`).
#[test]
fn a_savepoint_rollback_leaves_the_drop_mark_in_place() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit of the create");

    let dropper = begin(&txn);
    let mark = txn.savepoint(&dropper).expect("savepoint");
    catalog.drop_table(&dropper, meta.id).expect("drop_table");
    txn.rollback_to(&dropper, mark)
        .expect("rollback to the savepoint");
    let err = catalog
        .drop_table(&dropper, meta.id)
        .expect_err("the mark is still there: current state, not the aim");
    assert_eq!(err.number, 3701);
    txn.commit(dropper).expect("commit");
    assert!(
        table_ids(&storage, db).contains(&meta.storage_id),
        "the deferred drop was forgotten by rollback_to: the table is still in storage"
    );
}

/// The other half of the savepoint case: a `CREATE` undone by `rollback_to` is compensated by
/// `txn`, and the catalogue follows because `storage` lost the table.
#[test]
fn a_savepoint_rollback_undoes_a_create() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let mark = txn.savepoint(&handle).expect("savepoint");
    let first = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("create_table");
    txn.rollback_to(&handle, mark)
        .expect("rollback to the savepoint");
    assert!(!table_ids(&storage, db).contains(&first.storage_id));
    let second = catalog
        .create_table(&handle, &two_columns("t"))
        .expect("the name is free again");
    txn.commit(handle).expect("commit");
    assert_ne!(first.id, second.id);
    assert!(table_ids(&storage, db).contains(&second.storage_id));
}

/// Two names that differ only outside ASCII are two tables here, where SQL Server answers
/// 2714 (`Été` / `été`, rustdoc of `live_named`). Frozen as the state of this version:
/// folding them together needs the collation of the database (`snapshot.rs`).
#[test]
fn two_names_that_differ_outside_ascii_are_two_tables_here() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let upper = catalog
        .create_table(&handle, &two_columns("Été"))
        .expect("create Été");
    let lower = catalog
        .create_table(&handle, &two_columns("été"))
        .expect("create été: not folded by eq_ignore_ascii_case");
    // The name written exactly the same way is caught, so the comparison is not simply off.
    let err = catalog
        .create_table(&handle, &two_columns("été"))
        .expect_err("that exact name is taken");
    txn.commit(handle).expect("commit");
    assert_eq!(err.number, 2714);
    assert_ne!(upper.id, lower.id);
    assert!(table_ids(&storage, db).contains(&upper.storage_id));
    assert!(table_ids(&storage, db).contains(&lower.storage_id));
}

/// A second catalogue over the same storage hands out identifiers clear of the first one's,
/// so a `drop_table` with an identifier of the first catalogue destroys nothing.
#[test]
fn a_second_catalogue_hands_out_other_object_ids() {
    let (first, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let old = first
        .create_table(&handle, &two_columns("t1"))
        .expect("create t1");
    txn.commit(handle).expect("commit");
    drop(first);

    let second = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("restart");
    let handle = begin(&txn);
    let new = second
        .create_table(&handle, &two_columns("t2"))
        .expect("create t2");
    assert_ne!(old.id, new.id, "the counter was raised past the first run");
    let err = second
        .drop_table(&handle, old.id)
        .expect_err("the second catalogue holds no such object");
    assert_eq!(err.number, 3701);
    txn.commit(handle).expect("commit");
    assert!(table_ids(&storage, db).contains(&old.storage_id));
    assert!(table_ids(&storage, db).contains(&new.storage_id));
}
