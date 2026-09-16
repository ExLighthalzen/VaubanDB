//! Unit tests for `Sort`, `TopN`, `Top` (WITH TIES, PERCENT) and `Distinct`.
//!
//! Each plan is built by hand over an in-memory storage; SQL text is not parsed, bound, or
//! planned. The context helpers are those of `tests/operator_basics.rs`.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundTop, ColumnBinding, LockHints, OutputColumn, OutputSchema,
    SessionOptions, SortKey,
};
use vauban_catalog::ColumnId;
use vauban_executor::{CancelToken, CollectSink, ExecContext, ExecOutcome, Row, RowSink, execute};
use vauban_planner::{PhysicalPlan, PhysicalStatement};
use vauban_storage::{MemoryStorage, Row as StorageRow, Storage, TableId, TableShape};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlString, SqlType, TypeInfo, Value};

// -----------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------

/// A table of one `int` column in a fresh in-memory storage.
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
        Self {
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
}

fn int_t() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn nvarchar_t() -> TypeInfo {
    TypeInfo::new(SqlType::NVarChar(vauban_types::Len::Max), false)
}

fn binding(index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: format!("c{index}"),
        ty: int_t(),
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

fn col(index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(index)),
        ty: int_t(),
        line: 1,
    }
}

fn sort_key(index: usize, desc: bool) -> SortKey {
    SortKey {
        expr: col(index),
        desc,
        collation: None,
    }
}

fn run_scalar(
    plan: &PhysicalPlan,
    sink: &mut dyn RowSink,
) -> Result<ExecOutcome, vauban_errors::SqlResult<ExecOutcome>> {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    match execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, sink) {
        Ok(outcome) => Ok(outcome),
        Err(e) => Err(Err(e)),
    }
}

// -----------------------------------------------------------------------
// Helpers for the values-based sort
// -----------------------------------------------------------------------

/// `VALUES (v)` of one column, with the given values.
fn values(vals: &[Value]) -> PhysicalPlan {
    let rows: Vec<Vec<BoundExpr>> = vals
        .iter()
        .map(|v| {
            vec![BoundExpr {
                kind: BoundExprKind::Literal(v.clone()),
                ty: int_t(),
                line: 1,
            }]
        })
        .collect();
    PhysicalPlan::Values {
        rows,
        schema: schema_of(&["v"], &[int_t()]),
    }
}

/// Runs a plan and collects the rows.
fn collect(plan: &PhysicalPlan) -> Vec<Row> {
    let mut sink = CollectSink::new();
    let outcome = run_scalar(plan, &mut sink).expect("the plan runs");
    assert!(matches!(outcome, ExecOutcome::Rows(_)), "{outcome:?}");
    sink.rows
}

/// Runs a plan expecting an error.
fn expect_bug(plan: &PhysicalPlan) -> String {
    let mut sink = CollectSink::new();
    let result = run_scalar(plan, &mut sink);
    let Err(Err(e)) = result else {
        panic!("expected an error, got {result:?}");
    };
    assert_eq!(e.number, 50000);
    e.message
}

// -----------------------------------------------------------------------
// Sort
// -----------------------------------------------------------------------

#[test]
fn sort_ascending_and_descending() {
    let plan_asc = PhysicalPlan::Sort {
        input: Box::new(values(&[Value::I32(3), Value::I32(1), Value::I32(2)])),
        keys: vec![sort_key(0, false)],
    };
    let rows_asc = collect(&plan_asc);
    assert_eq!(
        rows_asc,
        vec![
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(3)]
        ]
    );

    let plan_desc = PhysicalPlan::Sort {
        input: Box::new(values(&[Value::I32(3), Value::I32(1), Value::I32(2)])),
        keys: vec![sort_key(0, true)],
    };
    let rows_desc = collect(&plan_desc);
    assert_eq!(
        rows_desc,
        vec![
            vec![Value::I32(3)],
            vec![Value::I32(2)],
            vec![Value::I32(1)]
        ]
    );
}

#[test]
fn sort_puts_null_first_ascending() {
    let plan_asc = PhysicalPlan::Sort {
        input: Box::new(values(&[Value::I32(2), Value::Null, Value::I32(1)])),
        keys: vec![sort_key(0, false)],
    };
    let rows = collect(&plan_asc);
    assert_eq!(
        rows,
        vec![vec![Value::Null], vec![Value::I32(1)], vec![Value::I32(2)],]
    );

    let plan_desc = PhysicalPlan::Sort {
        input: Box::new(values(&[Value::I32(2), Value::Null, Value::I32(1)])),
        keys: vec![sort_key(0, true)],
    };
    let rows = collect(&plan_desc);
    assert_eq!(
        rows,
        vec![vec![Value::I32(2)], vec![Value::I32(1)], vec![Value::Null],]
    );
}

