//! The T-SQL keywords the lexer recognises.
//!
//! [`Keyword`] holds one variant per word; [`KEYWORDS`] maps the spelling to the variant
//! and [`Keyword::is_reserved`] says whether T-SQL forbids the word as a bare identifier.
//! Being listed here does **not** make a word reserved: the list also holds
//! context-sensitive words (`APPLY`, `ROWS`, `MATCHED`, ...) that T-SQL still accepts as
//! identifiers, and that is why `SELECT sum FROM t` parses.
//!
//! The reserved words are the ones of the T-SQL reference
//! <https://learn.microsoft.com/en-us/sql/t-sql/language-elements/reserved-keywords-transact-sql>,
//! **first table** (the SQL Server reserved keywords, `keyword_reserved_set` below);
//! neither the Synapse `LABEL`
//! table, nor the ODBC reserved keywords, nor the future keywords further down that page.
//! The other words come from the statement grammars of [MS-TSQL] that the module parses.
//!
//! `GO` is deliberately absent: it is a client batch separator, not a T-SQL keyword, and
//! the lexer turns it into a plain identifier.

use std::cmp::Ordering;

/// A T-SQL keyword, one variant per word, spelled in `PascalCase`.
///
/// Underscores of the original spelling disappear (`TRY_CONVERT` -> `TryConvert`), except
/// where [`KEYWORDS`] says otherwise; the table is the only place that holds the spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Keyword {
    Action,
    Add,
    All,
    Alter,
    And,
    Any,
    Apply,
    As,
    Asc,
    Authorization,
    Auto,
    Backup,
    Begin,
    Between,
    Break,
    Browse,
    Bulk,
    By,
    Cache,
    Cascade,
    Case,
    Cast,
    Catch,
    Character,
    Check,
    Checkpoint,
    Close,
    Clustered,
    Coalesce,
    Collate,
    Column,
    Commit,
    Committed,
    Compute,
    Constraint,
    Contains,
    ContainsTable,
    Continue,
    Convert,
    Create,
    Cross,
    Current,
    CurrentDate,
    CurrentTime,
    CurrentTimestamp,
    CurrentUser,
    Cursor,
    Cycle,
    Database,
    Dbcc,
    Deallocate,
    Declare,
    Default,
    Delay,
    Delete,
    Deleted,
    Deny,
    Desc,
    Disk,
    Distinct,
    Distributed,
    Double,
    Drop,
    Dump,
    Else,
    Enable,
    End,
    Errlvl,
    Escape,
    Except,
    Exec,
    Execute,
    Exists,
    Exit,
    External,
    Fetch,
    File,
    FillFactor,
    First,
    Following,
    For,
    Foreign,
    Freetext,
    FreetextTable,
    From,
    Full,
    Function,
    Goto,
    Grant,
    Group,
    Having,
    Holdlock,
    Identity,
    IdentityCol,
    IdentityInsert,
    If,
    Ignore,
    In,
    Include,
    Index,
    Inner,
    Insert,
    Inserted,
    Instead,
    Intersect,
    Into,
    Is,
    Isolation,
    Join,
    Json,
    Key,
    Kill,
    Left,
    Level,
    Like,
    Lineno,
    Load,
    Mark,
    Matched,
    Max,
    MaxValue,
    Merge,
    MinValue,
    National,
    Next,
    No,
    NoCheck,
    NoCount,
    NoLock,
    NonClustered,
    Not,
    Null,
    NullIf,
    Of,
    Off,
    Offset,
    Offsets,
    On,
    Only,
    Open,
    OpenDataSource,
    OpenQuery,
    OpenRowset,
    OpenXml,
    Option,
    Or,
    Order,
    Out,
    Outer,
    Output,
    Over,
    Partition,
    Path,
    Percent,
    Persisted,
    Pivot,
    Plan,
    Preceding,
    Precision,
    Primary,
    Print,
    Proc,
    Procedure,
    Public,
    Raiserror,
    Range,
    Raw,
    Read,
    ReadOnly,
    ReadText,
    Reconfigure,
    References,
    Repeatable,
    Replication,
    Restore,
    Restrict,
    Return,
    Returns,
    Revert,
    Revoke,
    Right,
    Rollback,
    RowCount,
    RowGuidCol,
    RowLock,
    Rows,
    Rule,
    Save,
    Schema,
    SecurityAudit,
    Select,
    SemanticKeyPhraseTable,
    SemanticSimilarityDetailsTable,
    SemanticSimilarityTable,
    Sequence,
    Serializable,
    SessionUser,
    Set,
    SetUser,
    Shutdown,
    Snapshot,
    Some,
    Source,
    Start,
    Statistics,
    Sum,
    SystemUser,
    Table,
    TableSample,
    Target,
    TextSize,
    Then,
    Throw,
    Ties,
    Time,
    To,
    Top,
    Tran,
    Transaction,
    Trigger,
    Truncate,
    Try,
    TryCast,
    TryConvert,
    TryParse,
    Tsequal,
    Unbounded,
    Uncommitted,
    Union,
    Unique,
    Unpivot,
    Update,
    UpdateText,
    Use,
    User,
    Value,
    Values,
    Varchar,
    Varying,
    View,
    Waitfor,
    When,
    Where,
    While,
    With,
    Within,
    Work,
    WriteText,
    Xml,
}

