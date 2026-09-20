use super::parse_statement;
use crate::ast::stmt::Statement;
use crate::parser::{ParseOptions, Parser};

/// Runs the dispatch on `text` and returns the message of the error it produced.
///
/// A statement whose rule is a stub fails there, so a dispatch that succeeds means the
/// routing table sent the text somewhere it should not have.
fn dispatch(text: &str, first_in_batch: bool) -> (u32, String) {
    match try_dispatch(text, first_in_batch) {
        Ok(statement) => unreachable!("no rule is written yet, got {statement:?}"),
        Err((number, message)) => (number, message),
    }
}

/// Runs the dispatch on `text`, whether it parses or not.
fn try_dispatch(text: &str, first_in_batch: bool) -> Result<Statement, (u32, String)> {
    static OPTIONS: ParseOptions = ParseOptions {
        quoted_identifier: true,
    };
    let mut p = match Parser::new(text, &OPTIONS) {
        Ok(parser) => parser,
        Err(error) => unreachable!("{text} lexes: {error:?}"),
    };
    match parse_statement(&mut p, first_in_batch) {
        Ok(statement) => Ok(statement),
        Err(error) => Err((error.number, error.message)),
    }
}

/// Asserts that `text` was routed to `query` and parsed as a `SELECT`.
fn routes_to_a_select(text: &str) {
    match try_dispatch(text, true) {
        Ok(Statement::Select(_)) => {}
        Ok(other) => unreachable!("{text} is a SELECT, got {other:?}"),
        Err((number, message)) => unreachable!("{text} should parse: {number} {message}"),
    }
}

/// Asserts that `text` was routed to a rule that parsed it, and returns the statement
/// it produced, so that the caller can check which one it is.
fn routes_to_a_statement(text: &str) -> Statement {
    match try_dispatch(text, true) {
        Ok(statement) => statement,
        Err((number, message)) => unreachable!("{text} should parse: {number} {message}"),
    }
}

/// Asserts that `text` reached the stub of `expected`, which proves the routing: a
/// statement that was not routed comes back as a syntax error 102 instead.
fn routes_to(text: &str, expected: &str) {
    let (number, message) = dispatch(text, true);
    assert_eq!(number, 50000, "{text} was not routed: {message}");
    assert!(
        message.contains(expected),
        "{text} should reach {expected}, got {message}"
    );
}

/// Asserts that `text` was routed to a rule that is written and parses it, and that
/// the statement it yields is the `expected` one.
///
/// The three table statements do not reach a stub, so the proof of their routing is
/// the variant they produce rather than the name in a stub message.
fn routes_to_a_table_statement(text: &str, expected: &str) {
    match try_dispatch(text, true) {
        Ok(statement) => {
            let found = match statement {
                Statement::CreateTable(_) => "CREATE TABLE",
                Statement::AlterTable(_) => "ALTER TABLE",
                Statement::DropTable { .. } => "DROP TABLE",
                other => unreachable!("{text} is a table statement, got {other:?}"),
            };
            assert_eq!(found, expected, "{text} was routed to {found}");
        }
        Err((number, message)) => unreachable!("{text} should parse: {number} {message}"),
    }
}

/// Asserts that `text` was routed to a rule of `flow` that is written and parses it,
/// and that the statement it yields is the `expected` variant.
///
/// The eleven flow-of-control rows do not reach a stub either, so the proof of their
/// routing is the variant they produce, as it is for the table statements.
fn routes_to_a_flow_statement(text: &str, expected: &str) {
    match try_dispatch(text, true) {
        Ok(statement) => {
            let found = match statement {
                Statement::Declare(_) => "DECLARE",
                Statement::Set(_) => "SET",
                Statement::SetOption(_) => "SET OPTION",
                Statement::If { .. } => "IF",
                Statement::While { .. } => "WHILE",
                Statement::Block { .. } => "BLOCK",
                Statement::BeginTransaction { .. } => "BEGIN TRANSACTION",
                Statement::Commit { .. } => "COMMIT",
                Statement::Rollback { .. } => "ROLLBACK",
                Statement::Save { .. } => "SAVE",
                Statement::Print { .. } => "PRINT",
                Statement::Execute(_) => "EXECUTE",
                Statement::Return { .. } => "RETURN",
                Statement::Break(_) => "BREAK",
                Statement::Continue(_) => "CONTINUE",
                Statement::Waitfor(_) => "WAITFOR",
                Statement::Throw { .. } => "THROW",
                other => unreachable!("{text} is a flow statement, got {other:?}"),
            };
            assert_eq!(found, expected, "{text} was routed to {found}");
        }
        Err((number, message)) => unreachable!("{text} should parse: {number} {message}"),
    }
}

