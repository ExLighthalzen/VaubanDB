//! `Display` on the AST: every tree is built **by hand**, without the parser. Each test
//! compares `to_string()` to the exact expected text.

use vauban_parser::{
    AliasStyle, AlterTableAction, AlterTableStatement, AssignOp, AssignTarget, Assignment, Batch,
    BinaryOp, CaseArm, Clustering, ColumnConstraint, ColumnConstraintKind, ColumnDef, ColumnRef,
    CommonTableExpr, CreateDatabaseStatement, CreateIndexStatement, CreateProcedureStatement,
    CreateTableStatement, DataType, DeclareItem, DeclareStatement, DeleteStatement,
    DropIndexStatement, ExecuteArg, ExecuteStatement, ExecuteTarget, Expr, ForeignKeyRef,
    FrameBound, FrameUnits, Ident, Identity, InList, IndexColumn, IndexStorage, InsertSource,
    InsertStatement, JoinKind, Literal, MergeClause, MergeStatement, ObjectName, OrderItem, Over,
    Quantifier, QueryBody, QuerySpec, RefAction, SelectItem, SelectStatement, SetOp,
    SetOptionStatement, SetOptionValue, SetStatement, SetValue, SortDirection, Span, Statement,
    TableConstraint, TableConstraintKind, TableDefinition, TableHint, TableRef, Top, TypeArg,
    UnaryOp, UpdateStatement, WindowFrame, With,
};

// ---------------------------------------------------------------------------
// Builders. `Span::EMPTY` everywhere: a span never changes what `Display` writes.
// ---------------------------------------------------------------------------

fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

fn quoted(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: true,
    }
}

fn object(name: &str) -> ObjectName {
    ObjectName {
        server: None,
        database: None,
        schema: None,
        name: ident(name),
        span: Span::EMPTY,
    }
}

fn qualified(schema: &str, name: &str) -> ObjectName {
    ObjectName {
        server: None,
        database: None,
        schema: Some(ident(schema)),
        name: ident(name),
        span: Span::EMPTY,
    }
}

fn column(name: &str) -> Expr {
    Expr::Column(ColumnRef {
        qualifier: None,
        name: ident(name),
        span: Span::EMPTY,
    })
}

fn qualified_column(table: &str, name: &str) -> Expr {
    Expr::Column(ColumnRef {
        qualifier: Some(object(table)),
        name: ident(name),
        span: Span::EMPTY,
    })
}

fn integer(text: &str) -> Expr {
    Expr::Literal(Literal::Integer(text.to_owned()), Span::EMPTY)
}

fn string(value: &str) -> Expr {
    Expr::Literal(
        Literal::Str {
            value: value.to_owned(),
            unicode: false,
        },
        Span::EMPTY,
    )
}

fn variable(name: &str) -> Expr {
    Expr::Variable {
        name: name.to_owned(),
        span: Span::EMPTY,
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        op_span: Span::EMPTY,
        left: Box::new(left),
        right: Box::new(right),
        span: Span::EMPTY,
    }
}

fn unary(op: UnaryOp, expr: Expr) -> Expr {
    Expr::Unary {
        op,
        expr: Box::new(expr),
        span: Span::EMPTY,
    }
}

fn count_star() -> Expr {
    Expr::Function {
        name: object("COUNT"),
        args: Vec::new(),
        star: true,
        distinct: false,
        over: None,
        span: Span::EMPTY,
    }
}

fn ty(name: &str, args: Vec<TypeArg>) -> DataType {
    DataType {
        name: name.to_owned(),
        args,
        span: Span::EMPTY,
    }
}

fn item(expr: Expr) -> SelectItem {
    SelectItem::Expr {
        expr,
        alias: None,
        alias_style: AliasStyle::As,
    }
}

fn aliased(expr: Expr, alias: &str, alias_style: AliasStyle) -> SelectItem {
    SelectItem::Expr {
        expr,
        alias: Some(ident(alias)),
        alias_style,
    }
}

