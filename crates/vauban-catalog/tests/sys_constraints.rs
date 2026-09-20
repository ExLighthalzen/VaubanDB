//! What the bootstrap leaves in `storage` for `views/sys_constraints.rs`: the four internal
//! tables the `sys.foreign_keys`, `sys.foreign_key_columns`, `sys.check_constraints` and
//! `sys.default_constraints` views read.
//!
//! These tests know the public API only, as those of `tests/sys_tables.rs` do: the names of
//! the internal tables are `pub(crate)`, so a test that must reach one finds it by its shape.
//! The column names of the four views, the text of their definitions and the rows a
//! `TableMeta` gives are checked by the unit tests of `src/views/sys_constraints.rs`, which
//! can read what this file cannot.

use std::sync::Arc;

use vauban_catalog::{
    AlterTable, Catalog, ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, SortedColumn,
    TableDef, TableMeta,
};
use vauban_parser::{Expr, Literal, Span};
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

/// The identifiers of the tables of `master` whose shape is `columns`.
///
/// How a test outside the crate reaches an internal table without knowing its name. Unlike
/// `tests/sys_tables.rs`, this file reads a list rather than one table: the tables behind
/// `sys.check_constraints` and `sys.default_constraints` hold the same eight columns (unit
/// test `the_two_constraint_tables_share_their_shape`).
fn tables_shaped(storage: &Arc<dyn Storage>, columns: &[TypeInfo]) -> Vec<TableId> {
    storage
        .tables(master(storage))
        .expect("tables(master)")
        .into_iter()
        .filter(|(_, shape)| shape.columns == columns)
        .map(|(id, _)| id)
        .collect()
}

/// The number of rows of `table`, read through a transaction of its own.
fn row_count(storage: &Arc<dyn Storage>, txn: &Arc<TransactionManager>, table: TableId) -> usize {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = txn.statement_snapshot(&handle);
    let rows = storage.scan(&snapshot, table).expect("scan").count();
    txn.commit(handle).expect("commit of the reading txn");
    rows
}

/// A column of a definition, with the default given and nothing else on it.
fn column(name: &str, ty: SqlType, default: Option<Expr>) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, true),
        default,
        identity: None,
        computed: None,
    }
}

/// The integer literal `value` as an [`Expr`].
fn integer(value: &str) -> Expr {
    Expr::Literal(Literal::Integer(value.to_owned()), Span::EMPTY)
}

/// The shape of the internal table `sys.foreign_keys` reads: 12 columns.
fn foreign_keys_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::TinyInt, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::TinyInt, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Bit, false),
    ]
}

/// The shape of the internal table `sys.foreign_key_columns` reads: 7 columns.
fn foreign_key_columns_shape() -> Vec<TypeInfo> {
    vec![TypeInfo::new(SqlType::Int, false); 7]
}

/// The shape of the two internal tables `sys.check_constraints` and
/// `sys.default_constraints` read: 8 columns each, the same ones.
fn constraint_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Max), false),
        TypeInfo::new(SqlType::Bit, false),
    ]
}

#[test]
fn the_bootstrap_creates_the_four_internal_tables_of_this_file() {
    let (_catalog, storage, txn) = instance();
    let found = [
        (foreign_keys_shape(), 1),
        (foreign_key_columns_shape(), 1),
        (constraint_shape(), 2),
    ];
    for (shape, count) in found {
        let tables = tables_shaped(&storage, &shape);
        assert_eq!(tables.len(), count, "tables of that shape in master");
        for table in tables {
            // No row at bootstrap: the rows of the four tables are those of the constraints
            // a client declares, and a fresh instance holds none (unit test
            // `views_exist_and_are_empty_without_fk`).
            assert_eq!(row_count(&storage, &txn, table), 0);
        }
    }
}

