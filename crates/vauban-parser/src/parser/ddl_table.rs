//! Table definition statements, columns and constraints.
//!
//! `CREATE TABLE` is the largest grammar of the V1 data definition subset: every column
//! may carry half a dozen constraints, every constraint exists in a column-level and a
//! table-level spelling, and all of them may be named. **Nothing is checked here**: an
//! unknown type, a duplicate column, a non-constant `DEFAULT` and a foreign key that
//! points nowhere all parse, and it is the binder that reports the errors 2715, 2705 and
//! 2714.
//!
//! # No imposed order inside a column definition
//!
//! After its type, a column reads its `COLLATE`, its `IDENTITY` and its constraints in a
//! loop, in whatever order they were written: T-SQL imposes none, and the constraints go
//! into a single `Vec` in the order they appear, which is what makes the re-serialisation
//! faithful. `NULL` and `NOT NULL` are two ordinary entries of that vector.
//!
//! # Re-serialisation deviations
//!
//! The `parse` -> `Display` -> `parse` loop holds -- the trees compare equal -- but the
//! text `Display` writes differs from the source on five points, all of them because the
//! AST does not record which of several equivalent spellings was written:
//!
//! - **`IDENTITY` moves**: it is a field of [`ColumnDef`], not one of its constraints, so
//!   it is always written right after the type. `id int PRIMARY KEY IDENTITY(1,1)` comes
//!   back as `id int IDENTITY(1, 1) PRIMARY KEY`.
//! - **`PERSISTED` is dropped**: a computed column reads it and forgets it, the AST has
//!   no field for it. `b AS a + 1 PERSISTED` comes back as `b AS a + 1`.
//! - **The `WITH (…)` of a `CREATE TABLE` is dropped**: it is read and thrown away.
//!   `CREATE TABLE t (a int) WITH (DATA_COMPRESSION = PAGE)` comes back without the
//!   clause. The `ON …` and `TEXTIMAGE_ON …` of the table, and the `WITH (…)` and
//!   `ON …` of a key constraint, are kept and written back.
//! - **`FOREIGN KEY` disappears from a column constraint**: the words are optional there
//!   and the AST keeps only the reference, so `f int FOREIGN KEY REFERENCES p (id)` comes
//!   back as `f int REFERENCES p (id)`.
//! - **`ON DELETE` is written before `ON UPDATE`**, whichever order was written, because
//!   [`ForeignKeyRef`] holds one field per action.
//!
//! A sixth: `ALTER TABLE … CHECK CONSTRAINT` is written with an explicit `WITH CHECK` or
//! `WITH NOCHECK` prefix, since [`AlterTableAction::Check`] has no "absent" state. When
//! the user writes no `WITH` clause, `with_check` is set to **false**: `WITH CHECK` is
//! assumed for a *new* constraint and `WITH NOCHECK` for a *re-enabled* one, and `CHECK
//! CONSTRAINT` re-enables and does nothing else. Writing the assumed word back therefore keeps the
//! meaning of the statement.
//!
//! # Storage clauses of a key and of a table
//!
//! A `PRIMARY KEY` or `UNIQUE` constraint, at column or table level, may end with
//! `WITH (<option> = <value>, …)` then `ON <filegroup | scheme (column)>`, in that order
//! and each at most once; a `FOREIGN KEY`, a `CHECK` or a `DEFAULT` takes neither. The
//! option list is **open**: any regular name, and a value that is `ON`, `OFF`, an
//! unsigned integer or a bare word. SQL Server 2022 checks the names and the value types
//! after the parse, with errors 155, 153, 129 and 1080 (`tests/ddl_table.rs`); VaubanDB
//! leaves that to the catalog and accepts the shapes at the parse.
//!
//! The body of a `CREATE TABLE` may be followed by `ON …`, `TEXTIMAGE_ON <filegroup>`
//! and `WITH (…)`, in that order, each at most once. The first two reach the AST; the
//! third is still read and thrown away.
//!
//! The grammar follows the T-SQL reference of `CREATE TABLE`, `ALTER TABLE` and
//! `DROP TABLE`. No third-party parser is used.

use vauban_errors::{SqlError, SqlResult};

use crate::ast::ddl::{
    AlterTableAction, AlterTableStatement, Clustering, ColumnConstraint, ColumnConstraintKind,
    ColumnDef, ConstraintCheck, CreateTableStatement, DefaultConstraint, ForeignKeyRef, Identity,
    IndexColumn, IndexOption, IndexOptionValue, IndexStorage, RefAction, SortDirection,
    StoragePlacement, TableConstraint, TableConstraintKind, TableDefinition,
};
use crate::ast::expr::{DataType, Expr, Ident};
use crate::ast::stmt::Statement;
use crate::keyword::Keyword;
use crate::parser::Parser;
use crate::parser::datatype::parse_data_type;
use crate::parser::expr::{parse_expr, parse_value_expr};
use crate::span::Span;
use crate::token::{Op, Punct, TokenKind};

