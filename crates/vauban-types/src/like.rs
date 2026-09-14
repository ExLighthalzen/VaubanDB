//! `LIKE` and `PATINDEX` under a collation: one `impl Collation` block.
//!
//! The pattern is read once into a list of [`Token`]s, then matched against the characters
//! of the string by a single greedy pass with one backtracking point per `%`
//! ([`Collation::match_from`]): no recursion, no allocation per compared character, and a
//! cost that stays linear in the size of the string times the size of the pattern. Every
//! character comparison goes through [`Collation::compare_char`], so `LIKE` is case
//! insensitive and accent sensitive under `SQL_Latin1_General_CP1_CI_AS` exactly like `=`.
//!
//! The four wildcards, `ESCAPE`, the collation of the expression and the trailing blanks
//! of a pattern are the documented part; what the documentation leaves open is stated
//! where it is used and exercised by `tests/like.rs`.
//!
//! Out of scope, as the module README says: supplementary characters (surrogate pairs) and
//! `_SC` collations are not V1, so `_` matches one `char` — one Unicode scalar value —
//! which splits a surrogate pair the way a non-`_SC` collation does not.

use std::cmp::Ordering;
use std::iter::Peekable;

use crate::Collation;

/// `fBinary` of the 5-byte `Collation` rule (`[MS-TDS]` 2.2.5.1.2), the bit a `_BIN` name
/// sets. Declared here, next to its only use in this module, because the constants of
/// `collation.rs` are private to that file.
const FLAG_BINARY: u8 = 0x10;
/// `fBinary2`, the bit a `_BIN2` name sets.
const FLAG_BINARY2: u8 = 0x20;

