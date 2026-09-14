//! Statements: the batch, the `Statement` enumeration, and the data manipulation,
//! variable, transaction and flow-of-control statements.

use crate::ast::ddl::{
    AlterDatabaseStatement, AlterTableStatement, CreateDatabaseStatement, CreateIndexStatement,
    CreateTableStatement, DropIndexStatement, TableDefinition,
};
use crate::ast::expr::{ColumnRef, DataType, Expr, Ident, ObjectName};
use crate::ast::proc::{
    CreateFunctionStatement, CreateProcedureStatement, CreateSequenceStatement,
    CreateTriggerStatement, CreateViewStatement, MergeStatement,
};
use crate::ast::query::{SelectItem, SelectStatement, TableRef, Top};
use crate::span::Span;

/// A parsed batch: the statements of one client request, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// The statements, in the order they were written.
    pub statements: Vec<Statement>,
}

/// A T-SQL statement.
///
/// Variants marked (V2) or (V3) are declared but produced by no V1 grammar rule: they
/// exist so that `binder`, `planner` and `executor` never have to change type when the
/// grammar catches up. Payloads of more than three fields live in a named structure boxed
/// into the variant, which keeps the enumeration small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// `SELECT …`.
    Select(Box<SelectStatement>),
    /// `INSERT …`.
    Insert(Box<InsertStatement>),
    /// `UPDATE …`.
    Update(Box<UpdateStatement>),
    /// `DELETE …`.
    Delete(Box<DeleteStatement>),
    /// (V3) `MERGE …`.
    Merge(Box<MergeStatement>),
    /// `TRUNCATE TABLE t`.
    Truncate {
        /// The table.
        table: ObjectName,
        /// Position of the whole statement.
        span: Span,
    },
    /// `CREATE DATABASE d`.
    CreateDatabase(Box<CreateDatabaseStatement>),
    /// `ALTER DATABASE d …`.
    AlterDatabase(Box<AlterDatabaseStatement>),
    /// `DROP DATABASE [IF EXISTS] d1, d2`.
    DropDatabase {
        /// The database names.
        names: Vec<Ident>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// `USE d`.
    Use {
        /// The database name.
        database: Ident,
        /// Position of the whole statement.
        span: Span,
    },
    /// `CREATE TABLE t (…)`.
    CreateTable(Box<CreateTableStatement>),
    /// `ALTER TABLE t …`.
    AlterTable(Box<AlterTableStatement>),
    /// `DROP TABLE [IF EXISTS] t1, t2`.
    DropTable {
        /// The table names.
        names: Vec<ObjectName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// `CREATE INDEX ix ON t (c)`.
    CreateIndex(Box<CreateIndexStatement>),
    /// `DROP INDEX ix ON t`.
    DropIndex(Box<DropIndexStatement>),
    /// (V2) `CREATE [OR ALTER] PROCEDURE p …`.
    CreateProcedure(Box<CreateProcedureStatement>),
    /// (V2) `CREATE [OR ALTER] FUNCTION f …`.
    CreateFunction(Box<CreateFunctionStatement>),
    /// (V2) `CREATE [OR ALTER] VIEW v …`.
    CreateView(Box<CreateViewStatement>),
    /// (V2) `CREATE [OR ALTER] TRIGGER tr …`.
    CreateTrigger(Box<CreateTriggerStatement>),
    /// (V3) `CREATE SEQUENCE s …`.
    CreateSequence(Box<CreateSequenceStatement>),
    /// (V2) `DROP PROCEDURE [IF EXISTS] p1, p2`.
    DropProcedure {
        /// The procedure names.
        names: Vec<ObjectName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V2) `DROP FUNCTION [IF EXISTS] f1, f2`.
    DropFunction {
        /// The function names.
        names: Vec<ObjectName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V2) `DROP VIEW [IF EXISTS] v1, v2`.
    DropView {
        /// The view names.
        names: Vec<ObjectName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V2) `DROP TRIGGER [IF EXISTS] tr1, tr2`.
    DropTrigger {
        /// The trigger names.
        names: Vec<ObjectName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V3) `DROP SEQUENCE [IF EXISTS] s1, s2`.
    DropSequence {
        /// The sequence names.
        names: Vec<ObjectName>,
        /// True for `IF EXISTS`.
        if_exists: bool,
        /// Position of the whole statement.
        span: Span,
    },
    /// `DECLARE @x int = 1, @y varchar(10)`.
    Declare(Box<DeclareStatement>),
    /// `SET @x = 1`, the assignment of a variable.
    Set(Box<SetStatement>),
    /// `SET NOCOUNT ON`, the session option: a different statement from [`Statement::Set`].
    SetOption(Box<SetOptionStatement>),
    /// `IF cond stmt [ELSE stmt]`.
    If {
        /// The condition.
        condition: Expr,
        /// The statement run when the condition holds.
        then_branch: Box<Statement>,
        /// The `ELSE` statement, when written.
        else_branch: Option<Box<Statement>>,
        /// Position of the whole statement.
        span: Span,
    },
    /// `WHILE cond stmt`.
    While {
        /// The condition.
        condition: Expr,
        /// The loop body.
        body: Box<Statement>,
        /// Position of the whole statement.
        span: Span,
    },
    /// `BEGIN … END`.
    Block {
        /// The statements of the block.
        statements: Vec<Statement>,
        /// Position of the whole block.
        span: Span,
    },
    /// `BREAK`.
    Break(Span),
    /// `CONTINUE`.
    Continue(Span),
    /// `RETURN [expr]`.
    Return {
        /// The returned value, when written.
        value: Option<Expr>,
        /// Position of the whole statement.
        span: Span,
    },
    /// `PRINT expr`.
    Print {
        /// The printed expression.
        expr: Expr,
        /// Position of the whole statement.
        span: Span,
    },
    /// `EXECUTE p @a = 1` and `EXECUTE ('…')`.
    Execute(Box<ExecuteStatement>),
    /// `BEGIN TRANSACTION [name] [WITH MARK '…']`.
    BeginTransaction {
        /// The transaction name, when written.
        name: Option<Ident>,
        /// The `WITH MARK` description, when written.
        mark: Option<String>,
        /// Position of the whole statement.
        span: Span,
    },
    /// `COMMIT TRANSACTION [name]`.
    Commit {
        /// The transaction name, when written.
        name: Option<Ident>,
        /// Position of the whole statement.
        span: Span,
    },
    /// `ROLLBACK TRANSACTION [name]`.
    Rollback {
        /// The transaction or savepoint name, when written.
        name: Option<Ident>,
        /// Position of the whole statement.
        span: Span,
    },
    /// `SAVE TRANSACTION name`.
    Save {
        /// The savepoint name.
        name: Ident,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V2) `WAITFOR DELAY '…'`.
    Waitfor(Box<WaitforStatement>),
    /// (V2) `BEGIN TRY … END TRY BEGIN CATCH … END CATCH`.
    TryCatch {
        /// The statements of the `TRY` block.
        try_block: Vec<Statement>,
        /// The statements of the `CATCH` block.
        catch_block: Vec<Statement>,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V3) `THROW [number, message, state]`. The three parts are boxed to keep this
    /// enumeration small; a bare `THROW` re-raises and has none of them.
    Throw {
        /// The error number.
        number: Option<Box<Expr>>,
        /// The message.
        message: Option<Box<Expr>>,
        /// The state.
        state: Option<Box<Expr>>,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V2) `RAISERROR (…)`.
    RaiseError(Box<RaiseErrorStatement>),
    /// `GOTO label`, refused by a syntax error in V1.
    Goto {
        /// The target label.
        label: Ident,
        /// Position of the whole statement.
        span: Span,
    },
    /// `label:`, the target of a `GOTO`.
    Label {
        /// The label name.
        name: Ident,
        /// Position of the whole statement.
        span: Span,
    },
    /// (V2) The cursor statements, `DECLARE CURSOR` through `DEALLOCATE`.
    Cursor(Box<CursorStatement>),
    /// (V2) `GRANT`, `DENY` and `REVOKE`.
    Grant(Box<GrantStatement>),
    // A new construct gets a **new variant** here; never reuse an existing one for
    // something the grammar spells differently, because `binder` and `executor` match
    // exhaustively on this enumeration and must break loudly when it grows.
}

/// `INSERT INTO t (c) VALUES (…)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStatement {
    /// The target: a named table with its hints ([`TableRef::Table`]), a table variable
    /// ([`TableRef::Variable`]) or a name given parentheses
    /// ([`TableRef::Function`]). The same type as [`UpdateStatement::target`] and
    /// [`DeleteStatement::target`]; the alias is `None` on the three (`INSERT INTO @t AS x`
    /// is a 156 on SQL Server, `tests/dml.rs` `insert_target_errors`).
    pub target: TableRef,
    /// The column list, empty when the user wrote none.
    pub columns: Vec<Ident>,
    /// Where the rows come from.
    pub source: InsertSource,
    /// The `TOP` clause.
    pub top: Option<Top>,
    /// (V2) The `OUTPUT` clause.
    pub output: Option<OutputClause>,
    /// Position of the whole statement.
    pub span: Span,
}