/// Parses a `CREATE TABLE` statement, starting on its `CREATE` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// The trailing `ON …` and `TEXTIMAGE_ON …` clauses reach the AST; the `WITH (<options>)`
/// that may follow them is read and thrown away (see the module deviations).
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_create_table(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Create)?;
    p.expect_keyword(Keyword::Table)?;
    let name = p.parse_object_name()?;
    let definition = parse_table_definition(p)?;
    let placement = if p.eat_keyword(Keyword::On) {
        Some(parse_storage_placement(p)?)
    } else {
        None
    };
    let textimage_on = if at_word(p, TEXTIMAGE_ON) {
        p.advance();
        Some(parse_filegroup_name(p)?)
    } else {
        None
    };
    skip_table_options(p);
    Ok(Statement::CreateTable(Box::new(CreateTableStatement {
        name,
        definition,
        placement,
        textimage_on,
        span: p.span_from(start),
    })))
}

/// The contextual word that opens the large-object filegroup clause of a `CREATE TABLE`.
/// It is not a keyword: the lexer hands it over as a regular identifier.
const TEXTIMAGE_ON: &str = "TEXTIMAGE_ON";

/// Whether the cursor sits on the regular, undelimited identifier `word`, compared
/// without regard to case.
fn at_word(p: &Parser, word: &str) -> bool {
    matches!(
        &p.peek().kind,
        TokenKind::Ident { value, quoted: false } if value.eq_ignore_ascii_case(word)
    )
}

/// Parses an `ALTER TABLE` statement, starting on its `ALTER` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_alter_table(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Alter)?;
    p.expect_keyword(Keyword::Table)?;
    let name = p.parse_object_name()?;
    let action = parse_alter_table_action(p)?;
    Ok(Statement::AlterTable(Box::new(AlterTableStatement {
        name,
        action,
        span: p.span_from(start),
    })))
}

/// Parses a `DROP TABLE` statement, starting on its `DROP` keyword.
///
/// The head token is still unconsumed: `parse_statement` decides on it without eating
/// it, and this function reads it back itself, because it needs it for the statement
/// span.
///
/// A temporary table is nothing special here: `#t` and `##t` are plain identifiers to
/// the lexer, so they are plain table names to this rule.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_drop_table(p: &mut Parser) -> SqlResult<Statement> {
    let start = p.mark();
    p.expect_keyword(Keyword::Drop)?;
    p.expect_keyword(Keyword::Table)?;
    let if_exists = p.eat_keyword_seq(&[Keyword::If, Keyword::Exists]);
    let mut names = vec![p.parse_object_name()?];
    while p.eat_punct(Punct::Comma) {
        names.push(p.parse_object_name()?);
    }
    Ok(Statement::DropTable {
        names,
        if_exists,
        span: p.span_from(start),
    })
}

/// Reads the body of a table -- its columns and its table-level constraints -- **the
/// opening and the closing parenthesis included**.
///
/// The caller sits on the `(` and gets the cursor back just after the matching `)`. That
/// is what makes the function reusable: `flow.rs` calls it directly on the `(a int NOT
/// NULL, PRIMARY KEY (a))` of a `DECLARE @t TABLE (…)`, with no `CREATE TABLE` around it.
///
/// Columns and constraints are told apart on the first word of each element: `CONSTRAINT`,
/// `PRIMARY`, `UNIQUE`, `FOREIGN`, `CHECK`, `DEFAULT` and `INDEX` open a table-level
/// constraint, anything else opens a column. The seven are reserved words
/// (`Keyword::is_reserved`), so a column that is really named `primary` is written
/// `[primary]` and lexes as an identifier, not as a keyword. Inline indexes (`INDEX IX_a (a)`) are out of the V1 subset and are
/// refused there, and so is a `DEFAULT` constraint (see [`parse_table_constraint`]).
///
/// The AST keeps columns and constraints in two vectors, so an element order that
/// interleaves them is not preserved: `Display` writes every column, then every
/// constraint.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_table_definition(p: &mut Parser) -> SqlResult<TableDefinition> {
    p.expect_punct(Punct::LeftParen)?;
    let mut columns = Vec::new();
    let mut constraints = Vec::new();
    loop {
        if at_table_constraint(p) {
            constraints.push(parse_table_constraint(p)?);
        } else {
            columns.push(parse_column_def(p)?);
        }
        if !p.eat_punct(Punct::Comma) {
            break;
        }
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(TableDefinition {
        columns,
        constraints,
    })
}

/// The keyword `n` tokens ahead, or `None` when that token is not a keyword.
fn keyword_at(p: &Parser, n: usize) -> Option<Keyword> {
    match p.peek_at(n).kind {
        TokenKind::Keyword(keyword) => Some(keyword),
        _ => None,
    }
}

/// Whether the element the cursor sits on is a table-level constraint rather than a
/// column definition.
fn at_table_constraint(p: &Parser) -> bool {
    matches!(
        keyword_at(p, 0),
        Some(
            Keyword::Constraint
                | Keyword::Primary
                | Keyword::Unique
                | Keyword::Foreign
                | Keyword::Check
                | Keyword::Default
                | Keyword::Index
        )
    )
}

