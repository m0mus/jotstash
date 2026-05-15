# jotlog

> A terminal-based, plain-text, captain's-log notes app — Markdown-aware,
> AI-assisted, spell-checked, and GitHub-synced. Single binary, no daemon,
> no cloud account required.

```
─ Morning standup ──────────────────────────────────── 2026-05-15 09:14 ─
Discussed the Q3 roadmap with the team. Three things need to land before
the release cut:

- [ ] Finish the export-to-Markdown story
- [x] Land tag autocomplete
- [ ] Pin/star notes

#standup #q3 #roadmap

─ Bug investigation ──────────────────────────────────────── 2026-05-14 ─
The reproducer is short: open a file with **wrap** on, paste 1k chars,
press `Ctrl+Z`. Cursor jumps to the wrong row...
```

A single keyboard-driven editor for the kind of notes that pile up over years: meeting minutes, debugging journals, "what did I do today" logs, half-baked ideas you want to find again. Stored as plain text in a Markdown-ish format, queryable by tag and date, polishable with an LLM when prose matters, and synced across machines via Git so your corporate laptop and home desktop see the same file without going through anyone's cloud.

## Why jotlog

I wanted a notes tool that satisfied four constraints simultaneously:

1. **Terminal-fast.** Open, capture a thought, close, in under a second.
2. **Plain text I can grep.** No vendor lock-in, no SQLite, no binary format.
3. **In-editor AI polish.** Drafting in poor English, then refining with a prompt — without copy-pasting to a browser.
4. **Sync that works behind corporate IT.** Dropbox/iCloud are blocked at most jobs. Git over HTTPS is universally allowed.

Nothing on the market ticks all four (Obsidian: no terminal; Standard Notes: encrypted blob; Logseq: requires their sync; vim/emacs: no AI panel of this shape). So I built it.

## Features

### Captain's-log file format

A single `.jot` file holds the whole journal. Notes are separated by `===` lines with a title and ISO-8601 date:

```
=== Morning standup | 2026-05-15 09:14
Body in Markdown...

=== Other note | 2026-05-14
...
```

The `===` line auto-stamps with the current time when you type `===` Enter. Dates are editable plain text (rare; the system never invents one). File order doesn't have to match date order — back-dated entries are first-class.

### TUI editor that just works

- Word wrap with cursor-aware navigation (Up/Down move by visual row)
- Selection (`Shift+Arrow` / `Home` / `End` / `PageUp/Down`)
- System clipboard (`Ctrl+C` / `Ctrl+X` / `Ctrl+V`), bracketed paste — large pastes land instantly and undo as a single step
- Search overlay (`Ctrl+F`), find next / previous (`F3` / `Shift+F3`)
- Mouse-wheel scroll
- Per-file cursor memory across sessions

### Markdown highlighting

`**bold**`, `*italic*`, `~~strikethrough~~`, `[link](url)`, headings, lists, blockquotes, fenced code, horizontal rules — all rendered with terminal styles while the source characters stay visible (you never edit a "rendered" view that differs from disk).

### Decorated note delimiters

`===` lines render as a continuous horizontal rule with the title on the left and the date on the right — clean visual separation without altering the underlying text. When your cursor lands on a delimiter row, it reverts to the raw `=== Title | Date` form so editing is normal.

### Filter & navigate

- `Ctrl+P` opens a live-picker panel
- Type `tag:oci`, `date:2026-05`, `title:standup`, or any text — results update as you type
- ↑↓ navigates matches; the editor cursor follows for live preview
- `Enter` commits, `Esc` restores
- `Ctrl+J` / `Ctrl+K` walk between matches when a filter is active

### AI-assisted polish

`Ctrl+L` opens a prompt panel:

- Type a freeform prompt, see the result, edit the prompt and regenerate
- `Tab` toggles between original and AI candidate
- `Ctrl+Enter` accepts (single undo step)
- Works with OpenAI, Ollama, LM Studio (any OpenAI-compatible endpoint)

### Spell check

`F7` opens a wizard. English dictionary downloaded on first use after explicit confirmation. Personal word additions go to a plain-text file you can hand-edit or sync.

### GitHub sync (transparent)

- Pull on open, commit + push on save — automatic, in the background
- Status-bar indicator: `✓` idle, `↑N` ahead, `↓…` pulling, `↑…` pushing, `offline`, `conflict`
- Conflict overlay with **Keep local** / **Take remote** / **Edit manually** options
- Inherits your existing `git` auth (Credential Manager, SSH, PAT — whatever you have)

### Help is one keystroke away

`F1` opens a categorised cheat-sheet with every shortcut and command. Discovery is built-in; you don't have to read this README to use the app.

## Install

```bash
git clone https://github.com/m0mus/jotlog
cd jotlog
cargo install --path .
```

This puts the `jot` binary in `~/.cargo/bin/`. Make sure that's on your `PATH`.

Or for development:

```bash
cargo build --release
# binary at target/release/jot
```

Requirements:
- Rust 1.75+
- `git` CLI (only if you want sync)

## Quick start

```bash
jot
```

On first run, the file `<Documents>/jotlog/log.jot` is opened (the parent directory is created if missing; the file itself appears on first save). Inside:

- Type `===` then `Enter` to start a new note — the date is stamped for you
- Write your note in plain Markdown
- Tag with `#tagname`, todos with `TODO:` or `- [ ]`
- `Ctrl+S` to save
- `F1` for every shortcut, `Ctrl+;` for the command bar

To use a specific file instead:

```bash
jot ~/work/work.jot
```

