//! `HashJoin` operator tests: data matching the nested loop join tests, served through
//! a hash table. The reference is the set of rows a `NestedLoopJoin` on the same keys
//! would produce, asserted directly here (the nested loop operator was parallel and not yet
//! merged when this file was written).

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, CompareOp, LockHints, OutputColumn, OutputSchema,
    SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_executor::{
    CancelToken, CollectSink, ExecContext, ExecOutcome, RowSink, build_operator, execute,
};
use vauban_planner::{PhysicalJoinKind, PhysicalPlan, PhysicalStatement};
use vauban_storage::{MemoryStorage, Snapshot, Storage, TableShape};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo, Value};

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn varchar() -> TypeInfo {
    TypeInfo::new(SqlType::VarChar(vauban_types::Len::Fixed(10)), true)
}

fn binding(index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: format!("c{index}"),
        ty: int(),
    }
}

fn schema_of(names: &[&str]) -> OutputSchema {
    OutputSchema {
        columns: names
            .iter()
            .map(|name| OutputColumn {
                name: (*name).to_owned(),
                ty: int(),
            })
            .collect(),
    }
}

fn col(index: usize) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::ColumnRef(binding(index)),
        ty: int(),
        line: 1,
    }
}

fn lit(value: i32) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::I32(value)),
        ty: int(),
        line: 1,
    }
}

/// Two tables in one storage, plus the transaction manager.
struct DualFixture {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    build_table: vauban_storage::TableId,
    probe_table: vauban_storage::TableId,
}

impl DualFixture {
    fn new(build_width: usize, probe_width: usize) -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");

        let build_shape = TableShape {
            columns: vec![int(); build_width],
            clustered_key: None,
        };
        let build_table = storage.create_table(db, &build_shape).expect("build table");

        let probe_shape = TableShape {
            columns: vec![int(); probe_width],
            clustered_key: None,
        };
        let probe_table = storage.create_table(db, &probe_shape).expect("probe table");

        let txn = TransactionManager::new(Arc::clone(&storage));
        Self {
            storage,
            txn,
            build_table,
            probe_table,
        }
    }

    fn insert_build(&self, values: &[i32]) {
        let h = self.txn.begin(IsolationLevel::ReadCommitted);
        self.storage
            .insert(
                h.id,
                self.build_table,
                &vauban_storage::Row(values.iter().map(|v| Value::I32(*v)).collect()),
            )
            .expect("insert build row");
        self.txn.commit(h).expect("commit");
    }

    fn insert_probe(&self, values: &[i32]) {
        let h = self.txn.begin(IsolationLevel::ReadCommitted);
        self.storage
            .insert(
                h.id,
                self.probe_table,
                &vauban_storage::Row(values.iter().map(|v| Value::I32(*v)).collect()),
            )
            .expect("insert probe row");
        self.txn.commit(h).expect("commit");
    }

    fn insert_build_values(&self, values: &[Value]) {
        let h = self.txn.begin(IsolationLevel::ReadCommitted);
        self.storage
            .insert(
                h.id,
                self.build_table,
                &vauban_storage::Row(values.to_vec()),
            )
            .expect("insert build row");
        self.txn.commit(h).expect("commit");
    }

    fn insert_probe_values(&self, values: &[Value]) {
        let h = self.txn.begin(IsolationLevel::ReadCommitted);
        self.storage
            .insert(
                h.id,
                self.probe_table,
                &vauban_storage::Row(values.to_vec()),
            )
            .expect("insert probe row");
        self.txn.commit(h).expect("commit");
    }

    fn snapshot(&self) -> Snapshot {
        let h = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&h)
    }

    fn build_scan(&self, width: usize) -> PhysicalPlan {
        let columns: Vec<ColumnBinding> = (0..width).map(binding).collect();
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        PhysicalPlan::TableScan {
            table: self.build_table,
            columns,
            alias: "b".to_owned(),
            schema: schema_of(&names.iter().map(String::as_str).collect::<Vec<_>>()),
            hints: LockHints::default(),
        }
    }

    fn probe_scan(&self, width: usize) -> PhysicalPlan {
        let columns: Vec<ColumnBinding> = (0..width).map(binding).collect();
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        PhysicalPlan::TableScan {
            table: self.probe_table,
            columns,
            alias: "p".to_owned(),
            schema: schema_of(&names.iter().map(String::as_str).collect::<Vec<_>>()),
            hints: LockHints::default(),
        }
    }

    fn run(&self, plan: &PhysicalPlan, sink: &mut dyn RowSink) -> ExecOutcome {
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, sink).expect("the plan runs")
    }

    fn run_cancelled(
        &self,
        plan: &PhysicalPlan,
        token: &CancelToken,
        sink: &mut dyn RowSink,
    ) -> ExecOutcome {
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_engine(self.storage.as_ref(), &self.txn, &snap)
            .with_cancel(token);
        execute(&PhysicalStatement::Query(plan.clone()), &mut ctx, sink)
            .expect("cancellation is not an error")
    }
}