/// Every keyword the lexer knows, sorted by its **uppercase** spelling so that
/// [`Keyword::parse`] can binary-search it. The order is checked by a test.
///
/// The one entry of that table that is **not** here is `WITHIN GROUP`: it is written
/// in two words, and this table is looked up one word at a time. `WITHIN` and `GROUP` are
/// declared separately (`GROUP` is reserved in its own right), so a lone `WITHIN` is not
/// reported as reserved. The only visible consequence is an error 102 instead of a 156 on
/// a misplaced `WITHIN`.
pub(crate) const KEYWORDS: &[(&str, Keyword)] = &[
    ("ACTION", Keyword::Action),
    ("ADD", Keyword::Add),
    ("ALL", Keyword::All),
    ("ALTER", Keyword::Alter),
    ("AND", Keyword::And),
    ("ANY", Keyword::Any),
    ("APPLY", Keyword::Apply),
    ("AS", Keyword::As),
    ("ASC", Keyword::Asc),
    ("AUTHORIZATION", Keyword::Authorization),
    ("AUTO", Keyword::Auto),
    ("BACKUP", Keyword::Backup),
    ("BEGIN", Keyword::Begin),
    ("BETWEEN", Keyword::Between),
    ("BREAK", Keyword::Break),
    ("BROWSE", Keyword::Browse),
    ("BULK", Keyword::Bulk),
    ("BY", Keyword::By),
    ("CACHE", Keyword::Cache),
    ("CASCADE", Keyword::Cascade),
    ("CASE", Keyword::Case),
    ("CAST", Keyword::Cast),
    ("CATCH", Keyword::Catch),
    ("CHARACTER", Keyword::Character),
    ("CHECK", Keyword::Check),
    ("CHECKPOINT", Keyword::Checkpoint),
    ("CLOSE", Keyword::Close),
    ("CLUSTERED", Keyword::Clustered),
    ("COALESCE", Keyword::Coalesce),
    ("COLLATE", Keyword::Collate),
    ("COLUMN", Keyword::Column),
    ("COMMIT", Keyword::Commit),
    ("COMMITTED", Keyword::Committed),
    ("COMPUTE", Keyword::Compute),
    ("CONSTRAINT", Keyword::Constraint),
    ("CONTAINS", Keyword::Contains),
    ("CONTAINSTABLE", Keyword::ContainsTable),
    ("CONTINUE", Keyword::Continue),
    ("CONVERT", Keyword::Convert),
    ("CREATE", Keyword::Create),
    ("CROSS", Keyword::Cross),
    ("CURRENT", Keyword::Current),
    ("CURRENT_DATE", Keyword::CurrentDate),
    ("CURRENT_TIME", Keyword::CurrentTime),
    ("CURRENT_TIMESTAMP", Keyword::CurrentTimestamp),
    ("CURRENT_USER", Keyword::CurrentUser),
    ("CURSOR", Keyword::Cursor),
    ("CYCLE", Keyword::Cycle),
    ("DATABASE", Keyword::Database),
    ("DBCC", Keyword::Dbcc),
    ("DEALLOCATE", Keyword::Deallocate),
    ("DECLARE", Keyword::Declare),
    ("DEFAULT", Keyword::Default),
    ("DELAY", Keyword::Delay),
    ("DELETE", Keyword::Delete),
    ("DELETED", Keyword::Deleted),
    ("DENY", Keyword::Deny),
    ("DESC", Keyword::Desc),
    ("DISK", Keyword::Disk),
    ("DISTINCT", Keyword::Distinct),
    ("DISTRIBUTED", Keyword::Distributed),
    ("DOUBLE", Keyword::Double),
    ("DROP", Keyword::Drop),
    ("DUMP", Keyword::Dump),
    ("ELSE", Keyword::Else),
    ("ENABLE", Keyword::Enable),
    ("END", Keyword::End),
    ("ERRLVL", Keyword::Errlvl),
    ("ESCAPE", Keyword::Escape),
    ("EXCEPT", Keyword::Except),
    ("EXEC", Keyword::Exec),
    ("EXECUTE", Keyword::Execute),
    ("EXISTS", Keyword::Exists),
    ("EXIT", Keyword::Exit),
    ("EXTERNAL", Keyword::External),
    ("FETCH", Keyword::Fetch),
    ("FILE", Keyword::File),
    ("FILLFACTOR", Keyword::FillFactor),
    ("FIRST", Keyword::First),
    ("FOLLOWING", Keyword::Following),
    ("FOR", Keyword::For),
    ("FOREIGN", Keyword::Foreign),
    ("FREETEXT", Keyword::Freetext),
    ("FREETEXTTABLE", Keyword::FreetextTable),
    ("FROM", Keyword::From),
    ("FULL", Keyword::Full),
    ("FUNCTION", Keyword::Function),
    ("GOTO", Keyword::Goto),
    ("GRANT", Keyword::Grant),
    ("GROUP", Keyword::Group),
    ("HAVING", Keyword::Having),
    ("HOLDLOCK", Keyword::Holdlock),
    ("IDENTITY", Keyword::Identity),
    ("IDENTITYCOL", Keyword::IdentityCol),
    ("IDENTITY_INSERT", Keyword::IdentityInsert),
    ("IF", Keyword::If),
    ("IGNORE", Keyword::Ignore),
    ("IN", Keyword::In),
    ("INCLUDE", Keyword::Include),
    ("INDEX", Keyword::Index),
    ("INNER", Keyword::Inner),
    ("INSERT", Keyword::Insert),
    ("INSERTED", Keyword::Inserted),
    ("INSTEAD", Keyword::Instead),
    ("INTERSECT", Keyword::Intersect),
    ("INTO", Keyword::Into),
    ("IS", Keyword::Is),
    ("ISOLATION", Keyword::Isolation),
    ("JOIN", Keyword::Join),
    ("JSON", Keyword::Json),
    ("KEY", Keyword::Key),
    ("KILL", Keyword::Kill),
    ("LEFT", Keyword::Left),
    ("LEVEL", Keyword::Level),
    ("LIKE", Keyword::Like),
    ("LINENO", Keyword::Lineno),
    ("LOAD", Keyword::Load),
    ("MARK", Keyword::Mark),
    ("MATCHED", Keyword::Matched),
    ("MAX", Keyword::Max),
    ("MAXVALUE", Keyword::MaxValue),
    ("MERGE", Keyword::Merge),
    ("MINVALUE", Keyword::MinValue),
    ("NATIONAL", Keyword::National),
    ("NEXT", Keyword::Next),
    ("NO", Keyword::No),
    ("NOCHECK", Keyword::NoCheck),
    ("NOCOUNT", Keyword::NoCount),
    ("NOLOCK", Keyword::NoLock),
    ("NONCLUSTERED", Keyword::NonClustered),
    ("NOT", Keyword::Not),
    ("NULL", Keyword::Null),
    ("NULLIF", Keyword::NullIf),
    ("OF", Keyword::Of),
    ("OFF", Keyword::Off),
    ("OFFSET", Keyword::Offset),
    ("OFFSETS", Keyword::Offsets),
    ("ON", Keyword::On),
    ("ONLY", Keyword::Only),
    ("OPEN", Keyword::Open),
    ("OPENDATASOURCE", Keyword::OpenDataSource),
    ("OPENQUERY", Keyword::OpenQuery),
    ("OPENROWSET", Keyword::OpenRowset),
    ("OPENXML", Keyword::OpenXml),
    ("OPTION", Keyword::Option),
    ("OR", Keyword::Or),
    ("ORDER", Keyword::Order),
    ("OUT", Keyword::Out),
    ("OUTER", Keyword::Outer),
    ("OUTPUT", Keyword::Output),
    ("OVER", Keyword::Over),
    ("PARTITION", Keyword::Partition),
    ("PATH", Keyword::Path),
    ("PERCENT", Keyword::Percent),
    ("PERSISTED", Keyword::Persisted),
    ("PIVOT", Keyword::Pivot),
    ("PLAN", Keyword::Plan),
    ("PRECEDING", Keyword::Preceding),
    ("PRECISION", Keyword::Precision),
    ("PRIMARY", Keyword::Primary),
    ("PRINT", Keyword::Print),
    ("PROC", Keyword::Proc),
    ("PROCEDURE", Keyword::Procedure),
    ("PUBLIC", Keyword::Public),
    ("RAISERROR", Keyword::Raiserror),
    ("RANGE", Keyword::Range),
    ("RAW", Keyword::Raw),
    ("READ", Keyword::Read),
    ("READONLY", Keyword::ReadOnly),
    ("READTEXT", Keyword::ReadText),
    ("RECONFIGURE", Keyword::Reconfigure),
    ("REFERENCES", Keyword::References),
    ("REPEATABLE", Keyword::Repeatable),
    ("REPLICATION", Keyword::Replication),
    ("RESTORE", Keyword::Restore),
    ("RESTRICT", Keyword::Restrict),
    ("RETURN", Keyword::Return),
    ("RETURNS", Keyword::Returns),
    ("REVERT", Keyword::Revert),
    ("REVOKE", Keyword::Revoke),
    ("RIGHT", Keyword::Right),
    ("ROLLBACK", Keyword::Rollback),
    ("ROWCOUNT", Keyword::RowCount),
    ("ROWGUIDCOL", Keyword::RowGuidCol),
    ("ROWLOCK", Keyword::RowLock),
    ("ROWS", Keyword::Rows),
    ("RULE", Keyword::Rule),
    ("SAVE", Keyword::Save),
    ("SCHEMA", Keyword::Schema),
    ("SECURITYAUDIT", Keyword::SecurityAudit),
    ("SELECT", Keyword::Select),
    ("SEMANTICKEYPHRASETABLE", Keyword::SemanticKeyPhraseTable),
    (
        "SEMANTICSIMILARITYDETAILSTABLE",
        Keyword::SemanticSimilarityDetailsTable,
    ),
    ("SEMANTICSIMILARITYTABLE", Keyword::SemanticSimilarityTable),
    ("SEQUENCE", Keyword::Sequence),
    ("SERIALIZABLE", Keyword::Serializable),
    ("SESSION_USER", Keyword::SessionUser),
    ("SET", Keyword::Set),
    ("SETUSER", Keyword::SetUser),
    ("SHUTDOWN", Keyword::Shutdown),
    ("SNAPSHOT", Keyword::Snapshot),
    ("SOME", Keyword::Some),
    ("SOURCE", Keyword::Source),
    ("START", Keyword::Start),
    ("STATISTICS", Keyword::Statistics),
    ("SUM", Keyword::Sum),
    ("SYSTEM_USER", Keyword::SystemUser),
    ("TABLE", Keyword::Table),
    ("TABLESAMPLE", Keyword::TableSample),
    ("TARGET", Keyword::Target),
    ("TEXTSIZE", Keyword::TextSize),
    ("THEN", Keyword::Then),
    ("THROW", Keyword::Throw),
    ("TIES", Keyword::Ties),
    ("TIME", Keyword::Time),
    ("TO", Keyword::To),
    ("TOP", Keyword::Top),
    ("TRAN", Keyword::Tran),
    ("TRANSACTION", Keyword::Transaction),
    ("TRIGGER", Keyword::Trigger),
    ("TRUNCATE", Keyword::Truncate),
    ("TRY", Keyword::Try),
    ("TRY_CAST", Keyword::TryCast),
    ("TRY_CONVERT", Keyword::TryConvert),
    ("TRY_PARSE", Keyword::TryParse),
    ("TSEQUAL", Keyword::Tsequal),
    ("UNBOUNDED", Keyword::Unbounded),
    ("UNCOMMITTED", Keyword::Uncommitted),
    ("UNION", Keyword::Union),
    ("UNIQUE", Keyword::Unique),
    ("UNPIVOT", Keyword::Unpivot),
    ("UPDATE", Keyword::Update),
    ("UPDATETEXT", Keyword::UpdateText),
    ("USE", Keyword::Use),
    ("USER", Keyword::User),
    ("VALUE", Keyword::Value),
    ("VALUES", Keyword::Values),
    ("VARCHAR", Keyword::Varchar),
    ("VARYING", Keyword::Varying),
    ("VIEW", Keyword::View),
    ("WAITFOR", Keyword::Waitfor),
    ("WHEN", Keyword::When),
    ("WHERE", Keyword::Where),
    ("WHILE", Keyword::While),
    ("WITH", Keyword::With),
    ("WITHIN", Keyword::Within),
    ("WORK", Keyword::Work),
    ("WRITETEXT", Keyword::WriteText),
    ("XML", Keyword::Xml),
];

