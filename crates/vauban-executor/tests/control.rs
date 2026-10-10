//! Tests for variables and control of flow: `DECLARE`, `SET`, `SELECT @x =`, `IF`,
//! `WHILE`, `BREAK`, `CONTINUE`, `RETURN`, `PRINT`, `Block`.
//!
//! All `PhysicalStatement`s are built by hand; the binder and the parser are not involved.

use std::sync::Arc;

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundProjection, BoundTop, ColumnBinding,
    DeferredStatement, OutputColumn, OutputSchema, SessionOptions, SortKey, bind,
};
use vauban_catalog::{Catalog, ColumnId};
use vauban_errors::SqlResult;
use vauban_executor::{
    CancelToken, CollectSink, ExecContext, ExecOutcome, ExecSession, execute, execute_collect,
};
use vauban_parser::{ParseOptions, parse_batch};
use vauban_planner::{
    NoIndexes, PhysicalDeclaration, PhysicalPlan, PhysicalStatement, PlanContext, plan,
};
use vauban_storage::{MemoryStorage, Storage};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

/// An `int`, nullable.
fn int_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

/// A `varchar(n)`, nullable.
fn varchar_ty(n: u16) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(n)), true)
}

/// A `bit`, nullable.
fn bit_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, true)
}

/// A bound literal of the given value and type.
fn lit(value: Value, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 1,
    }
}

/// A bound integer literal.
fn int_lit(n: i32) -> BoundExpr {
    lit(Value::I32(n), int_ty())
}

/// A bound string literal.
fn str_lit(s: &str) -> BoundExpr {
    lit(
        Value::String(SqlString { text: s.to_owned() }),
        varchar_ty(s.len() as u16 + 1),
    )
}

/// A bound variable reference.
fn var_ref(name: &str) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Variable {
            name: name.to_owned(),
        },
        ty: int_ty(),
        line: 1,
    }
}

/// A binder predicate comparison, `left = right`.
fn eq(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: vauban_binder::CompareOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: bit_ty(),
        line: 1,
    }
}

/// A `<` comparison.
fn lt(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: vauban_binder::CompareOp::Lt,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: bit_ty(),
        line: 1,
    }
}

/// A `+` arithmetic expression.
fn add(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Arith {
            op: vauban_types::BinaryOp::Add,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: int_ty(),
        line: 1,
    }
}

/// Runs `stmt` with a fresh session and a scalar context, answers the outcome and the sink.
fn run(session: &mut ExecSession, stmt: &PhysicalStatement) -> (ExecOutcome, CollectSink) {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(session);
    let mut sink = CollectSink::new();
    let outcome = execute(stmt, &mut ctx, &mut sink).expect("the statement executes");
    (outcome, sink)
}

/// A fresh session with no variables.
fn new_session() -> ExecSession {
    ExecSession::default()
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// `DECLARE @i int; SET @i = 3;` then read `@i` — it must be 3.
#[test]
fn declare_then_set_then_read() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    let (outcome, _) = run(&mut session, &declare);
    assert!(matches!(outcome, ExecOutcome::NoRows));

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: int_lit(3),
        subplans: Vec::new(),
    };
    let (outcome, _) = run(&mut session, &set);
    assert!(matches!(outcome, ExecOutcome::NoRows));

    assert_eq!(session.variables.get("@i"), Some(&Value::I32(3)));
}

/// `DECLARE @i int` without `SET` — the variable is `NULL`.
#[test]
fn declare_gives_null() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    let (outcome, _) = run(&mut session, &declare);
    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.variables.get("@i"), Some(&Value::Null));
}

/// `DECLARE @i int; SET @i = '3'` converts the string to int (3).
#[test]
fn set_converts_to_the_declared_type() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: str_lit("3"),
        subplans: Vec::new(),
    };
    let (outcome, _) = run(&mut session, &set);
    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.variables.get("@i"), Some(&Value::I32(3)));
}

