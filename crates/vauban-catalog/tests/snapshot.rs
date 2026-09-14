//! What a [`CatalogSnapshot`] resolves, read through the public API of the crate —
//! `Catalog::bootstrap`, `create_database`, `create_table`, `snapshot`.
//!
//! The indexes of a resolved table — `CatalogSnapshot::indexes_of` read from the
//! `IndexStore` — are checked here as well.
//!
//! The two sources a snapshot reads and the bound of the table half are written in
//! `src/snapshot.rs`; this file checks them. `the_table_half_does_not_follow_the_transaction_yet`
//! freezes the state of that bound, so the change which versions those rows sees it turn
//! red.

use std::sync::Arc;

use vauban_catalog::{
    Catalog, ColumnDef, ConstraintDef, ConstraintMeta, IndexDef, IndexMeta, ObjectId, ObjectKind,
    QualifiedName, SortedColumn, TableDef, TableMeta,
};
use vauban_storage::{IndexId, KeyColumn, MemoryStorage, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

/// A catalogue bootstrapped over a fresh `MemoryStorage`, with the manager its transactions
/// come from.
fn instance() -> (Catalog, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
    (catalog, manager)
}

/// The same instance, keeping the `Storage` the catalogue was built on: what a test needs to
/// read the rows behind the `storage_id` of an internal table.
fn instance_with_its_storage() -> (Catalog, Arc<TransactionManager>, Arc<dyn Storage>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog =
        Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
    (catalog, manager, storage)
}

/// The rows of the storage table `table`, read in a transaction of its own.
fn rows(
    storage: &Arc<dyn Storage>,
    manager: &Arc<TransactionManager>,
    table: TableId,
) -> Vec<Vec<Value>> {
    let handle = begin(manager);
    let snapshot = manager.statement_snapshot(&handle);
    let read: Vec<Vec<Value>> = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    manager.commit(handle).expect("commit of the reading txn");
    read
}

/// The `nvarchar` value of a piece of text, as the internal tables store it.
fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

/// The internal tables of `master` this build describes, in the order the bootstrap creates
/// them: the tables of the files of `views/` and then the two of the bootstrap. The list read
/// from the code is in the unit test
/// `the_internal_tables_are_numbered_from_their_position` of `src/snapshot.rs`, which is where
/// `bootstrap::internal_table_defs` is reachable; here it is written out, so that a table
/// added to `views/` is added to this list too.
const INTERNAL_TABLES: [&str; 20] = [
    "vauban_sys_objects",
    "vauban_sys_columns",
    "vauban_sys_types",
    "vauban_sys_indexes",
    "vauban_sys_index_columns",
    "vauban_sys_key_constraints",
    "vauban_sys_identity_columns",
    "vauban_is_tables",
    "vauban_is_columns",
    "vauban_sys_foreign_keys",
    "vauban_sys_foreign_key_columns",
    "vauban_sys_check_constraints",
    "vauban_sys_default_constraints",
    "vauban_sys_all_objects",
    "vauban_sys_all_columns",
    "vauban_sys_partitions",
    "vauban_sys_allocation_units",
    "vauban_sys_files",
    "vauban_sys_databases",
    "vauban_sys_schemas",
];

/// Opens a transaction of `manager`.
fn begin(manager: &Arc<TransactionManager>) -> TxnHandle {
    manager.begin(IsolationLevel::ReadCommitted)
}

/// Creates the database `name` in a transaction of its own and commits it.
fn database(catalog: &Catalog, manager: &Arc<TransactionManager>, name: &str) {
    let handle = begin(manager);
    catalog
        .create_database(&handle, name, None)
        .expect("create_database");
    manager.commit(handle).expect("commit of the create");
}

/// A one-column table `db.schema.name`, the column being `a int not null`.
fn def(db: &str, schema: &str, name: &str) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: db.to_owned(),
            schema: schema.to_owned(),
            name: name.to_owned(),
        },
        columns: vec![ColumnDef {
            name: "a".to_owned(),
            ty: TypeInfo::new(SqlType::Int, false),
            default: None,
            identity: None,
            computed: None,
        }],
        constraints: Vec::new(),
    }
}

/// Creates the table of `def` in a transaction of its own and commits it.
fn table(catalog: &Catalog, manager: &Arc<TransactionManager>, def: &TableDef) -> TableMeta {
    let handle = begin(manager);
    let meta = catalog.create_table(&handle, def).expect("create_table");
    manager.commit(handle).expect("commit of the create");
    meta
}

/// A catalogue holding `d.dbo.t`, the table the tests are written on.
fn instance_with_d_dbo_t() -> (Catalog, Arc<TransactionManager>, TableMeta) {
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let meta = table(&catalog, &manager, &def("d", "dbo", "t"));
    (catalog, manager, meta)
}

/// The same table plus the column `b`, with a clustered `PRIMARY KEY` on `a`: the table the
/// index tests are written on.
fn def_with_a_key(db: &str, schema: &str, name: &str) -> TableDef {
    let mut def = def(db, schema, name);
    def.columns.push(ColumnDef {
        name: "b".to_owned(),
        ty: TypeInfo::new(SqlType::Int, false),
        default: None,
        identity: None,
        computed: None,
    });
    def.constraints = vec![ConstraintDef::PrimaryKey {
        name: None,
        columns: vec![SortedColumn {
            column: "a".to_owned(),
            descending: false,
        }],
        clustered: true,
    }];
    def
}

/// A catalogue holding `d.dbo.t` with a clustered `PRIMARY KEY` on `a`, and the identifier
/// of the index that key is backed by.
fn instance_with_a_keyed_table() -> (Catalog, Arc<TransactionManager>, TableMeta, IndexId) {
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let meta = table(&catalog, &manager, &def_with_a_key("d", "dbo", "t"));
    let clustered = meta.clustered.expect("a clustered primary key");
    (catalog, manager, meta, clustered)
}

/// A non-unique, non-clustered index called `name` over the column `b`.
fn index_on_b(table: ObjectId, name: &str) -> IndexDef {
    IndexDef {
        table,
        name: name.to_owned(),
        columns: vec![SortedColumn {
            column: "b".to_owned(),
            descending: false,
        }],
        unique: false,
        clustered: false,
    }
}

