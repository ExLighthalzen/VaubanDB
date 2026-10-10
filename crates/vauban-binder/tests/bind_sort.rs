//! `ORDER BY`, `SELECT DISTINCT` and the `TOP` that rides on them.
//!
//! Each test starts from SQL text, bound against the double below — one table
//! `dbo.t (a int NOT NULL, b int NULL, c varchar(10) NULL)`. The numbers asserted here
//! are those SQL Server answers; the shape of the plan is the one `sort.rs` documents.
//!
//! The bound nodes derive no `PartialEq` (`FunctionDef` has none): a plan is read by
//! pattern matching, an expression by its shape and its `ty`.

use vauban_binder::{
    BindContext, BoundExpr, BoundExprKind, BoundStatement, CatalogView, ColumnBinding, LogicalPlan,
    NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions, SortKey, bind,
};
use vauban_catalog::{ColumnId, ObjectId, TableId};
use vauban_errors::SqlError;
use vauban_parser::{Ident, ObjectName, ParseOptions, parse_batch};
use vauban_sysfn::register_builtins;
use vauban_types::{Len, SqlType, TypeInfo};

/// A catalogue holding `dbo.t (a int NOT NULL, b int NULL, c varchar(10) NULL)`.
struct OneTable;

impl CatalogView for OneTable {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        (name.name.value.eq_ignore_ascii_case("t")).then(|| ResolvedTable {
            object: ObjectId(42),
            table: Some(TableId(7)),
            columns: vec![
                column(1, 0, "a", TypeInfo::new(SqlType::Int, false)),
                column(2, 1, "b", TypeInfo::new(SqlType::Int, true)),
                column(
                    3,
                    2,
                    "c",
                    TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true),
                ),
            ],
            kind: ResolvedTableKind::Table,
        })
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

/// One column of the table above.
fn column(id: i32, index: usize, name: &str, ty: TypeInfo) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(id),
        index,
        name: name.to_owned(),
        ty,
        depth: 0,
    }
}

/// Binds the first statement of `text` against the table above.
fn bound(text: &str) -> Result<BoundStatement, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let catalog = OneTable;
    let variables = NoVariables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &variables,
        options: SessionOptions::default(),
    };
    bind(batch.statements.first().expect("one statement"), &ctx)
}

/// The plan of a statement that binds.
fn plan(text: &str) -> LogicalPlan {
    match bound(text).unwrap_or_else(|e| panic!("{text} binds, got {} {}", e.number, e.message)) {
        BoundStatement::Query(plan) => *plan,
        other => panic!("{text} is a query, got {other:?}"),
    }
}

/// The error a statement that does not bind raises.
fn err(text: &str) -> SqlError {
    bound(text).expect_err(text)
}

/// The name of each operator of a plan, outermost first.
fn stack(plan: &LogicalPlan) -> Vec<&'static str> {
    let mut names = Vec::new();
    let mut node = plan;
    loop {
        let (name, next) = match node {
            LogicalPlan::Limit { input, .. } => ("Limit", Some(input.as_ref())),
            LogicalPlan::Sort { input, .. } => ("Sort", Some(input.as_ref())),
            LogicalPlan::Distinct(input) => ("Distinct", Some(input.as_ref())),
            LogicalPlan::Project { input, .. } => ("Project", Some(input.as_ref())),
            LogicalPlan::Filter { input, .. } => ("Filter", Some(input.as_ref())),
            LogicalPlan::Scan { .. } => ("Scan", None),
            LogicalPlan::OneRow => ("OneRow", None),
            other => panic!("unexpected node {other:?}"),
        };
        names.push(name);
        match next {
            Some(input) => node = input,
            None => return names,
        }
    }
}

