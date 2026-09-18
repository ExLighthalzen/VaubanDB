//! What the binder needs to know besides the AST: the batch text, the catalogue, the
//! current database and schema, the variables in scope and the `SET` options.

use vauban_catalog::{ColumnId, ObjectId};
use vauban_types::TypeInfo;

use crate::bound::ColumnBinding;

/// Everything the binder consults besides the statement it binds.
///
/// Borrowed, not owned: one context is built per batch by the caller (`session` in
/// production, [`BindContext::scalar`] in the tests) and lives as long as the binding.
pub struct BindContext<'a> {
    /// Text of the batch, to slice the `near '…'` of messages 102 and 4145 out of a
    /// `parser::Span`.
    pub text: &'a str,
    /// Optional catalogue; `None` binds scalar expressions without a `FROM`.
    pub catalog: Option<&'a dyn CatalogView>,
    /// Current database (`DB_NAME()`), for three- and four-part names.
    pub database: &'a str,
    /// Default schema of the user, `dbo`.
    pub default_schema: &'a str,
    /// Batch variables in scope; `&NoVariables` when there are none.
    pub variables: &'a dyn VariableScope,
    /// `SET` options that change typing and evaluation.
    pub options: SessionOptions,
}

/// The single shared value of [`NoVariables`], so that [`BindContext::scalar`] can hand
/// out a reference that outlives the context it builds.
static NO_VARIABLES: NoVariables = NoVariables;

impl<'a> BindContext<'a> {
    /// A minimal context: no catalogue, no variable, default database and schema.
    ///
    /// `database` is `master` and `default_schema` is `dbo`, the pair a client connecting
    /// without a database name gets. `session` builds production contexts from the
    /// connection state instead.
    #[must_use]
    pub fn scalar(text: &'a str, options: SessionOptions) -> Self {
        Self {
            text,
            catalog: None,
            database: "master",
            default_schema: "dbo",
            variables: &NO_VARIABLES,
            options,
        }
    }
}

