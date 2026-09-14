//! The `IndexSeek` operator over a hand-built physical plan: an in-memory storage filled
//! through `storage.insert`, an index created through `storage.create_index`, and a plan
//! whose bounds are literal expressions. No SQL text is parsed, bound or planned here.

use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use vauban_binder::{
    BoundExpr, BoundExprKind, ColumnBinding, OutputColumn, OutputSchema, SessionOptions,
};
use vauban_catalog::ColumnId;
use vauban_errors::SqlResult;
use vauban_executor::{ExecContext, RowSet, execute_collect};
use vauban_planner::{KeyRangeExpr, PhysicalPlan, PhysicalStatement};
use vauban_storage::{
    DbId, Direction, IndexId, IndexShape, KeyColumn, KeyRange, MemoryStorage, Row as StorageRow,
    RowId, RowIter, SavepointId, Snapshot, Storage, TableId, TableShape, TxnId,
};
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo, Value};

// ---------------------------------------------------------------------------------------
// A storage that counts its seeks
// ---------------------------------------------------------------------------------------

/// An in-memory storage that counts the calls to `seek` and hands everything else over
/// as is.
struct CountingStorage {
    inner: MemoryStorage,
    seeks: AtomicUsize,
}

impl CountingStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            seeks: AtomicUsize::new(0),
        }
    }

    fn seeks(&self) -> usize {
        self.seeks.load(Ordering::SeqCst)
    }
}

impl Storage for CountingStorage {
    fn create_database(&self, name: &str) -> SqlResult<DbId> {
        self.inner.create_database(name)
    }
    fn drop_database(&self, db: DbId) -> SqlResult<()> {
        self.inner.drop_database(db)
    }
    fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
        self.inner.databases()
    }
    fn create_table(&self, db: DbId, shape: &TableShape) -> SqlResult<TableId> {
        self.inner.create_table(db, shape)
    }
    fn drop_table(&self, table: TableId) -> SqlResult<()> {
        self.inner.drop_table(table)
    }
    fn tables(&self, db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
        self.inner.tables(db)
    }
    fn create_index(&self, table: TableId, def: &IndexShape) -> SqlResult<IndexId> {
        self.inner.create_index(table, def)
    }
    fn drop_index(&self, index: IndexId) -> SqlResult<()> {
        self.inner.drop_index(index)
    }
    fn indexes(&self, table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
        self.inner.indexes(table)
    }
    fn insert(&self, txn: TxnId, table: TableId, row: &StorageRow) -> SqlResult<RowId> {
        self.inner.insert(txn, table, row)
    }
    fn update(&self, txn: TxnId, table: TableId, id: RowId, row: &StorageRow) -> SqlResult<()> {
        self.inner.update(txn, table, id, row)
    }
    fn delete(&self, txn: TxnId, table: TableId, id: RowId) -> SqlResult<()> {
        self.inner.delete(txn, table, id)
    }
    fn get(&self, snap: &Snapshot, table: TableId, id: RowId) -> SqlResult<Option<StorageRow>> {
        self.inner.get(snap, table, id)
    }
    fn scan(&self, snap: &Snapshot, table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
        self.inner.scan(snap, table)
    }
    fn seek(
        &self,
        snap: &Snapshot,
        index: IndexId,
        range: &KeyRange,
        dir: Direction,
    ) -> SqlResult<Box<dyn RowIter + '_>> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        self.inner.seek(snap, index, range, dir)
    }
    fn latest_version(&self, table: TableId, id: RowId) -> SqlResult<Option<(TxnId, StorageRow)>> {
        self.inner.latest_version(table, id)
    }
    fn commit(&self, txn: TxnId) -> SqlResult<()> {
        self.inner.commit(txn)
    }
    fn rollback(&self, txn: TxnId) -> SqlResult<()> {
        self.inner.rollback(txn)
    }
    fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId> {
        self.inner.savepoint(txn)
    }
    fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()> {
        self.inner.rollback_to(txn, sp)
    }
    fn checkpoint(&self) -> SqlResult<()> {
        self.inner.checkpoint()
    }
    fn vacuum(&self, horizon: TxnId) -> SqlResult<()> {
        self.inner.vacuum(horizon)
    }
}

// ---------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------

fn int() -> TypeInfo {
    TypeInfo::new(SqlType::Int, true)
}

fn bigint() -> TypeInfo {
    TypeInfo::new(SqlType::BigInt, true)
}

