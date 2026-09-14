//! `CREATE`/`ALTER`/`DROP DATABASE`, `USE`, `CREATE`/`DROP INDEX`, and the refusal of the
//! programmability DDL.
//!
//! Everything here goes through the public entry point of the crate, `parse_batch`: what
//! a client sends is a batch, and the shape of the AST it yields is what `binder` and
//! `executor` are written against.
//!
//! # The `parse` -> `Display` -> `parse` loop
//!
//! [`rt`] is the loop the module README makes a contract: for any accepted text, parsing
//! what `Display` wrote yields an **equal** `Batch`. The text `Display` writes is not the
//! source text, and the two deviations of this module -- the whitespace of a swallowed
//! database option and the deprecated `DROP INDEX t.ix` -- are asserted here as
//! deviations rather than hidden. The `WITH (…)`/`ON …` of a `CREATE INDEX` reach the
//! AST ([`create_index_storage_clauses`]).

use vauban_errors::SqlError;
use vauban_parser::{
    Batch, Clustering, CreateDatabaseStatement, CreateIndexStatement, DatabaseOption,
    DropIndexStatement, Ident, IndexOption, IndexOptionValue, IndexStorage, ParseOptions,
    Statement, StoragePlacement, parse_batch,
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
        None => unreachable!("{text} yielded no statement"),
    }
}

/// The one `CREATE DATABASE` of `text`.
fn created_database(text: &str) -> CreateDatabaseStatement {
    match one(text) {
        Statement::CreateDatabase(create) => *create,
        other => unreachable!("{text} is a CREATE DATABASE, got {other:?}"),
    }
}

/// The one `CREATE INDEX` of `text`.
fn created_index(text: &str) -> CreateIndexStatement {
    match one(text) {
        Statement::CreateIndex(create) => *create,
        other => unreachable!("{text} is a CREATE INDEX, got {other:?}"),
    }
}

/// The one `DROP INDEX` of `text`.
fn dropped_index(text: &str) -> DropIndexStatement {
    match one(text) {
        Statement::DropIndex(drop) => *drop,
        other => unreachable!("{text} is a DROP INDEX, got {other:?}"),
    }
}

/// The options of `text` as `(name, value)` pairs, which is what the swallower keeps.
fn options(list: &[DatabaseOption]) -> Vec<(String, Option<String>)> {
    list.iter()
        .map(|option| (option.name.clone(), option.value.clone()))
        .collect()
}

/// An unquoted identifier, for comparison against a parsed one.
fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

#[test]
fn use_statement() {
    match one("USE master") {
        Statement::Use { database, .. } => assert_eq!(database, ident("master")),
        other => unreachable!("USE master is a USE, got {other:?}"),
    }
    assert_eq!(rt("USE master"), "USE master");
    // A delimited name keeps its brackets, because they are the only spelling that parses
    // back to the same name.
    assert_eq!(rt("USE [my db]"), "USE [my db]");
    match one("USE [my db]") {
        Statement::Use { database, .. } => {
            assert_eq!(database.value, "my db");
            assert!(database.quoted);
        }
        other => unreachable!("it is a USE, got {other:?}"),
    }
}

#[test]
fn create_database_simple() {
    let create = created_database("CREATE DATABASE d");
    assert_eq!(create.name, ident("d"));
    assert!(create.options.is_empty(), "no option was written");
    assert_eq!(rt("CREATE DATABASE d"), "CREATE DATABASE d");
    // The `;` is the batch separator, not part of the statement.
    assert_eq!(rt("CREATE DATABASE d;"), "CREATE DATABASE d");
}

