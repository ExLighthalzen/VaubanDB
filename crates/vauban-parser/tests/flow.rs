//! Flow of control, variables, session options, transactions, `PRINT` and `EXECUTE`.
//!
//! Everything goes through the public entry point of the crate, `parse_batch`: a client
//! sends a batch, and the shape of the AST it yields is what `binder` and `executor` are
//! written against.
//!
//! # The `parse` -> `Display` -> `parse` loop
//!
//! [`rt`] is the loop the module README makes a contract, and the same helper as in
//! `tests/select.rs`: for any accepted text, parsing what `Display` wrote yields an
//! **equal** `Batch`. The text `Display` writes is not the source text, and the assumed
//! deviations (`BEGIN TRAN` written back `BEGIN TRANSACTION`, `EXEC` written back
//! `EXECUTE`) are asserted here as deviations rather than hidden.
//!
//! Error numbers are pinned individually; the remaining grammar gaps are documented
//! beside their queries.

use vauban_errors::SqlError;
use vauban_parser::{
    AssignOp, AssignTarget, Batch, DeclareItem, ExecuteTarget, ParseOptions, SetOptionValue,
    SetValue, Statement, parse_batch,
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

/// The one statement of a batch that holds exactly one.
fn one(text: &str) -> Statement {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(statement) => statement,
        None => unreachable!("{text} has one statement"),
    }
}

/// The items of the one `DECLARE` of `text`.
fn declare_items(text: &str) -> Vec<DeclareItem> {
    match one(text) {
        Statement::Declare(declare) => declare.items,
        other => unreachable!("{text} is a DECLARE, got {other:?}"),
    }
}

/// The target, the operator and the value of the one `SET @x = …` of `text`.
fn set_assignment(text: &str) -> (AssignTarget, AssignOp, SetValue) {
    match one(text) {
        Statement::Set(set) => (set.target, set.op, set.value),
        other => unreachable!("{text} is a SET of a variable, got {other:?}"),
    }
}

/// The options of the one `SET <option>` of `text`.
fn set_options(text: &str) -> Vec<(String, SetOptionValue)> {
    match one(text) {
        Statement::SetOption(set) => set.options,
        other => unreachable!("{text} is a SET of an option, got {other:?}"),
    }
}

/// Asserts the current 156 diagnostic on a reserved token.
/// Call sites document the SQL Server outcome, including known grammar gaps.
fn assert_error_on_reserved(error: &SqlError, token: &str) {
    assert_eq!(
        error.number, 156,
        "{token}: {} {}",
        error.number, error.message
    );
    assert!(
        error.message.contains(&format!("'{token}'")),
        "the error should name {token}, got {}",
        error.message
    );
}

#[test]
fn declare() {
    assert_eq!(rt("DECLARE @x int"), "DECLARE @x int");
    assert_eq!(rt("DECLARE @x int = 1"), "DECLARE @x int = 1");

    let text = "DECLARE @x int = 1, @y varchar(10), @z decimal(18, 2) = 0.5";
    let items = declare_items(text);
    assert_eq!(items.len(), 3, "{items:?}");
    match &items[0] {
        DeclareItem::Variable { name, ty, default } => {
            assert_eq!(name, "@x");
            assert_eq!(ty.name, "int");
            assert!(default.is_some(), "@x has an initial value");
        }
        other => unreachable!("@x is a scalar variable, got {other:?}"),
    }
    match &items[1] {
        DeclareItem::Variable { name, ty, default } => {
            assert_eq!(name, "@y");
            assert_eq!(ty.name, "varchar");
            assert!(default.is_none(), "@y has no initial value");
        }
        other => unreachable!("@y is a scalar variable, got {other:?}"),
    }
    assert_eq!(rt(text), text);

    // A table variable (V2 of the executor, V1 of the grammar): the table body rule of
    // `ddl_table` is called on the `(` and gives the cursor back after the `)`.
    let text = "DECLARE @t TABLE (a int NOT NULL, PRIMARY KEY (a))";
    match declare_items(text).pop() {
        Some(DeclareItem::TableVariable { name, definition }) => {
            assert_eq!(name, "@t");
            assert_eq!(definition.columns.len(), 1);
            assert_eq!(definition.constraints.len(), 1);
        }
        other => unreachable!("@t is a table variable, got {other:?}"),
    }
    assert_eq!(rt(text), text);
}

