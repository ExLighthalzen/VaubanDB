//! Integration tests of `Collation`: `parse` reads a collation name, `compare`,
//! `compare_char` and `find` order and search strings under
//! `SQL_Latin1_General_CP1_CI_AS`.

use std::cmp::Ordering::{Equal, Greater, Less};

use vauban_types::Collation;

// Bit values of the `flags` field, repeated here so that the test is readable without
// opening the crate ([MS-TDS] 2.2.5.1.2, rule `ColFlags`).
const IGNORE_CASE: u8 = 0x01;
const IGNORE_ACCENT: u8 = 0x02;
const IGNORE_KANA: u8 = 0x04;
const IGNORE_WIDTH: u8 = 0x08;
const BINARY2: u8 = 0x20;
const UTF8: u8 = 0x40;

/// Parses `name` or fails the test with the error the engine would send to a client.
fn parse(name: &str) -> Collation {
    match Collation::parse(name) {
        Ok(collation) => collation,
        Err(error) => panic!("`{name}` should parse, got {error}"),
    }
}

#[test]
fn parse_default_collation() {
    assert_eq!(parse("SQL_Latin1_General_CP1_CI_AS"), Collation::DEFAULT);
    // The name is matched without regard to ASCII case, as `COLLATE` is.
    assert_eq!(parse("sql_latin1_general_cp1_ci_as"), Collation::DEFAULT);
    assert_eq!(parse("SQL_LATIN1_GENERAL_CP1_CI_AS"), Collation::DEFAULT);
}

#[test]
fn parse_flags() {
    // `_CS` clears "ignore case", `_AS` clears "ignore accent"; kana and width stay set.
    assert_eq!(
        parse("SQL_Latin1_General_CP1_CS_AS").flags,
        IGNORE_KANA | IGNORE_WIDTH
    );
    assert_eq!(parse("SQL_Latin1_General_CP1_CS_AS").flags, 0x0C);

    // `_CI_AI` ignores everything the four sensitivity bits cover.
    assert_eq!(parse("SQL_Latin1_General_CP1_CI_AI").flags, 0x0F);

    // A Windows collation: no `SortId`, no version, and the `_BIN2` bit.
    let bin2 = parse("Latin1_General_BIN2");
    assert_eq!(bin2.flags & BINARY2, BINARY2);
    assert_eq!(bin2.version, 0);
    assert_eq!(bin2.sort_id, 0);
    assert_eq!(bin2.lcid, 0x0409);

    // `_100_` is version 2 on the wire; `vauban-tds` encodes this very collation as
    // `09 04 D0 20 00`.
    let windows_100 = parse("Latin1_General_100_CI_AS");
    assert_eq!(windows_100.version, 2);
    assert_eq!(windows_100.lcid, 0x0409);
    assert_eq!(windows_100.sort_id, 0);
    assert_eq!(windows_100.flags, IGNORE_CASE | IGNORE_KANA | IGNORE_WIDTH);

    // `_SC` carries no bit; `_UTF8` does.
    let utf8 = parse("Latin1_General_100_CI_AS_SC_UTF8");
    assert_eq!(utf8.flags & UTF8, UTF8);
    assert_eq!(utf8.flags, windows_100.flags | UTF8);
    assert_eq!(utf8.version, 2);
}

#[test]
fn parse_ks_and_ws_clear_their_bit() {
    // [MS-TDS] 2.2.5.1.2 Collation Rule Definition gives the order of the two bits in the
    // `ColFlags` rule -- and *not* in the list of `BIT` declarations just above it, which
    // spells `fIgnoreWidth` before `fIgnoreKana`:
    //
    //     ColFlags = fIgnoreCase fIgnoreAccent fIgnoreKana
    //                fIgnoreWidth fBinary fBinary2 fUTF8
    //                FRESERVEDBIT
    //
    // followed by the note "ColFlags is represented in least significant bit order", which
    // section 2.2.5.1.1 defines as "the first listed flag is placed in the least
    // significant bit position". So fIgnoreKana is 0x04 and fIgnoreWidth is 0x08.
    // The Windows head, not the legacy one: `SQL_Latin1_General_CP1_CI_AS_KS` and its two
    // companions below are **not** collations (`COLLATIONPROPERTY` answers `NULL`,
    // `COLLATE` answers 448), and `parse` refuses them. The three `Latin1_General_*`
    // spellings used here do exist, and they exercise the same two bits with the same
    // expectations.
    let ks = parse("Latin1_General_CI_AS_KS");
    assert_eq!(ks.flags & IGNORE_KANA, 0, "_KS clears fIgnoreKana (0x04)");
    assert_eq!(
        ks.flags & IGNORE_WIDTH,
        IGNORE_WIDTH,
        "_KS leaves fIgnoreWidth (0x08) set"
    );
    assert_eq!(ks.flags, IGNORE_CASE | IGNORE_WIDTH);

    let ws = parse("Latin1_General_CI_AS_WS");
    assert_eq!(ws.flags & IGNORE_WIDTH, 0, "_WS clears fIgnoreWidth (0x08)");
    assert_eq!(
        ws.flags & IGNORE_KANA,
        IGNORE_KANA,
        "_WS leaves fIgnoreKana (0x04) set"
    );
    assert_eq!(ws.flags, IGNORE_CASE | IGNORE_KANA);

    // Both together clear both, which is what makes 0x0D of the default collation
    // unambiguous only once the two names above are distinguished.
    let ks_ws = parse("Latin1_General_CI_AS_KS_WS");
    assert_eq!(ks_ws.flags, IGNORE_CASE);
    assert_eq!(
        Collation::DEFAULT.flags & (IGNORE_KANA | IGNORE_WIDTH),
        0x0C
    );
    assert_eq!(Collation::DEFAULT.flags & IGNORE_ACCENT, 0);
}