#[test]
fn a_created_table_with_a_default_writes_the_default_constraints_row() {
    // `sys_rows.rs` writes `default_constraint_rows` alongside `check_constraint_rows`: a
    // column-level `DEFAULT` gains a row in one of the two tables of `constraint_shape()`,
    // the other — `sys.check_constraints`' — staying at zero.
    let (catalog, storage, txn) = instance();
    let constraint_tables = tables_shaped(&storage, &constraint_shape());
    assert_eq!(constraint_tables.len(), 2);
    let before: Vec<usize> = constraint_tables
        .iter()
        .map(|&table| row_count(&storage, &txn, table))
        .collect();

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: QualifiedName {
                    database: "master".to_owned(),
                    schema: "dbo".to_owned(),
                    name: "tbl_default".to_owned(),
                },
                columns: vec![
                    column("id", SqlType::Int, None),
                    column("amount", SqlType::Int, Some(integer("0"))),
                ],
                constraints: Vec::new(),
            },
        )
        .expect("a column-level DEFAULT is stored by create_table");
    txn.commit(handle).expect("commit");

    // The catalogue kept the expression on the column, which is what the rows of
    // `sys.default_constraints` are built from.
    assert_eq!(meta.columns[1].default, Some(integer("0")));
    let after: Vec<usize> = constraint_tables
        .iter()
        .map(|&table| row_count(&storage, &txn, table))
        .collect();
    let gained = after.iter().zip(&before).filter(|(a, b)| a > b).count();
    assert_eq!(gained, 1, "one of the two tables gains the DEFAULT's row");
    let total_before: usize = before.iter().sum();
    let total_after: usize = after.iter().sum();
    assert_eq!(total_after, total_before + 1);
}

/// The shape of the internal table `sys.objects` and `sys.tables` read
/// (`src/views/sys_tables.rs`, `objects_columns`): 11 columns.
fn objects_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Char(Len::Fixed(2)), false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(60)), false),
        TypeInfo::new(SqlType::Bit, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::DateTime, false),
        TypeInfo::new(SqlType::Int, false),
    ]
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

/// `FOREIGN KEY (columns) REFERENCES master.dbo.<referenced> (referenced_columns)`, named.
fn foreign_key(
    name: &str,
    columns: &[&str],
    referenced: &str,
    referenced_columns: &[&str],
) -> ConstraintDef {
    ConstraintDef::ForeignKey {
        name: Some(name.to_owned()),
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
        on_delete: vauban_parser::RefAction::NoAction,
        on_update: vauban_parser::RefAction::NoAction,
    }
}

/// The `(type, name)` pairs `sys.objects` carries for `parent`, sorted — how a test that
/// cannot read `objects_columns` by name reaches its `type` and `name` columns (module
/// documentation).
fn object_kinds_of(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
    parent: i32,
) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = rows(storage, txn, table)
        .into_iter()
        .filter(|row| int_at(row, 4) == parent)
        .map(|row| (text_at(&row, 5), text_at(&row, 2)))
        .collect();
    found.sort();
    found
}

#[test]
fn create_table_writes_a_row_per_kind_in_sys_objects_and_in_its_own_view() {
    // A table with the three kinds `sys_rows.rs` now writes a `sys.objects` row for —
    // `FOREIGN KEY`, `CHECK`, `DEFAULT` — plus the table's own `U ` row and its parent's:
    // five new rows of `sys.objects`, one each of `sys.foreign_keys`, `sys.check_constraints`
    // and `sys.default_constraints` (module documentation, `src/views/sys_tables.rs`,
    // `src/views/sys_constraints.rs`).
    let (catalog, storage, txn) = instance();
    let objects_table = tables_shaped(&storage, &objects_shape())
        .pop()
        .expect("one sys.objects table");
    let fk_table = tables_shaped(&storage, &foreign_keys_shape())
        .pop()
        .expect("one sys.foreign_keys table");
    let constraint_tables = tables_shaped(&storage, &constraint_shape());
    let objects_before = row_count(&storage, &txn, objects_table);
    let fk_before = row_count(&storage, &txn, fk_table);
    let constraint_before: usize = constraint_tables
        .iter()
        .map(|&table| row_count(&storage, &txn, table))
        .sum();

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let parent = catalog
        .create_table(
            &handle,
            &table(
                "tbl015_parent",
                vec![column("id", SqlType::Int, None)],
                vec![primary_key(&["id"])],
            ),
        )
        .expect("the parent table");
    let child = catalog
        .create_table(
            &handle,
            &table(
                "tbl015_child",
                vec![
                    column("id", SqlType::Int, None),
                    column("amount", SqlType::Int, None),
                    column("parent_id", SqlType::Int, None),
                ],
                vec![
                    ConstraintDef::Check {
                        name: Some("ck_tbl015".to_owned()),
                        expr: integer("1"),
                    },
                    named_default("df_tbl015", "amount", "0"),
                    foreign_key("fk_tbl015", &["parent_id"], "tbl015_parent", &["id"]),
                ],
            ),
        )
        .expect("the child table and its three constraints");
    txn.commit(handle).expect("commit");

    assert_eq!(
        row_count(&storage, &txn, objects_table),
        objects_before + 5,
        "two table rows, one FOREIGN KEY, one CHECK, one DEFAULT"
    );
    assert_eq!(row_count(&storage, &txn, fk_table), fk_before + 1);
    let constraint_after: usize = constraint_tables
        .iter()
        .map(|&table| row_count(&storage, &txn, table))
        .sum();
    assert_eq!(
        constraint_after,
        constraint_before + 2,
        "one CHECK, one DEFAULT"
    );

    let kinds = object_kinds_of(&storage, &txn, objects_table, child.id.0);
    assert_eq!(
        kinds,
        vec![
            ("C ".to_owned(), "ck_tbl015".to_owned()),
            ("D ".to_owned(), "df_tbl015".to_owned()),
            ("F ".to_owned(), "fk_tbl015".to_owned()),
        ]
    );
    // The parent carries no constraint row of its own: its `PRIMARY KEY` is not one of the
    // three kinds `sys.objects` gets a row for here (module documentation).
    assert_eq!(
        object_kinds_of(&storage, &txn, objects_table, parent.id.0),
        Vec::new()
    );
}

