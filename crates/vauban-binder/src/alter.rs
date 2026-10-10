//! `ALTER TABLE`: `ADD`/`DROP COLUMN` and `ADD`/`DROP CONSTRAINT`. The entry point below
//! and the [`DdlStatement::AlterTable`](crate::bound::DdlStatement::AlterTable) variant it
//! fills are declared, the action being the one
//! [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table) takes, so that the
//! actions the catalogue adds do not reopen `bound/mod.rs`.
//!
//! `ALTER DATABASE … SET` is the other half of the pair and is **not** here: it belongs
//! with the rest of the database statements, in `ddl.rs`.
//!
//! # What is checked here, and what waits for the control of flow
//!
//! One of the numbers this file raises names an object the catalogue already holds: 2705
//! state 4, an `ADD` of a column the table has. Written at the head of a batch, it is a
//! binding error raised here (`tests/bind_alter_table.rs`, `add_existing_column_is_2705`).
//! Written inside `IF`, `WHILE` or `BEGIN … END`, the statement is not bound here at all:
//! `control.rs` keeps it as it was parsed until the control of flow reaches it, so a branch
//! that is not taken does not raise it (`add_existing_column_in_a_branch_binds_deferred`).
//!
//! The state is what separates 2705 state 4 from 2705 state 3, the column list that writes
//! one name twice: a fault of the written text, which stays a binding error inside a branch
//! (`add_two_columns_duplicate_within_the_list_in_a_branch_is_still_2705`).
//!
//! # What is left to the catalogue
//!
//! Two refusals belong to [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table),
//! which runs them when the statement runs, and are not repeated here: the object that
//! still depends on a dropped column (5074), and the constraint name a table, a
//! `FOREIGN KEY`, a `CHECK` or a `DEFAULT` of the schema already holds (2714 state 5). This
//! file binds both statements (`tests/catalog_view.rs`,
//! `drop_indexed_column_binds_and_leaves_5074_to_the_catalogue`,
//! `a_taken_constraint_name_binds_and_leaves_2714_to_the_catalogue`).

use vauban_catalog::{
    AlterTable, ColumnDef, ConstraintDef, IdentitySpec, QualifiedName, SortedColumn,
};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{
    AlterTableAction, AlterTableStatement, Clustering, ColumnConstraintKind,
    ColumnDef as AstColumnDef, Ident, IndexColumn, ObjectName, RefAction, Span, TableConstraint,
    TableConstraintKind,
};
use vauban_types::{Collation, TypeInfo};

use crate::bound::{BoundStatement, DdlStatement};
use crate::context::{BindContext, CatalogView, ResolvedTable, ResolvedTableKind};
use crate::datatype::resolve_data_type;
use crate::ddl::{check_check_expression, check_default_expression, table_name};
use crate::expr::Scope;
use crate::names::from_needs_the_catalogue;
use crate::query::{not_implemented, not_yet};
use crate::star::Source;

/// State 4 for error 2705 when an `ALTER TABLE … ADD` names a column the table already has.
const DUPLICATE_COLUMN_2705_ALTER_TABLE_STATE: u8 = 4;