impl Collation {
    /// The `LIKE` predicate: does `pattern` match the whole of `s` under this collation?
    ///
    /// The wildcards are `%` (zero or more characters), `_` (exactly one character),
    /// `[abc]` (one character of the set), `[a-c]` (one character of the range, bounds
    /// included, in the order of the collation) and `[^abc]` / `[^a-c]` (the negation of
    /// either). Any other character stands for itself and is compared with
    /// [`Collation::compare_char`]: `'ABC' LIKE 'a%'` is true under `_CI`, `'é' LIKE 'e'`
    /// is false under `_AS`.
    ///
    /// `escape` is the character of the `ESCAPE` clause. It makes the character that
    /// follows it literal — `%`, `_`, `[` or the escape character itself — and it does so
    /// **inside a `[...]` set as well**. An escaped character is a member of the set and
    /// nothing else: an escaped `]` does not close it, an escaped `^` right after the `[`
    /// does not negate it, and an escaped `-` does not make a range, but an escaped
    /// character may be the *bound* of a range. With `ESCAPE '!'` (each line is one
    /// `SELECT CASE WHEN v LIKE p ESCAPE '!' THEN 1 ELSE 0 END`; `tests/like_escape_corpus.rs`):
    ///
    /// | pattern | matches | does not match | why |
    /// |---|---|---|---|
    /// | `a[!]c` | nothing | `a!c`, `a]c` | the `]` is escaped, the `[` is never closed |
    /// | `a[!]]c` | `a]c` | `a!c` | the set is `{]}` |
    /// | `[^!]]` | `a`, `!` | `]` | negated `{]}`: the `^` still negates, the `]` is a member |
    /// | `[!^a]` | `^`, `a` | `b` | escaped, the `^` is a member and negates nothing |
    /// | `a[b!-d]c` | `abc`, `a-c` | `acc` | escaped, the `-` is a member: no range |
    /// | `[!a-c]`, `[a-!c]` | `a`, `b`, `c` | `-` | an escaped character is still a bound |
    /// | `[!!]` | `!` | `]` | the escape character escapes itself |
    /// | `[a!]` | nothing | `a`, `]`, `a]` | escaped `]`, so the set is never closed |
    ///
    /// The escape character is recognised **by identity**, not through the collation:
    /// `'a%' LIKE 'aX%' ESCAPE 'x'` is false and `'aX%' LIKE 'aX%' ESCAPE 'x'` is true,
    /// under `_CI_AS`, `Latin1_General_CS_AS` and `Latin1_General_BIN2` alike — had the
    /// `X` been read as the escape, the pattern would have been the literal `a%`, and the
    /// two answers would be the other way round. Under `_CI_AS`, `'ax' LIKE 'aX%' ESCAPE
    /// 'x'` is true as well, because the `X` is an ordinary character that `x` matches
    /// and the `%` is a wildcard; that one is **false** under `_CS_AS` and `_BIN2`, for
    /// the case of the `x` and not for the escape. And the escape character keeps its
    /// power when it is itself a metacharacter of the pattern: with `ESCAPE '^'`, `[^a]`
    /// is the set `{a}` and not a negation; with `ESCAPE '['`, `[[]` is the two literals
    /// `[` and `]`, so `'[]'` matches it and `'['` does not; with `ESCAPE '-'`, `[a-c]`
    /// is the set `{a, c}`.
    ///
    /// A pattern that **ends with the escape character** matches nothing at all — inside a
    /// set or outside it — and raises nothing: `'ab' LIKE 'ab!' ESCAPE '!'` and
    /// `'ab!' LIKE 'ab!' ESCAPE '!'` are both false (the second one is the vector that
    /// separates "matches nothing" from "the dangling escape is literal"). No error is
    /// raised there.
    ///
    /// **Trailing blanks.** The two sides are not treated alike, and not the way `=` treats
    /// them either. Every character of the pattern is significant, blanks included, and
    /// nothing pads the string to the length of the pattern: `'abc' LIKE 'abc '` is false
    /// (where `'abc' = 'abc '` is true), and so is `'abc ' LIKE 'abc  '`. But the blanks
    /// the pattern leaves over at the **end of the string** are forgiven: `'abc ' LIKE
    /// 'abc'` and `'abc  ' LIKE 'abc '` are true. They
    /// are forgiven, not trimmed: the space of `'abc '` is still a character a `_` can
    /// match, since `'abc ' LIKE 'abc_'` is true as well. Leading blanks are ordinary
    /// characters (`' abc' LIKE 'abc'` is false).
    ///
    /// `NULL` never reaches this function: three-valued logic belongs to the `binder` and
    /// the `executor`.
    ///
    /// Two edges the documentation does not cover:
    /// * a `[` that no `]` ever closes is read in one of two ways, and which one depends on
    ///   the collation ([`Collation::closes_class_at_pattern_end`]). Under the default
    ///   collation the pattern matches **nothing**, the `[` not being a literal bracket:
    ///   `'a[b' LIKE 'a[b'` is false, and so is `'ab' LIKE 'a[b'`. Under a Windows
    ///   collation that is not binary, the end of the pattern closes the set instead, so
    ///   `('ab' COLLATE Latin1_General_CI_AS) LIKE 'a[b'` is **true**. The documented way
    ///   to match a bracket under either rule is `[[]`;
    /// * the first unescaped `]` of a set closes it, so `[]]` is an empty set, which
    ///   matches no character, followed by a literal `]`, and `'a]c' LIKE 'a[]]c'` is
    ///   false.
    ///
    /// ```
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert!(latin1.like("abc", "a%", None));
    /// assert!(latin1.like("ABC", "a_c", None));
    /// assert!(!latin1.like("abc", "abc ", None));
    /// assert!(latin1.like("abc ", "abc", None));
    /// assert!(latin1.like("50%", "50!%", Some('!')));
    /// ```
    pub fn like(&self, s: &str, pattern: &str, escape: Option<char>) -> bool {
        self.match_start(s, pattern, escape).is_some()
    }

