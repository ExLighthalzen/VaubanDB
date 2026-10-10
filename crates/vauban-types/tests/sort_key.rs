//! Integration tests of `vauban_types::sort_key`, `hash_key` and `Collation::sort_key`:
//! the key of two values orders them as `compare` does, so equal values share a key and
//! a hash, under each family of collation rules.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use vauban_types::{
    Collation, Date, DateTime, DateTime2, DateTimeOffset, Decimal, SqlString, Time, Value, compare,
    hash_key, sort_key,
};

/// The collations the property runs under: each family of rules, crossed with the
/// sensitivity flags it accepts.
const COLLATIONS: [&str; 26] = [
    "SQL_Latin1_General_CP1_CI_AS",
    "SQL_Latin1_General_CP1_CS_AS",
    "SQL_Latin1_General_CP1_CI_AI",
    "Latin1_General_CI_AS",
    "Latin1_General_CS_AS",
    "Latin1_General_CI_AI",
    "Latin1_General_CS_AI",
    "Latin1_General_CI_AS_KS",
    "Latin1_General_CI_AS_WS",
    "Latin1_General_CI_AI_KS_WS",
    "Latin1_General_CS_AS_KS_WS",
    "Latin1_General_BIN",
    "Latin1_General_BIN2",
    "Latin1_General_100_CI_AS",
    "Latin1_General_100_CS_AS",
    "Latin1_General_100_CI_AI",
    "Latin1_General_100_CS_AI",
    "Latin1_General_100_CI_AS_KS",
    "Latin1_General_100_CI_AS_WS",
    "Latin1_General_100_CI_AI_WS",
    "Latin1_General_100_CS_AS_KS_WS",
    "Latin1_General_100_CI_AS_SC",
    "Latin1_General_100_CI_AS_SC_UTF8",
    "Latin1_General_100_BIN",
    "Latin1_General_100_BIN2",
    "Latin1_General_100_BIN2_UTF8",
];

/// Letters, digits, spaces, the characters the Windows rules weigh last or skip (hyphen,
/// apostrophe, dashes, soft hyphen, controls, `NUL`), the letters that expand (`æ`, `œ`,
/// `ß`, `þ`), accented and cased letters of code page 1252, symbols, a character outside
/// it, full-width forms and kana.
const ALPHABET: &[char] = &[
    'a', 'b', 'c', 'A', 'B', 'C', 'e', 'E', 'h', 'H', 'o', 'O', 's', 'S', 't', 'T', 'x', 'y', 'Y',
    'z', 'Z', '0', '1', '9', ' ', ' ', '-', '\'', '\t', '\u{01}', '\u{1F}', '\u{7F}', '\0', 'é',
    'É', 'è', 'à', 'À', 'ä', 'æ', 'Æ', 'œ', 'Œ', 'ß', 'Ð', 'ð', 'Þ', 'þ', 'ÿ', 'Ÿ', 'š', 'Š', 'Ç',
    'ç', '\u{AD}', '–', '—', '€', 'ƒ', '¹', 'ª', 'º', '½', 'Ā', 'Ƒ', 'ａ', 'Ａ', '０', '－', '＇',
    'ア', 'あ', 'カ', 'か',
];

/// Replacements that keep a string equal, or nearly so, under some collation: an
/// expansion against the letters it stands for, an accent against its base letter, a
/// full-width form against its ASCII character, a katakana against its hiragana.
const NEIGHBOURS: [(&str, &str); 24] = [
    ("æ", "ae"),
    ("Æ", "AE"),
    ("œ", "oe"),
    ("Œ", "OE"),
    ("ß", "ss"),
    ("þ", "th"),
    ("Þ", "TH"),
    ("é", "e"),
    ("É", "E"),
    ("è", "e"),
    ("à", "a"),
    ("À", "A"),
    ("ä", "a"),
    ("ÿ", "y"),
    ("Ÿ", "Y"),
    ("š", "s"),
    ("Ç", "C"),
    ("ç", "c"),
    ("ａ", "a"),
    ("Ａ", "A"),
    ("０", "0"),
    ("－", "-"),
    ("＇", "'"),
    ("ア", "あ"),
];

/// A deterministic generator (xorshift64*), so that a failure replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn string(&mut self, max: usize, ascii: bool) -> String {
        let len = self.below(max + 1);
        (0..len)
            .map(|_| {
                if ascii {
                    // The ASCII part of the alphabet: its first 33 characters.
                    ALPHABET[self.below(33)]
                } else {
                    ALPHABET[self.below(ALPHABET.len())]
                }
            })
            .collect()
    }
}