/// `DECLARE @i int; SET @i = 'x'` raises a conversion error (245).
#[test]
fn set_conversion_error_has_the_original_number() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: str_lit("x"),
        subplans: Vec::new(),
    };
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let mut sink = CollectSink::new();
    let err = execute(&set, &mut ctx, &mut sink).expect_err("conversion fails");
    assert_eq!(err.number, 245);
}

/// The three rows `1`, `2`, `3` read as one `int` column named `col`.
fn three_rows() -> PhysicalPlan {
    PhysicalPlan::Values {
        rows: vec![vec![int_lit(1)], vec![int_lit(2)], vec![int_lit(3)]],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: "col".to_owned(),
                ty: int_ty(),
            }],
        },
    }
}

/// A reference to the column of [`three_rows`], or to the `n`-th column of a wider plan.
fn col_ref(index: usize, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(ColumnBinding {
            column: ColumnId(i32::try_from(index).expect("a small index") + 1),
            index,
            name: format!("c{index}"),
            ty: ty.clone(),
            depth: 0,
        }),
        ty,
        line: 1,
    }
}

/// `DECLARE @x int` (or another type), run against `session`.
fn declare(session: &mut ExecSession, name: &str, ty: TypeInfo, value: Option<BoundExpr>) {
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: name.to_owned(),
        ty,
        value,
        subplans: Vec::new(),
    }]);
    run(session, &declare);
}

/// `SELECT @x = col FROM (three rows)`: the row walked last leaves its value behind, and
/// the statement answers no result set.
#[test]
fn select_assign_keeps_the_last_row() {
    let mut session = new_session();
    declare(&mut session, "@x", int_ty(), None);

    let stmt = PhysicalStatement::SelectAssign {
        input: three_rows(),
        assignments: vec![("@x".to_owned(), col_ref(0, int_ty()))],
    };
    let (outcome, sink) = run(&mut session, &stmt);

    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert!(sink.columns.is_none(), "the assignment published no schema");
    assert!(sink.rows.is_empty());
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(3)));
}

/// The same three rows under a `Sort` on `col DESC`: the winner follows the order the
/// plan delivers, so `1` is the row walked last.
#[test]
fn select_assign_follows_the_order_by() {
    let mut session = new_session();
    declare(&mut session, "@x", int_ty(), None);

    let sorted = PhysicalPlan::Sort {
        input: Box::new(three_rows()),
        keys: vec![SortKey {
            expr: col_ref(0, int_ty()),
            desc: true,
            collation: None,
        }],
    };
    let stmt = PhysicalStatement::SelectAssign {
        input: sorted,
        assignments: vec![("@x".to_owned(), col_ref(0, int_ty()))],
    };
    run(&mut session, &stmt);

    assert_eq!(session.variables.get("@x"), Some(&Value::I32(1)));
}

/// `SELECT @s = @s + col FROM …`: the value read on each row is the one the row before
/// it stored, so the three rows accumulate into `123`.
#[test]
fn select_assign_accumulates_over_the_rows() {
    let mut session = new_session();
    declare(&mut session, "@s", varchar_ty(20), Some(str_lit("")));

    let rows = PhysicalPlan::Values {
        rows: vec![vec![str_lit("1")], vec![str_lit("2")], vec![str_lit("3")]],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: "col".to_owned(),
                ty: varchar_ty(2),
            }],
        },
    };
    let accumulate = BoundExpr {
        kind: BoundExprKind::Arith {
            op: vauban_types::BinaryOp::Add,
            left: Box::new(BoundExpr {
                kind: BoundExprKind::Variable {
                    name: "@s".to_owned(),
                },
                ty: varchar_ty(20),
                line: 1,
            }),
            right: Box::new(col_ref(0, varchar_ty(2))),
        },
        ty: varchar_ty(20),
        line: 1,
    };
    let stmt = PhysicalStatement::SelectAssign {
        input: rows,
        assignments: vec![("@s".to_owned(), accumulate)],
    };
    run(&mut session, &stmt);

    assert_eq!(
        session.variables.get("@s"),
        Some(&Value::String(SqlString {
            text: "123".to_owned()
        }))
    );
}

