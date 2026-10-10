//! The procedural interpreter: a body of statements compiled into a linear program of
//! instructions and jumps, run with a program counter.
//!
//! # The program
//!
//! [`Program::compile`] walks a list of planned statements once and flattens the
//! control of flow into jumps (`compile.rs`):
//!
//! * `IF c s1 ELSE s2` becomes a test of `c` that jumps over `s1` when `c` is not true,
//!   `s1`, a jump over `s2`, then `s2`;
//! * `WHILE c s` becomes a test at the head of the loop that jumps past it when `c` is not
//!   true, the body, and a jump back to the head; `BREAK` jumps past the loop, `CONTINUE`
//!   back to its head;
//! * `BEGIN … END` adds nothing: its statements follow one another;
//! * a label is the position of the instruction written after it, whatever the depth of
//!   the block that holds it, and a `GOTO` is a jump to that position. A `GOTO` may leave a
//!   loop, or enter a block, a branch or a loop body from outside
//!   (`interp::tests::a_goto_into_a_loop_body_runs_the_rest_of_the_body`). A name no label
//!   of the program declares answers 133 when the program is compiled, before anything
//!   runs (`interp::tests::a_goto_to_an_unknown_label_answers_133_before_anything_runs`). A
//!   name declared twice keeps its first position.
//!
//! # Running it
//!
//! [`Program::run`] moves a program counter over the instructions. Each statement that
//! holds no other statement runs through [`crate::execute`], with its compile-time checks
//! and its statement frame, and its results go to the [`StatementSink`]; the interpreter
//! closes it with [`StatementSink::done`], the row count of a result set included.
//!
//! An error a statement raises comes back to the interpreter unsent, and reaches the sink
//! once the interpreter has decided how far it reaches (`run.rs`, `decide`). The scope of
//! the error decides: an error whose scope is the statement
//! ([`vauban_errors::ErrorScope::Statement`]) lets the program go on with the next
//! instruction, and any other error, or any error under `SET XACT_ABORT ON`, ends the
//! program
//! (`interp::tests::a_duplicate_key_in_a_loop_of_three_turns_answers_three_errors`). How
//! far that end reaches is read from the scope too ([`ProgramEnd::ended_by`]): an error that
//! stops its level ([`vauban_errors::ErrorScope::Level`], 208 for instance) ends this
//! program with [`ProgramEnd::LevelAborted`], and the caller of a procedure or of a dynamic
//! text goes on; an error that stops the batch, or any error under `SET XACT_ABORT ON`, ends
//! it with [`ProgramEnd::Aborted`], and each caller stops too
//! (`interp::tests::a_callee_ended_by_an_error_of_its_level_lets_the_caller_go_on`). Once
//! decided, the error goes to the host ([`Host::raise`]) with its reach, and the host closes
//! its statement as that reach requires
//! (`interp::tests::an_error_reaches_the_host_with_the_reach_the_interpreter_decided`). An
//! error raised while a condition is evaluated counts as a condition that is not true: an
//! `IF` takes its `ELSE`, a `WHILE` ends.
//!
//! The cancellation token is read before each test at the head of a loop and at each
//! backward jump, so a loop written with a `WHILE` or with a `GOTO` ends by
//! [`ProgramEnd::Cancelled`] once the caller raised it.
//!
//! # Calls
//!
//! An `EXECUTE` is evaluated where it is written, then handed to the [`CallHandler`] the
//! caller supplied, which runs the procedure or the dynamic text in place and answers how
//! it ended. The program goes on with the next instruction, so a call in the body of a
//! loop runs once per turn (`interp::tests::a_loop_that_calls_three_times_runs_three_turns`),
//! unless the callee ended by [`ProgramEnd::Aborted`] or [`ProgramEnd::Cancelled`].
//!
//! The callee runs in a level of its own: the program sets its own variables and options
//! aside in a frame before the handler runs, and takes them back after (`frame.rs`).
//!
//! # A host that opens a context per statement
//!
//! [`Program::run`] runs the instructions in the one [`ExecContext`] its caller built.
//! [`Program::run_hosted`] hands each step to a [`Host`] instead: the host runs each
//! statement that holds no other one in a context, a transaction and a sink of its own,
//! evaluates each condition, reports each error the interpreter raises, and runs each call.
//! The interpreter keeps what is common to both: the program counter, the jumps, the
//! labels, the decision that an error lets the program go on or ends it, and the level a
//! call opens (`frame.rs`). A body the host prepared itself, statement by statement, is
//! compiled by [`Program::compile_roots`]: a root the host leaves as `None` is run by the
//! host from its position.
//!
//! # A statement run alone
//!
//! `control.rs` runs a single `IF`, `WHILE` or `BEGIN … END` handed to [`crate::execute`]
//! as a closed program of its own (`single.rs`). Each statement it holds reports its own
//! end; an error that ends the program leaves as the `Err` of the statement, and a call it
//! holds is refused with the internal error, since nothing there can run it.

