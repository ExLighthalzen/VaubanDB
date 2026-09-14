//! What the executor needs besides the statement it runs.

use vauban_binder::SessionOptions;
use vauban_catalog::Catalog;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{Snapshot, Storage};
use vauban_sysfn::EvalContext;
use vauban_txn::{TransactionManager, TxnHandle};

/// Everything the executor consults besides the bound statement it runs.
///
/// Borrowed, never owned: one context is built per statement by the caller (`session` in
/// production, the tests otherwise) and lives as long as the execution
/// (`tests/exec_shape.rs`). A scalar evaluation needs two things; reading a table or
/// running DDL adds the engine — storage, transaction manager, catalogue, the snapshot
/// the statement reads through and the transaction it writes in. The cancellation token
/// and the batch variables are not carried yet.
///
/// # Why five fields and not one `Engine`
///
/// An `engine: &Engine` grouping storage, transaction manager and catalogue would be the
/// natural shape, but `Engine` is a type of `session`, and `session` depends on
/// `executor`: naming it here would reverse the dependency of the engine. The three
/// references are therefore carried side by side, plus the [`Snapshot`] and the
/// [`TxnHandle`], which are not part of an `Engine`: the snapshot is taken per statement
/// by the caller ([`TransactionManager::statement_snapshot`]) and the handle is the
/// transaction that caller opened; this crate opens no transaction itself.
///
/// # Two shapes, one structure
///
/// [`ExecContext::scalar`] builds the scalar shape: no storage, no transaction manager, no
/// catalogue, no snapshot, no transaction handle. It is what a `SELECT` without `FROM`
/// needs. [`ExecContext::with_engine`] adds what a
/// [`Scan`](vauban_binder::LogicalPlan::Scan) needs, [`ExecContext::with_catalog`] and
/// [`ExecContext::with_handle`] what the DDL needs; running one of those without its
/// field is the internal error 50000 raised by the four accessors, and not a panic
/// (`context::tests::a_scalar_context_has_no_engine`).
pub struct ExecContext<'a> {
    /// Session context for the built-in functions (`GETDATE`, `@@SPID`, `@@ROWCOUNT`…).
    pub eval: &'a dyn EvalContext,
    /// `SET` options that change evaluation (`binder::SessionOptions`).
    pub options: SessionOptions,
    /// Where the rows live. `None` in a scalar context.
    pub storage: Option<&'a dyn Storage>,
    /// The transaction manager of the server. `None` in a scalar context.
    pub txn: Option<&'a TransactionManager>,
    /// The catalogue, for the statements that read or change metadata. `None` in a scalar
    /// context; the DDL reads it through [`ExecContext::with_catalog`].
    pub catalog: Option<&'a Catalog>,
    /// What this statement is allowed to see, taken by the caller before the statement
    /// started. `None` in a scalar context.
    pub snap: Option<&'a Snapshot>,
    /// The transaction the caller opened for this statement, which a mutation of the
    /// catalogue takes part in. `None` in a scalar context.
    ///
    /// Distinct from `txn`, which is the manager:
    /// [`Catalog::create_table`](vauban_catalog::Catalog::create_table) and its neighbours
    /// take the [`TxnHandle`] itself, and this crate opens no transaction of its own.
    /// Filled by [`ExecContext::with_handle`], which `session` calls with the handle it
    /// opened for the statement.
    pub handle: Option<&'a TxnHandle>,
}

impl<'a> ExecContext<'a> {
    /// A context for a statement that touches no table: the scalar shape.
    ///
    /// The engine fields are `None`; a plan that needs one answers 50000 instead of
    /// reading it (`context::tests::a_scalar_context_has_no_engine`).
    #[must_use]
    pub fn scalar(eval: &'a dyn EvalContext, options: SessionOptions) -> Self {
        Self {
            eval,
            options,
            storage: None,
            txn: None,
            catalog: None,
            snap: None,
            handle: None,
        }
    }

    /// The same context, plus what reading a table takes.
    ///
    /// `snap` is built by the caller, once per statement
    /// ([`TransactionManager::statement_snapshot`]): the executor neither opens nor ends a
    /// transaction.
    #[must_use]
    pub fn with_engine(
        mut self,
        storage: &'a dyn Storage,
        txn: &'a TransactionManager,
        snap: &'a Snapshot,
    ) -> Self {
        self.storage = Some(storage);
        self.txn = Some(txn);
        self.snap = Some(snap);
        self
    }

