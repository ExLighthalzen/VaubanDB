//! The four transaction statements, bound: what `TxnStatement` carries, and what the
//! binder deliberately does not look at.
//!
//! `txn_stmt.rs` translates `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION` into
//! [`TxnStatement`]. The tests below read the bound shape of each form, and three of them
//! say where the neighbouring forms stop instead: the parser (`BEGIN DISTRIBUTED
//! TRANSACTION`, `BEGIN TRAN @n`) or the session (`SET TRANSACTION ISOLATION LEVEL`,
//! `SET LOCK_TIMEOUT`, `SET XACT_ABORT`).

use vauban_binder::{BindContext, BoundStatement, SessionOptions, TxnStatement, bind};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};

/// The statements of `text`, bound one by one.
///
/// # Panics
///
/// When the text does not parse, or when one of its statements does not bind: both are the
/// failure the calling test reports.
fn bound(text: &str) -> Vec<BoundStatement> {
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|error| panic!("{text} parses, got {}: {}", error.number, error.message));
    let ctx = BindContext::scalar(text, SessionOptions::default());
    batch
        .statements
        .iter()
        .map(|statement| {
            bind(statement, &ctx).unwrap_or_else(|error| {
                panic!("{text} binds, got {}: {}", error.number, error.message)
            })
        })
        .collect()
}

/// The transaction statement `text` binds to, when it holds exactly one statement.
///
/// # Panics
///
/// When the text binds to anything other than one [`BoundStatement::Transaction`].
fn txn(text: &str) -> TxnStatement {
    match bound(text).as_slice() {
        [BoundStatement::Transaction(statement)] => statement.clone(),
        other => panic!("{text} binds to one transaction statement, got {other:?}"),
    }
}

/// The error the first statement of `text` answers, be it the parser's or the binder's.
///
/// # Panics
///
/// When every statement of the text binds: a test that calls this expects a refusal.
fn refusal(text: &str) -> SqlError {
    let batch = match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => batch,
        Err(error) => return error,
    };
    let ctx = BindContext::scalar(text, SessionOptions::default());
    for statement in &batch.statements {
        if let Err(error) = bind(statement, &ctx) {
            return error;
        }
    }
    panic!("{text} was expected to be refused");
}

/// The name of a transaction reaches the bound statement as the text wrote it.
///
/// The counter-proof is the pair `T1` / `t1`: a binder that folded the case would bind the
/// two spellings to one name, and the last assertion would fail. Delimiters are the
/// lexer's business and are already gone (`[My Tran]`).
#[test]
fn begin_tran_with_name_keeps_the_name_as_written() {
    let vectors: &[(&str, Option<&str>)] = &[
        ("BEGIN TRANSACTION", None),
        ("BEGIN TRAN", None),
        ("BEGIN TRAN T1", Some("T1")),
        ("begin tran t1", Some("t1")),
        ("BEGIN TRANSACTION MixedCase", Some("MixedCase")),
        ("BEGIN TRAN [My Tran]", Some("My Tran")),
    ];
    for (text, expected) in vectors {
        match txn(text) {
            TxnStatement::Begin { name, mark } => {
                assert_eq!(name.as_deref(), *expected, "{text}");
                assert_eq!(mark, None, "{text} was written without WITH MARK");
            }
            other => panic!("{text} binds to a BEGIN, got {other:?}"),
        }
    }

    let upper = txn("BEGIN TRAN T1");
    let lower = txn("BEGIN TRAN t1");
    match (upper, lower) {
        (TxnStatement::Begin { name: upper, .. }, TxnStatement::Begin { name: lower, .. }) => {
            assert_ne!(upper, lower, "the two spellings keep their own case")
        }
        other => panic!("both bind to a BEGIN, got {other:?}"),
    }
}

/// The text of a `WITH MARK` is carried into the bound statement; an absent description is
/// the empty string, and an absent clause is `None`.
#[test]
fn begin_tran_carries_the_text_of_with_mark() {
    let vectors: &[(&str, Option<&str>)] = &[
        ("BEGIN TRAN t1", None),
        ("BEGIN TRAN t1 WITH MARK 'daily load'", Some("daily load")),
        ("BEGIN TRAN t1 WITH MARK", Some("")),
        ("BEGIN TRANSACTION WITH MARK 'm'", Some("m")),
    ];
    for (text, expected) in vectors {
        match txn(text) {
            TxnStatement::Begin { mark, .. } => assert_eq!(mark.as_deref(), *expected, "{text}"),
            other => panic!("{text} binds to a BEGIN, got {other:?}"),
        }
    }
}

/// `COMMIT` and `ROLLBACK` bind outside any transaction: the binder reads no counter.
///
/// 3902 and 3903 are the executor's, raised when the bound statement runs; here the
/// batch `COMMIT` alone binds, and so does a `ROLLBACK` naming a transaction that was
/// not opened. The counter-proof that the assertions check something is
/// `distributed_transaction_is_refused_by_the_parser`, where a transaction statement of
/// the same family is refused and `bound` would panic.
#[test]
fn commit_and_rollback_bind_without_state_check() {
    let commits: &[(&str, Option<&str>)] = &[
        ("COMMIT", None),
        ("COMMIT TRAN", None),
        ("COMMIT TRANSACTION", None),
        ("COMMIT WORK", None),
        ("COMMIT TRAN t1", Some("t1")),
        ("COMMIT TRANSACTION T1", Some("T1")),
    ];
    for (text, expected) in commits {
        match txn(text) {
            TxnStatement::Commit { name } => assert_eq!(name.as_deref(), *expected, "{text}"),
            other => panic!("{text} binds to a COMMIT, got {other:?}"),
        }
    }

    let rollbacks: &[(&str, Option<&str>)] = &[
        ("ROLLBACK", None),
        ("ROLLBACK TRAN", None),
        ("ROLLBACK WORK", None),
        ("ROLLBACK TRANSACTION never_opened", Some("never_opened")),
    ];
    for (text, expected) in rollbacks {
        match txn(text) {
            TxnStatement::Rollback { name } => assert_eq!(name.as_deref(), *expected, "{text}"),
            other => panic!("{text} binds to a ROLLBACK, got {other:?}"),
        }
    }

    // The whole batch of a client that commits twice more than it opened binds: five
    // statements in, five bound statements out.
    assert_eq!(
        bound("BEGIN TRAN; COMMIT; COMMIT; ROLLBACK; COMMIT TRAN t1;").len(),
        5
    );
}

