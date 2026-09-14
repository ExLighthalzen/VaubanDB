//! Iterative destruction of the three node families a batch can nest without bound.
//!
//! # The defect
//!
//! Derived drop glue is recursive: freeing a node frees its children, which free theirs.
//! A left-leaning chain of ten thousand nodes therefore needs ten thousand nested frames
//! to be freed, and a stack overflow in Rust is not an error a caller can catch -- the
//! runtime aborts the **process**. The abort lands *after* the parse succeeded, which is
//! what makes it easy to miss: everything went well, and the server died putting the tree
//! away.
//!
//! [`MAX_NESTING_DEPTH`](crate::parser::MAX_NESTING_DEPTH) does not cover it. That counter
//! counts re-entries into a recursive rule, and the shapes below are read by **loops**
//! that re-enter nothing: they spent no unit of it at any length tried, up to 400 000
//! links.
//!
//! # The shapes that comb, and the ones that do not
//!
//! A shape can only grow past the guard where the parser reads it in a **loop**, since a
//! rule that re-enters itself is counted. Reading the loops of `src/parser/` gives seven
//! spellings, carried by three types -- that list is a reading of the source, and what
//! follows is the trial of each of the seven. The numbers are the longest chain the
//! **derived** glue could still free on a 2 MiB stack -- tokio's default -- bisected one
//! link at a time:
//!
//! | shape | batch | node | freed, debug | freed, release |
//! |---|---|:-:|:-:|:-:|
//! | infix ladder | `SELECT 1 + 1 + …` | [`Expr::Binary`] | 18 807 | 32 942 |
//! | logical ladder | `WHERE 1 = 1 AND …` | [`Expr::Binary`] | 18 808 | 32 941 |
//! | collation suffix | `SELECT 'a' COLLATE … COLLATE …` | [`Expr::Collate`] | 18 807 | 32 942 |
//! | `UNION ALL` chain | `SELECT 1 UNION ALL …` | [`QueryBody::SetOp`] | 18 807 | 32 942 |
//! | `INTERSECT` chain | `SELECT 1 INTERSECT …` | [`QueryBody::SetOp`] | 18 807 | 32 942 |
//! | join chain | `FROM t0 CROSS JOIN t1 …` | [`TableRef::Join`] | 18 807 | 26 353 |
//! | `APPLY` chain | `FROM t0 CROSS APPLY t1 …` | [`TableRef::Apply`] | 18 807 | 26 353 |
//!
//! One link further, each of the seven aborted the process. With the destructors below, the
//! same seven are freed at 50 000 links in both profiles (`tests/nesting_depth.rs`), and
//! were freed at 200 000 in debug while that test was being sized.
//!
//! Shapes crossed and **not** kept, each for a reason of its own:
//!
//! - **Nesting that carries a bracket** -- parentheses, calls, `CASE`, scalar subqueries,
//!   derived tables, `APPLY` operands, `BEGIN…END`, prefix operators. Each costs one unit
//!   of the guard, so none of them reaches 33, let alone 18 807; that is what the rest of
//!   `tests/nesting_depth.rs` measures, shape by shape.
//! - **Breadth.** A select list, a `VALUES` row, an `IN` list, a call argument list, the
//!   `WHEN` arms of a `CASE` and the statements of a batch are `Vec`s, and a `Vec` is freed
//!   by a loop already. Tried on one of them, in debug: a batch of 200 000 `PRINT 1` is
//!   freed without this `Drop`, and nothing here changes how a `Vec` is freed.
//! - **`ELSE IF` chains.** `IF … ELSE IF … ELSE IF …` nests statements, but the statement
//!   dispatch is one of the guard's counted doors: 200 000 of them answer 191 without this
//!   `Drop`, tried in debug.
//! - **`PIVOT`/`UNPIVOT`**, which recurse through `Box<PivotSource>`: V2 variants no rule of
//!   `src/parser/` builds, so no batch produces one.
//!
//! # What is freed by recursion still, and why that is enough
//!
//! Two things, both bounded.
//!
//! A child that has **no child of its own** is left where it is rather than moved to the
//! worklist: the glue then recurses one level and stops. That is what keeps an ordinary
//! query as cheap as it was -- a tree two levels deep is freed without a single
//! allocation -- and it caps the recursion at two frames, not at the height of the tree.
//!
//! Crossing from one family to another -- `Expr::Subquery` into a `SelectStatement`, a
//! `QuerySpec` into its `FROM`, a derived table back into a `SelectStatement` -- is left to
//! the glue as well, because **each of the three crossings just named goes through a counted
//! door of the parse guard**: the parenthesis of a subquery, the one of a derived table, the `(` of
//! a parenthesised body. A parsed batch can therefore alternate families at most
//! [`MAX_NESTING_DEPTH`](crate::parser::MAX_NESTING_DEPTH) times.
//!
//! That bound is on trees the **parser** builds. A tree assembled by hand -- a test, a
//! rewriting pass -- can alternate families as deep as it likes and would still recurse;
//! nothing here protects that, and such a tree is not what a client sends.
//!
//! # The tombstones
//!
//! A child is taken out of its parent by putting something in its place, since a type that
//! implements `Drop` cannot be moved out of. The replacement is freed with the husk that
//! holds it, before this `drop` returns, and what runs between the swap and that free is this
//! destructor. [`Expr`] and [`TableRef`] each have a variant that allocates
//! nothing, so their tombstones are free; [`QueryBody`] has none -- its three variants all
//! box something -- so unlinking one set operator costs one small allocation, freed on the
//! next line. A query pays it for each [`QueryBody`] node that has a child **and is itself the
//! child of another**: the root is never unlinked, so a chain of *n* set operators costs *n*-1
//! (counted during `drop`: `SELECT 1 UNION SELECT 2` 0, `SELECT 1 UNION (SELECT 2)` 1,
//! `(SELECT 1) UNION (SELECT 2)` 2, ten `UNION ALL` 9).
//!
//! # One match per family
//!
//! Each family has exactly one `match` over its variants, [`for_each_expr_child`] and its
//! two siblings, and everything else is written in terms of it. That is deliberate: two
//! matches would drift apart, and this one has no wildcard arm, so a variant added to the
//! tree stops the build here instead of quietly going back to recursive glue.

