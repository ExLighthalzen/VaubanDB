//! Live session registry: rows in `master.dbo.vauban_sys_sessions` and
//! `master.dbo.vauban_sys_connections`, written outside the session transaction.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{Row, RowId, TableId};
use vauban_txn::IsolationLevel;
use vauban_types::{
    Date, DateTime, DateTime2, SqlString, SqlType, Time, TypeInfo, Value,
    calendar::days_from_civil, convert,
};

use crate::server::Engine;
use crate::state::SessionState;

/// Names of the internal tables; column order matches `bootstrap.rs`.
const SESSIONS_TABLE: &str = "vauban_sys_sessions";
const CONNECTIONS_TABLE: &str = "vauban_sys_connections";

mod sessions_columns {
    pub(super) const SESSION_ID: usize = 0;
    pub(super) const LOGIN_NAME: usize = 1;
    pub(super) const HOST_NAME: usize = 2;
    pub(super) const PROGRAM_NAME: usize = 3;
    pub(super) const DATABASE_ID: usize = 4;
    pub(super) const LOGIN_TIME: usize = 5;
    pub(super) const LAST_REQUEST_TIME: usize = 6;
    pub(super) const STATUS: usize = 7;
    pub(super) const LAST_SQL_TEXT_LEN: usize = 8;
    pub(super) const WIDTH: usize = 9;
}

mod connections_columns {
    pub(super) const SESSION_ID: usize = 0;
    pub(super) const CLIENT_NET_ADDRESS: usize = 1;
    pub(super) const CLIENT_TCP_PORT: usize = 2;
    pub(super) const PROTOCOL_VERSION: usize = 3;
    pub(super) const ENCRYPT_OPTION: usize = 4;
    pub(super) const NET_TRANSPORT: usize = 5;
    pub(super) const PROTOCOL_TYPE: usize = 6;
    pub(super) const AUTH_SCHEME: usize = 7;
    pub(super) const WIDTH: usize = 8;
}

/// What the client negotiated at login, kept for the connection row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectionInfo {
    /// Dotted or bracketed client address.
    pub client_address: String,
    /// Client TCP port, `0` when unknown.
    pub client_port: u16,
    /// TDS version from the LOGIN7.
    pub tds_version: u32,
    /// Text carried in `encrypt_option`, `TRUE` or `FALSE`.
    pub encrypt_option: String,
}

/// Whether a session is executing a batch or waiting for the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStatus {
    /// A batch or RPC is running on the blocking pool.
    Running,
    /// The connection is idle between requests.
    Sleeping,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Sleeping => "sleeping",
        }
    }
}

impl Engine {
    /// Clears the session tables (`clear_leaves_the_session_tables_empty`). Called from
    /// [`Engine::new`].
    pub(crate) fn clear_session_registry(&self) -> SqlResult<()> {
        with_registry_txn(self, |engine, txn| {
            clear_table(engine, txn, SESSIONS_TABLE)?;
            clear_table(engine, txn, CONNECTIONS_TABLE)?;
            Ok(())
        })
    }

    /// Inserts the session and connection rows after a successful login.
    pub(crate) fn register_session(
        &self,
        state: &SessionState,
        connection: &ConnectionInfo,
    ) -> SqlResult<()> {
        let database_id = database_id_of(self, &state.database)?;
        let now = now_datetime();
        with_registry_txn(self, |engine, txn| {
            let sessions = table_of(engine, txn, SESSIONS_TABLE)?;
            let mut row = vec![Value::Null; sessions_columns::WIDTH];
            row[sessions_columns::SESSION_ID] = Value::I16(state.spid);
            row[sessions_columns::LOGIN_NAME] = text(&state.login);
            row[sessions_columns::HOST_NAME] = text(&state.hostname);
            row[sessions_columns::PROGRAM_NAME] = text(&state.app_name);
            row[sessions_columns::DATABASE_ID] = Value::I32(database_id);
            row[sessions_columns::LOGIN_TIME] = now.clone();
            row[sessions_columns::LAST_REQUEST_TIME] = now;
            row[sessions_columns::STATUS] = text(SessionStatus::Sleeping.as_str());
            row[sessions_columns::LAST_SQL_TEXT_LEN] = Value::I32(0);
            engine.storage.insert(txn.id, sessions, &Row(row))?;

            let connections = table_of(engine, txn, CONNECTIONS_TABLE)?;
            let mut row = vec![Value::Null; connections_columns::WIDTH];
            row[connections_columns::SESSION_ID] = Value::I16(state.spid);
            row[connections_columns::CLIENT_NET_ADDRESS] = text(&connection.client_address);
            row[connections_columns::CLIENT_TCP_PORT] =
                Value::I32(i32::from(connection.client_port));
            row[connections_columns::PROTOCOL_VERSION] =
                Value::I32(i32::try_from(connection.tds_version).unwrap_or(i32::MAX));
            row[connections_columns::ENCRYPT_OPTION] = text(&connection.encrypt_option);
            row[connections_columns::NET_TRANSPORT] = text("TCP");
            row[connections_columns::PROTOCOL_TYPE] = text("TSQL");
            row[connections_columns::AUTH_SCHEME] = text("SQL");
            engine.storage.insert(txn.id, connections, &Row(row))?;
            Ok(())
        })
    }

