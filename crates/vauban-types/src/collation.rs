//! Collation identifier, as SQL Server names and transmits it.

use std::cmp::Ordering;
use std::iter::Peekable;

use vauban_errors::SqlResult;

use crate::code_page;
use crate::errors;

/// A SQL Server collation, decomposed as in the 5-byte TDS `Collation` rule
/// (`[MS-TDS]` 2.2.5.1.2): a 20-bit LCID, 8 bits of comparison flags, a 4-bit version and
/// a `SortId`. Encoding to and decoding from the wire belong to the `tds` module; name
/// parsing is [`Collation::parse`] and comparison rules live in `compare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Collation {
    /// Windows locale identifier, 20 bits (e.g. `0x0409` for `en-US`).
    pub lcid: u32,
    /// Comparison flags, 8 bits: ignore case, ignore accent, ignore kana, ignore width,
    /// binary, binary2, UTF-8, as laid out by `[MS-TDS]`.
    ///
    /// The order of the middle two is the one `[MS-TDS]` 2.2.5.1.2 gives in the `ColFlags`
    /// production, *not* the order in which the same section declares the `BIT` fields
    /// just above it (which lists `fIgnoreWidth` before `fIgnoreKana`):
    ///
    /// ```text
    /// ColFlags = fIgnoreCase fIgnoreAccent fIgnoreKana
    ///            fIgnoreWidth fBinary fBinary2 fUTF8
    ///            FRESERVEDBIT
    /// ```
    ///
    /// with the note "ColFlags is represented in least significant bit order", i.e. the
    /// first flag listed sits in the least significant bit (`[MS-TDS]` 2.2.5.1.1). Hence
    /// `fIgnoreKana` = `0x04` and `fIgnoreWidth` = `0x08`.
    pub flags: u8,
    /// Collation version, 4 bits (`0` for the legacy `SQL_*` collations).
    pub version: u8,
    /// `SortId` of the legacy `SQL_*` collations; `0` for Windows collations.
    pub sort_id: u8,
}

/// `fIgnoreCase`: the collation is case-insensitive (name without `_CS`).
const FLAG_IGNORE_CASE: u8 = 0x01;
/// `fIgnoreAccent`: the collation is accent-insensitive (name without `_AS`).
const FLAG_IGNORE_ACCENT: u8 = 0x02;
/// `fIgnoreKana`: the collation is kana-type-insensitive (name without `_KS`).
const FLAG_IGNORE_KANA: u8 = 0x04;
/// `fIgnoreWidth`: the collation is width-insensitive (name without `_WS`).
const FLAG_IGNORE_WIDTH: u8 = 0x08;
/// `fBinary`: `_BIN`, code-point-then-byte ordering.
const FLAG_BINARY: u8 = 0x10;
/// `fBinary2`: `_BIN2`, pure code point ordering.
const FLAG_BINARY2: u8 = 0x20;
/// `fUTF8`: `_UTF8`, the character data is encoded in UTF-8.
const FLAG_UTF8: u8 = 0x40;

/// Flags of a name that carries no sensitivity suffix at all: everything is ignored until
/// a `_CS`, `_AS`, `_KS` or `_WS` clears its bit.
const FLAGS_ALL_INSENSITIVE: u8 =
    FLAG_IGNORE_CASE | FLAG_IGNORE_ACCENT | FLAG_IGNORE_KANA | FLAG_IGNORE_WIDTH;
/// Flags of `_CI_AS`, the suffix of [`Collation::DEFAULT`].
const FLAGS_CI_AS: u8 = FLAG_IGNORE_CASE | FLAG_IGNORE_KANA | FLAG_IGNORE_WIDTH;
/// Flags of `_CS_AS`.
const FLAGS_CS_AS: u8 = FLAG_IGNORE_KANA | FLAG_IGNORE_WIDTH;
/// Flags of `_CI_AI`.
const FLAGS_CI_AI: u8 = FLAGS_ALL_INSENSITIVE;

/// LCID of `Latin1_General` and `SQL_Latin1_General`: 1033 (`en-US`), what
/// `COLLATIONPROPERTY('SQL_Latin1_General_CP1_CI_AS', 'LCID')` reports.
const LCID_LATIN1_GENERAL: u32 = 0x0409;

impl Collation {
    /// SQL Server `SQL_Latin1_General_CP1_CI_AS`, the default collation of a fresh
    /// instance. On the wire these are the bytes `09 04 D0 00 34`: LCID `0x0409`, flags
    /// `0x0D` (ignore case, ignore kana, ignore width), version `0`, `SortId` `52`.
    pub const DEFAULT: Collation = Collation {
        lcid: 0x0409,
        flags: 0x0D,
        version: 0,
        sort_id: 52,
    };

