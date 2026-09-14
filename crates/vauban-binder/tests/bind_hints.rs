//! The locking hints of a table reference, read into the `LockHints` of its `Scan`: the
//! fourteen words and the flags they set, the two spellings of one meaning, the pairs that
//! answer 1047 and the line that error carries, the words that are refused without a
//! number of their own, and the words that are dropped.
//!
//! The double of the catalogue knows two tables in `master.dbo`:
//! `t (id int NOT NULL, a int NULL)` and `u (id int NOT NULL)`. The bound nodes derive no
//! `PartialEq`; `LockHints` does, so a scan's hints compare whole.

use vauban_binder::{
    BindContext, BoundStatement, CatalogView, ColumnBinding, LockHints, LogicalPlan, NoVariables,
    ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{SqlType, TypeInfo};

/// A catalogue of two tables in `master.dbo`: `t (id, a)` and `u (id)`.
struct TwoTables;

impl CatalogView for TwoTables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        database: &str,
        default_schema: &str,
    ) -> Option<ResolvedTable> {
        if name.server.is_some() {
            return None;
        }
        let part = |ident: Option<&Ident>, default: &str| {
            ident.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
        };
        if !part(name.database.as_ref(), database).eq_ignore_ascii_case("master")
            || !part(name.schema.as_ref(), default_schema).eq_ignore_ascii_case("dbo")
        {
            return None;
        }
        let (object, columns) = match name.name.value.to_ascii_lowercase().as_str() {
            "t" => (
                1,
                vec![
                    column(ColumnId(1), 0, "id", false),
                    column(ColumnId(2), 1, "a", true),
                ],
            ),
            "u" => (2, vec![column(ColumnId(1), 0, "id", false)]),
            _ => return None,
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns,
            kind: ResolvedTableKind::Table,
        })
    }
}

fn column(id: ColumnId, index: usize, name: &str, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: id,
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, nullable),
    }
}

/// Binds the statements of `text` in order against the two tables: the plan of the last
/// one, or the first error.
fn bind_query(text: &str) -> Result<LogicalPlan, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let catalog = TwoTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    let mut last = None;
    for statement in &batch.statements {
        last = Some(match bind(statement, &ctx)? {
            BoundStatement::Query(plan) => *plan,
            other => panic!("{text}: not a query: {other:?}"),
        });
    }
    Ok(last.expect("one statement"))
}