#[test]
fn drop_constraint_then_drop_table_remove_the_rows_again() {
    // The other half of the test above: what `CREATE TABLE` wrote,
    // `ALTER TABLE … DROP CONSTRAINT` and `DROP TABLE` take back.
    let (catalog, storage, txn) = instance();
    let objects_table = tables_shaped(&storage, &objects_shape())
        .pop()
        .expect("one sys.objects table");
    let fk_table = tables_shaped(&storage, &foreign_keys_shape())
        .pop()
        .expect("one sys.foreign_keys table");
    let constraint_tables = tables_shaped(&storage, &constraint_shape());
    let objects_before = row_count(&storage, &txn, objects_table);
    let fk_before = row_count(&storage, &txn, fk_table);
    let constraint_before: usize = constraint_tables
        .iter()
        .map(|&table| row_count(&storage, &txn, table))
        .sum();

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let parent = catalog
        .create_table(
            &handle,
            &table(
                "tbl016_parent",
                vec![column("id", SqlType::Int, None)],
                vec![primary_key(&["id"])],
            ),
        )
        .expect("the parent table");
    let child = catalog
        .create_table(
            &handle,
            &table(
                "tbl016_child",
                vec![
                    column("id", SqlType::Int, None),
                    column("amount", SqlType::Int, None),
                    column("parent_id", SqlType::Int, None),
                ],
                vec![
                    ConstraintDef::Check {
                        name: Some("ck_tbl016".to_owned()),
                        expr: integer("1"),
                    },
                    named_default("df_tbl016", "amount", "0"),
                    foreign_key("fk_tbl016", &["parent_id"], "tbl016_parent", &["id"]),
                ],
            ),
        )
        .expect("the child table and its three constraints");
    txn.commit(handle).expect("commit");

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .alter_table(
            &handle,
            child.id,
            &AlterTable::DropConstraint {
                name: "ck_tbl016".to_owned(),
            },
        )
        .expect("drop the CHECK");
    txn.commit(handle).expect("commit");
    assert_eq!(
        object_kinds_of(&storage, &txn, objects_table, child.id.0),
        vec![
            ("D ".to_owned(), "df_tbl016".to_owned()),
            ("F ".to_owned(), "fk_tbl016".to_owned()),
        ],
        "the CHECK's row is gone from sys.objects, the other two stay"
    );

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_table(&handle, child.id).expect("drop_table");
    txn.commit(handle).expect("commit");

    assert_eq!(
        object_kinds_of(&storage, &txn, objects_table, child.id.0),
        Vec::new(),
        "DROP TABLE takes the remaining constraint rows with it"
    );
    assert_eq!(
        row_count(&storage, &txn, objects_table),
        objects_before + 1,
        "the parent's own row is all that is left"
    );
    assert_eq!(row_count(&storage, &txn, fk_table), fk_before);
    let constraint_after: usize = constraint_tables
        .iter()
        .map(|&table| row_count(&storage, &txn, table))
        .sum();
    assert_eq!(constraint_after, constraint_before);
    assert!(object_kinds_of(&storage, &txn, objects_table, parent.id.0).is_empty());
}

