//! Running `ALTER TABLE ADD` / `DROP COLUMN` and `ADD` / `DROP CONSTRAINT`.
//!
//! [`DdlStatement::AlterTable`] carries the three-part table name the binder resolved and
//! the [`AlterTable`](vauban_catalog::AlterTable) action the catalogue takes. This file
//! turns the name into an [`ObjectId`](vauban_catalog::ObjectId) and calls
//! [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table) inside the transaction the
//! caller opened ([`ExecContext::handle`](crate::ExecContext)), then answers
//! [`ExecOutcome::NoRows`]: no COLMETADATA and no row, like the database and table DDL of
//! `ddl.rs`.
//!
//! # What this file checks and what it propagates
//!
//! 4901 when a `NOT NULL` column without a default is added to a table that already holds
//! rows, 5074 when a column an index or constraint still references, 3728 when a constraint
//! name is unknown on `DROP CONSTRAINT`, and what [`Catalog::alter_table`] raises for
//! `FOREIGN KEY` and `DEFAULT` constraints come from `catalog`, already built with their
//! number, severity and state. The binder refused `ALTER COLUMN` and `WITH NOCHECK` before
//! the statement reaches this file.
//!
//! Two things are evaluated here, by the evaluator of the queries, since the catalogue
//! evaluates no expression:
//!
//! - the predicate of an added `CHECK`, over each row the table already holds, before the
//!   constraint is added: the first row it finds false is the 547 of `ALTER TABLE`
//!   (`tests/ddl_alter.rs`, `add_check_reads_the_existing_rows_with_the_query_evaluator`);
//! - the `DEFAULT` of a `NOT NULL` column added to a table that holds rows, once per
//!   existing row, through [`Catalog::alter_table_filling`] (`tests/ddl_alter.rs`,
//!   `add_not_null_columns_with_non_literal_defaults_fill_existing_rows`).
//!
//! # How a table is found
//!
//! [`CatalogSnapshot::resolve_object`](vauban_catalog::CatalogSnapshot::resolve_object) is the
//! way from the three-part [`QualifiedName`] the binder stored to the [`ObjectId`] that
//! [`Catalog::alter_table`] takes — the binder already proved the table exists when it
//! bound the statement, so a name that resolves to nothing here is the internal 50000 of
//! [`locate`], not a client error number.

use std::collections::HashMap;

use vauban_binder::{
    BindContext, BoundExpr, BoundStatement, CatalogView, DdlStatement, ExpressionKind, LogicalPlan,
    bind, parse_stored_expression,
};
use vauban_catalog::{
    AlterTable, Catalog, CatalogSnapshot, ColumnDef, ColumnMeta, ConstraintDef, ObjectId,
    QualifiedName, TableMeta,
};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{Expr, ParseOptions, parse_batch};
use vauban_txn::TxnHandle;
use vauban_types::Value;

use crate::context::ExecContext;
use crate::dml::assign::assign_value;
use crate::expr::eval_expr;
use crate::row::ExecOutcome;

