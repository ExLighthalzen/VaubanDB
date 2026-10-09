//! Nested-loop join tests: all five join kinds, the reopen guarantee, and cancellation.
//!
//! Each test builds a physical plan by hand over a `MemoryStorage`, runs it through
//! [`execute_collect`] and inspects the rows. No SQL text is parsed here.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, JoinKind, LockHints, LogicalOp,
    OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_executor::{ExecContext, ExecOutcome, Row, execute_collect};
use vauban_planner::{KeyRangeExpr, PhysicalJoinKind, PhysicalPlan, PhysicalStatement, SubPlan};
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
        depth: 0,
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
        hints: LockHints::default(),
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
        hints: LockHints::default(),
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

/// A `WHERE` equality over a comma-style cross product reads both sides of the
/// concatenated row, not the inner scan indices pushed for correlation.
#[test]
fn filter_over_a_cross_join_compares_both_sides() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage
        .create_database("mydb")
        .expect("the database is new");
    let shape = TableShape {
        columns: vec![int_ty(), int_ty()],
        clustered_key: None,
    };
    let clients = storage.create_table(db, &shape).expect("clients");
    let regions = storage.create_table(db, &shape).expect("regions");
    let txn = TransactionManager::new(Arc::clone(&storage));

    let insert = |table, a: i32, b: i32| {
        let h = txn.begin(IsolationLevel::ReadCommitted);
        storage
            .insert(h.id, table, &StorageRow(vec![Value::I32(a), Value::I32(b)]))
            .expect("insert");
        txn.commit(h).expect("commit");
    };
    insert(clients, 1, 1);
    insert(clients, 2, 1);
    insert(clients, 3, 2);
    insert(clients, 4, 99);
    insert(regions, 1, 10);
    insert(regions, 2, 20);
    insert(regions, 3, 30);
    insert(regions, 4, 40);

    fn wide_binding(index: usize) -> ColumnBinding {
        ColumnBinding {
            column: ColumnId(i32::try_from(index).expect("small index") + 1),
            index,
            name: format!("c{index}"),
            ty: int_ty(),
            depth: 0,
        }
    }

    fn wide_scan(table: TableId) -> PhysicalPlan {
        PhysicalPlan::TableScan {
            table,
            columns: vec![wide_binding(0), wide_binding(1)],
            alias: "t".to_owned(),
            schema: schema_of(&["k", "v"]),
            hints: LockHints::default(),
        }
    }

    let cross = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(wide_scan(clients)),
        inner: Box::new(wide_scan(regions)),
        kind: PhysicalJoinKind::Cross,
        on: None,
        schema: schema_of(&["client_k", "client_v", "region_k", "region_v"]),
    };
    let predicate = eq(col_ref(1), col_ref(2));
    let plan = PhysicalPlan::Filter {
        input: Box::new(cross),
        predicate,
    };

    let eval = StaticContext::default();
    let snap = txn.statement_snapshot(&txn.begin(IsolationLevel::ReadCommitted));
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    let stmt = PhysicalStatement::Query(plan);
    let (_, set) = execute_collect(&stmt, &mut ctx).expect("the filter over cross runs");

    let mut rows = set.rows;
    rows.sort_by(|a, b| {
        (
            match a[0] {
                Value::I32(v) => v,
                _ => 0,
            },
            match a[2] {
                Value::I32(v) => v,
                _ => 0,
            },
        )
            .cmp(&(
                match b[0] {
                    Value::I32(v) => v,
                    _ => 0,
                },
                match b[2] {
                    Value::I32(v) => v,
                    _ => 0,
                },
            ))
    });
    assert_eq!(
        rows,
        vec![
            vec![Value::I32(1), Value::I32(1), Value::I32(1), Value::I32(10)],
            vec![Value::I32(2), Value::I32(1), Value::I32(1), Value::I32(10)],
            vec![Value::I32(3), Value::I32(2), Value::I32(2), Value::I32(20)],
        ],
        "each client pairs with its region only"
    );
}

fn column_binding(column: i32, index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(column),
        index,
        name: format!("c{index}"),
        ty: int_ty(),
        depth: 0,
    }
}

fn column_ref(column: i32, index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(column_binding(column, index)),
        ty: int_ty(),
        line: 1,
    }
}

fn join_column_ref(column: i32, index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(ColumnBinding {
            column: ColumnId(column),
            index,
            // Join-wide bindings keep the inner scan name; only the index shifts.
            name: "c0".to_owned(),
            ty: int_ty(),
            depth: 0,
        }),
        ty: int_ty(),
        line: 1,
    }
}

