use super::UNKNOWN_DDL_STATE;
use crate::{SqlError, message_template};

/// Every constructor of this module, called with dummy arguments.
fn all_constructors() -> Vec<SqlError> {
    vec![
        SqlError::incorrect_syntax_near("tok", 1),
        SqlError::invalid_object_name("dbo.t"),
        SqlError::invalid_column_name("c"),
        SqlError::cannot_insert_null("c", "db.dbo.t", "INSERT"),
        SqlError::unique_violation("PRIMARY KEY", "PK_t", "dbo.t", "(1)"),
        SqlError::duplicate_key_index("dbo.t", "IX_t", "(1)"),
        SqlError::fk_violation("INSERT", "FOREIGN KEY", "FK_x", "db", "dbo.p", Some("id")),
        SqlError::fk_violation("INSERT", "CHECK", "CK_x", "db", "dbo.t", None),
        SqlError::login_failed("sa"),
        SqlError::database_not_found("nope"),
        SqlError::cannot_open_database("nope"),
        SqlError::procedure_not_found("dbo.p"),
        SqlError::procedure_expects_parameter("sp_executesql", "@statement"),
        SqlError::too_many_arguments("sp_who"),
        SqlError::not_a_parameter("@x", "sp_who"),
        SqlError::no_parameter_but_arguments(""),
        SqlError::positional_after_named(2),
        SqlError::output_on_a_constant(),
        SqlError::procedure_expects_type("@statement", "ntext/nchar/nvarchar"),
        SqlError::parameter_not_supplied("(@x int)SELECT @x", "@x"),
        SqlError::prepared_statement_not_found(123456),
        SqlError::statement_could_not_be_prepared(),
        SqlError::exec_procedure_name_not_character(),
        SqlError::object_missing_in_database("nosuchobj", "master"),
        SqlError::help_database_not_found("nosuchdb"),
        SqlError::conversion_failed("varchar", "abc", "int"),
        SqlError::error_converting_data_type("varchar", "numeric"),
        SqlError::conversion_failed_datetime(),
        SqlError::converting_datetime_from_binary(),
        SqlError::arithmetic_overflow("expression", "int"),
        SqlError::divide_by_zero(),
        SqlError::unclosed_quotation_mark("abc;", 4),
        SqlError::missing_end_comment_mark(2),
        SqlError::incorrect_syntax_near_keyword("FROM", 3),
        SqlError::out_of_range_conversion("date", "datetime"),
        SqlError::overflow_for_data_type("tinyint", 300),
        SqlError::overflow_for_type("real", 1e40),
        SqlError::invalid_collation("Klingon_CI_AS"),
        SqlError::conversion_failed_guid(),
        SqlError::unsupported_convert_style(112, "date", "datetimeoffset"),
        SqlError::string_or_binary_truncated(),
        SqlError::explicit_conversion_not_allowed("date", "int"),
        SqlError::implicit_conversion_not_allowed("date", "int"),
        SqlError::operand_type_clash("date", "int"),
        SqlError::function_arg_count("LEN", 1),
        SqlError::function_arg_count_range("ROUND", 2, 3),
        SqlError::invalid_argument_type("uniqueidentifier", 1, "len"),
        SqlError::invalid_length_parameter("left"),
        SqlError::datetime_overflow("datetime"),
        SqlError::eomonth_overflow(),
        SqlError::smalldatetime_intermediate_overflow(),
        SqlError::invalid_function_parameter(1, "datepart"),
        SqlError::datediff_datepart_not_supported("iso_week", "date", "date"),
        SqlError::datediff_overflow(),
        SqlError::datepart_not_supported("hour", "dateadd", "date"),
        SqlError::not_a_recognized_option("bogus", "datepart"),
        SqlError::invalid_floating_point_operation(),
        SqlError::coalesce_all_null(),
        SqlError::cannot_construct_type("datetime"),
        SqlError::not_a_recognized_name("NO_SUCH_FN", "built-in function"),
        SqlError::invalid_operand_type("uniqueidentifier", "add"),
        SqlError::non_boolean_expression("1"),
        SqlError::cannot_find_data_type(1, "foo"),
        SqlError::type_does_not_allow_length(1, "sysname"),
        SqlError::must_declare_scalar_variable("@x"),
        SqlError::goto_label_not_declared("skipit"),
        SqlError::insufficient_result_space_money("smallmoney"),
        SqlError::insufficient_result_space_money("int"),
        SqlError::number_out_of_numeric_range("1234567890123456789012345678901234567.89"),
        SqlError::float_out_of_range("1e400"),
        SqlError::invalid_money_value("$99999999999999999999"),
        SqlError::conversion_overflowed_int("varchar", "99999999999"),
        SqlError::conversion_overflowed_small_int("varchar", "300", "INT1"),
        SqlError::conversion_overflowed_small_int("varchar", "99999", "INT2"),
        SqlError::char_to_money_syntax(),
        SqlError::arithmetic_overflow_to_numeric("varchar"),
        SqlError::arithmetic_overflow_to_numeric("float"),
        SqlError::collate_on_non_string("int"),
        SqlError::top_negative(),
        SqlError::top_null(),
        SqlError::select_star_without_from(),
        SqlError::size_out_of_range(9000, "type", "varchar", 8000),
        SqlError::invalid_escape("ab", "LIKE"),
        SqlError::incompatible_types_for_operator("bit", "bit", "add"),
        SqlError::insufficient_result_space_money_to("varchar"),
        SqlError::insufficient_result_space_smallmoney_to("varchar"),
        SqlError::insufficient_result_space_guid(),
        SqlError::nullif_first_argument_null(),
        SqlError::arithmetic_overflow_from("numeric", "money"),
        SqlError::overflow_for_data_type_from("money", "smallint", 400_000_000),
        SqlError::overflow_for_type_from("money", "tinyint", 300.0),
        SqlError::convert_specification_size_out_of_range(5000, "nvarchar", 4000),
        SqlError::scale_greater_than_precision(),
        SqlError::invalid_length_or_precision(1, 0),
        SqlError::invalid_scale(1, 8),
        SqlError::type_size_out_of_range(39, "decimal", 38),
        SqlError::parameter_size_out_of_range(5000, "@v", 4000),
        SqlError::precision_greater_than_maximum(1, 54, 53),
        SqlError::not_a_defined_system_type("foo"),
        SqlError::conversion_failed_smalldatetime(),
        SqlError::invalid_length_parameter("substring"),
        SqlError::size_out_of_range(9000, "column", "c", 8000),
        SqlError::cannot_find_column_or_function("dbo.LEN"),
        SqlError::multi_part_identifier("t.c"),
        SqlError::must_declare_scalar_variable_assigned("@x"),
        SqlError::top_with_ties_without_order_by(),
        SqlError::column_prefix_does_not_match("t"),
        SqlError::too_many_column_prefixes("a.b.c.d"),
        SqlError::invalid_style_number(999, "date"),
        SqlError::input_does_not_follow_style(100),
        SqlError::percent_out_of_range(),
        SqlError::top_invalid_value(),
        SqlError::invalid_escape_unicode("ab", "LIKE"),
        SqlError::error_converting_data_type("varchar", "datetimeoffset"),
        SqlError::nested_too_deeply(1),
        SqlError::stack_limit_reached(),
        SqlError::parameters_supplied_to_non_function("dbo.T"),
        SqlError::database_already_exists("d"),
        SqlError::object_already_exists("t"),
        SqlError::cannot_drop("drop", "table", "nope"),
        SqlError::cannot_drop("drop", "database", "nope"),
        SqlError::cannot_drop("drop", "index", "t3.ix_nope"),
        SqlError::statement_not_allowed_in_transaction("CREATE DATABASE"),
        SqlError::ambiguous_column_name("a"),
        SqlError::cannot_drop("drop", "sequence", "nope_sequence"),
        SqlError::cannot_drop("drop", "synonym", "nope_synonym"),
        SqlError::cannot_drop_system_database("master"),
        SqlError::drop_database_in_transaction(),
        SqlError::duplicate_column_name("a", "t_dup"),
        SqlError::cannot_find_data_type_in_table(1, "foo"),
        SqlError::only_one_expression_in_subquery(),
        SqlError::assignment_mixed_with_data_retrieval(),
        SqlError::order_by_item_not_in_distinct_select_list(),
        SqlError::set_operator_column_count_mismatch(),
        SqlError::transaction_count_after_execute(0, 1),
        SqlError::subquery_returned_more_than_one_value(),
        SqlError::conflicting_locking_hints(),
        SqlError::nolock_not_allowed_on_target(),
        SqlError::foreign_key_references_invalid_column("fk_bad", "nosuch", "lone"),
        SqlError::no_matching_key_in_referenced_table("dbo.nopk", "fk_nopk"),
        SqlError::cannot_drop_referenced_object("dbo.parent"),
        SqlError::snapshot_isolation_not_allowed("snapdb"),
        SqlError::cannot_truncate_referenced_table("dbo.parent"),
        SqlError::cannot_add_column_to_non_empty_table("b", "notempty"),
        SqlError::identity_insert_requires_column_list("dbo.ident"),
        SqlError::cannot_update_identity_column("id"),
        SqlError::identity_insert_already_on("master", "dbo", "t1", "dbo.t2"),
        SqlError::identity_insert_table_has_no_identity("dbo.plain"),
        SqlError::cannot_find_object_for_identity_insert("dbo.nosuch"),
        SqlError::cannot_update_computed_column("c"),
        SqlError::column_invalid_in_having("dbo.t2", "b"),
        SqlError::column_count_does_not_match_table(),
        SqlError::identity_insert_is_off("ident"),
        SqlError::deadlock_victim(60, "lock"),
        SqlError::lock_request_timeout_on_row(),
        SqlError::lock_request_timeout_on_object(),
        SqlError::commit_without_begin(),
        SqlError::rollback_without_begin(),
        SqlError::snapshot_update_conflict("dbo.s", "cc"),
        SqlError::column_invalid_in_select_list("dbo.t2", "b"),
        SqlError::multiple_primary_keys("dbo.t"),
        SqlError::primary_key_on_nullable_column("t"),
        SqlError::multiple_clustered_index_constraints("dbo.t"),
        SqlError::more_than_one_clustered_index("dbo.t", "ix_a"),
        SqlError::column_does_not_exist_in_target("nosuchcolumn"),
        SqlError::index_name_already_exists("ix_t", "dbo.t"),
        SqlError::duplicate_column_in_index("a"),
        SqlError::duplicate_column_in_included_columns("a"),
        SqlError::could_not_create_constraint_or_index(),
        SqlError::default_on_identity_column("t", "a"),
        SqlError::duplicate_name_in_this_context("c1"),
        SqlError::duplicate_index_name_in_this_context("ix_t"),
        SqlError::cannot_drop_constraint_index("dbo.t.pk_t", "PRIMARY KEY"),
        SqlError::cannot_drop_constraint_index("dbo.t.uq_t", "UNIQUE KEY"),
        SqlError::cannot_create_index_on_view("v"),
        SqlError::cannot_find_object_to_index("dbo.nosuch"),
        SqlError::invalid_identity_column_type("a"),
        SqlError::identity_on_nullable_column("a", "dbo.t"),
        SqlError::multiple_column_defaults("a", "dbo.t"),
        SqlError::invalid_object_name_of_another_type("sys.sp_executesql"),
        SqlError::collate_on_non_string_column("int"),
        SqlError::must_declare_table_variable("@tv"),
        SqlError::linked_server_not_found("nosuchserver"),
        SqlError::duplicate_correlation_name("x"),
        SqlError::correlation_name_is_a_table_name("u", "dbo.u"),
        SqlError::same_exposed_names("dbo.t", "dbo.t"),
        SqlError::table_is_ambiguous("t"),
        SqlError::no_column_name_in_derived_table(2, "d"),
        SqlError::object_or_column_name_missing(),
        SqlError::identifier_too_long("abc", 128),
        SqlError::number_too_long("123", 128),
        SqlError::money_literal_too_long("$123", 128),
        SqlError::database_does_not_exist("nosuchdb"),
        SqlError::cannot_find_object_to_truncate("nosuch"),
        SqlError::cannot_open_login_database("nosuchdb", "master"),
    ]
}

#[test]
fn datepart_not_supported_states_follow_function_type_and_part() {
    // One representative per function/type/state, DATETRUNC included although the
    // function is not registered by VaubanDB yet.
    for (part, function, ty, state) in [
        ("hour", "dateadd", "date", 1),
        ("iso_week", "dateadd", "date", 2),
        ("iso_week", "dateadd", "datetime2", 2),
        ("iso_week", "dateadd", "datetime", 0),
        ("iso_week", "dateadd", "datetimeoffset", 2),
        ("iso_week", "dateadd", "smalldatetime", 3),
        ("day", "dateadd", "time", 1),
        ("hour", "datename", "date", 4),
        ("tzoffset", "datename", "datetime", 7),
        ("tzoffset", "datename", "smalldatetime", 7),
        ("day", "datename", "time", 5),
        ("hour", "datepart", "date", 2),
        ("tzoffset", "datepart", "datetime", 6),
        ("tzoffset", "datepart", "smalldatetime", 6),
        ("day", "datepart", "time", 3),
        ("hour", "datetrunc", "date", 10),
        ("weekday", "datetrunc", "date", 11),
        ("nanosecond", "datetrunc", "datetime2", 11),
        ("microsecond", "datetrunc", "datetime", 9),
        ("nanosecond", "datetrunc", "datetimeoffset", 11),
        ("microsecond", "datetrunc", "smalldatetime", 8),
        ("day", "datetrunc", "time", 10),
        ("nanosecond", "datetrunc", "time", 11),
    ] {
        let error = SqlError::datepart_not_supported(part, function, ty);
        assert_eq!(
            (error.number, error.severity, error.state),
            (9810, 16, state),
            "{function}/{ty}/{part}"
        );
        assert_eq!(
            error.message,
            format!(
                "Datepart {part} cannot be used with the date function {function} on data type {ty}."
            )
        );
    }
    assert_eq!(
        SqlError::datediff_datepart_not_supported("iso_week", "datetime", "smalldatetime").state,
        2
    );
    assert_eq!(
        SqlError::datediff_datepart_not_supported("iso_week", "datetime", "date").state,
        0
    );
    assert_eq!(SqlError::smalldatetime_intermediate_overflow().state, 1);
}

#[test]
fn date_function_constructors_carry_their_states() {
    assert_eq!(
        message_template(1023).unwrap().template,
        "Parameter %d of %ls is not valid."
    );
    assert_eq!(
        message_template(9806).unwrap().template,
        "Datepart %.*ls cannot be used with the date function %.*ls."
    );
    for (ty, state) in [
        ("datetime", 1),
        ("smalldatetime", 2),
        ("date", 3),
        ("datetime2", 3),
        ("datetimeoffset", 3),
    ] {
        let error = SqlError::datetime_overflow(ty);
        assert_eq!(
            (error.number, error.severity, error.state),
            (517, 16, state)
        );
        assert_eq!(
            error.message,
            format!("The addition overflowed the '{ty}' column.")
        );
    }
    assert_eq!(SqlError::eomonth_overflow().state, 1);
    assert_eq!(SqlError::cannot_construct_type("date").state, 1);
    assert_eq!(SqlError::cannot_construct_type("datetime").state, 3);
    let error = SqlError::invalid_function_parameter(1, "datepart");
    assert_eq!((error.number, error.severity, error.state), (1023, 15, 1));
    assert_eq!(error.message, "Parameter 1 of datepart is not valid.");
    let error = SqlError::datediff_datepart_not_supported("iso_week", "date", "date");
    assert_eq!((error.number, error.severity, error.state), (9806, 16, 0));
    assert_eq!(
        error.message,
        "Datepart iso_week cannot be used with the date function datediff."
    );
}

#[test]
fn parameters_supplied_to_non_function_is_215() {
    let error = SqlError::parameters_supplied_to_non_function("dbo.T");
    assert_eq!(error.number, 215);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(
        error.message,
        "Object 'dbo.T' is not a function and takes no parameters; a table hint needs the WITH keyword."
    );
}

