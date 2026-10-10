use super::*;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_storage::{DiskOptions, DiskStorage, MemoryStorage, Storage};
use vauban_txn::{IsolationLevel, TransactionManager};

use crate::def::{ColumnDef, ConstraintDef, IndexDef, SortedColumn, TableDef};

/// A subscriber that keeps the message of each `WARN` event, for the warning a reopening
/// writes when it leaves a part of a table out.
#[derive(Clone, Default)]
struct Warnings(Arc<std::sync::Mutex<Vec<String>>>);

impl Warnings {
    fn messages(&self) -> Vec<String> {
        self.0.lock().map(|kept| kept.clone()).unwrap_or_default()
    }
}

/// Reads the `message` field of an event.
struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Warnings {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            let mut message = Message(String::new());
            event.record(&mut message);
            if let Ok(mut kept) = self.0.lock() {
                kept.push(message.0);
            }
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// A directory of its own under the temporary directory, removed when dropped.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "vauban-catalog-reload-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create the directory of the instance");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn name(table: &str) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: "dbo".to_owned(),
        name: table.to_owned(),
    }
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

fn sorted(name: &str, descending: bool) -> SortedColumn {
    SortedColumn {
        column: name.to_owned(),
        descending,
    }
}

fn value(text: &str) -> vauban_parser::Expr {
    parse_stored_expression(text, ExpressionKind::Value).expect("a value")
}

/// `parent (id int PRIMARY KEY, code varchar(10) UNIQUE)`.
fn parent() -> TableDef {
    TableDef {
        name: name("parent"),
        columns: vec![
            column("id", SqlType::Int, false),
            column("code", SqlType::VarChar(Len::Fixed(10)), false),
        ],
        constraints: vec![
            ConstraintDef::PrimaryKey {
                name: None,
                columns: vec![sorted("id", false)],
                clustered: true,
            },
            ConstraintDef::Unique {
                name: Some("uq_parent_code".to_owned()),
                columns: vec![sorted("code", true)],
                clustered: false,
            },
        ],
    }
}

/// A table with each part the module documentation lists: an identity key, a foreign key, a
/// `CHECK`, a named and a bare `DEFAULT`, a computed column, a column of another collation.
fn child() -> TableDef {
    let mut id = column("id", SqlType::BigInt, false);
    id.identity = Some(IdentitySpec {
        seed: 10,
        increment: 5,
    });
    let mut status = column("status", SqlType::TinyInt, false);
    status.default = Some(value("7"));
    let mut doubled = column("doubled", SqlType::Int, true);
    doubled.computed = Some(value("qty * 2"));
    let mut label = column("label", SqlType::NVarChar(Len::Fixed(20)), true);
    label.ty.collation = Some(Collation::parse("Latin1_General_BIN").expect("a collation"));
    TableDef {
        name: name("child"),
        columns: vec![
            id,
            column("parent_id", SqlType::Int, false),
            column(
                "qty",
                SqlType::Decimal {
                    precision: 9,
                    scale: 2,
                },
                false,
            ),
            column("created", SqlType::DateTime, false),
            status,
            doubled,
            label,
            column("note", SqlType::VarChar(Len::Max), true),
        ],
        constraints: vec![
            ConstraintDef::PrimaryKey {
                name: Some("pk_child".to_owned()),
                columns: vec![sorted("id", false)],
                clustered: true,
            },
            ConstraintDef::ForeignKey {
                name: None,
                columns: vec!["parent_id".to_owned()],
                referenced: name("parent"),
                referenced_columns: vec!["id".to_owned()],
                on_delete: RefAction::Cascade,
                on_update: RefAction::NoAction,
            },
            ConstraintDef::Check {
                name: Some("ck_child_qty".to_owned()),
                expr: parse_stored_expression("qty > 0", ExpressionKind::Predicate)
                    .expect("a predicate"),
            },
            ConstraintDef::Default {
                name: Some("df_child_created".to_owned()),
                column: "created".to_owned(),
                expr: value("getdate()"),
            },
        ],
    }
}