mod compile;
mod frame;
mod run;
mod single;

pub(crate) use frame::NameMap;
pub(crate) use run::evaluate_return;
pub(crate) use single::run_alone;

use std::collections::HashMap;

use vauban_errors::{ErrorScope, SqlError, SqlResult};
use vauban_planner::PhysicalStatement;
use vauban_types::Value;

use crate::context::ExecContext;
use crate::row::{EvaluatedExecArg, ExecOutcome};
use crate::sink::StatementSink;

use compile::{Instr, Labels, Loop, Placement};
use run::End;
use single::SingleContext;

/// A body of statements compiled into instructions and jumps (module documentation).
///
/// It borrows the statements it was compiled from.
#[derive(Debug)]
pub struct Program<'s> {
    /// The instructions, in the order the program counter visits them when no jump is
    /// taken.
    code: Vec<Instr<'s>>,
    /// The position of each label, keyed by its name folded to lower case.
    labels: HashMap<String, usize>,
    /// The head and the exit of each `WHILE`, indexed by the loop number the instructions
    /// carry.
    loops: Vec<Loop>,
}

impl<'s> Program<'s> {
    /// Compiles `statements`, the body of a batch, of a procedure or of a dynamic text.
    ///
    /// # Errors
    ///
    /// 133 for a `GOTO` that names a label the body does not declare, 135 for a `BREAK`
    /// and 136 for a `CONTINUE` written outside any `WHILE`.
    pub fn compile(statements: &'s [PhysicalStatement]) -> SqlResult<Self> {
        compile::compile(statements, Labels::Closed, false)
    }

