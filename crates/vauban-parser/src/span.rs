//! Source positions: [`Span`], attached to every AST node, and `LineIndex`, the
//! precomputed table of line starts used to turn a byte offset into a line and a column.

/// The position of a construct in the text of the batch it was parsed from.
///
/// `line` and `column` are **1-based**, as SQL Server numbers them (this is what
/// `SqlError::line` carries). `offset` and `len` are **byte** counts inside the batch
/// text, while `column` counts **characters**, so that `N'é'` does not shift the column.
///
/// # Equality is always true
///
/// [`PartialEq`] is implemented by hand and `eq` returns **`true` for any pair of
/// spans**. Comparing two AST values is therefore purely structural: the `parse` →
/// `Display` → `parse` loop tests of the whole module compare trees whose positions
/// necessarily differ. Use [`Span::same_position`] to compare positions for real.
///
/// Consequence: neither `Span` nor any AST node derives `Hash`, because
/// `a == b` no longer implies `hash(a) == hash(b)`.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    /// 1-based line of the first character, in the batch text.
    pub line: u32,
    /// 1-based column of the first character, counted in characters.
    pub column: u32,
    /// Byte offset of the first character, from the start of the batch text.
    pub offset: u32,
    /// Length in bytes.
    pub len: u32,
}

impl Span {
    /// The span of a node built by hand, with no text behind it: line 1, column 1,
    /// offset 0, length 0.
    pub const EMPTY: Self = Self {
        line: 1,
        column: 1,
        offset: 0,
        len: 0,
    };

    /// Compares the four fields, which [`PartialEq`] deliberately does not do.
    #[must_use]
    pub fn same_position(&self, other: &Self) -> bool {
        self.line == other.line
            && self.column == other.column
            && self.offset == other.offset
            && self.len == other.len
    }
}

/// Always `true`: see the type documentation. Positions are compared by
/// [`Span::same_position`].
impl PartialEq for Span {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Span {}

/// Line starts of a batch text, to turn a byte offset into a (line, column) pair.
///
/// Line endings are `\n` and `\r\n`; a lone `\r` does **not** end a line, and a `\r\n`
/// counts for a single ending. Columns count characters, offsets count bytes.
pub(crate) struct LineIndex<'a> {
    text: &'a str,
    line_starts: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    /// Precomputes the byte offset at which each line of `text` starts.
    pub(crate) fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0u32];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(index as u32 + 1);
            }
        }
        Self { text, line_starts }
    }

    /// Returns the 1-based line and column of `offset`.
    ///
    /// An offset past the end of the text yields the last position instead of panicking.
    pub(crate) fn position(&self, offset: u32) -> (u32, u32) {
        let clamped = offset.min(self.text.len() as u32);
        let index = self
            .line_starts
            .partition_point(|&start| start <= clamped)
            .saturating_sub(1);
        let line_start = self
            .line_starts
            .get(index)
            .map_or(0, |&start| start as usize);
        let column = self
            .text
            .get(line_start..clamped as usize)
            .map_or(1, |slice| slice.chars().count() as u32 + 1);
        (index as u32 + 1, column)
    }
}

#[cfg(test)]
mod tests;
