//! `CREATE TABLE`, `ALTER TABLE` and `DROP TABLE`: columns, `IDENTITY` and constraints.
//!
//! Everything goes through the public entry point of the crate, `parse_batch`: what a
//! client sends is a batch, and the shape of the AST it yields is what `binder` and
//! `catalog` are written against.
//!
//! [`rt`] is the `parse` -> `Display` -> `parse` loop the module README makes a contract.
//! The assumed deviations of the re-serialisation -- the `IDENTITY` that moves, the
//! `PERSISTED` and the trailing clauses that vanish, the `FOREIGN KEY` that a column
//! constraint drops -- are asserted here as deviations rather than hidden.

use vauban_errors::SqlError;
use vauban_parser::{
    AlterTableAction, AlterTableStatement, Batch, Clustering, ColumnConstraintKind, ColumnDef,
    ConstraintCheck, CreateTableStatement, Ident, IndexOption, IndexOptionValue, IndexStorage,
    ObjectName, ParseOptions, RefAction, StoragePlacement, TableConstraint, TableConstraintKind,
    TypeArg, parse_batch,
};

/// Parses `text`, which must parse.
fn p(text: &str) -> Batch {
    parse_batch(text, &ParseOptions::default()).unwrap()
}

/// Checks the `parse` -> `Display` -> `parse` loop on `text` and returns what `Display`
/// wrote.
fn rt(text: &str) -> String {
    let batch = p(text);
    let printed = batch.to_string();
    assert_eq!(batch, p(&printed), "{text} was printed as {printed}");
    printed
}

/// The error of a text that must **not** parse.
fn p_err(text: &str) -> SqlError {
    match parse_batch(text, &ParseOptions::default()) {
        Ok(batch) => unreachable!("{text} should not parse, got {batch:?}"),
        Err(error) => error,
    }
}

/// The one `CREATE TABLE` of a batch that holds one statement.
fn create(text: &str) -> CreateTableStatement {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(vauban_parser::Statement::CreateTable(create)) => *create,
        other => unreachable!("{text} is a CREATE TABLE, got {other:?}"),
    }
}

/// The columns of the one `CREATE TABLE` of `text`.
fn columns(text: &str) -> Vec<ColumnDef> {
    create(text).definition.columns
}

/// The table-level constraints of the one `CREATE TABLE` of `text`.
fn table_constraints(text: &str) -> Vec<TableConstraint> {
    create(text).definition.constraints
}

/// The one `ALTER TABLE` of a batch that holds one statement.
fn alter(text: &str) -> AlterTableStatement {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(vauban_parser::Statement::AlterTable(alter)) => *alter,
        other => unreachable!("{text} is an ALTER TABLE, got {other:?}"),
    }
}

/// The names and the `IF EXISTS` flag of the one `DROP TABLE` of `text`.
fn drop_names(text: &str) -> (Vec<ObjectName>, bool) {
    let mut statements = p(text).statements;
    assert_eq!(statements.len(), 1, "{text} is one statement");
    match statements.pop() {
        Some(vauban_parser::Statement::DropTable {
            names, if_exists, ..
        }) => (names, if_exists),
        other => unreachable!("{text} is a DROP TABLE, got {other:?}"),
    }
}

/// The kinds of the constraints of column `index` of `text`, in the order written.
fn column_kinds(text: &str, index: usize) -> Vec<ColumnConstraintKind> {
    columns(text)[index]
        .constraints
        .iter()
        .map(|constraint| constraint.kind.clone())
        .collect()
}

/// The name of the sole constraint of column `index` of `text`.
fn column_constraint_name(text: &str, index: usize) -> Option<Ident> {
    columns(text)[index].constraints[0].name.clone()
}

#[test]
fn create_table_minimal() {
    let statement = create("CREATE TABLE t (a int)");
    assert_eq!(statement.name.name.value, "t");
    assert!(statement.definition.constraints.is_empty());
    assert_eq!(statement.definition.columns.len(), 1);
    let column = &statement.definition.columns[0];
    assert_eq!(column.name.value, "a");
    assert_eq!(column.ty.name, "int");
    assert!(column.ty.args.is_empty());
    assert!(column.constraints.is_empty());
    assert!(column.collation.is_none());
    assert!(column.identity.is_none());
    assert!(column.computed.is_none());
    assert_eq!(rt("CREATE TABLE t (a int)"), "CREATE TABLE t (a int)");
}

#[test]
fn create_table_types() {
    let text = "CREATE TABLE t (a int, b varchar(50), c nvarchar(max), \
                d decimal(18, 2), e datetime2(7), f uniqueidentifier, g dbo.MonType)";
    let columns = columns(text);
    let types: Vec<(String, Vec<TypeArg>)> = columns
        .iter()
        .map(|column| (column.ty.name.clone(), column.ty.args.clone()))
        .collect();
    assert_eq!(
        types,
        vec![
            ("int".to_owned(), vec![]),
            ("varchar".to_owned(), vec![TypeArg::Number(50)]),
            ("nvarchar".to_owned(), vec![TypeArg::Max]),
            (
                "decimal".to_owned(),
                vec![TypeArg::Number(18), TypeArg::Number(2)]
            ),
            ("datetime2".to_owned(), vec![TypeArg::Number(7)]),
            ("uniqueidentifier".to_owned(), vec![]),
            // A user-defined type is a qualified name kept whole.
            ("dbo.MonType".to_owned(), vec![]),
        ]
    );
    // The one thing `Display` does not write back as written is the `max` argument, which
    // `Display` writes uppercase; everything else is exact.
    assert_eq!(rt(text), text.replace("nvarchar(max)", "nvarchar(MAX)"));
    assert_eq!(
        rt("CREATE TABLE t (c nvarchar(MAX))"),
        "CREATE TABLE t (c nvarchar(MAX))"
    );
}

#[test]
fn create_table_nullability_and_default() {
    let text = "CREATE TABLE t (a int NOT NULL, b int NULL, c int DEFAULT 0, \
                d varchar(10) NOT NULL DEFAULT 'x', e int CONSTRAINT DF_e DEFAULT (0))";
    assert_eq!(column_kinds(text, 0), vec![ColumnConstraintKind::NotNull]);
    assert_eq!(column_kinds(text, 1), vec![ColumnConstraintKind::Null]);
    assert!(matches!(
        column_kinds(text, 2).as_slice(),
        [ColumnConstraintKind::Default(_)]
    ));
    assert!(matches!(
        column_kinds(text, 3).as_slice(),
        [
            ColumnConstraintKind::NotNull,
            ColumnConstraintKind::Default(_)
        ]
    ));
    assert!(matches!(
        column_kinds(text, 4).as_slice(),
        [ColumnConstraintKind::Default(_)]
    ));
    assert_eq!(
        column_constraint_name(text, 4).map(|name| name.value),
        Some("DF_e".to_owned())
    );
    assert_eq!(rt(text), text);

    // The reverse order gives the same set of constraints, in the order written: they are
    // a `Vec`, not a set of fields, which is what keeps the re-serialisation faithful.
    let reversed = "CREATE TABLE t (d varchar(10) DEFAULT 'x' NOT NULL)";
    assert!(matches!(
        column_kinds(reversed, 0).as_slice(),
        [
            ColumnConstraintKind::Default(_),
            ColumnConstraintKind::NotNull
        ]
    ));
    assert_eq!(rt(reversed), reversed);
}

