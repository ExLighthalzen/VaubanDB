//! What a caller asks the catalogue to create: the `*Def` types the mutations take, and the
//! description of an internal system table.
//!
//! A `*Def` is an **input**, close to what the parser produced but already resolved into
//! catalogue terms (a [`TypeInfo`] rather than a `DataType`, a [`QualifiedName`] rather than
//! an `ObjectName`); the matching `*Meta` of `meta.rs` is the **output**, with
//! the identifiers the catalogue assigned. Turning a
//! [`CreateTableStatement`](vauban_parser::CreateTableStatement) into a [`TableDef`] is the
//! business of this crate, not of its callers.

use vauban_parser::{Expr, RefAction};
use vauban_storage::{KeyColumn, Row};
use vauban_types::TypeInfo;

use crate::ids::ObjectId;
use crate::meta::{IdentitySpec, QualifiedName};

/// A key column named by its name, as a `CREATE` statement writes it.
///
/// The catalogue turns it into a [`KeyColumn`] — a position in the row — once it knows the
/// columns of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortedColumn {
    /// Name of the column.
    pub column: String,
    /// `true` for `DESC`.
    pub descending: bool,
}

/// A column of a [`TableDef`]. The catalogue answers with a
/// [`ColumnMeta`](crate::ColumnMeta), which adds the identifier and the position.
///
/// Distinct from [`vauban_parser::ColumnDef`], which holds the text of the declaration: this
/// one holds a resolved [`TypeInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// Name of the column.
    pub name: String,
    /// Type, nullability and collation of the column.
    pub ty: TypeInfo,
    /// Expression of its `DEFAULT`, `None` when the column has no default clause.
    pub default: Option<Expr>,
    /// `IDENTITY` property, `None` for a column declared without the clause. A bare
    /// `IDENTITY` is [`IdentitySpec::default`].
    pub identity: Option<IdentitySpec>,
    /// Expression of a computed column, `None` for a stored column.
    pub computed: Option<Expr>,
}

/// A constraint of a [`TableDef`].
///
/// `name` is `None` when the statement did not name the constraint: SQL Server then
/// generates one, and the code that creates the constraint decides the generated text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintDef {
    /// `PRIMARY KEY`.
    PrimaryKey {
        /// Name of the constraint, `None` when it was not written.
        name: Option<String>,
        /// Key columns, in key order.
        columns: Vec<SortedColumn>,
        /// `true` for `CLUSTERED`, the default of a `PRIMARY KEY` in SQL Server.
        clustered: bool,
    },
    /// `UNIQUE`.
    Unique {
        /// Name of the constraint, `None` when it was not written.
        name: Option<String>,
        /// Key columns, in key order.
        columns: Vec<SortedColumn>,
        /// `true` for `CLUSTERED`.
        clustered: bool,
    },
    /// `FOREIGN KEY`.
    ForeignKey {
        /// Name of the constraint, `None` when it was not written.
        name: Option<String>,
        /// Constrained columns of this table.
        columns: Vec<String>,
        /// Table pointed at.
        referenced: QualifiedName,
        /// Columns pointed at, in the same order as `columns`.
        referenced_columns: Vec<String>,
        /// `ON DELETE` action.
        on_delete: RefAction,
        /// `ON UPDATE` action.
        on_update: RefAction,
    },
    /// `CHECK`.
    Check {
        /// Name of the constraint, `None` when it was not written.
        name: Option<String>,
        /// Predicate the rows must satisfy.
        expr: Expr,
    },
    /// `DEFAULT`.
    Default {
        /// Name of the constraint, `None` when it was not written.
        name: Option<String>,
        /// Column the default applies to.
        column: String,
        /// Expression evaluated when the column is left out of an `INSERT`.
        expr: Expr,
    },
}

/// The table [`Catalog::create_table`](crate::Catalog::create_table) is asked to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    /// Three-part name of the table.
    pub name: QualifiedName,
    /// Columns, in the order they were declared.
    pub columns: Vec<ColumnDef>,
    /// Constraints declared on the table, column constraints included.
    pub constraints: Vec<ConstraintDef>,
}

/// The index [`Catalog::create_index`](crate::Catalog::create_index) is asked to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// Table the index is built on.
    pub table: ObjectId,
    /// Name of the index.
    pub name: String,
    /// Key columns, in key order.
    pub columns: Vec<SortedColumn>,
    /// `true` for `UNIQUE`.
    pub unique: bool,
    /// `true` for `CLUSTERED`.
    pub clustered: bool,
}

/// A column of an [`InternalTableDef`]: a name and a type.
///
/// [`TableShape`](vauban_storage::TableShape) has no column names — `storage` does not need
/// them — so the names of an internal table live here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InternalColumnDef {
    /// Name of the column, as the view definition reads it.
    pub name: String,
    /// Type of the column.
    pub ty: TypeInfo,
}

/// A view of the catalogue exposed to clients: its name and its T-SQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemViewDef {
    /// Name the client writes, `master.sys.databases` for instance.
    pub name: QualifiedName,
    /// T-SQL text of the view, a `SELECT` over the internal table that carries it.
    pub definition: String,
}

/// One internal system table — its shape, its rows and the views built on it.
///
/// Each file of `views/` describes what it needs with an
/// `internal_tables() -> Vec<InternalTableDef>` (a file may carry several views), and the
/// bootstrap creates in `master` the tables of those that answer a non-empty vector. The
/// name is a `vauban_sys_*` name of our own; what a client reads is the [`SystemViewDef`]
/// built on top of it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InternalTableDef {
    /// Internal name of the table, `vauban_sys_databases` for instance.
    pub name: String,
    /// Columns, in the order of the [`Row`]s below.
    pub columns: Vec<InternalColumnDef>,
    /// Clustered key of the table, in the terms of
    /// [`TableShape::clustered_key`](vauban_storage::TableShape::clustered_key), `None` for
    /// a heap.
    pub clustered_key: Option<Vec<KeyColumn>>,
    /// Rows the bootstrap inserts, each one as long as `columns`.
    pub rows: Vec<Row>,
    /// Views this table carries, `sys.databases` for instance.
    pub views: Vec<SystemViewDef>,
}