#[test]
fn create_database_with_options() {
    let text = "CREATE DATABASE d COLLATE SQL_Latin1_General_CP1_CI_AS";
    let create = created_database(text);
    assert_eq!(
        options(&create.options),
        [(
            "COLLATE".to_owned(),
            Some("SQL_Latin1_General_CP1_CI_AS".to_owned())
        )]
    );
    assert_eq!(rt(text), text);

    // Two first-level clauses: `ON (…)` and `LOG ON (…)`. The `ON` of `LOG ON` does not
    // open a clause of its own.
    let text = "CREATE DATABASE d ON (NAME = f, FILENAME = 'c:\\d.mdf') \
                LOG ON (NAME = fl, FILENAME = 'c:\\d.ldf')";
    let create = created_database(text);
    assert_eq!(
        options(&create.options),
        [
            (
                "ON".to_owned(),
                Some("( NAME = f , FILENAME = 'c:\\d.mdf' )".to_owned())
            ),
            (
                "LOG".to_owned(),
                Some("ON ( NAME = fl , FILENAME = 'c:\\d.ldf' )".to_owned())
            ),
        ]
    );
    // The loop is on the **AST**, not on the text: rebuilding a clause from the source
    // text of its tokens joined by a space normalises the whitespace. What `Display`
    // writes is a fixed point.
    let printed = rt(text);
    assert_ne!(printed, text, "the whitespace of the options is normalised");
    assert_eq!(p(&printed).to_string(), printed, "Display is a fixed point");
    assert_eq!(
        printed,
        "CREATE DATABASE d ON ( NAME = f , FILENAME = 'c:\\d.mdf' ) \
         LOG ON ( NAME = fl , FILENAME = 'c:\\d.ldf' )"
    );

    // `ON PRIMARY (…)`, `CONTAINMENT`, `WITH` and `FOR ATTACH` are swallowed the same way.
    let text = "CREATE DATABASE d CONTAINMENT = PARTIAL ON PRIMARY (NAME = f) FOR ATTACH";
    assert_eq!(
        options(&created_database(text).options),
        [
            ("CONTAINMENT".to_owned(), Some("= PARTIAL".to_owned())),
            ("ON".to_owned(), Some("PRIMARY ( NAME = f )".to_owned())),
            ("FOR".to_owned(), Some("ATTACH".to_owned())),
        ]
    );
    let printed = rt(text);
    assert_eq!(p(&printed).to_string(), printed, "Display is a fixed point");

    // The swallower stops on a word that can only open another statement: this is two
    // statements, not one option named `SELECT`.
    let statements = p("CREATE DATABASE d COLLATE c SELECT 1").statements;
    assert_eq!(statements.len(), 2);
    assert!(matches!(statements[0], Statement::CreateDatabase(_)));
    assert!(matches!(statements[1], Statement::Select(_)));
    // `GO` is a word of the client and never reaches the server: it is refused, not
    // swallowed.
    assert_eq!(p_err("CREATE DATABASE d\nGO").line, 2);
}

