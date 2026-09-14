//! The shape of the relational, DML and control-of-flow plan, built by hand, and the form
//! each unbound statement names.
//!
//! This file is in two halves: the first builds a node of each variant and reads its
//! `schema()`, the second sends a form the binder does not bind yet through `bind` and
//! reads the form named in the internal error that comes back.
//!
//! The `use` below is the contract `executor`, `planner` and `session` compile against, one
//! name per public type.

use vauban_binder::{
    AggregateCall, BindContext, BoundDeclaration, BoundExpr, BoundExprKind, BoundProjection,
    BoundStatement, CatalogView, ColumnBinding, DeletePlan, InsertPlan, JoinKind, LockHints,
    LogicalPlan, NoVariables, OutputColumn, OutputSchema, ResolvedTable, ResolvedTableKind,
    SessionOptions, SetOpKind, SortKey, TxnStatement, UpdatePlan, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlResult;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::{Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind, register_builtins};
use vauban_types::{SqlType, TypeInfo, Value};

/// A type of the right shape for the tests: `int`, nullable.
fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

/// A bound literal, the simplest expression a node can hold.
fn literal() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(1)),
        ty: int(),
        line: 1,
    }
}

/// A schema of one column of the given name.
fn schema_of(names: &[&str]) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .map(|name| OutputColumn {
                name: (*name).to_owned(),
                ty: int(),
            })
            .collect(),
    }
}

/// The names a plan publishes, in order.
fn names(plan: &LogicalPlan) -> Vec<String> {
    plan.schema()
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect()
}

/// A `Project` of one constant column named `name`, used as the input of the nodes below.
fn source(name: &str) -> LogicalPlan {
    LogicalPlan::Project {
        input: Box::new(LogicalPlan::OneRow),
        exprs: vec![BoundProjection {
            expr: literal(),
            name: name.to_owned(),
        }],
        schema: schema_of(&[name]),
    }
}

fn test_return_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, true))
}

fn test_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::Null)
}

/// An aggregate definition local to this test: `AggregateCall` holds a `&'static
/// FunctionDef`, and the shape of the node is what is under test, not the function.
static TEST_AGGREGATE: FunctionDef = FunctionDef {
    name: "TEST_COUNT",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: test_return_type,
    eval: test_eval,
    aggregate: None,
};

/// One column binding, for the DML plans and the `Scan` of the hints.
fn binding(name: &str, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(1),
        index,
        name: name.to_owned(),
        ty: int(),
    }
}

// ---------------------------------------------------------------------------------------
// The shape of the plan
// ---------------------------------------------------------------------------------------

