//! Database and index definition statements.
//!
//! `CREATE`/`ALTER`/`DROP DATABASE`, `USE`, `CREATE`/`DROP INDEX`, and the refusal by
//! name of the programmability objects (procedures, functions, views, triggers, ...),
//! which belong to V2 and V3.
//!
//! The signatures are the ones `parser::stmt` calls.
//!
//! # The options of a database are kept as text, never modelled
//!
//! VaubanDB has its own storage: `ON (NAME = …, FILENAME = …)`,
//! `LOG ON (…)`, `COLLATE`, `WITH …` and the `SET` options of a database will never be
//! applied. Everything written after the database name is therefore **swallowed** by
//! [`swallow_options`] and kept, token for token, in
//! [`DatabaseOption`]s so that `Display` can write it back and the binder can refuse
//! some of it later. Two heuristics do that swallowing, and both are deliberately
//! coarse: [`STATEMENT_STARTERS`] says where the statement ends, and
//! [`OPTION_CLAUSE_STARTERS`] says where one clause ends and the next one begins.
//!
//! # Re-serialisation deviations
//!
//! Two, both deliberate:
//!
//! - the whitespace of a swallowed clause is normalised: the value of a
//!   [`DatabaseOption`] is rebuilt from the source text of its tokens joined by a single
//!   space, so `ON(NAME=f)` comes back as `ON ( NAME = f )`. The AST of both spellings is
//!   the same one, which is what the `parse` → `Display` → `parse` loop needs;
//! - the deprecated `DROP INDEX t.ix` is read into the same AST as `DROP INDEX ix ON t`,
//!   which is the one form `Display` writes.
//!
//! The `WITH (…)` and `ON …` clauses of a `CREATE
//! INDEX`, and the `WITH (…)` of a `DROP INDEX`, reach the AST and come back from
//! `Display`. They are read by the rules of `parser/ddl_table.rs`, shared with the key
//! constraints.
//!
//! The grammar follows the T-SQL reference of each statement.

use vauban_errors::{SqlError, SqlResult};

use crate::ast::ddl::{
    AlterDatabaseStatement, Clustering, CreateDatabaseStatement, CreateIndexStatement,
    DatabaseOption, DropIndexStatement, IndexColumn,
};
use crate::ast::expr::{Ident, ObjectName};
use crate::ast::stmt::Statement;
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::parser::ddl_table::{parse_index_options, parse_index_storage};
use crate::parser::expr::parse_expr;
use crate::token::{Punct, TokenKind};

/// Parses a `CREATE DATABASE` statement, starting on its `CREATE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// Everything after the database name is swallowed as text (see the module
/// documentation): `CREATE DATABASE d ON PRIMARY (NAME = f) LOG ON (NAME = fl) COLLATE c`
/// yields three options, and none of them means anything to the engine.
///
/// # Errors
///
/// The syntax error that stopped the parse: a missing or malformed database name. The
/// swallowed part cannot fail, since it is not grammar.
pub(crate) fn parse_create_database(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Create)?;
    p.expect_keyword(Keyword::Database)?;
    let name = p.parse_ident()?;
    let options = swallow_options(p, OptionSplit::Clauses);
    Ok(Statement::CreateDatabase(Box::new(
        CreateDatabaseStatement {
            name,
            options,
            span: p.span_from(start),
        },
    )))
}

/// Parses an `ALTER DATABASE` statement, starting on its `ALTER` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The options are swallowed and cut on **top-level commas**, whether the statement is
/// written `ALTER DATABASE d SET a ON, b OFF` or without the `SET`
/// (`COLLATE c`, `MODIFY NAME = e`, `ADD FILE (…), (…) TO FILEGROUP fg`). The cut has to
/// be the same in both cases because `Display for AlterDatabaseStatement`
/// always writes a `SET` and always joins the options with `, `: cutting the `SET`-less
/// form clause by clause, as a `CREATE DATABASE` is cut, would read a comma written
/// **inside** an option back as a separator and change the AST on the second pass.
///
/// The `SET` itself is **not** stored: `Display` writes it back in front of the options,
/// so an option named `SET` would come back doubled. An `ALTER DATABASE d` whose options
/// are all empty — `ALTER DATABASE d` and `ALTER DATABASE d ,` alike — is refused below,
/// since `Display` would write a text the parser no longer accepts.
///
/// # Errors
///
/// The syntax error that stopped the parse: a missing or malformed database name, or a
/// statement that says nothing about the database — `ALTER DATABASE d` alone alters
/// nothing and SQL Server refuses it too.
pub(crate) fn parse_alter_database(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Alter)?;
    p.expect_keyword(Keyword::Database)?;
    let name = p.parse_ident()?;
    p.eat_keyword(Keyword::Set);
    let options = swallow_options(p, OptionSplit::Commas);
    if options.is_empty() {
        return Err(p.error_here());
    }
    Ok(Statement::AlterDatabase(Box::new(AlterDatabaseStatement {
        name,
        options,
        span: p.span_from(start),
    })))
}