/// Creates the index of `def` in a transaction of its own and commits it.
fn create_index(catalog: &Catalog, manager: &Arc<TransactionManager>, def: &IndexDef) -> IndexMeta {
    let handle = begin(manager);
    let meta = catalog.create_index(&handle, def).expect("create_index");
    manager.commit(handle).expect("commit of the create");
    meta
}

/// The identifiers of the indexes of an answer of `indexes_of`, in the order answered.
fn ids(indexes: &[IndexMeta]) -> Vec<IndexId> {
    indexes.iter().map(|index| index.id).collect()
}

#[test]
fn resolve_unqualified_uses_default_schema() {
    let (catalog, manager, meta) = instance_with_d_dbo_t();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    // One part: the name alone, under the default schema of the session.
    let object = snapshot
        .resolve_object("d", None, "t", "dbo")
        .expect("`t` under the default schema `dbo`");
    assert_eq!(object.id, meta.id);
    assert_eq!(object.kind, ObjectKind::Table);
    // Two and three parts give the same object.
    assert_eq!(
        snapshot.resolve_object("d", Some("dbo"), "t", "dbo"),
        Some(object)
    );
    assert_eq!(
        snapshot.resolve_object("d", Some("dbo"), "t", "other"),
        Some(object),
        "a written schema is used, the default schema is not consulted"
    );
    // No second try on `dbo`: a session whose default schema is another one finds nothing.
    assert!(
        snapshot.resolve_object("d", None, "t", "other").is_none(),
        "an unqualified name is looked up in the default schema only"
    );
    assert!(
        snapshot
            .resolve_object("d", Some("other"), "t", "dbo")
            .is_none()
    );
}

#[test]
fn resolve_is_case_insensitive() {
    let (catalog, manager, meta) = instance_with_d_dbo_t();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    for (db, schema, name) in [
        ("d", "dbo", "t"),
        ("d", "DBO", "T"),
        ("D", "dbo", "T"),
        ("D", "DBO", "t"),
    ] {
        let object = snapshot
            .resolve_object(db, Some(schema), name, "dbo")
            .unwrap_or_else(|| panic!("{db}.{schema}.{name} resolves to the table"));
        assert_eq!(object.id, meta.id);
    }
    assert_eq!(
        snapshot.resolve_object("d", None, "T", "DBO").map(|o| o.id),
        Some(meta.id),
        "the default schema is folded like a written one"
    );
    assert!(snapshot.database("D").is_some(), "so is a database name");

    // Accent-sensitive, and case folded on `É`, which is what tells the collation apart from
    // an ASCII case fold: in SQL Server, after `CREATE TABLE dbo.[té] (a int);`,
    // `OBJECT_ID(N'dbo.TÉ')` is the identifier of that table while `OBJECT_ID(N'dbo.te')`
    // is NULL. `É` is a character of CP1252; the fold stops at the edge of that page here,
    // which `a_name_outside_cp1252_is_not_case_folded` checks.
    let accented = table(&catalog, &manager, &def("d", "dbo", "t_té"));
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    assert_eq!(
        snapshot
            .resolve_object("d", None, "T_TÉ", "dbo")
            .map(|object| object.id),
        Some(accented.id),
        "case folded beyond ASCII, within CP1252"
    );
    assert!(
        snapshot.resolve_object("d", None, "t_te", "dbo").is_none(),
        "accents kept apart"
    );
}

#[test]
fn unknown_object_is_none() {
    let (catalog, manager, meta) = instance_with_d_dbo_t();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    assert!(
        snapshot
            .resolve_object("d", None, "nosuch", "dbo")
            .is_none()
    );
    assert!(
        snapshot
            .resolve_object("d", Some("nosuch"), "t", "dbo")
            .is_none()
    );
    assert!(
        snapshot
            .resolve_object("nosuch", None, "t", "dbo")
            .is_none(),
        "a database that is not there resolves nothing, and is no error"
    );
    assert!(snapshot.database("nosuch").is_none());
    assert!(snapshot.table(ObjectId(meta.id.0 + 1)).is_none());
    assert!(snapshot.object_name(ObjectId(meta.id.0 + 1)).is_none());
    assert!(snapshot.indexes_of(ObjectId(meta.id.0 + 1)).is_empty());
    assert!(snapshot.view_definition(ObjectId(meta.id.0 + 1)).is_none());
}

#[test]
fn uncommitted_table_is_visible_in_own_snapshot() {
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");

    let writer = begin(&manager);
    let meta = catalog
        .create_table(&writer, &def("d", "dbo", "t"))
        .expect("create_table");
    let snapshot = catalog.snapshot(&writer);
    assert_eq!(
        snapshot.resolve_object("d", None, "t", "dbo").map(|o| o.id),
        Some(meta.id),
        "the transaction sees the table it has just created"
    );
    assert_eq!(
        snapshot.table(meta.id).map(|t| t.storage_id),
        Some(meta.storage_id)
    );

    // Counter-proof that this is the table of that transaction and not a name resolved out
    // of thin air: rolled back, the compensation drops it and no snapshot resolves it.
    manager.rollback(writer).expect("rollback");
    let reader = begin(&manager);
    let snapshot = catalog.snapshot(&reader);
    assert!(snapshot.resolve_object("d", None, "t", "dbo").is_none());
    assert!(snapshot.table(meta.id).is_none());
}

#[test]
fn the_table_half_does_not_follow_the_transaction_yet() {
    // The bound of `src/snapshot.rs`: the metadata of a table is a map in memory, so the
    // two shapes below answer as this build does and not as SQL Server answers them.
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");

    let writer = begin(&manager);
    let meta = catalog
        .create_table(&writer, &def("d", "dbo", "t"))
        .expect("create_table");
    let reader = begin(&manager);
    assert_eq!(
        catalog
            .snapshot(&reader)
            .resolve_object("d", None, "t", "dbo")
            .map(|object| object.id),
        Some(meta.id),
        "current state: another transaction resolves a table whose create is not committed"
    );
    manager.commit(writer).expect("commit of the create");
    manager.commit(reader).expect("commit of the reader");

    let dropper = begin(&manager);
    catalog.drop_table(&dropper, meta.id).expect("drop_table");
    let reader = begin(&manager);
    assert!(
        catalog
            .snapshot(&reader)
            .resolve_object("d", None, "t", "dbo")
            .is_none(),
        "current state: another transaction misses a table whose drop is not committed"
    );
    manager.commit(reader).expect("commit of the reader");
    manager.rollback(dropper).expect("rollback of the drop");
}

