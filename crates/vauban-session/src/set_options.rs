//! `SetOptions`, `DateFormat`, `IsolationLevel`, and how a `SET <option>` statement changes
//! them.
//!
//! [`apply_set_option`] reads a `SET` statement as the parser built it and updates the
//! session state, without any other effect: an unknown or malformed `SET` is `Ignored` and
//! logged at `debug`. The session applies it where its program reaches it, written at the
//! top level of a batch or held by an `IF`, a `WHILE` or a `BEGIN … END`
//! (`tests/set_options_effects.rs`, `a_set_held_by_a_block_or_a_branch_changes_the_options`).

use std::sync::Arc;
use tracing::debug;
use vauban_binder::SessionOptions as BinderOptions;
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{SqlError, SqlResult};

use vauban_executor::ExecSession;
use vauban_parser::{
    Expr, Literal, ObjectName, ParseOptions, SetOptionStatement, SetOptionValue, UnaryOp,
};
use vauban_planner::{PhysicalPlan, PhysicalStatement};
use vauban_storage::{DbId, Storage};
use vauban_txn::{LockTimeout, TransactionManager, TxnHandle};

use crate::state::{IdentityInsertTable, SessionState};

pub use vauban_txn::IsolationLevel;

/// Options of a session settable with `SET`.
///
/// The defaults are those of a client connection: the ones the ODBC and OLE DB drivers set
/// when they connect, which the TDS login sequence mirrors.
///
/// # What each option does
///
/// Every field carries one of three mentions, and `tests/set_options_effects.rs`
/// (`every_option_is_documented`) fails when one has none:
///
/// - **Honoured**: the option changes what a client observes, and a test proves it;
/// - **No effect**: nothing the engine runs can reveal it (no lock, no cursor), so
///   honouring it would be pretending;
/// - **Deliberate difference from SQL Server**: a client can tell the difference and the
///   engine does not follow SQL Server.
///
/// Where an option is *read* decides whether a `SET` reaches the statements that follow it
/// in the **same batch**. Options read at execution (`to_binder`, `EvalContext`) do: `SET
/// NOCOUNT ON; SELECT 1;` clears `DONE_COUNT` on that very `SELECT` here as on SQL Server.
/// `QUOTED_IDENTIFIER` is read during parsing too: batch preparation simulates each `SET`
/// before reparsing the following statement (`parse_options`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOptions {
    /// `SET ANSI_NULLS`: comparisons with NULL follow ISO.
    ///
    /// Deliberate difference from SQL Server: passed to the binder by
    /// [`to_binder`](Self::to_binder) and read by nobody, `OFF` behaves as `ON`. `SET
    /// ANSI_NULLS OFF; SELECT CASE WHEN NULL = NULL THEN 1 ELSE 0 END;` answers `1` on SQL
    /// Server and `0` here. The option is announced as deprecated, with `ON` as the
    /// behaviour that remains, which is why that one alone is implemented.
    pub ansi_nulls: bool,
    /// `SET ANSI_PADDING`: trailing blanks and zeros are kept.
    ///
    /// No effect: the option governs how a **column** stores a value, and the storage
    /// does not read it.
    pub ansi_padding: bool,
    /// `SET ANSI_WARNINGS`: ISO warnings on NULL aggregation, overflow, division by zero.
    ///
    /// Deliberate difference from SQL Server: passed to the binder and read by nobody; a
    /// division by zero raises 8134 regardless of this and `ARITHABORT`, where SQL Server
    /// answers `NULL` once **both** are `OFF`.
    pub ansi_warnings: bool,
    /// `SET ANSI_NULL_DFLT_ON`: columns without `NULL`/`NOT NULL` are nullable.
    ///
    /// No effect: `CREATE TABLE` does not read it.
    pub ansi_null_dflt_on: bool,
    /// `SET ARITHABORT`: overflow and division by zero end the query.
    ///
    /// Deliberate difference from SQL Server: passed to the binder and read by nobody; the
    /// engine behaves as `ON` under both values.
    pub arithabort: bool,
    /// `SET ARITHIGNORE`: overflow and division by zero return no message.
    ///
    /// Deliberate difference from SQL Server, the same as `ARITHABORT`: the engine does not
    /// read it. SQL Server raises 8134 on `SELECT 1 / 0;` with `ARITHIGNORE ON`, `ARITHABORT
    /// ON` and `ANSI_WARNINGS OFF`, raises it too with `ARITHIGNORE ON`, `ARITHABORT OFF`
    /// and `ANSI_WARNINGS ON`, and answers `NULL` with the three at `ON`/`OFF`/`OFF`.
    pub arithignore: bool,
    /// `SET CONCAT_NULL_YIELDS_NULL`: `NULL + 'a'` is NULL.
    ///
    /// Honoured: [`to_binder`](Self::to_binder) hands it to the executor, whose
    /// concatenation treats a `NULL` operand as the empty string under `OFF`.
    /// `SET CONCAT_NULL_YIELDS_NULL OFF; SELECT 'a' + CAST(NULL AS varchar(1));` answers
    /// `a`, and `NULL` under `ON` (`tests/set_options_effects.rs`).
    pub concat_null_yields_null: bool,
    /// `SET QUOTED_IDENTIFIER`: `"..."` delimits identifiers.
    ///
    /// Honoured by the parser ([`parse_options`](Self::parse_options)): with `OFF`,
    /// `SELECT "a"` answers the string `a`; with `ON`, error 207. Batch preparation applies
    /// the option in both directions to the statements following the `SET`.
    pub quoted_identifier: bool,
    /// `SET NOCOUNT`: no "N rows affected" message, DONE without row count.
    ///
    /// Honoured (`batch.rs`): under `ON`, the DONE of a `SELECT` goes out without
    /// `DONE_COUNT`, and `@@ROWCOUNT` is posted all the same: `SET NOCOUNT ON; SELECT 1;`
    /// answers a DONE with status `0x0000` where `SET NOCOUNT OFF; SELECT 1;` answers
    /// `0x0010` (`DONE_COUNT`), and `SET NOCOUNT ON; SELECT 1; SELECT @@ROWCOUNT;` still
    /// answers `1`. The option takes effect in its own batch, both ways, and stays on for
    /// the batches that follow. `tests/set_options_effects.rs` pins the sequence.
    pub nocount: bool,
    /// `SET XACT_ABORT`: a run-time error rolls back the transaction.
    ///
    /// Honoured: [`sync_exec_session`](sync_exec_session) copies it to
    /// [`ExecSession::xact_abort`], and the executor rolls the open transaction back on
    /// a statement error when it is `ON` (`tests/isolation_options.rs`,
    /// `xact_abort_aborts_the_transaction`).
    pub xact_abort: bool,
    /// `SET IMPLICIT_TRANSACTIONS`: a transaction opens implicitly before some statements.
    ///
    /// No effect: the session opens no implicit transaction. A `SELECT` without `FROM`
    /// opens nothing on SQL Server either, so `SET IMPLICIT_TRANSACTIONS ON; SELECT 1;
    /// SELECT @@TRANCOUNT;` answers `0` on both.
    pub implicit_transactions: bool,
    /// `SET CURSOR_CLOSE_ON_COMMIT`: open cursors close at commit.
    ///
    /// No effect: no cursor.
    pub cursor_close_on_commit: bool,
    /// `SET NUMERIC_ROUNDABORT`: loss of precision is an error.
    ///
    /// Deliberate difference from SQL Server: passed to the binder and read by nobody.
    /// `SET NUMERIC_ROUNDABORT ON; SET ARITHABORT ON; SELECT CAST(1.15 AS decimal(3, 1));`
    /// raises 8115 on SQL Server and answers `1.2` here.
    pub numeric_roundabort: bool,
    /// `SET TEXTSIZE`: bytes returned for `text`, `ntext`, `varchar(max)`... columns.
    ///
    /// Deliberate difference from SQL Server: stored and not read. `SET TEXTSIZE 2; SELECT
    /// CAST('abcdef' AS varchar(max));` answers `ab` on SQL Server and `abcdef` here, and
    /// `SET TEXTSIZE 0` restores a limit of 4096 there, not "unlimited": a 5000-character
    /// `varchar(max)` comes out cut to 4096 characters there, whole here.
    pub textsize: i32,
    /// `SET LOCK_TIMEOUT`: milliseconds to wait for a lock, `-1` waits forever.
    ///
    /// Honoured: [`sync_exec_session`](sync_exec_session) copies it to
    /// [`ExecSession::lock_timeout`] as [`LockTimeout::Infinite`] for `-1`,
    /// [`LockTimeout::NoWait`] for `0`, and [`LockTimeout::Millis`] otherwise
    /// (`tests/isolation_options.rs`, `lock_timeout_reaches_the_lock_manager`).
    pub lock_timeout: i32,
    /// `SET DATEFORMAT`: order of the date parts when parsing strings.
    ///
    /// Deliberate difference from SQL Server: stored and not read, the conversion of a
    /// string to a date follows `mdy` under the six values. `SET DATEFORMAT dmy; SELECT
    /// CAST('02/01/2000' AS date);` answers `2000-01-02` on SQL Server and `2000-02-01`
    /// here.
    pub dateformat: DateFormat,
    /// `SET DATEFIRST`: first day of the week, 1 = Monday ... 7 = Sunday.
    ///
    /// Honoured: `sysfn` reads it through `EvalContext::datefirst`
    /// (`eval_context.rs`) for `DATEPART(weekday, ...)` and `DATEPART(week, ...)`. `SET
    /// DATEFIRST 1; SELECT DATEPART(weekday, CAST('2000-01-02' AS date));` answers `7`,
    /// and `1` under `DATEFIRST 7` (`tests/set_options_effects.rs`).
    pub datefirst: u8,
    /// `SET LANGUAGE`: language of the messages and of the date parsing.
    ///
    /// Deliberate difference from SQL Server: exposed by `EvalContext::language` and read
    /// by nobody; messages stay in English (module `errors`) and dates keep the `mdy`
    /// order, where SQL Server translates the message of `SELECT 1 / 0;` under `SET
    /// LANGUAGE French`.
    pub language: String,
    /// `SET DEADLOCK_PRIORITY`: `-10..=10`, `LOW` = -5, `NORMAL` = 0, `HIGH` = 5.
    ///
    /// Honoured for the open transaction: [`apply_deadlock_priority`](apply_deadlock_priority)
    /// forwards the value to the transaction manager when a handle is opened
    /// (`tests/isolation_options.rs`, `deadlock_priority_reaches_the_lock_manager`).
    pub deadlock_priority: i8,
}

