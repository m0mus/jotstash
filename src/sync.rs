//! GitHub-backed sync layer. Shell out to the `git` CLI so the user's existing
//! credential setup (Credential Manager / SSH / PAT) is inherited automatically.
//! Each function below is a thin wrapper around a `git` subprocess invocation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Visible state used by the status bar and the event loop. Cheap to clone.
#[derive(Debug, Clone)]
pub enum SyncState {
    /// File is not inside a git working tree, or `[sync] enabled = false`.
    Disabled,
    /// Up to date with remote.
    Idle,
    /// Pull in flight (background).
    Pulling,
    /// Commit + push in flight (background).
    Pushing,
    /// Local has N unpushed commits.
    AheadBy(usize),
    /// The last network operation failed (DNS, connection refused, etc).
    Offline,
    /// The working tree has unresolved merge conflict markers.
    Conflict,
    /// Some other failure (auth, etc.). Message displayed in status bar.
    Error(String),
}

#[derive(Debug)]
pub enum PullOutcome {
    UpToDate,
    FastForwarded,
    Conflicted,
    Offline,
    Error(String),
}

#[derive(Debug)]
pub enum PushOutcome {
    Pushed,
    NothingToPush,
    Conflicted,
    Offline,
    Error(String),
}

#[derive(Debug)]
pub enum FetchOutcome {
    UpToDate,
    BehindBy(usize),
    Offline,
    Error(String),
}

/// Returns the repository root if `file`'s ancestor chain contains a `.git`
/// directory; `None` otherwise.
pub fn detect_repo(file: &Path) -> Option<PathBuf> {
    let mut cur = file.canonicalize().ok().unwrap_or_else(|| file.to_path_buf());
    if cur.is_file() {
        cur = cur.parent()?.to_path_buf();
    }
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return None,
        }
    }
}

/// True if `git remote` lists at least one remote in `repo`.
pub fn has_remote(repo: &Path) -> bool {
    let out = match Command::new("git")
        .arg("remote")
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// True if `content` has a Git merge conflict marker at the start of any line.
pub fn has_conflict_markers(content: &str) -> bool {
    content
        .lines()
        .any(|l| l.starts_with("<<<<<<<") || l.starts_with("=======") || l.starts_with(">>>>>>>"))
}

/// Run `git pull --rebase --no-edit` in `repo`. Kill the subprocess if it
/// exceeds `timeout`.
pub fn pull_with_timeout(repo: &Path, timeout: Duration) -> PullOutcome {
    let mut cmd = Command::new("git");
    cmd.args(["pull", "--rebase", "--no-edit"])
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_timeout(cmd, timeout, classify_pull_output)
}

/// Stage `file`, commit (with `message`) if there's something to commit,
/// then push. Returns the outcome.
pub fn commit_and_push(repo: &Path, file: &Path, message: &str) -> PushOutcome {
    // 1. Stage the file (relative to repo, since current_dir is set).
    let rel = file
        .strip_prefix(repo)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    if let Err(e) = git_run(repo, &["add", "--", rel.as_str()]) {
        return PushOutcome::Error(e);
    }

    // 2. Anything to commit? `git diff --cached --quiet` exits 0 if no changes.
    let nothing_staged = match Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(repo)
        .status()
    {
        Ok(s) => s.success(),
        Err(e) => return PushOutcome::Error(e.to_string()),
    };

    if !nothing_staged {
        if let Err(e) = git_run(repo, &["commit", "-m", message]) {
            return PushOutcome::Error(e);
        }
    }

    // 3. Push.
    let mut cmd = Command::new("git");
    cmd.arg("push")
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_timeout(cmd, Duration::from_secs(30), classify_push_output)
}

/// Fetch from origin and compute how many commits the current branch is behind.
/// Used by the idle-pull timer to surface remote changes without doing a full
/// pull when there's nothing to pull.
pub fn fetch_and_count_behind(repo: &Path) -> FetchOutcome {
    let mut cmd = Command::new("git");
    cmd.args(["fetch", "--quiet"])
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let fetched: bool = run_with_timeout(cmd, Duration::from_secs(15), |s, _| s.status.success());
    if !fetched {
        return FetchOutcome::Offline;
    }

    let count = match Command::new("git")
        .args(["rev-list", "--count", "HEAD..@{upstream}"])
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0),
        _ => 0,
    };

    if count == 0 {
        FetchOutcome::UpToDate
    } else {
        FetchOutcome::BehindBy(count)
    }
}