/// Binds an `ALTER TABLE` into [`BoundStatement::Ddl`].
///
/// A name [`CatalogView`] does not resolve answers 4902, state 1, on the line of the
/// statement ([`SqlError::cannot_find_object_to_alter_table`]) — a binding error the
/// session defers the same way it defers 208, so a table created earlier in the batch or
/// by a nested `EXEC` still lets the statement bind once it exists.
pub(crate) fn bind_alter_table(
    stmt: &AlterTableStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    let table = table_name(&stmt.name, ctx).map_err(|err| on_statement(err, &stmt.span))?;
    // An internal table of the catalogue answers as a table that does not exist, as SQL
    // Server answers `ALTER TABLE sys.objects ADD c int` (`tests/catalog_view.rs`,
    // `a_write_of_an_internal_table_is_refused`).
    let resolved = catalog
        .resolve_table(&stmt.name, ctx.database, ctx.default_schema)
        .filter(|resolved| {
            !resolved
                .table
                .is_some_and(|storage| catalog.is_internal_table(storage))
        })
        .ok_or_else(|| {
            SqlError::cannot_find_object_to_alter_table(&qualified(&table))
                .with_line(stmt.span.line)
        })?;
    if resolved.kind == ResolvedTableKind::View {
        return Err(not_yet(
            "bind_alter_table: ALTER TABLE on a view is not implemented yet (V2)",
        ));
    }
    let action = match &stmt.action {
        AlterTableAction::AddColumns {
            columns,
            with_check,
        } => {
            refuse_with_check(with_check)?;
            bind_add_columns(columns, &table, &resolved, &stmt.span, ctx)?
        }
        AlterTableAction::DropColumns {
            names,
            if_exists: _,
        } => bind_drop_column(first_ident(names)?, &table, &resolved, &stmt.span)?,
        AlterTableAction::AddConstraints {
            constraints,
            with_check,
        } => {
            refuse_with_check(with_check)?;
            bind_add_constraint(
                first_constraint(constraints)?,
                &table,
                &resolved,
                catalog,
                &stmt.span,
                ctx,
            )?
        }
        AlterTableAction::DropConstraints {
            names,
            if_exists: _,
        } => bind_drop_constraint(first_ident(names)?, &resolved, catalog, &stmt.span)?,
        AlterTableAction::AlterColumn(_) => {
            return Err(not_yet(
                "bind_alter_table: ALTER COLUMN is not implemented yet (V2)",
            ));
        }
        AlterTableAction::AddDefaults { .. } => {
            return Err(not_yet(
                "bind_alter_table: ADD DEFAULT is not implemented yet (V2)",
            ));
        }
        AlterTableAction::Check { .. } => {
            return Err(not_yet(
                "bind_alter_table: CHECK/NOCHECK CONSTRAINT is not implemented yet (V2)",
            ));
        }
    };
    Ok(BoundStatement::Ddl(DdlStatement::AlterTable {
        table,
        action,
    }))
}

/// Binds `ADD a …, b …, …`, one call for the whole list: [`AlterTable::AddColumn`] for one
/// column, [`AlterTable::AddColumns`] from two up.
///
/// Every name is checked twice before any of them is resolved into a [`ColumnDef`] (a
/// resolution that can itself fail, on an unknown data type for instance): first against the
/// columns the table already has (2705, state 4, the qualified table name), then against an
/// earlier name of the same list (2705, state 3, the bare table name). A name that clears the
/// table wins over a duplicate written earlier in the list, and a duplicate against the table
/// wins over a bad type written earlier in the list, in list order otherwise
/// (`add_two_columns_duplicate_against_the_table_beats_an_earlier_unknown_type` in
/// `tests/bind_alter_table.rs`).
fn bind_add_columns(
    columns: &[AstColumnDef],
    table: &QualifiedName,
    resolved: &ResolvedTable,
    span: &Span,
    ctx: &BindContext<'_>,
) -> SqlResult<AlterTable> {
    if columns.is_empty() {
        return Err(SqlError::from(InternalError::Bug(
            "bind_alter_table: empty ADD".into(),
        )));
    }
    for column in columns {
        if resolved
            .columns
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&column.name.value))
        {
            let printed = format!("{}.{}", table.schema, table.name);
            let mut err = SqlError::duplicate_column_name(&column.name.value, &printed);
            err.state = DUPLICATE_COLUMN_2705_ALTER_TABLE_STATE;
            return Err(err.with_line(span.line));
        }
    }
    for (position, column) in columns.iter().enumerate() {
        if columns[..position]
            .iter()
            .any(|earlier| earlier.name.value.eq_ignore_ascii_case(&column.name.value))
        {
            return Err(
                SqlError::duplicate_column_name(&column.name.value, &table.name)
                    .with_line(span.line),
            );
        }
    }
    let base = u32::try_from(resolved.columns.len() + 1).unwrap_or(u32::MAX);
    let mut added = Vec::with_capacity(columns.len());
    for (offset, column) in columns.iter().enumerate() {
        let position = base.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        let (def, default_name) = column_def(column, position, span)?;
        if let Some(expr) = &def.default {
            check_default_expression(expr, &def.ty, span.line, ctx)?;
        }
        added.push((Box::new(def), default_name));
    }
    if added.len() == 1 {
        let (column, default_name) = added.remove(0);
        return Ok(AlterTable::AddColumn {
            column,
            default_name,
        });
    }
    Ok(AlterTable::AddColumns { columns: added })
}

