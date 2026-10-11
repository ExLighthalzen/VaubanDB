//! Collation identifier, as SQL Server names and transmits it.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::sync::OnceLock;

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
    /// The flags of a name decide how it compares: see [`Collation::compare`].
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

    /// Orders two strings under this collation.
    ///
    /// The collation picks one of four families of rules from its five bytes:
    ///
    /// - **`_BIN2`**: UTF-16 code units, in numeric order. The shorter operand is padded
    ///   with spaces, so `'abc'` sorts after `'abc' + CHAR(9)`.
    /// - **`_BIN`**: the first character as a UTF-16 code unit, then the rest of the string
    ///   as little-endian UTF-16 bytes; trailing spaces are dropped and a string that is a
    ///   prefix of the other one is the smaller one.
    /// - **`SQL_Latin1_General_CP1_*`**: the sort table of code page 1252 (the order of
    ///   `varchar` data under these collations).
    /// - **`Latin1_General_*`** and **`Latin1_General_100_*`**: the Windows rules, one table
    ///   per version.
    ///
    /// The two table families compare level by level over the whole string: first the
    /// **primary** weights (the letter), then, when the collation is accent-sensitive
    /// (`_AS`), the **accent** weights, then, when it is case-, kana- or width-sensitive
    /// (`_CS`, `_KS`, `_WS`), a **tertiary** weight that orders the case first, the width
    /// second and the kana last. An insensitive axis drops out of the comparison:
    /// `'a' = 'A'` under `_CI_AS` and not under `_CS_AS`, `'e' = 'é'` under `_CI_AI` and not
    /// under `_CI_AS` (`tests/collation.rs`, `case_sensitivity_follows_the_flag` and
    /// `accent_sensitivity_follows_the_flag`).
    ///
    /// The shorter operand is padded with spaces at each of these levels, which is what
    /// makes trailing spaces insignificant: `'abc' = 'abc   '`. The padding is not a
    /// trimming: a character that sorts below the space, such as a tab under the code page
    /// 1252 table, makes `'abc' + CHAR(9)` sort before `'abc'`.
    ///
    /// What sets the two table families apart:
    ///
    /// - the code page 1252 table sorts upper case before lower case, the Windows tables
    ///   lower case first (`'A' < 'a'` against `'a' < 'A'` under `_CS_AS`);
    /// - a few letters expand to two: `Æ` to `AE` and `ß` to `ss` in both, `Œ` to `OE` and
    ///   `Þ` to `TH` under the Windows rules. Under the code page 1252 table `Ç` takes the
    ///   primary weight of `C`, and these letters stay apart from their expansion through a
    ///   level of their own, compared after the accents and before the case, which an
    ///   accent-insensitive collation keeps: `'Ç' <> 'C'` under
    ///   `SQL_Latin1_General_CP1_CI_AI`;
    /// - the Windows rules leave the hyphen, the apostrophe, the en and em dashes and the
    ///   control characters out of the first three levels, and compare them last, by
    ///   position: `'a-b'` sorts after `'ab'` and before `'ac'`. `NUL` is ignored there
    ///   altogether.
    ///
    /// A character outside code page 1252 sorts after the characters of the tables, by
    /// code point. The full-width forms of ASCII and the katakana are the exception: they
    /// take the weights of the ASCII character and of the hiragana, and differ from them at
    /// the tertiary level (`tests/collation.rs`, `kana_and_width_sensitivity_follow_the_flags`).
    /// The full-width hyphen and apostrophe follow theirs to the last level, where the width
    /// tells them apart under `_WS` (`full_width_hyphen_and_apostrophe_are_weighed_last`).
    ///
    /// # Limits
    ///
    /// A `SQL_*` collation sorts non-Unicode data (`char`, `varchar`) with the table of its
    /// code page and Unicode data (`nchar`, `nvarchar`) with the Windows rules, and `_BIN`
    /// sorts `varchar` data by code page byte with space padding. [`SqlString`](crate::SqlString)
    /// does not say which side a value is on, so this function applies the `varchar` order
    /// under `SQL_*` and the `nvarchar` order under `_BIN`: `'à' < 'á'` holds here, which is
    /// the `varchar` answer.
    ///
    /// The placement of a character outside code page 1252 by code point is a
    /// simplification: SQL Server files `Ā` (U+0100) with the `a` and this function after
    /// `ÿ`.
    ///
    /// ```
    /// use std::cmp::Ordering;
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(latin1.compare("abc", "ABC   "), Ordering::Equal);
    /// assert_eq!(latin1.compare("e", "é"), Ordering::Less);
    /// assert_eq!(latin1.compare("é", "f"), Ordering::Less);
    ///
    /// let cs = Collation::parse("Latin1_General_CS_AS").unwrap();
    /// assert_eq!(cs.compare("a", "A"), Ordering::Less);
    /// let ai = Collation::parse("Latin1_General_CI_AI").unwrap();
    /// assert_eq!(ai.compare("e", "É"), Ordering::Equal);
    /// let bin2 = Collation::parse("Latin1_General_BIN2").unwrap();
    /// assert_eq!(bin2.compare("Z", "a"), Ordering::Less);
    /// ```
    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        // Identical text is equal under every rule here (`tests/collation.rs`); the shortcut
        // spares the level-by-level walk to the most frequent comparison of names.
        if a == b {
            return Ordering::Equal;
        }
        match self.rules() {
            Rules::Binary => compare_binary(a.trim_end_matches(' '), b.trim_end_matches(' ')),
            Rules::Binary2 => compare_units(a.encode_utf16(), b.encode_utf16(), Some(SPACE_UNIT)),
            Rules::Table(table) => self.compare_weighted(table, a, b, true),
        }
    }

    /// Compares two characters under the same rules as [`Collation::compare`], without
    /// allocating.
    ///
    /// This is the primitive `LIKE` (character classes and ranges) and the string
    /// functions of `sysfn` build on; they do not reimplement the collation.
    ///
    /// The only difference with `compare` on two one-character strings is the space:
    /// `compare(" ", "\t")` pads the empty side and compares a space with a tab, while
    /// `compare_char(' ', '\t')` compares the two characters as they are. Under the
    /// default collation the space sorts after the control characters, so both answer
    /// `Greater` there; under the Windows rules the comparison of a space with an empty
    /// string differs. Trailing-blank padding is a property of string operands, not of
    /// characters.
    ///
    /// ```
    /// use std::cmp::Ordering;
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(latin1.compare_char('a', 'A'), Ordering::Equal);
    /// assert_eq!(latin1.compare_char('e', 'é'), Ordering::Less);
    /// let cs = Collation::parse("SQL_Latin1_General_CP1_CS_AS").unwrap();
    /// assert_eq!(cs.compare_char('A', 'a'), Ordering::Less);
    /// ```
    pub fn compare_char(&self, a: char, b: char) -> Ordering {
        if a == b {
            return Ordering::Equal;
        }
        let mut a_units = [0u16; 2];
        let mut b_units = [0u16; 2];
        let a_units = a.encode_utf16(&mut a_units);
        let b_units = b.encode_utf16(&mut b_units);
        match self.rules() {
            Rules::Binary | Rules::Binary2 => {
                compare_units(a_units.iter().copied(), b_units.iter().copied(), None)
            }
            Rules::Table(table) => {
                let mut a_text = [0u8; 4];
                let mut b_text = [0u8; 4];
                self.compare_weighted(
                    table,
                    a.encode_utf8(&mut a_text),
                    b.encode_utf8(&mut b_text),
                    false,
                )
            }
        }
    }

    /// Position of the first occurrence of `needle` in `haystack` under this collation,
    /// **1-based**, or `None` when it does not occur. This is the primitive of `CHARINDEX`.
    ///
    /// The search starts at the character `start` (1-based; `0` is treated as `1`, as
    /// `CHARINDEX` does with a non-positive start). Characters are matched with
    /// [`Collation::compare_char`], so under the default collation the case is ignored and
    /// the accents are not: `find("ABCDEF", "cd", 1)` is `Some(3)` and `find("abc", "é", 1)`
    /// is `None`. Under `_CS_AS` or `_BIN2`, `find("ABCDEF", "cd", 1)` is `None`.
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

    /// The name of the collation, as the server spells it in a message.
    ///
    /// A non-zero `SortId` names a legacy `SQL_Latin1_General_CP1` collation, the version of
    /// the `_100_` family adds `_100`, a binary collation names `_BIN` or `_BIN2` alone, and
    /// the others spell `_CI`/`_CS`, `_AI`/`_AS`, then `_KS` and `_WS` for the bits they do
    /// not ignore. `_SC` has no bit and is restored before `_UTF8` on a linguistic `_100_`
    /// name, where the `_UTF8` spelling without `_SC` does not exist
    /// (`precedence::tests::a_collation_spells_back_its_name`).
    pub fn name(&self) -> String {
        let mut name = String::from(if self.sort_id != 0 {
            "SQL_Latin1_General_CP1"
        } else if self.version == WINDOWS_100_VERSION {
            "Latin1_General_100"
        } else {
            "Latin1_General"
        });
        let flags = self.flags;
        let binary = flags & (FLAG_BINARY | FLAG_BINARY2) != 0;
        if flags & FLAG_BINARY2 != 0 {
            name.push_str("_BIN2");
        } else if flags & FLAG_BINARY != 0 {
            name.push_str("_BIN");
        } else {
            name.push_str(if flags & FLAG_IGNORE_CASE == 0 {
                "_CS"
            } else {
                "_CI"
            });
            name.push_str(if flags & FLAG_IGNORE_ACCENT == 0 {
                "_AS"
            } else {
                "_AI"
            });
            if flags & FLAG_IGNORE_KANA == 0 {
                name.push_str("_KS");
            }
            if flags & FLAG_IGNORE_WIDTH == 0 {
                name.push_str("_WS");
            }
        }
        if flags & FLAG_UTF8 != 0 {
            if self.sort_id == 0 && self.version == WINDOWS_100_VERSION && !binary {
                name.push_str("_SC");
            }
            name.push_str("_UTF8");
        }
        name
    }

    /// The family of rules this collation compares with.
    pub(crate) fn rules(&self) -> Rules {
        if self.flags & FLAG_BINARY2 != 0 {
            Rules::Binary2
        } else if self.flags & FLAG_BINARY != 0 {
            Rules::Binary
        } else if self.sort_id != 0 {
            Rules::Table(&SQL_CP1)
        } else if self.version == WINDOWS_100_VERSION {
            Rules::Table(&WINDOWS_100)
        } else {
            Rules::Table(&WINDOWS)
        }
    }

    /// The levels a table comparison goes through for this collation, in order.
    pub(crate) fn levels(&self, table: &Table) -> ([Level; 5], usize) {
        let mut levels = [Level::Primary; 5];
        let mut count = 1;
        let mut push = |level| {
            levels[count] = level;
            count += 1;
        };
        if self.flags & FLAG_IGNORE_ACCENT == 0 {
            push(Level::Accent);
        }
        if table.own_level {
            push(Level::Own);
        }
        if self.flags & (FLAG_IGNORE_CASE | FLAG_IGNORE_KANA | FLAG_IGNORE_WIDTH)
            != (FLAG_IGNORE_CASE | FLAG_IGNORE_KANA | FLAG_IGNORE_WIDTH)
        {
            push(Level::Tertiary);
        }
        if table.has_last_level {
            push(Level::Last);
        }
        (levels, count)
    }

    /// Compares two strings level by level under a weight table. With `pad`, the shorter
    /// side of each level but the last is padded with the weight of a space.
    fn compare_weighted(&self, table: &Table, a: &str, b: &str, pad: bool) -> Ordering {
        // An identical leading run of characters adds the same weights, at the same
        // positions, to both sides of each level: it cannot decide anything.
        let mut common = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
        while !a.is_char_boundary(common) {
            common -= 1;
        }
        let (a, b) = (&a[common..], &b[common..]);
        // Two strings of plain ASCII characters compare in one pass: the first primary
        // difference decides, then the first accent difference, then the first case
        // difference (ASCII has no width, no kana, no expansion and no alias, and its
        // `Own` weight is its primary weight).
        let mut accent = Ordering::Equal;
        let mut case = Ordering::Equal;
        let mut whole = a.len() == b.len();
        for (x, y) in a.bytes().zip(b.bytes()) {
            if !x.is_ascii() || !y.is_ascii() {
                whole = false;
                break;
            }
            let (x, y) = (table.ascii[usize::from(x)], table.ascii[usize::from(y)]);
            if x.class != AsciiClass::Plain || y.class != AsciiClass::Plain {
                whole = false;
                break;
            }
            let (x, y) = (x.weights, y.weights);
            if x.primary != y.primary {
                return x.primary.cmp(&y.primary);
            }
            if accent == Ordering::Equal {
                accent = x.accent.cmp(&y.accent);
            }
            if case == Ordering::Equal {
                case = if self.flags & FLAG_IGNORE_ACCENT != 0 {
                    x.case_only.cmp(&y.case_only)
                } else {
                    x.case_accent.cmp(&y.case_accent)
                };
            }
        }
        if whole {
            if self.flags & FLAG_IGNORE_ACCENT == 0 && accent != Ordering::Equal {
                return accent;
            }
            if self.flags & FLAG_IGNORE_CASE == 0 {
                return case;
            }
            return Ordering::Equal;
        }
        let (levels, count) = self.levels(table);
        if a.is_ascii() && b.is_ascii() {
            return self.compare_ascii(table, a.as_bytes(), b.as_bytes(), pad, &levels[..count]);
        }
        for &level in &levels[..count] {
            let padding = (pad && level != Level::Last)
                .then(|| self.level_weights(table, " ", level).next())
                .flatten();
            let ordering = compare_sequences(
                self.level_weights(table, a, level),
                self.level_weights(table, b, level),
                padding,
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    /// [`Collation::compare_weighted`] on two ASCII strings, through the rows of
    /// [`Table::ascii`]: ASCII has no expansion and no alias, so a byte is one row.
    fn compare_ascii(
        &self,
        table: &Table,
        a: &[u8],
        b: &[u8],
        pad: bool,
        levels: &[Level],
    ) -> Ordering {
        let rows = &table.ascii;
        let flags = self.flags;
        let row = |byte: &u8| rows[usize::from(*byte & 0x7F)];
        for &level in levels {
            let ordering = if level == Level::Last {
                compare_sequences(ascii_last(rows, a), ascii_last(rows, b), None)
            } else {
                let weight = move |r: AsciiRow| ascii_weight(r, level, flags);
                let plain = |r: &AsciiRow| r.class == AsciiClass::Plain;
                compare_sequences(
                    a.iter().map(row).filter(plain).map(weight),
                    b.iter().map(row).filter(plain).map(weight),
                    pad.then(|| weight(rows[usize::from(b' ')])),
                )
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    /// The weights of `s` at one level, in order.
    pub(crate) fn level_weights<'a>(
        &self,
        table: &'a Table,
        s: &'a str,
        level: Level,
    ) -> impl Iterator<Item = u64> + 'a {
        let flags = self.flags;
        s.chars()
            .map(move |c| table.cached_unit(c))
            .filter(|unit| !matches!(unit, Unit::Ignored))
            .enumerate()
            .flat_map(move |(position, unit)| {
                let (elements, own, last) = match unit {
                    Unit::Ignored => ([None, None], None, None),
                    Unit::Last(weight) => ([None, None], None, Some(weight)),
                    Unit::Elements(elements, own) => (elements, Some(own), None),
                };
                let weights: [Option<u64>; 2] = match level {
                    Level::Own => [own.map(u64::from), None],
                    Level::Last => [
                        last.map(|weight| {
                            let weight = if flags & FLAG_IGNORE_WIDTH != 0 {
                                weight & !1
                            } else {
                                weight
                            };
                            ((position as u64) << 32) | u64::from(weight)
                        }),
                        None,
                    ],
                    _ => elements.map(|element| element.map(|e| e.weight(level, flags))),
                };
                weights.into_iter().flatten()
            })
    }
}

/// The four families of comparison rules ([`Collation::compare`]).
#[derive(Clone, Copy)]
pub(crate) enum Rules {
    /// `_BIN`: first code unit, then little-endian bytes.
    Binary,
    /// `_BIN2`: UTF-16 code units.
    Binary2,
    /// A weight table: code page 1252 or one of the two Windows versions.
    Table(&'static Table),
}

/// One level of a table comparison ([`Collation::compare`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// The letter.
    Primary,
    /// The accent, compared under `_AS`.
    Accent,
    /// The level that tells a letter of the code page 1252 table from its expansion.
    Own,
    /// Case, then width, then kana, each one when the collation is sensitive to it.
    Tertiary,
    /// The characters the Windows rules leave out of the other levels, by position.
    Last,
}

/// The weights of one character of code page 1252 under a table, one per sensitivity
/// combination: each weight orders the characters as the collation of that combination
/// does, two characters with the same weight being equal under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Weights {
    /// The order under `_CI_AI`.
    primary: u16,
    /// The order under `_CI_AS`.
    accent: u16,
    /// The order under `_CS_AS`.
    case_accent: u16,
    /// The order under `_CS_AI`.
    case_only: u16,
}

/// Shorthand for the rows of the weight tables.
const fn w(primary: u16, accent: u16, case_accent: u16, case_only: u16) -> Weights {
    Weights {
        primary,
        accent,
        case_accent,
        case_only,
    }
}

/// A weight table and the rules that go with it.
pub(crate) struct Table {
    /// The weights, indexed by code page 1252 byte.
    weights: &'static [Weights; 256],
    /// The letters that expand to two, with their expansion.
    expansions: &'static [(char, [char; 2])],
    /// Letters that keep the primary weight of another one, with that one.
    aliases: &'static [(char, char)],
    /// Characters left out of the levels before the last one.
    last: &'static [char],
    /// Characters the comparison skips altogether.
    ignored: &'static [char],
    /// The table compares an `Own` level.
    own_level: bool,
    /// The table compares a `Last` level.
    has_last_level: bool,
    /// The 128 ASCII characters, read without lookup ([`ascii_rows`]).
    pub(crate) ascii: [AsciiRow; 128],
    /// [`Table::unit`] of U+0000 to U+00FF, computed on first use.
    latin1: OnceLock<[Unit; 256]>,
}

/// How an ASCII character takes part in a table comparison.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsciiClass {
    /// Weighed at the levels before the last.
    Plain,
    /// Weighed at the last level.
    Last,
    /// Skipped.
    Ignored,
}

/// The weights and the class of one ASCII character under a table.
#[derive(Clone, Copy)]
pub(crate) struct AsciiRow {
    /// The class.
    pub(crate) class: AsciiClass,
    /// The weights, from the table row of the same byte.
    weights: Weights,
}

/// The last-level weights of an ASCII string: position among the characters that are not
/// skipped, then weight, as [`Collation::level_weights`] computes them.
pub(crate) fn ascii_last<'a>(
    rows: &'a [AsciiRow; 128],
    text: &'a [u8],
) -> impl Iterator<Item = u64> + 'a {
    text.iter()
        .map(move |byte| rows[usize::from(*byte & 0x7F)])
        .filter(|row| row.class != AsciiClass::Ignored)
        .enumerate()
        .filter(|(_, row)| row.class == AsciiClass::Last)
        .map(|(position, row)| {
            ((position as u64) << 32) | (u64::from(row.weights.case_accent) << 1)
        })
}

