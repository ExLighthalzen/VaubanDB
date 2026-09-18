//! Running `ALTER TABLE ADD` / `DROP COLUMN` and `ADD` / `DROP CONSTRAINT`.
//!
//! [`DdlStatement::AlterTable`] carries the three-part table name the binder resolved and
//! the [`AlterTable`](vauban_catalog::AlterTable) action the catalogue takes. This file
//! turns the name into an [`ObjectId`](vauban_catalog::ObjectId) and calls
//! [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table) inside the transaction the
//! caller opened ([`ExecContext::handle`](crate::ExecContext)), then answers
//! [`ExecOutcome::NoRows`]: no COLMETADATA and no row, like the database and table DDL of
//! `ddl.rs`.
//!
//! # What this file checks and what it propagates
//!
//! Nothing is re-checked here. 4901 when a `NOT NULL` column without a default is added to a
//! table that already holds rows, 5074 when a column an index or constraint still references,
//! 3728 when a constraint name is unknown on `DROP CONSTRAINT`, and what
//! [`Catalog::alter_table`] raises for `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints come
//! from `catalog`, already built with their number, severity and state. The binder refused
//! `ALTER COLUMN` and `WITH NOCHECK` before the statement reaches this file.
//!
//! # How a table is found
//!
//! [`CatalogSnapshot::resolve_object`](vauban_catalog::CatalogSnapshot::resolve_object) is the
//! way from the three-part [`QualifiedName`] the binder stored to the [`ObjectId`] that
//! [`Catalog::alter_table`] takes — the binder already proved the table exists when it
//! bound the statement, so a name that resolves to nothing here is the internal 50000 of
//! [`locate`], not a client error number.

use vauban_binder::DdlStatement;
use vauban_catalog::{AlterTable, Catalog, ConstraintDef, ObjectId, QualifiedName};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_txn::TxnHandle;

use crate::context::ExecContext;
use crate::row::ExecOutcome;

/// Runs the `ALTER TABLE` of `stmt` and answers [`ExecOutcome::NoRows`].
///
/// The single entry point `ddl.rs` calls from its `AlterTable` arm; the other variants of
/// [`DdlStatement`] belong to `ddl.rs`, `ddl_index.rs` and `ddl_options.rs` and are the
/// internal 50000 here (`a_statement_that_is_not_an_alter_table_one_is_a_bug`), so a variant
/// routed to the wrong file is reported rather than run as something else.
///
/// # Errors
///
/// What `catalog.alter_table` raises, unchanged — 4901, 5074, 3728 and what the constraint
/// helpers answer — plus the internal 50000 this file raises when the table name no longer
/// resolves, and the internal 50000 for a statement that is not an `ALTER TABLE` one.
pub(crate) fn execute_alter_table(
    stmt: &DdlStatement,
    ctx: &ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let catalog = ctx.catalog()?;
    let handle = ctx.handle()?;
    match stmt {
        DdlStatement::AlterTable { table, action } => {
            let id = locate(catalog, handle, table)?;
            if let Err(err) = catalog.alter_table(handle, id, action) {
                if err.number == 2601 && key_constraint_add(action) {
                    return Err(SqlError::could_not_create_constraint_or_index());
                }
                return Err(err);
            }
        }
        DdlStatement::CreateDatabase { .. }
        | DdlStatement::DropDatabase { .. }
        | DdlStatement::CreateTable { .. }
        | DdlStatement::DropTable { .. }
        | DdlStatement::AlterDatabase { .. }
        | DdlStatement::CreateIndex { .. }
        | DdlStatement::DropIndex { .. }
        | DdlStatement::TruncateTable { .. } => {
            return Err(bug(
                "execute_alter_table: only ALTER TABLE is run here; the rest is run by \
                 ddl.rs, ddl_index.rs and ddl_options.rs",
            ));
        }
    }
    Ok(ExecOutcome::NoRows)
}

/// The [`ObjectId`] of the table named `table`.
///
/// # Errors
///
/// The internal 50000 when `table` resolves to nothing (module documentation).
fn locate(catalog: &Catalog, handle: &TxnHandle, table: &QualifiedName) -> SqlResult<ObjectId> {
    catalog
        .snapshot(handle)
        .resolve_object(
            &table.database,
            Some(&table.schema),
            &table.name,
            &table.schema,
        )
        .map(|object| object.id)
        .ok_or_else(|| {
            bug(&format!(
                "ALTER TABLE: the catalogue holds no table named '{}.{}'",
                table.schema, table.name
            ))
        })
}

