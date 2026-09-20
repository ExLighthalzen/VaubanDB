//! Binding of `CREATE`/`DROP INDEX` and of the `PRIMARY KEY` and `UNIQUE` constraints that
//! carry an index.
//!
//! The binder turns the AST into the [`IndexDef`] the catalogue takes and checks what SQL
//! Server checks while it **compiles**. Creating the index is the executor's job and the
//! catalogue's, not this file's.
//!
//! # What is checked here, and what is left to execution
//!
//! With each batch opening with `SELECT 1;`, a batch that answers one result set before its
//! error failed while it ran, a batch that answers none failed while it compiled (same
//! reading as `ddl.rs`). What SQL Server answers:
//!
//! | batch, after its `SELECT 1;` | number | state | result sets | the check runs |
//! |---|:-:|:-:|:-:|---|
//! | `CREATE INDEX i ON dbo.t1 (a, a);` | 1909 | 1 | 0 | at compilation |
//! | `CREATE INDEX i ON srv.db.dbo.s1 (a);` | 117 | 1 | 0 | at compilation |
//! | `CREATE INDEX i ON dbo.nosuchtable (a);` | 1088 | 12 | **1** | at execution |
//! | `CREATE INDEX i ON nosuchschema.r1 (a);` | 1088 | 12 | **1** | at execution |
//! | `CREATE INDEX i ON dbo.t1 (nosuchcolumn);` | 1911 | 1 | **1** | at execution |
//! | `CREATE INDEX ix_exists ON dbo.t2 (b);` (`ix_exists` is on `t2`) | 1913 | 1 | **1** | at execution |
//! | `CREATE CLUSTERED INDEX i2 …` after a clustered `i1` on the same table | 1902 | 3 | **1** | at execution |
//! | `CREATE INDEX i ON dbo.v1 (a);` (`v1` is a view) | 1939 | 1 | **1** | at execution |
//! | `DROP INDEX i ON dbo.t2;` (no such index) | 3701 | 7 | **1** | at execution |
//! | `DROP INDEX i ON dbo.nosuchtable;` | 3701 | 6 | **1** | at execution |
//! | `DROP INDEX IF EXISTS i ON dbo.nosuchtable;` | — | — | 2 | nothing raised |
//! | `DROP INDEX i ON srv.db.dbo.s1;` | 117 | 1 | 0 | at compilation |
//!
//! So one check of a `CREATE INDEX` belongs here — the duplicate key column, 1909 — and the
//! existence of the table, of its columns and of the index name belong to the catalogue
//! and the executor, which is where SQL Server answers them.
//!
//! One deviation is forced: [`IndexDef::table`] is an [`ObjectId`], so a `CREATE INDEX`
//! cannot be bound at all without resolving its table. A name
//! [`CatalogView`](crate::CatalogView) does not resolve is therefore refused here, at
//! compilation, by the internal 50000 that names 1088 — a provisional refusal, like the
//! ones `ddl.rs` raises. `DROP INDEX` carries a
//! [`QualifiedName`](vauban_catalog::QualifiedName) and needs no catalogue, so it keeps SQL
//! Server's own phase.
//!
//! # The line each number carries
//!
//! With the statement starting on line 3 of the batch and the node named below on a line
//! of its own: 1909 answers **3** although its duplicated column sits on line 6, 1088
//! answers **3** although the table name sits on line 5, 1911 answers **3** although the
//! column sits on line 6, and 1913 answers **3** although the index name sits on line 4.
//! Each of the four names the **statement**.
//!
//! # A `PRIMARY KEY` is clustered, unless another constraint took the place
//!
//! `CREATE TABLE dbo.pk1 (a int PRIMARY KEY, b int);` reads
//! `sys.indexes.type_desc = CLUSTERED` on its key, where
//! `CREATE TABLE dbo.uq1 (a int UNIQUE, b int);` reads `NONCLUSTERED` and leaves the table a
//! `HEAP`: the default of a `PRIMARY KEY` is not the default of a `UNIQUE`, and the second
//! shape is what tells the two apart.
//!
//! The default is **conditional**:
//! `CREATE TABLE dbo.x1 (a int PRIMARY KEY, b int UNIQUE CLUSTERED);` creates the table and
//! reads `NONCLUSTERED` on the key of the `PRIMARY KEY` and `CLUSTERED` on the `UNIQUE` —
//! in both orders and at table level too. A `PRIMARY KEY` that writes no clustering is
//! clustered when no constraint of the same statement writes `CLUSTERED`
//! ([`primary_key_clustered_by_default`]).
//!
//! # Numbers this file has no template for
//!
//! 1088, 1909, 1939, 8110 and 8112 have no constructor in `vauban-errors`, so each refusal
//! is the internal 50000 that names the number, as `ddl.rs` does for 8111.

