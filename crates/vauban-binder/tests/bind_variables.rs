//! The variables of a batch: `DECLARE`, `SET @x`, `SELECT @x = e`, and the scope that holds
//! them, `BatchVariables`.
//!
//! The batches are bound **statement by statement**, the way a session binds them: the
//! declarations a `DECLARE` binds to enter the scope before the next statement is bound
//! ([`run`]). The scope is built in the test, never by the binder, which reads it and does
//! not write it. The cases with a `FROM` use a double of `CatalogView` that knows one
//! table, `t`, with two `int` columns `a` and `b`.
//!
//! The bound nodes derive no `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{
    BatchVariables, BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView,
    ColumnBinding, LogicalPlan, ResolvedTable, ResolvedTableKind, SessionOptions, VariableScope,
    bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo, Value};

/// A catalogue that knows one table, `t (a int, b int)`.
struct OneTable;

impl CatalogView for OneTable {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        (name.name.value == "t").then(|| ResolvedTable {
            object: ObjectId(1),
            table: Some(TableId(1)),
            columns: vec![column("a", 0), column("b", 1)],
            kind: ResolvedTableKind::Table,
        })
    }
}

fn column(name: &str, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(SqlType::Int, true),
    }
}

/// Binds `text` statement by statement against a scope that starts empty, entering the
/// names of each bound `DECLARE` before the next statement, and stops at the first error.
///
/// Returns the statements bound so far, the error that stopped the batch, and the scope
/// as it stands.
fn run(text: &str) -> (Vec<BoundStatement>, Option<SqlError>, BatchVariables) {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let catalog = OneTable;
    let mut scope = BatchVariables::new();
    let mut bound = Vec::new();
    for statement in &batch.statements {
        let ctx = BindContext {
            text,
            catalog: Some(&catalog),
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
            Err(error) => return (bound, Some(error), scope),
        }
    }
    (bound, None, scope)
}

/// The statements of a batch that binds to the end.
fn binds(text: &str) -> Vec<BoundStatement> {
    let (bound, error, _) = run(text);
    assert!(error.is_none(), "{text} binds, got {error:?}");
    bound
}

/// The error that stops a batch.
fn err(text: &str) -> SqlError {
    let (_, error, _) = run(text);
    error.unwrap_or_else(|| unreachable!("{text} does not bind"))
}

/// The `Convert` node at the top of `expr`: its target type and its operand.
fn convert_of(expr: &BoundExpr) -> (&TypeInfo, &BoundExpr) {
    match &expr.kind {
        BoundExprKind::Convert {
            expr: inner,
            style: None,
            try_: false,
        } => (&expr.ty, inner),
        other => panic!("expected a Convert, got {other:?}"),
    }
}

/// The `(name, value)` of a `SetVariable`.
fn set_variable(stmt: &BoundStatement) -> (&str, &BoundExpr) {
    match stmt {
        BoundStatement::SetVariable { name, value } => (name, value),
        other => panic!("expected a SetVariable, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// DECLARE and the scope
// ---------------------------------------------------------------------------------------

/// `DECLARE @x int; SELECT @x;` binds `@x` with the type `int`, and `DECLARE @s
/// varchar(10)` with the length written, both nullable.
#[test]
fn declare_then_use_binds_with_the_declared_type() {
    let bound = binds("DECLARE @x int; DECLARE @s varchar(10); SELECT @x, @s;");
    assert_eq!(bound.len(), 3);
    let BoundStatement::Declare(declarations) = &bound[0] else {
        panic!("expected a Declare, got {:?}", bound[0]);
    };
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0].name, "@x");
    assert_eq!(declarations[0].ty, TypeInfo::new(SqlType::Int, true));
    assert!(declarations[0].value.is_none());

    let BoundStatement::Query(plan) = &bound[2] else {
        panic!("expected a Query, got {:?}", bound[2]);
    };
    let columns = &plan.schema().columns;
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].ty, TypeInfo::new(SqlType::Int, true));
    assert_eq!(
        columns[1].ty,
        TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true)
    );
    let LogicalPlan::Project { exprs, .. } = &**plan else {
        panic!("expected a Project, got {plan:?}");
    };
    assert!(
        matches!(&exprs[0].expr.kind, BoundExprKind::Variable { name } if name == "@x"),
        "the read of a declared variable is a Variable node, got {:?}",
        exprs[0].expr.kind
    );
}