    /// Parses a collation name into its five wire bytes, or fails with error 448
    /// (severity 16, state 1).
    ///
    /// The name is matched case-insensitively (ASCII). `Latin1_General` and
    /// `SQL_Latin1_General_CP1` are the known locales; anything else is error 448.
    ///
    /// This is **not** a loose grammar: it accepts exactly the 72 names of the two
    /// locales that `fn_helpcollations()` lists, and refuses the 1 656 other spellings of
    /// the space made of the four heads below crossed with the seven suffix axes
    /// (`tests::parse_accepts_exactly_the_names_the_server_has`).
    ///
    /// What the 72 names are made of, head by head:
    ///
    /// - `SQL_Latin1_General_CP1_` has **three** members and no more: `_CI_AI`, `_CI_AS`
    ///   and `_CS_AS`, i.e. exactly the three that own a [`sql_sort_id`]. `_CS_AI` is
    ///   missing, and so is every `_KS`, `_WS`, `_BIN`, `_BIN2`, `_SC` and `_UTF8`
    ///   spelling of the legacy family;
    /// - `Latin1_General_` has 18: `_BIN`, `_BIN2`, and `CI`/`CS` × `AI`/`AS` with an
    ///   optional `_KS` and an optional `_WS`;
    /// - `Latin1_General_100_` has 51: the same 16 sensitivity names, each with an
    ///   optional `_SC` then an optional `_UTF8` after it, plus `_BIN`, `_BIN2` and
    ///   `_BIN2_UTF8`;
    /// - `Latin1_General_90_` has **none**: no `_90_` collation is installed, and every
    ///   one of its 432 spellings answers 448.
    ///
    /// Four rules summarise it, and the fourth carries its own exception: `CI`/`CS` and
    /// `AI`/`AS` are required *together*; `_BIN` and `_BIN2` come *alone* instead; `_SC`
    /// only exists on a `_100_` name; `_UTF8` only exists after `_SC`, except in
    /// `Latin1_General_100_BIN2_UTF8`. The suffixes keep their order — `CI`/`CS`, `AI`/`AS`,
    /// `KS`, `WS`, `BIN`/`BIN2`, `SC`, `UTF8` — and a head alone is not a name.
    ///
    /// `_SC` (supplementary characters) has **no bit** in the 5-byte rule, so it is
    /// checked and then dropped: `Latin1_General_100_CI_AS_SC` and
    /// `Latin1_General_100_CI_AS` parse to the same five bytes. The name cannot be spelled
    /// back in full: a deliberate difference from SQL Server.
    ///
    /// A collation VaubanDB cannot yet *compare* is still parsed: `_CS_AS`, `_BIN2` and
    /// `Latin1_General_100_*` are accepted here, and `compare` treats them like the
    /// default collation. That gap is deliberate.
    ///
    /// ```
    /// use vauban_types::Collation;
    ///
    /// assert_eq!(
    ///     Collation::parse("SQL_Latin1_General_CP1_CI_AS"),
    ///     Ok(Collation::DEFAULT)
    /// );
    /// assert_eq!(Collation::parse("Klingon_CI_AS").unwrap_err().number, 448);
    /// // A well-formed name the server does not have is refused just the same.
    /// assert_eq!(
    ///     Collation::parse("Latin1_General_CI_AS_SC").unwrap_err().number,
    ///     448
    /// );
    /// assert!(Collation::parse("Latin1_General_100_CI_AS_SC").is_ok());
    /// ```
    pub fn parse(name: &str) -> SqlResult<Self> {
        Self::parse_name(name).ok_or_else(|| errors::invalid_collation(name))
    }

