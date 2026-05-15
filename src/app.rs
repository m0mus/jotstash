use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

use crate::ai::AiProvider;
use crate::buffer::TextBuffer;
use crate::date;
use crate::persistence::{detect_line_ending, write_atomic, ConflictStatus, FileSnapshot};
use crate::spans::Span as ByteSpan;
use crate::state::{load_state, save_state, CursorRecord};

// ---------------------------------------------------------------------------
// Undo / redo history
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BatchKind {
    Word,       // alphanumeric or _
    Space,      // space or tab
    DeleteBack, // consecutive Backspace presses
    DeleteFwd,  // consecutive Delete presses
    Burst,      // multiple queued events processed in one render cycle (paste)
}

#[derive(Clone)]
struct HistoryEntry {
    content: String,
    cursor_line: usize,
    cursor_byte: usize,
}

struct UndoHistory {
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    last_batch: Option<BatchKind>,
}

impl UndoHistory {
    fn new() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), last_batch: None }
    }

    /// Always start a new undo entry (non-batchable edit, e.g. Enter).
    fn push(&mut self, snap: HistoryEntry) {
        self.undo.push(snap);
        self.trim();
        self.redo.clear();
        self.last_batch = None;
    }

    /// Start a new undo entry only when the batch kind changes.
    fn push_batch(&mut self, snap: HistoryEntry, kind: BatchKind) {
        if self.last_batch.map_or(true, |k| k != kind) {
            self.undo.push(snap);
            self.trim();
            self.redo.clear();
        }
        self.last_batch = Some(kind);
    }

    /// Navigation breaks the current batch so the next edit starts fresh.
    fn break_batch(&mut self) {
        self.last_batch = None;
    }

    fn undo(&mut self, current: HistoryEntry) -> Option<HistoryEntry> {
        let entry = self.undo.pop()?;
        self.redo.push(current);
        self.last_batch = None;
        Some(entry)
    }

    fn redo(&mut self, current: HistoryEntry) -> Option<HistoryEntry> {
        let entry = self.redo.pop()?;
        self.undo.push(current);
        self.last_batch = None;
        Some(entry)
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last_batch = None;
    }

    fn trim(&mut self) {
        const MAX: usize = 1000;
        if self.undo.len() > MAX {
            self.undo.drain(0..self.undo.len() - MAX);
        }
    }
}

// ---------------------------------------------------------------------------
// Todo overlay
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum TodoItemKind {
    Prefix,   // TODO: / DONE:
    Checkbox, // - [ ] / - [x]
}

#[derive(Clone)]
struct TodoItem {
    line: usize,
    text: String,
    is_done: bool,
    kind: TodoItemKind,
}

struct TodoOverlayState {
    items: Vec<TodoItem>,
    selected: usize,
    /// When false only open todos are shown; true shows all.
    show_done: bool,
    /// Saved view state — restored if the user cancels with `Esc`.
    saved_cursor_line: usize,
    saved_cursor_byte: usize,
    saved_scroll: usize,
}

impl TodoOverlayState {
    fn visible<'a>(&'a self) -> Vec<&'a TodoItem> {
        self.items
            .iter()
            .filter(|t| self.show_done || !t.is_done)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Search state
// ---------------------------------------------------------------------------

struct SearchState {
    query: String,
    /// `(line_idx, byte_in_line, match_len_bytes)` for every match in the file.
    matches: Vec<(usize, usize, usize)>,
    /// Index of the "current" (focused) match in `matches`.
    current: usize,
}

impl SearchState {
    fn new() -> Self {
        Self { query: String::new(), matches: Vec::new(), current: 0 }
    }
}

// ---------------------------------------------------------------------------
// Command bar state
// ---------------------------------------------------------------------------

struct CommandBarState {
    input: String,
    /// Index into `App::cmd_history` while browsing with Up/Down; `None` = live input.
    hist_pos: Option<usize>,
    /// Live input saved while browsing history so Down can restore it.
    saved_input: String,
}

impl CommandBarState {
    fn new() -> Self {
        Self { input: String::new(), hist_pos: None, saved_input: String::new() }
    }
}

// ---------------------------------------------------------------------------
// Mode / dialog types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum ConflictKind {
    Changed,
    Missing,
    SyncRebase,
}

enum Mode {
    Normal,
    Conflict(ConflictKind),
    OpenPrompt(String),
    Search(SearchState),
    CommandBar(CommandBarState),
    TodoOverlay(TodoOverlayState),
    AiPanel(AiPanelState),
    SpellDictPrompt(SpellScope),
    SpellDownload(SpellDownloadState),
    SpellCheck(SpellWizardState),
    FilterOverlay(FilterPanelState),
    Help(HelpState),
}

// ---------------------------------------------------------------------------
// Help overlay
// ---------------------------------------------------------------------------

struct HelpState {
    scroll: u16,
}

// ---------------------------------------------------------------------------
// Filter panel (live-picker overlay)
// ---------------------------------------------------------------------------

struct FilterPanelState {
    query: String,
    /// Byte cursor within `query`.
    query_cursor: usize,
    /// Note indices that match, ordered newest-first by date.
    matches: Vec<usize>,
    selected: usize,
    /// Saved view state — restored if the user cancels with `Esc`.
    saved_cursor_line: usize,
    saved_cursor_byte: usize,
    saved_scroll: usize,
}

// ---------------------------------------------------------------------------
// Spell check
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum SpellScope {
    CurrentNote,
    WholeFile,
    Selection(ByteSpan),
}

struct SpellDownloadState {
    scope: SpellScope,
    rx: Option<std::sync::mpsc::Receiver<Result<PathBuf, crate::spell::SpellError>>>,
    spinner_tick: u8,
    error: Option<String>,
}

struct Misspelling {
    span: ByteSpan,
    word: String,
}

impl Misspelling {
    fn clone_for_use(&self) -> Misspelling {
        Misspelling {
            span: self.span,
            word: self.word.clone(),
        }
    }
}

/// Preserve the case pattern of `original` when substituting `suggestion`.
/// Title-case original → Title-case result; UPPER → UPPER; otherwise lower.
fn case_preserving(original: &str, suggestion: &str) -> String {
    if original.is_empty() {
        return suggestion.to_string();
    }
    let all_upper = original.chars().all(|c| !c.is_alphabetic() || c.is_uppercase());
    let first_upper = original
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false);
    if all_upper && original.chars().filter(|c| c.is_alphabetic()).count() > 1 {
        suggestion.to_uppercase()
    } else if first_upper {
        let mut out = String::new();
        let mut chars = suggestion.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
        }
        out.push_str(&chars.collect::<String>().to_lowercase());
        out
    } else {
        suggestion.to_lowercase()
    }
}

struct SpellWizardState {
    scope: ByteSpan,
    current: Option<Misspelling>,
    suggestions: Vec<String>,
    selected: usize,
    session_skipped: std::collections::HashSet<String>,
    /// Total misspellings counted at the start of the session — for progress.
    initial_total: usize,
    /// How many have been processed (fixed / skipped / added).
    processed: usize,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// AI panel
// ---------------------------------------------------------------------------

struct AiPanelState {
    prompt: String,
    prompt_cursor: usize,
    candidate: Option<String>,
    candidate_scroll: usize,
    loading: bool,
    error: Option<String>,
    /// Original note body text — used for Tab toggle and accept-time replacement.
    original: String,
    /// Byte range in the file buffer that this panel will replace on accept.
    body_range: ByteSpan,
    ai_rx: Option<std::sync::mpsc::Receiver<Result<String, crate::ai::AiError>>>,
    showing_original: bool,
    spinner_tick: u8,
}

impl AiPanelState {
    fn new(prompt: String, original: String, body_range: ByteSpan) -> Self {
        let prompt_cursor = prompt.len();
        Self {
            prompt,
            prompt_cursor,
            candidate: None,
            candidate_scroll: 0,
            loading: false,
            error: None,
            original,
            body_range,
            ai_rx: None,
            showing_original: false,
            spinner_tick: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub buffer: TextBuffer,
    pub file: PathBuf,
    pub dirty: bool,
    cursor_line: usize,
    /// Byte offset of cursor within the current line (never past content end).
    cursor_byte: usize,
    /// Desired visual column; preserved across up/down for sticky-column behaviour.
    preferred_col: usize,
    scroll: usize,
    viewport_height: usize,
    viewport_width: usize,
    status_msg: Option<String>,
    /// Snapshot of the file as it existed when last opened/saved — used for
    /// external-change detection before each save.
    snapshot: Option<FileSnapshot>,
    mode: Mode,
    /// True after the first Ctrl+Q with unsaved changes; second Ctrl+Q quits.
    pending_quit: bool,
    history: UndoHistory,
    /// Hash of the buffer content as it exists on disk (last open/save/reload).
    /// Used to detect whether undo brought us back to the clean saved state.
    saved_hash: Option<u64>,
    /// Command-bar history (most-recent-last); persists for the session.
    cmd_history: Vec<String>,
    ai_cfg: crate::config::AiConfig,
    /// `(line, byte)` anchor when a selection is active; `None` when no
    /// selection. Selection range is from this point to the cursor.
    selection_anchor: Option<(usize, usize)>,
    spell_cfg: crate::config::SpellConfig,
    /// Lazily-loaded spell engine. Built on the first successful spell run.
    spell_engine: Option<crate::spell::SpellEngine>,
    line_wrap: bool,
    /// True while draining a burst of queued input events (e.g. a paste).
    /// Insert operations during a burst form a single undo entry.
    in_burst: bool,
    /// Active filter query (or `None`). Matches are re-computed on each use
    /// so they stay current as the buffer is edited.
    active_filter: Option<String>,
    /// GitHub sync state and bookkeeping.
    sync_cfg: crate::config::SyncConfig,
    sync_state: crate::sync::SyncState,
    sync_repo: Option<PathBuf>,
    sync_rx: Option<std::sync::mpsc::Receiver<SyncMsg>>,
    last_idle_pull: std::time::Instant,
}

enum SyncMsg {
    PushDone(crate::sync::PushOutcome),
    PullDone(crate::sync::PullOutcome),
}

impl App {
    pub fn open(path: &Path, cfg: &crate::config::Config) -> Result<Self> {
        // ---- Sync: detect repo, optionally pull (blocking, 5s timeout) ------
        let sync_repo = if cfg.sync.enabled {
            crate::sync::detect_repo(path)
        } else {
            None
        };
        let mut sync_state = crate::sync::SyncState::Disabled;
        if let Some(repo) = sync_repo.as_ref() {
            if cfg.sync.pull_on_open && crate::sync::has_remote(repo) {
                use crate::sync::PullOutcome;
                match crate::sync::pull_with_timeout(repo, std::time::Duration::from_secs(5)) {
                    PullOutcome::UpToDate | PullOutcome::FastForwarded => {
                        sync_state = crate::sync::SyncState::Idle;
                    }
                    PullOutcome::Conflicted => {
                        sync_state = crate::sync::SyncState::Conflict;
                    }
                    PullOutcome::Offline => {
                        sync_state = crate::sync::SyncState::Offline;
                    }
                    PullOutcome::Error(e) => {
                        sync_state = crate::sync::SyncState::Error(e);
                    }
                }
            } else if crate::sync::has_remote(repo) {
                sync_state = crate::sync::SyncState::Idle;
            } else {
                sync_state = crate::sync::SyncState::Disabled;
            }
        }

        let (buffer, snapshot) = if path.exists() {
            let buf = TextBuffer::from_file(path)?;
            let snap = FileSnapshot::capture(path).ok();
            (buf, snap)
        } else {
            (TextBuffer::empty(), None)
        };
        let saved_hash = Some(hash_content(buffer.as_str()));

        // If the working tree has conflict markers, flag it.
        if crate::sync::has_conflict_markers(buffer.as_str())
            && !matches!(sync_state, crate::sync::SyncState::Conflict)
        {
            sync_state = crate::sync::SyncState::Conflict;
        }

        // Restore saved cursor position for this file (best-effort).
        let path_key = path.to_string_lossy().into_owned();
        let state = load_state();
        let (cursor_line, cursor_byte) = state
            .cursor
            .iter()
            .find(|r| r.path == path_key)
            .map(|r| {
                let lc = buffer.line_count();
                let li = r.line.min(lc.saturating_sub(1));
                let cl = line_content_len(buffer.line_text(li).unwrap_or(""));
                (li, r.byte.min(cl))
            })
            .unwrap_or((0, 0));

        let mut app = Self {
            buffer,
            file: path.to_path_buf(),
            dirty: false,
            cursor_line,
            cursor_byte,
            preferred_col: 0,
            scroll: 0,
            viewport_height: 24,
            viewport_width: 80,
            status_msg: None,
            snapshot,
            mode: Mode::Normal,
            pending_quit: false,
            history: UndoHistory::new(),
            saved_hash,
            cmd_history: Vec::new(),
            ai_cfg: cfg.ai.clone(),
            selection_anchor: None,
            spell_cfg: cfg.spell.clone(),
            spell_engine: None,
            line_wrap: cfg.editor.line_wrap,
            in_burst: false,
            active_filter: None,
            sync_cfg: cfg.sync.clone(),
            sync_state,
            sync_repo,
            sync_rx: None,
            last_idle_pull: std::time::Instant::now(),
        };
        app.update_preferred_col();
        app.scroll_to_cursor();
        Ok(app)
    }

    pub fn run(&mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
        // If open() detected a sync conflict, show the overlay immediately.
        if matches!(self.sync_state, crate::sync::SyncState::Conflict) {
            self.mode = Mode::Conflict(ConflictKind::SyncRebase);
        }
        let result = self.event_loop(&mut terminal);
        let _ = execute!(std::io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        ratatui::restore();
        self.save_cursor_state();
        result
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        'outer: loop {
            let size = terminal.size()?;
            self.viewport_height = (size.height as usize).saturating_sub(1);
            self.viewport_width = size.width as usize;

            terminal.draw(|frame| self.render(frame))?;

            let ai_loading = matches!(&self.mode, Mode::AiPanel(s) if s.loading);
            let dict_loading = matches!(&self.mode, Mode::SpellDownload(_));
            let sync_in_flight = self.sync_rx.is_some();
            if ai_loading || dict_loading || sync_in_flight {
                if ai_loading {
                    self.poll_ai_response();
                }
                if dict_loading {
                    self.poll_dict_download();
                }
                if sync_in_flight {
                    self.poll_sync();
                }
                if event::poll(std::time::Duration::from_millis(100))? {
                    let ev = event::read()?;
                    if self.process_event(ev)? {
                        break 'outer;
                    }
                }
                continue;
            }

            // Idle-pull timer: when no sync is in flight and we've been idle
            // for the configured interval, kick off a background pull.
            let idle_interval = self.sync_cfg.idle_interval_duration();
            if idle_interval > std::time::Duration::ZERO
                && self.sync_repo.is_some()
                && matches!(
                    self.sync_state,
                    crate::sync::SyncState::Idle
                        | crate::sync::SyncState::Offline
                        | crate::sync::SyncState::AheadBy(_)
                )
                && self.last_idle_pull.elapsed() >= idle_interval
            {
                self.last_idle_pull = std::time::Instant::now();
                self.spawn_background_pull();
            }

            // Block until the first event.
            let first = event::read()?;
            let was_queued = event::poll(std::time::Duration::from_millis(0))?;
            self.in_burst = was_queued;

            // Accumulator for plain character events arriving in a burst.
            // We process them as one insert_str call instead of N, which
            // makes paste truly instant (one buffer rebuild instead of N).
            let mut accum = String::new();
            let push_to_accum = |first_evt: &Event, accum: &mut String| -> bool {
                if let Some(c) = accumulatable_char(first_evt) {
                    accum.push(c);
                    true
                } else {
                    false
                }
            };

            let first_pushed = was_queued && push_to_accum(&first, &mut accum);
            if !first_pushed {
                if self.process_event(first)? {
                    break 'outer;
                }
            }

            if was_queued {
                while event::poll(std::time::Duration::from_millis(0))? {
                    let ev = event::read()?;
                    if push_to_accum(&ev, &mut accum) {
                        continue;
                    }
                    // Non-character event: flush accumulator first.
                    if !accum.is_empty() {
                        self.insert_str(&accum);
                        accum.clear();
                    }
                    if self.process_event(ev)? {
                        self.in_burst = false;
                        break 'outer;
                    }
                }
                if !accum.is_empty() {
                    self.insert_str(&accum);
                }
            }
            self.in_burst = false;
        }
        Ok(())
    }

    fn process_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                return Ok(self.handle_key(key));
            }
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Paste(text) => self.handle_paste(&text),
            _ => {}
        }
        Ok(false)
    }

    /// Handle a bracketed-paste event delivered by the terminal. Only fires
    /// when the panel is in a mode that should accept text input (Normal).
    /// Other modes route the paste to their own handlers.
    fn handle_paste(&mut self, text: &str) {
        let le = detect_line_ending(self.buffer.as_str());
        let normalised = normalise_line_endings(text, le);
        match &self.mode {
            Mode::Normal => {
                if self.selection_anchor.is_some() {
                    self.replace_selection(&normalised);
                } else {
                    self.insert_at_cursor(&normalised);
                }
            }
            Mode::OpenPrompt(_) | Mode::Search(_) | Mode::CommandBar(_) => {
                // Inject into the prompt-style input buffers (single-line each).
                let single_line: String = normalised
                    .replace(['\r', '\n'], " ")
                    .trim()
                    .to_string();
                match &mut self.mode {
                    Mode::OpenPrompt(input) => input.push_str(&single_line),
                    Mode::Search(s) => {
                        s.query.push_str(&single_line);
                        self.update_search();
                    }
                    Mode::CommandBar(s) => {
                        s.input.push_str(&single_line);
                    }
                    _ => unreachable!(),
                }
            }
            Mode::AiPanel(_) => {
                let single_line: String = normalised
                    .replace(['\r', '\n'], " ")
                    .trim()
                    .to_string();
                if let Mode::AiPanel(ref mut s) = self.mode {
                    s.prompt.insert_str(s.prompt_cursor, &single_line);
                    s.prompt_cursor += single_line.len();
                }
            }
            _ => {
                // Conflict dialog, todo overlay, spell modes — ignore paste.
            }
        }
    }

    fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        // Mouse scroll is the only mouse interaction we react to; the cursor
        // intentionally stays put, like every other scrollable editor.
        let max_scroll = self.buffer.line_count().saturating_sub(1);
        match m.kind {
            MouseEventKind::ScrollDown => {
                self.scroll = (self.scroll + 3).min(max_scroll);
            }
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
            }
            _ => {}
        }
    }

    // -------------------------------------------------------------------------
    // Rendering
    // -------------------------------------------------------------------------

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let (content_area, status_area) = (chunks[0], chunks[1]);

        self.render_text(frame, content_area);
        self.render_status(frame, status_area);

        if let Mode::Conflict(kind) = &self.mode {
            self.render_conflict_dialog(frame, *kind, area);
        }

        if let Mode::TodoOverlay(_) = &self.mode {
            self.render_todo_overlay(frame, area);
        }

        if let Mode::AiPanel(_) = &self.mode {
            self.render_ai_panel(frame, area);
        }

        if let Mode::SpellDictPrompt(_) = &self.mode {
            self.render_spell_dict_prompt(frame, area);
        }
        if let Mode::SpellDownload(_) = &self.mode {
            self.render_spell_download(frame, area);
        }
        if let Mode::SpellCheck(_) = &self.mode {
            self.render_spell_wizard(frame, area);
        }
        if let Mode::FilterOverlay(_) = &self.mode {
            self.render_filter_panel(frame, area);
        }
        if let Mode::Help(_) = &self.mode {
            self.render_help(frame, area);
        }
    }

    fn render_todo_overlay(&self, frame: &mut Frame, area: Rect) {
        let state = match &self.mode {
            Mode::TodoOverlay(s) => s,
            _ => return,
        };
        let visible_items = state.visible();
        let total_open = state.items.iter().filter(|t| !t.is_done).count();
        let total_done = state.items.iter().filter(|t| t.is_done).count();

        // Bottom 2/3 overlay, matching the AI / spell / filter panel pattern.
        let panel_height = (area.height * 2 / 3).max(10);
        let panel_y = area.y + area.height.saturating_sub(panel_height);
        let panel_area = Rect {
            x: area.x,
            y: panel_y,
            width: area.width,
            height: panel_height,
        };
        frame.render_widget(Clear, panel_area);

        let title = format!(" todo · {} open · {} done ", total_open, total_done);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green));
        let inner = block.inner(panel_area);
        frame.render_widget(block, panel_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let (list_area, footer_area) = (chunks[0], chunks[1]);

        if visible_items.is_empty() {
            let msg = if state.items.is_empty() {
                "  No todos in this file."
            } else {
                "  All todos are done. Press `a` to show them."
            };
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
                list_area,
            );
        } else {
            let list_height = list_area.height as usize;
            let scroll = if state.selected >= list_height {
                state.selected + 1 - list_height
            } else {
                0
            };
            let rows: Vec<Line> = visible_items
                .iter()
                .enumerate()
                .skip(scroll)
                .take(list_height)
                .map(|(idx, item)| {
                    let marker = match item.kind {
                        TodoItemKind::Checkbox => {
                            if item.is_done { "[x]" } else { "[ ]" }
                        }
                        TodoItemKind::Prefix => {
                            if item.is_done { "DONE:" } else { "TODO:" }
                        }
                    };
                    let row_text = format!(" {marker} {}", item.text);
                    let is_sel = idx == state.selected;
                    if is_sel {
                        Line::from(Span::styled(
                            row_text,
                            Style::default()
                                .bg(Color::Rgb(60, 80, 110))
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else if item.is_done {
                        Line::from(Span::styled(
                            row_text,
                            Style::default().fg(Color::DarkGray),
                        ))
                    } else {
                        Line::from(Span::raw(row_text))
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(Text::from(rows)), list_area);
        }

        let show_hide = if state.show_done { "hide done" } else { "show done" };
        let footer = format!(
            " \u{2191}\u{2193} pick \u{00b7} Space toggle \u{00b7} a {show_hide} \u{00b7} Enter commit \u{00b7} Esc cancel "
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            )),
            footer_area,
        );
    }

    fn render_text(&self, frame: &mut Frame, area: Rect) {
        let line_count = self.buffer.line_count();
        let visible = area.height as usize;

        // Scan lines before the viewport to determine whether we start inside a
        // fenced code block (``` or ~~~).
        let mut in_fence = false;
        for li in 0..self.scroll.min(line_count) {
            let raw = self.buffer.line_text(li).unwrap_or("");
            let c = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');
            if is_fence_line(c) {
                in_fence = !in_fence;
            }
        }

        // In search mode, build a per-line map of match regions for visible lines.
        let mut search_matches: std::collections::HashMap<usize, Vec<(usize, usize, bool)>> =
            std::collections::HashMap::new();
        if let Mode::Search(ref s) = self.mode {
            for (idx, &(li, by, len)) in s.matches.iter().enumerate() {
                if li >= self.scroll && li < self.scroll + visible {
                    search_matches
                        .entry(li)
                        .or_default()
                        .push((by, len, idx == s.current));
                }
            }
        }

        // Selection ranges per visible line: (start_byte_in_line, end_byte_in_line).
        let mut selection_per_line: std::collections::HashMap<usize, (usize, usize)> =
            std::collections::HashMap::new();
        if let Some(sel) = self.selection_span() {
            let line_idx = self.buffer.line_index();
            let first_line = line_idx.offset_to_line(sel.start);
            let last_line = line_idx.offset_to_line(sel.end);
            for line in first_line..=last_line {
                if line < self.scroll || line >= self.scroll + visible {
                    continue;
                }
                let line_start = line_idx.line_start(line).unwrap_or(0);
                let next_start = line_idx.line_start(line + 1).unwrap_or(self.buffer.as_str().len());
                let lo = sel.start.max(line_start) - line_start;
                let hi = sel.end.min(next_start) - line_start;
                if hi > lo {
                    selection_per_line.insert(line, (lo, hi));
                }
            }
        }

        let wrap_width = area.width as usize;
        let wrap_on = self.line_wrap && wrap_width > 0;

        let mut lines: Vec<Line> = Vec::with_capacity(visible);
        // Track where each visible file line begins, and where the cursor's
        // file line begins, in visual-row indices within `lines`.
        let mut cursor_file_line_visual_row: Option<usize> = None;
        let mut cursor_segment_in_line: usize = 0;
        let mut cursor_segment_start_byte: usize = 0;

        for li in self.scroll..self.scroll + visible {
            if lines.len() >= visible {
                break;
            }
            if li >= line_count {
                lines.push(Line::from(Span::styled(
                    "~",
                    Style::default().fg(Color::DarkGray),
                )));
                continue;
            }
            let raw = self.buffer.line_text(li).unwrap_or("");
            let content = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');

            // Build the fully-styled line for this file line.
            let styled_line: Line<'static> = if is_fence_line(content) {
                in_fence = !in_fence;
                let mut line = Line::from(Span::styled(
                    content.to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
                if let Some(&(s, e)) = selection_per_line.get(&li) {
                    line = overlay_selection(line, s, e);
                }
                line
            } else if in_fence {
                let mut line = Line::from(Span::styled(
                    content.to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
                if let Some(&(s, e)) = selection_per_line.get(&li) {
                    line = overlay_selection(line, s, e);
                }
                line
            } else {
                let is_delim = content == "===" || content.starts_with("=== ");
                let has_search_match = search_matches.contains_key(&li);
                let has_selection = selection_per_line.contains_key(&li);
                let on_cursor = li == self.cursor_line;

                if is_delim && !on_cursor && !has_search_match && !has_selection {
                    // Decorated delimiter — render exactly area.width cells; do
                    // NOT pass through wrap-slicing (its byte layout differs
                    // from the source `content`, so slicing would chop it).
                    let decorated = render_decorated_delimiter(content, area.width as usize);
                    if lines.len() < visible {
                        lines.push(decorated);
                    }
                    continue;
                }

                let mut hl = highlight_normal_line(content);
                if let Some(m) = search_matches.get(&li) {
                    hl = overlay_search_matches(hl, m);
                }
                if let Some(&(s, e)) = selection_per_line.get(&li) {
                    hl = overlay_selection(hl, s, e);
                }
                hl
            };

            // Apply word wrap.
            let segments = if wrap_on {
                wrap_segments(content, wrap_width)
            } else {
                vec![(0, content.len())]
            };

            if li == self.cursor_line {
                cursor_file_line_visual_row = Some(lines.len());
                let mut seg_idx = 0usize;
                for (i, &(s, e)) in segments.iter().enumerate() {
                    if self.cursor_byte >= s && self.cursor_byte <= e {
                        seg_idx = i;
                        break;
                    }
                    if self.cursor_byte > e && i + 1 == segments.len() {
                        seg_idx = i;
                    }
                }
                cursor_segment_in_line = seg_idx;
                cursor_segment_start_byte = segments[seg_idx].0;
            }

            for (s, e) in &segments {
                if lines.len() >= visible {
                    break;
                }
                lines.push(slice_line(&styled_line, *s, *e));
            }
        }

        frame.render_widget(Paragraph::new(Text::from(lines)), area);

        // Terminal cursor — Normal and Search modes; dialogs/prompts reposition it.
        if matches!(self.mode, Mode::Normal | Mode::Search(_)) {
            if let Some(base_row) = cursor_file_line_visual_row {
                let screen_row = base_row + cursor_segment_in_line;
                let max_row = (area.height as usize).saturating_sub(1);
                let display_row = screen_row.min(max_row);
                let raw = self.buffer.line_text(self.cursor_line).unwrap_or("");
                let content = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');
                let seg_end = (cursor_segment_start_byte.saturating_add(content.len()))
                    .min(content.len());
                let local_byte = self.cursor_byte.saturating_sub(cursor_segment_start_byte);
                let local_byte =
                    local_byte.min(seg_end.saturating_sub(cursor_segment_start_byte));
                let vcol = visual_col_for_byte(
                    &content[cursor_segment_start_byte..],
                    local_byte,
                );
                let cx = (area.x as usize + vcol)
                    .min(area.right().saturating_sub(1) as usize) as u16;
                let cy = area.y + display_row as u16;
                frame.set_cursor_position((cx, cy));
            }
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let (text, style, cursor_x) = match &self.mode {
            Mode::OpenPrompt(input) => {
                let prompt = format!(" Open file: {input}");
                let cx = (area.x as usize + prompt.len()).min(area.right() as usize - 1) as u16;
                (
                    prompt,
                    Style::default().bg(Color::DarkGray).fg(Color::White),
                    Some((cx, area.y)),
                )
            }
            Mode::CommandBar(s) => {
                let prompt = format!(":{}", s.input);
                let cx = (area.x as usize + prompt.len()).min(area.right() as usize - 1) as u16;
                let hint = if s.input.is_empty() {
                    "   type :help for shortcuts \u{00b7} Tab to complete"
                } else {
                    ""
                };
                let display = format!("{prompt}{hint}");
                (
                    display,
                    Style::default().bg(Color::DarkGray).fg(Color::White),
                    Some((cx, area.y)),
                )
            }
            Mode::Search(s) => {
                let count_str = if s.matches.is_empty() && !s.query.is_empty() {
                    " (no matches)".to_string()
                } else if s.matches.is_empty() {
                    String::new()
                } else {
                    format!("  {}/{}", s.current + 1, s.matches.len())
                };
                let bg = if !s.query.is_empty() && s.matches.is_empty() {
                    Color::Red
                } else {
                    Color::Blue
                };
                (
                    format!(" Find: {}{count_str}", s.query),
                    Style::default().bg(bg).fg(Color::White),
                    None,
                )
            }
            _ => {
                let filename = self
                    .file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.file.display().to_string());
                let modified = if self.dirty { " [+]" } else { "" };
                let sync_indicator = sync_status_indicator(&self.sync_state);
                let filter_indicator = self
                    .active_filter
                    .as_deref()
                    .map(|q| format!("  │  [filter: {q}]"))
                    .unwrap_or_default();
                let msg = self
                    .status_msg
                    .as_deref()
                    .map(|m| format!("  │  {m}"))
                    .unwrap_or_default();
                let line_count = self.buffer.line_count();
                let s = format!(
                    " {filename}{modified}{sync_indicator}   Ln {}, Col {}   {line_count} lines{filter_indicator}{msg}",
                    self.cursor_line + 1,
                    self.cursor_visual_col() + 1,
                );
                (s, Style::default().bg(Color::DarkGray).fg(Color::White), None)
            }
        };

        frame.render_widget(Paragraph::new(text).style(style), area);

        if let Some((cx, cy)) = cursor_x {
            frame.set_cursor_position((cx, cy));
        }
    }

    fn render_conflict_dialog(&self, frame: &mut Frame, kind: ConflictKind, area: Rect) {
        let dialog = centered_rect(56, 11, area);
        frame.render_widget(Clear, dialog);

        let filename = self
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.file.display().to_string());

        let (title, body) = match kind {
            ConflictKind::Changed => (
                " File Changed Externally ",
                format!(
                    "\n\"{filename}\" was modified on disk\nwhile you were editing it.\n\n\
                     [O] Overwrite disk with your version\n\
                     [R] Reload from disk (discard edits)\n\
                     [C] Cancel (keep editing)"
                ),
            ),
            ConflictKind::Missing => (
                " File Deleted Externally ",
                format!(
                    "\n\"{filename}\" was deleted from disk\nwhile you were editing it.\n\n\
                     [O] Re-create file with your version\n\
                     [R] Discard edits and close buffer\n\
                     [C] Cancel (keep editing)"
                ),
            ),
            ConflictKind::SyncRebase => (
                " Sync Merge Conflict ",
                format!(
                    "\nPulling from remote produced conflicting changes\nin \"{filename}\".\n\n\
                     [K] Keep local — discard remote changes\n\
                     [R] Take remote — discard local changes\n\
                     [E] Edit manually — resolve markers, then :sync\n\
                     [C] Cancel"
                ),
            ),
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(
            Paragraph::new(body)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_ai_panel(&self, frame: &mut Frame, area: Rect) {
        let s = match &self.mode {
            Mode::AiPanel(s) => s,
            _ => return,
        };

        // Panel: bottom half of the screen.
        let panel_height = (area.height / 2).max(10);
        let panel_y = area.y + area.height.saturating_sub(panel_height);
        let panel_area = Rect {
            x: area.x,
            y: panel_y,
            width: area.width,
            height: panel_height,
        };
        frame.render_widget(Clear, panel_area);

        let title = format!(" ai · {} · {} ", self.ai_cfg.provider, self.ai_cfg.model);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta));
        let inner = block.inner(panel_area);
        frame.render_widget(block, panel_area);

        // Split inner into: prompt(2 rows) + separator(1) + body(rest) + footer(1).
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let (prompt_area, sep_area, body_area, footer_area) =
            (chunks[0], chunks[1], chunks[2], chunks[3]);

        // Prompt field.
        let prompt_label = Span::styled("Prompt: ", Style::default().fg(Color::Yellow));
        let prompt_line = Line::from(vec![prompt_label, Span::raw(s.prompt.clone())]);
        frame.render_widget(Paragraph::new(prompt_line).wrap(Wrap { trim: false }), prompt_area);

        // Separator.
        let sep: String = "\u{2500}".repeat(sep_area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(sep, Style::default().fg(Color::DarkGray))),
            sep_area,
        );

        // Body: a small label line + scrollable text (spinner / error /
        // candidate / original). The default view is the original until a
        // candidate exists; after generation, the candidate is shown and Tab
        // toggles back to the original to compare.
        let body_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(body_area);
        let (label_area, text_area) = (body_chunks[0], body_chunks[1]);

        let view_label = if s.loading {
            ""
        } else if s.error.is_some() {
            "[error]"
        } else if s.candidate.is_some() && !s.showing_original {
            "[candidate]"
        } else {
            "[original]"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                view_label,
                Style::default().fg(Color::DarkGray),
            )),
            label_area,
        );

        let body_text: Text = if s.loading {
            let frames = ['\u{2502}', '\u{2571}', '\u{2500}', '\u{2572}'];
            let spin = frames[(s.spinner_tick as usize) % frames.len()];
            Text::from(format!("  {spin}  thinking…"))
        } else if let Some(err) = &s.error {
            Text::from(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            )))
        } else if s.candidate.is_some() && !s.showing_original {
            Text::from(s.candidate.as_deref().unwrap_or(""))
        } else {
            Text::from(s.original.as_str())
        };
        frame.render_widget(
            Paragraph::new(body_text)
                .wrap(Wrap { trim: false })
                .scroll((s.candidate_scroll as u16, 0)),
            text_area,
        );

        // Footer.
        let footer_text = if s.loading {
            " Esc cancel "
        } else if s.candidate.is_some() {
            if s.showing_original {
                " Enter regenerate \u{00b7} Ctrl+Enter accept \u{00b7} Tab show candidate \u{00b7} Esc close "
            } else {
                " Enter regenerate \u{00b7} Ctrl+Enter accept \u{00b7} Tab show original \u{00b7} Esc close "
            }
        } else {
            " Enter generate \u{00b7} Esc close "
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(Color::DarkGray),
            )),
            footer_area,
        );

        // Position the terminal cursor at the end of the prompt input.
        if !s.loading {
            let prompt_x = inner.x + 8 + s.prompt_cursor as u16; // "Prompt: " is 8 chars
            let prompt_y = inner.y;
            if prompt_x < inner.x + inner.width {
                frame.set_cursor_position((prompt_x, prompt_y));
            }
        }
    }

    fn handle_ai_panel_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                let loading = matches!(&self.mode, Mode::AiPanel(s) if s.loading);
                if loading {
                    self.cancel_ai_request();
                } else {
                    self.mode = Mode::Normal;
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Enter) => {
                self.accept_ai_candidate();
            }
            (_, KeyCode::Enter) => {
                self.submit_ai_request();
            }
            (_, KeyCode::Tab) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    if s.candidate.is_some() {
                        s.showing_original = !s.showing_original;
                    }
                }
            }
            (_, KeyCode::Backspace) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    if s.prompt_cursor > 0 {
                        let prev = prev_char_boundary(&s.prompt, s.prompt_cursor);
                        s.prompt.replace_range(prev..s.prompt_cursor, "");
                        s.prompt_cursor = prev;
                    }
                }
            }
            (_, KeyCode::Left) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    if s.prompt_cursor > 0 {
                        s.prompt_cursor = prev_char_boundary(&s.prompt, s.prompt_cursor);
                    }
                }
            }
            (_, KeyCode::Right) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    if s.prompt_cursor < s.prompt.len() {
                        let next = next_char_boundary(&s.prompt, s.prompt_cursor);
                        s.prompt_cursor = next;
                    }
                }
            }
            (_, KeyCode::Home) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    s.prompt_cursor = 0;
                }
            }
            (_, KeyCode::End) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    s.prompt_cursor = s.prompt.len();
                }
            }
            (_, KeyCode::Up) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    s.candidate_scroll = s.candidate_scroll.saturating_sub(1);
                }
            }
            (_, KeyCode::Down) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    s.candidate_scroll = s.candidate_scroll.saturating_add(1);
                }
            }
            (m, KeyCode::Char(ch))
                if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    let mut buf = [0u8; 4];
                    let utf8 = ch.encode_utf8(&mut buf);
                    s.prompt.insert_str(s.prompt_cursor, utf8);
                    s.prompt_cursor += utf8.len();
                }
            }
            _ => {}
        }
        false
    }

    fn open_ai_panel(&mut self) {
        // Selection takes precedence; fall back to the current note's body.
        let body_range = if let Some(sel) = self.selection_span() {
            sel
        } else {
            let index = crate::index::FileIndex::build(&self.buffer);
            let cursor_abs = self.cursor_abs_pos();
            index
                .notes()
                .iter()
                .find(|n| cursor_abs >= n.span.start && cursor_abs <= n.span.end)
                .map(|n| n.body)
                .unwrap_or_else(|| ByteSpan::new(0, self.buffer.as_str().len()))
        };

        let original = self.buffer.as_str()[body_range.start..body_range.end].to_string();

        // Restore last-used prompt.
        let last = load_state().last_ai_prompt.unwrap_or_default();

        self.mode = Mode::AiPanel(AiPanelState::new(last, original, body_range));
    }

    fn submit_ai_request(&mut self) {
        // Capture prompt + input under an immutable borrow, then mutate.
        let (prompt, input) = match &self.mode {
            Mode::AiPanel(s) if !s.loading => (s.prompt.clone(), s.original.clone()),
            _ => return,
        };
        if prompt.trim().is_empty() {
            if let Mode::AiPanel(ref mut s) = self.mode {
                s.error = Some("empty prompt".into());
            }
            return;
        }

        // Persist last prompt.
        let mut st = load_state();
        st.last_ai_prompt = Some(prompt.clone());
        save_state(&st);

        // Build provider; surface config errors immediately.
        let provider = match crate::ai::provider_from_config(&self.ai_cfg) {
            Ok(p) => p,
            Err(e) => {
                if let Mode::AiPanel(ref mut s) = self.mode {
                    s.error = Some(e.to_string());
                }
                return;
            }
        };

        // Spawn worker thread; channel delivers the result back.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = provider.complete(&prompt, &input);
            let _ = tx.send(result);
        });

        if let Mode::AiPanel(ref mut s) = self.mode {
            s.loading = true;
            s.error = None;
            s.candidate = None;
            s.showing_original = false;
            s.ai_rx = Some(rx);
            s.spinner_tick = 0;
            s.candidate_scroll = 0;
        }
    }

    /// Poll the in-flight request without blocking. Called from the event loop
    /// during loading. Returns `true` if the panel state changed (request
    /// finished or spinner ticked).
    fn poll_ai_response(&mut self) -> bool {
        let received = {
            let rx = match &mut self.mode {
                Mode::AiPanel(s) if s.loading => s.ai_rx.as_ref(),
                _ => return false,
            };
            match rx {
                Some(r) => r.try_recv().ok(),
                None => None,
            }
        };
        if let Some(result) = received {
            if let Mode::AiPanel(ref mut s) = self.mode {
                s.loading = false;
                s.ai_rx = None;
                match result {
                    Ok(text) => {
                        s.candidate = Some(text);
                        s.error = None;
                    }
                    Err(e) => {
                        s.error = Some(e.to_string());
                    }
                }
            }
            return true;
        }
        if let Mode::AiPanel(ref mut s) = self.mode {
            if s.loading {
                s.spinner_tick = s.spinner_tick.wrapping_add(1);
            }
        }
        true
    }

    fn cancel_ai_request(&mut self) {
        if let Mode::AiPanel(ref mut s) = self.mode {
            s.loading = false;
            s.ai_rx = None; // drop receiver; worker thread is abandoned
        }
    }

    fn accept_ai_candidate(&mut self) {
        let (candidate, body_range) = match &self.mode {
            Mode::AiPanel(s) if s.candidate.is_some() && !s.loading => {
                (s.candidate.clone().unwrap(), s.body_range)
            }
            _ => return,
        };
        let snap = self.make_snapshot();
        self.history.push(snap);
        self.buffer.replace_span(body_range, &candidate);
        // Place cursor at start of replaced range.
        let new_abs = body_range.start;
        let li = self.buffer.line_index();
        let line = li.offset_to_line(new_abs);
        let line_start = li.line_start(line).unwrap_or(0);
        self.cursor_line = line;
        self.cursor_byte = new_abs.saturating_sub(line_start);
        self.dirty = true;
        self.mode = Mode::Normal;
        self.update_preferred_col();
        self.scroll_to_cursor();
    }

    // -------------------------------------------------------------------------
    // Spell check
    // -------------------------------------------------------------------------

    fn open_spell_check(&mut self, scope: SpellScope) {
        let lang = self.spell_cfg.language.clone();
        let dict_exists = crate::spell::dict_path(&lang)
            .map(|p| p.exists())
            .unwrap_or(false);
        if !dict_exists {
            self.mode = Mode::SpellDictPrompt(scope);
            return;
        }
        // Dict exists; ensure engine is loaded and start the wizard.
        if self.spell_engine.is_none() {
            match crate::spell::SpellEngine::load(&lang) {
                Ok(eng) => self.spell_engine = Some(eng),
                Err(e) => {
                    self.status_msg = Some(format!("spell: {e}"));
                    return;
                }
            }
        }
        self.start_spell_wizard(scope);
    }

    fn start_spell_wizard(&mut self, scope: SpellScope) {
        let span = self.resolve_spell_scope(scope);
        let misspellings = self.find_misspellings_in(span);
        let initial_total = misspellings.len();
        let first = misspellings.into_iter().next();
        let mut state = SpellWizardState {
            scope: span,
            current: first.map(|m| Misspelling {
                span: m.span,
                word: m.text,
            }),
            suggestions: Vec::new(),
            selected: 0,
            session_skipped: std::collections::HashSet::new(),
            initial_total,
            processed: 0,
            error: None,
        };
        if state.current.is_none() {
            self.status_msg = Some(format!(
                "No misspellings found ({} word{} checked)",
                if initial_total == 0 { 0 } else { initial_total },
                if initial_total == 1 { "" } else { "s" },
            ));
            return;
        }
        self.refresh_suggestions(&mut state);
        self.scroll_cursor_to_misspelling(&state);
        self.mode = Mode::SpellCheck(state);
    }

    fn resolve_spell_scope(&self, scope: SpellScope) -> ByteSpan {
        match scope {
            SpellScope::Selection(span) => span,
            SpellScope::WholeFile => ByteSpan::new(0, self.buffer.as_str().len()),
            SpellScope::CurrentNote => {
                let index = crate::index::FileIndex::build(&self.buffer);
                let cursor_abs = self.cursor_abs_pos();
                index
                    .notes()
                    .iter()
                    .find(|n| cursor_abs >= n.span.start && cursor_abs <= n.span.end)
                    .map(|n| n.body)
                    .unwrap_or_else(|| ByteSpan::new(0, self.buffer.as_str().len()))
            }
        }
    }

    fn find_misspellings_in(&self, scope: ByteSpan) -> Vec<crate::spell::WordToken> {
        let engine = match self.spell_engine.as_ref() {
            Some(e) => e,
            None => return Vec::new(),
        };
        let words = crate::spell::tokenize_for_spell(self.buffer.as_str(), scope.start, scope.end);
        words
            .into_iter()
            .filter(|w| !engine.is_correct(&w.text))
            .collect()
    }

    /// Find the first misspelling whose span starts at or after `after`,
    /// skipping any words in the session-skip set.
    fn next_misspelling(
        &self,
        scope: ByteSpan,
        after: usize,
        skipped: &std::collections::HashSet<String>,
    ) -> Option<crate::spell::WordToken> {
        let from = after.max(scope.start);
        let words = self.find_misspellings_in(ByteSpan::new(from, scope.end));
        words
            .into_iter()
            .find(|w| !skipped.contains(&w.text.to_ascii_lowercase()))
    }

    fn refresh_suggestions(&mut self, state: &mut SpellWizardState) {
        state.selected = 0;
        state.suggestions.clear();
        if let (Some(engine), Some(current)) = (self.spell_engine.as_ref(), state.current.as_ref())
        {
            state.suggestions = engine.suggest(&current.word, 5);
        }
    }

    fn scroll_cursor_to_misspelling(&mut self, state: &SpellWizardState) {
        if let Some(m) = &state.current {
            let li = self.buffer.line_index();
            let line = li.offset_to_line(m.span.start);
            let line_start = li.line_start(line).unwrap_or(0);
            self.cursor_line = line;
            self.cursor_byte = m.span.start - line_start;
            self.scroll_to_cursor();
        }
    }

    fn handle_spell_dict_prompt_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let scope = match &self.mode {
            Mode::SpellDictPrompt(s) => *s,
            _ => return false,
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.start_dict_download(scope);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status_msg = Some("Spell check cancelled".into());
            }
            _ => {}
        }
        false
    }

    fn start_dict_download(&mut self, scope: SpellScope) {
        let (tx, rx) = std::sync::mpsc::channel();
        let lang = self.spell_cfg.language.clone();
        std::thread::spawn(move || {
            let result = crate::spell::download_dictionary(&lang);
            let _ = tx.send(result);
        });
        self.mode = Mode::SpellDownload(SpellDownloadState {
            scope,
            rx: Some(rx),
            spinner_tick: 0,
            error: None,
        });
    }

    fn poll_dict_download(&mut self) -> bool {
        let received = {
            let rx = match &mut self.mode {
                Mode::SpellDownload(s) => s.rx.as_ref(),
                _ => return false,
            };
            match rx {
                Some(r) => r.try_recv().ok(),
                None => None,
            }
        };
        if let Some(result) = received {
            match result {
                Ok(_) => {
                    let scope = match &self.mode {
                        Mode::SpellDownload(s) => s.scope,
                        _ => return true,
                    };
                    // Load the engine and proceed.
                    let lang = self.spell_cfg.language.clone();
                    match crate::spell::SpellEngine::load(&lang) {
                        Ok(eng) => {
                            self.spell_engine = Some(eng);
                            self.start_spell_wizard(scope);
                        }
                        Err(e) => {
                            if let Mode::SpellDownload(ref mut s) = self.mode {
                                s.error = Some(e.to_string());
                                s.rx = None;
                            }
                        }
                    }
                }
                Err(e) => {
                    if let Mode::SpellDownload(ref mut s) = self.mode {
                        s.error = Some(e.to_string());
                        s.rx = None;
                    }
                }
            }
            return true;
        }
        if let Mode::SpellDownload(ref mut s) = self.mode {
            s.spinner_tick = s.spinner_tick.wrapping_add(1);
        }
        true
    }

    fn handle_spell_download_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let has_error = matches!(&self.mode, Mode::SpellDownload(s) if s.error.is_some());
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                if !has_error {
                    self.status_msg = Some("Download cancelled".into());
                }
            }
            _ => {}
        }
        false
    }

    fn handle_spell_check_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
            }
            KeyCode::Down => {
                if let Mode::SpellCheck(ref mut s) = self.mode {
                    if !s.suggestions.is_empty() && s.selected + 1 < s.suggestions.len() {
                        s.selected += 1;
                    }
                }
            }
            KeyCode::Up => {
                if let Mode::SpellCheck(ref mut s) = self.mode {
                    if s.selected > 0 {
                        s.selected -= 1;
                    }
                }
            }
            KeyCode::Enter => {
                self.apply_spell_fix(None);
            }
            KeyCode::Char(ch) if ('1'..='5').contains(&ch) => {
                let idx = (ch as u8 - b'1') as usize;
                self.apply_spell_fix(Some(idx));
            }
            KeyCode::Char('s') => {
                self.spell_skip_current();
            }
            KeyCode::Char('a') => {
                self.spell_add_to_dict();
            }
            _ => {}
        }
        false
    }

    fn apply_spell_fix(&mut self, force_index: Option<usize>) {
        let (mis_span, replacement) = {
            let s = match &self.mode {
                Mode::SpellCheck(s) => s,
                _ => return,
            };
            let m = match &s.current {
                Some(m) => m.clone_for_use(),
                None => return,
            };
            let idx = force_index.unwrap_or(s.selected);
            let suggestion = match s.suggestions.get(idx) {
                Some(s) => s.clone(),
                None => return,
            };
            (m.span, case_preserving(&m.word, &suggestion))
        };

        // Apply edit as a single undo entry.
        let snap = self.make_snapshot();
        self.history.push(snap);
        self.buffer.replace_span(mis_span, &replacement);
        self.dirty = true;

        // Adjust scope end for length delta.
        let delta: i64 = replacement.len() as i64 - (mis_span.end - mis_span.start) as i64;
        let advance_to = (mis_span.start as i64 + replacement.len() as i64) as usize;
        self.advance_spell_after_edit(delta, advance_to);
    }

    fn spell_skip_current(&mut self) {
        let (word_lower, advance_to) = {
            let s = match &self.mode {
                Mode::SpellCheck(s) => s,
                _ => return,
            };
            let m = match &s.current {
                Some(m) => m,
                None => return,
            };
            (m.word.to_ascii_lowercase(), m.span.end)
        };
        if let Mode::SpellCheck(ref mut s) = self.mode {
            s.session_skipped.insert(word_lower);
        }
        self.advance_spell_after_edit(0, advance_to);
    }

    fn spell_add_to_dict(&mut self) {
        let (word, advance_to) = {
            let s = match &self.mode {
                Mode::SpellCheck(s) => s,
                _ => return,
            };
            let m = match &s.current {
                Some(m) => m,
                None => return,
            };
            (m.word.clone(), m.span.end)
        };
        if let Some(engine) = self.spell_engine.as_mut() {
            if let Err(e) = engine.add_to_user_dict(&word) {
                if let Mode::SpellCheck(ref mut s) = self.mode {
                    s.error = Some(format!("add failed: {e}"));
                }
                return;
            }
        }
        self.advance_spell_after_edit(0, advance_to);
    }

    /// After a fix/skip/add, expand the scope by `delta` bytes (if a fix shrank
    /// or grew the buffer), find the next misspelling at or after `from`, and
    /// update wizard state. If no more misspellings, close the wizard.
    fn advance_spell_after_edit(&mut self, delta: i64, from: usize) {
        // Apply delta to scope.end.
        if let Mode::SpellCheck(ref mut s) = self.mode {
            let new_end = (s.scope.end as i64 + delta).max(s.scope.start as i64) as usize;
            s.scope = ByteSpan::new(s.scope.start, new_end);
            s.processed += 1;
        }

        let (scope, skipped) = match &self.mode {
            Mode::SpellCheck(s) => (s.scope, s.session_skipped.clone()),
            _ => return,
        };

        let next = self.next_misspelling(scope, from, &skipped);
        if let Mode::SpellCheck(ref mut s) = self.mode {
            match next {
                Some(w) => {
                    s.current = Some(Misspelling {
                        span: w.span,
                        word: w.text,
                    });
                    s.error = None;
                }
                None => {
                    s.current = None;
                }
            }
        }

        // Refresh outside the borrow to call self methods.
        let mut state = std::mem::replace(
            match &mut self.mode {
                Mode::SpellCheck(s) => s,
                _ => return,
            },
            SpellWizardState {
                scope,
                current: None,
                suggestions: Vec::new(),
                selected: 0,
                session_skipped: std::collections::HashSet::new(),
                initial_total: 0,
                processed: 0,
                error: None,
            },
        );

        if state.current.is_some() {
            self.refresh_suggestions(&mut state);
            self.scroll_cursor_to_misspelling(&state);
            self.mode = Mode::SpellCheck(state);
        } else {
            // Done!
            let processed = state.processed;
            self.mode = Mode::Normal;
            self.status_msg = Some(format!(
                "Spell check complete ({} word{} processed)",
                processed,
                if processed == 1 { "" } else { "s" },
            ));
        }
    }

    fn render_spell_dict_prompt(&self, frame: &mut Frame, area: Rect) {
        let dialog = centered_rect(60, 9, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .title(" Spell check ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let body = format!(
            "\nNo English dictionary found.\n\n\
             Download from raw.githubusercontent.com (~1.5 MB)?\n\n\
             [Y]es  [N]o"
        );
        frame.render_widget(
            Paragraph::new(body)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_spell_download(&self, frame: &mut Frame, area: Rect) {
        let s = match &self.mode {
            Mode::SpellDownload(s) => s,
            _ => return,
        };
        let dialog = centered_rect(60, 9, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .title(" Spell check ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let body = if let Some(err) = &s.error {
            format!("\nDownload failed:\n{err}\n\nPress Esc to close")
        } else {
            let frames = ['\u{2502}', '\u{2571}', '\u{2500}', '\u{2572}'];
            let spin = frames[(s.spinner_tick as usize) % frames.len()];
            format!("\n{spin}  Downloading English dictionary…\n\n(Esc to cancel)")
        };
        frame.render_widget(
            Paragraph::new(body)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_spell_wizard(&self, frame: &mut Frame, area: Rect) {
        let s = match &self.mode {
            Mode::SpellCheck(s) => s,
            _ => return,
        };
        let current = match &s.current {
            Some(c) => c,
            None => return,
        };
        let panel_height = (area.height / 2).max(12);
        let panel_y = area.y + area.height.saturating_sub(panel_height);
        let panel_area = Rect {
            x: area.x,
            y: panel_y,
            width: area.width,
            height: panel_height,
        };
        frame.render_widget(Clear, panel_area);

        let title = format!(
            " spell · {} of {} ",
            s.processed + 1,
            s.initial_total.max(s.processed + 1),
        );
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(panel_area);
        frame.render_widget(block, panel_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let (word_area, context_area, sep_area, sugg_area, footer_area) =
            (chunks[0], chunks[1], chunks[2], chunks[3], chunks[4]);

        // Misspelled word.
        let word_line = Line::from(vec![
            Span::styled("Misspelled: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                current.word.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(word_line), word_area);

        // Context line.
        let li = self.buffer.line_index();
        let line_idx = li.offset_to_line(current.span.start);
        let line_start = li.line_start(line_idx).unwrap_or(0);
        let line_text = self
            .buffer
            .line_text(line_idx)
            .unwrap_or("")
            .trim_end_matches(|c: char| c == '\r' || c == '\n');
        let word_in_line_start = current.span.start - line_start;
        let word_in_line_end = current.span.end - line_start;
        let context = Line::from(vec![
            Span::styled("Context: ", Style::default().fg(Color::DarkGray)),
            Span::raw(line_text[..word_in_line_start].to_string()),
            Span::styled(
                line_text[word_in_line_start..word_in_line_end].to_string(),
                Style::default().bg(Color::Rgb(80, 30, 30)).fg(Color::White),
            ),
            Span::raw(line_text[word_in_line_end..].to_string()),
        ]);
        frame.render_widget(
            Paragraph::new(context).wrap(Wrap { trim: false }),
            context_area,
        );

        // Separator.
        let sep: String = "\u{2500}".repeat(sep_area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(sep, Style::default().fg(Color::DarkGray))),
            sep_area,
        );

        // Suggestions.
        let mut lines: Vec<Line> = Vec::new();
        if s.suggestions.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no suggestions)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (i, sg) in s.suggestions.iter().enumerate() {
                let is_sel = i == s.selected;
                let marker = if is_sel { "→ " } else { "  " };
                let style = if is_sel {
                    Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(marker.to_string(), style),
                    Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
                    Span::styled(sg.clone(), style),
                ]));
            }
        }
        if let Some(err) = &s.error {
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            )));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), sugg_area);

        // Footer hints.
        let footer = " \u{2191}\u{2193} pick \u{00b7} Enter/1-5 fix \u{00b7} s skip word \u{00b7} a add to dict \u{00b7} Esc quit ";
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            )),
            footer_area,
        );
    }

    // -------------------------------------------------------------------------
    // Filter panel (live picker)
    // -------------------------------------------------------------------------

    fn open_filter_panel(&mut self) {
        let saved_cursor_line = self.cursor_line;
        let saved_cursor_byte = self.cursor_byte;
        let saved_scroll = self.scroll;
        let query = self.active_filter.clone().unwrap_or_default();
        let query_cursor = query.len();
        let state = FilterPanelState {
            query,
            query_cursor,
            matches: Vec::new(),
            selected: 0,
            saved_cursor_line,
            saved_cursor_byte,
            saved_scroll,
        };
        self.mode = Mode::FilterOverlay(state);
        self.filter_panel_refresh_matches();
    }

    /// Recompute the match list from the current panel's query, reset selection
    /// to 0, mirror the query into `active_filter`, and move the editor cursor
    /// to the first match (if any).
    fn filter_panel_refresh_matches(&mut self) {
        let query = match &self.mode {
            Mode::FilterOverlay(s) => s.query.clone(),
            _ => return,
        };
        let trimmed = query.trim().to_string();
        // Update persistent active filter (None when blank).
        self.active_filter = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.clone())
        };
        let matches = if trimmed.is_empty() {
            Vec::new()
        } else {
            let index = crate::index::FileIndex::build(&self.buffer);
            crate::commands::collect_filter_matches(&self.buffer, &index, &trimmed)
        };
        if let Mode::FilterOverlay(ref mut s) = self.mode {
            s.matches = matches;
            s.selected = 0;
        }
        self.filter_panel_jump_to_selected();
    }

    fn filter_panel_jump_to_selected(&mut self) {
        let target = match &self.mode {
            Mode::FilterOverlay(s) => s.matches.get(s.selected).copied(),
            _ => return,
        };
        if let Some(ni) = target {
            let index = crate::index::FileIndex::build(&self.buffer);
            if let Some(note) = index.notes().get(ni) {
                let abs = note.span.start;
                let li = self.buffer.line_index();
                let line = li.offset_to_line(abs);
                let line_start = li.line_start(line).unwrap_or(0);
                self.cursor_line = line;
                self.cursor_byte = abs.saturating_sub(line_start);
                self.update_preferred_col();
                // The filter panel covers the bottom ~2/3 of the editor area,
                // so `scroll_to_cursor`'s "anywhere in the viewport" rule isn't
                // enough — the cursor can land behind the panel. Force the
                // matched note's first line to the top of the editor area so
                // it's always visible in the uncovered strip above the panel.
                self.scroll = line;
            }
        }
    }

    fn clear_filter(&mut self) {
        let had = self.active_filter.take().is_some();
        self.status_msg = Some(if had {
            "Filter cleared".into()
        } else {
            "No active filter".into()
        });
    }

    /// Jump cursor to the next (forward=true) / previous (forward=false)
    /// match of the active filter.
    fn jump_to_match(&mut self, forward: bool) {
        let query = match self.active_filter.clone() {
            Some(q) => q,
            None => {
                self.status_msg = Some("No active filter".into());
                return;
            }
        };
        let index = crate::index::FileIndex::build(&self.buffer);
        let matches = crate::commands::collect_filter_matches(&self.buffer, &index, &query);
        if matches.is_empty() {
            self.status_msg = Some(format!("[filter: {query}] no matches"));
            return;
        }

        // Order matches by file position for navigation (date-sort is for
        // the overlay; file-order is more intuitive for :next/:prev).
        let mut by_pos = matches.clone();
        by_pos.sort_by_key(|&ni| index.notes()[ni].span.start);

        let cursor_abs = self.cursor_abs_pos();
        let target_ni = if forward {
            by_pos
                .iter()
                .find(|&&ni| index.notes()[ni].span.start > cursor_abs)
                .or_else(|| by_pos.first())
                .copied()
        } else {
            by_pos
                .iter()
                .rev()
                .find(|&&ni| index.notes()[ni].span.start < cursor_abs)
                .or_else(|| by_pos.last())
                .copied()
        };
        if let Some(ni) = target_ni {
            self.jump_cursor_to(index.notes()[ni].span.start);
        }
    }

    fn jump_cursor_to(&mut self, abs: usize) {
        let li = self.buffer.line_index();
        let line = li.offset_to_line(abs);
        let line_start = li.line_start(line).unwrap_or(0);
        self.cursor_line = line;
        self.cursor_byte = abs.saturating_sub(line_start);
        self.update_preferred_col();
        self.scroll_to_cursor();
    }

    fn handle_filter_panel_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                // Cancel: restore the cursor/scroll captured at open-time.
                // Active filter stays (closing the panel doesn't clear it).
                if let Mode::FilterOverlay(s) = &self.mode {
                    let (cl, cb, sc) = (s.saved_cursor_line, s.saved_cursor_byte, s.saved_scroll);
                    self.cursor_line = cl;
                    self.cursor_byte = cb;
                    self.scroll = sc;
                }
                self.mode = Mode::Normal;
                self.update_preferred_col();
            }
            (_, KeyCode::Enter) => {
                // Commit: keep cursor where it is (already at the selection).
                self.mode = Mode::Normal;
                self.update_preferred_col();
            }
            (_, KeyCode::Down) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    if s.selected + 1 < s.matches.len() {
                        s.selected += 1;
                    }
                }
                self.filter_panel_jump_to_selected();
            }
            (_, KeyCode::Up) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    if s.selected > 0 {
                        s.selected -= 1;
                    }
                }
                self.filter_panel_jump_to_selected();
            }
            (_, KeyCode::PageDown) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    let len = s.matches.len();
                    s.selected = (s.selected + 10).min(len.saturating_sub(1));
                }
                self.filter_panel_jump_to_selected();
            }
            (_, KeyCode::PageUp) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    s.selected = s.selected.saturating_sub(10);
                }
                self.filter_panel_jump_to_selected();
            }
            (_, KeyCode::Home) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    s.query_cursor = 0;
                }
            }
            (_, KeyCode::End) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    s.query_cursor = s.query.len();
                }
            }
            (_, KeyCode::Left) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    if s.query_cursor > 0 {
                        s.query_cursor = prev_char_boundary(&s.query, s.query_cursor);
                    }
                }
            }
            (_, KeyCode::Right) => {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    if s.query_cursor < s.query.len() {
                        s.query_cursor = next_char_boundary(&s.query, s.query_cursor);
                    }
                }
            }
            (_, KeyCode::Backspace) => {
                let changed = if let Mode::FilterOverlay(ref mut s) = self.mode {
                    if s.query_cursor > 0 {
                        let prev = prev_char_boundary(&s.query, s.query_cursor);
                        s.query.replace_range(prev..s.query_cursor, "");
                        s.query_cursor = prev;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if changed {
                    self.filter_panel_refresh_matches();
                }
            }
            (m, KeyCode::Char(ch))
                if !m.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if let Mode::FilterOverlay(ref mut s) = self.mode {
                    let mut buf = [0u8; 4];
                    let utf8 = ch.encode_utf8(&mut buf);
                    s.query.insert_str(s.query_cursor, utf8);
                    s.query_cursor += utf8.len();
                }
                self.filter_panel_refresh_matches();
            }
            _ => {}
        }
        false
    }

    fn render_filter_panel(&self, frame: &mut Frame, area: Rect) {
        let s = match &self.mode {
            Mode::FilterOverlay(s) => s,
            _ => return,
        };

        let panel_height = (area.height * 2 / 3).max(12);
        let panel_y = area.y + area.height.saturating_sub(panel_height);
        let panel_area = Rect {
            x: area.x,
            y: panel_y,
            width: area.width,
            height: panel_height,
        };
        frame.render_widget(Clear, panel_area);

        let title = format!(
            " filter · {} match{} ",
            s.matches.len(),
            if s.matches.len() == 1 { "" } else { "es" },
        );
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue));
        let inner = block.inner(panel_area);
        frame.render_widget(block, panel_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let (query_area, sep_area, list_area, footer_area) =
            (chunks[0], chunks[1], chunks[2], chunks[3]);

        // Query input row.
        let query_label = Span::styled("Query: ", Style::default().fg(Color::Yellow));
        let query_line = Line::from(vec![query_label, Span::raw(s.query.clone())]);
        frame.render_widget(Paragraph::new(query_line), query_area);

        // Separator.
        let sep: String = "\u{2500}".repeat(sep_area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(sep, Style::default().fg(Color::DarkGray))),
            sep_area,
        );

        // Match list (scroll window so selected stays visible).
        let visible = list_area.height as usize;
        let scroll = if s.matches.is_empty() {
            0
        } else if s.selected >= visible {
            (s.selected + 1).saturating_sub(visible)
        } else {
            0
        };

        if s.matches.is_empty() {
            let msg = if s.query.trim().is_empty() {
                "Type a query (e.g. tag:oci, title:standup, date:2026-05)"
            } else {
                "No matches"
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                )),
                list_area,
            );
        } else {
            let index = crate::index::FileIndex::build(&self.buffer);
            let mut lines: Vec<Line> = Vec::new();
            for (row_idx, &ni) in s.matches.iter().enumerate().skip(scroll).take(visible) {
                let is_sel = row_idx == s.selected;
                let note = match index.notes().get(ni) {
                    Some(n) => n,
                    None => continue,
                };
                let date_text = note
                    .date
                    .map(|sp| self.buffer.span_text(sp).to_string())
                    .unwrap_or_else(|| "          ".into());
                let title_text = note
                    .title
                    .map(|sp| self.buffer.span_text(sp).to_string())
                    .unwrap_or_default();

                let mut tag_text = String::new();
                for t in index.tags_for_note(ni) {
                    if !tag_text.is_empty() {
                        tag_text.push(' ');
                    }
                    tag_text.push('#');
                    tag_text.push_str(self.buffer.span_text(t.name));
                }

                let body = self.buffer.span_text(note.body);
                let snippet: String = body
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();

                let marker = if is_sel { "\u{25b8} " } else { "  " };
                let base = if is_sel {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(60, 80, 110))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let mut spans = vec![
                    Span::styled(marker.to_string(), base),
                    Span::styled(date_text, base.fg(Color::Cyan)),
                    Span::styled("  ".to_string(), base),
                ];
                if !title_text.is_empty() {
                    spans.push(Span::styled(
                        title_text,
                        base.add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled("  ".to_string(), base));
                }
                if !tag_text.is_empty() {
                    spans.push(Span::styled(tag_text, base.fg(Color::LightCyan)));
                    spans.push(Span::styled("  ".to_string(), base));
                }
                if !snippet.is_empty() {
                    spans.push(Span::styled(
                        snippet,
                        base.fg(Color::Gray).add_modifier(Modifier::DIM),
                    ));
                }
                lines.push(Line::from(spans));
            }
            frame.render_widget(Paragraph::new(Text::from(lines)), list_area);
        }

        let footer =
            " \u{2191}\u{2193} pick \u{00b7} Enter commit \u{00b7} Esc cancel \u{00b7} edit query above ";
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            )),
            footer_area,
        );

        // Terminal cursor in the query input.
        let cx_offset = "Query: ".chars().count() + s.query[..s.query_cursor].chars().count();
        let cx = (query_area.x as usize + cx_offset)
            .min(query_area.right().saturating_sub(1) as usize) as u16;
        frame.set_cursor_position((cx, query_area.y));
    }

    // -------------------------------------------------------------------------
    // Help overlay
    // -------------------------------------------------------------------------

    fn open_help(&mut self) {
        self.mode = Mode::Help(HelpState { scroll: 0 });
    }

    fn handle_help_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let max_scroll = self.help_max_scroll();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) => {
                self.mode = Mode::Normal;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Mode::Help(ref mut s) = self.mode {
                    s.scroll = s.scroll.saturating_add(1).min(max_scroll);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Mode::Help(ref mut s) = self.mode {
                    s.scroll = s.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                if let Mode::Help(ref mut s) = self.mode {
                    s.scroll = s.scroll.saturating_add(10).min(max_scroll);
                }
            }
            KeyCode::PageUp => {
                if let Mode::Help(ref mut s) = self.mode {
                    s.scroll = s.scroll.saturating_sub(10);
                }
            }
            KeyCode::Home => {
                if let Mode::Help(ref mut s) = self.mode {
                    s.scroll = 0;
                }
            }
            KeyCode::End => {
                if let Mode::Help(ref mut s) = self.mode {
                    s.scroll = max_scroll;
                }
            }
            _ => {}
        }
        false
    }

    /// Upper bound for `HelpState::scroll` — the number of content lines
    /// that fall beyond the visible body area.
    fn help_max_scroll(&self) -> u16 {
        let total = help_content().len();
        // Mirror the layout in `render_help`: panel is 2/3 of full terminal
        // height (min 12); inner = panel - 2 (borders); body = inner - 1
        // (footer row).
        let term_h = self.viewport_height.saturating_add(1);
        let panel_h = (term_h * 2 / 3).max(12);
        let body_h = panel_h.saturating_sub(3);
        total.saturating_sub(body_h) as u16
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let s = match &self.mode {
            Mode::Help(s) => s,
            _ => return,
        };

        let panel_height = (area.height * 2 / 3).max(12);
        let panel_y = area.y + area.height.saturating_sub(panel_height);
        let panel_area = Rect {
            x: area.x,
            y: panel_y,
            width: area.width,
            height: panel_height,
        };
        frame.render_widget(Clear, panel_area);

        let block = Block::default()
            .title(" help \u{00b7} shortcuts & commands ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray));
        let inner = block.inner(panel_area);
        frame.render_widget(block, panel_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let (body_area, footer_area) = (chunks[0], chunks[1]);

        let lines = help_content();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).scroll((s.scroll, 0)),
            body_area,
        );

        let footer =
            " \u{2191}\u{2193} scroll \u{00b7} PgUp/PgDn page \u{00b7} Esc / F1 / q close ";
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            )),
            footer_area,
        );
    }

    // -------------------------------------------------------------------------
    // Key handling — dispatches to mode-specific handlers
    // -------------------------------------------------------------------------

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match &self.mode {
            Mode::Conflict(_) => return self.handle_conflict_key(key),
            Mode::OpenPrompt(_) => return self.handle_open_prompt_key(key),
            Mode::Search(_) => return self.handle_search_key(key),
            Mode::CommandBar(_) => return self.handle_command_bar_key(key),
            Mode::TodoOverlay(_) => return self.handle_todo_overlay_key(key),
            Mode::AiPanel(_) => return self.handle_ai_panel_key(key),
            Mode::SpellDictPrompt(_) => return self.handle_spell_dict_prompt_key(key),
            Mode::SpellDownload(_) => return self.handle_spell_download_key(key),
            Mode::SpellCheck(_) => return self.handle_spell_check_key(key),
            Mode::FilterOverlay(_) => return self.handle_filter_panel_key(key),
            Mode::Help(_) => return self.handle_help_key(key),
            Mode::Normal => {}
        }
        self.handle_normal_key(key)
    }

    fn handle_normal_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let is_ctrl_q = key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('q');
        if !is_ctrl_q {
            self.status_msg = None;
            self.pending_quit = false;
        }

        // Selection anchor management: Shift+movement extends; plain movement
        // clears; Esc clears.
        let is_movement = matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        );
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if is_movement {
            if shift {
                if self.selection_anchor.is_none() {
                    self.selection_anchor = Some((self.cursor_line, self.cursor_byte));
                }
            } else {
                self.selection_anchor = None;
            }
        } else if key.code == KeyCode::Esc {
            self.selection_anchor = None;
        } else if matches!(
            key.code,
            KeyCode::Char('j') | KeyCode::Char('k')
        ) && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Note navigation jumps clear selection.
            self.selection_anchor = None;
        }

        match (key.modifiers, key.code) {
            // ---- application control ----
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                if self.dirty && !self.pending_quit {
                    self.pending_quit = true;
                    self.status_msg = Some(
                        "Unsaved changes — Ctrl+S to save, Ctrl+Q again to discard".into(),
                    );
                    return false;
                }
                return true;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('s')) => self.save(),
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.mode = Mode::OpenPrompt(String::new());
            }
            (KeyModifiers::CONTROL, KeyCode::Char('z')) => self.do_undo(),
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => self.do_redo(),
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.mode = Mode::Search(SearchState::new());
            }
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.open_ai_panel();
            }
            (_, KeyCode::F(7)) => {
                let scope = self
                    .selection_span()
                    .map(SpellScope::Selection)
                    .unwrap_or(SpellScope::CurrentNote);
                self.open_spell_check(scope);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.copy_selection(),
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => self.cut_selection(),
            (KeyModifiers::CONTROL, KeyCode::Char('v')) => self.paste_clipboard(),
            (KeyModifiers::CONTROL, KeyCode::Char(';'))
            | (_, KeyCode::F(10)) => {
                self.mode = Mode::CommandBar(CommandBarState::new());
            }
            (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                self.history.break_batch();
                if self.active_filter.is_some() {
                    self.jump_to_match(true);
                } else {
                    match self.next_note_delimiter() {
                        Some(li) => {
                            self.cursor_line = li;
                            self.cursor_byte = 0;
                            self.update_preferred_col();
                            self.scroll_to_cursor();
                        }
                        None => {
                            self.status_msg = Some("No next note".into());
                        }
                    }
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.history.break_batch();
                if self.active_filter.is_some() {
                    self.jump_to_match(false);
                } else {
                    match self.prev_note_delimiter() {
                        Some(li) => {
                            self.cursor_line = li;
                            self.cursor_byte = 0;
                            self.update_preferred_col();
                            self.scroll_to_cursor();
                        }
                        None => {
                            self.status_msg = Some("No previous note".into());
                        }
                    }
                }
            }
            // Filter shortcuts:
            //   Ctrl+P → open the live filter panel
            //   F4 / Shift+F4 → jump to next / previous match (when a filter
            //   is already active and the panel is closed)
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.open_filter_panel();
            }
            (_, KeyCode::F(8)) => {
                self.open_todo_overlay();
            }
            (_, KeyCode::F(1)) => {
                self.open_help();
            }
            (m, KeyCode::F(4)) => {
                self.history.break_batch();
                if m.contains(KeyModifiers::SHIFT) {
                    self.jump_to_match(false);
                } else {
                    self.jump_to_match(true);
                }
            }

            // ---- navigation ----
            (_, KeyCode::Down) => { self.history.break_batch(); self.move_down(1); }
            (_, KeyCode::Up) => { self.history.break_batch(); self.move_up(1); }
            (_, KeyCode::Left) => {
                self.history.break_batch();
                self.move_left();
                self.update_preferred_col();
            }
            (_, KeyCode::Right) => {
                self.history.break_batch();
                self.move_right();
                self.update_preferred_col();
            }
            (_, KeyCode::PageDown) => {
                self.history.break_batch();
                self.move_down(self.viewport_height.saturating_sub(2));
            }
            (_, KeyCode::PageUp) => {
                self.history.break_batch();
                self.move_up(self.viewport_height.saturating_sub(2));
            }
            (_, KeyCode::Home) => {
                self.history.break_batch();
                self.cursor_byte = 0;
                self.update_preferred_col();
            }
            (_, KeyCode::End) => {
                self.history.break_batch();
                let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
                self.cursor_byte = line_content_len(line);
                self.update_preferred_col();
            }
            (KeyModifiers::CONTROL, KeyCode::Home) => {
                self.history.break_batch();
                self.cursor_line = 0;
                self.cursor_byte = 0;
                self.scroll = 0;
                self.update_preferred_col();
            }
            (KeyModifiers::CONTROL, KeyCode::End) => {
                self.history.break_batch();
                self.cursor_line = self.buffer.line_count().saturating_sub(1);
                self.cursor_byte = 0;
                self.scroll_to_cursor();
                self.update_preferred_col();
            }

            // ---- editing ----
            (_, KeyCode::Enter) => {
                if !self.try_expand_delimiter() {
                    self.insert_newline();
                }
            }
            (_, KeyCode::Backspace) => self.delete_backward(),
            (_, KeyCode::Delete) => self.delete_forward(),
            (_, KeyCode::Tab) => self.insert_str("    "),
            (m, KeyCode::Char(ch))
                if !m.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.insert_char(ch);
            }

            _ => {}
        }
        false
    }

    fn handle_conflict_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let kind = match &self.mode {
            Mode::Conflict(k) => *k,
            _ => unreachable!(),
        };
        match (kind, key.code) {
            // Sync conflict — three resolution choices plus cancel.
            (ConflictKind::SyncRebase, KeyCode::Char('k') | KeyCode::Char('K')) => {
                self.mode = Mode::Normal;
                self.resolve_sync_conflict(true);
            }
            (ConflictKind::SyncRebase, KeyCode::Char('r') | KeyCode::Char('R')) => {
                self.mode = Mode::Normal;
                self.resolve_sync_conflict(false);
            }
            (ConflictKind::SyncRebase, KeyCode::Char('e') | KeyCode::Char('E')) => {
                // Close the overlay and route the user to manual editing.
                self.mode = Mode::Normal;
                self.status_msg =
                    Some("Resolve conflict markers in the editor, then run :sync".into());
                // Force-reload the buffer so the conflict markers are visible.
                self.reload();
            }
            (ConflictKind::SyncRebase, KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc) => {
                self.mode = Mode::Normal;
            }

            // Existing external-change conflict.
            (_, KeyCode::Char('o') | KeyCode::Char('O')) => {
                self.mode = Mode::Normal;
                self.force_save();
            }
            (_, KeyCode::Char('r') | KeyCode::Char('R')) => {
                self.mode = Mode::Normal;
                match kind {
                    ConflictKind::Changed => self.reload(),
                    ConflictKind::Missing => {
                        // Nothing useful to reload — just clear the dirty flag
                        // and let the user keep editing.
                        self.dirty = false;
                        self.snapshot = None;
                        self.status_msg = Some(
                            "File was deleted; save again to re-create it".into(),
                        );
                    }
                    ConflictKind::SyncRebase => unreachable!(),
                }
            }
            (_, KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc) => {
                self.mode = Mode::Normal;
                self.status_msg = Some("Save cancelled".into());
            }
            _ => {}
        }
        false
    }

    fn handle_open_prompt_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status_msg = Some("Open cancelled".into());
            }
            KeyCode::Backspace => {
                if let Mode::OpenPrompt(ref mut input) = self.mode {
                    input.pop();
                }
            }
            KeyCode::Enter => {
                let path = if let Mode::OpenPrompt(ref input) = self.mode {
                    PathBuf::from(input.trim())
                } else {
                    unreachable!()
                };
                self.mode = Mode::Normal;
                if path.as_os_str().is_empty() {
                    self.status_msg = Some("Open cancelled".into());
                } else {
                    self.open_file(path);
                }
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Mode::OpenPrompt(ref mut input) = self.mode {
                    input.push(ch);
                }
            }
            _ => {}
        }
        false
    }

    fn handle_search_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
            }
            KeyCode::Enter | KeyCode::F(3) => {
                self.advance_search(!shift);
            }
            KeyCode::Backspace => {
                if let Mode::Search(ref mut s) = self.mode {
                    s.query.pop();
                }
                self.update_search();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Mode::Search(ref mut s) = self.mode {
                    s.query.push(ch);
                }
                self.update_search();
            }
            _ => {}
        }
        false
    }

    fn update_search(&mut self) {
        let query = match &self.mode {
            Mode::Search(s) => s.query.clone(),
            _ => return,
        };
        let matches = compute_matches(&self.buffer, &query);
        let current = find_nearest_match(&matches, self.cursor_line, self.cursor_byte);
        if let Mode::Search(ref mut s) = self.mode {
            s.matches = matches;
            s.current = current;
        }
        let jump = if let Mode::Search(ref s) = self.mode {
            s.matches.get(s.current).copied()
        } else {
            None
        };
        if let Some((li, by, _)) = jump {
            self.cursor_line = li;
            self.cursor_byte = by;
            self.update_preferred_col();
            self.scroll_to_cursor();
        }
    }

    fn advance_search(&mut self, forward: bool) {
        let next = match &self.mode {
            Mode::Search(s) if !s.matches.is_empty() => {
                let total = s.matches.len();
                if forward {
                    (s.current + 1) % total
                } else {
                    (s.current + total - 1) % total
                }
            }
            _ => return,
        };
        if let Mode::Search(ref mut s) = self.mode {
            s.current = next;
        }
        let jump = if let Mode::Search(ref s) = self.mode {
            s.matches.get(next).copied()
        } else {
            None
        };
        if let Some((li, by, _)) = jump {
            self.cursor_line = li;
            self.cursor_byte = by;
            self.update_preferred_col();
            self.scroll_to_cursor();
        }
    }

    fn handle_command_bar_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status_msg = Some("Cancelled".into());
            }
            KeyCode::Enter => {
                let input = match &self.mode {
                    Mode::CommandBar(s) => s.input.trim().to_string(),
                    _ => return false,
                };
                self.mode = Mode::Normal;
                if !input.is_empty() {
                    if self.cmd_history.last().map(String::as_str) != Some(input.as_str()) {
                        self.cmd_history.push(input.clone());
                    }
                    return self.execute_command(&input);
                }
            }
            KeyCode::Backspace => {
                if let Mode::CommandBar(ref mut s) = self.mode {
                    s.hist_pos = None;
                    s.input.pop();
                }
            }
            KeyCode::Tab => {
                // Completion applies only to the verb (before any space).
                let cur = match &self.mode {
                    Mode::CommandBar(s) => s.input.clone(),
                    _ => return false,
                };
                if !cur.contains(' ') {
                    if let Some(completion) = complete_command_verb(&cur) {
                        if let Mode::CommandBar(ref mut s) = self.mode {
                            s.input = completion;
                            s.hist_pos = None;
                        }
                    }
                }
            }
            KeyCode::Up => {
                let hist_len = self.cmd_history.len();
                if hist_len == 0 {
                    return false;
                }
                let (cur_pos, cur_input) = match &self.mode {
                    Mode::CommandBar(s) => (s.hist_pos, s.input.clone()),
                    _ => return false,
                };
                let target = match cur_pos {
                    None => hist_len - 1,
                    Some(i) if i > 0 => i - 1,
                    _ => return false,
                };
                let new_input = self.cmd_history[target].clone();
                if let Mode::CommandBar(ref mut s) = self.mode {
                    if s.hist_pos.is_none() {
                        s.saved_input = cur_input;
                    }
                    s.hist_pos = Some(target);
                    s.input = new_input;
                }
            }
            KeyCode::Down => {
                let hist_len = self.cmd_history.len();
                let (cur_pos, saved) = match &self.mode {
                    Mode::CommandBar(s) => (s.hist_pos, s.saved_input.clone()),
                    _ => return false,
                };
                if let Some(i) = cur_pos {
                    if i + 1 < hist_len {
                        let new_input = self.cmd_history[i + 1].clone();
                        if let Mode::CommandBar(ref mut s) = self.mode {
                            s.hist_pos = Some(i + 1);
                            s.input = new_input;
                        }
                    } else {
                        if let Mode::CommandBar(ref mut s) = self.mode {
                            s.input = saved;
                            s.hist_pos = None;
                        }
                    }
                }
            }
            KeyCode::Char(ch)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Mode::CommandBar(ref mut s) = self.mode {
                    s.hist_pos = None;
                    s.input.push(ch);
                }
            }
            _ => {}
        }
        false
    }

    fn open_todo_overlay(&mut self) {
        let items = collect_todos(&self.buffer);
        let state = TodoOverlayState {
            items,
            selected: 0,
            show_done: false,
            saved_cursor_line: self.cursor_line,
            saved_cursor_byte: self.cursor_byte,
            saved_scroll: self.scroll,
        };
        self.mode = Mode::TodoOverlay(state);
        self.todo_overlay_jump_to_selected();
    }

    /// Move the editor cursor to the line of the currently-selected todo,
    /// and set `scroll` so that line is at the top of the editor area
    /// (so it's visible above the panel that covers the bottom ~2/3).
    fn todo_overlay_jump_to_selected(&mut self) {
        let target_line = match &self.mode {
            Mode::TodoOverlay(s) => s.visible().get(s.selected).map(|t| t.line),
            _ => return,
        };
        if let Some(line) = target_line {
            self.cursor_line = line;
            self.cursor_byte = 0;
            self.update_preferred_col();
            self.scroll = line;
        }
    }

    fn handle_todo_overlay_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Cancel: restore cursor + scroll to where they were before
                // the overlay opened.
                if let Mode::TodoOverlay(s) = &self.mode {
                    let (cl, cb, sc) = (s.saved_cursor_line, s.saved_cursor_byte, s.saved_scroll);
                    self.cursor_line = cl;
                    self.cursor_byte = cb;
                    self.scroll = sc;
                }
                self.mode = Mode::Normal;
                self.update_preferred_col();
            }
            KeyCode::Enter => {
                // Commit: cursor stays where it is (already at selection).
                self.mode = Mode::Normal;
                self.update_preferred_col();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Mode::TodoOverlay(ref mut s) = self.mode {
                    let count = s.visible().len();
                    if count > 0 {
                        s.selected = (s.selected + 1).min(count - 1);
                    }
                }
                self.todo_overlay_jump_to_selected();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Mode::TodoOverlay(ref mut s) = self.mode {
                    s.selected = s.selected.saturating_sub(1);
                }
                self.todo_overlay_jump_to_selected();
            }
            KeyCode::Char('a') => {
                if let Mode::TodoOverlay(ref mut s) = self.mode {
                    s.show_done = !s.show_done;
                    let count = s.visible().len();
                    if count > 0 && s.selected >= count {
                        s.selected = count - 1;
                    }
                }
                self.todo_overlay_jump_to_selected();
            }
            KeyCode::Char(' ') => {
                // Toggle in place — do not advance, do not close.
                let item = match &self.mode {
                    Mode::TodoOverlay(s) => s.visible().get(s.selected).copied().cloned(),
                    _ => None,
                };
                if let Some(item) = item {
                    self.toggle_todo_item(&item);
                    // Rebuild the list so line numbers stay correct after the edit.
                    let new_items = collect_todos(&self.buffer);
                    let (cur_sel, cur_show) = match &self.mode {
                        Mode::TodoOverlay(s) => (s.selected, s.show_done),
                        _ => (0, false),
                    };
                    if let Mode::TodoOverlay(ref mut s) = self.mode {
                        s.items = new_items;
                        s.show_done = cur_show;
                        let count = s.visible().len();
                        if count > 0 && cur_sel >= count {
                            s.selected = count - 1;
                        } else {
                            s.selected = cur_sel;
                        }
                    }
                    self.todo_overlay_jump_to_selected();
                }
            }
            _ => {}
        }
        false
    }

    fn toggle_todo_item(&mut self, item: &TodoItem) {
        let snap = self.make_snapshot();
        self.history.push(snap);

        let line_start = self.buffer.line_index().line_start(item.line).unwrap_or(0);

        let (old_prefix, new_prefix): (&str, &str) = match item.kind {
            TodoItemKind::Prefix => {
                if item.is_done { ("DONE:", "TODO:") } else { ("TODO:", "DONE:") }
            }
            TodoItemKind::Checkbox => {
                if item.is_done { ("- [x]", "- [ ]") } else { ("- [ ]", "- [x]") }
            }
        };

        // Handle the uppercase variant of done checkboxes.
        let raw = self.buffer.line_text(item.line).unwrap_or("");
        let actual_prefix = if item.kind == TodoItemKind::Checkbox
            && item.is_done
            && raw.starts_with("- [X]")
        {
            "- [X]"
        } else {
            old_prefix
        };

        self.buffer.replace_span(
            ByteSpan::new(line_start, line_start + actual_prefix.len()),
            new_prefix,
        );
        self.dirty = true;
    }

    /// Parse and execute a command-bar command.  Returns `true` if the app
    /// should exit.
    fn execute_command(&mut self, cmd: &str) -> bool {
        let (verb, arg) = cmd
            .split_once(' ')
            .map(|(v, a)| (v, a.trim()))
            .unwrap_or((cmd, ""));

        match verb {
            "w" | "write" => {
                self.save();
                false
            }
            "q" | "quit" => {
                if self.dirty {
                    self.status_msg = Some(
                        "Unsaved changes — :w to save, :q! to discard, :wq to save+quit".into(),
                    );
                    false
                } else {
                    true
                }
            }
            "q!" | "quit!" => true,
            "wq" | "x" => {
                self.save();
                !self.dirty
            }
            "goto" | "go" => {
                match arg.parse::<usize>() {
                    Ok(n) if n >= 1 => {
                        let lc = self.buffer.line_count();
                        self.cursor_line = (n - 1).min(lc.saturating_sub(1));
                        self.cursor_byte = 0;
                        self.update_preferred_col();
                        self.scroll_to_cursor();
                        self.status_msg = Some(format!("Line {n}"));
                    }
                    _ => {
                        self.status_msg = Some(if arg.is_empty() {
                            "Usage: :goto <line>".into()
                        } else {
                            format!("Invalid line number: '{arg}'")
                        });
                    }
                }
                false
            }
            "todo" => {
                self.open_todo_overlay();
                false
            }
            "ai" => {
                self.open_ai_panel();
                false
            }
            "filter" => {
                let arg = arg.trim();
                if !arg.is_empty() {
                    self.active_filter = Some(arg.to_string());
                }
                self.open_filter_panel();
                false
            }
            "next" => {
                self.jump_to_match(true);
                false
            }
            "prev" => {
                self.jump_to_match(false);
                false
            }
            "clear" => {
                self.clear_filter();
                false
            }
            "help" => {
                self.open_help();
                false
            }
            "sync" => {
                self.cmd_sync();
                false
            }
            "spell" => {
                let scope = if arg == "all" {
                    SpellScope::WholeFile
                } else if let Some(s) = self.selection_span() {
                    SpellScope::Selection(s)
                } else {
                    SpellScope::CurrentNote
                };
                self.open_spell_check(scope);
                false
            }
            "set" => {
                self.status_msg = Some("No configurable settings yet".into());
                false
            }
            other => {
                self.status_msg = Some(format!("Unknown command: :{other}"));
                false
            }
        }
    }

    // -------------------------------------------------------------------------
    // Save / open / reload
    // -------------------------------------------------------------------------

    fn save(&mut self) {
        // Check for external changes before writing.
        if let Some(ref snap) = self.snapshot {
            match snap.check() {
                Ok(ConflictStatus::Clean) => {}
                Ok(ConflictStatus::Changed) => {
                    self.mode = Mode::Conflict(ConflictKind::Changed);
                    return;
                }
                Ok(ConflictStatus::Missing) => {
                    self.mode = Mode::Conflict(ConflictKind::Missing);
                    return;
                }
                Err(e) => {
                    self.status_msg = Some(format!("Conflict check failed: {e}"));
                    return;
                }
            }
        }
        self.force_save();
    }

    /// Write without conflict check (user chose Overwrite, or no snapshot exists).
    fn force_save(&mut self) {
        match write_atomic(&self.file, self.buffer.as_str()) {
            Ok(()) => {
                self.saved_hash = Some(hash_content(self.buffer.as_str()));
                self.dirty = false;
                self.snapshot = FileSnapshot::capture(&self.file).ok();
                self.status_msg = Some("Saved".into());
                self.spawn_background_push();
            }
            Err(e) => {
                self.status_msg = Some(format!("Save failed: {e}"));
            }
        }
    }

    /// Spawn a background commit+push, if sync is enabled and a repo is
    /// configured. Stores the receiver on `App` for the event loop to drain.
    fn spawn_background_push(&mut self) {
        if !self.sync_cfg.push_on_save {
            return;
        }
        if self.sync_rx.is_some() {
            return;
        }
        let repo = match self.sync_repo.clone() {
            Some(r) => r,
            None => return,
        };
        // Don't push when there's an unresolved conflict.
        if matches!(self.sync_state, crate::sync::SyncState::Conflict) {
            return;
        }
        let file = self.file.clone();
        let msg = format!(
            "notes: {}",
            crate::date::format_datetime(crate::date::now().naive_local())
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = crate::sync::commit_and_push(&repo, &file, &msg);
            let _ = tx.send(SyncMsg::PushDone(outcome));
        });
        self.sync_state = crate::sync::SyncState::Pushing;
        self.sync_rx = Some(rx);
    }

    /// Spawn a background pull (full `git pull --rebase`).
    fn spawn_background_pull(&mut self) {
        if self.sync_rx.is_some() {
            return;
        }
        let repo = match self.sync_repo.clone() {
            Some(r) => r,
            None => return,
        };
        if matches!(self.sync_state, crate::sync::SyncState::Conflict) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome =
                crate::sync::pull_with_timeout(&repo, std::time::Duration::from_secs(15));
            let _ = tx.send(SyncMsg::PullDone(outcome));
        });
        self.sync_state = crate::sync::SyncState::Pulling;
        self.sync_rx = Some(rx);
    }

    /// Resolve a sync rebase conflict by taking one side, then continue + push.
    /// `keep_local = true` discards the remote's conflicting hunks;
    /// `keep_local = false` discards local.
    fn resolve_sync_conflict(&mut self, keep_local: bool) {
        let repo = match self.sync_repo.clone() {
            Some(r) => r,
            None => return,
        };
        // In a `git pull --rebase`, "ours" = the remote (the branch we're
        // rebasing onto) and "theirs" = our local commits. So plain-English
        // "keep local" corresponds to `--theirs`, and "take remote" to `--ours`.
        let side = if keep_local { "--theirs" } else { "--ours" };
        let file = self.file.clone();
        match crate::sync::resolve_rebase_conflict(&repo, &file, side) {
            Ok(()) => {
                self.status_msg = Some("Conflict resolved and pushed".into());
                // Reload the file (resolution is now on disk).
                self.reload();
                self.sync_state = crate::sync::SyncState::Idle;
            }
            Err(e) => {
                self.status_msg = Some(format!("Resolve failed: {e}"));
                self.sync_state = crate::sync::SyncState::Error(e);
            }
        }
    }

    /// Manual `:sync` — spawn a background pull. If we're ahead of the
    /// remote, the post-pull state machine will trigger a push next.
    fn cmd_sync(&mut self) {
        if self.sync_repo.is_none() {
            self.status_msg = Some("Sync disabled (not in a git repo)".into());
            return;
        }
        if matches!(self.sync_state, crate::sync::SyncState::Conflict) {
            self.status_msg = Some("Resolve sync conflict first".into());
            return;
        }
        if self.sync_rx.is_some() {
            self.status_msg = Some("Sync already in progress".into());
            return;
        }
        self.spawn_background_pull();
    }

    /// Drain any completed sync message. Called from the event loop while
    /// `sync_rx` is `Some`.
    fn poll_sync(&mut self) {
        let recv = match self.sync_rx.as_ref() {
            Some(rx) => rx.try_recv().ok(),
            None => return,
        };
        let msg = match recv {
            Some(m) => m,
            None => return,
        };
        self.sync_rx = None;
        match msg {
            SyncMsg::PushDone(outcome) => {
                use crate::sync::PushOutcome;
                self.sync_state = match outcome {
                    PushOutcome::Pushed | PushOutcome::NothingToPush => {
                        crate::sync::SyncState::Idle
                    }
                    PushOutcome::Conflicted => {
                        // Remote moved while we were committing — needs pull+rebase.
                        let ahead = self
                            .sync_repo
                            .as_deref()
                            .map(crate::sync::count_ahead)
                            .unwrap_or(0);
                        crate::sync::SyncState::AheadBy(ahead.max(1))
                    }
                    PushOutcome::Offline => crate::sync::SyncState::Offline,
                    PushOutcome::Error(e) => crate::sync::SyncState::Error(e),
                };
            }
            SyncMsg::PullDone(outcome) => {
                use crate::sync::PullOutcome;
                self.sync_state = match outcome {
                    PullOutcome::UpToDate => crate::sync::SyncState::Idle,
                    PullOutcome::FastForwarded => {
                        // File on disk may have new content — reload if buffer is clean.
                        if !self.dirty {
                            if let Ok(buf) = crate::buffer::TextBuffer::from_file(&self.file) {
                                self.buffer = buf;
                                self.saved_hash = Some(hash_content(self.buffer.as_str()));
                                self.snapshot = FileSnapshot::capture(&self.file).ok();
                                self.cursor_line = self.cursor_line.min(
                                    self.buffer.line_count().saturating_sub(1),
                                );
                                self.cursor_byte = 0;
                                self.update_preferred_col();
                                self.scroll_to_cursor();
                            }
                        }
                        if crate::sync::has_conflict_markers(self.buffer.as_str()) {
                            crate::sync::SyncState::Conflict
                        } else {
                            crate::sync::SyncState::Idle
                        }
                    }
                    PullOutcome::Conflicted => crate::sync::SyncState::Conflict,
                    PullOutcome::Offline => crate::sync::SyncState::Offline,
                    PullOutcome::Error(e) => crate::sync::SyncState::Error(e),
                };
            }
        }
        // If we landed in Conflict state and aren't already showing the overlay,
        // open it so the user can resolve immediately.
        if matches!(self.sync_state, crate::sync::SyncState::Conflict)
            && !matches!(self.mode, Mode::Conflict(ConflictKind::SyncRebase))
        {
            self.mode = Mode::Conflict(ConflictKind::SyncRebase);
        }
    }

    fn reload(&mut self) {
        match TextBuffer::from_file(&self.file) {
            Ok(buffer) => {
                self.buffer = buffer;
                self.saved_hash = Some(hash_content(self.buffer.as_str()));
                self.snapshot = FileSnapshot::capture(&self.file).ok();
                self.history.clear();
                self.dirty = false;
                let lc = self.buffer.line_count();
                if self.cursor_line >= lc {
                    self.cursor_line = lc.saturating_sub(1);
                }
                let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
                let cl = line_content_len(line);
                if self.cursor_byte > cl {
                    self.cursor_byte = cl;
                }
                self.update_preferred_col();
                self.status_msg = Some("Reloaded from disk".into());
            }
            Err(e) => {
                self.status_msg = Some(format!("Reload failed: {e}"));
            }
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        // Persist cursor position for the file we're leaving.
        self.save_cursor_state();

        let discard_msg = if self.dirty {
            Some("  (unsaved changes discarded)")
        } else {
            None
        };

        let (buffer, snapshot) = if path.exists() {
            match TextBuffer::from_file(&path) {
                Ok(buf) => {
                    let snap = FileSnapshot::capture(&path).ok();
                    (buf, snap)
                }
                Err(e) => {
                    self.status_msg = Some(format!("Cannot open: {e}"));
                    return;
                }
            }
        } else {
            (TextBuffer::empty(), None)
        };

        // Restore saved cursor for the new file.
        let path_key = path.to_string_lossy().into_owned();
        let state = load_state();
        let (cursor_line, cursor_byte) = state
            .cursor
            .iter()
            .find(|r| r.path == path_key)
            .map(|r| {
                let lc = buffer.line_count();
                let li = r.line.min(lc.saturating_sub(1));
                let cl = line_content_len(buffer.line_text(li).unwrap_or(""));
                (li, r.byte.min(cl))
            })
            .unwrap_or((0, 0));

        self.saved_hash = Some(hash_content(buffer.as_str()));
        self.buffer = buffer;
        self.file = path;
        self.snapshot = snapshot;
        self.history.clear();
        self.dirty = false;
        self.cursor_line = cursor_line;
        self.cursor_byte = cursor_byte;
        self.preferred_col = 0;
        self.scroll = 0;
        self.update_preferred_col();
        self.scroll_to_cursor();

        self.status_msg = Some(format!("Opened{}", discard_msg.unwrap_or("")));
    }

    // -------------------------------------------------------------------------
    // Cursor movement
    // -------------------------------------------------------------------------

    fn move_down(&mut self, n: usize) {
        if self.line_wrap {
            let width = self.viewport_width.max(1);
            for _ in 0..n {
                if !self.visual_step(true, width) {
                    break;
                }
            }
            self.scroll_to_cursor();
            return;
        }
        let lc = self.buffer.line_count();
        self.cursor_line = (self.cursor_line + n).min(lc.saturating_sub(1));
        self.apply_preferred_col();
        self.scroll_to_cursor();
    }

    fn move_up(&mut self, n: usize) {
        if self.line_wrap {
            let width = self.viewport_width.max(1);
            for _ in 0..n {
                if !self.visual_step(false, width) {
                    break;
                }
            }
            self.scroll_to_cursor();
            return;
        }
        self.cursor_line = self.cursor_line.saturating_sub(n);
        self.apply_preferred_col();
        if self.cursor_line < self.scroll {
            self.scroll = self.cursor_line;
        }
    }

    /// Step the cursor one visual row in wrap mode. Returns `false` if we're
    /// already at the top/bottom edge.
    fn visual_step(&mut self, forward: bool, width: usize) -> bool {
        let cur_line_text = self.buffer.line_text(self.cursor_line).unwrap_or("");
        let cur_content: String = cur_line_text
            .trim_end_matches(|c: char| c == '\r' || c == '\n')
            .to_string();
        let cur_segs = wrap_segments(&cur_content, width);
        let cur_seg_idx = cur_segs
            .iter()
            .position(|&(s, e)| self.cursor_byte >= s && self.cursor_byte <= e)
            .unwrap_or(cur_segs.len().saturating_sub(1));

        let (new_line, new_seg, new_content) = if forward {
            if cur_seg_idx + 1 < cur_segs.len() {
                (self.cursor_line, cur_segs[cur_seg_idx + 1], cur_content.clone())
            } else if self.cursor_line + 1 < self.buffer.line_count() {
                let next_line = self.cursor_line + 1;
                let next_text = self.buffer.line_text(next_line).unwrap_or("");
                let next_content: String = next_text
                    .trim_end_matches(|c: char| c == '\r' || c == '\n')
                    .to_string();
                let next_segs = wrap_segments(&next_content, width);
                let first = *next_segs.first().unwrap_or(&(0, next_content.len()));
                (next_line, first, next_content)
            } else {
                return false;
            }
        } else {
            if cur_seg_idx > 0 {
                (self.cursor_line, cur_segs[cur_seg_idx - 1], cur_content.clone())
            } else if self.cursor_line > 0 {
                let prev_line = self.cursor_line - 1;
                let prev_text = self.buffer.line_text(prev_line).unwrap_or("");
                let prev_content: String = prev_text
                    .trim_end_matches(|c: char| c == '\r' || c == '\n')
                    .to_string();
                let prev_segs = wrap_segments(&prev_content, width);
                let last = *prev_segs.last().unwrap_or(&(0, prev_content.len()));
                (prev_line, last, prev_content)
            } else {
                return false;
            }
        };

        let (s, e) = new_seg;
        let seg_content = &new_content[s..e];
        let byte_in_seg = byte_for_visual_col(seg_content, self.preferred_col);
        self.cursor_line = new_line;
        self.cursor_byte = s + byte_in_seg;
        true
    }

    fn move_left(&mut self) {
        if self.cursor_byte > 0 {
            let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
            self.cursor_byte = prev_char_boundary(line, self.cursor_byte);
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            if self.cursor_line < self.scroll {
                self.scroll = self.cursor_line;
            }
            let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
            self.cursor_byte = line_content_len(line);
        }
    }

    fn move_right(&mut self) {
        let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
        let content_len = line_content_len(line);
        if self.cursor_byte < content_len {
            let ch_len = line[self.cursor_byte..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.cursor_byte += ch_len;
        } else if self.cursor_line + 1 < self.buffer.line_count() {
            self.cursor_line += 1;
            self.cursor_byte = 0;
            self.scroll_to_cursor();
        }
    }

    fn update_preferred_col(&mut self) {
        if self.line_wrap {
            let width = self.viewport_width.max(1);
            let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
            let content = line.trim_end_matches(|c: char| c == '\r' || c == '\n');
            let segs = wrap_segments(content, width);
            let seg = segs
                .iter()
                .find(|&&(s, e)| self.cursor_byte >= s && self.cursor_byte <= e)
                .copied()
                .unwrap_or((0, content.len()));
            self.preferred_col = visual_col_for_byte(
                &content[seg.0..seg.1],
                self.cursor_byte.saturating_sub(seg.0),
            );
            return;
        }
        self.preferred_col = self.cursor_visual_col();
    }

    fn apply_preferred_col(&mut self) {
        let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
        self.cursor_byte = byte_for_visual_col(line, self.preferred_col);
    }

    // -------------------------------------------------------------------------
    // Editing operations
    // -------------------------------------------------------------------------

    fn insert_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.insert_str(s);
    }

    fn insert_str(&mut self, s: &str) {
        if self.selection_anchor.is_some() {
            self.replace_selection(s);
            return;
        }
        let snap = self.make_snapshot();
        if self.in_burst {
            self.history.push_batch(snap, BatchKind::Burst);
        } else {
            match str_batch_kind(s) {
                Some(k) => self.history.push_batch(snap, k),
                None => self.history.push(snap),
            }
        }
        let abs = self.cursor_abs_pos();
        self.buffer.replace_span(ByteSpan::new(abs, abs), s);
        self.cursor_byte += s.len();
        self.dirty = true;
        self.update_preferred_col();
    }

    fn insert_newline(&mut self) {
        if self.selection_anchor.is_some() {
            let le = detect_line_ending(self.buffer.as_str()).to_string();
            self.replace_selection(&le);
            return;
        }
        let snap = self.make_snapshot();
        if self.in_burst {
            self.history.push_batch(snap, BatchKind::Burst);
        } else {
            self.history.push(snap);
        }
        let le = detect_line_ending(self.buffer.as_str());
        let abs = self.cursor_abs_pos();
        self.buffer.replace_span(ByteSpan::new(abs, abs), le);
        self.cursor_line += 1;
        self.cursor_byte = 0;
        self.preferred_col = 0;
        self.dirty = true;
        self.scroll_to_cursor();
    }

    /// If the current line is a delimiter (`===` or `=== <title>`) that does
    /// not yet have a date or a `|` separator, expand it: stamp the current
    /// date/time, separating it from any title with `|`. Drops the cursor onto
    /// the body line below. Returns `true` if the expansion happened.
    fn try_expand_delimiter(&mut self) -> bool {
        let line = match self.buffer.line_text(self.cursor_line) {
            Some(l) => l,
            None => return false,
        };
        let content_len = line_content_len(line);
        let content = &line[..content_len];

        if content != "===" && !content.starts_with("=== ") {
            return false;
        }
        // Idempotent: if a separator or date is already present, don't expand.
        if content.contains('|') {
            return false;
        }
        let title = content.strip_prefix("===").unwrap_or("").trim();
        if looks_like_date_prefix(title) {
            return false;
        }

        let snap = self.make_snapshot();
        self.history.push(snap);

        let line_start = self
            .buffer
            .line_index()
            .line_start(self.cursor_line)
            .unwrap_or(0);
        let stamp = date::format_datetime(date::now().naive_local());
        let le = detect_line_ending(self.buffer.as_str()).to_string();

        let replacement = if title.is_empty() {
            format!("=== {stamp}{le}")
        } else {
            format!("=== {title} | {stamp}{le}")
        };

        self.buffer.replace_span(
            ByteSpan::new(line_start, line_start + content_len),
            &replacement,
        );

        self.cursor_line += 1;
        self.cursor_byte = 0;
        self.preferred_col = 0;
        self.dirty = true;
        self.scroll_to_cursor();
        true
    }

    fn delete_backward(&mut self) {
        if self.selection_anchor.is_some() {
            self.replace_selection("");
            return;
        }
        if self.cursor_byte > 0 {
            let snap = self.make_snapshot();
            self.history.push_batch(snap, BatchKind::DeleteBack);
            let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
            let prev = prev_char_boundary(line, self.cursor_byte);
            let line_start = self
                .buffer
                .line_index()
                .line_start(self.cursor_line)
                .unwrap_or(0);
            self.buffer.replace_span(
                ByteSpan::new(line_start + prev, line_start + self.cursor_byte),
                "",
            );
            self.cursor_byte = prev;
            self.dirty = true;
            self.update_preferred_col();
        } else if self.cursor_line > 0 {
            let snap = self.make_snapshot();
            self.history.push(snap); // line-merge is always its own entry
            let prev_line = self.cursor_line - 1;
            let prev_text = self.buffer.line_text(prev_line).unwrap_or("");
            let prev_content_len = line_content_len(prev_text);
            let prev_start = self
                .buffer
                .line_index()
                .line_start(prev_line)
                .unwrap_or(0);
            let curr_start = self
                .buffer
                .line_index()
                .line_start(self.cursor_line)
                .unwrap_or(0);
            self.buffer
                .replace_span(ByteSpan::new(prev_start + prev_content_len, curr_start), "");
            self.cursor_line -= 1;
            self.cursor_byte = prev_content_len;
            if self.cursor_line < self.scroll {
                self.scroll = self.cursor_line;
            }
            self.dirty = true;
            self.update_preferred_col();
        }
    }

    fn delete_forward(&mut self) {
        if self.selection_anchor.is_some() {
            self.replace_selection("");
            return;
        }
        let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
        let content_len = line_content_len(line);
        let line_start = self
            .buffer
            .line_index()
            .line_start(self.cursor_line)
            .unwrap_or(0);

        if self.cursor_byte < content_len {
            let snap = self.make_snapshot();
            self.history.push_batch(snap, BatchKind::DeleteFwd);
            let ch_len = line[self.cursor_byte..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            let abs = line_start + self.cursor_byte;
            self.buffer
                .replace_span(ByteSpan::new(abs, abs + ch_len), "");
            self.dirty = true;
            self.update_preferred_col();
        } else if self.cursor_line + 1 < self.buffer.line_count() {
            let snap = self.make_snapshot();
            self.history.push(snap); // line-merge is always its own entry
            let abs_nl = line_start + content_len;
            let next_start = self
                .buffer
                .line_index()
                .line_start(self.cursor_line + 1)
                .unwrap_or(abs_nl + 1);
            self.buffer
                .replace_span(ByteSpan::new(abs_nl, next_start), "");
            self.dirty = true;
            self.update_preferred_col();
        }
    }

    // -------------------------------------------------------------------------
    // Undo / redo
    // -------------------------------------------------------------------------

    fn make_snapshot(&self) -> HistoryEntry {
        HistoryEntry {
            content: self.buffer.as_str().to_string(),
            cursor_line: self.cursor_line,
            cursor_byte: self.cursor_byte,
        }
    }

    fn restore_entry(&mut self, entry: HistoryEntry) {
        self.buffer = TextBuffer::new(entry.content);
        self.cursor_line = entry.cursor_line;
        self.cursor_byte = entry.cursor_byte;
        // Clamp in case the restored state is smaller than current cursor.
        let lc = self.buffer.line_count();
        if self.cursor_line >= lc {
            self.cursor_line = lc.saturating_sub(1);
        }
        let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
        let cl = line_content_len(line);
        if self.cursor_byte > cl {
            self.cursor_byte = cl;
        }
        self.update_preferred_col();
        // Correctly clear dirty flag when undo brings us back to the saved state.
        self.dirty = self
            .saved_hash
            .map_or(true, |h| h != hash_content(self.buffer.as_str()));
        self.scroll_to_cursor();
    }

    fn do_undo(&mut self) {
        let current = self.make_snapshot();
        match self.history.undo(current) {
            Some(entry) => {
                let remaining = self.history.undo.len();
                self.restore_entry(entry);
                self.status_msg = if remaining > 0 {
                    Some(format!("Undo  ({remaining} more)"))
                } else {
                    Some("Undo  (nothing more)".into())
                };
            }
            None => {
                self.status_msg = Some("Nothing to undo".into());
            }
        }
    }

    fn do_redo(&mut self) {
        let current = self.make_snapshot();
        match self.history.redo(current) {
            Some(entry) => {
                let remaining = self.history.redo.len();
                self.restore_entry(entry);
                self.status_msg = if remaining > 0 {
                    Some(format!("Redo  ({remaining} more)"))
                } else {
                    Some("Redo".into())
                };
            }
            None => {
                self.status_msg = Some("Nothing to redo".into());
            }
        }
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Persist the current cursor position for `self.file` into state.toml.
    fn save_cursor_state(&self) {
        let path_key = self.file.to_string_lossy().into_owned();
        let mut state = load_state();
        if let Some(rec) = state.cursor.iter_mut().find(|r| r.path == path_key) {
            rec.line = self.cursor_line;
            rec.byte = self.cursor_byte;
        } else {
            state.cursor.push(CursorRecord {
                path: path_key,
                line: self.cursor_line,
                byte: self.cursor_byte,
            });
        }
        const MAX_ENTRIES: usize = 100;
        if state.cursor.len() > MAX_ENTRIES {
            state.cursor.drain(0..state.cursor.len() - MAX_ENTRIES);
        }
        save_state(&state);
    }

    /// Return the line index of the next `=== ` note delimiter after the cursor.
    fn next_note_delimiter(&self) -> Option<usize> {
        for li in (self.cursor_line + 1)..self.buffer.line_count() {
            let raw = self.buffer.line_text(li).unwrap_or("");
            let c = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');
            if c == "===" || c.starts_with("=== ") {
                return Some(li);
            }
        }
        None
    }

    /// Return the line index of the nearest `=== ` note delimiter before the cursor.
    fn prev_note_delimiter(&self) -> Option<usize> {
        for li in (0..self.cursor_line).rev() {
            let raw = self.buffer.line_text(li).unwrap_or("");
            let c = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');
            if c == "===" || c.starts_with("=== ") {
                return Some(li);
            }
        }
        None
    }

    fn cursor_abs_pos(&self) -> usize {
        self.buffer
            .line_index()
            .line_start(self.cursor_line)
            .unwrap_or(0)
            + self.cursor_byte
    }

    /// Returns the active selection's absolute byte range (normalised so
    /// `start <= end`), or `None` if no selection is active or it's empty.
    fn selection_span(&self) -> Option<ByteSpan> {
        let (al, ab) = self.selection_anchor?;
        let li = self.buffer.line_index();
        let anchor_abs = li.line_start(al).unwrap_or(0) + ab;
        let cursor_abs = self.cursor_abs_pos();
        if anchor_abs == cursor_abs {
            return None;
        }
        Some(ByteSpan::new(
            anchor_abs.min(cursor_abs),
            anchor_abs.max(cursor_abs),
        ))
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Replace the active selection with `replacement` as a single undo entry,
    /// moving the cursor to just after the replacement. No-op if no selection.
    fn replace_selection(&mut self, replacement: &str) -> bool {
        let span = match self.selection_span() {
            Some(s) => s,
            None => return false,
        };
        let snap = self.make_snapshot();
        self.history.push(snap);
        self.buffer.replace_span(span, replacement);
        let new_abs = span.start + replacement.len();
        let li = self.buffer.line_index();
        let new_line = li.offset_to_line(new_abs);
        let new_line_start = li.line_start(new_line).unwrap_or(0);
        self.cursor_line = new_line;
        self.cursor_byte = new_abs - new_line_start;
        self.selection_anchor = None;
        self.dirty = true;
        self.update_preferred_col();
        self.scroll_to_cursor();
        true
    }

    fn copy_selection(&mut self) {
        let span = match self.selection_span() {
            Some(s) => s,
            None => {
                self.status_msg = Some("Nothing selected".into());
                return;
            }
        };
        let text = self.buffer.as_str()[span.start..span.end].to_string();
        match set_clipboard_text(&text) {
            Ok(()) => {
                let n = text.chars().count();
                self.status_msg = Some(format!(
                    "Copied {n} character{}",
                    if n == 1 { "" } else { "s" }
                ));
            }
            Err(e) => {
                self.status_msg = Some(format!("Copy failed: {e}"));
            }
        }
    }

    fn cut_selection(&mut self) {
        let span = match self.selection_span() {
            Some(s) => s,
            None => {
                self.status_msg = Some("Nothing selected".into());
                return;
            }
        };
        let text = self.buffer.as_str()[span.start..span.end].to_string();
        match set_clipboard_text(&text) {
            Ok(()) => {
                let n = text.chars().count();
                self.replace_selection("");
                self.status_msg = Some(format!(
                    "Cut {n} character{}",
                    if n == 1 { "" } else { "s" }
                ));
            }
            Err(e) => {
                self.status_msg = Some(format!("Cut failed: {e}"));
            }
        }
    }

    fn paste_clipboard(&mut self) {
        match get_clipboard_text() {
            Ok(text) if !text.is_empty() => {
                // Normalise line endings to match the current file.
                let le = detect_line_ending(self.buffer.as_str());
                let normalised = normalise_line_endings(&text, le);
                if self.selection_anchor.is_some() {
                    self.replace_selection(&normalised);
                } else {
                    self.insert_at_cursor(&normalised);
                }
            }
            Ok(_) => {
                self.status_msg = Some("Clipboard empty".into());
            }
            Err(e) => {
                self.status_msg = Some(format!("Paste failed: {e}"));
            }
        }
    }

    /// Insert `text` at the cursor as a single undo entry. Used by paste; the
    /// normal `insert_str` would batch char-by-char.
    fn insert_at_cursor(&mut self, text: &str) {
        let snap = self.make_snapshot();
        self.history.push(snap);
        let abs = self.cursor_abs_pos();
        self.buffer.replace_span(ByteSpan::new(abs, abs), text);
        let new_abs = abs + text.len();
        let li = self.buffer.line_index();
        let new_line = li.offset_to_line(new_abs);
        let line_start = li.line_start(new_line).unwrap_or(0);
        self.cursor_line = new_line;
        self.cursor_byte = new_abs - line_start;
        self.dirty = true;
        self.update_preferred_col();
        self.scroll_to_cursor();
    }

    fn cursor_visual_col(&self) -> usize {
        let line = self.buffer.line_text(self.cursor_line).unwrap_or("");
        visual_col_for_byte(line, self.cursor_byte)
    }

    fn scroll_to_cursor(&mut self) {
        const MARGIN: usize = 3;
        // Top edge: pull the cursor file line into view.
        if self.cursor_line < self.scroll {
            self.scroll = self.cursor_line;
        }

        if !self.line_wrap {
            // Without wrap, every file line is one visual row — file-line math
            // suffices and avoids the cost of wrap_segments.
            if self.viewport_height > MARGIN
                && self.cursor_line + MARGIN >= self.scroll + self.viewport_height
            {
                self.scroll = self.cursor_line + MARGIN + 1 - self.viewport_height;
            }
            return;
        }

        // Wrap-aware: a single file line can produce many visual rows. Advance
        // `self.scroll` one file line at a time until the cursor's visual row
        // sits inside the viewport.
        let width = self.viewport_width.max(1);
        while self.scroll < self.cursor_line {
            let v = self.cursor_visual_row_from_scroll(width);
            if v + MARGIN < self.viewport_height || self.viewport_height <= MARGIN {
                break;
            }
            self.scroll += 1;
        }
    }

    /// Visual-row offset of the cursor relative to `self.scroll`, accounting
    /// for word-wrap segments along the way. Used by wrap-aware
    /// `scroll_to_cursor`.
    fn cursor_visual_row_from_scroll(&self, width: usize) -> usize {
        let mut row = 0usize;
        for li in self.scroll..self.cursor_line {
            let raw = self.buffer.line_text(li).unwrap_or("");
            let content = raw.trim_end_matches(|c: char| c == '\r' || c == '\n');
            row = row.saturating_add(wrap_segments(content, width).len());
        }
        let raw = self.buffer.line_text(self.cursor_line).unwrap_or("");
        let content = raw.trim_end_matches(|c: char| c == '\r' || c == '\n');
        let segs = wrap_segments(content, width);
        let seg_idx = segs
            .iter()
            .position(|&(s, e)| self.cursor_byte >= s && self.cursor_byte <= e)
            .unwrap_or(segs.len().saturating_sub(1));
        row.saturating_add(seg_idx)
    }
}

// ---------------------------------------------------------------------------
// Layout helper
// ---------------------------------------------------------------------------

/// Return a centered rectangle of exactly `height` rows and `width_pct`% of
/// the terminal width.
fn centered_rect(width_pct: u16, height: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .split(area);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(vert[1]);
    horiz[1]
}

// ---------------------------------------------------------------------------
// Unicode / byte helpers
// ---------------------------------------------------------------------------

fn visual_col_for_byte(line: &str, byte: usize) -> usize {
    let byte = byte.min(line.len());
    line[..byte]
        .chars()
        .map(|c| c.width().unwrap_or(0))
        .sum()
}

fn byte_for_visual_col(line: &str, col: usize) -> usize {
    let content = line.trim_end_matches(|c: char| c == '\n' || c == '\r');
    let mut visual = 0usize;
    for (byte, ch) in content.char_indices() {
        let w = ch.width().unwrap_or(0);
        if visual + w > col {
            return byte;
        }
        visual += w;
    }
    content.len()
}

fn line_content_len(line: &str) -> usize {
    line.trim_end_matches(|c: char| c == '\n' || c == '\r').len()
}

/// Every command-bar verb that's wired into `execute_command`. Used for Tab
/// completion in the command bar.
const COMMAND_VERBS: &[&str] = &[
    "w", "write", "q", "quit", "q!", "quit!", "wq", "x", "goto", "go", "todo", "ai",
    "filter", "next", "prev", "clear", "spell", "help", "sync", "set",
];

/// Try to complete `input` against `COMMAND_VERBS`. Returns the new input
/// string if completion produced a longer prefix; `None` if there's nothing to
/// complete (no matches, or already at the common prefix among multiple).
fn complete_command_verb(input: &str) -> Option<String> {
    let matches: Vec<&str> = COMMAND_VERBS
        .iter()
        .copied()
        .filter(|c| c.starts_with(input))
        .collect();
    if matches.is_empty() {
        return None;
    }
    if matches.len() == 1 {
        return Some(format!("{} ", matches[0]));
    }
    // Multiple matches → complete to the longest common prefix.
    let first = matches[0];
    let mut prefix_len = first.len();
    for m in &matches[1..] {
        prefix_len = prefix_len.min(
            first
                .chars()
                .zip(m.chars())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    let prefix: String = first.chars().take(prefix_len).collect();
    if prefix.len() > input.len() {
        Some(prefix)
    } else {
        None
    }
}

/// Hard-coded help content as a 3-column table: shortcut · command · description.
/// Single source of truth for the in-app help overlay; SPEC §4.3 / §6 should
/// match.
fn help_content() -> Vec<Line<'static>> {
    const SC_W: usize = 28;
    const CMD_W: usize = 14;

    let header = |s: &'static str| {
        Line::from(Span::styled(
            s,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
    };
    let row = |shortcut: &str, command: &str, desc: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:<width$}", shortcut, width = SC_W),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("{:<width$}", command, width = CMD_W),
                Style::default().fg(Color::LightCyan),
            ),
            Span::raw(desc.to_string()),
        ])
    };
    let blank = || Line::from("");

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Column headers.
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<width$}", "Shortcut", width = SC_W),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<width$}", "Command", width = CMD_W),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Description",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(blank());

    lines.push(header("File"));
    lines.push(row("Ctrl+S", ":w", "Save"));
    lines.push(row("", ":wq, :x", "Save and exit"));
    lines.push(row("Ctrl+O", "", "Open file…"));
    lines.push(row("Ctrl+Q", ":q", "Quit (warns on unsaved)"));
    lines.push(row("", ":q!", "Quit without saving"));
    lines.push(blank());

    lines.push(header("Edit"));
    lines.push(row("Ctrl+Z", "", "Undo"));
    lines.push(row("Ctrl+Y", "", "Redo"));
    lines.push(row("Ctrl+C", "", "Copy selection"));
    lines.push(row("Ctrl+X", "", "Cut selection"));
    lines.push(row("Ctrl+V", "", "Paste at cursor"));
    lines.push(row("Shift+Arrow", "", "Extend selection (also Home/End/PgUp/Dn)"));
    lines.push(row("Esc", "", "Clear selection / close overlay"));
    lines.push(blank());

    lines.push(header("Navigate"));
    lines.push(row("Ctrl+J", "", "Next note (filter-aware)"));
    lines.push(row("Ctrl+K", "", "Previous note (filter-aware)"));
    lines.push(row("", ":goto N", "Jump to line N"));
    lines.push(row("Mouse wheel", "", "Scroll viewport (cursor stays put)"));
    lines.push(row("Enter on `===`", "", "Stamp date and start a new note"));
    lines.push(blank());

    lines.push(header("Find"));
    lines.push(row("Ctrl+F", "", "Search overlay"));
    lines.push(row("F3", "", "Search: next match"));
    lines.push(row("Shift+F3", "", "Search: previous match"));
    lines.push(blank());

    lines.push(header("Filter"));
    lines.push(row("Ctrl+P", ":filter", "Filter panel (live picker)"));
    lines.push(row("F4", ":next", "Filter: next match"));
    lines.push(row("Shift+F4", ":prev", "Filter: previous match"));
    lines.push(row("", ":clear", "Clear active filter"));
    lines.push(blank());

    lines.push(header("Workflows"));
    lines.push(row("Ctrl+L", ":ai", "AI panel (Polish workflow)"));
    lines.push(row("F7", ":spell", "Spell check"));
    lines.push(row("F8", ":todo", "Todo overlay"));
    lines.push(blank());

    lines.push(header("Sync"));
    lines.push(row("", ":sync", "Manual pull+push (when in a git repo)"));
    lines.push(row("", "", "Pull on open, push on save are automatic"));
    lines.push(row("", "", "Status bar: ✓ idle · ↑N ahead · ↓… pulling · ↑… pushing"));
    lines.push(blank());

    lines.push(header("Command bar"));
    lines.push(row("Ctrl+; / F10", "", "Open command bar"));
    lines.push(row("Tab", "", "Complete command verb"));
    lines.push(row("Up / Down", "", "Browse session history"));
    lines.push(row("F1", ":help", "Show this help"));
    lines.push(blank());

    lines.push(header("Filter query operators"));
    lines.push(row("tag:foo", "", "Has tag `foo`"));
    lines.push(row("tag:foo,bar", "", "Has tag foo OR bar (OR within comma)"));
    lines.push(row("tag:foo tag:bar", "", "Has tag foo AND bar (AND across tokens)"));
    lines.push(row("title:keyword", "", "Title contains `keyword`"));
    lines.push(row("date:2026-05-12", "", "Note dated on this day"));
    lines.push(row("date:2026-05", "", "Note dated in this month"));
    lines.push(row("any other word", "", "Substring match in note body"));

    lines
}