/// Binds `DROP COLUMN c`: 4924 when the table has no column `c`. The objects that depend on
/// `c` are the catalogue's to refuse (module documentation).
fn bind_drop_column(
    name: &str,
    table: &QualifiedName,
    resolved: &ResolvedTable,
    span: &Span,
) -> SqlResult<AlterTable> {
    let Some(column) = resolved
        .columns
        .iter()
        .find(|col| col.name.eq_ignore_ascii_case(name))
    else {
        return Err(
            SqlError::alter_table_drop_column_missing(name, &table.name).with_line(span.line)
        );
    };
    Ok(AlterTable::DropColumn {
        name: column.name.clone(),
    })
}

fn bind_add_constraint(
    constraint: &TableConstraint,
    table: &QualifiedName,
    resolved: &ResolvedTable,
    catalog: &dyn CatalogView,
    span: &Span,
    ctx: &BindContext<'_>,
) -> SqlResult<AlterTable> {
    let scope = table_scope(resolved, table, ctx);
    let def = table_constraint_def(constraint, &scope, ctx, span)?;
    if let ConstraintDef::ForeignKey {
        columns,
        referenced,
        referenced_columns,
        name,
        ..
    } = &def
    {
        bind_foreign_key_reference(ForeignKeyBind {
            columns,
            referenced,
            referenced_columns,
            constraint: name.as_deref().unwrap_or("FK"),
            referencing: resolved,
            referencing_table: &table.name,
            catalog,
            ctx,
            span,
        })?;
    }
    Ok(AlterTable::AddConstraint {
        constraint: Box::new(def),
    })
}

fn bind_drop_constraint(
    name: &str,
    resolved: &ResolvedTable,
    catalog: &dyn CatalogView,
    span: &Span,
) -> SqlResult<AlterTable> {
    if !catalog
        .constraint_names_on_table(resolved.object)
        .iter()
        .any(|constraint| constraint.eq_ignore_ascii_case(name))
    {
        return Err(SqlError::constraint_not_on_table(name).with_line(span.line));
    }
    Ok(AlterTable::DropConstraint {
        name: name.to_owned(),
    })
}

struct ForeignKeyBind<'a, 'b> {
    columns: &'a [String],
    referenced: &'a QualifiedName,
    referenced_columns: &'a [String],
    constraint: &'a str,
    referencing: &'a ResolvedTable,
    referencing_table: &'a str,
    catalog: &'a dyn CatalogView,
    ctx: &'a BindContext<'b>,
    span: &'a Span,
}

fn bind_foreign_key_reference(input: ForeignKeyBind<'_, '_>) -> SqlResult<()> {
    let ForeignKeyBind {
        columns,
        referenced,
        referenced_columns,
        constraint,
        referencing,
        referencing_table,
        catalog,
        ctx,
        span,
    } = input;
    let reference = object_name_from_qualified(referenced);
    let table_part = if referenced.schema.is_empty() {
        referenced.name.clone()
    } else {
        format!("{}.{}", referenced.schema, referenced.name)
    };
    for column in columns {
        if !referencing
            .columns
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(column))
        {
            return Err(SqlError::foreign_key_references_invalid_column(
                constraint,
                column,
                referencing_table,
            )
            .with_line(span.line));
        }
    }
    let Some(resolved) = catalog.resolve_table(&reference, ctx.database, ctx.default_schema) else {
        return Err(foreign_key_invalid_table(constraint, &table_part).with_line(span.line));
    };
    for column in referenced_columns {
        if !resolved
            .columns
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(column))
        {
            return Err(
                SqlError::no_matching_key_in_referenced_table(&table_part, constraint)
                    .with_line(span.line),
            );
        }
    }
    let key = catalog.primary_key_columns(resolved.object).or_else(|| {
        catalog
            .unique_key_on(resolved.object, referenced_columns)
            .then(|| referenced_columns.to_vec())
    });
    let Some(key) = key else {
        return Err(
            SqlError::no_matching_key_in_referenced_table(&table_part, constraint)
                .with_line(span.line),
        );
    };
    if key.len() != referenced_columns.len()
        || !key
            .iter()
            .zip(referenced_columns)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    {
        return Err(
            SqlError::no_matching_key_in_referenced_table(&table_part, constraint)
                .with_line(span.line),
        );
    }
    Ok(())
}