impl Default for SetOptions {
    fn default() -> Self {
        Self {
            // The ODBC driver and the OLE DB provider set it ON when connecting.
            ansi_nulls: true,
            // Set ON by the ODBC driver and the OLE DB provider.
            ansi_padding: true,
            // Set ON by the ODBC driver and the OLE DB provider.
            ansi_warnings: true,
            // Set ON by the ODBC driver and the OLE DB provider.
            ansi_null_dflt_on: true,
            // The default of a client connection is OFF (Management Studio turns it ON);
            // the ODBC and OLE DB drivers do not set it.
            arithabort: false,
            // Default OFF.
            arithignore: false,
            // Set ON by the ODBC driver and the OLE DB provider.
            concat_null_yields_null: true,
            // Set ON by the ODBC driver and the OLE DB provider.
            quoted_identifier: true,
            // Default OFF.
            nocount: false,
            // Default OFF.
            xact_abort: false,
            // Default OFF (the ODBC driver and the OLE DB provider set it OFF when
            // connecting).
            implicit_transactions: false,
            // Default OFF (the ODBC driver and the OLE DB provider set it OFF when
            // connecting).
            cursor_close_on_commit: false,
            // Default OFF.
            numeric_roundabort: false,
            // The server default is 4096 bytes; the drivers raise it themselves with a
            // `SET TEXTSIZE` after the login.
            textsize: 4096,
            // Default -1, wait forever.
            lock_timeout: -1,
            // The default of the `us_english` language is `mdy`.
            dateformat: DateFormat::Mdy,
            // The default of the `us_english` language is 7 (Sunday).
            datefirst: 7,
            // The default language of the login, `us_english` here.
            language: DEFAULT_LANGUAGE.into(),
            // Default NORMAL, that is 0.
            deadlock_priority: 0,
        }
    }
}