    /// Compiles a body whose statements the host prepared one by one: a root that is a
    /// statement is compiled as [`Program::compile`] compiles it, and a root left `None`
    /// becomes one instruction the [`Host`] runs from its position (module
    /// documentation, "A host that opens a context per statement").
    ///
    /// # Errors
    ///
    /// Those of [`Program::compile`], over the labels and the jumps of the roots that are
    /// statements.
    pub fn compile_roots(roots: &[Option<&'s PhysicalStatement>]) -> SqlResult<Self> {
        compile::compile_roots(roots, Labels::Closed, false, Placement::Body)
    }

    /// Runs the program against `ctx`, its results into `sink`, its calls through
    /// `calls` (module documentation).
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take what it is handed, and the internal
    /// error of a broken precondition. The errors the statements raise are not among
    /// them: they go to the sink.
    pub fn run(
        &self,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn StatementSink,
        calls: &mut dyn CallHandler,
    ) -> SqlResult<ProgramEnd> {
        let mut host = SingleContext::new(ctx, sink, calls);
        Ok(program_end(run::run(self, &mut host)?))
    }

    /// Runs the program through `host`, which opens the context of each step (module
    /// documentation, "A host that opens a context per statement").
    ///
    /// # Errors
    ///
    /// The errors of the host that end the run at once: a sink that cannot take what it is
    /// handed, a cancelled request, a broken precondition. The errors the statements raise
    /// are not among them: the host reports them.
    pub fn run_hosted(&self, host: &mut dyn Host) -> SqlResult<ProgramEnd> {
        Ok(program_end(run::run(self, host)?))
    }
}

/// What the end of a run is to the caller of a whole program.
fn program_end(end: End) -> ProgramEnd {
    match end {
        End::Completed(_) => ProgramEnd::Completed,
        End::Returned(code) => ProgramEnd::Returned(code),
        End::Cancelled => ProgramEnd::Cancelled,
        End::Aborted(err) => ProgramEnd::Aborted(err),
        End::LevelAborted(err) => ProgramEnd::LevelAborted(err),
        // A closed program resolves each of its jumps: what is left is a jump a statement
        // bound at run time named.
        End::Escaped(outcome) => ProgramEnd::Aborted(run::escape_error(&outcome)),
    }
}

/// How a program, or a call it made, ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgramEnd {
    /// The program counter went past the last instruction.
    Completed,
    /// A `RETURN` ran, with the status it returned (0 for a bare `RETURN`).
    Returned(i32),
    /// The caller raised the cancellation token.
    Cancelled,
    /// An error ended the program and the batch that holds it; it was already handed to
    /// the sink. The caller of a procedure or of a dynamic text that ended so stops too.
    Aborted(SqlError),
    /// An error ended the level of the program and that level alone; it was already handed
    /// to the sink. The caller of a procedure or of a dynamic text that ended so goes on
    /// with its next statement.
    LevelAborted(SqlError),
    /// The body of a call was refused before its first statement ran, by an error raised
    /// while it was prepared (its syntax, a `GOTO` to a label it does not declare, its
    /// binding). The error is not sent yet: the program that made the call decides it where
    /// the `EXECUTE` is written, then hands it to its host
    /// (`interp::tests::a_callee_refused_before_it_ran_is_decided_by_the_caller`).
    Refused(SqlError),
}

impl ProgramEnd {
    /// How a program that `err` ended ends, `xact_abort` telling whether `SET XACT_ABORT ON`
    /// held in it: [`ProgramEnd::LevelAborted`] when the scope of the error does not stop a
    /// caller ([`vauban_errors::ErrorScope::stops_the_caller`]) and `XACT_ABORT` is off,
    /// [`ProgramEnd::Aborted`] otherwise.
    #[must_use]
    pub fn ended_by(err: SqlError, xact_abort: bool) -> Self {
        match run::ending_reach(run::reach(&err, xact_abort)) {
            ErrorScope::Level => Self::LevelAborted(err),
            _ => Self::Aborted(err),
        }
    }
}

/// A call an `EXECUTE` of the program asks for, its arguments evaluated.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `EXEC proc …`.
    Procedure {
        /// Normalised procedure name, or the value of a procedure variable, lower-cased.
        name: String,
        /// Database part written left of the procedure name, as written.
        database: Option<String>,
        /// Arguments in written order.
        args: Vec<EvaluatedExecArg>,
        /// The `@rc =` variable receiving the return status, when the form was written.
        return_into: Option<String>,
        /// Line of the statement.
        line: u32,
    },
    /// `EXEC ('text')`.
    Dynamic {
        /// The text to run.
        text: String,
        /// Line of the statement.
        line: u32,
    },
}

impl Call {
    /// The call an `EXECUTE` answered, `None` for any other outcome.
    fn from_outcome(outcome: ExecOutcome) -> Option<Self> {
        match outcome {
            ExecOutcome::CallProcedure {
                name,
                database,
                args,
                return_into,
                line,
            } => Some(Self::Procedure {
                name,
                database,
                args,
                return_into,
                line,
            }),
            ExecOutcome::RunDynamic { text, line } => Some(Self::Dynamic { text, line }),
            _ => None,
        }
    }
}

