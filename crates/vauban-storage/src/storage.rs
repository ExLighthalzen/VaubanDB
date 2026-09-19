//! The [`Storage`] trait: the single contract implemented by the in-memory and the on-disk
//! storage engines.

use vauban_errors::SqlResult;

use crate::{
    DbId, Direction, IndexId, IndexShape, KeyRange, Row, RowId, RowIter, SavepointId, Snapshot,
    TableId, TableShape, TxnId,
};

/// Durable storage of databases, tables and indexes, serving versioned rows to the rest of
/// the engine. Implemented twice (`MemoryStorage` and `DiskStorage`); the callers
/// (`catalog`, `txn`, `executor`) are written against this trait alone. Exactly 22 methods; the model (logical rows, versions, snapshots)
/// is described in the [crate documentation](crate).
///
/// # Indexes are maintained by the implementation
///
/// Indexes are maintained by the implementation on every `insert`/`update`/`delete`/
/// `rollback`/`vacuum`. The caller only declares them ([`Storage::create_index`]) and
/// searches them ([`Storage::seek`]).
///
/// # DDL is not transactional at the storage level
///
/// `create_*`/`drop_*` have no `txn` parameter and take effect immediately. The
/// transactional behaviour of DDL at the SQL level is obtained by the caller:
/// [`Storage::drop_table`], [`Storage::drop_index`] and [`Storage::drop_database`] are
/// deferred to commit (a list of commit-time actions in `txn`, kept per savepoint), and a
/// [`Storage::create_table`]/[`Storage::create_index`] whose transaction rolls back is
/// compensated by the matching `drop_*`. A crash between the two leaves at worst an orphan
/// object with no reference in the catalogue. Protecting concurrent readers is the job of a
/// schema lock in `txn`. Note that SQL Server itself refuses `CREATE`/`DROP DATABASE` inside a
/// transaction (error 226). `drop_table` removes every version and every index of the
/// table; `drop_database` removes its tables and indexes.
///
/// # No shape change, no unversioned data
///
/// There is no `ALTER TABLE`: the caller creates a table of the new shape, copies the visible
/// rows, switches the catalogue reference and drops the old table at commit. There is no
/// unversioned data either: a counter that must survive a rollback (`IDENTITY`) is a short
/// autonomous transaction, serialised by the caller. This trait has no counting, statistics
/// or locking method: `txn` handles locks, the executor counts.
///
/// # Errors
///
/// `storage` knows nothing of SQL semantics. Every precondition violation (unknown
/// identifier, `Row` of the wrong arity, already committed `TxnId`, `RowId` whose latest
/// version is not current, invalid `SavepointId`) is a **caller bug** and returns
/// `Err(InternalError::Bug(msg).into())`, not a panic. The "business" errors an
/// implementation emits are the uniqueness violation (2601, "Duplicate keys" below) and
/// I/O failures (`InternalError::Io`, on disk).
///
/// # Duplicate keys
///
/// Applies to [`Storage::insert`], [`Storage::update`] and [`Storage::create_index`]. On a
/// duplicate, return exactly
/// `SqlError::duplicate_key_index(&table.to_string(), &index.to_string(), &key_text)`;
/// `TableId`/`IndexId` display as the bare decimal integer, which lets the caller identify
/// the violated index and rephrase the error with the catalogue names. This check sees the
/// uncommitted inserts of other transactions and replaces SQL Server's wait with an immediate
/// error: a deliberate difference from SQL Server. This is the barrier that holds in the
/// presence of concurrent writers: a prior `seek` by the executor is not enough.
///
/// A `unique` index forbids two *live* rows with the same key, where `NULL` equals `NULL` in
/// every column (so `(1, NULL)` can exist only once, like in a SQL Server unique index and
/// unlike the `=` predicate). "Live", for this test and from the point of view of the
/// writing transaction `txn`, means a version whose `xmin` is not `Aborted` and whose `xmax`
/// is `None`, or `Some(x)` with `x != txn` and `status(x) == InProgress`. A version deleted or
/// replaced by `txn` itself is never live for `txn` (an `UPDATE` that keeps the key, or a
/// `DELETE` followed by an `INSERT` of the same key in the same transaction, pass). An
/// in-progress transaction that inserted the key therefore blocks the others (SQL Server
/// waits; we return the error).
///
/// # Key order
///
/// See [`TableShape`] ("Key order"): `NULL` first, then `vauban_types::compare` with
/// the column's collation, reversed by `descending`, column by column, then by `RowId`.
pub trait Storage: Send + Sync {
    // ---------------------------------------------------------------- Databases

