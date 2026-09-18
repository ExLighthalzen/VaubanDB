//! `FOREIGN KEY` and `CHECK` at write time: the 547 of a row that breaks one.

use std::collections::HashMap;

use vauban_binder::{
    BindContext, BoundStatement, CatalogView, LogicalPlan, NoVariables, SessionOptions, bind,
};
use vauban_catalog::{Catalog, CatalogSnapshot, ColumnId, ConstraintMeta, ObjectId, TableMeta};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{Expr, ParseOptions, parse_batch};
use vauban_storage::{Direction, IndexId, KeyRange};
use vauban_types::Value;

use crate::context::ExecContext;
use crate::errors::at;
use crate::expr::eval_expr;
use crate::row::Row;

/// Runs the `CHECK` and outgoing `FOREIGN KEY` constraints of `table` against `row`.
pub(crate) fn check_row(
    table: &TableMeta,
    row: &Row,
    statement: &str,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<()> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    let snap = catalog.snapshot(handle);
    let db = database_name(&snap, table.database);
    let table_object = format!("{}.{}", table.schema, table.name);
    let constraint_names = constraint_names(catalog, table.id)?;

    for constraint in &table.constraints {
        match constraint {
            ConstraintMeta::Check {
                constraint, expr, ..
            } => {
                let bound = bind_check_predicate(table, &db, expr, &snap, ctx.options)?;
                let value = eval_expr(&bound, Some(row), ctx)?;
                if matches!(value, Value::Bit(false)) {
                    let name = constraint_names
                        .get(constraint)
                        .map(String::as_str)
                        .unwrap_or("?");
                    return Err(at(
                        SqlError::fk_violation(statement, "CHECK", name, &db, &table_object, None),
                        bound.line,
                    ));
                }
            }
            ConstraintMeta::ForeignKey {
                constraint,
                columns,
                referenced_table,
                referenced_columns,
                referenced_index,
                ..
            } => {
                let key = key_values(table, row, columns)?;
                if key.iter().any(|value| matches!(value, Value::Null)) {
                    continue;
                }
                let Some(referenced) = snap.table(*referenced_table) else {
                    return Err(bug(
                        "FOREIGN KEY: referenced table is missing from the snapshot",
                    ));
                };
                if !parent_key_exists(ctx, *referenced_index, &key)? {
                    let name = constraint_names
                        .get(constraint)
                        .map(String::as_str)
                        .unwrap_or("?");
                    let column = referenced_column_name(referenced, referenced_columns.first());
                    return Err(at(
                        SqlError::fk_violation(
                            statement,
                            "FOREIGN KEY",
                            name,
                            &db,
                            &format!("{}.{}", referenced.schema, referenced.name),
                            column.as_deref(),
                        ),
                        0,
                    ));
                }
            }
            ConstraintMeta::PrimaryKey(_)
            | ConstraintMeta::Unique(_)
            | ConstraintMeta::Default { .. } => {}
        }
    }
    Ok(())
}

/// Refuses when a row of another table still references `row` through a foreign key.
pub(crate) fn check_no_referencing_rows(
    table: &TableMeta,
    row: &Row,
    statement: &str,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<()> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    let snap = catalog.snapshot(handle);
    let db = database_name(&snap, table.database);
    let refs = catalog.referencing_foreign_keys(table.id)?;
    if refs.is_empty() {
        return Ok(());
    }

    for reference in refs {
        let Some(child) = snap.table(reference.table) else {
            return Err(bug("REFERENCE: child table is missing from the snapshot"));
        };
        let key = key_values(table, row, &reference.referenced_columns)?;
        if key.iter().any(|value| matches!(value, Value::Null)) {
            continue;
        }
        if child_row_exists(ctx, child, &reference.columns, &key)? {
            let names = constraint_names(catalog, child.id)?;
            let name = names
                .get(&reference.constraint)
                .map(String::as_str)
                .unwrap_or("?");
            let column = referenced_column_name(child, reference.columns.first());
            return Err(at(
                SqlError::fk_violation(
                    statement,
                    "REFERENCE",
                    name,
                    &db,
                    &format!("{}.{}", child.schema, child.name),
                    column.as_deref(),
                ),
                0,
            ));
        }
    }
    Ok(())
}

