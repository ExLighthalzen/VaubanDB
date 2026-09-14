//! The depth guard of the binder: however deep the tree, the **recursive descent of the
//! binder** does not overflow the stack of the thread that serves it.
//!
//! # What this guard does not promise
//!
//! It does not keep the process alive on every batch, and reading it that way sends the next
//! reader looking in the wrong file. Two other stages recurse over one tree on one thread and
//! spend nothing of the counter below: the parse of a chain of *prefix* operators and the
//! destructor of the AST. Both are guarded in the parser: `SELECT 1 WHERE NOT NOT … 1 = 1`
//! of 5 000 terms comes back a **191** from the parser, whose guard stops that shape at
//! 216, and a flat `SELECT 1 + 1 + …` of 20 000 terms comes back the 8631 below with the
//! server still answering, its tree freed by the iterative destructor of the AST (the same
//! chain is parsed and freed at 50 000 terms on a 16 MiB thread by
//! `crates/vauban-parser/tests/nesting_depth.rs`). What is bounded here is the descent of
//! `bind_expr`, no more.
//!
//! # Why the parser's guard is not this one
//!
//! The parser counts the recursion of the **parse**. The Pratt loop of `parse_expr_bp` is
//! iterative, so a chain of operators of the same precedence — `1 + 1 + 1 + …` — spends not
//! one unit of that counter: the parse walks the chain in a loop. The tree it builds is a
//! comb all the same, one `Binary` node per term, and [`bind_expr`](crate::expr::bind_expr)
//! descends it **recursively**. The two counters count two different things (the recursion
//! of the analysis, and the depth of the tree it produced) and both are needed.
//!
//! # Where the counter is spent
//!
//! One unit per entry into `bind_expr`, and nowhere else: the descents of the binder into a
//! child node go through that function — `bind_condition`, `bind_binary`, `bind_case`,
//! `bind_in`, the operands of a call in `call.rs` and the clauses of `query.rs` call it.
//! Counting there counts the depth of the tree, whatever shape of text produced it.
//!
//! # The limit
//!
//! [`MAX_BIND_DEPTH`] is **830** nodes. It is not SQL Server's limit and it is not the
//! deepest tree the binder survives: it is half of the deepest tree the **whole** server
//! survives on the narrowest stack it has, the **16 MiB** a thread of the blocking pool gets
//! (`cli` builds the runtime with `thread_stack_size(REQUEST_THREAD_STACK_SIZE)`, see
//! `crates/vauban-session/src/server.rs`, and `session` runs one request per
//! `spawn_blocking` thread).
//!
//! On a `SELECT 1 + 1 + …` of *n* terms, whose bound tree is *n* nodes deep, on a thread
//! of exactly that size (`tests/nesting_depth.rs`) and on a running `vauban serve`:
//!
//! | what runs on the thread | debug | release |
//! |---|---|---|
//! | binding alone, deepest tree that survives | 2781 | 13 805 |
//! | the server: parse, bind, execute, drop | 1669 | 13 803 |
//!
//! Execution recurses over that tree and costs more per node than binding in a debug build,
//! which is why the server gives out first there; in release the two figures are two apart.
//! The guard is at the binder because that is where the depth of the tree is known before
//! anything walks it again. Half of 1669, rounded down to a ten, is 830: a factor 2.01 on
//! the tightest of these figures (the debug profile) and a factor of 16.6 on the release
//! profile. On a 2 MiB stack those numbers would be 438, 208 and a guard of 100.
//!
//! A chain of `AND` gives 1667 conjunctions on the server in debug, a tree one node deeper
//! than the count of conjunctions, so the guard answers 8631 at 830 `1 = 1 AND …` and lets
//! 829 through, in both profiles.
//!
//! SQL Server answers the same **8631** on the `+` shape, but not before about 3 000 terms:
//! the window from 831 to about 2 950 is a deliberate difference from SQL Server.

use std::cell::Cell;

use vauban_errors::{SqlError, SqlResult};

/// Deepest expression tree the binder accepts, in nodes.
///
/// See the module documentation for where this number comes from.
pub(crate) const MAX_BIND_DEPTH: u32 = 830;

/// Number of the error a tree deeper than [`MAX_BIND_DEPTH`] raises.
///
/// Its severity, state and text belong to `errors`, which catalogues 8631; the number is
/// repeated here so that [`at_statement`] can recognise the error it must place on the line
/// of the statement.
///
/// It is **not** the number the depth of a tree raises by itself: within the same family
/// (191, the 125 of the `CASE`, 8631) the number follows the **shape of the text**, not the
/// depth of the tree that shape builds. A chain of *unary* operators is flat text and a deep
/// tree nonetheless, and SQL Server answers 191, severity 15, state 1, on it:
/// `SELECT - - … 1` computes up to 1015 operators and answers 191 from 1016 (likewise `+`
/// and `~`), and `SELECT 1 WHERE NOT NOT … 1 = 1` computes up to 1006 and answers 191 from
/// 1007. On a unary chain the number VaubanDB sends is 191 too, and it comes from the
/// parser's guard, which stops that shape at 216 to 217 operators; the guard below answers
/// 8631 on the shapes it does see, from 831 nodes. Both halves of that difference are
/// deliberate differences from SQL Server.
pub(crate) const STACK_LIMIT_NUMBER: u32 = 8631;

