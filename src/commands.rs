use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveTime};
use std::path::Path;

use crate::buffer::TextBuffer;
use crate::date;
use crate::index::FileIndex;
use crate::persistence::{detect_line_ending, read_modify_write, WriteOutcome};
use crate::spans::{NoteSpan, TodoState};

// ---------------------------------------------------------------------------
// --add
// ---------------------------------------------------------------------------

pub fn cmd_note_add(file: &Path, text: &str, tags: Option<&str>) -> Result<()> {
    let outcome = read_modify_write(file, |current| {
        Ok(append_note(current, text, tags))
    })?;
    if outcome == WriteOutcome::ConflictRetryFailed {
        eprintln!("warning: file was modified externally; note may not have been saved");
    }
    Ok(())
}

/// Build the text to append for a new note and return the full new file content.
fn append_note(current: &str, text: &str, tags: Option<&str>) -> String {
    let le = detect_line_ending(current);
    let now = date::now().naive_local();
    let time_str = date::format_datetime(now);

    // Build the header: "=== YYYY-MM-DD HH:MM"
    let header = format!("=== {time_str}");

    // Build tag suffix if any
    let tag_suffix = match tags {
        Some(t) if !t.trim().is_empty() => {
            let tag_line: String = t
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    if s.starts_with('#') {
                        s.to_string()
                    } else {
                        format!("#{s}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            Some(tag_line)
        }
        _ => None,
    };

    let mut out = String::with_capacity(current.len() + 128);

    // Decide whether to emit a blank separator before the new note.
    // Rule: if the file is non-empty and doesn't already end with a blank line,
    // insert one blank line between the previous content and the new delimiter.
    if !current.is_empty() {
        out.push_str(current);
        // Ensure the file ends with exactly one newline before we start the separator.
        if !out.ends_with('\n') {
            out.push_str(le);
        }
        // Check if we need a blank separator line.
        let last_nonempty = current.trim_end();
        if !last_nonempty.is_empty() {
            // If the last non-whitespace line is not already a blank line we add one.
            let trailing_blank = current
                .trim_end_matches(|c: char| c == '\r' || c == '\n')
                .is_empty()
                || {
                    // Count trailing newlines: need at least 2 for a blank line.
                    let stripped = current.trim_end_matches(|c: char| c == '\r' || c == '\n');
                    let trailing = &current[stripped.len()..];
                    // Count line endings in trailing whitespace
                    let newline_count = if le == "\r\n" {
                        trailing.matches("\r\n").count()
                    } else {
                        trailing.matches('\n').count()
                    };
                    newline_count >= 2
                };
            if !trailing_blank {
                out.push_str(le);
            }
        }
    }

    // Emit the delimiter + body
    out.push_str(&header);
    out.push_str(le);
    out.push_str(text);
    out.push_str(le);

    if let Some(tl) = tag_suffix {
        out.push_str(&tl);
        out.push_str(le);
    }

    out
}

// ---------------------------------------------------------------------------
// --todo --add
// ---------------------------------------------------------------------------

pub fn cmd_todo_add(file: &Path, text: &str) -> Result<()> {
    let outcome = read_modify_write(file, |current| {
        Ok(append_todo(current, text))
    })?;
    if outcome == WriteOutcome::ConflictRetryFailed {
        eprintln!("warning: file was modified externally; todo may not have been saved");
    }
    Ok(())
}

fn append_todo(current: &str, text: &str) -> String {
    let le = detect_line_ending(current);
    let now = date::now().naive_local();
    let time_str = date::format_datetime(now);
    let header = format!("=== {time_str}");

    let mut out = String::with_capacity(current.len() + 128);

    if !current.is_empty() {
        out.push_str(current);
        if !out.ends_with('\n') {
            out.push_str(le);
        }
        // Add blank separator if needed (same logic as append_note)
        let stripped = current.trim_end_matches(|c: char| c == '\r' || c == '\n');
        let trailing = &current[stripped.len()..];
        let newline_count = if le == "\r\n" {
            trailing.matches("\r\n").count()
        } else {
            trailing.matches('\n').count()
        };
        if newline_count < 2 {
            out.push_str(le);
        }
    }

    out.push_str(&header);
    out.push_str(le);
    out.push_str(&format!("TODO: {text}"));
    out.push_str(le);
    out
}

// ---------------------------------------------------------------------------
// --filter
// ---------------------------------------------------------------------------

pub fn cmd_filter(file: &Path, query: &str) -> Result<()> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("reading {}", file.display()))?;
    let buffer = TextBuffer::new(content);
    let index = FileIndex::build(&buffer);

    let matches = collect_filter_matches(&buffer, &index, query);

    for ni in &matches {
        let note = &index.notes()[*ni];
        let note_text = buffer.span_text(note.span);
        print!("{note_text}");
        if !note_text.ends_with('\n') {
            println!();
        }
        println!(); // blank line between notes
    }

    if matches.is_empty() {
        eprintln!("no notes matched");
    }

    Ok(())
}