impl Keyword {
    /// Returns the keyword `word` spells, ignoring ASCII case, or `None` when the word is
    /// a plain identifier.
    ///
    /// Only ASCII case is folded, which is enough: every keyword of T-SQL is ASCII, so a
    /// word holding any other letter cannot be one.
    pub(crate) fn parse(word: &str) -> Option<Keyword> {
        let index = KEYWORDS
            .binary_search_by(|(spelling, _)| compare_ignoring_ascii_case(spelling, word))
            .ok()?;
        KEYWORDS.get(index).map(|(_, keyword)| *keyword)
    }

    /// Whether the word is a **reserved** keyword, which T-SQL refuses as a bare
    /// identifier and which the parser reports with error 156 instead of 102.
    ///
    /// The set is exactly the first table of the page cited at the top of this file
    /// (184 one-word entries; see [`KEYWORDS`] for the 185th, `WITHIN GROUP`). A word
    /// missing from it, such as `VARCHAR`, `SUM` or `OFFSET`, stays usable as an
    /// identifier: `SELECT varchar FROM t` is legal T-SQL.
    ///
    /// Callers: `display::is_reserved_keyword` (the safety rule that brackets a bare
    /// reserved word) and the error 156 of the parser.
    pub(crate) fn is_reserved(self) -> bool {
        matches!(
            self,
            Keyword::Add
                | Keyword::All
                | Keyword::Alter
                | Keyword::And
                | Keyword::Any
                | Keyword::As
                | Keyword::Asc
                | Keyword::Authorization
                | Keyword::Backup
                | Keyword::Begin
                | Keyword::Between
                | Keyword::Break
                | Keyword::Browse
                | Keyword::Bulk
                | Keyword::By
                | Keyword::Cascade
                | Keyword::Case
                | Keyword::Check
                | Keyword::Checkpoint
                | Keyword::Close
                | Keyword::Clustered
                | Keyword::Coalesce
                | Keyword::Collate
                | Keyword::Column
                | Keyword::Commit
                | Keyword::Compute
                | Keyword::Constraint
                | Keyword::Contains
                | Keyword::ContainsTable
                | Keyword::Continue
                | Keyword::Convert
                | Keyword::Create
                | Keyword::Cross
                | Keyword::Current
                | Keyword::CurrentDate
                | Keyword::CurrentTime
                | Keyword::CurrentTimestamp
                | Keyword::CurrentUser
                | Keyword::Cursor
                | Keyword::Database
                | Keyword::Dbcc
                | Keyword::Deallocate
                | Keyword::Declare
                | Keyword::Default
                | Keyword::Delete
                | Keyword::Deny
                | Keyword::Desc
                | Keyword::Disk
                | Keyword::Distinct
                | Keyword::Distributed
                | Keyword::Double
                | Keyword::Drop
                | Keyword::Dump
                | Keyword::Else
                | Keyword::End
                | Keyword::Errlvl
                | Keyword::Escape
                | Keyword::Except
                | Keyword::Exec
                | Keyword::Execute
                | Keyword::Exists
                | Keyword::Exit
                | Keyword::External
                | Keyword::Fetch
                | Keyword::File
                | Keyword::FillFactor
                | Keyword::For
                | Keyword::Foreign
                | Keyword::Freetext
                | Keyword::FreetextTable
                | Keyword::From
                | Keyword::Full
                | Keyword::Function
                | Keyword::Goto
                | Keyword::Grant
                | Keyword::Group
                | Keyword::Having
                | Keyword::Holdlock
                | Keyword::Identity
                | Keyword::IdentityCol
                | Keyword::IdentityInsert
                | Keyword::If
                | Keyword::In
                | Keyword::Index
                | Keyword::Inner
                | Keyword::Insert
                | Keyword::Intersect
                | Keyword::Into
                | Keyword::Is
                | Keyword::Join
                | Keyword::Key
                | Keyword::Kill
                | Keyword::Left
                | Keyword::Like
                | Keyword::Lineno
                | Keyword::Load
                | Keyword::Merge
                | Keyword::National
                | Keyword::NoCheck
                | Keyword::NonClustered
                | Keyword::Not
                | Keyword::Null
                | Keyword::NullIf
                | Keyword::Of
                | Keyword::Off
                | Keyword::Offsets
                | Keyword::On
                | Keyword::Open
                | Keyword::OpenDataSource
                | Keyword::OpenQuery
                | Keyword::OpenRowset
                | Keyword::OpenXml
                | Keyword::Option
                | Keyword::Or
                | Keyword::Order
                | Keyword::Outer
                | Keyword::Over
                | Keyword::Percent
                | Keyword::Pivot
                | Keyword::Plan
                | Keyword::Precision
                | Keyword::Primary
                | Keyword::Print
                | Keyword::Proc
                | Keyword::Procedure
                | Keyword::Public
                | Keyword::Raiserror
                | Keyword::Read
                | Keyword::ReadText
                | Keyword::Reconfigure
                | Keyword::References
                | Keyword::Replication
                | Keyword::Restore
                | Keyword::Restrict
                | Keyword::Return
                | Keyword::Revert
                | Keyword::Revoke
                | Keyword::Right
                | Keyword::Rollback
                | Keyword::RowCount
                | Keyword::RowGuidCol
                | Keyword::Rule
                | Keyword::Save
                | Keyword::Schema
                | Keyword::SecurityAudit
                | Keyword::Select
                | Keyword::SemanticKeyPhraseTable
                | Keyword::SemanticSimilarityDetailsTable
                | Keyword::SemanticSimilarityTable
                | Keyword::SessionUser
                | Keyword::Set
                | Keyword::SetUser
                | Keyword::Shutdown
                | Keyword::Some
                | Keyword::Statistics
                | Keyword::SystemUser
                | Keyword::Table
                | Keyword::TableSample
                | Keyword::TextSize
                | Keyword::Then
                | Keyword::To
                | Keyword::Top
                | Keyword::Tran
                | Keyword::Transaction
                | Keyword::Trigger
                | Keyword::Truncate
                | Keyword::TryConvert
                | Keyword::Tsequal
                | Keyword::Union
                | Keyword::Unique
                | Keyword::Unpivot
                | Keyword::Update
                | Keyword::UpdateText
                | Keyword::Use
                | Keyword::User
                | Keyword::Values
                | Keyword::Varying
                | Keyword::View
                | Keyword::Waitfor
                | Keyword::When
                | Keyword::Where
                | Keyword::While
                | Keyword::With
                | Keyword::WriteText
        )
    }
}