/// Creates `parent`, `child` and a nonclustered index on `child`, and commits.
fn create_both(catalog: &Catalog, manager: &TransactionManager) -> (TableMeta, TableMeta) {
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let parent = catalog.create_table(&handle, &parent()).expect("parent");
    let child = catalog.create_table(&handle, &child()).expect("child");
    catalog
        .create_index(
            &handle,
            &IndexDef {
                table: child.id,
                name: "ix_child_parent".to_owned(),
                columns: vec![sorted("parent_id", false), sorted("qty", true)],
                unique: false,
                clustered: false,
            },
        )
        .expect("index");
    manager.commit(handle).expect("commit");
    (parent, child)
}

/// What the store of `catalog` holds about `table`: its metadata, its indexes, its
/// constraint objects.
fn held(catalog: &Catalog, table: ObjectId) -> (TableMeta, Vec<IndexMeta>, Vec<ObjectMeta>) {
    let store = super::super::store(catalog);
    (
        store.get(table).cloned().expect("the table is held"),
        store.indexes.of_table(table).into_iter().cloned().collect(),
        store
            .constraints
            .of_table(table)
            .into_iter()
            .cloned()
            .collect(),
    )
}

#[test]
fn a_reopened_catalogue_holds_the_tables_it_wrote() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let first = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
    let (parent, child) = create_both(&first, &manager);
    let before: Vec<_> = [parent.id, child.id]
        .iter()
        .map(|&id| held(&first, id))
        .collect();
    // The parts the test is about are there before the restart.
    assert_eq!(before[1].1.len(), 2, "the key and the index");
    assert_eq!(before[1].2.len(), 4, "foreign key, check, two defaults");

    let second = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("reopen");
    let after: Vec<_> = [parent.id, child.id]
        .iter()
        .map(|&id| held(&second, id))
        .collect();
    assert_eq!(after, before);
    // The shipped tables of `master.dbo` come back with the others.
    let store = super::super::store(&second);
    for shipped in crate::bootstrap::MASTER_DBO_SHIPPED_TABLES {
        assert!(
            store.live().any(|table| table.name == shipped),
            "{shipped} is not held after the reopen"
        );
    }
}

#[test]
fn a_table_reopened_from_disk_keeps_its_rows_and_its_identity() {
    let dir = TempDir::new("disk");
    let open = || {
        let storage: Arc<dyn Storage> =
            Arc::new(DiskStorage::open(&dir.0, DiskOptions::default()).expect("open the instance"));
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog =
            Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
        (catalog, manager)
    };
    let (parent, child, before, last) = {
        let (catalog, manager) = open();
        let (parent, child) = create_both(&catalog, &manager);
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let mut row = vec![Value::Null; parent.columns.len()];
        row[0] = Value::I32(1);
        row[1] = text("a");
        catalog
            .storage
            .insert(handle.id, parent.storage_id, &Row(row))
            .expect("insert");
        let last = catalog
            .next_identity(&handle, child.id)
            .expect("identity")
            .mantissa;
        manager.commit(handle).expect("commit");
        let before = held(&catalog, child.id);
        (parent, child, before, last)
    };
    let (catalog, manager) = open();
    assert_eq!(held(&catalog, child.id), before);
    let snapshot = catalog
        .snapshot(&manager.begin(IsolationLevel::ReadCommitted))
        .table(parent.id)
        .cloned();
    assert_eq!(
        snapshot.map(|meta| meta.storage_id),
        Some(parent.storage_id)
    );
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let rows = catalog
        .storage
        .scan(&manager.statement_snapshot(&handle), parent.storage_id)
        .expect("scan")
        .count();
    assert_eq!(rows, 1);
    let next = catalog
        .next_identity(&handle, child.id)
        .expect("identity")
        .mantissa;
    assert_eq!((last, next), (10, 15));
}

