//! `EXECUTE`: arguments evaluated, hand-off to the session.
//!
//! The bound plans are built by hand; the binder and the planner are not involved.

use vauban_binder::{
    BoundExecArg, BoundExecTarget, BoundExecute, BoundExpr, BoundExprKind, SessionOptions,
};
use vauban_errors::SqlResult;
use vauban_executor::{
    ExecContext, ExecOutcome, ExecSession, RowSink, eval_expr, execute, execute_bound,
};
use vauban_parser::{Ident, ObjectName, Span};
use vauban_planner::PhysicalStatement;
use vauban_sysfn::StaticContext;
use vauban_types::{BinaryOp, Len, SqlString, SqlType, TypeInfo, Value};

fn int_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn varchar_ty(n: u16) -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(Len::Fixed(n)), true)
}

fn int_lit(n: i32) -> BoundExpr {
    lit(Value::I32(n), int_ty())
}

fn lit(value: Value, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 1,
    }
}

fn str_lit(s: &str) -> BoundExpr {
    lit(
        Value::String(SqlString { text: s.to_owned() }),
        varchar_ty(u16::try_from(s.len()).unwrap_or(u16::MAX).max(1)),
    )
}

fn var_ref(name: &str, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Variable {
            name: name.to_owned(),
        },
        ty,
        line: 1,
    }
}

fn add(left: BoundExpr, right: BoundExpr, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Arith {
            op: BinaryOp::Add,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        line: 1,
    }
}

struct CountingSink {
    rows: usize,
    columns: usize,
    infos: usize,
}

impl RowSink for CountingSink {
    fn columns(&mut self, _schema: &vauban_binder::OutputSchema) -> SqlResult<()> {
        self.columns += 1;
        Ok(())
    }

    fn row(&mut self, _row: &[Value]) -> SqlResult<()> {
        self.rows += 1;
        Ok(())
    }

    fn info(&mut self, _message: &vauban_errors::InfoMessage) -> SqlResult<()> {
        self.infos += 1;
        Ok(())
    }
}

fn procedure_target(name: &str) -> BoundExecTarget {
    BoundExecTarget::Procedure {
        name: name.to_owned(),
        raw: ObjectName {
            server: None,
            database: None,
            schema: None,
            name: Ident {
                value: name.to_owned(),
                quoted: false,
            },
            span: Span::EMPTY,
        },
    }
}

fn run(session: &mut ExecSession, stmt: &BoundExecute) -> (ExecOutcome, CountingSink) {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(session);
    let mut sink = CountingSink {
        rows: 0,
        columns: 0,
        infos: 0,
    };
    let outcome = execute_bound(stmt, &mut ctx, &mut sink).expect("the EXECUTE evaluates");
    (outcome, sink)
}

/// Session `@Nom = 1`, read `@nom` — the value is 1.
#[test]
fn variable_lookup_ignores_ascii_case() {
    let mut session = ExecSession::default();
    session.variables.insert("@Nom".to_owned(), Value::I32(1));
    session.variable_types.insert("@Nom".to_owned(), int_ty());
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let value =
        eval_expr(&var_ref("@nom", int_ty()), None, &mut ctx).expect("the variable is found");
    assert_eq!(value, Value::I32(1));
}

/// `SET @NOM = 2` updates `@Nom` in place — one entry remains.
#[test]
fn variable_assignment_updates_the_declared_spelling() {
    let mut session = ExecSession::default();
    session.variables.insert("@Nom".to_owned(), Value::I32(1));
    session.variable_types.insert("@Nom".to_owned(), int_ty());
    let set = PhysicalStatement::SetVariable {
        name: "@NOM".to_owned(),
        value: int_lit(2),
    };
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_session(&mut session);
    let mut sink = CountingSink {
        rows: 0,
        columns: 0,
        infos: 0,
    };
    let outcome = execute(&set, &mut ctx, &mut sink).expect("SET runs");
    assert!(matches!(outcome, ExecOutcome::NoRows));
    assert_eq!(session.variables.len(), 1);
    assert_eq!(session.variables.get("@Nom"), Some(&Value::I32(2)));
    assert!(!session.variables.contains_key("@NOM"));
}

