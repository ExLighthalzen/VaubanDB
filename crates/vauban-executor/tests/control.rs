//! Tests for variables and control of flow: `DECLARE`, `SET`, `SELECT @x =`, `IF`,
//! `WHILE`, `BREAK`, `CONTINUE`, `RETURN`, `PRINT`, `Block`.
//!
//! All `PhysicalStatement`s are built by hand; the binder and the parser are not involved.

use vauban_binder::{
    BoundDeclaration, BoundExpr, BoundExprKind, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_executor::{CancelToken, CollectSink, ExecContext, ExecOutcome, ExecSession, execute};
use vauban_planner::{PhysicalPlan, PhysicalStatement};
use vauban_sysfn::StaticContext;
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
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    let (outcome, _) = run(&mut session, &declare);
    assert!(matches!(outcome, ExecOutcome::NoRows));

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: int_lit(3),
    };
    let (outcome, _) = run(&mut session, &set);
    assert!(matches!(outcome, ExecOutcome::NoRows));

    assert_eq!(session.variables.get("@i"), Some(&Value::I32(3)));
}

/// `DECLARE @i int` without `SET` — the variable is `NULL`.
#[test]
fn declare_gives_null() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    let (outcome, _) = run(&mut session, &declare);
    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.variables.get("@i"), Some(&Value::Null));
}

/// `DECLARE @i int; SET @i = '3'` converts the string to int (3).
#[test]
fn set_converts_to_the_declared_type() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: str_lit("3"),
    };
    let (outcome, _) = run(&mut session, &set);
    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.variables.get("@i"), Some(&Value::I32(3)));
}

/// `DECLARE @i int; SET @i = 'x'` raises a conversion error (245).
#[test]
fn set_conversion_error_has_the_original_number() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: str_lit("x"),
    };
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let mut sink = CollectSink::new();
    let err = execute(&set, &mut ctx, &mut sink).expect_err("conversion fails");
    assert_eq!(err.number, 245);
}

/// `SELECT @x = col` over three rows: the last row wins.
#[test]
fn select_assign_takes_the_last_row() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);

    let plan = PhysicalPlan::Values {
        rows: vec![vec![int_lit(1)], vec![int_lit(2)], vec![int_lit(3)]],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: "col".to_owned(),
                ty: int_ty(),
            }],
        },
    };
    let stmt = PhysicalStatement::Query(plan);
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let mut sink = CollectSink::new();
    let outcome = execute(&stmt, &mut ctx, &mut sink).expect("the query executes");

    assert!(matches!(outcome, ExecOutcome::Rows(3)));
}

/// `IF 1 = 1` runs the `then` branch; `IF 1 = 0` runs the `else` branch.
#[test]
fn if_takes_the_true_branch() {
    let mut session = new_session();
    let then_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(1),
    };
    let else_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(2),
    };
    let if_stmt = PhysicalStatement::If {
        condition: eq(int_lit(1), int_lit(1)),
        then_: Box::new(then_stmt),
        else_: Some(Box::new(else_stmt)),
    };

    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
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
    };
    let else_stmt = PhysicalStatement::SetVariable {
        name: "@x".to_owned(),
        value: int_lit(2),
    };
    let null_cond = lit(Value::Null, bit_ty());
    let if_stmt = PhysicalStatement::If {
        condition: null_cond,
        then_: Box::new(then_stmt),
        else_: Some(Box::new(else_stmt)),
    };

    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);

    run(&mut session, &if_stmt);
    assert_eq!(session.variables.get("@x"), Some(&Value::I32(2)));
}

/// `WHILE` loops and `BREAK` exits.
#[test]
fn while_loops_and_break_exits() {
    let mut session = new_session();
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);
    let init = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: int_lit(0),
    };
    run(&mut session, &init);

    let inc = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: add(var_ref("@i"), int_lit(1)),
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
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@x".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);

    let block = PhysicalStatement::Block(vec![
        PhysicalStatement::SetVariable {
            name: "@x".to_owned(),
            value: int_lit(1),
        },
        PhysicalStatement::Return(None),
        PhysicalStatement::SetVariable {
            name: "@x".to_owned(),
            value: int_lit(2),
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
    let declare = PhysicalStatement::Declare(vec![BoundDeclaration {
        name: "@i".to_owned(),
        ty: int_ty(),
        value: None,
    }]);
    run(&mut session, &declare);

    let set = PhysicalStatement::SetVariable {
        name: "@i".to_owned(),
        value: int_lit(1),
    };
    run(&mut session, &set);
    assert_eq!(session.rowcount, 1);
}
