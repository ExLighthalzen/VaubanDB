use super::*;
use std::sync::Arc;

use vauban_storage::{MemoryStorage, Storage};
use vauban_txn::TransactionManager;

use crate::bootstrap::DATABASES_TABLE;

fn bootstrapped() -> Catalog {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    Catalog::bootstrap(storage, txn).expect("bootstrap")
}

fn table(catalog: &Catalog, name: &str) -> TableId {
    internal_table_id(catalog, name)
        .expect("registry")
        .expect("the table is registered")
}

fn when(days: i32) -> DateTime {
    DateTime {
        days,
        ticks_300th: 0,
    }
}

fn session(spid: i16) -> LiveSession {
    LiveSession {
        session_id: spid,
        login_name: "sa".into(),
        host_name: "host".into(),
        program_name: "app".into(),
        database_name: "master".into(),
        database_id: 1,
        login_time: when(10),
        last_request_time: when(10),
        status: "sleeping",
        last_sql_text_len: 0,
    }
}

fn connection(port: i32) -> LiveConnection {
    LiveConnection {
        client_net_address: "127.0.0.1".into(),
        client_tcp_port: port,
        protocol_version: 0x7400_0004,
        encrypt_option: "FALSE".into(),
    }
}

#[test]
fn rows_follow_the_map() {
    let catalog = bootstrapped();
    let sessions = table(&catalog, SESSIONS_TABLE);
    let connections = table(&catalog, CONNECTIONS_TABLE);
    assert_eq!(catalog.live_rows(sessions), Some(Vec::new()));

    catalog
        .live_sessions()
        .register(session(52), connection(50_000));
    catalog
        .live_sessions()
        .register(session(51), connection(50_001));
    catalog.live_sessions().update(52, |s| {
        s.status = "running";
        s.last_sql_text_len = 8;
        s.last_request_time = when(11);
    });

    let rows = catalog.live_rows(sessions).expect("sessions are live");
    assert_eq!(rows.len(), 2);
    let row = &rows[1].0;
    assert_eq!(row[sessions_columns::SESSION_ID], Value::I16(52));
    assert_eq!(row[sessions_columns::STATUS], text("running"));
    assert_eq!(row[sessions_columns::LAST_SQL_TEXT_LEN], Value::I32(8));
    assert_eq!(
        row[sessions_columns::LAST_REQUEST_TIME],
        Value::DateTime(when(11))
    );
    assert_eq!(row[sessions_columns::LOGIN_TIME], Value::DateTime(when(10)));
    assert_eq!(rows[0].0[sessions_columns::STATUS], text("sleeping"));

    let rows = catalog
        .live_rows(connections)
        .expect("connections are live");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0[connections_columns::SESSION_ID], Value::I16(51));
    assert_eq!(
        rows[0].0[connections_columns::CLIENT_TCP_PORT],
        Value::I32(50_001)
    );
    assert_eq!(rows[0].0[connections_columns::NET_TRANSPORT], text("TCP"));

    catalog.live_sessions().unregister(51);
    assert_eq!(catalog.live_rows(sessions).map(|rows| rows.len()), Some(1));
    assert_eq!(
        catalog.live_rows(connections).map(|rows| rows.len()),
        Some(1)
    );
}

#[test]
fn another_table_is_read_from_storage() {
    let catalog = bootstrapped();
    let databases = table(&catalog, DATABASES_TABLE);
    assert_eq!(catalog.live_rows(databases), None);
}

#[test]
fn an_update_of_an_unknown_spid_changes_nothing() {
    let catalog = bootstrapped();
    catalog.live_sessions().register(session(60), connection(1));
    catalog.live_sessions().update(61, |s| s.status = "running");
    assert_eq!(catalog.live_sessions().get(60), Some(session(60)));
    assert_eq!(catalog.live_sessions().get(61), None);
}

#[test]
fn clear_empties_the_map() {
    let catalog = bootstrapped();
    catalog.live_sessions().register(session(70), connection(1));
    assert_eq!(catalog.live_sessions().len(), 1);
    catalog.live_sessions().clear();
    assert!(catalog.live_sessions().is_empty());
}
