//! `INSERT`, `UPDATE`, `DELETE` and `TRUNCATE TABLE`.
//!
//! Everything here goes through the public entry point of the crate, `parse_batch`: what
//! a client sends is a batch, and the shape of the AST it yields is what `binder` and
//! `executor` are written against.
//!
//! Each accepted form and each refused one below follows SQL Server's grammar. Where SQL
//! Server's answer differs from what the parser produces, it is quoted next to the
//! assertion.
//!
//! # The `parse` -> `Display` -> `parse` loop
//!
//! [`rt`] is the loop the module README makes a contract: for any accepted text, parsing
//! what `Display` wrote yields an **equal** `Batch`. The text `Display` writes is not the
//! source text, and the assumed deviations (`INSERT t` written back `INSERT INTO t`,
//! `DELETE t` written back `DELETE FROM t`) are asserted here as deviations rather than
//! hidden.

use vauban_errors::SqlError;
use vauban_parser::{
    AssignOp, AssignTarget, Batch, DeleteStatement, Expr, InsertSource, InsertStatement, Literal,
    ObjectName, ParseOptions, SelectItem, Span, Statement, TableRef, UpdateStatement, parse_batch,
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
        None => unreachable!("{text} is one statement"),
    }
}

/// The one `INSERT` of `text`.
fn insert(text: &str) -> InsertStatement {
    match one(text) {
        Statement::Insert(insert) => *insert,
        other => unreachable!("{text} is an INSERT, got {other:?}"),
    }
}

/// The one `UPDATE` of `text`.
fn update(text: &str) -> UpdateStatement {
    match one(text) {
        Statement::Update(update) => *update,
        other => unreachable!("{text} is an UPDATE, got {other:?}"),
    }
}

/// The one `DELETE` of `text`.
fn delete(text: &str) -> DeleteStatement {
    match one(text) {
        Statement::Delete(delete) => *delete,
        other => unreachable!("{text} is a DELETE, got {other:?}"),
    }
}

/// An integer literal expression, for comparison against a parsed one.
fn integer(text: &str) -> Expr {
    Expr::Literal(Literal::Integer(text.to_owned()), Span::EMPTY)
}

/// The unqualified name of a `TableRef::Table`, hints and alias ignored.
fn table_name(table_ref: &TableRef) -> String {
    match table_ref {
        TableRef::Table { name, .. } => name.name.value.clone(),
        other => unreachable!("expected a named table, got {other:?}"),
    }
}

/// The names of the columns of an object name written as a list.
fn names(columns: &[vauban_parser::Ident]) -> Vec<String> {
    columns.iter().map(|c| c.value.clone()).collect()
}

#[test]
fn insert_values() {
    let statement = insert("INSERT INTO t (a, b) VALUES (1, 2)");
    assert_eq!(table_name(&statement.target), "t");
    assert_eq!(names(&statement.columns), ["a", "b"]);
    match &statement.source {
        InsertSource::Values(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0], [integer("1"), integer("2")]);
        }
        other => unreachable!("expected VALUES, got {other:?}"),
    }
    assert!(statement.top.is_none());
    assert!(statement.output.is_none());

    let statement = insert("INSERT INTO t VALUES (1, 2), (3, 4)");
    assert!(statement.columns.is_empty());
    match &statement.source {
        InsertSource::Values(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[1], [integer("3"), integer("4")]);
        }
        other => unreachable!("expected VALUES, got {other:?}"),
    }

    // `INSERT t VALUES (1)` without `INTO` is legal T-SQL, and
    // the AST does not record that the word was left out: `Display` writes it back of its
    // own accord. A deliberate deviation.
    assert_eq!(rt("INSERT t VALUES (1)"), "INSERT INTO t VALUES (1)");
    assert_eq!(p("INSERT t VALUES (1)"), p("INSERT INTO t VALUES (1)"));

    assert_eq!(
        rt("INSERT INTO dbo.t (a, b) VALUES (1, 2), (3, 4)"),
        "INSERT INTO dbo.t (a, b) VALUES (1, 2), (3, 4)"
    );
}

#[test]
fn insert_default_values() {
    assert_eq!(
        insert("INSERT INTO t DEFAULT VALUES").source,
        InsertSource::DefaultValues
    );
    // `INSERT INTO t (a) DEFAULT VALUES` is accepted by SQL Server too.
    assert_eq!(
        insert("INSERT INTO t (a) DEFAULT VALUES").source,
        InsertSource::DefaultValues
    );
    assert_eq!(
        rt("INSERT INTO t DEFAULT VALUES"),
        "INSERT INTO t DEFAULT VALUES"
    );

    match insert("INSERT INTO t (a) VALUES (DEFAULT)").source {
        InsertSource::Values(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0], [Expr::Literal(Literal::Default, Span::EMPTY)]);
        }
        other => unreachable!("expected VALUES, got {other:?}"),
    }
    assert_eq!(
        rt("INSERT INTO t (a) VALUES (DEFAULT)"),
        "INSERT INTO t (a) VALUES (DEFAULT)"
    );
}