/// The keys of the one `Sort` of a plan.
fn keys(plan: &LogicalPlan) -> &[SortKey] {
    let mut node = plan;
    loop {
        node = match node {
            LogicalPlan::Sort { keys, .. } => return keys,
            LogicalPlan::Limit { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Filter { input, .. } => input,
            LogicalPlan::Distinct(input) => input,
            other => panic!("no Sort over {other:?}"),
        };
    }
}

/// The expression each key of the one `Sort` of a plan orders on: the column of the
/// projection under the `Sort` the key designates, or the key itself over a `Distinct`.
fn ordered_on(plan: &LogicalPlan) -> Vec<BoundExpr> {
    let mut node = plan;
    loop {
        node = match node {
            LogicalPlan::Sort { input, keys } => {
                let LogicalPlan::Project { exprs, .. } = input.as_ref() else {
                    return keys.iter().map(|key| key.expr.clone()).collect();
                };
                return keys
                    .iter()
                    .map(|key| match &key.expr.kind {
                        BoundExprKind::ColumnRef(binding) => exprs[binding.index].expr.clone(),
                        other => panic!("a key over a projection is a column, got {other:?}"),
                    })
                    .collect();
            }
            LogicalPlan::Limit { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Filter { input, .. } => input,
            LogicalPlan::Distinct(input) => input,
            other => panic!("no Sort over {other:?}"),
        };
    }
}

/// The projections of the outermost `Project` of a plan, as `(name, ty)` pairs.
fn projections(plan: &LogicalPlan) -> Vec<(String, TypeInfo)> {
    let mut node = plan;
    loop {
        node = match node {
            LogicalPlan::Project { exprs, .. } => {
                return exprs
                    .iter()
                    .map(|p| (p.name.clone(), p.expr.ty.clone()))
                    .collect();
            }
            LogicalPlan::Limit { input, .. } | LogicalPlan::Sort { input, .. } => input,
            LogicalPlan::Distinct(input) => input,
            other => panic!("no Project over {other:?}"),
        };
    }
}

// ---------------------------------------------------------------------------------------
// The shape of the plan
// ---------------------------------------------------------------------------------------

/// The two stacks `sort.rs` documents, and nothing in between.
///
/// With `DISTINCT` the `Sort` is over the `Distinct`; without it, over a projection widened
/// by the hidden columns of its keys, under a last `Project` that keeps the select list.
/// The `Limit` of the `TOP` sits directly over the `Sort` in both, since `TOP` counts the
/// rows of the ordered result.
#[test]
fn the_plan_shape_is_the_documented_one() {
    assert_eq!(
        stack(&plan("SELECT DISTINCT TOP 2 c FROM dbo.t ORDER BY c")),
        ["Limit", "Sort", "Distinct", "Project", "Scan"]
    );
    assert_eq!(
        stack(&plan("SELECT TOP 2 a FROM dbo.t ORDER BY b")),
        ["Project", "Limit", "Sort", "Project", "Scan"]
    );
    assert_eq!(
        stack(&plan("SELECT a FROM dbo.t WHERE b = 1 ORDER BY b")),
        ["Project", "Sort", "Project", "Filter", "Scan"]
    );
    assert_eq!(
        stack(&plan("SELECT DISTINCT c FROM dbo.t")),
        ["Distinct", "Project", "Scan"]
    );
    // Counter-proof: without the clause, neither node is put on.
    assert_eq!(
        stack(&plan("SELECT TOP 2 a FROM dbo.t")),
        ["Limit", "Project", "Scan"]
    );
}

/// A `TOP` keeps its `PERCENT` and its `WITH TIES` across the surgery the `ORDER BY` does
/// on the plan, and the row count stays the bound expression `query::bind_top` produced.
#[test]
fn the_top_survives_the_sort() {
    let percent = plan("SELECT TOP 50 PERCENT a FROM dbo.t ORDER BY a");
    let LogicalPlan::Project { input, .. } = &percent else {
        panic!("the last Project is outermost, got {percent:?}")
    };
    let LogicalPlan::Limit { top, .. } = input.as_ref() else {
        panic!("a TOP is a Limit, got {input:?}")
    };
    assert!(top.percent, "PERCENT is kept");
    assert!(!top.with_ties);
    // The row count is the `Convert` to `bigint` `query.rs` inserts, handed over
    // untouched.
    assert!(matches!(top.expr.kind, BoundExprKind::Convert { .. }));

    let ties = plan("SELECT DISTINCT TOP 1 WITH TIES c FROM dbo.t ORDER BY c");
    let LogicalPlan::Limit { top, .. } = &ties else {
        panic!("a TOP is a Limit, got {ties:?}")
    };
    assert!(top.with_ties, "WITH TIES is kept");
    assert_eq!(
        stack(&ties),
        ["Limit", "Sort", "Distinct", "Project", "Scan"]
    );
}

// ---------------------------------------------------------------------------------------
// What a key is written as
// ---------------------------------------------------------------------------------------

/// `ORDER BY x` over `SELECT c AS x` orders by the expression of that item: the key and the
/// projection are the same column of the same type, which is what the alias resolves to.
#[test]
fn order_by_an_alias_resolves_to_the_select_item() {
    let plan = plan("SELECT c AS x FROM dbo.t ORDER BY x");
    let [key] = ordered_on(&plan).try_into().expect("one key");
    let BoundExprKind::ColumnRef(binding) = &key.kind else {
        panic!("the key is a column, got {:?}", key.kind)
    };
    assert_eq!((binding.name.as_str(), binding.index), ("c", 2));
    let columns = projections(&plan);
    let [(name, ty)] = columns.as_slice() else {
        panic!("one projection")
    };
    assert_eq!(name, "x");
    assert_eq!(ty.ty, key.ty.ty);
    assert_eq!(ty.nullable, key.ty.nullable);

    // The alias wins over the column of the table of the same name: `SELECT a AS b …
    // ORDER BY b` orders by `a` on SQL Server, which answers the rows in the order of `a`
    // and not of `b`.
    let hidden = plan_key_column("SELECT a AS b FROM dbo.t ORDER BY b");
    assert_eq!(hidden, ("a".to_owned(), 0));
    // Counter-proof: a name the select list does not publish binds against the `FROM`.
    assert_eq!(
        plan_key_column("SELECT a FROM dbo.t ORDER BY b"),
        ("b".to_owned(), 1)
    );
    // The case of the name does not matter.
    assert_eq!(
        plan_key_column("SELECT a AS x FROM dbo.t ORDER BY X"),
        ("a".to_owned(), 0)
    );
}

/// The `(name, index)` of the column the one key of a plan reads.
fn plan_key_column(text: &str) -> (String, usize) {
    column_read(text, &ordered_on(&plan(text)))
}

/// The `(name, index)` of the column of the one expression of `keys`.
fn column_read(text: &str, keys: &[BoundExpr]) -> (String, usize) {
    let [key] = keys else {
        panic!("{text}: one key")
    };
    match &key.kind {
        BoundExprKind::ColumnRef(binding) => (binding.name.clone(), binding.index),
        other => panic!("{text}: the key is a column, got {other:?}"),
    }
}

/// `ORDER BY 2` orders by the second output column, 1-based, a `*` counting its expanded
/// columns.
#[test]
fn order_by_an_ordinal_resolves_to_the_nth_item() {
    assert_eq!(
        plan_key_column("SELECT a, b FROM dbo.t ORDER BY 2"),
        ("b".to_owned(), 1)
    );
    assert_eq!(
        plan_key_column("SELECT * FROM dbo.t ORDER BY 3"),
        ("c".to_owned(), 2)
    );
    // The position reads the select list, not the table: `ORDER BY 1` over `SELECT b` is
    // `b`, which is the second column of `dbo.t`.
    assert_eq!(
        plan_key_column("SELECT b FROM dbo.t ORDER BY 1"),
        ("b".to_owned(), 1)
    );
    // A position that names a constant item is not the 408 of a constant key.
    let plan = plan("SELECT 1 AS x, a FROM dbo.t ORDER BY 1");
    let [key] = ordered_on(&plan).try_into().expect("one key");
    assert!(matches!(key.kind, BoundExprKind::Literal(_)));
}

/// A position outside the select list is 108, with the position as written in the message.
#[test]
fn an_ordinal_out_of_range_is_108() {
    for (text, position) in [
        ("SELECT a, b FROM dbo.t ORDER BY 5", "5"),
        ("SELECT a, b FROM dbo.t ORDER BY 0", "0"),
        ("SELECT a, b FROM dbo.t ORDER BY -1", "-1"),
        ("SELECT * FROM dbo.t ORDER BY 4", "4"),
    ] {
        let error = err(text);
        assert_eq!(error.number, 108, "{text}: {}", error.message);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
        assert_eq!(
            error.message,
            format!("ORDER BY position {position} is outside the select list.")
        );
    }
    // Counter-proof: the last position of the list binds.
    assert_eq!(
        plan_key_column("SELECT a, b FROM dbo.t ORDER BY 2"),
        ("b".to_owned(), 1)
    );
    // A position outside the list beats the 145 of a `SELECT DISTINCT`.
    assert_eq!(err("SELECT DISTINCT a FROM dbo.t ORDER BY 5").number, 108);
}

/// A name that matches two output columns is 209, as it is in a `WHERE`.
#[test]
fn an_ambiguous_output_name_is_209() {
    for (text, name) in [
        ("SELECT a AS x, b AS x FROM dbo.t ORDER BY x", "x"),
        ("SELECT a AS b, b FROM dbo.t ORDER BY b", "b"),
    ] {
        let error = err(text);
        assert_eq!(error.number, 209, "{text}: {}", error.message);
        assert_eq!(error.message, format!("Column name '{name}' is ambiguous."));
    }
    // Counter-proof: one item of that name resolves.
    assert_eq!(
        plan_key_column("SELECT a AS x FROM dbo.t ORDER BY x"),
        ("a".to_owned(), 0)
    );
}

/// `ASC` and `DESC` are kept key by key: `ORDER BY a, b DESC` marks the second only.
#[test]
fn desc_is_kept_per_key() {
    let second = plan("SELECT a, b FROM dbo.t ORDER BY a, b DESC");
    let marks: Vec<bool> = keys(&second).iter().map(|key| key.desc).collect();
    assert_eq!(marks, [false, true]);
    let first = plan("SELECT a, b FROM dbo.t ORDER BY a DESC, b ASC");
    let written: Vec<bool> = keys(&first).iter().map(|key| key.desc).collect();
    assert_eq!(written, [true, false]);
}

/// A `COLLATE` on a key is 447 over a number and carries the collation over a string.
#[test]
fn a_collate_on_a_key_follows_the_type_of_the_key() {
    let error = err("SELECT a FROM dbo.t ORDER BY a COLLATE Latin1_General_CI_AS");
    assert_eq!(error.number, 447, "{}", error.message);
    assert_eq!(
        error.message,
        "COLLATE cannot apply to an expression of type int."
    );

    let plan = plan("SELECT c FROM dbo.t ORDER BY c COLLATE Latin1_General_CS_AS");
    let [key] = keys(&plan) else {
        panic!("one key")
    };
    assert!(key.collation.is_some(), "the key carries its collation");
    assert_eq!(key.collation, key.expr.ty.collation);

    // A `COLLATE` written on a key sends the name to the `FROM` and not to the select list:
    // `SELECT c AS x … ORDER BY x COLLATE …` answers 207.
    assert_eq!(
        err("SELECT c AS x FROM dbo.t ORDER BY x COLLATE Latin1_General_CI_AS").number,
        207
    );
}

/// A constant key is 408, with the 1-based position of the key in the message.
#[test]
fn a_constant_key_is_408() {
    for (text, position) in [
        ("SELECT a FROM dbo.t ORDER BY 'abc'", 1),
        ("SELECT a FROM dbo.t ORDER BY NULL", 1),
        ("SELECT a FROM dbo.t ORDER BY 2.0", 1),
        ("SELECT a FROM dbo.t ORDER BY CAST(1 AS int)", 1),
        ("SELECT a FROM dbo.t ORDER BY LEN('abc')", 1),
        ("SELECT a FROM dbo.t ORDER BY a, 'abc'", 2),
    ] {
        let error = err(text);
        assert_eq!(error.number, 408, "{text}: {}", error.message);
        assert_eq!(error.severity, 16);
        assert_eq!(
            error.message,
            format!("ORDER BY item at position {position} is a constant expression.")
        );
    }
    // Counter-proof: a key that reads something at run time is not constant.
    for text in [
        "SELECT a FROM dbo.t ORDER BY @@SPID",
        "SELECT a FROM dbo.t ORDER BY 1 + a",
        "SELECT a FROM dbo.t ORDER BY LEN(c)",
    ] {
        assert!(bound(text).is_ok(), "{text} binds");
    }
}

/// Two keys that designate the same column are 169, whatever each was written as.
#[test]
fn the_same_key_twice_is_169() {
    for text in [
        "SELECT a, b FROM dbo.t ORDER BY a, a",
        "SELECT a, b FROM dbo.t ORDER BY a, dbo.t.a",
        "SELECT a, b FROM dbo.t ORDER BY 1, a",
        "SELECT a, b FROM dbo.t ORDER BY 1, 1",
        "SELECT a, b FROM dbo.t ORDER BY a + 1, a + 1",
        "SELECT a, b FROM dbo.t ORDER BY a ASC, a DESC",
        "SELECT a AS x, b FROM dbo.t ORDER BY x, a",
        "SELECT a,\nb\nFROM dbo.t\nORDER BY a + 1,\na + 1",
    ] {
        let error = err(text);
        assert_eq!(error.number, 169, "{text}: {}", error.message);
        assert_eq!(error.severity, 15);
        assert_eq!(
            error.message,
            "A column appears more than once in the ORDER BY list."
        );
    }
    // Counter-proof: two keys of the same column under different expressions are two keys.
    assert!(bound("SELECT a, b FROM dbo.t ORDER BY a, a + 0").is_ok());
    assert!(bound("SELECT a, b FROM dbo.t ORDER BY a, b").is_ok());
    // 169 comes before 145.
    assert_eq!(
        err("SELECT DISTINCT a FROM dbo.t ORDER BY b, b").number,
        169
    );
}

// ---------------------------------------------------------------------------------------
// `SELECT DISTINCT`
// ---------------------------------------------------------------------------------------

/// A key outside the select list of a `SELECT DISTINCT` is 145, and the same statement
/// without the word binds.
#[test]
fn order_by_outside_the_select_list_with_distinct_is_145() {
    for text in [
        "SELECT DISTINCT a FROM dbo.t ORDER BY b",
        "SELECT DISTINCT a FROM dbo.t ORDER BY b + 1",
        "SELECT DISTINCT a + 1 FROM dbo.t ORDER BY a",
        "SELECT DISTINCT a FROM dbo.t ORDER BY a + 1",
        "SELECT DISTINCT a + 1 FROM dbo.t ORDER BY 1 + a",
        "SELECT DISTINCT a FROM dbo.t ORDER BY a, b",
    ] {
        let error = err(text);
        assert_eq!(error.number, 145, "{text}: {}", error.message);
        assert_eq!(error.severity, 15);
    }
    // The counter-proof: the same keys without `DISTINCT` bind.
    for text in [
        "SELECT a FROM dbo.t ORDER BY b",
        "SELECT a FROM dbo.t ORDER BY b + 1",
        "SELECT a + 1 FROM dbo.t ORDER BY a",
        "SELECT a FROM dbo.t ORDER BY a + 1",
        "SELECT a FROM dbo.t ORDER BY a, b",
    ] {
        assert!(bound(text).is_ok(), "{text} binds without DISTINCT");
    }
    // And the keys the select list does publish bind with it: the position, the alias, the
    // same expression, and the column written with a qualifier on one side alone.
    for text in [
        "SELECT DISTINCT a FROM dbo.t ORDER BY 1",
        "SELECT DISTINCT a + 1 AS x FROM dbo.t ORDER BY x",
        "SELECT DISTINCT a + 1 FROM dbo.t ORDER BY a + 1",
        "SELECT DISTINCT a FROM dbo.t ORDER BY dbo.t.a",
        "SELECT DISTINCT dbo.t.a FROM dbo.t ORDER BY a",
    ] {
        assert!(bound(text).is_ok(), "{text} binds");
    }
    // A key that resolves to nothing answers its own error before 145.
    assert_eq!(err("SELECT DISTINCT a FROM dbo.t ORDER BY z").number, 207);
}

/// Over a `DISTINCT`, a key reads the row the deduplication publishes: it is the column at
/// the position the key has in the select list, with the type of that output column.
#[test]
fn a_distinct_key_reads_the_projected_row() {
    let plan = plan("SELECT DISTINCT b, c FROM dbo.t ORDER BY c DESC");
    let [key] = keys(&plan) else {
        panic!("one key")
    };
    let BoundExprKind::ColumnRef(binding) = &key.expr.kind else {
        panic!(
            "the key is a column of the projected row, got {:?}",
            key.expr.kind
        )
    };
    // `c` is the **second** output column here and the third column of `dbo.t`: the index
    // is the one of the row the `Distinct` publishes.
    assert_eq!(binding.index, 1);
    assert_eq!(binding.name, "c");
    assert_eq!(key.expr.ty.ty, SqlType::VarChar(Len::Fixed(10)));
    assert!(key.desc);
    // Without `DISTINCT` the same key reads the row of the `Scan`, where `c` is third.
    assert_eq!(
        plan_key_column("SELECT b, c FROM dbo.t ORDER BY c DESC"),
        ("c".to_owned(), 2)
    );
}

// ---------------------------------------------------------------------------------------
// What the clause refuses
// ---------------------------------------------------------------------------------------

/// `TOP … WITH TIES` without an `ORDER BY` is 1062, through its constructor.
#[test]
fn with_ties_without_order_by_is_1062() {
    for text in [
        "SELECT TOP 2 WITH TIES a FROM dbo.t",
        "SELECT DISTINCT TOP 2 WITH TIES a FROM dbo.t",
    ] {
        let error = err(text);
        assert_eq!(error.number, 1062, "{text}: {}", error.message);
        assert_eq!(error.severity, 15);
    }
    // Counter-proof: with an `ORDER BY`, the same `TOP` binds.
    assert!(bound("SELECT TOP 2 WITH TIES a FROM dbo.t ORDER BY a").is_ok());
    // A `TOP` whose row count is negative binds — the 127 SQL Server answers to
    // `SELECT TOP (-1) a FROM dbo.t ORDER BY a` is raised when the count is evaluated.
    assert!(bound("SELECT TOP (-1) a FROM dbo.t ORDER BY a").is_ok());
}

/// `OFFSET … FETCH` stays an internal refusal, `ORDER BY` or not.
#[test]
fn offset_fetch_is_refused() {
    let error = err("SELECT a FROM dbo.t ORDER BY a OFFSET 1 ROWS FETCH NEXT 1 ROWS ONLY");
    assert_eq!(error.number, 50000, "{}", error.message);
    assert!(error.message.contains("OFFSET"), "{}", error.message);
    assert!(
        error.message.contains("not implemented yet"),
        "{}",
        error.message
    );
}

/// A `SELECT` without a `FROM` orders by a position or by an alias, and the plan keeps the
/// `Sort` between the two projections there too.
#[test]
fn a_select_without_a_from_orders_by_a_position() {
    assert_eq!(
        stack(&plan("SELECT 1 ORDER BY 1")),
        ["Project", "Sort", "Project", "OneRow"]
    );
    assert!(bound("SELECT 1 AS x ORDER BY x").is_ok());
    assert_eq!(err("SELECT 1 ORDER BY 2").number, 108);
    assert_eq!(err("SELECT 1 ORDER BY 'a'").number, 408);
}

// ---------------------------------------------------------------------------------------
// The edges of a position, of a `COLLATE` on a key, and of the fingerprint of a key
// ---------------------------------------------------------------------------------------

/// A sign and parentheses around an integer literal leave a position.
#[test]
fn a_position_survives_its_signs_and_its_parentheses() {
    for written in ["+2", "(2)", "((2))", "+(2)", "(+2)"] {
        assert_eq!(
            plan_key_column(&format!("SELECT a, b FROM dbo.t ORDER BY {written}")),
            ("b".to_owned(), 1),
            "ORDER BY {written} is the 2nd item"
        );
    }
    for (written, named) in [("+5", "5"), ("(5)", "5"), ("-(1)", "-1"), ("(-1)", "-1")] {
        let error = err(&format!("SELECT a, b FROM dbo.t ORDER BY {written}"));
        assert_eq!(error.number, 108, "{written}: {}", error.message);
        assert_eq!(
            error.message,
            format!("ORDER BY position {named} is outside the select list.")
        );
    }
    // A position written that way is still the column 169 counts.
    assert_eq!(err("SELECT a, b FROM dbo.t ORDER BY b, (2)").number, 169);
    assert_eq!(err("SELECT a, b FROM dbo.t ORDER BY 1, (1)").number, 169);
    // Counter-proof: what the parentheses hold is a literal and its sign, not an
    // expression.
    assert_eq!(err("SELECT a, b FROM dbo.t ORDER BY (1 + 1)").number, 408);
    assert_eq!(err("SELECT a, b FROM dbo.t ORDER BY (2.0)").number, 408);
}

/// A position is an `int`: past what `int` holds, the key is a constant and answers 408.
#[test]
fn a_position_stops_at_what_int_holds() {
    for written in ["2147483647", "-2147483648"] {
        let error = err(&format!("SELECT a, b FROM dbo.t ORDER BY {written}"));
        assert_eq!(error.number, 108, "{written}: {}", error.message);
        assert_eq!(
            error.message,
            format!("ORDER BY position {written} is outside the select list.")
        );
    }
    for written in ["2147483648", "-2147483649", "99999999999999999999"] {
        let error = err(&format!("SELECT a, b FROM dbo.t ORDER BY {written}"));
        assert_eq!(error.number, 408, "{written}: {}", error.message);
        assert_eq!(
            error.message,
            "ORDER BY item at position 1 is a constant expression."
        );
    }
}

/// A `COLLATE` on an item stops the position from being read: the key is the `int` literal
/// itself, and 447 names its type.
#[test]
fn a_collate_on_a_position_is_447() {
    for text in [
        // The 1st item is a `varchar`, and 447 still says `int`.
        "SELECT c FROM dbo.t ORDER BY 1 COLLATE Latin1_General_CI_AS",
        "SELECT a FROM dbo.t ORDER BY 1 COLLATE Latin1_General_CI_AS",
        "SELECT c FROM dbo.t ORDER BY (1) COLLATE Latin1_General_CI_AS",
        // Before 108: the position is not even checked.
        "SELECT c FROM dbo.t ORDER BY 5 COLLATE Latin1_General_CI_AS",
        // And before 448: the type is read before the collation name.
        "SELECT c FROM dbo.t ORDER BY 1 COLLATE Not_A_Collation",
    ] {
        let error = err(text);
        assert_eq!(error.number, 447, "{text}: {}", error.message);
        assert_eq!(
            error.message,
            "COLLATE cannot apply to an expression of type int."
        );
    }
    // Counter-proof: a key whose type is a string goes on to the 408 of a constant, and a
    // `decimal` one answers 447 naming its type.
    assert_eq!(
        err("SELECT c FROM dbo.t ORDER BY 'abc' COLLATE Latin1_General_CI_AS").number,
        408
    );
    let decimal = err("SELECT c FROM dbo.t ORDER BY 2.0 COLLATE Latin1_General_CI_AS");
    assert_eq!(decimal.number, 447, "{}", decimal.message);
    assert_eq!(
        decimal.message,
        "COLLATE cannot apply to an expression of type numeric."
    );
}

/// The `COLLATE` of a key is part of the expression 145 looks for in the select list, and
/// part of what makes two keys the same column for 169.
#[test]
fn a_collate_is_part_of_the_key_it_is_written_on() {
    // The clause on the key alone: the select list publishes `c`, the key is
    // `c COLLATE X`.
    assert_eq!(
        err("SELECT DISTINCT c FROM dbo.t ORDER BY c COLLATE Latin1_General_CI_AS").number,
        145
    );
    // The mirror binds: both sides carry the same clause.
    assert!(
        bound(
            "SELECT DISTINCT c COLLATE Latin1_General_CI_AS FROM dbo.t \
             ORDER BY c COLLATE Latin1_General_CI_AS"
        )
        .is_ok(),
        "both sides collated the same way bind"
    );
    // Two collations are two expressions, and the clause on the item alone is 145 too.
    assert_eq!(
        err("SELECT DISTINCT c COLLATE Latin1_General_CI_AS FROM dbo.t \
             ORDER BY c COLLATE Latin1_General_CS_AS")
        .number,
        145
    );
    assert_eq!(
        err("SELECT DISTINCT c COLLATE Latin1_General_CI_AS FROM dbo.t ORDER BY c").number,
        145
    );
    // 169 reads the same difference: the collated key and the bare one are two keys, two
    // collated ones are one.
    assert!(
        bound("SELECT c FROM dbo.t ORDER BY c COLLATE Latin1_General_CI_AS, c").is_ok(),
        "a collated key and a bare one are two keys"
    );
    assert_eq!(
        err(
            "SELECT c FROM dbo.t ORDER BY c COLLATE Latin1_General_CI_AS, \
             c COLLATE Latin1_General_CI_AS"
        )
        .number,
        169
    );
}

/// 145 hangs on the word `DISTINCT` of the statement, which a `SELECT` without a `FROM`
/// writes without any `Distinct` node being put on.
#[test]
fn a_distinct_without_a_from_still_answers_145() {
    for text in [
        "SELECT DISTINCT 1 ORDER BY @@SPID",
        "SELECT DISTINCT 1 ORDER BY GETDATE()",
    ] {
        let error = err(text);
        assert_eq!(error.number, 145, "{text}: {}", error.message);
    }
    // Counter-proof: the same keys without the word bind.
    assert!(bound("SELECT 1 ORDER BY @@SPID").is_ok());
    assert!(bound("SELECT 1 ORDER BY GETDATE()").is_ok());
    // And the keys the select list publishes bind with it, where a position outside it
    // answers 108.
    assert!(bound("SELECT DISTINCT 1 ORDER BY 1").is_ok());
    assert!(bound("SELECT DISTINCT 1 AS x ORDER BY x").is_ok());
    assert_eq!(err("SELECT DISTINCT 1 ORDER BY 2").number, 108);
    // The plan is the one of a `SELECT` without a `FROM`: `query.rs` puts no `Distinct` on.
    assert_eq!(
        stack(&plan("SELECT DISTINCT 1 ORDER BY 1")),
        ["Project", "Sort", "Project", "OneRow"]
    );
}

/// The words `line: 1` inside a literal are not the line a node was written on: two keys
/// that differ by that literal are two keys.
#[test]
fn a_literal_holding_the_word_line_is_not_the_line_of_a_node() {
    assert!(
        bound("SELECT c FROM dbo.t ORDER BY c + 'line: 1', c + 'line: 2'").is_ok(),
        "two literals, two keys (case `two_keys_holding_the_words_line_one_and_two`)"
    );
    // Counter-proof: the same literal on both keys is one key.
    assert_eq!(
        err("SELECT c FROM dbo.t ORDER BY c + 'line: 1', c + 'line: 1'").number,
        169
    );
    // And the select list of a `SELECT DISTINCT` is read the same way.
    assert_eq!(
        err("SELECT DISTINCT c + 'line: 1' FROM dbo.t ORDER BY c + 'line: 2'").number,
        145
    );
    assert!(bound("SELECT DISTINCT c + 'line: 1' FROM dbo.t ORDER BY c + 'line: 1'").is_ok());
    // The line itself still leaves one key when the same expression is split over two
    // lines, which is what the blanking is for.
    assert_eq!(
        err("SELECT c\nFROM dbo.t\nORDER BY c + 'x',\nc + 'x'").number,
        169
    );
}

/// What the shape of the plan changes: a key written as an alias orders on the column the
/// projection computes, so the plan holds one node for it, the value published being the
/// value ordered on. SQL Server answers that form its rows in the order of the published
/// value.
#[test]
fn a_key_written_as_an_alias_orders_on_its_column() {
    let text = "SELECT CAST(NEWID() AS varchar(36)) AS x FROM dbo.t ORDER BY x";
    let alias = plan(text);
    assert_eq!(stack(&alias), ["Project", "Sort", "Project", "Scan"]);
    let LogicalPlan::Project { input, .. } = &alias else {
        panic!("the last Project is outermost, got {alias:?}")
    };
    let LogicalPlan::Sort { keys, input } = input.as_ref() else {
        panic!("the Sort is under it, got {input:?}")
    };
    let LogicalPlan::Project { exprs, .. } = input.as_ref() else {
        panic!("the projection is under the Sort, got {input:?}")
    };
    // One `Convert`, computed once: the key adds no hidden column and reads column 0.
    assert_eq!(exprs.len(), 1, "no hidden column: {exprs:?}");
    assert!(matches!(exprs[0].expr.kind, BoundExprKind::Convert { .. }));
    assert!(
        matches!(&keys[0].expr.kind, BoundExprKind::ColumnRef(binding) if binding.index == 0),
        "the key reads the column of its item, got {:?}",
        keys[0].expr.kind
    );
    // Counter-proof: the same call written as an expression key is a hidden column of
    // its own, a second draw.
    let written = plan("SELECT NEWID() AS x FROM dbo.t ORDER BY NEWID()");
    let LogicalPlan::Project { input, schema, .. } = &written else {
        panic!("the last Project is outermost, got {written:?}")
    };
    assert_eq!(schema.columns.len(), 1, "the hidden column is not returned");
    let LogicalPlan::Sort { input, .. } = input.as_ref() else {
        panic!("the Sort is under it, got {input:?}")
    };
    let LogicalPlan::Project { exprs, .. } = input.as_ref() else {
        panic!("the projection is under the Sort, got {input:?}")
    };
    assert_eq!(exprs.len(), 2, "one hidden column: {exprs:?}");
}

/// A key the select list does not return is a hidden column of the projection under the
/// `Sort`, which the last `Project` drops: the client sees the columns of the select list.
#[test]
fn a_key_outside_the_select_list_is_a_hidden_column() {
    let hidden = plan("SELECT a FROM dbo.t ORDER BY b DESC, c");
    let LogicalPlan::Project {
        exprs,
        schema,
        input,
    } = &hidden
    else {
        panic!("the last Project is outermost, got {hidden:?}")
    };
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a"]);
    assert!(
        matches!(&exprs[0].expr.kind, BoundExprKind::ColumnRef(binding) if binding.index == 0),
        "the last Project reads column 0, got {:?}",
        exprs[0].expr.kind
    );
    let LogicalPlan::Sort { keys, input } = input.as_ref() else {
        panic!("the Sort is under it, got {input:?}")
    };
    let LogicalPlan::Project { exprs, .. } = input.as_ref() else {
        panic!("the widened projection is under the Sort, got {input:?}")
    };
    let widened: Vec<&str> = exprs
        .iter()
        .map(|p| match &p.expr.kind {
            BoundExprKind::ColumnRef(binding) => binding.name.as_str(),
            other => panic!("a column, got {other:?}"),
        })
        .collect();
    assert_eq!(widened, ["a", "b", "c"]);
    let read: Vec<(usize, bool)> = keys
        .iter()
        .map(|key| match &key.expr.kind {
            BoundExprKind::ColumnRef(binding) => (binding.index, key.desc),
            other => panic!("a column, got {other:?}"),
        })
        .collect();
    assert_eq!(read, [(1, true), (2, false)]);
}

/// The keys are taken in written order, each one checked against the keys before it
/// before the next is resolved: 169 on the second `c` comes before the 207 or the 108 of
/// a third key, and after the 207 of a first one.
#[test]
fn keys_are_checked_in_written_order() {
    assert_eq!(err("SELECT c FROM dbo.t ORDER BY c, c, nosuch").number, 169);
    assert_eq!(err("SELECT c FROM dbo.t ORDER BY c, c, 5").number, 169);
    assert_eq!(err("SELECT c FROM dbo.t ORDER BY nosuch, c, c").number, 207);
    assert_eq!(err("SELECT c FROM dbo.t ORDER BY 5, c, c").number, 108);
    // Over a grouped query, the 8127 of a key comes in its turn too.
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY b, 5").number,
        8127
    );
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY 5, b").number,
        108
    );
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY c, c, b").number,
        169
    );
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY b, c, c").number,
        8127
    );
}