#[test]
fn the_counter_starts_past_every_stored_identifier() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let first = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
    let (_, child) = create_both(&first, &manager);
    // Ten constraint objects for one storage table: the identifiers run ahead of the
    // `TableId`s, so the floor `raise_counter_above` draws from `storage` stays below them.
    let many = TableDef {
        name: name("many"),
        columns: vec![column("a", SqlType::Int, true)],
        constraints: (0..10)
            .map(|bound| ConstraintDef::Check {
                name: None,
                expr: parse_stored_expression(&format!("a > {bound}"), ExpressionKind::Predicate)
                    .expect("a predicate"),
            })
            .collect(),
    };
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    first.create_table(&handle, &many).expect("many");
    manager.commit(handle).expect("commit");
    // The last table created carries an identity whose counter row outlives it.
    let mut def = TableDef {
        name: name("dropped"),
        columns: vec![column("id", SqlType::Int, false)],
        constraints: Vec::new(),
    };
    def.columns[0].identity = Some(IdentitySpec::default());
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let dropped = first.create_table(&handle, &def).expect("dropped");
    first.next_identity(&handle, dropped.id).expect("identity");
    manager.commit(handle).expect("commit");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    first.drop_table(&handle, dropped.id).expect("drop");
    manager.commit(handle).expect("commit");
    let stored_largest = {
        let read = Stored::read(&first, &registry_snapshot(&first, TxnId(0))).expect("read");
        read.largest_identifier(&first, &registry_snapshot(&first, TxnId(0)))
            .expect("largest")
            .expect("some identifier")
    };
    assert_eq!(stored_largest, dropped.id.0);
    assert!(dropped.id > child.id);

    let second = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("reopen");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let fresh = second
        .create_table(
            &handle,
            &TableDef {
                name: name("fresh"),
                columns: vec![column("a", SqlType::Int, true)],
                constraints: Vec::new(),
            },
        )
        .expect("fresh");
    manager.commit(handle).expect("commit");
    // Past the dropped table, whose identifier is kept by its counter row alone.
    assert!(
        fresh.id > dropped.id,
        "{} is not past {}",
        fresh.id,
        dropped.id
    );
}

#[test]
fn each_type_reads_back_from_its_column_row() {
    let types = [
        SqlType::Bit,
        SqlType::TinyInt,
        SqlType::SmallInt,
        SqlType::Int,
        SqlType::BigInt,
        SqlType::Decimal {
            precision: 38,
            scale: 10,
        },
        SqlType::Numeric {
            precision: 5,
            scale: 0,
        },
        SqlType::Float,
        SqlType::Real,
        SqlType::Money,
        SqlType::SmallMoney,
        SqlType::Char(Len::Fixed(3)),
        SqlType::VarChar(Len::Fixed(8000)),
        SqlType::VarChar(Len::Max),
        SqlType::NChar(Len::Fixed(4)),
        SqlType::NVarChar(Len::Fixed(4000)),
        SqlType::NVarChar(Len::Max),
        SqlType::Binary(Len::Fixed(16)),
        SqlType::VarBinary(Len::Max),
        SqlType::Date,
        SqlType::Time(0),
        SqlType::Time(7),
        SqlType::DateTime,
        SqlType::SmallDateTime,
        SqlType::DateTime2(3),
        SqlType::DateTimeOffset(7),
        SqlType::UniqueIdentifier,
    ];
    let table = TableMeta {
        id: ObjectId(1_000_000),
        storage_id: TableId(1),
        database: DbId(1),
        schema: "dbo".to_owned(),
        name: "t".to_owned(),
        columns: types
            .iter()
            .enumerate()
            .map(|(position, ty)| ColumnMeta {
                id: ColumnId(i32::try_from(position).unwrap() + 1),
                name: format!("c{position}"),
                ty: TypeInfo::new(*ty, true),
                ordinal: u16::try_from(position).unwrap(),
                default: None,
                identity: None,
                computed: None,
            })
            .collect(),
        clustered: None,
        constraints: Vec::new(),
    };
    use sys_tables::columns_columns as at;
    let rows = sys_tables::column_rows(std::slice::from_ref(&table)).expect("rows");
    let read: Vec<Option<SqlType>> = rows
        .iter()
        .map(|row| {
            type_of(
                tiny(&row.0, at::SYSTEM_TYPE_ID).unwrap(),
                small(&row.0, at::MAX_LENGTH).unwrap(),
                tiny(&row.0, at::PRECISION).unwrap(),
                tiny(&row.0, at::SCALE).unwrap(),
            )
        })
        .collect();
    let expected: Vec<Option<SqlType>> = types.iter().copied().map(Some).collect();
    assert_eq!(read, expected);
    assert_eq!(type_of(0, 4, 10, 0), None, "a system_type_id not written");
}

