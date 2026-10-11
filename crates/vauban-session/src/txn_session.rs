//! The transaction of one session: the one a `BEGIN TRANSACTION` opened, kept open across
//! the statements of a batch and across batches, its TDS transaction descriptor, and the
//! one-statement transaction a statement outside it runs in.
//!
//! # What the wire shows
//!
//! The outermost `BEGIN` opens the transaction and announces it with an ENVCHANGE of type 8
//! carrying the descriptor; a `BEGIN` at a deeper level just counts one more. `COMMIT` at the
//! outermost level announces type 9 and closes the transaction, a nested `COMMIT` counts one
//! less. `ROLLBACK` with no name announces type 10 and closes everything; `ROLLBACK` to a
//! savepoint, and `SAVE TRANSACTION`, change nothing on the wire. A batch that ends with a
//! transaction open leaves it open and raises nothing.
//!
//! # Who owns what
//!
//! [`SessionState::txn`] holds the transaction between statements. The executor keeps its own
//! [`ExecSession`] while it runs one: [`statement_txn`] copies the session transaction into
//! it, and [`finish_statement`] reads back what the executor left, emits the ENVCHANGE and
//! updates [`SessionState`]. The copy is what lets the executor's `BEGIN`/`COMMIT`/`ROLLBACK`
//! change the session transaction without knowing the session.
//!
//! # The schema a statement runs on
//!
//! A statement is bound before it runs, the statements of a batch all at once, and another
//! transaction may alter or drop a table it names in between. [`execute_in_a_transaction`]
//! takes the `Sch-S` of each user table the binding named ([`SchemaStamp`]) in the
//! transaction that runs the statement, waiting for a `DDL` another transaction has not
//! ended, then looks each table up in `storage`. A `DROP TABLE` and an `ALTER TABLE` that
//! rebuilds its table drop the storage table the binding read when they commit, so a table
//! no longer there sends the statement back to be bound again ([`Executed::Rebind`])
//! instead of running a plan written for it; the tables still there keep their `Sch-S`
//! until the statement ends, so a `DDL` started meanwhile waits for that end
//! (`tests/ddl_visibility.rs`, `an_insert_bound_before_an_alter_waits_and_keeps_its_row`,
//! `an_insert_bound_before_a_committed_alter_is_bound_again`). In the session transaction,
//! the end of the statement gives back that `Sch-S` and the `IS` of its `READ COMMITTED`
//! reads (`vauban_txn::TransactionManager::end_statement`); a one-statement transaction
//! gives back everything when it ends. A statement held by an `IF`, a `WHILE` or a
//! `BEGIN … END` takes the `Sch-S` of the tables it names, not of every table its holder
//! was bound against (`tests/ddl_visibility.rs`,
//! `a_held_statement_holds_the_schema_of_its_own_tables_only`).

use std::sync::Arc;

use vauban_binder::{DdlStatement, TxnStatement};
use vauban_catalog::{CatalogSnapshot, ConstraintMeta, TableMeta};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, StatementSink};
use vauban_planner::{PhysicalPlan, PhysicalStatement, SubPlan};
use vauban_storage::{DbId, IndexId, Storage, TableId};
use vauban_tds::EnvChange;
use vauban_txn::TxnHandle;

use crate::Engine;
use crate::batch::{LOCK_REQUEST_TIMEOUT, LOCK_TIMEOUT_STATE_OF_A_CREATE};
use crate::cancel::CancelHandle;
use crate::eval_context::SessionEvalContext;
use crate::set_options::{
    apply_deadlock_priority, begin_autocommit_txn, guard_snapshot_table_read, lock_timeout_from_ms,
    sync_exec_session,
};
use crate::sink::ResultSink;
use crate::state::SessionState;

