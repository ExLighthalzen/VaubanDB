//! The shape of the bound plan, built by hand, without ever calling the binder.
//!
//! This file is the contract the `executor` and the `session` modules compile against: the
//! `use` below must keep compiling as written, one name per public type of the module. The
//! entry point `bind` is exercised by `tests/bind_select.rs`.

use vauban_binder::{
    BindContext, BoundCaseArm, BoundExpr, BoundExprKind, BoundProjection, BoundStatement, BoundTop,
    CatalogView, CompareOp, LogicalOp, LogicalPlan, NoVariables, OutputColumn, OutputSchema,
    SessionOptions, VariableScope,
};
use vauban_errors::SqlResult;
use vauban_sysfn::{Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind};
use vauban_types::{BinaryOp, SqlType, TypeInfo, Value};

/// A type of the right shape for the tests: `int`, nullable.
fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

/// A bound expression of the given kind, on line 1.
fn expr(kind: BoundExprKind) -> BoundExpr {
    BoundExpr {
        kind,
        ty: int(),
        line: 1,
    }
}

/// A bound literal, the simplest non-predicate expression.
fn literal() -> BoundExpr {
    expr(BoundExprKind::Literal(Value::I32(1)))
}

fn test_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, true))
}

fn test_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::Null)
}

/// A function definition local to this test: the registry is empty until a server calls
/// `register_builtins`, and the shape of the node is what is under test, not the function.
static TEST_FUNCTION: FunctionDef = FunctionDef {
    name: "TEST_FUNCTION",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: test_return_type,
    eval: test_eval,
    aggregate: None,
};

/// A catalogue that answers nothing, to check that `BindContext` accepts one at all.
struct EmptyCatalog;

impl CatalogView for EmptyCatalog {}

/// A scope holding a single `@x`, to check that another crate can plug its own.
struct OneVariable;

impl VariableScope for OneVariable {
    fn type_of(&self, name: &str) -> Option<TypeInfo> {
        (name == "@x").then(int)
    }
}

#[test]
fn session_options_defaults() {
    let options = SessionOptions::default();
    assert!(options.ansi_nulls);
    assert!(options.ansi_warnings);
    assert!(options.arithabort);
    assert!(options.concat_null_yields_null);
    assert!(!options.numeric_roundabort);
}

#[test]
fn bind_context_scalar_has_no_catalog() {
    let ctx = BindContext::scalar("SELECT 1", SessionOptions::default());
    assert!(ctx.catalog.is_none());
    assert_eq!(ctx.database, "master");
    assert_eq!(ctx.default_schema, "dbo");
    assert!(ctx.variables.type_of("@x").is_none());
}

#[test]
fn bind_context_accepts_a_catalog_and_a_scope() {
    // `session` builds its context this way; this checks that the types fit.
    let catalog = EmptyCatalog;
    let ctx = BindContext {
        text: "SELECT 1",
        catalog: Some(&catalog),
        database: "vauban",
        default_schema: "dbo",
        variables: &OneVariable,
        options: SessionOptions::default(),
    };
    assert!(ctx.catalog.is_some());
    assert_eq!(ctx.variables.type_of("@x"), Some(int()));
    assert!(ctx.variables.type_of("@y").is_none());
    // The empty scope of `BindContext::scalar` is public too.
    assert!(NoVariables.type_of("@x").is_none());
}