use std::mem;

use crate::ast::expr::{Expr, InList};
use crate::ast::query::{QueryBody, QuerySpec, TableRef};
use crate::span::Span;

/// Frees an expression without recursing through its expression children.
///
/// The tree is dismantled into a worklist on the heap: the root hands over the children
/// that have children, then each of those hands over its own before being freed. One
/// worklist serves a whole tree: the root opens it, and the nodes it pops push their children
/// into it. Each popped node also opens its own `Vec` when its own `drop` runs, but no child
/// left to it has a child of its own, so it never pushes and `Vec::new` allocates nothing there.
impl Drop for Expr {
    fn drop(&mut self) {
        let mut worklist = Vec::new();
        unlink_expr(self, &mut worklist);
        while let Some(mut node) = worklist.pop() {
            // `node` is freed at the end of the turn, by this same `drop` -- which finds
            // no child with a child of its own, so the derived glue descends one more
            // level at most (the two frames the module documentation counts).
            unlink_expr(&mut node, &mut worklist);
        }
    }
}

/// Frees a query body without recursing through its set-operator operands.
impl Drop for QueryBody {
    fn drop(&mut self) {
        let mut worklist = Vec::new();
        unlink_query_body(self, &mut worklist);
        while let Some(mut node) = worklist.pop() {
            unlink_query_body(&mut node, &mut worklist);
        }
    }
}

/// Frees a table reference without recursing through its join tree.
impl Drop for TableRef {
    fn drop(&mut self) {
        let mut worklist = Vec::new();
        unlink_table_ref(self, &mut worklist);
        while let Some(mut node) = worklist.pop() {
            unlink_table_ref(&mut node, &mut worklist);
        }
    }
}

/// Moves the expression children of `node` that have children of their own onto
/// `worklist`, leaving tombstones in their place.
fn unlink_expr(node: &mut Expr, worklist: &mut Vec<Expr>) {
    for_each_expr_child(node, &mut |child| {
        if has_expr_child(child) {
            worklist.push(mem::replace(child, Expr::Placeholder(Span::EMPTY)));
        }
    });
}

/// Whether `node` holds an expression directly.
fn has_expr_child(node: &mut Expr) -> bool {
    let mut any = false;
    for_each_expr_child(node, &mut |_| any = true);
    any
}

