//! Binding of `CREATE`/`DROP DATABASE`, `CREATE`/`DROP TABLE` and `USE`.
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
//!
//! So the type of a column is resolved here (2715), and the existence of a table or of a
//! database is **not** looked at for a `DROP`: the executor and the catalogue answer 3701
//! when they cannot find it, which is where SQL Server answers it too.
//!
//! 2714 is the one deliberate difference from SQL Server: the binder refuses a
//! `CREATE TABLE` whose name the catalogue already resolves, **before** anything is
//! written, where SQL Server refuses it while it runs. A batch that mixes a result set and
//! a duplicate `CREATE TABLE` therefore answers one result set on SQL Server and zero here
//! (`ddl::tests::create_table_duplicate_name_is_2714`).
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
    ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, SortedColumn, TableDef,
};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{
    Clustering, ColumnConstraintKind, ColumnDef as AstColumnDef, CreateDatabaseStatement,
    CreateTableStatement, DatabaseOption, ForeignKeyRef, Ident, IndexColumn, ObjectName, RefAction,
    SortDirection, Span, TableConstraint, TableConstraintKind,
};
use vauban_types::{Collation, TypeInfo};

use crate::bound::{BoundStatement, DdlStatement};
use crate::context::BindContext;
use crate::datatype::resolve_data_type;
use crate::ddl_index::{
    primary_key_clustered_by_default, refuse_duplicate_key_column, refuse_two_keys,
};

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
        let (def, column_constraints) = column_of(column, position, &stmt.span, pk_clustered)?;
        columns.push(def);
        constraints.extend(column_constraints);
    }
    for constraint in &stmt.definition.constraints {
        constraints.push(table_constraint(constraint, pk_clustered)?);
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

    if let Some(catalog) = ctx.catalog
        && catalog
            .resolve_table(&stmt.name, ctx.database, ctx.default_schema)
            .is_some()
    {
        return Err(
            SqlError::object_already_exists(&stmt.name.name.value).with_line(stmt.span.line)
        );
    }

    Ok(BoundStatement::Ddl(DdlStatement::CreateTable {
        def: TableDef {
            name,
            columns,
            constraints,
        },
    }))
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
/// An unnamed `DEFAULT` fills [`ColumnDef::default`], which is the field the catalogue
/// reads; a `CONSTRAINT df DEFAULT 0` becomes a [`ConstraintDef::Default`] instead, so that
/// the name the client chose survives to `sys.default_constraints`. Neither is evaluated
/// here: the catalogue holds the `parser::Expr`.
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
    let mut nullable = true;
    let mut default = None;
    let mut constraints = Vec::new();
    for constraint in &column.constraints {
        let name = constraint.name.as_ref().map(|ident| ident.value.clone());
        match &constraint.kind {
            ColumnConstraintKind::Null => nullable = true,
            ColumnConstraintKind::NotNull => nullable = false,
            ColumnConstraintKind::Default(expr) => match name {
                Some(name) => constraints.push(ConstraintDef::Default {
                    name: Some(name),
                    column: column.name.value.clone(),
                    expr: expr.clone(),
                }),
                None => default = Some(expr.clone()),
            },
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
                ));
            }
            ColumnConstraintKind::Check { expr, .. } => constraints.push(ConstraintDef::Check {
                name,
                expr: expr.clone(),
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
            computed: column.computed.clone(),
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
fn table_constraint(constraint: &TableConstraint, pk_clustered: bool) -> SqlResult<ConstraintDef> {
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
        ),
        TableConstraintKind::Check { expr, .. } => ConstraintDef::Check {
            name,
            expr: expr.clone(),
        },
    })
}

