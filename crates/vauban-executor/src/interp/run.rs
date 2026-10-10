//! The program counter: runs the instructions of a [`Program`] through a [`Host`] (module
//! documentation of `interp`).

use std::ops::Range;

use vauban_binder::DeferredStatement;
use vauban_errors::{ErrorScope, InternalError, SqlError, SqlResult, error_behavior};
use vauban_planner::{PhysicalStatement, StatementExpr};
use vauban_types::{SqlType, TypeInfo, Value, convert};

use crate::compile::compile as compile_checks;
use crate::context::ExecContext;
use crate::control;
use crate::errors::at;
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
    /// An error ended the program and its batch; it is in the sink already.
    Aborted(SqlError),
    /// An error ended the level of the program alone; it is in the sink already.
    LevelAborted(SqlError),
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
    Ok(match run_from(program, 0, host, Rebinding::Here)? {
        Ended::End(end) => end,
        // A program that binds its own roots again hands no binding upward.
        Ended::Rebind { .. } => End::Aborted(bug("interp: a root to bind again left its program")),
    })
}

/// What a program does with a statement that came back with [`Ran::Rebind`] (module
/// documentation of `interp`, "A statement bound again").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rebinding {
    /// It asks the host to bind again the root that holds the statement, and runs the rest
    /// of that root from the new binding.
    Here,
    /// It runs the new binding of a root: the program that holds the root binds it again.
    Above,
    /// It holds a statement bound when the program reached it, which is not bound again:
    /// the statement fails with the internal error.
    Refused,
}

/// How [`run_from`] ended.
enum Ended {
    /// The run ended.
    End(End),
    /// The statement at position `at` came back with [`Ran::Rebind`], in a program run
    /// with [`Rebinding::Above`].
    Rebind {
        /// Its position in the program.
        at: usize,
        /// Where it sits in the body.
        site: Site,
    },
}

/// A root of the program the host bound again, whose instructions run in place of the
/// ones of `range`.
struct Replacement {
    /// The position, in the body, of the root.
    root: usize,
    /// The instructions of the root in the program.
    range: Range<usize>,
    /// The new binding of the root.
    statement: Box<PhysicalStatement>,
}

/// Runs `program` from position `start`.
fn run_from(
    program: &Program<'_>,
    start: usize,
    host: &mut dyn Host,
    rebinding: Rebinding,
) -> SqlResult<Ended> {
    let mut pc = start;
    let mut answer = ExecOutcome::NoRows;
    let mut replaced: Vec<Replacement> = Vec::new();
    loop {
        if let Some(replacement) = replaced.iter().find(|r| r.range.contains(&pc)) {
            let step = match run_replacement(program, replacement, pc, host)? {
                Resumed::Step(step) => step,
                Resumed::Rebind { at, site } => {
                    rebind_root(program, &mut replaced, at, site, host)?
                }
            };
            match step {
                Step::Go(next) => pc = next,
                Step::Stop(end) => return Ok(Ended::End(end)),
            }
            continue;
        }
        let Some(instr) = program.code.get(pc) else {
            break;
        };
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
                    Reached::Failed(err) => go_on_or_stop(err, Origin::Statement, pc + 1, host)?,
                    Reached::Rebind => match rebinding {
                        Rebinding::Here => rebind_root(program, &mut replaced, pc, *site, host)?,
                        Rebinding::Above => {
                            return Ok(Ended::Rebind {
                                at: pc,
                                site: *site,
                            });
                        }
                        Rebinding::Refused => fail(gone_error(), pc + 1, *site, host)?,
                    },
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
                    let mut evaluate = |ctx: &mut ExecContext<'_>| {
                        control::eval_statement_expr(&condition.expr, &condition.subplans, ctx)
                            .map_err(|err| condition_error(err, condition.expr.line))
                    };
                    match host.condition(*site, &mut evaluate)? {
                        Ok(true) => {
                            host.control(*site)?;
                            Step::Go(pc + 1)
                        }
                        Ok(false) => {
                            host.control(*site)?;
                            Step::Go(*otherwise)
                        }
                        // An error of the condition counts as a condition that is not true.
                        Err(err) => go_on_or_stop(err, Origin::Statement, *otherwise, host)?,
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
            Step::Stop(end) => return Ok(Ended::End(end)),
        }
    }
    Ok(Ended::End(End::Completed(answer)))
}

/// What running the new binding of a root reached.
enum Resumed {
    /// The program counter goes on, or the run stops.
    Step(Step),
    /// A statement of it came back with [`Ran::Rebind`]: its position in the enclosing
    /// program, and where it sits in the body.
    Rebind { at: usize, site: Site },
}

