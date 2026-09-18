//! The `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints of a `CREATE TABLE`, as objects of
//! the catalogue.
//!
//! # One object per constraint
//!
//! `index.rs` makes a `PRIMARY KEY` and a `UNIQUE` into indexes, named by their
//! [`IndexMeta`]. The three kinds here have no index: each one becomes an [`ObjectMeta`] of
//! kind [`ObjectKind::Constraint`], with an [`ObjectId`] taken from the counter of the tables
//! ([`TableStore::take_object_id`]), the table as `parent`, and the schema and database of
//! that table. The objects live in [`ConstraintStore`], the `constraints` field of the
//! [`TableStore`] of `table.rs`, beside the [`IndexStore`](crate::index::IndexStore) and for
//! the reason written there: the rows of the internal tables of `views/` are written by
//! `sys_rows.rs`, which does not walk the four tables of `views/sys_constraints.rs`, so what
//! a client reads through `sys.foreign_keys` and its three neighbours is empty.
//!
//! The [`ConstraintMeta`] the table carries holds the identifier of that object
//! ([`ConstraintMeta::ForeignKey`] and its two neighbours), so a reader goes from the table
//! to the object without a search (`tests/constraints.rs`,
//! `named_constraints_become_objects`).
//!
//! # What the code below keys on
//!
//! | Statement | Answer |
//! |---|---|
//! | `CREATE TABLE dbo.d1 (a int NULL CONSTRAINT ck_dup CHECK (a > 0));` then the same constraint name on `dbo.d2` | 2714 state 5, the constraint name printed without its schema |
//! | the same two constraint names, one in `dbo`, one in another schema | accepted: `sys.objects` gives two rows of that name, one per schema |
//! | a constraint named like a table of its schema | 2714 state 5 |
//! | a `CHECK`, a `DEFAULT` or a `FOREIGN KEY` named like the table of its own `CREATE TABLE` | 2714 state 5 |
//! | two constraints of one name in one `CREATE TABLE`, `CHECK`/`CHECK` or `CHECK`/`DEFAULT` | 8168 state 0 |
//! | `FOREIGN KEY (nosuch) REFERENCES dbo.g1 (id)` | 1769 state 1, the referencing table printed unqualified |
//! | `FOREIGN KEY (a) REFERENCES dbo.h1 (id)`, the referenced table without key | 1776 state 0, the referenced table printed qualified |
//! | `REFERENCES dbo.z1 (r, l)` against the key `(l, r)`, and `REFERENCES dbo.y1 (r)` against the same key | 1776 both times: the referenced columns are read in key order, a prefix or a permutation matching no key |
//! | `REFERENCES dbo.x1` without a column list | accepted, `key_index_id` 1 and `referenced_column` `id`: the primary key of the referenced table |
//! | `REFERENCES dbo.m1 (id)` where `id` carries a `CREATE UNIQUE INDEX` and no `PRIMARY KEY` | accepted, `key_index_id` 2, that unique index |
//! | `DROP TABLE dbo.o1` while `dbo.o2` points at it | 3726 state 1 |
//! | the child dropped first, then the parent | both accepted |
//! | `DROP TABLE dbo.s`, whose foreign key points at itself | accepted |
//!
//! # Seven numbers this file does not raise
//!
//! The seven rows below are numbers `vauban-errors` does not carry; 8168, in the table
//! above, is an eighth, raised by [`register`]. Each one answers an [`InternalError::Bug`]
//! naming the number and the shape, as `index.rs` does for the eight numbers it names
//! (`index.rs`, section "Eight numbers this file does not raise"):
//!
//! | Statement | Number |
//! |---|---|
//! | `REFERENCES dbo.u1` without a column list, the referenced table carrying a `CREATE UNIQUE INDEX` and no `PRIMARY KEY`; the same shape on a table which carries neither | 1773 state 0 |
//! | `FOREIGN KEY (a) REFERENCES master.dbo.p (id)` from another database | 1763 state 0 |
//! | `REFERENCES dbo.nosuch (id)` | 1767 state 0 |
//! | `REFERENCES dbo.l1 (nosuch)` | 1770 state 0 |
//! | `FOREIGN KEY (a, b) REFERENCES dbo.i1 (id)` | 8139 state 0 |
//! | `int` pointing at `varchar(10)` | 1778 state 0 |
//! | `varchar(20)` pointing at `varchar(10)` | 1753 state 0 |
//!
//! A column declared `NOT NULL` pointing at a `PRIMARY KEY` is accepted, so
//! [`compatible_types`] reads the [`SqlType`] and leaves `nullable` out.
//!
//! # What this file does not do
//!
//! - apply a constraint to a row: 547 and the three-valued `CHECK` belong to the executor.
//!   `ON DELETE` and `ON UPDATE` are stored as declared and not applied;
//! - resolve the single column a `CHECK` names, which SQL Server publishes as
//!   `parent_column_id` (`views/sys_constraints.rs`, module documentation);
//! - reprint a `CHECK` as SQL Server stores it. The text kept in
//!   [`ConstraintMeta::Check::definition`] is the expression printed between parentheses,
//!   where SQL Server publishes `([a]>=(0))` for `CHECK (a >= 0)` and `([b]<>'z' AND
//!   [a]<(10))` for `CHECK (b <> 'z' AND a < 10)`: the brackets around an identifier and the
//!   parentheses around a literal are not written here.

