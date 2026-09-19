//! [`CatalogSnapshot`]: the catalogue as one transaction sees it, with name resolution, the
//! rows of the databases and the system views.
//!
//! # Where the halves of a snapshot come from
//!
//! The catalogue keeps its metadata in four places, so a snapshot reads four sources:
//!
//! - the **databases** are the rows of `vauban_sys_databases`, read with
//!   [`Storage::scan`](vauban_storage::Storage::scan) under the snapshot of the transaction
//!   ([`TransactionManager::statement_snapshot`](vauban_txn::TransactionManager::statement_snapshot)).
//!   Those rows are versioned, so they follow the transaction that wrote them: a
//!   `CREATE DATABASE` its transaction has not committed is in the snapshot of that
//!   transaction and not in the snapshot of another one (`tests/snapshot.rs`,
//!   `an_uncommitted_database_is_visible_in_its_own_snapshot_only`);
//! - the **tables** are the [`TableMeta`] of the in-memory store of `table.rs`, read through
//!   `table::store` and `TableStore::live`, refreshed against `storage` first
//!   (`table::refresh`). Their **indexes** come from the same store, the `IndexStore` that
//!   `table.rs` holds beside the tables: read through `IndexStore::of_table` once
//!   `index::refresh` has brought it back in line with `storage`, which is the reference on
//!   existence (`tests/snapshot.rs`, `indexes_of_a_table_with_a_primary_key_has_its_index`);
//! - the **internal tables of `master`** are the `InternalTableDef` the bootstrap created a
//!   table of `storage` for, read from code ([`internal_table_objects`]) and paired with the
//!   `TableId` `storage` handed out, by position. They are entries of `tables` as well as of
//!   `objects`, so the plan the binder puts in place of `FROM sys.tables` — a `SELECT` over
//!   `master.dbo.vauban_sys_objects` — resolves to a table the planner can scan (section
//!   below; `tests/snapshot.rs`, `vauban_sys_databases_resolves_to_a_table_with_its_columns`);
//! - the **system views** are the `SystemViewDef` the files of `views/` describe, read from
//!   code and not from `storage` ([`described_views`]): a [`Catalog`] built from this binary
//!   describes the same ones, so there is no row to scan for them (section below).
//!
//! # The system views of `views/`
//!
//! Each file of `views/` hangs its views on the `InternalTableDef` of the internal table they
//! read, and the two tables of the bootstrap get theirs from `views::sys_core::bootstrap_views`
//! (`bootstrap.rs`). [`described_views`] puts those two sources end to end and
//! [`system_view_objects`] turns them into entries of `objects`, of kind
//! [`ObjectKind::View`], carrying the T-SQL text [`CatalogSnapshot::view_definition`] hands
//! back. They are not in `tables`, so [`CatalogSnapshot::table`] answers `None` for one
//! (`tests/snapshot.rs`, `sys_tables_resolves_to_a_view_with_its_definition`).
//!
//! Three rules, each one a property SQL Server has:
//!
//! - **one entry per `<schema>.<name>`, named in `master`.** `views::internal_tables()`
//!   installs each view once per system database — four `SystemViewDef` of the same
//!   `sys.tables` — while `OBJECT_ID` answers one identifier for the view however its first
//!   part is written: `OBJECT_ID('sys.tables')`, `OBJECT_ID('master.sys.tables')` and
//!   `OBJECT_ID('d.sys.tables')` are one number. So the snapshot holds one entry for the
//!   four definitions, the first of them, and [`CatalogSnapshot::object_name`] writes its
//!   first part as the name of `master` (`tests/snapshot.rs`,
//!   `a_system_view_is_visible_from_a_user_database`);
//! - **visible from the databases the snapshot holds**, and not from a first part that names
//!   no database: [`CatalogSnapshot::resolve_object`] starts with
//!   [`CatalogSnapshot::database`], so `nosuchdb.sys.tables` resolves to nothing
//!   (`tests/snapshot.rs`, `a_system_view_is_visible_from_a_user_database`). The schema is
//!   written or the name resolves to nothing: `OBJECT_ID('objects')` is `NULL` and
//!   `SELECT COUNT(*) FROM objects` is error 208, while `OBJECT_ID('SYS.TABLES')` and
//!   `OBJECT_ID('information_schema.tables')` resolve: the two parts are compared under the
//!   collation of the database, as for a user object, and the schema `INFORMATION_SCHEMA`
//!   goes through this lookup like `sys` (`tests/snapshot.rs`,
//!   `information_schema_tables_resolves`);
//! - **negative identifiers** (section below), the sign SQL Server gives its system views.
//!
//! A user table of the same name does not take the place of a view: after
//! `CREATE TABLE dbo.[tables] (a int);`, `OBJECT_ID('sys.tables')` keeps its number while
//! `OBJECT_ID('dbo.tables')` and `OBJECT_ID('tables')` give the new table — the schema tells
//! them apart (`tests/snapshot.rs`,
//! `a_user_table_named_like_a_system_view_does_not_shadow_it`). The other way round is not
//! reachable in SQL Server: `CREATE TABLE sys.t (a int);` is error 2760. Here `table.rs`
//! takes that `CREATE TABLE`, and [`CatalogSnapshot::resolve_object`] reads the system views
//! **before** the objects of the database, so such a table would not be reachable by name;
//! the refusal belongs to `table.rs`.
//!
//! # The identifiers of the system views are the rows of `sys.all_objects`
//!
//! [`system_view_objects`] numbers the views by rank: the view of rank `n` in
//! [`described_views`], counted from `0` over the distinct names, gets `ObjectId(-(n + 1))`.
//! That is the rule `views/sys_extra.rs` numbers its rows of `vauban_sys_all_objects` with —
//! its `FIRST_SYSTEM_VIEW_OBJECT_ID` is `-1` and the next ones go down by one — over the list
//! its `installed_system_views` builds, and [`described_views`] walks the files of `views/` in
//! that same order. So the identifier a snapshot answers for a view is the `object_id` its row
//! of `sys.all_objects` publishes, name by name (unit test
//! `system_view_ids_match_sys_all_objects`, which compares the two lists).
//!
//! The numbers of SQL Server are negative and not contiguous, so the rule shares the **sign**
//! with it and nothing more.
//!
//! Two consequences of numbering by rank: the identifier of a view holds for one build of the
//! crate and moves when a file of `views/` starts describing views before it — the seven views
//! of `views/sys_extra.rs` are the last seven, `-18` to `-24`, and those of the two tables of
//! the bootstrap `-16` and `-17` (unit test `the_described_views_are_those_of_the_files`) —
//! and a `CREATE VIEW` will hand out a positive identifier from `FIRST_USER_OBJECT_ID`
//! (`table.rs`), so the two ranges do not meet.
//!
//! # The internal tables of `master`
//!
//! The bootstrap creates one table of `storage` per `InternalTableDef` in `master`, in the
//! order `bootstrap::internal_table_defs` lists them (`bootstrap.rs`, section "Order of the
//! tables in `master`"). [`internal_table_objects`] walks the list beside
//! `Storage::tables(master)` and builds, for the description of position `n`:
//!
//! - a [`TableMeta`] whose `storage_id` is the [`TableId`](vauban_storage::TableId) at position
//!   `n` of `Storage::tables(master)` — the pairing `bootstrap::internal_table_id` reads one
//!   table with — and whose columns are those of the description: its name, its
//!   [`TypeInfo`](vauban_types::TypeInfo) and its nullability, `ColumnId(ordinal + 1)` and the
//!   ordinal counted from `0`, the two numberings `table.rs` gives a user table
//!   (`tests/snapshot.rs`, `vauban_sys_databases_resolves_to_a_table_with_its_columns`);
//! - an entry of `objects` of kind [`ObjectKind::Table`] named `master.dbo.<internal name>`:
//!   `dbo` is the schema the definitions of `views/` write in their `FROM`
//!   (`FROM master.dbo.vauban_sys_objects`), so what the binder puts in place of `sys.tables`
//!   resolves to this entry and the planner reads its `storage_id`.
//!
//! [`TableMeta::clustered`] is `None` for such a table and [`CatalogSnapshot::indexes_of`]
//! answers an empty slice: the bootstrap hands the clustered key of a description to
//! `TableShape::clustered_key` and builds no `IndexShape` beside it, so there is no `IndexId`
//! to carry (`tests/snapshot.rs`, `every_described_internal_table_resolves`).
//!
//! ## The identifier is the position, in a range of its own
//!
//! The description of position `n` gets `ObjectId(FIRST_INTERNAL_TABLE_OBJECT_ID + n)`, which
//! is `100_000 + n`. What the range buys is that two entries of one snapshot carry two
//! identifiers: the user tables of `table.rs` start at `FIRST_USER_OBJECT_ID`, `1_000_000`, the
//! system views above are numbered from `-1` downwards, and a constraint carries `0`
//! (`views/sys_constraints.rs`, `NO_OBJECT_ID`), so the 20 internal tables of this build sit
//! between the views and the user tables with 900_000 numbers of room ahead of them (unit test
//! `the_internal_tables_are_numbered_from_their_position`, which reads the three constants;
//! `tests/snapshot.rs`, `a_user_table_and_an_internal_table_do_not_collide`).
//!
//! The **sign** is what this rule shares with SQL Server, as for the views: its system base
//! tables (`sysrowsets`, `sysschobjs`) carry positive, small `object_id`s while a
//! compatibility view such as `sysobjects` carries a negative one. The numbers themselves
//! are ours.
//!
//! The identifier of an internal table holds for one build of the crate, like that of a system
//! view: a file of `views/` that starts describing a table moves the tables described after it,
//! the two tables of the bootstrap closing the list (`bootstrap.rs`). Two snapshots of one
//! build answer the same identifier for one table, the position being read from the code and
//! not from a counter (`tests/snapshot.rs`, `every_described_internal_table_resolves`).
//!
//! ## Visible in `master`, not published, not dropped
//!
//! - **`master` only.** The entries carry the [`DbId`] of `master` and
//!   [`CatalogSnapshot::resolve_object`] compares the database of an object with the one the
//!   first part names — the system views being the exception it looks up first — so
//!   `d.dbo.vauban_sys_databases`, and `dbo.vauban_sys_databases` written from a session whose
//!   database is `d`, resolve to `None` (`tests/snapshot.rs`,
//!   `an_internal_table_is_not_visible_from_a_user_database`).
//! - **Not a row of `sys.objects`.** The rows of that view are the ones `views/sys_tables.rs`
//!   describes; an entry built here writes no row, so `sys.objects` and `sys.tables` do not
//!   publish the internal tables (`tests/snapshot.rs`,
//!   `an_internal_table_is_not_published_by_sys_objects`).
//! - **Not droppable.** [`Catalog::drop_table`] of one of these identifiers answers 3701,
//!   severity 11, state 5 — the number SQL Server answers a `DROP TABLE` of a system table
//!   with (`DROP TABLE sys.sysobjects`, `sys.objects`, `sys.databases`). `table.rs` gives
//!   the number without a change: an identifier its store does not hold is a `cannot_drop`
//!   (`tests/snapshot.rs`,
//!   `an_internal_table_cannot_be_dropped`).
//!
//! # Bound: what the table half of a snapshot does not follow
//!
//! The store of `table.rs` is a map in memory, not a set of versioned rows. A snapshot shows
//! the tables of the store as they stand when it is built, whichever transaction created or
//! dropped them, so visibility splits in two:
//!
//! - a `create_table` its transaction has not committed **is** in the snapshot of that
//!   transaction (`tests/snapshot.rs`, `uncommitted_table_is_visible_in_own_snapshot`);
//! - it is in the snapshot of another transaction as well, and a `drop_table` not committed
//!   yet is missing from the snapshot of another transaction. That state is frozen by
//!   `tests/snapshot.rs`, `the_table_half_does_not_follow_the_transaction_yet`, so that the
//!   change which makes a DDL follow its transaction sees the test turn red when it fixes
//!   them. Nothing readable from this file tells the two apart today: an entry of the store
//!   carries no creating transaction, and `vauban_txn` publishes no way to read a registered
//!   action (`table.rs`, section "Bound").
//!
//! An index sits in the same store as its table, so it carries that bound too: a
//! `create_index` its transaction has not committed is in the snapshot of another
//! transaction (`tests/snapshot.rs`, `indexes_of_after_create_index_has_two`), and an index
//! whose table carries a deferred `DROP TABLE` is out of the snapshot before the `COMMIT`,
//! as its table is (`tests/snapshot.rs`, `the_indexes_of_a_dropped_table_leave_with_it`).
//!
//! What does follow the transaction, through the rows above: a table whose database is
//! invisible to the snapshot is invisible too, its three-part name having no first part to
//! be written with.
//!
//! # Comparison of names
//!
//! A database name is compared under [`Collation::DEFAULT`], the collation of `master`, as
//! `database.rs` compares it when it decides 1801. A schema and an object name are compared
//! under the collation of **their** database; `database.rs` stores
//! `SQL_Latin1_General_CP1_CI_AS` for each database.
//!
//! What that comparison folds is bounded by [`Collation::compare`] of `vauban_types`, whose
//! weights are indexed by CP1252 byte: the case of a character CP1252 carries is folded
//! (`T`/`t`, `É`/`é`, `Š`/`š`) and the case of a character it does not carry is left alone,
//! so `Ф` does not resolve `ф` here, where SQL Server reads `dbo.[ф]` and `dbo.[Ф]` as one
//! name (2714 on the second `CREATE TABLE`, one `OBJECT_ID` for the two spellings), as it
//! reads `dbo.[š]` and `dbo.[Š]`. The two shapes are held apart by `tests/snapshot.rs`,
//! `a_name_outside_cp1252_is_not_case_folded`; the fold itself is in `vauban_types`.
//! Accents separate two names under both (`tests/snapshot.rs`, `resolve_is_case_insensitive`).
//!
//! # Bound: two entries can match one written name
//!
//! `table.rs` refuses a second table of the same name with `eq_ignore_ascii_case` (2714)
//! while resolution compares under the collation, so a pair the collation reads as one name
//! can sit twice in the catalogue: `t` and `t  ` (trailing blanks, which
//! [`Collation::compare`] trims), `Été` and `été`. In SQL Server the second `CREATE TABLE`
//! of such a pair answers 2714 and one `OBJECT_ID` serves both spellings. Here both tables
//! exist, and [`CatalogSnapshot::resolve_object`] answers the one of the **smallest**
//! [`ObjectId`] — the objects are held in a `BTreeMap` keyed by identifier — which breaks
//! the round trip of [`CatalogSnapshot::object_name`] (`tests/snapshot.rs`,
//! `two_tables_the_collation_reads_as_one_name_resolve_to_the_smallest_id`). The refusal
//! belongs to `table.rs`.

