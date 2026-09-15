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
    while !p.at_eof() {
        if p.eat_punct(Punct::Semicolon) {
            continue;
        }
        // `first_in_batch`: the flag the implicit `EXEC` rule reads.
        statements.push(stmt::parse_statement(&mut p, statements.is_empty())?);
        // A `;` after a statement is optional: it is eaten when it is there.
        let _ = p.eat_punct(Punct::Semicolon);
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
        })
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
mod tests {
    use vauban_errors::SqlResult;

    use super::{MAX_NESTING_DEPTH, ParseOptions, Parser};
    use crate::keyword::Keyword;
    use crate::token::{Punct, TokenKind};

    /// Builds a cursor over `text` with the default options.
    fn cursor(text: &str) -> Parser<'static> {
        // `OPTIONS` is a `static`, so the borrow lives as long as the test needs it.
        static OPTIONS: ParseOptions = ParseOptions {
            quoted_identifier: true,
        };
        match Parser::new(text, &OPTIONS) {
            Ok(parser) => parser,
            Err(error) => unreachable!("the test text lexes: {error:?}"),
        }
    }

    #[test]
    fn parse_options_default() {
        assert!(ParseOptions::default().quoted_identifier);
    }

    #[test]
    fn cursor_walks_tokens() {
        let mut p = cursor("SELECT 1");
        assert_eq!(p.peek().kind, TokenKind::Keyword(Keyword::Select));
        assert_eq!(p.advance().kind, TokenKind::Keyword(Keyword::Select));
        assert_eq!(p.peek().kind, TokenKind::Integer);
        assert_eq!(p.advance().kind, TokenKind::Integer);
        assert_eq!(p.peek().kind, TokenKind::Eof);
        assert!(p.at_eof());
        // `Eof` is handed out again and again, and the index stays in bounds.
        let at_end = p.mark();
        assert_eq!(p.advance().kind, TokenKind::Eof);
        assert_eq!(p.advance().kind, TokenKind::Eof);
        assert_eq!(p.mark(), at_end);
        assert_eq!(p.peek().kind, TokenKind::Eof);
    }

    #[test]
    fn cursor_peek_at_reads_ahead() {
        let p = cursor("SELECT 1");
        assert_eq!(p.peek_at(0).kind, TokenKind::Keyword(Keyword::Select));
        assert_eq!(p.peek_at(1).kind, TokenKind::Integer);
        assert_eq!(p.peek_at(2).kind, TokenKind::Eof);
        // Far past the end, and past `usize::MAX` too.
        assert_eq!(p.peek_at(99).kind, TokenKind::Eof);
        assert_eq!(p.peek_at(usize::MAX).kind, TokenKind::Eof);
        assert_eq!(p.mark(), 0);
    }

    #[test]
    fn cursor_eat_keyword() {
        let mut p = cursor("SELECT 1");
        assert!(p.eat_keyword(Keyword::Select));
        assert_eq!(p.mark(), 1);
        assert!(!p.eat_keyword(Keyword::From));
        assert_eq!(p.mark(), 1);
    }

    #[test]
    fn cursor_eat_punct() {
        let mut p = cursor("(1)");
        assert!(!p.eat_punct(Punct::RightParen));
        assert!(p.eat_punct(Punct::LeftParen));
        assert_eq!(p.mark(), 1);
    }

    #[test]
    fn cursor_expect_keyword_errors() {
        let mut p = cursor("SELECT 1");
        p.advance();
        // The cursor sits on `1`, which is not a reserved word: this stays a 102, where
        // the same test on `SELECT` gives a 156.
        let error = match p.expect_keyword(Keyword::From) {
            Ok(()) => unreachable!("there is no FROM here"),
            Err(error) => error,
        };
        assert_eq!(error.number, 102);
        assert_eq!(error.line, 1);
        assert_eq!(p.mark(), 1);
    }

    #[test]
    fn cursor_expect_punct_errors() {
        let mut p = cursor("1");
        let error = match p.expect_punct(Punct::Comma) {
            Ok(()) => unreachable!("there is no comma here"),
            Err(error) => error,
        };
        assert_eq!(error.number, 102);
        assert_eq!(p.mark(), 0);
    }

    #[test]
    fn cursor_eat_seq() {
        let mut p = cursor("GROUP BY a");
        assert!(p.eat_keyword_seq(&[Keyword::Group, Keyword::By]));
        assert_eq!(p.mark(), 2);

        let mut p = cursor("GROUP a");
        assert!(!p.eat_keyword_seq(&[Keyword::Group, Keyword::By]));
        // The `GROUP` already eaten is given back.
        assert_eq!(p.mark(), 0);
        assert!(p.at_keyword(Keyword::Group));
    }

    #[test]
    fn cursor_parse_ident_accepts_non_reserved_keyword() {
        let ident = match cursor("sum").parse_ident() {
            Ok(ident) => ident,
            Err(error) => unreachable!("SUM is not reserved: {error:?}"),
        };
        assert_eq!(ident.value, "sum");
        assert!(!ident.quoted);

        let mut p = cursor("select");
        let error = match p.parse_ident() {
            Ok(ident) => unreachable!("SELECT is reserved, got {ident:?}"),
            Err(error) => error,
        };
        // SQL Server: `USE select` => 156, exercising a reserved word as an identifier.
        assert_eq!(error.number, 156);
        assert_eq!(p.mark(), 0);

        let ident = match cursor("[select]").parse_ident() {
            Ok(ident) => ident,
            Err(error) => unreachable!("a delimited name may be any word: {error:?}"),
        };
        assert_eq!(ident.value, "select");
        assert!(ident.quoted);
    }

    #[test]
    fn cursor_parse_object_name() {
        let parts = |text: &str| {
            let name = match cursor(text).parse_object_name() {
                Ok(name) => name,
                Err(error) => unreachable!("{text} is a valid name: {error:?}"),
            };
            let part = |ident: Option<crate::ast::expr::Ident>| ident.map(|i| i.value);
            (
                part(name.server),
                part(name.database),
                part(name.schema),
                name.name.value,
            )
        };
        assert_eq!(parts("a"), (None, None, None, "a".to_owned()));
        assert_eq!(
            parts("a.b"),
            (None, None, Some("a".to_owned()), "b".to_owned())
        );
        assert_eq!(
            parts("a.b.c"),
            (
                None,
                Some("a".to_owned()),
                Some("b".to_owned()),
                "c".to_owned()
            )
        );
        assert_eq!(
            parts("a.b.c.d"),
            (
                Some("a".to_owned()),
                Some("b".to_owned()),
                Some("c".to_owned()),
                "d".to_owned()
            )
        );
        // An empty part is legal T-SQL: `a..c` is a database and an object, no schema.
        assert_eq!(
            parts("a..c"),
            (None, Some("a".to_owned()), None, "c".to_owned())
        );

        let mut p = cursor("a.b.c.d.e");
        let error = match p.parse_object_name() {
            Ok(name) => unreachable!("five parts is too many, got {name:?}"),
            Err(error) => error,
        };
        assert_eq!(error.number, 102);
        assert_eq!(p.mark(), 0);
    }

    #[test]
    fn cursor_span_from() {
        let mut p = cursor("SELECT 1");
        let start = p.mark();
        // Nothing consumed yet: an empty span on the first token.
        assert_eq!(p.span_from(start).len, 0);
        p.advance();
        p.advance();
        let span = p.span_from(start);
        assert_eq!(span.line, 1);
        assert_eq!(span.column, 1);
        assert_eq!(span.offset, 0);
        assert_eq!(span.len, 8);

        // From the middle: the position is the one of the first token consumed.
        let mut p = cursor("SELECT\n  a + 1");
        p.advance();
        let start = p.mark();
        p.advance();
        p.advance();
        p.advance();
        let span = p.span_from(start);
        assert_eq!(span.line, 2);
        assert_eq!(span.column, 3);
        assert_eq!(span.offset, 9);
        assert_eq!(span.len, 5);
    }

    #[test]
    fn cursor_reset_undoes_everything() {
        let mut p = cursor("SELECT 1");
        let start = p.mark();
        p.advance();
        p.advance();
        p.reset(start);
        assert_eq!(p.peek().kind, TokenKind::Keyword(Keyword::Select));
    }

    #[test]
    fn cursor_opts_are_the_ones_given() {
        assert!(cursor("SELECT 1").opts().quoted_identifier);
    }

    #[test]
    fn cursor_reports_an_unknown_character() {
        // The lexer hands `\` over as `Unknown`; the cursor turns it into a 102.
        let p = cursor("\\");
        assert_eq!(p.peek().kind, TokenKind::Unknown);
        let error = p.error_here();
        assert_eq!(error.number, 102);
        assert_eq!(error.message, "Syntax error near '\\'.");
    }

    /// Recurses through [`Parser::nested`] `levels` times, counting how far it got.
    ///
    /// The closure is the shape every guarded rule has, so what it measures is what the
    /// grammar gets. The batch is irrelevant: only the counter is under test here.
    fn descend(p: &mut Parser, levels: u32, reached: &mut u32) -> SqlResult<()> {
        p.nested(|p| {
            *reached += 1;
            if levels == 0 {
                Ok(())
            } else {
                descend(p, levels - 1, reached)
            }
        })
    }

    #[test]
    fn nesting_is_allowed_up_to_the_limit() {
        let mut p = cursor("SELECT 1");
        let mut reached = 0;
        let outcome = descend(&mut p, MAX_NESTING_DEPTH - 1, &mut reached);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(reached, MAX_NESTING_DEPTH);
    }

    #[test]
    fn nesting_one_level_past_the_limit_is_error_191() {
        let mut p = cursor("SELECT 1");
        let mut reached = 0;
        let Err(error) = descend(&mut p, MAX_NESTING_DEPTH, &mut reached) else {
            unreachable!("one level past the limit must be refused");
        };
        // The rule at the limit ran; the one below it never started.
        assert_eq!(reached, MAX_NESTING_DEPTH);
        assert_eq!(error.number, 191);
        assert_eq!(error.severity, 15);
        assert_eq!(error.state, 1);
        assert_eq!(
            error.message,
            "The statement is nested too deeply; split it into smaller queries."
        );
    }

    #[test]
    fn the_counter_comes_back_down_after_a_failure() {
        let mut p = cursor("SELECT 1");
        let mut wasted = 0;
        assert!(descend(&mut p, MAX_NESTING_DEPTH, &mut wasted).is_err());
        assert_eq!(p.depth, 0, "a refused descent left levels behind");
        // And the cursor is good for another full descent, which a leak would refuse.
        let mut reached = 0;
        assert!(descend(&mut p, MAX_NESTING_DEPTH - 1, &mut reached).is_ok());
    }

    #[test]
    fn the_191_carries_the_line_the_cursor_is_on() {
        let mut p = cursor("SELECT\n1");
        p.advance();
        assert_eq!(p.nested_too_deeply().line, 2);
    }
}