    /// The position `PATINDEX(pattern, s)` returns, **1-based**, or `None` where `PATINDEX`
    /// returns `0`. Same wildcards as [`Collation::like`], and no `ESCAPE` clause, which
    /// `PATINDEX` does not have.
    ///
    /// The position is that of the **first character the pattern matches** once its leading
    /// `%` has skipped as few characters as possible; it is `1` when the matched part is
    /// empty. A pattern that does **not** start with `%` is anchored at the beginning of
    /// the string, exactly like `LIKE`:
    ///
    /// | expression | result |
    /// |---|---|
    /// | `PATINDEX('%cd%', 'abcdef')` | `3` |
    /// | `PATINDEX('%CD%', 'abcdef')` | `3` (the collation ignores the case) |
    /// | `PATINDEX('%z%', 'abcdef')` | `0` |
    /// | `PATINDEX('cd', 'abcdef')` | `0` — anchored, and `'cd'` is not the whole string |
    /// | `PATINDEX('a%', 'abcdef')` | `1` |
    /// | `PATINDEX('', 'abc')` | `0` — an empty pattern matches an empty string only |
    /// | `PATINDEX('', '')` | `1` |
    /// | `PATINDEX('%', 'abc')` | `1` — the matched part is empty, at the beginning |
    /// | `PATINDEX('%%', 'abc')` | `1` |
    /// | `PATINDEX('abc', 'abc ')` | `1` — same trailing blanks as [`Collation::like`] |
    /// | `PATINDEX('%c', 'abc ')` | `3` |
    ///
    /// For a pattern surrounded by `%`, the answer agrees with [`Collation::like`]:
    /// `pattern_position(s, p).is_some() == like(s, p, None)`.
    ///
    /// ```
    /// use vauban_types::Collation;
    ///
    /// let latin1 = Collation::DEFAULT;
    /// assert_eq!(latin1.pattern_position("abcdef", "%cd%"), Some(3));
    /// assert_eq!(latin1.pattern_position("abcdef", "%z%"), None);
    /// assert_eq!(latin1.pattern_position("abcdef", "a%"), Some(1));
    /// ```
    pub fn pattern_position(&self, s: &str, pattern: &str) -> Option<usize> {
        self.match_start(s, pattern, None).map(|start| start + 1)
    }

    /// The 0-based index of the first character of `s` the pattern matches, or `None` when
    /// it does not match at all. The single engine behind [`Collation::like`] (which only
    /// looks at whether there is an answer) and [`Collation::pattern_position`] (which
    /// makes it 1-based).
    ///
    /// The leading `%` of the pattern is not a token but a permission to start further in
    /// the string; once it is set aside, the rest of the pattern must match the whole
    /// remainder, trailing blanks excepted. A pattern made of `%` only therefore matches at
    /// index `0` (the matched part is empty), and an empty pattern matches nothing but an
    /// empty — or all-blank — string.
    fn match_start(&self, s: &str, pattern: &str, escape: Option<char>) -> Option<usize> {
        // An unclosed `[` either closes at the end of the pattern or makes the pattern
        // match nothing, depending on the collation.
        let tokens = tokenize(pattern, escape, self.closes_class_at_pattern_end())?;
        let text: Vec<char> = s.chars().collect();
        let content = content_length(&text);
        let lead = tokens
            .iter()
            .take_while(|token| matches!(token, Token::Any))
            .count();
        let rest = &tokens[lead..];
        if rest.is_empty() {
            // Nothing but `%` (or nothing at all).
            return if lead > 0 || content == 0 {
                Some(0)
            } else {
                None
            };
        }
        self.match_from(&text, content, rest, lead == 0)
    }