/// The constructors the server raises with a severity other than the catalogue's:
/// number, severity sent on the wire, severity of the catalogue, with a query that
/// raises it in the comment.
const SEVERITY_OVERRIDES: &[(u32, u8, u8)] = &[
    // SELECT CAST(1 AS nvarchar(5000)); — the `type` filling of 131 keeps 15.
    (131, 16, 15),
    // DECLARE @v decimal(5,6);
    (192, 15, 16),
    // DECLARE @v varchar(0);
    (1001, 15, 16),
    // DECLARE @v time(8);
    (1002, 15, 16),
    // DECLARE @v decimal(39,2); and DECLARE @v nvarchar(5000);
    (2717, 16, 15),
    // SELECT TOP (1) WITH TIES 1 AS c;
    (1062, 15, 16),
    // SELECT 1 WHERE 1 = (SELECT a, b FROM dbo.t2);
    (116, 16, 15),
    // SELECT 1 FROM dbo.t AS x, dbo.t AS x;
    (1011, 16, 15),
    // SELECT 1 FROM dbo.t AS u JOIN dbo.u ON 1 = 1;
    (1012, 16, 15),
    // SELECT 1 FROM dbo.t, dbo.t;
    (1013, 16, 15),
    // CREATE INDEX ix_t ON dbo.nosuch (a);
    (1088, 16, 15),
    // CREATE TABLE dbo.t (a int NULL, CONSTRAINT pk_t PRIMARY KEY (a));
    // catalogued 10, sent 16.
    (1750, 16, 10),
    // UPDATE t SET a = 1 FROM dbo.t AS x, dbo.t AS y;
    (8154, 16, 15),
    // SELECT * FROM (SELECT a, a + 1 FROM dbo.t) AS d;
    (8155, 16, 15),
];

#[test]
fn every_constructor_number_is_in_the_catalog() {
    for err in all_constructors() {
        let def = message_template(err.number)
            .unwrap_or_else(|| panic!("error {} is not in the catalog", err.number));
        let overridden = SEVERITY_OVERRIDES.contains(&(err.number, err.severity, def.severity));
        assert!(
            err.severity == def.severity || overridden,
            "severity {} of error {} is neither the catalog's {} nor a known override",
            err.severity,
            err.number,
            def.severity
        );
        assert!(
            !err.message.contains('%'),
            "error {} still contains a specifier: {}",
            err.number,
            err.message
        );
        assert_eq!(err.procedure, None);
    }
}

/// The rendered message of each literal and conversion constructor, with a query
/// that raises it quoted next to it; state and severity go with the query.
#[test]
fn literal_and_conversion_messages_are_rendered() {
    let expected: &[(SqlError, u32, u8, u8, &str)] = &[
        (
            // SELECT CAST(CAST(300000 AS money) AS smallmoney);
            SqlError::insufficient_result_space_money("smallmoney"),
            237,
            16,
            3,
            "A money value does not fit in the result type smallmoney.",
        ),
        (
            // SELECT CAST(CAST(99999999999 AS money) AS int);
            SqlError::insufficient_result_space_money("int"),
            237,
            16,
            1,
            "A money value does not fit in the result type int.",
        ),
        (
            // SELECT CAST(SQL_VARIANT_PROPERTY(123456789012345678901234567890123456789, 'BaseType') AS varchar(30));
            SqlError::number_out_of_numeric_range("123456789012345678901234567890123456789"),
            1007,
            15,
            1,
            "The number '123456789012345678901234567890123456789' exceeds the numeric range (precision is limited to 38).",
        ),
        (
            // SELECT CAST(SQL_VARIANT_PROPERTY(1e400, 'BaseType') AS varchar(30));
            SqlError::float_out_of_range("1e400"),
            168,
            15,
            1,
            "The floating point literal '1e400' cannot be represented in 8 bytes.",
        ),
        (
            // SELECT CAST(SQL_VARIANT_PROPERTY($99999999999999999999, 'BaseType') AS varchar(30));
            SqlError::invalid_money_value("$99999999999999999999"),
            151,
            15,
            1,
            "'$99999999999999999999' cannot be read as a money value.",
        ),
        (
            // SELECT CAST('99999999999' AS int);
            SqlError::conversion_overflowed_int("varchar", "99999999999"),
            248,
            16,
            1,
            "The varchar value '99999999999' does not fit in an int column.",
        ),
        (
            // SELECT CAST('300' AS tinyint);
            SqlError::conversion_overflowed_small_int("varchar", "300", "INT1"),
            244,
            16,
            1,
            "The varchar value '300' does not fit in an INT1 column; a wider integer type is needed.",
        ),
        (
            // SELECT CAST('99999' AS smallint);
            SqlError::conversion_overflowed_small_int("varchar", "99999", "INT2"),
            244,
            16,
            2,
            "The varchar value '99999' does not fit in an INT2 column; a wider integer type is needed.",
        ),
        (
            // SELECT CAST('abc' AS money);
            SqlError::char_to_money_syntax(),
            235,
            16,
            0,
            "The character value is not a valid money literal and could not be converted.",
        ),
        (
            // SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;
            SqlError::collate_on_non_string("int"),
            447,
            16,
            0,
            "COLLATE cannot apply to an expression of type int.",
        ),
        (
            // SELECT TOP (-1) 1;
            SqlError::top_negative(),
            127,
            15,
            1,
            "The row count of TOP or FETCH cannot be negative.",
        ),
        (
            // SELECT TOP (NULL) 1;
            SqlError::top_null(),
            1060,
            15,
            1,
            "The row count of TOP or FETCH has to be an integer.",
        ),
        (
            // SELECT *;
            SqlError::select_star_without_from(),
            263,
            16,
            1,
            "No table to select from.",
        ),
        (
            // DECLARE @v varchar(9000);
            SqlError::size_out_of_range(9000, "type", "varchar", 8000),
            131,
            15,
            3,
            "Size 9000 of the type 'varchar' is larger than any data type allows (8000).",
        ),
        (
            // DECLARE @v varbinary(9000);
            SqlError::size_out_of_range(9000, "type", "varbinary", 8000),
            131,
            15,
            3,
            "Size 9000 of the type 'varbinary' is larger than any data type allows (8000).",
        ),
        (
            // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE 'ab';
            SqlError::invalid_escape("ab", "LIKE"),
            506,
            16,
            1,
            "The escape character \"ab\" of the LIKE predicate must be a single character.",
        ),
        (
            // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE '';
            SqlError::invalid_escape("", "LIKE"),
            506,
            16,
            1,
            "The escape character \"\" of the LIKE predicate must be a single character.",
        ),
    ];
    for (err, number, severity, state, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of {number}");
        assert_eq!(err.state, *state, "state of {number}");
        assert_eq!(&err.message, message, "message of {number}");
        assert_eq!(err.line, 0);
    }
}

/// The message, severity and state of each procedure-call constructor, with the form
/// that raises it quoted next to it.
#[test]
fn procedure_call_messages_are_rendered() {
    let expected: &[(SqlError, u32, u8, u8, &str)] = &[
        (
            // EXEC sp_executesql;
            SqlError::procedure_expects_parameter("sp_executesql", "@statement"),
            201,
            16,
            10,
            "Procedure 'sp_executesql' was called without the parameter '@statement' it requires.",
        ),
        (
            // EXEC sp_who 'sa', 'x';
            SqlError::too_many_arguments("sp_who"),
            8144,
            16,
            2,
            "Procedure sp_who was called with more arguments than it declares.",
        ),
        (
            // EXEC sp_who @x = 1;
            SqlError::not_a_parameter("@x", "sp_who"),
            8145,
            16,
            1,
            "@x is not a parameter declared by procedure sp_who.",
        ),
        (
            // EXEC sp_executesql N'SELECT 1', N'', N'', N'';
            SqlError::no_parameter_but_arguments(""),
            8146,
            16,
            1,
            "Procedure  declares no parameter and was called with arguments.",
        ),
        (
            // EXEC sp_executesql @stmt = N'SELECT 1', 1;
            SqlError::positional_after_named(2),
            119,
            15,
            1,
            "Parameter number 2 and the ones after it have to use the '@name = value' form; once that form has been used, a positional argument may not follow it.",
        ),
        (
            // EXEC sp_executesql N'SELECT @x OUTPUT', N'@x int', 1 OUTPUT;
            SqlError::output_on_a_constant(),
            179,
            15,
            1,
            "The OUTPUT option cannot be used on a constant argument of a procedure.",
        ),
        (
            // EXEC sp_executesql 123;
            SqlError::procedure_expects_type("@statement", "ntext/nchar/nvarchar"),
            214,
            16,
            2,
            "The parameter '@statement' has to be of type 'ntext/nchar/nvarchar' for this procedure.",
        ),
        (
            // EXEC sp_executesql N'SELECT @x', N'@x int';
            SqlError::parameter_not_supplied("(@x int)SELECT @x", "@x"),
            8178,
            16,
            1,
            "The parameterized query '(@x int)SELECT @x' was called without its parameter '@x'.",
        ),
        (
            // EXEC sp_execute 123456;
            SqlError::prepared_statement_not_found(123456),
            8179,
            16,
            4,
            "No prepared statement of this session has the handle 123456.",
        ),
        (
            // DECLARE @p int; EXEC sp_prepare @p OUTPUT, N'@x int, @x int', N'SELECT @x';
            SqlError::statement_could_not_be_prepared(),
            8180,
            16,
            1,
            "The statement could not be prepared.",
        ),
        (
            // DECLARE @t int; EXEC @t;
            SqlError::exec_procedure_name_not_character(),
            8199,
            16,
            1,
            "The procedure name in EXECUTE must be a string literal or a char, varchar, nchar, or nvarchar variable.",
        ),
        (
            // EXEC sp_help 'nosuchobj';
            SqlError::object_missing_in_database("nosuchobj", "master"),
            15009,
            16,
            1,
            "The object 'nosuchobj' is not in database 'master', or this operation does not accept it.",
        ),
        (
            // EXEC sp_helpdb N'nosuchdb';
            SqlError::help_database_not_found("nosuchdb"),
            15010,
            16,
            1,
            "No database named 'nosuchdb' exists; give a valid database name.",
        ),
    ];
    for (err, number, severity, state, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of {number}");
        assert_eq!(err.state, *state, "state of {number}");
        assert_eq!(&err.message, message, "message of {number}");
        assert_eq!(err.line, 0);
    }
}

/// The rendered message of each operator and result-space constructor, with a query
/// that raises it quoted next to it; state and severity go with the query.
#[test]
fn operator_and_result_space_messages_are_rendered() {
    let expected: &[(SqlError, u32, u8, u8, &str)] = &[
        (
            // SELECT CAST(1 AS bit) + CAST(1 AS bit);
            SqlError::incompatible_types_for_operator("bit", "bit", "add"),
            402,
            16,
            1,
            "The types bit and bit cannot be combined by the add operator.",
        ),
        (
            // DECLARE @d date = '2020-01-01'; DECLARE @t datetime = '2020-01-01'; SELECT @d + @t;
            SqlError::incompatible_types_for_operator("date", "datetime", "add"),
            402,
            16,
            1,
            "The types date and datetime cannot be combined by the add operator.",
        ),
        (
            // DECLARE @v varchar(1)='a'; DECLARE @b varbinary(1)=0x41; SELECT @v + @b;
            SqlError::incompatible_types_for_operator("varchar", "varbinary", "add"),
            402,
            16,
            1,
            "The types varchar and varbinary cannot be combined by the add operator.",
        ),
        (
            // DECLARE @v varchar(1)='a'; DECLARE @w varchar(1)='b'; SELECT @v - @w;
            SqlError::incompatible_types_for_operator("varchar", "varchar", "subtract"),
            402,
            16,
            1,
            "The types varchar and varchar cannot be combined by the subtract operator.",
        ),
        (
            // DECLARE @f float = 1; SELECT @f & 1;
            SqlError::incompatible_types_for_operator("float", "int", "'&'"),
            402,
            16,
            1,
            "The types float and int cannot be combined by the '&' operator.",
        ),
        (
            // SELECT NULLIF(NULL, 1);
            SqlError::nullif_first_argument_null(),
            4151,
            16,
            1,
            "The first argument of NULLIF cannot be the NULL constant: its type has to be known.",
        ),
        (
            // DECLARE @m money = 922337203685477.58; SELECT CAST(@m AS varchar(3));
            SqlError::insufficient_result_space_money_to("varchar"),
            234,
            16,
            2,
            "A money value does not fit in the result type varchar.",
        ),
        (
            // DECLARE @m money = 922337203685477.58; SELECT CAST(@m AS nvarchar(3));
            SqlError::insufficient_result_space_money_to("nvarchar"),
            234,
            16,
            2,
            "A money value does not fit in the result type nvarchar.",
        ),
        (
            // DECLARE @s smallmoney = 214748.3647; SELECT CAST(@s AS varchar(3));
            SqlError::insufficient_result_space_smallmoney_to("varchar"),
            292,
            16,
            2,
            "A smallmoney value does not fit in the result type varchar.",
        ),
        (
            // DECLARE @g uniqueidentifier = NEWID(); SELECT CAST(@g AS varchar(35));
            SqlError::insufficient_result_space_guid(),
            8170,
            16,
            2,
            "A uniqueidentifier value does not fit in the char result.",
        ),
    ];
    for (err, number, severity, state, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of {number}");
        assert_eq!(err.state, *state, "state of {number}");
        assert_eq!(&err.message, message, "message of {number}");
        assert_eq!(err.line, 0);
    }
}

/// The rendered message of each type declaration constructor, with a query that
/// raises it quoted next to it; severity and state go with the query, the catalogue
/// notwithstanding.
#[test]
fn type_declaration_messages_are_rendered() {
    let expected: &[(SqlError, u32, u8, u8, &str)] = &[
        (
            // SELECT CAST(1 AS nvarchar(5000));
            SqlError::convert_specification_size_out_of_range(5000, "nvarchar", 4000),
            131,
            16,
            1,
            "Size 5000 of the convert specification 'nvarchar' is larger than any data type allows (4000).",
        ),
        (
            // SELECT CAST(1 AS nchar(5000));
            SqlError::convert_specification_size_out_of_range(5000, "nchar", 4000),
            131,
            16,
            1,
            "Size 5000 of the convert specification 'nchar' is larger than any data type allows (4000).",
        ),
        (
            // DECLARE @v decimal(5,6);
            SqlError::scale_greater_than_precision(),
            192,
            15,
            1,
            "Scale cannot exceed precision.",
        ),
        (
            // DECLARE @v varchar(0);
            SqlError::invalid_length_or_precision(1, 0),
            1001,
            15,
            1,
            "Line 1: the length or precision 0 is not valid.",
        ),
        (
            // DECLARE @v time(8);
            SqlError::invalid_scale(1, 8),
            1002,
            15,
            1,
            "Line 1: the scale 8 is not valid.",
        ),
        (
            // DECLARE @v decimal(39,2);
            SqlError::type_size_out_of_range(39, "decimal", 38),
            2717,
            16,
            1,
            "Size 39 of the type 'decimal' is larger than the maximum (38).",
        ),
        (
            // DECLARE @v nvarchar(5000);
            SqlError::parameter_size_out_of_range(5000, "@v", 4000),
            2717,
            16,
            2,
            "Size 5000 of the parameter '@v' is larger than the maximum (4000).",
        ),
        (
            // CREATE TABLE #t(c nvarchar(5000));
            SqlError::parameter_size_out_of_range(5000, "c", 4000),
            2717,
            16,
            2,
            "Size 5000 of the parameter 'c' is larger than the maximum (4000).",
        ),
        (
            // DECLARE @v float(54);
            SqlError::precision_greater_than_maximum(1, 54, 53),
            2750,
            16,
            1,
            "Column or parameter #1: precision 54 exceeds the maximum of 53.",
        ),
        (
            // CREATE TABLE #t(c decimal(39,2));
            SqlError::precision_greater_than_maximum(1, 39, 38),
            2750,
            16,
            1,
            "Column or parameter #1: precision 39 exceeds the maximum of 38.",
        ),
        (
            // SELECT CAST(1 AS foo);
            SqlError::not_a_defined_system_type("foo"),
            243,
            16,
            1,
            "foo is not a known system type.",
        ),
        (
            // SELECT CAST('not a date' AS smalldatetime);
            SqlError::conversion_failed_smalldatetime(),
            295,
            16,
            3,
            "The character string could not be converted to smalldatetime.",
        ),
    ];
    for (err, number, severity, state, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of {number}");
        assert_eq!(err.state, *state, "state of {number}");
        assert_eq!(&err.message, message, "message of {number}");
    }
}