/// What runs the calls of a program, supplied by the caller of [`Program::run`].
pub trait CallHandler {
    /// Runs `call` in place and answers how it ended.
    ///
    /// The call runs in a level of its own, opened before this method is called: the
    /// variables of `ctx` are empty, the handler enters the parameters of the callee there,
    /// and the options are those of the caller. The level is closed after this method
    /// returns, so a value meant for the caller is not written here.
    ///
    /// The handler hands the results of the callee to `sink` itself. A body refused before
    /// its first statement ran answers [`ProgramEnd::Refused`] with its error unsent, and
    /// the program decides it. A callee that ended by
    /// an error it already handed to the sink answers [`ProgramEnd::ended_by`] that error:
    /// [`ProgramEnd::LevelAborted`] lets the calling program go on with its next
    /// instruction, [`ProgramEnd::Aborted`] ends it too; a callee cancelled answers
    /// [`ProgramEnd::Cancelled`], which ends it as well.
    ///
    /// # Errors
    ///
    /// The error of the `EXECUTE` statement itself, a procedure that does not exist for
    /// instance: the interpreter hands it to the sink and decides from its scope whether
    /// the program goes on.
    fn call(
        &mut self,
        call: Call,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn StatementSink,
    ) -> SqlResult<ProgramEnd>;

    /// Makes `database` the current database again once a call returned: the one the
    /// caller had when the call was entered. A `USE` the callee ran ends with it.
    ///
    /// # Errors
    ///
    /// The error of the change of database; the interpreter hands it to the sink and
    /// decides from its scope whether the program goes on.
    fn restore_database(&mut self, database: &str, ctx: &mut ExecContext<'_>) -> SqlResult<()>;
}

/// Where a statement of a program sits in the body a [`Host`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    /// The position, in the body, of the statement written at its top level that holds
    /// it, or that it is.
    pub root: usize,
    /// The statement is held by that root (a branch, a loop body, a block, or a statement
    /// the root turned into once bound) rather than being the root itself.
    pub nested: bool,
}

/// What a statement a [`Host`] ran answered.
#[derive(Debug)]
pub enum Ran {
    /// It ended with this outcome; its results and its end are in the sink. An outcome
    /// that names a call, a `RETURN`, a cancellation or an error of the whole batch is the
    /// program's to act on.
    Outcome(ExecOutcome),
    /// It raised this error, on the line the client reads it at. Nothing of the error is in
    /// the sink yet: the interpreter decides how far it reaches, then hands it back to
    /// [`Host::raise`].
    Failed(SqlError),
    /// It raised this error, on the line the client reads it at, and the program ends with
    /// it: its level at least, its batch when the scope of the error or `SET XACT_ABORT ON`
    /// says so. Nothing of the error is in the sink yet, as for [`Ran::Failed`].
    Stopped(SqlError),
    /// It ended by this error, which the statement handed to the sink itself with its end,
    /// and the program ends with it as with [`Ran::Stopped`], the error not sent twice.
    Reported(SqlError),
    /// The statement at this position, bound when the program reached it, holds others:
    /// the program runs it in place, its jumps resolved against the labels of the body.
    Holds(Box<PhysicalStatement>),
}

/// What runs the steps of a program for [`Program::run_hosted`] (module documentation, "A
/// host that opens a context per statement").
pub trait Host {
    /// Runs one statement that holds no other one: `stmt`, or the root at `site` when
    /// `stmt` is `None`. The results and the end of a statement that succeeded go to the
    /// host's sink; the error a statement raises comes back in [`Ran::Failed`] or
    /// [`Ran::Stopped`], unsent.
    ///
    /// # Errors
    ///
    /// An error that ends the run at once (module documentation of [`Program::run_hosted`]).
    fn statement(&mut self, stmt: Option<&PhysicalStatement>, site: Site) -> SqlResult<Ran>;

    /// Evaluates the condition of an `IF` or of a `WHILE` written at `site` by running
    /// `evaluate` in a context opened for it: `Ok(true)` when it is true, `Ok(false)` when
    /// it is false or unknown, `Err` with the error the evaluation raised, on the line the
    /// client reads it at and unsent, as for [`Ran::Failed`].
    ///
    /// `evaluate` is the interpreter's: it installs the plans of the subqueries the
    /// condition holds while it reads it, as for the value of a `SET`
    /// (`tests/subquery.rs`, `an_if_reads_an_exists_nested_in_an_exists`), and answers an
    /// error on the line the condition starts on.
    ///
    /// # Errors
    ///
    /// An error that ends the run at once.
    fn condition(
        &mut self,
        site: Site,
        evaluate: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<Value>,
    ) -> SqlResult<Result<bool, SqlError>>;

