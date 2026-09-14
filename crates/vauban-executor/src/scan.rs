//! Reading the rows of one table: [`LogicalPlan::Scan`](vauban_binder::LogicalPlan::Scan).
//!
//! # Materialised, like the rest of the plan
//!
//! [`execute_scan`] pulls the whole iterator of
//! [`Storage::scan`](vauban_storage::Storage::scan) into a [`RowSet`] before answering, as
//! `plan.rs` does for the other nodes of the bound plan. Streaming through an `Operator`
//! trait is not written; the shape of [`crate::execute`] does not change when it lands.
//!
//! # Which value of the storage row each output column takes
//!
//! A `Scan` carries a [`ColumnBinding`] per column it produces, and the `index` of that
//! binding is **the position of the value in the row `storage` hands out** — the `ordinal`
//! of the column, which `binder::catalog_view::columns_of` copies from the catalogue
//! (`columns_are_indexed_by_ordinal_not_by_identifier`, in `vauban-binder`). The `i`-th
//! column of the answer is therefore `row.0[columns[i].index]`, not `row.0[i]`, so a `Scan`
//! may read a subset of the storage columns, in its own order. The two readings — `index` as
//! a source position, `index` as the output position `i` — agree whenever a `Scan` lists the
//! storage columns in storage order, which is why the test that separates them
//! (`scan::tests::scan_projects_the_columns_it_names`) reads column 2 then column 0 of a
//! three-column table and asserts the values, not just the arity.
//!
//! The [`ColumnId`](vauban_catalog::ColumnId) a binding also carries is not consulted here:
//! it identifies the column for the catalogue and survives the drop of a column before it,
//! which makes it useless as a position.