/// Parsed query — combinable AND across fields, OR within tag groups.
#[derive(Debug, Default, Clone)]
pub struct Query {
    /// Each inner Vec is one comma-separated tag group (OR within). Across
    /// groups is AND. Empty means no tag constraint.
    pub tag_groups: Vec<Vec<String>>,
    /// Free-text terms. All must appear in the note body (AND).
    pub texts: Vec<String>,
    /// `title:foo` terms. All must appear in the title (AND, substring).
    pub titles: Vec<String>,
    /// `date:YYYY-MM-DD` or `date:YYYY-MM` — note's date must begin with one
    /// of these prefixes. Empty means no date constraint.
    pub date_prefixes: Vec<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.tag_groups.is_empty()
            && self.texts.is_empty()
            && self.titles.is_empty()
            && self.date_prefixes.is_empty()
    }
}

/// Parse a filter query string into a `Query`.
pub fn parse_filter(query: &str) -> Query {
    let mut q = Query::default();
    for token in query.split_whitespace() {
        if let Some(rest) = token.strip_prefix("tag:") {
            let group: Vec<String> = rest
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            if !group.is_empty() {
                q.tag_groups.push(group);
            }
        } else if let Some(rest) = token.strip_prefix("title:") {
            let t = rest.trim().to_ascii_lowercase();
            if !t.is_empty() {
                q.titles.push(t);
            }
        } else if let Some(rest) = token.strip_prefix("date:") {
            let t = rest.trim().to_string();
            if !t.is_empty() {
                q.date_prefixes.push(t);
            }
        } else {
            q.texts.push(token.to_ascii_lowercase());
        }
    }
    q
}

/// Collect filter matches for `query` and order them by date+time
/// (newest-first), with file position as the tiebreaker.
pub fn collect_filter_matches(buffer: &TextBuffer, index: &FileIndex, query: &str) -> Vec<usize> {
    let q = parse_filter(query);

    let mut matches: Vec<usize> = (0..index.notes().len())
        .filter(|&ni| matches_query(buffer, index, ni, &q))
        .collect();

    matches.sort_by(|&a, &b| {
        let key_a = note_sort_key(buffer, &index.notes()[a]);
        let key_b = note_sort_key(buffer, &index.notes()[b]);
        key_b.cmp(&key_a).then(a.cmp(&b))
    });

    matches
}

/// Sort key for a note: parsed date and time, both optional. `None` sorts
/// below `Some(_)`, so undated notes land at the end under newest-first.
pub fn note_sort_key(
    buffer: &TextBuffer,
    note: &NoteSpan,
) -> (Option<NaiveDate>, Option<NaiveTime>) {
    let date = note
        .date
        .and_then(|s| date::parse_date(buffer.span_text(s)));
    let time = note
        .time
        .and_then(|s| NaiveTime::parse_from_str(buffer.span_text(s), "%H:%M").ok());
    (date, time)
}

