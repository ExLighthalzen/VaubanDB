//! What the `SET` options change, seen through `Session::run_batch`.
//!
//! Every option of `SetOptions` is honoured, without effect, or a deliberate difference
//! from SQL Server; `every_option_is_documented` is the guard that no field escapes that
//! sorting, and the other tests prove the honoured ones.
//!
//! No socket and no TDS here: the sequence of `columns` / `row` / `done` / `error` calls a
//! batch produces is what a client sees as result sets, DONE tokens and errors, and
//! `done(None, ..)` is what `sink.rs` turns into a DONE without `DONE_COUNT`. The
//! reference for every `NOCOUNT` test is the DONE token SQL Server sends on the wire
//! ([MS-TDS] 2.2.7.6):
//!
//! | batch | DONE status of each statement |
//! |---|---|
//! | `SELECT 1; SELECT 1 WHERE 1 = 0;` | `0x0011` (MORE\|COUNT, 1), `0x0010` (COUNT, 0) |
//! | `SET NOCOUNT ON; SELECT 1; SELECT 1 WHERE 1 = 0;` | `0x0001`, `0x0001`, `0x0000` |
//! | `SELECT 1; SELECT @@ROWCOUNT;` (next batch, same connection) | `0x0001`, `0x0000`, and the row is `1` |
//! | `SET NOCOUNT OFF; SELECT 1; SELECT @@ROWCOUNT;` | `0x0001`, `0x0011`, `0x0010`, row `1` |
//! | `SET NOCOUNT ON; SELECT 1; SET NOCOUNT OFF; SELECT 2;` | `0x0001`, `0x0001`, `0x0001`, `0x0010` |
//! | `SET NOCOUNT ON; SELECT 1 WHERE 1 = 0; SELECT @@ROWCOUNT;` | `0x0001`, `0x0001`, `0x0000`, row `0` |
//!
//! With the flag clear the server still writes the count in `DoneRowCount`; the spec says
//! the field is then not valid, and `ResultSink::done` cannot express it, so the DONE of a
//! `SELECT` under `NOCOUNT ON` is `done(None, more)`.
//!
//! `vauban_sysfn::register_builtins()` is called by every test that needs a function: the
//! registry is process-wide, filled by the binary at start-up, idempotent.

use std::collections::BTreeSet;
use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState, SetOptions};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{SqlString, Value};

/// SPID of every session built here.
const SPID: i16 = 51;

// ---------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------

/// Everything a sink receives, in order.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns(Vec<ColumnMeta>),
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    Error(SqlError),
    Other,
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.0.push(Event::Columns(cols.to_vec()));
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.0.push(Event::Row(row.to_vec()));
        Ok(())
    }
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.0.push(Event::Done(rowcount, more));
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        self.0.push(Event::Other);
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.clone()));
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        self.0.push(Event::Other);
        Ok(())
    }
    fn return_value(
        &mut self,
        _name: &str,
        _ty: &vauban_types::TypeInfo,
        _value: &Value,
    ) -> SqlResult<()> {
        self.0.push(Event::Other);
        Ok(())
    }
    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        self.0.push(Event::Other);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

/// A session on an in-memory engine, with the built-in functions registered and the
/// options of a fresh connection.
fn session() -> Session {
    session_with(SetOptions::default())
}

/// A session whose options are `options` when the first batch arrives.
fn session_with(options: SetOptions) -> Session {
    vauban_sysfn::register_builtins();
    let mut state = SessionState::new(SPID);
    state.options = options;
    Session::new(Arc::new(Engine::new(Arc::new(MemoryStorage::new()))), state)
}

/// Runs `text` as one batch on a fresh session and returns what the sink received.
fn run(text: &str) -> Vec<Event> {
    run_on(&mut session(), text)
}

/// Runs `text` on an existing session, keeping its state.
fn run_on(session: &mut Session, text: &str) -> Vec<Event> {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("a batch reports its errors through the sink, not through its result");
    sink.0
}

/// The `done` calls of `events`, in order.
fn dones(events: &[Event]) -> Vec<(Option<u64>, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Done(count, more) => Some((*count, *more)),
            _ => None,
        })
        .collect()
}

/// The values of the rows of `events`, in order, whatever the result set.
fn rows(events: &[Event]) -> Vec<Vec<Value>> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Row(row) => Some(row.clone()),
            _ => None,
        })
        .collect()
}

