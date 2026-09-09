//! Git subprocess execution and repository status, history, and diff loading.

use super::truncate_visible;
use super::{DiffKind, DiffLine, GitFile, GitSection, GitStatus, HistoryEntry, LoadedDiff};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub(super) const MAX_DIFF_LINES: usize = 2000;

pub(super) const MAX_DIFF_BYTES: usize = 1024 * 1024;

/// `git status` invocation shared by loads and the poll fingerprint.
pub(super) const STATUS_ARGS: &[&str] = &[
    "status",
    "--porcelain=v1",
    "-z",
    "-b",
    "--untracked-files=normal",
];

pub(super) fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// How locating the work-tree root can fail. The variants matter: a plain
/// non-repo offers repository setup, while a git failure is shown as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FindRootError {
    /// The directory is not inside a git work tree.
    NotARepo,
    /// Git itself failed (missing binary, I/O error, empty root).
    Git(String),
}

impl std::fmt::Display for FindRootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepo => formatter.write_str("not a git repository"),
            Self::Git(detail) => formatter.write_str(detail),
        }
    }
}

/// Resolves the work-tree root containing `cwd` via `git rev-parse`.
///
/// # Errors
///
/// Returns [`FindRootError::NotARepo`] when `cwd` is not inside a work tree.
/// Returns [`FindRootError::Git`] when git is missing, cannot run, or prints
/// an empty root.
pub(super) fn find_root(cwd: &Path) -> Result<PathBuf, FindRootError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            FindRootError::Git(if error.kind() == std::io::ErrorKind::NotFound {
                "git is not installed".to_string()
            } else {
                format!("could not run git: {error}")
            })
        })?;
    if !output.status.success() {
        return Err(FindRootError::NotARepo);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Err(FindRootError::Git(
            "git returned an empty work-tree root".to_string(),
        ));
    }
    Ok(PathBuf::from(root))
}

/// Maximum interval between cancellation checks; completion wakes immediately.
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Runs `git -C root …` non-interactively and returns its stdout.
pub(super) fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    run_git_env(root, args, &[])
}

/// Runs `git -C root …` non-interactively and returns stdout. Extra `env`
/// entries extend the child environment (tests pin `GIT_CONFIG_GLOBAL`
/// this way) without touching the process environment.
///
/// A waiter reports child completion while the worker checks cancellation.
/// Cancellation kills and reaps the process group, including hooks holding
/// output pipes open, without changing the caller's cancellation flag.
///
/// # Errors
///
/// Returns a message when git is missing, cannot spawn, is cancelled, or
/// exits non-zero (carrying the first line of its trimmed stderr, falling
/// back to stdout).
pub(super) fn run_git_env(
    root: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<String, String> {
    run_git_output(root, args, env, usize::MAX)
}

/// Captures at most `stdout_limit` bytes while continuing to drain the child.
fn run_git_output(
    root: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdout_limit: usize,
) -> Result<String, String> {
    if crate::cancellation::interrupted() {
        return Err("cancelled.".into());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .envs(env.iter().copied())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "git is not installed".to_string()
            } else {
                format!("could not run git: {error}")
            }
        })?;
    // Drain stdout/stderr on reader threads while polling: a child that
    // emits more than the pipe buffer (~64 KiB, e.g. `diff -U10000` of a
    // large file) would otherwise block on write() forever while we block
    // on try_wait().
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let pid = child.id() as libc::pid_t;
    let (status, stdout_bytes, stderr_bytes) = std::thread::scope(|scope| {
        let out_handle = scope.spawn(|| drain_pipe(stdout_pipe, stdout_limit));
        let err_handle = scope.spawn(|| drain_pipe(stderr_pipe, usize::MAX));
        // A blocking waiter reports completion immediately. The timeout only
        // bounds cancellation checks, so fast Git commands do not pay a tick.
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let _ = tx.send(child.wait());
        });
        let status = loop {
            if crate::cancellation::interrupted() {
                kill_git_group(pid);
                return Err("cancelled.".to_string());
            }
            match rx.recv_timeout(GIT_POLL_INTERVAL) {
                Ok(Ok(status)) => break status,
                Ok(Err(error)) => {
                    kill_git_group(pid);
                    return Err(format!("could not wait for git: {error}"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    kill_git_group(pid);
                    return Err("Git waiter stopped unexpectedly".into());
                }
            }
        };
        let stdout_bytes = out_handle.join().unwrap_or_default();
        let stderr_bytes = err_handle.join().unwrap_or_default();
        Ok::<_, String>((status, stdout_bytes, stderr_bytes))
    })?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        let first = detail.lines().next().unwrap_or("git failed").trim();
        return Err(format!("git {} failed: {first}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&stdout_bytes).into_owned())
}

fn kill_git_group(pid: libc::pid_t) {
    // SAFETY: the child was started in its own process group; a negative PID
    // targets that group, including hooks that inherited the output pipes.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// Drains a piped child stream to completion. `None` (pipe already taken)
/// yields empty output.
pub(super) fn drain_pipe(pipe: Option<impl std::io::Read>, limit: usize) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match std::io::Read::read(&mut pipe, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let keep = read.min(limit.saturating_sub(buffered.len()));
                buffered.extend_from_slice(&chunk[..keep]);
            }
        }
    }
    buffered
}