/// Reads one column definition: a name, a type or a computed expression, then its
/// collation, its `IDENTITY` property and its constraints, in any order.
///
/// A computed column has no declared type; the AST has no room for its absence, so the
/// field holds an empty [`DataType`] that `Display` never writes, since it writes the
/// expression instead.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_column_def(p: &mut Parser) -> SqlResult<ColumnDef> {
    let start = p.mark();
    let name = p.parse_ident()?;
    let mut computed = None;
    let ty = if p.eat_keyword(Keyword::As) {
        computed = Some(parse_value_expr(p)?);
        // `PERSISTED` is accepted and forgotten: see the module deviations.
        let _ = p.eat_keyword(Keyword::Persisted);
        DataType {
            name: String::new(),
            args: Vec::new(),
            span: Span::EMPTY,
        }
    } else {
        parse_data_type(p)?
    };
    let mut collation = None;
    let mut identity = None;
    let mut constraints = Vec::new();
    loop {
        if collation.is_none() && p.at_keyword(Keyword::Collate) {
            p.advance();
            // A collation is a bare regular name, never a string literal, exactly as in
            // the `COLLATE` of an expression.
            collation = Some(p.parse_ident()?.value);
            continue;
        }
        if identity.is_none() && p.at_keyword(Keyword::Identity) {
            identity = Some(parse_identity(p)?);
            continue;
        }
        match parse_column_constraint(p)? {
            Some(constraint) => constraints.push(constraint),
            // The first word that opens no constraint ends the column: it is the `,` or
            // the `)` of the list, or a syntax error the caller reports.
            None => break,
        }
    }
    Ok(ColumnDef {
        name,
        ty,
        collation,
        constraints,
        identity,
        computed,
        span: p.span_from(start),
    })
}

/// Reads the `IDENTITY` property, the keyword included, with its seed and increment when
/// they were written.
///
/// The two arguments are **signed integers, not expressions**: the lexer hands `IDENTITY(10,
/// -2)` over as `Integer`, `Comma`, `Minus`, `Integer`, so an optional sign is read in
/// front of each of them and folded into the value.
///
/// # Errors
///
/// The syntax error of an argument that is not a signed integer, or of a missing comma or
/// parenthesis.
fn parse_identity(p: &mut Parser) -> SqlResult<Identity> {
    p.expect_keyword(Keyword::Identity)?;
    if !p.eat_punct(Punct::LeftParen) {
        return Ok(Identity {
            seed: None,
            increment: None,
        });
    }
    let seed = parse_signed_integer(p)?;
    p.expect_punct(Punct::Comma)?;
    let increment = parse_signed_integer(p)?;
    p.expect_punct(Punct::RightParen)?;
    Ok(Identity {
        seed: Some(seed),
        increment: Some(increment),
    })
}

/// Reads an optionally signed integer literal and returns its value.
///
/// # Errors
///
/// The syntax error of the token that is no integer, and of an integer too large for an
/// `i64`, which is never silently truncated.
fn parse_signed_integer(p: &mut Parser) -> SqlResult<i64> {
    let mut text = String::new();
    match op_at_cursor(p) {
        Some(Op::Minus) => {
            p.advance();
            text.push('-');
        }
        Some(Op::Plus) => {
            p.advance();
        }
        _ => {}
    }
    if !matches!(p.peek().kind, TokenKind::Integer) {
        return Err(p.error_here());
    }
    text.push_str(&p.peek().text);
    let Ok(value) = text.parse::<i64>() else {
        return Err(p.error_here());
    };
    p.advance();
    Ok(value)
}

/// The operator the cursor sits on, or `None` when it sits on something else.
fn op_at_cursor(p: &Parser) -> Option<Op> {
    match p.peek().kind {
        TokenKind::Op(op) => Some(op),
        _ => None,
    }
}

/// Reads one column-level constraint, or nothing at all when the cursor opens none.
///
/// `Ok(None)` means "this is not a constraint" and the cursor is left exactly where it
/// was, `CONSTRAINT name` prefix included: it is how the loop of [`parse_column_def`]
/// stops. A `CONSTRAINT name` followed by no constraint at all is a syntax error, not a
/// stop.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_column_constraint(p: &mut Parser) -> SqlResult<Option<ColumnConstraint>> {
    let start = p.mark();
    let name = if p.eat_keyword(Keyword::Constraint) {
        Some(p.parse_ident()?)
    } else {
        None
    };
    let kind = match keyword_at(p, 0) {
        Some(Keyword::Null) => {
            p.advance();
            ColumnConstraintKind::Null
        }
        Some(Keyword::Not) => {
            p.advance();
            p.expect_keyword(Keyword::Null)?;
            ColumnConstraintKind::NotNull
        }
        Some(Keyword::Default) => {
            p.advance();
            ColumnConstraintKind::Default(parse_value_expr(p)?)
        }
        Some(Keyword::Primary) => {
            p.advance();
            p.expect_keyword(Keyword::Key)?;
            ColumnConstraintKind::PrimaryKey {
                clustering: parse_clustering(p),
                order: parse_sort_direction(p),
            }
        }
        Some(Keyword::Unique) => {
            p.advance();
            ColumnConstraintKind::Unique {
                clustering: parse_clustering(p),
                order: parse_sort_direction(p),
            }
        }
        // `FOREIGN KEY` is optional in a column constraint: `REFERENCES p (id)` alone is
        // the usual spelling, and the AST keeps no trace of the longer one.
        Some(Keyword::Foreign) => {
            p.advance();
            p.expect_keyword(Keyword::Key)?;
            ColumnConstraintKind::ForeignKey(parse_foreign_key_ref(p)?)
        }
        Some(Keyword::References) => ColumnConstraintKind::ForeignKey(parse_foreign_key_ref(p)?),
        Some(Keyword::Check) => {
            let (expr, not_for_replication) = parse_check(p)?;
            ColumnConstraintKind::Check {
                expr,
                not_for_replication,
            }
        }
        Some(Keyword::RowGuidCol) => {
            p.advance();
            ColumnConstraintKind::RowGuidCol
        }
        _ => {
            if name.is_some() {
                return Err(p.error_here());
            }
            p.reset(start);
            return Ok(None);
        }
    };
    let storage = parse_key_storage(p, &kind)?;
    Ok(Some(ColumnConstraint {
        name,
        kind,
        storage,
        span: p.span_from(start),
    }))
}