/// The single error of `events`; panics when there is none or more than one.
fn only_error(events: &[Event]) -> SqlError {
    let errors: Vec<&SqlError> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error(err) => Some(err),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "exactly one error expected: {events:#?}");
    errors[0].clone()
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

// ---------------------------------------------------------------------------------------
// NOCOUNT
// ---------------------------------------------------------------------------------------

#[test]
fn nocount_removes_the_count() {
    // Second line of the table above: `SET NOCOUNT ON; SELECT 1;` answers `0x0001` for
    // the `SET` and `0x0000` for the `SELECT`, no `DONE_COUNT` on either.
    let events = run("SET NOCOUNT ON; SELECT 1");
    assert_eq!(
        events,
        vec![
            Event::Done(None, true),
            match &events[1] {
                Event::Columns(cols) => Event::Columns(cols.clone()),
                other => panic!("a result set was expected, got {other:?}"),
            },
            Event::Row(vec![Value::I32(1)]),
            Event::Done(None, false),
        ],
        "{events:#?}"
    );

    // Fourth line: `SET NOCOUNT OFF; SELECT 1;` answers `0x0011` for the `SELECT`.
    let events = run("SET NOCOUNT OFF; SELECT 1");
    assert_eq!(
        dones(&events),
        vec![(None, true), (Some(1), false)],
        "{events:#?}"
    );
}

#[test]
fn nocount_takes_effect_in_its_own_batch_both_ways() {
    // Fifth line of the table above: `0x0001`, `0x0001`, `0x0001`, `0x0010`. Unlike
    // `QUOTED_IDENTIFIER`, which the parser reads before the first statement runs, this
    // option is read when the DONE goes out, after the `SET` before it ran.
    let events = run("SET NOCOUNT ON; SELECT 1; SET NOCOUNT OFF; SELECT 2");
    assert_eq!(
        dones(&events),
        vec![(None, true), (None, true), (None, true), (Some(1), false)],
        "{events:#?}"
    );
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(2)]],
        "the option changes the DONE, not the rows"
    );
}

#[test]
fn nocount_stays_on_for_the_next_batch() {
    // Third line of the table above: a batch with no `SET` of its own, after the one
    // that turned the option on, still answers `0x0001` then `0x0000`.
    let mut session = session();
    run_on(&mut session, "SET NOCOUNT ON");
    assert!(session.state().options.nocount);
    let events = run_on(&mut session, "SELECT 1; SELECT @@ROWCOUNT");
    assert_eq!(
        dones(&events),
        vec![(None, true), (None, false)],
        "{events:#?}"
    );

    // And the way back, on the same connection.
    run_on(&mut session, "SET NOCOUNT OFF");
    let events = run_on(&mut session, "SELECT 1");
    assert_eq!(dones(&events), vec![(Some(1), false)], "{events:#?}");
}

#[test]
fn nocount_does_not_change_rowcount() {
    // Third and fourth lines of the table above: `@@ROWCOUNT` answers `1` after
    // `SELECT 1` whether the option is on or off. Both are checked: a vector that answers
    // the same under both hypotheses proves nothing on its own, and here the hypothesis
    // to refute is "the option resets the count", which only the `ON` side can show.
    let events = run("SET NOCOUNT ON; SELECT 1; SELECT @@ROWCOUNT");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(1)]],
        "{events:#?}"
    );
    let events = run("SET NOCOUNT OFF; SELECT 1; SELECT @@ROWCOUNT");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(1)], vec![Value::I32(1)]],
        "{events:#?}"
    );
}

#[test]
fn nocount_on_a_select_without_rows() {
    // Last line of the table above: the empty result set gets `0x0001` (no count, not
    // a count of 0), and `@@ROWCOUNT` is `0` after it.
    let events = run("SET NOCOUNT ON; SELECT 1 WHERE 1 = 0; SELECT @@ROWCOUNT");
    assert_eq!(
        dones(&events),
        vec![(None, true), (None, true), (None, false)],
        "{events:#?}"
    );
    assert_eq!(rows(&events), vec![vec![Value::I32(0)]], "{events:#?}");
    // First line, the `OFF` side: the empty result set carries a count of 0.
    let events = run("SELECT 1; SELECT 1 WHERE 1 = 0");
    assert_eq!(
        dones(&events),
        vec![(Some(1), true), (Some(0), false)],
        "{events:#?}"
    );
}

