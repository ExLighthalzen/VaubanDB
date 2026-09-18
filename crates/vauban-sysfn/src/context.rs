//! The contract through which a built-in function reaches the session.
//!
//! `sysfn` knows nothing about `session`, `catalog` or `txn`: everything a function needs
//! from its environment goes through [`EvalContext`], provided by the caller. The
//! [`StaticContext`] implementation answers with fixed values and serves the tests of every
//! crate that registers functions, as well as the evaluation of constant expressions
//! outside a session.

use vauban_types::{Date, DateTime2, Decimal, Time, Value};

/// Session-dependent information a built-in function may need while evaluating.
///
/// Implemented by the session and by [`StaticContext`] for tests. The methods
/// are read-only: a function never mutates the session through this trait.
pub trait EvalContext {
    /// Current date and time in the server's local time zone (`GETDATE()`, `SYSDATETIME()`).
    fn now_local(&self) -> DateTime2;

    /// Current date and time in UTC (`GETUTCDATE()`, `SYSUTCDATETIME()`).
    fn now_utc(&self) -> DateTime2;

    /// Number of rows affected by the last statement (`@@ROWCOUNT`).
    fn rowcount(&self) -> i64;

    /// Last identity value inserted in the session (`@@IDENTITY`, `SCOPE_IDENTITY()`),
    /// `None` when no identity value has been generated yet.
    fn last_identity(&self) -> Option<Decimal>;

    /// Session identifier (`@@SPID`).
    fn spid(&self) -> i16;

    /// Name of the current database (`DB_NAME()`).
    fn current_database(&self) -> &str;

    /// Name of the server instance (`@@SERVERNAME`, `SERVERPROPERTY('ServerName')`).
    fn server_name(&self) -> &str;

    /// Resolves an object name to its `object_id` through the catalogue (`OBJECT_ID`),
    /// `None` when the object does not exist.
    fn object_id(&self, name: &str) -> Option<i32>;

    /// Resolves an `object_id` to its name through the catalogue (`OBJECT_NAME`),
    /// `None` when no object has this id.
    fn object_name(&self, id: i32) -> Option<String>;

    /// Value of a session variable such as `@@ERROR` or `@@TRANCOUNT`, `None` when the
    /// session does not know it. `name` is passed as written in the query (with `@@`).
    ///
    /// Returned **by value**: the session computes these variables when asked and does not
    /// keep them in a live structure, which a borrowed answer would force it to do. The
    /// values are scalars, so the copy costs nothing.
    fn variable(&self, name: &str) -> Option<Value>;

    /// Transaction state of the session (`XACT_STATE()`), a `smallint`.
    ///
    /// `0` by default: the value of `SELECT XACT_STATE();` alone in its batch and outside
    /// any `BEGIN TRANSACTION`. The session plugs its own state in; the answer of a
    /// context that overrides nothing is that of a session with no explicit transaction.
    fn xact_state(&self) -> i16 {
        0
    }

    /// Lock timeout of the session in milliseconds (`@@LOCK_TIMEOUT`), as `SET LOCK_TIMEOUT`
    /// left it.
    ///
    /// `-1` by default (wait without a deadline), the value of `SELECT @@LOCK_TIMEOUT;`
    /// on a connection that has run no `SET LOCK_TIMEOUT`; right after `SET LOCK_TIMEOUT
    /// 5000` it is `5000`.
    fn lock_timeout(&self) -> i32 {
        -1
    }

    /// Number of open transactions in the session (`@@TRANCOUNT`).
    ///
    /// `0` by default, the value of `SELECT @@TRANCOUNT;` outside any explicit
    /// transaction. Its own method rather than [`EvalContext::variable`]: the session
    /// counts the nesting of `BEGIN TRANSACTION` and answers here.
    fn trancount(&self) -> i32 {
        0
    }