/// Reads the storage clauses of a column-level constraint when it is a key, and leaves
/// the cursor alone otherwise: a `WITH` or `ON` after a `DEFAULT`, a `REFERENCES` or a
/// `CHECK` is not part of the constraint and becomes the syntax error of the caller.
///
/// # Errors
///
/// The syntax error that stopped the parse of the clauses.
fn parse_key_storage(p: &mut Parser, kind: &ColumnConstraintKind) -> SqlResult<IndexStorage> {
    if matches!(
        kind,
        ColumnConstraintKind::PrimaryKey { .. } | ColumnConstraintKind::Unique { .. }
    ) {
        parse_index_storage(p)
    } else {
        Ok(IndexStorage::default())
    }
}

/// Reads one table-level constraint, its optional `CONSTRAINT name` prefix included.
///
/// `DEFAULT expr FOR column` is a constraint of an `ALTER TABLE … ADD`
/// ([`parse_default_constraint`]), not of a table body. In the body of a table, SQL
/// Server 2022 reads the `DEFAULT expr` and then refuses the `FOR` with a **102 near
/// 'for'**, the word lower-cased although `FOR` and `fOr` were written (`CREATE TABLE t
/// (a int, CONSTRAINT df DEFAULT 0 fOr a)` and the same body in a `DECLARE @t TABLE`
/// both give a 102 near 'for', `tests/ddl_table.rs`
/// `default_constraint_in_table_body_refused`); that is what is reproduced here. Without
/// a `FOR`, SQL Server reports 142 (a syntax error in the definition of the constraint),
/// which is not in the catalogue: VaubanDB reports the syntax error of the token that
/// follows the expression instead.
///
/// # Errors
///
/// The syntax error that stopped the parse, in particular on the `INDEX` of an inline
/// index, which the V1 subset does not accept.
fn parse_table_constraint(p: &mut Parser) -> SqlResult<TableConstraint> {
    let start = p.mark();
    let name = if p.eat_keyword(Keyword::Constraint) {
        Some(p.parse_ident()?)
    } else {
        None
    };
    let kind = match keyword_at(p, 0) {
        Some(Keyword::Primary) => {
            p.advance();
            p.expect_keyword(Keyword::Key)?;
            let clustering = parse_clustering(p);
            TableConstraintKind::PrimaryKey {
                columns: parse_index_columns(p)?,
                clustering,
            }
        }
        Some(Keyword::Unique) => {
            p.advance();
            let clustering = parse_clustering(p);
            TableConstraintKind::Unique {
                columns: parse_index_columns(p)?,
                clustering,
            }
        }
        Some(Keyword::Default) => {
            p.advance();
            parse_value_expr(p)?;
            if p.at_keyword(Keyword::For) {
                let line = p.peek().span.line;
                return Err(SqlError::incorrect_syntax_near("for", line));
            }
            return Err(p.error_here());
        }
        Some(Keyword::Foreign) => {
            p.advance();
            p.expect_keyword(Keyword::Key)?;
            TableConstraintKind::ForeignKey {
                columns: parse_name_list(p)?,
                reference: parse_foreign_key_ref(p)?,
            }
        }
        Some(Keyword::Check) => {
            let (expr, not_for_replication) = parse_check(p)?;
            TableConstraintKind::Check {
                expr,
                not_for_replication,
            }
        }
        _ => return Err(p.error_here()),
    };
    let storage = if matches!(
        kind,
        TableConstraintKind::PrimaryKey { .. } | TableConstraintKind::Unique { .. }
    ) {
        parse_index_storage(p)?
    } else {
        IndexStorage::default()
    };
    Ok(TableConstraint {
        name,
        kind,
        storage,
        span: p.span_from(start),
    })
}

