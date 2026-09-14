//! The rows the internal tables of `master` carry about a user table: written when
//! `CREATE TABLE` builds its final [`TableMeta`], rewritten when an index of the table is
//! created or dropped, removed when the table is dropped.
//!
//! # What this file adds to the files of `views/`
//!
//! Those files describe the shape of each internal table and the function that turns a
//! `&[TableMeta]` — with the [`IndexStore`] beside it for the index views — into the [`Row`]s
//! of that table. This file is the caller: it maps each internal table to its row
//! constructor and to the way a row of it names the table it describes ([`Owner`]), so that
//! [`write`], [`remove`] and [`rewrite`] walk one list.
//!
//! # Transaction
//!
//! The rows are inserted and deleted inside the caller's transaction, as `database.rs` does
//! for the row of a database (section "Transaction" there) and not as a commit action.
//! They are ordinary versioned rows, so a `ROLLBACK` takes back what a `CREATE TABLE` wrote
//! (`tests/sys_rows.rs`, `a_rolled_back_create_table_leaves_no_row`) and the rows of a
//! `DROP TABLE` stay readable by the other transactions until the `COMMIT`, which is what the
//! deferred `CommitAction::DropTable` of `table.rs` does for the table itself
//! (`tests/sys_rows.rs`, `drop_table_removes_every_row_of_the_table`,
//! `another_transaction_reads_the_rows_until_the_commit_of_the_drop`).
//!
//! # Removing a row without a scan key
//!
//! `storage` deletes by [`RowId`], so removal is a filtered scan. Most internal tables carry
//! `database_id` and an `object_id` naming the table, which is the pair [`Owner::Object`]
//! matches; `vauban_is_tables` and `vauban_is_columns` carry the schema and the name instead
//! ([`Owner::Named`]), and `vauban_sys_allocation_units` names its table through the
//! `container_id` of the partition it holds ([`Owner::Container`]), read from the partition
//! rows of the table before they are deleted ([`containers`]).
//!
//! # Bound: what this file does not write
//!
//! The four tables of `views/sys_constraints.rs` are not in the list of [`rows_of`]: the
//! objects of a `FOREIGN KEY`, a `CHECK` and a `DEFAULT` live in the store of
//! `constraints.rs` and are not written into `storage`. The rows are not written again when
//! the instance restarts either — the store of `table.rs` is in memory and is not reloaded —
//! and `ALTER TABLE` is a stub, so no caller of this file changes the columns of a table that
//! already has rows.

use std::collections::BTreeSet;

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::{DbId, Row, RowId, TableId};
use vauban_txn::TxnHandle;
use vauban_types::Value;

use crate::bootstrap::internal_table_id;
use crate::catalog::Catalog;
use crate::index::IndexStore;
use crate::meta::TableMeta;
use crate::views::{info_schema, sys_extra, sys_indexes, sys_tables};

/// How a row of an internal table names the table it describes, which is what tells the rows
/// to delete from the rows of the other tables of the instance.
#[derive(Debug, Clone, Copy)]
enum Owner {
    /// The row carries the `database_id` and an `object_id` equal to
    /// [`TableMeta::id`]: the two positions are `(database, object)`.
    Object(usize, usize),
    /// The row carries the `database_id`, the schema and the name of the table: the three
    /// positions are `(database, schema, name)`. The shape of `vauban_is_tables` and of
    /// `vauban_is_columns`, which publish names and no identifier.
    Named(usize, usize, usize),
    /// The row carries the `database_id` and the `container_id` of a partition of the table:
    /// the two positions are `(database, container)`. The shape of
    /// `vauban_sys_allocation_units`.
    Container(usize, usize),
}

impl Owner {
    /// Whether `row` describes `table`, which sits in the database published as `database`
    /// and holds the partitions `containers` lists.
    ///
    /// A row too short for the positions of the variant answers `false` rather than an
    /// error: the caller is deleting, and a row it cannot read is a row it did not write
    /// (unit test `a_row_shorter_than_its_positions_is_not_matched`).
    fn matches(
        self,
        row: &[Value],
        table: &TableMeta,
        database: i32,
        containers: &BTreeSet<i64>,
    ) -> bool {
        match self {
            Owner::Object(db, object) => {
                row.get(db) == Some(&Value::I32(database))
                    && row.get(object) == Some(&Value::I32(table.id.0))
            }
            Owner::Named(db, schema, name) => {
                row.get(db) == Some(&Value::I32(database))
                    && text_of(row.get(schema)) == Some(table.schema.as_str())
                    && text_of(row.get(name)) == Some(table.name.as_str())
            }
            Owner::Container(db, container) => {
                row.get(db) == Some(&Value::I32(database))
                    && matches!(row.get(container), Some(&Value::I64(id)) if containers.contains(&id))
            }
        }
    }
}

