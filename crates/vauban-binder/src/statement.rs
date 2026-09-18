//! The entry point of the binder: one statement in, one bound statement out.
//!
//! Each statement is dispatched to the file that binds it — `query.rs`, `ddl.rs`,
//! `ddl_index.rs`, `insert.rs`, `update_delete.rs`, `variables.rs`, `control.rs`,
//! `txn_stmt.rs`, `alter.rs`. A variant of `parser::Statement` that no module binds answers
//! an **internal** error 50000 naming the statement: a client does not see a made-up
//! message, and a reader of the log knows at once what is missing.
//!
//! The `match` of [`unsupported`] has no `_ =>` arm, so a variant added to
//! `parser::Statement` breaks this file rather than being silently reported as something
//! else.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{QueryBody, Statement};

use crate::alter::bind_alter_table;
use crate::bound::BoundStatement;
use crate::context::BindContext;
use crate::control::{
    bind_block, bind_break, bind_continue, bind_if, bind_print, bind_return, bind_while,
};
use crate::ddl::{
    bind_alter_database, bind_create_database, bind_create_table, bind_drop_database,
    bind_drop_table, bind_truncate, bind_use,
};
use crate::ddl_index::{bind_create_index, bind_drop_index};
use crate::depth::at_statement;
use crate::errors::on_the_statement;
use crate::execute::bind_execute;
use crate::insert::{bind_insert, bind_select_into};
use crate::query::bind_select;
use crate::txn_stmt::{bind_begin, bind_commit, bind_rollback, bind_save};
use crate::update_delete::{bind_delete, bind_update};
use crate::variables::{
    bind_declare, bind_select_assignment, bind_set_variable, is_assignment_select,
};

/// Binds one statement of a batch against `ctx`.
///
/// # Errors
///
/// A binding error is a `SqlError` with the number SQL Server uses (137, 195, 207, 263,
/// 1060, 4145, 8117…), so that a client cannot tell the two apart. An expression whose tree
/// is deeper than the binder walks is 8631, carrying the line of the statement. A statement
/// the binder does not bind yet is the internal error 50000 built by `unsupported`, which
/// names it. The DDL of `ddl.rs` adds 2714, 2715 and 448 to that list.
///
/// # Where the line of a binding error comes from
///
/// From the node the check hangs on, and **there is no rule** saying which node that is
/// (`crate::errors`, module header). The raising sites therefore each answer their own
/// line, and this function puts the **statement**'s line on the numbers that want it
/// (`crate::errors::NAMES_THE_STATEMENT`) and on those alone — the mirror of what `session`
/// does for run-time errors, which want it without exception. 257 belongs to that list
/// although the very call that raises it raises 402 and 8117, which do not: the number
/// decides, not the site.
pub fn bind(stmt: &Statement, ctx: &BindContext<'_>) -> SqlResult<BoundStatement> {
    match stmt {
        // `SELECT @x = e` assigns a variable and is not a query: `variables.rs` binds it.
        Statement::Select(select) if is_assignment_select(select) => {
            bind_select_assignment(select, ctx)
                .map_err(|err| at_statement(err, select.span.line))
                .map_err(|err| on_the_statement(err, select.span.line))
        }
        Statement::Select(select) if select_into_target(select).is_some() => {
            bind_select_into(select, ctx)
                .map_err(|err| at_statement(err, select.span.line))
                .map_err(|err| on_the_statement(err, select.span.line))
        }
        Statement::Select(select) => bind_select(select, ctx)
            .map(|plan| BoundStatement::Query(Box::new(plan)))
            // 8631 is raised deep in the descent, which does not know where the statement
            // starts; SQL Server reports the line of the statement.
            .map_err(|err| at_statement(err, select.span.line))
            .map_err(|err| on_the_statement(err, select.span.line)),
        Statement::CreateDatabase(create) => bind_create_database(create),
        Statement::DropDatabase {
            names, if_exists, ..
        } => bind_drop_database(names, *if_exists),
        Statement::Use { database, .. } => bind_use(database),
        Statement::CreateTable(create) => bind_create_table(create, ctx),
        Statement::DropTable {
            names, if_exists, ..
        } => bind_drop_table(names, *if_exists, ctx),
        Statement::CreateIndex(create) => bind_create_index(create, ctx),
        Statement::DropIndex(drop) => bind_drop_index(drop, ctx),
        Statement::AlterDatabase(alter) => bind_alter_database(alter, ctx),
        Statement::AlterTable(alter) => bind_alter_table(alter, ctx),
        Statement::Truncate { table, span } => bind_truncate(table, *span, ctx),
        Statement::Insert(insert) => bind_insert(insert, ctx),
        Statement::Update(update) => bind_update(update, ctx),
        Statement::Delete(delete) => bind_delete(delete, ctx),
        Statement::Declare(declare) => bind_declare(declare, ctx),
        Statement::Set(set) => bind_set_variable(set, ctx),
        Statement::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => bind_if(condition, then_branch, else_branch.as_deref(), ctx),
        Statement::While {
            condition, body, ..
        } => bind_while(condition, body, ctx),
        Statement::Block { statements, .. } => bind_block(statements, ctx),
        Statement::Break(_) => bind_break(ctx),
        Statement::Continue(_) => bind_continue(ctx),
        Statement::Return { value, span } => bind_return(value.as_ref(), span.line, ctx),
        Statement::Print { expr, .. } => bind_print(expr, ctx),
        Statement::BeginTransaction { name, mark, .. } => {
            bind_begin(name.as_ref(), mark.as_deref(), ctx)
        }
        Statement::Commit { name, .. } => bind_commit(name.as_ref(), ctx),
        Statement::Rollback { name, .. } => bind_rollback(name.as_ref(), ctx),
        Statement::Save { name, .. } => bind_save(name, ctx),
        Statement::Execute(execute) => bind_execute(execute, ctx),
        other => Err(unsupported(other)),
    }
}

