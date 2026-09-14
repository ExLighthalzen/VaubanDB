//! The locking hints of a table reference: the words of a `WITH (…)` read into the
//! [`LockHints`] of the [`LogicalPlan::Scan`](crate::bound::LogicalPlan::Scan) they were
//! written on, 1047 for a pair SQL Server refuses, 1065 for `NOLOCK` on the target of a
//! data-modification statement.
//!
//! # Three axes, one flag per word
//!
//! [`LockHints`] keeps one flag per word, and the words fall on three axes:
//!
//! - **isolation**: `NOLOCK` / `READUNCOMMITTED` (one flag, `nolock`), `READCOMMITTED` /
//!   `READCOMMITTEDLOCK` (`readcommitted`), `REPEATABLEREAD`, `SERIALIZABLE` / `HOLDLOCK`
//!   (`serializable`), `SNAPSHOT`. Once the executor has translated it for `txn`, that axis
//!   is read ahead of the level of the transaction, `HOLDLOCK` being the `SERIALIZABLE`
//!   hint under another word (`tests/bind_hints.rs`, `holdlock_is_serializable`);
//! - **granularity**: `ROWLOCK`, `PAGLOCK`, `TABLOCK`, `TABLOCKX`. `TABLOCKX` is a
//!   `TABLOCK` taken as an exclusive lock, which [`LockHints::locks_the_table`] and
//!   [`LockHints::locks_exclusively`] read as such (`tablockx_is_table_plus_exclusive`).
//!   The row is the granularity `txn` locks at, so `ROWLOCK` asks for what it gets, and
//!   `PAGLOCK`, `TABLOCK` and `TABLOCKX` are carried without effect until the lock manager
//!   knows a table lock;
//! - **modifiers**: `UPDLOCK`, `XLOCK`, `READPAST`, `NOWAIT`, each with its field on the
//!   lock intent of `txn`.
//!
//! # What is refused, and what is let through
//!
//! A pair of words is refused with 1047 on the list alone, before the name is resolved
//! (`conflicting_hints_come_before_208`) and in `FROM` order across references. The pairs
//! refused are the ones of [`conflicts`]: two isolation words of different meaning
//! (`NOLOCK` and `READUNCOMMITTED` do not conflict, nor `SERIALIZABLE` and `HOLDLOCK`, nor a
//! word with itself), `NOLOCK` with a granularity word or with `UPDLOCK` or `XLOCK`, two
//! granularity words (`TABLOCK` with `TABLOCKX` included), `UPDLOCK` with `XLOCK`. The
//! line of a 1047 is the line of the token that follows the second word of the pair, a
//! comma or a closing parenthesis, comments skipped (`the_line_of_1047_is_the_token_after_the_word`).
//!
//! Three refusals of SQL Server have no number in `vauban-errors` yet and are raised as the
//! internal error 50000, so that the statement is refused rather than run on a meaning it
//! does not have: a word that is no table hint (321 on SQL Server,
//! `an_unknown_word_under_with_is_refused`), `READPAST` with `NOLOCK`, `READUNCOMMITTED`,
//! `SERIALIZABLE` or `HOLDLOCK` (650, `readpast_under_nolock_or_serializable_is_refused`),
//! and `SNAPSHOT`, which SQL Server accepts on memory-optimized tables alone (367,
//! `snapshot_is_refused`). The unknown word is looked for first, then a 1047 pair, then
//! the two others, whichever the order the words were written in
//! (`an_unknown_word_wins_over_a_conflict`).
//!
//! The words that are not locking hints (`INDEX(…)`, `FORCESEEK`, `FORCESCAN`, `NOEXPAND`,
//! `KEEPIDENTITY`, `KEEPDEFAULTS`, `IGNORE_CONSTRAINTS`, `IGNORE_TRIGGERS`,
//! `SPATIAL_WINDOW_MAX_CELLS`) are accepted and dropped: whether SQL Server accepts them
//! depends on the object and on the plan, which this module does not look at
//! (`non_locking_hints_are_dropped`).
//!
//! # The target of `INSERT`, `UPDATE`, `DELETE`
//!
//! [`bind_target_hints`] refuses `NOLOCK` and `READUNCOMMITTED` with 1065 before anything
//! else is looked at: before a conflict of the same list, before an unknown word, before
//! the name is resolved. The line of a 1065 is 15 on the shapes tried, the statement
//! sitting on line 1 or 21 and its hint on line 1, 3 or 30 (`the_line_of_1065_is_fixed`).
//! The refusal reads the list written on the target itself: the same words on an alias of
//! the target in a `FROM` are the ordinary hints of that reference.

use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, Span, TableHint};

use crate::bound::LockHints;
use crate::context::BindContext;
use crate::query::bug;

