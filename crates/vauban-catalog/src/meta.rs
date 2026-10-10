//! What the catalogue knows about an object once it exists: the `*Meta` types the rest of
//! the engine reads through a [`CatalogSnapshot`](crate::CatalogSnapshot).
//!
//! A `*Meta` describes a **stored** object and is produced by the catalogue; a `*Def`
//! (`def.rs`) describes an object a caller **asks for** and is consumed by the
//! mutations. `CREATE TABLE` takes a [`TableDef`](crate::TableDef) and gives back a
//! [`TableMeta`].

use vauban_errors::{InternalError, SqlResult};
use vauban_parser::{
    BinaryOp, ColumnRef, DataType, Expr, InList, Literal, ParseOptions, QueryBody, RefAction,
    SelectItem, Span, Statement, TypeArg, UnaryOp, parse_batch,
};
use vauban_storage::{DbId, IndexId, KeyColumn, TableId};
use vauban_types::{Collation, TypeInfo};

use crate::ids::{ColumnId, ObjectId};

/// A name as a client wrote it, in three parts, database included.
///
/// The parts are stored with the case they were given; comparison is the business of the
/// resolution in `snapshot.rs`, which uses the collation of the database.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName {
    /// Database part, `master` in `master.sys.objects`.
    pub database: String,
    /// Schema part, `sys` in `master.sys.objects`.
    pub schema: String,
    /// Object part, `objects` in `master.sys.objects`.
    pub name: String,
}

/// The `IDENTITY(seed, increment)` property of a column.
///
/// A bare `IDENTITY` means `IDENTITY(1, 1)`, which is what [`IdentitySpec::default`]
/// builds. Both parts are `i64`, which holds the integer types (`tinyint`, `smallint`,
/// `int`, `bigint`), `bigint` being the widest of those four; an `IDENTITY` on a
/// `decimal(38, 0)` column, whose seed can exceed `i64`, is not served. The value handed to
/// a row is a [`Decimal`](vauban_types::Decimal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdentitySpec {
    /// First value handed out.
    pub seed: i64,
    /// Added to the last value handed out to get the next one.
    pub increment: i64,
}

impl Default for IdentitySpec {
    /// `IDENTITY(1, 1)`, the property of a bare `IDENTITY`.
    fn default() -> Self {
        IdentitySpec {
            seed: 1,
            increment: 1,
        }
    }
}

/// What kind of object an [`ObjectMeta`] describes.
///
/// The four variants are the ones the catalogue needs to tell apart to answer
/// `sys.objects`, `sys.tables`, `sys.views`, `sys.indexes` and the constraint views.
/// Procedures, functions and triggers are not stored yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// A user or system table, `type` `U` or `S` in `sys.objects`.
    Table,
    /// A view, `type` `V`.
    View,
    /// A constraint: `PK`, `UQ`, `F`, `C` or `D` in `sys.objects`.
    Constraint,
    /// An index. Not a row of `sys.objects` in SQL Server; the catalogue gives it an
    /// [`ObjectId`] so that one map holds the named things the catalogue resolves.
    Index,
}

/// A named object of the catalogue — table, view, constraint, index: the row behind
/// `sys.objects` and
/// the result of [`CatalogSnapshot::resolve_object`](crate::CatalogSnapshot::resolve_object).
///
/// The name of a constraint or of an index lives here, not in
/// [`ConstraintMeta`] or [`IndexMeta`]: a name is a property of the object, and one lookup
/// table answers `OBJECT_NAME` for a table as for a constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Identifier of the object, unique within its database.
    pub id: ObjectId,
    /// Kind of object, which says which other `*Meta` describes it further.
    pub kind: ObjectKind,
    /// Three-part name of the object.
    pub name: QualifiedName,
    /// Database the object belongs to, as `storage` numbers them.
    pub database: DbId,
    /// Containing object for a constraint or an index — its table — `None` for a table or a
    /// view. This is `sys.objects.parent_object_id`.
    pub parent: Option<ObjectId>,
    /// T-SQL text of a [`ObjectKind::View`], read by
    /// [`CatalogSnapshot::view_definition`](crate::CatalogSnapshot::view_definition),
    /// `None` for the other kinds.
    pub definition: Option<String>,
}

/// A database, as `sys.databases` publishes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseMeta {
    /// Identifier handed out by
    /// [`Storage::create_database`](vauban_storage::Storage::create_database).
    pub id: DbId,
    /// Name of the database, with the case it was created with.
    pub name: String,
    /// Collation of the database, which the objects it holds inherit.
    pub collation: Collation,
    /// `READ_COMMITTED_SNAPSHOT`, published as `sys.databases.is_read_committed_snapshot_on`:
    /// whether `READ COMMITTED` is served by versioning rather than by locking. The
    /// catalogue carries the flag; the transaction manager makes it effective.
    pub read_committed_snapshot: bool,
    /// `ALLOW_SNAPSHOT_ISOLATION`, published as `sys.databases.snapshot_isolation_state` and
    /// `snapshot_isolation_state_desc`.
    pub snapshot_isolation: SnapshotIsolationState,
}

/// The state of `ALLOW_SNAPSHOT_ISOLATION` of a database: the pair `sys.databases` publishes
/// as `snapshot_isolation_state` (`tinyint`) and `snapshot_isolation_state_desc`
/// (`nvarchar(60)`).
///
/// [`Self::Off`] 0 / `OFF` is what a fresh database shows, [`Self::On`] 1 / `ON` what it
/// shows after `ALTER DATABASE … SET ALLOW_SNAPSHOT_ISOLATION ON` (unit test
/// `snapshot_isolation_state_desc_has_four_labels` in `database.rs`).
///
/// [`Self::InTransitionToOn`] 2 and [`Self::InTransitionToOff`] 3 are the states a database
/// shows while a switch waits for the transactions another session holds open. The catalogue
/// represents them and does not produce them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotIsolationState {
    /// 0, `OFF`: the database refuses `SET TRANSACTION ISOLATION LEVEL SNAPSHOT`.
    Off,
    /// 1, `ON`: the database keeps the versions a snapshot transaction reads.
    On,
    /// 2, `IN_TRANSITION_TO_ON`: a switch to `ON` waits for the open transactions.
    InTransitionToOn,
    /// 3, `IN_TRANSITION_TO_OFF`: a switch to `OFF` waits for them.
    InTransitionToOff,
}

impl SnapshotIsolationState {
    /// The number `sys.databases.snapshot_isolation_state` carries for this state.
    #[must_use]
    pub fn state(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::InTransitionToOn => 2,
            Self::InTransitionToOff => 3,
        }
    }

    /// The text `sys.databases.snapshot_isolation_state_desc` carries for this state.
    #[must_use]
    pub fn desc(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::On => "ON",
            Self::InTransitionToOn => "IN_TRANSITION_TO_ON",
            Self::InTransitionToOff => "IN_TRANSITION_TO_OFF",
        }
    }

    /// The state a stored `snapshot_isolation_state` names, `None` for a number outside the
    /// four of [`Self::state`].
    #[must_use]
    pub fn from_state(state: u8) -> Option<Self> {
        match state {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::InTransitionToOn),
            3 => Some(Self::InTransitionToOff),
            _ => None,
        }
    }
}

/// A database option [`Catalog::set_database_option`](crate::Catalog::set_database_option)
/// switches. The other options of `ALTER DATABASE … SET` are not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseOption {
    /// `READ_COMMITTED_SNAPSHOT`, stored in [`DatabaseMeta::read_committed_snapshot`].
    ReadCommittedSnapshot,
    /// `ALLOW_SNAPSHOT_ISOLATION`, stored in [`DatabaseMeta::snapshot_isolation`] as
    /// [`SnapshotIsolationState::On`] or [`SnapshotIsolationState::Off`].
    AllowSnapshotIsolation,
}

/// A table, with its columns and its constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMeta {
    /// Identifier of the table as an object (`sys.tables.object_id`).
    pub id: ObjectId,
    /// Identifier of the table in `storage`. The two are distinct: `alter_table` builds a
    /// new storage table and switches this field while `id` stays put.
    pub storage_id: TableId,
    /// Database the table lives in.
    pub database: DbId,
    /// Schema part of its name.
    pub schema: String,
    /// Object part of its name.
    pub name: String,
    /// Columns, in the order of the [`Row`](vauban_storage::Row) of `storage`.
    pub columns: Vec<ColumnMeta>,
    /// Index built on the clustered key, when the table has one. `storage` does not treat a
    /// clustered key as an index; the catalogue creates an `IndexShape` equal to the key so
    /// that the planner has something to `seek`.
    pub clustered: Option<IndexId>,
    /// Constraints declared on the table.
    pub constraints: Vec<ConstraintMeta>,
}

/// A column of a [`TableMeta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    /// Identifier of the column within its table (`sys.columns.column_id`).
    pub id: ColumnId,
    /// Name of the column.
    pub name: String,
    /// Type, nullability and collation of the column.
    pub ty: TypeInfo,
    /// Position of the column in the [`Row`](vauban_storage::Row) of `storage`, from `0`.
    /// Distinct from `id`, which keeps its value when a column before it is dropped.
    pub ordinal: u16,
    /// Expression of the `DEFAULT` constraint of the column, the tree read back from its
    /// stored text, `None` when it has no default.
    pub default: Option<Expr>,
    /// `IDENTITY` property of the column, `None` for a column declared without the clause.
    pub identity: Option<IdentitySpec>,
    /// Expression of a computed column, the tree read back from its normalised text, `None`
    /// for a stored column.
    pub computed: Option<Expr>,
}