/// A table of the given column types in a counting storage, an index on the first
/// `key_width` columns, and the manager that opens transactions on it.
struct Fixture {
    counting: Arc<CountingStorage>,
    storage: Arc<dyn Storage>,
    txn: TransactionManager,
    table: TableId,
    index: IndexId,
    columns: Vec<TypeInfo>,
}

impl Fixture {
    fn new(columns: &[TypeInfo], key_width: u16, unique: bool) -> Self {
        let counting = Arc::new(CountingStorage::new());
        let storage: Arc<dyn Storage> = Arc::clone(&counting) as Arc<dyn Storage>;
        let db = storage
            .create_database("mydb")
            .expect("the database is new");
        let shape = TableShape {
            columns: columns.to_vec(),
            clustered_key: None,
        };
        let table = storage.create_table(db, &shape).expect("the table is new");
        let index = storage
            .create_index(
                table,
                &IndexShape {
                    columns: (0..key_width)
                        .map(|column| KeyColumn {
                            column,
                            descending: false,
                        })
                        .collect(),
                    unique,
                    included: Vec::new(),
                },
            )
            .expect("the index is new");
        let txn = TransactionManager::new(Arc::clone(&storage));
        Self {
            counting,
            storage,
            txn,
            table,
            index,
            columns: columns.to_vec(),
        }
    }

    /// A table of `width` `int` columns, with a unique index on column 0.
    fn ints(width: usize) -> Self {
        Self::new(&vec![int(); width], 1, true)
    }

    /// Inserts one row with a transaction of its own, committed.
    fn insert(&self, values: Vec<Value>) {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.storage
            .insert(handle.id, self.table, &StorageRow(values))
            .expect("the row is inserted");
        self.txn.commit(handle).expect("the transaction commits");
    }

    /// Inserts one row of integer values, each typed as its column: `I32` for an `int`
    /// column, `I64` for a `bigint` one, `F64` for a `float` one.
    fn insert_ints(&self, values: &[i64]) {
        let row = values
            .iter()
            .zip(&self.columns)
            .map(|(v, ty)| match ty.ty {
                SqlType::BigInt => Value::I64(*v),
                SqlType::Float => Value::F64(*v as f64),
                _ => Value::I32(i32::try_from(*v).expect("a small value")),
            })
            .collect();
        self.insert(row);
    }

    /// A snapshot of a freshly opened transaction, as `session` takes one per statement.
    fn snapshot(&self) -> Snapshot {
        let handle = self.txn.begin(IsolationLevel::ReadCommitted);
        self.txn.statement_snapshot(&handle)
    }

    /// The bindings of the storage columns `indexes`, in that order.
    fn bindings(&self, indexes: &[usize]) -> Vec<ColumnBinding> {
        indexes
            .iter()
            .map(|index| ColumnBinding {
                column: ColumnId(i32::try_from(*index).expect("a small index") + 1),
                index: *index,
                name: format!("c{index}"),
                ty: self.columns[*index].clone(),
            })
            .collect()
    }

    /// An `IndexSeek` node over the index of the fixture, reading the storage columns
    /// `indexes`, in that order.
    fn seek(&self, range: KeyRangeExpr, direction: Direction, indexes: &[usize]) -> PhysicalPlan {
        let columns = self.bindings(indexes);
        PhysicalPlan::IndexSeek {
            index: self.index,
            range,
            columns: columns.clone(),
            direction,
            schema: schema_of(&columns),
        }
    }

    /// A `TableScan` node over the table of the fixture, reading every column.
    fn scan(&self) -> PhysicalPlan {
        let columns = self.bindings(&(0..self.columns.len()).collect::<Vec<_>>());
        PhysicalPlan::TableScan {
            table: self.table,
            columns: columns.clone(),
            alias: "t".to_owned(),
            schema: schema_of(&columns),
        }
    }

    /// Runs `plan` through `execute_collect` with a context that carries the engine.
    fn run(&self, plan: &PhysicalPlan) -> SqlResult<RowSet> {
        let eval = StaticContext::default();
        let snap = self.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            self.storage.as_ref(),
            &self.txn,
            &snap,
        );
        let stmt = PhysicalStatement::Query(plan.clone());
        execute_collect(&stmt, &mut ctx).map(|(_, set)| set)
    }

    /// The first value of each row `plan` answers, as `i64`.
    fn keys(&self, plan: &PhysicalPlan) -> Vec<i64> {
        self.run(plan)
            .expect("the plan runs")
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::I32(n) => i64::from(*n),
                Value::I64(n) => *n,
                other => panic!("not an integer: {other:?}"),
            })
            .collect()
    }
}

