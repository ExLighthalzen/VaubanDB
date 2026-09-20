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

use vauban_catalog::{
    CatalogSnapshot, ColumnId, ColumnMeta, ConstraintMeta, IndexMeta, ObjectId, ObjectKind,
};
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

    /// The column of `object` whose [`ColumnMeta::identity`] is set, read from
    /// [`CatalogSnapshot::table`]; `None` on a snapshot that holds no such table (unit test
    /// `an_empty_snapshot_has_no_identity_nor_computed_column`).
    fn identity_column(&self, object: ObjectId) -> Option<ColumnId> {
        self.table(object)?
            .columns
            .iter()
            .find(|column| column.identity.is_some())
            .map(|column| column.id)
    }

    /// The columns of `object` whose [`ColumnMeta::computed`] is set, read from
    /// [`CatalogSnapshot::table`].
    fn computed_columns(&self, object: ObjectId) -> Vec<ColumnId> {
        self.table(object).map_or_else(Vec::new, |meta| {
            meta.columns
                .iter()
                .filter(|column| column.computed.is_some())
                .map(|column| column.id)
                .collect()
        })
    }

    /// Names of the constraints on `table`: index-backed `PRIMARY KEY` and `UNIQUE`, then
    /// `FOREIGN KEY`, `CHECK` and `DEFAULT` objects.
    ///
    /// The three non-key kinds are named through [`CatalogSnapshot::constraint_name`] and not
    /// [`CatalogSnapshot::object_name`]: a constraint is not an entry of `objects`
    /// ([`CatalogSnapshot`] module documentation), whether it was declared in the
    /// `CREATE TABLE` or added by a later `ALTER TABLE … ADD` (`bind_alter_table.rs`,
    /// `drop_constraint_binds_a_create_table_foreign_key`,
    /// `drop_constraint_binds_an_added_check`).
    fn constraint_names_on_table(&self, table: ObjectId) -> Vec<String> {
        let Some(meta) = self.table(table) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for index in self.indexes_of(table) {
            if meta.constraints.iter().any(|constraint| {
                matches!(
                    constraint,
                    ConstraintMeta::PrimaryKey(id) | ConstraintMeta::Unique(id) if *id == index.id
                )
            }) {
                names.push(index.name.clone());
            }
        }
        for constraint in &meta.constraints {
            let object = match constraint {
                ConstraintMeta::ForeignKey { constraint, .. }
                | ConstraintMeta::Check { constraint, .. }
                | ConstraintMeta::Default { constraint, .. } => *constraint,
                _ => continue,
            };
            if let Some(qualified) = self.constraint_name(object) {
                names.push(qualified.name);
            }
        }
        names
    }

    /// Key columns of the `PRIMARY KEY` of `table`, read from [`CatalogSnapshot::indexes_of`]
    /// and the `PRIMARY KEY` entry of the table constraints.
    fn primary_key_columns(&self, table: ObjectId) -> Option<Vec<String>> {
        let meta = self.table(table)?;
        let pk_id = meta
            .constraints
            .iter()
            .find_map(|constraint| match constraint {
                ConstraintMeta::PrimaryKey(id) => Some(*id),
                _ => None,
            })?;
        let pk_index = self
            .indexes_of(table)
            .iter()
            .find(|index| index.id == pk_id)?;
        key_column_names(meta, pk_index)
    }

    /// Whether `table` declares a `UNIQUE` key exactly on `columns`, in order.
    fn unique_key_on(&self, table: ObjectId, columns: &[String]) -> bool {
        let Some(meta) = self.table(table) else {
            return false;
        };
        if columns.is_empty() {
            return false;
        }
        self.indexes_of(table).iter().any(|index| {
            index.unique
                && index.columns.len() == columns.len()
                && key_matches_names(meta, index, columns)
        })
    }
}

/// The names of the key columns of `index` on `table`, in key order.
fn key_column_names(meta: &vauban_catalog::TableMeta, index: &IndexMeta) -> Option<Vec<String>> {
    index
        .columns
        .iter()
        .map(|key| {
            meta.columns
                .iter()
                .find(|column| column.ordinal == key.column)
                .map(|column| column.name.clone())
        })
        .collect()
}

/// Whether the key columns of `index` match `names`, in order, case-insensitively.
fn key_matches_names(
    meta: &vauban_catalog::TableMeta,
    index: &IndexMeta,
    names: &[String],
) -> bool {
    index.columns.len() == names.len()
        && index.columns.iter().zip(names.iter()).all(|(key, name)| {
            meta.columns
                .iter()
                .find(|column| column.ordinal == key.column)
                .is_some_and(|column| column.name.eq_ignore_ascii_case(name))
        })
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
            depth: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests;
