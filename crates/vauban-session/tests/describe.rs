//! Describing a T-SQL batch without running it, from an `EXEC` in a batch.
//!
//! The column list of the two result sets is the compatibility layer's; this file registers
//! a resolver carrying the same list so the session runs its own path end to end, and checks
//! what the session puts on the wire: the COLMETADATA the caller decodes, and one row per
//! described column. The resolver also serves `sp_executesql`, which brings the describe call
//! one nesting level down.

use std::sync::{Arc, Once};

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{
    DescribeKind, Engine, ProcAction, ProcArg, ResultSink, Session, SessionState,
    StaticResultColumn, register_system_procedure_resolver,
};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

const SPID: i16 = 91;

/// Name, type and nullability of each column of the first-result-set description, in order.
const FIRST_RESULT_SET: &[(&str, SqlType, bool)] = &[
    ("is_hidden", SqlType::Bit, false),
    ("column_ordinal", SqlType::Int, false),
    ("name", SqlType::NVarChar(Len::Fixed(128)), true),
    ("is_nullable", SqlType::Bit, false),
    ("system_type_id", SqlType::Int, false),
    ("system_type_name", SqlType::NVarChar(Len::Fixed(128)), true),
    ("max_length", SqlType::SmallInt, false),
    ("precision", SqlType::TinyInt, false),
    ("scale", SqlType::TinyInt, false),
    ("collation_name", SqlType::NVarChar(Len::Fixed(128)), true),
    ("user_type_id", SqlType::Int, true),
    (
        "user_type_database",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    ("user_type_schema", SqlType::NVarChar(Len::Fixed(128)), true),
    ("user_type_name", SqlType::NVarChar(Len::Fixed(128)), true),
    (
        "assembly_qualified_type_name",
        SqlType::NVarChar(Len::Fixed(4_000)),
        true,
    ),
    ("xml_collection_id", SqlType::Int, true),
    (
        "xml_collection_database",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "xml_collection_schema",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "xml_collection_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    ("is_xml_document", SqlType::Bit, false),
    ("is_case_sensitive", SqlType::Bit, false),
    ("is_fixed_length_clr_type", SqlType::Bit, false),
    ("source_server", SqlType::NVarChar(Len::Fixed(128)), true),
    ("source_database", SqlType::NVarChar(Len::Fixed(128)), true),
    ("source_schema", SqlType::NVarChar(Len::Fixed(128)), true),
    ("source_table", SqlType::NVarChar(Len::Fixed(128)), true),
    ("source_column", SqlType::NVarChar(Len::Fixed(128)), true),
    ("is_identity_column", SqlType::Bit, true),
    ("is_part_of_unique_key", SqlType::Bit, true),
    ("is_updateable", SqlType::Bit, true),
    ("is_computed_column", SqlType::Bit, true),
    ("is_sparse_column_set", SqlType::Bit, true),
    ("ordinal_in_order_by_list", SqlType::SmallInt, true),
    ("order_by_is_descending", SqlType::Bit, true),
    ("order_by_list_length", SqlType::SmallInt, true),
    ("tds_type_id", SqlType::Int, false),
    ("tds_length", SqlType::Int, false),
    ("tds_collation_id", SqlType::Int, true),
    ("tds_collation_sort_id", SqlType::TinyInt, true),
];

/// Name, type and nullability of each column of the undeclared-parameter description.
const UNDECLARED_PARAMETERS: &[(&str, SqlType, bool)] = &[
    ("parameter_ordinal", SqlType::Int, false),
    ("name", SqlType::NVarChar(Len::Fixed(128)), false),
    ("suggested_system_type_id", SqlType::Int, false),
    (
        "suggested_system_type_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    ("suggested_max_length", SqlType::SmallInt, false),
    ("suggested_precision", SqlType::TinyInt, false),
    ("suggested_scale", SqlType::TinyInt, false),
    ("suggested_user_type_id", SqlType::Int, true),
    (
        "suggested_user_type_database",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "suggested_user_type_schema",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "suggested_user_type_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "suggested_assembly_qualified_type_name",
        SqlType::NVarChar(Len::Fixed(4_000)),
        true,
    ),
    ("suggested_xml_collection_id", SqlType::Int, true),
    (
        "suggested_xml_collection_database",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "suggested_xml_collection_schema",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    (
        "suggested_xml_collection_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    ("suggested_is_xml_document", SqlType::Bit, false),
    ("suggested_is_case_sensitive", SqlType::Bit, false),
    ("suggested_is_fixed_length_clr_type", SqlType::Bit, false),
    ("suggested_is_input", SqlType::Bit, false),
    ("suggested_is_output", SqlType::Bit, false),
    (
        "formal_parameter_name",
        SqlType::NVarChar(Len::Fixed(128)),
        true,
    ),
    ("suggested_tds_type_id", SqlType::Int, false),
    ("suggested_tds_length", SqlType::Int, false),
];

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

#[derive(Default)]
struct Recording {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    errors: Vec<u32>,
}

impl Recording {
    /// The value of `column` in the row at `index`.
    fn cell(&self, index: usize, column: &str) -> &Value {
        let position = self
            .columns
            .iter()
            .position(|meta| meta.name == column)
            .unwrap_or_else(|| panic!("{column} is a column of the description"));
        &self.rows[index][position]
    }
}

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.columns = cols.to_vec();
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }
    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn done_in_proc(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn done_proc(&mut self, _rowcount: Option<u64>) -> SqlResult<()> {
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.errors.push(err.number);
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }
    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

fn columns_of(kind: DescribeKind) -> Vec<StaticResultColumn> {
    let list = match kind {
        DescribeKind::FirstResultSet => FIRST_RESULT_SET,
        DescribeKind::UndeclaredParameters => UNDECLARED_PARAMETERS,
    };
    list.iter()
        .map(|(name, ty, nullable)| StaticResultColumn {
            name: (*name).to_owned(),
            ty: TypeInfo::new(*ty, *nullable),
        })
        .collect()
}

fn first_text(args: &[ProcArg<'_>]) -> Option<String> {
    match args.first().map(|arg| arg.value) {
        Some(Value::String(text)) => Some(text.text.clone()),
        _ => None,
    }
}

fn resolve_describe(name: &str, args: &[ProcArg<'_>]) -> Option<Result<ProcAction, SqlError>> {
    let base = name.replace(['[', ']'], "");
    let base = base.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    // `sp_executesql` is served here too, so that a describe call reached through dynamic SQL
    // travels the nested path (`describing_through_dynamic_sql_answers_the_same_row`).
    if base == "sp_executesql" {
        let statement = first_text(args)?;
        return Some(Ok(ProcAction::ExecuteSql {
            statement,
            params: Vec::new(),
        }));
    }
    let kind = match base.as_str() {
        "sp_describe_first_result_set" => DescribeKind::FirstResultSet,
        "sp_describe_undeclared_parameters" => DescribeKind::UndeclaredParameters,
        _ => return None,
    };
    let Some(statement) = first_text(args) else {
        return Some(Err(SqlError::procedure_expects_parameter(&base, "@tsql")));
    };
    Some(Ok(ProcAction::Describe {
        statement,
        kind,
        columns: columns_of(kind),
    }))
}

fn session() -> Session {
    vauban_sysfn::register_builtins();
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| register_system_procedure_resolver(resolve_describe));
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(SPID),
    )
}

fn run(session: &mut Session, text: &str) -> Recording {
    let mut sink = Recording::default();
    session.run_batch(text, &mut sink).expect("batch runs");
    sink
}

#[test]
fn describing_a_literal_names_its_alias() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT 1 AS n'",
    );
    assert!(sink.errors.is_empty(), "no error: {:?}", sink.errors);
    assert_eq!(sink.rows.len(), 1);
    assert_eq!(
        sink.cell(0, "name"),
        &text("n"),
        "the alias names the column"
    );
    assert_eq!(sink.cell(0, "column_ordinal"), &Value::I32(1));
    assert_eq!(sink.cell(0, "system_type_name"), &text("int"));
    assert_eq!(sink.cell(0, "is_nullable"), &Value::Bit(false));
}

#[test]
fn the_metadata_carries_the_thirty_nine_columns_in_order() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT 1 AS n'",
    );
    let names: Vec<&str> = sink.columns.iter().map(|col| col.name.as_str()).collect();
    let expected: Vec<&str> = FIRST_RESULT_SET.iter().map(|(name, ..)| *name).collect();
    assert_eq!(names, expected);
    assert_eq!(sink.columns.len(), 39);

    let column = |name: &str| {
        sink.columns
            .iter()
            .find(|col| col.name == name)
            .unwrap_or_else(|| panic!("{name} is sent"))
    };
    assert_eq!(column("is_hidden").ty.ty, SqlType::Bit);
    assert!(!column("is_hidden").flags.nullable);
    assert_eq!(column("name").ty.ty, SqlType::NVarChar(Len::Fixed(128)));
    assert!(column("name").flags.nullable);
    assert_eq!(column("max_length").ty.ty, SqlType::SmallInt);
    assert_eq!(column("precision").ty.ty, SqlType::TinyInt);
    assert_eq!(
        column("assembly_qualified_type_name").ty.ty,
        SqlType::NVarChar(Len::Fixed(4_000))
    );
    assert_eq!(column("tds_collation_sort_id").ty.ty, SqlType::TinyInt);
    assert!(column("tds_collation_sort_id").flags.nullable);
}

#[test]
fn the_constant_columns_name_no_product() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT 1 AS n'",
    );
    for name in [
        "source_server",
        "source_database",
        "source_schema",
        "source_table",
        "source_column",
        "user_type_database",
        "user_type_schema",
        "user_type_name",
        "assembly_qualified_type_name",
    ] {
        assert_eq!(sink.cell(0, name), &Value::Null, "{name} carries no name");
    }
    assert_eq!(sink.cell(0, "collation_name"), &Value::Null);
}