impl SetOptions {
    /// The options the **parser** depends on.
    ///
    /// `QUOTED_IDENTIFIER` alone: it decides whether `"a"` is a delimited identifier or a
    /// character string literal, which is a lexing question and nothing else.
    ///
    /// During batch preparation, `session` advances a cloned option state after every `SET`
    /// and reparses the following statement with this value. The live option is changed only
    /// later, if execution reaches that `SET`.
    #[must_use]
    pub fn parse_options(&self) -> ParseOptions {
        ParseOptions {
            quoted_identifier: self.quoted_identifier,
        }
    }

    /// The options **typing and evaluation** depend on.
    ///
    /// The five booleans of `binder::SessionOptions` are the only contract shared with
    /// `binder` and `executor`, which know nothing of this crate. The values are always
    /// those of the live connection: `binder::SessionOptions::default()` is a value for
    /// the tests of that crate (it has `arithabort: true`), not a fallback for a session,
    /// whose default is `arithabort: false`.
    #[must_use]
    pub fn to_binder(&self) -> BinderOptions {
        BinderOptions {
            ansi_nulls: self.ansi_nulls,
            ansi_warnings: self.ansi_warnings,
            arithabort: self.arithabort,
            concat_null_yields_null: self.concat_null_yields_null,
            numeric_roundabort: self.numeric_roundabort,
            identity_insert: None,
        }
    }
}

impl SessionState {
    /// The binder options of this session, including the table `SET IDENTITY_INSERT` opened.
    ///
    /// `to_binder` carries the five booleans; this adds the identity table so `INSERT`
    /// can accept an explicit value (`tests/set_options_effects.rs`).
    pub(crate) fn binder_options(&self) -> BinderOptions {
        match &self.identity_insert {
            Some(table) => self.options.to_binder().with_identity_insert(
                &table.database,
                &table.schema,
                &table.name,
            ),
            None => self.options.to_binder(),
        }
    }
}

/// The default isolation level of a session: `READ COMMITTED`.
pub(crate) fn default_isolation() -> IsolationLevel {
    IsolationLevel::ReadCommitted
}

/// The [`LockTimeout`] that matches a `SET LOCK_TIMEOUT` value.
#[must_use]
pub fn lock_timeout_from_ms(ms: i32) -> LockTimeout {
    match ms {
        -1 => LockTimeout::Infinite,
        0 => LockTimeout::NoWait,
        n => LockTimeout::Millis(u32::try_from(n).unwrap_or(u32::MAX)),
    }
}

/// Copies the session options the executor reads into `exec`.
pub fn sync_exec_session(state: &SessionState, exec: &mut ExecSession) {
    exec.isolation = state.isolation;
    exec.xact_abort = state.options.xact_abort;
    exec.lock_timeout = lock_timeout_from_ms(state.options.lock_timeout);
}

/// The `DbId` of the session's current database.
pub(crate) fn current_db_id(storage: &Arc<dyn Storage>, database: &str) -> SqlResult<DbId> {
    storage
        .databases()?
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(database))
        .map(|(id, _)| id)
        .ok_or_else(|| SqlError::database_not_found(database))
}

/// Turns the internal refusal of [`TransactionManager::begin_in`] into error 3952.
#[must_use]
pub(crate) fn map_begin_in_error(database: &str, err: SqlError) -> SqlError {
    if err.number == 50000 && err.message.contains("ALLOW_SNAPSHOT_ISOLATION") {
        return SqlError::snapshot_isolation_not_allowed(database);
    }
    err
}

/// Opens a one-statement transaction on the session's database at its isolation level.
///
/// When the level is [`IsolationLevel::Snapshot`] and the database has not turned
/// `ALLOW_SNAPSHOT_ISOLATION` on, the transaction manager refuses `begin_in`; this opens
/// at the session level on the default database instead and leaves 3952 to
/// [`guard_snapshot_table_read`] on the first user-table read
/// (`tests/isolation_options.rs`, `snapshot_without_the_database_option`).
pub fn begin_autocommit_txn(
    state: &SessionState,
    storage: &Arc<dyn Storage>,
    txn: &TransactionManager,
) -> SqlResult<TxnHandle> {
    let db = current_db_id(storage, &state.database)?;
    txn.begin_in(db, state.isolation).or_else(|err| {
        if err.number == 50000 && err.message.contains("ALLOW_SNAPSHOT_ISOLATION") {
            Ok(txn.begin(state.isolation))
        } else {
            Err(map_begin_in_error(&state.database, err))
        }
    })
}

/// True when `plan` reads at least one user table.
fn plan_reads_user_table(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::OneRow | PhysicalPlan::Values { .. } => false,
        PhysicalPlan::TableScan { .. } | PhysicalPlan::IndexSeek { .. } => true,
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::HashAggregate { input, .. }
        | PhysicalPlan::StreamAggregate { input, .. }
        | PhysicalPlan::SubqueryEval { input, .. } => plan_reads_user_table(input),
        PhysicalPlan::Distinct(input) => plan_reads_user_table(input),
        PhysicalPlan::NestedLoopJoin { outer, inner, .. }
        | PhysicalPlan::HashJoin {
            build: outer,
            probe: inner,
            ..
        } => plan_reads_user_table(outer) || plan_reads_user_table(inner),
        PhysicalPlan::Union { inputs, .. }
        | PhysicalPlan::Except { inputs, .. }
        | PhysicalPlan::Intersect { inputs, .. } => inputs.iter().any(plan_reads_user_table),
    }
}