/// The weight of a plain ASCII row at a level before the last, as [`Element::weight`]
/// computes it for the same character under a collation with `flags`.
pub(crate) fn ascii_weight(row: AsciiRow, level: Level, flags: u8) -> u64 {
    let w = &row.weights;
    match level {
        Level::Primary | Level::Own => u64::from(w.primary),
        Level::Accent => u64::from(w.accent),
        Level::Tertiary => {
            let case = if flags & FLAG_IGNORE_CASE != 0 {
                0
            } else if flags & FLAG_IGNORE_ACCENT != 0 {
                w.case_only
            } else {
                w.case_accent
            };
            u64::from(case) << 2
        }
        Level::Last => 0,
    }
}

/// The ASCII rows of a table, from its weights and its `last` and `ignored` lists.
const fn ascii_rows(weights: &[Weights; 256], last: &[char], ignored: &[char]) -> [AsciiRow; 128] {
    let mut rows = [AsciiRow {
        class: AsciiClass::Plain,
        weights: w(0, 0, 0, 0),
    }; 128];
    let mut byte = 0;
    while byte < 128 {
        rows[byte].weights = weights[byte];
        let mut i = 0;
        while i < last.len() {
            if last[i] as usize == byte {
                rows[byte].class = AsciiClass::Last;
            }
            i += 1;
        }
        let mut i = 0;
        while i < ignored.len() {
            if ignored[i] as usize == byte {
                rows[byte].class = AsciiClass::Ignored;
            }
            i += 1;
        }
        byte += 1;
    }
    rows
}

