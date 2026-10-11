//! `FOREIGN KEY` and `CHECK` at write time: the 547 of a row that breaks one.
//!
//! # Resolved once, checked per row
//!
//! [`WriteChecks::resolve`] reads what a statement checks out of the catalogue snapshot the
//! statement already holds and out of the `CHECK` predicates its plan carries, bound once
//! by the binder: the name of each constraint, the ordinals of the key columns, the index a
//! parent key is sought through, and the tables whose foreign keys point at the written
//! table. Checking a row is then evaluating those predicates and seeking those keys, with no
//! catalogue snapshot and no text to read again (`tests/fk_check.rs`,
//! `an_insert_select_builds_one_catalogue_snapshot`).
//!
//! The snapshot is the one of the transaction that writes: a `FOREIGN KEY` another
//! transaction has added and not committed is not checked on either of its tables
//! (`fk_check/tests/mod.rs`, `a_foreign_key_another_transaction_has_not_committed_is_not_checked`).
//!
//! # A foreign key of a table towards itself: checked at the end of the statement
//!
//! A `FOREIGN KEY` whose parent is the written table itself is checked once the statement
//! has written all its rows, not row by row: a row may name itself as its parent, rows of one
//! `INSERT` may name each other, and a `DELETE` of a whole hierarchy leaves no orphan
//! behind it (`fk_check/tests/mod.rs`, `a_row_may_reference_itself_or_a_row_of_the_same_statement`,
//! `deleting_a_whole_hierarchy_leaves_no_orphan`). [`WriteChecks::check_row`] and
//! [`WriteChecks::check_no_referencing_rows`] set those keys aside, and
//! [`WriteChecks::finish`] reads them against the rows the statement leaves: first the keys
//! the statement removed that a remaining row still references (`SAME TABLE REFERENCE`),
//! then the keys the written rows reference that no row holds (`FOREIGN KEY SAME TABLE`).
//!
//! # The rows a table holds when a foreign key is added
//!
//! [`refuse_rows_without_parent`] reads them for `ALTER TABLE ADD FOREIGN KEY`: a row whose
//! key no parent row holds is the 547 of `ALTER TABLE`, and the constraint is not added
//! (`tests/ddl_alter.rs`, `add_foreign_key_reads_the_existing_rows`).
//!
//! A `CHECK` the plan does not carry, because its stored text did not bind, is not skipped:
//! a row written under it is refused with the internal error, which names the constraint
//! (`tests/fk_check.rs`, `a_check_missing_from_the_plan_refuses_the_row`).

use std::cell::RefCell;

use vauban_binder::{BoundExpr, BoundObjectExpression};
use vauban_catalog::{
    CatalogSnapshot, ColumnId, ConstraintMeta, ObjectId, QualifiedName, TableMeta,
};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{Direction, IndexId, KeyRange, Snapshot, Storage, TableId};
use vauban_types::Value;

use crate::context::ExecContext;
use crate::errors::at;
use crate::expr::eval_expr;
use crate::row::Row;

/// What one statement checks on each row it writes into one table.
pub(crate) struct WriteChecks {
    /// `INSERT`, `UPDATE` or `DELETE`, for the 547 text.
    statement: &'static str,
    /// The database of the table, for the 547 text.
    database: String,
    /// `schema.table`, for the 547 of a `CHECK`.
    object: String,
    /// The `CHECK` and outgoing `FOREIGN KEY` constraints, in the order the table declares
    /// them.
    rules: Vec<Rule>,
    /// The foreign keys of other tables that point at this one.
    incoming: Vec<Incoming>,
    /// The keys of the foreign keys of the table towards itself, set aside until
    /// [`WriteChecks::finish`].
    deferred: RefCell<Deferred>,
}