/// The rendered message of the length and qualified-name constructors, with a query
/// that raises it quoted next to it; severity and state go with the query.
#[test]
fn qualified_function_name_messages_are_rendered() {
    let expected: &[(SqlError, u32, u8, u8, &str)] = &[
        (
            // SELECT LEFT('abc', -1);
            SqlError::invalid_length_parameter("left"),
            536,
            16,
            6,
            "The length given to the left function is not valid.",
        ),
        (
            // SELECT RIGHT('abc', -1);
            SqlError::invalid_length_parameter("right"),
            536,
            16,
            6,
            "The length given to the right function is not valid.",
        ),
        (
            // SELECT SUBSTRING('abc', 1, -1);
            SqlError::invalid_length_parameter("substring"),
            536,
            16,
            8,
            "The length given to the substring function is not valid.",
        ),
        (
            // CREATE TABLE #t(c varchar(9000));
            SqlError::size_out_of_range(9000, "column", "c", 8000),
            131,
            15,
            2,
            "Size 9000 of the column 'c' is larger than any data type allows (8000).",
        ),
        (
            // SELECT dbo.LEN(1);
            SqlError::cannot_find_column_or_function("dbo.LEN"),
            4121,
            16,
            1,
            "Neither a column \"dbo\" nor a user-defined function or aggregate \"dbo.LEN\" was found, or the name is ambiguous.",
        ),
        (
            // SELECT foo.bar(1);
            SqlError::cannot_find_column_or_function("foo.bar"),
            4121,
            16,
            1,
            "Neither a column \"foo\" nor a user-defined function or aggregate \"foo.bar\" was found, or the name is ambiguous.",
        ),
        (
            // SELECT sys.LEN(1);
            SqlError::cannot_find_column_or_function("sys.LEN"),
            4121,
            16,
            1,
            "Neither a column \"sys\" nor a user-defined function or aggregate \"sys.LEN\" was found, or the name is ambiguous.",
        ),
        (
            // SELECT dbo.foo.bar(1);
            SqlError::cannot_find_column_or_function("dbo.foo.bar"),
            4121,
            16,
            1,
            "Neither a column \"dbo\" nor a user-defined function or aggregate \"dbo.foo.bar\" was found, or the name is ambiguous.",
        ),
    ];
    for (err, number, severity, state, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of {number}");
        assert_eq!(err.state, *state, "state of {number}");
        assert_eq!(&err.message, message, "message of {number}");
        assert_eq!(err.line, 0);
    }
}

/// 4121 fills its two placeholders from one name: the leading part, then the whole
/// name. A name without a dot repeats itself, which is what a caller that qualified
/// nothing would print; `binder` sends 195 in that case (`SELECT bar(1);` raises 195
/// state 10).
#[test]
fn cannot_find_column_or_function_splits_on_the_first_dot() {
    let one_part = SqlError::cannot_find_column_or_function("bar");
    assert_eq!(
        one_part.message,
        "Neither a column \"bar\" nor a user-defined function or aggregate \"bar\" was found, or the name is ambiguous."
    );

    let three_parts = SqlError::cannot_find_column_or_function("db1.dbo.bar");
    assert!(three_parts.message.contains("column \"db1\""));
    assert!(three_parts.message.contains("aggregate \"db1.dbo.bar\""));
}

/// Error 4104 for each form of a qualified column name: one `%.*ls` and the whole
/// name in it, state 1 throughout, with the query in the comment.
#[test]
fn multi_part_identifier_is_4104() {
    let expected: &[(SqlError, &str)] = &[
        (
            // SELECT t.c;
            SqlError::multi_part_identifier("t.c"),
            "The qualified name \"t.c\" matches nothing in scope.",
        ),
        (
            // SELECT a.b.c.d;
            SqlError::multi_part_identifier("a.b.c.d"),
            "The qualified name \"a.b.c.d\" matches nothing in scope.",
        ),
        (
            // SELECT [dbo].[t].[c]; — the server prints the name brackets stripped.
            SqlError::multi_part_identifier("dbo.t.c"),
            "The qualified name \"dbo.t.c\" matches nothing in scope.",
        ),
        (
            // SELECT x.y FROM (SELECT 1 AS c) AS z;
            SqlError::multi_part_identifier("x.y"),
            "The qualified name \"x.y\" matches nothing in scope.",
        ),
    ];
    for (err, message) in expected {
        assert_eq!(err.number, 4104);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1, "state of {message}");
        assert_eq!(&err.message, message);
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }
}

/// The state of 237 follows the target on the four targets that raise it, and the
/// message is the same text throughout (`SELECT CAST(CAST(300000 AS money) AS
/// <target>);`, except `int`, which needs a larger value).
#[test]
fn insufficient_result_space_money_states() {
    let expected: &[(&str, u8)] = &[
        // SELECT CAST(CAST(99999999999 AS money) AS int);
        ("int", 1),
        // SELECT CAST(CAST(300000 AS money) AS smallint);
        ("smallint", 2),
        // SELECT CAST(CAST(300000 AS money) AS tinyint);
        ("tinyint", 3),
        // SELECT CAST(CAST(300000 AS money) AS smallmoney);
        ("smallmoney", 3),
    ];
    for (to, state) in expected {
        let err = SqlError::insufficient_result_space_money(to);
        assert_eq!(err.number, 237);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, *state, "state towards {to}");
        assert_eq!(
            err.message,
            format!("A money value does not fit in the result type {to}.")
        );
    }

    // A target without a row keeps the default state.
    assert_eq!(
        SqlError::insufficient_result_space_money("bigint").state,
        super::DEFAULT_RESULT_SPACE_STATE
    );
}

/// 131 and 2717 each carry one template and several constructors: the `%S_MSG`, the
/// severity and the state are what tells the contexts apart.
/// `SELECT CAST(1 AS nvarchar(5000));`, `DECLARE @v varchar(9000);` and
/// `CREATE TABLE #t(c varchar(9000));` for the three fillings of 131,
/// `DECLARE @v decimal(39,2);` against `DECLARE @v nvarchar(5000);` for 2717.
#[test]
fn the_two_size_contexts_of_131_and_2717_differ_by_severity_and_state() {
    let convert = SqlError::convert_specification_size_out_of_range(5000, "nvarchar", 4000);
    let declaration = SqlError::size_out_of_range(9000, "type", "varchar", 8000);
    let column = SqlError::size_out_of_range(9000, "column", "c", 8000);
    assert_eq!(
        (convert.number, convert.severity, convert.state),
        (131, 16, 1)
    );
    assert_eq!(
        (declaration.number, declaration.severity, declaration.state),
        (131, 15, 3)
    );
    assert_eq!((column.number, column.severity, column.state), (131, 15, 2));
    assert!(declaration.message.contains("the type 'varchar'"));
    assert!(column.message.contains("the column 'c'"));

    let ty = SqlError::type_size_out_of_range(39, "decimal", 38);
    let parameter = SqlError::parameter_size_out_of_range(5000, "@v", 4000);
    assert_eq!((ty.number, ty.severity, ty.state), (2717, 16, 1));
    assert_eq!(
        (parameter.number, parameter.severity, parameter.state),
        (2717, 16, 2)
    );
    assert!(ty.message.contains("the type 'decimal'"));
    assert!(parameter.message.contains("the parameter '@v'"));
}

/// 1001 and 1002 print the batch line inside the message *and* carry it as the
/// error's line: `DECLARE @a int;` then `DECLARE @v varchar(0);` prints `Line 2:` and
/// the server sends line 2.
#[test]
fn the_line_of_1001_and_1002_is_in_the_message_and_on_the_error() {
    let length = SqlError::invalid_length_or_precision(2, 0);
    assert_eq!(length.line, 2);
    assert_eq!(
        length.message,
        "Line 2: the length or precision 0 is not valid."
    );

    let scale = SqlError::invalid_scale(2, 8);
    assert_eq!(scale.line, 2);
    assert_eq!(scale.message, "Line 2: the scale 8 is not valid.");
}

/// 234 and 237 share their text and differ by number and state: the character target
/// sends 234 state 2, the numeric one 237 state 1, 2 or 3 depending on the target
/// (table `RESULT_SPACE_237_STATES`).
#[test]
fn error_234_and_237_differ_by_number_and_state() {
    let to_character = SqlError::insufficient_result_space_money_to("varchar");
    let to_numeric = SqlError::insufficient_result_space_money("int");
    assert_eq!(to_character.number, 234);
    assert_eq!(to_numeric.number, 237);
    assert_eq!(to_character.state, 2);
    assert_eq!(to_numeric.state, 1);
    assert_eq!(
        SqlError::insufficient_result_space_money("smallmoney").state,
        3
    );
    assert_eq!(
        to_character.message,
        "A money value does not fit in the result type varchar."
    );
}

/// The states of 220, 232 and 8115 index the conversion routine, not the target: the
/// same target reached from two sources carries two states, with the query in the
/// comment; a pair without a row keeps state 2, which the stateless constructors send.
#[test]
fn overflow_states_follow_the_source_and_target() {
    // SELECT CAST(CAST(99999999999999999 AS numeric(20,0)) AS money); / AS smallmoney);
    for to in ["money", "smallmoney"] {
        let err = SqlError::arithmetic_overflow_from("numeric", to);
        assert_eq!(err.number, 8115);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 4, "state of numeric -> {to}");
        assert_eq!(
            err.message,
            format!("Converting numeric to data type {to} overflowed.")
        );
    }
    // SELECT CAST('1234.5' AS decimal(5,2)); / SELECT CAST(CAST(1234.5 AS float) AS decimal(5,2));
    assert_eq!(
        SqlError::arithmetic_overflow_from("varchar", "numeric").state,
        8
    );
    assert_eq!(
        SqlError::arithmetic_overflow_from("float", "numeric").state,
        6
    );
    // SELECT CAST(CAST(3000000000 AS bigint) AS int);
    assert_eq!(
        SqlError::arithmetic_overflow_from("expression", "int").state,
        2
    );
    // DECLARE @n numeric(20,0) = 99999999999999999; SELECT CAST(@n AS varchar(3)); / AS char(3)); / AS nvarchar(3));
    for to in ["varchar", "char"] {
        let err = SqlError::arithmetic_overflow_from("numeric", to);
        assert_eq!(err.state, 5, "state of numeric -> {to}");
    }
    assert_eq!(
        SqlError::arithmetic_overflow_from("expression", "nvarchar").state,
        2
    );

    // SELECT CAST(CAST(300 AS float) AS tinyint); / AS int); / SELECT CAST(CAST(300 AS money) AS tinyint);
    for (from, to, state) in [
        ("float", "tinyint", 1),
        ("real", "tinyint", 1),
        ("float", "int", 3),
        ("real", "int", 3),
        ("float", "smallint", 2),
        ("float", "smallmoney", 2),
        ("float", "money", 2),
        ("float", "real", 2),
        ("money", "tinyint", 11),
        ("numeric", "tinyint", 2),
    ] {
        let err = SqlError::overflow_for_type_from(from, to, 300.0);
        assert_eq!(err.number, 232);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, state, "state of 232, {from} -> {to}");
        assert_eq!(
            err.message,
            format!("Value out of range for type {to}: 300.000000.")
        );
    }

    // SELECT CAST(CAST(40000 AS money) AS smallint); / AS smallmoney); / CAST(40000 AS int) AS smallint);
    // `DECLARE @t TABLE(c smallint); INSERT @t VALUES(CAST(40000 AS money));` sends the
    // same 220 state 7: the state follows the pair, not the statement.
    for (from, to, state) in [
        ("money", "smallint", 7),
        ("smallmoney", "smallint", 5),
        ("int", "smallint", 1),
        ("int", "tinyint", 2),
        ("smallint", "tinyint", 2),
    ] {
        let err = SqlError::overflow_for_data_type_from(from, to, 400_000_000);
        assert_eq!(err.number, 220);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, state, "state of 220, {from} -> {to}");
        assert_eq!(
            err.message,
            format!("Value out of range for data type {to}: 400000000.")
        );
    }
    // `bigint`, `numeric` and `smallmoney` towards `tinyint` raise 8115 state 2
    // instead of 220 (`DECLARE @b bigint = 300; SELECT CAST(@b AS tinyint);`), so the
    // fallback of `overflow_state` is defensive.
    assert_eq!(
        SqlError::arithmetic_overflow_from("expression", "tinyint").state,
        super::DEFAULT_OVERFLOW_STATE
    );
}

/// The three stateless overflow constructors keep the signature and the state their
/// callers of `vauban-types` were written against: the twins add states, they do not
/// change them.
#[test]
fn the_stateless_overflow_constructors_are_unchanged() {
    assert_eq!(SqlError::arithmetic_overflow("numeric", "money").state, 2);
    assert_eq!(SqlError::overflow_for_type("tinyint", 300.0).state, 2);
    assert_eq!(SqlError::overflow_for_data_type("smallint", 40000).state, 2);
}

/// Error 8115 towards `numeric` keeps its number and severity but changes state, and
/// the two-argument constructor is untouched: `SELECT CAST('1234.5' AS decimal(5,2));`
/// (state 8), `SELECT CAST(CAST(1234.5 AS float) AS decimal(5,2));` (state 6) and
/// `SELECT CAST('99999999999' AS bigint) * 1000000000;` (state 2).
#[test]
fn arithmetic_overflow_to_numeric_states_follow_the_source() {
    let from_varchar = SqlError::arithmetic_overflow_to_numeric("varchar");
    assert_eq!(from_varchar.number, 8115);
    assert_eq!(from_varchar.severity, 16);
    assert_eq!(from_varchar.state, 8);
    assert_eq!(
        from_varchar.message,
        "Converting varchar to data type numeric overflowed."
    );

    for from in ["nvarchar", "int", "bigint", "numeric", "money"] {
        assert_eq!(SqlError::arithmetic_overflow_to_numeric(from).state, 8);
    }
    for from in ["float", "real"] {
        let err = SqlError::arithmetic_overflow_to_numeric(from);
        assert_eq!(err.state, 6, "state from {from}");
        assert_eq!(
            err.message,
            format!("Converting {from} to data type numeric overflowed.")
        );
    }

    let expression = SqlError::arithmetic_overflow("expression", "bigint");
    assert_eq!(expression.state, 2);
    assert_eq!(
        expression.message,
        "Converting expression to data type bigint overflowed."
    );
}

#[test]
fn invalid_object_name_208_message_is_exact() {
    let err = SqlError::invalid_object_name("dbo.t");
    assert_eq!(err.number, 208);
    assert_eq!(err.message, "Unknown object name 'dbo.t'.");
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(err.line, 0);
}

#[test]
fn invalid_column_name_207_message_is_exact() {
    let err = SqlError::invalid_column_name("c");
    assert_eq!(err.number, 207);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "Unknown column name 'c'.");
}

#[test]
fn incorrect_syntax_near_sets_line() {
    let err = SqlError::incorrect_syntax_near("SELEC", 1);
    assert_eq!(err.number, 102);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(err.line, 1);
    assert_eq!(err.message, "Syntax error near 'SELEC'.");
}

#[test]
fn divide_by_zero_8134_has_no_arguments() {
    let err = SqlError::divide_by_zero();
    assert_eq!(err.number, 8134);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "Division by zero.");
}

#[test]
fn cannot_insert_null_515_message_and_state() {
    let err = SqlError::cannot_insert_null("c", "db.dbo.t", "INSERT");
    assert_eq!(err.number, 515);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
    assert_eq!(
        err.message,
        "Column 'c' of table 'db.dbo.t' does not accept NULL; INSERT fails."
    );
}

#[test]
fn cannot_insert_null_515_update_statement() {
    let err = SqlError::cannot_insert_null("c", "db.dbo.t", "UPDATE");
    assert!(err.message.ends_with(" UPDATE fails."));
}