fn matches_query(buffer: &TextBuffer, index: &FileIndex, ni: usize, q: &Query) -> bool {
    let note = &index.notes()[ni];

    // Tags — every tag_group must be satisfied by at least one tag on the note.
    if !q.tag_groups.is_empty() {
        let note_tags: Vec<String> = index
            .tags_for_note(ni)
            .map(|ts| buffer.span_text(ts.name).to_ascii_lowercase())
            .collect();
        for group in &q.tag_groups {
            if !group.iter().any(|t| note_tags.iter().any(|nt| nt == t)) {
                return false;
            }
        }
    }

    // Title — every term must appear in the title (case-insensitive substring).
    if !q.titles.is_empty() {
        let title_text = note
            .title
            .map(|s| buffer.span_text(s).to_ascii_lowercase())
            .unwrap_or_default();
        for t in &q.titles {
            if !title_text.contains(t.as_str()) {
                return false;
            }
        }
    }

    // Date — note's date span must start with one of the listed prefixes (OR
    // across prefixes; if any prefix matches, the date filter passes).
    if !q.date_prefixes.is_empty() {
        let date_text = note.date.map(|s| buffer.span_text(s)).unwrap_or("");
        if !q.date_prefixes.iter().any(|p| date_text.starts_with(p)) {
            return false;
        }
    }

    // Free text — every term must appear in the note body.
    if !q.texts.is_empty() {
        let body_text = buffer.span_text(note.body).to_ascii_lowercase();
        for t in &q.texts {
            if !body_text.contains(t.as_str()) {
                return false;
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------
// --todo interactive
// ---------------------------------------------------------------------------

pub fn cmd_todo_interactive(file: &Path, show_all: bool) -> Result<()> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyModifiers},
        execute,
        style::{Color, Print, ResetColor, SetForegroundColor},
        terminal,
    };
    use std::io::{self, Write};

    let mut stdout = io::stdout();

    // Load todos.
    let mut entries = load_todos(file, show_all)?;

    if entries.is_empty() {
        println!("No todos found.");
        return Ok(());
    }

    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let mut selected = 0usize;
    let mut message: Option<String> = None;

    loop {
        // Reload on each render to stay in sync.
        let height = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);

        execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(terminal::ClearType::All))?;

        // Header
        execute!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("Todos"),
            ResetColor,
            Print(if show_all { " (all)" } else { " (open)" }),
            Print("  j/k navigate  Space toggle  + add  q quit\r\n"),
        )?;
        execute!(stdout, Print("─".repeat(60)), Print("\r\n"))?;

        let visible_count = height.saturating_sub(4); // header(2) + status(1) + input(1)
        let start = if entries.len() <= visible_count {
            0
        } else if selected >= visible_count / 2 {
            (selected - visible_count / 2).min(entries.len() - visible_count)
        } else {
            0
        };

        for (i, entry) in entries.iter().enumerate().skip(start).take(visible_count) {
            let is_sel = i == selected;
            let marker = match entry.state {
                TodoState::Done => "[x]",
                TodoState::Open => "[ ]",
            };
            if is_sel {
                execute!(stdout, SetForegroundColor(Color::Yellow))?;
                execute!(stdout, Print("> "))?;
            } else {
                execute!(stdout, Print("  "))?;
            }
            if entry.state == TodoState::Done {
                execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
            }
            execute!(stdout, Print(format!("{marker} {}", entry.text)), ResetColor, Print("\r\n"))?;
        }

        // Status line
        let status = if let Some(ref m) = message {
            m.clone()
        } else {
            format!("{}/{}", selected + 1, entries.len())
        };
        execute!(stdout, Print(format!("\r\n{status}\r\n")))?;
        stdout.flush()?;

        // Event loop
        if let Event::Key(key) = event::read()? {
            message = None;
            match (key.modifiers, key.code) {
                (_, KeyCode::Char('q')) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => break,
                (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
                    if selected + 1 < entries.len() {
                        selected += 1;
                    }
                }
                (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
                    if selected > 0 {
                        selected -= 1;
                    }
                }
                (_, KeyCode::Char(' ')) | (_, KeyCode::Enter) => {
                    let entry = &entries[selected];
                    let outcome = toggle_todo_in_file(file, entry.line_byte_start, entry.state)?;
                    if outcome == WriteOutcome::ConflictRetryFailed {
                        message = Some("warning: conflict — reload and retry".into());
                    }
                    entries = load_todos(file, show_all)?;
                    if selected >= entries.len() && !entries.is_empty() {
                        selected = entries.len() - 1;
                    }
                }
                (_, KeyCode::Char('+')) => {
                    // Inline add: drop to a simple readline at the bottom.
                    execute!(
                        stdout,
                        cursor::MoveTo(0, (height - 1) as u16),
                        terminal::Clear(terminal::ClearType::CurrentLine),
                        Print("Add todo: "),
                        cursor::Show,
                    )?;
                    terminal::disable_raw_mode()?;
                    stdout.flush()?;

                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    terminal::enable_raw_mode()?;
                    execute!(stdout, cursor::Hide)?;

                    let trimmed = input.trim();
                    if !trimmed.is_empty() {
                        let outcome = read_modify_write(file, |cur| Ok(append_todo(cur, trimmed)))?;
                        if outcome == WriteOutcome::ConflictRetryFailed {
                            message = Some("warning: conflict saving new todo".into());
                        }
                        entries = load_todos(file, show_all)?;
                    }
                }
                _ => {}
            }
        }
    }

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// --todo --grep
// ---------------------------------------------------------------------------