use std::collections::BTreeMap;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{Expr, RefAction};
use vauban_storage::{DbId, IndexId};
use vauban_types::TypeInfo;

use crate::catalog::Catalog;
use crate::def::{ConstraintDef, TableDef};
use crate::ids::{ColumnId, ObjectId};
use crate::meta::{
    ColumnMeta, ConstraintMeta, IndexMeta, ObjectKind, ObjectMeta, QualifiedName, TableMeta,
};
use crate::table::{self, TableStore};
use crate::views::sys_constraints::generated_constraint_name;

/// The state error 2714 sends when the name taken is that of a **constraint**, where
/// `vauban-errors` keys its constructor on the state of a `CREATE TABLE`.
///
/// State 5 on three shapes — a second `CHECK` of one name, a `CHECK` and a `DEFAULT` of
/// one name in two tables, and a constraint named like a table of the schema — against
/// state 6 for a second `CREATE TABLE` of one name (`vauban-errors`,
/// `OBJECT_EXISTS_2714_TABLE_STATE`). Frozen by `tests/constraints.rs`,
/// `duplicate_constraint_name_is_2714`.
const CONSTRAINT_EXISTS_2714_STATE: u8 = 5;

/// The constraint objects the catalogue has created, by [`ObjectId`].
///
/// Held by the `constraints` field of the [`TableStore`] of `table.rs`; `constraints.rs`
/// owns the type and the accesses to it, as `index.rs` owns the index store. An entry is an
/// [`ObjectMeta`] of kind [`ObjectKind::Constraint`] whose `parent` is the table.
#[derive(Debug, Default)]
pub(crate) struct ConstraintStore {
    /// One entry per constraint of a table this catalogue holds.
    pub(crate) entries: BTreeMap<ObjectId, ObjectMeta>,
}

impl ConstraintStore {
    /// The constraint objects of the table `table`, by increasing [`ObjectId`], which is the
    /// order they were declared in.
    ///
    /// The read side of the store: `Catalog::constraints_of` answers with it and
    /// `views/sys_constraints.rs` builds its rows from it (`tests/constraints.rs`,
    /// `named_constraints_become_objects`).
    pub(crate) fn of_table(&self, table: ObjectId) -> Vec<&ObjectMeta> {
        self.entries
            .values()
            .filter(|object| object.parent == Some(table))
            .collect()
    }

    /// The constraint of the database `database` called `schema.name`, `None` when the two
    /// parts match no entry.
    ///
    /// Compared with `eq_ignore_ascii_case`, as `table.rs` compares the names of tables and
    /// for the reason written there: folding as the collation of the database does asks for
    /// the resolution of `snapshot.rs`.
    fn named(&self, database: DbId, schema: &str, name: &str) -> Option<&ObjectMeta> {
        self.entries.values().find(|object| {
            object.database == database
                && object.name.schema.eq_ignore_ascii_case(schema)
                && object.name.name.eq_ignore_ascii_case(name)
        })
    }
}