/// Where the rows of an `INSERT` come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertSource {
    /// `VALUES (…), (…)`: one inner vector per row.
    Values(Vec<Vec<Expr>>),
    /// `INSERT … SELECT …`.
    Query(Box<SelectStatement>),
    /// `DEFAULT VALUES`.
    DefaultValues,
    /// (V2) `INSERT … EXECUTE p`.
    Execute(Box<ExecuteStatement>),
}

/// `UPDATE t SET c = 1 FROM … WHERE …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatement {
    /// The updated table or alias.
    pub target: TableRef,
    /// The `TOP` clause.
    pub top: Option<Top>,
    /// The `SET` list, never empty.
    pub assignments: Vec<Assignment>,
    /// The `FROM` clause, empty when absent.
    pub from: Vec<TableRef>,
    /// The `WHERE` predicate.
    pub where_: Option<Expr>,
    /// (V2) The `OUTPUT` clause.
    pub output: Option<OutputClause>,
    /// Position of the whole statement.
    pub span: Span,
}

/// One assignment of a `SET` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// What is assigned.
    pub target: AssignTarget,
    /// The assignment operator.
    pub op: AssignOp,
    /// The assigned expression.
    pub value: Expr,
}

/// What an assignment writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignTarget {
    /// A column.
    Column(ColumnRef),
    /// A variable, `@` included.
    Variable(String),
    /// An UPDATE item assigning a column and then its value to a variable.
    VariableAndColumn {
        /// Variable name, including `@`.
        variable: String,
        /// Column receiving the assignment expression.
        column: ColumnRef,
    },
}