#[test]
fn describing_a_table_read_reports_its_column_types() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE dbo.t (a int NOT NULL, b int NULL, c nvarchar(50) NULL, d decimal(10,2) NOT NULL)",
    );
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT a, b, c, d FROM dbo.t'",
    );
    assert!(sink.errors.is_empty(), "no error: {:?}", sink.errors);
    assert_eq!(sink.rows.len(), 4);

    assert_eq!(sink.cell(0, "name"), &text("a"));
    assert_eq!(sink.cell(0, "is_nullable"), &Value::Bit(false));
    assert_eq!(sink.cell(0, "system_type_id"), &Value::I32(56));
    assert_eq!(sink.cell(0, "max_length"), &Value::I16(4));
    assert_eq!(sink.cell(0, "precision"), &Value::I8(10));
    assert_eq!(sink.cell(0, "tds_type_id"), &Value::I32(56));
    assert_eq!(sink.cell(0, "tds_length"), &Value::I32(4));
    assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(true));
    assert_eq!(sink.cell(0, "is_computed_column"), &Value::Bit(false));

    assert_eq!(sink.cell(1, "is_nullable"), &Value::Bit(true));
    assert_eq!(sink.cell(1, "tds_type_id"), &Value::I32(38));

    assert_eq!(sink.cell(2, "system_type_name"), &text("nvarchar(50)"));
    assert_eq!(sink.cell(2, "system_type_id"), &Value::I32(231));
    assert_eq!(sink.cell(2, "max_length"), &Value::I16(100));
    assert_eq!(sink.cell(2, "tds_length"), &Value::I32(100));
    assert_eq!(
        sink.cell(2, "collation_name"),
        &text("SQL_Latin1_General_CP1_CI_AS")
    );
    assert_eq!(sink.cell(2, "tds_collation_id"), &Value::I32(13_632_521));
    assert_eq!(sink.cell(2, "tds_collation_sort_id"), &Value::I8(52));

    assert_eq!(sink.cell(3, "system_type_name"), &text("decimal(10,2)"));
    assert_eq!(sink.cell(3, "system_type_id"), &Value::I32(106));
    assert_eq!(sink.cell(3, "max_length"), &Value::I16(9));
    assert_eq!(sink.cell(3, "precision"), &Value::I8(10));
    assert_eq!(sink.cell(3, "scale"), &Value::I8(2));
    assert_eq!(sink.cell(3, "tds_length"), &Value::I32(17));
}