/// The scope answers a declared name in another case, and the binder reads it so:
/// `DECLARE @x int; SELECT @X;` binds.
#[test]
fn the_scope_is_case_insensitive() {
    let mut scope = BatchVariables::new();
    scope
        .declare("@x", TypeInfo::new(SqlType::Int, true))
        .expect("a first declaration");
    assert_eq!(scope.type_of("@X"), Some(TypeInfo::new(SqlType::Int, true)));
    assert_eq!(scope.type_of("@y"), None);
    assert_eq!(
        scope
            .declare("@X", TypeInfo::new(SqlType::Bit, true))
            .expect_err("the same name in another case")
            .number,
        134
    );

    let bound = binds("DECLARE @x int; SELECT @X;");
    assert_eq!(bound.len(), 2);
}

/// `SELECT @y;` with nothing declared is 137, severity 15, state 2, on the line of the
/// variable, and the message of the constructor.
#[test]
fn an_undeclared_variable_is_137() {
    let error = err("SELECT\n@y;");
    assert_eq!(error.number, 137);
    assert_eq!(error.severity, 15);
    assert_eq!(error.state, 2);
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        SqlError::must_declare_scalar_variable("@y").message
    );

    // The assigning forms send state 1.
    let error = err("SET @y = 1;");
    assert_eq!((error.number, error.state, error.line), (137, 1, 1));
    let error = err("SELECT @y = 1;");
    assert_eq!((error.number, error.state, error.line), (137, 1, 1));
}

/// A second `DECLARE` of a name in scope is 134, severity 15, state 1, whether in another
/// statement, in the same statement, or in another case; the message quotes the second
/// spelling.
#[test]
fn a_second_declare_of_the_same_name_is_134() {
    let error = err("DECLARE @x int;\nDECLARE @x int;");
    assert_eq!((error.number, error.severity, error.state), (134, 15, 1));
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        SqlError::variable_already_declared("@x").message
    );

    let error = err("DECLARE @x int, @x bit;");
    assert_eq!((error.number, error.line), (134, 1));

    let error = err("DECLARE @x int; DECLARE @X int;");
    assert_eq!(error.number, 134);
    assert!(error.message.contains("'@X'"), "{}", error.message);
}

/// `DECLARE @x int = '1'` carries a `Convert` towards `int` over the literal, not a
/// folded value; `DECLARE @x int = 1` carries the literal itself, already an `int`.
#[test]
fn declare_with_an_initial_value_inserts_a_convert() {
    let bound = binds("DECLARE @x int = '1';");
    let BoundStatement::Declare(declarations) = &bound[0] else {
        panic!("expected a Declare, got {:?}", bound[0]);
    };
    let value = declarations[0].value.as_ref().expect("an initial value");
    let (ty, inner) = convert_of(value);
    assert_eq!(ty.ty, SqlType::Int);
    assert!(ty.nullable);
    assert!(
        matches!(&inner.kind, BoundExprKind::Literal(Value::String(_))),
        "the operand is the string literal, got {:?}",
        inner.kind
    );

    let bound = binds("DECLARE @x int = 1;");
    let BoundStatement::Declare(declarations) = &bound[0] else {
        panic!("expected a Declare, got {:?}", bound[0]);
    };
    let value = declarations[0].value.as_ref().expect("an initial value");
    assert!(
        matches!(&value.kind, BoundExprKind::Literal(Value::I32(1))),
        "an int literal needs no conversion, got {:?}",
        value.kind
    );

    // `DECLARE @x int = 1 + 1` is the operation, not 2: the binder folds nothing.
    let bound = binds("DECLARE @x int = 1 + 1;");
    let BoundStatement::Declare(declarations) = &bound[0] else {
        panic!("expected a Declare, got {:?}", bound[0]);
    };
    let value = declarations[0].value.as_ref().expect("an initial value");
    assert!(
        matches!(&value.kind, BoundExprKind::Arith { .. }),
        "got {:?}",
        value.kind
    );
}

/// Within one `DECLARE`, an item does not see the items before it: `DECLARE @a int = 1,
/// @b int = @a;` is 137 on `@a`, state 2. The counter-proof: split in two statements, it
/// binds.
#[test]
fn a_declaration_does_not_see_the_items_of_its_own_statement() {
    let error = err("DECLARE @a int = 1, @b int = @a;");
    assert_eq!((error.number, error.state), (137, 2));
    assert!(error.message.contains("\"@a\""), "{}", error.message);

    let bound = binds("DECLARE @a int = 1; DECLARE @b int = @a;");
    assert_eq!(bound.len(), 2);
}

