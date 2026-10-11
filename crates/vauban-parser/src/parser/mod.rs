//! The recursive-descent parser.
//!
//! [`ParseOptions`], [`parse_batch`], the `Parser` cursor and its token-consuming helpers
//! live here. The grammar itself lives in the sibling modules (see the file plan in
//! `lib.rs`).
//!
//! # What this file is, and what it is not
//!
//! It is the cursor and nothing else. Every grammar rule, down to the smallest, lives in
//! a sibling module; `mod.rs` is the one file they share, so it holds no grammar at all.
//!
//! # The rule the whole module relies on: a failure consumes nothing
//!
//! Every `expect_*` and every `parse_*` of the cursor leaves `pos` untouched when it
//! returns `Err`. The grammar tries an alternative and falls back all the time, and it
//! can only do so if a failed attempt costs nothing. Anything that needs more than one
//! token to decide uses [`Parser::mark`] and [`Parser::reset`].

mod datatype;
mod ddl_db;
mod ddl_table;
mod dml;
mod parameters;
pub use parameters::*;
// `pub(crate)`: `display::expr` shares `expr::NILADIC_FUNCTIONS`.
pub(crate) mod expr;
mod flow;
mod from;
mod query;
mod stmt;

use std::mem;

use vauban_errors::{SqlError, SqlResult};

use crate::ast::expr::{Ident, ObjectName};
use crate::ast::stmt::Batch;
use crate::keyword::Keyword;
use crate::lexer::tokenize;
use crate::span::Span;
use crate::syntax_error;
use crate::token::{Punct, Token, TokenKind};

/// The session settings that change how a batch is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOptions {
    /// The `QUOTED_IDENTIFIER` session option. When true, `"x"` is a delimited
    /// identifier; when false, it is a character string literal.
    pub quoted_identifier: bool,
}

/// `QUOTED_IDENTIFIER ON`, which is what every modern client driver sets on connect.
impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            quoted_identifier: true,
        }
    }
}

/// Parses the text of one batch into its statements.
///
/// The text is a whole batch: the `GO` separator is a client-side word and never reaches
/// the server. Errors are client-visible `SqlError`s (102, 105, 156).
///
/// # How a batch is cut into statements
///
/// The `;` is a **separator**, not a terminator: it may be left out (`SELECT 1 SELECT 2`
/// is two statements), written after the last statement, or written several times in a
/// row (an empty statement is ignored). A text that is empty, blank or nothing but a
/// comment yields a `Batch` with no statement at all.
///
/// # Errors
///
/// The first syntax error stops everything: SQL Server never compiles a batch partly,
/// the message it sends is a single one and the `DONE` token carries `ERROR`. There is
/// no error recovery and no list of errors.
pub fn parse_batch(text: &str, opts: &ParseOptions) -> SqlResult<Batch> {
    let mut p = Parser::new(text, opts)?;
    let mut statements = Vec::new();
    // Whether the statement just read ended without a `;`: a CTE cannot follow it.
    let mut unterminated = false;
    while !p.at_eof() {
        if p.eat_punct(Punct::Semicolon) {
            unterminated = false;
            continue;
        }
        // `first_in_batch`: the flag the implicit `EXEC` rule reads.
        statements.push(stmt::parse_statement(
            &mut p,
            statements.is_empty(),
            unterminated,
        )?);
        // A `;` after a statement is optional: it is eaten when it is there.
        unterminated = !p.eat_punct(Punct::Semicolon);
    }
    // A batch that reads to its end without a syntax error but holds a shape too deep for
    // the engine is refused as a whole, with the first such error: a syntax error later in
    // the text wins over it (`tests/nesting_depth.rs`,
    // `a_syntax_error_wins_over_a_set_operator_chain_too_deep`).
    if let Some(err) = p.too_deep.take() {
        return Err(err);
    }
    Ok(Batch { statements })
}

