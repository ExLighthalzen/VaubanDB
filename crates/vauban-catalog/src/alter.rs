//! `ALTER TABLE` by copy: the catalogue builds a table of the new shape, copies the rows
//! visible to the current transaction, switches [`TableMeta::storage_id`] and defers the
//! drop of the old storage table to the `COMMIT`.
//!
//! The copy includes rows visible to the current transaction; uncommitted writes from
//! other sessions fall outside that snapshot until a schema lock serializes readers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{Expr, InList};
use vauban_storage::{IndexId, KeyColumn, Row, TableId};
use vauban_txn::{CommitAction, RollbackAction, TxnHandle};

use crate::catalog::Catalog;
use crate::def::{AlterTable, ColumnDef, ConstraintDef, IndexDef, SortedColumn, TableDef};
use crate::ids::{ColumnId, ObjectId};
use crate::index;
use crate::meta::{ColumnMeta, ConstraintMeta, QualifiedName, TableMeta};
use crate::table::{self, TableStore};

/// Key for [`column_id_watermarks`]: one catalogue instance and one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WatermarkKey {
    catalogue: usize,
    table: ObjectId,
}

/// Highest `column_id` handed out per table, including ids of columns since dropped
/// (`tests/alter.rs`, `drop_last_column_then_add_uses_high_water`).
fn column_id_watermarks() -> &'static Mutex<BTreeMap<WatermarkKey, i32>> {
    static WATERMARKS: Mutex<BTreeMap<WatermarkKey, i32>> = Mutex::new(BTreeMap::new());
    &WATERMARKS
}

fn watermark_key(catalog: &Catalog, table: ObjectId) -> WatermarkKey {
    WatermarkKey {
        catalogue: Arc::as_ptr(&catalog.storage) as *const () as usize,
        table,
    }
}

/// The highest `column_id` already used on `table`, live or dropped.
fn max_column_id_used(catalog: &Catalog, table: ObjectId, columns: &[ColumnMeta]) -> i32 {
    let live_max = columns.iter().map(|column| column.id.0).max().unwrap_or(0);
    column_id_watermarks()
        .lock()
        .expect("column_id watermarks mutex")
        .get(&watermark_key(catalog, table))
        .copied()
        .unwrap_or(0)
        .max(live_max)
}

fn set_max_column_id_used(catalog: &Catalog, table: ObjectId, value: i32) {
    column_id_watermarks()
        .lock()
        .expect("column_id watermarks mutex")
        .insert(watermark_key(catalog, table), value);
}

/// What one `ALTER TABLE` builds before the copy.
struct AlterPlan {
    columns: Vec<ColumnMeta>,
    keys: Vec<ConstraintDef>,
    preserved: Vec<ConstraintMeta>,
    add: Vec<ConstraintDef>,
    statement_indexes: Vec<IndexDef>,
}

