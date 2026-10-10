//! Integration tests of batch `EXEC` routed through procedure dispatch.

use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    Engine, ProcAction, ProcArg, ProcParam, ResultSink, Session, SessionState,
    SystemProcedureResolver, is_unicode_text,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{TypeInfo, Value};

const SPID: i16 = 72;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Columns,
    Row(Vec<Value>),
    Done(Option<u64>, bool),
    DoneInProc,
    /// Number and line of an error the batch sent to the client.
    Error(u32, u32),
}

#[derive(Default)]
struct Recording(Vec<Event>);

impl ResultSink for Recording {
    fn columns(&mut self, _cols: &[ColumnMeta]) -> SqlResult<()> {
        self.0.push(Event::Columns);
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
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        self.0.push(Event::DoneInProc);
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.0.push(Event::Error(err.number, err.line));
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }
    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn test_resolver() -> Arc<SystemProcedureResolver> {
    fn resolve(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
        if is_catalogue_name(name) {
            return Some(catalogue_answer(args));
        }
        if !name.eq_ignore_ascii_case("sp_executesql")
            && !name.to_ascii_lowercase().ends_with(".sp_executesql")
        {
            return None;
        }
        if let Some(number) = refused_number(args) {
            return Some(Err(SqlError::new(number, 16, 2, "argument error")));
        }
        if let Some(arg) = statement_argument(args)
            && !is_unicode_text(arg.ty)
        {
            return Some(Err(SqlError::procedure_expects_type(
                "@statement",
                "ntext/nchar/nvarchar",
            )));
        }
        let mut params = Vec::new();
        for arg in args {
            let Some(name) = arg.name.filter(|name| name.starts_with('@')) else {
                continue;
            };
            if name == "@statement" || name == "@stmt" || name == "@params" || name == "params" {
                continue;
            }
            params.push(ProcParam {
                name: name.to_owned(),
                ty: arg.ty.clone(),
                value: arg.value.clone(),
                output: arg.output,
            });
        }
        let statement = args
            .iter()
            .find_map(|arg| match (arg.name, arg.value) {
                (None | Some("@statement") | Some("@stmt"), Value::String(text)) => {
                    Some(text.text.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| "SET @o = @a + 1".into());
        Some(Ok(ProcAction::ExecuteSql { statement, params }))
    }
    Arc::new(resolve)
}

/// The argument carrying the statement of an `sp_executesql` call: the first positional
/// one, or the one named `@statement` or `@stmt`.
fn statement_argument<'a>(
    args: &'a [vauban_session::ProcArg<'a>],
) -> Option<&'a vauban_session::ProcArg<'a>> {
    args.iter()
        .find(|arg| matches!(arg.name, None | Some("@statement") | Some("@stmt")))
}

/// `sp_tables` stands here for the catalogue procedures, whose text parameters are declared
/// `sysname`: a non-Unicode argument reaches them converted, so the resolver refuses one it
/// receives under a non-Unicode type.
fn is_catalogue_name(name: &str) -> bool {
    let base = name.replace(['[', ']'], "");
    base.rsplit('.')
        .next()
        .is_some_and(|part| part.eq_ignore_ascii_case("sp_tables"))
}

fn catalogue_answer(args: &[vauban_session::ProcArg<'_>]) -> Result<ProcAction, SqlError> {
    for arg in args {
        if matches!(arg.value, Value::String(_)) && !is_unicode_text(arg.ty) {
            return Err(SqlError::procedure_expects_type(
                "@table_name",
                "ntext/nchar/nvarchar",
            ));
        }
    }
    Ok(ProcAction::ExecuteSql {
        statement: "SELECT 4 AS tables_seen".to_owned(),
        params: Vec::new(),
    })
}

/// The number an `@refuse = <n>` argument asks the resolver to answer, so a test can read
/// what the dispatch does with an error the resolution raised, whatever its number.
fn refused_number(args: &[vauban_session::ProcArg<'_>]) -> Option<u32> {
    args.iter()
        .find(|arg| {
            arg.name
                .is_some_and(|name| name.eq_ignore_ascii_case("@refuse"))
        })
        .and_then(|arg| match arg.value {
            Value::I32(number) => u32::try_from(*number).ok(),
            _ => None,
        })
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::with_system_procedures(
            Arc::new(MemoryStorage::new()),
            test_resolver(),
        )),
        SessionState::new(SPID),
    )
}

fn run(text: &str) -> Recording {
    let mut sink = Recording::default();
    session()
        .run_batch(text, &mut sink)
        .expect("batch completes");
    sink
}

#[test]
fn batch_sp_executesql_with_output_writes_the_caller_variable() {
    let events = run(
        "DECLARE @v int; EXEC sp_executesql N'SET @o = @a + 1', N'@a int, @o int OUTPUT', @a = 6, @o = @v OUTPUT; SELECT @v AS n;",
    );
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Row(values) if values == &[Value::I32(7)])),
        "{:?}",
        events.0
    );
}