#[test]
fn object_name_round_trip() {
    let (catalog, manager, meta) = instance_with_d_dbo_t();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    // Resolved with another case, the name comes back with the case of the creation.
    let object = snapshot
        .resolve_object("D", Some("DBO"), "T", "dbo")
        .expect("the table resolves");
    assert_eq!(
        snapshot.object_name(object.id),
        Some(QualifiedName {
            database: "d".to_owned(),
            schema: "dbo".to_owned(),
            name: "t".to_owned(),
        })
    );
    assert_eq!(snapshot.object_name(meta.id), Some(object.name.clone()));
}

#[test]
fn an_uncommitted_database_is_visible_in_its_own_snapshot_only() {
    // The database half of a snapshot reads versioned rows, so it follows the transaction.
    let (catalog, manager) = instance();
    let writer = begin(&manager);
    catalog
        .create_database(&writer, "d", None)
        .expect("create_database");
    assert!(catalog.snapshot(&writer).database("d").is_some());

    let reader = begin(&manager);
    assert!(
        catalog.snapshot(&reader).database("d").is_none(),
        "the row of the create is invisible to another transaction"
    );
    manager.commit(writer).expect("commit of the create");
    manager.commit(reader).expect("commit of the reader");

    let after = begin(&manager);
    let snapshot = catalog.snapshot(&after);
    let database = snapshot.database("d").expect("committed, the row is read");
    assert_eq!(database.name, "d");
    assert_eq!(database.collation, vauban_types::Collation::DEFAULT);
}

#[test]
fn a_table_of_a_database_another_transaction_cannot_see_is_not_resolved() {
    // What the table half does follow: an object of a database the snapshot does not see is
    // not resolved, its three-part name having no first part.
    let (catalog, manager) = instance();
    let writer = begin(&manager);
    catalog
        .create_database(&writer, "d", None)
        .expect("create_database");
    let meta = catalog
        .create_table(&writer, &def("d", "dbo", "t"))
        .expect("create_table");
    assert!(catalog.snapshot(&writer).table(meta.id).is_some());

    let reader = begin(&manager);
    let snapshot = catalog.snapshot(&reader);
    assert!(snapshot.resolve_object("d", None, "t", "dbo").is_none());
    assert!(snapshot.table(meta.id).is_none());
    assert!(snapshot.object_name(meta.id).is_none());
    manager.commit(reader).expect("commit of the reader");
    manager.commit(writer).expect("commit of the create");
}

#[test]
fn a_resolved_table_carries_its_columns() {
    let (catalog, manager, meta, clustered) = instance_with_a_keyed_table();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let object = snapshot
        .resolve_object("d", None, "t", "dbo")
        .expect("the table resolves");
    let read = snapshot.table(object.id).expect("the table is a table");
    assert_eq!(read, &meta, "what `create_table` gave back");
    assert_eq!(read.columns.len(), 2);
    assert_eq!(read.columns[0].name, "a");
    assert_eq!(read.storage_id, meta.storage_id);
    // The index of the primary key is reached from the same identifier as the columns.
    assert_eq!(ids(snapshot.indexes_of(object.id)), vec![clustered]);
    assert_eq!(
        read.constraints,
        vec![ConstraintMeta::PrimaryKey(clustered)]
    );
    assert!(
        snapshot.view_definition(object.id).is_none(),
        "a table is no view"
    );
}

/// The index a clustered `PRIMARY KEY` is backed by is the one `indexes_of` answers, and its
/// identifier is the `clustered` of the table.
#[test]
fn indexes_of_a_table_with_a_primary_key_has_its_index() {
    let (catalog, manager, meta, clustered) = instance_with_a_keyed_table();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let read = snapshot.indexes_of(meta.id);
    assert_eq!(read.len(), 1, "the index of the primary key");
    assert_eq!(read[0].id, clustered, "the IndexId of TableMeta.clustered");
    assert!(read[0].unique, "the index of a key is unique");
    assert!(read[0].primary_key);
    assert!(read[0].clustered);
    assert_eq!(
        read[0].columns,
        vec![KeyColumn {
            column: 0,
            descending: false,
        }],
        "the key column `a`, first in the row"
    );
}

/// A `CREATE INDEX` over that table gives a second entry, after the one of the key: the list
/// is ordered by `IndexId`.
#[test]
fn indexes_of_after_create_index_has_two() {
    let (catalog, manager, meta, clustered) = instance_with_a_keyed_table();
    let handle = begin(&manager);
    let created = catalog
        .create_index(&handle, &index_on_b(meta.id, "ix_b"))
        .expect("create_index");
    // The store does not follow the transaction (`src/snapshot.rs`, section "Bound"): the
    // index is in the snapshot of another transaction before the commit of this one.
    let other = begin(&manager);
    assert_eq!(
        ids(catalog.snapshot(&other).indexes_of(meta.id)),
        vec![clustered, created.id]
    );
    manager.commit(handle).expect("commit of the create");

    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    let read = snapshot.indexes_of(meta.id);
    assert_eq!(read.len(), 2);
    assert!(read[0].id < read[1].id, "by increasing IndexId");
    assert_eq!(ids(read), vec![clustered, created.id]);
    assert_eq!(read[1].name, "ix_b");
    assert!(!read[1].unique, "`CREATE INDEX` without UNIQUE");
    assert!(!read[1].primary_key);
    assert!(!read[1].clustered);
    assert_eq!(
        read[1].columns,
        vec![KeyColumn {
            column: 1,
            descending: false,
        }],
        "the column `b`, second in the row"
    );
}

/// An identifier the snapshot does not hold answers an empty slice, as a table created
/// without key and without index does.
#[test]
fn indexes_of_an_unknown_table_is_empty() {
    let (catalog, manager, meta, _) = instance_with_a_keyed_table();
    let heap = table(&catalog, &manager, &def("d", "dbo", "heap"));
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    assert!(
        snapshot.indexes_of(ObjectId(meta.id.0 + 10_000)).is_empty(),
        "an identifier the snapshot does not hold"
    );
    assert!(snapshot.table(ObjectId(meta.id.0 + 10_000)).is_none());
    assert!(
        snapshot.indexes_of(heap.id).is_empty(),
        "a table created without key and without index"
    );
    // Counter-proof that the emptiness comes from the identifier and not from the method.
    assert_eq!(snapshot.indexes_of(meta.id).len(), 1);
}

