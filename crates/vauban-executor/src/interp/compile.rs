//! Flattens a list of planned statements into the instructions of a [`Program`].
//!
//! One pass over the statements emits the instructions; the jumps whose target is not
//! known yet, the exit of a loop and the position of a label written further down, are
//! kept aside and patched at the end of the pass (`compile/tests/mod.rs`).

use std::collections::HashMap;

use vauban_binder::BoundExpr;
use vauban_errors::{SqlError, SqlResult};
use vauban_planner::PhysicalStatement;

use super::Program;

/// One instruction of a [`Program`].
#[derive(Debug)]
pub(crate) enum Instr<'s> {
    /// Runs one statement that holds no other statement, or one the binder left unbound.
    Run {
        /// The statement.
        stmt: &'s PhysicalStatement,
        /// What it answers is what the whole statement tree answers (`super::run_tree`):
        /// it is reached from the root through branches of `IF`s, and through no block
        /// and no loop (`compile::tests::the_tail_is_what_the_branches_of_an_if_hold`).
        tail: bool,
        /// The innermost `WHILE` that holds it, by loop number: where a `BREAK` or a
        /// `CONTINUE` that a statement bound at run time answers goes.
        enclosing: Option<usize>,
    },
    /// Evaluates `condition` and goes to `otherwise` when it is not true, to the next
    /// instruction when it is.
    Test {
        /// The condition of the `IF` or of the `WHILE`.
        condition: &'s BoundExpr,
        /// Where the program goes when the condition is false or unknown.
        otherwise: usize,
        /// The test heads a loop: the cancellation token is read before it.
        loop_head: bool,
    },
    /// Goes to `target`; a jump backward reads the cancellation token first.
    Jump {
        /// The position the program counter takes.
        target: usize,
    },
    /// `RETURN`, with its value.
    Return(Option<&'s BoundExpr>),
    /// Leaves the program with this jump, which the program does not resolve: a program
    /// compiled with [`Labels::Open`]
    /// (`compile::tests::an_open_program_lets_a_jump_it_cannot_resolve_leave`).
    Escape(Escape),
}

/// A jump that leaves a program compiled with [`Labels::Open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Escape {
    /// A `GOTO` to a label the program does not declare, with the name as written.
    Goto(String),
    /// A `BREAK` outside any loop of the program.
    Break,
    /// A `CONTINUE` outside any loop of the program.
    Continue,
}

/// The head and the exit of one `WHILE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Loop {
    /// The position of the test at the head of the loop.
    pub(crate) head: usize,
    /// The position after the jump back to the head.
    pub(crate) exit: usize,
}

/// What a jump that the program does not resolve becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Labels {
    /// An error: 133, 135 or 136. The body is the whole of what runs.
    Closed,
    /// An [`Instr::Escape`]: the body is a statement of a list held by someone else.
    Open,
}

/// A jump whose target the end of the pass writes.
enum Fixup {
    /// The exit of a loop, by loop number.
    Exit(usize),
    /// The position of a label, by name as written.
    Label(String),
}

/// Compiles `statements` into a program (module documentation). `tail` marks the root
/// statements as the ones whose answer is the answer of the tree.
pub(crate) fn compile(
    statements: &[PhysicalStatement],
    labels: Labels,
    tail: bool,
) -> SqlResult<Program<'_>> {
    let mut compiler = Compiler {
        code: Vec::new(),
        labels: HashMap::new(),
        loops: Vec::new(),
        fixups: Vec::new(),
        open: Vec::new(),
        mode: labels,
    };
    for statement in statements {
        compiler.statement(statement, tail)?;
    }
    compiler.finish()
}

struct Compiler<'s> {
    code: Vec<Instr<'s>>,
    labels: HashMap<String, usize>,
    loops: Vec<Loop>,
    fixups: Vec<(usize, Fixup)>,
    /// The loops being compiled, innermost last.
    open: Vec<usize>,
    mode: Labels,
}

