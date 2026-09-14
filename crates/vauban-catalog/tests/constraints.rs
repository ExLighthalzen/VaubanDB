//! The `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints of a `CREATE TABLE`, seen from
//! outside the crate.
//!
//! These tests know the public API only: they build a [`TableDef`] by hand — turning a
//! statement into one is the business of the binder — and read the result through the
//! [`TableMeta`](vauban_catalog::TableMeta) `create_table` answers, through
//! `Catalog::constraints_of`, `Catalog::resolve_constraint` and
//! `Catalog::referencing_foreign_keys`.
//!
//! The statements these assertions key on are in the module documentation of
//! `src/constraints.rs`. The tables are created in `master`, which the bootstrap made.

use std::sync::Arc;

use vauban_catalog::{
    Catalog, ColumnDef, ConstraintDef, ConstraintMeta, ObjectId, ObjectKind, QualifiedName,
    SortedColumn, TableDef, TableMeta,
};
use vauban_parser::{Expr, Literal, RefAction, Span};
use vauban_storage::{DbId, MemoryStorage, Storage};
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{SqlType, TypeInfo};

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

/// `master.dbo.<name>` with the columns and the constraints given, each column `int`.
fn table(name: &str, columns: &[&str], constraints: Vec<ConstraintDef>) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
        },
        columns: columns
            .iter()
            .map(|column| ColumnDef {
                name: (*column).to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                default: None,
                identity: None,
                computed: None,
            })
            .collect(),
        constraints,
    }
}

/// `PRIMARY KEY` over the columns given, clustered.
fn primary_key(columns: &[&str]) -> ConstraintDef {
    ConstraintDef::PrimaryKey {
        name: None,
        columns: columns
            .iter()
            .map(|column| SortedColumn {
                column: (*column).to_owned(),
                descending: false,
            })
            .collect(),
        clustered: true,
    }
}

/// `FOREIGN KEY (columns) REFERENCES master.dbo.<referenced> (referenced_columns)`.
fn foreign_key(
    name: Option<&str>,
    columns: &[&str],
    referenced: &str,
    referenced_columns: &[&str],
) -> ConstraintDef {
    ConstraintDef::ForeignKey {
        name: name.map(str::to_owned),
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        referenced: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: referenced.to_owned(),
        },
        referenced_columns: referenced_columns
            .iter()
            .map(|column| (*column).to_owned())
            .collect(),
        on_delete: RefAction::NoAction,
        on_update: RefAction::NoAction,
    }
}

/// The integer literal `value`, the simplest expression a `CHECK` or a `DEFAULT` carries.
fn integer(value: &str) -> Expr {
    Expr::Literal(Literal::Integer(value.to_owned()), Span::EMPTY)
}

/// The parent table `<name>` with a clustered `PRIMARY KEY` on `id`, created and committed.
fn parent(catalog: &Catalog, txn: &Arc<TransactionManager>, name: &str) -> TableMeta {
    let handle = begin(txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(name, &["id", "code"], vec![primary_key(&["id"])]),
        )
        .expect("the parent table");
    txn.commit(handle).expect("commit");
    meta
}