#[test]
fn a_foreign_key_that_points_at_nothing_is_refused_by_create_table() {
    // Why the four tables stay empty here: `constraints.rs` stores a `FOREIGN KEY` in the
    // store of `table.rs` and the four internal tables of this file are not in
    // `sys_rows::rows_of`, which is the file that writes rows (`src/constraints.rs`,
    // `tests/constraints.rs`). This `CREATE TABLE` points at a table the catalogue does not
    // hold, which `constraints.rs` refuses with the number SQL Server sends, 1767.
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let err = catalog
        .create_table(
            &handle,
            &TableDef {
                name: QualifiedName {
                    database: "master".to_owned(),
                    schema: "dbo".to_owned(),
                    name: "tbl_child".to_owned(),
                },
                columns: vec![column("parent_id", SqlType::Int, None)],
                constraints: vec![ConstraintDef::ForeignKey {
                    name: None,
                    columns: vec!["parent_id".to_owned()],
                    referenced: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "tbl_parent".to_owned(),
                    },
                    referenced_columns: vec!["id".to_owned()],
                    on_delete: vauban_parser::RefAction::NoAction,
                    on_update: vauban_parser::RefAction::NoAction,
                }],
            },
        )
        .expect_err("the referenced table is not there");
    assert_eq!((err.number, err.severity, err.state), (1767, 16, 0));
    assert!(err.message.contains("dbo.tbl_parent"), "{}", err.message);
    txn.commit(handle).expect("commit");
}

#[test]
fn a_named_default_fills_the_column_and_keeps_its_name() {
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: QualifiedName {
                    database: "master".to_owned(),
                    schema: "dbo".to_owned(),
                    name: "tbl_named_default".to_owned(),
                },
                columns: vec![
                    column("id", SqlType::Int, None),
                    column("amount", SqlType::Int, None),
                ],
                constraints: vec![ConstraintDef::Default {
                    name: Some("df_amount".to_owned()),
                    column: "amount".to_owned(),
                    expr: integer("0"),
                }],
            },
        )
        .expect("a named DEFAULT is accepted");
    txn.commit(handle).expect("commit");

    // The column carries the expression, which is what the executor reads...
    assert_eq!(meta.columns[1].default, Some(integer("0")));
    // ...and the constraint keeps its name, which is what the catalogue views read.
    let constraints = catalog.constraints_of(meta.id).expect("constraints_of");
    assert_eq!(constraints.len(), 1);
    assert_eq!(constraints[0].name.name, "df_amount");
}

/// A `TableDef` of one or more columns in `master.dbo`.
fn table(name: &str, columns: Vec<ColumnDef>, constraints: Vec<ConstraintDef>) -> TableDef {
    TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: name.to_owned(),
        },
        columns,
        constraints,
    }
}

/// An integer column with an `IDENTITY` property and no default.
fn identity_column(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
        default: None,
        identity: Some(IdentitySpec::default()),
        computed: None,
    }
}

/// A named `DEFAULT` constraint on `column`.
fn named_default(name: &str, column: &str, value: &str) -> ConstraintDef {
    ConstraintDef::Default {
        name: Some(name.to_owned()),
        column: column.to_owned(),
        expr: integer(value),
    }
}

/// An unnamed `DEFAULT` constraint on `column`.
fn unnamed_default(column: &str, value: &str) -> ConstraintDef {
    ConstraintDef::Default {
        name: None,
        column: column.to_owned(),
        expr: integer(value),
    }
}

