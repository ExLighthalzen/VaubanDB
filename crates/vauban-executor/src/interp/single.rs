//! The host of a program that runs in the one context its caller built: the host of
//! [`Program::run`](super::Program::run), and of a statement that holds others when it is
//! run alone through [`crate::execute`].

use vauban_binder::BoundExpr;
use vauban_errors::{ErrorScope, InfoMessage, SqlError, SqlResult};
use vauban_planner::PhysicalStatement;
use vauban_types::Value;

use crate::context::ExecContext;
use crate::expr::eval_expr;
use crate::row::ExecOutcome;
use crate::sink::{EnvironmentChange, ResultColumn, StatementSink};

use super::compile::{Labels, compile};
use super::run::{End, bug, condition_error, escape_error, run};
use super::{Call, CallHandler, Host, ProgramEnd, Ran, Site};

/// A [`Host`] over one context, one sink and one [`CallHandler`].
///
/// Each statement runs through [`crate::execute`] in that context and is closed in the
/// sink with its row count, `NULL` under `SET NOCOUNT ON`; an error goes to the sink with
/// the end of its statement once the interpreter decided its reach, whatever that reach
/// (`interp::tests::an_error_reaches_the_host_with_the_reach_the_interpreter_decided`), and
/// `@@ERROR` takes its number.
pub(crate) struct SingleContext<'r, 'a> {
    ctx: &'r mut ExecContext<'a>,
    sink: &'r mut dyn StatementSink,
    calls: &'r mut dyn CallHandler,
}

impl<'r, 'a> SingleContext<'r, 'a> {
    /// The host of a program run in `ctx`.
    pub(crate) fn new(
        ctx: &'r mut ExecContext<'a>,
        sink: &'r mut dyn StatementSink,
        calls: &'r mut dyn CallHandler,
    ) -> Self {
        Self { ctx, sink, calls }
    }
}

impl Host for SingleContext<'_, '_> {
    fn statement(&mut self, stmt: Option<&PhysicalStatement>, _site: Site) -> SqlResult<Ran> {
        let Some(stmt) = stmt else {
            return Err(bug(
                "interp: a root the host prepared reached a host that prepared nothing",
            ));
        };
        match crate::execute(stmt, self.ctx, &mut *self.sink) {
            Ok(ExecOutcome::Rows(count)) => {
                let mut nocount = false;
                if let Some(session) = self.ctx.session.as_deref_mut() {
                    session.rowcount = i64::try_from(count).unwrap_or(i64::MAX);
                    nocount = session.nocount;
                }
                self.sink.done(if nocount { None } else { Some(count) })?;
                Ok(Ran::Outcome(ExecOutcome::Rows(count)))
            }
            Ok(ExecOutcome::NoRows) => {
                self.sink.done(None)?;
                Ok(Ran::Outcome(ExecOutcome::NoRows))
            }
            Ok(outcome) => Ok(Ran::Outcome(outcome)),
            Err(err) => Ok(Ran::Failed(err)),
        }
    }

    fn condition(
        &mut self,
        condition: &BoundExpr,
        _site: Site,
    ) -> SqlResult<Result<bool, SqlError>> {
        match eval_expr(condition, None, self.ctx) {
            Ok(value) => Ok(Ok(matches!(value, Value::Bit(true)))),
            Err(err) => Ok(Err(condition_error(err, condition.line))),
        }
    }

    fn within(
        &mut self,
        _site: Site,
        step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
    ) -> SqlResult<()> {
        step(self.ctx)
    }

    // The line an error carries is the one the executor gave it: a program run in one
    // context knows no line of the statements it holds.
    fn locate(&self, err: SqlError, _site: Site) -> SqlError {
        err
    }

    fn raise(&mut self, err: &SqlError, _reach: ErrorScope) -> SqlResult<()> {
        report(err, self.ctx, &mut *self.sink)
    }

    fn call(&mut self, call: Call, _site: Site) -> SqlResult<Result<ProgramEnd, SqlError>> {
        Ok(self.calls.call(call, self.ctx, &mut *self.sink))
    }

    fn leave_call(&mut self, database: &str) -> SqlResult<()> {
        self.calls.restore_database(database, self.ctx)
    }

    // The end of a statement of the control of flow is not handed to the sink here: the
    // sink closes the statements that run (`interp::tests::each_statement_is_closed_in_the_sink`).
    fn control(&mut self, _site: Site) -> SqlResult<()> {
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.ctx.cancelled()
    }

    fn xact_abort(&self) -> bool {
        self.ctx
            .session
            .as_deref()
            .is_some_and(|session| session.xact_abort)
    }
}

/// Hands `err` to the sink and ends its statement; `@@ERROR` takes its number.
fn report(
    err: &SqlError,
    ctx: &mut ExecContext<'_>,
    sink: &mut dyn StatementSink,
) -> SqlResult<()> {
    if let Some(session) = ctx.session.as_deref_mut() {
        session.last_error = err.number;
    }
    sink.error(err)?;
    sink.done(None)
}

