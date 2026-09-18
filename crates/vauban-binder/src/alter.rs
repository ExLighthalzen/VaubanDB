//! `ALTER TABLE`: `ADD`/`DROP COLUMN` and `ADD`/`DROP CONSTRAINT`. The entry point below
//! and the [`DdlStatement::AlterTable`](crate::bound::DdlStatement::AlterTable) variant it
//! fills are declared, the action being the one
//! [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table) takes, so that the
//! actions the catalogue adds do not reopen `bound/mod.rs`.
//!
//! `ALTER DATABASE … SET` is the other half of the pair and is **not** here: it belongs
//! with the rest of the database statements, in `ddl.rs`.

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
use crate::ddl::table_name;
use crate::expr::{Scope, bind_condition};
use crate::names::from_needs_the_catalogue;
use crate::query::{not_implemented, not_yet};
use crate::star::Source;

/// State 4 for error 2705 when an `ALTER TABLE … ADD` names a column the table already has.
const DUPLICATE_COLUMN_2705_ALTER_TABLE_STATE: u8 = 4;

/// State 5 for error 2714 when an `ADD CONSTRAINT` reuses a name the database already holds.
const CONSTRAINT_EXISTS_2714_STATE: u8 = 5;

/// Binds an `ALTER TABLE` into [`BoundStatement::Ddl`].
pub(crate) fn bind_alter_table(
    stmt: &AlterTableStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let catalog = ctx.catalog.ok_or_else(from_needs_the_catalogue)?;
    let table = table_name(&stmt.name, ctx).map_err(|err| on_statement(err, &stmt.span))?;
    let resolved = catalog
        .resolve_table(&stmt.name, ctx.database, ctx.default_schema)
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
            bind_add_columns(columns, &table, &resolved, &stmt.span)?
        }
        AlterTableAction::DropColumns {
            names,
            if_exists: _,
        } => bind_drop_column(first_ident(names)?, &table, &resolved, catalog, &stmt.span)?,
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

fn bind_add_columns(
    columns: &[AstColumnDef],
    table: &QualifiedName,
    resolved: &ResolvedTable,
    span: &Span,
) -> SqlResult<AlterTable> {
    let column = columns
        .first()
        .ok_or_else(|| SqlError::from(InternalError::Bug("bind_alter_table: empty ADD".into())))?;
    if columns
        .iter()
        .skip(1)
        .any(|earlier| earlier.name.value.eq_ignore_ascii_case(&column.name.value))
    {
        return Err(
            SqlError::duplicate_column_name(&column.name.value, &table.name).with_line(span.line),
        );
    }
    let position = u32::try_from(resolved.columns.len() + 1).unwrap_or(u32::MAX);
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
    let def = column_def(column, position, span)?;
    Ok(AlterTable::AddColumn {
        column: Box::new(def),
    })
}

fn bind_drop_column(
    name: &str,
    table: &QualifiedName,
    resolved: &ResolvedTable,
    catalog: &dyn CatalogView,
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
    if let Some(index) = catalog.index_on_column(resolved.object, &column.name) {
        return Err(
            SqlError::object_depends_on_column("index", &index, &column.name).with_line(span.line),
        );
    }
    if catalog
        .primary_key_columns(resolved.object)
        .is_some_and(|columns| {
            columns
                .iter()
                .any(|key| key.eq_ignore_ascii_case(&column.name))
        })
    {
        let name = catalog
            .primary_key_constraint_name(resolved.object)
            .unwrap_or_else(|| "PRIMARY KEY".to_owned());
        return Err(
            SqlError::object_depends_on_column("object", &name, &column.name).with_line(span.line),
        );
    }
    for constraint in catalog.constraint_names_on_table(resolved.object) {
        if catalog.check_constraint_mentions_column(resolved.object, &constraint, &column.name)
            || catalog.default_constraint_on_column(resolved.object, &constraint, &column.name)
            || catalog.foreign_key_constraint_on_column(resolved.object, &constraint, &column.name)
            || catalog.unique_constraint_on_column(resolved.object, &constraint, &column.name)
        {
            return Err(
                SqlError::object_depends_on_column("object", &constraint, &column.name)
                    .with_line(span.line),
            );
        }
    }
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
    if let Some(name) = constraint.name.as_ref() {
        refuse_constraint_name(&name.value, catalog, span)?;
    }
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
        return Err(
            SqlError::no_matching_key_in_referenced_table(&table_part, constraint)
                .with_line(span.line),
        );
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
    Ok(match &constraint.kind {
        TableConstraintKind::PrimaryKey {
            columns,
            clustering,
        } => ConstraintDef::PrimaryKey {
            name,
            columns: columns.iter().map(sorted_column).collect(),
            clustered: is_clustered(*clustering, true),
        },
        TableConstraintKind::Unique {
            columns,
            clustering,
        } => ConstraintDef::Unique {
            name,
            columns: columns.iter().map(sorted_column).collect(),
            clustered: is_clustered(*clustering, false),
        },
        TableConstraintKind::ForeignKey { columns, reference } => foreign_key(
            name,
            columns.iter().map(|ident| ident.value.clone()).collect(),
            reference,
        ),
        TableConstraintKind::Check { expr, .. } => {
            bind_condition(expr, ctx, scope).map_err(|err| on_statement(err, span))?;
            ConstraintDef::Check {
                name,
                expr: expr.clone(),
            }
        }
    })
}

fn column_def(column: &AstColumnDef, position: u32, span: &Span) -> SqlResult<ColumnDef> {
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
    for constraint in &column.constraints {
        match &constraint.kind {
            ColumnConstraintKind::Null => nullable = true,
            ColumnConstraintKind::NotNull => nullable = false,
            ColumnConstraintKind::Default(expr) if default.is_none() => {
                default = Some(expr.clone())
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
    Ok(ColumnDef {
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
    })
}

fn foreign_key(
    name: Option<String>,
    columns: Vec<String>,
    reference: &vauban_parser::ForeignKeyRef,
) -> ConstraintDef {
    ConstraintDef::ForeignKey {
        name,
        columns,
        referenced: QualifiedName {
            database: part(reference.table.database.as_ref(), ""),
            schema: part(reference.table.schema.as_ref(), ""),
            name: reference.table.name.value.clone(),
        },
        referenced_columns: reference
            .columns
            .iter()
            .map(|ident| ident.value.clone())
            .collect(),
        on_delete: reference.on_delete.unwrap_or(RefAction::NoAction),
        on_update: reference.on_update.unwrap_or(RefAction::NoAction),
    }
}

fn refuse_constraint_name(name: &str, catalog: &dyn CatalogView, span: &Span) -> SqlResult<()> {
    if catalog.object_name_taken(name) {
        let mut err = SqlError::object_already_exists(name);
        err.state = CONSTRAINT_EXISTS_2714_STATE;
        return Err(err.with_line(span.line));
    }
    Ok(())
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

fn part(part: Option<&Ident>, default: &str) -> String {
    part.map_or_else(|| default.to_owned(), |ident| ident.value.clone())
}

fn qualified(name: &QualifiedName) -> String {
    format!("{}.{}", name.schema, name.name)
}

fn object_name_from_qualified(name: &QualifiedName) -> ObjectName {
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