#[test]
fn two_defaults_on_one_column_are_8148() {
    // Four TableDef shapes of two defaults on one column, matching
    // `CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2)`,
    // `CREATE TABLE dbo.t (a int CONSTRAINT c DEFAULT 2 DEFAULT 1)`,
    // `CREATE TABLE dbo.t (a int CONSTRAINT c1 DEFAULT 1 CONSTRAINT c2 DEFAULT 2)` and
    // `CREATE TABLE dbo.t (a int DEFAULT 1 DEFAULT 2)` written as two unnamed constraints.
    let shapes = [
        table(
            "tbl_unnamed_then_named",
            vec![column("a", SqlType::Int, Some(integer("1")))],
            vec![named_default("c", "a", "2")],
        ),
        table(
            "tbl_named_then_unnamed",
            vec![column("a", SqlType::Int, Some(integer("1")))],
            vec![named_default("c", "a", "2")],
        ),
        table(
            "tbl_two_named",
            vec![column("a", SqlType::Int, None)],
            vec![named_default("c1", "a", "1"), named_default("c2", "a", "2")],
        ),
        table(
            "tbl_two_unnamed",
            vec![column("a", SqlType::Int, None)],
            vec![unnamed_default("a", "1"), unnamed_default("a", "2")],
        ),
    ];
    let (catalog, _storage, txn) = instance();
    for def in shapes {
        let printed = format!("dbo.{}", def.name.name);
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let err = catalog
            .create_table(&handle, &def)
            .expect_err("two defaults on one column");
        txn.rollback(handle)
            .expect("rollback of the refused create");
        assert_eq!(err.number, 8148, "{}", def.name.name);
        assert_eq!(err.severity, 16, "{}", def.name.name);
        assert_eq!(err.state, 0, "{}", def.name.name);
        assert_eq!(
            err.message,
            format!("A second DEFAULT constraint is refused for column 'a' of table '{printed}'."),
            "{}",
            def.name.name
        );
    }
}

#[test]
fn identity_and_default_are_1754() {
    let (catalog, _storage, txn) = instance();
    let unnamed = table(
        "tbl_id_unnamed",
        vec![ColumnDef {
            name: "a".to_owned(),
            ty: TypeInfo::new(SqlType::Int, true),
            default: Some(integer("1")),
            identity: Some(IdentitySpec::default()),
            computed: None,
        }],
        Vec::new(),
    );
    let named = table(
        "tbl_id_named",
        vec![identity_column("a")],
        vec![named_default("d", "a", "1")],
    );
    for def in [unnamed, named] {
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let err = catalog
            .create_table(&handle, &def)
            .expect_err("IDENTITY plus DEFAULT");
        txn.rollback(handle)
            .expect("rollback of the refused create");
        assert_eq!(err.number, 1754, "{}", def.name.name);
        assert_eq!(err.severity, 16, "{}", def.name.name);
        assert_eq!(err.state, 0, "{}", def.name.name);
        assert_eq!(
            err.message,
            format!(
                "A DEFAULT cannot sit on an IDENTITY column. Table '{}', column 'a'.",
                def.name.name
            ),
            "{}",
            def.name.name
        );
    }
}

#[test]
fn two_named_defaults_on_distinct_columns_succeed() {
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_two_cols_defaults",
                vec![
                    column("a", SqlType::Int, None),
                    column("b", SqlType::Int, None),
                ],
                vec![named_default("da", "a", "1"), named_default("db", "b", "2")],
            ),
        )
        .expect("one default per column");
    txn.commit(handle).expect("commit");
    assert_eq!(meta.columns[0].default, Some(integer("1")));
    assert_eq!(meta.columns[1].default, Some(integer("2")));
}

#[test]
fn two_defaults_on_identity_are_8148_not_1754() {
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_id_two_def",
                vec![identity_column("a")],
                vec![named_default("c1", "a", "1"), named_default("c2", "a", "2")],
            ),
        )
        .expect_err("two defaults win over IDENTITY");
    txn.rollback(handle)
        .expect("rollback of the refused create");
    assert_eq!(err.number, 8148);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert_eq!(
        err.message,
        "A second DEFAULT constraint is refused for column 'a' of table 'dbo.tbl_id_two_def'."
    );
}

#[test]
fn two_defaults_on_an_existing_name_are_8148() {
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_taken",
                vec![column("a", SqlType::Int, None)],
                Vec::new(),
            ),
        )
        .expect("first table");
    let err = catalog
        .create_table(
            &handle,
            &table(
                "tbl_taken",
                vec![column("a", SqlType::Int, Some(integer("1")))],
                vec![named_default("c", "a", "2")],
            ),
        )
        .expect_err("two defaults beat the existing name");
    txn.rollback(handle).expect("rollback");
    assert_eq!(err.number, 8148);
    assert_eq!(err.state, 0);
}

#[test]
fn identity_and_default_on_another_column_succeed() {
    let (catalog, _storage, txn) = instance();
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &table(
                "tbl_id_other_def",
                vec![identity_column("a"), column("b", SqlType::Int, None)],
                vec![named_default("db", "b", "0")],
            ),
        )
        .expect("IDENTITY on a, DEFAULT on b");
    txn.commit(handle).expect("commit");
    assert!(meta.columns[0].identity.is_some());
    assert_eq!(meta.columns[0].default, None);
    assert_eq!(meta.columns[1].default, Some(integer("0")));
}