#[test]
fn a_column_row_carries_the_collation_and_the_computed_text() {
    let mut meta = TableMeta {
        id: ObjectId(1_000_001),
        storage_id: TableId(2),
        database: DbId(1),
        schema: "dbo".to_owned(),
        name: "t".to_owned(),
        columns: Vec::new(),
        clustered: None,
        constraints: Vec::new(),
    };
    let mut label = TypeInfo::new(SqlType::NVarChar(Len::Fixed(5)), true);
    label.collation = Some(Collation::parse("Latin1_General_BIN").expect("a collation"));
    meta.columns.push(ColumnMeta {
        id: ColumnId(3),
        name: "label".to_owned(),
        ty: label.clone(),
        ordinal: 0,
        default: None,
        identity: None,
        computed: Some(value("([a]*(2))")),
    });
    let rows = column_rows(&meta).expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(collation_of(&rows[0].0), label.collation);
    assert_eq!(
        string(&rows[0].0, column_details_columns::COMPUTED_DEFINITION),
        Some("([a]*(2))")
    );
    assert_eq!(small(&rows[0].0, column_details_columns::ORDINAL), Some(0));
}

/// `ALTER TABLE t ADD <name> int NULL` on `table`.
fn add_column(catalog: &Catalog, handle: &vauban_txn::TxnHandle, table: ObjectId, name: &str) {
    catalog
        .alter_table(
            handle,
            table,
            &crate::def::AlterTable::AddColumn {
                column: Box::new(column(name, SqlType::Int, true)),
                default_name: None,
            },
        )
        .expect("add column");
}

/// `ALTER TABLE t DROP COLUMN <name>` on `table`.
fn drop_column(catalog: &Catalog, handle: &vauban_txn::TxnHandle, table: ObjectId, name: &str) {
    catalog
        .alter_table(
            handle,
            table,
            &crate::def::AlterTable::DropColumn {
                name: name.to_owned(),
            },
        )
        .expect("drop column");
}

#[test]
fn the_column_watermark_survives_a_dropped_column_and_a_reopen() {
    // Columns a, b, c; DROP c; ADD d (4); DROP d; reopen; ADD e takes 5, not 3.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let first = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let table = first
        .create_table(
            &handle,
            &TableDef {
                name: name("w"),
                columns: vec![
                    column("a", SqlType::Int, true),
                    column("b", SqlType::Int, true),
                    column("c", SqlType::Int, true),
                ],
                constraints: Vec::new(),
            },
        )
        .expect("w")
        .id;
    manager.commit(handle).expect("commit");
    for step in [
        |catalog: &Catalog, handle: &vauban_txn::TxnHandle, table| {
            drop_column(catalog, handle, table, "c")
        },
        |catalog: &Catalog, handle: &vauban_txn::TxnHandle, table| {
            add_column(catalog, handle, table, "d")
        },
        |catalog: &Catalog, handle: &vauban_txn::TxnHandle, table| {
            drop_column(catalog, handle, table, "d")
        },
    ] {
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        step(&first, &handle, table);
        manager.commit(handle).expect("commit");
    }

    let second = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("reopen");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    add_column(&second, &handle, table, "e");
    manager.commit(handle).expect("commit");
    let ids: Vec<(String, i32)> = held(&second, table)
        .0
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.id.0))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("a".to_owned(), 1),
            ("b".to_owned(), 2),
            ("e".to_owned(), 5)
        ]
    );
}