#[test]
fn parse_rejects_unknown() {
    let error = Collation::parse("Klingon_CI_AS").expect_err("unknown locale");
    assert_eq!(error.number, 448);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert_eq!(error.message, "Unknown collation 'Klingon_CI_AS'.");

    // An empty name and a locale without any suffix are not collation names either.
    for name in ["", "Latin1_General", "SQL_Latin1_General_CP1", "SQL"] {
        let error = Collation::parse(name).expect_err(name);
        assert_eq!(error.number, 448, "{name}");
        assert_eq!(
            error.message,
            format!("Unknown collation '{name}'."),
            "{name}"
        );
    }

    // Suffixes out of order, unknown suffixes, unknown code pages and unknown versions.
    for name in [
        "Latin1_General_AS_CI",
        "Latin1_General_CI_AS_XX",
        "SQL_Latin1_General_CP850_CI_AS",
        "Latin1_General_140_CI_AS",
        "Latin1_General_CI_CS",
    ] {
        assert_eq!(
            Collation::parse(name).expect_err(name).number,
            448,
            "{name}"
        );
    }
}

#[test]
fn parse_sort_id_of_sql_collations() {
    // COLLATIONPROPERTY(..., 'SortId'): 52 for _CI_AS, 51 for _CS_AS, 54 for _CI_AI, and
    // 0 (no SortId) for every Windows collation.
    assert_eq!(parse("SQL_Latin1_General_CP1_CI_AS").sort_id, 52);
    assert_eq!(parse("SQL_Latin1_General_CP1_CS_AS").sort_id, 51);
    assert_eq!(parse("SQL_Latin1_General_CP1_CI_AI").sort_id, 54);
    assert_eq!(parse("Latin1_General_CI_AS").sort_id, 0);
    assert_eq!(parse("Latin1_General_100_CI_AS").sort_id, 0);
}

#[test]
fn compare_is_case_insensitive_and_accent_sensitive() {
    let latin1 = Collation::DEFAULT;

    // `_CI`: case never counts, at any level.
    assert_eq!(latin1.compare("abc", "ABC"), Equal);
    assert_eq!(latin1.compare("Hello World", "hELLO wORLD"), Equal);

    // `_AS`: an accented letter is neither equal to its base letter nor sorted away from
    // it: it slips between its base letter and the next one.
    assert_eq!(latin1.compare("e", "é"), Less);
    assert_eq!(latin1.compare("é", "f"), Less);
    assert_eq!(latin1.compare("a", "á"), Less);
    assert_eq!(latin1.compare("á", "b"), Less);
    assert_eq!(latin1.compare("f", "é"), Greater);

    // The accents of one letter are ordered among themselves, and the two characters no
    // rule of the plan places: `æ` before `b`, `ß` before `t`.
    assert_eq!(latin1.compare("à", "á"), Less);
    assert_eq!(latin1.compare("â", "ä"), Less);
    assert_eq!(latin1.compare("æ", "b"), Less);
    assert_eq!(latin1.compare("ß", "t"), Less);

    // Case and accent together: the case is dropped first, so 'é' beats 'E'.
    assert_eq!(latin1.compare("é", "E"), Greater);
    assert_eq!(latin1.compare("É", "e"), Greater);
    assert_eq!(latin1.compare("É", "é"), Equal);
}