#[test]
fn create_table_identity() {
    let identity = |text: &str| columns(text)[0].identity.clone();
    let bare = identity("CREATE TABLE t (id int IDENTITY)");
    assert_eq!(
        bare.as_ref().map(|i| (i.seed, i.increment)),
        Some((None, None))
    );
    assert_eq!(
        identity("CREATE TABLE t (id int IDENTITY(1, 1))").map(|i| (i.seed, i.increment)),
        Some((Some(1), Some(1)))
    );
    // The sign is read as part of the argument, not as an expression.
    assert_eq!(
        identity("CREATE TABLE t (id int IDENTITY(10, -2))").map(|i| (i.seed, i.increment)),
        Some((Some(10), Some(-2)))
    );
    assert_eq!(
        identity("CREATE TABLE t (id int IDENTITY(+10, +2))").map(|i| (i.seed, i.increment)),
        Some((Some(10), Some(2)))
    );

    assert_eq!(
        rt("CREATE TABLE t (id int IDENTITY)"),
        "CREATE TABLE t (id int IDENTITY)"
    );
    assert_eq!(
        rt("CREATE TABLE t (id int IDENTITY(10, -2))"),
        "CREATE TABLE t (id int IDENTITY(10, -2))"
    );
    // Assumed deviation: `IDENTITY` is a field of the column, not one of its
    // constraints, so it is always written right after the type. The tree stays equal,
    // which is what `rt` checks.
    assert_eq!(
        rt("CREATE TABLE t (id int PRIMARY KEY IDENTITY(1,1))"),
        "CREATE TABLE t (id int IDENTITY(1, 1) PRIMARY KEY)"
    );
}

#[test]
fn create_table_column_constraints() {
    let text = "CREATE TABLE t (id int PRIMARY KEY, u int UNIQUE NONCLUSTERED, \
                f int REFERENCES dbo.p (id) ON DELETE CASCADE ON UPDATE NO ACTION, \
                g int FOREIGN KEY REFERENCES p (id), c int CHECK (c > 0), \
                n int CONSTRAINT PK_n PRIMARY KEY CLUSTERED)";
    assert_eq!(
        column_kinds(text, 0),
        vec![ColumnConstraintKind::PrimaryKey {
            clustering: None,
            order: None
        }]
    );
    assert_eq!(
        column_kinds(text, 1),
        vec![ColumnConstraintKind::Unique {
            clustering: Some(Clustering::NonClustered),
            order: None
        }]
    );
    match &column_kinds(text, 2)[0] {
        ColumnConstraintKind::ForeignKey(reference) => {
            assert_eq!(
                reference.table.schema.as_ref().map(|s| s.value.clone()),
                Some("dbo".to_owned())
            );
            assert_eq!(reference.table.name.value, "p");
            assert_eq!(reference.columns.len(), 1);
            assert_eq!(reference.columns[0].value, "id");
            assert_eq!(reference.on_delete, Some(RefAction::Cascade));
            assert_eq!(reference.on_update, Some(RefAction::NoAction));
        }
        other => unreachable!("column f is a foreign key, got {other:?}"),
    }
    // `FOREIGN KEY` is optional in a column constraint and leaves no trace in the AST.
    match &column_kinds(text, 3)[0] {
        ColumnConstraintKind::ForeignKey(reference) => {
            assert_eq!(reference.table.name.value, "p");
            assert!(reference.table.schema.is_none());
            assert_eq!(reference.on_delete, None);
            assert_eq!(reference.on_update, None);
        }
        other => unreachable!("column g is a foreign key, got {other:?}"),
    }
    assert!(matches!(
        column_kinds(text, 4).as_slice(),
        [ColumnConstraintKind::Check {
            not_for_replication: false,
            ..
        }]
    ));
    assert_eq!(
        column_kinds(text, 5),
        vec![ColumnConstraintKind::PrimaryKey {
            clustering: Some(Clustering::Clustered),
            order: None
        }]
    );
    assert_eq!(
        column_constraint_name(text, 5).map(|name| name.value),
        Some("PK_n".to_owned())
    );
    for index in 0..5 {
        assert!(column_constraint_name(text, index).is_none());
    }

    // Assumed deviation: `FOREIGN KEY` is dropped from a column constraint, since the
    // AST keeps only the reference. The tree stays equal, which is what `rt` checks.
    assert_eq!(
        rt("CREATE TABLE t (g int FOREIGN KEY REFERENCES p (id))"),
        "CREATE TABLE t (g int REFERENCES p (id))"
    );
    // A reference without a column list means the primary key of the referenced table.
    let implied = "CREATE TABLE t (g int REFERENCES p)";
    match &column_kinds(implied, 0)[0] {
        ColumnConstraintKind::ForeignKey(reference) => assert!(reference.columns.is_empty()),
        other => unreachable!("column g is a foreign key, got {other:?}"),
    }
    assert_eq!(rt(implied), implied);
    for spelling in [
        "CREATE TABLE t (a int REFERENCES p (id) ON DELETE NO ACTION)",
        "CREATE TABLE t (a int REFERENCES p (id) ON DELETE SET NULL)",
        "CREATE TABLE t (a int REFERENCES p (id) ON UPDATE SET DEFAULT)",
        "CREATE TABLE t (a int REFERENCES p (id) ON DELETE CASCADE ON UPDATE CASCADE)",
    ] {
        assert_eq!(rt(spelling), spelling);
    }
    // `ON UPDATE` may be written first; the AST holds one field per action, so `Display`
    // always writes `ON DELETE` first (assumed deviation).
    assert_eq!(
        rt("CREATE TABLE t (a int REFERENCES p (id) ON UPDATE CASCADE ON DELETE SET NULL)"),
        "CREATE TABLE t (a int REFERENCES p (id) ON DELETE SET NULL ON UPDATE CASCADE)"
    );
}