/// Runs the `ALTER TABLE` of `stmt` and answers [`ExecOutcome::NoRows`].
///
/// The single entry point `ddl.rs` calls from its `AlterTable` arm; the other variants of
/// [`DdlStatement`] belong to `ddl.rs`, `ddl_index.rs` and `ddl_options.rs` and are the
/// internal 50000 here (`a_statement_that_is_not_an_alter_table_one_is_a_bug`), so a variant
/// routed to the wrong file is reported rather than run as something else.
///
/// # Errors
///
/// What `catalog.alter_table` raises, unchanged — 4901, 5074, 3728 and what the constraint
/// helpers answer — plus the internal 50000 this file raises when the table name no longer
/// resolves, and the internal 50000 for a statement that is not an `ALTER TABLE` one.
pub(crate) fn execute_alter_table(
    stmt: &DdlStatement,
    ctx: &ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    match stmt {
        DdlStatement::AlterTable { table, action } => {
            let id = locate(catalog, handle, table)?;
            let snap = catalog.snapshot(handle);
            let meta = snap
                .table(id)
                .ok_or_else(|| bug("ALTER TABLE: the table is missing from the snapshot"))?;
            let database = table.database.as_str();
            if let AlterTable::AddConstraint { constraint } = action
                && let ConstraintDef::Check { name, expr } = constraint.as_ref()
            {
                let expr = parse_stored_expression(expr, ExpressionKind::Predicate)?;
                refuse_check_violated_by_existing_rows(meta, &snap, database, name, &expr, ctx)?;
            }
            let added = added_columns(action);
            let outcome = if added.is_empty() {
                catalog.alter_table(handle, id, action)
            } else {
                let name = format!("{database}.{}.{}", meta.schema, meta.name);
                let mut defaults = NewDefaults::new(&added, &name, ctx)?;
                catalog.alter_table_filling(handle, id, action, &mut |column| {
                    defaults.value_for(column)
                })
            };
            if let Err(err) = outcome {
                if err.number == 2601 && key_constraint_add(action) {
                    return Err(SqlError::could_not_create_constraint_or_index());
                }
                return Err(err);
            }
        }
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        | DdlStatement::AlterDatabase { .. }
        | DdlStatement::CreateIndex { .. }
        | DdlStatement::DropIndex { .. }
        | DdlStatement::TruncateTable { .. } => {
            return Err(bug(
                "execute_alter_table: only ALTER TABLE is run here; the rest is run by \
                 ddl.rs, ddl_index.rs and ddl_options.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// A context over the same engine, catalogue and transaction as `ctx`, without the session:
/// what evaluating an object expression of a table needs.
fn evaluation_context<'a>(ctx: &ExecContext<'a>) -> ExecContext<'a> {
    let mut local = ExecContext::scalar(ctx.eval, ctx.options);
    local.storage = ctx.storage;
    local.txn = ctx.txn;
    local.catalog = ctx.catalog;
    local.handle = ctx.handle;
    local.cancel = ctx.cancel;
    local.snap = ctx.snap;
    local
}

/// Refuses a `CHECK` that a row the table already holds makes false (547).
///
/// The predicate is bound once against the row of the table and evaluated over each row the
/// statement sees, by [`eval_expr`]; `NULL` (unknown) passes, as it does for a written row.
/// The schema lock of the table is taken first, so that no other transaction writes a row
/// between this read and the addition of the constraint.
fn refuse_check_violated_by_existing_rows(
    meta: &TableMeta,
    snap: &CatalogSnapshot,
    database: &str,
    name: &Option<String>,
    expr: &Expr,
    ctx: &ExecContext<'_>,
) -> SqlResult<()> {
    let handle = ctx.handle()?;
    let manager = ctx
        .txn
        .ok_or_else(|| bug("ALTER TABLE: ADD CHECK needs a transaction manager"))?;
    let storage = ctx.storage()?;
    manager.schema_modify_lock(handle, meta.storage_id)?;
    let predicate = bind_check_predicate(meta, snap, database, expr, ctx)?;
    let rows_snapshot = manager.statement_snapshot(handle);
    let mut local = evaluation_context(ctx);
    for item in storage.scan(&rows_snapshot, meta.storage_id)? {
        let (_, row) = item?;
        if matches!(
            eval_expr(&predicate, Some(&row.0), &mut local)?,
            Value::Bit(false)
        ) {
            return Err(SqlError::fk_violation(
                "ALTER TABLE",
                "CHECK",
                name.as_deref().unwrap_or("CHECK"),
                database,
                &format!("{}.{}", meta.schema, meta.name),
                first_column(expr),
            ));
        }
    }
    Ok(())
}

/// The predicate `expr` bound against the row of `meta`, as the `WHERE` of
/// `SELECT 1 FROM database.schema.table`.
fn bind_check_predicate(
    meta: &TableMeta,
    snap: &CatalogSnapshot,
    database: &str,
    expr: &Expr,
    ctx: &ExecContext<'_>,
) -> SqlResult<BoundExpr> {
    let text = format!(
        "SELECT 1 FROM {}.{}.{} WHERE {expr};",
        bracketed(database),
        bracketed(&meta.schema),
        bracketed(&meta.name)
    );
    let plan = bind_query(&text, Some(snap), database, ctx)?;
    where_predicate(&plan).ok_or_else(|| bug("ALTER TABLE: the CHECK query has no WHERE"))
}

/// `name` between brackets, a closing bracket in it doubled.
fn bracketed(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// The first column `expr` reads, which the 547 of `ALTER TABLE` names.
fn first_column(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Column(column) => Some(column.name.value.as_str()),
        Expr::Binary { left, .. } => first_column(left),
        Expr::Unary { expr, .. } | Expr::Nested(expr, _) => first_column(expr),
        Expr::IsNull { expr, .. } => first_column(expr),
        Expr::Between { expr, .. } => first_column(expr),
        _ => None,
    }
}

/// The query of `text`, parsed and bound.
fn bind_query(
    text: &str,
    snap: Option<&CatalogSnapshot>,
    database: &str,
    ctx: &ExecContext<'_>,
) -> SqlResult<LogicalPlan> {
    let batch = parse_batch(text, &ParseOptions::default())?;
    let Some(statement) = batch.statements.first() else {
        return Err(bug("ALTER TABLE: the expression query is empty"));
    };
    let mut bind_ctx = BindContext::scalar(text, ctx.options);
    bind_ctx.catalog = snap.map(|snap| snap as &dyn CatalogView);
    bind_ctx.database = database;
    match bind(statement, &bind_ctx)? {
        BoundStatement::Query(plan) => Ok(*plan),
        _ => Err(bug("ALTER TABLE: the expression query is not a query")),
    }
}

fn where_predicate(plan: &LogicalPlan) -> Option<BoundExpr> {
    match plan {
        LogicalPlan::Filter { predicate, .. } => Some(predicate.clone()),
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct(input) => where_predicate(input),
        _ => None,
    }
}

/// The columns `action` adds, each with its definition.
fn added_columns(action: &AlterTable) -> Vec<&ColumnDef> {
    match action {
        AlterTable::AddColumn { column, .. } => vec![column.as_ref()],
        AlterTable::AddColumns { columns } => {
            columns.iter().map(|(column, _)| column.as_ref()).collect()
        }
        AlterTable::DropColumn { .. }
        | AlterTable::AddConstraint { .. }
        | AlterTable::DropConstraint { .. } => Vec::new(),
    }
}

/// The `DEFAULT` of each added column, bound once, and evaluated for each existing row the
/// catalogue copies.
struct NewDefaults<'a> {
    /// The bound default of each added column that has one, by name.
    bound: HashMap<String, BoundExpr>,
    /// `database.schema.table`, for the messages of a conversion.
    table: String,
    ctx: ExecContext<'a>,
}

impl<'a> NewDefaults<'a> {
    fn new(columns: &[&ColumnDef], table: &str, ctx: &ExecContext<'a>) -> SqlResult<Self> {
        let mut bound = HashMap::new();
        for column in columns {
            if let Some(default) = &column.default {
                let default = parse_stored_expression(default, ExpressionKind::Value)?;
                let text = format!("SELECT {default};");
                let plan = bind_query(&text, None, "master", ctx)?;
                let LogicalPlan::Project { exprs, .. } = plan else {
                    return Err(bug("ALTER TABLE: the DEFAULT query is not a projection"));
                };
                let Some(first) = exprs.into_iter().next() else {
                    return Err(bug("ALTER TABLE: the DEFAULT query projects nothing"));
                };
                bound.insert(column.name.to_lowercase(), first.expr);
            }
        }
        Ok(Self {
            bound,
            table: table.to_owned(),
            ctx: evaluation_context(ctx),
        })
    }

    /// The value of the `DEFAULT` of `column` for one existing row, of the type of the
    /// column.
    fn value_for(&mut self, column: &ColumnMeta) -> SqlResult<Value> {
        let Some(expr) = self.bound.get(&column.name.to_lowercase()) else {
            return Err(bug(&format!(
                "ALTER TABLE: the added column '{}' has no DEFAULT to fill the rows with",
                column.name
            )));
        };
        let value = eval_expr(expr, None, &mut self.ctx)?;
        assign_value(value, &expr.ty, column, &self.table, "ALTER TABLE")
    }
}

/// The [`ObjectId`] of the table named `table`.
///
/// # Errors
///
/// The internal 50000 when `table` resolves to nothing (module documentation).
fn locate(catalog: &Catalog, handle: &TxnHandle, table: &QualifiedName) -> SqlResult<ObjectId> {
    catalog
        .snapshot(handle)
        .resolve_object(
            &table.database,
            Some(&table.schema),
            &table.name,
            &table.schema,
        )
        .map(|object| object.id)
        .ok_or_else(|| {
            bug(&format!(
                "ALTER TABLE: the catalogue holds no table named '{}.{}'",
                table.schema, table.name
            ))
        })
}

/// Whether `action` adds a `PRIMARY KEY` or `UNIQUE` constraint through a table rebuild.
fn key_constraint_add(action: &AlterTable) -> bool {
    matches!(
        action,
        AlterTable::AddConstraint { constraint } if matches!(
            constraint.as_ref(),
            ConstraintDef::PrimaryKey { .. } | ConstraintDef::Unique { .. }
        )
    )
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests;