/// The `AS` between the name and the type is optional, on both forms of a `DECLARE`.
///
/// T-SQL spells them `DECLARE @local_variable [AS] data_type` and
/// `@table_variable_name [AS] TABLE`; the server accepts the four batches below.
/// The AST has no field for a word that carries nothing, so `Display` writes the item back
/// without it: the printed text differs from the source, the parsed batches are equal, and
/// that is the contract [`rt`] checks.
#[test]
fn declare_optional_as() {
    assert_eq!(p("DECLARE @x AS int"), p("DECLARE @x int"));
    assert_eq!(rt("DECLARE @x AS int"), "DECLARE @x int");

    let text = "DECLARE @x AS int = 1, @y AS varchar(10)";
    assert_eq!(p(text), p("DECLARE @x int = 1, @y varchar(10)"));
    assert_eq!(rt(text), "DECLARE @x int = 1, @y varchar(10)");

    match declare_items("DECLARE @t AS TABLE (a int)").pop() {
        Some(DeclareItem::TableVariable { name, definition }) => {
            assert_eq!(name, "@t");
            assert_eq!(definition.columns.len(), 1);
        }
        other => unreachable!("@t is a table variable, got {other:?}"),
    }
    assert_eq!(
        rt("DECLARE @t AS TABLE (a int)"),
        "DECLARE @t TABLE (a int)"
    );

    // `DECLARE @x AS` is a syntax error on the server too (`102 near 'AS'`); here the
    // cursor has run out of input, so `syntax_error.rs` prints the last token.
    let error = p_err("DECLARE @x AS");
    assert_eq!(error.number, 102, "{}", error.message);
}

#[test]
fn set_variable() {
    let (target, op, value) = set_assignment("SET @x = 1");
    assert_eq!(target, AssignTarget::Variable("@x".to_owned()));
    assert_eq!(op, AssignOp::Set);
    assert!(matches!(value, SetValue::Expr(_)), "{value:?}");
    assert_eq!(rt("SET @x = 1"), "SET @x = 1");

    let (_, op, _) = set_assignment("SET @x += 1");
    assert_eq!(op, AssignOp::AddAssign);
    assert_eq!(rt("SET @x += 1"), "SET @x += 1");

    // The other seven compound operators, all accepted by the server.
    for (text, expected) in [
        ("SET @x -= 1", AssignOp::SubAssign),
        ("SET @x *= 2", AssignOp::MulAssign),
        ("SET @x /= 2", AssignOp::DivAssign),
        ("SET @x %= 2", AssignOp::ModAssign),
        ("SET @x &= 2", AssignOp::BitAndAssign),
        ("SET @x |= 2", AssignOp::BitOrAssign),
        ("SET @x ^= 2", AssignOp::BitXorAssign),
    ] {
        let (_, op, _) = set_assignment(text);
        assert_eq!(op, expected, "{text}");
        assert_eq!(rt(text), text);
    }

    let (_, _, value) = set_assignment("SET @x = (SELECT 1)");
    assert!(matches!(value, SetValue::Query(_)), "{value:?}");
    assert_eq!(rt("SET @x = (SELECT 1)"), "SET @x = (SELECT 1)");

    let (_, _, value) = set_assignment("SET @x = @y * 2");
    assert!(matches!(value, SetValue::Expr(_)), "{value:?}");
    assert_eq!(rt("SET @x = @y * 2"), "SET @x = @y * 2");
}