#[track_caller]
fn plan_of(text: &str) -> LogicalPlan {
    bind_query(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

#[track_caller]
fn error_of(text: &str) -> SqlError {
    bind_query(text).expect_err(text)
}

/// The scans of a plan, in the order they are reached: the left input of a join before
/// the right one.
fn scans(plan: &LogicalPlan) -> Vec<(&str, &LockHints)> {
    match plan {
        LogicalPlan::Scan { alias, hints, .. } => vec![(alias.as_str(), hints)],
        LogicalPlan::Join { left, right, .. } => {
            let mut out = scans(left);
            out.extend(scans(right));
            out
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. } => scans(input),
        LogicalPlan::Distinct(input) => scans(input),
        other => panic!("no scan under {other:?}"),
    }
}

/// The hints of the single scan of `text`.
#[track_caller]
fn hints_of(text: &str) -> LockHints {
    let plan = plan_of(text);
    let found = scans(&plan);
    assert_eq!(found.len(), 1, "{text}: one scan expected, got {found:?}");
    *found[0].1
}

/// The hints of `SELECT id FROM dbo.t WITH (<list>) WHERE id = 1`.
#[track_caller]
fn with(list: &str) -> LockHints {
    hints_of(&format!("SELECT id FROM dbo.t WITH ({list}) WHERE id = 1"))
}

/// The error of `SELECT id FROM dbo.t WITH (<list>) WHERE id = 1`.
#[track_caller]
fn error_with(list: &str) -> SqlError {
    error_of(&format!("SELECT id FROM dbo.t WITH ({list}) WHERE id = 1"))
}

// ---------------------------------------------------------------------------------------
// One word, one flag
// ---------------------------------------------------------------------------------------

#[test]
fn nolock_sets_read_uncommitted_on_the_scan() {
    assert_eq!(
        with("NOLOCK"),
        LockHints {
            nolock: true,
            ..LockHints::default()
        }
    );
    assert_eq!(with("READUNCOMMITTED"), with("NOLOCK"));
    assert_eq!(with("nolock"), with("NOLOCK"));
    assert_eq!(with("NoLock"), with("NOLOCK"));
    assert_eq!(
        with("NOLOCK, NOWAIT"),
        LockHints {
            nolock: true,
            nowait: true,
            ..LockHints::default()
        }
    );
    assert_eq!(
        with("UPDLOCK, ROWLOCK, NOWAIT"),
        LockHints {
            updlock: true,
            rowlock: true,
            nowait: true,
            ..LockHints::default()
        }
    );
    assert_eq!(
        hints_of("SELECT id FROM dbo.t"),
        LockHints::default(),
        "a reference without a hint list"
    );
}

/// The fourteen locking words, each on a flag of `LockHints`. A word that is carried
/// without effect for now (`ROWLOCK`, which asks for the granularity the engine locks at
/// anyway; `PAGLOCK`, `TABLOCK`, `TABLOCKX`, which wait for a lock manager that knows a
/// page or a table) still sets its flag: the plan says what was asked for.
#[test]
fn each_of_the_fourteen_words_sets_its_flag() {
    let d = LockHints::default;
    let expected: [(&str, LockHints); 14] = [
        (
            "NOLOCK",
            LockHints {
                nolock: true,
                ..d()
            },
        ),
        (
            "READUNCOMMITTED",
            LockHints {
                nolock: true,
                ..d()
            },
        ),
        (
            "READCOMMITTED",
            LockHints {
                readcommitted: true,
                ..d()
            },
        ),
        (
            "REPEATABLEREAD",
            LockHints {
                repeatableread: true,
                ..d()
            },
        ),
        (
            "SERIALIZABLE",
            LockHints {
                serializable: true,
                ..d()
            },
        ),
        (
            "HOLDLOCK",
            LockHints {
                serializable: true,
                ..d()
            },
        ),
        (
            "UPDLOCK",
            LockHints {
                updlock: true,
                ..d()
            },
        ),
        ("XLOCK", LockHints { xlock: true, ..d() }),
        (
            "TABLOCK",
            LockHints {
                tablock: true,
                ..d()
            },
        ),
        (
            "TABLOCKX",
            LockHints {
                tablockx: true,
                ..d()
            },
        ),
        (
            "READPAST",
            LockHints {
                readpast: true,
                ..d()
            },
        ),
        (
            "ROWLOCK",
            LockHints {
                rowlock: true,
                ..d()
            },
        ),
        (
            "PAGLOCK",
            LockHints {
                paglock: true,
                ..d()
            },
        ),
        (
            "NOWAIT",
            LockHints {
                nowait: true,
                ..d()
            },
        ),
    ];
    for (word, hints) in expected {
        assert_eq!(with(word), hints, "{word}");
        assert_ne!(with(word), d(), "{word} sets a flag");
    }
}

/// `HOLDLOCK` is the `SERIALIZABLE` hint under its older name, as the table hints page
/// of the T-SQL reference (`hints-transact-sql-table`) states under both words: one flag,
/// and no conflict between the two spellings.
#[test]
fn holdlock_is_serializable() {
    assert_eq!(with("HOLDLOCK"), with("SERIALIZABLE"));
    assert!(with("HOLDLOCK").serializable);
    assert_eq!(with("SERIALIZABLE, HOLDLOCK"), with("HOLDLOCK"));
    assert_eq!(with("HOLDLOCK, SERIALIZABLE"), with("HOLDLOCK"));
}

/// `TABLOCKX` asks for an exclusive lock on the whole table, which the same page
/// (`hints-transact-sql-table`) describes as what `TABLOCK` together with `XLOCK` asks for:
/// the two accessors read both spellings alike, while the flag keeps the word written.
#[test]
fn tablockx_is_table_plus_exclusive() {
    let tablockx = with("TABLOCKX");
    assert!(tablockx.tablockx && !tablockx.tablock && !tablockx.xlock);
    assert!(tablockx.locks_the_table() && tablockx.locks_exclusively());

    let spelled_out = with("TABLOCK, XLOCK");
    assert!(spelled_out.tablock && spelled_out.xlock && !spelled_out.tablockx);
    assert!(spelled_out.locks_the_table() && spelled_out.locks_exclusively());

    let shared = with("TABLOCK");
    assert!(shared.locks_the_table() && !shared.locks_exclusively());
    let row = with("XLOCK, ROWLOCK");
    assert!(!row.locks_the_table() && row.locks_exclusively());
    assert!(!LockHints::default().locks_the_table() && !LockHints::default().locks_exclusively());
}

// ---------------------------------------------------------------------------------------
// 1047
// ---------------------------------------------------------------------------------------

/// The pairs that answer 1047, severity 15, state 1, in both orders.
#[test]
fn conflicting_hints_are_1047() {
    let pairs = [
        // two isolation words of different meaning
        ("NOLOCK", "READCOMMITTED"),
        ("NOLOCK", "REPEATABLEREAD"),
        ("NOLOCK", "SERIALIZABLE"),
        ("NOLOCK", "HOLDLOCK"),
        ("READUNCOMMITTED", "READCOMMITTED"),
        ("READUNCOMMITTED", "HOLDLOCK"),
        ("READCOMMITTED", "REPEATABLEREAD"),
        ("READCOMMITTED", "SERIALIZABLE"),
        ("READCOMMITTED", "HOLDLOCK"),
        ("REPEATABLEREAD", "SERIALIZABLE"),
        ("REPEATABLEREAD", "HOLDLOCK"),
        ("READCOMMITTEDLOCK", "READCOMMITTED"),
        ("READCOMMITTEDLOCK", "NOLOCK"),
        ("READCOMMITTEDLOCK", "REPEATABLEREAD"),
        ("READCOMMITTEDLOCK", "SERIALIZABLE"),
        ("SNAPSHOT", "NOLOCK"),
        ("SNAPSHOT", "READCOMMITTED"),
        ("SNAPSHOT", "REPEATABLEREAD"),
        ("SNAPSHOT", "SERIALIZABLE"),
        ("SNAPSHOT", "HOLDLOCK"),
        ("SNAPSHOT", "READCOMMITTEDLOCK"),
        // NOLOCK with a lock it says it does not take
        ("NOLOCK", "UPDLOCK"),
        ("NOLOCK", "XLOCK"),
        ("NOLOCK", "ROWLOCK"),
        ("NOLOCK", "PAGLOCK"),
        ("NOLOCK", "TABLOCK"),
        ("NOLOCK", "TABLOCKX"),
        ("READUNCOMMITTED", "UPDLOCK"),
        ("READUNCOMMITTED", "TABLOCKX"),
        // two granularities
        ("ROWLOCK", "PAGLOCK"),
        ("ROWLOCK", "TABLOCK"),
        ("ROWLOCK", "TABLOCKX"),
        ("PAGLOCK", "TABLOCK"),
        ("PAGLOCK", "TABLOCKX"),
        ("TABLOCK", "TABLOCKX"),
        // two lock modes
        ("UPDLOCK", "XLOCK"),
    ];
    for (a, b) in pairs {
        for list in [format!("{a}, {b}"), format!("{b}, {a}")] {
            let err = error_with(&list);
            assert_eq!(
                (err.number, err.severity, err.state),
                (1047, 15, 1),
                "{list}: {err:?}"
            );
        }
    }
    // A third word does not hide the pair, wherever it sits.
    for list in [
        "NOLOCK, ROWLOCK, NOWAIT",
        "NOLOCK, READPAST, ROWLOCK",
        "UPDLOCK, XLOCK, TABLOCK",
        "NOWAIT, UPDLOCK, ROWLOCK, XLOCK",
    ] {
        assert_eq!(error_with(list).number, 1047, "{list}");
    }
}

/// The pairs that bind: a word with itself, the two spellings of a meaning, an isolation
/// word other than `NOLOCK` with a granularity or a lock mode, `READPAST` and `NOWAIT`
/// with what they do not contradict.
#[test]
fn accepted_pairs_bind() {
    let lists = [
        "NOLOCK, NOLOCK",
        "NOLOCK, READUNCOMMITTED",
        "NOLOCK, NOWAIT",
        "SERIALIZABLE, HOLDLOCK",
        "TABLOCK, TABLOCK",
        "HOLDLOCK, UPDLOCK",
        "HOLDLOCK, XLOCK",
        "HOLDLOCK, TABLOCK",
        "HOLDLOCK, TABLOCKX",
        "HOLDLOCK, PAGLOCK",
        "HOLDLOCK, ROWLOCK",
        "HOLDLOCK, NOWAIT",
        "READCOMMITTED, UPDLOCK",
        "READCOMMITTED, XLOCK",
        "READCOMMITTED, TABLOCKX",
        "READCOMMITTED, READPAST",
        "READCOMMITTEDLOCK, UPDLOCK",
        "READCOMMITTEDLOCK, TABLOCKX",
        "READCOMMITTEDLOCK, READPAST",
        "REPEATABLEREAD, UPDLOCK",
        "REPEATABLEREAD, TABLOCK",
        "REPEATABLEREAD, READPAST",
        "UPDLOCK, ROWLOCK",
        "UPDLOCK, PAGLOCK",
        "UPDLOCK, TABLOCK",
        "UPDLOCK, TABLOCKX",
        "UPDLOCK, READPAST",
        "UPDLOCK, NOWAIT",
        "XLOCK, ROWLOCK",
        "XLOCK, PAGLOCK",
        "XLOCK, TABLOCK",
        "XLOCK, TABLOCKX",
        "XLOCK, READPAST",
        "XLOCK, NOWAIT",
        "READPAST, ROWLOCK",
        "READPAST, PAGLOCK",
        "READPAST, TABLOCK",
        "READPAST, TABLOCKX",
        "READPAST, NOWAIT",
        "ROWLOCK, NOWAIT",
        "PAGLOCK, NOWAIT",
        "TABLOCK, NOWAIT",
        "TABLOCKX, NOWAIT",
        "UPDLOCK, HOLDLOCK, ROWLOCK",
        "TABLOCKX, HOLDLOCK, NOWAIT",
        "READPAST, UPDLOCK, ROWLOCK",
    ];
    for list in lists {
        let hints = with(list);
        assert_ne!(hints, LockHints::default(), "{list}");
    }
    assert_eq!(
        with("READPAST, UPDLOCK, ROWLOCK"),
        LockHints {
            readpast: true,
            updlock: true,
            rowlock: true,
            ..LockHints::default()
        }
    );
    assert_eq!(with("READCOMMITTEDLOCK"), with("READCOMMITTED"));
}

/// The hints are read before the name is resolved, and reference by reference in `FROM`
/// order: 1047 beats the 208 of the same reference and the 207 of the select list.
#[test]
fn conflicting_hints_come_before_208() {
    assert_eq!(
        error_of("SELECT id FROM dbo.nosuch WITH (NOLOCK, TABLOCKX)").number,
        1047
    );
    assert_eq!(
        error_of("SELECT nosuch FROM dbo.t WITH (NOLOCK, TABLOCKX) WHERE id = 1").number,
        1047
    );
    assert_eq!(
        error_of(
            "SELECT t.id FROM dbo.t WITH (NOLOCK, TABLOCKX) JOIN dbo.u WITH (FOO) ON t.id = u.id"
        )
        .number,
        1047,
        "the first reference is read first"
    );
    assert_eq!(
        error_of(
            "SELECT t.id FROM dbo.t WITH (FOO) JOIN dbo.u WITH (NOLOCK, TABLOCKX) ON t.id = u.id"
        )
        .number,
        50000,
        "the unknown word of the first reference comes before the pair of the second"
    );
}

/// The line of a 1047 is the line of the token after the second word of the pair: the
/// comma or the closing parenthesis that follows it, blank lines and comments skipped.
#[test]
fn the_line_of_1047_is_the_token_after_the_word() {
    let shapes = [
        (
            "SELECT id FROM dbo.t WITH (NOLOCK, TABLOCKX) WHERE id = 1",
            1,
        ),
        (
            "SELECT id\nFROM dbo.t\nWITH (NOLOCK, TABLOCKX)\nWHERE id = 1",
            3,
        ),
        (
            "SELECT id\nFROM dbo.t\nWITH (NOLOCK,\nTABLOCKX)\nWHERE id = 1",
            4,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK,\nTABLOCKX\n) WHERE id = 1",
            3,
        ),
        (
            "SELECT id FROM dbo.t WITH (TABLOCKX,\nNOLOCK) WHERE id = 1",
            2,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK, TABLOCKX,\nNOWAIT) WHERE id = 1",
            1,
        ),
        (
            "SELECT id FROM dbo.t\nWITH\n(NOLOCK, TABLOCKX) WHERE id = 1",
            3,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK,\nTABLOCKX,\nNOWAIT\n) WHERE id = 1",
            2,
        ),
        (
            "SELECT id FROM dbo.t WITH (\nNOLOCK,\nTABLOCKX) WHERE id = 1",
            3,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK, TABLOCKX)\n\n\nWHERE id = 1",
            1,
        ),
        (
            "SELECT id FROM dbo.t WITH (UPDLOCK,\nROWLOCK,\nXLOCK) WHERE id = 1",
            3,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK, TABLOCKX\n\n\n) WHERE id = 1",
            4,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK, TABLOCKX -- c\n\n) WHERE id = 1",
            3,
        ),
        (
            "SELECT id FROM dbo.t WITH (NOLOCK, TABLOCKX\n\n, NOWAIT) WHERE id = 1",
            3,
        ),
        (
            "SELECT z.id FROM dbo.t\nAS z\nWITH (NOLOCK, TABLOCKX)\nWHERE z.id = 1",
            3,
        ),
        (
            "SELECT 1 AS n;\nSELECT id\nFROM dbo.t\nWITH (NOLOCK, TABLOCKX)\nWHERE id = 1",
            4,
        ),
        (
            "SELECT t.id\nFROM dbo.t\nJOIN dbo.u\nWITH (NOLOCK, TABLOCKX)\nON t.id = u.id",
            4,
        ),
    ];
    for (text, line) in shapes {
        let err = error_of(text);
        assert_eq!((err.number, err.line), (1047, line), "{text:?}");
    }
}

