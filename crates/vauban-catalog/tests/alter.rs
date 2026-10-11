//! `Catalog::alter_table` seen from outside the crate.

use std::sync::Arc;

use vauban_catalog::{
    AlterTable, Catalog, ColumnDef, ConstraintDef, IdentitySpec, ObjectId, QualifiedName, TableDef,
};
use vauban_storage::{DbId, MemoryStorage, Row, Storage, TableId};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{Len, SqlType, TypeInfo, Value};

fn instance() -> (Catalog, Arc<dyn Storage>, Arc<TransactionManager>) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("bootstrap");
    (catalog, storage, txn)
}

fn master(storage: &Arc<dyn Storage>) -> DbId {
    storage
        .databases()
        .expect("databases()")
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("master"))
        .expect("master")
        .0
}

fn begin(txn: &Arc<TransactionManager>) -> TxnHandle {
    txn.begin(IsolationLevel::ReadCommitted)
}

fn column(name: &str, ty: SqlType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
        default: None,
        identity: None,
        computed: None,
    }
}

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

fn two_int_columns(name: &str) -> TableDef {
    table(
        name,
        vec![
            column("a", SqlType::Int, false),
            column("b", SqlType::Int, false),
        ],
    )
}

fn insert_rows(storage: &Arc<dyn Storage>, txn: &TxnHandle, table: TableId, rows: &[(i32, i32)]) {
    for (a, b) in rows {
        storage
            .insert(txn.id, table, &Row(vec![Value::I32(*a), Value::I32(*b)]))
            .expect("insert");
    }
}

fn scan_values(
    storage: &Arc<dyn Storage>,
    txn: &TxnHandle,
    manager: &TransactionManager,
    table: TableId,
    width: usize,
) -> Vec<Vec<Value>> {
    let snap = manager.statement_snapshot(txn);
    storage
        .scan(&snap, table)
        .expect("scan")
        .map(|result| {
            let (_, row) = result.expect("row");
            let mut values = row.0.clone();
            values.resize(width, Value::Null);
            values
        })
        .collect()
}

fn insert_three_col_rows(
    storage: &Arc<dyn Storage>,
    txn: &TxnHandle,
    table: TableId,
    rows: &[(i32, i32, i32)],
) {
    for (a, b, c) in rows {
        storage
            .insert(
                txn.id,
                table,
                &Row(vec![Value::I32(*a), Value::I32(*b), Value::I32(*c)]),
            )
            .expect("insert");
    }
}

#[test]
fn add_column_keeps_object_id_and_rows() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t"))
        .expect("create_table");
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2), (3, 4)]);
    let object_id = meta.id;
    let before_storage = meta.storage_id;
    let altered = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(column("c", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("alter_table");
    assert_eq!(altered.id, object_id);
    assert_ne!(altered.storage_id, before_storage);
    let rows = scan_values(&storage, &handle, &txn, altered.storage_id, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::I32(1));
    assert_eq!(rows[0][1], Value::I32(2));
    assert_eq!(rows[0][2], Value::Null);
    assert_eq!(rows[1][2], Value::Null);
    txn.commit(handle).expect("commit");
}

#[test]
fn drop_column_keeps_the_other_column_ids() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    column("a", SqlType::Int, false),
                    column("b", SqlType::Int, false),
                    column("c", SqlType::Int, false),
                ],
            ),
        )
        .expect("create_table");
    let id_a = meta.columns[0].id;
    let id_c = meta.columns[2].id;
    insert_three_col_rows(&storage, &handle, meta.storage_id, &[(1, 2, 7), (3, 4, 8)]);
    let altered = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "b".to_owned(),
            },
        )
        .expect("alter_table");
    assert_eq!(altered.columns[0].id, id_a);
    assert_eq!(altered.columns[1].id, id_c);
    let rows = scan_values(&storage, &handle, &txn, altered.storage_id, 2);
    assert_eq!(rows[0][1], Value::I32(7));
    assert_eq!(rows[1][1], Value::I32(8));
    txn.commit(handle).expect("commit");
}

#[test]
fn dropped_table_is_deferred_to_commit() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t"))
        .expect("create_table");
    let old_storage = meta.storage_id;
    txn.commit(handle).expect("commit create");

    let handle = begin(&txn);
    let altered = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(column("c", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("alter_table");
    assert!(
        storage
            .tables(db)
            .expect("tables")
            .iter()
            .any(|(id, _)| *id == old_storage)
    );
    assert!(
        storage
            .tables(db)
            .expect("tables")
            .iter()
            .any(|(id, _)| *id == altered.storage_id)
    );
    txn.commit(handle).expect("commit alter");
    assert!(
        storage
            .tables(db)
            .expect("tables")
            .iter()
            .all(|(id, _)| *id != old_storage)
    );
}

#[test]
fn alter_rollback_restores_the_old_shape() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit create");

    let handle = begin(&txn);
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2)]);
    let old_storage = meta.storage_id;
    let old_shape = storage
        .tables(master(&storage))
        .expect("tables")
        .into_iter()
        .find(|(id, _)| *id == old_storage)
        .expect("old table")
        .1
        .columns
        .len();
    catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(column("c", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("alter_table");
    txn.rollback(handle).expect("rollback");
    let reader = begin(&txn);
    let snapshot = catalog.snapshot(&reader);
    let after = snapshot.table(meta.id).expect("table");
    assert_eq!(after.storage_id, old_storage);
    assert_eq!(after.columns.len(), old_shape);
    assert!(
        storage
            .tables(master(&storage))
            .expect("tables")
            .iter()
            .all(|(id, _)| *id != old_storage || id == &old_storage)
    );
}

/// An `ALTER TABLE` not committed yet gives its own transaction the new shape and the other
/// transactions the old one, on the old storage table; two `ALTER TABLE` in one transaction
/// keep the first shape for the others. The commit hands the last shape to everyone.
#[test]
fn another_transaction_reads_the_old_shape_until_the_commit() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t"))
        .expect("create_table");
    txn.commit(handle).expect("commit create");

    let alterer = begin(&txn);
    let add = |name: &str| AlterTable::AddColumn {
        column: Box::new(column(name, SqlType::Int, true)),
        default_name: None,
    };
    catalog
        .alter_table(&alterer, meta.id, &add("c"))
        .expect("first alter_table");
    let altered = catalog
        .alter_table(&alterer, meta.id, &add("d"))
        .expect("second alter_table");
    let shape = |handle: &TxnHandle| {
        let snapshot = catalog.snapshot(handle);
        snapshot
            .table(meta.id)
            .map(|table| (table.storage_id, table.columns.len()))
    };
    let reader = begin(&txn);
    assert_eq!(shape(&reader), Some((meta.storage_id, 2)), "the old shape");
    assert_eq!(
        shape(&alterer),
        Some((altered.storage_id, 4)),
        "the new shape for the altering transaction"
    );
    txn.commit(alterer).expect("commit of the alter");
    assert_eq!(shape(&reader), Some((altered.storage_id, 4)));
    txn.commit(reader).expect("commit of the reader");
}