#[test]
fn describing_an_insert_leaves_the_table_unchanged() {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int NOT NULL)");
    let described = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'INSERT dbo.t (a) VALUES (1)'",
    );
    assert!(described.errors.is_empty(), "{:?}", described.errors);
    assert!(described.rows.is_empty(), "an INSERT has no result set");

    let read = run(&mut session, "SELECT a FROM dbo.t");
    assert!(read.rows.is_empty(), "the description inserted no row");
}

#[test]
fn describing_an_empty_batch_has_no_row() {
    let mut session = session();
    let sink = run(&mut session, "EXEC sys.sp_describe_first_result_set N''");
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert!(sink.rows.is_empty());
    assert_eq!(sink.columns.len(), 39);
}

#[test]
fn describing_an_unknown_table_fails_with_its_binding_error() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT x FROM dbo.nosuchtable'",
    );
    assert_eq!(sink.errors, vec![208]);
    assert!(sink.rows.is_empty());
}

#[test]
fn a_batch_that_declares_its_parameters_has_none_undeclared() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_undeclared_parameters N'SELECT 1 AS n'",
    );
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert!(sink.rows.is_empty());
    assert_eq!(sink.columns.len(), 24);
}

#[test]
fn the_first_result_set_of_a_batch_is_the_one_described() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT 1 AS a; SELECT 2 AS b;'",
    );
    assert_eq!(sink.rows.len(), 1);
    assert_eq!(sink.cell(0, "name"), &text("a"));
}