fn parse_branch_header(header: &str, status: &mut GitStatus) {
    // `## main...origin/main [ahead 1, behind 2]`, `## main`, `## No commits
    // yet on main`, `## HEAD (no branch)`.
    let header = header.strip_prefix("## ").unwrap_or(header);
    let (branch_part, extra) = match header.split_once(" [") {
        Some((left, right)) => (left, Some(right)),
        None => (header, None),
    };
    if let Some((local, upstream)) = branch_part.split_once("...") {
        status.branch = if local.is_empty() {
            "(detached)".to_string()
        } else {
            local.to_string()
        };
        status.upstream = if upstream.is_empty() {
            None
        } else {
            Some(upstream.to_string())
        };
    } else if branch_part == "HEAD (no branch)" {
        status.branch = "(detached)".to_string();
    } else {
        status.branch = branch_part
            .strip_prefix("No commits yet on ")
            .unwrap_or(branch_part)
            .to_string();
    }
    if let Some(extra) = extra {
        for chunk in extra.trim_end_matches(']').split(", ") {
            if let Some(count) = chunk.strip_prefix("ahead ") {
                status.ahead = count.parse().unwrap_or(0);
            } else if let Some(count) = chunk.strip_prefix("behind ") {
                status.behind = count.parse().unwrap_or(0);
            }
        }
    }
}

pub(super) fn parse_status_porcelain(output: &str) -> GitStatus {
    let mut status = GitStatus {
        branch: "(unknown)".to_string(),
        ..GitStatus::default()
    };
    let mut tokens = output.split_terminator('\0').peekable();
    if let Some(first) = tokens.peek()
        && first.starts_with("## ")
    {
        let header = tokens.next().unwrap_or_default().to_string();
        parse_branch_header(&header, &mut status);
    }
    let mut pending_rename: Option<(char, char)> = None;
    let mut pending_path: Option<String> = None;
    for token in tokens {
        if pending_path.is_none() && pending_rename.is_none() {
            if token.len() < 4 {
                continue;
            }
            let mut chars = token.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            if token.as_bytes().get(2) != Some(&b' ') {
                continue;
            }
            let path = token[3..].to_string();
            if x == 'R' || x == 'C' {
                pending_rename = Some((x, y));
                pending_path = Some(path);
                continue;
            }
            push_status_entry(&mut status, x, y, path, None);
        } else {
            // Second NUL of a rename/copy entry: the original path. Porcelain
            // -z reverses the order (new first, original second).
            let new_path = pending_path.take().unwrap_or_default();
            let (x, y) = pending_rename.take().unwrap_or(('R', ' '));
            push_status_entry(&mut status, x, y, new_path, Some(token.to_string()));
        }
    }
    status
}

fn push_status_entry(
    status: &mut GitStatus,
    x: char,
    y: char,
    path: String,
    renamed_from: Option<String>,
) {
    if x == '?' && y == '?' {
        status.untracked.push(GitFile {
            path,
            x,
            y,
            section: GitSection::Untracked,
            renamed_from,
        });
        return;
    }
    if x == '!' {
        return;
    }
    let unmerged = x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D');
    if unmerged {
        status.unstaged.push(GitFile {
            path,
            x,
            y,
            section: GitSection::Unstaged,
            renamed_from,
        });
        return;
    }
    if x != ' ' {
        status.staged.push(GitFile {
            path: path.clone(),
            x,
            y,
            section: GitSection::Staged,
            renamed_from: renamed_from.clone(),
        });
    }
    if y != ' ' {
        status.unstaged.push(GitFile {
            path,
            x,
            y,
            section: GitSection::Unstaged,
            renamed_from,
        });
    }
}

