//! Control of flow: `IF … ELSE`, `WHILE`, `BEGIN … END`, `BREAK`, `CONTINUE`, `RETURN` and
//! `PRINT`.
//!
//! The bound nodes derive no `PartialEq`: a shape is checked by pattern matching.

use vauban_binder::{
    BindContext, BoundExprKind, BoundStatement, CatalogView, ColumnBinding, NoVariables,
    ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, Statement, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo, Value};

/// Binds one statement by itself, returning the first error (parse or bind).
fn err(text: &str) -> SqlError {
    register_builtins();
    match parse_batch(text, &ParseOptions::default()) {
        Err(e) => return e,
        Ok(batch) => {
            let ctx = BindContext::scalar(text, SessionOptions::default());
            for statement in &batch.statements {
                if let Err(error) = bind(statement, &ctx) {
                    return error;
                }
            }
        }
    }
    unreachable!("{text} should not bind")
}

/// Binds one statement, returning the bound form.
fn bind_one(text: &str) -> BoundStatement {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let ctx = BindContext::scalar(text, SessionOptions::default());
    bind(&batch.statements[0], &ctx).unwrap_or_else(|e| panic!("{text} should bind, got {e:?}"))
}

// ---------------------------------------------------------------------------
// IF / ELSE
// ---------------------------------------------------------------------------

/// `IF 1 = 1 SELECT 1 ELSE SELECT 2` binds both branches: `then_` is the `SELECT 1`
/// query and `else_` is the `SELECT 2` query.
#[test]
fn if_binds_both_branches() {
    let stmt = bind_one("IF 1 = 1 SELECT 1 ELSE SELECT 2");
    let BoundStatement::If {
        condition,
        then_,
        else_,
    } = &stmt
    else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(condition.is_predicate(), "the condition is a predicate");
    assert!(
        matches!(then_.as_ref(), BoundStatement::Query(_)),
        "then_ is a Query, got {then_:?}"
    );
    let else_stmt = else_.as_ref().expect("ELSE was written");
    assert!(
        matches!(else_stmt.as_ref(), BoundStatement::Query(_)),
        "else_ is a Query, got {else_stmt:?}"
    );
}

// ---------------------------------------------------------------------------
// 4145 on a value in a condition position
// ---------------------------------------------------------------------------

/// `IF 1 SELECT 1` raises 4145 because `1` is not a predicate.
#[test]
fn a_value_in_a_condition_is_4145() {
    let error = err("IF 1 SELECT 1");
    assert_eq!(error.number, 4145);

    let error = err("WHILE 'a' SELECT 1");
    assert_eq!(error.number, 4145);
}

// ---------------------------------------------------------------------------
// WHILE
// ---------------------------------------------------------------------------

/// `WHILE 1 = 1 BEGIN SELECT 1; SELECT 2; END` binds the body as a `Block` of two
/// queries.
#[test]
fn while_body_may_be_a_block() {
    let stmt = bind_one("WHILE 1 = 1 BEGIN SELECT 1; SELECT 2; END");
    let BoundStatement::While { condition, body } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(condition.is_predicate());
    let BoundStatement::Block(statements) = body.as_ref() else {
        panic!("body is a Block, got {body:?}");
    };
    assert_eq!(statements.len(), 2);
    assert!(
        matches!(&statements[0], BoundStatement::Query(_)),
        "first body statement is a Query"
    );
    assert!(
        matches!(&statements[1], BoundStatement::Query(_)),
        "second body statement is a Query"
    );
}

/// `BREAK` inside a `WHILE` binds as `BoundStatement::Break`.
#[test]
fn break_inside_while_binds() {
    let stmt = bind_one("WHILE 1 = 1 BREAK;");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(
        matches!(body.as_ref(), BoundStatement::Break),
        "body of WHILE is Break, got {body:?}"
    );
}

/// `CONTINUE` inside a `WHILE` binds as `BoundStatement::Continue`.
#[test]
fn continue_inside_while_binds() {
    let stmt = bind_one("WHILE 1 = 1 CONTINUE;");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(
        matches!(body.as_ref(), BoundStatement::Continue),
        "body of WHILE is Continue, got {body:?}"
    );
}

/// `BREAK` outside a `WHILE` raises error 135.
#[test]
fn break_outside_while_is_135() {
    let error = err("BREAK;");
    assert_eq!(error.number, 135);
}