/// The text of a `nvarchar` value, `None` for anything else.
fn text_of(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(text)) => Some(text.text.as_str()),
        _ => None,
    }
}

/// One internal table, the way its rows name their table, and the rows `table` asks for.
struct Rows {
    /// Name of the internal table, as `bootstrap.rs` created it in `master`.
    name: &'static str,
    /// How a row of that table names the table it describes.
    owner: Owner,
    /// The rows the constructor of `views/` built for the table.
    rows: Vec<Row>,
}

/// The rows each internal table carries about `table`, in the order of the files of `views/`.
///
/// The list is the one both [`write`] and [`remove`] walk: a table added here is written at
/// `CREATE` and removed at `DROP` by that single edit (unit test
/// `every_internal_table_of_the_list_is_one_the_bootstrap_created` checks the twelve names
/// against `bootstrap::internal_table_defs`).
///
/// # Errors
///
/// The error of a row constructor of `views/`.
fn rows_of(table: &TableMeta, indexes: &IndexStore) -> SqlResult<Vec<Rows>> {
    let one = std::slice::from_ref(table);
    Ok(vec![
        Rows {
            name: sys_tables::OBJECTS_TABLE,
            owner: Owner::Object(
                sys_tables::objects_columns::DATABASE_ID,
                sys_tables::objects_columns::OBJECT_ID,
            ),
            rows: sys_tables::object_rows(one)?,
        },
        Rows {
            name: sys_tables::COLUMNS_TABLE,
            owner: Owner::Object(
                sys_tables::columns_columns::DATABASE_ID,
                sys_tables::columns_columns::OBJECT_ID,
            ),
            rows: sys_tables::column_rows(one)?,
        },
        Rows {
            name: sys_indexes::INDEXES_TABLE,
            owner: Owner::Object(
                sys_indexes::indexes_columns::DATABASE_ID,
                sys_indexes::indexes_columns::OBJECT_ID,
            ),
            rows: sys_indexes::index_rows(one, indexes)?,
        },
        Rows {
            name: sys_indexes::INDEX_COLUMNS_TABLE,
            owner: Owner::Object(
                sys_indexes::index_columns_columns::DATABASE_ID,
                sys_indexes::index_columns_columns::OBJECT_ID,
            ),
            rows: sys_indexes::index_column_rows(one, indexes)?,
        },
        Rows {
            name: sys_indexes::KEY_CONSTRAINTS_TABLE,
            // A key constraint is an object of its own; the table it belongs to is its
            // `parent_object_id`, there being no `object_id` column in that internal table.
            owner: Owner::Object(
                sys_indexes::key_constraints_columns::DATABASE_ID,
                sys_indexes::key_constraints_columns::PARENT_OBJECT_ID,
            ),
            rows: sys_indexes::key_constraint_rows(one, indexes)?,
        },
        Rows {
            name: sys_indexes::IDENTITY_COLUMNS_TABLE,
            owner: Owner::Object(
                sys_indexes::identity_columns_columns::DATABASE_ID,
                sys_indexes::identity_columns_columns::OBJECT_ID,
            ),
            rows: sys_indexes::identity_column_rows(one)?,
        },
        Rows {
            name: info_schema::TABLES_TABLE,
            owner: Owner::Named(
                info_schema::tables_columns::DATABASE_ID,
                info_schema::tables_columns::TABLE_SCHEMA,
                info_schema::tables_columns::TABLE_NAME,
            ),
            rows: info_schema::table_rows(one)?,
        },
        Rows {
            name: info_schema::COLUMNS_TABLE,
            owner: Owner::Named(
                info_schema::columns_columns::DATABASE_ID,
                info_schema::columns_columns::TABLE_SCHEMA,
                info_schema::columns_columns::TABLE_NAME,
            ),
            rows: info_schema::column_rows(one)?,
        },
        Rows {
            name: sys_extra::ALL_OBJECTS_TABLE,
            owner: Owner::Object(
                sys_extra::objects_columns::DATABASE_ID,
                sys_extra::objects_columns::OBJECT_ID,
            ),
            rows: sys_extra::user_object_rows(one)?,
        },
        Rows {
            name: sys_extra::ALL_COLUMNS_TABLE,
            // The table of `sys.all_columns` has the shape of the one of `sys.columns`
            // (`views/sys_extra.rs`), so its positions are those of `views/sys_tables.rs`.
            owner: Owner::Object(
                sys_tables::columns_columns::DATABASE_ID,
                sys_tables::columns_columns::OBJECT_ID,
            ),
            rows: sys_extra::user_column_rows(one)?,
        },
        Rows {
            name: sys_extra::PARTITIONS_TABLE,
            owner: Owner::Object(
                sys_extra::partitions_columns::DATABASE_ID,
                sys_extra::partitions_columns::OBJECT_ID,
            ),
            rows: sys_extra::partition_rows(one)?,
        },
        Rows {
            name: sys_extra::ALLOCATION_UNITS_TABLE,
            owner: Owner::Container(
                sys_extra::allocation_units_columns::DATABASE_ID,
                sys_extra::allocation_units_columns::CONTAINER_ID,
            ),
            rows: sys_extra::allocation_unit_rows(one)?,
        },
    ])
}

