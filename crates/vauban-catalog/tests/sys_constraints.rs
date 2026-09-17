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

use vauban_catalog::{Catalog, ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, TableDef};
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

/// The shape of the internal table `sys.foreign_keys` reads: 11 columns.
fn foreign_keys_shape() -> Vec<TypeInfo> {
    vec![
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::Int, false),
        TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
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
fn a_created_table_with_a_default_leaves_the_internal_tables_untouched_for_now() {
    // The boundary `src/views/sys_constraints.rs` states: `create_table` keeps its
    // `TableMeta` in the store of `table.rs`, and `sys_rows.rs` does not write
    // `default_constraint_rows` into `storage` yet. Persisting them turns the zero below
    // into 1.
    let (catalog, storage, txn) = instance();
    let defaults = tables_shaped(&storage, &constraint_shape());
    assert_eq!(defaults.len(), 2);

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
    for table in defaults {
        assert_eq!(row_count(&storage, &txn, table), 0);
    }
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
    assert!(err.message.contains("1767"), "{}", err.message);
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