#[test]
fn insert_select() {
    let statement = insert("INSERT INTO t (a) SELECT b FROM u WHERE b > 1");
    assert_eq!(names(&statement.columns), ["a"]);
    assert!(matches!(statement.source, InsertSource::Query(_)));
    assert_eq!(
        rt("INSERT INTO t (a) SELECT b FROM u WHERE b > 1"),
        "INSERT INTO t (a) SELECT b FROM u WHERE b > 1"
    );

    // The whole set operation is the source, not just its first operand.
    match insert("INSERT INTO t SELECT 1 UNION SELECT 2").source {
        InsertSource::Query(query) => {
            assert!(matches!(query.body, vauban_parser::QueryBody::SetOp { .. }))
        }
        other => unreachable!("expected a query, got {other:?}"),
    }
    assert_eq!(
        rt("INSERT INTO t SELECT 1 UNION SELECT 2"),
        "INSERT INTO t SELECT 1 UNION SELECT 2"
    );

    // A parenthesised source, which SQL Server accepts after a column list.
    assert!(matches!(
        insert("INSERT INTO t (a) (SELECT 1)").source,
        InsertSource::Query(_)
    ));
}

#[test]
fn insert_top_and_output() {
    let statement = insert("INSERT TOP (5) INTO t (a) SELECT b FROM u");
    match &statement.top {
        Some(top) => {
            assert_eq!(top.expr, integer("5"));
            assert!(top.parenthesized);
            assert!(!top.percent);
            assert!(!top.with_ties);
        }
        None => unreachable!("the TOP clause is there"),
    }
    assert_eq!(
        rt("INSERT TOP (5) INTO t (a) SELECT b FROM u"),
        "INSERT TOP (5) INTO t (a) SELECT b FROM u"
    );
    assert_eq!(
        rt("INSERT TOP (50) PERCENT INTO t (a) SELECT b FROM u"),
        "INSERT TOP (50) PERCENT INTO t (a) SELECT b FROM u"
    );

    let statement = insert("INSERT INTO t (a) OUTPUT INSERTED.id VALUES (1)");
    let output = match statement.output {
        Some(output) => output,
        None => unreachable!("the OUTPUT clause is there"),
    };
    assert_eq!(output.items.len(), 1);
    match &output.items[0] {
        SelectItem::Expr {
            expr: Expr::Column(column),
            ..
        } => {
            assert_eq!(column.name.value, "id");
            match &column.qualifier {
                Some(ObjectName { name, .. }) => assert_eq!(name.value, "INSERTED"),
                None => unreachable!("INSERTED. is the qualifier"),
            }
        }
        other => unreachable!("expected INSERTED.id, got {other:?}"),
    }
    assert!(output.into.is_none());
    assert_eq!(
        rt("INSERT INTO t (a) OUTPUT INSERTED.id VALUES (1)"),
        "INSERT INTO t (a) OUTPUT INSERTED.id VALUES (1)"
    );

    // `OUTPUT … INTO @out (id)` fills `output.into` with a `TableRef::Variable`
    // which `Display` writes back as written: `INTO @out`, not `INTO [@out]`.
    let text = "INSERT INTO t (a) OUTPUT INSERTED.id INTO @out (id) VALUES (1)";
    let statement = insert(text);
    let output = match statement.output {
        Some(output) => output,
        None => unreachable!("the OUTPUT clause is there"),
    };
    match &output.into {
        Some(TableRef::Variable { name, alias, .. }) => {
            assert_eq!(name, "@out");
            assert!(alias.is_none());
        }
        other => unreachable!("INTO @out fills `into` with a variable, got {other:?}"),
    }
    assert_eq!(names(&output.into_columns), ["id"]);
    assert_eq!(rt(text), text);

    // The same clause on a real table does round-trip.
    assert_eq!(
        rt("INSERT INTO t (a) OUTPUT INSERTED.id INTO #x (id) VALUES (1)"),
        "INSERT INTO t (a) OUTPUT INSERTED.id INTO #x (id) VALUES (1)"
    );
}

/// (V2) `INSERT … EXEC` goes through `flow::parse_execute`.
#[test]
fn insert_execute() {
    assert!(matches!(
        insert("INSERT INTO t EXEC dbo.p").source,
        InsertSource::Execute(_)
    ));
    assert!(matches!(
        insert("INSERT INTO t (a) EXECUTE dbo.p 1, 2").source,
        InsertSource::Execute(_)
    ));
}

#[test]
fn update_simple() {
    let statement = update("UPDATE t SET a = 1");
    assert_eq!(table_name(&statement.target), "t");
    assert_eq!(statement.assignments.len(), 1);
    assert_eq!(statement.assignments[0].op, AssignOp::Set);
    assert_eq!(statement.assignments[0].value, integer("1"));
    assert!(statement.from.is_empty());
    assert!(statement.where_.is_none());
    assert_eq!(rt("UPDATE t SET a = 1"), "UPDATE t SET a = 1");

    let statement = update("UPDATE t SET a = 1, b = 2 WHERE c = 3");
    assert_eq!(statement.assignments.len(), 2);
    assert!(statement.where_.is_some());
    assert_eq!(
        rt("UPDATE t SET a = 1, b = 2 WHERE c = 3"),
        "UPDATE t SET a = 1, b = 2 WHERE c = 3"
    );

    // A qualified target column, and a schema-qualified target table.
    assert_eq!(rt("UPDATE dbo.t SET t.a = 1"), "UPDATE dbo.t SET t.a = 1");
    // `WITH (…)` on the target, which SQL Server accepts.
    assert_eq!(
        rt("UPDATE t WITH (TABLOCK) SET a = 1"),
        "UPDATE t WITH (TABLOCK) SET a = 1"
    );
}