/// An initial value whose type has no implicit conversion to the declared type is 206,
/// severity 16, state 2, the value's type named first, on the line of the statement.
#[test]
fn an_initial_value_of_an_incompatible_type_is_206() {
    let error = err("DECLARE @x int = NEWID();");
    assert_eq!((error.number, error.severity, error.state), (206, 16, 2));
    assert_eq!(error.line, 1);
    assert_eq!(
        error.message,
        SqlError::operand_type_clash("uniqueidentifier", "int").message
    );

    let error = err("DECLARE @g uniqueidentifier = 1;");
    assert_eq!(
        error.message,
        SqlError::operand_type_clash("int", "uniqueidentifier").message
    );

    let error = err("DECLARE @d date = 1;");
    assert_eq!(error.number, 206);

    // An untyped NULL converts to a type an int would not.
    let bound = binds("DECLARE @g uniqueidentifier = NULL;");
    assert_eq!(bound.len(), 1);
}

/// `DECLARE @x foo` is 2715 naming the position of the item; the second item is `#2`.
#[test]
fn an_unknown_type_is_2715_with_the_position_of_the_item() {
    let error = err("DECLARE @x foo;");
    assert_eq!(error.number, 2715);
    assert!(error.message.contains("#1"), "{}", error.message);

    let error = err("DECLARE @x int, @y foo;");
    assert_eq!(error.number, 2715);
    assert!(error.message.contains("#2"), "{}", error.message);
}

/// `DECLARE @t TABLE (…)` answers the internal error 50000 naming V2, and enters nothing
/// into the scope.
#[test]
fn a_table_variable_is_out_of_scope() {
    let (bound, error, scope) = run("DECLARE @t TABLE (a int); SELECT 1;");
    let error = error.expect("a table variable does not bind");
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("V2"), "{}", error.message);
    assert!(error.message.contains("TABLE"), "{}", error.message);
    assert!(bound.is_empty());
    assert_eq!(scope.type_of("@t"), None);
}

// ---------------------------------------------------------------------------------------
// SET @x
// ---------------------------------------------------------------------------------------

/// `SET @x = '2'` on an `int` binds a `SetVariable` whose value is a `Convert` to `int`
/// over the literal; `SET @x = 2` needs none.
#[test]
fn set_variable_converts_to_the_declared_type() {
    let bound = binds("DECLARE @x int; SET @x = '2';");
    let (name, value) = set_variable(&bound[1]);
    assert_eq!(name, "@x");
    let (ty, inner) = convert_of(value);
    assert_eq!(ty.ty, SqlType::Int);
    assert!(matches!(
        &inner.kind,
        BoundExprKind::Literal(Value::String(_))
    ));

    let bound = binds("DECLARE @x int; SET @x = 2;");
    let (_, value) = set_variable(&bound[1]);
    assert!(matches!(&value.kind, BoundExprKind::Literal(Value::I32(2))));

    // The declared length is the target: a longer literal converts to varchar(10).
    let bound = binds("DECLARE @s varchar(10); SET @s = 'abcdefghijklmnop';");
    let (_, value) = set_variable(&bound[1]);
    let (ty, _) = convert_of(value);
    assert_eq!(ty.ty, SqlType::VarChar(Len::Fixed(10)));

    let error = err("DECLARE @x int; SET @x = NEWID();");
    assert_eq!((error.number, error.line), (206, 1));
}

/// `SET @x += 2` binds the value `@x + 2`: an `Arith` whose left operand is the variable,
/// typed as `SELECT @x + 2` is. On a `varchar`, `+=` is the concatenation.
#[test]
fn a_compound_assignment_is_the_arithmetic_on_the_variable() {
    let bound = binds("DECLARE @x int = 1; SET @x += 2;");
    let (name, value) = set_variable(&bound[1]);
    assert_eq!(name, "@x");
    let BoundExprKind::Arith { left, .. } = &value.kind else {
        panic!("expected an Arith, got {:?}", value.kind);
    };
    assert!(
        matches!(&left.kind, BoundExprKind::Variable { name } if name == "@x"),
        "got {:?}",
        left.kind
    );
    assert_eq!(value.ty.ty, SqlType::Int);

    let bound = binds("DECLARE @s varchar(10) = 'ab'; SET @s += 'cd';");
    let (_, value) = set_variable(&bound[1]);
    // `varchar(10) + varchar(2)` is a `varchar(12)`, converted back to the declared
    // `varchar(10)`.
    let (ty, inner) = convert_of(value);
    assert_eq!(ty.ty, SqlType::VarChar(Len::Fixed(10)));
    assert!(matches!(&inner.kind, BoundExprKind::Arith { .. }));

    // The other operators desugar the same way.
    for op in ["-=", "*=", "/=", "%=", "&=", "|=", "^="] {
        let text = format!("DECLARE @x int = 1; SET @x {op} 2;");
        let bound = binds(&text);
        let (_, value) = set_variable(&bound[1]);
        assert!(
            matches!(&value.kind, BoundExprKind::Arith { .. }),
            "{text}: got {:?}",
            value.kind
        );
    }
}