/// An index, as `sys.indexes` publishes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    /// Identifier handed out by
    /// [`Storage::create_index`](vauban_storage::Storage::create_index).
    pub id: IndexId,
    /// Name of the index.
    pub name: String,
    /// `true` when two live rows may not share a key.
    pub unique: bool,
    /// `true` when the index carries the clustered key of its table.
    pub clustered: bool,
    /// `true` when the index backs a `PRIMARY KEY` constraint.
    pub primary_key: bool,
    /// Key columns, in key order, numbered as `storage` numbers them (position in the row).
    pub columns: Vec<KeyColumn>,
}

/// A constraint declared on a table.
///
/// The variants carry what the constraint *does*; its **name** is in the [`ObjectMeta`] of
/// kind [`ObjectKind::Constraint`] that the catalogue stores beside it, one lookup table
/// answering `OBJECT_NAME` for a table as for a constraint. The `FOREIGN KEY`, `CHECK` and
/// `DEFAULT` variants carry the identifier of that object, so that a reader of a
/// [`ConstraintMeta::ForeignKey`] reaches the object without a search
/// (`tests/constraints.rs`, `named_constraints_become_objects`); a `PRIMARY KEY` and a
/// `UNIQUE` are named by their index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintMeta {
    /// `PRIMARY KEY`, backed by the index of this identifier.
    PrimaryKey(IndexId),
    /// `UNIQUE`, backed by the index of this identifier.
    Unique(IndexId),
    /// `FOREIGN KEY`.
    ForeignKey {
        /// Identifier of the constraint as an object of the catalogue, whose [`ObjectMeta`]
        /// of kind [`ObjectKind::Constraint`] carries its name and its parent table
        /// (`constraints.rs`).
        constraint: ObjectId,
        /// `true` when the catalogue made the name up rather than reading it from the
        /// statement, which is what `sys.foreign_keys.is_system_named` publishes
        /// (`tests/constraints.rs`, `anonymous_constraint_name_has_the_generated_shape`).
        system_named: bool,
        /// Constrained columns of this table, in the order they were declared.
        columns: Vec<ColumnId>,
        /// Table pointed at.
        referenced_table: ObjectId,
        /// Columns pointed at, in the same order as `columns`.
        referenced_columns: Vec<ColumnId>,
        /// Index of the referenced table whose key is `referenced_columns`: the
        /// `PRIMARY KEY` or `UNIQUE` index that `sys.foreign_keys.key_index_id` publishes.
        referenced_index: IndexId,
        /// `ON DELETE` action, `NO ACTION` when the clause was left out. Stored here; the
        /// executor applies it.
        on_delete: RefAction,
        /// `ON UPDATE` action, `NO ACTION` when the clause was left out, stored under the
        /// rule written on `on_delete`.
        on_update: RefAction,
    },
    /// `CHECK`.
    Check {
        /// Identifier of the constraint as an object of the catalogue, as in
        /// [`ConstraintMeta::ForeignKey`].
        constraint: ObjectId,
        /// `true` when the catalogue made the name up, as in
        /// [`ConstraintMeta::ForeignKey`].
        system_named: bool,
        /// Predicate the rows must satisfy: the tree `stored_expression` (this file) read back
        /// from `definition`, not the tree the statement wrote.
        expr: Expr,
        /// Text `sys.check_constraints.definition` publishes: the normalised text
        /// `stored_expression` writes, `([a]>=(0))` for `CHECK (a >= 0)`. `expr` is read
        /// back from it, so a table rebuilt from this text holds the same predicate
        /// (`tests/sys_constraints.rs`,
        /// `a_table_rebuilt_from_its_stored_text_holds_the_same_expressions`).
        definition: String,
    },
    /// `DEFAULT`.
    Default {
        /// Identifier of the constraint as an object of the catalogue, as in
        /// [`ConstraintMeta::ForeignKey`].
        constraint: ObjectId,
        /// `true` when the catalogue made the name up, as in
        /// [`ConstraintMeta::ForeignKey`].
        system_named: bool,
        /// Column the default applies to. Its [`ColumnMeta::id`] counts from `1` and is the
        /// number `sys.default_constraints.parent_column_id` carries.
        column: ColumnId,
        /// Expression evaluated when the column is left out of an `INSERT`: the tree read back
        /// from the text `sys.default_constraints.definition` publishes, `((0))` for
        /// `DEFAULT 0` (`tests/sys_constraints.rs`,
        /// `a_table_rebuilt_from_its_stored_text_holds_the_same_expressions`).
        expr: Expr,
    },
}

impl ColumnMeta {
    /// The stored text of the expression of a computed column, `None` for a stored column:
    /// the text `sys.computed_columns.definition` publishes, which the binder parses and
    /// binds (unit test `the_expression_texts_are_the_published_texts`).
    pub fn computed_text(&self) -> Option<String> {
        self.computed.as_ref().map(definition_text)
    }

    /// The stored text of the `DEFAULT` of the column, `None` for a column without one: the text
    /// `sys.default_constraints.definition` publishes, `((0))` for `DEFAULT 0`.
    pub fn default_text(&self) -> Option<String> {
        self.default.as_ref().map(definition_text)
    }
}

impl ConstraintMeta {
    /// The stored text of the expression of a `CHECK` or of a `DEFAULT`, `None` for the
    /// other kinds: `([a]>=(0))` for `CHECK (a >= 0)`, `((0))` for `DEFAULT 0` (unit test
    /// `the_expression_texts_are_the_published_texts`).
    pub fn expression_text(&self) -> Option<String> {
        match self {
            ConstraintMeta::Check { definition, .. } => Some(definition.clone()),
            ConstraintMeta::Default { expr, .. } => Some(definition_text(expr)),
            ConstraintMeta::PrimaryKey(_)
            | ConstraintMeta::Unique(_)
            | ConstraintMeta::ForeignKey { .. } => None,
        }
    }
}

pub use crate::def::AlterTable;

/// How the text of an object expression is read back: a `CHECK` is a predicate, a `DEFAULT`
/// and a computed column are values. The two are parsed in different positions of a `SELECT`
/// ([`parse_stored_expression`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpressionKind {
    /// The predicate of a `CHECK`.
    Predicate,
    /// The value of a `DEFAULT` or of a computed column.
    Value,
}

/// The text the catalogue stores for `expr`, and the tree read back from that text.
///
/// The text is the one `sys.check_constraints.definition`, `sys.default_constraints.definition`
/// and `INFORMATION_SCHEMA.COLUMNS.COLUMN_DEFAULT` publish ([`definition_text`]); the tree is
/// [`parse_stored_expression`] of that text, so that a [`TableMeta`] built from the statement
/// and one rebuilt from its stored rows hold the same tree (unit test
/// `a_tree_rebuilt_from_its_text_is_the_stored_tree`).
///
/// # Errors
///
/// [`InternalError::Bug`] when neither the normalised text nor the text `Display` writes
/// reads back as one expression.
pub(crate) fn stored_expression(expr: &Expr, kind: ExpressionKind) -> SqlResult<(String, Expr)> {
    if let Some(text) = normalized_text(expr)
        && let Ok(tree) = parse_stored_expression(&text, kind)
    {
        return Ok((text, tree));
    }
    let text = display_text(expr);
    let tree = parse_stored_expression(&text, kind)?;
    Ok((text, tree))
}

/// The text the catalogue publishes for `expr`: the normalised form of
/// [`normalized_text`], or, for a shape that function does not write (a subquery, a
/// windowed call, a qualified column), the text `Display` writes between parentheses.
///
/// Applied to the tree [`stored_expression`] read back, it gives the stored text again (unit
/// test `the_text_of_a_stored_tree_is_the_stored_text`).
pub(crate) fn definition_text(expr: &Expr) -> String {
    normalized_text(expr).unwrap_or_else(|| display_text(expr))
}

/// `Display` of `expr` between one pair of parentheses, one level of parentheses of `expr`
/// itself taken off so that the text of a tree read back from this text is this text.
fn display_text(expr: &Expr) -> String {
    match expr {
        Expr::Nested(inner, _) => format!("({inner})"),
        other => format!("({other})"),
    }
}

