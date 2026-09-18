//! Integration tests for the uniqueness checks at write time: 2627 for the index of a
//! `PRIMARY KEY` or a `UNIQUE` constraint, 2601 for a `CREATE UNIQUE INDEX`, the text of
//! the duplicate key, and the `NULL` a unique index admits.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, LockHints, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::{
    Catalog, ColumnDef, ConstraintDef, IndexDef, QualifiedName, SortedColumn, TableDef,
};
use vauban_errors::SqlError;
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, RowSet, execute_collect};
use vauban_planner::{PhysicalInsert, PhysicalPlan, PhysicalStatement, PhysicalUpdate};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager, TxnHandle};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

/// Builds an `int` type, nullable or not.
fn int(nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::Int, nullable)
}

/// Builds an `nvarchar(n)` type, nullable or not.
fn nvarchar(len: u16, nullable: bool) -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Fixed(len)), nullable)
}

/// A `BoundExpr` literal of the given type.
fn lit(value: Value, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 0,
    }
}

/// A `ColumnBinding` for a column of the given ordinal and type.
fn col_binding(index: usize, name: &str, ty: TypeInfo) -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(index as i32),
        index,
        name: name.to_owned(),
        ty,
    }
}

/// One `OutputColumn` with the given name and type.
fn out_col(name: &str, ty: TypeInfo) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        ty,
    }
}

/// A table name on `master.dbo.{name}`.
fn tbl_name(name: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: name.to_owned(),
    }
}

/// A column definition of type `ty`, without default, identity or computation.
fn column(name: &str, ty: TypeInfo) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty,
        default: None,
        identity: None,
        computed: None,
    }
}

/// A `PRIMARY KEY` over one column named `column`.
fn primary_key(column: &str) -> ConstraintDef {
    ConstraintDef::PrimaryKey {
        name: None,
        columns: vec![SortedColumn {
            column: column.to_owned(),
            descending: false,
        }],
        clustered: true,
    }
}

/// Builds a `PhysicalStatement::Insert` of `rows` into the columns named in `bindings`,
/// whose values are given the matching types.
fn insert_stmt(
    table: vauban_storage::TableId,
    columns: Vec<ColumnBinding>,
    rows: Vec<Vec<BoundExpr>>,
) -> PhysicalStatement {
    let schema = OutputSchema {
        columns: columns
            .iter()
            .map(|binding| out_col(&binding.name, binding.ty.clone()))
            .collect(),
    };
    PhysicalStatement::Insert(PhysicalInsert {
        table,
        columns,
        source: PhysicalPlan::Values { rows, schema },
        spool: false,
    })
}

/// Runs `stmt` on the engine built around `handle`.
fn execute(
    stmt: &PhysicalStatement,
    storage: &dyn Storage,
    txn_mgr: &TransactionManager,
    catalog: &Catalog,
    handle: &TxnHandle,
    eval: &StaticContext,
    session: &mut ExecSession,
) -> Result<(ExecOutcome, RowSet), SqlError> {
    let snap = txn_mgr.statement_snapshot(handle);
    let mut ctx = ExecContext::scalar(eval, SessionOptions::default())
        .with_engine(storage, txn_mgr, &snap)
        .with_catalog(catalog)
        .with_handle(handle)
        .with_session(session);
    execute_collect(stmt, &mut ctx)
}

/// A storage, its transaction manager, a catalogue and an open handle.
fn engine() -> (
    Arc<MemoryStorage>,
    Arc<TransactionManager>,
    Catalog,
    TxnHandle,
) {
    let storage = Arc::new(MemoryStorage::new());
    let txn_mgr = Arc::new(TransactionManager::new(storage.clone() as Arc<dyn Storage>));
    let catalog = Catalog::bootstrap(storage.clone(), txn_mgr.clone()).expect("bootstrap");
    let handle = txn_mgr.begin(IsolationLevel::ReadCommitted);
    (storage, txn_mgr, catalog, handle)
}