/// Parses `git status` into file groups, keeping the raw output beside it as
/// the poll fingerprint.
///
/// # Errors
///
/// Returns [`run_git`]'s message when the status call fails.
pub(super) fn load_status(root: &Path) -> Result<(GitStatus, String), String> {
    let output = run_git(root, STATUS_ARGS)?;
    Ok((parse_status_porcelain(&output), output))
}

pub(super) fn parse_unified_diff(path: &str, staged: bool, output: &str) -> LoadedDiff {
    let mut lines = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let (mut old_no, mut new_no) = (0usize, 0usize);
    let mut binary = false;
    let mut truncated = output.len() > MAX_DIFF_BYTES;
    let output = &output[..output.floor_char_boundary(output.len().min(MAX_DIFF_BYTES))];
    let mut in_hunk = false;
    for raw in output.lines() {
        if raw.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if raw.starts_with("Binary files ") {
            binary = true;
            break;
        }
        if lines.len() >= MAX_DIFF_LINES {
            truncated = true;
            break;
        }
        if let Some(hunk) = raw.strip_prefix("@@ ") {
            in_hunk = true;
            let mut numbers = String::new();
            for ch in hunk.chars() {
                if ch.is_ascii_digit() || ch == '-' || ch == '+' || ch == ',' || ch == ' ' {
                    numbers.push(ch);
                } else {
                    break;
                }
            }
            let mut parts = numbers.split_whitespace();
            let old_part = parts.next().unwrap_or_default().trim_start_matches('-');
            let new_part = parts.next().unwrap_or_default().trim_start_matches('+');
            let old_start = old_part
                .split(',')
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            let new_start = new_part
                .split(',')
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            old_no = old_start;
            new_no = new_start;
            lines.push(DiffLine {
                old_no: None,
                new_no: None,
                kind: DiffKind::Hunk,
                text: format!("@@ {numbers}@@"),
            });
            continue;
        }
        // Once a hunk begins, `--- text` and `+++ text` are content.
        // Only the next file boundary returns us to parsing headers.
        if !in_hunk || raw.starts_with('\\') {
            continue;
        }
        if let Some(text) = raw.strip_prefix('+') {
            new_no += 1;
            added += 1;
            lines.push(DiffLine {
                old_no: None,
                new_no: Some(new_no - 1),
                kind: DiffKind::Added,
                text: text.to_string(),
            });
        } else if let Some(text) = raw.strip_prefix('-') {
            old_no += 1;
            removed += 1;
            lines.push(DiffLine {
                old_no: Some(old_no - 1),
                new_no: None,
                kind: DiffKind::Removed,
                text: text.to_string(),
            });
        } else if let Some(text) = raw.strip_prefix(' ') {
            old_no += 1;
            new_no += 1;
            lines.push(DiffLine {
                old_no: Some(old_no - 1),
                new_no: Some(new_no - 1),
                kind: DiffKind::Context,
                text: text.to_string(),
            });
        }
    }
    LoadedDiff {
        path: path.to_string(),
        staged,
        untracked: false,
        commit: None,
        lines,
        added,
        removed,
        scroll: 0,
        binary,
        truncated,
    }
}