fn schema_of(columns: &[ColumnBinding]) -> OutputSchema {
    OutputSchema {
        columns: columns
            .iter()
            .map(|binding| OutputColumn {
                name: binding.name.clone(),
                ty: binding.ty.clone(),
            })
            .collect(),
    }
}

/// A literal bound of type `ty`.
fn literal(value: Value, ty: TypeInfo) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 1,
    }
}

/// An `int` literal bound.
fn int_literal(n: i32) -> BoundExpr {
    literal(Value::I32(n), TypeInfo::new(SqlType::Int, false))
}

fn point(n: i32) -> KeyRangeExpr {
    KeyRangeExpr::Point(vec![int_literal(n)])
}

fn between(lower: Bound<i32>, upper: Bound<i32>) -> KeyRangeExpr {
    KeyRangeExpr::Between(
        lower.map(|n| vec![int_literal(n)]),
        upper.map(|n| vec![int_literal(n)]),
    )
}

// ---------------------------------------------------------------------------------------
// The ranges
// ---------------------------------------------------------------------------------------

/// An equality on a unique key answers the one row of that key, and no row for a key
/// the table does not hold.
#[test]
fn seek_point_finds_one_row() {
    let fixture = Fixture::ints(2);
    for k in 1..=5 {
        fixture.insert_ints(&[k, k * 10]);
    }
    let found = fixture
        .run(&fixture.seek(point(3), Direction::Forward, &[0, 1]))
        .expect("the seek runs");
    assert_eq!(found.rows, vec![vec![Value::I32(3), Value::I32(30)]]);
    let missing = fixture
        .run(&fixture.seek(point(99), Direction::Forward, &[0, 1]))
        .expect("the seek runs");
    assert!(missing.rows.is_empty());
}

/// An inclusive lower bound keeps its key, an exclusive upper bound drops its key.
#[test]
fn seek_between_inclusive_and_exclusive() {
    let fixture = Fixture::ints(1);
    for k in 1..=5 {
        fixture.insert_ints(&[k]);
    }
    let range = between(Bound::Included(2), Bound::Excluded(4));
    assert_eq!(
        fixture.keys(&fixture.seek(range, Direction::Forward, &[0])),
        vec![2, 3]
    );
    let range = between(Bound::Excluded(2), Bound::Included(4));
    assert_eq!(
        fixture.keys(&fixture.seek(range, Direction::Forward, &[0])),
        vec![3, 4]
    );
    let range = between(Bound::Unbounded, Bound::Excluded(3));
    assert_eq!(
        fixture.keys(&fixture.seek(range, Direction::Forward, &[0])),
        vec![1, 2]
    );
}

/// `Full` reads the rows of the whole index, as many as the scan of the table reads.
#[test]
fn seek_full_range_gives_every_row_of_the_index() {
    let fixture = Fixture::ints(2);
    for k in [5, 1, 4, 2, 3] {
        fixture.insert_ints(&[k, k * 10]);
    }
    let scanned = fixture.run(&fixture.scan()).expect("the scan runs");
    let plan = fixture.seek(KeyRangeExpr::Full, Direction::Forward, &[0, 1]);
    let sought = fixture.run(&plan).expect("the seek runs");
    assert_eq!(sought.rows.len(), scanned.rows.len());
    assert_eq!(sought.rows.len(), 5);
    // The seek reads in key order, whatever the order the rows were written in.
    assert_eq!(fixture.keys(&plan), vec![1, 2, 3, 4, 5]);
}

/// The same range in `Backward` answers the same keys, in the reverse order.
#[test]
fn seek_backward_reverses_the_order() {
    let fixture = Fixture::ints(1);
    for k in [5, 1, 4, 2, 3] {
        fixture.insert_ints(&[k]);
    }
    let range = || between(Bound::Included(2), Bound::Included(4));
    let forward = fixture.keys(&fixture.seek(range(), Direction::Forward, &[0]));
    let backward = fixture.keys(&fixture.seek(range(), Direction::Backward, &[0]));
    assert_eq!(forward, vec![2, 3, 4]);
    let mut reversed = forward;
    reversed.reverse();
    assert_eq!(backward, reversed);
}

// ---------------------------------------------------------------------------------------
// The ranges that read no row
// ---------------------------------------------------------------------------------------

