//! Opening a catalogue over a storage that already holds user tables: the [`TableStore`] is
//! rebuilt from the committed rows the internal tables of `master` carry about each table.
//!
//! # What is read, and from where
//!
//! | Part of the metadata | Internal table |
//! |---|---|
//! | name of the table, `max_column_id_used` | `vauban_sys_objects` |
//! | name, type, length, precision, scale, nullability of a column | `vauban_sys_columns` |
//! | seed and increment of an `IDENTITY` column | `vauban_sys_identity_columns` |
//! | name, kind and key of an index, the `PRIMARY KEY` and `UNIQUE` it backs | `vauban_sys_indexes`, `vauban_sys_index_columns` |
//! | `FOREIGN KEY` | `vauban_sys_foreign_keys`, `vauban_sys_foreign_key_columns` |
//! | `CHECK` and `DEFAULT`, by their stored text | `vauban_sys_check_constraints`, `vauban_sys_default_constraints` |
//! | schema of the table, [`TableId`] of the table, [`IndexId`] of each index, largest `column_id` handed out | [`STORAGE_TABLE`], this file |
//! | position in the row, collation, text of a computed column | [`COLUMNS_TABLE`], this file |
//! | largest object identifier handed out | [`HIGH_WATER_TABLE`], this file |
//!
//! The tables of this file are written by `sys_rows.rs` with the others, in the transaction
//! of the statement, from [`storage_rows`], [`column_rows`] and [`record_high_water`]. They
//! carry what the published tables do not: `sys.objects.schema_id` is `0` for a schema the
//! instance does not hold, the row of `vauban_sys_columns` carries the default collation name
//! on a character column declared with another collation (`views/sys_tables.rs`,
//! `column_row`), `sys.tables.max_column_id_used` counts the columns the table still holds,
//! and the views do not publish the identifiers `storage` gives a table and an index.
//!
//! A `DEFAULT`, a `CHECK` and a computed column come back as the tree
//! [`parse_stored_expression`] reads from their stored text, the tree `create_table` holds
//! as well (`meta.rs`, `stored_expression`), so a reopened catalogue holds the table it wrote
//! (unit test `a_reopened_catalogue_holds_the_tables_it_wrote`).
//!
//! # What is left out
//!
//! A table whose row of [`STORAGE_TABLE`] is missing — a directory written before that table
//! existed — or whose storage table is gone is not rebuilt.
//!
//! A part the rows do not describe in full is left out with a warning in the log, and the
//! opening goes on: an index or a constraint that cannot be rebuilt is dropped from the table,
//! which stays readable and writable; a table whose own rows cannot be read — its name, a
//! column, a type — is dropped from the store, and the other tables are opened. The first
//! choice keeps the rows of a table reachable when one of its constraints names an index
//! that has gone (a `FOREIGN KEY` over a `UNIQUE INDEX` later dropped): the table answers
//! instead of `208`, and the constraint is not enforced until it is created again (unit test
//! `a_foreign_key_whose_index_has_gone_is_left_out`).
//!
//! # The identifier counter
//!
//! The counter of the store is moved past the largest identifier the rows hold — tables,
//! constraints, the rows of [`HIGH_WATER_TABLE`] — and past the largest key of the counters of
//! `identity.rs`, whose row stays when its table is dropped: an identifier handed out after
//! the restart is none of those, a dropped table's included (unit test
//! `the_counter_starts_past_every_stored_identifier`).

use std::collections::BTreeMap;

use vauban_errors::{InternalError, SqlResult};
use vauban_parser::RefAction;
use vauban_storage::{DbId, IndexId, KeyColumn, Row, Snapshot, TableId, TxnId};
use vauban_txn::TxnHandle;
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

use super::{TableEntry, TableStore};
use crate::bootstrap::{
    DATABASES_TABLE, SYSTEM_DATABASES, databases_columns, internal_table_id,
    registered_internal_tables, registry_snapshot,
};
use crate::catalog::Catalog;
use crate::def::{InternalColumnDef, InternalTableDef};
use crate::identity::COUNTERS_TABLE;
use crate::ids::{ColumnId, ObjectId};
use crate::index::{IndexEntry, IndexStore};
use crate::meta::{
    ColumnMeta, ConstraintMeta, ExpressionKind, IdentitySpec, IndexMeta, ObjectKind, ObjectMeta,
    QualifiedName, TableMeta, definition_text, parse_stored_expression,
};
use crate::views::{sys_constraints, sys_indexes, sys_tables};

/// The internal table that ties a user table and its indexes to `storage`.
pub(crate) const STORAGE_TABLE: &str = "vauban_sys_object_storage";

/// The internal table that carries what `vauban_sys_columns` does not about a column.
pub(crate) const COLUMNS_TABLE: &str = "vauban_sys_column_details";

/// The internal table that keeps the largest object identifier handed out, one row per
/// statement that raised it ([`record_high_water`]).
pub(crate) const HIGH_WATER_TABLE: &str = "vauban_sys_object_high_water";

/// Where each column of [`STORAGE_TABLE`] sits in a [`Row`].
pub(crate) mod storage_columns {
    /// `database_id int`: the database of the table.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `schema_name nvarchar(128)`: the schema of the table, on the row of the table alone.
    pub(crate) const SCHEMA_NAME: usize = 2;
    /// `index_name nvarchar(128)`: `NULL` on the row of the table, the name of the index on
    /// the row of an index.
    pub(crate) const INDEX_NAME: usize = 3;
    /// `storage_id bigint`: the [`TableId`] of the table, or the [`IndexId`] of the index.
    pub(crate) const STORAGE_ID: usize = 4;
    /// `max_column_id_used int`: on the row of the table, the largest `column_id` the table
    /// has handed out, dropped columns included; `NULL` on the row of an index.
    pub(crate) const MAX_COLUMN_ID_USED: usize = 5;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 6;
}