/// Worktree diff of one file for the left-pane viewer.
///
/// # Errors
///
/// Returns a message when the file cannot be read (untracked) or its `git
/// diff` call fails.
pub(super) fn load_diff(root: &Path, file: &GitFile) -> Result<LoadedDiff, String> {
    if file.section == GitSection::Untracked {
        use std::io::Read;

        let abs = root.join(&file.path);
        let mut bytes = Vec::new();
        std::fs::File::open(&abs)
            .and_then(|input| {
                input
                    .take(MAX_DIFF_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|e| format!("could not read {}: {e}", file.path))?;
        if bytes.len() > MAX_DIFF_BYTES {
            return Ok(LoadedDiff {
                path: file.path.clone(),
                staged: false,
                untracked: true,
                commit: None,
                lines: Vec::new(),
                added: 0,
                removed: 0,
                scroll: 0,
                binary: false,
                truncated: true,
            });
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(LoadedDiff {
                path: file.path.clone(),
                staged: false,
                untracked: true,
                commit: None,
                lines: Vec::new(),
                added: 0,
                removed: 0,
                scroll: 0,
                binary: true,
                truncated: false,
            });
        };
        let mut lines = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if index >= MAX_DIFF_LINES {
                return Ok(LoadedDiff {
                    path: file.path.clone(),
                    staged: false,
                    untracked: true,
                    commit: None,
                    lines,
                    added: index,
                    removed: 0,
                    scroll: 0,
                    binary: false,
                    truncated: true,
                });
            }
            lines.push(DiffLine {
                old_no: None,
                new_no: Some(index + 1),
                kind: DiffKind::Added,
                text: line.to_string(),
            });
        }
        let added = lines.len();
        return Ok(LoadedDiff {
            path: file.path.clone(),
            staged: false,
            untracked: true,
            commit: None,
            lines,
            added,
            removed: 0,
            scroll: 0,
            binary: false,
            truncated: false,
        });
    }
    let staged = file.section == GitSection::Staged;
    // Keep full-file context when it fits. Otherwise ask Git for compact
    // hunks so unchanged lines cannot consume the entire display budget.
    let mut flags = vec!["--no-color", "--no-ext-diff", "-U10000"];
    if staged {
        flags.push("--cached");
    }
    let output = run_git_files(root, "diff", &flags, &[&file.path])?;
    let mut diff = parse_unified_diff(&file.path, staged, &output);
    if diff.truncated {
        flags[2] = "-U3";
        let output = run_git_files(root, "diff", &flags, &[&file.path])?;
        diff = parse_unified_diff(&file.path, staged, &output);
    }
    Ok(diff)
}

/// Sorted local branch names for the branch switcher.
///
/// # Errors
///
/// Returns [`run_git`]'s message when the branch listing fails.
pub(super) fn load_branches(root: &Path) -> Result<Vec<String>, String> {
    let output = run_git(root, &["branch", "--format=%(refname:short)"])?;
    let mut branches: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    branches.sort();
    Ok(branches)
}

/// Parses `git log --format=%H%x1f%h%x1f%D%x1f%an%x1f%ad%x1f%s` output. The
/// subject is last so a stray separator inside a message cannot shift the
/// earlier fields.
fn parse_history(output: &str) -> Vec<HistoryEntry> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\x1f');
            Some(HistoryEntry {
                hash: parts.next()?.to_string(),
                short: parts.next()?.to_string(),
                refs: parts.next().unwrap_or_default().to_string(),
                author: parts.next().unwrap_or_default().to_string(),
                date: parts.next().unwrap_or_default().to_string(),
                subject: parts.collect::<Vec<_>>().join("\x1f"),
            })
        })
        .filter(|entry| !entry.hash.is_empty())
        .collect()
}

/// Loads up to `limit` newest commits plus whether older commits may remain.
/// The `has_more` flag is optimistic: reaching the limit assumes more history
/// until a wider load proves otherwise, so huge repositories never pay for a
/// full log up front.
pub(super) fn load_history_limit(root: &Path, limit: usize) -> (Vec<HistoryEntry>, bool) {
    let limit = limit.max(1);
    let max_count = format!("--max-count={limit}");
    let output = run_git(
        root,
        &[
            "log",
            "--format=%H%x1f%h%x1f%D%x1f%an%x1f%ad%x1f%s",
            "--date=short",
            max_count.as_str(),
        ],
    )
    .unwrap_or_default();
    let history = parse_history(&output);
    let has_more = history.len() >= limit;
    (history, has_more)
}

/// Full-file diff of a historical commit for the left-pane viewer.
///
/// # Errors
///
/// Returns [`run_git`]'s message when `git show` fails (e.g. a pruned commit).
pub(super) fn load_commit_diff(root: &Path, entry: &HistoryEntry) -> Result<LoadedDiff, String> {
    let mut args = [
        "show",
        "--no-color",
        "--no-ext-diff",
        "-U10000",
        "--format=",
        "--first-parent",
        entry.hash.as_str(),
        "--",
    ];
    let output = run_git_output(root, &args, &[], MAX_DIFF_BYTES + 1)?;
    let title = format!("{} {}", entry.short, truncate_visible(&entry.subject, 60));
    let mut diff = parse_unified_diff(&title, false, &output);
    if diff.truncated {
        args[3] = "-U3";
        let output = run_git_output(root, &args, &[], MAX_DIFF_BYTES + 1)?;
        diff = parse_unified_diff(&title, false, &output);
    }
    diff.commit = Some(entry.hash.clone());
    // The first-change anchor is resolved to visual rows on the next render,
    // when the pane width is known (long lines wrap). `usize::MAX` marks it
    // pending; explicit scrolling before that render cancels it to the top.
    diff.scroll = usize::MAX;
    Ok(diff)
}