/// Reads `[CONSTRAINT name] DEFAULT expr FOR column`, the form of an `ALTER TABLE … ADD`.
///
/// # Errors
///
/// The syntax error that stopped the parse: a missing `FOR` (SQL Server reports 142
/// there, out of the catalogue), or a column that is no identifier (`FOR select` is 156,
/// `FOR a.b` is 102 near '.').
fn parse_default_constraint(p: &mut Parser) -> SqlResult<DefaultConstraint> {
    let start = p.mark();
    let name = if p.eat_keyword(Keyword::Constraint) {
        Some(p.parse_ident()?)
    } else {
        None
    };
    p.expect_keyword(Keyword::Default)?;
    let expr = parse_value_expr(p)?;
    p.expect_keyword(Keyword::For)?;
    let column = p.parse_ident()?;
    Ok(DefaultConstraint {
        name,
        expr,
        column,
        span: p.span_from(start),
    })
}

/// Whether the element the cursor sits on is a `DEFAULT … FOR` constraint, with or
/// without its `CONSTRAINT name` prefix.
fn at_default_constraint(p: &Parser) -> bool {
    match keyword_at(p, 0) {
        Some(Keyword::Default) => true,
        Some(Keyword::Constraint) => keyword_at(p, 2) == Some(Keyword::Default),
        _ => false,
    }
}

/// Reads `CHECK [NOT FOR REPLICATION] (expr)`, the keyword included, and returns the
/// predicate and whether replication was excluded.
///
/// The predicate is a **search condition**, so it goes through `parse_expr` and not
/// through the value expression of a `DEFAULT`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_check(p: &mut Parser) -> SqlResult<(Expr, bool)> {
    p.expect_keyword(Keyword::Check)?;
    let not_for_replication = p.eat_keyword(Keyword::Not);
    if not_for_replication {
        p.expect_keyword(Keyword::For)?;
        p.expect_keyword(Keyword::Replication)?;
    }
    p.expect_punct(Punct::LeftParen)?;
    let expr = parse_expr(p)?;
    p.expect_punct(Punct::RightParen)?;
    Ok((expr, not_for_replication))
}

/// Reads what a foreign key points at: `REFERENCES t [(c, …)]` and its actions.
///
/// The referenced column list may be left out, and then [`ForeignKeyRef::columns`] is
/// empty: the primary key of the referenced table is implied, and resolving it is the
/// binder's job.
///
/// `ON DELETE` and `ON UPDATE` come in either order, each at most once; the last one
/// written wins, and the binder is the one that could complain about a repetition.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_foreign_key_ref(p: &mut Parser) -> SqlResult<ForeignKeyRef> {
    p.expect_keyword(Keyword::References)?;
    let table = p.parse_object_name()?;
    let columns = if p.at_punct(Punct::LeftParen) {
        parse_name_list(p)?
    } else {
        Vec::new()
    };
    let mut on_delete = None;
    let mut on_update = None;
    // The `ON` of an action is only ever read inside the parentheses of a table body or
    // after an `ALTER TABLE … ADD`, so it never competes with the `ON <filegroup>` of a
    // `CREATE TABLE`, which sits after the closing parenthesis.
    while keyword_at(p, 0) == Some(Keyword::On)
        && matches!(keyword_at(p, 1), Some(Keyword::Delete | Keyword::Update))
    {
        p.advance();
        let deleting = p.eat_keyword(Keyword::Delete);
        if !deleting {
            p.expect_keyword(Keyword::Update)?;
        }
        let action = parse_ref_action(p)?;
        if deleting {
            on_delete = Some(action);
        } else {
            on_update = Some(action);
        }
    }
    Ok(ForeignKeyRef {
        table,
        columns,
        on_delete,
        on_update,
    })
}

/// Reads the action of an `ON DELETE` or `ON UPDATE` clause.
///
/// `NO ACTION`, `CASCADE`, `SET NULL` or `SET DEFAULT`, two words each but `CASCADE`.
///
/// # Errors
///
/// The syntax error of the word that names no action.
fn parse_ref_action(p: &mut Parser) -> SqlResult<RefAction> {
    match keyword_at(p, 0) {
        Some(Keyword::No) => {
            p.advance();
            p.expect_keyword(Keyword::Action)?;
            Ok(RefAction::NoAction)
        }
        Some(Keyword::Cascade) => {
            p.advance();
            Ok(RefAction::Cascade)
        }
        Some(Keyword::Set) => {
            p.advance();
            if p.eat_keyword(Keyword::Null) {
                return Ok(RefAction::SetNull);
            }
            p.expect_keyword(Keyword::Default)?;
            Ok(RefAction::SetDefault)
        }
        _ => Err(p.error_here()),
    }
}

/// Reads `CLUSTERED` or `NONCLUSTERED` when one of them is written.
fn parse_clustering(p: &mut Parser) -> Option<Clustering> {
    if p.eat_keyword(Keyword::Clustered) {
        return Some(Clustering::Clustered);
    }
    if p.eat_keyword(Keyword::NonClustered) {
        return Some(Clustering::NonClustered);
    }
    None
}

/// Reads `ASC` or `DESC` when one of them is written.
fn parse_sort_direction(p: &mut Parser) -> Option<SortDirection> {
    if p.eat_keyword(Keyword::Asc) {
        return Some(SortDirection::Asc);
    }
    if p.eat_keyword(Keyword::Desc) {
        return Some(SortDirection::Desc);
    }
    None
}