/// A string close to `a`: its case swapped, trailing spaces added, a character weighed
/// last inserted or dropped, an expansion or an accent or a width or a kana swapped, or a
/// prefix.
fn neighbour(rng: &mut Rng, a: &str) -> String {
    match rng.below(7) {
        0 => a
            .chars()
            .map(|c| {
                let swapped: Vec<char> = if c.is_uppercase() {
                    c.to_lowercase().collect()
                } else {
                    c.to_uppercase().collect()
                };
                match (swapped.first().copied(), swapped.get(1)) {
                    (Some(s), None) if rng.chance(50) => s,
                    _ => c,
                }
            })
            .collect(),
        1 => format!("{a}{}", " ".repeat(1 + rng.below(3))),
        2 => {
            let inserted = ['-', '\'', '\0', '\u{AD}', '\u{01}', '－'][rng.below(6)];
            let at = rng.below(a.chars().count() + 1);
            let mut out: String = a.chars().take(at).collect();
            out.push(inserted);
            out.extend(a.chars().skip(at));
            out
        }
        3 => a
            .chars()
            .filter(|c| !matches!(c, '-' | '\'' | '\0'))
            .collect(),
        4 | 5 => {
            let mut out = a.to_owned();
            for _ in 0..3 {
                let (x, y) = NEIGHBOURS[rng.below(NEIGHBOURS.len())];
                out = if rng.chance(50) {
                    out.replace(x, y)
                } else {
                    out.replace(y, x)
                };
            }
            out
        }
        _ => a.chars().take(a.chars().count() / 2).collect(),
    }
}

fn text(t: &str) -> Value {
    Value::String(SqlString { text: t.to_owned() })
}

fn key(v: &Value, collation: &Collation) -> Vec<u8> {
    let mut out = Vec::new();
    sort_key(v, collation, &mut out).expect("a valid value has a key");
    out
}

fn hash(v: &Value, collation: &Collation) -> u64 {
    let mut state = DefaultHasher::new();
    hash_key(v, collation, &mut state).expect("a valid value has a key");
    state.finish()
}

/// Asserts that the keys of `a` and `b` order them as `compare` does, and that equal
/// values hash alike.
fn assert_consistent(a: &Value, b: &Value, collation: &Collation) -> Ordering {
    let expected = compare(a, b, collation)
        .expect("same family")
        .expect("no NULL");
    let (key_a, key_b) = (key(a, collation), key(b, collation));
    assert_eq!(
        key_a.cmp(&key_b),
        expected,
        "{a:?} against {b:?} under {collation:?}: keys {key_a:02X?} and {key_b:02X?}"
    );
    if expected == Ordering::Equal {
        assert_eq!(hash(a, collation), hash(b, collation), "{a:?} = {b:?}");
    }
    expected
}

#[test]
fn string_keys_order_as_compare_under_each_collation() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for name in COLLATIONS {
        let collation = Collation::parse(name).expect("a known collation");
        let mut equal_but_different = 0;
        for _ in 0..6_000 {
            let ascii = rng.chance(40);
            let prefix = rng.string(6, ascii);
            let a = format!("{prefix}{}", rng.string(4, ascii));
            let b = if rng.chance(60) {
                neighbour(&mut rng, &a)
            } else {
                format!("{prefix}{}", rng.string(4, ascii))
            };
            let ordering = assert_consistent(&text(&a), &text(&b), &collation);
            // The keys of two columns, end to end, order the rows column after column.
            let (c, d) = (rng.string(3, ascii), rng.string(3, ascii));
            let mut row_a = key(&text(&a), &collation);
            row_a.extend(key(&text(&c), &collation));
            let mut row_b = key(&text(&b), &collation);
            row_b.extend(key(&text(&d), &collation));
            assert_eq!(
                row_a.cmp(&row_b),
                ordering.then(collation.compare(&c, &d)),
                "({a:?}, {c:?}) against ({b:?}, {d:?}) under {name}"
            );
            if ordering == Ordering::Equal && a != b {
                equal_but_different += 1;
            }
        }
        // The pairs that differ in text and compare equal are the ones the key has to fold.
        assert!(
            equal_but_different >= 200,
            "{name}: {equal_but_different} equal pairs of different texts"
        );
    }
}

