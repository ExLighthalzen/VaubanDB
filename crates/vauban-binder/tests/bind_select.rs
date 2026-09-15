//! The plan and the output schema of a `SELECT` without `FROM`.
//!
//! Most tests start from SQL text, so that they read like the batches a client sends. A
//! few clauses (`INTO`, `GROUP BY`, `UNION`) are built by hand, with the spans of the text
//! they stand for so that a message quoting the source quotes the right token; so are the
//! statements other than `SELECT`.
//!
//! The bound nodes derive no `PartialEq` (`FunctionDef` has none): a plan is checked by
//! pattern matching, a schema by the `(name, type, nullable)` triples of [`cols`].
//!
//! The `FROM` of the table-argument tests below is parsed from text: what the binder does
//! with the **arguments** glued to a table name is observable through `bind` even when the
//! clause itself is refused for want of a catalogue — 215 and the errors of the arguments
//! come out first, and a lone hint word falls through to the same refusal as `WITH (…)`.

use vauban_binder::{
    BindContext, BoundExprKind, BoundStatement, CompareOp, LogicalPlan, SessionOptions, bind,
};
use vauban_errors::SqlError;
use vauban_parser::{
    AliasStyle, Expr, Ident, Literal, ObjectName, ParseOptions, QueryBody, QuerySpec, SelectItem,
    SelectStatement, SetOp, Span, Statement, TableHint, TableRef, WaitforKind, WaitforStatement,
    parse_batch,
};
use vauban_sysfn::register_builtins;
use vauban_types::SqlType;

/// Binds the first statement of `text` and returns its plan.
fn plan(text: &str) -> LogicalPlan {
    match bound(text) {
        BoundStatement::Query(plan) => *plan,
        // The texts this file hands over are `SELECT`s, so reaching here is a failure of
        // the test, not of the binder.
        other => panic!("expected a bound query, got {other:?}"),
    }
}

/// Binds the first statement of `text`, whatever it binds to.
fn bound(text: &str) -> BoundStatement {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(&batch.statements[0], &ctx).expect("the statement binds")
}

/// The error binding the first statement of `text` raises.
fn err(text: &str) -> SqlError {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(&batch.statements[0], &ctx).expect_err("the statement does not bind")
}

/// The error binding a hand-built statement raises.
fn err_of(stmt: &Statement) -> SqlError {
    let ctx = BindContext::scalar("", SessionOptions::default());
    bind(stmt, &ctx).expect_err("the statement does not bind")
}

/// The `(name, type, nullable)` of every column of the result set of `text`.
fn cols(text: &str) -> Vec<(String, SqlType, bool)> {
    plan(text)
        .schema()
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.ty.ty, column.ty.nullable))
        .collect()
}

/// A `varchar(n)`, the type of a string literal of `n` characters.
fn varchar(n: u16) -> SqlType {
    SqlType::VarChar(vauban_types::Len::Fixed(n))
}

/// An `nvarchar(n)`, the type of an `N'…'` literal of `n` characters.
fn nvarchar(n: u16) -> SqlType {
    SqlType::NVarChar(vauban_types::Len::Fixed(n))
}

/// A bare identifier, as the parser stores one.
fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

/// A one-part object name.
fn object(name: &str) -> ObjectName {
    ObjectName {
        server: None,
        database: None,
        schema: None,
        name: ident(name),
        span: Span::EMPTY,
    }
}

/// `SELECT 1` as a specification, to be amended clause by clause.
fn spec_of_select_one() -> QuerySpec {
    QuerySpec {
        distinct: false,
        top: None,
        items: vec![SelectItem::Expr {
            expr: Expr::Literal(Literal::Integer("1".into()), Span::EMPTY),
            alias: None,
            alias_style: AliasStyle::As,
        }],
        into: None,
        from: Vec::new(),
        where_: None,
        group_by: Vec::new(),
        having: None,
        span: Span::EMPTY,
    }
}

/// A `SELECT` statement built around `spec`.
fn select_of(spec: QuerySpec) -> Statement {
    Statement::Select(Box::new(SelectStatement {
        with: None,
        body: QueryBody::Select(Box::new(spec)),
        order_by: Vec::new(),
        offset_fetch: None,
        for_clause: None,
        span: Span::EMPTY,
    }))
}

/// The error binding `stmt` raises, with `text` as the text of the batch: a message that
/// quotes the source (4145 and its `near '…'`) needs the two to agree.
fn err_in(stmt: &Statement, text: &str) -> SqlError {
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(stmt, &ctx).expect_err("the statement does not bind")
}