    /// Updates the session row after each batch or RPC.
    pub(crate) fn touch_session(
        &self,
        spid: i16,
        sql_text_len: i32,
        status: SessionStatus,
        database: &str,
    ) -> SqlResult<()> {
        let database_id = database_id_of(self, database)?;
        let now = now_datetime();
        with_registry_txn(self, |engine, txn| {
            let table = table_of(engine, txn, SESSIONS_TABLE)?;
            let Some((id, mut row)) = row_for_spid(engine, txn, table, spid)? else {
                return Ok(());
            };
            row[sessions_columns::DATABASE_ID] = Value::I32(database_id);
            row[sessions_columns::LAST_REQUEST_TIME] = now;
            row[sessions_columns::STATUS] = text(status.as_str());
            row[sessions_columns::LAST_SQL_TEXT_LEN] = Value::I32(sql_text_len);
            engine.storage.update(txn.id, table, id, &Row(row))?;
            Ok(())
        })
    }

    /// Deletes the session and connection rows when the connection ends.
    pub(crate) fn unregister_session(&self, spid: i16) -> SqlResult<()> {
        with_registry_txn(self, |engine, txn| {
            delete_for_spid(engine, txn, SESSIONS_TABLE, spid)?;
            delete_for_spid(engine, txn, CONNECTIONS_TABLE, spid)?;
            Ok(())
        })
    }
}

fn with_registry_txn<F>(engine: &Engine, work: F) -> SqlResult<()>
where
    F: FnOnce(&Engine, &vauban_txn::TxnHandle) -> SqlResult<()>,
{
    let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
    match work(engine, &handle) {
        Ok(()) => engine.txn.commit(handle),
        Err(err) => {
            engine.txn.rollback(handle).ok();
            Err(err)
        }
    }
}

fn table_of(engine: &Engine, txn: &vauban_txn::TxnHandle, name: &str) -> SqlResult<TableId> {
    let snapshot = engine.catalog.snapshot(txn);
    let object = snapshot
        .resolve_object("master", Some("dbo"), name, "dbo")
        .ok_or_else(|| {
            SqlError::from(InternalError::Bug(format!(
                "session registry: internal table {name} is not in this catalogue"
            )))
        })?;
    let table = snapshot.table(object.id).ok_or_else(|| {
        SqlError::from(InternalError::Bug(format!(
            "session registry: {name} is not a table in this catalogue"
        )))
    })?;
    Ok(table.storage_id)
}

fn database_id_of(engine: &Engine, database: &str) -> SqlResult<i32> {
    let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
    let id = engine
        .catalog
        .snapshot(&handle)
        .database(database)
        .map(|meta| i32::try_from(meta.id.0).unwrap_or(i32::MAX))
        .unwrap_or(0);
    engine.txn.commit(handle)?;
    Ok(id)
}

fn row_for_spid(
    engine: &Engine,
    txn: &vauban_txn::TxnHandle,
    table: TableId,
    spid: i16,
) -> SqlResult<Option<(RowId, Vec<Value>)>> {
    let snapshot = engine.txn.statement_snapshot(txn);
    for row in engine.storage.scan(&snapshot, table)? {
        let (id, values) = row?;
        if values.0[sessions_columns::SESSION_ID] == Value::I16(spid) {
            return Ok(Some((id, values.0)));
        }
    }
    Ok(None)
}