/// `CONTINUE` outside a `WHILE` raises error 136.
#[test]
fn continue_outside_while_is_136() {
    let error = err("CONTINUE;");
    assert_eq!(error.number, 136);
}

/// 135 carries the line of the BREAK statement, not line 1 of the batch.
#[test]
fn break_outside_while_carries_the_line_of_the_statement() {
    let error = err("SELECT 1;\nBREAK;");
    assert_eq!(error.line, 2);
}

/// 136 carries the line of the CONTINUE statement, not line 1 of the batch.
#[test]
fn continue_outside_while_carries_the_line_of_the_statement() {
    let error = err("SELECT 1;\nSELECT 2;\nCONTINUE;");
    assert_eq!(error.line, 3);
}

// ---------------------------------------------------------------------------
// BEGIN … END
// ---------------------------------------------------------------------------

/// Three levels of `BEGIN … END` bind to nested `Block` statements.
#[test]
fn nested_blocks_bind_to_nested_statements() {
    let stmt = bind_one("BEGIN BEGIN BEGIN SELECT 1; END; END; END;");
    let BoundStatement::Block(level1) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert_eq!(level1.len(), 1);
    let BoundStatement::Block(level2) = &level1[0] else {
        panic!("expected a nested Block, got {:?}", level1[0]);
    };
    assert_eq!(level2.len(), 1);
    let BoundStatement::Block(level3) = &level2[0] else {
        panic!("expected a third nested Block, got {:?}", level2[0]);
    };
    assert_eq!(level3.len(), 1);
    assert!(
        matches!(&level3[0], BoundStatement::Query(_)),
        "the innermost body is a Query"
    );
}

// ---------------------------------------------------------------------------
// PRINT
// ---------------------------------------------------------------------------