/// A `NULL` bound answers no row and calls no seek, for a point as for either side of a
/// range.
#[test]
fn seek_null_bound_gives_no_row() {
    let fixture = Fixture::ints(1);
    for k in 1..=3 {
        fixture.insert_ints(&[k]);
    }
    let null = || literal(Value::Null, int());
    let ranges = [
        KeyRangeExpr::Point(vec![null()]),
        KeyRangeExpr::Between(Bound::Excluded(vec![null()]), Bound::Unbounded),
        KeyRangeExpr::Between(
            Bound::Included(vec![null()]),
            Bound::Included(vec![int_literal(3)]),
        ),
        KeyRangeExpr::Between(
            Bound::Included(vec![int_literal(1)]),
            Bound::Included(vec![null()]),
        ),
    ];
    for range in ranges {
        let set = fixture
            .run(&fixture.seek(range.clone(), Direction::Forward, &[0]))
            .expect("the seek runs");
        assert!(set.rows.is_empty(), "{range:?}");
        assert_eq!(set.schema.columns.len(), 1, "{range:?}");
    }
    assert_eq!(fixture.counting.seeks(), 0);
    // The counter does count: a point that reads a row goes through the storage.
    assert_eq!(
        fixture.keys(&fixture.seek(point(2), Direction::Forward, &[0])),
        vec![2]
    );
    assert_eq!(fixture.counting.seeks(), 1);
}

/// A lower bound above the upper bound answers no row and calls no seek.
#[test]
fn seek_empty_range_gives_no_row() {
    let fixture = Fixture::ints(1);
    for k in 1..=5 {
        fixture.insert_ints(&[k]);
    }
    let set = fixture
        .run(&fixture.seek(
            between(Bound::Included(4), Bound::Included(2)),
            Direction::Forward,
            &[0],
        ))
        .expect("the seek runs");
    assert!(set.rows.is_empty());
    assert_eq!(fixture.counting.seeks(), 0);
    // The point without itself is empty too; the point with itself is one row.
    let set = fixture
        .run(&fixture.seek(
            between(Bound::Excluded(3), Bound::Included(3)),
            Direction::Forward,
            &[0],
        ))
        .expect("the seek runs");
    assert!(set.rows.is_empty());
    assert_eq!(fixture.counting.seeks(), 0);
    let range = between(Bound::Included(3), Bound::Included(3));
    assert_eq!(
        fixture.keys(&fixture.seek(range, Direction::Forward, &[0])),
        vec![3]
    );
    assert_eq!(fixture.counting.seeks(), 1);
}

// ---------------------------------------------------------------------------------------
// The type of a bound and the columns of the answer
// ---------------------------------------------------------------------------------------

/// A bound of another type than its key column is converted to that type: an `int`
/// bound on a `bigint` key finds the row, and an `int` bound on a `float` key too. The
/// storage refuses an integer on a `float` column, which is the shape that separates
/// the conversion from its absence.
#[test]
fn seek_bound_is_converted_to_the_key_type() {
    let on_bigint = Fixture::new(&[bigint(), int()], 1, true);
    for k in 1..=3 {
        on_bigint.insert_ints(&[k, k * 10]);
    }
    let plan = on_bigint.seek(point(2), Direction::Forward, &[0, 1]);
    let set = on_bigint.run(&plan).expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I64(2), Value::I32(20)]]);

    let on_float = Fixture::new(&[TypeInfo::new(SqlType::Float, true), int()], 1, true);
    for k in 1..=3 {
        on_float.insert_ints(&[k, k * 10]);
    }
    let plan = on_float.seek(point(2), Direction::Forward, &[1]);
    let set = on_float.run(&plan).expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I32(20)]]);
    let plan = on_float.seek(
        between(Bound::Excluded(1), Bound::Unbounded),
        Direction::Backward,
        &[1],
    );
    let set = on_float.run(&plan).expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I32(30)], vec![Value::I32(20)]]);
}

/// A bound the key type cannot hold is the conversion error, on the line of the bound.
#[test]
fn seek_bound_that_does_not_convert_is_the_conversion_error() {
    let fixture = Fixture::ints(1);
    fixture.insert_ints(&[1]);
    let text = literal(
        Value::String(vauban_types::SqlString {
            text: "abc".to_owned(),
        }),
        TypeInfo::new(SqlType::VarChar(vauban_types::Len::Fixed(3)), false),
    );
    let mut plan = fixture.seek(KeyRangeExpr::Point(vec![text]), Direction::Forward, &[0]);
    if let PhysicalPlan::IndexSeek { range, .. } = &mut plan
        && let KeyRangeExpr::Point(exprs) = range
    {
        exprs[0].line = 7;
    }
    let error = fixture.run(&plan).expect_err("'abc' is no int");
    assert_eq!(error.number, 245);
    assert_eq!(error.line, 7);
    assert_eq!(fixture.counting.seeks(), 0);
}

