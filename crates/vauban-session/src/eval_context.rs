//! `SessionEvalContext`: what a built-in function of `sysfn` may read from the session.
//!
//! `sysfn` knows nothing of this crate (the dependency runs the other way), so everything
//! `@@SPID`, `DB_NAME()`, `@@ROWCOUNT` or `GETDATE()` needs from a connection arrives
//! through `sysfn::EvalContext`. This file is the implementation of that trait in
//! `session`, and `batch.rs` builds one per statement.
//!
//! # The clock is frozen for the whole statement
//!
//! `SELECT GETDATE(), GETDATE()` returns twice the same value on SQL Server: the clock is
//! read once per statement, not once per call (the functions are non-deterministic but
//! constant within a query). [`SessionEvalContext`] therefore reads the instant in
//! [`SessionEvalContext::new`] and hands the **same** `DateTime2` to each call of
//! `now_local` and `now_utc` (unit test `the_clock_is_read_once_and_never_again`).
//!
//! `NEWID()` is the opposite and stays outside this file: it draws a fresh value per call.
//!
//! # The catalogue arrives as a snapshot, the one of the statement
//!
//! `OBJECT_ID`, `OBJECT_NAME`, `DB_ID` and `DB_NAME` read metadata, and this file reaches
//! metadata through a [`CatalogSnapshot`] rather than through `vauban_catalog::Catalog`:
//! `batch.rs` hands over the snapshot its statement already reads through, so two calls of
//! one statement answer one catalogue. A context built without one (the tests of the clock
//! below) answers `None` to its four catalogue methods, that is `NULL` to `OBJECT_ID`,
//! `OBJECT_NAME`, `DB_ID(name)` and `DB_NAME(id)` (unit test
//! `without_a_catalogue_nothing_resolves`).
//!
//! ## The name of `OBJECT_ID` is given as a string, from one to four parts
//!
//! The parts are therefore split here and not by the parser: [`WrittenName::parse`] removes
//! the `[...]` and `"..."` delimiters and keeps the text as written otherwise, then
//! [`CatalogSnapshot::resolve_object`] compares under the collation of the database, which
//! folds the case. On a table `dbo.t` of the current database, the right column being the
//! unit test that holds the form:
//!
//! | written | resolves | test |
//! |---|:-:|---|
//! | `dbo.t`, `[dbo].[t]`, `"dbo"."t"` | yes | `object_id_of_created_table_is_some` |
//! | `t` (one part, default schema) | yes | same test |
//! | `DBO.T` (case folded) | yes | same test |
//! | `<current database>.dbo.t` | yes | same test |
//! | `.t` (empty schema part), `.dbo.t` (empty database part) | yes | same test |
//! | `..dbo.t`, `[].[].dbo.t` (empty server part) | yes | same test |
//! | `dbo.t ` (blank after an undelimited part), `[dbo].[t ]` | yes | same test |
//! | `srv.<current database>.dbo.t` (server part written) | no | `object_id_unknown_is_none` |
//! | `a.b.<current database>.dbo.t` (five parts) | no | same test |
//! | ` dbo.t `, `dbo . t` (blank inside an undelimited part) | no | same test |
//! | `[dbo].[t] `, `[dbo].[t]x`, `[dbo] .t` | no | same test |
//! | `dbo.` and the empty string (empty object part) | no | same test |
//! | `[dbo].[t` (unterminated delimiter) | no | same test |
//! | `sys.t` (another schema) | no | same test |
//!
//! So an empty part is read as "not written" wherever it sits before the object (server,
//! database and schema, rows 5 and 6), while a blank **inside** an undelimited part belongs
//! to the name it is written in (row 7 resolves, row 11 does not, the difference being where
//! the blank falls), and a character written outside the delimiters of a part refuses the
//! name (row 12, and the table of [`split_parts`]). A **written** fourth part is a linked
//! server, which no [`CatalogSnapshot`] serves and which `binder/catalog_view.rs` refuses in
//! the same way; row 9 is the spelling the tests hold.
//!
//! ## `DB_NAME(id)`, `SCHEMA_NAME` and `SCHEMA_ID`
//!
//! `DB_NAME(id)` answers the name of the database of that identifier among the databases
//! the snapshot holds ([`CatalogSnapshot::database_by_id`]), the current one or another:
//! `DB_NAME(2)` is `tempdb` from `master` (`database_name_answers_any_database_of_the_snapshot`).
//! `0` names the current database, as `DB_NAME()` does; an identifier no database carries,
//! negative or too large, answers `None`.
//!
//! [`EvalContext::schema_name`] reads [`CatalogSnapshot::schema_name`]: the three schemas
//! the bootstrap writes (`dbo` 1, `INFORMATION_SCHEMA` 3, `sys` 4; unit test
//! `schema_name_from_identifier`). `SCHEMA_NAME()` without an argument is `dbo` inside
//! `sysfn` and does not come through this file.
//!
//! [`EvalContext::schema_id`] answers the default schema, `dbo` = 1, and `None` for another
//! name, where SQL Server answers 2, 3 and 4 for `guest`, `INFORMATION_SCHEMA` and `sys`
//! (`schema_id_of_another_schema_is_not_resolved_yet`). Reading those three by name needs the
//! same lookup [`CatalogSnapshot::schema_name`] already provides in the other direction, a
//! reader API the catalogue does not publish yet.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_catalog::{Catalog, CatalogSnapshot, ObjectId};
use vauban_errors::SqlResult;
use vauban_executor::ExecSession;
use vauban_sysfn::EvalContext;
use vauban_txn::{LockWait, TransactionManager, TxnHandle};
use vauban_types::calendar::days_from_civil;
use vauban_types::{Collation, Date, DateTime2, Decimal, Time, Value};