/// A deferred `DROP INDEX` takes the index out of the snapshot before its `COMMIT` — the
/// rule of `index.rs`, written in the rustdoc of `indexes_of` — and the commit leaves it out.
#[test]
fn a_dropped_index_leaves_the_snapshot_after_commit() {
    let (catalog, manager, meta, clustered) = instance_with_a_keyed_table();
    let created = create_index(&catalog, &manager, &index_on_b(meta.id, "ix_b"));
    let before = begin(&manager);
    assert_eq!(
        ids(catalog.snapshot(&before).indexes_of(meta.id)),
        vec![clustered, created.id]
    );

    let dropper = begin(&manager);
    catalog
        .drop_index(&dropper, created.id)
        .expect("drop_index");
    assert_eq!(
        ids(catalog.snapshot(&dropper).indexes_of(meta.id)),
        vec![clustered],
        "the mark of the deferred drop hides the index from its own transaction"
    );
    let other = begin(&manager);
    assert_eq!(
        ids(catalog.snapshot(&other).indexes_of(meta.id)),
        vec![clustered],
        "and from another transaction"
    );
    manager.commit(dropper).expect("commit of the drop");

    let after = begin(&manager);
    assert_eq!(
        ids(catalog.snapshot(&after).indexes_of(meta.id)),
        vec![clustered],
        "the index is gone from storage, the key is still there"
    );
}

/// A `DROP INDEX` undone by a `ROLLBACK` puts the index back in the next snapshot.
#[test]
fn a_rolled_back_drop_index_is_in_the_snapshot_again() {
    let (catalog, manager, meta, clustered) = instance_with_a_keyed_table();
    let created = create_index(&catalog, &manager, &index_on_b(meta.id, "ix_b"));

    let dropper = begin(&manager);
    catalog
        .drop_index(&dropper, created.id)
        .expect("drop_index");
    manager.rollback(dropper).expect("rollback of the drop");

    let handle = begin(&manager);
    assert_eq!(
        ids(catalog.snapshot(&handle).indexes_of(meta.id)),
        vec![clustered, created.id]
    );
}

/// An index follows its table: the deferred `DROP TABLE` takes the table out of the snapshot
/// and `indexes_of` has nothing left to answer under its identifier.
#[test]
fn the_indexes_of_a_dropped_table_leave_with_it() {
    let (catalog, manager, meta, _) = instance_with_a_keyed_table();
    create_index(&catalog, &manager, &index_on_b(meta.id, "ix_b"));

    let dropper = begin(&manager);
    catalog.drop_table(&dropper, meta.id).expect("drop_table");
    let snapshot = catalog.snapshot(&dropper);
    assert!(snapshot.table(meta.id).is_none(), "the table is claimed");
    assert!(snapshot.indexes_of(meta.id).is_empty());
    manager.commit(dropper).expect("commit of the drop");

    let handle = begin(&manager);
    assert!(catalog.snapshot(&handle).indexes_of(meta.id).is_empty());
}

#[test]
fn two_databases_hold_two_tables_of_the_same_name() {
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let in_d = table(&catalog, &manager, &def("d", "dbo", "t"));
    let in_master = table(&catalog, &manager, &def("master", "dbo", "t"));
    assert_ne!(in_d.id, in_master.id);

    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    assert_eq!(
        snapshot.resolve_object("d", None, "t", "dbo").map(|o| o.id),
        Some(in_d.id)
    );
    assert_eq!(
        snapshot
            .resolve_object("master", None, "t", "dbo")
            .map(|o| o.id),
        Some(in_master.id)
    );
    assert_eq!(
        snapshot.object_name(in_master.id).map(|name| name.database),
        Some("master".to_owned())
    );
}

#[test]
fn a_dropped_table_resolves_to_nothing_after_the_commit() {
    let (catalog, manager, meta) = instance_with_d_dbo_t();
    let handle = begin(&manager);
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    manager.commit(handle).expect("commit of the drop");

    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    assert!(snapshot.resolve_object("d", None, "t", "dbo").is_none());
    assert!(snapshot.table(meta.id).is_none());
    assert!(snapshot.object_name(meta.id).is_none());
    assert!(
        snapshot.database("d").is_some(),
        "the database is still there"
    );
}

#[test]
fn a_dropped_database_takes_the_resolution_of_its_tables_with_it() {
    let (catalog, manager, meta) = instance_with_d_dbo_t();
    let handle = begin(&manager);
    catalog.drop_table(&handle, meta.id).expect("drop_table");
    catalog.drop_database(&handle, "d").expect("drop_database");
    manager.commit(handle).expect("commit of the two drops");

    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    assert!(snapshot.database("d").is_none());
    assert!(snapshot.resolve_object("d", None, "t", "dbo").is_none());
    assert_eq!(
        snapshot.database("master").map(|db| db.name.clone()),
        Some("master".to_owned()),
        "the system databases are still resolved"
    );
}

#[test]
fn a_name_outside_cp1252_is_not_case_folded() {
    // `Collation::compare` of `vauban_types` indexes its weights by CP1252 byte, so the case
    // of `š` is folded and the case of `ф` is not. Both pairs are one name in SQL Server:
    // `CREATE TABLE dbo.[š] (a int);` then `CREATE TABLE dbo.[Š] (a int);` is 2714 state 6
    // and `OBJECT_ID` of the two spellings is one number; the pair `ф` / `Ф` answers the
    // same 2714 and one number. The state below is the one of this build; the fold lives in
    // `vauban_types`.
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let inside = table(&catalog, &manager, &def("d", "dbo", "t_š"));
    let outside = table(&catalog, &manager, &def("d", "dbo", "t_ф"));

    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    assert_eq!(
        snapshot
            .resolve_object("d", None, "T_Š", "dbo")
            .map(|object| object.id),
        Some(inside.id),
        "`Š` is a character of CP1252: its case is folded"
    );
    assert_eq!(
        snapshot
            .resolve_object("d", None, "t_ф", "dbo")
            .map(|object| object.id),
        Some(outside.id),
        "the spelling of the creation resolves"
    );
    assert!(
        snapshot.resolve_object("d", None, "t_Ф", "dbo").is_none(),
        "current state: `Ф` does not resolve `ф`, where SQL Server answers one object"
    );
}