/// The user tables the binding of a statement reached, each with the database of the
/// version the binding read (module documentation, "The schema a statement runs on").
///
/// A table the binding did not find, the name another open transaction was creating or
/// dropping, comes without a database: its `Sch-S` is waited for and its presence is not
/// looked up.
///
/// The stamp of a statement that holds others (an `IF`, a `WHILE`, a `BEGIN … END`) also
/// keeps, for each of its tables, the indexes a seek reaches it by and, when the statement
/// holds a write, the tables its `FOREIGN KEY` constraints tie it to: what
/// [`SchemaStamp::of_held`] needs to narrow the stamp to one statement it holds, and
/// [`SchemaStamp::of_conditions`] to the tests of its conditions.
#[derive(Debug, Clone, Default)]
pub(crate) struct SchemaStamp {
    tables: Arc<Vec<(TableId, Option<DbId>)>>,
    reach: Arc<Vec<TableReach>>,
    conditions: Option<Arc<Vec<TableId>>>,
}

/// How the statements a block holds reach one table of its stamp.
#[derive(Debug, Clone)]
struct TableReach {
    table: TableId,
    /// The indexes of the table, the clustered one included.
    indexes: Vec<IndexId>,
    /// The tables a `FOREIGN KEY` of this table references, and those whose `FOREIGN KEY`
    /// references it: a write of its rows checks them. Empty unless the holder writes.
    linked: Vec<TableId>,
}

impl SchemaStamp {
    /// The stamp of a binding against `snapshot` that reached `tables`.
    pub(crate) fn of(snapshot: &CatalogSnapshot, tables: &[TableId]) -> Self {
        Self {
            tables: Arc::new(
                tables
                    .iter()
                    .map(|table| {
                        let database = snapshot.table_by_storage(*table).map(|meta| meta.database);
                        (*table, database)
                    })
                    .collect(),
            ),
            reach: Arc::default(),
            conditions: None,
        }
    }

    /// The stamp of `holder`, a statement that holds others, bound against `snapshot`,
    /// whose binding reached `tables`.
    pub(crate) fn of_holder(
        snapshot: &CatalogSnapshot,
        tables: &[TableId],
        holder: &PhysicalStatement,
    ) -> Self {
        let writes = holds_a_write(holder);
        let reach = tables
            .iter()
            .filter_map(|table| snapshot.table_by_storage(*table))
            .map(|meta| {
                let mut indexes: Vec<IndexId> = snapshot
                    .indexes_of(meta.id)
                    .iter()
                    .map(|index| index.id)
                    .collect();
                indexes.extend(meta.clustered);
                let linked = if writes {
                    foreign_key_neighbours(snapshot, meta)
                } else {
                    Vec::new()
                };
                TableReach {
                    table: meta.storage_id,
                    indexes,
                    linked,
                }
            })
            .collect();
        let mut stamp = Self::of(snapshot, tables);
        stamp.reach = Arc::new(reach);
        let mut named = Named::default();
        conditions_named(holder, &mut named);
        stamp.conditions = stamp.resolve(&named).map(Arc::new);
        stamp
    }

    /// The part of this stamp, the one of the statement that holds `held`, that `held`
    /// names: the tables it reads or writes and, for a write, the tables its `FOREIGN KEY`
    /// constraints check, as the binding of the same statement at the top level would reach
    /// (`tests/ddl_visibility.rs`,
    /// `a_held_statement_holds_the_schema_of_its_own_tables_only`).
    ///
    /// The whole stamp when that part cannot be told: a `DDL` statement, or a seek on an
    /// index no table of the stamp has.
    pub(crate) fn of_held(&self, held: &PhysicalStatement) -> Self {
        let mut named = Named::default();
        if !statement_named(held, &mut named) {
            return self.clone();
        }
        match self.resolve(&named) {
            Some(tables) => self.narrowed(&tables),
            None => self.clone(),
        }
    }

    /// The part of this stamp the conditions of its `IF` and `WHILE` statements read,
    /// the whole stamp when that cannot be told (`tests/ddl_visibility.rs`,
    /// `a_condition_holds_the_schema_of_the_tables_it_reads`).
    ///
    /// The conditions of one statement share this part: the host that tests a condition is
    /// not told which one it is.
    pub(crate) fn of_conditions(&self) -> Self {
        match &self.conditions {
            Some(tables) => self.narrowed(tables),
            None => self.clone(),
        }
    }

