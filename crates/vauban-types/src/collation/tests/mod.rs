use std::cmp::Ordering;

use super::Collation;

/// The collation names whose locale is `Latin1_General` or `SQL_Latin1_General_CP1`,
/// the result of
///
/// ```sql
/// SELECT name FROM fn_helpcollations()
/// WHERE name LIKE 'Latin1[_]General[_]%' OR name LIKE 'SQL[_]Latin1[_]General[_]CP1[_]%'
/// ORDER BY name;
/// ```
///
/// The same 72 names, and no other, are the ones `'a' COLLATE <name>` accepts over the
/// sweep of [`NAME_SPACE_HEADS`] and the suffixes; the 1 656 others raise 448,
/// severity 16, state 1.
const SERVER_COLLATIONS: [&str; 72] = [
    "Latin1_General_100_BIN",
    "Latin1_General_100_BIN2",
    "Latin1_General_100_BIN2_UTF8",
    "Latin1_General_100_CI_AI",
    "Latin1_General_100_CI_AI_KS",
    "Latin1_General_100_CI_AI_KS_SC",
    "Latin1_General_100_CI_AI_KS_SC_UTF8",
    "Latin1_General_100_CI_AI_KS_WS",
    "Latin1_General_100_CI_AI_KS_WS_SC",
    "Latin1_General_100_CI_AI_KS_WS_SC_UTF8",
    "Latin1_General_100_CI_AI_SC",
    "Latin1_General_100_CI_AI_SC_UTF8",
    "Latin1_General_100_CI_AI_WS",
    "Latin1_General_100_CI_AI_WS_SC",
    "Latin1_General_100_CI_AI_WS_SC_UTF8",
    "Latin1_General_100_CI_AS",
    "Latin1_General_100_CI_AS_KS",
    "Latin1_General_100_CI_AS_KS_SC",
    "Latin1_General_100_CI_AS_KS_SC_UTF8",
    "Latin1_General_100_CI_AS_KS_WS",
    "Latin1_General_100_CI_AS_KS_WS_SC",
    "Latin1_General_100_CI_AS_KS_WS_SC_UTF8",
    "Latin1_General_100_CI_AS_SC",
    "Latin1_General_100_CI_AS_SC_UTF8",
    "Latin1_General_100_CI_AS_WS",
    "Latin1_General_100_CI_AS_WS_SC",
    "Latin1_General_100_CI_AS_WS_SC_UTF8",
    "Latin1_General_100_CS_AI",
    "Latin1_General_100_CS_AI_KS",
    "Latin1_General_100_CS_AI_KS_SC",
    "Latin1_General_100_CS_AI_KS_SC_UTF8",
    "Latin1_General_100_CS_AI_KS_WS",
    "Latin1_General_100_CS_AI_KS_WS_SC",
    "Latin1_General_100_CS_AI_KS_WS_SC_UTF8",
    "Latin1_General_100_CS_AI_SC",
    "Latin1_General_100_CS_AI_SC_UTF8",
    "Latin1_General_100_CS_AI_WS",
    "Latin1_General_100_CS_AI_WS_SC",
    "Latin1_General_100_CS_AI_WS_SC_UTF8",
    "Latin1_General_100_CS_AS",
    "Latin1_General_100_CS_AS_KS",
    "Latin1_General_100_CS_AS_KS_SC",
    "Latin1_General_100_CS_AS_KS_SC_UTF8",
    "Latin1_General_100_CS_AS_KS_WS",
    "Latin1_General_100_CS_AS_KS_WS_SC",
    "Latin1_General_100_CS_AS_KS_WS_SC_UTF8",
    "Latin1_General_100_CS_AS_SC",
    "Latin1_General_100_CS_AS_SC_UTF8",
    "Latin1_General_100_CS_AS_WS",
    "Latin1_General_100_CS_AS_WS_SC",
    "Latin1_General_100_CS_AS_WS_SC_UTF8",
    "Latin1_General_BIN",
    "Latin1_General_BIN2",
    "Latin1_General_CI_AI",
    "Latin1_General_CI_AI_KS",
    "Latin1_General_CI_AI_KS_WS",
    "Latin1_General_CI_AI_WS",
    "Latin1_General_CI_AS",
    "Latin1_General_CI_AS_KS",
    "Latin1_General_CI_AS_KS_WS",
    "Latin1_General_CI_AS_WS",
    "Latin1_General_CS_AI",
    "Latin1_General_CS_AI_KS",
    "Latin1_General_CS_AI_KS_WS",
    "Latin1_General_CS_AI_WS",
    "Latin1_General_CS_AS",
    "Latin1_General_CS_AS_KS",
    "Latin1_General_CS_AS_KS_WS",
    "Latin1_General_CS_AS_WS",
    "SQL_Latin1_General_CP1_CI_AI",
    "SQL_Latin1_General_CP1_CI_AS",
    "SQL_Latin1_General_CP1_CS_AS",
];

