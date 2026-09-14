#![deny(missing_docs)]
//! Crate `vauban-storage`: the [`Storage`] trait, its associated types and the MVCC
//! visibility rule shared by every implementation.
//!
//! This crate is a **contract between modules that do not talk to each other**: the
//! catalogue, the transaction manager and the executor are written against the in-memory
//! implementation (`MemoryStorage`) and switch to the on-disk one (`DiskStorage`) without
//! changing a line. Rows are versioned in both implementations: the on-disk one persists
//! what the in-memory one already models.
//!
//! # Model
//!
//! - A **logical row** is identified by a [`RowId`], unique within its table and never reused
//!   after a delete or a vacuum. **The `RowId` of a logical row never changes, whatever the
//!   implementation**: a physical move (a clustered-key change on disk) is invisible to the
//!   caller. The `txn` module relies on this for `lock_row` and `check_write_conflict`.
//! - A logical row has one or more **versions**. Each version carries `xmin: TxnId` (the
//!   transaction that created it) and `xmax: Option<TxnId>` (the transaction that deleted or
//!   replaced it, `None` otherwise). **Invariant**: the versions of a logical row form a
//!   chain (the `xmax` of one version is the `xmin` of the next), so for a given snapshot
//!   **at most one** version of a logical row is visible.
//! - [`TxnId`]s are assigned by `txn` and are strictly increasing over time. `storage` never
//!   creates a `TxnId`: it discovers a transaction at its first write and learns its fate
//!   through [`Storage::commit`] or [`Storage::rollback`].
//! - A [`Snapshot`] describes what a reader may see: `xmin` is the smallest `TxnId` still
//!   active when the snapshot was taken (every `TxnId < xmin` is finished), `xmax` is the
//!   next `TxnId` to be assigned (every `TxnId >= xmax` is invisible), `active` lists the
//!   transactions in progress within `[xmin, xmax)` (sorted), and `own` is the reading
//!   transaction. `own` may be `>= xmax` when the snapshot was taken before the transaction
//!   started; the visibility rule handles `own` before anything else.
//! - **Visibility rule** ([`Snapshot::is_settled`], [`Snapshot::is_visible`]): a transaction
//!   `t` is *settled* for a snapshot when `t == own`, or when `t < xmax`, `t` is not listed
//!   in `active` and its status is `Committed`. A version `(xmin, xmax)` is *visible* when
//!   `xmin` is settled and `xmax` is not `Some(x)` with `x` settled. Consequences: a
//!   transaction sees its own uncommitted writes; a delete by another uncommitted
//!   transaction does not hide the row; a version whose creator rolled back is never visible;
//!   a delete by a rolled-back transaction hides nothing. `txn` reuses `is_settled` in
//!   `check_write_conflict`.
//! - **DDL is not transactional at the storage level**: `create_*`/`drop_*` take effect
//!   immediately and have no `txn` parameter. The SQL-level transactional behaviour of DDL is
//!   built by the caller; the protocol is written in the [`Storage`] documentation.
//! - **No shape change**: the trait has no `ALTER TABLE`. The caller creates a table of the
//!   new shape, copies the visible rows, switches the reference in the catalogue and drops the
//!   old table at commit. `TRUNCATE` is a `delete` of every visible row, or a re-creation by
//!   the same procedure.
//! - **No unversioned data**: a counter that must survive a rollback (`IDENTITY`) is
//!   implemented with a short autonomous transaction, serialised by the caller.
// The crate of the workspace where `unsafe` is tolerated; the pragma is unused so far.
#![allow(unsafe_code)]

mod disk;
mod ids;
mod key_range;
mod memory;
mod row;
mod shape;
mod snapshot;
mod storage;
#[cfg(feature = "testsuite")]
pub mod testsuite;

pub use disk::{DiskOptions, DiskStorage};
pub use ids::{DbId, IndexId, RowId, SavepointId, TableId, TxnId};
pub use key_range::{Direction, KeyRange};
pub use memory::MemoryStorage;
pub use row::{Row, RowIter};
pub use shape::{IndexShape, KeyColumn, TableShape};
pub use snapshot::{Snapshot, TxnStatus};
pub use storage::Storage;