use crate::set_options::lock_timeout_from_ms;
use crate::state::SessionState;

/// Copies what the executor wrote in `exec` into `state` after a statement that succeeded.
///
/// `@@ERROR` goes back to `0` after most successful statements — `SELECT`, `SET`, `PRINT`,
/// `USE`, `BEGIN TRAN` and `DECLARE @x int = 1` after `SELECT 1/0`, but not a bare
/// `DECLARE @x int` (`tests/session_variables.rs`,
/// `last_error_is_cleared_by_a_successful_statement`). The identity functions follow
/// `ExecSession` when an `INSERT` ran; `@@ROWCOUNT` is posted by `batch.rs`.
pub(crate) fn apply_exec_session(
    state: &mut SessionState,
    exec: &ExecSession,
    succeeded: bool,
    clears_last_error: bool,
) {
    if !succeeded {
        return;
    }
    if clears_last_error {
        state.last_error = 0;
    }
    if exec.identity_updated {
        state.last_identity = exec.last_identity;
    }
}

/// 100-nanosecond ticks in one second, the unit of `Time` and of `datetime2(7)`.
const TICKS_PER_SECOND: u64 = 10_000_000;
/// Seconds in one day.
const SECONDS_PER_DAY: u64 = 86_400;

/// Schema an unqualified name of `OBJECT_ID` is resolved in, the `dbo` of a login.
///
/// `batch.rs` holds the same constant for the binder and a session carries no schema of its
/// own yet, so the two are one value: `OBJECT_ID('t')` looks where `SELECT ... FROM t` looks
/// (unit test `object_id_of_created_table_is_some`, the one-part form). A default schema
/// borne by the session is not implemented.
const DEFAULT_SCHEMA: &str = "dbo";

/// The identifier of [`DEFAULT_SCHEMA`], answered by `SCHEMA_ID()` and `SCHEMA_ID('dbo')`:
/// `1` for each, the number the bootstrap of `catalog` writes for `dbo` in `sys.schemas`.
/// Written here rather than read there: [`CatalogSnapshot`] publishes no schema entry
/// (module documentation, section "`DB_NAME(id)`, `SCHEMA_NAME` and `SCHEMA_ID`").
const DEFAULT_SCHEMA_ID: i32 = 1;

