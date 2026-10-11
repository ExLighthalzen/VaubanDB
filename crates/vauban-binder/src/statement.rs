//! The entry point of the binder: one statement in, one bound statement out.
//!
//! Each statement is dispatched to the file that binds it — `query.rs`, `ddl.rs`,
//! `ddl_index.rs`, `insert.rs`, `update_delete.rs`, `variables.rs`, `control.rs`,
//! `txn_stmt.rs`, `alter.rs`. A variant of `parser::Statement` that no module binds answers
//! an **internal** error 50000 naming the statement: a client does not see a made-up
//! message, and a reader of the log knows at once what is missing.
//!
//! The `match` of [`unsupported`] has no `_ =>` arm, so a variant added to
//! `parser::Statement` breaks this file rather than being silently reported as something
//! else.

use std::cell::Cell;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{QueryBody, Span, Statement};

use crate::alter::bind_alter_table;
use crate::bound::{BoundStatement, DeferredStatement};
use crate::context::BindContext;
use crate::control::{
    bind_block, bind_break, bind_continue, bind_goto, bind_if, bind_label, bind_print, bind_return,
    bind_while,
};
use crate::ddl::{
    bind_alter_database, bind_create_database, bind_create_table, bind_drop_database,
    bind_drop_table, bind_truncate, bind_use,
};
use crate::ddl_index::{bind_create_index, bind_drop_index};
use crate::depth::at_statement;
use crate::errors::on_the_statement;
use crate::execute::bind_execute;
use crate::insert::{bind_insert, bind_select_into};
use crate::query::bind_select;
use crate::txn_stmt::{bind_begin, bind_commit, bind_rollback, bind_save};
use crate::update_delete::{bind_delete, bind_update};
use crate::variables::{
    bind_declare, bind_select_assignment, bind_set_variable, is_assignment_select,
};

/// Binds one statement of a batch against `ctx`.
///
/// # Errors
///
/// A binding error is a `SqlError` with the number SQL Server uses (137, 195, 207, 263,
/// 1060, 4145, 8117…), so that a client cannot tell the two apart. An expression whose tree
/// is deeper than the binder walks is 8631, carrying the line of the statement. A statement
/// the binder does not bind yet is the internal error 50000 built by `unsupported`, which
/// names it. The DDL of `ddl.rs` adds 2714, 2715 and 448 to that list.
///
/// # Where the line of a binding error comes from
///
/// From the node the check hangs on, and **there is no rule** saying which node that is
/// (`crate::errors`, module header). The raising sites therefore each answer their own
/// line, and this function puts the **statement**'s line on the numbers that want it
/// (`crate::errors::NAMES_THE_STATEMENT`) and on those alone — the mirror of what `session`
/// does for run-time errors, which want it without exception. 257 belongs to that list
/// although the very call that raises it raises 402 and 8117, which do not: the number
/// decides, not the site.
pub fn bind(stmt: &Statement, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    match stmt {
        // `SELECT @x = e` assigns a variable and is not a query: `variables.rs` binds it.
        Statement::Select(select) if is_assignment_select(select) => {
            bind_select_assignment(select, ctx)
                .map_err(|err| at_statement(err, select.span.line))
                .map_err(|err| on_the_statement(err, select.span.line))
        }
        Statement::Select(select) if select_into_target(select).is_some() => {
            bind_select_into(select, ctx)
                .map_err(|err| at_statement(err, select.span.line))
                .map_err(|err| on_the_statement(err, select.span.line))
        }
        Statement::Select(select) => bind_select(select, ctx)
            .map(|plan| BoundStatement::Query(Box::new(plan)))
            // 8631 is raised deep in the descent, which does not know where the statement
            // starts; SQL Server reports the line of the statement.
            .map_err(|err| at_statement(err, select.span.line))
            .map_err(|err| on_the_statement(err, select.span.line)),
        Statement::CreateDatabase(create) => bind_create_database(create),
        Statement::DropDatabase {
            names, if_exists, ..
        } => bind_drop_database(names, *if_exists),
        Statement::Use { database, .. } => bind_use(database),
        Statement::CreateTable(create) => bind_create_table(create, ctx),
        Statement::DropTable {
            names, if_exists, ..
        } => bind_drop_table(names, *if_exists, ctx),
        Statement::CreateIndex(create) => bind_create_index(create, ctx),
        Statement::DropIndex(drop) => bind_drop_index(drop, ctx),
        Statement::AlterDatabase(alter) => bind_alter_database(alter, ctx),
        Statement::AlterTable(alter) => bind_alter_table(alter, ctx),
        Statement::Truncate { table, span } => bind_truncate(table, *span, ctx),
        Statement::Insert(insert) => bind_insert(insert, ctx)
            .and_then(|bound| refuse_internal_write(bound, insert.span, ctx)),
        Statement::Update(update) => bind_update(update, ctx)
            .and_then(|bound| refuse_internal_write(bound, update.span, ctx)),
        Statement::Delete(delete) => bind_delete(delete, ctx)
            .and_then(|bound| refuse_internal_write(bound, delete.span, ctx)),
        Statement::Declare(declare) => bind_declare(declare, ctx),
        Statement::Set(set) => bind_set_variable(set, ctx),
        Statement::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => bind_if(condition, then_branch, else_branch.as_deref(), ctx),
        Statement::While {
            condition, body, ..
        } => bind_while(condition, body, ctx),
        Statement::Block { statements, .. } => bind_block(statements, ctx),
        Statement::Break(_) if jumps_left_to_the_program() => Ok(BoundStatement::Break),
        Statement::Continue(_) if jumps_left_to_the_program() => Ok(BoundStatement::Continue),
        Statement::Break(span) => bind_break(span.line, ctx),
        Statement::Continue(span) => bind_continue(span.line, ctx),
        Statement::Return { value, span } => bind_return(value.as_ref(), span.line, ctx),
        Statement::Print { expr, .. } => bind_print(expr, ctx),
        Statement::BeginTransaction { name, mark, .. } => {
            bind_begin(name.as_ref(), mark.as_deref(), ctx)
        }
        Statement::Commit { name, .. } => bind_commit(name.as_ref(), ctx),
        Statement::Rollback { name, .. } => bind_rollback(name.as_ref(), ctx),
        Statement::Save { name, .. } => bind_save(name, ctx),
        Statement::Execute(execute) => bind_execute(execute, ctx),
        Statement::Goto { label, .. } => Ok(bind_goto(label)),
        Statement::Label { name, .. } => Ok(bind_label(name)),
        Statement::SetOption(set) => Ok(BoundStatement::SetOption(set.clone())),
        other => Err(unsupported(other)),
    }
}

