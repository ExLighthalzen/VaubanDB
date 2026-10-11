//! Binding of `CREATE`/`DROP DATABASE`, `ALTER DATABASE … SET`, `CREATE`/`DROP TABLE` and
//! `USE`.
//!
//! The binder turns the AST of a DDL statement into the `*Def` the catalogue takes
//! ([`TableDef`]) and checks what it can check without writing anything. Calling
//! `catalog.create_*` is the executor's job, not this file's.
//!
//! # What is checked here, and what is left to execution
//!
//! With `SELECT 1;` **before** the DDL statement, a batch that answers one result set before
//! its error failed while it ran, a batch that answers zero failed while it compiled.
//! `session::prepare_batch` binds the statements of a batch before it runs any of them, so a
//! check moved into the binder moves the error to compilation time and takes that result
//! set away. What SQL Server answers:
//!
//! | batch | number | result sets | the check runs |
//! |---|:-:|:-:|---|
//! | `SELECT 1; CREATE TABLE dbo.m5 (a int, b nosuch);` | 2715 | 0 | at compilation |
//! | `SELECT 1; CREATE TABLE dbo.n1 (a int, a int);` | 2705 | 0 | at compilation |
//! | `SELECT 1; CREATE TABLE dbo.ex (a int);` (`ex` exists) | 2714 | **1** | at execution |
//! | `SELECT 1; DROP TABLE dbo.nosuchtable;` | 3701 | **1** | at execution |
//! | `SELECT 1; DROP DATABASE nosuchdatabase;` | 3701 | **1** | at execution |
//! | `SELECT 1; USE nosuchdatabase;` | 911 | 0 | at compilation |
//! | `SELECT 1; ALTER DATABASE nosuchdatabase SET READ_COMMITTED_SNAPSHOT ON;` | 5011 | **1** | at execution |
//! | `SELECT 1; CREATE TABLE dbo.d1 (a int, b int DEFAULT a);` | 128 | 0 | at compilation |
//! | `SELECT 1; CREATE TABLE dbo.d2 (a int, b int DEFAULT (SELECT 1));` | 1046 | 0 | at compilation |
//! | `SELECT 1; CREATE TABLE dbo.d3 (a int, CHECK (z > 0));` | 207 | 0 | at compilation |
//! | `SELECT 1; CREATE TABLE dbo.d9 (a date DEFAULT 1); SELECT 2;` | 206 | 0 | at compilation, the batch stops |
//!
//! The `DEFAULT` and `CHECK` expressions are bound here ([`check_default_expression`],
//! [`check_check_expression`]); the refusals they lead to are listed there.
//!
//! So the type of a column is resolved here (2715), and the existence of a table or of a
//! database is **not** looked at for a `DROP` or an `ALTER DATABASE`: the executor and the
//! catalogue answer 3701 or 5011 when they cannot find it, which is where SQL Server answers
//! it too. The 5011 of an `ALTER DATABASE … SET` is severity 14, state 5, on the line of the
//! statement, and the batch goes on after it (`SELECT 2;` written after the failing
//! statement still answers its row): a run-time error scoped to its statement.
//!
//! 2714 is a difference this file makes on purpose, and it is bounded to the statement
//! written at the head of a batch: the binder refuses a `CREATE TABLE` whose name the
//! catalogue already resolves, **before** anything is written, where SQL Server refuses it
//! while it runs. A batch that mixes a result set and such a `CREATE TABLE` therefore
//! answers one result set on SQL Server and zero here
//! (`ddl::tests::create_table_duplicate_name_is_2714`).
//!
//! Written inside `IF`, `WHILE` or `BEGIN … END`, that `CREATE TABLE` is not bound here at
//! all: `control.rs` keeps it as it was parsed until the control of flow reaches it, which
//! puts its 2714 back where the table above reads it, at execution
//! (`tests/bind_control_flow.rs`,
//! `a_branch_that_creates_a_table_the_catalogue_holds_binds_deferred`). That is what lets a
//! migration script write `IF OBJECT_ID('dbo.t') IS NULL CREATE TABLE dbo.t (a int);` and
//! replay it over the table it created.
//!
//! `USE` binds whatever the name: [`CatalogView`](crate::CatalogView) answers about tables
//! and views, not about databases, so the binder cannot tell `USE nosuch` from
//! `USE master`. 911 is `session`'s, at execution, where SQL Server answers it at
//! compilation — and on the line of the **name**, not of the statement: `USE` on line 3
//! with `nosuchdatabase` on line 4 answers 4.
//!
//! # The line each number carries
//!
//! With the statement starting on line 3 of the batch and the node named below on a line
//! of its own: 2715 answers **3** although the unknown type sits on line 6, 2714 answers
//! **3** although the duplicate name sits on line 4, and 2705 answers **3**. The three of
//! them name the **statement**; 448 after a `CREATE DATABASE … COLLATE` names the
//! collation, as 447 and 448 do in an expression (`errors.rs`).
//!
//! # A fault in the column list: the first one wins
//!
//! Duplicate columns answer 2705, severity 16, state 3, on the line of the statement; the
//! comparison ignores case (`(a int, A int)` answers it, naming `'A'`, the occurrence
//! written last) and the table name is printed **without** its schema.
//! [`SqlError::duplicate_column_name`] builds it.
//!
//! 2705 and 2715 do not rank: the column list is read left to right and the first faulty
//! column decides. `CREATE TABLE dbo.m6 (a int, a nosuch);` answers 2705 because the
//! duplicate `a` comes first, where `CREATE TABLE t_both (a foo, b int, b int);` answers
//! 2715 state 6 because the unknown type comes first. This file reads the list in that
//! order (`ddl::tests::the_first_fault_of_the_column_list_wins`).
//!
//! That 2715 is the one of a column list, state **6**
//! ([`SqlError::cannot_find_data_type_in_table`]), and not the state 3 of a `DECLARE` that
//! [`SqlError::cannot_find_data_type`] sends; [`in_column_list`] moves it. Still without a
//! constructor: 448 sends state **3** after `CREATE DATABASE … COLLATE` and state **2**
//! after a column `COLLATE`, where [`SqlError::invalid_collation`] sends the 1 of
//! `SELECT 'a' COLLATE …`.

use vauban_catalog::{
    ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, RefAction as CatalogAction,
    SortedColumn, TableDef,
};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{
    AlterDatabaseStatement, Clustering, ColumnConstraintKind, ColumnDef as AstColumnDef, ColumnRef,
    CreateDatabaseStatement, CreateTableStatement, DatabaseOption, Expr, ForeignKeyRef, Ident,
    InList, IndexColumn, ObjectName, RefAction, SortDirection, Span, TableConstraint,
    TableConstraintKind,
};
use vauban_types::{Collation, SqlType, TypeInfo};

use crate::bound::{BoundStatement, ColumnBinding, DdlStatement};
use crate::call::{bind_niladic, is_untyped_null};
use crate::context::{BindContext, NoVariables, ResolvedTableKind};
use crate::datatype::resolve_data_type;
use crate::ddl_index::{
    primary_key_clustered_by_default, refuse_duplicate_key_column, refuse_two_keys,
};
use crate::expr::{Scope, bind_condition, bind_expr};
use crate::insert::unbracket_type_names;
use crate::names::from_needs_the_catalogue;
use crate::query::not_yet;
use crate::star::Source;
use crate::stored_text::{ExpressionKind, stored_text};