/// True when `bound` reads at least one user table.
fn statement_reads_user_table(bound: &PhysicalStatement) -> bool {
    match bound {
        PhysicalStatement::Query(plan) => plan_reads_user_table(plan),
        // `SELECT @x = col FROM t` reads its source the way the same `FROM` read as a
        // query does; the assigned values hang off that plan and add no source of rows.
        PhysicalStatement::SelectAssign { input, .. } => plan_reads_user_table(input),
        PhysicalStatement::Insert(_)
        | PhysicalStatement::SelectInto(_)
        | PhysicalStatement::Update(_)
        | PhysicalStatement::Delete(_) => true,
        PhysicalStatement::If { then_, else_, .. } => {
            statement_reads_user_table(then_)
                || else_
                    .as_ref()
                    .is_some_and(|branch| statement_reads_user_table(branch))
        }
        PhysicalStatement::While { body, .. } => statement_reads_user_table(body),
        PhysicalStatement::Block(stmts) => stmts.iter().any(statement_reads_user_table),
        PhysicalStatement::Ddl(_)
        | PhysicalStatement::Use { .. }
        | PhysicalStatement::SetVariable { .. }
        | PhysicalStatement::Declare(_)
        | PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Goto { .. }
        | PhysicalStatement::Label { .. }
        | PhysicalStatement::Return(_)
        | PhysicalStatement::Print(_)
        | PhysicalStatement::Transaction(_)
        | PhysicalStatement::Execute(_)
        | PhysicalStatement::SetOption(_) => false,
        // A statement that is not bound yet gets its plan when the control of flow
        // reaches it (`vauban_executor`, `control.rs`), and the tables it names are
        // resolved there.
        PhysicalStatement::Deferred(_) => false,
    }
}

/// Refuses a user-table read under [`IsolationLevel::Snapshot`] when the current database
/// has not turned `ALLOW_SNAPSHOT_ISOLATION` on.
///
/// # Errors
///
/// Error 3952, severity 16, state 1 (`tests/isolation_options.rs`,
/// `snapshot_without_the_database_option`).
pub(crate) fn guard_snapshot_table_read(
    state: &SessionState,
    storage: &Arc<dyn Storage>,
    txn: &TransactionManager,
    bound: &PhysicalStatement,
) -> SqlResult<()> {
    if !statement_reads_user_table(bound) {
        return Ok(());
    }
    let db = current_db_id(storage, &state.database)?;
    if state.isolation == IsolationLevel::Snapshot
        && !txn.versioning_options(db).allow_snapshot_isolation
    {
        return Err(SqlError::snapshot_isolation_not_allowed(&state.database));
    }
    Ok(())
}

/// Forwards the session's `SET DEADLOCK_PRIORITY` to an open handle.
pub(crate) fn apply_deadlock_priority(txn: &TransactionManager, handle: &TxnHandle, priority: i8) {
    txn.set_deadlock_priority(handle, i16::from(priority));
}

/// Default session language (`sys.syslanguages`).
const DEFAULT_LANGUAGE: &str = "us_english";

/// Order of the day, month and year parts when a string is converted to a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateFormat {
    /// `mdy`, the default of `us_english`.
    #[default]
    Mdy,
    /// `dmy`.
    Dmy,
    /// `ymd`.
    Ymd,
    /// `ydm`.
    Ydm,
    /// `myd`.
    Myd,
    /// `dym`.
    Dym,
}

impl DateFormat {
    /// The format named `name` (case-insensitive), `None` if it is not one of the six.
    fn parse(name: &str) -> Option<Self> {
        let format = match name.to_ascii_lowercase().as_str() {
            "mdy" => Self::Mdy,
            "dmy" => Self::Dmy,
            "ymd" => Self::Ymd,
            "ydm" => Self::Ydm,
            "myd" => Self::Myd,
            "dym" => Self::Dym,
            _ => return None,
        };
        Some(format)
    }
}

/// Result of [`apply_set_option`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    /// The statement was recognised and the state updated.
    Applied,
    /// The statement is not a recognised `SET`; the state is unchanged.
    Ignored,
    /// The statement was recognised and refused; the state is unchanged.
    Failed(SqlError),
}

/// Applies a `SET <option>` statement, as the parser built it, to `state`.
///
/// Recognised: `SET <opt> [, <opt>...] ON|OFF` for the boolean options of `SetOptions`,
/// `SET ANSI_DEFAULTS ON|OFF`, `SET TEXTSIZE n`, `SET LOCK_TIMEOUT n`, `SET DATEFORMAT f`,
/// `SET DATEFIRST n`, `SET LANGUAGE name`, `SET DEADLOCK_PRIORITY LOW|NORMAL|HIGH|n`,
/// `SET TRANSACTION ISOLATION LEVEL ...` and `SET IDENTITY_INSERT <table> ON|OFF`, a number
/// or a name being written as a literal. Anything else (`SET NOEXEC`, `SET ROWCOUNT`,
/// `SET SHOWPLAN_*`, an unknown option, a value out of range, a variable) is `Ignored` with
/// a `debug` log and leaves the state untouched: no error here. A second
/// `IDENTITY_INSERT ON` while another table is open is `Failed` with 8107
/// (`tests/set_options_effects.rs`).
pub(crate) fn apply_set_option(
    state: &mut SessionState,
    set: &SetOptionStatement,
    catalog: Option<&CatalogSnapshot>,
) -> SetOutcome {
    let applied = match (&set.table, set.options.as_slice()) {
        (Some(table), [(_, value)]) => apply_identity_insert(state, table, value, catalog),
        (None, [(name, value)]) => Ok(apply_known_option(state, name, value)),
        (None, entries) => Ok(apply_boolean_list(&mut state.options, entries)),
        (Some(_), _) => Ok(None),
    };
    match applied {
        Ok(Some(())) => SetOutcome::Applied,
        Ok(None) => {
            debug!(statement = %set, "SET statement ignored");
            SetOutcome::Ignored
        }
        Err(err) => SetOutcome::Failed(err),
    }
}

/// Default schema of a login, `dbo` until the catalogue knows better.
const DEFAULT_SCHEMA: &str = "dbo";

/// Refuses `SET IDENTITY_INSERT … ON` or `… OFF` when the name resolves to nothing or to
/// a table without an identity column. An unknown table is checked before a missing identity.
fn validate_identity_insert_target(
    catalog: &CatalogSnapshot,
    database: &str,
    schema: &str,
    name: &str,
) -> Result<(), SqlError> {
    let qualified = format!("{schema}.{name}");
    let Some(object) = catalog.resolve_object(database, Some(schema), name, DEFAULT_SCHEMA) else {
        return Err(SqlError::cannot_find_object_for_identity_insert(&qualified));
    };
    let Some(meta) = catalog.table(object.id) else {
        return Err(SqlError::cannot_find_object_for_identity_insert(&qualified));
    };
    if !meta.columns.iter().any(|column| column.identity.is_some()) {
        return Err(SqlError::identity_insert_table_has_no_identity(&qualified));
    }
    Ok(())
}