#[test]
fn string_or_binary_data_truncated_2628_message_and_state() {
    let err = SqlError::string_or_binary_data_truncated("db.dbo.t", "code", "ABCDEFGHIJ");
    assert_eq!(err.number, 2628);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "Data too long for table 'db.dbo.t', column 'code': the value 'ABCDEFGHIJ' would be cut."
    );
}

#[test]
fn procedure_not_found_2812_message_is_exact() {
    let err = SqlError::procedure_not_found("dbo.p");
    assert_eq!(err.number, 2812);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 62);
    assert_eq!(err.message, "Unknown stored procedure 'dbo.p'.");
}

#[test]
fn cannot_open_database_4060_message_and_severity() {
    let err = SqlError::cannot_open_database("nope");
    assert_eq!(err.number, 4060);
    assert_eq!(err.severity, 11);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "The database \"nope\" requested at login could not be opened; login refused."
    );
}

#[test]
fn database_not_found_911_message_is_exact() {
    let err = SqlError::database_not_found("nope");
    assert_eq!(err.number, 911);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "No database named 'nope' exists; check the spelling of the name."
    );
}

#[test]
fn unique_violation_2627_message_is_exact() {
    let err = SqlError::unique_violation("PRIMARY KEY", "PK_t", "dbo.t", "(1)");
    assert_eq!(err.number, 2627);
    assert_eq!(err.severity, 14);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "The PRIMARY KEY constraint 'PK_t' rejects a duplicate key in object 'dbo.t': the value (1) exists already."
    );
}

#[test]
fn duplicate_key_index_2601_message_is_exact() {
    let err = SqlError::duplicate_key_index("dbo.t", "IX_t", "(1)");
    assert_eq!(err.number, 2601);
    assert_eq!(err.severity, 14);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "Duplicate key in object 'dbo.t' for unique index 'IX_t': the value (1) exists already."
    );
}

#[test]
fn fk_violation_547_with_column() {
    let err = SqlError::fk_violation(
        "INSERT",
        "FOREIGN KEY",
        "FK_child_parent",
        "db",
        "dbo.parent",
        Some("id"),
    );
    assert_eq!(err.number, 547);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert_eq!(
        err.message,
        "INSERT violates the FOREIGN KEY constraint \"FK_child_parent\" in database \"db\", table \"dbo.parent\", column 'id'."
    );
}

#[test]
fn fk_violation_547_without_column() {
    let err = SqlError::fk_violation("INSERT", "CHECK", "CK_t_positive", "db", "dbo.t", None);
    assert_eq!(
        err.message,
        "INSERT violates the CHECK constraint \"CK_t_positive\" in database \"db\", table \"dbo.t\"."
    );
}

#[test]
fn login_failed_18456_message_is_exact() {
    let err = SqlError::login_failed("sa");
    assert_eq!(err.number, 18456);
    assert_eq!(err.severity, 14);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "Login refused for user 'sa'.");
}

#[test]
fn conversion_failed_245_message_is_exact() {
    let err = SqlError::conversion_failed("varchar", "abc", "int");
    assert_eq!(err.number, 245);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "The varchar value 'abc' could not be converted to data type int."
    );
}

#[test]
fn error_converting_data_type_8114_message_and_state() {
    let err = SqlError::error_converting_data_type("varchar", "numeric");
    assert_eq!(err.number, 8114);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 5);
    assert_eq!(
        err.message,
        "Data type varchar could not be converted to numeric."
    );
}

#[test]
fn conversion_failed_datetime_241_message_is_exact() {
    let err = SqlError::conversion_failed_datetime();
    assert_eq!(err.number, 241);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "The character string could not be converted to a date or time."
    );
}

#[test]
fn arithmetic_overflow_8115_message_and_state() {
    let err = SqlError::arithmetic_overflow("expression", "int");
    assert_eq!(err.number, 8115);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 2);
    assert_eq!(
        err.message,
        "Converting expression to data type int overflowed."
    );
}

#[test]
fn unclosed_quotation_mark_is_105() {
    let err = SqlError::unclosed_quotation_mark("abc;", 4);
    assert_eq!(err.number, 105);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(err.line, 4);
    assert_eq!(
        err.message,
        "Quotation mark left open after the string 'abc;'."
    );
}

#[test]
fn missing_end_comment_mark_is_113() {
    let err = SqlError::missing_end_comment_mark(2);
    assert_eq!(err.number, 113);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(err.line, 2);
    assert_eq!(err.message, "Comment is not closed: '*/' expected.");
}

#[test]
fn incorrect_syntax_near_keyword_is_156() {
    let err = SqlError::incorrect_syntax_near_keyword("FROM", 3);
    assert_eq!(err.number, 156);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(err.line, 3);
    assert_eq!(err.message, "Syntax error near the keyword 'FROM'.");
}

#[test]
fn variable_already_declared_is_134() {
    let err = SqlError::variable_already_declared("@X");
    assert_eq!(err.number, 134);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "The variable '@X' is already declared; a batch or a procedure declares each name once."
    );
}

/// Error 133 for `GOTO skipit;` in a batch whose labels do not declare `skipit:`:
/// severity 15, state 1, and the name the `GOTO` wrote in the text.
#[test]
fn goto_label_not_declared_is_133() {
    let err = SqlError::goto_label_not_declared("skipit");
    assert_eq!(err.number, 133);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "GOTO names the label 'skipit', which this batch does not declare."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);
}

#[test]
fn must_declare_scalar_variable_is_137() {
    let err = SqlError::must_declare_scalar_variable("@x");
    assert_eq!(err.number, 137);
    assert_eq!(err.severity, 15);
    assert_eq!(err.message, "The scalar variable \"@x\" is not declared.");
}

/// The state of 137 follows the form: the variable a statement assigns sends 1, the one
/// it reads sends 2, with the queries in the comments, which answer the same number,
/// the same severity and the same text.
#[test]
fn must_declare_scalar_variable_states_follow_the_form() {
    // SELECT @x = 1; — and SET @x = 1;, and SELECT @x = 1, @y = 2;
    let assigned = SqlError::must_declare_scalar_variable_assigned("@x");
    // SELECT @x; — and SELECT @x + 1;, PRINT @x;, SELECT 1 WHERE @x = 1;
    let read = SqlError::must_declare_scalar_variable("@x");

    assert_eq!(assigned.state, 1);
    assert_eq!(read.state, 2);
    for err in [&assigned, &read] {
        assert_eq!(err.number, 137);
        assert_eq!(err.severity, 15);
        assert_eq!(err.message, "The scalar variable \"@x\" is not declared.");
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }

    // SELECT @x = @y; — the read of `@y` wins over the assignment of `@x`.
    assert_eq!(SqlError::must_declare_scalar_variable("@y").state, 2);
}

/// Error 1062 for `SELECT TOP (1) WITH TIES 1 AS c;`: severity 15 on the wire where
/// the catalogue holds 16, state 1, and a text with no argument.
#[test]
fn top_with_ties_without_order_by_is_1062() {
    let err = SqlError::top_with_ties_without_order_by();
    assert_eq!(err.number, 1062);
    assert_eq!(err.severity, 15);
    assert_eq!(
        message_template(1062).expect("1062 is catalogued").severity,
        16
    );
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "TOP WITH TIES requires an ORDER BY clause.");
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);
}

/// Errors 107 and 117 for each form of a qualified wildcard. Both severity 15 and
/// state 1; 107 prints the prefix, 117 the whole name, its `%S_MSG` filled with
/// `column` and its `%d` with 3.
#[test]
fn qualified_wildcard_errors_are_107_and_117() {
    let expected: &[(SqlError, u32, &str)] = &[
        (
            // SELECT t.*;
            SqlError::column_prefix_does_not_match("t"),
            107,
            "The column prefix 't' matches no table or alias of the query.",
        ),
        (
            // SELECT [dbo].[t].*; — the server prints the prefix brackets stripped.
            SqlError::column_prefix_does_not_match("dbo.t"),
            107,
            "The column prefix 'dbo.t' matches no table or alias of the query.",
        ),
        (
            // SELECT db1.dbo.t.*;
            SqlError::column_prefix_does_not_match("db1.dbo.t"),
            107,
            "The column prefix 'db1.dbo.t' matches no table or alias of the query.",
        ),
        (
            // SELECT a.b.c.d.*;
            SqlError::too_many_column_prefixes("a.b.c.d"),
            117,
            "The column name 'a.b.c.d' has too many prefixes; at most 3 are allowed.",
        ),
        (
            // SELECT a.b.c.d.e.*;
            SqlError::too_many_column_prefixes("a.b.c.d.e"),
            117,
            "The column name 'a.b.c.d.e' has too many prefixes; at most 3 are allowed.",
        ),
    ];
    for (err, number, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, 15, "severity of {number}");
        assert_eq!(err.state, 1, "state of {number}");
        assert_eq!(&err.message, message);
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }
}

/// Error 281 names the source in its `%ls`: six sources under one target print six
/// different words; four targets under one source print the same one, which is what
/// separates "the `%ls` is the source" from "the `%ls` is the target".
#[test]
fn invalid_style_number_names_the_source_type() {
    let sources: &[(&str, &str)] = &[
        // SELECT CONVERT(varchar(30), CAST('2020-01-01' AS date), 999);
        (
            "date",
            "Style 999 is not defined for converting date to a character string.",
        ),
        // SELECT CONVERT(varchar(30), CAST('10:00:00' AS time), 999);
        (
            "time",
            "Style 999 is not defined for converting time to a character string.",
        ),
        // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00:00' AS datetime), 999);
        (
            "datetime",
            "Style 999 is not defined for converting datetime to a character string.",
        ),
        // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00' AS smalldatetime), 999);
        (
            "smalldatetime",
            "Style 999 is not defined for converting smalldatetime to a character string.",
        ),
        // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00:00' AS datetime2), 999);
        (
            "datetime2",
            "Style 999 is not defined for converting datetime2 to a character string.",
        ),
        // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00:00 +02:00' AS datetimeoffset), 999);
        (
            "datetimeoffset",
            "Style 999 is not defined for converting datetimeoffset to a character string.",
        ),
    ];
    for (from, message) in sources {
        let err = SqlError::invalid_style_number(999, from);
        assert_eq!(err.number, 281);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(&err.message, message);
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }

    // CONVERT(nvarchar(30), …), CONVERT(char(30), …) and CONVERT(nchar(30), …) print
    // the same text as CONVERT(varchar(30), …): the target never reaches the message,
    // so the constructor has no parameter for it.
    assert_eq!(
        SqlError::invalid_style_number(999, "date").message,
        sources[0].1
    );

    // SELECT CONVERT(varchar(30), CAST('2020-01-01' AS date), -1); and …, 1000);
    assert_eq!(
        SqlError::invalid_style_number(-1, "date").message,
        "Style -1 is not defined for converting date to a character string."
    );
    assert_eq!(
        SqlError::invalid_style_number(1000, "date").message,
        "Style 1000 is not defined for converting date to a character string."
    );
}