/// The plan of `stmt`, bound with `text` as the text of the batch.
fn plan_of(stmt: &Statement) -> LogicalPlan {
    let ctx = BindContext::scalar("", SessionOptions::default());
    match bind(stmt, &ctx).expect("the statement binds") {
        BoundStatement::Query(plan) => *plan,
        // As in `plan`.
        other => panic!("expected a bound query, got {other:?}"),
    }
}

/// A span of `len` bytes at `offset` on the first line.
fn span(offset: u32, len: u32) -> Span {
    Span {
        line: 1,
        column: offset + 1,
        offset,
        len,
    }
}

/// `SELECT 1 WHERE <condition>`, built by hand.
fn select_where(condition: Expr) -> Statement {
    let mut spec = spec_of_select_one();
    spec.where_ = Some(condition);
    select_of(spec)
}

#[test]
fn select_one_literal() {
    let plan = plan("SELECT 1");
    let LogicalPlan::Project { input, exprs, .. } = &plan else {
        panic!("expected a Project, got {plan:?}");
    };
    assert!(matches!(**input, LogicalPlan::OneRow));
    assert_eq!(exprs.len(), 1);
    assert_eq!(cols("SELECT 1"), [(String::new(), SqlType::Int, false)]);
}

/// `SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')` answers four unnamed
/// columns of types `int`, `int`, `int`, `varchar(1)` — the types
/// `sys.dm_exec_describe_first_result_set` reports on SQL Server.
///
/// The first three are **nullable**: `INTNTYPE` with `fNullable = 1` on the wire (an
/// arithmetic operator, `LEN` and a `CAST` are nullable whatever they receive); `ISNULL`
/// with a non-null literal is not.
#[test]
fn a_select_list_of_expressions_publishes_unnamed_typed_columns() {
    register_builtins();
    assert_eq!(
        cols("SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')"),
        [
            (String::new(), SqlType::Int, true),
            (String::new(), SqlType::Int, true),
            (String::new(), SqlType::Int, true),
            (String::new(), varchar(1), false),
        ]
    );
}

#[test]
fn select_literals_publishes_the_type_of_each_literal() {
    assert_eq!(
        cols("SELECT 1, 'a', N'é', 1.5, NULL"),
        [
            (String::new(), SqlType::Int, false),
            (String::new(), varchar(1), false),
            (String::new(), nvarchar(1), false),
            (
                String::new(),
                SqlType::Numeric {
                    precision: 2,
                    scale: 1
                },
                false
            ),
            (String::new(), SqlType::Int, true),
        ]
    );
}

#[test]
fn aliases_name_the_column() {
    let named = [(String::from("n"), SqlType::Int, false)];
    assert_eq!(cols("SELECT 1 AS n"), named);
    assert_eq!(cols("SELECT 1 n"), named);
    assert_eq!(cols("SELECT n = 1"), named);
    assert_eq!(cols("SELECT 1 [my col]")[0].0, "my col");
    assert_eq!(cols("SELECT 1 'lit'")[0].0, "lit");
}

#[test]
fn where_is_a_filter_under_the_project() {
    // `1 = 0`: a condition that is false on the single row.
    let condition = Expr::Binary {
        op: vauban_parser::BinaryOp::Eq,
        op_span: Span::EMPTY,
        left: Box::new(Expr::Literal(Literal::Integer("1".into()), Span::EMPTY)),
        right: Box::new(Expr::Literal(Literal::Integer("0".into()), Span::EMPTY)),
        span: Span::EMPTY,
    };
    let plan = plan_of(&select_where(condition));
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("expected a Project, got {plan:?}");
    };
    let LogicalPlan::Filter { input, predicate } = &**input else {
        panic!("expected a Filter under the Project, got {input:?}");
    };
    assert!(matches!(**input, LogicalPlan::OneRow));
    assert!(matches!(
        predicate.kind,
        BoundExprKind::Compare {
            op: CompareOp::Eq,
            ..
        }
    ));
    // The Filter changes no column: the schema is the one of the Project above it.
    assert_eq!(plan.schema().columns.len(), 1);
}

#[test]
fn where_requires_a_condition() {
    // The `1` of `SELECT 1 WHERE 1;` sits at offset 15; 4145 quotes the token that follows.
    let text = "SELECT 1 WHERE 1;";
    let condition = Expr::Literal(Literal::Integer("1".into()), span(15, 1));
    let error = err_in(&select_where(condition), text);
    assert_eq!(error.number, 4145);
    assert_eq!(
        error.message,
        "A condition is expected near ';', but the expression is not boolean."
    );
    assert_eq!(error.line, 1);
}

