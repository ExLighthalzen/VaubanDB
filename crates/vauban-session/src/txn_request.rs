//! TRANSACTION_MANAGER requests routed to the session transaction.

use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult, message_template};
use vauban_tds::{EnvChange, TmRequest};
use vauban_txn::IsolationLevel as TxnIsolation;

use crate::Engine;
use crate::set_options::IsolationLevel;
use crate::txn_session::SessionTxn;
use crate::{ResultSink, SessionState};

/// Whether a `Begin` opened a transaction or already ended its response.
#[derive(PartialEq, Eq)]
enum BeginOutcome {
    Opened,
    Refused,
}

/// Handles one TRANSACTION_MANAGER request and emits its complete logical response.
///
/// SQL Server also emits SESSIONSTATE after the opening ENVCHANGE when the login
/// negotiated SESSIONRECOVERY. VaubanDB does not acknowledge that feature, so this path
/// deliberately emits no SESSIONSTATE. `server.rs` supplies the `CurCmd` 0xFD
/// when it relays the DONE produced through [`ResultSink::done`].
pub(crate) fn handle(
    request: &TmRequest,
    engine: &Engine,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    match request {
        TmRequest::Begin { isolation, .. } => {
            if begin(*isolation, engine, state, sink)? == BeginOutcome::Opened {
                sink.done(None, false)?;
            }
        }
        TmRequest::Commit {
            transaction_descriptor,
            begin_next,
            ..
        } => finish(
            *transaction_descriptor,
            begin_next.as_ref().copied(),
            false,
            engine,
            state,
            sink,
        )?,
        TmRequest::Rollback {
            transaction_descriptor,
            begin_next,
            ..
        } => finish(
            *transaction_descriptor,
            begin_next.as_ref().copied(),
            true,
            engine,
            state,
            sink,
        )?,
        TmRequest::Save { .. } => refuse(3903, state, sink)?,
        TmRequest::Unsupported(request_type) => {
            tracing::warn!(request_type, "unsupported transaction manager request");
            let error: SqlError = InternalError::Bug(format!(
                "unsupported TRANSACTION_MANAGER request type {request_type}"
            ))
            .into();
            sink.error(&error)?;
            sink.done(None, false)?;
        }
    }
    Ok(())
}