/// Error 9807 and its state 0 for `SELECT CONVERT(date, '2020-01-01', 100);`; 8134
/// (state 1) and 235 (state 0) are asserted beside it as the two states a reader has
/// to tell apart.
#[test]
fn input_does_not_follow_style_is_9807_state_zero() {
    let err = SqlError::input_does_not_follow_style(100);
    assert_eq!(err.number, 9807);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert_eq!(
        err.message,
        "The input string does not match style 100; change the string or the style."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    // The two states a reader has to tell apart: 1 next to 0.
    assert_eq!(SqlError::divide_by_zero().state, 1);
    assert_eq!(SqlError::char_to_money_syntax().state, 0);

    // The seventeen strict styles (`SELECT CONVERT(date, '2020-01-01', <style>);` and
    // its siblings towards `time(7)`, `datetime2(7)` and `datetimeoffset(7)`): each
    // carries its own number into the `%d`, at state 0.
    for style in [
        6, 7, 8, 9, 12, 13, 14, 24, 100, 106, 107, 108, 109, 112, 113, 114, 130,
    ] {
        let err = SqlError::input_does_not_follow_style(style);
        assert_eq!(err.state, 0);
        assert!(
            err.message
                .starts_with(&format!("The input string does not match style {style};")),
            "style {style}"
        );
    }
}

/// Errors 1031 and 1014 for `SELECT TOP (101) PERCENT 1;` and
/// `SELECT TOP (NULL) PERCENT 1;`, both severity 15, beside 8134 and 8115 at 16 and
/// 127, 1060 and 102 at 15.
#[test]
fn top_percent_errors_are_1031_and_1014_severity_15() {
    let percent = SqlError::percent_out_of_range();
    assert_eq!(percent.number, 1031);
    assert_eq!(percent.severity, 15);
    assert_eq!(percent.state, 1);
    assert_eq!(
        percent.message,
        "A percentage has to lie between 0 and 100."
    );

    let invalid = SqlError::top_invalid_value();
    assert_eq!(invalid.number, 1014);
    assert_eq!(invalid.severity, 15);
    assert_eq!(invalid.state, 1);
    assert_eq!(
        invalid.message,
        "The value of the TOP or FETCH clause is not valid."
    );

    for err in [&percent, &invalid] {
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }

    // Neighbouring numbers keep the two severities apart: 15 against 16.
    assert_eq!(SqlError::top_negative().severity, 15);
    assert_eq!(SqlError::top_null().severity, 15);
    assert_eq!(SqlError::divide_by_zero().severity, 16);
}

/// The state of 506 follows the predicate, not the operand the message quotes: the
/// three operands narrow send 1, one Unicode operand among the three sends 2, with
/// the queries in the comments, which answer the same number, the same severity and
/// the same text.
#[test]
fn invalid_escape_states_follow_the_predicate() {
    // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE 'ab';
    let narrow = SqlError::invalid_escape("ab", "LIKE");
    // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE N'ab'; — the escape operand alone flips it,
    // and so do N'a' LIKE 'a' ESCAPE 'ab' and 'a' LIKE N'a' ESCAPE 'ab'.
    let unicode = SqlError::invalid_escape_unicode("ab", "LIKE");

    assert_eq!(narrow.state, 1);
    assert_eq!(unicode.state, 2);
    for err in [&narrow, &unicode] {
        assert_eq!(err.number, 506);
        assert_eq!(err.severity, 16);
        assert_eq!(
            err.message,
            "The escape character \"ab\" of the LIKE predicate must be a single character."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }

    // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE ''; and its Unicode twin: same rule.
    assert_eq!(SqlError::invalid_escape("", "LIKE").state, 1);
    assert_eq!(SqlError::invalid_escape_unicode("", "LIKE").state, 2);
}

/// The state of 8114 for a conversion written in an expression: `datetimeoffset`
/// sends 31 from each of the four narrow source names, the other targets send 5, with
/// the queries in the comments.
///
/// The four names are all in the table because the caller passes a bare type name and
/// the server normalizes the fixed-length ones in the message only: the state of
/// `char` is that of `varchar`, not a fallback.
#[test]
fn error_converting_data_type_states_follow_the_target_in_an_expression() {
    // SELECT CAST('0001-01-01T01:59:59+02:00' AS datetimeoffset(7));           -> varchar
    // SELECT CAST(N'0001-01-01T01:59:59+02:00' AS datetimeoffset(7));          -> nvarchar
    // SELECT CAST(CAST('…' AS char(30)) AS datetimeoffset(7));                 -> varchar
    // SELECT CAST(CAST(N'…' AS nchar(30)) AS datetimeoffset(7));               -> nvarchar
    // and the `varchar(max)` / `nvarchar(max)` forms, which print the same two names.
    for from in ["varchar", "nvarchar", "char", "nchar"] {
        let err = SqlError::error_converting_data_type(from, "datetimeoffset");
        assert_eq!(err.state, 31, "state of {from} to datetimeoffset");
        assert_eq!(err.number, 8114);
        assert_eq!(err.severity, 16);
        assert_eq!(
            err.message,
            format!("Data type {from} could not be converted to datetimeoffset.")
        );
    }

    // The other targets, from each source: state 5.
    // SELECT CAST('abc' AS bigint); / CAST('' AS numeric(10,2)); / CAST('abc' AS float);
    // SELECT CONVERT(varbinary(8), '0x1', 1);
    // EXEC sp_executesql N'SELECT @a', N'@a int', @a = 'abc';
    for from in ["varchar", "nvarchar", "char", "nchar"] {
        for to in ["bigint", "numeric", "float", "real", "varbinary", "int"] {
            let err = SqlError::error_converting_data_type(from, to);
            assert_eq!(err.state, 5, "state of {from} to {to}");
            assert_eq!(err.number, 8114);
            assert_eq!(err.severity, 16);
        }
    }
}

/// The counter-example to "the target settles the state of 8114": the same pair, the
/// same message, two states, because the path differs. Seven parameter-bound
/// variants, each state 5:
///
/// ```text
/// EXEC sp_executesql N'SELECT @a', N'@a datetimeoffset(7)', @a = '0001-01-01T01:59:59+02:00';
/// … the same with N'…', with datetimeoffset(0), with datetimeoffset(3),
/// … with a second parameter before it, with an OUTPUT parameter,
/// EXEC #p @a = '0001-01-01T01:59:59+02:00';   -- procedure taking a datetimeoffset(7)
/// ```
///
/// against 31 for `SELECT CAST('0001-01-01T01:59:59+02:00' AS datetimeoffset(7));`,
/// `CONVERT(datetimeoffset(7), …, 126)`, a `DECLARE` assignment and an `INSERT`. The
/// control pair `varchar` to `numeric` sends 5 down both paths, so the pair alone does
/// not explain the split.
///
/// The crate has no constructor for the parameter path and needs none: `vauban-types`
/// converts inside expressions alone. The test keeps the rule so that the day a caller
/// binds a parameter, it knows this constructor does not answer for it.
#[test]
fn error_converting_data_type_ignores_the_parameter_path() {
    let in_an_expression = SqlError::error_converting_data_type("varchar", "datetimeoffset");
    assert_eq!(in_an_expression.state, 31);

    // The control pair, which sends 5 on both paths.
    let control = SqlError::error_converting_data_type("varchar", "numeric");
    assert_eq!(control.state, 5);

    // The parameter path sends 5 for the very same message; the constructor renders
    // the expression path, so the two differ here — that is the point of the vector.
    let parameter_path_state = 5;
    assert_ne!(in_an_expression.state, parameter_path_state);
    assert_eq!(control.state, parameter_path_state);
}

#[test]
fn conversion_not_allowed_pair() {
    let explicit = SqlError::explicit_conversion_not_allowed("date", "int");
    assert_eq!(explicit.number, 529);
    assert_eq!(
        explicit.message,
        "No explicit conversion exists from date to int."
    );

    let implicit = SqlError::implicit_conversion_not_allowed("date", "int");
    assert_eq!(implicit.number, 257);
    assert_eq!(
        implicit.message,
        "No implicit conversion from date to int; use CONVERT explicitly."
    );

    let clash = SqlError::operand_type_clash("date", "int");
    assert_eq!(clash.number, 206);
    assert_eq!(
        clash.message,
        "Type mismatch: date cannot be combined with int."
    );
}

#[test]
fn datepart_not_supported_is_9810() {
    let err = SqlError::datepart_not_supported("hour", "dateadd", "date");
    assert_eq!(err.number, 9810);
    assert_eq!(
        err.message,
        "Datepart hour cannot be used with the date function dateadd on data type date."
    );
}

/// The state of 536 follows the calling function, not the number: `left` and `right`
/// send 6 where `substring` sends 8 (`SELECT LEFT('abc', -1);`,
/// `SELECT RIGHT('abc', -1);`, `SELECT SUBSTRING('abc', 1, -1);`). The message is
/// the same in the three, the function name apart.
#[test]
fn invalid_length_parameter_states_follow_the_function() {
    let expected: &[(&str, u8)] = &[("left", 6), ("right", 6), ("substring", 8)];
    for &(function, state) in expected {
        let err = SqlError::invalid_length_parameter(function);
        assert_eq!(err.number, 536, "number for {function}");
        assert_eq!(err.severity, 16, "severity for {function}");
        assert_eq!(err.state, state, "state for {function}");
        assert_eq!(
            err.message,
            format!("The length given to the {function} function is not valid.")
        );
    }

    // The case of the argument is the caller's, not a different code path.
    assert_eq!(SqlError::invalid_length_parameter("LEFT").state, 6);
    assert_eq!(
        SqlError::invalid_length_parameter("LEFT").message,
        "The length given to the LEFT function is not valid."
    );

    // A function without a row keeps the default 1.
    assert_eq!(SqlError::invalid_length_parameter("stuff").state, 1);
}

#[test]
fn function_arity_errors() {
    let exact = SqlError::function_arg_count("LEN", 1);
    assert_eq!(exact.number, 174);
    assert_eq!(exact.severity, 15);
    assert_eq!(
        exact.message,
        "The function LEN takes exactly 1 argument(s)."
    );

    let range = SqlError::function_arg_count_range("ROUND", 2, 3);
    assert_eq!(range.number, 189);
    assert_eq!(range.severity, 15);
    assert_eq!(
        range.message,
        "The function ROUND takes between 2 and 3 arguments."
    );
}

#[test]
fn invalid_argument_type_is_8116() {
    let err = SqlError::invalid_argument_type("uniqueidentifier", 1, "len");
    assert_eq!(err.number, 8116);
    assert_eq!(err.severity, 16);
    assert_eq!(
        err.message,
        "Data type uniqueidentifier is not accepted for argument 1 of the len function."
    );
}

#[test]
fn binder_errors() {
    let unknown = SqlError::not_a_recognized_name("NO_SUCH_FN", "built-in function");
    assert_eq!(unknown.number, 195);
    assert_eq!(unknown.severity, 15);
    assert_eq!(
        unknown.message,
        "'NO_SUCH_FN' is not a known built-in function name."
    );

    let operand = SqlError::invalid_operand_type("uniqueidentifier", "add");
    assert_eq!(operand.number, 8117);
    assert_eq!(
        operand.message,
        "Data type uniqueidentifier is not accepted by the add operator."
    );

    let boolean = SqlError::non_boolean_expression("1");
    assert_eq!(boolean.number, 4145);
    assert_eq!(boolean.severity, 15);
    assert_eq!(
        boolean.message,
        "A condition is expected near '1', but the expression is not boolean."
    );

    let ty = SqlError::cannot_find_data_type(1, "foo");
    assert_eq!(ty.number, 2715);
    assert_eq!(
        ty.message,
        "Column, parameter or variable #1: unknown data type foo."
    );
}

#[test]
fn types_errors() {
    let range = SqlError::out_of_range_conversion("date", "datetime");
    assert_eq!(range.number, 242);
    assert_eq!(range.state, 3);
    assert_eq!(
        range.message,
        "Converting date to datetime produced a value outside the target range."
    );

    let integral = SqlError::overflow_for_data_type("tinyint", 300);
    assert_eq!(integral.number, 220);
    assert_eq!(integral.state, 2);
    assert_eq!(
        integral.message,
        "Value out of range for data type tinyint: 300."
    );

    // `SELECT CAST(1e40 AS real);` prints the value with a C `%f`, not in scientific
    // notation: six decimals and 17 significant digits.
    let floating = SqlError::overflow_for_type("real", 1e40);
    assert_eq!(floating.number, 232);
    assert_eq!(floating.state, 2);
    assert_eq!(
        floating.message,
        "Value out of range for type real: 10000000000000000000000000000000000000000.000000."
    );
    assert!(
        floating
            .message
            .ends_with(": 10000000000000000000000000000000000000000.000000.")
    );

    let collation = SqlError::invalid_collation("Klingon_CI_AS");
    assert_eq!(collation.number, 448);
    assert_eq!(collation.message, "Unknown collation 'Klingon_CI_AS'.");

    let style = SqlError::unsupported_convert_style(112, "date", "datetimeoffset");
    assert_eq!(style.number, 9809);
    assert_eq!(
        style.message,
        "Style 112 is not defined for converting date to datetimeoffset."
    );

    let guid = SqlError::conversion_failed_guid();
    assert_eq!(guid.number, 8169);
    assert_eq!(
        guid.message,
        "The character string could not be converted to uniqueidentifier."
    );
}

#[test]
fn sysfn_errors() {
    let option = SqlError::not_a_recognized_option("bogus", "datepart");
    assert_eq!(option.number, 155);
    assert_eq!(option.severity, 15);
    assert_eq!(option.message, "'bogus' is not a known datepart option.");

    let overflow = SqlError::datetime_overflow("datetime");
    assert_eq!(overflow.number, 517);
    assert_eq!(
        overflow.message,
        "The addition overflowed the 'datetime' column."
    );

    let datediff = SqlError::datediff_overflow();
    assert_eq!(datediff.number, 535);
    assert_eq!(
        datediff.message,
        "datediff overflowed: too many dateparts separate the two instants. Call datediff with a coarser datepart."
    );

    let float_op = SqlError::invalid_floating_point_operation();
    assert_eq!(float_op.number, 3623);
    assert_eq!(
        float_op.message,
        "The floating point operation is undefined."
    );

    let coalesce = SqlError::coalesce_all_null();
    assert_eq!(coalesce.number, 4127);
    assert_eq!(
        coalesce.message,
        "COALESCE needs at least one argument other than the NULL constant."
    );

    let construct = SqlError::cannot_construct_type("datetime");
    assert_eq!(construct.number, 289);
    assert_eq!(
        construct.message,
        "Data type datetime cannot be built from these arguments: at least one value is out of range."
    );

    let truncated = SqlError::string_or_binary_truncated();
    assert_eq!(truncated.number, 8152);
    assert_eq!(truncated.severity, 16);
    assert_eq!(truncated.state, 17);
    assert_eq!(
        truncated.message,
        "Data too long: the string or binary value would be cut."
    );
}

/// Error 191, the same for seven nesting shapes and six paths into the parser:
/// severity 15, the catalogue's, so no override; state 1; a text with no argument.
///
/// The severity is asserted against the two other numbers of the same family: 125 is
/// severity 15 like 191, 8631 is severity 17. 125 is not catalogued; the assertion
/// below holds its value for the day it arrives, rather than pinning its absence.
#[test]
fn nested_too_deeply_is_191_severity_15_state_1() {
    let err = SqlError::nested_too_deeply(4);
    assert_eq!(err.number, 191);
    assert_eq!(err.severity, 15);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "The statement is nested too deeply; split it into smaller queries."
    );
    assert_eq!(err.line, 4);
    assert_eq!(err.procedure, None);

    // The catalogue carries the published severity unchanged: no `SEVERITY_OVERRIDES`
    // row for 191.
    assert_eq!(message_template(191).map(|def| def.severity), Some(15));
    assert!(!SEVERITY_OVERRIDES.iter().any(|(number, ..)| *number == 191));

    // The other two answers to deep nesting differ in severity as well as in number:
    // 125 severity 15 (an eleventh nested `CASE`) and 8631 severity 17 (a flat chain
    // of 20 000 `+`).
    for (number, severity) in [(125u32, 15u8), (8631, 17)] {
        if let Some(def) = message_template(number) {
            assert_eq!(def.severity, severity, "severity of error {number}");
        }
    }
}

/// Error 8631, the same four fields for ten operators, nine statement contexts and six
/// paths into the compiler. Severity 17 is the catalogue's, so no override; state 1;
/// a text with no argument.
#[test]
fn stack_limit_reached_is_8631_severity_17_state_1() {
    let err = SqlError::stack_limit_reached();
    assert_eq!(err.number, 8631);
    assert_eq!(err.severity, 17);
    assert_eq!(err.state, 1);
    assert_eq!(
        err.message,
        "Internal error: the server ran out of stack; the query is probably nested too deeply and needs simplifying."
    );
    // The binder puts the line of the *statement* on it further up, so it leaves this
    // constructor without one.
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    // The catalogue carries the published severity unchanged: no `SEVERITY_OVERRIDES`
    // row for 8631.
    assert_eq!(message_template(8631).map(|def| def.severity), Some(17));
    assert!(
        !SEVERITY_OVERRIDES
            .iter()
            .any(|(number, ..)| *number == 8631)
    );
}

/// The three numbers of the deep-expression family are distinct, and a caller that
/// confused two of them would send a different severity as well as a different number.
///
/// A flat chain of 20 000 `+` answers 8631 severity 17, 20 000 prefix `-` answers 191
/// severity 15, and an eleventh nested `CASE` answers 125 severity 15 state 4. 125 is
/// not catalogued here; the loop holds its severity for the day it arrives.
#[test]
fn the_three_numbers_of_the_family_are_not_interchangeable() {
    let flat = SqlError::stack_limit_reached();
    let nested = SqlError::nested_too_deeply(1);
    assert_ne!(flat.number, nested.number);
    assert_ne!(flat.severity, nested.severity, "17 against 15");
    assert_ne!(flat.message, nested.message);
    // Both are at state 1 all the same: the state does not tell them apart.
    assert_eq!(flat.state, nested.state);

    for (number, severity) in [(125u32, 15u8), (191, 15), (8631, 17)] {
        if let Some(def) = message_template(number) {
            assert_eq!(def.severity, severity, "severity of error {number}");
        }
    }
}

// ---------------------------------------------------------------------------------
// The DDL errors; the query that raises each line is in the comment next to it.
// ---------------------------------------------------------------------------------

/// Error 1801, as `CREATE DATABASE master;` printed it: severity 16, state 3.
#[test]
fn database_already_exists_1801() {
    let err = SqlError::database_already_exists("d");
    assert_eq!(err.number, 1801);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 3);
    assert_eq!(
        err.message,
        "A database named 'd' exists already; pick another name."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);
    // The catalogue publishes the severity the wire carried: no override row.
    assert_eq!(message_template(1801).map(|def| def.severity), Some(16));
    assert!(
        !SEVERITY_OVERRIDES
            .iter()
            .any(|(number, ..)| *number == 1801)
    );
}

/// Error 2714, as a second `CREATE TABLE t (a int);` printed it: severity 16, state 6.
#[test]
fn object_already_exists_2714() {
    let err = SqlError::object_already_exists("t");
    assert_eq!(err.number, 2714);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 6);
    assert_eq!(
        err.message,
        "An object named 't' exists already in the database."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);
    // CREATE TABLE dbo.t2 (a int); twice prints the name without its schema.
    assert_eq!(
        SqlError::object_already_exists("t2").message,
        "An object named 't2' exists already in the database."
    );
    assert_eq!(message_template(2714).map(|def| def.severity), Some(16));
    assert!(
        !SEVERITY_OVERRIDES
            .iter()
            .any(|(number, ..)| *number == 2714)
    );
}

