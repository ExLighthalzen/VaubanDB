//! The niladic functions (`CURRENT_TIMESTAMP`, `CURRENT_USER`, `SESSION_USER`,
//! `SYSTEM_USER`, `USER`) and the invalid spellings whose diagnostic is deferred to the
//! binder.
use vauban_errors::SqlError;
use vauban_parser::{Expr, ParseOptions, QueryBody, QuerySpec, SelectItem, Statement, parse_batch};

/// The deferred diagnostic the binder would restitute, of the first invalid niladic
/// spelling met in reading order, or `None` when the expression holds none.
fn deferred(expr: &Expr) -> Option<SqlError> {
    match expr {
        Expr::InvalidNiladic {
            diagnostic_token,
            diagnostic_number,
            diagnostic_span,
            ..
        } => Some(if *diagnostic_number == 156 {
            SqlError::incorrect_syntax_near_keyword(diagnostic_token, diagnostic_span.line)
        } else {
            SqlError::incorrect_syntax_near(diagnostic_token, diagnostic_span.line)
        }),
        Expr::Nested(inner, _) | Expr::Cast { expr: inner, .. } => deferred(inner),
        Expr::Binary { left, right, .. } => deferred(left).or_else(|| deferred(right)),
        Expr::Function { args, .. } => args.iter().find_map(deferred),
        Expr::Convert { expr, .. } => deferred(expr),
        Expr::Case { arms, else_, .. } => arms
            .iter()
            .find_map(|arm| deferred(&arm.when).or_else(|| deferred(&arm.then)))
            .or_else(|| else_.as_deref().and_then(deferred)),
        _ => None,
    }
}

/// The query specification of a one-statement batch, or the parse error that stopped it.
fn spec(sql: &str) -> Result<QuerySpec, SqlError> {
    let batch = parse_batch(sql, &ParseOptions::default())?;
    let Statement::Select(select) = &batch.statements[0] else {
        panic!("SELECT expected")
    };
    let QueryBody::Select(spec) = &select.body else {
        panic!("query spec expected")
    };
    Ok(*spec.clone())
}

fn diagnostic(sql: &str) -> SqlError {
    let spec = match spec(sql) {
        Ok(spec) => spec,
        Err(error) => return error,
    };
    let SelectItem::Expr { expr, .. } = &spec.items[0] else {
        panic!("expression expected")
    };
    deferred(expr).unwrap_or_else(|| panic!("expected deferred niladic diagnostic: {expr:?}"))
}

/// The count of a `TOP (n)` clause, which sits outside the projection.
fn top_diagnostic(sql: &str) -> SqlError {
    let top = spec(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")).top;
    let top = top.unwrap_or_else(|| panic!("{sql}: TOP expected"));
    deferred(&top.expr).unwrap_or_else(|| panic!("{sql}: no deferred diagnostic"))
}

#[test]
fn niladic_parentheses_report_the_first_argument_token() {
    for name in [
        "CURRENT_TIMESTAMP",
        "CURRENT_USER",
        "SESSION_USER",
        "SYSTEM_USER",
        "USER",
    ] {
        for (args, number, message) in [
            ("", 102, "Syntax error near ')'."),
            ("1", 102, "Syntax error near '1'."),
            ("1, 2", 102, "Syntax error near '1'."),
            ("NULL", 156, "Syntax error near the keyword 'NULL'."),
        ] {
            let sql = format!("SELECT {name}({args});");
            let error = diagnostic(&sql);
            assert_eq!(
                (error.number, error.severity, error.state, error.line),
                (number, 15, 1, 1),
                "{sql}"
            );
            assert_eq!(error.message, message, "{sql}");
        }
    }
}

#[test]
fn false_niladics_are_reserved_but_delimited_names_parse() {
    for name in [
        "CURRENT_DATE",
        "CURRENT_TIME",
        "current_date",
        "current_time",
        "Current_Date",
        "Current_Time",
    ] {
        for sql in [
            format!("SELECT {name};"),
            format!("SELECT LEN({name});"),
            format!("SELECT 1 AS {name};"),
            format!("SELECT 1 {name};"),
            format!("SELECT {name}();"),
            format!("SELECT t.{name};"),
        ] {
            let error = diagnostic(&sql);
            assert_eq!(
                (error.number, error.severity, error.state, error.line),
                (156, 15, 1, 1),
                "{sql}"
            );
            assert_eq!(
                error.message,
                format!("Syntax error near the keyword '{name}'."),
                "{sql}"
            );
        }
        for sql in [
            format!("SELECT [{name}];"),
            format!("SELECT LEN([{name}]);"),
            format!("SELECT 1 AS [{name}];"),
            format!("SELECT 1 [{name}];"),
            format!("SELECT t.[{name}];"),
        ] {
            let batch = parse_batch(&sql, &ParseOptions::default()).unwrap();
            assert_eq!(
                parse_batch(&batch.to_string(), &ParseOptions::default()).unwrap(),
                batch,
                "{sql}"
            );
        }
    }
}

#[test]
fn deferred_niladic_roundtrips_and_keeps_error_positions() {
    for sql in [
        "SELECT CURRENT_TIMESTAMP();",
        "SELECT [USER](1);",
        "SELECT CURRENT_DATE;",
        "SELECT @x, CURRENT_TIME;",
        "SELECT CURRENT_TIMESTAMP(
(1));",
    ] {
        let batch = parse_batch(sql, &ParseOptions::default()).unwrap();
        assert_eq!(
            batch,
            parse_batch(&batch.to_string(), &ParseOptions::default()).unwrap()
        );
    }
    assert_eq!(
        diagnostic(
            "SELECT CURRENT_TIMESTAMP(
(1));"
        )
        .line,
        2
    );
}

