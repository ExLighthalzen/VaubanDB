use super::{
    BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
    TableReferenceKind, VariableScope,
};
use vauban_catalog::ObjectId;
use vauban_parser::{Ident, ObjectName, Span};

/// A view that knows one table, `t`, and nothing else.
struct OneTable;

impl CatalogView for OneTable {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        (name.name.value == "t").then(|| ResolvedTable {
            object: ObjectId(42),
            table: None,
            columns: Vec::new(),
            kind: ResolvedTableKind::Table,
        })
    }
}

fn name(value: &str) -> ObjectName {
    ObjectName {
        server: None,
        database: None,
        schema: None,
        name: Ident {
            value: value.to_owned(),
            quoted: false,
        },
        span: Span {
            line: 1,
            column: 1,
            offset: 0,
            len: 1,
        },
    }
}

/// `OneTable` implements `resolve_table` and not `classify_table_reference`: the answer
/// below comes from the default of the trait reading the resolution.
#[test]
fn classify_known_table_is_table() {
    assert_eq!(
        OneTable.classify_table_reference(&name("t"), "master", "dbo"),
        TableReferenceKind::Table
    );
}

/// The counter-proof of `classify_known_table_is_table`: the same default answers
/// `Unknown` for the name `OneTable` does not resolve, so the `Table` above is not a
/// constant.
#[test]
fn classify_unknown_table_stays_unknown() {
    assert_eq!(
        OneTable.classify_table_reference(&name("other"), "master", "dbo"),
        TableReferenceKind::Unknown
    );
    assert!(
        OneTable
            .resolve_table(&name("other"), "master", "dbo")
            .is_none()
    );
    assert!(OneTable.view_definition(ObjectId(42)).is_none());
}

#[test]
fn no_variables_knows_no_variable() {
    assert!(NoVariables.type_of("@x").is_none());
    assert!(NoVariables.type_of("@@rowcount").is_none());
}

#[test]
fn scalar_context_keeps_the_text_it_was_given() {
    let ctx = BindContext::scalar("SELECT 1", SessionOptions::default());
    assert_eq!(ctx.text, "SELECT 1");
    assert!(ctx.catalog.is_none());
}