#[test]
fn add_not_null_without_default_follows_the_measure() {
    let (catalog, storage, txn) = instance();
    let empty = begin(&txn);
    let empty_table = catalog
        .create_table(
            &empty,
            &table("empty_t", vec![column("a", SqlType::Int, false)]),
        )
        .expect("create empty");
    catalog
        .alter_table(
            &empty,
            empty_table.id,
            &AlterTable::AddColumn {
                column: Box::new(column("b", SqlType::Int, false)),
                default_name: None,
            },
        )
        .expect("ALTER TABLE empty_t ADD b int NOT NULL;");
    txn.commit(empty).expect("commit empty");

    let handle = begin(&txn);
    let notempty = catalog
        .create_table(
            &handle,
            &table("notempty", vec![column("a", SqlType::Int, false)]),
        )
        .expect("create notempty");
    storage
        .insert(handle.id, notempty.storage_id, &Row(vec![Value::I32(1)]))
        .expect("insert");
    let err = catalog
        .alter_table(
            &handle,
            notempty.id,
            &AlterTable::AddColumn {
                column: Box::new(column("b", SqlType::Int, false)),
                default_name: None,
            },
        )
        .expect_err("ALTER TABLE notempty ADD b int NOT NULL;");
    assert_eq!(err.number, 4901);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn add_two_columns_appends_both_in_written_order() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t2col"))
        .expect("create_table");
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2)]);
    let altered = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumns {
                columns: vec![
                    (Box::new(column("c", SqlType::Int, true)), None),
                    (Box::new(column("d", SqlType::Int, true)), None),
                ],
            },
        )
        .expect("ALTER TABLE t2col ADD c int, d int;");
    assert_eq!(altered.columns.len(), 4, "both columns are appended");
    assert_eq!(altered.columns[2].name, "c");
    assert_eq!(altered.columns[3].name, "d");
    assert_ne!(
        altered.columns[2].id, altered.columns[3].id,
        "each added column gets its own id"
    );
    let rows = scan_values(&storage, &handle, &txn, altered.storage_id, 4);
    assert_eq!(
        rows[0],
        vec![Value::I32(1), Value::I32(2), Value::Null, Value::Null]
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn add_two_columns_one_needing_a_default_is_4901_and_neither_is_added() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table("t2fail", vec![column("a", SqlType::Int, false)]),
        )
        .expect("create_table");
    storage
        .insert(handle.id, meta.storage_id, &Row(vec![Value::I32(1)]))
        .expect("insert");
    let err = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumns {
                columns: vec![
                    (Box::new(column("b", SqlType::Int, true)), None),
                    (Box::new(column("c", SqlType::Int, false)), None),
                ],
            },
        )
        .expect_err("ALTER TABLE t2fail ADD b int NULL, c int NOT NULL;");
    assert_eq!(err.number, 4901);
    let snapshot = catalog.snapshot(&handle);
    let unchanged = snapshot.table(meta.id).expect("table still there");
    assert_eq!(unchanged.columns.len(), 1, "neither b nor c was added");
    txn.rollback(handle).expect("rollback");
}

#[test]
fn add_nullable_column_with_default_leaves_existing_rows_null() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("nullable_default"))
        .expect("create_table");
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2)]);
    let mut with_default = column("c", SqlType::Int, true);
    with_default.default = Some("((7))".to_owned());
    let altered = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(with_default),
                default_name: None,
            },
        )
        .expect("ALTER TABLE nullable_default ADD c int NULL DEFAULT (7);");
    let rows = scan_values(&storage, &handle, &txn, altered.storage_id, 3);
    assert_eq!(
        rows[0][2],
        Value::Null,
        "a nullable column keeps DEFAULT for future inserts, not for a row that predates it"
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn add_nullable_column_with_a_getdate_default_is_accepted() {
    // `ALTER TABLE t ADD d datetime DEFAULT GETDATE()`: a default that is not a literal is
    // stored as the text `(getdate())`, the existing row keeps `NULL`, and the column holds
    // that text.
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("getdate_default"))
        .expect("create_table");
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2)]);
    let mut with_default = column("d", SqlType::DateTime, true);
    with_default.default = Some("(getdate())".to_owned());
    let altered = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(with_default),
                default_name: None,
            },
        )
        .expect("ALTER TABLE getdate_default ADD d datetime DEFAULT GETDATE();");
    let rows = scan_values(&storage, &handle, &txn, altered.storage_id, 3);
    assert_eq!(rows[0][2], Value::Null);
    let stored = altered.columns[2].default.as_ref().map(ToString::to_string);
    assert_eq!(stored.as_deref(), Some("(getdate())"));
    let objects = catalog.constraints_of(meta.id).expect("constraints_of");
    assert_eq!(objects.len(), 1, "the DEFAULT is an object of its own");
    txn.commit(handle).expect("commit");
}