    /// Matches `tokens` — which never begins with [`Token::Any`] — against the whole of
    /// `text`, and answers with the index at which the match starts. When `anchored` is
    /// false the match may start anywhere, as after a leading `%`, and the smallest such
    /// index wins. `content` is [`content_length`] of `text`: reaching it is enough, since
    /// the blanks the pattern leaves over at the end of the string are forgiven.
    ///
    /// One greedy pass with a single backtracking point, the last `%` met: on a mismatch,
    /// that `%` swallows one more character and matching resumes just after it. When there
    /// is no `%` to go back to, the implicit leading `%` slides by one character instead
    /// (this is what makes the answer the leftmost one). Two facts make this enough to
    /// never miss a match — the classic argument for wildcard matching:
    /// * making an earlier `%` swallow more can never rescue a match a later `%` failed to
    ///   complete, since that later `%` explored every remaining position;
    /// * the tokens between the implicit leading `%` and the first `%` of the pattern all
    ///   consume exactly one character, so a match starting further right implies a match
    ///   starting here.
    fn match_from(
        &self,
        text: &[char],
        content: usize,
        tokens: &[Token],
        anchored: bool,
    ) -> Option<usize> {
        // Characters skipped by the implicit leading `%`, i.e. the start of the match.
        let mut start = 0usize;
        let mut text_index = start;
        let mut token_index = 0usize;
        // The last `%` met: its index in `tokens` and how far it has swallowed the text.
        let mut star: Option<(usize, usize)> = None;
        loop {
            if let Some(token) = tokens.get(token_index) {
                if matches!(token, Token::Any) {
                    star = Some((token_index, text_index));
                    token_index += 1;
                    continue;
                }
                if text_index < text.len() && self.matches_one(token, text[text_index]) {
                    token_index += 1;
                    text_index += 1;
                    continue;
                }
            } else if text_index >= content {
                // The pattern is exhausted and only blanks are left over.
                return Some(start);
            }
            // Mismatch, or real text left over once the pattern is exhausted.
            if let Some((star_index, swallowed)) = star {
                if swallowed < text.len() {
                    star = Some((star_index, swallowed + 1));
                    token_index = star_index + 1;
                    text_index = swallowed + 1;
                    continue;
                }
                return None;
            }
            if !anchored && start < text.len() {
                start += 1;
                text_index = start;
                token_index = 0;
                continue;
            }
            return None;
        }
    }

    /// Does the end of the pattern **close** a `[...]` set this collation left open, or
    /// does an unclosed set make the whole pattern match nothing?
    ///
    /// `LIKE` has two grammars, and the collation of the matched expression picks one. The
    /// frontier is drawn by crossing four unclosed shapes, `'a' LIKE '[a'`, `'b' LIKE
    /// '[a-c'`, `'b' LIKE '[^a'` and `']' LIKE '[a!]b' ESCAPE '!'`, plus the closed control
    /// `'a' LIKE '[a]'`, with the 72 collation names of the two locales. The control
    /// answers `1` for the 72 names; the four unclosed shapes split them in two groups, and
    /// split them the same way (`tests/like.rs`):
    ///
    /// * **64 names answer `1` to the four shapes**: the `Latin1_General_*` and
    ///   `Latin1_General_100_*` names built from `CI`/`CS` and `AI`/`AS`, with or without
    ///   their `_KS`, `_WS`, `_SC` and `_UTF8` suffixes. These close the set at the end of
    ///   the pattern;
    /// * **8 names answer `0` to the four shapes**: the three `SQL_Latin1_General_CP1_*`
    ///   names, and the five binary ones — `Latin1_General_BIN`, `Latin1_General_BIN2`,
    ///   `Latin1_General_100_BIN`, `Latin1_General_100_BIN2`,
    ///   `Latin1_General_100_BIN2_UTF8`. For these, an unclosed set matches nothing.
    ///
    /// So the frontier is not "Windows against the rest": `Latin1_General_BIN` is a Windows
    /// head that follows the `SQL_*` rule, which is the vector that separates "is the head
    /// `SQL_`?" from "is the collation binary or `SQL_`?". In the five wire bytes that is
    /// `sort_id != 0` (the legacy family, [`Collation::parse`]) or a `fBinary` / `fBinary2`
    /// flag.
    ///
    /// What "closing at the end" means, under `Latin1_General_CI_AS` against the same
    /// shapes under `SQL_Latin1_General_CP1_CI_AS` and `Latin1_General_BIN2` (each pair of
    /// answers differs, so each line is a vector; asserted by `tests/like.rs`, in
    /// `windows_collation_closes_the_class_at_the_end_of_the_pattern`):
    ///
    /// | vector | Windows | `SQL_*`, `_BIN2` |
    /// |---|---|---|
    /// | `'a' LIKE '[a'` | 1 | 0 |
    /// | `'b' LIKE '[ab'` | 1 | 0 |
    /// | `'ab' LIKE '[ab'` | 0 | 0 — the set is still one character |
    /// | `'b' LIKE '[a-c'` | 1 | 0 |
    /// | `'-' LIKE '[a-'` | 1 | 0 — a trailing `-` is a member, not a range |
    /// | `'d' LIKE '[^a'` | 1 | 0 |
    /// | `'a' LIKE '[^'` | 1 | 0 — an empty negated set matches one character |
    /// | `'a' LIKE '['` | 0 | 0 — an empty set matches no character |
    /// | `'[' LIKE '[['` | 1 | 0 — the inner `[` is a member of the set |
    /// | `'ab' LIKE 'a[b'` | 1 | 0 |
    /// | `'xab' LIKE '%a[b'` | 1 | 0 |
    /// | `'ab' LIKE '[a][b'` | 1 | 0 — the first set closes, the second does not |
    /// | `'ab' LIKE '[a_'` | 0 | 0 — the `_` is a member, so the set is one character |
    /// | `'a%' LIKE '[a%'` | 0 | 0 — so is the `%` |
    /// | `']' LIKE '[a!]b' ESCAPE '!'` | 1 | 0 — the set is `{a, ], b}` |
    /// | `'c' LIKE '[a!]b' ESCAPE '!'` | 0 | 0 |
    /// | `'a' LIKE '[a!' ESCAPE '!'` | 0 | 0 — a pattern ending on the escape character |
    ///
    /// The last line is the one that orders the two rules: a pattern that ends on the
    /// escape character matches nothing under either grammar, so the dangling escape is
    /// read before the end of the pattern closes anything.
    fn closes_class_at_pattern_end(&self) -> bool {
        self.sort_id == 0 && self.flags & (FLAG_BINARY | FLAG_BINARY2) == 0
    }