/// Where each column of [`COLUMNS_TABLE`] sits in a [`Row`].
pub(crate) mod column_details_columns {
    /// `database_id int`: the database of the table.
    pub(crate) const DATABASE_ID: usize = 0;
    /// `object_id int`: the table.
    pub(crate) const OBJECT_ID: usize = 1;
    /// `column_id int`: the column, as `vauban_sys_columns` numbers it.
    pub(crate) const COLUMN_ID: usize = 2;
    /// `ordinal smallint`: the position of the column in the row of `storage`, from `0`.
    pub(crate) const ORDINAL: usize = 3;
    /// `collation_lcid int`: the locale of the collation, `NULL` outside the character types.
    pub(crate) const COLLATION_LCID: usize = 4;
    /// `collation_flags tinyint`: the comparison flags of the collation.
    pub(crate) const COLLATION_FLAGS: usize = 5;
    /// `collation_version tinyint`: the version of the collation.
    pub(crate) const COLLATION_VERSION: usize = 6;
    /// `collation_sort_id tinyint`: the sort identifier of the collation.
    pub(crate) const COLLATION_SORT_ID: usize = 7;
    /// `computed_definition nvarchar(max)`: the stored text of a computed column, `NULL` for
    /// a stored column.
    pub(crate) const COMPUTED_DEFINITION: usize = 8;
    /// Width of a row of the table.
    pub(crate) const WIDTH: usize = 9;
}

/// The three internal tables of this file, empty, for the list `bootstrap.rs` creates.
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    let sysname = SqlType::NVarChar(Len::Fixed(128));
    let column = |name: &str, ty: SqlType, nullable: bool| InternalColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    };
    vec![
        InternalTableDef {
            name: STORAGE_TABLE.to_owned(),
            columns: vec![
                column("database_id", SqlType::Int, false),
                column("object_id", SqlType::Int, false),
                column("schema_name", sysname, true),
                column("index_name", sysname, true),
                column("storage_id", SqlType::BigInt, false),
                column("max_column_id_used", SqlType::Int, true),
            ],
            clustered_key: None,
            rows: Vec::new(),
            views: Vec::new(),
        },
        InternalTableDef {
            name: COLUMNS_TABLE.to_owned(),
            columns: vec![
                column("database_id", SqlType::Int, false),
                column("object_id", SqlType::Int, false),
                column("column_id", SqlType::Int, false),
                column("ordinal", SqlType::SmallInt, false),
                column("collation_lcid", SqlType::Int, true),
                column("collation_flags", SqlType::TinyInt, true),
                column("collation_version", SqlType::TinyInt, true),
                column("collation_sort_id", SqlType::TinyInt, true),
                column("computed_definition", SqlType::NVarChar(Len::Max), true),
            ],
            clustered_key: None,
            rows: Vec::new(),
            views: Vec::new(),
        },
        InternalTableDef {
            name: HIGH_WATER_TABLE.to_owned(),
            columns: vec![column("largest_object_id", SqlType::Int, false)],
            clustered_key: None,
            rows: Vec::new(),
            views: Vec::new(),
        },
    ]
}

/// The rows of [`STORAGE_TABLE`] for `table`: the row of the table, which carries
/// `watermark` as its `max_column_id_used`, then one per index of `indexes` that belongs to it.
///
/// # Errors
///
/// [`InternalError::Bug`] when the [`DbId`] of the table does not fit in an `int`.
pub(crate) fn storage_rows(
    table: &TableMeta,
    indexes: &IndexStore,
    watermark: i32,
) -> SqlResult<Vec<Row>> {
    let database = database_id(table.database)?;
    let row = |schema: Value, index: Value, storage: u32| {
        let mut row = vec![Value::Null; storage_columns::WIDTH];
        row[storage_columns::DATABASE_ID] = Value::I32(database);
        row[storage_columns::OBJECT_ID] = Value::I32(table.id.0);
        row[storage_columns::SCHEMA_NAME] = schema;
        row[storage_columns::INDEX_NAME] = index;
        row[storage_columns::STORAGE_ID] = Value::I64(i64::from(storage));
        Row(row)
    };
    let mut rows = vec![row(text(&table.schema), Value::Null, table.storage_id.0)];
    rows[0].0[storage_columns::MAX_COLUMN_ID_USED] = Value::I32(watermark);
    for index in indexes.of_table(table.id) {
        rows.push(row(Value::Null, text(&index.name), index.id.0));
    }
    Ok(rows)
}

/// The rows of [`COLUMNS_TABLE`] for `table`, one per column.
///
/// # Errors
///
/// [`InternalError::Bug`] when the [`DbId`] of the table does not fit in an `int`.
pub(crate) fn column_rows(table: &TableMeta) -> SqlResult<Vec<Row>> {
    use column_details_columns as at;

    let database = database_id(table.database)?;
    Ok(table
        .columns
        .iter()
        .map(|column| {
            let mut row = vec![Value::Null; at::WIDTH];
            row[at::DATABASE_ID] = Value::I32(database);
            row[at::OBJECT_ID] = Value::I32(table.id.0);
            row[at::COLUMN_ID] = Value::I32(column.id.0);
            row[at::ORDINAL] = Value::I16(i16::try_from(column.ordinal).unwrap_or(i16::MAX));
            if let Some(collation) = column.ty.collation {
                row[at::COLLATION_LCID] =
                    Value::I32(i32::try_from(collation.lcid).unwrap_or(i32::MAX));
                row[at::COLLATION_FLAGS] = Value::I8(collation.flags);
                row[at::COLLATION_VERSION] = Value::I8(collation.version);
                row[at::COLLATION_SORT_ID] = Value::I8(collation.sort_id);
            }
            if let Some(computed) = &column.computed {
                row[at::COMPUTED_DEFINITION] = text(&definition_text(computed));
            }
            Row(row)
        })
        .collect())
}