/// Returns `Some(ch)` if `event` is a plain character keystroke that's safe to
/// accumulate into a single insert during burst processing (paste). Returns
/// `None` for any event that would otherwise change state — modifiers like
/// Ctrl, special keys like Enter/Tab/Backspace, mouse events, paste events.
fn accumulatable_char(event: &Event) -> Option<char> {
    if let Event::Key(k) = event {
        if k.kind == KeyEventKind::Press
            && !k.modifiers.intersects(
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
            )
        {
            if let KeyCode::Char(c) = k.code {
                return Some(c);
            }
        }
    }
    None
}

/// Compact sync-status chunk for the status bar. Empty when sync is Disabled
/// so the status bar isn't cluttered for users not using the feature.
fn sync_status_indicator(state: &crate::sync::SyncState) -> String {
    use crate::sync::SyncState::*;
    match state {
        Disabled => String::new(),
        Idle => "  \u{2713}".into(),                       // ✓
        Pulling => "  \u{2193}\u{2026}".into(),            // ↓…
        Pushing => "  \u{2191}\u{2026}".into(),            // ↑…
        AheadBy(n) => format!("  \u{2191}{n}"),            // ↑N
        Offline => "  offline".into(),
        Conflict => "  conflict".into(),
        Error(e) => format!("  sync: {}", short_err(e)),
    }
}

