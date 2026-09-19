//! `SetOptions`, `DateFormat`, `IsolationLevel` and the minimal recognition of the `SET`
//! statements drivers send when a connection opens.
//!
//! `split_statements` cuts a batch into statements and `apply_set_statement` recognises a
//! `SET` statement word by word and updates the session state, without any other effect
//! and without failing: an unknown or malformed `SET` is `Ignored` and logged at `debug`.
//! Both predate the T-SQL parser, which `batch.rs` uses instead.

use std::sync::Arc;
use tracing::debug;
use vauban_binder::SessionOptions as BinderOptions;
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{SqlError, SqlResult};

use vauban_executor::ExecSession;
use vauban_parser::ParseOptions;
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
        | PhysicalStatement::Return(_)
        | PhysicalStatement::Print(_)
        | PhysicalStatement::Transaction(_)
        | PhysicalStatement::Execute(_) => false,
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

/// Result of `apply_set_statement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    /// The statement was recognised and the state updated.
    Applied,
    /// The statement is not a recognised `SET`; the state is unchanged. The payload is the
    /// statement as received, for the log.
    Ignored(String),
    /// The statement was recognised and refused; the state is unchanged.
    Failed(SqlError),
}

/// Keywords that start a statement: a line beginning with one of them starts a new
/// statement even without `;` (drivers separate their `SET`s with `;` or with new lines).
const STATEMENT_KEYWORDS: [&str; 4] = ["SET", "SELECT", "USE", "WAITFOR"];

/// Cuts a batch into statements, on `;` and on the lines that begin with a statement
/// keyword (case-insensitive), ignoring `--` and `/* */` comments and `'...'` strings.
/// Each statement is trimmed; empty ones and comment-only ones are dropped.
///
/// Enough for the batches of the drivers; the parser replaces it on the query path.
// Dead on the query path: `run_batch` does not cut a batch itself, and the callers left
// are the unit tests of this file, which rustc does not count. The function goes away
// with `apply_set_statement`.
#[allow(dead_code)]
pub(crate) fn split_statements(batch: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    // Byte offset where the current statement starts and whether it has a token yet.
    let mut start = 0;
    let mut has_token = false;
    let mut push = |from: usize, to: usize, has_token: bool| {
        let text = batch[from..to].trim();
        if has_token && !text.is_empty() {
            statements.push(text);
        }
    };

    for spanned in tokenize(batch) {
        match spanned.token {
            Token::Semicolon => {
                push(start, spanned.start, has_token);
                start = spanned.end;
                has_token = false;
            }
            Token::Word(word) if spanned.line_start && has_token && is_keyword(word) => {
                push(start, spanned.start, has_token);
                start = spanned.start;
                has_token = true;
            }
            _ => has_token = true,
        }
    }
    push(start, batch.len(), has_token);
    statements
}

fn is_keyword(word: &str) -> bool {
    STATEMENT_KEYWORDS
        .iter()
        .any(|keyword| keyword.eq_ignore_ascii_case(word))
}

/// Recognises a `SET` statement (case-insensitive, any spacing) and updates `state`.
///
/// Recognised: `SET <opt> [, <opt>...] ON|OFF` for the boolean options of `SetOptions`,
/// `SET ANSI_DEFAULTS ON|OFF`, `SET TEXTSIZE n`, `SET LOCK_TIMEOUT n`, `SET DATEFORMAT f`,
/// `SET DATEFIRST n`, `SET LANGUAGE name`, `SET DEADLOCK_PRIORITY LOW|NORMAL|HIGH|n`,
/// `SET TRANSACTION ISOLATION LEVEL ...` and `SET IDENTITY_INSERT <table> ON|OFF`.
/// Anything else (`SET NOEXEC`, `SET ROWCOUNT`, `SET SHOWPLAN_*`, unknown option,
/// malformed value) is `Ignored` with a `debug` log and leaves the state untouched: no
/// error here. A second `IDENTITY_INSERT ON` while another table is open is `Failed`
/// with 8107 (`tests/set_options_effects.rs`).
pub(crate) fn apply_set_statement(
    state: &mut SessionState,
    stmt: &str,
    catalog: Option<&CatalogSnapshot>,
) -> SetOutcome {
    let tokens: Vec<Token<'_>> = tokenize(stmt).into_iter().map(|s| s.token).collect();
    match apply_tokens(state, &tokens, catalog) {
        Ok(Some(())) => SetOutcome::Applied,
        Ok(None) => {
            debug!(statement = stmt, "SET statement ignored");
            SetOutcome::Ignored(stmt.to_owned())
        }
        Err(err) => SetOutcome::Failed(err),
    }
}