/// Most parts a name of `OBJECT_ID` may carry: server, database, schema, object.
const MAX_NAME_PARTS: usize = 4;

/// The session, seen by a built-in function during one statement.
///
/// Borrowed and short-lived: `batch.rs` builds one just before `executor::execute` and
/// drops it just after, so `@@ROWCOUNT` is the count of the **previous** statement for the
/// whole of this one.
pub(crate) struct SessionEvalContext<'a> {
    /// The state of the connection; read-only through this type.
    state: &'a SessionState,
    /// The instant this statement started, answered by `now_local` and `now_utc` alike.
    now: DateTime2,
    /// Where the metadata functions read their catalogue, `None` for a context built without
    /// one (module documentation): they then answer `NULL`.
    catalog: Option<CatalogSource<'a>>,
    /// The lock manager `OBJECT_ID` waits through for a table another transaction is
    /// changing, with the cancellation of the request; `None` for a context that does not
    /// wait ([`SessionEvalContext::with_schema_waits`]).
    schema_waits: Option<(&'a TransactionManager, LockWait)>,
}

/// The state of the 1222 `OBJECT_ID` raises when its wait runs out, where a table read
/// raises 56.
const LOCK_TIMEOUT_STATE_OF_A_METADATA_FUNCTION: u8 = 51;

/// The catalogue of a statement, ready or still to be read.
///
/// A snapshot copies the rows it shows in, so building one per statement costs even for a
/// statement that names no object, and a batch of thousands of `SELECT` without a
/// metadata function pays for thousands of snapshots it does not read. Hence the second
/// variant: the snapshot is read on the first call of `OBJECT_ID`, `OBJECT_NAME`, `DB_ID`
/// or `DB_NAME`, once for the statement, and left unread for a statement that calls no
/// metadata function (unit test
/// `the_deferred_catalogue_is_read_on_the_first_call_and_not_before`).
enum CatalogSource<'a> {
    /// A snapshot the caller already holds, the binding of the batch (`bind_batch`).
    Ready(&'a CatalogSnapshot),
    /// The catalogue and the transaction of the statement, read on the first call.
    Deferred {
        /// The catalogue of the engine.
        catalog: &'a Catalog,
        /// The transaction the statement runs in, which the snapshot is taken through.
        txn: &'a TxnHandle,
        /// The snapshot, once something has asked for it.
        built: OnceCell<CatalogSnapshot>,
    },
}

impl CatalogSource<'_> {
    /// The snapshot, read now if it has not been read yet.
    fn snapshot(&self) -> &CatalogSnapshot {
        match self {
            Self::Ready(snapshot) => snapshot,
            Self::Deferred {
                catalog,
                txn,
                built,
            } => built.get_or_init(|| catalog.snapshot(txn)),
        }
    }
}

impl<'a> SessionEvalContext<'a> {
    /// A context over `state` and the snapshot `catalog`, with the clock read **now** and
    /// frozen.
    pub(crate) fn new(state: &'a SessionState, catalog: Option<&'a CatalogSnapshot>) -> Self {
        Self {
            state,
            now: read_clock(),
            catalog: catalog.map(CatalogSource::Ready),
            schema_waits: None,
        }
    }

    /// A context whose catalogue is read from `catalog` through `txn` on the first call of a
    /// metadata function, and left unread without one ([`CatalogSource`], unit test
    /// `the_deferred_catalogue_is_read_on_the_first_call_and_not_before`).
    pub(crate) fn deferred(
        state: &'a SessionState,
        catalog: &'a Catalog,
        txn: &'a TxnHandle,
    ) -> Self {
        Self {
            state,
            now: read_clock(),
            catalog: Some(CatalogSource::Deferred {
                catalog,
                txn,
                built: OnceCell::new(),
            }),
            schema_waits: None,
        }
    }