/// One foreign key that points at a table: what
/// [`Catalog::referencing_foreign_keys`] answers.
///
/// The executor reads it for a `DELETE`, to find the rows a child table holds for the row
/// being deleted; `drop_table` reads it to refuse 3726 (`tests/constraints.rs`,
/// `referencing_foreign_keys_lists_the_child`, `drop_table_referenced_is_3726`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencingForeignKey {
    /// Table that carries the foreign key — the child.
    pub table: ObjectId,
    /// The constraint object, whose name is read through
    /// [`Catalog::constraints_of`].
    pub constraint: ObjectId,
    /// Columns of the child, in the order they were declared.
    pub columns: Vec<ColumnId>,
    /// Columns of the referenced table, in the same order.
    pub referenced_columns: Vec<ColumnId>,
    /// `ON DELETE` action as declared. Stored and not applied (module documentation).
    pub on_delete: RefAction,
    /// `ON UPDATE` action as declared, under the same rule.
    pub on_update: RefAction,
}

impl Catalog {
    /// The foreign keys that point at `table`, by increasing child [`ObjectId`].
    ///
    /// A table no foreign key points at answers an empty vector, and a foreign key of the
    /// table pointing at itself is in the answer — `drop_table` is what leaves that one out,
    /// SQL Server accepting the `DROP` of a self-referencing table (module documentation;
    /// `tests/constraints.rs`, `referencing_foreign_keys_lists_the_child`).
    ///
    /// On [`Catalog`] and not on [`CatalogSnapshot`](crate::CatalogSnapshot): the fields of
    /// that type are private to `snapshot.rs`, so a block written here cannot read the
    /// tables it holds.
    ///
    /// # Errors
    ///
    /// The error of the `storage` calls [`table::refresh`] makes.
    pub fn referencing_foreign_keys(
        &self,
        table: ObjectId,
    ) -> SqlResult<Vec<ReferencingForeignKey>> {
        let mut store = table::store(self);
        table::refresh(self, &mut store)?;
        forget_dropped(&mut store);
        let tables: Vec<TableMeta> = store.live().cloned().collect();
        Ok(referencing_foreign_keys(&tables, table))
    }

    /// The constraint objects of `table` — its `FOREIGN KEY`, `CHECK` and `DEFAULT` — by
    /// increasing [`ObjectId`], each one carrying its name, its three-part name and the
    /// table as `parent`.
    ///
    /// A `PRIMARY KEY` and a `UNIQUE` are not here: `index.rs` names them through their index
    /// (`CatalogSnapshot::indexes_of`). An empty vector for a table this catalogue does not
    /// hold (`tests/constraints.rs`, `named_constraints_become_objects`).
    ///
    /// # Errors
    ///
    /// The error of the `storage` calls [`table::refresh`] makes.
    pub fn constraints_of(&self, table: ObjectId) -> SqlResult<Vec<ObjectMeta>> {
        let mut store = table::store(self);
        table::refresh(self, &mut store)?;
        forget_dropped(&mut store);
        Ok(store
            .constraints
            .of_table(table)
            .into_iter()
            .cloned()
            .collect())
    }

    /// The constraint of the database `database` written `schema.name`, `None` when the name
    /// matches no constraint of this catalogue.
    ///
    /// The name of a constraint is resolved here and not through
    /// [`CatalogSnapshot::resolve_object`](crate::CatalogSnapshot::resolve_object), for the
    /// reason written on [`Catalog::referencing_foreign_keys`] (`tests/constraints.rs`,
    /// `named_constraints_become_objects`).
    ///
    /// # Errors
    ///
    /// The error of the `storage` calls [`table::refresh`] makes.
    pub fn resolve_constraint(
        &self,
        database: DbId,
        schema: &str,
        name: &str,
    ) -> SqlResult<Option<ObjectMeta>> {
        let mut store = table::store(self);
        table::refresh(self, &mut store)?;
        forget_dropped(&mut store);
        Ok(store.constraints.named(database, schema, name).cloned())
    }
}

/// The foreign keys of `tables` that point at `table`, by increasing child [`ObjectId`].
///
/// The pure half of [`Catalog::referencing_foreign_keys`]: `drop_table` calls it with the
/// live tables of the store it already holds (`table.rs`).
pub(crate) fn referencing_foreign_keys(
    tables: &[TableMeta],
    table: ObjectId,
) -> Vec<ReferencingForeignKey> {
    let mut found: Vec<ReferencingForeignKey> = Vec::new();
    for child in tables {
        for constraint in &child.constraints {
            let ConstraintMeta::ForeignKey {
                constraint,
                columns,
                referenced_table,
                referenced_columns,
                on_delete,
                on_update,
                ..
            } = constraint
            else {
                continue;
            };
            if *referenced_table != table {
                continue;
            }
            found.push(ReferencingForeignKey {
                table: child.id,
                constraint: *constraint,
                columns: columns.clone(),
                referenced_columns: referenced_columns.clone(),
                on_delete: *on_delete,
                on_update: *on_update,
            });
        }
    }
    found.sort_by_key(|key| (key.table, key.constraint));
    found
}