#[test]
fn create_table_table_constraints() {
    let text = "CREATE TABLE t (a int, b int, \
                CONSTRAINT PK_t PRIMARY KEY CLUSTERED (a ASC, b DESC), \
                CONSTRAINT UQ_t UNIQUE (b), \
                CONSTRAINT FK_t FOREIGN KEY (a) REFERENCES dbo.p (id) ON DELETE SET NULL, \
                CONSTRAINT CK_t CHECK (a > b), CHECK (b > 0))";
    let constraints = table_constraints(text);
    assert_eq!(constraints.len(), 5);
    let names: Vec<Option<String>> = constraints
        .iter()
        .map(|constraint| constraint.name.as_ref().map(|name| name.value.clone()))
        .collect();
    assert_eq!(
        names,
        vec![
            Some("PK_t".to_owned()),
            Some("UQ_t".to_owned()),
            Some("FK_t".to_owned()),
            Some("CK_t".to_owned()),
            // The last one is unnamed: SQL Server generates its name.
            None,
        ]
    );
    match &constraints[0].kind {
        TableConstraintKind::PrimaryKey {
            columns,
            clustering,
        } => {
            assert_eq!(constraints[0].storage, IndexStorage::default());
            assert_eq!(*clustering, Some(Clustering::Clustered));
            assert_eq!(columns.len(), 2);
            assert_eq!(columns[0].name.value, "a");
            assert!(!columns[0].desc);
            assert!(columns[0].explicit_direction);
            assert_eq!(columns[1].name.value, "b");
            assert!(columns[1].desc);
        }
        other => unreachable!("the first constraint is a primary key, got {other:?}"),
    }
    match &constraints[1].kind {
        TableConstraintKind::Unique {
            columns,
            clustering,
        } => {
            assert_eq!(constraints[1].storage, IndexStorage::default());
            assert_eq!(*clustering, None);
            assert_eq!(columns.len(), 1);
            // No direction written: `Display` writes none back.
            assert!(!columns[0].explicit_direction);
        }
        other => unreachable!("the second constraint is unique, got {other:?}"),
    }
    match &constraints[2].kind {
        TableConstraintKind::ForeignKey { columns, reference } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].value, "a");
            assert_eq!(reference.table.name.value, "p");
            assert_eq!(reference.on_delete, Some(RefAction::SetNull));
            assert_eq!(reference.on_update, None);
        }
        other => unreachable!("the third constraint is a foreign key, got {other:?}"),
    }
    assert!(matches!(
        constraints[3].kind,
        TableConstraintKind::Check { .. }
    ));
    assert!(matches!(
        constraints[4].kind,
        TableConstraintKind::Check { .. }
    ));
    assert_eq!(rt(text), text);

    // A key with no `CLUSTERED` and no direction, and the unnamed spellings.
    for spelling in [
        "CREATE TABLE t (a int, PRIMARY KEY (a))",
        "CREATE TABLE t (a int, UNIQUE NONCLUSTERED (a))",
        "CREATE TABLE t (a int, b int, FOREIGN KEY (a, b) REFERENCES p (x, y))",
    ] {
        assert_eq!(rt(spelling), spelling);
    }
}

#[test]
fn create_table_computed_column() {
    let text = "CREATE TABLE t (a int, b AS a + 1)";
    let columns = columns(text);
    assert_eq!(columns.len(), 2);
    assert!(columns[0].computed.is_none());
    assert!(columns[1].computed.is_some());
    assert_eq!(rt(text), text);

    // Assumed deviation: `PERSISTED` is accepted and dropped, the AST has no field for
    // it, so the two spellings share one tree.
    let persisted = "CREATE TABLE t (a int, b AS a + 1 PERSISTED)";
    assert_eq!(rt(persisted), text);
    assert_eq!(p(persisted), p(text));
}

#[test]
fn create_table_collate() {
    let text = "CREATE TABLE t (a varchar(10) COLLATE SQL_Latin1_General_CP1_CS_AS NOT NULL)";
    let columns = columns(text);
    assert_eq!(
        columns[0].collation.as_deref(),
        Some("SQL_Latin1_General_CP1_CS_AS")
    );
    assert_eq!(column_kinds(text, 0), vec![ColumnConstraintKind::NotNull]);
    assert_eq!(rt(text), text);
}

#[test]
fn create_table_trailing_clauses() {
    // `ON …` and `TEXTIMAGE_ON …` reach the AST and come back.
    let created = create("CREATE TABLE t (a int) ON [PRIMARY]");
    assert_eq!(
        created.placement,
        Some(StoragePlacement {
            name: Ident {
                value: "PRIMARY".to_owned(),
                quoted: true,
            },
            partition_column: None,
        })
    );
    assert_eq!(created.textimage_on, None);
    assert_eq!(
        rt("CREATE TABLE t (a int) ON [PRIMARY]"),
        "CREATE TABLE t (a int) ON [PRIMARY]"
    );
    let created = create("CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]");
    assert!(created.placement.is_some());
    assert_eq!(
        created.textimage_on,
        Some(Ident {
            value: "PRIMARY".to_owned(),
            quoted: true,
        })
    );
    assert_eq!(
        rt("CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]"),
        "CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]"
    );
    // A partition scheme and its column, and the delimited `"default"` filegroup.
    let created = create("CREATE TABLE t (a int) ON ps (a)");
    assert_eq!(
        created
            .placement
            .and_then(|placement| placement.partition_column),
        Some(Ident {
            value: "a".to_owned(),
            quoted: false,
        })
    );
    assert_eq!(
        rt("CREATE TABLE t (a int) ON ps (a) TEXTIMAGE_ON [PRIMARY]"),
        "CREATE TABLE t (a int) ON ps (a) TEXTIMAGE_ON [PRIMARY]"
    );
    assert_eq!(
        rt("CREATE TABLE t (a int) ON \"default\""),
        "CREATE TABLE t (a int) ON [default]"
    );
    // `TEXTIMAGE_ON` alone is accepted at the parse: SQL Server 2022 accepts it on a
    // table without any large-object column too.
    assert_eq!(
        rt("CREATE TABLE t (a int) TEXTIMAGE_ON [PRIMARY]"),
        "CREATE TABLE t (a int) TEXTIMAGE_ON [PRIMARY]"
    );
    // Assumed deviation: the `WITH (…)` of a table is read and
    // thrown away, so the re-serialisation does not write it back.
    assert_eq!(
        rt("CREATE TABLE t (a int) WITH (DATA_COMPRESSION = PAGE)"),
        "CREATE TABLE t (a int)"
    );
    assert_eq!(
        rt("CREATE TABLE t (a int) ON [PRIMARY] WITH (DATA_COMPRESSION = PAGE)"),
        "CREATE TABLE t (a int) ON [PRIMARY]"
    );
    assert_eq!(
        rt(
            "CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY] WITH (DATA_COMPRESSION = PAGE)"
        ),
        "CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]"
    );
    assert_eq!(
        rt("CREATE TABLE t (a int) TEXTIMAGE_ON [PRIMARY] WITH (DATA_COMPRESSION = PAGE)"),
        "CREATE TABLE t (a int) TEXTIMAGE_ON [PRIMARY]"
    );
    // The clauses end the statement: what follows is another statement of the batch.
    assert_eq!(
        p("CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY] SELECT 1")
            .statements
            .len(),
        2
    );
}

