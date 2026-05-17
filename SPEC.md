# Notes App — Product Specification & Implementation Plan

## Context

A terminal-based personal notes application that stores everything in a plain text file with a custom structural syntax. Note bodies use Markdown. The app has a rich TUI editor, AI-assisted text polishing, tag-based organization, configurable auto-separation, and a powerful query language for filtering. Targeting Rust + Ratatui.

---

## 1. File Format Specification

Two layers:
- **Structural** — delimiter lines starting with `===`
- **Content** — Markdown inside note bodies

### 1.1 Delimiter syntax

```
=== [title] [| date [time]]        ← preferred (title-first)
=== [date [time]] [| title]        ← legacy (still parsed)
```

| Token | Meaning | Example |
|---|---|---|
| `YYYY-MM-DD` | Date (ISO 8601) | `2026-05-12` |
| `HH:MM` | Time, 24h (only valid immediately after a date) | `15:22` |
| `\| ...` | Pipe separator between title and date | `Morning standup \| 2026-05-12 09:14` |

Date/time format is always ISO: `2026-05-12` or `2026-05-12 15:22`. Not configurable.

**Dual-format parsing.** The parser identifies which side of the `|` holds the date by its fixed `YYYY-MM-DD` shape, regardless of position. Both orderings parse to the same `(date, time, title)` tuple. New entries created via the TUI (`===` Enter expansion) and the CLI (`--add`) are written in the title-first form; existing files in the legacy form keep working without migration.

### 1.2 In-body tokens

**Tags** — `#tagname` (no space; disambiguated from `# Heading` which requires a space):
```
#oci #helidon #michael
```

**Todos** — two equivalent syntaxes, the app supports both:

| Inline (anywhere) | List (in lists) |
|---|---|
| `TODO: Pay the bill` | `- [ ] Pay the bill` |
| `DONE: Pay the bill` | `- [x] Pay the bill` |

Toggling preserves the original syntax (`TODO:` ↔ `DONE:`, `- [ ]` ↔ `- [x]`). The `--todo` view shows both, mixed.

**Due dates** — optional `due:YYYY-MM-DD` token anywhere on a todo line:
```
TODO: Pay the bill due:2026-05-15
- [ ] Send proposal due:2026-05-20
```
Filter queries: `todo:overdue`, `todo:due-today`, `todo:due-week`, `due:<2026-05-15`, `due:2026-05-15`.

### 1.3 Sample file

```
=== Meeting notes | 2026-05-11
Lorem ipsum bla bla bla

## Follow-up items

TODO: Send proposal to Michael
- [ ] Review OCI deployment docs
- [x] Book the conference room

#oci #helidon #michael

=== Morning standup | 2026-05-12 09:14
Here is another note.
DONE: Verify last night's deploy
#tag1 #tag2
```

### 1.4 Rules

- A note runs from one `===` line to the next (or EOF).
- `===` lines are opening-only.
- `#tagname` (no space) = tag. `# Heading` (with space) = Markdown heading.
- `TODO:` / `DONE:` are uppercase, with colon, single-line — the rest of the paragraph is commentary, not part of the item.
- `- [ ]` / `- [x]` may be nested in lists; the task text is the first line only.
- File may begin without a leading `===` (implicit first note).
- Bare `===` is a valid manual separator.
- File extension: `.jot`.
- Markdown Setext H1 headings (`Title\n===`) are not supported inside note bodies. Use ATX headings (`# Heading`) instead — a bare `===` line is always interpreted as a note delimiter.

### 1.4.1 Tokenization rules

The parser must respect Markdown context when scanning for tags and todos:

