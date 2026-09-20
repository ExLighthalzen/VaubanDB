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
