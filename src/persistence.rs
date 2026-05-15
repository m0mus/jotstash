use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// FileSnapshot — mtime + content hash captured at file-open time
// ---------------------------------------------------------------------------

pub struct FileSnapshot {
    path: PathBuf,
    mtime: SystemTime,
    hash: u64,
}

#[derive(Debug, PartialEq)]
pub enum ConflictStatus {
    /// File on disk matches the snapshot (safe to overwrite).
    Clean,
    /// File on disk has changed since the snapshot was taken.
    Changed,
    /// File has been deleted since the snapshot was taken.
    Missing,
}

impl FileSnapshot {
    /// Capture mtime and content hash for `path`.
    pub fn capture(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?;
        let mtime = meta
            .modified()
            .with_context(|| format!("mtime unavailable for {}", path.display()))?;
        let content = fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            mtime,
            hash: hash_bytes(&content),
        })
    }

    /// Check whether the file on disk still matches the snapshot.
    pub fn check(&self) -> Result<ConflictStatus> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ConflictStatus::Missing);
            }
            Err(e) => return Err(e.into()),
        };

        let mtime = meta
            .modified()
            .with_context(|| format!("mtime {}", self.path.display()))?;

        // Fast path: mtime unchanged → no conflict.
        if mtime == self.mtime {
            return Ok(ConflictStatus::Clean);
        }

        // mtime changed: verify with hash to tolerate same-second writes
        // (e.g. a sync tool that preserves mtime, or a file copied over itself).
        let content = fs::read(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        if hash_bytes(&content) == self.hash {
            Ok(ConflictStatus::Clean)
        } else {
            Ok(ConflictStatus::Changed)
        }
    }
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------------

/// Write `content` to `path` atomically using a temp file + rename.
///
/// The temp file is placed in the same directory as `path` so it shares the
/// same filesystem and the rename is atomic. The original file is not touched
/// until the rename succeeds, so a crash mid-write leaves the original intact.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{stem}.tmp"));

    fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("writing temp file {}", tmp.display()))?;

    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read-modify-write with retry-once conflict detection
// ---------------------------------------------------------------------------

/// Outcome of a [`read_modify_write`] call.
#[derive(Debug, PartialEq)]
pub enum WriteOutcome {
    Success,
    /// The file was modified externally and retrying once still saw a conflict.
    /// The caller should warn the user.
    ConflictRetryFailed,
}

/// Read `path`, call `modify(current_content)` to produce new content, then
/// write it back atomically with conflict detection.
///
/// - If the file does not exist yet, `modify("")` is called and the result is
///   written as a new file (no conflict check needed).
/// - If the file exists and is modified externally between the read and write,
///   the operation retries once. A second conflict returns
///   [`WriteOutcome::ConflictRetryFailed`].
///
/// `modify` receives the current file content and must return the desired new
/// content. It is declared `Fn` (not `FnOnce`) because it may be called twice
/// on a retry.
pub fn read_modify_write(
    path: &Path,
    modify: impl Fn(&str) -> Result<String>,
) -> Result<WriteOutcome> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
    }

    for attempt in 0..2u8 {
        let (current, snapshot) = if path.exists() {
            let text = fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let snap = FileSnapshot::capture(path)?;
            (text, Some(snap))
        } else {
            (String::new(), None)
        };

        let new_content = modify(&current)?;

        // Conflict check — skip when we are creating a new file.
        if let Some(snap) = &snapshot {
            match snap.check()? {
                ConflictStatus::Clean => {}
                ConflictStatus::Missing => {
                    return Err(anyhow::anyhow!(
                        "file disappeared during write: {}",
                        path.display()
                    ));
                }
                ConflictStatus::Changed => {
                    if attempt == 0 {
                        continue; // retry once
                    } else {
                        return Ok(WriteOutcome::ConflictRetryFailed);
                    }
                }
            }
        }

        write_atomic(path, &new_content)?;
        return Ok(WriteOutcome::Success);
    }

    unreachable!()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Detect the dominant line ending in `content`.