#[test]
fn nested_tokens_and_delimited_calls_move_the_diagnostic() {
    for name in [
        "CURRENT_TIMESTAMP",
        "CURRENT_USER",
        "SESSION_USER",
        "SYSTEM_USER",
        "USER",
    ] {
        for (argument, token, number) in [
            ("(1)", "1", 102),
            ("SELECT", ")", 102),
            ("-1", "-", 102),
            ("DEFAULT", "DEFAULT", 156),
        ] {
            let sql = format!("SELECT {name}({argument});");
            let error = diagnostic(&sql);
            assert_eq!(error.number, number, "{sql}");
            assert!(
                error.message.contains(&format!("'{token}'")),
                "{sql}: {error:?}"
            );
        }
        for argument in ["", "1"] {
            let sql = format!("SELECT [{name}]({argument});");
            let error = diagnostic(&sql);
            let token = if argument.is_empty() { ")" } else { "1" };
            assert_eq!(error.message, format!("Syntax error near '{token}'."));
            let batch = parse_batch(&sql, &ParseOptions::default()).unwrap();
            assert_eq!(
                batch,
                parse_batch(&batch.to_string(), &ParseOptions::default()).unwrap()
            );
        }
    }
    for name in ["CURRENT_TIMESTAMP", "[CURRENT_TIMESTAMP]", "[CURRENT_DATE]"] {
        let sql = format!("SELECT LEN({name}());");
        assert_eq!(diagnostic(&sql).message, "Syntax error near '('.");
    }
    assert_eq!(
        diagnostic("SELECT CURRENT_TIMESTAMP([x]);").message,
        "Syntax error near 'x'."
    );
}

/// A delimited expression around the spelling moves the diagnostic to the call parenthesis:
/// a second statement cannot start inside it, so SQL Server stops on the `(` it cannot
/// read. `LEN((x))`, `LEN(1+x)`, `ABS(x)`, `COALESCE(1,x)`, `CAST(x AS int)`,
/// `CASE … ELSE x END`, `unknown(x)`, `TOP (x)` and `x+@missing` answer that way on SQL
/// Server, for the three written forms below, and so does
/// `SELECT 1 WHERE (CURRENT_TIMESTAMP())=1;`, 102 near `(`.
#[test]
fn a_delimited_context_names_the_call_parenthesis() {
    for name in ["CURRENT_TIMESTAMP", "[CURRENT_TIMESTAMP]", "[CURRENT_DATE]"] {
        for sql in [
            format!("SELECT LEN(({name}()));"),
            format!("SELECT LEN(1+{name}());"),
            format!("SELECT ABS({name}());"),
            format!("SELECT COALESCE(1,{name}());"),
            format!("SELECT CAST({name}() AS int);"),
            format!("SELECT CASE WHEN 1=1 THEN 1 ELSE {name}() END;"),
            format!("SELECT unknown({name}());"),
            format!("SELECT ({name}());"),
        ] {
            let error = diagnostic(&sql);
            assert_eq!(
                (error.number, error.severity, error.state, error.line),
                (102, 15, 1, 1),
                "{sql}"
            );
            assert_eq!(error.message, "Syntax error near '('.", "{sql}");
        }

        // The count of `TOP` sits in the clause and not in the projection, and its own
        // parentheses belong to the clause too.
        let sql = format!("SELECT TOP ({name}()) 1;");
        let error = top_diagnostic(&sql);
        assert_eq!(error.number, 102, "{sql}");
        assert_eq!(error.message, "Syntax error near '('.", "{sql}");

        // Outside any delimiter, the same spelling keeps the token it names on its own.
        let sql = format!("SELECT {name}()+@missing;");
        assert_eq!(diagnostic(&sql).message, "Syntax error near ')'.", "{sql}");
    }

    // The two names written without a call carry 156 into these contexts, `TOP` included:
    // there is no parenthesis of theirs to name. `LEN`, `CAST`, `CASE` and `TOP` are
    // the shapes SQL Server answers with 156; the bare `SELECT (CURRENT_DATE);` follows
    // the three others here.
    for sql in [
        "SELECT LEN(CURRENT_DATE);",
        "SELECT CAST(CURRENT_DATE AS int);",
        "SELECT CASE WHEN 1=1 THEN 1 ELSE CURRENT_DATE END;",
        "SELECT (CURRENT_DATE);",
    ] {
        let error = diagnostic(sql);
        assert_eq!(error.number, 156, "{sql}");
        assert_eq!(
            error.message, "Syntax error near the keyword 'CURRENT_DATE'.",
            "{sql}"
        );
    }
    assert_eq!(top_diagnostic("SELECT TOP (CURRENT_DATE) 1;").number, 156);
}