// ---------------------------------------------------------------------------------------
// Refused without a number of their own
// ---------------------------------------------------------------------------------------

/// A word that is no table hint is refused with the internal error, on the line of the
/// word, and before a pair of the same list is looked at.
#[test]
fn an_unknown_word_under_with_is_refused() {
    for list in ["FOO", "foo", "NOLOCK, FOO", "FOO, NOLOCK"] {
        let err = error_with(list);
        assert_eq!(err.number, 50000, "{list}: {err:?}");
        assert!(
            err.message.contains("is not a table hint"),
            "{list}: {err:?}"
        );
    }
    let err = error_of("SELECT id FROM dbo.t WITH (NOLOCK,\nFOO) WHERE id = 1");
    assert_eq!((err.number, err.line), (50000, 2));
    let err = error_of("SELECT id FROM dbo.t WITH (NOLOCK, FOO\n\n) WHERE id = 1");
    assert_eq!((err.number, err.line), (50000, 1));
}

#[test]
fn an_unknown_word_wins_over_a_conflict() {
    for list in [
        "FOO, NOLOCK, TABLOCKX",
        "NOLOCK, TABLOCKX, FOO",
        "NOLOCK, READPAST, FOO",
    ] {
        let err = error_with(list);
        assert_eq!(err.number, 50000, "{list}: {err:?}");
        assert!(err.message.contains("FOO"), "{list}: {err:?}");
    }
}