/// Asserts that `text` was routed to a rule of `dml` that is written and parses it,
/// and that the statement it yields is the `expected` one.
///
/// The four DML statements do not reach a stub, so the proof of their routing is the
/// variant they produce rather than the name in a stub message.
fn routes_to_a_dml_statement(text: &str, expected: &str) {
    match try_dispatch(text, true) {
        Ok(statement) => {
            let found = match statement {
                Statement::Insert(_) => "INSERT",
                Statement::Update(_) => "UPDATE",
                Statement::Delete(_) => "DELETE",
                Statement::Truncate { .. } => "TRUNCATE TABLE",
                other => unreachable!("{text} is a DML statement, got {other:?}"),
            };
            assert_eq!(found, expected, "{text} was routed to {found}");
        }
        Err((number, message)) => unreachable!("{text} should parse: {number} {message}"),
    }
}

/// The four statements the routing table sends to `dml`: they
/// parse, so what proves the routing is the variant they yield. `MERGE` is V3 and
/// still reaches its stub, so it stays in `dispatch_routes_every_statement`.
#[test]
fn dispatch_routes_the_dml_statements() {
    routes_to_a_dml_statement("INSERT INTO t VALUES (1)", "INSERT");
    routes_to_a_dml_statement("UPDATE t SET a = 1", "UPDATE");
    routes_to_a_dml_statement("DELETE FROM t", "DELETE");
    routes_to_a_dml_statement("TRUNCATE TABLE t", "TRUNCATE TABLE");
}

/// The rows of the routing table that no longer reach a stub are checked elsewhere:
/// `SELECT`, `WITH` and `(` by `dispatch_routes_a_select`, the six database and index
/// statements by `dispatch_routes_the_ddl_of_databases_and_indexes` and
/// `dispatch_names_the_object_of_an_unsupported_ddl`, the three table statements by
/// `dispatch_routes_the_table_statements`, the flow-of-control ones by
/// `dispatch_routes_the_flow_statements`, and the four DML ones by
/// `dispatch_routes_the_dml_statements`.
#[test]
fn dispatch_routes_every_statement() {
    // One line per row of the routing table that still reaches a stub. `MERGE` is
    // the one left. A new stubbed row means a new line here.
    let cases: [(&str, &str); 1] = [("MERGE t USING s ON 1 = 1", "dml::parse_merge")];
    for (text, expected) in cases {
        routes_to(text, expected);
    }
}

/// The eleven rows the routing table sends to `flow`.
///
/// `BEGIN TRY` and `GOTO` are routed too, but their rule refuses them on purpose
/// (V2 and V1 respectively), so they are checked by `dispatch_refuses_what_v1_has_not`
/// rather than by the variant they would have yielded.
#[test]
fn dispatch_routes_the_flow_statements() {
    let cases: [(&str, &str); 17] = [
        ("DECLARE @x int", "DECLARE"),
        ("SET @x = 1", "SET"),
        ("SET NOCOUNT ON", "SET OPTION"),
        ("IF 1 = 1 PRINT 'x'", "IF"),
        ("WHILE 1 = 1 BREAK", "WHILE"),
        ("BEGIN PRINT 'x' END", "BLOCK"),
        ("BEGIN TRANSACTION", "BEGIN TRANSACTION"),
        ("COMMIT", "COMMIT"),
        ("ROLLBACK", "ROLLBACK"),
        ("SAVE TRANSACTION s", "SAVE"),
        ("PRINT 'x'", "PRINT"),
        ("EXEC p", "EXECUTE"),
        ("EXECUTE p", "EXECUTE"),
        ("RETURN", "RETURN"),
        ("BREAK", "BREAK"),
        ("CONTINUE", "CONTINUE"),
        ("WAITFOR DELAY '00:00:01'", "WAITFOR"),
    ];
    for (text, expected) in cases {
        routes_to_a_flow_statement(text, expected);
    }
    // `THROW` on its own would be routed too, but `THROW 50000, 'x', 1` is the shape
    // the AST fills, and both reach the same rule.
    routes_to_a_flow_statement("THROW 50000, 'x', 1", "THROW");
}