use vauban_binder::{ColumnBinding, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::TableId;

use crate::context::ExecContext;
use crate::row::RowSet;

/// Reads the rows of `table` visible to the snapshot of `ctx`, keeping the columns
/// `columns` names, in that order.
///
/// The schema of the answer is the `schema` the binder put in the node — the same thing
/// [`LogicalPlan::schema`](vauban_binder::LogicalPlan::schema) answers — so an empty table
/// still tells the client the shape of the columns it has no row for
/// (`scan::tests::scan_empty_table_zero_rows`).
///
/// # Errors
///
/// Whatever [`Storage::scan`](vauban_storage::Storage::scan) raises, unchanged: an unknown
/// table is its `InternalError::Bug`, an I/O failure its `InternalError::Io`, either at
/// creation or as an item of the iterator, and an `Err` item ends the iteration. The
/// internal error 50000 of this file covers three broken preconditions, each of which takes
/// a bug of the caller or of the binder: a context with no engine
/// ([`ExecContext::storage`]), a `schema` and a `columns` of different widths, and an
/// `index` past the end of the row `storage` handed out.
pub(crate) fn execute_scan(
    table: TableId,
    columns: &[ColumnBinding],
    schema: &OutputSchema,
    ctx: &ExecContext<'_>,
) -> SqlResult<RowSet> {
    if schema.columns.len() != columns.len() {
        return Err(bug(&format!(
            "execute_scan: the node publishes {} column(s) and reads {}",
            schema.columns.len(),
            columns.len()
        )));
    }
    let storage = ctx.storage()?;
    let snap = ctx.snapshot()?;
    let mut rows = Vec::new();
    for item in storage.scan(snap, table)? {
        let (_, source) = item?;
        let mut row = Vec::with_capacity(columns.len());
        for binding in columns {
            let value = source.0.get(binding.index).ok_or_else(|| {
                bug(&format!(
                    "execute_scan: column `{}` is at index {} of a row of {} value(s)",
                    binding.name,
                    binding.index,
                    source.0.len()
                ))
            })?;
            row.push(value.clone());
        }
        rows.push(row);
    }
    Ok(RowSet {
        schema: schema.clone(),
        rows,
    })
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_binder::{LockHints, LogicalPlan, OutputColumn, SessionOptions};
    use vauban_catalog::ColumnId;
    use vauban_storage::{MemoryStorage, Row, Snapshot, Storage, TableShape};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo, Value};

    /// A three-column `int` table in a fresh in-memory storage, and the manager that opens
    /// transactions on it.
    struct Fixture {
        storage: Arc<dyn Storage>,
        txn: TransactionManager,
        table: TableId,
    }

    impl Fixture {
        fn new(width: usize) -> Self {
            let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
            let db = storage
                .create_database("mydb")
                .expect("the database is new");
            let shape = TableShape {
                columns: vec![TypeInfo::new(SqlType::Int, true); width],
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

        /// Inserts one row of `int` values with a transaction of its own, committed.
        fn insert(&self, values: &[i64]) {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            let row = Row(values.iter().map(|v| Value::I64(*v)).collect());
            self.storage
                .insert(handle.id, self.table, &row)
                .expect("the row is inserted");
            self.txn.commit(handle).expect("the transaction commits");
        }

        /// A snapshot of a freshly opened transaction, as `session` takes one per statement.
        fn snapshot(&self) -> Snapshot {
            let handle = self.txn.begin(IsolationLevel::ReadCommitted);
            self.txn.statement_snapshot(&handle)
        }
    }

    /// A `Scan` node reading the storage columns `indexes`, in that order.
    fn scan_node(table: TableId, indexes: &[usize]) -> LogicalPlan {
        let columns: Vec<ColumnBinding> = indexes
            .iter()
            .map(|index| ColumnBinding {
                // `column_id` is 1-based in `sys.columns`; `execute_scan` reads `index`.
                column: ColumnId(i32::try_from(*index).expect("a small index") + 1),
                index: *index,
                name: format!("c{index}"),
                ty: TypeInfo::new(SqlType::Int, true),
            })
            .collect();
        let schema = OutputSchema {
            columns: columns
                .iter()
                .map(|binding| OutputColumn {
                    name: binding.name.clone(),
                    ty: binding.ty.clone(),
                })
                .collect(),
        };
        LogicalPlan::Scan {
            table,
            columns,
            alias: "t".to_owned(),
            schema,
            hints: LockHints::default(),
        }
    }

    /// Runs `plan` through [`crate::plan::execute_plan`] with a context that carries the
    /// engine of `fixture`.
    fn run(fixture: &Fixture, plan: &LogicalPlan) -> SqlResult<RowSet> {
        let eval = StaticContext::default();
        let snap = fixture.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            fixture.storage.as_ref(),
            &fixture.txn,
            &snap,
        );
        crate::plan::execute_plan(plan, &mut ctx)
    }

    /// An empty table answers no row, and still publishes the schema of the node.
    #[test]
    fn scan_empty_table_zero_rows() {
        let fixture = Fixture::new(2);
        let plan = scan_node(fixture.table, &[0, 1]);
        let set = run(&fixture, &plan).expect("the scan runs");
        assert!(set.rows.is_empty());
        assert_eq!(set.schema.columns.len(), plan.schema().columns.len());
        let names: Vec<&str> = set
            .schema
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names, ["c0", "c1"]);
        assert_eq!(set.schema.columns[0].ty, TypeInfo::new(SqlType::Int, true));
    }

    /// A row written through `storage.insert` is read back by the scan, values included.
    #[test]
    fn scan_sees_inserted_row() {
        let fixture = Fixture::new(2);
        fixture.insert(&[7, 8]);
        let set = run(&fixture, &scan_node(fixture.table, &[0, 1])).expect("the scan runs");
        assert_eq!(set.rows.len(), 1);
        assert_eq!(set.rows[0], vec![Value::I64(7), Value::I64(8)]);
    }

    /// Two rows come out as two rows, and the count is what `session` puts in the `DONE`.
    #[test]
    fn scan_sees_every_committed_row() {
        let fixture = Fixture::new(2);
        fixture.insert(&[1, 2]);
        fixture.insert(&[3, 4]);
        let set = run(&fixture, &scan_node(fixture.table, &[0, 1])).expect("the scan runs");
        assert_eq!(set.rows.len(), 2);
    }

    /// The vector that separates `index` as a source position from `index` as the output
    /// position: the node reads columns 2 then 0 of a three-column row, and the answer is
    /// two values, in that order.
    #[test]
    fn scan_projects_the_columns_it_names() {
        let fixture = Fixture::new(3);
        fixture.insert(&[10, 20, 30]);
        let set = run(&fixture, &scan_node(fixture.table, &[2, 0])).expect("the scan runs");
        assert_eq!(set.rows.len(), 1);
        assert_eq!(set.rows[0], vec![Value::I64(30), Value::I64(10)]);
        // Under the other reading (`index` is the output position `i`), the answer would
        // be the first two values of the row, `[10, 20]`.
        assert_ne!(set.rows[0], vec![Value::I64(10), Value::I64(20)]);
    }

    /// A row written by a transaction that is still open is invisible to another one, which
    /// is the visibility rule of `storage` and not something this file implements.
    #[test]
    fn scan_hides_an_uncommitted_row() {
        let fixture = Fixture::new(1);
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        fixture
            .storage
            .insert(handle.id, fixture.table, &Row(vec![Value::I64(1)]))
            .expect("the row is inserted");
        let set = run(&fixture, &scan_node(fixture.table, &[0])).expect("the scan runs");
        assert!(set.rows.is_empty());
        fixture.txn.commit(handle).expect("the transaction commits");
        let after = run(&fixture, &scan_node(fixture.table, &[0])).expect("the scan runs");
        assert_eq!(after.rows.len(), 1);
    }

    /// A scalar context is the internal error 50000, not a panic and not an empty answer.
    #[test]
    fn scan_without_an_engine_is_a_bug() {
        let fixture = Fixture::new(1);
        let eval = StaticContext::default();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default());
        let error = crate::plan::execute_plan(&scan_node(fixture.table, &[0]), &mut ctx)
            .expect_err("a scan needs storage");
        assert_eq!(error.number, 50000);
    }

    /// A `columns` and a `schema` of different widths is a bug of the binder, reported as
    /// 50000 before the table is even opened.
    #[test]
    fn scan_with_a_mismatched_schema_is_a_bug() {
        let fixture = Fixture::new(2);
        let LogicalPlan::Scan {
            table,
            columns,
            alias,
            mut schema,
            hints,
        } = scan_node(fixture.table, &[0, 1])
        else {
            unreachable!("scan_node builds a Scan")
        };
        schema.columns.pop();
        let plan = LogicalPlan::Scan {
            table,
            columns,
            alias,
            schema,
            hints,
        };
        let error = run(&fixture, &plan).expect_err("the arities disagree");
        assert_eq!(error.number, 50000);
    }

    /// An `index` past the end of the storage row is 50000 as well, and the message names
    /// the column.
    #[test]
    fn scan_with_an_index_past_the_row_is_a_bug() {
        let fixture = Fixture::new(1);
        fixture.insert(&[1]);
        let error = run(&fixture, &scan_node(fixture.table, &[3])).expect_err("index 3 of 1");
        assert_eq!(error.number, 50000);
        assert!(error.message.contains("c3"), "message: {}", error.message);
    }
}