#[test]
fn alter_and_drop_database() {
    let text = "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON";
    match one(text) {
        Statement::AlterDatabase(alter) => {
            assert_eq!(alter.name, ident("d"));
            assert_eq!(
                options(&alter.options),
                [("READ_COMMITTED_SNAPSHOT".to_owned(), Some("ON".to_owned()))]
            );
        }
        other => unreachable!("{text} is an ALTER DATABASE, got {other:?}"),
    }
    // The `SET` is not stored: `Display` writes it back itself.
    assert_eq!(rt(text), text);
    // The options of a `SET` are separated by commas, not by clause words.
    let text = "ALTER DATABASE d SET ALLOW_SNAPSHOT_ISOLATION ON, RECOVERY SIMPLE";
    assert_eq!(
        rt(text),
        "ALTER DATABASE d SET ALLOW_SNAPSHOT_ISOLATION ON, RECOVERY SIMPLE"
    );
    // A form without `SET` comes back with the `SET` `Display` always writes, which
    // parses back to the same AST.
    let printed = rt("ALTER DATABASE d COLLATE c");
    assert_eq!(printed, "ALTER DATABASE d SET COLLATE c");
    assert_eq!(p(&printed).to_string(), printed, "Display is a fixed point");
    // A `SET`-less form whose options hold top-level commas of their own. `Display`
    // joins the options with `, `, so cutting this one clause by clause would read the
    // comma back as a separator and yield two options on the second pass: without `SET`
    // as with it, the cut is made on the top-level commas.
    let text = "ALTER DATABASE d ADD FILE (NAME = f1, FILENAME = 'c:\\f1.ndf'), \
                (NAME = f2, FILENAME = 'c:\\f2.ndf') TO FILEGROUP fg";
    match one(text) {
        Statement::AlterDatabase(alter) => assert_eq!(
            options(&alter.options),
            [
                (
                    "ADD".to_owned(),
                    Some("FILE ( NAME = f1 , FILENAME = 'c:\\f1.ndf' )".to_owned())
                ),
                (
                    "(".to_owned(),
                    Some("NAME = f2 , FILENAME = 'c:\\f2.ndf' ) TO FILEGROUP fg".to_owned())
                ),
            ]
        ),
        other => unreachable!("{text} is an ALTER DATABASE, got {other:?}"),
    }
    let printed = rt(text);
    assert_eq!(
        printed,
        "ALTER DATABASE d SET ADD FILE ( NAME = f1 , FILENAME = 'c:\\f1.ndf' ), \
         ( NAME = f2 , FILENAME = 'c:\\f2.ndf' ) TO FILEGROUP fg"
    );
    assert_eq!(p(&printed).to_string(), printed, "Display is a fixed point");
    let text = "ALTER DATABASE d ADD LOG FILE (NAME = l1, FILENAME = 'c:\\l1.ldf'), \
                (NAME = l2, FILENAME = 'c:\\l2.ldf')";
    let printed = rt(text);
    assert_eq!(
        printed,
        "ALTER DATABASE d SET ADD LOG FILE ( NAME = l1 , FILENAME = 'c:\\l1.ldf' ), \
         ( NAME = l2 , FILENAME = 'c:\\l2.ldf' )"
    );
    assert_eq!(p(&printed).to_string(), printed, "Display is a fixed point");
    // The same cut makes a lone comma say nothing about the database, which the check on
    // empty options refuses; `Display` would otherwise write a text that no longer parses.
    // SQL Server: `ALTER DATABASE d ,` => 102.
    assert_eq!(p_err("ALTER DATABASE d ,").number, 102);

    match one("DROP DATABASE d") {
        Statement::DropDatabase {
            names, if_exists, ..
        } => {
            assert_eq!(names, [ident("d")]);
            assert!(!if_exists);
        }
        other => unreachable!("it is a DROP DATABASE, got {other:?}"),
    }
    assert_eq!(rt("DROP DATABASE d"), "DROP DATABASE d");

    match one("DROP DATABASE IF EXISTS d") {
        Statement::DropDatabase {
            names, if_exists, ..
        } => {
            assert_eq!(names, [ident("d")]);
            assert!(if_exists, "IF EXISTS was written");
        }
        other => unreachable!("it is a DROP DATABASE, got {other:?}"),
    }
    assert_eq!(rt("DROP DATABASE IF EXISTS d"), "DROP DATABASE IF EXISTS d");

    match one("DROP DATABASE a, b") {
        Statement::DropDatabase { names, .. } => {
            assert_eq!(names, [ident("a"), ident("b")]);
        }
        other => unreachable!("it is a DROP DATABASE, got {other:?}"),
    }
    assert_eq!(rt("DROP DATABASE a, b"), "DROP DATABASE a, b");
}

