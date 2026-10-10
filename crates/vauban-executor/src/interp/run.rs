//! The program counter: runs the instructions of a [`Program`] (module documentation of
//! `interp`).

use vauban_binder::{BoundExpr, DeferredStatement};
use vauban_errors::{BatchErrorScope, InternalError, SqlError, SqlResult};
use vauban_planner::PhysicalStatement;
use vauban_types::{SqlType, TypeInfo, Value, convert};

use crate::compile::compile as compile_checks;
use crate::context::ExecContext;
use crate::control;
use crate::errors::at;
use crate::expr::eval_expr;
use crate::row::ExecOutcome;
use crate::sink::StatementSink;

use super::compile::{Escape, Instr, Labels, compile};
use super::frame::Frame;
use super::{Call, CallHandler, Program, ProgramEnd};

/// Who runs the program, and so what becomes of an error and of a call.
pub(crate) enum Host<'h> {
    /// [`Program::run`]: errors go to the sink, calls to the handler.
    Calls(&'h mut dyn CallHandler),
    /// [`super::run_tree`]: errors leave as `Err`, calls as the outcome that names them.
    Tree,
}

/// How a run ended.
#[derive(Debug)]
pub(crate) enum End {
    /// Past the last instruction, with the answer of the tail statement that ran, or
    /// `NoRows`.
    Completed(ExecOutcome),
    /// A `RETURN` ran.
    Returned(i32),
    /// The token was raised.
    Cancelled,
    /// An error ended the program; it is in the sink already.
    Aborted(SqlError),
    /// A jump or a call left the program: [`Host::Tree`], or a jump a statement bound at
    /// run time named.
    Escaped(ExecOutcome),
}

/// What the program counter does after an instruction.
enum Step {
    /// Goes to this position.
    Go(usize),
    /// Stops the run.
    Stop(End),
}

/// Runs `program` from its first instruction.
pub(crate) fn run<'a>(
    program: &Program<'_>,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
    host: &mut Host<'_>,
) -> SqlResult<End> {
    let mut pc = 0;
    let mut answer = ExecOutcome::NoRows;
    while let Some(instr) = program.code.get(pc) {
        let step = match instr {
            Instr::Run {
                stmt,
                tail,
                enclosing,
            } => match run_statement(stmt, ctx, sink, host) {
                Ok(Reached::Ended(end)) => Step::Stop(end),
                Ok(Reached::Outcome(outcome)) => after_statement(
                    program,
                    pc,
                    *enclosing,
                    outcome,
                    *tail,
                    &mut answer,
                    ctx,
                    sink,
                    host,
                )?,
                Err(err) => fail(err, pc + 1, ctx, sink, host)?,
            },
            Instr::Test {
                condition,
                otherwise,
                loop_head,
            } => {
                if *loop_head && ctx.cancelled() {
                    Step::Stop(End::Cancelled)
                } else {
                    match eval_expr(condition, None, ctx) {
                        Ok(Value::Bit(true)) => Step::Go(pc + 1),
                        Ok(_) => Step::Go(*otherwise),
                        // An error of the condition counts as a condition that is not true.
                        Err(err) => fail(condition_error(err), *otherwise, ctx, sink, host)?,
                    }
                }
            }
            Instr::Jump { target } => jump(pc, *target, ctx),
            Instr::Return(value) => match return_code(*value, ctx) {
                Ok(code) => Step::Stop(End::Returned(code)),
                Err(err) => fail(err, pc + 1, ctx, sink, host)?,
            },
            Instr::Escape(escape) => Step::Stop(End::Escaped(escape_outcome(escape))),
        };
        match step {
            Step::Go(next) => pc = next,
            Step::Stop(end) => return Ok(end),
        }
    }
    Ok(End::Completed(answer))
}

/// What running one statement reached.
enum Reached {
    /// The statement answered.
    Outcome(ExecOutcome),
    /// A program the statement compiled at run time ended the run.
    Ended(End),
}

/// Runs one statement that holds no other one: through [`crate::execute`] for a program,
/// as `control.rs` ran a nested statement for the statement tree.
fn run_statement<'a>(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
    host: &mut Host<'_>,
) -> SqlResult<Reached> {
    if let PhysicalStatement::Deferred(deferred) = stmt {
        return run_deferred(deferred, ctx, sink, host);
    }
    // In the statement tree, a statement of the control of flow runs inside the frame of
    // the statement that holds it.
    let in_frame = matches!(host, Host::Tree)
        && matches!(
            stmt,
            PhysicalStatement::Declare(_)
                | PhysicalStatement::SetVariable { .. }
                | PhysicalStatement::SelectAssign { .. }
                | PhysicalStatement::Print(_)
        );
    let outcome = if in_frame {
        control::execute(stmt, ctx, sink)?
    } else {
        crate::execute(stmt, ctx, sink)?
    };
    Ok(Reached::Outcome(outcome))
}

