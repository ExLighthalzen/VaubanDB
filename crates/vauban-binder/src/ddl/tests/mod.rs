use vauban_catalog::{ConstraintDef, ObjectId, QualifiedName, TableDef};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, RefAction, Statement, parse_batch};
use vauban_types::{Collation, Len, SqlType};

use crate::bound::{BoundStatement, DdlStatement};
use crate::context::{BindContext, CatalogView, ResolvedTable, ResolvedTableKind, SessionOptions};
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
fn bind_with(text: &str, catalog: Option<&dyn CatalogView>) -> Result<BoundStatement, SqlError> {
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
    let def =
        table_def("CREATE TABLE t (a int, b int, CONSTRAINT pk PRIMARY KEY (a DESC), UNIQUE (b));");
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

    let def = table_def("CREATE TABLE t (a int PRIMARY KEY NONCLUSTERED, b int UNIQUE CLUSTERED);");
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
/// statement leaves them out, and the referenced table is resolved like a `FROM` table.
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
            assert_eq!(referenced.database, "master");
            assert_eq!(referenced.schema, "dbo");
            assert_eq!(referenced.name, "other");
            assert_eq!(referenced_columns, &["b".to_owned()]);
            assert_eq!(*on_delete, RefAction::NoAction);
            assert_eq!(*on_update, RefAction::NoAction);
        }
        other => unreachable!("one FOREIGN KEY, got {other:?}"),
    }
}

/// `REFERENCES dbo.parent (id)` fills the current database before the constraint reaches
/// the catalogue.
#[test]
fn two_part_reference_fills_the_current_database() {
    let def = table_def("CREATE TABLE t (a int REFERENCES dbo.parent (id));");
    match def.constraints.as_slice() {
        [ConstraintDef::ForeignKey { referenced, .. }] => {
            assert_eq!(referenced.database, "master");
            assert_eq!(referenced.schema, "dbo");
            assert_eq!(referenced.name, "parent");
        }
        other => unreachable!("one FOREIGN KEY, got {other:?}"),
    }
}

/// A reference that names another database answers 1763 on the line of the statement.
#[test]
fn cross_database_foreign_key_is_1763() {
    let error = err("CREATE TABLE t (a int REFERENCES tempdb.dbo.parent (id));");
    assert_eq!((error.number, error.severity, error.state), (1763, 16, 0));
    assert_eq!(
        error.message,
        "Cross-database foreign key references are not supported. Foreign key \
         'tempdb.dbo.parent'."
    );
    assert_eq!(error.line, 1);
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

/// The dispatch of `statement.rs` sends the six statements of this file here, and not
/// one of them is a `SELECT`.
#[test]
fn the_six_statements_of_this_file_bind() {
    for text in [
        "CREATE DATABASE d;",
        "DROP DATABASE d;",
        "ALTER DATABASE d SET READ_COMMITTED_SNAPSHOT ON;",
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