fn and(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Logical {
            op: LogicalOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    }
}

fn logical_plan_from_physical(plan: &PhysicalPlan) -> vauban_binder::LogicalPlan {
    match plan {
        PhysicalPlan::TableScan {
            table,
            columns,
            alias,
            schema,
            hints,
        } => vauban_binder::LogicalPlan::Scan {
            table: *table,
            columns: columns.clone(),
            alias: alias.clone(),
            schema: schema.clone(),
            hints: *hints,
        },
        PhysicalPlan::Filter { input, predicate } => vauban_binder::LogicalPlan::Filter {
            input: Box::new(logical_plan_from_physical(input)),
            predicate: predicate.clone(),
        },
        PhysicalPlan::NestedLoopJoin {
            outer,
            inner,
            kind,
            on,
            schema,
        } => vauban_binder::LogicalPlan::Join {
            left: Box::new(logical_plan_from_physical(outer)),
            right: Box::new(logical_plan_from_physical(inner)),
            kind: match kind {
                PhysicalJoinKind::Cross => JoinKind::Cross,
                PhysicalJoinKind::Inner => JoinKind::Inner,
                PhysicalJoinKind::Left => JoinKind::Left,
                PhysicalJoinKind::Right => JoinKind::Right,
                PhysicalJoinKind::Full => JoinKind::Full,
                PhysicalJoinKind::Semi | PhysicalJoinKind::AntiSemi => {
                    panic!("semi join shape not used in this test")
                }
            },
            on: on.clone(),
            schema: schema.clone(),
        },
        other => panic!("unsupported test plan shape: {other:?}"),
    }
}

thread_local! {
    static SUBQUERY_IDS: std::cell::RefCell<std::collections::HashMap<String, vauban_binder::SubqueryId>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The identity a test gives the subquery whose plan is `plan`: the node built over a plan
/// and the entry of the `SubqueryEval` built over the same plan share it.
fn subquery_id(plan: &PhysicalPlan) -> vauban_binder::SubqueryId {
    SUBQUERY_IDS.with(|ids| {
        *ids.borrow_mut()
            .entry(format!("{plan:?}"))
            .or_insert_with(vauban_binder::SubqueryId::fresh)
    })
}

/// A bound subquery over the logical form of `plan`, under [`subquery_id`].
fn bound_subquery(plan: &PhysicalPlan) -> vauban_binder::BoundSubquery {
    vauban_binder::BoundSubquery {
        id: subquery_id(plan),
        plan: Box::new(logical_plan_from_physical(plan)),
    }
}

fn exists_expr(plan: &PhysicalPlan) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Exists(bound_subquery(plan)),
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    }
}

fn subquery_eval(input: PhysicalPlan, subplans: Vec<SubPlan>) -> PhysicalPlan {
    let schema = input.schema().clone();
    PhysicalPlan::SubqueryEval {
        input: Box::new(input),
        subplans,
        schema,
    }
}