/// The keys a statement sets aside for the foreign keys of its table towards itself.
#[derive(Default)]
struct Deferred {
    /// Keys the written rows reference: the position of the rule in `rules`, and the key.
    referencing: Vec<(usize, Vec<Value>)>,
    /// Keys the statement removed: the position of the foreign key in `incoming`, and the
    /// key.
    removed: Vec<(usize, Vec<Value>)>,
}

/// One `CHECK` or outgoing `FOREIGN KEY` of the written table.
enum Rule {
    /// A `CHECK`; `predicate` is `None` when the plan carries no bound predicate for it.
    Check {
        name: String,
        predicate: Option<BoundExpr>,
    },
    /// A `FOREIGN KEY` of the written table.
    ForeignKey {
        name: String,
        /// The ordinals of the key columns in a row of the written table.
        ordinals: Vec<usize>,
        /// The index of the parent key the key is sought in.
        index: IndexId,
        /// `schema.table` of the parent.
        parent: String,
        /// The first referenced column, which the 547 text names.
        column: Option<String>,
        /// Whether the parent is the written table itself.
        same_table: bool,
    },
}

/// A foreign key of a child table that points at the written table.
struct Incoming {
    name: String,
    /// The storage table of the child.
    child: TableId,
    /// The ordinals of the key columns in a row of the child.
    child_ordinals: Vec<usize>,
    /// An index of the child whose leading columns are the key, when there is one.
    child_index: Option<IndexId>,
    /// The ordinals of the referenced columns in a row of the written table.
    referenced_ordinals: Vec<usize>,
    /// `schema.table` of the child.
    object: String,
    /// The first key column of the child, which the 547 text names.
    column: Option<String>,
    /// Whether the child is the written table itself.
    same_table: bool,
    /// The index of the written table the foreign key reads its key in.
    referenced_index: IndexId,
}

impl WriteChecks {
    /// Resolves what a statement writing into `table` checks.
    ///
    /// `checks` are the bound `CHECK` predicates of the plan; `incoming` asks for the
    /// foreign keys that point at the table, which an `UPDATE` and a `DELETE` need and an
    /// `INSERT` does not.
    ///
    /// Names, foreign keys and indexes are read in `snap` alone, which the caller built for
    /// the transaction of the statement.
    ///
    /// # Errors
    ///
    /// The internal error when the snapshot no longer holds a table a constraint names.
    pub(crate) fn resolve(
        table: &TableMeta,
        snap: &CatalogSnapshot,
        checks: &[BoundObjectExpression],
        statement: &'static str,
        incoming: bool,
        _ctx: &ExecContext<'_>,
    ) -> SqlResult<Self> {
        let database = snap
            .database_by_id(table.database)
            .map_or_else(|| "?".to_owned(), |db| db.name.clone());
        let name_of = |id: &ObjectId| constraint_name(snap, *id);
        let mut rules = Vec::new();
        for constraint in &table.constraints {
            match constraint {
                ConstraintMeta::Check { constraint, .. } => {
                    let name = name_of(constraint);
                    let predicate = checks
                        .iter()
                        .find(|check| check.constraint.as_deref() == Some(name.as_str()))
                        .map(|check| check.expr.clone());
                    rules.push(Rule::Check { name, predicate });
                }
                ConstraintMeta::ForeignKey {
                    constraint,
                    columns,
                    referenced_table,
                    referenced_columns,
                    referenced_index,
                    ..
                } => {
                    let Some(parent) = snap.table(*referenced_table) else {
                        return Err(bug(
                            "FOREIGN KEY: referenced table is missing from the snapshot",
                        ));
                    };
                    rules.push(Rule::ForeignKey {
                        name: name_of(constraint),
                        ordinals: ordinals(table, columns)?,
                        index: *referenced_index,
                        parent: format!("{}.{}", parent.schema, parent.name),
                        column: column_name(parent, referenced_columns.first()),
                        same_table: *referenced_table == table.id,
                    });
                }
                ConstraintMeta::PrimaryKey(_)
                | ConstraintMeta::Unique(_)
                | ConstraintMeta::Default { .. } => {}
            }
        }
        let incoming = if incoming {
            resolve_incoming(table, snap)?
        } else {
            Vec::new()
        };
        Ok(Self {
            statement,
            database,
            object: format!("{}.{}", table.schema, table.name),
            rules,
            incoming,
            deferred: RefCell::new(Deferred::default()),
        })
    }

