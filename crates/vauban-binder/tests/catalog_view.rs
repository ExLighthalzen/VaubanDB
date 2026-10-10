//! `CatalogView` as production answers it: each method asked of a [`CatalogSnapshot`] taken
//! from a [`Catalog`] built by [`Catalog::bootstrap`] over memory storage, and the refusals
//! those answers drive, bound against that snapshot.
//!
//! The tables are created by binding their `CREATE TABLE`, `CREATE INDEX` and
//! `ALTER TABLE` text against the snapshot of the moment and handing the bound statement to
//! the catalogue, one committed transaction per statement.

use std::sync::Arc;

use vauban_binder::{
    BindContext, BoundStatement, CatalogView, DdlStatement, NoVariables, ResolvedTableKind,
    SessionOptions, TableReferenceKind, bind,
};
use vauban_catalog::{
    AlterTable, Catalog, CatalogSnapshot, ColumnDef, ObjectId, QualifiedName, TableDef,
};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, Ident, Literal, ObjectName, ParseOptions, Span, parse_batch};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::register_builtins;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo};

/// A bootstrapped catalogue over memory storage and the transaction manager it runs under.
struct Fixture {
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Fixture {
    fn new() -> Self {
        register_builtins();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&txn)).expect("bootstrap");
        Self { txn, catalog }
    }

    /// The fixture after each statement of `text` was bound and run, in order.
    fn with(text: &str) -> Self {
        let fixture = Self::new();
        fixture.run(text).expect("setup runs");
        fixture
    }

    /// The catalogue as a new transaction sees it.
    fn snapshot(&self) -> CatalogSnapshot {
        let read = self.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = self.catalog.snapshot(&read);
        self.txn.commit(read).expect("commit read");
        snapshot
    }

    /// Binds each statement of `text` against the snapshot of the moment and runs the DDL it
    /// binds to, one committed transaction per statement.
    fn run(&self, text: &str) -> SqlResult<()> {
        let batch = parse_batch(text, &ParseOptions::default()).expect("parse");
        for statement in &batch.statements {
            let snapshot = self.snapshot();
            let bound = bind(statement, &ctx(text, &snapshot))?;
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            let outcome = self.apply(&handle, &snapshot, &bound);
            match outcome {
                Ok(()) => self.txn.commit(handle).expect("commit"),
                Err(err) => {
                    self.txn.rollback(handle).expect("rollback");
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn apply(
        &self,
        handle: &vauban_txn::TxnHandle,
        snapshot: &CatalogSnapshot,
        bound: &BoundStatement,
    ) -> SqlResult<()> {
        match bound {
            BoundStatement::Ddl(DdlStatement::CreateTable { def }) => {
                self.catalog.create_table(handle, def).map(|_| ())
            }
            BoundStatement::Ddl(DdlStatement::CreateIndex { def }) => {
                self.catalog.create_index(handle, def).map(|_| ())
            }
            BoundStatement::Ddl(DdlStatement::AlterTable { table, action }) => {
                let object = id_of(snapshot, &table.name);
                self.catalog.alter_table(handle, object, action).map(|_| ())
            }
            BoundStatement::Ddl(DdlStatement::DropIndex { name, table, .. }) => {
                let object = id_of(snapshot, &table.name);
                let index = snapshot
                    .indexes_of(object)
                    .iter()
                    .find(|index| index.name.eq_ignore_ascii_case(name))
                    .unwrap_or_else(|| panic!("{name} is an index of {}", table.name))
                    .id;
                self.catalog.drop_index(handle, index)
            }
            BoundStatement::Block(statements) => statements
                .iter()
                .try_for_each(|statement| self.apply(handle, snapshot, statement)),
            other => panic!("the fixture runs DDL only, got {other:?}"),
        }
    }

    /// What binding the first statement of `text` gives.
    fn bound(&self, text: &str) -> BoundStatement {
        let snapshot = self.snapshot();
        let batch = parse_batch(text, &ParseOptions::default()).expect("parse");
        bind(&batch.statements[0], &ctx(text, &snapshot)).expect("binds")
    }
}

fn ctx<'a>(text: &'a str, snapshot: &'a CatalogSnapshot) -> BindContext<'a> {
    BindContext {
        text,
        catalog: Some(snapshot),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    }
}

/// `dbo.<name>` as the parser builds it.
fn name(table: &str) -> ObjectName {
    let ident = |value: &str| Ident {
        value: value.to_owned(),
        quoted: false,
    };
    ObjectName {
        server: None,
        database: None,
        schema: Some(ident(if table.contains('.') { "sys" } else { "dbo" })),
        name: ident(table.rsplit('.').next().unwrap_or(table)),
        span: Span::EMPTY,
    }
}

/// The identifier of `dbo.<table>` (or `sys.<view>`) in `snapshot`.
fn id_of(snapshot: &CatalogSnapshot, table: &str) -> ObjectId {
    snapshot
        .resolve_table(&name(table), "master", "dbo")
        .unwrap_or_else(|| panic!("{table} resolves"))
        .object
}

fn number_state(err: &SqlError) -> (u32, u8, u8) {
    (err.number, err.severity, err.state)
}

/// The tables most tests below read: a key `pk`, a table `fk` that references it, a table
/// whose foreign key points at itself, a `UNIQUE` constraint, and a unique index that a
/// foreign key of another table points at.
const KEYS: &str = "\
CREATE TABLE pk (id int PRIMARY KEY, v int);
CREATE TABLE fk (r int REFERENCES pk(id));
CREATE TABLE self_ref (id int PRIMARY KEY, parent int REFERENCES self_ref(id));
CREATE TABLE u (id int NOT NULL, k int CONSTRAINT uq_u UNIQUE);
CREATE TABLE r (id int NOT NULL, k int NOT NULL, n int);
CREATE UNIQUE INDEX ux_r ON r(k);
CREATE UNIQUE INDEX ux_lone ON r(id);
CREATE INDEX ix_n ON r(n);
CREATE TABLE f (x int CONSTRAINT fk_f REFERENCES r(k));";

#[test]
fn resolve_table_finds_a_table_and_a_view_and_misses_an_unknown_name() {
    let fixture = Fixture::with(KEYS);
    let snapshot = fixture.snapshot();
    let table = snapshot
        .resolve_table(&name("pk"), "master", "dbo")
        .expect("pk");
    assert_eq!(table.kind, ResolvedTableKind::Table);
    assert!(table.table.is_some());
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["id", "v"]);
    let view = snapshot
        .resolve_table(&name("sys.tables"), "master", "dbo")
        .expect("sys.tables");
    assert_eq!(view.kind, ResolvedTableKind::View);
    assert!(view.table.is_none());
    assert!(
        snapshot
            .resolve_table(&name("nosuch"), "master", "dbo")
            .is_none()
    );
}

#[test]
fn view_definition_answers_for_a_view_and_not_for_a_table() {
    let fixture = Fixture::with(KEYS);
    let snapshot = fixture.snapshot();
    let view = id_of(&snapshot, "sys.tables");
    assert!(
        CatalogView::view_definition(&snapshot, view)
            .is_some_and(|text| text.to_ascii_uppercase().contains("SELECT"))
    );
    assert!(CatalogView::view_definition(&snapshot, id_of(&snapshot, "pk")).is_none());
}

#[test]
fn identity_column_is_the_column_declared_identity() {
    let fixture = Fixture::with("CREATE TABLE i (a int, b int IDENTITY(1,1) NOT NULL);");
    let snapshot = fixture.snapshot();
    let table = id_of(&snapshot, "i");
    let b = snapshot
        .table(table)
        .expect("meta")
        .columns
        .iter()
        .find(|column| column.name == "b")
        .expect("b")
        .id;
    assert_eq!(snapshot.identity_column(table), Some(b));
    let plain = Fixture::with("CREATE TABLE p (a int);");
    let plain_snapshot = plain.snapshot();
    assert_eq!(
        plain_snapshot.identity_column(id_of(&plain_snapshot, "p")),
        None
    );
}

/// A computed column is created through [`Catalog::create_table`] directly: the binder does
/// not bind one yet.
#[test]
fn computed_columns_lists_the_computed_column() {
    let fixture = Fixture::new();
    let column = |name: &str, computed: Option<Expr>| ColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
        default: None,
        identity: None,
        computed,
    };
    let def = TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "c".to_owned(),
        },
        columns: vec![
            column("a", None),
            column(
                "b",
                Some(Expr::Literal(Literal::Integer("1".to_owned()), Span::EMPTY)),
            ),
        ],
        constraints: Vec::new(),
    };
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let meta = fixture.catalog.create_table(&handle, &def).expect("create");
    fixture.txn.commit(handle).expect("commit");
    let snapshot = fixture.snapshot();
    let b = meta.columns.iter().find(|c| c.name == "b").expect("b").id;
    assert_eq!(snapshot.computed_columns(meta.id), vec![b]);
    let other = Fixture::with(KEYS);
    let other_snapshot = other.snapshot();
    assert!(
        other_snapshot
            .computed_columns(id_of(&other_snapshot, "pk"))
            .is_empty()
    );
}