thread_local! {
    /// Depth of the binding in progress on this thread.
    ///
    /// The whole engine is synchronous and one request is bound at a time on the thread
    /// that serves it, so one counter per thread is one counter per binding. It is a
    /// `Cell`, not borrowed across a call.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Enters one level of the binding, or refuses the tree as too deep.
///
/// The returned guard leaves the level when it is dropped, on the way back **and** on the
/// `?` of an error, so the counter cannot drift between two statements of the same batch.
///
/// # Errors
///
/// [`STACK_LIMIT_NUMBER`] when the tree is deeper than [`MAX_BIND_DEPTH`]. The error
/// carries no line: `bind_expr` does not know where the statement starts, and SQL Server
/// reports the line of the **statement**, not of the node — see [`at_statement`].
pub(crate) fn enter() -> SqlResult<DepthGuard> {
    DEPTH.with(|depth| {
        let entered = depth.get().saturating_add(1);
        if entered > MAX_BIND_DEPTH {
            return Err(SqlError::stack_limit_reached());
        }
        depth.set(entered);
        Ok(DepthGuard)
    })
}

/// One level of binding held open; leaves it when dropped.
///
/// `#[must_use]`: a guard dropped at once would count nothing.
#[derive(Debug)]
#[must_use = "the level is left as soon as the guard is dropped"]
pub(crate) struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Gives the depth error the line of the statement it was raised in.
///
/// SQL Server reports the first line of the **statement**, not the line of the node the
/// descent gave up on: a `SELECT` whose 3 200 terms are spread over 3 200 lines answers
/// `line 2` (the line of the `SELECT`), and so does a chain written in a `WHERE` on the
/// third line. The other errors of the binder keep the line of their node, so this touches
/// 8631 alone, and while it has no line (`only_the_depth_error_takes_the_statement_line`).
pub(crate) fn at_statement(err: SqlError, line: u32) -> SqlError {
    if err.number == STACK_LIMIT_NUMBER && err.line == 0 {
        return err.with_line(line);
    }
    err
}

#[cfg(test)]
mod tests {
    use super::{DEPTH, MAX_BIND_DEPTH, STACK_LIMIT_NUMBER, at_statement, enter};
    use vauban_errors::SqlError;

    #[test]
    fn a_guard_leaves_the_level_it_entered() {
        assert_eq!(DEPTH.with(std::cell::Cell::get), 0);
        {
            let _outer = enter().expect("one level fits");
            assert_eq!(DEPTH.with(std::cell::Cell::get), 1);
            let _inner = enter().expect("two levels fit");
            assert_eq!(DEPTH.with(std::cell::Cell::get), 2);
        }
        assert_eq!(DEPTH.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn the_last_level_that_fits_is_the_limit_itself() {
        let mut guards = Vec::new();
        for level in 1..=MAX_BIND_DEPTH {
            guards.push(
                enter().unwrap_or_else(|_| panic!("level {level} is within {MAX_BIND_DEPTH}")),
            );
        }
        let refused = enter().expect_err("one level past the limit is refused");
        assert_eq!(refused.number, STACK_LIMIT_NUMBER);
        assert_eq!(refused.severity, 17);
        assert_eq!(refused.state, 1);
        assert_eq!(refused.line, 0, "the line is the statement's, added later");
        assert!(
            refused
                .message
                .starts_with("Internal error: the server ran out of stack"),
            "{}",
            refused.message
        );
        drop(guards);
        assert_eq!(DEPTH.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn a_refused_level_does_not_count_itself() {
        let mut guards = Vec::new();
        for _ in 1..=MAX_BIND_DEPTH {
            guards.push(enter().expect("within the limit"));
        }
        // Ten refusals in a row must not push the counter past the limit for good.
        for _ in 0..10 {
            assert!(enter().is_err());
        }
        assert_eq!(DEPTH.with(std::cell::Cell::get), MAX_BIND_DEPTH);
        drop(guards);
        assert!(enter().is_ok(), "the next statement binds normally");
    }

    #[test]
    fn only_the_depth_error_takes_the_statement_line() {
        let deep = at_statement(SqlError::stack_limit_reached(), 7);
        assert_eq!(deep.line, 7);
        let already_placed = at_statement(SqlError::stack_limit_reached().with_line(3), 7);
        assert_eq!(already_placed.line, 3, "a line already set is kept");
        let other = at_statement(SqlError::new(207, 16, 1, "Unknown column name 'c'."), 7);
        assert_eq!(other.line, 0, "another error keeps the line of its node");
    }
}
