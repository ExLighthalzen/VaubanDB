//! The `SELECT` statement and the cutting of a batch into statements: the select list,
//! `FROM`, the joins, the filtering and grouping clauses, the sort and the set operators.
//!
//! Everything here goes through the public entry point of the crate, `parse_batch`: what
//! a client sends is a batch, and the shape of the AST it yields is what `binder` and
//! `executor` are written against.
//!
//! # The `parse` -> `Display` -> `parse` loop
//!
//! [`rt`] is the loop the module README makes a contract: for any accepted text, parsing
//! what `Display` wrote yields an **equal** `Batch`. The text `Display` writes is not the
//! source text, and the assumed deviations (an alias written as a string, the `ALL` of a
//! `SELECT ALL`) are asserted here as deviations rather than hidden.

use vauban_errors::SqlError;
use vauban_parser::{
    AliasStyle, ApplyKind, Batch, ColumnRef, Expr, Ident, JoinKind, Literal, ParseOptions,
    QueryBody, QuerySpec, SelectItem, SelectStatement, SetOp, Span, Statement, TableRef,
    parse_batch,
};

/// Parses `text`, which must parse.
fn p(text: &str) -> Batch {
    parse_batch(text, &ParseOptions::default()).unwrap()
}

/// Checks the `parse` -> `Display` -> `parse` loop on `text` and returns what `Display`
/// wrote.
fn rt(text: &str) -> String {
    let batch = p(text);
    let printed = batch.to_string();
    assert_eq!(batch, p(&printed), "{text} was printed as {printed}");
    printed
}

/// The error of a text that must **not** parse.
fn p_err(text: &str) -> SqlError {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
        Err(error) => error,
    }
}

/// The one query specification of a batch that holds one `SELECT`.
fn spec(text: &str) -> QuerySpec {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    // `QueryBody` implements `Drop`, which forbids moving the boxed
    // specification out of it by pattern matching (E0509): this helper hands out a clone of
    // it instead. The tests below read that clone, they do not put it back.
    match statements.pop() {
        Some(Statement::Select(select)) => match &select.body {
            QueryBody::Select(spec) => spec.as_ref().clone(),
            other => unreachable!("{text} is one specification, got {other:?}"),
        },
        other => unreachable!("{text} is a SELECT, got {other:?}"),
    }
}

/// The items of the one `SELECT` of `text`.
fn items(text: &str) -> Vec<SelectItem> {
    spec(text).items
}

/// The alias and the style of the one item of `text`.
fn alias(text: &str) -> (Option<Ident>, AliasStyle) {
    match items(text).pop() {
        Some(SelectItem::Expr {
            alias, alias_style, ..
        }) => (alias, alias_style),
        other => unreachable!("{text} has one expression item, got {other:?}"),
    }
}

/// An integer literal expression, for comparison against a parsed one.
fn integer(text: &str) -> Expr {
    Expr::Literal(Literal::Integer(text.to_owned()), Span::EMPTY)
}

/// An unqualified, unquoted column reference, for comparison against a parsed one.
fn column(name: &str) -> Expr {
    Expr::Column(ColumnRef {
        qualifier: None,
        name: Ident {
            value: name.to_owned(),
            quoted: false,
        },
        span: Span::EMPTY,
    })
}

/// The `FROM` clause of the one `SELECT` of `text`.
fn from(text: &str) -> Vec<TableRef> {
    spec(text).from
}

/// The one table reference of the `FROM` clause of `text`.
fn one_table_ref(text: &str) -> TableRef {
    let mut refs = from(text);
    assert_eq!(refs.len(), 1, "{text} has one table reference");
    match refs.pop() {
        Some(table_ref) => table_ref,
        None => unreachable!("{text} has one table reference"),
    }
}

/// The one `SELECT` statement of `text`, with the clauses that wrap its body.
fn statement(text: &str) -> SelectStatement {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(Statement::Select(select)) => *select,
        other => unreachable!("{text} is a SELECT, got {other:?}"),
    }
}

/// The name of a table reference, its four parts joined by dots, and its alias.
fn table_parts(table_ref: &TableRef) -> (String, Option<String>) {
    match table_ref {
        TableRef::Table { name, alias, .. } => {
            let mut parts = Vec::new();
            for part in [&name.server, &name.database, &name.schema]
                .into_iter()
                .flatten()
            {
                parts.push(part.value.clone());
            }
            parts.push(name.name.value.clone());
            (parts.join("."), alias.as_ref().map(|a| a.value.clone()))
        }
        other => unreachable!("a named table, got {other:?}"),
    }
}

/// The kind of a join and whether it carries an `ON`.
fn join_kind(text: &str) -> (JoinKind, bool) {
    // `TableRef` implements `Drop`, which forbids moving a field out of one
    // by pattern matching (E0509): the reference is read through a borrow here and below.
    match &one_table_ref(text) {
        TableRef::Join { kind, on, .. } => (*kind, on.is_some()),
        other => unreachable!("{text} is a join, got {other:?}"),
    }
}

#[test]
fn select_constant() {
    let batch = p("SELECT 1");
    assert_eq!(batch.statements.len(), 1);
    assert!(matches!(batch.statements[0], Statement::Select(_)));

    let spec = spec("SELECT 1");
    assert!(!spec.distinct);
    assert!(spec.top.is_none());
    assert!(spec.into.is_none());
    assert!(spec.from.is_empty(), "a SELECT without FROM has no source");
    assert!(spec.where_.is_none());
    assert!(spec.group_by.is_empty());
    assert!(spec.having.is_none());
    match &spec.items[..] {
        [SelectItem::Expr { expr, alias, .. }] => {
            assert_eq!(*expr, integer("1"));
            assert!(alias.is_none());
        }
        other => unreachable!("one item without alias, got {other:?}"),
    }

    assert_eq!(rt("SELECT 1"), "SELECT 1");
}