#[test]
fn top_is_a_limit_above_the_project() {
    let limited = plan("SELECT TOP 1 1");
    let LogicalPlan::Limit { input, top } = &limited else {
        panic!("expected a Limit, got {limited:?}");
    };
    assert!(matches!(**input, LogicalPlan::Project { .. }));
    assert!(!top.percent);
    assert!(!top.with_ties);
    // Not a bigint yet: the binder inserts the conversion the executor evaluates.
    assert!(matches!(top.expr.kind, BoundExprKind::Convert { .. }));
    assert_eq!(top.expr.ty.ty, SqlType::BigInt);

    let percent = plan("SELECT TOP (5) PERCENT 1");
    let LogicalPlan::Limit { top, .. } = &percent else {
        panic!("expected a Limit, got {percent:?}");
    };
    assert!(top.percent);
    assert_eq!(top.expr.ty.ty, SqlType::Float);
}

/// `TOP … WITH TIES` without an `ORDER BY` is 1062, as SQL Server answers.
#[test]
fn with_ties_without_order_by_is_1062() {
    for sql in [
        "SELECT TOP (1) WITH TIES 1",
        "SELECT TOP (0) WITH TIES 1",
        "SELECT TOP (1) PERCENT WITH TIES 1",
    ] {
        let error = err(sql);
        assert_eq!((error.number, error.severity, error.state), (1062, 15, 1));
    }
}

/// A row count that is not an integer is error 1060, `TOP 5.5` included. `PERCENT`
/// accepts it, as SQL Server does.
#[test]
fn a_non_integer_top_is_1060() {
    let error = err("SELECT TOP 5.5 1");
    assert_eq!((error.number, error.severity, error.state), (1060, 15, 1));
    assert_eq!(err("SELECT TOP (NULL) 1").number, 1060);
    let plan = plan("SELECT TOP 5.5 PERCENT 1");
    assert!(matches!(plan, LogicalPlan::Limit { .. }));
}

#[test]
fn distinct_is_accepted() {
    assert_eq!(cols("SELECT DISTINCT 1"), cols("SELECT 1"));
    assert!(matches!(
        plan("SELECT DISTINCT 1"),
        LogicalPlan::Project { .. }
    ));
}

/// The statement is built by hand. The error is internal (50000) and its message names
/// the missing catalogue; [`from_needs_the_catalogue`] checks the same thing on parsed
/// text.
#[test]
fn from_without_a_catalogue_is_refused() {
    let mut spec = spec_of_select_one();
    spec.from = vec![TableRef::Table {
        name: object("t"),
        alias: None,
        hints: Vec::<TableHint>::new(),
        span: Span::EMPTY,
    }];
    let error = err_of(&select_of(spec));
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("catalog"), "{}", error.message);
    assert!(error.message.contains("FROM"), "{}", error.message);
}

#[test]
fn unsupported_clauses_are_internal_errors() {
    // `ORDER BY` is bound: `SELECT 1 ORDER BY 1` answers its row and the tests of the
    // clause are in `tests/bind_sort.rs`.

    // `GROUP BY` and `HAVING` are bound: the tests of the two clauses are in
    // `tests/bind_aggregate.rs`.

    // INTO, built by hand.
    let mut spec = spec_of_select_one();
    spec.into = Some(object("t"));
    let error = err_of(&select_of(spec));
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("INTO"), "{}", error.message);

    // UNION, built by hand: a set operation over two identical specifications.
    let union = Statement::Select(Box::new(SelectStatement {
        with: None,
        body: QueryBody::SetOp {
            op: SetOp::Union,
            all: false,
            left: Box::new(QueryBody::Select(Box::new(spec_of_select_one()))),
            right: Box::new(QueryBody::Select(Box::new(spec_of_select_one()))),
            span: Span::EMPTY,
        },
        order_by: Vec::new(),
        offset_fetch: None,
        for_clause: None,
        span: Span::EMPTY,
    }));
    let error = err_of(&union);
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("UNION"), "{}", error.message);
}

#[test]
fn variable_assignment_is_137() {
    let error = err("SELECT @x = 1");
    assert_eq!(error.number, 137);
    assert_eq!(error.message, "The scalar variable \"@x\" is not declared.");
    // `SELECT @x = 1;` answers state 1, where `SELECT @x;` answers 2.
    assert_eq!(error.state, 1);
    assert_eq!(error.severity, 15);
}

/// `SELECT *` without a `FROM` is error 263, severity 16, state 1, through its
/// constructor and not an internal error.
#[test]
fn a_wildcard_without_from_is_263() {
    let error = err("SELECT *");
    assert_eq!(error.number, 263);
    assert_eq!(error.message, "No table to select from.");
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
}