/// Returns `"\r\n"` if any `\r\n` sequence is found, otherwise `"\n"`.
pub fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") { "\r\n" } else { "\n" }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ---- write_atomic -------------------------------------------------------

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.notes");
        write_atomic(&path, "hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[test]
    fn atomic_write_overwrites_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.notes");
        write_atomic(&path, "first\n").unwrap();
        write_atomic(&path, "second\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
    }

    #[test]
    fn atomic_write_no_tmp_left_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.notes");
        write_atomic(&path, "content\n").unwrap();
        let tmp = dir.path().join(".test.notes.tmp");
        assert!(!tmp.exists(), "temp file should not remain after success");
    }

    #[test]
    fn atomic_write_preserves_line_endings_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.notes");
        write_atomic(&path, "line1\r\nline2\r\n").unwrap();
        let raw = fs::read(&path).unwrap();
        assert!(raw.windows(2).any(|w| w == b"\r\n"), "CRLF should be preserved");
    }

    // ---- FileSnapshot -------------------------------------------------------

    #[test]
    fn snapshot_clean_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        fs::write(&path, "content\n").unwrap();
        let snap = FileSnapshot::capture(&path).unwrap();
        assert_eq!(snap.check().unwrap(), ConflictStatus::Clean);
    }

    #[test]
    fn snapshot_changed_after_external_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        fs::write(&path, "original\n").unwrap();
        let snap = FileSnapshot::capture(&path).unwrap();

        // Simulate external write with different mtime by sleeping briefly.
        // On Windows, filesystem mtime resolution is ~10ms; use a small sleep.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, "changed by external tool\n").unwrap();

        assert_eq!(snap.check().unwrap(), ConflictStatus::Changed);
    }

    #[test]
    fn snapshot_clean_same_content_different_mtime() {
        // A sync tool might update mtime without changing content.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        let content = "identical content\n";
        fs::write(&path, content).unwrap();
        let snap = FileSnapshot::capture(&path).unwrap();

        std::thread::sleep(Duration::from_millis(20));
        // Write same content (different mtime, same hash)
        fs::write(&path, content).unwrap();

        assert_eq!(snap.check().unwrap(), ConflictStatus::Clean);
    }

    #[test]
    fn snapshot_missing_when_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        fs::write(&path, "content\n").unwrap();
        let snap = FileSnapshot::capture(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(snap.check().unwrap(), ConflictStatus::Missing);
    }

    // ---- read_modify_write --------------------------------------------------

    #[test]
    fn rmw_creates_file_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.notes");
        assert!(!path.exists());

        let outcome = read_modify_write(&path, |_| Ok("new content\n".into())).unwrap();
        assert_eq!(outcome, WriteOutcome::Success);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content\n");
    }

    #[test]
    fn rmw_modifies_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        fs::write(&path, "line1\n").unwrap();

        let outcome = read_modify_write(&path, |cur| {
            Ok(format!("{cur}line2\n"))
        }).unwrap();

        assert_eq!(outcome, WriteOutcome::Success);
        assert_eq!(fs::read_to_string(&path).unwrap(), "line1\nline2\n");
    }

    #[test]
    fn rmw_conflict_retry_success() {
        // First attempt: simulate conflict by writing a file then modifying it
        // mid-operation. We can't actually intercept between read and write,
        // so we test the retry path by making the modify fn change the file
        // on first call only (via a Cell).
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        fs::write(&path, "original\n").unwrap();

        let call_count = Cell::new(0u32);
        let path_clone = path.clone();

        let outcome = read_modify_write(&path, |cur| {
            let n = call_count.get();
            call_count.set(n + 1);
            if n == 0 {
                // Simulate external change between our read and this modify
                std::thread::sleep(std::time::Duration::from_millis(20));
                fs::write(&path_clone, "externally changed\n").unwrap();
            }
            Ok(format!("{cur}appended\n"))
        }).unwrap();

        // The first attempt sees a conflict and retries. Second attempt succeeds.
        assert_eq!(call_count.get(), 2);
        assert_eq!(outcome, WriteOutcome::Success);
    }

    #[test]
    fn rmw_conflict_retry_failed() {
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.notes");
        fs::write(&path, "original\n").unwrap();

        let call_count = Cell::new(0u32);
        let path_clone = path.clone();

        // Every call triggers an external modification → both attempts conflict.
        // Write distinct content each call so the hash never matches the snapshot.
        let outcome = read_modify_write(&path, |cur| {
            let n = call_count.get();
            call_count.set(n + 1);
            std::thread::sleep(std::time::Duration::from_millis(20));
            fs::write(&path_clone, format!("always changing {n}\n")).unwrap();
            Ok(format!("{cur}appended\n"))
        }).unwrap();

        assert_eq!(call_count.get(), 2);
        assert_eq!(outcome, WriteOutcome::ConflictRetryFailed);
    }

    #[test]
    fn rmw_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("notes.notes");
        assert!(!path.parent().unwrap().exists());
        read_modify_write(&path, |_| Ok("content\n".into())).unwrap();
        assert!(path.exists());
    }

    // ---- detect_line_ending -------------------------------------------------

    #[test]
    fn line_ending_lf() {
        assert_eq!(detect_line_ending("line1\nline2\n"), "\n");
    }

    #[test]
    fn line_ending_crlf() {
        assert_eq!(detect_line_ending("line1\r\nline2\r\n"), "\r\n");
    }

    #[test]
    fn line_ending_empty() {
        assert_eq!(detect_line_ending(""), "\n"); // default to LF for new files
    }
}