/// The token every out-of-range read yields, so that the cursor never indexes past its
/// vector and never needs an `unwrap`.
///
/// `tokenize` always ends its vector with an `Eof` token, so this value is in practice
/// unreachable; it is the safety net that keeps [`Parser::peek`] total.
static PAST_THE_END: Token = Token {
    kind: TokenKind::Eof,
    text: String::new(),
    span: Span::EMPTY,
};

/// How deep the descent may go, counted in re-entries into a recursive rule.
///
/// # Why a guard at all
///
/// A recursive-descent parser turns the nesting the client wrote into frames on the
/// thread stack, and a stack overflow in Rust is not an error but an **abort of the whole
/// process**: every connection of the server dies, not just the one that sent the batch.
/// `SELECT ((((…))))` was therefore a remote, unauthenticated denial of service until this
/// counter existed.
///
/// # The stack this number is taken against
///
/// The reference is the **narrowest** stack a batch is parsed on, not the main thread's
/// and not a test binary's: `session` hands each request to `spawn_blocking`, and `cli`
/// builds its runtime with `thread_stack_size(REQUEST_THREAD_STACK_SIZE)`, so its workers
/// and its blocking pool are given the **16 MiB** fixed in
/// `crates/vauban-session/src/server.rs`. The figures below are taken on 16 MiB, in a
/// **debug** build. The abort of the unguarded binary named a tokio thread.
///
/// # Why this number
///
/// 220 is **half of what the whole server survives in debug**, counted in units of this
/// counter, on the shape that costs the most stack per unit. That shape is a **derived
/// table** -- `SELECT * FROM (SELECT * FROM (…) t) u`, with `CROSS APPLY` and `OUTER
/// APPLY` costing it within one level -- because one unit of this counter buys the chain
/// of *uncounted* frames between two query bodies: query body, set-operator term, query
/// spec, `FROM`, join tree, table primary, derived table, subquery, select, query body
/// again. That is about ten frames per unit, against three for a parenthesis; counting
/// *units*, the dearest shape is the scalar subquery, which is not the same question.
///
/// On a 16 MiB stack, a chain of derived tables comes back an error up to **439**
/// levels and takes the process down at 440 in debug (1781 / 1782 in release). Half of 439
/// is 219.5 levels, which is 222 units of this counter; rounded down to a ten, 220, and
/// 220 units let 216 derived tables through -- a margin of 2.03 over the debug abort.
///
/// Bisected stack size by stack size on the deepest batch of each shape this constant lets
/// through (the same shapes are in `tests/nesting_depth.rs`, which re-derives the depths
/// from the guard rather than hard-coding them):
///
/// | shape | levels let through | smallest stack it parses on, debug | release |
/// |---|:-:|:-:|:-:|
/// | derived table, `CROSS APPLY` | 216 | **8081 KiB** | 1985 KiB |
/// | scalar subquery | 108 | 4641 KiB | 1137 KiB |
/// | `EXISTS` | 108 | 3857 KiB | 1041 KiB |
/// | `CASE` | 216 | 4081 KiB | 465 KiB |
/// | nested call | 217 | 3409 KiB | 657 KiB |
/// | parenthesis | 217 | 3217 KiB | 465 KiB |
/// | prefix `-` in the select list | 217 | 1873 KiB | 465 KiB |
/// | prefix `NOT` in a `WHERE` | 216 | 1857 KiB | 465 KiB |
/// | `BEGIN…END` | 218 | 609 KiB | 129 KiB |
///
/// So the margin over the 16 MiB reference stack is a factor of **2.03** (16384 / 8081),
/// taken on the derived table; the eight other shapes of the table above parse on 4641 KiB
/// or less.
///
/// **The debug build is the worst case, not the best.** The same derived table parses on
/// 1985 KiB in a release build -- 4.1 times less, which would leave a margin of 8.3 rather
/// than 2.03 -- so a peak taken in release understates the real one about fourfold.
/// Whoever narrows this stack or raises this constant must re-bisect **the derived
/// table**, in **debug**: it is the shape that sets the limit, and it sits 2.03 away from
/// an abort.
///
/// # This number is not dictated by the parser
///
/// The three stages a batch goes through, bisected on 16 MiB, `+` chain for the two
/// downstream stages and derived tables for the parse:
///
/// | stage | holds to (debug) | (release) |
/// |---|:-:|:-:|
/// | parse + drop of a flat `+` chain | ≥ 60 000 | ≥ 60 000 |
/// | binding alone, `+` chain | 2781 | 13 805 |
/// | **whole server**, `+` chain | **1669** | **13 803** |
/// | whole server, derived tables | 439 | 1781 |
///
/// Execution gives way before the binding in debug, and the parse of a derived table
/// before either. This constant is therefore not protecting the parser from itself; it is
/// kept where it is because it protects **the shortest link of the chain**. The 2 MiB
/// default held 208 nodes of a `+` chain and 53 derived tables in debug; the 16 MiB stack
/// multiplied those by 8.0 and 8.3.
///
/// # Raising this constant means reading the binder's guard first
///
/// This threshold is 220 units of this counter; the binder's, on the same stack, is 830
/// levels of `bind_expr`, past which it sends **8631**. On a shape this
/// counter counts -- parentheses, nested calls, derived tables -- the parse runs before the
/// binding, so **191 comes out first**, and 191 is what SQL Server sends on those shapes.
/// That ordering is a consequence of 220 < 830 and of nothing else: **raise this constant
/// above the binder's without raising the binder's, and 8631 would come out where SQL
/// Server says 191.** The two are raised together for exactly this reason.
///
/// # What this guard does *not* close
///
/// The counter counts re-entries into a recursive rule, so a **flat** chain of operators
/// spends no unit at all: the expression ladder is an iterative Pratt loop, and such a
/// batch goes through untouched however long it is.
///
/// **"Flat" means infix, and infix alone**. A chain of *prefix* operators reads
/// nothing like one: `expr::parse_prefix` reads its operand by re-entering
/// `parse_expr_bp`, which is this counter's expression door, so `NOT`, `-`, `+` and `~`
/// each cost **one unit, exactly like one parenthesis**. On the reference stack, at the
/// level: `SELECT - - … 1` and `SELECT ((… 1 …))` both parse at 217 and answer 191 at 218;
/// `SELECT 1 WHERE NOT … 1 = 1` parses at 216, `SELECT CASE WHEN NOT … 1 = 1 THEN 1 ELSE
/// 0 END` at 215. A tree without this guard dies between 240 and 250 `NOT` on a debug
/// server; with it, that shape answers 191. `tests/nesting_depth.rs` crosses the four
/// operators and eight positions so that the sentence above is never misread as covering
/// the prefix operators.
///
/// Two stack hazards live past this point and neither is a `parser` matter:
///
/// - **binding** such a chain used to abort the process; the binder's guard, 830 levels,
///   closes it and sends 8631 past them.
/// - **dropping** the AST of such a chain recursed and aborted until the destructor was
///   made iterative: on the reference stack, a flat `SELECT 1 + 1 + …` parses and is freed
///   at 60 000 terms in debug as in release, the longest tried here.
///
/// Neither of them is reachable through a prefix chain, since this counter stops it at 218:
/// `std::mem::forget` on the parsed batch moves the abort of a pre-guard parser by not one
/// level (250 with `drop`, 250 with `forget`, bisected on the reference stack in debug), so
/// the stage that gave way on that shape was the **parse**, never the destructor.
///
/// Do not read this guard as closing the whole class.
///
/// # What it costs in fidelity
///
/// SQL Server is far more generous, and its own limit is neither one number nor one error
/// -- it accepts
/// 1015 nested parentheses, 1013 nested calls, 509 nested `BEGIN…END`, 168 nested scalar
/// subqueries and 83 nested `EXISTS`, and answers 191 one level past each. That is a
/// factor of twelve between its most and least generous shapes. The `CASE` has a rule of
/// its own: an eleventh one answers **125**, severity 15, state 4 (a `CASE` cannot be
/// nested deeper than level 10), and was, while this constant stood at 32, the single shape
/// on which VaubanDB was the *more* permissive of the two.
///
/// At 220 units the distance is what it is: on the shapes of
/// `tests/nesting_depth.rs`, VaubanDB parses 217 parentheses against 1015, 217 calls
/// against 1013, 218 `BEGIN…END` against 509, 216 derived tables against 100 and 108
/// scalar subqueries against 168, and answers 191 one level past each. Two shapes have
/// crossed over since the guard was raised -- the derived table (216 against 100) and the
/// `EXISTS` chain (108 against 83) -- and are refused sooner by SQL Server than by us.
/// Raising this number further is a `session` matter -- a wider stack for the blocking
/// pool -- and not a `parser` one; it went from 32 to 220 when that stack went from 2 MiB
/// to 16 MiB.
///
/// This gap is a deliberate difference from SQL Server.
pub(crate) const MAX_NESTING_DEPTH: u32 = 220;

