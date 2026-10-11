//! What the catalogue knows about an object once it exists: the `*Meta` types the rest of
//! the engine reads through a [`CatalogSnapshot`](crate::CatalogSnapshot).
//!
//! A `*Meta` describes a **stored** object and is produced by the catalogue; a `*Def`
//! (`def.rs`) describes an object a caller **asks for** and is consumed by the
//! mutations. `CREATE TABLE` takes a [`TableDef`](crate::TableDef) and gives back a
//! [`TableMeta`].

use vauban_storage::{DbId, IndexId, KeyColumn, TableId};
use vauban_types::{Collation, TypeInfo};

use crate::def::RefAction;
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
    /// Stored text of the `DEFAULT` constraint of the column, the text
    /// `sys.default_constraints.definition` publishes (`((0))` for `DEFAULT 0`), `None` when it
    /// has no default.
    pub default: Option<String>,
    /// `IDENTITY` property of the column, `None` for a column declared without the clause.
    pub identity: Option<IdentitySpec>,
    /// Stored text of the expression of a computed column, the text
    /// `sys.computed_columns.definition` publishes, `None` for a stored column.
    pub computed: Option<String>,
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
        /// Stored text of the predicate the rows must satisfy, the text
        /// `sys.check_constraints.definition` publishes: `([a]>=(0))` for `CHECK (a >= 0)`.
        /// A table rebuilt from its stored rows holds the same text
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
        /// Stored text of the expression evaluated when the column is left out of an
        /// `INSERT`, the text `sys.default_constraints.definition` publishes: `((0))` for
        /// `DEFAULT 0` (`tests/sys_constraints.rs`,
        /// `a_table_rebuilt_from_its_stored_text_holds_the_same_expressions`).
        definition: String,
    },
}

impl ColumnMeta {
    /// The stored text of the expression of a computed column, `None` for a stored column:
    /// the text `sys.computed_columns.definition` publishes, which the binder parses and
    /// binds (unit test `the_expression_texts_are_the_published_texts`).
    pub fn computed_text(&self) -> Option<String> {
        self.computed.clone()
    }

    /// The stored text of the `DEFAULT` of the column, `None` for a column without one: the text
    /// `sys.default_constraints.definition` publishes, `((0))` for `DEFAULT 0`.
    pub fn default_text(&self) -> Option<String> {
        self.default.clone()
    }
}

impl ConstraintMeta {
    /// The stored text of the expression of a `CHECK` or of a `DEFAULT`, `None` for the
    /// other kinds: `([a]>=(0))` for `CHECK (a >= 0)`, `((0))` for `DEFAULT 0` (unit test
    /// `the_expression_texts_are_the_published_texts`).
    pub fn expression_text(&self) -> Option<String> {
        match self {
            ConstraintMeta::Check { definition, .. }
            | ConstraintMeta::Default { definition, .. } => Some(definition.clone()),
            ConstraintMeta::PrimaryKey(_)
            | ConstraintMeta::Unique(_)
            | ConstraintMeta::ForeignKey { .. } => None,
        }
    }
}

pub use crate::def::AlterTable;

