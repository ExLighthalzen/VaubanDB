use vauban_parser::{ParseOptions, Span, Statement, TableHint, TableRef, parse_batch};

use super::{Word, bind_hints, bind_target_hints, conflicts, line_after};
use crate::bound::LockHints;
use crate::context::{BindContext, SessionOptions};

/// The hint list of the target of the single statement of `text`, an `UPDATE`,
/// `DELETE` or `INSERT`.
fn target_hints(text: &str) -> Vec<TableHint> {
    let batch = parse_batch(text, &ParseOptions::default()).expect("parses");
    let target = match &batch.statements[0] {
        Statement::Update(update) => &update.target,
        Statement::Delete(delete) => &delete.target,
        Statement::Insert(insert) => &insert.target,
        other => panic!("not a data-modification statement: {other:?}"),
    };
    match target {
        TableRef::Table { hints, .. } => hints.clone(),
        other => panic!("not a named target: {other:?}"),
    }
}

#[test]
fn nolock_on_update_target_is_1065() {
    for text in [
        "UPDATE dbo.t WITH (NOLOCK) SET a = 1 WHERE id = 99;",
        "UPDATE dbo.t WITH (readuncommitted) SET a = 1 WHERE id = 99;",
        "DELETE FROM dbo.t WITH (NOLOCK) WHERE id = 99;",
        "DELETE dbo.t WITH (NOLOCK) WHERE id = 99;",
        "INSERT INTO dbo.t WITH (NOLOCK) (id, a) VALUES (99, 0);",
        // 1065 before the 1047 of the same list, before an unknown word, before
        // the refusal of READPAST.
        "UPDATE dbo.t WITH (TABLOCKX, NOLOCK) SET a = 1 WHERE id = 99;",
        "UPDATE dbo.t WITH (NOLOCK, FOO) SET a = 1 WHERE id = 99;",
        "UPDATE dbo.t WITH (NOLOCK, READPAST) SET a = 1 WHERE id = 99;",
    ] {
        let ctx = BindContext::scalar(text, SessionOptions::default());
        let err = bind_target_hints(&target_hints(text), &ctx).expect_err(text);
        assert_eq!(
            (err.number, err.severity, err.state),
            (1065, 15, 1),
            "{text}"
        );
    }
}

#[test]
fn the_line_of_1065_is_fixed() {
    let on_line_21 = format!("{}UPDATE dbo.t WITH (NOLOCK) SET a = 1;", "\n".repeat(20));
    let hint_on_line_30 = format!("UPDATE dbo.t{}WITH (NOLOCK) SET a = 1;", "\n".repeat(28));
    for text in [
        "UPDATE dbo.t WITH (NOLOCK) SET a = 1;",
        "UPDATE dbo.t\nWITH\n(NOLOCK)\nSET a = 1;",
        on_line_21.as_str(),
        hint_on_line_30.as_str(),
    ] {
        let ctx = BindContext::scalar(text, SessionOptions::default());
        let err = bind_target_hints(&target_hints(text), &ctx).expect_err(text);
        assert_eq!(err.line, 15, "{text}");
    }
}

#[test]
fn a_target_without_nolock_reads_the_ordinary_rules() {
    for (text, number) in [
        ("UPDATE dbo.t WITH (TABLOCKX) SET a = 1;", None),
        ("UPDATE dbo.t WITH (PAGLOCK) SET a = 1;", None),
        (
            "UPDATE dbo.t WITH (ROWLOCK, TABLOCK) SET a = 1;",
            Some(1047),
        ),
        ("UPDATE dbo.t WITH (UPDLOCK, XLOCK) SET a = 1;", Some(1047)),
        (
            "UPDATE dbo.t WITH (SERIALIZABLE, READPAST) SET a = 1;",
            Some(50000),
        ),
    ] {
        let ctx = BindContext::scalar(text, SessionOptions::default());
        let bound = bind_target_hints(&target_hints(text), &ctx);
        assert_eq!(bound.as_ref().err().map(|e| e.number), number, "{text}");
    }
    let text = "UPDATE dbo.t WITH (TABLOCKX) SET a = 1;";
    let ctx = BindContext::scalar(text, SessionOptions::default());
    let hints = bind_target_hints(&target_hints(text), &ctx).expect("binds");
    assert!(hints.tablockx && hints.locks_the_table() && hints.locks_exclusively());
}

#[test]
fn a_word_does_not_conflict_with_itself() {
    for word in [
        Word::ReadUncommitted,
        Word::ReadCommitted,
        Word::Serializable,
        Word::UpdLock,
        Word::RowLock,
        Word::TabLock,
        Word::ReadPast,
        Word::NoWait,
    ] {
        assert!(!conflicts(word, word), "{word:?}");
    }
}

#[test]
fn conflicts_is_symmetric() {
    let words = [
        Word::ReadUncommitted,
        Word::ReadCommitted,
        Word::ReadCommittedLock,
        Word::RepeatableRead,
        Word::Serializable,
        Word::Snapshot,
        Word::UpdLock,
        Word::XLock,
        Word::RowLock,
        Word::PagLock,
        Word::TabLock,
        Word::TabLockX,
        Word::ReadPast,
        Word::NoWait,
    ];
    for a in words {
        for b in words {
            assert_eq!(conflicts(a, b), conflicts(b, a), "{a:?} {b:?}");
        }
    }
}

#[test]
fn the_line_after_a_word_skips_blanks_and_comments() {
    let text = "SELECT 1 FROM t WITH (NOLOCK, TABLOCKX -- c\n\n) WHERE 1 = 1";
    assert_eq!(&text[30..38], "TABLOCKX");
    let word = Span {
        line: 1,
        column: 31,
        offset: 30,
        len: 8,
    };
    assert_eq!(line_after(&word, text), 3);

    let text = "WITH (NOLOCK, TABLOCKX /* a\nb */\n, NOWAIT)";
    assert_eq!(&text[14..22], "TABLOCKX");
    let word = Span {
        line: 1,
        column: 15,
        offset: 14,
        len: 8,
    };
    assert_eq!(line_after(&word, text), 3);
    assert_eq!(line_after(&word, "WITH (NOLOCK, TABLOCKX"), 1);
}

#[test]
fn an_empty_list_is_the_default() {
    let ctx = BindContext::scalar("", SessionOptions::default());
    assert_eq!(bind_hints(&[], &ctx).expect("binds"), LockHints::default());
}
