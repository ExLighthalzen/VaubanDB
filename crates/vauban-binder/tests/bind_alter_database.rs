//! `ALTER DATABASE … SET` of the two versioning options, bound: what
//! [`DdlStatement::AlterDatabase`] carries, and what the binder refuses.
//!
//! `ddl.rs` binds `READ_COMMITTED_SNAPSHOT` and `ALLOW_SNAPSHOT_ISOLATION`, `ON` or `OFF`,
//! with or without a termination clause, and nothing else of `ALTER DATABASE`. The tests
//! below read the bound shape of each form, then the refusals: the 102 of a malformed value
//! or clause, and the internal 50000 that names the option it does not bind.
//!
//! The database is not resolved here: whether it exists is answered while the statement
//! runs (`ddl.rs`, module header), so a database the context knows nothing about binds like
//! the current one.

use vauban_binder::{BindContext, BoundStatement, CatalogView, DdlStatement, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};

/// A catalogue view that knows nothing: the shape the production context hands over is
/// what matters here, not what it resolves.
struct EmptyCatalog;

impl CatalogView for EmptyCatalog {}

/// Binds the single statement of `text` with `database` as the current database.
fn bind_in(text: &str, database: &str) -> Result<BoundStatement, SqlError> {
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|error| panic!("{text} parses, got {}: {}", error.number, error.message));
    let [statement] = batch.statements.as_slice() else {
        panic!("{text} is one statement, got {}", batch.statements.len())
    };
    let catalog = EmptyCatalog;
    let mut ctx = BindContext::scalar(text, SessionOptions::default());
    ctx.catalog = Some(&catalog);
    ctx.database = database;
    bind(statement, &ctx)
}

/// The database and the options `text` binds to, when it binds to one
/// [`DdlStatement::AlterDatabase`], with `mydb` as the current database.
///
/// # Panics
///
/// When the text binds to anything else.
fn alter(text: &str) -> (String, Vec<(String, Option<String>)>) {
    match bind_in(text, "mydb") {
        Ok(BoundStatement::Ddl(DdlStatement::AlterDatabase { name, options })) => (name, options),
        other => panic!("{text} binds to an AlterDatabase, got {other:?}"),
    }
}

/// The error `text` answers, be it the parser's or the binder's.
///
/// # Panics
///
/// When the text binds: a test that calls this expects a refusal.
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
    panic!("{text} binds; a refusal was expected")
}

/// A pair of the option list, as the bound statement carries it.
fn pair(name: &str, value: &str) -> (String, Option<String>) {
    (name.to_owned(), Some(value.to_owned()))
}

#[test]
fn rcsi_on_binds() {
    let (name, options) = alter("ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON;");
    assert_eq!(name, "d");
    assert_eq!(options, [pair("READ_COMMITTED_SNAPSHOT", "ON")]);
    let (_, with_termination) =
        alter("ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;");
    assert_eq!(
        with_termination,
        [
            pair("READ_COMMITTED_SNAPSHOT", "ON"),
            pair("WITH", "ROLLBACK IMMEDIATE")
        ],
        "the termination mode is the second pair"
    );
}

#[test]
fn allow_snapshot_isolation_off_binds() {
    let (name, options) = alter("ALTER DATABASE d SET ALLOW_SNAPSHOT_ISOLATION OFF;");
    assert_eq!(name, "d");
    assert_eq!(options, [pair("ALLOW_SNAPSHOT_ISOLATION", "OFF")]);
    // The four combinations bind, and the value is upper-cased as the name is.
    for (option, value) in [
        ("READ_COMMITTED_SNAPSHOT", "OFF"),
        ("ALLOW_SNAPSHOT_ISOLATION", "ON"),
        ("read_committed_snapshot", "on"),
        ("allow_snapshot_isolation", "off"),
    ] {
        let (_, options) = alter(&format!("ALTER DATABASE d SET {option} {value};"));
        assert_eq!(
            options,
            [pair(
                &option.to_ascii_uppercase(),
                &value.to_ascii_uppercase()
            )],
            "{option} {value}"
        );
    }
}

/// The termination clause is carried for both options, in its three modes, and the clause
/// is normalised: `ROLLBACK AFTER 5` comes back with its `SECONDS`.
#[test]
fn with_rollback_immediate_is_kept() {
    for (written, carried) in [
        ("WITH ROLLBACK IMMEDIATE", "ROLLBACK IMMEDIATE"),
        ("with rollback immediate", "ROLLBACK IMMEDIATE"),
        ("WITH ROLLBACK AFTER 5 SECONDS", "ROLLBACK AFTER 5 SECONDS"),
        ("WITH ROLLBACK AFTER 5", "ROLLBACK AFTER 5 SECONDS"),
        ("WITH ROLLBACK AFTER 0", "ROLLBACK AFTER 0 SECONDS"),
        ("WITH NO_WAIT", "NO_WAIT"),
    ] {
        for option in ["READ_COMMITTED_SNAPSHOT", "ALLOW_SNAPSHOT_ISOLATION"] {
            let text = format!("ALTER DATABASE d SET {option} ON {written};");
            let (_, options) = alter(&text);
            assert_eq!(
                options,
                [pair(option, "ON"), pair("WITH", carried)],
                "{text}"
            );
        }
    }
    // Without a clause, the list holds the option and nothing else.
    let (_, options) = alter("ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT OFF;");
    assert_eq!(options.len(), 1);
}

