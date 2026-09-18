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
mod tests {
    use vauban_catalog::{ConstraintDef, IndexDef, ObjectId, QualifiedName, TableDef};
    use vauban_errors::SqlError;
    use vauban_parser::{ObjectName, ParseOptions, Statement, parse_batch};

    use crate::bound::{BoundStatement, DdlStatement};
    use crate::context::{
        BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
    };
    use crate::statement::bind;

    /// A catalogue that resolves one table, `t1`, and one view, `v1`.
    struct OneTableOneView;

    impl CatalogView for OneTableOneView {
        fn resolve_table(
            &self,
            name: &ObjectName,
            _database: &str,
            _default_schema: &str,
        ) -> Option<ResolvedTable> {
            let kind = if name.name.value.eq_ignore_ascii_case("t1") {
                ResolvedTableKind::Table
            } else if name.name.value.eq_ignore_ascii_case("v1") {
                ResolvedTableKind::View
            } else {
                return None;
            };
            Some(ResolvedTable {
                object: ObjectId(7),
                table: None,
                columns: Vec::new(),
                kind,
            })
        }
    }

    /// Binds the first statement of `text` against a catalogue that knows `t1` and `v1`.
    fn bind_one(text: &str) -> Result<BoundStatement, SqlError> {
        let batch = parse_batch(text, &ParseOptions::default())
            .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
        let catalog = OneTableOneView;
        let ctx = BindContext {
            text,
            catalog: Some(&catalog),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        };
        bind(&batch.statements[0], &ctx)
    }

    /// The first binding error of `text`, statement by statement, as a session reports it:
    /// the batches that place a statement on line 2 open with a `SELECT 1;` that binds.
    fn err(text: &str) -> SqlError {
        let batch = parse_batch(text, &ParseOptions::default())
            .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
        let catalog = OneTableOneView;
        let ctx = BindContext {
            text,
            catalog: Some(&catalog),
            database: "master",
            default_schema: "dbo",
            variables: &NoVariables,
            options: SessionOptions::default(),
        };
        for statement in &batch.statements {
            if let Err(error) = bind(statement, &ctx) {
                return error;
            }
        }
        unreachable!("{text} should not bind")
    }

    /// The [`IndexDef`] a `CREATE INDEX` binds to.
    fn index_of(text: &str) -> IndexDef {
        match bind_one(text) {
            Ok(BoundStatement::Ddl(DdlStatement::CreateIndex { def })) => def,
            other => unreachable!("{text} binds to a CREATE INDEX, got {other:?}"),
        }
    }

    /// The [`TableDef`] a `CREATE TABLE` binds to.
    fn table_of(text: &str) -> TableDef {
        match bind_one(text) {
            Ok(BoundStatement::Ddl(DdlStatement::CreateTable { def })) => def,
            other => unreachable!("{text} binds to a CREATE TABLE, got {other:?}"),
        }
    }