/// What one character contributes to a table comparison.
#[derive(Clone, Copy)]
enum Unit {
    /// Nothing, at any level.
    Ignored,
    /// A weight at the last level, and nothing before it.
    Last(u32),
    /// One or two elements, and the weight of the character at the `Own` level.
    Elements([Option<Element>; 2], u32),
}

/// One collation element: the weights a character, or half of an expansion, carries.
#[derive(Clone, Copy)]
struct Element {
    /// The weights of the table row the element reads.
    weights: Weights,
    /// The primary weight of a character outside the table, by code point.
    outside: Option<u32>,
    /// A full-width form.
    wide: bool,
    /// A hiragana, which sorts after the katakana of the same sound.
    hiragana: bool,
}

impl Element {
    /// The primary weight, inside or outside the table.
    fn primary_weight(&self) -> u32 {
        self.outside
            .unwrap_or_else(|| u32::from(self.weights.primary))
    }

    /// The weight of this element at `level` under a collation with `flags`.
    fn weight(&self, level: Level, flags: u8) -> u64 {
        match level {
            Level::Primary => match self.outside {
                Some(primary) => u64::from(primary),
                None => u64::from(self.weights.primary),
            },
            Level::Accent => u64::from(self.weights.accent),
            Level::Tertiary => {
                let case = if flags & FLAG_IGNORE_CASE != 0 || self.outside.is_some() {
                    0
                } else if flags & FLAG_IGNORE_ACCENT != 0 {
                    self.weights.case_only
                } else {
                    self.weights.case_accent
                };
                let wide = flags & FLAG_IGNORE_WIDTH == 0 && self.wide;
                let hiragana = flags & FLAG_IGNORE_KANA == 0 && self.hiragana;
                (u64::from(case) << 2) | (u64::from(wide) << 1) | u64::from(hiragana)
            }
            // Read off the unit, not the element.
            Level::Own | Level::Last => 0,
        }
    }
}

impl Table {
    /// [`Table::unit`], read from [`Table::latin1`] below U+0100.
    fn cached_unit(&self, c: char) -> Unit {
        match usize::try_from(u32::from(c)) {
            Ok(code) if code < 0x100 => {
                self.latin1.get_or_init(|| {
                    std::array::from_fn(|code| {
                        // `code` is below 256, so it is a scalar value.
                        self.unit(char::from_u32(code as u32).unwrap_or('\0'))
                    })
                })[code]
            }
            _ => self.unit(c),
        }
    }

    /// What `c` contributes to a comparison under this table.
    fn unit(&self, c: char) -> Unit {
        // ASCII letters, digits and the space are plain in the three tables: no lookup.
        if c.is_ascii_alphanumeric() || c == ' ' {
            let element = self.element(c);
            return Unit::Elements([Some(element), None], element.primary_weight());
        }
        if self.ignored.contains(&c) {
            return Unit::Ignored;
        }
        // A full-width hyphen or apostrophe goes to the last level like its ASCII form, and
        // differs from it by width there.
        let code = u32::from(c);
        let (narrow, wide) = match code {
            FULL_WIDTH_FIRST..=FULL_WIDTH_LAST => {
                (char::from_u32(code - FULL_WIDTH_OFFSET).unwrap_or(c), true)
            }
            _ => (c, false),
        };
        if self.last.contains(&narrow) {
            let weight =
                cp1252_byte(narrow).map_or(0, |byte| self.weights[byte as usize].case_accent);
            return Unit::Last((u32::from(weight) << 1) | u32::from(wide));
        }
        let own = self.element(c).primary_weight();
        if let Some((_, [first, second])) = self.expansions.iter().find(|(from, _)| *from == c) {
            return Unit::Elements(
                [Some(self.element(*first)), Some(self.element(*second))],
                own,
            );
        }
        let base = self
            .aliases
            .iter()
            .find(|(from, _)| *from == c)
            .map_or(c, |(_, to)| *to);
        Unit::Elements([Some(self.element(base)), None], own)
    }

    /// The element of a single character, folding the full-width forms of ASCII onto
    /// ASCII and the katakana onto the hiragana.
    fn element(&self, c: char) -> Element {
        let code = u32::from(c);
        let (folded, wide, hiragana) = match code {
            FULL_WIDTH_FIRST..=FULL_WIDTH_LAST => (code - FULL_WIDTH_OFFSET, true, false),
            KATAKANA_FIRST..=KATAKANA_LAST => (code - KANA_OFFSET, false, false),
            HIRAGANA_FIRST..=HIRAGANA_LAST => (code, false, true),
            _ => (code, false, false),
        };
        let folded = char::from_u32(folded).unwrap_or(c);
        match cp1252_byte(folded) {
            Some(byte) => Element {
                weights: self.weights[byte as usize],
                outside: None,
                wide,
                hiragana,
            },
            None => Element {
                weights: w(0, 0, 0, 0),
                outside: Some(PRIMARY_ABOVE_TABLE + u32::from(folded)),
                wide,
                hiragana,
            },
        }
    }
}

/// Compares two weight sequences; with `padding`, the shorter one is extended with it,
/// otherwise a sequence that is a prefix of the other one is the smaller one.
fn compare_sequences(
    mut a: impl Iterator<Item = u64>,
    mut b: impl Iterator<Item = u64>,
    padding: Option<u64>,
) -> Ordering {
    loop {
        let (x, y) = match (a.next(), b.next(), padding) {
            (None, None, _) => return Ordering::Equal,
            (Some(x), Some(y), _) => (x, y),
            (Some(x), None, Some(pad)) => (x, pad),
            (None, Some(y), Some(pad)) => (pad, y),
            (Some(_), None, None) => return Ordering::Greater,
            (None, Some(_), None) => return Ordering::Less,
        };
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
}

/// [`compare_sequences`] over UTF-16 code units.
fn compare_units(
    a: impl Iterator<Item = u16>,
    b: impl Iterator<Item = u16>,
    padding: Option<u16>,
) -> Ordering {
    compare_sequences(a.map(u64::from), b.map(u64::from), padding.map(u64::from))
}

/// The `_BIN` order of two strings whose trailing spaces are already dropped: the first
/// code unit, then the little-endian bytes of the remaining ones.
fn compare_binary(a: &str, b: &str) -> Ordering {
    let mut a_units = a.encode_utf16();
    let mut b_units = b.encode_utf16();
    match (a_units.next(), b_units.next()) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| {
            compare_sequences(
                a_units.flat_map(u16::to_le_bytes).map(u64::from),
                b_units.flat_map(u16::to_le_bytes).map(u64::from),
                None,
            )
        }),
    }
}

/// The space as a UTF-16 code unit, the padding of `_BIN2`.
pub(crate) const SPACE_UNIT: u16 = 0x20;
/// Version byte of the `Latin1_General_100_*` collations.
const WINDOWS_100_VERSION: u8 = 2;
/// First full-width form of an ASCII character (`！`, U+FF01).
const FULL_WIDTH_FIRST: u32 = 0xFF01;
/// Last full-width form of an ASCII character (`～`, U+FF5E).
const FULL_WIDTH_LAST: u32 = 0xFF5E;
/// Distance from a full-width form to its ASCII character.
const FULL_WIDTH_OFFSET: u32 = 0xFEE0;
/// First hiragana that has a katakana twin (U+3041).
const HIRAGANA_FIRST: u32 = 0x3041;
/// Last such hiragana (U+3096).
const HIRAGANA_LAST: u32 = 0x3096;
/// First katakana that has a hiragana twin (U+30A1).
const KATAKANA_FIRST: u32 = 0x30A1;
/// Last such katakana (U+30F6).
const KATAKANA_LAST: u32 = 0x30F6;
/// Distance from a katakana to its hiragana.
const KANA_OFFSET: u32 = 0x60;

/// The control characters the Windows rules compare at the last level.
const WINDOWS_CONTROLS: [char; 27] = [
    '\u{01}', '\u{02}', '\u{03}', '\u{04}', '\u{05}', '\u{06}', '\u{07}', '\u{08}', '\u{0E}',
    '\u{0F}', '\u{10}', '\u{11}', '\u{12}', '\u{13}', '\u{14}', '\u{15}', '\u{16}', '\u{17}',
    '\u{18}', '\u{19}', '\u{1A}', '\u{1B}', '\u{1C}', '\u{1D}', '\u{1E}', '\u{1F}', '\u{7F}',
];

/// The characters the versionless Windows rules compare at the last level: the
/// hyphen, the apostrophe, the en and em dashes, the soft hyphen, the control characters
/// and the five bytes code page 1252 leaves unassigned.
const WINDOWS_LAST: [char; 37] = {
    let mut last = ['\0'; 37];
    let head = [
        '-', '\'', '\u{2013}', '\u{2014}', '\u{AD}', '\u{81}', '\u{8D}', '\u{8F}', '\u{90}',
        '\u{9D}',
    ];
    let mut i = 0;
    while i < head.len() {
        last[i] = head[i];
        i += 1;
    }
    while i < last.len() {
        last[i] = WINDOWS_CONTROLS[i - head.len()];
        i += 1;
    }
    last
};

/// The same list under `Latin1_General_100_*`: the soft hyphen leaves it for the ignored
/// characters, and the five unassigned bytes become ordinary symbols.
const WINDOWS_100_LAST: [char; 31] = {
    let mut last = ['\0'; 31];
    let head = ['-', '\'', '\u{2013}', '\u{2014}'];
    let mut i = 0;
    while i < head.len() {
        last[i] = head[i];
        i += 1;
    }
    while i < last.len() {
        last[i] = WINDOWS_CONTROLS[i - head.len()];
        i += 1;
    }
    last
};

/// The letters both Windows tables expand to two.
const WINDOWS_EXPANSIONS: [(char, [char; 2]); 7] = [
    ('Æ', ['A', 'E']),
    ('æ', ['a', 'e']),
    ('Œ', ['O', 'E']),
    ('œ', ['o', 'e']),
    ('ß', ['s', 's']),
    ('Þ', ['T', 'H']),
    ('þ', ['t', 'h']),
];