/// Reads back the text of an object expression.
///
/// A predicate is read as the `WHERE` of `SELECT 1 WHERE <text>`, a value as the one item of
/// `SELECT <text>`. A type name written between brackets, `CONVERT([int],(1))`, is kept with
/// its brackets by `vauban-parser`; it is taken off here so that the tree names the type
/// `int`, as it would for `CONVERT(int, 1)`.
///
/// # Errors
///
/// The syntax error of `vauban-parser`, or [`InternalError::Bug`] when the text holds
/// something else than one expression.
pub(crate) fn parse_stored_expression(text: &str, kind: ExpressionKind) -> SqlResult<Expr> {
    let batch = match kind {
        ExpressionKind::Predicate => format!("SELECT 1 WHERE {text}"),
        ExpressionKind::Value => format!("SELECT {text}"),
    };
    let batch = parse_batch(&batch, &ParseOptions::default())?;
    let found = match batch.statements.as_slice() {
        [Statement::Select(select)] => match &select.body {
            QueryBody::Select(spec) if spec.from.is_empty() => match kind {
                ExpressionKind::Predicate => spec.where_.clone(),
                ExpressionKind::Value => match spec.items.as_slice() {
                    [
                        SelectItem::Expr {
                            expr, alias: None, ..
                        },
                    ] if spec.where_.is_none() => Some(expr.clone()),
                    _ => None,
                },
            },
            _ => None,
        },
        _ => None,
    };
    let mut tree = found.ok_or_else(|| {
        InternalError::Bug(format!(
            "catalog: the stored expression {text} does not read back as one expression"
        ))
    })?;
    unbracket_type_names(&mut tree);
    Ok(tree)
}

/// Takes the brackets off the type names of the `CAST` and `CONVERT` calls of `expr`.
fn unbracket_type_names(expr: &mut Expr) {
    match expr {
        Expr::Cast {
            expr: inner, ty, ..
        } => {
            unbracket_type_name(ty);
            unbracket_type_names(inner);
        }
        Expr::Convert {
            ty,
            expr: inner,
            style,
            ..
        } => {
            unbracket_type_name(ty);
            unbracket_type_names(inner);
            if let Some(style) = style {
                unbracket_type_names(style);
            }
        }
        Expr::Binary { left, right, .. } => {
            unbracket_type_names(left);
            unbracket_type_names(right);
        }
        Expr::Unary { expr: inner, .. }
        | Expr::Nested(inner, _)
        | Expr::IsNull { expr: inner, .. }
        | Expr::Collate { expr: inner, .. } => unbracket_type_names(inner),
        Expr::Function { args, .. } => args.iter_mut().for_each(unbracket_type_names),
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            if let Some(operand) = operand {
                unbracket_type_names(operand);
            }
            for arm in arms {
                unbracket_type_names(&mut arm.when);
                unbracket_type_names(&mut arm.then);
            }
            if let Some(else_) = else_ {
                unbracket_type_names(else_);
            }
        }
        Expr::In {
            expr: inner, list, ..
        } => {
            unbracket_type_names(inner);
            if let InList::Exprs(items) = list {
                items.iter_mut().for_each(unbracket_type_names);
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            ..
        } => {
            unbracket_type_names(inner);
            unbracket_type_names(pattern);
            if let Some(escape) = escape {
                unbracket_type_names(escape);
            }
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            unbracket_type_names(inner);
            unbracket_type_names(low);
            unbracket_type_names(high);
        }
        _ => {}
    }
}

/// `[varchar]` to `varchar`, `]]` inside the brackets to `]`.
fn unbracket_type_name(ty: &mut DataType) {
    if let Some(inner) = ty
        .name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        ty.name = inner.replace("]]", "]");
    }
}

/// Where an expression is written, which decides the parentheses around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The whole expression, inside the outer pair of parentheses.
    Top,
    /// An operand of a comparison or of `IS NULL`.
    Comparison,
    /// An operand of `+`, `-`, `&`, `|` or `^`.
    Additive,
    /// An operand of `*`, `/` or `%`.
    Multiplicative,
    /// The operand of a unary `-` or `~`.
    Sign,
    /// An operand of `AND`, `OR` or `NOT`.
    Logic,
    /// An argument of a call, a branch of a `CASE`, an operand of `LIKE` or `COLLATE`.
    Other,
}

/// The normalised text of `expr`, the form SQL Server publishes in the `definition` of a
/// `CHECK` or of a `DEFAULT`, `None` for a shape this function does not write.
///
/// The rules, frozen by the unit test `the_definition_text_is_normalised` on the forms its
/// corpus holds:
///
/// - the whole text is between one pair of parentheses, and the parentheses the statement
///   wrote are dropped: `CHECK ((a > 0))` is `([a]>(0))`;
/// - a column is between brackets, `]` doubled; a number is between parentheses, `(0)`,
///   `(-1)`, `(1.5)`, a sign before a number folded into it (`-(1)` is `(-1)`, `- -1` is
///   `(1)`); an integer outside the range of `int` gets a final point (`(2147483648.)`); a
///   `float` is written with sixteen decimals and a three-digit exponent
///   (`(1.0000000000000000e+003)`), a `money` with four decimals (`($5.0000)`); a string, a
///   binary and `NULL` are bare (`'x'`, `N'x'`, `0xAB`, `NULL`);
/// - operators are written without spaces, `AND`, `OR` and `NOT` with spaces; `!=` is `<>`,
///   `!<` is `>=`, `!>` is `<=`;
/// - an arithmetic operation inside another one is between parentheses, except a `*`, `/`
///   or `%` under an additive one (`+`, `-`, `&`, `|`, `^`): `(1)+(2)*(3)` but
///   `((1)+(2))*(3)` and `((1)+(2))+(3)`; an additive one under a comparison is between
///   parentheses too: `([a]+(1))>(0)` but `[a]*(2)>(-1)`; a unary `-` over something else
///   than a number is ` -` between parentheses under an operator, `( -[a])<(0)`, and bare
///   in a call, a `CONVERT` or a `CASE`, `abs( -[a])`, `then  -[a]`; a column qualified by
///   its table is `[t].[a]`;
/// - an `OR` under an `AND` is between parentheses, as is a right operand of the same
///   operator: `[a]=(1) OR ([a]=(2) OR [a]=(3))`; `NOT` puts parentheses around an `AND` or an
///   `OR`;
/// - `IN` is a chain of `OR` over the list read from its end (`a IN (1, 2)` is
///   `[a]=(2) OR [a]=(1)`), `BETWEEN` an `AND` of `>=` and `<=`, `NOT LIKE` is `NOT … like`;
/// - a function is in lower case, its arguments separated by a bare comma; `CURRENT_TIMESTAMP`
///   is `getdate()`, `USER`, `CURRENT_USER` and `SESSION_USER` are `user_name()`,
///   `SYSTEM_USER` is `suser_sname()`; `YEAR` is `datepart(year,…)` (`MONTH` and `DAY` are
///   written on that shape, outside the corpus), and the first argument of `datepart`,
///   `dateadd` and `datediff` is the full name of the part (`dd` is `day`);
/// - `CAST` is `CONVERT([type],…)`, `TRY_CAST` and `TRY_CONVERT` without style are
///   `TRY_CAST(… AS [type])`; `CASE`, `when`, `then`, `else`, `end`, `like`, `escape` and
///   `collate` are in lower case.
pub(crate) fn normalized_text(expr: &Expr) -> Option<String> {
    Some(format!("({})", write(expr, Slot::Top)?))
}

/// The text of `expr` written in `slot`. See [`normalized_text`].
fn write(expr: &Expr, slot: Slot) -> Option<String> {
    match expr {
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => write(inner, slot),
        Expr::Literal(literal, _) => literal_text(literal, false),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr: inner,
            ..
        } => match signed_literal(inner, true) {
            Some((literal, negative)) => literal_text(literal, negative),
            // ` -[a]`, between parentheses under an operator: `( -[a])<(0)`; as the whole
            // expression the outer pair stands for them: `( -((1)+(1)))`.
            None => {
                // Bare in a call, a `CONVERT` or a `CASE` too: `abs( -[a])`,
                // `then  -[a]`, `case  -[a] when`.
                let text = format!(" -{}", write(inner, Slot::Sign)?);
                Some(if matches!(slot, Slot::Top | Slot::Other) {
                    text
                } else {
                    format!("({text})")
                })
            }
        },
        Expr::Unary {
            op: UnaryOp::BitNot,
            expr: inner,
            ..
        } => {
            let text = format!("~{}", write(inner, Slot::Sign)?);
            Some(if slot == Slot::Sign {
                format!("({text})")
            } else {
                text
            })
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr: inner,
            ..
        } => Some(format!("NOT {}", negated_text(inner)?)),
        Expr::Column(column) => column_text(column),
        Expr::Variable { name, .. } => name.starts_with("@@").then(|| name.to_ascii_lowercase()),
        Expr::Binary {
            op, left, right, ..
        } => binary_text(*op, left, right, slot),
        Expr::Function {
            name,
            args,
            star: false,
            distinct: false,
            over: None,
            ..
        } if name.server.is_none() && name.database.is_none() && name.schema.is_none() => {
            function_text(&name.name.value, args)
        }
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            let mut text = String::from("case");
            if let Some(operand) = operand {
                text.push(' ');
                text.push_str(&write(operand, Slot::Other)?);
            }
            for arm in arms {
                text.push_str(" when ");
                text.push_str(&write(&arm.when, Slot::Other)?);
                text.push_str(" then ");
                text.push_str(&write(&arm.then, Slot::Other)?);
            }
            // `case when (1)=(1) then (1) else (0) end`, and two spaces before `end` without
            // an `else`: `case [a] when (1) then 'x'  end`.
            text.push(' ');
            if let Some(else_) = else_ {
                text.push_str("else ");
                text.push_str(&write(else_, Slot::Other)?);
            }
            text.push_str(" end");
            Some(text)
        }
        Expr::Cast {
            expr: inner,
            ty,
            try_,
            ..
        } => conversion_text(ty, inner, None, *try_),
        Expr::Convert {
            ty,
            expr: inner,
            style,
            try_,
            ..
        } => conversion_text(ty, inner, style.as_deref(), *try_),
        Expr::IsNull {
            expr: inner,
            negated,
            ..
        } => Some(format!(
            "{} IS {}NULL",
            write(inner, Slot::Comparison)?,
            if *negated { "NOT " } else { "" }
        )),
        Expr::In {
            expr: inner,
            list: InList::Exprs(items),
            negated,
            ..
        } => {
            let chain = in_as_chain(inner, items)?;
            if *negated {
                Some(format!("NOT {}", negated_text(&chain)?))
            } else {
                write(&chain, slot)
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            negated,
            ..
        } => {
            let mut text = format!(
                "{} like {}",
                write(inner, Slot::Other)?,
                write(pattern, Slot::Other)?
            );
            if let Some(escape) = escape {
                text.push_str(&format!(" escape {} ", write(escape, Slot::Other)?));
            }
            Some(if *negated {
                format!("NOT {text}")
            } else {
                text
            })
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
            ..
        } => {
            let range = binary(
                BinaryOp::And,
                binary(BinaryOp::Ge, (**inner).clone(), (**low).clone()),
                binary(BinaryOp::Le, (**inner).clone(), (**high).clone()),
            );
            if *negated {
                Some(format!("NOT {}", negated_text(&range)?))
            } else {
                write(&range, slot)
            }
        }
        Expr::Collate {
            expr: inner,
            collation,
            ..
        } => Some(format!(
            "({}) collate {collation}",
            write(inner, Slot::Other)?
        )),
        Expr::NextValueFor {
            sequence,
            over: None,
            ..
        } => {
            let parts: Vec<String> = [&sequence.database, &sequence.schema]
                .into_iter()
                .flatten()
                .chain(std::iter::once(&sequence.name))
                .map(|part| bracketed(&part.value))
                .collect();
            Some(format!("NEXT VALUE FOR {}", parts.join(".")))
        }
        _ => None,
    }
}