/// The orders and spellings of the trailing clauses SQL Server 2022 refuses, each with
/// the number and the token it reports. The two that SQL Server refuses after the parse
/// (152 for a
/// second `TEXTIMAGE_ON`, 319 for a second `WITH`) are out of the catalogue: VaubanDB
/// reports a plain syntax error on the repeated word instead.
#[test]
fn create_table_trailing_clauses_refused() {
    for (text, number, near) in [
        // The bare `PRIMARY` is a reserved word, not a name.
        ("CREATE TABLE t (a int) ON PRIMARY", 156, "PRIMARY"),
        ("CREATE TABLE t (a int) ON default", 156, "default"),
        (
            "CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON PRIMARY",
            156,
            "PRIMARY",
        ),
        // `ON` comes first, `TEXTIMAGE_ON` second, `WITH` last, each once.
        (
            "CREATE TABLE t (a int) TEXTIMAGE_ON [PRIMARY] ON [PRIMARY]",
            156,
            "ON",
        ),
        (
            "CREATE TABLE t (a int) ON [PRIMARY] WITH (DATA_COMPRESSION = PAGE) TEXTIMAGE_ON [PRIMARY]",
            102,
            "TEXTIMAGE_ON",
        ),
        (
            "CREATE TABLE t (a int) ON [PRIMARY] ON [PRIMARY]",
            156,
            "ON",
        ),
        // SQL Server: 152; VaubanDB: the syntax error of the second word.
        (
            "CREATE TABLE t (a int) TEXTIMAGE_ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]",
            102,
            "TEXTIMAGE_ON",
        ),
        // SQL Server: 319; VaubanDB: the syntax error of the second `WITH`.
        (
            "CREATE TABLE t (a int) WITH (DATA_COMPRESSION = PAGE) WITH (DATA_COMPRESSION = PAGE)",
            156,
            "WITH",
        ),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, number, "{text}: {}", error.message);
        assert!(
            error.message.contains(&format!("'{near}'")),
            "{text}: {}",
            error.message
        );
    }
}

#[test]
fn alter_table_add() {
    let one = alter("ALTER TABLE t ADD c int NULL");
    assert_eq!(one.name.name.value, "t");
    match &one.action {
        AlterTableAction::AddColumns {
            columns,
            with_check,
        } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].name.value, "c");
            assert_eq!(columns[0].constraints.len(), 1);
            assert_eq!(*with_check, None);
        }
        other => unreachable!("ADD c int NULL adds a column, got {other:?}"),
    }
    match alter("ALTER TABLE t ADD c int, d varchar(10)").action {
        AlterTableAction::AddColumns { columns, .. } => assert_eq!(columns.len(), 2),
        other => unreachable!("two columns were added, got {other:?}"),
    }
    match alter("ALTER TABLE t ADD CONSTRAINT PK_t PRIMARY KEY (a)").action {
        AlterTableAction::AddConstraints {
            constraints,
            with_check,
        } => {
            assert_eq!(with_check, None);
            assert_eq!(constraints.len(), 1);
            assert_eq!(
                constraints[0].name.as_ref().map(|name| name.value.clone()),
                Some("PK_t".to_owned())
            );
        }
        other => unreachable!("a constraint was added, got {other:?}"),
    }
    for text in [
        "ALTER TABLE t ADD c int NULL",
        "ALTER TABLE t ADD c int, d varchar(10)",
        "ALTER TABLE t ADD CONSTRAINT PK_t PRIMARY KEY (a)",
        "ALTER TABLE dbo.t ADD CONSTRAINT FK_t FOREIGN KEY (a) REFERENCES p (id)",
        "ALTER TABLE t ADD CHECK (a > 0)",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn alter_table_drop() {
    match alter("ALTER TABLE t DROP COLUMN c").action {
        AlterTableAction::DropColumns { names, if_exists } => {
            assert_eq!(names.len(), 1);
            assert_eq!(names[0].value, "c");
            assert!(!if_exists);
        }
        other => unreachable!("a column was dropped, got {other:?}"),
    }
    match alter("ALTER TABLE t DROP COLUMN IF EXISTS c, d").action {
        AlterTableAction::DropColumns { names, if_exists } => {
            assert_eq!(names.len(), 2);
            assert!(if_exists);
        }
        other => unreachable!("two columns were dropped, got {other:?}"),
    }
    match alter("ALTER TABLE t DROP CONSTRAINT PK_t").action {
        AlterTableAction::DropConstraints { names, if_exists } => {
            assert_eq!(names.len(), 1);
            assert_eq!(names[0].value, "PK_t");
            assert!(!if_exists);
        }
        other => unreachable!("a constraint was dropped, got {other:?}"),
    }
    for text in [
        "ALTER TABLE t DROP COLUMN c",
        "ALTER TABLE t DROP COLUMN IF EXISTS c, d",
        "ALTER TABLE t DROP CONSTRAINT PK_t",
        "ALTER TABLE t DROP CONSTRAINT IF EXISTS PK_t, UQ_t",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn alter_table_alter_and_check() {
    match alter("ALTER TABLE t ALTER COLUMN c varchar(50) NOT NULL").action {
        AlterTableAction::AlterColumn(column) => {
            assert_eq!(column.name.value, "c");
            assert_eq!(column.ty.name, "varchar");
            assert_eq!(column.ty.args, vec![TypeArg::Number(50)]);
            assert_eq!(
                column
                    .constraints
                    .iter()
                    .map(|constraint| constraint.kind.clone())
                    .collect::<Vec<_>>(),
                vec![ColumnConstraintKind::NotNull]
            );
        }
        other => unreachable!("a column was altered, got {other:?}"),
    }
    assert_eq!(
        rt("ALTER TABLE t ALTER COLUMN c varchar(50) NOT NULL"),
        "ALTER TABLE t ALTER COLUMN c varchar(50) NOT NULL"
    );

    // Yes, `CHECK` twice: the first belongs to `WITH CHECK`, the second is the verb.
    // An empty name list means `ALL`, which is reserved and can never be an `Ident`.
    match alter("ALTER TABLE t WITH CHECK CHECK CONSTRAINT ALL").action {
        AlterTableAction::Check {
            constraints,
            enable,
            with_check,
        } => {
            assert!(constraints.is_empty());
            assert!(enable);
            assert!(with_check);
        }
        other => unreachable!("constraints were re-enabled, got {other:?}"),
    }
    assert_eq!(
        rt("ALTER TABLE t WITH CHECK CHECK CONSTRAINT ALL"),
        "ALTER TABLE t WITH CHECK CHECK CONSTRAINT ALL"
    );
    match alter("ALTER TABLE t NOCHECK CONSTRAINT FK_t").action {
        AlterTableAction::Check {
            constraints,
            enable,
            with_check,
        } => {
            assert_eq!(constraints.len(), 1);
            assert_eq!(constraints[0].value, "FK_t");
            assert!(!enable);
            // No `WITH` clause written: T-SQL assumes `WITH NOCHECK` when a constraint is
            // re-enabled, and `Display` writes the assumed word back (assumed deviation).
            assert!(!with_check);
        }
        other => unreachable!("a constraint was disabled, got {other:?}"),
    }
    assert_eq!(
        rt("ALTER TABLE t NOCHECK CONSTRAINT FK_t"),
        "ALTER TABLE t WITH NOCHECK NOCHECK CONSTRAINT FK_t"
    );
    assert_eq!(
        rt("ALTER TABLE t WITH NOCHECK CHECK CONSTRAINT FK_t, CK_t"),
        "ALTER TABLE t WITH NOCHECK CHECK CONSTRAINT FK_t, CK_t"
    );
}

#[test]
fn drop_table() {
    let (names, if_exists) = drop_names("DROP TABLE t");
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].name.value, "t");
    assert!(!if_exists);

    let (names, if_exists) = drop_names("DROP TABLE IF EXISTS dbo.t");
    assert_eq!(names.len(), 1);
    assert_eq!(
        names[0].schema.as_ref().map(|s| s.value.clone()),
        Some("dbo".to_owned())
    );
    assert!(if_exists);

    let (names, _) = drop_names("DROP TABLE a, b, c");
    assert_eq!(names.len(), 3);

    for text in [
        "DROP TABLE t",
        "DROP TABLE IF EXISTS dbo.t",
        "DROP TABLE a, b, c",
        // A temporary table is just a name to the parser.
        "DROP TABLE #t",
    ] {
        assert_eq!(rt(text), text);
    }
}

#[test]
fn ddl_table_errors() {
    // A truncated batch names the last token read; the shapes below are 102.
    assert_eq!(p_err("CREATE TABLE t").number, 102);
    assert_eq!(p_err("ALTER TABLE t").number, 102);
    // SQL Server: `ALTER TABLE t ADD` => 102.
    assert_eq!(p_err("ALTER TABLE t ADD").number, 102);

    for (text, token) in [
        ("CREATE TABLE t ()", ")"),
        ("CREATE TABLE t (a)", ")"),
        ("CREATE TABLE t (a int,)", ")"),
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert!(error.message.contains(token), "{text}: {}", error.message);
    }

    // SQL Server: `CREATE TABLE t (a int, INDEX IX_a (a))` succeeds.
    // VaubanDB still refuses INDEX with 156, a known deviation.
    let error = p_err("CREATE TABLE t (a int, INDEX IX_a (a))");
    assert_eq!(error.number, 156, "{} {}", error.number, error.message);
    assert!(error.message.contains("INDEX"), "{}", error.message);

    // SQL Server answers the exact queries below with 102, 156, 102, 4902, 102.
    // `ALTER TABLE t DROP c` is accepted syntactically there (missing table => 4902),
    // while VaubanDB still refuses it with 102, a known deviation.
    for (text, number) in [
        ("CREATE TABLE t (a int CONSTRAINT c)", 102),
        (
            "CREATE TABLE t (a int REFERENCES p (id) ON DELETE RESTRICT)",
            156,
        ),
        ("CREATE TABLE t (a int IDENTITY(1))", 102),
        ("ALTER TABLE t DROP c", 102),
        ("ALTER TABLE t FOO", 102),
    ] {
        assert_eq!(p_err(text).number, number, "{text}");
    }
}

#[test]
fn create_table_names() {
    // A qualified name, a delimited one and a temporary table: all ordinary names here.
    for text in [
        "CREATE TABLE dbo.t (a int)",
        "CREATE TABLE d.dbo.t (a int)",
        "CREATE TABLE [my table] (a int)",
        "CREATE TABLE #t (a int)",
        "CREATE TABLE ##t (a int)",
    ] {
        assert_eq!(rt(text), text);
    }
    assert_eq!(create("CREATE TABLE #t (a int)").name.name.value, "#t");
    // A column whose name is a reserved word has to be delimited, and stays delimited.
    assert_eq!(
        rt("CREATE TABLE t ([primary] int, [key] int)"),
        "CREATE TABLE t ([primary] int, [key] int)"
    );
}

// ---------------------------------------------------------------------------
// The storage clauses of a key, the `WITH CHECK ADD` and the named default, one batch
// per shape.
// ---------------------------------------------------------------------------

/// An unquoted identifier, for comparison against a parsed one.
fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

/// `[value]`, a delimited identifier.
fn quoted(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: true,
    }
}