fn fk_table(storage: &Arc<dyn Storage>) -> TableId {
    let tables = tables_shaped(storage, &foreign_keys_shape());
    assert_eq!(tables.len(), 1);
    tables[0]
}

fn fkc_table(storage: &Arc<dyn Storage>) -> TableId {
    let tables = tables_shaped(storage, &foreign_key_columns_shape());
    assert_eq!(tables.len(), 1);
    tables[0]
}

fn parent_and_child_tables(
    catalog: &Catalog,
    txn: &Arc<TransactionManager>,
    child_constraints: Vec<ConstraintDef>,
) -> (TableMeta, TableMeta) {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let parent = catalog
        .create_table(
            &handle,
            &table(
                "tbl_fk_parent",
                vec![column("id", SqlType::Int, None)],
                vec![ConstraintDef::PrimaryKey {
                    name: None,
                    columns: vec![SortedColumn {
                        column: "id".to_owned(),
                        descending: false,
                    }],
                    clustered: true,
                }],
            ),
        )
        .expect("parent");
    let child = catalog
        .create_table(
            &handle,
            &table(
                "tbl_fk_enfant",
                vec![
                    column("id", SqlType::Int, None),
                    column("pid", SqlType::Int, None),
                ],
                child_constraints,
            ),
        )
        .expect("child");
    txn.commit(handle).expect("commit");
    (parent, child)
}

#[test]
fn create_table_with_a_foreign_key_writes_one_row_in_each_internal_table() {
    let (catalog, storage, txn) = instance();
    let fk = fk_table(&storage);
    let fkc = fkc_table(&storage);
    let (parent, child) = parent_and_child_tables(
        &catalog,
        &txn,
        vec![ConstraintDef::ForeignKey {
            name: None,
            columns: vec!["pid".to_owned()],
            referenced: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "tbl_fk_parent".to_owned(),
            },
            referenced_columns: vec!["id".to_owned()],
            on_delete: vauban_parser::RefAction::NoAction,
            on_update: vauban_parser::RefAction::NoAction,
        }],
    );
    assert_eq!(row_count(&storage, &txn, fk), 1);
    assert_eq!(row_count(&storage, &txn, fkc), 1);
    let fk_rows = rows_of_parent(&storage, &txn, fk, child.id, 4);
    assert_eq!(int_at(&fk_rows[0], 5), parent.id.0);
    assert_eq!(int_at(&fk_rows[0], 6), 1, "key_index_id of a primary key");
    assert_eq!(
        fk_rows[0].get(7),
        Some(&vauban_types::Value::I8(0)),
        "delete_referential_action NO_ACTION"
    );
    assert_eq!(text_at(&fk_rows[0], 8), "NO_ACTION");
    let fkc_rows = rows_of_parent(&storage, &txn, fkc, child.id, 3);
    assert_eq!(int_at(&fkc_rows[0], 2), 1, "constraint_column_id");
    assert_eq!(int_at(&fkc_rows[0], 4), 2, "parent_column_id of pid");
    assert_eq!(int_at(&fkc_rows[0], 6), 1, "referenced_column_id of id");
}

#[test]
fn a_two_column_foreign_key_writes_two_column_rows_in_key_order() {
    let (catalog, storage, txn) = instance();
    let fk = fk_table(&storage);
    let fkc = fkc_table(&storage);
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_fk_parent2",
                vec![
                    column("a", SqlType::Int, None),
                    column("b", SqlType::Int, None),
                ],
                vec![ConstraintDef::PrimaryKey {
                    name: None,
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
            ),
        )
        .expect("parent");
    let child = catalog
        .create_table(
            &handle,
            &table(
                "tbl_fk_enfant2",
                vec![
                    column("x", SqlType::Int, None),
                    column("y", SqlType::Int, None),
                    column("z", SqlType::Int, None),
                ],
                vec![ConstraintDef::ForeignKey {
                    name: Some("fk_two".to_owned()),
                    columns: vec!["x".to_owned(), "y".to_owned()],
                    referenced: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "tbl_fk_parent2".to_owned(),
                    },
                    referenced_columns: vec!["a".to_owned(), "b".to_owned()],
                    on_delete: vauban_parser::RefAction::NoAction,
                    on_update: vauban_parser::RefAction::NoAction,
                }],
            ),
        )
        .expect("child");
    txn.commit(handle).expect("commit");
    assert_eq!(row_count(&storage, &txn, fk), 1);
    assert_eq!(row_count(&storage, &txn, fkc), 2);
    let mut seq: Vec<i32> = rows_of_parent(&storage, &txn, fkc, child.id, 3)
        .into_iter()
        .map(|row| int_at(&row, 2))
        .collect();
    seq.sort();
    assert_eq!(seq, vec![1, 2]);
}