/// Rejects a batch or an RPC when its ALL_HEADERS carry a transaction descriptor that
/// does not match the session transaction.
///
/// Returns `Ok(true)` when the request may proceed, `Ok(false)` after a refusal was sent.
pub(crate) fn reject_mismatched_descriptor(
    descriptor: u64,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<bool> {
    let expected = state.transaction_descriptor;
    if expected == 0 {
        return Ok(true);
    }
    if descriptor == 0 {
        refuse(3989, state, sink)?;
        return Ok(false);
    }
    if descriptor != expected {
        refuse_mismatched_batch_descriptor(descriptor, state, sink)?;
        return Ok(false);
    }
    Ok(true)
}

/// Opens a driver transaction, or refuses a second `Begin` on the same session.
fn begin(
    isolation: u8,
    engine: &Engine,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<BeginOutcome> {
    if state.txn.is_some() {
        refuse(3989, state, sink)?;
        return Ok(BeginOutcome::Refused);
    }
    open(
        engine,
        state,
        isolation_from_message(isolation, state.isolation),
        sink,
    )?;
    Ok(BeginOutcome::Opened)
}

/// Starts a tracked transaction and announces it on the wire.
fn open(
    engine: &Engine,
    state: &mut SessionState,
    level: TxnIsolation,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    let handle = engine.txn.begin(level);
    let Some(descriptor) = state.allocate_transaction_descriptor() else {
        engine.txn.rollback(handle)?;
        return Err(bug("transaction descriptor space exhausted"));
    };
    state.txn = Some(SessionTxn {
        handle,
        descriptor,
        depth: 1,
    });
    state.transaction_descriptor = descriptor;
    state.trancount = 1;
    sink.env_change(&EnvChange::BeginTransaction(descriptor))
}

/// Commits or rolls back the current tracked transaction, optionally chaining a new one.
fn finish(
    descriptor: u64,
    begin_next: Option<u8>,
    rollback: bool,
    engine: &Engine,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    if descriptor == 0 || descriptor != state.transaction_descriptor {
        return refuse(if rollback { 3903 } else { 3902 }, state, sink);
    }

    let txn = state
        .txn
        .take()
        .ok_or_else(|| bug("finish without a session transaction"))?;
    if rollback {
        engine.txn.rollback(txn.handle)?;
        sink.env_change(&EnvChange::RollbackTransaction(descriptor))?;
    } else {
        engine.txn.commit(txn.handle)?;
        sink.env_change(&EnvChange::CommitTransaction(descriptor))?;
    }
    state.transaction_descriptor = 0;
    state.trancount = 0;
    if let Some(isolation) = begin_next {
        open(
            engine,
            state,
            isolation_from_message(isolation, state.isolation),
            sink,
        )?;
    }
    sink.done(None, false)
}

/// Refuses a batch whose descriptor does not match the session transaction.
fn refuse_mismatched_batch_descriptor(
    descriptor: u64,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    sink.info(&catalogue_info(3926))?;
    let error = SqlError::failed_to_resume_transaction(descriptor);
    state.last_error = error.number;
    sink.error(&error)?;
    sink.done(None, false)
}

/// Sends the catalogue error used for an operation without a matching transaction. The
/// state is left untouched so a mismatched descriptor cannot end the current transaction.
fn refuse(number: u32, state: &mut SessionState, sink: &mut dyn ResultSink) -> SqlResult<()> {
    let error = catalogue_error(number);
    state.last_error = error.number;
    sink.error(&error)?;
    sink.done(None, false)
}

/// Builds one argument-free catalogue error with its default severity and state 1.
fn catalogue_error(number: u32) -> SqlError {
    match message_template(number) {
        Some(def) => SqlError::new(def.number, def.severity, 1, def.template),
        None => InternalError::Bug(format!("error {number} missing from the catalogue")).into(),
    }
}

/// Builds one argument-free informational message from the catalogue.
fn catalogue_info(number: u32) -> InfoMessage {
    match message_template(number) {
        Some(def) => InfoMessage {
            number: def.number,
            severity: def.severity,
            state: 1,
            message: def.template.to_string(),
            line: 0,
        },
        None => InfoMessage {
            number,
            severity: 10,
            state: 1,
            message: format!("message {number} missing from the catalogue"),
            line: 0,
        },
    }
}

/// Maps the isolation byte of a TRANSACTION_MANAGER request to a transaction level.
fn isolation_from_message(byte: u8, session: IsolationLevel) -> TxnIsolation {
    match byte {
        0 => txn_isolation(session),
        1 => TxnIsolation::ReadUncommitted,
        2 => TxnIsolation::ReadCommitted,
        3 => TxnIsolation::RepeatableRead,
        4 => TxnIsolation::Serializable,
        5 => TxnIsolation::Snapshot,
        _ => txn_isolation(session),
    }
}

/// The transaction level of a session isolation level.
fn txn_isolation(level: IsolationLevel) -> TxnIsolation {
    match level {
        IsolationLevel::ReadUncommitted => TxnIsolation::ReadUncommitted,
        IsolationLevel::ReadCommitted => TxnIsolation::ReadCommitted,
        IsolationLevel::RepeatableRead => TxnIsolation::RepeatableRead,
        IsolationLevel::Snapshot => TxnIsolation::Snapshot,
        IsolationLevel::Serializable => TxnIsolation::Serializable,
    }
}

/// The internal error 50000 for a broken precondition.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vauban_errors::{InfoMessage, SqlError};
    use vauban_storage::MemoryStorage;
    use vauban_tds::{ColumnMeta, EnvChange};
    use vauban_types::{TypeInfo, Value};

    use super::*;
    use crate::Engine;

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Done,
        Error(SqlError),
        Info(InfoMessage),
        EnvChange(EnvChange),
    }

    #[derive(Default)]
    struct Recording(Vec<Event>);

    impl ResultSink for Recording {
        fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
            unreachable!()
        }
        fn row(&mut self, _row: &[Value]) -> SqlResult<()> {
            unreachable!()
        }
        fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
            assert_eq!(rowcount, None);
            assert!(!more);
            self.0.push(Event::Done);
            Ok(())
        }
        fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
            self.0.push(Event::Info(msg.clone()));
            Ok(())
        }
        fn error(&mut self, error: &SqlError) -> SqlResult<()> {
            self.0.push(Event::Error(error.clone()));
            Ok(())
        }
        fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
            self.0.push(Event::EnvChange(change.clone()));
            Ok(())
        }
        fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
            unreachable!()
        }
        fn return_status(&mut self, _status: i32) -> SqlResult<()> {
            unreachable!()
        }
    }

    fn engine() -> Engine {
        vauban_sysfn::register_builtins();
        Engine::new(Arc::new(MemoryStorage::new()))
    }

    fn begin_request(isolation: u8) -> TmRequest {
        TmRequest::Begin {
            transaction_descriptor: 0,
            isolation,
            name: String::new(),
        }
    }

    fn begin(state: &mut SessionState, engine: &Engine) -> (u64, Recording) {
        let mut sink = Recording::default();
        handle(&begin_request(0), engine, state, &mut sink).unwrap();
        (state.transaction_descriptor, sink)
    }

    #[test]
    fn begin_answers_with_an_envchange() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (descriptor, sink) = begin(&mut state, &engine);
        assert_ne!(descriptor, 0);
        assert_eq!(state.trancount, 1);
        assert!(engine.txn.active_sessions().len() == 1);
        assert_eq!(
            sink.0,
            vec![
                Event::EnvChange(EnvChange::BeginTransaction(descriptor)),
                Event::Done,
            ]
        );
    }

    #[test]
    fn second_begin_returns_3989() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (_, _) = begin(&mut state, &engine);
        let mut sink = Recording::default();
        handle(&begin_request(0), &engine, &mut state, &mut sink).unwrap();
        assert_error(&sink, 3989);
        assert_eq!(state.trancount, 1);
    }

    #[test]
    fn descriptor_is_stable_and_unique() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (first, _) = begin(&mut state, &engine);
        let mut sink = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: first,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut sink,
        )
        .unwrap();
        let (second, _) = begin(&mut state, &engine);
        assert_ne!(first, second);
        assert_eq!(state.transaction_descriptor, second);
    }

    #[test]
    fn commit_matches_the_descriptor() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (descriptor, _) = begin(&mut state, &engine);

        let mut mismatch = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: descriptor + 1,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut mismatch,
        )
        .unwrap();
        assert_error(&mismatch, 3902);
        assert_eq!(state.transaction_descriptor, descriptor);
        assert_eq!(state.trancount, 1);

        let mut sink = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: descriptor,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut sink,
        )
        .unwrap();
        assert_eq!(state.transaction_descriptor, 0);
        assert_eq!(state.trancount, 0);
        assert_eq!(
            sink.0,
            vec![
                Event::EnvChange(EnvChange::CommitTransaction(descriptor)),
                Event::Done,
            ]
        );

        let mut sink = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: descriptor,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut sink,
        )
        .unwrap();
        assert_error(&sink, 3902);
    }

    #[test]
    fn rollback_matches_the_descriptor() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (descriptor, _) = begin(&mut state, &engine);

        let mut mismatch = Recording::default();
        handle(
            &TmRequest::Rollback {
                transaction_descriptor: descriptor + 1,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut mismatch,
        )
        .unwrap();
        assert_error(&mismatch, 3903);
        assert_eq!(state.transaction_descriptor, descriptor);
        assert_eq!(state.trancount, 1);

        let mut sink = Recording::default();
        handle(
            &TmRequest::Rollback {
                transaction_descriptor: descriptor,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut sink,
        )
        .unwrap();
        assert_eq!(state.transaction_descriptor, 0);
        assert_eq!(state.trancount, 0);
        assert_eq!(
            sink.0,
            vec![
                Event::EnvChange(EnvChange::RollbackTransaction(descriptor)),
                Event::Done,
            ]
        );

        let mut sink = Recording::default();
        handle(
            &TmRequest::Rollback {
                transaction_descriptor: descriptor,
                name: String::new(),
                begin_next: None,
            },
            &engine,
            &mut state,
            &mut sink,
        )
        .unwrap();
        assert_error(&sink, 3903);
    }

    #[test]
    fn commit_with_begin_next_chains() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (first, _) = begin(&mut state, &engine);
        let mut sink = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: first,
                name: String::new(),
                begin_next: Some(0),
            },
            &engine,
            &mut state,
            &mut sink,
        )
        .unwrap();
        let second = state.transaction_descriptor;
        assert_ne!(first, second);
        assert_eq!(state.trancount, 1);
        assert_eq!(
            sink.0,
            vec![
                Event::EnvChange(EnvChange::CommitTransaction(first)),
                Event::EnvChange(EnvChange::BeginTransaction(second)),
                Event::Done,
            ]
        );
    }

    #[test]
    fn mismatched_batch_descriptor_returns_3926_then_3971() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (descriptor, _) = begin(&mut state, &engine);
        let mut sink = Recording::default();
        assert!(!reject_mismatched_descriptor(descriptor + 1, &mut state, &mut sink).unwrap());
        assert_eq!(
            sink.0,
            vec![
                Event::Info(catalogue_info(3926)),
                Event::Error(SqlError::failed_to_resume_transaction(descriptor + 1)),
                Event::Done,
            ]
        );
    }

    #[test]
    fn zero_descriptor_with_open_transaction_returns_3989() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let (_, _) = begin(&mut state, &engine);
        let mut sink = Recording::default();
        assert!(!reject_mismatched_descriptor(0, &mut state, &mut sink).unwrap());
        assert_error(&sink, 3989);
    }

    #[test]
    fn unsupported_does_not_close() {
        let engine = engine();
        let mut state = SessionState::new(51);
        let mut sink = Recording::default();
        handle(&TmRequest::Unsupported(0), &engine, &mut state, &mut sink).unwrap();
        assert!(matches!(sink.0.as_slice(), [Event::Error(_), Event::Done]));

        let (_, next) = begin(&mut state, &engine);
        assert!(matches!(
            next.0.as_slice(),
            [
                Event::EnvChange(EnvChange::BeginTransaction(_)),
                Event::Done
            ]
        ));
    }

    fn assert_error(sink: &Recording, number: u32) {
        let [Event::Error(error), Event::Done] = sink.0.as_slice() else {
            panic!("expected ERROR then DONE, got {:?}", sink.0);
        };
        assert_eq!(error.number, number);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
    }
}