/// `SET IDENTITY_INSERT <table> {ON|OFF}`. A second `ON` while another table is open
/// answers 8107 and leaves the first table open.
fn apply_identity_insert(
    state: &mut SessionState,
    table: &ObjectName,
    value: &SetOptionValue,
    catalog: Option<&CatalogSnapshot>,
) -> Result<Option<()>, SqlError> {
    let on = match value {
        SetOptionValue::On => true,
        SetOptionValue::Off => false,
        SetOptionValue::Value(_) | SetOptionValue::Word(_) => return Ok(None),
    };
    let Some(target) = identity_target(table, &state.database) else {
        return Ok(None);
    };
    if let Some(catalog) = catalog {
        validate_identity_insert_target(
            catalog,
            &target.table.database,
            &target.table.schema,
            &target.table.name,
        )?;
    }
    if on {
        if let Some(open) = &state.identity_insert {
            if open.same_as(&target.table) {
                return Ok(Some(()));
            }
            return Err(SqlError::identity_insert_already_on(
                &open.database,
                &open.schema,
                &open.name,
                &target.written,
            ));
        }
        state.identity_insert = Some(target.table);
    } else if state
        .identity_insert
        .as_ref()
        .is_some_and(|open| open.same_as(&target.table))
    {
        state.identity_insert = None;
    }
    Ok(Some(()))
}

/// A table named by `SET IDENTITY_INSERT`, read two ways.
struct IdentityTarget {
    /// The three parts the state keeps and compares, delimiters removed.
    table: IdentityInsertTable,
    /// The same name with as many parts as the statement wrote, delimiters removed: what
    /// 8107 puts in its fourth specifier (`tests/set_options_effects.rs`,
    /// `identity_insert_names_the_refused_table_without_its_delimiters`).
    written: String,
}

/// The table `name` designates: a missing schema is `dbo`, a missing database is
/// `current_db`. `None` for a name that carries a linked server part.
///
/// What the state keeps is the name without its delimiters, so that a later `OFF` written
/// another way closes the option that `[dbo].[t] ON` opened
/// (`tests/set_options_effects.rs`, `identity_insert_opened_on_a_delimited_name_is_closed_by_a_canonical_off`).
fn identity_target(name: &ObjectName, current_db: &str) -> Option<IdentityTarget> {
    if name.server.is_some() {
        return None;
    }
    let written: Vec<&str> = [name.database.as_ref(), name.schema.as_ref()]
        .into_iter()
        .map(|part| part.map(|part| part.value.as_str()))
        .skip_while(Option::is_none)
        .map(|part| part.unwrap_or(""))
        .chain(std::iter::once(name.name.value.as_str()))
        .collect();
    Some(IdentityTarget {
        table: IdentityInsertTable {
            database: name
                .database
                .as_ref()
                .map_or_else(|| current_db.to_owned(), |part| part.value.clone()),
            schema: name
                .schema
                .as_ref()
                .map_or_else(|| DEFAULT_SCHEMA.to_owned(), |part| part.value.clone()),
            name: name.name.value.clone(),
        },
        written: written.join("."),
    })
}

/// `Some(())` once the state is updated by the option `name` set to `value`; `None` leaves
/// it untouched.
fn apply_known_option(state: &mut SessionState, name: &str, value: &SetOptionValue) -> Option<()> {
    let options = &mut state.options;
    match normalised(name).as_str() {
        "TRANSACTION ISOLATION LEVEL" => state.isolation = isolation_level(value)?,
        "TEXTSIZE" => {
            let size = i32::try_from(integer(value)?).ok()?;
            // 0 resets the default of 4096 bytes.
            options.textsize = if size == 0 { 4096 } else { size };
        }
        "LOCK_TIMEOUT" => options.lock_timeout = i32::try_from(integer(value)?).ok()?,
        "DATEFORMAT" => options.dateformat = DateFormat::parse(&name_value(value)?)?,
        "DATEFIRST" => {
            let day = u8::try_from(integer(value)?).ok()?;
            if !(1..=7).contains(&day) {
                return None;
            }
            options.datefirst = day;
        }
        "LANGUAGE" => options.language = name_value(value)?,
        "DEADLOCK_PRIORITY" => options.deadlock_priority = deadlock_priority(value)?,
        "ANSI_DEFAULTS" => {
            let on = on_off(value)?;
            // The seven options `ANSI_DEFAULTS` controls.
            options.ansi_nulls = on;
            options.ansi_null_dflt_on = on;
            options.ansi_padding = on;
            options.ansi_warnings = on;
            options.cursor_close_on_commit = on;
            options.implicit_transactions = on;
            options.quoted_identifier = on;
        }
        _ => return apply_boolean_list(options, &[(name.to_owned(), value.clone())]),
    }
    Some(())
}

/// `<opt> [, <opt>...] ON|OFF` over the boolean options. The names are validated before one
/// is applied: a partial application would leave an inconsistent state on `None`
/// (`unknown_or_malformed_statements_are_ignored_without_change`, `SET ANSI_NULLS, FOO ON`).
fn apply_boolean_list(
    options: &mut SetOptions,
    entries: &[(String, SetOptionValue)],
) -> Option<()> {
    let (_, value) = entries.first()?;
    let on = on_off(value)?;
    if entries
        .iter()
        .any(|(name, value)| value != &entries[0].1 || bool_option(options, name).is_none())
    {
        return None;
    }
    for (name, _) in entries {
        if let Some(field) = bool_option(options, name) {
            *field = on;
        }
    }
    Some(())
}

