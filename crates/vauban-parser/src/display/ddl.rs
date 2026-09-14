//! `Display` for data definition statements.

use std::fmt;

use crate::ast::ddl::{
    AlterDatabaseStatement, AlterTableAction, AlterTableStatement, Clustering, ColumnConstraint,
    ColumnConstraintKind, ColumnDef, ConstraintCheck, CreateDatabaseStatement,
    CreateIndexStatement, CreateTableStatement, DatabaseOption, DefaultConstraint,
    DropIndexStatement, ForeignKeyRef, Identity, IndexColumn, IndexOption, IndexOptionValue,
    IndexStorage, RefAction, SortDirection, StoragePlacement, TableConstraint, TableConstraintKind,
    TableDefinition,
};
use crate::display::{comma_separated, parenthesised_list, separated};

impl fmt::Display for IndexOptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::On => f.write_str("ON"),
            Self::Off => f.write_str("OFF"),
            Self::Integer(text) | Self::Word(text) => f.write_str(text),
        }
    }
}

/// Writes `NAME = VALUE`, the name as written: a delimited option name is a syntax
/// error at the parse (`tests/ddl_table.rs` `index_option_shapes_refused`), so
/// the name has no delimiter to restore.
impl fmt::Display for IndexOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.name, self.value)
    }
}

/// Writes ` WITH (a = b, …)`, leading space included, or nothing for an empty list: the
/// clause `WITH ()` is a syntax error at the parse (`tests/ddl_table.rs`
/// `constraint_storage_clauses_refused`), so an empty list means "not written".
fn write_index_options(f: &mut fmt::Formatter<'_>, options: &[IndexOption]) -> fmt::Result {
    if options.is_empty() {
        return Ok(());
    }
    f.write_str(" WITH ")?;
    parenthesised_list(f, options)
}

/// Writes what follows the `ON` keyword, which the caller writes: `[PRIMARY]` or
/// `ps (col)`.
impl fmt::Display for StoragePlacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(column) = &self.partition_column {
            write!(f, " ({column})")?;
        }
        Ok(())
    }
}

/// Writes ` WITH (…)` then ` ON …`, each with its leading space, each when present: the
/// order SQL Server requires (`tests/ddl_table.rs` `constraint_storage_clauses_refused`),
/// and the whole thing is empty for a key or an index written without storage clauses.
impl fmt::Display for IndexStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_index_options(f, &self.options)?;
        if let Some(placement) = &self.placement {
            write!(f, " ON {placement}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ConstraintCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Check => "WITH CHECK",
            Self::NoCheck => "WITH NOCHECK",
        })
    }
}

impl fmt::Display for SortDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        })
    }
}

impl fmt::Display for Clustering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clustered => "CLUSTERED",
            Self::NonClustered => "NONCLUSTERED",
        })
    }
}

impl fmt::Display for RefAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoAction => "NO ACTION",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        })
    }
}

/// Writes the direction when the user wrote it, and also when `desc` is set without
/// `explicit_direction`, which would otherwise silently lose the descending order.
impl fmt::Display for IndexColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if self.explicit_direction || self.desc {
            f.write_str(if self.desc { " DESC" } else { " ASC" })?;
        }
        Ok(())
    }
}

/// Writes what follows a `REFERENCES` keyword, which the caller writes: the table, the
/// referenced columns when they were written, then the `ON DELETE` and `ON UPDATE`
/// actions.
impl fmt::Display for ForeignKeyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.table)?;
        if !self.columns.is_empty() {
            f.write_str(" ")?;
            parenthesised_list(f, &self.columns)?;
        }
        if let Some(action) = &self.on_delete {
            write!(f, " ON DELETE {action}")?;
        }
        if let Some(action) = &self.on_update {
            write!(f, " ON UPDATE {action}")?;
        }
        Ok(())
    }
}

/// Writes `IDENTITY` alone, or `IDENTITY(seed, increment)` when both parts were written.
impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IDENTITY")?;
        if let (Some(seed), Some(increment)) = (self.seed, self.increment) {
            write!(f, "({seed}, {increment})")?;
        }
        Ok(())
    }
}

/// Writes the constraint, then its storage clauses, which are empty unless a key
/// carried them.
impl fmt::Display for ColumnConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            write!(f, "CONSTRAINT {name} ")?;
        }
        write!(f, "{}{}", self.kind, self.storage)
    }
}