/// The operator of an assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Set,
    /// `+=`
    AddAssign,
    /// `-=`
    SubAssign,
    /// `*=`
    MulAssign,
    /// `/=`
    DivAssign,
    /// `%=`
    ModAssign,
    /// `&=`
    BitAndAssign,
    /// `|=`
    BitOrAssign,
    /// `^=`
    BitXorAssign,
}

/// `DELETE FROM t WHERE …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStatement {
    /// The table rows are deleted from.
    pub target: TableRef,
    /// The `TOP` clause.
    pub top: Option<Top>,
    /// The second `FROM` clause, empty when absent.
    pub from: Vec<TableRef>,
    /// The `WHERE` predicate.
    pub where_: Option<Expr>,
    /// (V2) The `OUTPUT` clause.
    pub output: Option<OutputClause>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) `OUTPUT inserted.c INTO t (c)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputClause {
    /// The returned items.
    pub items: Vec<SelectItem>,
    /// The `INTO` target, when written: a [`TableRef::Table`] without alias nor hints,
    /// or a [`TableRef::Variable`]. SQL Server refuses an alias, a hint list
    /// and a function there (`tests/dml.rs` `output_into_targets`).
    pub into: Option<TableRef>,
    /// The column list of the `INTO` target.
    pub into_columns: Vec<Ident>,
}

/// `DECLARE @x int = 1, @t TABLE (…)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclareStatement {
    /// The declared items, in order.
    pub items: Vec<DeclareItem>,
    /// Position of the whole statement.
    pub span: Span,
}

/// One item of a `DECLARE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclareItem {
    /// A scalar variable, with its optional initial value.
    Variable {
        /// The name, `@` included.
        name: String,
        /// The declared type.
        ty: DataType,
        /// The `= expr` initial value, boxed to keep this enumeration small.
        default: Option<Box<Expr>>,
    },
    /// (V2) A table variable, `DECLARE @t TABLE (…)`.
    TableVariable {
        /// The name, `@` included.
        name: String,
        /// The columns and constraints.
        definition: Box<TableDefinition>,
    },
    /// (V2) A cursor variable, `DECLARE @c CURSOR`.
    Cursor {
        /// The name, `@` included.
        name: String,
    },
}

/// `SET @x = expr`: the assignment of a variable, not a session option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetStatement {
    /// The assigned variable.
    pub target: AssignTarget,
    /// The assignment operator.
    pub op: AssignOp,
    /// The assigned value.
    pub value: SetValue,
    /// Position of the whole statement.
    pub span: Span,
}