#[test]
fn add_not_null_column_with_default_still_fills_existing_rows() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("not_null_default"))
        .expect("create_table");
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2)]);
    let mut with_default = column("c", SqlType::Int, false);
    with_default.default = Some("((7))".to_owned());
    let mut filled = Vec::new();
    let altered = catalog
        .alter_table_filling(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(with_default.clone()),
                default_name: None,
            },
            &mut |column| {
                filled.push(column.name.clone());
                Ok(Value::I32(7))
            },
        )
        .expect("ALTER TABLE not_null_default ADD c int NOT NULL DEFAULT (7);");
    assert_eq!(filled, vec!["c".to_owned()], "one call per existing row");
    let rows = scan_values(&storage, &handle, &txn, altered.storage_id, 3);
    assert_eq!(
        rows[0][2],
        Value::I32(7),
        "a NOT NULL column has no row left NULL to leave alone, so it keeps filling"
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn add_not_null_column_with_default_without_a_fill_is_an_internal_error() {
    // The catalogue evaluates no expression: a value for the existing rows comes from the
    // caller of `alter_table_filling`, and plain `alter_table` has none to write.
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("not_null_no_fill"))
        .expect("create_table");
    insert_rows(&storage, &handle, meta.storage_id, &[(1, 2)]);
    let mut with_default = column("c", SqlType::Int, false);
    with_default.default = Some("((7))".to_owned());
    let err = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(with_default),
                default_name: None,
            },
        )
        .expect_err("no value for the existing row");
    assert_eq!(err.number, 50000);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn drop_column_used_by_an_index_is_refused() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("ix_t"))
        .expect("create_table");
    catalog
        .create_index(
            &handle,
            &vauban_catalog::IndexDef {
                table: meta.id,
                name: "ix".to_owned(),
                columns: vec![vauban_catalog::SortedColumn {
                    column: "b".to_owned(),
                    descending: false,
                }],
                unique: false,
                clustered: false,
            },
        )
        .expect("create_index");
    let err = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "b".to_owned(),
            },
        )
        .expect_err("ALTER TABLE ix_t DROP COLUMN b;");
    assert_eq!(err.number, 5074);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn drop_last_column_then_add_uses_high_water() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t"))
        .expect("create_table");
    let without_b = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "b".to_owned(),
            },
        )
        .expect("drop b");
    assert_eq!(without_b.columns.len(), 1);
    let with_d = catalog
        .alter_table(
            &handle,
            without_b.id,
            &AlterTable::AddColumn {
                column: Box::new(column("d", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("add d");
    assert_eq!(with_d.columns[0].id.0, 1);
    assert_eq!(with_d.columns[1].id.0, 3);
    txn.commit(handle).expect("commit");
}

#[test]
fn drop_last_of_three_then_add_uses_high_water() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    column("a", SqlType::Int, false),
                    column("b", SqlType::Int, false),
                    column("c", SqlType::Int, false),
                ],
            ),
        )
        .expect("create_table");
    let without_c = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "c".to_owned(),
            },
        )
        .expect("drop c");
    let with_d = catalog
        .alter_table(
            &handle,
            without_c.id,
            &AlterTable::AddColumn {
                column: Box::new(column("d", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("add d");
    assert_eq!(with_d.columns[0].id.0, 1);
    assert_eq!(with_d.columns[1].id.0, 2);
    assert_eq!(with_d.columns[2].id.0, 4);
    txn.commit(handle).expect("commit");
}

/// The high water mark belongs to the table of one catalogue: a later catalogue numbers the
/// columns of its own table from its own history, whatever the first one dropped.
#[test]
fn the_high_water_of_a_closed_catalogue_does_not_number_the_next_one() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    column("a", SqlType::Int, false),
                    column("b", SqlType::Int, false),
                    column("c", SqlType::Int, false),
                ],
            ),
        )
        .expect("create_table");
    let without_c = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "c".to_owned(),
            },
        )
        .expect("drop c");
    catalog
        .alter_table(
            &handle,
            without_c.id,
            &AlterTable::AddColumn {
                column: Box::new(column("d", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("add d");
    txn.commit(handle).expect("commit");
    drop((catalog, _storage, txn));

    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &two_int_columns("t"))
        .expect("create_table");
    let without_b = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "b".to_owned(),
            },
        )
        .expect("drop b");
    let with_d = catalog
        .alter_table(
            &handle,
            without_b.id,
            &AlterTable::AddColumn {
                column: Box::new(column("d", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("add d");
    assert_eq!(with_d.columns[1].id.0, 3);
    txn.commit(handle).expect("commit");
}

#[test]
fn add_after_drop_does_not_reuse_freed_column_id() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "t",
                vec![
                    column("a", SqlType::Int, false),
                    column("b", SqlType::Int, false),
                    column("c", SqlType::Int, false),
                ],
            ),
        )
        .expect("create_table");
    let without_b = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "b".to_owned(),
            },
        )
        .expect("drop b");
    let with_d = catalog
        .alter_table(
            &handle,
            without_b.id,
            &AlterTable::AddColumn {
                column: Box::new(column("d", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("add d");
    assert_eq!(with_d.columns[0].id.0, 1);
    assert_eq!(with_d.columns[1].id.0, 3);
    assert_eq!(with_d.columns[2].id.0, 4);
    txn.commit(handle).expect("commit");
}

#[test]
fn drop_column_used_by_a_check_is_refused() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![column("a", SqlType::Int, false)]))
        .expect("create_table");
    catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck".to_owned()),
                    expr: "([a]>(0))".to_owned(),
                }),
            },
        )
        .expect("add check");
    let err = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropColumn {
                name: "a".to_owned(),
            },
        )
        .expect_err("ALTER TABLE t DROP COLUMN a;");
    assert_eq!(err.number, 5074);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn drop_unknown_constraint_is_3728() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![column("a", SqlType::Int, false)]))
        .expect("create_table");
    let err = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropConstraint {
                name: "nosuch".to_owned(),
            },
        )
        .expect_err("ALTER TABLE t DROP CONSTRAINT nosuch;");
    assert_eq!(err.number, 3728);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn add_check_constraint_becomes_an_object() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![column("a", SqlType::Int, false)]))
        .expect("create_table");
    let with_ck = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck".to_owned()),
                    expr: "([a]>(0))".to_owned(),
                }),
            },
        )
        .expect("add constraint");
    assert_eq!(with_ck.constraints.len(), 1);
    let objects = catalog.constraints_of(with_ck.id).expect("constraints_of");
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].parent, Some(with_ck.id));
    assert_eq!(objects[0].name.name, "ck");
    txn.commit(handle).expect("commit");
}