#[test]
fn two_tables_the_collation_reads_as_one_name_resolve_to_the_smallest_id() {
    // `table.rs` compares a new name with `eq_ignore_ascii_case` and resolution compares
    // under the collation, so a pair the collation reads as one name sits twice here. In
    // SQL Server the second create of such a pair is refused: `CREATE TABLE dbo.[tb] (a int);`
    // then `CREATE TABLE dbo.[tb  ] (a int);` is 2714 state 6, and `OBJECT_ID(N'dbo.tb')`
    // and `OBJECT_ID(N'dbo.[tb  ]')` are one number. The state below is the one of this
    // build.
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let padded = table(&catalog, &manager, &def("d", "dbo", "t_tb  "));
    let bare = table(&catalog, &manager, &def("d", "dbo", "t_tb"));
    assert!(
        padded.id < bare.id,
        "the two creations went through and the counter moved forward"
    );

    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    let resolved = snapshot
        .resolve_object("d", None, "t_tb", "dbo")
        .expect("the written name matches both entries");
    assert_eq!(
        resolved.id, padded.id,
        "the object of the smallest identifier is answered, the one created first"
    );
    assert_eq!(
        snapshot.object_name(resolved.id).map(|name| name.name),
        Some("t_tb  ".to_owned()),
        "current state: the name given back is not the name that was written"
    );

    // Same shape on a case the collation folds and `table.rs` keeps apart.
    let upper = table(&catalog, &manager, &def("d", "dbo", "Été"));
    let lower = table(&catalog, &manager, &def("d", "dbo", "été"));
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    assert!(upper.id < lower.id);
    assert_eq!(
        snapshot
            .resolve_object("d", None, "été", "dbo")
            .map(|object| object.id),
        Some(upper.id),
        "current state: the two entries exist and the smallest identifier wins"
    );
    assert_eq!(
        snapshot.object_name(upper.id).map(|name| name.name),
        Some("Été".to_owned())
    );
}

/// The three-part name `master.sys.<name>`, the name a system view is given back under.
fn in_master_sys(name: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "sys".to_owned(),
        name: name.to_owned(),
    }
}

/// The identifier of the system view `sys.<name>` read from the database `db`.
fn system_view_id(snapshot: &vauban_catalog::CatalogSnapshot, db: &str, name: &str) -> ObjectId {
    snapshot
        .resolve_object(db, Some("sys"), name, "dbo")
        .unwrap_or_else(|| panic!("sys.{name} resolves from {db}"))
        .id
}

#[test]
fn sys_tables_resolves_to_a_view_with_its_definition() {
    let (catalog, manager) = instance();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let view = snapshot
        .resolve_object("master", Some("sys"), "tables", "dbo")
        .expect("sys.tables resolves on a bootstrapped catalogue");
    assert_eq!(view.kind, ObjectKind::View);
    assert!(view.id.0 < 0, "the identifier is negative: {}", view.id);
    assert_eq!(view.parent, None);
    assert!(
        snapshot.table(view.id).is_none(),
        "a system view is not given back by `table`"
    );
    assert_eq!(snapshot.object_name(view.id), Some(in_master_sys("tables")));
    assert!(snapshot.indexes_of(view.id).is_empty());

    // The text of `views/sys_tables.rs`: a `SELECT` over the internal table of the objects, filtered on
    // the current database and on the type of a user table.
    let definition = snapshot
        .view_definition(view.id)
        .expect("the view carries its text");
    assert!(definition.starts_with("SELECT "), "{definition}");
    assert!(
        definition.contains("\n  FROM master.dbo.vauban_sys_objects"),
        "{definition}"
    );
    assert!(
        definition.contains("\n WHERE database_id = DB_ID()"),
        "{definition}"
    );
    assert!(definition.contains("type = 'U '"), "{definition}");
    assert_eq!(
        definition,
        view.definition
            .as_deref()
            .expect("the same text on the meta")
    );

    // The filter on the type is what tells `sys.tables` from `sys.objects`, which reads the
    // same internal table: the two texts are two.
    let objects = snapshot
        .resolve_object("master", Some("sys"), "objects", "dbo")
        .expect("sys.objects resolves");
    let other = snapshot
        .view_definition(objects.id)
        .expect("sys.objects carries its text");
    assert_ne!(definition, other);
    assert!(!other.contains("type = 'U '"), "{other}");
}

#[test]
fn information_schema_tables_resolves() {
    // The view `views/info_schema.rs` describes, read through the same lookup as
    // `sys.tables` on the other schema of the system views. In SQL Server,
    // `OBJECT_ID('INFORMATION_SCHEMA.TABLES')` and `OBJECT_ID('information_schema.tables')`
    // are one number, the same read from a user database and from `master`.
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let view = snapshot
        .resolve_object("master", Some("INFORMATION_SCHEMA"), "TABLES", "dbo")
        .expect("INFORMATION_SCHEMA.TABLES resolves on a bootstrapped catalogue");
    assert_eq!(view.kind, ObjectKind::View);
    assert!(view.id.0 < 0, "the identifier is negative: {}", view.id);
    assert!(snapshot.table(view.id).is_none());
    assert_eq!(
        snapshot.object_name(view.id),
        Some(QualifiedName {
            database: "master".to_owned(),
            schema: "INFORMATION_SCHEMA".to_owned(),
            name: "TABLES".to_owned(),
        })
    );
    // The text of `views/info_schema.rs`: a `SELECT` over its own internal table,
    // `vauban_is_tables`.
    let definition = snapshot
        .view_definition(view.id)
        .expect("the view carries its text");
    assert!(
        definition.starts_with("SELECT DB_NAME() AS TABLE_CATALOG"),
        "{definition}"
    );
    assert!(
        definition.contains("\n  FROM master.dbo.vauban_is_tables"),
        "{definition}"
    );
    assert!(
        definition.contains("\n WHERE database_id = DB_ID()"),
        "{definition}"
    );
    // Lower case, from a user database: one view, one identifier.
    assert_eq!(
        snapshot
            .resolve_object("d", Some("information_schema"), "tables", "dbo")
            .map(|object| object.id),
        Some(view.id)
    );
    // The schema tells it apart from `sys.tables`, which has its own text.
    let in_sys = system_view_id(&snapshot, "master", "tables");
    assert_ne!(in_sys, view.id);
    assert_ne!(snapshot.view_definition(in_sys), Some(definition));
}

