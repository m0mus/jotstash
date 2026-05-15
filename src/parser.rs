use crate::buffer::TextBuffer;
use crate::spans::{LineIndex, NoteSpan, Span, TagSpan, TodoKind, TodoSpan, TodoState};

pub struct ParseResult {
    pub notes: Vec<NoteSpan>,
    /// (note_idx, tag)
    pub tags: Vec<(usize, TagSpan)>,
    /// (note_idx, todo)
    pub todos: Vec<(usize, TodoSpan)>,
}

pub fn parse(buffer: &TextBuffer) -> ParseResult {
    let text = buffer.as_str();
    let li = buffer.line_index();

    let delim_lines: Vec<usize> = (0..li.line_count())
        .filter(|&l| {
            buffer
                .line_text(l)
                .map(is_delimiter_line)
                .unwrap_or(false)
        })
        .collect();

    let notes = build_note_spans(text, li, &delim_lines);

    let mut tags = Vec::new();
    let mut todos = Vec::new();
    for (note_idx, note) in notes.iter().enumerate() {
        scan_body(text, note.body, note_idx, &mut tags, &mut todos);
    }

    ParseResult { notes, tags, todos }
}

// ---------------------------------------------------------------------------
// Delimiter detection
// ---------------------------------------------------------------------------

fn is_delimiter_line(line: &str) -> bool {
    line.starts_with("===")
}

fn is_fence_line(line: &str) -> bool {
    let t = line.trim_start_matches([' ', '\t']);
    t.starts_with("```") || t.starts_with("~~~")
}

// ---------------------------------------------------------------------------
// Note splitting
// ---------------------------------------------------------------------------

