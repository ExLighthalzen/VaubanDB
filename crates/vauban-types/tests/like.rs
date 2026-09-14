//! `Collation::like` and `Collation::pattern_position`, and the escape character inside a
//! class.
//!
//! The escape vectors below are a selection of `like_escape_corpus.rs`; each one is quoted
//! with its answer.

use vauban_types::Collation;

/// `%` stands for zero or more characters, `_` for exactly one.
#[test]
fn like_percent_and_underscore() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("abc", "a%", None));
    assert!(latin1.like("abc", "%c", None));
    assert!(latin1.like("abc", "%b%", None));
    assert!(latin1.like("abc", "a_c", None));
    assert!(!latin1.like("abc", "a_", None));
    assert!(latin1.like("abc", "%", None));
    assert!(latin1.like("", "%", None));
    assert!(!latin1.like("", "_", None));
    // The empty pattern matches nothing but an empty string.
    assert!(latin1.like("", "", None));
    assert!(!latin1.like("a", "", None));
}

/// `LIKE` uses the collation of the expression: `_CI_AS` ignores the case and keeps the
/// accents.
#[test]
fn like_is_case_insensitive_accent_sensitive() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("ABC", "a%", None));
    assert!(!latin1.like("é", "e", None));
    assert!(latin1.like("É", "é", None));
}

/// Sets, ranges and negations, plus two bracket edges: a leading `-` is a character, and
/// the first `]` closes the set, so `[]]` is an *empty* set, which matches nothing,
/// followed by a literal `]`.
#[test]
fn like_character_classes() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("abc", "[a-c]bc", None));
    assert!(!latin1.like("dbc", "[a-c]bc", None));
    assert!(latin1.like("dbc", "[^a-c]bc", None));
    assert!(latin1.like("abc", "[abc]bc", None));
    assert!(latin1.like("a-c", "a[-]c", None));
    // `SELECT CASE WHEN 'a]c' LIKE 'a[]]c' THEN 1 ELSE 0 END` gives 0.
    assert!(!latin1.like("a]c", "a[]]c", None));
    // The bounds of a range obey the collation, like every other comparison.
    assert!(latin1.like("B", "[a-c]", None));
    assert!(!latin1.like("é", "[a-c]", None));
}

/// A `[` that no `]` closes makes the pattern match nothing — not even a literal bracket;
/// `[[]` is the way to match one.
#[test]
fn like_unclosed_bracket_matches_nothing() {
    let latin1 = Collation::DEFAULT;
    assert!(!latin1.like("a[b", "a[b", None));
    assert!(!latin1.like("ab", "a[b", None));
    assert!(latin1.like("a[b", "a[[]b", None));
}

/// `ESCAPE c` makes the character after `c` literal, `c` itself included.
#[test]
fn like_escape() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("50%", "50!%", Some('!')));
    assert!(!latin1.like("50x", "50!%", Some('!')));
    assert!(latin1.like("a_b", "a!_b", Some('!')));
    assert!(!latin1.like("axb", "a!_b", Some('!')));
    assert!(latin1.like("a!b", "a!!b", Some('!')));
    assert!(latin1.like("a[b", "a![b", Some('!')));
}

/// The escape character acts **inside** a class too. `'a!c' LIKE 'a[!]c' ESCAPE '!'` is
/// 0, the `]` being escaped, so the class is never closed and the pattern matches
/// nothing, and `'a]c' LIKE 'a[!]]c' ESCAPE '!'` is 1, the class being `{]}`. Then the
/// vector that separates the two rules: `'acc' LIKE 'a[b!-d]c' ESCAPE '!'` is 0, because
/// the escaped `-` is a member and not a range (`'a-c'` matches, 1). `a[!%]c` and
/// `a[!!]c` answer 1 under both rules and prove nothing; they are kept as plain facts.
#[test]
fn like_escape_acts_inside_a_class() {
    let latin1 = Collation::DEFAULT;
    assert!(!latin1.like("a!c", "a[!]c", Some('!')));
    assert!(!latin1.like("a]c", "a[!]c", Some('!')));
    assert!(latin1.like("a]c", "a[!]]c", Some('!')));
    assert!(!latin1.like("a!c", "a[!]]c", Some('!')));
    assert!(!latin1.like("acc", "a[b!-d]c", Some('!')));
    assert!(latin1.like("a-c", "a[b!-d]c", Some('!')));
    assert!(latin1.like("abc", "a[b!-d]c", Some('!')));
    assert!(latin1.like("a%c", "a[!%]c", Some('!')));
    assert!(latin1.like("a!c", "a[!!]c", Some('!')));
    // `[!!]` with `'!'` -> 1 and with `']'` -> 0: the escape escapes itself.
    assert!(latin1.like("!", "[!!]", Some('!')));
    assert!(!latin1.like("]", "[!!]", Some('!')));
}