#[test]
fn update_compound_operators() {
    let text = "UPDATE t SET a += 1, b -= 2, c *= 3, d /= 4, e %= 5, f &= 6, g |= 7, h ^= 8";
    let statement = update(text);
    let ops: Vec<AssignOp> = statement.assignments.iter().map(|a| a.op).collect();
    assert_eq!(
        ops,
        [
            AssignOp::AddAssign,
            AssignOp::SubAssign,
            AssignOp::MulAssign,
            AssignOp::DivAssign,
            AssignOp::ModAssign,
            AssignOp::BitAndAssign,
            AssignOp::BitOrAssign,
            AssignOp::BitXorAssign,
        ]
    );
    assert_eq!(rt(text), text);
}

#[test]
fn update_from_and_alias() {
    let text = "UPDATE a SET a.x = b.y FROM t AS a INNER JOIN u AS b ON a.id = b.id WHERE b.z = 1";
    let statement = update(text);
    // The target is a name that the `FROM` declares as an alias; the parser records it as
    // a plain `TableRef::Table` and the binder resolves it.
    assert_eq!(table_name(&statement.target), "a");
    assert_eq!(statement.from.len(), 1);
    assert!(matches!(statement.from[0], TableRef::Join { .. }));
    assert!(statement.where_.is_some());
    assert_eq!(rt(text), text);
}

#[test]
fn update_variable_target() {
    let statement = update("UPDATE t SET @x = a");
    assert_eq!(
        statement.assignments[0].target,
        AssignTarget::Variable("@x".to_owned())
    );
    assert_eq!(rt("UPDATE t SET @x = a"), "UPDATE t SET @x = a");

    // A `SET` list may mix the two kinds of target, as SQL Server does.
    let statement = update("UPDATE t SET a = 1, @x = 2");
    assert!(matches!(
        statement.assignments[0].target,
        AssignTarget::Column(_)
    ));
    assert!(matches!(
        statement.assignments[1].target,
        AssignTarget::Variable(_)
    ));
}

#[test]
fn update_top_and_output() {
    let statement = update("UPDATE TOP (1) t SET a = 1");
    match &statement.top {
        Some(top) => {
            assert_eq!(top.expr, integer("1"));
            assert!(top.parenthesized);
        }
        None => unreachable!("the TOP clause is there"),
    }
    assert_eq!(
        rt("UPDATE TOP (1) t SET a = 1"),
        "UPDATE TOP (1) t SET a = 1"
    );
    assert_eq!(
        rt("UPDATE TOP (50) PERCENT t SET a = 1"),
        "UPDATE TOP (50) PERCENT t SET a = 1"
    );

    let text = "UPDATE t SET a = 1 OUTPUT DELETED.a, INSERTED.a WHERE b = 2";
    let statement = update(text);
    match &statement.output {
        Some(output) => assert_eq!(output.items.len(), 2),
        None => unreachable!("the OUTPUT clause is there"),
    }
    assert!(statement.where_.is_some());
    assert_eq!(rt(text), text);

    // `OUTPUT` stands before the `FROM` of the sources and before the `WHERE`; written
    // after the `WHERE` it is a syntax error, as on SQL Server (102 near 'OUTPUT').
    assert_eq!(
        rt("UPDATE t SET a = 1 OUTPUT DELETED.a FROM u WHERE b = 2"),
        "UPDATE t SET a = 1 OUTPUT DELETED.a FROM u WHERE b = 2"
    );
    let error = p_err("UPDATE t SET a = 1 WHERE b = 2 OUTPUT DELETED.a");
    assert_eq!(error.number, 102);
    assert_eq!(error.message, "Syntax error near 'OUTPUT'.");
}