#[test]
fn add_check_does_not_duplicate_existing_default() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let mut bare = table("t_bare", vec![column("a", SqlType::Int, true)]);
    bare.columns[0].default = Some("((0))".to_owned());
    let bare_table = catalog.create_table(&handle, &bare).expect("create_table");
    assert_eq!(
        catalog
            .constraints_of(bare_table.id)
            .expect("constraints_of")
            .len(),
        1,
        "the bare column default is one object"
    );
    catalog
        .alter_table(
            &handle,
            bare_table.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck_bare".to_owned()),
                    expr: "([a]>(0))".to_owned(),
                }),
            },
        )
        .expect("add check");
    assert_eq!(
        catalog
            .constraints_of(bare_table.id)
            .expect("constraints_of")
            .len(),
        2,
        "one DEFAULT and one CHECK"
    );

    let mut named = table("t_named", vec![column("a", SqlType::Int, true)]);
    named.constraints.push(ConstraintDef::Default {
        name: Some("df_a".to_owned()),
        column: "a".to_owned(),
        expr: "((1))".to_owned(),
    });
    let named_table = catalog.create_table(&handle, &named).expect("create_table");
    assert_eq!(
        catalog
            .constraints_of(named_table.id)
            .expect("constraints_of")
            .len(),
        1,
        "the named DEFAULT is one object"
    );
    catalog
        .alter_table(
            &handle,
            named_table.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck_named".to_owned()),
                    expr: "((1))".to_owned(),
                }),
            },
        )
        .expect("add check");
    assert_eq!(
        catalog
            .constraints_of(named_table.id)
            .expect("constraints_of")
            .len(),
        2,
        "one DEFAULT and one CHECK"
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn drop_constraint_removes_the_object() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![column("a", SqlType::Int, false)]))
        .expect("create_table");
    let storage_id = meta.storage_id;
    let with_ck = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck".to_owned()),
                    expr: "((1))".to_owned(),
                }),
            },
        )
        .expect("add constraint");
    let dropped = catalog
        .alter_table(
            &handle,
            with_ck.id,
            &AlterTable::DropConstraint {
                name: "ck".to_owned(),
            },
        )
        .expect("drop constraint");
    assert_eq!(dropped.storage_id, storage_id);
    assert!(dropped.constraints.is_empty());
    // Read by another transaction once the table it sees is committed: an uncommitted
    // `CREATE TABLE` is in the snapshot of its own transaction alone.
    txn.commit(handle).expect("commit");
    let reader = begin(&txn);
    let snapshot = catalog.snapshot(&reader);
    let after = snapshot.table(dropped.id).expect("table");
    assert!(after.constraints.is_empty());
    assert!(
        catalog
            .constraints_of(dropped.id)
            .expect("constraints_of")
            .is_empty()
    );
}

#[test]
fn drop_foreign_key_unlists_it_from_the_target() {
    use vauban_catalog::SortedColumn;
    let (catalog, _storage, txn) = instance();
    let setup = begin(&txn);
    let mut parent_def = table("parent_t", vec![column("id", SqlType::Int, false)]);
    parent_def.constraints.push(ConstraintDef::PrimaryKey {
        name: None,
        columns: vec![SortedColumn {
            column: "id".to_owned(),
            descending: false,
        }],
        clustered: true,
    });
    let parent = catalog.create_table(&setup, &parent_def).expect("parent");
    let child = catalog
        .create_table(
            &setup,
            &table("child_t", vec![column("a", SqlType::Int, false)]),
        )
        .expect("child");
    txn.commit(setup).expect("commit setup");

    let handle = begin(&txn);
    catalog
        .alter_table(
            &handle,
            child.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::ForeignKey {
                    name: Some("fk_c".to_owned()),
                    columns: vec!["a".to_owned()],
                    referenced: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "parent_t".to_owned(),
                    },
                    referenced_columns: vec!["id".to_owned()],
                    on_delete: vauban_catalog::RefAction::NoAction,
                    on_update: vauban_catalog::RefAction::NoAction,
                }),
            },
        )
        .expect("add fk");
    assert_eq!(
        catalog
            .referencing_foreign_keys(parent.id)
            .expect("before drop")
            .len(),
        1
    );
    catalog
        .alter_table(
            &handle,
            child.id,
            &AlterTable::DropConstraint {
                name: "fk_c".to_owned(),
            },
        )
        .expect("drop fk");
    assert!(
        catalog
            .referencing_foreign_keys(parent.id)
            .expect("after drop")
            .is_empty()
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn constraint_changes_roll_back() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![column("a", SqlType::Int, false)]))
        .expect("create_table");
    txn.commit(handle).expect("commit create");

    let handle = begin(&txn);
    catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck".to_owned()),
                    expr: "((1))".to_owned(),
                }),
            },
        )
        .expect("add constraint");
    txn.rollback(handle).expect("rollback add");
    assert!(
        catalog
            .constraints_of(meta.id)
            .expect("constraints_of after add rollback")
            .is_empty()
    );

    let handle = begin(&txn);
    catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck".to_owned()),
                    expr: "((1))".to_owned(),
                }),
            },
        )
        .expect("add ck");
    txn.commit(handle).expect("commit ck");

    let handle = begin(&txn);
    catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::DropConstraint {
                name: "ck".to_owned(),
            },
        )
        .expect("drop constraint");
    txn.rollback(handle).expect("rollback drop");
    let objects = catalog
        .constraints_of(meta.id)
        .expect("constraints_of after drop rollback");
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].name.name, "ck");
}

#[test]
fn add_column_default_then_drop_is_5074() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let meta = catalog
        .create_table(&handle, &table("t", vec![column("a", SqlType::Int, false)]))
        .expect("create_table");
    let mut with_default = column("b", SqlType::Int, true);
    with_default.default = Some("((0))".to_owned());
    let with_b = catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(with_default),
                default_name: None,
            },
        )
        .expect("add b");
    assert_eq!(
        catalog
            .constraints_of(with_b.id)
            .expect("constraints_of")
            .len(),
        1,
        "ADD COLUMN with DEFAULT creates one default object"
    );
    let err = catalog
        .alter_table(
            &handle,
            with_b.id,
            &AlterTable::DropColumn {
                name: "b".to_owned(),
            },
        )
        .expect_err("drop b");
    assert_eq!(err.number, 5074);
    txn.rollback(handle).expect("rollback");
}

