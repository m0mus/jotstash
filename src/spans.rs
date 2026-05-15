/// A half-open byte range `[start, end)` into a `TextBuffer`'s content string.
/// All span byte offsets must sit on UTF-8 char boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "Span: start({start}) > end({end})");
        Self { start, end }
    }

    pub fn len(self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains_offset(self, offset: usize) -> bool {
        offset >= self.start && offset < self.end
    }

    pub fn contains(self, other: Span) -> bool {
        other.start >= self.start && other.end <= self.end
    }

    pub fn overlaps(self, other: Span) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// Maps between byte offsets and (line, column) positions.
///
/// Built from a text snapshot; rebuild after any mutation.
/// Lines are 0-indexed. Columns are byte offsets from the line start.
pub struct LineIndex {
    /// Byte offset of the first byte of each line.
    /// Always starts with 0; line N starts at `starts[N]`.
    starts: Vec<usize>,
    /// Total byte length of the indexed text.
    total: usize,
}

impl LineIndex {
    pub fn build(text: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self {
            starts,
            total: text.len(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset of the first byte of `line` (inclusive).
    pub fn line_start(&self, line: usize) -> Option<usize> {
        self.starts.get(line).copied()
    }

    /// Byte offset just past the last byte of `line` (exclusive).
    /// For lines with `\n`, this is the byte after `\n`.
    /// For the last line (no trailing `\n`), this is `total`.
    pub fn line_end(&self, line: usize) -> Option<usize> {
        if line >= self.starts.len() {
            return None;
        }
        let end = self
            .starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.total);
        Some(end)
    }

    pub fn line_span(&self, line: usize) -> Option<Span> {
        Some(Span::new(self.line_start(line)?, self.line_end(line)?))
    }

    /// Returns the 0-indexed line that contains `offset`.
    /// Clamps to the last valid line when `offset >= total`.
    pub fn offset_to_line(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(ins) => ins.saturating_sub(1),
        }
    }

    pub fn offset_to_line_col(&self, offset: usize) -> (usize, usize) {
        let line = self.offset_to_line(offset);
        let col = offset - self.starts[line];
        (line, col)
    }

    /// Convert (line, col) to a byte offset. Returns `None` if out of range.
    pub fn line_col_to_offset(&self, line: usize, col: usize) -> Option<usize> {
        let start = self.line_start(line)?;
        let end = self.line_end(line)?;
        let offset = start + col;
        (offset <= end).then_some(offset)
    }
}

// ---------------------------------------------------------------------------
// Domain span types — populated by parser.rs (phase 3)
// ---------------------------------------------------------------------------

/// The full extent of one note, from its `===` delimiter (or file start) to
/// the byte before the next `===` delimiter (or EOF).
#[derive(Debug, Clone)]
pub struct NoteSpan {
    /// Full note span (header line, if any, plus body).
    pub span: Span,
    /// The `===` line including its trailing newline.
    /// `None` for an implicit first note that has no delimiter.
    pub header: Option<Span>,
    /// Body: everything after the header line (or the whole note if no header).
    pub body: Span,
    /// `YYYY-MM-DD` token within the header, if present.
    pub date: Option<Span>,
    /// `HH:MM` token within the header, if present.
    pub time: Option<Span>,
    /// Title text within the header (after `|`), trimmed, if present.
    pub title: Option<Span>,
}

/// A `#tagname` token found in a note body.
#[derive(Debug, Clone, Copy)]
pub struct TagSpan {
    /// Includes the leading `#`.
    pub span: Span,
    /// Just the tag name (without `#`).
    pub name: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoKind {
    /// `TODO: text` / `DONE: text`
    Prefix,
    /// `- [ ] text` / `- [x] text`
    Checkbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoState {
    Open,
    Done,
}

impl TodoState {
    pub fn is_open(self) -> bool {
        matches!(self, TodoState::Open)
    }
    pub fn toggle(self) -> Self {
        match self {
            TodoState::Open => TodoState::Done,
            TodoState::Done => TodoState::Open,
        }
    }
}

/// A todo item found in a note body.
#[derive(Debug, Clone)]
pub struct TodoSpan {
    /// The full line including its trailing newline.
    pub span: Span,
    pub kind: TodoKind,
    pub state: TodoState,
    /// The task text (after `TODO: ` / `DONE: ` / `- [ ] ` prefix).
    pub text: Span,
    /// The `due:YYYY-MM-DD` token, if present.
    pub due: Option<Span>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Span ---------------------------------------------------------------

    #[test]
    fn span_len() {
        assert_eq!(Span::new(2, 5).len(), 3);
        assert_eq!(Span::new(0, 0).len(), 0);
    }

    #[test]
    fn span_contains_offset() {
        let s = Span::new(2, 5);
        assert!(!s.contains_offset(1));
        assert!(s.contains_offset(2));
        assert!(s.contains_offset(4));
        assert!(!s.contains_offset(5));
    }

    #[test]
    fn span_overlaps() {
        assert!(Span::new(0, 4).overlaps(Span::new(2, 6)));
        assert!(!Span::new(0, 2).overlaps(Span::new(2, 4)));
        assert!(!Span::new(2, 4).overlaps(Span::new(0, 2)));
    }

    #[test]
    fn span_contains() {
        let outer = Span::new(0, 10);
        assert!(outer.contains(Span::new(2, 5)));
        assert!(outer.contains(Span::new(0, 10)));
        assert!(!outer.contains(Span::new(5, 11)));
    }

    // ---- LineIndex ----------------------------------------------------------

    fn idx(text: &str) -> LineIndex {
        LineIndex::build(text)
    }

    #[test]
    fn empty_text() {
        let li = idx("");
        assert_eq!(li.line_count(), 1); // one empty line
        assert_eq!(li.line_start(0), Some(0));
        assert_eq!(li.line_end(0), Some(0));
        assert_eq!(li.line_start(1), None);
    }

    #[test]
    fn single_line_no_newline() {
        let li = idx("hello");
        assert_eq!(li.line_count(), 1);
        assert_eq!(li.line_span(0), Some(Span::new(0, 5)));
    }

    #[test]
    fn single_line_with_newline() {
        let li = idx("hello\n");
        assert_eq!(li.line_count(), 2);
        assert_eq!(li.line_span(0), Some(Span::new(0, 6))); // includes \n
        assert_eq!(li.line_span(1), Some(Span::new(6, 6))); // empty final line
    }

    #[test]
    fn multi_line() {
        let text = "aaa\nbbb\nccc";
        let li = idx(text);
        assert_eq!(li.line_count(), 3);
        assert_eq!(li.line_span(0), Some(Span::new(0, 4)));
        assert_eq!(li.line_span(1), Some(Span::new(4, 8)));
        assert_eq!(li.line_span(2), Some(Span::new(8, 11)));
    }

    #[test]
    fn crlf_line_endings() {
        let text = "aaa\r\nbbb\r\n";
        let li = idx(text);
        // Each line span includes \r\n
        assert_eq!(li.line_span(0), Some(Span::new(0, 5)));
        assert_eq!(li.line_span(1), Some(Span::new(5, 10)));
        assert_eq!(li.line_span(2), Some(Span::new(10, 10)));
    }

    #[test]
    fn offset_to_line_basic() {
        let text = "aaa\nbbb\nccc";
        let li = idx(text);
        assert_eq!(li.offset_to_line(0), 0); // 'a'
        assert_eq!(li.offset_to_line(3), 0); // '\n' (still line 0)
        assert_eq!(li.offset_to_line(4), 1); // first 'b'
        assert_eq!(li.offset_to_line(8), 2); // first 'c'
    }

    #[test]
    fn line_col_round_trip() {
        let text = "hello\nworld\n";
        let li = idx(text);
        for offset in 0..text.len() {
            let (line, col) = li.offset_to_line_col(offset);
            assert_eq!(li.line_col_to_offset(line, col), Some(offset));
        }
    }

    #[test]
    fn unicode_multibyte() {
        // "é" is 2 bytes in UTF-8 (U+00E9 = 0xC3 0xA9)
        let text = "café\nok";
        let li = idx(text);
        assert_eq!(li.line_count(), 2);
        // "café" is 5 bytes (c=1, a=1, f=1, é=2) + newline = 6 bytes for line 0
        assert_eq!(li.line_span(0), Some(Span::new(0, 6)));
        assert_eq!(li.line_span(1), Some(Span::new(6, 8)));
    }

    #[test]
    fn offset_to_line_clamped_at_total() {
        let text = "ab";
        let li = idx(text);
        assert_eq!(li.offset_to_line(100), 0); // only one line
    }
}