    /// The storage tables of this stamp.
    pub(crate) fn tables(&self) -> Vec<TableId> {
        self.tables.iter().map(|(table, _)| *table).collect()
    }

    /// The tables `named` reaches, the indexes it seeks turned into their tables and the
    /// tables its writes check added; `None` for an index this stamp does not know.
    fn resolve(&self, named: &Named) -> Option<Vec<TableId>> {
        let mut tables = named.tables.clone();
        for index in &named.indexes {
            let reach = self
                .reach
                .iter()
                .find(|reach| reach.indexes.contains(index))?;
            tables.push(reach.table);
        }
        for written in &named.written {
            if let Some(reach) = self.reach.iter().find(|reach| reach.table == *written) {
                tables.extend(reach.linked.iter().copied());
            }
        }
        Some(tables)
    }

    /// The entries of this stamp whose table is one of `tables`, in the order of the stamp.
    fn narrowed(&self, tables: &[TableId]) -> Self {
        Self {
            tables: Arc::new(
                self.tables
                    .iter()
                    .filter(|(table, _)| tables.contains(table))
                    .copied()
                    .collect(),
            ),
            reach: Arc::default(),
            conditions: None,
        }
    }

    /// The storage tables the binding read that are no longer in `storage`.
    ///
    /// One listing per database, its tables sorted by identifier; a database that is gone
    /// counts as its tables gone.
    pub(crate) fn gone_tables(&self, storage: &dyn Storage) -> Vec<TableId> {
        let mut listed: Vec<(DbId, Option<Vec<TableId>>)> = Vec::new();
        let mut gone = Vec::new();
        for (table, database) in self.tables.iter() {
            let Some(database) = database else { continue };
            let position = match listed.iter().position(|(db, _)| db == database) {
                Some(position) => position,
                None => {
                    let tables = storage
                        .tables(*database)
                        .ok()
                        .map(|tables| tables.into_iter().map(|(id, _)| id).collect());
                    listed.push((*database, tables));
                    listed.len() - 1
                }
            };
            let present = listed[position]
                .1
                .as_ref()
                .is_some_and(|tables: &Vec<TableId>| tables.binary_search(table).is_ok());
            if !present {
                gone.push(*table);
            }
        }
        gone
    }
}

/// The storage tables the `FOREIGN KEY` constraints of `meta` reference, and those of the
/// tables whose `FOREIGN KEY` references it.
fn foreign_key_neighbours(snapshot: &CatalogSnapshot, meta: &TableMeta) -> Vec<TableId> {
    let mut linked = Vec::new();
    for constraint in &meta.constraints {
        if let ConstraintMeta::ForeignKey {
            referenced_table, ..
        } = constraint
            && let Some(referenced) = snapshot.table(*referenced_table)
        {
            linked.push(referenced.storage_id);
        }
    }
    for other in snapshot.tables() {
        let references = other.constraints.iter().any(|constraint| {
            matches!(
                constraint,
                ConstraintMeta::ForeignKey { referenced_table, .. } if *referenced_table == meta.id
            )
        });
        if references {
            linked.push(other.storage_id);
        }
    }
    linked
}

/// What a statement names in its plans: the tables it scans or writes, the indexes it
/// seeks, and the tables it writes.
#[derive(Debug, Default)]
struct Named {
    tables: Vec<TableId>,
    indexes: Vec<IndexId>,
    written: Vec<TableId>,
}

/// Whether `statement` holds an `INSERT`, an `UPDATE` or a `DELETE`, itself included.
pub(crate) fn holds_a_write(statement: &PhysicalStatement) -> bool {
    match statement {
        PhysicalStatement::Insert(_)
        | PhysicalStatement::Update(_)
        | PhysicalStatement::Delete(_) => true,
        PhysicalStatement::If { then_, else_, .. } => {
            holds_a_write(then_) || else_.as_deref().is_some_and(holds_a_write)
        }
        PhysicalStatement::While { body, .. } => holds_a_write(body),
        PhysicalStatement::Block(statements) => statements.iter().any(holds_a_write),
        _ => false,
    }
}