/// Applies `change` to `table`. See [`Catalog::alter_table`].
///
/// # Errors
///
/// - 4901 when a `NOT NULL` column without a default is added to a table that already holds
///   rows (`tests/alter.rs`, `add_not_null_without_default_follows_the_measure`);
/// - 5074 when a column an index or constraint still references
///   (`tests/alter.rs`, `drop_column_used_by_an_index_is_refused`,
///   `add_column_default_then_drop_is_5074`);
/// - 3728 when a constraint name is unknown on `DROP CONSTRAINT`
///   (`tests/alter.rs`, `drop_unknown_constraint_is_3728`);
/// - what [`crate::index::table_keys`], [`crate::index::apply_table_keys`] and
///   [`crate::constraints::apply_table_constraints`] answer;
/// - the error of `storage` or of the transaction manager otherwise.
pub(crate) fn alter_table(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: ObjectId,
    change: &AlterTable,
) -> SqlResult<TableMeta> {
    if matches!(
        change,
        AlterTable::AddConstraint { .. } | AlterTable::DropConstraint { .. }
    ) {
        return alter_table_constraint(catalog, txn, table, change);
    }
    let (old, plan, old_storage) = {
        let mut store = table::store(catalog);
        table::refresh(catalog, &mut store)?;
        index::refresh(catalog, &mut store)?;
        crate::constraints::refresh_sessions(catalog, &mut store)?;
        crate::constraints::forget_dropped(&mut store);
        let Some(entry) = store.entries.get(&table) else {
            return Err(SqlError::cannot_drop("alter", "table", &table.to_string()));
        };
        if entry.dropped_by.is_some() {
            return Err(SqlError::cannot_drop("alter", "table", &entry.meta.name));
        }
        let old = entry.meta.clone();
        let plan = plan_change(catalog, txn, &store, &old, change)?;
        let old_storage = old.storage_id;
        (old, plan, old_storage)
    };
    let (meta, index_defs) = copy_table(catalog, txn, &old, &plan)?;
    catalog
        .txn
        .register_on_commit(txn, CommitAction::DropTable(old_storage))?;
    {
        let mut store = table::store(catalog);
        remove_storage_indexes(&mut store, old.id, old_storage);
        if let Some(entry) = store.entries.get_mut(&table) {
            entry.previous_meta = Some(old.clone());
            entry.meta = meta.clone();
            entry.dropped_by = None;
        }
        prune_constraint_objects(&mut store, &meta);
    }
    for def in index_defs {
        catalog.create_index(txn, &def)?;
    }
    let store = table::store(catalog);
    Ok(store
        .get(table)
        .cloned()
        .expect("alter_table stored the table"))
}

fn alter_table_constraint(
    catalog: &Catalog,
    txn: &TxnHandle,
    table: ObjectId,
    change: &AlterTable,
) -> SqlResult<TableMeta> {
    let mut store = table::store(catalog);
    table::refresh(catalog, &mut store)?;
    index::refresh(catalog, &mut store)?;
    crate::constraints::refresh_sessions(catalog, &mut store)?;
    crate::constraints::forget_dropped(&mut store);
    let Some(entry) = store.entries.get(&table) else {
        return Err(SqlError::cannot_drop("alter", "table", &table.to_string()));
    };
    if entry.dropped_by.is_some() {
        return Err(SqlError::cannot_drop("alter", "table", &entry.meta.name));
    }
    match change {
        AlterTable::AddConstraint { constraint } => match constraint.as_ref() {
            ConstraintDef::PrimaryKey { .. } | ConstraintDef::Unique { .. } => {
                return Err(InternalError::Bug(
                    "Catalog::alter_table: ADD CONSTRAINT PRIMARY KEY or UNIQUE needs an index; \
                     index.rs owns that shape"
                        .to_owned(),
                )
                .into());
            }
            _ => crate::constraints::add_table_constraint(
                catalog, txn, &mut store, table, constraint,
            )?,
        },
        AlterTable::DropConstraint { name } => {
            crate::constraints::drop_table_constraint(catalog, txn, &mut store, table, name)?;
        }
        _ => unreachable!("alter_table_constraint called on a column alter"),
    }
    Ok(store
        .get(table)
        .cloned()
        .expect("alter_table stored the table"))
}