#[test]
fn current_resolves_to_the_context_database() {
    let (name, _) = alter("ALTER DATABASE CURRENT SET READ_COMMITTED_SNAPSHOT ON;");
    assert_eq!(name, "mydb");
    let (name, _) = alter("alter database current set read_committed_snapshot on;");
    assert_eq!(name, "mydb", "the keyword is read in any case");
    // The context decides: the same text under another database binds that one.
    match bind_in(
        "ALTER DATABASE CURRENT SET READ_COMMITTED_SNAPSHOT ON;",
        "other",
    ) {
        Ok(BoundStatement::Ddl(DdlStatement::AlterDatabase { name, .. })) => {
            assert_eq!(name, "other");
        }
        other => panic!("binds to an AlterDatabase, got {other:?}"),
    }
}

/// `[CURRENT]` and `"CURRENT"` name a database called `CURRENT`: the keyword is the bare
/// spelling alone. This is the counter-proof of `current_resolves_to_the_context_database`.
#[test]
fn current_between_delimiters_is_a_database_name() {
    let (name, _) = alter("ALTER DATABASE [CURRENT] SET READ_COMMITTED_SNAPSHOT ON;");
    assert_eq!(name, "CURRENT");
    let (name, _) = alter("ALTER DATABASE \"current\" SET READ_COMMITTED_SNAPSHOT ON;");
    assert_eq!(name, "current");
}

/// A database the context does not know binds: its absence is answered while the statement
/// runs (5011, severity 14, state 5, the batch going on after it), not here.
#[test]
fn unknown_database_is_left_to_execution() {
    let (name, options) = alter("ALTER DATABASE no_such_db SET READ_COMMITTED_SNAPSHOT ON;");
    assert_eq!(name, "no_such_db");
    assert_eq!(options, [pair("READ_COMMITTED_SNAPSHOT", "ON")]);
    let (name, _) = alter("ALTER DATABASE [no such db] SET ALLOW_SNAPSHOT_ISOLATION OFF;");
    assert_eq!(name, "no such db", "the name is carried unquoted");
}

/// The internal 50000 quotes the option as written, and names the version that takes it.
#[test]
fn other_set_option_names_itself_and_its_version() {
    for (text, written) in [
        ("ALTER DATABASE d SET MULTI_USER;", "MULTI_USER"),
        ("ALTER DATABASE d SET RECOVERY SIMPLE;", "RECOVERY SIMPLE"),
        (
            "ALTER DATABASE d SET COMPATIBILITY_LEVEL = 150;",
            "COMPATIBILITY_LEVEL = 150",
        ),
        ("ALTER DATABASE d SET ONLINE;", "ONLINE"),
        ("ALTER DATABASE d SET read_only;", "read_only"),
        (
            "ALTER DATABASE d SET NO_SUCH_OPTION ON;",
            "NO_SUCH_OPTION ON",
        ),
        // A versioning option next to another option is refused naming the other one.
        (
            "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON, RECOVERY SIMPLE;",
            "RECOVERY SIMPLE",
        ),
        (
            "ALTER DATABASE d SET RECOVERY SIMPLE, READ_COMMITTED_SNAPSHOT ON;",
            "RECOVERY SIMPLE",
        ),
    ] {
        let error = refusal(text);
        assert_eq!(error.number, 50000, "{text}");
        assert!(
            error.message.contains(&format!("SET {written} ")),
            "{text} names the option: {}",
            error.message
        );
        assert!(error.message.contains("V2"), "{text}: {}", error.message);
    }
}

/// The two versioning options in one statement, or one of them twice, are refused naming
/// the list.
#[test]
fn two_versioning_options_in_one_statement_are_refused() {
    for text in [
        "ALTER DATABASE d SET ALLOW_SNAPSHOT_ISOLATION ON, READ_COMMITTED_SNAPSHOT ON;",
        "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON, READ_COMMITTED_SNAPSHOT ON;",
    ] {
        let error = refusal(text);
        assert_eq!(error.number, 50000, "{text}");
        assert!(
            error
                .message
                .contains("READ_COMMITTED_SNAPSHOT ON, READ_COMMITTED_SNAPSHOT ON")
                || error
                    .message
                    .contains("ALLOW_SNAPSHOT_ISOLATION ON, READ_COMMITTED_SNAPSHOT ON"),
            "{text}: {}",
            error.message
        );
    }
}