fn table_constraint_def(
    constraint: &TableConstraint,
    scope: &Scope,
    ctx: &BindContext<'_>,
    span: &Span,
) -> SqlResult<ConstraintDef> {
    let name = constraint.name.as_ref().map(|ident| ident.value.clone());
    match &constraint.kind {
        TableConstraintKind::PrimaryKey {
            columns,
            clustering,
        } => Ok(ConstraintDef::PrimaryKey {
            name,
            columns: columns.iter().map(sorted_column).collect(),
            clustered: is_clustered(*clustering, true),
        }),
        TableConstraintKind::Unique {
            columns,
            clustering,
        } => Ok(ConstraintDef::Unique {
            name,
            columns: columns.iter().map(sorted_column).collect(),
            clustered: is_clustered(*clustering, false),
        }),
        TableConstraintKind::ForeignKey { columns, reference } => foreign_key(
            name,
            columns.iter().map(|ident| ident.value.clone()).collect(),
            reference,
            ctx,
            span,
        ),
        TableConstraintKind::Check { expr, .. } => {
            check_check_expression(expr, scope, ctx).map_err(|err| on_statement(err, span))?;
            Ok(ConstraintDef::Check {
                name,
                expr: expr.clone(),
            })
        }
    }
}

/// Binds one column of an `ALTER TABLE … ADD`, and the name written on its `DEFAULT`, if any.
///
/// A `CONSTRAINT df DEFAULT 0` keeps `df`: [`bind_add_columns`] carries it apart from
/// [`ColumnDef::default`], which has no field for a name, to
/// [`AlterTable::AddColumn::default_name`](vauban_catalog::AlterTable), the way
/// `vauban_catalog::Catalog::alter_table` reads it (`crates/vauban-catalog/src/alter.rs`,
/// `plan_change`) — the same split `crates/vauban-binder/src/ddl.rs`'s `column_of` makes for
/// `CREATE TABLE` between [`ColumnDef::default`] and a named [`vauban_catalog::ConstraintDef::Default`]
/// (unit test `add_column_with_a_named_default_keeps_its_name`).
fn column_def(
    column: &AstColumnDef,
    position: u32,
    span: &Span,
) -> SqlResult<(ColumnDef, Option<String>)> {
    if column.computed.is_some() {
        return Err(SqlError::from(InternalError::Bug(format!(
            "bind_alter_table: the computed column '{}' is not implemented yet",
            column.name.value
        ))));
    }
    let ty = resolve_data_type(&column.ty, position)
        .map_err(|err| in_column_list(err, position, &column.ty.name))
        .map_err(|err| on_statement(err, span))?;
    let mut nullable = true;
    let mut default = None;
    let mut default_name = None;
    for constraint in &column.constraints {
        match &constraint.kind {
            ColumnConstraintKind::Null => nullable = true,
            ColumnConstraintKind::NotNull => nullable = false,
            ColumnConstraintKind::Default(expr) if default.is_none() => {
                default = Some(expr.clone());
                default_name = constraint.name.as_ref().map(|ident| ident.value.clone());
            }
            ColumnConstraintKind::PrimaryKey { .. }
            | ColumnConstraintKind::Unique { .. }
            | ColumnConstraintKind::ForeignKey { .. }
            | ColumnConstraintKind::Check { .. }
            | ColumnConstraintKind::Default(_)
            | ColumnConstraintKind::RowGuidCol => {
                return Err(not_implemented("ALTER TABLE ADD column-level constraint"));
            }
        }
    }
    let mut info = TypeInfo::new(ty, nullable);
    if let Some(collation) = &column.collation {
        info.collation = Some(Collation::parse(collation).map_err(|err| on_statement(err, span))?);
    }
    Ok((
        ColumnDef {
            name: column.name.value.clone(),
            ty: info,
            default,
            identity: column.identity.as_ref().map(|identity| IdentitySpec {
                seed: identity.seed.unwrap_or(IdentitySpec::default().seed),
                increment: identity
                    .increment
                    .unwrap_or(IdentitySpec::default().increment),
            }),
            computed: column.computed.clone(),
        },
        default_name,
    ))
}