/// `SQL_Latin1_General_CP1_*`: [`SQL_CP1_WEIGHTS`], with `Æ` and `ß` expanded, `Ç` on the
/// primary weight of `C`, and the `Own` level that keeps these apart from what they stand
/// for.
static SQL_CP1: Table = Table {
    weights: &SQL_CP1_WEIGHTS,
    expansions: &[('Æ', ['A', 'E']), ('æ', ['a', 'e']), ('ß', ['s', 's'])],
    aliases: &[('Ç', 'C'), ('ç', 'c')],
    last: &[],
    ignored: &[],
    own_level: true,
    has_last_level: false,
    ascii: ascii_rows(&SQL_CP1_WEIGHTS, &[], &[]),
    latin1: OnceLock::new(),
};

/// `Latin1_General_*`: [`WINDOWS_WEIGHTS`].
static WINDOWS: Table = Table {
    weights: &WINDOWS_WEIGHTS,
    expansions: &WINDOWS_EXPANSIONS,
    aliases: &[],
    last: &WINDOWS_LAST,
    ignored: &['\0'],
    own_level: false,
    has_last_level: true,
    ascii: ascii_rows(&WINDOWS_WEIGHTS, &WINDOWS_LAST, &['\0']),
    latin1: OnceLock::new(),
};

/// `Latin1_General_100_*`: [`WINDOWS_100_WEIGHTS`].
static WINDOWS_100: Table = Table {
    weights: &WINDOWS_100_WEIGHTS,
    expansions: &WINDOWS_EXPANSIONS,
    aliases: &[],
    last: &WINDOWS_100_LAST,
    ignored: &['\0', '\u{AD}'],
    own_level: false,
    has_last_level: true,
    ascii: ascii_rows(&WINDOWS_100_WEIGHTS, &WINDOWS_100_LAST, &['\0', '\u{AD}']),
    latin1: OnceLock::new(),
};

/// The sort table of `SQL_Latin1_General_CP1_*`, indexed by **code page 1252 byte** and
/// not by code point: 27 of the characters the code page puts in its `0x80`-`0x9F` range
/// (`€` U+20AC, `Œ` U+0152, `’` U+2019...) live far beyond `0xFF`. [`cp1252_byte`] does
/// the conversion.
///
/// The control characters come first, then the space and the punctuation (the symbols of
/// the `0x80`-`0xBF` range included, `Š`, `Œ`, `Ž`, `ƒ` and `Ÿ` among them), then the
/// digits, then the letters, upper case before lower case. `Ð` and `Þ` sort after `z`.
/// `Æ`, `ß` and `Ç` are read through [`SQL_CP1`].
const SQL_CP1_WEIGHTS: [Weights; 256] = [
    w(1, 1, 1, 1),         // 0x00 NUL
    w(2, 2, 2, 2),         // 0x01 control
    w(3, 3, 3, 3),         // 0x02 control
    w(4, 4, 4, 4),         // 0x03 control
    w(5, 5, 5, 5),         // 0x04 control
    w(6, 6, 6, 6),         // 0x05 control
    w(7, 7, 7, 7),         // 0x06 control
    w(8, 8, 8, 8),         // 0x07 control
    w(9, 9, 9, 9),         // 0x08 control
    w(10, 10, 10, 10),     // 0x09 tab
    w(11, 11, 11, 11),     // 0x0A LF
    w(12, 12, 12, 12),     // 0x0B control
    w(13, 13, 13, 13),     // 0x0C control
    w(14, 14, 14, 14),     // 0x0D CR
    w(15, 15, 15, 15),     // 0x0E control
    w(16, 16, 16, 16),     // 0x0F control
    w(17, 17, 17, 17),     // 0x10 control
    w(18, 18, 18, 18),     // 0x11 control
    w(19, 19, 19, 19),     // 0x12 control
    w(20, 20, 20, 20),     // 0x13 control
    w(21, 21, 21, 21),     // 0x14 control
    w(22, 22, 22, 22),     // 0x15 control
    w(23, 23, 23, 23),     // 0x16 control
    w(24, 24, 24, 24),     // 0x17 control
    w(25, 25, 25, 25),     // 0x18 control
    w(26, 26, 26, 26),     // 0x19 control
    w(27, 27, 27, 27),     // 0x1A control
    w(28, 28, 28, 28),     // 0x1B ESC
    w(29, 29, 29, 29),     // 0x1C control
    w(30, 30, 30, 30),     // 0x1D control
    w(31, 31, 31, 31),     // 0x1E control
    w(32, 32, 32, 32),     // 0x1F control
    w(33, 33, 33, 33),     // 0x20 space
    w(34, 34, 34, 34),     // 0x21 '!'
    w(35, 35, 35, 35),     // 0x22 '"'
    w(36, 36, 36, 36),     // 0x23 '#'
    w(37, 37, 37, 37),     // 0x24 '$'
    w(38, 38, 38, 38),     // 0x25 '%'
    w(39, 39, 39, 39),     // 0x26 '&'
    w(40, 40, 40, 40),     // 0x27 '''
    w(41, 41, 41, 41),     // 0x28 '('
    w(42, 42, 42, 42),     // 0x29 ')'
    w(43, 43, 43, 43),     // 0x2A '*'
    w(44, 44, 44, 44),     // 0x2B '+'
    w(45, 45, 45, 45),     // 0x2C ','
    w(46, 46, 46, 46),     // 0x2D '-'
    w(47, 47, 47, 47),     // 0x2E '.'
    w(48, 48, 48, 48),     // 0x2F '/'
    w(133, 133, 133, 133), // 0x30 '0'
    w(134, 134, 134, 134), // 0x31 '1'
    w(135, 135, 135, 135), // 0x32 '2'
    w(136, 136, 136, 136), // 0x33 '3'
    w(137, 137, 137, 137), // 0x34 '4'
    w(138, 138, 138, 138), // 0x35 '5'
    w(139, 139, 139, 139), // 0x36 '6'
    w(140, 140, 140, 140), // 0x37 '7'
    w(141, 141, 141, 141), // 0x38 '8'
    w(142, 142, 142, 142), // 0x39 '9'
    w(49, 49, 49, 49),     // 0x3A ':'
    w(50, 50, 50, 50),     // 0x3B ';'
    w(51, 51, 51, 51),     // 0x3C '<'
    w(52, 52, 52, 52),     // 0x3D '='
    w(53, 53, 53, 53),     // 0x3E '>'
    w(54, 54, 54, 54),     // 0x3F '?'
    w(55, 55, 55, 55),     // 0x40 '@'
    w(143, 143, 143, 143), // 0x41 'A'
    w(145, 151, 159, 159), // 0x42 'B'
    w(146, 152, 161, 161), // 0x43 'C'
    w(148, 154, 165, 165), // 0x44 'D'
    w(149, 155, 167, 167), // 0x45 'E'
    w(150, 160, 177, 177), // 0x46 'F'
    w(151, 161, 179, 179), // 0x47 'G'
    w(152, 162, 181, 181), // 0x48 'H'
    w(153, 163, 183, 183), // 0x49 'I'
    w(154, 168, 193, 193), // 0x4A 'J'
    w(155, 169, 195, 195), // 0x4B 'K'
    w(156, 170, 197, 197), // 0x4C 'L'
    w(157, 171, 199, 199), // 0x4D 'M'
    w(158, 172, 201, 201), // 0x4E 'N'
    w(159, 174, 205, 205), // 0x4F 'O'
    w(160, 181, 219, 219), // 0x50 'P'
    w(161, 182, 221, 221), // 0x51 'Q'
    w(162, 183, 223, 223), // 0x52 'R'
    w(163, 184, 225, 225), // 0x53 'S'
    w(165, 186, 228, 228), // 0x54 'T'
    w(166, 187, 230, 230), // 0x55 'U'
    w(167, 192, 240, 240), // 0x56 'V'
    w(168, 193, 242, 242), // 0x57 'W'
    w(169, 194, 244, 244), // 0x58 'X'
    w(170, 195, 246, 246), // 0x59 'Y'
    w(171, 198, 251, 251), // 0x5A 'Z'
    w(56, 56, 56, 56),     // 0x5B '['
    w(57, 57, 57, 57),     // 0x5C '\'
    w(58, 58, 58, 58),     // 0x5D ']'
    w(59, 59, 59, 59),     // 0x5E '^'
    w(60, 60, 60, 60),     // 0x5F '_'
    w(61, 61, 61, 61),     // 0x60 '`'
    w(143, 143, 144, 144), // 0x61 'a'
    w(145, 151, 160, 160), // 0x62 'b'
    w(146, 152, 162, 162), // 0x63 'c'
    w(148, 154, 166, 166), // 0x64 'd'
    w(149, 155, 168, 168), // 0x65 'e'
    w(150, 160, 178, 178), // 0x66 'f'
    w(151, 161, 180, 180), // 0x67 'g'
    w(152, 162, 182, 182), // 0x68 'h'
    w(153, 163, 184, 184), // 0x69 'i'
    w(154, 168, 194, 194), // 0x6A 'j'
    w(155, 169, 196, 196), // 0x6B 'k'
    w(156, 170, 198, 198), // 0x6C 'l'
    w(157, 171, 200, 200), // 0x6D 'm'
    w(158, 172, 202, 202), // 0x6E 'n'
    w(159, 174, 206, 206), // 0x6F 'o'
    w(160, 181, 220, 220), // 0x70 'p'
    w(161, 182, 222, 222), // 0x71 'q'
    w(162, 183, 224, 224), // 0x72 'r'
    w(163, 184, 226, 226), // 0x73 's'
    w(165, 186, 229, 229), // 0x74 't'
    w(166, 187, 231, 231), // 0x75 'u'
    w(167, 192, 241, 241), // 0x76 'v'
    w(168, 193, 243, 243), // 0x77 'w'
    w(169, 194, 245, 245), // 0x78 'x'
    w(170, 195, 247, 247), // 0x79 'y'
    w(171, 198, 252, 252), // 0x7A 'z'
    w(62, 62, 62, 62),     // 0x7B '{'
    w(63, 63, 63, 63),     // 0x7C '|'
    w(64, 64, 64, 64),     // 0x7D '}'
    w(65, 65, 65, 65),     // 0x7E '~'
    w(66, 66, 66, 66),     // 0x7F DEL
    w(67, 67, 67, 67),     // 0x80 '€'
    w(68, 68, 68, 68),     // 0x81 unassigned in CP1252
    w(69, 69, 69, 69),     // 0x82 '‚'
    w(70, 70, 70, 70),     // 0x83 'ƒ'
    w(71, 71, 71, 71),     // 0x84 '„'
    w(72, 72, 72, 72),     // 0x85 '…'
    w(73, 73, 73, 73),     // 0x86 '†'
    w(74, 74, 74, 74),     // 0x87 '‡'
    w(75, 75, 75, 75),     // 0x88 'ˆ'
    w(76, 76, 76, 76),     // 0x89 '‰'
    w(77, 77, 77, 77),     // 0x8A 'Š'
    w(78, 78, 78, 78),     // 0x8B '‹'
    w(79, 79, 79, 79),     // 0x8C 'Œ'
    w(80, 80, 80, 80),     // 0x8D unassigned in CP1252
    w(81, 81, 81, 81),     // 0x8E 'Ž'
    w(82, 82, 82, 82),     // 0x8F unassigned in CP1252
    w(83, 83, 83, 83),     // 0x90 unassigned in CP1252
    w(84, 84, 84, 84),     // 0x91 '‘'
    w(85, 85, 85, 85),     // 0x92 '’'
    w(86, 86, 86, 86),     // 0x93 '“'
    w(87, 87, 87, 87),     // 0x94 '”'
    w(88, 88, 88, 88),     // 0x95 '•'
    w(89, 89, 89, 89),     // 0x96 '–'
    w(90, 90, 90, 90),     // 0x97 '—'
    w(91, 91, 91, 91),     // 0x98 '˜'
    w(92, 92, 92, 92),     // 0x99 '™'
    w(93, 93, 93, 93),     // 0x9A 'š'
    w(94, 94, 94, 94),     // 0x9B '›'
    w(95, 95, 95, 95),     // 0x9C 'œ'
    w(96, 96, 96, 96),     // 0x9D unassigned in CP1252
    w(97, 97, 97, 97),     // 0x9E 'ž'
    w(98, 98, 98, 98),     // 0x9F 'Ÿ'
    w(99, 99, 99, 99),     // 0xA0 no-break space
    w(100, 100, 100, 100), // 0xA1 '¡'
    w(101, 101, 101, 101), // 0xA2 '¢'
    w(102, 102, 102, 102), // 0xA3 '£'
    w(103, 103, 103, 103), // 0xA4 '¤'
    w(104, 104, 104, 104), // 0xA5 '¥'
    w(105, 105, 105, 105), // 0xA6 '¦'
    w(106, 106, 106, 106), // 0xA7 '§'
    w(107, 107, 107, 107), // 0xA8 '¨'
    w(108, 108, 108, 108), // 0xA9 '©'
    w(109, 109, 109, 109), // 0xAA 'ª'
    w(110, 110, 110, 110), // 0xAB left guillemet
    w(111, 111, 111, 111), // 0xAC '¬'
    w(112, 112, 112, 112), // 0xAD soft hyphen
    w(113, 113, 113, 113), // 0xAE '®'
    w(114, 114, 114, 114), // 0xAF '¯'
    w(115, 115, 115, 115), // 0xB0 '°'
    w(116, 116, 116, 116), // 0xB1 '±'
    w(117, 117, 117, 117), // 0xB2 '²'
    w(118, 118, 118, 118), // 0xB3 '³'
    w(119, 119, 119, 119), // 0xB4 '´'
    w(120, 120, 120, 120), // 0xB5 'µ'
    w(121, 121, 121, 121), // 0xB6 '¶'
    w(122, 122, 122, 122), // 0xB7 '·'
    w(123, 123, 123, 123), // 0xB8 '¸'
    w(124, 124, 124, 124), // 0xB9 '¹'
    w(125, 125, 125, 125), // 0xBA 'º'
    w(126, 126, 126, 126), // 0xBB right guillemet
    w(127, 127, 127, 127), // 0xBC '¼'
    w(128, 128, 128, 128), // 0xBD '½'
    w(129, 129, 129, 129), // 0xBE '¾'
    w(130, 130, 130, 130), // 0xBF '¿'
    w(143, 144, 145, 145), // 0xC0 'À'
    w(143, 145, 147, 147), // 0xC1 'Á'
    w(143, 146, 149, 149), // 0xC2 'Â'
    w(143, 147, 151, 151), // 0xC3 'Ã'
    w(143, 148, 153, 153), // 0xC4 'Ä'
    w(143, 149, 155, 155), // 0xC5 'Å'
    w(144, 150, 157, 157), // 0xC6 'Æ'
    w(147, 153, 163, 163), // 0xC7 'Ç'
    w(149, 156, 169, 169), // 0xC8 'È'
    w(149, 157, 171, 171), // 0xC9 'É'
    w(149, 158, 173, 173), // 0xCA 'Ê'
    w(149, 159, 175, 175), // 0xCB 'Ë'
    w(153, 164, 185, 185), // 0xCC 'Ì'
    w(153, 165, 187, 187), // 0xCD 'Í'
    w(153, 166, 189, 189), // 0xCE 'Î'
    w(153, 167, 191, 191), // 0xCF 'Ï'
    w(172, 199, 253, 253), // 0xD0 'Ð'
    w(158, 173, 203, 203), // 0xD1 'Ñ'
    w(159, 175, 207, 207), // 0xD2 'Ò'
    w(159, 176, 209, 209), // 0xD3 'Ó'
    w(159, 177, 211, 211), // 0xD4 'Ô'
    w(159, 178, 213, 213), // 0xD5 'Õ'
    w(159, 179, 215, 215), // 0xD6 'Ö'
    w(131, 131, 131, 131), // 0xD7 '×'
    w(159, 180, 217, 217), // 0xD8 'Ø'
    w(166, 188, 232, 232), // 0xD9 'Ù'
    w(166, 189, 234, 234), // 0xDA 'Ú'
    w(166, 190, 236, 236), // 0xDB 'Û'
    w(166, 191, 238, 238), // 0xDC 'Ü'
    w(170, 196, 248, 248), // 0xDD 'Ý'
    w(173, 200, 255, 255), // 0xDE 'Þ'
    w(164, 185, 227, 227), // 0xDF 'ß'
    w(143, 144, 146, 146), // 0xE0 'à'
    w(143, 145, 148, 148), // 0xE1 'á'
    w(143, 146, 150, 150), // 0xE2 'â'
    w(143, 147, 152, 152), // 0xE3 'ã'
    w(143, 148, 154, 154), // 0xE4 'ä'
    w(143, 149, 156, 156), // 0xE5 'å'
    w(144, 150, 158, 158), // 0xE6 'æ'
    w(147, 153, 164, 164), // 0xE7 'ç'
    w(149, 156, 170, 170), // 0xE8 'è'
    w(149, 157, 172, 172), // 0xE9 'é'
    w(149, 158, 174, 174), // 0xEA 'ê'
    w(149, 159, 176, 176), // 0xEB 'ë'
    w(153, 164, 186, 186), // 0xEC 'ì'
    w(153, 165, 188, 188), // 0xED 'í'
    w(153, 166, 190, 190), // 0xEE 'î'
    w(153, 167, 192, 192), // 0xEF 'ï'
    w(172, 199, 254, 254), // 0xF0 'ð'
    w(158, 173, 204, 204), // 0xF1 'ñ'
    w(159, 175, 208, 208), // 0xF2 'ò'
    w(159, 176, 210, 210), // 0xF3 'ó'
    w(159, 177, 212, 212), // 0xF4 'ô'
    w(159, 178, 214, 214), // 0xF5 'õ'
    w(159, 179, 216, 216), // 0xF6 'ö'
    w(132, 132, 132, 132), // 0xF7 '÷'
    w(159, 180, 218, 218), // 0xF8 'ø'
    w(166, 188, 233, 233), // 0xF9 'ù'
    w(166, 189, 235, 235), // 0xFA 'ú'
    w(166, 190, 237, 237), // 0xFB 'û'
    w(166, 191, 239, 239), // 0xFC 'ü'
    w(170, 196, 249, 249), // 0xFD 'ý'
    w(173, 200, 256, 256), // 0xFE 'þ'
    w(170, 197, 250, 250), // 0xFF 'ÿ'
];