    /// Does `token` — anything but [`Token::Any`], which the caller handles — match the
    /// single character `c`? Every comparison goes through [`Collation::compare_char`], so
    /// a set and a range obey the collation just like a literal does.
    fn matches_one(&self, token: &Token, c: char) -> bool {
        match token {
            // `%` is not matched one character at a time; `match_from` never comes here
            // with it, and "zero or more characters" does include this one anyway.
            Token::Any | Token::One => true,
            Token::Char(literal) => self.compare_char(c, *literal) == Ordering::Equal,
            Token::Class { negated, items } => {
                let inside = items.iter().any(|item| match item {
                    ClassItem::Char(literal) => self.compare_char(c, *literal) == Ordering::Equal,
                    ClassItem::Range(low, high) => {
                        self.compare_char(*low, c) != Ordering::Greater
                            && self.compare_char(c, *high) != Ordering::Greater
                    }
                });
                inside != *negated
            }
        }
    }
}

/// The length of `text` once its trailing spaces are set aside: the point from which the
/// pattern may stop matching. Computed once per call so that the end of the match stays a
/// comparison of two indexes and the whole matching stays linear.
fn content_length(text: &[char]) -> usize {
    let mut length = text.len();
    while length > 0 && text[length - 1] == ' ' {
        length -= 1;
    }
    length
}

/// One element of a `LIKE` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// `%`: zero or more characters.
    Any,
    /// `_`: exactly one character, whatever it is.
    One,
    /// A character that stands for itself, be it escaped or not.
    Char(char),
    /// `[...]`, or `[^...]` when `negated`: one character of the set.
    Class {
        /// The set opened with `^`, so it matches the characters it does *not* list.
        negated: bool,
        /// The contents of the set; an empty set matches no character at all.
        items: Vec<ClassItem>,
    },
}

/// One element of a `[...]` set: a character or a range.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    /// A single character of the set.
    Char(char),
    /// `a-c`: every character between the two bounds, both included, in the order of the
    /// collation.
    Range(char, char),
}

