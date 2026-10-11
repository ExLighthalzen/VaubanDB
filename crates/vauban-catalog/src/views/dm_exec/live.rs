//! The live sessions of the instance, held in memory and served as the rows of
//! [`SESSIONS_TABLE`] and [`CONNECTIONS_TABLE`].
//!
//! The two internal tables keep their place in the catalogue: they carry the names and the
//! types of the columns the `sys.dm_exec_*` views select, so the binder resolves them as it
//! resolves any table. Their rows, however, do not come from `storage`. A scan of either
//! table asks [`Catalog::live_rows`](crate::Catalog::live_rows) first, which answers the
//! rows of [`LiveSessions`] as they are at that moment; the storage table stays empty. A
//! session that logs in, runs a request or leaves changes this map under a mutex
//! (unit test `views::dm_exec::live::tests::rows_follow_the_map`); the map itself is not
//! stored.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use vauban_storage::{Row, TableId};
use vauban_types::{DateTime, SqlString, Value};

use crate::Catalog;
use crate::bootstrap::{
    CONNECTIONS_TABLE, SESSIONS_TABLE, connections_columns, internal_table_id, sessions_columns,
};

/// What [`SESSIONS_TABLE`] publishes about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// `session_id`: the SPID of the connection.
    pub session_id: i16,
    /// `login_name`.
    pub login_name: String,
    /// `host_name`: the workstation the client named in its login.
    pub host_name: String,
    /// `program_name`: the application the client named in its login.
    pub program_name: String,
    /// The name of the current database, kept so that a caller can tell whether
    /// `database_id` needs a new lookup. Not published.
    pub database_name: String,
    /// `database_id`: the identifier of the current database.
    pub database_id: i32,
    /// `login_time`.
    pub login_time: DateTime,
    /// `last_request_time`: the start or the end of the latest request.
    pub last_request_time: DateTime,
    /// `status`: `running` while a request executes, `sleeping` between requests.
    pub status: &'static str,
    /// `last_sql_text_len`: the length of the text of the latest batch or RPC.
    pub last_sql_text_len: i32,
}

/// What [`CONNECTIONS_TABLE`] publishes about the connection of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConnection {
    /// `client_net_address`.
    pub client_net_address: String,
    /// `client_tcp_port`.
    pub client_tcp_port: i32,
    /// `protocol_version`: the TDS version the client announced.
    pub protocol_version: i32,
    /// `encrypt_option`: `TRUE` or `FALSE`.
    pub encrypt_option: String,
}

/// The sessions currently connected, keyed by SPID.
///
/// Shared by the sessions of the engine through the [`Catalog`] they hold.
#[derive(Debug, Default)]
pub struct LiveSessions {
    entries: Mutex<BTreeMap<i16, (LiveSession, LiveConnection)>>,
    /// The storage identifiers of [`SESSIONS_TABLE`] and [`CONNECTIONS_TABLE`], found on
    /// the first scan that asks.
    tables: OnceLock<(TableId, TableId)>,
}

impl LiveSessions {
    /// The map, recovered from a poisoned mutex: a panic in another session leaves the
    /// entries whole, each change being a single map operation.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<i16, (LiveSession, LiveConnection)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `session` and its `connection`, replacing an entry of the same SPID.
    pub fn register(&self, session: LiveSession, connection: LiveConnection) {
        self.lock()
            .insert(session.session_id, (session, connection));
    }

    /// Changes the entry of `spid` through `change`; does nothing when `spid` is not
    /// registered.
    pub fn update(&self, spid: i16, change: impl FnOnce(&mut LiveSession)) {
        if let Some((session, _)) = self.lock().get_mut(&spid) {
            change(session);
        }
    }

    /// The entry of `spid`, `None` when it is not registered.
    pub fn get(&self, spid: i16) -> Option<LiveSession> {
        self.lock().get(&spid).map(|(session, _)| session.clone())
    }

    /// Removes the entry of `spid`.
    pub fn unregister(&self, spid: i16) {
        self.lock().remove(&spid);
    }

    /// Removes every entry (`views::dm_exec::live::tests::clear_empties_the_map`).
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// The number of registered sessions.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no session is registered.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn session_rows(&self) -> Vec<Row> {
        self.lock()
            .values()
            .map(|(session, _)| session_row(session))
            .collect()
    }

    fn connection_rows(&self) -> Vec<Row> {
        self.lock()
            .iter()
            .map(|(spid, (_, connection))| connection_row(*spid, connection))
            .collect()
    }
}

impl Catalog {
    /// The live sessions of the engine.
    pub fn live_sessions(&self) -> &LiveSessions {
        &self.live
    }

    /// The rows a scan of `table` reads instead of `storage`: those of the live sessions
    /// for [`SESSIONS_TABLE`] and [`CONNECTIONS_TABLE`], `None` for any other table.
    pub fn live_rows(&self, table: TableId) -> Option<Vec<Row>> {
        let (sessions, connections) = self.live_tables()?;
        if table == sessions {
            Some(self.live.session_rows())
        } else if table == connections {
            Some(self.live.connection_rows())
        } else {
            None
        }
    }

    fn live_tables(&self) -> Option<(TableId, TableId)> {
        if let Some(found) = self.live.tables.get() {
            return Some(*found);
        }
        let sessions = internal_table_id(self, SESSIONS_TABLE).ok()??;
        let connections = internal_table_id(self, CONNECTIONS_TABLE).ok()??;
        Some(*self.live.tables.get_or_init(|| (sessions, connections)))
    }
}

fn session_row(session: &LiveSession) -> Row {
    let mut row = vec![Value::Null; sessions_columns::WIDTH];
    row[sessions_columns::SESSION_ID] = Value::I16(session.session_id);
    row[sessions_columns::LOGIN_NAME] = text(&session.login_name);
    row[sessions_columns::HOST_NAME] = text(&session.host_name);
    row[sessions_columns::PROGRAM_NAME] = text(&session.program_name);
    row[sessions_columns::DATABASE_ID] = Value::I32(session.database_id);
    row[sessions_columns::LOGIN_TIME] = Value::DateTime(session.login_time);
    row[sessions_columns::LAST_REQUEST_TIME] = Value::DateTime(session.last_request_time);
    row[sessions_columns::STATUS] = text(session.status);
    row[sessions_columns::LAST_SQL_TEXT_LEN] = Value::I32(session.last_sql_text_len);
    Row(row)
}

fn connection_row(spid: i16, connection: &LiveConnection) -> Row {
    let mut row = vec![Value::Null; connections_columns::WIDTH];
    row[connections_columns::SESSION_ID] = Value::I16(spid);
    row[connections_columns::CLIENT_NET_ADDRESS] = text(&connection.client_net_address);
    row[connections_columns::CLIENT_TCP_PORT] = Value::I32(connection.client_tcp_port);
    row[connections_columns::PROTOCOL_VERSION] = Value::I32(connection.protocol_version);
    row[connections_columns::ENCRYPT_OPTION] = text(&connection.encrypt_option);
    row[connections_columns::NET_TRANSPORT] = text("TCP");
    row[connections_columns::PROTOCOL_TYPE] = text("TSQL");
    row[connections_columns::AUTH_SCHEME] = text("SQL");
    Row(row)
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

#[cfg(test)]
mod tests;