/// `name = value`, one entry of a `WITH (…)` list.
fn option(name: &str, value: IndexOptionValue) -> IndexOption {
    IndexOption {
        name: name.to_owned(),
        value,
    }
}

/// The storage of the table-level constraint at `index` in the one `CREATE TABLE` of
/// `text`.
fn key_storage(text: &str, index: usize) -> IndexStorage {
    table_constraints(text)[index].storage.clone()
}

/// Asserts that `text` fails with `number` and that the message names `near`.
fn refused(text: &str, number: u32, near: &str) {
    let error = p_err(text);
    assert_eq!(error.number, number, "{text}: {}", error.message);
    assert!(
        error.message.contains(&format!("'{near}'")),
        "{text}: {}",
        error.message
    );
}

#[test]
fn table_constraint_storage_clauses() {
    // The SSMS shape: `)WITH (…) ON [PRIMARY]` after a clustered primary key.
    let text = "CREATE TABLE [dbo].[Orders](\n\t[Id] [int] IDENTITY(1,1) NOT NULL,\n \
                CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED \n(\n\t[Id] ASC\n)WITH \
                (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, IGNORE_DUP_KEY = OFF, \
                ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) \
                ON [PRIMARY]\n) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]";
    let storage = key_storage(text, 0);
    assert_eq!(
        storage.options,
        vec![
            option("PAD_INDEX", IndexOptionValue::Off),
            option("STATISTICS_NORECOMPUTE", IndexOptionValue::Off),
            option("IGNORE_DUP_KEY", IndexOptionValue::Off),
            option("ALLOW_ROW_LOCKS", IndexOptionValue::On),
            option("ALLOW_PAGE_LOCKS", IndexOptionValue::On),
            option("OPTIMIZE_FOR_SEQUENTIAL_KEY", IndexOptionValue::Off),
        ]
    );
    assert_eq!(
        storage.placement,
        Some(StoragePlacement {
            name: quoted("PRIMARY"),
            partition_column: None,
        })
    );
    let printed = rt(text);
    assert_eq!(
        printed,
        "CREATE TABLE [dbo].[Orders] ([Id] [int] IDENTITY(1, 1) NOT NULL, CONSTRAINT [PK_Orders] \
         PRIMARY KEY CLUSTERED ([Id] ASC) WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, \
         IGNORE_DUP_KEY = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, \
         OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]"
    );
    // The option list does not disappear at re-serialisation (acceptance criterion).
    assert!(printed.contains("WITH (PAD_INDEX = OFF"), "{printed}");

    // A unique key, each clause alone, a partition scheme, the four value shapes.
    let text = "CREATE TABLE t (a int, b int, CONSTRAINT UQ_t UNIQUE NONCLUSTERED (b) \
                WITH (PAD_INDEX = OFF), CONSTRAINT PK_t PRIMARY KEY (a) ON ps (a))";
    assert_eq!(
        key_storage(text, 0),
        IndexStorage {
            options: vec![option("PAD_INDEX", IndexOptionValue::Off)],
            placement: None,
        }
    );
    assert_eq!(
        key_storage(text, 1),
        IndexStorage {
            options: Vec::new(),
            placement: Some(StoragePlacement {
                name: ident("ps"),
                partition_column: Some(ident("a")),
            }),
        }
    );
    assert_eq!(rt(text), text);
    let text = "CREATE TABLE t (a int, PRIMARY KEY (a) WITH (FILLFACTOR = 80, ONLINE = ON, \
                DATA_COMPRESSION = PAGE, MAXDOP = 4))";
    assert_eq!(
        key_storage(text, 0).options,
        vec![
            // `FILLFACTOR` is a reserved word and still an option name.
            option("FILLFACTOR", IndexOptionValue::Integer("80".to_owned())),
            option("ONLINE", IndexOptionValue::On),
            option(
                "DATA_COMPRESSION",
                IndexOptionValue::Word("PAGE".to_owned())
            ),
            option("MAXDOP", IndexOptionValue::Integer("4".to_owned())),
        ]
    );
    assert_eq!(rt(text), text);
    // The delimited `"default"` filegroup, and `[default]`.
    assert_eq!(
        rt("CREATE TABLE t (a int, PRIMARY KEY (a) ON \"default\")"),
        "CREATE TABLE t (a int, PRIMARY KEY (a) ON [default])"
    );
    // `DECLARE @t TABLE` shares the body and accepts the clause, as SQL Server does.
    assert_eq!(
        rt("DECLARE @t TABLE (a int, PRIMARY KEY (a) WITH (FILLFACTOR = 80))"),
        "DECLARE @t TABLE (a int, PRIMARY KEY (a) WITH (FILLFACTOR = 80))"
    );
    // `ALTER TABLE … ADD` takes the clauses too, on each key of the list.
    let text = "ALTER TABLE t ADD CONSTRAINT PK_t PRIMARY KEY CLUSTERED (a) WITH (FILLFACTOR = 80) \
                ON [PRIMARY], CONSTRAINT UQ_t UNIQUE (b) WITH (PAD_INDEX = OFF)";
    match alter(text).action {
        AlterTableAction::AddConstraints { constraints, .. } => {
            assert_eq!(constraints.len(), 2);
            assert!(matches!(
                constraints[1].kind,
                TableConstraintKind::Unique { .. }
            ));
            assert_eq!(constraints[1].storage.options.len(), 1);
            assert!(constraints[1].storage.placement.is_none());
        }
        other => unreachable!("two keys were added, got {other:?}"),
    }
    assert_eq!(rt(text), text);
}