fn plan_change(
    catalog: &Catalog,
    txn: &TxnHandle,
    store: &TableStore,
    old: &TableMeta,
    change: &AlterTable,
) -> SqlResult<AlterPlan> {
    let (mut keys, preserved) = split_constraints(store, old)?;
    let statement_indexes = statement_index_defs(store, old);
    let add = Vec::new();
    let columns = match change {
        AlterTable::AddColumn { column } => {
            refuse_duplicate_column(old, &column.name)?;
            if !column.ty.nullable
                && column.default.is_none()
                && column.identity.is_none()
                && column.computed.is_none()
                && !table_is_empty(catalog, txn, old)?
            {
                return Err(SqlError::cannot_add_column_to_non_empty_table(
                    &column.name,
                    &old.name,
                ));
            }
            let mut columns = old.columns.clone();
            let ordinal = u16::try_from(columns.len()).map_err(|_| {
                InternalError::Bug(format!(
                    "Catalog::alter_table: table {} has more columns than a u16 ordinal holds",
                    old.name
                ))
            })?;
            let next = max_column_id_used(catalog, old.id, &old.columns) + 1;
            set_max_column_id_used(catalog, old.id, next);
            columns.push(ColumnMeta {
                id: ColumnId(next),
                name: column.name.clone(),
                ty: column.ty.clone(),
                ordinal,
                default: column.default.clone(),
                identity: column.identity,
                computed: column.computed.clone(),
            });
            columns
        }
        AlterTable::DropColumn { name } => {
            let Some(target) = old
                .columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(name))
            else {
                return Err(SqlError::column_does_not_exist_in_target(name));
            };
            refuse_drop_column_used(store, old, target)?;
            set_max_column_id_used(
                catalog,
                old.id,
                max_column_id_used(catalog, old.id, &old.columns).max(target.id.0),
            );
            let mut columns = Vec::new();
            for (position, column) in old
                .columns
                .iter()
                .filter(|column| !column.name.eq_ignore_ascii_case(name))
                .enumerate()
            {
                columns.push(ColumnMeta {
                    ordinal: u16::try_from(position).map_err(|_| {
                        InternalError::Bug(format!(
                            "Catalog::alter_table: table {} has more columns than a u16 ordinal \
                             holds",
                            old.name
                        ))
                    })?,
                    ..column.clone()
                });
            }
            keys.retain(|key| !key_constraint_uses_column(key, target.id, old));
            let preserved = preserved
                .into_iter()
                .filter(|other| !constraint_meta_uses_column(other, target.id))
                .collect();
            return Ok(AlterPlan {
                columns,
                keys,
                preserved,
                add,
                statement_indexes,
            });
        }
        AlterTable::AddConstraint { .. } | AlterTable::DropConstraint { .. } => {
            unreachable!("constraint alters do not go through plan_change");
        }
    };
    Ok(AlterPlan {
        columns,
        keys,
        preserved,
        add,
        statement_indexes,
    })
}

fn copy_table(
    catalog: &Catalog,
    txn: &TxnHandle,
    old: &TableMeta,
    plan: &AlterPlan,
) -> SqlResult<(TableMeta, Vec<IndexDef>)> {
    let def = build_table_def(old, &plan.columns, &plan.keys, &[]);
    let keys = index::table_keys(&crate::constraints::keys_only(&def), &plan.columns)?;
    let shape = vauban_storage::TableShape {
        columns: plan
            .columns
            .iter()
            .map(|column| column.ty.clone())
            .collect(),
        clustered_key: keys.clustered_key(),
    };
    let new_storage = catalog.storage.create_table(old.database, &shape)?;
    if let Err(err) = catalog
        .txn
        .register_on_rollback(txn, RollbackAction::DropTable(new_storage))
    {
        catalog.storage.drop_table(new_storage)?;
        return Err(err);
    }
    copy_visible_rows(catalog, txn, old, new_storage, &plan.columns)?;
    let mut meta = TableMeta {
        id: old.id,
        storage_id: new_storage,
        database: old.database,
        schema: old.schema.clone(),
        name: old.name.clone(),
        columns: plan.columns.clone(),
        clustered: None,
        constraints: Vec::new(),
    };
    {
        let mut store = table::store(catalog);
        index::apply_table_keys(catalog, txn, &mut store, &keys, &mut meta)?;
        meta.constraints.extend(plan.preserved.clone());
        crate::constraints::apply_table_constraints(
            &mut store,
            &build_table_def(old, &plan.columns, &[], &[]),
            &mut meta,
        )?;
        if !plan.add.is_empty() {
            let def = build_table_def(old, &plan.columns, &[], &plan.add);
            crate::constraints::apply_table_constraints(&mut store, &def, &mut meta)?;
        }
        for constraint in &plan.add {
            if let ConstraintDef::Default { column, expr, .. } = constraint
                && let Some(meta_column) = meta
                    .columns
                    .iter_mut()
                    .find(|candidate| candidate.name.eq_ignore_ascii_case(column))
                && meta_column.default.is_none()
            {
                meta_column.default = Some(expr.clone());
            }
        }
        crate::sys_rows::rewrite(catalog, txn, &meta, &store.indexes)?;
    }
    Ok((meta, plan.statement_indexes.clone()))
}