/// Binds, plans and checks a statement the binder left unbound, then runs what it turned
/// out to be: a statement of its own, or a program of its own when it holds others.
fn run_deferred<'a>(
    deferred: &DeferredStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
    host: &mut Host<'_>,
) -> SqlResult<Reached> {
    let statement = control::bind_and_plan(deferred, ctx)?;
    compile_checks(&statement, ctx)?;
    match statement {
        PhysicalStatement::If { .. }
        | PhysicalStatement::While { .. }
        | PhysicalStatement::Block(_)
        | PhysicalStatement::Goto { .. }
        | PhysicalStatement::Label { .. }
        | PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Return(_)
        | PhysicalStatement::Deferred(_) => {
            let tail = matches!(host, Host::Tree);
            let program = compile(std::slice::from_ref(&statement), Labels::Open, tail)?;
            Ok(match run(&program, ctx, sink, host)? {
                End::Completed(outcome) | End::Escaped(outcome) => Reached::Outcome(outcome),
                end => Reached::Ended(end),
            })
        }
        statement => run_statement(&statement, ctx, sink, host),
    }
}

/// Acts on what a statement answered at position `pc`.
#[expect(
    clippy::too_many_arguments,
    reason = "the state of the program counter, passed rather than bundled for one caller"
)]
fn after_statement<'a>(
    program: &Program<'_>,
    pc: usize,
    enclosing: Option<usize>,
    outcome: ExecOutcome,
    tail: bool,
    answer: &mut ExecOutcome,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
    host: &mut Host<'_>,
) -> SqlResult<Step> {
    let reports = matches!(host, Host::Calls(_));
    Ok(match outcome {
        ExecOutcome::Rows(count) => {
            if reports {
                let mut nocount = false;
                if let Some(session) = ctx.session.as_deref_mut() {
                    session.rowcount = i64::try_from(count).unwrap_or(i64::MAX);
                    nocount = session.nocount;
                }
                sink.done(if nocount { None } else { Some(count) })?;
            }
            if tail {
                *answer = ExecOutcome::Rows(count);
            }
            Step::Go(pc + 1)
        }
        ExecOutcome::NoRows => {
            if reports {
                sink.done(None)?;
            }
            if tail {
                *answer = ExecOutcome::NoRows;
            }
            Step::Go(pc + 1)
        }
        ExecOutcome::Cancelled => Step::Stop(End::Cancelled),
        ExecOutcome::Return(code) => Step::Stop(End::Returned(code)),
        ExecOutcome::BatchAbort(err) => match host {
            Host::Tree => Step::Stop(End::Escaped(ExecOutcome::BatchAbort(err))),
            Host::Calls(_) => {
                report(&err, ctx, sink)?;
                Step::Stop(End::Aborted(err))
            }
        },
        call @ (ExecOutcome::CallProcedure { .. } | ExecOutcome::RunDynamic { .. }) => match host {
            Host::Tree => Step::Stop(End::Escaped(call)),
            Host::Calls(handler) => {
                let Some(call) = Call::from_outcome(call) else {
                    return Err(bug("interp: a call outcome that names no call"));
                };
                let frame = match Frame::enter(ctx) {
                    Ok(frame) => frame,
                    Err(err) => return fail(err, pc + 1, ctx, sink, host),
                };
                let ended = handler.call(call, ctx, sink);
                let left = frame.leave(ctx, &mut **handler);
                match (ended, left) {
                    (Ok(ProgramEnd::Completed | ProgramEnd::Returned(_)), Ok(())) => {
                        Step::Go(pc + 1)
                    }
                    (Ok(ProgramEnd::Completed | ProgramEnd::Returned(_)), Err(err)) => {
                        fail(err, pc + 1, ctx, sink, host)?
                    }
                    (Err(err), left) => {
                        let step = fail(err, pc + 1, ctx, sink, host)?;
                        report_left(left, ctx, sink)?;
                        step
                    }
                    (Ok(ProgramEnd::Cancelled), left) => {
                        report_left(left, ctx, sink)?;
                        Step::Stop(End::Cancelled)
                    }
                    (Ok(ProgramEnd::Aborted(err)), left) => {
                        report_left(left, ctx, sink)?;
                        Step::Stop(End::Aborted(err))
                    }
                }
            }
        },
        // A jump a program compiled at run time could not resolve: this program resolves
        // it if it can, and lets it leave otherwise.
        ExecOutcome::Break => match enclosing.and_then(|number| program.loops.get(number)) {
            Some(lp) => Step::Go(lp.exit),
            None => escape(ExecOutcome::Break, pc, ctx, sink, host)?,
        },
        ExecOutcome::Continue => match enclosing.and_then(|number| program.loops.get(number)) {
            Some(lp) => jump(pc, lp.head, ctx),
            None => escape(ExecOutcome::Continue, pc, ctx, sink, host)?,
        },
        ExecOutcome::Goto(label) => match program.labels.get(&label.to_ascii_lowercase()) {
            Some(&target) => jump(pc, target, ctx),
            None => escape(ExecOutcome::Goto(label), pc, ctx, sink, host)?,
        },
    })
}