/// Runs the new binding of a root from `pc`, a position inside the instructions it
/// replaces, up to the end of those instructions or to a jump that leaves them.
fn run_replacement(
    program: &Program<'_>,
    replacement: &Replacement,
    pc: usize,
    host: &mut dyn Host,
) -> SqlResult<Resumed> {
    let range = &replacement.range;
    let site = Site {
        root: replacement.root,
        nested: true,
    };
    let roots = [Some(replacement.statement.as_ref())];
    let sub = compile_roots(&roots, Labels::Open, false, Placement::Within(site.root))?;
    let ended = run_from(&sub, pc - range.start, host, Rebinding::Above)?;
    let last = range.end.saturating_sub(1);
    Ok(Resumed::Step(match ended {
        Ended::Rebind { at, site } => {
            return Ok(Resumed::Rebind {
                at: range.start + at,
                site,
            });
        }
        Ended::End(End::Completed(_)) => Step::Go(range.end),
        // A `GOTO` to a label written outside the root: this program lands on it, or lets
        // it leave as the instructions the root replaced would have.
        Ended::End(End::Escaped(ExecOutcome::Goto(label))) => {
            match program.labels.get(&label.to_ascii_lowercase()) {
                Some(&target) => jump(last, target, host),
                None => Step::Stop(End::Escaped(ExecOutcome::Goto(label))),
            }
        }
        Ended::End(end) => Step::Stop(end),
    }))
}

/// Asks the host to bind again the root that holds the statement at `pc`, which came back
/// with [`Ran::Rebind`], and goes back to `pc` to run it from the new binding.
///
/// The new binding replaces the instructions of the root when it compiles to the same
/// instructions, so that each position of the root keeps its statement: the statements
/// of the root that ran are not run again. Otherwise, the statement fails with the
/// internal error, as it does when the host cannot bind the root again.
fn rebind_root(
    program: &Program<'_>,
    replaced: &mut Vec<Replacement>,
    pc: usize,
    site: Site,
    host: &mut dyn Host,
) -> SqlResult<Step> {
    let statement = match host.rebind(site)? {
        Ok(statement) => statement,
        Err(err) => return go_on_or_stop(err, Origin::Statement, pc + 1, host),
    };
    let start = program
        .code
        .iter()
        .position(|instr| site_of(instr).is_some_and(|at| at.root == site.root));
    let range = start.and_then(|start| {
        let roots = [Some(&statement)];
        let sub = compile_roots(&roots, Labels::Open, false, Placement::Within(site.root)).ok()?;
        let range = start..start + sub.code.len();
        let old = program.code.get(range.clone())?;
        same_shape(old, start, &sub.code).then_some(range)
    });
    match range {
        Some(range) if range.contains(&pc) => {
            replaced.retain(|replacement| replacement.range != range);
            replaced.push(Replacement {
                root: site.root,
                range,
                statement: Box::new(statement),
            });
            Ok(Step::Go(pc))
        }
        _ => fail(gone_error(), pc + 1, site, host),
    }
}

/// Where the statement an instruction comes from sits in the body, `None` for a jump the
/// control of flow implies.
fn site_of(instr: &Instr<'_>) -> Option<Site> {
    match instr {
        Instr::Run { site, .. }
        | Instr::Test { site, .. }
        | Instr::Return { site, .. }
        | Instr::Escape { site, .. } => Some(*site),
        Instr::Jump { stated, .. } => *stated,
    }
}

/// Whether `new`, a root compiled alone, holds the instructions `old` holds from position
/// `start` of its program, its jumps counted from that position.
fn same_shape(old: &[Instr<'_>], start: usize, new: &[Instr<'_>]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|pair| match pair {
            (Instr::Run { .. }, Instr::Run { .. })
            | (Instr::Return { .. }, Instr::Return { .. })
            | (Instr::Jump { .. } | Instr::Escape { .. }, Instr::Escape { .. }) => true,
            (Instr::Test { otherwise: a, .. }, Instr::Test { otherwise: b, .. })
            | (Instr::Jump { target: a, .. }, Instr::Jump { target: b, .. }) => {
                a.checked_sub(start) == Some(*b)
            }
            _ => false,
        })
}

/// The internal error of a statement whose tables went before it ran, and which is not
/// bound again.
pub(crate) fn gone_error() -> SqlError {
    bug("a table this statement was bound against was altered or dropped before it ran")
}

