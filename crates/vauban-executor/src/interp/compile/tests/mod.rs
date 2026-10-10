//! The instructions a body compiles into, and the jumps the end of the pass writes.

use vauban_planner::PhysicalStatement;

use super::*;
use crate::interp::tests::planned;

/// The instructions of `program`, one word each, with their targets.
fn shape(program: &Program<'_>) -> Vec<String> {
    program
        .code
        .iter()
        .map(|instr| match instr {
            Instr::Run { .. } => "run".to_owned(),
            Instr::Test {
                otherwise,
                loop_head: false,
                ..
            } => format!("test ->{otherwise}"),
            Instr::Test {
                otherwise,
                loop_head: true,
                ..
            } => format!("loop ->{otherwise}"),
            Instr::Jump { target } => format!("jump ->{target}"),
            Instr::Return(_) => "return".to_owned(),
            Instr::Escape(escape) => format!("escape {escape:?}"),
        })
        .collect()
}

fn compiled(text: &str) -> Vec<String> {
    let statements = planned(text, None, None);
    let program = compile(&statements, Labels::Closed, false).expect("the body compiles");
    shape(&program)
}

/// `IF … ELSE` tests, runs the branch, then jumps over the other one.
#[test]
fn an_if_with_an_else_jumps_over_the_else() {
    assert_eq!(
        compiled("IF 1 = 1 SELECT 1; ELSE SELECT 2; SELECT 3;"),
        ["test ->3", "run", "jump ->4", "run", "run"]
    );
    assert_eq!(
        compiled("IF 1 = 1 SELECT 1; SELECT 3;"),
        ["test ->2", "run", "run"]
    );
}

/// `WHILE` tests at its head; `BREAK` jumps past the jump back, `CONTINUE` to the head.
#[test]
fn a_loop_jumps_back_to_its_head_and_break_jumps_past_it() {
    assert_eq!(
        compiled("WHILE 1 = 1 BEGIN SELECT 1; BREAK; CONTINUE; END; SELECT 2;"),
        ["loop ->5", "run", "jump ->5", "jump ->0", "jump ->0", "run"]
    );
}

/// A `BREAK` in a loop nested in another leaves the inner loop and no further.
#[test]
fn break_leaves_the_innermost_loop() {
    assert_eq!(
        compiled("WHILE 1 = 1 BEGIN WHILE 2 = 2 BREAK; SELECT 1; END;"),
        [
            "loop ->6", "loop ->4", "jump ->4", "jump ->1", "run", "jump ->0"
        ]
    );
}

/// A label is the position of the instruction after it, inside a block or not, and a
/// `GOTO` written before it jumps forward to it.
#[test]
fn a_goto_jumps_to_a_label_inside_a_block() {
    assert_eq!(
        compiled("GOTO here; BEGIN SELECT 1; here: SELECT 2; END; SELECT 3;"),
        ["jump ->2", "run", "run", "run"]
    );
}

/// A name declared twice keeps its first position, and the case of the name is not read.
#[test]
fn a_label_declared_twice_keeps_its_first_position() {
    assert_eq!(
        compiled("a: SELECT 1; a: SELECT 2; GOTO A;"),
        ["run", "run", "jump ->0"]
    );
}

/// A closed program refuses what it cannot resolve: 133, 135, 136.
#[test]
fn a_closed_program_refuses_a_jump_it_cannot_resolve() {
    let bodies = [
        vec![PhysicalStatement::Goto {
            label: "away".to_owned(),
        }],
        vec![PhysicalStatement::Break],
        vec![PhysicalStatement::Continue],
    ];
    let numbers: Vec<u32> = bodies
        .iter()
        .map(|body| {
            compile(body, Labels::Closed, false)
                .expect_err("nothing resolves the jump")
                .number
        })
        .collect();
    assert_eq!(numbers, [133, 135, 136]);
}

/// An open program lets the same jumps leave it.
#[test]
fn an_open_program_lets_a_jump_it_cannot_resolve_leave() {
    let body = [PhysicalStatement::Block(vec![
        PhysicalStatement::Goto {
            label: "away".to_owned(),
        },
        PhysicalStatement::Break,
        PhysicalStatement::Continue,
    ])];
    let program = compile(&body, Labels::Open, false).expect("an open body compiles");
    assert_eq!(
        shape(&program),
        ["escape Goto(\"away\")", "escape Break", "escape Continue"]
    );
}

/// The statements reached from the root through the branches of `IF`s answer for the
/// statement tree, and a statement of a block does not.
#[test]
fn the_tail_is_what_the_branches_of_an_if_hold() {
    let statements = planned("IF 1 = 1 SELECT 1; ELSE BEGIN SELECT 2; END;", None, None);
    let program = compile(&statements, Labels::Open, true).expect("the tree compiles");
    let tails: Vec<bool> = program
        .code
        .iter()
        .filter_map(|instr| match instr {
            Instr::Run { tail, .. } => Some(*tail),
            _ => None,
        })
        .collect();
    assert_eq!(tails, [true, false]);
}