/// How many `ELSE` keywords an `IF … ELSE IF … ELSE …` chain written at the top of a
/// batch may hold, the last plain `ELSE` included.
///
/// `flow::parse_if` reads such a chain in a loop, so its links spend no unit of
/// [`MAX_NESTING_DEPTH`]; [`Parser::open_else`] counts them against this budget instead.
/// At the top of a batch, the chain of 253 `ELSE` runs and the 254th `ELSE` is answered
/// with 191 on its own line (`tests/nesting_depth.rs`,
/// `else_if_chain_stops_at_its_own_limit`). The budget is shared: the `ELSE` of the
/// chains that enclose this one count, and so does each enclosing statement level, at
/// half the weight of an `ELSE` (`tests/nesting_depth.rs`,
/// `nested_else_if_chains_share_one_budget`). Without that sharing, chains nested through
/// `ELSE BEGIN … END` would build a tree as deep as their total length.
pub(crate) const MAX_ELSE_CHAIN: u32 = 253;

/// A cursor over the tokens of one batch, with the helpers every grammar rule shares.
///
/// The token vector always ends with [`TokenKind::Eof`] and `pos` never goes past it, so
/// [`Parser::peek`] always has a token to return and the grammar never has to test for
/// the end of the vector, only for `Eof`.
pub(crate) struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    opts: &'a ParseOptions,
    /// How many recursive rules are currently on the stack; see [`Parser::nested`].
    depth: u32,
    /// How many `CASE` expressions the cursor is inside; see [`Parser::nested_case`].
    case_depth: u32,
    /// How many `ELSE` of `IF` chains enclose the cursor; see [`Parser::open_else`].
    else_links: u32,
    /// The deepest set-operator tree finished in the current window; see
    /// [`Parser::measure_set_ops`].
    set_op_depth: u32,
    /// The line of the statement the cursor is in; see [`Parser::in_statement`].
    statement_line: u32,
    /// The first error 8631 met so far, sent once the batch has no syntax error; see
    /// [`Parser::defer_too_deep`].
    too_deep: Option<SqlError>,
}