use vauban_catalog::{ConstraintDef, IndexDef, ObjectId, SortedColumn};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{
    Clustering, ColumnConstraintKind, CreateIndexStatement, CreateTableStatement,
    DropIndexStatement, IndexColumn, TableConstraintKind,
};

use crate::bound::{BoundStatement, DdlStatement};
use crate::context::{BindContext, ResolvedTableKind};
use crate::ddl::{table_name, written};

/// Binds `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX ix ON t (c1, c2 DESC)`.
///
/// The checks run in the order SQL Server answers them: the written name first (117 for a
/// four-part name, over 1909), then the duplicate key column (1909, over 1088, 1913 and
/// 1911), then the table itself.
///
/// `CLUSTERED` is what was written, and `NONCLUSTERED` when nothing was:
/// `CREATE INDEX ix_plain ON dbo.t1 (a);` reads `sys.indexes.type_desc = NONCLUSTERED`,
/// where the `PRIMARY KEY` of a `CREATE TABLE` reads `CLUSTERED` (module header) — the two
/// defaults differ, which is why they are not written once.
///
/// # Errors
///
/// - the internal 50000 naming 117 for a four-part name and the one for a temporary
///   table, both from [`table_name`].
/// - the internal 50000 naming 1909 for a key column written twice, on the line of the
///   statement.
/// - the internal 50000 for `INCLUDE (…)` and for the `WHERE` of a filtered index:
///   [`IndexDef`] carries neither, and binding them would drop them in silence. Both are
///   served by SQL Server (`sys.index_columns.is_included_column` = 1 on the included
///   column, `sys.indexes.filter_definition` = `([a]>(1))` on the filtered index), so the
///   refusal is ours, not a syntax error.
/// - the internal 50000 naming 1088 when the table does not resolve, and naming 1939 for
///   an index on a view.
pub(crate) fn bind_create_index(
    stmt: &CreateIndexStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    // For its refusals: `DdlStatement::CreateIndex` carries the `ObjectId` of the table, not
    // its three-part name, so the name resolved here is checked and then dropped.
    table_name(&stmt.table, ctx)?;
    if let Some(column) = duplicate_key_column(&stmt.columns) {
        return Err(duplicate_index_column(column, stmt.span.line));
    }
    if !stmt.include.is_empty() {
        return Err(bug(format!(
            "bind: the INCLUDE columns of index '{}' are not implemented yet; IndexDef has \
             no place for them",
            stmt.name.value
        )));
    }
    if stmt.where_.is_some() {
        return Err(bug(format!(
            "bind: the WHERE of the filtered index '{}' is not implemented yet; IndexDef \
             has no place for it",
            stmt.name.value
        )));
    }
    Ok(BoundStatement::Ddl(DdlStatement::CreateIndex {
        def: IndexDef {
            table: index_target(stmt, ctx)?,
            name: stmt.name.value.clone(),
            columns: stmt.columns.iter().map(key_column).collect(),
            unique: stmt.unique,
            clustered: stmt.clustering == Some(Clustering::Clustered),
        },
    }))
}

/// The object the index is created on.
///
/// # Errors
///
/// The internal 50000 naming 1088 when the name resolves to nothing, and naming 1939 when
/// it resolves to a view: an index on a view is 1939 on SQL Server (the view is not schema
/// bound), and `SCHEMABINDING` itself is not implemented.
///
/// A batch bound without a catalogue cannot answer either way, and refusing is what is
/// left: [`IndexDef::table`] is an identifier the catalogue hands out.
fn index_target(stmt: &CreateIndexStatement, ctx: &BindContext<'_>) -> SqlResult<ObjectId> {
    let Some(catalog) = ctx.catalog else {
        return Err(bug(format!(
            "bind: CREATE INDEX on '{}' needs a catalogue to name the table, and this batch \
             is bound without one",
            written(&stmt.table)
        )));
    };
    let Some(resolved) = catalog.resolve_table(&stmt.table, ctx.database, ctx.default_schema)
    else {
        return Err(bug(format!(
            "bind: CREATE INDEX cannot find the object \"{}\"; SQL Server answers 1088 state \
             12 while it runs, which is not implemented yet",
            written(&stmt.table)
        )));
    };
    if resolved.kind == ResolvedTableKind::View {
        return Err(bug(format!(
            "bind: an index on the view '{}' is not implemented yet; SQL Server answers \
             1939 while it runs",
            written(&stmt.table)
        )));
    }
    Ok(resolved.object)
}