/// A `FOREIGN KEY` / `REFERENCES`, with the actions the statement left out filled with the
/// `NO ACTION` SQL Server applies to `ON DELETE` and `ON UPDATE`.
///
/// The referenced name keeps the parts that were written and leaves the others empty: which
/// database and which schema a `REFERENCES other (b)` reaches, and whether the table is
/// there, is the catalogue's to decide when it creates the constraint.
fn foreign_key(
    name: Option<String>,
    columns: Vec<String>,
    reference: &ForeignKeyRef,
) -> ConstraintDef {
    ConstraintDef::ForeignKey {
        name,
        columns,
        referenced: QualifiedName {
            database: part(reference.table.database.as_ref(), ""),
            schema: part(reference.table.schema.as_ref(), ""),
            name: reference.table.name.value.clone(),
        },
        referenced_columns: reference
            .columns
            .iter()
            .map(|ident| ident.value.clone())
            .collect(),
        on_delete: reference.on_delete.unwrap_or(RefAction::NoAction),
        on_update: reference.on_update.unwrap_or(RefAction::NoAction),
    }
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

#[cfg(test)]
mod tests {
    use vauban_catalog::{ConstraintDef, ObjectId, QualifiedName, TableDef};
    use vauban_errors::SqlError;
    use vauban_parser::{ObjectName, ParseOptions, RefAction, Statement, parse_batch};
    use vauban_types::{Collation, Len, SqlType};

    use crate::bound::{BoundStatement, DdlStatement};
    use crate::context::{
        BindContext, CatalogView, ResolvedTable, ResolvedTableKind, SessionOptions,
    };
    use crate::statement::bind;

    /// A catalogue that resolves one table, `ex`, and nothing else.
    struct OneTable;

    impl CatalogView for OneTable {
        fn resolve_table(
            &self,
            name: &ObjectName,
            _database: &str,
            _default_schema: &str,
        ) -> Option<ResolvedTable> {
            name.name
                .value
                .eq_ignore_ascii_case("ex")
                .then(|| ResolvedTable {
                    object: ObjectId(42),
                    table: None,
                    columns: Vec::new(),
                    kind: ResolvedTableKind::Table,
                })
        }
    }

    /// Binds the single statement of `text` without a catalogue.
    fn bound(text: &str) -> Result<BoundStatement, SqlError> {
        bind_with(text, None)
    }

    /// Binds the single statement of `text` against `catalog`.
    fn bind_with(
        text: &str,
        catalog: Option<&dyn CatalogView>,
    ) -> Result<BoundStatement, SqlError> {
        let batch = parse_batch(text, &ParseOptions::default())
            .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
        let [statement] = batch.statements.as_slice() else {
            unreachable!("{text} is one statement")
        };
        let mut ctx = BindContext::scalar(text, SessionOptions::default());
        ctx.catalog = catalog;
        bind(statement, &ctx)
    }

    /// The `TableDef` of a bound `CREATE TABLE`.
    fn table_def(text: &str) -> TableDef {
        match bound(text) {
            Ok(BoundStatement::Ddl(DdlStatement::CreateTable { def })) => def,
            other => unreachable!("{text} binds to a CreateTable, got {other:?}"),
        }
    }

    /// The error of a statement that does not bind.
    fn err(text: &str) -> SqlError {
        bound(text).expect_err("this statement does not bind")
    }

    #[test]
    fn create_table_binds_to_ddl() {
        let def = table_def("CREATE TABLE dbo.t (a int NOT NULL, b nvarchar(20));");
        assert_eq!(
            def.name,
            QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "t".to_owned(),
            }
        );
        let [a, b] = def.columns.as_slice() else {
            unreachable!("two columns, got {:?}", def.columns)
        };
        assert_eq!(a.name, "a");
        assert_eq!(a.ty.ty, SqlType::Int);
        assert!(!a.ty.nullable, "`NOT NULL` was written");
        assert!(a.ty.collation.is_none(), "`int` carries no collation");
        assert_eq!(b.name, "b");
        assert_eq!(b.ty.ty, SqlType::NVarChar(Len::Fixed(20)));
        assert!(
            b.ty.nullable,
            "nothing was written, so the column is nullable"
        );
        assert_eq!(b.ty.collation, Some(Collation::DEFAULT));
        assert!(def.constraints.is_empty());
    }

    /// The schema and the database of the session fill what the name leaves out, and a
    /// written part wins over the default.
    #[test]
    fn create_table_name_takes_the_session_database_and_schema() {
        assert_eq!(
            table_def("CREATE TABLE t (a int);").name,
            QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "t".to_owned(),
            }
        );
        assert_eq!(
            table_def("CREATE TABLE db.s.t (a int);").name,
            QualifiedName {
                database: "db".to_owned(),
                schema: "s".to_owned(),
                name: "t".to_owned(),
            }
        );
        assert_eq!(
            table_def("CREATE TABLE db..t (a int);").name,
            QualifiedName {
                database: "db".to_owned(),
                schema: "dbo".to_owned(),
                name: "t".to_owned(),
            }
        );
    }

    #[test]
    fn create_table_duplicate_name_is_2714() {
        let error = bind_with("CREATE TABLE dbo.ex (a int);", Some(&OneTable))
            .expect_err("`ex` is already there");
        assert_eq!(error.number, 2714);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 6);
        assert_eq!(
            error.message,
            "An object named 'ex' exists already in the database."
        );
        assert_eq!(error.line, 1);
    }

    /// The counter-proof of `create_table_duplicate_name_is_2714`: the same catalogue, a
    /// name it does not resolve, and the statement binds. Without it, a 2714 raised for any
    /// `CREATE TABLE` would pass the test above.
    #[test]
    fn create_table_unknown_name_binds_against_the_same_catalogue() {
        let bound = bind_with("CREATE TABLE dbo.other (a int);", Some(&OneTable))
            .expect("`other` is not in the catalogue");
        assert!(matches!(
            bound,
            BoundStatement::Ddl(DdlStatement::CreateTable { .. })
        ));
    }

    /// Without a catalogue there is nothing to resolve the name against, so the statement
    /// binds; production hands one over.
    #[test]
    fn create_table_without_a_catalogue_binds() {
        assert!(matches!(
            bound("CREATE TABLE dbo.ex (a int);"),
            Ok(BoundStatement::Ddl(DdlStatement::CreateTable { .. }))
        ));
    }

    /// The line of 2714 is the statement's, although the duplicate name sits one line below
    /// it: `CREATE TABLE` / `dbo.ex` / `(a int);` answers 1 on a batch that starts there, as
    /// SQL Server answers 3 when the statement starts on line 3.
    #[test]
    fn duplicate_name_2714_carries_the_line_of_the_statement() {
        let error = bind_with("CREATE TABLE\ndbo.ex\n(a int);", Some(&OneTable))
            .expect_err("`ex` is already there");
        assert_eq!((error.number, error.line), (2714, 1));
    }

    #[test]
    fn unknown_type_is_2715() {
        let error = err("CREATE TABLE t (a nosuch);");
        // State 6, the one of a column list, and not the 3 of a `DECLARE` (module header).
        assert_eq!((error.number, error.severity, error.state), (2715, 16, 6));
        assert_eq!(
            error.message,
            "Column, parameter or variable #1: unknown data type nosuch."
        );
        // The state is the whole difference with the 3 of a `DECLARE`: same number, same
        // sentence, same `#<n>` (`SqlError::cannot_find_data_type`).
        assert_eq!(
            SqlError::cannot_find_data_type(1, "nosuch").message,
            error.message
        );
        assert_eq!(SqlError::cannot_find_data_type(1, "nosuch").state, 3);
    }

    /// `#<n>` counts the columns of the statement from 1, a well-typed column included, and
    /// the line is the statement's, not the column's.
    #[test]
    fn unknown_type_2715_numbers_the_column_and_names_the_statement() {
        let error = err("CREATE TABLE t (a int, b varchar(10), c nosuch);");
        assert_eq!(
            error.message,
            "Column, parameter or variable #3: unknown data type nosuch."
        );
        let error = err("CREATE TABLE\nt\n(a int,\nb nosuch);");
        assert_eq!((error.number, error.line), (2715, 1));
    }

    /// The type of a column is resolved before the name is looked up: a `CREATE TABLE` that
    /// is both a duplicate and badly typed answers 2715, as SQL Server does.
    #[test]
    fn unknown_type_wins_over_duplicate_name() {
        let error = bind_with("CREATE TABLE dbo.ex (a nosuch);", Some(&OneTable))
            .expect_err("the type does not exist");
        assert_eq!(error.number, 2715);
    }

    #[test]
    fn temp_table_name_is_an_internal_error() {
        let error = err("CREATE TABLE #t (a int);");
        assert_eq!(error.number, 50000);
        assert!(
            error
                .message
                .ends_with("bind: the temporary table '#t' is not implemented yet"),
            "{}",
            error.message
        );
        assert!(
            err("CREATE TABLE ##t (a int);")
                .message
                .ends_with("bind: the temporary table '##t' is not implemented yet")
        );
        assert!(
            err("CREATE TABLE [#t] (a int);")
                .message
                .ends_with("bind: the temporary table '#t' is not implemented yet"),
            "a delimited name is the same temporary table"
        );
        assert_eq!(err("DROP TABLE #t;").number, 50000);
    }

    /// A four-part name names a linked server; 117's `object` filling has no constructor, so
    /// the refusal is the internal 50000 and it names the number it stands for.
    #[test]
    fn four_part_name_is_internal_v2() {
        let error = err("CREATE TABLE srv.db.dbo.t (a int);");
        assert_eq!(error.number, 50000);
        assert!(
            error.message.contains("'srv.db.dbo.t'") && error.message.contains("117"),
            "{}",
            error.message
        );
        assert_eq!(err("DROP TABLE a.b.c.d;").number, 50000);
    }

    /// A `PRIMARY KEY` column is not nullable, written at column level or at table level; a
    /// `UNIQUE` column stays nullable, which is what separates this rule from "a key column
    /// is not nullable".
    #[test]
    fn primary_key_columns_are_not_nullable_but_unique_ones_are() {
        let def = table_def("CREATE TABLE t (a int PRIMARY KEY, b int);");
        assert!(!def.columns[0].ty.nullable);
        assert!(def.columns[1].ty.nullable);

        let def = table_def("CREATE TABLE t (a int, b int, PRIMARY KEY (a));");
        assert!(!def.columns[0].ty.nullable);
        assert!(def.columns[1].ty.nullable);

        let def = table_def("CREATE TABLE t (a int UNIQUE, b int);");
        assert!(def.columns[0].ty.nullable, "a UNIQUE key stays nullable");
        assert!(def.columns[1].ty.nullable);
    }

    /// A `PRIMARY KEY` is clustered and a `UNIQUE` is not, unless the statement says
    /// otherwise; both are carried to the catalogue, which applies them.
    #[test]
    fn primary_key_and_unique_reach_the_table_def() {
        let def = table_def(
            "CREATE TABLE t (a int, b int, CONSTRAINT pk PRIMARY KEY (a DESC), UNIQUE (b));",
        );
        let [first, second] = def.constraints.as_slice() else {
            unreachable!("two constraints, got {:?}", def.constraints)
        };
        match first {
            ConstraintDef::PrimaryKey {
                name,
                columns,
                clustered,
            } => {
                assert_eq!(name.as_deref(), Some("pk"));
                assert_eq!(columns.len(), 1);
                assert_eq!(columns[0].column, "a");
                assert!(columns[0].descending);
                assert!(*clustered, "a PRIMARY KEY is CLUSTERED by default");
            }
            other => unreachable!("a PRIMARY KEY, got {other:?}"),
        }
        match second {
            ConstraintDef::Unique {
                name,
                columns,
                clustered,
            } => {
                assert!(name.is_none(), "the statement named no constraint");
                assert_eq!(columns[0].column, "b");
                assert!(
                    !*clustered,
                    "a UNIQUE constraint is NONCLUSTERED by default"
                );
            }
            other => unreachable!("a UNIQUE constraint, got {other:?}"),
        }

        let def =
            table_def("CREATE TABLE t (a int PRIMARY KEY NONCLUSTERED, b int UNIQUE CLUSTERED);");
        assert!(matches!(
            def.constraints.as_slice(),
            [
                ConstraintDef::PrimaryKey {
                    clustered: false,
                    ..
                },
                ConstraintDef::Unique {
                    clustered: true,
                    ..
                }
            ]
        ));
    }

    /// An unnamed `DEFAULT` fills the column, a named one becomes a constraint that keeps
    /// its name.
    #[test]
    fn default_goes_to_the_column_unless_it_was_named() {
        let def = table_def("CREATE TABLE t (a int DEFAULT 0);");
        assert!(def.columns[0].default.is_some());
        assert!(def.constraints.is_empty());

        let def = table_def("CREATE TABLE t (a int CONSTRAINT df DEFAULT 0);");
        assert!(def.columns[0].default.is_none());
        match def.constraints.as_slice() {
            [ConstraintDef::Default { name, column, .. }] => {
                assert_eq!(name.as_deref(), Some("df"));
                assert_eq!(column, "a");
            }
            other => unreachable!("one named DEFAULT, got {other:?}"),
        }
    }

    /// A bare `IDENTITY` is `IDENTITY(1, 1)`, and a written one keeps its two numbers.
    #[test]
    fn identity_keeps_its_seed_and_increment() {
        let def = table_def("CREATE TABLE t (a int IDENTITY, b int IDENTITY(10, 5), c int);");
        let seeds = |index: usize| {
            def.columns[index]
                .identity
                .map(|spec| (spec.seed, spec.increment))
        };
        assert_eq!(seeds(0), Some((1, 1)));
        assert_eq!(seeds(1), Some((10, 5)));
        assert_eq!(seeds(2), None);
    }

    /// A column `COLLATE` replaces the default collation of the type, and an unknown name
    /// is 448.
    #[test]
    fn column_collation_is_resolved() {
        let def = table_def("CREATE TABLE t (a varchar(10) COLLATE Latin1_General_CS_AS);");
        assert_eq!(
            def.columns[0].ty.collation,
            Some(Collation::parse("Latin1_General_CS_AS").expect("a known collation"))
        );
        assert_ne!(def.columns[0].ty.collation, Some(Collation::DEFAULT));
        assert_eq!(
            err("CREATE TABLE t (a varchar(10) COLLATE Klingon_CI_AS);").number,
            448
        );
    }

    #[test]
    fn create_database_binds_with_its_collation() {
        match bound("CREATE DATABASE d;") {
            Ok(BoundStatement::Ddl(DdlStatement::CreateDatabase { name, collation })) => {
                assert_eq!(name, "d");
                assert!(collation.is_none(), "no COLLATE was written");
            }
            other => unreachable!("a CreateDatabase, got {other:?}"),
        }
        match bound("CREATE DATABASE [d d] COLLATE Latin1_General_CS_AS;") {
            Ok(BoundStatement::Ddl(DdlStatement::CreateDatabase { name, collation })) => {
                assert_eq!(name, "d d", "the delimiters are not part of the name");
                assert_eq!(
                    collation,
                    Some(Collation::parse("Latin1_General_CS_AS").expect("a known collation"))
                );
            }
            other => unreachable!("a CreateDatabase, got {other:?}"),
        }
    }

    /// The options VaubanDB does not apply are dropped, and the `COLLATE` that follows them
    /// is still read (the parser cuts a `CREATE DATABASE` clause by clause).
    #[test]
    fn create_database_drops_the_file_options_and_keeps_the_collation() {
        let text = "CREATE DATABASE d ON PRIMARY (NAME = f, FILENAME = 'f.mdf') \
                    COLLATE Latin1_General_CS_AS;";
        match bound(text) {
            Ok(BoundStatement::Ddl(DdlStatement::CreateDatabase { name, collation })) => {
                assert_eq!(name, "d");
                assert!(collation.is_some());
            }
            other => unreachable!("a CreateDatabase, got {other:?}"),
        }
        assert_eq!(err("CREATE DATABASE d COLLATE Klingon_CI_AS;").number, 448);
    }

    #[test]
    fn drop_database_carries_its_names_and_if_exists() {
        match bound("DROP DATABASE IF EXISTS d1, [d 2];") {
            Ok(BoundStatement::Ddl(DdlStatement::DropDatabase { names, if_exists })) => {
                assert_eq!(names, ["d1", "d 2"]);
                assert!(if_exists);
            }
            other => unreachable!("a DropDatabase, got {other:?}"),
        }
        match bound("DROP DATABASE d1;") {
            Ok(BoundStatement::Ddl(DdlStatement::DropDatabase { if_exists, .. })) => {
                assert!(!if_exists);
            }
            other => unreachable!("a DropDatabase, got {other:?}"),
        }
    }

    /// A `DROP TABLE` binds against a catalogue that does not know the table: SQL Server
    /// answers 3701 while the statement runs, after an earlier `SELECT` of the same batch
    /// has already sent its rows (module header).
    #[test]
    fn drop_table_does_not_check_existence() {
        match bind_with("DROP TABLE dbo.nosuchtable, ex;", Some(&OneTable)) {
            Ok(BoundStatement::Ddl(DdlStatement::DropTable { names, if_exists })) => {
                assert_eq!(names.len(), 2);
                assert_eq!(names[0].name, "nosuchtable");
                assert_eq!(names[0].schema, "dbo");
                assert_eq!(names[1].schema, "dbo", "the default schema fills the gap");
                assert!(!if_exists);
            }
            other => unreachable!("a DropTable, got {other:?}"),
        }
        match bound("DROP TABLE IF EXISTS t;") {
            Ok(BoundStatement::Ddl(DdlStatement::DropTable { if_exists, .. })) => {
                assert!(if_exists);
            }
            other => unreachable!("a DropTable, got {other:?}"),
        }
    }

    #[test]
    fn use_binds() {
        match bound("USE nosuchdatabase;") {
            Ok(BoundStatement::Use { database }) => assert_eq!(database, "nosuchdatabase"),
            other => unreachable!("a Use, got {other:?}"),
        }
        match bound("USE [my db];") {
            Ok(BoundStatement::Use { database }) => {
                assert_eq!(database, "my db", "the delimiters are not part of the name");
            }
            other => unreachable!("a Use, got {other:?}"),
        }
    }

    /// `ROWGUIDCOL` is read by the parser and held by no `*Def` of the catalogue.
    #[test]
    fn rowguidcol_is_internal_v2() {
        let error = err("CREATE TABLE t (a uniqueidentifier ROWGUIDCOL);");
        assert_eq!(error.number, 50000);
        assert!(error.message.contains("ROWGUIDCOL"), "{}", error.message);
    }

    /// A `FOREIGN KEY` reaches the catalogue with the actions SQL Server applies when the
    /// statement leaves them out.
    #[test]
    fn foreign_key_fills_the_actions_it_was_not_given() {
        let def = table_def("CREATE TABLE t (a int REFERENCES other (b));");
        match def.constraints.as_slice() {
            [
                ConstraintDef::ForeignKey {
                    columns,
                    referenced,
                    referenced_columns,
                    on_delete,
                    on_update,
                    ..
                },
            ] => {
                assert_eq!(columns, &["a".to_owned()]);
                assert_eq!(referenced.name, "other");
                assert_eq!(referenced_columns, &["b".to_owned()]);
                assert_eq!(*on_delete, RefAction::NoAction);
                assert_eq!(*on_update, RefAction::NoAction);
            }
            other => unreachable!("one FOREIGN KEY, got {other:?}"),
        }
    }

    /// A column written twice answers 2705, on the line of the statement.
    #[test]
    fn duplicate_column_is_2705() {
        let error = err("CREATE TABLE dbo.m1 (a int, a int);");
        assert_eq!((error.number, error.severity, error.state), (2705, 16, 3));
        assert_eq!(
            error.message,
            "Column 'a' of table 'm1' is defined twice; column names have to be unique."
        );
        // The comparison ignores case and the message names the occurrence written last, as
        // `CREATE TABLE dbo.m2 (a int, A int);` does on SQL Server.
        let other_case = err("CREATE TABLE dbo.m2 (a int, A int);");
        assert_eq!((other_case.number, other_case.state), (2705, 3));
        assert_eq!(
            other_case.message,
            "Column 'A' of table 'm2' is defined twice; column names have to be unique."
        );
        // The duplicate sits before an unknown type.
        let and_unknown_type = err("CREATE TABLE dbo.m6 (a int, a nosuch);");
        assert_eq!((and_unknown_type.number, and_unknown_type.state), (2705, 3));
        assert_eq!(
            and_unknown_type.message,
            "Column 'a' of table 'm6' is defined twice; column names have to be unique."
        );
        // Line of the statement, not of the column: `CREATE TABLE` on line 1 here.
        assert_eq!(err("CREATE TABLE\nq2\n(a int,\na int);").line, 1);
        // Two columns whose names differ still bind.
        assert_eq!(table_def("CREATE TABLE t (a int, b int);").columns.len(), 2);
    }

    /// 2705 and 2715 do not rank: the first faulty column of the list decides, on
    /// `CREATE TABLE dbo.m6 (a int, a nosuch);` (2705) and on
    /// `CREATE TABLE t_both (a foo, b int, b int);` (2715 state 6).
    #[test]
    fn the_first_fault_of_the_column_list_wins() {
        assert_eq!(err("CREATE TABLE dbo.m6 (a int, a nosuch);").number, 2705);
        let unknown_first = err("CREATE TABLE t_both (a foo, b int, b int);");
        assert_eq!((unknown_first.number, unknown_first.state), (2715, 6));
        assert_eq!(
            unknown_first.message,
            "Column, parameter or variable #1: unknown data type foo."
        );
    }

    /// A computed column is refused, as `ROWGUIDCOL` is: the catalogue holds
    /// `ColumnDef::computed`, but nothing fills a computed column from it, and `AS a + 1`
    /// writes no type — 2715 would print an empty type name. SQL Server creates the table
    /// (`sys.columns.is_computed` = 1), so the refusal is the internal 50000.
    #[test]
    fn computed_column_is_an_internal_error() {
        let error = err("CREATE TABLE dbo.rc1 (a int, b AS a + 1);");
        assert_eq!(error.number, 50000);
        assert!(
            error
                .message
                .ends_with("bind: the computed column 'b' is not implemented yet"),
            "{}",
            error.message
        );
        // The refusal comes before the type is resolved: no 2715 on an empty type name.
        assert!(!error.message.contains("2715"), "{}", error.message);
        // A plain column of the same statement still binds.
        assert_eq!(table_def("CREATE TABLE t (a int, b int);").columns.len(), 2);
    }

    /// A `PRIMARY KEY` column that writes `NULL` answers 8111 state 1 on SQL Server, in the
    /// three forms below; 8111 has no constructor, so the binder refuses by the internal
    /// 50000 that names it rather than turning the column around in silence.
    #[test]
    fn primary_key_on_a_column_written_null_is_refused() {
        for text in [
            "CREATE TABLE dbo.z1 (a int NULL PRIMARY KEY);",
            "CREATE TABLE dbo.z1 (a int PRIMARY KEY NULL);",
            "CREATE TABLE dbo.z1 (a int NULL, PRIMARY KEY (a));",
        ] {
            let error = err(text);
            assert_eq!(error.number, 50000, "{text}");
            assert!(
                error.message.ends_with(
                    "bind: column 'a' of table 'z1' writes NULL and is part of the PRIMARY \
                     KEY; SQL Server answers 8111, which is not implemented yet"
                ),
                "{text} : {}",
                error.message
            );
        }
        // `UNIQUE` on a column written NULL creates the table on SQL Server, and binds
        // here, nullable.
        let def = table_def("CREATE TABLE dbo.z3 (a int NULL, UNIQUE (a));");
        assert!(def.columns[0].ty.nullable);
        // A key column that writes no nullability keeps the silent NOT NULL.
        let def = table_def("CREATE TABLE dbo.p3 (a int, b int, PRIMARY KEY (a));");
        assert!(!def.columns[0].ty.nullable);
        assert!(def.columns[1].ty.nullable);
        // `NOT NULL` written next to the key is not a written `NULL`.
        let def = table_def("CREATE TABLE dbo.p4 (a int NOT NULL PRIMARY KEY);");
        assert!(!def.columns[0].ty.nullable);
    }

    /// The dispatch of `statement.rs` sends the five statements of this file here, and not
    /// one of them is a `SELECT`.
    #[test]
    fn the_five_statements_of_this_file_bind() {
        for text in [
            "CREATE DATABASE d;",
            "DROP DATABASE d;",
            "USE d;",
            "CREATE TABLE t (a int);",
            "DROP TABLE t;",
        ] {
            let batch = parse_batch(text, &ParseOptions::default())
                .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
            assert!(
                !matches!(batch.statements[0], Statement::Select(_)),
                "{text} is not a SELECT"
            );
            assert!(bound(text).is_ok(), "{text} binds");
        }
        assert!(matches!(bound("SELECT 1;"), Ok(BoundStatement::Query(_))));
    }
}