// ---------------------------------------------------------------------------------------
// `ORDER BY` over a grouped query
// ---------------------------------------------------------------------------------------

/// The `Aggregate` under a grouped `ORDER BY`, and the `Sort` over it.
fn grouped(plan: &LogicalPlan) -> (&[SortKey], &[vauban_binder::BoundProjection], usize) {
    let LogicalPlan::Project { input, .. } = plan else {
        panic!("the last Project is outermost, got {plan:?}")
    };
    let mut node = input.as_ref();
    while let LogicalPlan::Limit { input, .. } = node {
        node = input;
    }
    let LogicalPlan::Sort { keys, input } = node else {
        panic!("the Sort is under the last Project, got {node:?}")
    };
    let LogicalPlan::Project { exprs, input, .. } = input.as_ref() else {
        panic!("the widened projection is under the Sort, got {input:?}")
    };
    let mut node = input.as_ref();
    while let LogicalPlan::Filter { input, .. } = node {
        node = input;
    }
    let LogicalPlan::Aggregate { aggregates, .. } = node else {
        panic!("the Aggregate is under the projection, got {node:?}")
    };
    (keys, exprs, aggregates.len())
}

/// `ORDER BY COUNT(*)` over a query that does not return it: the `Aggregate` computes the
/// call, a hidden column reads it, and the client sees the select list alone.
#[test]
fn a_grouped_query_orders_on_an_aggregate_it_does_not_return() {
    let unreturned = plan("SELECT c FROM dbo.t GROUP BY c ORDER BY COUNT(*)");
    assert_eq!(unreturned.schema().columns.len(), 1);
    let (keys, exprs, aggregates) = grouped(&unreturned);
    assert_eq!(aggregates, 1, "the ORDER BY adds its aggregate to the node");
    assert_eq!(exprs.len(), 2, "one hidden column");
    assert!(
        matches!(&exprs[1].expr.kind, BoundExprKind::ColumnRef(binding) if binding.index == 1),
        "the hidden column reads the aggregate, column 1 of the node, got {:?}",
        exprs[1].expr.kind
    );
    assert!(matches!(&keys[0].expr.kind, BoundExprKind::ColumnRef(binding) if binding.index == 1));

    // An aggregate the select list returns is read through its item, by an alias, a
    // position or the call written again, and is computed once.
    for text in [
        "SELECT c, COUNT(*) AS n FROM dbo.t GROUP BY c ORDER BY n DESC, c",
        "SELECT c, COUNT(*) AS n FROM dbo.t GROUP BY c ORDER BY 2 DESC, 1",
    ] {
        let returned = plan(text);
        let (keys, exprs, aggregates) = grouped(&returned);
        assert_eq!(
            (aggregates, exprs.len()),
            (1, 2),
            "{text}: no hidden column"
        );
        let read: Vec<(usize, bool)> = keys
            .iter()
            .map(|key| match &key.expr.kind {
                BoundExprKind::ColumnRef(binding) => (binding.index, key.desc),
                other => panic!("{text}: a column, got {other:?}"),
            })
            .collect();
        assert_eq!(read, [(1, true), (0, false)], "{text}");
    }
    let again = plan("SELECT c, COUNT(*) AS n FROM dbo.t GROUP BY c ORDER BY COUNT(*) DESC");
    let (_, exprs, aggregates) = grouped(&again);
    assert_eq!(aggregates, 1, "COUNT(*) written twice is one aggregate");
    assert_eq!(
        exprs.len(),
        3,
        "the call written as an expression is a hidden column"
    );

    // A key, an expression on a key, a `HAVING` and a `TOP` bind as well.
    for text in [
        "SELECT c FROM dbo.t GROUP BY c ORDER BY c",
        "SELECT c FROM dbo.t GROUP BY c ORDER BY LEN(c) DESC",
        "SELECT c FROM dbo.t GROUP BY c HAVING COUNT(*) > 1 ORDER BY MAX(b)",
        "SELECT TOP 1 WITH TIES c FROM dbo.t GROUP BY c ORDER BY COUNT(*) DESC",
        "SELECT COUNT(*) FROM dbo.t ORDER BY COUNT(*)",
        "SELECT 1 FROM dbo.t ORDER BY COUNT(*)",
    ] {
        assert!(bound(text).is_ok(), "{text} binds: {:?}", bound(text).err());
    }
}

