#![deny(missing_docs)]
//! Crate `vauban-parser`: T-SQL lexer and parser producing a complete,
//! position-annotated AST.
//!
//! The lexer and the parser are written by hand, with no dependency other than
//! `vauban-errors`. The grammar follows [MS-TSQL] and the public T-SQL reference. The
//! crate knows nothing about tables, types or sessions: `SELECT * FROM nowhere` parses,
//! and it is the binder that rejects it.
//!
//! The AST is designed for V2 and V3 from the start: variants marked `(V2)` or `(V3)` in
//! their documentation are declared but produced by no V1 grammar rule, so that `binder`,
//! `planner` and `executor` never face a breaking type change.
//!
//! # File plan
//!
//! | File(s) | Contents |
//! |---|---|
//! | `span.rs`, `token.rs`, `keyword.rs`, `lexer.rs` | Positions, tokens, keywords, the lexer |
//! | `ast/*` | The tree, its `Drop` |
//! | `display/*` | Re-serialisation of the tree |
//! | `parser/mod.rs` | `ParseOptions`, the cursor, the nesting guard |
//! | `parser/stmt.rs`, `parser/flow.rs` | Statement dispatch, flow of control |
//! | `parser/expr.rs`, `parser/datatype.rs` | Expressions, data types |
//! | `parser/query.rs`, `parser/from.rs` | Queries, table references |
//! | `parser/dml.rs`, `parser/ddl_db.rs`, `parser/ddl_table.rs` | DML, database and table DDL |
//! | `syntax_error.rs` | Errors 102, 105 and 156 |
//! | `tests/corpus/` | Batches of common client libraries and tools |

mod ast;
mod display;
mod keyword;
mod lexer;
mod parser;
mod span;
mod syntax_error;
mod token;

pub use ast::ddl::{
    AlterDatabaseStatement, AlterTableAction, AlterTableStatement, Clustering, ColumnConstraint,
    ColumnConstraintKind, ColumnDef, ConstraintCheck, CreateDatabaseStatement,
    CreateIndexStatement, CreateTableStatement, DatabaseOption, DefaultConstraint,
    DropIndexStatement, ForeignKeyRef, Identity, IndexColumn, IndexOption, IndexOptionValue,
    IndexStorage, RefAction, SortDirection, StoragePlacement, TableConstraint, TableConstraintKind,
    TableDefinition,
};
pub use ast::expr::{
    BinaryOp, CaseArm, ColumnRef, DataType, Expr, FrameBound, FrameUnits, Ident, InList, Literal,
    ObjectName, Over, Quantifier, TypeArg, UnaryOp, WindowFrame,
};
pub use ast::proc::{
    CreateFunctionStatement, CreateProcedureStatement, CreateSequenceStatement,
    CreateTriggerStatement, CreateViewStatement, FunctionBody, FunctionReturns, MergeClause,
    MergeStatement, ProcParam, SequenceCache, TriggerEvent, TriggerTiming,
};
pub use ast::query::{
    AliasStyle, ApplyKind, CommonTableExpr, ForClause, JoinKind, OffsetFetch, OrderItem,
    PivotSource, QueryBody, QuerySpec, SelectItem, SelectStatement, SetOp, TableHint, TableRef,
    Top, UnpivotSource, With,
};
pub use ast::stmt::{
    AssignOp, AssignTarget, Assignment, Batch, CursorAction, CursorStatement, DeclareItem,
    DeclareStatement, DeleteStatement, ExecuteArg, ExecuteStatement, ExecuteTarget, GrantAction,
    GrantStatement, InsertSource, InsertStatement, OutputClause, RaiseErrorStatement,
    SetOptionStatement, SetOptionValue, SetStatement, SetValue, Statement, UpdateStatement,
    WaitforKind, WaitforStatement,
};
pub use parser::{ParameterDeclaration, ParseOptions, parse_batch, parse_parameter_declarations};
pub use span::Span;