#[test]
fn delete_forms() {
    // `DELETE FROM t` and `DELETE t` are the same statement; `Display` always writes the
    // decorative `FROM`. A deliberate deviation.
    assert_eq!(rt("DELETE FROM t"), "DELETE FROM t");
    assert_eq!(rt("DELETE t"), "DELETE FROM t");
    assert_eq!(p("DELETE t"), p("DELETE FROM t"));

    let statement = delete("DELETE FROM t WHERE a = 1");
    assert_eq!(table_name(&statement.target), "t");
    assert!(statement.where_.is_some());
    assert!(statement.from.is_empty());
    assert_eq!(rt("DELETE FROM t WHERE a = 1"), "DELETE FROM t WHERE a = 1");

    let statement = delete("DELETE TOP (10) FROM t");
    match &statement.top {
        Some(top) => {
            assert_eq!(top.expr, integer("10"));
            assert!(top.parenthesized);
        }
        None => unreachable!("the TOP clause is there"),
    }
    assert_eq!(rt("DELETE TOP (10) FROM t"), "DELETE TOP (10) FROM t");

    // The two `FROM`: the target on one side, the join sources on the other.
    let text = "DELETE a FROM t AS a INNER JOIN u AS b ON a.id = b.id WHERE b.z = 1";
    let statement = delete(text);
    assert_eq!(table_name(&statement.target), "a");
    assert_eq!(statement.from.len(), 1);
    assert!(matches!(statement.from[0], TableRef::Join { .. }));
    assert_eq!(
        rt(text),
        "DELETE FROM a FROM t AS a INNER JOIN u AS b ON a.id = b.id WHERE b.z = 1"
    );
    // Both `FROM` written out is legal T-SQL and yields the same AST.
    assert_eq!(p(text), p(&rt(text)));

    let text = "DELETE FROM t OUTPUT DELETED.id WHERE a = 1";
    let statement = delete(text);
    assert!(statement.output.is_some());
    assert_eq!(rt(text), text);

    // `WITH (…)` on the target, which SQL Server accepts.
    assert_eq!(
        rt("DELETE FROM t WITH (TABLOCK) WHERE a = 1"),
        "DELETE FROM t WITH (TABLOCK) WHERE a = 1"
    );
}

#[test]
fn truncate() {
    match one("TRUNCATE TABLE dbo.t") {
        Statement::Truncate { table, .. } => {
            assert_eq!(table.name.value, "t");
            match &table.schema {
                Some(schema) => assert_eq!(schema.value, "dbo"),
                None => unreachable!("dbo. is the schema"),
            }
        }
        other => unreachable!("expected a TRUNCATE, got {other:?}"),
    }
    assert_eq!(rt("TRUNCATE TABLE dbo.t"), "TRUNCATE TABLE dbo.t");
    assert_eq!(rt("TRUNCATE TABLE t"), "TRUNCATE TABLE t");

    // `TABLE` is mandatory: SQL Server answers `TRUNCATE t` with a 102 near 't'.
    let error = p_err("TRUNCATE t");
    assert_eq!(error.number, 102);
    assert_eq!(error.message, "Syntax error near 't'.");
}

#[test]
fn merge_is_not_supported_yet() {
    // `MERGE` is V3: `dml::parse_merge` is a stub and yields the generic internal error
    // 50000. SQL Server answers a syntax error 102 (near '1' on this text), so the
    // difference is visible to a client, and deliberate.
    let error = p_err("MERGE t AS a USING u AS b ON 1 = 1");
    assert_eq!(error.number, 50000);
}

#[test]
fn dml_errors() {
    // A **truncated** batch is reported with a 102 on the last token read, whether or not
    // that token is a reserved word: `INSERT INTO t VALUES` answers a 102 near 'VALUES'
    // and `UPDATE t SET` a 102 near 'SET', not 156; the 156 is for a reserved word that
    // is present and out of place. That rule lives in `syntax_error.rs` and is asserted
    // by `tests/syntax_errors.rs`, so only the number is asserted here for the truncated
    // texts.
    //
    // `DELETE` alone is no exception, and there is no second path: a batch reduced to a
    // reserved word is parsed like any other batch. Sent as an ordinary batch:
    // `DELETE` -> 102 near 'DELETE', `SELECT` -> 102 near 'SELECT', `UPDATE` -> 102 near
    // 'UPDATE', `SET` -> 156 near the keyword 'SET'. The implicit `EXECUTE` only opens on
    // a bare **name**: `foo` alone, and only that shape, answers 2812 (unknown stored
    // procedure).
    //
    // A client that sends a one-word text as a procedure call rather than as a batch
    // gets a 2812 on each line above instead.
    for text in [
        "INSERT INTO t",           // 102, near 't'
        "INSERT INTO t VALUES",    // 102, near 'VALUES'
        "UPDATE t",                // 102, near 't'
        "UPDATE t SET",            // 102, near 'SET'
        "DELETE",                  // 102, near 'DELETE'
        "INSERT INTO t (a)",       // 102, near ')'
        "UPDATE t SET a = 1 FROM", // 102, near 'FROM'
        "DELETE FROM t WHERE",     // 102, near 'WHERE'
    ] {
        assert_eq!(p_err(text).number, 102, "{text}");
    }

    // A token that is present: the message is already exact.
    let error = p_err("TRUNCATE t");
    assert_eq!(error.number, 102);
    assert_eq!(error.message, "Syntax error near 't'.");

    // `UPDATE t AS a SET a = 1` and `UPDATE t a SET a = 1` are both refused by SQL
    // Server: a DML target takes no alias of its own.
    // SQL Server: `UPDATE t AS a SET a = 1` => 156; `UPDATE t a SET a = 1` => 102.
    for (text, number) in [
        ("UPDATE t AS a SET a = 1", 156),
        ("UPDATE t a SET a = 1", 102),
    ] {
        assert_eq!(p_err(text).number, number, "{text}");
    }

    // `WITH TIES` belongs to `SELECT`: `DELETE TOP (1) WITH TIES FROM t` is a 156 near
    // 'WITH' on SQL Server. The words are simply never read here, so the error falls on
    // `WITH` too.
    assert_eq!(p_err("DELETE TOP (1) WITH TIES FROM t").number, 156);

    // `(` after an INSERT target always opens the column list, never a query.
    // SQL Server: `INSERT INTO t (SELECT 1)` => 156.
    assert_eq!(p_err("INSERT INTO t (SELECT 1)").number, 156);
}