/// The heads of the name space the sweep enumerates.
const NAME_SPACE_HEADS: [&str; 4] = [
    "SQL_Latin1_General_CP1",
    "Latin1_General",
    "Latin1_General_90",
    "Latin1_General_100",
];

/// Every name of the space: a head, then one choice on each of the seven suffix axes.
/// 4 × 3 × 3 × 2 × 2 × 3 × 2 × 2 = 1 728 names, the four bare heads included.
fn name_space() -> Vec<String> {
    let axes: [&[&str]; 7] = [
        &["", "_CI", "_CS"],
        &["", "_AI", "_AS"],
        &["", "_KS"],
        &["", "_WS"],
        &["", "_BIN", "_BIN2"],
        &["", "_SC"],
        &["", "_UTF8"],
    ];
    let mut names = Vec::new();
    for head in NAME_SPACE_HEADS {
        let mut suffixes = vec![String::new()];
        for axis in axes {
            suffixes = suffixes
                .iter()
                .flat_map(|prefix| axis.iter().map(move |part| format!("{prefix}{part}")))
                .collect();
        }
        names.extend(suffixes.into_iter().map(|suffix| format!("{head}{suffix}")));
    }
    names
}

/// The sweep: over the whole name space, `parse` says yes exactly where the server
/// does. Zero false accept **and** zero false reject; the second matters most, since a
/// name the server has but VaubanDB refuses breaks a query that used to work.
#[test]
fn parse_accepts_exactly_the_names_the_server_has() {
    let space = name_space();
    assert_eq!(space.len(), 1728, "the enumerated space");
    let real: std::collections::BTreeSet<&str> = SERVER_COLLATIONS.iter().copied().collect();
    assert_eq!(real.len(), 72, "the listed names are distinct");

    let mut false_accepts = Vec::new();
    let mut false_rejects = Vec::new();
    for name in &space {
        let accepted = Collation::parse(name).is_ok();
        match (accepted, real.contains(name.as_str())) {
            (true, false) => false_accepts.push(name.clone()),
            (false, true) => false_rejects.push(name.clone()),
            _ => {}
        }
    }
    assert!(
        false_rejects.is_empty(),
        "{} names the server has are refused: {:?}",
        false_rejects.len(),
        false_rejects
    );
    assert!(
        false_accepts.is_empty(),
        "{} names the server has not are accepted: {:?}",
        false_accepts.len(),
        &false_accepts[..false_accepts.len().min(20)]
    );
}

/// The name space is not made of structures: `SQL_Latin1_General_CP1_BIN` and
/// `Latin1_General_BIN` parse to the *same* five bytes, and only the second one
/// exists. A grammar that only looked at the parsed collation could not tell them
/// apart, so the rejection has to happen on the written name.
#[test]
fn the_two_binary_twins_share_their_bytes_and_not_their_fate() {
    let windows = Collation::parse("Latin1_General_BIN").expect("the server has it");
    assert_eq!(
        Collation::parse("SQL_Latin1_General_CP1_BIN")
            .expect_err("the server answers 448")
            .number,
        448
    );
    // The five bytes the accepted twin yields, and the very same five the refused one
    // used to yield: the `SQL_` head changes no field here, since `0x1F` owns no
    // `SortId` and the legacy family has no version either.
    assert_eq!(
        windows,
        Collation {
            lcid: 0x0409,
            // 0x0F, everything ignored, plus fBinary 0x10.
            flags: 0x1F,
            version: 0,
            sort_id: 0,
        }
    );
}