/// `READPAST` next to `NOLOCK`, `READUNCOMMITTED`, `SERIALIZABLE` or `HOLDLOCK` is
/// refused with the internal error; a 1047 pair of the same list comes first.
#[test]
fn readpast_under_nolock_or_serializable_is_refused() {
    for list in [
        "NOLOCK, READPAST",
        "READPAST, NOLOCK",
        "READUNCOMMITTED, READPAST",
        "SERIALIZABLE, READPAST",
        "HOLDLOCK, READPAST",
    ] {
        let err = error_with(list);
        assert_eq!(err.number, 50000, "{list}: {err:?}");
        assert!(err.message.contains("READPAST"), "{list}: {err:?}");
    }
    for list in ["READPAST, NOLOCK, ROWLOCK", "NOLOCK, ROWLOCK, READPAST"] {
        assert_eq!(error_with(list).number, 1047, "{list}");
    }
    assert!(with("READCOMMITTED, READPAST").readpast);
    assert!(with("REPEATABLEREAD, READPAST").readpast);
}

/// `SNAPSHOT` is refused with the internal error on its own or next to a word it does not
/// conflict with; next to another isolation word, the pair answers 1047 first.
#[test]
fn snapshot_is_refused() {
    for list in [
        "SNAPSHOT",
        "SNAPSHOT, NOWAIT",
        "SNAPSHOT, ROWLOCK",
        "UPDLOCK, SNAPSHOT",
    ] {
        let err = error_with(list);
        assert_eq!(err.number, 50000, "{list}: {err:?}");
        assert!(err.message.contains("SNAPSHOT"), "{list}: {err:?}");
    }
    assert_eq!(error_with("SNAPSHOT, NOLOCK").number, 1047);
}