fn copy_visible_rows(
    catalog: &Catalog,
    txn: &TxnHandle,
    old: &TableMeta,
    new_storage: TableId,
    new_columns: &[ColumnMeta],
) -> SqlResult<()> {
    let snap = catalog.txn.statement_snapshot(txn);
    let map = column_copy_map(&old.columns, new_columns);
    let width = new_columns.len();
    for result in catalog.storage.scan(&snap, old.storage_id)? {
        let (_, row) = result?;
        let mut copied = vec![vauban_types::Value::Null; width];
        for (from, to) in &map {
            copied[*to] = row.0[*from].clone();
        }
        catalog.storage.insert(txn.id, new_storage, &Row(copied))?;
    }
    Ok(())
}

fn prune_constraint_objects(store: &mut TableStore, table: &TableMeta) {
    let kept: BTreeSet<ObjectId> = table
        .constraints
        .iter()
        .filter_map(constraint_object_id)
        .collect();
    store
        .constraints
        .entries
        .retain(|id, object| object.parent != Some(table.id) || kept.contains(id));
}

fn remove_storage_indexes(store: &mut TableStore, table: ObjectId, storage_table: TableId) {
    store
        .indexes
        .entries
        .retain(|_, entry| !(entry.table == table && entry.storage_table == storage_table));
}

fn table_is_empty(catalog: &Catalog, txn: &TxnHandle, table: &TableMeta) -> SqlResult<bool> {
    let snap = catalog.txn.statement_snapshot(txn);
    Ok(catalog
        .storage
        .scan(&snap, table.storage_id)?
        .next()
        .is_none())
}

fn refuse_duplicate_column(table: &TableMeta, name: &str) -> SqlResult<()> {
    if table
        .columns
        .iter()
        .any(|column| column.name.eq_ignore_ascii_case(name))
    {
        return Err(SqlError::duplicate_column_name(name, &table.name));
    }
    Ok(())
}

