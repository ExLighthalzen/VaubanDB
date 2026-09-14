#![deny(missing_docs)]
//! Crate `vauban-catalog`: the metadata of the instance — databases, schemas, tables,
//! columns, indexes, constraints — and the `sys.*` / `INFORMATION_SCHEMA` views built on
//! them.
//!
//! # Metadata, definitions, identifiers
//!
//! - A `*Meta` ([`TableMeta`], [`ColumnMeta`], [`IndexMeta`], [`ConstraintMeta`],
//!   [`ObjectMeta`], [`DatabaseMeta`]) describes an object the catalogue **holds**.
//! - A `*Def` ([`TableDef`], [`IndexDef`]) describes an object a caller **asks for**. The
//!   translation from the AST of `parser` is done by this crate, not by its callers.
//! - An [`ObjectId`] is what a client reads in `sys.objects.object_id`; a
//!   [`TableId`](vauban_storage::TableId) is what `storage` handed out when the table was
//!   created. [`TableMeta`] carries both.
//!
//! # File map
//!
//! | File | Content |
//! |---|---|
//! | `ids.rs` | [`ObjectId`], [`ColumnId`] |
//! | `meta.rs` | [`QualifiedName`], [`IdentitySpec`], the `*Meta` types, [`ObjectKind`], [`AlterTable`] |
//! | `def.rs` | [`TableDef`], [`IndexDef`], [`ColumnDef`], [`ConstraintDef`], [`SortedColumn`], the internal table description |
//! | `catalog.rs` | [`Catalog`]: its fields and the dispatch of each method |
//! | `snapshot.rs` | [`CatalogSnapshot`]: type and name resolution |
//! | `bootstrap.rs` | system databases, schemas, internal tables |
//! | `database.rs` | `create_database` / `drop_database` |
//! | `table.rs` | `create_table` / `alter_table` / `drop_table` |
//! | `index.rs` | indexes, `PRIMARY KEY` and `UNIQUE` constraints |
//! | `constraints.rs` | `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints |
//! | `identity.rs` | `next_identity` and its autonomous transaction |
//! | `sys_rows.rs` | the rows the internal tables carry about a user table, written at each DDL |
//! | `views/` | one file per group of system views, listed in `views/mod.rs` |
//!
//! The internal tables are named `vauban_sys_*` and are not published to clients; what a
//! client reads is a view over them.

mod bootstrap;
mod catalog;
mod constraints;
mod database;
mod def;
mod identity;
mod ids;
mod index;
mod meta;
mod snapshot;
mod sys_rows;
mod table;
mod views;

pub use catalog::Catalog;
pub use def::{ColumnDef, ConstraintDef, IndexDef, SortedColumn, TableDef};
pub use ids::{ColumnId, ObjectId};
pub use meta::{
    AlterTable, ColumnMeta, ConstraintMeta, DatabaseMeta, IdentitySpec, IndexMeta, ObjectKind,
    ObjectMeta, QualifiedName, TableMeta,
};
pub use snapshot::CatalogSnapshot;
// Re-exported so that a crate which names a table of `storage` through the catalogue —
// `binder`, for `LogicalPlan::Scan` — does not have to depend on `vauban-storage`.
pub use vauban_storage::TableId;