/// Error 3701: the two `%S_MSG` carry the action and the kind, the template stays the
/// catalogue's, and the state follows the kind.
#[test]
fn cannot_drop_3701() {
    let expected: &[(&str, u8, &str, &str)] = &[
        // DROP TABLE nope;
        (
            "table",
            5,
            "nope",
            "Unable to drop the table 'nope': it does not exist or is not accessible.",
        ),
        // DROP DATABASE nope_db;
        (
            "database",
            1,
            "nope_db",
            "Unable to drop the database 'nope_db': it does not exist or is not accessible.",
        ),
        // DROP INDEX ix_nope ON t3; — the name joins the table and the index.
        (
            "index",
            7,
            "t3.ix_nope",
            "Unable to drop the index 't3.ix_nope': it does not exist or is not accessible.",
        ),
        // DROP VIEW nope_v;
        (
            "view",
            5,
            "nope_v",
            "Unable to drop the view 'nope_v': it does not exist or is not accessible.",
        ),
        // DROP PROCEDURE nope_p;
        (
            "procedure",
            5,
            "nope_p",
            "Unable to drop the procedure 'nope_p': it does not exist or is not accessible.",
        ),
        // DROP FUNCTION nope_f;
        (
            "function",
            5,
            "nope_f",
            "Unable to drop the function 'nope_f': it does not exist or is not accessible.",
        ),
        // DROP TRIGGER nope_tr;
        (
            "trigger",
            5,
            "nope_tr",
            "Unable to drop the trigger 'nope_tr': it does not exist or is not accessible.",
        ),
        // DROP SEQUENCE nope_sequence;
        (
            "sequence",
            5,
            "nope_sequence",
            "Unable to drop the sequence 'nope_sequence': it does not exist or is not accessible.",
        ),
        // DROP SEQUENCE dbo.nope_sequence; — the qualifier stays in the name.
        (
            "sequence",
            5,
            "dbo.nope_sequence",
            "Unable to drop the sequence 'dbo.nope_sequence': it does not exist or is not accessible.",
        ),
        // DROP SYNONYM nope_synonym;
        (
            "synonym",
            5,
            "nope_synonym",
            "Unable to drop the synonym 'nope_synonym': it does not exist or is not accessible.",
        ),
        // DROP SYNONYM dbo.nope_synonym;
        (
            "synonym",
            5,
            "dbo.nope_synonym",
            "Unable to drop the synonym 'dbo.nope_synonym': it does not exist or is not accessible.",
        ),
    ];
    for (kind, state, name, message) in expected {
        let err = SqlError::cannot_drop("drop", kind, name);
        assert_eq!(err.number, 3701);
        assert_eq!(err.severity, 11, "severity of the {kind} filling");
        assert_eq!(err.state, *state, "state of the {kind} filling");
        assert_eq!(&err.message, message, "message of the {kind} filling");
    }

    // The template holds two `%S_MSG` then the `%.*ls`, in that order.
    assert_eq!(
        message_template(3701).map(|def| def.template),
        Some("Unable to %S_MSG the %S_MSG '%.*ls': it does not exist or is not accessible.")
    );
    // A kind without a row falls back to the catalogue default; the fallback is read
    // on `assembly`, which has no row, against `sequence`, which has one.
    assert_eq!(
        SqlError::cannot_drop("drop", "assembly", "a").state,
        super::UNKNOWN_DDL_STATE
    );
    assert_ne!(
        SqlError::cannot_drop("drop", "sequence", "nope_sequence").state,
        super::UNKNOWN_DDL_STATE
    );
}

/// Error 226, as `BEGIN TRANSACTION; CREATE DATABASE d;` printed it: severity 16,
/// state 5, and the statement name in the `%ls`.
#[test]
fn statement_not_allowed_in_transaction_226() {
    let err = SqlError::statement_not_allowed_in_transaction("CREATE DATABASE");
    assert_eq!(err.number, 226);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 5);
    assert_eq!(
        err.message,
        "CREATE DATABASE is not allowed inside a multi-statement transaction."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    // BEGIN TRANSACTION; ALTER DATABASE scratch_db SET RECOVERY SIMPLE;
    let alter = SqlError::statement_not_allowed_in_transaction("ALTER DATABASE");
    assert_eq!(alter.state, 6);
    assert_eq!(
        alter.message,
        "ALTER DATABASE is not allowed inside a multi-statement transaction."
    );
    // BEGIN TRANSACTION; ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = 1;
    let scoped =
        SqlError::statement_not_allowed_in_transaction("ALTER DATABASE SCOPED CONFIGURATION");
    assert_eq!(scoped.state, 7);
    assert_eq!(
        scoped.message,
        "ALTER DATABASE SCOPED CONFIGURATION is not allowed inside a multi-statement transaction."
    );

    // BEGIN TRANSACTION; DROP DATABASE d; answers 574, not 226, and its sentence is
    // not the one of 226.
    let inside = message_template(574).expect("574 is in the catalog");
    assert_ne!(
        inside.template,
        message_template(226).map_or("", |d| d.template)
    );
}

/// Error 209, as `SELECT c FROM (SELECT 1 AS c) AS x CROSS JOIN (SELECT 2 AS c) AS y;`
/// printed it: severity 16, state 1.
#[test]
fn ambiguous_column_name_209() {
    let err = SqlError::ambiguous_column_name("a");
    assert_eq!(err.number, 209);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 1);
    assert_eq!(err.message, "Column name 'a' is ambiguous.");
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);
    // The same query over `[c d]` prints the name without its delimiters.
    assert_eq!(
        SqlError::ambiguous_column_name("c d").message,
        "Column name 'c d' is ambiguous."
    );
}

/// Error 3708 for `DROP DATABASE master;`: severity 16, state 4, and the three
/// `%S_MSG` filled `drop`, `database` and `database`.
#[test]
fn cannot_drop_system_database_3708() {
    let err = SqlError::cannot_drop_system_database("master");
    assert_eq!(err.number, 3708);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 4);
    assert_eq!(
        err.message,
        "Unable to drop the database 'master': it is a system database."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    // The three other system databases and the two case spellings. The delimited
    // forms print `master` and `msdb` without their brackets, and a list prints
    // `master`.
    for name in ["tempdb", "model", "msdb", "MASTER", "MaStEr"] {
        let other = SqlError::cannot_drop_system_database(name);
        assert_eq!(other.number, 3708);
        assert_eq!(other.severity, 16);
        assert_eq!(other.state, 4, "state of the {name} batch");
        assert_eq!(
            other.message,
            format!("Unable to drop the database '{name}': it is a system database.")
        );
    }

    // The sentence is not the one 3701 prints for a missing database, and neither is
    // the number.
    let missing = SqlError::cannot_drop("drop", "database", "nope_db");
    assert_eq!(missing.number, 3701);
    assert!(err.message.ends_with(": it is a system database."));
    assert!(
        missing
            .message
            .ends_with(": it does not exist or is not accessible.")
    );
}

/// Error 574 for `BEGIN TRANSACTION; DROP DATABASE d;`: severity 16, state 0,
/// `DROP DATABASE` in the `%ls`.
#[test]
fn drop_database_in_transaction_574() {
    let err = SqlError::drop_database_in_transaction();
    assert_eq!(err.number, 574);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert_eq!(
        err.message,
        "DROP DATABASE is not allowed inside a user transaction."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    // The same four fields over `master`: on that path the server answers 574 and not
    // the 3708 of `DROP DATABASE master;` alone.
    assert_ne!(
        err.number,
        SqlError::cannot_drop_system_database("master").number
    );

    // 226 is another sentence for another statement.
    let create = SqlError::statement_not_allowed_in_transaction("CREATE DATABASE");
    assert_eq!(create.number, 226);
    assert_ne!(create.state, err.state);
    assert!(err.message.contains("inside a user transaction."));
    assert!(
        create
            .message
            .contains("inside a multi-statement transaction.")
    );
}

/// Error 2705 for `CREATE TABLE t_dup (a int, a int);`: severity 16, state 3, the
/// column name then the table name.
#[test]
fn duplicate_column_name_2705() {
    let err = SqlError::duplicate_column_name("a", "t_dup");
    assert_eq!(err.number, 2705);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 3);
    assert_eq!(
        err.message,
        "Column 'a' of table 't_dup' is defined twice; column names have to be unique."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    // Four other shapes at state 3: the later spelling of the column
    // (`a int, A int`), the delimited table `[t dup]`, the temporary table `#t_dup`
    // and the table variable `DECLARE @t TABLE (a int, a int);`.
    for (column, table) in [("A", "t_dup"), ("a", "t dup"), ("a", "#t_dup"), ("a", "@t")] {
        let other = SqlError::duplicate_column_name(column, table);
        assert_eq!(other.number, 2705);
        assert_eq!(other.severity, 16);
        assert_eq!(other.state, 3, "state of the {table} batch");
        assert_eq!(
            other.message,
            format!(
                "Column '{column}' of table '{table}' is defined twice; column names have to be unique."
            )
        );
    }

    // The arguments are not interchangeable: swapping them swaps the two names.
    assert!(
        SqlError::duplicate_column_name("t_dup", "a")
            .message
            .contains("Column 't_dup' of table 'a'")
    );
}

/// Error 8148 for `CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2);`:
/// severity 16, state 0, the column then the table as written.
#[test]
fn multiple_column_defaults_8148() {
    let err = SqlError::multiple_column_defaults("a", "dbo.t");
    assert_eq!(err.number, 8148);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert_eq!(
        err.message,
        "A second DEFAULT constraint is refused for column 'a' of table 'dbo.t'."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    let quoted = SqlError::multiple_column_defaults("A col", "dbo.T Q");
    assert_eq!(
        quoted.message,
        "A second DEFAULT constraint is refused for column 'A col' of table 'dbo.T Q'."
    );
    let unqualified = SqlError::multiple_column_defaults("a", "t");
    assert_eq!(
        unqualified.message,
        "A second DEFAULT constraint is refused for column 'a' of table 't'."
    );
}

/// Error 1754 for `CREATE TABLE dbo.t (a int IDENTITY DEFAULT 1);`: severity 16,
/// state 0, the table without its schema then the column.
#[test]
fn default_on_identity_column_1754() {
    let err = SqlError::default_on_identity_column("t", "a");
    assert_eq!(err.number, 1754);
    assert_eq!(err.severity, 16);
    assert_eq!(err.state, 0);
    assert_eq!(
        err.message,
        "A DEFAULT cannot sit on an IDENTITY column. Table 't', column 'a'."
    );
    assert_eq!(err.line, 0);
    assert_eq!(err.procedure, None);

    let quoted = SqlError::default_on_identity_column("T I", "A col");
    assert_eq!(
        quoted.message,
        "A DEFAULT cannot sit on an IDENTITY column. Table 'T I', column 'A col'."
    );
}

/// Error 2715 has two states for one sentence: 3 for a scalar declaration and a
/// routine parameter, 6 for the column list of a table.
#[test]
fn cannot_find_data_type_in_table_2715_state_6() {
    // CREATE TABLE t_unknown (a foo);
    let column = SqlError::cannot_find_data_type_in_table(1, "foo");
    assert_eq!(column.number, 2715);
    assert_eq!(column.severity, 16);
    assert_eq!(column.state, 6);
    assert_eq!(
        column.message,
        "Column, parameter or variable #1: unknown data type foo."
    );
    assert_eq!(column.line, 0);
    assert_eq!(column.procedure, None);

    // CREATE TABLE t_unknown (a int, b foo); prints the rank of the column.
    assert_eq!(
        SqlError::cannot_find_data_type_in_table(2, "foo").message,
        "Column, parameter or variable #2: unknown data type foo."
    );
    // CREATE TABLE t_unknown (a dbo.foo); keeps the qualifier.
    assert_eq!(
        SqlError::cannot_find_data_type_in_table(1, "dbo.foo").message,
        "Column, parameter or variable #1: unknown data type dbo.foo."
    );

    // DECLARE @x foo; and the CREATE PROCEDURE parameter stay at 3: same number, same
    // sentence, another state, so the two constructors are not interchangeable.
    let declaration = SqlError::cannot_find_data_type(1, "foo");
    assert_eq!(declaration.state, 3);
    assert_eq!(declaration.message, column.message);
    assert_ne!(declaration.state, column.state);
}

/// Error 2716, a type recognised but given a length it does not accept
/// (`DECLARE @v int(10);`, `DECLARE @v sysname(10);`): `sysname` carries its own state,
/// the other types below carry the default.
#[test]
fn type_does_not_allow_length_2716_state_follows_the_type() {
    let sysname = SqlError::type_does_not_allow_length(1, "sysname");
    assert_eq!(sysname.number, 2716);
    assert_eq!(sysname.severity, 16);
    assert_eq!(sysname.state, 3);
    assert_eq!(
        sysname.message,
        "Column, parameter or variable #1: data type sysname does not accept a length."
    );
    assert_eq!(sysname.line, 0);
    assert_eq!(sysname.procedure, None);

    // DECLARE @v int(10); and DECLARE @v date(3); and DECLARE @v bit(1);
    for name in ["int", "date", "bit"] {
        let err = SqlError::type_does_not_allow_length(1, name);
        assert_eq!(err.state, 1, "{name}");
    }

    // DECLARE @a int, @b sysname(5); prints the rank of the column.
    assert_eq!(
        SqlError::type_does_not_allow_length(2, "sysname").message,
        "Column, parameter or variable #2: data type sysname does not accept a length."
    );
}

/// The DDL constructors carry their own states, not the catalogue default.
///
/// Counter-check: the fourteen calls below span seven distinct states and the ordered
/// vector holds each call, so resetting one of them to [`UNKNOWN_DDL_STATE`], or
/// giving 2715 one state for its two contexts, changes a vector and reddens this test.
#[test]
fn the_ddl_states_are_not_the_default() {
    let mut states = vec![
        // CREATE DATABASE master; tempdb; model; [master]; a user database twice.
        SqlError::database_already_exists("d").state,
        // CREATE TABLE t (a int); twice, and the same over an existing view.
        SqlError::object_already_exists("t").state,
        // DROP TABLE nope;
        SqlError::cannot_drop("drop", "table", "nope").state,
        // DROP DATABASE nope_db;
        SqlError::cannot_drop("drop", "database", "nope").state,
        // DROP INDEX ix_nope ON t3;
        SqlError::cannot_drop("drop", "index", "t3.ix_nope").state,
        // BEGIN TRANSACTION; CREATE DATABASE scratch_db;
        SqlError::statement_not_allowed_in_transaction("CREATE DATABASE").state,
        // SELECT c FROM (SELECT 1 AS c) AS x CROSS JOIN (SELECT 2 AS c) AS y; and
        // the five other shapes tried, which sent state 1 too.
        SqlError::ambiguous_column_name("c").state,
        // DROP SEQUENCE nope_sequence; and DROP SYNONYM nope_synonym;
        SqlError::cannot_drop("drop", "sequence", "nope_sequence").state,
        SqlError::cannot_drop("drop", "synonym", "nope_synonym").state,
        // DROP DATABASE master; tempdb; model; msdb; and the five other spellings.
        SqlError::cannot_drop_system_database("master").state,
        // BEGIN TRANSACTION; DROP DATABASE scratch_db; and the five other shapes.
        SqlError::drop_database_in_transaction().state,
        // CREATE TABLE t_dup (a int, a int); and the eight other shapes.
        SqlError::duplicate_column_name("a", "t_dup").state,
        // CREATE TABLE t_unknown (a foo); against DECLARE @x foo;, which sends 3.
        SqlError::cannot_find_data_type_in_table(1, "foo").state,
        SqlError::cannot_find_data_type(1, "foo").state,
    ];
    assert_eq!(states, vec![3, 6, 5, 1, 7, 5, 1, 5, 5, 4, 0, 3, 6, 3]);
    states.sort_unstable();
    states.dedup();
    assert_eq!(states, vec![0, 1, 3, 4, 5, 6, 7]);
}

/// Error 271 quotes the column between double quotes, where 8102 uses single ones
/// (`UPDATE dbo.tc SET c = 1;` over a computed column `c`).
#[test]
fn computed_column_271_is_rendered() {
    let error = SqlError::cannot_update_computed_column("c");
    assert_eq!((error.number, error.severity, error.state), (271, 16, 1));
    assert_eq!(
        error.message,
        "The column \"c\" cannot be modified: it is computed, or it comes out of a UNION."
    );
    assert_eq!(error.line, 0);
    let identity = SqlError::cannot_update_identity_column("id");
    assert_eq!(
        (identity.number, identity.severity, identity.state),
        (8102, 16, 1)
    );
    assert_eq!(identity.message, "Identity column 'id' cannot be updated.");
}

/// The DML, flow control, transaction and concurrency constructors, on four fields:
/// number, severity on the wire, state, and the substituted message compared as a
/// whole string. Each row is built with the arguments of the query quoted next to it,
/// so the comparison leaves no specifier behind. Thirty rows for twenty-nine
/// numbers: 1222 has one constructor per lock granularity.
#[test]
fn dml_and_transaction_messages_are_rendered() {
    let expected: &[(SqlError, u32, u8, u8, &str)] = &[
        (
            // SELECT 1 WHERE 1 = (SELECT a, b FROM dbo.t2);
            SqlError::only_one_expression_in_subquery(),
            116,
            16,
            1,
            "A subquery not introduced by EXISTS must select a single expression.",
        ),
        (
            // DECLARE @x int; SELECT @x = a, b FROM dbo.t2;
            SqlError::assignment_mixed_with_data_retrieval(),
            141,
            15,
            1,
            "A SELECT that assigns variables cannot also return rows.",
        ),
        (
            // SELECT DISTINCT a FROM dbo.t2 ORDER BY b;
            SqlError::order_by_item_not_in_distinct_select_list(),
            145,
            15,
            1,
            "With SELECT DISTINCT, each ORDER BY item has to be in the select list.",
        ),
        (
            // SELECT a FROM dbo.t2 UNION SELECT a, b FROM dbo.t2;
            SqlError::set_operator_column_count_mismatch(),
            205,
            16,
            1,
            "Each side of a UNION, INTERSECT or EXCEPT must select the same number of expressions.",
        ),
        (
            // EXEC('CREATE PROCEDURE dbo.p266 AS BEGIN TRANSACTION;'); EXEC dbo.p266;
            SqlError::transaction_count_after_execute(0, 1),
            266,
            16,
            2,
            "The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before 0, after 1).",
        ),
        (
            // SELECT (SELECT a FROM dbo.tv); over two rows
            SqlError::subquery_returned_more_than_one_value(),
            512,
            16,
            1,
            "The subquery returned several values, which is not allowed after a comparison operator or as an expression.",
        ),
        (
            // SELECT * FROM dbo.lone WITH (NOLOCK, TABLOCKX);
            SqlError::conflicting_locking_hints(),
            1047,
            15,
            1,
            "The locking hints contradict each other.",
        ),
        (
            // UPDATE dbo.lone WITH (NOLOCK) SET a = 1;
            SqlError::nolock_not_allowed_on_target(),
            1065,
            15,
            1,
            "NOLOCK and READUNCOMMITTED cannot be applied to the target table of INSERT, UPDATE, DELETE or MERGE.",
        ),
        (
            // ALTER TABLE dbo.lone ADD CONSTRAINT fk_bad FOREIGN KEY (nosuch) REFERENCES dbo.parent(id);
            SqlError::foreign_key_references_invalid_column("fk_bad", "nosuch", "lone"),
            1769,
            16,
            1,
            "Foreign key 'fk_bad' names the column 'nosuch', which the referencing table 'lone' does not have.",
        ),
        (
            // ALTER TABLE dbo.lone ADD CONSTRAINT fk_nopk FOREIGN KEY (a) REFERENCES dbo.nopk(id);
            SqlError::no_matching_key_in_referenced_table("dbo.nopk", "fk_nopk"),
            1776,
            16,
            0,
            "The referenced table 'dbo.nopk' has no primary or unique key matching the columns of foreign key 'fk_nopk'.",
        ),
        (
            // DROP TABLE dbo.parent; while another table references it
            SqlError::cannot_drop_referenced_object("dbo.parent"),
            3726,
            16,
            1,
            "Object 'dbo.parent' is referenced by a FOREIGN KEY constraint and cannot be dropped.",
        ),
        (
            // SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRANSACTION; SELECT v FROM dbo.s WHERE k = 1; in snapdb
            SqlError::snapshot_isolation_not_allowed("snapdb"),
            3952,
            16,
            1,
            "Database 'snapdb' does not allow snapshot isolation; enable it with ALTER DATABASE.",
        ),
        (
            // TRUNCATE TABLE dbo.parent; while another table references it
            SqlError::cannot_truncate_referenced_table("dbo.parent"),
            4712,
            16,
            1,
            "Table 'dbo.parent' is referenced by a FOREIGN KEY constraint and cannot be truncated.",
        ),
        (
            // ALTER TABLE dbo.notempty ADD b int NOT NULL; on a table holding one row
            SqlError::cannot_add_column_to_non_empty_table("b", "notempty"),
            4901,
            16,
            1,
            "A column added to a non-empty table has to be nullable, have a DEFAULT, or be an identity or timestamp column. Column 'b' cannot be added to table 'notempty'.",
        ),
        (
            // INSERT INTO dbo.ident VALUES (5, 1); on a table whose first column is an IDENTITY
            SqlError::identity_insert_requires_column_list("dbo.ident"),
            8101,
            16,
            1,
            "An explicit value for the identity column of table 'dbo.ident' requires a column list and IDENTITY_INSERT ON.",
        ),
        (
            // UPDATE dbo.ident SET id = 2;
            SqlError::cannot_update_identity_column("id"),
            8102,
            16,
            1,
            "Identity column 'id' cannot be updated.",
        ),
        (
            // SELECT a FROM dbo.t2 GROUP BY a HAVING b > 1;
            SqlError::column_invalid_in_having("dbo.t2", "b"),
            8121,
            16,
            1,
            "Column 'dbo.t2.b' of the HAVING clause is neither aggregated nor part of GROUP BY.",
        ),
        (
            // SELECT SUM(COUNT(*)) FROM dbo.t2;
            SqlError::nested_aggregate(),
            130,
            15,
            1,
            "An aggregate function cannot be applied to an expression that holds an aggregate or a subquery.",
        ),
        (
            // SELECT a FROM dbo.t2 GROUP BY SUM(b);
            SqlError::aggregate_in_group_by(),
            144,
            15,
            1,
            "A GROUP BY expression cannot hold an aggregate or a subquery.",
        ),
        (
            // SELECT a FROM dbo.t2 WHERE COUNT(*) > 1;
            SqlError::aggregate_in_where(),
            147,
            15,
            1,
            "An aggregate cannot appear in a WHERE clause, except inside a subquery of a HAVING clause or a select list, aggregating an outer reference.",
        ),
        (
            // INSERT INTO dbo.t2 VALUES (1); on a two-column table
            SqlError::column_count_does_not_match_table(),
            213,
            16,
            1,
            "The supplied values do not match the columns of the table.",
        ),
        (
            // INSERT INTO dbo.ident (id, v) VALUES (1, 1); with IDENTITY_INSERT off
            SqlError::identity_insert_is_off("ident"),
            544,
            16,
            1,
            "An explicit value for the identity column of table 'ident' requires IDENTITY_INSERT ON.",
        ),
        (
            // SET IDENTITY_INSERT dbo.t1 ON; SET IDENTITY_INSERT dbo.t2 ON;
            SqlError::identity_insert_already_on("master", "dbo", "t1", "dbo.t2"),
            8107,
            16,
            1,
            "A session holds IDENTITY_INSERT for one table at a time; 'master.dbo.t1' already has it, and 'dbo.t2' is refused.",
        ),
        (
            // SET IDENTITY_INSERT dbo.plain ON;
            SqlError::identity_insert_table_has_no_identity("dbo.plain"),
            8106,
            16,
            1,
            "Table 'dbo.plain' lacks an IDENTITY column; SET IDENTITY_INSERT cannot run on it.",
        ),
        (
            // SET IDENTITY_INSERT dbo.nosuch ON;
            SqlError::cannot_find_object_for_identity_insert("dbo.nosuch"),
            1088,
            16,
            11,
            "Object \"dbo.nosuch\" was not found: it does not exist or is not accessible.",
        ),
        (
            // two sessions updating the same two rows in opposite order
            SqlError::deadlock_victim(60, "lock"),
            1205,
            13,
            51,
            "Process 60 was chosen as the victim of a deadlock on lock resources; run the transaction again.",
        ),
        (
            // SET LOCK_TIMEOUT 2000; SELECT v FROM dbo.s WHERE k = 1; behind an uncommitted update
            SqlError::lock_request_timeout_on_row(),
            1222,
            16,
            51,
            "The lock could not be acquired before the timeout.",
        ),
        (
            // SET LOCK_TIMEOUT 2000; DROP TABLE dbo.s; behind an uncommitted update
            SqlError::lock_request_timeout_on_object(),
            1222,
            16,
            56,
            "The lock could not be acquired before the timeout.",
        ),
        (
            // COMMIT TRANSACTION; outside a transaction
            SqlError::commit_without_begin(),
            3902,
            16,
            1,
            "COMMIT TRANSACTION without a matching BEGIN TRANSACTION.",
        ),
        (
            // ROLLBACK TRANSACTION; outside a transaction
            SqlError::rollback_without_begin(),
            3903,
            16,
            1,
            "ROLLBACK TRANSACTION without a matching BEGIN TRANSACTION.",
        ),
        (
            // UPDATE dbo.s SET v = v + 100 WHERE k = 1; under snapshot isolation, after another session committed the row
            SqlError::snapshot_update_conflict("dbo.s", "cc"),
            3960,
            16,
            2,
            "Update conflict under snapshot isolation: a row of table 'dbo.s' in database 'cc' was changed by another transaction. Retry or change the isolation level.",
        ),
        (
            // SELECT a, b FROM dbo.t2 GROUP BY a;
            SqlError::column_invalid_in_select_list("dbo.t2", "b"),
            8120,
            16,
            1,
            "Column 'dbo.t2.b' of the select list is neither aggregated nor part of GROUP BY.",
        ),
    ];
    for (err, number, severity, state, message) in expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of error {number}");
        assert_eq!(err.state, *state, "state of error {number}");
        assert_eq!(&err.message, message, "message of error {number}");
        assert!(
            !err.message.contains('%'),
            "error {number} still carries a specifier"
        );
        assert_eq!(err.line, 0, "line of error {number}");
    }
    assert_eq!(expected.len(), 32);
    let mut numbers: Vec<u32> = expected.iter().map(|row| row.1).collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), 31);
}

