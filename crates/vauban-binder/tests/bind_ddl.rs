//! Binding of `CREATE TABLE`: a second bare `DEFAULT` on one column is kept, and which of
//! the two faults of a name the catalogue already holds decides.

use vauban_binder::{
    BindContext, BoundStatement, CatalogView, ColumnBinding, DdlStatement, NoVariables,
    ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ConstraintDef, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_types::{SqlType, TypeInfo};

/// The `TableDef` of a bound `CREATE TABLE`.
fn table_def(text: &str) -> vauban_catalog::TableDef {
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|error| panic!("{text} parses, got {}: {}", error.number, error.message));
    let [statement] = batch.statements.as_slice() else {
        panic!("{text} is one statement, got {}", batch.statements.len())
    };
    let ctx = BindContext::scalar(text, SessionOptions::default());
    match bind(statement, &ctx) {
        Ok(BoundStatement::Ddl(DdlStatement::CreateTable { def })) => def,
        other => panic!("{text} binds to a CreateTable, got {other:?}"),
    }
}

/// Two bare `DEFAULT` clauses on one column both reach the bound `TableDef`
/// (`CREATE TABLE t (a int DEFAULT 1 DEFAULT 2)`): the first fills the column, the
/// second is a nameless `ConstraintDef::Default`.
#[test]
fn two_bare_defaults_survive_binding() {
    let def = table_def("CREATE TABLE t (a int DEFAULT 1 DEFAULT 2);");
    assert!(
        def.columns[0].default.is_some(),
        "the first DEFAULT stays on the column"
    );
    match def.constraints.as_slice() {
        [ConstraintDef::Default { name, column, .. }] => {
            assert_eq!(name.as_deref(), None);
            assert_eq!(column, "a");
        }
        other => panic!("the second DEFAULT is a nameless constraint, got {other:?}"),
    }
}

/// A catalogue holding one table, `dbo.t (a int)`.
struct HoldsT;

impl CatalogView for HoldsT {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        name.name
            .value
            .eq_ignore_ascii_case("t")
            .then(|| ResolvedTable {
                object: ObjectId(1),
                table: Some(TableId(1)),
                columns: vec![ColumnBinding {
                    column: ColumnId(1),
                    index: 0,
                    name: "a".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, false),
                    depth: 0,
                }],
                kind: ResolvedTableKind::Table,
            })
    }

    fn view_definition(&self, _object: vauban_catalog::ObjectId) -> Option<&str> {
        None
    }

    fn identity_column(
        &self,
        _object: vauban_catalog::ObjectId,
    ) -> Option<vauban_catalog::ColumnId> {
        None
    }

    fn computed_columns(&self, _object: vauban_catalog::ObjectId) -> Vec<vauban_catalog::ColumnId> {
        Vec::new()
    }

    fn constraint_names_on_table(&self, _table: vauban_catalog::ObjectId) -> Vec<String> {
        Vec::new()
    }

    fn primary_key_columns(&self, _table: vauban_catalog::ObjectId) -> Option<Vec<String>> {
        None
    }

    fn unique_key_on(&self, _table: vauban_catalog::ObjectId, _columns: &[String]) -> bool {
        false
    }

    fn classify_table_reference(
        &self,
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> vauban_binder::TableReferenceKind {
        match vauban_binder::CatalogView::resolve_table(self, name, database, default_schema) {
            Some(_) => vauban_binder::TableReferenceKind::Table,
            None => vauban_binder::TableReferenceKind::Unknown,
        }
    }
}

/// Binds the first statement of `text` against [`HoldsT`].
fn bind_holding(text: &str) -> Result<BoundStatement, SqlError> {
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|error| panic!("{text} parses, got {}", error.number));
    bind(
        &batch.statements[0],
        &BindContext {
            text,
            catalog: Some(&HoldsT),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        },
    )
}

/// The column list is read before the name is looked up, so a `CREATE TABLE` that both
/// repeats a column and reuses the name of a table the catalogue holds answers 2705 state
/// 3: a fault of the text, which stays a binding error even inside a branch, where the
/// 2714 of the name alone waits for the control of flow.
#[test]
fn a_repeated_column_is_answered_before_the_name_the_catalogue_holds() {
    let error = bind_holding("IF 1 = 0 CREATE TABLE dbo.t (a int, a int);")
        .expect_err("the repeated column stops the binding");
    assert_eq!((error.number, error.state), (2705, 3), "{error:?}");

    let error = bind_holding("CREATE TABLE dbo.t (a int, a int);")
        .expect_err("the repeated column stops the binding");
    assert_eq!((error.number, error.state), (2705, 3), "{error:?}");

    let bound = bind_holding("IF 1 = 0 CREATE TABLE dbo.t (a int);")
        .expect("the name alone lets the IF bind");
    let BoundStatement::If { then_, .. } = &bound else {
        panic!("expected If, got {bound:?}");
    };
    assert!(
        matches!(then_.as_ref(), BoundStatement::Deferred(_)),
        "the branch waits, got {then_:?}"
    );
}
