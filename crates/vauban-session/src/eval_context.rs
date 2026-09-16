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
//! ## Two answers this file leaves to a later reader API of the catalogue
//!
//! `SCHEMA_NAME(id)` and `DB_NAME(id)` need a lookup by identifier that
//! [`CatalogSnapshot`] does not publish: it answers a database by **name**
//! ([`CatalogSnapshot::database`]) and carries no schema entry (an `ObjectMeta` holds a
//! schema *name*, no `schema_id`; `schema_name_is_not_resolved_yet`). So:
//!
//! - [`EvalContext::schema_name`] is left on its trait default, `None`, where SQL Server
//!   answers `dbo`, `guest`, `INFORMATION_SCHEMA` and `sys` for 1, 2, 3 and 4
//!   (`schema_name_is_not_resolved_yet`). `SCHEMA_NAME()` without an argument is `dbo`
//!   inside `sysfn` and does not come through this file;
//! - [`EvalContext::schema_id`] answers the default schema, `dbo` = 1, and `None`
//!   for another name, where SQL Server answers 2, 3 and 4 for `guest`,
//!   `INFORMATION_SCHEMA` and `sys` (`schema_id_of_another_schema_is_not_resolved_yet`).
//!   Reading those three needs the same lookup: `CatalogSnapshot` carries no schema entry
//!   and the numbers the bootstrap writes in `sys.schemas` are `pub(crate)` to
//!   `vauban_catalog`;
//! - `database_name` answers the **current** database and `None` for another identifier,
//!   where SQL Server answers `tempdb` for 2
//!   (`database_name_answers_the_current_database_only`).
//!
//! Both widen when `catalog` publishes those lookups.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_catalog::{Catalog, CatalogSnapshot, ObjectId};
use vauban_sysfn::EvalContext;
use vauban_txn::TxnHandle;
use vauban_types::calendar::days_from_civil;
use vauban_types::{Collation, Date, DateTime2, Decimal, Time, Value};

use crate::state::SessionState;

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
/// (module documentation, section "Two answers this file leaves to a later reader API of
/// the catalogue").
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
}

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
        }
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
        // The session does not track the identity values `INSERT` generates.
        None
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
        let catalog = self.catalog()?;
        let written = WrittenName::parse(name)?;
        if written.server.is_some() {
            // A four-part name names a linked server, which no snapshot serves: the rule
            // of `binder/catalog_view.rs`, and `NULL` on SQL Server.
            return None;
        }
        let database = written.database.as_deref().unwrap_or(&self.state.database);
        let object = catalog.resolve_object(
            database,
            written.schema.as_deref(),
            &written.object,
            DEFAULT_SCHEMA,
        )?;
        Some(object.id.0)
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

    /// The name of the database of identifier `id` (`DB_NAME(id)`), answered for the
    /// **current** database and `None` for another identifier, the bound of the module
    /// documentation, section "Two answers this file leaves to a later reader API of the
    /// catalogue" (`database_name_answers_the_current_database_only`).
    ///
    /// `DB_NAME(-1)` is `NULL` here as on SQL Server.
    fn database_name(&self, id: i32) -> Option<String> {
        let catalog = self.catalog()?;
        let current = catalog.database(&self.state.database)?;
        (i32::try_from(current.id.0) == Ok(id)).then(|| current.name.clone())
    }

    /// The identifier of the schema `name`, or of the session's default schema when `name`
    /// is `None` (`SCHEMA_ID`): [`DEFAULT_SCHEMA_ID`] for [`DEFAULT_SCHEMA`], `None` for a
    /// name that is not it, the bound of the module documentation, section "Two answers
    /// this file leaves to a later reader API of the catalogue"
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
mod tests {
    use std::sync::Arc;

    use vauban_catalog::{ColumnDef, QualifiedName, TableDef};
    use vauban_storage::MemoryStorage;
    use vauban_txn::IsolationLevel;
    use vauban_types::{SqlType, TypeInfo};

    use super::*;
    use crate::server::Engine;

    fn state() -> SessionState {
        let mut state = SessionState::new(57);
        state.login = "sa".into();
        state.app_name = "sqlcmd".into();
        state.hostname = "WORKSTATION".into();
        state.rowcount = 3;
        state.last_error = 8134;
        state
    }