#[test]
fn select_several_items() {
    let text = "SELECT 1, 'a', @@ROWCOUNT, GETDATE(), 1 + 2 * 3";
    let items = items(text);
    assert_eq!(items.len(), 5);
    // In the order they were written.
    assert!(matches!(&items[0], SelectItem::Expr { expr, .. } if *expr == integer("1")));
    assert!(matches!(
        &items[1],
        SelectItem::Expr {
            expr: Expr::Literal(Literal::Str { .. }, _),
            ..
        }
    ));
    assert!(matches!(
        &items[2],
        SelectItem::Expr {
            expr: Expr::Variable { name, .. },
            ..
        } if name == "@@ROWCOUNT"
    ));
    assert!(matches!(
        &items[3],
        SelectItem::Expr {
            expr: Expr::Function { .. },
            ..
        }
    ));
    assert!(matches!(
        &items[4],
        SelectItem::Expr {
            expr: Expr::Binary { .. },
            ..
        }
    ));
    assert_eq!(rt(text), text);
}

#[test]
fn select_aliases() {
    let named = |text: &str| match alias(text) {
        (Some(name), style) => (name, style),
        (None, _) => unreachable!("{text} has an alias"),
    };

    let (name, style) = named("SELECT 1 AS n");
    assert_eq!(name.value, "n");
    assert!(!name.quoted);
    assert_eq!(style, AliasStyle::As);

    let (name, style) = named("SELECT 1 n");
    assert_eq!(name.value, "n");
    assert_eq!(style, AliasStyle::Bare);

    let (name, style) = named("SELECT n = 1");
    assert_eq!(name.value, "n");
    assert_eq!(style, AliasStyle::Equals);
    // The alias is the name, the expression is what follows the `=`.
    assert!(matches!(
        &items("SELECT n = 1")[0],
        SelectItem::Expr { expr, .. } if *expr == integer("1")
    ));

    // A word T-SQL does not reserve is a legal bare alias; a reserved one never is, which
    // is what keeps `SELECT 1 SELECT 2` a batch of two statements.
    let (name, style) = named("SELECT 1 sum");
    assert_eq!(name.value, "sum");
    assert_eq!(style, AliasStyle::Bare);

    let (name, style) = named("SELECT 1 [my col]");
    assert_eq!(name.value, "my col");
    assert!(name.quoted);
    assert_eq!(style, AliasStyle::Bare);

    // SQL Server accepts a character string as an alias and treats it as a delimited
    // name, which is what `quoted` records.
    let (name, style) = named("SELECT 1 'litteral'");
    assert_eq!(name.value, "litteral");
    assert!(name.quoted);
    assert_eq!(style, AliasStyle::Bare);
    let (name, _) = named("SELECT 1 AS 'litteral'");
    assert_eq!(name.value, "litteral");
    assert!(name.quoted);

    // SQL Server also accepts a character string on the **left** of an `=`, and treats
    // it there as the delimited name of the alias.
    let (name, style) = named("SELECT 'mon alias' = 1");
    assert_eq!(name.value, "mon alias");
    assert!(name.quoted);
    assert_eq!(style, AliasStyle::Equals);
    assert_eq!(rt("SELECT 'mon alias' = 1"), "SELECT [mon alias] = 1");

    for text in [
        "SELECT 1 AS n",
        "SELECT 1 n",
        "SELECT n = 1",
        "SELECT 1 sum",
        "SELECT 1 [my col]",
    ] {
        assert_eq!(rt(text), text);
    }
    // Assumed deviation: `Display` only knows that the name was quoted, so it writes it
    // between brackets. Only the AST goes round unchanged.
    assert_eq!(rt("SELECT 1 'litteral'"), "SELECT 1 [litteral]");
    assert_eq!(rt("SELECT 1 AS 'litteral'"), "SELECT 1 AS [litteral]");
}