    /// The same context, plus the catalogue the metadata statements read.
    #[must_use]
    pub fn with_catalog(mut self, catalog: &'a Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// The same context, plus the transaction the DDL takes part in.
    ///
    /// Kept apart from [`ExecContext::with_engine`] so that a caller that reads rows without
    /// changing metadata hands over no handle: `snap` is what a read needs, the handle is
    /// what a mutation of
    /// the catalogue needs.
    #[must_use]
    pub fn with_handle(mut self, handle: &'a TxnHandle) -> Self {
        self.handle = Some(handle);
        self
    }

    /// Where the rows live, or the internal error 50000 when the caller built a scalar
    /// context for a statement that reads a table.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `storage` is `None`, which is what a caller that built a
    /// scalar context for a statement reading a table gets
    /// (`a_scalar_context_has_no_engine`); no client input can cause it.
    pub(crate) fn storage(&self) -> SqlResult<&'a dyn Storage> {
        self.storage.ok_or_else(|| {
            bug("ExecContext: reading a table needs `storage`, and this context has none")
        })
    }

    /// What this statement may see, or the internal error 50000 when the caller built a
    /// scalar context for a statement that reads a table.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `snap` is `None`, same cause as [`ExecContext::storage`].
    pub(crate) fn snapshot(&self) -> SqlResult<&'a Snapshot> {
        self.snap.ok_or_else(|| {
            bug("ExecContext: reading a table needs `snap`, and this context has none")
        })
    }

    /// The catalogue, or the internal error 50000 when the caller built a context without
    /// one for a statement that reads or changes metadata.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `catalog` is `None`, same cause as [`ExecContext::storage`]
    /// (`a_scalar_context_has_no_engine`).
    pub(crate) fn catalog(&self) -> SqlResult<&'a Catalog> {
        self.catalog.ok_or_else(|| {
            bug("ExecContext: a DDL statement needs `catalog`, and this context has none")
        })
    }

    /// The transaction of this statement, or the internal error 50000 when the caller built
    /// a context without one for a statement that changes the catalogue.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` when `handle` is `None`, same cause as [`ExecContext::storage`]
    /// (`a_scalar_context_has_no_engine`).
    pub(crate) fn handle(&self) -> SqlResult<&'a TxnHandle> {
        self.handle.ok_or_else(|| {
            bug("ExecContext: a DDL statement needs `handle`, and this context has none")
        })
    }
}

/// The internal error 50000 for a broken precondition, not a message for the client.
fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vauban_sysfn::StaticContext;

    /// A scalar context answers 50000 for the two things a `Scan` reads, instead of
    /// panicking or handing out a default.
    #[test]
    fn a_scalar_context_has_no_engine() {
        let eval = StaticContext::default();
        let ctx = ExecContext::scalar(&eval, SessionOptions::default());
        assert!(ctx.storage.is_none());
        assert!(ctx.txn.is_none());
        assert!(ctx.catalog.is_none());
        assert!(ctx.snap.is_none());
        assert!(ctx.handle.is_none());
        let no_storage = ctx
            .storage()
            .err()
            .expect("a scalar context has no storage");
        assert_eq!(no_storage.number, 50000);
        assert_eq!(ctx.snapshot().unwrap_err().number, 50000);
        assert_eq!(
            ctx.catalog()
                .err()
                .expect("a scalar context has no catalogue")
                .number,
            50000
        );
        assert_eq!(ctx.handle().unwrap_err().number, 50000);
    }

    /// The counter-proof of the test above: once `with_engine` has been called, the two
    /// accessors answer the references they were given.
    #[test]
    fn with_engine_fills_storage_and_snapshot() {
        use std::sync::Arc;
        use vauban_storage::{MemoryStorage, TxnId};
        use vauban_txn::IsolationLevel;

        let eval = StaticContext::default();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let txn = TransactionManager::new(Arc::clone(&storage));
        let handle = txn.begin(IsolationLevel::ReadCommitted);
        let snap = txn.statement_snapshot(&handle);
        let ctx = ExecContext::scalar(&eval, SessionOptions::default()).with_engine(
            storage.as_ref(),
            &txn,
            &snap,
        );
        assert!(ctx.storage().is_ok());
        assert_eq!(ctx.snapshot().expect("the snapshot is there").own, TxnId(1));
        assert!(ctx.txn.is_some());
        assert!(ctx.catalog.is_none(), "`with_engine` does not set it");
        assert!(ctx.handle.is_none(), "`with_engine` does not set it either");
        let ctx = ctx.with_handle(&handle);
        assert_eq!(ctx.handle().expect("the handle is there").id, TxnId(1));
    }
}