    /// Runs `step` in a context opened for it, then closes that context: the value of a
    /// `RETURN`, the binding of a statement left unbound, the opening and the closing of
    /// the level of a call.
    ///
    /// # Errors
    ///
    /// The error `step` answers, and the error of opening or closing the context.
    fn within(
        &mut self,
        site: Site,
        step: &mut dyn FnMut(&mut ExecContext<'_>) -> SqlResult<()>,
    ) -> SqlResult<()>;

    /// Puts on `err`, an error the interpreter raised itself at `site` (a jump that leaves
    /// the program, the value of a `RETURN`, the level of a call), the line the client reads
    /// it at.
    fn locate(&self, err: SqlError, site: Site) -> SqlError;

    /// Hands `err` to the sink, with the end of its statement, once the interpreter decided
    /// how far it reaches: [`ErrorScope::Statement`] when the program goes on with its next
    /// instruction, [`ErrorScope::Level`] when the program ends and the caller of its level
    /// goes on, a wider scope when the batch ends with it. `@@ERROR` takes its number.
    ///
    /// It is the path of an error of a level to the client, apart from an RPC, the
    /// `WAITFOR` fall-back ([`Ran::Reported`]) and the 266 a called level raises when it ends
    /// with another transaction count: the error of a statement, of a condition, of
    /// an `EXECUTE`, and the error that refused the body of a callee before it ran
    /// ([`ProgramEnd::Refused`]) each reach the client through it
    /// (`interp::tests::an_error_reaches_the_host_with_the_reach_the_interpreter_decided`,
    /// `interp::tests::a_callee_refused_before_it_ran_is_decided_by_the_caller`, and in the
    /// session `tests/nested.rs`,
    /// `a_dynamic_text_refused_before_it_ran_keeps_its_tokens_at_each_depth`).
    ///
    /// # Errors
    ///
    /// The error of the sink.
    fn raise(&mut self, err: &SqlError, reach: ErrorScope) -> SqlResult<()>;

    /// Runs `call`, made at `site`, in the level the interpreter opened for it, as
    /// [`CallHandler::call`] does: `Ok` with how the callee ended, or `Err` with the error
    /// of the `EXECUTE` statement itself (a procedure that does not exist, for instance),
    /// on the line the client reads it at and unsent, as for [`Ran::Failed`].
    ///
    /// # Errors
    ///
    /// An error that ends the run at once.
    fn call(&mut self, call: Call, site: Site) -> SqlResult<Result<ProgramEnd, SqlError>>;

    /// The level of the call is closed and the caller's state is back: makes `database`
    /// the current database again and writes what the callee hands back to its caller.
    ///
    /// # Errors
    ///
    /// The error of the change of database; the interpreter reports it.
    fn leave_call(&mut self, database: &str) -> SqlResult<()>;

    /// A statement of the control of flow written at `site` ended: the test of an `IF` or
    /// of a `WHILE` that was evaluated, a `GOTO`, a `BREAK`, a `CONTINUE` or a `RETURN`.
    /// A label and the bounds of a `BEGIN … END` are not statements that end.
    ///
    /// # Errors
    ///
    /// The error of the sink.
    fn control(&mut self, site: Site) -> SqlResult<()>;

    /// Whether the caller raised the cancellation token.
    fn cancelled(&self) -> bool;

    /// Whether `SET XACT_ABORT ON` holds, under which an error of statement scope ends the
    /// program too (`interp::tests::a_duplicate_key_under_xact_abort_ends_the_program`).
    fn xact_abort(&self) -> bool;
}

#[cfg(test)]
mod tests;