/// The two constructs the routing table sends to a rule that refuses them: `GOTO` and
/// `BEGIN TRY`, both out of the V1 subset (see `flow.rs`). A syntax error is what
/// proves nothing else claimed them.
#[test]
fn dispatch_refuses_what_v1_has_not() {
    // SQL Server: `GOTO l` => 133 (VaubanDB still 156);
    // `BEGIN TRY SELECT 1 END TRY` => 102 near TRY.
    for (text, expected) in [("GOTO l", 156), ("BEGIN TRY SELECT 1 END TRY", 102)] {
        let (number, message) = dispatch(text, true);
        assert_eq!(number, expected, "{text}: {number} {message}");
    }
}

/// The three statements the routing table sends to `ddl_table`: they parse, so what
/// proves the routing is the variant they yield.
#[test]
fn dispatch_routes_the_table_statements() {
    routes_to_a_table_statement("CREATE TABLE t (a int)", "CREATE TABLE");
    routes_to_a_table_statement("ALTER TABLE t ADD a int", "ALTER TABLE");
    routes_to_a_table_statement("DROP TABLE t", "DROP TABLE");
}

#[test]
fn dispatch_ignores_the_case_of_the_head_keyword() {
    routes_to_a_select("select 1");
    routes_to_a_table_statement("Create Table t (a int)", "CREATE TABLE");
}

/// The three heads of a `SELECT` statement reach `query::parse_select_statement`:
/// `SELECT` and the parenthesised query of a set operation both parse, while the
/// `WITH` of a common table expression is refused there (V2).
/// A refusal is a syntax error, and a text that was not routed at all would give the
/// very same 102: what proves the routing is that the first two parse.
#[test]
fn dispatch_routes_a_select() {
    routes_to_a_select("SELECT 1");
    routes_to_a_select("(SELECT 1) UNION SELECT 2");
    let text = "WITH c AS (SELECT 1) SELECT * FROM c";
    let (number, message) = dispatch(text, true);
    // `WITH` is reserved and unexpected here, so this is a 156. This is a known gap:
    // SQL Server does parse the common table expression and answers 8155, severity
    // 16, state 2 on this very text; the number will follow when table expression
    // queries are read.
    assert_eq!(number, 156, "{text}: {number} {message}");
}

/// The six spellings of the head of a `CREATE INDEX` reach the rule of `ddl_db`,
/// which parses them: a text that was not routed would be a syntax error instead.
#[test]
fn dispatch_reads_the_adjectives_of_a_create_index() {
    for text in [
        "CREATE INDEX ix ON t (a)",
        "CREATE UNIQUE INDEX ix ON t (a)",
        "CREATE CLUSTERED INDEX ix ON t (a)",
        "CREATE NONCLUSTERED INDEX ix ON t (a)",
        "CREATE UNIQUE CLUSTERED INDEX ix ON t (a)",
        "CREATE UNIQUE NONCLUSTERED INDEX ix ON t (a)",
    ] {
        let statement = routes_to_a_statement(text);
        assert!(
            matches!(statement, Statement::CreateIndex(_)),
            "{text} is a CREATE INDEX, got {statement:?}"
        );
    }
}