/// Names are unquoted but retain their case, dots and omitted schema.
#[test]
fn a_qualified_wildcard_names_its_number() {
    for (sql, name, number) in [
        ("SELECT t.*;", "t", 107),
        ("SELECT [DbO].[T].*;", "DbO.T", 107),
        ("SELECT db.dbo.t.*;", "db.dbo.t", 107),
        ("SELECT db..t.*;", "db..t", 107),
        ("SELECT a.b.c.d.*;", "a.b.c.d", 117),
        ("SELECT [a.b].[C].[d].[E].*;", "a.b.C.d.E", 117),
        ("SELECT a..c.d.*;", "a..c.d", 117),
    ] {
        let error = err(sql);
        assert_eq!(
            (error.number, error.severity, error.state),
            (number, 15, 1),
            "{sql}"
        );
        let message = if number == 107 {
            format!("The column prefix '{name}' matches no table or alias of the query.")
        } else {
            format!("The column name '{name}' has too many prefixes; at most 3 are allowed.")
        };
        assert_eq!(error.message, message, "{sql}");
    }
}

#[test]
fn bind_dispatches_only_select() {
    // `WAITFOR` is not bound yet (V2), so the binder reports an internal error.
    let waitfor = Statement::Waitfor(Box::new(WaitforStatement {
        kind: WaitforKind::Delay,
        value: Expr::Literal(
            Literal::Str {
                value: "00:00:00".into(),
                unicode: false,
            },
            Span::EMPTY,
        ),
        span: Span::EMPTY,
    }));
    let error = err_of(&waitfor);
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("WAITFOR"), "{}", error.message);
}

#[test]
fn plan_schema_matches_project() {
    for (text, width) in [
        ("SELECT 1", 1),
        ("SELECT 1, 'a', N'é', 1.5, NULL", 5),
        ("SELECT TOP 1 1, 2", 2),
        ("SELECT DISTINCT 1 AS n", 1),
    ] {
        let plan = plan(text);
        assert_eq!(plan.schema().columns.len(), width, "{text}");
        // A Limit hands back the schema of its input, untouched.
        let projected = match &plan {
            LogicalPlan::Limit { input, .. } => input.schema(),
            other => other.schema(),
        };
        assert_eq!(projected.columns.len(), width, "{text}");
    }
    // `OneRow` has no column at all, and a `Filter` over it none either: the schema of the
    // result set is the one the `Project` computes.
    let condition = Expr::Binary {
        op: vauban_parser::BinaryOp::Eq,
        op_span: Span::EMPTY,
        left: Box::new(Expr::Literal(Literal::Integer("1".into()), Span::EMPTY)),
        right: Box::new(Expr::Literal(Literal::Integer("0".into()), Span::EMPTY)),
        span: Span::EMPTY,
    };
    let plan = plan_of(&select_where(condition));
    assert_eq!(plan.schema().columns.len(), 1);
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("expected a Project, got {plan:?}");
    };
    assert!(matches!(**input, LogicalPlan::Filter { .. }));
    assert_eq!(input.schema().columns.len(), 0);
    assert_eq!(LogicalPlan::OneRow.schema().columns.len(), 0);
}

// ---------------------------------------------------------------------------------------
// The arguments glued to a table name — a hint, or error 215.
//
// The numbers, states, names and lines below are those SQL Server answers. `is_215`
// checks each field, name included.
// ---------------------------------------------------------------------------------------

/// The text of 215 for `name`.
fn text_of_215(name: &str) -> String {
    format!(
        "Object '{name}' is not a function and takes no parameters; a table hint needs the WITH keyword."
    )
}

/// Asserts each field of error 215 on `name`.
#[track_caller]
fn is_215(error: &SqlError, name: &str) {
    assert_eq!(error.number, 215, "{}", error.message);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(error.message, text_of_215(name));
}

/// Asserts that `error` is the refusal of the `FROM` clause for want of a catalogue, and
/// **not** 215: the arguments were re-read as a hint and the reference went through as
/// `WITH (…)` does.
#[track_caller]
fn from_needs_the_catalogue(error: &SqlError) {
    assert_eq!(error.number, 50000, "{}", error.message);
    assert!(error.message.contains("FROM"), "{}", error.message);
    assert!(error.message.contains("catalog"), "{}", error.message);
    assert!(
        !error.message.contains("215"),
        "re-read as 215 instead of a hint: {}",
        error.message
    );
}

