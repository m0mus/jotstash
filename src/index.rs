use crate::buffer::TextBuffer;
use crate::parser::{parse, ParseResult};
use crate::spans::{NoteSpan, TagSpan, TodoSpan, TodoState};

/// Derived index over a parsed `.notes` file.
/// Wraps [`ParseResult`] and provides convenient accessors for CLI commands
/// and (later) TUI features.
pub struct FileIndex {
    pub result: ParseResult,
}

impl FileIndex {
    pub fn build(buffer: &TextBuffer) -> Self {
        Self { result: parse(buffer) }
    }

    pub fn note_count(&self) -> usize {
        self.result.notes.len()
    }

    pub fn notes(&self) -> &[NoteSpan] {
        &self.result.notes
    }

    pub fn note(&self, idx: usize) -> Option<&NoteSpan> {
        self.result.notes.get(idx)
    }

    /// All todos (open + done), in file order.
    pub fn todos_all(&self) -> impl Iterator<Item = (usize, &TodoSpan)> {
        self.result.todos.iter().map(|(ni, ts)| (*ni, ts))
    }

    /// Open todos only, in file order.
    pub fn todos_open(&self) -> impl Iterator<Item = (usize, &TodoSpan)> {
        self.result
            .todos
            .iter()
            .filter(|(_, ts)| ts.state == TodoState::Open)
            .map(|(ni, ts)| (*ni, ts))
    }

    pub fn tags_for_note(&self, note_idx: usize) -> impl Iterator<Item = &TagSpan> {
        self.result
            .tags
            .iter()
            .filter(move |(ni, _)| *ni == note_idx)
            .map(|(_, ts)| ts)
    }

    /// Sorted, deduplicated tag names across the whole file.
    pub fn unique_tag_names<'a>(&'a self, buffer: &'a TextBuffer) -> Vec<&'a str> {
        let mut names: Vec<&str> = self
            .result
            .tags
            .iter()
            .map(|(_, ts)| buffer.span_text(ts.name))
            .collect();
        names.sort_unstable_by_key(|s| s.to_ascii_lowercase());
        names.dedup();
        names
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> TextBuffer {
        TextBuffer::new(s.to_string())
    }

    #[test]
    fn index_note_count() {
        let b = buf("=== 2026-05-11\nbody1\n\n=== 2026-05-12\nbody2\n");
        let idx = FileIndex::build(&b);
        assert_eq!(idx.note_count(), 2);
    }

    #[test]
    fn todos_open_only() {
        let text = "=== 2026-05-12\nTODO: open one\nDONE: done one\n- [ ] open two\n- [x] done two\n";
        let b = buf(text);
        let idx = FileIndex::build(&b);
        assert_eq!(idx.todos_open().count(), 2);
        assert_eq!(idx.todos_all().count(), 4);
    }

    #[test]
    fn tags_for_note_scoped() {
        let text = "=== 2026-05-11\n#oci #helidon\n\n=== 2026-05-12\n#java\n";
        let b = buf(text);
        let idx = FileIndex::build(&b);
        let note0_tags: Vec<_> = idx.tags_for_note(0).map(|t| b.span_text(t.name)).collect();
        let note1_tags: Vec<_> = idx.tags_for_note(1).map(|t| b.span_text(t.name)).collect();
        assert_eq!(note0_tags.len(), 2);
        assert_eq!(note1_tags.len(), 1);
        assert!(note1_tags.contains(&"java"));
    }

    #[test]
    fn unique_tag_names_sorted_deduped() {
        let text = "=== 2026-05-11\n#oci #java\n\n=== 2026-05-12\n#oci #rust\n";
        let b = buf(text);
        let idx = FileIndex::build(&b);
        let names = idx.unique_tag_names(&b);
        assert_eq!(names, vec!["java", "oci", "rust"]); // sorted, deduped
    }
}