/// A source that produces nothing leaves the variable at the value it held, where the
/// subquery spelling `SET @x = (SELECT …)` over the same source stores `NULL`.
#[test]
fn select_assign_over_no_row_keeps_the_value() {
    let mut session = new_session();
    declare(&mut session, "@x", int_ty(), Some(int_lit(7)));

    let empty = PhysicalPlan::Filter {
        input: Box::new(three_rows()),
        predicate: eq(int_lit(1), int_lit(0)),
    };
    let stmt = PhysicalStatement::SelectAssign {
        input: empty,
        assignments: vec![("@x".to_owned(), col_ref(0, int_ty()))],
    };
    let (outcome, _) = run(&mut session, &stmt);

    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(7)));
}

/// Within one row the targets are stored left to right, so the second value reads the
/// first target at the value of the row being walked, not of the row before it.
#[test]
fn select_assign_reads_a_target_assigned_before_it() {
    let mut session = new_session();
    declare(&mut session, "@a", int_ty(), None);
    declare(&mut session, "@b", int_ty(), None);

    let stmt = PhysicalStatement::SelectAssign {
        input: three_rows(),
        assignments: vec![
            ("@a".to_owned(), col_ref(0, int_ty())),
            ("@b".to_owned(), var_ref("@a")),
        ],
    };
    run(&mut session, &stmt);

    assert_eq!(session.variables.get("@a"), Some(&Value::I32(3)));
    assert_eq!(session.variables.get("@b"), Some(&Value::I32(3)));
}

/// `@@ROWCOUNT` after the statement is the number of rows walked.
#[test]
fn select_assign_sets_rowcount_to_the_rows_walked() {
    let mut session = new_session();
    declare(&mut session, "@x", int_ty(), None);
    session.rowcount = 99;

    let stmt = PhysicalStatement::SelectAssign {
        input: three_rows(),
        assignments: vec![("@x".to_owned(), col_ref(0, int_ty()))],
    };
    run(&mut session, &stmt);

    assert_eq!(session.rowcount, 3);
}

/// A source that produces nothing puts `@@ROWCOUNT` at `0`, over the count the previous
/// statement left: the count is written whether or not a variable was assigned.
#[test]
fn select_assign_over_no_row_sets_rowcount_to_zero() {
    let mut session = new_session();
    declare(&mut session, "@x", int_ty(), Some(int_lit(7)));
    session.rowcount = 99;

    let empty = PhysicalPlan::Filter {
        input: Box::new(three_rows()),
        predicate: eq(int_lit(1), int_lit(0)),
    };
    let stmt = PhysicalStatement::SelectAssign {
        input: empty,
        assignments: vec![("@x".to_owned(), col_ref(0, int_ty()))],
    };
    run(&mut session, &stmt);

    assert_eq!(session.rowcount, 0);
}

/// `IF 1 = 1` runs the `then` branch; `IF 1 = 0` runs the `else` branch.
#[test]
fn if_takes_the_true_branch() {
    let mut session = new_session();
    let then_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(1),
        subplans: Vec::new(),
    };
    let else_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(2),
        subplans: Vec::new(),
    };
    let if_stmt = PhysicalStatement::If {
        condition: eq(int_lit(1), int_lit(1)),
        then_: Box::new(then_stmt),
        else_: Some(Box::new(else_stmt)),
    };

    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);

    run(&mut session, &if_stmt);
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(1)));
}

