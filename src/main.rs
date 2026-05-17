mod ai;
mod app;
mod buffer;
mod commands;
mod config;
mod date;
mod index;
mod parser;
mod persistence;
mod spans;
mod spell;
mod state;
mod sync;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "jot",
    version,
    about = "JotStash — terminal-based, plain-text journal app"
)]
struct Cli {
    /// Path to the .notes file (overrides --file and default_file)
    file_pos: Option<PathBuf>,

    /// Path to the .notes file (overrides default_file in config)
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,

    /// Quick-add a note (with --todo: quick-add a todo)
    #[arg(long, value_name = "TEXT", conflicts_with = "filter")]
    add: Option<String>,

    /// Comma-separated tags for --add
    #[arg(long, value_name = "TAGS", requires = "add")]
    tags: Option<String>,

    /// Print notes matching <query> to stdout
    #[arg(long, value_name = "QUERY", conflicts_with = "todo")]
    filter: Option<String>,

    /// Operate on todos (list, toggle, or add with --add)
    #[arg(long)]
    todo: bool,

    /// With --todo: include DONE items
    #[arg(long, requires = "todo")]
    all: bool,

    /// With --todo: print matching todos only
    #[arg(long, value_name = "PHRASE", requires = "todo")]
    grep: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load()?;

    let (file, used_default) = match cli
        .file
        .clone()
        .or_else(|| cli.file_pos.clone())
        .or_else(|| cfg.default_file.clone())
    {
        Some(p) => (p, false),
        None => (config::default_notes_path(), true),
    };

    if used_default {
        // Ensure the parent directory exists so opening / saving the file
        // doesn't fail later. We don't pre-create the file itself — App::open
        // already handles a missing file by opening an empty buffer.
        if let Some(parent) = file.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating directory {}", parent.display())
                })?;
            }
        }
        if !file.exists() {
            eprintln!("Using default file: {}", file.display());
        }
    }

    if let Some(text) = cli.add.as_deref() {
        return if cli.todo {
            commands::cmd_todo_add(&file, text)
        } else {
            commands::cmd_note_add(&file, text, cli.tags.as_deref())
        };
    }

    if cli.todo {
        if let Some(phrase) = cli.grep.as_deref() {
            return commands::cmd_todo_grep(&file, phrase, cli.all);
        }
        return commands::cmd_todo_interactive(&file, cli.all);
    }

    if let Some(query) = cli.filter.as_deref() {
        return commands::cmd_filter(&file, query);
    }

    cmd_tui(&file, &cfg)
}

fn cmd_tui(file: &std::path::Path, cfg: &config::Config) -> Result<()> {
    let mut a = app::App::open(file, cfg)?;
    a.run()
}