    /// Name of the client workstation (`HOST_NAME()`), `None` when the client did not send
    /// one. SQL Server returns `NULL` in that case, which the default reproduces.
    fn host_name(&self) -> Option<&str> {
        None
    }

    /// Name of the client application (`APP_NAME()`), `None` when the client did not send
    /// one.
    fn app_name(&self) -> Option<&str> {
        None
    }

    /// Login name of the connection (`SUSER_SNAME()`), `None` outside a session.
    fn login_name(&self) -> Option<&str> {
        None
    }

    /// Database user name of the connection (`USER_NAME()`), `None` outside a session.
    fn user_name(&self) -> Option<&str> {
        None
    }

    /// Identifier of a database by name, or of the current database when `name` is `None`
    /// (`DB_ID`). `None` when the database does not exist.
    fn database_id(&self, name: Option<&str>) -> Option<i32> {
        let _ = name;
        None
    }

    /// Name of a database from its identifier (`DB_NAME(id)`), `None` when no database has
    /// this identifier.
    fn database_name(&self, id: i32) -> Option<String> {
        let _ = id;
        None
    }

    /// Name of a schema from its identifier (`SCHEMA_NAME(id)`), `None` when no schema has
    /// this identifier.
    fn schema_name(&self, id: i32) -> Option<String> {
        let _ = id;
        None
    }

    /// Identifier of a schema by name, or of the session's default schema when `name` is
    /// `None` (`SCHEMA_ID`). `None` when the context resolves no schema of that name, which
    /// is the answer of the default: a caller without a catalogue gets `NULL`, as SQL Server
    /// answers `NULL` for a schema it does not have (`SCHEMA_ID('no_such_schema')`).
    fn schema_id(&self, name: Option<&str>) -> Option<i32> {
        let _ = name;
        None
    }

    /// Last identity value generated in the current scope (`SCOPE_IDENTITY()`), `None`
    /// when the scope generated none. Narrower than [`EvalContext::last_identity`].
    fn scope_identity(&self) -> Option<Decimal> {
        None
    }

    /// Last identity value generated for a table, whatever the session or scope
    /// (`IDENT_CURRENT`), `None` when the table is unknown or has no identity column.
    fn ident_current(&self, table: &str) -> Option<Decimal> {
        let _ = table;
        None
    }

    /// First day of the week, `1` (Monday) to `7` (Sunday), as `SET DATEFIRST` left it.
    /// `7` by default, the value of the `us_english` language.
    fn datefirst(&self) -> u8 {
        7
    }

    /// Language of the session, as `SET LANGUAGE` left it, used by `DATENAME` and by date
    /// parsing. `"us_english"` by default.
    fn language(&self) -> &str {
        "us_english"
    }

    /// Text of `@@VERSION`. `None` lets `compat` fall back to the default banner.
    fn version_banner(&self) -> Option<&str> {
        None
    }

    /// `SERVERPROPERTY('Edition')`. `None` lets `compat` fall back to the default edition.
    fn edition(&self) -> Option<&str> {
        None
    }

    /// Value of `SESSIONPROPERTY(name)`: `1` or `0` for a known option, `None` for an
    /// unknown name.
    fn session_property(&self, name: &str) -> Option<i32> {
        let _ = name;
        None
    }

    /// Local network address of the server endpoint
    /// (`CONNECTIONPROPERTY('local_net_address')`), when the session knows it.
    fn local_net_address(&self) -> Option<&str> {
        None
    }

    /// TCP port of the local server endpoint (`CONNECTIONPROPERTY('local_tcp_port')`),
    /// when the session knows it.
    fn local_tcp_port(&self) -> Option<i32> {
        None
    }

    /// Client network address (`CONNECTIONPROPERTY('client_net_address')`), when the
    /// session knows it.
    fn client_net_address(&self) -> Option<&str> {
        None
    }
}

