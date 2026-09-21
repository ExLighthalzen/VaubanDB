//! Resolution of the two procedures that describe a T-SQL batch without running it.

use vauban_compat::{
    DescribeKind, ProcAction, ProcArg, describe_columns, resolve_system_procedure,
};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

fn nvarchar(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

fn nvarchar_ty() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(Len::Max), false)
}

fn int_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

fn arg<'a>(name: Option<&'a str>, ty: &'a TypeInfo, value: &'a Value) -> ProcArg<'a> {
    ProcArg {
        name,
        ty,
        value,
        output: false,
        default: false,
    }
}

/// The names of the first result set, in the order the caller receives them.
const FIRST_RESULT_SET_NAMES: &[&str] = &[
    "is_hidden",
    "column_ordinal",
    "name",
    "is_nullable",
    "system_type_id",
    "system_type_name",
    "max_length",
    "precision",
    "scale",
    "collation_name",
    "user_type_id",
    "user_type_database",
    "user_type_schema",
    "user_type_name",
    "assembly_qualified_type_name",
    "xml_collection_id",
    "xml_collection_database",
    "xml_collection_schema",
    "xml_collection_name",
    "is_xml_document",
    "is_case_sensitive",
    "is_fixed_length_clr_type",
    "source_server",
    "source_database",
    "source_schema",
    "source_table",
    "source_column",
    "is_identity_column",
    "is_part_of_unique_key",
    "is_updateable",
    "is_computed_column",
    "is_sparse_column_set",
    "ordinal_in_order_by_list",
    "order_by_is_descending",
    "order_by_list_length",
    "tds_type_id",
    "tds_length",
    "tds_collation_id",
    "tds_collation_sort_id",
];

/// The names of the undeclared-parameter result set, in the order the caller receives them.
const UNDECLARED_PARAMETER_NAMES: &[&str] = &[
    "parameter_ordinal",
    "name",
    "suggested_system_type_id",
    "suggested_system_type_name",
    "suggested_max_length",
    "suggested_precision",
    "suggested_scale",
    "suggested_user_type_id",
    "suggested_user_type_database",
    "suggested_user_type_schema",
    "suggested_user_type_name",
    "suggested_assembly_qualified_type_name",
    "suggested_xml_collection_id",
    "suggested_xml_collection_database",
    "suggested_xml_collection_schema",
    "suggested_xml_collection_name",
    "suggested_is_xml_document",
    "suggested_is_case_sensitive",
    "suggested_is_fixed_length_clr_type",
    "suggested_is_input",
    "suggested_is_output",
    "formal_parameter_name",
    "suggested_tds_type_id",
    "suggested_tds_length",
];