/// Binds `DROP INDEX [IF EXISTS] ix ON t`.
///
/// Nothing is looked up: an index that is not there is 3701 while the statement runs (state
/// 7 when the table is there, 6 when it is not), and `IF EXISTS` answers no error
/// (module header). The name is turned into a three-part name, which refuses the two
/// shapes [`table_name`] refuses — and 117 for a four-part name is answered at compilation
/// for `DROP INDEX` too, where its message names "the index name" instead of "the object
/// name".
///
/// # Errors
///
/// The internal 50000 of [`table_name`]: 117 for a four-part name, and the one for a
/// temporary table.
pub(crate) fn bind_drop_index(
    stmt: &DropIndexStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    Ok(BoundStatement::Ddl(DdlStatement::DropIndex {
        name: stmt.name.value.clone(),
        table: table_name(&stmt.table, ctx)?,
        if_exists: stmt.if_exists,
    }))
}

/// Whether a `PRIMARY KEY` of this `CREATE TABLE` that writes no clustering is clustered.
///
/// `true` when no constraint of the statement writes `CLUSTERED`, in a column or at table
/// level (module header). A `UNIQUE` that writes nothing is nonclustered (one leaves the
/// table a `HEAP`, two read two `NONCLUSTERED`), so this answer serves a `PRIMARY KEY`.
pub(crate) fn primary_key_clustered_by_default(stmt: &CreateTableStatement) -> bool {
    !clusterings(stmt).any(|clustering| clustering == Some(Clustering::Clustered))
}

/// Refuses the two shapes of a `CREATE TABLE` that SQL Server counts before it reads the
/// column list.
///
/// - 8110 (more than one `PRIMARY KEY` on the table), state 0, for a second `PRIMARY KEY`,
///   under each of the three clusterings and at table level.
/// - 8112 (more than one clustered index for constraints on the table), state 0, for a
///   second constraint **writing** `CLUSTERED`. The implicit clustering of a `PRIMARY KEY`
///   does not count: `(a int PRIMARY KEY, b int UNIQUE CLUSTERED)` creates the table.
///
/// Both run before the column list and before the key columns: 8110 wins over 2705, 8112
/// wins over 2705, over 2715, over 1909, over 1911 and over 8111; 8110 wins over 8112 when
/// both apply. Both name the table as it was written, schema included, and answer the line
/// of the statement (a statement spanning lines 3 to 6 answers 3).
///
/// # Errors
///
/// The internal 50000 naming 8110 or 8112, on the line of the statement.
pub(crate) fn refuse_two_keys(stmt: &CreateTableStatement) -> SqlResult<()> {
    let table = written(&stmt.name);
    if primary_keys(stmt) > 1 {
        return Err(bug(format!(
            "bind: table '{table}' is given more than one PRIMARY KEY; SQL Server answers \
             8110, which is not implemented yet"
        ))
        .with_line(stmt.span.line));
    }
    let clustered = clusterings(stmt)
        .filter(|clustering| *clustering == Some(Clustering::Clustered))
        .count();
    if clustered > 1 {
        return Err(bug(format!(
            "bind: table '{table}' writes CLUSTERED on {clustered} constraints; SQL Server \
             answers 8112, which is not implemented yet"
        ))
        .with_line(stmt.span.line));
    }
    Ok(())
}

