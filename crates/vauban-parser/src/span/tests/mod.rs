use super::{LineIndex, Span};

#[test]
fn spans_are_structurally_equal() {
    let a = Span {
        line: 1,
        column: 1,
        offset: 0,
        len: 3,
    };
    let b = Span {
        line: 9,
        column: 9,
        offset: 99,
        len: 1,
    };
    assert_eq!(a, b);
    assert!(!a.same_position(&b));
    // `Span` is `Copy`, so this copies rather than clones.
    let copy = a;
    assert!(a.same_position(&copy));
}

#[test]
fn line_index_maps_offsets() {
    // Offset 15 is the `\r`, 16 the `\n`, 17 the `W`.
    let text = "SELECT 1\nFROM t\r\nWHERE x";
    let index = LineIndex::new(text);
    assert_eq!(index.position(0), (1, 1));
    assert_eq!(index.position(7), (1, 8));
    assert_eq!(index.position(9), (2, 1));
    assert_eq!(index.position(17), (3, 1));
    // The `\n` of a `\r\n` still belongs to the line it ends.
    assert_eq!(index.position(16).0, 2);
    // Out of bounds yields the last position.
    assert_eq!(index.position(9_999), (3, 8));
}

#[test]
fn lone_carriage_return_does_not_end_a_line() {
    let index = LineIndex::new("a\rb");
    assert_eq!(index.position(2), (1, 3));
}

#[test]
fn columns_count_characters_not_bytes() {
    let index = LineIndex::new("N'é' x");
    assert_eq!(index.position(6), (1, 6));
}

#[test]
fn empty_span_is_line_one_column_one() {
    assert!(Span::EMPTY.same_position(&Span {
        line: 1,
        column: 1,
        offset: 0,
        len: 0,
    }));
}