use std::collections::BTreeMap;

use vauban_errors::{InternalError, SqlResult};
use vauban_storage::DbId;
use vauban_txn::TxnHandle;
use vauban_types::{Collation, Value};

use crate::bootstrap::{
    DATABASES_TABLE, SCHEMAS_TABLE, SYSTEM_DATABASES, databases_columns, internal_table_defs,
    internal_table_id,
};
use crate::catalog::Catalog;
use crate::def::{InternalTableDef, SystemViewDef};
use crate::ids::{ColumnId, ObjectId};
use crate::meta::{
    ColumnMeta, DatabaseMeta, IndexMeta, ObjectKind, ObjectMeta, QualifiedName,
    SnapshotIsolationState, TableMeta,
};
use crate::{index, table, views};

/// The [`ObjectId`] of the internal table whose description comes first in
/// `bootstrap::internal_table_defs`; the following ones count up from it.
///
/// A range of its own, between the system views and the user tables (module documentation,
/// section "The identifier is the position, in a range of its own"): `table.rs` hands out
/// `FIRST_USER_OBJECT_ID` and up, `views/` is numbered from `-1` down, and a constraint of
/// `views/sys_constraints.rs` carries `0` (unit test
/// `the_internal_tables_are_numbered_from_their_position`).
const FIRST_INTERNAL_TABLE_OBJECT_ID: i32 = 100_000;