    /// The grammar of [`Collation::parse`], with `None` for "this is not a name the server
    /// has". Splitting the failure out keeps a single place where error 448 is built.
    ///
    /// The name is read in two steps: the head, then the seven suffix axes in their fixed
    /// order. Only then is the combination checked against [`Head::allows`], because a
    /// spelling is legal or not as a *whole*: `_SC` alone is fine on a `_100_` name and
    /// nowhere else, and `_UTF8` alone is fine after `_SC` and in one binary name.
    fn parse_name(name: &str) -> Option<Self> {
        let mut parts = name.split('_').peekable();

        let is_sql = take(&mut parts, "SQL");
        if !parts.next()?.eq_ignore_ascii_case("Latin1")
            || !parts.next()?.eq_ignore_ascii_case("General")
        {
            return None;
        }

        let head = if is_sql {
            // A SQL collation names its code page where a Windows one names its version.
            // CP1 (code page 1252) is the one code page of the family.
            if !parts.next()?.eq_ignore_ascii_case("CP1") {
                return None;
            }
            Head::Sql
        } else {
            let digits = parts.next_if(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
            match digits {
                None => Head::Windows,
                // `Latin1_General_100_CI_AS` is `version: 2` on the wire, as the encoder of
                // `vauban-tds` (`types::collation`) expects it.
                Some("100") => Head::Windows100,
                // `_90_` is spelled by no installed collation: the whole head raises 448.
                // 140 and beyond are not installed either, so no version number is
                // invented for them.
                Some(_) => return None,
            }
        };

        let mut flags = FLAGS_ALL_INSENSITIVE;
        // `Some(true)` for `_CI`, `Some(false)` for `_CS`, `None` when neither is written.
        let ignore_case = if take(&mut parts, "CI") {
            Some(true)
        } else if take(&mut parts, "CS") {
            flags &= !FLAG_IGNORE_CASE;
            Some(false)
        } else {
            None
        };
        // Likewise `Some(true)` for `_AI` and `Some(false)` for `_AS`.
        let ignore_accent = if take(&mut parts, "AI") {
            Some(true)
        } else if take(&mut parts, "AS") {
            flags &= !FLAG_IGNORE_ACCENT;
            Some(false)
        } else {
            None
        };
        let kana = take(&mut parts, "KS");
        if kana {
            flags &= !FLAG_IGNORE_KANA;
        }
        let width = take(&mut parts, "WS");
        if width {
            flags &= !FLAG_IGNORE_WIDTH;
        }
        // `take` compares whole segments, so `BIN` never swallows the `BIN2` of a name.
        let binary = if take(&mut parts, "BIN") {
            flags |= FLAG_BINARY;
            true
        } else if take(&mut parts, "BIN2") {
            flags |= FLAG_BINARY2;
            true
        } else {
            false
        };
        // `_SC` has no bit in the 5-byte rule: it is checked here and then forgotten.
        let supplementary = take(&mut parts, "SC");
        let utf8 = take(&mut parts, "UTF8");
        if utf8 {
            flags |= FLAG_UTF8;
        }

        // A segment left over: unknown, or written out of order.
        if parts.next().is_some() {
            return None;
        }

        let written = Suffixes {
            ignore_case,
            ignore_accent,
            kana,
            width,
            binary: binary.then_some(flags & FLAG_BINARY2 != 0),
            supplementary,
            utf8,
        };
        if !head.allows(&written) {
            return None;
        }

        Some(Collation {
            lcid: LCID_LATIN1_GENERAL,
            flags,
            version: head.version(),
            sort_id: if is_sql { sql_sort_id(flags) } else { 0 },
        })
    }

    /// Orders two strings the way `SQL_Latin1_General_CP1_CI_AS` does.
    ///
    /// 1. Trailing spaces (U+0020) of both operands are ignored: SQL Server blank-pads the
    ///    shorter one, which amounts to the same. Leading and inner spaces are significant.
    /// 2. The **primary** weights of [`WEIGHTS`] are compared character by character; a
    ///    string that is a prefix of the other one is the smaller one.
    /// 3. On a tie, the **secondary** weights are compared the same way and in the same
    ///    order: that is what makes the collation accent-sensitive (`_AS`).
    /// 4. Case never takes part (`_CI`): `'a'` and `'A'` share both weights, at every
    ///    level.
    ///
    /// A character with no code page 1252 byte sorts after every character of the table,
    /// ordered by code point.
    ///
    /// # What this really implements
    ///
    /// Only the default collation, and `self` is deliberately not consulted: the `_CS_AS`,
    /// `_BIN2` and `Latin1_General_100_*` names [`Collation::parse`] accepts are compared
    /// with the rules above, which is wrong for them. That is an assumed gap, not an
    /// oversight; the whole engine assumes the instance collation for now.
    ///
    /// A second, subtler limit: a `SQL_*` collation sorts non-Unicode data (`char`,
    /// `varchar`) with the sort table of its code page, and Unicode data (`nchar`,
    /// `nvarchar`) with the Windows rules of `Latin1_General`. The two orders differ, and
    /// [`SqlString`](crate::SqlString) does not say which side a value is on. This is the
    /// **`varchar`** order: `'a-b' < 'ab'` and `'à' < 'á'` are true for `varchar` and
    /// false for `nvarchar`.
    ///
    /// ```
    /// use std::cmp::Ordering;
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(latin1.compare("abc", "ABC   "), Ordering::Equal);
    /// assert_eq!(latin1.compare("e", "é"), Ordering::Less);
    /// assert_eq!(latin1.compare("é", "f"), Ordering::Less);
    /// ```
    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        let a = a.trim_end_matches(' ');
        let b = b.trim_end_matches(' ');
        primaries(a)
            .cmp(primaries(b))
            .then_with(|| secondaries(a).cmp(secondaries(b)))
    }

    /// Compares two characters under the same rules as [`Collation::compare`], without
    /// allocating: primary weight first, secondary weight on a tie.
    ///
    /// This is the primitive `LIKE` (character classes and ranges) and the string
    /// functions of `sysfn` build on; they do not reimplement the collation.
    ///
    /// The only difference with `compare` on two one-character strings is the space:
    /// `compare(" ", "\t")` trims the space away and answers `Less`, while
    /// `compare_char(' ', '\t')` compares the two weights and answers `Greater`, the space
    /// sorting after the control characters. Trailing-blank padding is a property of
    /// string operands, not of characters.
    ///
    /// ```
    /// use std::cmp::Ordering;
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(latin1.compare_char('a', 'A'), Ordering::Equal);
    /// assert_eq!(latin1.compare_char('e', 'é'), Ordering::Less);
    /// ```
    pub fn compare_char(&self, a: char, b: char) -> Ordering {
        let (primary_a, secondary_a) = sort_key(a);
        let (primary_b, secondary_b) = sort_key(b);
        primary_a
            .cmp(&primary_b)
            .then(secondary_a.cmp(&secondary_b))
    }

