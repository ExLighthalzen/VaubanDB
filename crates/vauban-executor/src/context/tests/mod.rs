use super::*;
use vauban_sysfn::StaticContext;

/// A scalar context answers 50000 for the things a plan reads from the engine, instead
/// of panicking or handing out a default.
#[test]
fn a_scalar_context_has_no_engine() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    assert!(ctx.storage.is_none());
    assert!(ctx.txn.is_none());
    assert!(ctx.catalog.is_none());
    assert!(ctx.snap.is_none());
    assert!(ctx.handle.is_none());
    assert!(ctx.session.is_none());
    assert!(!ctx.cancelled());
    let no_storage = ctx
        .storage()
        .err()
        .expect("a scalar context has no storage");
    assert_eq!(no_storage.number, 50000);
    assert_eq!(ctx.snapshot().unwrap_err().number, 50000);
    assert_eq!(
        ctx.catalog()
            .err()
            .expect("a scalar context has no catalogue")
            .number,
        50000
    );
    assert_eq!(ctx.handle().unwrap_err().number, 50000);
    assert_eq!(ctx.session().unwrap_err().number, 50000);
}

/// The counter-proof of the test above: once `with_engine` has been called, the two
/// accessors answer the references they were given.
#[test]
fn with_engine_fills_storage_and_snapshot() {
    use std::sync::Arc;
    use vauban_storage::{MemoryStorage, TxnId};
    use vauban_txn::IsolationLevel;

    let eval = StaticContext::default();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = TransactionManager::new(Arc::clone(&storage));
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let snap = txn.statement_snapshot(&handle);
    let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    assert!(ctx.storage().is_ok());
    assert_eq!(ctx.snapshot().expect("the snapshot is there").own, TxnId(1));
    assert!(ctx.txn.is_some());
    assert!(ctx.catalog.is_none(), "`with_engine` does not set it");
    assert!(ctx.handle.is_none(), "`with_engine` does not set it either");
    let ctx = ctx.with_handle(&handle);
    assert_eq!(ctx.handle().expect("the handle is there").id, TxnId(1));
}

/// `with_session` lends the state, and what a statement writes through the accessor
/// is what the caller reads back once the context is gone.
#[test]
fn with_session_lends_the_state() {
    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    {
        let mut ctx =
            ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
        ctx.session().expect("the session is there").rowcount = 7;
    }
    assert_eq!(session.rowcount, 7);
}

/// A raised token is read through the context, on the token itself and on a clone.
#[test]
fn a_raised_token_is_seen_through_the_context() {
    let eval = StaticContext::default();
    let token = CancelToken::new();
    let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_cancel(&token);
    assert!(!ctx.cancelled());
    token.clone().cancel();
    assert!(ctx.cancelled());
    assert!(token.is_cancelled());
}

/// The token of a scalar context cannot be raised: `cancel` on it is a no-op.
#[test]
fn a_never_token_stays_down() {
    let token = CancelToken::never();
    token.cancel();
    assert!(!token.is_cancelled());
    let eval = StaticContext::default();
    let ctx = ExecContext::scalar(&eval, SessionOptions::default());
    ctx.cancel.cancel();
    assert!(!ctx.cancelled());
    // The counter-proof: a token built with `new` does go up.
    let armed = CancelToken::new();
    armed.cancel();
    assert!(armed.is_cancelled());
}

/// The queue keeps the order of `emit_info`, and `take_infos` empties it.
#[test]
fn infos_are_queued_in_order() {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let first = InfoMessage {
        number: 1,
        severity: 0,
        state: 1,
        message: "first".to_owned(),
        line: 0,
        procedure: None,
    };
    let second = InfoMessage {
        number: 2,
        ..first.clone()
    };
    ctx.emit_info(first.clone());
    ctx.emit_info(second.clone());
    assert_eq!(ctx.take_infos(), vec![first, second]);
    assert!(ctx.take_infos().is_empty());
}