/// The view of the catalogue the binder needs.
///
/// Each method has a default that answers as an empty catalogue would — `None`, and
/// `Unknown` for the classification — so that a caller which knows one thing (the fake views
/// of the tests) implements that one thing. `catalog_view.rs` implements the
/// trait for a [`CatalogSnapshot`](vauban_catalog::CatalogSnapshot), which is what `session`
/// hands over in production.
pub trait CatalogView {
    /// Resolves a one- to four-part table or view name against the current database and the
    /// default schema of the session, or `None` when it names nothing resolvable.
    ///
    /// `database` is used when the name has no database part, `default_schema` when it has
    /// no schema part. Used to build a [`LogicalPlan::Scan`](crate::LogicalPlan) and to
    /// raise error 208 when the answer is `None`.
    fn resolve_table(
        &self,
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> Option<ResolvedTable> {
        let _ = (name, database, default_schema);
        None
    }

    /// The T-SQL text of the view of identifier `object`, `None` when `object` is not a
    /// view. `view.rs` parses that text to expand the view into the plan.
    fn view_definition(&self, object: ObjectId) -> Option<&str> {
        let _ = object;
        None
    }

    /// The `IDENTITY` column of the table of identifier `object`, `None` for a table
    /// declared without one and when `object` is not a table.
    ///
    /// An `INSERT` leaves that column out of the list it builds when none was written, and
    /// refuses a value written for it unless `SET IDENTITY_INSERT` is open for the table
    /// (`insert.rs`).
    fn identity_column(&self, object: ObjectId) -> Option<ColumnId> {
        let _ = object;
        None
    }

    /// The computed columns of the table of identifier `object`, empty when it declares
    /// none or when `object` is not a table. An `INSERT` cannot fill them (`insert.rs`).
    fn computed_columns(&self, object: ObjectId) -> Vec<ColumnId> {
        let _ = object;
        Vec::new()
    }

    /// Whether another table holds a `FOREIGN KEY` that references `object`.
    fn is_referenced_by_foreign_key(&self, object: ObjectId) -> bool {
        let _ = object;
        false
    }

    /// Name of a non-key index on `column` of `table`, when the catalogue knows one
    /// (`alter.rs`).
    fn index_on_column(&self, table: ObjectId, column: &str) -> Option<String> {
        let _ = (table, column);
        None
    }

    /// Names of constraints on `table` (`alter.rs`).
    fn constraint_names_on_table(&self, table: ObjectId) -> Vec<String> {
        let _ = table;
        Vec::new()
    }

    /// Whether an object named `name` already exists in the current database (`alter.rs`).
    fn object_name_taken(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// Key columns of the `PRIMARY KEY` of `table`, when one is declared (`alter.rs`).
    fn primary_key_columns(&self, table: ObjectId) -> Option<Vec<String>> {
        let _ = table;
        None
    }

    /// Name of the `PRIMARY KEY` constraint on `table`, when one is declared (`alter.rs`).
    fn primary_key_constraint_name(&self, table: ObjectId) -> Option<String> {
        let _ = table;
        None
    }

    /// Whether the named `FOREIGN KEY` on `table` references `column` (`alter.rs`).
    fn foreign_key_constraint_on_column(
        &self,
        table: ObjectId,
        constraint: &str,
        column: &str,
    ) -> bool {
        let _ = (table, constraint, column);
        false
    }

    /// Whether the named `UNIQUE` constraint on `table` includes `column` (`alter.rs`).
    fn unique_constraint_on_column(&self, table: ObjectId, constraint: &str, column: &str) -> bool {
        let _ = (table, constraint, column);
        false
    }

    /// Whether `table` declares a `UNIQUE` key exactly on `columns`, in order (`alter.rs`).
    fn unique_key_on(&self, table: ObjectId, columns: &[String]) -> bool {
        let _ = (table, columns);
        false
    }

    /// Whether the named `CHECK` on `table` mentions `column` (`alter.rs`).
    fn check_constraint_mentions_column(
        &self,
        table: ObjectId,
        constraint: &str,
        column: &str,
    ) -> bool {
        let _ = (table, constraint, column);
        false
    }

    /// Whether the named `DEFAULT` on `table` applies to `column` (`alter.rs`).
    fn default_constraint_on_column(
        &self,
        table: ObjectId,
        constraint: &str,
        column: &str,
    ) -> bool {
        let _ = (table, constraint, column);
        false
    }

    /// Classifies the written name in the current database and default schema.
    /// `Unknown` means no classification is available, not that the object is absent.
    ///
    /// The default answers from [`CatalogView::resolve_table`]: a name that resolves to a
    /// table or to a view is a [`TableReferenceKind::Table`], and a name that resolves to
    /// nothing keeps `Unknown` (unit tests `classify_known_table_is_table` and
    /// `classify_unknown_table_stays_unknown`). A view that knows table-valued functions
    /// overrides the method.
    fn classify_table_reference(
        &self,
        name: &vauban_parser::ObjectName,
        database: &str,
        default_schema: &str,
    ) -> TableReferenceKind {
        match self.resolve_table(name, database, default_schema) {
            Some(_) => TableReferenceKind::Table,
            None => TableReferenceKind::Unknown,
        }
    }
}

/// What [`CatalogView::resolve_table`] found behind a written name.
#[derive(Debug, Clone)]
pub struct ResolvedTable {
    /// Identifier of the object, as `sys.objects.object_id` publishes it. The argument of
    /// [`CatalogView::view_definition`].
    pub object: ObjectId,
    /// Identifier of the rows in `storage`, for a [`ResolvedTableKind::Table`]; `None` for
    /// a [`ResolvedTableKind::View`], which stores no row of its own and which `view.rs`
    /// expands into the plan of its definition.
    pub table: Option<vauban_catalog::TableId>,
    /// The columns, in the order of the row `storage` hands out.
    ///
    /// `columns[i].index` is **not** `i`: it is the `ordinal` of the column in that row,
    /// which the drop of a column before it does not close up (`catalog_view.rs`,
    /// `columns_are_indexed_by_ordinal_not_by_identifier`; `star.rs`,
    /// `the_expanded_columns_index_the_row_of_storage`).
    pub columns: Vec<ColumnBinding>,
    /// Whether the name reached a table or a view.
    pub kind: ResolvedTableKind,
}

/// What a resolved name turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedTableKind {
    /// A table, with its own rows in `storage`.
    Table,
    /// A view, whose rows come from the query of its definition.
    View,
}

/// The classification needed before interpreting arguments attached to a table name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableReferenceKind {
    /// A table or view whose arguments may spell a table hint.
    Table,
    /// A table-valued function whose arguments are expressions, not hints.
    TableValuedFunction,
    /// No classification is available; the arguments are read as a table hint.
    Unknown,
}