/// What an escaped character is and is not, inside a class (each line with `ESCAPE '!'`):
/// an escaped `]` is a member, an escaped `^` right after the `[`
/// negates nothing while an unescaped one still does, an escaped `-` separates no range,
/// and an escaped character remains a valid **bound** of a range.
#[test]
fn like_escaped_character_is_a_member_and_nothing_else() {
    let latin1 = Collation::DEFAULT;
    // `[^!]]` -> `a` 1, `!` 1, `]` 0: negated `{]}`.
    assert!(latin1.like("a", "[^!]]", Some('!')));
    assert!(latin1.like("!", "[^!]]", Some('!')));
    assert!(!latin1.like("]", "[^!]]", Some('!')));
    // `[!^a]` -> `^` 1, `a` 1, `b` 0: the set `{^, a}`, not a negation.
    assert!(latin1.like("^", "[!^a]", Some('!')));
    assert!(latin1.like("a", "[!^a]", Some('!')));
    assert!(!latin1.like("b", "[!^a]", Some('!')));
    // `[!a-c]` and `[a-!c]` -> `b` 1, `-` 0: the range `a`..`c`, escaped bound or not.
    assert!(latin1.like("b", "[!a-c]", Some('!')));
    assert!(!latin1.like("-", "[!a-c]", Some('!')));
    assert!(latin1.like("b", "[a-!c]", Some('!')));
    assert!(!latin1.like("-", "[a-!c]", Some('!')));
    // `[a!]]` -> `a` 1, `]` 1, `a]` 0: the set `{a, ]}`, closed by the second `]`.
    assert!(latin1.like("a", "[a!]]", Some('!')));
    assert!(latin1.like("]", "[a!]]", Some('!')));
    assert!(!latin1.like("a]", "[a!]]", Some('!')));
    // `[a!]` -> `a` 0, `]` 0, `a]` 0: escaped `]`, never closed, nothing matches.
    assert!(!latin1.like("a", "[a!]", Some('!')));
    assert!(!latin1.like("]", "[a!]", Some('!')));
    assert!(!latin1.like("a]", "[a!]", Some('!')));
}

/// A pattern that ends with the escape character matches nothing, and raises nothing:
/// `'ab' LIKE 'ab!' ESCAPE '!'` is 0, and so is `'ab!' LIKE 'ab!' ESCAPE '!'`, the vector
/// that separates "nothing" from "the dangling escape is literal". Inside a class as
/// well: `'a' LIKE '[a!' ESCAPE '!'` is 0.
#[test]
fn like_pattern_ending_with_the_escape_matches_nothing() {
    let latin1 = Collation::DEFAULT;
    assert!(!latin1.like("ab", "ab!", Some('!')));
    assert!(!latin1.like("ab!", "ab!", Some('!')));
    assert!(!latin1.like("!", "!", Some('!')));
    assert!(!latin1.like("", "!", Some('!')));
    assert!(!latin1.like("a", "[a!", Some('!')));
    assert!(!latin1.like("a!", "[a!", Some('!')));
    assert!(!latin1.like("a]", "[a]!", Some('!')));
    // Without the clause, the same `!` is an ordinary character (the control).
    assert!(latin1.like("ab!", "ab!", None));
}