#[test]
fn named_constraints_become_objects() {
    let (catalog, storage, txn) = instance();
    let db = master(&storage);
    parent(&catalog, &txn, "tbl_p");
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_c",
                &["id", "a", "b", "c"],
                vec![
                    ConstraintDef::Check {
                        name: Some("ck_a".to_owned()),
                        expr: integer("1"),
                    },
                    ConstraintDef::Default {
                        name: Some("df_b".to_owned()),
                        column: "b".to_owned(),
                        expr: integer("0"),
                    },
                    foreign_key(Some("fk_c"), &["c"], "tbl_p", &["id"]),
                ],
            ),
        )
        .expect("the table and its three constraints");

    let objects = catalog.constraints_of(meta.id).expect("constraints_of");
    let names: Vec<&str> = objects
        .iter()
        .map(|object| object.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["ck_a", "df_b", "fk_c"]);
    for object in &objects {
        assert_eq!(object.kind, ObjectKind::Constraint);
        assert_eq!(object.parent, Some(meta.id));
        assert_eq!(object.database, db);
        assert_eq!(object.name.schema, "dbo");
        assert_eq!(object.definition, None);
    }
    // Three identifiers, none of them the table's and none of them shared.
    let ids: Vec<ObjectId> = objects.iter().map(|object| object.id).collect();
    assert_eq!(ids.len(), 3);
    assert!(!ids.contains(&meta.id), "{ids:?} against {}", meta.id);
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 3, "{ids:?}");
    // The identifier stored on the constraint is the identifier of its object.
    let carried: Vec<ObjectId> = meta
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            ConstraintMeta::Check { constraint, .. }
            | ConstraintMeta::Default { constraint, .. }
            | ConstraintMeta::ForeignKey { constraint, .. } => Some(*constraint),
            _ => None,
        })
        .collect();
    assert_eq!(carried, ids);
    // The names resolve, without regard to ASCII case, and another name does not.
    for name in ["ck_a", "DF_B", "fk_c"] {
        let found = catalog
            .resolve_constraint(db, "dbo", name)
            .expect("resolve_constraint");
        assert_eq!(
            found.map(|object| object.parent),
            Some(Some(meta.id)),
            "{name}"
        );
    }
    assert!(
        catalog
            .resolve_constraint(db, "dbo", "ck_nope")
            .expect("resolve_constraint")
            .is_none()
    );
    assert!(
        catalog
            .constraints_of(ObjectId(1))
            .expect("constraints_of")
            .is_empty()
    );
}

#[test]
fn anonymous_constraint_name_has_the_generated_shape() {
    // SQL Server names `CREATE TABLE dbo.ab (a int NULL REFERENCES dbo.q (id),
    // verylongcolumnnamehere int NULL CHECK (verylongcolumnnamehere > 0), c int NULL
    // DEFAULT 0);` with `FK__ab__a__38996AB5`, `CK__ab__verylongcolu__398D8EEE` and
    // `DF__ab__c__3A81B327`, and a table-level `CHECK` over two columns of `multi_check`
    // with `CK__multi_check__36B12243`: the two
    // letters of the kind, two underscores, the head of the table name, the head of the
    // column when the constraint holds exactly one, two underscores, eight hexadecimal
    // digits. The digits are ours (`src/constraints.rs`), so the shape is compared and the
    // digits are not.
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    parent(&catalog, &txn, "tbl_q");
    let handle = begin(&txn);
    let mut def = table(
        "ab",
        &["a", "verylongcolumnnamehere", "c"],
        vec![
            foreign_key(None, &["a"], "tbl_q", &["id"]),
            ConstraintDef::Check {
                name: None,
                expr: integer("1"),
            },
        ],
    );
    def.columns[2].default = Some(integer("0"));
    let meta = catalog
        .create_table(&handle, &def)
        .expect("the table and its three constraints");

    let names: Vec<String> = catalog
        .constraints_of(meta.id)
        .expect("constraints_of")
        .into_iter()
        .map(|object| object.name.name)
        .collect();
    assert_eq!(names.len(), 3);
    let kinds: Vec<&str> = names.iter().map(|name| &name[..2]).collect();
    assert_eq!(kinds, vec!["FK", "CK", "DF"]);
    for name in &names {
        let groups: Vec<&str> = name.split("__").collect();
        let digits = groups.last().expect("a name ends with its digits");
        assert_eq!(digits.len(), 8, "{name}");
        assert!(
            digits
                .chars()
                .all(|digit| digit.is_ascii_hexdigit() && !digit.is_ascii_lowercase()),
            "{name}"
        );
        assert_eq!(groups[1], "ab", "{name}");
    }
    // The foreign key and the default hold one column each, so their name carries its head;
    // the `CHECK` holds none, so it has one group less.
    assert!(names[0].starts_with("FK__ab__a__"), "{}", names[0]);
    assert!(names[2].starts_with("DF__ab__c__"), "{}", names[2]);
    assert_eq!(names[0].len(), "FK__ab__a__".len() + 8, "{}", names[0]);
    assert_eq!(names[1].split("__").count(), 3, "{}", names[1]);
    assert_eq!(names[0].split("__").count(), 4, "{}", names[0]);
    // Two names of one table differ, and the name does not move between two reads.
    let again: Vec<String> = catalog
        .constraints_of(meta.id)
        .expect("constraints_of")
        .into_iter()
        .map(|object| object.name.name)
        .collect();
    assert_eq!(again, names);
    assert_ne!(names[0], names[2]);
}

