use super::{bind, unsupported};
use vauban_parser::{Expr, Ident, Literal, ParseOptions, Span, Statement, parse_batch};
use vauban_sysfn::register_builtins;

use vauban_catalog::ObjectId;
use vauban_parser::ObjectName;

use crate::context::{
    BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
};

/// The first binding error of a batch, statement by statement, as a session reports it.
fn err(text: &str) -> (u32, u32) {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let ctx = BindContext::scalar(text, SessionOptions::default());
    for statement in &batch.statements {
        if let Err(error) = bind(statement, &ctx) {
            return (error.number, error.line);
        }
    }
    unreachable!("{text} should not bind")
}

/// The binding errors of the table of `crate::errors`, each on the line SQL Server
/// gives it.
///
/// Each batch opens with a comment line, so that the statement starts on line 2. Each
/// batch puts the node it names and its statement on different lines: one that did not
/// would answer the same under both hypotheses and would separate nothing.
///
/// A number absent from this list is a number this test says nothing about, not a
/// number that agrees.
#[test]
fn the_listed_binding_errors_carry_the_line_of_their_own_node() {
    let head = "-- head\n";
    // (batch after the comment line, expected number, expected line)
    let vectors: &[(&str, u32, u32)] = &[
        // 402 — the operator. Four positions of the same operator, and the comment
        // between the operands that a naive scan would count as the operator's line.
        ("SELECT\nCAST(1 AS bit)\n+\nCAST(1 AS bit);", 402, 4),
        ("SELECT\nCAST(1 AS bit) +\nCAST(1 AS bit);", 402, 3),
        ("SELECT\nCAST(1 AS bit)\n+ CAST(1 AS bit);", 402, 4),
        (
            "SELECT\nCAST(1 AS bit)\n-- a comment\n+\nCAST(1 AS bit);",
            402,
            5,
        ),
        (
            "SELECT\nCAST(1 AS bit) /* c\nc */ +\nCAST(1 AS bit);",
            402,
            4,
        ),
        ("SELECT\nCAST(1 AS float)\n&\n1;", 402, 4),
        (
            "SELECT 1;\nSELECT\nCAST(1 AS bit)\n+\nCAST(1 AS bit);",
            402,
            5,
        ),
        (
            "SELECT 1 WHERE\nCAST(1 AS bit)\n+\nCAST(1 AS bit) = 1;",
            402,
            4,
        ),
        // 8117 — the operator too, binary and unary.
        ("SELECT\nNEWID()\n+\nNEWID();", 8117, 4),
        ("SELECT\nNEWID() +\nNEWID();", 8117, 3),
        ("SELECT\n(NEWID())\n+\nNEWID();", 8117, 4),
        ("SELECT\n1\n,\n-\nNEWID();", 8117, 5),
        // 206, 263, 4121, 4151, 8116 — the statement, wherever the node is.
        ("SELECT\nNEWID()\n+\n1;", 206, 2),
        ("SELECT\n1\n,\n1 +\n(NEWID()\n+\n1);", 206, 2),
        ("SELECT 1;\nSELECT\nNEWID()\n+\n1;", 206, 3),
        ("SELECT\n1\n,\n*;", 263, 2),
        ("SELECT\n1\n,\n2\n,\n*;", 263, 2),
        ("SELECT\n1\n,\ndbo.no_such_fn(1);", 4121, 2),
        ("SELECT\n1\n,\nNULLIF(NULL,\n1);", 4151, 2),
        (
            "SELECT\n1\n,\nSUBSTRING(CAST('12:00' AS time),\n1,\n2);",
            8116,
            2,
        ),
        (
            "SELECT\n1\n,\n1 +\nSUBSTRING(CAST('12:00' AS time),\n1,\n2);",
            8116,
            2,
        ),
        // 257 — the statement, though the very `binary_op_type` call that raises it
        // raises 402 and 8117 too, which name the operator. `smalldatetime` answers
        // like `datetime`, `/` like `*`, and the operand order does not matter.
        ("SELECT\nCAST('2020-01-01' AS datetime)\n*\n2;", 257, 2),
        ("SELECT\nCAST('2020-01-01' AS datetime)\n/\n2;", 257, 2),
        ("SELECT\nCAST('2020-01-01' AS smalldatetime)\n*\n2;", 257, 2),
        ("SELECT\n2\n*\nCAST('2020-01-01' AS datetime);", 257, 2),
        (
            "SELECT\n1\n,\n1 +\n(CAST('2020-01-01' AS datetime)\n*\n2);",
            257,
            2,
        ),
        (
            "SELECT 1;\nSELECT\nCAST('2020-01-01' AS datetime)\n*\n2;",
            257,
            3,
        ),
        // Nested block comments between the operands: T-SQL nests `/* */`, so the
        // first `*/` closes nothing and `still` is not a token. Every line here is one
        // more than what a scan stopping at the first `*/` answers.
        (
            "SELECT\nCAST(1 AS bit)\n/* outer /* inner\n*/ still outer */\n+\nCAST(1 AS bit);",
            402,
            6,
        ),
        (
            "SELECT\nCAST(1 AS bit)\n/* outer /* inner */ still outer */\n+\nCAST(1 AS bit);",
            402,
            5,
        ),
        (
            "SELECT\nCAST(1 AS bit)\n/* a /* b\n*/ c */\n/* d /* e\n*/ f */\n+\nCAST(1 AS bit);",
            402,
            8,
        ),
        (
            "SELECT\nNEWID()\n/* outer /* inner\n*/ still outer */\n+\nNEWID();",
            8117,
            6,
        ),
        (
            "SELECT\n1\nWHERE\n1\n/* outer /* inner\n*/ still outer */\n;",
            4145,
            8,
        ),
        (
            "SELECT\nTOP (1) WITH TIES\n1\n/* outer /* inner\n*/ still outer */\n;",
            1062,
            7,
        ),
        // The same comment on two numbers that read no text: the nesting of comments
        // moves nothing there.
        (
            "SELECT\nNEWID()\n/* outer /* inner\n*/ still outer */\n+\n1;",
            206,
            2,
        ),
        (
            "SELECT\n1\n,\n1\nCOLLATE\nLatin1_General_CI_AS\n/* outer /* inner\n*/ still outer */\n;",
            447,
            7,
        ),
        // 4145 — the token the message quotes, not the expression's line.
        ("SELECT\n1\nWHERE\n1;", 4145, 5),
        ("SELECT\n1\nWHERE\n1\n+\n1;", 4145, 7),
        ("SELECT\n1\nWHERE\n1\n;", 4145, 6),
        ("SELECT\n1\nWHERE\n1\nAND\n1 = 1;", 4145, 6),
        ("SELECT\n1\nWHERE\n(\n1\n)\n;", 4145, 8),
        ("SELECT\n1\n,\nCASE\nWHEN\n1\nTHEN 1 END;", 4145, 8),
        ("SELECT 1 WHERE\n1 = 1\nAND\n1\n;", 4145, 6),
        // 447 and 448 — the collation name, the last token of the clause.
        ("SELECT\n1\n,\n1\nCOLLATE\nLatin1_General_CI_AS;", 447, 7),
        ("SELECT\n1\n,\n1\nCOLLATE\nLatin1_General_CI_AS\n;", 447, 7),
        ("SELECT\n1\n,\n1 COLLATE Latin1_General_CI_AS;", 447, 5),
        (
            "SELECT\n1\n,\n(1\nCOLLATE\nLatin1_General_CI_AS)\n+\n1;",
            447,
            7,
        ),
        ("SELECT\n1\n,\n'a'\nCOLLATE\nNO_SUCH_COLLATION;", 448, 7),
        (
            "SELECT\n1\n,\n('a'\nCOLLATE\nNO_SUCH_COLLATION)\n+\n'b';",
            448,
            7,
        ),
        // 1062 — the last token of the statement, its `;` included.
        ("SELECT\nTOP (1) WITH TIES\n1\n,\n2;", 1062, 6),
        ("SELECT\nTOP (1) WITH TIES\n1\n,\n2\n,\n3;", 1062, 8),
        ("SELECT\nTOP (1) WITH TIES\n1\nWHERE\n1 = 1;", 1062, 6),
        ("SELECT\nTOP (1) WITH TIES\n1\n;", 1062, 5),
        (
            "SELECT\nTOP (1) WITH TIES\n1;\n-- a trailing comment",
            1062,
            4,
        ),
        ("SELECT\nTOP (1) WITH TIES\n1;\nSELECT\n2;", 1062, 4),
        ("SELECT\nTOP (1) WITH TIES\n1\n", 1062, 4),
        ("SELECT 1;\nSELECT\nTOP (1) WITH TIES\n1\n,\n2;", 1062, 7),
        // The numbers that name their own node, kept under guard: moving one of them
        // would be a regression this test catches.
        ("SELECT\n1\n,\nc;", 207, 5),
        ("SELECT\nc\n,\n1;", 207, 3),
        ("SELECT\n1\n,\n1 +\nc;", 207, 6),
        ("SELECT\n1\n,\nNO_SUCH_FN(1);", 195, 5),
        ("SELECT\nNO_SUCH_FN(\n1\n);", 195, 3),
        ("SELECT\n1\n,\n1 +\n(2 *\nNO_SUCH_FN(1));", 195, 7),
        ("SELECT\n1\n,\nLEN();", 174, 5),
        ("SELECT\nLEN(\n1\n,\n2\n);", 174, 3),
        ("SELECT\n1\n,\nROUND(1);", 189, 5),
        ("SELECT\nROUND(\n1\n);", 189, 3),
        ("SELECT\n1\n,\n@x;", 137, 5),
        ("SELECT\n1\n,\n1 +\n@x;", 137, 6),
        ("SELECT\n1\n,\n@x\n=\n1;", 137, 5),
        ("SELECT\n1\n,\nt.c;", 4104, 5),
        ("SELECT\n1\n,\n1 +\nt.c;", 4104, 6),
        ("SELECT\n1\n,\nCAST(1 AS\nfoo);", 243, 5),
        ("SELECT\n1\n,\n1 +\nCAST(1 AS\nfoo);", 243, 6),
        ("SELECT\n1\n,\nCONVERT(\nfoo\n, 1);", 243, 5),
        ("SELECT\n1\n,\nCAST(\nNEWID()\nAS int);", 529, 5),
        ("SELECT\n1\n,\n1 +\nCAST(\nNEWID()\nAS int);", 529, 6),
        ("SELECT\n1\n,\nCONVERT(int\n,\nNEWID());", 529, 5),
        (
            "SELECT\n1\n,\nDATEPART(\nno_such\n, CAST('2020-01-01' AS date));",
            155,
            5,
        ),
        ("SELECT\n1\n,\nt.*;", 107, 5),
        ("SELECT\n1\n,\na.b.c.d.*;", 117, 5),
    ];
    for (text, number, line) in vectors {
        let batch = format!("{head}{text}");
        assert_eq!(err(&batch), (*number, *line), "in {batch:?}");
    }
}

