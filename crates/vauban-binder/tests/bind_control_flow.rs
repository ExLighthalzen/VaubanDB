//! Control of flow: `IF … ELSE`, `WHILE`, `BEGIN … END`, `BREAK`, `CONTINUE`, `RETURN` and
//! `PRINT`.
//!
//! The bound nodes derive no `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{
    BindContext, BoundExprKind, BoundStatement, CatalogView, NoVariables, SessionOptions, bind,
};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, Statement, parse_batch};
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

/// 135 carries the line of the BREAK statement, not line 1 of the batch.
#[test]
fn break_outside_while_carries_the_line_of_the_statement() {
    let error = err("SELECT 1;\nBREAK;");
    assert_eq!(error.line, 2);
}

/// 136 carries the line of the CONTINUE statement, not line 1 of the batch.
#[test]
fn continue_outside_while_carries_the_line_of_the_statement() {
    let error = err("SELECT 1;\nSELECT 2;\nCONTINUE;");
    assert_eq!(error.line, 3);
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

/// `GOTO label` parses; the binder does not bind it yet and reports the generic internal
/// error, naming the statement.
#[test]
fn goto_is_unsupported_by_the_binder() {
    let error = err("GOTO foo;");
    assert_eq!(error.number, 50000, "{}", error.message);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(
        error.message,
        "Internal error: internal bug: GOTO is not implemented yet"
    );
}

// ---------------------------------------------------------------------------
// A statement of a branch or of a loop body over a table the catalogue lacks
// ---------------------------------------------------------------------------

/// A catalogue that holds nothing: every name it is asked about is missing, which is what
/// puts 208 where a scalar context answers "this form needs a catalogue".
struct EmptyCatalog;

impl CatalogView for EmptyCatalog {}

/// Binds one statement against [`EmptyCatalog`], returning the bound form.
fn bind_empty(text: &str) -> BoundStatement {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    bind(&batch.statements[0], &empty_ctx(text))
        .unwrap_or_else(|e| panic!("{text} should bind, got {e:?}"))
}

/// Binds one statement against [`EmptyCatalog`], returning the error it answers.
fn err_empty(text: &str) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    bind(&batch.statements[0], &empty_ctx(text))
        .err()
        .unwrap_or_else(|| panic!("{text} should not bind"))
}

/// A binding context over [`EmptyCatalog`].
fn empty_ctx(text: &str) -> BindContext<'_> {
    BindContext {
        text,
        catalog: Some(&EmptyCatalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    }
}

/// The statement a [`BoundStatement::Deferred`] kept, or a panic naming what was bound.
fn deferred(stmt: &BoundStatement) -> &Statement {
    match stmt {
        BoundStatement::Deferred(deferred) => deferred.statement.as_ref(),
        other => panic!("expected a deferred statement, got {other:?}"),
    }
}

/// `IF 1 = 0 SELECT * FROM dbo.absent` binds: the branch keeps the `SELECT` as it was
/// parsed instead of resolving `dbo.absent` now.
///
/// The context of these tests carries no catalogue, so every table name is missing: what
/// separates this test from the one below is the **position** of the statement, not the
/// name it writes.
#[test]
fn a_branch_over_a_missing_table_binds_deferred() {
    let stmt = bind_empty("IF 1 = 0 SELECT * FROM dbo.absent");
    let BoundStatement::If { then_, else_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(
        matches!(deferred(then_), Statement::Select(_)),
        "the branch kept the SELECT"
    );
    assert!(else_.is_none(), "no ELSE was written");
}

/// The same statement written at the head of a batch is still error 208: the deferral
/// belongs to the branch and to the loop body, and the caller decides what to do with a
/// statement of its own.
#[test]
fn a_head_statement_over_a_missing_table_is_still_208() {
    assert_eq!(err_empty("SELECT * FROM dbo.absent").number, 208);
    assert_eq!(err_empty("INSERT dbo.absent VALUES (1)").number, 208);
}

/// `BEGIN CREATE TABLE …; INSERT … END` binds: the `CREATE TABLE` binds as it always did
/// and the `INSERT` after it waits, which is what lets the block create a table and then
/// write to it.
#[test]
fn a_block_that_creates_then_inserts_defers_the_insert() {
    let stmt = bind_empty("BEGIN CREATE TABLE dbo.blk (a int); INSERT dbo.blk VALUES (1); END");
    let BoundStatement::Block(statements) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(
        matches!(&statements[0], BoundStatement::Ddl(_)),
        "the CREATE TABLE bound, got {:?}",
        statements[0]
    );
    assert!(
        matches!(deferred(&statements[1]), Statement::Insert(_)),
        "the INSERT waits"
    );
}

/// The body of a `WHILE` and the statement of an `ELSE` are deferred on the same rule.
#[test]
fn a_while_body_and_an_else_over_a_missing_table_bind_deferred() {
    let stmt = bind_empty("WHILE 1 = 0 INSERT dbo.absent VALUES (1)");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(matches!(deferred(body), Statement::Insert(_)));

    let stmt = bind_empty("IF 1 = 1 SELECT 1 ELSE SELECT * FROM dbo.absent");
    let BoundStatement::If { then_, else_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(
        matches!(then_.as_ref(), BoundStatement::Query(_)),
        "the taken branch bound as usual"
    );
    let else_ = else_.as_ref().expect("ELSE was written");
    assert!(matches!(deferred(else_), Statement::Select(_)));
}

/// A deferred statement carries the line its first binding attempt reported, so an error
/// raised later has one to fall back on.
#[test]
fn a_deferred_statement_carries_the_line_of_its_branch() {
    let stmt = bind_empty("IF 1 = 0\n  SELECT * FROM dbo.absent");
    let BoundStatement::If { then_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    let BoundStatement::Deferred(deferred) = then_.as_ref() else {
        panic!("expected a deferred statement, got {then_:?}");
    };
    assert_eq!(deferred.line, 2, "{deferred:?}");
}

/// A branch is deferred on the numbers that name a missing table and on no other: a
/// non-boolean condition written in the branch is still error 4145 for the whole `IF`,
/// and an undeclared variable is still error 137.
#[test]
fn a_branch_that_fails_on_another_number_is_still_a_binding_error() {
    assert_eq!(err("IF 1 = 0 SELECT 1 WHERE 1;").number, 4145);
    assert_eq!(err("IF 1 = 0 SELECT @nope;").number, 137);
}

/// The **condition** is read before a branch is chosen, so a missing table written there
/// is error 208 for the whole `IF`, deferred by the caller as a statement of its own.
#[test]
fn a_condition_over_a_missing_table_is_still_208() {
    assert_eq!(
        err_empty("IF EXISTS (SELECT * FROM dbo.absent) SELECT 1;").number,
        208
    );
    assert_eq!(
        err_empty("WHILE EXISTS (SELECT * FROM dbo.absent) SELECT 1;").number,
        208
    );
}

/// One level down, the same condition is deferred with the `IF` that holds it: the block
/// keeps the whole `IF` unbound rather than answering 208 for the batch.
#[test]
fn a_condition_of_a_nested_if_is_deferred_with_it() {
    let stmt = bind_empty("BEGIN IF EXISTS (SELECT * FROM dbo.absent) SELECT 1; END");
    let BoundStatement::Block(statements) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert_eq!(statements.len(), 1, "{statements:?}");
    assert!(matches!(deferred(&statements[0]), Statement::If { .. }));
}