#[test]
fn set_option() {
    assert_eq!(
        set_options("SET NOCOUNT ON"),
        vec![("NOCOUNT".to_owned(), SetOptionValue::On)]
    );
    assert_eq!(rt("SET NOCOUNT ON"), "SET NOCOUNT ON");

    // Several names share one value; `Display` writes the value once.
    assert_eq!(
        set_options("SET ANSI_NULLS, ANSI_PADDING ON"),
        vec![
            ("ANSI_NULLS".to_owned(), SetOptionValue::On),
            ("ANSI_PADDING".to_owned(), SetOptionValue::On),
        ]
    );
    assert_eq!(
        rt("SET ANSI_NULLS, ANSI_PADDING ON"),
        "SET ANSI_NULLS, ANSI_PADDING ON"
    );

    let options = set_options("SET LOCK_TIMEOUT 5000");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].0, "LOCK_TIMEOUT");
    assert!(
        matches!(options[0].1, SetOptionValue::Value(_)),
        "{:?}",
        options[0].1
    );
    assert_eq!(rt("SET LOCK_TIMEOUT 5000"), "SET LOCK_TIMEOUT 5000");

    // The four words of `SET TRANSACTION ISOLATION LEVEL` are one option name; the level
    // is its value. None of them may go through `parse_ident`: `TRANSACTION` and `READ`
    // are reserved words.
    assert_eq!(
        set_options("SET TRANSACTION ISOLATION LEVEL READ COMMITTED"),
        vec![(
            "TRANSACTION ISOLATION LEVEL".to_owned(),
            SetOptionValue::Word("READ COMMITTED".to_owned())
        )]
    );
    assert_eq!(
        rt("SET TRANSACTION ISOLATION LEVEL READ COMMITTED"),
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
    );

    // `IDENTITY_INSERT` is reserved too, and its table is part of the option name.
    assert_eq!(
        set_options("SET IDENTITY_INSERT dbo.t ON"),
        vec![("IDENTITY_INSERT dbo.t".to_owned(), SetOptionValue::On)]
    );
    assert_eq!(
        rt("SET IDENTITY_INSERT dbo.t ON"),
        "SET IDENTITY_INSERT dbo.t ON"
    );
}

/// The five isolation levels of T-SQL, and the refusal of anything else:
/// `SET TRANSACTION ISOLATION LEVEL FOO` is a syntax error on `FOO`.
#[test]
fn set_transaction_isolation_levels() {
    for level in [
        "READ UNCOMMITTED",
        "READ COMMITTED",
        "REPEATABLE READ",
        "SNAPSHOT",
        "SERIALIZABLE",
    ] {
        let text = format!("SET TRANSACTION ISOLATION LEVEL {level}");
        assert_eq!(
            set_options(&text),
            vec![(
                "TRANSACTION ISOLATION LEVEL".to_owned(),
                SetOptionValue::Word(level.to_owned())
            )]
        );
        assert_eq!(rt(&text), text);
    }
    let error = p_err("SET TRANSACTION ISOLATION LEVEL FOO");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near 'FOO'.");
}

/// The twelve `SET` batches an application or a migration script commonly opens with.
#[test]
fn set_option_corpus_of_the_drivers() {
    for text in [
        "SET QUOTED_IDENTIFIER ON",
        "SET ANSI_NULLS ON",
        "SET ANSI_PADDING ON",
        "SET ANSI_WARNINGS ON",
        "SET ARITHABORT ON",
        "SET CONCAT_NULL_YIELDS_NULL ON",
        "SET NUMERIC_ROUNDABORT OFF",
        "SET IMPLICIT_TRANSACTIONS OFF",
        "SET TEXTSIZE 2147483647",
        "SET DATEFORMAT ymd",
        "SET LANGUAGE us_english",
        "SET DATEFIRST 7",
    ] {
        let options = set_options(text);
        assert_eq!(options.len(), 1, "{text}");
        assert_eq!(rt(text), text);
    }
    // `TEXTSIZE` and `ROWCOUNT` are reserved words and are still option names.
    assert_eq!(
        set_options("SET TEXTSIZE 2147483647")[0].0,
        "TEXTSIZE".to_owned()
    );
    assert_eq!(rt("SET ROWCOUNT 0"), "SET ROWCOUNT 0");
    // A bare word value stays a word, a number becomes an expression.
    assert_eq!(
        set_options("SET DATEFORMAT ymd")[0].1,
        SetOptionValue::Word("ymd".to_owned())
    );
    assert!(matches!(
        set_options("SET DATEFIRST 7")[0].1,
        SetOptionValue::Value(_)
    ));
}

