//! The row identifier in the row stream: a scan or a seek asked for it appends it after the
//! visible values of each row, and the filters, joins, sorts and projections above hand it
//! over, so that each row of the answer names the stored rows it was built from.
//!
//! Each test builds its tables through `storage`, keeps the `RowId` each insert answers,
//! and compares it with the tail of the rows the plan produces. No SQL text is parsed,
//! bound or planned here.

use std::sync::Arc;

use vauban_binder::{
    BoundExpr, BoundExprKind, BoundProjection, ColumnBinding, CompareOp, LockHints, OutputColumn,
    OutputSchema, SessionOptions, SortKey,
};
use vauban_catalog::ColumnId;
use vauban_executor::{ExecContext, execute_collect};
use vauban_planner::{
    KeyRangeExpr, PhysicalJoinKind, PhysicalPlan, PhysicalStatement, row_id_binding,
};
use vauban_storage::{
    Direction, IndexId, IndexShape, KeyColumn, MemoryStorage, RowId, Storage, TableId, TableShape,
};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo, Value};

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn bit() -> TypeInfo {
    TypeInfo::new(SqlType::Bit, true)
}

/// The binding of the storage column `index`, read at depth 0.
fn binding(index: usize) -> ColumnBinding {
    ColumnBinding {
        column: ColumnId(i32::try_from(index).expect("a small index") + 1),
        index,
        name: format!("c{index}"),
        ty: int(),
        depth: 0,
    }
}

/// The value at position `index` of the row of the node below.
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

fn compare(op: CompareOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: bit(),
        line: 1,
    }
}

fn schema_of(width: usize) -> OutputSchema {
    OutputSchema {
        columns: (0..width)
            .map(|index| OutputColumn {
                name: format!("c{index}"),
                ty: int(),
            })
            .collect(),
    }
}

fn concat_schema(left: &PhysicalPlan, right: &PhysicalPlan) -> OutputSchema {
    let mut columns = left.schema().columns.clone();
    columns.extend(right.schema().columns.iter().cloned());
    OutputSchema { columns }
}

/// The value the tail of a row holds for `id`: a `bigint` of the same 64 bits.
fn rid(id: RowId) -> Value {
    Value::I64(i64::from_ne_bytes(id.0.to_ne_bytes()))
}

fn ints(values: &[i32]) -> Vec<Value> {
    values.iter().map(|v| Value::I32(*v)).collect()
}

/// A storage with tables of `int` columns, and the manager that opens transactions on it.
struct Db {
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    db: vauban_storage::DbId,
}

impl Db {
    fn new() -> Self {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self { storage, txn, db }
    }

    fn table(&self, width: usize) -> TableId {
        let shape = TableShape {
            columns: vec![int(); width],
            clustered_key: None,
        };
        self.storage
            .create_table(self.db, &shape)
            .expect("the table is new")
    }

    /// A non-unique index on the first column of `table`.
    fn index(&self, table: TableId) -> IndexId {
        self.storage
            .create_index(
                table,
                &IndexShape {
                    columns: vec![KeyColumn {
                        column: 0,
                        descending: false,
                    }],
                    unique: false,
                    included: Vec::new(),
                },
            )
            .expect("the index is new")
    }

    /// Inserts one committed row and answers the identifier the storage gave it.
    fn insert(&self, table: TableId, values: &[i32]) -> RowId {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let id = self
            .storage
            .insert(handle.id, table, &vauban_storage::Row(ints(values)))
            .expect("the row is inserted");
        self.txn.commit(handle).expect("the transaction commits");
        id
    }

    /// Inserts then deletes `count` rows of `table`, committed, so that the next row of
    /// `table` gets an identifier no row of a fresh table gets.
    fn burn_ids(&self, table: TableId, width: usize, count: usize) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        for _ in 0..count {
            let id = self
                .storage
                .insert(
                    handle.id,
                    table,
                    &vauban_storage::Row(ints(&vec![-1; width])),
                )
                .expect("the row is inserted");
            self.storage
                .delete(handle.id, table, id)
                .expect("the row is deleted");
        }
        self.txn.commit(handle).expect("the transaction commits");
    }

    /// The rows `plan` hands to the sink.
    fn run(&self, plan: &PhysicalPlan) -> Vec<Vec<Value>> {
        let eval = StaticContext::default();
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        let snap = self.txn.statement_snapshot(&handle);
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let (_, set) = execute_collect(&PhysicalStatement::Query(plan.clone()), &mut ctx)
            .expect("the plan runs");
        set.rows
    }
}