/// The `max_column_id_used` the row of [`STORAGE_TABLE`] that `txn` sees carries for `table`,
/// `None` when there is no such row.
///
/// `sys_rows.rs` reads it before it rewrites the rows of a table, so that the value it writes
/// back does not fall when the column of the largest `column_id` is dropped (unit test
/// `the_column_watermark_survives_a_dropped_column_and_a_reopen`).
///
/// # Errors
///
/// The error of the scan.
pub(crate) fn stored_watermark(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: &TableMeta,
) -> SqlResult<Option<i32>> {
    let database = database_id(table.database)?;
    let snapshot = catalog.txn.statement_snapshot(txn);
    Ok(scan_named(catalog, &snapshot, STORAGE_TABLE)?
        .iter()
        .filter(|row| {
            int(row, storage_columns::DATABASE_ID) == Some(database)
                && int(row, storage_columns::OBJECT_ID) == Some(table.id.0)
                && matches!(row.get(storage_columns::INDEX_NAME), Some(Value::Null))
        })
        .filter_map(|row| int(row, storage_columns::MAX_COLUMN_ID_USED))
        .max())
}

/// The tables `txn` sees, each with the [`TableId`] of the version it sees: the rows of
/// [`STORAGE_TABLE`] that describe a table, read under the snapshot of the statement.
///
/// The rows are versioned, so a table whose `CREATE` is not committed is in the answer of
/// its own transaction alone, one whose `DROP` is not committed is missing from that answer
/// alone, and an uncommitted `ALTER TABLE` gives its own transaction the new storage table
/// and the others the old one (`table.rs`, module documentation).
///
/// # Errors
///
/// The error of the scan.
pub(crate) fn visible_tables(
    catalog: &Catalog,
    txn: &TxnHandle,
) -> SqlResult<BTreeMap<ObjectId, TableId>> {
    let snapshot = catalog.txn.statement_snapshot(txn);
    let mut visible = BTreeMap::new();
    for row in scan_named(catalog, &snapshot, STORAGE_TABLE)? {
        if !matches!(row.get(storage_columns::INDEX_NAME), Some(Value::Null)) {
            continue;
        }
        let (Some(object), Some(&Value::I64(storage))) = (
            int(&row, storage_columns::OBJECT_ID),
            row.get(storage_columns::STORAGE_ID),
        ) else {
            continue;
        };
        if let Ok(storage) = u32::try_from(storage) {
            visible.insert(ObjectId(object), TableId(storage));
        }
    }
    Ok(visible)
}

/// Writes in `txn` a row of [`HIGH_WATER_TABLE`] when an identifier `table` carries — its
/// own, one of its constraints — is above the largest value `txn` reads there.
///
/// Rows are inserted rather than one row updated, so two transactions that create tables
/// concurrently write two rows and not the same one. A `DROP` leaves the rows in place, so
/// the identifier of a dropped table stays taken after a reopen (unit test
/// `a_dropped_table_keeps_its_identifier_taken_across_a_reopen`). The cost is one row per statement
/// that raises the largest identifier.
///
/// # Errors
///
/// The error of the scan or of the insert.
pub(crate) fn record_high_water(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: &TableMeta,
) -> SqlResult<()> {
    let Some(high_water) = internal_table_id(catalog, HIGH_WATER_TABLE)? else {
        return Ok(());
    };
    let largest = table
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            ConstraintMeta::ForeignKey { constraint, .. }
            | ConstraintMeta::Check { constraint, .. }
            | ConstraintMeta::Default { constraint, .. } => Some(constraint.0),
            ConstraintMeta::PrimaryKey(_) | ConstraintMeta::Unique(_) => None,
        })
        .chain(std::iter::once(table.id.0))
        .max()
        .unwrap_or(table.id.0);
    let snapshot = catalog.txn.statement_snapshot(txn);
    let stored = scan_named(catalog, &snapshot, HIGH_WATER_TABLE)?
        .iter()
        .filter_map(|row| int(row, 0))
        .max();
    if stored.is_none_or(|stored| largest > stored) {
        catalog
            .storage
            .insert(txn.id, high_water, &Row(vec![Value::I32(largest)]))?;
    }
    Ok(())
}