/// A column of the `ORDER BY` of a grouped query that is neither a key nor aggregated is
/// 8127, on the line of the column, naming the table as written.
#[test]
fn a_column_outside_the_group_by_in_the_order_by_is_8127() {
    for text in [
        "SELECT c FROM dbo.t GROUP BY c ORDER BY b",
        "SELECT c FROM dbo.t GROUP BY c ORDER BY b + 1",
        "SELECT c FROM dbo.t AS q GROUP BY c ORDER BY q.b",
        "SELECT DISTINCT c FROM dbo.t GROUP BY c ORDER BY b",
    ] {
        let error = err(text);
        assert_eq!(error.number, 8127, "{text}: {}", error.message);
        assert_eq!((error.severity, error.state), (16, 1));
        assert!(
            error.message.contains("'dbo.t.b'"),
            "{text}: {}",
            error.message
        );
    }
    assert_eq!(err("SELECT c\nFROM dbo.t\nGROUP BY c\nORDER BY\nb").line, 5);
    // The clauses before it answer first: the select list, the `HAVING`.
    assert_eq!(
        err("SELECT c, b FROM dbo.t GROUP BY c ORDER BY nosuch").number,
        8120
    );
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c HAVING b > 1 ORDER BY nosuch").number,
        8121
    );
    // An aggregate in the `ORDER BY` alone groups the query: 8120 on the select list.
    assert_eq!(err("SELECT c FROM dbo.t ORDER BY COUNT(*)").number, 8120);
    // 145 over a `SELECT DISTINCT` whose list does not return the aggregate, where the
    // list that returns it binds.
    assert_eq!(
        err("SELECT DISTINCT c FROM dbo.t GROUP BY c ORDER BY COUNT(*)").number,
        145
    );
    assert!(bound("SELECT DISTINCT c, COUNT(*) FROM dbo.t GROUP BY c ORDER BY COUNT(*)").is_ok());
    // A key that resolves to nothing, an aggregate of an aggregate and a constant.
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY nosuch").number,
        207
    );
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY SUM(COUNT(*))").number,
        130
    );
    assert_eq!(
        err("SELECT c FROM dbo.t GROUP BY c ORDER BY 'a'").number,
        408
    );
    assert_eq!(err("SELECT c FROM dbo.t GROUP BY c ORDER BY 3").number, 108);
    assert_eq!(
        err("SELECT c, COUNT(*) FROM dbo.t GROUP BY c ORDER BY 2, COUNT(*)").number,
        169
    );
    // A key written as an expression is looked up in the `FROM`, as in a query that
    // groups nothing.
    assert_eq!(
        err("SELECT c AS k FROM dbo.t GROUP BY c ORDER BY k + 1").number,
        207
    );
}