fn describe_action(name: &str, args: &[ProcArg<'_>]) -> ProcAction {
    resolve_system_procedure(name, args)
        .unwrap_or_else(|| panic!("{name} is a known procedure"))
        .unwrap_or_else(|err| panic!("{name} resolves: {}", err.number))
}

fn describe_error(name: &str, args: &[ProcArg<'_>]) -> u32 {
    resolve_system_procedure(name, args)
        .unwrap_or_else(|| panic!("{name} is a known procedure"))
        .expect_err("refused")
        .number
}

#[test]
fn both_names_resolve_with_and_without_the_schema_brackets_and_case() {
    let text = nvarchar("SELECT 1 AS n");
    let ty = nvarchar_ty();
    let args = [arg(None, &ty, &text)];
    let spellings = [
        ("sp_describe_first_result_set", DescribeKind::FirstResultSet),
        (
            "sys.sp_describe_first_result_set",
            DescribeKind::FirstResultSet,
        ),
        (
            "[sys].[SP_Describe_First_Result_Set]",
            DescribeKind::FirstResultSet,
        ),
        (
            "SP_DESCRIBE_UNDECLARED_PARAMETERS",
            DescribeKind::UndeclaredParameters,
        ),
        (
            "sys.sp_describe_undeclared_parameters",
            DescribeKind::UndeclaredParameters,
        ),
        (
            "[sp_describe_undeclared_parameters]",
            DescribeKind::UndeclaredParameters,
        ),
    ];
    for (spelling, expected) in spellings {
        match describe_action(spelling, &args) {
            ProcAction::Describe {
                statement, kind, ..
            } => {
                assert_eq!(statement, "SELECT 1 AS n", "{spelling}");
                assert_eq!(kind, expected, "{spelling}");
            }
            other => panic!("{spelling} gave {other:?}"),
        }
    }
}

#[test]
fn named_and_positional_forms_give_the_same_text() {
    let text = nvarchar("SELECT 1 AS n");
    let ty = nvarchar_ty();
    let positional = describe_action("sp_describe_first_result_set", &[arg(None, &ty, &text)]);
    let named = describe_action(
        "sp_describe_first_result_set",
        &[arg(Some("@tsql"), &ty, &text)],
    );
    assert_eq!(positional, named);
}

#[test]
fn a_call_without_an_argument_is_201() {
    assert_eq!(describe_error("sp_describe_first_result_set", &[]), 201);
    assert_eq!(
        describe_error("sp_describe_undeclared_parameters", &[]),
        201
    );
}

#[test]
fn a_tsql_that_is_not_unicode_text_is_214() {
    let null = Value::Null;
    let text = nvarchar("SELECT 1 AS n");
    let ty = nvarchar_ty();
    let varchar = TypeInfo::new(SqlType::VarChar(Len::Fixed(50)), false);
    let int = int_ty();
    let one = Value::I32(1);
    for name in [
        "sp_describe_first_result_set",
        "sp_describe_undeclared_parameters",
    ] {
        assert_eq!(
            describe_error(name, &[arg(None, &ty, &null)]),
            214,
            "{name}"
        );
        assert_eq!(
            describe_error(name, &[arg(None, &varchar, &text)]),
            214,
            "{name} reads the batch from a Unicode string"
        );
        assert_eq!(
            describe_error(name, &[arg(None, &int, &one)]),
            214,
            "{name}"
        );
    }
    let nchar = TypeInfo::new(SqlType::NChar(Len::Fixed(13)), false);
    match describe_action("sp_describe_first_result_set", &[arg(None, &nchar, &text)]) {
        ProcAction::Describe { statement, .. } => assert_eq!(statement, "SELECT 1 AS n"),
        other => panic!("nchar carries the batch too, got {other:?}"),
    }
}

#[test]
fn a_declaration_list_or_a_browse_mode_is_refused_with_8144() {
    let text = nvarchar("SELECT 1 AS n");
    let params = nvarchar("@x int");
    let mode = Value::I8(1);
    let ty = nvarchar_ty();
    let tinyint = TypeInfo::new(SqlType::TinyInt, false);
    assert_eq!(
        describe_error(
            "sp_describe_first_result_set",
            &[arg(None, &ty, &text), arg(None, &ty, &params)]
        ),
        8144
    );
    assert_eq!(
        describe_error(
            "sp_describe_first_result_set",
            &[
                arg(None, &ty, &text),
                arg(None, &ty, &Value::Null),
                arg(None, &tinyint, &mode)
            ]
        ),
        8144
    );
    assert_eq!(
        describe_error(
            "sp_describe_undeclared_parameters",
            &[arg(None, &ty, &text), arg(None, &ty, &params)]
        ),
        8144
    );
}

#[test]
fn the_neutral_values_of_the_other_arguments_are_accepted() {
    let text = nvarchar("SELECT 1 AS n");
    let empty = nvarchar("");
    let zero = Value::I8(0);
    let ty = nvarchar_ty();
    let tinyint = TypeInfo::new(SqlType::TinyInt, false);
    let action = describe_action(
        "sp_describe_first_result_set",
        &[
            arg(None, &ty, &text),
            arg(None, &ty, &empty),
            arg(None, &tinyint, &zero),
        ],
    );
    match action {
        ProcAction::Describe { statement, .. } => assert_eq!(statement, "SELECT 1 AS n"),
        other => panic!("expected Describe, got {other:?}"),
    }
}

#[test]
fn an_argument_beyond_the_declared_ones_is_8144() {
    let text = nvarchar("SELECT 1 AS n");
    let ty = nvarchar_ty();
    let int = int_ty();
    let extra = Value::I32(9);
    assert_eq!(
        describe_error(
            "sp_describe_first_result_set",
            &[
                arg(None, &ty, &text),
                arg(None, &ty, &Value::Null),
                arg(None, &int, &Value::Null),
                arg(None, &int, &extra),
            ]
        ),
        8144
    );
    assert_eq!(
        describe_error(
            "sp_describe_undeclared_parameters",
            &[
                arg(None, &ty, &text),
                arg(None, &ty, &Value::Null),
                arg(None, &int, &extra),
            ]
        ),
        8144
    );
}

#[test]
fn a_name_does_not_move_its_argument() {
    let text = nvarchar("SELECT 1 AS n");
    let ty = nvarchar_ty();
    let mode = TypeInfo::new(SqlType::TinyInt, false);
    let zero = Value::I8(0);

    // A name the two procedures do not declare still fills the slot of its position.
    for written in ["@batch", "@sql", "@params"] {
        match describe_action(
            "sp_describe_first_result_set",
            &[arg(Some(written), &ty, &text)],
        ) {
            ProcAction::Describe { statement, .. } => assert_eq!(statement, "SELECT 1 AS n"),
            other => panic!("{written} gave {other:?}"),
        }
    }

    // Writing `@tsql` second leaves the first slot holding the other argument, which is
    // what the batch is read from.
    assert_eq!(
        describe_error(
            "sp_describe_first_result_set",
            &[
                arg(Some("@params"), &ty, &Value::Null),
                arg(Some("@tsql"), &ty, &text)
            ]
        ),
        214
    );
    assert_eq!(
        describe_error(
            "sp_describe_first_result_set",
            &[
                arg(Some("@browse_information_mode"), &mode, &zero),
                arg(Some("@tsql"), &ty, &text)
            ]
        ),
        214
    );

    // The same two arguments in the other order describe the batch.
    match describe_action(
        "sp_describe_first_result_set",
        &[
            arg(Some("@tsql"), &ty, &text),
            arg(Some("@params"), &ty, &Value::Null),
        ],
    ) {
        ProcAction::Describe { statement, .. } => assert_eq!(statement, "SELECT 1 AS n"),
        other => panic!("expected Describe, got {other:?}"),
    }
}

#[test]
fn the_first_result_set_has_its_thirty_nine_columns_in_order() {
    let columns = describe_columns(DescribeKind::FirstResultSet);
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    assert_eq!(names, FIRST_RESULT_SET_NAMES);
    assert_eq!(columns.len(), 39);
}

#[test]
fn the_undeclared_parameters_have_their_twenty_four_columns_in_order() {
    let columns = describe_columns(DescribeKind::UndeclaredParameters);
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    assert_eq!(names, UNDECLARED_PARAMETER_NAMES);
    assert_eq!(columns.len(), 24);
}

#[test]
fn the_column_types_of_the_first_result_set_are_the_declared_ones() {
    let columns = describe_columns(DescribeKind::FirstResultSet);
    let by_name = |name: &str| {
        columns
            .iter()
            .find(|column| column.name == name)
            .unwrap_or_else(|| panic!("{name} is a column"))
            .ty
            .clone()
    };
    assert_eq!(by_name("is_hidden").ty, SqlType::Bit);
    assert!(!by_name("is_hidden").nullable);
    assert_eq!(by_name("column_ordinal").ty, SqlType::Int);
    assert_eq!(by_name("name").ty, SqlType::NVarChar(Len::Fixed(128)));
    assert!(by_name("name").nullable);
    assert_eq!(by_name("max_length").ty, SqlType::SmallInt);
    assert_eq!(by_name("precision").ty, SqlType::TinyInt);
    assert_eq!(
        by_name("assembly_qualified_type_name").ty,
        SqlType::NVarChar(Len::Fixed(4_000))
    );
    assert_eq!(by_name("is_identity_column").ty, SqlType::Bit);
    assert!(by_name("is_identity_column").nullable);
    assert_eq!(by_name("tds_collation_sort_id").ty, SqlType::TinyInt);
}

#[test]
fn the_column_types_of_the_undeclared_parameters_are_the_declared_ones() {
    let columns = describe_columns(DescribeKind::UndeclaredParameters);
    let by_name = |name: &str| {
        columns
            .iter()
            .find(|column| column.name == name)
            .unwrap_or_else(|| panic!("{name} is a column"))
            .ty
            .clone()
    };
    assert_eq!(by_name("parameter_ordinal").ty, SqlType::Int);
    assert!(!by_name("parameter_ordinal").nullable);
    assert_eq!(by_name("name").ty, SqlType::NVarChar(Len::Fixed(128)));
    assert!(!by_name("name").nullable);
    assert_eq!(by_name("suggested_max_length").ty, SqlType::SmallInt);
    assert_eq!(by_name("suggested_is_input").ty, SqlType::Bit);
    assert!(!by_name("suggested_is_input").nullable);
    assert!(by_name("formal_parameter_name").nullable);
}

#[test]
fn another_procedure_name_is_not_a_describe_call() {
    let text = nvarchar("SELECT 1 AS n");
    let ty = nvarchar_ty();
    let args = [arg(None, &ty, &text)];
    let resolved = resolve_system_procedure("sp_executesql", &args)
        .expect("sp_executesql resolves")
        .expect("sp_executesql resolves");
    assert!(
        !matches!(resolved, ProcAction::Describe { .. }),
        "sp_executesql runs its text, it does not describe it: {resolved:?}"
    );
    assert!(resolve_system_procedure("sp_describe_parameter_encryption", &args).is_none());
}
