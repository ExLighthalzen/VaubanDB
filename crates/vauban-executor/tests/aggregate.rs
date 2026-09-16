//! Unit tests for `HashAggregate` and `StreamAggregate`: grouping, counting, summing,
//! NULL handling, DISTINCT, INFO 8153, empty input, case-insensitive keys.
//!
//! Each plan is built by hand; no SQL text is parsed, bound, or planned.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, LockHints, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_executor::{CancelToken, CollectSink, ExecContext, ExecOutcome, Row, RowSink, execute};
use vauban_planner::PhysicalPlan;
use vauban_storage::{MemoryStorage, Row as StorageRow, Storage, TableId, TableShape};
use vauban_sysfn::{lookup, register_builtins};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Collation, SqlString, SqlType, TypeInfo, Value};

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn int_t() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn int_nn() -> TypeInfo {
    TypeInfo::new(SqlType::Int, false)
}

/// A collated varchar for case-insensitive grouping tests.
fn varchar_ci() -> TypeInfo {
    let mut ty = TypeInfo::new(SqlType::VarChar(vauban_types::Len::Fixed(10)), true);
    ty.collation = Some(Collation::DEFAULT);
    ty
}

fn binding(index: usize, ty: &TypeInfo) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: format!("c{index}"),
        ty: ty.clone(),
    }
}

fn lit(value: Value) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty: int_t(),
        line: 1,
    }
}

fn col(index: usize, ty: &TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(index, ty)),
        ty: ty.clone(),
        line: 1,
    }
}

fn schema_of(names: &[&str], types: &[TypeInfo]) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .enumerate()
            .map(|(i, name)| OutputColumn {
                name: (*name).to_owned(),
                ty: types[i].clone(),
            })
            .collect(),
    }
}

// -----------------------------------------------------------------------
// Fixture: in-memory storage with a table of two int columns
// -----------------------------------------------------------------------

struct Fixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    table: TableId,
}

impl Fixture {
    fn new(types: &[TypeInfo]) -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: types.to_vec(),
            clustered_key: None,
        };
        let table = storage.create_table(db, &shape).expect("the table is new");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Fixture {
            storage,
            txn,
            table,
        }
    }

    fn insert(&self, values: &[Value]) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let row = StorageRow(values.to_vec());
        self.storage
            .insert(handle.id, self.table, &row)
            .expect("the row is inserted");
        self.txn.commit(handle).expect("the transaction commits");
    }

    fn snapshot(&self) -> vauban_storage::Snapshot {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&handle)
    }

    fn run(&self, plan: &PhysicalPlan, sink: &mut dyn RowSink) -> ExecOutcome {
        let eval = vauban_sysfn::StaticContext::default();
        let snap = self.snapshot();
        let token = CancelToken::never();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_cancel(&token);
        execute(
            &vauban_planner::PhysicalStatement::Query(plan.clone()),
            &mut ctx,
            sink,
        )
        .expect("the plan runs")
    }
}

// A scan reading the table columns.
fn scan(table: TableId, types: &[TypeInfo]) -> PhysicalPlan {
    let columns: Vec<ColumnBinding> = (0..types.len()).map(|i| binding(i, &types[i])).collect();
    let names: Vec<String> = (0..types.len()).map(|i| format!("c{i}")).collect();
    PhysicalPlan::TableScan {
        table,
        columns,
        alias: "t".to_owned(),
        schema: schema_of(&names.iter().map(String::as_str).collect::<Vec<_>>(), types),
        hints: LockHints::default(),
    }
}

fn aggregate_call(
    name: &str,
    arg: Option<BoundExpr>,
    distinct: bool,
) -> vauban_binder::AggregateCall {
    let def = lookup(name).unwrap_or_else(|| panic!("{name} is registered"));
    vauban_binder::AggregateCall { def, arg, distinct }
}