#[test]
fn nocount_leaves_the_done_of_an_error_alone() {
    // `SET NOCOUNT ON; SELECT 1; SELECT 1 / 0;` and its `OFF` twin both close the failing
    // statement with `0x0003` (MORE|ERROR) and no count on SQL Server. The DONE of an
    // error never carried one here either, so the option has nothing to do.
    for set in ["ON", "OFF"] {
        let events = run(&format!("SET NOCOUNT {set}; SELECT 1 / 0"));
        assert_eq!(only_error(&events).number, 8134);
        assert_eq!(
            dones(&events),
            vec![(None, true), (None, false)],
            "{events:#?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// CONCAT_NULL_YIELDS_NULL
// ---------------------------------------------------------------------------------------

#[test]
fn concat_null_yields_null_off() {
    // `a` under OFF, NULL under ON, as on SQL Server. The `SET` reaches the `SELECT` of
    // its own batch, because the option is read when the statement is bound.
    let events = run("SET CONCAT_NULL_YIELDS_NULL OFF; SELECT 'a' + CAST(NULL AS varchar(1))");
    assert_eq!(rows(&events), vec![vec![text("a")]], "{events:#?}");
    let events = run("SET CONCAT_NULL_YIELDS_NULL ON; SELECT 'a' + CAST(NULL AS varchar(1))");
    assert_eq!(rows(&events), vec![vec![Value::Null]], "{events:#?}");
    // The default of a connection is ON.
    let events = run("SELECT 'a' + CAST(NULL AS varchar(1))");
    assert_eq!(rows(&events), vec![vec![Value::Null]], "{events:#?}");
}

#[test]
fn concat_null_yields_null_off_leaves_arithmetic_alone() {
    // The option touches `+` between two strings only, `1 + NULL` stays NULL.
    let events = run("SET CONCAT_NULL_YIELDS_NULL OFF; SELECT 1 + NULL");
    assert_eq!(rows(&events), vec![vec![Value::Null]], "{events:#?}");
}

#[test]
fn concat_with_an_untyped_null_follows_the_option() {
    // `SELECT 'a' + NULL` is a `varchar` concatenation on SQL Server, because the untyped
    // NULL takes the type of the other operand: `a` under OFF, NULL under ON. A binder
    // that typed the literal NULL as an `int` would make `+` an integer addition and fail
    // to convert `'a'` (245) under either option, which is what this vector separates.
    let events = run("SET CONCAT_NULL_YIELDS_NULL OFF; SELECT 'a' + NULL");
    assert_eq!(rows(&events), vec![vec![text("a")]], "{events:#?}");
    let events = run("SET CONCAT_NULL_YIELDS_NULL ON; SELECT 'a' + NULL");
    assert_eq!(rows(&events), vec![vec![Value::Null]], "{events:#?}");
    // The default of a connection is ON, like the typed vector above.
    let events = run("SELECT 'a' + NULL");
    assert_eq!(rows(&events), vec![vec![Value::Null]], "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// QUOTED_IDENTIFIER
// ---------------------------------------------------------------------------------------

#[test]
fn quoted_identifier_switches_the_parser() {
    // 207 under ON, the string `a` under OFF. The option is read by the parser, so the
    // state is set before the batch here, which tests the parser alone; a `SET` in the
    // batch reaches the statements after it too
    // (`quoted_identifier_takes_effect_in_its_own_batch_both_ways` of
    // `run_batch_pipeline.rs`).
    let events = run("SELECT \"a\"");
    let err = only_error(&events);
    assert_eq!(err.number, 207);
    assert_eq!(err.message, SqlError::invalid_column_name("a").message);

    let options = SetOptions {
        quoted_identifier: false,
        ..SetOptions::default()
    };
    let events = run_on(&mut session_with(options), "SELECT \"a\"");
    assert_eq!(rows(&events), vec![vec![text("a")]], "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// DATEFIRST, the fourth honoured option
// ---------------------------------------------------------------------------------------

#[test]
fn datefirst_reaches_datepart() {
    // 2000-01-02 is a Sunday: 7 under `DATEFIRST 1`, 1 under `DATEFIRST 7`.
    let query = "SELECT DATEPART(weekday, CAST('2000-01-02' AS date))";
    let events = run(&format!("SET DATEFIRST 1; {query}"));
    assert_eq!(rows(&events), vec![vec![Value::I32(7)]], "{events:#?}");
    let events = run(&format!("SET DATEFIRST 7; {query}"));
    assert_eq!(rows(&events), vec![vec![Value::I32(1)]], "{events:#?}");
}

// ---------------------------------------------------------------------------------------
// The guard: no option without a verdict
// ---------------------------------------------------------------------------------------

/// The fields of `SetOptions`, read from the `Debug` rendering of a value: this is the
/// real struct, whatever the source file says.
fn fields_of_set_options() -> BTreeSet<String> {
    let rendered = format!("{:?}", SetOptions::default());
    let body = rendered
        .strip_prefix("SetOptions { ")
        .and_then(|rest| rest.strip_suffix(" }"))
        .unwrap_or_else(|| panic!("unexpected Debug rendering: {rendered}"));
    body.split(", ")
        .map(|field| {
            field
                .split_once(':')
                .unwrap_or_else(|| panic!("no `name:` in {field:?}"))
                .0
                .to_owned()
        })
        .collect()
}

/// The fields of `pub struct SetOptions` in `src/set_options.rs`, each with the rustdoc
/// lines that precede it.
fn documented_fields() -> Vec<(String, String)> {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/set_options.rs"))
            .expect("src/set_options.rs is readable");
    let start = source
        .find("pub struct SetOptions {")
        .expect("`pub struct SetOptions {` is in the file");
    let body = &source[start..];
    let end = body.find("\n}\n").expect("the struct body ends");
    let mut fields = Vec::new();
    let mut doc = String::new();
    for line in body[..end].lines().skip(1) {
        let trimmed = line.trim_start();
        if let Some(text) = trimmed.strip_prefix("///") {
            doc.push_str(text);
            doc.push('\n');
        } else if let Some(rest) = trimmed.strip_prefix("pub ")
            && let Some((name, _)) = rest.split_once(':')
        {
            fields.push((name.to_owned(), std::mem::take(&mut doc)));
        } else if trimmed.starts_with('#') || trimmed.is_empty() {
            // An attribute or a blank line between the doc and the field.
        } else {
            panic!("unexpected line in the body of SetOptions: {line:?}");
        }
    }
    fields
}

/// The verdict written in the rustdoc of one field: `Honoured`, `No effect` or
/// `Deliberate difference from SQL Server`.
///
/// The three markers exclude each other; a field whose rustdoc carries none or more than
/// one has no verdict.
fn verdict(doc: &str) -> Option<&'static str> {
    let lower = doc.to_ascii_lowercase();
    let found: Vec<&'static str> = [
        ("honoured", "honoured"),
        ("no effect", "no effect"),
        ("deliberate difference from sql server", "deviation"),
    ]
    .into_iter()
    .filter(|(marker, _)| lower.contains(marker))
    .map(|(_, verdict)| verdict)
    .collect();
    match found.as_slice() {
        [single] => Some(single),
        _ => None,
    }
}

#[test]
fn every_option_is_documented() {
    let real = fields_of_set_options();
    let documented = documented_fields();
    let parsed: BTreeSet<String> = documented.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        parsed, real,
        "the fields read from the source must be the fields of the struct"
    );
    assert!(!real.is_empty());

    let mut verdicts = BTreeSet::new();
    for (name, doc) in &documented {
        let verdict = verdict(doc).unwrap_or_else(|| {
            panic!(
                "`{name}` has no single verdict: its rustdoc must say `Honoured`, `No effect` \
                 or `Deliberate difference from SQL Server`, and one of the three"
            )
        });
        verdicts.insert(verdict);
    }
    // The three verdicts are all in use: a guard that only ever sees one kind of mention
    // could be matching the wrong text.
    assert_eq!(verdicts.len(), 3, "{verdicts:?}");
}

#[test]
fn the_honoured_options_are_the_ones_with_a_test() {
    // The list is closed on purpose: writing `Honoured` on a field is a claim, and the
    // claim needs its test, above, in `run_batch_pipeline.rs` or in `isolation_options.rs`.
    let honoured: BTreeSet<String> = documented_fields()
        .into_iter()
        .filter(|(_, doc)| verdict(doc) == Some("honoured"))
        .map(|(name, _)| name)
        .collect();
    let expected: BTreeSet<String> = [
        "concat_null_yields_null",
        "datefirst",
        "deadlock_priority",
        "lock_timeout",
        "nocount",
        "quoted_identifier",
        "xact_abort",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(honoured, expected);
}

// ---------------------------------------------------------------------------------------
// IDENTITY_INSERT
// ---------------------------------------------------------------------------------------

/// Two sessions on one in-memory engine.
fn shared_sessions() -> (Session, Session) {
    vauban_sysfn::register_builtins();
    let engine = Arc::new(Engine::new(Arc::new(MemoryStorage::new())));
    let a = Session::new(Arc::clone(&engine), SessionState::new(SPID));
    let b = Session::new(engine, SessionState::new(SPID + 1));
    (a, b)
}

fn no_error(events: &[Event], text: &str) {
    assert!(
        !events.iter().any(|event| matches!(event, Event::Error(_))),
        "`{text}` raised an error: {events:#?}"
    );
}

#[test]
fn identity_insert_on_then_off_accepts_then_refuses_an_explicit_value() {
    let mut session = session();
    no_error(
        &run_on(
            &mut session,
            "CREATE TABLE dbo.t1 (id int IDENTITY NOT NULL, v int NOT NULL)",
        ),
        "CREATE TABLE",
    );
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT dbo.t1 ON"),
        "SET ON",
    );
    no_error(
        &run_on(&mut session, "INSERT INTO dbo.t1 (id, v) VALUES (50, 1)"),
        "INSERT with IDENTITY_INSERT ON",
    );
    let events = run_on(&mut session, "SELECT id, v FROM dbo.t1");
    assert_eq!(
        rows(&events),
        vec![vec![Value::I32(50), Value::I32(1)]],
        "{events:#?}"
    );
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT dbo.t1 OFF"),
        "SET OFF",
    );
    let events = run_on(&mut session, "INSERT INTO dbo.t1 (id, v) VALUES (51, 1)");
    let err = only_error(&events);
    assert_eq!(err.number, 544);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
}

#[test]
fn identity_insert_without_a_column_list_is_8101() {
    let mut session = session();
    no_error(
        &run_on(
            &mut session,
            "CREATE TABLE dbo.t1 (id int IDENTITY NOT NULL, v int NOT NULL)",
        ),
        "CREATE TABLE",
    );
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT dbo.t1 ON"),
        "SET ON",
    );
    let events = run_on(&mut session, "INSERT INTO dbo.t1 VALUES (50, 1)");
    let err = only_error(&events);
    assert_eq!(err.number, 8101);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    let events = run_on(&mut session, "INSERT INTO dbo.t1 (id, v) VALUES (50, 1)");
    no_error(&events, "INSERT with a column list");
}

#[test]
fn a_second_table_on_is_8107_and_the_first_stays_open() {
    let mut session = session();
    no_error(
        &run_on(
            &mut session,
            "CREATE TABLE dbo.t1 (id int IDENTITY NOT NULL, v int NOT NULL)",
        ),
        "CREATE TABLE t1",
    );
    no_error(
        &run_on(
            &mut session,
            "CREATE TABLE dbo.t2 (id int IDENTITY NOT NULL, v int NOT NULL)",
        ),
        "CREATE TABLE t2",
    );
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT dbo.t1 ON"),
        "SET t1 ON",
    );
    let events = run_on(&mut session, "SET IDENTITY_INSERT dbo.t2 ON");
    let err = only_error(&events);
    assert_eq!(err.number, 8107);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        SqlError::identity_insert_already_on("master", "dbo", "t1", "dbo.t2").message
    );
    let open = session
        .state()
        .identity_insert
        .as_ref()
        .expect("t1 stays open");
    assert_eq!(open.name, "t1");
    no_error(
        &run_on(&mut session, "INSERT INTO dbo.t1 (id, v) VALUES (52, 1)"),
        "INSERT t1 after 8107",
    );
    let events = run_on(&mut session, "INSERT INTO dbo.t2 (id, v) VALUES (52, 1)");
    assert_eq!(only_error(&events).number, 544);
}

