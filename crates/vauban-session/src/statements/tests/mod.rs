use super::*;
use vauban_parser::{ParseOptions, parse_batch};

fn parse(text: &str) -> Vec<Statement> {
    parse_batch(text, &ParseOptions::default())
        .expect("the text parses")
        .statements
}

#[test]
fn statement_text_is_the_text_the_client_wrote() {
    let text = "SET nocount on;\nWAITFOR DELAY '00:00:01'";
    let statements = parse(text);
    assert_eq!(statement_text(text, &statements[0]), "SET nocount on");
    assert_eq!(
        statement_text(text, &statements[1]),
        "WAITFOR DELAY '00:00:01'"
    );
}

#[test]
fn statement_text_preserves_the_select_source() {
    let text = "select  1";
    let statements = parse(text);
    assert_eq!(statement_text(text, &statements[0]), text);
}

#[test]
fn statement_fragment_keeps_the_separator_and_its_line() {
    let text = "SELECT 1\n;\nSELECT 2";
    let statements = parse(text);
    assert_eq!(
        statement_fragment(text, &statements[0], statements.get(1)),
        "SELECT 1\n;\n"
    );
}

#[test]
fn statement_line_is_the_line_of_the_first_token() {
    // The `SELECT` of the second statement is on line 3, its select list on line 4.
    let text = "SELECT 1;\n\n SELECT\n 1 / 0;";
    let statements = parse(text);
    assert_eq!(statement_line(&statements[0]), 1);
    assert_eq!(statement_line(&statements[1]), 3);
    // A comment before the statement is trivia: the line is the keyword's.
    let text = "SELECT 1;\n-- a comment on its own line\nSELECT\n1 / 0;";
    assert_eq!(statement_line(&parse(text)[1]), 3);
    // Two statements on one line share it.
    assert_eq!(statement_line(&parse("SELECT 1; SELECT\n1 / 0;")[1]), 1);
}

/// The variants that carry no data definition — `PRINT`, an assignment `SET`, a `SET`
/// option, `TRUNCATE`, `THROW`, `WAITFOR`, `RETURN`, and the three that hold another
/// statement — answer the line of their first token, like a `SELECT`.
///
/// Each statement below starts on line 2 and spreads over line 3, so a reading that took
/// the last line of the statement would answer 3 and a statement left without a line
/// would answer 0. The three that hold another statement put that one on line 3: the
/// answer is the holder's line, not the held one's.
#[test]
fn statement_line_covers_the_control_flow_variants_and_print() {
    for text in [
        "SELECT 1;\nPRINT\n  @v;",
        "SELECT 1;\nSET @v =\n  1;",
        "SELECT 1;\nSET NOCOUNT\n  ON;",
        "SELECT 1;\nTRUNCATE TABLE\n  dbo.t;",
        "SELECT 1;\nTHROW 51000,\n  'x', 1;",
        "SELECT 1;\nWAITFOR DELAY\n  '00:00:01';",
        "SELECT 1;\nRETURN\n;",
        "SELECT 1;\nIF 1 = 1\n  SELECT 2;",
        "SELECT 1;\nWHILE 1 = 1\n  BREAK;",
        "SELECT 1;\nBEGIN\n  SELECT 2;\nEND",
    ] {
        assert_eq!(statement_line(&parse(text)[1]), 2, "{text}");
    }
}

/// An `EXEC` answers the line of its first token, not the line its procedure name sits on:
/// the statement below opens on line 2 and spreads over line 3.
#[test]
fn statement_line_of_an_exec_is_the_line_it_starts_on() {
    let text = "SELECT 1;\nEXEC\n  dbo.p;";
    let statements = parse(text);
    assert!(matches!(statements[1], Statement::Execute(_)));
    assert_eq!(statement_line(&statements[1]), 2);
}

/// The DDL variants, `USE`, `INSERT`, `UPDATE` and `DELETE` answer the line of their
/// first token, like a `SELECT`.
///
/// Each statement below starts on line 2 and spreads over line 3: a reading that took the
/// last line of the statement would answer 3, and a statement left without a line would
/// answer 0.
#[test]
fn statement_line_covers_the_ddl_variants_and_use() {
    for text in [
        "SELECT 1;\nCREATE TABLE dbo.t\n  (a int);",
        "SELECT 1;\nDROP TABLE\n  dbo.t;",
        "SELECT 1;\nCREATE DATABASE\n  d;",
        "SELECT 1;\nDROP DATABASE\n  d;",
        "SELECT 1;\nCREATE INDEX ix ON dbo.t\n  (a);",
        "SELECT 1;\nDROP INDEX ix ON\n  dbo.t;",
        "SELECT 1;\nUSE\n  d;",
        "SELECT 1;\nINSERT INTO dbo.t\n  (a) VALUES (1);",
        "SELECT 1;\nUPDATE dbo.t\n  SET a = 1;",
        "SELECT 1;\nDELETE FROM\n  dbo.t;",
        "SELECT 1;\nDELETE\n  dbo.t WHERE a = 1;",
    ] {
        assert_eq!(statement_line(&parse(text)[1]), 2, "{text}");
    }
}

#[test]
fn at_statement_overrides_the_line_of_the_sub_expression() {
    let raised_on_line_4 = SqlError::divide_by_zero().with_line(4);
    assert_eq!(at_statement(raised_on_line_4, 3).line, 3);
}

#[test]
fn at_statement_without_a_line_changes_nothing() {
    let raised_on_line_4 = SqlError::divide_by_zero().with_line(4);
    assert_eq!(at_statement(raised_on_line_4, 0).line, 4);
}

#[test]
fn an_internal_error_still_carries_no_line() {
    let bug = SqlError::from(vauban_errors::InternalError::Bug("x".to_owned()));
    let wrapped = at_statement(bug, 3);
    assert_eq!(wrapped.number, INTERNAL_ERROR);
    assert_eq!(wrapped.line, 0);
}

#[test]
fn the_two_compilation_checks_keep_their_own_line() {
    // `SELECT` on line 3, `TOP (-1)` on line 4: SQL Server answers 4 for both numbers.
    assert_eq!(
        at_statement(SqlError::top_negative().with_line(4), 3).line,
        4
    );
    assert_eq!(at_statement(SqlError::top_null().with_line(4), 3).line, 4);
}

/// [`use_name_line`] on the shapes of its table, without the chain around it: the
/// trivia between `USE` and its name — spaces, a line comment, a block comment, a
/// nested block comment — is counted, and the answer is where the name starts.
#[test]
fn use_name_line_is_the_line_the_name_starts_on() {
    for (text, line) in [
        ("USE d;", 1),
        ("SELECT 1;\nUSE\n  d\n;", 3),
        ("SELECT 1;\nUSE /* c\n  c */ d;", 3),
        ("SELECT 1;\nUSE\n-- a comment\n  d;", 4),
        ("SELECT 1;\nUSE [d\ne];", 2),
        ("SELECT 1;\nUSE /* a /* b\nb */ a */\n d;", 4),
        ("SELECT 1;\nUSE\n\n\n  d;", 5),
    ] {
        let statements = parse(text);
        let stmt = statements.last().expect("the batch has a statement");
        assert!(matches!(stmt, Statement::Use { .. }), "{text}");
        assert_eq!(use_name_line(text, stmt), line, "{text}");
    }
}