impl<'s> Compiler<'s> {
    fn statement(&mut self, stmt: &'s PhysicalStatement, tail: bool) -> SqlResult<()> {
        match stmt {
            PhysicalStatement::If {
                condition,
                then_,
                else_,
            } => {
                let test = self.emit(Instr::Test {
                    condition,
                    otherwise: 0,
                    loop_head: false,
                });
                self.statement(then_, tail)?;
                match else_ {
                    None => self.patch_test(test, self.code.len()),
                    Some(else_) => {
                        let skip = self.emit(Instr::Jump { target: 0 });
                        self.patch_test(test, self.code.len());
                        self.statement(else_, tail)?;
                        self.patch_jump(skip, self.code.len());
                    }
                }
            }
            PhysicalStatement::While { condition, body } => {
                let number = self.loops.len();
                let head = self.code.len();
                self.loops.push(Loop { head, exit: 0 });
                self.emit(Instr::Test {
                    condition,
                    otherwise: 0,
                    loop_head: true,
                });
                self.open.push(number);
                self.statement(body, false)?;
                self.open.pop();
                self.emit(Instr::Jump { target: head });
                let exit = self.code.len();
                self.loops[number].exit = exit;
                self.patch_test(head, exit);
            }
            PhysicalStatement::Block(statements) => {
                for statement in statements {
                    self.statement(statement, false)?;
                }
            }
            PhysicalStatement::Label { name } => {
                let position = self.code.len();
                self.labels
                    .entry(name.to_ascii_lowercase())
                    .or_insert(position);
            }
            PhysicalStatement::Goto { label } => {
                let at = self.emit(Instr::Jump { target: 0 });
                self.fixups.push((at, Fixup::Label(label.clone())));
            }
            PhysicalStatement::Break => match self.open.last() {
                Some(&number) => {
                    let at = self.emit(Instr::Jump { target: 0 });
                    self.fixups.push((at, Fixup::Exit(number)));
                }
                None => self.unresolved(Escape::Break)?,
            },
            PhysicalStatement::Continue => match self.open.last() {
                Some(&number) => {
                    let head = self.loops[number].head;
                    self.emit(Instr::Jump { target: head });
                }
                None => self.unresolved(Escape::Continue)?,
            },
            PhysicalStatement::Return(value) => {
                self.emit(Instr::Return(value.as_ref()));
            }
            PhysicalStatement::Query(_)
            | PhysicalStatement::Ddl(_)
            | PhysicalStatement::Use { .. }
            | PhysicalStatement::Insert(_)
            | PhysicalStatement::SelectInto(_)
            | PhysicalStatement::Update(_)
            | PhysicalStatement::Delete(_)
            | PhysicalStatement::SetVariable { .. }
            | PhysicalStatement::SelectAssign { .. }
            | PhysicalStatement::Declare(_)
            | PhysicalStatement::Print(_)
            | PhysicalStatement::Transaction(_)
            | PhysicalStatement::Execute(_)
            | PhysicalStatement::Deferred(_) => {
                let enclosing = self.open.last().copied();
                self.emit(Instr::Run {
                    stmt,
                    tail,
                    enclosing,
                });
            }
        }
        Ok(())
    }

    fn emit(&mut self, instr: Instr<'s>) -> usize {
        self.code.push(instr);
        self.code.len() - 1
    }

    fn patch_test(&mut self, at: usize, position: usize) {
        if let Some(Instr::Test { otherwise, .. }) = self.code.get_mut(at) {
            *otherwise = position;
        }
    }

    fn patch_jump(&mut self, at: usize, position: usize) {
        if let Some(Instr::Jump { target }) = self.code.get_mut(at) {
            *target = position;
        }
    }

    /// A `BREAK` or a `CONTINUE` outside any loop of the program.
    fn unresolved(&mut self, escape: Escape) -> SqlResult<()> {
        match (self.mode, &escape) {
            (Labels::Open, _) => {
                self.emit(Instr::Escape(escape));
                Ok(())
            }
            (Labels::Closed, Escape::Continue) => Err(SqlError::continue_without_while()),
            (Labels::Closed, _) => Err(SqlError::break_without_while()),
        }
    }

    fn finish(mut self) -> SqlResult<Program<'s>> {
        for (at, fixup) in std::mem::take(&mut self.fixups) {
            match fixup {
                Fixup::Exit(number) => {
                    let exit = self.loops[number].exit;
                    self.patch_jump(at, exit);
                }
                Fixup::Label(name) => match self.labels.get(&name.to_ascii_lowercase()) {
                    Some(&position) => self.patch_jump(at, position),
                    None if self.mode == Labels::Open => {
                        self.code[at] = Instr::Escape(Escape::Goto(name));
                    }
                    None => return Err(SqlError::goto_label_not_declared(&name)),
                },
            }
        }
        Ok(Program {
            code: self.code,
            labels: self.labels,
            loops: self.loops,
        })
    }
}

#[cfg(test)]
mod tests;
