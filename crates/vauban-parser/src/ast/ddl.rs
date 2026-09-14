//! Data definition: databases, tables, columns, constraints and indexes.

use crate::ast::expr::{DataType, Expr, Ident, ObjectName};
use crate::span::Span;

/// The direction written next to a key column of a column-level constraint.
///
/// `None` in the constraint means nothing was written, so `Display` writes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// `ASC`
    Asc,
    /// `DESC`
    Desc,
}

/// Whether an index or a key is clustered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clustering {
    /// `CLUSTERED`
    Clustered,
    /// `NONCLUSTERED`
    NonClustered,
}

/// One column of an index or of a table-level key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumn {
    /// The column name.
    pub name: Ident,
    /// True for `DESC`.
    pub desc: bool,
    /// True when `ASC` or `DESC` was written, so that `Display` restores it.
    pub explicit_direction: bool,
}

/// What a `FOREIGN KEY` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyRef {
    /// The referenced table.
    pub table: ObjectName,
    /// The referenced columns, empty when the primary key is implied.
    pub columns: Vec<Ident>,
    /// The `ON DELETE` action.
    pub on_delete: Option<RefAction>,
    /// The `ON UPDATE` action.
    pub on_update: Option<RefAction>,
}

/// The action of an `ON DELETE` or `ON UPDATE` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefAction {
    /// `NO ACTION`
    NoAction,
    /// `CASCADE`
    Cascade,
    /// `SET NULL`
    SetNull,
    /// `SET DEFAULT`
    SetDefault,
}

/// One `name = value` entry of the `WITH (…)` list of a key constraint, a `CREATE INDEX`
/// or a `DROP INDEX`: `PAD_INDEX = OFF`, `FILLFACTOR = 80`, `DATA_COMPRESSION = PAGE`.
///
/// The list is **open**: the name is any regular identifier, and nothing here
/// says whether SQL Server knows it or whether the value fits it. What the engine does
/// with an option belongs to the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexOption {
    /// The option name, as written, not delimited (`[PAD_INDEX] = OFF` is a syntax error,
    /// `tests/ddl_table.rs` `index_option_shapes_refused`).
    pub name: String,
    /// The value.
    pub value: IndexOptionValue,
}

/// The value of an [`IndexOption`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOptionValue {
    /// `ON`
    On,
    /// `OFF`
    Off,
    /// An unsigned integer literal, kept as its source text (`80`).
    Integer(String),
    /// A bare word that is neither `ON` nor `OFF` (`PAGE`, `NONE`, `ROW`).
    Word(String),
}

/// The `ON <filegroup>` or `ON <partition scheme> (<column>)` of a table, a key
/// constraint or an index.
///
/// The name is an ordinary identifier: `ON [PRIMARY]` and `ON "default"` are delimited
/// names, a string (`ON 'PRIMARY'`) is read as a delimited name too, and the bare
/// `ON PRIMARY` is a syntax error (156), as SQL Server answers it
/// (`tests/ddl_table.rs` `filegroup_name_as_string`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePlacement {
    /// The filegroup or partition scheme.
    pub name: Ident,
    /// The partitioning column, when the name is a partition scheme: `ON ps (a)`.
    pub partition_column: Option<Ident>,
}

/// The physical storage clauses of a key constraint or an index: its `WITH (…)` options
/// and its `ON …` placement, in that order, each written at most once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStorage {
    /// The `WITH (…)` options, empty when the clause is absent.
    pub options: Vec<IndexOption>,
    /// The `ON …` placement, when written.
    pub placement: Option<StoragePlacement>,
}

/// The `WITH CHECK` or `WITH NOCHECK` written between the table name and `ADD` in an
/// `ALTER TABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintCheck {
    /// `WITH CHECK`
    Check,
    /// `WITH NOCHECK`
    NoCheck,
}

/// The `IDENTITY(seed, increment)` property of a column.
///
/// Both parts are absent when the user wrote a bare `IDENTITY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The seed.
    pub seed: Option<i64>,
    /// The increment.
    pub increment: Option<i64>,
}

/// A constraint written inside a column definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnConstraint {
    /// The `CONSTRAINT name` prefix, when written.
    pub name: Option<Ident>,
    /// What the constraint is.
    pub kind: ColumnConstraintKind,
    /// The `WITH (…)` options and the `ON …` placement that may follow a `PRIMARY KEY`
    /// or a `UNIQUE`; left at its default for the other kinds, which take neither
    /// (`tests/ddl_table.rs` `constraint_storage_clauses_refused`).
    pub storage: IndexStorage,
    /// Position of the constraint.
    pub span: Span,
}