    #[test]
    fn reads_the_session_state() {
        let state = state();
        let ctx = SessionEvalContext::new(&state, None);
        let ctx: &dyn EvalContext = &ctx;
        assert_eq!(ctx.spid(), 57);
        assert_eq!(ctx.current_database(), "master");
        assert_eq!(ctx.rowcount(), 3);
        assert_eq!(ctx.login_name(), Some("sa"));
        assert_eq!(ctx.app_name(), Some("sqlcmd"));
        assert_eq!(ctx.host_name(), Some("WORKSTATION"));
        assert_eq!(ctx.datefirst(), 7);
        assert_eq!(ctx.language(), "us_english");
        assert_eq!(
            ctx.server_name(),
            "vauban",
            "the default of a state built by hand"
        );
    }

    /// The name of the state is the one answered, and a session started elsewhere answers
    /// elsewhere: the counter-proof of a constant answer is that the two names below differ
    /// from each other and from the default `vauban`.
    #[test]
    fn server_name_comes_from_the_session_state() {
        for name in ["VAUBAN-NODE-2", "sql-01\\INSTANCE"] {
            let mut state = state();
            state.server_name = name.into();
            let ctx = SessionEvalContext::new(&state, None);
            let ctx: &dyn EvalContext = &ctx;
            assert_eq!(ctx.server_name(), name);
            assert_ne!(ctx.server_name(), "vauban");
        }
    }

    #[test]
    fn login7_host_and_application_preserve_empty_space_and_128_characters() {
        // SQL Server 2022: empty, one ASCII space and 128 ASCII characters survive
        // HOST_NAME()/APP_NAME(); an empty username is rejected before evaluation.
        for text in [String::new(), " ".to_owned(), "x".repeat(128)] {
            let mut state = SessionState::new(51);
            state.hostname = text.clone();
            state.app_name = text.clone();
            state.login = "sa".into();
            let ctx = SessionEvalContext::new(&state, None);
            assert_eq!(ctx.host_name(), Some(text.as_str()));
            assert_eq!(ctx.app_name(), Some(text.as_str()));
            assert_eq!(ctx.login_name(), Some("sa"));
        }
        let state = SessionState::new(51);
        assert_eq!(SessionEvalContext::new(&state, None).login_name(), None);
    }

    #[test]
    fn error_and_trancount_are_session_variables() {
        let mut state = state();
        state.trancount = 2;
        let ctx = SessionEvalContext::new(&state, None);
        let ctx: &dyn EvalContext = &ctx;
        assert_eq!(ctx.variable("@@ERROR"), Some(Value::I32(8134)));
        assert_eq!(ctx.variable("@@TRANCOUNT"), Some(Value::I32(2)));
        // T-SQL names are case-insensitive; the parser hands the text as written.
        assert_eq!(ctx.variable("@@error"), Some(Value::I32(8134)));
        assert_eq!(ctx.variable("@@ROWCOUNT"), None, "own method on the trait");
        assert_eq!(ctx.variable("@@FETCH_STATUS"), None);
    }

    #[test]
    fn version_banner_and_edition_come_from_the_session_state() {
        let mut state = state();
        state.version_banner = "CustomBanner".into();
        state.edition = "Custom Edition".into();
        let ctx = SessionEvalContext::new(&state, None);
        let ctx: &dyn EvalContext = &ctx;
        assert_eq!(ctx.version_banner(), Some("CustomBanner"));
        assert_eq!(ctx.edition(), Some("Custom Edition"));
    }

    #[test]
    fn the_clock_is_read_once_and_never_again() {
        let state = SessionState::new(51);
        let ctx = SessionEvalContext::new(&state, None);
        let first = ctx.now_local();
        // Enough 100 ns ticks pass here for a clock read per call to differ.
        for _ in 0..10_000 {
            assert_eq!(ctx.now_local(), first);
        }
        assert_eq!(ctx.now_utc(), first);
    }