/// Runs `tool` with `flags` over status-derived `paths` as literal
/// pathspecs (`:(literal)`), so names like `*.rs` never glob-match other
/// files, with `--` ending option parsing. Every command that takes a
/// filename from `git status` goes through here. (`--literal-pathspecs`
/// would read better but `git add` rejects it; the magic works everywhere
/// pathspecs do.)
pub(super) fn run_git_files(
    root: &Path,
    tool: &str,
    flags: &[&str],
    paths: &[&str],
) -> Result<String, String> {
    let literal: Vec<String> = paths
        .iter()
        .map(|path| format!(":(literal){path}"))
        .collect();
    let mut args = Vec::with_capacity(flags.len() + literal.len() + 2);
    args.push(tool);
    args.extend_from_slice(flags);
    args.push("--");
    args.extend(literal.iter().map(String::as_str));
    if tool == "diff" {
        // The extra byte tells the parser the captured preview is partial.
        run_git_output(root, &args, &[], MAX_DIFF_BYTES + 1)
    } else {
        run_git(root, &args)
    }
}

/// Filenames a file operation targets. Staged renames (`R old -> new`) need
/// both sides: touching only `new` leaves `D old` behind in the index.
/// Unstaged and untracked entries target `new` alone — `old` is already
/// staged (or absent), and adding it would stage its deletion.
pub(super) fn target_paths<'a>(
    section: GitSection,
    path: &'a str,
    renamed_from: Option<&'a str>,
) -> Vec<&'a str> {
    let mut paths = Vec::with_capacity(2);
    if section == GitSection::Staged
        && let Some(old) = renamed_from
    {
        paths.push(old);
    }
    paths.push(path);
    paths
}