/// Reads a parenthesised list of bare names, both parentheses included.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_name_list(p: &mut Parser) -> SqlResult<Vec<Ident>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut names = vec![p.parse_ident()?];
    while p.eat_punct(Punct::Comma) {
        names.push(p.parse_ident()?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(names)
}

/// Reads a parenthesised list of key columns, each with its optional direction, both
/// parentheses included.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_index_columns(p: &mut Parser) -> SqlResult<Vec<IndexColumn>> {
    p.expect_punct(Punct::LeftParen)?;
    let mut columns = vec![parse_index_column(p)?];
    while p.eat_punct(Punct::Comma) {
        columns.push(parse_index_column(p)?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(columns)
}

/// Reads one key column and the `ASC` or `DESC` written next to it, if any.
///
/// # Errors
///
/// The syntax error of the token that is no name.
fn parse_index_column(p: &mut Parser) -> SqlResult<IndexColumn> {
    let name = p.parse_ident()?;
    let direction = parse_sort_direction(p);
    Ok(IndexColumn {
        name,
        desc: direction == Some(SortDirection::Desc),
        explicit_direction: direction.is_some(),
    })
}

/// Reads a comma-separated list of bare names, without parentheses.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_bare_name_list(p: &mut Parser) -> SqlResult<Vec<Ident>> {
    let mut names = vec![p.parse_ident()?];
    while p.eat_punct(Punct::Comma) {
        names.push(p.parse_ident()?);
    }
    Ok(names)
}

/// Reads what an `ALTER TABLE` does, the table name already consumed.
///
/// An optional `WITH CHECK` or `WITH NOCHECK` comes first; only `ADD`, `CHECK
/// CONSTRAINT` and `NOCHECK CONSTRAINT` may follow it. SQL Server 2022 accepts the prefix
/// in front of an added column as well as of an added constraint, and refuses it in
/// front of `DROP` (156 near 'DROP') and `ALTER COLUMN` (156 near 'ALTER')
/// (`tests/ddl_table.rs`).
///
/// # Errors
///
/// The syntax error that stopped the parse. A statement whose action word is none of
/// `ADD`, `DROP`, `ALTER`, `WITH`, `CHECK` or `NOCHECK` stops on that word.
fn parse_alter_table_action(p: &mut Parser) -> SqlResult<AlterTableAction> {
    let with_check = parse_with_check(p)?;
    if p.eat_keyword(Keyword::Add) {
        // Defaults, columns and constraints are told apart on the first words of the
        // list, as inside a table body, and the AST holds one of the three, never a mix
        // (`tests/ddl_table.rs` `alter_table_add_default_for`).
        if at_default_constraint(p) {
            let mut defaults = vec![parse_default_constraint(p)?];
            while p.eat_punct(Punct::Comma) {
                defaults.push(parse_default_constraint(p)?);
            }
            return Ok(AlterTableAction::AddDefaults {
                defaults,
                with_check,
            });
        }
        if at_table_constraint(p) {
            let mut constraints = vec![parse_table_constraint(p)?];
            while p.eat_punct(Punct::Comma) {
                constraints.push(parse_table_constraint(p)?);
            }
            return Ok(AlterTableAction::AddConstraints {
                constraints,
                with_check,
            });
        }
        let mut columns = vec![parse_column_def(p)?];
        while p.eat_punct(Punct::Comma) {
            columns.push(parse_column_def(p)?);
        }
        return Ok(AlterTableAction::AddColumns {
            columns,
            with_check,
        });
    }
    if with_check.is_some() {
        return parse_check_constraint_action(p, with_check);
    }
    if p.eat_keyword(Keyword::Drop) {
        if p.eat_keyword(Keyword::Column) {
            let if_exists = p.eat_keyword_seq(&[Keyword::If, Keyword::Exists]);
            return Ok(AlterTableAction::DropColumns {
                names: parse_bare_name_list(p)?,
                if_exists,
            });
        }
        // The `CONSTRAINT` word is optional in T-SQL, but the AST does not record whether
        // it was written, so the V1 subset requires it.
        p.expect_keyword(Keyword::Constraint)?;
        let if_exists = p.eat_keyword_seq(&[Keyword::If, Keyword::Exists]);
        return Ok(AlterTableAction::DropConstraints {
            names: parse_bare_name_list(p)?,
            if_exists,
        });
    }
    if p.eat_keyword(Keyword::Alter) {
        p.expect_keyword(Keyword::Column)?;
        return Ok(AlterTableAction::AlterColumn(Box::new(parse_column_def(
            p,
        )?)));
    }
    parse_check_constraint_action(p, None)
}

/// Reads the optional `WITH CHECK` or `WITH NOCHECK` that may open an `ALTER TABLE`
/// action, and returns `None` without moving when there is no `WITH`.
///
/// # Errors
///
/// The syntax error of the word after `WITH` when it is neither `CHECK` nor `NOCHECK`.
fn parse_with_check(p: &mut Parser) -> SqlResult<Option<ConstraintCheck>> {
    if !p.eat_keyword(Keyword::With) {
        return Ok(None);
    }
    if p.eat_keyword(Keyword::Check) {
        return Ok(Some(ConstraintCheck::Check));
    }
    p.expect_keyword(Keyword::NoCheck)?;
    Ok(Some(ConstraintCheck::NoCheck))
}

/// Reads `{CHECK | NOCHECK} CONSTRAINT {ALL | name, …}`, the `WITH CHECK` or `WITH
/// NOCHECK` prefix already consumed by the caller and handed over as `with_check`.
///
/// Yes, `CHECK` twice: `ALTER TABLE t WITH CHECK CHECK CONSTRAINT ALL` is what the
/// scripts SSMS generates look like. The first word belongs to the `WITH` clause, the
/// second is the verb.
///
/// An **empty** name list means `ALL`: `ALL` is a reserved word, `Parser::parse_ident`
/// refuses it, so it cannot be an `Ident`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
fn parse_check_constraint_action(
    p: &mut Parser,
    with_check: Option<ConstraintCheck>,
) -> SqlResult<AlterTableAction> {
    // False when the `WITH` clause is absent: `WITH NOCHECK` is assumed for a re-enabled
    // constraint, and this verb re-enables and does nothing else (module deviations).
    let with_check = with_check == Some(ConstraintCheck::Check);
    let enable = p.eat_keyword(Keyword::Check);
    if !enable {
        p.expect_keyword(Keyword::NoCheck)?;
    }
    p.expect_keyword(Keyword::Constraint)?;
    let constraints = if p.eat_keyword(Keyword::All) {
        Vec::new()
    } else {
        parse_bare_name_list(p)?
    };
    Ok(AlterTableAction::Check {
        constraints,
        enable,
        with_check,
    })
}

/// Reads the `WITH (<option> = <value>, …)` list of a key constraint or an index, the
/// `WITH` keyword included, and returns nothing when the cursor is not on a `WITH`.
///
/// The list is open (module documentation): a name is any regular identifier or
/// non-reserved keyword, and a value is `ON`, `OFF`, an unsigned integer or another bare
/// word. As SQL Server answers them, reproduced here (`index_option_shapes_refused` in
/// `tests/ddl_table.rs`):
/// `WITH ()` is 102 near `')'`, `[PAD_INDEX] = OFF` is 102 near `'PAD_INDEX'`,
/// `SELECT = OFF` and `FOO = SELECT` are 156, `FOO = 'x'` is 102 near `'x'`,
/// `DATA_COMPRESSION = [PAGE]` is 102 near `'PAGE'`, `FILLFACTOR = +1` is 102 near `'+'`.
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_index_options(p: &mut Parser) -> SqlResult<Vec<IndexOption>> {
    if !p.eat_keyword(Keyword::With) {
        return Ok(Vec::new());
    }
    p.expect_punct(Punct::LeftParen)?;
    let mut options = vec![parse_index_option(p)?];
    while p.eat_punct(Punct::Comma) {
        options.push(parse_index_option(p)?);
    }
    p.expect_punct(Punct::RightParen)?;
    Ok(options)
}

/// Reads one `name = value` of an option list.
///
/// A name is a bare word, or the reserved keyword `FILLFACTOR`: it is the one index
/// option that is a reserved word, and `WITH (FILLFACTOR = 80)` parses on SQL Server
/// while `WITH (SELECT = OFF)` is 156.
///
/// # Errors
///
/// The syntax error of a delimited or reserved name, of a missing `=`, or of a value
/// outside the four accepted shapes.
fn parse_index_option(p: &mut Parser) -> SqlResult<IndexOption> {
    let name = if p.at_keyword(Keyword::FillFactor) {
        p.peek().text.clone()
    } else {
        let Some(name) = bare_word_at_cursor(p) else {
            return Err(p.error_here());
        };
        name
    };
    p.advance();
    if op_at_cursor(p) != Some(Op::Eq) {
        return Err(p.error_here());
    }
    p.advance();
    let value = match keyword_at(p, 0) {
        Some(Keyword::On) => IndexOptionValue::On,
        Some(Keyword::Off) => IndexOptionValue::Off,
        _ => {
            if matches!(p.peek().kind, TokenKind::Integer) {
                IndexOptionValue::Integer(p.peek().text.clone())
            } else if let Some(word) = bare_word_at_cursor(p) {
                IndexOptionValue::Word(word)
            } else {
                return Err(p.error_here());
            }
        }
    };
    p.advance();
    Ok(IndexOption { name, value })
}

/// The text of the cursor token when it is a regular, undelimited identifier or a
/// non-reserved keyword; `None` for anything else, delimited names and the temporary
/// names `#x`/`##x` included (the lexer hands those over as identifiers; SQL Server 2022
/// refuses `WITH (ONLINE = #x)` with 102 near '#x' and `WITH (#x = ON)` with 155,
/// `tests/ddl_table.rs` `index_option_shapes_refused`).
fn bare_word_at_cursor(p: &Parser) -> Option<String> {
    let token = p.peek();
    match &token.kind {
        TokenKind::Ident {
            value,
            quoted: false,
        } if !value.starts_with('#') => Some(value.clone()),
        TokenKind::Keyword(keyword) if !keyword.is_reserved() => Some(token.text.clone()),
        _ => None,
    }
}

/// Reads what follows the `ON` of a storage clause, the keyword already consumed: a
/// filegroup name, or a partition scheme and its column in parentheses.
///
/// The name goes through [`parse_filegroup_name`]: `ON PRIMARY` and `ON default` are 156
/// near that word, `ON [PRIMARY]`, `ON "default"`, `ON [default]`, `ON 'PRIMARY'` and
/// `ON N'PRIMARY'` parse, as SQL Server 2022 does (`tests/ddl_table.rs`
/// `filegroup_name_as_string`). The partition column is a
/// plain identifier: `ON ps ('a')` is 102 near 'a' on both sides.
///
/// # Errors
///
/// The syntax error of a name that is no identifier, or of a malformed column group.
pub(crate) fn parse_storage_placement(p: &mut Parser) -> SqlResult<StoragePlacement> {
    let name = parse_filegroup_name(p)?;
    let partition_column = if p.eat_punct(Punct::LeftParen) {
        let column = p.parse_ident()?;
        p.expect_punct(Punct::RightParen)?;
        Some(column)
    } else {
        None
    };
    Ok(StoragePlacement {
        name,
        partition_column,
    })
}

/// Reads a filegroup or partition scheme name: an identifier, or a character string
/// (`'PRIMARY'`, `N'PRIMARY'`) that SQL Server 2022 accepts there and that becomes a
/// delimited [`Ident`], so that `Display` writes `[PRIMARY]` and the tree survives the
/// loop (`tests/ddl_table.rs` `filegroup_name_as_string`).
///
/// # Errors
///
/// The syntax error of a token that is neither.
pub(crate) fn parse_filegroup_name(p: &mut Parser) -> SqlResult<Ident> {
    if let TokenKind::Str { value, .. } = &p.peek().kind {
        let name = Ident {
            value: value.clone(),
            quoted: true,
        };
        p.advance();
        return Ok(name);
    }
    p.parse_ident()
}

/// Reads the storage clauses of a key constraint or an index: an optional `WITH (…)`
/// then an optional `ON …`, in that order.
///
/// The reverse order is not read: on `ON [PRIMARY] WITH (…)` the `WITH` is left to the
/// caller, whose next expectation fails on it, as SQL Server 2022 does (156 near 'WITH'
/// after a constraint).
///
/// # Errors
///
/// The syntax error that stopped the parse.
pub(crate) fn parse_index_storage(p: &mut Parser) -> SqlResult<IndexStorage> {
    let options = parse_index_options(p)?;
    let placement = if p.eat_keyword(Keyword::On) {
        Some(parse_storage_placement(p)?)
    } else {
        None
    };
    Ok(IndexStorage { options, placement })
}

/// Reads and throws away the `WITH (<options>)` that may end a `CREATE TABLE`, after
/// its `ON …` and `TEXTIMAGE_ON …`.
///
/// The AST records nothing of it (module deviations): table options are not index
/// options, and they are not modelled. A `WITH` that is not followed by
/// `(` is left untouched and becomes the syntax error of the caller.
fn skip_table_options(p: &mut Parser) {
    if keyword_at(p, 0) == Some(Keyword::With)
        && matches!(p.peek_at(1).kind, TokenKind::Punct(Punct::LeftParen))
    {
        p.advance();
        skip_balanced_parens(p);
    }
}

/// Consumes a parenthesised group whose `(` the cursor sits on, nesting included.
///
/// Does nothing when the cursor is elsewhere, and stops at the end of the batch rather
/// than looping forever on an unclosed group, which the caller then reports.
fn skip_balanced_parens(p: &mut Parser) {
    if !p.eat_punct(Punct::LeftParen) {
        return;
    }
    let mut depth = 1_usize;
    while depth > 0 && !p.at_eof() {
        if p.at_punct(Punct::LeftParen) {
            depth += 1;
        } else if p.at_punct(Punct::RightParen) {
            depth -= 1;
        }
        p.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::parse_table_definition;
    use crate::parser::{ParseOptions, Parser};

    /// Builds a cursor over `text` with the default options.
    fn cursor(text: &str) -> Parser<'static> {
        static OPTIONS: ParseOptions = ParseOptions {
            quoted_identifier: true,
        };
        match Parser::new(text, &OPTIONS) {
            Ok(parser) => parser,
            Err(error) => unreachable!("the test text lexes: {error:?}"),
        }
    }

    /// The entry point `flow.rs` needs for `DECLARE @t TABLE (…)`: called on the `(`, it
    /// gives the cursor back after the matching `)`, with no statement around it.
    #[test]
    fn table_definition_is_reusable() {
        let text = "(a int NOT NULL, PRIMARY KEY (a))";
        let mut p = cursor(text);
        let definition = match parse_table_definition(&mut p) {
            Ok(definition) => definition,
            Err(error) => unreachable!("{text} should parse: {error:?}"),
        };
        assert_eq!(definition.columns.len(), 1);
        assert_eq!(definition.columns[0].name.value, "a");
        assert_eq!(definition.constraints.len(), 1);
        // Both parentheses are consumed: nothing is left for the caller.
        assert!(p.at_eof(), "{text} left {:?} unread", p.peek());
    }
}
