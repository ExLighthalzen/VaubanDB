//! `Catalog::alter_table` seen from outside the crate.

use std::sync::Arc;

use vauban_catalog::{
    AlterTable, Catalog, ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, TableDef,
};
use vauban_parser::{Expr, Literal, Span};
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
            },
        )
        .expect_err("ALTER TABLE notempty ADD b int NOT NULL;");
    assert_eq!(err.number, 4901);
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
                    expr: Expr::Binary {
                        op: vauban_parser::BinaryOp::Gt,
                        op_span: Span::EMPTY,
                        left: Box::new(Expr::Column(vauban_parser::ColumnRef {
                            qualifier: None,
                            name: vauban_parser::Ident {
                                value: "a".to_owned(),
                                quoted: false,
                            },
                            span: Span::EMPTY,
                        })),
                        right: Box::new(Expr::Literal(
                            Literal::Integer("0".to_owned()),
                            Span::EMPTY,
                        )),
                        span: Span::EMPTY,
                    },
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
                    expr: Expr::Binary {
                        op: vauban_parser::BinaryOp::Gt,
                        op_span: Span::EMPTY,
                        left: Box::new(Expr::Column(vauban_parser::ColumnRef {
                            qualifier: None,
                            name: vauban_parser::Ident {
                                value: "a".to_owned(),
                                quoted: false,
                            },
                            span: Span::EMPTY,
                        })),
                        right: Box::new(Expr::Literal(
                            Literal::Integer("0".to_owned()),
                            Span::EMPTY,
                        )),
                        span: Span::EMPTY,
                    },
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
    bare.columns[0].default = Some(Expr::Literal(Literal::Integer("0".to_owned()), Span::EMPTY));
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
                    expr: Expr::Binary {
                        op: vauban_parser::BinaryOp::Gt,
                        op_span: Span::EMPTY,
                        left: Box::new(Expr::Column(vauban_parser::ColumnRef {
                            qualifier: None,
                            name: vauban_parser::Ident {
                                value: "a".to_owned(),
                                quoted: false,
                            },
                            span: Span::EMPTY,
                        })),
                        right: Box::new(Expr::Literal(
                            Literal::Integer("0".to_owned()),
                            Span::EMPTY,
                        )),
                        span: Span::EMPTY,
                    },
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
        expr: Expr::Literal(Literal::Integer("1".to_owned()), Span::EMPTY),
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
                    expr: Expr::Literal(Literal::Integer("1".to_owned()), Span::EMPTY),
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
                    expr: Expr::Literal(Literal::Integer("1".to_owned()), Span::EMPTY),
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
    txn.commit(handle).expect("commit");
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
                    on_delete: vauban_parser::RefAction::NoAction,
                    on_update: vauban_parser::RefAction::NoAction,
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
                    expr: Expr::Literal(Literal::Integer("1".to_owned()), Span::EMPTY),
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
                    expr: Expr::Literal(Literal::Integer("1".to_owned()), Span::EMPTY),
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
