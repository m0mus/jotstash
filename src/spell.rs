//! Spell-check engine and tokenizer.
//!
//! The engine wraps `symspell`. The dictionary is downloaded once on first
//! use (see `download_dictionary`) and cached in `%APPDATA%\jotlog\` (or
//! the equivalent XDG path). A user dictionary at `dictionary.txt` augments
//! it — words the user adds via `a` in the wizard go there.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use symspell::{AsciiStringStrategy, SymSpell, Verbosity};

use crate::spans::Span as ByteSpan;

const DICT_URL_EN: &str =
    "https://raw.githubusercontent.com/wolfgarbe/SymSpell/master/SymSpell/frequency_dictionary_en_82_765.txt";

#[derive(Debug)]
pub enum SpellError {
    NoConfigDir,
    DictMissing(PathBuf),
    Download(String),
    Io(String),
    Engine(String),
}

impl std::fmt::Display for SpellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpellError::NoConfigDir => write!(f, "no config directory available"),
            SpellError::DictMissing(p) => write!(f, "dictionary file missing at {}", p.display()),
            SpellError::Download(m) => write!(f, "download error: {m}"),
            SpellError::Io(m) => write!(f, "i/o error: {m}"),
            SpellError::Engine(m) => write!(f, "spell engine error: {m}"),
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("jotlog"))
}

pub fn dict_path(lang: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join(format!("dictionary_{lang}.txt")))
}

pub fn custom_dict_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("dictionary.txt"))
}

pub fn dict_url(lang: &str) -> Option<&'static str> {
    match lang {
        "en" => Some(DICT_URL_EN),
        _ => None,
    }
}

/// Synchronously download the dictionary for `lang` and write it to
/// `dict_path(lang)`. Intended to be called from a worker thread.
pub fn download_dictionary(lang: &str) -> Result<PathBuf, SpellError> {
    let url = dict_url(lang)
        .ok_or_else(|| SpellError::Download(format!("no URL configured for '{lang}'")))?;
    let path = dict_path(lang).ok_or(SpellError::NoConfigDir)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| SpellError::Io(e.to_string()))?;
    }
    let resp = reqwest::blocking::get(url)
        .and_then(|r| r.error_for_status())
        .map_err(|e| SpellError::Download(e.to_string()))?;
    let bytes = resp
        .bytes()
        .map_err(|e| SpellError::Download(e.to_string()))?;
    let mut f = fs::File::create(&path).map_err(|e| SpellError::Io(e.to_string()))?;
    f.write_all(&bytes).map_err(|e| SpellError::Io(e.to_string()))?;
    Ok(path)
}

pub struct SpellEngine {
    symspell: SymSpell<AsciiStringStrategy>,
    user_words: HashSet<String>,
}

impl SpellEngine {
    /// Load the dictionary for `lang` plus the user's custom dictionary.
    /// Returns `Err(DictMissing(...))` if the main dictionary file doesn't exist.
    pub fn load(lang: &str) -> Result<Self, SpellError> {
        let path = dict_path(lang).ok_or(SpellError::NoConfigDir)?;
        if !path.exists() {
            return Err(SpellError::DictMissing(path));
        }
        let mut symspell: SymSpell<AsciiStringStrategy> = SymSpell::default();
        let path_str = path
            .to_str()
            .ok_or_else(|| SpellError::Io("dictionary path not UTF-8".into()))?;
        if !symspell.load_dictionary(path_str, 0, 1, " ") {
            return Err(SpellError::Engine(
                "symspell failed to load dictionary".into(),
            ));
        }

        let mut user_words = HashSet::new();
        if let Some(udp) = custom_dict_path() {
            if udp.exists() {
                if let Ok(text) = fs::read_to_string(&udp) {
                    for line in text.lines() {
                        let w = line.trim();
                        if !w.is_empty() {
                            user_words.insert(w.to_ascii_lowercase());
                        }
                    }
                }
            }
        }

        Ok(Self {
            symspell,
            user_words,
        })
    }

    /// Returns `true` if `word` is in the main or user dictionary.
    pub fn is_correct(&self, word: &str) -> bool {
        let lower = word.to_ascii_lowercase();
        if self.user_words.contains(&lower) {
            return true;
        }
        // SymSpell verbosity::Top with 0 max_edit_distance returns the word
        // itself if it exists in the dictionary.
        let s = self.symspell.lookup(&lower, Verbosity::Top, 0);
        !s.is_empty()
    }

    /// Top suggestions for a misspelled `word`. Empty if symspell finds none.
    pub fn suggest(&self, word: &str, max: usize) -> Vec<String> {
        let lower = word.to_ascii_lowercase();
        let mut results = self.symspell.lookup(&lower, Verbosity::Closest, 2);
        results.truncate(max);
        results.into_iter().map(|s| s.term).collect()
    }