/// Binds `CREATE DATABASE d [options]`.
///
/// Everything the parser swallowed after the name is dropped but the `COLLATE` clause
/// (`FILENAME`, `ON PRIMARY`, `LOG ON`, `WITH …`: VaubanDB has its own storage). The
/// collation is resolved here because [`DdlStatement::CreateDatabase`] carries a
/// [`Collation`], not a name.
///
/// Whether the database already exists is **not** checked: 1801 belongs to
/// `Catalog::create_database`, the layer that would write.
///
/// # Errors
///
/// 448 when the `COLLATE` clause names a collation `types` does not know. SQL Server puts
/// that error on the line of the collation **name** (module header); the parser keeps a
/// swallowed clause as text without a span, so this site answers the line of the
/// statement instead — a difference visible on a `CREATE DATABASE` whose `COLLATE` sits
/// on another line than its `CREATE`.
pub(crate) fn bind_create_database(stmt: &CreateDatabaseStatement) -> SqlResult<BoundStatement> {
    let collation = match collate_option(&stmt.options) {
        Some(name) => Some(Collation::parse(name).map_err(|err| on_statement(err, &stmt.span))?),
        None => None,
    };
    Ok(BoundStatement::Ddl(DdlStatement::CreateDatabase {
        name: stmt.name.value.clone(),
        collation,
    }))
}

/// The name written after `COLLATE` among the options the parser swallowed, `None` when the
/// statement has no such clause.
///
/// `swallow_options` cuts a `CREATE DATABASE` clause by clause and keeps the first token of
/// each as its name, so `COLLATE Latin1_General_CI_AS` is one option named `COLLATE` whose
/// value is the rest of the clause.
fn collate_option(options: &[DatabaseOption]) -> Option<&str> {
    options
        .iter()
        .find(|option| option.name.eq_ignore_ascii_case("COLLATE"))
        .and_then(|option| option.value.as_deref())
}

/// Binds `DROP DATABASE [IF EXISTS] d1, d2`.
///
/// The names are carried as written: which of them exist is decided while the statement
/// runs, because that is where SQL Server decides it (module header, 3701 with one result
/// set already sent).
pub(crate) fn bind_drop_database(names: &[Ident], if_exists: bool) -> SqlResult<BoundStatement> {
    Ok(BoundStatement::Ddl(DdlStatement::DropDatabase {
        names: names.iter().map(|name| name.value.clone()).collect(),
        if_exists,
    }))
}

/// Binds `USE d`.
///
/// The name is carried as written: the binder has no way to know which databases exist
/// (module header), so `session` is left to answer 911.
pub(crate) fn bind_use(database: &Ident) -> SqlResult<BoundStatement> {
    Ok(BoundStatement::Use {
        database: database.value.clone(),
    })
}

// -------------------------------------------------------------------------------------------
// ALTER DATABASE … SET
// -------------------------------------------------------------------------------------------

/// The two options of `ALTER DATABASE … SET` this file binds, spelled as the bound statement
/// carries them.
///
/// `READ_COMMITTED_SNAPSHOT ON` asks that each statement under `READ COMMITTED` read a
/// snapshot instead of taking shared locks; `ALLOW_SNAPSHOT_ISOLATION ON` asks that the
/// database keep the versions a `SNAPSHOT` transaction reads. Binding them changes nothing:
/// switching the option in the catalogue, and what the sessions already open see of it, is
/// the executor's job.
const VERSIONING_OPTIONS: [&str; 2] = ["READ_COMMITTED_SNAPSHOT", "ALLOW_SNAPSHOT_ISOLATION"];

/// The first word of the `SET`-less forms of `ALTER DATABASE` — `ADD FILE`, `REMOVE FILE`,
/// `MODIFY NAME`, `COLLATE` — which the parser hands over as an option named after that
/// word, since it does not keep the `SET` itself.
const CLAUSES_WITHOUT_SET: [&str; 4] = ["ADD", "REMOVE", "MODIFY", "COLLATE"];

/// The name the bound statement gives the termination clause, `WITH …`, when the statement
/// writes one.
const TERMINATION_OPTION: &str = "WITH";

/// Binds `ALTER DATABASE <name | CURRENT> SET <option> { ON | OFF } [WITH <termination>]`,
/// where `<option>` is one of the two versioning options of [`VERSIONING_OPTIONS`].
///
/// # The shape of the bound statement
///
/// [`DdlStatement::AlterDatabase`] carries the database and a list of `(name, value)` pairs.
/// This function fills the list in a fixed shape, so that the executor reads it without
/// re-parsing text:
///
/// - `name` is the database name as written and unquoted, or the current database of the
///   context when the statement writes the bare keyword `CURRENT` (in any case:
///   `current` binds the same). A delimited `[CURRENT]` or `"CURRENT"` is a database named
///   `CURRENT`, which the executor will not find (`bind_alter_database::current_*` tests).
/// - `options[0]` is `(OPTION, Some("ON" | "OFF"))`, the option name and its value both
///   upper-cased.
/// - `options[1]`, present when the statement writes a termination clause, is
///   `("WITH", Some(mode))` with `mode` one of `ROLLBACK IMMEDIATE`, `ROLLBACK AFTER <n>
///   SECONDS` (the `SECONDS` written back when the statement left it out) and `NO_WAIT`.
///   The clause is carried for both options and refused for neither: with
///   `ALLOW_SNAPSHOT_ISOLATION`, SQL Server refuses it while the statement runs, by 5083
///   (severity 16, state 1), in the five spellings above, `ON` and `OFF` alike, on a
///   database whose state the value leaves as it is; with `READ_COMMITTED_SNAPSHOT`, the
///   same five spellings are accepted and the option is switched. That refusal belongs to
///   the executor, next to the switch itself.
///
/// Whether the database exists is **not** checked here: SQL Server answers 5011 while the
/// statement runs, with the result sets of the statements before it already sent (module
/// header), and [`CatalogView`](crate::CatalogView) answers about tables and views, not
/// about databases.
///
/// # Errors
///
/// - 102, severity 15, when the value or the termination clause is malformed. The state
///   and the quoted token follow the shapes below, on a statement written on one line;
///   the line is the statement's, where SQL Server puts the line of the quoted token:
///
///   | written | near | state |
///   |---|---|:-:|
///   | `READ_COMMITTED_SNAPSHOT MAYBE` (a bare word other than `ON` / `OFF`) | `'MAYBE'` | 6 |
///   | `READ_COMMITTED_SNAPSHOT` (no value) | `'READ_COMMITTED_SNAPSHOT'` | 6 |
///   | `READ_COMMITTED_SNAPSHOT = ON` | `'READ_COMMITTED_SNAPSHOT'` | 6 |
///   | `READ_COMMITTED_SNAPSHOT 1`, `'ON'`, `[ON]` | `'1'`, `'ON'`, `'ON'` | 1 |
///   | `READ_COMMITTED_SNAPSHOT ON foo` | `'foo'` | 1 |
///   | `… ON WITH FOO`, `… WITH ROLLBACK AFTER x` | `'FOO'`, `'x'` | 1 |
///   | `… ON WITH ROLLBACK AFTER 5 MINUTES` | `'MINUTES'` | 1 |
///   | `… ON WITH ROLLBACK AFTER;` (no count) | `'AFTER'` | 1 |
///   | `… ON WITH;`, `… ON WITH ROLLBACK;` | `';'` | 1 |
///   | `… ON WITH ROLLBACK IMMEDIATE, ALLOW_SNAPSHOT_ISOLATION ON` | `','` | 1 |
///
/// - 50000, the internal error, naming what it refuses, for an option of
///   `ALTER DATABASE … SET` other than the two above (`MULTI_USER`, `RECOVERY SIMPLE`,
///   `COMPATIBILITY_LEVEL = 150`, an option that does not exist:
///   `bind_alter_database::other_set_option_names_itself_and_its_version`) and for the
///   `SET`-less clauses (`ADD FILE`, `MODIFY NAME`, `COLLATE`:
///   `bind_alter_database::alter_database_add_file_names_v2`): the message quotes the
///   clause as written and names the version that takes it (V2). A statement that writes a versioning option **and** another
///   option is refused the same way, naming the other option; one that writes the two
///   versioning options, or one of them twice, is refused naming the list (SQL Server answers
///   5082 on the two together, while the statement runs).
pub(crate) fn bind_alter_database(
    stmt: &AlterDatabaseStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let name = if !stmt.name.quoted && stmt.name.value.eq_ignore_ascii_case("CURRENT") {
        ctx.database.to_owned()
    } else {
        stmt.name.value.clone()
    };
    let [first, ..] = stmt.options.as_slice() else {
        // The parser refuses an `ALTER DATABASE d` that says nothing about the database.
        return Err(SqlError::from(InternalError::Bug(
            "bind: ALTER DATABASE without an option".to_owned(),
        )));
    };
    if CLAUSES_WITHOUT_SET
        .iter()
        .any(|clause| first.name.eq_ignore_ascii_case(clause))
    {
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind: ALTER DATABASE {} is not implemented yet (V2)",
            written_option(first)
        ))));
    }
    if let Some(other) = stmt
        .options
        .iter()
        .find(|option| !is_versioning_option(option))
    {
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind: ALTER DATABASE … SET {} is not implemented yet (V2); READ_COMMITTED_SNAPSHOT \
             and ALLOW_SNAPSHOT_ISOLATION are the options that bind",
            written_option(other)
        ))));
    }
    // A termination clause closes the statement: an option written after it is 102 near
    // the comma, before the list is looked at.
    if let Some((_, rest)) = stmt.options.split_last()
        && rest
            .iter()
            .any(|option| matches!(versioning_value(option), Ok((_, Some(_)))))
    {
        return Err(on_statement(syntax_error_near(","), &stmt.span));
    }
    let [option] = stmt.options.as_slice() else {
        let list = stmt
            .options
            .iter()
            .map(written_option)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind: ALTER DATABASE … SET {list} writes several versioning options in one \
             statement, which is not implemented yet; SQL Server answers 5082 on the two \
             together"
        ))));
    };
    let (value, termination) =
        versioning_value(option).map_err(|err| on_statement(err, &stmt.span))?;
    let mut options = vec![(option.name.to_ascii_uppercase(), Some(value.to_owned()))];
    if let Some(mode) = termination {
        options.push((TERMINATION_OPTION.to_owned(), Some(mode)));
    }
    Ok(BoundStatement::Ddl(DdlStatement::AlterDatabase {
        name,
        options,
    }))
}

