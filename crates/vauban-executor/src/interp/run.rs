//! The program counter: runs the instructions of a [`Program`] through a [`Host`] (module
//! documentation of `interp`).

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

use super::compile::{Escape, Instr, Labels, Placement, compile_roots};
use super::frame::Frame;
use super::{Call, Host, Program, ProgramEnd, Ran, Site};

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
    /// A jump left a program compiled with [`Labels::Open`]: the enclosing program
    /// resolves it.
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
pub(crate) fn run(program: &Program<'_>, host: &mut dyn Host) -> SqlResult<End> {
    let mut pc = 0;
    let mut answer = ExecOutcome::NoRows;
    while let Some(instr) = program.code.get(pc) {
        let step = match instr {
            Instr::Run {
                stmt,
                site,
                tail,
                enclosing,
            } => {
                let reached = match stmt {
                    Some(PhysicalStatement::Deferred(deferred)) => {
                        run_deferred(deferred, *site, host)?
                    }
                    _ => run_statement(*stmt, *site, host)?,
                };
                match reached {
                    Reached::Ended(end) => Step::Stop(end),
                    Reached::Failed(err) => decide(err, pc + 1, host),
                    Reached::Outcome(outcome) => {
                        if *tail && matches!(outcome, ExecOutcome::Rows(_) | ExecOutcome::NoRows) {
                            answer = outcome.clone();
                        }
                        after_statement(program, pc, *site, *enclosing, outcome, host)?
                    }
                }
            }
            Instr::Test {
                condition,
                otherwise,
                loop_head,
                site,
            } => {
                if *loop_head && host.cancelled() {
                    Step::Stop(End::Cancelled)
                } else {
                    match host.condition(condition, *site)? {
                        Ok(true) => {
                            host.control(*site)?;
                            Step::Go(pc + 1)
                        }
                        Ok(false) => {
                            host.control(*site)?;
                            Step::Go(*otherwise)
                        }
                        // An error of the condition counts as a condition that is not true;
                        // the host reported it.
                        Err(err) => decide(err, *otherwise, host),
                    }
                }
            }
            Instr::Jump { target, stated } => {
                if let Some(site) = stated {
                    host.control(*site)?;
                }
                jump(pc, *target, host)
            }
            Instr::Return { value, site } => match return_code(*value, *site, host) {
                Ok(code) => {
                    host.control(*site)?;
                    Step::Stop(End::Returned(code))
                }
                Err(err) => fail(err, pc + 1, *site, host)?,
            },
            Instr::Escape { escape, site } => {
                host.control(*site)?;
                Step::Stop(End::Escaped(escape_outcome(escape)))
            }
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
    /// The statement answered; its end is in the sink.
    Outcome(ExecOutcome),
    /// The statement raised an error the sink holds already.
    Failed(SqlError),
    /// A program the statement compiled at run time ended the run.
    Ended(End),
}

/// Runs one statement that holds no other one through the host, or the program of a
/// statement the host bound when the program reached it.
fn run_statement(
    stmt: Option<&PhysicalStatement>,
    site: Site,
    host: &mut dyn Host,
) -> SqlResult<Reached> {
    Ok(match host.statement(stmt, site)? {
        Ran::Outcome(outcome) => Reached::Outcome(outcome),
        Ran::Failed(err) => Reached::Failed(err),
        Ran::Stopped(err) => Reached::Ended(End::Aborted(err)),
        Ran::Holds(statement) => run_held(&statement, site, host)?,
    })
}

/// Runs `statement`, which holds others, as a program of its own whose jumps the
/// enclosing program resolves.
fn run_held(statement: &PhysicalStatement, site: Site, host: &mut dyn Host) -> SqlResult<Reached> {
    let roots = [Some(statement)];
    let program = match compile_roots(&roots, Labels::Open, false, Placement::Within(site.root)) {
        Ok(program) => program,
        Err(err) => {
            host.report(&err, site)?;
            return Ok(Reached::Failed(err));
        }
    };
    Ok(match run(&program, host)? {
        End::Completed(_) => Reached::Outcome(ExecOutcome::NoRows),
        End::Escaped(outcome) => Reached::Outcome(outcome),
        end => Reached::Ended(end),
    })
}

/// Binds, plans and checks a statement the binder left unbound, then runs what it turned
/// out to be: a statement of its own, or a program of its own when it holds others.
fn run_deferred(
    deferred: &DeferredStatement,
    site: Site,
    host: &mut dyn Host,
) -> SqlResult<Reached> {
    let mut bound = None;
    let checked = host.within(site, &mut |ctx: &mut ExecContext<'_>| {
        let statement = control::bind_and_plan(deferred, ctx)?;
        compile_checks(&statement, ctx)?;
        bound = Some(statement);
        Ok(())
    });
    let statement = match (checked, bound) {
        (Ok(()), Some(statement)) => statement,
        (Err(err), _) => {
            host.report(&err, site)?;
            return Ok(Reached::Failed(err));
        }
        (Ok(()), None) => return Err(bug("interp: a deferred statement bound to nothing")),
    };
    let held = Site {
        nested: true,
        ..site
    };
    match statement {
        PhysicalStatement::If { .. }
        | PhysicalStatement::While { .. }
        | PhysicalStatement::Block(_)
        | PhysicalStatement::Goto { .. }
        | PhysicalStatement::Label { .. }
        | PhysicalStatement::Break
        | PhysicalStatement::Continue
        | PhysicalStatement::Return(_)
        | PhysicalStatement::Deferred(_) => run_held(&statement, held, host),
        statement => run_statement(Some(&statement), held, host),
    }
}

/// Acts on what a statement answered at position `pc`; its end is in the sink already.
fn after_statement(
    program: &Program<'_>,
    pc: usize,
    site: Site,
    enclosing: Option<usize>,
    outcome: ExecOutcome,
    host: &mut dyn Host,
) -> SqlResult<Step> {
    Ok(match outcome {
        ExecOutcome::Rows(_) | ExecOutcome::NoRows => Step::Go(pc + 1),
        ExecOutcome::Cancelled => Step::Stop(End::Cancelled),
        ExecOutcome::Return(code) => Step::Stop(End::Returned(code)),
        ExecOutcome::BatchAbort(err) => {
            host.report(&err, site)?;
            Step::Stop(End::Aborted(err))
        }
        call @ (ExecOutcome::CallProcedure { .. } | ExecOutcome::RunDynamic { .. }) => {
            let Some(call) = Call::from_outcome(call) else {
                return Err(bug("interp: a call outcome that names no call"));
            };
            run_call(call, pc, site, host)?
        }
        // A jump a program compiled at run time could not resolve: this program resolves
        // it if it can, and it is an error otherwise.
        ExecOutcome::Break => match enclosing.and_then(|number| program.loops.get(number)) {
            Some(lp) => Step::Go(lp.exit),
            None => escape(ExecOutcome::Break, pc, site, host)?,
        },
        ExecOutcome::Continue => match enclosing.and_then(|number| program.loops.get(number)) {
            Some(lp) => jump(pc, lp.head, host),
            None => escape(ExecOutcome::Continue, pc, site, host)?,
        },
        ExecOutcome::Goto(label) => match program.labels.get(&label.to_ascii_lowercase()) {
            Some(&target) => jump(pc, target, host),
            None => escape(ExecOutcome::Goto(label), pc, site, host)?,
        },
    })
}

/// Runs `call` in a level of its own: the frame is opened before the host runs it and
/// closed after, a callee that failed included
/// (`interp::frame::tests::a_callee_that_fails_still_gives_the_caller_its_state_back`).
fn run_call(call: Call, pc: usize, site: Site, host: &mut dyn Host) -> SqlResult<Step> {
    let mut frame = None;
    let entered = host.within(site, &mut |ctx: &mut ExecContext<'_>| {
        frame = Some(Frame::enter(ctx)?);
        Ok(())
    });
    let frame = match (entered, frame) {
        (Ok(()), Some(frame)) => frame,
        (Err(err), _) => return fail(err, pc + 1, site, host),
        (Ok(()), None) => return Err(bug("interp: a call level opened nothing")),
    };
    let ended = host.call(call, site);
    let left = leave(frame, site, host);
    Ok(match (ended, left) {
        (Ok(ProgramEnd::Completed | ProgramEnd::Returned(_)), Ok(())) => Step::Go(pc + 1),
        (Ok(ProgramEnd::Completed | ProgramEnd::Returned(_)), Err(err)) => {
            fail(err, pc + 1, site, host)?
        }
        (Err(err), left) => {
            let step = fail(err, pc + 1, site, host)?;
            report_left(left, site, host)?;
            step
        }
        (Ok(ProgramEnd::Cancelled), left) => {
            report_left(left, site, host)?;
            Step::Stop(End::Cancelled)
        }
        (Ok(ProgramEnd::Aborted(err)), left) => {
            report_left(left, site, host)?;
            Step::Stop(End::Aborted(err))
        }
    })
}

