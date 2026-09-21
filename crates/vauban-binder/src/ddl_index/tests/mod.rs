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
/// result set already sent, module header). The session defers this number the same way
/// it defers 208 (`vauban-session`, `tests/run_batch_pipeline.rs`); what this binder-level
/// test pins is the error the binding itself produces.
#[test]
fn create_index_unknown_table_is_1088() {
    let error = err("CREATE INDEX ix ON dbo.nosuchtable (a);");
    assert_eq!((error.number, error.severity, error.state), (1088, 16, 12));
    assert_eq!(
        error.message,
        "Object \"dbo.nosuchtable\" was not found: it does not exist or is not accessible."
    );
    assert_eq!(error.line, 1);
    // A four-line statement still answers the line it starts on, not the line the table
    // name sits on.
    let multiline = err("SELECT 1;\nCREATE INDEX\nix\nON dbo.nosuchtable2\n(a);");
    assert_eq!(multiline.number, 1088);
    assert_eq!(multiline.line, 2);
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
        err("CREATE TABLE dbo.tc6 (a int PRIMARY KEY CLUSTERED, b nosuchtype UNIQUE CLUSTERED);")
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
