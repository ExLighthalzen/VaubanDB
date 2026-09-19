//! Binding of `ALTER TABLE` against a hand-built [`CatalogView`].

use vauban_binder::{
    BindContext, BoundStatement, CatalogView, ColumnBinding, DdlStatement, NoVariables,
    ResolvedTable, ResolvedTableKind, SessionOptions, bind,
};
use vauban_catalog::{AlterTable, ConstraintDef, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo};

struct Tables {
    taken_names: Vec<String>,
    index_on_b: bool,
}

fn column(id: i32, index: usize, name: &str, ty: SqlType, nullable: bool) -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(id),
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

fn table(object: i32, columns: Vec<ColumnBinding>) -> ResolvedTable {
    ResolvedTable {
        object: ObjectId(object),
        table: Some(TableId(u32::try_from(object).expect("small id"))),
        columns,
        kind: ResolvedTableKind::Table,
    }
}

impl CatalogView for Tables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        if name.server.is_some() {
            return None;
        }
        if let Some(schema) = &name.schema
            && !schema.value.eq_ignore_ascii_case("dbo")
        {
            return None;
        }
        match name.name.value.to_ascii_lowercase().as_str() {
            "t" => Some(table(
                1,
                vec![
                    column(1, 0, "a", SqlType::Int, false),
                    column(2, 1, "b", SqlType::NVarChar(Len::Fixed(10)), true),
                ],
            )),
            "parent" => Some(table(2, vec![column(1, 0, "id", SqlType::Int, false)])),
            "refnopk" => Some(table(3, vec![column(1, 0, "x", SqlType::Int, true)])),
            "pkonly" => Some(table(
                4,
                vec![
                    column(1, 0, "a", SqlType::Int, false),
                    column(2, 1, "b", SqlType::Int, true),
                ],
            )),
            _ => None,
        }
    }

    fn index_on_column(&self, table: ObjectId, column: &str) -> Option<String> {
        (self.index_on_b && table == ObjectId(1) && column.eq_ignore_ascii_case("b"))
            .then(|| "ix_b".to_owned())
    }

    fn constraint_names_on_table(&self, table: ObjectId) -> Vec<String> {
        if table == ObjectId(1) {
            vec!["pk_t".to_owned(), "ck_t".to_owned()]
        } else {
            Vec::new()
        }
    }

    fn object_name_taken(&self, name: &str) -> bool {
        self.taken_names
            .iter()
            .any(|taken| taken.eq_ignore_ascii_case(name))
    }

    fn primary_key_columns(&self, table: ObjectId) -> Option<Vec<String>> {
        match table {
            ObjectId(1) => Some(vec!["a".to_owned()]),
            ObjectId(2) => Some(vec!["id".to_owned()]),
            ObjectId(4) => Some(vec!["a".to_owned()]),
            _ => None,
        }
    }

    fn primary_key_constraint_name(&self, table: ObjectId) -> Option<String> {
        match table {
            ObjectId(1) => Some("pk_t".to_owned()),
            ObjectId(4) => Some("pk_pkonly".to_owned()),
            _ => None,
        }
    }

    fn check_constraint_mentions_column(
        &self,
        table: ObjectId,
        constraint: &str,
        column: &str,
    ) -> bool {
        table == ObjectId(1) && constraint.eq_ignore_ascii_case("ck_t") && column == "a"
    }
}