/// `FROM t (NOLOCK)` binds like `FROM t WITH (NOLOCK)`: the same refusal of the clause,
/// where `FROM t (1)` answers 215. The two forms are told apart here; the shape itself
/// (`t WITH (NOLOCK)`, alias included) is asserted on the re-read reference by the unit
/// tests of `query.rs`. The spellings below answer the row on SQL Server.
#[test]
fn a_lone_hint_word_binds_like_with() {
    register_builtins();
    from_needs_the_catalogue(&err("SELECT 1 FROM t WITH (NOLOCK)"));
    from_needs_the_catalogue(&err("SELECT 1 FROM t AS z (NOLOCK)"));
    for text in [
        "SELECT 1 FROM t (NOLOCK)",
        "SELECT 1 FROM dbo.t (NOLOCK)",
        "SELECT 1 FROM t (nolock)",
        "SELECT 1 FROM t ([NOLOCK])",
        "SELECT 1 FROM t ([nolock])",
        "SELECT 1 FROM t (\"NOLOCK\")",
        "SELECT 1 FROM t ((NOLOCK))",
        "SELECT 1 FROM t (((NOLOCK)))",
        "SELECT 1 FROM t(NOLOCK)",
        "SELECT 1 FROM t (NOLOCK) AS z",
        "SELECT 1 FROM t (NOLOCK) z",
        "SELECT 1 FROM [dbo].[t] (NOLOCK)",
    ] {
        from_needs_the_catalogue(&err(text));
    }
    is_215(&err("SELECT 1 FROM t (1)"), "t");
}

/// The words SQL Server re-reads as a hint (`query::TABLE_HINT_WORDS`), but `HOLDLOCK`,
/// which the parser of VaubanDB refuses as a reserved word (156) where SQL Server answers
/// the row — a deliberate difference.
#[test]
fn every_hint_word_is_re_read() {
    register_builtins();
    for word in [
        "FORCESCAN",
        "FORCESEEK",
        "IGNORE_CONSTRAINTS",
        "IGNORE_TRIGGERS",
        "KEEPDEFAULTS",
        "KEEPIDENTITY",
        "NOEXPAND",
        "NOLOCK",
        "NOWAIT",
        "PAGLOCK",
        "READCOMMITTED",
        "READCOMMITTEDLOCK",
        "READPAST",
        "READUNCOMMITTED",
        "REPEATABLEREAD",
        "ROWLOCK",
        "SERIALIZABLE",
        "SNAPSHOT",
        "TABLOCK",
        "TABLOCKX",
        "UPDLOCK",
        "XLOCK",
    ] {
        let text = format!("SELECT 1 FROM t ({word})");
        from_needs_the_catalogue(&err(&text));
    }
    let holdlock = parse_batch("SELECT 1 FROM t (HOLDLOCK)", &ParseOptions::default())
        .expect_err("HOLDLOCK is a reserved word for the parser");
    assert_eq!(holdlock.number, 156);

    // A word that is no longer a hint is a column: 207.
    let error = err("SELECT 1 FROM t (FASTFIRSTROW)");
    assert_eq!(error.number, 207);
    assert_eq!(error.message, "Unknown column name 'FASTFIRSTROW'.");
}

/// Anything that is not a lone hint word is 215 once the arguments bind.
#[test]
fn arguments_that_are_not_a_hint_are_215() {
    register_builtins();
    for (text, name) in [
        ("SELECT 1 FROM dbo.t (1)", "dbo.t"),
        ("SELECT 1 FROM t (1)", "t"),
        ("SELECT 1 FROM t ()", "t"),
        ("SELECT 1 FROM t ('a')", "t"),
        ("SELECT 1 FROM t ('NOLOCK')", "t"),
        ("SELECT 1 FROM t (NULL)", "t"),
        ("SELECT 1 FROM t (1, 2)", "t"),
        // The argument type-checks and does not run: 215, not 245.
        ("SELECT 1 FROM t (1 + 'a')", "t"),
        // A niladic function binds as a call, not as a hint word.
        ("SELECT 1 FROM t (CURRENT_TIMESTAMP)", "t"),
        ("SELECT 1 FROM t (1) AS z", "t"),
        ("SELECT 1 FROM t(1)", "t"),
        ("SELECT 1 FROM dbo.v (1)", "dbo.v"),
        ("SELECT 1 FROM sys.objects (1)", "sys.objects"),
    ] {
        is_215(&err(text), name);
    }
    // The catalogue supplies the severity 16 and state 1 checked above.
    assert!(text_of_215("t").starts_with("Object 't' is not a function"));
}