#[test]
fn select_star() {
    assert!(matches!(&items("SELECT *")[0], SelectItem::Wildcard(_)));

    match &items("SELECT t.*")[0] {
        SelectItem::QualifiedWildcard(name) => {
            assert_eq!(name.name.value, "t");
            assert!(name.schema.is_none());
        }
        other => unreachable!("a qualified wildcard, got {other:?}"),
    }
    match &items("SELECT dbo.t.*")[0] {
        SelectItem::QualifiedWildcard(name) => {
            assert_eq!(name.name.value, "t");
            assert_eq!(
                name.schema.as_ref().map(|part| part.value.as_str()),
                Some("dbo")
            );
            assert!(name.database.is_none());
        }
        other => unreachable!("a two-part qualifier, got {other:?}"),
    }

    // A wildcard mixes with an expression.
    let mixed = items("SELECT *, 1");
    assert_eq!(mixed.len(), 2);
    assert!(matches!(mixed[0], SelectItem::Wildcard(_)));
    assert!(matches!(mixed[1], SelectItem::Expr { .. }));

    for text in ["SELECT *", "SELECT t.*", "SELECT dbo.t.*", "SELECT *, 1"] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn select_distinct() {
    assert!(spec("SELECT DISTINCT 1").distinct);
    assert!(!spec("SELECT 1").distinct);
    // `ALL` is the default and the AST does not record it.
    assert!(!spec("SELECT ALL 1").distinct);

    assert_eq!(rt("SELECT DISTINCT 1"), "SELECT DISTINCT 1");
    // Assumed deviation: `SELECT ALL 1` comes back as `SELECT 1`, and the two texts have
    // the very same AST.
    assert_eq!(rt("SELECT ALL 1"), "SELECT 1");
    assert_eq!(p("SELECT ALL 1"), p("SELECT 1"));
}

#[test]
fn select_top() {
    let top = |text: &str| match spec(text).top {
        Some(top) => top,
        None => unreachable!("{text} has a TOP clause"),
    };

    let bare = top("SELECT TOP 5 1");
    assert_eq!(bare.expr, integer("5"));
    assert!(!bare.parenthesized);
    assert!(!bare.percent);
    assert!(!bare.with_ties);

    let parenthesised = top("SELECT TOP (5) 1");
    assert_eq!(parenthesised.expr, integer("5"));
    assert!(parenthesised.parenthesized);

    let percent = top("SELECT TOP (@n) PERCENT 1");
    assert!(matches!(percent.expr, Expr::Variable { .. }));
    assert!(percent.parenthesized);
    assert!(percent.percent);
    assert!(!percent.with_ties);

    let ties = top("SELECT TOP (5) WITH TIES 1");
    assert!(ties.parenthesized);
    assert!(!ties.percent);
    assert!(ties.with_ties);

    // `WITH TIES` without an `ORDER BY` is the error 1033 of SQL Server; the parser
    // accepts it and the binder is the stage that refuses it.
    let both = top("SELECT TOP 5 PERCENT WITH TIES 1");
    assert!(!both.parenthesized);
    assert!(both.percent);
    assert!(both.with_ties);

    // The row count of an unparenthesised `TOP` is a **constant** in T-SQL, and not a
    // full expression: the `*` of `SELECT TOP 5 * FROM t` opens
    // the select list and is never read as a multiplication.
    assert!(matches!(
        &items("SELECT TOP 5 *")[..],
        [SelectItem::Wildcard(_)]
    ));
    // A variable therefore needs the parenthesised form, exactly as in SQL Server.
    assert_eq!(p_err("SELECT TOP @n 1").number, 102);

    for text in [
        "SELECT TOP 5 1",
        "SELECT TOP (5) 1",
        "SELECT TOP (@n) PERCENT 1",
        "SELECT TOP (5) WITH TIES 1",
        "SELECT TOP 5 PERCENT WITH TIES 1",
        "SELECT TOP 5 *",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn select_variable_assignment() {
    match &items("SELECT @x = 1")[0] {
        SelectItem::Expr { expr, alias, .. } => {
            match expr {
                Expr::Assign { target, value, .. } => {
                    assert_eq!(target, "@x");
                    assert_eq!(**value, integer("1"));
                }
                other => unreachable!("an assignment, got {other:?}"),
            }
            assert!(alias.is_none(), "an assignment is not an alias");
        }
        other => unreachable!("one expression item, got {other:?}"),
    }

    let two = items("SELECT @x = 1, @y = 'a'");
    assert_eq!(two.len(), 2);
    assert!(two.iter().all(|item| matches!(
        item,
        SelectItem::Expr {
            expr: Expr::Assign { .. },
            ..
        }
    )));

    // An assignment takes no alias: SQL Server answers a 102 near 'n' to `SELECT @x = 1
    // n`, so the word ends the statement and opens none.
    assert_eq!(p_err("SELECT @x = 1 n").number, 102);

    assert_eq!(rt("SELECT @x = 1"), "SELECT @x = 1");
    assert_eq!(rt("SELECT @x = 1, @y = 'a'"), "SELECT @x = 1, @y = 'a'");
    // The two spellings print the same way and are not the same thing: `x = 1` names a
    // column, `@x = 1` assigns a variable.
    assert_eq!(rt("SELECT x = 1"), "SELECT x = 1");
    assert_ne!(p("SELECT @x = 1"), p("SELECT x = 1"));
}

/// `ORDER BY` sorts the whole query, so it hangs from the statement and not from the
/// specification. This test only proves that the clause is read and printed back where
/// the AST expects it.
#[test]
fn select_order_by() {
    let batch = p("SELECT 1 ORDER BY 1 DESC");
    match &batch.statements[..] {
        [Statement::Select(select)] => {
            assert_eq!(select.order_by.len(), 1);
            assert!(select.order_by[0].desc);
            assert!(select.order_by[0].explicit_direction);
        }
        other => unreachable!("one SELECT, got {other:?}"),
    }
    assert_eq!(rt("SELECT 1 ORDER BY 1 DESC"), "SELECT 1 ORDER BY 1 DESC");
    assert!(p("SELECT 1").statements.len() == 1);
}

#[test]
fn batch_separators() {
    assert_eq!(p("SELECT 1; SELECT 2").statements.len(), 2);
    // A trailing `;` is optional, and so is every `;`.
    assert_eq!(p("SELECT 1;").statements.len(), 1);
    assert_eq!(p("SELECT 1;;SELECT 2").statements.len(), 2);
    assert_eq!(p(";;").statements.len(), 0);
    assert_eq!(p("").statements.len(), 0);
    assert_eq!(p("   ").statements.len(), 0);
    assert_eq!(p("-- rien").statements.len(), 0);
    // The `;` is a separator and T-SQL does not require it.
    assert_eq!(p("SELECT 1\nSELECT 2").statements.len(), 2);
}

#[test]
fn batch_display_roundtrip() {
    assert_eq!(rt("SELECT 1; SELECT 2"), "SELECT 1;\nSELECT 2");
    assert_eq!(rt("SELECT 1\nSELECT 2"), "SELECT 1;\nSELECT 2");
}

#[test]
fn select_errors() {
    // A truncated batch ends on a reserved word, and the last token read is reported
    // rather than the empty end of the batch -- with a **102**, never a 156, which is the
    // rule of the end of the text: `SELECT`, `SELECT 1 AS` and `SELECT DISTINCT` answer a
    // 102 near 'SELECT' / 'AS' / 'DISTINCT', on SQL Server as here.
    for (text, named) in [
        ("SELECT", "SELECT"),
        ("SELECT 1 AS", "AS"),
        ("SELECT DISTINCT", "DISTINCT"),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}");
        assert_eq!(
            error.message,
            format!("Syntax error near '{named}'."),
            "{text}"
        );
    }
    // Punctuation is never a keyword: 102.
    assert_eq!(p_err("SELECT ,1").number, 102);
    // `FROM` is consumed and `parse_from_clause` fails on the token that follows it,
    // which is the `;`.
    assert_eq!(p_err("SELECT * FROM;").number, 102);
    // The message is frozen by `tests/syntax_errors.rs`; only the token it names is
    // asserted here.
    assert!(
        p_err("SELECT ,1").message.contains(','),
        "the error names the offending token"
    );
}

/// `GO` is a word of the client, never received by the server: after a first statement
/// it is a plain identifier that opens no statement, hence a syntax error.
///
/// The `SELECT 1;` in front matters: with the implicit `EXEC`, a `GO` alone at
/// the head of a batch becomes an `EXECUTE` of a procedure named `GO`, which is what SQL
/// Server does, and the test would measure something else.
#[test]
fn parse_batch_rejects_go() {
    let error = p_err("SELECT 1;\nGO");
    assert_eq!(error.number, 102);
    assert_eq!(error.line, 2);
}

#[test]
fn from_simple() {
    assert_eq!(
        table_parts(&one_table_ref("SELECT c FROM t")),
        ("t".to_owned(), None)
    );
    assert_eq!(
        table_parts(&one_table_ref("SELECT c FROM dbo.t")),
        ("dbo.t".to_owned(), None)
    );
    assert_eq!(
        table_parts(&one_table_ref("SELECT c FROM db.dbo.t AS x")),
        ("db.dbo.t".to_owned(), Some("x".to_owned()))
    );
    // With or without `AS`, the alias is the same one: unlike a select item, a table
    // reference does not record which spelling was written.
    assert_eq!(
        table_parts(&one_table_ref("SELECT c FROM db.dbo.t x")),
        ("db.dbo.t".to_owned(), Some("x".to_owned()))
    );
    assert!(matches!(
        &one_table_ref("SELECT c FROM t"),
        TableRef::Table { hints, .. } if hints.is_empty()
    ));

    for text in [
        "SELECT c FROM t",
        "SELECT c FROM dbo.t",
        "SELECT c FROM db.dbo.t AS x",
    ] {
        assert_eq!(rt(text), text);
    }
    // Assumed deviation (no `AliasStyle` on a `TableRef`, and `display/mod.rs`): the
    // alias of a table reference is always written back
    // with `AS`. Only the AST goes round unchanged, which is what `rt` checks.
    assert_eq!(
        rt("SELECT c FROM db.dbo.t x"),
        "SELECT c FROM db.dbo.t AS x"
    );
    assert_eq!(
        p("SELECT c FROM db.dbo.t x"),
        p("SELECT c FROM db.dbo.t AS x")
    );
}

/// The comma is the historical cross join and yields one reference per operand, where
/// `CROSS JOIN` yields a single `TableRef::Join`.
#[test]
fn from_comma_list() {
    let refs = from("SELECT * FROM a, b, c");
    assert_eq!(refs.len(), 3);
    assert!(refs.iter().all(|r| matches!(r, TableRef::Table { .. })));
    assert_eq!(rt("SELECT * FROM a, b, c"), "SELECT * FROM a, b, c");
}

#[test]
fn from_joins() {
    assert_eq!(
        join_kind("SELECT * FROM a JOIN b ON a.id = b.id"),
        (JoinKind::Inner, true)
    );
    assert_eq!(
        join_kind("SELECT * FROM a INNER JOIN b ON a.id = b.id"),
        (JoinKind::Inner, true)
    );
    assert_eq!(
        join_kind("SELECT * FROM a LEFT JOIN b ON a.id = b.id"),
        (JoinKind::Left, true)
    );
    assert_eq!(
        join_kind("SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id"),
        (JoinKind::Left, true)
    );
    assert_eq!(
        join_kind("SELECT * FROM a RIGHT JOIN b ON a.id = b.id"),
        (JoinKind::Right, true)
    );
    assert_eq!(
        join_kind("SELECT * FROM a FULL OUTER JOIN b ON a.id = b.id"),
        (JoinKind::Full, true)
    );
    // A `CROSS JOIN` takes no `ON`, and every other join requires one.
    assert_eq!(
        join_kind("SELECT * FROM a CROSS JOIN b"),
        (JoinKind::Cross, false)
    );
    // SQL Server: `SELECT * FROM a JOIN b` => 102.
    assert_eq!(p_err("SELECT * FROM a JOIN b").number, 102);

    for text in [
        "SELECT * FROM a INNER JOIN b ON a.id = b.id",
        "SELECT * FROM a LEFT JOIN b ON a.id = b.id",
        "SELECT * FROM a RIGHT JOIN b ON a.id = b.id",
        "SELECT * FROM a FULL JOIN b ON a.id = b.id",
        "SELECT * FROM a CROSS JOIN b",
    ] {
        assert_eq!(rt(text), text);
    }
    // Assumed deviation: `OUTER` is optional and `JoinKind` does not record it, so
    // `LEFT OUTER JOIN` is written back `LEFT JOIN`. The two texts have the very same AST.
    assert_eq!(
        rt("SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id"),
        "SELECT * FROM a LEFT JOIN b ON a.id = b.id"
    );
    assert_eq!(
        p("SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id"),
        p("SELECT * FROM a LEFT JOIN b ON a.id = b.id")
    );
    // A bare `JOIN` is an inner join, and so writes itself back `INNER JOIN`.
    assert_eq!(
        rt("SELECT * FROM a JOIN b ON a.id = b.id"),
        "SELECT * FROM a INNER JOIN b ON a.id = b.id"
    );
}

#[test]
fn from_joins_are_left_associative() {
    let text = "SELECT * FROM a JOIN b ON 1 = 1 JOIN c ON 2 = 2";
    match &one_table_ref(text) {
        TableRef::Join { left, right, .. } => {
            // `(a JOIN b) JOIN c`, never `a JOIN (b JOIN c)`.
            assert!(
                matches!(**left, TableRef::Join { .. }),
                "the left side is the first join"
            );
            assert_eq!(table_parts(right).0, "c");
        }
        other => unreachable!("{text} is a join, got {other:?}"),
    }
    assert_eq!(
        rt(text),
        "SELECT * FROM a INNER JOIN b ON 1 = 1 INNER JOIN c ON 2 = 2"
    );
}

#[test]
fn from_derived_table() {
    match &one_table_ref("SELECT * FROM (SELECT 1 AS n) AS d") {
        TableRef::Derived { alias, columns, .. } => {
            assert_eq!(
                alias.as_ref().map(|a| a.value.clone()),
                Some("d".to_owned())
            );
            assert!(columns.is_empty());
        }
        other => unreachable!("a derived table, got {other:?}"),
    }
    match &one_table_ref("SELECT * FROM (SELECT 1) d (n)") {
        TableRef::Derived { alias, columns, .. } => {
            assert_eq!(
                alias.as_ref().map(|a| a.value.clone()),
                Some("d".to_owned())
            );
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].value, "n");
        }
        other => unreachable!("a derived table with a column list, got {other:?}"),
    }
    // T-SQL requires the alias of a derived table.
    assert_eq!(p_err("SELECT * FROM (SELECT 1)").number, 102);

    assert_eq!(
        rt("SELECT * FROM (SELECT 1 AS n) AS d"),
        "SELECT * FROM (SELECT 1 AS n) AS d"
    );
    assert_eq!(
        rt("SELECT * FROM (SELECT 1) d (n)"),
        "SELECT * FROM (SELECT 1) AS d (n)"
    );
}