/// Whether `action` adds a `PRIMARY KEY` or `UNIQUE` constraint through a table rebuild.
fn key_constraint_add(action: &AlterTable) -> bool {
    matches!(
        action,
        AlterTable::AddConstraint { constraint } if matches!(
            constraint.as_ref(),
            ConstraintDef::PrimaryKey { .. } | ConstraintDef::Unique { .. }
        )
    )
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_binder::{BindContext, SessionOptions, bind};
    use vauban_catalog::TableDef;
    use vauban_parser::{ParseOptions, parse_batch};
    use vauban_planner::{NoIndexes, PhysicalStatement, PlanContext, plan};
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_sysfn::StaticContext;
    use vauban_txn::{IsolationLevel, TransactionManager};
    use vauban_types::{SqlType, TypeInfo};

    /// A bootstrapped catalogue over an in-memory storage.
    struct Fixture {
        txn: Arc<TransactionManager>,
        catalog: Catalog,
    }

    impl Fixture {
        fn new() -> Self {
            let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
            let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
            let catalog = Catalog::bootstrap(Arc::clone(&storage), Arc::clone(&txn))
                .expect("the catalogue boots on a fresh storage");
            Self { txn, catalog }
        }
    }

    /// The single statement of `text`, bound against a catalogue view and planned.
    fn bound(fixture: &Fixture, handle: &TxnHandle, text: &str) -> PhysicalStatement {
        let batch = parse_batch(text, &ParseOptions::default()).expect("the text parses");
        assert_eq!(batch.statements.len(), 1, "one statement per call");
        let snapshot = fixture.catalog.snapshot(handle);
        let mut bind_ctx = BindContext::scalar(text, SessionOptions::default());
        bind_ctx.catalog = Some(&snapshot);
        let bound = bind(&batch.statements[0], &bind_ctx).expect("the statement binds");
        plan(
            bound,
            &PlanContext {
                catalog: &NoIndexes,
            },
        )
        .expect("the statement plans")
    }

    /// A `TableDef` of one `int` column named `a` in `master.dbo`.
    fn table_def(name: &str) -> TableDef {
        TableDef {
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            columns: vec![vauban_catalog::ColumnDef {
                name: "a".to_owned(),
                ty: TypeInfo::new(SqlType::Int, true),
                default: None,
                identity: None,
                computed: None,
            }],
            constraints: Vec::new(),
        }
    }

    /// Each variant reaching this file through `execute_ddl` is routed here and answers
    /// [`ExecOutcome::NoRows`], rather than an internal 50000 from the stub in `ddl.rs`.
    #[test]
    fn alter_table_reaches_ddl_alter() {
        let fixture = Fixture::new();
        let eval = StaticContext::default();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        fixture
            .catalog
            .create_table(&handle, &table_def("t"))
            .expect("the table is created");
        let ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);
        let stmt = bound(&fixture, &handle, "ALTER TABLE dbo.t ADD c int;");
        let PhysicalStatement::Ddl(ddl) = stmt else {
            panic!("expected Ddl");
        };
        assert!(matches!(
            execute_alter_table(&ddl, &ctx).expect("the column is added"),
            ExecOutcome::NoRows
        ));
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }

    /// A statement that is not an `ALTER TABLE` one is the internal 50000 of this file.
    #[test]
    fn a_statement_that_is_not_an_alter_table_one_is_a_bug() {
        let fixture = Fixture::new();
        let eval = StaticContext::default();
        let handle = fixture.txn.begin(IsolationLevel::ReadCommitted);
        let ctx = ExecContext::scalar(&eval, SessionOptions::default())
            .with_catalog(&fixture.catalog)
            .with_handle(&handle);
        let stmt = DdlStatement::CreateTable {
            def: table_def("t"),
        };
        assert_eq!(
            execute_alter_table(&stmt, &ctx)
                .expect_err("not ALTER TABLE")
                .number,
            50000
        );
        fixture
            .txn
            .rollback(handle)
            .expect("the transaction rolls back");
    }
}