    /// Creates a database and returns its fresh [`DbId`].
    ///
    /// # Contract
    ///
    /// `name` is stored as given (no trimming, no case folding) and returned verbatim by
    /// [`Storage::databases`]. The database starts empty. Takes effect immediately (DDL is
    /// not transactional at this level, see the trait documentation).
    ///
    /// # Preconditions
    ///
    /// No existing database has the same name: the catalogue checks it (error 1801 comes
    /// from the catalogue, not from here).
    ///
    /// # Errors
    ///
    /// `InternalError::Io` on disk. Nothing else.
    fn create_database(&self, name: &str) -> SqlResult<DbId>;

    /// Drops a database with all its tables and indexes.
    ///
    /// # Contract
    ///
    /// Removes the database, every table it contains (all their versions, visible or not,
    /// whatever their transaction) and every index of those tables. Takes effect
    /// immediately; the caller defers the call to commit time to make `DROP DATABASE`
    /// transactional at the SQL level. After the call, `db` and the ids of its tables and
    /// indexes are unknown and never reused. In-flight writes on those tables are ignored by
    /// the later [`Storage::commit`]/[`Storage::rollback`]/[`Storage::rollback_to`].
    ///
    /// # Preconditions
    ///
    /// `db` was returned by [`Storage::create_database`] and has not been dropped.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `db` is unknown. `InternalError::Io` on disk.
    fn drop_database(&self, db: DbId) -> SqlResult<()>;

    /// Lists the databases of the instance.
    ///
    /// # Contract
    ///
    /// Returns `Ok` of every `(DbId, name)` pair, `name` exactly as given to
    /// [`Storage::create_database`], sorted by increasing `DbId`. Introspection only, no
    /// MVCC: reflects the DDL applied so far.
    ///
    /// # Preconditions
    ///
    /// None.
    ///
    /// # Errors
    ///
    /// `InternalError::Io` on disk. Nothing else.
    fn databases(&self) -> SqlResult<Vec<(DbId, String)>>;

    // ------------------------------------------------------- Tables and indexes

    /// Creates an empty table of the given shape in database `db` and returns its fresh
    /// [`TableId`].
    ///
    /// # Contract
    ///
    /// The table has no row and no index. `shape` is stored as given and returned verbatim
    /// by [`Storage::tables`]. The returned `TableId` is unique within the whole instance,
    /// not per database, and never reused. Takes effect immediately; a rolled-back
    /// `CREATE TABLE` is compensated by the caller with [`Storage::drop_table`].
    ///
    /// # Preconditions
    ///
    /// `db` is known. `shape.columns` is not empty. If `shape.clustered_key` is `Some`, it is
    /// not empty and every `KeyColumn::column` is `< shape.columns.len()`.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` on any precondition violation. `InternalError::Io` on disk.
    fn create_table(&self, db: DbId, shape: &TableShape) -> SqlResult<TableId>;

    /// Drops a table with all its versions and indexes.
    ///
    /// # Contract
    ///
    /// Removes every version of every row (visible or not, whatever their transaction) and
    /// every index of the table. Takes effect immediately; the caller defers the call to
    /// commit time to make `DROP TABLE` transactional at the SQL level. After the call,
    /// `table` and its `IndexId`s are unknown and never reused. In-flight writes on the
    /// table are ignored by the later [`Storage::commit`]/[`Storage::rollback`]/
    /// [`Storage::rollback_to`] of their transaction. No database parameter: `TableId` is
    /// unique within the instance.
    ///
    /// # Preconditions
    ///
    /// `table` was returned by [`Storage::create_table`] and has not been dropped.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `table` is unknown. `InternalError::Io` on disk.
    fn drop_table(&self, table: TableId) -> SqlResult<()>;