/// The column names the stored text of an object expression reads, in the order they are
/// written, duplicates included.
///
/// A name is the last part of a chain of identifiers (`a` in `[t].[a]`) that is not a call
/// (`abs(`), a keyword, the type of a `CONVERT` or of an `AS`, a collation, a variable, or a
/// part of a subquery or of an `OVER` clause. Strings and numbers are skipped. The first
/// argument of a date-part function (`year` in `datepart(year,[c])`) is read as a name: a
/// table that has no column of that name does not match it (unit test
/// `the_column_names_of_a_stored_text`).
pub(crate) fn column_names(text: &str) -> Vec<String> {
    let tokens = expression_tokens(text);
    let mut names = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        let token = &tokens[at];
        match token {
            Token::Word(word) if word.eq_ignore_ascii_case("select") => {
                at = after_enclosing_group(&tokens, at);
                continue;
            }
            Token::Word(word)
                if word.eq_ignore_ascii_case("over")
                    && matches!(tokens.get(at + 1), Some(Token::Punct('('))) =>
            {
                at = after_enclosing_group(&tokens, at + 2);
                continue;
            }
            Token::Word(word) if is_keyword(word) => {
                // The name after `COLLATE`, `AS` or `FOR` is a collation, a type or a
                // sequence: skip its chain.
                if ["collate", "as", "for"]
                    .iter()
                    .any(|keyword| word.eq_ignore_ascii_case(keyword))
                {
                    at = after_chain(&tokens, at + 1);
                } else {
                    at += 1;
                }
                continue;
            }
            Token::Word(_) | Token::Bracketed(_) => {
                let end = after_chain(&tokens, at);
                let is_call = matches!(tokens.get(end), Some(Token::Punct('(')));
                let is_type = at >= 2
                    && matches!(tokens[at - 1], Token::Punct('('))
                    && matches!(&tokens[at - 2], Token::Word(call)
                        if call.eq_ignore_ascii_case("convert")
                            || call.eq_ignore_ascii_case("try_convert"));
                if !is_call
                    && !is_type
                    && let Some(Token::Word(last) | Token::Bracketed(last)) = tokens.get(end - 1)
                {
                    names.push(last.clone());
                }
                at = end;
                continue;
            }
            Token::Punct(_) | Token::Other => at += 1,
        }
    }
    names
}

/// A token of a stored expression text, as [`column_names`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// An identifier written bare, a keyword or a function name.
    Word(String),
    /// An identifier between brackets, `]]` read as `]`.
    Bracketed(String),
    /// `(`, `)`, `.` or `,`.
    Punct(char),
    /// A string, a number, a variable or an operator.
    Other,
}

/// The tokens of `text`.
fn expression_tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        if c == '\'' || ((c == 'N' || c == 'n') && chars.get(at + 1) == Some(&'\'')) {
            at += if c == '\'' { 1 } else { 2 };
            while at < chars.len() {
                if chars[at] == '\'' {
                    if chars.get(at + 1) == Some(&'\'') {
                        at += 2;
                        continue;
                    }
                    break;
                }
                at += 1;
            }
            at += 1;
            tokens.push(Token::Other);
        } else if c == '[' {
            let mut name = String::new();
            at += 1;
            while at < chars.len() {
                if chars[at] == ']' {
                    if chars.get(at + 1) == Some(&']') {
                        name.push(']');
                        at += 2;
                        continue;
                    }
                    break;
                }
                name.push(chars[at]);
                at += 1;
            }
            at += 1;
            tokens.push(Token::Bracketed(name));
        } else if c == '@' || c == '$' || c.is_ascii_digit() {
            at += 1;
            while at < chars.len() && (chars[at].is_alphanumeric() || "_@#$.".contains(chars[at])) {
                at += 1;
            }
            tokens.push(Token::Other);
        } else if c.is_alphabetic() || c == '_' || c == '#' {
            let start = at;
            while at < chars.len() && (chars[at].is_alphanumeric() || "_@#$".contains(chars[at])) {
                at += 1;
            }
            tokens.push(Token::Word(chars[start..at].iter().collect()));
        } else if c == '.'
            && chars.get(at + 1).is_some_and(char::is_ascii_digit)
            && !matches!(tokens.last(), Some(Token::Word(_) | Token::Bracketed(_)))
        {
            at += 1;
            while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '.') {
                at += 1;
            }
            tokens.push(Token::Other);
        } else if "().,".contains(c) {
            tokens.push(Token::Punct(c));
            at += 1;
        } else {
            if !c.is_whitespace() {
                tokens.push(Token::Other);
            }
            at += 1;
        }
    }
    tokens
}