/// Rebuilds the store of `catalog` from the committed rows of the internal tables. See the
/// module documentation.
///
/// Called by `open` in `bootstrap.rs`, after the internal tables are installed, on a storage
/// that first start did not create.
///
/// # Errors
///
/// [`InternalError`] when a call to `storage` fails, or [`InternalError::Bug`] naming the
/// table when a row the rebuild needs is missing or unreadable.
pub(crate) fn reload(catalog: &Catalog) -> Result<(), InternalError> {
    let snapshot = registry_snapshot(catalog, TxnId(0));
    let read = Stored::read(catalog, &snapshot)?;
    let mut live: BTreeMap<u32, Vec<TableId>> = BTreeMap::new();
    for (db, _) in catalog.storage.databases().map_err(internal)? {
        let tables = catalog.storage.tables(db).map_err(internal)?;
        live.insert(db.0, tables.into_iter().map(|(id, _)| id).collect());
    }
    let internal: Vec<TableId> = registered_internal_tables(catalog)?
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    let mut claimed: Vec<TableId> = Vec::new();
    let mut store = super::store(catalog);
    for row in &read.storage {
        let (Some(database), Some(object), Some(storage)) = (
            int(row, storage_columns::DATABASE_ID),
            int(row, storage_columns::OBJECT_ID),
            big(row, storage_columns::STORAGE_ID),
        ) else {
            continue;
        };
        if !matches!(row.get(storage_columns::INDEX_NAME), Some(Value::Null)) {
            continue;
        }
        let Ok(db) = u32::try_from(database) else {
            continue;
        };
        let Ok(storage) = u32::try_from(storage).map(TableId) else {
            continue;
        };
        if !live
            .get(&db)
            .is_some_and(|tables| tables.contains(&storage))
        {
            continue;
        }
        // A storage table is one user table: an internal table, or one a row read earlier
        // has taken, is not given to a second object.
        if internal.contains(&storage)
            || claimed.contains(&storage)
            || store.entries.contains_key(&ObjectId(object))
        {
            left_out(
                ObjectId(object),
                "the table",
                &InternalError::Bug(format!("catalog: storage table {storage} is already taken")),
            );
            continue;
        }
        let schema = string(row, storage_columns::SCHEMA_NAME).unwrap_or_default();
        let watermark = int(row, storage_columns::MAX_COLUMN_ID_USED);
        let rebuilt = match read.table(DbId(db), ObjectId(object), schema, storage) {
            Ok(rebuilt) => rebuilt,
            Err(err) => {
                left_out(ObjectId(object), "the table", &err);
                continue;
            }
        };
        claimed.push(storage);
        for index in rebuilt.indexes {
            store.indexes.entries.insert(
                index.id,
                IndexEntry::reloaded(rebuilt.meta.id, storage, index),
            );
        }
        for object in rebuilt.objects {
            store.constraints.entries.insert(object.id, object);
        }
        store.entries.insert(
            rebuilt.meta.id,
            TableEntry {
                meta: rebuilt.meta,
                previous_metas: Vec::new(),
                created_by: None,
                dropped_by: None,
                column_id_watermark: rebuilt.column_id_watermark.max(watermark.unwrap_or(0)),
            },
        );
    }
    raise_past(&mut store, read.largest_identifier(catalog, &snapshot)?);
    Ok(())
}

/// Raises the counter of `store` to `largest + 1` when that is above where it stands.
fn raise_past(store: &mut TableStore, largest: Option<i32>) {
    if let Some(largest) = largest {
        store.next_object_id = store.next_object_id.max(largest.saturating_add(1));
    }
}

/// One table rebuilt from its rows, and what goes beside it in the store.
struct Rebuilt {
    /// The metadata of the table.
    meta: TableMeta,
    /// Its indexes, with the [`IndexId`] `storage` handed out.
    indexes: Vec<IndexMeta>,
    /// Its `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints, as objects.
    objects: Vec<ObjectMeta>,
    /// The largest `column_id` of the table.
    column_id_watermark: i32,
}

/// The committed rows of the internal tables the rebuild reads, each as its list of values.
#[derive(Default)]
struct Stored {
    databases: Vec<Vec<Value>>,
    objects: Vec<Vec<Value>>,
    columns: Vec<Vec<Value>>,
    identities: Vec<Vec<Value>>,
    indexes: Vec<Vec<Value>>,
    index_columns: Vec<Vec<Value>>,
    foreign_keys: Vec<Vec<Value>>,
    foreign_key_columns: Vec<Vec<Value>>,
    checks: Vec<Vec<Value>>,
    defaults: Vec<Vec<Value>>,
    storage: Vec<Vec<Value>>,
    details: Vec<Vec<Value>>,
    high_water: Vec<Vec<Value>>,
}

impl Stored {
    /// Scans each internal table the rebuild reads, as `snapshot` sees it. A table the
    /// registry does not name reads as empty.
    fn read(catalog: &Catalog, snapshot: &Snapshot) -> Result<Self, InternalError> {
        let rows = |name: &str| scan_named(catalog, snapshot, name);
        Ok(Stored {
            databases: rows(DATABASES_TABLE)?,
            objects: rows(sys_tables::OBJECTS_TABLE)?,
            columns: rows(sys_tables::COLUMNS_TABLE)?,
            identities: rows(sys_indexes::IDENTITY_COLUMNS_TABLE)?,
            indexes: rows(sys_indexes::INDEXES_TABLE)?,
            index_columns: rows(sys_indexes::INDEX_COLUMNS_TABLE)?,
            foreign_keys: rows(sys_constraints::FOREIGN_KEYS_TABLE)?,
            foreign_key_columns: rows(sys_constraints::FOREIGN_KEY_COLUMNS_TABLE)?,
            checks: rows(sys_constraints::CHECK_CONSTRAINTS_TABLE)?,
            defaults: rows(sys_constraints::DEFAULT_CONSTRAINTS_TABLE)?,
            storage: rows(STORAGE_TABLE)?,
            details: rows(COLUMNS_TABLE)?,
            high_water: rows(HIGH_WATER_TABLE)?,
        })
    }

    /// The name of the database `database` as its row of [`DATABASES_TABLE`] publishes it.
    fn database_name(&self, database: i32) -> String {
        self.databases
            .iter()
            .find(|row| int(row, databases_columns::DATABASE_ID) == Some(database))
            .and_then(|row| string(row, databases_columns::NAME))
            .map_or_else(|| SYSTEM_DATABASES[0].to_owned(), str::to_owned)
    }