#[test]
fn drop_table_removes_foreign_key_rows() {
    let (catalog, storage, txn) = instance();
    let fk = fk_table(&storage);
    let fkc = fkc_table(&storage);
    let (_parent, child) = parent_and_child_tables(
        &catalog,
        &txn,
        vec![ConstraintDef::ForeignKey {
            name: None,
            columns: vec!["pid".to_owned()],
            referenced: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "tbl_fk_parent".to_owned(),
            },
            referenced_columns: vec!["id".to_owned()],
            on_delete: vauban_parser::RefAction::NoAction,
            on_update: vauban_parser::RefAction::NoAction,
        }],
    );
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog.drop_table(&handle, child.id).expect("drop child");
    txn.commit(handle).expect("commit");
    assert_eq!(row_count(&storage, &txn, fk), 0);
    assert_eq!(row_count(&storage, &txn, fkc), 0);
}

#[test]
fn alter_drop_constraint_removes_foreign_key_rows() {
    let (catalog, storage, txn) = instance();
    let fk = fk_table(&storage);
    let fkc = fkc_table(&storage);
    let (_parent, child) = parent_and_child_tables(
        &catalog,
        &txn,
        vec![ConstraintDef::ForeignKey {
            name: Some("fk_drop".to_owned()),
            columns: vec!["pid".to_owned()],
            referenced: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "tbl_fk_parent".to_owned(),
            },
            referenced_columns: vec!["id".to_owned()],
            on_delete: vauban_parser::RefAction::NoAction,
            on_update: vauban_parser::RefAction::NoAction,
        }],
    );
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .alter_table(
            &handle,
            child.id,
            &AlterTable::DropConstraint {
                name: "fk_drop".to_owned(),
            },
        )
        .expect("drop constraint");
    txn.commit(handle).expect("commit");
    assert_eq!(row_count(&storage, &txn, fk), 0);
    assert_eq!(row_count(&storage, &txn, fkc), 0);
}

#[test]
fn alter_table_add_check_constraint_writes_its_row() {
    // The `ALTER TABLE … ADD CONSTRAINT` origin of a `CHECK`, alongside `CREATE TABLE`'s
    // (`create_table_writes_a_row_per_kind_in_sys_objects_and_in_its_own_view`) and `DROP`'s
    // (`drop_constraint_then_drop_table_remove_the_rows_again`).
    let (catalog, storage, txn) = instance();
    let objects_table = tables_shaped(&storage, &objects_shape())
        .pop()
        .expect("one sys.objects table");
    let constraint_tables = tables_shaped(&storage, &constraint_shape());

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let target = catalog
        .create_table(
            &handle,
            &table(
                "tbl017_alter_check",
                vec![column("amount", SqlType::Int, None)],
                Vec::new(),
            ),
        )
        .expect("create_table");
    catalog
        .alter_table(
            &handle,
            target.id,
            &AlterTable::AddConstraint {
                constraint: Box::new(ConstraintDef::Check {
                    name: Some("ck_tbl017".to_owned()),
                    expr: integer("1"),
                }),
            },
        )
        .expect("add check");
    txn.commit(handle).expect("commit");

    assert_eq!(
        object_kinds_of(&storage, &txn, objects_table, target.id.0),
        vec![("C ".to_owned(), "ck_tbl017".to_owned())]
    );
    let row = constraint_tables
        .iter()
        .flat_map(|&table| rows(&storage, &txn, table))
        .find(|row| int_at(row, 4) == target.id.0)
        .expect("the row of the CHECK");
    assert_eq!(text_at(&row, 2), "ck_tbl017");
    assert!(
        !bit_at(&row, 7),
        "a name was written, is_system_named is false"
    );
}

