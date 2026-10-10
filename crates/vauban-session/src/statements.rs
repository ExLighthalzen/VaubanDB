//! Geometry of one statement inside the text that carries it: its span, the source
//! fragment it owns, the line it starts on, the line the name of a `USE` starts on, and
//! the line an error leaves with. The labels of a text and the `GOTO` statements that
//! name them are read here too: both are positions in the list of statements.
//!
//! The three texts a session binds — the batch it receives, the nested text of a
//! procedure-style call, and the text of a prepared handle — read their statements the
//! same way, so the answers live here once (`statements/tests/mod.rs`).

use std::collections::HashMap;

use vauban_errors::SqlError;
use vauban_executor::ExecSession;
use vauban_parser::Statement;
use vauban_planner::PhysicalStatement;
use vauban_types::Value;

#[cfg(test)]
mod tests;

/// Number of the generic internal error (`errors`, `InternalError` → `SqlError`): the
/// binder answers it for each statement that is not implemented yet.
pub(crate) const INTERNAL_ERROR: u32 = 50000;

/// Number of the error the binder answers for a table, view or object the catalogue does
/// not hold (`errors`, `SqlError::invalid_object_name`). It is the one number a batch does
/// not treat as a compilation error: the statement that raised it is bound again when the
/// batch reaches it (`batch.rs`, `PreparedStatement::Deferred`).
pub(crate) const INVALID_OBJECT_NAME: u32 = 208;

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
///
/// A `GOTO` and a label leave it in place too: `SELECT 1 / 0;` followed by a label and a
/// `SELECT @@ERROR;` answers 8134, and so does the same with a `GOTO` between the two
/// (`tests/run_batch_pipeline.rs`, `a_goto_and_a_label_leave_the_last_error_alone`). A
/// `PRINT` in their place clears it, which is what tells the rule from "nothing clears it
/// here" (`a_print_between_the_two_clears_it`, same file).
pub(crate) fn statement_clears_last_error(stmt: &PhysicalStatement) -> bool {
    match stmt {
        PhysicalStatement::Declare(declarations) => declarations
            .iter()
            .all(|declaration| declaration.value.is_some()),
        PhysicalStatement::Goto { .. } | PhysicalStatement::Label { .. } => false,
        _ => true,
    }
}

/// `@@ROWCOUNT` after a top-level statement that answered `ExecOutcome::NoRows`, given
/// what it held just before the statement ran (`previous`) and what nested execution left
/// in `ExecSession::rowcount` (`from_exec`).
///
/// A `DECLARE` that gives at least one of its variables an initial value is an assignment,
/// like `SET`: `DECLARE @a int = 1, @b int = 2; SELECT @@ROWCOUNT;` answers `1`, not `2`
/// (`tests/run_batch_pipeline.rs`, `declare_with_an_initial_value_sets_rowcount_to_one`). A
/// `DECLARE` with no initializer on any variable is not an assignment at all: it leaves
/// `previous` untouched (`declare_without_a_value_leaves_rowcount_alone`), unlike every
/// other statement here, which starts its own count at `0`.
///
/// A `WHILE` ends by evaluating its condition once more and finding it false, which is
/// itself a no-op statement: `0`, over `from_exec`, be it still at the `0` it started the
/// loop on or moved by a `SET` inside the last iteration of the body
/// (`tests/run_batch_pipeline.rs`, `while_ends_with_rowcount_at_zero`).
///
/// A `SELECT` that assigns from a `FROM` posts the number of rows its source produced
/// into `ExecSession::rowcount` and answers `NoRows`, so it needs no rule of its own here:
/// reading `from_exec` gives that number, and `0` over a source that produced nothing
/// (`tests/run_batch_pipeline.rs`, `select_assign_from_rows_sets_rowcount_to_the_rows_walked`
/// and `select_assign_over_no_row_sets_rowcount_to_zero`).
///
/// A `GOTO` and a label count no row and leave `previous` where it was, as a bare
/// `DECLARE` does: a `SELECT` of two rows followed by a label and a `SELECT @@ROWCOUNT;`
/// answers `2`, and a `SELECT` of three rows read through a `GOTO` answers `3`
/// (`tests/run_batch_pipeline.rs`, `a_goto_and_a_label_leave_the_row_count_alone`). A
/// `PRINT` written in their place answers `0`, which separates this rule from "the count
/// survives anything" (`a_print_between_the_two_puts_the_row_count_back_to_zero`).
///
/// Any other statement reads `from_exec` unchanged: `0` unless a nested `SET` changed it,
/// the shape this already gives `IF` on its own (`ExecSession::rowcount` starts at `0` for
/// the statement as a whole, so a false condition and a taken branch that assigns nothing
/// both read back as `0`, and a taken branch that assigns behaves like the same code
/// written outside the `IF`).
pub(crate) fn next_rowcount(stmt: &PhysicalStatement, previous: i64, from_exec: i64) -> i64 {
    match stmt {
        PhysicalStatement::Declare(declarations) => {
            if declarations
                .iter()
                .any(|declaration| declaration.value.is_some())
            {
                1
            } else {
                previous
            }
        }
        PhysicalStatement::While { .. } => 0,
        PhysicalStatement::Goto { .. } | PhysicalStatement::Label { .. } => previous,
        _ => from_exec,
    }
}

