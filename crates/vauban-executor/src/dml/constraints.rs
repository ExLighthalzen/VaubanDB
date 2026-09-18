//! `PRIMARY KEY`, `UNIQUE` and `NOT NULL` at write time: the 2627 and 2601 of a duplicate
//! key, named after the catalogue rather than after the identifiers `storage` reports.
//!
//! # A uniqueness failure leaves `storage` unnamed
//!
//! The uniqueness barrier is `storage`, which maintains the indexes without a catalogue:
//! the 2601 it raises names the table and the index by their decimal identifiers. A caller
//! that holds the [`TableMeta`] and a [`CatalogSnapshot`] turns that error into the one a
//! client expects:
//!
//! - the index of a `PRIMARY KEY` or of a `UNIQUE` constraint becomes **2627**, whose text
//!   names the kind of constraint, the constraint, the object and the duplicate key;
//! - an index a `CREATE UNIQUE INDEX` built becomes **2601**, whose text names the object,
//!   the index and the duplicate key.
//!
//! [`translate_unique`] is that translation. An error of another number, and a 2601 whose
//! index the snapshot does not hold, come back untouched: it rephrases, it does not check
//! twice.
//!
//! # The duplicate key as text
//!
//! The key is written between parentheses, its values separated by `, `, a value as
//! `CONVERT(varchar, value)` writes it, and a `NULL` as `<NULL>`: `(1)`, `(1, x y)`,
//! `(2024-01-02)`, `(<NULL>)`. [`key_text`] reads the key columns of the index out of the row
//! the statement wrote and renders them that way. The verb of the statement is not part of the
//! 2627 or the 2601 text, so [`translate_unique`] takes it and does not read it.
//!
//! # The wait that is not taken
//!
//! `storage` answers the duplicate at once. SQL Server makes the second writer wait on the
//! lock of the first and raises the error when that lock is released, or raises nothing when
//! the first rolls back. The awaiting form belongs to the executor write lock and is not
//! implemented yet; the difference is the `unique-conflict-no-wait` gap.

use vauban_catalog::{CatalogSnapshot, ConstraintMeta, IndexMeta, TableMeta};
use vauban_errors::SqlError;
use vauban_storage::IndexId;
use vauban_types::{TypeInfo, Value, default_display};

use crate::row::Row;

/// Rephrases the 2601 `storage` raises for a duplicate key into the number and the names a
/// client expects, and leaves another error untouched.
///
/// The identifier of the violated index is read out of the `storage` message, the index is
/// found in `snap` by that identifier, and the constraint that backs it — when one does —
/// decides between 2627 and 2601. `row` is the row the statement wrote, from which
/// [`key_text`] renders the duplicate key. `statement` is the verb of the failing statement;
/// the 2627 and 2601 texts of SQL Server carry it in neither, so it is not read.
pub(crate) fn translate_unique(
    err: SqlError,
    table: &TableMeta,
    snap: &CatalogSnapshot,
    row: &Row,
    statement: &str,
) -> SqlError {
    if err.number != 2601 {
        return err;
    }
    let Some(id) = storage_index_id(&err.message) else {
        return err;
    };
    let indexes = snap.indexes_of(table.id);
    let Some(index) = indexes.iter().find(|index| index.id == id) else {
        return err;
    };
    let _ = statement;
    let object = format!("{}.{}", table.schema, table.name);
    let key = key_text(index, table, row);
    match backed_constraint(table, id) {
        Some(kind) => SqlError::unique_violation(kind, &index.name, &object, &key),
        None => SqlError::duplicate_key_index(&object, &index.name, &key),
    }
}

/// The identifier of the index the `storage` 2601 names, read out of its message.
///
/// `storage` writes the identifier as a bare decimal integer between the quotes of
/// `for unique index '<id>'`. A message that does not carry that shape — another number, or a
/// 2601 whose text names something other than an identifier — answers `None`.
fn storage_index_id(message: &str) -> Option<IndexId> {
    const MARKER: &str = "for unique index '";
    let rest = message.split_once(MARKER)?.1;
    let digits = rest.split('\'').next()?;
    digits.parse::<u32>().ok().map(IndexId)
}

