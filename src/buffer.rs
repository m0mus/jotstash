use crate::spans::{LineIndex, Span};
use anyhow::Result;
use std::path::Path;

/// The core text container for a `.notes` file.
///
/// Holds the raw content as a UTF-8 string and maintains a [`LineIndex`]
/// for O(log n) byte-offset ↔ (line, col) conversions.
///
/// Every mutation (replace_span, append) rebuilds the LineIndex. This is
/// inexpensive for files up to tens of thousands of lines and keeps the
/// invariant that `line_index()` always reflects the current content.
///
/// Cursor movement, undo/redo, and selection are added in MVP-1 (phase 7/9)
/// when the TUI editor is built on top of this buffer.
pub struct TextBuffer {
    content: String,
    line_index: LineIndex,
}

impl TextBuffer {
    pub fn new(content: String) -> Self {
        let line_index = LineIndex::build(&content);
        Self { content, line_index }
    }

    pub fn empty() -> Self {
        Self::new(String::new())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::new(content))
    }

    // ---- Read access --------------------------------------------------------

    pub fn as_str(&self) -> &str {
        &self.content
    }

    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.line_index.line_count()
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    /// Returns the text covered by `span`. Panics if `span` is out of range
    /// or not on char boundaries (only occurs with a bug in the parser).
    pub fn span_text(&self, span: Span) -> &str {
        &self.content[span.start..span.end]
    }

    /// Returns the full text of line `line` including its trailing newline,
    /// or `None` if `line` is out of range.
    pub fn line_text(&self, line: usize) -> Option<&str> {
        let span = self.line_index.line_span(line)?;
        Some(self.span_text(span))
    }

    // ---- Mutation -----------------------------------------------------------

    /// Replace the text in `[span.start, span.end)` with `replacement`.
    ///
    /// All spans previously derived from this buffer become invalid and must
    /// be re-derived via the parser. This is intentional: the buffer is the
    /// source of truth; indexes are derived, not co-maintained.
    pub fn replace_span(&mut self, span: Span, replacement: &str) {
        self.content.replace_range(span.start..span.end, replacement);
        self.line_index = LineIndex::build(&self.content);
    }

    /// Append `text` at the end of the buffer.
    pub fn append(&mut self, text: &str) {
        self.content.push_str(text);
        self.line_index = LineIndex::build(&self.content);
    }

    /// Consume the buffer and return the owned content string (for atomic writes).
    pub fn into_content(self) -> String {
        self.content
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer() {
        let buf = TextBuffer::empty();
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.line_text(0), Some(""));
        assert_eq!(buf.line_text(1), None);
    }

    #[test]
    fn line_text() {
        let buf = TextBuffer::new("hello\nworld\n".into());
        assert_eq!(buf.line_text(0), Some("hello\n"));
        assert_eq!(buf.line_text(1), Some("world\n"));
        assert_eq!(buf.line_text(2), Some(""));
        assert_eq!(buf.line_text(3), None);
    }

    #[test]
    fn span_text() {
        let buf = TextBuffer::new("hello world".into());
        assert_eq!(buf.span_text(Span::new(6, 11)), "world");
        assert_eq!(buf.span_text(Span::new(0, 5)), "hello");
    }

    #[test]
    fn append_updates_line_index() {
        let mut buf = TextBuffer::new("line1\n".into());
        assert_eq!(buf.line_count(), 2); // "line1\n" + empty line
        buf.append("line2\n");
        assert_eq!(buf.line_count(), 3);
        assert_eq!(buf.line_text(1), Some("line2\n"));
    }

    #[test]
    fn replace_span_middle() {
        let mut buf = TextBuffer::new("TODO: old task\n".into());
        // Replace "old task" with "new task"
        let span = Span::new(6, 14); // "old task"
        buf.replace_span(span, "new task");
        assert_eq!(buf.as_str(), "TODO: new task\n");
    }

    #[test]
    fn replace_span_todo_toggle() {
        let mut buf = TextBuffer::new("TODO: pay bill\n".into());
        // Toggle: replace "TODO" with "DONE"
        buf.replace_span(Span::new(0, 4), "DONE");
        assert_eq!(buf.as_str(), "DONE: pay bill\n");
    }

    #[test]
    fn replace_span_checkbox_toggle() {
        let mut buf = TextBuffer::new("- [ ] pay bill\n".into());
        // Toggle: replace "[ ]" with "[x]"
        buf.replace_span(Span::new(2, 5), "[x]");
        assert_eq!(buf.as_str(), "- [x] pay bill\n");
    }

    #[test]
    fn replace_span_rebuilds_line_index() {
        let mut buf = TextBuffer::new("aaa\nbbb\nccc\n".into());
        // Insert a newline in the middle of line 1 — creates new line
        let span = Span::new(4, 7); // "bbb"
        buf.replace_span(span, "bb\nbb");
        assert_eq!(buf.line_count(), 5); // was 4 (aaa, bbb, ccc, empty), now 5
        assert_eq!(buf.line_text(1), Some("bb\n"));
        assert_eq!(buf.line_text(2), Some("bb\n"));
    }

    #[test]
    fn span_text_unicode() {
        let buf = TextBuffer::new("café\n".into());
        // "café" = c(0) a(1) f(2) é(3,4) = bytes 0..5, newline at 5
        let span = Span::new(0, 5);
        assert_eq!(buf.span_text(span), "café");
    }

    #[test]
    fn into_content_roundtrip() {
        let text = "=== 2026-05-12 | Hello\nBody text\n";
        let buf = TextBuffer::new(text.into());
        assert_eq!(buf.into_content(), text);
    }
}