/// A locking word, one per meaning: the two spellings of a meaning fall on one variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Word {
    /// `NOLOCK`, `READUNCOMMITTED`
    ReadUncommitted,
    /// `READCOMMITTED`
    ReadCommitted,
    /// `READCOMMITTEDLOCK`
    ReadCommittedLock,
    /// `REPEATABLEREAD`
    RepeatableRead,
    /// `SERIALIZABLE`, `HOLDLOCK`
    Serializable,
    /// `SNAPSHOT`
    Snapshot,
    /// `UPDLOCK`
    UpdLock,
    /// `XLOCK`
    XLock,
    /// `ROWLOCK`
    RowLock,
    /// `PAGLOCK`
    PagLock,
    /// `TABLOCK`
    TabLock,
    /// `TABLOCKX`
    TabLockX,
    /// `READPAST`
    ReadPast,
    /// `NOWAIT`
    NoWait,
}

impl Word {
    /// The word `name` spells, ASCII case ignored; `None` for a word that is no locking
    /// hint.
    fn parse(name: &str) -> Option<Word> {
        Some(match name.to_ascii_uppercase().as_str() {
            "NOLOCK" | "READUNCOMMITTED" => Word::ReadUncommitted,
            "READCOMMITTED" => Word::ReadCommitted,
            "READCOMMITTEDLOCK" => Word::ReadCommittedLock,
            "REPEATABLEREAD" => Word::RepeatableRead,
            "SERIALIZABLE" | "HOLDLOCK" => Word::Serializable,
            "SNAPSHOT" => Word::Snapshot,
            "UPDLOCK" => Word::UpdLock,
            "XLOCK" => Word::XLock,
            "ROWLOCK" => Word::RowLock,
            "PAGLOCK" => Word::PagLock,
            "TABLOCK" => Word::TabLock,
            "TABLOCKX" => Word::TabLockX,
            "READPAST" => Word::ReadPast,
            "NOWAIT" => Word::NoWait,
            _ => return None,
        })
    }

    /// Whether the word names an isolation level.
    fn is_isolation(self) -> bool {
        matches!(
            self,
            Word::ReadUncommitted
                | Word::ReadCommitted
                | Word::ReadCommittedLock
                | Word::RepeatableRead
                | Word::Serializable
                | Word::Snapshot
        )
    }

    /// Whether the word names a granularity.
    fn is_granularity(self) -> bool {
        matches!(
            self,
            Word::RowLock | Word::PagLock | Word::TabLock | Word::TabLockX
        )
    }
}

/// The words that are hints without being locking hints: accepted and dropped (module
/// documentation).
const NON_LOCKING_WORDS: [&str; 9] = [
    "FORCESCAN",
    "FORCESEEK",
    "IGNORE_CONSTRAINTS",
    "IGNORE_TRIGGERS",
    "INDEX",
    "KEEPDEFAULTS",
    "KEEPIDENTITY",
    "NOEXPAND",
    "SPATIAL_WINDOW_MAX_CELLS",
];

/// Whether two locking words refuse each other with 1047 (module documentation).
fn conflicts(a: Word, b: Word) -> bool {
    if a == b {
        return false;
    }
    (a.is_isolation() && b.is_isolation())
        || (a.is_granularity() && b.is_granularity())
        || matches!(
            (a, b),
            (Word::ReadUncommitted, Word::UpdLock | Word::XLock)
                | (Word::UpdLock | Word::XLock, Word::ReadUncommitted)
                | (Word::UpdLock, Word::XLock)
                | (Word::XLock, Word::UpdLock)
        )
        || (a == Word::ReadUncommitted && b.is_granularity())
        || (b == Word::ReadUncommitted && a.is_granularity())
}

/// Reads the hint list written on a table reference a query reads into the [`LockHints`]
/// of its `Scan`.
///
/// # Errors
///
/// - the internal error 50000 for a word that is no table hint, `SNAPSHOT`, and
///   `READPAST` next to `NOLOCK`, `READUNCOMMITTED`, `SERIALIZABLE` or `HOLDLOCK`;
/// - 1047 for a pair of words that refuse each other.
///
/// The order the three are looked for in is the module documentation's.
pub(crate) fn bind_hints(hints: &[TableHint], ctx: &BindContext<'_>) -> SqlResult<LockHints> {
    let mut words: Vec<(Word, Span)> = Vec::with_capacity(hints.len());
    for hint in hints {
        match Word::parse(&hint.name) {
            Some(word) => words.push((word, hint.span)),
            None if is_non_locking(&hint.name) => {}
            None => return Err(unknown_hint(&hint.name, hint.span)),
        }
    }
    for (i, &(later, span)) in words.iter().enumerate() {
        if words[..i]
            .iter()
            .any(|&(earlier, _)| conflicts(earlier, later))
        {
            return Err(
                SqlError::conflicting_locking_hints().with_line(line_after(&span, ctx.text))
            );
        }
    }
    let first_line = words.first().map_or(0, |(_, span)| span.line);
    let has = |wanted: Word| words.iter().any(|&(word, _)| word == wanted);
    if has(Word::ReadPast) && (has(Word::ReadUncommitted) || has(Word::Serializable)) {
        return Err(
            bug("READPAST is allowed with READ COMMITTED and REPEATABLE READ alone")
                .with_line(first_line),
        );
    }
    if has(Word::Snapshot) {
        return Err(
            bug("the SNAPSHOT hint is allowed on memory-optimized tables alone")
                .with_line(first_line),
        );
    }
    Ok(LockHints {
        nolock: has(Word::ReadUncommitted),
        readcommitted: has(Word::ReadCommitted) || has(Word::ReadCommittedLock),
        repeatableread: has(Word::RepeatableRead),
        serializable: has(Word::Serializable),
        snapshot: false,
        updlock: has(Word::UpdLock),
        xlock: has(Word::XLock),
        rowlock: has(Word::RowLock),
        paglock: has(Word::PagLock),
        tablock: has(Word::TabLock),
        tablockx: has(Word::TabLockX),
        readpast: has(Word::ReadPast),
        nowait: has(Word::NoWait),
    })
}