/// Each relational variant answers the `schema()` its rustdoc promises:
/// `Join` the columns of its left input then those of its right, `Aggregate` the `GROUP BY`
/// keys then the aggregates, `SetOp` and `Subquery` their own, and `Sort` and `Distinct`
/// the schema of the input they do not reshape.
#[test]
fn every_relational_plan_variant_reports_its_schema() {
    let left = source("l");
    let right = source("r");
    let concatenated = OutputSchema {
        columns: left
            .schema()
            .columns
            .iter()
            .chain(right.schema().columns.iter())
            .cloned()
            .collect(),
    };
    let join = LogicalPlan::Join {
        left: Box::new(left),
        right: Box::new(right),
        kind: JoinKind::Inner,
        on: Some(BoundExpr {
            kind: BoundExprKind::Compare {
                op: vauban_binder::CompareOp::Eq,
                left: Box::new(literal()),
                right: Box::new(literal()),
            },
            ty: TypeInfo::new(SqlType::Bit, false),
            line: 1,
        }),
        schema: concatenated,
    };
    assert_eq!(names(&join), vec!["l".to_owned(), "r".to_owned()]);

    // The keys of the `GROUP BY` come first and the aggregates after, which is what the
    // projection above an `Aggregate` indexes (`bound/mod.rs`).
    let aggregate = LogicalPlan::Aggregate {
        input: Box::new(source("k")),
        group_by: vec![literal()],
        aggregates: vec![AggregateCall {
            def: &TEST_AGGREGATE,
            arg: None,
            distinct: false,
        }],
        schema: schema_of(&["k", "n"]),
    };
    let LogicalPlan::Aggregate {
        group_by,
        aggregates,
        ..
    } = &aggregate
    else {
        unreachable!("the node above is an Aggregate")
    };
    assert_eq!(group_by.len(), 1);
    assert_eq!(aggregates.len(), 1);
    // `arg: None` is a call written with a star, `COUNT(*)` or `COUNT_BIG(*)`: no
    // expression under it.
    assert!(aggregates[0].arg.is_none());
    assert_eq!(
        aggregate.schema().columns.len(),
        group_by.len() + aggregates.len()
    );
    assert_eq!(names(&aggregate), vec!["k".to_owned(), "n".to_owned()]);

    // `Sort` and `Distinct` reshape nothing, so both answer the schema of their input.
    let sort = LogicalPlan::Sort {
        input: Box::new(source("s")),
        keys: vec![SortKey {
            expr: literal(),
            desc: true,
            collation: None,
        }],
    };
    assert_eq!(names(&sort), vec!["s".to_owned()]);
    let distinct = LogicalPlan::Distinct(Box::new(source("d")));
    assert_eq!(names(&distinct), vec!["d".to_owned()]);
    // Stacked, they still delegate down to the `Project` at the bottom.
    let stacked = LogicalPlan::Distinct(Box::new(sort));
    assert_eq!(names(&stacked), vec!["s".to_owned()]);

    let set_op = LogicalPlan::SetOp {
        op: SetOpKind::Union,
        all: true,
        left: Box::new(source("u")),
        right: Box::new(source("other")),
        schema: schema_of(&["u"]),
    };
    // The name comes from the left operand, not from the right one.
    assert_eq!(names(&set_op), vec!["u".to_owned()]);

    let derived = LogicalPlan::Subquery {
        input: Box::new(source("inner")),
        alias: "d".to_owned(),
        schema: schema_of(&["renamed"]),
    };
    assert_eq!(names(&derived), vec!["renamed".to_owned()]);

    // The `hints` field is on the `Scan`, and its default is "no hint".
    let scan = LogicalPlan::Scan {
        table: TableId(1),
        columns: vec![binding("c", 0)],
        alias: "t".to_owned(),
        schema: schema_of(&["c"]),
        hints: LockHints::default(),
    };
    let LogicalPlan::Scan { hints, .. } = &scan else {
        unreachable!("the node above is a Scan")
    };
    assert!(!hints.nolock);
    assert_eq!(*hints, LockHints::default());
    assert_eq!(names(&scan), vec!["c".to_owned()]);
}

/// The three relational expression forms are predicates, except the scalar subquery, which
/// is a value: `WHERE (SELECT 1)` answers 4145 as `WHERE 1` does (`bound/mod.rs`).
#[test]
fn a_subquery_predicate_is_a_predicate_and_a_scalar_one_is_not() {
    let plan = || Box::new(source("c"));
    let predicate = |kind| BoundExpr {
        kind,
        ty: TypeInfo::new(SqlType::Bit, false),
        line: 1,
    };
    assert!(predicate(BoundExprKind::Exists(plan())).is_predicate());
    assert!(
        predicate(BoundExprKind::InSubquery {
            expr: Box::new(literal()),
            plan: plan(),
            negated: true,
        })
        .is_predicate()
    );
    assert!(!predicate(BoundExprKind::ScalarSubquery(plan())).is_predicate());
}