/// Adds to `named` what `statement` names; `false` for a statement whose tables its plan
/// does not show, a `DDL` statement.
fn statement_named(statement: &PhysicalStatement, named: &mut Named) -> bool {
    match statement {
        PhysicalStatement::Query(plan) | PhysicalStatement::SelectAssign { input: plan, .. } => {
            plan_named(plan, named);
        }
        PhysicalStatement::Insert(insert) => {
            named.tables.push(insert.table);
            named.written.push(insert.table);
            plan_named(&insert.source, named);
        }
        PhysicalStatement::SelectInto(into) => plan_named(&into.source, named),
        PhysicalStatement::Update(update) => {
            named.tables.push(update.table);
            named.written.push(update.table);
            plan_named(&update.input, named);
        }
        PhysicalStatement::Delete(delete) => {
            named.tables.push(delete.table);
            named.written.push(delete.table);
            plan_named(&delete.input, named);
        }
        PhysicalStatement::SetVariable { subplans, .. } => subplans_named(subplans, named),
        PhysicalStatement::Declare(declarations) => {
            for declaration in declarations {
                subplans_named(&declaration.subplans, named);
            }
        }
        PhysicalStatement::If {
            condition,
            then_,
            else_,
        } => {
            subplans_named(&condition.subplans, named);
            if !statement_named(then_, named) {
                return false;
            }
            if let Some(else_) = else_
                && !statement_named(else_, named)
            {
                return false;
            }
        }
        PhysicalStatement::While { condition, body } => {
            subplans_named(&condition.subplans, named);
            return statement_named(body, named);
        }
        PhysicalStatement::Block(statements) => {
            return statements
                .iter()
                .all(|statement| statement_named(statement, named));
        }
        PhysicalStatement::Return(value) => {
            if let Some(value) = value {
                subplans_named(&value.subplans, named);
            }
        }
        PhysicalStatement::Ddl(_) => return false,
        // A statement left unbound gets its own stamp when it is bound; an `EXECUTE` runs
        // a body bound on its own; the rest reads no table.
        PhysicalStatement::Use { .. }
        | PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Goto { .. }
        | PhysicalStatement::Label { .. }
        | PhysicalStatement::Print(_)
        | PhysicalStatement::Transaction(_)
        | PhysicalStatement::Execute(_)
        | PhysicalStatement::SetOption(_)
        | PhysicalStatement::Deferred(_) => {}
    }
    true
}

/// Adds to `named` what the conditions of the `IF` and `WHILE` statements `statement`
/// holds name, itself included.
fn conditions_named(statement: &PhysicalStatement, named: &mut Named) {
    match statement {
        PhysicalStatement::If {
            condition,
            then_,
            else_,
        } => {
            subplans_named(&condition.subplans, named);
            conditions_named(then_, named);
            if let Some(else_) = else_ {
                conditions_named(else_, named);
            }
        }
        PhysicalStatement::While { condition, body } => {
            subplans_named(&condition.subplans, named);
            conditions_named(body, named);
        }
        PhysicalStatement::Block(statements) => {
            for statement in statements {
                conditions_named(statement, named);
            }
        }
        _ => {}
    }
}

fn subplans_named(subplans: &[SubPlan], named: &mut Named) {
    for subplan in subplans {
        plan_named(&subplan.plan, named);
    }
}

/// Adds to `named` the tables `plan` scans and the indexes it seeks, those of the
/// subqueries it evaluates included.
fn plan_named(plan: &PhysicalPlan, named: &mut Named) {
    match plan {
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } => {}
        PhysicalPlan::TableScan { table, .. } => named.tables.push(*table),
        PhysicalPlan::IndexSeek { index, .. } => named.indexes.push(*index),
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::HashAggregate { input, .. }
        | PhysicalPlan::StreamAggregate { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::Distinct(input) => plan_named(input, named),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            plan_named(outer, named);
            plan_named(inner, named);
        }
        PhysicalPlan::HashJoin { build, probe, .. } => {
            plan_named(build, named);
            plan_named(probe, named);
        }
        PhysicalPlan::SubqueryEval {
            input, subplans, ..
        } => {
            plan_named(input, named);
            subplans_named(subplans, named);
        }
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => {
            for input in inputs {
                plan_named(input, named);
            }
        }
    }
}