#[test]
fn identity_counter_survives_the_copy() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let mut def = table("t", vec![column("id", SqlType::Int, false)]);
    def.columns[0].identity = Some(IdentitySpec {
        seed: 10,
        increment: 2,
    });
    let meta = catalog.create_table(&handle, &def).expect("create_table");
    assert_eq!(
        catalog.next_identity(&handle, meta.id).expect("first"),
        vauban_types::Decimal {
            mantissa: 10,
            precision: 38,
            scale: 0,
        }
    );
    assert_eq!(
        catalog.next_identity(&handle, meta.id).expect("second"),
        vauban_types::Decimal {
            mantissa: 12,
            precision: 38,
            scale: 0,
        }
    );
    catalog
        .alter_table(
            &handle,
            meta.id,
            &AlterTable::AddColumn {
                column: Box::new(column("note", SqlType::NVarChar(Len::Fixed(10)), true)),
                default_name: None,
            },
        )
        .expect("alter_table");
    assert_eq!(
        catalog
            .next_identity(&handle, meta.id)
            .expect("after alter"),
        vauban_types::Decimal {
            mantissa: 14,
            precision: 38,
            scale: 0,
        }
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn drop_constraint_declared_in_create_table_removes_it() {
    use vauban_catalog::SortedColumn;
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let mut t1 = table(
        "t1",
        vec![
            column("a", SqlType::Int, false),
            column("b", SqlType::Int, false),
        ],
    );
    t1.constraints.push(ConstraintDef::PrimaryKey {
        name: Some("pk_t1".to_owned()),
        columns: vec![SortedColumn {
            column: "a".to_owned(),
            descending: false,
        }],
        clustered: true,
    });
    t1.constraints.push(ConstraintDef::Unique {
        name: Some("uq_t1".to_owned()),
        columns: vec![SortedColumn {
            column: "b".to_owned(),
            descending: false,
        }],
        clustered: false,
    });
    let t1_meta = catalog.create_table(&handle, &t1).expect("create t1");

    let mut t2 = table(
        "t2",
        vec![
            column("a", SqlType::Int, false),
            column("b", SqlType::Int, false),
        ],
    );
    t2.constraints.push(ConstraintDef::ForeignKey {
        name: Some("fk_t2".to_owned()),
        columns: vec!["b".to_owned()],
        referenced: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "t1".to_owned(),
        },
        referenced_columns: vec!["a".to_owned()],
        on_delete: vauban_catalog::RefAction::NoAction,
        on_update: vauban_catalog::RefAction::NoAction,
    });
    t2.constraints.push(ConstraintDef::Check {
        name: Some("ck_t2".to_owned()),
        expr: "([b]>(0))".to_owned(),
    });
    let t2_meta = catalog.create_table(&handle, &t2).expect("create t2");
    txn.commit(handle).expect("commit create");
    let handle = begin(&txn);
    assert_eq!(catalog.constraints_of(t2_meta.id).expect("before").len(), 2);

    let after_ck = catalog
        .alter_table(
            &handle,
            t2_meta.id,
            &AlterTable::DropConstraint {
                name: "ck_t2".to_owned(),
            },
        )
        .expect("ALTER TABLE t2 DROP CONSTRAINT ck_t2;");
    assert_eq!(after_ck.constraints.len(), 1);

    let after_fk = catalog
        .alter_table(
            &handle,
            t2_meta.id,
            &AlterTable::DropConstraint {
                name: "fk_t2".to_owned(),
            },
        )
        .expect("ALTER TABLE t2 DROP CONSTRAINT fk_t2;");
    assert!(after_fk.constraints.is_empty());
    assert!(
        catalog
            .referencing_foreign_keys(t1_meta.id)
            .expect("referencing_foreign_keys")
            .is_empty()
    );
    txn.commit(handle).expect("commit");
}

#[test]
fn drop_constraint_rollback_restores_a_create_table_foreign_key() {
    use vauban_catalog::SortedColumn;
    let (catalog, _storage, txn) = instance();
    let setup = begin(&txn);
    let mut t1 = table("t1", vec![column("a", SqlType::Int, false)]);
    t1.constraints.push(ConstraintDef::PrimaryKey {
        name: Some("pk_t1".to_owned()),
        columns: vec![SortedColumn {
            column: "a".to_owned(),
            descending: false,
        }],
        clustered: true,
    });
    let t1_meta = catalog.create_table(&setup, &t1).expect("create t1");

    let mut t2 = table("t2", vec![column("b", SqlType::Int, false)]);
    t2.constraints.push(ConstraintDef::ForeignKey {
        name: Some("fk_t2".to_owned()),
        columns: vec!["b".to_owned()],
        referenced: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "t1".to_owned(),
        },
        referenced_columns: vec!["a".to_owned()],
        on_delete: vauban_catalog::RefAction::NoAction,
        on_update: vauban_catalog::RefAction::NoAction,
    });
    let t2_meta = catalog.create_table(&setup, &t2).expect("create t2");
    txn.commit(setup).expect("commit create");

    // BEGIN TRAN; ALTER TABLE t2 DROP CONSTRAINT fk_t2; -- the foreign key leaves the table
    // this transaction sees.
    let handle = begin(&txn);
    assert_eq!(
        catalog
            .referencing_foreign_keys(t1_meta.id)
            .expect("referencing_foreign_keys before the drop")
            .len(),
        1
    );
    let dropped = catalog
        .alter_table(
            &handle,
            t2_meta.id,
            &AlterTable::DropConstraint {
                name: "fk_t2".to_owned(),
            },
        )
        .expect("ALTER TABLE t2 DROP CONSTRAINT fk_t2;");
    assert!(dropped.constraints.is_empty());
    assert!(
        catalog
            .referencing_foreign_keys(t1_meta.id)
            .expect("referencing_foreign_keys mid-transaction")
            .is_empty(),
        "the drop is visible to its own transaction before it commits"
    );

    // ROLLBACK; -- the foreign key is back, as SQL Server leaves it.
    txn.rollback(handle).expect("rollback");
    let reader = begin(&txn);
    let snapshot = catalog.snapshot(&reader);
    let after_rollback = snapshot.table(t2_meta.id).expect("t2 after rollback");
    assert_eq!(after_rollback.constraints.len(), 1);
    assert_eq!(
        catalog
            .referencing_foreign_keys(t1_meta.id)
            .expect("referencing_foreign_keys after rollback")
            .len(),
        1,
        "a rolled back DROP CONSTRAINT leaves the foreign key on the child table"
    );
    assert_eq!(
        catalog
            .constraints_of(t2_meta.id)
            .expect("constraints_of after rollback")
            .len(),
        1
    );
    txn.commit(reader).expect("commit reader");
}