/// A cross product filtered inside a correlated `EXISTS` keeps both correlation paths.
#[test]
fn correlated_exists_with_a_cross_join_filter_inside() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage
        .create_database("mydb")
        .expect("the database is new");
    let shape = TableShape {
        columns: vec![int_ty()],
        clustered_key: None,
    };
    let outer_table = storage.create_table(db, &shape).expect("outer");
    let side_a = storage.create_table(db, &shape).expect("side a");
    let side_b = storage.create_table(db, &shape).expect("side b");
    let txn = TransactionManager::new(Arc::clone(&storage));

    let insert = |table: TableId, value: i32| {
        let h = txn.begin(IsolationLevel::ReadCommitted);
        storage
            .insert(h.id, table, &StorageRow(vec![Value::I32(value)]))
            .expect("insert");
        txn.commit(h).expect("commit");
    };
    for value in [10, 20, 30] {
        insert(outer_table, value);
    }
    insert(side_a, 10);
    insert(side_a, 20);
    insert(side_a, 99);
    insert(side_b, 10);
    insert(side_b, 20);
    insert(side_b, 88);

    let scan_side = |table: TableId, column: i32| PhysicalPlan::TableScan {
        table,
        columns: vec![column_binding(column, 0)],
        alias: "t".to_owned(),
        schema: schema_of(&["c0"]),
        hints: LockHints::default(),
    };

    let cross = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan_side(side_a, 10)),
        inner: Box::new(scan_side(side_b, 11)),
        kind: PhysicalJoinKind::Cross,
        on: None,
        schema: schema_of(&["a", "b"]),
    };
    let inner = PhysicalPlan::Filter {
        input: Box::new(cross),
        predicate: and(
            eq(join_column_ref(10, 0), join_column_ref(11, 1)),
            eq(join_column_ref(10, 0), column_ref(1, 0)),
        ),
    };
    let outer_scan = PhysicalPlan::TableScan {
        table: outer_table,
        columns: vec![column_binding(1, 0)],
        alias: "o".to_owned(),
        schema: schema_of(&["c0"]),
        hints: LockHints::default(),
    };
    let exists = exists_expr(&inner);
    let plan = PhysicalPlan::Filter {
        input: Box::new(subquery_eval(
            outer_scan,
            vec![SubPlan {
                id: subquery_id(&inner),
                plan: inner.clone(),
                correlated: true,
            }],
        )),
        predicate: exists,
    };

    let eval = StaticContext::default();
    let snap = txn.statement_snapshot(&txn.begin(IsolationLevel::ReadCommitted));
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    let (_, set) = execute_collect(&PhysicalStatement::Query(plan), &mut ctx)
        .expect("correlated exists over cross runs");
    assert_eq!(
        set.rows,
        vec![vec![Value::I32(10)], vec![Value::I32(20)]],
        "only outer rows with a matching pair in the cross product stay"
    );
}

// ---------------------------------------------------------------------------------------
// Self-joins: the same table under two aliases
// ---------------------------------------------------------------------------------------

/// A two-column table read twice: both scans carry the same column identifiers and names,
/// so only the index in the concatenated row tells one alias from the other.
struct SelfJoinFixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    table: TableId,
}

impl SelfJoinFixture {
    /// Rows `(1, 10)` and `(2, 20)`.
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: vec![int_ty(), int_ty()],
            clustered_key: None,
        };
        let table = storage.create_table(db, &shape).expect("table");
        let txn = TransactionManager::new(Arc::clone(&storage));
        for (a, b) in [(1, 10), (2, 20)] {
            let h = txn.begin(IsolationLevel::ReadCommitted);
            storage
                .insert(h.id, table, &StorageRow(vec![Value::I32(a), Value::I32(b)]))
                .expect("insert");
            txn.commit(h).expect("commit");
        }
        Self {
            storage,
            txn,
            table,
        }
    }

    /// The scan of one alias: column ids 1 and 2, names `a` and `b`, row-local indices.
    fn scan(&self, alias: &str) -> PhysicalPlan {
        let column = |id: i32, index: usize, name: &str| ColumnBinding {
            column: ColumnId(id),
            index,
            name: name.to_owned(),
            ty: int_ty(),
            depth: 0,
        };
        PhysicalPlan::TableScan {
            table: self.table,
            columns: vec![column(1, 0, "a"), column(2, 1, "b")],
            alias: alias.to_owned(),
            schema: schema_of(&["a", "b"]),
            hints: LockHints::default(),
        }
    }

    /// A reference to a column of the concatenated join row: `x.a` is index 0, `x.b` 1,
    /// `y.a` 2 and `y.b` 3, with the column id and name of the underlying scan.
    fn join_ref(index: usize) -> BoundExpr {
        let (id, name) = if index.is_multiple_of(2) {
            (1, "a")
        } else {
            (2, "b")
        };
        BoundExpr {
            kind: BoundExprKind::ColumnRef(ColumnBinding {
                column: ColumnId(id),
                index,
                name: name.to_owned(),
                ty: int_ty(),
                depth: 0,
            }),
            ty: int_ty(),
            line: 1,
        }
    }

    fn join(&self, kind: PhysicalJoinKind, on: Option<BoundExpr>) -> PhysicalPlan {
        PhysicalPlan::NestedLoopJoin {
            outer: Box::new(self.scan("x")),
            inner: Box::new(self.scan("y")),
            kind,
            on,
            schema: schema_of(&["a", "b", "a", "b"]),
        }
    }

    fn project(input: PhysicalPlan, indices: &[usize]) -> PhysicalPlan {
        PhysicalPlan::Project {
            input: Box::new(input),
            exprs: indices
                .iter()
                .map(|index| vauban_binder::BoundProjection {
                    expr: Self::join_ref(*index),
                    name: format!("c{index}"),
                })
                .collect(),
            schema: schema_of(&vec!["c"; indices.len()]),
        }
    }

    fn run(&self, plan: PhysicalPlan) -> Vec<Row> {
        let eval = StaticContext::default();
        let snap = self
            .txn
            .statement_snapshot(&self.txn.begin(IsolationLevel::ReadCommitted));
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let (_, set) =
            execute_collect(&PhysicalStatement::Query(plan), &mut ctx).expect("the plan runs");
        let mut rows = set.rows;
        rows.sort_by_key(|row| {
            row.iter()
                .map(|value| match value {
                    Value::I32(v) => *v,
                    _ => -1,
                })
                .collect::<Vec<_>>()
        });
        rows
    }
}

