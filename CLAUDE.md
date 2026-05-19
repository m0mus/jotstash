# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```pwsh
cargo build              # debug build
cargo build --release    # release build
cargo test               # run all tests
cargo test <module>      # e.g. cargo test commands::tests
cargo run -- --help      # see CLI surface
cargo run -- --add "note text" --file path/to/log.jot
cargo run -- --filter "tag:rust kubernetes" --file path/to/log.jot
cargo run -- --todo --file path/to/log.jot
cargo run -- --file path/to/log.jot   # open in TUI
```

Note: on a fresh PowerShell session, prepend `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"` if `cargo` is not found.

## Architecture

The file text is the **source of truth**. Notes, tags, todos, and filter matches are indexes over spans into a `TextBuffer`, not a separate model.

| Module | Role |
|---|---|
| `buffer.rs` | `TextBuffer` — owns the content `String` + `LineIndex` |
| `spans.rs` | `Span` (byte range), `LineIndex` (byte↔line mapping), domain span types (`NoteSpan`, `TagSpan`, `TodoSpan`) |
| `parser.rs` | text → `ParseResult` (NoteSpan/TagSpan/TodoSpan with note_idx) |
| `index.rs` | `FileIndex` wrapping `ParseResult`; accessors for todos, tags, notes |
| `persistence.rs` | Atomic write (temp + rename), mtime/hash, conflict detection |
| `commands.rs` | `--add`, `--filter`, `--todo` (interactive + grep + add) |
| `config.rs` | `Config` loaded from `%APPDATA%\jotstash\config.toml` (Windows) or `~/.config/jotstash/config.toml` |
| `date.rs` | ISO date/datetime formatting and parsing helpers |
| `main.rs` | CLI entry: dispatches to `commands::*` |

## Implementation Status

MVP-0 (CLI-first capture & query): **COMPLETE** (86 tests)
- [x] Phase 1: Skeleton (Cargo, clap, config, chrono)
- [x] Phase 2: Text model & spans (TextBuffer, Span, LineIndex, domain span types)
- [x] Phase 3: Parser / indexer (text → spans, golden-file tests)
- [x] Phase 4: Persistence (atomic write, mtime/hash conflict detection)
- [x] Phase 5: CLI commands (--add, --filter, --todo interactive/grep/add)

MVP-1 (TUI editor): **COMPLETE**
- [x] Phase 6: TUI shell — ratatui + crossterm, render full file, status bar
- [x] Phase 7: Editor buffer ops — insert/delete, cursor (unicode-width), Ctrl+S save
- [x] Phase 8: Save/open/dirty state — Ctrl+S, Ctrl+O, conflict-detection dialog
- [x] Phase 9: Undo/redo — Ctrl+Z, Ctrl+Y (word-level batching, hash-based dirty tracking)
- [x] Phase 10: Syntax highlighting — delimiters, tags, todos, Markdown, fenced code
- [x] Phase 11: Search overlay — Ctrl+F incremental find (yellow/green bg highlights, Enter to advance, Esc to close)
- [x] Phase 12: Command bar — Ctrl+; / F10, :w :q :q! :wq :goto :set :todo (Up/Down history)
- [x] Phase 13: Todo overlay — :todo opens full-screen list, Space/Enter toggles (undo-able, marks dirty), j/k navigate, a shows all
- [x] Phase 14: Note navigation — Ctrl+J/Ctrl+K jump between note delimiters (cursor opens at end-of-doc)

## File Format

Delimiter: `=== YYYY-MM-DD [HH:MM] [| Title]` — opening-only.  
Tags: `#tagname` (no space; `# Heading` with space is Markdown).  
Todos: `TODO: text` / `DONE: text` (inline) or `- [ ] text` / `- [x] text` (checkbox).  
See `SPEC.md` for full format specification.
