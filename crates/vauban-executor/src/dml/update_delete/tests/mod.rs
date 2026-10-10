use std::sync::Arc;

use vauban_binder::{OutputSchema, SessionOptions};
use vauban_catalog::{Catalog, ColumnDef, QualifiedName, TableDef};
use vauban_storage::MemoryStorage;
use vauban_sysfn::StaticContext;
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{SqlType, TypeInfo};

use super::*;
use crate::context::CancelToken;

/// An input of `count` rows, each the identifier of one target row and nothing else.
struct Identifiers {
    count: i64,
    next: i64,
    schema: OutputSchema,
}

impl<'a> Operator<'a> for Identifiers {
    fn open(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<()> {
        self.next = 0;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'a>) -> SqlResult<Option<Row>> {
        if self.next == self.count {
            return Ok(None);
        }
        self.next += 1;
        Ok(Some(vec![Value::I64(self.next)]))
    }

    fn close(&mut self) {}

    fn schema(&self) -> &OutputSchema {
        &self.schema
    }
}

/// A spool whose read the token stops hands out nothing, and the statement answers
/// cancelled rather than zero rows written.
#[test]
fn a_cancelled_spool_cancels_the_statement() {
    let storage = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(
        storage.clone() as Arc<dyn vauban_storage::Storage>
    ));
    let catalog = Catalog::bootstrap(storage.clone(), txn.clone()).expect("bootstraps");
    let handle = txn.begin(IsolationLevel::ReadCommitted);
    let meta = catalog
        .create_table(
            &handle,
            &TableDef {
                name: QualifiedName {
                    database: "master".to_owned(),
                    schema: "dbo".to_owned(),
                    name: "c".to_owned(),
                },
                columns: vec![ColumnDef {
                    name: "a".to_owned(),
                    ty: TypeInfo::new(SqlType::Int, true),
                    default: None,
                    identity: None,
                    computed: None,
                }],
                constraints: Vec::new(),
            },
        )
        .expect("the table is new");
    let cat_snap = catalog.snapshot(&handle);
    let target = Target {
        table: meta.storage_id,
        meta: cat_snap
            .table_by_storage(meta.storage_id)
            .expect("the table is in the snapshot"),
        cat_snap: &cat_snap,
        name: "master.dbo.c".to_owned(),
        verb: "DELETE",
        assignments: None,
    };
    let source = PhysicalPlan::OneRow;
    let slot = TargetSlot { tail: 1, index: 0 };
    let eval = StaticContext::default();
    let token = CancelToken::new();
    token.cancel();
    let mut ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_cancel(&token);
    // More rows than the interval at which the spool reads the token.
    let input = Identifiers {
        count: 5000,
        next: 0,
        schema: OutputSchema {
            columns: Vec::new(),
        },
    };
    let mut root = Spool::wrap_if(true, Box::new(input));
    let outcome = drive(&mut *root, &source, slot, true, &target, &mut ctx);
    root.close();
    assert_eq!(outcome.expect("a cancellation is not an error"), None);
}