/// One key column `name`, ascending.
fn key_on(name: &str) -> Vec<vauban_catalog::SortedColumn> {
    vec![vauban_catalog::SortedColumn {
        column: name.to_owned(),
        descending: false,
    }]
}

fn primary_key(name: Option<&str>, column: &str, clustered: bool) -> ConstraintDef {
    ConstraintDef::PrimaryKey {
        name: name.map(str::to_owned),
        columns: key_on(column),
        clustered,
    }
}

fn unique(name: Option<&str>, column: &str) -> ConstraintDef {
    ConstraintDef::Unique {
        name: name.map(str::to_owned),
        columns: key_on(column),
        clustered: false,
    }
}

fn drop_constraint(name: &str) -> AlterTable {
    AlterTable::DropConstraint {
        name: name.to_owned(),
    }
}

/// The names of the indexes `catalog` holds on `table`, and how many are unique.
fn index_names(catalog: &Catalog, txn: &Arc<TransactionManager>, table: ObjectId) -> Vec<String> {
    let reader = begin(txn);
    let mut names: Vec<String> = catalog
        .snapshot(&reader)
        .indexes_of(table)
        .iter()
        .map(|index| index.name.clone())
        .collect();
    txn.commit(reader).expect("commit reader");
    names.sort();
    names
}

/// `CREATE TABLE t (a int PRIMARY KEY, b int CONSTRAINT uq UNIQUE);`, the key clustered or
/// not, then `ALTER TABLE t DROP CONSTRAINT uq;`: the `UNIQUE` goes with its index, the key
/// stays, two equal `b` go in, and a reopened catalogue holds the same.
fn drop_the_unique_beside_a_primary_key(clustered: bool) {
    let (catalog, storage, txn) = instance();
    let setup = begin(&txn);
    let mut def = two_int_columns("t");
    def.constraints.push(primary_key(None, "a", clustered));
    def.constraints.push(unique(Some("uq"), "b"));
    let created = catalog.create_table(&setup, &def).expect("create t");
    txn.commit(setup).expect("commit create");
    let pk_name = index_names(&catalog, &txn, created.id)
        .into_iter()
        .find(|name| name.starts_with("PK__"))
        .expect("the primary key");

    let handle = begin(&txn);
    let altered = catalog
        .alter_table(&handle, created.id, &drop_constraint("uq"))
        .expect("ALTER TABLE t DROP CONSTRAINT uq;");
    assert_eq!(altered.constraints.len(), 1, "the primary key alone");
    insert_rows(&storage, &handle, altered.storage_id, &[(1, 5), (2, 5)]);
    txn.commit(handle).expect("commit");
    assert_eq!(
        index_names(&catalog, &txn, created.id),
        vec![pk_name.clone()]
    );

    let reopened = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("reopen");
    assert_eq!(index_names(&reopened, &txn, created.id), vec![pk_name]);
    let reader = begin(&txn);
    let meta = reopened
        .snapshot(&reader)
        .table(created.id)
        .cloned()
        .expect("t after the reopen");
    txn.commit(reader).expect("commit reader");
    let writer = begin(&txn);
    insert_rows(&storage, &writer, meta.storage_id, &[(3, 5)]);
    txn.commit(writer)
        .expect("a third equal b after the reopen");
    let reader = begin(&txn);
    assert_eq!(
        scan_values(&storage, &reader, &txn, meta.storage_id, 2).len(),
        3
    );
    txn.commit(reader).expect("commit reader");
}

#[test]
fn dropping_a_unique_beside_a_clustered_primary_key_frees_its_values() {
    drop_the_unique_beside_a_primary_key(true);
}

#[test]
fn dropping_a_unique_on_a_heap_beside_a_primary_key_frees_its_values() {
    drop_the_unique_beside_a_primary_key(false);
}