thread_local! {
    /// Set while [`DeferredStatement::bind`] runs: a `BREAK` or a `CONTINUE` met then is
    /// bound as is, and the program that runs the statement resolves it.
    static JUMPS_LEFT_TO_THE_PROGRAM: Cell<bool> = const { Cell::new(false) };
}

/// Whether a `BREAK` or a `CONTINUE` is bound as is rather than checked against the
/// `WHILE` statements being bound around it.
fn jumps_left_to_the_program() -> bool {
    JUMPS_LEFT_TO_THE_PROGRAM.with(Cell::get)
}

impl DeferredStatement {
    /// Binds the statement against `ctx`, as [`bind`] does, with one difference: a
    /// `BREAK` or a `CONTINUE` it holds binds without a `WHILE` around it in the statement.
    ///
    /// The statement was written inside an `IF`, a `WHILE` or a `BEGIN … END` whose loop
    /// is no longer in view when the program reaches it: the program that runs it holds
    /// that loop, and it is the program that sends the jump to its exit or to its head, or
    /// answers 135 or 136 when no loop of its own encloses the statement
    /// (`statement::tests::a_deferred_break_binds_without_its_loop`).
    ///
    /// # Errors
    ///
    /// Those of [`bind`], 135 and 136 excepted.
    pub fn bind(&self, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
        let before = JUMPS_LEFT_TO_THE_PROGRAM.with(|flag| flag.replace(true));
        let bound = bind(&self.statement, ctx);
        JUMPS_LEFT_TO_THE_PROGRAM.with(|flag| flag.set(before));
        bound
    }
}