/// The schema the internal tables of `master` are named in.
///
/// The schema the definitions of `views/` write in their `FROM`
/// (`FROM master.dbo.vauban_sys_objects`), which is what the binder resolves
/// (`tests/snapshot.rs`, `vauban_sys_databases_resolves_to_a_table_with_its_columns`).
const INTERNAL_TABLE_SCHEMA: &str = "dbo";

/// The catalogue as it stands for one transaction: the databases, objects and tables its
/// snapshot makes visible.
///
/// Handed out by [`Catalog::snapshot`]. Binder and planner read a snapshot, they do not read
/// the [`Catalog`]: two statements of the same transaction see the same catalogue, and a
/// `CREATE DATABASE` of another transaction does not appear in the middle of a statement.
///
/// The rows are copied in when the snapshot is built, so its answers do not change under its
/// reader. A snapshot built without a transaction — [`CatalogSnapshot::default`], which
/// `binder` uses in its tests — holds no row and its six methods answer `None` or an empty
/// slice (unit test `an_empty_snapshot_resolves_nothing`).
#[derive(Debug, Default)]
pub struct CatalogSnapshot {
    /// The databases the transaction sees, in the order their rows were scanned.
    databases: Vec<DatabaseMeta>,
    /// The named objects the transaction sees, by identifier.
    objects: BTreeMap<ObjectId, ObjectMeta>,
    /// The identifiers of the system views among `objects`, in the order
    /// [`system_view_objects`] built them. They are kept apart so that a name written from
    /// another database than `master` finds them without the database of the entry matching
    /// (module documentation, section "The system views of `views/`").
    system_views: Vec<ObjectId>,
    /// The tables among those objects, by the same identifier.
    tables: BTreeMap<ObjectId, TableMeta>,
    /// The indexes of those tables, keyed by the table they are built on, each list by
    /// increasing [`IndexId`](vauban_storage::IndexId). A table without an index has no
    /// entry here, and [`CatalogSnapshot::indexes_of`] answers an empty slice for it
    /// (`tests/snapshot.rs`, `indexes_of_an_unknown_table_is_empty`).
    indexes: BTreeMap<ObjectId, Vec<IndexMeta>>,
}

impl CatalogSnapshot {
    /// The database named `name`, `None` when the name matches no database.
    ///
    /// The name is compared under [`Collation::DEFAULT`] (module documentation), so `D` and
    /// `d` are the same database (`tests/snapshot.rs`, `resolve_is_case_insensitive`).
    pub fn database(&self, name: &str) -> Option<&DatabaseMeta> {
        self.databases
            .iter()
            .find(|database| same_name(Collation::DEFAULT, &database.name, name))
    }

    /// The database of identifier `id`, `None` when this snapshot holds no such database.
    pub fn database_by_id(&self, id: vauban_storage::DbId) -> Option<&DatabaseMeta> {
        self.databases.iter().find(|database| database.id == id)
    }

    /// The object named by one to three parts, `None` when the name resolves to nothing.
    ///
    /// `schema` is the schema the client wrote, `default_schema` the one of its session,
    /// used when `schema` is `None`. There is no second try: an object of `dbo` is not found
    /// under a session whose default schema is another one (`tests/snapshot.rs`,
    /// `resolve_unqualified_uses_default_schema`). A session gets `dbo`.
    ///
    /// The fourth part of a four-part name — the linked server — is no parameter here: the
    /// binder answers it (`binder/catalog_view.rs`, which resolves a name whose `server` is
    /// set to nothing).
    ///
    /// Schema and object are compared under the collation of the database `db` names, which
    /// folds the case of the characters CP1252 carries and leaves the others alone (module
    /// documentation).
    ///
    /// Two entries can match one written name — `t` and `t  `, `Été` and `été`, which
    /// `table.rs` keeps apart and the collation reads as one name: the object of the
    /// smallest [`ObjectId`] is the one answered, which is the one created first
    /// (`tests/snapshot.rs`, `two_tables_the_collation_reads_as_one_name_resolve_to_the_smallest_id`).
    ///
    /// A **system view** is looked up first, and from whichever database `db` names: the
    /// entries of `views/` belong to no one database, so `sys.tables` resolves to the same
    /// [`ObjectMeta`] read from `master` and from a user database, as SQL Server answers the
    /// same `object_id` for both (module documentation; `tests/snapshot.rs`,
    /// `a_system_view_is_visible_from_a_user_database`). The objects of the database are read
    /// next, so a user table whose schema is not `sys` keeps its own identifier
    /// (`tests/snapshot.rs`, `a_user_table_named_like_a_system_view_does_not_shadow_it`).
    pub fn resolve_object(
        &self,
        db: &str,
        schema: Option<&str>,
        name: &str,
        default_schema: &str,
    ) -> Option<&ObjectMeta> {
        let database = self.database(db)?;
        let collation = database.collation;
        let schema = schema.unwrap_or(default_schema);
        self.system_view(collation, schema, name).or_else(|| {
            self.objects.values().find(|object| {
                object.database == database.id
                    && same_name(collation, &object.name.schema, schema)
                    && same_name(collation, &object.name.name, name)
            })
        })
    }

    /// The system view written `schema.name`, `None` when the two parts match no entry of
    /// `system_views`.
    ///
    /// The schema is compared like the name, so an unqualified name resolves to no system
    /// view — `default_schema` being `dbo` for a session — which is the answer SQL Server
    /// gives (module documentation, error 208 on `SELECT COUNT(*) FROM objects`).
    fn system_view(&self, collation: Collation, schema: &str, name: &str) -> Option<&ObjectMeta> {
        self.system_views
            .iter()
            .filter_map(|id| self.objects.get(id))
            .find(|object| {
                same_name(collation, &object.name.schema, schema)
                    && same_name(collation, &object.name.name, name)
            })
    }

    /// The table of identifier `id`, `None` when `id` is not a table of this snapshot.
    ///
    /// The identifier is the one [`CatalogSnapshot::resolve_object`] gave back, which is how
    /// the binder reaches the columns of a table it has just resolved
    /// (`binder/catalog_view.rs`; `tests/snapshot.rs`, `a_resolved_table_carries_its_columns`).
    ///
    /// `None` for the identifier of a system view: a view is an entry of `objects` and not of
    /// `tables`, so a caller that needs columns here gets nothing and reads
    /// [`CatalogSnapshot::view_definition`] instead (`tests/snapshot.rs`,
    /// `sys_tables_resolves_to_a_view_with_its_definition`).
    pub fn table(&self, id: ObjectId) -> Option<&crate::meta::TableMeta> {
        self.tables.get(&id)
    }

    /// The table whose storage identifier is `storage_id`, `None` when no table of this
    /// snapshot carries that identifier.
    pub fn table_by_storage(&self, storage_id: vauban_storage::TableId) -> Option<&TableMeta> {
        self.tables
            .values()
            .find(|meta| meta.storage_id == storage_id)
    }