/// The target of an `INSERT`: a table variable, a temporary table, a name of two to four
/// parts, a hint list. Each text is one SQL Server accepts, the variable declared in the
/// same batch (an undeclared one is a 1087).
#[test]
fn insert_targets() {
    // `INSERT INTO @t (c) VALUES (1)`: the batch `proc_table_variable_join` of the corpus
    // was refused for this one form (102 near '@Ids').
    let text = "INSERT INTO @t (c) VALUES (1)";
    let statement = insert(text);
    match &statement.target {
        TableRef::Variable { name, alias, .. } => {
            assert_eq!(name, "@t");
            assert!(alias.is_none());
        }
        other => unreachable!("@t is a table variable, got {other:?}"),
    }
    assert_eq!(names(&statement.columns), ["c"]);
    assert_eq!(rt(text), text);

    // `INSERT @t`, without `INTO`: same AST, `INTO` written back (a deliberate deviation).
    assert_eq!(p("INSERT @t (c) VALUES (1)"), p(text));

    // The hints of a named target are kept, and written back where they stand: before the
    // column list.
    let statement = insert("INSERT INTO dbo.t WITH (TABLOCK, HOLDLOCK) (c) VALUES (1)");
    match &statement.target {
        TableRef::Table { name, hints, .. } => {
            assert_eq!(name.name.value, "t");
            let hint_names: Vec<&str> = hints.iter().map(|h| h.name.as_str()).collect();
            assert_eq!(hint_names, ["TABLOCK", "HOLDLOCK"]);
        }
        other => unreachable!("dbo.t is a named table, got {other:?}"),
    }

    // `[@t]` is a bracketed name, not a variable: SQL Server accepts it as a table name.
    match &insert("INSERT INTO [@t] (c) VALUES (1)").target {
        TableRef::Table { name, .. } => {
            assert_eq!(name.name.value, "@t");
            assert!(name.name.quoted);
        }
        other => unreachable!("[@t] is a quoted name, got {other:?}"),
    }

    for text in [
        "INSERT INTO @t (c) VALUES (1)",
        "INSERT INTO @t SELECT 1",
        "INSERT INTO @t DEFAULT VALUES",
        "INSERT INTO @t (c) OUTPUT INSERTED.c VALUES (1)",
        "INSERT INTO @t (c) OUTPUT INSERTED.c INTO @o (a) VALUES (1)",
        "INSERT TOP (1) INTO @t (c) SELECT 1",
        "INSERT INTO #t (c) VALUES (1)",
        "INSERT INTO ##t (c) VALUES (1)",
        "INSERT INTO db..t (c) VALUES (1)",
        "INSERT INTO [srv].[db].[dbo].[t] (c) VALUES (1)",
        "INSERT INTO t WITH (TABLOCK) (c) VALUES (1)",
        "INSERT INTO dbo.t WITH (TABLOCK, HOLDLOCK) (c) VALUES (1)",
        "INSERT INTO [@t] (c) VALUES (1)",
    ] {
        assert_eq!(rt(text), text, "{text} does not print back as written");
    }
}

/// What SQL Server refuses after the target of an `INSERT`, the variable declared in the
/// batch: the number and the token.
#[test]
fn insert_target_errors() {
    for (text, number, message) in [
        // A table variable takes no hint list.
        (
            "INSERT INTO @t WITH (TABLOCK) (c) VALUES (1)",
            156,
            "Syntax error near the keyword 'WITH'.",
        ),
        // A target takes no alias, whatever its kind.
        (
            "INSERT INTO @t AS x (c) VALUES (1)",
            156,
            "Syntax error near the keyword 'AS'.",
        ),
        (
            "INSERT INTO @t x (c) VALUES (1)",
            102,
            "Syntax error near 'x'.",
        ),
        (
            "INSERT INTO f() AS x (c) VALUES (1)",
            156,
            "Syntax error near the keyword 'AS'.",
        ),
        // A variable has no parts.
        (
            "INSERT INTO @t.x (c) VALUES (1)",
            102,
            "Syntax error near '.'.",
        ),
        // The parenthesised hint list without `WITH` is a column list here, and what
        // follows it is out of place.
        (
            "INSERT INTO @t (TABLOCK) (c) VALUES (1)",
            102,
            "Syntax error near 'c'.",
        ),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}");
        assert_eq!(error.message, message, "{text}");
    }
}