    #[test]
    fn two_contexts_read_two_instants() {
        // The freeze is per context, that is per statement: a second statement of the same
        // batch reads the clock again. Monotonic, not necessarily different: the assertion
        // is on the order, which a per-call read would also satisfy — what distinguishes
        // the two is `the_clock_is_read_once_and_never_again` above.
        let state = SessionState::new(51);
        let first = SessionEvalContext::new(&state, None).now_utc();
        let second = SessionEvalContext::new(&state, None).now_utc();
        assert!(
            second.date.days > first.date.days || second.time.ticks_100ns >= first.time.ticks_100ns
        );
    }

    #[test]
    fn the_clock_lands_in_this_century() {
        // Guards the epoch conversion: a wrong origin lands centuries away. 2020-01-01 and
        // 2200-01-01 in days since 0001-01-01.
        let state = SessionState::new(51);
        let now = SessionEvalContext::new(&state, None).now_utc();
        assert!(now.date.days > days_from_civil(2020, 1, 1), "{now:?}");
        assert!(now.date.days < days_from_civil(2200, 1, 1), "{now:?}");
        assert!(
            now.time.ticks_100ns < SECONDS_PER_DAY * TICKS_PER_SECOND,
            "{now:?}"
        );
    }

    #[test]
    fn the_clock_can_be_given_by_the_caller() {
        let state = SessionState::new(51);
        let noon = DateTime2 {
            date: Date {
                days: days_from_civil(2026, 9, 10),
            },
            time: Time {
                ticks_100ns: 12 * 3600 * TICKS_PER_SECOND,
            },
        };
        let ctx = SessionEvalContext::with_clock(&state, noon);
        assert_eq!(ctx.now_local(), noon);
        assert_eq!(ctx.now_utc(), noon);
    }

    // The metadata functions read the catalogue of the statement.