#[test]
fn foreign_key_binds_the_referenced_index() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let target = parent(&catalog, &txn, "tbl_p");
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_c",
                &["id", "c"],
                vec![foreign_key(Some("fk_c"), &["c"], "tbl_p", &["id"])],
            ),
        )
        .expect("the foreign key");

    let snapshot = catalog.snapshot(&handle);
    let key = snapshot
        .indexes_of(target.id)
        .iter()
        .find(|index| index.primary_key)
        .expect("the primary key of the parent")
        .id;
    let ConstraintMeta::ForeignKey {
        columns,
        referenced_table,
        referenced_columns,
        referenced_index,
        system_named,
        ..
    } = meta
        .constraints
        .iter()
        .find(|constraint| matches!(constraint, ConstraintMeta::ForeignKey { .. }))
        .expect("the constraint is stored")
    else {
        panic!("the constraint is a foreign key");
    };
    assert_eq!(*referenced_index, key);
    assert_eq!(*referenced_table, target.id);
    assert_eq!(columns.len(), 1);
    assert_eq!(referenced_columns.len(), 1);
    assert!(!system_named, "the statement wrote the name");
}

#[test]
fn foreign_key_without_candidate_key_is_1776() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let handle = begin(&txn);
    catalog
        .create_table(&handle, &table("tbl_h1", &["id", "code"], Vec::new()))
        .expect("a table without a key");
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_h2",
                &["a"],
                vec![foreign_key(Some("fk_tbl_h2"), &["a"], "tbl_h1", &["id"])],
            ),
        )
        .expect_err("no candidate key in the referenced table");
    assert_eq!(err.number, 1776);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert!(err.message.contains("'dbo.tbl_h1'"), "{}", err.message);
    assert!(err.message.contains("'fk_tbl_h2'"), "{}", err.message);
}

#[test]
fn foreign_key_on_unknown_column_is_1769() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    parent(&catalog, &txn, "tbl_g1");
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_g2",
                &["a"],
                vec![foreign_key(
                    Some("fk_tbl_g2"),
                    &["nosuch"],
                    "tbl_g1",
                    &["id"],
                )],
            ),
        )
        .expect_err("the referencing column is not there");
    assert_eq!(err.number, 1769);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert!(err.message.contains("'fk_tbl_g2'"), "{}", err.message);
    assert!(err.message.contains("'nosuch'"), "{}", err.message);
    assert!(err.message.contains("'tbl_g2'"), "{}", err.message);
    assert!(!err.message.contains("'dbo.tbl_g2'"), "{}", err.message);
}

#[test]
fn duplicate_constraint_name_is_2714() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let handle = begin(&txn);
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_d1",
                &["a"],
                vec![ConstraintDef::Check {
                    name: Some("ck_tbl_dup".to_owned()),
                    expr: integer("1"),
                }],
            ),
        )
        .expect("the first constraint");
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_d2",
                &["b"],
                vec![ConstraintDef::Check {
                    name: Some("CK_TBL_DUP".to_owned()),
                    expr: integer("1"),
                }],
            ),
        )
        .expect_err("the name is taken in the schema");
    assert_eq!(err.number, 2714);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 5, "the state of a constraint");
    assert!(err.message.contains("'CK_TBL_DUP'"), "{}", err.message);
    // A constraint named like a table of the schema is refused the same way.
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_d3",
                &["b"],
                vec![ConstraintDef::Check {
                    name: Some("tbl_d1".to_owned()),
                    expr: integer("1"),
                }],
            ),
        )
        .expect_err("the name is that of a table");
    assert_eq!(err.number, 2714);
    assert_eq!(err.state, 5);
    // Two constraints of one name in one statement answer 8168 in SQL Server, which
    // `vauban-errors` does not carry: the catalogue names the number in an internal bug.
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_d4",
                &["a", "b"],
                vec![
                    ConstraintDef::Check {
                        name: Some("ck_twice".to_owned()),
                        expr: integer("1"),
                    },
                    ConstraintDef::Check {
                        name: Some("ck_twice".to_owned()),
                        expr: integer("2"),
                    },
                ],
            ),
        )
        .expect_err("one name twice in one statement");
    assert!(err.message.contains("8168"), "{}", err.message);
}