/// The 1-based line the statement starts on, in the text that carries it.
///
/// [`statement_span`] has one arm per variant of the parser, and the line of that span is
/// the answer here; the `EXEC` section below says what the answer buys there. One rule
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
/// # `EXEC` answers the line it starts on, and the call sorts out what keeps it
///
/// An `EXEC` answers the line of its own first token, like the other variants: a name no
/// procedure carries answers 2812 on the line the `EXEC` sits on, and so does an argument
/// list the binder refuses (`tests/batch.rs`,
/// `an_exec_of_an_unknown_procedure_answers_on_the_line_it_sits_on`;
/// `statements/tests/mod.rs`, `statement_line_of_an_exec_is_the_line_it_starts_on`).
///
/// That line is the wrong one for what the **call** raises about its arguments — a
/// parameter left without a value, a declaration that does not parse — which is located
/// in the parameterized text, whose first line is where the client reads it. The dispatch
/// keeps those where they are rather than stamping this line on them (`procedures.rs`,
/// `at_call_line`).
pub(crate) fn statement_line(stmt: &Statement) -> u32 {
    statement_span(stmt).line
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

/// The line the **first statement held** by `stmt` starts on, for the three statements
/// that hold statements, and `None` for the others.
///
/// What it is read for: an error the executor lines at or after that line keeps that
/// line, it was raised by a statement the `IF`, the `WHILE` or the `BEGIN … END` holds; an
/// error of the **condition** arrives with no line of its own (the host of the batch clears
/// it, `batch.rs`, `BatchHost::condition`), so [`at_statement`] puts
/// the enclosing statement's line on it, a condition folded onto the line of its first
/// held statement included. Both halves are fixed by `tests/run_batch_pipeline.rs`,
/// `an_error_of_a_condition_carries_the_line_of_the_statement_that_holds_it` and
/// `an_error_of_a_branch_keeps_the_line_the_executor_gave_it`.
///
/// The line kept is the line of the failing sub-expression, which parts company with the
/// line the statement starts on as soon as the statement spans two: `IF 1 = 1` followed by
/// a `SELECT` on one line and its `1 / 0` on the next answers the line of the `1 / 0`
/// (`a_branch_statement_over_two_lines_answers_the_line_of_the_fault`).
pub(crate) fn first_held_statement_line(stmt: &Statement) -> Option<u32> {
    match stmt {
        Statement::If { then_branch, .. } => Some(statement_span(then_branch).line),
        Statement::While { body, .. } => Some(statement_span(body).line),
        Statement::Block { statements, .. } => {
            statements.first().map(|first| statement_span(first).line)
        }
        _ => None,
    }
}

/// Enters the variables of `stmt` with no value when `stmt` is a `DECLARE`, leaving a name
/// already entered as it stands; does nothing for any other statement.
///
/// What it is for: a variable of a text belongs to the whole text, not to the place its
/// `DECLARE` is written, so a statement that reads it after a `GOTO` jumped over that
/// `DECLARE` reads `NULL` and may be assigned to (`tests/run_batch_pipeline.rs`,
/// `a_goto_over_a_declare_leaves_the_variable_at_null` and
/// `a_variable_whose_declare_was_jumped_over_can_still_be_assigned`, and, in a batch bound
/// one statement at a time because it holds an `ALTER TABLE`,
/// `a_goto_over_a_declare_in_a_batch_with_an_alter_table_still_declares_it`). The jump
/// enters the names it stepped over once they are bound; running the `DECLARE` itself is
/// what gives them their initial value, which is why a name already entered keeps what it
/// holds.
pub(crate) fn declare_variables_of(stmt: &PhysicalStatement, exec: &mut ExecSession) {
    let PhysicalStatement::Declare(declarations) = stmt else {
        return;
    };
    for declaration in declarations {
        exec.variables
            .entry(declaration.name.clone())
            .or_insert(Value::Null);
        exec.variable_types
            .entry(declaration.name.clone())
            .or_insert_with(|| declaration.ty.clone());
    }
}

/// [`declare_variables_of`] for `stmt` and for each statement it holds: the `DECLARE`s a
/// branch, a loop body or a block writes belong to the whole text as well, whether the
/// program reaches them or jumps over them.
pub(crate) fn declare_variables_held_by(stmt: &PhysicalStatement, exec: &mut ExecSession) {
    match stmt {
        PhysicalStatement::If { then_, else_, .. } => {
            declare_variables_held_by(then_, exec);
            if let Some(branch) = else_ {
                declare_variables_held_by(branch, exec);
            }
        }
        PhysicalStatement::While { body, .. } => declare_variables_held_by(body, exec),
        PhysicalStatement::Block(statements) => {
            for held in statements {
                declare_variables_held_by(held, exec);
            }
        }
        other => declare_variables_of(other, exec),
    }
}

/// The labels `statements` declares at any depth, from the name folded to lower case to the
/// index of the statement of the list that declares it or holds it: the labels a `GOTO` of
/// the batch may land on, since the program that runs the batch holds a position inside
/// an `IF`, a `WHILE` and a `BEGIN … END` (`tests/run_batch_pipeline.rs`,
/// `a_goto_into_a_block_of_the_batch_runs_the_block_from_the_label`).
pub(crate) fn labels_at_any_depth(statements: &[Statement]) -> HashMap<String, usize> {
    let mut labels = HashMap::new();
    for (index, stmt) in statements.iter().enumerate() {
        enter_labels(stmt, index, &mut labels);
    }
    labels
}

/// [`labels_at_any_depth`] for one statement and the statements it holds.
fn enter_labels(stmt: &Statement, index: usize, labels: &mut HashMap<String, usize>) {
    match stmt {
        Statement::Label { name, .. } => {
            labels
                .entry(name.value.to_ascii_lowercase())
                .or_insert(index);
        }
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            enter_labels(then_branch, index, labels);
            if let Some(branch) = else_branch {
                enter_labels(branch, index, labels);
            }
        }
        Statement::While { body, .. } => enter_labels(body, index, labels),
        Statement::Block { statements, .. } => {
            for held in statements {
                enter_labels(held, index, labels);
            }
        }
        // The other statements hold no statement, so they declare no label either.
        _ => {}
    }
}