    /// Position of the first occurrence of `needle` in `haystack` under this collation,
    /// **1-based**, or `None` when it does not occur. This is the primitive of `CHARINDEX`.
    ///
    /// The search starts at the character `start` (1-based; `0` is treated as `1`, as
    /// `CHARINDEX` does with a non-positive start). Characters are matched with
    /// [`Collation::compare_char`], so the case is ignored and the accents are not:
    /// `find("ABCDEF", "cd", 1)` is `Some(3)` and `find("abc", "é", 1)` is `None`.
    ///
    /// An **empty** `needle` is never found (`tests/collation.rs`): `CHARINDEX('', 'abc')`
    /// and `CHARINDEX('', 'abc', 2)` return `0`, which is what `CHARINDEX` returns when the
    /// needle is absent.
    ///
    /// No blank padding happens here, unlike in [`Collation::compare`]: the needle is
    /// matched literally, trailing spaces included.
    ///
    /// ```
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(latin1.find("abcdef", "CD", 1), Some(3));
    /// assert_eq!(latin1.find("abcabc", "b", 3), Some(5));
    /// assert_eq!(latin1.find("abcdef", "z", 1), None);
    /// ```
    pub fn find(&self, haystack: &str, needle: &str, start: usize) -> Option<usize> {
        if needle.is_empty() {
            return None;
        }
        let haystack: Vec<char> = haystack.chars().collect();
        let needle: Vec<char> = needle.chars().collect();
        // A needle longer than the haystack cannot occur anywhere.
        let last_start = haystack.len().checked_sub(needle.len())?;
        // `start` counts from 1; `start = 0` means "from the beginning", like CHARINDEX.
        let first_start = start.max(1) - 1;
        (first_start..=last_start)
            .find(|&at| {
                haystack[at..at + needle.len()]
                    .iter()
                    .zip(&needle)
                    .all(|(h, n)| self.compare_char(*h, *n) == Ordering::Equal)
            })
            .map(|at| at + 1)
    }
}