/// Writes into `txn` the rows the internal tables of [`rows_of`] carry about `table`.
///
/// Called by `create_table` once the [`TableMeta`] is final — the identifiers of the keys of
/// `apply_table_keys` are in `meta.constraints` and in `indexes` — and by [`rewrite`].
///
/// # Errors
///
/// The error of a row constructor, [`InternalError::Bug`] when an internal table of the list
/// is not in this storage ([`table_id`]), or the error of a `storage` call.
pub(crate) fn write(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: &TableMeta,
    indexes: &IndexStore,
) -> SqlResult<()> {
    for described in rows_of(table, indexes)? {
        let id = table_id(catalog, described.name)?;
        for row in &described.rows {
            catalog.storage.insert(txn.id, id, row)?;
        }
    }
    Ok(())
}

/// Deletes in `txn` the rows the internal tables of [`rows_of`] carry about `table`.
///
/// The rows deleted are those an [`Owner`] matches, not those [`rows_of`] would build now:
/// what a `CREATE TABLE` wrote is removed even when the shape of the table has moved since,
/// which is what makes [`rewrite`] safe to call after the index store has changed.
///
/// # Errors
///
/// Those of [`write`], plus the error of the scan.
pub(crate) fn remove(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: &TableMeta,
    indexes: &IndexStore,
) -> SqlResult<()> {
    let database = database_id(table.database)?;
    let containers = containers(catalog, txn, table, database)?;
    for described in rows_of(table, indexes)? {
        let id = table_id(catalog, described.name)?;
        for (row_id, values) in scan(catalog, txn, id)? {
            if described
                .owner
                .matches(&values.0, table, database, &containers)
            {
                catalog.storage.delete(txn.id, id, row_id)?;
            }
        }
    }
    Ok(())
}

/// Deletes the rows of `table` and writes them again from the state the stores hold now.
///
/// What `create_index` and `drop_index` call: an index changes the rows of `sys.indexes`,
/// `sys.index_columns` and `sys.key_constraints` and renumbers the `index_id` of the indexes
/// that follow it (`views/sys_indexes.rs`, `numbered_indexes`), so the rows of the table are
/// rebuilt rather than patched (`tests/sys_rows.rs`,
/// `create_index_and_drop_index_maintain_sys_indexes`).
///
/// # Errors
///
/// Those of [`remove`] and of [`write`].
pub(crate) fn rewrite(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: &TableMeta,
    indexes: &IndexStore,
) -> SqlResult<()> {
    remove(catalog, txn, table, indexes)?;
    write(catalog, txn, table, indexes)
}

/// The `container_id` of each partition row `txn` sees for `table`, which is what
/// [`Owner::Container`] matches an allocation unit against.
///
/// Read before any deletion, the partition rows being deleted by the same [`remove`].
///
/// # Errors
///
/// Those of [`table_id`] and of the scan.
fn containers(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: &TableMeta,
    database: i32,
) -> SqlResult<BTreeSet<i64>> {
    let partitions = table_id(catalog, sys_extra::PARTITIONS_TABLE)?;
    let owner = Owner::Object(
        sys_extra::partitions_columns::DATABASE_ID,
        sys_extra::partitions_columns::OBJECT_ID,
    );
    let mut found = BTreeSet::new();
    for (_, values) in scan(catalog, txn, partitions)? {
        if !owner.matches(&values.0, table, database, &found) {
            continue;
        }
        if let Some(&Value::I64(id)) = values.0.get(sys_extra::partitions_columns::PARTITION_ID) {
            found.insert(id);
        }
    }
    Ok(found)
}

/// The rows of the internal table `id` that `txn` sees, with their [`RowId`].
///
/// The snapshot is taken at the call, as `database.rs` takes it: the rows `txn` wrote itself
/// are there, which is what lets a `CREATE TABLE` and the `DROP TABLE` of the same
/// transaction meet (`tests/sys_rows.rs`, `a_table_created_and_dropped_in_one_transaction_leaves_no_row`).
///
/// # Errors
///
/// The error of the scan.
fn scan(catalog: &Catalog, txn: &TxnHandle, id: TableId) -> SqlResult<Vec<(RowId, Row)>> {
    let snapshot = catalog.txn.statement_snapshot(txn);
    let mut rows = Vec::new();
    for row in catalog.storage.scan(&snapshot, id)? {
        rows.push(row?);
    }
    Ok(rows)
}