/// The right-hand side of a `SET @x = …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetValue {
    /// An expression.
    Expr(Expr),
    /// A parenthesised query, `SET @x = (SELECT …)`.
    Query(Box<SelectStatement>),
}

/// `SET NOCOUNT ON`, `SET LOCK_TIMEOUT 5000`,
/// `SET TRANSACTION ISOLATION LEVEL READ COMMITTED`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOptionStatement {
    /// The options set by this statement, each with its value.
    ///
    /// One statement can carry several names: `SET ANSI_NULLS, ANSI_PADDING ON` yields two
    /// entries sharing the same value.
    pub options: Vec<(String, SetOptionValue)>,
    /// Position of the whole statement.
    pub span: Span,
}

/// The value given to a session option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetOptionValue {
    /// `ON`
    On,
    /// `OFF`
    Off,
    /// A value, as in `SET LOCK_TIMEOUT 5000`.
    Value(Expr),
    /// A word, as the `READ COMMITTED` of an isolation level, kept as written.
    Word(String),
}

/// `EXECUTE p @a = 1 OUTPUT` and `EXECUTE ('…')`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteStatement {
    /// What is executed.
    pub target: ExecuteTarget,
    /// The arguments, in order.
    pub args: Vec<ExecuteArg>,
    /// The `@rc = EXECUTE …` variable receiving the return status.
    pub return_into: Option<String>,
    /// True when the user wrote no `EXEC`: a bare `p 1, 2` batch.
    pub implicit: bool,
    /// Position of the whole statement.
    pub span: Span,
}

/// What an `EXECUTE` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteTarget {
    /// A stored procedure, by name.
    Procedure(ObjectName),
    /// A procedure named by a variable, `EXEC @p`.
    Variable(String),
    /// A string of T-SQL, `EXEC ('SELECT 1')`.
    Literal(Box<Expr>),
}

/// One argument of an `EXECUTE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteArg {
    /// The parameter name, `@` included, for a named argument.
    pub name: Option<String>,
    /// The value.
    pub value: Expr,
    /// True when `OUTPUT` (or `OUT`) followed the value.
    pub output: bool,
}

/// (V2) `WAITFOR DELAY '00:00:01'` or `WAITFOR TIME '23:00'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitforStatement {
    /// `DELAY` or `TIME`.
    pub kind: WaitforKind,
    /// The delay or the time.
    pub value: Expr,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) What a `WAITFOR` waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitforKind {
    /// `DELAY`: a duration.
    Delay,
    /// `TIME`: an instant.
    Time,
}

/// (V2) `RAISERROR (message, severity, state, args…) WITH NOWAIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaiseErrorStatement {
    /// The message, a string, a number or a variable.
    pub message: Expr,
    /// The severity.
    pub severity: Expr,
    /// The state.
    pub state: Expr,
    /// The substitution arguments.
    pub args: Vec<Expr>,
    /// The `WITH` options, kept as written (`LOG`, `NOWAIT`, `SETERROR`).
    pub options: Vec<String>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) One of the cursor statements, all carried by one structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorStatement {
    /// Which statement it is.
    pub action: CursorAction,
    /// The cursor name, `@` included for a cursor variable.
    pub name: String,
    /// The query of a `DECLARE … CURSOR FOR`.
    pub query: Option<Box<SelectStatement>>,
    /// The `INTO @a, @b` variables of a `FETCH`.
    pub into: Vec<String>,
    /// The declaration options, kept as written (`LOCAL`, `FAST_FORWARD`, …).
    pub options: Vec<String>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) Which cursor statement a [`CursorStatement`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    /// `DECLARE c CURSOR FOR …`
    Declare,
    /// `OPEN c`
    Open,
    /// `FETCH NEXT FROM c INTO …`
    Fetch,
    /// `CLOSE c`
    Close,
    /// `DEALLOCATE c`
    Deallocate,
}

/// (V2) `GRANT`, `DENY` and `REVOKE`, all carried by one structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStatement {
    /// Which statement it is.
    pub action: GrantAction,
    /// The permissions, as written (`SELECT`, `EXECUTE`, …).
    pub permissions: Vec<String>,
    /// The securable the permissions apply to, when written.
    pub on: Option<ObjectName>,
    /// The principals the permissions are granted to.
    pub principals: Vec<Ident>,
    /// Position of the whole statement.
    pub span: Span,
}

/// (V2) Which permission statement a [`GrantStatement`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantAction {
    /// `GRANT`
    Grant,
    /// `DENY`
    Deny,
    /// `REVOKE`
    Revoke,
}