/// Refuses a `PRIMARY KEY` or a `UNIQUE` of a `CREATE TABLE` that lists the same column
/// twice, which is the 1909 of an index: `CREATE TABLE dbo.tc8 (a int, b int, PRIMARY KEY
/// (a, a));` answers 1909 (duplicate column names in an index), state 1, at compilation.
///
/// It runs **after** the column list and **before** the nullability of the key: 2705 wins
/// over it, 2715 wins over it, and it wins over 8111.
///
/// # Errors
///
/// The internal 50000 naming 1909, on the line of the statement.
pub(crate) fn refuse_duplicate_key_column(
    constraints: &[ConstraintDef],
    line: u32,
) -> SqlResult<()> {
    for constraint in constraints {
        let (ConstraintDef::PrimaryKey { columns, .. } | ConstraintDef::Unique { columns, .. }) =
            constraint
        else {
            continue;
        };
        if let Some(column) = duplicate_sorted_column(columns) {
            return Err(duplicate_index_column(column, line));
        }
    }
    Ok(())
}

/// The 1909 of a key column written twice, on the line of the statement.
///
/// The message names the **later** occurrence and the comparison ignores case:
/// `CREATE INDEX ix_d2 ON dbo.s1 (a, A);` names `'A'` and
/// `CREATE INDEX ix_w ON dbo.r1 ([a], "A");` names `'A'` too, delimiters removed. The
/// direction written next to a key column changes nothing: `(a, a DESC)` answers 1909.
fn duplicate_index_column(column: &str, line: u32) -> SqlError {
    bug(format!(
        "bind: column '{column}' is listed more than once in an index; SQL Server answers \
         1909, which is not implemented yet"
    ))
    .with_line(line)
}

/// The later of two key columns of the same name, `None` when the list has no such pair.
fn duplicate_key_column(columns: &[IndexColumn]) -> Option<&str> {
    columns.iter().enumerate().find_map(|(index, column)| {
        columns[..index]
            .iter()
            .any(|earlier| earlier.name.value.eq_ignore_ascii_case(&column.name.value))
            .then_some(column.name.value.as_str())
    })
}

/// The same answer as [`duplicate_key_column`], on the key of a constraint already turned
/// into the terms of the catalogue.
fn duplicate_sorted_column(columns: &[SortedColumn]) -> Option<&str> {
    columns.iter().enumerate().find_map(|(index, column)| {
        columns[..index]
            .iter()
            .any(|earlier| earlier.column.eq_ignore_ascii_case(&column.column))
            .then_some(column.column.as_str())
    })
}

/// One key column of a `CREATE INDEX`, in the terms of the catalogue.
fn key_column(column: &IndexColumn) -> SortedColumn {
    SortedColumn {
        column: column.name.value.clone(),
        descending: column.desc,
    }
}

/// How many `PRIMARY KEY` constraints the statement writes, in its columns and at table
/// level.
fn primary_keys(stmt: &CreateTableStatement) -> usize {
    let in_columns = stmt
        .definition
        .columns
        .iter()
        .flat_map(|column| &column.constraints)
        .filter(|constraint| matches!(constraint.kind, ColumnConstraintKind::PrimaryKey { .. }))
        .count();
    let at_table_level = stmt
        .definition
        .constraints
        .iter()
        .filter(|constraint| matches!(constraint.kind, TableConstraintKind::PrimaryKey { .. }))
        .count();
    in_columns + at_table_level
}

/// The clustering written by each `PRIMARY KEY` and each `UNIQUE` of the statement, in its
/// columns then at table level; `None` for a constraint that wrote nothing.
fn clusterings(stmt: &CreateTableStatement) -> impl Iterator<Item = Option<Clustering>> + '_ {
    let in_columns = stmt
        .definition
        .columns
        .iter()
        .flat_map(|column| &column.constraints)
        .filter_map(|constraint| match constraint.kind {
            ColumnConstraintKind::PrimaryKey { clustering, .. }
            | ColumnConstraintKind::Unique { clustering, .. } => Some(clustering),
            _ => None,
        });
    let at_table_level =
        stmt.definition
            .constraints
            .iter()
            .filter_map(|constraint| match constraint.kind {
                TableConstraintKind::PrimaryKey { clustering, .. }
                | TableConstraintKind::Unique { clustering, .. } => Some(clustering),
                _ => None,
            });
    in_columns.chain(at_table_level)
}

/// The internal 50000 a refusal of this file carries.
fn bug(message: String) -> SqlError {
    SqlError::from(InternalError::Bug(message))
}

#[cfg(test)]
mod tests;