/// An option name or a value upper-cased, its words separated by one space:
/// `transaction  isolation level` reads `TRANSACTION ISOLATION LEVEL`.
fn normalised(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The field of `options` named `name` (case-insensitive) when it is a boolean option.
fn bool_option<'a>(options: &'a mut SetOptions, name: &str) -> Option<&'a mut bool> {
    let field = match name.to_ascii_uppercase().as_str() {
        "ANSI_NULLS" => &mut options.ansi_nulls,
        "ANSI_PADDING" => &mut options.ansi_padding,
        "ANSI_WARNINGS" => &mut options.ansi_warnings,
        "ANSI_NULL_DFLT_ON" => &mut options.ansi_null_dflt_on,
        "ARITHABORT" => &mut options.arithabort,
        "ARITHIGNORE" => &mut options.arithignore,
        "CONCAT_NULL_YIELDS_NULL" => &mut options.concat_null_yields_null,
        "QUOTED_IDENTIFIER" => &mut options.quoted_identifier,
        "NOCOUNT" => &mut options.nocount,
        "XACT_ABORT" => &mut options.xact_abort,
        "IMPLICIT_TRANSACTIONS" => &mut options.implicit_transactions,
        "CURSOR_CLOSE_ON_COMMIT" => &mut options.cursor_close_on_commit,
        "NUMERIC_ROUNDABORT" => &mut options.numeric_roundabort,
        _ => return None,
    };
    Some(field)
}

/// `READ UNCOMMITTED|READ COMMITTED|REPEATABLE READ|SNAPSHOT|SERIALIZABLE`.
fn isolation_level(value: &SetOptionValue) -> Option<IsolationLevel> {
    let SetOptionValue::Word(level) = value else {
        return None;
    };
    let level = match normalised(level).as_str() {
        "READ UNCOMMITTED" => IsolationLevel::ReadUncommitted,
        "READ COMMITTED" => IsolationLevel::ReadCommitted,
        "REPEATABLE READ" => IsolationLevel::RepeatableRead,
        "SNAPSHOT" => IsolationLevel::Snapshot,
        "SERIALIZABLE" => IsolationLevel::Serializable,
        _ => return None,
    };
    Some(level)
}

/// `LOW|NORMAL|HIGH|n` with `n` in `-10..=10`.
fn deadlock_priority(value: &SetOptionValue) -> Option<i8> {
    let priority = match value {
        SetOptionValue::Word(word) => match word.to_ascii_uppercase().as_str() {
            "LOW" => -5,
            "NORMAL" => 0,
            "HIGH" => 5,
            _ => return None,
        },
        _ => i8::try_from(integer(value)?).ok()?,
    };
    (-10..=10).contains(&priority).then_some(priority)
}

/// `ON` or `OFF`.
fn on_off(value: &SetOptionValue) -> Option<bool> {
    match value {
        SetOptionValue::On => Some(true),
        SetOptionValue::Off => Some(false),
        SetOptionValue::Value(_) | SetOptionValue::Word(_) => None,
    }
}

/// An integer literal, signed or not: `5000`, `-1`, `+3`.
fn integer(value: &SetOptionValue) -> Option<i64> {
    let SetOptionValue::Value(expr) = value else {
        return None;
    };
    integer_expr(expr)
}

fn integer_expr(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(Literal::Integer(digits), _) => digits.parse().ok(),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr,
            ..
        } => integer_expr(expr)?.checked_neg(),
        Expr::Unary {
            op: UnaryOp::Plus,
            expr,
            ..
        } => integer_expr(expr),
        _ => None,
    }
}

