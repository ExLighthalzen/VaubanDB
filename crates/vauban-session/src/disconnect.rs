//! Connection teardown and ATTENTION cleanup: one path to roll back, release locks, and
//! leave the session in a consistent state.

use std::sync::Arc;

use vauban_errors::SqlResult;
use vauban_storage::TxnId;

use crate::batch::Session;
use crate::txn_session;

/// Why [`release`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    /// The connection is ending: roll back open transactions and give locks back.
    Disconnect,
    /// An ATTENTION arrived: cancel the running instruction, keep the session transaction.
    Attention,
}

/// The single release path for a connection that leaves or is interrupted.
///
/// [`Release::Disconnect`] rolls back the session transaction and the autocommit
/// transaction of the statement in flight when one exists, then calls
/// [`vauban_txn::LockManager::release_all`] for each collected identifier
/// (`release_is_idempotent`, `disconnect_releases_the_locks`). A second invocation still
/// returns `Ok(())`.
///
/// [`Release::Attention`] does not close the session transaction: the cancellation flag
/// raised by the connection task already stopped the running instruction.
pub(crate) fn release(session: &mut Session, cause: Release) -> SqlResult<()> {
    match cause {
        Release::Disconnect => release_on_disconnect(session),
        Release::Attention => Ok(()),
    }
}

fn release_on_disconnect(session: &mut Session) -> SqlResult<()> {
    let engine = Arc::clone(session.engine());
    let spid = session.state().spid;
    engine.unregister_session(spid)?;
    let mut ids = Vec::new();
    if let Some(txn) = session.state().txn.as_ref() {
        ids.push(txn.handle.id);
    }
    if let Some(handle) = session.exec().txn.as_ref()
        && !ids.contains(&handle.id)
    {
        ids.push(handle.id);
    }
    session.exec_mut().txn = None;
    txn_session::rollback_all(session.state_mut(), &engine)?;
    for id in ids {
        release_locks(&engine, id);
    }
    Ok(())
}

fn release_locks(engine: &crate::Engine, id: TxnId) {
    engine.txn.locks().release_all(id);
}

#[cfg(test)]
mod tests;