/// Whether `option` names one of the two versioning options, in any case.
fn is_versioning_option(option: &DatabaseOption) -> bool {
    VERSIONING_OPTIONS
        .iter()
        .any(|name| option.name.eq_ignore_ascii_case(name))
}

/// The option as the statement wrote it: its name, then its value when it has one.
fn written_option(option: &DatabaseOption) -> String {
    match &option.value {
        Some(value) => format!("{} {value}", option.name),
        None => option.name.clone(),
    }
}

/// The value of a versioning option, `ON` or `OFF`, and the termination clause written after
/// it, as [`bind_alter_database`] carries it.
///
/// The parser hands the value over as the text of the tokens after the option name, joined
/// with one space, delimiters kept: `ON WITH ROLLBACK IMMEDIATE`, `'ON'`, `= ON`, `ON foo`.
/// This function reads that text word by word.
///
/// # Errors
///
/// 102, without a line (the caller puts the statement's): the table of
/// [`bind_alter_database`] gives the token quoted and the state for each malformed shape.
fn versioning_value(option: &DatabaseOption) -> SqlResult<(&'static str, Option<String>)> {
    let Some(text) = option.value.as_deref() else {
        return Err(syntax_error_in_option_position(&option.name));
    };
    let mut words = text.split(' ').filter(|word| !word.is_empty());
    let value = match words.next() {
        Some(word) if word.eq_ignore_ascii_case("ON") => "ON",
        Some(word) if word.eq_ignore_ascii_case("OFF") => "OFF",
        // `= ON`: the option name is quoted, as when the value is missing.
        Some("=") | None => return Err(syntax_error_in_option_position(&option.name)),
        Some(word) if is_bare_word(word) => return Err(syntax_error_in_option_position(word)),
        Some(token) => return Err(syntax_error_near(token)),
    };
    let termination = match words.next() {
        None => None,
        Some(word) if word.eq_ignore_ascii_case(TERMINATION_OPTION) => {
            Some(termination_mode(&mut words)?)
        }
        Some(token) => return Err(syntax_error_near(token)),
    };
    Ok((value, termination))
}

/// The termination mode written after `WITH`, normalised: `ROLLBACK IMMEDIATE`,
/// `ROLLBACK AFTER <n> SECONDS` or `NO_WAIT`.
///
/// # Errors
///
/// 102 state 1 near the token that does not fit, near `AFTER` when the count is missing,
/// near `;` when the clause stops after `WITH` or after `ROLLBACK`, near `,` when another
/// option follows the clause.
fn termination_mode<'a>(words: &mut impl Iterator<Item = &'a str>) -> SqlResult<String> {
    let mode = match words.next() {
        Some(word) if word.eq_ignore_ascii_case("NO_WAIT") => "NO_WAIT".to_owned(),
        Some(word) if word.eq_ignore_ascii_case("ROLLBACK") => match words.next() {
            Some(word) if word.eq_ignore_ascii_case("IMMEDIATE") => "ROLLBACK IMMEDIATE".to_owned(),
            Some(after) if after.eq_ignore_ascii_case("AFTER") => {
                let count = match words.next() {
                    Some(count) if count.bytes().all(|byte| byte.is_ascii_digit()) => count,
                    Some(token) => return Err(syntax_error_near(token)),
                    None => return Err(syntax_error_near(after)),
                };
                let unit = words.next();
                if let Some(word) = unit.filter(|word| !word.eq_ignore_ascii_case("SECONDS")) {
                    return Err(syntax_error_near(word));
                }
                format!("ROLLBACK AFTER {count} SECONDS")
            }
            Some(token) => return Err(syntax_error_near(token)),
            None => return Err(syntax_error_near(";")),
        },
        Some(token) => return Err(syntax_error_near(token)),
        None => return Err(syntax_error_near(";")),
    };
    match words.next() {
        None => Ok(mode),
        Some(token) => Err(syntax_error_near(token)),
    }
}

/// Whether `token` is a bare identifier or keyword, as opposed to a number, a string or a
/// delimited identifier: the parser keeps the delimiters of the last two in the text.
fn is_bare_word(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '@' || first == '#')
}

/// The 102 of a bare word met where an option name or its value was expected: state 6, the
/// word quoted. `word` is quoted as written.
fn syntax_error_in_option_position(word: &str) -> SqlError {
    let mut err = SqlError::incorrect_syntax_near(word, 0);
    err.state = 6;
    err
}