#[test]
fn a_dropped_table_keeps_its_identifier_taken_across_a_reopen() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let first = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
    let plain = |table: &str| TableDef {
        name: name(table),
        columns: vec![column("a", SqlType::Int, true)],
        constraints: Vec::new(),
    };
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let last = first.create_table(&handle, &plain("zlast")).expect("zlast");
    manager.commit(handle).expect("commit");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    first.drop_table(&handle, last.id).expect("drop");
    manager.commit(handle).expect("commit");

    let second = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("reopen");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let fresh = second.create_table(&handle, &plain("znew")).expect("znew");
    manager.commit(handle).expect("commit");
    assert!(fresh.id > last.id, "{} is not past {}", fresh.id, last.id);
}

#[test]
fn a_foreign_key_whose_index_has_gone_is_left_out() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let first = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("bootstrap");
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let referenced = first
        .create_table(
            &handle,
            &TableDef {
                name: name("r"),
                columns: vec![
                    column("id", SqlType::Int, false),
                    column("k", SqlType::Int, false),
                ],
                constraints: Vec::new(),
            },
        )
        .expect("r");
    let index = first
        .create_index(
            &handle,
            &IndexDef {
                table: referenced.id,
                name: "ux_r".to_owned(),
                columns: vec![sorted("k", false)],
                unique: true,
                clustered: false,
            },
        )
        .expect("ux_r");
    let referencing = first
        .create_table(
            &handle,
            &TableDef {
                name: name("f"),
                columns: vec![column("x", SqlType::Int, true)],
                constraints: vec![
                    ConstraintDef::ForeignKey {
                        name: Some("fk_f".to_owned()),
                        columns: vec!["x".to_owned()],
                        referenced: name("r"),
                        referenced_columns: vec!["k".to_owned()],
                        on_delete: RefAction::NoAction,
                        on_update: RefAction::NoAction,
                    },
                    ConstraintDef::Check {
                        name: Some("ck_f".to_owned()),
                        expr: parse_stored_expression("x > 0", ExpressionKind::Predicate)
                            .expect("a predicate"),
                    },
                ],
            },
        )
        .expect("f");
    let other = first.create_table(&handle, &parent()).expect("parent");
    manager.commit(handle).expect("commit");
    // `Catalog::drop_index` refuses an index a foreign key reads (3723): the drop below
    // passes over that refusal to leave the foreign key without its index.
    let handle = manager.begin(IsolationLevel::ReadCommitted);
    let refused = first
        .drop_index(&handle, index.id)
        .expect_err("a foreign key reads ux_r");
    assert_eq!(refused.number, 3723);
    crate::index::drop_index_under_its_constraints(&first, &handle, index.id).expect("drop ux_r");
    manager.commit(handle).expect("commit");

    let warnings = Warnings::default();
    let second = tracing::subscriber::with_default(warnings.clone(), || {
        Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&manager)).expect("the instance opens")
    });
    // The foreign key is reported in the log.
    let logged = warnings.messages();
    assert!(
        logged
            .iter()
            .any(|message| message.contains("a foreign key is left out")),
        "{logged:?}"
    );
    // The other tables come back whole.
    assert_eq!(held(&second, other.id), held(&first, other.id));
    assert_eq!(
        held(&second, referenced.id).0,
        held(&first, referenced.id).0
    );
    // The referencing table comes back without its foreign key, with its check.
    let (meta, _, objects) = held(&second, referencing.id);
    assert_eq!(meta.columns, referencing.columns);
    assert!(
        matches!(meta.constraints.as_slice(), [ConstraintMeta::Check { .. }]),
        "{:?}",
        meta.constraints
    );
    let names: Vec<&str> = objects
        .iter()
        .map(|object| object.name.name.as_str())
        .collect();
    assert_eq!(names, vec!["ck_f"]);
}