#[test]
fn duplicate_primary_key_is_2627() {
    let (storage, txn_mgr, catalog, handle) = engine();
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("pk_dup"),
                columns: vec![column("id", int(false)), column("note", nvarchar(20, true))],
                constraints: vec![ConstraintDef::PrimaryKey {
                    name: Some("pk_dup_constraint".to_owned()),
                    columns: vec![SortedColumn {
                        column: "id".to_owned(),
                        descending: false,
                    }],
                    clustered: true,
                }],
            },
        )
        .expect("create_table");
    let eval = StaticContext::default();
    let mut session = ExecSession::default();

    let columns = vec![
        col_binding(0, "id", int(false)),
        col_binding(1, "note", nvarchar(20, true)),
    ];
    let first = insert_stmt(
        meta.storage_id,
        columns.clone(),
        vec![vec![
            lit(Value::I32(1), int(true)),
            lit(
                Value::String(SqlString {
                    text: "a".to_owned(),
                }),
                nvarchar(20, true),
            ),
        ]],
    );
    execute(
        &first,
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect("the first row goes in");

    let duplicate = insert_stmt(
        meta.storage_id,
        columns,
        vec![vec![
            lit(Value::I32(1), int(true)),
            lit(
                Value::String(SqlString {
                    text: "b".to_owned(),
                }),
                nvarchar(20, true),
            ),
        ]],
    );
    let err = execute(
        &duplicate,
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect_err("the second row duplicates the key");

    assert_eq!(err.number, 2627);
    assert_eq!(err.severity, 14);
    assert_eq!(err.state, 1);
    assert!(
        err.message.contains("'pk_dup_constraint'"),
        "the constraint is named: {}",
        err.message
    );
    assert!(
        err.message.contains("object 'dbo.pk_dup'"),
        "the object is schema-qualified: {}",
        err.message
    );
    assert!(
        err.message.contains("value (1)"),
        "the key is the duplicate: {}",
        err.message
    );
}

#[test]
fn duplicate_unique_index_is_2601() {
    let (storage, txn_mgr, catalog, handle) = engine();
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("ux_dup"),
                columns: vec![column("id", int(false)), column("nom", nvarchar(50, true))],
                constraints: vec![primary_key("id")],
            },
        )
        .expect("create_table");
    catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ix_ux_dup_nom".to_owned(),
                columns: vec![SortedColumn {
                    column: "nom".to_owned(),
                    descending: false,
                }],
                unique: true,
                clustered: false,
            },
        )
        .expect("create_index");
    let eval = StaticContext::default();
    let mut session = ExecSession::default();

    let columns = vec![
        col_binding(0, "id", int(false)),
        col_binding(1, "nom", nvarchar(50, true)),
    ];
    let name = |text: &str| {
        lit(
            Value::String(SqlString {
                text: text.to_owned(),
            }),
            nvarchar(50, true),
        )
    };
    execute(
        &insert_stmt(
            meta.storage_id,
            columns.clone(),
            vec![vec![lit(Value::I32(1), int(true)), name("Alpha")]],
        ),
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect("the first row goes in");

    let err = execute(
        &insert_stmt(
            meta.storage_id,
            columns,
            vec![vec![lit(Value::I32(2), int(true)), name("Alpha")]],
        ),
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect_err("the second row duplicates the index key");

    assert_eq!(err.number, 2601, "an index is not a constraint");
    assert_eq!(err.severity, 14);
    assert!(
        err.message.contains("'ix_ux_dup_nom'"),
        "the index is named: {}",
        err.message
    );
    assert!(
        err.message.contains("object 'dbo.ux_dup'"),
        "the object is schema-qualified: {}",
        err.message
    );
    assert!(
        err.message.contains("value (Alpha)"),
        "the key is the duplicate: {}",
        err.message
    );
}

#[test]
fn composite_key_text_in_the_message() {
    // SQL Server writes the key of `(1, N'x y')` as `(1, x y)`: parentheses, a comma and a
    // space between the values, and the character value without quotes.
    let (storage, txn_mgr, catalog, handle) = engine();
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("ck_dup"),
                columns: vec![column("a", int(false)), column("b", nvarchar(20, false))],
                constraints: vec![ConstraintDef::PrimaryKey {
                    name: Some("pk_ck_dup".to_owned()),
                    columns: vec![
                        SortedColumn {
                            column: "a".to_owned(),
                            descending: false,
                        },
                        SortedColumn {
                            column: "b".to_owned(),
                            descending: false,
                        },
                    ],
                    clustered: true,
                }],
            },
        )
        .expect("create_table");
    let eval = StaticContext::default();
    let mut session = ExecSession::default();

    let columns = vec![
        col_binding(0, "a", int(false)),
        col_binding(1, "b", nvarchar(20, false)),
    ];
    let row = || {
        vec![
            lit(Value::I32(1), int(true)),
            lit(
                Value::String(SqlString {
                    text: "x y".to_owned(),
                }),
                nvarchar(20, false),
            ),
        ]
    };
    execute(
        &insert_stmt(meta.storage_id, columns.clone(), vec![row()]),
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect("the first row goes in");

    let err = execute(
        &insert_stmt(meta.storage_id, columns, vec![row()]),
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect_err("the second row duplicates the composite key");

    assert_eq!(err.number, 2627);
    assert!(
        err.message.ends_with("value (1, x y) exists already."),
        "the key text is the one SQL Server writes: {}",
        err.message
    );
}

#[test]
fn update_into_duplicate_is_2627() {
    let (storage, txn_mgr, catalog, handle) = engine();
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("upd_dup"),
                columns: vec![column("id", int(false)), column("note", nvarchar(20, true))],
                constraints: vec![primary_key("id")],
            },
        )
        .expect("create_table");
    for id in [1, 2] {
        storage
            .insert(
                handle.id,
                meta.storage_id,
                &vauban_storage::Row(vec![Value::I32(id), Value::Null]),
            )
            .expect("insert");
    }
    let eval = StaticContext::default();
    let mut session = ExecSession::default();

    let columns = vec![
        col_binding(0, "id", int(false)),
        col_binding(1, "note", nvarchar(20, true)),
    ];
    let stmt = PhysicalStatement::Update(PhysicalUpdate {
        table: meta.storage_id,
        input: PhysicalPlan::TableScan {
            table: meta.storage_id,
            columns: columns.clone(),
            schema: OutputSchema {
                columns: vec![
                    out_col("id", int(false)),
                    out_col("note", nvarchar(20, true)),
                ],
            },
            alias: "t".to_owned(),
            hints: LockHints::default(),
        },
        assignments: vec![(
            col_binding(0, "id", int(false)),
            lit(Value::I32(1), int(true)),
        )],
        spool: false,
    });
    let err = execute(
        &stmt,
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect_err("moving row 2 onto the key of row 1 duplicates it");

    assert_eq!(err.number, 2627);
    assert_eq!(err.severity, 14);
    assert!(
        err.message.contains("(1)"),
        "the duplicate key is named: {}",
        err.message
    );

    let scan = PhysicalStatement::Query(PhysicalPlan::TableScan {
        table: meta.storage_id,
        columns,
        schema: OutputSchema {
            columns: vec![
                out_col("id", int(false)),
                out_col("note", nvarchar(20, true)),
            ],
        },
        alias: "t".to_owned(),
        hints: LockHints::default(),
    });
    let (_, set) = execute(
        &scan,
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect("scan");
    assert_eq!(
        set.rows,
        vec![
            vec![Value::I32(1), Value::Null],
            vec![Value::I32(2), Value::Null]
        ],
        "the refused row keeps its key"
    );
}

#[test]
fn null_in_unique_index_is_rejected_once() {
    // SQL Server admits one `NULL` in a unique index; the second one is 2601 with the key
    // `(<NULL>)`.
    let (storage, txn_mgr, catalog, handle) = engine();
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: tbl_name("null_ux"),
                columns: vec![column("id", int(false)), column("v", int(true))],
                constraints: vec![primary_key("id")],
            },
        )
        .expect("create_table");
    catalog
        .create_index(
            &handle,
            &IndexDef {
                table: meta.id,
                name: "ux_null_v".to_owned(),
                columns: vec![SortedColumn {
                    column: "v".to_owned(),
                    descending: false,
                }],
                unique: true,
                clustered: false,
            },
        )
        .expect("create_index");
    let eval = StaticContext::default();
    let mut session = ExecSession::default();

    let columns = vec![
        col_binding(0, "id", int(false)),
        col_binding(1, "v", int(true)),
    ];
    let null_row = |id: i32| {
        insert_stmt(
            meta.storage_id,
            columns.clone(),
            vec![vec![
                lit(Value::I32(id), int(true)),
                lit(Value::Null, int(true)),
            ]],
        )
    };
    execute(
        &null_row(1),
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect("the first NULL goes in");

    let err = execute(
        &null_row(2),
        storage.as_ref(),
        txn_mgr.as_ref(),
        &catalog,
        &handle,
        &eval,
        &mut session,
    )
    .expect_err("the second NULL duplicates the index key");

    assert_eq!(err.number, 2601);
    assert!(
        err.message.ends_with("value (<NULL>) exists already."),
        "a NULL is written <NULL>: {}",
        err.message
    );
}