/// The escape character is looked at before the metacharacters, so a metacharacter
/// chosen as the escape loses its meaning (with each `ESCAPE`):
/// `ESCAPE '^'` makes `[^a]` the set `{a}` (`'a'` 1, `'b'` 0); `ESCAPE '['` makes `[[]`
/// two literals (`'[]'` 1, `'['` 0); `ESCAPE '-'` makes `[a-c]` the set `{a, c}` (`'b'`
/// 0, `'c'` 1) and `[a--c]` the set `{a, -, c}`; `ESCAPE ']'` makes `[a]]` an unclosed
/// class (`'a'` 0, `']'` 0); `ESCAPE '%'` makes `%%` a literal (`'%'` 1, `'a'` 0).
#[test]
fn like_escape_beats_the_metacharacters() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("a", "[^a]", Some('^')));
    assert!(!latin1.like("b", "[^a]", Some('^')));
    assert!(latin1.like("[]", "[[]", Some('[')));
    assert!(!latin1.like("[", "[[]", Some('[')));
    assert!(!latin1.like("b", "[a-c]", Some('-')));
    assert!(latin1.like("c", "[a-c]", Some('-')));
    assert!(latin1.like("-", "[a--c]", Some('-')));
    assert!(!latin1.like("b", "[a--c]", Some('-')));
    assert!(!latin1.like("a", "[a]]", Some(']')));
    assert!(!latin1.like("]", "[a]]", Some(']')));
    assert!(latin1.like("%", "%%", Some('%')));
    assert!(!latin1.like("a", "%%", Some('%')));
}

/// The escape character is recognised by identity, not through the collation:
/// under `_CI_AS`, `'ax' LIKE 'aX%' ESCAPE 'x'` is 1 — the `X` is an ordinary character
/// the `x` of the value matches, and the `%` stays a wildcard; had the `X` been the
/// escape, the pattern would have been the literal `a%`. Likewise `'x' LIKE '[Xb]'
/// ESCAPE 'x'` is 1 (the set `{X, b}`) where `'x' LIKE '[xb]' ESCAPE 'x'` is 0 (the set
/// `{b}`). The eight forms below are asserted under `_CI_AS`; a case-sensitive collation
/// would turn `'ax' LIKE 'aX%'` and `'x' LIKE '[Xb]'` (both with `ESCAPE 'x'`) into 0
/// because it does not match an `x` to an `X`, not because the escape is read
/// differently: `'a%' LIKE 'aX%'` is 0 and `'aX%' LIKE 'aX%'` is 1 regardless of the
/// collation, which the literal `a%` would have answered the other way round.
#[test]
fn like_escape_is_recognised_by_identity() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("ax", "aX%", Some('x')));
    assert!(latin1.like("aX%", "aX%", Some('x')));
    assert!(!latin1.like("a%", "aX%", Some('x')));
    assert!(latin1.like("a%", "ax%", Some('x')));
    assert!(!latin1.like("ax", "ax%", Some('x')));
    assert!(latin1.like("x", "[Xb]", Some('x')));
    assert!(!latin1.like("x", "[xb]", Some('x')));
    assert!(latin1.like("b", "[xb]", Some('x')));
}

/// The degenerate classes, with an `ESCAPE` clause that does not appear in them: the
/// clause changes nothing (same answers with and without it). `[]` is an empty
/// set, `[^]` its negation (everything but the empty string), `[a-]` and `[-a]` are the
/// set `{a, -}`, `[]]` is an empty set then a literal `]`, `[^]]` is everything then `]`,
/// `[a` and `[^` match nothing.
#[test]
fn like_degenerate_classes_ignore_an_unused_escape() {
    let latin1 = Collation::DEFAULT;
    for escape in [None, Some('!')] {
        assert!(!latin1.like("a", "[]", escape));
        assert!(!latin1.like("", "[]", escape));
        assert!(latin1.like("a", "[^]", escape));
        assert!(latin1.like("]", "[^]", escape));
        assert!(!latin1.like("", "[^]", escape));
        assert!(latin1.like("a", "[a-]", escape));
        assert!(latin1.like("-", "[a-]", escape));
        assert!(!latin1.like("b", "[a-]", escape));
        assert!(latin1.like("a", "[-a]", escape));
        assert!(latin1.like("-", "[-a]", escape));
        assert!(!latin1.like("]", "[]]", escape));
        assert!(!latin1.like("a]", "[]]", escape));
        assert!(latin1.like("a]", "[^]]", escape));
        assert!(!latin1.like("]", "[^]]", escape));
        assert!(!latin1.like("a", "[a", escape));
        assert!(!latin1.like("[a", "[a", escape));
        assert!(!latin1.like("^", "[^", escape));
    }
}