// ---------------------------------------------------------------------------------------
// SELECT @x = e
// ---------------------------------------------------------------------------------------

/// `SELECT @x = 1` binds to a `SetVariable`, not to a query: the bound statement has no
/// result set and no output column.
#[test]
fn select_assignment_produces_no_result_column() {
    let bound = binds("DECLARE @x int; SELECT @x = 1;");
    assert_eq!(bound.len(), 2);
    assert!(
        !matches!(&bound[1], BoundStatement::Query(_)),
        "an assignment is not a query, got {:?}",
        bound[1]
    );
    let (name, value) = set_variable(&bound[1]);
    assert_eq!(name, "@x");
    assert!(matches!(&value.kind, BoundExprKind::Literal(Value::I32(1))));

    // The value converts to the declared type, as with SET.
    let bound = binds("DECLARE @s varchar(5); SELECT @s = 'abcdefg';");
    let (_, value) = set_variable(&bound[1]);
    let (ty, _) = convert_of(value);
    assert_eq!(ty.ty, SqlType::VarChar(Len::Fixed(5)));

    // DISTINCT over the one row changes nothing and is accepted.
    let bound = binds("DECLARE @x int; SELECT DISTINCT @x = 1;");
    set_variable(&bound[1]);

    let error = err("DECLARE @x int; SELECT @x = NEWID();");
    assert_eq!(error.number, 206);
}

/// `SELECT @x = 1, @y = @x` binds to a `Block` of two `SetVariable`, in written order,
/// the second reading the variable the first assigns.
#[test]
fn several_assignments_bind_to_a_block_in_written_order() {
    let bound = binds("DECLARE @x int, @y int; SELECT @x = 1, @y = @x;");
    let BoundStatement::Block(statements) = &bound[1] else {
        panic!("expected a Block, got {:?}", bound[1]);
    };
    assert_eq!(statements.len(), 2);
    let (first, _) = set_variable(&statements[0]);
    let (second, value) = set_variable(&statements[1]);
    assert_eq!(first, "@x");
    assert_eq!(second, "@y");
    assert!(matches!(&value.kind, BoundExprKind::Variable { name } if name == "@x"));
}

/// `SELECT @x = 1, 2` and `SELECT 2, @x = 1` are 141, severity 15, state 1, on the line
/// of the statement; with `@z` undeclared, `SELECT @z = 1, 2` is 137 instead: the 137
/// comes first.
#[test]
fn mixing_assignment_and_column_is_141() {
    let error = err("DECLARE @x int;\nSELECT @x = 1, 2;");
    assert_eq!((error.number, error.severity, error.state), (141, 15, 1));
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        SqlError::assignment_mixed_with_data_retrieval().message
    );

    let error = err("DECLARE @x int; SELECT 2, @x = 1;");
    assert_eq!(error.number, 141);
    let error = err("DECLARE @x int; SELECT @x = 1, *;");
    assert_eq!(error.number, 141);

    let error = err("SELECT @z = 1, 2;");
    assert_eq!((error.number, error.state), (137, 1));
    let error = err("DECLARE @x int; SELECT @x = 1, @z = 2;");
    assert_eq!((error.number, error.state), (137, 1));
    assert!(error.message.contains("\"@z\""), "{}", error.message);
}

/// `SELECT @x = a FROM t` is not bound yet: the internal error 50000 names the `FROM`.
/// The 137 of an undeclared target comes before it, as does the 141 of a mixed list.
/// `WHERE`, `TOP` and `ORDER BY` are deferred the same way.
#[test]
fn select_assign_with_from_is_deferred() {
    let error = err("DECLARE @x int; SELECT @x = a FROM t;");
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("FROM"), "{}", error.message);
    assert!(
        error.message.contains("not implemented"),
        "{}",
        error.message
    );

    let error = err("SELECT @x = a FROM t;");
    assert_eq!((error.number, error.state), (137, 1));

    let error = err("DECLARE @x int; SELECT @x = a, b FROM t;");
    assert_eq!(error.number, 141);

    for (text, clause) in [
        ("DECLARE @x int; SELECT @x = 1 WHERE 1 = 0;", "WHERE"),
        ("DECLARE @x int; SELECT TOP (0) @x = 1;", "TOP"),
        ("DECLARE @x int; SELECT @x = 1 ORDER BY 1;", "ORDER BY"),
    ] {
        let error = err(text);
        assert_eq!(error.number, 50000, "{text}");
        assert!(error.message.contains(clause), "{text}: {}", error.message);
    }
}