#[test]
fn if_else() {
    match one("IF @x = 1 PRINT 'a'") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert!(matches!(*then_branch, Statement::Print { .. }));
            assert!(else_branch.is_none(), "no ELSE was written");
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
    assert_eq!(rt("IF @x = 1 PRINT 'a'"), "IF @x = 1 PRINT 'a'");

    match one("IF @x = 1 PRINT 'a' ELSE PRINT 'b'") {
        Statement::If { else_branch, .. } => {
            assert!(else_branch.is_some(), "the ELSE was written");
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
    assert_eq!(
        rt("IF @x = 1 PRINT 'a' ELSE PRINT 'b'"),
        "IF @x = 1 PRINT 'a' ELSE PRINT 'b'"
    );

    let text = "IF @x = 1 BEGIN SET @x = 2 SET @y = 3 END ELSE BEGIN PRINT 'b' END";
    match one(text) {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            match *then_branch {
                Statement::Block { statements, .. } => assert_eq!(statements.len(), 2),
                other => unreachable!("the THEN is a block, got {other:?}"),
            }
            match else_branch.map(|branch| *branch) {
                Some(Statement::Block { statements, .. }) => assert_eq!(statements.len(), 1),
                other => unreachable!("the ELSE is a block, got {other:?}"),
            }
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
    assert_eq!(
        rt(text),
        "IF @x = 1 BEGIN SET @x = 2; SET @y = 3 END ELSE BEGIN PRINT 'b' END"
    );

    assert_eq!(
        rt("IF EXISTS (SELECT 1 FROM t) PRINT 'a'"),
        "IF EXISTS (SELECT 1 FROM t) PRINT 'a'"
    );
}

/// The dangling `ELSE` goes to the nearest `IF`, and the recursion is what does it.
///
/// On SQL Server, `IF 1 = 1 IF 2 = 2 PRINT 'x' ELSE PRINT 'y'` prints `x` and
/// `IF 1 = 1 IF 2 = 3 PRINT 'x' ELSE PRINT 'y'` prints `y`, so the second `IF` is the one
/// the `ELSE` belongs to.
#[test]
fn if_else_binds_to_nearest_if() {
    match one("IF a = 1 IF b = 2 PRINT 'x' ELSE PRINT 'y'") {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            assert!(else_branch.is_none(), "the outer IF has no ELSE");
            match *then_branch {
                Statement::If { else_branch, .. } => {
                    assert!(else_branch.is_some(), "the inner IF has the ELSE");
                }
                other => unreachable!("the THEN is the inner IF, got {other:?}"),
            }
        }
        other => unreachable!("it is an IF, got {other:?}"),
    }
    assert_eq!(
        rt("IF a = 1 IF b = 2 PRINT 'x' ELSE PRINT 'y'"),
        "IF a = 1 IF b = 2 PRINT 'x' ELSE PRINT 'y'"
    );
}

#[test]
fn while_break_continue() {
    let text = "WHILE @x < 10 BEGIN SET @x += 1 IF @x = 5 BREAK CONTINUE END";
    match one(text) {
        Statement::While { body, .. } => match *body {
            Statement::Block { statements, .. } => {
                assert_eq!(statements.len(), 3, "{statements:?}");
                assert!(matches!(statements[0], Statement::Set(_)));
                assert!(matches!(statements[1], Statement::If { .. }));
                assert!(matches!(statements[2], Statement::Continue(_)));
            }
            other => unreachable!("the body is a block, got {other:?}"),
        },
        other => unreachable!("it is a WHILE, got {other:?}"),
    }
    assert_eq!(
        rt(text),
        "WHILE @x < 10 BEGIN SET @x += 1; IF @x = 5 BREAK; CONTINUE END"
    );
}

/// A block holds at least one statement, its `;` are optional, and it must be closed.
///
/// `BEGIN END` is not an empty block, it is a syntax error: on SQL Server, `BEGIN END`,
/// `BEGIN ; END`, `IF 1 = 1 BEGIN END` and `WHILE 1 = 0 BEGIN END` all answer
/// `102 near 'END'`.
#[test]
fn block_and_empty_block() {
    let error = p_err("BEGIN END");
    // `BEGIN END` and `BEGIN ; END`: SQL Server answers 102; VaubanDB remains 156,
    // a known grammar deviation.
    assert_error_on_reserved(&error, "END");
    assert_error_on_reserved(&p_err("BEGIN ; END"), "END");

    match one("BEGIN SELECT 1; SELECT 2 END") {
        Statement::Block { statements, .. } => assert_eq!(statements.len(), 2),
        other => unreachable!("it is a block, got {other:?}"),
    }
    assert_eq!(
        rt("BEGIN SELECT 1; SELECT 2 END"),
        "BEGIN SELECT 1; SELECT 2 END"
    );
    // The `;` are optional and a run of them is one separator, as in `parse_batch`.
    assert_eq!(
        rt("BEGIN SELECT 1 SELECT 2 END"),
        "BEGIN SELECT 1; SELECT 2 END"
    );
    assert_eq!(
        rt("BEGIN ;; SELECT 1 ;; SELECT 2 ;; END"),
        "BEGIN SELECT 1; SELECT 2 END"
    );

    // No `END`: the batch runs out and the error is a 102 on its last token, `1`
    // (`BEGIN SELECT 1` answers `102 near '1'`).
    let error = p_err("BEGIN SELECT 1");
    assert_eq!(error.number, 102, "{}", error.message);
}

#[test]
fn print_and_return() {
    assert_eq!(rt("PRINT 'a'"), "PRINT 'a'");
    assert_eq!(rt("PRINT @x + 'b'"), "PRINT @x + 'b'");
    // `PRINT` takes a value, never a predicate: `PRINT 1 = 1` is a 102 on `=`.
    let error = p_err("PRINT 1 = 1");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near '='.");

    match one("RETURN") {
        Statement::Return { value, .. } => assert!(value.is_none(), "a bare RETURN"),
        other => unreachable!("it is a RETURN, got {other:?}"),
    }
    assert_eq!(rt("RETURN"), "RETURN");

    match one("RETURN 1") {
        Statement::Return { value, .. } => assert!(value.is_some(), "RETURN 1 returns 1"),
        other => unreachable!("it is a RETURN, got {other:?}"),
    }
    assert_eq!(rt("RETURN 1"), "RETURN 1");

    // A bare `RETURN` may be followed by another statement: `RETURN SELECT 1` is
    // accepted and is two statements.
    let statements = p("RETURN SELECT 1").statements;
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(matches!(
        statements[0],
        Statement::Return { value: None, .. }
    ));
}

#[test]
fn transactions() {
    // `TRAN` and `TRANSACTION` are the same node; `Display` writes the long form.
    assert_eq!(p("BEGIN TRAN"), p("BEGIN TRANSACTION"));
    assert_eq!(rt("BEGIN TRAN"), "BEGIN TRANSACTION");
    assert_eq!(rt("BEGIN TRANSACTION"), "BEGIN TRANSACTION");

    match one("BEGIN TRANSACTION t1") {
        Statement::BeginTransaction { name, mark, .. } => {
            assert_eq!(name.map(|ident| ident.value), Some("t1".to_owned()));
            assert!(mark.is_none(), "no WITH MARK was written");
        }
        other => unreachable!("it is a BEGIN TRANSACTION, got {other:?}"),
    }
    assert_eq!(rt("BEGIN TRANSACTION t1"), "BEGIN TRANSACTION t1");

    match one("BEGIN TRANSACTION t1 WITH MARK 'm'") {
        Statement::BeginTransaction { mark, .. } => assert_eq!(mark, Some("m".to_owned())),
        other => unreachable!("it is a BEGIN TRANSACTION, got {other:?}"),
    }
    assert_eq!(
        rt("BEGIN TRANSACTION t1 WITH MARK 'm'"),
        "BEGIN TRANSACTION t1 WITH MARK 'm'"
    );

    assert_eq!(rt("COMMIT"), "COMMIT TRANSACTION");
    assert_eq!(rt("COMMIT TRAN"), "COMMIT TRANSACTION");
    assert_eq!(rt("COMMIT WORK"), "COMMIT TRANSACTION");
    match one("COMMIT TRANSACTION t1") {
        Statement::Commit { name, .. } => {
            assert_eq!(name.map(|ident| ident.value), Some("t1".to_owned()));
        }
        other => unreachable!("it is a COMMIT, got {other:?}"),
    }
    assert_eq!(rt("COMMIT TRANSACTION t1"), "COMMIT TRANSACTION t1");

    assert_eq!(rt("ROLLBACK"), "ROLLBACK TRANSACTION");
    assert_eq!(rt("ROLLBACK WORK"), "ROLLBACK TRANSACTION");
    assert_eq!(rt("ROLLBACK TRANSACTION t1"), "ROLLBACK TRANSACTION t1");

    match one("SAVE TRANSACTION s1") {
        Statement::Save { name, .. } => assert_eq!(name.value, "s1"),
        other => unreachable!("it is a SAVE, got {other:?}"),
    }
    assert_eq!(rt("SAVE TRANSACTION s1"), "SAVE TRANSACTION s1");
    assert_eq!(rt("SAVE TRAN s1"), "SAVE TRANSACTION s1");

    // `WORK` carries no name, and a name without `TRAN[SACTION]` is not one either:
    // `COMMIT WORK t1`, `ROLLBACK WORK t1` and `COMMIT t1` are all
    // `102 near 't1'`.
    for text in ["COMMIT WORK t1", "ROLLBACK WORK t1", "COMMIT t1"] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, "Syntax error near 't1'.", "{text}");
    }
}

