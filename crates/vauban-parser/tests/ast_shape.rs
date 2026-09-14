//! The shape of the AST is a contract: `binder` and `executor` are written against the
//! names re-exported at the crate root and against the fields declared there.
//!
//! The first `use` below is that contract; it must keep
//! compiling as written. Everything the tests build is built by hand, without the parser,
//! which is still a stub at this point.

use vauban_parser::{
    Batch, BinaryOp, CaseArm, CommonTableExpr, DataType, Expr, Ident, JoinKind, Literal,
    ObjectName, OffsetFetch, OrderItem, Over, ParseOptions, QueryBody, QuerySpec, SelectItem,
    SelectStatement, SetOp, Span, Statement, TableRef, Top, TypeArg, UnaryOp, WindowFrame, With,
    parse_batch,
};

use vauban_parser::{AliasStyle, CreateProcedureStatement, FrameBound, FrameUnits};

fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

fn object_name(name: &str) -> ObjectName {
    ObjectName {
        server: None,
        database: None,
        schema: None,
        name: ident(name),
        span: Span::EMPTY,
    }
}

fn integer(text: &str) -> Expr {
    Expr::Literal(Literal::Integer(text.to_owned()), Span::EMPTY)
}

fn query_spec(items: Vec<SelectItem>) -> QuerySpec {
    QuerySpec {
        distinct: false,
        top: None,
        items,
        into: None,
        from: Vec::new(),
        where_: None,
        group_by: Vec::new(),
        having: None,
        span: Span::EMPTY,
    }
}

fn select_statement(body: QueryBody) -> SelectStatement {
    SelectStatement {
        with: None,
        body,
        order_by: Vec::new(),
        offset_fetch: None,
        for_clause: None,
        span: Span::EMPTY,
    }
}

/// `SELECT 1 AS n`, built by hand.
fn select_one_as_n(alias: &str) -> Statement {
    let item = SelectItem::Expr {
        expr: integer("1"),
        alias: Some(ident(alias)),
        alias_style: AliasStyle::As,
    };
    Statement::Select(Box::new(select_statement(QueryBody::Select(Box::new(
        query_spec(vec![item]),
    )))))
}

/// A minimal `CREATE PROCEDURE p AS`, a (V2) node no V1 rule produces.
fn create_procedure() -> Statement {
    Statement::CreateProcedure(Box::new(CreateProcedureStatement {
        name: object_name("p"),
        params: Vec::new(),
        body: Vec::new(),
        or_alter: false,
        with_options: Vec::new(),
        span: Span::EMPTY,
    }))
}

#[test]
fn ast_is_constructible() {
    let batch = Batch {
        statements: vec![select_one_as_n("n"), create_procedure()],
    };
    assert_eq!(batch.clone(), batch);
    assert_eq!(batch.statements.len(), 2);
}

#[test]
fn equality_ignores_positions_but_not_structure() {
    let moved = match select_one_as_n("n") {
        Statement::Select(mut select) => {
            select.span = Span {
                line: 42,
                column: 7,
                offset: 100,
                len: 3,
            };
            Statement::Select(select)
        }
        other => other,
    };
    assert_eq!(select_one_as_n("n"), moved);
    assert_ne!(select_one_as_n("n"), select_one_as_n("m"));
}

#[test]
fn clauses_of_a_query_are_constructible() {
    let spec = QuerySpec {
        distinct: true,
        top: Some(Top {
            expr: integer("10"),
            percent: false,
            with_ties: false,
            parenthesized: true,
            span: Span::EMPTY,
        }),
        items: vec![SelectItem::Wildcard(Span::EMPTY)],
        into: None,
        from: vec![TableRef::Join {
            left: Box::new(TableRef::Table {
                name: object_name("t"),
                alias: None,
                hints: Vec::new(),
                span: Span::EMPTY,
            }),
            right: Box::new(TableRef::Table {
                name: object_name("u"),
                alias: None,
                hints: Vec::new(),
                span: Span::EMPTY,
            }),
            kind: JoinKind::Inner,
            on: Some(Expr::Binary {
                op: BinaryOp::Eq,
                op_span: Span::EMPTY,
                left: Box::new(integer("1")),
                right: Box::new(Expr::Unary {
                    op: UnaryOp::Minus,
                    expr: Box::new(integer("1")),
                    span: Span::EMPTY,
                }),
                span: Span::EMPTY,
            }),
            span: Span::EMPTY,
        }],
        where_: None,
        group_by: Vec::new(),
        having: None,
        span: Span::EMPTY,
    };

    let mut statement = select_statement(QueryBody::SetOp {
        op: SetOp::Union,
        all: true,
        left: Box::new(QueryBody::Select(Box::new(spec))),
        right: Box::new(QueryBody::Select(Box::new(query_spec(vec![
            SelectItem::QualifiedWildcard(object_name("t")),
        ])))),
        span: Span::EMPTY,
    });
    statement.order_by = vec![OrderItem {
        expr: integer("1"),
        desc: true,
        explicit_direction: true,
        collate: None,
    }];
    statement.offset_fetch = Some(OffsetFetch {
        offset: integer("0"),
        fetch: Some(integer("10")),
        fetch_first: false,
        rows_singular: false,
    });
    statement.with = Some(With {
        recursive_allowed: false,
        ctes: vec![CommonTableExpr {
            name: ident("c"),
            columns: vec![ident("n")],
            query: Box::new(select_statement(QueryBody::Select(Box::new(query_spec(
                vec![SelectItem::Wildcard(Span::EMPTY)],
            ))))),
            span: Span::EMPTY,
        }],
        span: Span::EMPTY,
    });

    assert_eq!(statement.clone(), statement);
}

#[test]
fn expression_nodes_are_constructible() {
    let case = Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: integer("1"),
            then: integer("2"),
        }],
        else_: Some(Box::new(integer("3"))),
        span: Span::EMPTY,
    };
    let windowed = Expr::Function {
        name: object_name("count"),
        args: Vec::new(),
        star: true,
        distinct: false,
        over: Some(Box::new(Over {
            partition_by: vec![integer("1")],
            order_by: Vec::new(),
            frame: Some(WindowFrame {
                units: FrameUnits::Rows,
                start: FrameBound::UnboundedPreceding,
                end: Some(FrameBound::CurrentRow),
            }),
            span: Span::EMPTY,
        })),
        span: Span::EMPTY,
    };
    let cast = Expr::Cast {
        expr: Box::new(case.clone()),
        ty: DataType {
            name: "varchar".to_owned(),
            args: vec![TypeArg::Max, TypeArg::Number(10)],
            span: Span::EMPTY,
        },
        try_: true,
        span: Span::EMPTY,
    };
    assert_eq!(case.clone(), case);
    assert_eq!(windowed.clone(), windowed);
    assert_eq!(cast.clone(), cast);
}

#[test]
fn parse_batch_is_the_crate_entry_point() {
    // Referenced, not called: what a call returns is checked by `tests/select.rs`; this
    // file asserts the shape of the AST, not the grammar.
    let _entry_point = parse_batch;
    assert!(ParseOptions::default().quoted_identifier);
}