#[test]
fn create_index() {
    let text = "CREATE INDEX IX_t ON t (a)";
    let create = created_index(text);
    assert_eq!(create.name, ident("IX_t"));
    assert_eq!(create.table.name, ident("t"));
    assert!(create.table.schema.is_none());
    assert!(!create.unique);
    assert!(create.clustering.is_none());
    assert!(create.include.is_empty());
    assert!(create.where_.is_none());
    match &create.columns[..] {
        [column] => {
            assert_eq!(column.name, ident("a"));
            assert!(!column.desc);
            assert!(
                !column.explicit_direction,
                "no ASC or DESC was written next to the column"
            );
        }
        other => unreachable!("one key column, got {other:?}"),
    }
    assert_eq!(rt(text), text);

    let text = "CREATE UNIQUE CLUSTERED INDEX IX_t ON dbo.t (a ASC, b DESC)";
    let create = created_index(text);
    assert!(create.unique);
    assert_eq!(create.clustering, Some(Clustering::Clustered));
    assert_eq!(create.table.schema, Some(ident("dbo")));
    match &create.columns[..] {
        [first, second] => {
            assert_eq!(first.name, ident("a"));
            assert!(!first.desc);
            assert!(first.explicit_direction, "ASC was written");
            assert_eq!(second.name, ident("b"));
            assert!(second.desc);
            assert!(second.explicit_direction);
        }
        other => unreachable!("two key columns, got {other:?}"),
    }
    assert_eq!(rt(text), text);

    let text = "CREATE NONCLUSTERED INDEX IX_t ON t (a) INCLUDE (b, c)";
    let create = created_index(text);
    assert_eq!(create.clustering, Some(Clustering::NonClustered));
    assert_eq!(create.include, [ident("b"), ident("c")]);
    assert_eq!(rt(text), text);

    let text = "CREATE INDEX IX_t ON t (a) WHERE a > 0";
    let create = created_index(text);
    assert!(create.where_.is_some(), "a filtered index keeps its WHERE");
    assert_eq!(rt(text), text);

    // A `CREATE INDEX` without storage clauses carries none.
    let text = "CREATE INDEX IX_t ON t (a)";
    assert_eq!(created_index(text).storage, IndexStorage::default());
}

/// `WITH (…)` and `ON …` reach the AST and come back from `Display`.
#[test]
fn create_index_storage_clauses() {
    let text = "CREATE INDEX IX_t ON t (a) WITH (FILLFACTOR = 80) ON [PRIMARY]";
    let create = created_index(text);
    assert_eq!(create.name, ident("IX_t"));
    assert_eq!(create.columns.len(), 1);
    assert_eq!(
        create.storage,
        IndexStorage {
            options: vec![IndexOption {
                name: "FILLFACTOR".to_owned(),
                value: IndexOptionValue::Integer("80".to_owned()),
            }],
            placement: Some(StoragePlacement {
                name: Ident {
                    value: "PRIMARY".to_owned(),
                    quoted: true,
                },
                partition_column: None,
            }),
        }
    );
    let printed = rt(text);
    assert_eq!(printed, text);
    assert!(printed.contains("WITH (FILLFACTOR = 80)"), "{printed}");
    assert!(printed.contains("ON [PRIMARY]"), "{printed}");

    // The two clauses together, on top of everything else, with a partition scheme.
    let text = "CREATE UNIQUE NONCLUSTERED INDEX IX_t ON t (a DESC) INCLUDE (b) \
                WHERE a > 0 WITH (ONLINE = ON, FILLFACTOR = 80) ON ps (a)";
    let create = created_index(text);
    assert_eq!(
        create.storage.placement,
        Some(StoragePlacement {
            name: ident("ps"),
            partition_column: Some(ident("a")),
        })
    );
    assert_eq!(rt(text), text);

    // Each clause alone.
    assert_eq!(
        rt("CREATE INDEX IX_t ON t (a) ON [PRIMARY]"),
        "CREATE INDEX IX_t ON t (a) ON [PRIMARY]"
    );
    assert_eq!(
        rt("CREATE INDEX IX_t ON t (a) WITH (PAD_INDEX = OFF)"),
        "CREATE INDEX IX_t ON t (a) WITH (PAD_INDEX = OFF)"
    );

    // The SSMS shape: eight options, a line break before `INCLUDE`, no space before `(`.
    let text = "CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders]\n(\n\t[CustomerId] ASC\n)\n\
                INCLUDE([OrderDate],[Total]) WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, \
                SORT_IN_TEMPDB = OFF, DROP_EXISTING = OFF, ONLINE = OFF, ALLOW_ROW_LOCKS = ON, \
                ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]";
    let create = created_index(text);
    assert_eq!(create.storage.options.len(), 8);
    assert_eq!(
        create.storage.options[4],
        IndexOption {
            name: "ONLINE".to_owned(),
            value: IndexOptionValue::Off,
        }
    );
    assert_eq!(
        rt(text),
        "CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] ([CustomerId] ASC) \
         INCLUDE ([OrderDate], [Total]) WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, \
         SORT_IN_TEMPDB = OFF, DROP_EXISTING = OFF, ONLINE = OFF, ALLOW_ROW_LOCKS = ON, \
         ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]"
    );
    // The clauses end the statement: what follows is another statement of the batch.
    assert_eq!(
        p("CREATE INDEX IX_t ON t (a) WITH (ONLINE = OFF) ON [PRIMARY] SELECT 1")
            .statements
            .len(),
        2
    );
}

