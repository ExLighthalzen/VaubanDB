//! Set operators: `UNION`, `UNION ALL`, `EXCEPT` and `INTERSECT`.
//!
//! Each plan is built by hand over `Values` or `TableScan`; SQL text is not parsed here.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, LockHints, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_executor::{CollectSink, ExecContext, ExecOutcome, Row, execute};
use vauban_planner::{PhysicalPlan, PhysicalStatement};
use vauban_storage::{MemoryStorage, Row as StorageRow, Snapshot, Storage, TableId, TableShape};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo, Value};

// -----------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------

struct Fixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    left: TableId,
    right: TableId,
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: vec![int_t()],
            clustered_key: None,
        };
        let left = storage
            .create_table(db, &shape)
            .expect("the left table is new");
        let right = storage
            .create_table(db, &shape)
            .expect("the right table is new");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self {
            storage,
            txn,
            left,
            right,
        }
    }

    fn insert(&self, table: TableId, value: i32) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let row = StorageRow(vec![Value::I32(value)]);
        self.storage
            .insert(handle.id, table, &row)
            .expect("the row is inserted");
        self.txn.commit(handle).expect("the transaction commits");
    }

    fn snapshot(&self) -> Snapshot {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&handle)
    }

    fn run(&self, plan: &PhysicalPlan) -> Vec<Row> {
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let mut sink = CollectSink::new();
        let outcome = execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, &mut sink)
            .expect("the plan runs");
        assert!(matches!(outcome, ExecOutcome::Rows(_)), "{outcome:?}");
        sink.rows
    }
}

fn int_t() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn schema_v() -> OutputSchema {
    OutputSchema {
        columns: vec![OutputColumn {
            name: "v".to_owned(),
            ty: int_t(),
        }],
    }
}

fn lit(value: Value) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty: int_t(),
        line: 1,
    }
}

fn values(vals: &[Value]) -> PhysicalPlan {
    let rows: Vec<Vec<BoundExpr>> = vals.iter().map(|v| vec![lit(v.clone())]).collect();
    PhysicalPlan::Values {
        rows,
        schema: schema_v(),
    }
}

fn binding() -> ColumnBinding {
    ColumnBinding {
        column: vauban_catalog::ColumnId(1),
        index: 0,
        name: "v".to_owned(),
        ty: int_t(),
        depth: 0,
    }
}

fn scan(table: TableId) -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table,
        columns: vec![binding()],
        schema: schema_v(),
        alias: "t".to_owned(),
        hints: LockHints::default(),
    }
}

fn union_all(left: PhysicalPlan, right: PhysicalPlan) -> PhysicalPlan {
    PhysicalPlan::Union {
        inputs: vec![left, right],
        all: true,
        schema: schema_v(),
    }
}

fn union_distinct(left: PhysicalPlan, right: PhysicalPlan) -> PhysicalPlan {
    PhysicalPlan::Distinct(Box::new(union_all(left, right)))
}

fn except(left: PhysicalPlan, right: PhysicalPlan) -> PhysicalPlan {
    PhysicalPlan::Except {
        inputs: vec![left, right],
        all: false,
        schema: schema_v(),
    }
}

fn intersect(left: PhysicalPlan, right: PhysicalPlan) -> PhysicalPlan {
    PhysicalPlan::Intersect {
        inputs: vec![left, right],
        all: false,
        schema: schema_v(),
    }
}

fn collect_scalar(plan: &PhysicalPlan) -> Vec<Row> {
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let mut sink = CollectSink::new();
    let outcome = execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, &mut sink)
        .expect("the plan runs");
    assert!(matches!(outcome, ExecOutcome::Rows(_)), "{outcome:?}");
    sink.rows
}

fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        use std::cmp::Ordering;
        match (&a[0], &b[0]) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::I32(x), Value::I32(y)) => x.cmp(y),
            _ => Ordering::Equal,
        }
    });
}

// -----------------------------------------------------------------------
// Values-based set operations
// -----------------------------------------------------------------------

#[test]
fn union_distinct_removes_duplicates_and_folds_nulls() {
    let left = values(&[Value::I32(1), Value::I32(2), Value::Null]);
    let right = values(&[Value::I32(2), Value::I32(3), Value::Null]);
    let mut rows = collect_scalar(&union_distinct(left, right));
    sort_rows(&mut rows);
    assert_eq!(
        rows,
        vec![
            vec![Value::Null],
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(3)],
        ]
    );
}

#[test]
fn nulls_are_equal_for_union_and_intersect() {
    let null_pair = union_distinct(values(&[Value::Null]), values(&[Value::Null]));
    assert_eq!(collect_scalar(&null_pair), vec![vec![Value::Null]]);

    let null_intersect = intersect(values(&[Value::Null]), values(&[Value::Null]));
    assert_eq!(collect_scalar(&null_intersect), vec![vec![Value::Null]]);

    // Counter-proof: treating NULL as unequal would answer two rows here.
    let null_all = union_all(values(&[Value::Null]), values(&[Value::Null]));
    assert_eq!(
        collect_scalar(&null_all),
        vec![vec![Value::Null], vec![Value::Null]]
    );
}

#[test]
fn union_all_keeps_duplicates() {
    let left = values(&[Value::I32(1), Value::I32(2)]);
    let right = values(&[Value::I32(2), Value::I32(3)]);
    let rows = collect_scalar(&union_all(left, right));
    assert_eq!(
        rows,
        vec![
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(2)],
            vec![Value::I32(3)],
        ]
    );
}