    /// The T-SQL text of the view of identifier `id`, `None` when `id` is not a view.
    ///
    /// The text is the one the file of `views/` that describes the view wrote — a `SELECT`
    /// over the internal table the view reads, which the binder expands in place of the name
    /// (`tests/snapshot.rs`, `sys_tables_resolves_to_a_view_with_its_definition`).
    ///
    /// `None` for a table, which carries no definition (`tests/snapshot.rs`,
    /// `a_resolved_table_carries_its_columns`), and for an identifier this snapshot does not
    /// hold (unit test `an_empty_snapshot_resolves_nothing`). The views of a snapshot are the
    /// system views of `views/`; `CREATE VIEW` is not implemented yet.
    pub fn view_definition(&self, id: ObjectId) -> Option<&str> {
        self.objects
            .get(&id)
            .and_then(|object| object.definition.as_deref())
    }

    /// The indexes of the table of identifier `table`, by increasing
    /// [`IndexId`](vauban_storage::IndexId).
    ///
    /// An empty slice for a table this snapshot holds that carries no index, for an
    /// identifier it does not hold and for an empty snapshot (`tests/snapshot.rs`,
    /// `indexes_of_an_unknown_table_is_empty`; unit test
    /// `an_empty_snapshot_resolves_nothing`).
    ///
    /// The index a `PRIMARY KEY` or a `UNIQUE` constraint is backed by is one of them, with
    /// `unique` set; for a clustered `PRIMARY KEY` its identifier is the one
    /// [`TableMeta::clustered`] carries, which is how a reader goes from the table to the
    /// metadata of its key (`tests/snapshot.rs`,
    /// `indexes_of_a_table_with_a_primary_key_has_its_index`,
    /// `indexes_of_after_create_index_has_two`).
    ///
    /// The [`IndexMeta`] are copied in when the snapshot is built, from the `IndexStore` of
    /// `index.rs` refreshed against `storage` (module documentation), so a snapshot keeps the
    /// answer it was built with. Two shapes that rule decides:
    ///
    /// - an index whose `DROP INDEX` is deferred to the `COMMIT` is out of a snapshot built
    ///   before that commit — `index.rs` marks the entry and `IndexStore::of_table` skips a
    ///   marked one — for the transaction that asked for the drop as for another one, while
    ///   `storage` still holds the index. A `ROLLBACK` clears the mark, and a snapshot built
    ///   afterwards carries the index again (`tests/snapshot.rs`,
    ///   `a_dropped_index_leaves_the_snapshot_after_commit`,
    ///   `a_rolled_back_drop_index_is_in_the_snapshot_again`);
    /// - an index follows its table: a table out of the snapshot answers here with an empty
    ///   slice, the indexes being read under the tables `TableStore::live` gives
    ///   (`tests/snapshot.rs`, `the_indexes_of_a_dropped_table_leave_with_it`).
    pub fn indexes_of(&self, table: ObjectId) -> &[IndexMeta] {
        self.indexes.get(&table).map_or(&[], Vec::as_slice)
    }

    /// The three-part name of the object of identifier `id`, `None` when `id` names nothing.
    ///
    /// The three parts carry the case they were created with, not the case the caller of
    /// [`CatalogSnapshot::resolve_object`] wrote (`tests/snapshot.rs`,
    /// `object_name_round_trip`).
    ///
    /// The round trip breaks on a name two entries match (module documentation, section
    /// "Bound: two entries can match one written name"): `resolve_object` answers the
    /// object of the smallest [`ObjectId`], and the name given back here is that object's —
    /// trailing blanks and non-ASCII case included — which is not the name the caller wrote
    /// (`tests/snapshot.rs`, `two_tables_the_collation_reads_as_one_name_resolve_to_the_smallest_id`).
    pub fn object_name(&self, id: ObjectId) -> Option<QualifiedName> {
        self.objects.get(&id).map(|object| object.name.clone())
    }
}

/// Builds the snapshot [`Catalog::snapshot`] returns.
///
/// [`Catalog::snapshot`] answers a [`CatalogSnapshot`] and not a `SqlResult`, so a failing
/// read gives an **empty** snapshot: the
/// caller then resolves nothing and reports the error of an unknown object rather than a
/// storage error. That path is taken by a [`Catalog`] whose storage carries no internal
/// table, one that was not bootstrapped (unit test
/// `a_catalogue_without_internal_tables_gives_an_empty_snapshot`).
pub(crate) fn build(catalog: &Catalog, txn: &TxnHandle) -> CatalogSnapshot {
    read(catalog, txn).unwrap_or_default()
}

/// Reads the four sources of the module documentation into a snapshot.
///
/// The system views and the internal tables of `master` are added to `objects` before the
/// user tables, and the three ranges of identifiers are disjoint — negative for a view,
/// `FIRST_INTERNAL_TABLE_OBJECT_ID` and up for an internal table, `FIRST_USER_OBJECT_ID` and
/// up for a user table (`table.rs`) — so one entry does not take the place of another (unit
/// test `the_internal_tables_are_numbered_from_their_position`). A storage whose
/// `vauban_sys_databases` carries no `master` gets neither the views nor the internal tables,
/// there being no database to name them in (unit test
/// `a_catalogue_without_internal_tables_gives_an_empty_snapshot`).
///
/// An object whose database is not among the ones read is left out: its three-part name has
/// no first part, and a database the transaction cannot see holds nothing it can name.
///
/// The indexes are read under the same lock as the tables and in the same pass: a table and
/// its indexes come from one state of the store, rather than from two states a concurrent
/// `DROP INDEX` could sit between.
///
/// # Errors
///
/// The error of the `storage` calls, of the scan of `vauban_sys_databases` as of the refresh
/// of the table store and of the index store.
fn read(catalog: &Catalog, txn: &TxnHandle) -> SqlResult<CatalogSnapshot> {
    let databases = databases_of(catalog, txn)?;
    let (live, mut indexes) = {
        let mut store = table::store(catalog);
        table::refresh(catalog, &mut store)?;
        index::refresh(catalog, &mut store)?;
        let live: Vec<TableMeta> = store.live().cloned().collect();
        let indexes: BTreeMap<ObjectId, Vec<IndexMeta>> = live
            .iter()
            .filter_map(|meta| {
                let metas: Vec<IndexMeta> = store
                    .indexes
                    .of_table(meta.id)
                    .into_iter()
                    .cloned()
                    .collect();
                (!metas.is_empty()).then_some((meta.id, metas))
            })
            .collect();
        (live, indexes)
    };
    let mut objects = BTreeMap::new();
    let mut system_views = Vec::new();
    let mut tables = BTreeMap::new();
    if let Some(master) = databases
        .iter()
        .find(|database| same_name(Collation::DEFAULT, &database.name, SYSTEM_DATABASES[0]))
    {
        for object in system_view_objects(&described_views(), master) {
            system_views.push(object.id);
            objects.insert(object.id, object);
        }
        for (object, meta) in internal_table_objects(catalog, master)? {
            objects.insert(object.id, object);
            tables.insert(meta.id, meta);
        }
    }
    for meta in live {
        let Some(database) = databases.iter().find(|db| db.id == meta.database) else {
            continue;
        };
        objects.insert(
            meta.id,
            ObjectMeta {
                id: meta.id,
                kind: ObjectKind::Table,
                name: QualifiedName {
                    database: database.name.clone(),
                    schema: meta.schema.clone(),
                    name: meta.name.clone(),
                },
                database: meta.database,
                parent: None,
                definition: None,
            },
        );
        tables.insert(meta.id, meta);
    }
    // A table left out above — its database is invisible to this snapshot — takes its
    // indexes with it, `indexes_of` being keyed by an identifier `tables` carries.
    indexes.retain(|table, _| tables.contains_key(table));
    Ok(CatalogSnapshot {
        databases,
        objects,
        system_views,
        tables,
        indexes,
    })
}