fn spec(items: Vec<SelectItem>) -> QuerySpec {
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

fn statement_of(spec: QuerySpec) -> SelectStatement {
    SelectStatement {
        with: None,
        body: QueryBody::Select(Box::new(spec)),
        order_by: Vec::new(),
        offset_fetch: None,
        for_clause: None,
        span: Span::EMPTY,
    }
}

/// `SELECT <n>`, the smallest query there is.
fn select_literal(text: &str) -> SelectStatement {
    statement_of(spec(vec![item(integer(text))]))
}

fn table(name: &str) -> TableRef {
    TableRef::Table {
        name: object(name),
        alias: None,
        hints: Vec::new(),
        span: Span::EMPTY,
    }
}

fn order_item(expr: Expr, desc: bool, explicit_direction: bool) -> OrderItem {
    OrderItem {
        expr,
        desc,
        explicit_direction,
        collate: None,
    }
}

fn set_variable(name: &str, op: AssignOp, value: Expr) -> Statement {
    Statement::Set(Box::new(SetStatement {
        target: AssignTarget::Variable(name.to_owned()),
        op,
        value: SetValue::Expr(value),
        span: Span::EMPTY,
    }))
}

fn column_def(name: &str, ty: DataType, constraints: Vec<ColumnConstraint>) -> ColumnDef {
    ColumnDef {
        name: ident(name),
        ty,
        collation: None,
        constraints,
        identity: None,
        computed: None,
        span: Span::EMPTY,
    }
}

fn constraint(name: Option<&str>, kind: TableConstraintKind) -> TableConstraint {
    TableConstraint {
        name: name.map(ident),
        kind,
        storage: IndexStorage::default(),
        span: Span::EMPTY,
    }
}

fn column_constraint(kind: ColumnConstraintKind) -> ColumnConstraint {
    ColumnConstraint {
        name: None,
        kind,
        storage: IndexStorage::default(),
        span: Span::EMPTY,
    }
}

fn index_column(name: &str, desc: bool, explicit_direction: bool) -> IndexColumn {
    IndexColumn {
        name: ident(name),
        desc,
        explicit_direction,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn display_mots_cles_en_majuscules() {
    // Every keyword `Display` writes is uppercase; the identifiers below are lowercase,
    // so a lowercase keyword in the output would be a bug.
    let mut query = spec(vec![item(column("c"))]);
    query.from = vec![table("t")];
    query.where_ = Some(Expr::IsNull {
        expr: Box::new(column("c")),
        negated: true,
        span: Span::EMPTY,
    });
    let mut select = statement_of(query);
    select.order_by = vec![order_item(column("c"), true, true)];
    let rendered = select.to_string();
    assert_eq!(
        rendered,
        "SELECT c FROM t WHERE c IS NOT NULL ORDER BY c DESC"
    );
    for keyword in ["select", "from", "where", "is not null", "order by", "desc"] {
        assert!(
            !rendered.contains(keyword),
            "lowercase keyword {keyword} in {rendered}"
        );
    }
}

#[test]
fn display_idents() {
    assert_eq!(ident("c").to_string(), "c");
    assert_eq!(quoted("my col").to_string(), "[my col]");
    assert_eq!(quoted("a]b").to_string(), "[a]]b]");
    // Safety rule: a value that is not a valid regular identifier is bracketed even when
    // the user did not write brackets, because that is the only spelling that re-parses.
    assert_eq!(ident("my col").to_string(), "[my col]");
    assert_eq!(ident("select").to_string(), "[select]");
    // A temporary table and an underscore stay bare: both are regular identifiers.
    assert_eq!(ident("#t").to_string(), "#t");
    assert_eq!(ident("_a1").to_string(), "_a1");
}

#[test]
fn display_object_name() {
    let all_quoted = ObjectName {
        server: Some(quoted("s")),
        database: Some(quoted("d")),
        schema: Some(quoted("dbo")),
        name: quoted("t"),
        span: Span::EMPTY,
    };
    assert_eq!(all_quoted.to_string(), "[s].[d].[dbo].[t]");
    let bare = ObjectName {
        server: Some(ident("srv")),
        database: Some(ident("db")),
        schema: Some(ident("dbo")),
        name: ident("t"),
        span: Span::EMPTY,
    };
    assert_eq!(bare.to_string(), "srv.db.dbo.t");
    assert_eq!(object("t").to_string(), "t");
    assert_eq!(qualified("dbo", "t").to_string(), "dbo.t");
    // A skipped level keeps its dot: `srv..dbo.t`.
    let skipped = ObjectName {
        server: Some(ident("srv")),
        database: None,
        schema: Some(ident("dbo")),
        name: ident("t"),
        span: Span::EMPTY,
    };
    assert_eq!(skipped.to_string(), "srv..dbo.t");
}

#[test]
fn display_literals() {
    assert_eq!(Literal::Integer("1".to_owned()).to_string(), "1");
    assert_eq!(Literal::Decimal("1.50".to_owned()).to_string(), "1.50");
    assert_eq!(Literal::Float("1.5E-2".to_owned()).to_string(), "1.5E-2");
    assert_eq!(
        Literal::Str {
            value: "a'b".to_owned(),
            unicode: false,
        }
        .to_string(),
        "'a''b'"
    );
    assert_eq!(
        Literal::Str {
            value: "é".to_owned(),
            unicode: true,
        }
        .to_string(),
        "N'é'"
    );
    assert_eq!(Literal::Binary("00FF".to_owned()).to_string(), "0x00FF");
    assert_eq!(Literal::Money("1.50".to_owned()).to_string(), "$1.50");
    assert_eq!(Literal::Money("-1.50".to_owned()).to_string(), "$-1.50");
    assert_eq!(Literal::Null.to_string(), "NULL");
    assert_eq!(Literal::Default.to_string(), "DEFAULT");
}

#[test]
fn display_normalisations() {
    // `<>`, never `!=`.
    assert_eq!(
        binary(BinaryOp::Ne, column("a"), integer("1")).to_string(),
        "a <> 1"
    );
    // `LEFT JOIN`, never `LEFT OUTER JOIN`.
    let join = TableRef::Join {
        left: Box::new(table("a")),
        right: Box::new(table("b")),
        kind: JoinKind::Left,
        on: Some(binary(BinaryOp::Eq, integer("1"), integer("1"))),
        span: Span::EMPTY,
    };
    assert_eq!(join.to_string(), "a LEFT JOIN b ON 1 = 1");
    // Transactions: the long form, never `TRAN` nor `WORK`.
    assert_eq!(
        Statement::BeginTransaction {
            name: None,
            mark: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "BEGIN TRANSACTION"
    );
    assert_eq!(
        Statement::Commit {
            name: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "COMMIT TRANSACTION"
    );
    assert_eq!(
        Statement::Rollback {
            name: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "ROLLBACK TRANSACTION"
    );
    assert_eq!(
        Statement::Save {
            name: ident("s1"),
            span: Span::EMPTY,
        }
        .to_string(),
        "SAVE TRANSACTION s1"
    );
    // `EXECUTE`, never `EXEC`; an implicit call writes nothing before the name.
    let execute = |implicit| {
        ExecuteStatement {
            target: ExecuteTarget::Procedure(object("p")),
            args: vec![ExecuteArg {
                name: None,
                value: integer("1"),
                output: false,
            }],
            return_into: None,
            implicit,
            span: Span::EMPTY,
        }
        .to_string()
    };
    assert_eq!(execute(false), "EXECUTE p 1");
    assert_eq!(execute(true), "p 1");
    // The `INTO` of an `INSERT` is always written.
    assert_eq!(
        InsertStatement {
            target: table("t"),
            columns: Vec::new(),
            source: InsertSource::DefaultValues,
            top: None,
            output: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "INSERT INTO t DEFAULT VALUES"
    );
    // `DROP INDEX ix ON t`, never `DROP INDEX t.ix`.
    assert_eq!(
        DropIndexStatement {
            name: ident("IX_t"),
            table: object("t"),
            if_exists: false,
            options: Vec::new(),
            span: Span::EMPTY,
        }
        .to_string(),
        "DROP INDEX IX_t ON t"
    );
    // An empty constraint list means `ALL`.
    assert_eq!(
        AlterTableStatement {
            name: object("t"),
            action: AlterTableAction::Check {
                constraints: Vec::new(),
                enable: true,
                with_check: true,
            },
            span: Span::EMPTY,
        }
        .to_string(),
        "ALTER TABLE t WITH CHECK CHECK CONSTRAINT ALL"
    );
}

#[test]
fn display_expressions() {
    let sum = binary(
        BinaryOp::Add,
        integer("1"),
        binary(BinaryOp::Mul, integer("2"), integer("3")),
    );
    assert_eq!(sum.to_string(), "1 + 2 * 3");
    let nested = binary(
        BinaryOp::Mul,
        Expr::Nested(
            Box::new(binary(BinaryOp::Add, integer("1"), integer("2"))),
            Span::EMPTY,
        ),
        integer("3"),
    );
    assert_eq!(nested.to_string(), "(1 + 2) * 3");
    assert_eq!(unary(UnaryOp::Minus, integer("1")).to_string(), "-1");
    assert_eq!(
        unary(
            UnaryOp::Not,
            binary(BinaryOp::Eq, column("a"), integer("1"))
        )
        .to_string(),
        "NOT a = 1"
    );
    assert_eq!(unary(UnaryOp::BitNot, column("x")).to_string(), "~x");
}

#[test]
fn display_predicats() {
    let is_null = |negated| {
        Expr::IsNull {
            expr: Box::new(column("c")),
            negated,
            span: Span::EMPTY,
        }
        .to_string()
    };
    assert_eq!(is_null(false), "c IS NULL");
    assert_eq!(is_null(true), "c IS NOT NULL");
    let in_list = |negated| {
        Expr::In {
            expr: Box::new(column("c")),
            list: InList::Exprs(vec![integer("1"), integer("2")]),
            negated,
            span: Span::EMPTY,
        }
        .to_string()
    };
    assert_eq!(in_list(false), "c IN (1, 2)");
    assert_eq!(in_list(true), "c NOT IN (1, 2)");
    assert_eq!(
        Expr::In {
            expr: Box::new(column("c")),
            list: InList::Subquery(Box::new(select_literal("1"))),
            negated: false,
            span: Span::EMPTY,
        }
        .to_string(),
        "c IN (SELECT 1)"
    );
    assert_eq!(
        Expr::Like {
            expr: Box::new(column("c")),
            pattern: Box::new(string("a%")),
            escape: Some(Box::new(string("!"))),
            negated: false,
            span: Span::EMPTY,
        }
        .to_string(),
        "c LIKE 'a%' ESCAPE '!'"
    );
    assert_eq!(
        Expr::Between {
            expr: Box::new(column("c")),
            low: Box::new(integer("1")),
            high: Box::new(integer("2")),
            negated: false,
            span: Span::EMPTY,
        }
        .to_string(),
        "c BETWEEN 1 AND 2"
    );
    assert_eq!(
        Expr::Exists(Box::new(select_literal("1")), Span::EMPTY).to_string(),
        "EXISTS (SELECT 1)"
    );
    assert_eq!(
        Expr::Subquery(Box::new(select_literal("1")), Span::EMPTY).to_string(),
        "(SELECT 1)"
    );
    assert_eq!(
        Expr::Quantified {
            expr: Box::new(column("c")),
            op: BinaryOp::Gt,
            quantifier: Quantifier::All,
            subquery: Box::new(select_literal("1")),
            span: Span::EMPTY,
        }
        .to_string(),
        "c > ALL (SELECT 1)"
    );
}

#[test]
fn display_case_cast_convert() {
    let searched = Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: binary(BinaryOp::Eq, integer("1"), integer("1")),
            then: string("a"),
        }],
        else_: Some(Box::new(string("b"))),
        span: Span::EMPTY,
    };
    assert_eq!(
        searched.to_string(),
        "CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END"
    );
    let simple = Expr::Case {
        operand: Some(Box::new(column("c"))),
        arms: vec![CaseArm {
            when: integer("1"),
            then: string("a"),
        }],
        else_: None,
        span: Span::EMPTY,
    };
    assert_eq!(simple.to_string(), "CASE c WHEN 1 THEN 'a' END");
    let cast = |try_, name, args| {
        Expr::Cast {
            expr: Box::new(column("c")),
            ty: ty(name, args),
            try_,
            span: Span::EMPTY,
        }
        .to_string()
    };
    assert_eq!(
        cast(false, "varchar", vec![TypeArg::Number(10)]),
        "CAST(c AS varchar(10))"
    );
    assert_eq!(cast(true, "int", Vec::new()), "TRY_CAST(c AS int)");
    assert_eq!(
        Expr::Convert {
            ty: ty("varchar", vec![TypeArg::Number(10)]),
            expr: Box::new(column("c")),
            style: Some(Box::new(integer("120"))),
            try_: false,
            span: Span::EMPTY,
        }
        .to_string(),
        "CONVERT(varchar(10), c, 120)"
    );
    assert_eq!(
        Expr::Convert {
            ty: ty("int", Vec::new()),
            expr: Box::new(column("c")),
            style: None,
            try_: true,
            span: Span::EMPTY,
        }
        .to_string(),
        "TRY_CONVERT(int, c)"
    );
    assert_eq!(
        ty("varchar", vec![TypeArg::Max]).to_string(),
        "varchar(MAX)"
    );
}

#[test]
fn display_functions() {
    assert_eq!(count_star().to_string(), "COUNT(*)");
    assert_eq!(
        Expr::Function {
            name: object("COUNT"),
            args: vec![column("c")],
            star: false,
            distinct: true,
            over: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "COUNT(DISTINCT c)"
    );
    assert_eq!(
        Expr::Function {
            name: qualified("dbo", "f"),
            args: vec![integer("1"), string("a")],
            star: false,
            distinct: false,
            over: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "dbo.f(1, 'a')"
    );
    // (V2) Window functions.
    assert_eq!(
        Expr::Function {
            name: object("ROW_NUMBER"),
            args: Vec::new(),
            star: false,
            distinct: false,
            over: Some(Box::new(Over {
                partition_by: vec![column("a")],
                order_by: vec![order_item(column("b"), true, true)],
                frame: None,
                span: Span::EMPTY,
            })),
            span: Span::EMPTY,
        }
        .to_string(),
        "ROW_NUMBER() OVER (PARTITION BY a ORDER BY b DESC)"
    );
    assert_eq!(
        Expr::Function {
            name: object("SUM"),
            args: vec![column("x")],
            star: false,
            distinct: false,
            over: Some(Box::new(Over {
                partition_by: Vec::new(),
                order_by: vec![order_item(column("d"), false, false)],
                frame: Some(WindowFrame {
                    units: FrameUnits::Rows,
                    start: FrameBound::UnboundedPreceding,
                    end: Some(FrameBound::CurrentRow),
                }),
                span: Span::EMPTY,
            })),
            span: Span::EMPTY,
        }
        .to_string(),
        "SUM(x) OVER (ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
    );
}

/// A function name is never bracketed by the safety rule: `LEFT`, `COALESCE`, `NULLIF`,
/// `CONVERT` and the niladic `CURRENT_TIMESTAMP` are reserved keywords that the grammar
/// reads as function names, and `[LEFT](x, 1)` would re-parse with `Ident.quoted` turned
/// to `true`, breaking the parse -> `Display` -> parse contract. Only what the user quoted
/// and what is not spelled like an identifier keeps its brackets.
#[test]
fn display_function_name_is_never_bracketed() {
    fn call(name: Ident, args: Vec<Expr>) -> Expr {
        Expr::Function {
            name: ObjectName {
                server: None,
                database: None,
                schema: None,
                name,
                span: Span::EMPTY,
            },
            args,
            star: false,
            distinct: false,
            over: None,
            span: Span::EMPTY,
        }
    }

    assert_eq!(
        call(ident("LEFT"), vec![column("x"), integer("1")]).to_string(),
        "LEFT(x, 1)"
    );
    assert_eq!(
        call(ident("COALESCE"), vec![column("a"), column("b")]).to_string(),
        "COALESCE(a, b)"
    );
    // Whether the niladic form is an `Expr::Function` with no argument is the grammar's
    // choice; what this asserts is only that the reserved name stays bare.
    assert_eq!(
        call(ident("CURRENT_TIMESTAMP"), Vec::new()).to_string(),
        "CURRENT_TIMESTAMP()"
    );
    // Brackets the user wrote are kept: `[LEFT]` is a name he quoted.
    assert_eq!(
        call(quoted("LEFT"), vec![column("x"), integer("1")]).to_string(),
        "[LEFT](x, 1)"
    );
    // A value that is not spelled like an identifier is still bracketed, since writing it
    // bare would not parse back.
    assert_eq!(call(ident("my fn"), Vec::new()).to_string(), "[my fn]()");
    // A qualified name keeps the safety rule on its qualifiers: only the last part is a
    // function name.
    assert_eq!(
        Expr::Function {
            name: qualified("select", "LEFT"),
            args: vec![column("x")],
            star: false,
            distinct: false,
            over: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "[select].LEFT(x)"
    );
}

/// A niladic function written without parentheses is an `Expr::Column` whose one-part
/// name is a reserved keyword; the safety rule that would bracket it is lifted, exactly
/// as it is for a function name, because `[CURRENT_TIMESTAMP]` parses back as a quoted
/// name and breaks the parse -> `Display` -> parse contract.
#[test]
fn display_niladic_column_is_never_bracketed() {
    for name in [
        "CURRENT_TIMESTAMP",
        "CURRENT_USER",
        "SESSION_USER",
        "SYSTEM_USER",
        "USER",
    ] {
        assert_eq!(column(name).to_string(), name);
    }
    // The case the user wrote is kept, and the exception with it.
    assert_eq!(column("current_timestamp").to_string(), "current_timestamp");
    // Brackets the user wrote are kept: a quoted name is a column, not a function.
    assert_eq!(
        Expr::Column(ColumnRef {
            qualifier: None,
            name: quoted("CURRENT_TIMESTAMP"),
            span: Span::EMPTY,
        })
        .to_string(),
        "[CURRENT_TIMESTAMP]"
    );
    // A qualified name is a plain column reference and keeps the safety rule.
    assert_eq!(
        qualified_column("t", "CURRENT_TIMESTAMP").to_string(),
        "t.[CURRENT_TIMESTAMP]"
    );
    // Another reserved word is still bracketed: the exception covers these five names.
    assert_eq!(column("select").to_string(), "[select]");
}

#[test]
fn display_select() {
    let mut query = spec(vec![
        aliased(column("a"), "x", AliasStyle::As),
        aliased(column("b"), "y", AliasStyle::Bare),
        aliased(column("c"), "z", AliasStyle::Equals),
        SelectItem::QualifiedWildcard(object("t")),
        SelectItem::Wildcard(Span::EMPTY),
    ]);
    query.distinct = true;
    query.top = Some(Top {
        expr: integer("10"),
        percent: true,
        with_ties: true,
        parenthesized: true,
        span: Span::EMPTY,
    });
    query.from = vec![TableRef::Table {
        name: object("t"),
        alias: None,
        hints: vec![TableHint {
            name: "NOLOCK".to_owned(),
            args: Vec::new(),
            span: Span::EMPTY,
        }],
        span: Span::EMPTY,
    }];
    query.where_ = Some(binary(BinaryOp::Eq, column("a"), integer("1")));
    query.group_by = vec![column("a")];
    query.having = Some(binary(BinaryOp::Gt, count_star(), integer("1")));
    let mut select = statement_of(query);
    select.order_by = vec![
        order_item(column("a"), true, true),
        order_item(integer("2"), false, false),
    ];
    assert_eq!(
        select.to_string(),
        "SELECT DISTINCT TOP (10) PERCENT WITH TIES a AS x, b y, z = c, t.*, * \
         FROM t WITH (NOLOCK) WHERE a = 1 GROUP BY a HAVING COUNT(*) > 1 \
         ORDER BY a DESC, 2"
    );
    assert_eq!(select_literal("1").to_string(), "SELECT 1");
}

#[test]
fn display_joins_and_setops() {
    let inner = TableRef::Join {
        left: Box::new(table("a")),
        right: Box::new(table("b")),
        kind: JoinKind::Inner,
        on: Some(binary(
            BinaryOp::Eq,
            qualified_column("a", "id"),
            qualified_column("b", "id"),
        )),
        span: Span::EMPTY,
    };
    let derived = TableRef::Derived {
        query: Box::new(select_literal("1")),
        alias: Some(ident("d")),
        columns: Vec::new(),
        span: Span::EMPTY,
    };
    let mut query = spec(vec![SelectItem::Wildcard(Span::EMPTY)]);
    query.from = vec![TableRef::Join {
        left: Box::new(inner),
        right: Box::new(derived),
        kind: JoinKind::Left,
        on: Some(binary(BinaryOp::Eq, integer("1"), integer("1"))),
        span: Span::EMPTY,
    }];
    assert_eq!(
        statement_of(query).to_string(),
        "SELECT * FROM a INNER JOIN b ON a.id = b.id LEFT JOIN (SELECT 1) AS d ON 1 = 1"
    );

    let union = QueryBody::SetOp {
        op: SetOp::Union,
        all: true,
        left: Box::new(QueryBody::Select(Box::new(spec(vec![item(integer("1"))])))),
        right: Box::new(QueryBody::Select(Box::new(spec(vec![item(integer("2"))])))),
        span: Span::EMPTY,
    };
    let except = SelectStatement {
        with: None,
        body: QueryBody::SetOp {
            op: SetOp::Except,
            all: false,
            left: Box::new(union),
            right: Box::new(QueryBody::Select(Box::new(spec(vec![item(integer("3"))])))),
            span: Span::EMPTY,
        },
        order_by: Vec::new(),
        offset_fetch: None,
        for_clause: None,
        span: Span::EMPTY,
    };
    assert_eq!(
        except.to_string(),
        "SELECT 1 UNION ALL SELECT 2 EXCEPT SELECT 3"
    );

    let mut cross = spec(vec![SelectItem::Wildcard(Span::EMPTY)]);
    cross.from = vec![TableRef::Join {
        left: Box::new(table("a")),
        right: Box::new(table("b")),
        kind: JoinKind::Cross,
        on: None,
        span: Span::EMPTY,
    }];
    assert_eq!(
        statement_of(cross).to_string(),
        "SELECT * FROM a CROSS JOIN b"
    );
}

#[test]
fn display_dml() {
    let values = InsertStatement {
        target: table("t"),
        columns: vec![ident("a"), ident("b")],
        source: InsertSource::Values(vec![
            vec![integer("1"), integer("2")],
            vec![integer("3"), integer("4")],
        ]),
        top: None,
        output: None,
        span: Span::EMPTY,
    };
    assert_eq!(
        values.to_string(),
        "INSERT INTO t (a, b) VALUES (1, 2), (3, 4)"
    );
    let default_values = InsertStatement {
        target: table("t"),
        columns: Vec::new(),
        source: InsertSource::DefaultValues,
        top: None,
        output: None,
        span: Span::EMPTY,
    };
    assert_eq!(default_values.to_string(), "INSERT INTO t DEFAULT VALUES");
    let from_query = InsertStatement {
        target: table("t"),
        columns: Vec::new(),
        source: InsertSource::Query(Box::new(select_literal("1"))),
        top: None,
        output: None,
        span: Span::EMPTY,
    };
    assert_eq!(from_query.to_string(), "INSERT INTO t SELECT 1");

    let update = UpdateStatement {
        target: table("t"),
        top: None,
        assignments: vec![
            Assignment {
                target: AssignTarget::Column(ColumnRef {
                    qualifier: None,
                    name: ident("a"),
                    span: Span::EMPTY,
                }),
                op: AssignOp::Set,
                value: integer("1"),
            },
            Assignment {
                target: AssignTarget::Column(ColumnRef {
                    qualifier: None,
                    name: ident("b"),
                    span: Span::EMPTY,
                }),
                op: AssignOp::AddAssign,
                value: integer("2"),
            },
        ],
        from: vec![TableRef::Join {
            left: Box::new(table("t")),
            right: Box::new(table("u")),
            kind: JoinKind::Inner,
            on: Some(binary(BinaryOp::Eq, integer("1"), integer("1"))),
            span: Span::EMPTY,
        }],
        where_: Some(binary(BinaryOp::Eq, column("a"), integer("1"))),
        output: None,
        span: Span::EMPTY,
    };
    assert_eq!(
        update.to_string(),
        "UPDATE t SET a = 1, b += 2 FROM t INNER JOIN u ON 1 = 1 WHERE a = 1"
    );

    let delete = DeleteStatement {
        target: table("t"),
        top: None,
        from: Vec::new(),
        where_: Some(binary(BinaryOp::Eq, column("a"), integer("1"))),
        output: None,
        span: Span::EMPTY,
    };
    assert_eq!(delete.to_string(), "DELETE FROM t WHERE a = 1");

    assert_eq!(
        Statement::Truncate {
            table: object("t"),
            span: Span::EMPTY,
        }
        .to_string(),
        "TRUNCATE TABLE t"
    );
}

#[test]
fn display_ddl() {
    let mut id = column_def(
        "id",
        ty("int", Vec::new()),
        vec![
            column_constraint(ColumnConstraintKind::NotNull),
            column_constraint(ColumnConstraintKind::PrimaryKey {
                clustering: Some(Clustering::Clustered),
                order: None,
            }),
        ],
    );
    id.identity = Some(Identity {
        seed: Some(1),
        increment: Some(1),
    });
    let name = column_def(
        "name",
        ty("nvarchar", vec![TypeArg::Number(50)]),
        vec![
            column_constraint(ColumnConstraintKind::Null),
            column_constraint(ColumnConstraintKind::Default(string("x"))),
        ],
    );
    let create = CreateTableStatement {
        name: qualified("dbo", "t"),
        definition: TableDefinition {
            columns: vec![id, name],
            constraints: vec![
                constraint(
                    Some("UQ_t"),
                    TableConstraintKind::Unique {
                        columns: vec![index_column("name", false, true)],
                        clustering: None,
                    },
                ),
                constraint(
                    Some("FK_t"),
                    TableConstraintKind::ForeignKey {
                        columns: vec![ident("id")],
                        reference: ForeignKeyRef {
                            table: qualified("dbo", "u"),
                            columns: vec![ident("id")],
                            on_delete: Some(RefAction::Cascade),
                            on_update: None,
                        },
                    },
                ),
                constraint(
                    None,
                    TableConstraintKind::Check {
                        expr: binary(BinaryOp::Gt, column("id"), integer("0")),
                        not_for_replication: false,
                    },
                ),
            ],
        },
        placement: None,
        textimage_on: None,
        span: Span::EMPTY,
    };
    assert_eq!(
        create.to_string(),
        "CREATE TABLE dbo.t (id int IDENTITY(1, 1) NOT NULL PRIMARY KEY CLUSTERED, \
         name nvarchar(50) NULL DEFAULT 'x', CONSTRAINT UQ_t UNIQUE (name ASC), \
         CONSTRAINT FK_t FOREIGN KEY (id) REFERENCES dbo.u (id) ON DELETE CASCADE, \
         CHECK (id > 0))"
    );

    assert_eq!(
        AlterTableStatement {
            name: object("t"),
            action: AlterTableAction::AddColumns {
                columns: vec![column_def(
                    "c",
                    ty("int", Vec::new()),
                    vec![column_constraint(ColumnConstraintKind::Null)],
                )],
                with_check: None,
            },
            span: Span::EMPTY,
        }
        .to_string(),
        "ALTER TABLE t ADD c int NULL"
    );
    assert_eq!(
        AlterTableStatement {
            name: object("t"),
            action: AlterTableAction::DropColumns {
                names: vec![ident("c")],
                if_exists: false,
            },
            span: Span::EMPTY,
        }
        .to_string(),
        "ALTER TABLE t DROP COLUMN c"
    );
    assert_eq!(
        Statement::DropTable {
            names: vec![object("t")],
            if_exists: false,
            span: Span::EMPTY,
        }
        .to_string(),
        "DROP TABLE t"
    );
    assert_eq!(
        CreateIndexStatement {
            name: ident("IX_t"),
            table: object("t"),
            columns: vec![index_column("a", true, true)],
            unique: true,
            clustering: Some(Clustering::NonClustered),
            include: Vec::new(),
            where_: None,
            storage: IndexStorage::default(),
            span: Span::EMPTY,
        }
        .to_string(),
        "CREATE UNIQUE NONCLUSTERED INDEX IX_t ON t (a DESC)"
    );
    assert_eq!(
        DropIndexStatement {
            name: ident("IX_t"),
            table: object("t"),
            if_exists: false,
            options: Vec::new(),
            span: Span::EMPTY,
        }
        .to_string(),
        "DROP INDEX IX_t ON t"
    );
    assert_eq!(
        CreateDatabaseStatement {
            name: ident("d"),
            options: Vec::new(),
            span: Span::EMPTY,
        }
        .to_string(),
        "CREATE DATABASE d"
    );
    assert_eq!(
        Statement::Use {
            database: ident("d"),
            span: Span::EMPTY,
        }
        .to_string(),
        "USE d"
    );
    // A column-level `ASC`/`DESC` and a sort direction on its own.
    assert_eq!(SortDirection::Desc.to_string(), "DESC");
}

#[test]
fn display_flow_and_txn() {
    let declare = DeclareStatement {
        items: vec![
            DeclareItem::Variable {
                name: "@x".to_owned(),
                ty: ty("int", Vec::new()),
                default: Some(Box::new(integer("1"))),
            },
            DeclareItem::Variable {
                name: "@y".to_owned(),
                ty: ty("varchar", vec![TypeArg::Number(10)]),
                default: None,
            },
        ],
        span: Span::EMPTY,
    };
    assert_eq!(declare.to_string(), "DECLARE @x int = 1, @y varchar(10)");
    assert_eq!(
        set_variable("@x", AssignOp::Set, integer("1")).to_string(),
        "SET @x = 1"
    );
    assert_eq!(
        set_variable("@x", AssignOp::AddAssign, integer("1")).to_string(),
        "SET @x += 1"
    );
    assert_eq!(
        SetOptionStatement {
            options: vec![("NOCOUNT".to_owned(), SetOptionValue::On)],
            span: Span::EMPTY,
        }
        .to_string(),
        "SET NOCOUNT ON"
    );

    let if_statement = Statement::If {
        condition: binary(BinaryOp::Eq, variable("@x"), integer("1")),
        then_branch: Box::new(Statement::Print {
            expr: string("a"),
            span: Span::EMPTY,
        }),
        else_branch: Some(Box::new(Statement::Block {
            statements: vec![set_variable("@x", AssignOp::Set, integer("2"))],
            span: Span::EMPTY,
        })),
        span: Span::EMPTY,
    };
    assert_eq!(
        if_statement.to_string(),
        "IF @x = 1 PRINT 'a' ELSE BEGIN SET @x = 2 END"
    );
    let while_statement = Statement::While {
        condition: binary(BinaryOp::Lt, variable("@x"), integer("3")),
        body: Box::new(Statement::Block {
            statements: vec![Statement::Break(Span::EMPTY)],
            span: Span::EMPTY,
        }),
        span: Span::EMPTY,
    };
    assert_eq!(while_statement.to_string(), "WHILE @x < 3 BEGIN BREAK END");
    assert_eq!(
        Statement::Return {
            value: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "RETURN"
    );
    assert_eq!(
        Statement::Return {
            value: Some(integer("1")),
            span: Span::EMPTY,
        }
        .to_string(),
        "RETURN 1"
    );
    assert_eq!(Statement::Continue(Span::EMPTY).to_string(), "CONTINUE");

    assert_eq!(
        Statement::BeginTransaction {
            name: Some(ident("t1")),
            mark: None,
            span: Span::EMPTY,
        }
        .to_string(),
        "BEGIN TRANSACTION t1"
    );
    assert_eq!(
        Statement::Rollback {
            name: Some(ident("t1")),
            span: Span::EMPTY,
        }
        .to_string(),
        "ROLLBACK TRANSACTION t1"
    );

    let execute = ExecuteStatement {
        target: ExecuteTarget::Procedure(qualified("dbo", "p")),
        args: vec![
            ExecuteArg {
                name: Some("@a".to_owned()),
                value: integer("1"),
                output: false,
            },
            ExecuteArg {
                name: None,
                value: variable("@b"),
                output: true,
            },
        ],
        return_into: None,
        implicit: false,
        span: Span::EMPTY,
    };
    assert_eq!(execute.to_string(), "EXECUTE dbo.p @a = 1, @b OUTPUT");
    let execute_text = ExecuteStatement {
        target: ExecuteTarget::Literal(Box::new(string("SELECT 1"))),
        args: Vec::new(),
        return_into: None,
        implicit: false,
        span: Span::EMPTY,
    };
    assert_eq!(execute_text.to_string(), "EXECUTE ('SELECT 1')");
}

#[test]
fn display_batch_separator() {
    let batch = Batch {
        statements: vec![
            Statement::Select(Box::new(select_literal("1"))),
            Statement::Select(Box::new(select_literal("2"))),
        ],
    };
    assert_eq!(batch.to_string(), "SELECT 1;\nSELECT 2");
}

#[test]
fn display_block() {
    let block = Statement::Block {
        statements: vec![
            set_variable("@x", AssignOp::Set, integer("1")),
            set_variable("@y", AssignOp::Set, integer("2")),
        ],
        span: Span::EMPTY,
    };
    assert_eq!(block.to_string(), "BEGIN SET @x = 1; SET @y = 2 END");
    let empty = Statement::Block {
        statements: Vec::new(),
        span: Span::EMPTY,
    };
    assert_eq!(empty.to_string(), "BEGIN END");
}

#[test]
fn display_v2_v3_variants_are_covered() {
    // The exact rendering of these variants is not frozen: the grammar that produces
    // them will fix it. What is checked here is that `Display` covers them.
    let try_catch = Statement::TryCatch {
        try_block: vec![Statement::Select(Box::new(select_literal("1")))],
        catch_block: vec![Statement::Print {
            expr: string("a"),
            span: Span::EMPTY,
        }],
        span: Span::EMPTY,
    };
    assert!(try_catch.to_string().starts_with("BEGIN TRY "));

    let merge = Statement::Merge(Box::new(MergeStatement {
        target: table("t"),
        source: table("u"),
        on: binary(BinaryOp::Eq, integer("1"), integer("1")),
        clauses: vec![MergeClause::MatchedDelete { condition: None }],
        output: None,
        span: Span::EMPTY,
    }));
    assert!(merge.to_string().starts_with("MERGE "));

    let procedure = Statement::CreateProcedure(Box::new(CreateProcedureStatement {
        name: qualified("dbo", "p"),
        params: Vec::new(),
        body: vec![Statement::Select(Box::new(select_literal("1")))],
        or_alter: false,
        with_options: Vec::new(),
        span: Span::EMPTY,
    }));
    assert!(procedure.to_string().starts_with("CREATE PROCEDURE "));

    let mut with_cte = select_literal("1");
    with_cte.with = Some(With {
        recursive_allowed: false,
        ctes: vec![CommonTableExpr {
            name: ident("c"),
            columns: vec![ident("a")],
            query: Box::new(select_literal("1")),
            span: Span::EMPTY,
        }],
        span: Span::EMPTY,
    });
    assert!(with_cte.to_string().starts_with("WITH "));
}