    /// The clustering of each constraint of a `CREATE TABLE`, in the order the binder wrote
    /// them: `("PK", true)` for a clustered `PRIMARY KEY`.
    fn keys(def: &TableDef) -> Vec<(&'static str, bool)> {
        def.constraints
            .iter()
            .filter_map(|constraint| match constraint {
                ConstraintDef::PrimaryKey { clustered, .. } => Some(("PK", *clustered)),
                ConstraintDef::Unique { clustered, .. } => Some(("UQ", *clustered)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn create_index_on_known_table_binds() {
        let def = index_of("CREATE INDEX ix ON dbo.t1 (a, b DESC);");
        assert_eq!(def.table, ObjectId(7));
        assert_eq!(def.name, "ix");
        assert_eq!(def.columns.len(), 2);
        assert_eq!(def.columns[0].column, "a");
        assert!(!def.columns[0].descending);
        assert_eq!(def.columns[1].column, "b");
        assert!(def.columns[1].descending, "DESC is kept on the second key");
        assert!(!def.unique, "no UNIQUE was written");
        assert!(
            !def.clustered,
            "a CREATE INDEX that writes no clustering is NONCLUSTERED \
             (case create_index_default_clustering)"
        );

        // `UNIQUE` and `CLUSTERED` are read from the statement, which is the counter-proof
        // of the two `false` above: the same binder answers `true` when they are written.
        let unique = index_of("CREATE UNIQUE CLUSTERED INDEX ix ON dbo.t1 (a);");
        assert!(unique.unique);
        assert!(unique.clustered);
        let nonclustered = index_of("CREATE UNIQUE NONCLUSTERED INDEX ix ON dbo.t1 (a);");
        assert!(nonclustered.unique);
        assert!(!nonclustered.clustered);
    }

    /// The table of a `CREATE INDEX` that the catalogue does not resolve.
    ///
    /// SQL Server answers **1088** state 12, and answers it while the statement runs (one
    /// result set already sent, module header). 1088 has no constructor in
    /// `vauban-errors`, so the refusal is the internal 50000 naming it.
    #[test]
    fn create_index_unknown_table_is_1088() {
        let error = err("CREATE INDEX ix ON dbo.nosuchtable (a);");
        assert_eq!(error.number, 50000);
        assert!(
            error.message.ends_with(
                "bind: CREATE INDEX cannot find the object \"dbo.nosuchtable\"; SQL Server \
                 answers 1088 state 12 while it runs, which is not implemented yet"
            ),
            "{}",
            error.message
        );
        // The counter-proof: the same catalogue, the same statement, a name it resolves.
        assert!(bind_one("CREATE INDEX ix ON dbo.t1 (a);").is_ok());
        // And a name it resolves to a view is refused by another sentence, 1939's.
        let view = err("CREATE INDEX ix ON dbo.v1 (a);");
        assert_eq!(view.number, 50000);
        assert!(
            view.message.ends_with(
                "bind: an index on the view 'dbo.v1' is not implemented yet; SQL Server \
                 answers 1939 while it runs"
            ),
            "{}",
            view.message
        );
    }

    /// A key column written twice is 1909, at compilation, on the line of the statement.
    #[test]
    fn create_index_duplicate_key_column_is_1909() {
        // The statement starts on line 2 and its duplicate sits on line 5: the line
        // asserted below is the statement's, and no other node of the batch carries it.
        let error = err("SELECT 1;\nCREATE INDEX\nix\nON dbo.t1\n(a,\na);");
        assert_eq!(error.number, 50000);
        assert_eq!(error.line, 2);
        assert!(
            error.message.ends_with(
                "bind: column 'a' is listed more than once in an index; SQL Server answers \
                 1909, which is not implemented yet"
            ),
            "{}",
            error.message
        );
        // The later occurrence is the one named, and the comparison ignores case.
        assert!(
            err("CREATE INDEX ix ON dbo.t1 (a, A);")
                .message
                .contains("column 'A' is listed"),
            "the message names the occurrence written last, as SQL Server does"
        );
        // A direction on the second copy changes nothing, and 1909 wins over the table:
        // the catalogue does not resolve `nosuchtable`, yet the answer is 1909.
        assert!(
            err("CREATE INDEX ix ON dbo.nosuchtable (a, a DESC);")
                .message
                .contains("listed more than once"),
            "1909 is answered before the table is looked up"
        );
        // Counter-proof: two different columns bind.
        assert!(bind_one("CREATE INDEX ix ON dbo.t1 (a, b);").is_ok());
    }

    /// `INCLUDE` and the `WHERE` of a filtered index are refused: `IndexDef` holds neither.
    #[test]
    fn create_index_include_and_filter_are_refused() {
        let include = err("CREATE INDEX ix ON dbo.t1 (a) INCLUDE (b);");
        assert_eq!(include.number, 50000);
        assert!(
            include.message.ends_with(
                "bind: the INCLUDE columns of index 'ix' are not implemented yet; IndexDef \
                 has no place for them"
            ),
            "{}",
            include.message
        );
        let filtered = err("CREATE UNIQUE INDEX ix ON dbo.t1 (a) WHERE a > 1;");
        assert_eq!(filtered.number, 50000);
        assert!(
            filtered.message.ends_with(
                "bind: the WHERE of the filtered index 'ix' is not implemented yet; \
                 IndexDef has no place for it"
            ),
            "{}",
            filtered.message
        );
        // Counter-proof: the same index without either clause binds.
        assert!(bind_one("CREATE INDEX ix ON dbo.t1 (a);").is_ok());
    }

    /// A four-part name and a temporary table are refused for a `CREATE INDEX` as they are
    /// for a `CREATE TABLE` (`ddl.rs`), and 117 is answered at compilation for both
    /// `CREATE INDEX` and `DROP INDEX`.
    #[test]
    fn index_on_a_four_part_name_or_a_temporary_table_is_refused() {
        assert!(
            err("CREATE INDEX ix ON srv.db.dbo.t1 (a);")
                .message
                .contains("names a linked server")
        );
        assert!(
            err("DROP INDEX ix ON srv.db.dbo.t1;")
                .message
                .contains("names a linked server")
        );
        assert!(
            err("CREATE INDEX ix ON #t1 (a);")
                .message
                .contains("temporary table '#t1'")
        );
        assert!(
            err("DROP INDEX ix ON #t1;")
                .message
                .contains("temporary table '#t1'")
        );
    }

    /// A `CREATE INDEX` cannot be bound without a catalogue: [`IndexDef::table`] is an
    /// identifier the catalogue hands out.
    #[test]
    fn create_index_without_a_catalogue_is_refused() {
        let text = "CREATE INDEX ix ON dbo.t1 (a);";
        let batch = parse_batch(text, &ParseOptions::default())
            .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
        let ctx = BindContext::scalar(text, SessionOptions::default());
        let error = bind(&batch.statements[0], &ctx).expect_err("no catalogue, no ObjectId");
        assert_eq!(error.number, 50000);
        assert!(
            error.message.ends_with(
                "bind: CREATE INDEX on 'dbo.t1' needs a catalogue to name the table, and this \
                 batch is bound without one"
            ),
            "{}",
            error.message
        );
        // The counter-proof: a `DROP INDEX` needs no catalogue and binds under it too.
        let drop = parse_batch("DROP INDEX ix ON dbo.t1;", &ParseOptions::default())
            .unwrap_or_else(|e| unreachable!("{e:?}"));
        assert!(matches!(
            bind(&drop.statements[0], &ctx),
            Ok(BoundStatement::Ddl(DdlStatement::DropIndex { .. }))
        ));
    }

    #[test]
    fn drop_index_binds() {
        let bound = bind_one("DROP INDEX ix ON dbo.t1;").expect("DROP INDEX binds");
        let BoundStatement::Ddl(DdlStatement::DropIndex {
            name,
            table,
            if_exists,
        }) = bound
        else {
            unreachable!("DROP INDEX binds to DdlStatement::DropIndex, got {bound:?}")
        };
        assert_eq!(name, "ix");
        assert_eq!(
            table,
            QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: "t1".to_owned(),
            },
            "the written name is completed with the database and schema of the session"
        );
        assert!(!if_exists);

        // `IF EXISTS` is carried as written, and a table the catalogue does not resolve is
        // bound the same way: 3701 is answered while the statement runs, states 7 and 6
        // (module header), so the binder looks nothing up here.
        let bound = bind_one("DROP INDEX IF EXISTS ix ON dbo.nosuchtable;")
            .expect("DROP INDEX on an unknown table binds");
        let BoundStatement::Ddl(DdlStatement::DropIndex {
            table, if_exists, ..
        }) = bound
        else {
            unreachable!("DROP INDEX binds to DdlStatement::DropIndex, got {bound:?}")
        };
        assert!(if_exists);
        assert_eq!(table.name, "nosuchtable");
    }

    /// A `PRIMARY KEY` that writes no clustering is clustered, and the `UNIQUE` next to it
    /// is not.
    #[test]
    fn create_table_primary_key_is_clustered_by_default() {
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.pk1 (a int PRIMARY KEY, b int);"
            )),
            [("PK", true)],
            "sys.indexes reads CLUSTERED on this key (case pk_default_clustering)"
        );
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.pk2 (a int, b int, PRIMARY KEY (a, b DESC));"
            )),
            [("PK", true)],
            "a table-level PRIMARY KEY answers the same (table_level_pk_default_clustering)"
        );
        // The counter-proof, without which "a key is clustered" would read the same: a
        // `UNIQUE` that writes nothing is NONCLUSTERED and leaves the table a HEAP.
        assert_eq!(
            keys(&table_of("CREATE TABLE dbo.uq1 (a int UNIQUE, b int);")),
            [("UQ", false)]
        );
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.pu1 (a int PRIMARY KEY, b int UNIQUE);"
            )),
            [("PK", true), ("UQ", false)],
            "one clustered PRIMARY KEY, one nonclustered UNIQUE (pk_and_unique_default_clustering)"
        );
        // What is written wins over the default, both ways.
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.tc5 (a int PRIMARY KEY NONCLUSTERED, b int UNIQUE CLUSTERED);"
            )),
            [("PK", false), ("UQ", true)],
            "case nonclustered_pk_and_clustered_unique"
        );
    }

    /// The default of a `PRIMARY KEY` is conditional: another constraint writing
    /// `CLUSTERED` takes the place, and the key becomes nonclustered instead of colliding.
    #[test]
    fn primary_key_yields_the_clustered_place_to_a_written_clustered() {
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.x1 (a int PRIMARY KEY, b int UNIQUE CLUSTERED);"
            )),
            [("PK", false), ("UQ", true)],
            "case pk_default_next_to_clustered_unique: the PRIMARY KEY reads NONCLUSTERED"
        );
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.x2 (a int UNIQUE CLUSTERED, b int PRIMARY KEY);"
            )),
            [("UQ", true), ("PK", false)],
            "case clustered_unique_before_default_pk: the order changes nothing"
        );
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.x3 (a int, b int, PRIMARY KEY (a), UNIQUE CLUSTERED (b));"
            )),
            [("PK", false), ("UQ", true)],
            "case table_level_pk_default_next_to_clustered_unique"
        );
        // The counter-proof of the three above: a `UNIQUE` that writes nothing leaves the
        // clustered place to the `PRIMARY KEY`.
        assert_eq!(
            keys(&table_of(
                "CREATE TABLE dbo.x4 (a int PRIMARY KEY, b int UNIQUE, c int UNIQUE);"
            )),
            [("PK", true), ("UQ", false), ("UQ", false)],
            "case pk_default_and_two_default_uniques"
        );
    }

    /// Two `PRIMARY KEY` constraints, or two constraints writing `CLUSTERED`, are refused
    /// before the column list is read.
    #[test]
    fn two_keys_of_the_same_kind_are_refused() {
        let two_clustered = err(
            "SELECT 1;\nCREATE TABLE\ndbo.tc2\n(a int PRIMARY KEY CLUSTERED,\nb int UNIQUE CLUSTERED);",
        );
        assert_eq!(two_clustered.number, 50000);
        assert_eq!(
            two_clustered.line, 2,
            "the line of the statement, whose CLUSTERED constraints sit on lines 4 and 5"
        );
        assert!(
            two_clustered.message.ends_with(
                "bind: table 'dbo.tc2' writes CLUSTERED on 2 constraints; SQL Server answers \
                 8112, which is not implemented yet"
            ),
            "{}",
            two_clustered.message
        );
        let two_pk = err("CREATE TABLE dbo.w3 (a int PRIMARY KEY, b int PRIMARY KEY);");
        assert!(
            two_pk.message.ends_with(
                "bind: table 'dbo.w3' is given more than one PRIMARY KEY; SQL Server answers \
                 8110, which is not implemented yet"
            ),
            "{}",
            two_pk.message
        );
        // 8110 wins over 8112 when both apply, and
        // both win over the column list: 2705 and 2715 are not what these two answer.
        assert!(
            err("CREATE TABLE dbo.k1 (a int PRIMARY KEY CLUSTERED, b int PRIMARY KEY CLUSTERED);")
                .message
                .contains("more than one PRIMARY KEY")
        );
        assert!(
            err("CREATE TABLE dbo.tc7 (a int PRIMARY KEY CLUSTERED, a int UNIQUE CLUSTERED);")
                .message
                .contains("writes CLUSTERED on 2 constraints"),
            "8112 wins over the 2705 of the duplicate column (two_clustered_and_duplicate_column)"
        );
        assert!(
            err(
                "CREATE TABLE dbo.tc6 (a int PRIMARY KEY CLUSTERED, b nosuchtype UNIQUE CLUSTERED);"
            )
            .message
            .contains("writes CLUSTERED on 2 constraints"),
            "8112 wins over the 2715 of the unknown type (two_clustered_and_unknown_type)"
        );
        assert!(
            err("CREATE TABLE dbo.w9 (a int NULL PRIMARY KEY CLUSTERED, b int UNIQUE CLUSTERED);")
                .message
                .contains("writes CLUSTERED on 2 constraints"),
            "8112 wins over the 8111 of the nullable key (clustered_pk_written_null_column)"
        );
        // Counter-proof: one written CLUSTERED next to a default PRIMARY KEY binds.
        assert!(
            bind_one("CREATE TABLE dbo.w1 (a int PRIMARY KEY, b int UNIQUE CLUSTERED);").is_ok(),
            "case implicit_clustered_pk_and_explicit_clustered_unique creates the table"
        );
    }

    /// A `PRIMARY KEY` or a `UNIQUE` of a `CREATE TABLE` that lists the same column twice
    /// is the 1909 of an index, after the column list and before the nullability of the key.
    #[test]
    fn duplicate_key_column_of_a_create_table_is_1909() {
        let error = err("SELECT 1;\nCREATE TABLE dbo.tc8\n(a int, b int, PRIMARY KEY (a, a));");
        assert_eq!(error.number, 50000);
        assert_eq!(error.line, 2, "the line of the statement");
        assert!(
            error.message.ends_with(
                "bind: column 'a' is listed more than once in an index; SQL Server answers \
                 1909, which is not implemented yet"
            ),
            "{}",
            error.message
        );
        assert!(
            err("CREATE TABLE dbo.q4 (a int, b int, UNIQUE (a, a));")
                .message
                .contains("listed more than once"),
            "a UNIQUE answers it too (case unique_dup_key)"
        );
        assert!(
            err("CREATE TABLE dbo.q5 (a int, b int, CONSTRAINT u1 UNIQUE (A, a));")
                .message
                .contains("column 'a' is listed"),
            "the comparison ignores case and names the later occurrence \
             (unique_dup_key_column_level_impossible)"
        );
        // The order around it: 2705 and 2715 win, 8111 loses.
        assert_eq!(
            err("CREATE TABLE dbo.q3 (a int, a int, PRIMARY KEY (b, b));").number,
            2705,
            "case pk_dup_key_and_duplicate_column"
        );
        assert_eq!(
            err("CREATE TABLE dbo.q2 (a int, b nosuchtype, PRIMARY KEY (a, a));").number,
            2715,
            "case pk_dup_key_and_unknown_type"
        );
        assert!(
            err("CREATE TABLE dbo.z1 (a int NULL, b int, PRIMARY KEY (a, a));")
                .message
                .contains("listed more than once"),
            "1909 wins over 8111 (case null_pk_with_duplicate_key_column)"
        );
        // Counter-proof: a key over two different columns binds, and keeps its order.
        let def = table_of("CREATE TABLE dbo.ok1 (a int, b int, PRIMARY KEY (a, b DESC));");
        let [ConstraintDef::PrimaryKey { columns, .. }] = def.constraints.as_slice() else {
            unreachable!("one PRIMARY KEY, got {:?}", def.constraints)
        };
        assert_eq!(columns.len(), 2);
        assert!(columns[1].descending);
    }

    /// The statements this file binds are not the internal 50000 of `statement.rs`.
    #[test]
    fn create_and_drop_index_are_bound() {
        assert!(matches!(
            bind_one("CREATE INDEX ix ON dbo.t1 (a);"),
            Ok(BoundStatement::Ddl(DdlStatement::CreateIndex { .. }))
        ));
        assert!(matches!(
            bind_one("DROP INDEX ix ON dbo.t1;"),
            Ok(BoundStatement::Ddl(DdlStatement::DropIndex { .. }))
        ));
        assert!(matches!(
            bind_one("ALTER TABLE dbo.t1 ADD c int;"),
            Ok(BoundStatement::Ddl(DdlStatement::AlterTable { .. }))
        ));
    }

    /// The `Statement` variants this file reads are the two the parser produces for them.
    #[test]
    fn the_parser_hands_over_the_two_index_statements() {
        let batch = parse_batch(
            "CREATE INDEX ix ON dbo.t1 (a);\nDROP INDEX ix ON dbo.t1;",
            &ParseOptions::default(),
        )
        .unwrap_or_else(|e| unreachable!("{e:?}"));
        assert!(matches!(batch.statements[0], Statement::CreateIndex(_)));
        assert!(matches!(batch.statements[1], Statement::DropIndex(_)));
    }
}