/// Hints are read, kept as they were written and never interpreted. They
/// only ever come from a hint list **position**: `WITH (…)`, or the deprecated `(…)`
/// written after the alias. Parentheses glued to the name are arguments, and live in
/// [`table_arguments_are_a_function_call`].
#[test]
fn from_hints() {
    let hints = |text: &str| match &one_table_ref(text) {
        TableRef::Table { hints, .. } => hints.clone(),
        other => unreachable!("{text} is a named table, got {other:?}"),
    };

    let one = hints("SELECT * FROM t WITH (NOLOCK)");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].name, "NOLOCK");
    assert!(one[0].args.is_empty());

    let two = hints("SELECT * FROM t AS x WITH (NOLOCK, ROWLOCK)");
    assert_eq!(two.len(), 2);
    assert_eq!(two[1].name, "ROWLOCK");

    // The deprecated spelling, which SQL Server still answers `SELECT * FROM dbo.t AS z
    // (NOLOCK)` with two rows for.
    let deprecated = hints("SELECT * FROM t AS z (NOLOCK)");
    assert_eq!(deprecated.len(), 1);
    assert_eq!(deprecated[0].name, "NOLOCK");
    // Without `AS` just the same: `SELECT * FROM dbo.t z (NOLOCK)` returns its rows too.
    assert_eq!(hints("SELECT * FROM t z (NOLOCK)")[0].name, "NOLOCK");

    // An argument list is kept as written, and a reserved word is a legal hint name.
    let indexed = hints("SELECT * FROM t WITH (INDEX(1))");
    assert_eq!(indexed[0].name, "INDEX");
    assert_eq!(indexed[0].args, vec!["1".to_owned()]);

    assert_eq!(
        rt("SELECT * FROM t WITH (NOLOCK)"),
        "SELECT * FROM t WITH (NOLOCK)"
    );
    assert_eq!(
        rt("SELECT * FROM t AS x WITH (NOLOCK, ROWLOCK)"),
        "SELECT * FROM t AS x WITH (NOLOCK, ROWLOCK)"
    );
    assert_eq!(
        rt("SELECT * FROM t WITH (INDEX(1))"),
        "SELECT * FROM t WITH (INDEX(1))"
    );
    // The deprecated spelling writes itself back with `WITH`, and the two texts have the
    // very same AST.
    assert_eq!(
        rt("SELECT * FROM t AS z (NOLOCK)"),
        "SELECT * FROM t AS z WITH (NOLOCK)"
    );
    assert_eq!(
        p("SELECT * FROM t AS z (NOLOCK)"),
        p("SELECT * FROM t AS z WITH (NOLOCK)")
    );
}

