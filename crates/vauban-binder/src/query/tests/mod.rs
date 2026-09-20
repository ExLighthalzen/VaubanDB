use super::{
    TABLE_HINT_WORDS, column_name, dotted, is_null_literal, is_table_hint, lone_hint_word,
    reread_table_arguments,
};
use crate::bound::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::context::{BindContext, SessionOptions};
use vauban_catalog::ColumnId;
use vauban_parser::{
    Expr, Ident, ObjectName, ParseOptions, QueryBody, Span, Statement, TableRef, parse_batch,
};
use vauban_types::{SqlType, TypeInfo, Value};

fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

/// An expression without an alias has no name; a column reference keeps the name it
/// was written with, parentheses and delimiters looked through, and the alias wins over
/// both. The four spellings of the module documentation of [`column_name`].
#[test]
fn a_column_keeps_its_written_name_and_an_expression_has_none() {
    let literal = BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(1)),
        ty: TypeInfo::new(SqlType::Int, false),
        line: 1,
    };
    let one = expr_of("SELECT 1");
    assert_eq!(column_name(&one, &literal, None), "");
    assert_eq!(column_name(&one, &literal, Some(&ident("n"))), "n");

    let reference = BoundExpr {
        kind: BoundExprKind::ColumnRef(ColumnBinding {
            column: ColumnId(1),
            index: 0,
            name: "a".to_owned(),
            ty: TypeInfo::new(SqlType::Int, false),
            depth: 0,
        }),
        ty: TypeInfo::new(SqlType::Int, false),
        line: 1,
    };
    for (text, expected) in [
        ("SELECT a FROM t", "a"),
        ("SELECT A FROM t", "A"),
        ("SELECT [a] FROM t", "a"),
        ("SELECT (a) FROM t", "a"),
        ("SELECT t.a FROM t", "a"),
        ("SELECT +a FROM t", "a"),
        ("SELECT + +a FROM t", "a"),
        ("SELECT +(a) FROM t", "a"),
    ] {
        let written = expr_of(text);
        assert_eq!(column_name(&written, &reference, None), expected, "{text}");
    }
    // The alias still wins, and a node that did not bind to a column has no name even
    // when it was written as one (`SELECT USER FROM dbo.t`).
    let written = expr_of("SELECT a FROM t");
    assert_eq!(column_name(&written, &reference, Some(&ident("x"))), "x");
    assert_eq!(column_name(&written, &literal, None), "");
    // `-a` and `~a` are not column references, written or bound: the unary plus is the
    // one `written_column` looks through.
    for text in ["SELECT -a FROM t", "SELECT ~a FROM t"] {
        let written = expr_of(text);
        assert_eq!(column_name(&written, &reference, None), "", "{text}");
    }
}

/// The expression of the first select item of `text`.
#[track_caller]
fn expr_of(text: &str) -> Expr {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let Statement::Select(select) = &batch.statements[0] else {
        panic!("not a SELECT: {text}");
    };
    let QueryBody::Select(spec) = &select.body else {
        panic!("not a plain SELECT: {text}");
    };
    let vauban_parser::SelectItem::Expr { expr, .. } = &spec.items[0] else {
        panic!("not an expression: {text}");
    };
    expr.clone()
}

#[test]
fn dotted_joins_the_parts_that_were_written() {
    let mut name = ObjectName {
        server: None,
        database: None,
        schema: None,
        name: ident("t"),
        span: Span::EMPTY,
    };
    assert_eq!(dotted(&name), "t");
    name.schema = Some(ident("dbo"));
    assert_eq!(dotted(&name), "dbo.t");
    name.database = Some(ident("db"));
    name.server = Some(ident("srv"));
    assert_eq!(dotted(&name), "srv.db.dbo.t");
}

#[test]
fn only_the_null_literal_is_a_null_literal() {
    let null = BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty: TypeInfo::new(SqlType::Int, true),
        line: 1,
    };
    assert!(is_null_literal(&null));
    let one = BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(1)),
        ty: TypeInfo::new(SqlType::Int, false),
        line: 1,
    };
    assert!(!is_null_literal(&one));
}