/// One value of each statement variant builds and matches by pattern, which is what
/// `executor` and `session` do with them.
#[test]
fn every_statement_variant_exists() {
    let values = vec![
        BoundStatement::Insert(InsertPlan {
            table: TableId(1),
            columns: vec![binding("c", 0)],
            source: Box::new(source("c")),
        }),
        BoundStatement::Update(UpdatePlan {
            table: TableId(1),
            input: Box::new(source("c")),
            assignments: vec![(binding("c", 0), literal())],
        }),
        BoundStatement::Delete(DeletePlan {
            table: TableId(1),
            input: Box::new(source("c")),
        }),
        BoundStatement::SetVariable {
            name: "@x".to_owned(),
            value: literal(),
        },
        BoundStatement::SelectAssign {
            input: Box::new(source("c")),
            assignments: vec![("@x".to_owned(), literal())],
        },
        BoundStatement::Declare(vec![BoundDeclaration {
            name: "@x".to_owned(),
            ty: int(),
            value: Some(literal()),
        }]),
        BoundStatement::If {
            condition: literal(),
            then_: Box::new(BoundStatement::Break),
            else_: Some(Box::new(BoundStatement::Continue)),
        },
        BoundStatement::While {
            condition: literal(),
            body: Box::new(BoundStatement::Block(vec![BoundStatement::Break])),
        },
        BoundStatement::Block(Vec::new()),
        BoundStatement::Break,
        BoundStatement::Continue,
        BoundStatement::Return(None),
        BoundStatement::Print(literal()),
        BoundStatement::Transaction(TxnStatement::Begin {
            name: Some("t".to_owned()),
            mark: None,
        }),
    ];
    assert_eq!(values.len(), 14, "one value per variant");
    let mut matched = 0;
    for value in &values {
        matched += match value {
            BoundStatement::Insert(plan) => usize::from(plan.columns.len() == 1),
            BoundStatement::Update(plan) => usize::from(plan.assignments.len() == 1),
            BoundStatement::Delete(plan) => usize::from(plan.table == TableId(1)),
            BoundStatement::SetVariable { name, .. } => usize::from(name == "@x"),
            BoundStatement::SelectAssign { assignments, .. } => usize::from(assignments.len() == 1),
            BoundStatement::Declare(declarations) => usize::from(declarations.len() == 1),
            BoundStatement::If { else_, .. } => usize::from(else_.is_some()),
            BoundStatement::While { .. }
            | BoundStatement::Block(_)
            | BoundStatement::Break
            | BoundStatement::Continue
            | BoundStatement::Return(_)
            | BoundStatement::Print(_) => 1,
            BoundStatement::Transaction(TxnStatement::Begin { name, .. }) => {
                usize::from(name.as_deref() == Some("t"))
            }
            BoundStatement::Transaction(_) => 0,
            BoundStatement::Query(_) | BoundStatement::Ddl(_) | BoundStatement::Use { .. } => 0,
        };
    }
    assert_eq!(matched, values.len(), "each value matched its own variant");

    // The three other transaction statements build too.
    let others = [
        TxnStatement::Commit { name: None },
        TxnStatement::Rollback {
            name: Some("s".to_owned()),
        },
        TxnStatement::Save {
            name: "s".to_owned(),
        },
    ];
    assert_eq!(others.len(), 3);
}

// ---------------------------------------------------------------------------------------
// The form an unbound statement names
// ---------------------------------------------------------------------------------------

/// A catalogue holding two one-column tables, `dbo.a` and `dbo.b`, so that a `FROM` over
/// both resolves and reaches the dispatch rather than the refusal of a missing catalogue.
struct TwoTables;

impl CatalogView for TwoTables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        let object = match name.name.value.as_str() {
            "a" => 1,
            "b" => 2,
            _ => return None,
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns: vec![binding("c", 0)],
            kind: ResolvedTableKind::Table,
        })
    }
}

/// The first binding error of `text`, bound against the two tables above.
fn error_of(text: &str) -> vauban_errors::SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let catalog = TwoTables;
    let variables = NoVariables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &variables,
        options: SessionOptions::default(),
    };
    for statement in &batch.statements {
        if let Err(error) = bind(statement, &ctx) {
            return error;
        }
    }
    unreachable!("{text} should not bind yet")
}