## File format at a glance

```
=== Meeting notes | 2026-05-11
Lorem ipsum.

## Follow-up items

TODO: Send proposal to Michael
- [ ] Review the deployment docs
- [x] Book the conference room

#oci #helidon

=== Morning standup | 2026-05-12 09:14
Another note. **Bold** and *italic* work.
See [the wiki](https://example.com) for more.
```

Two structural rules:
- A line starting with `===` opens a new note. Optional `Title | YYYY-MM-DD [HH:MM]` after.
- Tags are `#word` (no space; `# Heading` with a space is a Markdown heading).
- Todos are `TODO:` / `DONE:` (inline) or `- [ ]` / `- [x]` (checkbox).

Full spec: [SPEC.md](SPEC.md).

## Configuration

Config lives at:
- **Windows:** `%APPDATA%\jotlog\config.toml`
- **macOS / Linux:** `~/.config/jotlog/config.toml`

It's optional — defaults are sensible. A complete annotated example (also in `config.toml.example`):

```toml
# Path to the notes file. If omitted, falls back to
# <Documents>/jotlog/log.jot on first run.
default_file = "C:/Users/you/Documents/jotlog/log.jot"

[editor]
keybindings = "default"          # only "default" supported in v1 (vim mode planned)
line_wrap = true                 # word-wrap long lines at viewport width
tab_width = 4

[ai]
# OpenAI (default)
provider     = "openai"
model        = "gpt-4o-mini"
base_url     = "https://api.openai.com/v1"
api_key_env  = "OPENAI_API_KEY"

# Ollama (local)
# provider     = "ollama"
# model        = "llama3.2"
# base_url     = "http://localhost:11434/v1"
# api_key_env omitted — no auth header sent

# LM Studio (local)
# provider     = "lmstudio"
# model        = "<model loaded in LM Studio>"
# base_url     = "http://localhost:1234/v1"

[spell]
language = "en"                  # only "en" supported in v1

[sync]
enabled            = true        # auto-detected when file is inside a git repo
pull_on_open       = true
push_on_save       = true
idle_pull_interval = "5m"        # "0" disables
```

## AI setup

1. **OpenAI:**
   ```pwsh
   $env:OPENAI_API_KEY = "sk-..."
   ```
   That's it. `Ctrl+L` in the editor opens the panel.

2. **Ollama (local):**
   ```bash
   ollama serve
   ollama pull llama3.2
   ```
   Set `provider = "ollama"` in config. No API key needed.

3. **LM Studio (local):**
   - Open LM Studio, load a model, start the local server (default port 1234).
   - Set `provider = "lmstudio"`, `model = "<your model name>"`, `base_url = "http://localhost:1234/v1"`.
   - No API key needed.

## GitHub sync setup

The simplest path:

1. Create a private GitHub repo (just for your notes).
2. Clone it where jotlog expects the file:
   ```bash
   git clone git@github.com:you/notes.git ~/Documents/jotlog
   ```
3. Run `jot`. Status bar shows `✓` once the initial pull completes.

On other machines, repeat steps 2–3. Pulls and pushes happen automatically (every save, plus every 5 minutes idle). The app shells out to your existing `git` CLI, so whatever auth you've set up (Credential Manager / SSH / PAT) is what gets used.

Conflicts produce a yellow modal with three options:

- `K` Keep local
- `R` Take remote
- `E` Edit manually (resolve `<<<<<<<` markers, then `:sync`)

## Keyboard shortcuts (essentials)

The full list is one keystroke away via `F1`. Here's a working subset:

| Key | Action |
|---|---|
| `Ctrl+S` | Save |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste (needs selection) |
| `Shift+Arrow` etc. | Extend selection |
| `Ctrl+F` | Search · `F3` / `Shift+F3` next / prev match |
| `Ctrl+P` | Filter panel · `F4` / `Shift+F4` next / prev match |
| `Ctrl+J` / `Ctrl+K` | Next / previous note (filter-aware) |
| `Ctrl+L` | AI panel |
| `F7` | Spell check |
| `F8` | Todo overlay |
| `Ctrl+;` / `F10` | Command bar |
| `F1` | Help (full cheat-sheet) |

## Commands (`:` palette)

Open with `Ctrl+;`, then type. `Tab` completes the verb.

| Command | What it does |
|---|---|
| `:w` / `:write` | Save |
| `:q` / `:wq` / `:q!` | Quit (warn / save+quit / discard) |
| `:goto N` | Jump to line N |
| `:filter <query>` | Open filter panel (pre-filled) |
| `:next` / `:prev` / `:clear` | Walk filter matches / clear filter |
| `:todo` | Todo overlay |
| `:ai` | AI panel |
| `:spell` / `:spell all` | Spell check current note / whole file |
| `:sync` | Manual git pull + push |
| `:help` | Help overlay |

## Architecture

The file text is the **source of truth**. Notes, tags, todos, and filter matches are *indexes over spans into the buffer*, not a separate model. Edits update the buffer; indexes are derived on every parse. This keeps undo/redo, save, and conflict detection from fighting each other over multiple representations of the same text.

For the long version, see [SPEC.md](SPEC.md).

## Roadmap

Planned and probable, in no particular order:

- Tag autocompletion (popup on `#`)
- AI prompt presets (`[ai.presets]` quick-pick library)
- Saved filters bound to F-keys
- Vim mode (Normal / Insert / Visual)
- Multi-file workspace
- Export to standard Markdown
- Better default dictionary / multi-language spell check
- Anthropic provider for AI

## License

[MIT](LICENSE).
