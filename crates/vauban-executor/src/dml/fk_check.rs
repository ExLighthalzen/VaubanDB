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
//! A `CHECK` the plan does not carry, because its stored text did not bind, is not skipped:
//! a row written under it is refused with the internal error, which names the constraint
//! (`tests/fk_check.rs`, `a_check_missing_from_the_plan_refuses_the_row`).

use vauban_binder::{BoundExpr, BoundObjectExpression};
use vauban_catalog::{CatalogSnapshot, ColumnId, ConstraintMeta, ObjectId, TableMeta};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_storage::{Direction, IndexId, KeyRange, TableId};
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
        })
    }

    /// Runs the `CHECK` and outgoing `FOREIGN KEY` constraints against `row`, a row of the
    /// written table.
    ///
    /// # Errors
    ///
    /// 547 for the first constraint `row` breaks; the internal error for a `CHECK` the plan
    /// carries no predicate for; what evaluating a predicate or seeking a key raises.
    pub(crate) fn check_row(&self, row: &Row, ctx: &mut ExecContext<'_>) -> SqlResult<()> {
        for rule in &self.rules {
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
                } => {
                    let key = key_values(row, ordinals)?;
                    if key.iter().any(|value| matches!(value, Value::Null)) {
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
    pub(crate) fn check_no_referencing_rows(
        &self,
        row: &Row,
        ctx: &mut ExecContext<'_>,
    ) -> SqlResult<()> {
        for reference in &self.incoming {
            let key = key_values(row, &reference.referenced_ordinals)?;
            if key.iter().any(|value| matches!(value, Value::Null)) {
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
    let storage = ctx.storage()?;
    let snap = ctx.snapshot()?;
    let mut iter = storage.seek(
        snap,
        index,
        &KeyRange::Point(key.to_vec()),
        Direction::Forward,
    )?;
    Ok(iter.next().transpose()?.is_some())
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