/// `IF NULL` takes the `else` branch (or falls through).
#[test]
fn if_takes_else_on_unknown() {
    let mut session = new_session();
    let then_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(1),
        subplans: Vec::new(),
    };
    let else_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(2),
        subplans: Vec::new(),
    };
    let null_cond = lit(Value::Null, bit_ty());
    let if_stmt = PhysicalStatement::If {
        condition: null_cond,
        then_: Box::new(then_stmt),
        else_: Some(Box::new(else_stmt)),
    };

    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);

    run(&mut session, &if_stmt);
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(2)));
}

/// `WHILE` loops and `BREAK` exits.
#[test]
fn while_loops_and_break_exits() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);
    let init = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: int_lit(0),
        subplans: Vec::new(),
    };
    run(&mut session, &init);

    let inc = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: add(var_ref("@i"), int_lit(1)),
        subplans: Vec::new(),
    };
    let break_if_three = PhysicalStatement::If {
        condition: eq(var_ref("@i"), int_lit(3)),
        then_: Box::new(PhysicalStatement::Break),
        else_: None,
    };
    let body = PhysicalStatement::Block(vec![inc, break_if_three]);
    let while_stmt = PhysicalStatement::While {
        condition: lt(var_ref("@i"), int_lit(5)),
        body: Box::new(body),
    };

    run(&mut session, &while_stmt);
    assert_eq!(session.variables.get("@i"), Some(&Value::I32(3)));
}

/// An infinite loop is cancellable: the token raised before execution stops it.
#[test]
fn infinite_loop_is_cancellable() {
    let mut session = new_session();
    let while_stmt = PhysicalStatement::While {
        condition: eq(int_lit(1), int_lit(1)),
        body: Box::new(PhysicalStatement::Block(vec![])),
    };

    let eval = StaticContext::default();
    let cancel = CancelToken::new();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_session(&mut session)
        .with_cancel(&cancel);
    let mut sink = CollectSink::new();

    // Raise the token in another thread after a short delay.
    let cancel_clone = cancel.clone();
    let guard = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        cancel_clone.cancel();
    });

    // The loop calls `ctx.cancelled()` at each iteration, which reads `cancel`.
    let outcome = execute(&while_stmt, &mut ctx, &mut sink);
    guard.join().expect("the guard thread joined");
    assert!(matches!(outcome, Ok(ExecOutcome::Cancelled)));
}

/// `BEGIN ... END` stops at the first `RETURN`.
#[test]
fn block_stops_at_the_first_return() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);

    let block = PhysicalStatement::Block(vec![
        PhysicalStatement::SetVariable {
            name: "@x".to_owned(),
            value: int_lit(1),
            subplans: Vec::new(),
        },
        PhysicalStatement::Return(None),
        PhysicalStatement::SetVariable {
            name: "@x".to_owned(),
            value: int_lit(2),
            subplans: Vec::new(),
        },
    ]);

    let outcome = run(&mut session, &block).0;
    assert!(matches!(outcome, ExecOutcome::Return(0)));
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(1)));
}

/// `PRINT 'hello'` emits an `InfoMessage` with number 0.
#[test]
fn print_emits_info_number_zero() {
    let mut session = new_session();
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let mut sink = CollectSink::new();
    let print = PhysicalStatement::Print(str_lit("hello"));
    let outcome = execute(&print, &mut ctx, &mut sink).expect("PRINT runs");
    assert!(matches!(outcome, ExecOutcome::NoRows));

    assert_eq!(sink.infos.len(), 1);
    assert_eq!(sink.infos[0].number, 0);
    assert_eq!(sink.infos[0].message, "hello");
}

/// `@@ROWCOUNT` after a successful `SET` is 1.
#[test]
fn rowcount_after_set() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![PhysicalDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
        subplans: Vec::new(),
    }]);
    run(&mut session, &declare);

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: int_lit(1),
        subplans: Vec::new(),
    };
    run(&mut session, &set);
    assert_eq!(session.rowcount, 1);
}

