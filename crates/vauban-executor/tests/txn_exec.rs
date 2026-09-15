#![allow(missing_docs)]
use std::sync::Arc;

use vauban_binder::{SessionOptions, TxnStatement};
use vauban_executor::{ExecContext, ExecOutcome, ExecSession, execute_collect};
use vauban_planner::PhysicalStatement;
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};

/// A storage and a transaction manager for the transaction tests.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self { storage, txn }
    }

    /// Runs a transaction statement and checks the outcome.
    fn run(&self, stmt: &TxnStatement, session: &mut ExecSession) -> ExecOutcome {
        let eval = StaticContext::default();
        let snap = if let Some(ref h) = session.txn {
            self.txn.statement_snapshot(h)
        } else {
            let h = self.txn.begin(IsolationLevel::ReadCommitted);
            self.txn.statement_snapshot(&h)
        };
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_session(session);
        let physical = PhysicalStatement::Transaction(stmt.clone());
        let (outcome, _) = execute_collect(&physical, &mut ctx).expect("the statement runs");
        outcome
    }

    /// Runs a transaction statement that is expected to fail, returning the error number.
    fn run_err(&self, stmt: &TxnStatement, session: &mut ExecSession) -> u32 {
        let eval = StaticContext::default();
        let snap = if let Some(ref h) = session.txn {
            self.txn.statement_snapshot(h)
        } else {
            let h = self.txn.begin(IsolationLevel::ReadCommitted);
            self.txn.statement_snapshot(&h)
        };
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_session(session);
        let physical = PhysicalStatement::Transaction(stmt.clone());
        execute_collect(&physical, &mut ctx)
            .expect_err("the statement fails")
            .number
    }
}

/// A bare `BEGIN` opens transaction count 1.
#[test]
fn begin_sets_trancount_to_one() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let stmt = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    let outcome = fix.run(&stmt, &mut session);
    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.trancount, 1);
    assert!(session.txn.is_some());
}

/// A second `BEGIN` increments `trancount` but opens no new transaction.
#[test]
fn nested_begin_increments_trancount() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let stmt = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    fix.run(&stmt, &mut session);
    fix.run(&stmt, &mut session);
    assert_eq!(session.trancount, 2);
}

/// `COMMIT` without `BEGIN` yields 3902.
#[test]
fn commit_without_begin_is_3902() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let stmt = TxnStatement::Commit { name: None };
    let number = fix.run_err(&stmt, &mut session);
    assert_eq!(number, 3902);
}

/// `ROLLBACK` without `BEGIN` yields 3903.
#[test]
fn rollback_without_begin_is_3903() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let stmt = TxnStatement::Rollback { name: None };
    let number = fix.run_err(&stmt, &mut session);
    assert_eq!(number, 3903);
}

/// Nested `BEGIN` / `COMMIT` commit once: the outer commit validates the transaction.
#[test]
fn nested_begin_commits_once() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    let commit = TxnStatement::Commit { name: None };

    fix.run(&begin, &mut session);
    assert_eq!(session.trancount, 1);

    fix.run(&begin, &mut session);
    assert_eq!(session.trancount, 2);

    // Inner commit: trancount decrements, no real commit
    fix.run(&commit, &mut session);
    assert_eq!(session.trancount, 1);
    assert!(session.txn.is_some());

    // Outer commit: real commit, trancount = 0
    fix.run(&commit, &mut session);
    assert_eq!(session.trancount, 0);
    assert!(session.txn.is_none());
}

/// `COMMIT` after the outer level yields 3902.
#[test]
fn one_commit_too_many_is_3902() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    let commit = TxnStatement::Commit { name: None };

    fix.run(&begin, &mut session);
    fix.run(&commit, &mut session);
    let number = fix.run_err(&commit, &mut session);
    assert_eq!(number, 3902);
}

/// `ROLLBACK` undoes every nesting level at once.
#[test]
fn rollback_undoes_every_level() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    let rollback = TxnStatement::Rollback { name: None };

    fix.run(&begin, &mut session);
    fix.run(&begin, &mut session);
    assert_eq!(session.trancount, 2);

    fix.run(&rollback, &mut session);
    assert_eq!(session.trancount, 0);
    assert!(session.txn.is_none());
}

