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
/// the ones of that table. Five of them, `DISK`, `DUMP`, `LOAD`, `PRECISION` and
/// `SECURITYAUDIT`, are plain names for SQL Server 2022 and leave the set, which keeps
/// 179. Adding a variant to [`Keyword`] later must not change this number unless the
/// page changes.
#[test]
fn keyword_reserved_set_size() {
    let reserved = KEYWORDS
        .iter()
        .filter(|(_, keyword)| keyword.is_reserved())
        .count();
    assert_eq!(reserved, 179);
}

#[test]
fn keyword_legacy_words_are_not_reserved() {
    for keyword in [
        Keyword::Disk,
        Keyword::Dump,
        Keyword::Load,
        Keyword::Precision,
        Keyword::SecurityAudit,
    ] {
        assert!(!keyword.is_reserved(), "{keyword:?} must not be reserved");
    }
    // Neighbours of the same table that stay reserved.
    for keyword in [Keyword::File, Keyword::Key, Keyword::Plan] {
        assert!(keyword.is_reserved(), "{keyword:?} must be reserved");
    }
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
        Keyword::Precision,
        Keyword::Disk,
        Keyword::Dump,
        Keyword::Load,
        Keyword::SecurityAudit,
    ] {
        assert!(!keyword.is_reserved(), "{keyword:?} must not be reserved");
    }
    // `OFFSETS` is the reserved one, not `OFFSET`.
    assert!(Keyword::Offsets.is_reserved());
}