/// The 102 of a token met after a complete option: state 1, the token quoted without its
/// delimiters (`[ON]` and `'ON'` both quote `ON`).
fn syntax_error_near(token: &str) -> SqlError {
    let printed = token
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .or_else(|| {
            token
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .or_else(|| {
            token
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or(token);
    SqlError::incorrect_syntax_near(printed, 0)
}

/// Binds `CREATE TABLE t (…)` into the [`TableDef`] the catalogue takes.
///
/// The columns are resolved first and the name is looked up afterwards, in that order,
/// because SQL Server answers that way: `CREATE TABLE dbo.ex (a nosuch);` on an `ex` that
/// already exists answers 2715, not 2714. Inside
/// the column list, the first faulty column decides between 2705 and 2715 (module header).
///
/// # Errors
///
/// - 2715, on the line of the statement, when a column names a type `datatype.rs` does not
///   resolve. `#<n>` counts the columns of the statement from 1, whatever their type:
///   `(a int, b varchar(10), c nosuch)` answers `#3`.
/// - 2714, on the line of the statement, when [`CatalogView`](crate::CatalogView) resolves
///   the name already. The message prints the object part alone, delimiters removed:
///   `CREATE TABLE dbo.[ex2] (a int);` prints `'ex2'`.
/// - 448 when a column `COLLATE` names an unknown collation.
/// - 2705, on the line of the statement, for a column written twice; the message names the
///   later copy and the table without its schema (module header).
/// - the internal 50000 for a temporary table (`#t`), for a four-part name, for
///   `ROWGUIDCOL` and for a computed column: see [`table_name`] and [`column_of`].
/// - the internal 50000 standing in for 8111 when a `PRIMARY KEY` column writes `NULL`:
///   see [`apply_primary_key_not_null`].
/// - the internal 50000 standing in for 8110, 8112 and 1909, which count the keys of the
///   statement: see [`refuse_two_keys`] and [`refuse_duplicate_key_column`].
pub(crate) fn bind_create_table(
    stmt: &CreateTableStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let name = table_name(&stmt.name, ctx)?;
    // Before the column list: 8110 and 8112 win over 2705 and 2715 (`ddl_index.rs`).
    refuse_two_keys(stmt)?;
    let pk_clustered = primary_key_clustered_by_default(stmt);
    let mut columns = Vec::with_capacity(stmt.definition.columns.len());
    let mut constraints = Vec::new();
    let mut deferred = None;
    for (index, column) in stmt.definition.columns.iter().enumerate() {
        // `#<n>` of message 2715 is 1-based and counts the columns of the statement, whatever
        // their type (`ddl::tests::unknown_type_2715_numbers_the_column_and_names_the_statement`).
        let position = u32::try_from(index + 1).unwrap_or(u32::MAX);
        // The first fault of the column list wins, and on the column it lands on, 2705 comes
        // before the type is looked at (module header).
        if written_earlier(&stmt.definition.columns[..index], &column.name.value) {
            return Err(
                SqlError::duplicate_column_name(&column.name.value, &stmt.name.name.value)
                    .with_line(stmt.span.line),
            );
        }
        let (def, column_constraints) = column_of(
            column,
            position,
            &stmt.span,
            pk_clustered,
            ctx,
            &mut deferred,
        )?;
        columns.push(def);
        constraints.extend(column_constraints);
    }
    for constraint in &stmt.definition.constraints {
        constraints.push(table_constraint(
            constraint,
            pk_clustered,
            ctx,
            &stmt.span,
            &mut deferred,
        )?);
    }
    // After the column list and before the nullability of the key (`ddl_index.rs`).
    refuse_duplicate_key_column(&constraints, stmt.span.line)?;
    let written_null: Vec<&str> = stmt
        .definition
        .columns
        .iter()
        .filter(|column| {
            column
                .constraints
                .iter()
                .any(|constraint| matches!(constraint.kind, ColumnConstraintKind::Null))
        })
        .map(|column| column.name.value.as_str())
        .collect();
    apply_primary_key_not_null(
        &mut columns,
        &constraints,
        &written_null,
        &stmt.name.name.value,
    )?;
    check_table_expressions(stmt, &columns, ctx)?;

    if let Some(catalog) = ctx.catalog
        && catalog
            .resolve_table(&stmt.name, ctx.database, ctx.default_schema)
            .is_some()
    {
        return Err(
            SqlError::object_already_exists(&stmt.name.name.value).with_line(stmt.span.line)
        );
    }
    if let Some(err) = deferred {
        return Err(err);
    }

    Ok(BoundStatement::Ddl(DdlStatement::CreateTable {
        def: TableDef {
            name,
            columns,
            constraints,
        },
    }))
}

/// Checks each `DEFAULT` and `CHECK` of a `CREATE TABLE` by binding it, in written order:
/// [`check_default_expression`] against the type of its column, [`check_check_expression`]
/// against the columns of the statement, exposed under the name of the table.
fn check_table_expressions(
    stmt: &CreateTableStatement,
    columns: &[ColumnDef],
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    let bindings: Vec<ColumnBinding> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| ColumnBinding {
            column: vauban_catalog::ColumnId(i32::try_from(index + 1).unwrap_or(i32::MAX)),
            index,
            name: column.name.clone(),
            ty: column.ty.clone(),
            depth: 0,
        })
        .collect();
    let scope = Scope::over(Source::new(
        &stmt.name.name.value,
        Some(&stmt.name),
        &bindings,
        ctx.default_schema,
        ctx.database,
    ));
    for (column, def) in stmt.definition.columns.iter().zip(columns) {
        for constraint in &column.constraints {
            match &constraint.kind {
                ColumnConstraintKind::Default(expr) => {
                    check_default_expression(expr, &def.ty, stmt.span.line, ctx)?;
                }
                ColumnConstraintKind::Check { expr, .. } => {
                    check_check_expression(expr, &scope, ctx)?;
                }
                _ => {}
            }
        }
    }
    for constraint in &stmt.definition.constraints {
        if let TableConstraintKind::Check { expr, .. } = &constraint.kind {
            check_check_expression(expr, &scope, ctx)?;
        }
    }
    Ok(())
}

/// Binds `DROP TABLE [IF EXISTS] t1, t2`.
///
/// The names are resolved into three-part names and nothing else: a table that is not there
/// is 3701 while the statement runs (module header), and `IF EXISTS` is carried to the
/// executor untouched.
pub(crate) fn bind_drop_table(
    names: &[ObjectName],
    if_exists: bool,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        resolved.push(table_name(name, ctx)?);
    }
    Ok(BoundStatement::Ddl(DdlStatement::DropTable {
        names: resolved,
        if_exists,
    }))
}

// -------------------------------------------------------------------------------------------
// TRUNCATE TABLE
// -------------------------------------------------------------------------------------------

/// Binds `TRUNCATE TABLE t` into [`DdlStatement::TruncateTable`].
///
/// A name the catalogue does not resolve answers 4701 state 1 on the line of the statement
/// (`TRUNCATE TABLE dbo.nosuch;`). A table another table references through a `FOREIGN KEY`
/// binds: its 4712 is raised while the statement runs, against the catalogue of that moment
/// (`vauban-executor`, `dml/truncate.rs`), so a `TRUNCATE` the control of flow does not
/// reach raises nothing (`tests/catalog_view.rs`,
/// `truncate_of_a_referenced_table_binds_and_leaves_4712_to_the_execution`). A view or a
/// temporary table is the internal error 50000 naming V2, as for [`bind_drop_table`].
///
/// # Errors
///
/// - 4701 when the target is not in the catalogue, or is one of its internal tables;
/// - the internal 50000 for a view, a temporary table or a four-part name.
pub(crate) fn bind_truncate(
    table: &ObjectName,
    span: Span,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    let name = table_name(table, ctx).map_err(|err| on_statement(err, &span))?;
    // An internal table of the catalogue answers as a table that does not exist, as SQL
    // Server answers `TRUNCATE TABLE sys.objects` (`tests/catalog_view.rs`,
    // `a_write_of_an_internal_table_is_refused`).
    let resolved = catalog
        .resolve_table(table, ctx.database, ctx.default_schema)
        .filter(|resolved| {
            !resolved
                .table
                .is_some_and(|storage| catalog.is_internal_table(storage))
        })
        .ok_or_else(|| {
            SqlError::cannot_find_object_to_truncate(&table.name.value).with_line(span.line)
        })?;
    match resolved.kind {
        ResolvedTableKind::View => {
            return Err(not_yet(
                "bind_truncate: TRUNCATE TABLE on a view is not implemented yet (V2)",
            ));
        }
        ResolvedTableKind::Table => {}
    }
    Ok(BoundStatement::Ddl(DdlStatement::TruncateTable { name }))
}

/// The three-part name a written table name denotes: the database of the session when the
/// statement left that part out, its default schema when it left that one out.
///
/// # Errors
///
/// The internal 50000 for the two shapes VaubanDB does not serve yet:
///
/// - a temporary table (`#t`, `##t`), whose rows live in `tempdb` and whose scope follows
///   the connection or the batch. A delimited `[#t]` is the same table.
/// - a four-part name (`srv.db.dbo.t`), which names a linked server. SQL Server answers
///   error 117 (the object name has more than the maximum of 2 prefixes) at compilation,
///   for `CREATE TABLE` and for `DROP TABLE` alike. `vauban-errors` carries 117 with the
///   `column` filling and a maximum of 3 ([`SqlError::too_many_column_prefixes`]), not this
///   one: the `object` filling has no constructor, and dropping the server part in silence
///   would bind a statement other than the one the client wrote.
///
/// `pub(crate)` for `ddl_index.rs`, which resolves the table of a `CREATE`/`DROP INDEX` the
/// same way and refuses the same two shapes.
pub(crate) fn table_name(name: &ObjectName, ctx: &BindContext<'_>) -> SqlResult<QualifiedName> {
    if name.name.value.starts_with('#') {
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind: the temporary table '{}' is not implemented yet",
            name.name.value
        ))));
    }
    if name.server.is_some() {
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind: the four-part name '{}' names a linked server, which is not implemented \
             yet; SQL Server answers 117, whose object filling has no constructor yet",
            written(name)
        ))));
    }
    Ok(QualifiedName {
        database: part(name.database.as_ref(), ctx.database),
        schema: part(name.schema.as_ref(), ctx.default_schema),
        name: name.name.value.clone(),
    })
}