#[test]
fn string_keys_fold_what_each_collation_ignores() {
    let same = |name: &str, a: &str, b: &str| {
        let collation = Collation::parse(name).expect("a known collation");
        let mut key_a = Vec::new();
        let mut key_b = Vec::new();
        collation.sort_key(a, &mut key_a);
        collation.sort_key(b, &mut key_b);
        key_a == key_b
    };
    // Trailing spaces, under each family.
    for name in COLLATIONS {
        assert!(same(name, "ab", "ab  "), "{name}");
    }
    // Case and accents follow the flags.
    assert!(same("SQL_Latin1_General_CP1_CI_AS", "abc", "ABC"));
    assert!(!same("SQL_Latin1_General_CP1_CS_AS", "abc", "ABC"));
    assert!(!same("SQL_Latin1_General_CP1_CI_AS", "e", "é"));
    assert!(same("SQL_Latin1_General_CP1_CI_AI", "e", "É"));
    assert!(!same("SQL_Latin1_General_CP1_CI_AI", "C", "Ç"));
    // Expansions.
    assert!(same("Latin1_General_CI_AS", "Æ", "ae"));
    assert!(same("Latin1_General_100_CI_AS", "Straße", "STRASSE"));
    assert!(same("Latin1_General_CI_AI", "þ", "TH"));
    // The characters the Windows rules weigh last: the hyphen counts, NUL does not.
    assert!(!same("Latin1_General_CI_AS", "a-b", "ab"));
    assert!(same("Latin1_General_CI_AS", "a\0b", "ab"));
    assert!(same("Latin1_General_CI_AS", "a-b", "a－b"));
    assert!(!same("Latin1_General_CI_AS_WS", "a-b", "a－b"));
    // Width and kana.
    assert!(same("Latin1_General_CI_AS", "ａ", "a"));
    assert!(!same("Latin1_General_CI_AS_WS", "ａ", "a"));
    assert!(same("Latin1_General_CI_AS", "ア", "あ"));
    assert!(!same("Latin1_General_CI_AS_KS", "ア", "あ"));
    // Binary collations tell case and tabs apart.
    assert!(!same("Latin1_General_BIN", "a", "A"));
    assert!(!same("Latin1_General_BIN2", "a\t", "a"));
}

#[test]
fn string_keys_order_a_sorted_list() {
    let words = [
        "", " ", "a", "A", "á", "ab", "a-b", "a b", "aB", "Æ", "ae", "b", "Straße", "strasse", "ß",
        "-", "'", "z", "ÿ", "0", "\t", "ａ", "ア", "あ",
    ];
    for name in COLLATIONS {
        let collation = Collation::parse(name).expect("a known collation");
        let mut by_compare = words.to_vec();
        by_compare.sort_by(|a, b| collation.compare(a, b));
        let mut by_key = words.to_vec();
        by_key.sort_by_cached_key(|w| {
            let mut out = Vec::new();
            collation.sort_key(w, &mut out);
            out
        });
        for (x, y) in by_compare.iter().zip(&by_key) {
            assert_eq!(
                collation.compare(x, y),
                Ordering::Equal,
                "{name}: {x:?} {y:?}"
            );
        }
    }
}

#[test]
fn value_keys_order_as_compare_within_each_family() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let collation = Collation::DEFAULT;
    let small = |rng: &mut Rng| (rng.below(7) as i64) - 3;
    for _ in 0..4_000 {
        let (x, y) = (small(&mut rng), small(&mut rng));
        // Integers of different widths.
        let width = |rng: &mut Rng, v: i64| match rng.below(4) {
            0 if v >= 0 => Value::I8(v as u8),
            1 => Value::I16(v as i16),
            2 => Value::I32(v as i32),
            _ => Value::I64(v),
        };
        let (a, b) = (width(&mut rng, x), width(&mut rng, y));
        assert_consistent(&a, &b, &collation);
        // Decimals of different scales: `v / 2` written with 1 to 38 decimals.
        let decimal = |rng: &mut Rng, v: i64| {
            let scale = 1 + rng.below(38) as u32;
            Value::Decimal(Decimal {
                mantissa: i128::from(v) * 5 * 10_i128.pow(scale - 1),
                precision: 38,
                scale: scale as u8,
            })
        };
        let (a, b) = (decimal(&mut rng, x), decimal(&mut rng, y));
        assert_consistent(&a, &b, &collation);
        // Floats, both widths, both zeros.
        let float = |rng: &mut Rng, v: i64| {
            let f = v as f64 / 4.0;
            let f = if f == 0.0 && rng.chance(50) { -0.0 } else { f };
            if rng.chance(50) {
                Value::F32(f as f32)
            } else {
                Value::F64(f)
            }
        };
        let (a, b) = (float(&mut rng, x), float(&mut rng, y));
        assert_consistent(&a, &b, &collation);
        // Money.
        assert_consistent(&Value::Money(x * 10_000), &Value::Money(y), &collation);
        // Bytes, with and without trailing zeros.
        let bytes = |rng: &mut Rng| {
            let len = rng.below(5);
            Value::Bytes((0..len).map(|_| [0, 1, 0xFF][rng.below(3)]).collect())
        };
        let (a, b) = (bytes(&mut rng), bytes(&mut rng));
        assert_consistent(&a, &b, &collation);
        // Dates and times on both sides of their origin.
        let datetime2 = |days: i64, ticks: i64| DateTime2 {
            date: Date { days: days as i32 },
            time: Time {
                ticks_100ns: (ticks + 3) as u64,
            },
        };
        assert_consistent(
            &Value::Date(Date { days: x as i32 }),
            &Value::Date(Date { days: y as i32 }),
            &collation,
        );
        assert_consistent(
            &Value::DateTime(DateTime {
                days: x as i32,
                ticks_300th: (y + 3) as u32,
            }),
            &Value::DateTime(DateTime {
                days: y as i32,
                ticks_300th: (x + 3) as u32,
            }),
            &collation,
        );
        assert_consistent(
            &Value::DateTime2(datetime2(x, y)),
            &Value::DateTime2(datetime2(x, x)),
            &collation,
        );
        // A datetimeoffset compares on its instant, its offset left out.
        assert_consistent(
            &Value::DateTimeOffset(DateTimeOffset {
                utc: datetime2(x, y),
                offset_minutes: 60,
            }),
            &Value::DateTimeOffset(DateTimeOffset {
                utc: datetime2(y, x),
                offset_minutes: -120,
            }),
            &collation,
        );
        // Guids, through the order of their bytes.
        let guid = |rng: &mut Rng| {
            let mut g = [0u8; 16];
            g[rng.below(16)] = rng.below(2) as u8;
            g[rng.below(16)] = rng.below(2) as u8;
            Value::Guid(g)
        };
        let (a, b) = (guid(&mut rng), guid(&mut rng));
        assert_consistent(&a, &b, &collation);
        assert_consistent(&Value::Bit(x > 0), &Value::Bit(y > 0), &collation);
    }
}