/// Hands `visit` every expression held **directly** by `node`, in writing order.
///
/// Children that sit behind another family -- the `SELECT` of a subquery, the `OVER` of a
/// window function, the operand of a `PIVOT` -- are not children here: they are reached
/// through a node the parse guard counts, so the glue may recurse through them.
///
/// The `match` has no wildcard arm on purpose; see the note at the top of the file.
fn for_each_expr_child(node: &mut Expr, visit: &mut impl FnMut(&mut Expr)) {
    match node {
        // Leaves, and the nodes whose only children sit behind another family.
        Expr::Literal(..)
        | Expr::Column(..)
        | Expr::Variable { .. }
        | Expr::Exists(..)
        | Expr::Subquery(..)
        | Expr::NextValueFor { .. }
        | Expr::InvalidNiladic { .. }
        | Expr::Placeholder(..) => {}
        // The infix ladder, the shape that combs.
        Expr::Binary { left, right, .. } => {
            visit(left);
            visit(right);
        }
        Expr::Unary { expr, .. }
        | Expr::Nested(expr, _)
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        // The suffix that combs, the other one.
        | Expr::Collate { expr, .. }
        | Expr::Quantified { expr, .. }
        | Expr::Assign { value: expr, .. } => visit(expr),
        Expr::Function { args, .. } => {
            for arg in args {
                visit(arg);
            }
        }
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            if let Some(operand) = operand {
                visit(operand);
            }
            for arm in arms {
                visit(&mut arm.when);
                visit(&mut arm.then);
            }
            if let Some(else_) = else_ {
                visit(else_);
            }
        }
        Expr::Convert { expr, style, .. } => {
            visit(expr);
            if let Some(style) = style {
                visit(style);
            }
        }
        Expr::In { expr, list, .. } => {
            visit(expr);
            match list {
                InList::Exprs(exprs) => {
                    for item in exprs {
                        visit(item);
                    }
                }
                InList::Subquery(_) => {}
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            visit(expr);
            visit(pattern);
            if let Some(escape) = escape {
                visit(escape);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            visit(expr);
            visit(low);
            visit(high);
        }
    }
}

/// Moves the query-body children of `node` that have children of their own onto
/// `worklist`.
///
/// The tombstone left behind is the emptiest specification there is, and it is the one
/// tombstone allocation this file makes: [`QueryBody`] has no variant that boxes nothing.
/// The worklist `Vec` allocates as well, on its own account, as it grows.
fn unlink_query_body(node: &mut QueryBody, worklist: &mut Vec<QueryBody>) {
    for_each_query_body_child(node, &mut |child| {
        if has_query_body_child(child) {
            worklist.push(mem::replace(child, empty_query_body()));
        }
    });
}

/// Whether `node` holds a query body directly.
fn has_query_body_child(node: &mut QueryBody) -> bool {
    let mut any = false;
    for_each_query_body_child(node, &mut |_| any = true);
    any
}

/// Hands `visit` every query body held directly by `node`.
fn for_each_query_body_child(node: &mut QueryBody, visit: &mut impl FnMut(&mut QueryBody)) {
    match node {
        // The specification is where a body stops being a body.
        QueryBody::Select(_) => {}
        QueryBody::SetOp { left, right, .. } => {
            visit(left);
            visit(right);
        }
        QueryBody::Nested(body, _) => visit(body),
    }
}

/// A query body that holds nothing, used as a tombstone.
fn empty_query_body() -> QueryBody {
    QueryBody::Select(Box::new(QuerySpec {
        distinct: false,
        top: None,
        items: Vec::new(),
        into: None,
        from: Vec::new(),
        where_: None,
        group_by: Vec::new(),
        having: None,
        span: Span::EMPTY,
    }))
}

/// Moves the table references of `node` that have references of their own onto `worklist`.
///
/// The `ON` predicate of a join stays where it is: it is an [`Expr`], and freeing one is
/// already iterative.
fn unlink_table_ref(node: &mut TableRef, worklist: &mut Vec<TableRef>) {
    for_each_table_ref_child(node, &mut |child| {
        if has_table_ref_child(child) {
            worklist.push(mem::replace(child, empty_table_ref()));
        }
    });
}

/// Whether `node` holds a table reference directly.
fn has_table_ref_child(node: &mut TableRef) -> bool {
    let mut any = false;
    for_each_table_ref_child(node, &mut |_| any = true);
    any
}

/// Hands `visit` every table reference held directly by `node`.
fn for_each_table_ref_child(node: &mut TableRef, visit: &mut impl FnMut(&mut TableRef)) {
    match node {
        // A derived table holds a `SELECT`, a `PIVOT` holds its source behind a box the
        // parser never fills: neither is a direct child.
        TableRef::Table { .. }
        | TableRef::Derived { .. }
        | TableRef::Variable { .. }
        | TableRef::Function { .. }
        | TableRef::Pivot(_)
        | TableRef::Unpivot(_) => {}
        TableRef::Join { left, right, .. } | TableRef::Apply { left, right, .. } => {
            visit(left);
            visit(right);
        }
    }
}

/// A table reference that holds nothing, used as a tombstone: `String::new` allocates
/// nothing, so this one is free.
fn empty_table_ref() -> TableRef {
    TableRef::Variable {
        name: String::new(),
        alias: None,
        span: Span::EMPTY,
    }
}