/// The 2715 of a column list, which sends state **6** where the `DECLARE` of `datatype.rs`
/// sends state 3 ([`SqlError::cannot_find_data_type_in_table`] against
/// [`SqlError::cannot_find_data_type`]). Same number, same sentence, same
/// `#<n>`: the state is the whole difference, and it is pinned by
/// `ddl::tests::unknown_type_is_2715`.
///
/// The placeholder 2715 of an out-of-range parameter (`out_of_range` calls `unknown` in
/// `datatype.rs`) takes the state 6 as well: it carries the same message and lands in the same
/// column list. An error of another number is carried through untouched.
fn in_column_list(err: SqlError, position: u32, name: &str) -> SqlError {
    if err.number == 2715 {
        SqlError::cannot_find_data_type_in_table(position, name)
    } else {
        err
    }
}

/// Whether one of the columns written before this one carries `name`.
///
/// The comparison ignores case, as SQL Server's does under the default collation:
/// `CREATE TABLE dbo.m2 (a int, A int);` answers 2705 naming `'A'`, the occurrence written
/// last (`ddl::tests::duplicate_column_is_2705`).
fn written_earlier(earlier: &[AstColumnDef], name: &str) -> bool {
    earlier
        .iter()
        .any(|column| column.name.value.eq_ignore_ascii_case(name))
}

/// The written part when there is one, `default` otherwise: `db..t` takes the schema of the
/// session exactly as `t` does.
fn part(part: Option<&Ident>, default: &str) -> String {
    part.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
}

/// The parts of a name joined by dots, delimiters removed, as the messages print it.
///
/// `pub(crate)` for `ddl_index.rs`, whose 1088, 8110 and 8112 print the name that way.
pub(crate) fn written(name: &ObjectName) -> String {
    [
        name.server.as_ref(),
        name.database.as_ref(),
        name.schema.as_ref(),
        Some(&name.name),
    ]
    .into_iter()
    .flatten()
    .map(|ident| ident.value.as_str())
    .collect::<Vec<_>>()
    .join(".")
}