/// Sort weights of `SQL_Latin1_General_CP1_CI_AS`, indexed by **code page 1252 byte** and
/// not by code point: 27 of the characters CP1252 puts in its `0x80`-`0x9F` range (`€`
/// U+20AC, `Œ` U+0152, `’` U+2019...) live far beyond `0xFF` and would not fit a 256-entry
/// index. [`cp1252_byte`] does the conversion.
///
/// Each entry is `(primary, secondary)`. The primary weights follow one another in this
/// order: the control characters, then the space and the punctuation in code page order,
/// then the ten digits, then the 26 base letters. A letter with a diacritic takes the
/// primary weight of its base letter and a secondary weight that separates it from its
/// sisters, ranked by CP1252 byte; a bare letter has secondary `0` and therefore sorts
/// first among its family. Upper and lower case share both weights (`_CI`).
///
/// **What is tested** (`tests/collation.rs`): `'ABC' = 'abc'`, `'e' <> 'é'`, `'a' < 'á'`,
/// `'á' < 'b'`, `'e' < 'é'`, `'é' < 'f'`, `'à' < 'á'`, `'â' < 'ä'`, `'æ' < 'b'`,
/// `'ß' < 't'`, `'a-b' < 'ab'`, `'1' < 'a'` and `CHARINDEX('cd', 'abcdef') = 3`. The
/// entries marked `extrapolated` have no test of their own: their weight follows the rules
/// above (code page order between families, byte order between sisters). `æ`, `ß`, `ø`,
/// `ð`, `þ` and `œ` are placed in the family of the letter they are usually filed under;
/// `æ < b` and `ß < t` are the two tested ones.
pub(crate) const WEIGHTS: [(u16, u8); 256] = [
    (1, 0),   // 0x00 NUL, extrapolated
    (2, 0),   // 0x01 control, extrapolated
    (3, 0),   // 0x02 control, extrapolated
    (4, 0),   // 0x03 control, extrapolated
    (5, 0),   // 0x04 control, extrapolated
    (6, 0),   // 0x05 control, extrapolated
    (7, 0),   // 0x06 control, extrapolated
    (8, 0),   // 0x07 control, extrapolated
    (9, 0),   // 0x08 control, extrapolated
    (10, 0),  // 0x09 tab, extrapolated
    (11, 0),  // 0x0A LF, extrapolated
    (12, 0),  // 0x0B control, extrapolated
    (13, 0),  // 0x0C control, extrapolated
    (14, 0),  // 0x0D CR, extrapolated
    (15, 0),  // 0x0E control, extrapolated
    (16, 0),  // 0x0F control, extrapolated
    (17, 0),  // 0x10 control, extrapolated
    (18, 0),  // 0x11 control, extrapolated
    (19, 0),  // 0x12 control, extrapolated
    (20, 0),  // 0x13 control, extrapolated
    (21, 0),  // 0x14 control, extrapolated
    (22, 0),  // 0x15 control, extrapolated
    (23, 0),  // 0x16 control, extrapolated
    (24, 0),  // 0x17 control, extrapolated
    (25, 0),  // 0x18 control, extrapolated
    (26, 0),  // 0x19 control, extrapolated
    (27, 0),  // 0x1A control, extrapolated
    (28, 0),  // 0x1B ESC, extrapolated
    (29, 0),  // 0x1C control, extrapolated
    (30, 0),  // 0x1D control, extrapolated
    (31, 0),  // 0x1E control, extrapolated
    (32, 0),  // 0x1F control, extrapolated
    (39, 0),  // 0x20 space
    (40, 0),  // 0x21 '!', extrapolated
    (41, 0),  // 0x22 '"', extrapolated
    (42, 0),  // 0x23 '#', extrapolated
    (43, 0),  // 0x24 '$', extrapolated
    (44, 0),  // 0x25 '%', extrapolated
    (45, 0),  // 0x26 '&', extrapolated
    (46, 0),  // 0x27 ''', extrapolated
    (47, 0),  // 0x28 '(', extrapolated
    (48, 0),  // 0x29 ')', extrapolated
    (49, 0),  // 0x2A '*', extrapolated
    (50, 0),  // 0x2B '+', extrapolated
    (51, 0),  // 0x2C ',', extrapolated
    (52, 0),  // 0x2D '-'
    (53, 0),  // 0x2E '.', extrapolated
    (54, 0),  // 0x2F '/', extrapolated
    (125, 0), // 0x30 '0', extrapolated
    (126, 0), // 0x31 '1'
    (127, 0), // 0x32 '2', extrapolated
    (128, 0), // 0x33 '3', extrapolated
    (129, 0), // 0x34 '4', extrapolated
    (130, 0), // 0x35 '5', extrapolated
    (131, 0), // 0x36 '6', extrapolated
    (132, 0), // 0x37 '7', extrapolated
    (133, 0), // 0x38 '8', extrapolated
    (134, 0), // 0x39 '9', extrapolated
    (55, 0),  // 0x3A ':', extrapolated
    (56, 0),  // 0x3B ';', extrapolated
    (57, 0),  // 0x3C '<', extrapolated
    (58, 0),  // 0x3D '=', extrapolated
    (59, 0),  // 0x3E '>', extrapolated
    (60, 0),  // 0x3F '?', extrapolated
    (61, 0),  // 0x40 '@', extrapolated
    (135, 0), // 0x41 'A'
    (136, 0), // 0x42 'B'
    (137, 0), // 0x43 'C'
    (138, 0), // 0x44 'D'
    (139, 0), // 0x45 'E'
    (140, 0), // 0x46 'F'
    (141, 0), // 0x47 'G', extrapolated
    (142, 0), // 0x48 'H', extrapolated
    (143, 0), // 0x49 'I', extrapolated
    (144, 0), // 0x4A 'J', extrapolated
    (145, 0), // 0x4B 'K', extrapolated
    (146, 0), // 0x4C 'L', extrapolated
    (147, 0), // 0x4D 'M', extrapolated
    (148, 0), // 0x4E 'N', extrapolated
    (149, 0), // 0x4F 'O', extrapolated
    (150, 0), // 0x50 'P', extrapolated
    (151, 0), // 0x51 'Q', extrapolated
    (152, 0), // 0x52 'R', extrapolated
    (153, 0), // 0x53 'S', extrapolated
    (154, 0), // 0x54 'T'
    (155, 0), // 0x55 'U', extrapolated
    (156, 0), // 0x56 'V', extrapolated
    (157, 0), // 0x57 'W', extrapolated
    (158, 0), // 0x58 'X', extrapolated
    (159, 0), // 0x59 'Y', extrapolated
    (160, 0), // 0x5A 'Z', extrapolated
    (62, 0),  // 0x5B '[', extrapolated
    (63, 0),  // 0x5C '\', extrapolated
    (64, 0),  // 0x5D ']', extrapolated
    (65, 0),  // 0x5E '^', extrapolated
    (66, 0),  // 0x5F '_', extrapolated
    (67, 0),  // 0x60 '`', extrapolated
    (135, 0), // 0x61 'a'
    (136, 0), // 0x62 'b'
    (137, 0), // 0x63 'c'
    (138, 0), // 0x64 'd'
    (139, 0), // 0x65 'e'
    (140, 0), // 0x66 'f'
    (141, 0), // 0x67 'g', extrapolated
    (142, 0), // 0x68 'h', extrapolated
    (143, 0), // 0x69 'i', extrapolated
    (144, 0), // 0x6A 'j', extrapolated
    (145, 0), // 0x6B 'k', extrapolated
    (146, 0), // 0x6C 'l', extrapolated
    (147, 0), // 0x6D 'm', extrapolated
    (148, 0), // 0x6E 'n', extrapolated
    (149, 0), // 0x6F 'o', extrapolated
    (150, 0), // 0x70 'p', extrapolated
    (151, 0), // 0x71 'q', extrapolated
    (152, 0), // 0x72 'r', extrapolated
    (153, 0), // 0x73 's', extrapolated
    (154, 0), // 0x74 't'
    (155, 0), // 0x75 'u', extrapolated
    (156, 0), // 0x76 'v', extrapolated
    (157, 0), // 0x77 'w', extrapolated
    (158, 0), // 0x78 'x', extrapolated
    (159, 0), // 0x79 'y', extrapolated
    (160, 0), // 0x7A 'z', extrapolated
    (68, 0),  // 0x7B '{', extrapolated
    (69, 0),  // 0x7C '|', extrapolated
    (70, 0),  // 0x7D '}', extrapolated
    (71, 0),  // 0x7E '~', extrapolated
    (33, 0),  // 0x7F DEL, extrapolated
    (72, 0),  // 0x80 '€', extrapolated
    (34, 0),  // 0x81 unassigned in CP1252, extrapolated
    (73, 0),  // 0x82 '‚', extrapolated
    (140, 1), // 0x83 'ƒ', extrapolated
    (74, 0),  // 0x84 '„', extrapolated
    (75, 0),  // 0x85 '…', extrapolated
    (76, 0),  // 0x86 '†', extrapolated
    (77, 0),  // 0x87 '‡', extrapolated
    (78, 0),  // 0x88 'ˆ', extrapolated
    (79, 0),  // 0x89 '‰', extrapolated
    (153, 1), // 0x8A 'Š', extrapolated
    (80, 0),  // 0x8B '‹', extrapolated
    (149, 1), // 0x8C 'Œ', extrapolated
    (35, 0),  // 0x8D unassigned in CP1252, extrapolated
    (160, 1), // 0x8E 'Ž', extrapolated
    (36, 0),  // 0x8F unassigned in CP1252, extrapolated
    (37, 0),  // 0x90 unassigned in CP1252, extrapolated
    (81, 0),  // 0x91 '‘', extrapolated
    (82, 0),  // 0x92 '’', extrapolated
    (83, 0),  // 0x93 '“', extrapolated
    (84, 0),  // 0x94 '”', extrapolated
    (85, 0),  // 0x95 '•', extrapolated
    (86, 0),  // 0x96 '–', extrapolated
    (87, 0),  // 0x97 '—', extrapolated
    (88, 0),  // 0x98 '˜', extrapolated
    (89, 0),  // 0x99 '™', extrapolated
    (153, 1), // 0x9A 'š', extrapolated
    (90, 0),  // 0x9B '›', extrapolated
    (149, 1), // 0x9C 'œ', extrapolated
    (38, 0),  // 0x9D unassigned in CP1252, extrapolated
    (160, 1), // 0x9E 'ž', extrapolated
    (159, 1), // 0x9F 'Ÿ', extrapolated
    (91, 0),  // 0xA0 no-break space, extrapolated
    (92, 0),  // 0xA1 '¡', extrapolated
    (93, 0),  // 0xA2 '¢', extrapolated
    (94, 0),  // 0xA3 '£', extrapolated
    (95, 0),  // 0xA4 '¤', extrapolated
    (96, 0),  // 0xA5 '¥', extrapolated
    (97, 0),  // 0xA6 '¦', extrapolated
    (98, 0),  // 0xA7 '§', extrapolated
    (99, 0),  // 0xA8 '¨', extrapolated
    (100, 0), // 0xA9 '©', extrapolated
    (101, 0), // 0xAA 'ª', extrapolated
    (102, 0), // 0xAB left guillemet, extrapolated
    (103, 0), // 0xAC '¬', extrapolated
    (104, 0), // 0xAD soft hyphen, extrapolated
    (105, 0), // 0xAE '®', extrapolated
    (106, 0), // 0xAF '¯', extrapolated
    (107, 0), // 0xB0 '°', extrapolated
    (108, 0), // 0xB1 '±', extrapolated
    (109, 0), // 0xB2 '²', extrapolated
    (110, 0), // 0xB3 '³', extrapolated
    (111, 0), // 0xB4 '´', extrapolated
    (112, 0), // 0xB5 'µ', extrapolated
    (113, 0), // 0xB6 '¶', extrapolated
    (114, 0), // 0xB7 '·', extrapolated
    (115, 0), // 0xB8 '¸', extrapolated
    (116, 0), // 0xB9 '¹', extrapolated
    (117, 0), // 0xBA 'º', extrapolated
    (118, 0), // 0xBB right guillemet, extrapolated
    (119, 0), // 0xBC '¼', extrapolated
    (120, 0), // 0xBD '½', extrapolated
    (121, 0), // 0xBE '¾', extrapolated
    (122, 0), // 0xBF '¿', extrapolated
    (135, 1), // 0xC0 'À'
    (135, 2), // 0xC1 'Á'
    (135, 3), // 0xC2 'Â'
    (135, 4), // 0xC3 'Ã', extrapolated
    (135, 5), // 0xC4 'Ä'
    (135, 6), // 0xC5 'Å', extrapolated
    (135, 7), // 0xC6 'Æ'
    (137, 1), // 0xC7 'Ç', extrapolated
    (139, 1), // 0xC8 'È', extrapolated
    (139, 2), // 0xC9 'É'
    (139, 3), // 0xCA 'Ê', extrapolated
    (139, 4), // 0xCB 'Ë', extrapolated
    (143, 1), // 0xCC 'Ì', extrapolated
    (143, 2), // 0xCD 'Í', extrapolated
    (143, 3), // 0xCE 'Î', extrapolated
    (143, 4), // 0xCF 'Ï', extrapolated
    (138, 1), // 0xD0 'Ð', extrapolated
    (148, 1), // 0xD1 'Ñ', extrapolated
    (149, 2), // 0xD2 'Ò', extrapolated
    (149, 3), // 0xD3 'Ó', extrapolated
    (149, 4), // 0xD4 'Ô', extrapolated
    (149, 5), // 0xD5 'Õ', extrapolated
    (149, 6), // 0xD6 'Ö', extrapolated
    (123, 0), // 0xD7 '×', extrapolated
    (149, 7), // 0xD8 'Ø', extrapolated
    (155, 1), // 0xD9 'Ù', extrapolated
    (155, 2), // 0xDA 'Ú', extrapolated
    (155, 3), // 0xDB 'Û', extrapolated
    (155, 4), // 0xDC 'Ü', extrapolated
    (159, 2), // 0xDD 'Ý', extrapolated
    (154, 1), // 0xDE 'Þ', extrapolated
    (153, 2), // 0xDF 'ß'
    (135, 1), // 0xE0 'à'
    (135, 2), // 0xE1 'á'
    (135, 3), // 0xE2 'â'
    (135, 4), // 0xE3 'ã', extrapolated
    (135, 5), // 0xE4 'ä'
    (135, 6), // 0xE5 'å', extrapolated
    (135, 7), // 0xE6 'æ'
    (137, 1), // 0xE7 'ç', extrapolated
    (139, 1), // 0xE8 'è', extrapolated
    (139, 2), // 0xE9 'é'
    (139, 3), // 0xEA 'ê', extrapolated
    (139, 4), // 0xEB 'ë', extrapolated
    (143, 1), // 0xEC 'ì', extrapolated
    (143, 2), // 0xED 'í', extrapolated
    (143, 3), // 0xEE 'î', extrapolated
    (143, 4), // 0xEF 'ï', extrapolated
    (138, 1), // 0xF0 'ð', extrapolated
    (148, 1), // 0xF1 'ñ', extrapolated
    (149, 2), // 0xF2 'ò', extrapolated
    (149, 3), // 0xF3 'ó', extrapolated
    (149, 4), // 0xF4 'ô', extrapolated
    (149, 5), // 0xF5 'õ', extrapolated
    (149, 6), // 0xF6 'ö', extrapolated
    (124, 0), // 0xF7 '÷', extrapolated
    (149, 7), // 0xF8 'ø', extrapolated
    (155, 1), // 0xF9 'ù', extrapolated
    (155, 2), // 0xFA 'ú', extrapolated
    (155, 3), // 0xFB 'û', extrapolated
    (155, 4), // 0xFC 'ü', extrapolated
    (159, 2), // 0xFD 'ý', extrapolated
    (154, 1), // 0xFE 'þ', extrapolated
    (159, 1), // 0xFF 'ÿ', extrapolated
];