/// A call of a registered built-in, typed by `sysfn::check_call` as the binder types it.
fn call(name: &str, args: Vec<BoundExpr>) -> BoundExpr {
    vauban_sysfn::register_builtins();
    let def = vauban_sysfn::lookup(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    let types: Vec<TypeInfo> = args.iter().map(|arg| arg.ty.clone()).collect();
    let ty = vauban_sysfn::check_call(def, &types).expect("the call is well formed");
    BoundExpr {
        kind: BoundExprKind::Function { def, args },
        ty,
        line: 1,
    }
}

/// `TOP (n)` over `input`, without `PERCENT` and without `WITH TIES`.
fn top(input: PhysicalPlan, n: i64) -> PhysicalPlan {
    PhysicalPlan::Top {
        input: Box::new(input),
        top: BoundTop {
            expr: BoundExpr {
                kind: BoundExprKind::Literal(Value::I64(n)),
                ty: TypeInfo::new(SqlType::BigInt, false),
                line: 1,
            },
            percent: false,
            with_ties: false,
        },
    }
}

/// Runs `stmt` and answers the error it raised.
fn run_err(session: &mut ExecSession, stmt: &PhysicalStatement) -> vauban_errors::SqlError {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(session);
    let mut sink = CollectSink::new();
    execute(stmt, &mut ctx, &mut sink).expect_err("the statement fails")
}

/// `SELECT TOP (-1) @s = SUBSTRING('abc', 1, -1) FROM t` faults in two places at once: the
/// row count is negative (127) and the length argument is negative (536). It answers
/// **536**, the number the same source read as a query answers
/// (`select_of_the_same_two_faults_answers_the_length_error`, just below), because the
/// select list the binder detached is checked before the `TOP` that sits above the source.
#[test]
fn select_assign_answers_the_length_error_before_the_top_error() {
    let mut session = new_session();
    declare(&mut session, "@s", varchar_ty(10), None);

    let stmt = PhysicalStatement::SelectAssign {
        input: top(three_rows(), -1),
        assignments: vec![(
            "@s".to_owned(),
            call("SUBSTRING", vec![str_lit("abc"), int_lit(1), int_lit(-1)]),
        )],
    };
    assert_eq!(run_err(&mut session, &stmt).number, 536);
}

/// The query spelling of the same two faults, which is the shape the assignment above is
/// aligned on: `SELECT TOP (-1) SUBSTRING('abc', 1, -1) FROM t` answers 536 as well.
#[test]
fn select_of_the_same_two_faults_answers_the_length_error() {
    let mut session = new_session();
    let projected = PhysicalPlan::Project {
        input: Box::new(three_rows()),
        exprs: vec![BoundProjection {
            expr: call("SUBSTRING", vec![str_lit("abc"), int_lit(1), int_lit(-1)]),
            name: String::new(),
        }],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: String::new(),
                ty: varchar_ty(3),
            }],
        },
    };
    let stmt = PhysicalStatement::Query(top(projected, -1));
    assert_eq!(run_err(&mut session, &stmt).number, 536);
}

/// The counter-proof of the pair: with the length argument put right, the same assignment
/// still answers the 127 of its `TOP`, so the 536 above is the length check running first
/// rather than the row-count check being skipped.
#[test]
fn select_assign_over_a_negative_top_answers_127() {
    let mut session = new_session();
    declare(&mut session, "@s", varchar_ty(10), None);

    let stmt = PhysicalStatement::SelectAssign {
        input: top(three_rows(), -1),
        assignments: vec![(
            "@s".to_owned(),
            call("SUBSTRING", vec![str_lit("abc"), int_lit(1), int_lit(1)]),
        )],
    };
    assert_eq!(run_err(&mut session, &stmt).number, 127);
}

// ---------------------------------------------------------------------------------------
// A statement the binder left unbound
// ---------------------------------------------------------------------------------------

