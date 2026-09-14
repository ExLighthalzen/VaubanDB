//! `impl CatalogView for CatalogSnapshot`: the binder reads a snapshot of the catalogue.
//!
//! The implementation lives in `binder` and not in `catalog` because of the orphan rule:
//! [`CatalogView`] is declared here, [`CatalogSnapshot`] is declared there, and the `impl`
//! belongs to a crate that declares one of the two. Making `catalog` depend on `binder` for
//! it would reverse the dependency of the whole engine.
//!
//! # What this file does
//!
//! Each method forwards to the method of [`CatalogSnapshot`] that answers it
//! ([`CatalogSnapshot::resolve_object`], [`CatalogSnapshot::table`],
//! [`CatalogSnapshot::view_definition`]) and converts the result into the types of the
//! binder.

use vauban_catalog::{CatalogSnapshot, ColumnMeta, ObjectId, ObjectKind};
use vauban_parser::ObjectName;

use crate::bound::ColumnBinding;
use crate::context::{CatalogView, ResolvedTable, ResolvedTableKind};

impl CatalogView for CatalogSnapshot {
    /// Resolves the written name against this snapshot.
    ///
    /// A four-part name (`srv.db.dbo.t`) resolves to nothing here: a linked server is not
    /// served by VaubanDB, and the binder reports its own error rather than pretending the
    /// object is local (unit test `a_linked_server_name_resolves_to_nothing`). A name that
    /// reaches a constraint or an index is not a table reference either.
    fn resolve_table(
        &self,
        name: &ObjectName,
        database: &str,
        default_schema: &str,
    ) -> Option<ResolvedTable> {
        if name.server.is_some() {
            return None;
        }
        let db = name
            .database
            .as_ref()
            .map_or(database, |ident| ident.value.as_str());
        let schema = name.schema.as_ref().map(|ident| ident.value.as_str());
        let object = self.resolve_object(db, schema, &name.name.value, default_schema)?;
        let (object_id, kind) = match object.kind {
            ObjectKind::Table => (object.id, ResolvedTableKind::Table),
            ObjectKind::View => (object.id, ResolvedTableKind::View),
            ObjectKind::Constraint | ObjectKind::Index => return None,
        };
        let (table, columns) = match kind {
            ResolvedTableKind::Table => {
                let meta = self.table(object_id)?;
                (Some(meta.storage_id), columns_of(&meta.columns))
            }
            // A view has no row of its own: `view.rs` expands `view_definition` into the plan.
            ResolvedTableKind::View => (None, Vec::new()),
        };
        Some(ResolvedTable {
            object: object_id,
            table,
            columns,
            kind,
        })
    }

    /// The T-SQL text of a view, straight from [`CatalogSnapshot::view_definition`].
    fn view_definition(&self, object: ObjectId) -> Option<&str> {
        CatalogSnapshot::view_definition(self, object)
    }
}

/// The columns of a table as the binder names them, in the order of the row of `storage`.
///
/// `index` is the `ordinal` of the column — its position in the row `storage` hands out —
/// and not its [`ColumnId`](vauban_catalog::ColumnId), which keeps its value when a column
/// before it is dropped (unit test `columns_are_indexed_by_ordinal_not_by_identifier`).
fn columns_of(columns: &[ColumnMeta]) -> Vec<ColumnBinding> {
    columns
        .iter()
        .map(|column| ColumnBinding {
            column: column.id,
            index: usize::from(column.ordinal),
            name: column.name.clone(),
            ty: column.ty.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TableReferenceKind;
    use vauban_catalog::ColumnId;
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
}