/// The difference with `=` everybody meets in production: a trailing space of the
/// **pattern** is a character to match, where `=` treats it as padding.
///
/// The value side is a third rule: `'abc ' LIKE 'abc'` is **true**. The blanks the
/// pattern leaves over at the end of the value are forgiven, but they are not trimmed
/// away beforehand, since `'abc ' LIKE 'abc_'` is true too, and nothing pads the value
/// either, since `'abc ' LIKE 'abc _'` is false.
#[test]
fn like_trailing_spaces_are_significant() {
    let latin1 = Collation::DEFAULT;
    // Pattern side: every character of the pattern must be matched.
    assert!(!latin1.like("abc", "abc ", None));
    assert!(!latin1.like("abc", "abc  ", None));
    assert!(!latin1.like("abc ", "abc  ", None));
    assert!(!latin1.like("abc ", "abc _", None));
    // Value side: leftover blanks are forgiven, not trimmed, and not padded.
    assert!(latin1.like("abc ", "abc", None));
    assert!(latin1.like("abc  ", "abc", None));
    assert!(latin1.like("abc  ", "abc ", None));
    assert!(latin1.like("abc ", "abc ", None));
    assert!(latin1.like("abc ", "abc_", None));
    assert!(latin1.like("abc ", "abc[ ]", None));
    assert!(latin1.like("abc ", "ab_", None));
    assert!(latin1.like("abc ", "%c", None));
    assert!(latin1.like("abc ", "abc%", None));
    // A leading space is an ordinary character.
    assert!(!latin1.like(" abc", "abc", None));
    // The same two strings, compared: `=` blank-pads the shorter operand and calls them
    // equal, on both sides, which is why `'abc' LIKE 'abc '` above is false and
    // `'abc' = 'abc '` is true.
    assert_eq!(latin1.compare("abc", "abc "), std::cmp::Ordering::Equal);
}

/// The position `PATINDEX` returns, 1-based.
#[test]
fn pattern_position_is_one_based() {
    let latin1 = Collation::DEFAULT;
    assert_eq!(latin1.pattern_position("abcdef", "%cd%"), Some(3));
    assert_eq!(latin1.pattern_position("ABCDEF", "%cd%"), Some(3));
    assert_eq!(latin1.pattern_position("abcdef", "%z%"), None);
    assert_eq!(latin1.pattern_position("abcdef", "a%"), Some(1));
    // `PATINDEX('cd', 'abcdef')` is 0: a pattern with no leading `%` is
    // anchored, exactly like `LIKE`.
    assert_eq!(latin1.pattern_position("abcdef", "cd"), None);
    // `PATINDEX('', 'abc')` is 0, `PATINDEX('', '')` is 1, `PATINDEX('%', 'abc')`
    // and `PATINDEX('%%', 'abc')` are 1.
    assert_eq!(latin1.pattern_position("abc", ""), None);
    assert_eq!(latin1.pattern_position("", ""), Some(1));
    assert_eq!(latin1.pattern_position("abc", "%"), Some(1));
    assert_eq!(latin1.pattern_position("abc", "%%"), Some(1));
    assert_eq!(latin1.pattern_position("", "%"), Some(1));
    // `PATINDEX('abc', 'abc ')` is 1 and `PATINDEX('%c', 'abc ')` is 3: the
    // trailing blanks of the value are forgiven here exactly as they are in `like`.
    assert_eq!(latin1.pattern_position("abc ", "abc"), Some(1));
    assert_eq!(latin1.pattern_position("abc ", "%c"), Some(3));
}

/// The two functions share one engine: for a pattern surrounded by `%`, one answers where
/// the other answers whether.
#[test]
fn pattern_position_agrees_with_like() {
    let latin1 = Collation::DEFAULT;
    for pattern in ["%cd%", "%z%", "%a%", "%_%", "%[a-c]%", "%%", "%a%f%"] {
        for s in ["abcdef", "", "z", "ABCDEF", "a"] {
            assert_eq!(
                latin1.pattern_position(s, pattern).is_some(),
                latin1.like(s, pattern, None),
                "pattern {pattern:?} on {s:?}"
            );
        }
    }
}