    /// The same context, whose `OBJECT_ID` waits through `manager` for a table another
    /// transaction creates, alters or drops without having committed, under the
    /// `SET LOCK_TIMEOUT` of the session, and answers once that transaction has ended
    /// (`tests/ddl_visibility.rs`,
    /// `object_id_waits_for_an_uncommitted_create_or_drop_of_its_table`). A context whose
    /// catalogue is a snapshot already read does not wait.
    pub(crate) fn with_schema_waits(
        mut self,
        manager: &'a TransactionManager,
        wait: LockWait,
    ) -> Self {
        self.schema_waits = Some((manager, wait));
        self
    }

    /// The catalogue of the statement, `None` for a context built without one.
    fn catalog(&self) -> Option<&CatalogSnapshot> {
        self.catalog.as_ref().map(CatalogSource::snapshot)
    }

    /// A context over `state` with a clock given by the caller and no catalogue, for the
    /// tests.
    #[cfg(test)]
    pub(crate) fn with_clock(state: &'a SessionState, now: DateTime2) -> Self {
        Self {
            state,
            now,
            catalog: None,
            schema_waits: None,
        }
    }
}

/// The one to four parts of the name `OBJECT_ID` was given, delimiters removed.
///
/// Built by [`WrittenName::parse`]. The parts keep the case and the spaces they were
/// written with: folding the case is the collation's business, inside
/// [`CatalogSnapshot::resolve_object`], and a space is part of a name (module
/// documentation, the table of forms).
#[derive(Debug, PartialEq, Eq)]
struct WrittenName {
    /// Linked server of a four-part name; `None` below four parts.
    server: Option<String>,
    /// Database part, `None` when it was not written **or** written empty.
    database: Option<String>,
    /// Schema part, `None` under the same rule.
    schema: Option<String>,
    /// Object part, which a one-part name carries by itself — possibly empty, a form that
    /// resolves to nothing (`object_id_unknown_is_none`).
    object: String,
}

impl WrittenName {
    /// The parts of `text`, or `None` when `text` is no name of one to four parts.
    ///
    /// `None` for more than [`MAX_NAME_PARTS`] parts and for a delimiter left open, which
    /// SQL Server answers `NULL` for as well (module documentation). An empty part is read
    /// as "not written" for the server, the database and the schema alike, so that `.t`,
    /// `..t`, `..dbo.t` and `[].[].dbo.t` name the object `t` of the default schema of the
    /// current database, as on SQL Server (`object_id_of_created_table_is_some`).
    fn parse(text: &str) -> Option<Self> {
        let mut parts = split_parts(text)?;
        let object = parts.pop()?;
        let schema = parts.pop().filter(|part| !part.is_empty());
        let database = parts.pop().filter(|part| !part.is_empty());
        let server = parts.pop().filter(|part| !part.is_empty());
        Some(Self {
            server,
            database,
            schema,
            object,
        })
    }
}