#[test]
fn a_system_view_is_visible_from_a_user_database() {
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    // In SQL Server, from a user database `d`, `OBJECT_ID('sys.objects')` and
    // `OBJECT_ID('master.sys.objects')` are one number; read from `master`,
    // `OBJECT_ID('d.sys.objects')` is that number too. One view, one identifier, whichever
    // database names it.
    let from_user = system_view_id(&snapshot, "d", "objects");
    let from_master = system_view_id(&snapshot, "master", "objects");
    assert_eq!(from_user, from_master);
    assert_eq!(
        snapshot.object_name(from_user),
        Some(in_master_sys("objects")),
        "the first part given back is `master`, the database the entry is named in"
    );
    assert!(from_user.0 < 0, "{from_user}");
    assert!(snapshot.table(from_user).is_none());
    assert!(snapshot.view_definition(from_user).is_some());

    // The case of the two written parts is folded, as `OBJECT_ID('SYS.TABLES')` resolves in
    // SQL Server.
    assert_eq!(
        snapshot
            .resolve_object("d", Some("SYS"), "OBJECTS", "dbo")
            .map(|object| object.id),
        Some(from_user)
    );
    // A first part that names no database resolves nothing, view or not.
    assert!(
        snapshot
            .resolve_object("nosuchdb", Some("sys"), "objects", "dbo")
            .is_none()
    );
    // An unqualified name is looked up in the default schema of the session, `dbo` here, so
    // it finds no view: in SQL Server, `OBJECT_ID('objects')` is NULL and
    // `SELECT COUNT(*) FROM objects` is error 208.
    assert!(
        snapshot
            .resolve_object("d", None, "objects", "dbo")
            .is_none()
    );
    assert!(
        snapshot
            .resolve_object("d", Some("sys"), "nosuchview", "dbo")
            .is_none()
    );
}

#[test]
fn system_view_ids_are_the_negative_ones_of_sys_all_objects() {
    // The identifiers the rows of `vauban_sys_all_objects` publish, read here through the
    // public API: the comparison row by row with that table needs `views::internal_tables()`,
    // which is `pub(crate)`, so it is the unit test `system_view_ids_match_sys_all_objects` of
    // `src/snapshot.rs`. What this test checks is what a client sees: negative identifiers,
    // the ranks the two sides agree on, and the same answer from two snapshots. The
    // identifiers of SQL Server are negative and not contiguous, so the rule shares the sign
    // with it and nothing more.
    let (catalog, manager) = instance();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    // `views/sys_tables.rs` is read first, then `views/sys_core.rs`, and the two views of the
    // tables of the bootstrap sit at -16 and -17, before the seven of `views/sys_extra.rs`.
    let ranked: Vec<(ObjectId, &str)> = [
        "objects",
        "tables",
        "columns",
        "types",
        "databases",
        "schemas",
        "all_objects",
        "master_files",
    ]
    .into_iter()
    .map(|name| (system_view_id(&snapshot, "master", name), name))
    .collect();
    assert_eq!(
        ranked,
        vec![
            (ObjectId(-1), "objects"),
            (ObjectId(-2), "tables"),
            (ObjectId(-3), "columns"),
            (ObjectId(-4), "types"),
            (ObjectId(-16), "databases"),
            (ObjectId(-17), "schemas"),
            (ObjectId(-18), "all_objects"),
            (ObjectId(-24), "master_files"),
        ]
    );
    // Two snapshots of two transactions give the same identifiers: the rank is read from the
    // code, not from a counter.
    let other = catalog.snapshot(&begin(&manager));
    for (id, name) in ranked {
        assert_eq!(system_view_id(&other, "master", name), id);
        assert!(id.0 < 0, "{name} has a negative identifier: {id}");
    }
}

#[test]
fn a_user_table_named_like_a_system_view_does_not_shadow_it() {
    // In SQL Server, after `CREATE TABLE dbo.[tables] (a int);`, `OBJECT_ID('sys.tables')`
    // keeps its number while `OBJECT_ID('dbo.tables')` and `OBJECT_ID('tables')` give the
    // new table.
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let user = table(&catalog, &manager, &def("d", "dbo", "tables"));
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let view = system_view_id(&snapshot, "d", "tables");
    assert!(view.0 < 0, "{view}");
    assert_ne!(view, user.id);
    assert!(snapshot.view_definition(view).is_some());
    assert!(snapshot.table(view).is_none());

    let resolved = snapshot
        .resolve_object("d", Some("dbo"), "tables", "dbo")
        .expect("the user table resolves under its own schema");
    assert_eq!(resolved.id, user.id);
    assert_eq!(resolved.kind, ObjectKind::Table);
    assert_eq!(
        snapshot.table(user.id).map(|meta| meta.columns.len()),
        Some(1),
        "the user table keeps its columns"
    );
    assert!(snapshot.view_definition(user.id).is_none());
    // Unqualified, under the default schema `dbo`: the table, as in SQL Server.
    assert_eq!(
        snapshot
            .resolve_object("d", None, "tables", "dbo")
            .map(|object| object.id),
        Some(user.id)
    );
    // The same name created in `master`, where the views are named, is still another object.
    let in_master = table(&catalog, &manager, &def("master", "dbo", "tables"));
    let snapshot = catalog.snapshot(&begin(&manager));
    assert_eq!(system_view_id(&snapshot, "master", "tables"), view);
    assert_eq!(
        snapshot
            .resolve_object("master", Some("dbo"), "tables", "dbo")
            .map(|object| object.id),
        Some(in_master.id)
    );
}

// # The internal tables of `master`
//
// What a client sees of the internal tables the bootstrap created: they resolve as tables of
// `master.dbo`, carry the columns of their description and the `storage_id` of their table, and
// stay out of the other databases, out of `sys.objects` and out of a `DROP TABLE`. The rules
// are in `src/snapshot.rs`, section "The internal tables of `master`".
//
// **The end-to-end vector is left out of this file.** `SELECT name FROM sys.databases` goes
// through `session`, `binder`, `planner` and `executor`, which this crate does not depend on (a
// dependency the other way round), so there is no short path to `run_batch` here. What this
// file checks of that path is its last link: the plan the binder expands names
// `master.dbo.vauban_sys_objects`-like tables, and those names resolve to a table whose
// `storage_id` holds the rows (`vauban_sys_databases_resolves_to_a_table_with_its_columns`).