/// Refuses a bound `INSERT`, `UPDATE` or `DELETE` whose target is an internal table of the
/// catalogue ([`CatalogView::is_internal_table`](crate::CatalogView::is_internal_table)):
/// error 259, severity 16, state 1, on the line of the statement.
///
/// The check runs once the statement is bound, so an error of the statement itself comes
/// first: `DELETE FROM sys.objects WHERE nosuch = 1` answers 207 before 259 in SQL Server,
/// and the same order holds here (`tests/catalog_view.rs`,
/// `a_write_of_an_internal_table_is_refused`). The message does not name the table.
fn refuse_internal_write(
    bound: BoundStatement,
    span: Span,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let target = match &bound {
        BoundStatement::Insert(plan) => plan.table,
        BoundStatement::Update(plan) => plan.table,
        BoundStatement::Delete(plan) => plan.table,
        _ => return Ok(bound),
    };
    match ctx.catalog {
        Some(catalog) if catalog.is_internal_table(target) => Err(SqlError::new(
            AD_HOC_CATALOG_UPDATE_259,
            16,
            1,
            "Direct changes to the system catalogue are refused.",
        )
        .with_line(span.line)),
        _ => Ok(bound),
    }
}

/// The number SQL Server answers a client statement that writes a system catalogue with.
const AD_HOC_CATALOG_UPDATE_259: u32 = 259;

/// The `INTO` target of a top-level `SELECT`, when the statement writes one.
fn select_into_target(
    select: &vauban_parser::SelectStatement,
) -> Option<&vauban_parser::ObjectName> {
    match &select.body {
        QueryBody::Select(spec) => spec.into.as_ref(),
        QueryBody::SetOp { .. } | QueryBody::Nested(..) => None,
    }
}

/// The internal error a statement the binder does not bind answers: its keyword, and that
/// it is not implemented.
///
/// The statements `bind` dispatches — `SELECT`, the DDL of `ddl.rs`, the two of
/// `ddl_index.rs`, the DML, the control-of-flow and transaction statements routed to their
/// own file, `TRUNCATE TABLE` and `SET <option>` included — do not reach this function, and
/// are listed to keep the `match` exhaustive.
fn unsupported(stmt: &Statement) -> SqlError {
    let name = match stmt {
        // Bound by `bind`; listed so that the match stays exhaustive.
        Statement::Select(_) => "SELECT",
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Merge(_) => "MERGE",
        Statement::Truncate { .. } => "TRUNCATE TABLE",
        Statement::CreateDatabase(_) => "CREATE DATABASE",
        Statement::DropDatabase { .. } => "DROP DATABASE",
        Statement::Use { .. } => "USE",
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::DropTable { .. } => "DROP TABLE",
        Statement::AlterDatabase(_) => "ALTER DATABASE",
        Statement::AlterTable(_) => "ALTER TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::DropIndex(_) => "DROP INDEX",
        Statement::CreateProcedure(_) => "CREATE PROCEDURE",
        Statement::CreateFunction(_) => "CREATE FUNCTION",
        Statement::CreateView(_) => "CREATE VIEW",
        Statement::CreateTrigger(_) => "CREATE TRIGGER",
        Statement::CreateSequence(_) => "CREATE SEQUENCE",
        Statement::DropProcedure { .. } => "DROP PROCEDURE",
        Statement::DropFunction { .. } => "DROP FUNCTION",
        Statement::DropView { .. } => "DROP VIEW",
        Statement::DropTrigger { .. } => "DROP TRIGGER",
        Statement::DropSequence { .. } => "DROP SEQUENCE",
        Statement::Declare(_) => "DECLARE",
        Statement::Set(_) => "SET @variable",
        Statement::SetOption(_) => "SET <option>",
        Statement::If { .. } => "IF",
        Statement::While { .. } => "WHILE",
        Statement::Block { .. } => "BEGIN … END",
        Statement::Break(_) => "BREAK",
        Statement::Continue(_) => "CONTINUE",
        Statement::Return { .. } => "RETURN",
        Statement::Print { .. } => "PRINT",
        Statement::Execute(_) => "EXECUTE",
        Statement::BeginTransaction { .. } => "BEGIN TRANSACTION",
        Statement::Commit { .. } => "COMMIT",
        Statement::Rollback { .. } => "ROLLBACK",
        Statement::Save { .. } => "SAVE TRANSACTION",
        Statement::Goto { .. } => "GOTO",
        Statement::Label { .. } => "a GOTO label",
        Statement::Waitfor(_) => "WAITFOR",
        Statement::TryCatch { .. } => "BEGIN TRY … BEGIN CATCH",
        Statement::Throw { .. } => "THROW",
        Statement::RaiseError(_) => "RAISERROR",
        Statement::Cursor(_) => "a cursor statement",
        Statement::Grant(_) => "GRANT, DENY and REVOKE",
    };
    SqlError::from(InternalError::Bug(format!("{name} is not implemented yet")))
}

#[cfg(test)]
mod tests;