pub fn cmd_todo_grep(file: &Path, phrase: &str, show_all: bool) -> Result<()> {
    let entries = load_todos(file, show_all)?;
    let phrase_lower = phrase.to_ascii_lowercase();
    let mut found = false;
    for (i, entry) in entries.iter().enumerate() {
        if entry.text.to_ascii_lowercase().contains(&phrase_lower) {
            let state = if entry.state == TodoState::Done { "x" } else { " " };
            println!("{:>3}. [{}] {}", i + 1, state, entry.text);
            found = true;
        }
    }
    if !found {
        eprintln!("no todos matched");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

struct TodoEntry {
    state: TodoState,
    text: String,
    /// Byte offset of the start of this todo's line in the file.
    line_byte_start: usize,
}

fn load_todos(file: &Path, show_all: bool) -> Result<Vec<TodoEntry>> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("reading {}", file.display()))?;
    let buffer = TextBuffer::new(content);
    let index = FileIndex::build(&buffer);

    let todos: Vec<TodoEntry> = if show_all {
        index
            .todos_all()
            .map(|(_, ts)| TodoEntry {
                state: ts.state,
                text: buffer.span_text(ts.text).to_string(),
                line_byte_start: ts.span.start,
            })
            .collect()
    } else {
        index
            .todos_open()
            .map(|(_, ts)| TodoEntry {
                state: ts.state,
                text: buffer.span_text(ts.text).to_string(),
                line_byte_start: ts.span.start,
            })
            .collect()
    };

    Ok(todos)
}

/// Toggle the todo at `line_byte_start` in `file`.
fn toggle_todo_in_file(
    file: &Path,
    line_byte_start: usize,
    current_state: TodoState,
) -> Result<WriteOutcome> {
    read_modify_write(file, |content| {
        // Locate the line at this byte offset and rewrite it.
        let prefix_end = line_byte_start.min(content.len());
        let rest = &content[prefix_end..];
        let line_end = rest
            .find('\n')
            .map(|i| prefix_end + i + 1)
            .unwrap_or(content.len());
        let line = &content[prefix_end..line_end];
        let toggled = toggle_todo_line(line, current_state);
        Ok(format!(
            "{}{}{}",
            &content[..prefix_end],
            toggled,
            &content[line_end..]
        ))
    })
}