impl fmt::Display for ColumnConstraintKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::NotNull => f.write_str("NOT NULL"),
            Self::Default(expr) => write!(f, "DEFAULT {expr}"),
            Self::PrimaryKey { clustering, order } => {
                f.write_str("PRIMARY KEY")?;
                write_clustering_and_order(f, clustering.as_ref(), order.as_ref())
            }
            Self::Unique { clustering, order } => {
                f.write_str("UNIQUE")?;
                write_clustering_and_order(f, clustering.as_ref(), order.as_ref())
            }
            Self::ForeignKey(reference) => write!(f, "REFERENCES {reference}"),
            Self::Check {
                expr,
                not_for_replication,
            } => write_check(f, expr, *not_for_replication),
            Self::RowGuidCol => f.write_str("ROWGUIDCOL"),
        }
    }
}

/// Writes the optional `CLUSTERED`/`NONCLUSTERED` then the optional `ASC`/`DESC` of a
/// column-level key, each preceded by a space when present.
fn write_clustering_and_order(
    f: &mut fmt::Formatter<'_>,
    clustering: Option<&Clustering>,
    order: Option<&SortDirection>,
) -> fmt::Result {
    if let Some(clustering) = clustering {
        write!(f, " {clustering}")?;
    }
    if let Some(order) = order {
        write!(f, " {order}")?;
    }
    Ok(())
}

/// Writes `CHECK [NOT FOR REPLICATION] (expr)`, the order of T-SQL for both column-level
/// and table-level check constraints.
fn write_check(
    f: &mut fmt::Formatter<'_>,
    expr: &crate::ast::expr::Expr,
    not_for_replication: bool,
) -> fmt::Result {
    f.write_str("CHECK")?;
    if not_for_replication {
        f.write_str(" NOT FOR REPLICATION")?;
    }
    write!(f, " ({expr})")
}

/// Writes the constraint, then its storage clauses, which are empty unless a key
/// carried them.
impl fmt::Display for TableConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            write!(f, "CONSTRAINT {name} ")?;
        }
        write!(f, "{}{}", self.kind, self.storage)
    }
}

impl fmt::Display for DefaultConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            write!(f, "CONSTRAINT {name} ")?;
        }
        write!(f, "DEFAULT {} FOR {}", self.expr, self.column)
    }
}

impl fmt::Display for TableConstraintKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrimaryKey {
                columns,
                clustering,
            } => {
                f.write_str("PRIMARY KEY")?;
                if let Some(clustering) = clustering {
                    write!(f, " {clustering}")?;
                }
                f.write_str(" ")?;
                parenthesised_list(f, columns)
            }
            Self::Unique {
                columns,
                clustering,
            } => {
                f.write_str("UNIQUE")?;
                if let Some(clustering) = clustering {
                    write!(f, " {clustering}")?;
                }
                f.write_str(" ")?;
                parenthesised_list(f, columns)
            }
            Self::ForeignKey { columns, reference } => {
                f.write_str("FOREIGN KEY ")?;
                parenthesised_list(f, columns)?;
                write!(f, " REFERENCES {reference}")
            }
            Self::Check {
                expr,
                not_for_replication,
            } => write_check(f, expr, *not_for_replication),
        }
    }
}

/// Writes a column definition: its name, its type (or the expression of a computed
/// column), its collation, its `IDENTITY` property then its constraints.
///
/// `IDENTITY` is written **before** the constraints whatever their order in the source,
/// because the AST keeps it in a field of its own: `id int PRIMARY KEY IDENTITY(1,1)`
/// comes back as `id int IDENTITY(1, 1) PRIMARY KEY`. The tree stays equal, so the
/// `parse` → `Display` → `parse` loop holds.
impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        match &self.computed {
            Some(expr) => write!(f, " AS {expr}")?,
            None => write!(f, " {}", self.ty)?,
        }
        if let Some(collation) = &self.collation {
            write!(f, " COLLATE {collation}")?;
        }
        if let Some(identity) = &self.identity {
            write!(f, " {identity}")?;
        }
        for constraint in &self.constraints {
            write!(f, " {constraint}")?;
        }
        Ok(())
    }
}

/// Writes the body of a table, **parentheses included**: `(a int, b int, CHECK (a > 0))`.
/// Every caller writes a space then this, be it `CREATE TABLE t (…)` or
/// `DECLARE @t TABLE (…)`.
impl fmt::Display for TableDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        comma_separated(f, &self.columns)?;
        if !self.constraints.is_empty() {
            if !self.columns.is_empty() {
                f.write_str(", ")?;
            }
            comma_separated(f, &self.constraints)?;
        }
        f.write_str(")")
    }
}

