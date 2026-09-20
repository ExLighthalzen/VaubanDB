//! Geometry of one statement inside the text that carries it: its span, the source
//! fragment it owns, the line it starts on, the line the name of a `USE` starts on, and
//! the line an error leaves with.
//!
//! The three texts a session binds — the batch it receives, the nested text of a
//! procedure-style call, and the text of a prepared handle — read their statements the
//! same way, so the answers live here once (`statements/tests/mod.rs`).

use vauban_errors::SqlError;
use vauban_parser::Statement;
use vauban_planner::PhysicalStatement;

#[cfg(test)]
mod tests;

/// Number of the generic internal error (`errors`, `InternalError` → `SqlError`): the
/// binder answers it for each statement that is not implemented yet.
pub(crate) const INTERNAL_ERROR: u32 = 50000;

/// The numbers that come out of `executor::execute` with a line of their own to keep.
///
/// 127 (`TOP` row count below zero) and 1060 (`TOP` row count `NULL`) are the two checks
/// the executor performs whose line SQL Server counts on the **row-count expression** of
/// the `TOP` rather than on the statement (module header of `batch.rs`). `executor` already
/// puts that line on them, so [`at_statement`] steps aside.
///
/// These two are not a class: 1062 and 1031, raised while the very same `TOP` clause is
/// compiled, answer the last line of the statement and the first line of the select list
/// respectively on SQL Server, and neither of those is the row count (module header of
/// `batch.rs`).
pub(crate) const COMPILED_WITH_THE_STATEMENT: [u32; 2] = [127, 1060];

/// Number of bytes of the `USE` keyword, the first token of the statement
/// [`use_name_line`] walks.
pub(crate) const USE_KEYWORD_LEN: usize = "USE".len();

/// Whether a successful statement resets `@@ERROR` to `0`. A bare `DECLARE` with no
/// initializer on any item leaves the previous error in place
/// (`tests/session_variables.rs`, `last_error_is_cleared_by_a_successful_statement`).
pub(crate) fn statement_clears_last_error(stmt: &PhysicalStatement) -> bool {
    match stmt {
        PhysicalStatement::Declare(declarations) => declarations
            .iter()
            .all(|declaration| declaration.value.is_some()),
        _ => true,
    }
}

/// The 1-based line the statement starts on, in the text that carries it.
///
/// [`statement_span`] has one arm per variant of the parser, and the line of that span is
/// the answer here for each variant but `EXEC`, which has its own section below. One rule
/// for the three texts a session binds: the batch it receives, the nested text of a
/// parameterized call, and the text of a prepared handle.
///
/// # What the line is for
///
/// - The fragment of the statement is padded with `line - 1` newlines before it is
///   rebound, so a **binding** error lands on the line the client wrote the statement on
///   rather than on line 1. Pinned end to end by
///   `a_binding_error_of_a_declare_carries_the_line_of_its_statement` in `tests/batch.rs`,
///   `tests/nested.rs` and `tests/prepared.rs`, and by
///   `a_binding_error_of_a_print_carries_the_line_of_its_statement` in `tests/batch.rs`
///   and `tests/prepared.rs`.
/// - [`at_statement`] puts the line on a **run-time** error, over the line of the
///   sub-expression the executor answered (`a_run_time_ddl_error_carries_the_line_of_its_statement`).
///
/// # The statement's line, not the first line of the text and not the failing node's
///
/// The shapes below tell the readings apart. The first two put the failing node below the
/// first line of the statement, so "the statement's line" and "the node's line" differ;
/// the third leaves a blank line before the statement, so "line 1", "the rank of the
/// statement in the text" and "the line it starts on" answer 1, 2 and 3.
///
/// | text | statement | failing node | answer |
/// |---|:-:|:-:|:-:|
/// | `SELECT 1;` / `DROP TABLE` / `  dbo.t;` | 2 | 3 | **2** (3701) |
/// | `SELECT 1;` / `CREATE TABLE dbo.w` / `  (a nosuchtype);` | 2 | 3 | **2** (2715) |
/// | `SELECT 1;` / `` / `PRINT @v;` | 3 | 3 | **3** (137) |
///
/// `USE` is the statement whose line the client does not read: 911 leaves on the line of
/// the **name** ([`use_name_line`]), and what the line of the statement buys there is the
/// line of the INFO 5701 of a `USE` that works, which is the statement's (rustdoc of
/// `Session::switch_database`).
///
/// A statement that holds another — `IF`, `WHILE`, `BEGIN`/`END` — answers the line of
/// its own first token, not the line of the statement it runs
/// (`statements/tests/mod.rs`).
///
/// # `EXEC` is the variant with no line to offer
///
/// An `EXEC` of a procedure answers 0, the "no line to offer" of [`at_statement`], so an
/// error raised by the call leaves on the line the dispatch put on it: an `EXEC` of a
/// name no procedure carries answers 2812 on the first line of the batch, and not on the
/// line the `EXEC` sits on (`tests/batch.rs`,
/// `an_exec_of_an_unknown_procedure_answers_on_the_first_line`;
/// `statements/tests/mod.rs`, `statement_line_of_an_exec_is_zero`).
///
/// The line of the statement would be the closer answer for that one, and the wrong one
/// for the rest: what a call raises about its own arguments — a parameter left without a
/// value, a declaration that does not parse — is located in the parameterized text, whose
/// first line is where the client reads it, and forcing the line of the `EXEC` on it
/// would move it off that line.
pub(crate) fn statement_line(stmt: &Statement) -> u32 {
    match stmt {
        Statement::Execute(_) => 0,
        _ => statement_span(stmt).line,
    }
}