/// One column of the `CREATE TABLE`, and the constraints written inside it that the
/// catalogue holds at table level.
///
/// A column is nullable unless `NOT NULL` was written: after
/// `CREATE TABLE dbo.p1 (a int, b int NULL, c int NOT NULL, …)`, `sys.columns.is_nullable`
/// answers 1, 1 and 0.
///
/// The first unnamed `DEFAULT` fills [`ColumnDef::default`], which is the field the
/// catalogue reads. A later unnamed `DEFAULT` on the same column is pushed as
/// [`ConstraintDef::Default`] without a name so the catalogue can refuse the pair
/// (`tests/bind_ddl.rs`). A `CONSTRAINT df DEFAULT 0` becomes a
/// [`ConstraintDef::Default`] so that the name the client chose survives to
/// `sys.default_constraints`. Neither is evaluated here: [`bind_create_table`] binds each
/// expression and refuses the shapes [`check_default_expression`] lists, and the catalogue
/// holds the text [`stored_text_or_defer`] writes for it.
///
/// # Errors
///
/// 2715 for an unresolvable type, 448 for an unknown `COLLATE`, and the internal 50000 for
/// `ROWGUIDCOL`, which the parser reads and which no `*Def` of the catalogue holds: binding
/// it would drop it in silence.
///
/// `pk_clustered` is what a `PRIMARY KEY` writing no clustering takes; it is `true` or
/// `false` by the rest of the statement, see [`primary_key_clustered_by_default`].
fn column_of(
    column: &AstColumnDef,
    position: u32,
    statement: &Span,
    pk_clustered: bool,
    ctx: &BindContext<'_>,
    deferred: &mut Option<SqlError>,
) -> SqlResult<(ColumnDef, Vec<ConstraintDef>)> {
    if column.computed.is_some() {
        // Before the type is resolved: `AS a + 1` writes no type, and `parse_column_def` fills
        // the hole with an empty `DataType` that 2715 would print as a blank name.
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind: the computed column '{}' is not implemented yet",
            column.name.value
        ))));
    }
    let ty = resolve_data_type(&column.ty, position)
        .map_err(|err| in_column_list(err, position, &column.ty.name))
        .map_err(|err| on_statement(err, statement))?;
    // `sysname` is `NOT NULL` unless `NULL` is written explicitly, unlike the
    // other types whose default is nullable.
    let mut nullable = !column.ty.name.eq_ignore_ascii_case("sysname");
    let mut default = None;
    let mut constraints = Vec::new();
    for constraint in &column.constraints {
        let name = constraint.name.as_ref().map(|ident| ident.value.clone());
        match &constraint.kind {
            ColumnConstraintKind::Null => nullable = true,
            ColumnConstraintKind::NotNull => nullable = false,
            ColumnConstraintKind::Default(expr) => {
                let text = stored_text_or_defer(expr, ExpressionKind::Value, deferred);
                if name.is_none() && default.is_none() {
                    default = Some(text);
                } else {
                    constraints.push(ConstraintDef::Default {
                        name,
                        column: column.name.value.clone(),
                        expr: text,
                    });
                }
            }
            ColumnConstraintKind::PrimaryKey { clustering, order } => {
                constraints.push(ConstraintDef::PrimaryKey {
                    name,
                    columns: vec![key_column(&column.name.value, *order)],
                    clustered: is_clustered(*clustering, pk_clustered),
                });
            }
            ColumnConstraintKind::Unique { clustering, order } => {
                constraints.push(ConstraintDef::Unique {
                    name,
                    columns: vec![key_column(&column.name.value, *order)],
                    clustered: is_clustered(*clustering, false),
                });
            }
            ColumnConstraintKind::ForeignKey(reference) => {
                constraints.push(foreign_key(
                    name,
                    vec![column.name.value.clone()],
                    reference,
                    ctx,
                    statement,
                )?);
            }
            ColumnConstraintKind::Check { expr, .. } => constraints.push(ConstraintDef::Check {
                name,
                expr: stored_text_or_defer(expr, ExpressionKind::Predicate, deferred),
            }),
            ColumnConstraintKind::RowGuidCol => {
                return Err(SqlError::from(InternalError::Bug(format!(
                    "bind: ROWGUIDCOL on column '{}' is not implemented yet",
                    column.name.value
                ))));
            }
        }
    }
    let mut info = TypeInfo::new(ty, nullable);
    if let Some(collation) = &column.collation {
        info.collation =
            Some(Collation::parse(collation).map_err(|err| on_statement(err, statement))?);
    }
    Ok((
        ColumnDef {
            name: column.name.value.clone(),
            ty: info,
            default,
            identity: column.identity.as_ref().map(|identity| IdentitySpec {
                seed: identity.seed.unwrap_or(IdentitySpec::default().seed),
                increment: identity
                    .increment
                    .unwrap_or(IdentitySpec::default().increment),
            }),
            computed: None,
        },
        constraints,
    ))
}

/// One constraint written at table level.
///
/// # Errors
///
/// Nothing is refused here today: each variant of `parser::TableConstraintKind` has its
/// counterpart in `catalog::ConstraintDef`. The signature keeps the `SqlResult` of its
/// caller so that a check added here does not change it.
fn table_constraint(
    constraint: &TableConstraint,
    pk_clustered: bool,
    ctx: &BindContext<'_>,
    span: &Span,
    deferred: &mut Option<SqlError>,
) -> SqlResult<ConstraintDef> {
    let name = constraint.name.as_ref().map(|ident| ident.value.clone());
    Ok(match &constraint.kind {
        TableConstraintKind::PrimaryKey {
            columns,
            clustering,
        } => ConstraintDef::PrimaryKey {
            name,
            columns: columns.iter().map(sorted_column).collect(),
            clustered: is_clustered(*clustering, pk_clustered),
        },
        TableConstraintKind::Unique {
            columns,
            clustering,
        } => ConstraintDef::Unique {
            name,
            columns: columns.iter().map(sorted_column).collect(),
            clustered: is_clustered(*clustering, false),
        },
        TableConstraintKind::ForeignKey { columns, reference } => foreign_key(
            name,
            columns.iter().map(|ident| ident.value.clone()).collect(),
            reference,
            ctx,
            span,
        )?,
        TableConstraintKind::Check { expr, .. } => ConstraintDef::Check {
            name,
            expr: stored_text_or_defer(expr, ExpressionKind::Predicate, deferred),
        },
    })
}

/// The text the catalogue stores for `expr` ([`stored_text`]). When it cannot be written,
/// the error is kept in `deferred` for the end of the statement, so that the refusals the
/// binding of the expression raises come first, and the text is empty.
pub(crate) fn stored_text_or_defer(
    expr: &Expr,
    kind: ExpressionKind,
    deferred: &mut Option<SqlError>,
) -> String {
    match stored_text(expr, kind) {
        Ok(text) => text,
        Err(err) => {
            deferred.get_or_insert(err);
            String::new()
        }
    }
}

/// The action of the catalogue for the `ON DELETE` or `ON UPDATE` a statement wrote,
/// `NO ACTION` for a clause left out.
pub(crate) fn catalog_action(action: Option<RefAction>) -> CatalogAction {
    match action {
        None | Some(RefAction::NoAction) => CatalogAction::NoAction,
        Some(RefAction::Cascade) => CatalogAction::Cascade,
        Some(RefAction::SetNull) => CatalogAction::SetNull,
        Some(RefAction::SetDefault) => CatalogAction::SetDefault,
    }
}

/// A `FOREIGN KEY` / `REFERENCES`, with the actions the statement left out filled with the
/// `NO ACTION` SQL Server applies to `ON DELETE` and `ON UPDATE`.
///
/// The referenced table is resolved like a `FROM` table: the current database and the
/// default schema of the session fill the parts the statement left out. A reference that
/// names another database answers 1763 on the line of the statement.
///
/// # Errors
///
/// - 1763 when the referenced table names a database other than the current one;
/// - the internal 50000 for a temporary table or a four-part name on the reference.
fn foreign_key(
    name: Option<String>,
    columns: Vec<String>,
    reference: &ForeignKeyRef,
    ctx: &BindContext<'_>,
    span: &Span,
) -> SqlResult<ConstraintDef> {
    let referenced = table_name(&reference.table, ctx).map_err(|err| on_statement(err, span))?;
    if !referenced.database.eq_ignore_ascii_case(ctx.database) {
        let printed = format!(
            "{}.{}.{}",
            referenced.database, referenced.schema, referenced.name
        );
        return Err(SqlError::new(
            1763,
            16,
            0,
            format!(
                "Cross-database foreign key references are not supported. Foreign key '{printed}'."
            ),
        )
        .with_line(span.line));
    }
    Ok(ConstraintDef::ForeignKey {
        name,
        columns,
        referenced,
        referenced_columns: reference
            .columns
            .iter()
            .map(|ident| ident.value.clone())
            .collect(),
        on_delete: catalog_action(reference.on_delete),
        on_update: catalog_action(reference.on_update),
    })
}

/// The single key column of a column-level `PRIMARY KEY` or `UNIQUE`.
fn key_column(column: &str, order: Option<SortDirection>) -> SortedColumn {
    SortedColumn {
        column: column.to_owned(),
        descending: order == Some(SortDirection::Desc),
    }
}

/// A key column of a table-level constraint, in the terms of the catalogue.
fn sorted_column(column: &IndexColumn) -> SortedColumn {
    SortedColumn {
        column: column.name.value.clone(),
        descending: column.desc,
    }
}