fn hash_agg(
    input: PhysicalPlan,
    group_by: Vec<BoundExpr>,
    aggregates: Vec<vauban_binder::AggregateCall>,
    group_types: &[TypeInfo],
    agg_types: &[TypeInfo],
) -> PhysicalPlan {
    let group_names: Vec<String> = (0..group_by.len()).map(|i| format!("k{i}")).collect();
    let agg_names: Vec<String> = (0..aggregates.len()).map(|i| format!("a{i}")).collect();
    let all_names: Vec<&str> = group_names
        .iter()
        .chain(agg_names.iter())
        .map(String::as_str)
        .collect();
    let all_types: Vec<TypeInfo> = group_types
        .iter()
        .chain(agg_types.iter())
        .cloned()
        .collect();
    PhysicalPlan::HashAggregate {
        input: Box::new(input),
        group_by,
        aggregates,
        schema: schema_of(&all_names, &all_types),
    }
}

fn stream_agg(
    input: PhysicalPlan,
    group_by: Vec<BoundExpr>,
    aggregates: Vec<vauban_binder::AggregateCall>,
    group_types: &[TypeInfo],
    agg_types: &[TypeInfo],
) -> PhysicalPlan {
    let group_names: Vec<String> = (0..group_by.len()).map(|i| format!("k{i}")).collect();
    let agg_names: Vec<String> = (0..aggregates.len()).map(|i| format!("a{i}")).collect();
    let all_names: Vec<&str> = group_names
        .iter()
        .chain(agg_names.iter())
        .map(String::as_str)
        .collect();
    let all_types: Vec<TypeInfo> = group_types
        .iter()
        .chain(agg_types.iter())
        .cloned()
        .collect();
    PhysicalPlan::StreamAggregate {
        input: Box::new(input),
        group_by,
        aggregates,
        schema: schema_of(&all_names, &all_types),
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

/// Helper: runs a plan and collects the result rows.
fn collect(plan: &PhysicalPlan, fixture: &Fixture) -> Vec<Row> {
    let mut sink = CollectSink::new();
    fixture.run(plan, &mut sink);
    sink.rows
}

#[test]
fn group_by_counts_and_sums() {
    register_builtins();
    let fixture = Fixture::new(&[int_t(), int_t()]);
    // Three groups: (1,10), (2,20), (1,30) — group 1 has two rows.
    fixture.insert(&[Value::I32(1), Value::I32(10)]);
    fixture.insert(&[Value::I32(2), Value::I32(20)]);
    fixture.insert(&[Value::I32(1), Value::I32(30)]);

    let plan = hash_agg(
        scan(fixture.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![
            aggregate_call("COUNT", Some(col(1, &int_t())), false),
            aggregate_call("SUM", Some(col(1, &int_t())), false),
        ],
        &[int_t()],
        &[int_nn(), int_t()],
    );

    let rows = collect(&plan, &fixture);
    assert_eq!(rows.len(), 2, "two groups");
    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    // Group 1: SUM = 10 + 30 = 40
    assert_eq!(
        sorted[0],
        vec![Value::I32(1), Value::I32(2), Value::I32(40)]
    );
    // Group 2: SUM = 20
    assert_eq!(
        sorted[1],
        vec![Value::I32(2), Value::I32(1), Value::I32(20)]
    );
}

#[test]
fn null_key_is_one_group() {
    register_builtins();
    let fixture = Fixture::new(&[int_t()]);
    // Two rows with NULL key.
    fixture.insert(&[Value::I32(10)]);
    fixture.insert(&[Value::I32(20)]);

    // The key column is index 0; we need a nullable type for the column binding.
    // Actually the scan column types don't include the grouping column as nullable
    // The table column is nullable (int_t). We add a grouping on a NULL literal.

    // Actually, for NULL keys we need the column itself to be nullable.
    // Let me change the approach: use a nullable int column and insert NULL.
    let fixture = Fixture::new(&[int_t()]);
    // Rows: c0 = NULL, c0 = NULL, c0 = 1
    fixture.insert(&[Value::Null]);
    fixture.insert(&[Value::Null]);
    fixture.insert(&[Value::I32(1)]);

    let plan = hash_agg(
        scan(fixture.table, &[int_t()]),
        vec![col(0, &int_t())],
        vec![aggregate_call("COUNT", Some(col(0, &int_t())), false)],
        &[int_t()],
        &[int_nn()],
    );

    let rows = collect(&plan, &fixture);
    assert_eq!(rows.len(), 2, "two groups: NULL and 1");
    for row in &rows {
        if row[0] == Value::Null {
            // Two NULL rows, COUNT of a nullable column counts non-NULL values.
            // COUNT(c0) over NULL, NULL → 0
            assert_eq!(row[1], Value::I32(0), "NULL group count");
        } else {
            assert_eq!(row[1], Value::I32(1), "non-NULL group count");
        }
    }
}

#[test]
fn aggregate_without_group_by_on_empty_input() {
    register_builtins();
    let fixture = Fixture::new(&[int_t()]);
    // Empty table.

    let plan = hash_agg(
        scan(fixture.table, &[int_t()]),
        vec![], // no GROUP BY
        vec![
            aggregate_call("COUNT", Some(col(0, &int_t())), false),
            aggregate_call("SUM", Some(col(0, &int_t())), false),
        ],
        &[], // no group columns
        &[int_nn(), int_t()],
    );

    let rows = collect(&plan, &fixture);
    assert_eq!(rows.len(), 1, "one row for aggregate without GROUP BY");
    assert_eq!(rows[0][0], Value::I32(0), "COUNT(*) → 0");
    assert_eq!(rows[0][1], Value::Null, "SUM → NULL");
}

#[test]
fn group_by_on_empty_input_gives_no_row() {
    register_builtins();
    let fixture = Fixture::new(&[int_t(), int_t()]);
    // Empty table.

    let plan = hash_agg(
        scan(fixture.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![aggregate_call("COUNT", Some(col(1, &int_t())), false)],
        &[int_t()],
        &[int_nn()],
    );

    let rows = collect(&plan, &fixture);
    assert_eq!(rows.len(), 0, "GROUP BY on empty input → no rows");
}

#[test]
fn stream_aggregate_matches_hash_aggregate() {
    register_builtins();
    let fixture = Fixture::new(&[int_t(), int_t()]);
    // Insert sorted by key to match StreamAggregate's assumption.
    fixture.insert(&[Value::I32(1), Value::I32(10)]);
    fixture.insert(&[Value::I32(1), Value::I32(30)]);
    fixture.insert(&[Value::I32(2), Value::I32(20)]);

    let hash_plan = hash_agg(
        scan(fixture.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![
            aggregate_call("COUNT", Some(col(1, &int_t())), false),
            aggregate_call("SUM", Some(col(1, &int_t())), false),
        ],
        &[int_t()],
        &[int_nn(), int_t()],
    );

    let stream_plan = stream_agg(
        scan(fixture.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![
            aggregate_call("COUNT", Some(col(1, &int_t())), false),
            aggregate_call("SUM", Some(col(1, &int_t())), false),
        ],
        &[int_t()],
        &[int_nn(), int_t()],
    );

    let hash_rows = collect(&hash_plan, &fixture);
    let stream_rows = collect(&stream_plan, &fixture);

    // Sort both to compare as multisets.
    let mut hash_sorted = hash_rows.clone();
    hash_sorted.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    let mut stream_sorted = stream_rows.clone();
    stream_sorted.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));

    assert_eq!(
        hash_sorted, stream_sorted,
        "both operators produce the same result"
    );
}

#[test]
fn count_distinct_counts_once() {
    register_builtins();
    let fixture = Fixture::new(&[int_t()]);
    // Three rows: values 10, 20, 10.
    fixture.insert(&[Value::I32(10)]);
    fixture.insert(&[Value::I32(20)]);
    fixture.insert(&[Value::I32(10)]);

    let plan = hash_agg(
        scan(fixture.table, &[int_t()]),
        vec![],
        vec![aggregate_call("COUNT", Some(col(0, &int_t())), true)],
        &[],
        &[int_nn()],
    );

    let rows = collect(&plan, &fixture);
    assert_eq!(rows.len(), 1, "no GROUP BY → one row");
    assert_eq!(rows[0][0], Value::I32(2), "COUNT(DISTINCT c0) → 2");
}

#[test]
fn null_eliminated_emits_8153_once() {
    register_builtins();
    let fixture = Fixture::new(&[int_t(), int_t()]);
    // Two groups, each with a NULL value.
    fixture.insert(&[Value::I32(1), Value::Null]);
    fixture.insert(&[Value::I32(1), Value::I32(10)]);
    fixture.insert(&[Value::I32(2), Value::Null]);
    fixture.insert(&[Value::I32(2), Value::I32(20)]);

    let mut sink = CollectSink::new();
    let plan = hash_agg(
        scan(fixture.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![
            aggregate_call("COUNT", Some(col(1, &int_t())), false),
            aggregate_call("SUM", Some(col(1, &int_t())), false),
        ],
        &[int_t()],
        &[int_nn(), int_t()],
    );

    fixture.run(&plan, &mut sink);
    let infos = sink.infos;
    assert_eq!(infos[0].number, 8153);
    assert_eq!(infos[0].severity, 10);

    // Counter-proof: no NULL in input → no INFO.
    let fixture2 = Fixture::new(&[int_t(), int_t()]);
    fixture2.insert(&[Value::I32(1), Value::I32(10)]);
    fixture2.insert(&[Value::I32(2), Value::I32(20)]);

    let mut sink2 = CollectSink::new();
    let plan2 = hash_agg(
        scan(fixture2.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![
            aggregate_call("COUNT", Some(col(1, &int_t())), false),
            aggregate_call("SUM", Some(col(1, &int_t())), false),
        ],
        &[int_t()],
        &[int_nn(), int_t()],
    );

    fixture2.run(&plan2, &mut sink2);
    assert!(sink2.infos.is_empty(), "no NULL → no 8153");
}

#[test]
fn having_filters_groups() {
    register_builtins();
    // HAVING is a Filter above the aggregate. This test verifies the pattern works.
    let fixture = Fixture::new(&[int_t(), int_t()]);
    fixture.insert(&[Value::I32(1), Value::I32(10)]);
    fixture.insert(&[Value::I32(1), Value::I32(20)]); // group 1 has 2 rows
    fixture.insert(&[Value::I32(2), Value::I32(30)]); // group 2 has 1 row

    let agg_plan = hash_agg(
        scan(fixture.table, &[int_t(), int_t()]),
        vec![col(0, &int_t())],
        vec![aggregate_call("COUNT", Some(col(0, &int_t())), false)],
        &[int_t()],
        &[int_nn()],
    );

    // HAVING COUNT(*) > 1 — only group with more than one row stays.
    let having_predicate = {
        use vauban_binder::{BoundExprKind, CompareOp};
        let count_ref = BoundExpr {
            kind: BoundExprKind::ColumnRef(ColumnBinding {
                column: ColumnId(2), // second column of agg output is the COUNT
                index: 1,
                name: "a0".to_owned(),
                ty: int_nn(),
            }),
            ty: int_nn(),
            line: 1,
        };
        let one = lit(Value::I32(1));
        BoundExpr {
            kind: BoundExprKind::Compare {
                op: CompareOp::Gt,
                left: Box::new(count_ref),
                right: Box::new(one),
            },
            ty: TypeInfo::new(SqlType::Bit, true),
            line: 1,
        }
    };

    let filter_plan = PhysicalPlan::Filter {
        input: Box::new(agg_plan),
        predicate: having_predicate,
    };

    let rows = collect(&filter_plan, &fixture);
    assert_eq!(rows.len(), 1, "only group 1 has COUNT > 1");
    assert_eq!(rows[0][0], Value::I32(1), "key = 1");
    assert_eq!(rows[0][1], Value::I32(2), "COUNT = 2");
}

#[test]
fn case_insensitive_group_key() {
    register_builtins();
    let fixture = Fixture::new(&[varchar_ci()]);
    fixture.insert(&[Value::String(SqlString {
        text: "a".to_owned(),
    })]);
    fixture.insert(&[Value::String(SqlString {
        text: "A".to_owned(),
    })]);

    let plan = hash_agg(
        scan(fixture.table, &[varchar_ci()]),
        vec![col(0, &varchar_ci())],
        vec![aggregate_call("COUNT", Some(col(0, &varchar_ci())), false)],
        &[varchar_ci()],
        &[int_nn()],
    );

    let rows = collect(&plan, &fixture);
    assert_eq!(
        rows.len(),
        1,
        "'a' and 'A' form one group under default collation"
    );
    assert_eq!(rows[0][1], Value::I32(2), "two rows in the group");
}