#[test]
fn rowcount_after_sp_executesql_is_the_row_count_of_its_last_statement() {
    let events =
        run("EXEC sp_executesql N'SELECT 1 AS n', N''; SELECT @@ROWCOUNT AS rc, @@ERROR AS err;");
    assert!(
        events.0.iter().any(|event| matches!(
            event,
            Event::Row(values) if values == &[Value::I32(1), Value::I32(0)]
        )),
        "{:?}",
        events.0
    );
}

#[test]
fn return_status_after_a_failing_sp_executesql_is_the_error_number() {
    let events = run("DECLARE @r int; EXEC @r = sp_executesql N'SELECT 1/0', N''; SELECT @r AS n;");
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Row(values) if values == &[Value::I32(8134)])),
        "{:?}",
        events.0
    );
}

#[test]
fn exec_literal_select_returns_one_row() {
    let events = run("EXEC('SELECT 1 AS n');");
    assert!(events.0.contains(&Event::Columns));
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Row(values) if values == &[Value::I32(1)]))
    );
}

#[test]
fn exec_null_text_emits_no_row() {
    let events = run("DECLARE @t nvarchar(100) = NULL; EXEC(@t);");
    assert!(
        !events.0.iter().any(|event| matches!(event, Event::Row(_))),
        "{:?}",
        events.0
    );
}