#[test]
fn vauban_sys_databases_resolves_to_a_table_with_its_columns() {
    let (catalog, manager, storage) = instance_with_its_storage();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let object = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_databases", "dbo")
        .expect("master.dbo.vauban_sys_databases resolves on a bootstrapped catalogue");
    assert_eq!(object.kind, ObjectKind::Table);
    assert_eq!(object.parent, None);
    assert_eq!(object.definition, None);
    assert!(snapshot.view_definition(object.id).is_none());
    assert_eq!(
        snapshot.object_name(object.id),
        Some(QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "vauban_sys_databases".to_owned(),
        })
    );
    // Two parts under a session on `master`, and the name alone under the default schema
    // `dbo`: the same object.
    assert_eq!(
        snapshot
            .resolve_object("master", None, "vauban_sys_databases", "dbo")
            .map(|object| object.id),
        Some(object.id)
    );
    assert_eq!(
        snapshot
            .resolve_object("MASTER", Some("DBO"), "VAUBAN_SYS_DATABASES", "dbo")
            .map(|object| object.id),
        Some(object.id),
        "the three parts are folded like those of a user table"
    );

    // The columns the bootstrap declared for the table: the name, the type, the nullability,
    // the identifier counted from 1 and the ordinal counted from 0.
    let meta = snapshot
        .table(object.id)
        .expect("an internal table is a table of the snapshot");
    assert_eq!(meta.id, object.id);
    assert_eq!(meta.schema, "dbo");
    assert_eq!(meta.name, "vauban_sys_databases");
    let columns: Vec<(i32, &str, u16, TypeInfo)> = meta
        .columns
        .iter()
        .map(|column| {
            (
                column.id.0,
                column.name.as_str(),
                column.ordinal,
                column.ty.clone(),
            )
        })
        .collect();
    let sysname = |nullable| TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), nullable);
    assert_eq!(
        columns,
        vec![
            (1, "database_id", 0, TypeInfo::new(SqlType::Int, false)),
            (2, "name", 1, sysname(false)),
            (3, "collation_name", 2, sysname(true)),
            // The two versioning options of the internal table, with the types of the
            // columns of `sys.databases` that publish them.
            (
                4,
                "is_read_committed_snapshot_on",
                3,
                TypeInfo::new(SqlType::Bit, false),
            ),
            (
                5,
                "snapshot_isolation_state",
                4,
                TypeInfo::new(SqlType::TinyInt, false),
            ),
        ]
    );
    assert_eq!(meta.clustered, None);
    assert!(meta.constraints.is_empty());
    assert!(snapshot.indexes_of(object.id).is_empty());

    // The `storage_id` is the table the bootstrap wrote the rows of the databases into: read
    // through it, the four system databases come back. That is what the unit test
    // `the_storage_id_of_an_internal_table_is_the_one_internal_table_id_finds` states against
    // `bootstrap::internal_table_id`, checked here from the outside.
    let read = rows(&storage, &manager, meta.storage_id);
    let mut names: Vec<String> = read
        .iter()
        .filter_map(|row| match row.get(1) {
            Some(Value::String(name)) => Some(name.text.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["master", "model", "msdb", "tempdb"]);
    assert_eq!(read.len(), 4);
    // Counter-proof that the rows come from this table and not from any table of `master`: the
    // table of the schemas, resolved the same way, holds the twelve (database, schema) pairs.
    let schemas = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_schemas", "dbo")
        .and_then(|object| snapshot.table(object.id))
        .expect("master.dbo.vauban_sys_schemas resolves to a table");
    assert_eq!(rows(&storage, &manager, schemas.storage_id).len(), 12);
    assert_ne!(schemas.storage_id, meta.storage_id);
}

#[test]
fn every_described_internal_table_resolves() {
    // The twenty tables of `INTERNAL_TABLES`, each resolved from `master`: twenty tables,
    // twenty identifiers, numbered from 100_000 by the position of the description
    // (`src/snapshot.rs`). A user table of the same catalogue carries none of those
    // identifiers.
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let user_in_d = table(&catalog, &manager, &def("d", "dbo", "t"));
    let user_in_master = table(&catalog, &manager, &def("master", "dbo", "u"));
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let mut resolved: Vec<(ObjectId, &str)> = Vec::new();
    for name in INTERNAL_TABLES {
        let object = snapshot
            .resolve_object("master", Some("dbo"), name, "dbo")
            .unwrap_or_else(|| panic!("master.dbo.{name} resolves"));
        assert_eq!(object.kind, ObjectKind::Table, "{name}");
        let meta = snapshot
            .table(object.id)
            .unwrap_or_else(|| panic!("{name} is a table of the snapshot"));
        assert!(!meta.columns.is_empty(), "{name} carries its columns");
        assert_eq!(meta.clustered, None, "{name}");
        assert!(snapshot.indexes_of(object.id).is_empty(), "{name}");
        resolved.push((object.id, name));
    }
    let numbered: Vec<(ObjectId, &str)> = INTERNAL_TABLES
        .into_iter()
        .enumerate()
        .map(|(position, name)| {
            let rank = i32::try_from(position).expect("a position fits in an i32");
            (ObjectId(100_000 + rank), name)
        })
        .collect();
    assert_eq!(resolved, numbered, "numbered by the position of the table");
    // Distinct from one another and from the two user tables: twenty-two identifiers for
    // twenty-two tables.
    let mut ids: Vec<i32> = resolved.iter().map(|&(id, _)| id.0).collect();
    ids.extend([user_in_d.id.0, user_in_master.id.0]);
    let mut once = ids.clone();
    once.sort_unstable();
    once.dedup();
    assert_eq!(once.len(), 22, "{ids:?}");
    assert!(
        ids.iter().take(20).all(|&id| id < user_in_master.id.0),
        "{ids:?}"
    );
}

#[test]
fn an_internal_table_is_not_visible_from_a_user_database() {
    // An internal table lives in `master` and is named there: the three other system
    // databases and a user database resolve it to nothing, unlike a system view of `views/`,
    // which the snapshot looks up before the objects of the database
    // (`a_system_view_is_visible_from_a_user_database`).
    let (catalog, manager) = instance();
    database(&catalog, &manager, "d");
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let from_master = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_databases", "dbo")
        .expect("master.dbo.vauban_sys_databases resolves from master");
    for db in ["d", "tempdb", "model", "msdb"] {
        assert!(
            snapshot
                .resolve_object(db, Some("dbo"), "vauban_sys_databases", "dbo")
                .is_none(),
            "two parts from {db}"
        );
        assert!(
            snapshot
                .resolve_object(db, None, "vauban_sys_databases", "dbo")
                .is_none(),
            "one part from {db}, under the default schema"
        );
        assert!(
            snapshot
                .resolve_object(db, Some("sys"), "vauban_sys_databases", "dbo")
                .is_none(),
            "under the schema of the system views, from {db}"
        );
    }
    assert!(
        snapshot
            .resolve_object("nosuchdb", Some("dbo"), "vauban_sys_databases", "dbo")
            .is_none()
    );
    // The schema is compared too: the internal tables are named in `dbo`, not in `sys`.
    assert!(
        snapshot
            .resolve_object("master", Some("sys"), "vauban_sys_databases", "dbo")
            .is_none()
    );
    assert_eq!(
        snapshot
            .object_name(from_master.id)
            .map(|name| name.database),
        Some("master".to_owned())
    );
}

#[test]
fn a_user_table_and_an_internal_table_do_not_collide() {
    // `CREATE TABLE dbo.t` in `master`, where the internal tables are: the two resolve, each
    // with its own identifier, its own `storage_id` and its own columns.
    let (catalog, manager, storage) = instance_with_its_storage();
    let user = table(&catalog, &manager, &def("master", "dbo", "t"));
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);

    let internal = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_schemas", "dbo")
        .and_then(|object| snapshot.table(object.id))
        .expect("master.dbo.vauban_sys_schemas resolves to a table");
    let resolved = snapshot
        .resolve_object("master", Some("dbo"), "t", "dbo")
        .expect("master.dbo.t resolves");
    assert_eq!(resolved.id, user.id);
    assert_ne!(internal.id, user.id);
    assert_ne!(internal.storage_id, user.storage_id);
    assert_eq!(
        snapshot.table(user.id).map(|meta| meta.columns.len()),
        Some(1),
        "the user table keeps its single column"
    );
    assert_eq!(
        internal.columns.len(),
        4,
        "database_id, schema_id, name, principal_id"
    );
    // The two rows sets are two: the internal table holds the twelve pairs of the bootstrap,
    // the user table is empty.
    assert_eq!(rows(&storage, &manager, internal.storage_id).len(), 12);
    assert_eq!(rows(&storage, &manager, user.storage_id).len(), 0);

    // A user table created under the name of an internal table is a second entry matching one
    // written name, the shape `src/snapshot.rs` describes in "Bound: two entries can match
    // one written name": the smaller identifier wins, which is the internal table. The
    // refusal belongs to `table.rs`.
    let shadow = table(
        &catalog,
        &manager,
        &def("master", "dbo", "vauban_sys_schemas"),
    );
    let snapshot = catalog.snapshot(&begin(&manager));
    assert_ne!(shadow.id, internal.id);
    assert_eq!(
        snapshot
            .resolve_object("master", Some("dbo"), "vauban_sys_schemas", "dbo")
            .map(|object| object.id),
        Some(internal.id),
        "current state: the internal table answers, its identifier being the smaller"
    );
    assert!(
        snapshot.table(shadow.id).is_some(),
        "the user table is in the snapshot, reachable by its identifier"
    );
}