/// A scan of the first `width` columns of `table`, with the row identifier when `row_id`.
fn scan(table: TableId, width: usize, row_id: bool) -> PhysicalPlan {
    let mut columns: Vec<ColumnBinding> = (0..width).map(binding).collect();
    if row_id {
        columns.push(row_id_binding(table).expect("a small table identifier"));
    }
    PhysicalPlan::TableScan {
        table,
        columns,
        alias: format!("t{table}"),
        schema: schema_of(width),
        hints: LockHints::default(),
    }
}

/// The row identifiers of tables `a`, `b` and `c` cross a hash join, a nested-loop join
/// over an index seek, a filter and a sort, and each one names the stored row whose values
/// sit in the same output row.
#[test]
fn the_row_id_of_a_joined_table_crosses_the_joins_the_filter_and_the_sort() {
    let db = Db::new();
    let a = db.table(2);
    let b = db.table(2);
    let c = db.table(1);
    let c_index = db.index(c);
    // Distinct identifiers from one table to the other: an identifier read in the table
    // of another tail finds no row there.
    db.burn_ids(b, 2, 10);
    db.burn_ids(c, 1, 20);
    let a_ids: Vec<RowId> = [[1, 10], [2, 20], [3, 30]]
        .iter()
        .map(|row| db.insert(a, row))
        .collect();
    let b_ids: Vec<RowId> = [[1, 100], [2, 200], [3, 300], [4, 400]]
        .iter()
        .map(|row| db.insert(b, row))
        .collect();
    let c_ids: Vec<RowId> = [[1], [2], [3]]
        .iter()
        .map(|row| db.insert(c, row))
        .collect();

    // a JOIN b ON a.k = b.k, hashed on b: a.k, a.v, b.k, b.w.
    let probe = scan(a, 2, true);
    let build = scan(b, 2, true);
    let hash_schema = concat_schema(&probe, &build);
    let hash = PhysicalPlan::HashJoin {
        build: Box::new(build),
        probe: Box::new(probe),
        kind: PhysicalJoinKind::Inner,
        keys: vec![(col(0), col(2))],
        residual: None,
        schema: hash_schema,
    };
    // … JOIN c ON c.k = a.k, by a seek on c's index for each outer row.
    let seek = PhysicalPlan::IndexSeek {
        index: c_index,
        range: KeyRangeExpr::Point(vec![col(0)]),
        columns: vec![
            binding(0),
            row_id_binding(c).expect("a small table identifier"),
        ],
        direction: Direction::Forward,
        schema: schema_of(1),
        hints: LockHints::default(),
    };
    let nl_schema = concat_schema(&hash, &seek);
    let nl = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(hash),
        inner: Box::new(seek),
        kind: PhysicalJoinKind::Inner,
        on: None,
        schema: nl_schema,
    };
    // … WHERE a.v > 10 ORDER BY a.v DESC.
    let plan = PhysicalPlan::Sort {
        input: Box::new(PhysicalPlan::Filter {
            input: Box::new(nl),
            predicate: compare(CompareOp::Gt, col(1), lit(10)),
        }),
        keys: vec![SortKey {
            expr: col(1),
            desc: true,
            collation: None,
        }],
    };
    assert_eq!(plan.row_id_tables(), vec![a, b, c]);
    for id in a_ids.iter().chain(&b_ids) {
        assert!(!c_ids.contains(id), "{id:?}");
    }
    assert!(a_ids.iter().all(|id| !b_ids.contains(id)));

    let rows = db.run(&plan);
    let expected = |k: usize| {
        let mut row = ints(&[
            i32::try_from(k + 1).expect("small"),
            i32::try_from((k + 1) * 10).expect("small"),
            i32::try_from(k + 1).expect("small"),
            i32::try_from((k + 1) * 100).expect("small"),
            i32::try_from(k + 1).expect("small"),
        ]);
        row.extend([rid(a_ids[k]), rid(b_ids[k]), rid(c_ids[k])]);
        row
    };
    assert_eq!(rows, vec![expected(2), expected(1)]);

    // Each identifier reads back the row whose values precede it.
    let storage = db.storage.as_ref();
    for row in &rows {
        for (offset, (table, width, first)) in
            [(a, 2, 0), (b, 2, 2), (c, 1, 4)].into_iter().enumerate()
        {
            let Value::I64(bits) = row[5 + offset] else {
                panic!("a row identifier is a bigint: {row:?}");
            };
            let id = RowId(u64::from_ne_bytes(bits.to_ne_bytes()));
            let (_, stored) = storage
                .latest_version(table, id)
                .expect("the storage answers")
                .expect("the row exists");
            assert_eq!(stored.0[..width], row[first..first + width]);
        }
    }
}