#[test]
fn column_constraint_storage_clauses() {
    let text = "CREATE TABLE t (a int PRIMARY KEY WITH (FILLFACTOR = 80) ON [PRIMARY], \
                b int UNIQUE NONCLUSTERED DESC WITH (PAD_INDEX = OFF))";
    assert_eq!(
        column_kinds(text, 0),
        vec![ColumnConstraintKind::PrimaryKey {
            clustering: None,
            order: None,
        }]
    );
    let columns = columns(text);
    assert_eq!(
        columns[0].constraints[0].storage,
        IndexStorage {
            options: vec![option(
                "FILLFACTOR",
                IndexOptionValue::Integer("80".to_owned())
            )],
            placement: Some(StoragePlacement {
                name: quoted("PRIMARY"),
                partition_column: None,
            }),
        }
    );
    assert!(matches!(
        columns[1].constraints[0].kind,
        ColumnConstraintKind::Unique { .. }
    ));
    assert_eq!(columns[1].constraints[0].storage.options.len(), 1);
    assert_eq!(rt(text), text);
    // The clauses belong to the key: other constraints may follow them.
    let text = "CREATE TABLE t (a int PRIMARY KEY WITH (FILLFACTOR = 80) NOT NULL, \
                b int UNIQUE ON [PRIMARY] CHECK (b > 0))";
    assert_eq!(column_kinds(text, 0).len(), 2);
    assert_eq!(column_kinds(text, 1).len(), 2);
    assert_eq!(rt(text), text);
}

/// The storage clauses SQL Server 2022 refuses after a constraint, with its number and
/// the token it names, all reproduced.
#[test]
fn constraint_storage_clauses_refused() {
    for (text, number, near) in [
        // The bare `PRIMARY` is a reserved word, not a filegroup name.
        (
            "CREATE TABLE t (a int, CONSTRAINT pk PRIMARY KEY (a) ON PRIMARY)",
            156,
            "PRIMARY",
        ),
        // `WITH` and `ON` are for keys only.
        (
            "CREATE TABLE t (a int, CONSTRAINT fk FOREIGN KEY (a) REFERENCES u (a) WITH (PAD_INDEX = OFF))",
            156,
            "WITH",
        ),
        (
            "CREATE TABLE t (a int, CONSTRAINT ck CHECK (a > 0) ON [PRIMARY])",
            156,
            "ON",
        ),
        // `WITH` once, then `ON` once.
        (
            "CREATE TABLE t (a int, CONSTRAINT pk PRIMARY KEY (a) WITH (PAD_INDEX = OFF) WITH (FILLFACTOR = 80))",
            156,
            "WITH",
        ),
        (
            "CREATE TABLE t (a int, CONSTRAINT pk PRIMARY KEY (a) ON [PRIMARY] WITH (PAD_INDEX = OFF))",
            156,
            "WITH",
        ),
        (
            "CREATE TABLE t (a int PRIMARY KEY ON [PRIMARY] WITH (FILLFACTOR = 80))",
            156,
            "WITH",
        ),
        // The list is parenthesised and never empty.
        (
            "CREATE TABLE t (a int, CONSTRAINT pk PRIMARY KEY (a) WITH ())",
            102,
            ")",
        ),
    ] {
        refused(text, number, near);
    }
    // Two shapes SQL Server accepts and this grammar does not: the
    // old `WITH FILLFACTOR = 80` without parentheses, and a `WITH (…)` after a
    // column-level `DEFAULT` (102 near '(' on SQL Server).
    assert_eq!(
        p_err("CREATE TABLE t (a int, CONSTRAINT pk PRIMARY KEY (a) WITH FILLFACTOR = 80)").number,
        156
    );
    assert_eq!(
        p_err("CREATE TABLE t (a int DEFAULT 0 WITH (FILLFACTOR = 80))").number,
        156
    );
}

/// The shapes of an option that SQL Server 2022 refuses at the parse, with its number and
/// the token it names, all reproduced. For `FOO = 'x'` SQL Server reports 155 (unknown
/// option) **and then** 102 near 'x'; VaubanDB reports the 102 only.
#[test]
fn index_option_shapes_refused() {
    for (text, number, near) in [
        (
            "CREATE INDEX ix ON t (a) WITH ([PAD_INDEX] = OFF)",
            102,
            "PAD_INDEX",
        ),
        (
            "CREATE INDEX ix ON t (a) WITH (SELECT = OFF)",
            156,
            "SELECT",
        ),
        (
            "CREATE INDEX ix ON t (a) WITH (FOO = SELECT)",
            156,
            "SELECT",
        ),
        ("CREATE INDEX ix ON t (a) WITH (FOO = NULL)", 156, "NULL"),
        ("CREATE INDEX ix ON t (a) WITH (FOO = 'x')", 102, "x"),
        (
            "CREATE INDEX ix ON t (a) WITH (DATA_COMPRESSION = [PAGE])",
            102,
            "PAGE",
        ),
        ("CREATE INDEX ix ON t (a) WITH (FILLFACTOR = +1)", 102, "+"),
        ("CREATE INDEX ix ON t (a) WITH (PAD_INDEX)", 102, ")"),
        ("CREATE INDEX ix ON t (a) WITH (PAD_INDEX = )", 102, ")"),
        ("CREATE INDEX ix ON t (a) WITH (PAD_INDEX = OFF,)", 102, ")"),
    ] {
        refused(text, number, near);
    }
}