/// A table with an identity column and one ordinary column, on `session`.
fn create_identity_table(session: &mut Session, name: &str) {
    no_error(
        &run_on(
            session,
            &format!("CREATE TABLE dbo.{name} (id int IDENTITY NOT NULL, v int NOT NULL)"),
        ),
        "CREATE TABLE",
    );
}

#[test]
fn identity_insert_opened_on_a_delimited_name_is_closed_by_a_canonical_off() {
    for opened_as in ["[dbo].[ra]", "\"dbo\".\"ra\"", "[DBO].[RA]", "[ra]"] {
        let mut session = session();
        create_identity_table(&mut session, "ra");
        no_error(
            &run_on(&mut session, &format!("SET IDENTITY_INSERT {opened_as} ON")),
            opened_as,
        );
        // The explicit value goes in, so the option was read.
        no_error(
            &run_on(&mut session, "INSERT INTO dbo.ra (id, v) VALUES (5, 1)"),
            "INSERT with the option open",
        );
        // The canonical form of the same table is not a second table.
        no_error(
            &run_on(&mut session, "SET IDENTITY_INSERT dbo.ra ON"),
            "the same table again",
        );
        // And the canonical `OFF` closes what the delimited `ON` opened: without this the
        // session would hold an option no statement can close.
        no_error(
            &run_on(&mut session, "SET IDENTITY_INSERT dbo.ra OFF"),
            "SET OFF",
        );
        assert_eq!(session.state().identity_insert, None, "{opened_as}");
        let events = run_on(&mut session, "INSERT INTO dbo.ra (id, v) VALUES (6, 1)");
        assert_eq!(only_error(&events).number, 544, "{opened_as}");
    }
}