/// Reads a pattern into its tokens, or answers `None` for a pattern that matches against
/// nothing: one that ends with the escape character (`'ab!' LIKE 'ab!' ESCAPE '!'` is
/// false), and, when `close_at_end` is false, one whose `[` no `]` closes.
///
/// `close_at_end` is [`Collation::closes_class_at_pattern_end`], which the caller has
/// already read off the collation: with it, the end of the pattern closes a set the way a
/// `]` would.
///
/// `escape` makes the character that follows it literal, inside a `[...]` set as much as
/// outside it (see [`Collation::like`]). It is compared by identity, whatever the
/// collation, and it is looked at *before* the metacharacters: `ESCAPE '['` turns `[[]`
/// into two literals, `ESCAPE '^'` turns `[^a]` into the set `{a}`.
fn tokenize(pattern: &str, escape: Option<char>, close_at_end: bool) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            // A dangling escape character at the end of the pattern: nothing matches.
            let literal = chars.next()?;
            tokens.push(Token::Char(literal));
            continue;
        }
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '[' => parse_class(&mut chars, escape, close_at_end)?,
            _ => Token::Char(c),
        });
    }
    Some(tokens)
}

/// Reads a `[...]` set, the opening bracket already consumed, or answers `None` when the
/// pattern ends on the escape character, or when no `]` closes the set and `close_at_end`
/// says this collation does not close it at the end of the pattern.
///
/// `close_at_end` decides what the *end of the pattern* does, and nothing more: a pattern
/// that stops on the escape character matches nothing under either rule
/// (`'a' LIKE '[a!' ESCAPE '!'` is false under `Latin1_General_CI_AS` too).
///
/// An unescaped `^` right after the bracket negates the set; anywhere else it is an
/// ordinary character. The **first** unescaped `]` closes the set, so `[]]` is an empty set
/// followed by a literal `]`. An unescaped `-` is a range between two characters and
/// nothing else: first or last in the set, it stands for itself (`'a-c' LIKE 'a[-]c'` is
/// true). An escaped character is a plain member of the set — it neither closes, negates
/// nor separates a range — yet it may be the bound of one: `[!a-c]` and `[a-!c]` are both
/// the range `a` to `c` (with `ESCAPE '!'`).
fn parse_class<I: Iterator<Item = char>>(
    chars: &mut Peekable<I>,
    escape: Option<char>,
    close_at_end: bool,
) -> Option<Token> {
    // The escape character wins over the negation: with `ESCAPE '^'`, `[^a]` is `{a}`.
    let negated = chars.peek() == Some(&'^') && escape != Some('^');
    if negated {
        chars.next();
    }
    // Each member with whether it was escaped, which is what stops it from being a `-`
    // separator.
    let mut content: Vec<(char, bool)> = Vec::new();
    loop {
        // The pattern ends before the closing bracket: a Windows collation closes the set
        // here, the others make the whole pattern match nothing.
        let Some(c) = chars.next() else {
            if close_at_end {
                break;
            }
            return None;
        };
        if Some(c) == escape {
            // `None` here means the pattern ends on the escape character.
            content.push((chars.next()?, true));
            continue;
        }
        if c == ']' {
            break;
        }
        content.push((c, false));
    }
    let mut items = Vec::new();
    let mut i = 0;
    while i < content.len() {
        // A range needs a character on each side of an unescaped `-`.
        if content.len() >= i + 3 && content[i + 1] == ('-', false) {
            items.push(ClassItem::Range(content[i].0, content[i + 2].0));
            i += 3;
        } else {
            items.push(ClassItem::Char(content[i].0));
            i += 1;
        }
    }
    Some(Token::Class { negated, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pattern is read once, and read the way `LIKE` reads it.
    #[test]
    fn tokenize_reads_wildcards_sets_and_escapes() {
        assert_eq!(
            tokenize("a%_b", None, false),
            Some(vec![
                Token::Char('a'),
                Token::Any,
                Token::One,
                Token::Char('b'),
            ])
        );
        assert_eq!(
            tokenize("[^a-c]", None, false),
            Some(vec![Token::Class {
                negated: true,
                items: vec![ClassItem::Range('a', 'c')],
            }])
        );
        // A `-` first or last in the set is a character, not a range.
        assert_eq!(
            tokenize("[-a-]", None, false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![
                    ClassItem::Char('-'),
                    ClassItem::Char('a'),
                    ClassItem::Char('-'),
                ],
            }])
        );
        // The first `]` closes the set: `[]]` is an empty set then a literal `]`.
        assert_eq!(
            tokenize("[]]", None, false),
            Some(vec![
                Token::Class {
                    negated: false,
                    items: vec![],
                },
                Token::Char(']'),
            ])
        );
        // The escape character makes the next one literal, itself included.
        assert_eq!(
            tokenize("!%!!![", Some('!'), false),
            Some(vec![Token::Char('%'), Token::Char('!'), Token::Char('['),])
        );
        // Inside a set, the escape character acts too: an escaped `]` is a member and does
        // not close the set, an escaped `-` is a member and makes no range, an escaped
        // `^` negates nothing, and an escaped character may still bound a range.
        assert_eq!(
            tokenize("[!%]", Some('!'), false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Char('%')],
            }])
        );
        assert_eq!(
            tokenize("[a!]]", Some('!'), false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Char('a'), ClassItem::Char(']')],
            }])
        );
        assert_eq!(
            tokenize("[^!]]", Some('!'), false),
            Some(vec![Token::Class {
                negated: true,
                items: vec![ClassItem::Char(']')],
            }])
        );
        assert_eq!(
            tokenize("[!^a]", Some('!'), false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Char('^'), ClassItem::Char('a')],
            }])
        );
        assert_eq!(
            tokenize("[b!-d]", Some('!'), false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![
                    ClassItem::Char('b'),
                    ClassItem::Char('-'),
                    ClassItem::Char('d'),
                ],
            }])
        );
        // Deduced from `[!a-c]` and `[a-!c]`, which both match `a`, `b`, `c` and not `-`
        // (an escaped character is still a bound).
        assert_eq!(
            tokenize("[!a-!c]", Some('!'), false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Range('a', 'c')],
            }])
        );
        // The escape character is looked at before the metacharacters.
        assert_eq!(
            tokenize("[^a]", Some('^'), false),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Char('a')],
            }])
        );
        assert_eq!(
            tokenize("[[]", Some('['), false),
            Some(vec![Token::Char('['), Token::Char(']')])
        );
        // An unclosed `[` makes the whole pattern impossible, an escaped `]` included.
        assert_eq!(tokenize("a[b", None, false), None);
        assert_eq!(tokenize("a[!]c", Some('!'), false), None);
        // So does a pattern that ends on the escape character, in or out of a set.
        assert_eq!(tokenize("ab!", Some('!'), false), None);
        assert_eq!(tokenize("[a!", Some('!'), false), None);
        assert_eq!(tokenize("!", Some('!'), false), None);
    }

    /// The other grammar: with `close_at_end`, the end of the pattern closes the set the
    /// way a `]` would, and the same patterns that answered `None` above become tokens.
    /// Each assertion here answers `None` under the rule of the SQL collations — the
    /// `false` column of `tokenize_reads_wildcards_sets_and_escapes`, in this `like.rs` —
    /// which is what makes it a vector and not a restatement.
    #[test]
    fn tokenize_closes_the_class_at_the_end_of_the_pattern() {
        assert_eq!(
            tokenize("a[b", None, true),
            Some(vec![
                Token::Char('a'),
                Token::Class {
                    negated: false,
                    items: vec![ClassItem::Char('b')],
                },
            ])
        );
        // A range still needs a character on each side of its `-`; a trailing `-` is a
        // member (`'-' LIKE '[a-'` is true under `Latin1_General_CI_AS`).
        assert_eq!(
            tokenize("[a-c", None, true),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Range('a', 'c')],
            }])
        );
        assert_eq!(
            tokenize("[a-", None, true),
            Some(vec![Token::Class {
                negated: false,
                items: vec![ClassItem::Char('a'), ClassItem::Char('-')],
            }])
        );
        // The `^` still negates, and `[^` alone is an empty negated set, which matches
        // one character of any kind (`'a' LIKE '[^'` is true, `'' LIKE '[^'` is false).
        assert_eq!(
            tokenize("[^a", None, true),
            Some(vec![Token::Class {
                negated: true,
                items: vec![ClassItem::Char('a')],
            }])
        );
        assert_eq!(
            tokenize("[^", None, true),
            Some(vec![Token::Class {
                negated: true,
                items: vec![],
            }])
        );
        // `[` alone is an empty set, which matches no character.
        assert_eq!(
            tokenize("[", None, true),
            Some(vec![Token::Class {
                negated: false,
                items: vec![],
            }])
        );
        // Inside the set, `%`, `_` and `[` are members like any other character.
        assert_eq!(
            tokenize("[a_%[", None, true),
            Some(vec![Token::Class {
                negated: false,
                items: vec![
                    ClassItem::Char('a'),
                    ClassItem::Char('_'),
                    ClassItem::Char('%'),
                    ClassItem::Char('['),
                ],
            }])
        );
        // An escaped `]` is a member and does not close the set, which then runs to the
        // end of the pattern: `[a!]b` is `{a, ], b}`.
        assert_eq!(
            tokenize("[a!]b", Some('!'), true),
            Some(vec![Token::Class {
                negated: false,
                items: vec![
                    ClassItem::Char('a'),
                    ClassItem::Char(']'),
                    ClassItem::Char('b'),
                ],
            }])
        );
        // A pattern that ends on the escape character matches nothing under this rule too:
        // the dangling escape is read before the end of the pattern closes anything.
        assert_eq!(tokenize("[a!", Some('!'), true), None);
        assert_eq!(tokenize("ab!", Some('!'), true), None);
    }

    /// The five wire bytes decide which grammar applies, and `Latin1_General_BIN` is the
    /// name that stops it from being "Windows against the rest".
    #[test]
    fn collation_family_decides_which_grammar_applies() {
        let closes = |name: &str| {
            Collation::parse(name)
                .expect("a collation name")
                .closes_class_at_pattern_end()
        };
        assert!(closes("Latin1_General_CI_AS"));
        assert!(closes("Latin1_General_100_CS_AS_KS_WS_SC_UTF8"));
        assert!(!closes("Latin1_General_BIN"));
        assert!(!closes("Latin1_General_100_BIN2_UTF8"));
        assert!(!closes("SQL_Latin1_General_CP1_CI_AS"));
        assert!(!Collation::DEFAULT.closes_class_at_pattern_end());
    }

    /// The internal engine answers with the start of the match, which is what `PATINDEX`
    /// needs and what `LIKE` throws away.
    #[test]
    fn match_start_gives_the_leftmost_start() {
        let latin1 = Collation::DEFAULT;
        assert_eq!(latin1.match_start("abcdef", "%cd%", None), Some(2));
        assert_eq!(latin1.match_start("abcdef", "a%", None), Some(0));
        assert_eq!(latin1.match_start("abcdef", "cd", None), None);
        // Nothing but `%`: the matched part is empty, at the beginning of the string.
        assert_eq!(latin1.match_start("abc", "%", None), Some(0));
        assert_eq!(latin1.match_start("abc", "%%", None), Some(0));
        // The leading `%` skips as few characters as it can.
        assert_eq!(latin1.match_start("aaab", "%ab", None), Some(2));
    }

    /// Trailing spaces stop the match without being part of it.
    #[test]
    fn content_length_sets_trailing_spaces_aside() {
        assert_eq!(content_length(&['a', 'b', ' ', ' ']), 2);
        assert_eq!(content_length(&[' ', ' ']), 0);
        assert_eq!(content_length(&[]), 0);
        assert_eq!(content_length(&[' ', 'a']), 2);
        // Only the space is a blank here: a tab is an ordinary character.
        assert_eq!(content_length(&['a', '\t']), 2);
    }
}