    /// The table `object` of the database `db`, stored in `storage`. See the module
    /// documentation.
    fn table(
        &self,
        db: DbId,
        object: ObjectId,
        schema: &str,
        storage: TableId,
    ) -> Result<Rebuilt, InternalError> {
        let database = database_id(db)?;
        let owned = |rows: &[Vec<Value>], database_at: usize, object_at: usize| {
            rows.iter()
                .filter(|row| {
                    int(row, database_at) == Some(database) && int(row, object_at) == Some(object.0)
                })
                .cloned()
                .collect::<Vec<Vec<Value>>>()
        };
        let object_row = owned(
            &self.objects,
            sys_tables::objects_columns::DATABASE_ID,
            sys_tables::objects_columns::OBJECT_ID,
        )
        .into_iter()
        .next()
        .ok_or_else(|| missing(object, "the row of vauban_sys_objects"))?;
        let name = string(&object_row, sys_tables::objects_columns::NAME)
            .ok_or_else(|| missing(object, "a name"))?
            .to_owned();
        let mut meta = TableMeta {
            id: object,
            storage_id: storage,
            database: db,
            schema: schema.to_owned(),
            name,
            columns: self.columns_of(database, object)?,
            clustered: None,
            constraints: Vec::new(),
        };
        let indexes = self.indexes_of(database, &mut meta)?;
        let objects = self.constraints_of(database, &mut meta)?;
        let column_id_watermark = int(&object_row, sys_tables::objects_columns::MAX_COLUMN_ID_USED)
            .unwrap_or(0)
            .max(
                meta.columns
                    .iter()
                    .map(|column| column.id.0)
                    .max()
                    .unwrap_or(0),
            );
        Ok(Rebuilt {
            meta,
            indexes,
            objects,
            column_id_watermark,
        })
    }

    /// The columns of the table `object`, in the order of the row of `storage`.
    fn columns_of(
        &self,
        database: i32,
        object: ObjectId,
    ) -> Result<Vec<ColumnMeta>, InternalError> {
        use sys_tables::columns_columns as at;

        let mut rows: Vec<&Vec<Value>> = self
            .columns
            .iter()
            .filter(|row| {
                int(row, at::DATABASE_ID) == Some(database)
                    && int(row, at::OBJECT_ID) == Some(object.0)
            })
            .collect();
        rows.sort_by_key(|row| int(row, at::COLUMN_ID));
        let mut columns = Vec::with_capacity(rows.len());
        for (rank, row) in rows.into_iter().enumerate() {
            let id = int(row, at::COLUMN_ID).ok_or_else(|| missing(object, "a column_id"))?;
            let name = string(row, at::NAME)
                .ok_or_else(|| missing(object, "a column name"))?
                .to_owned();
            let ty = type_of(
                tiny(row, at::SYSTEM_TYPE_ID).unwrap_or(0),
                small(row, at::MAX_LENGTH).unwrap_or(0),
                tiny(row, at::PRECISION).unwrap_or(0),
                tiny(row, at::SCALE).unwrap_or(0),
            )
            .ok_or_else(|| missing(object, &format!("a known type for column {name}")))?;
            let detail = self.detail_of(database, object, id);
            let collation = if ty.is_string() {
                Some(detail.and_then(collation_of).unwrap_or(Collation::DEFAULT))
            } else {
                None
            };
            let ordinal = detail
                .and_then(|row| small(row, column_details_columns::ORDINAL))
                .and_then(|ordinal| u16::try_from(ordinal).ok())
                .or_else(|| u16::try_from(rank).ok())
                .ok_or_else(|| missing(object, "a column position"))?;
            let computed = match detail
                .and_then(|row| string(row, column_details_columns::COMPUTED_DEFINITION))
            {
                Some(text) => {
                    Some(parse_stored_expression(text, ExpressionKind::Value).map_err(internal)?)
                }
                None => None,
            };
            columns.push(ColumnMeta {
                id: ColumnId(id),
                name,
                ty: TypeInfo {
                    ty,
                    nullable: bit(row, at::IS_NULLABLE).unwrap_or(true),
                    collation,
                },
                ordinal,
                default: None,
                identity: self.identity_of(database, object, id),
                computed,
            });
        }
        columns.sort_by_key(|column| column.ordinal);
        Ok(columns)
    }

    /// The row of [`COLUMNS_TABLE`] of one column, if any.
    fn detail_of(&self, database: i32, object: ObjectId, column: i32) -> Option<&[Value]> {
        use column_details_columns as at;

        self.details
            .iter()
            .find(|row| {
                int(row, at::DATABASE_ID) == Some(database)
                    && int(row, at::OBJECT_ID) == Some(object.0)
                    && int(row, at::COLUMN_ID) == Some(column)
            })
            .map(Vec::as_slice)
    }

    /// The `IDENTITY` property of one column, `None` when the column has no row in
    /// `vauban_sys_identity_columns`.
    fn identity_of(&self, database: i32, object: ObjectId, column: i32) -> Option<IdentitySpec> {
        use sys_indexes::identity_columns_columns as at;

        self.identities
            .iter()
            .find(|row| {
                int(row, at::DATABASE_ID) == Some(database)
                    && int(row, at::OBJECT_ID) == Some(object.0)
                    && int(row, at::COLUMN_ID) == Some(column)
            })
            .map(|row| IdentitySpec {
                seed: big(row, at::SEED_VALUE).unwrap_or(1),
                increment: big(row, at::INCREMENT_VALUE).unwrap_or(1),
            })
    }

    /// The [`IndexId`] the row of [`STORAGE_TABLE`] gives the index `name` of `object`.
    fn index_id(&self, database: i32, object: ObjectId, name: &str) -> Option<IndexId> {
        self.storage
            .iter()
            .find(|row| {
                int(row, storage_columns::DATABASE_ID) == Some(database)
                    && int(row, storage_columns::OBJECT_ID) == Some(object.0)
                    && string(row, storage_columns::INDEX_NAME) == Some(name)
            })
            .and_then(|row| big(row, storage_columns::STORAGE_ID))
            .and_then(|id| u32::try_from(id).ok())
            .map(IndexId)
    }

    /// The name of the index `object` publishes under `index_id` in `vauban_sys_indexes`.
    fn index_name(&self, database: i32, object: ObjectId, index_id: i32) -> Option<&str> {
        use sys_indexes::indexes_columns as at;

        self.indexes
            .iter()
            .find(|row| {
                int(row, at::DATABASE_ID) == Some(database)
                    && int(row, at::OBJECT_ID) == Some(object.0)
                    && int(row, at::INDEX_ID) == Some(index_id)
            })
            .and_then(|row| string(row, at::NAME))
    }