/// A name written as a word or as a `'...'`/`N'...'` string: `SET LANGUAGE us_english`,
/// `SET DATEFORMAT 'ymd'`.
fn name_value(value: &SetOptionValue) -> Option<String> {
    match value {
        SetOptionValue::Word(word) => Some(word.clone()),
        SetOptionValue::Value(Expr::Literal(Literal::Str { value, .. }, _)) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SessionState {
        SessionState::new(51)
    }

    /// The one `SET <option>` of `text`, as the parser builds it.
    fn parsed(text: &str) -> SetOptionStatement {
        let batch = vauban_parser::parse_batch(text, &ParseOptions::default())
            .unwrap_or_else(|err| panic!("{text} parses: {}", err.message));
        match batch.statements.as_slice() {
            [vauban_parser::Statement::SetOption(set)] => (**set).clone(),
            other => panic!("{text} is one SET of an option, got {other:?}"),
        }
    }

    fn apply(state: &mut SessionState, stmt: &str) -> SetOutcome {
        apply_set_option(state, &parsed(stmt), None)
    }

    #[test]
    fn defaults_are_those_of_a_client_connection() {
        let options = SetOptions::default();
        assert_eq!(
            options,
            SetOptions {
                ansi_nulls: true,
                ansi_padding: true,
                ansi_warnings: true,
                ansi_null_dflt_on: true,
                arithabort: false,
                arithignore: false,
                concat_null_yields_null: true,
                quoted_identifier: true,
                nocount: false,
                xact_abort: false,
                implicit_transactions: false,
                cursor_close_on_commit: false,
                numeric_roundabort: false,
                textsize: 4096,
                lock_timeout: -1,
                dateformat: DateFormat::Mdy,
                datefirst: 7,
                language: "us_english".into(),
                deadlock_priority: 0,
            }
        );
        assert_eq!(default_isolation(), IsolationLevel::ReadCommitted);
        assert_eq!(DateFormat::default(), DateFormat::Mdy);
    }

    #[test]
    fn boolean_option_list() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET ANSI_NULLS, QUOTED_IDENTIFIER OFF"),
            SetOutcome::Applied
        );
        assert!(!state.options.ansi_nulls);
        assert!(!state.options.quoted_identifier);
        let expected = SetOptions {
            ansi_nulls: false,
            quoted_identifier: false,
            ..SetOptions::default()
        };
        assert_eq!(state.options, expected);

        assert_eq!(apply(&mut state, "set nocount on"), SetOutcome::Applied);
        assert!(state.options.nocount);
        assert_eq!(
            apply(&mut state, "SET\tXACT_ABORT ,ARITHABORT\n ON"),
            SetOutcome::Applied
        );
        assert!(state.options.xact_abort);
        assert!(state.options.arithabort);
    }

    #[test]
    fn every_boolean_option_is_recognised() {
        let names = [
            "ANSI_NULLS",
            "ANSI_PADDING",
            "ANSI_WARNINGS",
            "ANSI_NULL_DFLT_ON",
            "ARITHABORT",
            "ARITHIGNORE",
            "CONCAT_NULL_YIELDS_NULL",
            "QUOTED_IDENTIFIER",
            "NOCOUNT",
            "XACT_ABORT",
            "IMPLICIT_TRANSACTIONS",
            "CURSOR_CLOSE_ON_COMMIT",
            "NUMERIC_ROUNDABORT",
        ];
        let mut state = state();
        for name in names {
            assert_eq!(
                apply(&mut state, &format!("SET {name} ON")),
                SetOutcome::Applied
            );
        }
        assert!(state.options.arithabort && state.options.numeric_roundabort);
        for name in names {
            assert_eq!(
                apply(&mut state, &format!("SET {name} OFF")),
                SetOutcome::Applied
            );
        }
        assert!(!state.options.ansi_nulls && !state.options.quoted_identifier);
    }

    #[test]
    fn isolation_level() {
        let mut state = state();
        assert_eq!(
            apply(
                &mut state,
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"
            ),
            SetOutcome::Applied
        );
        assert_eq!(state.isolation, IsolationLevel::RepeatableRead);
        assert_eq!(
            apply(
                &mut state,
                "set   transaction  isolation\tlevel   read   committed"
            ),
            SetOutcome::Applied
        );
        assert_eq!(state.isolation, IsolationLevel::ReadCommitted);
        for (stmt, level) in [
            (
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
                IsolationLevel::ReadUncommitted,
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
                IsolationLevel::Snapshot,
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
                IsolationLevel::Serializable,
            ),
        ] {
            assert_eq!(apply(&mut state, stmt), SetOutcome::Applied);
            assert_eq!(state.isolation, level);
        }
        assert_eq!(state.isolation, IsolationLevel::Serializable);
    }

    #[test]
    fn valued_options() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET TEXTSIZE 2147483647"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.textsize, 2147483647);
        assert_eq!(apply(&mut state, "SET TEXTSIZE 0"), SetOutcome::Applied);
        assert_eq!(state.options.textsize, 4096);
        assert_eq!(
            apply(&mut state, "SET LOCK_TIMEOUT 5000"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.lock_timeout, 5000);
        assert_eq!(
            apply(&mut state, "SET LOCK_TIMEOUT -1"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.lock_timeout, -1);
        assert_eq!(apply(&mut state, "SET DATEFORMAT dmy"), SetOutcome::Applied);
        assert_eq!(state.options.dateformat, DateFormat::Dmy);
        assert_eq!(
            apply(&mut state, "SET DATEFORMAT 'YMD'"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.dateformat, DateFormat::Ymd);
        assert_eq!(apply(&mut state, "SET DATEFIRST 1"), SetOutcome::Applied);
        assert_eq!(state.options.datefirst, 1);
        assert_eq!(
            apply(&mut state, "SET LANGUAGE N'us_english'"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.language, "us_english");
        assert_eq!(
            apply(&mut state, "SET LANGUAGE British"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.language, "British");
        assert_eq!(
            apply(&mut state, "SET LANGUAGE 'Deutsch'"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.language, "Deutsch");
        assert_eq!(
            apply(&mut state, "SET DEADLOCK_PRIORITY LOW"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.deadlock_priority, -5);
        assert_eq!(
            apply(&mut state, "SET DEADLOCK_PRIORITY high"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.deadlock_priority, 5);
        assert_eq!(
            apply(&mut state, "SET DEADLOCK_PRIORITY 3"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.deadlock_priority, 3);
        assert_eq!(
            apply(&mut state, "SET DEADLOCK_PRIORITY NORMAL"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.deadlock_priority, 0);
    }

    #[test]
    fn ansi_defaults() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET ANSI_DEFAULTS ON"),
            SetOutcome::Applied
        );
        let o = &state.options;
        assert!(
            o.ansi_nulls
                && o.ansi_null_dflt_on
                && o.ansi_padding
                && o.ansi_warnings
                && o.cursor_close_on_commit
                && o.implicit_transactions
                && o.quoted_identifier
        );
        assert_eq!(
            apply(&mut state, "SET ANSI_DEFAULTS OFF"),
            SetOutcome::Applied
        );
        let o = &state.options;
        assert!(
            !o.ansi_nulls
                && !o.ansi_null_dflt_on
                && !o.ansi_padding
                && !o.ansi_warnings
                && !o.cursor_close_on_commit
                && !o.implicit_transactions
                && !o.quoted_identifier
        );
        // Untouched by ANSI_DEFAULTS.
        assert!(o.concat_null_yields_null && !o.arithabort);
    }

    #[test]
    fn unknown_or_malformed_statements_are_ignored_without_change() {
        let mut state = state();
        let before = state.clone();
        for stmt in [
            "SET NOEXEC ON",
            "SET FOO ON",
            "SET ANSI_NULLS MAYBE",
            "SET ANSI_NULLS, FOO ON",
            "SET ROWCOUNT 10",
            "SET SHOWPLAN_XML ON",
            "SET STATISTICS IO ON",
            "SET TEXTSIZE abc",
            "SET TEXTSIZE 99999999999",
            "SET TEXTSIZE @size",
            "SET LOCK_TIMEOUT 1.5",
            "SET DATEFIRST 8",
            "SET DATEFIRST -1",
            "SET DATEFORMAT xyz",
            "SET DATEFORMAT 5",
            "SET DEADLOCK_PRIORITY 11",
            "SET DEADLOCK_PRIORITY URGENT",
            "SET LANGUAGE 5",
            "SET ANSI_DEFAULTS 1",
            "SET NOCOUNT 1",
        ] {
            assert_eq!(apply(&mut state, stmt), SetOutcome::Ignored, "{stmt}");
        }
        assert_eq!(state, before);
    }

    #[test]
    fn negative_and_signed_numbers_are_read() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET DEADLOCK_PRIORITY -10"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.deadlock_priority, -10);
        assert_eq!(
            apply(&mut state, "SET LOCK_TIMEOUT +250"),
            SetOutcome::Applied
        );
        assert_eq!(state.options.lock_timeout, 250);
        assert_eq!(
            apply(&mut state, "SET DEADLOCK_PRIORITY -11"),
            SetOutcome::Ignored
        );
        assert_eq!(state.options.deadlock_priority, -10);
    }

    #[test]
    fn identity_insert_opens_one_table_and_refuses_a_second() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT dbo.t1 ON"),
            SetOutcome::Applied
        );
        assert_eq!(
            state
                .identity_insert
                .as_ref()
                .map(|t| { (t.database.as_str(), t.schema.as_str(), t.name.as_str(),) }),
            Some(("master", "dbo", "t1"))
        );
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT dbo.t1 ON"),
            SetOutcome::Applied
        );
        let failed = apply(&mut state, "SET IDENTITY_INSERT dbo.t2 ON");
        match failed {
            SetOutcome::Failed(err) => {
                assert_eq!(
                    err,
                    SqlError::identity_insert_already_on("master", "dbo", "t1", "dbo.t2")
                );
            }
            other => panic!("expected 8107, got {other:?}"),
        }
        assert_eq!(
            state.identity_insert.as_ref().map(|t| t.name.as_str()),
            Some("t1")
        );
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT dbo.t2 OFF"),
            SetOutcome::Applied
        );
        assert_eq!(
            state.identity_insert.as_ref().map(|t| t.name.as_str()),
            Some("t1")
        );
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT t1 OFF"),
            SetOutcome::Applied
        );
        assert_eq!(state.identity_insert, None);
    }

    /// The three parts the state keeps for the table of `text`, and the name 8107 writes.
    fn parts(text: &str) -> Option<(String, String, String, String)> {
        let set = parsed(text);
        let table = set.table.expect("an IDENTITY_INSERT names a table");
        identity_target(&table, "master").map(|target| {
            (
                target.table.database,
                target.table.schema,
                target.table.name,
                target.written,
            )
        })
    }

    #[test]
    fn a_dot_or_a_bracket_inside_a_delimited_part_does_not_split_it() {
        // One part holding a dot, not two parts.
        assert_eq!(
            parts("SET IDENTITY_INSERT [dbo.x] ON"),
            Some((
                "master".to_owned(),
                "dbo".to_owned(),
                "dbo.x".to_owned(),
                "dbo.x".to_owned()
            ))
        );
        assert_eq!(
            parts("SET IDENTITY_INSERT [dbo].[dbo.x] ON").map(|p| p.2),
            Some("dbo.x".to_owned())
        );
        // A doubled closing delimiter stands for itself: `a]b`, not `a]]b`.
        assert_eq!(
            parts("SET IDENTITY_INSERT [a]]b] ON").map(|p| p.2),
            Some("a]b".to_owned())
        );
        assert_eq!(
            parts("SET IDENTITY_INSERT \"a\"\"b\" ON").map(|p| p.2),
            Some("a\"b".to_owned())
        );
        // Delimiters removed, part count kept.
        assert_eq!(
            parts("SET IDENTITY_INSERT [dbo].[ra] ON").map(|p| p.3),
            Some("dbo.ra".to_owned())
        );
        assert_eq!(
            parts("SET IDENTITY_INSERT [ra] ON").map(|p| p.3),
            Some("ra".to_owned())
        );
        assert_eq!(
            parts("SET IDENTITY_INSERT [db].[dbo].[ra] ON").map(|p| p.3),
            Some("db.dbo.ra".to_owned())
        );
        assert_eq!(
            parts("SET IDENTITY_INSERT dbo.ra ON"),
            Some((
                "master".to_owned(),
                "dbo".to_owned(),
                "ra".to_owned(),
                "dbo.ra".to_owned()
            ))
        );
        assert_eq!(
            parts("SET IDENTITY_INSERT db.dbo.ra ON").map(|p| p.0),
            Some("db".to_owned())
        );
        // A linked server part names no table of this server.
        assert_eq!(parts("SET IDENTITY_INSERT srv.db.dbo.ra ON"), None);
    }

    #[test]
    fn identity_insert_on_a_delimited_name_keeps_the_canonical_form() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT [dbo].[ra] ON"),
            SetOutcome::Applied
        );
        assert_eq!(
            state.identity_insert.as_ref().map(|t| (
                t.database.as_str(),
                t.schema.as_str(),
                t.name.as_str()
            )),
            Some(("master", "dbo", "ra"))
        );
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT dbo.ra OFF"),
            SetOutcome::Applied
        );
        assert_eq!(state.identity_insert, None);
    }

    #[test]
    fn identity_insert_on_a_delimited_name_with_a_space_is_read_whole() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT [my table] ON"),
            SetOutcome::Applied
        );
        assert_eq!(
            state.identity_insert.as_ref().map(|t| t.name.as_str()),
            Some("my table")
        );
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT dbo.[my table] OFF"),
            SetOutcome::Applied
        );
        assert_eq!(state.identity_insert, None);
    }

    #[test]
    fn sqlclient_opening_batch_is_fully_applied() {
        let batch = "SET DATEFORMAT mdy; SET ANSI_NULLS ON; SET ANSI_PADDING ON; \
                     SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; \
                     SET QUOTED_IDENTIFIER ON; SET NUMERIC_ROUNDABORT OFF; \
                     SET TEXTSIZE 2147483647; SET TRANSACTION ISOLATION LEVEL READ COMMITTED";
        let mut state = state();
        let statements = vauban_parser::parse_batch(batch, &ParseOptions::default())
            .expect("the opening batch parses")
            .statements;
        assert_eq!(statements.len(), 10);
        for stmt in &statements {
            let vauban_parser::Statement::SetOption(set) = stmt else {
                panic!("{stmt:?} is a SET of an option");
            };
            assert_eq!(apply_set_option(&mut state, set, None), SetOutcome::Applied);
        }
        assert!(state.options.arithabort);
        assert_eq!(state.options.textsize, 2147483647);
        assert_eq!(state.isolation, IsolationLevel::ReadCommitted);
    }

    #[test]
    fn comments_inside_a_statement_are_skipped() {
        let mut state = state();
        assert_eq!(
            apply(&mut state, "/* x */ SET -- comment\n NOCOUNT /* y */ ON"),
            SetOutcome::Applied
        );
        assert!(state.options.nocount);
    }
}