/// `def` with its `FOREIGN KEY`, `CHECK` and `DEFAULT` constraints left out.
///
/// `create_table` hands this to [`crate::index::table_keys`], which owns the `PRIMARY KEY`
/// and `UNIQUE` constraints and answers a stub on the other three
/// (`index.rs`). The copy is the one allocation this branching costs; `table_keys` reads the
/// name of the table from it, which the copy keeps (`tests/constraints.rs`,
/// `named_constraints_become_objects`).
pub(crate) fn keys_only(def: &TableDef) -> TableDef {
    TableDef {
        name: def.name.clone(),
        columns: def.columns.clone(),
        constraints: def
            .constraints
            .iter()
            .filter(|constraint| {
                matches!(
                    constraint,
                    ConstraintDef::PrimaryKey { .. } | ConstraintDef::Unique { .. }
                )
            })
            .cloned()
            .collect(),
    }
}

/// Creates the object of each `FOREIGN KEY`, `CHECK` and `DEFAULT` of `def` and writes them
/// on `meta`.
///
/// Called by `create_table` once [`crate::index::apply_table_keys`] has returned, with the
/// `meta` the catalogue is about to store: the indexes of the table are then in the store,
/// which is what a foreign key pointing at the table being created reads (module
/// documentation, a self-referencing table). One
/// [`ConstraintMeta::ForeignKey`], [`ConstraintMeta::Check`] or [`ConstraintMeta::Default`]
/// is pushed per constraint, in declaration order, then one
/// [`ConstraintMeta::Default`] per column whose `DEFAULT` clause no
/// [`ConstraintDef::Default`] named (`tests/constraints.rs`,
/// `default_constraint_carries_the_column_ordinal`).
///
/// # Errors
///
/// - 2714 at [`CONSTRAINT_EXISTS_2714_STATE`] when the schema already holds an object of that
///   name (`tests/constraints.rs`, `duplicate_constraint_name_is_2714`);
/// - 1769 when a foreign key names a column the table has not, 1776 when the referenced
///   table has no key matching the referenced columns (`tests/constraints.rs`,
///   `foreign_key_on_unknown_column_is_1769`, `foreign_key_without_candidate_key_is_1776`);
/// - [`InternalError::Bug`] naming the number SQL Server sends for the seven shapes of the
///   module documentation, for two constraints of one name in one statement (8168), and for
///   a `DEFAULT` on a column the table does not declare, resolving a name being the business
///   of the binder and of `snapshot.rs`.
pub(crate) fn apply_table_constraints(
    store: &mut TableStore,
    def: &TableDef,
    meta: &mut TableMeta,
) -> SqlResult<()> {
    forget_dropped(store);
    // The three-part name of a constraint object carries the database it lives in, which
    // `TableMeta` holds as a `DbId` and `TableDef` as the name the statement wrote.
    let database = def.name.database.clone();
    let mut made: Vec<String> = Vec::new();
    let mut columns_with_a_named_default: Vec<ColumnId> = Vec::new();
    for constraint in &def.constraints {
        match constraint {
            ConstraintDef::PrimaryKey { .. } | ConstraintDef::Unique { .. } => {}
            ConstraintDef::ForeignKey {
                name,
                columns,
                referenced,
                referenced_columns,
                on_delete,
                on_update,
            } => {
                let written = name.clone();
                let head = columns.first().cloned();
                let single = if columns.len() == 1 { head } else { None };
                let name = name_for("FK", written.as_deref(), single.as_deref(), meta);
                let resolved = resolve_foreign_key(
                    store,
                    meta,
                    &database,
                    &name,
                    columns,
                    referenced,
                    referenced_columns,
                )?;
                let id = register(store, meta, &database, &mut made, &name)?;
                meta.constraints.push(ConstraintMeta::ForeignKey {
                    constraint: id,
                    system_named: written.is_none(),
                    columns: resolved.columns,
                    referenced_table: resolved.referenced_table,
                    referenced_columns: resolved.referenced_columns,
                    referenced_index: resolved.referenced_index,
                    on_delete: *on_delete,
                    on_update: *on_update,
                });
            }
            ConstraintDef::Check { name, expr } => {
                let written = name.clone();
                let name = name_for("CK", written.as_deref(), None, meta);
                let id = register(store, meta, &database, &mut made, &name)?;
                meta.constraints.push(ConstraintMeta::Check {
                    constraint: id,
                    system_named: written.is_none(),
                    expr: expr.clone(),
                    definition: definition_text(expr),
                });
            }
            ConstraintDef::Default { name, column, expr } => {
                let written = name.clone();
                let name = name_for("DF", written.as_deref(), Some(column.as_str()), meta);
                let target = column_named(meta, column).ok_or_else(|| {
                    InternalError::Bug(format!(
                        "Catalog::create_table: the DEFAULT {name} names the column {column}, \
                         which the table {} does not declare; resolving a name is the business \
                         of the binder",
                        meta.name
                    ))
                })?;
                let id = register(store, meta, &database, &mut made, &name)?;
                columns_with_a_named_default.push(target);
                meta.constraints.push(ConstraintMeta::Default {
                    constraint: id,
                    system_named: written.is_none(),
                    column: target,
                    expr: expr.clone(),
                });
            }
        }
    }
    // The `DEFAULT` of a column declaration, which `table.rs` keeps on
    // `ColumnMeta::default`: SQL Server gives it an object too, `DF__ab__c__3A81B327` for
    // `c int NULL DEFAULT 0`.
    let bare: Vec<(String, ColumnId, Expr)> = meta
        .columns
        .iter()
        .filter(|column| !columns_with_a_named_default.contains(&column.id))
        .filter_map(|column| {
            column
                .default
                .clone()
                .map(|expr| (column.name.clone(), column.id, expr))
        })
        .collect();
    for (column_name, column, expr) in bare {
        let name = name_for("DF", None, Some(&column_name), meta);
        let id = register(store, meta, &database, &mut made, &name)?;
        meta.constraints.push(ConstraintMeta::Default {
            constraint: id,
            system_named: true,
            column,
            expr,
        });
    }
    Ok(())
}