/// `INSERT INTO f() …`: a name given parentheses is **accepted** by the grammar of SQL
/// Server, and refused later by name
/// resolution: `INSERT INTO dbo.f(1) (c) VALUES (1)` is a 208
/// `Unknown object name 'dbo.f'.`, and on a temporary table `INSERT INTO #q() (c) VALUES
/// (1)` is a 215 `Parameters supplied for object '#q' which is not a function…`. The
/// parser yields a `TableRef::Function`, as `FROM f()` does, and the binder decides.
///
/// Where the argument list ends and the column list begins: see `parse_insert_target` in
/// `src/parser/dml.rs`. Each text below is one batch.
#[test]
fn insert_function_target() {
    let statement = insert("INSERT INTO dbo.f(1, @x) (c) VALUES (1)");
    match &statement.target {
        TableRef::Function {
            name, args, alias, ..
        } => {
            assert_eq!(name.name.value, "f");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], integer("1"));
            assert!(matches!(&args[1], Expr::Variable { name, .. } if name == "@x"));
            assert!(alias.is_none());
        }
        other => unreachable!("dbo.f(1, @x) is a function target, got {other:?}"),
    }
    assert_eq!(names(&statement.columns), ["c"]);

    // Accepted: an empty list, or a list whose first item is a value.
    for text in [
        "INSERT INTO f() (c) VALUES (1)",
        "INSERT INTO f() VALUES (1)",
        "INSERT INTO f() DEFAULT VALUES",
        "INSERT INTO f() OUTPUT INSERTED.c VALUES (1)",
        "INSERT INTO dbo.f(1) (c) VALUES (1)",
        "INSERT INTO f(1) VALUES (1)",
        "INSERT INTO f(1) SELECT 1",
        "INSERT INTO f(-1) (c) VALUES (1)",
        "INSERT INTO f(-1.5) (c) VALUES (1)",
        "INSERT INTO f(NULL) (c) VALUES (1)",
        "INSERT INTO f(DEFAULT) (c) VALUES (1)",
        "INSERT INTO f(1, 'a') (c) VALUES (1)",
        "INSERT INTO f(N'a') (c) VALUES (1)",
        "INSERT INTO f(1.5) (c) VALUES (1)",
        "INSERT INTO f(0x01) (c) VALUES (1)",
        "INSERT INTO f($1) (c) VALUES (1)",
        "INSERT INTO f(@x) (c) VALUES (1)",
        "INSERT INTO f(@x) VALUES (1)",
        "INSERT INTO [srv].[db].[dbo].[f](1) (c) VALUES (1)",
        "INSERT TOP (1) INTO f(1) (c) SELECT 1",
    ] {
        assert_eq!(rt(text), text, "{text} does not print back as written");
        assert!(
            matches!(insert(text).target, TableRef::Function { .. }),
            "{text} yields a function target"
        );
    }
    // `f(- 1)`, a blank between the sign and the digit, is accepted and printed `-1`.
    assert_eq!(
        p("INSERT INTO f(- 1) (c) VALUES (1)"),
        p("INSERT INTO f(-1) (c) VALUES (1)")
    );
    // `f(a)` alone is a column list, not a call.
    let statement = insert("INSERT INTO f(a) VALUES (1)");
    assert_eq!(table_name(&statement.target), "f");
    assert_eq!(names(&statement.columns), ["a"]);

    // Refused: an identifier opens the column list, and what follows it is out of place;
    // an argument is one value and nothing more; no hint follows the parentheses.
    for (text, number, message) in [
        (
            "INSERT INTO f(a) (c) VALUES (1)",
            102,
            "Syntax error near 'c'.",
        ),
        (
            "INSERT INTO f([a]) (c) VALUES (1)",
            102,
            "Syntax error near 'c'.",
        ),
        // Not asserted: `f(x.a) (c)`, a 102 near 'c' on SQL Server, is a 102 near '.'
        // here, because the column list of an INSERT does not read a qualified name
        // (`INSERT INTO #q (#q.a) VALUES (1)` is accepted by SQL Server and inserts).
        // A defect of the column list, not of the target.
        (
            "INSERT INTO f(a, b) (c) VALUES (1)",
            102,
            "Syntax error near 'c'.",
        ),
        (
            "INSERT INTO f(a, 1) (c) VALUES (1)",
            102,
            "Syntax error near '1'.",
        ),
        (
            "INSERT INTO f(1, a) (c) VALUES (1)",
            102,
            "Syntax error near 'a'.",
        ),
        (
            "INSERT INTO f(@x, a) (c) VALUES (1)",
            102,
            "Syntax error near 'a'.",
        ),
        (
            "INSERT INTO f(1 + 1) (c) VALUES (1)",
            102,
            "Syntax error near '+'.",
        ),
        (
            "INSERT INTO f(-1 + 1) (c) VALUES (1)",
            102,
            "Syntax error near '+'.",
        ),
        (
            "INSERT INTO f(1 - 1) (c) VALUES (1)",
            102,
            "Syntax error near '-'.",
        ),
        // A `+` opens no value; a `-` is followed by a number; a global variable is no
        // value.
        (
            "INSERT INTO f(+1) (c) VALUES (1)",
            102,
            "Syntax error near '+'.",
        ),
        (
            "INSERT INTO f(-a) (c) VALUES (1)",
            102,
            "Syntax error near 'a'.",
        ),
        (
            "INSERT INTO f(-@x) (c) VALUES (1)",
            102,
            "Syntax error near '@x'.",
        ),
        (
            "INSERT INTO f(-) (c) VALUES (1)",
            102,
            "Syntax error near ')'.",
        ),
        (
            "INSERT INTO f(@@ROWCOUNT) (c) VALUES (1)",
            102,
            "Syntax error near '@@ROWCOUNT'.",
        ),
        (
            "INSERT INTO f(1,) (c) VALUES (1)",
            102,
            "Syntax error near ')'.",
        ),
        (
            "INSERT INTO f(1 2) (c) VALUES (1)",
            102,
            "Syntax error near '2'.",
        ),
        (
            "INSERT INTO f(NULL, a) (c) VALUES (1)",
            102,
            "Syntax error near 'a'.",
        ),
        (
            "INSERT INTO f('a' + 'b') (c) VALUES (1)",
            102,
            "Syntax error near '+'.",
        ),
        (
            "INSERT INTO f(@x + 1) (c) VALUES (1)",
            102,
            "Syntax error near '+'.",
        ),
        (
            "INSERT INTO f((1)) (c) VALUES (1)",
            102,
            "Syntax error near '('.",
        ),
        (
            "INSERT INTO f((SELECT 1)) (c) VALUES (1)",
            102,
            "Syntax error near '('.",
        ),
        (
            "INSERT INTO f(*) (c) VALUES (1)",
            102,
            "Syntax error near '*'.",
        ),
        (
            "INSERT INTO f(GETDATE()) (c) VALUES (1)",
            102,
            "Syntax error near '('.",
        ),
        (
            "INSERT INTO f() () VALUES (1)",
            102,
            "Syntax error near ')'.",
        ),
        (
            "INSERT INTO f() WITH (TABLOCK) (c) VALUES (1)",
            156,
            "Syntax error near the keyword 'WITH'.",
        ),
        (
            "INSERT INTO f(1) WITH (TABLOCK) (c) VALUES (1)",
            156,
            "Syntax error near the keyword 'WITH'.",
        ),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}");
        assert_eq!(error.message, message, "{text}");
    }
}