    /// Lists the tables of database `db` with their shape.
    ///
    /// # Contract
    ///
    /// Returns every `(TableId, TableShape)` of the tables of `db`, sorted by increasing
    /// `TableId`, each shape exactly as given to [`Storage::create_table`]. Introspection
    /// only, no MVCC: describes the shape, not rows, and reflects the DDL applied so far
    /// (DDL is immediate). The catalogue uses it to find its root table at start-up.
    ///
    /// # Preconditions
    ///
    /// `db` is known.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `db` is unknown. `InternalError::Io` on disk.
    fn tables(&self, db: DbId) -> SqlResult<Vec<(TableId, TableShape)>>;

    /// Creates an index on `table` and returns its fresh [`IndexId`].
    ///
    /// # Contract
    ///
    /// Indexes every existing version whose `xmin` is not `Aborted` (one entry per version)
    /// and every future version; from then on the implementation maintains it on every
    /// `insert`/`update`/`delete`/`rollback`/`vacuum`. If `def.unique` and live rows already
    /// share a key, fails with 2601 ("Duplicate keys" in the trait documentation) and
    /// creates nothing. `def.included` is stored as given and has no effect on semantics (an
    /// on-disk optimisation). The returned `IndexId` is unique within the whole instance and
    /// never reused. Takes effect immediately; a rolled-back `CREATE INDEX` is compensated by
    /// the caller with [`Storage::drop_index`]. A clustered key is not an index: to `seek`
    /// it, create an index whose `columns` equal the table's `clustered_key` (an
    /// implementation may serve it from the clustered tree).
    ///
    /// # Preconditions
    ///
    /// `table` is known. `def.columns` is not empty and every `KeyColumn::column` and every
    /// entry of `def.included` is `< columns.len()` of the table.
    ///
    /// # Errors
    ///
    /// `SqlError::duplicate_key_index(&table.to_string(), &index.to_string(), &key_text)`
    /// (2601) when `def.unique` and live rows share a key; `index` is the id the index
    /// would have received. `InternalError::Bug` on a precondition violation.
    /// `InternalError::Io` on disk.
    fn create_index(&self, table: TableId, def: &IndexShape) -> SqlResult<IndexId>;

    /// Drops an index.
    ///
    /// # Contract
    ///
    /// Removes the index and all its entries; the rows are untouched. Takes effect
    /// immediately; the caller defers the call to commit time to make `DROP INDEX`
    /// transactional at the SQL level. After the call, `index` is unknown and never reused.
    /// In-flight writes that touched the index are ignored by the later
    /// [`Storage::commit`]/[`Storage::rollback`]/[`Storage::rollback_to`]. No table
    /// parameter: `IndexId` is unique within the instance.
    ///
    /// # Preconditions
    ///
    /// `index` was returned by [`Storage::create_index`] and has not been dropped (neither
    /// directly nor through [`Storage::drop_table`]/[`Storage::drop_database`]).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `index` is unknown. `InternalError::Io` on disk.
    fn drop_index(&self, index: IndexId) -> SqlResult<()>;