/// The storage clauses SQL Server 2022 refuses on a `CREATE INDEX`, with the number
/// VaubanDB reports.
/// Two of them are 319 on SQL Server (the "common table expression" message, not in the
/// catalogue): VaubanDB reports 156 on the misplaced `WITH` instead.
#[test]
fn create_index_storage_clauses_refused() {
    for (text, number, near) in [
        ("CREATE INDEX IX_t ON t (a) ON PRIMARY", 156, "PRIMARY"),
        (
            "CREATE INDEX IX_t ON t (a) ON [PRIMARY] ON [PRIMARY]",
            156,
            "ON",
        ),
        // SQL Server: 319 on both.
        (
            "CREATE INDEX IX_t ON t (a) ON [PRIMARY] WITH (FILLFACTOR = 80)",
            156,
            "WITH",
        ),
        (
            "CREATE INDEX IX_t ON t (a) WITH (FILLFACTOR = 80) WITH (FILLFACTOR = 80)",
            156,
            "WITH",
        ),
        ("CREATE INDEX IX_t ON t (a) WITH ()", 102, ")"),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert!(
            error.message.contains(&format!("'{near}'")),
            "{text}: {}",
            error.message
        );
    }
}

/// `DROP INDEX ix ON t WITH (…)`: the SSMS shape, kept in the AST and written back.
#[test]
fn drop_index_options() {
    let text = "DROP INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] WITH ( ONLINE = OFF )";
    let drop = dropped_index(text);
    assert_eq!(
        drop.options,
        vec![IndexOption {
            name: "ONLINE".to_owned(),
            value: IndexOptionValue::Off,
        }]
    );
    let printed = rt(text);
    assert_eq!(
        printed,
        "DROP INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] WITH (ONLINE = OFF)"
    );
    assert_eq!(
        rt("DROP INDEX IF EXISTS IX_t ON t WITH (ONLINE = OFF, MAXDOP = 4)"),
        "DROP INDEX IF EXISTS IX_t ON t WITH (ONLINE = OFF, MAXDOP = 4)"
    );
    assert!(dropped_index("DROP INDEX IX_t ON t").options.is_empty());
    // The clause ends the statement.
    assert_eq!(
        p("DROP INDEX IX_t ON t WITH (ONLINE = OFF) SELECT 1")
            .statements
            .len(),
        2
    );

    // `ON` after the options, and an empty list: same numbers as SQL Server.
    for (text, number, near) in [
        (
            "DROP INDEX IX_t ON t WITH (ONLINE = OFF) ON [PRIMARY]",
            156,
            "ON",
        ),
        ("DROP INDEX IX_t ON t WITH ()", 102, ")"),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert!(
            error.message.contains(&format!("'{near}'")),
            "{text}: {}",
            error.message
        );
    }
    // Two shapes SQL Server 2022 accepts and this grammar does not:
    // several indexes in one `DROP INDEX`, and the two-word option `MOVE TO`.
    assert_eq!(
        p_err("DROP INDEX IX_t ON t WITH (ONLINE = OFF), IX_u ON t WITH (ONLINE = OFF)").number,
        102
    );
    assert_eq!(
        p_err("DROP INDEX IX_t ON t WITH (MOVE TO [PRIMARY])").number,
        156
    );
}