/// The kinds of column-level constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnConstraintKind {
    /// `NULL`, which states that the column accepts null values.
    Null,
    /// `NOT NULL`.
    NotNull,
    /// `DEFAULT expr`.
    Default(Expr),
    /// `PRIMARY KEY [CLUSTERED|NONCLUSTERED] [ASC|DESC]`, its storage clauses in
    /// [`ColumnConstraint::storage`].
    PrimaryKey {
        /// `CLUSTERED` or `NONCLUSTERED`, when written.
        clustering: Option<Clustering>,
        /// `ASC` or `DESC`, when written.
        order: Option<SortDirection>,
    },
    /// `UNIQUE [CLUSTERED|NONCLUSTERED] [ASC|DESC]`, its storage clauses in
    /// [`ColumnConstraint::storage`].
    Unique {
        /// `CLUSTERED` or `NONCLUSTERED`, when written.
        clustering: Option<Clustering>,
        /// `ASC` or `DESC`, when written.
        order: Option<SortDirection>,
    },
    /// `REFERENCES t (c)` with its optional actions.
    ForeignKey(ForeignKeyRef),
    /// `CHECK (expr)`.
    Check {
        /// The predicate.
        expr: Expr,
        /// True for `NOT FOR REPLICATION`.
        not_for_replication: bool,
    },
    /// (V2) `ROWGUIDCOL`.
    RowGuidCol,
}

/// A constraint written at table level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableConstraint {
    /// The `CONSTRAINT name` prefix, when written.
    pub name: Option<Ident>,
    /// What the constraint is.
    pub kind: TableConstraintKind,
    /// The `WITH (…)` options and the `ON …` placement that may follow a `PRIMARY KEY`
    /// or a `UNIQUE`; left at its default for the other kinds, which take neither
    /// (`tests/ddl_table.rs` `constraint_storage_clauses_refused`).
    pub storage: IndexStorage,
    /// Position of the constraint.
    pub span: Span,
}

/// The kinds of table-level constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableConstraintKind {
    /// `PRIMARY KEY [CLUSTERED|NONCLUSTERED] (c1, c2 DESC)`, its storage clauses in
    /// [`TableConstraint::storage`].
    PrimaryKey {
        /// The key columns.
        columns: Vec<IndexColumn>,
        /// `CLUSTERED` or `NONCLUSTERED`, when written.
        clustering: Option<Clustering>,
    },
    /// `UNIQUE [CLUSTERED|NONCLUSTERED] (c1, c2 DESC)`, its storage clauses in
    /// [`TableConstraint::storage`].
    Unique {
        /// The key columns.
        columns: Vec<IndexColumn>,
        /// `CLUSTERED` or `NONCLUSTERED`, when written.
        clustering: Option<Clustering>,
    },
    /// `FOREIGN KEY (c) REFERENCES t (c)`.
    ForeignKey {
        /// The constrained columns.
        columns: Vec<Ident>,
        /// What they point at.
        reference: ForeignKeyRef,
    },
    /// `CHECK (expr)`.
    Check {
        /// The predicate.
        expr: Expr,
        /// True for `NOT FOR REPLICATION`.
        not_for_replication: bool,
    },
}

/// `[CONSTRAINT name] DEFAULT expr FOR column`, the named default SSMS scripts as
/// `ALTER TABLE t ADD CONSTRAINT df DEFAULT ((0)) FOR [c]`.
///
/// An `ALTER TABLE … ADD` produces it and nothing else does: inside the body of a
/// `CREATE TABLE` or of a `DECLARE @t TABLE`, SQL Server 2022 refuses the form (102
/// near 'for'), asserted in `tests/ddl_table.rs` `default_constraint_in_table_body_refused`.
/// Hence a struct of its own rather than a variant of [`TableConstraintKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultConstraint {
    /// The `CONSTRAINT name` prefix, when written.
    pub name: Option<Ident>,
    /// The default value.
    pub expr: Expr,
    /// The column the default applies to.
    pub column: Ident,
    /// Position of the constraint.
    pub span: Span,
}

/// One column of a table definition.
///
/// `identity` is a **field**, not an entry of `constraints`: where `IDENTITY` sat among
/// the written constraints is therefore lost when re-serialising, and
/// `id int PRIMARY KEY IDENTITY(1,1)` comes back as `id int IDENTITY(1, 1) PRIMARY KEY`.
/// The AST stays equal, so the `parse` → `Display` → `parse` loop holds; the deviation is
/// is deliberate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column name.
    pub name: Ident,
    /// The declared type.
    pub ty: DataType,
    /// The `COLLATE` name, when written.
    pub collation: Option<String>,
    /// The column-level constraints, in the order they were written.
    pub constraints: Vec<ColumnConstraint>,
    /// The `IDENTITY` property, when written.
    pub identity: Option<Identity>,
    /// (V2) The expression of a computed column, `AS (expr)`.
    pub computed: Option<Expr>,
    /// Position of the whole definition.
    pub span: Span,
}

/// The body of a table: its columns and its table-level constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDefinition {
    /// The columns, in the order they were written.
    pub columns: Vec<ColumnDef>,
    /// The table-level constraints.
    pub constraints: Vec<TableConstraint>,
}

/// `CREATE TABLE t (…) [ON …] [TEXTIMAGE_ON filegroup]`.
///
/// The `WITH (…)` table options that may follow are read and thrown away: they are not
/// index options and no field holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    /// The table name.
    pub name: ObjectName,
    /// The columns and constraints.
    pub definition: TableDefinition,
    /// The `ON <filegroup>` or `ON <partition scheme> (<column>)`, when written.
    pub placement: Option<StoragePlacement>,
    /// The `TEXTIMAGE_ON <filegroup>`, when written.
    pub textimage_on: Option<Ident>,
    /// Position of the whole statement.
    pub span: Span,
}