/// Runs `stmt`, an `IF`, a `WHILE`, a `BEGIN … END` or a statement left unbound, as a
/// closed program of its own (module documentation of `interp`, "A statement run alone").
///
/// The answer is the one of the statement that ran last when that statement is the branch
/// of an `IF`, or the row count of the last statement closed, and its end is left to the
/// caller, which closes the whole statement. An error that ends the program is not left in
/// the sink: it is the `Err` of the statement, which the caller reports. A call the
/// statement holds is refused with the internal error.
///
/// # Errors
///
/// The error that ended the program, 133 for a `GOTO` to a label the statement does not
/// hold, and the error of the sink.
pub(crate) fn run_alone(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'_>,
    sink: &mut dyn StatementSink,
) -> SqlResult<ExecOutcome> {
    let program = compile(std::slice::from_ref(stmt), Labels::Closed, true)?;
    let mut held = Holding::new(sink);
    let mut calls = NoCalls;
    let end = {
        let mut host = SingleContext::new(ctx, &mut held, &mut calls);
        run(&program, &mut host)
    };
    let end = match end {
        Ok(end) => end,
        Err(err) => {
            held.flush()?;
            return Err(err);
        }
    };
    match end {
        End::Completed(answer) => held.close(answer),
        End::Returned(code) => {
            held.flush()?;
            Ok(ExecOutcome::Return(code))
        }
        End::Cancelled => {
            held.flush()?;
            Ok(ExecOutcome::Cancelled)
        }
        End::Aborted(err) | End::LevelAborted(err) => {
            held.withdraw(&err)?;
            Err(err)
        }
        End::Escaped(outcome) => {
            held.flush()?;
            Err(escape_error(&outcome))
        }
    }
}

/// The calls of a statement run alone: nothing here can run a procedure or a dynamic text.
struct NoCalls;

impl CallHandler for NoCalls {
    fn call(
        &mut self,
        _call: Call,
        _ctx: &mut ExecContext<'_>,
        _sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd> {
        Err(bug(
            "an EXECUTE held by an IF, a WHILE or a BEGIN ... END run alone is not \
             implemented",
        ))
    }

    fn restore_database(&mut self, _database: &str, _ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        Ok(())
    }
}

/// One end of statement or one error held back by [`Holding`].
#[derive(Debug)]
enum HeldEvent {
    Done(Option<u64>),
    Error(SqlError),
}

/// A sink that holds back the last end of statement, and the error before it, until
/// something else follows: what the program of a statement run alone sent last is the
/// caller's to send ([`run_alone`]).
struct Holding<'s> {
    sink: &'s mut dyn StatementSink,
    held: Vec<HeldEvent>,
}

impl<'s> Holding<'s> {
    fn new(sink: &'s mut dyn StatementSink) -> Self {
        Self {
            sink,
            held: Vec::new(),
        }
    }

    /// Sends what is held.
    fn flush(&mut self) -> SqlResult<()> {
        for event in self.held.drain(..) {
            match event {
                HeldEvent::Done(count) => self.sink.done(count)?,
                HeldEvent::Error(err) => self.sink.error(&err)?,
            }
        }
        Ok(())
    }

    /// The program completed: the end of its last statement is dropped, and its count is
    /// what the statement answers, the caller closing it.
    fn close(mut self, answer: ExecOutcome) -> SqlResult<ExecOutcome> {
        let count = match self.held.as_slice() {
            [HeldEvent::Done(count)] => *count,
            _ => {
                self.flush()?;
                return Ok(answer);
            }
        };
        self.held.clear();
        Ok(match (answer, count) {
            (ExecOutcome::Rows(rows), _) => ExecOutcome::Rows(rows),
            (_, Some(rows)) => ExecOutcome::Rows(rows),
            (answer, None) => answer,
        })
    }

    /// The program ended by `err`: the error and the end of its statement are dropped,
    /// since the caller reports it.
    fn withdraw(mut self, err: &SqlError) -> SqlResult<()> {
        if let [HeldEvent::Error(held), HeldEvent::Done(_)] = self.held.as_slice()
            && held.number == err.number
        {
            self.held.clear();
            return Ok(());
        }
        self.flush()
    }
}

impl StatementSink for Holding<'_> {
    fn columns(&mut self, columns: &[ResultColumn]) -> SqlResult<()> {
        self.flush()?;
        self.sink.columns(columns)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.flush()?;
        self.sink.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        if !matches!(self.held.as_slice(), [HeldEvent::Error(_)]) {
            self.flush()?;
        }
        self.held.push(HeldEvent::Done(rowcount));
        Ok(())
    }

    fn info(&mut self, message: &InfoMessage) -> SqlResult<()> {
        self.flush()?;
        self.sink.info(message)
    }

    fn error(&mut self, error: &SqlError) -> SqlResult<()> {
        self.flush()?;
        self.held.push(HeldEvent::Error(error.clone()));
        Ok(())
    }

    fn env_change(&mut self, change: &EnvironmentChange) -> SqlResult<()> {
        self.flush()?;
        self.sink.env_change(change)
    }
}