/// The name written by the first `GOTO` of `statements` that names a label `labels` does
/// not hold, in written order, `None` when each `GOTO` names a label of the table.
///
/// The `GOTO` written inside an `IF`, a `WHILE` or a `BEGIN … END` is looked at too: its
/// jump leaves the statement that holds it, so it is checked against the same table
/// (`tests/run_batch_pipeline.rs`, `a_goto_in_a_branch_that_is_not_taken_is_still_133`).
pub(crate) fn first_undeclared_goto<'a>(
    statements: &'a [Statement],
    labels: &HashMap<String, usize>,
) -> Option<&'a str> {
    statements
        .iter()
        .find_map(|stmt| undeclared_goto(stmt, labels))
}

/// [`first_undeclared_goto`] for one statement and the statements it holds.
fn undeclared_goto<'a>(stmt: &'a Statement, labels: &HashMap<String, usize>) -> Option<&'a str> {
    match stmt {
        Statement::Goto { label, .. } => (!labels.contains_key(&label.value.to_ascii_lowercase()))
            .then_some(label.value.as_str()),
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => undeclared_goto(then_branch, labels).or_else(|| {
            else_branch
                .as_deref()
                .and_then(|branch| undeclared_goto(branch, labels))
        }),
        Statement::While { body, .. } => undeclared_goto(body, labels),
        Statement::Block { statements, .. } => first_undeclared_goto(statements, labels),
        // The other statements hold no statement, so they hold no `GOTO` either.
        _ => None,
    }
}

/// The 1-based number of the last line of `text`, which is the line 133 leaves on.
///
/// Neither the line of the `GOTO` nor the line of the last statement: a trailing blank
/// line and a trailing comment both push the answer past the last statement, which is
/// what tells the three readings apart.
///
/// | text | `GOTO` | last statement | answer |
/// |---|:-:|:-:|:-:|
/// | `GOTO nowhere;` / `SELECT 1;` | 1 | 2 | **2** |
/// | `GOTO nowhere;` / `SELECT 1;` / `` / `-- a comment` | 1 | 2 | **4** |
/// | `GOTO nowhere;` / `SELECT` / `  1;` | 1 | 2 | **3** |
/// | `GOTO nowhere;` | 1 | 1 | **1** |
///
/// The four shapes are run end to end by `tests/run_batch_pipeline.rs`,
/// `the_line_of_133_is_the_last_line_of_the_batch`; a text that ends with a newline ends
/// on the line that newline closes, not on an empty line after it.
pub(crate) fn last_line(text: &str) -> u32 {
    let counted = text.strip_suffix('\n').unwrap_or(text);
    let breaks = u32::try_from(counted.matches('\n').count()).unwrap_or(u32::MAX);
    breaks.saturating_add(1)
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