/// The sort table of the versionless `Latin1_General_*` collations, indexed like
/// [`SQL_CP1_WEIGHTS`].
///
/// Lower case sorts before upper case; the superscript digits and the ordinal indicators
/// `ª` and `º` differ from their base at the case level. The characters [`WINDOWS`]
/// lists as `last` or `ignored` keep a row here, read at the last level.
const WINDOWS_WEIGHTS: [Weights; 256] = [
    w(1, 1, 1, 1),         // 0x00 NUL
    w(2, 2, 2, 2),         // 0x01 control
    w(3, 3, 3, 3),         // 0x02 control
    w(4, 4, 4, 4),         // 0x03 control
    w(5, 5, 5, 5),         // 0x04 control
    w(6, 6, 6, 6),         // 0x05 control
    w(7, 7, 7, 7),         // 0x06 control
    w(8, 8, 8, 8),         // 0x07 control
    w(9, 9, 9, 9),         // 0x08 control
    w(40, 40, 40, 40),     // 0x09 tab
    w(41, 41, 41, 41),     // 0x0A LF
    w(42, 42, 42, 42),     // 0x0B control
    w(43, 43, 43, 43),     // 0x0C control
    w(44, 44, 44, 44),     // 0x0D CR
    w(10, 10, 10, 10),     // 0x0E control
    w(11, 11, 11, 11),     // 0x0F control
    w(12, 12, 12, 12),     // 0x10 control
    w(13, 13, 13, 13),     // 0x11 control
    w(14, 14, 14, 14),     // 0x12 control
    w(15, 15, 15, 15),     // 0x13 control
    w(16, 16, 16, 16),     // 0x14 control
    w(17, 17, 17, 17),     // 0x15 control
    w(18, 18, 18, 18),     // 0x16 control
    w(19, 19, 19, 19),     // 0x17 control
    w(20, 20, 20, 20),     // 0x18 control
    w(21, 21, 21, 21),     // 0x19 control
    w(22, 22, 22, 22),     // 0x1A control
    w(23, 23, 23, 23),     // 0x1B ESC
    w(24, 24, 24, 24),     // 0x1C control
    w(25, 25, 25, 25),     // 0x1D control
    w(26, 26, 26, 26),     // 0x1E control
    w(27, 27, 27, 27),     // 0x1F control
    w(1, 1, 1, 1),         // 0x20 space
    w(45, 45, 45, 45),     // 0x21 '!'
    w(46, 46, 46, 46),     // 0x22 '"'
    w(47, 47, 47, 47),     // 0x23 '#'
    w(48, 48, 48, 48),     // 0x24 '$'
    w(49, 49, 49, 49),     // 0x25 '%'
    w(50, 50, 50, 50),     // 0x26 '&'
    w(34, 34, 34, 34),     // 0x27 '''
    w(51, 51, 51, 51),     // 0x28 '('
    w(52, 52, 52, 52),     // 0x29 ')'
    w(53, 53, 53, 53),     // 0x2A '*'
    w(87, 88, 88, 87),     // 0x2B '+'
    w(54, 54, 54, 54),     // 0x2C ','
    w(35, 35, 35, 35),     // 0x2D '-'
    w(55, 55, 55, 55),     // 0x2E '.'
    w(56, 56, 56, 56),     // 0x2F '/'
    w(114, 115, 115, 114), // 0x30 '0'
    w(118, 119, 119, 118), // 0x31 '1'
    w(119, 120, 121, 120), // 0x32 '2'
    w(120, 121, 123, 122), // 0x33 '3'
    w(121, 122, 125, 124), // 0x34 '4'
    w(122, 123, 126, 125), // 0x35 '5'
    w(123, 124, 127, 126), // 0x36 '6'
    w(124, 125, 128, 127), // 0x37 '7'
    w(125, 126, 129, 128), // 0x38 '8'
    w(126, 127, 130, 129), // 0x39 '9'
    w(57, 57, 57, 57),     // 0x3A ':'
    w(58, 58, 58, 58),     // 0x3B ';'
    w(88, 89, 89, 88),     // 0x3C '<'
    w(89, 90, 90, 89),     // 0x3D '='
    w(90, 91, 91, 90),     // 0x3E '>'
    w(59, 59, 59, 59),     // 0x3F '?'
    w(60, 60, 60, 60),     // 0x40 '@'
    w(127, 128, 132, 132), // 0x41 'A'
    w(129, 137, 149, 136), // 0x42 'B'
    w(130, 138, 151, 138), // 0x43 'C'
    w(131, 140, 155, 140), // 0x44 'D'
    w(132, 142, 159, 142), // 0x45 'E'
    w(133, 147, 169, 144), // 0x46 'F'
    w(134, 149, 172, 146), // 0x47 'G'
    w(135, 150, 174, 148), // 0x48 'H'
    w(136, 151, 176, 150), // 0x49 'I'
    w(137, 156, 186, 152), // 0x4A 'J'
    w(138, 157, 188, 154), // 0x4B 'K'
    w(139, 158, 190, 156), // 0x4C 'L'
    w(140, 159, 192, 158), // 0x4D 'M'
    w(141, 160, 194, 160), // 0x4E 'N'
    w(142, 162, 198, 163), // 0x4F 'O'
    w(144, 171, 215, 167), // 0x50 'P'
    w(145, 172, 217, 169), // 0x51 'Q'
    w(146, 173, 219, 171), // 0x52 'R'
    w(147, 174, 221, 173), // 0x53 'S'
    w(149, 177, 226, 176), // 0x54 'T'
    w(152, 180, 231, 181), // 0x55 'U'
    w(153, 185, 241, 183), // 0x56 'V'
    w(154, 186, 243, 185), // 0x57 'W'
    w(155, 187, 245, 187), // 0x58 'X'
    w(156, 188, 247, 189), // 0x59 'Y'
    w(157, 191, 253, 191), // 0x5A 'Z'
    w(61, 61, 61, 61),     // 0x5B '['
    w(62, 62, 62, 62),     // 0x5C '\'
    w(63, 63, 63, 63),     // 0x5D ']'
    w(64, 64, 64, 64),     // 0x5E '^'
    w(65, 66, 66, 65),     // 0x5F '_'
    w(66, 67, 67, 66),     // 0x60 '`'
    w(127, 128, 131, 130), // 0x61 'a'
    w(129, 137, 148, 135), // 0x62 'b'
    w(130, 138, 150, 137), // 0x63 'c'
    w(131, 140, 154, 139), // 0x64 'd'
    w(132, 142, 158, 141), // 0x65 'e'
    w(133, 147, 168, 143), // 0x66 'f'
    w(134, 149, 171, 145), // 0x67 'g'
    w(135, 150, 173, 147), // 0x68 'h'
    w(136, 151, 175, 149), // 0x69 'i'
    w(137, 156, 185, 151), // 0x6A 'j'
    w(138, 157, 187, 153), // 0x6B 'k'
    w(139, 158, 189, 155), // 0x6C 'l'
    w(140, 159, 191, 157), // 0x6D 'm'
    w(141, 160, 193, 159), // 0x6E 'n'
    w(142, 162, 197, 161), // 0x6F 'o'
    w(144, 171, 214, 166), // 0x70 'p'
    w(145, 172, 216, 168), // 0x71 'q'
    w(146, 173, 218, 170), // 0x72 'r'
    w(147, 174, 220, 172), // 0x73 's'
    w(149, 177, 225, 175), // 0x74 't'
    w(152, 180, 230, 180), // 0x75 'u'
    w(153, 185, 240, 182), // 0x76 'v'
    w(154, 186, 242, 184), // 0x77 'w'
    w(155, 187, 244, 186), // 0x78 'x'
    w(156, 188, 246, 188), // 0x79 'y'
    w(157, 191, 252, 190), // 0x7A 'z'
    w(67, 68, 68, 67),     // 0x7B '{'
    w(68, 69, 69, 68),     // 0x7C '|'
    w(69, 70, 70, 69),     // 0x7D '}'
    w(70, 71, 71, 70),     // 0x7E '~'
    w(28, 28, 28, 28),     // 0x7F DEL
    w(113, 114, 114, 113), // 0x80 '€'
    w(29, 29, 29, 29),     // 0x81 unassigned in CP1252
    w(81, 82, 82, 81),     // 0x82 '‚'
    w(133, 148, 170, 143), // 0x83 'ƒ'
    w(84, 85, 85, 84),     // 0x84 '„'
    w(111, 112, 112, 111), // 0x85 '…'
    w(108, 109, 109, 108), // 0x86 '†'
    w(109, 110, 110, 109), // 0x87 '‡'
    w(64, 65, 65, 64),     // 0x88 'ˆ'
    w(112, 113, 113, 112), // 0x89 '‰'
    w(147, 175, 223, 173), // 0x8A 'Š'
    w(85, 86, 86, 85),     // 0x8B '‹'
    w(143, 170, 213, 165), // 0x8C 'Œ'
    w(30, 30, 30, 30),     // 0x8D unassigned in CP1252
    w(157, 192, 255, 191), // 0x8E 'Ž'
    w(31, 31, 31, 31),     // 0x8F unassigned in CP1252
    w(32, 32, 32, 32),     // 0x90 unassigned in CP1252
    w(79, 80, 80, 79),     // 0x91 '‘'
    w(80, 81, 81, 80),     // 0x92 '’'
    w(82, 83, 83, 82),     // 0x93 '“'
    w(83, 84, 84, 83),     // 0x94 '”'
    w(110, 111, 111, 110), // 0x95 '•'
    w(37, 37, 37, 37),     // 0x96 '–'
    w(38, 38, 38, 38),     // 0x97 '—'
    w(78, 79, 79, 78),     // 0x98 '˜'
    w(151, 179, 229, 179), // 0x99 '™'
    w(147, 175, 222, 172), // 0x9A 'š'
    w(86, 87, 87, 86),     // 0x9B '›'
    w(143, 170, 212, 164), // 0x9C 'œ'
    w(33, 33, 33, 33),     // 0x9D unassigned in CP1252
    w(157, 192, 254, 190), // 0x9E 'ž'
    w(156, 190, 251, 189), // 0x9F 'Ÿ'
    w(39, 39, 39, 39),     // 0xA0 no-break space
    w(71, 72, 72, 71),     // 0xA1 '¡'
    w(96, 97, 97, 96),     // 0xA2 '¢'
    w(97, 98, 98, 97),     // 0xA3 '£'
    w(98, 99, 99, 98),     // 0xA4 '¤'
    w(99, 100, 100, 99),   // 0xA5 '¥'
    w(72, 73, 73, 72),     // 0xA6 '¦'
    w(100, 101, 101, 100), // 0xA7 '§'
    w(73, 74, 74, 73),     // 0xA8 '¨'
    w(101, 102, 102, 101), // 0xA9 '©'
    w(127, 129, 133, 131), // 0xAA 'ª'
    w(92, 93, 93, 92),     // 0xAB left guillemet
    w(102, 103, 103, 102), // 0xAC '¬'
    w(36, 36, 36, 36),     // 0xAD soft hyphen
    w(103, 104, 104, 103), // 0xAE '®'
    w(74, 75, 75, 74),     // 0xAF '¯'
    w(104, 105, 105, 104), // 0xB0 '°'
    w(91, 92, 92, 91),     // 0xB1 '±'
    w(119, 120, 122, 121), // 0xB2 '²'
    w(120, 121, 124, 123), // 0xB3 '³'
    w(75, 76, 76, 75),     // 0xB4 '´'
    w(105, 106, 106, 105), // 0xB5 'µ'
    w(106, 107, 107, 106), // 0xB6 '¶'
    w(107, 108, 108, 107), // 0xB7 '·'
    w(76, 77, 77, 76),     // 0xB8 '¸'
    w(118, 119, 120, 119), // 0xB9 '¹'
    w(142, 163, 199, 162), // 0xBA 'º'
    w(93, 94, 94, 93),     // 0xBB right guillemet
    w(115, 116, 116, 115), // 0xBC '¼'
    w(116, 117, 117, 116), // 0xBD '½'
    w(117, 118, 118, 117), // 0xBE '¾'
    w(77, 78, 78, 77),     // 0xBF '¿'
    w(127, 131, 137, 132), // 0xC0 'À'
    w(127, 130, 135, 132), // 0xC1 'Á'
    w(127, 132, 139, 132), // 0xC2 'Â'
    w(127, 134, 143, 132), // 0xC3 'Ã'
    w(127, 133, 141, 132), // 0xC4 'Ä'
    w(127, 135, 145, 132), // 0xC5 'Å'
    w(128, 136, 147, 134), // 0xC6 'Æ'
    w(130, 139, 153, 138), // 0xC7 'Ç'
    w(132, 144, 163, 142), // 0xC8 'È'
    w(132, 143, 161, 142), // 0xC9 'É'
    w(132, 145, 165, 142), // 0xCA 'Ê'
    w(132, 146, 167, 142), // 0xCB 'Ë'
    w(136, 153, 180, 150), // 0xCC 'Ì'
    w(136, 152, 178, 150), // 0xCD 'Í'
    w(136, 154, 182, 150), // 0xCE 'Î'
    w(136, 155, 184, 150), // 0xCF 'Ï'
    w(131, 141, 157, 140), // 0xD0 'Ð'
    w(141, 161, 196, 160), // 0xD1 'Ñ'
    w(142, 165, 203, 163), // 0xD2 'Ò'
    w(142, 164, 201, 163), // 0xD3 'Ó'
    w(142, 166, 205, 163), // 0xD4 'Ô'
    w(142, 168, 209, 163), // 0xD5 'Õ'
    w(142, 167, 207, 163), // 0xD6 'Ö'
    w(94, 95, 95, 94),     // 0xD7 '×'
    w(142, 169, 211, 163), // 0xD8 'Ø'
    w(152, 182, 235, 181), // 0xD9 'Ù'
    w(152, 181, 233, 181), // 0xDA 'Ú'
    w(152, 183, 237, 181), // 0xDB 'Û'
    w(152, 184, 239, 181), // 0xDC 'Ü'
    w(156, 189, 249, 189), // 0xDD 'Ý'
    w(150, 178, 228, 178), // 0xDE 'Þ'
    w(148, 176, 224, 174), // 0xDF 'ß'
    w(127, 131, 136, 130), // 0xE0 'à'
    w(127, 130, 134, 130), // 0xE1 'á'
    w(127, 132, 138, 130), // 0xE2 'â'
    w(127, 134, 142, 130), // 0xE3 'ã'
    w(127, 133, 140, 130), // 0xE4 'ä'
    w(127, 135, 144, 130), // 0xE5 'å'
    w(128, 136, 146, 133), // 0xE6 'æ'
    w(130, 139, 152, 137), // 0xE7 'ç'
    w(132, 144, 162, 141), // 0xE8 'è'
    w(132, 143, 160, 141), // 0xE9 'é'
    w(132, 145, 164, 141), // 0xEA 'ê'
    w(132, 146, 166, 141), // 0xEB 'ë'
    w(136, 153, 179, 149), // 0xEC 'ì'
    w(136, 152, 177, 149), // 0xED 'í'
    w(136, 154, 181, 149), // 0xEE 'î'
    w(136, 155, 183, 149), // 0xEF 'ï'
    w(131, 141, 156, 139), // 0xF0 'ð'
    w(141, 161, 195, 159), // 0xF1 'ñ'
    w(142, 165, 202, 161), // 0xF2 'ò'
    w(142, 164, 200, 161), // 0xF3 'ó'
    w(142, 166, 204, 161), // 0xF4 'ô'
    w(142, 168, 208, 161), // 0xF5 'õ'
    w(142, 167, 206, 161), // 0xF6 'ö'
    w(95, 96, 96, 95),     // 0xF7 '÷'
    w(142, 169, 210, 161), // 0xF8 'ø'
    w(152, 182, 234, 180), // 0xF9 'ù'
    w(152, 181, 232, 180), // 0xFA 'ú'
    w(152, 183, 236, 180), // 0xFB 'û'
    w(152, 184, 238, 180), // 0xFC 'ü'
    w(156, 189, 248, 188), // 0xFD 'ý'
    w(150, 178, 227, 177), // 0xFE 'þ'
    w(156, 190, 250, 188), // 0xFF 'ÿ'
];