/// A binary node built from two operands, with no text behind it.
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        op_span: Span::EMPTY,
        left: Box::new(left),
        right: Box::new(right),
        span: Span::EMPTY,
    }
}

/// `e IN (i1, …, in)` as `e=in OR … OR e=i1`, `None` for an empty list.
fn in_as_chain(expr: &Expr, items: &[Expr]) -> Option<Expr> {
    let mut items = items.iter().rev();
    let first = items.next()?;
    let mut chain = binary(BinaryOp::Eq, expr.clone(), first.clone());
    for item in items {
        chain = binary(
            BinaryOp::Or,
            chain,
            binary(BinaryOp::Eq, expr.clone(), item.clone()),
        );
    }
    Some(chain)
}

/// The operand of `NOT`: between parentheses when it is an `AND` or an `OR`.
fn negated_text(expr: &Expr) -> Option<String> {
    let text = write(expr, Slot::Logic)?;
    Some(if logic_operator(expr).is_some() {
        format!("({text})")
    } else {
        text
    })
}

/// `AND` or `OR` when `expr` is written as one: the operator itself, an `IN` over two items
/// or more (a chain of `OR`), a `BETWEEN` (an `AND`).
fn logic_operator(expr: &Expr) -> Option<BinaryOp> {
    match expr {
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => logic_operator(inner),
        Expr::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or),
            ..
        } => Some(*op),
        Expr::In {
            list: InList::Exprs(items),
            negated: false,
            ..
        } if items.len() > 1 => Some(BinaryOp::Or),
        Expr::Between { negated: false, .. } => Some(BinaryOp::And),
        _ => None,
    }
}

/// The text of a binary operation written in `slot`.
fn binary_text(op: BinaryOp, left: &Expr, right: &Expr, slot: Slot) -> Option<String> {
    if let Some(keyword) = match op {
        BinaryOp::And => Some("AND"),
        BinaryOp::Or => Some("OR"),
        _ => None,
    } {
        let mut left_text = write(left, Slot::Logic)?;
        if op == BinaryOp::And && logic_operator(left) == Some(BinaryOp::Or) {
            left_text = format!("({left_text})");
        }
        let mut right_text = write(right, Slot::Logic)?;
        if let Some(child) = logic_operator(right)
            && (child == op || op == BinaryOp::And)
        {
            right_text = format!("({right_text})");
        }
        return Some(format!("{left_text} {keyword} {right_text}"));
    }
    let (symbol, comparison, additive) = match op {
        BinaryOp::Eq => ("=", true, false),
        BinaryOp::Ne => ("<>", true, false),
        BinaryOp::Lt => ("<", true, false),
        BinaryOp::Le | BinaryOp::NotGt => ("<=", true, false),
        BinaryOp::Gt => (">", true, false),
        BinaryOp::Ge | BinaryOp::NotLt => (">=", true, false),
        BinaryOp::Mul => ("*", false, false),
        BinaryOp::Div => ("/", false, false),
        BinaryOp::Mod => ("%", false, false),
        BinaryOp::Add | BinaryOp::Concat => ("+", false, true),
        BinaryOp::Sub => ("-", false, true),
        BinaryOp::BitAnd => ("&", false, true),
        BinaryOp::BitOr => ("|", false, true),
        BinaryOp::BitXor => ("^", false, true),
        BinaryOp::And | BinaryOp::Or => return None,
    };
    if comparison {
        return Some(format!(
            "{}{symbol}{}",
            write(left, Slot::Comparison)?,
            write(right, Slot::Comparison)?
        ));
    }
    let operand = if additive {
        Slot::Additive
    } else {
        Slot::Multiplicative
    };
    let text = format!(
        "{}{symbol}{}",
        write(left, operand)?,
        write(right, operand)?
    );
    // `(1)+(2)*(3)`, `((1)+(2))*(3)`, `((1)+(2))+(3)`, `([a]*[a2])*[b2]`.
    let wrapped = match slot {
        Slot::Multiplicative | Slot::Sign => true,
        Slot::Additive | Slot::Comparison => additive,
        Slot::Top | Slot::Logic | Slot::Other => false,
    };
    Some(if wrapped { format!("({text})") } else { text })
}

/// The functions whose first argument names a part of a date.
const DATE_PART_FUNCTIONS: [&str; 6] = [
    "datepart",
    "dateadd",
    "datediff",
    "datediff_big",
    "datename",
    "datetrunc",
];

/// The text of a call of the unqualified function `name`.
fn function_text(name: &str, args: &[Expr]) -> Option<String> {
    let name = name.to_ascii_lowercase();
    if matches!(name.as_str(), "year" | "month" | "day")
        && let [arg] = args
    {
        return Some(format!("datepart({name},{})", write(arg, Slot::Other)?));
    }
    let mut texts = Vec::with_capacity(args.len());
    for (position, arg) in args.iter().enumerate() {
        let part = (position == 0 && DATE_PART_FUNCTIONS.contains(&name.as_str()))
            .then(|| date_part_name(arg))
            .flatten();
        texts.push(match part {
            Some(part) => part,
            None => write(arg, Slot::Other)?,
        });
    }
    Some(format!("{name}({})", texts.join(",")))
}

/// The full name of the date part an unquoted name designates, `day` for `dd`; `None` for
/// another shape of argument.
///
/// The corpus of `the_definition_text_is_normalised` holds `dd` and `year`; the other names
/// follow the abbreviation table of `DATEPART`, and a name outside it is kept in lower case.
fn date_part_name(arg: &Expr) -> Option<String> {
    let Expr::Column(ColumnRef {
        qualifier: None,
        name,
        ..
    }) = arg
    else {
        return None;
    };
    if name.quoted {
        return None;
    }
    let written = name.value.to_ascii_lowercase();
    let full = match written.as_str() {
        "yy" | "yyyy" => "year",
        "qq" | "q" => "quarter",
        "mm" | "m" => "month",
        "dy" | "y" => "dayofyear",
        "dd" | "d" => "day",
        "wk" | "ww" => "week",
        "dw" | "w" => "weekday",
        "hh" => "hour",
        "mi" | "n" => "minute",
        "ss" | "s" => "second",
        "ms" => "millisecond",
        "mcs" => "microsecond",
        "ns" => "nanosecond",
        "tz" => "tzoffset",
        "isowk" | "isoww" => "iso_week",
        other => other,
    };
    Some(full.to_owned())
}

/// `CONVERT([type],expr[,style])`, or `TRY_CAST(expr AS [type])` for a `TRY_CAST` and for a
/// `TRY_CONVERT` without style.
fn conversion_text(ty: &DataType, expr: &Expr, style: Option<&Expr>, try_: bool) -> Option<String> {
    let ty = type_text(ty)?;
    let expr = write(expr, Slot::Other)?;
    match (try_, style) {
        (true, None) => Some(format!("TRY_CAST({expr} AS {ty})")),
        (true, Some(_)) => None,
        (false, None) => Some(format!("CONVERT({ty},{expr})")),
        (false, Some(style)) => Some(format!(
            "CONVERT({ty},{expr},{})",
            write(style, Slot::Other)?
        )),
    }
}