#[test]
fn drop_index() {
    let modern = dropped_index("DROP INDEX IX_t ON t");
    assert_eq!(modern.name, ident("IX_t"));
    assert_eq!(modern.table.name, ident("t"));
    assert!(!modern.if_exists);
    assert_eq!(rt("DROP INDEX IX_t ON t"), "DROP INDEX IX_t ON t");

    let guarded = dropped_index("DROP INDEX IF EXISTS IX_t ON t");
    assert!(guarded.if_exists);
    assert_eq!(
        rt("DROP INDEX IF EXISTS IX_t ON t"),
        "DROP INDEX IF EXISTS IX_t ON t"
    );

    // Assumed deviation: the deprecated `t.ix` spelling yields the very same AST, and
    // `Display` writes the modern one back.
    let old = dropped_index("DROP INDEX t.IX_t");
    assert_eq!(old, modern, "both spellings give one AST");
    assert_eq!(rt("DROP INDEX t.IX_t"), "DROP INDEX IX_t ON t");
    // Qualified further: `dbo.t.IX_t` is the index `IX_t` of the table `dbo.t`.
    let qualified = dropped_index("DROP INDEX dbo.t.IX_t");
    assert_eq!(qualified.name, ident("IX_t"));
    assert_eq!(qualified.table.name, ident("t"));
    assert_eq!(qualified.table.schema, Some(ident("dbo")));
    assert_eq!(rt("DROP INDEX dbo.t.IX_t"), "DROP INDEX IX_t ON dbo.t");
}

#[test]
fn unsupported_ddl_is_refused() {
    // These queries reach SQL Server semantic analysis or succeed:
    // CREATE PROCEDURE/PROC/FUNCTION and CREATE SCHEMA succeed; CREATE VIEW => 4511;
    // CREATE TRIGGER => 8197; ALTER PROCEDURE => 208; DROP PROCEDURE/VIEW => 3701.
    // VaubanDB still refuses the exact queries below with 156, a known deviation.
    for (text, object) in [
        ("CREATE PROCEDURE p AS SELECT 1", "PROCEDURE"),
        ("CREATE PROC p AS SELECT 1", "PROC"),
        (
            "CREATE FUNCTION f() RETURNS int AS BEGIN RETURN 1 END",
            "FUNCTION",
        ),
        ("CREATE VIEW v AS SELECT 1", "VIEW"),
        ("CREATE TRIGGER g ON t AFTER INSERT AS SELECT 1", "TRIGGER"),
        ("CREATE SCHEMA s", "SCHEMA"),
        ("ALTER PROCEDURE p AS SELECT 1", "PROCEDURE"),
        ("DROP PROCEDURE p", "PROCEDURE"),
        ("DROP VIEW v", "VIEW"),
    ] {
        let error = p_err(text);
        assert_eq!(
            error.number, 156,
            "{text}: {} {}",
            error.number, error.message
        );
        assert!(
            error.message.contains(object),
            "{text} must name {object}, got {}",
            error.message
        );
    }
    // `SEQUENCE` is not a reserved word, so the error is a 102 and its text is frozen.
    let error = p_err("CREATE SEQUENCE s");
    assert_eq!(error.number, 102);
    assert_eq!(error.message, "Syntax error near 'SEQUENCE'.");
    // Neither are `TYPE`, `LOGIN` and `ROLE`, which are not keywords at all.
    for (text, object) in [
        ("CREATE TYPE ty FROM int", "TYPE"),
        ("CREATE LOGIN l WITH PASSWORD = 'x'", "LOGIN"),
        ("CREATE ROLE r", "ROLE"),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, format!("Syntax error near '{object}'."));
    }
}