#[test]
fn arguments_are_evaluated_in_order() {
    let mut session = ExecSession::default();
    session.variables.insert("@v".to_owned(), Value::I32(7));
    session.variable_types.insert("@v".to_owned(), int_ty());
    let stmt = BoundExecute {
        target: procedure_target("sp_x"),
        args: vec![
            BoundExecArg {
                name: Some("@a".to_owned()),
                value: Some(BoundExprKind::Literal(Value::I32(1))),
                output: false,
            },
            BoundExecArg {
                name: Some("@b".to_owned()),
                value: Some(BoundExprKind::Variable {
                    name: "@v".to_owned(),
                }),
                output: false,
            },
        ],
        return_into: None,
        line: 1,
    };
    let (outcome, sink) = run(&mut session, &stmt);
    let ExecOutcome::CallProcedure { args, .. } = outcome else {
        panic!("expected CallProcedure, got {outcome:?}");
    };
    assert_eq!(args.len(), 2);
    assert_eq!(args[0].name.as_deref(), Some("@a"));
    assert_eq!(
        args[0].value.as_ref().map(|(v, t)| (v, t.ty)),
        Some((&Value::I32(1), SqlType::Int))
    );
    assert_eq!(args[1].name.as_deref(), Some("@b"));
    assert_eq!(
        args[1].value.as_ref().map(|(v, t)| (v, t.ty)),
        Some((&Value::I32(7), SqlType::Int))
    );
    assert_eq!(sink.rows, 0);
    assert_eq!(sink.columns, 0);
    assert_eq!(sink.infos, 0);
}

#[test]
fn output_argument_keeps_the_caller_variable() {
    let mut session = ExecSession::default();
    session.variables.insert("@o".to_owned(), Value::I32(0));
    session.variable_types.insert("@o".to_owned(), int_ty());
    let stmt = BoundExecute {
        target: procedure_target("sp_x"),
        args: vec![BoundExecArg {
            name: None,
            value: Some(BoundExprKind::Variable {
                name: "@o".to_owned(),
            }),
            output: true,
        }],
        return_into: None,
        line: 1,
    };
    let (outcome, sink) = run(&mut session, &stmt);
    let ExecOutcome::CallProcedure { args, .. } = outcome else {
        panic!("expected CallProcedure, got {outcome:?}");
    };
    assert!(args[0].output);
    assert_eq!(args[0].output_variable.as_deref(), Some("@o"));
    assert_eq!(args[0].value, Some((Value::I32(0), int_ty())));
    assert_eq!(sink.rows, 0);
}

#[test]
fn dynamic_text_is_concatenated() {
    let mut session = ExecSession::default();
    let text_ty = varchar_ty(20);
    let stmt = BoundExecute {
        target: BoundExecTarget::Dynamic(add(str_lit("SEL"), str_lit("ECT 1"), text_ty.clone())),
        args: Vec::new(),
        return_into: None,
        line: 1,
    };
    let (outcome, sink) = run(&mut session, &stmt);
    let ExecOutcome::RunDynamic { text, line } = outcome else {
        panic!("expected RunDynamic, got {outcome:?}");
    };
    assert_eq!(text, "SELECT 1");
    assert_eq!(line, 1);
    assert_eq!(sink.rows, 0);
}

#[test]
fn null_dynamic_text_is_empty() {
    let mut session = ExecSession::default();
    session.variables.insert("@t".to_owned(), Value::Null);
    session
        .variable_types
        .insert("@t".to_owned(), varchar_ty(10));
    let stmt = BoundExecute {
        target: BoundExecTarget::Dynamic(var_ref("@t", varchar_ty(10))),
        args: Vec::new(),
        return_into: None,
        line: 1,
    };
    let (outcome, _sink) = run(&mut session, &stmt);
    let ExecOutcome::RunDynamic { text, .. } = outcome else {
        panic!("expected RunDynamic, got {outcome:?}");
    };
    assert!(
        text.is_empty(),
        "NULL dynamic text is empty on that shape only"
    );
}

#[test]
fn call_procedure_and_run_dynamic_emit_nothing_on_the_sink() {
    let mut session = ExecSession::default();
    session.variables.insert("@o".to_owned(), Value::I32(0));
    session.variable_types.insert("@o".to_owned(), int_ty());
    let procedure = BoundExecute {
        target: procedure_target("sp_x"),
        args: vec![BoundExecArg {
            name: None,
            value: Some(BoundExprKind::Literal(Value::I32(1))),
            output: false,
        }],
        return_into: None,
        line: 1,
    };
    let dynamic = BoundExecute {
        target: BoundExecTarget::Dynamic(str_lit("SELECT 1")),
        args: Vec::new(),
        return_into: None,
        line: 2,
    };
    for stmt in [&procedure, &dynamic] {
        let (_, sink) = run(&mut session, stmt);
        assert_eq!(sink.rows, 0, "no row for {stmt:?}");
        assert_eq!(sink.columns, 0, "no schema for {stmt:?}");
        assert_eq!(sink.infos, 0, "no info for {stmt:?}");
    }
}