/// `SAVE TRAN s1` binds to the variant that names a savepoint, the name kept as written.
#[test]
fn save_tran_binds() {
    let vectors: &[(&str, &str)] = &[
        ("SAVE TRAN s1", "s1"),
        ("SAVE TRANSACTION S1", "S1"),
        ("SAVE TRAN [my point]", "my point"),
    ];
    for (text, expected) in vectors {
        match txn(text) {
            TxnStatement::Save { name } => assert_eq!(name, *expected, "{text}"),
            other => panic!("{text} binds to a SAVE, got {other:?}"),
        }
    }
}

/// The bound `ROLLBACK` does not record whether its name is a transaction or a savepoint:
/// one `name` field, filled the same way in both readings.
///
/// The batch has the same spelling `ROLLBACK TRAN x` follow a `BEGIN TRAN x` in one half
/// and a `SAVE TRAN x` in the other: the two bound statements carry the same name and the
/// same variant, which is the shape the executor resolves at run time.
#[test]
fn rollback_to_savepoint_binds() {
    for text in [
        "BEGIN TRAN x; ROLLBACK TRAN x;",
        "SAVE TRAN x; ROLLBACK TRAN x;",
    ] {
        match bound(text).as_slice() {
            [
                _,
                BoundStatement::Transaction(TxnStatement::Rollback { name }),
            ] => {
                assert_eq!(name.as_deref(), Some("x"), "{text}");
            }
            other => panic!("{text} binds to two statements, the second a ROLLBACK: {other:?}"),
        }
    }
}

/// `BEGIN DISTRIBUTED TRANSACTION` stops at the parser, which is why the binder holds no
/// arm for it: 156 on the keyword `DISTRIBUTED`. `BEGIN TRAN` is the counter-proof in the
/// same test — the same head word, accepted.
#[test]
fn distributed_transaction_is_refused_by_the_parser() {
    for text in ["BEGIN DISTRIBUTED TRANSACTION", "BEGIN DISTRIBUTED TRAN"] {
        let error = refusal(text);
        assert_eq!(error.number, 156, "{text}: {}", error.message);
        assert_eq!(
            error.message, "Syntax error near the keyword 'DISTRIBUTED'.",
            "{text}"
        );
    }
    assert!(matches!(txn("BEGIN TRAN"), TxnStatement::Begin { .. }));
}

/// A transaction named by a variable stops at the parser too: `Statement::BeginTransaction`
/// holds an `Ident`, not an expression, and the parser answers 102 on the variable.
#[test]
fn a_transaction_named_by_a_variable_is_refused_by_the_parser() {
    for text in ["BEGIN TRAN @n", "COMMIT TRAN @n", "SAVE TRAN @n"] {
        let error = refusal(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, "Syntax error near '@n'.", "{text}");
    }
    assert!(matches!(txn("BEGIN TRAN n"), TxnStatement::Begin { .. }));
}

/// A `SET <option>` binds to `BoundStatement::SetOption`, the statement the parser built
/// unchanged: the session applies it when it runs, so the binder checks nothing, an option
/// it does not know included. Written in a block or a branch, it binds there too.
#[test]
fn set_options_bind_as_they_were_parsed() {
    let texts = [
        "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCK_TIMEOUT 500",
        "SET LOCK_TIMEOUT -1",
        "SET XACT_ABORT ON",
        "SET XACT_ABORT OFF",
        "SET IMPLICIT_TRANSACTIONS ON",
        "SET DEADLOCK_PRIORITY LOW",
        "SET NO_SUCH_OPTION ON",
    ];
    for text in texts {
        let parsed = match parse_batch(text, &ParseOptions::default()) {
            Ok(batch) => batch.statements,
            Err(error) => panic!("{text} parses: {}", error.message),
        };
        let [vauban_parser::Statement::SetOption(written)] = parsed.as_slice() else {
            panic!("{text} is one SET of an option");
        };
        match bound(text).as_slice() {
            [BoundStatement::SetOption(set)] => assert_eq!(set, written, "{text}"),
            other => panic!("{text} binds to one SET of an option, got {other:?}"),
        }
    }
    match bound("BEGIN SET XACT_ABORT ON END").as_slice() {
        [BoundStatement::Block(held)] => {
            assert!(matches!(held.as_slice(), [BoundStatement::SetOption(_)]));
        }
        other => panic!("a block that holds a SET binds to a block, got {other:?}"),
    }
    match bound("IF 1 = 1 SET NOCOUNT ON").as_slice() {
        [BoundStatement::If { then_, .. }] => {
            assert!(matches!(then_.as_ref(), BoundStatement::SetOption(_)));
        }
        other => panic!("an IF that holds a SET binds to an IF, got {other:?}"),
    }
    assert!(matches!(txn("BEGIN TRAN"), TxnStatement::Begin { .. }));
}