#[test]
fn identity_insert_names_the_refused_table_without_its_delimiters() {
    let mut session = session();
    create_identity_table(&mut session, "ra");
    create_identity_table(&mut session, "rb");
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT dbo.ra ON"),
        "SET ra ON",
    );
    let events = run_on(&mut session, "SET IDENTITY_INSERT [dbo].[rb] ON");
    let err = only_error(&events);
    assert_eq!(err.number, 8107);
    assert_eq!(
        err.message,
        SqlError::identity_insert_already_on("master", "dbo", "ra", "dbo.rb").message
    );
    // One written part stays one written part.
    let events = run_on(&mut session, "SET IDENTITY_INSERT [rb] ON");
    assert_eq!(
        only_error(&events).message,
        SqlError::identity_insert_already_on("master", "dbo", "ra", "rb").message
    );
}

#[test]
fn identity_insert_8107_carries_the_line_of_its_statement() {
    let mut session = session();
    create_identity_table(&mut session, "ra");
    create_identity_table(&mut session, "rb");
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT dbo.ra ON"),
        "SET ra ON",
    );
    // The refused `SET` is the third statement and starts on line 4: the two hypotheses
    // "line of the statement" and "line of the batch" answer differently.
    let events = run_on(
        &mut session,
        "SELECT 1;\nSELECT 2;\n\nSET IDENTITY_INSERT dbo.rb ON;",
    );
    let err = only_error(&events);
    assert_eq!(err.number, 8107);
    assert_eq!(err.line, 4, "{events:#?}");
}

