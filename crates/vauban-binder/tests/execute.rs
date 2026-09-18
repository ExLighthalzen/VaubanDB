//! The `EXECUTE` statement: the target and the arguments of a call, bound.
//!
//! The batches are bound **statement by statement**, the way a session binds them, so that
//! a `DECLARE` enters the scope before the `EXEC` that uses its variable. No catalogue is
//! consulted: the binder normalises the procedure name and leaves its resolution to the
//! session.
//!
//! The bound nodes derive no `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{
    BatchVariables, BindContext, BoundExecTarget, BoundExecute, BoundExprKind, BoundStatement,
    SessionOptions, bind,
};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo};

/// Binds `text` statement by statement against a scope that starts empty, entering each
/// bound `DECLARE` before the next statement, and stops at the first error.
fn run(text: &str) -> (Vec<BoundStatement>, Option<SqlError>) {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let mut scope = BatchVariables::new();
    let mut bound = Vec::new();
    for statement in &batch.statements {
        let ctx = BindContext {
            text,
            catalog: None,
            database: "master",
            default_schema: "dbo",
            variables: &scope,
            options: SessionOptions::default(),
        };
        match bind(statement, &ctx) {
            Ok(stmt) => {
                if let BoundStatement::Declare(declarations) = &stmt {
                    for declaration in declarations {
                        scope
                            .declare(&declaration.name, declaration.ty.clone())
                            .expect("the binder refused the duplicates first");
                    }
                }
                bound.push(stmt);
            }
            Err(error) => return (bound, Some(error)),
        }
    }
    (bound, None)
}

/// The `EXECUTE` a batch that binds to the end ends with.
fn execute_of(text: &str) -> BoundExecute {
    let (bound, error) = run(text);
    assert!(error.is_none(), "{text} binds, got {error:?}");
    match bound.into_iter().next_back() {
        Some(BoundStatement::Execute(execute)) => execute,
        other => panic!("expected an EXECUTE, got {other:?}"),
    }
}

/// The error a batch stops on.
fn error_of(text: &str) -> SqlError {
    let (_, error) = run(text);
    error.unwrap_or_else(|| panic!("{text} is refused"))
}

/// The normalised name of a procedure call.
fn name_of(text: &str) -> String {
    match execute_of(text).target {
        BoundExecTarget::Procedure { name, .. } => name,
        other => panic!("expected a procedure, got {other:?}"),
    }
}

#[test]
fn a_system_procedure_name_is_normalised() {
    assert_eq!(name_of("EXEC sp_executesql N'SELECT 1';"), "sp_executesql");
    assert_eq!(
        name_of("EXEC [sys].[sp_executesql] @stmt = N'SELECT 1';"),
        "sp_executesql"
    );
    assert_eq!(
        name_of("EXEC master..sp_executesql @stmt = N'SELECT 1';"),
        "sp_executesql"
    );
}

#[test]
fn a_positional_argument_has_no_name_and_a_named_one_keeps_its() {
    let positional = execute_of("EXEC sp_executesql N'SELECT 1';");
    assert_eq!(positional.args.len(), 1);
    assert_eq!(positional.args[0].name, None);
    assert!(!positional.args[0].output);

    let named = execute_of("EXEC sp_executesql @stmt = N'SELECT 1';");
    assert_eq!(named.args.len(), 1);
    assert_eq!(named.args[0].name.as_deref(), Some("@stmt"));
}

#[test]
fn positional_after_named_is_119() {
    let error = error_of("EXEC sp_executesql @stmt = N'SELECT 1', 2;");
    assert_eq!(error.number, 119);
}

#[test]
fn output_on_a_constant_is_179() {
    let error = error_of("EXEC sp_executesql N'SELECT 1' OUTPUT;");
    assert_eq!(error.number, 179);
}

#[test]
fn an_unknown_variable_target_is_137() {
    let error = error_of("EXEC (@t);");
    assert_eq!(error.number, 137);
    let error = error_of("EXEC @t;");
    assert_eq!(error.number, 137);
}

#[test]
fn an_unknown_argument_variable_is_137() {
    let error = error_of("EXEC sp_executesql N'SELECT 1', @nosuch;");
    assert_eq!(error.number, 137);
}

#[test]
fn an_unknown_return_variable_is_137() {
    let error = error_of("EXEC @r = sp_executesql N'SELECT 1';");
    assert_eq!(error.number, 137);
}

#[test]
fn an_argument_expression_is_102() {
    let error = error_of("EXEC sp_executesql N'SELECT 1', 1 + 1;");
    assert_eq!(error.number, 102);
}

#[test]
fn a_parenthesised_target_that_is_not_a_string_is_102() {
    let error = error_of("EXEC (1);");
    assert_eq!(error.number, 102);
}

#[test]
fn dynamic_text_is_bound_as_a_string_expression() {
    let execute = execute_of("EXEC ('SEL' + 'ECT 1');");
    match execute.target {
        BoundExecTarget::Dynamic(expr) => {
            assert!(expr.ty.ty.is_string(), "the text is a string: {expr:?}");
        }
        other => panic!("expected Dynamic, got {other:?}"),
    }
}