#[test]
fn constraint_names_on_table_lists_each_kind() {
    let fixture = Fixture::with(
        "CREATE TABLE k (id int CONSTRAINT pk_k PRIMARY KEY, u int CONSTRAINT uq_k UNIQUE, \
         c int CONSTRAINT ck_k CHECK (c > 0), d int CONSTRAINT df_k DEFAULT 1, \
         s int CONSTRAINT fk_k REFERENCES k(id));",
    );
    let snapshot = fixture.snapshot();
    let mut names = snapshot.constraint_names_on_table(id_of(&snapshot, "k"));
    names.sort();
    assert_eq!(names, ["ck_k", "df_k", "fk_k", "pk_k", "uq_k"]);
}

#[test]
fn primary_key_columns_names_the_key_in_order() {
    let fixture = Fixture::with(
        "CREATE TABLE two (a int NOT NULL, b int NOT NULL, CONSTRAINT pk_two PRIMARY KEY (b, a));",
    );
    let snapshot = fixture.snapshot();
    assert_eq!(
        snapshot.primary_key_columns(id_of(&snapshot, "two")),
        Some(vec!["b".to_owned(), "a".to_owned()])
    );
    let keys = Fixture::with(KEYS);
    let keys_snapshot = keys.snapshot();
    assert_eq!(
        keys_snapshot.primary_key_columns(id_of(&keys_snapshot, "r")),
        None
    );
}