fn short_err(s: &str) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    let max = 50;
    if line.chars().count() > max {
        let mut out: String = line.chars().take(max).collect();
        out.push('\u{2026}');
        out
    } else {
        line.to_string()
    }
}

fn set_clipboard_text(text: &str) -> std::result::Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.to_string()).map_err(|e| e.to_string())
}

fn get_clipboard_text() -> std::result::Result<String, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.get_text().map_err(|e| e.to_string())
}

/// Normalise `\r\n` / `\r` line endings in `text` to `target_le`. Keeps pasted
/// content consistent with whatever the file already uses.
fn normalise_line_endings(text: &str, target_le: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str(target_le);
            }
            '\n' => out.push_str(target_le),
            other => out.push(other),
        }
    }
    out
}

fn prev_char_boundary(s: &str, byte: usize) -> usize {
    let mut b = byte - 1;
    while !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

fn next_char_boundary(s: &str, byte: usize) -> usize {
    let mut b = byte + 1;
    while b < s.len() && !s.is_char_boundary(b) {
        b += 1;
    }
    b.min(s.len())
}

fn hash_content(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Determine whether successive insertions of `s` should be batched together
/// for undo purposes. Returns `None` for non-batchable edits (punctuation etc.).
fn str_batch_kind(s: &str) -> Option<BatchKind> {
    match s.chars().next()? {
        c if c.is_alphanumeric() || c == '_' => Some(BatchKind::Word),
        ' ' | '\t' => Some(BatchKind::Space),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Syntax highlighting
// ---------------------------------------------------------------------------

fn is_fence_line(content: &str) -> bool {
    content.starts_with("```") || content.starts_with("~~~")
}

/// Word-aware wrap. Returns a list of `(start_byte, end_byte)` ranges in
/// `content`, one per visual row. Breaks after the last space that still fits
/// within `width` cells; falls back to a hard break inside the word if no such
/// space exists. Leading spaces of continuation segments are dropped.
fn wrap_segments(content: &str, width: usize) -> Vec<(usize, usize)> {
    if width == 0 || content.is_empty() {
        return vec![(0, content.len())];
    }
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(content) <= width {
        return vec![(0, content.len())];
    }

    let mut segs = Vec::new();
    let mut seg_start = 0usize;
    let mut last_space_after: Option<usize> = None; // byte index just after the last space we passed
    let mut cur_w = 0usize;

    for (b, c) in content.char_indices() {
        let cw = c.width().unwrap_or(0);
        let is_space = c == ' ' || c == '\t';
        // Whitespace itself never triggers wrap — it just becomes a candidate
        // break point. Only the next non-space char that overflows the row
        // forces the break.
        if !is_space && cur_w + cw > width {
            let cut = match last_space_after {
                Some(s) if s > seg_start => s,
                _ => b,
            };
            let mut seg_end = cut;
            while seg_end > seg_start
                && matches!(content.as_bytes()[seg_end - 1], b' ' | b'\t')
            {
                seg_end -= 1;
            }
            segs.push((seg_start, seg_end));
            seg_start = cut;
            while seg_start < content.len()
                && matches!(content.as_bytes()[seg_start], b' ' | b'\t')
            {
                seg_start += 1;
            }
            cur_w = UnicodeWidthStr::width(&content[seg_start..b]);
            last_space_after = None;
        }
        if is_space {
            last_space_after = Some(b + c.len_utf8());
        }
        cur_w += cw;
    }
    segs.push((seg_start, content.len()));
    segs
}

/// Slice a styled `Line` to the byte range `[start, end)`, preserving each
/// underlying span's style on the portion that falls inside the range.
fn slice_line(line: &Line<'static>, start: usize, end: usize) -> Line<'static> {
    if start >= end {
        return Line::from(Vec::<Span<'static>>::new());
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    for sp in &line.spans {
        let sp_len = sp.content.len();
        let sp_start = pos;
        let sp_end = pos + sp_len;
        pos = sp_end;
        if sp_end <= start {
            continue;
        }
        if sp_start >= end {
            break;
        }
        let chunk_start = start.max(sp_start) - sp_start;
        let chunk_end = end.min(sp_end) - sp_start;
        let s = sp.content.as_ref();
        if !s.is_char_boundary(chunk_start) || !s.is_char_boundary(chunk_end) {
            continue;
        }
        if chunk_end > chunk_start {
            out.push(Span::styled(s[chunk_start..chunk_end].to_string(), sp.style));
        }
    }
    Line::from(out)
}

/// Build a styled `Line` for a single non-fenced line.
fn highlight_normal_line(content: &str) -> Line<'static> {
    // Note delimiter  ===  (with or without date / title)
    if content == "===" || content.starts_with("=== ") {
        return Line::from(Span::styled(
            content.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Markdown headings  #  through  ######
    let hashes = content.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let after = &content[hashes..];
        if after.is_empty() || after.starts_with(' ') {
            return Line::from(Span::styled(
                content.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // TODO:  prefix — colour the keyword, then highlight the rest inline
    if let Some(rest) = content.strip_prefix("TODO:") {
        let mut spans = vec![Span::styled(
            "TODO:".to_string(),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(highlight_inline(rest).spans);
        return Line::from(spans);
    }

    // DONE:  prefix — dim the whole line
    if content.starts_with("DONE:") {
        return Line::from(Span::styled(
            content.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Open checkbox  - [ ]
    if let Some(rest) = content.strip_prefix("- [ ]") {
        let mut spans = vec![Span::styled(
            "- [ ]".to_string(),
            Style::default().fg(Color::Yellow),
        )];
        spans.extend(highlight_inline(rest).spans);
        return Line::from(spans);
    }

    // Done checkbox  - [x]  or  - [X]
    if content.starts_with("- [x]") || content.starts_with("- [X]") {
        return Line::from(Span::styled(
            content.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Horizontal rule  ---  ***  ___  (checked before list-bullet to avoid
    // catching `---` as `- --`).
    if is_hr_line(content) {
        return Line::from(Span::styled(
            content.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    // Blockquote  > body
    if let Some(rest) = content.strip_prefix("> ") {
        let mut spans = vec![Span::styled(
            "> ".to_string(),
            Style::default().fg(Color::Yellow),
        )];
        // Apply inline highlighting on the body, then add a dim italic tint
        // by post-processing the spans (mix in modifier).
        let inner = highlight_inline(rest);
        for sp in inner.spans {
            // Preserve any specific foreground; just layer italic over the top.
            let style = sp.style.add_modifier(Modifier::ITALIC);
            spans.push(Span::styled(sp.content.into_owned(), style));
        }
        return Line::from(spans);
    }

    // Unordered list bullet  - text  /  * text  /  + text
    // (skip `- [`, which is the checkbox path above).
    if let Some(rest) = list_bullet_rest(content) {
        let bullet_len = content.len() - rest.len();
        let mut spans = vec![Span::styled(
            content[..bullet_len].to_string(),
            Style::default().fg(Color::Yellow),
        )];
        spans.extend(highlight_inline(rest).spans);
        return Line::from(spans);
    }

    // Ordered list  N. text
    if let Some(prefix_len) = ordered_list_prefix_len(content) {
        let mut spans = vec![Span::styled(
            content[..prefix_len].to_string(),
            Style::default().fg(Color::Yellow),
        )];
        spans.extend(highlight_inline(&content[prefix_len..]).spans);
        return Line::from(spans);
    }

    // Everything else — inline scan
    highlight_inline(content)
}

/// True iff `s` is composed of one of the HR characters (`-`, `*`, `_`)
/// repeated ≥3 times, optionally surrounded by whitespace.
fn is_hr_line(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 3 {
        return false;
    }
    let c = match t.chars().next() {
        Some(c) if matches!(c, '-' | '*' | '_') => c,
        _ => return false,
    };
    t.chars().all(|x| x == c)
}

/// If `content` starts with an unordered list bullet (`- `, `* `, or `+ `)
/// — but not a checkbox prefix — return the rest of the line after the bullet
/// and its single trailing space.
fn list_bullet_rest(content: &str) -> Option<&str> {
    let bytes = content.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let c = bytes[0];
    if !matches!(c, b'-' | b'*' | b'+') {
        return None;
    }
    if bytes[1] != b' ' {
        return None;
    }
    // `- [` is the checkbox path — caught earlier. Defensive guard here too.
    if c == b'-' && bytes.len() >= 3 && bytes[2] == b'[' {
        return None;
    }
    Some(&content[2..])
}

/// If `content` starts with `N. ` (one or more ASCII digits, a `.`, then a
/// space), return the length of that prefix.
fn ordered_list_prefix_len(content: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    if i + 1 >= bytes.len() {
        return None;
    }
    if bytes[i] != b'.' || bytes[i + 1] != b' ' {
        return None;
    }
    Some(i + 2)
}

/// Scan `content` for `#tagname` and `` `code` `` tokens and return a `Line`
/// with those regions styled, leaving the rest as plain text.
fn highlight_inline(content: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain_start = 0usize;
    let mut pos = 0usize;
    let bytes = content.as_bytes();

    let dim = Style::default().fg(Color::DarkGray);
    let bold_style = Style::default().add_modifier(Modifier::BOLD);
    let italic_style = Style::default().add_modifier(Modifier::ITALIC);
    let strike_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::CROSSED_OUT);
    let link_text_style = Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::UNDERLINED);
    let url_style = Style::default().fg(Color::DarkGray);

    let flush_plain =
        |spans: &mut Vec<Span<'static>>, content: &str, plain_start: usize, pos: usize| {
            if plain_start < pos {
                spans.push(Span::raw(content[plain_start..pos].to_string()));
            }
        };

    while pos < content.len() {
        let b = bytes[pos];

        // ---- Bold  **...**  ----
        if b == b'*' && pos + 1 < content.len() && bytes[pos + 1] == b'*' {
            if let Some(rel) = content[pos + 2..].find("**") {
                let close = pos + 2 + rel;
                if close > pos + 2 {
                    flush_plain(&mut spans, content, plain_start, pos);
                    spans.push(Span::styled("**".to_string(), dim));
                    spans.push(Span::styled(
                        content[pos + 2..close].to_string(),
                        bold_style,
                    ));
                    spans.push(Span::styled("**".to_string(), dim));
                    plain_start = close + 2;
                    pos = close + 2;
                    continue;
                }
            }
        }

        // ---- Bold  __...__  ----
        if b == b'_' && pos + 1 < content.len() && bytes[pos + 1] == b'_' {
            if let Some(rel) = content[pos + 2..].find("__") {
                let close = pos + 2 + rel;
                if close > pos + 2 {
                    flush_plain(&mut spans, content, plain_start, pos);
                    spans.push(Span::styled("__".to_string(), dim));
                    spans.push(Span::styled(
                        content[pos + 2..close].to_string(),
                        bold_style,
                    ));
                    spans.push(Span::styled("__".to_string(), dim));
                    plain_start = close + 2;
                    pos = close + 2;
                    continue;
                }
            }
        }

        // ---- Strikethrough  ~~...~~  ----
        if b == b'~' && pos + 1 < content.len() && bytes[pos + 1] == b'~' {
            if let Some(rel) = content[pos + 2..].find("~~") {
                let close = pos + 2 + rel;
                if close > pos + 2 {
                    flush_plain(&mut spans, content, plain_start, pos);
                    spans.push(Span::styled("~~".to_string(), dim));
                    spans.push(Span::styled(
                        content[pos + 2..close].to_string(),
                        strike_style,
                    ));
                    spans.push(Span::styled("~~".to_string(), dim));
                    plain_start = close + 2;
                    pos = close + 2;
                    continue;
                }
            }
        }

        // ---- Italic  *...*  ---- (single, not part of **)
        if b == b'*'
            && pos + 1 < content.len()
            && bytes[pos + 1] != b'*'
        {
            if let Some(rel) = content[pos + 1..].find('*') {
                let close = pos + 1 + rel;
                if close > pos + 1 {
                    flush_plain(&mut spans, content, plain_start, pos);
                    spans.push(Span::styled("*".to_string(), dim));
                    spans.push(Span::styled(
                        content[pos + 1..close].to_string(),
                        italic_style,
                    ));
                    spans.push(Span::styled("*".to_string(), dim));
                    plain_start = close + 1;
                    pos = close + 1;
                    continue;
                }
            }
        }

        // ---- Italic  _..._  ---- (single, not part of __)
        if b == b'_'
            && pos + 1 < content.len()
            && bytes[pos + 1] != b'_'
        {
            if let Some(rel) = content[pos + 1..].find('_') {
                let close = pos + 1 + rel;
                if close > pos + 1 {
                    flush_plain(&mut spans, content, plain_start, pos);
                    spans.push(Span::styled("_".to_string(), dim));
                    spans.push(Span::styled(
                        content[pos + 1..close].to_string(),
                        italic_style,
                    ));
                    spans.push(Span::styled("_".to_string(), dim));
                    plain_start = close + 1;
                    pos = close + 1;
                    continue;
                }
            }
        }

        // ---- Link [text](url)  /  Image ![alt](url) ----
        let is_image = b == b'!' && pos + 1 < content.len() && bytes[pos + 1] == b'[';
        if b == b'[' || is_image {
            let bracket_pos = if is_image { pos + 1 } else { pos };
            if let Some(rel_close) = content[bracket_pos + 1..].find(']') {
                let close_bracket = bracket_pos + 1 + rel_close;
                if close_bracket + 1 < content.len() && bytes[close_bracket + 1] == b'(' {
                    if let Some(rel_paren) = content[close_bracket + 2..].find(')') {
                        let close_paren = close_bracket + 2 + rel_paren;
                        let text = &content[bracket_pos + 1..close_bracket];
                        let url = &content[close_bracket + 2..close_paren];
                        if !text.is_empty() && !url.is_empty() {
                            flush_plain(&mut spans, content, plain_start, pos);
                            if is_image {
                                spans.push(Span::styled("!".to_string(), dim));
                            }
                            spans.push(Span::styled("[".to_string(), dim));
                            spans.push(Span::styled(text.to_string(), link_text_style));
                            spans.push(Span::styled("](".to_string(), dim));
                            spans.push(Span::styled(url.to_string(), url_style));
                            spans.push(Span::styled(")".to_string(), dim));
                            plain_start = close_paren + 1;
                            pos = close_paren + 1;
                            continue;
                        }
                    }
                }
            }
        }

        // ---- Inline code  `...`  ----
        if b == b'`' && pos + 1 < content.len() {
            if let Some(rel) = content[pos + 1..].find('`') {
                let end = pos + 1 + rel + 1; // byte after closing backtick
                flush_plain(&mut spans, content, plain_start, pos);
                spans.push(Span::styled(
                    content[pos..end].to_string(),
                    Style::default().fg(Color::Magenta),
                ));
                plain_start = end;
                pos = end;
                continue;
            }
        }

        // ---- Tag  #name  ----
        if b == b'#' {
            let prev_ok = pos == 0
                || content[..pos]
                    .chars()
                    .last()
                    .map_or(true, |c| c.is_whitespace());
            if prev_ok {
                if let Some(first) = content[pos + 1..].chars().next() {
                    if is_tag_name_start(first) {
                        let name_start = pos + 1;
                        let name_len = content[name_start..]
                            .find(|c: char| !is_tag_name_char(c))
                            .unwrap_or(content.len() - name_start);
                        let tag_end = name_start + name_len;
                        flush_plain(&mut spans, content, plain_start, pos);
                        spans.push(Span::styled(
                            content[pos..tag_end].to_string(),
                            Style::default().fg(Color::Green),
                        ));
                        plain_start = tag_end;
                        pos = tag_end;
                        continue;
                    }
                }
            }
        }

        // Advance one Unicode scalar
        pos += content[pos..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
    }

    if plain_start < content.len() {
        spans.push(Span::raw(content[plain_start..].to_string()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }

    Line::from(spans)
}

fn is_tag_name_start(c: char) -> bool {
    matches!(c, 'A'..='Z' | 'a'..='z' | '_' | '/' | '-')
}

fn is_tag_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '/' | '-')
}

// ---------------------------------------------------------------------------
// Decorated delimiter rendering
// ---------------------------------------------------------------------------

/// Split a delimiter line into (date+time text, optional title). The date may
/// appear on either side of the `|` separator; whichever side starts with
/// `YYYY-MM-DD` is treated as the date, the other side as the title. Returns
/// empty date / `None` title when neither half is recognised.
fn parse_delimiter_for_render(content: &str) -> (String, Option<String>) {
    let body = content.strip_prefix("===").unwrap_or("").trim();

    let (left, right) = match body.find('|') {
        Some(i) => (body[..i].trim(), Some(body[i + 1..].trim())),
        None => (body, None),
    };

    let left_is_date = looks_like_date_prefix(left);
    let right_is_date = right.map(looks_like_date_prefix).unwrap_or(false);

    let (date_text, title_text): (&str, &str) = if left_is_date {
        (left, right.unwrap_or(""))
    } else if right_is_date {
        (right.unwrap_or(""), left)
    } else {
        // No recognisable date in either half — pick a non-empty side as the
        // title (preferring the left, which is the new-format position).
        let title = if !left.is_empty() {
            left
        } else {
            right.unwrap_or("")
        };
        ("", title)
    };

    let title = if title_text.is_empty() {
        None
    } else {
        Some(title_text.to_string())
    };
    (date_text.to_string(), title)
}

/// True if `s` starts with a `YYYY-MM-DD` token at position 0.
fn looks_like_date_prefix(s: &str) -> bool {
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

/// Truncate `s` so its display width does not exceed `max`.
fn truncate_to_width(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Build a decorated delimiter line: a horizontal rule running edge-to-edge,
/// with the title (left) and date (right) "punched out" with 1-space gaps on
/// each side. Total width is exactly `width`.
///
/// Examples (W = total width):
/// - Both: `─ Title ─────────── Date ─`
/// - Title only: `─ Title ──────────────`
/// - Date only: `──────────────── Date ─`
/// - Bare: `─────────────────────────`
fn render_decorated_delimiter(content: &str, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    const MIN_MIDDLE_RULE: usize = 3;

    let (date_text, mut title_opt) = parse_delimiter_for_render(content);
    let date_w = UnicodeWidthStr::width(date_text.as_str());

    // Fixed cost of each text block (edge rule + space + text + space).
    let title_fixed = |t_w: usize| 1 + 1 + t_w + 1;
    let date_fixed = if date_w > 0 { 1 + date_w + 1 + 1 } else { 0 };

    let mut title_w = title_opt
        .as_ref()
        .map(|t| UnicodeWidthStr::width(t.as_str()))
        .unwrap_or(0);

    let required_middle = if title_w > 0 && date_w > 0 {
        MIN_MIDDLE_RULE
    } else {
        0
    };

    // Truncate title with `…` so the middle rule has at least `required_middle`
    // cells. If the title can't fit even at 1 char + ellipsis, drop it entirely.
    let mut current_title_fixed = if title_w > 0 { title_fixed(title_w) } else { 0 };
    if width.saturating_sub(current_title_fixed + date_fixed) < required_middle && title_w > 0 {
        let max_title_block = width.saturating_sub(date_fixed + required_middle);
        let max_title_w = max_title_block.saturating_sub(3);
        if max_title_w <= 1 {
            title_opt = None;
            title_w = 0;
            current_title_fixed = 0;
        } else {
            let trunc = truncate_to_width(title_opt.as_ref().unwrap(), max_title_w - 1);
            let new_title = format!("{trunc}\u{2026}");
            title_w = UnicodeWidthStr::width(new_title.as_str());
            title_opt = Some(new_title);
            current_title_fixed = title_fixed(title_w);
        }
    }

    let middle = width.saturating_sub(current_title_fixed + date_fixed);

    // `Modifier::DIM` is a portable SGR code (`2`) that asks the terminal to
    // render the foreground at reduced intensity. Combined with an indexed
    // grey, it works on truecolor terminals AND 256-colour fallbacks.
    let rule_style = Style::default()
        .fg(Color::Indexed(244))
        .add_modifier(Modifier::DIM);
    let title_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let date_style = Style::default().fg(Color::Cyan);

    let mut spans: Vec<Span<'static>> = Vec::new();

    if title_w > 0 {
        spans.push(Span::styled("\u{2500}".to_string(), rule_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(title_opt.unwrap(), title_style));
        spans.push(Span::raw(" "));
    }

    spans.push(Span::styled("\u{2500}".repeat(middle), rule_style));

    if date_w > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(date_text, date_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("\u{2500}".to_string(), rule_style));
    }

    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Incremental search helpers
// ---------------------------------------------------------------------------

/// Return all `(line_idx, byte_in_line, match_len_bytes)` occurrences of
/// `query` in the buffer (case-insensitive Unicode fold).
fn compute_matches(buffer: &TextBuffer, query: &str) -> Vec<(usize, usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    // Fold query to lowercase chars once.
    let q_chars: Vec<char> = query
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let mut out = Vec::new();

    for li in 0..buffer.line_count() {
        let raw = buffer.line_text(li).unwrap_or("");
        let content = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');
        // Collect (byte_offset, lowercased_char) for each char in this line.
        let chars: Vec<(usize, char)> = content
            .char_indices()
            .map(|(b, c)| (b, c.to_lowercase().next().unwrap_or(c)))
            .collect();
        if chars.len() < q_chars.len() {
            continue;
        }
        for i in 0..=chars.len() - q_chars.len() {
            let hit = chars[i..i + q_chars.len()]
                .iter()
                .map(|&(_, c)| c)
                .eq(q_chars.iter().copied());
            if hit {
                let byte_start = chars[i].0;
                let byte_end = if i + q_chars.len() < chars.len() {
                    chars[i + q_chars.len()].0
                } else {
                    content.len()
                };
                out.push((li, byte_start, byte_end - byte_start));
            }
        }
    }
    out
}

/// Return the index of the first match at or after `(cursor_line, cursor_byte)`,
/// wrapping around to 0 if no later match exists.
fn find_nearest_match(
    matches: &[(usize, usize, usize)],
    cursor_line: usize,
    cursor_byte: usize,
) -> usize {
    for (i, &(li, by, _)) in matches.iter().enumerate() {
        if li > cursor_line || (li == cursor_line && by >= cursor_byte) {
            return i;
        }
    }
    0
}

/// Apply search-match background highlights to an already-highlighted `Line`.
///
/// `matches` is a list of `(byte_start, byte_len, is_current)` regions
/// relative to the line content (newlines stripped).  Non-current matches
/// get a yellow background; the current match gets a bold green background.
fn overlay_search_matches(
    line: Line<'static>,
    matches: &[(usize, usize, bool)],
) -> Line<'static> {
    if matches.is_empty() {
        return line;
    }

    // Collect full text and per-span style ranges.
    let full_text: String = line.spans.iter().map(|s| s.content.as_ref() as &str).collect();
    let total = full_text.len();

    // Build sorted breakpoints at every span boundary and match boundary.
    let mut bp = std::collections::BTreeSet::new();
    bp.insert(0usize);
    bp.insert(total);
    {
        let mut byte = 0usize;
        for s in &line.spans {
            byte += s.content.len();
            bp.insert(byte);
        }
    }
    for &(start, len, _) in matches {
        let end = (start + len).min(total);
        if start < total { bp.insert(start); }
        bp.insert(end);
    }

    // Map byte ranges to their base styles from the original spans.
    let span_styles: Vec<(usize, usize, Style)> = {
        let mut v = Vec::new();
        let mut byte = 0usize;
        for s in &line.spans {
            v.push((byte, byte + s.content.len(), s.style));
            byte += s.content.len();
        }
        v
    };

    let bps: Vec<usize> = bp.into_iter().collect();
    let mut result: Vec<Span<'static>> = Vec::new();

    for w in bps.windows(2) {
        let (seg_start, seg_end) = (w[0], w[1]);
        if seg_start >= total { break; }
        if !full_text.is_char_boundary(seg_start) || !full_text.is_char_boundary(seg_end) {
            continue;
        }

        let base = span_styles
            .iter()
            .find(|&&(s, e, _)| s <= seg_start && seg_start < e)
            .map(|&(_, _, st)| st)
            .unwrap_or_default();

        let style = if let Some(&(_, _, is_current)) =
            matches.iter().find(|&&(mb, ml, _)| mb <= seg_start && seg_start < mb + ml)
        {
            let bg = if is_current { Color::LightGreen } else { Color::Yellow };
            let mut s = base.bg(bg);
            if is_current { s = s.add_modifier(Modifier::BOLD); }
            s
        } else {
            base
        };

        result.push(Span::styled(full_text[seg_start..seg_end].to_string(), style));
    }

    if result.is_empty() {
        result.push(Span::raw(String::new()));
    }
    Line::from(result)
}

/// Apply the `DIM` modifier to every span of `line`, preserving each span's
/// content and other style attributes. Used to dim notes that fall outside
/// the active filter.
fn dim_line(line: Line<'static>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            let new_style = s.style.add_modifier(Modifier::DIM);
            Span::styled(s.content.into_owned(), new_style)
        })
        .collect();
    Line::from(spans)
}

/// Overlay a selection-background style on the byte range `[start, end)` of
/// `line`, preserving the underlying foreground styles.
fn overlay_selection(line: Line<'static>, start: usize, end: usize) -> Line<'static> {
    if start >= end {
        return line;
    }

    let full_text: String = line.spans.iter().map(|s| s.content.as_ref() as &str).collect();
    let total = full_text.len();
    let lo = start.min(total);
    let hi = end.min(total);
    if lo >= hi {
        return line;
    }

    let mut bp = std::collections::BTreeSet::new();
    bp.insert(0usize);
    bp.insert(total);
    {
        let mut byte = 0usize;
        for s in &line.spans {
            byte += s.content.len();
            bp.insert(byte);
        }
    }
    bp.insert(lo);
    bp.insert(hi);

    let span_styles: Vec<(usize, usize, Style)> = {
        let mut v = Vec::new();
        let mut byte = 0usize;
        for s in &line.spans {
            v.push((byte, byte + s.content.len(), s.style));
            byte += s.content.len();
        }
        v
    };

    let bps: Vec<usize> = bp.into_iter().collect();
    let mut result: Vec<Span<'static>> = Vec::new();
    for w in bps.windows(2) {
        let (seg_start, seg_end) = (w[0], w[1]);
        if seg_start >= total {
            break;
        }
        if !full_text.is_char_boundary(seg_start) || !full_text.is_char_boundary(seg_end) {
            continue;
        }
        let base = span_styles
            .iter()
            .find(|&&(s, e, _)| s <= seg_start && seg_start < e)
            .map(|&(_, _, st)| st)
            .unwrap_or_default();
        let style = if seg_start >= lo && seg_start < hi {
            base.bg(Color::Rgb(60, 80, 110))
        } else {
            base
        };
        result.push(Span::styled(full_text[seg_start..seg_end].to_string(), style));
    }
    if result.is_empty() {
        result.push(Span::raw(String::new()));
    }
    Line::from(result)
}

// ---------------------------------------------------------------------------
// Todo helpers
// ---------------------------------------------------------------------------

/// Scan every line in `buffer` and return a `TodoItem` for each todo found.
fn collect_todos(buffer: &TextBuffer) -> Vec<TodoItem> {
    let mut items = Vec::new();
    for li in 0..buffer.line_count() {
        let raw = buffer.line_text(li).unwrap_or("");
        let content = raw.trim_end_matches(|c: char| c == '\n' || c == '\r');
        if let Some(rest) = content.strip_prefix("TODO:") {
            items.push(TodoItem {
                line: li,
                text: rest.trim_start().to_string(),
                is_done: false,
                kind: TodoItemKind::Prefix,
            });
        } else if let Some(rest) = content.strip_prefix("DONE:") {
            items.push(TodoItem {
                line: li,
                text: rest.trim_start().to_string(),
                is_done: true,
                kind: TodoItemKind::Prefix,
            });
        } else if let Some(rest) = content.strip_prefix("- [ ]") {
            items.push(TodoItem {
                line: li,
                text: rest.trim_start().to_string(),
                is_done: false,
                kind: TodoItemKind::Checkbox,
            });
        } else if content.starts_with("- [x]") || content.starts_with("- [X]") {
            items.push(TodoItem {
                line: li,
                text: content[5..].trim_start().to_string(),
                is_done: true,
                kind: TodoItemKind::Checkbox,
            });
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn line_display_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    fn line_to_plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn parse_delimiter_full() {
        let (meta, title) = parse_delimiter_for_render("=== 2026-05-13 14:32 | My title");
        assert_eq!(meta, "2026-05-13 14:32");
        assert_eq!(title.as_deref(), Some("My title"));
    }

    #[test]
    fn parse_delimiter_date_only() {
        let (meta, title) = parse_delimiter_for_render("=== 2026-05-13 14:32");
        assert_eq!(meta, "2026-05-13 14:32");
        assert!(title.is_none());
    }

    #[test]
    fn parse_delimiter_bare() {
        let (meta, title) = parse_delimiter_for_render("===");
        assert_eq!(meta, "");
        assert!(title.is_none());
    }

    #[test]
    fn parse_delimiter_empty_title() {
        let (meta, title) = parse_delimiter_for_render("=== 2026-05-13 |");
        assert_eq!(meta, "2026-05-13");
        assert!(title.is_none());
    }

    #[test]
    fn parse_delimiter_no_date() {
        let (meta, title) = parse_delimiter_for_render("=== | Title only");
        assert_eq!(meta, "");
        assert_eq!(title.as_deref(), Some("Title only"));
    }

    #[test]
    fn decorated_total_width_full() {
        let line = render_decorated_delimiter("=== 2026-05-13 14:32 | Hi", 80);
        assert_eq!(line_display_width(&line), 80);
        let s = line_to_plain(&line);
        assert!(s.contains("Hi"));
        assert!(s.contains("2026-05-13 14:32"));
        assert!(s.contains('\u{2500}'));
    }

    #[test]
    fn decorated_total_width_date_only() {
        let line = render_decorated_delimiter("=== 2026-05-13 14:32", 60);
        assert_eq!(line_display_width(&line), 60);
        let s = line_to_plain(&line);
        assert!(s.contains("2026-05-13 14:32"));
    }

    #[test]
    fn decorated_bare_full_rule() {
        let line = render_decorated_delimiter("===", 40);
        assert_eq!(line_display_width(&line), 40);
        let s = line_to_plain(&line);
        // Bare delimiter: full row of rule chars.
        let rule_chars = s.chars().filter(|&c| c == '\u{2500}').count();
        assert_eq!(rule_chars, 40);
        for ch in s.chars() {
            assert_eq!(ch, '\u{2500}');
        }
    }

    #[test]
    fn decorated_has_edge_rules_with_text() {
        // Both title and date present: row begins and ends with a rule char.
        let s = line_to_plain(&render_decorated_delimiter("=== 2026-05-13 14:32 | Hi", 80));
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        assert_eq!(first, '\u{2500}', "row should start with a rule char: {s:?}");
        assert_eq!(last, '\u{2500}', "row should end with a rule char: {s:?}");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn md_bold_double_star() {
        let line = highlight_inline("a **b** c");
        // Expect: "a ", "**", "b", "**", " c"
        assert_eq!(line.spans.len(), 5);
        assert_eq!(line.spans[0].content.as_ref(), "a ");
        assert_eq!(line.spans[1].content.as_ref(), "**");
        assert_eq!(line.spans[2].content.as_ref(), "b");
        assert_eq!(line.spans[3].content.as_ref(), "**");
        assert_eq!(line.spans[4].content.as_ref(), " c");
        assert_eq!(line_text(&line), "a **b** c");
    }

    #[test]
    fn md_italic_single_star() {
        let line = highlight_inline("a *b* c");
        assert_eq!(line.spans.len(), 5);
        assert_eq!(line.spans[1].content.as_ref(), "*");
        assert_eq!(line.spans[2].content.as_ref(), "b");
        assert_eq!(line.spans[3].content.as_ref(), "*");
    }

    #[test]
    fn md_strikethrough_double_tilde() {
        let line = highlight_inline("~~gone~~");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content.as_ref(), "~~");
        assert_eq!(line.spans[1].content.as_ref(), "gone");
        assert_eq!(line.spans[2].content.as_ref(), "~~");
    }

    #[test]
    fn md_link_pattern() {
        let line = highlight_inline("see [docs](https://example.com) for more");
        // "see ", "[", "docs", "](", "https://example.com", ")", " for more"
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            texts,
            vec!["see ", "[", "docs", "](", "https://example.com", ")", " for more"]
        );
    }

    #[test]
    fn md_image_pattern() {
        let line = highlight_inline("![alt](url)");
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["!", "[", "alt", "](", "url", ")"]);
    }

    #[test]
    fn md_unclosed_marker_stays_plain() {
        // No closing ** — treat as plain text.
        let line = highlight_inline("**foo with no end");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content.as_ref(), "**foo with no end");
    }

    #[test]
    fn md_is_hr_line() {
        assert!(is_hr_line("---"));
        assert!(is_hr_line("***"));
        assert!(is_hr_line("___"));
        assert!(is_hr_line("------"));
        assert!(is_hr_line("  ---  "));
        assert!(!is_hr_line("--"));
        assert!(!is_hr_line("- --"));
        assert!(!is_hr_line("==="));
        assert!(!is_hr_line("hello"));
        assert!(!is_hr_line(""));
    }

    #[test]
    fn md_blockquote_line() {
        let line = highlight_normal_line("> hello world");
        // First span is the "> " marker (yellow).
        assert_eq!(line.spans[0].content.as_ref(), "> ");
    }

    #[test]
    fn md_unordered_list_bullet() {
        let line = highlight_normal_line("- bullet item");
        // First span should be "- ", remainder inline-scanned.
        assert_eq!(line.spans[0].content.as_ref(), "- ");
    }

    #[test]
    fn md_unordered_list_does_not_catch_checkbox() {
        // `- [ ]` should still be handled as a checkbox, not as a list.
        let line = highlight_normal_line("- [ ] task");
        // Checkbox path emits "- [ ]" as the first span.
        assert_eq!(line.spans[0].content.as_ref(), "- [ ]");
    }

    #[test]
    fn md_ordered_list_prefix() {
        assert_eq!(ordered_list_prefix_len("1. foo"), Some(3));
        assert_eq!(ordered_list_prefix_len("42. bar"), Some(4));
        assert_eq!(ordered_list_prefix_len("1.foo"), None); // need space after .
        assert_eq!(ordered_list_prefix_len("a. foo"), None);
        assert_eq!(ordered_list_prefix_len("foo"), None);
    }

    #[test]
    fn complete_unique_prefix_adds_space() {
        // "fil" → "filter " (unique; trailing space)
        assert_eq!(complete_command_verb("fil").as_deref(), Some("filter "));
    }

    #[test]
    fn complete_no_match_returns_none() {
        assert!(complete_command_verb("zzz").is_none());
    }

    #[test]
    fn complete_common_prefix() {
        // "q" matches q, quit, q!, quit! — common prefix is just "q",
        // which equals the input, so no progress is possible.
        assert!(complete_command_verb("q").is_none());
        // "qu" matches "quit", "quit!" — common prefix "quit", longer than input.
        assert_eq!(complete_command_verb("qu").as_deref(), Some("quit"));
    }

    #[test]
    fn wrap_fits_no_break() {
        assert_eq!(wrap_segments("hello", 10), vec![(0, 5)]);
        assert_eq!(wrap_segments("hello world", 11), vec![(0, 11)]);
    }

    #[test]
    fn wrap_word_break() {
        // "hello world goodbye" with width 10 → "hello", "world", "goodbye"
        let segs = wrap_segments("hello world goodbye", 10);
        // Three visual rows; each segment trims to "hello", "world", "goodbye".
        let texts: Vec<&str> = segs
            .iter()
            .map(|&(s, e)| &"hello world goodbye"[s..e])
            .collect();
        assert_eq!(texts, vec!["hello", "world", "goodbye"]);
    }

    #[test]
    fn wrap_hard_break_long_word() {
        // Single word longer than width — hard break inside the word.
        let segs = wrap_segments("abcdefghij", 4);
        let texts: Vec<&str> = segs
            .iter()
            .map(|&(s, e)| &"abcdefghij"[s..e])
            .collect();
        assert_eq!(texts, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_preserves_trailing_word() {
        let segs = wrap_segments("foo bar baz qux", 7);
        let s = "foo bar baz qux";
        let texts: Vec<&str> = segs.iter().map(|&(a, b)| &s[a..b]).collect();
        assert_eq!(texts, vec!["foo bar", "baz qux"]);
    }

    #[test]
    fn normalise_line_endings_to_lf() {
        let out = normalise_line_endings("a\r\nb\rc\nd", "\n");
        assert_eq!(out, "a\nb\nc\nd");
    }

    #[test]
    fn normalise_line_endings_to_crlf() {
        let out = normalise_line_endings("a\nb\rc", "\r\n");
        assert_eq!(out, "a\r\nb\r\nc");
    }

    #[test]
    fn decorated_title_only_runs_to_end() {
        // Title but no date: row ends in a continuous rule (no spaces near right edge).
        let s = line_to_plain(&render_decorated_delimiter("=== | Just a title", 50));
        assert!(s.starts_with('\u{2500}'));
        assert!(s.ends_with('\u{2500}'));
        // The last several chars should all be rule chars (no date area).
        let tail: String = s.chars().rev().take(5).collect();
        for ch in tail.chars() {
            assert_eq!(ch, '\u{2500}');
        }
    }

    #[test]
    fn decorated_truncates_long_title() {
        let long = "A very long title that should not fit in a narrow terminal viewport";
        let line = render_decorated_delimiter(
            &format!("=== 2026-05-13 14:32 | {long}"),
            40,
        );
        assert_eq!(line_display_width(&line), 40);
        let s = line_to_plain(&line);
        assert!(s.contains('\u{2026}'), "expected ellipsis in: {s:?}");
        // At least MIN_RULE rule cells must remain.
        let rule_chars = s.chars().filter(|&c| c == '\u{2500}').count();
        assert!(rule_chars >= 3, "expected ≥3 rules, got {rule_chars}: {s:?}");
    }

    #[test]
    fn decorated_no_date_with_title() {
        let line = render_decorated_delimiter("=== | Title only", 50);
        assert_eq!(line_display_width(&line), 50);
        let s = line_to_plain(&line);
        assert!(s.contains("Title only"));
    }

    // ---- new title-first format ---------------------------------------------

    #[test]
    fn parse_delimiter_new_format() {
        // `=== Title | Date` — title on left, date on right.
        let (meta, title) = parse_delimiter_for_render("=== My title | 2026-05-13 14:32");
        assert_eq!(meta, "2026-05-13 14:32");
        assert_eq!(title.as_deref(), Some("My title"));
    }

    #[test]
    fn parse_delimiter_new_format_title_only() {
        // New-format draft: title typed, date not yet stamped.
        let (meta, title) = parse_delimiter_for_render("=== Just a title");
        assert_eq!(meta, "");
        assert_eq!(title.as_deref(), Some("Just a title"));
    }

    #[test]
    fn decorated_new_format_matches_old() {
        // Same logical note in old and new formats should render to the same plain text.
        let old = line_to_plain(&render_decorated_delimiter(
            "=== 2026-05-13 14:32 | Standup",
            60,
        ));
        let new = line_to_plain(&render_decorated_delimiter(
            "=== Standup | 2026-05-13 14:32",
            60,
        ));
        assert_eq!(old, new);
    }

}