/// A statement kept as it was parsed, the shape the binder leaves in a branch or in a loop
/// body for a name the catalogue did not hold.
fn deferred(text: &str) -> PhysicalStatement {
    vauban_sysfn::register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    PhysicalStatement::Deferred(Box::new(DeferredStatement {
        text: Arc::from(text),
        statement: Box::new(batch.statements[0].clone()),
        database: "master".to_owned(),
        default_schema: "dbo".to_owned(),
        line: 1,
    }))
}

/// An `IF` over `taken` whose branch holds `stmt`.
fn if_over(taken: bool, stmt: PhysicalStatement) -> PhysicalStatement {
    PhysicalStatement::If {
        condition: eq(int_lit(1), int_lit(if taken { 1 } else { 0 })),
        then_: Box::new(stmt),
        else_: None,
    }
}

/// A block reaching a statement left unbound binds it, plans it and runs it: the rows
/// arrive as they would have from a statement bound with the block.
#[test]
fn a_deferred_statement_binds_when_the_block_reaches_it() {
    let mut session = new_session();
    let stmt = PhysicalStatement::Block(vec![deferred("SELECT 1;")]);
    let (_, sink) = run(&mut session, &stmt);
    assert_eq!(sink.rows, vec![vec![Value::I32(1)]], "{sink:?}");
}

/// A branch that is not taken never binds the statement it holds, and the same statement
/// in the branch that **is** taken answers the error its binding raises.
///
/// The pair is what separates "not bound" from "bound and silent": `SELECT @nope` reads a
/// variable no `DECLARE` entered, which is error 137 at binding time.
#[test]
fn a_branch_that_is_not_taken_never_binds_its_statement() {
    let mut session = new_session();
    let (outcome, sink) = run(&mut session, &if_over(false, deferred("SELECT @nope;")));
    assert!(matches!(outcome, ExecOutcome::NoRows), "{outcome:?}");
    assert!(sink.rows.is_empty(), "{sink:?}");

    let mut session = new_session();
    let error = run_err(&mut session, &if_over(true, deferred("SELECT @nope;")));
    assert_eq!(error.number, 137, "{error:?}");
}

/// A statement left unbound in the body of a loop runs on every turn.
#[test]
fn a_deferred_statement_in_a_loop_runs_on_every_turn() {
    let mut session = new_session();
    declare(&mut session, "@i", int_ty(), Some(int_lit(0)));
    let stmt = PhysicalStatement::While {
        condition: lt(var_ref("@i"), int_lit(3)),
        body: Box::new(PhysicalStatement::Block(vec![
            PhysicalStatement::SetVariable {
                name: "@i".to_owned(),
                value: add(var_ref("@i"), int_lit(1)),
                subplans: Vec::new(),
            },
            deferred("SELECT 1;"),
        ])),
    };
    let (_, sink) = run(&mut session, &stmt);
    assert_eq!(sink.rows.len(), 3, "{sink:?}");
}

/// A statement bound this late reads the variables the session holds when it is reached,
/// so a `DECLARE` written before the block is in scope.
#[test]
fn a_deferred_statement_reads_a_variable_declared_before_it() {
    let mut session = new_session();
    declare(&mut session, "@v", int_ty(), Some(int_lit(7)));
    let stmt = PhysicalStatement::Block(vec![deferred("SELECT @v;")]);
    let (_, sink) = run(&mut session, &stmt);
    assert_eq!(sink.rows, vec![vec![Value::I32(7)]], "{sink:?}");
}