/// Splits `text` on the dots that sit outside a `[…]` or `"…"` delimiter.
///
/// A delimiter is removed and the text inside it is taken literally, a doubled closing
/// delimiter standing for itself: `[a.b]` is one part named `a.b`, and `[a]]b]` one part
/// named `a]b`. `None` when the text carries more than [`MAX_NAME_PARTS`] parts, a
/// delimiter left open, or a delimited part that is not alone in its part
/// (`a_written_name_is_split_on_its_undelimited_dots`).
///
/// # A delimited part fills its part, or the name resolves to nothing
///
/// A character written outside the delimiters of a part makes the name resolve to
/// nothing, as on SQL Server; on a table `dbo.v`:
///
/// | written | resolves |
/// |---|:-:|
/// | `[dbo].[v] ` (space after the closing delimiter) | no |
/// | `[dbo].[v]` + a tab | no |
/// | `[dbo].[v]x` | no |
/// | `[dbo] .[v]`, `[dbo] .v` (space before the dot) | no |
/// | ` [dbo].[v]` (space before the opening delimiter) | no |
/// | `[dbo].[v ]` (the space **inside** the delimiter) | yes |
/// | `dbo.v ` (space after an undelimited part) | yes |
///
/// The last two rows are what separates "the space is refused" from "the delimiters are
/// exclusive": a blank kept inside a name is trimmed by the collation of the database
/// ([`CatalogSnapshot::resolve_object`]), an undelimited part with the same blank resolves
/// too, and it takes a character written **outside** a delimiter of the same part to refuse
/// the whole name. Held by `object_id_of_created_table_is_some` (the two rows that
/// resolve) and `object_id_unknown_is_none` (the five that do not).
fn split_parts(text: &str) -> Option<Vec<String>> {
    let mut parts = vec![String::new()];
    // Whether the part being read was written between delimiters, which forbids any other
    // character in it.
    let mut delimited = false;
    let mut rest = text.chars().peekable();
    while let Some(c) = rest.next() {
        match c {
            '.' => {
                if parts.len() == MAX_NAME_PARTS {
                    return None;
                }
                parts.push(String::new());
                delimited = false;
            }
            '[' | '"' => {
                if delimited || !parts.last()?.is_empty() {
                    // A second delimited group, or text before the opening delimiter.
                    return None;
                }
                let closing = if c == '[' { ']' } else { '"' };
                let part = parts.last_mut()?;
                loop {
                    match rest.next() {
                        Some(c) if c == closing => {
                            if rest.peek() == Some(&closing) {
                                rest.next();
                                part.push(closing);
                            } else {
                                break;
                            }
                        }
                        Some(c) => part.push(c),
                        // A delimiter left open: no object bears this name.
                        None => return None,
                    }
                }
                delimited = true;
            }
            // Text after the closing delimiter of this part.
            _ if delimited => return None,
            c => parts.last_mut()?.push(c),
        }
    }
    Some(parts)
}

/// The system clock as a `datetime2(7)`.
///
/// UTC, and the same value for `now_local`: the local time zone of the server cannot be
/// read without a time-zone database, and no dependency of the workspace provides one.
///
/// A clock before 1970 (system time set backwards) yields the epoch rather than a panic:
/// `panic!` is forbidden on the path of a query.
/// The identifier `snapshot` holds for the name `name` written in `OBJECT_ID`, read from
/// the database `current` when the name does not name one; `None` for a name it resolves
/// nothing for (module documentation, table of forms).
fn object_id_in(snapshot: &CatalogSnapshot, current: &str, name: &str) -> Option<i32> {
    let written = WrittenName::parse(name)?;
    if written.server.is_some() {
        // A four-part name names a linked server, which no snapshot serves: the rule
        // of `binder/catalog_view.rs`, and `NULL` on SQL Server.
        return None;
    }
    let database = written.database.as_deref().unwrap_or(current);
    let object = snapshot.resolve_object(
        database,
        written.schema.as_deref(),
        &written.object,
        DEFAULT_SCHEMA,
    )?;
    Some(object.id.0)
}

fn read_clock() -> DateTime2 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed
                .as_secs()
                .saturating_mul(TICKS_PER_SECOND)
                .saturating_add(u64::from(elapsed.subsec_nanos()) / 100)
        });
    let seconds = elapsed / TICKS_PER_SECOND;
    DateTime2 {
        date: Date {
            days: unix_epoch_days().saturating_add((seconds / SECONDS_PER_DAY) as i32),
        },
        time: Time {
            ticks_100ns: (seconds % SECONDS_PER_DAY) * TICKS_PER_SECOND
                + elapsed % TICKS_PER_SECOND,
        },
    }
}

/// Days from 0001-01-01 (the origin of `Date`) to 1970-01-01 (the origin of `SystemTime`).
fn unix_epoch_days() -> i32 {
    days_from_civil(1970, 1, 1)
}