/// Forgets the constraint objects whose table the store no longer holds.
///
/// The mirror of [`table::refresh`] and of [`crate::index::refresh`], which the caller runs
/// first: a table gone from `storage` — a `CREATE TABLE` undone by a `ROLLBACK`, a
/// `DROP TABLE` carried out by a `COMMIT` — leaves the store of `table.rs`, and its
/// constraints leave with it, which is what frees their names (`tests/constraints.rs`,
/// `drop_referencing_table_first_succeeds`).
///
/// A table marked by a deferred `DROP` keeps its constraints until the `COMMIT`, as it keeps
/// its indexes (`index.rs`): [`TableStore::get`] still answers for it.
pub(crate) fn forget_dropped(store: &mut TableStore) {
    let gone: Vec<ObjectId> = store
        .constraints
        .entries
        .values()
        .filter(|object| object.parent.is_none_or(|table| store.get(table).is_none()))
        .map(|object| object.id)
        .collect();
    for id in gone {
        store.constraints.entries.remove(&id);
    }
}

/// The name of a constraint: the one the statement wrote, or the one the catalogue makes up.
///
/// `column` is the column the constraint belongs to, which the generated name carries when
/// the constraint has exactly one; the shape is the one `views/sys_constraints.rs` builds
/// ([`generated_constraint_name`]). The position that separates two generated
/// names of one table is the number of constraints already on the table, so a second
/// anonymous `CHECK` takes another name (`tests/constraints.rs`,
/// `anonymous_constraint_name_has_the_generated_shape`).
fn name_for(kind: &str, written: Option<&str>, column: Option<&str>, meta: &TableMeta) -> String {
    match written {
        Some(name) => name.to_owned(),
        None => generated_constraint_name(kind, meta, column, meta.constraints.len()),
    }
}