#[test]
fn unique_key_on_reads_unique_constraints_and_unique_indexes() {
    let fixture = Fixture::with(KEYS);
    let snapshot = fixture.snapshot();
    let k = ["k".to_owned()];
    assert!(snapshot.unique_key_on(id_of(&snapshot, "u"), &k));
    assert!(snapshot.unique_key_on(id_of(&snapshot, "r"), &k));
    assert!(!snapshot.unique_key_on(id_of(&snapshot, "r"), &["n".to_owned()]));
    assert!(!snapshot.unique_key_on(id_of(&snapshot, "r"), &[]));
    assert!(!snapshot.unique_key_on(id_of(&snapshot, "fk"), &["r".to_owned()]));
}

#[test]
fn classify_known_table_is_table() {
    let fixture = Fixture::with(KEYS);
    let snapshot = fixture.snapshot();
    assert_eq!(
        snapshot.classify_table_reference(&name("pk"), "master", "dbo"),
        TableReferenceKind::Table
    );
    assert_eq!(
        snapshot.classify_table_reference(&name("sys.tables"), "master", "dbo"),
        TableReferenceKind::Table
    );
}

/// The counter-proof of `classify_known_table_is_table`: the same snapshot answers
/// `Unknown` for a name it does not resolve.
#[test]
fn classify_unknown_table_stays_unknown() {
    let fixture = Fixture::with(KEYS);
    let snapshot = fixture.snapshot();
    assert_eq!(
        snapshot.classify_table_reference(&name("nosuch"), "master", "dbo"),
        TableReferenceKind::Unknown
    );
}