/// The [`TableId`] of the internal table called `name`.
///
/// # Errors
///
/// [`InternalError::Bug`] when the storage carries no such table, which means the catalogue
/// was not bootstrapped, as `database.rs` reports the same case.
fn table_id(catalog: &Catalog, name: &str) -> SqlResult<TableId> {
    internal_table_id(catalog, name)?.ok_or_else(|| {
        InternalError::Bug(format!("internal table {name} is not in this storage")).into()
    })
}

/// The `int` a [`DbId`] is published as, as `views/sys_tables.rs` publishes it.
///
/// # Errors
///
/// [`InternalError::Bug`] when the identifier does not fit in an `int`.
fn database_id(id: DbId) -> SqlResult<i32> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "Catalog: database id {id} does not fit in the int the views publish"
        ))
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::internal_table_defs;
    use crate::ids::{ColumnId, ObjectId};
    use vauban_types::{SqlString, SqlType, TypeInfo};

    /// A table with one column, enough to build an [`Owner`] match.
    fn one_table() -> TableMeta {
        TableMeta {
            id: ObjectId(1_000_000),
            storage_id: TableId(7),
            database: DbId(1),
            schema: "dbo".to_owned(),
            name: "t".to_owned(),
            columns: vec![crate::meta::ColumnMeta {
                id: ColumnId(1),
                name: "a".to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                ordinal: 0,
                default: None,
                identity: None,
                computed: None,
            }],
            clustered: None,
            constraints: Vec::new(),
        }
    }

    #[test]
    fn every_internal_table_of_the_list_is_one_the_bootstrap_created() {
        let described: Vec<String> = internal_table_defs()
            .expect("the internal tables")
            .into_iter()
            .map(|def| def.name)
            .collect();
        let listed = rows_of(&one_table(), &IndexStore::default()).expect("the rows");
        assert_eq!(listed.len(), 12, "the twelve tables of the module list");
        for table in &listed {
            assert!(
                described.contains(&table.name.to_owned()),
                "{} is not an internal table: {described:?}",
                table.name
            );
        }
    }

    #[test]
    fn a_row_shorter_than_its_positions_is_not_matched() {
        let table = one_table();
        let empty = BTreeSet::new();
        assert!(!Owner::Object(0, 1).matches(&[], &table, 1, &empty));
        assert!(!Owner::Named(0, 1, 2).matches(&[Value::I32(1)], &table, 1, &empty));
        assert!(!Owner::Container(0, 2).matches(&[Value::I32(1)], &table, 1, &empty));
    }

    #[test]
    fn an_owner_matches_the_database_and_the_table_of_the_row() {
        let table = one_table();
        let empty = BTreeSet::new();
        let owner = Owner::Object(0, 1);
        let row = [Value::I32(1), Value::I32(1_000_000)];
        assert!(owner.matches(&row, &table, 1, &empty));
        // Another database, or another table of the same database, is not this table.
        assert!(!owner.matches(&row, &table, 2, &empty));
        assert!(!owner.matches(&[Value::I32(1), Value::I32(1_000_001)], &table, 1, &empty));
    }

    #[test]
    fn a_named_owner_matches_the_schema_and_the_name() {
        let table = one_table();
        let empty = BTreeSet::new();
        let owner = Owner::Named(0, 1, 2);
        let row = |schema: &str, name: &str| {
            [
                Value::I32(1),
                Value::String(SqlString {
                    text: schema.to_owned(),
                }),
                Value::String(SqlString {
                    text: name.to_owned(),
                }),
            ]
        };
        assert!(owner.matches(&row("dbo", "t"), &table, 1, &empty));
        assert!(!owner.matches(&row("sys", "t"), &table, 1, &empty));
        assert!(!owner.matches(&row("dbo", "u"), &table, 1, &empty));
    }

    #[test]
    fn a_container_owner_matches_the_partitions_it_was_given() {
        let table = one_table();
        let owner = Owner::Container(0, 2);
        let row = [Value::I32(1), Value::I64(9), Value::I64(4)];
        assert!(owner.matches(&row, &table, 1, &BTreeSet::from([4])));
        assert!(!owner.matches(&row, &table, 1, &BTreeSet::from([5])));
        assert!(!owner.matches(&row, &table, 1, &BTreeSet::new()));
    }

    #[test]
    fn a_table_of_a_database_beyond_an_int_is_a_bug() {
        let err = database_id(DbId(u32::MAX)).expect_err("2^32 - 1 does not fit in an int");
        assert!(
            err.message
                .contains("does not fit in the int the views publish"),
            "{}",
            err.message
        );
    }
}