/// Options the grammar accepts and SQL Server 2022 refuses **after** the parse, each
/// with its number: 155 for an unknown name (`FOO = ON`), 153 for a value that
/// does not fit a known name (`PAD_INDEX = 3`), 129 for `FILLFACTOR = -1`, 1080 for
/// `FILLFACTOR = 1.5`, 156 near 'ON' for `FILLFACTOR = ON`, 102 near 'abc' for a bare
/// word given to an unknown name (`FOO = abc`). None of the four numbers that are not
/// syntax errors is raised here: the option list is open, and checking the names belongs
/// to the catalog. The two syntax errors of this group (`FILLFACTOR = ON`, `FOO = abc`)
/// are assumed deviations: reproducing them needs the closed list of option names and
/// value types that the open list rules out.
#[test]
fn index_options_accepted_at_parse() {
    for text in [
        "CREATE INDEX ix ON t (a) WITH (FOO = ON)",
        "CREATE INDEX ix ON t (a) WITH (PAD_INDEX = 3)",
        "CREATE INDEX ix ON t (a) WITH (FILLFACTOR = ON)",
        "CREATE INDEX ix ON t (a) WITH (FOO = abc)",
        "CREATE INDEX ix ON t (a) WITH (PAD_INDEX = OFF, PAD_INDEX = OFF)",
        "CREATE INDEX ix ON t (a) WITH (DATA_COMPRESSION = NONE, FOO = ROW, BAR = COLUMNSTORE)",
        "DROP INDEX ix ON t WITH (FOO = ON)",
        "CREATE TABLE t (a int, CONSTRAINT pk PRIMARY KEY (a) WITH (FOO = ON))",
    ] {
        assert_eq!(rt(text), text);
    }
    // Signed and decimal values are not read: 102 on the sign or the number, where SQL
    // Server reads them and then reports 129 or 1080.
    refused("CREATE INDEX ix ON t (a) WITH (FILLFACTOR = -1)", 102, "-");
    refused(
        "CREATE INDEX ix ON t (a) WITH (FILLFACTOR = 1.5)",
        102,
        "1.5",
    );
}

#[test]
fn alter_table_with_check_add() {
    // The SSMS shapes.
    let text = "ALTER TABLE [dbo].[Orders]  WITH CHECK ADD  CONSTRAINT [FK_Orders_Customers] \
                FOREIGN KEY([CustomerId])\nREFERENCES [dbo].[Customers] ([Id])";
    match alter(text).action {
        AlterTableAction::AddConstraints {
            constraints,
            with_check,
        } => {
            assert_eq!(with_check, Some(ConstraintCheck::Check));
            assert_eq!(constraints.len(), 1);
            assert!(matches!(
                constraints[0].kind,
                TableConstraintKind::ForeignKey { .. }
            ));
        }
        other => unreachable!("a foreign key was added, got {other:?}"),
    }
    assert_eq!(
        rt(text),
        "ALTER TABLE [dbo].[Orders] WITH CHECK ADD CONSTRAINT [FK_Orders_Customers] FOREIGN KEY \
         ([CustomerId]) REFERENCES [dbo].[Customers] ([Id])"
    );
    let text = "ALTER TABLE [dbo].[Orders]  WITH CHECK ADD  CONSTRAINT [CK_Orders_Total] CHECK  (([Total]>=(0)))";
    assert_eq!(
        rt(text),
        "ALTER TABLE [dbo].[Orders] WITH CHECK ADD CONSTRAINT [CK_Orders_Total] CHECK (([Total] >= (0)))"
    );
    // `WITH NOCHECK`, and the prefix in front of a column, which SQL Server accepts.
    match alter("ALTER TABLE t WITH NOCHECK ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES u (a)")
        .action
    {
        AlterTableAction::AddConstraints { with_check, .. } => {
            assert_eq!(with_check, Some(ConstraintCheck::NoCheck));
        }
        other => unreachable!("a foreign key was added, got {other:?}"),
    }
    match alter("ALTER TABLE t WITH CHECK ADD c int").action {
        AlterTableAction::AddColumns {
            columns,
            with_check,
        } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(with_check, Some(ConstraintCheck::Check));
        }
        other => unreachable!("a column was added, got {other:?}"),
    }
    for text in [
        "ALTER TABLE t WITH NOCHECK ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES u (a)",
        "ALTER TABLE t WITH CHECK ADD CONSTRAINT pk PRIMARY KEY (a)",
        "ALTER TABLE t WITH CHECK ADD c int",
        // The prefix still opens the `CHECK CONSTRAINT` verb.
        "ALTER TABLE t WITH CHECK NOCHECK CONSTRAINT ALL",
        "ALTER TABLE t WITH NOCHECK CHECK CONSTRAINT fk, ck",
    ] {
        assert_eq!(rt(text), text);
    }
    // What SQL Server refuses after the prefix: `DROP`, `ALTER COLUMN`, anything that is
    // neither `CHECK` nor `NOCHECK` after `WITH`.
    refused("ALTER TABLE t WITH CHECK DROP CONSTRAINT c", 156, "DROP");
    refused("ALTER TABLE t WITH CHECK ALTER COLUMN a int", 156, "ALTER");
    refused("ALTER TABLE t WITH SELECT 1", 156, "SELECT");
    refused("ALTER TABLE t WITH CHECK SELECT 1", 156, "SELECT");
    // A batch that stops after `WITH CHECK`: 102 on the last token read, where SQL Server
    // names the `WITH` (a 102 near 'WITH'): assumed deviation of the end-of-batch rule.
    refused("ALTER TABLE t WITH CHECK", 102, "CHECK");
}