/// `PRINT 1` wraps the integer literal in a `Convert` to `nvarchar(max)`.
#[test]
fn print_converts_to_nvarchar_max() {
    let stmt = bind_one("PRINT 1");
    let BoundStatement::Print(expr) = &stmt else {
        panic!("expected Print, got {stmt:?}");
    };
    // PRINT accepts any expression; the binder inserts a Convert to nvarchar(max).
    // The same conversion applies to `PRINT NULL`, `PRINT GETDATE()`, and `PRINT @x`
    // where `@x` is an int.
    assert_eq!(expr.ty, TypeInfo::new(SqlType::NVarChar(Len::Max), true));
    match &expr.kind {
        BoundExprKind::Convert {
            expr: inner,
            style: None,
            try_: false,
        } => {
            assert!(
                matches!(inner.kind, BoundExprKind::Literal(Value::I32(1))),
                "the operand is the literal 1, got {:?}",
                inner.kind
            );
        }
        other => panic!("expected Convert, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// RETURN
// ---------------------------------------------------------------------------

/// A bare `RETURN` outside a procedure is accepted and binds to
/// `BoundStatement::Return(None)`.
#[test]
fn return_outside_procedure_is_accepted() {
    let stmt = bind_one("RETURN;");
    assert!(
        matches!(&stmt, BoundStatement::Return(None)),
        "bare RETURN is Return(None), got {stmt:?}"
    );
}

/// `RETURN 1` outside a procedure raises error 178.
#[test]
fn return_with_value_outside_procedure_is_178() {
    let error = err("RETURN 1");
    assert_eq!(error.number, 178);
}

// ---------------------------------------------------------------------------
// A statement of a branch or of a loop body over a table the catalogue lacks
// ---------------------------------------------------------------------------

/// A catalogue that holds nothing: every name it is asked about is missing, which is what
/// puts 208 where a scalar context answers "this form needs a catalogue".
struct EmptyCatalog;

impl CatalogView for EmptyCatalog {}

/// Binds one statement against [`EmptyCatalog`], returning the bound form.
fn bind_empty(text: &str) -> BoundStatement {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    bind(&batch.statements[0], &empty_ctx(text))
        .unwrap_or_else(|e| panic!("{text} should bind, got {e:?}"))
}

/// Binds one statement against [`EmptyCatalog`], returning the error it answers.
fn err_empty(text: &str) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    bind(&batch.statements[0], &empty_ctx(text))
        .err()
        .unwrap_or_else(|| panic!("{text} should not bind"))
}

/// A binding context over [`EmptyCatalog`].
fn empty_ctx(text: &str) -> BindContext<'_> {
    BindContext {
        text,
        catalog: Some(&EmptyCatalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    }
}

/// The statement a [`BoundStatement::Deferred`] kept, or a panic naming what was bound.
fn deferred(stmt: &BoundStatement) -> &Statement {
    match stmt {
        BoundStatement::Deferred(deferred) => deferred.statement.as_ref(),
        other => panic!("expected a deferred statement, got {other:?}"),
    }
}

/// `IF 1 = 0 SELECT * FROM dbo.absent` binds: the branch keeps the `SELECT` as it was
/// parsed instead of resolving `dbo.absent` now.
///
/// The context of these tests carries no catalogue, so every table name is missing: what
/// separates this test from the one below is the **position** of the statement, not the
/// name it writes.
#[test]
fn a_branch_over_a_missing_table_binds_deferred() {
    let stmt = bind_empty("IF 1 = 0 SELECT * FROM dbo.absent");
    let BoundStatement::If { then_, else_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(
        matches!(deferred(then_), Statement::Select(_)),
        "the branch kept the SELECT"
    );
    assert!(else_.is_none(), "no ELSE was written");
}

/// The same statement written at the head of a batch is still error 208: the deferral
/// belongs to the branch and to the loop body, and the caller decides what to do with a
/// statement of its own.
#[test]
fn a_head_statement_over_a_missing_table_is_still_208() {
    assert_eq!(err_empty("SELECT * FROM dbo.absent").number, 208);
    assert_eq!(err_empty("INSERT dbo.absent VALUES (1)").number, 208);
}

/// `BEGIN CREATE TABLE …; INSERT … END` binds: the `CREATE TABLE` binds as it always did
/// and the `INSERT` after it waits, which is what lets the block create a table and then
/// write to it.
#[test]
fn a_block_that_creates_then_inserts_defers_the_insert() {
    let stmt = bind_empty("BEGIN CREATE TABLE dbo.blk (a int); INSERT dbo.blk VALUES (1); END");
    let BoundStatement::Block(statements) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(
        matches!(&statements[0], BoundStatement::Ddl(_)),
        "the CREATE TABLE bound, got {:?}",
        statements[0]
    );
    assert!(
        matches!(deferred(&statements[1]), Statement::Insert(_)),
        "the INSERT waits"
    );
}

/// The body of a `WHILE` and the statement of an `ELSE` are deferred on the same rule.
#[test]
fn a_while_body_and_an_else_over_a_missing_table_bind_deferred() {
    let stmt = bind_empty("WHILE 1 = 0 INSERT dbo.absent VALUES (1)");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(matches!(deferred(body), Statement::Insert(_)));

    let stmt = bind_empty("IF 1 = 1 SELECT 1 ELSE SELECT * FROM dbo.absent");
    let BoundStatement::If { then_, else_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(
        matches!(then_.as_ref(), BoundStatement::Query(_)),
        "the taken branch bound as usual"
    );
    let else_ = else_.as_ref().expect("ELSE was written");
    assert!(matches!(deferred(else_), Statement::Select(_)));
}

/// A deferred statement carries the line its first binding attempt reported, so an error
/// raised later has one to fall back on.
#[test]
fn a_deferred_statement_carries_the_line_of_its_branch() {
    let stmt = bind_empty("IF 1 = 0\n  SELECT * FROM dbo.absent");
    let BoundStatement::If { then_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    let BoundStatement::Deferred(deferred) = then_.as_ref() else {
        panic!("expected a deferred statement, got {then_:?}");
    };
    assert_eq!(deferred.line, 2, "{deferred:?}");
}

/// A branch is deferred on the numbers that name a missing table and on no other: a
/// non-boolean condition written in the branch is still error 4145 for the whole `IF`,
/// and an undeclared variable is still error 137.
#[test]
fn a_branch_that_fails_on_another_number_is_still_a_binding_error() {
    assert_eq!(err("IF 1 = 0 SELECT 1 WHERE 1;").number, 4145);
    assert_eq!(err("IF 1 = 0 SELECT @nope;").number, 137);
}

/// The **condition** is read before a branch is chosen, so a missing table written there
/// is error 208 for the whole `IF`, deferred by the caller as a statement of its own.
#[test]
fn a_condition_over_a_missing_table_is_still_208() {
    assert_eq!(
        err_empty("IF EXISTS (SELECT * FROM dbo.absent) SELECT 1;").number,
        208
    );
    assert_eq!(
        err_empty("WHILE EXISTS (SELECT * FROM dbo.absent) SELECT 1;").number,
        208
    );
}

/// One level down, the same condition is deferred with the `IF` that holds it: the block
/// keeps the whole `IF` unbound rather than answering 208 for the batch.
#[test]
fn a_condition_of_a_nested_if_is_deferred_with_it() {
    let stmt = bind_empty("BEGIN IF EXISTS (SELECT * FROM dbo.absent) SELECT 1; END");
    let BoundStatement::Block(statements) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert_eq!(statements.len(), 1, "{statements:?}");
    assert!(matches!(deferred(&statements[0]), Statement::If { .. }));
}

// ---------------------------------------------------------------------------
// A statement of a branch or of a loop body over a name the catalogue holds
// ---------------------------------------------------------------------------

/// A catalogue holding one table, `dbo.t (a int)`, and one taken object name, `ck1`: what
/// a branch that writes `CREATE TABLE dbo.t`, `SELECT … INTO dbo.t`, `ALTER TABLE dbo.t
/// ADD a int` or `ADD CONSTRAINT ck1` finds already there.
struct HoldsOneTable;

impl CatalogView for HoldsOneTable {
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
                table: Some(TableId(1)),
                columns: vec![ColumnBinding {
                    column: ColumnId(1),
                    index: 0,
                    name: "a".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, false),
                    depth: 0,
                }],
                kind: ResolvedTableKind::Table,
            })
    }

    fn object_name_taken(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("ck1")
    }
}

/// A binding context over [`HoldsOneTable`].
fn holding_ctx(text: &str) -> BindContext<'_> {
    BindContext {
        text,
        catalog: Some(&HoldsOneTable),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    }
}