/// Closes the level of a call: the caller's state back, then its database.
fn leave(frame: Frame, site: Site, host: &mut dyn Host) -> SqlResult<()> {
    let mut frame = Some(frame);
    let mut database = None;
    host.within(site, &mut |ctx: &mut ExecContext<'_>| {
        if let Some(frame) = frame.take() {
            database = Some(frame.leave(ctx)?);
        }
        Ok(())
    })?;
    match database {
        Some(database) => host.leave_call(&database),
        None => Err(bug("interp: a call level closed nothing")),
    }
}

/// A jump this program does not resolve either: an error of the program, which holds the
/// whole body.
fn escape(outcome: ExecOutcome, pc: usize, site: Site, host: &mut dyn Host) -> SqlResult<Step> {
    fail(escape_error(&outcome), pc + 1, site, host)
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
fn jump(pc: usize, target: usize, host: &dyn Host) -> Step {
    if target <= pc && host.cancelled() {
        Step::Stop(End::Cancelled)
    } else {
        Step::Go(target)
    }
}

/// An error the interpreter raised at `site` (module documentation of `interp`, "Running
/// it"): the host reports it, and the program goes to `next` when the error lets it go on.
fn fail(err: SqlError, next: usize, site: Site, host: &mut dyn Host) -> SqlResult<Step> {
    host.report(&err, site)?;
    Ok(decide(err, next, host))
}

/// Whether an error the sink holds lets the program go on at `next`: its scope is the
/// statement and `SET XACT_ABORT ON` does not hold.
fn decide(err: SqlError, next: usize, host: &dyn Host) -> Step {
    let goes_on =
        !host.xact_abort() && err.number != 0 && err.batch_scope() == BatchErrorScope::Statement;
    if goes_on {
        Step::Go(next)
    } else {
        Step::Stop(End::Aborted(err))
    }
}

/// Hands the error of [`Host::leave_call`] to the sink when the call already ended the
/// program or failed itself: the caller does not lose it, and its end stays the one the
/// call decided.
fn report_left(left: SqlResult<()>, site: Site, host: &mut dyn Host) -> SqlResult<()> {
    match left {
        Ok(()) => Ok(()),
        Err(err) => host.report(&err, site),
    }
}

/// The integer a `RETURN e` returns: `e` converted to `int`, `NULL` counting as 0.
fn return_code(value: Option<&BoundExpr>, site: Site, host: &mut dyn Host) -> SqlResult<i32> {
    let Some(expr) = value else {
        return Ok(0);
    };
    let mut code = 0;
    host.within(site, &mut |ctx: &mut ExecContext<'_>| {
        code = evaluate_return(expr, ctx)?;
        Ok(())
    })?;
    Ok(code)
}

/// The value of a `RETURN e`, in the context the host opened for it.
pub(crate) fn evaluate_return(expr: &BoundExpr, ctx: &mut ExecContext<'_>) -> SqlResult<i32> {
    let value = eval_expr(expr, None, ctx)?;
    let int = TypeInfo::new(SqlType::Int, false);
    let converted = convert(&value, &expr.ty, &int, None).map_err(|err| at(err, expr.line))?;
    Ok(match converted {
        Value::I32(n) => n,
        _ => 0,
    })
}

/// An error raised while the condition of an `IF` or a `WHILE` is evaluated belongs to
/// that statement, not to the sub-expression that failed: its line is cleared here, and
/// the host puts the line of the statement that holds the condition on it.
pub(crate) fn condition_error(err: SqlError) -> SqlError {
    err.with_line(0)
}

/// The internal error 50000 for a broken precondition.
pub(crate) fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