/// An [`EvalContext`] made of fixed values, without any session.
///
/// `now_local` and `now_utc` both return `now`; `last_identity`, `object_id`, `object_name`
/// and `variable` return `None`, and every method left out of the fields below keeps the
/// default of the trait. The `Default` value is 0001-01-01 00:00:00, SPID 0, empty database
/// and server names, `rowcount` 0, no host, application, login or user name, `datefirst` 7,
/// and the three transaction values of a session outside a transaction: `xact_state` 0,
/// `lock_timeout` -1, `trancount` 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticContext {
    /// Value returned by both `now_local` and `now_utc`.
    pub now: DateTime2,
    /// Value returned by `spid`.
    pub spid: i16,
    /// Value returned by `current_database`.
    pub database: String,
    /// Value returned by `server_name`.
    pub server_name: String,
    /// Value returned by `rowcount`.
    pub rowcount: i64,
    /// Value returned by `host_name`.
    pub host_name: Option<String>,
    /// Value returned by `app_name`.
    pub app_name: Option<String>,
    /// Value returned by `login_name`.
    pub login_name: Option<String>,
    /// Value returned by `user_name`.
    pub user_name: Option<String>,
    /// Value returned by `datefirst`; `7` in the `Default` value.
    pub datefirst: u8,
    /// Value returned by `xact_state`; `0` in the `Default` value.
    pub xact_state: i16,
    /// Value returned by `lock_timeout`; `-1` in the `Default` value.
    pub lock_timeout: i32,
    /// Value returned by `trancount`; `0` in the `Default` value.
    pub trancount: i32,
    /// Value returned by `version_banner`; `None` in the `Default` value.
    pub version_banner: Option<String>,
    /// Value returned by `edition`; `None` in the `Default` value.
    pub edition: Option<String>,
}

impl Default for StaticContext {
    /// 0001-01-01 00:00:00, SPID 0, empty database and server names, `rowcount` 0, no
    /// host, application, login or user name, `datefirst` 7, `xact_state` 0,
    /// `lock_timeout` -1, `trancount` 0.
    fn default() -> Self {
        Self {
            now: DateTime2 {
                date: Date { days: 0 },
                time: Time { ticks_100ns: 0 },
            },
            spid: 0,
            database: String::new(),
            server_name: String::new(),
            rowcount: 0,
            host_name: None,
            app_name: None,
            login_name: None,
            user_name: None,
            datefirst: 7,
            xact_state: 0,
            lock_timeout: -1,
            trancount: 0,
            version_banner: None,
            edition: None,
        }
    }
}

impl EvalContext for StaticContext {
    fn now_local(&self) -> DateTime2 {
        self.now
    }

    fn now_utc(&self) -> DateTime2 {
        self.now
    }

    fn rowcount(&self) -> i64 {
        self.rowcount
    }

    fn last_identity(&self) -> Option<Decimal> {
        None
    }

    fn spid(&self) -> i16 {
        self.spid
    }

    fn current_database(&self) -> &str {
        &self.database
    }

    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn object_id(&self, _name: &str) -> Option<i32> {
        None
    }

    fn object_name(&self, _id: i32) -> Option<String> {
        None
    }

    fn variable(&self, _name: &str) -> Option<Value> {
        None
    }

    fn host_name(&self) -> Option<&str> {
        self.host_name.as_deref()
    }

    fn app_name(&self) -> Option<&str> {
        self.app_name.as_deref()
    }

    fn login_name(&self) -> Option<&str> {
        self.login_name.as_deref()
    }

    fn user_name(&self) -> Option<&str> {
        self.user_name.as_deref()
    }

    fn datefirst(&self) -> u8 {
        self.datefirst
    }

    fn xact_state(&self) -> i16 {
        self.xact_state
    }

    fn lock_timeout(&self) -> i32 {
        self.lock_timeout
    }

    fn trancount(&self) -> i32 {
        self.trancount
    }

    fn version_banner(&self) -> Option<&str> {
        self.version_banner.as_deref()
    }

    fn edition(&self) -> Option<&str> {
        self.edition.as_deref()
    }