/// The name of 215 is printed as written, delimiters removed, case and qualification kept,
/// and not resolved.
#[test]
fn the_name_of_215_is_printed_as_written() {
    for (text, name) in [
        ("SELECT 1 FROM [dbo].[t] (1)", "dbo.t"),
        ("SELECT 1 FROM [t] (1)", "t"),
        ("SELECT 1 FROM \"t\" (1)", "t"),
        ("SELECT 1 FROM dbo.[t] (1)", "dbo.t"),
        ("SELECT 1 FROM dbo.T (1)", "dbo.T"),
        ("SELECT 1 FROM DBO.t (1)", "DBO.t"),
        (
            "SELECT 1 FROM master.dbo.spt_values (1)",
            "master.dbo.spt_values",
        ),
    ] {
        is_215(&err(text), name);
    }
}

/// 215 carries the line the **name starts on**: not the statement's, not the
/// parenthesis', not the argument's.
#[test]
fn the_line_of_215_is_the_line_of_the_name() {
    let error = err("SELECT 1\nFROM\nt\n(\n1\n)\n;");
    is_215(&error, "t");
    assert_eq!(error.line, 3);

    let error = err("SELECT 1\nFROM\ndbo\n.\nt\n(1);");
    is_215(&error, "dbo.t");
    assert_eq!(error.line, 3, "the first part of the name, not the last");

    let batch = parse_batch(
        "SELECT 1;\nSELECT 1\nFROM\nt\n(1);",
        &ParseOptions::default(),
    )
    .expect("the text parses");
    let ctx = BindContext::scalar(
        "SELECT 1;\nSELECT 1\nFROM\nt\n(1);",
        SessionOptions::default(),
    );
    let error = bind(&batch.statements[1], &ctx).expect_err("215");
    is_215(&error, "t");
    assert_eq!(error.line, 4);

    let error = err("SELECT 1\nFROM t AS a\nJOIN\nt\n(1) AS b ON a.id = b.id;");
    is_215(&error, "t");
    assert_eq!(error.line, 4);
}

/// When the arguments are not a lone hint word, they are **bound**, in order, and the
/// first that fails answers before 215 — a hint word among them included.
#[test]
fn the_arguments_are_bound_before_215() {
    register_builtins();
    for (text, column) in [
        ("SELECT 1 FROM t (x)", "x"),
        ("SELECT 1 FROM t (id)", "id"),
        ("SELECT 1 FROM t (x) AS z", "x"),
        ("SELECT 1 FROM t (x, y)", "x"),
        ("SELECT 1 FROM t (NOLOCK, 1)", "NOLOCK"),
        ("SELECT 1 FROM t (1, NOLOCK)", "NOLOCK"),
        ("SELECT 1 FROM t (NOLOCK, NOLOCK)", "NOLOCK"),
        ("SELECT 1 FROM t (NOLOCK, READPAST)", "NOLOCK"),
        ("SELECT 1 FROM t (NOLOCK + 0)", "NOLOCK"),
        ("SELECT 1 FROM t ([NO LOCK])", "NO LOCK"),
    ] {
        let error = err(text);
        assert_eq!(error.number, 207, "{text}: {}", error.message);
        assert_eq!(error.state, 1, "{text}");
        assert_eq!(
            error.message,
            format!("Unknown column name '{column}'."),
            "{text}"
        );
    }

    for (text, name) in [
        ("SELECT 1 FROM t (dbo.NOLOCK)", "dbo.NOLOCK"),
        ("SELECT 1 FROM t (t.NOLOCK)", "t.NOLOCK"),
    ] {
        let error = err(text);
        assert_eq!(error.number, 4104, "{text}: {}", error.message);
        assert_eq!(
            error.message,
            format!("The qualified name \"{name}\" matches nothing in scope."),
            "{text}"
        );
    }

    let error = err("SELECT 1 FROM t (@p)");
    assert_eq!(error.number, 137, "{}", error.message);
    assert_eq!(error.state, 2);
    assert_eq!(error.message, "The scalar variable \"@p\" is not declared.");

    let error = err("SELECT 1 FROM t (NO_SUCH_FN(1))");
    assert_eq!(error.number, 195, "{}", error.message);
    assert_eq!(error.state, 10);
    assert_eq!(
        error.message,
        "'NO_SUCH_FN' is not a known built-in function name."
    );

    // 207 keeps the line of the column, as everywhere: `x` on line 5.
    let error = err("SELECT 1\nFROM\nt\n(\nx\n);");
    assert_eq!(error.number, 207, "{}", error.message);
    assert_eq!(error.line, 5);
}