/// Parses a `DROP DATABASE` statement, starting on its `DROP` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// `IF EXISTS` is a V4 spelling (SQL Server 2016) that every migration script uses; the
/// AST carries the flag, so the parser accepts it.
///
/// # Errors
///
/// The syntax error that stopped the parse: a missing name, or a `,` followed by
/// something that is not one.
pub(crate) fn parse_drop_database(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Drop)?;
    p.expect_keyword(Keyword::Database)?;
    let if_exists = p.eat_keyword_seq(&[Keyword::If, Keyword::Exists]);
    let mut names = vec![p.parse_ident()?];
    while p.eat_punct(Punct::Comma) {
        names.push(p.parse_ident()?);
    }
    Ok(Statement::DropDatabase {
        names,
        if_exists,
        span: p.span_from(start),
    })
}

/// Parses a `USE` statement, starting on its `USE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The name is a plain identifier and never a qualified one: a database has no schema.
///
/// # Errors
///
/// The syntax error of the token that is not a database name.
pub(crate) fn parse_use(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Use)?;
    let database = p.parse_ident()?;
    Ok(Statement::Use {
        database,
        span: p.span_from(start),
    })
}

/// Parses a `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX` statement, starting on its `CREATE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The order of the clauses is the one of T-SQL:
/// `CREATE [UNIQUE] [CLUSTERED | NONCLUSTERED] INDEX ix ON t (c [ASC |
/// DESC], …) [INCLUDE (…)] [WHERE …] [WITH (…)] [ON …]`. `UNIQUE` comes before the
/// clustering word. `INCLUDE` and `WHERE` describe V2 features and are kept in the AST;
/// `WITH (…)` and `ON …` are read by `parse_index_storage`, shared with the key
/// constraints, and reach [`CreateIndexStatement::storage`].
///
/// # Errors
///
/// The syntax error that stopped the parse: a missing name or table, an empty or
/// malformed column list, an option list that is empty or malformed, an `ON` that names
/// no identifier.
pub(crate) fn parse_create_index(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Create)?;
    let unique = p.eat_keyword(Keyword::Unique);
    let clustering = if p.eat_keyword(Keyword::Clustered) {
        Some(Clustering::Clustered)
    } else if p.eat_keyword(Keyword::NonClustered) {
        Some(Clustering::NonClustered)
    } else {
        None
    };
    p.expect_keyword(Keyword::Index)?;
    let name = p.parse_ident()?;
    p.expect_keyword(Keyword::On)?;
    let table = p.parse_object_name()?;
    let columns = parse_index_columns(p)?;
    let include = if p.eat_keyword(Keyword::Include) {
        parse_column_list(p)?
    } else {
        Vec::new()
    };
    let where_ = if p.eat_keyword(Keyword::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };
    let storage = parse_index_storage(p)?;
    Ok(Statement::CreateIndex(Box::new(CreateIndexStatement {
        name,
        table,
        columns,
        unique,
        clustering,
        include,
        where_,
        storage,
        span: p.span_from(start),
    })))
}