/// Stores the object of a constraint called `name` on the table `meta` and answers its
/// identifier.
///
/// `made` holds the names this statement has already taken, which is what tells 8168 from
/// 2714 (module documentation).
///
/// # Errors
///
/// - [`InternalError::Bug`] naming 8168 when `made` already holds the name;
/// - 2714 at [`CONSTRAINT_EXISTS_2714_STATE`] when a constraint or a table of the same
///   database and schema carries it, the table being created included. The name printed is
///   the one written, without its schema (module documentation);
/// - [`InternalError::Bug`] when the identifier counter is exhausted
///   ([`TableStore::take_object_id`]).
fn register(
    store: &mut TableStore,
    meta: &TableMeta,
    database: &str,
    made: &mut Vec<String>,
    name: &str,
) -> SqlResult<ObjectId> {
    if made.iter().any(|taken| taken.eq_ignore_ascii_case(name)) {
        return Err(InternalError::Bug(format!(
            "Catalog::create_table: table {} carries two constraints named {name}; SQL Server \
             answers 8168, which vauban-errors does not catalogue",
            meta.name
        ))
        .into());
    }
    // The table being created is not in the store yet, and SQL Server counts it: a `CHECK`,
    // a `DEFAULT` and a `FOREIGN KEY` named like their own table each answer 2714 state 5
    // (module documentation).
    let taken_by_the_table_being_created = meta.name.eq_ignore_ascii_case(name);
    let taken_by_a_table = store.live().any(|table| {
        table.database == meta.database
            && table.schema.eq_ignore_ascii_case(&meta.schema)
            && table.name.eq_ignore_ascii_case(name)
    });
    if taken_by_the_table_being_created
        || taken_by_a_table
        || store
            .constraints
            .named(meta.database, &meta.schema, name)
            .is_some()
    {
        let mut err = SqlError::object_already_exists(name);
        err.state = CONSTRAINT_EXISTS_2714_STATE;
        return Err(err);
    }
    let id = store.take_object_id()?;
    store.constraints.entries.insert(
        id,
        ObjectMeta {
            id,
            kind: ObjectKind::Constraint,
            name: QualifiedName {
                database: database.to_owned(),
                schema: meta.schema.clone(),
                name: name.to_owned(),
            },
            database: meta.database,
            parent: Some(meta.id),
            definition: None,
        },
    );
    made.push(name.to_owned());
    Ok(id)
}

/// What [`resolve_foreign_key`] read from a [`ConstraintDef::ForeignKey`].
struct ResolvedForeignKey {
    /// Columns of the constrained table, in the order they were declared.
    columns: Vec<ColumnId>,
    /// Table pointed at.
    referenced_table: ObjectId,
    /// Columns pointed at, in the order of `columns`.
    referenced_columns: Vec<ColumnId>,
    /// Index of the referenced table whose key those columns are.
    referenced_index: IndexId,
}

/// Reads a `FOREIGN KEY` into identifiers: its columns, the table it points at, the columns
/// it points at and the index that carries them.
///
/// The referenced table is the one being created when the name is its own, the store not
/// holding it yet (`tests/constraints.rs`, `foreign_key_binds_the_referenced_index`).
///
/// # Errors
///
/// 1769 and 1776, and the [`InternalError::Bug`] of the seven numbers of the module
/// documentation.
fn resolve_foreign_key(
    store: &TableStore,
    meta: &TableMeta,
    database: &str,
    constraint: &str,
    columns: &[String],
    referenced: &QualifiedName,
    referenced_columns: &[String],
) -> SqlResult<ResolvedForeignKey> {
    if !referenced.database.eq_ignore_ascii_case(database) {
        return Err(InternalError::Bug(format!(
            "Catalog::create_table: the foreign key {constraint} points at {}.{}.{}, in \
             another database than {}; SQL Server answers 1763, which vauban-errors does not \
             catalogue",
            referenced.database, referenced.schema, referenced.name, database
        ))
        .into());
    }
    let target = referenced_table(store, meta, referenced).ok_or_else(|| {
        InternalError::Bug(format!(
            "Catalog::create_table: the foreign key {constraint} points at the unknown table \
             {}.{}; SQL Server answers 1767, which vauban-errors does not catalogue",
            referenced.schema, referenced.name
        ))
    })?;
    let mut child_columns = Vec::with_capacity(columns.len());
    for column in columns {
        let id = column_named(meta, column).ok_or_else(|| {
            SqlError::foreign_key_references_invalid_column(constraint, column, &meta.name)
        })?;
        child_columns.push(id);
    }
    // `REFERENCES t` without a column list points at the primary key of `t`, `key_index_id`
    // 1 (module documentation). Deprived of that primary key SQL Server leaves 1776 for
    // 1773, on two shapes: a referenced table carrying a `CREATE UNIQUE INDEX` and one
    // carrying neither (module documentation).
    let parent_columns: Vec<ColumnId> = if referenced_columns.is_empty() {
        primary_key_columns(store, target).ok_or_else(|| {
            InternalError::Bug(format!(
                "Catalog::create_table: the foreign key {constraint} points at the table {}.{} \
                 without a column list, and that table carries no PRIMARY KEY; SQL Server \
                 answers 1773, which vauban-errors does not catalogue",
                target.schema, target.name
            ))
        })?
    } else {
        let mut resolved = Vec::with_capacity(referenced_columns.len());
        for column in referenced_columns {
            let id = column_named(target, column).ok_or_else(|| {
                InternalError::Bug(format!(
                    "Catalog::create_table: the foreign key {constraint} points at the column \
                     {column}, which the table {}.{} does not declare; SQL Server answers \
                     1770, which vauban-errors does not catalogue",
                    target.schema, target.name
                ))
            })?;
            resolved.push(id);
        }
        resolved
    };
    if parent_columns.len() != child_columns.len() {
        return Err(InternalError::Bug(format!(
            "Catalog::create_table: the foreign key {constraint} of the table {} points at {} \
             columns with {} of its own; SQL Server answers 8139, which vauban-errors does not \
             catalogue",
            meta.name,
            parent_columns.len(),
            child_columns.len()
        ))
        .into());
    }
    for (child, parent) in child_columns.iter().zip(parent_columns.iter()) {
        let here = type_of(meta, *child);
        let there = type_of(target, *parent);
        if let (Some(here), Some(there)) = (here, there)
            && !compatible_types(here, there)
        {
            return Err(InternalError::Bug(format!(
                "Catalog::create_table: the column {}.{} and the column {}.{} of the foreign \
                 key {constraint} do not carry the same type; SQL Server answers 1778 when the \
                 types differ and 1753 when the length or the scale does, neither of which \
                 vauban-errors catalogues",
                target.name, parent, meta.name, child
            ))
            .into());
        }
    }
    let referenced_index = candidate_key(store, target, &parent_columns).ok_or_else(|| {
        SqlError::no_matching_key_in_referenced_table(
            &format!("{}.{}", target.schema, target.name),
            constraint,
        )
    })?;
    Ok(ResolvedForeignKey {
        columns: child_columns,
        referenced_table: target.id,
        referenced_columns: parent_columns,
        referenced_index,
    })
}