fn refuse_drop_column_used(
    store: &TableStore,
    table: &TableMeta,
    target: &ColumnMeta,
) -> SqlResult<()> {
    for index in store.indexes.of_table(table.id) {
        if index.columns.iter().any(|key| key.column == target.ordinal) {
            return Err(SqlError::object_depends_on_column(
                "index",
                &index.name,
                &target.name,
            ));
        }
    }
    for constraint in &table.constraints {
        match constraint {
            ConstraintMeta::ForeignKey {
                constraint,
                columns,
                ..
            } if columns.contains(&target.id) => {
                let name = constraint_object_name(store, *constraint)?;
                return Err(SqlError::object_depends_on_column(
                    "object",
                    &name,
                    &target.name,
                ));
            }
            ConstraintMeta::Check {
                constraint, expr, ..
            } if expr_mentions_column(expr, &target.name) => {
                let name = constraint_object_name(store, *constraint)?;
                return Err(SqlError::object_depends_on_column(
                    "object",
                    &name,
                    &target.name,
                ));
            }
            ConstraintMeta::Default {
                constraint, column, ..
            } if *column == target.id => {
                let name = constraint_object_name(store, *constraint)?;
                return Err(SqlError::object_depends_on_column(
                    "object",
                    &name,
                    &target.name,
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn constraint_object_name(store: &TableStore, object: ObjectId) -> SqlResult<String> {
    store
        .constraints
        .entries
        .get(&object)
        .map(|entry| entry.name.name.clone())
        .ok_or_else(|| {
            InternalError::Bug(format!(
                "Catalog::alter_table: constraint object {object} is missing from the store"
            ))
            .into()
        })
}

fn expr_mentions_column(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Column(column) => column.name.value.eq_ignore_ascii_case(name),
        Expr::Binary { left, right, .. } => {
            expr_mentions_column(left, name) || expr_mentions_column(right, name)
        }
        Expr::Unary { expr, .. } => expr_mentions_column(expr, name),
        Expr::Nested(inner, _) => expr_mentions_column(inner, name),
        Expr::Function { args, .. } => args.iter().any(|arg| expr_mentions_column(arg, name)),
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|value| expr_mentions_column(value, name))
                || arms.iter().any(|arm| {
                    expr_mentions_column(&arm.when, name) || expr_mentions_column(&arm.then, name)
                })
                || else_
                    .as_ref()
                    .is_some_and(|value| expr_mentions_column(value, name))
        }
        Expr::Cast { expr, .. } | Expr::Collate { expr, .. } | Expr::IsNull { expr, .. } => {
            expr_mentions_column(expr, name)
        }
        Expr::Convert { expr, style, .. } => {
            expr_mentions_column(expr, name)
                || style
                    .as_ref()
                    .is_some_and(|value| expr_mentions_column(value, name))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_mentions_column(expr, name)
                || expr_mentions_column(low, name)
                || expr_mentions_column(high, name)
        }
        Expr::In { expr, list, .. } => {
            expr_mentions_column(expr, name)
                || match list {
                    InList::Exprs(values) => {
                        values.iter().any(|value| expr_mentions_column(value, name))
                    }
                    InList::Subquery(_) => false,
                }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_mentions_column(expr, name)
                || expr_mentions_column(pattern, name)
                || escape
                    .as_ref()
                    .is_some_and(|value| expr_mentions_column(value, name))
        }
        Expr::Quantified { expr, .. } => expr_mentions_column(expr, name),
        Expr::Assign { value, .. } => expr_mentions_column(value, name),
        Expr::Exists(..)
        | Expr::Literal(..)
        | Expr::Variable { .. }
        | Expr::InvalidNiladic { .. }
        | Expr::Subquery(..)
        | Expr::NextValueFor { .. }
        | Expr::Placeholder(..) => false,
    }
}

fn key_constraint_uses_column(key: &ConstraintDef, column: ColumnId, table: &TableMeta) -> bool {
    let columns = match key {
        ConstraintDef::PrimaryKey { columns, .. } | ConstraintDef::Unique { columns, .. } => {
            columns
        }
        _ => return false,
    };
    columns.iter().any(|sorted| {
        table
            .columns
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(&sorted.column))
            .is_some_and(|entry| entry.id == column)
    })
}

fn constraint_meta_uses_column(constraint: &ConstraintMeta, column: ColumnId) -> bool {
    match constraint {
        ConstraintMeta::ForeignKey { columns, .. } => columns.contains(&column),
        ConstraintMeta::Default { column: target, .. } => *target == column,
        _ => false,
    }
}

fn constraint_object_id(constraint: &ConstraintMeta) -> Option<ObjectId> {
    match constraint {
        ConstraintMeta::PrimaryKey(_) | ConstraintMeta::Unique(_) => None,
        ConstraintMeta::ForeignKey { constraint, .. }
        | ConstraintMeta::Check { constraint, .. }
        | ConstraintMeta::Default { constraint, .. } => Some(*constraint),
    }
}

fn column_copy_map(old: &[ColumnMeta], new: &[ColumnMeta]) -> Vec<(usize, usize)> {
    let mut map = Vec::new();
    for (new_pos, new_column) in new.iter().enumerate() {
        if let Some(old_pos) = old
            .iter()
            .position(|old_column| old_column.id == new_column.id)
        {
            map.push((old_pos, new_pos));
        }
    }
    map
}

fn build_table_def(
    table: &TableMeta,
    columns: &[ColumnMeta],
    keys: &[ConstraintDef],
    others: &[ConstraintDef],
) -> TableDef {
    let mut constraints = keys.to_vec();
    constraints.extend_from_slice(others);
    TableDef {
        name: qualified_name(table),
        columns: columns
            .iter()
            .map(|column| ColumnDef {
                name: column.name.clone(),
                ty: column.ty.clone(),
                default: column.default.clone(),
                identity: column.identity,
                computed: column.computed.clone(),
            })
            .collect(),
        constraints,
    }
}

fn qualified_name(table: &TableMeta) -> QualifiedName {
    QualifiedName {
        database: "master".to_owned(),
        schema: table.schema.clone(),
        name: table.name.clone(),
    }
}

fn split_constraints(
    store: &TableStore,
    table: &TableMeta,
) -> SqlResult<(Vec<ConstraintDef>, Vec<ConstraintMeta>)> {
    let mut keys = Vec::new();
    let mut preserved = Vec::new();
    for constraint in &table.constraints {
        match constraint {
            ConstraintMeta::PrimaryKey(index) => {
                keys.push(key_constraint_def(store, table, *index, true)?);
            }
            ConstraintMeta::Unique(index) => {
                keys.push(key_constraint_def(store, table, *index, false)?);
            }
            ConstraintMeta::ForeignKey { .. }
            | ConstraintMeta::Check { .. }
            | ConstraintMeta::Default { .. } => preserved.push(constraint.clone()),
        }
    }
    Ok((keys, preserved))
}

fn key_constraint_def(
    store: &TableStore,
    table: &TableMeta,
    index: IndexId,
    primary_key: bool,
) -> SqlResult<ConstraintDef> {
    let Some(index) = store.indexes.get(index) else {
        return Err(InternalError::Bug(format!(
            "Catalog::alter_table: table {} carries a key index the store no longer holds",
            table.name
        ))
        .into());
    };
    let columns = sorted_columns(table, &index.columns)?;
    if primary_key {
        Ok(ConstraintDef::PrimaryKey {
            name: Some(index.name.clone()),
            columns,
            clustered: index.clustered,
        })
    } else {
        Ok(ConstraintDef::Unique {
            name: Some(index.name.clone()),
            columns,
            clustered: index.clustered,
        })
    }
}

fn sorted_columns(table: &TableMeta, keys: &[KeyColumn]) -> SqlResult<Vec<SortedColumn>> {
    keys.iter()
        .map(|key| {
            let name = table
                .columns
                .iter()
                .find(|column| column.ordinal == key.column)
                .map(|column| column.name.clone())
                .ok_or_else(|| {
                    SqlError::from(InternalError::Bug(format!(
                        "Catalog::alter_table: key of table {} names the unknown column {}",
                        table.name, key.column
                    )))
                })?;
            Ok(SortedColumn {
                column: name,
                descending: key.descending,
            })
        })
        .collect()
}

fn statement_index_defs(store: &TableStore, table: &TableMeta) -> Vec<IndexDef> {
    store
        .indexes
        .of_table(table.id)
        .into_iter()
        .filter(|index| {
            !table.constraints.iter().any(|constraint| match constraint {
                ConstraintMeta::PrimaryKey(id) | ConstraintMeta::Unique(id) => *id == index.id,
                _ => false,
            })
        })
        .filter_map(|index| {
            sorted_columns(table, &index.columns)
                .ok()
                .map(|columns| IndexDef {
                    table: table.id,
                    name: index.name.clone(),
                    columns,
                    unique: index.unique,
                    clustered: false,
                })
        })
        .collect()
}