/// The 1-based line the **name** of a `USE` statement starts on, in `text`, which is the
/// line SQL Server puts on the 911 of a database that is not there.
///
/// `Ident` carries no span (`parser`, `ast/expr.rs`), so the line is counted here, from the
/// span of the statement: past the `USE` keyword, then through the trivia — spaces, line
/// comments, block comments, nested or not — up to the first byte of the name.
///
/// # The rule, on the shapes that separate it from its neighbours
///
/// Each shape puts the name on a different line from the statement, so "the name's line"
/// and "the statement's line" answer differently; the first also separates it from "the
/// line the statement ends on":
///
/// | batch | statement | name | end | answer |
/// |---|:-:|:-:|:-:|:-:|
/// | `SELECT 1;` / `USE` / `  d` / `;` | 2 | 3 | 4 | **3** |
/// | `SELECT 1;` / `USE /* c` / `  c */ d;` | 2 | 3 | 3 | **3** |
/// | `SELECT 1;` / `USE` / `-- a comment` / `  d;` | 2 | 4 | 4 | **4** |
/// | `SELECT 1;` / `USE [d` / `e];` | 2 | 2 | 3 | **2** |
/// | `SELECT 1;` / `USE /* a /* b` / `b */ a */` / `  d;` | 2 | 4 | 4 | **4** |
/// | `SELECT 1;` / `USE` / `` / `` / `  d;` | 2 | 5 | 5 | **5** |
/// | `USE d;` | 1 | 1 | 1 | **1** |
///
/// The fourth shape is the one that says the answer is where the name **starts** and not
/// where it ends: a delimited name spread over two lines answers the line of its `[`. The
/// fifth is why the depth of a block comment is counted rather than its first `*/`: a
/// scan that stopped at `b */` would have answered 3. The last is the witness, where the
/// three hypotheses agree. Pinned by
/// `use_name_line_is_the_line_the_name_starts_on` (`statements/tests/mod.rs`) and, end to
/// end, `use_of_an_unknown_database_is_911_on_the_line_of_the_name`.
pub(crate) fn use_name_line(text: &str, stmt: &Statement) -> u32 {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    let Some(body) = text
        .get(start..start + span.len as usize)
        .and_then(|body| body.get(USE_KEYWORD_LEN..))
    else {
        return span.line;
    };
    let bytes = body.as_bytes();
    let mut line = span.line;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\n' => {
                line += 1;
                at += 1;
            }
            byte if byte.is_ascii_whitespace() => at += 1,
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                // A line comment runs to the end of its line; the newline itself is counted
                // by the arm above, on the next turn.
                at += 2;
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                // Block comments nest in T-SQL, as the parser reads them (module header of
                // `parser`, `lexer.rs`): the depth is counted, not the first `*/`.
                let mut depth = 1usize;
                at += 2;
                while at < bytes.len() && depth > 0 {
                    match bytes[at] {
                        b'\n' => line += 1,
                        b'/' if bytes.get(at + 1) == Some(&b'*') => {
                            depth += 1;
                            at += 1;
                        }
                        b'*' if bytes.get(at + 1) == Some(&b'/') => {
                            depth -= 1;
                            at += 1;
                        }
                        _ => {}
                    }
                    at += 1;
                }
            }
            // The first byte that is neither space nor comment opens the name.
            _ => return line,
        }
    }
    line
}