/// The table a `REFERENCES` clause names: the one being created when the clause names it,
/// one of the live tables of the store otherwise, `None` when the name matches neither.
fn referenced_table<'a>(
    store: &'a TableStore,
    meta: &'a TableMeta,
    referenced: &QualifiedName,
) -> Option<&'a TableMeta> {
    if meta.schema.eq_ignore_ascii_case(&referenced.schema)
        && meta.name.eq_ignore_ascii_case(&referenced.name)
    {
        return Some(meta);
    }
    store.live().find(|table| {
        table.database == meta.database
            && table.schema.eq_ignore_ascii_case(&referenced.schema)
            && table.name.eq_ignore_ascii_case(&referenced.name)
    })
}

/// The columns of the `PRIMARY KEY` of `table`, `None` when the table carries no
/// `PRIMARY KEY`.
///
/// Read by a `REFERENCES` clause without a column list, where the `None` becomes the
/// [`InternalError::Bug`] of 1773 (`tests/constraints.rs`,
/// `an_implicit_reference_without_a_primary_key_names_1773`).
fn primary_key_columns(store: &TableStore, table: &TableMeta) -> Option<Vec<ColumnId>> {
    let index = table
        .constraints
        .iter()
        .find_map(|constraint| match constraint {
            ConstraintMeta::PrimaryKey(id) => Some(*id),
            _ => None,
        })?;
    let key = store.indexes.get(index)?;
    key.columns
        .iter()
        .map(|column| column_at(table, column.column))
        .collect()
}

/// The index of `table` whose key is `columns`, in that order, and which holds one row per
/// key, `None` when no index of the table does.
///
/// This is what a `FOREIGN KEY` points at: the key `(l, r)` read as `(r, l)` or as `(r)`
/// is refused with 1776, and a unique index built by `CREATE UNIQUE INDEX` is taken as
/// readily as a `PRIMARY KEY` (module documentation).
fn candidate_key(store: &TableStore, table: &TableMeta, columns: &[ColumnId]) -> Option<IndexId> {
    store
        .indexes
        .of_table(table.id)
        .into_iter()
        .find(|index: &&IndexMeta| {
            index.unique
                && index.columns.len() == columns.len()
                && index
                    .columns
                    .iter()
                    .zip(columns.iter())
                    .all(|(key, column)| column_at(table, key.column) == Some(*column))
        })
        .map(|index| index.id)
}

