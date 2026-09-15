//! Nested-loop join tests: all five join kinds, the reopen guarantee, and cancellation.
//!
//! Each test builds a physical plan by hand over a `MemoryStorage`, runs it through
//! [`execute_collect`] and inspects the rows. No SQL text is parsed here.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_executor::{ExecContext, ExecOutcome, Row, execute_collect};
use vauban_planner::{KeyRangeExpr, PhysicalJoinKind, PhysicalPlan, PhysicalStatement};
use vauban_storage::{
    Direction, IndexShape, KeyColumn, MemoryStorage, Row as StorageRow, Snapshot, Storage, TableId,
    TableShape,
};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo, Value};

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

struct Fixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    left_table: TableId,
    right_table: TableId,
}

fn int_ty() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn binding(index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("small index") + 1),
        index,
        name: format!("c{index}"),
        ty: int_ty(),
    }
}

fn schema_of(names: &[&str]) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .map(|name| OutputColumn {
                name: (*name).to_owned(),
                ty: int_ty(),
            })
            .collect(),
    }
}

fn col_ref(index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(index)),
        ty: int_ty(),
        line: 1,
    }
}

fn eq(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    }
}

fn scan(table: TableId) -> PhysicalPlan {
    PhysicalPlan::TableScan {
        table,
        columns: vec![binding(0)],
        alias: "t".to_owned(),
        schema: schema_of(&["c0"]),
    }
}

impl Fixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: vec![int_ty()],
            clustered_key: None,
        };
        let left_table = storage.create_table(db, &shape).expect("left table");
        let right_table = storage.create_table(db, &shape).expect("right table");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self {
            storage,
            txn,
            left_table,
            right_table,
        }
    }

    fn insert(&self, table: TableId, values: &[i32]) {
        for v in values {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            let row = StorageRow(vec![Value::I32(*v)]);
            self.storage
                .insert(handle.id, table, &row)
                .expect("the row is inserted");
            self.txn.commit(handle).expect("the transaction commits");
        }
    }

    fn insert_null(&self, table: TableId) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let row = StorageRow(vec![Value::Null]);
        self.storage
            .insert(handle.id, table, &row)
            .expect("the row is inserted");
        self.txn.commit(handle).expect("the transaction commits");
    }

    fn snapshot(&self) -> Snapshot {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&handle)
    }

    fn run_join(
        &self,
        kind: PhysicalJoinKind,
        on: Option<BoundExpr>,
        left_plan: PhysicalPlan,
        right_plan: PhysicalPlan,
    ) -> (ExecOutcome, Vec<Row>) {
        let join = PhysicalPlan::NestedLoopJoin {
            outer: Box::new(left_plan),
            inner: Box::new(right_plan),
            kind,
            on,
            schema: schema_of(&["c0", "c1"]),
        };
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let stmt = PhysicalStatement::Query(join);
        let (outcome, set) = execute_collect(&stmt, &mut ctx).expect("the join runs");
        (outcome, set.rows)
    }
}

// ---------------------------------------------------------------------------------------
// CROSS JOIN
// ---------------------------------------------------------------------------------------

#[test]
fn cross_join_is_the_cartesian_product() {
    let f = Fixture::new();
    f.insert(f.left_table, &[1, 2, 3]);
    f.insert(f.right_table, &[4, 5]);
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Cross,
        None,
        scan(f.left_table),
        scan(f.right_table),
    );
    assert_eq!(rows.len(), 6, "3 × 2 = 6");
}

// ---------------------------------------------------------------------------------------
// INNER JOIN
// ---------------------------------------------------------------------------------------

#[test]
fn inner_join_keeps_only_true() {
    let f = Fixture::new();
    f.insert(f.left_table, &[1, 2]);
    f.insert_null(f.left_table);
    f.insert(f.right_table, &[1]);
    f.insert_null(f.right_table);
    f.insert(f.right_table, &[3]);

    let on = eq(col_ref(0), col_ref(1));
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Inner,
        Some(on),
        scan(f.left_table),
        scan(f.right_table),
    );
    // Only 1=1 is true; NULL=NULL is unknown, NULL=1 is unknown, 2=1 and 1=3 are false.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::I32(1));
    assert_eq!(rows[0][1], Value::I32(1));
}

// ---------------------------------------------------------------------------------------
// LEFT JOIN
// ---------------------------------------------------------------------------------------

#[test]
fn left_join_pads_the_inner_with_null() {
    let f = Fixture::new();
    f.insert(f.left_table, &[1, 2]);
    f.insert(f.right_table, &[1]);

    let on = eq(col_ref(0), col_ref(1));
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Left,
        Some(on),
        scan(f.left_table),
        scan(f.right_table),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::I32(1), Value::I32(1)]);
    assert_eq!(rows[1], vec![Value::I32(2), Value::Null]);
}

// ---------------------------------------------------------------------------------------
// RIGHT JOIN
// ---------------------------------------------------------------------------------------

#[test]
fn right_join_pads_the_outer_with_null() {
    let f = Fixture::new();
    f.insert(f.left_table, &[1]);
    f.insert(f.right_table, &[1, 2]);

    let on = eq(col_ref(0), col_ref(1));
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Right,
        Some(on),
        scan(f.left_table),
        scan(f.right_table),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::I32(1), Value::I32(1)]);
    assert_eq!(rows[1], vec![Value::Null, Value::I32(2)]);
}

// ---------------------------------------------------------------------------------------
// FULL JOIN
// ---------------------------------------------------------------------------------------