/// `true` when an `UPDATE` changes a column referenced by an incoming foreign key.
pub(crate) fn referenced_columns_changed(
    table: &TableMeta,
    old_row: &Row,
    new_row: &Row,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<bool> {
    let catalog = ctx.catalog()?;
    let refs = catalog.referencing_foreign_keys(table.id)?;
    if refs.is_empty() {
        return Ok(false);
    }
    let mut referenced: HashMap<ColumnId, ()> = HashMap::new();
    for reference in refs {
        for column in &reference.referenced_columns {
            referenced.insert(*column, ());
        }
    }
    for column in referenced.keys() {
        let ordinal = ordinal_of(table, *column)?;
        if old_row[ordinal] != new_row[ordinal] {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parent_key_exists(ctx: &mut ExecContext<'_>, index: IndexId, key: &[Value]) -> SqlResult<bool> {
    let storage = ctx.storage()?;
    let snap = ctx.snapshot()?;
    let mut iter = storage.seek(
        snap,
        index,
        &KeyRange::Point(key.to_vec()),
        Direction::Forward,
    )?;
    Ok(iter.next().transpose()?.is_some())
}

fn child_row_exists(
    ctx: &mut ExecContext<'_>,
    child: &TableMeta,
    fk_columns: &[ColumnId],
    key: &[Value],
) -> SqlResult<bool> {
    let storage = ctx.storage()?;
    let snap = ctx.snapshot()?;
    if let Some(index) = index_for_fk_columns(ctx, child, fk_columns)? {
        let mut iter = storage.seek(
            snap,
            index,
            &KeyRange::Point(key.to_vec()),
            Direction::Forward,
        )?;
        return Ok(iter.next().transpose()?.is_some());
    }
    for item in storage.scan(snap, child.storage_id)? {
        let (_, row) = item?;
        let child_key = key_values(child, &row.0, fk_columns)?;
        if child_key == key {
            return Ok(true);
        }
    }
    Ok(false)
}

fn index_for_fk_columns(
    ctx: &mut ExecContext<'_>,
    child: &TableMeta,
    columns: &[ColumnId],
) -> SqlResult<Option<IndexId>> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    let snap = catalog.snapshot(handle);
    let ordinals: Vec<u16> = columns
        .iter()
        .map(|column| {
            u16::try_from(ordinal_of(child, *column)?)
                .map_err(|_| bug("FOREIGN KEY: column ordinal does not fit an index key"))
        })
        .collect::<SqlResult<_>>()?;
    for index in snap.indexes_of(child.id) {
        if index.columns.len() < ordinals.len() {
            continue;
        }
        if index
            .columns
            .iter()
            .zip(ordinals.iter())
            .all(|(key, ordinal)| key.column == *ordinal)
        {
            return Ok(Some(index.id));
        }
    }
    Ok(None)
}

fn bind_check_predicate(
    table: &TableMeta,
    db: &str,
    expr: &Expr,
    snap: &CatalogSnapshot,
    options: SessionOptions,
) -> SqlResult<vauban_binder::BoundExpr> {
    let object = format!("{}.{}.{}", db, table.schema, table.name);
    let sql = format!("SELECT 1 FROM {object} WHERE {expr};");
    let batch = parse_batch(&sql, &ParseOptions::default()).map_err(|err| at(err, 0))?;
    let Some(statement) = batch.statements.first() else {
        return Err(bug("CHECK: synthetic predicate batch is empty"));
    };
    let bind_ctx = BindContext {
        text: &sql,
        catalog: Some(snap as &dyn CatalogView),
        database: db,
        default_schema: "dbo",
        variables: &NoVariables,
        options,
    };
    let bound = bind(statement, &bind_ctx).map_err(|err| at(err, 0))?;
    let BoundStatement::Query(plan) = bound else {
        return Err(bug("CHECK: synthetic predicate is not a query"));
    };
    extract_where_predicate(&plan).ok_or_else(|| bug("CHECK: synthetic query has no WHERE"))
}

fn extract_where_predicate(plan: &LogicalPlan) -> Option<vauban_binder::BoundExpr> {
    match plan {
        LogicalPlan::Filter { predicate, .. } => Some(predicate.clone()),
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct(input) => extract_where_predicate(input),
        _ => None,
    }
}

fn constraint_names(catalog: &Catalog, table: ObjectId) -> SqlResult<HashMap<ObjectId, String>> {
    Ok(catalog
        .constraints_of(table)?
        .into_iter()
        .map(|object| (object.id, object.name.name))
        .collect())
}

fn database_name(snap: &CatalogSnapshot, database: vauban_storage::DbId) -> String {
    snap.database_by_id(database)
        .map_or_else(|| "?".to_owned(), |db| db.name.clone())
}

fn key_values(table: &TableMeta, row: &Row, columns: &[ColumnId]) -> SqlResult<Vec<Value>> {
    columns
        .iter()
        .map(|column| {
            let ordinal = ordinal_of(table, *column)?;
            Ok(row[ordinal].clone())
        })
        .collect()
}

fn ordinal_of(table: &TableMeta, column: ColumnId) -> SqlResult<usize> {
    table
        .columns
        .iter()
        .find(|col| col.id == column)
        .map(|col| usize::from(col.ordinal))
        .ok_or_else(|| bug("constraint column is missing from the table metadata"))
}

fn referenced_column_name(table: &TableMeta, column: Option<&ColumnId>) -> Option<String> {
    column.and_then(|column| {
        table
            .columns
            .iter()
            .find(|col| col.id == *column)
            .map(|col| col.name.clone())
    })
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