#[test]
fn execute() {
    match one("EXEC dbo.p") {
        Statement::Execute(execute) => {
            assert!(!execute.implicit, "the EXEC was written");
            assert!(execute.args.is_empty(), "no argument");
            assert!(execute.return_into.is_none(), "no return variable");
            match execute.target {
                ExecuteTarget::Procedure(name) => assert_eq!(name.name.value, "p"),
                other => unreachable!("the target is a procedure, got {other:?}"),
            }
        }
        other => unreachable!("it is an EXECUTE, got {other:?}"),
    }
    // `Display` always writes `EXECUTE`, never the `EXEC` abbreviation.
    assert_eq!(rt("EXEC dbo.p"), "EXECUTE dbo.p");

    match one("EXECUTE dbo.p 1, 'a'") {
        Statement::Execute(execute) => {
            assert_eq!(execute.args.len(), 2);
            assert!(execute.args.iter().all(|arg| arg.name.is_none()));
        }
        other => unreachable!("it is an EXECUTE, got {other:?}"),
    }
    assert_eq!(rt("EXECUTE dbo.p 1, 'a'"), "EXECUTE dbo.p 1, 'a'");

    match one("EXEC dbo.p @a = 1, @b = 'x'") {
        Statement::Execute(execute) => {
            let names: Vec<Option<String>> =
                execute.args.iter().map(|arg| arg.name.clone()).collect();
            assert_eq!(
                names,
                vec![Some("@a".to_owned()), Some("@b".to_owned())],
                "both arguments are named"
            );
        }
        other => unreachable!("it is an EXECUTE, got {other:?}"),
    }
    assert_eq!(
        rt("EXEC dbo.p @a = 1, @b = 'x'"),
        "EXECUTE dbo.p @a = 1, @b = 'x'"
    );

    for (text, printed) in [
        ("EXEC dbo.p @a = @v OUTPUT", "EXECUTE dbo.p @a = @v OUTPUT"),
        // `OUT` is the short spelling; the AST keeps only the flag.
        ("EXEC dbo.p @a = @v OUT", "EXECUTE dbo.p @a = @v OUTPUT"),
    ] {
        match one(text) {
            Statement::Execute(execute) => {
                assert_eq!(execute.args.len(), 1, "{text}");
                assert!(execute.args[0].output, "{text} passes an output argument");
            }
            other => unreachable!("{text} is an EXECUTE, got {other:?}"),
        }
        assert_eq!(rt(text), printed);
    }

    match one("EXEC @r = dbo.p 1") {
        Statement::Execute(execute) => {
            assert_eq!(execute.return_into, Some("@r".to_owned()));
            assert_eq!(execute.args.len(), 1);
        }
        other => unreachable!("it is an EXECUTE, got {other:?}"),
    }
    assert_eq!(rt("EXEC @r = dbo.p 1"), "EXECUTE @r = dbo.p 1");

    match one("EXEC @proc") {
        Statement::Execute(execute) => {
            assert_eq!(execute.target, ExecuteTarget::Variable("@proc".to_owned()));
        }
        other => unreachable!("it is an EXECUTE, got {other:?}"),
    }
    assert_eq!(rt("EXEC @proc"), "EXECUTE @proc");

    match one("EXEC ('SELECT 1')") {
        Statement::Execute(execute) => {
            assert!(
                matches!(execute.target, ExecuteTarget::Literal(_)),
                "{:?}",
                execute.target
            );
        }
        other => unreachable!("it is an EXECUTE, got {other:?}"),
    }
    assert_eq!(rt("EXEC ('SELECT 1')"), "EXECUTE ('SELECT 1')");
    // The text may be built by concatenation; it is **not** parsed here.
    assert_eq!(
        rt("EXEC ('SELECT ' + @c + ' FROM t')"),
        "EXECUTE ('SELECT ' + @c + ' FROM t')"
    );
}