#[test]
fn alter_table_add_column_with_a_named_default_writes_its_row() {
    // The `ALTER TABLE … ADD` origin of a `DEFAULT`, the only one served (a standalone `ADD
    // DEFAULT … FOR col` is refused as not implemented yet): a named default written on the
    // added column keeps its name instead of falling to the generated shape
    // (`crates/vauban-catalog/src/alter.rs`, `plan_change`).
    let (catalog, storage, txn) = instance();
    let objects_table = tables_shaped(&storage, &objects_shape())
        .pop()
        .expect("one sys.objects table");
    let constraint_tables = tables_shaped(&storage, &constraint_shape());

    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let target = catalog
        .create_table(
            &handle,
            &table(
                "tbl018_alter_default",
                vec![column("id", SqlType::Int, None)],
                Vec::new(),
            ),
        )
        .expect("create_table");
    catalog
        .alter_table(
            &handle,
            target.id,
            &AlterTable::AddColumn {
                column: Box::new(column("amount", SqlType::Int, Some(integer("7")))),
                default_name: Some("df_tbl018".to_owned()),
            },
        )
        .expect("add column with a named default");
    txn.commit(handle).expect("commit");

    assert_eq!(
        object_kinds_of(&storage, &txn, objects_table, target.id.0),
        vec![("D ".to_owned(), "df_tbl018".to_owned())]
    );
    let row = constraint_tables
        .iter()
        .flat_map(|&table| rows(&storage, &txn, table))
        .find(|row| int_at(row, 4) == target.id.0)
        .expect("the row of the DEFAULT");
    assert_eq!(text_at(&row, 2), "df_tbl018");
    assert!(
        !bit_at(&row, 7),
        "a name was written, is_system_named is false"
    );
}

#[test]
fn a_rolled_back_create_table_with_a_foreign_key_leaves_no_row() {
    let (catalog, storage, txn) = instance();
    let fk = fk_table(&storage);
    let fkc = fkc_table(&storage);
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_fk_parent_rb",
                vec![column("id", SqlType::Int, None)],
                vec![ConstraintDef::PrimaryKey {
                    name: None,
                    columns: vec![SortedColumn {
                        column: "id".to_owned(),
                        descending: false,
                    }],
                    clustered: true,
                }],
            ),
        )
        .expect("parent");
    catalog
        .create_table(
            &handle,
            &table(
                "tbl_fk_enfant_rb",
                vec![
                    column("id", SqlType::Int, None),
                    column("pid", SqlType::Int, None),
                ],
                vec![ConstraintDef::ForeignKey {
                    name: None,
                    columns: vec!["pid".to_owned()],
                    referenced: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "tbl_fk_parent_rb".to_owned(),
                    },
                    referenced_columns: vec!["id".to_owned()],
                    on_delete: vauban_parser::RefAction::NoAction,
                    on_update: vauban_parser::RefAction::NoAction,
                }],
            ),
        )
        .expect("child");
    txn.rollback(handle).expect("rollback");
    assert_eq!(row_count(&storage, &txn, fk), 0);
    assert_eq!(row_count(&storage, &txn, fkc), 0);
}

fn rows_of_parent(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
    parent: vauban_catalog::ObjectId,
    parent_object_id_column: usize,
) -> Vec<Vec<vauban_types::Value>> {
    rows(storage, txn, table)
        .into_iter()
        .filter(|row| int_at(row, parent_object_id_column) == parent.0)
        .collect()
}

fn rows(
    storage: &Arc<dyn Storage>,
    txn: &Arc<TransactionManager>,
    table: TableId,
) -> Vec<Vec<vauban_types::Value>> {
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = txn.statement_snapshot(&handle);
    let read: Vec<_> = storage
        .scan(&snapshot, table)
        .expect("scan")
        .map(|row| row.expect("row").1.0)
        .collect();
    txn.commit(handle).expect("commit");
    read
}

fn int_at(row: &[vauban_types::Value], position: usize) -> i32 {
    match row.get(position) {
        Some(vauban_types::Value::I32(value)) => *value,
        other => panic!("column {position} is not an int: {other:?}"),
    }
}

fn text_at(row: &[vauban_types::Value], position: usize) -> String {
    match row.get(position) {
        Some(vauban_types::Value::String(text)) => text.text.clone(),
        other => panic!("column {position} is not text: {other:?}"),
    }
}

fn bit_at(row: &[vauban_types::Value], position: usize) -> bool {
    match row.get(position) {
        Some(vauban_types::Value::Bit(value)) => *value,
        other => panic!("column {position} is not a bit: {other:?}"),
    }
}