/// The six database and index statements that parse. What proves the routing is that they
/// parse at all, and into the variant of their own statement; their grammar is tested
/// by `tests/ddl_database.rs`.
#[test]
fn dispatch_routes_the_ddl_of_databases_and_indexes() {
    assert!(matches!(
        routes_to_a_statement("CREATE DATABASE d"),
        Statement::CreateDatabase(_)
    ));
    assert!(matches!(
        routes_to_a_statement("ALTER DATABASE d SET SINGLE_USER"),
        Statement::AlterDatabase(_)
    ));
    assert!(matches!(
        routes_to_a_statement("DROP DATABASE d"),
        Statement::DropDatabase { .. }
    ));
    assert!(matches!(
        routes_to_a_statement("USE d"),
        Statement::Use { .. }
    ));
    assert!(matches!(
        routes_to_a_statement("CREATE UNIQUE INDEX ix ON t (a)"),
        Statement::CreateIndex(_)
    ));
    assert!(matches!(
        routes_to_a_statement("DROP INDEX ix ON t"),
        Statement::DropIndex(_)
    ));
}

#[test]
fn dispatch_names_the_object_of_an_unsupported_ddl() {
    // These exact forms reach SQL Server semantic analysis or succeed.
    // The current parser refusal is fixed explicitly, not presented as compatibility.
    for (text, object, expected) in [
        // SQL Server: `CREATE PROCEDURE p AS SELECT 1` => success; VaubanDB remains 156.
        ("CREATE PROCEDURE p AS SELECT 1", "PROCEDURE", 156),
        // SQL Server: `CREATE PROC p AS SELECT 1` => success; VaubanDB remains 156.
        ("CREATE PROC p AS SELECT 1", "PROC", 156),
        // SQL Server: `CREATE FUNCTION f () RETURNS int AS BEGIN RETURN 1 END` => success; VaubanDB remains 156.
        (
            "CREATE FUNCTION f () RETURNS int AS BEGIN RETURN 1 END",
            "FUNCTION",
            156,
        ),
        // SQL Server: `CREATE VIEW v AS SELECT 1` => 4511; VaubanDB remains 156.
        ("CREATE VIEW v AS SELECT 1", "VIEW", 156),
        // SQL Server: `CREATE TRIGGER g ON t AFTER INSERT AS SELECT 1` => 8197; VaubanDB remains 156.
        (
            "CREATE TRIGGER g ON t AFTER INSERT AS SELECT 1",
            "TRIGGER",
            156,
        ),
        // SQL Server: `CREATE SCHEMA s` => success; VaubanDB remains 156.
        ("CREATE SCHEMA s", "SCHEMA", 156),
        // SQL Server: `CREATE SEQUENCE s` => success; VaubanDB remains 102.
        ("CREATE SEQUENCE s", "SEQUENCE", 102),
        // SQL Server: `CREATE TYPE ty FROM int` => success; VaubanDB remains 102.
        ("CREATE TYPE ty FROM int", "TYPE", 102),
        // SQL Server: `CREATE LOGIN l WITH PASSWORD = 'x'` => 33062; VaubanDB remains 102.
        ("CREATE LOGIN l WITH PASSWORD = 'x'", "LOGIN", 102),
        // SQL Server: `CREATE USER u` => 15007; VaubanDB remains 156.
        ("CREATE USER u", "USER", 156),
        // SQL Server: `CREATE ROLE r` => success; VaubanDB remains 102.
        ("CREATE ROLE r", "ROLE", 102),
        // SQL Server: `ALTER PROCEDURE p AS SELECT 1` => 208; VaubanDB remains 156.
        ("ALTER PROCEDURE p AS SELECT 1", "PROCEDURE", 156),
        // SQL Server: `DROP VIEW v` => 3701; VaubanDB remains 156.
        ("DROP VIEW v", "VIEW", 156),
        // SQL Server: `DROP TYPE ty` => 218; VaubanDB remains 102.
        ("DROP TYPE ty", "TYPE", 102),
    ] {
        let (number, message) = dispatch(text, true);
        assert_eq!(number, expected, "{text}: {number} {message}");
        assert!(
            message.contains(object),
            "{text} should name {object}, got {message}"
        );
    }
}