/// `ALTER TABLE t <action>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterTableStatement {
    /// The table name.
    pub name: ObjectName,
    /// What is altered.
    pub action: AlterTableAction,
    /// Position of the whole statement.
    pub span: Span,
}

/// What an `ALTER TABLE` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    /// `[WITH CHECK|WITH NOCHECK] ADD c int NOT NULL, d int`.
    AddColumns {
        /// The columns.
        columns: Vec<ColumnDef>,
        /// The `WITH CHECK` or `WITH NOCHECK` prefix, when written. SQL Server 2022
        /// accepts it in front of a column (`tests/ddl_table.rs`).
        with_check: Option<ConstraintCheck>,
    },
    /// `[WITH CHECK|WITH NOCHECK] ADD CONSTRAINT … `.
    AddConstraints {
        /// The constraints.
        constraints: Vec<TableConstraint>,
        /// The `WITH CHECK` or `WITH NOCHECK` prefix, when written.
        with_check: Option<ConstraintCheck>,
    },
    /// `[WITH CHECK|WITH NOCHECK] ADD [CONSTRAINT df] DEFAULT expr FOR c, …`.
    ///
    /// A list of defaults and nothing else: as with columns and constraints, the AST
    /// holds one kind of added element per statement, never a mix
    /// (`tests/ddl_table.rs` `alter_table_add_default_for`).
    AddDefaults {
        /// The defaults.
        defaults: Vec<DefaultConstraint>,
        /// The `WITH CHECK` or `WITH NOCHECK` prefix, when written.
        with_check: Option<ConstraintCheck>,
    },
    /// `DROP COLUMN [IF EXISTS] c, d`.
    DropColumns {
        /// The column names.
        names: Vec<Ident>,
        /// True for `IF EXISTS`.
        if_exists: bool,
    },
    /// `DROP CONSTRAINT [IF EXISTS] pk_t`.
    DropConstraints {
        /// The constraint names.
        names: Vec<Ident>,
        /// True for `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER COLUMN c int NOT NULL`.
    AlterColumn(Box<ColumnDef>),
    /// `[WITH CHECK|WITH NOCHECK] CHECK|NOCHECK CONSTRAINT ALL|c1, c2`.
    Check {
        /// The constraint names. **Empty means `ALL`**: `ALL` is reserved, `parse_ident`
        /// refuses it, so it can never be an `Ident`; `Display` then writes `ALL`.
        constraints: Vec<Ident>,
        /// True for `CHECK CONSTRAINT`, false for `NOCHECK CONSTRAINT`.
        enable: bool,
        /// True for `WITH CHECK`, false for `WITH NOCHECK`.
        with_check: bool,
    },
}

/// `CREATE [UNIQUE] [CLUSTERED] INDEX ix ON t (c) [WITH (…)] [ON …]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndexStatement {
    /// The index name.
    pub name: Ident,
    /// The table it is created on.
    pub table: ObjectName,
    /// The key columns.
    pub columns: Vec<IndexColumn>,
    /// True for `UNIQUE`.
    pub unique: bool,
    /// `CLUSTERED` or `NONCLUSTERED`, when written.
    pub clustering: Option<Clustering>,
    /// (V2) The `INCLUDE (…)` columns.
    pub include: Vec<Ident>,
    /// (V2) The `WHERE` predicate of a filtered index.
    pub where_: Option<Expr>,
    /// The `WITH (…)` options and the `ON …` placement.
    pub storage: IndexStorage,
    /// Position of the whole statement.
    pub span: Span,
}

/// `DROP INDEX [IF EXISTS] ix ON t [WITH (…)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIndexStatement {
    /// The index name.
    pub name: Ident,
    /// The table it belongs to.
    pub table: ObjectName,
    /// True for `IF EXISTS`.
    pub if_exists: bool,
    /// The `WITH (…)` options (`ONLINE = OFF`), empty when the clause is absent.
    pub options: Vec<IndexOption>,
    /// Position of the whole statement.
    pub span: Span,
}

/// `CREATE DATABASE d [options]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateDatabaseStatement {
    /// The database name.
    pub name: Ident,
    /// The options, kept as text pairs: the contents of `ON (NAME = …, FILENAME = …)` is
    /// not modelled finely.
    pub options: Vec<DatabaseOption>,
    /// Position of the whole statement.
    pub span: Span,
}

/// `ALTER DATABASE d SET …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterDatabaseStatement {
    /// The database name.
    pub name: Ident,
    /// The options, in the shape used by `CREATE DATABASE`.
    pub options: Vec<DatabaseOption>,
    /// Position of the whole statement.
    pub span: Span,
}

/// One database option, as a name and an optional value, both kept as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseOption {
    /// The option name, as written.
    pub name: String,
    /// The value, as written, absent for a flag-like option.
    pub value: Option<String>,
}