/// The variables declared in the batch being bound.
///
/// `@x` is a local variable, looked up here; `@@x` is a built-in function, looked up in
/// the `sysfn` registry instead.
pub trait VariableScope {
    /// The declared type of `name`, `@` included, or `None` when no such variable is in
    /// scope (which the binder reports as error 137).
    fn type_of(&self, name: &str) -> Option<TypeInfo>;
}

/// A scope with no variable at all: a client's `SELECT 1`, and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NoVariables;

impl VariableScope for NoVariables {
    fn type_of(&self, _name: &str) -> Option<TypeInfo> {
        None
    }
}

/// The subset of the `SET` options that typing and evaluation depend on.
///
/// `session::SetOptions` (the whole state of a connection) derives a value of this type;
/// `binder` and `executor` know nothing of the session crate, which depends on them. The
/// five booleans plus the table of `SET IDENTITY_INSERT`, a field of [`BindContext`] by
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionOptions {
    /// `SET ANSI_NULLS`: `= NULL` compares as unknown instead of as `IS NULL`.
    pub ansi_nulls: bool,
    /// `SET ANSI_WARNINGS`: an aggregate over a `NULL`, a divide by zero or a truncation
    /// raises instead of being silently ignored.
    pub ansi_warnings: bool,
    /// `SET ARITHABORT`: an overflow or a divide by zero ends the batch.
    pub arithabort: bool,
    /// `SET CONCAT_NULL_YIELDS_NULL`: `'a' + NULL` is `NULL` instead of `'a'`.
    pub concat_null_yields_null: bool,
    /// `SET NUMERIC_ROUNDABORT`: losing precision in a numeric operation raises.
    pub numeric_roundabort: bool,
    /// Database, schema and object of the table `SET IDENTITY_INSERT` opened, or absent.
    ///
    /// Filled by [`SessionOptions::with_identity_insert`]. `insert.rs` consults it
    /// (`tests/bind_insert.rs`, `identity_insert_on_for_the_target_accepts_an_explicit_value`).
    /// Packed so [`SessionOptions`] stays `Copy`.
    #[allow(clippy::type_complexity)]
    pub identity_insert: Option<([u8; 128], u8, [u8; 128], u8, [u8; 128], u8)>,
}

/// The options a client driver posts when it connects: everything `ON` except
/// `NUMERIC_ROUNDABORT`.
///
/// Production code does not rely on these defaults: `session` sends the values of the
/// connection it holds. They are what an expression binds under in the tests.
impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            ansi_nulls: true,
            ansi_warnings: true,
            arithabort: true,
            concat_null_yields_null: true,
            numeric_roundabort: false,
            identity_insert: None,
        }
    }
}

impl SessionOptions {
    /// Names the table whose identity column may take an explicit value.
    ///
    /// `database`, `schema` and `name` are the three parts `insert.rs` compares with the
    /// `INSERT` target, filling missing parts of the target from the bind context
    /// (`tests/bind_insert.rs`, `identity_insert_on_for_the_target_accepts_an_explicit_value`).
    #[must_use]
    pub fn with_identity_insert(mut self, database: &str, schema: &str, name: &str) -> Self {
        if let (Some(database), Some(schema), Some(name)) =
            (pack_ident(database), pack_ident(schema), pack_ident(name))
        {
            self.identity_insert =
                Some((database.0, database.1, schema.0, schema.1, name.0, name.1));
        }
        self
    }