/// A pattern full of `%` on a long string must not blow up: the matching is iterative and
/// backtracks at most once per `%`, so this answers instantly.
#[test]
fn like_backtracking_terminates() {
    let latin1 = Collation::DEFAULT;
    let s = "a".repeat(64) + "b";
    let started = std::time::Instant::now();
    assert!(!latin1.like(&s, "%a%a%a%a%a%c", None));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    // The same pattern that does match, on the same string.
    assert!(latin1.like(&s, "%a%a%a%a%a%b", None));
}

/// The 72 collation names crossed with four unclosed shapes and their closed control:
/// `SELECT CASE WHEN <value> COLLATE <name> LIKE <pattern> THEN 1 ELSE 0 END`, 360
/// vectors.
///
/// The split is 64 names against 8, and it is the same split for the four
/// shapes: the `Latin1_General_*` and `Latin1_General_100_*` names built from `CI`/`CS`
/// and `AI`/`AS` answer 1, the three `SQL_Latin1_General_CP1_*` names and the five binary
/// ones answer 0. The control answers 1 for the 72. `Latin1_General_BIN` and
/// `Latin1_General_100_BIN2_UTF8` are in the crossing, which is what tells the rule apart
/// from "the head is `SQL_`" and from "the head is `Latin1_General`".
#[test]
fn like_unclosed_class_splits_the_seventy_two_collation_names() {
    let names = collation_names();
    assert_eq!(names.len(), 72);

    // The eight names that answer 0 on the four shapes.
    let matches_nothing = [
        "SQL_Latin1_General_CP1_CI_AI",
        "SQL_Latin1_General_CP1_CI_AS",
        "SQL_Latin1_General_CP1_CS_AS",
        "Latin1_General_BIN",
        "Latin1_General_BIN2",
        "Latin1_General_100_BIN",
        "Latin1_General_100_BIN2",
        "Latin1_General_100_BIN2_UTF8",
    ];
    assert_eq!(matches_nothing.len(), 8);

    let shapes: [(&str, &str, Option<char>); 4] = [
        ("a", "[a", None),
        ("b", "[a-c", None),
        ("b", "[^a", None),
        ("]", "[a!]b", Some('!')),
    ];
    let mut closing = 0;
    for name in &names {
        let collation = Collation::parse(name).expect("a collation name");
        let closes = !matches_nothing.contains(&name.as_str());
        closing += usize::from(closes);
        for (value, pattern, escape) in shapes {
            assert_eq!(
                collation.like(value, pattern, escape),
                closes,
                "{name}: '{value}' LIKE '{pattern}'"
            );
        }
        // The closed control, 1 for the 72 names.
        assert!(collation.like("a", "[a]", None), "{name}: control");
    }
    assert_eq!(closing, 64);
}