/// The code page 1252 byte of `c`, or `None` when the code page has no such character.
///
/// A thin name for [`code_page::encode`] under the default collation: the sort weights of
/// [`WEIGHTS`] are indexed by that byte, and the code page table itself lives in
/// [`code_page`](crate::code_page), which is the only place that holds it.
pub(crate) fn cp1252_byte(c: char) -> Option<u8> {
    code_page::encode(c, &Collation::DEFAULT)
}

/// Primary weight of the first character that has no code page 1252 byte. Adding the code
/// point to it keeps every such character after the whole of [`WEIGHTS`] — whose primaries
/// are `u16`, hence below this bound — and orders them among themselves by code point.
const PRIMARY_ABOVE_TABLE: u32 = 0x1_0000;

/// The `(primary, secondary)` sort key of a character.
fn sort_key(c: char) -> (u32, u8) {
    match cp1252_byte(c) {
        Some(byte) => {
            let (primary, secondary) = WEIGHTS[byte as usize];
            (u32::from(primary), secondary)
        }
        None => (PRIMARY_ABOVE_TABLE + u32::from(c), 0),
    }
}

/// The primary weights of a string, in order.
fn primaries(s: &str) -> impl Iterator<Item = u32> + '_ {
    s.chars().map(|c| sort_key(c).0)
}