/// Writes the body, then `ON …` and `TEXTIMAGE_ON …` when they were written, in the
/// order SQL Server accepts (`tests/ddl_table.rs`
/// `create_table_trailing_clauses_refused` for the reverse one).
impl fmt::Display for CreateTableStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE TABLE {} {}", self.name, self.definition)?;
        if let Some(placement) = &self.placement {
            write!(f, " ON {placement}")?;
        }
        if let Some(filegroup) = &self.textimage_on {
            write!(f, " TEXTIMAGE_ON {filegroup}")?;
        }
        Ok(())
    }
}

impl fmt::Display for AlterTableStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ALTER TABLE {} {}", self.name, self.action)
    }
}

impl fmt::Display for AlterTableAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddColumns {
                columns,
                with_check,
            } => {
                write_with_check_add(f, with_check.as_ref())?;
                comma_separated(f, columns)
            }
            Self::AddConstraints {
                constraints,
                with_check,
            } => {
                write_with_check_add(f, with_check.as_ref())?;
                comma_separated(f, constraints)
            }
            Self::AddDefaults {
                defaults,
                with_check,
            } => {
                write_with_check_add(f, with_check.as_ref())?;
                comma_separated(f, defaults)
            }
            Self::DropColumns { names, if_exists } => {
                f.write_str("DROP COLUMN ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                comma_separated(f, names)
            }
            Self::DropConstraints { names, if_exists } => {
                f.write_str("DROP CONSTRAINT ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                comma_separated(f, names)
            }
            Self::AlterColumn(column) => write!(f, "ALTER COLUMN {column}"),
            Self::Check {
                constraints,
                enable,
                with_check,
            } => {
                f.write_str(if *with_check {
                    "WITH CHECK "
                } else {
                    "WITH NOCHECK "
                })?;
                f.write_str(if *enable { "CHECK " } else { "NOCHECK " })?;
                f.write_str("CONSTRAINT ")?;
                // An empty list means `ALL`: `ALL` is reserved, so it can never be an
                // `Ident`.
                if constraints.is_empty() {
                    f.write_str("ALL")
                } else {
                    comma_separated(f, constraints)
                }
            }
        }
    }
}

/// Writes `WITH CHECK ADD ` or `WITH NOCHECK ADD ` when the prefix was written, `ADD `
/// otherwise.
fn write_with_check_add(
    f: &mut fmt::Formatter<'_>,
    with_check: Option<&ConstraintCheck>,
) -> fmt::Result {
    if let Some(with_check) = with_check {
        write!(f, "{with_check} ")?;
    }
    f.write_str("ADD ")
}

impl fmt::Display for CreateIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.unique {
            f.write_str("UNIQUE ")?;
        }
        if let Some(clustering) = &self.clustering {
            write!(f, "{clustering} ")?;
        }
        write!(f, "INDEX {} ON {} ", self.name, self.table)?;
        parenthesised_list(f, &self.columns)?;
        if !self.include.is_empty() {
            f.write_str(" INCLUDE ")?;
            parenthesised_list(f, &self.include)?;
        }
        if let Some(where_) = &self.where_ {
            write!(f, " WHERE {where_}")?;
        }
        write!(f, "{}", self.storage)
    }
}

/// Writes the modern form `DROP INDEX ix ON t`, never the deprecated `DROP INDEX t.ix`,
/// which the AST does not distinguish, then the `WITH (…)` options when there are any.
impl fmt::Display for DropIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DROP INDEX ")?;
        if self.if_exists {
            f.write_str("IF EXISTS ")?;
        }
        write!(f, "{} ON {}", self.name, self.table)?;
        write_index_options(f, &self.options)
    }
}

/// Writes an option as its name, then its value separated by a space when it has one.
///
/// The two parts are kept as raw text by the parser, which is free to put what it needs
/// in them (`NAME = 'x'`, `COLLATE`, an `ON (…)` clause): the only rule `Display`
/// enforces is the single space between the two, which is why the layout of the options
/// of a `CREATE DATABASE` is an assumed deviation of the re-serialisation.
impl fmt::Display for DatabaseOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(value) = &self.value {
            write!(f, " {value}")?;
        }
        Ok(())
    }
}

impl fmt::Display for CreateDatabaseStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE DATABASE {}", self.name)?;
        if !self.options.is_empty() {
            f.write_str(" ")?;
            separated(f, &self.options, " ")?;
        }
        Ok(())
    }
}

/// Writes `ALTER DATABASE d SET …`, the one form of the statement the module parses;
/// the options are comma-separated.
impl fmt::Display for AlterDatabaseStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ALTER DATABASE {}", self.name)?;
        if !self.options.is_empty() {
            f.write_str(" SET ")?;
            comma_separated(f, &self.options)?;
        }
        Ok(())
    }
}