/// The references of a `FROM` are re-read in the order they were written, a join left to
/// right, and the first error wins.
#[test]
fn the_references_are_re_read_in_from_order() {
    register_builtins();
    let error = err("SELECT 1 FROM t (x), t (1)");
    assert_eq!(error.number, 207, "{}", error.message);
    is_215(&err("SELECT 1 FROM t (1), t (x)"), "t");
    is_215(&err("SELECT 1 FROM u (1), t (2)"), "u");
    is_215(&err("SELECT 1 FROM u, t (1)"), "t");
    is_215(&err("SELECT 1 FROM u JOIN t (1) ON u.id = t.id"), "t");
    is_215(&err("SELECT 1 FROM t (1) JOIN u ON u.id = t.id"), "t");
    is_215(&err("SELECT 1 FROM u CROSS APPLY t (1)"), "t");
    is_215(&err("SELECT 1 FROM u (1) JOIN t (x) ON u.id = t.id"), "u");
    from_needs_the_catalogue(&err("SELECT 1 FROM u JOIN t (NOLOCK) ON u.id = t.id"));
    from_needs_the_catalogue(&err("SELECT 1 FROM t (NOLOCK) JOIN u ON u.id = t.id"));
    from_needs_the_catalogue(&err("SELECT 1 FROM u, t (NOLOCK)"));
    from_needs_the_catalogue(&err("SELECT 1 FROM u CROSS APPLY t (NOLOCK)"));
}

/// A `FROM` under a subquery, `EXISTS` or derived table is now bound, so the re-reading
/// of the arguments of a `FROM` reference happens before the construct is refused. SQL
/// Server answers 215 in all three for `t (1)`.
#[test]
fn a_nested_from_answers_the_refusal_of_its_construct() {
    // The outer query has no FROM, so the subquery's FROM is the first that is walked.
    for text in [
        "SELECT (SELECT 1 FROM t (1))",
        "SELECT 1 WHERE EXISTS (SELECT 1 FROM t (1))",
    ] {
        let error = err(text);
        assert_eq!(error.number, 215, "{text}: {}", error.message);
    }
    // A derived table in the outer FROM cannot reach the inner one without a catalogue:
    // the outer FROM itself is refused first.
    let error = err("SELECT 1 FROM (SELECT 1 FROM t (1)) AS d");
    assert_eq!(error.number, 50000, "{}", error.message);
    assert!(
        error.message.contains("FROM requires the catalog"),
        "{}",
        error.message
    );
}

/// The same name and argument distinguish a table hint from a function argument.
/// The mock supplies classification only; no function execution is implemented here.
///
/// A `FROM` with a catalogue resolves its name, and the mock here classifies `dbo.f`
/// without resolving it, so the paths that are not a call answer 208 naming `dbo.f`. What
/// this test tells apart is the call: with a table-valued function classification
/// `NOLOCK` binds as an expression and answers 207, which neither other classification
/// does. The 215 and the 207 of the re-reading over a name that **does** resolve are
/// tested in `names.rs`, `a_resolved_table_ignores_its_hints`, and the 208 that precedes
/// them in `an_unknown_object_answers_208_before_its_arguments`.
#[test]
fn classified_table_functions_do_not_reread_hints() {
    use vauban_binder::{CatalogView, TableReferenceKind};
    struct Classified(TableReferenceKind);
    impl CatalogView for Classified {
        fn classify_table_reference(
            &self,
            name: &ObjectName,
            database: &str,
            default_schema: &str,
        ) -> TableReferenceKind {
            assert_eq!(name.name.value, "f");
            assert_eq!(name.schema.as_ref().unwrap().value, "dbo");
            assert_eq!(database, "master");
            assert_eq!(default_schema, "dbo");
            self.0
        }
    }
    for (argument, function_error) in [("NOLOCK", 207), ("1", 208), ("", 208)] {
        let text = format!("SELECT * FROM dbo.f ({argument});");
        let batch = parse_batch(&text, &ParseOptions::default()).unwrap();
        for (kind, number) in [
            (TableReferenceKind::TableValuedFunction, function_error),
            (TableReferenceKind::Table, 208),
            (TableReferenceKind::Unknown, 208),
        ] {
            let catalog = Classified(kind);
            let mut ctx = BindContext::scalar(&text, SessionOptions::default());
            ctx.catalog = Some(&catalog);
            let error = bind(&batch.statements[0], &ctx).unwrap_err();
            assert_eq!(error.number, number, "{kind:?}: {text}");
            if number == 207 {
                assert_eq!(error.message, "Unknown column name 'NOLOCK'.");
                assert_eq!((error.severity, error.state), (16, 1));
            }
            if number == 208 {
                assert_eq!(error.message, "Unknown object name 'dbo.f'.");
                assert_eq!((error.severity, error.state), (16, 1));
            }
        }
    }
}

#[test]
fn a_named_table_column_is_not_an_argument_scope() {
    let error = err("SELECT * FROM t (a);");
    assert_eq!((error.number, error.severity, error.state), (207, 16, 1));
    assert_eq!(error.message, "Unknown column name 'a'.");
}