    /// Runs the `CHECK` and outgoing `FOREIGN KEY` constraints against `row`, a row of the
    /// written table.
    ///
    /// # Errors
    ///
    /// 547 for the first constraint `row` breaks; the internal error for a `CHECK` the plan
    /// carries no predicate for; what evaluating a predicate or seeking a key raises.
    ///
    /// The key of a foreign key of the table towards itself is set aside for
    /// [`WriteChecks::finish`] (module documentation).
    pub(crate) fn check_row(&self, row: &Row, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        for (position, rule) in self.rules.iter().enumerate() {
            match rule {
                Rule::Check { name, predicate } => {
                    let Some(predicate) = predicate else {
                        return Err(bug(&format!(
                            "{}: the CHECK constraint '{name}' of table '{}' has no bound \
                             predicate, so the row is not written",
                            self.statement, self.object
                        )));
                    };
                    let value = eval_expr(predicate, Some(row), ctx)?;
                    if matches!(value, Value::Bit(false)) {
                        return Err(at(
                            SqlError::fk_violation(
                                self.statement,
                                "CHECK",
                                name,
                                &self.database,
                                &self.object,
                                None,
                            ),
                            predicate.line,
                        ));
                    }
                }
                Rule::ForeignKey {
                    name,
                    ordinals,
                    index,
                    parent,
                    column,
                    same_table,
                } => {
                    let key = key_values(row, ordinals)?;
                    if key.iter().any(|value| matches!(value, Value::Null)) {
                        continue;
                    }
                    if *same_table {
                        self.deferred.borrow_mut().referencing.push((position, key));
                        continue;
                    }
                    if !key_exists(ctx, *index, &key)? {
                        return Err(at(
                            SqlError::fk_violation(
                                self.statement,
                                "FOREIGN KEY",
                                name,
                                &self.database,
                                parent,
                                column.as_deref(),
                            ),
                            0,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Refuses when a row of another table still references `row` through a foreign key.
    ///
    /// # Errors
    ///
    /// 547 for the first foreign key that still finds a child row; what reading the child
    /// raises.
    ///
    /// The key of a foreign key of the table towards itself is set aside for
    /// [`WriteChecks::finish`] (module documentation).
    pub(crate) fn check_no_referencing_rows(
        &self,
        row: &Row,
        ctx: &mut ExecContext<'_>,
    ) -> SqlResult<()> {
        for (position, reference) in self.incoming.iter().enumerate() {
            let key = key_values(row, &reference.referenced_ordinals)?;
            if key.iter().any(|value| matches!(value, Value::Null)) {
                continue;
            }
            if reference.same_table {
                self.deferred.borrow_mut().removed.push((position, key));
                continue;
            }
            if child_row_exists(ctx, reference, &key)? {
                return Err(at(
                    SqlError::fk_violation(
                        self.statement,
                        "REFERENCE",
                        &reference.name,
                        &self.database,
                        &reference.object,
                        reference.column.as_deref(),
                    ),
                    0,
                ));
            }
        }
        Ok(())
    }

    /// Checks the keys [`WriteChecks::check_row`] and
    /// [`WriteChecks::check_no_referencing_rows`] set aside, after the last row the
    /// statement writes.
    ///
    /// A removed key that a row of the table still holds is no loss; one that a remaining
    /// row still references is the 547 of a `SAME TABLE REFERENCE`. A referenced key that no
    /// row holds is then the 547 of a `FOREIGN KEY SAME TABLE`.
    ///
    /// # Errors
    ///
    /// 547 for the first key that breaks; what seeking or reading the table raises.
    pub(crate) fn finish(&self, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        let deferred = std::mem::take(&mut *self.deferred.borrow_mut());
        for (position, key) in &deferred.removed {
            let Some(reference) = self.incoming.get(*position) else {
                return Err(bug("FOREIGN KEY: a set-aside key names no foreign key"));
            };
            if key_exists(ctx, reference.referenced_index, key)? {
                continue;
            }
            if child_row_exists(ctx, reference, key)? {
                return Err(at(
                    SqlError::fk_violation(
                        self.statement,
                        "SAME TABLE REFERENCE",
                        &reference.name,
                        &self.database,
                        &reference.object,
                        single_column(&reference.child_ordinals, &reference.column),
                    ),
                    0,
                ));
            }
        }
        for (position, key) in &deferred.referencing {
            let Some(Rule::ForeignKey {
                name,
                ordinals,
                index,
                parent,
                column,
                ..
            }) = self.rules.get(*position)
            else {
                return Err(bug("FOREIGN KEY: a set-aside key names no foreign key"));
            };
            if !key_exists(ctx, *index, key)? {
                return Err(at(
                    SqlError::fk_violation(
                        self.statement,
                        "FOREIGN KEY SAME TABLE",
                        name,
                        &self.database,
                        parent,
                        single_column(ordinals, column),
                    ),
                    0,
                ));
            }
        }
        Ok(())
    }

    /// `true` when an `UPDATE` changes a column an incoming foreign key references.
    pub(crate) fn referenced_columns_changed(&self, old_row: &Row, new_row: &Row) -> bool {
        self.incoming.iter().any(|reference| {
            reference
                .referenced_ordinals
                .iter()
                .any(|ordinal| old_row.get(*ordinal) != new_row.get(*ordinal))
        })
    }
}

/// The foreign keys of the tables of `snap` that point at `table`, resolved, by increasing
/// child [`ObjectId`] then constraint [`ObjectId`].
fn resolve_incoming(table: &TableMeta, snap: &CatalogSnapshot) -> SqlResult<Vec<Incoming>> {
    let mut found: Vec<((ObjectId, ObjectId), Incoming)> = Vec::new();
    for child in snap.tables() {
        for constraint in &child.constraints {
            let ConstraintMeta::ForeignKey {
                constraint,
                columns,
                referenced_table,
                referenced_columns,
                referenced_index,
                ..
            } = constraint
            else {
                continue;
            };
            if *referenced_table != table.id {
                continue;
            }
            let child_ordinals = ordinals(child, columns)?;
            let incoming = Incoming {
                name: constraint_name(snap, *constraint),
                child: child.storage_id,
                child_index: index_for_key(snap, child, &child_ordinals)?,
                child_ordinals,
                referenced_ordinals: ordinals(table, referenced_columns)?,
                object: format!("{}.{}", child.schema, child.name),
                column: column_name(child, columns.first()),
                same_table: child.id == table.id,
                referenced_index: *referenced_index,
            };
            found.push(((child.id, *constraint), incoming));
        }
    }
    found.sort_by_key(|(key, _)| *key);
    Ok(found.into_iter().map(|(_, incoming)| incoming).collect())
}

/// An index of `child` whose leading columns are `ordinals`, in that order.
fn index_for_key(
    snap: &CatalogSnapshot,
    child: &TableMeta,
    ordinals: &[usize],
) -> SqlResult<Option<IndexId>> {
    let ordinals: Vec<u16> = ordinals
        .iter()
        .map(|ordinal| {
            u16::try_from(*ordinal)
                .map_err(|_| bug("FOREIGN KEY: column ordinal does not fit an index key"))
        })
        .collect::<SqlResult<_>>()?;
    Ok(snap
        .indexes_of(child.id)
        .iter()
        .find(|index| {
            index.columns.len() >= ordinals.len()
                && index
                    .columns
                    .iter()
                    .zip(ordinals.iter())
                    .all(|(key, ordinal)| key.column == *ordinal)
        })
        .map(|index| index.id))
}

fn key_exists(ctx: &mut ExecContext<'_>, index: IndexId, key: &[Value]) -> SqlResult<bool> {
    seek_finds(ctx.storage()?, ctx.snapshot()?, index, key)
}

/// Whether `index` holds `key` for the reader of `snap`.
fn seek_finds(
    storage: &dyn Storage,
    snap: &Snapshot,
    index: IndexId,
    key: &[Value],
) -> SqlResult<bool> {
    let mut iter = storage.seek(
        snap,
        index,
        &KeyRange::Point(key.to_vec()),
        Direction::Forward,
    )?;
    Ok(iter.next().transpose()?.is_some())
}

/// A `FOREIGN KEY` that `ALTER TABLE ADD` is about to add, resolved in the catalogue
/// snapshot of the statement.
pub(crate) struct AddedForeignKey<'s> {
    /// The table the constraint is added to.
    table: &'s TableMeta,
    /// The table it points at, `table` itself for a foreign key of a table towards itself.
    parent: &'s TableMeta,
    /// The ordinals of the key columns in a row of `table`.
    ordinals: Vec<usize>,
    /// The index of `parent` the key is sought in.
    index: IndexId,
    /// The first referenced column, which the 547 text names.
    column: Option<String>,
}

impl<'s> AddedForeignKey<'s> {
    /// Resolves the `FOREIGN KEY (columns) REFERENCES referenced (referenced_columns)` that
    /// `ALTER TABLE` adds to `table`, as the catalogue will: the columns by name, the
    /// primary key of the parent when `referenced_columns` is empty, and the unique index of
    /// the parent whose key is those columns, in that order.
    ///
    /// `None` when one of these does not resolve: the catalogue then refuses the constraint
    /// with its own error, and there is no row to read.
    pub(crate) fn resolve(
        table: &'s TableMeta,
        snap: &'s CatalogSnapshot,
        columns: &[String],
        referenced: &QualifiedName,
        referenced_columns: &[String],
    ) -> Option<Self> {
        let database = snap.database_by_id(table.database)?;
        let parent = if referenced.database.eq_ignore_ascii_case(&database.name)
            && referenced.schema.eq_ignore_ascii_case(&table.schema)
            && referenced.name.eq_ignore_ascii_case(&table.name)
        {
            table
        } else {
            let object = snap.resolve_object(
                &referenced.database,
                Some(&referenced.schema),
                &referenced.name,
                &referenced.schema,
            )?;
            snap.table(object.id)?
        };
        let ordinals = ordinals_named(table, columns)?;
        let indexes = snap.indexes_of(parent.id);
        let parent_ordinals: Vec<usize> = if referenced_columns.is_empty() {
            let key = indexes.iter().find(|index| index.primary_key)?;
            key.columns
                .iter()
                .map(|key| usize::from(key.column))
                .collect()
        } else {
            ordinals_named(parent, referenced_columns)?
        };
        if parent_ordinals.len() != ordinals.len() {
            return None;
        }
        let index = indexes.iter().find(|index| {
            index.unique
                && index.columns.len() == parent_ordinals.len()
                && index
                    .columns
                    .iter()
                    .zip(parent_ordinals.iter())
                    .all(|(key, ordinal)| usize::from(key.column) == *ordinal)
        })?;
        let column = parent_ordinals
            .first()
            .and_then(|ordinal| parent.columns.get(*ordinal))
            .map(|column| column.name.clone());
        Some(Self {
            table,
            parent,
            ordinals,
            index: index.id,
            column,
        })
    }

    /// The storage table of the parent.
    pub(crate) fn parent_storage(&self) -> TableId {
        self.parent.storage_id
    }

    /// Refuses the constraint when a row of the table, read under `rows`, holds a key no
    /// parent row holds (547); a key with a `NULL` in it references nothing and passes.
    ///
    /// `name` is the name of the constraint, `None` when the statement does not write one.
    ///
    /// # Errors
    ///
    /// 547 for the first such row; what reading the table or seeking the parent raises.
    pub(crate) fn refuse_rows_without_parent(
        &self,
        storage: &dyn Storage,
        rows: &Snapshot,
        database: &str,
        name: Option<&str>,
    ) -> SqlResult<()> {
        for item in storage.scan(rows, self.table.storage_id)? {
            let (_, row) = item?;
            let key = key_values(&row.0, &self.ordinals)?;
            if key.iter().any(|value| matches!(value, Value::Null)) {
                continue;
            }
            if !seek_finds(storage, rows, self.index, &key)? {
                let kind = if self.parent.id == self.table.id {
                    "FOREIGN KEY SAME TABLE"
                } else {
                    "FOREIGN KEY"
                };
                return Err(SqlError::fk_violation(
                    "ALTER TABLE",
                    kind,
                    name.unwrap_or("FOREIGN KEY"),
                    database,
                    &format!("{}.{}", self.parent.schema, self.parent.name),
                    single_column(&self.ordinals, &self.column),
                ));
            }
        }
        Ok(())
    }
}

/// The column a 547 names for a key of `ordinals`: `column` for a key of one column, none
/// for a key of several (`fk_check/tests/mod.rs`, `a_composite_key_names_no_column`).
fn single_column<'c>(ordinals: &[usize], column: &'c Option<String>) -> Option<&'c str> {
    if ordinals.len() == 1 {
        column.as_deref()
    } else {
        None
    }
}

/// The ordinals of the columns of `table` named `names`, `None` when one is not a column of
/// the table.
fn ordinals_named(table: &TableMeta, names: &[String]) -> Option<Vec<usize>> {
    names
        .iter()
        .map(|name| {
            table
                .columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(name))
                .map(|column| usize::from(column.ordinal))
        })
        .collect()
}

fn child_row_exists(
    ctx: &mut ExecContext<'_>,
    reference: &Incoming,
    key: &[Value],
) -> SqlResult<bool> {
    if let Some(index) = reference.child_index {
        return key_exists(ctx, index, key);
    }
    let storage = ctx.storage()?;
    let snap = ctx.snapshot()?;
    for item in storage.scan(snap, reference.child)? {
        let (_, row) = item?;
        if key_values(&row.0, &reference.child_ordinals)? == key {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The name of the constraint `id` in `snap`, `?` when the snapshot names no such
/// constraint.
fn constraint_name(snap: &CatalogSnapshot, id: ObjectId) -> String {
    snap.constraint_name(id)
        .map_or_else(|| "?".to_owned(), |name| name.name)
}

fn key_values(row: &[Value], ordinals: &[usize]) -> SqlResult<Vec<Value>> {
    ordinals
        .iter()
        .map(|ordinal| {
            row.get(*ordinal)
                .cloned()
                .ok_or_else(|| bug("constraint column is past the end of the row"))
        })
        .collect()
}

fn ordinals(table: &TableMeta, columns: &[ColumnId]) -> SqlResult<Vec<usize>> {
    columns
        .iter()
        .map(|column| {
            table
                .columns
                .iter()
                .find(|col| col.id == *column)
                .map(|col| usize::from(col.ordinal))
                .ok_or_else(|| bug("constraint column is missing from the table metadata"))
        })
        .collect()
}

fn column_name(table: &TableMeta, column: Option<&ColumnId>) -> Option<String> {
    column.and_then(|column| {
        table
            .columns
            .iter()
            .find(|col| col.id == *column)
            .map(|col| col.name.clone())
    })
}

fn bug(what: &str) -> SqlError {
    SqlError::from(InternalError::Bug(what.to_owned()))
}

#[cfg(test)]
mod tests;
