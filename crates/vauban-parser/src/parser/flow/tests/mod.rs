use crate::{Batch, Expr, Literal, ParseOptions, Statement, parse_batch};
use vauban_errors::SqlError;

/// Parses `text`, which must parse.
fn p(text: &str) -> Batch {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => batch,
        Err(error) => unreachable!("{text} should parse: {} {}", error.number, error.message),
    }
}

/// The one statement of a batch that holds exactly one.
fn one(text: &str) -> Statement {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(statement) => statement,
        None => unreachable!("{text} has one statement"),
    }
}

/// The error of a text that must **not** parse.
fn p_err(text: &str) -> SqlError {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
        Err(error) => error,
    }
}

/// The string a `PRINT` statement carries, so a test can tell branches apart by their
/// value rather than by their shape alone.
fn printed_value(statement: &Statement) -> &str {
    match statement {
        Statement::Print {
            expr: Expr::Literal(Literal::Str { value, .. }, _),
            ..
        } => value,
        other => unreachable!("expected a PRINT of a string, got {other:?}"),
    }
}

#[test]
fn semicolon_before_else_after_a_plain_statement() {
    match one("IF 1 = 1 PRINT 'then'; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(printed_value(&then_branch), "then");
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past the ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

#[test]
fn semicolon_before_else_after_a_block() {
    match one("IF 1 = 1 BEGIN PRINT 'then' END; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            match *then_branch {
                Statement::Block { statements, .. } => {
                    assert_eq!(statements.len(), 1, "the THEN block holds one statement");
                    assert_eq!(printed_value(&statements[0]), "then");
                }
                other => unreachable!("the THEN is a block, got {other:?}"),
            }
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past the ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// Two `;` in a row before `ELSE` are both consumed, not just the first: a script that
/// doubles its separators by mistake still reaches the `ELSE`.
#[test]
fn a_run_of_semicolons_before_else_is_consumed_as_one() {
    match one("IF 1 = 1 PRINT 'then';; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(printed_value(&then_branch), "then");
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past both ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// A comment between two of the `;` changes nothing: it produces no token, so the run
/// looks the same to the parser as `;;`.
#[test]
fn a_comment_between_two_semicolons_before_else_changes_nothing() {
    match one("IF 1 = 1 PRINT 'then'; /* c */ ; ELSE PRINT 'else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(printed_value(&then_branch), "then");
            assert_eq!(
                printed_value(&else_branch.expect("the ELSE was read past both ';'")),
                "else"
            );
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// The `ELSE` still binds to the nearest `IF` once a `;` can come before it: the inner
/// `IF` reads its own `;` and `ELSE` before the outer one ever looks for either.
#[test]
fn semicolon_before_else_binds_to_the_nearest_if() {
    match one("IF 1 = 1 IF 2 = 3 PRINT 'inner-then'; ELSE PRINT 'inner-else';") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert!(else_branch.is_none(), "the outer IF has no ELSE");
            match *then_branch {
                Statement::If {
                    then_branch: inner_then,
                    else_branch: inner_else,
                    ..
                } => {
                    assert_eq!(printed_value(&inner_then), "inner-then");
                    assert_eq!(
                        printed_value(&inner_else.expect("the inner IF has the ELSE")),
                        "inner-else"
                    );
                }
                other => unreachable!("the THEN is the inner IF, got {other:?}"),
            }
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
}

/// Without an `ELSE` in sight, the `;` after the branch is left for the caller: it does
/// not become part of the `IF`, and a second statement still follows in the same batch.
#[test]
fn semicolon_without_else_is_left_to_the_caller() {
    let batch = p("IF 1 = 0 PRINT 'taken'; PRINT 'after';");
    assert_eq!(
        batch.statements.len(),
        2,
        "the ';' still separates two statements"
    );
    match &batch.statements[0] {
        Statement::If { else_branch, .. } => {
            assert!(else_branch.is_none(), "no ELSE was written")
        }
        other => unreachable!("the first statement is an IF, got {other:?}"),
    }
    assert_eq!(printed_value(&batch.statements[1]), "after");
}

#[test]
fn semicolon_right_after_the_condition_is_refused() {
    assert_eq!(p_err("IF 1 = 1; SELECT 1").number, 102);
}

#[test]
fn semicolon_alone_after_else_is_refused() {
    assert_eq!(p_err("IF 1 = 1 SELECT 1; ELSE;").number, 102);
}

#[test]
fn condition_with_nothing_after_it_is_refused() {
    assert_eq!(p_err("IF 1 = 1;").number, 102);
}

/// A delimited or quoted name is refused as a `GOTO` target and as a label declaration,
/// citing the name without its delimiters, exactly as an ordinary delimited token is
/// printed anywhere else in this crate.
#[test]
fn a_delimited_name_is_not_a_label_nor_a_goto_target() {
    for text in ["GOTO [l]", "GOTO \"l\"", "[l]: PRINT 1;", "\"l\": PRINT 1;"] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, "Syntax error near 'l'.", "{text}");
    }
}

/// The `:` of a label must sit right against the name: a space or a newline between the
/// two leaves the name unclaimed by any rule, exactly as an ordinary bare name is
/// anywhere else a statement is expected.
#[test]
fn a_gap_before_the_colon_is_not_a_label() {
    for text in ["PRINT 1; l : PRINT 2;", "PRINT 1; l\n: PRINT 2;"] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, "Syntax error near 'l'.", "{text}");
    }
}

/// The `PRINT` values of an `ELSE IF` chain, walked down its `else_branch` links, and the
/// length of every node's span.
fn chain_shape(statement: &Statement) -> (Vec<String>, Vec<u32>) {
    let mut values = Vec::new();
    let mut lengths = Vec::new();
    let mut current = Some(statement);
    while let Some(node) = current {
        match node {
            Statement::If {
                then_branch,
                else_branch,
                span,
                ..
            } => {
                values.push(printed_value(then_branch).to_owned());
                lengths.push(span.len);
                current = else_branch.as_deref();
            }
            other => {
                values.push(printed_value(other).to_owned());
                current = None;
            }
        }
    }
    (values, lengths)
}

#[test]
fn an_else_if_chain_builds_the_same_tree_as_nested_ifs() {
    let text =
        "IF @i = 1 PRINT 'a' ELSE IF @i = 2 PRINT 'b'; ELSE IF @i = 3 PRINT 'c' ELSE PRINT 'd'";
    let chain = one(text);
    let (values, lengths) = chain_shape(&chain);
    assert_eq!(values, ["a", "b", "c", "d"]);
    // Every node ends where the chain ends, so each span is the tail of the text.
    let expected: Vec<u32> = ["IF @i = 1", "IF @i = 2", "IF @i = 3"]
        .iter()
        .map(|head| (text.len() - text.find(head).expect("head in text")) as u32)
        .collect();
    assert_eq!(lengths, expected);
    // The same tree as an `ELSE` whose statement is an `IF` reached by recursion: a
    // `BEGIN … END` around the inner `IF` is the only difference that remains.
    let nested = one(
        "IF @i = 1 PRINT 'a' ELSE IF @i = 2 PRINT 'b' ELSE BEGIN IF @i = 3 PRINT 'c' ELSE PRINT 'd' END",
    );
    match (&chain, &nested) {
        (Statement::If { else_branch: a, .. }, Statement::If { else_branch: b, .. }) => {
            match (a.as_deref(), b.as_deref()) {
                (
                    Some(Statement::If {
                        else_branch: Some(inner_a),
                        ..
                    }),
                    Some(Statement::If {
                        else_branch: Some(inner_b),
                        ..
                    }),
                ) => match inner_b.as_ref() {
                    Statement::Block { statements, .. } => assert_eq!(**inner_a, statements[0]),
                    other => unreachable!("a block was expected, got {other:?}"),
                },
                other => unreachable!("two links were expected, got {other:?}"),
            }
        }
        other => unreachable!("two IF were expected, got {other:?}"),
    }
}

#[test]
fn an_if_nested_in_a_link_keeps_its_nearest_else() {
    // `ELSE 'y'` belongs to the `IF @j` inside the second link, and `ELSE 'z'` to the link.
    let chain =
        one("IF @i = 1 PRINT 'a' ELSE IF @i = 2 IF @j = 1 PRINT 'x' ELSE PRINT 'y' ELSE PRINT 'z'");
    let Statement::If {
        else_branch: Some(link),
        ..
    } = chain
    else {
        unreachable!("an IF with an ELSE was expected");
    };
    let Statement::If {
        then_branch,
        else_branch: Some(last),
        ..
    } = *link
    else {
        unreachable!("the second link has an ELSE");
    };
    assert_eq!(printed_value(&last), "z");
    let (values, _) = chain_shape(&then_branch);
    assert_eq!(values, ["x", "y"]);
}

/// `IF … SET @r = 0`, then `links` lines `ELSE IF … SET @r = k`, then an optional `ELSE`.
fn chain_text(links: usize, final_else: bool) -> String {
    let mut text = "IF @i = 0 SET @r = 0\n".to_owned();
    for k in 1..=links {
        text.push_str(&format!("ELSE IF @i = {k} SET @r = {k}\n"));
    }
    if final_else {
        text.push_str("ELSE SET @r = -1\n");
    }
    text
}

#[test]
fn the_else_past_the_chain_limit_is_refused_on_its_line() {
    p(&chain_text(253, false));
    p(&chain_text(252, true));
    for (links, final_else) in [(254, false), (253, true), (1_000, false)] {
        let error = p_err(&chain_text(links, final_else));
        assert_eq!(
            (error.number, error.line),
            (191, 255),
            "{links} {final_else}"
        );
    }
    // One line for the whole chain puts the error on that line.
    let error = p_err(&chain_text(300, true).replace('\n', " "));
    assert_eq!((error.number, error.line), (191, 1));
}

#[test]
fn a_chain_inside_a_block_holds_one_else_less() {
    p(&format!("BEGIN\n{}END", chain_text(252, false)));
    let error = p_err(&format!("BEGIN\n{}END", chain_text(252, true)));
    assert_eq!((error.number, error.line), (191, 255));
}

#[test]
fn the_else_of_a_refused_chain_is_given_back() {
    // The first chain fails on a syntax error after 200 ELSE; the second, in the next
    // batch parsed by a fresh cursor and in the same batch after it, still holds 253.
    assert_eq!(
        p_err(&format!("{}ELSE IF", chain_text(200, false))).number,
        102
    );
    let two = format!(
        "{}SELECT 1\n{}",
        chain_text(253, false),
        chain_text(253, false)
    );
    assert_eq!(p(&two).statements.len(), 3);
}