/// A `(` glued to a name opens arguments, whatever it holds and whatever the name turns
/// out to be. The parser makes no guess; the binder resolves the name and either calls a
/// function, re-reads the arguments as a hint, or raises 215.
///
/// On SQL Server, `dbo.t` a table and `dbo.f` a table-valued function of one argument:
///
/// ```text
/// SELECT * FROM dbo.t (a);  -- 207 (unknown column a), then 215
/// SELECT * FROM dbo.t (1);  -- 215 (parameters supplied to an object that is not a
///                           --     function; a hint needs the WITH keyword)
/// SELECT * FROM dbo.t ();   -- 215 as well
/// SELECT * FROM dbo.t (NOLOCK);   -- rows: the lone name is re-read as a hint
/// SELECT * FROM dbo.f (NOLOCK);   -- 207 (unknown column NOLOCK)
/// SELECT * FROM dbo.t CROSS APPLY dbo.f(a) AS x;   -- rows: a real call
/// ```
#[test]
fn table_arguments_are_a_function_call() {
    let call = |text: &str| match &one_table_ref(text) {
        TableRef::Function {
            name, args, alias, ..
        } => (name.clone(), args.clone(), alias.clone()),
        other => unreachable!("{text} gives arguments to a name, got {other:?}"),
    };

    // `FROM t (a)`: SQL Server answers 207 then 215, which only a function call explains.
    let (name, args, alias) = call("SELECT * FROM t (a)");
    assert_eq!(name.name.value, "t");
    assert_eq!(args, vec![column("a")]);
    assert!(alias.is_none());

    // A hint word that is no reserved word is an argument like any other: it is the
    // binder that sees a hint in it, and only because `t` is no function.
    assert_eq!(call("SELECT * FROM t (NOLOCK)").1, vec![column("NOLOCK")]);

    // Several arguments, a literal one, and none at all: still arguments.
    assert_eq!(
        call("SELECT * FROM f(1, 2)").1,
        vec![integer("1"), integer("2")]
    );
    assert!(call("SELECT * FROM t ()").1.is_empty());

    // The alias follows the arguments, with or without `AS`.
    assert_eq!(
        call("SELECT * FROM t (NOLOCK) AS z").2.map(|a| a.value),
        Some("z".to_owned())
    );

    // `WITH` is the only spelling left that yields hints on a named table, and the two
    // readings never meet: the binder tells them apart on the variant alone.
    assert!(matches!(
        one_table_ref("SELECT * FROM t WITH (NOLOCK)"),
        TableRef::Table { ref hints, .. } if hints.len() == 1 && hints[0].name == "NOLOCK"
    ));
    assert!(matches!(
        one_table_ref("SELECT * FROM t"),
        TableRef::Table { ref hints, .. } if hints.is_empty()
    ));

    // The loop holds on the five forms above, the deprecated spelling coming back as the
    // call it is read as.
    assert_eq!(rt("SELECT * FROM t (a)"), "SELECT * FROM t(a)");
    assert_eq!(rt("SELECT * FROM t (NOLOCK)"), "SELECT * FROM t(NOLOCK)");
    assert_eq!(rt("SELECT * FROM f(1)"), "SELECT * FROM f(1)");
    assert_eq!(rt("SELECT * FROM t ()"), "SELECT * FROM t()");
    assert_eq!(
        rt("SELECT * FROM a CROSS APPLY f(x)"),
        "SELECT * FROM a CROSS APPLY f(x)"
    );
    assert_eq!(
        rt("SELECT * FROM t WITH (NOLOCK)"),
        "SELECT * FROM t WITH (NOLOCK)"
    );
    // The two readings no longer share an AST, which is the whole point.
    assert_ne!(
        p("SELECT * FROM t (NOLOCK)"),
        p("SELECT * FROM t WITH (NOLOCK)")
    );

    // No hint clause follows a call: SQL Server answers `SELECT * FROM dbo.f(1) WITH
    // (NOLOCK)` with 102 near ')'.
    // SQL Server: `SELECT * FROM f(1) WITH (NOLOCK)` => 102. VaubanDB remains 156.
    assert_eq!(p_err("SELECT * FROM f(1) WITH (NOLOCK)").number, 156);
    // Deviations, all of them VaubanDB accepting what SQL Server refuses -- which the
    // module can afford, never interpreting a hint. Written without `WITH`, SQL Server's
    // list is one word and no argument, and it answers 1018 to the two spellings below;
    // 1018 is not at the catalogue of `errors`, so nothing is raised here.
    // `t (INDEX(1))` even reads as a call of a call, a reserved word being a legal
    // function name.
    assert_eq!(
        rt("SELECT * FROM t (INDEX(1))"),
        "SELECT * FROM t(INDEX(1))"
    );
    assert_eq!(
        rt("SELECT * FROM t AS z (NOLOCK, ROWLOCK)"),
        "SELECT * FROM t AS z WITH (NOLOCK, ROWLOCK)"
    );
    // The one deviation the other way: `SELECT * FROM dbo.t (HOLDLOCK)` returns its rows
    // on SQL Server. `HOLDLOCK` is reserved and heads no call, so it spells no expression
    // here, and reading it as an identifier would have `Display` bracket it and break the
    // loop. The usual hints -- `NOLOCK`, `ROWLOCK`, `TABLOCK`, `READPAST`,
    // `READCOMMITTED` -- are not reserved and go through.
    // SQL Server: `SELECT * FROM t (HOLDLOCK)` => 208. VaubanDB remains 156.
    assert_eq!(p_err("SELECT * FROM t (HOLDLOCK)").number, 156);
}

