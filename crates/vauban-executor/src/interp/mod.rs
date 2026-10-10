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
//! An error a statement raises goes to the sink, followed by the end of that statement,
//! before anything else runs. Then the scope of the error decides: an error whose scope is
//! the statement ([`vauban_errors::BatchErrorScope::Statement`]) lets the program go on
//! with the next instruction, and any other error, or any error under `SET XACT_ABORT ON`,
//! ends the program with [`ProgramEnd::Aborted`]
//! (`interp::tests::a_duplicate_key_in_a_loop_of_three_turns_answers_three_errors`). An
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
//! loop runs once per turn (`interp::tests::a_loop_that_calls_three_times_runs_three_turns`).
//!
//! The callee runs in a level of its own: the program sets its own variables and options
//! aside in a frame before the handler runs, and takes them back after (`frame.rs`).
//!
//! # The statement tree
//!
//! `control.rs` runs a single `IF`, `WHILE` or `BEGIN … END` handed to [`crate::execute`]
//! through the same program, in a mode kept for the callers that still run a batch one
//! statement at a time: an error leaves as the `Err` of the statement, and a call, a
//! `GOTO` to a label the statement does not hold, a `BREAK` or a `CONTINUE` outside any
//! loop it holds leave as the [`ExecOutcome`] that names them. That mode ends with the
//! last of those callers.

mod compile;
mod frame;
mod run;

pub(crate) use frame::NameMap;

use std::collections::HashMap;

use vauban_errors::{SqlError, SqlResult};
use vauban_planner::PhysicalStatement;

use crate::context::ExecContext;
use crate::row::{EvaluatedExecArg, ExecOutcome};
use crate::sink::StatementSink;

use compile::{Instr, Labels, Loop};
use run::{End, Host};

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
        let mut host = Host::Calls(calls);
        Ok(match run::run(self, ctx, sink, &mut host)? {
            End::Completed(_) => ProgramEnd::Completed,
            End::Returned(code) => ProgramEnd::Returned(code),
            End::Cancelled => ProgramEnd::Cancelled,
            End::Aborted(err) => ProgramEnd::Aborted(err),
            // A closed program resolves each of its jumps, and a call is handed to
            // `calls`: what is left is a jump a statement bound at run time named.
            End::Escaped(outcome) => ProgramEnd::Aborted(run::escape_error(&outcome)),
        })
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
    /// An error ended the program; it was already handed to the sink.
    Aborted(SqlError),
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
    /// The handler hands the results of the callee to `sink` itself. A callee that ended by
    /// an error it already handed to the sink answers [`ProgramEnd::Aborted`], and the
    /// calling program ends there too; a callee cancelled answers
    /// [`ProgramEnd::Cancelled`].
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

/// Runs one `IF`, `WHILE`, `BEGIN … END`, `GOTO`, label, `BREAK`, `CONTINUE`, `RETURN` or
/// statement left unbound, handed to [`crate::execute`] as a statement of its own (module
/// documentation, "The statement tree").
///
/// The answer is the one of the statement the tree ran last when that statement is the
/// branch of an `IF` (the rows of `IF c SELECT 1`), `NoRows` otherwise, or the outcome
/// that leaves the tree.
pub(crate) fn run_tree<'a>(
    stmt: &PhysicalStatement,
    ctx: &mut ExecContext<'a>,
    sink: &mut dyn StatementSink,
) -> SqlResult<ExecOutcome> {
    let program = compile::compile(std::slice::from_ref(stmt), Labels::Open, true)?;
    Ok(match run::run(&program, ctx, sink, &mut Host::Tree)? {
        End::Completed(outcome) | End::Escaped(outcome) => outcome,
        End::Returned(code) => ExecOutcome::Return(code),
        End::Cancelled => ExecOutcome::Cancelled,
        End::Aborted(err) => ExecOutcome::BatchAbort(err),
    })
}

#[cfg(test)]
mod tests;
