//! `TableScan`: reads the rows of one table, one at a time, over
//! [`Storage::scan`](vauban_storage::Storage::scan).
//!
//! # Streamed
//!
//! The operator takes the iterator of `storage.scan` when it opens, pulls one row from it
//! per [`Operator::next`] and drops it when it closes: three rows in the table are three
//! `row` calls on the sink, after one `columns` call (`tests/operator_basics.rs`,
//! `table_scan_streams_to_the_sink`). The iterator borrows the storage of the context,
//! which is why [`Operator`] carries the lifetime of the context.
//!
//! # Which value of the storage row each output column takes
//!
//! A `TableScan` carries a [`ColumnBinding`] per column it produces, and the `index` of
//! that binding is **the position of the value in the row `storage` hands out** — the
//! `ordinal` of the column, which `binder::catalog_view::columns_of` copies from the
//! catalogue (`columns_are_indexed_by_ordinal_not_by_identifier`, in `vauban-binder`).
//! The `i`-th column of the answer is therefore `row.0[columns[i].index]`, not
//! `row.0[i]`, so a scan may read a subset of the storage columns, in its own order. The
//! two readings — `index` as a source position, `index` as the output position `i` —
//! agree whenever a scan lists the storage columns in storage order, which is why the
//! test that separates them (`scan::tests::scan_projects_the_columns_it_names`) reads
//! column 2 then column 0 of a three-column table and asserts the values, not just the
//! arity.
//!
//! The [`ColumnId`](vauban_catalog::ColumnId) a binding also carries is not consulted
//! here: it identifies the column for the catalogue and survives the drop of a column
//! before it, which makes it useless as a position.

use vauban_binder::{ColumnBinding, OutputSchema};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{RowIter, TableId};

use crate::context::ExecContext;
use crate::operator::Operator;
use crate::row::Row;

/// The rows of `table` visible to the snapshot of the context, keeping the columns
/// `columns` names, in that order.
struct TableScan<'a> {
    table: TableId,
    columns: Vec<ColumnBinding>,
    schema: OutputSchema,
    /// The iterator of `storage.scan`, `Some` between `open` and the end of the rows.
    iter: Option<Box<dyn RowIter + 'a>>,
}

/// Builds the operator of a [`PhysicalPlan::TableScan`](vauban_planner::PhysicalPlan::TableScan).
///
/// The schema of the answer is the `schema` the binder put in the node, so an empty table
/// still tells the client the shape of the columns it has no row for
/// (`scan::tests::scan_empty_table_zero_rows`).
///
/// # Errors
///
/// The internal error 50000 for a `schema` and a `columns` of different widths, which
/// takes a bug of the binder.
pub(crate) fn build<'a>(
    table: TableId,
    columns: &[ColumnBinding],
    schema: &OutputSchema,
) -> SqlResult<Box<dyn Operator<'a> + 'a>> {
    if schema.columns.len() != columns.len() {
        return Err(bug(&format!(
            "TableScan: the node publishes {} column(s) and reads {}",
            schema.columns.len(),
            columns.len()
        )));
    }
    Ok(Box::new(TableScan {
        table,
        columns: columns.to_vec(),
        schema: schema.clone(),
        iter: None,
    }))
}

impl<'a> Operator<'a> for TableScan<'a> {
    /// Takes the iterator of `storage.scan`.
    ///
    /// # Errors
    ///
    /// Whatever [`Storage::scan`](vauban_storage::Storage::scan) raises, unchanged: an
    /// unknown table is its `InternalError::Bug`, an I/O failure its `InternalError::Io`.
    /// The internal error 50000 of a context with no engine ([`ExecContext::storage`]).
    fn open(&mut self, ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        let storage = ctx.storage()?;
        let snap = ctx.snapshot()?;
        self.iter = Some(storage.scan(snap, self.table)?);
        Ok(())
    }

    /// The next visible row, its columns picked by `index`.
    ///
    /// # Errors
    ///
    /// An `Err` item of the iterator, which ends the iteration, and the internal error
    /// 50000 for an `index` past the end of the row `storage` handed out.
    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        let Some(iter) = self.iter.as_mut() else {
            return Ok(None);
        };
        let source = match iter.next() {
            None => {
                self.iter = None;
                return Ok(None);
            }
            Some(Err(err)) => {
                self.iter = None;
                return Err(err);
            }
            Some(Ok((_, source))) => source,
        };
        let mut row = Vec::with_capacity(self.columns.len());
        for binding in &self.columns {
            let value = source.0.get(binding.index).ok_or_else(|| {
                bug(&format!(
                    "TableScan: column `{}` is at index {} of a row of {} value(s)",
                    binding.name,
                    binding.index,
                    source.0.len()
                ))
            })?;
            row.push(value.clone());
        }
        Ok(Some(row))
    }

    fn close(&mut self) {
        self.iter = None;
    }

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_binder::{OutputColumn, SessionOptions};
    use vauban_catalog::ColumnId;
    use vauban_planner::{PhysicalPlan, PhysicalStatement};
    use vauban_storage::{MemoryStorage, Row, Snapshot, Storage, TableShape};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo, Value};

    use crate::row::RowSet;

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

    /// A `TableScan` node reading the storage columns `indexes`, in that order.
    fn scan_node(table: TableId, indexes: &[usize]) -> PhysicalPlan {
        let columns: Vec<ColumnBinding> = indexes
            .iter()
            .map(|index| ColumnBinding {
                // `column_id` is 1-based in `sys.columns`; the operator reads `index`.
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
        PhysicalPlan::TableScan {
            table,
            columns,
            alias: "t".to_owned(),
            schema,
        }
    }

    /// Runs `plan` through [`crate::execute_collect`] with a context that carries the
    /// engine of `fixture`.
    fn run(fixture: &Fixture, plan: &PhysicalPlan) -> SqlResult<RowSet> {
        let eval = StaticContext::default();
        let snap = fixture.snapshot();
        let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            fixture.storage.as_ref(),
            &fixture.txn,
            &snap,
        );
        let stmt = PhysicalStatement::Query(plan.clone());
        crate::execute_collect(&stmt, &mut ctx).map(|(_, set)| set)
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
        let stmt = PhysicalStatement::Query(scan_node(fixture.table, &[0]));
        let error = crate::execute_collect(&stmt, &mut ctx).expect_err("a scan needs storage");
        assert_eq!(error.number, 50000);
    }

    /// A `columns` and a `schema` of different widths is a bug of the binder, reported as
    /// 50000 before the table is even opened.
    #[test]
    fn scan_with_a_mismatched_schema_is_a_bug() {
        let fixture = Fixture::new(2);
        let PhysicalPlan::TableScan {
            table,
            columns,
            alias,
            mut schema,
        } = scan_node(fixture.table, &[0, 1])
        else {
            unreachable!("scan_node builds a TableScan")
        };
        schema.columns.pop();
        let plan = PhysicalPlan::TableScan {
            table,
            columns,
            alias,
            schema,
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