// ---------------------------------------------------------------------------------------
// `ORDER BY` over a join or a derived table
// ---------------------------------------------------------------------------------------

/// Three tables in `master.dbo`: `a (k, c)`, `b (k, c)` and `d (k, e)`.
struct ThreeTables;

impl CatalogView for ThreeTables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        database: &str,
        default_schema: &str,
    ) -> Option<ResolvedTable> {
        if name.server.is_some() {
            return None;
        }
        let part = |ident: Option<&Ident>, default: &str| {
            ident.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
        };
        if !part(name.database.as_ref(), database).eq_ignore_ascii_case("master")
            || !part(name.schema.as_ref(), default_schema).eq_ignore_ascii_case("dbo")
        {
            return None;
        }
        let (object, second) = match name.name.value.to_ascii_lowercase().as_str() {
            "a" => (1, "c"),
            "b" => (2, "c"),
            "d" => (3, "e"),
            _ => return None,
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns: vec![
                column(object * 10 + 1, 0, "k", TypeInfo::new(SqlType::Int, false)),
                column(
                    object * 10 + 2,
                    1,
                    second,
                    TypeInfo::new(SqlType::Int, true),
                ),
            ],
            kind: ResolvedTableKind::Table,
        })
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

/// Two tables: `a (k, c)` and `b (k, c)`.
struct TwoTables;