/// The message of a statement `unsupported` reports: its keyword, and that it is not
/// implemented.
#[test]
fn an_unsupported_statement_names_itself() {
    let print = Statement::Print {
        expr: Expr::Literal(Literal::Integer("1".into()), Span::EMPTY),
        span: Span::EMPTY,
    };
    let error = unsupported(&print);
    assert_eq!(error.number, 50000);
    // `InternalError` prefixes the text it wraps; the sentence is the tail of it.
    // `bind` routes `PRINT` to `control.rs` and does not come here; the arm is kept
    // so that the `match` stays exhaustive.
    assert!(
        error.message.ends_with("PRINT is not implemented yet"),
        "{}",
        error.message
    );

    let goto = Statement::Goto {
        label: Ident {
            value: "l".into(),
            quoted: false,
        },
        span: Span::EMPTY,
    };
    assert!(
        unsupported(&goto)
            .message
            .ends_with("GOTO is not implemented yet"),
        "the message names the statement"
    );
}

/// A form `alter.rs` leaves for V2 names itself; `ALTER DATABASE … SET` is bound by
/// `ddl.rs`, which refuses an option it does not carry by naming the option, not the
/// statement. `CREATE INDEX` and `DROP INDEX` do not come through here at all —
/// `ddl_index.rs` binds them, and the two assertions at the end of this test are the
/// counter-proof of the two above.
#[test]
fn the_alter_statements_name_themselves() {
    struct OneTable;
    impl CatalogView for OneTable {
        fn resolve_table(
            &self,
            name: &ObjectName,
            _database: &str,
            _default_schema: &str,
        ) -> Option<ResolvedTable> {
            name.name
                .value
                .eq_ignore_ascii_case("t")
                .then(|| ResolvedTable {
                    object: ObjectId(1),
                    table: None,
                    columns: Vec::new(),
                    kind: ResolvedTableKind::Table,
                })
        }

        fn view_definition(&self, _object: vauban_catalog::ObjectId) -> Option<&str> {
            None
        }

        fn identity_column(
            &self,
            _object: vauban_catalog::ObjectId,
        ) -> Option<vauban_catalog::ColumnId> {
            None
        }

        fn computed_columns(
            &self,
            _object: vauban_catalog::ObjectId,
        ) -> Vec<vauban_catalog::ColumnId> {
            Vec::new()
        }

        fn constraint_names_on_table(&self, _table: vauban_catalog::ObjectId) -> Vec<String> {
            Vec::new()
        }

        fn primary_key_columns(&self, _table: vauban_catalog::ObjectId) -> Option<Vec<String>> {
            None
        }

        fn unique_key_on(&self, _table: vauban_catalog::ObjectId, _columns: &[String]) -> bool {
            false
        }

        fn classify_table_reference(
            &self,
            name: &vauban_parser::ObjectName,
            database: &str,
            default_schema: &str,
        ) -> crate::TableReferenceKind {
            match crate::CatalogView::resolve_table(self, name, database, default_schema) {
                Some(_) => crate::TableReferenceKind::Table,
                None => crate::TableReferenceKind::Unknown,
            }
        }
    }
    let message = |text: &str| {
        let batch = parse_batch(text, &ParseOptions::default())
            .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
        let catalog = OneTable;
        let ctx = BindContext {
            text,
            catalog: Some(&catalog),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        };
        bind(&batch.statements[0], &ctx)
            .expect_err("this statement is not bound")
            .message
    };
    assert!(
        message("ALTER TABLE t ALTER COLUMN c int;").contains("ALTER COLUMN"),
        "{}",
        message("ALTER TABLE t ALTER COLUMN c int;")
    );
    assert!(
        message("ALTER DATABASE d SET READ_ONLY;").contains("SET READ_ONLY"),
        "{}",
        message("ALTER DATABASE d SET READ_ONLY;")
    );
    let create_index = "CREATE INDEX ix ON t (a);";
    let create_batch = parse_batch(create_index, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{create_index} parses, got {e:?}"));
    let create_ctx = BindContext::scalar(create_index, SessionOptions::default());
    let create_error = bind(&create_batch.statements[0], &create_ctx)
        .expect_err("CREATE INDEX without a catalogue is refused, not unsupported");
    assert!(
        !create_error.message.contains("is not implemented yet"),
        "CREATE INDEX is bound; without a catalogue it answers for want of one, \
         which is `ddl_index::tests::create_index_without_a_catalogue_is_refused`: {}",
        create_error.message
    );
    let batch = parse_batch("DROP INDEX ix ON t;", &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{e:?}"));
    let text = "DROP INDEX ix ON t;";
    let ctx = BindContext::scalar(text, SessionOptions::default());
    assert!(
        bind(&batch.statements[0], &ctx).is_ok(),
        "DROP INDEX is bound, catalogue or not"
    );
}

/// The one statement of `text`, kept unbound as the binder keeps a statement of a branch.
fn kept(text: &str) -> crate::bound::DeferredStatement {
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let [statement] = <[Statement; 1]>::try_from(batch.statements)
        .unwrap_or_else(|s| unreachable!("{text} holds one statement, got {s:?}"));
    crate::bound::DeferredStatement {
        text: std::sync::Arc::from(text),
        statement: Box::new(statement),
        database: "master".to_owned(),
        default_schema: "dbo".to_owned(),
        line: 1,
    }
}

#[test]
fn a_deferred_break_binds_without_its_loop() {
    use crate::bound::BoundStatement;
    register_builtins();
    // (statement, number `bind` answers on it with no `WHILE` around)
    let vectors: &[(&str, u32)] = &[
        ("IF 1 = 1 BREAK;", 135),
        ("IF 1 = 1 CONTINUE;", 136),
        ("IF 1 = 0 PRINT 'x'; ELSE BREAK;", 135),
        ("BEGIN PRINT 'x'; IF 1 = 1 IF 2 = 2 CONTINUE; END", 136),
    ];
    for &(text, number) in vectors {
        let ctx = BindContext::scalar(text, SessionOptions::default());
        let deferred = kept(text);
        // Counter-proof: the same statement bound as a statement of the batch is refused.
        let refused = bind(&deferred.statement, &ctx).err().map(|e| e.number);
        assert_eq!(refused, Some(number), "{text}");
        let bound = deferred
            .bind(&ctx)
            .unwrap_or_else(|e| unreachable!("{text} binds as a deferred statement, got {e:?}"));
        assert!(
            matches!(bound, BoundStatement::If { .. } | BoundStatement::Block(_)),
            "{text}: {bound:?}"
        );
        // The jump is left to the program for the deferred binding alone: the statement of
        // the batch that follows is checked again.
        let after = bind(&deferred.statement, &ctx).err().map(|e| e.number);
        assert_eq!(after, Some(number), "{text}");
    }
}

#[test]
fn a_deferred_statement_that_fails_leaves_the_jumps_checked_again() {
    register_builtins();
    let text = "IF 1 = 1 SELECT nosuch;";
    let ctx = BindContext::scalar(text, SessionOptions::default());
    assert!(kept(text).bind(&ctx).is_err(), "{text}");
    let after = bind(&kept("IF 1 = 1 BREAK;").statement, &ctx)
        .err()
        .map(|e| e.number);
    assert_eq!(after, Some(135));
}