/// The secondary weights of a string, in order.
fn secondaries(s: &str) -> impl Iterator<Item = u8> + '_ {
    s.chars().map(|c| sort_key(c).1)
}

/// Consumes the next segment when it is `keyword` (ASCII case-insensitively), and says so.
fn take<'a, I: Iterator<Item = &'a str>>(parts: &mut Peekable<I>, keyword: &str) -> bool {
    parts.next_if(|p| p.eq_ignore_ascii_case(keyword)).is_some()
}

/// What a collation name starts with, once the head has been read.
///
/// The head decides which suffixes are legal, so it is carried until the whole name has
/// been read. `Latin1_General_90` is not a variant: no `_90_` collation is installed, and
/// the head is refused as soon as it is recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    /// `SQL_Latin1_General_CP1`, the legacy code-page family, three names.
    Sql,
    /// `Latin1_General`, the versionless Windows family, 18 names.
    Windows,
    /// `Latin1_General_100`, the version-100 Windows family, 51 names.
    Windows100,
}

/// The suffixes a name writes, as written and before any bit is looked at.
///
/// `_SC` in particular has no bit in the 5-byte rule, so the parsed [`Collation`] cannot
/// answer whether it was written; nor can it tell `SQL_Latin1_General_CP1_BIN` from
/// `Latin1_General_BIN`, which share their five bytes and not their existence. The
/// validation is therefore lexical, on this record, and not structural.
struct Suffixes {
    /// `Some(true)` for `_CI`, `Some(false)` for `_CS`, `None` when neither is written.
    ignore_case: Option<bool>,
    /// `Some(true)` for `_AI`, `Some(false)` for `_AS`, `None` when neither is written.
    ignore_accent: Option<bool>,
    /// `_KS` is written.
    kana: bool,
    /// `_WS` is written.
    width: bool,
    /// `Some(false)` for `_BIN`, `Some(true)` for `_BIN2`, `None` for neither.
    binary: Option<bool>,
    /// `_SC` is written.
    supplementary: bool,
    /// `_UTF8` is written.
    utf8: bool,
}