    /// Append `word` to the user dictionary file and the in-memory set.
    pub fn add_to_user_dict(&mut self, word: &str) -> Result<(), SpellError> {
        let lower = word.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return Ok(());
        }
        self.user_words.insert(lower.clone());
        let path = custom_dict_path().ok_or(SpellError::NoConfigDir)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| SpellError::Io(e.to_string()))?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| SpellError::Io(e.to_string()))?;
        writeln!(f, "{lower}").map_err(|e| SpellError::Io(e.to_string()))?;
        Ok(())
    }
}

/// A word found by the tokenizer, with its absolute byte range in the source.
#[derive(Debug, Clone)]
pub struct WordToken {
    pub span: ByteSpan,
    pub text: String,
}

/// Tokenize the byte range `[start, end)` of `source`, yielding words that
/// should be spell-checked. Skips:
///
/// - delimiter lines (`=== ...`)
/// - fenced code blocks (between ``` or ~~~ lines)
/// - inline code (`` `...` ``)
/// - URLs (anything starting with `http://`, `https://`, `ftp://`, `file://`)
/// - `#tags` (the leading `#` and the tag body)
/// - `TODO:` / `DONE:` keywords (the prefix only)
/// - tokens with non-letter characters mixed in (identifiers like `foo_bar`,
///   `path/to/x`, `kebab-case`)
/// - pure-digit tokens, dates (`YYYY-MM-DD`), times (`HH:MM`)
/// - tokens shorter than 2 characters
pub fn tokenize_for_spell(source: &str, start: usize, end: usize) -> Vec<WordToken> {
    let mut out = Vec::new();
    let end = end.min(source.len());
    if start >= end {
        return out;
    }

    let bytes = source.as_bytes();

    // Determine if `start` already sits inside a fenced code block by scanning
    // from the beginning of the file for fence toggles.
    let mut in_fence = false;
    {
        let mut line_start = 0usize;
        for (i, &b) in source.as_bytes().iter().enumerate() {
            if i >= start {
                break;
            }
            if b == b'\n' {
                let line = &source[line_start..i];
                let t = line.trim_start();
                if t.starts_with("```") || t.starts_with("~~~") {
                    in_fence = !in_fence;
                }
                line_start = i + 1;
            }
        }
    }

    // Walk lines within [start, end).
    let mut pos = start;
    while pos < end {
        let line_end = source[pos..end]
            .find('\n')
            .map(|i| pos + i)
            .unwrap_or(end);
        let raw_line = &source[pos..line_end];
        let line_content = raw_line.trim_end_matches('\r');

        let trimmed = line_content.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && !is_delimiter_line(line_content) {
            tokenize_prose_line(source, pos, line_content, &mut out);
        }

        pos = line_end + 1;
    }

    // Discard tokens that fall outside the requested range (in case start was mid-line).
    out.retain(|w| w.span.start >= start && w.span.end <= end);
    let _ = bytes; // unused
    out
}

fn is_delimiter_line(line: &str) -> bool {
    line.starts_with("===")
}

fn tokenize_prose_line(source: &str, line_base: usize, content: &str, out: &mut Vec<WordToken>) {
    let mut in_inline_code = false;
    let mut i = 0usize;
    let bytes = content.as_bytes();
    while i < content.len() {
        let b = bytes[i];

        // Inline code toggles on backtick.
        if b == b'`' {
            in_inline_code = !in_inline_code;
            i += 1;
            continue;
        }
        if in_inline_code {
            i += 1;
            continue;
        }

        // URL skipping.
        if let Some(skip_to) = url_end(content, i) {
            i = skip_to;
            continue;
        }

        // Tag skipping (entire tag including `#`).
        if b == b'#' && i + 1 < content.len() {
            let next = bytes[i + 1];
            if next.is_ascii_alphabetic() || next == b'_' {
                let mut j = i + 1;
                while j < content.len() {
                    let c = bytes[j];
                    if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'/') {
                        j += 1;
                    } else {
                        break;
                    }
                }
                i = j;
                continue;
            }
        }

        // TODO: / DONE: prefixes — skip the keyword + colon.
        if let Some(j) = skip_todo_prefix(content, i) {
            i = j;
            continue;
        }

        // `- [ ]` / `- [x]` checkbox prefix — skip.
        if let Some(j) = skip_checkbox_prefix(content, i) {
            i = j;
            continue;
        }

        // Word scan: start of a candidate word is a Unicode alphabetic character.
        let ch = match content[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        if !ch.is_alphabetic() {
            i += ch.len_utf8();
            continue;
        }
        let word_start = i;
        let mut j = i;
        let mut has_non_letter = false;
        let mut has_digit = false;
        while j < content.len() {
            let c = match content[j..].chars().next() {
                Some(c) => c,
                None => break,
            };
            if c.is_alphabetic() {
                j += c.len_utf8();
            } else if c == '\'' && j + 1 < content.len() {
                // Apostrophe in the middle of a word: keep going (don't / it's).
                j += c.len_utf8();
            } else if c.is_numeric() {
                has_digit = true;
                has_non_letter = true;
                j += c.len_utf8();
            } else if matches!(c, '_' | '-' | '/' | '.') && j + c.len_utf8() < content.len() {
                // Likely an identifier (`foo_bar`, `kebab-case`, `path/to/x`).
                has_non_letter = true;
                j += c.len_utf8();
            } else {
                break;
            }
        }
        let token = &content[word_start..j];
        let trimmed_apostrophe = token.trim_end_matches('\'');
        if !has_non_letter
            && !has_digit
            && trimmed_apostrophe.chars().count() >= 2
            && !is_camel_or_pascal(trimmed_apostrophe)
        {
            out.push(WordToken {
                span: ByteSpan::new(line_base + word_start, line_base + word_start + trimmed_apostrophe.len()),
                text: trimmed_apostrophe.to_string(),
            });
        }
        i = j;
    }
}