/// The referenced table is resolved like a `FROM` table: the current database and the
/// default schema of the session fill the parts the statement left out. A reference that
/// names another database answers 1763 on the line of the statement.
fn foreign_key(
    name: Option<String>,
    columns: Vec<String>,
    reference: &vauban_parser::ForeignKeyRef,
    ctx: &BindContext<'_>,
    span: &Span,
) -> SqlResult<ConstraintDef> {
    let referenced = table_name(&reference.table, ctx).map_err(|err| on_statement(err, span))?;
    if !referenced.database.eq_ignore_ascii_case(ctx.database) {
        let printed = format!(
            "{}.{}.{}",
            referenced.database, referenced.schema, referenced.name
        );
        return Err(SqlError::new(
            1763,
            16,
            0,
            format!(
                "Cross-database foreign key references are not supported. Foreign key '{printed}'."
            ),
        )
        .with_line(span.line));
    }
    Ok(ConstraintDef::ForeignKey {
        name,
        columns,
        referenced,
        referenced_columns: reference
            .columns
            .iter()
            .map(|ident| ident.value.clone())
            .collect(),
        on_delete: reference.on_delete.unwrap_or(RefAction::NoAction),
        on_update: reference.on_update.unwrap_or(RefAction::NoAction),
    })
}

fn foreign_key_invalid_table(constraint: &str, referenced: &str) -> SqlError {
    SqlError::new(
        1767,
        16,
        0,
        format!("Foreign key '{constraint}' references invalid table '{referenced}'."),
    )
}

fn refuse_with_check(with_check: &Option<vauban_parser::ConstraintCheck>) -> SqlResult<()> {
    if with_check.is_some() {
        return Err(not_yet(
            "bind_alter_table: WITH CHECK / WITH NOCHECK is not implemented yet (V2)",
        ));
    }
    Ok(())
}

fn table_scope(resolved: &ResolvedTable, table: &QualifiedName, ctx: &BindContext<'_>) -> Scope {
    Scope::over(Source::new(
        &table.name,
        None,
        &resolved.columns,
        ctx.default_schema,
        ctx.database,
    ))
}

fn first_ident(names: &[Ident]) -> SqlResult<&str> {
    names
        .first()
        .map(|ident| ident.value.as_str())
        .ok_or_else(|| {
            SqlError::from(InternalError::Bug(
                "bind_alter_table: empty name list".into(),
            ))
        })
}

fn first_constraint(constraints: &[TableConstraint]) -> SqlResult<&TableConstraint> {
    constraints.first().ok_or_else(|| {
        SqlError::from(InternalError::Bug(
            "bind_alter_table: empty constraint list".into(),
        ))
    })
}

fn sorted_column(column: &IndexColumn) -> SortedColumn {
    SortedColumn {
        column: column.name.value.clone(),
        descending: column.desc,
    }
}

fn is_clustered(clustering: Option<Clustering>, default: bool) -> bool {
    match clustering {
        Some(Clustering::Clustered) => true,
        Some(Clustering::NonClustered) => false,
        None => default,
    }
}

fn qualified(name: &QualifiedName) -> String {
    format!("{}.{}", name.schema, name.name)
}

/// The [`ObjectName`] a three-part name is written as, without delimiters; an empty part is
/// left out. `pub(crate)` for `insert.rs`, which exposes the row of a table under it.
pub(crate) fn object_name_from_qualified(name: &QualifiedName) -> ObjectName {
    ObjectName {
        server: None,
        database: if name.database.is_empty() {
            None
        } else {
            Some(Ident {
                value: name.database.clone(),
                quoted: false,
            })
        },
        schema: if name.schema.is_empty() {
            None
        } else {
            Some(Ident {
                value: name.schema.clone(),
                quoted: false,
            })
        },
        name: Ident {
            value: name.name.clone(),
            quoted: false,
        },
        span: Span::EMPTY,
    }
}

fn in_column_list(err: SqlError, position: u32, name: &str) -> SqlError {
    if err.number == 2715 {
        SqlError::cannot_find_data_type_in_table(position, name)
    } else {
        err
    }
}

fn on_statement(err: SqlError, statement: &Span) -> SqlError {
    err.with_line(statement.line)
}