/// `columns` decides which values of the storage row come out, and in which order: a
/// node reading column 1 alone answers rows of one value, and its schema is the schema
/// of the node.
#[test]
fn seek_projects_the_plan_columns() {
    let fixture = Fixture::ints(3);
    fixture.insert_ints(&[1, 10, 100]);
    fixture.insert_ints(&[2, 20, 200]);
    let plan = fixture.seek(KeyRangeExpr::Full, Direction::Forward, &[1]);
    let set = fixture.run(&plan).expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I32(10)], vec![Value::I32(20)]]);
    assert_eq!(set.schema.columns.len(), 1);
    assert_eq!(set.schema.columns[0].name, "c1");
    // Columns 2 then 0: the answer follows the node, not the storage.
    let plan = fixture.seek(point(2), Direction::Forward, &[2, 0]);
    let set = fixture.run(&plan).expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I32(200), Value::I32(2)]]);
}

// ---------------------------------------------------------------------------------------
// A composite key
// ---------------------------------------------------------------------------------------

/// On a two-column key, a prefix bound serves the whole group of the prefix and a full
/// point serves one key.
#[test]
fn seek_prefix_of_a_composite_key() {
    let fixture = Fixture::new(&[int(), int()], 2, false);
    for (a, b) in [(1, 1), (1, 2), (2, 1), (2, 2), (2, 3), (3, 1)] {
        fixture.insert_ints(&[a, b]);
    }
    let group = KeyRangeExpr::Between(
        Bound::Included(vec![int_literal(2)]),
        Bound::Included(vec![int_literal(2)]),
    );
    let set = fixture
        .run(&fixture.seek(group, Direction::Forward, &[0, 1]))
        .expect("the seek runs");
    assert_eq!(
        set.rows,
        vec![
            vec![Value::I32(2), Value::I32(1)],
            vec![Value::I32(2), Value::I32(2)],
            vec![Value::I32(2), Value::I32(3)],
        ]
    );
    let upper_half = KeyRangeExpr::Between(
        Bound::Excluded(vec![int_literal(2), int_literal(1)]),
        Bound::Included(vec![int_literal(2)]),
    );
    let set = fixture
        .run(&fixture.seek(upper_half, Direction::Forward, &[1]))
        .expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I32(2)], vec![Value::I32(3)]]);
    let point = KeyRangeExpr::Point(vec![int_literal(2), int_literal(3)]);
    let set = fixture
        .run(&fixture.seek(point, Direction::Forward, &[0, 1]))
        .expect("the seek runs");
    assert_eq!(set.rows, vec![vec![Value::I32(2), Value::I32(3)]]);
}

// ---------------------------------------------------------------------------------------
// The broken preconditions
// ---------------------------------------------------------------------------------------

/// An index no table holds is the internal error 50000, before any seek.
#[test]
fn seek_on_an_unknown_index_is_a_bug() {
    let fixture = Fixture::ints(1);
    let mut plan = fixture.seek(KeyRangeExpr::Full, Direction::Forward, &[0]);
    if let PhysicalPlan::IndexSeek { index, .. } = &mut plan {
        *index = IndexId(999);
    }
    let error = fixture.run(&plan).expect_err("no such index");
    assert_eq!(error.number, 50000);
    assert!(error.message.contains("999"), "message: {}", error.message);
    assert_eq!(fixture.counting.seeks(), 0);
}

/// A scalar context is the internal error 50000, not a panic and not an empty answer.
#[test]
fn seek_without_an_engine_is_a_bug() {
    let fixture = Fixture::ints(1);
    let eval = StaticContext::default();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
    let stmt = PhysicalStatement::Query(fixture.seek(KeyRangeExpr::Full, Direction::Forward, &[0]));
    let error = execute_collect(&stmt, &mut ctx).expect_err("a seek needs storage");
    assert_eq!(error.number, 50000);
}

/// A `columns` and a `schema` of different widths is a bug of the planner, reported as
/// 50000 before the index is opened.
#[test]
fn seek_with_a_mismatched_schema_is_a_bug() {
    let fixture = Fixture::ints(2);
    let mut plan = fixture.seek(KeyRangeExpr::Full, Direction::Forward, &[0, 1]);
    if let PhysicalPlan::IndexSeek { schema, .. } = &mut plan {
        schema.columns.pop();
    }
    let error = fixture.run(&plan).expect_err("the arities disagree");
    assert_eq!(error.number, 50000);
    assert_eq!(fixture.counting.seeks(), 0);
}