/// Parses a `DROP INDEX` statement, starting on its `DROP` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// Both spellings are read into the same AST: the modern `DROP INDEX ix ON t` and the
/// deprecated `DROP INDEX t.ix`, where the index name is the **last** part of a dotted
/// name and the table is everything before it (`dbo.t.ix` is the index `ix` of the table
/// `dbo.t`). Nothing records which one was written, so `Display` always writes the modern
/// one: that is the re-serialisation deviation of the module.
///
/// The modern spelling may end with `WITH (ONLINE = OFF, …)`, read by the open
/// option list of the key constraints. The deprecated one reads the list too and then
/// refuses it: a **well-formed** `DROP INDEX t.ix WITH (ONLINE = OFF)` is **102 near
/// 'with'**, lower-cased although the user wrote `WITH`, and on line 0, whether the batch
/// ends there or goes on with `SELECT 1` or `ON t`; a malformed list is the ordinary
/// syntax error of its faulty token (`WITH` alone: 102 'WITH'; `WITH x`: 102 'x'; `WITH
/// (`: 102 '('; `WITH ()`: 102 ')'; `WITH (ONLINE = OFF`: 102 'OFF'; `WITH SELECT`: 156),
/// and `WITH CHECK` is 102 near 'WITH'. Those eleven vectors are asserted in
/// `tests/ddl_database.rs` `drop_index_deprecated_with`.
///
/// # Errors
///
/// The syntax error that stopped the parse: a name that is neither followed by `ON` nor
/// qualified by its table (reported on the name), or a qualified name in front of an `ON`
/// (reported on the `ON`, as SQL Server does).
pub(crate) fn parse_drop_index(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Drop)?;
    p.expect_keyword(Keyword::Index)?;
    let if_exists = p.eat_keyword_seq(&[Keyword::If, Keyword::Exists]);
    let at_name = p.mark();
    let written = p.parse_object_name()?;
    let at_on = p.mark();
    let (name, table, options) = if p.eat_keyword(Keyword::On) {
        let table = p.parse_object_name()?;
        let Some(name) = only_part(written) else {
            // `DROP INDEX a.b ON t`: an index name is never qualified. SQL Server 2022
            // reports this on the `ON`, not on the name, whatever the number of parts:
            //     DROP INDEX a.b ON t
            //     DROP INDEX dbo.t.ix ON t
            // both give a 156 near the keyword 'ON'. Hence the reset to the `ON` rather
            // than to the start of the name.
            p.reset(at_on);
            return Err(p.error_here());
        };
        (name, table, parse_index_options(p)?)
    } else {
        let Some((name, table)) = split_qualified_index(written) else {
            // Neither `ON t` nor a `t.ix` spelling: the name alone says nothing about
            // the table the index belongs to.
            p.reset(at_name);
            return Err(p.error_here());
        };
        if p.at_keyword(Keyword::With) {
            // `WITH CHECK`: SQL Server names the `WITH` (102, its line), at the end of a
            // batch as before `SELECT 1`; the other malformed lists fall through to
            // `parse_index_options`, whose errors match SQL Server's (see above).
            if p.peek_at(1).kind == TokenKind::Keyword(Keyword::Check) {
                let with = p.peek();
                return Err(SqlError::incorrect_syntax_near(&with.text, with.span.line));
            }
            parse_index_options(p)?;
            // A well-formed list: 102, severity 15, state 1, line 0, near 'with', with
            // nothing, `SELECT 1` or `ON t` after the closing parenthesis (those three
            // shapes).
            return Err(SqlError::incorrect_syntax_near("with", 0));
        }
        (name, table, Vec::new())
    };
    Ok(Statement::DropIndex(Box::new(DropIndexStatement {
        name,
        table,
        if_exists,
        options,
        span: p.span_from(start),
    })))
}

/// Refuses a `CREATE`/`ALTER`/`DROP` of a programmability object (`PROCEDURE`, `PROC`, `FUNCTION`, `VIEW`, `TRIGGER`, `SCHEMA`, `SEQUENCE`, `TYPE`, `LOGIN`, `USER`, `ROLE`), which belongs to V2 or V3.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The refusal is a plain syntax error on the **object word**, not on the `CREATE`: the
/// message a client gets names what is not supported. Which of 102 and 156 that is
/// belongs to `syntax_error::at_cursor` and follows the nature of the token — 156 for the
/// reserved words (`PROCEDURE`, `PROC`, `FUNCTION`, `VIEW`, `TRIGGER`, `SCHEMA`), 102 for
/// the others (`SEQUENCE` is not reserved, and `TYPE`, `LOGIN` and `ROLE` are not
/// keywords at all).
///
/// SQL Server *accepts* these statements: refusing them is a deliberate difference.
///
/// # Errors
///
/// Always: that is the whole point of the function.
pub(crate) fn parse_unsupported_ddl(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    // The dispatch routes here on the token right after `CREATE`, `ALTER` or
    // `DROP`, so one step puts the cursor on the object word.
    p.advance();
    let error = p.error_here();
    p.reset(start);
    Err(error)
}