/// (V2) `APPLY` is accepted by the grammar; the binder is what refuses it for now. The
/// parentheses of `f(a.id)` follow the name, so they hold its arguments -- and so do
/// those of `f(x)`, which used to be read as the hint list `f AS t WITH (x)` while SQL
/// Server answers `SELECT * FROM dbo.t CROSS APPLY dbo.f(a) AS x` with two rows.
#[test]
fn from_apply() {
    let apply = |text: &str| match &one_table_ref(text) {
        TableRef::Apply { kind, right, .. } => (*kind, right.as_ref().clone()),
        other => unreachable!("{text} is an APPLY, got {other:?}"),
    };

    let (kind, right) = apply("SELECT * FROM a CROSS APPLY f(a.id) AS x");
    assert_eq!(kind, ApplyKind::Cross);
    match &right {
        TableRef::Function {
            name, args, alias, ..
        } => {
            assert_eq!(name.name.value, "f");
            assert_eq!(args.len(), 1);
            assert_eq!(
                alias.as_ref().map(|a| a.value.clone()),
                Some("x".to_owned())
            );
        }
        other => unreachable!("a table-valued function, got {other:?}"),
    }

    let (kind, _) = apply("SELECT * FROM a OUTER APPLY f(a.id) AS x");
    assert_eq!(kind, ApplyKind::Outer);

    // A bare identifier as the argument: a call all the same, not a hint named `x`.
    match &apply("SELECT * FROM a CROSS APPLY f(x)").1 {
        TableRef::Function { args, alias, .. } => {
            assert_eq!(*args, vec![column("x")]);
            assert!(alias.is_none());
        }
        other => unreachable!("a call, got {other:?}"),
    }

    for text in [
        "SELECT * FROM a CROSS APPLY f(a.id) AS x",
        "SELECT * FROM a OUTER APPLY f(a.id) AS x",
        "SELECT * FROM a CROSS APPLY f(x)",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn where_group_having() {
    let text = "SELECT a, COUNT(*) FROM t WHERE a > 1 GROUP BY a HAVING COUNT(*) > 2";
    let grouped = spec(text);
    assert!(grouped.where_.is_some());
    assert_eq!(grouped.group_by.len(), 1);
    assert!(grouped.having.is_some());
    assert_eq!(spec("SELECT a, b FROM t GROUP BY a, b").group_by.len(), 2);
    // A `WHERE` needs no `FROM`.
    assert!(spec("SELECT 1 WHERE 1 = 1").where_.is_some());

    for text in [
        text,
        "SELECT a, b FROM t GROUP BY a, b",
        "SELECT 1 WHERE 1 = 1",
        "SELECT a FROM t HAVING COUNT(*) > 2",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn select_into() {
    let into = |text: &str| match spec(text).into {
        Some(name) => name,
        None => unreachable!("{text} has an INTO"),
    };
    assert_eq!(into("SELECT * INTO #tmp FROM t").name.value, "#tmp");
    let qualified = into("SELECT * INTO dbo.t2 FROM t");
    assert_eq!(qualified.name.value, "t2");
    assert_eq!(
        qualified.schema.map(|part| part.value),
        Some("dbo".to_owned())
    );
    assert!(spec("SELECT * FROM t").into.is_none());

    for text in ["SELECT * INTO #tmp FROM t", "SELECT * INTO dbo.t2 FROM t"] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn order_by() {
    let items = |text: &str| statement(text).order_by;

    let implicit = items("SELECT * FROM t ORDER BY a");
    assert_eq!(implicit.len(), 1);
    assert!(!implicit[0].desc);
    assert!(!implicit[0].explicit_direction);

    let ascending = items("SELECT * FROM t ORDER BY a ASC");
    assert!(!ascending[0].desc);
    assert!(ascending[0].explicit_direction);

    let two = items("SELECT * FROM t ORDER BY a DESC, b");
    assert_eq!(two.len(), 2);
    assert!(two[0].desc);
    assert!(!two[1].explicit_direction);

    // `ORDER BY 1` sorts by position, and the AST keeps the literal as written: turning
    // a position into a column is the binder's business.
    assert_eq!(items("SELECT * FROM t ORDER BY 1")[0].expr, integer("1"));

    // The collation of a sort item is kept in the clause, not in the expression.
    let collated = items("SELECT * FROM t ORDER BY a COLLATE Latin1_General_CI_AS");
    assert_eq!(collated[0].collate.as_deref(), Some("Latin1_General_CI_AS"));
    assert!(matches!(collated[0].expr, Expr::Column(_)));

    // A query without an `ORDER BY` has an empty one.
    assert!(statement("SELECT * FROM t").order_by.is_empty());

    for text in [
        "SELECT * FROM t ORDER BY a",
        "SELECT * FROM t ORDER BY a ASC",
        "SELECT * FROM t ORDER BY a DESC, b",
        "SELECT * FROM t ORDER BY 1",
        "SELECT * FROM t ORDER BY a COLLATE Latin1_General_CI_AS",
        "SELECT * FROM t ORDER BY a COLLATE Latin1_General_CI_AS DESC",
    ] {
        assert_eq!(rt(text), text);
    }
}

/// (V3) `OFFSET`/`FETCH` is accepted and produced; `ROW`/`ROWS` and `FIRST`/`NEXT` are
/// interchangeable and the form written is restored.
#[test]
fn offset_fetch() {
    let clause = |text: &str| match statement(text).offset_fetch {
        Some(offset_fetch) => offset_fetch,
        None => unreachable!("{text} has an OFFSET"),
    };

    let offset_only = clause("SELECT * FROM t ORDER BY a OFFSET 10 ROWS");
    assert_eq!(offset_only.offset, integer("10"));
    assert!(offset_only.fetch.is_none());
    assert!(!offset_only.rows_singular);
    assert!(!offset_only.fetch_first);

    let fetched = clause("SELECT * FROM t ORDER BY a OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY");
    assert_eq!(fetched.fetch, Some(integer("5")));
    assert!(!fetched.fetch_first);

    let singular = clause("SELECT * FROM t ORDER BY a OFFSET 1 ROW FETCH FIRST 1 ROW ONLY");
    assert!(singular.rows_singular);
    assert!(singular.fetch_first);

    assert!(
        statement("SELECT * FROM t ORDER BY a")
            .offset_fetch
            .is_none()
    );

    for text in [
        "SELECT * FROM t ORDER BY a OFFSET 10 ROWS",
        "SELECT * FROM t ORDER BY a OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY",
        "SELECT * FROM t ORDER BY a OFFSET 1 ROW FETCH FIRST 1 ROW ONLY",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn set_operations() {
    let set_op = |text: &str| match statement(text).body {
        QueryBody::SetOp { op, all, .. } => (op, all),
        other => unreachable!("{text} is a set operation, got {other:?}"),
    };

    assert_eq!(set_op("SELECT 1 UNION SELECT 2"), (SetOp::Union, false));
    assert_eq!(set_op("SELECT 1 UNION ALL SELECT 2"), (SetOp::Union, true));
    assert_eq!(set_op("SELECT 1 EXCEPT SELECT 2"), (SetOp::Except, false));
    assert_eq!(
        set_op("SELECT 1 INTERSECT SELECT 2"),
        (SetOp::Intersect, false)
    );

    for text in [
        "SELECT 1 UNION SELECT 2",
        "SELECT 1 UNION ALL SELECT 2",
        "SELECT 1 EXCEPT SELECT 2",
        "SELECT 1 INTERSECT SELECT 2",
    ] {
        assert_eq!(rt(text), text);
    }
}

/// `INTERSECT` binds tighter than `UNION` and `EXCEPT`, which share one rank and
/// associate to the left.
#[test]
fn set_operation_precedence() {
    // `QueryBody` implements `Drop`, which forbids moving an operand out of
    // one by pattern matching (E0509): the body is read through a borrow here and below.
    match &statement("SELECT 1 UNION SELECT 2 INTERSECT SELECT 3").body {
        QueryBody::SetOp {
            op, left, right, ..
        } => {
            assert_eq!(*op, SetOp::Union);
            assert!(
                matches!(**left, QueryBody::Select(_)),
                "the left side is SELECT 1"
            );
            assert!(
                matches!(
                    **right,
                    QueryBody::SetOp {
                        op: SetOp::Intersect,
                        ..
                    }
                ),
                "the INTERSECT is the right operand of the UNION"
            );
        }
        other => unreachable!("a set operation, got {other:?}"),
    }
    match &statement("SELECT 1 UNION SELECT 2 EXCEPT SELECT 3").body {
        QueryBody::SetOp {
            op, left, right, ..
        } => {
            assert_eq!(*op, SetOp::Except);
            assert!(
                matches!(
                    **left,
                    QueryBody::SetOp {
                        op: SetOp::Union,
                        ..
                    }
                ),
                "same rank: the UNION is the left operand of the EXCEPT"
            );
            assert!(matches!(**right, QueryBody::Select(_)));
        }
        other => unreachable!("a set operation, got {other:?}"),
    }

    for text in [
        "SELECT 1 UNION SELECT 2 INTERSECT SELECT 3",
        "SELECT 1 UNION SELECT 2 EXCEPT SELECT 3",
    ] {
        assert_eq!(rt(text), text);
    }
}

/// An `ORDER BY` sorts the result of the whole set operation, so it hangs from the
/// statement and not from the specification on its right.
#[test]
fn set_operation_order_by_belongs_to_the_query() {
    let statement = statement("SELECT 1 UNION SELECT 2 ORDER BY 1");
    assert_eq!(statement.order_by.len(), 1);
    match &statement.body {
        QueryBody::SetOp { right, .. } => match &**right {
            QueryBody::Select(spec) => {
                assert_eq!(spec.items.len(), 1);
            }
            other => unreachable!("the right operand is a specification, got {other:?}"),
        },
        other => unreachable!("a set operation, got {other:?}"),
    }
    assert_eq!(
        rt("SELECT 1 UNION SELECT 2 ORDER BY 1"),
        "SELECT 1 UNION SELECT 2 ORDER BY 1"
    );
}

#[test]
fn nested_query_body() {
    match &statement("(SELECT 1) UNION (SELECT 2)").body {
        QueryBody::SetOp { left, right, .. } => {
            assert!(matches!(**left, QueryBody::Nested(..)));
            assert!(matches!(**right, QueryBody::Nested(..)));
        }
        other => unreachable!("a set operation, got {other:?}"),
    }
    // The parentheses are kept, since they may change what the operators mean.
    assert_eq!(
        rt("(SELECT 1) UNION (SELECT 2)"),
        "(SELECT 1) UNION (SELECT 2)"
    );
    assert_eq!(rt("(SELECT 1)"), "(SELECT 1)");
    assert_ne!(
        p("(SELECT 1 UNION SELECT 2) INTERSECT SELECT 3"),
        p("SELECT 1 UNION SELECT 2 INTERSECT SELECT 3")
    );
}

#[test]
fn subquery_in_where() {
    let correlated = "SELECT * FROM t WHERE a IN (SELECT b FROM u WHERE u.c = t.c)";
    assert!(spec(correlated).where_.is_some());
    assert_eq!(rt(correlated), correlated);

    let exists = "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u)";
    assert!(spec(exists).where_.is_some());
    assert_eq!(rt(exists), exists);
}

#[test]
fn query_errors() {
    // These truncated batches name their last token and return 102.
    // SQL Server: `SELECT * FROM` => 102.
    assert_eq!(p_err("SELECT * FROM").number, 102);
    // SQL Server: `SELECT * FROM t WHERE` => 102.
    assert_eq!(p_err("SELECT * FROM t WHERE").number, 102);
    // SQL Server: `SELECT * FROM t GROUP BY` => 102.
    assert_eq!(p_err("SELECT * FROM t GROUP BY").number, 102);
    // `PIVOT` is V2: the AST declares it, the grammar reads no such clause, so the word
    // opens no statement and is a syntax error where it stands.
    // SQL Server: `SELECT * PIVOT (1)` => 156.
    assert_eq!(p_err("SELECT * PIVOT (1)").number, 156);
    // A join without its `ON` stops on the last token read, an identifier: a 102.
    assert_eq!(p_err("SELECT * FROM t JOIN u").number, 102);
    // The old outer join operators `*=` and `=*` were removed from SQL Server in 2012.
    assert_eq!(p_err("SELECT * FROM a, b WHERE a.id *= b.id").number, 102);
}

#[test]
fn roundtrip_full_query() {
    let text = "SELECT DISTINCT TOP (10) a.x AS n, COUNT(*) c \
                FROM dbo.a AS a WITH (NOLOCK) \
                LEFT JOIN (SELECT 1 AS y) AS b ON a.x = b.y \
                WHERE a.x > 1 AND a.z IS NOT NULL \
                GROUP BY a.x HAVING COUNT(*) > 1 \
                ORDER BY n DESC, 2 ASC";
    assert_eq!(rt(text), text);
}

/// (V2) A table variable and a table-valued function are accepted by the grammar and
/// refused later by the binder; the `FROM` of the V1 subset does not have to know.
#[test]
fn from_variable_and_function() {
    match &one_table_ref("SELECT * FROM @t AS x") {
        TableRef::Variable { name, alias, .. } => {
            assert_eq!(name, "@t");
            assert_eq!(
                alias.as_ref().map(|a| a.value.clone()),
                Some("x".to_owned())
            );
        }
        other => unreachable!("a table variable, got {other:?}"),
    }
    // The parentheses are glued to the name, so they hold arguments.
    match &one_table_ref("SELECT * FROM f(1) AS t") {
        TableRef::Function { name, args, .. } => {
            assert_eq!(name.name.value, "f");
            assert_eq!(*args, vec![integer("1")]);
        }
        other => unreachable!("a table-valued function, got {other:?}"),
    }
    for text in ["SELECT * FROM @t AS x", "SELECT * FROM f(1) AS t"] {
        assert_eq!(rt(text), text);
    }
}

/// The clauses only parse in the order T-SQL writes them, and a hint sits between the
/// table and the clauses that follow.
#[test]
fn clause_order() {
    let text = "SELECT * FROM t WITH (NOLOCK) WHERE a = 1 GROUP BY a ORDER BY a";
    assert_eq!(rt(text), text);
    // `GROUP BY` before `WHERE` is not T-SQL: the `WHERE` opens no statement.
    // SQL Server: `SELECT * FROM t GROUP BY a WHERE a = 1` => 156.
    assert_eq!(p_err("SELECT * FROM t GROUP BY a WHERE a = 1").number, 156);
    // `OFFSET` without an `ORDER BY` is refused, as SQL Server refuses it.
    // SQL Server: `SELECT * FROM t OFFSET 1 ROWS` => 102.
    assert_eq!(p_err("SELECT * FROM t OFFSET 1 ROWS").number, 102);
}