/// The index after the chain of identifiers that starts at `at` (`[t].[a]`, `dbo.t.a`), `at`
/// itself when no identifier starts there.
fn after_chain(tokens: &[Token], at: usize) -> usize {
    let mut end = at;
    while matches!(tokens.get(end), Some(Token::Word(_) | Token::Bracketed(_))) {
        end += 1;
        if matches!(tokens.get(end), Some(Token::Punct('.')))
            && matches!(
                tokens.get(end + 1),
                Some(Token::Word(_) | Token::Bracketed(_))
            )
        {
            end += 1;
        } else {
            break;
        }
    }
    end
}

/// The index after the `)` that closes the group `at` is inside of, the end of `tokens` for
/// a group left open.
fn after_enclosing_group(tokens: &[Token], at: usize) -> usize {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(at) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') if depth == 0 => return index + 1,
            Token::Punct(')') => depth -= 1,
            _ => {}
        }
    }
    tokens.len()
}

/// The words of a stored expression text that are not names.
fn is_keyword(word: &str) -> bool {
    const KEYWORDS: [&str; 23] = [
        "and", "or", "not", "null", "is", "in", "like", "between", "case", "when", "then", "else",
        "end", "escape", "collate", "as", "exists", "next", "value", "for", "some", "any", "all",
    ];
    KEYWORDS
        .iter()
        .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expression_texts_are_the_published_texts() {
        let column = ColumnMeta {
            id: ColumnId(1),
            name: "a".to_owned(),
            ty: vauban_types::TypeInfo::new(vauban_types::SqlType::Int, true),
            ordinal: 0,
            default: Some("((0))".to_owned()),
            identity: None,
            computed: Some("([a]+(1))".to_owned()),
        };
        assert_eq!(column.default_text().as_deref(), Some("((0))"));
        assert_eq!(column.computed_text().as_deref(), Some("([a]+(1))"));
        let default = ConstraintMeta::Default {
            constraint: ObjectId(7),
            system_named: true,
            column: ColumnId(1),
            definition: "((0))".to_owned(),
        };
        assert_eq!(default.expression_text().as_deref(), Some("((0))"));
        let check = ConstraintMeta::Check {
            constraint: ObjectId(8),
            system_named: false,
            definition: "([a]>=(0))".to_owned(),
        };
        assert_eq!(check.expression_text().as_deref(), Some("([a]>=(0))"));
        assert_eq!(
            ConstraintMeta::PrimaryKey(IndexId(1)).expression_text(),
            None
        );
    }

    #[test]
    fn the_column_names_of_a_stored_text() {
        let cases: [(&str, &[&str]); 16] = [
            ("([a]>=(0))", &["a"]),
            ("([q4].[a]+[q4].[a2])", &["a", "a2"]),
            ("([a]]b]>(0) AND [my col]<(1))", &["a]b", "my col"]),
            ("(abs([a])=(2) OR abs([a])=(1))", &["a", "a"]),
            ("(CONVERT([varchar](10),[b])<>'')", &["b"]),
            ("(TRY_CAST([b] AS [int]))", &["b"]),
            ("([b] like 'x[a]%' escape '!')", &["b"]),
            ("([n]=N'[a]''s')", &["n"]),
            ("([b] collate Latin1_General_CS_AS='x')", &["b"]),
            ("(datepart(year,[c])>(2000))", &["year", "c"]),
            ("((1.0000000000000000e+003)+($5.0000)+0xAB)", &[]),
            (
                "(case when [a]>(0) then  -[a] else [b] end)",
                &["a", "a", "b"],
            ),
            ("([a] IS NOT NULL AND @@spid>(0))", &["a"]),
            ("(dbo.t.a>(0))", &["a"]),
            ("([a] IN (SELECT [x] FROM [s]) OR [b]>(0))", &["a", "b"]),
            ("(sum([a]) over (partition by [b]))", &["a"]),
        ];
        for (text, expected) in cases {
            assert_eq!(column_names(text), expected.to_vec(), "{text}");
        }
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