fn delete_for_spid(
    engine: &Engine,
    txn: &vauban_txn::TxnHandle,
    table_name: &str,
    spid: i16,
) -> SqlResult<()> {
    let table = table_of(engine, txn, table_name)?;
    let col = if table_name == SESSIONS_TABLE {
        sessions_columns::SESSION_ID
    } else {
        connections_columns::SESSION_ID
    };
    let snapshot = engine.txn.statement_snapshot(txn);
    let mut to_delete = Vec::new();
    for row in engine.storage.scan(&snapshot, table)? {
        let (id, values) = row?;
        if values.0[col] == Value::I16(spid) {
            to_delete.push(id);
        }
    }
    for id in to_delete {
        engine.storage.delete(txn.id, table, id)?;
    }
    Ok(())
}

fn clear_table(engine: &Engine, txn: &vauban_txn::TxnHandle, table_name: &str) -> SqlResult<()> {
    let table = table_of(engine, txn, table_name)?;
    let snapshot = engine.txn.statement_snapshot(txn);
    let ids: Vec<RowId> = engine
        .storage
        .scan(&snapshot, table)?
        .map(|row| row.map(|(id, _)| id))
        .collect::<SqlResult<_>>()?;
    for id in ids {
        engine.storage.delete(txn.id, table, id)?;
    }
    Ok(())
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn now_datetime() -> Value {
    Value::DateTime(system_time_to_datetime(SystemTime::now()))
}

fn system_time_to_datetime(time: SystemTime) -> DateTime {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_PER_DAY: u64 = 86_400;
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let ticks_100ns = elapsed
        .as_secs()
        .saturating_mul(TICKS_PER_SECOND)
        .saturating_add(u64::from(elapsed.subsec_nanos()) / 100);
    let dt2 = DateTime2 {
        date: Date {
            days: days_from_civil(1970, 1, 1)
                .saturating_add((elapsed.as_secs() / SECONDS_PER_DAY) as i32),
        },
        time: Time {
            ticks_100ns: (elapsed.as_secs() % SECONDS_PER_DAY) * TICKS_PER_SECOND
                + ticks_100ns % TICKS_PER_SECOND,
        },
    };
    match convert(
        &Value::DateTime2(dt2),
        &TypeInfo::new(SqlType::DateTime2(7), false),
        &TypeInfo::new(SqlType::DateTime, false),
        None,
    ) {
        Ok(Value::DateTime(dt)) => dt,
        _ => DateTime {
            days: 0,
            ticks_300th: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use vauban_errors::{InfoMessage, SqlResult};
    use vauban_storage::MemoryStorage;
    use vauban_tds::{ColumnMeta, EnvChange};
    use vauban_types::{TypeInfo, Value};

    use crate::sink::ResultSink;
    use crate::{Session, SessionState};

    fn engine() -> Arc<Engine> {
        vauban_sysfn::register_builtins();
        Arc::new(Engine::new(Arc::new(MemoryStorage::new())))
    }

    fn connection() -> ConnectionInfo {
        ConnectionInfo {
            client_address: "127.0.0.1".into(),
            client_port: 55100,
            tds_version: crate::login::TDS_VERSION_7_4,
            encrypt_option: "FALSE".into(),
        }
    }

    fn session(engine: &Arc<Engine>, spid: i16) -> Session {
        let mut state = SessionState::new(spid);
        state.login = "sa".into();
        state.app_name = "registry-test".into();
        state.hostname = "host".into();
        Session::new(Arc::clone(engine), state)
    }

    #[derive(Default)]
    struct Recording(Vec<Event>);

    #[derive(Debug, Clone, PartialEq)]
    enum Event {
        Row(Vec<Value>),
        Error(vauban_errors::SqlError),
    }

    impl ResultSink for Recording {
        fn columns(&mut self, _: &[ColumnMeta]) -> SqlResult<()> {
            Ok(())
        }
        fn row(&mut self, row: &[Value]) -> SqlResult<()> {
            self.0.push(Event::Row(row.to_vec()));
            Ok(())
        }
        fn done(&mut self, _: Option<u64>, _: bool) -> SqlResult<()> {
            Ok(())
        }
        fn info(&mut self, _: &InfoMessage) -> SqlResult<()> {
            Ok(())
        }
        fn error(&mut self, err: &vauban_errors::SqlError) -> SqlResult<()> {
            self.0.push(Event::Error(err.clone()));
            Ok(())
        }
        fn env_change(&mut self, _: &EnvChange) -> SqlResult<()> {
            Ok(())
        }
        fn return_value(&mut self, _: &str, _: &TypeInfo, _: &Value) -> SqlResult<()> {
            Ok(())
        }
        fn return_status(&mut self, _: i32) -> SqlResult<()> {
            Ok(())
        }
    }

    fn run(session: &mut Session, text: &str) -> Vec<Event> {
        let mut sink = Recording::default();
        session.run_batch(text, &mut sink).expect("batch");
        sink.0
    }

    fn one_int(events: &[Event]) -> i32 {
        for event in events {
            if let Event::Row(row) = event {
                match row.first() {
                    Some(Value::I32(n)) => return *n,
                    Some(Value::I16(n)) => return i32::from(*n),
                    _ => {}
                }
            }
        }
        panic!("no int row in {events:#?}");
    }

    fn register(engine: &Engine, state: &SessionState) {
        engine
            .register_session(state, &connection())
            .expect("register");
    }

    fn count_sessions(engine: &Engine) -> usize {
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let table = table_of(engine, &handle, SESSIONS_TABLE).expect("sessions table");
        let snapshot = engine.txn.statement_snapshot(&handle);
        let count = engine.storage.scan(&snapshot, table).expect("scan").count();
        engine.txn.commit(handle).expect("commit");
        count
    }

    #[test]
    fn clear_leaves_the_session_tables_empty() {
        let engine = engine();
        let state = session(&engine, 51).state().clone();
        register(&engine, &state);
        assert_eq!(count_sessions(&engine), 1);
        engine.clear_session_registry().expect("clear");
        assert_eq!(count_sessions(&engine), 0);
    }

    #[test]
    fn two_registered_sessions_are_visible() {
        let engine = engine();
        let mut a = session(&engine, 61);
        let b = session(&engine, 62);
        register(&engine, a.state());
        register(&engine, b.state());
        assert_eq!(
            one_int(&run(
                &mut a,
                "SELECT COUNT(*) FROM master.dbo.vauban_sys_sessions",
            )),
            2
        );
    }

    #[test]
    fn unregister_removes_one_session_row() {
        let engine = engine();
        let a = session(&engine, 71);
        let mut b = session(&engine, 72);
        register(&engine, a.state());
        register(&engine, b.state());
        drop(a);
        assert_eq!(
            one_int(&run(
                &mut b,
                "SELECT COUNT(*) FROM master.dbo.vauban_sys_sessions",
            )),
            1
        );
    }

    #[test]
    fn use_updates_database_id_in_the_registry() {
        let engine = engine();
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        engine
            .catalog
            .create_database(&handle, "RegDb2", None)
            .expect("create database");
        engine.txn.commit(handle).expect("commit");

        let mut s = session(&engine, 73);
        register(&engine, s.state());
        run(&mut s, "USE RegDb2");
        engine
            .touch_session(73, 0, SessionStatus::Sleeping, s.state().database.as_str())
            .expect("touch after use");
        let db_id = one_int(&run(
            &mut s,
            "SELECT database_id FROM master.dbo.vauban_sys_sessions WHERE session_id = 73",
        ));
        let expected = one_int(&run(&mut s, "SELECT DB_ID(N'RegDb2')"));
        assert_eq!(db_id, expected);
    }

    #[test]
    fn begin_tran_does_not_hide_the_session_from_another() {
        let engine = engine();
        let mut holder = session(&engine, 81);
        let mut reader = session(&engine, 82);
        register(&engine, holder.state());
        register(&engine, reader.state());
        run(&mut holder, "BEGIN TRAN");
        assert_eq!(
            one_int(&run(
                &mut reader,
                "SELECT COUNT(*) FROM master.dbo.vauban_sys_sessions WHERE session_id = 81",
            )),
            1
        );
    }

    #[test]
    fn attention_does_not_remove_the_session_row() {
        let engine = engine();
        let state = session(&engine, 83).state().clone();
        register(&engine, &state);
        let mut session = Session::new(Arc::clone(&engine), state);
        crate::disconnect::release(&mut session, crate::disconnect::Release::Attention)
            .expect("attention");
        assert_eq!(count_sessions(&engine), 1);
    }

    #[test]
    fn register_touch_and_unregister() {
        let engine = engine();
        let state = session(&engine, 52).state().clone();
        register(&engine, &state);
        assert_eq!(count_sessions(&engine), 1);
        engine
            .touch_session(52, 11, SessionStatus::Running, "master")
            .expect("touch");
        engine.unregister_session(52).expect("unregister");
        assert_eq!(count_sessions(&engine), 0);
    }
}