/// `[varchar](10)`, `[decimal](10,2)`, `[nvarchar](max)`, `[int]`.
fn type_text(ty: &DataType) -> Option<String> {
    let name = ty
        .name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .map_or_else(|| ty.name.clone(), |inner| inner.replace("]]", "]"));
    let mut text = bracketed(&name.to_ascii_lowercase());
    if !ty.args.is_empty() {
        let mut args = Vec::with_capacity(ty.args.len());
        for arg in &ty.args {
            args.push(match arg {
                TypeArg::Number(number) => number.to_string(),
                TypeArg::Max => "max".to_owned(),
                TypeArg::Ident(_) => return None,
            });
        }
        text.push_str(&format!("({})", args.join(",")));
    }
    Some(text)
}

/// `[name]`, `]` doubled.
fn bracketed(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// A column between brackets, or the call a niladic name stands for. A column qualified by
/// its table is `[t].[a]`, each part with the case it was written with; a qualifier of more
/// parts is not written.
fn column_text(column: &ColumnRef) -> Option<String> {
    if let Some(qualifier) = &column.qualifier {
        if qualifier.server.is_some() || qualifier.database.is_some() || qualifier.schema.is_some()
        {
            return None;
        }
        return Some(format!(
            "{}.{}",
            bracketed(&qualifier.name.value),
            bracketed(&column.name.value)
        ));
    }
    if !column.name.quoted {
        let niladic = match column.name.value.to_ascii_uppercase().as_str() {
            "CURRENT_TIMESTAMP" => Some("getdate()"),
            "USER" | "CURRENT_USER" | "SESSION_USER" => Some("user_name()"),
            "SYSTEM_USER" => Some("suser_sname()"),
            _ => None,
        };
        if let Some(call) = niladic {
            return Some(call.to_owned());
        }
    }
    Some(bracketed(&column.name.value))
}

/// The numeric literal `expr` is once its signs are folded, with whether it is negative:
/// `negative` is the sign already met. `None` when `expr` is not a sign over a number.
fn signed_literal(expr: &Expr, negative: bool) -> Option<(&Literal, bool)> {
    match expr {
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => signed_literal(inner, negative),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr: inner,
            ..
        } => signed_literal(inner, !negative),
        Expr::Literal(
            literal @ (Literal::Integer(_)
            | Literal::Decimal(_)
            | Literal::Float(_)
            | Literal::Money(_)),
            _,
        ) => Some((literal, negative)),
        _ => None,
    }
}

/// The text of a literal, `negative` for a number under an odd count of `-`.
fn literal_text(literal: &Literal, negative: bool) -> Option<String> {
    match literal {
        Literal::Integer(digits) => integer_text(digits, negative),
        Literal::Decimal(text) => decimal_text(text, negative),
        Literal::Float(text) => float_text(text, negative),
        Literal::Money(text) => money_text(text, negative),
        _ if negative => None,
        Literal::Str { value, unicode } => Some(format!(
            "{}'{}'",
            if *unicode { "N" } else { "" },
            value.replace('\'', "''")
        )),
        Literal::Binary(hex) => Some(format!("0x{}", hex.to_ascii_uppercase())),
        Literal::Null => Some("NULL".to_owned()),
        Literal::Default => None,
    }
}