/// The word `EXECUTE` may be left out, but only for the **first** statement of a batch.
///
/// `dbo.p 1` alone answers 2812 (unknown stored procedure),
/// which is a run-time error and proves the parse; `SELECT 1; dbo.p 1` answers
/// `102 near 'dbo'`.
#[test]
fn implicit_execute_first_statement_only() {
    match one("dbo.p 1") {
        Statement::Execute(execute) => {
            assert!(execute.implicit, "no EXEC was written");
            assert_eq!(execute.args.len(), 1);
            match execute.target {
                ExecuteTarget::Procedure(name) => {
                    assert_eq!(name.schema.map(|ident| ident.value), Some("dbo".to_owned()));
                    assert_eq!(name.name.value, "p");
                }
                other => unreachable!("the target is a procedure, got {other:?}"),
            }
        }
        other => unreachable!("it is an implicit EXECUTE, got {other:?}"),
    }
    // An implicit call writes nothing before the name, so the loop holds.
    assert_eq!(rt("dbo.p 1"), "dbo.p 1");
    assert_eq!(rt("sp_who"), "sp_who");

    let error = p_err("SELECT 1; dbo.p 1");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near 'dbo'.");
}

/// The four syntax errors of `tests/syntax_errors.rs` stay what they are: the implicit
/// `EXEC` only claims the **first** statement of a batch, so `SELECT 1; SELEC 1;` is still
/// a syntax error on `SELEC`.
#[test]
fn syntax_errors_case_still_passes() {
    let error = p_err("SELECT 1; SELEC 1;");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near 'SELEC'.");

    let error = p_err("SELECT * FROM;");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near ';'.");

    // 105 comes from the lexer and its wording from `errors`; the exact text is pinned by
    // `tests/syntax_errors.rs`.
    assert_eq!(p_err("SELECT 'abc;").number, 105);

    // `SELECT FROM t;` gives 156 on SQL Server. The head of
    // that batch is not turned into an implicit `EXEC` either, `SELECT` being a keyword
    // the dispatch claims.
    assert_error_on_reserved(&p_err("SELECT FROM t;"), "FROM");
}