#[test]
fn dispatch_begin_is_ambiguous() {
    routes_to_a_flow_statement("BEGIN SELECT 1 END", "BLOCK");
    routes_to_a_flow_statement("BEGIN TRAN", "BEGIN TRANSACTION");
    routes_to_a_flow_statement("BEGIN TRANSACTION t", "BEGIN TRANSACTION");
    // The third `BEGIN` is refused: `BEGIN TRY` is V2 (see `dispatch_refuses_what_v1_has_not`).
    let (number, message) = dispatch("BEGIN TRY SELECT 1 END TRY", true);
    // SQL Server: `BEGIN TRY SELECT 1 END TRY` => 102 near TRY.
    assert_eq!(number, 102, "{number} {message}");
}

#[test]
fn dispatch_unknown_word_is_syntax_error() {
    // `FOO` is no keyword, so the lexer makes an `Ident` of it and no rule claims it.
    // In the middle of a batch that is a syntax error; at its head it is the implicit
    // `EXEC`, which SQL Server answers with 2812 (unknown stored procedure `FOO`)
    // rather than with a syntax error.
    let (number, message) = dispatch("FOO 1", false);
    assert_eq!(number, 102, "{message}");
    assert_eq!(message, "Syntax error near 'FOO'.");

    match try_dispatch("FOO 1", true) {
        Ok(Statement::Execute(execute)) => assert!(execute.implicit, "FOO 1 is implicit"),
        Ok(other) => unreachable!("FOO 1 heads a batch, so it is an EXEC, got {other:?}"),
        Err((number, message)) => unreachable!("FOO 1 heads a batch: {number} {message}"),
    }
}

#[test]
fn dispatch_refuses_an_incomplete_ddl() {
    // `CREATE` followed by nothing the table knows is a syntax error, not a route.
    // The dispatch consumes nothing, so the error falls on the head keyword itself,
    // which is reserved in the three: 156. The dispatch does not reach
    // the end of the text, so the 102 of a truncated batch does not apply here.
    for text in ["CREATE", "ALTER x", "DROP 1"] {
        let (number, _) = dispatch(text, false);
        assert_eq!(number, 156, "{text} should not be routed");
    }
    // Artefacts of the grammar, kept as they are and to be revisited when the object
    // types of `CREATE`/`ALTER`/`DROP` are read. SQL Server consumes the head
    // word before it chokes: `CREATE` answers a 102 near 'CREATE' (the end of the
    // text, hence a 102 and not a 156), `DROP 1` answers a 102 near '1' (the error
    // falls on what follows the head word), and `ALTER x` answers 343 (unknown object
    // type), which is not a syntax error.
}

/// The dispatch hands the cursor over untouched, whatever the number of tokens it had
/// to look at to decide.
///
/// Two ways of seeing it: on a statement that still reaches a stub, here the
/// two-token lookahead
/// of `BEGIN TRY`, the cursor has not moved at all; on the longest lookahead of the
/// table, `CREATE UNIQUE CLUSTERED INDEX`, which `ddl_db` parses, the span of the
/// statement produced starts at the very first byte of the batch, which only the
/// owner rule reading its own `CREATE` back can give.
#[test]
fn dispatch_consumes_nothing_before_routing() {
    static OPTIONS: ParseOptions = ParseOptions {
        quoted_identifier: true,
    };
    let mut p = match Parser::new("BEGIN TRY SELECT 1 END TRY", &OPTIONS) {
        Ok(parser) => parser,
        Err(error) => unreachable!("it lexes: {error:?}"),
    };
    let start = p.mark();
    let _ = parse_statement(&mut p, true);
    assert_eq!(p.mark(), start);

    let text = "CREATE UNIQUE CLUSTERED INDEX ix ON t (a)";
    match routes_to_a_statement(text) {
        Statement::CreateIndex(create) => {
            assert_eq!(create.span.offset, 0);
            assert_eq!(create.span.len as usize, text.len());
        }
        other => unreachable!("{text} is a CREATE INDEX, got {other:?}"),
    }
}