#[test]
fn an_internal_table_is_not_published_by_sys_objects() {
    // `views/sys_tables.rs` builds the rows of `vauban_sys_objects` from the user tables of the catalogue
    // and leaves the internal tables out (`views/sys_tables.rs`, unit test
    // `internal_tables_are_not_user_tables`). Read from the outside: the table holds no row
    // naming one of the twenty internal tables, before and after a `CREATE TABLE` in `master`.
    let (catalog, manager, storage) = instance_with_its_storage();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    let objects = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_objects", "dbo")
        .and_then(|object| snapshot.table(object.id))
        .expect("master.dbo.vauban_sys_objects resolves to a table")
        .storage_id;
    let names: Vec<Value> = INTERNAL_TABLES.into_iter().map(text).collect();
    let published = |read: Vec<Vec<Value>>| -> Vec<Value> {
        read.into_iter()
            .flatten()
            .filter(|value| names.contains(value))
            .collect()
    };
    assert_eq!(published(rows(&storage, &manager, objects)), Vec::new());
    table(&catalog, &manager, &def("master", "dbo", "t"));
    assert_eq!(published(rows(&storage, &manager, objects)), Vec::new());
    // Counter-proof that the scan is not blind: the same reading of the table of the
    // databases finds the four names the bootstrap wrote there.
    let databases = catalog
        .snapshot(&begin(&manager))
        .resolve_object("master", Some("dbo"), "vauban_sys_databases", "dbo")
        .and_then(|object| {
            catalog
                .snapshot(&begin(&manager))
                .table(object.id)
                .map(|meta| meta.storage_id)
        })
        .expect("master.dbo.vauban_sys_databases resolves to a table");
    let read = rows(&storage, &manager, databases);
    assert_eq!(read.len(), 4);
    assert!(
        read.into_iter()
            .flatten()
            .any(|value| value == text("msdb"))
    );
}

#[test]
fn an_internal_table_cannot_be_dropped() {
    // 3701, severity 11, state 5 — the number and the state SQL Server 2022 answers a
    // `DROP TABLE` of a system table with (`src/snapshot.rs`, section "Visible in `master`,
    // not published, not dropped"). `table.rs` gives it without a change: the identifier of
    // an internal table is not in its store.
    let (catalog, manager) = instance();
    let handle = begin(&manager);
    let snapshot = catalog.snapshot(&handle);
    let internal = snapshot
        .resolve_object("master", Some("dbo"), "vauban_sys_databases", "dbo")
        .expect("master.dbo.vauban_sys_databases resolves")
        .id;

    let err = catalog
        .drop_table(&handle, internal)
        .expect_err("an internal table is not droppable");
    assert_eq!(err.number, 3701);
    assert_eq!(err.severity, 11);
    assert_eq!(err.state, 5);
    assert!(
        err.message.contains(&format!("'{internal}'")),
        "{}",
        err.message
    );
    manager.commit(handle).expect("commit");

    // The table is still there afterwards, with its rows: the refused drop registered nothing.
    let snapshot = catalog.snapshot(&begin(&manager));
    assert_eq!(
        snapshot
            .resolve_object("master", Some("dbo"), "vauban_sys_databases", "dbo")
            .map(|object| object.id),
        Some(internal)
    );
    assert!(snapshot.table(internal).is_some());
}