/// `GOTO` and `BEGIN TRY … END CATCH` are out of the V1 subset and are refused, although
/// SQL Server accepts both (`GOTO fin` answers 133, a semantic error about the undeclared
/// label, and the `TRY`/`CATCH` batch runs). A deliberate deviation.
#[test]
fn goto_and_try_catch_are_refused() {
    // `GOTO fin` gives 133 on SQL Server; VaubanDB remains 156 with
    // `Syntax error near the keyword 'GOTO'.`.
    assert_error_on_reserved(&p_err("GOTO fin"), "GOTO");

    // `TRY` is **not** reserved, so this one is a 102, and its
    // message is exact. The error deliberately names `TRY` rather than the `BEGIN` it
    // follows: `TRY` is the word that is out of the subset.
    let error = p_err("BEGIN TRY SELECT 1 END TRY BEGIN CATCH SELECT 2 END CATCH");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near 'TRY'.");
}

/// The cursor statements are V2 and are refused by the dispatch, on their head word.
///
/// `DECLARE @c CURSOR` is the exception, and on purpose: the server accepts it (it
/// answers nothing at all), and so does this grammar, as a variable whose type is
/// spelled `CURSOR`. Refusing it would be further from the server than accepting it, and
/// the type is the binder's to reject. `DeclareItem::Cursor` stays unproduced.
#[test]
fn cursor_statements_are_refused() {
    // SQL Server: `OPEN c`, `FETCH NEXT FROM c`, `CLOSE c`, `DEALLOCATE c`
    // each give 16916 (missing cursor). VaubanDB remains 156, a known deviation.
    for (text, token) in [
        ("OPEN c", "OPEN"),
        ("FETCH NEXT FROM c", "FETCH"),
        ("CLOSE c", "CLOSE"),
        ("DEALLOCATE c", "DEALLOCATE"),
    ] {
        assert_error_on_reserved(&p_err(text), token);
    }
    match declare_items("DECLARE @c CURSOR").pop() {
        Some(DeclareItem::Variable { name, ty, .. }) => {
            assert_eq!(name, "@c");
            assert_eq!(ty.name, "CURSOR");
        }
        other => unreachable!("@c is read as a scalar variable, got {other:?}"),
    }
}

