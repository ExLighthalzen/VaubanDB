//! Connection reset on RESETCONNECTION ([MS-TDS] 2.2.3.1.2).

use std::sync::Arc;

use vauban_errors::SqlResult;
use vauban_tds::{EnvChange, ResetConnection};

use crate::Engine;
use crate::batch::Session;
use crate::prepared::PreparedStatements;
use crate::set_options::{SetOptions, default_isolation};
use crate::sink::ResultSink;
use crate::state::SessionState;
use crate::txn_session;

/// Runs a client reset before the batch or RPC that requested it.
#[doc(hidden)]
pub fn apply_reset(
    session: &mut Session,
    engine: &Arc<Engine>,
    login_database: &str,
    mode: ResetConnection,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    if mode == ResetConnection::None {
        return Ok(());
    }
    session.exec_mut().variables.clear();
    session.exec_mut().variable_types.clear();
    session.exec_mut().txn = None;
    session
        .state_mut()
        .reset_to_login(engine, login_database, mode, sink)?;
    sync_exec_after_reset(session);
    Ok(())
}

fn sync_exec_after_reset(session: &mut Session) {
    let exec_txn = session
        .state()
        .txn
        .as_ref()
        .map(|txn| (txn.handle.clone(), txn.depth));
    match exec_txn {
        Some((handle, depth)) => {
            session.exec_mut().txn = Some(handle);
            session.exec_mut().trancount = depth;
        }
        None => {
            session.exec_mut().txn = None;
            session.exec_mut().trancount = 0;
        }
    }
}

impl SessionState {
    /// Returns the session to its post-login shape before the pending message runs.
    pub(crate) fn reset_to_login(
        &mut self,
        engine: &Engine,
        login_database: &str,
        mode: ResetConnection,
        sink: &mut dyn ResultSink,
    ) -> SqlResult<()> {
        if mode == ResetConnection::Full {
            rollback_for_reset(self, engine)?;
        }

        if self.database != login_database {
            let old = std::mem::replace(&mut self.database, login_database.to_owned());
            sink.env_change(&EnvChange::Database {
                old,
                new: login_database.to_owned(),
            })?;
        }

        self.options = SetOptions::default();
        self.isolation = default_isolation();
        self.rowcount = 0;
        self.last_error = 0;
        self.last_identity = None;
        self.identity_insert = None;
        self.prepared = PreparedStatements::new();

        if mode == ResetConnection::Full {
            self.trancount = 0;
            self.transaction_descriptor = 0;
        } else if let Some(txn) = &self.txn {
            self.trancount = txn.depth;
            self.transaction_descriptor = txn.descriptor;
        }

        Ok(())
    }
}

fn rollback_for_reset(state: &mut SessionState, engine: &Engine) -> SqlResult<()> {
    let id = state.txn.as_ref().map(|txn| txn.handle.id);
    txn_session::rollback_all(state, engine)?;
    if let Some(id) = id {
        engine.txn.locks().release_all(id);
    }
    Ok(())
}