fn hash_join_plan(
    build: PhysicalPlan,
    probe: PhysicalPlan,
    kind: PhysicalJoinKind,
    keys: Vec<(BoundExpr, BoundExpr)>,
    residual: Option<BoundExpr>,
) -> PhysicalPlan {
    let mut schema_cols =
        Vec::with_capacity(probe.schema().columns.len() + build.schema().columns.len());
    for c in &probe.schema().columns {
        schema_cols.push(c.clone());
    }
    for c in &build.schema().columns {
        schema_cols.push(c.clone());
    }
    PhysicalPlan::HashJoin {
        build: Box::new(build),
        probe: Box::new(probe),
        kind,
        keys,
        residual,
        schema: OutputSchema {
            columns: schema_cols,
        },
    }
}

/// Sort rows by their first two integers for deterministic comparison.
fn sort_i32_rows(rows: &mut [Vec<Value>]) {
    rows.sort_by(|a, b| {
        let get = |r: &[Value], i: usize| -> i32 {
            match r.get(i) {
                Some(Value::I32(v)) => *v,
                _ => 0,
            }
        };
        let (a0, a1, a2, a3) = (get(a, 0), get(a, 1), get(a, 2), get(a, 3));
        let (b0, b1, b2, b3) = (get(b, 0), get(b, 1), get(b, 2), get(b, 3));
        (a0, a1, a2, a3).cmp(&(b0, b1, b2, b3))
    });
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// INNER hash join on one key: matches rows on key equality.
#[test]
fn hash_inner_equals_nested_loop() {
    let f = DualFixture::new(2, 2);
    f.insert_build(&[1, 10]);
    f.insert_build(&[2, 20]);
    f.insert_build(&[3, 30]);
    f.insert_probe(&[1, 100]);
    f.insert_probe(&[2, 200]);
    f.insert_probe(&[4, 400]);

    let plan = hash_join_plan(
        f.build_scan(2),
        f.probe_scan(2),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        None,
    );

    let mut sink = CollectSink::new();
    let outcome = f.run(&plan, &mut sink);
    assert!(matches!(outcome, ExecOutcome::Rows(2)), "{outcome:?}");

    let mut rows = sink.rows.clone();
    sort_i32_rows(&mut rows);
    let mut expected = vec![
        vec![
            Value::I32(1),
            Value::I32(100),
            Value::I32(1),
            Value::I32(10),
        ],
        vec![
            Value::I32(2),
            Value::I32(200),
            Value::I32(2),
            Value::I32(20),
        ],
    ];
    sort_i32_rows(&mut expected);
    assert_eq!(rows, expected);
}

/// LEFT hash join: the unmatched probe row is emitted with NULLs on the build side.
#[test]
fn hash_left_pads_the_other_side() {
    let f = DualFixture::new(1, 1);
    f.insert_build(&[1]);
    f.insert_build(&[3]);
    f.insert_probe(&[1]);
    f.insert_probe(&[2]); // no build match
    f.insert_probe(&[3]);

    let plan = hash_join_plan(
        f.build_scan(1),
        f.probe_scan(1),
        PhysicalJoinKind::Left,
        vec![(col(0), col(0))],
        None,
    );

    let mut sink = CollectSink::new();
    let outcome = f.run(&plan, &mut sink);
    assert!(matches!(outcome, ExecOutcome::Rows(3)), "{outcome:?}");

    let mut rows = sink.rows.clone();
    sort_i32_rows(&mut rows);
    assert_eq!(
        rows,
        vec![
            vec![Value::I32(1), Value::I32(1)],
            vec![Value::I32(2), Value::Null],
            vec![Value::I32(3), Value::I32(3)],
        ]
    );

    // Counter-proof: INNER drops the unmatched row.
    let plan_inner = hash_join_plan(
        f.build_scan(1),
        f.probe_scan(1),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        None,
    );
    let mut sink_inner = CollectSink::new();
    f.run(&plan_inner, &mut sink_inner);
    let mut rows_inner = sink_inner.rows.clone();
    sort_i32_rows(&mut rows_inner);
    assert_eq!(
        rows_inner,
        vec![
            vec![Value::I32(1), Value::I32(1)],
            vec![Value::I32(3), Value::I32(3)],
        ],
    );
}

/// NULL keys do not match: build NULL row is skipped, probe NULL key finds no match.
#[test]
fn null_key_never_matches() {
    let f = DualFixture::new(2, 2);
    f.insert_build_values(&[Value::Null, Value::I32(10)]);
    f.insert_build_values(&[Value::I32(1), Value::I32(20)]);
    f.insert_probe_values(&[Value::Null, Value::I32(100)]);
    f.insert_probe_values(&[Value::I32(1), Value::I32(200)]);

    let plan = hash_join_plan(
        f.build_scan(2),
        f.probe_scan(2),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        None,
    );

    let mut sink = CollectSink::new();
    f.run(&plan, &mut sink);
    assert_eq!(sink.rows.len(), 1, "one inner match");

    // LEFT: NULL-key probe row is emitted with NULLs on the build side.
    let plan_left = hash_join_plan(
        f.build_scan(2),
        f.probe_scan(2),
        PhysicalJoinKind::Left,
        vec![(col(0), col(0))],
        None,
    );
    let mut sink_left = CollectSink::new();
    f.run(&plan_left, &mut sink_left);
    assert_eq!(sink_left.rows.len(), 2, "LEFT emits both probe rows");
    let null_rows: Vec<_> = sink_left
        .rows
        .iter()
        .filter(|r| matches!(r[0], Value::Null))
        .collect();
    assert_eq!(null_rows.len(), 1, "one NULL probe row");
    assert_eq!(null_rows[0][2], Value::Null, "build side col0 is NULL");
    assert_eq!(null_rows[0][3], Value::Null, "build side col1 is NULL");
}

/// Residual predicate filters after the key match.
#[test]
fn residual_filters_after_the_key_match() {
    let f = DualFixture::new(2, 1);
    f.insert_build(&[1, 10]);
    f.insert_build(&[1, 20]);
    f.insert_probe(&[1]);

    // Residual: build.c1 > 15. Concatenated row: [probe.c0, build.c0, build.c1].
    // build.c1 is at the probe width (1) + build key index (1) = 2.
    let residual = BoundExpr {
        kind: BoundExprKind::Compare {
            op: CompareOp::Gt,
            left: Box::new(BoundExpr {
                kind: BoundExprKind::ColumnRef(ColumnBinding {
                    column: ColumnId(3),
                    index: 2,
                    name: "build_c1".to_owned(),
                    ty: int(),
                }),
                ty: int(),
                line: 1,
            }),
            right: Box::new(lit(15)),
        },
        ty: TypeInfo::new(SqlType::Bit, true),
        line: 1,
    };

    let plan = hash_join_plan(
        f.build_scan(2),
        f.probe_scan(1),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        Some(residual),
    );

    let mut sink = CollectSink::new();
    f.run(&plan, &mut sink);
    assert_eq!(sink.rows.len(), 1);
    assert_eq!(
        sink.rows[0],
        vec![Value::I32(1), Value::I32(1), Value::I32(20)]
    );

    // Counter-proof: without residual, both build rows match.
    let plan_no_residual = hash_join_plan(
        f.build_scan(2),
        f.probe_scan(1),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        None,
    );
    let mut sink2 = CollectSink::new();
    f.run(&plan_no_residual, &mut sink2);
    assert_eq!(sink2.rows.len(), 2);
}

/// Duplicate build keys: two build rows with one key match the probe row twice.
#[test]
fn duplicate_build_keys_multiply_rows() {
    let f = DualFixture::new(1, 1);
    f.insert_build(&[1]);
    f.insert_build(&[1]);
    f.insert_probe(&[1]);

    let plan = hash_join_plan(
        f.build_scan(1),
        f.probe_scan(1),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        None,
    );

    let mut sink = CollectSink::new();
    f.run(&plan, &mut sink);
    assert_eq!(sink.rows.len(), 2);
    for row in &sink.rows {
        assert_eq!(*row, vec![Value::I32(1), Value::I32(1)]);
    }
}

/// FULL hash join is a Bug.
#[test]
fn full_hash_join_is_a_bug() {
    let f = DualFixture::new(1, 1);
    let plan = hash_join_plan(
        f.build_scan(1),
        f.probe_scan(1),
        PhysicalJoinKind::Full,
        vec![(col(0), col(0))],
        None,
    );
    let result = build_operator(&plan);
    let error = match result {
        Err(e) => e,
        Ok(_) => panic!("Full HashJoin should have returned an error"),
    };
    assert_eq!(error.number, 50000);
    assert!(
        error.message.contains("NestedLoopJoin"),
        "{}",
        error.message
    );
}

/// Cancellation during the build phase stops the operator.
#[test]
fn cancel_stops_the_build() {
    let f = DualFixture::new(1, 1);
    f.insert_build(&[1]);
    f.insert_build(&[2]);
    f.insert_build(&[3]);
    f.insert_probe(&[1]);

    let plan = hash_join_plan(
        f.build_scan(1),
        f.probe_scan(1),
        PhysicalJoinKind::Inner,
        vec![(col(0), col(0))],
        None,
    );

    let token = CancelToken::new();
    token.cancel();

    let mut sink = CollectSink::new();
    let outcome = f.run_cancelled(&plan, &token, &mut sink);
    assert!(matches!(outcome, ExecOutcome::Cancelled), "{outcome:?}");
    assert!(sink.rows.is_empty());
}

/// Case-insensitive key matching under the default collation.
#[test]
fn case_insensitive_key_matches_under_the_default_collation() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let db = storage.create_database("mydb").expect("db");

    let str_shape = TableShape {
        columns: vec![varchar(), int()],
        clustered_key: None,
    };
    let b_table = storage.create_table(db, &str_shape).expect("build table");
    let p_table = storage.create_table(db, &str_shape).expect("probe table");

    let txn = TransactionManager::new(Arc::clone(&storage));

    let ins = |tbl, v: &str, payload: i32| {
        let h = txn.begin(IsolationLevel::ReadCommitted);
        storage
            .insert(
                h.id,
                tbl,
                &vauban_storage::Row(vec![
                    Value::String(vauban_types::SqlString { text: v.to_owned() }),
                    Value::I32(payload),
                ]),
            )
            .expect("insert");
        txn.commit(h).expect("commit");
    };
    ins(b_table, "abc", 10);
    ins(p_table, "ABC", 100);

    fn str_binding(index: usize) -> ColumnBinding {
        ColumnBinding {
            column: ColumnId(i32::try_from(index).unwrap() + 1),
            index,
            name: format!("c{index}"),
            ty: varchar(),
        }
    }

    fn str_schema(names: &[&str]) -> OutputSchema {
        OutputSchema {
            columns: names
                .iter()
                .map(|name| OutputColumn {
                    name: (*name).to_owned(),
                    ty: varchar(),
                })
                .collect(),
        }
    }

    fn str_col(index: usize) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::ColumnRef(str_binding(index)),
            ty: varchar(),
            line: 1,
        }
    }

    let build_scan = PhysicalPlan::TableScan {
        table: b_table,
        columns: vec![str_binding(0), str_binding(1)],
        alias: "b".to_owned(),
        schema: str_schema(&["k", "v"]),
        hints: LockHints::default(),
    };
    let probe_scan = PhysicalPlan::TableScan {
        table: p_table,
        columns: vec![str_binding(0), str_binding(1)],
        alias: "p".to_owned(),
        schema: str_schema(&["k", "v"]),
        hints: LockHints::default(),
    };

    let plan = hash_join_plan(
        build_scan,
        probe_scan,
        PhysicalJoinKind::Inner,
        vec![(str_col(0), str_col(0))],
        None,
    );

    let mut sink = CollectSink::new();
    let snap = txn.statement_snapshot(&txn.begin(IsolationLevel::ReadCommitted));
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        storage.as_ref(),
        &txn,
        &snap,
    );
    execute(&PhysicalStatement::Query(plan), &mut ctx, &mut sink).expect("plan runs");
    assert_eq!(sink.rows.len(), 1, "'abc' matches 'ABC'");
    assert_eq!(
        sink.rows[0],
        vec![
            Value::String(vauban_types::SqlString {
                text: "ABC".to_owned()
            }),
            Value::I32(100),
            Value::String(vauban_types::SqlString {
                text: "abc".to_owned()
            }),
            Value::I32(10),
        ]
    );
}