/// Which token each rule stops on, on a truncated or malformed statement.
///
/// The numbers are those SQL Server answers. A statement that runs out of
/// input is a 102 even when its head word is reserved (`SELECT 1; DECLARE` answers
/// `102 near 'DECLARE'`), with one exception: `SELECT 1; SET` and `SET ;`
/// both answer `156 near the keyword 'SET'`, which is why `parse_set` reports on the
/// `SET` itself when nothing usable follows it. The proofs are written in the middle of a
/// batch on purpose: a batch made of a single **name** is the implicit `EXEC` (`SELEC`
/// alone answers 2812 (unknown stored procedure)), so a one-word batch
/// only proves the rule when that word is reserved, which is a needless subtlety here.
/// Printing the last token of a truncated batch is the job of `syntax_error.rs`, so only
/// the number is asserted where the offending token is the end of the batch.
#[test]
fn flow_errors() {
    // End of batch: a 102 whatever the last token was, the rule of `syntax_error.rs` and
    // the server's.
    for text in [
        "DECLARE",
        // The same rule proved where the implicit `EXEC` cannot interfere.
        "SELECT 1; DECLARE",
        "DECLARE @x",
        "SET @x",
        "IF",
        "IF @x = 1",
        "WHILE @x = 1",
        "PRINT",
        "EXEC",
        "SAVE TRANSACTION",
        "SET NOCOUNT",
        // The offending token is the `,` the batch ends on.
        "SET NOCOUNT,",
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.line, 1, "{text}");
    }

    // `SET` with nothing usable after it is reported on the `SET`, not on the end of the
    // batch: `SET ;` and `SELECT 1; SET` both answer
    // `156 near the keyword 'SET'`, and so does the one-word batch `SET`.
    assert_error_on_reserved(&p_err("SET"), "SET");
    assert_error_on_reserved(&p_err("SET ;"), "SET");
    assert_error_on_reserved(&p_err("SELECT 1; SET"), "SET");

    // A token that is there and is wrong keeps its own message.
    for (text, message) in [
        ("SET 1", "Syntax error near '1'."),
        ("EXEC 1", "Syntax error near '1'."),
        ("SET IDENTITY_INSERT dbo.t FOO", "Syntax error near 'FOO'."),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, message, "{text}");
    }

    // `SET TRANSACTION` commits the statement to the isolation-level form, so each of
    // these stops on its own last word rather than reading `TRANSACTION` as an option.
    for text in [
        "SET TRANSACTION",
        "SET TRANSACTION ISOLATION",
        "SET TRANSACTION ISOLATION LEVEL",
        "SET TRANSACTION ISOLATION LEVEL READ",
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
    }
}

/// The line of an error is the line of the offending token, not the line the statement
/// starts on.
#[test]
fn flow_error_line_is_the_token_line() {
    let error = p_err("BEGIN\nPRINT 'a'\nEND\nCOMMIT WORK t1");
    assert_eq!(error.number, 102, "{}", error.message);
    assert_eq!(error.message, "Syntax error near 't1'.");
    assert_eq!(error.line, 4);
}

/// A batch mixing everything: what a migration script really looks like, and the proof
/// that the statements nest and separate without a `;`.
#[test]
fn a_whole_script_round_trips() {
    let text = "SET NOCOUNT ON\n\
        DECLARE @i int = 0, @n int = 10\n\
        BEGIN TRANSACTION\n\
        WHILE @i < @n\n\
        BEGIN\n\
            SET @i += 1\n\
            IF @i % 2 = 0 PRINT 'even' ELSE PRINT 'odd'\n\
        END\n\
        IF @@ERROR <> 0 ROLLBACK TRANSACTION ELSE COMMIT TRANSACTION\n\
        EXEC dbo.log_it @message = 'done', @count = @i OUTPUT";
    let statements = p(text).statements;
    assert_eq!(statements.len(), 6, "{statements:?}");
    assert_eq!(
        rt(text),
        "SET NOCOUNT ON;\n\
         DECLARE @i int = 0, @n int = 10;\n\
         BEGIN TRANSACTION;\n\
         WHILE @i < @n BEGIN SET @i += 1; IF @i % 2 = 0 PRINT 'even' ELSE PRINT 'odd' END;\n\
         IF @@ERROR <> 0 ROLLBACK TRANSACTION ELSE COMMIT TRANSACTION;\n\
         EXECUTE dbo.log_it @message = 'done', @count = @i OUTPUT"
    );
}