/// The identifier of the column of `table` called `name`, `None` when the table declares no
/// such column.
///
/// Compared with `eq_ignore_ascii_case`, as `index.rs` compares the columns of a key.
fn column_named(table: &TableMeta, name: &str) -> Option<ColumnId> {
    table
        .columns
        .iter()
        .find(|column: &&ColumnMeta| column.name.eq_ignore_ascii_case(name))
        .map(|column| column.id)
}

/// The identifier of the column of `table` at the row position `ordinal`, `None` when the
/// table has no column there.
fn column_at(table: &TableMeta, ordinal: u16) -> Option<ColumnId> {
    table
        .columns
        .iter()
        .find(|column: &&ColumnMeta| column.ordinal == ordinal)
        .map(|column| column.id)
}

/// The type of the column of `table` of identifier `id`, `None` when the table holds no such
/// column.
fn type_of(table: &TableMeta, id: ColumnId) -> Option<&TypeInfo> {
    table
        .columns
        .iter()
        .find(|column: &&ColumnMeta| column.id == id)
        .map(|column| &column.ty)
}

/// Whether the two columns of one pair of a `FOREIGN KEY` carry the same type.
///
/// The [`SqlType`](vauban_types::SqlType) is compared with its parameters, length and scale
/// included, which is what separates two numbers: `int` against `varchar(10)` is 1778 and
/// `varchar(20)` against `varchar(10)` is 1753 (module documentation). `nullable` is left
/// out, a `NOT NULL` column pointing at a `PRIMARY KEY` being accepted; so is the
/// collation.
fn compatible_types(here: &TypeInfo, there: &TypeInfo) -> bool {
    here.ty == there.ty
}

/// The text `sys.check_constraints.definition` publishes for `expr`: what `Display` writes,
/// between parentheses.
///
/// The same rule as `views/sys_constraints.rs`, whose `definition_text` the rows of the four
/// views go through; the form SQL Server stores is in the module documentation.
fn definition_text(expr: &Expr) -> String {
    format!("({expr})")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A constraint object of the table `parent`, named `name`.
    fn object(id: i32, parent: i32, name: &str) -> ObjectMeta {
        ObjectMeta {
            id: ObjectId(id),
            kind: ObjectKind::Constraint,
            name: QualifiedName {
                database: "master".to_owned(),
                schema: "dbo".to_owned(),
                name: name.to_owned(),
            },
            database: DbId(0),
            parent: Some(ObjectId(parent)),
            definition: None,
        }
    }

    #[test]
    fn the_store_reads_back_what_it_registered() {
        let mut store = ConstraintStore::default();
        store.entries.insert(ObjectId(7), object(7, 1, "ck_a"));
        store.entries.insert(ObjectId(8), object(8, 1, "df_b"));
        store.entries.insert(ObjectId(9), object(9, 2, "ck_c"));
        let names: Vec<&str> = store
            .of_table(ObjectId(1))
            .into_iter()
            .map(|o| o.name.name.as_str())
            .collect();
        assert_eq!(names, vec!["ck_a", "df_b"]);
        assert!(store.of_table(ObjectId(3)).is_empty());
    }

    #[test]
    fn a_name_is_resolved_without_regard_to_ascii_case() {
        let mut store = ConstraintStore::default();
        store.entries.insert(ObjectId(7), object(7, 1, "ck_a"));
        assert!(store.named(DbId(0), "DBO", "CK_A").is_some());
        assert!(store.named(DbId(0), "dbo", "ck_b").is_none());
        assert!(store.named(DbId(1), "dbo", "ck_a").is_none());
    }

    #[test]
    fn a_type_pair_is_read_without_its_nullability() {
        use vauban_types::{Len, SqlType};
        let int_null = TypeInfo::new(SqlType::Int, true);
        let int_not_null = TypeInfo::new(SqlType::Int, false);
        let ten = TypeInfo::new(SqlType::VarChar(Len::Fixed(10)), true);
        let twenty = TypeInfo::new(SqlType::VarChar(Len::Fixed(20)), true);
        assert!(compatible_types(&int_null, &int_not_null));
        assert!(!compatible_types(&int_null, &ten), "1778 in SQL Server");
        assert!(!compatible_types(&twenty, &ten), "1753 in SQL Server");
    }
}