impl CatalogView for TwoTables {
    fn resolve_table(
        &self,
        name: &ObjectName,
        _database: &str,
        _default_schema: &str,
    ) -> Option<ResolvedTable> {
        let object = match name.name.value.as_str() {
            "a" | "A" => 1,
            "b" | "B" => 2,
            _ => return None,
        };
        Some(ResolvedTable {
            object: ObjectId(object),
            table: Some(TableId(u32::try_from(object).expect("a small identifier"))),
            columns: vec![
                column(1, 0, "k", TypeInfo::new(SqlType::Int, false)),
                column(2, 1, "c", TypeInfo::new(SqlType::Int, true)),
            ],
            kind: ResolvedTableKind::Table,
        })
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

/// Binds against `ThreeTables`.
fn bound_join(text: &str) -> Result<BoundStatement, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let catalog = ThreeTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    bind(batch.statements.first().expect("one statement"), &ctx)
}

fn plan_join(text: &str) -> LogicalPlan {
    match bound_join(text)
        .unwrap_or_else(|e| panic!("{text} binds, got {} {}", e.number, e.message))
    {
        BoundStatement::Query(plan) => *plan,
        other => panic!("{text} is a query, got {other:?}"),
    }
}

fn err_join(text: &str) -> SqlError {
    bound_join(text).expect_err(text)
}

/// Binds against `TwoTables`.
fn bound_derived(text: &str) -> Result<BoundStatement, SqlError> {
    register_builtins();
    let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
    let catalog = TwoTables;
    let ctx = BindContext {
        text,
        catalog: Some(&catalog),
        database: "master",
        default_schema: "dbo",
        variables: &NoVariables,
        options: SessionOptions::default(),
    };
    bind(batch.statements.first().expect("one statement"), &ctx)
}

fn plan_derived(text: &str) -> LogicalPlan {
    match bound_derived(text)
        .unwrap_or_else(|e| panic!("{text} binds, got {} {}", e.number, e.message))
    {
        BoundStatement::Query(plan) => *plan,
        other => panic!("{text} is a query, got {other:?}"),
    }
}

fn err_derived(text: &str) -> SqlError {
    bound_derived(text).expect_err(text)
}

/// The `(name, index)` of the column the one key of a join plan reads.
fn join_key(text: &str) -> (String, usize) {
    column_read(text, &ordered_on(&plan_join(text)))
}

/// The `(name, index)` of the column the one key of a derived-table plan reads.
fn derived_key(text: &str) -> (String, usize) {
    column_read(text, &ordered_on(&plan_derived(text)))
}

/// The operator stack through a `Join` or `Subquery` leaf.
fn stack_relational(plan: &LogicalPlan) -> Vec<&'static str> {
    let mut names = Vec::new();
    let mut node = plan;
    loop {
        let (name, next) = match node {
            LogicalPlan::Limit { input, .. } => ("Limit", Some(input.as_ref())),
            LogicalPlan::Sort { input, .. } => ("Sort", Some(input.as_ref())),
            LogicalPlan::Distinct(input) => ("Distinct", Some(input.as_ref())),
            LogicalPlan::Project { input, .. } => ("Project", Some(input.as_ref())),
            LogicalPlan::Filter { input, .. } => ("Filter", Some(input.as_ref())),
            LogicalPlan::Join { .. } => ("Join", None),
            LogicalPlan::Subquery { .. } => ("Subquery", None),
            LogicalPlan::Scan { .. } => ("Scan", None),
            LogicalPlan::OneRow => ("OneRow", None),
            other => panic!("unexpected node {other:?}"),
        };
        names.push(name);
        match next {
            Some(input) => node = input,
            None => return names,
        }
    }
}