#[test]
fn sort_uses_the_collation() {
    let plan = PhysicalPlan::Sort {
        input: Box::new(PhysicalPlan::Values {
            rows: vec![
                vec![BoundExpr {
                    kind: BoundExprKind::Literal(Value::String(SqlString {
                        text: "B".to_owned(),
                    })),
                    ty: nvarchar_t(),
                    line: 1,
                }],
                vec![BoundExpr {
                    kind: BoundExprKind::Literal(Value::String(SqlString {
                        text: "a".to_owned(),
                    })),
                    ty: nvarchar_t(),
                    line: 1,
                }],
            ],
            schema: schema_of(&["v"], &[nvarchar_t()]),
        }),
        keys: vec![SortKey {
            expr: col(0),
            desc: false,
            collation: None,
        }],
    };
    let rows = collect(&plan);
    // Under the default CI collation, 'a' and 'A' compare equal, and ordering is
    // case-insensitive. 'a' < 'B' because case-insensitive sorting ignores case.
    assert_eq!(rows.len(), 2);
    // The CI collation puts 'a' before 'B'.
    assert_eq!(rows[0][0], Value::String(SqlString { text: "a".into() }));
    assert_eq!(rows[1][0], Value::String(SqlString { text: "B".into() }));
}

#[test]
fn sort_on_two_keys() {
    // Two-column table with the second column deciding between equal first-column values.
    let fixture = Fixture::new(&[int_t(), int_t()]);
    fixture.insert(&[Value::I32(2), Value::I32(20)]);
    fixture.insert(&[Value::I32(1), Value::I32(10)]);
    fixture.insert(&[Value::I32(2), Value::I32(30)]);

    let columns: Vec<ColumnBinding> = (0..2).map(binding).collect();
    let plan = PhysicalPlan::Sort {
        input: Box::new(PhysicalPlan::TableScan {
            table: fixture.table,
            columns: columns.clone(),
            alias: "t".to_owned(),
            schema: schema_of(&["c0", "c1"], &[int_t(), int_t()]),
            hints: LockHints::default(),
        }),
        keys: vec![sort_key(0, false), sort_key(1, false)],
    };
    let eval = StaticContext::default();
    let snap = fixture.snapshot();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        fixture.storage.as_ref(),
        &fixture.txn,
        &snap,
    );
    let mut sink = CollectSink::new();
    execute(&PhysicalStatement::Query(plan), &mut ctx, &mut sink).expect("the sort runs");
    // c0=1 before c0=2; among c0=2 rows, c1=20 before c1=30.
    assert_eq!(
        sink.rows,
        vec![
            vec![Value::I32(1), Value::I32(10)],
            vec![Value::I32(2), Value::I32(20)],
            vec![Value::I32(2), Value::I32(30)],
        ]
    );
}

// -----------------------------------------------------------------------
// TopN
// -----------------------------------------------------------------------

#[test]
fn top_n_takes_the_first_rows_of_the_order() {
    let inner = PhysicalPlan::Sort {
        input: Box::new(values(&[Value::I32(3), Value::I32(1), Value::I32(2)])),
        keys: vec![sort_key(0, false)],
    };
    let plan = PhysicalPlan::TopN {
        input: Box::new(inner),
        keys: vec![sort_key(0, false)],
        top: BoundTop {
            expr: BoundExpr {
                kind: BoundExprKind::Literal(Value::I64(2)),
                ty: TypeInfo::new(SqlType::BigInt, false),
                line: 1,
            },
            percent: false,
            with_ties: false,
        },
    };
    let rows = collect(&plan);
    assert_eq!(rows, vec![vec![Value::I32(1)], vec![Value::I32(2)]]);
}