/// A hash join of two scans that hold no row-identifier binding answers rows of the width
/// of its schema: the row stream is unchanged when the plan did not ask.
#[test]
fn a_scan_asked_nothing_keeps_its_width() {
    let db = Db::new();
    let a = db.table(2);
    let b = db.table(2);
    db.insert(a, &[1, 10]);
    db.insert(b, &[1, 100]);
    let probe = scan(a, 2, false);
    let build = scan(b, 2, false);
    let schema = concat_schema(&probe, &build);
    let plan = PhysicalPlan::HashJoin {
        build: Box::new(build),
        probe: Box::new(probe),
        kind: PhysicalJoinKind::Inner,
        keys: vec![(col(0), col(2))],
        residual: None,
        schema,
    };
    assert_eq!(plan.row_id_count(), 0);
    assert_eq!(db.run(&plan), vec![ints(&[1, 10, 1, 100])]);
}

/// A scan asked for the row identifier answers it after the columns it reads.
#[test]
fn a_scan_asked_for_its_row_id_appends_it() {
    let db = Db::new();
    let a = db.table(3);
    let first = db.insert(a, &[1, 2, 3]);
    let second = db.insert(a, &[4, 5, 6]);
    let rows = db.run(&scan(a, 2, true));
    assert_eq!(
        rows,
        vec![
            vec![Value::I32(1), Value::I32(2), rid(first)],
            vec![Value::I32(4), Value::I32(5), rid(second)],
        ]
    );
}

/// A seek asked for the row identifier answers it after the columns it reads.
#[test]
fn a_seek_asked_for_its_row_id_appends_it() {
    let db = Db::new();
    let a = db.table(2);
    let index = db.index(a);
    db.insert(a, &[1, 10]);
    let wanted = db.insert(a, &[2, 20]);
    let plan = PhysicalPlan::IndexSeek {
        index,
        range: KeyRangeExpr::Point(vec![lit(2)]),
        columns: vec![
            binding(1),
            row_id_binding(a).expect("a small table identifier"),
        ],
        direction: Direction::Forward,
        schema: schema_of(1),
        hints: LockHints::default(),
    };
    assert_eq!(db.run(&plan), vec![vec![Value::I32(20), rid(wanted)]]);
}

/// A projection computes its values from the visible ones and hands the row identifier
/// over after them.
#[test]
fn a_projection_keeps_the_row_id_after_its_values() {
    let db = Db::new();
    let a = db.table(2);
    let id = db.insert(a, &[7, 70]);
    let plan = PhysicalPlan::Project {
        input: Box::new(scan(a, 2, true)),
        exprs: vec![BoundProjection {
            expr: col(1),
            name: "v".to_owned(),
        }],
        schema: schema_of(1),
    };
    assert_eq!(plan.row_id_tables(), vec![a]);
    assert_eq!(db.run(&plan), vec![vec![Value::I32(70), rid(id)]]);
}