#[test]
fn drop_table_referenced_is_3726() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let target = parent(&catalog, &txn, "tbl_o1");
    let handle = begin(&txn);
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_o2",
                &["a"],
                vec![foreign_key(Some("fk_tbl_o2"), &["a"], "tbl_o1", &["id"])],
            ),
        )
        .expect("the child table");
    let err = catalog
        .drop_table(&handle, target.id)
        .expect_err("a foreign key points at it");
    assert_eq!(err.number, 3726);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert!(err.message.contains("'dbo.tbl_o1'"), "{}", err.message);
}

#[test]
fn drop_referencing_table_first_succeeds() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let target = parent(&catalog, &txn, "tbl_r1");
    let handle = begin(&txn);
    let child = catalog
        .create_table(
            &handle,
            &table(
                "tbl_r2",
                &["a"],
                vec![foreign_key(Some("fk_tbl_r2"), &["a"], "tbl_r1", &["id"])],
            ),
        )
        .expect("the child table");
    catalog.drop_table(&handle, child.id).expect("the child");
    catalog.drop_table(&handle, target.id).expect("the parent");
}

#[test]
fn a_self_referencing_table_is_dropped() {
    // `DROP TABLE dbo.tbl_self`, whose foreign key points at itself, is accepted
    // (`src/constraints.rs`).
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_self",
                &["id", "parent_id"],
                vec![
                    primary_key(&["id"]),
                    foreign_key(Some("fk_self"), &["parent_id"], "tbl_self", &["id"]),
                ],
            ),
        )
        .expect("a table that points at itself");
    let keys = catalog
        .referencing_foreign_keys(meta.id)
        .expect("referencing_foreign_keys");
    assert_eq!(keys.len(), 1, "its own key is in the answer");
    assert_eq!(keys[0].table, meta.id);
    catalog.drop_table(&handle, meta.id).expect("the drop");
}

#[test]
fn referencing_foreign_keys_lists_the_child() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let target = parent(&catalog, &txn, "tbl_p");
    let lonely = parent(&catalog, &txn, "tbl_lonely");
    let handle = begin(&txn);
    let child = catalog
        .create_table(
            &handle,
            &table(
                "tbl_c",
                &["a"],
                vec![foreign_key(Some("fk_c"), &["a"], "tbl_p", &["id"])],
            ),
        )
        .expect("the child table");

    let keys = catalog
        .referencing_foreign_keys(target.id)
        .expect("referencing_foreign_keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].table, child.id);
    assert_eq!(keys[0].on_delete, RefAction::NoAction);
    assert_eq!(keys[0].on_update, RefAction::NoAction);
    assert_eq!(keys[0].referenced_columns.len(), 1);
    let named = catalog
        .constraints_of(child.id)
        .expect("constraints_of")
        .into_iter()
        .find(|object| object.id == keys[0].constraint)
        .expect("the constraint object of the key");
    assert_eq!(named.name.name, "fk_c");
    assert!(
        catalog
            .referencing_foreign_keys(lonely.id)
            .expect("referencing_foreign_keys")
            .is_empty(),
        "a table no key points at"
    );
}