// ---------------------------------------------------------------------------------------
// Dropped, and the argument form
// ---------------------------------------------------------------------------------------

/// The hints that are not locking hints leave the `LockHints` alone and do not hide the
/// locking words next to them.
#[test]
fn non_locking_hints_are_dropped() {
    for list in [
        "INDEX(0)",
        "INDEX(1)",
        "INDEX(nosuch)",
        "FORCESEEK",
        "FORCESCAN",
        "NOEXPAND",
        "KEEPIDENTITY",
        "KEEPDEFAULTS",
        "IGNORE_CONSTRAINTS",
        "IGNORE_TRIGGERS",
        "SPATIAL_WINDOW_MAX_CELLS(1)",
        "index(0)",
    ] {
        assert_eq!(with(list), LockHints::default(), "{list}");
    }
    assert_eq!(with("NOLOCK, INDEX(0)"), with("NOLOCK"));
    assert_eq!(with("INDEX(0), NOLOCK"), with("NOLOCK"));
    assert_eq!(error_with("INDEX(0), NOLOCK, TABLOCKX").number, 1047);
}

/// `t (NOLOCK)`, the argument form that the binder re-reads as a hint, reaches the scan
/// with the same flags as `t WITH (NOLOCK)`, under any number of parentheses and whatever
/// the case or the delimiters of the word (`tests/bind_hints.rs`, this test).
#[test]
fn a_hint_word_in_the_argument_form_reaches_the_scan() {
    for text in [
        "SELECT id FROM dbo.t (NOLOCK) WHERE id = 1",
        "SELECT id FROM t (nolock) WHERE id = 1",
        "SELECT id FROM t ([NOLOCK]) WHERE id = 1",
        "SELECT id FROM t ((NOLOCK)) WHERE id = 1",
        "SELECT id FROM t(NOLOCK) WHERE id = 1",
        "SELECT z.id FROM t (NOLOCK) AS z WHERE z.id = 1",
    ] {
        assert_eq!(hints_of(text), with("NOLOCK"), "{text}");
    }
    assert_eq!(
        hints_of("SELECT id FROM dbo.t (TABLOCKX) WHERE id = 1"),
        with("TABLOCKX")
    );
    assert_eq!(
        hints_of("SELECT id FROM dbo.t (READPAST) WHERE id = 1"),
        with("READPAST")
    );
    assert_eq!(
        hints_of("SELECT id FROM dbo.t (NOEXPAND) WHERE id = 1"),
        LockHints::default()
    );
}