#[test]
fn rowcount_after_exec_of_a_literal_select_is_the_row_count_of_its_last_statement() {
    // The row count the caller reads back after `EXEC('…')` returns matches what the same
    // text leaves when it runs through `sp_executesql` instead
    // (`rowcount_after_sp_executesql_is_the_row_count_of_its_last_statement`, this file).
    let events = run("EXEC('SELECT 1 AS n'); SELECT @@ROWCOUNT;");
    let rows: Vec<&Vec<Value>> = events
        .0
        .iter()
        .filter_map(|event| match event {
            Event::Row(values) => Some(values),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![&vec![Value::I32(1)], &vec![Value::I32(1)]],
        "{:?}",
        events.0
    );
}

/// The line each number of an argument error leaves with, on a batch whose `EXEC` sits on
/// line 3: two of them count arguments and leave without a line, two point inside the
/// parameterized text and leave on its first line.
///
/// | number | line |
/// |---|:-:|
/// | 8144 (too many arguments) | 0 |
/// | 8146 (arguments for a text that declares none) | 0 |
/// | 8178 (a declared parameter left without a value) | 1 |
/// | 214 (an argument whose type the procedure refuses) | 1 |
#[test]
fn argument_errors_of_a_call_leave_with_the_line_their_number_calls_for() {
    for (number, line) in [(8144, 0), (8146, 0), (8178, 1), (214, 1)] {
        let events = run(&format!(
            "SELECT 1 AS n;\n\nEXEC sp_executesql N'SELECT 1 AS n', N'', @refuse = {number};"
        ));
        assert!(
            events.0.contains(&Event::Error(number, line)),
            "{number}: {:?}",
            events.0
        );
    }
}

/// A name no procedure carries answers 2812 on the line the `EXEC` sits on, here the third
/// line of the batch, and not on the first line of the batch.
#[test]
fn an_exec_of_an_unknown_procedure_answers_on_the_line_of_its_statement() {
    let events = run("SELECT 1 AS n;\n\nEXEC dbo.nosuchprocedure;");
    assert!(events.0.contains(&Event::Error(2812, 3)), "{:?}", events.0);
}

/// `OUTPUT` written on a constant answers 179 on the line the `EXEC` sits on, here the
/// third line of the batch.
#[test]
fn output_on_a_constant_answers_on_the_line_of_its_statement() {
    let events = run(
        "SELECT 1 AS n;\n\nEXEC sp_executesql N'SELECT 1 AS n', N'@o int OUTPUT', @o = 1 OUTPUT;",
    );
    assert!(events.0.contains(&Event::Error(179, 3)), "{:?}", events.0);
}

/// An error raised by a statement of the text a call runs keeps the line that text puts on
/// it: the divide by zero below sits on the second line of the text, whatever line the
/// `EXEC` sits on.
#[test]
fn an_error_of_the_text_a_call_runs_keeps_the_line_of_that_text() {
    let events =
        run("SELECT 1 AS n;\n\nEXEC sp_executesql N'SELECT 1 AS n;\nSELECT 1/0 AS d;', N'';");
    assert!(events.0.contains(&Event::Error(8134, 2)), "{:?}", events.0);
}

/// Reads the errors a batch sent to the client, with the line of each.
fn errors(events: &Recording) -> Vec<(u32, u32)> {
    events
        .0
        .iter()
        .filter_map(|event| match event {
            Event::Error(number, line) => Some((*number, *line)),
            _ => None,
        })
        .collect()
}

fn rows(events: &Recording) -> Vec<Vec<Value>> {
    events
        .0
        .iter()
        .filter_map(|event| match event {
            Event::Row(values) => Some(values.clone()),
            _ => None,
        })
        .collect()
}

/// The statement of an `sp_executesql` call reaches the resolver under the type the caller
/// wrote it with, so a non-Unicode one is refused: written without the `N` prefix, or read
/// from a `varchar` variable, it answers 214. The line is the first of the call, as it is
/// for the other argument errors a resolution raises
/// (`argument_errors_of_a_call_leave_with_the_line_their_number_calls_for`, this file).
#[test]
fn a_varchar_statement_is_refused_by_executesql() {
    assert_eq!(
        errors(&run("EXEC sp_executesql 'SELECT 1 AS n';")),
        vec![(214, 1)]
    );
    assert_eq!(
        errors(&run(
            "DECLARE @s varchar(100) = 'SELECT 1 AS n';\nEXEC sp_executesql @s, N'';"
        )),
        vec![(214, 1)]
    );
}

/// The two Unicode spellings of the same statement run: the refusal above is about the type
/// the argument carries and not about the call itself.
#[test]
fn an_nvarchar_statement_is_run_by_executesql() {
    let literal = run("EXEC sp_executesql N'SELECT 1 AS n';");
    assert_eq!(errors(&literal), Vec::new());
    assert_eq!(rows(&literal), vec![vec![Value::I32(1)]]);
    let variable = run("DECLARE @s nvarchar(100) = N'SELECT 2 AS n'; EXEC sp_executesql @s, N'';");
    assert_eq!(errors(&variable), Vec::new());
    assert_eq!(rows(&variable), vec![vec![Value::I32(2)]]);
}

/// A catalogue procedure is served a text argument written without the `N` prefix, and one
/// read from a `varchar` variable: its parameters are declared `sysname` and the argument
/// reaches it converted.
#[test]
fn a_catalogue_procedure_still_reads_a_varchar_argument() {
    let literal = run("EXEC sys.sp_tables @table_name = 'clients';");
    assert_eq!(errors(&literal), Vec::new());
    assert_eq!(rows(&literal), vec![vec![Value::I32(4)]]);
    let variable = run("DECLARE @t varchar(40) = 'clients'; EXEC sys.sp_tables @table_name = @t;");
    assert_eq!(errors(&variable), Vec::new());
    assert_eq!(rows(&variable), vec![vec![Value::I32(4)]]);
}

/// The values of the rows a batch sent, in order.
fn row_values(events: &Recording) -> Vec<Vec<Value>> {
    events
        .0
        .iter()
        .filter_map(|event| match event {
            Event::Row(values) => Some(values.clone()),
            _ => None,
        })
        .collect()
}

/// A procedure called inside a `BEGIN … END` runs where it is written, and the block goes
/// on with the statement after the call: two result sets.
#[test]
fn a_procedure_called_in_a_block_lets_the_block_go_on() {
    let events = run("BEGIN EXEC sp_tables; SELECT 2 AS two; END");
    assert_eq!(
        row_values(&events),
        vec![vec![Value::I32(4)], vec![Value::I32(2)]],
        "{:?}",
        events.0
    );
    assert!(
        !events
            .0
            .iter()
            .any(|event| matches!(event, Event::Error(..))),
        "{:?}",
        events.0
    );
}

/// The `OUTPUT` value of a call written in a branch reaches the caller's variable once the
/// level of the call is closed, and so does each call of a loop.
#[test]
fn an_output_parameter_written_by_a_call_in_a_branch_reaches_the_caller() {
    let events = run(
        "DECLARE @v int; IF 1 = 1 EXEC sp_executesql N'SET @o = @a + 1', N'@a int, @o int OUTPUT', @a = 6, @o = @v OUTPUT; SELECT @v AS n;",
    );
    assert_eq!(
        row_values(&events).last(),
        Some(&vec![Value::I32(7)]),
        "{:?}",
        events.0
    );

    let events = run(
        "DECLARE @v int = 0, @i int = 0; WHILE @i < 3 BEGIN EXEC sp_executesql N'SET @o = @a + 1', N'@a int, @o int OUTPUT', @a = @v, @o = @v OUTPUT; SET @i += 1 END; SELECT @v AS n;",
    );
    assert_eq!(
        row_values(&events).last(),
        Some(&vec![Value::I32(3)]),
        "{:?}",
        events.0
    );
}

/// A procedure called by a branch of a dynamic text runs there, and the block of the branch
/// goes on: the result set of the call, then the one after it.
#[test]
fn a_procedure_called_in_a_branch_of_a_dynamic_text_lets_the_branch_go_on() {
    let events = run("EXEC('IF 1 = 1 BEGIN EXEC sp_tables; SELECT 2 AS two END');");
    assert_eq!(
        row_values(&events),
        vec![vec![Value::I32(4)], vec![Value::I32(2)]],
        "{:?}",
        events.0
    );
}