/// Whether a key is clustered: what was written, or `default` when nothing was.
///
/// The default of a `UNIQUE` constraint is `NONCLUSTERED`: after
/// `CREATE TABLE dbo.uq1 (a int UNIQUE, b int);` the table stays a `HEAP`. The default of
/// a `PRIMARY KEY` is `CLUSTERED` when no constraint of the statement writes `CLUSTERED`,
/// and `NONCLUSTERED` when one does, so its caller computes it
/// ([`primary_key_clustered_by_default`], `ddl_index.rs`).
fn is_clustered(clustering: Option<Clustering>, default: bool) -> bool {
    match clustering {
        Some(Clustering::Clustered) => true,
        Some(Clustering::NonClustered) => false,
        None => default,
    }
}

/// Makes the columns of the `PRIMARY KEY` not nullable when their declaration wrote no
/// nullability, and refuses the ones that wrote `NULL`.
///
/// `CREATE TABLE dbo.p1 (…, d int PRIMARY KEY, …)` and
/// `CREATE TABLE dbo.p3 (a int, b int, PRIMARY KEY (a))` both answer
/// `sys.columns.is_nullable` = 0 on the key column, where
/// `CREATE TABLE dbo.p2 (a int UNIQUE, b int)` answers 1 on both of its columns — a
/// `UNIQUE` key stays nullable, which is what tells this rule from "a key column is not
/// nullable". Neither shape writes a nullability on the key column.
///
/// A key column that **writes** `NULL` is not silently turned around: SQL Server answers
/// 8111 state 1 (a `PRIMARY KEY` cannot be defined on a nullable column) on
/// `(a int NULL PRIMARY KEY)`, `(a int PRIMARY KEY NULL)` and
/// `(a int NULL, PRIMARY KEY (a))`, where `(a int NULL, UNIQUE (a))` creates the table.
/// 8111 has no constructor in `vauban-errors`, so the refusal is the internal 50000 that
/// names the number (`ddl::tests::primary_key_on_a_column_written_null_is_refused`).
///
/// Names are matched without regard to ASCII case; matching them under the collation of the
/// database is `names.rs`'s business, and a `CREATE TABLE` writes both names itself.
fn apply_primary_key_not_null(
    columns: &mut [ColumnDef],
    constraints: &[ConstraintDef],
    written_null: &[&str],
    table: &str,
) -> SqlResult<()> {
    for constraint in constraints {
        let ConstraintDef::PrimaryKey { columns: keys, .. } = constraint else {
            continue;
        };
        for key in keys {
            if written_null
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&key.column))
            {
                return Err(SqlError::from(InternalError::Bug(format!(
                    "bind: column '{}' of table '{table}' writes NULL and is part of the \
                     PRIMARY KEY; SQL Server answers 8111, which is not implemented yet",
                    key.column
                ))));
            }
            for column in columns.iter_mut() {
                if column.name.eq_ignore_ascii_case(&key.column) {
                    column.ty.nullable = false;
                }
            }
        }
    }
    Ok(())
}

/// Puts the line of the statement on `err`.
///
/// 2705, 2714 and 2715 answer the line the statement starts on, with the node they name
/// several lines below it (module header); the two the binder raises are pinned by
/// `ddl::tests::duplicate_name_2714_carries_the_line_of_the_statement` and
/// `ddl::tests::unknown_type_2715_numbers_the_column_and_names_the_statement`.
fn on_statement(err: SqlError, statement: &Span) -> SqlError {
    err.with_line(statement.line)
}

// -------------------------------------------------------------------------------------------
// The object expressions of a table definition
// -------------------------------------------------------------------------------------------

/// The errors of a binding that a `DEFAULT` or a `CHECK` is refused for: an unknown column or
/// qualifier, an undeclared `@variable`, a value where a condition is expected, two operands
/// that do not compare (`ddl::tests`, `a_check_that_does_not_bind_is_refused`). Any other
/// error of the binding leaves the statement to bind: a function the binder does not know
/// (`NEWSEQUENTIALID()`, `FORMAT`, `CHECKSUM`) would otherwise refuse a table SQL Server
/// creates (`ddl::tests`, `a_function_the_binder_does_not_know_is_not_refused`). Two
/// exceptions, [`is_refused`], leave the statement to bind as well.
const REFUSED_BINDING_ERRORS: [u32; 5] = [137, 206, 207, 4104, 4145];

/// Whether `err`, raised by binding `expr`, refuses the `DEFAULT` or `CHECK`: a number of
/// [`REFUSED_BINDING_ERRORS`], except a 137 of an expression that reads a `@@` global the
/// binder does not know, and a 206 between `uniqueidentifier` and a binary type, which
/// SQL Server converts (`ddl::tests`, `a_global_or_a_guid_to_binary_comparison_is_not_refused`).
fn is_refused(err: &SqlError, expr: &Expr) -> bool {
    if !REFUSED_BINDING_ERRORS.contains(&err.number) {
        return false;
    }
    match err.number {
        137 => !reads_a_global(expr),
        206 => {
            let message = err.message.to_ascii_lowercase();
            !(message.contains("uniqueidentifier") && message.contains("binary"))
        }
        _ => true,
    }
}

/// Whether `expr` reads a `@@` global.
fn reads_a_global(expr: &Expr) -> bool {
    first_in(expr, &mut |node| match node {
        Expr::Variable { name, .. } if name.starts_with("@@") => Some(()),
        _ => None,
    })
    .is_some()
}

/// The date-part functions, whose first argument is a date part (`day`, `yy`) and not a
/// column.
const DATE_PART_FUNCTIONS: [&str; 7] = [
    "DATEADD",
    "DATEDIFF",
    "DATEDIFF_BIG",
    "DATENAME",
    "DATEPART",
    "DATETRUNC",
    "DATE_BUCKET",
];

/// Checks the expression of a `DEFAULT` written for a column of type `column` by binding it.
///
/// Errors, in this order, on the line of the node unless said otherwise (with the expression
/// on line 5 of a batch whose statement starts on line 2, the first two answer 5):
///
/// | written | refusal |
/// |---|---|
/// | `b int DEFAULT (SELECT 1)` | 1046, severity 15, state 1 |
/// | `b int DEFAULT a`, a column of the table or an unknown name | 128, severity 15, state 1 |
/// | `a int DEFAULT @x` | 137, as `bind_expr` raises it |
/// | `a date DEFAULT 1`, `a int DEFAULT NEWID()` | 206 state 2, the value's type named first, on the line of the statement |
///
/// The first argument of a date-part function (`DATEADD(day, 1, GETDATE())`) is a date part,
/// not a name. A type written between brackets (`CONVERT([bit],(0))`) is read without them.
///
/// The 206 is raised for the pairs of [`refuses_default_conversion`] alone, and, as SQL
/// Server raises it, at compilation: no result set of the batch is sent and the batch stops
/// (`ddl::tests`, `a_default_of_a_type_that_does_not_convert_is_206`). The untyped `NULL` is
/// exempt.
///
/// # Errors
///
/// The refusals of the table above.
pub(crate) fn check_default_expression(
    expr: &Expr,
    column: &TypeInfo,
    statement_line: u32,
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    let mut expr = expr.clone();
    unbracket_type_names(&mut expr);
    refuse_subquery(&expr)?;
    if let Some(column) = first_column_reference(&expr)? {
        return Err(column_not_permitted(&column.name.value).with_line(column.span.line));
    }
    let local = BindContext {
        text: ctx.text,
        catalog: ctx.catalog,
        database: ctx.database,
        default_schema: ctx.default_schema,
        variables: &NoVariables,
        options: ctx.options,
    };
    let bound = match bind_expr(&expr, &local, &Scope::empty()) {
        Ok(bound) => bound,
        Err(err) if is_refused(&err, &expr) => return Err(err),
        Err(_) => return Ok(()),
    };
    if !is_untyped_null(&expr) && refuses_default_conversion(&bound.ty.ty, &column.ty) {
        return Err(
            SqlError::operand_type_clash(bound.ty.ty.name(), column.ty.name())
                .with_line(statement_line),
        );
    }
    Ok(())
}