impl<'a> Parser<'a> {
    /// Lexes `text` and puts the cursor on its first token.
    ///
    /// # Errors
    ///
    /// The lexical error, which at this point can only be the 105 of an unclosed
    /// quotation mark. A character that starts no T-SQL token is not a lexical error: it
    /// becomes a [`TokenKind::Unknown`] token that the grammar reports as a 102.
    pub(crate) fn new(text: &str, opts: &'a ParseOptions) -> SqlResult<Self> {
        Ok(Self {
            tokens: tokenize(text, opts)?,
            pos: 0,
            opts,
            depth: 0,
            case_depth: 0,
            else_links: 0,
            set_op_depth: 0,
            statement_line: 1,
            too_deep: None,
        })
    }

    /// Runs `rule`, which parses one statement, with [`Parser::statement_line`] on the
    /// line of the token the cursor sits on, and puts the enclosing statement's line back
    /// afterwards, whether `rule` succeeded or failed.
    ///
    /// # Errors
    ///
    /// What `rule` returns.
    pub(crate) fn in_statement<T>(
        &mut self,
        rule: impl FnOnce(&mut Self) -> SqlResult<T>,
    ) -> SqlResult<T> {
        let line = self.peek().span.line;
        let outer = mem::replace(&mut self.statement_line, line);
        let parsed = rule(self);
        self.statement_line = outer;
        parsed
    }