fn ints(values: &[i32]) -> Row {
    values.iter().map(|v| Value::I32(*v)).collect()
}

/// `SELECT x.a, y.a, x.b, y.b FROM t x CROSS JOIN t y`: the projection reads the second
/// alias past the first one, not a copy of the first one.
#[test]
fn self_cross_join_projects_both_aliases() {
    let f = SelfJoinFixture::new();
    let plan = SelfJoinFixture::project(f.join(PhysicalJoinKind::Cross, None), &[0, 2, 1, 3]);
    assert_eq!(
        f.run(plan),
        vec![
            ints(&[1, 1, 10, 10]),
            ints(&[1, 2, 10, 20]),
            ints(&[2, 1, 20, 10]),
            ints(&[2, 2, 20, 20]),
        ],
        "each pair of the product keeps both sides"
    );
}

/// `SELECT x.a, y.a FROM t x, t y WHERE x.a <> y.a`: the filter and the projection read
/// the same `y`.
#[test]
fn self_comma_join_filter_and_projection_read_the_same_side() {
    let f = SelfJoinFixture::new();
    let predicate = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Ne,
            left: Box::new(SelfJoinFixture::join_ref(0)),
            right: Box::new(SelfJoinFixture::join_ref(2)),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    };
    let filter = PhysicalPlan::Filter {
        input: Box::new(f.join(PhysicalJoinKind::Cross, None)),
        predicate,
    };
    let plan = SelfJoinFixture::project(filter, &[0, 2]);
    assert_eq!(
        f.run(plan),
        vec![ints(&[1, 2]), ints(&[2, 1])],
        "the rows that pass the filter show the two distinct sides"
    );
}

/// `SELECT x.a, y.a FROM t x JOIN t y ON x.a < y.a`: an inner self-join with an `ON`.
#[test]
fn self_inner_join_on_an_inequality() {
    let f = SelfJoinFixture::new();
    let on = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Lt,
            left: Box::new(SelfJoinFixture::join_ref(0)),
            right: Box::new(SelfJoinFixture::join_ref(2)),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    };
    let plan = SelfJoinFixture::project(f.join(PhysicalJoinKind::Inner, Some(on)), &[0, 2]);
    assert_eq!(f.run(plan), vec![ints(&[1, 2])]);
}

/// `SELECT x.a, y.a, y.b FROM t x LEFT JOIN t y ON x.a < y.a`: the row of `x` without a
/// match is padded on the `y` side.
#[test]
fn self_left_join_pads_the_second_alias() {
    let f = SelfJoinFixture::new();
    let on = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Lt,
            left: Box::new(SelfJoinFixture::join_ref(0)),
            right: Box::new(SelfJoinFixture::join_ref(2)),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    };
    let plan = SelfJoinFixture::project(f.join(PhysicalJoinKind::Left, Some(on)), &[0, 2, 3]);
    assert_eq!(
        f.run(plan),
        vec![
            ints(&[1, 2, 20]),
            vec![Value::I32(2), Value::Null, Value::Null],
        ]
    );
}

// ---------------------------------------------------------------------------------------
// A derived table as the inner of a nested-loop join
//
// The axis is the layout the inner plan is described by while the join drives it: a
// derived table that reorders its source names its own columns, and a predicate inside
// the inner plan reads them at the positions the derived table gives them, not at the
// positions its source has underneath. The outer side is a plain scan here; the derived
// table on the outer side, the two join operators against each other and the choice of
// plan are left out, the last one being the planner's.
// ---------------------------------------------------------------------------------------

/// Two tables of two `int` columns: `small` holds `(1, 10)`, `(2, 20)` and `(3, 30)`,
/// `other` holds `(1, 100)`, `(3, 300)` and `(4, 400)`.
struct DerivedInnerFixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    small: TableId,
    other: TableId,
}