/// Whether git's stderr reports an unknown subcommand (pre-2.23 git has no
/// `restore`). `run_git` forces `LC_ALL=C`, so the English marker is stable.
/// Always matched on the immediate command error, never on notice text — a
/// file path can contain anything.
pub(super) fn is_unknown_subcommand(error: &str) -> bool {
    error.contains("is not a git command")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{temp_dir_unique, write_test_gitconfig};
    use super::*;

    #[test]
    fn branch_header_parses_ahead_and_behind() {
        let mut status = GitStatus::default();
        parse_branch_header("## main...origin/main [ahead 1, behind 2]", &mut status);
        assert_eq!(status.branch, "main");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (1, 2));

        let mut clean = GitStatus::default();
        parse_branch_header("## main", &mut clean);
        assert_eq!(clean.branch, "main");
        assert_eq!((clean.ahead, clean.behind), (0, 0));
    }

    #[test]
    fn porcelain_splits_staged_unstaged_and_untracked() {
        let output = "## main...origin/main\x00M  staged.rs\x00 M unstaged.rs\x00MM both.rs\x00?? new.rs\x00";
        let status = parse_status_porcelain(output);
        assert_eq!(status.branch, "main");
        assert!(status.staged.iter().any(|f| f.path == "staged.rs"));
        assert!(status.unstaged.iter().any(|f| f.path == "unstaged.rs"));
        // MM appears on both sides.
        assert!(status.staged.iter().any(|f| f.path == "both.rs"));
        assert!(status.unstaged.iter().any(|f| f.path == "both.rs"));
        assert_eq!(status.untracked.len(), 1);
    }

    #[test]
    fn unified_diff_assigns_old_and_new_line_numbers() {
        let output = "@@ -1,3 +1,3 @@\n context\n-old\n+new\n context2\n";
        let diff = parse_unified_diff("a.rs", false, output);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        let kinds: Vec<DiffKind> = diff.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context
            ]
        );
        assert_eq!(diff.lines[1].old_no, Some(1));
        assert_eq!(diff.lines[1].new_no, Some(1));
        assert_eq!(diff.lines[2].old_no, Some(2));
        assert_eq!(diff.lines[2].new_no, None);
        assert_eq!(diff.lines[3].old_no, None);
        assert_eq!(diff.lines[3].new_no, Some(2));
    }

    #[test]
    fn history_parses_hash_refs_author_and_subject() {
        let output = "abc1234567890\x1fabc1234\x1fHEAD -> main, origin/main\x1fAda\x1f2026-09-01\x1ffeat: add things\ndef5678\x1fdef5678\x1f\x1fBob\x1f2026-08-30\x1ffix it";
        let history = parse_history(output);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].short, "abc1234");
        assert_eq!(history[0].refs, "HEAD -> main, origin/main");
        assert_eq!(history[0].author, "Ada");
        assert_eq!(history[0].subject, "feat: add things");
        assert!(history[1].refs.is_empty());
    }

    /// The poll fingerprint must move whenever the worktree does, and stay
    /// put otherwise — that is what makes external changes appear without
    /// reopening the panel while keeping idle polls redraw-free.
    #[test]
    fn status_raw_fingerprint_tracks_external_changes() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = std::env::temp_dir().join(format!(
            "yawl-git-poll-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let git = |args: &[&str]| run_git(&work, args).map_err(crate::error::Error::Protocol);
        git(&["init", "--template="])?;
        std::fs::write(work.join("note.txt"), "v1")?;
        git(&["add", "note.txt"])?;
        git(&[
            "-c",
            "user.name=yawl",
            "-c",
            "user.email=yawl@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "init",
        ])?;

        let calm = git(STATUS_ARGS)?;
        // No change between reads: the poll must stay quiet.
        assert_eq!(git(STATUS_ARGS)?, calm);

        // An external edit (another terminal, an editor) moves it.
        std::fs::write(work.join("note.txt"), "v1\nexternal edit")?;
        let dirty = git(STATUS_ARGS)?;
        assert_ne!(dirty, calm);
        let status = parse_status_porcelain(&dirty);
        assert!(status.unstaged.iter().any(|file| file.path == "note.txt"));

        // So does staging it elsewhere.
        git(&["add", "note.txt"])?;
        let staged = git(STATUS_ARGS)?;
        assert_ne!(staged, dirty);
        let status = parse_status_porcelain(&staged);
        assert!(status.staged.iter().any(|file| file.path == "note.txt"));
        assert!(status.unstaged.is_empty());

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn unknown_subcommand_detector_matches_git_stderr() {
        assert!(is_unknown_subcommand(
            "git: 'restore' is not a git command. See 'git --help'."
        ));
        assert!(!is_unknown_subcommand(
            "error: pathspec 'x' did not match any file(s) known to git"
        ));
        // Notice text mentioning restore (e.g. a file named
        // `my-restore-notes.txt`) must never trigger the old-git fallback.
        assert!(!is_unknown_subcommand("Discarded my-restore-notes.txt."));
    }

    #[test]
    fn find_root_names_plain_directories_as_non_repos() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let dir = temp_dir_unique("detect");
        std::fs::create_dir_all(&dir)?;
        let error = find_root(&dir).expect_err("a plain directory must not resolve as a repo");
        assert!(
            matches!(error, FindRootError::NotARepo),
            "a plain directory must report NotARepo, got {error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn target_paths_carry_both_sides_of_staged_renames_only() {
        assert_eq!(
            target_paths(GitSection::Staged, "b.txt", Some("a.txt")),
            ["a.txt", "b.txt"]
        );
        assert_eq!(target_paths(GitSection::Staged, "b.txt", None), ["b.txt"]);
        // Unstaged entries target `new` alone: `old` is already staged, and
        // adding it would stage its deletion.
        assert_eq!(
            target_paths(GitSection::Unstaged, "b.txt", Some("a.txt")),
            ["b.txt"]
        );
        assert_eq!(
            target_paths(GitSection::Untracked, "b.txt", None),
            ["b.txt"]
        );
    }

    #[test]
    fn stage_targets_literal_filenames() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("literal");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("*.rs"), "star")?;
        std::fs::write(work.join("other.rs"), "other")?;
        let git = |args: &[&str]| run_git(&work, args).map_err(crate::error::Error::Protocol);
        git(&["init", "--template="])?;
        // Without the literal pathspec, `*.rs` would glob-match `other.rs`.
        run_git_files(&work, "add", &[], &["*.rs"]).map_err(crate::error::Error::Protocol)?;
        let status = parse_status_porcelain(&git(STATUS_ARGS)?);
        assert_eq!(
            status
                .staged
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["*.rs"]
        );
        assert!(status.untracked.iter().any(|file| file.path == "other.rs"));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn unstaging_a_staged_rename_clears_both_sides() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("rename");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let git = |args: &[&str]| {
            run_git_env(&work, args, &identity).map_err(crate::error::Error::Protocol)
        };
        git(&["init", "--template="])?;
        std::fs::write(work.join("a.txt"), "v1")?;
        git(&["add", "a.txt"])?;
        git(&["commit", "-m", "init"])?;
        git(&["mv", "a.txt", "b.txt"])?;
        let status = parse_status_porcelain(&git(STATUS_ARGS)?);
        let renamed = status
            .staged
            .iter()
            .find(|file| file.path == "b.txt")
            .expect("rename should be staged");
        assert_eq!(renamed.renamed_from.as_deref(), Some("a.txt"));
        // Mirror `set_file_staged(…, staged = false)`: both sides restore.
        let paths = target_paths(
            GitSection::Staged,
            &renamed.path,
            renamed.renamed_from.as_deref(),
        );
        run_git_files(&work, "restore", &["--staged"], &paths)
            .map_err(crate::error::Error::Protocol)?;
        let status = parse_status_porcelain(&git(STATUS_ARGS)?);
        assert!(
            status.staged.is_empty(),
            "no staged side may survive, got {:?}",
            status.staged
        );
        assert!(
            status.unstaged.iter().any(|file| file.path == "a.txt"),
            "old path returns as an unstaged deletion"
        );
        assert!(
            status.untracked.iter().any(|file| file.path == "b.txt"),
            "new path returns as untracked"
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// Regression test for the `/git` diff hang: `run_git_env` used to poll
    /// `try_wait()` without draining stdout, so any child emitting more than
    /// the pipe buffer (~64 KiB, e.g. `diff -U10000` of `git.rs`) blocked on
    /// write() while we blocked on wait — forever. Reader threads must keep
    /// the pipes drained.
    #[test]
    fn large_cached_diff_does_not_deadlock_on_full_pipes() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        crate::set_interrupted(false);
        let root = temp_dir_unique("large-diff");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        // ~240 KiB staged file: well over the pipe buffer even as a diff.
        let mut big = String::with_capacity(256 * 1024);
        for index in 0..3000 {
            big.push_str(&format!(
                "line {index:05} padding padding padding padding padding padding padding\n"
            ));
        }
        assert!(big.len() > 64 * 1024);
        std::fs::write(work.join("big.rs"), &big)?;
        let git = |args: &[&str]| run_git(&work, args).map_err(crate::error::Error::Protocol);
        git(&["init", "--template="])?;
        git(&["add", "big.rs"])?;
        // Same full-file flags `load_diff` uses. Run off-thread with a
        // timeout so a reintroduced deadlock fails instead of hanging CI.
        let work_clone = work.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let output = run_git(
                &work_clone,
                &["diff", "--cached", "--no-color", "--no-ext-diff", "-U10000"],
            );
            let _ = done_tx.send(output);
        });
        let output = done_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("large git diff must finish; pipe deadlock?")
            .map_err(crate::error::Error::Protocol)?;
        assert!(
            output.len() > 64 * 1024,
            "diff should exceed the pipe buffer, got {} bytes",
            output.len()
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn history_pagination_reports_more_only_until_a_short_page() -> Result<(), crate::error::Error>
    {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("history-pages");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let git = |args: &[&str]| {
            run_git_env(&work, args, &identity).map_err(crate::error::Error::Protocol)
        };
        git(&["init", "--template="])?;
        for index in 0..5 {
            std::fs::write(work.join("note.txt"), format!("v{index}\n"))?;
            git(&["add", "note.txt"])?;
            git(&["commit", "-m", &format!("commit {index}")])?;
        }
        let (first_page, has_more) = load_history_limit(&work, 2);
        assert_eq!(first_page.len(), 2);
        assert!(has_more, "hitting the limit assumes more history");
        let (full, has_more) = load_history_limit(&work, 10);
        assert_eq!(full.len(), 5);
        assert!(!has_more, "a short page proves the end of history");
        assert_eq!(full[0].subject, "commit 4");
        assert_eq!(full[4].subject, "commit 0");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