    /// Records error 8631 on the line of the current statement, to be sent when the whole
    /// batch has been read without a syntax error. The first one is kept, a later one dropped.
    ///
    /// The grammar goes on after this call, so it must not build the shape that made it:
    /// the caller drops what would have gone past its limit.
    pub(crate) fn defer_too_deep(&mut self) {
        if self.too_deep.is_none() {
            self.too_deep = Some(SqlError::stack_limit_reached().with_line(self.statement_line));
        }
    }

    /// Runs `rule` in a window of its own and returns, with what it parsed, the depth of
    /// the deepest set-operator tree finished inside it -- through a derived table, a
    /// subquery or parentheses as well. The depth noted by the enclosing window is put
    /// back afterwards, whether `rule` succeeded or failed.
    ///
    /// # Errors
    ///
    /// What `rule` returns.
    pub(crate) fn measure_set_ops<T>(
        &mut self,
        rule: impl FnOnce(&mut Self) -> SqlResult<T>,
    ) -> SqlResult<(T, u32)> {
        let outer = mem::take(&mut self.set_op_depth);
        let parsed = rule(self);
        let inner = mem::replace(&mut self.set_op_depth, outer);
        parsed.map(|value| (value, inner))
    }

    /// Records that a query body of `depth` set-operator levels has just been finished.
    pub(crate) fn record_set_op_depth(&mut self, depth: u32) {
        self.set_op_depth = self.set_op_depth.max(depth);
    }