/// Reads the parenthesised key column list of an index: `(a, b DESC)`.
///
/// # Errors
///
/// The syntax error of the missing parenthesis, or of an item that is not a column name.
fn parse_index_columns(p: &mut Parser) -> SqlResult<Vec<IndexColumn>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut columns = vec![parse_index_column(p)?];
    while p.eat_punct(Punct::Comma) {
        columns.push(parse_index_column(p)?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(columns)
}

/// Reads one key column and the direction written next to it.
///
/// [`IndexColumn::explicit_direction`] records whether `ASC` or `DESC` was written, so
/// that `Display` writes back what the user wrote and no more: ascending is the default.
///
/// # Errors
///
/// The syntax error of the token that is not a column name.
fn parse_index_column(p: &mut Parser) -> SqlResult<IndexColumn> {
    let name = p.parse_ident()?;
    let (desc, explicit_direction) = if p.eat_keyword(Keyword::Desc) {
        (true, true)
    } else if p.eat_keyword(Keyword::Asc) {
        (false, true)
    } else {
        (false, false)
    };
    Ok(IndexColumn {
        name,
        desc,
        explicit_direction,
    })
}

/// Reads a parenthesised list of plain column names: the `INCLUDE (b, c)` of an index.
///
/// # Errors
///
/// The syntax error of the missing parenthesis, or of an item that is not a column name.
fn parse_column_list(p: &mut Parser) -> SqlResult<Vec<Ident>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut columns = vec![p.parse_ident()?];
    while p.eat_punct(Punct::Comma) {
        columns.push(p.parse_ident()?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(columns)
}

/// The single part of `name`, or `None` when it is qualified.
fn only_part(name: ObjectName) -> Option<Ident> {
    let unqualified = name.server.is_none() && name.database.is_none() && name.schema.is_none();
    unqualified.then_some(name.name)
}

/// Splits the deprecated `t.ix` spelling of a `DROP INDEX` into the index name and the
/// table it belongs to.
///
/// The parts shift one level to the right: the last one is the index, the one before it
/// is the table, and so on, so `dbo.t.ix` is the index `ix` of the table `dbo.t`. `None`
/// when the name has no qualifier at all, since there is then no table to name.
fn split_qualified_index(name: ObjectName) -> Option<(Ident, ObjectName)> {
    let table = ObjectName {
        server: None,
        database: name.server,
        schema: name.database,
        name: name.schema?,
        span: name.span,
    };
    Some((name.name, table))
}

/// How [`swallow_options`] cuts the tokens it swallows into options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionSplit {
    /// One option per clause, a clause starting at a word of [`OPTION_CLAUSE_STARTERS`]:
    /// the options of a `CREATE DATABASE`.
    Clauses,
    /// One option per top-level `,`, which is dropped: the options of an
    /// `ALTER DATABASE`, with or without its `SET`.
    Commas,
}

/// The words that can only open a **new statement**, and therefore end the swallowing of
/// the options of a database.
///
/// T-SQL does not require the `;`, so `CREATE DATABASE d SELECT 1` is two
/// statements and the swallower has to stop somewhere. This list is a heuristic and it is
/// meant to be seen: a database option that started with one of these words would be cut
/// short. None does.
///
/// `GO` is **not** in it, because it is not a keyword: the lexer makes a plain identifier
/// of it. [`at_statement_end`] compares its text instead, so that
/// `CREATE DATABASE d GO` stops here and `GO` gets a syntax error, rather than being
/// silently swallowed as an option.
const STATEMENT_STARTERS: [Keyword; 16] = [
    Keyword::Select,
    Keyword::Insert,
    Keyword::Update,
    Keyword::Delete,
    Keyword::Create,
    Keyword::Alter,
    Keyword::Drop,
    Keyword::Use,
    Keyword::Declare,
    Keyword::Set,
    Keyword::If,
    Keyword::While,
    Keyword::Begin,
    Keyword::Exec,
    Keyword::Execute,
    Keyword::Print,
];

/// The words that open a first-level clause of a `CREATE DATABASE`.
///
/// `ON [PRIMARY] (…)`, `LOG ON (…)`,
/// `COLLATE c`, `WITH …`, `CONTAINMENT = …`, `FOR ATTACH`, `AS SNAPSHOT OF d`. This too
/// is a heuristic: the options are never applied, so the only thing that matters is that
/// the cut be a **pure function of the token stream**, which is what makes the
/// `parse` → `Display` → `parse` loop hold.
const OPTION_CLAUSE_STARTERS: [&str; 7] =
    ["ON", "LOG", "COLLATE", "WITH", "CONTAINMENT", "FOR", "AS"];

/// The word the lexer hands over as an identifier and that ends a batch client-side.
const GO: &str = "GO";

/// Swallows everything up to the end of the statement and cuts it into options.
///
/// Each option keeps the **source text** of its tokens: the first one is the name, the
/// others are the value, joined by a single space, which is exactly how
/// `Display for DatabaseOption` writes them back. Parentheses are balanced along the way,
/// so a `;` or a statement word inside `(…)` does not stop anything.
///
/// This is not grammar and it never fails: a text that says nothing sensible yields
/// options that say nothing sensible, and the binder is free to refuse them later.
fn swallow_options(p: &mut Parser, split: OptionSplit) -> Vec<DatabaseOption> {
    let mut clauses: Vec<Vec<String>> = Vec::new();
    let mut depth = 0_usize;
    while !at_statement_end(p, depth) {
        if depth == 0 && split == OptionSplit::Commas && p.at_punct(Punct::Comma) {
            p.advance();
            clauses.push(Vec::new());
            continue;
        }
        if clauses.is_empty()
            || (depth == 0 && split == OptionSplit::Clauses && opens_a_clause(p, clauses.last()))
        {
            clauses.push(Vec::new());
        }
        let token = p.advance();
        match token.kind {
            TokenKind::Punct(Punct::LeftParen) => depth += 1,
            TokenKind::Punct(Punct::RightParen) => depth = depth.saturating_sub(1),
            _ => {}
        }
        if let Some(clause) = clauses.last_mut() {
            clause.push(token.text);
        }
    }
    clauses.into_iter().filter_map(into_option).collect()
}

/// Whether the cursor sits on the end of the statement being swallowed.
///
/// The end of the batch stops the swallowing whatever the parenthesis depth, which is
/// what keeps [`swallow_options`] from looping on an unbalanced `(`.
fn at_statement_end(p: &Parser, depth: usize) -> bool {
    if p.at_eof() {
        return true;
    }
    if depth > 0 {
        return false;
    }
    if p.at_punct(Punct::Semicolon) {
        return true;
    }
    match &p.peek().kind {
        TokenKind::Keyword(keyword) => STATEMENT_STARTERS.contains(keyword),
        TokenKind::Ident { value, quoted } => !*quoted && value.eq_ignore_ascii_case(GO),
        _ => false,
    }
}

/// Whether the token the cursor sits on opens a new first-level clause.
///
/// `current` is the clause being built, and it is what tells the `ON` of `LOG ON (…)`
/// from the `ON` that opens a clause of its own: the two-word `LOG ON` is the one
/// clause opener of T-SQL, so an `ON` that comes right after a clause-opening `LOG`
/// continues it.
fn opens_a_clause(p: &Parser, current: Option<&Vec<String>>) -> bool {
    // A delimited name (`[ON]`) or a string (`'ON'`) keeps its delimiters in `text` and
    // matches nothing here, which is what we want: it is a value, not a clause word.
    let text = &p.peek().text;
    if !OPTION_CLAUSE_STARTERS
        .iter()
        .any(|word| text.eq_ignore_ascii_case(word))
    {
        return false;
    }
    let after_log = current.is_some_and(
        |clause| matches!(clause.as_slice(), [first] if first.eq_ignore_ascii_case("LOG")),
    );
    !(after_log && text.eq_ignore_ascii_case("ON"))
}

/// Turns the tokens of one clause into an option, or into nothing when the clause is
/// empty (a `SET a, , b` and a trailing comma both yield one).
fn into_option(clause: Vec<String>) -> Option<DatabaseOption> {
    let mut parts = clause.into_iter();
    let name = parts.next()?;
    let value: Vec<String> = parts.collect();
    Some(DatabaseOption {
        name,
        value: (!value.is_empty()).then(|| value.join(" ")),
    })
}