/// The compile-time checks run on what the binding produced, for the shapes that reach no
/// other caller: `PRINT` is run by this file and by nothing else, and its folded negative
/// length answers **536**, the compile-time number, where the same value computed at run
/// time answers 537 (`compile.rs`, module header).
///
/// 536 against 537 is what separates "checked before it runs" from "checked while it
/// runs" here; the query below is the neighbouring shape, whose 127 the caller of a query
/// would have raised anyway.
#[test]
fn a_deferred_statement_gets_the_compile_time_checks() {
    let mut session = new_session();
    let error = run_err(
        &mut session,
        &if_over(true, deferred("PRINT SUBSTRING('abc', 1, 0 - 1);")),
    );
    assert_eq!(error.number, 536, "{error:?}");

    let mut session = new_session();
    let error = run_err(
        &mut session,
        &if_over(true, deferred("SELECT TOP (0 - 1) 1;")),
    );
    assert_eq!(error.number, 127, "{error:?}");
}

// ---------------------------------------------------------------------------------------
// A statement left unbound over a name the catalogue holds
// ---------------------------------------------------------------------------------------

/// A bootstrapped catalogue over an in-memory storage: what a statement bound this late is
/// bound against, and what a `CREATE TABLE` writes into.
struct Fixture {
    storage: Arc<dyn Storage>,
    txn: Arc<TransactionManager>,
    catalog: Catalog,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
            .expect("the catalogue boots on a fresh storage");
        Self {
            storage,
            txn,
            catalog,
        }
    }
}

/// Parses, binds against the catalogue of `fixture`, plans and runs `text` as one
/// statement, in a transaction of its own.
fn run_over_catalog(fixture: &Fixture, text: &str) -> SqlResult<ExecOutcome> {
    vauban_sysfn::register_builtins();
    let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = fixture.catalog.snapshot(&handle);
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
    bind_ctx.catalog = Some(&snapshot);
    let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
    let planned = plan(
        bound,
        &PlanContext {
            catalog: &NoIndexes,
        },
    )
    .expect("the statement plans");
    let snap = fixture.txn.statement_snapshot(&handle);
    let eval = StaticContext::default();
    let mut session = ExecSession::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(fixture.storage.as_ref(), &fixture.txn, &snap)
        .with_catalog(&fixture.catalog)
        .with_handle(&handle)
        .with_session(&mut session);
    let outcome = execute_collect(&planned, &mut ctx).map(|(outcome, _)| outcome);
    match &outcome {
        Ok(_) => fixture.txn.commit(handle).expect("the transaction commits"),
        Err(_) => fixture.txn.rollback(handle).expect("it rolls back"),
    }
    outcome
}

/// The `CREATE TABLE` of a name the catalogue already holds, written in a branch, answers
/// 2714 when the branch is taken and nothing when it is not: the number falls while the
/// statement runs, which is what the pair separates from a binding error of the batch.
#[test]
fn a_branch_that_creates_a_table_the_catalogue_holds_answers_2714_when_it_is_taken() {
    let fixture = Fixture::new();
    run_over_catalog(&fixture, "CREATE TABLE dbo.t (a int);").expect("the table is created");

    run_over_catalog(&fixture, "IF 1 = 0 CREATE TABLE dbo.t (a int);")
        .expect("the branch is not taken, so nothing looks the name up");

    let error = run_over_catalog(&fixture, "IF 1 = 1 CREATE TABLE dbo.t (a int);")
        .expect_err("the branch is taken and the name is there");
    assert_eq!((error.number, error.state), (2714, 6), "{error:?}");
}

/// The loop body carries it too, and the `ALTER TABLE … ADD` of a column the table has
/// answers 2705 state 4 the same way.
#[test]
fn a_loop_body_and_an_alter_table_answer_their_number_when_they_are_reached() {
    let fixture = Fixture::new();
    run_over_catalog(&fixture, "CREATE TABLE dbo.t (a int);").expect("the table is created");

    let error = run_over_catalog(&fixture, "WHILE 1 = 1 CREATE TABLE dbo.t (a int);")
        .expect_err("the first turn reaches the statement");
    assert_eq!((error.number, error.state), (2714, 6), "{error:?}");

    let error = run_over_catalog(&fixture, "IF 1 = 1 ALTER TABLE dbo.t ADD a int;")
        .expect_err("the branch is taken and the column is there");
    assert_eq!((error.number, error.state), (2705, 4), "{error:?}");

    run_over_catalog(&fixture, "IF 1 = 0 ALTER TABLE dbo.t ADD a int;")
        .expect("the branch is not taken, so nothing looks the column up");
}