/// `PRIMARY KEY` or `UNIQUE KEY` when `index` backs a constraint of `table`, `None` for the
/// index a `CREATE UNIQUE INDEX` built.
fn backed_constraint(table: &TableMeta, index: IndexId) -> Option<&'static str> {
    table
        .constraints
        .iter()
        .find_map(|constraint| match constraint {
            ConstraintMeta::PrimaryKey(id) if *id == index => Some("PRIMARY KEY"),
            ConstraintMeta::Unique(id) if *id == index => Some("UNIQUE KEY"),
            _ => None,
        })
}

/// The duplicate key of `index` as SQL Server displays it: `(v1, v2)`.
///
/// The values are read from `row` at the ordinals of the key columns and rendered with
/// [`default_display`] under the type of their column; a `NULL` is written `<NULL>`.
fn key_text(index: &IndexMeta, table: &TableMeta, row: &Row) -> String {
    let values: Vec<String> = index
        .columns
        .iter()
        .map(|key| {
            let ordinal = usize::from(key.column);
            match row.get(ordinal) {
                Some(value) => match column_type(table, ordinal) {
                    Some(ty) => render(value, ty),
                    None => format!("{value:?}"),
                },
                None => "<NULL>".to_owned(),
            }
        })
        .collect();
    format!("({})", values.join(", "))
}

/// A key value rendered as the duplicate-key text writes it: `<NULL>` for `NULL`, and
/// [`default_display`] otherwise.
fn render(value: &Value, ty: &TypeInfo) -> String {
    match value {
        Value::Null => "<NULL>".to_owned(),
        value => default_display(value, ty),
    }
}

/// The type of the column of `table` at the row ordinal `ordinal`, `None` when the table
/// does not declare a column there.
fn column_type(table: &TableMeta, ordinal: usize) -> Option<&TypeInfo> {
    table
        .columns
        .iter()
        .find(|column| usize::from(column.ordinal) == ordinal)
        .map(|column| &column.ty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use vauban_catalog::{
        Catalog, ColumnDef, ConstraintDef, QualifiedName, SortedColumn, TableDef,
    };
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo};

    /// A catalogue holding `dbo.t (id int NOT NULL PRIMARY KEY)`, its transaction manager
    /// and the metadata of the table.
    fn table_with_a_primary_key() -> (Arc<TransactionManager>, Catalog, vauban_catalog::TableMeta) {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let meta = catalog
            .create_table(
                &handle,
                &TableDef {
                    name: QualifiedName {
                        database: "master".to_owned(),
                        schema: "dbo".to_owned(),
                        name: "t".to_owned(),
                    },
                    columns: vec![ColumnDef {
                        name: "id".to_owned(),
                        ty: TypeInfo::new(SqlType::Int, false),
                        default: None,
                        identity: None,
                        computed: None,
                    }],
                    constraints: vec![ConstraintDef::PrimaryKey {
                        name: Some("pk_t".to_owned()),
                        columns: vec![SortedColumn {
                            column: "id".to_owned(),
                            descending: false,
                        }],
                        clustered: true,
                    }],
                },
            )
            .expect("create_table");
        manager.commit(handle).expect("commit");
        (manager, catalog, meta)
    }

    #[test]
    fn unrelated_storage_error_is_not_rewritten() {
        let (manager, catalog, meta) = table_with_a_primary_key();
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snap = catalog.snapshot(&handle);
        let row = vec![Value::I32(1)];
        // The index identifier of the primary key: the disguised message names it, so that
        // without the filter on the number `translate_unique` would rephrase the 515 into a
        // 2627.
        let id = snap.indexes_of(meta.id)[0].id;
        let disguised = SqlError::new(
            515,
            16,
            2,
            format!(
                "Cannot insert the value NULL into column 'id', table 'master.dbo.t'; column \
                 does not allow nulls. for unique index '{id}': the value (1) exists already."
            ),
        );
        let translated = translate_unique(disguised.clone(), &meta, &snap, &row, "INSERT");
        assert_eq!(translated.number, 515);
        assert_eq!(translated.message, disguised.message);
    }
}