/// Rewrite a single todo line by toggling its state marker.
fn toggle_todo_line(line: &str, current_state: TodoState) -> String {
    match current_state {
        TodoState::Open => {
            if let Some(rest) = line.strip_prefix("TODO:") {
                format!("DONE:{rest}")
            } else if let Some(rest) = line.strip_prefix("- [ ]") {
                format!("- [x]{rest}")
            } else {
                line.to_string()
            }
        }
        TodoState::Done => {
            if let Some(rest) = line.strip_prefix("DONE:") {
                format!("TODO:{rest}")
            } else if let Some(rest) = line.strip_prefix("- [x]") {
                format!("- [ ]{rest}")
            } else if let Some(rest) = line.strip_prefix("- [X]") {
                format!("- [ ]{rest}")
            } else {
                line.to_string()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- append_note --------------------------------------------------------

    #[test]
    fn note_add_to_empty_file() {
        let result = append_note("", "My first note", None);
        assert!(result.contains("=== "), "should have delimiter");
        assert!(result.contains("My first note"));
        assert!(!result.contains("#"), "no tags");
    }

    #[test]
    fn note_add_with_tags() {
        let result = append_note("", "Tagged note", Some("rust,oci"));
        assert!(result.contains("#rust"), "rust tag");
        assert!(result.contains("#oci"), "oci tag");
    }

    #[test]
    fn note_add_with_hash_prefix_tags() {
        let result = append_note("", "Pre-hashed", Some("#rust, #oci"));
        // Should not double the hash
        assert!(!result.contains("##"));
        assert!(result.contains("#rust"));
        assert!(result.contains("#oci"));
    }

    #[test]
    fn note_add_separator_between_notes() {
        let existing = "=== 2026-05-01\nfirst note\n";
        let result = append_note(existing, "second note", None);
        // Should have blank line between first note and new delimiter
        assert!(result.contains("first note\n\n===") || result.contains("first note\r\n\r\n==="),
            "should have blank separator: {:?}", result);
    }

    #[test]
    fn note_add_no_double_blank_separator() {
        let existing = "=== 2026-05-01\nfirst note\n\n";
        let result = append_note(existing, "second note", None);
        // Should NOT insert extra blank line if one already exists
        assert!(
            !result.contains("first note\n\n\n"),
            "should not have triple newline: {:?}", result
        );
    }

    // ---- append_todo --------------------------------------------------------

    #[test]
    fn todo_add_creates_todo_line() {
        let result = append_todo("", "Write tests");
        assert!(result.contains("TODO: Write tests"), "result: {:?}", result);
    }

    // ---- parse_filter -------------------------------------------------------

    #[test]
    fn query_tag_only() {
        let q = parse_filter("tag:rust");
        assert_eq!(q.tag_groups, vec![vec!["rust".to_string()]]);
        assert!(q.texts.is_empty());
        assert!(q.titles.is_empty());
        assert!(q.date_prefixes.is_empty());
    }

    #[test]
    fn query_mixed() {
        let q = parse_filter("tag:oci kubernetes");
        assert_eq!(q.tag_groups, vec![vec!["oci".to_string()]]);
        assert_eq!(q.texts, vec!["kubernetes"]);
    }

    #[test]
    fn query_tag_or_group() {
        let q = parse_filter("tag:oci,helidon");
        assert_eq!(q.tag_groups, vec![vec!["oci".to_string(), "helidon".to_string()]]);
    }

    #[test]
    fn query_tag_and_across_tokens() {
        let q = parse_filter("tag:oci tag:rust");
        assert_eq!(q.tag_groups.len(), 2);
        assert_eq!(q.tag_groups[0], vec!["oci"]);
        assert_eq!(q.tag_groups[1], vec!["rust"]);
    }

    #[test]
    fn query_title_and_date() {
        let q = parse_filter("title:Standup date:2026-05");
        assert_eq!(q.titles, vec!["standup"]);
        assert_eq!(q.date_prefixes, vec!["2026-05"]);
    }

    // ---- toggle_todo_line ---------------------------------------------------

    #[test]
    fn toggle_prefix_open_to_done() {
        assert_eq!(toggle_todo_line("TODO: buy milk\n", TodoState::Open), "DONE: buy milk\n");
    }

    #[test]
    fn toggle_prefix_done_to_open() {
        assert_eq!(toggle_todo_line("DONE: buy milk\n", TodoState::Done), "TODO: buy milk\n");
    }

    #[test]
    fn toggle_checkbox_open_to_done() {
        assert_eq!(toggle_todo_line("- [ ] fix bug\n", TodoState::Open), "- [x] fix bug\n");
    }

    #[test]
    fn toggle_checkbox_done_to_open() {
        assert_eq!(toggle_todo_line("- [x] fix bug\n", TodoState::Done), "- [ ] fix bug\n");
    }

    // ---- filter ordering ----------------------------------------------------

    #[test]
    fn filter_orders_by_date_newest_first() {
        let text = "=== 2026-01-10\nfirst\n\n=== 2026-01-15\nsecond\n\n=== 2026-01-12\nthird\n";
        let buffer = TextBuffer::new(text.to_string());
        let index = FileIndex::build(&buffer);
        let matches = collect_filter_matches(&buffer, &index, "");
        let dates: Vec<&str> = matches
            .iter()
            .map(|&ni| buffer.span_text(index.notes()[ni].date.unwrap()))
            .collect();
        assert_eq!(dates, vec!["2026-01-15", "2026-01-12", "2026-01-10"]);
    }

    #[test]
    fn filter_stable_within_same_date() {
        let text = "=== 2026-01-10 10:00\nA\n\n=== 2026-01-10 10:00\nB\n";
        let buffer = TextBuffer::new(text.to_string());
        let index = FileIndex::build(&buffer);
        let matches = collect_filter_matches(&buffer, &index, "");
        // Identical timestamps: file order (note 0 before note 1) is preserved.
        assert_eq!(matches, vec![0, 1]);
    }

    #[test]
    fn filter_orders_time_within_same_day() {
        let text = "=== 2026-01-10 09:00\nA\n\n=== 2026-01-10 17:30\nB\n";
        let buffer = TextBuffer::new(text.to_string());
        let index = FileIndex::build(&buffer);
        let matches = collect_filter_matches(&buffer, &index, "");
        // Same day, B is later → B comes first.
        assert_eq!(matches, vec![1, 0]);
    }

    #[test]
    fn filter_handles_missing_date() {
        // First note has a malformed delimiter (no date). Second note has a real date.
        let text = "===\nno date\n\n=== 2026-01-15\nwith date\n";
        let buffer = TextBuffer::new(text.to_string());
        let index = FileIndex::build(&buffer);
        let matches = collect_filter_matches(&buffer, &index, "");
        // The dated note sorts first; the undated one is last.
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], 1);
        assert_eq!(matches[1], 0);
    }
}
