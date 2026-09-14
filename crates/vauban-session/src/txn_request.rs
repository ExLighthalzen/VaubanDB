//! TRANSACTION_MANAGER requests accepted for driver compatibility.
//!
//! The session tracks a descriptor and `@@TRANCOUNT`, but does not start a storage
//! transaction: writes remain autocommit.

use tracing::warn;
use vauban_errors::{InternalError, SqlError, SqlResult, message_template};
use vauban_tds::{EnvChange, TmRequest};

use crate::{ResultSink, SessionState};

/// Handles one TRANSACTION_MANAGER request and emits its complete logical response.
///
/// SQL Server also emits SESSIONSTATE after the opening ENVCHANGE when the login
/// negotiated SESSIONRECOVERY. VaubanDB does not acknowledge that feature, so this path
/// deliberately emits no SESSIONSTATE. `server.rs` supplies the `CurCmd` 0xFD
/// when it relays the DONE produced through [`ResultSink::done`].
pub(crate) fn handle(
    request: &TmRequest,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    match request {
        TmRequest::Begin { .. } => {
            begin(state, sink)?;
            sink.done(None, false)
        }
        TmRequest::Commit {
            transaction_descriptor,
            begin_next,
            ..
        } => finish(*transaction_descriptor, *begin_next, false, state, sink),
        TmRequest::Rollback {
            transaction_descriptor,
            begin_next,
            ..
        } => finish(*transaction_descriptor, *begin_next, true, state, sink),
        TmRequest::Save { .. } => refuse(3903, state, sink),
        TmRequest::Unsupported(request_type) => {
            warn!(request_type, "unsupported transaction manager request");
            let error: SqlError = InternalError::Bug(format!(
                "unsupported TRANSACTION_MANAGER request type {request_type}"
            ))
            .into();
            sink.error(&error)?;
            sink.done(None, false)
        }
    }
}

/// Starts the tracked transaction without closing the response: chained requests append
/// their new BEGIN before the single final DONE.
fn begin(state: &mut SessionState, sink: &mut dyn ResultSink) -> SqlResult<()> {
    let descriptor = state.allocate_transaction_descriptor().ok_or_else(|| {
        SqlError::from(InternalError::Bug(
            "transaction descriptor space exhausted".into(),
        ))
    })?;
    state.transaction_descriptor = descriptor;
    state.trancount = 1;
    sink.env_change(&EnvChange::BeginTransaction(descriptor))
}

/// Commits or rolls back the current tracked transaction, optionally chaining a new one.
fn finish(
    descriptor: u64,
    begin_next: Option<u8>,
    rollback: bool,
    state: &mut SessionState,
    sink: &mut dyn ResultSink,
) -> SqlResult<()> {
    if descriptor == 0 || descriptor != state.transaction_descriptor {
        return refuse(if rollback { 3903 } else { 3902 }, state, sink);
    }

    let change = if rollback {
        EnvChange::RollbackTransaction(descriptor)
    } else {
        EnvChange::CommitTransaction(descriptor)
    };
    sink.env_change(&change)?;
    state.transaction_descriptor = 0;
    state.trancount = 0;
    if begin_next.is_some() {
        begin(state, sink)?;
    }
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

#[cfg(test)]
mod tests {
    use vauban_errors::{InfoMessage, SqlError};
    use vauban_tds::{ColumnMeta, EnvChange};
    use vauban_types::{TypeInfo, Value};

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Done,
        Error(SqlError),
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
        fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
            unreachable!()
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

    fn begin_request() -> TmRequest {
        TmRequest::Begin {
            transaction_descriptor: 0,
            isolation: 0,
            name: String::new(),
        }
    }

    fn begin(state: &mut SessionState) -> (u64, Recording) {
        let mut sink = Recording::default();
        handle(&begin_request(), state, &mut sink).unwrap();
        (state.transaction_descriptor, sink)
    }

    #[test]
    fn begin_answers_with_an_envchange() {
        let mut state = SessionState::new(51);
        let (descriptor, sink) = begin(&mut state);
        assert_ne!(descriptor, 0);
        assert_eq!(state.trancount, 1);
        assert_eq!(
            sink.0,
            vec![
                Event::EnvChange(EnvChange::BeginTransaction(descriptor)),
                Event::Done,
            ]
        );
    }

    #[test]
    fn descriptor_is_stable_and_unique() {
        let mut state = SessionState::new(51);
        let (first, _) = begin(&mut state);
        let mut sink = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: first,
                name: String::new(),
                begin_next: None,
            },
            &mut state,
            &mut sink,
        )
        .unwrap();
        let (second, _) = begin(&mut state);
        assert_ne!(first, second);
        assert_eq!(state.transaction_descriptor, second);
    }

    #[test]
    fn commit_matches_the_descriptor() {
        let mut state = SessionState::new(51);
        let (descriptor, _) = begin(&mut state);

        let mut mismatch = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: descriptor + 1,
                name: String::new(),
                begin_next: None,
            },
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
            &mut state,
            &mut sink,
        )
        .unwrap();
        assert_error(&sink, 3902);
    }

    #[test]
    fn rollback_matches_the_descriptor() {
        let mut state = SessionState::new(51);
        let (descriptor, _) = begin(&mut state);

        let mut mismatch = Recording::default();
        handle(
            &TmRequest::Rollback {
                transaction_descriptor: descriptor + 1,
                name: String::new(),
                begin_next: None,
            },
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
            &mut state,
            &mut sink,
        )
        .unwrap();
        assert_error(&sink, 3903);
    }

    #[test]
    fn commit_with_begin_next_chains() {
        let mut state = SessionState::new(51);
        let (first, _) = begin(&mut state);
        let mut sink = Recording::default();
        handle(
            &TmRequest::Commit {
                transaction_descriptor: first,
                name: String::new(),
                begin_next: Some(0),
            },
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
    fn unsupported_does_not_close() {
        let mut state = SessionState::new(51);
        let mut sink = Recording::default();
        handle(&TmRequest::Unsupported(0), &mut state, &mut sink).unwrap();
        assert!(matches!(sink.0.as_slice(), [Event::Error(_), Event::Done]));

        let (_, next) = begin(&mut state);
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