#[test]
fn identity_insert_on_a_double_quoted_name_needs_quoted_identifier_on() {
    let mut session = session();
    create_identity_table(&mut session, "ra");
    no_error(
        &run_on(&mut session, "SET QUOTED_IDENTIFIER OFF"),
        "QUOTED_IDENTIFIER OFF",
    );
    // With the option off the name is a string, so the statement does not parse and
    // nothing is opened; with it on the same text opens the option.
    let events = run_on(&mut session, "SET IDENTITY_INSERT \"dbo\".\"ra\" ON");
    assert_eq!(only_error(&events).number, 102);
    assert_eq!(session.state().identity_insert, None);
    let events = run_on(&mut session, "INSERT INTO dbo.ra (id, v) VALUES (5, 1)");
    assert_eq!(only_error(&events).number, 544);

    no_error(
        &run_on(&mut session, "SET QUOTED_IDENTIFIER ON"),
        "QUOTED_IDENTIFIER ON",
    );
    no_error(
        &run_on(&mut session, "SET IDENTITY_INSERT \"dbo\".\"ra\" ON"),
        "a double quoted name under QUOTED_IDENTIFIER ON",
    );
    no_error(
        &run_on(&mut session, "INSERT INTO dbo.ra (id, v) VALUES (5, 1)"),
        "INSERT with the option open",
    );
}

#[test]
fn identity_insert_survives_the_next_batch_and_is_not_shared() {
    let (mut a, mut b) = shared_sessions();
    no_error(
        &run_on(
            &mut a,
            "CREATE TABLE dbo.t1 (id int IDENTITY NOT NULL, v int NOT NULL)",
        ),
        "CREATE TABLE",
    );
    no_error(&run_on(&mut a, "SET IDENTITY_INSERT dbo.t1 ON"), "SET ON");
    no_error(
        &run_on(&mut a, "INSERT INTO dbo.t1 (id, v) VALUES (7, 1)"),
        "INSERT on a after a later batch",
    );
    let events = run_on(&mut b, "INSERT INTO dbo.t1 (id, v) VALUES (8, 1)");
    assert_eq!(only_error(&events).number, 544);
    let events = run_on(&mut a, "SELECT id FROM dbo.t1 WHERE id = 7");
    assert_eq!(rows(&events), vec![vec![Value::I32(7)]], "{events:#?}");
}
