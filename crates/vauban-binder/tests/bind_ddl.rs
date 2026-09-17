//! Binding of `CREATE TABLE`: a second bare `DEFAULT` on one column is kept.

use vauban_binder::{BindContext, BoundStatement, DdlStatement, SessionOptions, bind};
use vauban_catalog::ConstraintDef;
use vauban_parser::{ParseOptions, parse_batch};

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