/// A join `ORDER BY` keeps the documented stack: `Sort` between the two projections, over
/// the `Join`.
#[test]
fn order_by_over_a_join_keeps_the_documented_stack() {
    assert_eq!(
        stack_relational(&plan_join(
            "SELECT a.k, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY b.c"
        )),
        ["Project", "Sort", "Project", "Join"]
    );
    assert_eq!(
        stack_relational(&plan_derived(
            "SELECT d.c FROM (SELECT c FROM a) AS d ORDER BY d.c"
        )),
        ["Project", "Sort", "Project", "Subquery"]
    );
}

/// Each side of a join may supply a sort key.
#[test]
fn order_by_either_side_of_a_join() {
    assert_eq!(
        join_key("SELECT a.k, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY b.c"),
        ("c".to_owned(), 3)
    );
    assert_eq!(
        join_key("SELECT a.k, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY a.k"),
        ("k".to_owned(), 0)
    );
}

/// A bare name two joined tables carry is 209 in the sort list.
#[test]
fn an_ambiguous_join_column_in_order_by_is_209() {
    let error = err_join("SELECT a.k FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY c");
    assert_eq!(error.number, 209, "{}", error.message);
    assert_eq!(error.message, "Column name 'c' is ambiguous.");
}

/// A table alias qualifies a sort key on a join.
#[test]
fn order_by_a_qualified_table_alias_on_a_join() {
    assert_eq!(
        join_key("SELECT a.k FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY b.k"),
        ("k".to_owned(), 2)
    );
}