/// What [`execute_in_a_transaction`] did with a statement.
#[derive(Debug)]
pub(crate) enum Executed {
    /// It ran, or failed before or while it ran, with this result.
    Ran(SqlResult<ExecOutcome>),
    /// A storage table it was bound against is no longer there: nothing of it ran, and it
    /// is to be bound again.
    Rebind,
}

/// The transaction a `BEGIN TRANSACTION` opened, as the session holds it between statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionTxn {
    /// The handle of the transaction manager.
    pub handle: TxnHandle,
    /// The TDS descriptor the opening ENVCHANGE announced, repeated by the closing one.
    pub descriptor: u64,
    /// How many `BEGIN TRANSACTION` are open, the value of `@@TRANCOUNT`.
    pub depth: i32,
}

/// The transaction one statement runs in, handed back to [`finish_statement`].
#[derive(Debug, Clone)]
pub(crate) enum StatementTxn {
    /// The session transaction is open: the statement ran in it and it stays open.
    Explicit,
    /// No session transaction: one was opened for this statement alone, to commit or to roll
    /// back once it returned.
    Autocommit(TxnHandle),
    /// Autocommit, but the statement failed before a handle was opened.
    Skipped,
}

/// The verb of a transaction statement, which decides between the two closing ENVCHANGE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxnKind {
    /// `BEGIN TRANSACTION`.
    Begin,
    /// `COMMIT` / `COMMIT WORK`.
    Commit,
    /// `ROLLBACK` with no name.
    Rollback,
    /// Any other statement, a `SAVE` or a `ROLLBACK` to a savepoint included.
    Other,
}

/// The verb of the transaction statement `statement` is, [`TxnKind::Other`] for the rest.
pub(crate) fn kind_of(statement: &PhysicalStatement) -> TxnKind {
    match statement {
        PhysicalStatement::Transaction(TxnStatement::Begin { .. }) => TxnKind::Begin,
        PhysicalStatement::Transaction(TxnStatement::Commit { .. }) => TxnKind::Commit,
        PhysicalStatement::Transaction(TxnStatement::Rollback { .. }) => TxnKind::Rollback,
        _ => TxnKind::Other,
    }
}

/// The transaction `statement` runs in, copied into `exec` so the executor sees it.
pub(crate) fn statement_txn(
    state: &SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
) -> SqlResult<StatementTxn> {
    sync_exec_session(state, exec);
    match &state.txn {
        Some(txn) => {
            exec.txn = Some(txn.handle.clone());
            exec.trancount = txn.depth;
            apply_deadlock_priority(&engine.txn, &txn.handle, state.options.deadlock_priority);
            Ok(StatementTxn::Explicit)
        }
        None => {
            exec.txn = None;
            exec.trancount = 0;
            let handle = begin_autocommit_txn(state, &engine.storage, &engine.txn)?;
            apply_deadlock_priority(&engine.txn, &handle, state.options.deadlock_priority);
            Ok(StatementTxn::Autocommit(handle))
        }
    }
}