fn url_end(content: &str, i: usize) -> Option<usize> {
    let rest = &content[i..];
    for prefix in &["http://", "https://", "ftp://", "file://"] {
        if rest.starts_with(prefix) {
            // Scan until whitespace.
            let end_offset = rest
                .find(|c: char| c.is_whitespace())
                .unwrap_or(rest.len());
            return Some(i + end_offset);
        }
    }
    None
}

fn skip_todo_prefix(content: &str, i: usize) -> Option<usize> {
    if i != 0 && !is_after_whitespace(content, i) {
        return None;
    }
    let rest = &content[i..];
    for prefix in &["TODO:", "DONE:"] {
        if rest.starts_with(prefix) {
            return Some(i + prefix.len());
        }
    }
    None
}

fn skip_checkbox_prefix(content: &str, i: usize) -> Option<usize> {
    let rest = &content[i..];
    for prefix in &["- [ ]", "- [x]", "- [X]"] {
        if rest.starts_with(prefix) {
            return Some(i + prefix.len());
        }
    }
    None
}

fn is_after_whitespace(content: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = content[..i].chars().last();
    prev.map(|c| c.is_whitespace()).unwrap_or(true)
}

fn is_camel_or_pascal(word: &str) -> bool {
    // Heuristic: word has at least one uppercase letter NOT at position 0.
    let mut chars = word.chars();
    let _first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    chars.any(|c| c.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(src: &str) -> Vec<String> {
        tokenize_for_spell(src, 0, src.len())
            .into_iter()
            .map(|w| w.text)
            .collect()
    }

    #[test]
    fn skip_delimiter_lines() {
        let words = tokenize("=== 2026-05-13 14:32 | My title\nhello world\n");
        assert_eq!(words, vec!["hello", "world"]);
    }

    #[test]
    fn skip_tags() {
        let words = tokenize("see #oci and #helidon-test for context\n");
        assert_eq!(words, vec!["see", "and", "for", "context"]);
    }

    #[test]
    fn skip_inline_code() {
        let words = tokenize("call `do_something` here\n");
        assert_eq!(words, vec!["call", "here"]);
    }

    #[test]
    fn skip_fenced_code_block() {
        let src = "before\n```\ncode line\n```\nafter\n";
        assert_eq!(tokenize(src), vec!["before", "after"]);
    }

    #[test]
    fn skip_urls() {
        let words = tokenize("visit https://example.com/path?q=1 for details\n");
        assert_eq!(words, vec!["visit", "for", "details"]);
    }

    #[test]
    fn skip_identifiers() {
        let words = tokenize("the foo_bar value is path/to/file\n");
        assert_eq!(words, vec!["the", "value", "is"]);
    }

    #[test]
    fn skip_camel_case() {
        let words = tokenize("MyClass and another HttpClient class\n");
        assert_eq!(words, vec!["and", "another", "class"]);
    }

    #[test]
    fn skip_todo_prefix() {
        let words = tokenize("TODO: write the docs\n");
        assert_eq!(words, vec!["write", "the", "docs"]);
    }

    #[test]
    fn skip_checkbox_prefix() {
        let words = tokenize("- [ ] something todo\n");
        assert_eq!(words, vec!["something", "todo"]);
    }

    #[test]
    fn keep_apostrophes_in_words() {
        let words = tokenize("don't worry it's fine\n");
        assert_eq!(words, vec!["don't", "worry", "it's", "fine"]);
    }

    #[test]
    fn skip_very_short_words() {
        // Single-character words aren't worth spell-checking.
        let words = tokenize("I a the\n");
        assert_eq!(words, vec!["the"]);
    }
}