/// The target of `OUTPUT … INTO`: a table variable or a name, and nothing after it, the
/// variables declared in the batch.
#[test]
fn output_into_targets() {
    for text in [
        "INSERT INTO t (a) OUTPUT INSERTED.a INTO @o (a) VALUES (1)",
        "INSERT INTO t (a) OUTPUT INSERTED.a INTO @o VALUES (1)",
        "INSERT INTO t (a) OUTPUT INSERTED.a INTO #o (a) VALUES (1)",
        "INSERT INTO t (a) OUTPUT INSERTED.a INTO dbo.o (a) VALUES (1)",
        "INSERT INTO t (a) OUTPUT INSERTED.a INTO [srv].[db].[dbo].[u] (a) VALUES (1)",
        "UPDATE t SET a = 1 OUTPUT DELETED.a INTO @o (a)",
        "DELETE FROM t OUTPUT DELETED.a INTO @o",
    ] {
        assert_eq!(rt(text), text, "{text} does not print back as written");
    }
    for (text, number, message) in [
        (
            "INSERT INTO t (a) OUTPUT INSERTED.a INTO @o AS x (a) VALUES (1)",
            156,
            "Syntax error near the keyword 'AS'.",
        ),
        (
            "INSERT INTO t (a) OUTPUT INSERTED.a INTO u AS x (a) VALUES (1)",
            156,
            "Syntax error near the keyword 'AS'.",
        ),
        (
            "INSERT INTO t (a) OUTPUT INSERTED.a INTO @o WITH (TABLOCK) (a) VALUES (1)",
            156,
            "Syntax error near the keyword 'WITH'.",
        ),
        (
            "INSERT INTO t (a) OUTPUT INSERTED.a INTO u WITH (TABLOCK) (a) VALUES (1)",
            156,
            "Syntax error near the keyword 'WITH'.",
        ),
        (
            "INSERT INTO t (a) OUTPUT INSERTED.a INTO @o.x (a) VALUES (1)",
            102,
            "Syntax error near '.'.",
        ),
        (
            "INSERT INTO t (a) OUTPUT INSERTED.a INTO f() (a) VALUES (1)",
            102,
            "Syntax error near ')'.",
        ),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}");
        assert_eq!(error.message, message, "{text}");
    }
}