/// `Ok(Some(()))` once the state is updated; `Ok(None)` leaves it untouched; `Err` is a
/// recognised `SET` that was refused (8107).
fn apply_tokens(
    state: &mut SessionState,
    tokens: &[Token<'_>],
    catalog: Option<&CatalogSnapshot>,
) -> Result<Option<()>, SqlError> {
    let (first, rest) = match tokens.split_first() {
        Some(pair) => pair,
        None => return Ok(None),
    };
    if !first.is_word("SET") {
        return Ok(None);
    }
    let (option, args) = match rest.split_first() {
        Some(pair) => pair,
        None => return Ok(None),
    };
    let Token::Word(option) = option else {
        return Ok(None);
    };
    if option.eq_ignore_ascii_case("IDENTITY_INSERT") {
        return apply_identity_insert(state, args, catalog);
    }
    Ok(apply_known_option(state, option, args))
}

/// Default schema of a login, `dbo` until the catalogue knows better.
const DEFAULT_SCHEMA: &str = "dbo";

/// `SET IDENTITY_INSERT <table> {ON|OFF}`. A second `ON` while another table is open
/// answers 8107 and leaves the first table open.
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

fn apply_identity_insert(
    state: &mut SessionState,
    args: &[Token<'_>],
    catalog: Option<&CatalogSnapshot>,
) -> Result<Option<()>, SqlError> {
    let [Token::Word(table), Token::Word(on_off)] = args else {
        return Ok(None);
    };
    let on = match on_off {
        word if word.eq_ignore_ascii_case("ON") => true,
        word if word.eq_ignore_ascii_case("OFF") => false,
        _ => return Ok(None),
    };
    let target = match parse_identity_table(table, &state.database, state.options.quoted_identifier)
    {
        Some(target) => target,
        None => return Ok(None),
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

/// One, two or three dotted parts: `t`, `dbo.t`, `db.dbo.t`. Missing schema is `dbo`;
/// missing database is `current_db`.
///
/// What the state keeps is the name without its delimiters, so that a later `OFF` written
/// another way closes the option that `[dbo].[t] ON` opened
/// (`tests/set_options_effects.rs`, `identity_insert_opened_on_a_delimited_name_is_closed_by_a_canonical_off`).
/// `"..."` is a name while `QUOTED_IDENTIFIER` is on; off, this answers `None` and
/// nothing is opened (`a_dot_or_a_bracket_inside_a_delimited_part_does_not_split_it`).
fn parse_identity_table(
    written: &str,
    current_db: &str,
    quoted_identifier: bool,
) -> Option<IdentityTarget> {
    let parts = split_identifier(written, quoted_identifier)?;
    let (database, schema, name) = match parts.as_slice() {
        [name] => (
            current_db.to_owned(),
            DEFAULT_SCHEMA.to_owned(),
            name.clone(),
        ),
        [schema, name] => (current_db.to_owned(), schema.clone(), name.clone()),
        [database, schema, name] => (database.clone(), schema.clone(), name.clone()),
        _ => return None,
    };
    Some(IdentityTarget {
        table: IdentityInsertTable {
            database,
            schema,
            name,
        },
        written: parts.join("."),
    })
}

/// Splits a table name into its parts on the dots that separate them, delimiters honoured
/// before anything is stripped.
///
/// A delimited part keeps what it holds: `[dbo.x]` is one part named `dbo.x`, and `[a]]b]`
/// one part named `a]b`, a doubled closing delimiter standing for itself. Splitting on the
/// dots first would read those two as two parts and as `a]]b`
/// (`a_dot_or_a_bracket_inside_a_delimited_part_does_not_split_it`).
///
/// `None` when the name is not one, two or three non-empty parts, when a delimiter is left
/// open, or when anything follows a closing delimiter other than a dot.
fn split_identifier(written: &str, quoted_identifier: bool) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = written;
    loop {
        let (part, tail) = match rest.chars().next()? {
            '[' => delimited(rest, ']')?,
            '"' if quoted_identifier => delimited(rest, '"')?,
            _ => {
                let end = rest.find(['.', '[', ']', '"']).unwrap_or(rest.len());
                (rest[..end].to_owned(), &rest[end..])
            }
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        match tail.strip_prefix('.') {
            Some(next) => rest = next,
            None if tail.is_empty() => break,
            None => return None,
        }
    }
    (1..=3).contains(&parts.len()).then_some(parts)
}

/// What a `[...]` or `"..."` part holds, and the text that follows it. The closing
/// delimiter written twice stands for itself. `None` for a part left unclosed
/// (`a_dot_or_a_bracket_inside_a_delimited_part_does_not_split_it`).
fn delimited(src: &str, close: char) -> Option<(String, &str)> {
    let mut value = String::new();
    // Past the opening delimiter, which is one byte (`[` or `"`).
    let mut rest = &src[1..];
    loop {
        let end = rest.find(close)?;
        value.push_str(&rest[..end]);
        let after = &rest[end + close.len_utf8()..];
        match after.strip_prefix(close) {
            Some(tail) => {
                value.push(close);
                rest = tail;
            }
            None => return Some((value, after)),
        }
    }
}

/// `Some(())` once the state is updated; `None` leaves it untouched (nothing is applied
/// before the whole statement is validated).
fn apply_known_option(state: &mut SessionState, option: &str, args: &[Token<'_>]) -> Option<()> {
    let options = &mut state.options;
    match option.to_ascii_uppercase().as_str() {
        "TRANSACTION" => state.isolation = parse_isolation(args)?,
        "TEXTSIZE" => {
            let size: i32 = single_word(args)?.parse().ok()?;
            // 0 resets the default of 4096 bytes.
            options.textsize = if size == 0 { 4096 } else { size };
        }
        "LOCK_TIMEOUT" => options.lock_timeout = single_word(args)?.parse().ok()?,
        "DATEFORMAT" => options.dateformat = DateFormat::parse(&single_value(args)?)?,
        "DATEFIRST" => {
            let day: u8 = single_word(args)?.parse().ok()?;
            if !(1..=7).contains(&day) {
                return None;
            }
            options.datefirst = day;
        }
        "LANGUAGE" => options.language = single_value(args)?,
        "DEADLOCK_PRIORITY" => options.deadlock_priority = parse_deadlock_priority(args)?,
        "ANSI_DEFAULTS" => {
            let on = parse_on_off(args)?;
            // The seven options `ANSI_DEFAULTS` controls.
            options.ansi_nulls = on;
            options.ansi_null_dflt_on = on;
            options.ansi_padding = on;
            options.ansi_warnings = on;
            options.cursor_close_on_commit = on;
            options.implicit_transactions = on;
            options.quoted_identifier = on;
        }
        _ => {
            let (names, on) = parse_option_list(option, args)?;
            // Validate every name before applying any: partial application would leave
            // an inconsistent state on `Ignored`.
            if names
                .iter()
                .any(|name| bool_option(options, name).is_none())
            {
                return None;
            }
            for name in names {
                if let Some(field) = bool_option(options, name) {
                    *field = on;
                }
            }
        }
    }
    Some(())
}

/// `<opt> [, <opt>...] ON|OFF`: the option names and the value.
fn parse_option_list<'a>(first: &'a str, args: &[Token<'a>]) -> Option<(Vec<&'a str>, bool)> {
    let mut names = vec![first];
    let mut expect_name = false;
    let mut args = args.iter();
    loop {
        match args.next()? {
            Token::Comma if !expect_name => expect_name = true,
            Token::Word(word) if expect_name => {
                names.push(word);
                expect_name = false;
            }
            Token::Word(word) if !expect_name => {
                let on = on_off(word)?;
                return args.next().is_none().then_some((names, on));
            }
            _ => return None,
        }
    }
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

/// `ISOLATION LEVEL READ UNCOMMITTED|READ COMMITTED|REPEATABLE READ|SNAPSHOT|SERIALIZABLE`.
fn parse_isolation(args: &[Token<'_>]) -> Option<IsolationLevel> {
    let words: Vec<String> = args
        .iter()
        .map(|token| match token {
            Token::Word(word) => Some(word.to_ascii_uppercase()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let level = match words.as_slice() {
        ["ISOLATION", "LEVEL", "READ", "UNCOMMITTED"] => IsolationLevel::ReadUncommitted,
        ["ISOLATION", "LEVEL", "READ", "COMMITTED"] => IsolationLevel::ReadCommitted,
        ["ISOLATION", "LEVEL", "REPEATABLE", "READ"] => IsolationLevel::RepeatableRead,
        ["ISOLATION", "LEVEL", "SNAPSHOT"] => IsolationLevel::Snapshot,
        ["ISOLATION", "LEVEL", "SERIALIZABLE"] => IsolationLevel::Serializable,
        _ => return None,
    };
    Some(level)
}

/// `LOW|NORMAL|HIGH|n` with `n` in `-10..=10`.
fn parse_deadlock_priority(args: &[Token<'_>]) -> Option<i8> {
    let word = single_word(args)?;
    let priority = match word.to_ascii_uppercase().as_str() {
        "LOW" => -5,
        "NORMAL" => 0,
        "HIGH" => 5,
        number => number.parse::<i8>().ok()?,
    };
    (-10..=10).contains(&priority).then_some(priority)
}

/// `ON|OFF` as the only argument.
fn parse_on_off(args: &[Token<'_>]) -> Option<bool> {
    on_off(single_word(args)?)
}

fn on_off(word: &str) -> Option<bool> {
    if word.eq_ignore_ascii_case("ON") {
        Some(true)
    } else if word.eq_ignore_ascii_case("OFF") {
        Some(false)
    } else {
        None
    }
}

/// The only argument, which must be a word.
fn single_word<'a>(args: &[Token<'a>]) -> Option<&'a str> {
    match args {
        [Token::Word(word)] => Some(word),
        _ => None,
    }
}

/// The only argument, a word or a `'...'`/`N'...'` string, without its quotes.
fn single_value(args: &[Token<'_>]) -> Option<String> {
    match args {
        [Token::Word(word)] => Some((*word).to_owned()),
        [Token::Str(text)] => Some(text.clone()),
        _ => None,
    }
}

/// A lexical token of a batch. Comments and white space are skipped by the tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token<'a> {
    /// A keyword, an identifier or a number: a run of characters up to white space or a
    /// delimiter.
    Word(&'a str),
    /// A `'...'` or `N'...'` literal, quotes removed and `''` unescaped.
    Str(String),
    /// `,`
    Comma,
    /// `;`
    Semicolon,
}

impl Token<'_> {
    fn is_word(&self, expected: &str) -> bool {
        matches!(self, Token::Word(word) if word.eq_ignore_ascii_case(expected))
    }
}

/// A token with its byte span and whether only white space (or comments) precede it on its
/// line.
#[derive(Debug)]
struct Spanned<'a> {
    token: Token<'a>,
    start: usize,
    end: usize,
    line_start: bool,
}

/// Characters that end a word.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ';' | '\'' | '(' | ')')
}

/// Lexes `src` into tokens, skipping white space, `--` comments and (nested) `/* */`
/// comments. Never fails: an unterminated string or comment runs to the end of the text.
fn tokenize(src: &str) -> Vec<Spanned<'_>> {
    let mut tokens = Vec::new();
    let mut line_start = true;
    let mut pos = 0;
    while pos < src.len() {
        let rest = &src[pos..];
        let c = match rest.chars().next() {
            Some(c) => c,
            // `pos` is always on a char boundary: it advances by whole chars or by the
            // length of ASCII markers.
            None => break,
        };
        if c == '\n' {
            line_start = true;
            pos += 1;
        } else if c.is_whitespace() {
            pos += c.len_utf8();
        } else if rest.starts_with("--") {
            pos += rest.find('\n').unwrap_or(rest.len());
        } else if rest.starts_with("/*") {
            pos += block_comment_len(rest);
        } else if c == '\'' || ((c == 'N' || c == 'n') && rest[1..].starts_with('\'')) {
            let quote = if c == '\'' { 0 } else { 1 };
            let (value, len) = string_literal(&rest[quote..]);
            tokens.push(Spanned {
                token: Token::Str(value),
                start: pos,
                end: pos + quote + len,
                line_start,
            });
            line_start = false;
            pos += quote + len;
        } else if c == ',' || c == ';' {
            let token = if c == ',' {
                Token::Comma
            } else {
                Token::Semicolon
            };
            tokens.push(Spanned {
                token,
                start: pos,
                end: pos + 1,
                line_start,
            });
            line_start = false;
            pos += 1;
        } else if c == '(' || c == ')' {
            // Parentheses are not part of any recognised statement: kept as one-character
            // words so that they make the statement `Ignored` rather than vanish.
            tokens.push(Spanned {
                token: Token::Word(&rest[..1]),
                start: pos,
                end: pos + 1,
                line_start,
            });
            line_start = false;
            pos += 1;
        } else {
            let len = rest.find(is_delimiter).unwrap_or(rest.len());
            tokens.push(Spanned {
                token: Token::Word(&rest[..len]),
                start: pos,
                end: pos + len,
                line_start,
            });
            line_start = false;
            pos += len;
        }
    }
    tokens
}

/// Length of the `/* ... */` comment at the start of `src`, nesting included; the whole
/// text when it is unterminated.
fn block_comment_len(src: &str) -> usize {
    let mut depth = 0usize;
    let mut pos = 0;
    while pos < src.len() {
        let rest = &src[pos..];
        if rest.starts_with("/*") {
            depth += 1;
            pos += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            pos += 2;
            if depth == 0 {
                return pos;
            }
        } else {
            pos += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    src.len()
}

/// The value of the `'...'` literal at the start of `src` (`''` unescaped) and its length
/// in bytes, quotes included; an unterminated literal runs to the end of the text.
fn string_literal(src: &str) -> (String, usize) {
    let mut value = String::new();
    let mut chars = src.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if chars.peek().is_some_and(|&(_, next)| next == '\'') {
                value.push('\'');
                chars.next();
            } else {
                return (value, i + 1);
            }
        } else {
            value.push(c);
        }
    }
    (value, src.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SessionState {
        SessionState::new(51)
    }

    fn apply(state: &mut SessionState, stmt: &str) -> SetOutcome {
        apply_set_statement(state, stmt, None)
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
    fn split_on_semicolons_and_keyword_lines() {
        let statements =
            split_statements("SET DATEFORMAT mdy; SET ANSI_NULLS ON\nSET TEXTSIZE 2147483647");
        assert_eq!(
            statements,
            vec![
                "SET DATEFORMAT mdy",
                "SET ANSI_NULLS ON",
                "SET TEXTSIZE 2147483647"
            ]
        );
    }

    #[test]
    fn split_ignores_semicolons_in_strings_and_comments() {
        assert_eq!(
            split_statements("SET LANGUAGE 'a;b'; SET NOCOUNT ON"),
            vec!["SET LANGUAGE 'a;b'", "SET NOCOUNT ON"]
        );
        assert_eq!(
            split_statements("SET NOCOUNT ON -- a; b\nSET XACT_ABORT ON"),
            vec!["SET NOCOUNT ON -- a; b", "SET XACT_ABORT ON"]
        );
        assert_eq!(
            split_statements("SET NOCOUNT /* a; b\n SET */ ON; SET XACT_ABORT ON"),
            vec!["SET NOCOUNT /* a; b\n SET */ ON", "SET XACT_ABORT ON"]
        );
        assert_eq!(
            split_statements("SET LANGUAGE 'it''s; here'"),
            vec!["SET LANGUAGE 'it''s; here'"]
        );
    }

    #[test]
    fn split_keeps_a_continued_statement_and_drops_empty_ones() {
        // A keyword in the middle of a line does not split; neither does an unknown one at
        // the start of a line.
        assert_eq!(
            split_statements("SET TRANSACTION ISOLATION LEVEL\n  READ COMMITTED"),
            vec!["SET TRANSACTION ISOLATION LEVEL\n  READ COMMITTED"]
        );
        assert_eq!(
            split_statements("select 1 set nocount on\nuse master\n\nWAITFOR DELAY '00:00:01'"),
            vec![
                "select 1 set nocount on",
                "use master",
                "WAITFOR DELAY '00:00:01'"
            ]
        );
        assert_eq!(
            split_statements(";; -- nothing\n/* still nothing */ ;"),
            Vec::<&str>::new()
        );
        assert_eq!(split_statements(""), Vec::<&str>::new());
        assert_eq!(
            split_statements("-- leading\nSET NOCOUNT ON;"),
            vec!["-- leading\nSET NOCOUNT ON"]
        );
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
        assert!(matches!(
            apply(&mut state, "SET TRANSACTION ISOLATION LEVEL READ"),
            SetOutcome::Ignored(_)
        ));
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
            "SET",
            "SET ANSI_NULLS",
            "SET ANSI_NULLS MAYBE",
            "SET ANSI_NULLS, FOO ON",
            "SET ANSI_NULLS ON extra",
            "SET ANSI_NULLS, ON",
            "SET ANSI_NULLS ,, NOCOUNT ON",
            "SET ROWCOUNT 10",
            "SET SHOWPLAN_XML ON",
            "SET TEXTSIZE abc",
            "SET TEXTSIZE 99999999999",
            "SET DATEFIRST 8",
            "SET DATEFORMAT xyz",
            "SET DEADLOCK_PRIORITY 11",
            "SET TRANSACTION ISOLATION LEVEL 'READ COMMITTED'",
            "SET @x = 1",
            "SELECT 1",
            "",
            "''",
            "SET 'ANSI_NULLS' ON",
        ] {
            assert_eq!(
                apply(&mut state, stmt),
                SetOutcome::Ignored(stmt.to_owned())
            );
        }
        assert_eq!(state, before);
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

    /// The three parts a name is read as, and the name as written with its delimiters
    /// removed.
    fn parts(written: &str) -> Option<(String, String, String, String)> {
        parse_identity_table(written, "master", true).map(|target| {
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
        // One part holding a dot, not two parts: splitting on the dots before honouring
        // the delimiters would answer `dbo` and `x`.
        assert_eq!(
            parts("[dbo.x]"),
            Some((
                "master".to_owned(),
                "dbo".to_owned(),
                "dbo.x".to_owned(),
                "dbo.x".to_owned()
            ))
        );
        // The same name written in two parts is the same table.
        assert_eq!(
            parts("[dbo].[dbo.x]").map(|p| p.2),
            Some("dbo.x".to_owned())
        );
        // A doubled closing delimiter stands for itself: `a]b`, not `a]]b`.
        assert_eq!(parts("[a]]b]").map(|p| p.2), Some("a]b".to_owned()));
        assert_eq!(parts("\"a\"\"b\"").map(|p| p.2), Some("a\"b".to_owned()));
        // Delimiters removed, part count kept.
        assert_eq!(parts("[dbo].[ra]").map(|p| p.3), Some("dbo.ra".to_owned()));
        assert_eq!(parts("[ra]").map(|p| p.3), Some("ra".to_owned()));
        assert_eq!(
            parts("[db].[dbo].[ra]").map(|p| p.3),
            Some("db.dbo.ra".to_owned())
        );
        // Undelimited names are unchanged.
        assert_eq!(
            parts("dbo.ra"),
            Some((
                "master".to_owned(),
                "dbo".to_owned(),
                "ra".to_owned(),
                "dbo.ra".to_owned()
            ))
        );
        assert_eq!(parts("db.dbo.ra").map(|p| p.0), Some("db".to_owned()));
        // Shapes with no name to read.
        for written in [
            "", ".", "dbo.", ".ra", "a.b.c.d", "[ra", "[ra]]", "[dbo]x", "ra]", "[]",
        ] {
            assert_eq!(parts(written), None, "{written}");
        }
        // `"..."` is a name while QUOTED_IDENTIFIER is on, and not while it is off.
        assert_eq!(
            parse_identity_table("\"dbo\".\"ra\"", "master", false).map(|t| t.written),
            None
        );
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
    fn identity_insert_on_a_name_the_tokenizer_splits_is_ignored() {
        // A delimited part holding white space reaches this module as several words, so
        // the statement is not recognised and the state is untouched: nothing is opened,
        // and a table already open stays open.
        let mut state = state();
        assert!(matches!(
            apply(&mut state, "SET IDENTITY_INSERT [my table] ON"),
            SetOutcome::Ignored(_)
        ));
        assert_eq!(state.identity_insert, None);
        assert_eq!(
            apply(&mut state, "SET IDENTITY_INSERT dbo.ra ON"),
            SetOutcome::Applied
        );
        assert!(matches!(
            apply(&mut state, "SET IDENTITY_INSERT [my table] OFF"),
            SetOutcome::Ignored(_)
        ));
        assert_eq!(
            state.identity_insert.as_ref().map(|t| t.name.as_str()),
            Some("ra")
        );
    }

    #[test]
    fn sqlclient_opening_batch_is_fully_applied() {
        let batch = "SET DATEFORMAT mdy; SET ANSI_NULLS ON; SET ANSI_PADDING ON; \
                     SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; \
                     SET QUOTED_IDENTIFIER ON; SET NUMERIC_ROUNDABORT OFF; \
                     SET TEXTSIZE 2147483647; SET TRANSACTION ISOLATION LEVEL READ COMMITTED";
        let mut state = state();
        let statements = split_statements(batch);
        assert_eq!(statements.len(), 10);
        for stmt in statements {
            assert_eq!(apply(&mut state, stmt), SetOutcome::Applied, "{stmt}");
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

    #[test]
    fn tokenizer_handles_unterminated_input() {
        // Never panics, whatever the text.
        let _ = tokenize("SET LANGUAGE 'unterminated");
        let _ = tokenize("SET /* unterminated");
        let _ = tokenize("N'x'' ");
        let _ = tokenize("é;ü,'é''ü'--é\n/*é*/");
        assert_eq!(
            tokenize("N'a''b'")
                .into_iter()
                .map(|s| s.token)
                .collect::<Vec<_>>(),
            vec![Token::Str("a'b".into())]
        );
    }
}