#[test]
fn describing_through_dynamic_sql_answers_the_same_row() {
    let mut session = session();
    let direct = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT 1 AS n'",
    );
    let nested = run(
        &mut session,
        "EXEC sp_executesql N'EXEC sys.sp_describe_first_result_set N''SELECT 1 AS n'''",
    );
    assert!(nested.errors.is_empty(), "{:?}", nested.errors);
    assert_eq!(nested.rows.len(), 1);
    assert_eq!(nested.cell(0, "name"), &text("n"));
    assert_eq!(nested.cell(0, "column_ordinal"), &Value::I32(1));
    assert_eq!(nested.cell(0, "system_type_name"), &text("int"));
    assert_eq!(nested.rows, direct.rows, "the nesting changes no cell");
    let nested_names: Vec<&str> = nested.columns.iter().map(|col| col.name.as_str()).collect();
    let direct_names: Vec<&str> = direct.columns.iter().map(|col| col.name.as_str()).collect();
    assert_eq!(nested_names, direct_names);
}

#[test]
fn a_binding_error_inside_dynamic_sql_reaches_the_caller() {
    let mut session = session();
    let sink = run(
        &mut session,
        "EXEC sp_executesql N'EXEC sys.sp_describe_first_result_set N''SELECT x FROM dbo.nosuchtable'''",
    );
    assert_eq!(sink.errors, vec![208]);
    assert!(sink.rows.is_empty());
}

#[test]
fn a_column_read_from_a_table_is_updateable() {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int NOT NULL)");
    for batch in [
        "EXEC sys.sp_describe_first_result_set N'SELECT a FROM dbo.t'",
        "EXEC sys.sp_describe_first_result_set N'SELECT a FROM dbo.t WHERE a = 1'",
        "EXEC sys.sp_describe_first_result_set N'SELECT DISTINCT a FROM dbo.t'",
        "EXEC sys.sp_describe_first_result_set N'SELECT a FROM dbo.t ORDER BY a'",
    ] {
        let sink = run(&mut session, batch);
        assert!(sink.errors.is_empty(), "{batch}: {:?}", sink.errors);
        assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(true), "{batch}");
        assert_eq!(
            sink.cell(0, "is_computed_column"),
            &Value::Bit(false),
            "{batch}"
        );
    }
}

#[test]
fn a_column_read_through_a_join_is_updateable() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE dbo.t (a int NOT NULL, b int NULL)",
    );
    run(
        &mut session,
        "CREATE TABLE dbo.u (a int NOT NULL, c nvarchar(20) NULL)",
    );
    for batch in [
        "EXEC sys.sp_describe_first_result_set N'SELECT t.b, u.c FROM dbo.t JOIN dbo.u ON t.a = u.a'",
        "EXEC sys.sp_describe_first_result_set N'SELECT t.b, u.c FROM dbo.t LEFT JOIN dbo.u ON t.a = u.a'",
        "EXEC sys.sp_describe_first_result_set N'SELECT t.b, u.c FROM dbo.t, dbo.u'",
        "EXEC sys.sp_describe_first_result_set N'SELECT b, c FROM (SELECT t.b, u.c FROM dbo.t JOIN dbo.u ON t.a = u.a) AS d'",
    ] {
        let sink = run(&mut session, batch);
        assert!(sink.errors.is_empty(), "{batch}: {:?}", sink.errors);
        for row in 0..2 {
            assert_eq!(
                sink.cell(row, "is_updateable"),
                &Value::Bit(true),
                "{batch}"
            );
            assert_eq!(
                sink.cell(row, "is_computed_column"),
                &Value::Bit(false),
                "{batch}"
            );
        }
    }
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT t.a + 1 AS s, u.c FROM dbo.t JOIN dbo.u ON t.a = u.a'",
    );
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(false));
    assert_eq!(sink.cell(0, "is_computed_column"), &Value::Bit(true));
    assert_eq!(sink.cell(1, "is_updateable"), &Value::Bit(true));
    assert_eq!(sink.cell(1, "is_computed_column"), &Value::Bit(false));
}