impl Head {
    /// The 4-bit version of the collations of this head, as `vauban-tds` encodes it.
    fn version(self) -> u8 {
        match self {
            Head::Sql | Head::Windows => 0,
            Head::Windows100 => 2,
        }
    }

    /// Whether this head really has the name these suffixes spell.
    ///
    /// Over the whole space of 1 728 spellings (see [`Collation::parse`]), the 72
    /// combinations this returns `true` for are those `fn_helpcollations()` lists; the
    /// other ones raise 448 in a `COLLATE` clause
    /// (`tests::parse_accepts_exactly_the_names_the_server_has`).
    fn allows(self, written: &Suffixes) -> bool {
        let sensitivity = written.ignore_case.is_some() && written.ignore_accent.is_some();
        match self {
            // The legacy family has three members: `_CI_AI`, `_CI_AS` and `_CS_AS`. It
            // spells no `_KS`, no `_WS`, no binary, no `_SC`, no `_UTF8`, and it lacks the
            // fourth sensitivity pair. Those three are exactly the names that own a
            // `SortId`, which is the same statement read from the other end.
            Head::Sql => {
                sensitivity
                    && !written.kana
                    && !written.width
                    && written.binary.is_none()
                    && !written.supplementary
                    && !written.utf8
                    && (written.ignore_case, written.ignore_accent) != (Some(false), Some(true))
            }
            Head::Windows | Head::Windows100 => {
                let hundred = self == Head::Windows100;
                match written.binary {
                    // `_BIN` and `_BIN2` come alone. The single exception of the whole
                    // grammar is `Latin1_General_100_BIN2_UTF8`, which exists while
                    // `Latin1_General_100_BIN_UTF8` and `Latin1_General_BIN2_UTF8` do not.
                    Some(is_bin2) => {
                        written.ignore_case.is_none()
                            && written.ignore_accent.is_none()
                            && !written.kana
                            && !written.width
                            && !written.supplementary
                            && (!written.utf8 || (hundred && is_bin2))
                    }
                    // Otherwise `CI`/`CS` and `AI`/`AS` are required together, `_KS` and
                    // `_WS` are free, `_SC` needs a `_100_` head and `_UTF8` needs `_SC`.
                    None => {
                        sensitivity
                            && (!written.supplementary || hundred)
                            && (!written.utf8 || written.supplementary)
                    }
                }
            }
        }
    }
}

/// `SortId` of a `SQL_Latin1_General_CP1_*` collation, keyed by its flags.
///
/// The three values are what `COLLATIONPROPERTY(<name>, 'SortId')` reports: 52 for
/// `_CI_AS`, 51 for `_CS_AS`, 54 for `_CI_AI`.
///
/// Those three names are the whole legacy family, so the `0` of the other arms is
/// unreachable through a `SQL_Latin1_General_CP1_*` name: [`Head::allows`] refuses
/// `..._CS_AI`, `..._CI_AS_KS`, `..._BIN` and `..._BIN2` before this function is called,
/// each of them a name without a `SortId` and refused with 448 by a `COLLATE` clause. The
/// arm stays because `flags` is a `u8` and this is a total function; no `SortId` is
/// invented for a combination the server does not have.
fn sql_sort_id(flags: u8) -> u8 {
    match flags {
        FLAGS_CI_AS => 52,
        FLAGS_CS_AS => 51,
        FLAGS_CI_AI => 54,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
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
        // A case pair of the code page shares its two weights (`_CI`).
        for (upper, lower) in [('A', 'a'), ('Z', 'z'), ('É', 'é'), ('Æ', 'æ'), ('Ø', 'ø')] {
            let (upper_byte, lower_byte) = (
                super::cp1252_byte(upper).expect("in the code page"),
                super::cp1252_byte(lower).expect("in the code page"),
            );
            assert_eq!(
                super::WEIGHTS[upper_byte as usize],
                super::WEIGHTS[lower_byte as usize],
                "{upper} and {lower}"
            );
        }

        // The four groups of primary weights, in order: control, punctuation, digit,
        // letter.
        let primary = |c: char| {
            let byte = super::cp1252_byte(c).expect("in the code page");
            super::WEIGHTS[byte as usize].0
        };
        assert!(primary('\t') < primary(' '));
        assert!(primary(' ') < primary('-'));
        assert!(primary('-') < primary('0'));
        assert!(primary('9') < primary('A'));
        assert!(primary('A') < primary('B'));
    }
}