#[test]
fn except_does_not_repeat_left_duplicates() {
    let left = values(&[Value::I32(1), Value::I32(1), Value::I32(2)]);
    let right = values(&[Value::I32(1)]);
    let rows = collect_scalar(&except(left, right));
    assert_eq!(rows, vec![vec![Value::I32(2)]]);
}

#[test]
fn except_and_intersect_on_values() {
    let left = values(&[Value::I32(1), Value::I32(2), Value::I32(3)]);
    let right = values(&[Value::I32(2), Value::I32(4)]);

    let mut except_rows = collect_scalar(&except(left.clone(), right.clone()));
    sort_rows(&mut except_rows);
    assert_eq!(except_rows, vec![vec![Value::I32(1)], vec![Value::I32(3)]]);

    let mut intersect_rows = collect_scalar(&intersect(left, right));
    sort_rows(&mut intersect_rows);
    assert_eq!(intersect_rows, vec![vec![Value::I32(2)]]);
}

#[test]
fn except_null_on_left_not_in_right() {
    let left = values(&[Value::Null]);
    let right = values(&[Value::I32(1)]);
    assert_eq!(
        collect_scalar(&except(left, right)),
        vec![vec![Value::Null]]
    );

    let both_null = except(values(&[Value::Null]), values(&[Value::Null]));
    assert!(collect_scalar(&both_null).is_empty());
}

// -----------------------------------------------------------------------
// TableScan-based set operations
// -----------------------------------------------------------------------

#[test]
fn setops_over_table_scans() {
    let fixture = Fixture::new();
    fixture.insert(fixture.left, 1);
    fixture.insert(fixture.left, 2);
    fixture.insert(fixture.left, 3);
    fixture.insert(fixture.right, 2);
    fixture.insert(fixture.right, 4);

    let left = scan(fixture.left);
    let right = scan(fixture.right);

    let mut union_rows = fixture.run(&union_distinct(left.clone(), right.clone()));
    sort_rows(&mut union_rows);
    assert_eq!(
        union_rows,
        vec![
            vec![Value::I32(1)],
            vec![Value::I32(2)],
            vec![Value::I32(3)],
            vec![Value::I32(4)],
        ]
    );

    let mut except_rows = fixture.run(&except(left.clone(), right.clone()));
    sort_rows(&mut except_rows);
    assert_eq!(except_rows, vec![vec![Value::I32(1)], vec![Value::I32(3)]]);

    let mut intersect_rows = fixture.run(&intersect(left, right));
    sort_rows(&mut intersect_rows);
    assert_eq!(intersect_rows, vec![vec![Value::I32(2)]]);
}

// -----------------------------------------------------------------------
// Keys under the type of the column
// -----------------------------------------------------------------------

fn typed_values(ty: &TypeInfo, vals: &[Value]) -> PhysicalPlan {
    let rows: Vec<Vec<BoundExpr>> = vals
        .iter()
        .map(|v| {
            vec![BoundExpr {
                kind: BoundExprKind::Literal(v.clone()),
                ty: ty.clone(),
                line: 1,
            }]
        })
        .collect();
    PhysicalPlan::Values {
        rows,
        schema: typed_schema(ty),
    }
}

fn typed_schema(ty: &TypeInfo) -> OutputSchema {
    OutputSchema {
        columns: vec![OutputColumn {
            name: "v".to_owned(),
            ty: ty.clone(),
        }],
    }
}

fn decimal(mantissa: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal(vauban_types::Decimal {
        mantissa,
        precision,
        scale,
    })
}

/// `1.0 INTERSECT 1.00` is one row, `1.0 EXCEPT 1.00` none: the key of a decimal is its
/// value, its scale left out.
#[test]
fn intersect_finds_equal_decimals_of_different_scales() {
    let ty = TypeInfo::new(
        SqlType::Decimal {
            precision: 6,
            scale: 2,
        },
        true,
    );
    let left = || typed_values(&ty, &[decimal(10, 5, 1)]);
    let right = || typed_values(&ty, &[decimal(100, 6, 2)]);
    let schema = typed_schema(&ty);
    let intersect = PhysicalPlan::Intersect {
        inputs: vec![left(), right()],
        all: false,
        schema: schema.clone(),
    };
    assert_eq!(collect_scalar(&intersect), vec![vec![decimal(10, 5, 1)]]);
    let except = PhysicalPlan::Except {
        inputs: vec![left(), right()],
        all: false,
        schema,
    };
    assert!(collect_scalar(&except).is_empty());
}

/// `'x' INTERSECT 'X'` under a case-insensitive collation is one row, and `'x', 'X'
/// EXCEPT 'y'` is one row: the left rows `'x'` and `'X'` are one value.
#[test]
fn set_operators_compare_strings_under_the_collation_of_the_column() {
    let text = |t: &str| Value::String(vauban_types::SqlString { text: t.to_owned() });
    let mut ty = TypeInfo::new(SqlType::VarChar(vauban_types::Len::Fixed(10)), true);
    ty.collation =
        Some(vauban_types::Collation::parse("SQL_Latin1_General_CP1_CI_AS").expect("known"));
    let intersect = PhysicalPlan::Intersect {
        inputs: vec![
            typed_values(&ty, &[text("x")]),
            typed_values(&ty, &[text("X")]),
        ],
        all: false,
        schema: typed_schema(&ty),
    };
    assert_eq!(collect_scalar(&intersect), vec![vec![text("x")]]);
    let except = PhysicalPlan::Except {
        inputs: vec![
            typed_values(&ty, &[text("x"), text("X")]),
            typed_values(&ty, &[text("y")]),
        ],
        all: false,
        schema: typed_schema(&ty),
    };
    assert_eq!(collect_scalar(&except), vec![vec![text("x")]]);
}