    /// Runs one **recursive** rule one level deeper, or refuses to go there at all.
    ///
    /// Every rule that can re-enter itself, however long the way round, goes through this
    /// method: the expression ladder, the query body and the statement dispatch. Three
    /// call sites are enough because no fourth path closes a **grammar cycle** -- a derived
    /// table, a scalar subquery, a parenthesised body, a `BEGIN…END` and a nested join all
    /// come back through one of them: 35 nesting shapes sent 3 000 levels deep each came
    /// back a 191 (`tests/nesting_depth.rs`).
    ///
    /// "No cycle" is not "no recursion", though: a **flat** chain of operators recurses
    /// nowhere and so spends nothing here, yet still builds a deep AST. See
    /// [`MAX_NESTING_DEPTH`] § "What this guard does *not* close".
    ///
    /// The counter is decremented whether the rule succeeded or failed, so the backtracking
    /// the grammar does all the time (see the module heading) never leaks a level.
    ///
    /// # Errors
    ///
    /// Error 191 when the batch is nested deeper than [`MAX_NESTING_DEPTH`], on the token
    /// the cursor sits on; otherwise whatever `rule` returns.
    pub(crate) fn nested<T>(
        &mut self,
        rule: impl FnOnce(&mut Self) -> SqlResult<T>,
    ) -> SqlResult<T> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.nested_too_deeply());
        }
        self.depth += 1;
        let parsed = rule(self);
        self.depth -= 1;
        parsed
    }

    /// Counts one more `ELSE` of an `IF` chain against [`MAX_ELSE_CHAIN`], or refuses it.
    ///
    /// The budget is reckoned in halves: an `ELSE` costs two, each level of
    /// [`Parser::nested`] the cursor is inside costs one, and the total may not pass
    /// `2 * MAX_ELSE_CHAIN + 1`. The statement level of a chain at the top of a batch is
    /// that `+ 1`, so such a chain holds exactly [`MAX_ELSE_CHAIN`] `ELSE`.
    ///
    /// # Errors
    ///
    /// Error 191 on the token the cursor sits on, the `ELSE` itself, when the budget is
    /// spent; the count is then left as it was.
    pub(crate) fn open_else(&mut self) -> SqlResult<()> {
        let spent = 2 * (self.else_links + 1) + self.depth;
        if spent > 2 * MAX_ELSE_CHAIN + 1 {
            return Err(self.nested_too_deeply());
        }
        self.else_links += 1;
        Ok(())
    }

    /// Gives back `count` `ELSE` opened by [`Parser::open_else`] when their chain ends.
    pub(crate) fn close_elses(&mut self, count: u32) {
        self.else_links = self.else_links.saturating_sub(count);
    }

    /// How many `CASE` expressions enclose the token the cursor sits on.
    ///
    /// A second counter, next to the one of [`Parser::nested`], because the `CASE` has a
    /// limit of its own on SQL Server -- ten levels, then error 125 -- which is neither the
    /// number nor the error of the general guard. The threshold, the error and
    /// what counts as a level live with the `CASE` grammar in `expr.rs`; this cursor holds
    /// the count and nothing else.
    pub(crate) fn case_depth(&self) -> u32 {
        self.case_depth
    }

    /// Runs the body of one `CASE` one level deeper on the `CASE` counter.
    ///
    /// The counter is decremented whether `rule` succeeded or failed, like the one of
    /// [`Parser::nested`], so that a `CASE` refused half-way leaks no level.
    ///
    /// # Errors
    ///
    /// What `rule` returns; this method refuses nothing itself.
    pub(crate) fn nested_case<T>(
        &mut self,
        rule: impl FnOnce(&mut Self) -> SqlResult<T>,
    ) -> SqlResult<T> {
        self.case_depth += 1;
        let parsed = rule(self);
        self.case_depth -= 1;
        parsed
    }

    /// The 191 that [`Parser::nested`] sends when the descent has gone deep enough.
    ///
    /// The number, severity, state and text belong to `errors`, which catalogues 191;
    /// this method puts it on the line the cursor sits on, nothing more.
    fn nested_too_deeply(&self) -> SqlError {
        SqlError::nested_too_deeply(self.peek().span.line)
    }

    /// The token at `index`, or the final `Eof` when `index` is past the end.
    fn token_at(&self, index: usize) -> &Token {
        let last = self.tokens.len().saturating_sub(1);
        self.tokens.get(index.min(last)).unwrap_or(&PAST_THE_END)
    }

    /// The token the cursor sits on, without consuming it.
    pub(crate) fn peek(&self) -> &Token {
        self.token_at(self.pos)
    }

    /// The token `n` positions further, without consuming anything; `Eof` past the end.
    ///
    /// `peek_at(0)` is [`Parser::peek`]. This is how a rule looks ahead when it only
    /// needs to read, rather than to try and fall back: the statement dispatch tells
    /// `CREATE TABLE` from `CREATE UNIQUE CLUSTERED INDEX` with it.
    pub(crate) fn peek_at(&self, n: usize) -> &Token {
        self.token_at(self.pos.saturating_add(n))
    }

    /// Consumes the current token and returns a clone of it.
    ///
    /// On `Eof` the cursor does not move and `Eof` is returned again, as many times as
    /// asked: a rule that loops until it has what it needs cannot run off the end.
    ///
    /// The token is **cloned** (it owns a `String`). That is deliberate: handing out
    /// borrowed tokens would freeze the cursor for the caller's whole rule, and the cost
    /// is nothing next to executing the query the parse produces.
    pub(crate) fn advance(&mut self) -> Token {
        let token = self.token_at(self.pos).clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    /// Whether the cursor sits on the end of the batch.
    pub(crate) fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// Whether the cursor sits on the keyword `k`, whatever case the user wrote it in.
    pub(crate) fn at_keyword(&self, k: Keyword) -> bool {
        matches!(self.peek().kind, TokenKind::Keyword(found) if found == k)
    }

    /// Consumes the keyword `k` if it is there, and says whether it was.
    pub(crate) fn eat_keyword(&mut self, k: Keyword) -> bool {
        let found = self.at_keyword(k);
        if found {
            self.advance();
        }
        found
    }

    /// Consumes the whole sequence `ks`, or nothing at all.
    ///
    /// All or nothing: `eat_keyword_seq(&[Group, By])` on `GROUP a` returns `false` and
    /// leaves the cursor on `GROUP`, ready for another rule to try.
    pub(crate) fn eat_keyword_seq(&mut self, ks: &[Keyword]) -> bool {
        let start = self.mark();
        for &k in ks {
            if !self.eat_keyword(k) {
                self.reset(start);
                return false;
            }
        }
        true
    }

    /// Consumes the keyword `k`, or fails without consuming anything.
    ///
    /// # Errors
    ///
    /// The syntax error of the token the cursor sits on, from
    /// [`crate::syntax_error::at_cursor`].
    pub(crate) fn expect_keyword(&mut self, k: Keyword) -> SqlResult<()> {
        if self.eat_keyword(k) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    /// Whether the cursor sits on the punctuation sign `p`.
    pub(crate) fn at_punct(&self, p: Punct) -> bool {
        matches!(self.peek().kind, TokenKind::Punct(found) if found == p)
    }

    /// Consumes the punctuation sign `p` if it is there, and says whether it was.
    pub(crate) fn eat_punct(&mut self, p: Punct) -> bool {
        let found = self.at_punct(p);
        if found {
            self.advance();
        }
        found
    }

    /// Consumes the punctuation sign `p`, or fails without consuming anything.
    ///
    /// # Errors
    ///
    /// The syntax error of the token the cursor sits on.
    pub(crate) fn expect_punct(&mut self, p: Punct) -> SqlResult<()> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    /// Reads one identifier: a regular or delimited name, or a **non-reserved** keyword.
    ///
    /// T-SQL only forbids the 184 reserved words as bare names, so `SELECT sum, name
    /// FROM t` is legal and this function accepts `sum`. An identifier built from a
    /// keyword has `quoted: false` and keeps the source spelling as its `value`.
    ///
    /// # A reserved keyword is refused, and that is not always what a rule wants
    ///
    /// Several constructs of T-SQL spell a name with a reserved word, and none of them
    /// may go through this function. They read the `TokenKind::Keyword` directly instead:
    ///
    /// - two-word type names -- `DOUBLE PRECISION`, `NATIONAL CHARACTER VARYING`,
    ///   `BINARY VARYING` (`DOUBLE`, `PRECISION`, `NATIONAL` and `VARYING` are reserved);
    /// - `SET` option names -- `TRANSACTION ISOLATION LEVEL`, `IDENTITY_INSERT`, ...
    ///   (`TRANSACTION`, `READ` and `IDENTITY_INSERT` are reserved);
    /// - function names -- `LEFT`, `RIGHT`, `CONVERT`, `USER`, `CURRENT_TIMESTAMP` are
    ///   all reserved, and all legal as the name of a call;
    /// - the `ALL` of `ALTER TABLE t CHECK CONSTRAINT ALL`.
    ///
    /// # Errors
    ///
    /// The syntax error of the token the cursor sits on, which stays where it is.
    pub(crate) fn parse_ident(&mut self) -> SqlResult<Ident> {
        let token = self.peek();
        let ident = match &token.kind {
            TokenKind::Ident { value, quoted } => Ident {
                value: value.clone(),
                quoted: *quoted,
            },
            TokenKind::Keyword(k) if !k.is_reserved() => Ident {
                value: token.text.clone(),
                quoted: false,
            },
            _ => return Err(self.error_here()),
        };
        self.advance();
        Ok(ident)
    }

    /// Reads a one- to four-part object name, `t` through `srv.db.dbo.t`.
    ///
    /// The parts fill **from the right**: one part is the object, two are schema and
    /// object, and so on. An empty part is legal in T-SQL and yields `None`, so `a..c`
    /// is database `a`, no schema, object `c`.
    ///
    /// # Errors
    ///
    /// The syntax error of the offending token -- a fifth part, or a dot followed by
    /// something that is not a name. The cursor is left where it started.
    pub(crate) fn parse_object_name(&mut self) -> SqlResult<ObjectName> {
        let start = self.mark();
        let mut parts: Vec<Option<Ident>> = vec![match self.parse_ident() {
            Ok(ident) => Some(ident),
            Err(error) => {
                self.reset(start);
                return Err(error);
            }
        }];
        while self.at_punct(Punct::Dot) {
            if parts.len() == 4 {
                // The dot that would open a fifth part is what SQL Server reports on.
                let error = self.error_here();
                self.reset(start);
                return Err(error);
            }
            self.eat_punct(Punct::Dot);
            if self.at_punct(Punct::Dot) {
                parts.push(None);
                continue;
            }
            match self.parse_ident() {
                Ok(ident) => parts.push(Some(ident)),
                Err(error) => {
                    self.reset(start);
                    return Err(error);
                }
            }
        }
        // The loop only pushes `None` for a part another dot follows, so the last part is
        // always a name.
        let Some(Some(name)) = parts.pop() else {
            self.reset(start);
            return Err(self.error_here());
        };
        let schema = parts.pop().flatten();
        let database = parts.pop().flatten();
        let server = parts.pop().flatten();
        Ok(ObjectName {
            server,
            database,
            schema,
            name,
            span: self.span_from(start),
        })
    }

    /// The current position, to hand back to [`Parser::reset`].
    pub(crate) fn mark(&self) -> usize {
        self.pos
    }

    /// Puts the cursor back where [`Parser::mark`] saw it, undoing every token consumed
    /// since.
    pub(crate) fn reset(&mut self, mark: usize) {
        self.pos = mark;
    }

    /// The span running from the token at `mark` to the last token consumed.
    ///
    /// `line` and `column` are those of the token at `mark`; `len` counts the bytes from
    /// the start of that token to the end of the last one consumed, comments and
    /// whitespace in between included. When nothing has been consumed since `mark`, the
    /// span is empty at that position.
    pub(crate) fn span_from(&self, mark: usize) -> Span {
        let start = self.token_at(mark).span;
        if self.pos <= mark {
            return Span { len: 0, ..start };
        }
        let last = self.token_at(self.pos - 1).span;
        let end = last.offset.saturating_add(last.len);
        Span {
            len: end.saturating_sub(start.offset),
            ..start
        }
    }

    /// The syntax error to report on the token the cursor sits on.
    ///
    /// Choosing between 102 and 156, framing the printed text and telling a refused token
    /// from a text that simply stopped all belong to [`crate::syntax_error::at_cursor`],
    /// each rule of the module goes through this one method so that there is a single
    /// place to change. The position matters as much as the token, which is why the whole
    /// cursor is handed over: the cursor is the one place that still has the tokens read
    /// before it.
    pub(crate) fn error_here(&self) -> SqlError {
        syntax_error::at_cursor(&self.tokens, self.pos)
    }

    /// The session settings the batch is parsed under.
    #[allow(dead_code)] // the `SET` options of a session are read here
    pub(crate) fn opts(&self) -> &ParseOptions {
        self.opts
    }
}

#[cfg(test)]
mod tests;