#[test]
fn alter_table_add_default_for() {
    let text = "ALTER TABLE [dbo].[Orders] ADD  CONSTRAINT [DF_Orders_Status]  DEFAULT (N'New') FOR [Status]";
    match alter(text).action {
        AlterTableAction::AddDefaults {
            defaults,
            with_check,
        } => {
            assert_eq!(with_check, None);
            assert_eq!(defaults.len(), 1);
            assert_eq!(defaults[0].name, Some(quoted("DF_Orders_Status")));
            assert_eq!(defaults[0].expr.to_string(), "(N'New')");
            assert_eq!(defaults[0].column, quoted("Status"));
        }
        other => unreachable!("a default was added, got {other:?}"),
    }
    match alter("ALTER TABLE t ADD DEFAULT 0 FOR a, CONSTRAINT df2 DEFAULT 1 FOR b").action {
        AlterTableAction::AddDefaults { defaults, .. } => {
            assert_eq!(defaults.len(), 2);
            assert_eq!(defaults[0].name, None);
            assert_eq!(defaults[1].name, Some(ident("df2")));
        }
        other => unreachable!("two defaults were added, got {other:?}"),
    }
    assert_eq!(
        rt(text),
        "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [DF_Orders_Status] DEFAULT (N'New') FOR [Status]"
    );
    for text in [
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT ((0)) FOR [a]",
        // Unnamed, and two in one statement: both accepted by SQL Server.
        "ALTER TABLE t ADD DEFAULT 0 FOR a",
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT 0 FOR a, CONSTRAINT df2 DEFAULT 1 FOR b",
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT getdate() FOR a",
        "ALTER TABLE t WITH CHECK ADD CONSTRAINT df DEFAULT 0 FOR a",
    ] {
        assert_eq!(rt(text), text);
    }
    // Refusals, with the number and token SQL Server reports.
    refused(
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT 0 FOR select",
        156,
        "select",
    );
    refused(
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT 0 FOR a.b",
        102,
        ".",
    );
    refused("ALTER TABLE t ADD DEFAULT 0 FOR", 102, "FOR");
    // Without `FOR`, SQL Server reports 142 (`Incorrect syntax for definition of the
    // 'TABLE' constraint.`), which is not in the catalogue: a plain syntax error here.
    refused("ALTER TABLE t ADD CONSTRAINT df DEFAULT 0", 102, "0");
    refused("ALTER TABLE t ADD CONSTRAINT df DEFAULT 0, b int", 102, ",");
    // A default mixed with a column or a key in one `ADD`: SQL Server 2022 accepts the
    // two shapes below; the AST holds one kind of added element, so they are refused
    // here, as a column mixed with a key is.
    refused(
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT 0 FOR a, b int",
        102,
        "b",
    );
    refused(
        "ALTER TABLE t ADD b int, CONSTRAINT df DEFAULT 0 FOR a",
        156,
        "CONSTRAINT",
    );
    // `WITH VALUES` is accepted by SQL Server and not read here.
    refused(
        "ALTER TABLE t ADD CONSTRAINT df DEFAULT 0 FOR a WITH VALUES",
        156,
        "WITH",
    );
}

/// A `DEFAULT` constraint inside a table body: SQL Server reads the expression and refuses
/// the `FOR` with a 102 near 'for', lower-cased whatever was written; without a `FOR` it
/// reports 142, which is not in the catalogue (a plain 102 here).
#[test]
fn default_constraint_in_table_body_refused() {
    for text in [
        "CREATE TABLE t (a int, CONSTRAINT df DEFAULT 0 FOR a)",
        "CREATE TABLE t (a int, CONSTRAINT df DEFAULT 0 fOr a)",
        "CREATE TABLE t (a int, DEFAULT 0 FOR a)",
        "DECLARE @t TABLE (a int, CONSTRAINT df DEFAULT 0 FOR a)",
        "CREATE TABLE t (a int, CONSTRAINT df DEFAULT 0 FOR a) SELECT 1",
    ] {
        let error = p_err(text);
        assert_eq!(error.number, 102, "{text}: {}", error.message);
        assert_eq!(error.message, "Syntax error near 'for'.", "{text}");
        assert_eq!(error.line, 1, "{text}");
    }
    refused(
        "CREATE TABLE t (a int, CONSTRAINT df DEFAULT 0 x)",
        102,
        "x",
    );
    refused("CREATE TABLE t (a int, CONSTRAINT df DEFAULT 0)", 102, ")");
}

/// A character string names a filegroup as well as an identifier does:
/// `ON 'PRIMARY'` and `ON N'PRIMARY'` parse on SQL Server 2022 on a table, a table-level
/// key, a column-level key, an index and `TEXTIMAGE_ON`. The string becomes a delimited
/// name, so `Display` writes `[PRIMARY]` and the tree survives the loop; `ON ps ('a')`
/// stays 102 near 'a', as on SQL Server.
#[test]
fn filegroup_name_as_string() {
    let created = create("CREATE TABLE t (a int) ON 'PRIMARY' TEXTIMAGE_ON N'PRIMARY'");
    assert_eq!(
        created.placement,
        Some(StoragePlacement {
            name: quoted("PRIMARY"),
            partition_column: None,
        })
    );
    assert_eq!(created.textimage_on, Some(quoted("PRIMARY")));
    for (text, printed) in [
        (
            "CREATE TABLE t (a int) ON 'PRIMARY'",
            "CREATE TABLE t (a int) ON [PRIMARY]",
        ),
        (
            "CREATE TABLE t (a int) ON N'PRIMARY'",
            "CREATE TABLE t (a int) ON [PRIMARY]",
        ),
        (
            "CREATE TABLE t (a int, PRIMARY KEY (a) ON 'PRIMARY')",
            "CREATE TABLE t (a int, PRIMARY KEY (a) ON [PRIMARY])",
        ),
        (
            "CREATE TABLE t (a int PRIMARY KEY ON 'PRIMARY')",
            "CREATE TABLE t (a int PRIMARY KEY ON [PRIMARY])",
        ),
        (
            "CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON 'PRIMARY'",
            "CREATE TABLE t (a int) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]",
        ),
        (
            "CREATE INDEX ix ON t (a) ON 'PRIMARY'",
            "CREATE INDEX ix ON t (a) ON [PRIMARY]",
        ),
    ] {
        assert_eq!(rt(text), printed);
        assert_eq!(
            p(text),
            p(printed),
            "{text}: the string and the delimited name give one tree"
        );
    }
    refused("CREATE INDEX ix ON t (a) ON ps ('a')", 102, "a");
}

/// A temporary name is not an option name nor an option value: SQL Server
/// 2022 refuses `WITH (ONLINE = #x)` and `= ##x` with 102 near '#x'/'##x' on a key, a
/// `CREATE INDEX` and a `DROP INDEX`, and `WITH (#x = ON)` with 155 (unknown option, a
/// number this open list does not raise: 102 near '#x' here).
#[test]
fn index_option_temporary_names_refused() {
    for (text, near) in [
        (
            "CREATE TABLE t (a int, PRIMARY KEY (a) WITH (ONLINE = #x))",
            "#x",
        ),
        (
            "CREATE TABLE t (a int, PRIMARY KEY (a) WITH (ONLINE = ##x))",
            "##x",
        ),
        ("CREATE INDEX ix ON t (a) WITH (ONLINE = #x)", "#x"),
        ("DROP INDEX ix ON t WITH (ONLINE = #x)", "#x"),
        (
            "CREATE TABLE t (a int, PRIMARY KEY (a) WITH (#x = ON))",
            "#x",
        ),
    ] {
        refused(text, 102, near);
    }
}