    /// The indexes of `meta`, its clustered index and the `PRIMARY KEY` and `UNIQUE`
    /// constraints they back, in increasing [`IndexId`] order, which is the order
    /// `create_table` created them in.
    fn indexes_of(
        &self,
        database: i32,
        meta: &mut TableMeta,
    ) -> Result<Vec<IndexMeta>, InternalError> {
        use sys_indexes::index_columns_columns as key_at;
        use sys_indexes::indexes_columns as at;

        let mut found = Vec::new();
        for row in self.indexes.iter().filter(|row| {
            int(row, at::DATABASE_ID) == Some(database)
                && int(row, at::OBJECT_ID) == Some(meta.id.0)
        }) {
            let kind = tiny(row, at::TYPE).unwrap_or(0);
            if kind == 0 {
                continue;
            }
            let read = || -> Result<(IndexMeta, bool), InternalError> {
                let index_id =
                    int(row, at::INDEX_ID).ok_or_else(|| missing(meta.id, "an index_id"))?;
                let name = string(row, at::NAME)
                    .ok_or_else(|| missing(meta.id, "an index name"))?
                    .to_owned();
                let id = self
                    .index_id(database, meta.id, &name)
                    .ok_or_else(|| missing(meta.id, &format!("the storage of index {name}")))?;
                let mut keys: Vec<(u8, KeyColumn)> = Vec::new();
                for key in self.index_columns.iter().filter(|key| {
                    int(key, key_at::DATABASE_ID) == Some(database)
                        && int(key, key_at::OBJECT_ID) == Some(meta.id.0)
                        && int(key, key_at::INDEX_ID) == Some(index_id)
                }) {
                    let column_id = int(key, key_at::COLUMN_ID);
                    let column = meta
                        .columns
                        .iter()
                        .find(|column| Some(column.id.0) == column_id)
                        .ok_or_else(|| {
                            missing(meta.id, &format!("the key columns of index {name}"))
                        })?;
                    keys.push((
                        tiny(key, key_at::KEY_ORDINAL).unwrap_or(0),
                        KeyColumn {
                            column: column.ordinal,
                            descending: bit(key, key_at::IS_DESCENDING_KEY).unwrap_or(false),
                        },
                    ));
                }
                keys.sort_by_key(|(ordinal, _)| *ordinal);
                Ok((
                    IndexMeta {
                        id,
                        name,
                        unique: bit(row, at::IS_UNIQUE).unwrap_or(false),
                        clustered: kind == 1,
                        primary_key: bit(row, at::IS_PRIMARY_KEY).unwrap_or(false),
                        columns: keys.into_iter().map(|(_, key)| key).collect(),
                    },
                    bit(row, at::IS_UNIQUE_CONSTRAINT).unwrap_or(false),
                ))
            };
            match read() {
                Ok(index) => found.push(index),
                Err(err) => left_out(meta.id, "an index", &err),
            }
        }
        found.sort_by_key(|(index, _)| index.id);
        for (index, unique_constraint) in &found {
            if index.clustered {
                meta.clustered = Some(index.id);
            }
            if index.primary_key {
                meta.constraints.push(ConstraintMeta::PrimaryKey(index.id));
            } else if *unique_constraint {
                meta.constraints.push(ConstraintMeta::Unique(index.id));
            }
        }
        Ok(found.into_iter().map(|(index, _)| index).collect())
    }