#[test]
fn a_constant_argument_keeps_its_value() {
    let execute = execute_of("EXEC sp_executesql N'S', N'@x int', -1;");
    let last = execute.args.last().expect("an argument");
    assert!(
        matches!(
            last.value,
            Some(BoundExprKind::Literal(_)) | Some(BoundExprKind::Negate(_))
        ),
        "a signed literal is a constant: {last:?}"
    );
}

#[test]
fn a_default_argument_keeps_no_value() {
    let execute = execute_of("DECLARE @i int; EXEC sp_x @i = DEFAULT;");
    let last = execute.args.last().expect("an argument");
    assert_eq!(last.name.as_deref(), Some("@i"));
    assert!(last.value.is_none(), "DEFAULT keeps no value");
}

#[test]
fn a_variable_argument_keeps_its_name() {
    let execute = execute_of("DECLARE @v int; EXEC sp_x @v;");
    let last = execute.args.last().expect("an argument");
    assert!(
        matches!(&last.value, Some(BoundExprKind::Variable { name }) if name == "@v"),
        "{last:?}"
    );
}

#[test]
fn return_into_is_kept_when_the_variable_is_declared() {
    let execute = execute_of("DECLARE @r int; EXEC @r = sp_executesql N'SELECT 1';");
    assert_eq!(execute.return_into.as_deref(), Some("@r"));
}

#[test]
fn a_declared_variable_target_is_a_procedure_variable() {
    // `EXEC @t` calls the procedure the variable names, where `EXEC(@t)` runs its value as
    // T-SQL: the two targets are distinct.
    let execute = execute_of("DECLARE @t varchar(20); EXEC @t;");
    match execute.target {
        BoundExecTarget::ProcedureVariable(expr) => {
            assert!(matches!(expr.kind, BoundExprKind::Variable { .. }));
        }
        other => panic!("expected ProcedureVariable, got {other:?}"),
    }
}

#[test]
fn an_int_procedure_variable_is_8199() {
    let error = error_of("DECLARE @t int; EXEC @t;");
    assert_eq!(error.number, 8199);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
}

#[test]
fn character_procedure_variables_bind() {
    for text in [
        "DECLARE @t varchar(20); EXEC @t;",
        "DECLARE @t nvarchar(20); EXEC @t;",
    ] {
        match execute_of(text).target {
            BoundExecTarget::ProcedureVariable(_) => {}
            other => panic!("{text} expected ProcedureVariable, got {other:?}"),
        }
    }
}

#[test]
fn a_sysname_procedure_variable_binds() {
    // `sysname` is `nvarchar(128)` on the server; `DECLARE @t sysname` is not bound here yet.
    register_builtins();
    let text = "EXEC @t;";
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let mut scope = BatchVariables::new();
    scope
        .declare(
            "@t",
            TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), false),
        )
        .expect("@t is declared once");
    let ctx = BindContext {
        text,
        catalog: None,
        database: "master",
        default_schema: "dbo",
        variables: &scope,
        options: SessionOptions::default(),
    };
    let BoundStatement::Execute(execute) = bind(&batch.statements[0], &ctx)
        .unwrap_or_else(|error| panic!("{text} binds, got {error:?}"))
    else {
        panic!("expected an EXECUTE");
    };
    match execute.target {
        BoundExecTarget::ProcedureVariable(_) => {}
        other => panic!("expected ProcedureVariable, got {other:?}"),
    }
}

#[test]
fn an_undeclared_procedure_variable_is_137_before_8199() {
    assert_eq!(error_of("EXEC @t;").number, 137);
}

#[test]
fn a_parenthesised_int_variable_is_102_not_8199() {
    // `EXEC(@t)` is dynamic: the binder checks the text shape, not the procedure-name rule.
    assert_eq!(error_of("DECLARE @t int = 1; EXEC(@t);").number, 102);
}

#[test]
fn the_refusal_order_is_sql_servers() {
    // The `@r =` variable first, then argument by argument; within one argument 102 before
    // 137, 137 before 119, 119 before 179.
    assert_eq!(error_of("EXEC sp_x @a = 1, @nosuch;").number, 137);
    assert_eq!(error_of("EXEC sp_x @a = 1, 1 + 1;").number, 102);
    assert_eq!(error_of("EXEC @r = sp_x @a = 1, 2;").number, 137);
    assert_eq!(error_of("EXEC @r = sp_x 1 OUTPUT;").number, 137);
    assert_eq!(error_of("EXEC @r = sp_x @nosuch;").number, 137);
    assert_eq!(error_of("EXEC sp_x @a = 1, 2 OUTPUT;").number, 119);
}

#[test]
fn output_on_default_is_179() {
    assert_eq!(error_of("EXEC sp_x DEFAULT OUTPUT;").number, 179);
}

#[test]
fn the_near_token_is_one_token() {
    assert!(
        error_of("EXEC sp_x 1 + 1;").message.contains("near '+'"),
        "an argument quotes its operator"
    );
    assert!(
        error_of("EXEC (CAST(1 AS int));")
            .message
            .contains("near 'CAST'"),
        "a target quotes its first token"
    );
}
