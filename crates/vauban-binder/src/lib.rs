#![deny(missing_docs)]
//! Crate `vauban-binder`: gives a meaning to the AST the `parser` produced.
//!
//! The binder resolves names, infers and checks the type of every expression, and produces
//! a **bound logical plan** the `planner` can optimise without ever looking at the SQL text
//! again. It computes nothing: constant folding, even of `1 + 1`, is not its job.
//!
//! # Catalogue access
//!
//! [`BindContext::catalog`] is an `Option<&dyn CatalogView>`: a supplied view resolves
//! tables, views and table-valued functions, and
//! [`CatalogSnapshot`](vauban_catalog::CatalogSnapshot) implements [`CatalogView`]
//! (`catalog_view.rs`). [`BoundStatement`] is deliberately **not** `#[non_exhaustive]`:
//! adding a variant must break the compilation of the consumers of the workspace rather
//! than pass unnoticed.
//!
//! # No boolean type
//!
//! T-SQL has no boolean type: a predicate is not a scalar expression. The bound plan says
//! so by the **variant**, not by the type — [`BoundExpr::is_predicate`] tells the two
//! apart, and the `ty` of a predicate is `bit`. Implicit conversions are **inserted
//! explicitly** into the plan as [`BoundExprKind::Convert`] nodes: after binding, the
//! executor decides no conversion of its own.
//!
//! # File plan
//!
//! | File(s) | Contents |
//! |---|---|
//! | `context.rs`, `bound/mod.rs`, `errors.rs`, this file | context, bound tree, error helpers |
//! | `catalog_view.rs` | the catalogue snapshot as a [`CatalogView`] |
//! | `names.rs`, `star.rs` | name resolution and `*` expansion |
//! | `view.rs`, `ddl.rs`, `ddl_index.rs`, `alter.rs` | views and DDL |
//! | `statement.rs` | dispatch by statement kind |
//! | `datatype.rs`, `literal.rs` | type names and literals |
//! | `expr.rs`, `call.rs` | scalar expressions and function calls |
//! | `query.rs`, `join.rs`, `aggregate.rs`, `sort.rs`, `subquery.rs`, `setop.rs` | queries |
//! | `variables.rs`, `control.rs` | variables and control flow |
//! | `insert.rs`, `update_delete.rs`, `txn_stmt.rs`, `hints.rs` | DML, transactions, hints |
//! | `depth.rs`, `tests/nesting_depth.rs` | the depth guard |
//!
//! A statement form the binder does not handle yet answers an internal error that names
//! the statement.
//!
//! # Depth
//!
//! The binder walks the tree the parser built **recursively**, and a flat text can build a
//! deep tree: `1 + 1 + 1 + …` is one loop of the parser and a comb of one node per term.
//! `depth.rs` bounds **that descent, and only that one** (`tests/nesting_depth.rs`): past
//! the bound the binder answers 8631 instead of recursing, so the tree it is handed does not
//! overflow the stack while it is bound.
//!
//! Two other stages walk the tree on the thread and spend nothing of this counter: the
//! parse of a chain of *prefix* operators, which the parser's own depth guard bounds
//! (`SELECT 1 WHERE NOT NOT … 1 = 1` answers 191 from the parser), and the destructor of
//! the AST, which the parser made iterative. What is bounded here is the descent of
//! `bind_expr`, up to the `MAX_BIND_DEPTH` of `depth.rs` (830 nodes), and nothing else.

mod aggregate;
mod alter;
mod bound;
mod call;
mod catalog_view;
mod context;
mod control;
mod datatype;
mod ddl;
mod ddl_index;
mod depth;
mod errors;
mod expr;
mod hints;
mod insert;
mod join;
mod literal;
mod names;
mod query;
mod setop;
mod sort;
mod star;
mod statement;
mod subquery;
mod txn_stmt;
mod update_delete;
mod variables;
mod view;

pub use bound::{
    AggregateCall, BoundCaseArm, BoundDeclaration, BoundExpr, BoundExprKind, BoundProjection,
    BoundStatement, BoundTop, ColumnBinding, CompareOp, DdlStatement, DeletePlan, InsertPlan,
    JoinKind, LockHints, LogicalOp, LogicalPlan, OutputColumn, OutputSchema, SetOpKind, SortKey,
    TxnStatement, UpdatePlan,
};
pub use context::{
    BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
    TableReferenceKind, VariableScope,
};
pub use statement::bind;
pub use variables::BatchVariables;