/// Fifteen DML numbers and their sixteen constructors, 1222 counting twice, each
/// sending the catalogue's severity.
#[test]
fn dml_constructors_exist_for_the_catalogued_numbers() {
    let calls: &[(SqlError, u32, u8)] = &[
        (SqlError::column_count_does_not_match_table(), 213, 16),
        (SqlError::identity_insert_is_off("ident"), 544, 16),
        (SqlError::deadlock_victim(60, "lock"), 1205, 13),
        (SqlError::lock_request_timeout_on_row(), 1222, 16),
        (SqlError::lock_request_timeout_on_object(), 1222, 16),
        (SqlError::commit_without_begin(), 3902, 16),
        (SqlError::rollback_without_begin(), 3903, 16),
        (SqlError::snapshot_update_conflict("dbo.s", "cc"), 3960, 16),
        (
            SqlError::column_invalid_in_select_list("dbo.t2", "b"),
            8120,
            16,
        ),
        (SqlError::more_columns_than_values(), 109, 15),
        (SqlError::more_values_than_columns(), 110, 15),
        (SqlError::select_list_shorter_than_insert_list(), 120, 15),
        (SqlError::select_list_longer_than_insert_list(), 121, 15),
        (SqlError::column_specified_more_than_once("a"), 264, 16),
        (SqlError::default_or_null_as_identity_value(), 339, 16),
        (SqlError::table_value_constructor_rows_differ(), 10709, 16),
    ];
    for (err, number, severity) in calls {
        assert_eq!(err.number, *number);
        assert_eq!(err.severity, *severity, "severity of error {number}");
        assert!(
            !err.message.contains('%'),
            "error {number} still carries a specifier"
        );
        let def = message_template(*number).expect("catalogued");
        assert_eq!(def.severity, *severity, "published severity of {number}");
    }
    assert_eq!(calls.len(), 16);
    let mut numbers: Vec<u32> = calls.iter().map(|row| row.1).collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), 15);
}

/// Error 1222 sends two states, so it carries two constructors rather than a state
/// the caller patches: 51 while waiting for a row lock
/// (`SELECT v FROM dbo.s WHERE k = 1;` and `UPDATE dbo.s SET v = v + 5 WHERE k = 1;`
/// behind another session's uncommitted update) and 56 while waiting for an object
/// lock (`ALTER TABLE dbo.s ADD zz int NULL;`, `DROP TABLE dbo.s;` and a `TABLOCKX`
/// read). Number, severity and sentence are shared; the state is the axis that moved.
#[test]
fn lock_request_timeout_1222_has_one_state_per_granularity() {
    let row = SqlError::lock_request_timeout_on_row();
    let object = SqlError::lock_request_timeout_on_object();
    assert_eq!((row.number, object.number), (1222, 1222));
    assert_eq!((row.severity, object.severity), (16, 16));
    assert_eq!(row.message, object.message);
    assert_eq!(row.state, 51);
    assert_eq!(object.state, 56);
}

/// The two `%ld` of 266 keep the order of the template, previous count then current:
/// a procedure opening two transactions prints `after 2`.
#[test]
fn transaction_count_after_execute_prints_previous_then_current() {
    let err = SqlError::transaction_count_after_execute(0, 2);
    assert_eq!(
        err.message,
        "The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before 0, after 2)."
    );
    assert_eq!((err.number, err.severity, err.state), (266, 16, 2));
}