/// The system views the files of `views/` describe, in the order they are numbered.
///
/// The order is the one `installed_system_views` of `views/sys_extra.rs` numbers its rows of
/// `vauban_sys_all_objects` in, so that the identifier of a view is the same on the two
/// sides (unit test `system_view_ids_match_sys_all_objects`): the five files of `views/`
/// other than
/// `sys_extra.rs`, in the order `views::internal_tables()` lists them, then the two views of
/// the tables of the bootstrap (`views::sys_core::bootstrap_views`, which `bootstrap.rs` reads
/// too), then the seven of `sys_extra.rs`. A file that describes no view adds nothing here, so
/// the vector grows with the files of `views/` — and so do the identifiers of the views that
/// follow the new ones (module documentation, section "The identifiers of the system views
/// are the rows of `sys.all_objects`").
fn described_views() -> Vec<SystemViewDef> {
    let of_the_five_files = [
        views::sys_tables::internal_tables(),
        views::sys_core::internal_tables(),
        views::sys_indexes::internal_tables(),
        views::info_schema::internal_tables(),
        views::sys_constraints::internal_tables(),
    ];
    let mut described: Vec<SystemViewDef> = of_the_five_files
        .into_iter()
        .flatten()
        .flat_map(|table| table.views)
        .collect();
    described.extend(views::sys_core::bootstrap_views(DATABASES_TABLE));
    described.extend(views::sys_core::bootstrap_views(SCHEMAS_TABLE));
    described.extend(
        views::sys_extra::internal_tables()
            .into_iter()
            .flat_map(|table| table.views),
    );
    described
}

/// The rows `views/sys_extra.rs` writes in `vauban_sys_all_objects` for the system views: the
/// `(object_id, name, schema_id)` of each, in the order of `installed_system_views`.
///
/// Read from the description of the internal table that carries the view `sys.all_objects` —
/// the one table of `views::internal_tables()` whose `views` name it — rather than from that
/// function, which `views/sys_extra.rs` keeps private. The rows of a user object are added to
/// the table by the catalogue at runtime, so what is described here is the system part
/// (`views/sys_extra.rs`, `system_view_rows`; unit test `system_view_ids_match_sys_all_objects`).
#[cfg(test)]
fn described_system_view_rows() -> Vec<(i32, String, i32)> {
    use crate::views::sys_extra::objects_columns;

    views::internal_tables()
        .into_iter()
        .filter(|table| {
            table
                .views
                .iter()
                .any(|installed| installed.name.name == "all_objects")
        })
        .flat_map(|table| table.rows)
        .filter_map(|row| {
            let id = match row.0.get(objects_columns::OBJECT_ID) {
                Some(&Value::I32(id)) => id,
                _ => return None,
            };
            let name = match row.0.get(objects_columns::NAME) {
                Some(Value::String(name)) => name.text.clone(),
                _ => return None,
            };
            let schema = match row.0.get(objects_columns::SCHEMA_ID) {
                Some(&Value::I32(schema)) => schema,
                _ => return None,
            };
            Some((id, name, schema))
        })
        .collect()
}

/// One [`ObjectMeta`] per distinct `<schema>.<name>` of `described`, named in `master`.
///
/// A view described once per system database — what `views::internal_tables()` hands out —
/// gives one entry here, the first of its copies, and the following copies are skipped:
/// SQL Server answers one `object_id` for a view however the first part of its name is written
/// (module documentation). The two parts are compared under the collation of `master`, the
/// database the entries are named in.
///
/// The identifier is `-(rank + 1)`, the rank being the position of the view among the
/// distinct names, from `0`. A description this rank cannot be written as an `i32` for stops
/// the numbering rather than repeating an identifier; `described_views()` of this crate is
/// far shorter than that (unit test `the_rank_rule_numbers_the_distinct_names_from_minus_one`).
fn system_view_objects(described: &[SystemViewDef], master: &DatabaseMeta) -> Vec<ObjectMeta> {
    let mut objects: Vec<ObjectMeta> = Vec::new();
    for def in described {
        let already = objects.iter().any(|object: &ObjectMeta| {
            same_name(master.collation, &object.name.schema, &def.name.schema)
                && same_name(master.collation, &object.name.name, &def.name.name)
        });
        if already {
            continue;
        }
        let Ok(rank) = i32::try_from(objects.len().saturating_add(1)) else {
            break;
        };
        objects.push(ObjectMeta {
            id: ObjectId(-rank),
            kind: ObjectKind::View,
            name: QualifiedName {
                database: master.name.clone(),
                schema: def.name.schema.clone(),
                name: def.name.name.clone(),
            },
            database: master.id,
            parent: None,
            definition: Some(def.definition.clone()),
        });
    }
    objects
}

/// One [`ObjectMeta`] and one [`TableMeta`] per internal table of `master`, in the order
/// `bootstrap::internal_table_defs` describes them (module documentation, section "The
/// internal tables of `master`").
///
/// The [`TableId`](vauban_storage::TableId) of the table of position `n` is the one at
/// position `n` of `Storage::tables(master)`, which comes back sorted by increasing
/// identifier, the order the bootstrap created the tables in: the pairing
/// `bootstrap::internal_table_id` performs for one name, done here for the whole list. A
/// description with no table at its position — a `master` whose tables were not all created,
/// or a storage a later build describes more tables than it holds — is skipped rather than
/// paired with the table of another description (unit test
/// `a_description_without_its_table_is_skipped`).
///
/// # Errors
///
/// The error of `Storage::tables`, and the one `bootstrap::internal_table_defs` answers with.
fn internal_table_objects(
    catalog: &Catalog,
    master: &DatabaseMeta,
) -> SqlResult<Vec<(ObjectMeta, TableMeta)>> {
    let created = catalog.storage.tables(master.id)?;
    let mut built = Vec::new();
    for (position, def) in internal_table_defs()?.into_iter().enumerate() {
        let Some(&(storage_id, _)) = created.get(position) else {
            continue;
        };
        let Some(id) = i32::try_from(position)
            .ok()
            .and_then(|rank| FIRST_INTERNAL_TABLE_OBJECT_ID.checked_add(rank))
            .map(ObjectId)
        else {
            break;
        };
        let object = ObjectMeta {
            id,
            kind: ObjectKind::Table,
            name: QualifiedName {
                database: master.name.clone(),
                schema: INTERNAL_TABLE_SCHEMA.to_owned(),
                name: def.name.clone(),
            },
            database: master.id,
            parent: None,
            definition: None,
        };
        let meta = TableMeta {
            id,
            storage_id,
            database: master.id,
            schema: INTERNAL_TABLE_SCHEMA.to_owned(),
            name: def.name.clone(),
            columns: internal_columns(&def),
            // The bootstrap gives the clustered key of a description to
            // `TableShape::clustered_key` and builds no `IndexShape` beside it, so there is
            // no `IndexId` to carry here (module documentation).
            clustered: None,
            constraints: Vec::new(),
        };
        built.push((object, meta));
    }
    Ok(built)
}

/// The [`ColumnMeta`] of the columns of an internal table, in the order the description
/// declares them.
///
/// The two numberings of `table.rs`: the identifier counts from `1`, as
/// `sys.columns.column_id` does, and the ordinal is the position in the
/// [`Row`](vauban_storage::Row) of `storage`, from `0`, which is what a reader of a row
/// indexes with (`binder/catalog_view.rs`). A column whose position does not fit in the `u16`
/// of [`ColumnMeta::ordinal`] stops the list rather than taking the ordinal of another
/// column; the widest description of this crate is far shorter than that (unit test
/// `the_internal_columns_are_the_declared_ones_numbered_from_zero`).
///
/// An internal table carries neither a default, an `IDENTITY` nor a computed column: the
/// description has no field for one ([`InternalTableDef`], a name and a type per column).
fn internal_columns(def: &InternalTableDef) -> Vec<ColumnMeta> {
    let mut columns = Vec::with_capacity(def.columns.len());
    for (position, column) in def.columns.iter().enumerate() {
        let Ok(ordinal) = u16::try_from(position) else {
            break;
        };
        columns.push(ColumnMeta {
            id: ColumnId(i32::from(ordinal) + 1),
            name: column.name.clone(),
            ty: column.ty.clone(),
            ordinal,
            default: None,
            identity: None,
            computed: None,
        });
    }
    columns
}