/// What running one statement reached.
enum Reached {
    /// The statement answered; its end is in the sink.
    Outcome(ExecOutcome),
    /// The statement raised an error, not sent yet.
    Failed(SqlError),
    /// A program the statement compiled at run time ended the run.
    Ended(End),
    /// Nothing ran: a table the statement was bound against went first ([`Ran::Rebind`]).
    Rebind,
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
        Ran::Stopped(err) => Reached::Ended(stop(err, Origin::Ending, host)?),
        Ran::Reported(err) => Reached::Ended(stop(err, Origin::Reported, host)?),
        Ran::Holds(statement) => run_held(&statement, site, host, Rebinding::Here)?,
        Ran::Rebind => Reached::Rebind,
    })
}

/// Runs `statement`, which holds others, as a program of its own whose jumps the
/// enclosing program resolves. `rebinding` is [`Rebinding::Here`] for a root the host bound
/// when the program reached it, and [`Rebinding::Refused`] for a statement a root holds.
fn run_held(
    statement: &PhysicalStatement,
    site: Site,
    host: &mut dyn Host,
    rebinding: Rebinding,
) -> SqlResult<Reached> {
    let roots = [Some(statement)];
    let program = match compile_roots(&roots, Labels::Open, false, Placement::Within(site.root)) {
        Ok(program) => program,
        Err(err) => return Ok(Reached::Failed(host.locate(err, site))),
    };
    Ok(match run_from(&program, 0, host, rebinding)? {
        Ended::End(End::Completed(_)) => Reached::Outcome(ExecOutcome::NoRows),
        Ended::End(End::Escaped(outcome)) => Reached::Outcome(outcome),
        Ended::End(end) => Reached::Ended(end),
        Ended::Rebind { .. } => Reached::Failed(host.locate(gone_error(), site)),
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
        (Err(err), _) => return Ok(Reached::Failed(host.locate(err, site))),
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
        | PhysicalStatement::Deferred(_) => run_held(&statement, held, host, Rebinding::Refused),
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
            go_on_or_stop(host.locate(err, site), Origin::Batch, pc + 1, host)?
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
    let ended = match ended {
        Ok(ended) => ended,
        Err(err) => {
            report_left(left, site, host)?;
            return Err(err);
        }
    };
    Ok(match (ended, left) {
        (
            Ok(ProgramEnd::Completed | ProgramEnd::Returned(_) | ProgramEnd::LevelAborted(_)),
            Ok(()),
        ) => Step::Go(pc + 1),
        (
            Ok(ProgramEnd::Completed | ProgramEnd::Returned(_) | ProgramEnd::LevelAborted(_)),
            Err(err),
        ) => fail(err, pc + 1, site, host)?,
        // The `EXECUTE` itself failed: its scope decides, as for any statement.
        (Err(err), left) => {
            let step = go_on_or_stop(err, Origin::Statement, pc + 1, host)?;
            report_left(left, site, host)?;
            step
        }
        // The body of the callee was refused before it ran: its error, unsent, is decided
        // here, where the `EXECUTE` is written.
        (Ok(ProgramEnd::Refused(err)), left) => {
            let step = go_on_or_stop(err, Origin::Callee, pc + 1, host)?;
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
/// it"): the host puts its line on it, then [`decide`] hands it to the host, and the program
/// goes to `next` when the error lets it go on.
fn fail(err: SqlError, next: usize, site: Site, host: &mut dyn Host) -> SqlResult<Step> {
    go_on_or_stop(host.locate(err, site), Origin::Statement, next, host)
}

/// [`decide`], read as the step of the program counter: `next` when the error lets the
/// program go on, the end of the run otherwise.
fn go_on_or_stop(
    err: SqlError,
    origin: Origin,
    next: usize,
    host: &mut dyn Host,
) -> SqlResult<Step> {
    Ok(match decide(err, origin, host)? {
        None => Step::Go(next),
        Some(end) => Step::Stop(end),
    })
}

/// [`decide`] for an error that ends the program, its level at least.
fn stop(err: SqlError, origin: Origin, host: &mut dyn Host) -> SqlResult<End> {
    match decide(err, origin, host)? {
        Some(end) => Ok(end),
        None => Err(bug("interp: an error that ends the program let it go on")),
    }
}

/// Where an error handed to [`decide`] comes from, which bounds how far it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A statement of this program, a condition, or the interpreter itself: the scope of
    /// the error alone decides.
    Statement,
    /// A statement that ends the program: the scope of its error, widened to the level
    /// ([`Ran::Stopped`]).
    Ending,
    /// As [`Origin::Ending`], for an error the statement already handed to the sink
    /// ([`Ran::Reported`]): it is not sent twice.
    Reported,
    /// An error that ends the batch ([`ExecOutcome::BatchAbort`]).
    Batch,
    /// The body of a call, refused before its first statement ran ([`ProgramEnd::Refused`]):
    /// an error that stops its level stops the callee, which is the `EXECUTE` statement of
    /// this program, and this program goes on; a wider scope reaches this program too.
    Callee,
}

/// The point where an error a statement, a callee refused before it ran, or the
/// interpreter raised is acted on, before anything of it is sent (module documentation of
/// `interp`, "Running it"): how far it reaches is [`reach`] bounded by `origin`, the host
/// hands it to the sink with that reach ([`Host::raise`]) unless `origin` says it is there
/// already, and the answer is `None` when the program goes on with its next instruction,
/// the end of the run otherwise.
fn decide(err: SqlError, origin: Origin, host: &mut dyn Host) -> SqlResult<Option<End>> {
    let read = reach(&err, host.xact_abort());
    let reach = match origin {
        Origin::Statement => read,
        Origin::Ending | Origin::Reported => ending_reach(read),
        Origin::Batch => widest(read, ErrorScope::Batch),
        Origin::Callee => match read {
            ErrorScope::Statement | ErrorScope::Level => ErrorScope::Statement,
            wider => wider,
        },
    };
    if origin != Origin::Reported {
        host.raise(&err, reach)?;
    }
    Ok(match reach {
        ErrorScope::Statement => None,
        ErrorScope::Level => Some(End::LevelAborted(err)),
        ErrorScope::Batch | ErrorScope::Connection => Some(End::Aborted(err)),
    })
}

/// How far `err` reaches, `xact_abort` telling whether `SET XACT_ABORT ON` holds: the scope
/// the catalogue gives its number, widened to the batch under `XACT_ABORT`. A number the
/// catalogue does not hold reaches the batch, and the number 0 does not stop at its
/// statement.
pub(crate) fn reach(err: &SqlError, xact_abort: bool) -> ErrorScope {
    let scope = error_behavior(err.number).scope;
    match scope {
        ErrorScope::Statement | ErrorScope::Level if xact_abort => ErrorScope::Batch,
        ErrorScope::Statement if err.number == 0 => ErrorScope::Level,
        scope => scope,
    }
}

/// The reach of an error that ends its program: the statement alone is widened to the
/// level.
pub(crate) fn ending_reach(reach: ErrorScope) -> ErrorScope {
    widest(reach, ErrorScope::Level)
}

/// The wider of two reaches.
fn widest(a: ErrorScope, b: ErrorScope) -> ErrorScope {
    fn rank(scope: ErrorScope) -> u8 {
        match scope {
            ErrorScope::Statement => 0,
            ErrorScope::Level => 1,
            ErrorScope::Batch => 2,
            ErrorScope::Connection => 3,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
}

/// Hands the error of [`Host::leave_call`] to [`decide`] when the call already ended the
/// program or failed itself: the caller does not lose it, it is decided as the error of any
/// statement is, and the end of the program stays the one the call decided, since the
/// call's own error or end came first.
fn report_left(left: SqlResult<()>, site: Site, host: &mut dyn Host) -> SqlResult<()> {
    match left {
        Ok(()) => Ok(()),
        Err(err) => {
            let err = host.locate(err, site);
            decide(err, Origin::Statement, host).map(|_| ())
        }
    }
}

/// The integer a `RETURN e` returns: `e` converted to `int`, `NULL` counting as 0.
fn return_code(value: Option<&StatementExpr>, site: Site, host: &mut dyn Host) -> SqlResult<i32> {
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

/// The value of a `RETURN e`, in the context the host opened for it, the plans of its
/// subqueries installed while it is read.
pub(crate) fn evaluate_return(value: &StatementExpr, ctx: &mut ExecContext<'_>) -> SqlResult<i32> {
    let expr = &value.expr;
    let value = control::eval_statement_expr(expr, &value.subplans, ctx)?;
    let int = TypeInfo::new(SqlType::Int, false);
    let converted = convert(&value, &expr.ty, &int, None).map_err(|err| at(err, expr.line))?;
    Ok(match converted {
        Value::I32(n) => n,
        _ => 0,
    })
}

/// An error raised while the condition of an `IF` or a `WHILE` is evaluated belongs to
/// that statement, not to the sub-expression that failed: it takes `line`, the line the
/// condition starts on. The program holds no line of the statement itself, so a condition
/// written below its `IF` answers its own line.
pub(crate) fn condition_error(err: SqlError, line: u32) -> SqlError {
    err.with_line(line)
}

/// The internal error 50000 for a broken precondition.
pub(crate) fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}