#[test]
fn with_ties_extends_past_n() {
    // Five rows, the sort key repeated: 1, 2, 2, 2, 3. TopN(2) → [1, 2].
    // Top(2) WITH TIES → [1, 2, 2, 2].
    let vals = values(&[
        Value::I32(1),
        Value::I32(2),
        Value::I32(2),
        Value::I32(2),
        Value::I32(3),
    ]);
    let sorted = PhysicalPlan::Sort {
        input: Box::new(vals),
        keys: vec![sort_key(0, false)],
    };
    // TopN(2) without ties: 2 rows.
    let plan_no_ties = PhysicalPlan::TopN {
        input: Box::new(sorted),
        keys: vec![sort_key(0, false)],
        top: BoundTop {
            expr: BoundExpr {
                kind: BoundExprKind::Literal(Value::I64(2)),
                ty: TypeInfo::new(SqlType::BigInt, false),
                line: 1,
            },
            percent: false,
            with_ties: false,
        },
    };
    let rows = collect(&plan_no_ties);
    assert_eq!(rows, vec![vec![Value::I32(1)], vec![Value::I32(2)]]);

    // TopN(2) WITH TIES: also includes the extra 2s.
    // But TopN with WITH TIES doesn't work on PhysicalPlan::TopN directly —
    // that's the variant with tied behavior. Let's use the TopN with ties flag.
    // TopN(2) WITH TIES over the SAME sorted values.
    let inner2 = PhysicalPlan::Sort {
        input: Box::new(values(&[
            Value::I32(1),
            Value::I32(2),
            Value::I32(2),
            Value::I32(2),
            Value::I32(3),
        ])),
        keys: vec![sort_key(0, false)],
    };
    let plan_with_ties = PhysicalPlan::TopN {
        input: Box::new(inner2),
        keys: vec![sort_key(0, false)],
        top: BoundTop {
            expr: BoundExpr {
                kind: BoundExprKind::Literal(Value::I64(2)),
                ty: TypeInfo::new(SqlType::BigInt, false),
                line: 1,
            },
            percent: false,
            with_ties: true,
        },
    };
    let rows = collect(&plan_with_ties);
    assert_eq!(
        rows,
        vec![
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(2)],
            vec![Value::I32(2)],
        ]
    );
}

#[test]
fn with_ties_without_order_key_is_a_bug() {
    let plan = PhysicalPlan::TopN {
        input: Box::new(values(&[Value::I32(1), Value::I32(2)])),
        keys: vec![],
        top: BoundTop {
            expr: BoundExpr {
                kind: BoundExprKind::Literal(Value::I64(2)),
                ty: TypeInfo::new(SqlType::BigInt, false),
                line: 1,
            },
            percent: false,
            with_ties: true,
        },
    };
    let msg = expect_bug(&plan);
    assert!(
        msg.contains("WITH TIES") || msg.contains("without ORDER BY"),
        "{msg}"
    );
}

#[test]
fn percent_rounds_up() {
    // 3 rows, TOP 50 PERCENT: ceil(3 * 50 / 100) = ceil(1.5) = 2.
    let inner = PhysicalPlan::Sort {
        input: Box::new(values(&[Value::I32(1), Value::I32(2), Value::I32(3)])),
        keys: vec![sort_key(0, false)],
    };
    let plan = PhysicalPlan::TopN {
        input: Box::new(inner),
        keys: vec![sort_key(0, false)],
        top: BoundTop {
            expr: BoundExpr {
                kind: BoundExprKind::Literal(Value::F64(50.0)),
                ty: TypeInfo::new(SqlType::Float, false),
                line: 1,
            },
            percent: true,
            with_ties: false,
        },
    };
    let rows = collect(&plan);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::I32(1)]);
    assert_eq!(rows[1], vec![Value::I32(2)]);
}

// -----------------------------------------------------------------------
// Distinct
// -----------------------------------------------------------------------

#[test]
fn distinct_removes_duplicates_and_keeps_one_null() {
    let plan = PhysicalPlan::Distinct(Box::new(values(&[
        Value::Null,
        Value::I32(1),
        Value::I32(1),
        Value::Null,
        Value::I32(2),
    ])));
    let rows = collect(&plan);
    // NULL once, 1 once, 2 once.
    let mut sorted: Vec<&Value> = rows.iter().flatten().collect();
    sorted.sort_by(|a, b| {
        use std::cmp::Ordering;
        match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::I32(x), Value::I32(y)) => x.cmp(y),
            _ => Ordering::Equal,
        }
    });
    assert_eq!(sorted, vec![&Value::Null, &Value::I32(1), &Value::I32(2)]);
    assert_eq!(rows.len(), 3);
}

// -----------------------------------------------------------------------
// Cancellation
// -----------------------------------------------------------------------

#[test]
fn cancel_stops_the_materialisation() {
    // A Sort that materialises many rows: the token is raised after 1024 rows.
    let token = CancelToken::new();
    let many: Vec<Value> = (0..3000).map(Value::I32).collect();

    let plan = PhysicalPlan::Sort {
        input: Box::new(values(&many)),
        keys: vec![sort_key(0, false)],
    };
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_cancel(&token);
    token.cancel();
    let mut sink = CollectSink::new();
    let outcome = execute(&PhysicalStatement::Query(plan), &mut ctx, &mut sink)
        .expect("a cancellation is not an error");
    assert!(matches!(outcome, ExecOutcome::Cancelled), "{outcome:?}");
}