#[test]
fn ddl_errors() {
    // A truncated batch keeps the number **102** even when it stops on a reserved word:
    // on SQL Server, `USE` gives a 102 near 'USE' and `CREATE DATABASE` a 102 near
    // 'DATABASE'. 156 is for a reserved word refused where it stands, never for a text
    // that simply stops, so these numbers are asserted exactly. The *printed token* is
    // another matter: `syntax_error::at_cursor` names the last token read.
    assert_eq!(p_err("USE").number, 102);
    assert_eq!(p_err("CREATE DATABASE").number, 102);
    // The last token read is not a keyword: 102.
    assert_eq!(p_err("CREATE INDEX IX_t ON t").number, 102);
    assert_eq!(p_err("DROP INDEX IX_t").number, 102);
    // `DROP INDEX IX_t` says nothing about the table, and the error names the index.
    assert!(
        p_err("DROP INDEX IX_t").message.contains("IX_t"),
        "the error names the offending token"
    );
    // An empty column list stops on the `)`, which is not a keyword either.
    let error = p_err("CREATE INDEX IX_t ON t ()");
    assert_eq!(error.number, 102);
    assert!(error.message.contains(')'), "{}", error.message);
    // An index name is never qualified in the modern spelling, and the engine blames the
    // `ON`, not the name: `DROP INDEX a.b ON t` and `DROP INDEX dbo.t.ix ON t` both give
    // a 156 near the keyword 'ON' on SQL Server. The token and number are reproduced by
    // the parser.
    for text in ["DROP INDEX a.b ON t", "DROP INDEX dbo.t.ix ON t"] {
        let error = p_err(text);
        assert_eq!(error.number, 156, "{text}: {}", error.message);
        assert!(error.message.contains("'ON'"), "{text}: {}", error.message);
    }
    // `ALTER DATABASE` must say what it alters. Truncated batch again: the engine gives
    // a 102 near 'd', so the number is asserted and the printed token is left to
    // `syntax_error.rs`.
    assert_eq!(p_err("ALTER DATABASE d").number, 102);
    // A database name is an identifier, never a qualified name nor a reserved word.
    // SQL Server: `USE select` => 156.
    assert_eq!(p_err("USE select").number, 156);
    assert_eq!(p_err("DROP DATABASE a,").number, 102);
}

/// The deprecated `DROP INDEX t.ix` with a `WITH`. On SQL Server, a **well-formed**
/// option list is refused
/// with 102 near 'with' -- lower-cased, line 0 -- at the end of a batch, before
/// `SELECT 1` and before `ON t`; a malformed one is the ordinary syntax error of its
/// faulty token; `WITH CHECK` names the `WITH`.
#[test]
fn drop_index_deprecated_with() {
    for text in [
        "DROP INDEX t.IX_t WITH (ONLINE = OFF)",
        "DROP INDEX t.IX_t WITH (ONLINE = OFF, MAXDOP = 4)",
        "DROP INDEX t.IX_t WITH (ONLINE = OFF) SELECT 1",
        "DROP INDEX t.IX_t WITH (ONLINE = OFF) ON t",
        "DROP INDEX dbo.t.IX_t WITH (ONLINE = OFF)",
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, "Syntax error near 'with'.", "{text}");
        assert_eq!(error.line, 0, "{text}");
    }
    for (text, number, near) in [
        ("DROP INDEX t.IX_t WITH", 102, "WITH"),
        ("DROP INDEX t.IX_t WITH x", 102, "x"),
        ("DROP INDEX t.IX_t WITH x SELECT 1", 102, "x"),
        ("DROP INDEX t.IX_t WITH 1", 102, "1"),
        ("DROP INDEX t.IX_t WITH (", 102, "("),
        ("DROP INDEX t.IX_t WITH ()", 102, ")"),
        ("DROP INDEX t.IX_t WITH (ONLINE = OFF", 102, "OFF"),
        ("DROP INDEX t.IX_t WITH SELECT", 156, "SELECT"),
        ("DROP INDEX t.IX_t WITH NOCHECK", 156, "NOCHECK"),
        ("DROP INDEX t.IX_t WITH CHECK", 102, "WITH"),
        ("DROP INDEX t.IX_t WITH CHECK SELECT 1", 102, "WITH"),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert!(
            error.message.contains(&format!("'{near}'")),
            "{text}: {}",
            error.message
        );
        assert_eq!(error.line, 1, "{text}");
    }
}