#[test]
fn value_keys_fold_the_differences_compare_ignores() {
    let c = Collation::DEFAULT;
    let decimal = |mantissa, scale| {
        Value::Decimal(Decimal {
            mantissa,
            precision: 38,
            scale,
        })
    };
    assert_eq!(key(&Value::I8(5), &c), key(&Value::I64(5), &c));
    assert_eq!(key(&decimal(15, 1), &c), key(&decimal(15_000, 4), &c));
    assert_eq!(key(&decimal(0, 0), &c), key(&decimal(0, 38), &c));
    assert_eq!(key(&decimal(-1, 37), &c), key(&decimal(-10, 38), &c));
    assert!(key(&decimal(-2, 0), &c) < key(&decimal(-15, 1), &c));
    assert!(key(&decimal(i128::MAX, 0), &c) > key(&decimal(i128::MAX, 38), &c));
    assert_eq!(key(&Value::F64(-0.0), &c), key(&Value::F32(0.0), &c));
    assert_eq!(key(&Value::F32(0.5), &c), key(&Value::F64(0.5), &c));
    assert_eq!(
        key(&Value::Bytes(vec![1]), &c),
        key(&Value::Bytes(vec![1, 0, 0]), &c)
    );
    assert_ne!(
        key(&Value::Bytes(vec![0, 1]), &c),
        key(&Value::Bytes(vec![1]), &c)
    );
    // NULL has the smallest key, and families do not share keys.
    assert!(key(&Value::Null, &c) < key(&Value::Bit(false), &c));
    assert_ne!(key(&Value::I32(1), &c), key(&Value::Money(1), &c));
    assert_ne!(key(&Value::I32(0), &c), key(&Value::F64(0.0), &c));
}

#[test]
fn a_decimal_scale_beyond_38_has_no_key() {
    let v = Value::Decimal(Decimal {
        mantissa: 1,
        precision: 38,
        scale: 39,
    });
    let err = sort_key(&v, &Collation::DEFAULT, &mut Vec::new()).expect_err("scale 39");
    assert_eq!(err.number, 50000);
}

#[test]
fn concatenated_keys_order_rows_column_after_column() {
    let collation = Collation::DEFAULT;
    let row = |values: &[Value]| {
        let mut out = Vec::new();
        for v in values {
            sort_key(v, &collation, &mut out).expect("a key");
        }
        out
    };
    // The first column decides, however long its value.
    assert!(row(&[text("a"), text("z")]) < row(&[text("ab"), text("a")]));
    assert!(row(&[text("ab"), text("a")]) > row(&[text("a"), text("b")]));
    assert_eq!(
        row(&[text("a "), text("B")]),
        row(&[text("A"), text("b  ")])
    );
    assert!(
        row(&[Value::Bytes(vec![1]), Value::I32(9)])
            < row(&[Value::Bytes(vec![1, 0, 1]), Value::I32(0)])
    );
}