/// The databases the rows of `vauban_sys_databases` visible to `txn` describe.
///
/// A row this function cannot read — a `database_id` that is not a non-negative `int`, a
/// name that is not a string — is skipped rather than turned into an error: one damaged row
/// would otherwise empty the whole snapshot ([`build`]), and a database that is not read is
/// a database that resolves to nothing. The two writers of that table, `bootstrap.rs` and
/// `database.rs`, write both columns (unit test `a_row_of_the_wrong_shape_is_skipped`).
///
/// The collation is [`Collation::parse`] of the `collation_name` column, and
/// [`Collation::DEFAULT`] when that column is `NULL` or holds a name that parses to nothing.
/// `database.rs` leaves the name `NULL` for a collation other than the default,
/// `Collation::parse` having no inverse (rustdoc of `create_database`), so a database created
/// with another collation is read here with the default one. What that changes is the
/// comparison of the names inside it; `SQL_Latin1_General_CP1_CI_AS` is served for each
/// database.
///
/// # Errors
///
/// The error of the scan, and the internal bug of a storage that carries no
/// `vauban_sys_databases` table.
fn databases_of(catalog: &Catalog, txn: &TxnHandle) -> SqlResult<Vec<DatabaseMeta>> {
    let table = internal_table_id(catalog, DATABASES_TABLE)?.ok_or_else(|| {
        InternalError::Bug(format!(
            "snapshot: internal table {DATABASES_TABLE} is not in this storage"
        ))
    })?;
    let snapshot = catalog.txn.statement_snapshot(txn);
    let mut databases = Vec::new();
    for row in catalog.storage.scan(&snapshot, table)? {
        let (_, values) = row?;
        if let Some(database) = database_meta(&values.0) {
            databases.push(database);
        }
    }
    Ok(databases)
}

/// The [`DatabaseMeta`] a row of `vauban_sys_databases` describes, `None` for a row whose
/// identifier or name is not of the type its column was declared with ([`databases_of`]).
///
/// The two versioning options are read from their columns, and a row that
/// carries something else there falls back on the couple a fresh database is created with
/// (`database::NEW_DATABASE_OPTIONS`), as `collation_name` falls back on the default
/// collation just below.
fn database_meta(values: &[Value]) -> Option<DatabaseMeta> {
    let Some(&Value::I32(id)) = values.get(databases_columns::DATABASE_ID) else {
        return None;
    };
    let Some(Value::String(name)) = values.get(databases_columns::NAME) else {
        return None;
    };
    let collation = match values.get(databases_columns::COLLATION_NAME) {
        Some(Value::String(stored)) => Collation::parse(&stored.text).unwrap_or(Collation::DEFAULT),
        _ => Collation::DEFAULT,
    };
    let read_committed_snapshot = match values.get(databases_columns::READ_COMMITTED_SNAPSHOT) {
        Some(&Value::Bit(on)) => on,
        _ => crate::database::NEW_DATABASE_OPTIONS.0,
    };
    let snapshot_isolation = match values.get(databases_columns::SNAPSHOT_ISOLATION_STATE) {
        Some(&Value::I8(state)) => SnapshotIsolationState::from_state(state)
            .unwrap_or(crate::database::NEW_DATABASE_OPTIONS.1),
        _ => crate::database::NEW_DATABASE_OPTIONS.1,
    };
    Some(DatabaseMeta {
        id: DbId(u32::try_from(id).ok()?),
        name: name.text.clone(),
        collation,
        read_committed_snapshot,
        snapshot_isolation,
    })
}