/// Runs one bound statement in a transaction of its own, committed on success and rolled back
/// on failure (autocommit: `@@TRANCOUNT` stays 0), or in the session transaction when one is
/// open.
///
/// `stamp` is what the binding of the statement read (module documentation, "The schema a
/// statement runs on"); a table of `went_by_this_session`, one an earlier statement of the
/// same batch dropped or rebuilt, does not send the statement back.
pub(crate) fn execute_in_a_transaction(
    state: &SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
    cancel: &CancelHandle,
    bound: &PhysicalStatement,
    (stamp, went_by_this_session): (&SchemaStamp, &[TableId]),
    sink: &mut dyn StatementSink,
) -> (Executed, StatementTxn) {
    if let Err(err) = guard_snapshot_table_read(state, &engine.storage, &engine.txn, bound) {
        sync_exec_session(state, exec);
        let txn = match &state.txn {
            Some(session_txn) => {
                exec.txn = Some(session_txn.handle.clone());
                exec.trancount = session_txn.depth;
                apply_deadlock_priority(
                    &engine.txn,
                    &session_txn.handle,
                    state.options.deadlock_priority,
                );
                StatementTxn::Explicit
            }
            None => StatementTxn::Skipped,
        };
        return (Executed::Ran(Err(err)), txn);
    }
    let txn = match statement_txn(state, engine, exec) {
        Ok(txn) => txn,
        Err(err) => return (Executed::Ran(Err(err)), StatementTxn::Skipped),
    };
    let handle = match &txn {
        StatementTxn::Explicit => state
            .txn
            .as_ref()
            .map(|session_txn| session_txn.handle.clone()),
        StatementTxn::Autocommit(handle) => Some(handle.clone()),
        StatementTxn::Skipped => None,
    };
    let Some(handle) = handle else {
        return (
            Executed::Ran(Err(SqlError::from(InternalError::Bug(
                "execute_in_a_transaction: the session transaction has no handle".to_owned(),
            )))),
            txn,
        );
    };
    let explicit = matches!(txn, StatementTxn::Explicit);
    let held = match hold_schema(state, engine, cancel, &handle, stamp) {
        Ok((held, gone))
            if gone
                .iter()
                .any(|table| !went_by_this_session.contains(table)) =>
        {
            if explicit {
                let _ = engine.txn.end_statement(&handle, &held);
            }
            return (Executed::Rebind, txn);
        }
        Ok((held, _)) => held,
        Err(mut err) => {
            if err.number == LOCK_REQUEST_TIMEOUT
                && matches!(
                    bound,
                    PhysicalStatement::Ddl(DdlStatement::CreateTable { .. })
                )
            {
                err.state = LOCK_TIMEOUT_STATE_OF_A_CREATE;
            }
            return (Executed::Ran(Err(err)), txn);
        }
    };
    let snap = engine.txn.statement_snapshot(&handle);
    let eval = SessionEvalContext::deferred(state, &engine.catalog, &handle)
        .with_schema_waits(&engine.txn, cancel.token().lock_wait());
    let token = cancel.token();
    let mut exec_ctx = ExecContext::scalar(&eval, state.options.to_binder())
        .with_engine(engine.storage.as_ref(), &engine.txn, &snap)
        .with_catalog(&engine.catalog)
        .with_handle(&handle)
        .with_cancel(&token)
        .with_session(exec);
    let mut result = vauban_executor::execute(bound, &mut exec_ctx, sink);
    if explicit
        && let Err(err) = engine.txn.end_statement(&handle, &held)
        && result.is_ok()
    {
        result = Err(err);
    }
    (Executed::Ran(result), txn)
}

/// Takes the `Sch-S` of each table of `stamp` in `handle`, under the `SET LOCK_TIMEOUT` of
/// the session, then looks them up in `storage`; answers the tables whose `Sch-S` it took,
/// and the ones that are gone.
///
/// A refused `Sch-S` gives back the ones taken before it, in a session transaction that
/// outlives the statement.
pub(crate) fn hold_schema(
    state: &SessionState,
    engine: &Engine,
    cancel: &CancelHandle,
    handle: &TxnHandle,
    stamp: &SchemaStamp,
) -> SqlResult<(Vec<TableId>, Vec<TableId>)> {
    let timeout = lock_timeout_from_ms(state.options.lock_timeout);
    let wait = cancel.token().lock_wait();
    let mut held = Vec::new();
    for (table, _) in stamp.tables.iter() {
        match engine
            .txn
            .hold_schema_stability(handle, *table, timeout, &wait)
        {
            Ok(hold) => {
                if hold.taken {
                    held.push(*table);
                }
            }
            Err(err) => {
                if state.txn.is_some() {
                    let _ = engine.txn.end_statement(handle, &held);
                }
                return Err(err);
            }
        }
    }
    Ok((held, stamp.gone_tables(engine.storage.as_ref())))
}