fn build_note_spans(text: &str, li: &LineIndex, delim_lines: &[usize]) -> Vec<NoteSpan> {
    // note_bounds: (start_line, Option<header_line>)
    let mut note_bounds: Vec<(usize, Option<usize>)> = Vec::new();

    // If first delimiter is not line 0, there is an implicit first note.
    if delim_lines.first().map_or(true, |&dl| dl > 0) {
        note_bounds.push((0, None));
    }
    for &dl in delim_lines {
        note_bounds.push((dl, Some(dl)));
    }

    let total = text.len();
    let line_count = li.line_count();

    note_bounds
        .iter()
        .enumerate()
        .map(|(i, &(start_line, header_line))| {
            let end_line = note_bounds.get(i + 1).map(|&(l, _)| l).unwrap_or(line_count);

            let span_start = li.line_start(start_line).unwrap_or(0);
            let span_end = li.line_start(end_line).unwrap_or(total);
            let span = Span::new(span_start, span_end);

            match header_line {
                Some(hl) => {
                    let header_span = li.line_span(hl).unwrap();
                    let body_line = hl + 1;
                    let body_start = li.line_start(body_line).unwrap_or(span_end);
                    let body = Span::new(body_start, span_end);
                    let (date, time, title) = parse_header(text, header_span);
                    NoteSpan { span, header: Some(header_span), body, date, time, title }
                }
                None => NoteSpan {
                    span,
                    header: None,
                    body: span,
                    date: None,
                    time: None,
                    title: None,
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Header parsing: `===` followed by an optional `|`-separated pair of
// segments. The date (`YYYY-MM-DD [HH:MM]`) may appear on either side of the
// pipe; the title is whatever non-date segment remains. Both orderings are
// supported transparently.
// ---------------------------------------------------------------------------

fn parse_header(text: &str, header: Span) -> (Option<Span>, Option<Span>, Option<Span>) {
    let s = &text[header.start..header.end];
    let base = header.start;

    if !s.starts_with("===") {
        return (None, None, None);
    }

    let content_end = s.trim_end_matches(['\n', '\r']).len();
    let after_eq = skip_sp(s, 3);

    // Split header content (after `===`) on the first `|`.
    let pipe_pos = s[after_eq..content_end].find('|').map(|i| after_eq + i);
    let (left_s, left_e, right_seg) = match pipe_pos {
        Some(p) => {
            let left_e = trim_trailing_spaces(s, after_eq, p);
            let right_s = skip_sp(s, p + 1);
            let right_e = trim_trailing_spaces(s, right_s, content_end);
            (after_eq, left_e, Some((right_s, right_e)))
        }
        None => {
            let left_e = trim_trailing_spaces(s, after_eq, content_end);
            (after_eq, left_e, None)
        }
    };

    let left_dt = parse_date_segment(s, left_s, left_e, base);
    let right_dt = right_seg.and_then(|(rs, re)| parse_date_segment(s, rs, re, base));

    // Decide which side holds the date; the other side (if any) is the title.
    let (date, time, title_range) = match (left_dt, right_dt) {
        // Date on left → title is right (old format, or no title).
        (Some((d, t)), _) => (Some(d), t, right_seg),
        // Date on right → title is left (new format).
        (None, Some((d, t))) => (Some(d), t, Some((left_s, left_e))),
        // No date anywhere.
        (None, None) => match right_seg {
            // With a pipe but no date, treat left as the title.
            Some(_) => (None, None, Some((left_s, left_e))),
            // No pipe: a single segment that isn't a date → it's the title.
            None => (None, None, Some((left_s, left_e))),
        },
    };

    let title = title_range.and_then(|(rs, re)| {
        if rs < re {
            Some(Span::new(base + rs, base + re))
        } else {
            None
        }
    });

    (date, time, title)
}

/// Try to parse `s[seg_start..seg_end]` as `YYYY-MM-DD [HH:MM]`. Returns
/// the date span and optional time span if the segment *starts* with a date.
fn parse_date_segment(
    s: &str,
    seg_start: usize,
    seg_end: usize,
    base: usize,
) -> Option<(Span, Option<Span>)> {
    if seg_start + 10 > seg_end || !is_date_str(&s[seg_start..seg_start + 10]) {
        return None;
    }
    let date = Span::new(base + seg_start, base + seg_start + 10);
    let mut pos = seg_start + 10;
    pos = skip_sp(s, pos);
    let time = if pos + 5 <= seg_end && is_time_str(&s[pos..pos + 5]) {
        Some(Span::new(base + pos, base + pos + 5))
    } else {
        None
    };
    Some((date, time))
}

fn trim_trailing_spaces(s: &str, start: usize, mut end: usize) -> usize {
    while end > start && s.as_bytes()[end - 1] == b' ' {
        end -= 1;
    }
    end
}

fn skip_sp(s: &str, mut pos: usize) -> usize {
    while pos < s.len() && s.as_bytes()[pos] == b' ' {
        pos += 1;
    }
    pos
}

fn is_date_str(s: &str) -> bool {
    if s.len() < 10 {
        return false;
    }
    let b = s.as_bytes();
    b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn is_time_str(s: &str) -> bool {
    if s.len() < 5 {
        return false;
    }
    let b = s.as_bytes();
    b[2] == b':' && b[..2].iter().all(u8::is_ascii_digit) && b[3..5].iter().all(u8::is_ascii_digit)
}

// ---------------------------------------------------------------------------
// Body scanning — tags and todos, respecting code fences and inline code
// ---------------------------------------------------------------------------

fn scan_body(
    text: &str,
    body: Span,
    note_idx: usize,
    tags: &mut Vec<(usize, TagSpan)>,
    todos: &mut Vec<(usize, TodoSpan)>,
) {
    if body.is_empty() {
        return;
    }

    let body_text = &text[body.start..body.end];
    let base = body.start;
    let mut in_fence = false;
    let mut offset = 0usize;

    while offset < body_text.len() {
        let rest = &body_text[offset..];
        let line_len = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let line = &rest[..line_len];
        let abs_start = base + offset;

        if is_fence_line(line) {
            in_fence = !in_fence;
            offset += line_len;
            continue;
        }

        if !in_fence {
            if let Some(todo) = scan_line_for_todo(line, abs_start) {
                todos.push((note_idx, todo));
            }
            scan_line_for_tags(line, abs_start, note_idx, tags);
        }

        offset += line_len;
    }
}

// ---------------------------------------------------------------------------
// Todo detection
// ---------------------------------------------------------------------------

fn scan_line_for_todo(line: &str, line_start: usize) -> Option<TodoSpan> {
    let indent = leading_whitespace(line);
    let trimmed = &line[indent..];
    if trimmed.is_empty() {
        return None;
    }

    let content_len = line.trim_end_matches(['\n', '\r']).len();

    // Checkbox: - [ ] or - [x]
    if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
        let state = if trimmed.starts_with("- [ ]") {
            TodoState::Open
        } else {
            TodoState::Done
        };
        let box_end = indent + 5; // end of "- [ ]" or "- [x]"
        let text_start = if line.as_bytes().get(box_end) == Some(&b' ') {
            box_end + 1
        } else {
            box_end
        };
        let due = find_due_span(line, line_start);
        return Some(TodoSpan {
            span: Span::new(line_start, line_start + line.len()),
            kind: TodoKind::Checkbox,
            state,
            text: Span::new(
                line_start + text_start,
                (line_start + content_len).max(line_start + text_start),
            ),
            due,
        });
    }

    // Prefix: TODO: or DONE:
    if trimmed.starts_with("TODO:") || trimmed.starts_with("DONE:") {
        let state = if trimmed.starts_with("TODO:") {
            TodoState::Open
        } else {
            TodoState::Done
        };
        let colon_end = indent + 5; // end of "TODO:" or "DONE:"
        let text_start = if line.as_bytes().get(colon_end) == Some(&b' ') {
            colon_end + 1
        } else {
            colon_end
        };
        let due = find_due_span(line, line_start);
        return Some(TodoSpan {
            span: Span::new(line_start, line_start + line.len()),
            kind: TodoKind::Prefix,
            state,
            text: Span::new(
                line_start + text_start,
                (line_start + content_len).max(line_start + text_start),
            ),
            due,
        });
    }

    None
}

fn leading_whitespace(s: &str) -> usize {
    s.as_bytes().iter().take_while(|&&b| b == b' ' || b == b'\t').count()
}

fn find_due_span(line: &str, line_start: usize) -> Option<Span> {
    let mut search = 0;
    while let Some(rel) = line[search..].find("due:") {
        let pos = search + rel;
        let preceded_ok = pos == 0 || matches!(line.as_bytes()[pos - 1], b' ' | b'\t');
        let date_pos = pos + 4;
        if preceded_ok
            && date_pos + 10 <= line.len()
            && is_date_str(&line[date_pos..date_pos + 10])
        {
            return Some(Span::new(line_start + pos, line_start + date_pos + 10));
        }
        search = pos + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Tag detection
// ---------------------------------------------------------------------------

fn scan_line_for_tags(
    line: &str,
    line_start: usize,
    note_idx: usize,
    tags: &mut Vec<(usize, TagSpan)>,
) {
    let code_ranges = inline_code_ranges(line);
    let bytes = line.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        if bytes[pos] != b'#' {
            pos += 1;
            continue;
        }

        if in_code_range(pos, &code_ranges) {
            pos += 1;
            continue;
        }

        // Must be at line start or preceded by whitespace
        let preceded_ok =
            pos == 0 || matches!(bytes[pos - 1], b' ' | b'\t' | b'\n' | b'\r');
        if !preceded_ok {
            pos += 1;
            continue;
        }

        let name_start = pos + 1;
        if name_start >= bytes.len() || !is_tag_name_start(bytes[name_start]) {
            pos += 1;
            continue;
        }

        let mut name_end = name_start + 1;
        while name_end < bytes.len() && is_tag_name_char(bytes[name_end]) {
            name_end += 1;
        }

        tags.push((
            note_idx,
            TagSpan {
                span: Span::new(line_start + pos, line_start + name_end),
                name: Span::new(line_start + name_start, line_start + name_end),
            },
        ));
        pos = name_end;
    }
}

/// Tag name first char: must not be a digit.
fn is_tag_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || matches!(b, b'_' | b'/' | b'-')
}

/// Tag name subsequent chars: alphanumeric or `_`, `/`, `-`.
fn is_tag_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'-')
}

// ---------------------------------------------------------------------------
// Inline code ranges (single-backtick spans only for MVP-0)
// ---------------------------------------------------------------------------

fn inline_code_ranges(line: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = line.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        if bytes[pos] == b'`' {
            let start = pos;
            pos += 1;
            // Find closing backtick (not past end of line)
            while pos < bytes.len() && bytes[pos] != b'`' && bytes[pos] != b'\n' {
                pos += 1;
            }
            if pos < bytes.len() && bytes[pos] == b'`' {
                ranges.push((start, pos + 1));
                pos += 1;
            }
        } else {
            pos += 1;
        }
    }

    ranges
}

fn in_code_range(pos: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(s, e)| pos >= s && pos < e)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextBuffer;

    fn buf(s: &str) -> TextBuffer {
        TextBuffer::new(s.to_string())
    }

    fn text_at(buf: &TextBuffer, span: Span) -> &str {
        buf.span_text(span)
    }

    // ---- Delimiter detection -----------------------------------------------

    #[test]
    fn delimiter_line_variants() {
        assert!(is_delimiter_line("===\n"));
        assert!(is_delimiter_line("=== 2026-05-12\n"));
        assert!(is_delimiter_line("=== 2026-05-12 | Title\n"));
        assert!(!is_delimiter_line(" ===\n")); // leading space → not delimiter
        assert!(!is_delimiter_line("== not\n"));
        assert!(!is_delimiter_line("body text\n"));
    }

    // ---- Header parsing ----------------------------------------------------

    #[test]
    fn header_bare() {
        let b = buf("===\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert!(note.date.is_none());
        assert!(note.time.is_none());
        assert!(note.title.is_none());
    }

    #[test]
    fn header_date_only() {
        let b = buf("=== 2026-05-12\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert_eq!(text_at(&b, note.date.unwrap()), "2026-05-12");
        assert!(note.time.is_none());
        assert!(note.title.is_none());
    }

    #[test]
    fn header_date_time() {
        let b = buf("=== 2026-05-12 15:22\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert_eq!(text_at(&b, note.date.unwrap()), "2026-05-12");
        assert_eq!(text_at(&b, note.time.unwrap()), "15:22");
        assert!(note.title.is_none());
    }

    #[test]
    fn header_date_title() {
        let b = buf("=== 2026-05-12 | Meeting notes\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert_eq!(text_at(&b, note.date.unwrap()), "2026-05-12");
        assert!(note.time.is_none());
        assert_eq!(text_at(&b, note.title.unwrap()), "Meeting notes");
    }

    #[test]
    fn header_full() {
        let b = buf("=== 2026-05-12 09:14 | Morning standup\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert_eq!(text_at(&b, note.date.unwrap()), "2026-05-12");
        assert_eq!(text_at(&b, note.time.unwrap()), "09:14");
        assert_eq!(text_at(&b, note.title.unwrap()), "Morning standup");
    }

    // ---- New title-first format -------------------------------------------

    #[test]
    fn header_new_format_title_date() {
        // `=== Title | Date` — title on the left, date on the right.
        let b = buf("=== Morning standup | 2026-05-12 09:14\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert_eq!(text_at(&b, note.date.unwrap()), "2026-05-12");
        assert_eq!(text_at(&b, note.time.unwrap()), "09:14");
        assert_eq!(text_at(&b, note.title.unwrap()), "Morning standup");
    }

    #[test]
    fn header_new_format_title_only() {
        // Draft: user typed title but hasn't stamped a date yet.
        let b = buf("=== Just a title\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert!(note.date.is_none());
        assert!(note.time.is_none());
        assert_eq!(text_at(&b, note.title.unwrap()), "Just a title");
    }

    #[test]
    fn header_new_format_date_only_no_time() {
        let b = buf("=== Project kickoff | 2026-05-12\n");
        let result = parse(&b);
        let note = &result.notes[0];
        assert_eq!(text_at(&b, note.date.unwrap()), "2026-05-12");
        assert!(note.time.is_none());
        assert_eq!(text_at(&b, note.title.unwrap()), "Project kickoff");
    }

    // ---- Note splitting ----------------------------------------------------

    #[test]
    fn single_note_with_header() {
        let b = buf("=== 2026-05-12 | Hello\nBody text\n");
        let result = parse(&b);
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes[0].header.is_some());
    }

    #[test]
    fn implicit_first_note() {
        let b = buf("Implicit content\n\n=== 2026-05-12\nSecond note\n");
        let result = parse(&b);
        assert_eq!(result.notes.len(), 2);
        assert!(result.notes[0].header.is_none(), "first note has no header");
        assert!(result.notes[1].header.is_some());
        assert_eq!(&b.as_str()[result.notes[0].body.start..result.notes[0].body.end], "Implicit content\n\n");
    }

    #[test]
    fn multiple_notes() {
        let text = "=== 2026-05-11 | Note 1\nbody1\n\n=== 2026-05-12 | Note 2\nbody2\n";
        let b = buf(text);
        let result = parse(&b);
        assert_eq!(result.notes.len(), 2);
        assert_eq!(text_at(&b, result.notes[0].title.unwrap()), "Note 1");
        assert_eq!(text_at(&b, result.notes[1].title.unwrap()), "Note 2");
    }

    #[test]
    fn bare_separator_only() {
        let b = buf("===\n");
        let result = parse(&b);
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes[0].header.is_some());
        assert!(result.notes[0].date.is_none());
    }

    #[test]
    fn empty_file() {
        let b = buf("");
        let result = parse(&b);
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes[0].header.is_none());
    }

    // ---- Tag parsing -------------------------------------------------------

    #[test]
    fn tags_in_body() {
        let b = buf("=== 2026-05-12\n#oci #helidon\n");
        let result = parse(&b);
        assert_eq!(result.tags.len(), 2);
        let names: Vec<&str> = result.tags.iter().map(|(_, t)| b.span_text(t.name)).collect();
        assert!(names.contains(&"oci"));
        assert!(names.contains(&"helidon"));
    }

    #[test]
    fn tag_at_line_start() {
        let b = buf("=== 2026-05-12\n#tag1\n");
        let result = parse(&b);
        assert_eq!(result.tags.len(), 1);
        assert_eq!(b.span_text(result.tags[0].1.name), "tag1");
    }

    #[test]
    fn tag_with_hyphen_and_slash() {
        let b = buf("=== 2026-05-12\n#helidon-oci #project/sub\n");
        let result = parse(&b);
        let names: Vec<&str> = result.tags.iter().map(|(_, t)| b.span_text(t.name)).collect();
        assert!(names.contains(&"helidon-oci"));
        assert!(names.contains(&"project/sub"));
    }

    #[test]
    fn tag_numeric_rejected() {
        // #123 must not be recognized as a tag
        let b = buf("=== 2026-05-12\n#123 and normal text\n");
        let result = parse(&b);
        assert_eq!(result.tags.len(), 0, "#123 should not be a tag");
    }

    #[test]
    fn no_tag_in_heading() {
        // "# Heading" (space after #) is NOT a tag
        let b = buf("=== 2026-05-12\n# This is a heading\n");
        let result = parse(&b);
        assert_eq!(result.tags.len(), 0);
    }

    #[test]
    fn no_tag_in_url_fragment() {
        // "url#fragment" — # not preceded by whitespace, should be ignored
        let b = buf("=== 2026-05-12\nSee https://example.com#section for details\n");
        let result = parse(&b);
        assert_eq!(result.tags.len(), 0);
    }

    #[test]
    fn no_tag_inside_inline_code() {
        let b = buf("=== 2026-05-12\nUse `#oci` in your queries\n");
        let result = parse(&b);
        assert_eq!(result.tags.len(), 0, "tag inside inline code should be ignored");
    }

    #[test]
    fn no_tag_inside_code_fence() {
        let b = buf("=== 2026-05-12\n```\n#oci inside fence\n```\n#real_tag\n");
        let result = parse(&b);
        let names: Vec<&str> = result.tags.iter().map(|(_, t)| b.span_text(t.name)).collect();
        assert!(!names.contains(&"oci"), "tag inside fence should be ignored");
        assert!(names.contains(&"real_tag"), "tag outside fence should be found");
    }

    // ---- Todo parsing ------------------------------------------------------

    #[test]
    fn todo_prefix_open() {
        let b = buf("=== 2026-05-12\nTODO: pay the bill\n");
        let result = parse(&b);
        assert_eq!(result.todos.len(), 1);
        let (_, todo) = &result.todos[0];
        assert_eq!(todo.state, TodoState::Open);
        assert_eq!(todo.kind, TodoKind::Prefix);
        assert_eq!(b.span_text(todo.text), "pay the bill");
    }

    #[test]
    fn todo_prefix_done() {
        let b = buf("=== 2026-05-12\nDONE: pay the bill\n");
        let result = parse(&b);
        let (_, todo) = &result.todos[0];
        assert_eq!(todo.state, TodoState::Done);
    }

    #[test]
    fn todo_checkbox_open() {
        let b = buf("=== 2026-05-12\n- [ ] review PR\n");
        let result = parse(&b);
        assert_eq!(result.todos.len(), 1);
        let (_, todo) = &result.todos[0];
        assert_eq!(todo.state, TodoState::Open);
        assert_eq!(todo.kind, TodoKind::Checkbox);
        assert_eq!(b.span_text(todo.text), "review PR");
    }

    #[test]
    fn todo_checkbox_done() {
        let b = buf("=== 2026-05-12\n- [x] review PR\n");
        let result = parse(&b);
        let (_, todo) = &result.todos[0];
        assert_eq!(todo.state, TodoState::Done);
    }

    #[test]
    fn todo_with_due_date() {
        let b = buf("=== 2026-05-12\nTODO: pay the bill due:2026-05-15\n");
        let result = parse(&b);
        let (_, todo) = &result.todos[0];
        assert!(todo.due.is_some());
        assert_eq!(b.span_text(todo.due.unwrap()), "due:2026-05-15");
    }

    #[test]
    fn todo_checkbox_with_due_date() {
        let b = buf("=== 2026-05-12\n- [ ] send proposal due:2026-05-20\n");
        let result = parse(&b);
        let (_, todo) = &result.todos[0];
        assert!(todo.due.is_some());
    }

    #[test]
    fn no_todo_inside_code_fence() {
        let b = buf("=== 2026-05-12\n```\nTODO: this is in a fence\n```\nTODO: real one\n");
        let result = parse(&b);
        assert_eq!(result.todos.len(), 1, "only one todo outside fence");
        assert_eq!(b.span_text(result.todos[0].1.text), "real one");
    }

    #[test]
    fn todo_note_association() {
        let b = buf("=== 2026-05-11\nTODO: first\n\n=== 2026-05-12\nTODO: second\n");
        let result = parse(&b);
        assert_eq!(result.todos.len(), 2);
        assert_eq!(result.todos[0].0, 0, "first todo belongs to note 0");
        assert_eq!(result.todos[1].0, 1, "second todo belongs to note 1");
    }

    // ---- Golden-file test (SPEC §1.3 sample) --------------------------------

    #[test]
    fn spec_sample_file() {
        let text = "\
=== 2026-05-11 | Meeting notes\n\
Lorem ipsum bla bla bla\n\
\n\
## Follow-up items\n\
\n\
TODO: Send proposal to Michael\n\
- [ ] Review OCI deployment docs\n\
- [x] Book the conference room\n\
\n\
#oci #helidon #michael\n\
\n\
=== 2026-05-12 09:14 | Morning standup\n\
Here is another note.\n\
DONE: Verify last night's deploy\n\
#tag1 #tag2\n";

        let b = buf(text);
        let result = parse(&b);

        // Two notes
        assert_eq!(result.notes.len(), 2);

        // Note 0
        assert_eq!(b.span_text(result.notes[0].date.unwrap()), "2026-05-11");
        assert!(result.notes[0].time.is_none());
        assert_eq!(b.span_text(result.notes[0].title.unwrap()), "Meeting notes");

        // Note 1
        assert_eq!(b.span_text(result.notes[1].date.unwrap()), "2026-05-12");
        assert_eq!(b.span_text(result.notes[1].time.unwrap()), "09:14");
        assert_eq!(b.span_text(result.notes[1].title.unwrap()), "Morning standup");

        // Tags: oci, helidon, michael on note 0; tag1, tag2 on note 1
        let note0_tags: Vec<&str> = result.tags.iter()
            .filter(|(ni, _)| *ni == 0)
            .map(|(_, t)| b.span_text(t.name))
            .collect();
        assert!(note0_tags.contains(&"oci"));
        assert!(note0_tags.contains(&"helidon"));
        assert!(note0_tags.contains(&"michael"));

        let note1_tags: Vec<&str> = result.tags.iter()
            .filter(|(ni, _)| *ni == 1)
            .map(|(_, t)| b.span_text(t.name))
            .collect();
        assert!(note1_tags.contains(&"tag1"));
        assert!(note1_tags.contains(&"tag2"));

        // Todos: 3 in note 0 (1 prefix + 2 checkbox), 1 in note 1
        let note0_todos: Vec<_> = result.todos.iter().filter(|(ni, _)| *ni == 0).collect();
        let note1_todos: Vec<_> = result.todos.iter().filter(|(ni, _)| *ni == 1).collect();
        assert_eq!(note0_todos.len(), 3);
        assert_eq!(note1_todos.len(), 1);

        // States: Send proposal + Review OCI = 2 open; Book conference + Verify = 2 done
        let open_count = result.todos.iter().filter(|(_, t)| t.state == TodoState::Open).count();
        let done_count = result.todos.iter().filter(|(_, t)| t.state == TodoState::Done).count();
        assert_eq!(open_count, 2);
        assert_eq!(done_count, 2);
    }
}