/// The binder leaves 4712 to the execution of `TRUNCATE TABLE`, which reads the catalogue
/// of the moment the statement runs.
#[test]
fn truncate_of_a_referenced_table_binds_and_leaves_4712_to_the_execution() {
    let fixture = Fixture::with(KEYS);
    for text in [
        "TRUNCATE TABLE pk;",
        "TRUNCATE TABLE dbo.r;",
        "TRUNCATE TABLE fk;",
        "TRUNCATE TABLE self_ref;",
    ] {
        assert!(
            matches!(
                fixture.bound(text),
                BoundStatement::Ddl(DdlStatement::TruncateTable { .. })
            ),
            "{text}"
        );
    }
}

/// `DROP INDEX` binds whatever the index serves: [`Catalog::drop_index`] refuses the unique
/// index a `FOREIGN KEY` points at with 3723 state 6 when the statement runs, and drops it
/// once that `FOREIGN KEY` is gone.
#[test]
fn drop_index_a_foreign_key_points_at_binds_and_is_3723_state_6_when_it_runs() {
    let fixture = Fixture::with(KEYS);
    for text in [
        "DROP INDEX ux_r ON dbo.r;",
        "DROP INDEX IF EXISTS ux_r ON r;",
    ] {
        assert!(
            matches!(
                fixture.bound(text),
                BoundStatement::Ddl(DdlStatement::DropIndex { .. })
            ),
            "{text}"
        );
        let error = fixture.run(text).expect_err("the catalogue refuses");
        assert_eq!(number_state(&error), (3723, 16, 6), "{text}");
    }
    fixture
        .run("DROP INDEX ix_n ON r; DROP INDEX ux_lone ON r;")
        .expect("no constraint needs these two");
    fixture
        .run("ALTER TABLE f DROP CONSTRAINT fk_f; DROP INDEX ux_r ON r;")
        .expect("no foreign key reads ux_r any more");
}

#[test]
fn drop_index_of_a_key_constraint_is_3723_state_4_or_5_when_it_runs() {
    let fixture = Fixture::with(
        "CREATE TABLE k (id int CONSTRAINT pk_k PRIMARY KEY, u int CONSTRAINT uq_k UNIQUE);",
    );
    let primary = fixture
        .run("DROP INDEX pk_k ON k;")
        .expect_err("primary key");
    assert_eq!(number_state(&primary), (3723, 16, 4));
    let unique = fixture.run("DROP INDEX uq_k ON k;").expect_err("unique");
    assert_eq!(number_state(&unique), (3723, 16, 5));
}

/// The binder leaves the dependent objects of a dropped column to
/// [`Catalog::alter_table`], which refuses with 5074 when the statement runs.
#[test]
fn drop_indexed_column_binds_and_leaves_5074_to_the_catalogue() {
    let fixture = Fixture::with(KEYS);
    let BoundStatement::Ddl(DdlStatement::AlterTable { action, .. }) =
        fixture.bound("ALTER TABLE r DROP COLUMN n;")
    else {
        panic!("expected ALTER TABLE");
    };
    assert_eq!(
        action,
        AlterTable::DropColumn {
            name: "n".to_owned()
        }
    );
    let error = fixture
        .run("ALTER TABLE r DROP COLUMN n;")
        .expect_err("the catalogue refuses");
    assert_eq!(number_state(&error), (5074, 16, 1));
    let key = fixture
        .run("ALTER TABLE pk DROP COLUMN id;")
        .expect_err("the key column is refused");
    assert_eq!(number_state(&key), (5074, 16, 1));
}

/// The binder leaves a taken constraint name to [`Catalog::alter_table`], which refuses with
/// 2714 state 5 when the statement runs.
#[test]
fn a_taken_constraint_name_binds_and_leaves_2714_to_the_catalogue() {
    let fixture = Fixture::with(KEYS);
    for text in [
        "ALTER TABLE r ADD CONSTRAINT fk CHECK (n > 0);",
        "ALTER TABLE r ADD CONSTRAINT fk_f CHECK (n > 0);",
    ] {
        assert!(matches!(
            fixture.bound(text),
            BoundStatement::Ddl(DdlStatement::AlterTable { .. })
        ));
        let error = fixture.run(text).expect_err("the catalogue refuses");
        assert_eq!(number_state(&error), (2714, 16, 5), "{text}");
    }
}