#[test]
fn dml_roundtrip_corpus() {
    // One statement per accepted form above, each legal T-SQL. Two forms
    // are deliberately absent, because `Display` does not write them back as written:
    // `INSERT t …` (the `INTO` is added) and `DELETE t` (the `FROM` is added), both
    // asserted as deviations in their own test.
    for text in [
        "INSERT INTO t (a, b) VALUES (1, 2)",
        "INSERT INTO t VALUES (1, 2), (3, 4)",
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t (a) VALUES (DEFAULT)",
        "INSERT INTO t (a) SELECT b FROM u WHERE b > 1",
        "INSERT INTO t SELECT 1 UNION SELECT 2",
        "INSERT TOP (5) INTO t (a) SELECT b FROM u",
        "INSERT INTO t (a) OUTPUT INSERTED.id VALUES (1)",
        "INSERT INTO t (a) OUTPUT INSERTED.id INTO #x (id) VALUES (1)",
        "INSERT INTO t (a) OUTPUT INSERTED.id INTO @out (id) VALUES (1)",
        "INSERT INTO @t (c) VALUES (1)",
        "INSERT INTO t WITH (TABLOCK) (c) VALUES (1)",
        "INSERT INTO f(1) (c) VALUES (1)",
        "UPDATE t SET a = 1",
        "UPDATE t SET a = 1, b = 2 WHERE c = 3",
        "UPDATE t SET a += 1, b -= 2, c *= 3, d /= 4, e %= 5, f &= 6, g |= 7, h ^= 8",
        "UPDATE a SET a.x = b.y FROM t AS a INNER JOIN u AS b ON a.id = b.id WHERE b.z = 1",
        "UPDATE t SET @x = a",
        "UPDATE TOP (1) t SET a = 1",
        "UPDATE t SET a = 1 OUTPUT DELETED.a, INSERTED.a WHERE b = 2",
        "UPDATE t WITH (TABLOCK) SET a = 1",
        "DELETE FROM t",
        "DELETE FROM t WHERE a = 1",
        "DELETE TOP (10) FROM t",
        "DELETE FROM a FROM t AS a INNER JOIN u AS b ON a.id = b.id WHERE b.z = 1",
        "DELETE FROM t OUTPUT DELETED.id WHERE a = 1",
        "TRUNCATE TABLE dbo.t",
    ] {
        assert_eq!(rt(text), text, "{text} does not print back as written");
    }
}

#[test]
fn dml_in_a_batch_of_several_statements() {
    // The `;` is a separator, not a terminator: a DML statement stops on the word that
    // opens the next one, exactly as a `SELECT` does.
    let batch = p("INSERT INTO t VALUES (1) DELETE FROM t; TRUNCATE TABLE t");
    assert_eq!(batch.statements.len(), 3);
    assert!(matches!(batch.statements[0], Statement::Insert(_)));
    assert!(matches!(batch.statements[1], Statement::Delete(_)));
    assert!(matches!(batch.statements[2], Statement::Truncate { .. }));
    assert_eq!(
        rt("INSERT INTO t VALUES (1) DELETE FROM t; TRUNCATE TABLE t"),
        "INSERT INTO t VALUES (1);\nDELETE FROM t;\nTRUNCATE TABLE t"
    );
}

#[test]
fn update_assigns_a_variable_and_a_column() {
    let text = "UPDATE t SET @x = c = c + 1";
    let statement = update(text);
    assert_eq!(rt(text), text);
    assert_eq!(statement.assignments.len(), 1);
    match &statement.assignments[0].target {
        AssignTarget::VariableAndColumn { variable, column } => {
            assert_eq!(variable, "@x");
            assert_eq!(column.to_string(), "c");
        }
        other => panic!("expected two targets, got {other:?}"),
    }
    assert_eq!(statement.assignments[0].op, AssignOp::Set);
    assert_eq!(statement.assignments[0].value.to_string(), "c + 1");
}

#[test]
fn update_double_target_compound_operators_and_qualified_columns() {
    for (operator, expected) in [
        ("+=", AssignOp::AddAssign),
        ("-=", AssignOp::SubAssign),
        ("*=", AssignOp::MulAssign),
        ("/=", AssignOp::DivAssign),
        ("%=", AssignOp::ModAssign),
        ("&=", AssignOp::BitAndAssign),
        ("|=", AssignOp::BitOrAssign),
        ("^=", AssignOp::BitXorAssign),
    ] {
        let text = format!("UPDATE p SET @x = p.c {operator} 2 FROM t AS p");
        let statement = update(&text);
        assert_eq!(rt(&text), text);
        assert_eq!(statement.assignments[0].op, expected);
        match &statement.assignments[0].target {
            AssignTarget::VariableAndColumn { variable, column } => {
                assert_eq!(variable, "@x");
                assert_eq!(column.to_string(), "p.c");
            }
            other => panic!("expected two targets, got {other:?}"),
        }
    }
    let text = "UPDATE t SET @x = c = c + 1, @y = d = d + 3";
    assert_eq!(update(text).assignments.len(), 2);
    assert_eq!(rt(text), text);
}

#[test]
fn update_double_target_refuses_the_neighbouring_shapes() {
    // As SQL Server answers them, sent as ordinary batches.
    for text in [
        "UPDATE t SET c = @x = 2",
        "UPDATE t SET @x = (c) = 2",
        "UPDATE t SET @x = c + 1 = 2",
        "UPDATE t SET @x = @y = 2",
        "UPDATE t SET @x = c = d = 2",
    ] {
        let error = p_err(text);
        assert_eq!(
            (error.number, error.severity, error.state),
            (102, 15, 1),
            "{text}"
        );
        assert_eq!(error.message, "Syntax error near '='.", "{text}");
    }
    let error = p_err("UPDATE t SET @x += c = 2");
    assert_eq!(error.number, 102);
    assert_eq!(error.message, "Syntax error near '+='.");
}