/// Count commits ahead of upstream (i.e. local commits not pushed yet).
pub fn count_ahead(repo: &Path) -> usize {
    match Command::new("git")
        .args(["rev-list", "--count", "@{upstream}..HEAD"])
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Resolve a rebase conflict by taking one side, then continue + push.
/// `side` is either `"--theirs"` (keep what was being rebased onto — i.e. the
/// remote in a pull --rebase, which feels weird) or `"--ours"` (keep local).
///
/// The plain-English K/R buttons in the UI translate as follows:
///   `K` keep local → resolve_conflict("--theirs") (rebase semantics inverted)
///   `R` take remote → resolve_conflict("--ours")
pub fn resolve_rebase_conflict(repo: &Path, file: &Path, side: &str) -> Result<(), String> {
    let rel = file
        .strip_prefix(repo)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    git_run(repo, &["checkout", side, "--", rel.as_str()])?;
    git_run(repo, &["add", "--", rel.as_str()])?;
    git_run(repo, &["rebase", "--continue"])?;
    // Push the resolved state.
    git_run(repo, &["push"])
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn git_run(repo: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("git {args:?} failed")
        } else {
            stderr
        })
    }
}

/// Run a `Command` with a wall-clock timeout. The classifier receives the
/// finished `std::process::Output` plus a hint (the elapsed duration); it
/// returns the typed outcome. On timeout, the subprocess is killed and the
/// caller is informed via a fallback.
fn run_with_timeout<T>(
    mut cmd: Command,
    timeout: Duration,
    classify: impl FnOnce(&std::process::Output, Duration) -> T + Send + 'static,
) -> T
where
    T: Send + 'static + From<TimeoutFallback>,
{
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return T::from(TimeoutFallback::SpawnFailed(e.to_string())),
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let res = child.wait_with_output();
        let _ = tx.send(res);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            let _ = handle.join();
            classify(&output, timeout)
        }
        Ok(Err(e)) => {
            let _ = handle.join();
            T::from(TimeoutFallback::IoError(e.to_string()))
        }
        Err(_) => {
            // Timeout — best-effort kill. We can't access `child` (moved into the
            // thread), so we rely on the OS process tree to clean up; the thread
            // will eventually finish and its result will be dropped.
            T::from(TimeoutFallback::Timeout)
        }
    }
}

/// Used by `run_with_timeout` to communicate non-finished-with-output paths.
pub enum TimeoutFallback {
    Timeout,
    SpawnFailed(String),
    IoError(String),
}

impl From<TimeoutFallback> for PullOutcome {
    fn from(t: TimeoutFallback) -> Self {
        match t {
            TimeoutFallback::Timeout => PullOutcome::Offline,
            TimeoutFallback::SpawnFailed(e) => PullOutcome::Error(format!("spawn git: {e}")),
            TimeoutFallback::IoError(e) => PullOutcome::Error(e),
        }
    }
}

impl From<TimeoutFallback> for PushOutcome {
    fn from(t: TimeoutFallback) -> Self {
        match t {
            TimeoutFallback::Timeout => PushOutcome::Offline,
            TimeoutFallback::SpawnFailed(e) => PushOutcome::Error(format!("spawn git: {e}")),
            TimeoutFallback::IoError(e) => PushOutcome::Error(e),
        }
    }
}

impl From<TimeoutFallback> for bool {
    fn from(_: TimeoutFallback) -> bool {
        // Used by fetch_and_count_behind: any non-success path → false.
        false
    }
}

fn classify_pull_output(out: &std::process::Output, _t: Duration) -> PullOutcome {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if combined.contains("CONFLICT") || combined.contains("Merge conflict") {
        return PullOutcome::Conflicted;
    }
    if out.status.success() {
        if combined.contains("Already up to date") || combined.contains("up-to-date") {
            return PullOutcome::UpToDate;
        }
        return PullOutcome::FastForwarded;
    }
    if is_offline_error(&combined) {
        PullOutcome::Offline
    } else {
        PullOutcome::Error(stderr.trim().to_string())
    }
}

fn classify_push_output(out: &std::process::Output, _t: Duration) -> PushOutcome {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if out.status.success() {
        if combined.contains("Everything up-to-date") {
            return PushOutcome::NothingToPush;
        }
        return PushOutcome::Pushed;
    }
    if combined.contains("rejected") && combined.contains("non-fast-forward") {
        return PushOutcome::Conflicted;
    }
    if is_offline_error(&combined) {
        PushOutcome::Offline
    } else {
        PushOutcome::Error(stderr.trim().to_string())
    }
}

fn is_offline_error(s: &str) -> bool {
    let s = s.to_lowercase();
    s.contains("could not resolve host")
        || s.contains("connection timed out")
        || s.contains("connection refused")
        || s.contains("could not read from remote repository")
        || s.contains("unable to access")
        || s.contains("network is unreachable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_conflict_markers() {
        assert!(has_conflict_markers("foo\n<<<<<<< HEAD\nbar\n=======\nbaz\n>>>>>>> branch\n"));
        assert!(!has_conflict_markers("foo\nbar\nbaz\n"));
        assert!(!has_conflict_markers("a < b\nstill < ok\n"));
    }

    #[test]
    fn offline_error_detection() {
        assert!(is_offline_error("fatal: Could not resolve host: github.com"));
        assert!(is_offline_error("fatal: unable to access 'https://github.com/...': "));
        assert!(!is_offline_error("fatal: Authentication failed"));
    }
}