/// Puts the line of the failing statement on a run-time error, over the line of the
/// sub-expression the executor put there.
///
/// Overriding rather than filling is the point: the executor answers a line for every
/// error it raises, and that line is the right one only when the statement fits on one
/// line (`statements/tests/mod.rs`). Three errors are left as they are:
///
/// - the internal error 50000, which names a hole in this engine and not a place in the
///   query (`executor`, `errors.rs`, keeps it at line 0);
/// - 127 and 1060, the two compilation checks the executor performs late, whose line SQL
///   Server counts on the `TOP` row count and not on the statement
///   ([`COMPILED_WITH_THE_STATEMENT`]);
/// - any error at all when `line` is 0, which means the caller has no line to offer.
pub(crate) fn at_statement(err: SqlError, line: u32) -> SqlError {
    if line == 0
        || err.number == INTERNAL_ERROR
        || COMPILED_WITH_THE_STATEMENT.contains(&err.number)
    {
        return err;
    }
    err.with_line(line)
}

/// The text of `stmt` as the client wrote it, cut out of the batch by the span of the
/// statement, or re-rendered from the AST when the variant carries no span this function
/// knows.
///
/// Only the two variants that need their text have a case here: a `SET` option, which
/// `set_options.rs` recognises from the text, and `WAITFOR`, which the fake engine still serves.
/// `Display` is the fall-back rather than an error because it round-trips through the
/// parser (`parser`, `display`), so a statement rendered this way is still the same one.
pub(crate) fn statement_text(batch: &str, stmt: &Statement) -> String {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    batch
        .get(start..start + span.len as usize)
        .map_or_else(|| stmt.to_string(), str::to_owned)
}

/// Exact source from this statement through its separator and trailing trivia.
/// Keeping that suffix preserves errors which SQL Server locates on a semicolon below the
/// expression rather than on the expression itself.
pub(crate) fn statement_fragment<'a>(
    batch: &'a str,
    stmt: &Statement,
    next: Option<&Statement>,
) -> &'a str {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    let end = next
        .map(statement_span)
        .map_or(batch.len(), |next_span| next_span.offset as usize);
    batch.get(start..end).unwrap_or(batch)
}

/// Position of a statement, one arm per variant of the parser. Keeping this match
/// exhaustive makes a new parser construct break compilation instead of silently
/// rebuilding the whole batch as one statement.
pub(crate) fn statement_span(stmt: &Statement) -> vauban_parser::Span {
    match stmt {
        Statement::Select(statement) => statement.span,
        Statement::Insert(statement) => statement.span,
        Statement::Update(statement) => statement.span,
        Statement::Delete(statement) => statement.span,
        Statement::Merge(statement) => statement.span,
        Statement::CreateDatabase(statement) => statement.span,
        Statement::AlterDatabase(statement) => statement.span,
        Statement::CreateTable(statement) => statement.span,
        Statement::AlterTable(statement) => statement.span,
        Statement::CreateIndex(statement) => statement.span,
        Statement::DropIndex(statement) => statement.span,
        Statement::CreateProcedure(statement) => statement.span,
        Statement::CreateFunction(statement) => statement.span,
        Statement::CreateView(statement) => statement.span,
        Statement::CreateTrigger(statement) => statement.span,
        Statement::CreateSequence(statement) => statement.span,
        Statement::Declare(statement) => statement.span,
        Statement::Set(statement) => statement.span,
        Statement::SetOption(statement) => statement.span,
        Statement::Execute(statement) => statement.span,
        Statement::Waitfor(statement) => statement.span,
        Statement::RaiseError(statement) => statement.span,
        Statement::Cursor(statement) => statement.span,
        Statement::Grant(statement) => statement.span,
        Statement::Break(span) | Statement::Continue(span) => *span,
        Statement::Truncate { span, .. }
        | Statement::DropDatabase { span, .. }
        | Statement::Use { span, .. }
        | Statement::DropTable { span, .. }
        | Statement::DropProcedure { span, .. }
        | Statement::DropFunction { span, .. }
        | Statement::DropView { span, .. }
        | Statement::DropTrigger { span, .. }
        | Statement::DropSequence { span, .. }
        | Statement::If { span, .. }
        | Statement::While { span, .. }
        | Statement::Block { span, .. }
        | Statement::Return { span, .. }
        | Statement::Print { span, .. }
        | Statement::BeginTransaction { span, .. }
        | Statement::Commit { span, .. }
        | Statement::Rollback { span, .. }
        | Statement::Save { span, .. }
        | Statement::TryCatch { span, .. }
        | Statement::Throw { span, .. }
        | Statement::Goto { span, .. }
        | Statement::Label { span, .. } => *span,
    }
}