    /// The table the tests of `OBJECT_ID` resolve, `dbo.t` of `master` with one `int`
    /// column (module documentation).
    fn table_def() -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "t".to_owned(),
            },
            columns: vec![ColumnDef {
                name: "a".to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                default: None,
                identity: None,
                computed: None,
            }],
            constraints: Vec::new(),
        }
    }

    /// An engine bootstrapped by `Engine::new` with `dbo.t` created in `master`, and the
    /// identifier the catalogue gave that table.
    fn engine_with_a_table() -> (Engine, i32) {
        let engine = Engine::new(Arc::new(MemoryStorage::new()));
        let created = {
            let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
            let meta = engine
                .catalog
                .create_table(&handle, &table_def())
                .expect("the table is created");
            engine
                .txn
                .commit(handle)
                .expect("the creating transaction commits");
            meta.id.0
        };
        (engine, created)
    }

    /// A snapshot of [`engine_with_a_table`] and the identifier of its table — what
    /// `batch.rs` hands to [`SessionEvalContext::new`] while it binds a batch.
    fn catalogue_with_a_table() -> (CatalogSnapshot, i32) {
        let (engine, created) = engine_with_a_table();
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let snapshot = engine.catalog.snapshot(&handle);
        engine
            .txn
            .commit(handle)
            .expect("the reading transaction commits");
        (snapshot, created)
    }

    #[test]
    fn object_id_of_created_table_is_some() {
        let (snapshot, created) = catalogue_with_a_table();
        let state = state();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        // Each form resolves (module documentation); here each answers the identifier
        // `create_table` handed out, which is more than `is_some`.
        for written in [
            "dbo.t",
            "[dbo].[t]",
            "\"dbo\".\"t\"",
            "t",
            "DBO.T",
            "master.dbo.t",
            ".t",
            ".dbo.t",
            "..dbo.t",
            "[].[].dbo.t",
            ".master.dbo.t",
            "[master].[dbo].[t]",
            "dbo.t ",
            "[dbo].[t ]",
        ] {
            assert_eq!(ctx.object_id(written), Some(created), "{written}");
        }
    }

    #[test]
    fn object_name_roundtrip() {
        let (snapshot, created) = catalogue_with_a_table();
        let state = state();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        // The object part alone, `OBJECT_NAME` being a `sysname` and not a three-part name.
        assert_eq!(ctx.object_name(created), Some("t".to_owned()));
        // The name comes back with the case it was created with, not the case written:
        // `OBJECT_NAME(OBJECT_ID('DBO.T'))` is `t` (`CatalogSnapshot`).
        let resolved = ctx.object_id("DBO.T").expect("the table resolves");
        assert_eq!(resolved, created);
        assert_eq!(ctx.object_name(resolved), Some("t".to_owned()));
    }

    #[test]
    fn object_id_unknown_is_none() {
        let (snapshot, _) = catalogue_with_a_table();
        let state = state();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        // The forms below resolve to nothing, that is NULL (module documentation): an
        // absent table, a written server part, five parts, a blank inside an undelimited
        // part, a character outside the delimiters of a part, an empty object part, a
        // delimiter left open, another schema, and a database this snapshot does not hold.
        for written in [
            "dbo.no_such_table",
            "no_such_table",
            "srv.master.dbo.t",
            "a.b.master.dbo.t",
            " dbo.t ",
            "dbo . t",
            "[dbo].[t] ",
            "[dbo].[t]\t",
            "[dbo].[t]x",
            "[dbo] .[t]",
            " [dbo].[t]",
            "[dbo] .t",
            "dbo.",
            "",
            "[dbo].[t",
            "sys.t",
            "no_such_db.dbo.t",
        ] {
            assert_eq!(ctx.object_id(written), None, "{written}");
        }
        // An identifier far outside the range this snapshot holds: NULL, as on SQL Server.
        assert_eq!(ctx.object_name(-100_000), None);
        assert_eq!(ctx.object_name(i32::MAX), None);
        // An identifier outside both numberings: NULL here and on SQL Server, where `-1`
        // names `sys.objects` here (see `object_name` above).
        assert_eq!(ctx.object_name(-987_654_321), None);
        assert_eq!(ctx.object_name(-1), Some("objects".to_owned()));
    }

    #[test]
    fn db_id_master_is_some() {
        let (snapshot, _) = catalogue_with_a_table();
        let state = state();
        assert_eq!(state.database, "master", "the session starts in `master`");
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        let master = ctx.database_id(None).expect("`master` has an identifier");
        // Named, and named in another case: `CatalogSnapshot::database` compares under the
        // default collation.
        assert_eq!(ctx.database_id(Some("master")), Some(master));
        assert_eq!(ctx.database_id(Some("MASTER")), Some(master));
        // Another database of the bootstrap carries an identifier of its own.
        let tempdb = ctx
            .database_id(Some("tempdb"))
            .expect("`tempdb` is in the bootstrap");
        assert_ne!(tempdb, master);
        assert_eq!(ctx.database_id(Some("no_such_db")), None);
    }

    #[test]
    fn database_name_answers_the_current_database_only() {
        let (snapshot, _) = catalogue_with_a_table();
        let state = state();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        let master = ctx.database_id(None).expect("`master` has an identifier");
        assert_eq!(ctx.database_name(master), Some("master".to_owned()));
        // `DB_NAME(-1)`: NULL here and on SQL Server.
        assert_eq!(ctx.database_name(-1), None);
        // The bound of the module documentation: another database of the same instance is
        // NULL here, where SQL Server answers its name (`DB_NAME(2)` is `tempdb`).
        let tempdb = ctx
            .database_id(Some("tempdb"))
            .expect("`tempdb` is in the bootstrap");
        assert_eq!(ctx.database_name(tempdb), None);
    }

    #[test]
    fn schema_name_is_not_resolved_yet() {
        // SQL Server answers `dbo`, `INFORMATION_SCHEMA` and `sys` for 1, 3 and 4;
        // `CatalogSnapshot` publishes no schema identifier, so this context answers NULL.
        // A reader API of `catalog` lifts it; `SCHEMA_ID` was added without one, so the
        // name of an identifier is still unresolved here.
        let (snapshot, _) = catalogue_with_a_table();
        let state = state();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        for id in [1, 3, 4, 99] {
            assert_eq!(ctx.schema_name(id), None, "{id}");
        }
    }

    /// `SCHEMA_ID()` and `SCHEMA_ID('dbo')` answer 1, the number of the default schema, with
    /// a catalogue as without one: this answer is the session's, not the snapshot's, and
    /// SQL Server answers 1 to both forms.
    #[test]
    fn schema_id_of_the_default_schema_is_one() {
        let (snapshot, _) = catalogue_with_a_table();
        let state = state();
        let with_catalogue = SessionEvalContext::new(&state, Some(&snapshot));
        let with_catalogue: &dyn EvalContext = &with_catalogue;
        assert_eq!(with_catalogue.schema_id(None), Some(1));
        assert_eq!(with_catalogue.schema_id(Some("dbo")), Some(1));
        let without = SessionEvalContext::new(&state, None);
        let without: &dyn EvalContext = &without;
        assert_eq!(without.schema_id(None), Some(1));
        assert_eq!(without.schema_id(Some("dbo")), Some(1));
    }

    /// The comparison is the default collation's: case folded and trailing blanks trimmed,
    /// a leading blank kept. `SCHEMA_ID('DBO')` and `SCHEMA_ID('dbo ')` are 1 on SQL Server,
    /// `SCHEMA_ID(' dbo')` is NULL there; the leading blank is the vector that separates
    /// "trailing blanks trimmed" from "blanks trimmed".
    #[test]
    fn schema_id_compares_under_the_collation() {
        let state = state();
        let ctx = SessionEvalContext::new(&state, None);
        let ctx: &dyn EvalContext = &ctx;
        assert_eq!(ctx.schema_id(Some("DBO")), Some(1));
        assert_eq!(ctx.schema_id(Some("dbo ")), Some(1));
        assert_eq!(ctx.schema_id(Some(" dbo")), None);
        assert_eq!(ctx.schema_id(Some("[dbo]")), None);
    }

    /// A schema that is not the default one is not resolved here, the bound of the module
    /// documentation: SQL Server answers 2, 3 and 4 for `guest`, `INFORMATION_SCHEMA` and
    /// `sys` where this context answers `None` for the three, as for `no_such_schema`,
    /// which neither server holds (the four assertions below).
    #[test]
    fn schema_id_of_another_schema_is_not_resolved_yet() {
        let (snapshot, _) = catalogue_with_a_table();
        let state = state();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        for name in ["sys", "INFORMATION_SCHEMA", "guest", "no_such_schema"] {
            assert_eq!(ctx.schema_id(Some(name)), None, "{name}");
        }
    }

    #[test]
    fn a_system_view_resolves_and_names_itself() {
        // `sys.tables` is an entry of `views/` with a negative identifier, and it
        // resolves from a session whose database is not `master`, as on SQL Server.
        let (snapshot, created) = catalogue_with_a_table();
        let mut state = state();
        state.database = "msdb".to_owned();
        let ctx = SessionEvalContext::new(&state, Some(&snapshot));
        let ctx: &dyn EvalContext = &ctx;
        let view = ctx.object_id("sys.tables").expect("`sys.tables` resolves");
        assert!(view < 0, "a system view carries a negative id: {view}");
        assert_eq!(ctx.object_name(view), Some("tables".to_owned()));
        // The table of `master` is out of reach of this session unless its database is
        // written, the schema `dbo` of `msdb` holding no `t`.
        assert_eq!(ctx.object_id("dbo.t"), None);
        assert_eq!(ctx.object_id("master.dbo.t"), Some(created));
    }

    #[test]
    fn the_deferred_catalogue_is_read_on_the_first_call_and_not_before() {
        // What `batch.rs` builds for a statement: the snapshot is taken from the transaction
        // of the statement, and only if something asks for it. A statement that calls no
        // metadata function leaves the cell empty.
        let (engine, created) = engine_with_a_table();
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let state = state();
        let ctx = SessionEvalContext::deferred(&state, &engine.catalog, &handle);
        let built = |ctx: &SessionEvalContext<'_>| match &ctx.catalog {
            Some(CatalogSource::Deferred { built, .. }) => built.get().is_some(),
            _ => unreachable!("`deferred` builds a `Deferred` source"),
        };
        assert!(!built(&ctx), "nothing has asked for the catalogue yet");
        assert_eq!(ctx.rowcount(), 3, "the session state needs no catalogue");
        assert!(!built(&ctx));
        assert_eq!(ctx.object_id("dbo.t"), Some(created));
        assert!(built(&ctx), "the first call read the catalogue");
        // Read once: the second call answers from the same snapshot.
        assert_eq!(ctx.object_name(created), Some("t".to_owned()));
        engine
            .txn
            .commit(handle)
            .expect("the reading transaction commits");
    }

    #[test]
    fn without_a_catalogue_nothing_resolves() {
        // The counter-proof of the four catalogue methods above: a context built without a
        // snapshot answers NULL for the seven calls below.
        let state = state();
        let ctx = SessionEvalContext::new(&state, None);
        let ctx: &dyn EvalContext = &ctx;
        assert_eq!(ctx.object_id("dbo.t"), None);
        assert_eq!(ctx.object_id("sys.tables"), None);
        assert_eq!(ctx.object_name(1), None);
        assert_eq!(ctx.database_id(None), None);
        assert_eq!(ctx.database_id(Some("master")), None);
        assert_eq!(ctx.database_name(1), None);
        assert_eq!(ctx.schema_name(1), None);
    }

    #[test]
    fn a_written_name_is_split_on_its_undelimited_dots() {
        // The splitting `object_id` resolves through. An empty schema or database part is
        // `None`, so `.t` and `..t` name `t` of the default schema of the current database,
        // as on SQL Server.
        assert_eq!(
            WrittenName::parse("t"),
            Some(WrittenName {
                server: None,
                database: None,
                schema: None,
                object: "t".to_owned(),
            })
        );
        assert_eq!(
            WrittenName::parse("[my db].[my schema].[my table]"),
            Some(WrittenName {
                server: None,
                database: Some("my db".to_owned()),
                schema: Some("my schema".to_owned()),
                object: "my table".to_owned(),
            })
        );
        assert_eq!(WrittenName::parse(".t").and_then(|name| name.schema), None);
        assert_eq!(
            WrittenName::parse("..t").and_then(|name| name.database),
            None
        );
        assert_eq!(
            WrittenName::parse("..t").map(|name| name.object),
            Some("t".to_owned())
        );
        assert_eq!(
            WrittenName::parse("srv.db.dbo.t").and_then(|name| name.server),
            Some("srv".to_owned())
        );
        // A dot inside a delimiter is part of the name, and a doubled closing delimiter
        // stands for itself: `[dbo]].[t]` is the single name `dbo].[t`, which resolves to
        // nothing on SQL Server too.
        assert_eq!(
            WrittenName::parse("[a.b]").map(|name| name.object),
            Some("a.b".to_owned())
        );
        assert_eq!(
            WrittenName::parse("[dbo]].[t]").map(|name| name.object),
            Some("dbo].[t".to_owned())
        );
        // Five parts and a delimiter left open give `None`, NULL on SQL Server.
        assert_eq!(WrittenName::parse("a.b.c.d.e"), None);
        assert_eq!(WrittenName::parse("[dbo].[t"), None);
        assert_eq!(WrittenName::parse("\"dbo"), None);
        // An empty server part is read as "not written", like an empty database and an empty
        // schema.
        assert_eq!(
            WrittenName::parse("..dbo.t"),
            Some(WrittenName {
                server: None,
                database: None,
                schema: Some("dbo".to_owned()),
                object: "t".to_owned(),
            })
        );
        assert_eq!(
            WrittenName::parse("[].[].dbo.t"),
            WrittenName::parse("..dbo.t")
        );
        // A delimited part fills its part: a character before the opening delimiter or after
        // the closing one refuses the whole name, where the same character inside the
        // delimiter is part of it (table of `split_parts`).
        for refused in [
            "[dbo].[t] ",
            "[dbo].[t]\t",
            "[dbo].[t]x",
            "[dbo] .[t]",
            " [dbo].[t]",
            "[dbo] .t",
            "[a][b]",
        ] {
            assert_eq!(WrittenName::parse(refused), None, "{refused}");
        }
        assert_eq!(
            WrittenName::parse("[dbo].[t ]").map(|name| name.object),
            Some("t ".to_owned())
        );
        assert_eq!(
            WrittenName::parse("dbo.t ").map(|name| name.object),
            Some("t ".to_owned())
        );
    }
}