/// The argument form that is no hint still answers 215, and its neighbours their own
/// numbers: reading the hints did not move the re-reading of the arguments.
#[test]
fn unknown_hint_still_is_215() {
    for text in [
        "SELECT id FROM dbo.t (1)",
        "SELECT id FROM dbo.t ()",
        "SELECT id FROM dbo.t ('NOLOCK')",
        "SELECT id FROM dbo.t (NULL)",
        "SELECT id FROM dbo.t (1, 2)",
    ] {
        let err = error_of(text);
        assert_eq!(
            (err.number, err.severity, err.state),
            (215, 16, 1),
            "{text}"
        );
    }
    assert_eq!(error_of("SELECT id FROM dbo.t (x)").number, 207);
    assert_eq!(error_of("SELECT id FROM dbo.t (NOLOCK, 1)").number, 207);
    assert_eq!(error_of("SELECT id FROM dbo.t (NOLOCK + 0)").number, 207);
    assert_eq!(error_of("SELECT id FROM dbo.t (dbo.NOLOCK)").number, 4104);
    assert_eq!(error_of("SELECT id FROM dbo.nosuch (NOLOCK)").number, 208);
    assert_eq!(
        error_of("SELECT id FROM dbo.nosuch WITH (NOLOCK)").number,
        208
    );
}