/// Under a `LEFT JOIN`, the right side is nullable and its columns sort from that side.
#[test]
fn order_by_the_right_side_of_a_left_join() {
    assert_eq!(
        join_key("SELECT a.k FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k ORDER BY b.c"),
        ("c".to_owned(), 3)
    );
    let plan = plan_join("SELECT b.c FROM dbo.a LEFT JOIN dbo.b ON a.k = b.k ORDER BY b.c");
    let [key] = ordered_on(&plan).try_into().expect("one key");
    assert!(key.ty.nullable, "b.c is nullable under a LEFT JOIN");
}

/// Three joined tables publish six columns; a sort key may name one of them.
#[test]
fn order_by_over_three_tables() {
    assert_eq!(
        join_key(
            "SELECT 1 FROM dbo.a JOIN dbo.b ON a.k = b.k JOIN dbo.d ON a.k = d.k ORDER BY d.e"
        ),
        ("e".to_owned(), 5)
    );
}

/// A derived table alias qualifies its published columns in the sort list.
#[test]
fn order_by_over_a_derived_table() {
    assert_eq!(
        derived_key("SELECT d.c FROM (SELECT c FROM a) AS d ORDER BY d.c"),
        ("c".to_owned(), 0)
    );
}

/// A derived table joined to a table publishes its columns in the sort scope of the join.
#[test]
fn order_by_a_derived_table_column_in_a_join() {
    assert_eq!(
        join_key(
            "SELECT a.k, d.c FROM dbo.a JOIN (SELECT k, c FROM dbo.b) AS d ON a.k = d.k ORDER BY d.c"
        ),
        ("c".to_owned(), 3)
    );
}

/// A column the derived table does not publish is 207 in the sort list.
#[test]
fn order_by_a_column_the_derived_table_does_not_publish_is_207() {
    let error = err_derived("SELECT d.c FROM (SELECT c FROM a) AS d ORDER BY d.k");
    assert_eq!(error.number, 207, "{}", error.message);
    assert_eq!(error.message, "Unknown column name 'k'.");
}

/// `ORDER BY 2` over a join reads the second item of the select list.
#[test]
fn order_by_an_ordinal_over_a_join() {
    assert_eq!(
        join_key("SELECT a.k, b.c FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY 2"),
        ("c".to_owned(), 3)
    );
    assert_eq!(
        err_join("SELECT a.k FROM dbo.a JOIN dbo.b ON a.k = b.k ORDER BY 99").number,
        108
    );
}