/// The session principal is established by authentication, and SQL Server rejects an
/// empty SQL username before a query could run; the fallback is kept for a state built
/// without a login, without claiming a SQL result for that state.
fn non_empty(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

impl EvalContext for SessionEvalContext<'_> {
    fn now_local(&self) -> DateTime2 {
        self.now
    }

    fn now_utc(&self) -> DateTime2 {
        self.now
    }

    fn rowcount(&self) -> i64 {
        self.state.rowcount
    }

    /// `@@TRANCOUNT`: how many `BEGIN TRANSACTION` are open. `txn_session.rs` keeps the
    /// count on the state; `@@TRANCOUNT` is read through this method and not through
    /// [`EvalContext::variable`].
    fn trancount(&self) -> i32 {
        self.state.trancount
    }

    fn last_identity(&self) -> Option<Decimal> {
        self.state.last_identity
    }

    /// Without trigger nor procedure, the scope is the current batch and answers the same
    /// value as [`EvalContext::last_identity`] on the forms covered by
    /// `tests/session_variables.rs`, `identity_after_an_insert`.
    fn scope_identity(&self) -> Option<Decimal> {
        self.state.last_identity
    }

    fn ident_current(&self, table: &str) -> Option<Decimal> {
        let written = WrittenName::parse(table)?;
        if written.server.is_some() {
            return None;
        }
        let source = self.catalog.as_ref()?;
        let (catalog, txn, snapshot) = match source {
            CatalogSource::Deferred {
                catalog,
                txn,
                built,
            } => {
                let snap = built.get_or_init(|| catalog.snapshot(txn));
                (catalog, txn, snap)
            }
            CatalogSource::Ready(_) => return None,
        };
        let database = written.database.as_deref().unwrap_or(&self.state.database);
        let object = snapshot.resolve_object(
            database,
            written.schema.as_deref(),
            &written.object,
            DEFAULT_SCHEMA,
        )?;
        catalog
            .identity_current(txn, ObjectId(object.id.0))
            .ok()
            .flatten()
    }

    fn xact_state(&self) -> i16 {
        if self.state.trancount == 0 { 0 } else { 1 }
    }

    fn lock_timeout(&self) -> i32 {
        self.state.options.lock_timeout
    }

    fn spid(&self) -> i16 {
        self.state.spid
    }

    fn current_database(&self) -> &str {
        &self.state.database
    }

    /// The name the instance was started under, carried by the session since `server.rs`
    /// wrote `ServerConfig::server_name` into the state at login.
    ///
    /// `@@SERVERNAME` (`sysfn/src/builtins/system.rs`) and `SERVERPROPERTY('ServerName')`
    /// (`compat/src/server_properties.rs`) both call this method, so the two answer one
    /// value; the state is the single field they read (unit tests
    /// `server_name_comes_from_the_session_state` here and
    /// `the_configured_server_name_reaches_the_evaluation_context` in `server.rs`). A state
    /// built without a login keeps `SessionState::DEFAULT_SERVER_NAME`.
    fn server_name(&self) -> &str {
        &self.state.server_name
    }

    /// The identifier the catalogue of the statement holds for `name`, `None` without a
    /// catalogue and for a name it resolves nothing for (module documentation, table of
    /// forms).
    ///
    /// The number is the one this engine handed out, not the one SQL Server would have.
    /// `ObjectId` is an `i32` in the catalogue as well, so the answer loses nothing on the
    /// way.
    fn object_id(&self, name: &str) -> Option<i32> {
        object_id_in(self.catalog()?, &self.state.database, name)
    }

    /// [`EvalContext::object_id`], once each user table the name reaches, and the table of
    /// that name another transaction creates or drops, is out of the `Sch-M` of another
    /// transaction; resolved again on the catalogue of that moment when one had to be waited
    /// for (`tests/ddl_visibility.rs`,
    /// `object_id_waits_for_an_uncommitted_create_or_drop_of_its_table`). Under
    /// `SET LOCK_TIMEOUT` the wait ends with 1222 state 51.
    fn object_id_when_stable(&self, name: &str) -> SqlResult<Option<i32>> {
        let (
            Some((manager, wait)),
            Some(CatalogSource::Deferred {
                catalog,
                txn,
                built,
            }),
        ) = (&self.schema_waits, &self.catalog)
        else {
            return Ok(self.object_id(name));
        };
        let snapshot = built.get_or_init(|| catalog.snapshot(txn));
        let _ = snapshot.take_resolved_tables(false);
        let found = object_id_in(snapshot, &self.state.database, name);
        let timeout = lock_timeout_from_ms(self.state.options.lock_timeout);
        let mut waited = false;
        for table in snapshot.take_resolved_tables(false) {
            waited |= manager
                .wait_for_schema_stability(txn, table, timeout, wait)
                .map_err(|mut err| {
                    if err.number == crate::batch::LOCK_REQUEST_TIMEOUT {
                        err.state = LOCK_TIMEOUT_STATE_OF_A_METADATA_FUNCTION;
                    }
                    err
                })?;
        }
        if !waited {
            return Ok(found);
        }
        Ok(object_id_in(
            &catalog.snapshot(txn),
            &self.state.database,
            name,
        ))
    }

    /// The object part of the name the catalogue holds for `id`, `None` without a catalogue
    /// and for an identifier it does not hold (`OBJECT_NAME(-987654321)`).
    ///
    /// `-1` **is** held: the catalogue numbers the system views `-1`...`-24`, so
    /// `OBJECT_NAME(-1)` answers `objects` here and `NULL` on SQL Server, whose own system
    /// views are numbered elsewhere. An identifier outside both numberings, such as
    /// `-987654321`, is what probes an absent object (`object_id_unknown_is_none`).
    ///
    /// The object part alone, `OBJECT_NAME` being typed `sysname`: the schema is
    /// `OBJECT_SCHEMA_NAME`'s and the database is the second argument's, which `sysfn`
    /// accepts and ignores (`builtins/objects.rs`). The snapshot is read for any
    /// identifier it holds, without filtering on the database the entry belongs to, so the
    /// round trip of `object_name_roundtrip` also holds for a system view whose session sits
    /// in another database than `master` (`a_system_view_resolves_and_names_itself`).
    fn object_name(&self, id: i32) -> Option<String> {
        let catalog = self.catalog()?;
        catalog.object_name(ObjectId(id)).map(|name| name.name)
    }

    fn variable(&self, name: &str) -> Option<Value> {
        // `@@ROWCOUNT` is **not** here: it has its own method on the trait. The comparison
        // is case-insensitive because T-SQL names are.
        if name.eq_ignore_ascii_case("@@ERROR") {
            // `@@ERROR` is an `int`; an error number never exceeds `i32::MAX` in practice,
            // and a number that did would be reported as 0 rather than wrap.
            return Some(Value::I32(
                i32::try_from(self.state.last_error).unwrap_or(0),
            ));
        }
        if name.eq_ignore_ascii_case("@@TRANCOUNT") {
            return Some(Value::I32(self.state.trancount));
        }
        if name.eq_ignore_ascii_case("@@LOCK_TIMEOUT") {
            return Some(Value::I32(self.state.options.lock_timeout));
        }
        if name.eq_ignore_ascii_case("@@DEADLOCK_PRIORITY") {
            return Some(Value::I32(i32::from(self.state.options.deadlock_priority)));
        }
        None
    }

    fn host_name(&self) -> Option<&str> {
        // An empty LOGIN7 hostname is an empty nvarchar(128), not NULL.
        Some(&self.state.hostname)
    }

    fn app_name(&self) -> Option<&str> {
        // An empty LOGIN7 application name is an empty nvarchar(128), not NULL.
        Some(&self.state.app_name)
    }

    fn login_name(&self) -> Option<&str> {
        non_empty(&self.state.login)
    }

    fn session_property(&self, name: &str) -> Option<i32> {
        let on = |enabled: bool| i32::from(enabled);
        let opts = &self.state.options;
        match name.to_ascii_uppercase().as_str() {
            "ANSI_NULLS" => Some(on(opts.ansi_nulls)),
            "ANSI_PADDING" => Some(on(opts.ansi_padding)),
            "ANSI_WARNINGS" => Some(on(opts.ansi_warnings)),
            "ARITHABORT" => Some(on(opts.arithabort)),
            "CONCAT_NULL_YIELDS_NULL" => Some(on(opts.concat_null_yields_null)),
            "QUOTED_IDENTIFIER" => Some(on(opts.quoted_identifier)),
            "NUMERIC_ROUNDABORT" => Some(on(opts.numeric_roundabort)),
            _ => None,
        }
    }

    /// The identifier of the database `name`, or of the current one when `name` is `None`
    /// (`DB_ID`), `None` without a catalogue and for a name no database of the snapshot
    /// carries (`db_id_master_is_some`).
    ///
    /// The name is compared by [`CatalogSnapshot::database`], under the default collation,
    /// so `MASTER` and `master` are the same database. `DbId` is a `u32` in `storage` where
    /// `DB_ID` is a `smallint`: an identifier too large for an `i32` answers `None` here, and
    /// `sysfn` narrows what is left to `i16` (`builtins/objects.rs`, `db_id_eval`).
    fn database_id(&self, name: Option<&str>) -> Option<i32> {
        let catalog = self.catalog()?;
        let database = catalog.database(name.unwrap_or(&self.state.database))?;
        i32::try_from(database.id.0).ok()
    }

    /// The name of the database of identifier `id` (`DB_NAME(id)`), read from the snapshot
    /// for the current database and for another one alike, the spelling the catalogue holds
    /// (`database_name_answers_any_database_of_the_snapshot`).
    ///
    /// `DB_NAME(0)` names the current database, as `DB_NAME()` does; `DB_NAME(-1)` and an
    /// identifier no database carries are `NULL`, as on SQL Server.
    fn database_name(&self, id: i32) -> Option<String> {
        let catalog = self.catalog()?;
        let database = match u32::try_from(id).ok()? {
            0 => catalog.database(&self.state.database)?,
            id => catalog.database_by_id(vauban_storage::DbId(id))?,
        };
        Some(database.name.clone())
    }

    /// The name of the schema of identifier `id` (`SCHEMA_NAME(id)`), read from the
    /// snapshot of the current database, `None` without a catalogue, when the session's
    /// database is not in the snapshot, or when the identifier matches no schema the
    /// catalogue holds (`schema_name_from_identifier`).
    fn schema_name(&self, id: i32) -> Option<String> {
        let catalog = self.catalog()?;
        catalog.database(&self.state.database)?;
        catalog.schema_name(id)
    }

    /// The identifier of the schema `name`, or of the session's default schema when `name`
    /// is `None` (`SCHEMA_ID`): [`DEFAULT_SCHEMA_ID`] for [`DEFAULT_SCHEMA`], `None` for a
    /// name that is not it, the bound of the module documentation, section
    /// "`DB_NAME(id)`, `SCHEMA_NAME` and `SCHEMA_ID`"
    /// (`schema_id_of_another_schema_is_not_resolved_yet`).
    ///
    /// The comparison is [`Collation::DEFAULT`], the one [`CatalogSnapshot::database`] uses
    /// on a database name, so `DBO` and a trailing blank name the same schema as `dbo`,
    /// where a leading blank does not: `SCHEMA_ID('DBO')` and `SCHEMA_ID('dbo ')` answer `1`
    /// and `SCHEMA_ID(' dbo')` answers `NULL`, as on SQL Server
    /// (`schema_id_compares_under_the_collation`).
    /// No snapshot is read: the schema this answers is the session's own default, and the
    /// catalogue publishes no schema entry to read instead.
    fn schema_id(&self, name: Option<&str>) -> Option<i32> {
        let name = name.unwrap_or(DEFAULT_SCHEMA);
        (Collation::DEFAULT.compare(name, DEFAULT_SCHEMA) == Ordering::Equal)
            .then_some(DEFAULT_SCHEMA_ID)
    }

    fn datefirst(&self) -> u8 {
        self.state.options.datefirst
    }

    fn language(&self) -> &str {
        &self.state.options.language
    }

    fn version_banner(&self) -> Option<&str> {
        Some(&self.state.version_banner)
    }

    fn edition(&self) -> Option<&str> {
        Some(&self.state.edition)
    }
}

#[cfg(test)]
mod tests;
