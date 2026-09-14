//! What the binder needs to know besides the AST: the batch text, the catalogue, the
//! current database and schema, the variables in scope and the `SET` options.

use vauban_catalog::ObjectId;
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
/// `binder` and `executor` know nothing of the session crate, which depends on them. Five
/// booleans, hence `Copy` and a field of [`BindContext`] by value.
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
        }
    }
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