- Tags and todos inside **fenced code blocks** (```` ``` ````) are ignored.
- Tags and todos inside **inline code** (`` `…` ``) are ignored.
- Tag charset: `[A-Za-z0-9_/-]+` with at least one non-digit character. Valid: `#oci`, `#helidon-oci`, `#project/oci`, `#v2`. Not valid: `#123` (avoids collision with issue references like "see #123").
- Tag matching is **case-insensitive**; display preserves the case of the first occurrence in the file.
- `#` followed by a space, end-of-line, or punctuation other than `_`/`/`/`-` is not a tag (`# Heading`, `URL#fragment`, `C#` are unaffected).

### 1.5 Date semantics

Dates in `=== Title | YYYY-MM-DD HH:MM` delimiters are **auto-stamped at creation** by the editor (TUI `===` Enter expansion) and CLI commands (`--add`, `--todo --add`). The system never invents a date — auto-stamps always reflect the current system time.

The TUI auto-expansion is title-aware:

| Line content before Enter | Line content after Enter |
|---|---|
| `===` | `=== YYYY-MM-DD HH:MM` |
| `=== Morning standup` | `=== Morning standup \| YYYY-MM-DD HH:MM` |
| `=== anything \| ...` (already has `\|`) | *(no expansion — manual edit in progress)* |
| `=== 2026-05-12 ...` (already has a date) | *(no expansion — idempotent)* |

Dates remain plain text and are **editable post-creation**. Backdating is intentionally low-friction: there is no separate "edit date" mode; the user simply edits the date span like any other text. This rare, deliberate operation is its own consent.

**File order does not have to match date order.** A user may insert a backdated entry anywhere in the file. Tools that present notes (filters, future TUI overlays) sort by **date, newest-first**, with file position as the tiebreaker. The file's spatial layout is the user's; the dates are the canonical ordering for queries.

---

## 2. Auto-Separation *(planned, not yet built)*

Configurable in `config.toml`. All modes independently toggleable.

| Mode | Default | Config key |
|---|---|---|
| Date boundary | on | `auto_sep.date = true` |
| Time gap | off | `auto_sep.time_gap = "2h"` |

Multiple conditions triggering simultaneously emit one combined delimiter: `=== 2026-05-12 15:22`. Always ISO format.

The config fields exist (defaults `date = true`, `time_gap = ""`) but no insertion logic is wired up yet. Today, new delimiters are produced explicitly via `===` Enter expansion in the TUI or `--add` in the CLI.

---

### Default file

When no file is specified via the CLI or `default_file` in config, the app falls back to:

| Platform | Default path |
|---|---|
| Windows | `%USERPROFILE%\Documents\jotstash\log.jot` |
| macOS | `~/Documents/jotstash/log.jot` |
| Linux | `$XDG_DOCUMENTS_DIR/jotstash/log.jot` (or `~/Documents/jotstash/log.jot`) |

The parent directory is auto-created on first use. The file itself is created on first save (the app opens an empty buffer if the file doesn't exist yet).

The first time the default is used, a one-line hint is printed to stderr so the user knows where their notes live. The user can move the file and set `default_file` in `config.toml` to override.

---

## 3. CLI Interface

```
jotstash [FILE]                            # Open TUI (default file in config)
jot --add "text"                         # Quick-add note, no TUI
jot --add --tags "oci,helidon" "..."     # Quick-add with tags
jot --todo                               # Interactive todo list (open only)
jot --todo --all                         # Include DONE items
jot --todo --add "task"                  # Quick-add a TODO (no TUI)
jot --todo --grep "phrase"               # Print matching todos only
jot --filter "<query>"                   # Print filtered notes to stdout
jot --file path.jot                      # All flags accept --file override
```

### `--todo` interactive view

```
$ jot --todo
1) [ ] Pay the bill            2026-05-11  #personal
2) [ ] Review PR               2026-05-12  #oci
3) [ ] Send proposal           2026-05-12  #oci
4) [x] Book the conference     2026-05-11  #oci #helidon  (hidden unless --all)
```

Interactive keys: `Space`/`Enter` toggle, `+` add new, `j`/`k` navigate, `q` quit. Each invocation re-numbers. Toggling is interactive-only; there is no scriptable `--done <n>` because indices are not stable across runs.

---

## 4. TUI Application

### 4.1 Layout

Current layout — full-screen editor with a one-line status bar at the bottom. Various overlays cover the editor when active (search, command bar, conflict dialog, todo overlay, AI panel, spell-check wizard).

```
┌──────────────────────────────────────────────────┐
│                                                  │
│  Editor — always shows the FULL file            │
│  (delimiter lines render as horizontal rules     │
│   with title-left, date-right)                   │
│                                                  │
├──────────────────────────────────────────────────┤
│  status / command bar                            │
└──────────────────────────────────────────────────┘
```

Mode-specific overlays:

- **Search** (`Ctrl+F`) — single-line input at the status bar; match highlights in the editor
- **Command bar** (`Ctrl+;` / `F10`) — single-line `:` palette at the status bar
- **Open prompt** (`Ctrl+O`) — single-line file-path input at the status bar
- **Todo overlay** (`:todo`) — full-screen list, navigation keys
- **AI panel** (`Ctrl+L` / `:ai`) — bottom-half panel: prompt input + body (original / candidate / spinner)
- **Spell wizard** (`F7` / `:spell`) — bottom-half panel: word + context + suggestion list
- **Conflict dialog** — centered modal on external file changes

The filter results overlay shown in earlier drafts is a **planned V1 feature** and not yet built (§5).

### 4.2 Editor & modal model

**Hybrid model — default (`editor.keybindings = "default"`)**

- Always in insert mode — type to edit immediately. No mode switching.
- `Ctrl+;` opens the command bar at the bottom of the screen.
- `Esc` / `Ctrl+G` cancels the command bar.
- `:` is **not** a special trigger in default mode — it types a literal colon (users write `TODO: text`, `Note: ...` constantly).

**Full vim mode (`editor.keybindings = "vim"`)**

- Standard Normal / Insert / Visual / Command modes.
- In Normal mode, `:` opens the command bar (standard vim behaviour).

Status bar shows current mode.

### 4.3 Keyboard shortcuts (default mode)

Only shortcuts that don't conflict with universal editing conventions are bound by default. Power features go through the command bar (`Ctrl+;`).

| Shortcut | Action |
|---|---|
| `Ctrl+S` | Save file |
| `Ctrl+F` | Freetext search overlay |
| `F3` / `Shift+F3` | Find next / previous match (within search overlay) |
| `Shift+Arrow / Shift+Home / Shift+End / Shift+PageUp/Down` | Extend selection; plain movement or `Esc` clears |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste via the system clipboard (cut/copy require a selection) |
| `Ctrl+L` | Open AI panel (Polish workflow) |
| `F7` | Spell check (selection if any, else current note; `:spell all` for whole file) |
| `Ctrl+O` | Open different file |
| `Ctrl+J` / `Ctrl+K` | Jump to next / previous note delimiter — when a filter is active, jumps across matches only |
| `Ctrl+P` | Open the filter panel (live picker) |
| `F4` / `Shift+F4` | Jump to next / previous filter match (when the panel is closed) |
| `F8` | Open the todo overlay |
| `Ctrl+Z` / `Ctrl+Y` | Undo / Redo |
| `Ctrl+;` / `F10` | Open command bar (`Tab` completes the verb) |
| `F1` | Help overlay (categorised shortcut + command cheat-sheet) |
| `Esc` | Close overlay / clear selection |
| `Mouse wheel` | Scroll the viewport (cursor stays put) |
| `Enter` on a line equal to `===` | Auto-expand to `=== YYYY-MM-DD HH:MM` (or with title: `=== Title \| date`) |

Defaults follow Windows conventions (`Ctrl+S` save, `Ctrl+Z` undo, `F3` find next, etc.).

**Removed from default shortcuts** (available via command bar instead):

| Feature | Command bar |
|---|---|
| AI polish | `:ai polish` |
| Todo overlay | `:todo` |
| Spell check | `:spell` |
| Tag browser | `:tags` |
| Date browser | `:dates` |

Shortcuts are configurable in `[bindings]` for users who want them back on keys.

**Portability fallbacks.** Some terminals don't deliver `Ctrl+;` reliably and `Ctrl+S` can be eaten by terminal flow control. The command bar is therefore always reachable via `F10` as well, and every command-bar action has a `:command` form. Users on affected terminals can rebind in `[bindings]`.

### 4.4 Visual styling

- **Delimiter lines: decorated horizontal rule.** A `===` line renders as a continuous horizontal rule (`─`, U+2500) spanning the full row width. The note's title (if present) sits on the left and the date/time on the right, each flanked by 1-space gaps so the rule visually punches through the text. Bare `===` (no metadata) renders as an uninterrupted full-width rule.
  - Decoration is a one-way display transform: the file content is unchanged, and cursor-to-byte positions stay 1:1.
  - When the cursor moves onto the delimiter row, or the row contains a current search match, the line falls back to its raw source form (`=== YYYY-MM-DD HH:MM | Title`) so editing and search highlighting are unaffected.
  - Long titles are truncated with `…` to preserve at least 3 rule cells between title and date.
- Tags (`#word`): green.
- `TODO:` bright yellow bold. `DONE:` dark gray.
- `- [ ]` / `- [x]`: rendered with proper checkbox visuals.
- `due:date` highlighted; overdue dates shown in red. *(planned)*
- **Markdown syntax highlighted in note bodies:**
  - ATX headings `# …` through `###### …` — yellow bold (whole line).
  - **`**bold**`** / `__bold__` — marker chars dim, body bold.
  - *`*italic*`* / `_italic_` — marker chars dim, body italic.
  - ~~`~~strike~~`~~ — marker chars dim, body crossed-out.
  - `[text](url)` / `![alt](url)` — bracket/paren punctuation dim, text underlined light-blue, URL dim gray.
  - `` `inline code` `` — magenta.
  - Fenced code blocks (` ``` ` or `~~~`) — whole block rendered dim.
  - `> blockquote` — `>` yellow, body italic.
  - Unordered list bullets `- `, `* `, `+ ` — bullet yellow, body inline-scanned.
  - Ordered list `N. ` — number+dot yellow, body inline-scanned.
  - Horizontal rule `---`, `***`, `___` (line of ≥3 identical chars) — full line dim.
  - Setext headings (`Title\n===`) are NOT supported — `===` is reserved for note delimiters.
- **Selection highlight**: selected byte range gets a dim blue background overlay (`Rgb(60, 80, 110)`), preserving the underlying foreground styles.
- **Word wrap**: when `editor.line_wrap = true` (default), long lines wrap at word boundaries within the viewport width; the cursor navigates by visual row, not file line. Wrap is fully cursor-aware — Up/Down step segments correctly.

### 4.5 Tag autocompletion *(planned, not yet built)*

- Typing `#` in the editor opens a fuzzy popup of existing tags.
- Tags are indexed at file load + maintained incrementally on edits.
- Ranking: recency (weighted) + frequency.
- As more chars typed, popup filters fuzzy.
- `Tab` / `Enter` accepts. `Esc` dismisses. Typing a space or unrelated char also dismisses (lets user type a brand-new tag).

---

## 5. Filtering & Search

**Current state:** TUI filter is **implemented as a live-picker panel** (aligned with the AI panel and Spell wizard patterns).

**Invocation.** `Ctrl+P` opens the panel; `:filter [query]` in the command bar opens it too (pre-filling the query if an arg is provided).

**Layout.** Bottom-half overlay: title bar with match count, query input row, separator, scrollable match list (`▸ date  title  #tags  snippet`), footer hints.

**Behaviour.**

- **Live query** — every keystroke in the input field re-runs the filter and rebuilds the match list.
- **Selection follows the cursor** — `↑` / `↓` (and `PgUp` / `PgDn`) move the selection AND move the editor cursor to the selected note's start. The main editor scrolls naturally; nothing is dimmed or otherwise altered.
- **`Enter`** — commit. Panel closes; the cursor stays where it is (on the selected note). Active filter is set to the current query.
- **`Esc`** — cancel. Panel closes; the cursor and scroll are restored to where they were when the panel opened. The active filter still gets set to the current query (closing the panel does not clear the filter).
- **Active-filter persistence** — the filter is set on every keystroke in the panel, mirrored to `self.active_filter`. It survives panel close. It's cleared only via `:clear`.

**Operators in v1:**

- `tag:foo` — note must have tag `foo`
- `tag:foo,bar` — note must have tag `foo` OR `bar` (OR within a comma-group)
- `tag:foo tag:bar` — note must have BOTH (AND across tokens)
- `title:keyword` — note's title contains `keyword`
- `date:2026-05-12` — note's date equals
- `date:2026-05` — note's date in May 2026 (prefix match)
- free text — case-insensitive substring match against note body

The same operators are available in CLI mode via `jot --filter "<query>"`. Results sort by date, newest-first; file position as tiebreaker.

**Auxiliary shortcuts** when a filter is active and the panel is closed:

- `Ctrl+J` / `Ctrl+K` — walk matches (instead of all delimiters).
- `F4` / `Shift+F4` — next / previous match.
- `:next` / `:prev` / `:clear` — same actions, via command bar.

The richer DSL described in §5.2 (`OR` across tokens, `-tag:foo` exclusion, `date:>X`/`date:<X`/ranges, `date:today`/aliases, `todo:`, `due:`) is V1+ work not yet built.

### 5.0 Model — filter is a navigation aid, not an editor mode

The main editor **always shows the full file**. Filtering never changes what is in the buffer or hides content from the editor — this guarantees that editing semantics (undo, save, cursor, line numbers, search-in-buffer) are unchanged regardless of filter state.

**`:filter <query>`** opens a **results overlay** (a picker pane). Each row shows date · title · tags · first-line snippet. Keys: `↑`/`↓` navigate, `Enter` closes the overlay and scrolls the editor to the chosen note, `Esc` closes without jumping.

**Active filter** is a global state, independent of whether the overlay is currently open. While a filter is active:
- The status bar shows `[filter: <query> (<n>)]` with the match count.
- `:next` / `:prev` jumps the cursor to the next/previous matching note in the editor.
- The todo overlay (`:todo`) is scoped to matching notes (§7.3).
- Re-opening the overlay with `:filter` (no args) re-shows the same result list.

**`:clear`** removes the active filter.

A v2 extension (out of scope) may add a `:follow-filter` toggle that visually dims non-matching notes in the editor; v1 keeps the editor view unaffected.

### 5.1 Query language

A learnable mini-DSL used in `:filter` and `--filter`.

### 5.2 Syntax

| Pattern | Meaning |
|---|---|
| `tag:oci` | Tag exactly |
| `tag:oci,helidon` | OR within field |
| `tag:oci tag:helidon` | AND across fields |
| `-tag:archived` | Exclude |
| `date:2026-05-12` | Specific date |
| `date:2026-05` | Month prefix |
| `date:-7` | Last 7 days (relative) |
| `date:>2026-05-01` | After |
| `date:<2026-05-01` | Before |
| `date:2026-05-01..2026-05-15` | Range |
| `todo:open` / `todo:done` / `todo:any` | Notes containing such todos |
| `todo:overdue` / `todo:due-today` / `todo:due-week` | Notes with todos by due-date state |
| `due:2026-05-15` / `due:<2026-05-15` / `due:>2026-05-15` | Filter on todo due-date |
| `title:Meeting` | Title contains |
| `text:"some phrase"` | Quoted = exact phrase |
| `text:word` | Unquoted = token |

### 5.3 Combinations

```
tag:oci date:-30 todo:open                   AND
tag:oci | tag:helidon                        OR
(tag:oci | tag:helidon) date:-30 -tag:dnu    Grouping + exclude
```

### 5.4 Reserved date aliases

| Alias | Meaning |
|---|---|
| `today` | Calendar today |
| `yesterday` | Calendar yesterday |
| `week` | This calendar week |
| `month` | This calendar month |

```
date:today
date:yesterday
date:week
```

---

## 6. Command Mode (`:` Palette)

Opened via `Ctrl+;` (or `F10` fallback) in default mode, or by typing `:` in Normal mode (vim, when built). Available commands:

**Implemented:**

| Command | Action |
|---|---|
| `:w` / `:write` | Save |
| `:q` / `:quit` | Exit (warns on unsaved changes) |
| `:q!` / `:quit!` | Exit without saving |
| `:wq` / `:x` | Save and exit |
| `:goto <n>` / `:go <n>` | Jump to line N |
| `:todo` | Open todo overlay |
| `:ai` | Open AI panel (Polish workflow, §8) |
| `:spell` | Run spell check on selection-or-current-note |
| `:spell all` | Run spell check on the whole file |
| `:filter <query>` | Run filter and open results overlay (§5) |
| `:filter` (no args) | Reopen overlay using the active filter |
| `:next` / `:prev` | Jump cursor to next / previous match in file order |
| `:clear` | Clear the active filter |
| `:help` | Open the help cheat-sheet overlay (same as `F1`) |
| `:set` | Reserved (no settings yet) |

**Planned (not yet wired):**

| Command | Action |
|---|---|
| `:save <name>` / `:load <name>` / `:filters` | Saved filters (§6.2) |
| `:tags` / `:dates` | Tag / date browser overlays |

### 6.1 Command history

- `↑` / `↓` in command bar cycles **session-only history** (implemented).
- Persistent cross-session history (`%LOCALAPPDATA%\jotstash\history` / `~/.local/state/jotstash/history`) and `Ctrl+R` reverse-search are **planned**.
- Last-applied filter persistence across sessions is part of the planned V1 filter overlay.

### 6.2 Saved filters + key bindings *(planned, not yet built)*

```toml
[filters]
today      = "date:today"
this-week  = "date:-7"
todos      = "todo:open"
oci-recent = "tag:oci date:-30"

[bindings]
"F2" = ":load todos"
"F3" = ":load oci-recent"
"F4" = ":load this-week"
```

Press `F2` → apply `todos` filter instantly. `:filters` lists everything.

---

## 7. Todo Management

### 7.1 Todo overlay (`:todo`)

Same content/keys as the `--todo` CLI view, embedded in the TUI. Numbered list, `Space`/`Enter` toggles, `+` adds new todo to current note context, `q` closes.

### 7.2 Toggle behavior

Toggling rewrites the relevant line in place (`TODO:` ↔ `DONE:` or `- [ ]` ↔ `- [x]`).

**TUI mode** — toggling modifies the in-memory buffer and marks the file dirty. It is undoable via `Ctrl+Z` and saved by the normal save path (`Ctrl+S`, `:w`, `:wq`). No implicit save.

**CLI mode** — `jot --todo` (interactive) and `jot --todo --add` perform atomic read-modify-write per toggle, with conflict detection per §9.3.

### 7.3 Filtered todos

The todo overlay respects the active filter (§5.0). With `:filter tag:oci` applied, the overlay only shows todos contained in matching notes. The main editor remains unaffected — it always shows the full file.

---

## 8. AI Integration

### 8.1 Interaction — the prompt loop

The user's workflow is iterative: write a prompt, see a candidate, adjust the prompt, regenerate, repeat until happy. The UI centres on **prompt → result → adjust → regenerate**, with accept/discard at the end. There is no fixed menu of "actions" — the prompt is the action.

**Invoke** with `Ctrl+L` or `:ai`. A panel opens at the bottom; the editor above is dimmed and read-only while the panel is active. The last prompt used in any session is pre-filled (persisted in `state.toml`).

**Scope.** The AI call operates on the **current note's body** (the bytes between the cursor's `===` line and the next `===`/EOF, excluding the delimiter lines). No text-selection feature is required for v1.

**Keys inside the panel:**

| Key | Action |
|---|---|
| Type | Edit the prompt |
| `Enter` | Submit the prompt; generate a candidate |
| `Ctrl+Enter` | Accept the candidate: replace the note body, close the panel |
| `Tab` | Toggle between viewing the candidate and the original |
| `Esc` | Cancel in-flight request; with no request, close the panel |
| `Up` / `Down` | Scroll the candidate pane |

While a request is in flight, a spinner runs in the body area. `Esc` cancels (the worker thread is abandoned; its result is discarded).

Accept is a single undo entry: `Ctrl+Z` after accept restores the original body in one step.

### 8.2 Backends

| Backend | `provider` value | Status | Required |
|---|---|---|---|
| OpenAI | `"openai"` | **implemented** | `api_key_env`, `base_url`, `model` |
| Ollama | `"ollama"` | **implemented** | `base_url`, `model` (no `api_key_env`) |
| Anthropic Claude | `"anthropic"` | deferred | `api_key_env`, `model` |

OpenAI and Ollama share one provider implementation since Ollama exposes an OpenAI-compatible `/v1/chat/completions` endpoint. The wire format is identical; the only difference is whether an `Authorization: Bearer` header is sent.

API keys are read from environment variables (named by `api_key_env`) at request time, never hardcoded. For Ollama, `api_key_env` is omitted from config — no auth header is sent.

---

## 9. File Persistence & Sync-Friendliness

### 9.1 Atomic writes

- All saves write a full new file via temp file + atomic rename.
- No partial writes on crash. No lock files. No `.swp`-style sidecars.

### 9.2 Git-friendly diffs

- Unchanged text is preserved exactly — no reformatting, no whitespace changes, no line reordering.
- Line endings preserved (LF stays LF, CRLF stays CRLF).
- A todo toggle changes only the relevant line's content; the rest of the file is written verbatim. Git diffs therefore show only the modified lines.

### 9.3 Conflict detection (optimistic concurrency)

No lock files, but the app detects external changes before overwriting:

1. **On open**: record the file's `mtime` and content hash.
2. **Before every save**: re-check `mtime`/hash against disk.
3. **If changed**: block the save and present three options:
   - **Reload** — discard in-memory edits, load the new file.
   - **Save copy** — write the in-memory version to `filename.conflict.jot`.
   - **Overwrite** — explicit opt-in to clobber the external change.
4. **`--add` and interactive `--todo` toggles**: always re-read the file immediately before writing, never operate on a stale buffer. If the file changed between read and write, retry once then warn.

### 9.4 Cursor position memory

- Per-file cursor position stored in `%LOCALAPPDATA%\jotstash\state.toml` (Windows) / `~/.local/state/jotstash/state.toml` (Unix).
- Restored on file open.
- Per-file last-applied filter stored alongside (separate from command history).

### 9.5 GitHub-backed sync

**Premise.** Many users keep a single notes file in sync across multiple machines (work laptop, home desktop, etc.). Where cloud storage (Dropbox, iCloud) is blocked by corporate IT, Git over HTTPS to GitHub is usually allowed because it's developer-essential. The app therefore offers transparent push/pull on a notes file that lives inside a git working tree.

**Mechanism.** The app shells out to the `git` CLI, inheriting the user's existing credential setup (Credential Manager on Windows, SSH agent, `gh auth`, etc.). No in-app authentication UI.

**Operations:**

- **Open** — if the file is inside a git repo with a remote, run `git pull --rebase --no-edit` (blocking, 5s timeout). On timeout or network failure, fall back to local content with the status bar showing `offline`.
- **Save** — after the atomic write succeeds, spawn a background `git add` + `git commit -m "notes: <timestamp>"` + `git push`. Status bar shows `↑…` then `✓` on success, or `↑N` if push fails but commits are queued locally.
- **Idle pull** — every `idle_pull_interval` (default 5 min), pull in the background to surface changes from other machines.
- **`:sync`** — manual trigger for pull+push, useful after an offline period.

**Conflict handling.** If a pull produces merge conflicts, the file contains `<<<<<<<` markers. The app opens a yellow conflict modal with three options:

- **[K] Keep local** — discard the remote's conflicting hunks (`git checkout --theirs`, continue, push)
- **[R] Take remote** — discard local hunks (`git checkout --ours`, continue, push)
- **[E] Edit manually** — close the dialog with the markers in the buffer; run `:sync` once resolved

**Status bar indicators:**

| State | Indicator |
|---|---|
| Disabled (not in a git repo) | (none) |
| Idle | `✓` |
| Pulling | `↓…` |
| Pushing | `↑…` |
| Ahead of remote | `↑N` |
| Offline | `offline` |
| Conflict | `conflict` |
| Error | `sync: <message>` |

**Config (`[sync]`):**

```toml
[sync]
enabled = true                   # auto-detected if file is in a git repo
pull_on_open = true
push_on_save = true
idle_pull_interval = "5m"        # "0" disables
```

**Requirements:**
- `git` CLI installed on the machine.
- The notes file lives inside a `git clone` of a GitHub (or any) repo.
- The user has set up git auth for that remote at least once (the app never prompts for credentials).

**Out of scope:**
- Auto-`git init` / repo creation.
- Branch switching.
- Stashing / partial commits.

---

## 10. Spell Check

Invoked **on-demand** via `:spell` or `F7`. No live checking, no inline marks in the editor, no auto-correct.

### 10.1 Scope

- **Active selection**, if any (`Shift+Arrows` etc. before invoking).
- Otherwise the **current note's body** (between `===` and the next `===`/EOF, exclusive of the delimiter lines).
- `:spell all` runs over the whole file.

### 10.2 Wizard UX

A bottom-half overlay walks misspellings one at a time. Each step shows: the misspelled word, a context line with the word highlighted, and up to five frequency-ranked suggestions.

| Key | Action |
|---|---|
| `↑` / `↓` | Pick a suggestion |
| `Enter` | Apply selected suggestion |
| `1`–`5` | Direct-select suggestion N (and apply) |
| `s` | Skip this word for the rest of the session |
| `a` | Add the word to the user dictionary |
| `Esc` | Quit the wizard |

Each fix is a **single undo entry**. After fix/skip/add the wizard **auto-advances** to the next misspelling. The session-skipped set is cleared when the wizard closes.

**Case preservation** — replacements match the case pattern of the original (`Helo` → `Hello`; `HELO` → `HELLO`; `helo` → `hello`).

### 10.3 Tokenisation

The scanner skips:

- `===` delimiter lines
- Fenced code blocks (` ``` … ``` ` and `~~~ … ~~~`)
- Inline code (`` `…` ``)
- URLs (`http://`, `https://`, `ftp://`, `file://`)
- `#tags` (whole tag including the `#`)
- `TODO:` / `DONE:` keywords and `- [ ]` / `- [x]` checkbox prefixes
- Identifiers — words containing `_`, `/`, `.`, digits (e.g. `foo_bar`, `path/to/x`, `v2`)
- CamelCase / PascalCase tokens
- Words shorter than 2 letters

Apostrophes inside words (`don't`, `it's`) are preserved.

### 10.4 Backend

**`symspell`** (Wolf Garbe's algorithm, pure Rust). Frequency-ranked suggestions, fast lookup, no native dependencies.

Dictionary source: `https://raw.githubusercontent.com/wolfgarbe/SymSpell/master/SymSpell/frequency_dictionary_<lang>_*.txt`. Downloaded on first use of `:spell` after explicit user confirmation. Cached at:

- `%APPDATA%\jotstash\dictionary_en.txt` (Windows)
- `~/.config/jotstash/dictionary_en.txt` (XDG)

### 10.5 User dictionary

Words added via `a` are appended to:

- `%APPDATA%\jotstash\dictionary.txt` (Windows)
- `~/.config/jotstash/dictionary.txt` (XDG)

Plain text, one lowercase word per line. The user can edit this file by hand. Loaded once at the start of each spell session.

### 10.6 Language

```toml
[spell]
language = "en"
```

Only `en` is supported in MVP. Architecture supports adding more languages (Spanish, German, Russian, etc. — all dictionaries are available from the same SymSpell repo); each adds a URL to the language → URL map.

---

## 11. Configuration

`%APPDATA%\jotstash\config.toml` (Windows) / `~/.config/jotstash/config.toml`.

```toml
default_file = "C:/Users/user/notes/log.jot"

[editor]
keybindings = "default"     # "default" (hybrid) or "vim" (vim not yet built)
line_wrap = true
tab_width = 4

[auto_sep]
date = true                  # not yet built
time_gap = ""                # not yet built; "2h", "30m" etc.

[ai]
# OpenAI (default)
provider = "openai"
model = "gpt-4o-mini"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

# Or, for local Ollama / LM Studio (OpenAI-compatible):
# provider = "ollama"
# model = "llama3.2"
# base_url = "http://localhost:11434/v1"
# (api_key_env omitted → no auth header sent)
#
# provider = "lmstudio"
# model = "<model loaded in LM Studio>"
# base_url = "http://localhost:1234/v1"

[spell]
language = "en"             # only "en" supported in MVP

# Planned (not yet wired):
# [filters]
# today     = "date:today"
# this-week = "date:-7"
#
# [bindings]
# "F2" = ":load todos"
```

State (per-file cursor memory + last AI prompt) lives separately at `state.toml` in the same directory, managed by `src/state.rs`.

---

## 12. Tech Stack (Rust)

| Concern | Crate |
|---|---|
| TUI framework | `ratatui` 0.28 (`crossterm` feature) |
| Terminal backend | `crossterm` 0.28 (`bracketed-paste` feature) |
| System clipboard | `arboard` 3 |
| Config / state | `toml` 0.8 + `serde` 1 |
| HTTP (AI) | `reqwest` 0.12 (`blocking`, `json`, `rustls-tls`) |
| JSON (AI) | `serde_json` 1 |
| Async / concurrency | `std::thread` + `std::sync::mpsc` (no tokio runtime) |
| Date/time | `chrono` 0.4 |
| Spell check | `symspell` 0.4 |
| Unicode display width | `unicode-width` 0.1 |
| CLI args | `clap` 4.5 (`derive`) |
| Config dirs | `dirs` 5 |
| Error context | `anyhow` 1 |

---

## 13. Project Structure

**Principle:** the file text is the source of truth. Notes, tags, todos, and filter matches are *indexes over spans into the buffer*, not a separate model. Edits update the buffer; indexes are derived.

```
jotstash/
├── src/
│   ├── main.rs              # CLI entry; dispatches to commands::* or App::run
│   ├── app.rs               # TUI app state machine, render, key handling, all modes
│   ├── buffer.rs            # TextBuffer: content String + LineIndex
│   ├── spans.rs             # Span (byte range), LineIndex, NoteSpan/TagSpan/TodoSpan
│   ├── parser.rs            # text → spans (notes, tags, todos, delimiters; dual-format)
│   ├── index.rs             # FileIndex wrapping ParseResult; accessors over spans
│   ├── persistence.rs       # Atomic write, mtime/hash, conflict detection
│   ├── commands.rs          # --add, --filter, --todo CLI handlers
│   ├── ai.rs                # AiProvider trait + OpenAiCompatProvider (OpenAI + Ollama)
│   ├── spell.rs             # symspell wrapper, dictionary download, tokeniser
│   ├── state.rs             # Per-file cursor memory + last AI prompt → state.toml
│   ├── date.rs              # ISO date/datetime formatting + parsing helpers
│   └── config.rs            # Config + sub-configs loaded from config.toml
├── Cargo.toml
└── SPEC.md
```

Modules planned but not yet split out (currently inlined or pending implementation):

| Future module | Status |
|---|---|
| `query.rs` (filter DSL) | Inlined in `commands.rs` as a minimal `tag:`/text matcher; full DSL not yet built |
| `editor.rs`, `commandbar.rs`, `search.rs`, `todo.rs` | Inlined in `app.rs` as mode states + render/handle methods |
| `ai/anthropic.rs` | Anthropic provider not yet implemented (deferred per §8.2) |
| `filters.rs` (saved filters) | Not yet built (V1 feature) |

---

## 14. Decisions Log

| Decision | Choice |
|---|---|
| File extension | `.jot` |
| Note delimiters | Opening-only `===`; preferred form is title-first (`=== Title \| Date`); legacy date-first parses transparently via dual-format parser keyed off the `YYYY-MM-DD` shape |
| Date on delimiter | Fixed ISO format: `2026-05-12` or `2026-05-12 15:22` |
| Date semantics | Auto-stamped at creation (TUI `===` Enter, CLI `--add`); editable post-creation as plain text; system never invents a date |
| File order vs. date order | Independent — backdated insertions allowed anywhere; tools that present notes sort by date, file position as tiebreaker |
| Decorated delimiter render | `===` lines display as a continuous `─` rule with title-left, date-right, dim grey rule (DIM modifier); raw form when cursor is on the row or it has a search match |
| Tags | `#tagname` (no space); charset `[A-Za-z0-9_/-]+` with ≥1 non-digit |
| Todos | Both `TODO:`/`DONE:` (inline) and `- [ ]`/`- [x]` (list) supported; toggle preserves syntax |
| Editor model | Hybrid: always-insert + `Ctrl+;` palette (default); full vim is a planned alternative |
| Selection | `Shift+Arrow`/`Home`/`End`/`PageUp/Down` anchor-based; replace-on-type; single undo per replace |
| Clipboard | System clipboard via `arboard`; bracketed paste enabled; burst-event detection groups paste into one undo entry |
| Word wrap | Word-aware wrap at viewport width; cursor moves by visual row when wrap is on; controlled by `editor.line_wrap` |
| Shortcuts | Windows conventions (Ctrl+S/Z/Y/C/X/V, F3 find next, F7 spell check); AI/todo/spell/tags/dates available via `:` command bar; some also bound to F-keys |
| Filter language | Full query DSL planned (§5.2); current CLI implementation is the minimal `tag:` + text subset |
| Filter sort | Date, newest-first; file position as tiebreaker; malformed/missing dates sort last |
| Filter UX | Live-picker panel (`Ctrl+P` or `:filter`): query input + results list. Selection follows the cursor in the editor. `Enter` commits, `Esc` restores. Editor view is never dimmed or hidden. Closing the panel does not clear the active filter. |
| TODO CLI indexing | Interactive-only toggling; no scriptable `--done <n>` (indices aren't stable across runs) |
| Spell check | On-demand via `:spell` or `F7`; selection-first scope, else current note; whole-file via `:spell all`; `symspell` engine; dictionary downloaded on first use with explicit user confirmation; user dictionary at `~/.config/jotstash/dictionary.txt` |
| AI backends | OpenAI + Ollama (OpenAI-compatible, single provider impl) shipped; Anthropic deferred. Trait `AiProvider` allows additions without restructuring |
| AI UX | Prompt-edit-regenerate loop (not a fixed action menu); last prompt persisted; Tab toggles original/candidate; current-note default scope, falls back to selection if present |
| Tag autocomplete | Fuzzy popup on `#` keystroke (planned) |
| Due dates | `due:YYYY-MM-DD` token on todo lines (planned) |
| Persistence | Atomic writes (temp + rename); mtime+hash conflict detection; per-file cursor memory in `state.toml`; line-endings preserved |
| Templates | Skipped for v1 |
| Session feature | Removed — auto-separation handled by date + time-gap only |

---

## 15. Implementation Plan — MVP-0, MVP-1, V1

The build is split into four milestones. **MVP-0** is a CLI-only tool that validates the file format and capture workflow before any editor work begins. **MVP-1** adds the TUI editor. **V1** delivers the full organizational feature set. **Post-V1** captures everything deferred.

The key principle is **buffer-first, span-first**: the text buffer with span-based indexes is built before parsing produces domain objects, and the editor is built on top of that infrastructure — not the other way around. This avoids the trap of having parsing, highlighting, todo toggling, and cursor restoration fight each other over different representations of the same text.

**Current state at a glance:**

| Milestone | Status |
|---|---|
| MVP-0 — CLI capture & query | ✅ Complete |
| MVP-1 — TUI editor | ✅ Complete |
| Post-MVP-1 polish | ✅ Decorated delimiter rendering, title-first format, selection, clipboard, mouse, word wrap, AI panel, spell check, F3 find next/prev |
| V1 — Organizing notes well | 🟡 Partial (filter is CLI-only; tag autocomplete, saved filters, browsers not yet built) |
| Post-V1 | ⬜ Anthropic provider, vim mode, multi-language spell, templates, etc. |

### 15.1 MVP-0 — "CLI-first capture & query"

**Goal.** Validate the `.jot` file format and capture workflow without sinking time into a terminal text editor.

**Build order:**

| # | Phase | Deliverable |
|---|---|---|
| 1 | Skeleton | Cargo project, `clap` CLI parsing, `toml`/`serde` config load, `chrono` date utils |
| 2 | Text model & spans | `TextBuffer` (byte/line mapping), `Span` (byte ranges into source). All later features address text via spans into the live buffer |
| 3 | Parser / indexer | text → spans for notes, delimiters, tags, todos, due dates; golden-file tests including code-block and inline-code exclusion (§1.4.1) |
| 4 | Persistence | Atomic write (temp + rename), mtime/hash capture, conflict-detection state machine |
| 5 | CLI | `--add`, `--filter` (simple `tag:` + `text:` only — no DSL yet), `--todo` interactive |

**Out of MVP-0:** anything full-screen. No TUI, no editing of existing notes — only append (`--add`) and interactive todo toggling.

**Done criteria:** the author has used MVP-0 for daily capture for 2 weeks without breaking changes to the file format.

---

### 15.2 MVP-1 — "TUI editor"

**Goal.** Bring the editor online on top of the stable buffer/index model from MVP-0.

**Build order:**

| # | Phase | Deliverable |
|---|---|---|
| 6 | TUI shell | ratatui + crossterm bootstrap; render full file; status bar |
| 7 | Editor buffer ops | Insert/delete, cursor movement, line wrap, Unicode handling — no undo yet |
| 8 | Save / open / dirty state | `Ctrl+S`, `Ctrl+O`, dirty flag; conflict-detection dialog from §9.3 |
| 9 | Undo / redo | `Ctrl+Z`, `Ctrl+Y` |
| 10 | Highlighting | Delimiters, `#tag`, `TODO:`/`DONE:`, `- [ ]`/`- [x]`, basic Markdown |
| 11 | Search overlay | `Ctrl+F` incremental find |
| 12 | Command bar | `Ctrl+;` (and `F10` fallback) opens it; supports `:w`, `:q`, `:wq`, `:goto`, `:set`, `:todo` |
| 13 | Todo overlay | TUI overlay scoped to current buffer; toggle marks buffer dirty (§7.2) |
| 14 | Cursor memory + note navigation | `state.toml` per-file cursor; `Ctrl+J` / `Ctrl+K` |

**Done criteria:** author switches from MVP-0 CLI workflow to the TUI editor for daily use for 2 weeks.

---

### 15.3 V1 — "Organizing notes well"

Everything in MVP-1 plus the organizational and search features. **No AI, no vim, no spell check** — those are deliberately deferred.

| Area | Adds on top of MVP-1 |
|---|---|
| Filter language | Full query DSL (§5.1–5.4): AND/OR/grouping/exclude/dates/tags/todos/due/text |
| Filter UX | Model A results overlay; `:filter`, `:next`, `:prev`, `:clear`, `:filter` (no args) |
| Tag autocomplete | Fuzzy popup on `#`, ranked by recency + frequency |
| Todo overlay | Interactive in TUI; `+` to add inline; respects active filter |
| Todo CLI | `--todo --all`, `--todo --grep "phrase"` |
| Due-date filtering | `todo:overdue`, `todo:due-today`, `todo:due-week`, `due:<date>` etc. |
| Auto-separation | Time-gap mode (`auto_sep.time_gap = "2h"`) |
| Command history | Persistent history file + `Ctrl+R` reverse search |
| Saved filters | `[filters]` config + `[bindings]` to F-keys |
| Browsers | `:tags` and `:dates` overlays |
| Status bar | Full functionality (active filter, match count, dirty flag) |

**Done criteria:** every behaviour described in §1–§14 *except* AI (§8), spell check (§10), and vim mode (§4.2 "Full vim mode") is implemented and covered by tests where applicable.

---

### 15.4 Post-V1

| Feature | Notes |
|---|---|
| **AI: Anthropic provider** | OpenAI + Ollama shipped (one OpenAI-compat impl); Anthropic uses a different wire shape (`POST /v1/messages`, `x-api-key`, `content[0].text`) — slots cleanly behind the existing `AiProvider` trait. |
| **AI: streaming responses** | Currently block-and-spinner; streaming tokens as they arrive is a UX upgrade not yet built. |
| **AI: prompt presets library** | `[ai.presets]` table + quick-pick keys in the panel. Discussed; deferred. |
| **Vim mode** (§4.2) | Full Normal/Insert/Visual/Command modes; `editor.keybindings = "vim"`. |
| **Spell: multi-language** | Architecture supports it; need URL map + tokeniser tweaks for non-Latin scripts. |
| **Spell: better dictionary / Unicode strategy** | `symspell` ASCII strategy limits coverage; switch to `UnicodeStringStrategy` and/or augment with a names list. |
| **Configurable keybindings** | Proper `[bindings]` section with action enum + key-combo parser. |
| Templates | `--template meeting` from `~/.config/jotstash/templates/`. |
| `:follow-filter` | Visual dim of non-matching notes inline. |
| Export to standard Markdown | Convert `.jot` → portable `.md`. |
| Pin / star notes | Mark notes for quick access. |
| Note archive concept | Mark whole notes as inactive. |
| Multi-file workspace | Navigate across multiple `.jot` files. |

---

### 15.5 Test strategy

- **Parser / tokenization**: golden-file tests — input `.jot` strings → expected spans and round-trip serialization. Cover tags in code blocks, inline code, headings, issue numbers (`#123`), `C#`, slash/dash tags (§1.4.1).
- **Editor buffer**: insert/delete, undo/redo, cursor movement across Unicode (combining chars, wide chars), line-ending preservation.
- **Span stability**: re-index after edits before / inside / after a note; toggle a todo by span and verify the span still resolves.
- **Persistence / conflicts**: clean save; save dirty buffer after external change (TUI conflict dialog); `--add` during external change (retry-once path); interactive `--todo` toggle during external change.
- **Filter**: table-driven query → matching note IDs against a fixture file, plus negative tests for malformed queries (good error UX matters).
- **CLI**: integration tests via `assert_cmd` against tempdir-scoped notes files.
- **TUI**: where feasible, snapshot-test ratatui rendering with `insta`; otherwise manual checklist per release.