/// Whether a `DEFAULT` of type `value` on a column of type `column` is refused with 206.
///
/// The pairs below are the ones SQL Server refuses with 206 on
/// `CREATE TABLE t (c <column> DEFAULT <value>)`; a pair outside them is not refused here,
/// `datetime` towards `int` included, which SQL Server refuses with 257 (`ddl::tests`,
/// `a_default_of_a_type_that_does_not_convert_is_206` and
/// `a_default_of_an_untested_pair_is_not_refused`):
///
/// - `bit`, `tinyint`, `smallint`, `int`, `bigint`, `decimal`, `numeric` towards `date`,
///   `time`, `datetime2`, `datetimeoffset`, `uniqueidentifier`;
/// - `uniqueidentifier` towards `date`, `time`, `datetime2`, `datetimeoffset`, `bit`,
///   `tinyint`, `smallint`, `int`, `bigint`, `decimal`, `numeric`, `float`, `money`;
/// - `datetime` towards `uniqueidentifier`.
fn refuses_default_conversion(value: &SqlType, column: &SqlType) -> bool {
    let exact = |ty: &SqlType| {
        matches!(
            ty,
            SqlType::Bit
                | SqlType::TinyInt
                | SqlType::SmallInt
                | SqlType::Int
                | SqlType::BigInt
                | SqlType::Decimal { .. }
                | SqlType::Numeric { .. }
        )
    };
    let dated = |ty: &SqlType| {
        matches!(
            ty,
            SqlType::Date | SqlType::Time(_) | SqlType::DateTime2(_) | SqlType::DateTimeOffset(_)
        )
    };
    match value {
        ty if exact(ty) => dated(column) || matches!(column, SqlType::UniqueIdentifier),
        SqlType::UniqueIdentifier => {
            dated(column) || exact(column) || matches!(column, SqlType::Float | SqlType::Money)
        }
        SqlType::DateTime => matches!(column, SqlType::UniqueIdentifier),
        _ => false,
    }
}

/// Checks the predicate of a `CHECK` by binding it against `scope`, the columns of its
/// table: 1046 for a subquery, then the errors of
/// [`bind_condition`] that [`REFUSED_BINDING_ERRORS`] lists (207 for an unknown column, 4104
/// for an unknown qualifier, 4145 for a value that is not a condition, 206 for operands that
/// do not compare), each on the line of its node (`ddl::tests`,
/// `a_check_that_does_not_bind_is_refused`). A type written between brackets is read
/// without them, as for [`check_default_expression`].
///
/// # Errors
///
/// The numbers above.
pub(crate) fn check_check_expression(
    expr: &Expr,
    scope: &Scope,
    ctx: &BindContext<'_>,
) -> SqlResult<()> {
    let mut expr = expr.clone();
    unbracket_type_names(&mut expr);
    refuse_subquery(&expr)?;
    match bind_condition(&expr, ctx, scope) {
        Err(err) if is_refused(&err, &expr) => Err(err),
        _ => Ok(()),
    }
}

/// 1046 on the line of the first subquery of `expr`, `Ok` otherwise.
fn refuse_subquery(expr: &Expr) -> SqlResult<()> {
    let found = first_in(expr, &mut |node| match node {
        Expr::Exists(query, _) | Expr::Subquery(query, _) => Some(query.span.line),
        Expr::Quantified { subquery, .. } => Some(subquery.span.line),
        Expr::In {
            list: InList::Subquery(query),
            ..
        } => Some(query.span.line),
        _ => None,
    });
    match found {
        Some(line) => Err(subquery_not_allowed().with_line(line)),
        None => Ok(()),
    }
}

/// The first name of `expr` that is a column: neither a niladic function
/// (`CURRENT_TIMESTAMP`, `USER`) nor the date part of a date-part function.
fn first_column_reference(expr: &Expr) -> SqlResult<Option<&ColumnRef>> {
    let mut date_parts: Vec<&ColumnRef> = Vec::new();
    let mut columns = Vec::new();
    first_in(expr, &mut |node| {
        match node {
            Expr::Function { name, args, .. }
                if name.schema.is_none()
                    && DATE_PART_FUNCTIONS
                        .iter()
                        .any(|function| function.eq_ignore_ascii_case(&name.name.value)) =>
            {
                if let Some(Expr::Column(part)) = args.first() {
                    date_parts.push(part);
                }
            }
            Expr::Column(column) => columns.push(column),
            _ => {}
        }
        None::<()>
    });
    for column in columns {
        if date_parts.iter().any(|part| std::ptr::eq(*part, column)) {
            continue;
        }
        if column.qualifier.is_none() && bind_niladic(&column.name, &column.span)?.is_some() {
            continue;
        }
        return Ok(Some(column));
    }
    Ok(None)
}

/// The first answer of `visit` over the nodes of `expr`, read in written order; the
/// subqueries are visited and not entered.
fn first_in<'e, T>(expr: &'e Expr, visit: &mut impl FnMut(&'e Expr) -> Option<T>) -> Option<T> {
    let mut pending = vec![expr];
    while let Some(node) = pending.pop() {
        if let Some(found) = visit(node) {
            return Some(found);
        }
        let mut children: Vec<&Expr> = Vec::new();
        match node {
            Expr::Nested(inner, _)
            | Expr::Unary { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::Collate { expr: inner, .. }
            | Expr::Quantified { expr: inner, .. }
            | Expr::Assign { value: inner, .. } => children.push(inner),
            Expr::Binary { left, right, .. } => children.extend([left.as_ref(), right.as_ref()]),
            Expr::Function { args, .. } => children.extend(args.iter()),
            Expr::Convert { expr, style, .. } => {
                children.push(expr);
                children.extend(style.as_deref());
            }
            Expr::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                children.extend(operand.as_deref());
                for arm in arms {
                    children.extend([&arm.when, &arm.then]);
                }
                children.extend(else_.as_deref());
            }
            Expr::In { expr, list, .. } => {
                children.push(expr);
                if let InList::Exprs(items) = list {
                    children.extend(items.iter());
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                children.extend([expr.as_ref(), pattern.as_ref()]);
                children.extend(escape.as_deref());
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                children.extend([expr.as_ref(), low.as_ref(), high.as_ref()]);
            }
            Expr::Literal(..)
            | Expr::Column(_)
            | Expr::Variable { .. }
            | Expr::Exists(..)
            | Expr::Subquery(..)
            | Expr::NextValueFor { .. }
            | Expr::InvalidNiladic { .. }
            | Expr::Placeholder(_) => {}
        }
        pending.extend(children.into_iter().rev());
    }
    None
}

/// Error 128 for `name`, a column named in a `DEFAULT`.
fn column_not_permitted(name: &str) -> SqlError {
    SqlError::column_name_not_permitted(name)
}

/// Error 1046, a subquery in a `DEFAULT` or a `CHECK`.
fn subquery_not_allowed() -> SqlError {
    SqlError::subquery_not_allowed_here()
}

#[cfg(test)]
mod tests;