/// Whether two identifiers are the same name under `collation`.
fn same_name(collation: Collation, left: &str, right: &str) -> bool {
    collation.compare(left, right) == std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vauban_storage::{MemoryStorage, Storage};
    use vauban_txn::{IsolationLevel, TransactionManager};

    #[test]
    fn an_empty_snapshot_resolves_nothing() {
        let snapshot = CatalogSnapshot::default();
        assert!(snapshot.database("master").is_none());
        assert!(
            snapshot
                .resolve_object("master", Some("sys"), "objects", "dbo")
                .is_none()
        );
        assert!(snapshot.table(ObjectId(1)).is_none());
        assert!(snapshot.view_definition(ObjectId(1)).is_none());
        assert!(snapshot.indexes_of(ObjectId(1)).is_empty());
        assert!(snapshot.object_name(ObjectId(1)).is_none());
    }

    #[test]
    fn a_catalogue_without_internal_tables_gives_an_empty_snapshot() {
        // A `Catalog` built by hand over a storage no bootstrap touched: the scan of
        // `vauban_sys_databases` has no table to read and `build` gives back the empty
        // snapshot.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog {
            storage,
            txn: Arc::clone(&manager),
            tables: std::sync::Mutex::default(),
        };
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snapshot = build(&catalog, &handle);
        assert!(snapshot.databases.is_empty());
        assert!(snapshot.database("master").is_none());
        // Counter-proof that the emptiness comes from the missing table and not from
        // `build`: the same construction over a bootstrapped storage reads four databases.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        assert_eq!(build(&catalog, &handle).databases.len(), 4);
    }

    #[test]
    fn a_row_of_the_wrong_shape_is_skipped() {
        // The shapes `database_meta` refuses, then the one it reads.
        assert!(database_meta(&[]).is_none());
        assert!(database_meta(&[Value::Null, Value::Null, Value::Null]).is_none());
        assert!(
            database_meta(&[Value::I32(-1), text("d"), Value::Null]).is_none(),
            "a negative database_id is no DbId"
        );
        assert!(database_meta(&[Value::I32(5), Value::I32(5), Value::Null]).is_none());
        let read = database_meta(&[Value::I32(5), text("d"), Value::Null])
            .expect("an int and a string make a database");
        assert_eq!(read.id, DbId(5));
        assert_eq!(read.name, "d");
        assert_eq!(
            read.collation,
            Collation::DEFAULT,
            "a NULL collation_name is read as the default collation"
        );
    }

    #[test]
    fn the_rank_rule_numbers_the_distinct_names_from_minus_one() {
        // Three distinct names out of five descriptions: the same view installed in another
        // database, and the same name under the collation of `master`, are one entry.
        let described = vec![
            view("master", "sys", "tables"),
            view("tempdb", "sys", "tables"),
            view("model", "SYS", "TABLES"),
            view("master", "sys", "objects"),
            view("master", "INFORMATION_SCHEMA", "TABLES"),
        ];
        let objects = system_view_objects(&described, &master());
        let built: Vec<(ObjectId, String, String)> = objects
            .iter()
            .map(|object| {
                (
                    object.id,
                    object.name.schema.clone(),
                    object.name.name.clone(),
                )
            })
            .collect();
        assert_eq!(
            built,
            vec![
                (ObjectId(-1), "sys".to_owned(), "tables".to_owned()),
                (ObjectId(-2), "sys".to_owned(), "objects".to_owned()),
                (
                    ObjectId(-3),
                    "INFORMATION_SCHEMA".to_owned(),
                    "TABLES".to_owned()
                ),
            ],
            "one entry per distinct name, numbered by rank, spelled as first described"
        );
        let in_master: Vec<String> = objects
            .iter()
            .map(|object| object.name.database.clone())
            .collect();
        assert_eq!(in_master, vec!["master".to_owned(); 3]);
        let kinds: Vec<ObjectKind> = objects.iter().map(|object| object.kind).collect();
        assert_eq!(kinds, vec![ObjectKind::View; 3]);
        assert_eq!(
            objects[0].definition.as_deref(),
            Some("SELECT 1 AS one -- sys.tables"),
            "the text of the description it was built from"
        );
        assert_eq!(objects[0].database, master().id);
        assert_eq!(objects[0].parent, None);
    }

    #[test]
    fn a_hand_built_view_of_information_schema_resolves() {
        // The lookup on a description written here, without a storage behind it: it reads the
        // schema of the entry and knows no list of schemas. The view `views/info_schema.rs`
        // describes is checked through the public API by `tests/snapshot.rs`,
        // `information_schema_tables_resolves`.
        let snapshot = hand_built(&[view("master", "INFORMATION_SCHEMA", "TABLES")]);
        let resolved = snapshot
            .resolve_object("master", Some("INFORMATION_SCHEMA"), "TABLES", "dbo")
            .expect("INFORMATION_SCHEMA.TABLES resolves from master");
        assert_eq!(resolved.id, ObjectId(-1));
        assert_eq!(resolved.kind, ObjectKind::View);
        assert_eq!(
            snapshot
                .resolve_object("d", Some("information_schema"), "tables", "dbo")
                .map(|object| object.id),
            Some(ObjectId(-1)),
            "visible from a user database, the case of the two parts folded"
        );
        assert_eq!(
            snapshot.view_definition(ObjectId(-1)),
            Some("SELECT 1 AS one -- INFORMATION_SCHEMA.TABLES")
        );
        assert!(snapshot.table(ObjectId(-1)).is_none());
        assert_eq!(
            snapshot.object_name(ObjectId(-1)),
            Some(QualifiedName {
                database: "master".to_owned(),
                schema: "INFORMATION_SCHEMA".to_owned(),
                name: "TABLES".to_owned(),
            })
        );
        assert!(
            snapshot
                .resolve_object("master", Some("sys"), "TABLES", "dbo")
                .is_none(),
            "the schema is compared: the view of INFORMATION_SCHEMA is not one of `sys`"
        );
        assert!(
            snapshot
                .resolve_object("master", None, "TABLES", "dbo")
                .is_none(),
            "unqualified, under `dbo`: nothing, as SQL Server answers error 208"
        );
    }

    #[test]
    fn the_described_views_are_those_of_the_files() {
        let described = described_views();
        let objects = system_view_objects(&described, &master());
        let names: Vec<String> = objects
            .iter()
            .map(|object| format!("{}.{}", object.name.schema, object.name.name))
            .collect();
        // `views/sys_tables.rs` is read first, then `views/sys_core.rs`; the two views of the
        // tables of the bootstrap sit at ranks 16 and 17, before the seven of
        // `views/sys_extra.rs`, which are the last described.
        let leading: Vec<&String> = names.iter().take(4).collect();
        assert_eq!(
            leading,
            vec!["sys.objects", "sys.tables", "sys.columns", "sys.types"]
        );
        assert_eq!(
            names.get(15..17),
            Some(["sys.databases".to_owned(), "sys.schemas".to_owned()].as_slice()),
            "-16 and -17, as the rows of `vauban_sys_all_objects` number them"
        );
        let trailing: Vec<&String> = names.iter().rev().take(2).collect();
        assert_eq!(trailing, vec!["sys.master_files", "sys.database_files"]);
        // The six files of `views/` are filled, for 24 distinct names, each described once per
        // system database, which is what the `views_of` of those files does: 96 descriptions.
        assert_eq!(described.len(), names.len() * SYSTEM_DATABASES.len());
        assert_eq!(named_once(&names), names, "a name is described twice");
        assert_eq!(names.len(), 24);
        assert_eq!(described.len(), 96);
        // The identifiers are negative and run from -1 without a hole, so two views of the
        // 24 do not share one: the rank rule numbers the distinct names (module
        // documentation, section "The identifiers of the system views are the rows of
        // `sys.all_objects`").
        let ids: Vec<i32> = objects.iter().map(|object| object.id.0).collect();
        let by_rank: Vec<i32> = (1..=i32::try_from(names.len()).expect("24 fits in an i32"))
            .map(|rank| -rank)
            .collect();
        assert_eq!(ids, by_rank);
    }

    #[test]
    fn system_view_ids_match_sys_all_objects() {
        // The identifier a snapshot answers for a system view is the `object_id` of the row
        // `views/sys_extra.rs` writes for it in `vauban_sys_all_objects`, the table
        // `sys.all_objects` reads. The two lists are compared in order, with the name
        // and the `schema_id` of each row, so a divergence of order is caught as well as a
        // divergence of number (`views/sys_extra.rs`, `installed_system_views`).
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snapshot = build(&catalog, &handle);
        let of_the_snapshot: Vec<(i32, String, i32)> = snapshot
            .system_views
            .iter()
            .filter_map(|id| snapshot.objects.get(id))
            .map(|object| {
                (
                    object.id.0,
                    object.name.name.clone(),
                    schema_id_of(&object.name.schema),
                )
            })
            .collect();
        let of_the_rows = described_system_view_rows();
        assert_eq!(of_the_snapshot, of_the_rows);
        assert_eq!(of_the_snapshot.len(), 24, "{of_the_snapshot:?}");
        // Counter-proof that the comparison reads something: the rows are there, negative,
        // and the first of them is the one of `sys.objects`.
        assert_eq!(
            of_the_rows.first().map(|&(id, _, _)| id),
            Some(-1),
            "{of_the_rows:?}"
        );
        assert_eq!(
            of_the_rows.last().map(|&(id, _, _)| id),
            Some(-24),
            "{of_the_rows:?}"
        );
    }

    #[test]
    fn the_internal_tables_are_numbered_from_their_position() {
        // The rule of the module documentation: the description of position `n` is the table
        // of identifier `FIRST_INTERNAL_TABLE_OBJECT_ID + n`, and the three ranges of
        // identifiers of a snapshot are apart.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snapshot = build(&catalog, &handle);

        let described = internal_table_defs().expect("the descriptions of the bootstrap");
        let expected: Vec<(ObjectId, String)> = described
            .iter()
            .enumerate()
            .map(|(position, def)| {
                let rank = i32::try_from(position).expect("a position fits in an i32");
                (
                    ObjectId(FIRST_INTERNAL_TABLE_OBJECT_ID + rank),
                    def.name.clone(),
                )
            })
            .collect();
        let held: Vec<(ObjectId, String)> = snapshot
            .tables
            .values()
            .map(|meta| (meta.id, meta.name.clone()))
            .collect();
        assert_eq!(held, expected);
        assert_eq!(held.len(), expected.len(), "{held:?}");
        // The ranges: the views below zero, a constraint at zero
        // (`views/sys_constraints.rs`), the internal tables from 100_000 and the user tables
        // from 1_000_000, which leaves 900_000 numbers between the two last ones.
        assert!(snapshot.system_views.iter().all(|id| id.0 < 0));
        let first = held.first().map(|&(id, _)| id.0);
        assert_eq!(first, Some(100_000), "positive, as in SQL Server");
        let last = FIRST_INTERNAL_TABLE_OBJECT_ID
            + i32::try_from(held.len()).expect("the table count fits in an i32")
            - 1;
        assert!(
            last < crate::table::FIRST_USER_OBJECT_ID,
            "{last} is in the range of the user tables"
        );
        assert_eq!(
            crate::table::FIRST_USER_OBJECT_ID - FIRST_INTERNAL_TABLE_OBJECT_ID,
            900_000
        );
        // Two snapshots of two transactions answer the same identifiers: the position is read
        // from the code, not from a counter.
        let other = build(&catalog, &manager.begin(IsolationLevel::ReadCommitted));
        let again: Vec<(ObjectId, String)> = other
            .tables
            .values()
            .map(|meta| (meta.id, meta.name.clone()))
            .collect();
        assert_eq!(again, expected);
    }

    #[test]
    fn the_storage_id_of_an_internal_table_is_the_one_internal_table_id_finds() {
        // Needs `bootstrap::internal_table_id`, which is `pub(crate)`: the integration test
        // `tests/snapshot.rs`, `vauban_sys_databases_resolves_to_a_table_with_its_columns`
        // reads the rows behind that identifier instead.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let catalog = Catalog::bootstrap(storage, Arc::clone(&manager)).expect("bootstrap");
        let handle = manager.begin(IsolationLevel::ReadCommitted);
        let snapshot = build(&catalog, &handle);

        for def in internal_table_defs().expect("the descriptions of the bootstrap") {
            let object = snapshot
                .resolve_object("master", Some("dbo"), &def.name, "dbo")
                .unwrap_or_else(|| panic!("master.dbo.{} resolves", def.name));
            let meta = snapshot
                .table(object.id)
                .unwrap_or_else(|| panic!("{} is a table of the snapshot", def.name));
            assert_eq!(
                Some(meta.storage_id),
                internal_table_id(&catalog, &def.name).expect("internal_table_id"),
                "{}",
                def.name
            );
        }
    }

    #[test]
    fn a_description_without_its_table_is_skipped() {
        // A `master` holding three tables while the bootstrap of this build describes twenty:
        // the three first descriptions are paired with the three tables, by position, and the
        // rest are left out rather than paired with the table of another description.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let manager = Arc::new(TransactionManager::new(Arc::clone(&storage)));
        let master = storage.create_database("master").expect("create_database");
        let shape = vauban_storage::TableShape {
            columns: vec![vauban_types::TypeInfo::new(
                vauban_types::SqlType::Int,
                false,
            )],
            clustered_key: None,
        };
        let created: Vec<vauban_storage::TableId> = (0..3)
            .map(|_| {
                storage
                    .create_table(master, &shape)
                    .expect("create_table of a table of master")
            })
            .collect();
        let catalog = Catalog {
            storage,
            txn: Arc::clone(&manager),
            tables: std::sync::Mutex::default(),
        };
        let meta = DatabaseMeta {
            id: master,
            name: "master".to_owned(),
            collation: Collation::DEFAULT,
            read_committed_snapshot: false,
            snapshot_isolation: SnapshotIsolationState::On,
        };

        let built = internal_table_objects(&catalog, &meta).expect("the three tables");
        let described = internal_table_defs().expect("the descriptions of the bootstrap");
        assert_eq!(built.len(), 3, "three tables, three entries");
        assert!(described.len() > built.len(), "{}", described.len());
        for (position, (object, table)) in built.iter().enumerate() {
            let rank = i32::try_from(position).expect("a position fits in an i32");
            assert_eq!(object.id, ObjectId(FIRST_INTERNAL_TABLE_OBJECT_ID + rank));
            assert_eq!(object.id, table.id);
            assert_eq!(object.kind, ObjectKind::Table);
            assert_eq!(object.definition, None);
            assert_eq!(object.parent, None);
            assert_eq!(object.database, master);
            assert_eq!(table.database, master);
            assert_eq!(table.storage_id, created[position]);
            assert_eq!(table.name, described[position].name);
            assert_eq!(object.name.name, described[position].name);
            assert_eq!(object.name.schema, "dbo");
            assert_eq!(object.name.database, "master");
            assert_eq!(table.clustered, None);
            assert!(table.constraints.is_empty());
        }
    }

    #[test]
    fn the_internal_columns_are_the_declared_ones_numbered_from_zero() {
        let described = internal_table_defs().expect("the descriptions of the bootstrap");
        let def = described
            .iter()
            .find(|def| def.name == DATABASES_TABLE)
            .expect("the table of the databases is described");
        let columns = internal_columns(def);
        let read: Vec<(i32, String, u16, bool)> = columns
            .iter()
            .map(|column| {
                (
                    column.id.0,
                    column.name.clone(),
                    column.ordinal,
                    column.ty.nullable,
                )
            })
            .collect();
        assert_eq!(
            read,
            vec![
                (1, "database_id".to_owned(), 0, false),
                (2, "name".to_owned(), 1, false),
                (3, "collation_name".to_owned(), 2, true),
                (4, "is_read_committed_snapshot_on".to_owned(), 3, false),
                (5, "snapshot_isolation_state".to_owned(), 4, false),
                (6, "owner_sid".to_owned(), 5, true),
                (7, "create_date".to_owned(), 6, false),
            ],
            "the identifier counts from 1, the ordinal from 0, the nullability is the declared one"
        );
        let types: Vec<&vauban_types::TypeInfo> = columns.iter().map(|column| &column.ty).collect();
        let declared: Vec<&vauban_types::TypeInfo> = def.columns.iter().map(|c| &c.ty).collect();
        assert_eq!(
            types, declared,
            "the types of the description, collation included"
        );
        for column in &columns {
            assert_eq!(column.default, None);
            assert_eq!(column.identity, None);
            assert_eq!(column.computed, None);
        }
    }

    /// The `schema_id` the bootstrap gives the schema `schema`, as the rows of
    /// `vauban_sys_all_objects` write it: `4` for `sys`, `3` for `INFORMATION_SCHEMA`.
    fn schema_id_of(schema: &str) -> i32 {
        crate::bootstrap::SYSTEM_SCHEMAS
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(schema))
            .map(|&(_, id, _)| id)
            .unwrap_or_else(|| panic!("{schema} is a schema of the bootstrap"))
    }

    /// `names` with the repeated entries dropped, the order kept: `names` itself when each
    /// name is there once.
    fn named_once(names: &[String]) -> Vec<String> {
        let mut once: Vec<String> = Vec::new();
        for name in names {
            if !once.contains(name) {
                once.push(name.clone());
            }
        }
        once
    }

    /// The description of the view `schema.name` installed in the database `db`, with a text
    /// that names it: what a file of `views/` hands over, without its select list.
    fn view(db: &str, schema: &str, name: &str) -> SystemViewDef {
        SystemViewDef {
            name: QualifiedName {
                database: db.to_owned(),
                schema: schema.to_owned(),
                name: name.to_owned(),
            },
            definition: format!("SELECT 1 AS one -- {schema}.{name}"),
        }
    }

    /// The `master` the entries of a hand-built snapshot are named in.
    fn master() -> DatabaseMeta {
        DatabaseMeta {
            id: DbId(1),
            name: "master".to_owned(),
            collation: Collation::DEFAULT,
            // The couple the bootstrap writes for `master`
            // (`bootstrap::SYSTEM_DATABASE_OPTIONS`).
            read_committed_snapshot: false,
            snapshot_isolation: SnapshotIsolationState::On,
        }
    }

    /// A snapshot holding `master`, a user database `d` and the system views `described`,
    /// without a storage behind it: the shape [`read`] builds, written here so that a view no
    /// file of `views/` describes yet can be checked.
    fn hand_built(described: &[SystemViewDef]) -> CatalogSnapshot {
        let mut objects = BTreeMap::new();
        let mut system_views = Vec::new();
        for object in system_view_objects(described, &master()) {
            system_views.push(object.id);
            objects.insert(object.id, object);
        }
        CatalogSnapshot {
            databases: vec![
                master(),
                DatabaseMeta {
                    id: DbId(7),
                    name: "d".to_owned(),
                    collation: Collation::DEFAULT,
                    // The couple of a database `CREATE DATABASE` makes
                    // (`database::NEW_DATABASE_OPTIONS`).
                    read_committed_snapshot: false,
                    snapshot_isolation: SnapshotIsolationState::Off,
                },
            ],
            objects,
            system_views,
            tables: BTreeMap::new(),
            indexes: BTreeMap::new(),
        }
    }

    /// The `nvarchar` value of a piece of text, as the internal tables store it.
    fn text(value: &str) -> Value {
        Value::String(vauban_types::SqlString {
            text: value.to_owned(),
        })
    }
}
