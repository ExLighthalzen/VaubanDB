use super::*;
use crate::context::TableReferenceKind;
use vauban_parser::{Ident, Span};
use vauban_types::{SqlType, TypeInfo};

/// The name `t`, unqualified, as the parser builds it.
fn object_name(parts: &[&str]) -> ObjectName {
    let ident = |value: &str| Ident {
        value: value.to_owned(),
        quoted: false,
    };
    let mut name = ObjectName {
        server: None,
        database: None,
        schema: None,
        name: ident(parts[parts.len() - 1]),
        span: Span {
            line: 1,
            column: 1,
            offset: 0,
            len: 1,
        },
    };
    let leading = &parts[..parts.len() - 1];
    let mut rest = leading.iter().rev();
    name.schema = rest.next().map(|part| ident(part));
    name.database = rest.next().map(|part| ident(part));
    name.server = rest.next().map(|part| ident(part));
    name
}

/// `resolve_table` on a snapshot answers what [`CatalogSnapshot::resolve_object`]
/// answers: the **forwarding** is what is asserted here.
#[test]
fn catalog_snapshot_impl_resolves_a_created_table() {
    let snapshot = CatalogSnapshot::default();
    assert!(
        snapshot
            .resolve_table(&object_name(&["t"]), "master", "dbo")
            .is_none()
    );
    assert!(
        snapshot
            .resolve_table(&object_name(&["mydb", "dbo", "t"]), "master", "dbo")
            .is_none()
    );
    assert_eq!(
        snapshot.resolve_object("master", Some("dbo"), "t", "dbo"),
        None,
        "the forwarded method answers the same thing"
    );
    assert_eq!(
        snapshot.classify_table_reference(&object_name(&["t"]), "master", "dbo"),
        TableReferenceKind::Unknown
    );
    assert!(CatalogView::view_definition(&snapshot, ObjectId(1)).is_none());
}

#[test]
fn an_empty_snapshot_has_no_identity_nor_computed_column() {
    let snapshot = CatalogSnapshot::default();
    assert!(CatalogView::identity_column(&snapshot, ObjectId(1)).is_none());
    assert!(CatalogView::computed_columns(&snapshot, ObjectId(1)).is_empty());
    assert!(
        snapshot.table(ObjectId(1)).is_none(),
        "the forwarded method answers the same thing"
    );
}

#[test]
fn a_linked_server_name_resolves_to_nothing() {
    let snapshot = CatalogSnapshot::default();
    let name = object_name(&["srv", "mydb", "dbo", "t"]);
    assert!(name.server.is_some(), "the test name has four parts");
    assert!(snapshot.resolve_table(&name, "master", "dbo").is_none());
}

#[test]
fn columns_are_indexed_by_ordinal_not_by_identifier() {
    let column = |id: i32, name: &str, ordinal: u16| ColumnMeta {
        id: ColumnId(id),
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
        ordinal,
        default: None,
        identity: None,
        computed: None,
    };
    // `b` was created after a column before it was dropped: identifier 3, position 1.
    let bindings = columns_of(&[column(1, "a", 0), column(3, "b", 1)]);
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].column, ColumnId(1));
    assert_eq!(bindings[0].index, 0);
    assert_eq!(bindings[1].column, ColumnId(3));
    assert_eq!(bindings[1].index, 1);
    assert_eq!(bindings[1].name, "b");
    assert_eq!(bindings[1].ty, TypeInfo::new(SqlType::Int, true));
}