/// What "the end of the pattern closes the set" means, under `Latin1_General_CI_AS` and
/// against the two collations that read the same patterns the other way. Each line of the
/// first loop is a vector — its answer differs between the two rules — and each line of
/// the second says which reading of the pattern it rules out.
#[test]
fn windows_collation_closes_the_class_at_the_end_of_the_pattern() {
    let windows = Collation::parse("Latin1_General_CI_AS").expect("installed collation");
    let sql = Collation::DEFAULT;
    let bin2 = Collation::parse("Latin1_General_BIN2").expect("installed collation");

    // 1 under `Latin1_General_CI_AS`, 0 under the other two.
    let windows_only: [(&str, &str, Option<char>); 10] = [
        ("a", "[a", None),
        ("b", "[ab", None),
        ("b", "[a-c", None),
        // A trailing `-` is a member of the set, not the start of a range.
        ("-", "[a-", None),
        ("d", "[^a", None),
        // An empty negated set matches one character, of any kind.
        ("a", "[^", None),
        // The inner `[` is an ordinary member.
        ("[", "[[", None),
        ("ab", "a[b", None),
        ("xab", "%a[b", None),
        // The first set closes on its `]`, the second one on the end of the pattern.
        ("ab", "[a][b", None),
    ];
    for (value, pattern, escape) in windows_only {
        assert!(
            windows.like(value, pattern, escape),
            "windows: '{value}' LIKE '{pattern}'"
        );
        assert!(!sql.like(value, pattern, escape), "sql: '{pattern}'");
        assert!(!bin2.like(value, pattern, escape), "bin2: '{pattern}'");
    }

    // 0 under the three, each for a reason of its own.
    let nobody: [(&str, &str, Option<char>, &str); 7] = [
        // The set stands for one character, so a two-character value cannot match.
        ("ab", "[ab", None, "the set is one character"),
        ("d", "[a-c", None, "outside the range"),
        ("a", "[^a", None, "inside the negated set"),
        ("", "[^", None, "no character to match"),
        ("a", "[", None, "an empty set matches no character"),
        // `_` and `%` are members of the set, not wildcards, so the set is one character.
        ("ab", "[a_", None, "the `_` is a member"),
        ("a%", "[a%", None, "the `%` is a member"),
    ];
    for (value, pattern, escape, why) in nobody {
        assert!(
            !windows.like(value, pattern, escape),
            "windows: '{value}' LIKE '{pattern}' — {why}"
        );
        assert!(!sql.like(value, pattern, escape), "sql: '{pattern}'");
        assert!(!bin2.like(value, pattern, escape), "bin2: '{pattern}'");
    }

    // The escaped `]` does not close the set, so `[a!]b` is the set `{a, ], b}` that the
    // end of the pattern closes: three members match under the Windows rule, `c` does not.
    for value in ["a", "]", "b"] {
        assert!(windows.like(value, "[a!]b", Some('!')));
        assert!(!sql.like(value, "[a!]b", Some('!')));
    }
    assert!(!windows.like("c", "[a!]b", Some('!')));

    // A pattern that ends on the escape character matches nothing under either rule: the
    // dangling escape is read before the end of the pattern closes anything.
    assert!(!windows.like("a", "[a!", Some('!')));
    assert!(!windows.like("ab", "ab!", Some('!')));

    // `PATINDEX` shares the tokenizer, so it follows the collation the same way.
    assert_eq!(windows.pattern_position("xya", "%[a"), Some(3));
    assert_eq!(sql.pattern_position("xya", "%[a"), None);
    assert_eq!(windows.pattern_position("a", "[a"), Some(1));
    assert_eq!(sql.pattern_position("a", "[a"), None);
    assert_eq!(windows.pattern_position("a", "[a]"), Some(1));
    assert_eq!(sql.pattern_position("a", "[a]"), Some(1));
}

/// The 72 collation names `Collation::parse` accepts, spelled the way `fn_helpcollations()`
/// spells them: three legacy names, 18 `Latin1_General_*` and 51
/// `Latin1_General_100_*`.
fn collation_names() -> Vec<String> {
    let mut names: Vec<String> = ["CI_AI", "CI_AS", "CS_AS"]
        .iter()
        .map(|suffix| format!("SQL_Latin1_General_CP1_{suffix}"))
        .collect();
    let mut sensitivity = Vec::new();
    for case in ["CI", "CS"] {
        for accent in ["AI", "AS"] {
            for kana in ["", "_KS"] {
                for width in ["", "_WS"] {
                    sensitivity.push(format!("{case}_{accent}{kana}{width}"));
                }
            }
        }
    }
    assert_eq!(sensitivity.len(), 16);
    names.extend(
        sensitivity
            .iter()
            .map(|suffix| format!("Latin1_General_{suffix}")),
    );
    names.push("Latin1_General_BIN".to_string());
    names.push("Latin1_General_BIN2".to_string());
    for suffix in &sensitivity {
        for tail in ["", "_SC", "_SC_UTF8"] {
            names.push(format!("Latin1_General_100_{suffix}{tail}"));
        }
    }
    names.push("Latin1_General_100_BIN".to_string());
    names.push("Latin1_General_100_BIN2".to_string());
    names.push("Latin1_General_100_BIN2_UTF8".to_string());
    names
}