/// The sort table of the `Latin1_General_100_*` collations, indexed like
/// [`SQL_CP1_WEIGHTS`]. It differs from [`WINDOWS_WEIGHTS`] in the place of the currency
/// signs, of the five bytes code page 1252 leaves unassigned and of `¾`, and in the soft
/// hyphen, which it ignores altogether.
const WINDOWS_100_WEIGHTS: [Weights; 256] = [
    w(1, 1, 1, 1),         // 0x00 NUL
    w(2, 2, 2, 2),         // 0x01 control
    w(3, 3, 3, 3),         // 0x02 control
    w(4, 4, 4, 4),         // 0x03 control
    w(5, 5, 5, 5),         // 0x04 control
    w(6, 6, 6, 6),         // 0x05 control
    w(7, 7, 7, 7),         // 0x06 control
    w(8, 8, 8, 8),         // 0x07 control
    w(9, 9, 9, 9),         // 0x08 control
    w(34, 34, 34, 34),     // 0x09 tab
    w(35, 35, 35, 35),     // 0x0A LF
    w(36, 36, 36, 36),     // 0x0B control
    w(37, 37, 37, 37),     // 0x0C control
    w(38, 38, 38, 38),     // 0x0D CR
    w(10, 10, 10, 10),     // 0x0E control
    w(11, 11, 11, 11),     // 0x0F control
    w(12, 12, 12, 12),     // 0x10 control
    w(13, 13, 13, 13),     // 0x11 control
    w(14, 14, 14, 14),     // 0x12 control
    w(15, 15, 15, 15),     // 0x13 control
    w(16, 16, 16, 16),     // 0x14 control
    w(17, 17, 17, 17),     // 0x15 control
    w(18, 18, 18, 18),     // 0x16 control
    w(19, 19, 19, 19),     // 0x17 control
    w(20, 20, 20, 20),     // 0x18 control
    w(21, 21, 21, 21),     // 0x19 control
    w(22, 22, 22, 22),     // 0x1A control
    w(23, 23, 23, 23),     // 0x1B ESC
    w(24, 24, 24, 24),     // 0x1C control
    w(25, 25, 25, 25),     // 0x1D control
    w(26, 26, 26, 26),     // 0x1E control
    w(27, 27, 27, 27),     // 0x1F control
    w(1, 1, 1, 1),         // 0x20 space
    w(39, 39, 39, 39),     // 0x21 '!'
    w(40, 40, 40, 40),     // 0x22 '"'
    w(41, 41, 41, 41),     // 0x23 '#'
    w(42, 42, 42, 42),     // 0x24 '$'
    w(43, 43, 43, 43),     // 0x25 '%'
    w(44, 44, 44, 44),     // 0x26 '&'
    w(29, 29, 29, 29),     // 0x27 '''
    w(45, 45, 45, 45),     // 0x28 '('
    w(46, 46, 46, 46),     // 0x29 ')'
    w(47, 47, 47, 47),     // 0x2A '*'
    w(86, 86, 86, 86),     // 0x2B '+'
    w(48, 48, 48, 48),     // 0x2C ','
    w(30, 30, 30, 30),     // 0x2D '-'
    w(49, 49, 49, 49),     // 0x2E '.'
    w(50, 50, 50, 50),     // 0x2F '/'
    w(109, 113, 113, 109), // 0x30 '0'
    w(111, 116, 116, 111), // 0x31 '1'
    w(112, 117, 118, 113), // 0x32 '2'
    w(113, 118, 120, 115), // 0x33 '3'
    w(114, 120, 123, 117), // 0x34 '4'
    w(115, 121, 124, 118), // 0x35 '5'
    w(116, 122, 125, 119), // 0x36 '6'
    w(117, 123, 126, 120), // 0x37 '7'
    w(118, 124, 127, 121), // 0x38 '8'
    w(119, 125, 128, 122), // 0x39 '9'
    w(51, 51, 51, 51),     // 0x3A ':'
    w(52, 52, 52, 52),     // 0x3B ';'
    w(87, 87, 87, 87),     // 0x3C '<'
    w(88, 88, 88, 88),     // 0x3D '='
    w(89, 89, 89, 89),     // 0x3E '>'
    w(53, 53, 53, 53),     // 0x3F '?'
    w(54, 54, 54, 54),     // 0x40 '@'
    w(120, 126, 130, 125), // 0x41 'A'
    w(122, 135, 147, 129), // 0x42 'B'
    w(123, 136, 149, 131), // 0x43 'C'
    w(124, 138, 153, 133), // 0x44 'D'
    w(125, 140, 157, 135), // 0x45 'E'
    w(126, 145, 167, 137), // 0x46 'F'
    w(127, 147, 170, 139), // 0x47 'G'
    w(128, 148, 172, 141), // 0x48 'H'
    w(129, 149, 174, 143), // 0x49 'I'
    w(130, 154, 184, 145), // 0x4A 'J'
    w(131, 155, 186, 147), // 0x4B 'K'
    w(132, 156, 188, 149), // 0x4C 'L'
    w(133, 157, 190, 151), // 0x4D 'M'
    w(134, 158, 192, 153), // 0x4E 'N'
    w(135, 160, 196, 156), // 0x4F 'O'
    w(137, 169, 213, 160), // 0x50 'P'
    w(138, 170, 215, 162), // 0x51 'Q'
    w(139, 171, 217, 164), // 0x52 'R'
    w(140, 172, 219, 166), // 0x53 'S'
    w(142, 175, 224, 169), // 0x54 'T'
    w(145, 178, 229, 174), // 0x55 'U'
    w(146, 183, 239, 176), // 0x56 'V'
    w(147, 184, 241, 178), // 0x57 'W'
    w(148, 185, 243, 180), // 0x58 'X'
    w(149, 186, 245, 182), // 0x59 'Y'
    w(150, 189, 251, 184), // 0x5A 'Z'
    w(55, 55, 55, 55),     // 0x5B '['
    w(56, 56, 56, 56),     // 0x5C '\'
    w(57, 57, 57, 57),     // 0x5D ']'
    w(58, 58, 58, 58),     // 0x5E '^'
    w(59, 59, 59, 59),     // 0x5F '_'
    w(60, 60, 60, 60),     // 0x60 '`'
    w(120, 126, 129, 123), // 0x61 'a'
    w(122, 135, 146, 128), // 0x62 'b'
    w(123, 136, 148, 130), // 0x63 'c'
    w(124, 138, 152, 132), // 0x64 'd'
    w(125, 140, 156, 134), // 0x65 'e'
    w(126, 145, 166, 136), // 0x66 'f'
    w(127, 147, 169, 138), // 0x67 'g'
    w(128, 148, 171, 140), // 0x68 'h'
    w(129, 149, 173, 142), // 0x69 'i'
    w(130, 154, 183, 144), // 0x6A 'j'
    w(131, 155, 185, 146), // 0x6B 'k'
    w(132, 156, 187, 148), // 0x6C 'l'
    w(133, 157, 189, 150), // 0x6D 'm'
    w(134, 158, 191, 152), // 0x6E 'n'
    w(135, 160, 195, 154), // 0x6F 'o'
    w(137, 169, 212, 159), // 0x70 'p'
    w(138, 170, 214, 161), // 0x71 'q'
    w(139, 171, 216, 163), // 0x72 'r'
    w(140, 172, 218, 165), // 0x73 's'
    w(142, 175, 223, 168), // 0x74 't'
    w(145, 178, 228, 173), // 0x75 'u'
    w(146, 183, 238, 175), // 0x76 'v'
    w(147, 184, 240, 177), // 0x77 'w'
    w(148, 185, 242, 179), // 0x78 'x'
    w(149, 186, 244, 181), // 0x79 'y'
    w(150, 189, 250, 183), // 0x7A 'z'
    w(61, 61, 61, 61),     // 0x7B '{'
    w(62, 62, 62, 62),     // 0x7C '|'
    w(63, 63, 63, 63),     // 0x7D '}'
    w(64, 64, 64, 64),     // 0x7E '~'
    w(28, 28, 28, 28),     // 0x7F DEL
    w(85, 85, 85, 85),     // 0x80 '€'
    w(108, 108, 108, 108), // 0x81 unassigned in CP1252
    w(75, 75, 75, 75),     // 0x82 '‚'
    w(126, 146, 168, 136), // 0x83 'ƒ'
    w(78, 78, 78, 78),     // 0x84 '„'
    w(103, 103, 103, 103), // 0x85 '…'
    w(104, 104, 104, 104), // 0x86 '†'
    w(105, 105, 105, 105), // 0x87 '‡'
    w(58, 58, 58, 58),     // 0x88 'ˆ'
    w(107, 107, 107, 107), // 0x89 '‰'
    w(140, 173, 221, 166), // 0x8A 'Š'
    w(79, 79, 79, 79),     // 0x8B '‹'
    w(136, 168, 211, 158), // 0x8C 'Œ'
    w(108, 109, 109, 108), // 0x8D unassigned in CP1252
    w(150, 190, 253, 184), // 0x8E 'Ž'
    w(108, 110, 110, 108), // 0x8F unassigned in CP1252
    w(108, 111, 111, 108), // 0x90 unassigned in CP1252
    w(73, 73, 73, 73),     // 0x91 '‘'
    w(74, 74, 74, 74),     // 0x92 '’'
    w(76, 76, 76, 76),     // 0x93 '“'
    w(77, 77, 77, 77),     // 0x94 '”'
    w(106, 106, 106, 106), // 0x95 '•'
    w(31, 31, 31, 31),     // 0x96 '–'
    w(32, 32, 32, 32),     // 0x97 '—'
    w(72, 72, 72, 72),     // 0x98 '˜'
    w(144, 177, 227, 172), // 0x99 '™'
    w(140, 173, 220, 165), // 0x9A 'š'
    w(80, 80, 80, 80),     // 0x9B '›'
    w(136, 168, 210, 157), // 0x9C 'œ'
    w(108, 112, 112, 108), // 0x9D unassigned in CP1252
    w(150, 190, 252, 183), // 0x9E 'ž'
    w(149, 188, 249, 182), // 0x9F 'Ÿ'
    w(33, 33, 33, 33),     // 0xA0 no-break space
    w(65, 65, 65, 65),     // 0xA1 '¡'
    w(81, 81, 81, 81),     // 0xA2 '¢'
    w(82, 82, 82, 82),     // 0xA3 '£'
    w(83, 83, 83, 83),     // 0xA4 '¤'
    w(84, 84, 84, 84),     // 0xA5 '¥'
    w(66, 66, 66, 66),     // 0xA6 '¦'
    w(95, 95, 95, 95),     // 0xA7 '§'
    w(67, 67, 67, 67),     // 0xA8 '¨'
    w(96, 96, 96, 96),     // 0xA9 '©'
    w(120, 127, 131, 124), // 0xAA 'ª'
    w(91, 91, 91, 91),     // 0xAB left guillemet
    w(97, 97, 97, 97),     // 0xAC '¬'
    w(1, 1, 1, 1),         // 0xAD soft hyphen
    w(98, 98, 98, 98),     // 0xAE '®'
    w(68, 68, 68, 68),     // 0xAF '¯'
    w(99, 99, 99, 99),     // 0xB0 '°'
    w(90, 90, 90, 90),     // 0xB1 '±'
    w(112, 117, 119, 114), // 0xB2 '²'
    w(113, 118, 121, 116), // 0xB3 '³'
    w(69, 69, 69, 69),     // 0xB4 '´'
    w(100, 100, 100, 100), // 0xB5 'µ'
    w(101, 101, 101, 101), // 0xB6 '¶'
    w(102, 102, 102, 102), // 0xB7 '·'
    w(70, 70, 70, 70),     // 0xB8 '¸'
    w(111, 116, 117, 112), // 0xB9 '¹'
    w(135, 161, 197, 155), // 0xBA 'º'
    w(92, 92, 92, 92),     // 0xBB right guillemet
    w(110, 114, 114, 110), // 0xBC '¼'
    w(110, 115, 115, 110), // 0xBD '½'
    w(113, 119, 122, 115), // 0xBE '¾'
    w(71, 71, 71, 71),     // 0xBF '¿'
    w(120, 129, 135, 125), // 0xC0 'À'
    w(120, 128, 133, 125), // 0xC1 'Á'
    w(120, 130, 137, 125), // 0xC2 'Â'
    w(120, 132, 141, 125), // 0xC3 'Ã'
    w(120, 131, 139, 125), // 0xC4 'Ä'
    w(120, 133, 143, 125), // 0xC5 'Å'
    w(121, 134, 145, 127), // 0xC6 'Æ'
    w(123, 137, 151, 131), // 0xC7 'Ç'
    w(125, 142, 161, 135), // 0xC8 'È'
    w(125, 141, 159, 135), // 0xC9 'É'
    w(125, 143, 163, 135), // 0xCA 'Ê'
    w(125, 144, 165, 135), // 0xCB 'Ë'
    w(129, 151, 178, 143), // 0xCC 'Ì'
    w(129, 150, 176, 143), // 0xCD 'Í'
    w(129, 152, 180, 143), // 0xCE 'Î'
    w(129, 153, 182, 143), // 0xCF 'Ï'
    w(124, 139, 155, 133), // 0xD0 'Ð'
    w(134, 159, 194, 153), // 0xD1 'Ñ'
    w(135, 163, 201, 156), // 0xD2 'Ò'
    w(135, 162, 199, 156), // 0xD3 'Ó'
    w(135, 164, 203, 156), // 0xD4 'Ô'
    w(135, 166, 207, 156), // 0xD5 'Õ'
    w(135, 165, 205, 156), // 0xD6 'Ö'
    w(93, 93, 93, 93),     // 0xD7 '×'
    w(135, 167, 209, 156), // 0xD8 'Ø'
    w(145, 180, 233, 174), // 0xD9 'Ù'
    w(145, 179, 231, 174), // 0xDA 'Ú'
    w(145, 181, 235, 174), // 0xDB 'Û'
    w(145, 182, 237, 174), // 0xDC 'Ü'
    w(149, 187, 247, 182), // 0xDD 'Ý'
    w(143, 176, 226, 171), // 0xDE 'Þ'
    w(141, 174, 222, 167), // 0xDF 'ß'
    w(120, 129, 134, 123), // 0xE0 'à'
    w(120, 128, 132, 123), // 0xE1 'á'
    w(120, 130, 136, 123), // 0xE2 'â'
    w(120, 132, 140, 123), // 0xE3 'ã'
    w(120, 131, 138, 123), // 0xE4 'ä'
    w(120, 133, 142, 123), // 0xE5 'å'
    w(121, 134, 144, 126), // 0xE6 'æ'
    w(123, 137, 150, 130), // 0xE7 'ç'
    w(125, 142, 160, 134), // 0xE8 'è'
    w(125, 141, 158, 134), // 0xE9 'é'
    w(125, 143, 162, 134), // 0xEA 'ê'
    w(125, 144, 164, 134), // 0xEB 'ë'
    w(129, 151, 177, 142), // 0xEC 'ì'
    w(129, 150, 175, 142), // 0xED 'í'
    w(129, 152, 179, 142), // 0xEE 'î'
    w(129, 153, 181, 142), // 0xEF 'ï'
    w(124, 139, 154, 132), // 0xF0 'ð'
    w(134, 159, 193, 152), // 0xF1 'ñ'
    w(135, 163, 200, 154), // 0xF2 'ò'
    w(135, 162, 198, 154), // 0xF3 'ó'
    w(135, 164, 202, 154), // 0xF4 'ô'
    w(135, 166, 206, 154), // 0xF5 'õ'
    w(135, 165, 204, 154), // 0xF6 'ö'
    w(94, 94, 94, 94),     // 0xF7 '÷'
    w(135, 167, 208, 154), // 0xF8 'ø'
    w(145, 180, 232, 173), // 0xF9 'ù'
    w(145, 179, 230, 173), // 0xFA 'ú'
    w(145, 181, 234, 173), // 0xFB 'û'
    w(145, 182, 236, 173), // 0xFC 'ü'
    w(149, 187, 246, 181), // 0xFD 'ý'
    w(143, 176, 225, 170), // 0xFE 'þ'
    w(149, 188, 248, 181), // 0xFF 'ÿ'
];

/// The code page 1252 byte of `c`, or `None` when the code page has no such character.
///
/// A thin name for [`code_page::encode`] under the default collation: the weight tables
/// ([`SQL_CP1_WEIGHTS`] and its two Windows siblings) are indexed by that byte, and the code page table itself lives in
/// [`code_page`](crate::code_page), which is the only place that holds it.
pub(crate) fn cp1252_byte(c: char) -> Option<u8> {
    code_page::encode(c, &Collation::DEFAULT)
}

/// Primary weight of the first character that has no code page 1252 byte. Adding the code
/// point to it keeps such a character after the whole of the weight tables, whose
/// primaries are `u16` and hence below this bound, and orders them among themselves by
/// code point.
const PRIMARY_ABOVE_TABLE: u32 = 0x1_0000;

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
mod tests;