/// A `GOTO` answers [`ExecOutcome::Goto`] with the name it wrote, and a label answers
/// `NoRows`: neither is acted upon here, since where the jump lands is a position in a
/// list of statements this crate does not hold.
#[test]
fn a_goto_answers_the_label_it_names_and_a_label_runs_nothing() {
    let mut session = new_session();
    let (outcome, sink) = run(
        &mut session,
        &PhysicalStatement::Goto {
            label: "again".to_owned(),
        },
    );
    assert!(
        matches!(&outcome, ExecOutcome::Goto(label) if label == "again"),
        "{outcome:?}"
    );
    assert!(sink.rows.is_empty(), "a GOTO sends no row");

    let (outcome, sink) = run(
        &mut session,
        &PhysicalStatement::Label {
            name: "again".to_owned(),
        },
    );
    assert!(matches!(outcome, ExecOutcome::NoRows), "{outcome:?}");
    assert!(sink.columns.is_none(), "a label announces no column");
}

/// A `BEGIN … END` stops at the `GOTO` it holds and hands its label up: the statement
/// written after the `GOTO` in the same block does not run.
#[test]
fn a_goto_in_a_block_stops_the_block() {
    let mut session = new_session();
    run(
        &mut session,
        &PhysicalStatement::Declare(vec![PhysicalDeclaration {
            name: "@i".to_owned(),
            ty: int_ty(),
            value: Some(int_lit(0)),
            subplans: Vec::new(),
        }]),
    );
    let block = PhysicalStatement::Block(vec![
        PhysicalStatement::SetVariable {
            name: "@i".to_owned(),
            value: int_lit(1),
            subplans: Vec::new(),
        },
        PhysicalStatement::Goto {
            label: "after".to_owned(),
        },
        PhysicalStatement::SetVariable {
            name: "@i".to_owned(),
            value: int_lit(99),
            subplans: Vec::new(),
        },
    ]);
    let (outcome, _) = run(&mut session, &block);
    assert!(
        matches!(&outcome, ExecOutcome::Goto(label) if label == "after"),
        "{outcome:?}"
    );
    assert_eq!(
        session.variables.get("@i"),
        Some(&Value::I32(1)),
        "the assignment written after the GOTO did not run"
    );
}

/// A `WHILE` passes a `Goto` up as it passes a `RETURN` up: the loop ends there, without
/// evaluating its condition again, where a `CONTINUE` would have started another turn.
#[test]
fn a_goto_in_a_while_body_leaves_the_loop() {
    let mut session = new_session();
    run(
        &mut session,
        &PhysicalStatement::Declare(vec![PhysicalDeclaration {
            name: "@i".to_owned(),
            ty: int_ty(),
            value: Some(int_lit(0)),
            subplans: Vec::new(),
        }]),
    );
    let body = PhysicalStatement::Block(vec![
        PhysicalStatement::SetVariable {
            name: "@i".to_owned(),
            value: add(var_ref("@i"), int_lit(1)),
            subplans: Vec::new(),
        },
        PhysicalStatement::Goto {
            label: "out".to_owned(),
        },
    ]);
    let while_stmt = PhysicalStatement::While {
        condition: lt(var_ref("@i"), int_lit(5)),
        body: Box::new(body),
    };
    let (outcome, _) = run(&mut session, &while_stmt);
    assert!(
        matches!(&outcome, ExecOutcome::Goto(label) if label == "out"),
        "{outcome:?}"
    );
    assert_eq!(
        session.variables.get("@i"),
        Some(&Value::I32(1)),
        "the body ran once and the loop stopped at the GOTO"
    );
}