    /// Whether `database.schema.name` is the table `SET IDENTITY_INSERT` opened.
    pub(crate) fn identity_insert_covers(&self, database: &str, schema: &str, name: &str) -> bool {
        self.identity_insert.is_some_and(
            |(open_db, db_len, open_schema, schema_len, open_name, name_len)| {
                unpack_ident(&open_db, db_len).eq_ignore_ascii_case(database)
                    && unpack_ident(&open_schema, schema_len).eq_ignore_ascii_case(schema)
                    && unpack_ident(&open_name, name_len).eq_ignore_ascii_case(name)
            },
        )
    }
}

/// Packs an identifier into 128 bytes; `None` when it does not fit.
fn pack_ident(part: &str) -> Option<([u8; 128], u8)> {
    let bytes = part.as_bytes();
    if bytes.len() > 128 {
        return None;
    }
    let len = u8::try_from(bytes.len()).ok()?;
    let mut buf = [0u8; 128];
    buf[..bytes.len()].copy_from_slice(bytes);
    Some((buf, len))
}

/// The identifier packed by [`pack_ident`]. Empty when the bytes are not UTF-8.
fn unpack_ident(buf: &[u8; 128], len: u8) -> &str {
    str::from_utf8(&buf[..usize::from(len)]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{
        BindContext, CatalogView, NoVariables, ResolvedTable, ResolvedTableKind, SessionOptions,
        TableReferenceKind, VariableScope,
    };
    use vauban_catalog::ObjectId;
    use vauban_parser::{Ident, ObjectName, Span};

    /// A view that knows one table, `t`, and nothing else.
    struct OneTable;

    impl CatalogView for OneTable {
        fn resolve_table(
            &self,
            name: &ObjectName,
            _database: &str,
            _default_schema: &str,
        ) -> Option<ResolvedTable> {
            (name.name.value == "t").then(|| ResolvedTable {
                object: ObjectId(42),
                table: None,
                columns: Vec::new(),
                kind: ResolvedTableKind::Table,
            })
        }
    }

    fn name(value: &str) -> ObjectName {
        ObjectName {
            server: None,
            database: None,
            schema: None,
            name: Ident {
                value: value.to_owned(),
                quoted: false,
            },
            span: Span {
                line: 1,
                column: 1,
                offset: 0,
                len: 1,
            },
        }
    }

    /// `OneTable` implements `resolve_table` and not `classify_table_reference`: the answer
    /// below comes from the default of the trait reading the resolution.
    #[test]
    fn classify_known_table_is_table() {
        assert_eq!(
            OneTable.classify_table_reference(&name("t"), "master", "dbo"),
            TableReferenceKind::Table
        );
    }

    /// The counter-proof of `classify_known_table_is_table`: the same default answers
    /// `Unknown` for the name `OneTable` does not resolve, so the `Table` above is not a
    /// constant.
    #[test]
    fn classify_unknown_table_stays_unknown() {
        assert_eq!(
            OneTable.classify_table_reference(&name("other"), "master", "dbo"),
            TableReferenceKind::Unknown
        );
        assert!(
            OneTable
                .resolve_table(&name("other"), "master", "dbo")
                .is_none()
        );
        assert!(OneTable.view_definition(ObjectId(42)).is_none());
    }

    #[test]
    fn no_variables_knows_no_variable() {
        assert!(NoVariables.type_of("@x").is_none());
        assert!(NoVariables.type_of("@@rowcount").is_none());
    }

    #[test]
    fn scalar_context_keeps_the_text_it_was_given() {
        let ctx = BindContext::scalar("SELECT 1", SessionOptions::default());
        assert_eq!(ctx.text, "SELECT 1");
        assert!(ctx.catalog.is_none());
    }
}