/// Compares a table spelling, already uppercase, with a word of the batch, folding the
/// ASCII case of the word. Byte order, which is the order [`KEYWORDS`] is sorted in.
fn compare_ignoring_ascii_case(spelling: &str, word: &str) -> Ordering {
    spelling
        .bytes()
        .cmp(word.bytes().map(|byte| byte.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::{KEYWORDS, Keyword};

    #[test]
    fn keyword_table_is_sorted() {
        for pair in KEYWORDS.windows(2) {
            let [(before, _), (after, _)] = pair else {
                unreachable!("windows(2) yields pairs")
            };
            assert!(before < after, "{before} must sort before {after}");
        }
    }

    #[test]
    fn keyword_parse_is_case_insensitive() {
        assert_eq!(Keyword::parse("select"), Some(Keyword::Select));
        assert_eq!(Keyword::parse("SeLeCt"), Some(Keyword::Select));
        assert_eq!(Keyword::parse("SELECT"), Some(Keyword::Select));
        assert_eq!(Keyword::parse("try_convert"), Some(Keyword::TryConvert));
        assert_eq!(Keyword::parse("customers"), None);
        // A client batch separator, not a T-SQL keyword.
        assert_eq!(Keyword::parse("GO"), None);
        // Non-ASCII letters cannot spell a keyword, and must not panic either.
        assert_eq!(Keyword::parse("sélect"), None);
        assert_eq!(Keyword::parse(""), None);
    }

    #[test]
    fn keyword_reserved_set() {
        assert!(Keyword::is_reserved(Keyword::Select));
        assert!(!Keyword::is_reserved(Keyword::Sum));
        assert_eq!(Keyword::parse("varchar"), Some(Keyword::Varchar));
        assert!(!Keyword::is_reserved(Keyword::Varchar));
    }

    /// The reserved keywords of T-SQL:
    /// <https://learn.microsoft.com/en-us/sql/t-sql/language-elements/reserved-keywords-transact-sql>
    ///
    /// The first table of that page has 185 entries, one of which, `WITHIN GROUP`, is
    /// written in two words and cannot be recognised word by word. The remaining 184 are
    /// the ones counted here. Adding a variant to [`Keyword`] later must not change this
    /// number unless the page changes.
    #[test]
    fn keyword_reserved_set_size() {
        let reserved = KEYWORDS
            .iter()
            .filter(|(_, keyword)| keyword.is_reserved())
            .count();
        assert_eq!(reserved, 184);
    }

    #[test]
    fn keyword_reserved_spot_checks() {
        for keyword in [
            Keyword::Fetch,
            Keyword::Over,
            Keyword::Merge,
            Keyword::Clustered,
            Keyword::NonClustered,
            Keyword::IdentityInsert,
            Keyword::TryConvert,
            Keyword::Percent,
            Keyword::With,
            Keyword::Goto,
            Keyword::Procedure,
            Keyword::Proc,
            Keyword::Function,
            Keyword::View,
            Keyword::Trigger,
            Keyword::Schema,
            Keyword::Read,
            Keyword::Transaction,
            Keyword::Double,
            Keyword::Precision,
            Keyword::National,
            Keyword::Varying,
            Keyword::Left,
            Keyword::Right,
            Keyword::Convert,
            Keyword::All,
            Keyword::User,
            Keyword::SessionUser,
            Keyword::SystemUser,
            Keyword::CurrentUser,
            Keyword::CurrentTimestamp,
        ] {
            assert!(keyword.is_reserved(), "{keyword:?} must be reserved");
        }
        for keyword in [
            Keyword::Ties,
            Keyword::NoLock,
            Keyword::RowLock,
            Keyword::Offset,
            Keyword::Next,
            Keyword::Only,
            Keyword::TryCast,
            Keyword::Include,
            Keyword::Catch,
            Keyword::Try,
            Keyword::Throw,
            Keyword::Partition,
            Keyword::Sequence,
            Keyword::Character,
            Keyword::Varchar,
            Keyword::Sum,
        ] {
            assert!(!keyword.is_reserved(), "{keyword:?} must not be reserved");
        }
        // `OFFSETS` is the reserved one, not `OFFSET`.
        assert!(Keyword::Offsets.is_reserved());
    }
}