#[test]
fn default_constraint_carries_the_column_ordinal() {
    // `parent_column_id` is the `column_id` of the column, counted from 1: 1, 2 and 3 for
    // the three columns of `dbo.tbl_df`.
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let handle = begin(&txn);
    let mut def = table(
        "tbl_df",
        &["a", "b", "c"],
        vec![ConstraintDef::Default {
            name: Some("df_tbl_b".to_owned()),
            column: "b".to_owned(),
            expr: integer("7"),
        }],
    );
    def.columns[2].default = Some(integer("0"));
    let meta = catalog.create_table(&handle, &def).expect("two defaults");

    let defaults: Vec<(vauban_catalog::ColumnId, bool)> = meta
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            ConstraintMeta::Default {
                column,
                system_named,
                ..
            } => Some((*column, *system_named)),
            _ => None,
        })
        .collect();
    assert_eq!(
        defaults,
        vec![
            (vauban_catalog::ColumnId(2), false),
            (vauban_catalog::ColumnId(3), true),
        ],
        "the written default of `b`, then the column default of `c`"
    );
    let names: Vec<String> = catalog
        .constraints_of(meta.id)
        .expect("constraints_of")
        .into_iter()
        .map(|object| object.name.name)
        .collect();
    assert_eq!(names.len(), 2);
    assert_eq!(names[0], "df_tbl_b");
    assert!(names[1].starts_with("DF__tbl_df__c__"), "{}", names[1]);
}

#[test]
fn a_check_keeps_its_expression_and_its_text() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_ck",
                &["a"],
                vec![ConstraintDef::Check {
                    name: Some("ck_one".to_owned()),
                    expr: integer("1"),
                }],
            ),
        )
        .expect("the check");
    let ConstraintMeta::Check {
        expr, definition, ..
    } = meta.constraints.first().expect("the constraint is stored")
    else {
        panic!("the constraint is a check");
    };
    assert_eq!(*expr, integer("1"));
    // The text is the expression between parentheses; SQL Server stores `([a]>=(0))` for
    // `CHECK (a >= 0)`, a form `src/constraints.rs` documents and does not reproduce.
    assert_eq!(definition, "(1)");
}

#[test]
fn a_cross_database_foreign_key_names_1763() {
    // 1763 is not in `vauban-errors`, so the catalogue answers an internal bug naming it
    // (`src/constraints.rs`, section "Seven numbers this file does not raise").
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    parent(&catalog, &txn, "tbl_p");
    let handle = begin(&txn);
    let mut def = table(
        "tbl_cross",
        &["a"],
        vec![foreign_key(Some("fk_cross"), &["a"], "tbl_p", &["id"])],
    );
    let ConstraintDef::ForeignKey { referenced, .. } = &mut def.constraints[0] else {
        panic!("the constraint is a foreign key");
    };
    referenced.database = "tempdb".to_owned();
    let err = catalog
        .create_table(&handle, &def)
        .expect_err("another database");
    assert!(err.message.contains("1763"), "{}", err.message);
}

#[test]
fn a_foreign_key_of_the_wrong_width_names_8139() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    parent(&catalog, &txn, "tbl_i1");
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_i2",
                &["a", "b"],
                vec![foreign_key(
                    Some("fk_tbl_i2"),
                    &["a", "b"],
                    "tbl_i1",
                    &["id"],
                )],
            ),
        )
        .expect_err("two columns against one");
    assert!(err.message.contains("8139"), "{}", err.message);
}

#[test]
fn a_rolled_back_create_table_takes_its_constraints_with_it() {
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let target = parent(&catalog, &txn, "tbl_p");
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_gone",
                &["a"],
                vec![foreign_key(Some("fk_gone"), &["a"], "tbl_p", &["id"])],
            ),
        )
        .expect("the child table");
    assert_eq!(catalog.constraints_of(meta.id).expect("before").len(), 1);
    txn.rollback(handle).expect("rollback");
    assert!(
        catalog.constraints_of(meta.id).expect("after").is_empty(),
        "the table is gone from storage, and its constraints with it"
    );
    assert!(
        catalog
            .referencing_foreign_keys(target.id)
            .expect("referencing_foreign_keys")
            .is_empty()
    );
}