/// Reads the hint list written on the target of an `INSERT`, `UPDATE` or `DELETE`: 1065
/// for `NOLOCK` or `READUNCOMMITTED`, then what [`bind_hints`] answers.
///
/// # Errors
///
/// 1065 first, on line 15 (module documentation); then the errors of [`bind_hints`].
#[allow(
    dead_code,
    reason = "the binders of INSERT, UPDATE and DELETE do not read their target yet"
)]
pub(crate) fn bind_target_hints(
    hints: &[TableHint],
    ctx: &BindContext<'_>,
) -> SqlResult<LockHints> {
    if hints
        .iter()
        .any(|hint| Word::parse(&hint.name) == Some(Word::ReadUncommitted))
    {
        return Err(SqlError::nolock_not_allowed_on_target().with_line(TARGET_NOLOCK_LINE));
    }
    bind_hints(hints, ctx)
}

/// The line a 1065 carries (module documentation, `the_line_of_1065_is_fixed`).
const TARGET_NOLOCK_LINE: u32 = 15;

/// Reads the arguments of a `nom(…)` that `query::check_table_arguments` accepted as a
/// lone hint word: `t (NOLOCK)` gives the hints of `t WITH (NOLOCK)`.
///
/// The word sits under any number of parentheses, as the check that accepted it allows
/// (`tests/bind_hints.rs`, `a_hint_word_in_the_argument_form_reaches_the_scan`). An
/// argument that is not such a word has already answered 215 or the error of its own
/// binding; one that reaches this function anyway is dropped, as the argument form
/// dropped its arguments before the words were read.
///
/// # Errors
///
/// Those of [`bind_hints`]; a lone word cannot conflict with itself, which leaves the
/// internal error 50000 of `SNAPSHOT`.
pub(crate) fn bind_argument_hints(args: &[Expr], ctx: &BindContext<'_>) -> SqlResult<LockHints> {
    let hints: Vec<TableHint> = args.iter().filter_map(lone_word).collect();
    bind_hints(&hints, ctx)
}

/// The hint an argument spells when it is a bare, unqualified identifier under any number
/// of parentheses; `None` otherwise.
fn lone_word(arg: &Expr) -> Option<TableHint> {
    match arg {
        Expr::Nested(inner, _) => lone_word(inner),
        Expr::Column(column) if column.qualifier.is_none() => Some(TableHint {
            name: column.name.value.clone(),
            args: Vec::new(),
            span: column.span,
        }),
        _ => None,
    }
}

/// Whether `name` is a hint that is not a locking hint, ASCII case ignored.
fn is_non_locking(name: &str) -> bool {
    NON_LOCKING_WORDS
        .iter()
        .any(|word| word.eq_ignore_ascii_case(name))
}

/// The internal error of a word that is no table hint, on the line of the word.
fn unknown_hint(name: &str, span: Span) -> SqlError {
    bug(format!("{name} is not a table hint")).with_line(span.line)
}

/// The line of the first token after `span` in `text`: whitespace, `--` comments and
/// `/* */` comments are skipped, and the line counted from the one the span starts on.
///
/// The span of a hint word does not cross a line, so the count starts at `span.line`.
/// When the text ends before a token is found, the line of the last character is answered.
fn line_after(span: &Span, text: &str) -> u32 {
    let start = usize::try_from(span.offset.saturating_add(span.len)).unwrap_or(usize::MAX);
    let rest = text.get(start..).unwrap_or("");
    let mut line = span.line;
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                line = line.saturating_add(1);
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    if bytes[i] == b'\n' {
                        line = line.saturating_add(1);
                    }
                    i += 1;
                }
                i += 2;
            }
            _ => break,
        }
    }
    line
}

impl LockHints {
    /// Whether the hints ask for a lock on the whole table: `TABLOCK` or `TABLOCKX`.
    #[must_use]
    pub fn locks_the_table(&self) -> bool {
        self.tablock || self.tablockx
    }

    /// Whether the hints ask for an exclusive lock: `XLOCK` or `TABLOCKX`.
    #[must_use]
    pub fn locks_exclusively(&self) -> bool {
        self.xlock || self.tablockx
    }
}

#[cfg(test)]
mod tests {
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
}