#[test]
fn compare_orders_digits_before_letters() {
    let latin1 = Collation::DEFAULT;

    // Digits sort before letters.
    assert_eq!(latin1.compare("1", "a"), Less);
    assert_eq!(latin1.compare("a", "1"), Greater);

    // An inner space is significant and sorts before the digits: the group order of the
    // weight table.
    assert_eq!(latin1.compare(" ", "1"), Less);
    assert_eq!(latin1.compare("a b", "ab"), Less);

    // Punctuation sorts before the letters too (the `varchar` order: the `nvarchar` order
    // says the opposite, and the engine follows the `varchar` one).
    assert_eq!(latin1.compare("a-b", "ab"), Less);

    assert_eq!(latin1.compare("a", "z"), Less);
    assert_eq!(latin1.compare("z", "a"), Greater);
}

#[test]
fn compare_ignores_trailing_spaces() {
    let latin1 = Collation::DEFAULT;

    // Trailing spaces are padding.
    assert_eq!(latin1.compare("abc", "abc   "), Equal);
    assert_eq!(latin1.compare("abc   ", "abc"), Equal);
    assert_eq!(latin1.compare("", "   "), Equal);

    // Leading spaces are significant, and only U+0020 is padding.
    assert_eq!(latin1.compare(" a", "a"), Less);
    assert_eq!(latin1.compare("a", " a"), Greater);
    assert_eq!(latin1.compare("abc", "abc\t"), Less);

    // A string that is a prefix of the other one is the smaller one.
    assert_eq!(latin1.compare("ab", "abc"), Less);
    assert_eq!(latin1.compare("", "a"), Less);
}

#[test]
fn compare_char_matches_compare() {
    let latin1 = Collation::DEFAULT;

    // Space is left out on purpose: `compare` trims a trailing space away, `compare_char`
    // weighs it, and the two disagree on `' '` against a control character. That is
    // documented on `Collation::compare_char`.
    let pairs = [
        ('a', 'A'),
        ('A', 'a'),
        ('a', 'b'),
        ('b', 'a'),
        ('e', 'é'),
        ('é', 'e'),
        ('é', 'E'),
        ('a', 'á'),
        ('à', 'á'),
        ('â', 'ä'),
        ('æ', 'b'),
        ('ß', 't'),
        ('1', 'a'),
        ('0', '9'),
        ('-', 'b'),
        ('-', '1'),
        ('z', 'é'),
        ('Z', 'z'),
        ('€', 'a'),
        ('ÿ', 'Ÿ'),
        ('\u{4E2D}', 'z'),
        ('\u{4E2D}', '\u{4E2E}'),
        ('\u{4E2D}', '\u{4E2D}'),
    ];
    for (a, b) in pairs {
        assert_eq!(
            latin1.compare_char(a, b),
            latin1.compare(&a.to_string(), &b.to_string()),
            "{a} against {b}"
        );
    }

    assert_eq!(latin1.compare_char('a', 'A'), Equal);
    assert_eq!(latin1.compare_char('e', 'é'), Less);
}

#[test]
fn find_is_one_based_and_collated() {
    let latin1 = Collation::DEFAULT;

    // CHARINDEX('cd', 'abcdef') is 3, and the case is ignored.
    assert_eq!(latin1.find("abcdef", "CD", 1), Some(3));
    assert_eq!(latin1.find("ABCDEF", "cd", 1), Some(3));
    assert_eq!(latin1.find("abcdef", "z", 1), None);

    // The search starts at `start`, counted from 1; 0 means 1.
    assert_eq!(latin1.find("abcabc", "b", 3), Some(5));
    assert_eq!(latin1.find("abcabc", "b", 1), Some(2));
    assert_eq!(latin1.find("abcdef", "cd", 0), Some(3));
    assert_eq!(latin1.find("abcdef", "cd", 3), Some(3));
    assert_eq!(latin1.find("abcdef", "cd", 4), None);
    assert_eq!(latin1.find("abcdef", "cd", 99), None);

    // Accents are significant, and a needle longer than the haystack is absent.
    assert_eq!(latin1.find("abc", "é", 1), None);
    assert_eq!(latin1.find("ÉLÈVE", "è", 1), Some(3));
    assert_eq!(latin1.find("abc", "abcd", 1), None);

    // An empty needle is never found: CHARINDEX('', 'abc') and CHARINDEX('', 'abc', 2)
    // are 0, and 0 is "absent".
    assert_eq!(latin1.find("abc", "", 1), None);
    assert_eq!(latin1.find("abc", "", 2), None);
    assert_eq!(latin1.find("", "", 1), None);
}