    fn session_property(&self, name: &str) -> Option<i32> {
        session_property_defaults(name)
    }
}

/// Default `SESSIONPROPERTY` answers of a client connection, aligned with
/// [`SetOptions::default`] in `session` without importing that crate here.
fn session_property_defaults(name: &str) -> Option<i32> {
    let on = |enabled: bool| i32::from(enabled);
    match name.to_ascii_uppercase().as_str() {
        "ANSI_NULLS" => Some(on(true)),
        "ANSI_PADDING" => Some(on(true)),
        "ANSI_WARNINGS" => Some(on(true)),
        "ARITHABORT" => Some(on(false)),
        "CONCAT_NULL_YIELDS_NULL" => Some(on(true)),
        "QUOTED_IDENTIFIER" => Some(on(true)),
        "NUMERIC_ROUNDABORT" => Some(on(false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_context_returns_its_fixed_values() {
        let now = DateTime2 {
            date: Date { days: 730_119 },
            time: Time { ticks_100ns: 42 },
        };
        let ctx = StaticContext {
            now,
            spid: 57,
            database: "master".to_owned(),
            server_name: "VAUBAN".to_owned(),
            rowcount: 3,
            ..StaticContext::default()
        };
        let dyn_ctx: &dyn EvalContext = &ctx;
        assert_eq!(dyn_ctx.now_local(), now);
        assert_eq!(dyn_ctx.now_utc(), now);
        assert_eq!(dyn_ctx.spid(), 57);
        assert_eq!(dyn_ctx.current_database(), "master");
        assert_eq!(dyn_ctx.server_name(), "VAUBAN");
        assert_eq!(dyn_ctx.rowcount(), 3);
        assert_eq!(dyn_ctx.last_identity(), None);
        assert_eq!(dyn_ctx.object_id("dbo.t"), None);
        assert_eq!(dyn_ctx.object_name(1), None);
        assert_eq!(dyn_ctx.variable("@@ERROR"), None);
    }

    #[test]
    fn static_context_default_is_neutral() {
        let ctx = StaticContext::default();
        assert_eq!(ctx.spid(), 0);
        assert_eq!(ctx.rowcount(), 0);
        assert_eq!(ctx.current_database(), "");
        assert_eq!(ctx.server_name(), "");
        assert_eq!(ctx.now_local().date.days, 0);
        assert_eq!(ctx.now_local().time.ticks_100ns, 0);
    }

    /// A context that implements the methods without a default and nothing else: the
    /// defaulted methods must still answer.
    struct MinimalContext;

    impl EvalContext for MinimalContext {
        fn now_local(&self) -> DateTime2 {
            DateTime2 {
                date: Date { days: 0 },
                time: Time { ticks_100ns: 0 },
            }
        }

        fn now_utc(&self) -> DateTime2 {
            self.now_local()
        }

        fn rowcount(&self) -> i64 {
            0
        }

        fn last_identity(&self) -> Option<Decimal> {
            None
        }

        fn spid(&self) -> i16 {
            0
        }

        fn current_database(&self) -> &str {
            "master"
        }

        fn server_name(&self) -> &str {
            "VAUBAN"
        }

        fn object_id(&self, _name: &str) -> Option<i32> {
            None
        }

        fn object_name(&self, _id: i32) -> Option<String> {
            None
        }

        fn variable(&self, name: &str) -> Option<Value> {
            match name {
                "@@ERROR" => Some(Value::I32(0)),
                _ => None,
            }
        }
    }

    #[test]
    fn variable_is_returned_by_value() {
        let ctx: &dyn EvalContext = &MinimalContext;
        // The caller owns the value: the context computed it and kept nothing.
        let value: Option<Value> = ctx.variable("@@ERROR");
        assert_eq!(value, Some(Value::I32(0)));
        assert_eq!(ctx.variable("@@TRANCOUNT"), None);
    }

    #[test]
    fn eval_context_defaults() {
        let ctx: &dyn EvalContext = &MinimalContext;
        assert_eq!(ctx.host_name(), None);
        assert_eq!(ctx.app_name(), None);
        assert_eq!(ctx.login_name(), None);
        assert_eq!(ctx.user_name(), None);
        assert_eq!(ctx.database_id(None), None);
        assert_eq!(ctx.database_id(Some("master")), None);
        assert_eq!(ctx.database_name(1), None);
        assert_eq!(ctx.schema_name(1), None);
        assert_eq!(ctx.schema_id(None), None);
        assert_eq!(ctx.schema_id(Some("dbo")), None);
        assert_eq!(ctx.scope_identity(), None);
        assert_eq!(ctx.ident_current("dbo.t"), None);
        assert_eq!(ctx.datefirst(), 7);
        assert_eq!(ctx.language(), "us_english");
    }

    #[test]
    fn static_context_new_accessors() {
        let ctx = StaticContext {
            host_name: Some("WORKSTATION".to_owned()),
            app_name: Some("sqlcmd".to_owned()),
            login_name: Some("sa".to_owned()),
            user_name: Some("dbo".to_owned()),
            datefirst: 1,
            ..StaticContext::default()
        };
        let dyn_ctx: &dyn EvalContext = &ctx;
        assert_eq!(dyn_ctx.host_name(), Some("WORKSTATION"));
        assert_eq!(dyn_ctx.app_name(), Some("sqlcmd"));
        assert_eq!(dyn_ctx.login_name(), Some("sa"));
        assert_eq!(dyn_ctx.user_name(), Some("dbo"));
        assert_eq!(dyn_ctx.datefirst(), 1);

        let empty = StaticContext::default();
        let dyn_empty: &dyn EvalContext = &empty;
        assert_eq!(dyn_empty.host_name(), None);
        assert_eq!(dyn_empty.app_name(), None);
        assert_eq!(dyn_empty.login_name(), None);
        assert_eq!(dyn_empty.user_name(), None);
        assert_eq!(dyn_empty.datefirst(), 7);
        // Not backed by a field: the trait defaults answer.
        assert_eq!(dyn_empty.language(), "us_english");
        assert_eq!(dyn_empty.scope_identity(), None);
    }

    /// The three transaction methods answer the value of a session outside a transaction
    /// on a context that overrides nothing, and on `StaticContext::default()`.
    ///
    /// The three values are those of SQL Server outside a transaction and without `SET
    /// LOCK_TIMEOUT`: `XACT_STATE()` 0, `@@LOCK_TIMEOUT` -1, `@@TRANCOUNT` 0. `-1` is the
    /// one that is not a zero: a default of 0 would mean "give up as soon as a lock is
    /// held", the opposite of the actual default.
    #[test]
    fn defaults_are_neutral() {
        let minimal: &dyn EvalContext = &MinimalContext;
        assert_eq!(minimal.xact_state(), 0);
        assert_eq!(minimal.lock_timeout(), -1);
        assert_eq!(minimal.trancount(), 0);

        let fallback = StaticContext::default();
        let static_ctx: &dyn EvalContext = &fallback;
        assert_eq!(static_ctx.xact_state(), 0);
        assert_eq!(static_ctx.lock_timeout(), -1);
        assert_eq!(static_ctx.trancount(), 0);
    }

    /// The three fields are read back, each with a value the defaults do not hold: a
    /// context built with them answers them and not the `0 / -1 / 0` above.
    #[test]
    fn static_context_carries_the_transaction_state() {
        let ctx = StaticContext {
            xact_state: -1,
            lock_timeout: 5_000,
            trancount: 2,
            ..StaticContext::default()
        };
        let dyn_ctx: &dyn EvalContext = &ctx;
        assert_eq!(dyn_ctx.xact_state(), -1);
        assert_eq!(dyn_ctx.lock_timeout(), 5_000);
        assert_eq!(dyn_ctx.trancount(), 2);
    }
}