#[test]
fn a_value_the_query_computes_is_a_computed_column() {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int NOT NULL)");
    for batch in [
        "EXEC sys.sp_describe_first_result_set N'SELECT 1 AS n'",
        "EXEC sys.sp_describe_first_result_set N'SELECT a + 1 AS s FROM dbo.t'",
    ] {
        let sink = run(&mut session, batch);
        assert!(sink.errors.is_empty(), "{batch}: {:?}", sink.errors);
        assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(false), "{batch}");
        assert_eq!(
            sink.cell(0, "is_computed_column"),
            &Value::Bit(true),
            "{batch}"
        );
    }
}

#[test]
fn an_aggregate_is_neither_updateable_nor_computed() {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int NOT NULL)");
    for batch in [
        "EXEC sys.sp_describe_first_result_set N'SELECT COUNT(*) AS c FROM dbo.t'",
        "EXEC sys.sp_describe_first_result_set N'SELECT MAX(a) AS m FROM dbo.t'",
    ] {
        let sink = run(&mut session, batch);
        assert!(sink.errors.is_empty(), "{batch}: {:?}", sink.errors);
        assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(false), "{batch}");
        assert_eq!(
            sink.cell(0, "is_computed_column"),
            &Value::Bit(false),
            "{batch}"
        );
    }
}

#[test]
fn a_grouping_key_keeps_the_flags_of_the_column_it_reads() {
    let mut session = session();
    run(&mut session, "CREATE TABLE dbo.t (a int NOT NULL)");
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT a, COUNT(*) AS c FROM dbo.t GROUP BY a'",
    );
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(sink.rows.len(), 2);
    assert_eq!(sink.cell(0, "name"), &text("a"));
    assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(true));
    assert_eq!(sink.cell(0, "is_computed_column"), &Value::Bit(false));
    assert_eq!(sink.cell(1, "name"), &text("c"));
    assert_eq!(sink.cell(1, "is_updateable"), &Value::Bit(false));
    assert_eq!(sink.cell(1, "is_computed_column"), &Value::Bit(false));
}

#[test]
fn a_column_of_a_union_is_neither_updateable_nor_computed() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE dbo.t (a int NOT NULL, b int NULL)",
    );
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT a FROM dbo.t UNION ALL SELECT b FROM dbo.t'",
    );
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(sink.rows.len(), 1);
    assert_eq!(sink.cell(0, "is_updateable"), &Value::Bit(false));
    assert_eq!(sink.cell(0, "is_computed_column"), &Value::Bit(false));
}

#[test]
fn the_wire_token_of_a_column_follows_its_nullability() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE dbo.w (i int NOT NULL, j int NULL, b bit NOT NULL, c bit NULL, \
         f float NOT NULL, g float NULL, d datetime NOT NULL, e datetime NULL, \
         m money NOT NULL, n money NULL)",
    );
    let sink = run(
        &mut session,
        "EXEC sys.sp_describe_first_result_set N'SELECT i, j, b, c, f, g, d, e, m, n FROM dbo.w'",
    );
    assert!(sink.errors.is_empty(), "{:?}", sink.errors);
    assert_eq!(sink.rows.len(), 10);
    // Each pair is the same type, first without then with `NULL`.
    let tokens = [56, 38, 50, 104, 62, 109, 61, 111, 60, 110];
    for (row, token) in tokens.iter().enumerate() {
        assert_eq!(
            sink.cell(row, "tds_type_id"),
            &Value::I32(*token),
            "row {row}"
        );
    }
}