#[test]
fn adding_a_column_keeps_a_named_primary_key() {
    let (catalog, storage, txn) = instance();
    let handle = begin(&txn);
    let mut def = two_int_columns("t");
    def.constraints.push(primary_key(Some("pk_t"), "a", true));
    def.constraints.push(unique(None, "b"));
    let created = catalog.create_table(&handle, &def).expect("create t");
    txn.commit(handle).expect("commit create");
    let before = index_names(&catalog, &txn, created.id);

    let handle = begin(&txn);
    let altered = catalog
        .alter_table(
            &handle,
            created.id,
            &AlterTable::AddColumn {
                column: Box::new(column("c", SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("ALTER TABLE t ADD c int;");
    assert_eq!(altered.columns.len(), 3);
    txn.commit(handle).expect("commit");
    assert_eq!(index_names(&catalog, &txn, created.id), before);

    let reopened = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("reopen");
    assert_eq!(index_names(&reopened, &txn, created.id), before);
    let reader = begin(&txn);
    assert_eq!(
        reopened
            .snapshot(&reader)
            .table(created.id)
            .expect("t after the reopen")
            .columns
            .len(),
        3
    );
    txn.commit(reader).expect("commit reader");
}

#[test]
fn adding_a_check_under_the_name_of_a_key_is_2714_state_5() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let mut def = two_int_columns("c1");
    def.constraints.push(primary_key(Some("pk_c1"), "a", true));
    def.constraints.push(unique(Some("uq_c1"), "b"));
    let c1 = catalog.create_table(&handle, &def).expect("create c1");
    let other = catalog
        .create_table(&handle, &two_int_columns("c2"))
        .expect("create c2");
    txn.commit(handle).expect("commit create");

    let check = |name: &str| AlterTable::AddConstraint {
        constraint: Box::new(ConstraintDef::Check {
            name: Some(name.to_owned()),
            expr: "((1))".to_owned(),
        }),
    };
    for (table, name) in [(c1.id, "pk_c1"), (c1.id, "UQ_C1"), (other.id, "pk_c1")] {
        let handle = begin(&txn);
        let err = catalog
            .alter_table(&handle, table, &check(name))
            .expect_err("a name a key of the schema carries");
        assert_eq!((err.number, err.state), (2714, 5), "{name}");
        txn.rollback(handle).expect("rollback");
    }
    // The refused name left nothing behind: dropping the key still finds one constraint.
    let handle = begin(&txn);
    let altered = catalog
        .alter_table(&handle, c1.id, &drop_constraint("uq_c1"))
        .expect("ALTER TABLE c1 DROP CONSTRAINT uq_c1;");
    assert_eq!(altered.constraints.len(), 1);
    txn.commit(handle).expect("commit");
}

/// `r (id PRIMARY KEY pk_r, k UNIQUE uq_r, m)` with `CREATE UNIQUE INDEX ux_r ON r (m)`, and
/// `f (x REFERENCES r (m), y REFERENCES r (id))`.
fn referenced_table(catalog: &Catalog, txn: &Arc<TransactionManager>) -> (ObjectId, ObjectId) {
    let handle = begin(txn);
    let mut r = table(
        "r",
        vec![
            column("id", SqlType::Int, false),
            column("k", SqlType::Int, true),
            column("m", SqlType::Int, false),
        ],
    );
    r.constraints.push(primary_key(Some("pk_r"), "id", true));
    r.constraints.push(unique(Some("uq_r"), "k"));
    let r_meta = catalog.create_table(&handle, &r).expect("create r");
    catalog
        .create_index(
            &handle,
            &vauban_catalog::IndexDef {
                table: r_meta.id,
                name: "ux_r".to_owned(),
                columns: key_on("m"),
                unique: true,
                clustered: false,
            },
        )
        .expect("create ux_r");
    let mut f = table(
        "f",
        vec![
            column("x", SqlType::Int, true),
            column("y", SqlType::Int, true),
        ],
    );
    for (name, from, to) in [("fk_f", "x", "m"), ("fk_g", "y", "id")] {
        f.constraints.push(ConstraintDef::ForeignKey {
            name: Some(name.to_owned()),
            columns: vec![from.to_owned()],
            referenced: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "r".to_owned(),
            },
            referenced_columns: vec![to.to_owned()],
            on_delete: vauban_catalog::RefAction::NoAction,
            on_update: vauban_catalog::RefAction::NoAction,
        });
    }
    let f_meta = catalog.create_table(&handle, &f).expect("create f");
    txn.commit(handle).expect("commit create");
    (r_meta.id, f_meta.id)
}

/// The name of the index each `FOREIGN KEY` of `table` reads, in declaration order.
fn foreign_key_targets(
    catalog: &Catalog,
    txn: &Arc<TransactionManager>,
    table: ObjectId,
) -> Vec<String> {
    let reader = begin(txn);
    let snapshot = catalog.snapshot(&reader);
    let meta = snapshot.table(table).expect("the table");
    let targets = meta
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            vauban_catalog::ConstraintMeta::ForeignKey {
                referenced_table,
                referenced_index,
                ..
            } => snapshot
                .indexes_of(*referenced_table)
                .iter()
                .find(|index| index.id == *referenced_index)
                .map(|index| index.name.clone())
                .or(Some("<gone>".to_owned())),
            _ => None,
        })
        .collect();
    txn.commit(reader).expect("commit reader");
    targets
}

#[test]
fn a_foreign_key_keeps_its_unique_index_across_a_dropped_constraint() {
    let (catalog, storage, txn) = instance();
    let (r, f) = referenced_table(&catalog, &txn);
    assert_eq!(foreign_key_targets(&catalog, &txn, f), vec!["ux_r", "pk_r"]);

    let handle = begin(&txn);
    catalog
        .alter_table(&handle, r, &drop_constraint("uq_r"))
        .expect("ALTER TABLE r DROP CONSTRAINT uq_r;");
    txn.commit(handle).expect("commit");
    assert_eq!(index_names(&catalog, &txn, r), vec!["pk_r", "ux_r"]);
    assert_eq!(foreign_key_targets(&catalog, &txn, f), vec!["ux_r", "pk_r"]);

    let reopened = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("reopen");
    assert_eq!(index_names(&reopened, &txn, r), vec!["pk_r", "ux_r"]);
    assert_eq!(
        foreign_key_targets(&reopened, &txn, f),
        vec!["ux_r", "pk_r"]
    );
}

#[test]
fn dropping_a_unique_constraint_a_foreign_key_reads_is_3725() {
    let (catalog, _storage, txn) = instance();
    let handle = begin(&txn);
    let mut r2 = two_int_columns("r2");
    r2.constraints.push(primary_key(None, "a", true));
    r2.constraints.push(unique(Some("uq_r2"), "b"));
    catalog.create_table(&handle, &r2).expect("create r2");
    let mut f2 = table("f2", vec![column("x", SqlType::Int, true)]);
    f2.constraints.push(ConstraintDef::ForeignKey {
        name: Some("fk_f2".to_owned()),
        columns: vec!["x".to_owned()],
        referenced: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "r2".to_owned(),
        },
        referenced_columns: vec!["b".to_owned()],
        on_delete: vauban_catalog::RefAction::NoAction,
        on_update: vauban_catalog::RefAction::NoAction,
    });
    catalog.create_table(&handle, &f2).expect("create f2");
    txn.commit(handle).expect("commit create");
    let r2_id = {
        let reader = begin(&txn);
        let id = catalog
            .snapshot(&reader)
            .resolve_object("master", Some("dbo"), "r2", "dbo")
            .expect("r2")
            .id;
        txn.commit(reader).expect("commit reader");
        id
    };

    let handle = begin(&txn);
    let err = catalog
        .alter_table(&handle, r2_id, &drop_constraint("uq_r2"))
        .expect_err("fk_f2 reads uq_r2");
    assert_eq!((err.number, err.severity, err.state), (3725, 16, 0));
    assert!(err.message.contains("'uq_r2'") && err.message.contains("'fk_f2'"));
    txn.rollback(handle).expect("rollback");
    assert_eq!(index_names(&catalog, &txn, r2_id).len(), 2);
}