/// `SAVE TRAN` followed by `ROLLBACK TRAN <name>` returns to the savepoint, leaving
/// `trancount` unchanged and the transaction still open.
#[test]
fn named_rollback_is_a_savepoint() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    let save = TxnStatement::Save {
        name: "sp1".to_owned(),
    };
    let rollback_sp = TxnStatement::Rollback {
        name: Some("sp1".to_owned()),
    };
    let commit = TxnStatement::Commit { name: None };

    fix.run(&begin, &mut session);
    fix.run(&save, &mut session);
    assert_eq!(session.trancount, 1);

    fix.run(&rollback_sp, &mut session);
    assert_eq!(session.trancount, 1);
    assert!(session.txn.is_some());

    fix.run(&commit, &mut session);
    assert_eq!(session.trancount, 0);
    assert!(session.txn.is_none());
}

/// `ROLLBACK TRAN <unknown_name>` fails with error 6401.
#[test]
fn rollback_unknown_savepoint_name_fails() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    let rollback_sp = TxnStatement::Rollback {
        name: Some("nosuch".to_owned()),
    };

    fix.run(&begin, &mut session);
    let number = fix.run_err(&rollback_sp, &mut session);
    assert_eq!(number, 6401);
}

/// `SAVE TRAN` outside a transaction fails with error 628.
#[test]
fn save_tran_without_transaction_fails() {
    let fix = Fixture::new();
    let mut session = ExecSession::default();
    let save = TxnStatement::Save {
        name: "sp1".to_owned(),
    };
    let number = fix.run_err(&save, &mut session);
    assert_eq!(number, 628);
}

/// `@@TRANCOUNT` is held in `ExecSession` and starts at 0.
#[test]
fn trancount_is_read_by_eval() {
    let mut session = ExecSession::default();
    assert_eq!(session.trancount, 0);

    let fix = Fixture::new();
    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    fix.run(&begin, &mut session);
    assert_eq!(session.trancount, 1);
}

/// `XACT_ABORT OFF` (default): `end_statement` rolls back to the statement savepoint,
/// keeping the transaction open.
#[test]
fn error_keeps_transaction_open() {
    use vauban_errors::SqlError;
    let fix = Fixture::new();
    let mut session = ExecSession::default();

    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    fix.run(&begin, &mut session);
    assert_eq!(session.trancount, 1);
    assert!(session.txn.is_some());

    let eval = StaticContext::default();
    let snap = fix.txn.statement_snapshot(session.txn.as_ref().unwrap());
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(fix.storage.as_ref(), &fix.txn, &snap)
        .with_session(&mut session);

    vauban_executor::txn_exec::begin_statement(&mut ctx).expect("begin_statement");
    let err: Result<ExecOutcome, _> = Err(SqlError::new(1234, 16, 1, "test error"));
    vauban_executor::txn_exec::end_statement(&mut ctx, &err).expect("end_statement");
    let s = ctx.session().unwrap();
    assert_eq!(s.trancount, 1);
    assert!(s.txn.is_some());
}

/// `XACT_ABORT ON`: `end_statement` rolls back the whole transaction.
#[test]
fn xact_abort_aborts_the_batch() {
    use vauban_errors::SqlError;
    let fix = Fixture::new();
    let mut session = ExecSession {
        xact_abort: true,
        ..ExecSession::default()
    };

    let begin = TxnStatement::Begin {
        name: None,
        mark: None,
    };
    fix.run(&begin, &mut session);
    assert_eq!(session.trancount, 1);

    let eval = StaticContext::default();
    let snap = fix.txn.statement_snapshot(session.txn.as_ref().unwrap());
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(fix.storage.as_ref(), &fix.txn, &snap)
        .with_session(&mut session);

    vauban_executor::txn_exec::begin_statement(&mut ctx).expect("begin_statement");
    let err: Result<ExecOutcome, _> = Err(SqlError::new(8134, 16, 1, "Divide by zero"));
    vauban_executor::txn_exec::end_statement(&mut ctx, &err).expect("end_statement");
    let s = ctx.session().unwrap();
    assert_eq!(s.trancount, 0);
    assert!(s.txn.is_none());
}