/// The `INTO` target of a top-level `SELECT`, when the statement writes one.
fn select_into_target(
    select: &vauban_parser::SelectStatement,
) -> Option<&vauban_parser::ObjectName> {
    match &select.body {
        QueryBody::Select(spec) => spec.into.as_ref(),
        QueryBody::SetOp { .. } | QueryBody::Nested(..) => None,
    }
}

/// The internal error a statement the binder does not bind answers: its keyword, and that
/// it is not implemented.
///
/// The statements `bind` dispatches — `SELECT`, the DDL of `ddl.rs`, the two of
/// `ddl_index.rs`, the DML, the control-of-flow and transaction statements routed to their
/// own file, `TRUNCATE TABLE` included — do not reach this function, and are listed to keep
/// the `match` exhaustive. `SET <option>` belongs to the session state, not to the binder.
fn unsupported(stmt: &Statement) -> SqlError {
    let name = match stmt {
        // Bound by `bind`; listed so that the match stays exhaustive.
        Statement::Select(_) => "SELECT",
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Merge(_) => "MERGE",
        Statement::Truncate { .. } => "TRUNCATE TABLE",
        Statement::CreateDatabase(_) => "CREATE DATABASE",
        Statement::DropDatabase { .. } => "DROP DATABASE",
        Statement::Use { .. } => "USE",
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::DropTable { .. } => "DROP TABLE",
        Statement::AlterDatabase(_) => "ALTER DATABASE",
        Statement::AlterTable(_) => "ALTER TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::DropIndex(_) => "DROP INDEX",
        Statement::CreateProcedure(_) => "CREATE PROCEDURE",
        Statement::CreateFunction(_) => "CREATE FUNCTION",
        Statement::CreateView(_) => "CREATE VIEW",
        Statement::CreateTrigger(_) => "CREATE TRIGGER",
        Statement::CreateSequence(_) => "CREATE SEQUENCE",
        Statement::DropProcedure { .. } => "DROP PROCEDURE",
        Statement::DropFunction { .. } => "DROP FUNCTION",
        Statement::DropView { .. } => "DROP VIEW",
        Statement::DropTrigger { .. } => "DROP TRIGGER",
        Statement::DropSequence { .. } => "DROP SEQUENCE",
        Statement::Declare(_) => "DECLARE",
        Statement::Set(_) => "SET @variable",
        Statement::SetOption(_) => "SET <option>",
        Statement::If { .. } => "IF",
        Statement::While { .. } => "WHILE",
        Statement::Block { .. } => "BEGIN … END",
        Statement::Break(_) => "BREAK",
        Statement::Continue(_) => "CONTINUE",
        Statement::Return { .. } => "RETURN",
        Statement::Print { .. } => "PRINT",
        Statement::Execute(_) => "EXECUTE",
        Statement::BeginTransaction { .. } => "BEGIN TRANSACTION",
        Statement::Commit { .. } => "COMMIT",
        Statement::Rollback { .. } => "ROLLBACK",
        Statement::Save { .. } => "SAVE TRANSACTION",
        Statement::Waitfor(_) => "WAITFOR",
        Statement::TryCatch { .. } => "BEGIN TRY … BEGIN CATCH",
        Statement::Throw { .. } => "THROW",
        Statement::RaiseError(_) => "RAISERROR",
        Statement::Goto { .. } => "GOTO",
        Statement::Label { .. } => "a GOTO label",
        Statement::Cursor(_) => "a cursor statement",
        Statement::Grant(_) => "GRANT, DENY and REVOKE",
    };
    SqlError::from(InternalError::Bug(format!("{name} is not implemented yet")))
}

#[cfg(test)]
mod tests {
    use super::{bind, unsupported};
    use vauban_parser::{Expr, Ident, Literal, ParseOptions, Span, Statement, parse_batch};
    use vauban_sysfn::register_builtins;

    use crate::context::{BindContext, SessionOptions};

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

    /// `ALTER TABLE` parses and is not bound, and the message names the statement;
    /// `ALTER DATABASE … SET` is bound by `ddl.rs`, which refuses an option it does not
    /// carry by naming the option, not the statement. `CREATE INDEX` and `DROP INDEX` do not
    /// come through here at all — `ddl_index.rs` binds them, and the two assertions at the
    /// end of this test are the counter-proof of the two above.
    #[test]
    fn the_alter_statements_name_themselves() {
        let message = |text: &str| {
            let batch = parse_batch(text, &ParseOptions::default())
                .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
            let ctx = BindContext::scalar(text, SessionOptions::default());
            bind(&batch.statements[0], &ctx)
                .expect_err("this statement is not bound")
                .message
        };
        assert!(
            message("ALTER TABLE t ADD c int;").ends_with("ALTER TABLE is not implemented yet"),
            "{}",
            message("ALTER TABLE t ADD c int;")
        );
        assert!(
            message("ALTER DATABASE d SET READ_ONLY;").contains("SET READ_ONLY"),
            "{}",
            message("ALTER DATABASE d SET READ_ONLY;")
        );
        assert!(
            !message("CREATE INDEX ix ON t (a);").contains("is not implemented yet"),
            "CREATE INDEX is bound; without a catalogue it answers for want of one, \
             which is `ddl_index::tests::create_index_without_a_catalogue_is_refused`"
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
}