#[test]
fn default_collation_fields() {
    assert_eq!(
        Collation::DEFAULT,
        Collation {
            lcid: 0x0409,
            flags: 0x0D,
            version: 0,
            sort_id: 52,
        }
    );
}

#[test]
fn parse_matches_default_constant() {
    assert_eq!(
        Collation::parse("SQL_Latin1_General_CP1_CI_AS"),
        Ok(Collation::DEFAULT)
    );
}

#[test]
fn compare_outside_code_page_1252() {
    let latin1 = Collation::DEFAULT;

    // Two characters with no CP1252 byte: ordered by code point, and after every
    // character of the weight table, the last Latin letters included.
    assert_eq!(latin1.compare("\u{4E2D}", "\u{4E2E}"), Ordering::Less);
    assert_eq!(latin1.compare("\u{4E2E}", "\u{4E2D}"), Ordering::Greater);
    assert_eq!(latin1.compare("\u{4E2D}", "z"), Ordering::Greater);
    assert_eq!(latin1.compare("\u{4E2D}", "ÿ"), Ordering::Greater);
    assert_eq!(latin1.compare("\u{4E2D}", "\u{4E2D}"), Ordering::Equal);

    // The conversion the table is indexed by.
    assert_eq!(super::cp1252_byte('A'), Some(0x41));
    assert_eq!(super::cp1252_byte('é'), Some(0xE9));
    assert_eq!(super::cp1252_byte('€'), Some(0x80));
    assert_eq!(super::cp1252_byte('Œ'), Some(0x8C));
    assert_eq!(super::cp1252_byte('Ÿ'), Some(0x9F));
    assert_eq!(super::cp1252_byte('\u{4E2D}'), None);
    // U+0080..=U+009F are C1 control characters, not code page 1252 characters.
    assert_eq!(super::cp1252_byte('\u{80}'), None);
}

#[test]
fn weights_are_case_insensitive_and_ordered_by_group() {
    let row = |table: &[super::Weights; 256], c: char| {
        table[super::cp1252_byte(c).expect("in the code page") as usize]
    };
    // A case pair of the code page shares its primary and accent weights, and is told
    // apart by the case weights only.
    for table in [
        &super::SQL_CP1_WEIGHTS,
        &super::WINDOWS_WEIGHTS,
        &super::WINDOWS_100_WEIGHTS,
    ] {
        for (upper, lower) in [('A', 'a'), ('Z', 'z'), ('É', 'é'), ('Æ', 'æ'), ('Ø', 'ø')] {
            let (upper, lower) = (row(table, upper), row(table, lower));
            assert_eq!(upper.primary, lower.primary);
            assert_eq!(upper.accent, lower.accent);
            assert_ne!(upper.case_accent, lower.case_accent);
        }
    }
    // Upper case first under the code page table, lower case first under Windows.
    assert!(
        row(&super::SQL_CP1_WEIGHTS, 'A').case_accent
            < row(&super::SQL_CP1_WEIGHTS, 'a').case_accent
    );
    assert!(
        row(&super::WINDOWS_WEIGHTS, 'a').case_accent
            < row(&super::WINDOWS_WEIGHTS, 'A').case_accent
    );

    // The four groups of primary weights of the code page table, in order: control,
    // punctuation, digit, letter.
    let primary = |c: char| row(&super::SQL_CP1_WEIGHTS, c).primary;
    assert!(primary('\t') < primary(' '));
    assert!(primary(' ') < primary('-'));
    assert!(primary('-') < primary('0'));
    assert!(primary('9') < primary('A'));
    assert!(primary('A') < primary('B'));
    // An accent shares the primary weight of its base letter and not its accent weight.
    assert_eq!(primary('e'), primary('é'));
    assert_ne!(
        row(&super::SQL_CP1_WEIGHTS, 'e').accent,
        row(&super::SQL_CP1_WEIGHTS, 'é').accent
    );
}
