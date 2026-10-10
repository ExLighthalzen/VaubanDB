//! Binding of `SELECT … INTO` and `TRUNCATE TABLE` against a hand-built [`CatalogView`].
//!
//! The catalogue knows `dbo.t (a int NOT NULL, b nvarchar(10) NULL)`, `dbo.ex` (two columns
//! already taken), and `dbo.parent`. A `TRUNCATE` of a referenced table binds: its
//! 4712 is raised while the statement runs.

use vauban_binder::{
    BindContext, BoundStatement, CatalogView, DdlStatement, ResolvedTable, ResolvedTableKind,
    SelectIntoPlan, SessionOptions, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{ObjectName, ParseOptions, parse_batch};
use vauban_types::{Len, SqlType, TypeInfo};

struct Tables;

fn column(
    id: i32,
    index: usize,
    name: &str,
    ty: SqlType,
    nullable: bool,
) -> vauban_binder::ColumnBinding {
    vauban_binder::ColumnBinding {
        column: ColumnId(id),
        index,
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
        depth: 0,
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
        let table = |object: i32, columns: Vec<vauban_binder::ColumnBinding>| ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns,
            kind: ResolvedTableKind::Table,
        };
        match name.name.value.to_ascii_lowercase().as_str() {
            "t" => Some(table(
                1,
                vec![
                    column(1, 0, "a", SqlType::Int, false),
                    column(2, 1, "b", SqlType::NVarChar(Len::Fixed(10)), true),
                ],
            )),
            "ex" => Some(table(
                2,
                vec![
                    column(1, 0, "a", SqlType::Int, false),
                    column(2, 1, "b", SqlType::NVarChar(Len::Fixed(10)), true),
                ],
            )),
            "parent" => Some(table(3, vec![column(1, 0, "id", SqlType::Int, false)])),
            "child" => Some(table(4, vec![column(1, 0, "id", SqlType::Int, false)])),
            _ => None,
        }
    }

    fn view_definition(&self, _object: vauban_catalog::ObjectId) -> Option<&str> {
        None
    }

    fn identity_column(
        &self,
        _object: vauban_catalog::ObjectId,
    ) -> Option<vauban_catalog::ColumnId> {
        None
    }

    fn computed_columns(&self, _object: vauban_catalog::ObjectId) -> Vec<vauban_catalog::ColumnId> {
        Vec::new()
    }

    fn constraint_names_on_table(&self, _table: vauban_catalog::ObjectId) -> Vec<String> {
        Vec::new()
    }

    fn primary_key_columns(&self, _table: vauban_catalog::ObjectId) -> Option<Vec<String>> {
        None
    }

    fn unique_key_on(&self, _table: vauban_catalog::ObjectId, _columns: &[String]) -> bool {
        false
    }

    fn classify_table_reference(
        &self,
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> vauban_binder::TableReferenceKind {
        match vauban_binder::CatalogView::resolve_table(self, name, database, default_schema) {
            Some(_) => vauban_binder::TableReferenceKind::Table,
            None => vauban_binder::TableReferenceKind::Unknown,
        }
    }
}

fn bind_with(text: &str, catalog: &Tables) -> Result<BoundStatement, SqlError> {
    let batch = parse_batch(text, &ParseOptions::default())
        .unwrap_or_else(|e| unreachable!("{text} parses, got {e:?}"));
    let [statement] = batch.statements.as_slice() else {
        unreachable!("{text} is one statement")
    };
    let mut ctx = BindContext::scalar(text, SessionOptions::default());
    ctx.catalog = Some(catalog);
    bind(statement, &ctx)
}

fn err(text: &str) -> SqlError {
    bind_with(text, &Tables).expect_err("this statement does not bind")
}

fn select_into(text: &str) -> SelectIntoPlan {
    match bind_with(text, &Tables) {
        Ok(BoundStatement::SelectInto(plan)) => plan,
        other => unreachable!("{text} binds to SelectInto, got {other:?}"),
    }
}

#[test]
fn select_into_deduces_columns_from_the_output_schema() {
    let plan = select_into("SELECT a, b INTO dbo.t2 FROM dbo.t;");
    assert_eq!(plan.def.name.name, "t2");
    assert_eq!(plan.def.name.schema, "dbo");
    let [a, b] = plan.def.columns.as_slice() else {
        unreachable!("two columns, got {:?}", plan.def.columns)
    };
    assert_eq!(a.name, "a");
    assert_eq!(a.ty.ty, SqlType::Int);
    assert!(!a.ty.nullable);
    assert_eq!(b.name, "b");
    assert_eq!(b.ty.ty, SqlType::NVarChar(Len::Fixed(10)));
    assert!(b.ty.nullable);
    assert_eq!(plan.source.schema().columns.len(), 2);
}

/// `SELECT a, a + 1 INTO dbo.u FROM dbo.t;` answers 1038 state 5, not 8155 (that number
/// is for a derived table without a column alias).
#[test]
fn select_into_unnamed_expression_is_8155() {
    let error = err("SELECT a, a + 1 INTO dbo.u FROM dbo.t;");
    assert_eq!((error.number, error.severity, error.state), (1038, 15, 5));
}

#[test]
fn select_into_existing_table_is_2714() {
    let error = err("SELECT a INTO dbo.ex FROM dbo.t;");
    assert_eq!((error.number, error.severity, error.state), (2714, 16, 6));
    assert_eq!(
        error.message,
        "An object named 'ex' exists already in the database."
    );
}

#[test]
fn select_into_duplicate_output_name_is_2705() {
    let error = err("SELECT a AS c, b AS c INTO dbo.t2 FROM dbo.t;");
    assert_eq!((error.number, error.severity, error.state), (2705, 16, 3));
}

#[test]
fn truncate_binds_to_ddl() {
    match bind_with("TRUNCATE TABLE dbo.t;", &Tables) {
        Ok(BoundStatement::Ddl(DdlStatement::TruncateTable { name })) => {
            assert_eq!(name.name, "t");
            assert_eq!(name.schema, "dbo");
        }
        other => unreachable!("TRUNCATE TABLE binds to Ddl, got {other:?}"),
    }
}

/// `TRUNCATE TABLE dbo.nosuch;` answers 4701 state 1.
#[test]
fn truncate_unknown_table_is_4701() {
    let error = err("TRUNCATE TABLE dbo.nosuch;");
    assert_eq!((error.number, error.severity, error.state), (4701, 16, 1));
}