/// Binds the first statement of `text` against [`HoldsOneTable`], returning the bound form.
fn bind_holding(text: &str) -> BoundStatement {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    bind(&batch.statements[0], &holding_ctx(text))
        .unwrap_or_else(|e| panic!("{text} should bind, got {e:?}"))
}

/// Binds the first statement of `text` against [`HoldsOneTable`], returning the error.
fn err_holding(text: &str) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    bind(&batch.statements[0], &holding_ctx(text))
        .err()
        .unwrap_or_else(|| panic!("{text} should not bind"))
}

/// The statement of a migration script: the `CREATE TABLE` of a name the catalogue already
/// resolves waits for the control of flow instead of failing the binding, so the `IF` binds
/// and its branch decides.
#[test]
fn a_branch_that_creates_a_table_the_catalogue_holds_binds_deferred() {
    let stmt = bind_holding("IF OBJECT_ID('dbo.t') IS NULL CREATE TABLE dbo.t (a int)");
    let BoundStatement::If { then_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(
        matches!(deferred(then_), Statement::CreateTable(_)),
        "the branch kept the CREATE TABLE"
    );
}

/// The counter-proof of the pair: the same `CREATE TABLE` written at the head of a batch
/// answers 2714 state 6 at binding, which is what makes the position the rule.
#[test]
fn a_head_create_table_of_a_name_the_catalogue_holds_is_still_2714() {
    let error = err_holding("CREATE TABLE dbo.t (a int)");
    assert_eq!((error.number, error.state), (2714, 6), "{error:?}");
}

/// The loop body and the block carry the same rule as the branch.
#[test]
fn a_loop_body_and_a_block_that_create_a_held_name_bind_deferred() {
    let stmt = bind_holding("WHILE 1 = 0 CREATE TABLE dbo.t (a int)");
    let BoundStatement::While { body, .. } = &stmt else {
        panic!("expected While, got {stmt:?}");
    };
    assert!(matches!(deferred(body), Statement::CreateTable(_)));

    let stmt = bind_holding("BEGIN CREATE TABLE dbo.t (a int); END");
    let BoundStatement::Block(statements) = &stmt else {
        panic!("expected Block, got {stmt:?}");
    };
    assert!(matches!(
        deferred(&statements[0]),
        Statement::CreateTable(_)
    ));
}

/// `SELECT … INTO` a name the catalogue resolves answers the same 2714 state 6 as a
/// `CREATE TABLE`, and waits the same way.
#[test]
fn a_branch_whose_select_into_names_a_held_table_binds_deferred() {
    let stmt = bind_holding("IF 1 = 0 SELECT a INTO dbo.t FROM dbo.t");
    let BoundStatement::If { then_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(matches!(deferred(then_), Statement::Select(_)));

    let error = err_holding("SELECT a INTO dbo.t FROM dbo.t");
    assert_eq!((error.number, error.state), (2714, 6), "{error:?}");
}

/// `ALTER TABLE … ADD` of a column the table has (2705 state 4) and `ADD CONSTRAINT` of a
/// name the database holds (2714 state 5) wait the same way.
#[test]
fn a_branch_that_alters_a_table_on_a_held_name_binds_deferred() {
    let stmt = bind_holding("IF 1 = 0 ALTER TABLE dbo.t ADD a int");
    let BoundStatement::If { then_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(matches!(deferred(then_), Statement::AlterTable(_)));

    let stmt = bind_holding("IF 1 = 0 ALTER TABLE dbo.t ADD CONSTRAINT ck1 CHECK (a > 0)");
    let BoundStatement::If { then_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(matches!(deferred(then_), Statement::AlterTable(_)));
}

/// The state is what separates the two 2705: a column list that writes one name twice is a
/// fault of the text and stays a binding error of the whole statement, in a branch as at
/// the head of a batch, where the `ALTER TABLE … ADD` of the test above waits.
#[test]
fn a_branch_whose_column_list_repeats_a_name_is_still_a_binding_error() {
    let error = err_holding("IF 1 = 0 CREATE TABLE dbo.x (a int, a int)");
    assert_eq!((error.number, error.state), (2705, 3), "{error:?}");

    let error = err_holding("IF 1 = 0 ALTER TABLE dbo.t ADD b int, b int");
    assert_eq!((error.number, error.state), (2705, 3), "{error:?}");

    let error = err_holding("IF 1 = 0 CREATE TABLE dbo.x (a nosuchtype)");
    assert_eq!(error.number, 2715, "{error:?}");
}

// ---------------------------------------------------------------------------
// GOTO and its labels
// ---------------------------------------------------------------------------

/// `GOTO again;` binds to [`BoundStatement::Goto`] carrying the name as it was written:
/// the binder resolves nothing, since where the jump lands is a position in a list of
/// statements it never sees.
#[test]
fn a_goto_binds_to_the_label_it_names() {
    let stmt = bind_one("GOTO again;");
    let BoundStatement::Goto { label } = &stmt else {
        panic!("expected Goto, got {stmt:?}");
    };
    assert_eq!(label, "again");
}

/// `again: SELECT 1;` is two statements: the label, which carries the name it declares,
/// then the `SELECT`.
#[test]
fn a_label_binds_to_the_name_it_declares() {
    register_builtins();
    let text = "again: SELECT 1;";
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let ctx = BindContext::scalar(text, SessionOptions::default());
    let bound: Vec<BoundStatement> = batch
        .statements
        .iter()
        .map(|statement| bind(statement, &ctx).expect("both statements bind"))
        .collect();
    assert_eq!(bound.len(), 2, "{bound:?}");
    let BoundStatement::Label { name } = &bound[0] else {
        panic!("expected Label, got {:?}", bound[0]);
    };
    assert_eq!(name, "again");
    assert!(
        matches!(&bound[1], BoundStatement::Query(_)),
        "the statement after the label is the query, got {:?}",
        bound[1]
    );
}

/// A `GOTO` written in a branch binds with the statement that holds it: the name travels
/// inside the `IF` rather than stopping the binding of the batch.
#[test]
fn a_goto_in_a_branch_binds_with_the_branch() {
    let stmt = bind_one("IF 1 = 1 GOTO again;");
    let BoundStatement::If { then_, else_, .. } = &stmt else {
        panic!("expected If, got {stmt:?}");
    };
    assert!(else_.is_none(), "no ELSE was written");
    assert!(
        matches!(then_.as_ref(), BoundStatement::Goto { label } if label == "again"),
        "then_ is the Goto, got {then_:?}"
    );
}