/// Reads back what the executor left, emits the ENVCHANGE an opening or a closing statement
/// deserves, and updates [`SessionState`].
///
/// `txn` is what [`statement_txn`] handed out, `kind` the verb of the statement, and
/// `succeeded` whether the statement returned without an error: the autocommit transaction is
/// committed on success and rolled back otherwise.
pub(crate) fn finish_statement(
    state: &mut SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
    txn: StatementTxn,
    kind: TxnKind,
    succeeded: bool,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    if let StatementTxn::Autocommit(handle) = txn {
        if succeeded {
            engine.txn.commit(handle)?;
        } else {
            let _ = engine.txn.rollback(handle);
        }
    }
    match (state.txn.take(), exec.txn.clone()) {
        (None, Some(handle)) => {
            apply_deadlock_priority(&engine.txn, &handle, state.options.deadlock_priority);
            // A transaction the wire announces carries a non-zero descriptor: an exhausted
            // descriptor space is an internal error, not a silent 0 on an open transaction.
            let Some(descriptor) = state.allocate_transaction_descriptor() else {
                return Err(bug(
                    "finish_statement: the transaction descriptor space is exhausted",
                ));
            };
            state.txn = Some(SessionTxn {
                handle,
                descriptor,
                depth: exec.trancount,
            });
            state.trancount = exec.trancount;
            state.transaction_descriptor = descriptor;
            sink.env_change(&EnvChange::BeginTransaction(descriptor))?;
        }
        (Some(existing), None) => {
            let descriptor = existing.descriptor;
            state.trancount = 0;
            state.transaction_descriptor = 0;
            let change = match kind {
                TxnKind::Commit => EnvChange::CommitTransaction(descriptor),
                _ => EnvChange::RollbackTransaction(descriptor),
            };
            sink.env_change(&change)?;
        }
        (Some(existing), Some(handle)) => {
            let descriptor = existing.descriptor;
            state.txn = Some(SessionTxn {
                handle,
                descriptor,
                depth: exec.trancount,
            });
            state.trancount = exec.trancount;
        }
        (None, None) => {
            state.trancount = 0;
        }
    }
    Ok(())
}

/// What a batch does when it ends with a transaction still open: nothing.
///
/// The transaction stays open for the next batch and raises nothing: 266 belongs to an
/// `EXECUTE` whose transaction count moved, the nested-batch work.
pub(crate) fn end_of_batch(state: &SessionState, sink: &mut dyn ResultSink) -> SqlResult<()> {
    let _ = (state, sink);
    Ok(())
}

/// Rolls back the session transaction after an error whose catalogue entry says so
/// ([`vauban_errors::ErrorBehavior::rolls_back_transaction`], the victim of a deadlock), and
/// announces it with the ENVCHANGE of type 10, which follows the ERROR token of that error.
/// The writes of the transaction are undone and `@@TRANCOUNT` reads 0 afterwards
/// (`tests/deadlock_victim.rs`). Nothing is sent when no transaction is open: an autocommit
/// statement was rolled back by [`finish_statement`], and `XACT_ABORT ON` by the executor.
pub(crate) fn roll_back_after_error(
    state: &mut SessionState,
    engine: &Engine,
    exec: &mut ExecSession,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    exec.txn = None;
    exec.trancount = 0;
    exec.stmt_savepoint = None;
    exec.savepoints.clear();
    let Some(txn) = state.txn.take() else {
        state.trancount = 0;
        return Ok(());
    };
    state.trancount = 0;
    state.transaction_descriptor = 0;
    engine.txn.rollback(txn.handle)?;
    sink.env_change(&EnvChange::RollbackTransaction(txn.descriptor))
}

/// Rolls back the session transaction, for a connection that ends.
pub(crate) fn rollback_all(state: &mut SessionState, engine: &Engine) -> SqlResult<()> {
    if let Some(txn) = state.txn.take() {
        engine.txn.rollback(txn.handle)?;
    }
    state.trancount = 0;
    state.transaction_descriptor = 0;
    Ok(())
}

/// The internal error 50000 for a broken precondition.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