/// A left join pads the side it has no row for with `NULL`, its row identifier included,
/// in a nested-loop join and in a hash join alike.
#[test]
fn a_left_join_pads_the_row_id_of_the_missing_side() {
    let db = Db::new();
    let a = db.table(1);
    let b = db.table(1);
    db.burn_ids(b, 1, 10);
    let a1 = db.insert(a, &[1]);
    let a2 = db.insert(a, &[2]);
    let b1 = db.insert(b, &[1]);
    assert_ne!(a1, b1);
    let on = compare(CompareOp::Eq, col(0), col(1));
    let nested = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(a, 1, true)),
        inner: Box::new(scan(b, 1, true)),
        kind: PhysicalJoinKind::Left,
        on: Some(on),
        schema: schema_of(2),
    };
    let hashed = PhysicalPlan::HashJoin {
        build: Box::new(scan(b, 1, true)),
        probe: Box::new(scan(a, 1, true)),
        kind: PhysicalJoinKind::Left,
        keys: vec![(col(0), col(1))],
        residual: None,
        schema: schema_of(2),
    };
    let expected = vec![
        vec![Value::I32(1), Value::I32(1), rid(a1), rid(b1)],
        vec![Value::I32(2), Value::Null, rid(a2), Value::Null],
    ];
    for plan in [nested, hashed] {
        assert_eq!(plan.row_id_tables(), vec![a, b]);
        assert_eq!(db.run(&plan), expected);
    }
}

/// A hash join where one side alone carries a row identifier, as the input of an
/// `UPDATE … FROM` does: the visible values of both sides keep their positions and the one
/// identifier comes last, whether the marked side is probed or built.
#[test]
fn a_hash_join_with_one_marked_side_keeps_the_visible_positions() {
    let db = Db::new();
    let a = db.table(2);
    let b = db.table(2);
    db.burn_ids(b, 2, 10);
    let a1 = db.insert(a, &[1, 10]);
    let b1 = db.insert(b, &[1, 100]);
    let join = |probe: PhysicalPlan, build: PhysicalPlan| {
        let schema = concat_schema(&probe, &build);
        PhysicalPlan::HashJoin {
            build: Box::new(build),
            probe: Box::new(probe),
            kind: PhysicalJoinKind::Inner,
            keys: vec![(col(0), col(2))],
            residual: None,
            schema,
        }
    };
    let probe_marked = join(scan(a, 2, true), scan(b, 2, false));
    assert_eq!(probe_marked.row_id_tables(), vec![a]);
    let mut expected = ints(&[1, 10, 1, 100]);
    expected.push(rid(a1));
    assert_eq!(db.run(&probe_marked), vec![expected]);

    let build_marked = join(scan(a, 2, false), scan(b, 2, true));
    assert_eq!(build_marked.row_id_tables(), vec![b]);
    let mut expected = ints(&[1, 10, 1, 100]);
    expected.push(rid(b1));
    assert_eq!(db.run(&build_marked), vec![expected]);
}

/// A semi-join hands over the outer row, its row identifier included, and none of the
/// inner side.
#[test]
fn a_semi_join_hands_over_the_outer_row_id() {
    let db = Db::new();
    let a = db.table(1);
    let b = db.table(1);
    let a1 = db.insert(a, &[1]);
    db.insert(a, &[2]);
    db.insert(b, &[1]);
    let plan = PhysicalPlan::NestedLoopJoin {
        outer: Box::new(scan(a, 1, true)),
        inner: Box::new(scan(b, 1, true)),
        kind: PhysicalJoinKind::Semi,
        on: Some(compare(CompareOp::Eq, col(0), col(1))),
        schema: schema_of(1),
    };
    assert_eq!(plan.row_id_tables(), vec![a]);
    assert_eq!(db.run(&plan), vec![vec![Value::I32(1), rid(a1)]]);
}

/// A column after a row-identifier binding would leave the position the plan gave it: the
/// scan refuses the node with the internal error 50000.
#[test]
fn a_column_after_a_row_id_binding_is_a_bug() {
    let db = Db::new();
    let a = db.table(2);
    db.insert(a, &[1, 2]);
    let plan = PhysicalPlan::TableScan {
        table: a,
        columns: vec![
            binding(0),
            row_id_binding(a).expect("a small table identifier"),
            binding(1),
        ],
        alias: "a".to_owned(),
        schema: schema_of(2),
        hints: LockHints::default(),
    };
    let eval = StaticContext::default();
    let handle = db.txn.begin(IsolationLevel::ReadCommitted);
    let snap = db.txn.statement_snapshot(&handle);
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
        db.storage.as_ref(),
        &db.txn,
        &snap,
    );
    let error = execute_collect(&PhysicalStatement::Query(plan), &mut ctx)
        .expect_err("the binding order is refused");
    assert_eq!(error.number, 50000);
}