/// A jump this program does not resolve either: it leaves the statement tree, and is an
/// error of a program, which holds the whole body.
fn escape<'a>(
    outcome: ExecOutcome,
    pc: usize,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
    host: &mut Host<'_>,
) -> SqlResult<Step> {
    match host {
        Host::Tree => Ok(Step::Stop(End::Escaped(outcome))),
        Host::Calls(_) => fail(escape_error(&outcome), pc + 1, ctx, sink, host),
    }
}

/// The error a jump that leaves a whole program answers.
pub(crate) fn escape_error(outcome: &ExecOutcome) -> SqlError {
    match outcome {
        ExecOutcome::Goto(label) => SqlError::goto_label_not_declared(label),
        ExecOutcome::Break => SqlError::break_without_while(),
        ExecOutcome::Continue => SqlError::continue_without_while(),
        other => bug(&format!("interp: {other:?} left a program")),
    }
}

/// The outcome an [`Instr::Escape`] leaves with.
fn escape_outcome(escape: &Escape) -> ExecOutcome {
    match escape {
        Escape::Goto(label) => ExecOutcome::Goto(label.clone()),
        Escape::Break => ExecOutcome::Break,
        Escape::Continue => ExecOutcome::Continue,
    }
}

/// Goes from `pc` to `target`, reading the token first when the jump goes backward.
fn jump(pc: usize, target: usize, ctx: &ExecContext<'_>) -> Step {
    if target <= pc && ctx.cancelled() {
        Step::Stop(End::Cancelled)
    } else {
        Step::Go(target)
    }
}

/// An error a statement, a condition or a call raised (module documentation of `interp`,
/// "Running it"). In the statement tree it leaves as `Err`; in a program it goes to the
/// sink, and the program goes to `next` when the error lets it go on.
fn fail<'a>(
    err: SqlError,
    next: usize,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
    host: &mut Host<'_>,
) -> SqlResult<Step> {
    if matches!(host, Host::Tree) {
        return Err(err);
    }
    report(&err, ctx, sink)?;
    let xact_abort = ctx
        .session
        .as_deref()
        .is_some_and(|session| session.xact_abort);
    let goes_on = !xact_abort && err.number != 0 && err.batch_scope() == BatchErrorScope::Statement;
    Ok(if goes_on {
        Step::Go(next)
    } else {
        Step::Stop(End::Aborted(err))
    })
}

/// Hands the error of [`CallHandler::restore_database`] to the sink when the call already
/// ended the program or failed itself: the caller does not lose it, and its end stays the
/// one the call decided.
fn report_left(
    left: SqlResult<()>,
    ctx: &mut ExecContext<'_>,
    sink: &mut dyn StatementSink,
) -> SqlResult<()> {
    match left {
        Ok(()) => Ok(()),
        Err(err) => report(&err, ctx, sink),
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

/// The integer a `RETURN e` returns: `e` converted to `int`, `NULL` counting as 0.
fn return_code(value: Option<&BoundExpr>, ctx: &mut ExecContext<'_>) -> SqlResult<i32> {
    let Some(expr) = value else {
        return Ok(0);
    };
    let value = eval_expr(expr, None, ctx)?;
    let int = TypeInfo::new(SqlType::Int, false);
    let converted = convert(&value, &expr.ty, &int, None).map_err(|err| at(err, expr.line))?;
    Ok(match converted {
        Value::I32(n) => n,
        _ => 0,
    })
}

/// An error raised while the condition of an `IF` or a `WHILE` is evaluated belongs to
/// that statement, not to the sub-expression that failed: its line is cleared here, and the
/// layer that runs the batch puts the statement's own line on it, whether the condition sits
/// on its own line or is folded onto the line of the first statement held
/// (`crates/vauban-session/tests/run_batch_pipeline.rs`,
/// `an_error_of_a_condition_carries_the_line_of_the_statement_that_holds_it`).
fn condition_error(err: SqlError) -> SqlError {
    err.with_line(0)
}

/// The internal error 50000 for a broken precondition.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