    /// The `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints of `meta`, pushed on
    /// `meta.constraints` in increasing [`ObjectId`] order, the default of each column set
    /// from its `DEFAULT`; answers their objects.
    fn constraints_of(
        &self,
        database: i32,
        meta: &mut TableMeta,
    ) -> Result<Vec<ObjectMeta>, InternalError> {
        use sys_constraints::constraint_columns as at;
        use sys_constraints::foreign_key_columns_columns as fk_column_at;
        use sys_constraints::foreign_keys_columns as fk_at;

        let mut found: Vec<(ObjectId, String, ConstraintMeta)> = Vec::new();
        let parented = |rows: &'_ [Vec<Value>], database_at: usize, parent_at: usize| {
            rows.iter()
                .filter(|row| {
                    int(row, database_at) == Some(database)
                        && int(row, parent_at) == Some(meta.id.0)
                })
                .cloned()
                .collect::<Vec<Vec<Value>>>()
        };
        for row in parented(
            &self.foreign_keys,
            fk_at::DATABASE_ID,
            fk_at::PARENT_OBJECT_ID,
        ) {
            let read = || -> Result<(ObjectId, String, ConstraintMeta), InternalError> {
                let id = int(&row, fk_at::OBJECT_ID)
                    .ok_or_else(|| missing(meta.id, "the identifier of a foreign key"))?;
                let name = string(&row, fk_at::NAME)
                    .ok_or_else(|| missing(meta.id, "a foreign key name"))?
                    .to_owned();
                let referenced = ObjectId(
                    int(&row, fk_at::REFERENCED_OBJECT_ID)
                        .ok_or_else(|| missing(meta.id, "a referenced table"))?,
                );
                let referenced_index = int(&row, fk_at::KEY_INDEX_ID)
                    .and_then(|index_id| self.index_name(database, referenced, index_id))
                    .and_then(|index| self.index_id(database, referenced, index))
                    .ok_or_else(|| {
                        missing(
                            meta.id,
                            &format!("the index the foreign key {name} points at"),
                        )
                    })?;
                let mut pairs: Vec<(i32, ColumnId, ColumnId)> = self
                    .foreign_key_columns
                    .iter()
                    .filter(|column| {
                        int(column, fk_column_at::DATABASE_ID) == Some(database)
                            && int(column, fk_column_at::CONSTRAINT_OBJECT_ID) == Some(id)
                    })
                    .map(|column| {
                        (
                            int(column, fk_column_at::CONSTRAINT_COLUMN_ID).unwrap_or(0),
                            ColumnId(int(column, fk_column_at::PARENT_COLUMN_ID).unwrap_or(0)),
                            ColumnId(int(column, fk_column_at::REFERENCED_COLUMN_ID).unwrap_or(0)),
                        )
                    })
                    .collect();
                pairs.sort_by_key(|(position, _, _)| *position);
                Ok((
                    ObjectId(id),
                    name,
                    ConstraintMeta::ForeignKey {
                        constraint: ObjectId(id),
                        system_named: bit(&row, fk_at::IS_SYSTEM_NAMED).unwrap_or(false),
                        columns: pairs.iter().map(|(_, column, _)| *column).collect(),
                        referenced_table: referenced,
                        referenced_columns: pairs.iter().map(|(_, _, column)| *column).collect(),
                        referenced_index,
                        on_delete: action(tiny(&row, fk_at::DELETE_REFERENTIAL_ACTION)),
                        on_update: action(tiny(&row, fk_at::UPDATE_REFERENTIAL_ACTION)),
                    },
                ))
            };
            match read() {
                Ok(constraint) => found.push(constraint),
                Err(err) => left_out(meta.id, "a foreign key", &err),
            }
        }
        for (rows, kind) in [
            (&self.checks, ExpressionKind::Predicate),
            (&self.defaults, ExpressionKind::Value),
        ] {
            for row in parented(rows, at::DATABASE_ID, at::PARENT_OBJECT_ID) {
                let read = || -> Result<(ObjectId, String, ConstraintMeta), InternalError> {
                    let id = int(&row, at::OBJECT_ID)
                        .ok_or_else(|| missing(meta.id, "the identifier of a constraint"))?;
                    let name = string(&row, at::NAME)
                        .ok_or_else(|| missing(meta.id, "a constraint name"))?
                        .to_owned();
                    let definition = string(&row, at::DEFINITION)
                        .ok_or_else(|| missing(meta.id, &format!("the text of {name}")))?
                        .to_owned();
                    let expr = parse_stored_expression(&definition, kind).map_err(internal)?;
                    let system_named = bit(&row, at::IS_SYSTEM_NAMED).unwrap_or(false);
                    let constraint = match kind {
                        ExpressionKind::Predicate => ConstraintMeta::Check {
                            constraint: ObjectId(id),
                            system_named,
                            expr,
                            definition,
                        },
                        ExpressionKind::Value => ConstraintMeta::Default {
                            constraint: ObjectId(id),
                            system_named,
                            column: ColumnId(int(&row, at::PARENT_COLUMN_ID).unwrap_or(0)),
                            expr,
                        },
                    };
                    Ok((ObjectId(id), name, constraint))
                };
                match read() {
                    Ok(constraint) => found.push(constraint),
                    Err(err) => left_out(meta.id, "a constraint", &err),
                }
            }
        }
        found.sort_by_key(|(id, _, _)| *id);
        let database_name = self.database_name(database);
        let mut objects = Vec::with_capacity(found.len());
        for (id, name, constraint) in found {
            if let ConstraintMeta::Default { column, expr, .. } = &constraint
                && let Some(target) = meta.columns.iter_mut().find(|c| c.id == *column)
                && target.default.is_none()
            {
                target.default = Some(expr.clone());
            }
            meta.constraints.push(constraint);
            objects.push(ObjectMeta {
                id,
                kind: ObjectKind::Constraint,
                name: QualifiedName {
                    database: database_name.clone(),
                    schema: meta.schema.clone(),
                    name,
                },
                database: meta.database,
                parent: Some(meta.id),
                definition: None,
            });
        }
        Ok(objects)
    }

    /// The largest identifier the rows hold — an object, the parent of an object, a table of
    /// [`STORAGE_TABLE`], a row of [`HIGH_WATER_TABLE`], the key of a counter of
    /// `identity.rs` — `None` when they hold none.
    fn largest_identifier(
        &self,
        catalog: &Catalog,
        snapshot: &Snapshot,
    ) -> Result<Option<i32>, InternalError> {
        use sys_tables::objects_columns as at;

        let mut ids: Vec<i32> = Vec::new();
        for row in &self.objects {
            ids.extend(int(row, at::OBJECT_ID));
            ids.extend(int(row, at::PARENT_OBJECT_ID));
        }
        for row in &self.storage {
            ids.extend(int(row, storage_columns::OBJECT_ID));
        }
        for row in &self.high_water {
            ids.extend(int(row, 0));
        }
        ids.extend(identity_keys(catalog, snapshot)?);
        Ok(ids.into_iter().max())
    }
}

/// The `object_id` each counter row of `identity.rs` is kept under.
///
/// The counter table is a table of `master` whose rows carry [`COUNTERS_TABLE`] in their first
/// column and the `object_id` in their second (`identity.rs`, module documentation); the rows
/// of the other tables of `master` are skipped by that first column.
fn identity_keys(catalog: &Catalog, snapshot: &Snapshot) -> Result<Vec<i32>, InternalError> {
    let Some((master, _)) = catalog
        .storage
        .databases()
        .map_err(internal)?
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(SYSTEM_DATABASES[0]))
    else {
        return Ok(Vec::new());
    };
    let three = |shape: &vauban_storage::TableShape| {
        shape.columns.len() == 3
            && matches!(shape.columns[0].ty, SqlType::NVarChar(_))
            && shape.columns[1].ty == SqlType::Int
            && shape.columns[2].ty == SqlType::BigInt
    };
    let mut keys = Vec::new();
    for (table, shape) in catalog.storage.tables(master).map_err(internal)? {
        if !three(&shape) {
            continue;
        }
        for row in catalog.storage.scan(snapshot, table).map_err(internal)? {
            let (_, values) = row.map_err(internal)?;
            if string(&values.0, 0) == Some(COUNTERS_TABLE) {
                keys.extend(int(&values.0, 1));
            }
        }
    }
    Ok(keys)
}