#[test]
fn an_implicit_reference_without_a_primary_key_names_1773() {
    // `REFERENCES t` without a column list, `t` carrying a `UNIQUE` index and no
    // `PRIMARY KEY`, then `t` carrying no key at all: SQL Server answers 1773, which
    // `vauban-errors` does not carry (`src/constraints.rs`, section "Seven numbers this
    // file does not raise").
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    let handle = begin(&txn);
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_u1",
                &["id"],
                vec![ConstraintDef::Unique {
                    name: Some("ux_tbl_u1".to_owned()),
                    columns: vec![SortedColumn {
                        column: "id".to_owned(),
                        descending: false,
                    }],
                    clustered: false,
                }],
            ),
        )
        .expect("a unique index and no primary key");
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_u2",
                &["a"],
                vec![foreign_key(Some("fk_tbl_u2"), &["a"], "tbl_u1", &[])],
            ),
        )
        .expect_err("an implicit reference to a table without a primary key");
    assert!(err.message.contains("1773"), "{}", err.message);
    assert!(!err.message.contains("1776"), "{}", err.message);
    catalog
        .create_table(&handle, &table("tbl_x1", &["id"], Vec::new()))
        .expect("a table without a key");
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_x2",
                &["a"],
                vec![foreign_key(Some("fk_tbl_x2"), &["a"], "tbl_x1", &[])],
            ),
        )
        .expect_err("an implicit reference to a table without any key");
    assert!(err.message.contains("1773"), "{}", err.message);
    // The axis that separates 1773 from the acceptance: a `PRIMARY KEY` on the referenced
    // table, which SQL Server takes with `key_index_id` 1.
    let target = parent(&catalog, &txn, "tbl_v1");
    let handle = begin(&txn);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_v2",
                &["a"],
                vec![foreign_key(Some("fk_tbl_v2"), &["a"], "tbl_v1", &[])],
            ),
        )
        .expect("an implicit reference to a primary key");
    let snapshot = catalog.snapshot(&handle);
    let key = snapshot
        .indexes_of(target.id)
        .iter()
        .find(|index| index.primary_key)
        .expect("the primary key of the parent")
        .id;
    let ConstraintMeta::ForeignKey {
        referenced_index, ..
    } = meta.constraints.first().expect("the constraint is stored")
    else {
        panic!("the constraint is a foreign key");
    };
    assert_eq!(*referenced_index, key);
}

#[test]
fn a_constraint_named_like_its_own_table_is_2714() {
    // The table of the `CREATE TABLE` being run counts as an object taken: SQL Server
    // answers 2714 state 5 for a `CHECK`, a `DEFAULT` and a `FOREIGN KEY` named like it
    // (`src/constraints.rs`, module documentation).
    let (catalog, storage, txn) = instance();
    let _ = master(&storage);
    parent(&catalog, &txn, "tbl_t3");
    let handle = begin(&txn);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_t1",
                &["a"],
                vec![ConstraintDef::Check {
                    name: Some("TBL_T1".to_owned()),
                    expr: integer("1"),
                }],
            ),
        )
        .expect_err("the check is named like the table being created");
    assert_eq!(err.number, 2714);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 5, "the state of a constraint");
    assert!(err.message.contains("'TBL_T1'"), "{}", err.message);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_t2",
                &["a"],
                vec![ConstraintDef::Default {
                    name: Some("tbl_t2".to_owned()),
                    column: "a".to_owned(),
                    expr: integer("0"),
                }],
            ),
        )
        .expect_err("the default is named like the table being created");
    assert_eq!(err.number, 2714);
    assert_eq!(err.state, 5);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_t4",
                &["a"],
                vec![foreign_key(Some("tbl_t4"), &["a"], "tbl_t3", &["id"])],
            ),
        )
        .expect_err("the foreign key is named like the table being created");
    assert_eq!(err.number, 2714);
    assert_eq!(err.state, 5);
    // The axis that separates the refusal from the acceptance: another name on the same
    // three constraints.
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_t5",
                &["a"],
                vec![ConstraintDef::Check {
                    name: Some("ck_tbl_t5".to_owned()),
                    expr: integer("1"),
                }],
            ),
        )
        .expect("a name of its own");
}
