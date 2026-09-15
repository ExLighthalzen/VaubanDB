//! Control of flow: `IF … ELSE`, `WHILE`, `BEGIN … END`, `BREAK`, `CONTINUE`, `RETURN` and
//! `PRINT`.
//!
//! The bound nodes derive no `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{BindContext, BoundExprKind, BoundStatement, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo, Value};

/// Binds one statement by itself, returning the first error (parse or bind).
fn err(text: &str) -> SqlError {
    register_builtins();
    match parse_batch(text, &ParseOptions::default()) {
        Err(e) => return e,
        Ok(batch) => {
            let ctx = BindContext::scalar(text, SessionOptions::default());
            for statement in &batch.statements {
                if let Err(error) = bind(statement, &ctx) {
                    return error;
                }
            }
        }
    }
    unreachable!("{text} should not bind")
}

/// Binds one statement, returning the bound form.
fn bind_one(text: &str) -> BoundStatement {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(&batch.statements[0], &ctx).unwrap_or_else(|e| panic!("{text} should bind, got {e:?}"))
}

// ---------------------------------------------------------------------------
// IF / ELSE
// ---------------------------------------------------------------------------

/// `IF 1 = 1 SELECT 1 ELSE SELECT 2` binds both branches: `then_` is the `SELECT 1`
/// query and `else_` is the `SELECT 2` query.
#[test]
fn if_binds_both_branches() {
    let stmt = bind_one("IF 1 = 1 SELECT 1 ELSE SELECT 2");
    let BoundStatement::If {
        condition,
        then_,
        else_,
    } = &stmt
    else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(condition.is_predicate(), "the condition is a predicate");
    assert!(
        matches!(then_.as_ref(), BoundStatement::Query(_)),
        "then_ is a Query, got {then_:?}"
    );
    let else_stmt = else_.as_ref().expect("ELSE was written");
    assert!(
        matches!(else_stmt.as_ref(), BoundStatement::Query(_)),
        "else_ is a Query, got {else_stmt:?}"
    );
}

// ---------------------------------------------------------------------------
// 4145 on a value in a condition position
// ---------------------------------------------------------------------------

/// `IF 1 SELECT 1` raises 4145 because `1` is not a predicate.
#[test]
fn a_value_in_a_condition_is_4145() {
    let error = err("IF 1 SELECT 1");
    assert_eq!(error.number, 4145);

    let error = err("WHILE 'a' SELECT 1");
    assert_eq!(error.number, 4145);
}

// ---------------------------------------------------------------------------
// WHILE
// ---------------------------------------------------------------------------

/// `WHILE 1 = 1 BEGIN SELECT 1; SELECT 2; END` binds the body as a `Block` of two
/// queries.
#[test]
fn while_body_may_be_a_block() {
    let stmt = bind_one("WHILE 1 = 1 BEGIN SELECT 1; SELECT 2; END");
    let BoundStatement::While { condition, body } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(condition.is_predicate());
    let BoundStatement::Block(statements) = body.as_ref() else {
        panic!("body is a Block, got {body:?}");
    };
    assert_eq!(statements.len(), 2);
    assert!(
        matches!(&statements[0], BoundStatement::Query(_)),
        "first body statement is a Query"
    );
    assert!(
        matches!(&statements[1], BoundStatement::Query(_)),
        "second body statement is a Query"
    );
}

/// `BREAK` inside a `WHILE` binds as `BoundStatement::Break`.
#[test]
fn break_inside_while_binds() {
    let stmt = bind_one("WHILE 1 = 1 BREAK;");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(
        matches!(body.as_ref(), BoundStatement::Break),
        "body of WHILE is Break, got {body:?}"
    );
}

/// `CONTINUE` inside a `WHILE` binds as `BoundStatement::Continue`.
#[test]
fn continue_inside_while_binds() {
    let stmt = bind_one("WHILE 1 = 1 CONTINUE;");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(
        matches!(body.as_ref(), BoundStatement::Continue),
        "body of WHILE is Continue, got {body:?}"
    );
}

/// `BREAK` outside a `WHILE` raises error 135.
#[test]
fn break_outside_while_is_135() {
    let error = err("BREAK;");
    assert_eq!(error.number, 135);
}

/// `CONTINUE` outside a `WHILE` raises error 136.
#[test]
fn continue_outside_while_is_136() {
    let error = err("CONTINUE;");
    assert_eq!(error.number, 136);
}

// ---------------------------------------------------------------------------
// BEGIN … END
// ---------------------------------------------------------------------------

/// Three levels of `BEGIN … END` bind to nested `Block` statements.
#[test]
fn nested_blocks_bind_to_nested_statements() {
    let stmt = bind_one("BEGIN BEGIN BEGIN SELECT 1; END; END; END;");
    let BoundStatement::Block(level1) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert_eq!(level1.len(), 1);
    let BoundStatement::Block(level2) = &level1[0] else {
        panic!("expected a nested Block, got {:?}", level1[0]);
    };
    assert_eq!(level2.len(), 1);
    let BoundStatement::Block(level3) = &level2[0] else {
        panic!("expected a third nested Block, got {:?}", level2[0]);
    };
    assert_eq!(level3.len(), 1);
    assert!(
        matches!(&level3[0], BoundStatement::Query(_)),
        "the innermost body is a Query"
    );
}

// ---------------------------------------------------------------------------
// PRINT
// ---------------------------------------------------------------------------

/// `PRINT 1` wraps the integer literal in a `Convert` to `nvarchar(max)`.
#[test]
fn print_converts_to_nvarchar_max() {
    let stmt = bind_one("PRINT 1");
    let BoundStatement::Print(expr) = &stmt else {
        panic!("expected Print, got {stmt:?}");
    };
    // PRINT accepts any expression; the binder inserts a Convert to nvarchar(max).
    // The same conversion applies to `PRINT NULL`, `PRINT GETDATE()`, and `PRINT @x`
    // where `@x` is an int.
    assert_eq!(expr.ty, TypeInfo::new(SqlType::NVarChar(Len::Max), true));
    match &expr.kind {
        BoundExprKind::Convert {
            expr: inner,
            style: None,
            try_: false,
        } => {
            assert!(
                matches!(inner.kind, BoundExprKind::Literal(Value::I32(1))),
                "the operand is the literal 1, got {:?}",
                inner.kind
            );
        }
        other => panic!("expected Convert, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// RETURN
// ---------------------------------------------------------------------------

/// A bare `RETURN` outside a procedure is accepted and binds to
/// `BoundStatement::Return(None)`.
#[test]
fn return_outside_procedure_is_accepted() {
    let stmt = bind_one("RETURN;");
    assert!(
        matches!(&stmt, BoundStatement::Return(None)),
        "bare RETURN is Return(None), got {stmt:?}"
    );
}

/// `RETURN 1` outside a procedure raises error 178.
#[test]
fn return_with_value_outside_procedure_is_178() {
    let error = err("RETURN 1");
    assert_eq!(error.number, 178);
}

// ---------------------------------------------------------------------------
// GOTO
// ---------------------------------------------------------------------------

/// `GOTO label` is rejected by the parser in V1 as a syntax error (156), before reaching
/// the binder.
#[test]
fn goto_is_refused_as_v2() {
    let error = err("GOTO foo;");
    assert_eq!(error.number, 156);
}