/// The digits of `digits` without their leading zeros, `0` for zero.
fn without_leading_zeros(digits: &str) -> &str {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

/// `(7)` for `007`, `(-2147483648)`, and `(2147483648.)` for an integer `int` does not hold,
/// which SQL Server reads as a `numeric`.
fn integer_text(digits: &str, negative: bool) -> Option<String> {
    let digits = without_leading_zeros(digits);
    if digits.len() > 38 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: u128 = digits.parse().ok()?;
    let sign = if negative && value != 0 { "-" } else { "" };
    let limit = if negative {
        2_147_483_648
    } else {
        2_147_483_647
    };
    let point = if value > limit { "." } else { "" };
    Some(format!("({sign}{digits}{point})"))
}

/// `(0.5)` for `.5` and for `00.5`, `(1.)` for `1.`, `(1.50)` for `1.50`.
fn decimal_text(text: &str, negative: bool) -> Option<String> {
    let (whole, fraction) = text.split_once('.')?;
    let sign = if negative { "-" } else { "" };
    Some(format!(
        "({sign}{}.{fraction})",
        without_leading_zeros(whole)
    ))
}

/// `(1.0000000000000000e+003)` for `1e3`: sixteen decimals, a signed exponent of three
/// digits.
fn float_text(text: &str, negative: bool) -> Option<String> {
    let value: f64 = text.parse().ok()?;
    let value = if negative { -value } else { value };
    let written = format!("{value:.16e}");
    let (mantissa, exponent) = written.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let sign = if exponent < 0 { '-' } else { '+' };
    Some(format!("({mantissa}e{sign}{:03})", exponent.unsigned_abs()))
}

/// `($5.0000)` for `$5`, `($-5.2500)` for `$-5.25`, `($1.2346)` for `$1.23456`: four
/// decimals, the fifth rounding away from zero.
fn money_text(text: &str, negative: bool) -> Option<String> {
    let (written_negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
        || whole.len() > 30
    {
        return None;
    }
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut kept = String::from(fraction);
    while kept.len() < 5 {
        kept.push('0');
    }
    let fourths: u128 = kept[..4].parse().ok()?;
    let round_up = kept.as_bytes()[4] >= b'5';
    let scaled = whole * 10_000 + fourths + u128::from(round_up);
    let sign = if (negative != written_negative) && scaled != 0 {
        "-"
    } else {
        ""
    };
    Some(format!(
        "(${sign}{}.{:04})",
        scaled / 10_000,
        scaled % 10_000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use ExpressionKind::{Predicate as P, Value as V};

    /// Expressions as a statement writes them, with the text SQL Server publishes in
    /// `definition` for each: a `DEFAULT` (`V`) or a `CHECK` (`P`) over the columns
    /// `a int, a2 int, b2 int, b varchar(20), n nvarchar(20), c datetime, d decimal(10,2),
    /// [my col] int, [a]]b] int`, some of them qualified by the name of their table.
    const NORMALISED: &[(ExpressionKind, &str, &str)] = &[
        (V, "0", "((0))"),
        (V, "(0)", "((0))"),
        (V, "((0))", "((0))"),
        (V, "-1", "((-1))"),
        (V, "(-1)", "((-1))"),
        (V, "- 1", "((-1))"),
        (V, "+1", "((1))"),
        (V, "-(1)", "((-1))"),
        (V, "1.5", "((1.5))"),
        (V, "-1.5", "((-1.5))"),
        (V, ".5", "((0.5))"),
        (V, "1.", "((1.))"),
        (V, "1e3", "((1.0000000000000000e+003))"),
        (V, "1.5E-3", "((1.5000000000000000e-003))"),
        (V, "$5", "(($5.0000))"),
        (V, "$-5.25", "(($-5.2500))"),
        (V, "'x'", "('x')"),
        (V, "''", "('')"),
        (V, "'it''s'", "('it''s')"),
        (V, "N'x'", "(N'x')"),
        (V, "NULL", "(NULL)"),
        (V, "null", "(NULL)"),
        (V, "getdate()", "(getdate())"),
        (V, "GETDATE()", "(getdate())"),
        (V, "GetDate ( )", "(getdate())"),
        (V, "CURRENT_TIMESTAMP", "(getdate())"),
        (V, "sysdatetime()", "(sysdatetime())"),
        (V, "getutcdate()", "(getutcdate())"),
        (V, "newid()", "(newid())"),
        (V, "NEWSEQUENTIALID()", "(newsequentialid())"),
        (V, "0x01", "(0x01)"),
        (V, "0xAbCd", "(0xABCD)"),
        (V, "0x", "(0x)"),
        (V, "1+1", "((1)+(1))"),
        (V, "1 + 2 * 3", "((1)+(2)*(3))"),
        (V, "(1+2)*3", "(((1)+(2))*(3))"),
        (V, "1-(-2)", "((1)-(-2))"),
        (V, "'a'+'b'", "('a'+'b')"),
        (V, "CONVERT(int,1)", "(CONVERT([int],(1)))"),
        (V, "CAST(1 AS int)", "(CONVERT([int],(1)))"),
        (V, "cast('1' as int)", "(CONVERT([int],'1'))"),
        (
            V,
            "CONVERT(varchar(10), getdate(), 103)",
            "(CONVERT([varchar](10),getdate(),(103)))",
        ),
        (V, "user_name()", "(user_name())"),
        (V, "SUSER_SNAME()", "(suser_sname())"),
        (V, "CURRENT_USER", "(user_name())"),
        (V, "SYSTEM_USER", "(suser_sname())"),
        (V, "SESSION_USER", "(user_name())"),
        (V, "USER", "(user_name())"),
        (V, "2147483648", "((2147483648.))"),
        (V, "9223372036854775807", "((9223372036854775807.))"),
        (V, "ABS(-5)", "(abs((-5)))"),
        (V, "UPPER('x')", "(upper('x'))"),
        (V, "DATEPART(year, getdate())", "(datepart(year,getdate()))"),
        (
            V,
            "DATEADD(day, 1, getdate())",
            "(dateadd(day,(1),getdate()))",
        ),
        (
            V,
            "CASE WHEN 1=1 THEN 1 ELSE 0 END",
            "(case when (1)=(1) then (1) else (0) end)",
        ),
        (V, "ISNULL(NULL,1)", "(isnull(NULL,(1)))"),
        (V, "COALESCE(NULL,2)", "(coalesce(NULL,(2)))"),
        (V, "@@SERVERNAME", "(@@servername)"),
        (V, "@@SPID", "(@@spid)"),
        (V, "1", "((1))"),
        (V, "'true'", "('true')"),
        (V, "~1", "(~(1))"),
        (V, "5 % 2", "((5)%(2))"),
        (V, "6/3", "((6)/(3))"),
        (V, "1&3", "((1)&(3))"),
        (V, "1|2", "((1)|(2))"),
        (V, "1^3", "((1)^(3))"),
        (
            V,
            "'x' COLLATE Latin1_General_CS_AS",
            "(('x') collate Latin1_General_CS_AS)",
        ),
        (V, "'2020-01-01'", "('2020-01-01')"),
        (V, "0.0", "((0.0))"),
        (V, "CONCAT('a', 'b', 1)", "(concat('a','b',(1)))"),
        (V, "NEXT VALUE FOR dbo.seq", "(NEXT VALUE FOR [dbo].[seq])"),
        (P, "a>0", "([a]>(0))"),
        (P, "a >= 0", "([a]>=(0))"),
        (P, "a<>0", "([a]<>(0))"),
        (P, "a!=0", "([a]<>(0))"),
        (P, "a !< 0", "([a]>=(0))"),
        (P, "a !> 0", "([a]<=(0))"),
        (P, "a>0 AND b<'z'", "([a]>(0) AND [b]<'z')"),
        (P, "a>0 OR a<-10", "([a]>(0) OR [a]<(-10))"),
        (P, "NOT a>0", "(NOT [a]>(0))"),
        (P, "NOT (a>0)", "(NOT [a]>(0))"),
        (P, "a IS NULL", "([a] IS NULL)"),
        (P, "a IS NOT NULL", "([a] IS NOT NULL)"),
        (P, "a BETWEEN 1 AND 5", "([a]>=(1) AND [a]<=(5))"),
        (P, "a NOT BETWEEN 1 AND 5", "(NOT ([a]>=(1) AND [a]<=(5)))"),
        (P, "a IN (1,2,3)", "([a]=(3) OR [a]=(2) OR [a]=(1))"),
        (P, "a NOT IN (1, 2)", "(NOT ([a]=(2) OR [a]=(1)))"),
        (P, "b LIKE 'x%'", "([b] like 'x%')"),
        (P, "b NOT LIKE 'x%'", "(NOT [b] like 'x%')"),
        (P, "b LIKE 'x!%' ESCAPE '!'", "([b] like 'x!%' escape '!' )"),
        (P, "len(b)>0", "(len([b])>(0))"),
        (P, "LEN(b) > 0", "(len([b])>(0))"),
        (P, "a+1>0", "(([a]+(1))>(0))"),
        (P, "a*2>-1", "([a]*(2)>(-1))"),
        (P, "(a>0)", "([a]>(0))"),
        (P, "((a>0))", "([a]>(0))"),
        (
            P,
            "a > 0 AND (b < 'm' OR a = 5)",
            "([a]>(0) AND ([b]<'m' OR [a]=(5)))",
        ),
        (
            P,
            "(a > 0 AND b < 'm') OR a = 5",
            "([a]>(0) AND [b]<'m' OR [a]=(5))",
        ),
        (
            P,
            "a > 0 AND b < 'm' OR a = 5",
            "([a]>(0) AND [b]<'m' OR [a]=(5))",
        ),
        (
            P,
            "a > 0 OR b < 'm' AND a = 5",
            "([a]>(0) OR [b]<'m' AND [a]=(5))",
        ),
        (
            P,
            "CASE WHEN a>0 THEN 1 ELSE 0 END=1",
            "(case when [a]>(0) then (1) else (0) end=(1))",
        ),
        (
            P,
            "CASE a WHEN 1 THEN 'x' END = 'x'",
            "(case [a] when (1) then 'x'  end='x')",
        ),
        (P, "a > -1", "([a]>(-1))"),
        (P, "b <> ''", "([b]<>'')"),
        (P, "n = N'x'", "([n]=N'x')"),
        (P, "d=1.5", "([d]=(1.5))"),
        (P, "CAST(a AS bigint)>0", "(CONVERT([bigint],[a])>(0))"),
        (
            P,
            "CONVERT(varchar(10),a)<>''",
            "(CONVERT([varchar](10),[a])<>'')",
        ),
        (P, "ISNULL(a,0)>0", "(isnull([a],(0))>(0))"),
        (P, "COALESCE(a,0) > 0", "(coalesce([a],(0))>(0))"),
        (P, "a % 2 = 0", "([a]%(2)=(0))"),
        (P, "a - a2 > 0", "(([a]-[a2])>(0))"),
        (P, "a/2>0", "([a]/(2)>(0))"),
        (P, "-a<0", "(( -[a])<(0))"),
        (P, "~a<>0", "(~[a]<>(0))"),
        (P, "a&1=0", "(([a]&(1))=(0))"),
        (P, "upper(b)='X'", "(upper([b])='X')"),
        (
            P,
            "b COLLATE Latin1_General_CS_AS = 'x'",
            "(([b]) collate Latin1_General_CS_AS='x')",
        ),
        (P, "[a]>0", "([a]>(0))"),
        (P, "[my col] > 0", "([my col]>(0))"),
        (P, "a2 > a", "([a2]>[a])"),
        (P, "c < getdate()", "([c]<getdate())"),
        (P, "c>'2020-01-01'", "([c]>'2020-01-01')"),
        (P, "a = 2147483648", "([a]=(2147483648.))"),
        (P, "a=+1", "([a]=(1))"),
        (P, "a = -(1)", "([a]=(-1))"),
        (P, "a = 1 + 2 * 3", "([a]=((1)+(2)*(3)))"),
        (P, "a = (1 + 2) * 3", "([a]=((1)+(2))*(3))"),
        (P, "a*(a2+1)>0", "([a]*([a2]+(1))>(0))"),
        (P, "a-(a2-1)>0", "(([a]-([a2]-(1)))>(0))"),
        (P, "a-a2-1>0", "((([a]-[a2])-(1))>(0))"),
        (
            P,
            "a > 0 AND a < 10 AND a <> 5",
            "([a]>(0) AND [a]<(10) AND [a]<>(5))",
        ),
        (
            P,
            "a = 1 OR a = 2 OR a = 3",
            "([a]=(1) OR [a]=(2) OR [a]=(3))",
        ),
        (P, "NOT a IS NULL", "(NOT [a] IS NULL)"),
        (
            P,
            "NOT (a IS NULL OR b IS NULL)",
            "(NOT ([a] IS NULL OR [b] IS NULL))",
        ),
        (P, "a IN (1)", "([a]=(1))"),
        (P, "b IN ('a','b')", "([b]='b' OR [b]='a')"),
        (P, "a=0x01", "([a]=0x01)"),
        (P, "b = 'it''s'", "([b]='it''s')"),
        (P, "DATALENGTH(b) < 10", "(datalength([b])<(10))"),
        (P, "b > N'a'", "([b]>N'a')"),
        (P, "c = CURRENT_TIMESTAMP", "([c]=getdate())"),
        (P, "a = @@SPID", "([a]=@@spid)"),
        (V, "007", "((7))"),
        (V, "0.50", "((0.50))"),
        (V, "1.50e0", "((1.5000000000000000e+000))"),
        (V, "-2147483648", "((-2147483648))"),
        (V, "-0", "((0))"),
        (V, "'a' + 'b' + 'c'", "(('a'+'b')+'c')"),
        (V, "1 + 2 + 3", "(((1)+(2))+(3))"),
        (V, "1 - 2 - 3", "(((1)-(2))-(3))"),
        (V, "1 - (2 - 3)", "((1)-((2)-(3)))"),
        (V, "2 * 3 * 4", "(((2)*(3))*(4))"),
        (V, "2 * (3 * 4)", "((2)*((3)*(4)))"),
        (V, "12 / 2 / 3", "(((12)/(2))/(3))"),
        (V, "-(-1)", "((1))"),
        (V, "-(1+1)", "( -((1)+(1)))"),
        (V, "+(1)", "((1))"),
        (V, "- -1", "((1))"),
        (V, "1.50", "((1.50))"),
        (V, "00.5", "((0.5))"),
        (V, "0x0a", "(0x0A)"),
        (V, "N''", "(N'')"),
        (V, "CAST(NULL AS int)", "(CONVERT([int],NULL))"),
        (
            V,
            "CONVERT(datetime, '2020-01-01')",
            "(CONVERT([datetime],'2020-01-01'))",
        ),
        (
            V,
            "CONVERT(nvarchar(max), N'x')",
            "(CONVERT([nvarchar](max),N'x'))",
        ),
        (
            V,
            "CAST(1 AS decimal(10, 2))",
            "(CONVERT([decimal](10,2),(1)))",
        ),
        (V, "CAST(0 AS bit)", "(CONVERT([bit],(0)))"),
        (V, "CAST(1 AS float)", "(CONVERT([float],(1)))"),
        (V, "CAST('x' AS varchar)", "(CONVERT([varchar],'x'))"),
        (V, "SYSUTCDATETIME()", "(sysutcdatetime())"),
        (V, "SYSDATETIMEOFFSET()", "(sysdatetimeoffset())"),
        (V, "HOST_NAME()", "(host_name())"),
        (V, "APP_NAME()", "(app_name())"),
        (V, "DB_NAME()", "(db_name())"),
        (V, "@@VERSION", "(@@version)"),
        (V, "1e0", "((1.0000000000000000e+000))"),
        (V, "2.5E+10", "((2.5000000000000000e+010))"),
        (V, "$0", "(($0.0000))"),
        (V, "$1.23456", "(($1.2346))"),
        (V, "ISNULL(NULL, 'x')", "(isnull(NULL,'x'))"),
        (V, "COALESCE(NULL, NULL, 3)", "(coalesce(NULL,NULL,(3)))"),
        (V, "NULLIF(1, 0)", "(nullif((1),(0)))"),
        (V, "LEFT('abc', 1)", "(left('abc',(1)))"),
        (V, "(1)*(-1)", "((1)*(-1))"),
        (V, "~~1", "(~(~(1)))"),
        (V, "-~1", "( -(~(1)))"),
        (V, "~-1", "(~(-1))"),
        (V, "-1e3", "((-1.0000000000000000e+003))"),
        (V, "-$5", "(($-5.0000))"),
        (V, "PI()", "(pi())"),
        (V, "ROUND(1.5, 0)", "(round((1.5),(0)))"),
        (V, "SPACE(1)", "(space((1)))"),
        (V, "1 + -1", "((1)+(-1))"),
        (V, "1 * -1", "((1)*(-1))"),
        (V, "(1)", "((1))"),
        (V, "REPLICATE('a', 3)", "(replicate('a',(3)))"),
        (
            V,
            "DATEADD(dd, -1, GETDATE())",
            "(dateadd(day,(-1),getdate()))",
        ),
        (
            V,
            "DATEDIFF(day, '2020-01-01', getdate())",
            "(datediff(day,'2020-01-01',getdate()))",
        ),
        (V, "YEAR(getdate())", "(datepart(year,getdate()))"),
        (V, "FORMAT(1, 'N')", "(format((1),'N'))"),
        (V, "TRY_CAST('1' AS int)", "(TRY_CAST('1' AS [int]))"),
        (V, "TRY_CONVERT(int, '1')", "(TRY_CAST('1' AS [int]))"),
        (V, "CHECKSUM(1)", "(checksum((1)))"),
        (V, "1", "((1))"),
        (V, "1.5e-10", "((1.5000000000000000e-010))"),
        (V, "123456789012345678.0e5", "((1.2345678901234568e+022))"),
        (V, "1.5e0", "((1.5000000000000000e+000))"),
        (V, "12345678901234567890123", "((12345678901234567890123.))"),
        (
            V,
            "CASE 1 WHEN 1 THEN 2 WHEN 3 THEN 4 ELSE 5 END",
            "(case (1) when (1) then (2) when (3) then (4) else (5) end)",
        ),
        (
            V,
            "'a' + CAST(1 AS varchar(10))",
            "('a'+CONVERT([varchar](10),(1)))",
        ),
        (V, "1 + (2)", "((1)+(2))"),
        (V, "(1 + 2)", "((1)+(2))"),
        (V, "((1 + 2))", "((1)+(2))"),
        (P, "a*a2*b2>0", "(([a]*[a2])*[b2]>(0))"),
        (P, "a*(a2*b2)>0", "([a]*([a2]*[b2])>(0))"),
        (P, "(a*2)>0", "([a]*(2)>(0))"),
        (P, "a+(1+2)>0", "(([a]+((1)+(2)))>(0))"),
        (P, "a+1+2>0", "((([a]+(1))+(2))>(0))"),
        (
            P,
            "a=1 OR (a=2 OR a=3)",
            "([a]=(1) OR ([a]=(2) OR [a]=(3)))",
        ),
        (P, "(a=1 OR a=2) OR a=3", "([a]=(1) OR [a]=(2) OR [a]=(3))"),
        (
            P,
            "a=1 AND (a2=2 AND b='x')",
            "([a]=(1) AND ([a2]=(2) AND [b]='x'))",
        ),
        (
            P,
            "(a=1 AND a2=2) AND b='x'",
            "([a]=(1) AND [a2]=(2) AND [b]='x')",
        ),
        (P, "-(a+1)<0", "(( -([a]+(1)))<(0))"),
        (P, "- -a<0", "(( -( -[a]))<(0))"),
        (P, "NOT NOT a>0", "(NOT NOT [a]>(0))"),
        (P, "~(a+1)<>0", "(~([a]+(1))<>(0))"),
        (P, "a IN (a2, 1+1)", "([a]=((1)+(1)) OR [a]=[a2])"),
        (P, "[a]]b] > 0", "([a]]b]>(0))"),
        (P, "b LIKE N'x%'", "([b] like N'x%')"),
        (P, "b LIKE 'x' + '%'", "([b] like 'x'+'%')"),
        (P, "CONVERT(varchar, a)<>''", "(CONVERT([varchar],[a])<>'')"),
        (
            P,
            "CONVERT(varchar(max), a)<>''",
            "(CONVERT([varchar](max),[a])<>'')",
        ),
        (
            P,
            "CAST(a AS decimal(10,2))>0",
            "(CONVERT([decimal](10,2),[a])>(0))",
        ),
        (
            P,
            "CAST(a AS nvarchar(max))<>N''",
            "(CONVERT([nvarchar](max),[a])<>N'')",
        ),
        (P, "TRY_CAST(a AS int) > 0", "(TRY_CAST([a] AS [int])>(0))"),
        (P, "COALESCE(a, a2, 0)>0", "(coalesce([a],[a2],(0))>(0))"),
        (P, "NULLIF(a,0) > 0", "(nullif([a],(0))>(0))"),
        (P, "a = NULL", "([a]=NULL)"),
        (P, "b + 'x' = 'yx'", "(([b]+'x')='yx')"),
        (
            P,
            "a IS NULL AND b IS NULL",
            "([a] IS NULL AND [b] IS NULL)",
        ),
        (P, "LEFT(b,1)='x'", "(left([b],(1))='x')"),
        (P, "DATEADD(dd, 1, c) > c", "(dateadd(day,(1),[c])>[c])"),
        (
            P,
            "DATEDIFF(day, c, getdate()) < 5",
            "(datediff(day,[c],getdate())<(5))",
        ),
        (P, "YEAR(c) > 2000", "(datepart(year,[c])>(2000))"),
        (P, "CHARINDEX('x', b) > 0", "(charindex('x',[b])>(0))"),
        (P, "SUBSTRING(b,1,2)='ab'", "(substring([b],(1),(2))='ab')"),
        (P, "LTRIM(RTRIM(b)) <> ''", "(ltrim(rtrim([b]))<>'')"),
        (P, "ROUND(d, 1) > 0", "(round([d],(1))>(0))"),
        (
            P,
            "CASE WHEN a>0 THEN 'x' WHEN a<0 THEN 'y' END='x'",
            "(case when [a]>(0) then 'x' when [a]<(0) then 'y'  end='x')",
        ),
        (
            P,
            "CASE WHEN a > 0 THEN 1 END = 1",
            "(case when [a]>(0) then (1)  end=(1))",
        ),
        (P, "a > 0 AND NOT b = 'x'", "([a]>(0) AND NOT [b]='x')"),
        (P, "NOT (a > 0) AND b = 'x'", "(NOT [a]>(0) AND [b]='x')"),
        (
            P,
            "NOT (a > 0 AND b = 'x') OR a2 = 1",
            "(NOT ([a]>(0) AND [b]='x') OR [a2]=(1))",
        ),
        (
            P,
            "a2 BETWEEN a AND a + 10",
            "([a2]>=[a] AND [a2]<=([a]+(10)))",
        ),
        (
            P,
            "b NOT LIKE 'x%' ESCAPE '!'",
            "(NOT [b] like 'x%' escape '!' )",
        ),
        (
            P,
            "a = 1 AND a2 = 2 OR b = 'x' AND n = N'y'",
            "([a]=(1) AND [a2]=(2) OR [b]='x' AND [n]=N'y')",
        ),
        (
            P,
            "(a = 1 OR a2 = 2) AND (b = 'x' OR n = N'y')",
            "(([a]=(1) OR [a2]=(2)) AND ([b]='x' OR [n]=N'y'))",
        ),
        (P, "a+a2*3>0", "(([a]+[a2]*(3))>(0))"),
        (P, "(a+a2)*3>0", "(([a]+[a2])*(3)>(0))"),
        (P, "a*3+a2>0", "(([a]*(3)+[a2])>(0))"),
        (P, "a-a2+1>0", "((([a]-[a2])+(1))>(0))"),
        (P, "a/(a2*2)>0", "([a]/([a2]*(2))>(0))"),
        (P, "a%a2%2=0", "(([a]%[a2])%(2)=(0))"),
        (P, "a&a2|1>0", "((([a]&[a2])|(1))>(0))"),
        (P, "a|a2&1>0", "((([a]|[a2])&(1))>(0))"),
        (P, "a^1>0", "(([a]^(1))>(0))"),
        (P, "a=a2", "([a]=[a2])"),
        (P, "1=1", "((1)=(1))"),
        (P, "a2 < a OR a2 IS NULL", "([a2]<[a] OR [a2] IS NULL)"),
        (P, "b IN ('x')", "([b]='x')"),
        (P, "b NOT IN ('x', 'y')", "(NOT ([b]='y' OR [b]='x'))"),
        (P, "a NOT IN (1)", "(NOT [a]=(1))"),
        (P, "a IN (1, NULL)", "([a]=NULL OR [a]=(1))"),
        (
            P,
            "c BETWEEN '2020-01-01' AND '2021-01-01'",
            "([c]>='2020-01-01' AND [c]<='2021-01-01')",
        ),
        (P, "d > -0.5", "([d]>(-0.5))"),
        (P, "d >= 0.0", "([d]>=(0.0))"),
        (P, "a > 1.5e0", "([a]>(1.5000000000000000e+000))"),
        (P, "d < $5", "([d]<($5.0000))"),
        (P, "NOT a IN (1,2)", "(NOT ([a]=(2) OR [a]=(1)))"),
        (P, "NOT a BETWEEN 1 AND 2", "(NOT ([a]>=(1) AND [a]<=(2)))"),
        (P, "NOT b LIKE 'x'", "(NOT [b] like 'x')"),
        (P, "(b) LIKE 'x'", "([b] like 'x')"),
        (P, "b LIKE ('x')", "([b] like 'x')"),
        (P, "(a) IS NULL", "([a] IS NULL)"),
        (P, "a + 1 IS NULL", "(([a]+(1)) IS NULL)"),
        (P, "ISNULL(b,'') <> ''", "(isnull([b],'')<>'')"),
        (P, "(((a)))>(((0)))", "([a]>(0))"),
        (P, "a <= 0x0A", "([a]<=0x0A)"),
        (
            P,
            "len(b) between 1 and 5",
            "(len([b])>=(1) AND len([b])<=(5))",
        ),
        (
            P,
            "b > 'a' COLLATE Latin1_General_CS_AS",
            "([b]>('a') collate Latin1_General_CS_AS)",
        ),
        (P, "abs(-a)>0", "(abs( -[a])>(0))"),
        (
            P,
            "CONVERT(varchar(10),-a)<>''",
            "(CONVERT([varchar](10), -[a])<>'')",
        ),
        (P, "CAST(-a AS bigint)>0", "(CONVERT([bigint], -[a])>(0))"),
        (
            P,
            "CASE WHEN a>0 THEN -a ELSE -a2 END>0",
            "(case when [a]>(0) then  -[a] else  -[a2] end>(0))",
        ),
        (
            P,
            "CASE -a WHEN 1 THEN 1 END=1",
            "(case  -[a] when (1) then (1)  end=(1))",
        ),
        (
            P,
            "CASE WHEN a>0 THEN -(a+1) END>0",
            "(case when [a]>(0) then  -([a]+(1))  end>(0))",
        ),
        (P, "coalesce(-a,0)>0", "(coalesce( -[a],(0))>(0))"),
        (P, "-a IN (1,2)", "(( -[a])=(2) OR ( -[a])=(1))"),
        (P, "-a IS NULL", "(( -[a]) IS NULL)"),
        (P, "-a BETWEEN 1 AND 2", "(( -[a])>=(1) AND ( -[a])<=(2))"),
        (
            P,
            "CASE WHEN -a>0 THEN 1 END=1",
            "(case when ( -[a])>(0) then (1)  end=(1))",
        ),
        (P, "abs(-(-a))>0", "(abs( -( -[a]))>(0))"),
        (P, "abs(-~a)>0", "(abs( -(~[a]))>(0))"),
        (P, "isnull(-a, -1)>0", "(isnull( -[a],(-1))>(0))"),
        (P, "abs(~a)>0", "(abs(~[a])>(0))"),
        (
            P,
            "CASE WHEN a>0 THEN ~a END>0",
            "(case when [a]>(0) then ~[a]  end>(0))",
        ),
        (P, "q1.a > 0", "([q1].[a]>(0))"),
        (P, "[q3].[my col] > 0", "([q3].[my col]>(0))"),
        (
            P,
            "q4.a + q4.a2 > 0 AND abs(q4.a) IN (1, 2)",
            "(([q4].[a]+[q4].[a2])>(0) AND (abs([q4].[a])=(2) OR abs([q4].[a])=(1)))",
        ),
        (P, "[q]]5].a > 0", "([q]]5].[a]>(0))"),
        (P, "Q6.A > 0", "([Q6].[A]>(0))"),
    ];

    /// The tree of `text` as a statement writes it.
    fn written(kind: ExpressionKind, text: &str) -> Expr {
        parse_stored_expression(text, kind)
            .unwrap_or_else(|error| panic!("{text} does not parse: {error:?}"))
    }

    #[test]
    fn the_definition_text_is_normalised() {
        let mut wrong = Vec::new();
        for (kind, input, expected) in NORMALISED {
            let text = normalized_text(&written(*kind, input));
            if text.as_deref() != Some(*expected) {
                wrong.push(format!("{input}: {text:?}, expected {expected}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {}:\n{}",
            wrong.len(),
            NORMALISED.len(),
            wrong.join("\n")
        );
        assert_eq!(NORMALISED.len(), 313);
    }

    #[test]
    fn the_text_of_a_stored_tree_is_the_stored_text() {
        for (kind, input, expected) in NORMALISED {
            let (text, tree) = stored_expression(&written(*kind, input), *kind).expect(input);
            assert_eq!(text, *expected, "{input}");
            assert_eq!(definition_text(&tree), text, "{input}");
        }
    }

    #[test]
    fn a_tree_rebuilt_from_its_text_is_the_stored_tree() {
        for (kind, input, _) in NORMALISED {
            let stored = stored_expression(&written(*kind, input), *kind).expect(input);
            let rebuilt = parse_stored_expression(&stored.0, *kind).expect(input);
            assert_eq!(rebuilt, stored.1, "{input}");
            assert_eq!(
                stored_expression(&rebuilt, *kind).expect(input),
                stored,
                "{input}"
            );
        }
    }

    #[test]
    fn a_shape_left_unnormalised_keeps_the_text_display_writes() {
        // A column qualified by a schema and a subquery are not normalised: the text is
        // `Display` between one pair of parentheses, and reads back to the same text.
        for (kind, input) in [
            (P, "dbo.t.a > 0"),
            (P, "(dbo.t.a > 0)"),
            (V, "(SELECT 1)"),
            (P, "a IN (SELECT 1)"),
        ] {
            let tree = written(kind, input);
            assert_eq!(normalized_text(&tree), None, "{input}");
            let (text, stored) = stored_expression(&tree, kind).expect(input);
            assert_eq!(text, display_text(&tree), "{input}");
            assert_eq!(definition_text(&stored), text, "{input}");
            assert_eq!(stored_expression(&stored, kind).expect(input).0, text);
        }
    }

    #[test]
    fn a_bracketed_type_name_reads_back_bare() {
        let tree = parse_stored_expression("(CONVERT([varchar](10),[a])<>'')", P).expect("parse");
        let Expr::Nested(inner, _) = &tree else {
            panic!("{tree:?}");
        };
        let Expr::Binary { left, .. } = inner.as_ref() else {
            panic!("{inner:?}");
        };
        let Expr::Convert { ty, .. } = left.as_ref() else {
            panic!("{left:?}");
        };
        assert_eq!(ty.name, "varchar");
        assert_eq!(ty.args, vec![TypeArg::Number(10)]);
    }

    #[test]
    fn the_expression_texts_are_the_published_texts() {
        let (_, zero) = stored_expression(&written(V, "0"), V).expect("0");
        let (definition, check) = stored_expression(&written(P, "a >= 0"), P).expect("a >= 0");
        let column = ColumnMeta {
            id: ColumnId(1),
            name: "a".to_owned(),
            ty: vauban_types::TypeInfo::new(vauban_types::SqlType::Int, true),
            ordinal: 0,
            default: Some(zero.clone()),
            identity: None,
            computed: Some(written(V, "a + 1")),
        };
        assert_eq!(column.default_text().as_deref(), Some("((0))"));
        assert_eq!(column.computed_text().as_deref(), Some("([a]+(1))"));
        let default = ConstraintMeta::Default {
            constraint: ObjectId(7),
            system_named: true,
            column: ColumnId(1),
            expr: zero,
        };
        assert_eq!(default.expression_text().as_deref(), Some("((0))"));
        let check = ConstraintMeta::Check {
            constraint: ObjectId(8),
            system_named: false,
            expr: check,
            definition,
        };
        assert_eq!(check.expression_text().as_deref(), Some("([a]>=(0))"));
        assert_eq!(
            ConstraintMeta::PrimaryKey(IndexId(1)).expression_text(),
            None
        );
    }

    #[test]
    fn bare_identity_is_seed_one_increment_one() {
        assert_eq!(
            IdentitySpec::default(),
            IdentitySpec {
                seed: 1,
                increment: 1
            }
        );
    }
}