    /// Lists the indexes of `table` with their shape.
    ///
    /// # Contract
    ///
    /// Returns every `(IndexId, IndexShape)` of the indexes of `table`, sorted by increasing
    /// `IndexId`, each shape exactly as given to [`Storage::create_index`]. Introspection
    /// only, no MVCC: describes the shape, not rows, and reflects the DDL applied so far
    /// (DDL is immediate).
    ///
    /// # Preconditions
    ///
    /// `table` is known.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `table` is unknown. `InternalError::Io` on disk.
    fn indexes(&self, table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>>;

    // ---------------------------------------------------------------------- Rows

    /// Inserts a new logical row on behalf of transaction `txn` and returns its [`RowId`].
    ///
    /// # Contract
    ///
    /// Creates a new logical row with a fresh `RowId` (unique within the table, never
    /// reused) and a single version `(xmin = txn, xmax = None)` holding `row`. Adds an entry
    /// to each index of the table; checks uniqueness on `unique` indexes ("Duplicate keys"
    /// in the trait documentation). Does **not** check that the values match the column
    /// types: that is the executor's job. If `txn` was unknown so far, the implementation
    /// registers it as `InProgress`.
    ///
    /// # Preconditions
    ///
    /// `table` is known. `row.0.len()` equals the number of columns of the table. `txn` has
    /// neither committed nor rolled back.
    ///
    /// # MVCC
    ///
    /// The new version is visible to `txn` itself immediately, and to other transactions only
    /// once `txn` has committed and is settled for their snapshot
    /// ([`Snapshot::is_visible`]). Rolling back `txn` removes the version and its index
    /// entries.
    ///
    /// # Errors
    ///
    /// `SqlError::duplicate_key_index(&table.to_string(), &index.to_string(), &key_text)`
    /// (2601) when a `unique` index already holds the key for a live row; nothing is
    /// inserted in that case. `InternalError::Bug` on a precondition violation.
    /// `InternalError::Io` on disk.
    fn insert(&self, txn: TxnId, table: TableId, row: &Row) -> SqlResult<RowId>;

    /// Replaces the content of logical row `id` on behalf of transaction `txn`.
    ///
    /// # Contract
    ///
    /// Sets `xmax = txn` on the current version of `id` and creates a new version
    /// `(xmin = txn, xmax = None)` holding `row`, chained after it. Maintains every index of
    /// the table (the old key stays indexed for the old version, the new key is indexed for
    /// the new one) and checks uniqueness on `unique` indexes ("Duplicate keys" in the
    /// trait documentation); the replaced version is not live for `txn`, so an `UPDATE`
    /// that keeps the key passes. Returns `Ok(())`: **the `RowId` of the logical row stays
    /// `id`, in every implementation**, even when a clustered-key column changes. Two
    /// successive `update`s by the same transaction create two versions; this is allowed.
    /// Does not check the value types.
    ///
    /// # Preconditions
    ///
    /// `table` is known. `id` exists in `table`, its most recent version has no `xmax`, and
    /// that version was created either by `txn` itself or by a `Committed` transaction
    /// (otherwise `Bug`: `txn` must have called [`Storage::latest_version`], obtained
    /// `Proceed` from the transaction manager and taken its row lock first; writing over a
    /// version created by another in-progress or aborted transaction would corrupt the undo
    /// chain). `row.0.len()` equals the number of columns. `txn` has neither committed nor
    /// rolled back. A refused call leaves no trace: `txn` is not registered by it.
    ///
    /// # MVCC
    ///
    /// For `txn`, the old version disappears and the new one appears at once. For another
    /// transaction whose snapshot does not settle `txn`, the old version stays visible; once
    /// `txn` is settled, only the new one is. Rolling back `txn` resets `xmax` to `None` on
    /// the old version and removes the new one.
    ///
    /// # Errors
    ///
    /// `SqlError::duplicate_key_index(&table.to_string(), &index.to_string(), &key_text)`
    /// (2601) when the new key collides with a live row on a `unique` index; the row is left
    /// unchanged in that case. `InternalError::Bug` on a precondition violation, including
    /// a stale `id` (its latest version already has an `xmax`). `InternalError::Io` on disk.
    fn update(&self, txn: TxnId, table: TableId, id: RowId, row: &Row) -> SqlResult<()>;

    /// Deletes logical row `id` on behalf of transaction `txn`.
    ///
    /// # Contract
    ///
    /// Sets `xmax = txn` on the current version of `id`. Nothing is removed physically:
    /// [`Storage::vacuum`] does that later. Index entries are kept for the deleted version
    /// until vacuum. A row inserted and then deleted by the same transaction is invisible
    /// to that transaction.
    ///
    /// # Preconditions
    ///
    /// Same as [`Storage::update`]: `table` is known, `id` exists, its most recent
    /// version has no `xmax` and was created by `txn` or by a `Committed` transaction (`txn`
    /// has called [`Storage::latest_version`] and taken its row lock), `txn` has neither
    /// committed nor rolled back.
    ///
    /// # MVCC
    ///
    /// The row disappears at once for `txn`. For another transaction, it stays visible until
    /// `txn` is settled for its snapshot ([`Snapshot::is_visible`]). Rolling back `txn`
    /// resets `xmax` to `None`.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` on a precondition violation, including a stale `id`.
    /// `InternalError::Io` on disk.
    fn delete(&self, txn: TxnId, table: TableId, id: RowId) -> SqlResult<()>;

    /// Reads the version of logical row `id` visible to `snap`.
    ///
    /// # Contract
    ///
    /// Returns `Some(row)` with the content of the version of `id` visible to `snap`, `None`
    /// if no version is visible. `None` is **never** an error: an unknown, vacuumed, deleted
    /// or not-yet-visible row all answer `None`.
    ///
    /// # Preconditions
    ///
    /// `table` is known.
    ///
    /// # MVCC
    ///
    /// Visibility is exactly [`Snapshot::is_visible`] applied to each version's
    /// `(xmin, xmax)` with the implementation's own knowledge of transaction statuses. By
    /// the chain invariant, at most one version qualifies.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `table` is unknown. `InternalError::Io` on disk.
    fn get(&self, snap: &Snapshot, table: TableId, id: RowId) -> SqlResult<Option<Row>>;

    /// Iterates over every logical row of `table` visible to `snap`.
    ///
    /// # Contract
    ///
    /// Yields every logical row that has a version visible to `snap`, exactly once each,
    /// as `(RowId, content of the visible version)`. Order: the table's `clustered_key` if
    /// defined ("Key order" in [`TableShape`]), otherwise a stable order that the trait
    /// does not guarantee (the executor sorts). The iterator is valid as long as `&self`
    /// lives; an `Err` item ends the iteration.
    ///
    /// # Preconditions
    ///
    /// `table` is known.
    ///
    /// # MVCC
    ///
    /// The iterator yields exactly the rows visible to `snap` as they were when the iterator
    /// was created, for every transaction other than `snap.own`. Writes by `snap.own` made
    /// after the iterator was created may or may not appear: a caller that modifies the table
    /// it is scanning must materialise the result before writing (Halloween protection, in
    /// `executor`).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `table` is unknown. `InternalError::Io` on disk, either at
    /// creation or as an `Err` item.
    fn scan(&self, snap: &Snapshot, table: TableId) -> SqlResult<Box<dyn RowIter + '_>>;

    /// Iterates over the rows visible to `snap` whose key falls within `range`, in the order
    /// of `index` and `dir`.
    ///
    /// # Contract
    ///
    /// Yields every logical row that has a version visible to `snap` **and** whose visible
    /// version's key is within `range`, exactly once each, ordered by the index key in
    /// `dir` (`Forward` = increasing index order, `Backward` = decreasing), ties by
    /// `RowId` in the same direction. The values of a [`KeyRange`] are compared column by
    /// column with the same comparator as the index ("Key order" in [`TableShape`]). Bounds
    /// are expressed in index order (`descending` already applied): `lo` is the first key
    /// served in `Forward`, `hi` the last. A bound on a prefix of length `p` designates the
    /// set of keys that start with that prefix: `Included(p)` includes that whole set,
    /// `Excluded(p)` excludes it entirely. `Bound::Unbounded` is accepted on either side.
    /// `Point(k)` with `k` of length `p` ≤ the number of key columns selects the keys whose
    /// first `p` columns equal `k`, `NULL` equal to `NULL`. `Point(vec![])` is equivalent to
    /// `Full`. `Full` is the whole index. A row whose key changed appears only once, under its
    /// visible key (chain invariant). The iterator is valid as long as `&self` lives; an `Err`
    /// item ends the iteration.
    ///
    /// # Preconditions
    ///
    /// `index` is known. Every key in `range` has at most as many values as the index has
    /// columns, and each [`vauban_types::Value`] variant matches the
    /// [`vauban_types::TypeInfo`] of its column.
    ///
    /// # MVCC
    ///
    /// Same isolation as [`Storage::scan`]: exactly the rows visible to `snap` as of the
    /// iterator's creation for every transaction other than `snap.own`; later writes by
    /// `snap.own` may or may not appear (the caller materialises before writing).
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `index` is unknown, a prefix is longer than the key, or a
    /// `Value` variant does not match its column. `InternalError::Io` on disk, either at
    /// creation or as an `Err` item.
    fn seek(
        &self,
        snap: &Snapshot,
        index: IndexId,
        range: &KeyRange,
        dir: Direction,
    ) -> SqlResult<Box<dyn RowIter + '_>>;

    /// Reads the most recent version of logical row `id`, ignoring any snapshot.
    ///
    /// # Contract
    ///
    /// Returns `Some((writer, row))` where `row` is the content of the most recent version
    /// of `id` and `writer` is the `TxnId` that produced the most recent state: the version's
    /// `xmax` if it is deleted, its `xmin` otherwise. Returns `None` if `id` is unknown or
    /// has been vacuumed. Serves the `txn` module to detect write conflicts
    /// (`check_write_conflict`, which decides with [`Snapshot::is_settled`] on `writer`).
    ///
    /// # Preconditions
    ///
    /// `table` is known.
    ///
    /// # MVCC
    ///
    /// Ignores visibility entirely: uncommitted, in-progress and aborted-but-not-yet-vacuumed
    /// versions are all reported if they are the most recent. `writer` may therefore be an
    /// `InProgress` or `Aborted` transaction.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `table` is unknown. `InternalError::Io` on disk.
    fn latest_version(&self, table: TableId, id: RowId) -> SqlResult<Option<(TxnId, Row)>>;

    // ------------------------------------------------- Transaction life cycle

    /// Commits transaction `txn`.
    ///
    /// # Contract
    ///
    /// Marks every version written by `txn` (created, or given an `xmax`) as `Committed`.
    /// When the call returns, those writes are durable (on disk: WAL written and `fsync`ed).
    /// A transaction that wrote nothing can be committed: `Ok(())`. Writes that touched a
    /// table or an index dropped in the meantime are ignored silently.
    ///
    /// # Preconditions
    ///
    /// `txn` has neither committed nor rolled back. Ordering: the caller calls `commit`
    /// **before** publishing the transaction as finished, that is before a new [`Snapshot`]
    /// can omit it from `active`.
    ///
    /// # MVCC
    ///
    /// After the call, the writes of `txn` become visible to every snapshot that settles
    /// `txn` ([`Snapshot::is_settled`]: `txn < xmax` and not listed in `active`). Snapshots
    /// that list `txn` in `active` keep not seeing them. The status reported for `txn` is
    /// `Committed` from now on, until [`Storage::vacuum`] forgets it.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `txn` was already committed or rolled back.
    /// `InternalError::Io` on disk (in which case durability is not guaranteed).
    fn commit(&self, txn: TxnId) -> SqlResult<()>;

    /// Rolls back transaction `txn`.
    ///
    /// # Contract
    ///
    /// Undoes every write of `txn`: versions it created are removed, `xmax`s it set are
    /// reset to `None`, its index entries are removed. Marks `txn` as `Aborted`. Every
    /// [`SavepointId`] of `txn` becomes invalid. A transaction that wrote nothing can be
    /// rolled back: `Ok(())`. Writes that touched a table or an index dropped in the meantime
    /// are ignored silently.
    ///
    /// # Preconditions
    ///
    /// `txn` has neither committed nor rolled back.
    ///
    /// # MVCC
    ///
    /// Nothing written by `txn` is ever visible to anyone afterwards (its versions are gone
    /// and its status is `Aborted`); rows it had deleted are visible again to whoever could
    /// see them before. The `RowId`s it created are never reused.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `txn` was already committed or rolled back.
    /// `InternalError::Io` on disk.
    fn rollback(&self, txn: TxnId) -> SqlResult<()>;

    /// Records a savepoint in transaction `txn` and returns its identifier.
    ///
    /// # Contract
    ///
    /// Returns a [`SavepointId`] increasing within `txn` and meaningful only for `txn`. The
    /// savepoint marks the writes of `txn` made so far; [`Storage::rollback_to`] undoes
    /// those made after it. Recording a savepoint writes nothing. If `txn` was unknown so
    /// far, the implementation registers it as `InProgress`.
    ///
    /// # Preconditions
    ///
    /// `txn` has neither committed nor rolled back.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `txn` was already committed or rolled back.
    /// `InternalError::Io` on disk.
    fn savepoint(&self, txn: TxnId) -> SqlResult<SavepointId>;

    /// Rolls transaction `txn` back to savepoint `sp`.
    ///
    /// # Contract
    ///
    /// Undoes the writes of `txn` made after `sp` (versions removed, `xmax`s reset to `None`,
    /// index entries removed), exactly as [`Storage::rollback`] would but only for those
    /// writes. Savepoints taken after `sp` become invalid; `sp` itself stays valid, so the
    /// caller can return to it several times (like `ROLLBACK TRANSACTION name`). `txn` stays
    /// `InProgress`. Writes that touched a table or an index dropped in the meantime are
    /// ignored silently.
    ///
    /// # Preconditions
    ///
    /// `txn` has neither committed nor rolled back. `sp` was returned by
    /// [`Storage::savepoint`] for this very `txn` and has not been invalidated by a
    /// `rollback_to` an earlier savepoint.
    ///
    /// # MVCC
    ///
    /// The undone writes are never visible to anyone afterwards, `txn` included; rows `txn`
    /// had deleted after `sp` are visible again to whoever could see them before. Writes made
    /// before `sp` are untouched and keep their visibility.
    ///
    /// # Errors
    ///
    /// `InternalError::Bug` if `txn` was already committed or rolled back, or if `sp` is
    /// unknown, belongs to another transaction or has been invalidated. `InternalError::Io`
    /// on disk.
    fn rollback_to(&self, txn: TxnId, sp: SavepointId) -> SqlResult<()>;

    /// Makes everything committed so far durable.
    ///
    /// # Contract
    ///
    /// On disk: flushes the committed state to the data files so that recovery has less log
    /// to replay; the WAL already guarantees durability of each commit. In memory: `Ok(())`,
    /// no effect. Does not change what any snapshot sees.
    ///
    /// # Preconditions
    ///
    /// None. May be called at any time, concurrently with reads and writes.
    ///
    /// # Errors
    ///
    /// `InternalError::Io` on disk. Nothing else.
    fn checkpoint(&self) -> SqlResult<()>;

    /// Removes the versions that no usable snapshot can see any more.
    ///
    /// # Contract
    ///
    /// `horizon` is provided by `txn`. Removes, in this order: the versions with
    /// `xmax = Some(x)` where `status(x) == Committed` and `x < horizon`; then the versions
    /// whose `xmin` is `Aborted`; then forgets from the transaction registry the finished
    /// transactions `< horizon`. Index entries of removed versions are removed. A version
    /// whose `xmax` is `None`, or belongs to an `InProgress` transaction, or is `>= horizon`,
    /// is never touched. Freed `RowId`s are never reused. Vacuuming with the same `horizon`
    /// twice is harmless.
    ///
    /// # Preconditions
    ///
    /// `horizon <= xmin` of every [`Snapshot`] still usable, and `horizon <=` every active
    /// `TxnId`. Why not simply "the oldest active transaction": a `SNAPSHOT` snapshot taken
    /// before a transaction committed its delete keeps that transaction in `active` and must
    /// still see the row, even if that transaction is smaller than every active one.
    ///
    /// # MVCC
    ///
    /// Under the precondition, no usable snapshot changes its answer: every removed version
    /// was invisible to all of them (deleted by a settled transaction, or created by an
    /// aborted one). After the registry is pruned, the status of a forgotten transaction is
    /// never consulted by a usable snapshot.
    ///
    /// # Errors
    ///
    /// `InternalError::Io` on disk. A `horizon` that violates the precondition is a caller
    /// bug that the implementation cannot detect: it is not reported.
    fn vacuum(&self, horizon: TxnId) -> SqlResult<()>;

    /// Lowest transaction identifier a new [`vauban_txn::TransactionManager`] should hand out.
    ///
    /// On disk reopen, identifiers the journal already ended must not be reused. The default
    /// suits memory and a fresh disk instance.
    fn next_txn_id(&self) -> u64 {
        1
    }

    /// Highest transaction identifier the storage already knows as finished or in progress.
    ///
    /// Memory managers may still hand out lower identifiers; start-up code uses this to
    /// pick a registry transaction that does not collide with an earlier bootstrap.
    fn committed_txn_high_water(&self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use vauban_errors::InternalError;

    use super::*;

    /// A do-nothing implementation: proves that the trait is implementable without
    /// `unsafe` and that every method can fail with a caller bug.
    struct Nop;

    fn bug<T>() -> SqlResult<T> {
        Err(InternalError::Bug("nop".into()).into())
    }

    impl Storage for Nop {
        fn create_database(&self, _name: &str) -> SqlResult<DbId> {
            bug()
        }
        fn drop_database(&self, _db: DbId) -> SqlResult<()> {
            bug()
        }
        fn databases(&self) -> SqlResult<Vec<(DbId, String)>> {
            bug()
        }
        fn create_table(&self, _db: DbId, _shape: &TableShape) -> SqlResult<TableId> {
            bug()
        }
        fn drop_table(&self, _table: TableId) -> SqlResult<()> {
            bug()
        }
        fn tables(&self, _db: DbId) -> SqlResult<Vec<(TableId, TableShape)>> {
            bug()
        }
        fn create_index(&self, _table: TableId, _def: &IndexShape) -> SqlResult<IndexId> {
            bug()
        }
        fn drop_index(&self, _index: IndexId) -> SqlResult<()> {
            bug()
        }
        fn indexes(&self, _table: TableId) -> SqlResult<Vec<(IndexId, IndexShape)>> {
            bug()
        }
        fn insert(&self, _txn: TxnId, _table: TableId, _row: &Row) -> SqlResult<RowId> {
            bug()
        }
        fn update(&self, _txn: TxnId, _table: TableId, _id: RowId, _row: &Row) -> SqlResult<()> {
            bug()
        }
        fn delete(&self, _txn: TxnId, _table: TableId, _id: RowId) -> SqlResult<()> {
            bug()
        }
        fn get(&self, _snap: &Snapshot, _table: TableId, _id: RowId) -> SqlResult<Option<Row>> {
            bug()
        }
        fn scan(&self, _snap: &Snapshot, _table: TableId) -> SqlResult<Box<dyn RowIter + '_>> {
            bug()
        }
        fn seek(
            &self,
            _snap: &Snapshot,
            _index: IndexId,
            _range: &KeyRange,
            _dir: Direction,
        ) -> SqlResult<Box<dyn RowIter + '_>> {
            bug()
        }
        fn latest_version(&self, _table: TableId, _id: RowId) -> SqlResult<Option<(TxnId, Row)>> {
            bug()
        }
        fn commit(&self, _txn: TxnId) -> SqlResult<()> {
            bug()
        }
        fn rollback(&self, _txn: TxnId) -> SqlResult<()> {
            bug()
        }
        fn savepoint(&self, _txn: TxnId) -> SqlResult<SavepointId> {
            bug()
        }
        fn rollback_to(&self, _txn: TxnId, _sp: SavepointId) -> SqlResult<()> {
            bug()
        }
        fn checkpoint(&self) -> SqlResult<()> {
            bug()
        }
        fn vacuum(&self, _horizon: TxnId) -> SqlResult<()> {
            bug()
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn boxed_storage_is_send_sync() {
        assert_send_sync::<Box<dyn Storage>>();
        assert_send_sync::<Nop>();
    }

    #[test]
    fn nop_is_usable_through_dyn_storage() {
        let storage: Box<dyn Storage> = Box::new(Nop);
        let err = storage.databases().unwrap_err();
        assert_eq!(err.number, 50000);
        assert_eq!(err.message, "Internal error: internal bug: nop");
        assert!(storage.checkpoint().is_err());
        assert!(storage.vacuum(TxnId(1)).is_err());
    }
}