fn ctx<'a>(text: &'a str, catalog: &'a Tables) -> BindContext<'a> {
    BindContext {
        text,
        catalog: Some(catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    }
}

fn err(text: &str, catalog: &Tables) -> SqlError {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("parse");
    bind(&batch.statements[0], &ctx(text, catalog)).expect_err("error")
}

fn alter(text: &str, catalog: &Tables) -> AlterTable {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("parse");
    match bind(&batch.statements[0], &ctx(text, catalog)).expect("bind") {
        BoundStatement::Ddl(DdlStatement::AlterTable { action, .. }) => action,
        other => panic!("expected AlterTable, got {other:?}"),
    }
}

fn catalog() -> Tables {
    Tables {
        taken_names: vec![],
        index_on_b: false,
    }
}

fn indexed_catalog() -> Tables {
    Tables {
        taken_names: vec![],
        index_on_b: true,
    }
}

#[test]
fn add_column_binds() {
    let action = alter("ALTER TABLE dbo.t ADD c int NULL;", &catalog());
    let AlterTable::AddColumn { column } = action else {
        panic!("expected AddColumn");
    };
    assert_eq!(column.name, "c");
    assert_eq!(column.ty.ty, SqlType::Int);
    assert!(column.ty.nullable);
}

#[test]
fn add_existing_column_is_2705() {
    let error = err("ALTER TABLE dbo.t ADD a int NULL;", &catalog());
    assert_eq!((error.number, error.severity, error.state), (2705, 16, 4));
}

#[test]
fn add_unknown_type_is_2715() {
    let error = err("ALTER TABLE dbo.t ADD c nosuch NULL;", &catalog());
    assert_eq!((error.number, error.severity, error.state), (2715, 16, 6));
    assert!(error.message.contains("#3"));
}

#[test]
fn drop_column_binds() {
    let action = alter("ALTER TABLE dbo.t DROP COLUMN b;", &catalog());
    assert_eq!(
        action,
        AlterTable::DropColumn {
            name: "b".to_owned()
        }
    );
}

#[test]
fn drop_unknown_column_is_4924() {
    let error = err("ALTER TABLE dbo.t DROP COLUMN nosuch;", &catalog());
    assert_eq!((error.number, error.severity, error.state), (4924, 16, 1));
}

#[test]
fn drop_indexed_column_is_5074() {
    let error = err("ALTER TABLE dbo.t DROP COLUMN b;", &indexed_catalog());
    assert_eq!((error.number, error.severity, error.state), (5074, 16, 1));
}

#[test]
fn drop_primary_key_column_is_5074() {
    let error = err("ALTER TABLE dbo.pkonly DROP COLUMN a;", &catalog());
    assert_eq!((error.number, error.severity, error.state), (5074, 16, 1));
}

#[test]
fn add_check_constraint_binds_a_predicate() {
    let action = alter(
        "ALTER TABLE dbo.t ADD CONSTRAINT ck_new CHECK (a > 0);",
        &catalog(),
    );
    let AlterTable::AddConstraint { constraint } = action else {
        panic!("expected AddConstraint");
    };
    match constraint.as_ref() {
        ConstraintDef::Check { name, .. } => assert_eq!(name.as_deref(), Some("ck_new")),
        other => panic!("expected Check, got {other:?}"),
    }
}

#[test]
fn add_check_non_predicate_is_4145() {
    let error = err(
        "ALTER TABLE dbo.t ADD CONSTRAINT ck_bad CHECK (1);",
        &catalog(),
    );
    assert_eq!(error.number, 4145);
}

#[test]
fn add_foreign_key_resolves_the_referenced_table() {
    let action = alter(
        "ALTER TABLE dbo.t ADD CONSTRAINT fk_t FOREIGN KEY (a) REFERENCES dbo.parent (id);",
        &catalog(),
    );
    let AlterTable::AddConstraint { constraint } = action else {
        panic!("expected AddConstraint");
    };
    match constraint.as_ref() {
        ConstraintDef::ForeignKey {
            referenced,
            referenced_columns,
            ..
        } => {
            assert_eq!(referenced.name, "parent");
            assert_eq!(referenced_columns, &["id".to_owned()]);
        }
        other => panic!("expected ForeignKey, got {other:?}"),
    }
}

#[test]
fn add_foreign_key_without_matching_key_is_1776() {
    let error = err(
        "ALTER TABLE dbo.t ADD CONSTRAINT fk_bad FOREIGN KEY (a) REFERENCES dbo.refnopk (x);",
        &catalog(),
    );
    assert_eq!((error.number, error.severity, error.state), (1776, 16, 0));
}

#[test]
fn drop_constraint_binds() {
    let action = alter("ALTER TABLE dbo.t DROP CONSTRAINT pk_t;", &catalog());
    assert_eq!(
        action,
        AlterTable::DropConstraint {
            name: "pk_t".to_owned()
        }
    );
}

#[test]
fn drop_unknown_constraint_is_3728() {
    let error = err("ALTER TABLE dbo.t DROP CONSTRAINT nosuch;", &catalog());
    assert_eq!((error.number, error.severity, error.state), (3728, 16, 1));
}

#[test]
fn add_not_null_without_default_binds() {
    let action = alter("ALTER TABLE dbo.t ADD c int NOT NULL;", &catalog());
    let AlterTable::AddColumn { column } = action else {
        panic!("expected AddColumn");
    };
    assert!(!column.ty.nullable);
    assert!(column.default.is_none());
}

#[test]
fn alter_column_names_v2() {
    let error = err("ALTER TABLE dbo.t ALTER COLUMN a int NULL;", &catalog());
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("ALTER COLUMN"));
    assert!(error.message.contains("V2"));
}

#[test]
fn alter_unknown_table_is_4902() {
    let error = err("ALTER TABLE dbo.nosuch ADD c int NULL;", &catalog());
    assert_eq!((error.number, error.severity, error.state), (4902, 16, 1));
}

#[test]
fn duplicate_constraint_name_is_2714() {
    let error = err(
        "ALTER TABLE dbo.t ADD CONSTRAINT taken CHECK (a > 0);",
        &Tables {
            taken_names: vec!["taken".to_owned()],
            index_on_b: false,
        },
    );
    assert_eq!((error.number, error.severity, error.state), (2714, 16, 5));
}