#[test]
fn full_join_emits_both_sides() {
    let f = Fixture::new();
    f.insert(f.left_table, &[1, 2]);
    f.insert(f.right_table, &[1, 3]);

    let on = eq(col_ref(0), col_ref(1));
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Full,
        Some(on),
        scan(f.left_table),
        scan(f.right_table),
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::I32(1), Value::I32(1)]);
    assert_eq!(rows[1], vec![Value::I32(2), Value::Null]);
    assert_eq!(rows[2], vec![Value::Null, Value::I32(3)]);
}

// ---------------------------------------------------------------------------------------
// Duplicate keys
// ---------------------------------------------------------------------------------------

#[test]
fn duplicate_keys_multiply_rows() {
    let f = Fixture::new();
    f.insert(f.left_table, &[1]);
    f.insert(f.right_table, &[1, 1]);

    let on = eq(col_ref(0), col_ref(1));
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Inner,
        Some(on),
        scan(f.left_table),
        scan(f.right_table),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::I32(1), Value::I32(1)]);
    assert_eq!(rows[1], vec![Value::I32(1), Value::I32(1)]);
}

// ---------------------------------------------------------------------------------------
// Reopen guarantee
// ---------------------------------------------------------------------------------------

#[test]
fn inner_is_reopened_for_each_outer_row() {
    let f = Fixture::new();
    f.insert(f.left_table, &[10, 20, 30]); // 3 outer rows
    f.insert(f.right_table, &[1]); // 1 inner row per open

    // No ON → effectively a cross join: 3 × 1 = 3. If the inner were opened only once,
    // only 1 row would come out.
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Inner,
        None,
        scan(f.left_table),
        scan(f.right_table),
    );
    assert_eq!(rows.len(), 3);
}

#[test]
fn full_join_materialises_inner_once() {
    let f = Fixture::new();
    f.insert(f.left_table, &[10, 20]);
    f.insert(f.right_table, &[100, 200]);

    let on = eq(col_ref(0), col_ref(1));
    let (_outcome, rows) = f.run_join(
        PhysicalJoinKind::Full,
        Some(on),
        scan(f.left_table),
        scan(f.right_table),
    );
    // No matches (keys differ), so: 2 unmatched outer + 2 unmatched inner = 4 rows
    assert_eq!(rows.len(), 4);
}

// ---------------------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------------------

#[test]
fn cancel_stops_a_long_join() {
    let f = Fixture::new();
    for i in 0..100 {
        f.insert(f.left_table, &[i]);
    }
    for i in 0..100 {
        f.insert(f.right_table, &[i]);
    }

    let on = eq(col_ref(0), col_ref(1));
    let join = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(f.left_table)),
        inner: Box::new(scan(f.right_table)),
        kind: PhysicalJoinKind::Inner,
        on: Some(on),
        schema: schema_of(&["c0", "c1"]),
    };

    let token = vauban_executor::CancelToken::new();
    token.cancel();
    let eval = StaticContext::default();
    let snap = f.snapshot();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
        .with_engine(f.storage.as_ref(), &f.txn, &snap)
        .with_cancel(&token);
    let stmt = PhysicalStatement::Query(join);
    let (outcome, _set) = execute_collect(&stmt, &mut ctx).expect("cancelled join");
    assert!(matches!(outcome, ExecOutcome::Cancelled), "{outcome:?}");
}

// ---------------------------------------------------------------------------------------
// Correlated seek
// ---------------------------------------------------------------------------------------

#[test]
fn inner_seek_uses_the_outer_row() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage.create_database("mydb").expect("new db");
    let shape = TableShape {
        columns: vec![int_ty()],
        clustered_key: None,
    };
    let left = storage.create_table(db, &shape).expect("left table");
    let right = storage.create_table(db, &shape).expect("right table");
    let index = storage
        .create_index(
            right,
            &IndexShape {
                columns: vec![KeyColumn {
                    column: 0,
                    descending: false,
                }],
                unique: false,
                included: Vec::new(),
            },
        )
        .expect("index created");

    let txn = TransactionManager::new(Arc::clone(&storage));

    let insert = |table: TableId, v: i32| {
        let h = txn.begin(IsolationLevel::ReadCommitted);
        storage
            .insert(h.id, table, &StorageRow(vec![Value::I32(v)]))
            .expect("insert");
        txn.commit(h).expect("commit");
    };

    for v in &[1, 2, 3] {
        insert(left, *v);
    }
    for v in &[1, 3, 5] {
        insert(right, *v);
    }

    let snap = {
        let h = txn.begin(IsolationLevel::ReadCommitted);
        txn.statement_snapshot(&h)
    };

    let inner_seek = PhysicalPlan::IndexSeek {
        index,
        range: KeyRangeExpr::Point(vec![col_ref(0)]),
        columns: vec![binding(0)],
        direction: Direction::Forward,
        schema: schema_of(&["c0"]),
    };

    let on = eq(col_ref(0), col_ref(1));

    let join_with_seek = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(left)),
        inner: Box::new(inner_seek),
        kind: PhysicalJoinKind::Inner,
        on: Some(on.clone()),
        schema: schema_of(&["c0", "c1"]),
    };

    let join_no_index = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(left)),
        inner: Box::new(scan(right)),
        kind: PhysicalJoinKind::Inner,
        on: Some(on),
        schema: schema_of(&["c0", "c1"]),
    };

    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    let stmt_seek = PhysicalStatement::Query(join_with_seek);
    let (_, set_seek) = execute_collect(&stmt_seek, &mut ctx).expect("seek join runs");

    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    let stmt_ref = PhysicalStatement::Query(join_no_index);
    let (_, set_ref) = execute_collect(&stmt_ref, &mut ctx).expect("ref join runs");

    assert_eq!(
        set_seek.rows, set_ref.rows,
        "seek and scan produce the same rows"
    );
}