// ---------------------------------------------------------------------------------------
// Joins
// ---------------------------------------------------------------------------------------

/// Each reference of a join carries its own hints on its own scan; a reference without a
/// list carries the default.
#[test]
fn hints_reach_the_scan_of_a_joined_table() {
    let plan = plan_of(
        "SELECT t.id FROM dbo.t WITH (NOLOCK) JOIN dbo.u WITH (TABLOCK, HOLDLOCK) ON t.id = u.id",
    );
    let joined = scans(&plan);
    assert_eq!(joined.len(), 2, "{joined:?}");
    assert_eq!(joined[0].0, "t");
    assert_eq!(
        *joined[0].1,
        LockHints {
            nolock: true,
            ..LockHints::default()
        }
    );
    assert_eq!(joined[1].0, "u");
    assert_eq!(
        *joined[1].1,
        LockHints {
            tablock: true,
            serializable: true,
            ..LockHints::default()
        }
    );

    let plan = plan_of("SELECT t.id FROM dbo.t, dbo.u WITH (UPDLOCK) WHERE t.id = u.id");
    let comma = scans(&plan);
    assert_eq!(*comma[0].1, LockHints::default());
    assert!(comma[1].1.updlock);

    let plan = plan_of("SELECT z.id FROM dbo.t AS z (ROWLOCK) JOIN dbo.u (NOLOCK) ON z.id = u.id");
    let deprecated = scans(&plan);
    assert!(deprecated[0].1.rowlock && deprecated[1].1.nolock);

    let err = error_of("SELECT t.id FROM dbo.t JOIN dbo.u WITH (NOLOCK, TABLOCKX) ON t.id = u.id");
    assert_eq!(err.number, 1047);
}