impl DerivedInnerFixture {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: vec![int_ty(), int_ty()],
            clustered_key: None,
        };
        let small = storage.create_table(db, &shape).expect("small");
        let other = storage.create_table(db, &shape).expect("other");
        let txn = TransactionManager::new(Arc::clone(&storage));
        let insert = |table: TableId, rows: &[(i32, i32)]| {
            for (a, b) in rows {
                let h = txn.begin(IsolationLevel::ReadCommitted);
                storage
                    .insert(
                        h.id,
                        table,
                        &StorageRow(vec![Value::I32(*a), Value::I32(*b)]),
                    )
                    .expect("insert");
                txn.commit(h).expect("commit");
            }
        };
        insert(small, &[(1, 10), (2, 20), (3, 30)]);
        insert(other, &[(1, 100), (3, 300), (4, 400)]);
        Self {
            storage,
            txn,
            small,
            other,
        }
    }

    fn column(id: i32, index: usize, name: &str) -> ColumnBinding {
        ColumnBinding {
            column: ColumnId(id),
            index,
            name: name.to_owned(),
            ty: int_ty(),
            depth: 0,
        }
    }

    fn column_ref(id: i32, index: usize, name: &str) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::ColumnRef(Self::column(id, index, name)),
            ty: int_ty(),
            line: 1,
        }
    }

    /// `small` scanned as `(k, v)`, column ids 1 and 2.
    fn small_scan(&self) -> PhysicalPlan {
        PhysicalPlan::TableScan {
            table: self.small,
            columns: vec![Self::column(1, 0, "k"), Self::column(2, 1, "v")],
            alias: "q".to_owned(),
            schema: schema_of(&["k", "v"]),
            hints: LockHints::default(),
        }
    }

    /// `other` scanned as `(ok, ow)`, column ids 11 and 12, which `small_scan` leaves
    /// unused: a reference to one of them is read from the outer row.
    fn other_scan(&self) -> PhysicalPlan {
        PhysicalPlan::TableScan {
            table: self.other,
            columns: vec![Self::column(11, 0, "ok"), Self::column(12, 1, "ow")],
            alias: "o".to_owned(),
            schema: schema_of(&["ok", "ow"]),
            hints: LockHints::default(),
        }
    }

    /// `(SELECT q.v, q.k FROM small q) d`: the projection swaps the two columns and keeps
    /// their names, so `d.k` sits at index 1 where the source keeps `k` at index 0.
    fn derived_swapping_small(&self) -> PhysicalPlan {
        PhysicalPlan::Project {
            input: Box::new(self.small_scan()),
            exprs: vec![
                vauban_binder::BoundProjection {
                    expr: Self::column_ref(2, 1, "v"),
                    name: "v".to_owned(),
                },
                vauban_binder::BoundProjection {
                    expr: Self::column_ref(1, 0, "k"),
                    name: "k".to_owned(),
                },
            ],
            schema: schema_of(&["v", "k"]),
        }
    }

    fn run(&self, plan: PhysicalPlan) -> Vec<Row> {
        let eval = StaticContext::default();
        let snap = self
            .txn
            .statement_snapshot(&self.txn.begin(IsolationLevel::ReadCommitted));
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let (_, set) =
            execute_collect(&PhysicalStatement::Query(plan), &mut ctx).expect("the plan runs");
        set.rows
    }
}

/// The join drives a derived table that swaps its source's columns, and the predicate
/// inside it compares the derived table's own `k` with the outer row's `ok`: the pairs
/// kept are those whose keys are equal.
#[test]
fn nested_loop_join_over_a_derived_table_that_reorders_its_source() {
    let f = DerivedInnerFixture::new();
    let inner = PhysicalPlan::Filter {
        input: Box::new(f.derived_swapping_small()),
        predicate: eq(
            DerivedInnerFixture::column_ref(1, 1, "k"),
            DerivedInnerFixture::column_ref(11, 0, "ok"),
        ),
    };
    let plan = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(f.other_scan()),
        inner: Box::new(inner),
        kind: PhysicalJoinKind::Cross,
        on: None,
        schema: schema_of(&["ok", "ow", "v", "k"]),
    };
    assert_eq!(
        f.run(plan),
        vec![ints(&[1, 100, 10, 1]), ints(&[3, 300, 30, 3])],
        "the derived table's `k` is read at index 1, where its own projection puts it"
    );
}