/// The first table reference of the `FROM` of `text`, a `SELECT`.
fn first_table_ref(text: &str) -> TableRef {
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let Statement::Select(select) = &batch.statements[0] else {
        panic!("not a SELECT: {text}");
    };
    let QueryBody::Select(spec) = &select.body else {
        panic!("not a plain SELECT: {text}");
    };
    spec.from[0].clone()
}

/// Re-reads the `nom(…)` written in `text`, and returns its re-serialisation.
fn reread(text: &str) -> String {
    // `TableRef` implements `Drop`, which forbids moving a field out of one by pattern
    // matching (E0509): it is read through a borrow instead.
    let table = first_table_ref(text);
    let TableRef::Function {
        name,
        args,
        alias,
        span,
    } = &table
    else {
        panic!("not a nom(…): {text}");
    };
    let ctx = BindContext::scalar(text, SessionOptions::default());
    reread_table_arguments(name, args, alias.as_ref(), span, &ctx)
        .expect("the arguments are a hint")
        .to_string()
}

/// `FROM t (NOLOCK)` is `FROM t WITH (NOLOCK)`: the re-read reference re-serialises
/// exactly like the one the parser builds from the `WITH` spelling, alias included.
#[test]
fn a_lone_hint_word_is_the_hint_list_of_the_table() {
    let with = first_table_ref("SELECT 1 FROM t WITH (NOLOCK)").to_string();
    assert_eq!(with, "t WITH (NOLOCK)");
    assert_eq!(reread("SELECT 1 FROM t (NOLOCK)"), with);
    assert_eq!(reread("SELECT 1 FROM t(NOLOCK)"), with);
    assert_eq!(reread("SELECT 1 FROM t ((NOLOCK))"), with);
    assert_eq!(reread("SELECT 1 FROM t (((NOLOCK)))"), with);
    assert_eq!(reread("SELECT 1 FROM t ([NOLOCK])"), with);
    assert_eq!(reread("SELECT 1 FROM t (\"NOLOCK\")"), with);

    let with_alias = first_table_ref("SELECT 1 FROM t AS z WITH (NOLOCK)").to_string();
    assert_eq!(with_alias, "t AS z WITH (NOLOCK)");
    assert_eq!(reread("SELECT 1 FROM t (NOLOCK) AS z"), with_alias);
    assert_eq!(reread("SELECT 1 FROM t (NOLOCK) z"), with_alias);

    // The name is kept as written, and so is the word.
    assert_eq!(
        reread("SELECT 1 FROM [dbo].[t] (nolock)"),
        first_table_ref("SELECT 1 FROM [dbo].[t] WITH (nolock)").to_string()
    );
    assert_eq!(
        reread("SELECT 1 FROM dbo.t (readpast)"),
        "dbo.t WITH (readpast)"
    );
}

#[test]
fn every_hint_word_is_matched_ignoring_case() {
    for word in TABLE_HINT_WORDS {
        assert!(is_table_hint(word), "{word}");
        assert!(is_table_hint(&word.to_ascii_lowercase()), "{word}");
    }
    assert!(!is_table_hint("x"));
    assert!(!is_table_hint("FASTFIRSTROW"));
    assert!(!is_table_hint("INDEX"));
    assert!(!is_table_hint("NO LOCK"));
}

/// A bare, unqualified hint word — alone — is a hint; the rest are arguments.
#[test]
fn what_is_not_a_lone_hint_word() {
    let args_of = |text: &str| {
        // Borrowed as in `reread` above, then cloned, because the reference itself
        // does not outlive the closure.
        let table = first_table_ref(text);
        let TableRef::Function { args, .. } = &table else {
            panic!("not a nom(…): {text}");
        };
        args.clone()
    };
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t (NOLOCK)")[0]).is_some());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t ((NOLOCK))")[0]).is_some());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t (x)")[0]).is_none());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t (1)")[0]).is_none());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t ('NOLOCK')")[0]).is_none());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t (@p)")[0]).is_none());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t (dbo.NOLOCK)")[0]).is_none());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t (NOLOCK + 0)")[0]).is_none());
    assert!(lone_hint_word(&args_of("SELECT 1 FROM t ([NO LOCK])")[0]).is_none());
    // Two words are two arguments, whatever they spell.
    let two = args_of("SELECT 1 FROM t (NOLOCK, READPAST)");
    assert_eq!(two.len(), 2);
}
