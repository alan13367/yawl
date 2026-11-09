//! Per-session touched-file checkpoints for `/undo`.
//!
//! A turn starts with an empty record. Before `write_file` or `edit_file`
//! runs, Yawl saves that path's pre-image under
//! `~/.yawl/checkpoints/<session-id>/`. Untouched paths are never scanned or
//! copied. If the working directory is a git repo, the user's HEAD is also
//! recorded so `/undo` can reset it when the agent moved it.

use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Error;

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const STORE_VERSION: u32 = 1;
/// How many unrestored files an undo names before summarizing the rest.
const MAX_UNRESTORED_LISTED: usize = 8;
const UNDO_REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const FINGERPRINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const STATUS_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UserHead {
    None,
    Unborn,
    Sha {
        sha: String,
        #[serde(default)]
        staged: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Overlay {
    path: String,
    existed: bool,
    rel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnRecord {
    user_head: UserHead,
    overlays: Vec<Overlay>,
    /// When the turn began, in Unix milliseconds. Older stores omit it.
    #[serde(default)]
    started_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
struct StoredCheckpoints {
    version: u32,
    turns: Vec<TurnRecord>,
}

#[derive(Serialize)]
struct StoredCheckpointsRef<'a> {
    version: u32,
    turns: &'a [TurnRecord],
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: bool,
    pub reset_head: bool,
    pub warning: Option<String>,
}

/// One path recorded by the checkpoints, with its oldest pre-image.
#[derive(Debug, Clone)]
pub struct TouchedFile {
    /// Absolute path as it was recorded when the file was touched.
    pub path: String,
    /// Whether the file existed before the first touch.
    pub existed: bool,
    /// Pre-image bytes when the file existed before the first touch.
    pub previous: Option<Vec<u8>>,
}

/// Stack of touched-file restore points for one session.
pub struct Checkpoints {
    dir: PathBuf,
    work_tree: PathBuf,
    stack: Vec<TurnRecord>,
    load_error: Option<Error>,
}

impl Checkpoints {
    pub fn open(home_dir: &Path, session_id: &str, work_tree: PathBuf) -> Self {
        let dir = home_dir.join("checkpoints").join(session_id);
        let (stack, load_error) = match read_stack(&dir) {
            Ok(Some(stack)) => (stack, None),
            Ok(None) if !dir.exists() => (Vec::new(), None),
            Ok(None) => (Vec::new(), fs::remove_dir_all(&dir).err().map(Error::Io)),
            Err(error) => (Vec::new(), Some(error)),
        };
        Self {
            dir,
            work_tree,
            stack,
            load_error,
        }
    }

    pub(crate) fn ensure_loaded(&self) -> Result<(), Error> {
        if let Some(error) = &self.load_error {
            return Err(Error::Protocol(format!(
                "cannot load undo checkpoints from {}: {error}",
                self.dir.display()
            )));
        }
        Ok(())
    }

    pub fn remove(home_dir: &Path, session_id: &str) {
        let _ = fs::remove_dir_all(home_dir.join("checkpoints").join(session_id));
    }

    /// Opens an empty touched-file restore point for the next turn.
    ///
    /// # Errors
    ///
    /// Returns I/O errors when the restore point cannot be written.
    pub fn snapshot(&mut self) -> Result<(), Error> {
        self.ensure_loaded()?;
        fs::create_dir_all(&self.dir)?;
        let index = self.stack.len();
        let _ = fs::remove_dir_all(self.dir.join("overlays").join(index.to_string()));
        let record = TurnRecord {
            user_head: read_user_head(&self.work_tree),
            overlays: Vec::new(),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };
        self.stack.push(record);
        persist_stack(&self.dir, &self.stack)
    }

    /// Saves the first pre-image of a `write_file`/`edit_file` path in this
    /// turn. No-op when there is no current turn or the path was already saved.
    ///
    /// # Errors
    ///
    /// Returns I/O errors while reading or storing the pre-image, or a protocol
    /// error when the target is not a regular file or exceeds the size limit.
    pub fn remember_path(&mut self, path: &Path) -> Result<(), Error> {
        self.ensure_loaded()?;
        let Some(index) = self.stack.len().checked_sub(1) else {
            return Ok(());
        };
        let abs = absolute(path, &self.work_tree);
        let path_str = abs.to_string_lossy().into_owned();
        let overlay_count = self.stack[index].overlays.len();
        if self.stack[index]
            .overlays
            .iter()
            .any(|overlay| overlay.path == path_str)
        {
            return Ok(());
        }
        let rel = format!("overlays/{index}/{overlay_count}");
        let dest = self.dir.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let existed = match fs::metadata(&abs) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                return Err(Error::Protocol(format!(
                    "{} exceeds the 32 MiB /undo file limit",
                    abs.display()
                )));
            }
            Ok(meta) if meta.is_file() => {
                fs::copy(&abs, &dest)?;
                true
            }
            Ok(_) => {
                return Err(Error::Protocol(format!(
                    "{} is not a regular file",
                    abs.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        self.stack[index].overlays.push(Overlay {
            path: path_str,
            existed,
            rel,
        });
        persist_stack(&self.dir, &self.stack)
    }

    /// Every path this session has touched, one entry per path, in first-touch
    /// order. The pre-image comes from the first turn that touched the path,
    /// so the result reflects the state before the session began. Turn records
    /// removed by a committed undo drop out.
    pub fn touched_files(&self) -> Vec<TouchedFile> {
        let mut seen = std::collections::HashSet::new();
        let mut files = Vec::new();
        for turn in &self.stack {
            for overlay in &turn.overlays {
                if !seen.insert(&overlay.path) {
                    continue;
                }
                let previous = if overlay.existed {
                    fs::read(self.dir.join(&overlay.rel)).ok()
                } else {
                    None
                };
                files.push(TouchedFile {
                    path: overlay.path.clone(),
                    existed: overlay.existed,
                    previous,
                });
            }
        }
        files
    }

    /// Pops the latest snapshot, resets user git HEAD if it moved, and
    /// restores files.
    ///
    /// # Errors
    ///
    /// Returns I/O errors while updating the stack file. Restore failures are
    /// reported on [`RestoreReport::warning`] so messages can still be dropped.
    #[cfg(test)]
    pub fn restore_last(&mut self) -> Result<RestoreReport, Error> {
        let index = self.last_index();
        let report = self.restore_retained(index)?;
        self.discard_restored(index)?;
        Ok(report)
    }

    pub(crate) fn last_index(&self) -> Option<usize> {
        self.stack.len().checked_sub(1)
    }

    /// Restore without consuming the checkpoint until session history commits.
    ///
    /// A snapshot that is gone, for example because the session log was
    /// copied without `~/.yawl/checkpoints` or the store was cleaned up, only
    /// becomes a warning: the history can still be undone.
    pub(crate) fn restore_retained(&self, index: Option<usize>) -> Result<RestoreReport, Error> {
        self.ensure_loaded()?;
        let Some(index) = index else {
            return Ok(RestoreReport::default());
        };
        if index >= self.stack.len() {
            return Ok(RestoreReport {
                restored: false,
                reset_head: false,
                warning: Some(
                    "the undo snapshot for this turn is missing, so files were not restored".into(),
                ),
            });
        }
        if self.last_index() != Some(index) {
            return Err(Error::Protocol(
                "undo checkpoint is missing or out of order".into(),
            ));
        }
        let turn = &self.stack[index];
        let mut report = RestoreReport {
            restored: true,
            reset_head: false,
            warning: None,
        };
        let mut staged_to_restore = None;
        if let UserHead::Sha { sha, staged } = &turn.user_head {
            let current = read_user_head(&self.work_tree);
            let reset_marker = self
                .dir
                .join("overlays")
                .join(index.to_string())
                .join("reset-head");
            let moved =
                matches!(current, UserHead::Sha { sha: current_sha, .. } if current_sha != *sha);
            if moved || reset_marker.exists() {
                // A crash after resetting HEAD must still restore the index
                // on retry, even though HEAD now matches the checkpoint.
                if let Some(parent) = reset_marker.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::File::create(&reset_marker)?.sync_all()?;
                match prepare_user_head_restore(&self.work_tree, sha) {
                    Ok(()) => {
                        report.reset_head = true;
                        staged_to_restore = Some(staged.as_slice());
                    }
                    Err(error) => {
                        report.warning = Some(format!("could not reset git HEAD: {error}"));
                    }
                }
            }
        }
        for overlay in &turn.overlays {
            if let Err(error) = restore_overlay(&self.dir, overlay) {
                let detail = format!("could not restore {}: {error}", overlay.path);
                report.warning = Some(match report.warning.take() {
                    Some(existing) => format!("{existing}; {detail}"),
                    None => detail,
                });
            }
        }
        if let Some(staged) = staged_to_restore
            && let Err(error) = restore_staged_paths(&self.work_tree, staged)
        {
            let detail = format!("could not restore staged paths: {error}");
            report.warning = Some(match report.warning.take() {
                Some(existing) => format!("{existing}; {detail}"),
                None => detail,
            });
        }
        if let Some(detail) = self.unrestored_changes(turn) {
            report.warning = Some(match report.warning.take() {
                Some(existing) => format!("{existing}; {detail}"),
                None => detail,
            });
        }
        Ok(report)
    }

    /// Names files that changed since the undone turn began and were not
    /// restored. Undo restores only what it saved first, and it must not touch
    /// anything else: a file that changed could as easily be the user's own
    /// edit. Deleted files leave nothing to date, so they are not named.
    fn unrestored_changes(&self, turn: &TurnRecord) -> Option<String> {
        if turn.started_unix_ms == 0 {
            return None;
        }
        let root = git_root(&self.work_tree).ok()?;
        let canonical =
            |path: &Path| crate::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let restored = turn
            .overlays
            .iter()
            .map(|overlay| canonical(Path::new(&overlay.path)))
            .collect::<HashSet<_>>();
        // This runs on the interface thread, so a huge tree only costs the
        // report: untracked directories stay whole and the wait is short.
        let changed = changed_paths(&root, "normal", UNDO_REPORT_TIMEOUT)?
            .into_iter()
            .map(|(_, path)| path)
            .filter(|path| {
                let path = root.join(path);
                !restored.contains(&canonical(&path)) && modified_since(&path, turn.started_unix_ms)
            })
            .collect::<Vec<_>>();
        if changed.is_empty() {
            return None;
        }
        let listed = changed[..changed.len().min(MAX_UNRESTORED_LISTED)]
            .iter()
            .map(|path| format!("`{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let more = changed.len().saturating_sub(MAX_UNRESTORED_LISTED);
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        Some(format!(
            "These files have changed since that turn began and were not restored, because /undo only restores changes made with write_file and edit_file: {listed}{more}. Review them with git diff."
        ))
    }

    /// A value that changes when the commit or the working tree's changed files
    /// do, leaving out paths `ignored` rejects. `None` outside a Git repository.
    pub(crate) fn work_fingerprint(&self, ignored: impl Fn(&Path) -> bool) -> Option<u64> {
        let root = git_root(&self.work_tree).ok()?;
        let mut hasher = DefaultHasher::new();
        // A change that was also committed leaves a clean status behind.
        user_git(&root, &["rev-parse", "--verify", "HEAD"])
            .ok()
            .hash(&mut hasher);
        for (status, path) in changed_paths(&root, "all", FINGERPRINT_TIMEOUT)? {
            if ignored(Path::new(&path)) {
                continue;
            }
            status.hash(&mut hasher);
            path.hash(&mut hasher);
            // Status alone misses a further change to an already changed file.
            if let Ok(metadata) = fs::symlink_metadata(root.join(&path)) {
                metadata.len().hash(&mut hasher);
                metadata.modified().ok().hash(&mut hasher);
            }
        }
        Some(hasher.finish())
    }

    /// Idempotent cleanup after the session's undo event has committed.
    pub(crate) fn discard_restored(&mut self, index: Option<usize>) -> Result<(), Error> {
        self.ensure_loaded()?;
        let Some(index) = index else {
            return Ok(());
        };
        if self.stack.len() == index + 1 {
            persist_stack(&self.dir, &self.stack[..index])?;
            self.stack.truncate(index);
        } else if self.stack.len() > index + 1 {
            return Err(Error::Protocol(
                "undo checkpoint cleanup is out of order".into(),
            ));
        }
        let _ = fs::remove_dir_all(self.dir.join("overlays").join(index.to_string()));
        Ok(())
    }
}

/// Files Git reports as changed under `root`, as status and path relative to
/// the root. A clean filter the repository's own config defines would run here,
/// so those are turned off.
fn changed_paths(
    root: &Path,
    untracked: &str,
    timeout: std::time::Duration,
) -> Option<Vec<(String, String)>> {
    let mut command = git_command(root);
    for setting in crate::tools::git::repository_filters_off(root) {
        command.args(["-c", &setting]);
    }
    command.args([
        "status",
        "--porcelain=v1",
        "-z",
        "--no-renames",
        &format!("--untracked-files={untracked}"),
    ]);
    let status = run_git_within(&mut command, timeout, STATUS_OUTPUT_LIMIT).ok()?;
    Some(
        status
            .split('\0')
            .filter_map(|entry| Some((entry.get(..2)?.to_string(), entry.get(3..)?.to_string())))
            .filter(|(_, path)| !path.is_empty())
            .collect(),
    )
}

fn modified_since(path: &Path, unix_ms: u64) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|modified| modified.as_millis() as u64 >= unix_ms)
}

/// Path argument for `write_file` or `edit_file`, if present.
pub fn mutating_tool_path(name: &str, args_json: &str) -> Option<PathBuf> {
    if !matches!(name, "write_file" | "edit_file") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(args_json).ok()?;
    value.get("path")?.as_str().map(PathBuf::from)
}

fn read_stack(dir: &Path) -> Result<Option<Vec<TurnRecord>>, Error> {
    let path = dir.join("stack.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stored: StoredCheckpoints = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(_)
            if serde_json::from_slice::<serde_json::Value>(&bytes)
                .is_ok_and(|value| value.is_array()) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if stored.version != STORE_VERSION {
        return Err(Error::Protocol(format!(
            "unsupported checkpoint store version {}",
            stored.version
        )));
    }
    Ok(Some(stored.turns))
}

fn persist_stack(dir: &Path, stack: &[TurnRecord]) -> Result<(), Error> {
    fs::create_dir_all(dir)?;
    let stored = StoredCheckpointsRef {
        version: STORE_VERSION,
        turns: stack,
    };
    let temporary = dir.join("stack.json.tmp");
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec(&stored)?)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join("stack.json"))?;
    Ok(())
}

#[cfg(test)]
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn restore_overlay(dir: &Path, overlay: &Overlay) -> Result<(), Error> {
    let path = PathBuf::from(&overlay.path);
    if overlay.existed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(dir.join(&overlay.rel), &path)?;
    } else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Overlays are matched by their recorded path string, so one file must
/// always produce one spelling. Rejoining the components gives `\`
/// separators and drops `.` parts, so `C:/repo/a.rs` and `C:\repo\a.rs`
/// are the same overlay and `/undo` never restores a stale pre-image last.
#[cfg(windows)]
fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    joined.components().collect()
}

fn read_user_head(work_tree: &Path) -> UserHead {
    let Ok(root) = git_root(work_tree) else {
        return UserHead::None;
    };
    match user_git(&root, &["rev-parse", "--verify", "HEAD"]) {
        Ok(sha) => UserHead::Sha {
            sha: sha.trim().to_string(),
            staged: match user_git(&root, &["diff", "--cached", "--name-only", "-z"]) {
                Ok(paths) => paths
                    .split('\0')
                    .filter(|path| !path.is_empty())
                    .map(str::to_string)
                    .collect(),
                Err(_) => Vec::new(),
            },
        },
        Err(_) => UserHead::Unborn,
    }
}

fn git_root(work_tree: &Path) -> Result<PathBuf, Error> {
    let root = user_git(work_tree, &["rev-parse", "--show-toplevel"])?;
    let root = root.trim();
    if root.is_empty() {
        return Err(Error::Protocol(
            "git returned an empty work-tree root".into(),
        ));
    }
    Ok(PathBuf::from(root))
}

fn prepare_user_head_restore(work_tree: &Path, sha: &str) -> Result<(), Error> {
    let root = git_root(work_tree)?;
    user_git(&root, &["reset", "--soft", sha])?;
    user_git(&root, &["reset", "--", "."])?;
    Ok(())
}

fn restore_staged_paths(work_tree: &Path, staged: &[String]) -> Result<(), Error> {
    if staged.is_empty() {
        return Ok(());
    }
    let root = git_root(work_tree)?;
    let mut command = git_command(&root);
    command.args(["add", "--"]).args(staged);
    run_git(&mut command)?;
    Ok(())
}

fn user_git(work_tree: &Path, args: &[&str]) -> Result<String, Error> {
    let mut command = git_command(work_tree);
    command.args(args);
    run_git(&mut command)
}

/// Builds a `git` command for the working tree with repository command hooks
/// and the fsmonitor disabled, and with inherited `GIT_*` routing removed so
/// the invocation cannot be aimed at a different repository. The user's own
/// global config is still honored because these are the user's repositories.
fn git_command(work_tree: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(work_tree);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ]);
    command
}

/// Bounds every `/undo` Git call, so a stuck repository cannot hold a turn
/// or an undo forever.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

fn run_git(cmd: &mut Command) -> Result<String, Error> {
    run_git_keeping(cmd, GIT_TIMEOUT, crate::tools::exec::Keep::All)
}

/// Runs a query whose answer is optional: it gives up sooner and keeps only the
/// start of very long output.
fn run_git_within(
    cmd: &mut Command,
    timeout: std::time::Duration,
    limit: usize,
) -> Result<String, Error> {
    run_git_keeping(cmd, timeout, crate::tools::exec::Keep::Head(limit))
}

fn run_git_keeping(
    cmd: &mut Command,
    timeout: std::time::Duration,
    stdout: crate::tools::exec::Keep,
) -> Result<String, Error> {
    let command = std::mem::replace(cmd, Command::new("git"));
    let result = crate::tools::exec::run(
        command,
        crate::tools::exec::RunOptions {
            stdin: None,
            timeout: Some(timeout),
            stdout,
            stderr: crate::tools::exec::Keep::All,
        },
    )
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::Config("git is not installed".into())
        } else {
            Error::Io(error)
        }
    })?;
    if result.interrupted {
        return Err(Error::Interrupted);
    }
    if result.timed_out {
        return Err(Error::Protocol(format!(
            "git timed out after {}s",
            timeout.as_secs()
        )));
    }
    if result.status != Some(0) {
        let detail = if result.stderr.trim().is_empty() {
            result.stdout.trim()
        } else {
            result.stderr.trim()
        };
        return Err(Error::Protocol(format!("git failed: {detail}")));
    }
    Ok(result.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("checkpoint-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ))
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, contents).expect("write");
    }

    #[test]
    fn mutating_tool_path_reads_write_and_edit_only() {
        assert_eq!(
            mutating_tool_path("write_file", r#"{"path":"src/main.rs","content":"x"}"#),
            Some(PathBuf::from("src/main.rs"))
        );
        assert_eq!(
            mutating_tool_path(
                "edit_file",
                r#"{"path":"/tmp/out.rs","old_string":"a","new_string":"b"}"#
            ),
            Some(PathBuf::from("/tmp/out.rs"))
        );
        assert_eq!(mutating_tool_path("shell", r#"{"command":"ls"}"#), None);
        assert_eq!(mutating_tool_path("read_file", r#"{"path":"a"}"#), None);
    }

    #[test]
    fn versioned_store_round_trips_current_turns() -> Result<(), Error> {
        let root = temp_root("versioned-store");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;

        let reopened = Checkpoints::open(&home, "sess", work);

        assert_eq!(reopened.stack.len(), 1);
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join("checkpoints/sess/stack.json"))?)?;
        assert_eq!(stored["version"], STORE_VERSION);
        assert_eq!(stored["turns"].as_array().map(Vec::len), Some(1));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn touched_files_restore_create_modify_and_delete() -> Result<(), Error> {
        let root = temp_root("touched-files");
        let home = root.join("home");
        let work = root.join("work");
        write(&work.join("keep.txt"), "keep");
        write(&work.join("edit.txt"), "before");
        write(&work.join("gone.txt"), "delete-me");
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("edit.txt"))?;
        checkpoints.remember_path(&work.join("gone.txt"))?;
        checkpoints.remember_path(&work.join("new.txt"))?;

        write(&work.join("edit.txt"), "after");
        write(&work.join("new.txt"), "created");
        fs::remove_file(work.join("gone.txt"))?;

        let report = checkpoints.restore_last()?;
        assert!(report.restored);
        assert!(!report.reset_head);
        assert_eq!(fs::read_to_string(work.join("keep.txt"))?, "keep");
        assert_eq!(fs::read_to_string(work.join("edit.txt"))?, "before");
        assert_eq!(fs::read_to_string(work.join("gone.txt"))?, "delete-me");
        assert!(!work.join("new.txt").exists());
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn untouched_files_are_left_alone() -> Result<(), Error> {
        let root = temp_root("untouched");
        let home = root.join("home");
        let work = root.join("work");
        write(&work.join("src.rs"), "src");
        write(&work.join("target/built"), "artifact");
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        write(&work.join("src.rs"), "changed");
        write(&work.join("target/built"), "new-artifact");
        checkpoints.restore_last()?;
        assert_eq!(fs::read_to_string(work.join("src.rs"))?, "changed");
        assert_eq!(
            fs::read_to_string(work.join("target/built"))?,
            "new-artifact"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn prompt_checkpoint_does_not_walk_unmodified_socket() -> Result<(), Error> {
        use std::os::unix::net::UnixListener;

        let root = std::env::temp_dir().join(format!("ys-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        let _socket = UnixListener::bind(work.join("server.sock"))?;
        let mut checkpoints = Checkpoints::open(&home, "sess", work);

        let result = checkpoints.snapshot();

        assert!(result.is_ok(), "checkpoint failed: {result:?}");
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn remembered_work_tree_file_records_one_preimage() -> Result<(), Error> {
        let root = temp_root("work-tree-preimage");
        let home = root.join("home");
        let work = root.join("work");
        let file = work.join("note.txt");
        write(&file, "before");
        let mut checkpoints = Checkpoints::open(&home, "sess", work);
        checkpoints.snapshot()?;

        checkpoints.remember_path(&file)?;

        assert_eq!(checkpoints.stack[0].overlays.len(), 1);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn touched_files_report_the_oldest_preimage_once_per_path() -> Result<(), Error> {
        let root = temp_root("touched-report");
        let home = root.join("home");
        let work = root.join("work");
        write(&work.join("note.txt"), "v1");
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("note.txt"))?;
        write(&work.join("note.txt"), "v2");
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("note.txt"))?;
        checkpoints.remember_path(&work.join("new.txt"))?;

        let files = checkpoints.touched_files();
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].path,
            work.join("note.txt").to_string_lossy().into_owned()
        );
        assert!(files[0].existed);
        assert_eq!(files[0].previous.as_deref(), Some(b"v1".as_slice()));
        assert_eq!(
            files[1].path,
            work.join("new.txt").to_string_lossy().into_owned()
        );
        assert!(!files[1].existed);
        assert_eq!(files[1].previous, None);

        checkpoints.restore_last()?;
        let files = checkpoints.touched_files();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].path,
            work.join("note.txt").to_string_lossy().into_owned()
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn open_discards_legacy_snapshot_stores() -> Result<(), Error> {
        let root = temp_root("legacy");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        let cases: [(&str, &[(&str, &str)]); 2] = [
            (
                "orphaned-data",
                &[
                    ("shadow.git/objects/orphan", "git"),
                    ("copies/0/orphan", "copy"),
                ],
            ),
            (
                "array-store",
                &[("stack.json", "[]"), ("shadow.git/objects/legacy", "git")],
            ),
        ];
        for (name, files) in cases {
            let home = root.join(name);
            let checkpoint_dir = home.join("checkpoints/sess");
            for (path, contents) in files {
                write(&checkpoint_dir.join(path), contents);
            }

            let checkpoints = Checkpoints::open(&home, "sess", work.clone());

            assert!(checkpoints.stack.is_empty(), "{name}");
            assert!(!checkpoint_dir.exists(), "{name}");
        }
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn open_preserves_unreadable_checkpoint_store() -> Result<(), Error> {
        let root = temp_root("unreadable-store");
        let home = root.join("home");
        let work = root.join("work");
        let checkpoint_dir = home.join("checkpoints/sess");
        fs::create_dir_all(checkpoint_dir.join("stack.json"))?;
        write(&checkpoint_dir.join("overlays/0/preimage"), "original");
        fs::create_dir_all(&work)?;

        let mut checkpoints = Checkpoints::open(&home, "sess", work);
        assert!(checkpoint_dir.join("stack.json").is_dir());
        assert!(checkpoint_dir.join("overlays/0/preimage").exists());
        assert!(
            checkpoints.snapshot().is_err(),
            "must not overwrite an unreadable store"
        );
        assert!(checkpoint_dir.join("overlays/0/preimage").exists());

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn open_preserves_unsupported_and_corrupt_checkpoint_stores() -> Result<(), Error> {
        let root = temp_root("invalid-store");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        for (id, contents) in [
            ("future", r#"{"version":999,"turns":[]}"#),
            ("corrupt", "{not-json"),
        ] {
            let dir = home.join("checkpoints").join(id);
            write(&dir.join("stack.json"), contents);
            let mut checkpoints = Checkpoints::open(&home, id, work.clone());
            assert!(checkpoints.snapshot().is_err());
            assert_eq!(fs::read_to_string(dir.join("stack.json"))?, contents);
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn outside_cwd_overlay_restores_and_deletes() -> Result<(), Error> {
        let root = temp_root("overlay");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        let outside = root.join("outside.txt");
        write(&outside, "original");
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&outside)?;
        write(&outside, "mutated");
        let created = root.join("created.txt");
        checkpoints.remember_path(&created)?;
        write(&created, "new");
        checkpoints.restore_last()?;
        assert_eq!(fs::read_to_string(&outside)?, "original");
        assert!(!created.exists());
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn restore_without_snapshot_is_a_no_op() -> Result<(), Error> {
        let root = temp_root("empty");
        let mut checkpoints = Checkpoints::open(&root.join("home"), "sess", root.join("work"));
        let report = checkpoints.restore_last()?;
        assert!(!report.restored);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    /// Commits the staged changes in `work` as a fixed test identity.
    fn commit(work: &Path, message: &str) -> Result<(), Error> {
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(work)
                .args(["-c", "user.name=yawl", "-c", "user.email=yawl@localhost"])
                .args(["-c", "commit.gpgsign=false", "commit", "-m", message]),
        )
        .map(|_| ())
    }

    #[test]
    fn touched_files_restore_after_agent_moves_git_head() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_root("git-head");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        write(&work.join("tracked.txt"), "v1");
        user_git(&work, &["init", "--template="])?;
        user_git(&work, &["add", "tracked.txt"])?;
        commit(&work, "init")?;
        write(&work.join("dirty.txt"), "user-dirty");
        let original_head = user_git(&work, &["rev-parse", "HEAD"])?.trim().to_string();

        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("tracked.txt"))?;
        checkpoints.remember_path(&work.join("added.txt"))?;
        write(&work.join("tracked.txt"), "agent");
        write(&work.join("added.txt"), "new");
        user_git(&work, &["add", "-A"])?;
        commit(&work, "agent")?;

        let report = checkpoints.restore_last()?;
        assert!(report.restored);
        assert!(report.reset_head);
        assert_eq!(fs::read_to_string(work.join("tracked.txt"))?, "v1");
        assert_eq!(fs::read_to_string(work.join("dirty.txt"))?, "user-dirty");
        assert!(!work.join("added.txt").exists());
        assert_eq!(
            user_git(&work, &["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn git_head_restore_recreates_preturn_staged_paths_on_retry() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_root("git-staged");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        write(&work.join("staged.txt"), "base staged");
        write(&work.join("unstaged.txt"), "base unstaged");
        user_git(&work, &["init", "--template="])?;
        user_git(&work, &["add", "staged.txt", "unstaged.txt"])?;
        commit(&work, "base")?;
        let original_head = user_git(&work, &["rev-parse", "HEAD"])?.trim().to_string();
        write(&work.join("staged.txt"), "user staged");
        user_git(&work, &["add", "staged.txt"])?;
        write(&work.join("unstaged.txt"), "user unstaged");

        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("staged.txt"))?;
        checkpoints.remember_path(&work.join("unstaged.txt"))?;
        write(&work.join("staged.txt"), "agent staged");
        write(&work.join("unstaged.txt"), "agent unstaged");
        user_git(&work, &["add", "-A"])?;
        commit(&work, "agent")?;

        let report = checkpoints.restore_retained(Some(0))?;
        // Simulate a crash after HEAD reset but before staging was restored.
        user_git(&work, &["reset", "--", "."])?;
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.restore_last()?;

        assert!(report.restored);
        assert!(report.reset_head);
        assert_eq!(
            user_git(&work, &["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(fs::read_to_string(work.join("staged.txt"))?, "user staged");
        assert_eq!(
            fs::read_to_string(work.join("unstaged.txt"))?,
            "user unstaged"
        );
        assert_eq!(
            user_git(&work, &["diff", "--cached", "--name-only"])?.trim(),
            "staged.txt"
        );
        assert_eq!(
            user_git(&work, &["diff", "--name-only"])?.trim(),
            "unstaged.txt"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn git_head_is_kept_when_staged_listing_fails() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_root("git-index-fail");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        write(&work.join("tracked.txt"), "v1");
        user_git(&work, &["init", "--template="])?;
        user_git(&work, &["add", "tracked.txt"])?;
        commit(&work, "init")?;
        let original_head = user_git(&work, &["rev-parse", "HEAD"])?.trim().to_string();
        fs::write(work.join(".git/index"), b"not a git index")?;

        let mut checkpoints = Checkpoints::open(&home, "sess", work);
        checkpoints.snapshot()?;

        match &checkpoints.stack[0].user_head {
            UserHead::Sha { sha, staged } => {
                assert_eq!(sha, &original_head);
                assert!(staged.is_empty());
            }
            other => panic!("expected recorded HEAD, got {other:?}"),
        }
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn undo_names_what_changed_outside_its_tracking_and_leaves_it_alone() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        // Outside this repository, so its own changes are not reported.
        let root = crate::test_support::unique_temp_dir("undo-unrestored");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        write(&work.join("tracked.txt"), "v1");
        write(&work.join("edited-by-shell.txt"), "v1");
        user_git(&work, &["init", "--template="])?;
        user_git(&work, &["add", "."])?;
        commit(&work, "init")?;

        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("tracked.txt"))?;
        write(&work.join("tracked.txt"), "agent");
        write(&work.join("edited-by-shell.txt"), "shell");
        write(&work.join("created/by-shell.txt"), "shell");

        let report = checkpoints.restore_last()?;
        let warning = report.warning.expect("unrestored files are reported");
        assert!(warning.contains("`edited-by-shell.txt`"), "{warning}");
        assert!(warning.contains("`created/`"), "{warning}");
        assert!(!warning.contains("`tracked.txt`"), "{warning}");
        assert!(warning.contains("write_file and edit_file"), "{warning}");
        // Named, not touched: this could as easily be the user's own edit.
        assert_eq!(fs::read_to_string(work.join("tracked.txt"))?, "v1");
        assert_eq!(
            fs::read_to_string(work.join("edited-by-shell.txt"))?,
            "shell"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn undo_stays_quiet_when_it_restored_everything_or_git_is_absent() -> Result<(), Error> {
        // Not a repository, so there is nothing to compare.
        let root = crate::test_support::unique_temp_dir("undo-quiet");
        let home = root.join("home");
        let work = root.join("work");
        write(&work.join("note.txt"), "v1");
        let mut checkpoints = Checkpoints::open(&home, "plain", work.clone());
        checkpoints.snapshot()?;
        write(&work.join("other.txt"), "changed");
        assert_eq!(checkpoints.restore_last()?.warning, None);

        if git_available() {
            user_git(&work, &["init", "--template="])?;
            user_git(&work, &["add", "."])?;
            commit(&work, "init")?;
            let mut checkpoints = Checkpoints::open(&home, "repo", work.clone());
            checkpoints.snapshot()?;
            checkpoints.remember_path(&work.join("note.txt"))?;
            write(&work.join("note.txt"), "agent");
            assert_eq!(checkpoints.restore_last()?.warning, None);
        }
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn undo_does_not_blame_the_turn_for_edits_made_before_it_began() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = crate::test_support::unique_temp_dir("undo-before");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        user_git(&work, &["init", "--template="])?;
        write(&work.join("mine.rs"), "my own edit");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        assert_eq!(checkpoints.restore_last()?.warning, None);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn a_planted_filter_does_not_run_for_the_undo_report_or_the_fingerprint() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = crate::test_support::unique_temp_dir("undo-filter");
        let home = root.join("home");
        let work = root.join("work");
        let marker = root.join("MARKER");
        fs::create_dir_all(&work)?;
        write(&work.join("a.txt"), "v1");
        write(&work.join(".gitattributes"), "a.txt filter=evil\n");
        user_git(&work, &["init", "--template="])?;
        user_git(&work, &["add", "."])?;
        commit(&work, "init")?;
        let planted = format!("touch '{}'; cat", marker.display());
        user_git(&work, &["config", "--local", "filter.evil.clean", &planted])?;
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&work.join("a.txt"), "changed");
        assert!(checkpoints.work_fingerprint(|_| false).is_some());
        checkpoints.restore_last()?;
        assert!(!marker.exists(), "the planted filter ran");
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn long_lists_of_unrestored_files_are_summarized() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = crate::test_support::unique_temp_dir("undo-many");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        user_git(&work, &["init", "--template="])?;
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        for index in 0..MAX_UNRESTORED_LISTED + 3 {
            write(&work.join(format!("file{index:02}.txt")), "new");
        }
        let warning = checkpoints.restore_last()?.warning.expect("reported");
        assert!(warning.contains("and 3 more"), "{warning}");
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn fingerprints_follow_changes_and_skip_ignored_paths() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = crate::test_support::unique_temp_dir("fingerprint");
        let home = root.join("home");
        let work = root.join("work");
        let document = |path: &Path| path.extension().is_some_and(|ext| ext == "md");
        fs::create_dir_all(&work)?;
        let checkpoints = Checkpoints::open(&home, "sess", work.clone());
        assert_eq!(checkpoints.work_fingerprint(document), None, "not a repo");

        write(&work.join("code.rs"), "v1");
        user_git(&work, &["init", "--template="])?;
        user_git(&work, &["add", "."])?;
        commit(&work, "init")?;
        let clean = checkpoints.work_fingerprint(document);
        assert!(clean.is_some());

        write(&work.join("notes.md"), "ignored");
        assert_eq!(checkpoints.work_fingerprint(document), clean);

        write(&work.join("code.rs"), "v2");
        let changed = checkpoints.work_fingerprint(document);
        assert_ne!(changed, clean);

        // Already changed, and changed again: only size and time can tell.
        write(&work.join("code.rs"), "v2 and more");
        let again = checkpoints.work_fingerprint(document);
        assert_ne!(again, changed);

        // A change that was committed leaves a clean status behind.
        user_git(&work, &["add", "code.rs"])?;
        commit(&work, "second")?;
        assert_ne!(checkpoints.work_fingerprint(document), clean);
        assert_ne!(checkpoints.work_fingerprint(document), again);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn unborn_head_skips_reset() -> Result<(), Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_root("unborn");
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&work)?;
        user_git(&work, &["init", "--template="])?;
        write(&work.join("file.txt"), "before");
        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("file.txt"))?;
        write(&work.join("file.txt"), "after");
        let report = checkpoints.restore_last()?;
        assert!(report.restored);
        assert!(!report.reset_head);
        assert_eq!(fs::read_to_string(work.join("file.txt"))?, "before");
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