/// The committed rows of the internal table `name`, empty when the registry does not name it.
fn scan_named(
    catalog: &Catalog,
    snapshot: &Snapshot,
    name: &str,
) -> Result<Vec<Vec<Value>>, InternalError> {
    let Some(table) = internal_table_id(catalog, name)? else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for row in catalog.storage.scan(snapshot, table).map_err(internal)? {
        rows.push(row.map_err(internal)?.1.0);
    }
    Ok(rows)
}

/// The [`SqlType`] `vauban_sys_columns` describes by `system_type_id`, `max_length`,
/// `precision` and `scale`, the inverse of what `views/sys_tables.rs` writes; `None` for a
/// `system_type_id` it does not write (unit test `each_type_reads_back_from_its_column_row`).
pub(crate) fn type_of(
    system_type_id: u8,
    max_length: i16,
    precision: u8,
    scale: u8,
) -> Option<SqlType> {
    let len = |bytes_per_unit: i16| -> Option<Len> {
        if max_length == -1 {
            Some(Len::Max)
        } else {
            u16::try_from(max_length / bytes_per_unit)
                .ok()
                .map(Len::Fixed)
        }
    };
    Some(match system_type_id {
        104 => SqlType::Bit,
        48 => SqlType::TinyInt,
        52 => SqlType::SmallInt,
        56 => SqlType::Int,
        127 => SqlType::BigInt,
        106 => SqlType::Decimal { precision, scale },
        108 => SqlType::Numeric { precision, scale },
        62 => SqlType::Float,
        59 => SqlType::Real,
        60 => SqlType::Money,
        122 => SqlType::SmallMoney,
        175 => SqlType::Char(len(1)?),
        167 => SqlType::VarChar(len(1)?),
        239 => SqlType::NChar(len(2)?),
        231 => SqlType::NVarChar(len(2)?),
        173 => SqlType::Binary(len(1)?),
        165 => SqlType::VarBinary(len(1)?),
        40 => SqlType::Date,
        41 => SqlType::Time(scale),
        61 => SqlType::DateTime,
        58 => SqlType::SmallDateTime,
        42 => SqlType::DateTime2(scale),
        43 => SqlType::DateTimeOffset(scale),
        36 => SqlType::UniqueIdentifier,
        _ => return None,
    })
}

/// The collation a row of [`COLUMNS_TABLE`] carries, `None` when its four parts are not there.
fn collation_of(row: &[Value]) -> Option<Collation> {
    use column_details_columns as at;

    Some(Collation {
        lcid: u32::try_from(int(row, at::COLLATION_LCID)?).ok()?,
        flags: tiny(row, at::COLLATION_FLAGS)?,
        version: tiny(row, at::COLLATION_VERSION)?,
        sort_id: tiny(row, at::COLLATION_SORT_ID)?,
    })
}

/// The [`RefAction`] of a `*_referential_action` code, `NO ACTION` for an unknown one.
fn action(code: Option<u8>) -> RefAction {
    match code {
        Some(1) => RefAction::Cascade,
        Some(2) => RefAction::SetNull,
        Some(3) => RefAction::SetDefault,
        _ => RefAction::NoAction,
    }
}

/// The error of a table whose rows lack `what`.
fn missing(table: ObjectId, what: &str) -> InternalError {
    InternalError::Bug(format!(
        "catalog: table {table} cannot be reopened, {what} is missing from its rows"
    ))
}

/// Writes a warning for a part of the table `table` the rebuild leaves out: the table itself
/// when `what` is `"the table"`, otherwise one of its indexes or constraints.
fn left_out(table: ObjectId, what: &str, err: &InternalError) {
    tracing::warn!(
        table = table.0,
        error = %err,
        "a stored table could not be rebuilt in full; {what} is left out"
    );
}

fn int(row: &[Value], at: usize) -> Option<i32> {
    match row.get(at) {
        Some(Value::I32(value)) => Some(*value),
        _ => None,
    }
}

fn big(row: &[Value], at: usize) -> Option<i64> {
    match row.get(at) {
        Some(Value::I64(value)) => Some(*value),
        _ => None,
    }
}

fn small(row: &[Value], at: usize) -> Option<i16> {
    match row.get(at) {
        Some(Value::I16(value)) => Some(*value),
        _ => None,
    }
}

fn tiny(row: &[Value], at: usize) -> Option<u8> {
    match row.get(at) {
        Some(Value::I8(value)) => Some(*value),
        _ => None,
    }
}

fn bit(row: &[Value], at: usize) -> Option<bool> {
    match row.get(at) {
        Some(Value::Bit(value)) => Some(*value),
        _ => None,
    }
}

fn string(row: &[Value], at: usize) -> Option<&str> {
    match row.get(at) {
        Some(Value::String(value)) => Some(value.text.as_str()),
        _ => None,
    }
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

/// The `int` a [`DbId`] is published as.
fn database_id(id: DbId) -> Result<i32, InternalError> {
    i32::try_from(id.0).map_err(|_| {
        InternalError::Bug(format!(
            "catalog: database id {id} does not fit in the int the internal tables carry"
        ))
    })
}

/// An [`InternalError`] from the [`vauban_errors::SqlError`] of a `storage` call.
fn internal(err: vauban_errors::SqlError) -> InternalError {
    InternalError::Bug(format!("catalog reload: {}", err.message))
}

#[cfg(test)]
mod tests;