/// A form the binder does not bind yet answers the internal error 50000 naming that form
/// and saying it is not implemented, one shape per stubbed file. A file that binds its
/// form leaves the list: `txn_stmt.rs` binds the transaction statements and reads their
/// bound shape in `tests/bind_txn_stmt.rs`, `variables.rs` binds `DECLARE` and `SET @x`
/// and reads theirs in `tests/bind_variables.rs`, `join.rs` binds the `FROM` of more than
/// one source and reads its shape in `tests/bind_join.rs`, `insert.rs` binds `INSERT` and
/// reads its shape in `tests/bind_insert.rs`.
///
/// The number is the internal 50000 of a bug, not a user-facing number: the 209, 8120,
/// 205, 213, 137, 116 and 145 of those forms are raised once they are bound.
#[test]
fn an_unimplemented_form_names_itself() {
    let shapes: &[(&str, &str)] = &[
        // `SELECT 1 FROM a, b` and `SELECT 1 FROM a JOIN b ON 1 = 1` are bound
        // (`tests/bind_join.rs`).
        ("SELECT 1 FROM a GROUP BY c", "GROUP BY and HAVING"),
        ("SELECT 1 FROM a HAVING 1 = 1", "GROUP BY and HAVING"),
        ("SELECT COUNT(*) FROM a", "GROUP BY and HAVING"),
        ("SELECT SUM(c) FROM a", "GROUP BY and HAVING"),
        // `SELECT 1 FROM a ORDER BY 1` and `SELECT DISTINCT c FROM a` are bound
        // (`tests/bind_sort.rs`).
        ("SELECT 1 WHERE EXISTS (SELECT 1)", "EXISTS"),
        ("SELECT 1 WHERE 1 IN (SELECT 1)", "IN (SELECT …)"),
        (
            "SELECT 1 FROM (SELECT 1 AS c) AS d",
            "a derived table in FROM",
        ),
        ("SELECT 1 UNION SELECT 2", "UNION, EXCEPT and INTERSECT"),
        ("SELECT 1 EXCEPT SELECT 2", "UNION, EXCEPT and INTERSECT"),
        // `INSERT INTO a (c) VALUES (1)` is bound (`tests/bind_insert.rs`).
        ("UPDATE a SET c = 1", "UPDATE"),
        ("DELETE FROM a", "DELETE"),
        ("IF 1 = 1 SELECT 1", "IF"),
        ("WHILE 1 = 1 BREAK", "WHILE"),
        ("PRINT 'a'", "PRINT"),
        ("ALTER TABLE a ADD d int", "ALTER TABLE"),
        ("ALTER DATABASE d SET READ_ONLY", "ALTER DATABASE"),
        ("SELECT 1 INTO b FROM a", "SELECT … INTO"),
        ("TRUNCATE TABLE a", "TRUNCATE TABLE"),
    ];
    for (text, form) in shapes {
        let error = error_of(text);
        assert_eq!(error.number, 50000, "{text}: {}", error.message);
        assert!(
            error.message.contains(form) && error.message.contains("is not implemented yet"),
            "{text} should name {form}: {}",
            error.message
        );
    }
    // `SELECT @x = 1` is not in that table: the undeclared variable answers **137** before
    // the assignment is looked at, which is the order `query::assignment` keeps and
    // `statement.rs` pins (`SELECT\n1\n,\n@x\n=\n1;`, 137 on line 5).
    assert_eq!(error_of("SELECT @x = 1").number, 137);

    // Counter-proof: a statement the binder binds answers no such error, so the assertions
    // above check the dispatch and not the absence of a catalogue.
    let text = "SELECT c FROM a";
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let catalog = TwoTables;
    let variables = NoVariables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &variables,
        options: SessionOptions::default(),
    };
    assert!(bind(&batch.statements[0], &ctx).is_ok());
}

/// A select list holding an aggregate reaches `aggregate.rs` with no `GROUP BY` written,
/// and a select list holding a scalar call does not.
///
/// The first half is the routing: the group of `SELECT COUNT(*) FROM a` is the whole
/// table, so the clause cannot be what sends the query there. The four spellings below
/// are the star, a named aggregate, an aggregate under an operator and one under a `CASE`
/// — the walk of `query::holds_an_aggregate` goes through both.
///
/// The second half is the counter-proof: without it the test would pass on a routing that
/// sent each function call to `aggregate.rs`. `ABS(c)` and `ISNULL(c, 1)` bind, and
/// `SUM(c)` written in a `WHERE` rather than in the select list stays with `call.rs`,
/// whose refusal names the aggregate calls from another site.
#[test]
fn an_aggregate_in_a_select_list_reaches_aggregate_rs() {
    let aggregated = [
        "SELECT COUNT(*) FROM a",
        "SELECT SUM(c) FROM a",
        "SELECT SUM(c) + 1 FROM a",
        "SELECT CASE WHEN 1 = 1 THEN MAX(c) ELSE 0 END FROM a",
    ];
    for text in aggregated {
        let error = error_of(text);
        assert_eq!(error.number, 50000, "{text}: {}", error.message);
        assert!(
            error.message.contains("GROUP BY and HAVING"),
            "{text} should reach aggregate.rs: {}",
            error.message
        );
    }

    let scalar = ["SELECT ABS(c) FROM a", "SELECT ISNULL(c, 1) FROM a"];
    for text in scalar {
        register_builtins();
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        let catalog = TwoTables;
        let variables = NoVariables;
        let ctx = BindContext {
            text,
            catalog: Some(&catalog),
            database: "master",
            default_schema: "dbo",
            variables: &variables,
            options: SessionOptions::default(),
        };
        assert!(
            bind(&batch.statements[0], &ctx).is_ok(),
            "{text} binds: a scalar call is not routed to aggregate.rs"
        );
    }

    let elsewhere = error_of("SELECT c FROM a WHERE SUM(c) = 1");
    assert_eq!(elsewhere.number, 50000, "{}", elsewhere.message);
    assert!(
        !elsewhere.message.contains("GROUP BY and HAVING"),
        "an aggregate in a WHERE stays with call.rs: {}",
        elsewhere.message
    );
    assert!(
        elsewhere
            .message
            .contains("aggregate calls of a select list"),
        "call.rs names the aggregate calls too: {}",
        elsewhere.message
    );
}
