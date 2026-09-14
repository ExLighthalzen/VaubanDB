//! The documented class forms of `LIKE`: `[ ]`, a set of characters to match, and `[^]`,
//! a set of characters not to match. The documented examples read tables; they are
//! rewritten here as literals.

use vauban_types::Collation;

/// A range inside a class, `m[n-z]%` over database names.
#[test]
fn range_m_n_to_z() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("model", "m[n-z]%", None));
    assert!(latin1.like("msdb", "m[n-z]%", None));
    assert!(!latin1.like("master", "m[n-z]%", None));
}

/// Four digit classes in a row: a shorter subject and a fourth character outside `[0-9]`
/// both come back 0.
#[test]
fn four_digit_class() {
    let latin1 = Collation::DEFAULT;
    const PATTERN: &str = "[0-9][0-9][0-9][0-9]";
    assert!(latin1.like("3000", PATTERN, None));
    assert!(!latin1.like("300", PATTERN, None));
    assert!(!latin1.like("300a", PATTERN, None));
}

/// A class that mixes a range and single characters, `_` being one of them. The third
/// vector is the one that separates the two readings: were `_` still the
/// single-character wildcard inside the class, `'axyz'` would come back 1 as well.
#[test]
fn mixed_set_underscore_is_a_member() {
    let latin1 = Collation::DEFAULT;
    const PATTERN: &str = "[0-9!@#$.,;_]%";
    assert!(latin1.like("2002", PATTERN, None));
    assert!(latin1.like("_xyz", PATTERN, None));
    assert!(!latin1.like("axyz", PATTERN, None));
}

/// `[^a]` rules out one character in one position, on `'Alex'` and `'Alan'` under the
/// default `_CI_AS` collation.
#[test]
fn exclude_third_letter() {
    let latin1 = Collation::DEFAULT;
    assert!(latin1.like("Alex", "Al[^a]%", None));
    assert!(!latin1.like("Alan", "Al[^a]%", None));
}

/// Two ranges under one negation, `A-z` being a range under the collation: `1, 0, 0` for
/// `'_xyz'`, `'Axyz'` and `'9xyz'`.
#[test]
fn negated_combined_ranges() {
    let latin1 = Collation::DEFAULT;
    const PATTERN: &str = "[^0-9A-z]%";
    assert!(latin1.like("_xyz", PATTERN, None));
    assert!(!latin1.like("Axyz", PATTERN, None));
    assert!(!latin1.like("9xyz", PATTERN, None));
}

/// `]` right after the opening bracket, followed by another character: `0, 0, 0`. This is
/// a different pattern from `[]]`.
#[test]
fn closing_bracket_in_a_class() {
    let latin1 = Collation::DEFAULT;
    assert!(!latin1.like("]", "[]a]", None));
    assert!(!latin1.like("a", "[]a]", None));
    assert!(!latin1.like("b", "[]a]", None));
}