#[test]
fn plan_schema_is_delegated() {
    assert!(LogicalPlan::OneRow.schema().columns.is_empty());

    let project = LogicalPlan::Project {
        input: Box::new(LogicalPlan::OneRow),
        exprs: vec![BoundProjection {
            expr: literal(),
            name: String::new(),
        }],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: String::new(),
                ty: int(),
            }],
        },
    };
    assert_eq!(project.schema().columns.len(), 1);
    assert_eq!(project.schema().columns[0].ty, int());

    let filter = LogicalPlan::Filter {
        input: Box::new(project),
        predicate: expr(BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(literal()),
            right: Box::new(literal()),
        }),
    };
    assert_eq!(filter.schema().columns.len(), 1);

    let limit = LogicalPlan::Limit {
        input: Box::new(filter),
        top: BoundTop {
            expr: literal(),
            percent: false,
            with_ties: false,
        },
    };
    assert_eq!(limit.schema().columns.len(), 1);

    // A `Values` node carries its own schema.
    let values = LogicalPlan::Values {
        rows: vec![vec![literal()]],
        schema: OutputSchema {
            columns: vec![OutputColumn {
                name: "c".into(),
                ty: int(),
            }],
        },
    };
    assert_eq!(values.schema().columns[0].name, "c");

    // A bound statement wraps the plan and nothing else.
    // The pattern is not irrefutable: `BoundStatement` has other variants than `Query`.
    let BoundStatement::Query(plan) = BoundStatement::Query(Box::new(limit)) else {
        panic!("a `Query` was just built");
    };
    assert_eq!(plan.schema().columns.len(), 1);
}

#[test]
fn is_predicate_lists_the_boolean_variants() {
    let predicates = vec![
        BoundExprKind::Compare {
            op: CompareOp::Ne,
            left: Box::new(literal()),
            right: Box::new(literal()),
        },
        BoundExprKind::Logical {
            op: LogicalOp::And,
            left: Box::new(literal()),
            right: Box::new(literal()),
        },
        BoundExprKind::Not(Box::new(literal())),
        BoundExprKind::IsNull {
            expr: Box::new(literal()),
            negated: true,
        },
        BoundExprKind::In {
            expr: Box::new(literal()),
            list: vec![literal()],
            negated: false,
        },
        BoundExprKind::Like {
            expr: Box::new(literal()),
            pattern: Box::new(literal()),
            escape: Some(Box::new(literal())),
            negated: false,
        },
    ];
    for kind in predicates {
        let bound = expr(kind);
        assert!(bound.is_predicate(), "expected a predicate: {bound:?}");
    }

    let values = vec![
        BoundExprKind::Literal(Value::Null),
        BoundExprKind::Variable { name: "@x".into() },
        BoundExprKind::Arith {
            op: BinaryOp::Add,
            left: Box::new(literal()),
            right: Box::new(literal()),
        },
        BoundExprKind::Negate(Box::new(literal())),
        BoundExprKind::BitNot(Box::new(literal())),
        BoundExprKind::Case {
            operand: Some(Box::new(literal())),
            arms: vec![BoundCaseArm {
                when: literal(),
                then: literal(),
            }],
            else_: Some(Box::new(literal())),
        },
        BoundExprKind::Convert {
            expr: Box::new(literal()),
            style: Some(126),
            try_: true,
        },
        BoundExprKind::Function {
            def: &TEST_FUNCTION,
            args: vec![literal()],
        },
        BoundExprKind::Collate {
            expr: Box::new(literal()),
        },
    ];
    for kind in values {
        let bound = expr(kind);
        assert!(!bound.is_predicate(), "expected a value: {bound:?}");
    }
}

/// Each `#[allow(dead_code)]` of the crate carries a `reason`, and no such allowance is
/// set for the whole crate.
///
/// `seen` is allowed to be zero, and the loop keeps guarding the allowances a later
/// change might add.
#[test]
fn dead_code_allowances_carry_a_reason() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut seen = 0;
    for path in rust_files(&root) {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for line in text.lines() {
            assert!(
                !line.contains("#![allow(dead_code)]"),
                "crate-wide dead_code allowance in {}",
                path.display()
            );
            if line.contains("allow(dead_code)") {
                seen += 1;
                let trimmed = line.trim_end();
                assert!(
                    trimmed.contains("reason = \""),
                    "dead_code allowance without a reason in {}: {trimmed}",
                    path.display()
                );
            }
        }
    }
    // Informative.
    let _ = seen;
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}