#[test]
fn alter_database_add_file_names_v2() {
    for (text, written) in [
        (
            "ALTER DATABASE d ADD FILE (NAME = f1, FILENAME = 'c:\\f1.ndf');",
            "ADD FILE ( NAME = f1 , FILENAME = 'c:\\f1.ndf' )",
        ),
        ("ALTER DATABASE d MODIFY NAME = e;", "MODIFY NAME = e"),
        ("ALTER DATABASE d REMOVE FILE f1;", "REMOVE FILE f1"),
        (
            "ALTER DATABASE d COLLATE Latin1_General_CI_AS;",
            "COLLATE Latin1_General_CI_AS",
        ),
    ] {
        let error = refusal(text);
        assert_eq!(error.number, 50000, "{text}");
        assert!(
            error.message.contains(written) && error.message.contains("V2"),
            "{text}: {}",
            error.message
        );
        assert!(
            !error.message.contains("SET"),
            "{text} is not a SET form: {}",
            error.message
        );
    }
}

/// A value that is not `ON` or `OFF`, or no value at all, is 102 severity 15; a bare word
/// in that position, or the option name when the value is missing or starts with `=`, is
/// quoted with state 6, and any other token with state 1.
#[test]
fn a_malformed_value_is_102() {
    for (text, near, state) in [
        (
            "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT MAYBE;",
            "MAYBE",
            6,
        ),
        (
            "ALTER DATABASE d SET ALLOW_SNAPSHOT_ISOLATION TRUE;",
            "TRUE",
            6,
        ),
        (
            "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT;",
            "READ_COMMITTED_SNAPSHOT",
            6,
        ),
        (
            "ALTER DATABASE d SET ALLOW_SNAPSHOT_ISOLATION = OFF;",
            "ALLOW_SNAPSHOT_ISOLATION",
            6,
        ),
        ("ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT 1;", "1", 1),
        (
            "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT 'ON';",
            "ON",
            1,
        ),
        (
            "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT [ON];",
            "ON",
            1,
        ),
        (
            "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON foo;",
            "foo",
            1,
        ),
    ] {
        let error = refusal(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (102, 15, state),
            "{text}: {}",
            error.message
        );
        assert!(
            error.message.contains(&format!("'{near}'")),
            "{text} quotes {near}: {}",
            error.message
        );
        assert_eq!(error.line, 1, "{text}");
    }
}

/// A termination clause that is not one of the three modes is 102 state 1, quoting the
/// token that does not fit; a clause cut short quotes what SQL Server meets next.
#[test]
fn a_malformed_termination_clause_is_102() {
    let base = "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON";
    for (tail, near) in [
        ("WITH FOO", "FOO"),
        ("WITH ROLLBACK AFTER x", "x"),
        ("WITH ROLLBACK AFTER -1", "-"),
        ("WITH ROLLBACK AFTER 1.5", "1.5"),
        ("WITH ROLLBACK AFTER 5 MINUTES", "MINUTES"),
        ("WITH ROLLBACK AFTER 5 SECONDS foo", "foo"),
        ("WITH NO_WAIT foo", "foo"),
        ("WITH ROLLBACK AFTER", "AFTER"),
        ("WITH", ";"),
        ("WITH ROLLBACK", ";"),
        ("WITH ROLLBACK IMMEDIATE, ALLOW_SNAPSHOT_ISOLATION ON", ","),
    ] {
        let text = format!("{base} {tail};");
        let error = refusal(&text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (102, 15, 1),
            "{text}: {}",
            error.message
        );
        assert!(
            error.message.contains(&format!("'{near}'")),
            "{text} quotes {near}: {}",
            error.message
        );
    }
}

/// The line of a 102 raised here is the statement's, on a statement written on one line;
/// the executor and the session put the statement's line on their own errors the same way.
#[test]
fn the_102_carries_the_line_of_the_statement() {
    let error =
        refusal("SELECT 1;\nSELECT 2;\nALTER DATABASE d SET READ_COMMITTED_SNAPSHOT MAYBE;");
    assert_eq!((error.number, error.line), (102, 3));
}

/// An `ALTER DATABASE … SET` of a versioning option is a `Ddl`, and neither a `Query` nor
/// a `Use`: the dispatch sends it here.
#[test]
fn the_statement_binds_to_a_ddl() {
    let bound = bind_in("ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON;", "mydb")
        .unwrap_or_else(|error| panic!("binds, got {}: {}", error.number, error.message));
    assert!(
        matches!(
            bound,
            BoundStatement::Ddl(DdlStatement::AlterDatabase { .. })
        ),
        "{bound:?}"
    );
}