#[test]
fn dropping_a_primary_key_a_foreign_key_reads_is_3725() {
    let (catalog, _storage, txn) = instance();
    let (r, _f) = referenced_table(&catalog, &txn);
    let handle = begin(&txn);
    let err = catalog
        .alter_table(&handle, r, &drop_constraint("pk_r"))
        .expect_err("fk_g reads pk_r");
    assert_eq!((err.number, err.state), (3725, 0));
    assert!(err.message.contains("'fk_g'"));
    txn.rollback(handle).expect("rollback");
}

/// `x (a PRIMARY KEY pk_x, b UNIQUE uq_x, c, d UNIQUE uq_d, e)` with
/// `CREATE INDEX ix_x ON x (c)`, and `y (p REFERENCES x (b), q REFERENCES x (a))`.
fn table_with_every_index(
    catalog: &Catalog,
    txn: &Arc<TransactionManager>,
) -> (ObjectId, ObjectId) {
    let handle = begin(txn);
    let mut x = table(
        "x",
        ["a", "b", "c", "d", "e"]
            .iter()
            .map(|name| column(name, SqlType::Int, *name != "a"))
            .collect(),
    );
    x.constraints.push(primary_key(Some("pk_x"), "a", true));
    x.constraints.push(unique(Some("uq_x"), "b"));
    x.constraints.push(unique(Some("uq_d"), "d"));
    let x_meta = catalog.create_table(&handle, &x).expect("create x");
    catalog
        .create_index(
            &handle,
            &vauban_catalog::IndexDef {
                table: x_meta.id,
                name: "ix_x".to_owned(),
                columns: key_on("c"),
                unique: false,
                clustered: false,
            },
        )
        .expect("create ix_x");
    let mut y = table(
        "y",
        vec![
            column("p", SqlType::Int, true),
            column("q", SqlType::Int, true),
        ],
    );
    for (name, from, to) in [("fk_yb", "p", "b"), ("fk_ya", "q", "a")] {
        y.constraints.push(ConstraintDef::ForeignKey {
            name: Some(name.to_owned()),
            columns: vec![from.to_owned()],
            referenced: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "x".to_owned(),
            },
            referenced_columns: vec![to.to_owned()],
            on_delete: vauban_catalog::RefAction::NoAction,
            on_update: vauban_catalog::RefAction::NoAction,
        });
    }
    let y_meta = catalog.create_table(&handle, &y).expect("create y");
    txn.commit(handle).expect("commit create");
    (x_meta.id, y_meta.id)
}

/// What a reader sees of `x` and `y`: the column count of `x`, the indexes of `x`, the
/// index each foreign key of `y` reads, and whether `x` still refuses a second `a` and a
/// second `b` through its storage.
fn state_of(
    catalog: &Catalog,
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    x: ObjectId,
    y: ObjectId,
) -> (usize, Vec<String>, Vec<String>, [bool; 2]) {
    let reader = begin(txn);
    let meta = catalog
        .snapshot(&reader)
        .table(x)
        .cloned()
        .expect("x is still there");
    txn.commit(reader).expect("commit reader");
    let width = meta.columns.len();
    let row = |a: i32, b: i32| {
        let mut values = vec![Value::Null; width];
        values[0] = Value::I32(a);
        values[1] = Value::I32(b);
        Row(values)
    };
    let mut refused = [false; 2];
    for (slot, (first, second)) in [(row(1, 1), row(1, 2)), (row(2, 3), row(3, 3))]
        .into_iter()
        .enumerate()
    {
        let writer = begin(txn);
        storage
            .insert(writer.id, meta.storage_id, &first)
            .expect("first row");
        refused[slot] = storage.insert(writer.id, meta.storage_id, &second).is_err();
        txn.rollback(writer).expect("rollback the probe");
    }
    (
        width,
        index_names(catalog, txn, x),
        foreign_key_targets(catalog, txn, y),
        refused,
    )
}

/// Each change the rebuild serves, undone by `ROLLBACK` and by `ROLLBACK TRANSACTION` to a
/// savepoint, leaves `x` and `y` as they were, before and after a reopen.
#[test]
fn a_rolled_back_rebuild_leaves_the_indexes_and_the_foreign_keys_as_they_were() {
    let changes = [
        AlterTable::AddColumn {
            column: Box::new(column("z", SqlType::Int, true)),
            default_name: None,
        },
        drop_constraint("uq_d"),
        AlterTable::DropColumn {
            name: "e".to_owned(),
        },
    ];
    for change in &changes {
        for savepoint in [false, true] {
            let (catalog, storage, txn) = instance();
            let (x, y) = table_with_every_index(&catalog, &txn);
            let before = state_of(&catalog, &storage, &txn, x, y);
            assert_eq!(before.1, vec!["ix_x", "pk_x", "uq_d", "uq_x"]);
            assert_eq!(before.2, vec!["uq_x", "pk_x"]);
            assert_eq!(before.3, [true, true]);

            let handle = begin(&txn);
            if savepoint {
                let mark = txn.savepoint(&handle).expect("SAVE TRANSACTION s");
                catalog
                    .alter_table(&handle, x, change)
                    .unwrap_or_else(|err| panic!("{change:?}: {err:?}"));
                txn.rollback_to(&handle, mark)
                    .expect("ROLLBACK TRANSACTION s");
                assert_eq!(
                    state_of(&catalog, &storage, &txn, x, y),
                    before,
                    "{change:?} inside the transaction, after the savepoint"
                );
                txn.commit(handle).expect("commit");
            } else {
                catalog
                    .alter_table(&handle, x, change)
                    .unwrap_or_else(|err| panic!("{change:?}: {err:?}"));
                txn.rollback(handle).expect("rollback");
            }
            assert_eq!(
                state_of(&catalog, &storage, &txn, x, y),
                before,
                "{change:?}, savepoint {savepoint}"
            );
            let reopened =
                Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn)).expect("reopen");
            assert_eq!(
                state_of(&reopened, &storage, &txn, x, y),
                before,
                "{change:?}, savepoint {savepoint}, reopened"
            );
            // The table still takes a change once the first one is undone.
            let handle = begin(&txn);
            catalog
                .alter_table(&handle, x, change)
                .unwrap_or_else(|err| panic!("{change:?} again: {err:?}"));
            txn.commit(handle).expect("commit the change");
            assert_eq!(foreign_key_targets(&catalog, &txn, y), vec!["uq_x", "pk_x"]);
        }
    }
}