#[test]
fn omitted_schema_is_preserved_in_215() {
    for text in [
        "SELECT * FROM master..spt_values (1);",
        "SELECT * FROM [master]..[spt_values] ();",
    ] {
        is_215(&err(text), "master..spt_values");
    }
}

#[test]
fn a_trailing_ascii_space_in_a_delimited_hint_is_ignored() {
    for text in [
        "SELECT * FROM t ([NOLOCK ]);",
        "SELECT * FROM t ([NOLOCK  ]);",
        "SELECT * FROM t ([nolock ]);",
    ] {
        let error = err(text);
        assert_eq!(error.number, 50000, "{text}: {error:?}");
        assert!(error.message.contains("FROM requires the catalog"));
    }
}

#[test]
fn a_tab_a_no_break_space_or_a_leading_space_does_not_spell_a_hint() {
    for word in ["NOLOCK\t", "NOLOCK\u{a0}", " NOLOCK"] {
        let error = err(&format!("SELECT * FROM t ([{word}]);"));
        assert_eq!(error.number, 207);
        assert_eq!(error.message, format!("Unknown column name '{word}'."));
    }
}

/// The output schema a client reads for a bare `NULL` beside an operand, from SQL text.
///
/// The `varchar(2)` of the first column is the whole subject: `SELECT 'a' + NULL;` would
/// raise 245 if the bare `NULL` entered as an `int` and made the `+` an addition.
#[test]
fn a_bare_null_operand_types_the_column_a_client_reads() {
    register_builtins();
    assert_eq!(
        cols(
            "SELECT 'a' + NULL, N'a' + NULL, 0x01 + NULL, 1 + NULL, 1.5 + NULL, 1e0 + NULL, CAST(1 AS money) + NULL, CAST(1 AS tinyint) + NULL"
        ),
        vec![
            (String::new(), varchar(2), true),
            (String::new(), nvarchar(2), true),
            (
                String::new(),
                SqlType::VarBinary(vauban_types::Len::Fixed(2)),
                true
            ),
            (String::new(), SqlType::Int, true),
            (
                String::new(),
                SqlType::Numeric {
                    precision: 3,
                    scale: 1
                },
                true
            ),
            (String::new(), SqlType::Float, true),
            (String::new(), SqlType::Money, true),
            (String::new(), SqlType::TinyInt, true),
        ]
    );
}

/// The counter-proof: with no sibling to read, the bare `NULL` keeps the `int` of
/// `SELECT NULL;` — nine columns, nine `int`.
#[test]
fn bare_nulls_without_a_typed_sibling_stay_an_int() {
    register_builtins();
    let schema = cols(
        "SELECT NULL + NULL, NULL - NULL, NULL * 2, NULL / 0, NULL % 2, NULL & 1, NULL | 1, NULL ^ 1, -NULL",
    );
    assert_eq!(schema.len(), 9);
    for (name, ty, _) in &schema {
        assert_eq!(*ty, SqlType::Int, "column {name:?}");
    }
}

/// The refusals of a bare `NULL` beside a `date`, message included. The three bitwise
/// operators print their quoted symbol, the five others the operation, and the written
/// operand is named `NULL` on both sides.
#[test]
fn a_refused_bare_null_operand_is_named_null() {
    register_builtins();
    let words = [
        ("+", "add"),
        ("-", "subtract"),
        ("*", "multiply"),
        ("/", "divide"),
        ("%", "modulo"),
        ("&", "'&'"),
        ("|", "'|'"),
        ("^", "'^'"),
    ];
    for (symbol, word) in words {
        let right = err(&format!("SELECT CAST('2020-01-01' AS date) {symbol} NULL"));
        assert_eq!(right.number, 402, "{symbol}");
        assert_eq!(
            right.message,
            format!("The types date and NULL cannot be combined by the {word} operator.")
        );
        let left = err(&format!("SELECT NULL {symbol} CAST('2020-01-01' AS date)"));
        assert_eq!(
            left.message,
            format!("The types NULL and date cannot be combined by the {word} operator.")
        );
    }
    // The same pair written with two typed operands answers 8117 and names one type: the
    // bare `NULL` moves both the number and the name.
    assert_eq!(
        err("SELECT CAST('2020-01-01' AS date) + CAST('2020-01-01' AS date)").message,
        "Data type date is not accepted by the add operator."
    );
    // And a call names it the same way, where retyping from the sibling wrote `date`.
    assert_eq!(
        err("SELECT DATEADD(day, NULL, CAST('2020-01-01' AS date))").message,
        "Data type NULL is not accepted for argument 2 of the dateadd function."
    );
}
