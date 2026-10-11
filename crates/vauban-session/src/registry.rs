//! Live session registry: the sessions of the engine, held in memory by its catalogue
//! ([`vauban_catalog::LiveSessions`]) and read by the `sys.dm_exec_*` views. Keeping it up to
//! date writes nothing to storage (`tests/registry.rs`,
//! `a_thousand_select_one_append_nothing_to_the_journal`); the identifier of the current
//! database is read through a read-only transaction of its own at login and at each request.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vauban_catalog::{LiveConnection, LiveSession};
use vauban_errors::SqlResult;
use vauban_txn::IsolationLevel;
use vauban_types::{
    Date, DateTime, DateTime2, SqlType, Time, TypeInfo, Value, calendar::days_from_civil, convert,
};

use crate::server::Engine;
use crate::state::SessionState;

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
    /// Empties the registry (`clear_leaves_the_registry_empty`). Called from
    /// [`Engine::new`].
    pub(crate) fn clear_session_registry(&self) -> SqlResult<()> {
        self.catalog.live_sessions().clear();
        Ok(())
    }

    /// Registers the session and its connection after a successful login.
    pub(crate) fn register_session(
        &self,
        state: &SessionState,
        connection: &ConnectionInfo,
    ) -> SqlResult<()> {
        let database_id = database_id_of(self, &state.database)?;
        let now = now_datetime();
        self.catalog.live_sessions().register(
            LiveSession {
                session_id: state.spid,
                login_name: state.login.clone(),
                host_name: state.hostname.clone(),
                program_name: state.app_name.clone(),
                database_name: state.database.clone(),
                database_id,
                login_time: now,
                last_request_time: now,
                status: SessionStatus::Sleeping.as_str(),
                last_sql_text_len: 0,
            },
            LiveConnection {
                client_net_address: connection.client_address.clone(),
                client_tcp_port: i32::from(connection.client_port),
                protocol_version: i32::try_from(connection.tds_version).unwrap_or(i32::MAX),
                encrypt_option: connection.encrypt_option.clone(),
            },
        );
        Ok(())
    }

    /// Updates the entry of the session at the start and at the end of each batch or RPC.
    ///
    /// The identifier of the current database is looked up at each call, so a database
    /// dropped and created again under the same name is followed
    /// (`use_updates_database_id_in_the_registry`,
    /// `a_database_created_again_under_its_name_gets_its_new_id`).
    pub(crate) fn touch_session(
        &self,
        spid: i16,
        sql_text_len: i32,
        status: SessionStatus,
        database: &str,
    ) -> SqlResult<()> {
        let live = self.catalog.live_sessions();
        if live.get(spid).is_none() {
            return Ok(());
        }
        let database_id = database_id_of(self, database)?;
        let now = now_datetime();
        live.update(spid, |session| {
            session.database_name = database.to_owned();
            session.database_id = database_id;
            session.last_request_time = now;
            session.status = status.as_str();
            session.last_sql_text_len = sql_text_len;
        });
        Ok(())
    }

    /// Removes the session when the connection ends.
    pub(crate) fn unregister_session(&self, spid: i16) -> SqlResult<()> {
        self.catalog.live_sessions().unregister(spid);
        Ok(())
    }
}

/// The identifier of `database`, `0` when no database has that name.
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

fn now_datetime() -> DateTime {
    system_time_to_datetime(SystemTime::now())
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
        engine.catalog.live_sessions().len()
    }

    #[test]
    fn clear_leaves_the_registry_empty() {
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

    #[test]
    fn a_running_session_is_a_request_and_a_sleeping_one_is_not() {
        let engine = engine();
        let mut s = session(&engine, 84);
        register(&engine, s.state());
        let count = "SELECT COUNT(*) FROM sys.dm_exec_requests WHERE session_id = 84";
        assert_eq!(one_int(&run(&mut s, count)), 0);
        engine
            .touch_session(84, 9, SessionStatus::Running, "master")
            .expect("touch");
        assert_eq!(one_int(&run(&mut s, count)), 1);
        assert_eq!(
            one_int(&run(
                &mut s,
                "SELECT client_tcp_port FROM sys.dm_exec_connections WHERE session_id = 84",
            )),
            55100
        );
    }

    #[test]
    fn a_database_created_again_under_its_name_gets_its_new_id() {
        let engine = engine();
        let mut s = session(&engine, 85);
        register(&engine, s.state());
        run(&mut s, "CREATE DATABASE RevDb");
        run(&mut s, "USE RevDb");
        engine
            .touch_session(85, 0, SessionStatus::Sleeping, s.state().database.as_str())
            .expect("touch after use");
        run(
            &mut s,
            "USE master; DROP DATABASE RevDb; CREATE DATABASE RevDb; USE RevDb;",
        );
        engine
            .touch_session(85, 0, SessionStatus::Sleeping, s.state().database.as_str())
            .expect("touch after the batch");
        let mut sink = Recording::default();
        s.run_batch(
            "SELECT DB_ID(), (SELECT database_id FROM sys.dm_exec_sessions WHERE session_id = @@SPID)",
            &mut sink,
        )
        .expect("batch");
        let row = sink
            .0
            .iter()
            .find_map(|event| match event {
                Event::Row(row) => Some(row.clone()),
                Event::Error(_) => None,
            })
            .expect("one row");
        assert_eq!(row.len(), 2);
        assert_eq!(
            row[0], row[1],
            "registry id {:?} vs DB_ID() {:?}",
            row[1], row[0]
        );
    }
}