/// Error 116 travels with severity 16 where the catalogue holds 15: the catalogue
/// keeps its value and the constructor overrides it, as `SEVERITY_OVERRIDES` records.
#[test]
fn only_one_expression_in_subquery_overrides_the_published_severity() {
    let err = SqlError::only_one_expression_in_subquery();
    assert_eq!(err.severity, 16);
    assert_eq!(message_template(116).map(|def| def.severity), Some(15));
    assert!(SEVERITY_OVERRIDES.contains(&(116, 16, 15)));
    for number in [141u32, 145, 205, 266, 512, 1047, 1065, 1769, 1776, 3726] {
        assert!(
            !SEVERITY_OVERRIDES.iter().any(|(n, ..)| *n == number),
            "error {number} takes no severity override"
        );
    }
}
/// The DDL and name resolution constructors, on four fields: number, severity on the
/// wire, state, and the substituted message compared as a whole string. Each row is
/// built with the arguments of the query quoted next to it. Thirty-three rows for
/// twenty-eight numbers: 1909 has one constructor per column list, 3723 one per
/// constraint kind and 103 one per shape of token; 208 and 447 each take a second
/// constructor for another context.
#[test]
fn ddl_and_name_resolution_messages_are_rendered() {
    let identifier = "b".repeat(128);
    let digits = "9".repeat(128);
    let money = format!("${}", "1".repeat(127));
    let expected: Vec<(SqlError, u32, u8, u8, String)> = vec![
        (
            // CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY, b int NOT NULL PRIMARY KEY);
            SqlError::multiple_primary_keys("dbo.t"),
            8110,
            16,
            0,
            "Table 'dbo.t' can have a single PRIMARY KEY constraint.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int NULL, CONSTRAINT pk_t PRIMARY KEY (a));
            SqlError::primary_key_on_nullable_column("t"),
            8111,
            16,
            1,
            "A PRIMARY KEY of table 't' cannot include a nullable column."
                .to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY CLUSTERED, b int NOT NULL UNIQUE CLUSTERED);
            SqlError::multiple_clustered_index_constraints("dbo.t"),
            8112,
            16,
            0,
            "Constraints of table 'dbo.t' can create a single clustered index."
                .to_owned(),
        ),
        (
            // CREATE CLUSTERED INDEX ix_b ON dbo.t (b); after CREATE CLUSTERED INDEX ix_a ON dbo.t (a);
            SqlError::more_than_one_clustered_index("dbo.t", "ix_a"),
            1902,
            16,
            3,
            "The table 'dbo.t' can have a single clustered index; drop 'ix_a' first.".to_owned(),
        ),
        (
            // CREATE INDEX ix_t ON dbo.t (nosuchcolumn);
            SqlError::column_does_not_exist_in_target("nosuchcolumn"),
            1911,
            16,
            1,
            "The target table or view has no column named 'nosuchcolumn'.".to_owned(),
        ),
        (
            // CREATE INDEX ix_t ON dbo.t (b); after the same name was created on (a)
            SqlError::index_name_already_exists("ix_t", "dbo.t"),
            1913,
            16,
            1,
            "An index or statistics named 'ix_t' exists already on table 'dbo.t'.".to_owned(),
        ),
        (
            // CREATE INDEX ix_t ON dbo.t (a, a);
            SqlError::duplicate_column_in_index("a"),
            1909,
            16,
            1,
            "The index cannot repeat a column: 'a' is listed twice."
                .to_owned(),
        ),
        (
            // CREATE INDEX ix_t ON dbo.t (a) INCLUDE (a);
            SqlError::duplicate_column_in_included_columns("a"),
            1909,
            16,
            2,
            "The index cannot repeat a column: 'a' is listed twice."
                .to_owned(),
        ),
        (
            // after 8111 and after 1909 (`COULD_NOT_CREATE_CONSTRAINT_1750_SEVERITY`)
            SqlError::could_not_create_constraint_or_index(),
            1750,
            16,
            0,
            "The constraint or index was not created; the previous errors say why.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int IDENTITY DEFAULT 1);
            SqlError::default_on_identity_column("t", "a"),
            1754,
            16,
            0,
            "A DEFAULT cannot sit on an IDENTITY column. Table 't', column 'a'.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT c1 PRIMARY KEY, b int NOT NULL CONSTRAINT c1 UNIQUE);
            SqlError::duplicate_name_in_this_context("c1"),
            8168,
            16,
            0,
            "The name 'c1' is used more than once for a constraint, column, index or trigger here; names have to be unique.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t2 (a int NOT NULL INDEX ix_t (a), b int NOT NULL, INDEX ix_t (b));
            SqlError::duplicate_index_name_in_this_context("ix_t"),
            8168,
            16,
            1,
            "The name 'ix_t' is used more than once for a constraint, column, index or trigger here; names have to be unique.".to_owned(),
        ),
        (
            // DROP INDEX pk_t ON dbo.t; over the index of a PRIMARY KEY
            SqlError::cannot_drop_constraint_index("dbo.t.pk_t", "PRIMARY KEY"),
            3723,
            16,
            4,
            "Index 'dbo.t.pk_t' enforces a PRIMARY KEY constraint and cannot be dropped directly.".to_owned(),
        ),
        (
            // DROP INDEX uq_t ON dbo.t; over the index of a UNIQUE constraint
            SqlError::cannot_drop_constraint_index("dbo.t.uq_t", "UNIQUE KEY"),
            3723,
            16,
            5,
            "Index 'dbo.t.uq_t' enforces a UNIQUE KEY constraint and cannot be dropped directly.".to_owned(),
        ),
        (
            // ALTER TABLE dbo.t DROP CONSTRAINT nosuch;
            SqlError::constraint_not_on_table("nosuch"),
            3728,
            16,
            1,
            "'nosuch' is not the name of a constraint here.".to_owned(),
        ),
        (
            // ALTER TABLE dbo.nosuch ADD c int NULL;
            SqlError::cannot_find_object_to_alter_table("dbo.nosuch"),
            4902,
            16,
            1,
            "Object \"dbo.nosuch\" was not found: it does not exist or this login lacks permissions.".to_owned(),
        ),
        (
            // ALTER TABLE dbo.t DROP COLUMN nosuch;
            SqlError::alter_table_drop_column_missing("nosuch", "t"),
            4924,
            16,
            1,
            "ALTER TABLE DROP COLUMN could not run: column 'nosuch' is missing from table 't'.".to_owned(),
        ),
        (
            // CREATE INDEX ix ON dbo.ix_t(b); ALTER TABLE dbo.ix_t DROP COLUMN b;
            SqlError::object_depends_on_column("index", "ix", "b"),
            5074,
            16,
            1,
            "index 'ix' still depends on column 'b'.".to_owned(),
        ),
        (
            // CREATE INDEX ix_v ON dbo.v (a); over a view created without WITH SCHEMABINDING
            SqlError::cannot_create_index_on_view("v"),
            1939,
            16,
            1,
            "The index requires the view 'v' to be schema bound."
                .to_owned(),
        ),
        (
            // CREATE INDEX ix_t ON dbo.nosuch (a);
            SqlError::cannot_find_object_to_index("dbo.nosuch"),
            1088,
            16,
            12,
            "Object \"dbo.nosuch\" was not found: it does not exist or is not accessible.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a decimal(9,2) IDENTITY(1,1) NOT NULL);
            SqlError::invalid_identity_column_type("a"),
            2749,
            16,
            2,
            "Identity column 'a' has to be a non-nullable int, bigint, smallint, tinyint, or decimal or numeric of scale 0.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int IDENTITY(1,1) NULL);
            SqlError::identity_on_nullable_column("a", "dbo.t"),
            8147,
            16,
            1,
            "Column 'a' of table 'dbo.t' is nullable and cannot be an IDENTITY column."
                .to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2);
            SqlError::multiple_column_defaults("a", "dbo.t"),
            8148,
            16,
            0,
            "A second DEFAULT constraint is refused for column 'a' of table 'dbo.t'."
                .to_owned(),
        ),
        (
            // SELECT 1 FROM sys.sp_executesql;
            SqlError::invalid_object_name_of_another_type("sys.sp_executesql"),
            208,
            16,
            3,
            "Unknown object name 'sys.sp_executesql'.".to_owned(),
        ),
        (
            // CREATE TABLE dbo.t (a int COLLATE Latin1_General_CI_AS NOT NULL);
            SqlError::collate_on_non_string_column("int"),
            447,
            16,
            1,
            "COLLATE cannot apply to an expression of type int.".to_owned(),
        ),
        (
            // SELECT 1 FROM @tv;
            SqlError::must_declare_table_variable("@tv"),
            1087,
            15,
            2,
            "The table variable \"@tv\" is not declared.".to_owned(),
        ),
        (
            // SELECT 1 FROM nosuchserver.somedb.dbo.t;
            SqlError::linked_server_not_found("nosuchserver"),
            7202,
            11,
            2,
            "Server 'nosuchserver' is not registered in sys.servers; check the name or add it with sp_addlinkedserver.".to_owned(),
        ),
        (
            // SELECT 1 FROM dbo.t AS x, dbo.t AS x;
            SqlError::duplicate_correlation_name("x"),
            1011,
            16,
            1,
            "Correlation name 'x' is used more than once in the FROM clause.".to_owned(),
        ),
        (
            // SELECT 1 FROM dbo.t AS u JOIN dbo.u ON 1 = 1;
            SqlError::correlation_name_is_a_table_name("u", "dbo.u"),
            1012,
            16,
            1,
            "Correlation name 'u' is also the exposed name of table 'dbo.u'; give one of them another alias.".to_owned(),
        ),
        (
            // SELECT 1 FROM dbo.t, dbo.t;
            SqlError::same_exposed_names("dbo.t", "dbo.t"),
            1013,
            16,
            1,
            "\"dbo.t\" and \"dbo.t\" expose the same name in the FROM clause; give them distinct aliases.".to_owned(),
        ),
        (
            // UPDATE t SET a = 1 FROM dbo.t AS x, dbo.t AS y;
            SqlError::table_is_ambiguous("t"),
            8154,
            16,
            1,
            "The reference to table 't' is ambiguous.".to_owned(),
        ),
        (
            // SELECT * FROM (SELECT a, a + 1 FROM dbo.t) AS d;
            SqlError::no_column_name_in_derived_table(2, "d"),
            8155,
            16,
            2,
            "Column 2 of 'd' has no name.".to_owned(),
        ),
        (
            // SELECT 1 AS [];
            SqlError::object_or_column_name_missing(),
            1038,
            15,
            4,
            "An object or column name is empty: each SELECT INTO column needs a name, and an alias written \"\" or [] is not allowed.".to_owned(),
        ),
        (
            // SELECT 1 AS "<129 b> left unclosed
            SqlError::identifier_too_long(&identifier, 128),
            103,
            15,
            4,
            format!("The identifier beginning with '{identifier}' exceeds the maximum length of 128."),
        ),
        (
            // SELECT 1 x <200 digits>;
            SqlError::number_too_long(&digits, 128),
            103,
            15,
            5,
            format!("The number beginning with '{digits}' exceeds the maximum length of 128."),
        ),
        (
            // SELECT 1 x $<130 digits>;
            SqlError::money_literal_too_long(&money, 128),
            103,
            15,
            6,
            format!("The number beginning with '{money}' exceeds the maximum length of 128."),
        ),
        (
            // CREATE TABLE nosuchdb.dbo.t (a int NOT NULL);
            SqlError::database_does_not_exist("nosuchdb"),
            2702,
            16,
            2,
            "No database named 'nosuchdb' exists.".to_owned(),
        ),
        (
            // TRUNCATE TABLE dbo.nosuch;
            SqlError::cannot_find_object_to_truncate("nosuch"),
            4701,
            16,
            1,
            "Object \"nosuch\" was not found: it does not exist or is not accessible.".to_owned(),
        ),
        (
            // a login asking for the database nosuchdb
            SqlError::cannot_open_login_database("nosuchdb", "master"),
            4063,
            11,
            1,
            "The database \"nosuchdb\" requested at login could not be opened; the default database \"master\" is used instead.".to_owned(),
        ),
    ];
    for (err, number, severity, state, message) in &expected {
        assert_eq!(err.number, *number, "number of {message}");
        assert_eq!(err.severity, *severity, "severity of error {number}");
        assert_eq!(err.state, *state, "state of error {number}");
        assert_eq!(&err.message, message, "message of error {number}");
        assert!(
            !err.message.contains('%'),
            "error {number} still carries a specifier"
        );
        assert_eq!(err.line, 0, "line of error {number}");
    }
    assert_eq!(expected.len(), 39);
    let mut numbers: Vec<u32> = expected.iter().map(|row| row.1).collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), 34);
}

/// Error 1909 sends two states, so it carries two constructors rather than a state
/// the caller patches: 1 when the repeated column is in the key
/// list (`CREATE INDEX ix_t ON dbo.t (a, a);`, a `PRIMARY KEY (a, a)`) and
/// 2 when it is in the `INCLUDE` list (`… (a) INCLUDE (a);` and `… (a) INCLUDE
/// (b, b);`). Number, severity and sentence are shared; the state is the axis that
/// moved.
#[test]
fn duplicate_column_1909_has_one_state_per_column_list() {
    let key = SqlError::duplicate_column_in_index("a");
    let included = SqlError::duplicate_column_in_included_columns("a");
    assert_eq!((key.number, included.number), (1909, 1909));
    assert_eq!((key.severity, included.severity), (16, 16));
    assert_eq!(key.message, included.message);
    assert_eq!(key.state, 1);
    assert_eq!(included.state, 2);
}

/// Error 8168 sends two states, so it carries two constructors rather than a state
/// the caller patches: 0 when the repeated name is a constraint
/// name (a `CREATE TABLE` with `CONSTRAINT c1 PRIMARY KEY` and
/// `CONSTRAINT c1 UNIQUE`, or two `CONSTRAINT c1 CHECK`) and 1 when it is the name
/// of two indexes of a `CREATE TABLE`
/// (`CREATE TABLE dbo.t2 (a int NOT NULL INDEX ix_t (a), b int NOT NULL, INDEX ix_t (b));`).
/// Number, severity and sentence are shared; the state is the
/// axis that moved. Two columns of one name are a third answer, 2705 state 3, which
/// `duplicate_column_name` builds.
#[test]
fn duplicate_name_8168_has_one_state_per_context() {
    let constraint = SqlError::duplicate_name_in_this_context("c1");
    let index = SqlError::duplicate_index_name_in_this_context("c1");
    assert_eq!((constraint.number, index.number), (8168, 8168));
    assert_eq!((constraint.severity, index.severity), (16, 16));
    assert_eq!(constraint.message, index.message);
    assert_eq!(constraint.state, 0);
    assert_eq!(index.state, 1);
    let column = SqlError::duplicate_column_name("a", "t");
    assert_eq!((column.number, column.state), (2705, 3));
}

/// Error 3723 reads its state from the constraint kind it prints: 4 for a
/// `PRIMARY KEY`, 5 for a `UNIQUE KEY`, and the catalogue default for another kind.
#[test]
fn drop_index_3723_state_follows_the_constraint_kind() {
    assert_eq!(
        SqlError::cannot_drop_constraint_index("dbo.t.pk", "PRIMARY KEY").state,
        4
    );
    assert_eq!(
        SqlError::cannot_drop_constraint_index("dbo.t.uq", "UNIQUE KEY").state,
        5
    );
    assert_eq!(
        SqlError::cannot_drop_constraint_index("dbo.t.xx", "CHECK").state,
        UNKNOWN_DDL_STATE
    );
}

/// Error 103 sends three states, one per shape of token, and the
/// `%S_MSG` follows two of the three: `identifier` for a name longer than 128
/// characters, `number` for a long literal, whether or not it opens with `$`. The
/// money literal and the plain one differ by their state alone, which is why they have
/// two constructors.
#[test]
fn identifier_too_long_103_has_one_state_per_shape() {
    let name = SqlError::identifier_too_long("abc", 128);
    let number = SqlError::number_too_long("123", 128);
    let money = SqlError::money_literal_too_long("$123", 128);
    assert_eq!((name.state, number.state, money.state), (4, 5, 6));
    assert!(
        name.message
            .starts_with("The identifier beginning with 'abc'")
    );
    assert!(
        number
            .message
            .starts_with("The number beginning with '123'")
    );
    assert!(
        money
            .message
            .starts_with("The number beginning with '$123'")
    );
    assert_eq!(
        (name.severity, number.severity, money.severity),
        (15, 15, 15)
    );
}

/// Error 208 keeps one entry and takes a second constructor: the state is what
/// separates a name that resolves to nothing (1) from a name that resolves to an
/// object of another type (3). The sentence is the same in both.
#[test]
fn invalid_object_name_208_has_one_constructor_per_resolution() {
    let missing = SqlError::invalid_object_name("sys.sp_executesql");
    let other_type = SqlError::invalid_object_name_of_another_type("sys.sp_executesql");
    assert_eq!(missing.message, other_type.message);
    assert_eq!((missing.number, other_type.number), (208, 208));
    assert_eq!((missing.severity, other_type.severity), (16, 16));
    assert_eq!((missing.state, other_type.state), (1, 3));
}

/// Error 447 keeps one entry and takes a second constructor: the first carries the
/// state 0 of a `COLLATE` in an expression, the second the state 1 of a `COLLATE` in
/// a column definition ([`COLLATE_ON_NON_STRING_447_COLUMN_STATE`]). Number, severity
/// and sentence are shared; the state is the axis that moves.
#[test]
fn collate_on_non_string_has_one_constructor_per_context() {
    for ty in ["int", "date"] {
        let expression = SqlError::collate_on_non_string(ty);
        let column = SqlError::collate_on_non_string_column(ty);
        assert_eq!(expression.message, column.message);
        assert_eq!(
            expression.message,
            format!("COLLATE cannot apply to an expression of type {ty}.")
        );
        assert_eq!((expression.number, column.number), (447, 447));
        assert_eq!((expression.severity, column.severity), (16, 16));
        assert_eq!((expression.state, column.state), (0, 1));
    }
}

/// Seven numbers whose catalogued severity is not the one the server sends; the
/// catalogue keeps its value and the constructors override it, as
/// `SEVERITY_OVERRIDES` records. 1750 is the widest gap: catalogued 10, the class of
/// an informational message, and sent 16.
#[test]
fn ddl_and_name_resolution_severity_overrides() {
    let overridden: &[(SqlError, u32, u8)] = &[
        (SqlError::duplicate_correlation_name("x"), 1011, 15),
        (
            SqlError::correlation_name_is_a_table_name("u", "dbo.u"),
            1012,
            15,
        ),
        (SqlError::same_exposed_names("dbo.t", "dbo.t"), 1013, 15),
        (SqlError::cannot_find_object_to_index("dbo.t"), 1088, 15),
        (SqlError::could_not_create_constraint_or_index(), 1750, 10),
        (SqlError::table_is_ambiguous("t"), 8154, 15),
        (SqlError::no_column_name_in_derived_table(2, "d"), 8155, 15),
    ];
    for (err, number, published) in overridden {
        assert_eq!(err.severity, 16, "wire severity of {number}");
        assert_eq!(
            message_template(*number).map(|def| def.severity),
            Some(*published),
            "published severity of {number}"
        );
        assert!(SEVERITY_OVERRIDES.contains(&(*number, 16, *published)));
    }
    for number in [
        103u32, 1038, 1087, 1902, 1909, 1911, 1913, 2702, 4063, 7202, 8110,
    ] {
        assert!(
            !SEVERITY_OVERRIDES.iter().any(|(n, ..)| *n == number),
            "error {number} takes no severity override"
        );
    }
}
