//! Per-session touched-file checkpoints for `/undo`.
//!
//! A turn starts with an empty record. Before `write_file` or `edit_file`
//! runs, Yawl saves that path's pre-image under
//! `~/.yawl/checkpoints/<session-id>/`. Untouched paths are never scanned or
//! copied. If the working directory is a git repo, the user's HEAD is also
//! recorded so `/undo` can reset it when the agent moved it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::Error;

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const STORE_VERSION: u32 = 1;

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

/// Stack of touched-file restore points for one session.
pub struct Checkpoints {
    dir: PathBuf,
    work_tree: PathBuf,
    stack: Vec<TurnRecord>,
}

impl Checkpoints {
    pub fn open(home_dir: &Path, session_id: &str, work_tree: PathBuf) -> Self {
        let dir = home_dir.join("checkpoints").join(session_id);
        let stack = match read_stack(&dir) {
            Ok(Some(stack)) => stack,
            Ok(None) if !dir.exists() => Vec::new(),
            Ok(None) | Err(_) => {
                let _ = fs::remove_dir_all(&dir);
                Vec::new()
            }
        };
        Self {
            dir,
            work_tree,
            stack,
        }
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
        fs::create_dir_all(&self.dir)?;
        let index = self.stack.len();
        let _ = fs::remove_dir_all(self.dir.join("overlays").join(index.to_string()));
        let record = TurnRecord {
            user_head: read_user_head(&self.work_tree),
            overlays: Vec::new(),
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

    /// Pops the latest snapshot, resets user git HEAD if it moved, and
    /// restores files.
    ///
    /// # Errors
    ///
    /// Returns I/O errors while updating the stack file. Restore failures are
    /// reported on [`RestoreReport::warning`] so messages can still be dropped.
    pub fn restore_last(&mut self) -> Result<RestoreReport, Error> {
        let turn_index = self.stack.len().checked_sub(1);
        let Some(turn) = self.stack.pop() else {
            return Ok(RestoreReport::default());
        };
        persist_stack(&self.dir, &self.stack)?;
        let mut report = RestoreReport {
            restored: true,
            reset_head: false,
            warning: None,
        };
        let mut staged_to_restore = None;
        if let UserHead::Sha { sha, staged } = &turn.user_head {
            let current = read_user_head(&self.work_tree);
            if let UserHead::Sha {
                sha: current_sha, ..
            } = current
                && current_sha != *sha
            {
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
        if let Some(index) = turn_index {
            let _ = fs::remove_dir_all(self.dir.join("overlays").join(index.to_string()));
        }
        Ok(report)
    }
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
    let stored: StoredCheckpoints = serde_json::from_slice(&bytes)?;
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
    fs::write(dir.join("stack.json"), serde_json::to_vec(&stored)?)?;
    Ok(())
}

#[cfg(test)]
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
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
    user_git(
        &root,
        &["-c", "core.hooksPath=/dev/null", "reset", "--soft", sha],
    )?;
    user_git(&root, &["reset", "--", "."])?;
    Ok(())
}

fn restore_staged_paths(work_tree: &Path, staged: &[String]) -> Result<(), Error> {
    if staged.is_empty() {
        return Ok(());
    }
    let root = git_root(work_tree)?;
    run_git(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "--"])
            .args(staged),
    )?;
    Ok(())
}

fn user_git(work_tree: &Path, args: &[&str]) -> Result<String, Error> {
    run_git(Command::new("git").arg("-C").arg(work_tree).args(args))
}

fn run_git(cmd: &mut Command) -> Result<String, Error> {
    let output = cmd.stdin(Stdio::null()).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::Config("git is not installed".into())
        } else {
            Error::Io(error)
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(Error::Protocol(format!("git failed: {detail}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn open_discards_orphaned_legacy_snapshot_data() -> Result<(), Error> {
        let root = temp_root("legacy-orphan");
        let home = root.join("home");
        let work = root.join("work");
        let checkpoint_dir = home.join("checkpoints/sess");
        write(&checkpoint_dir.join("shadow.git/objects/orphan"), "git");
        write(&checkpoint_dir.join("copies/0/orphan"), "copy");
        fs::create_dir_all(&work)?;

        let checkpoints = Checkpoints::open(&home, "sess", work);

        assert!(checkpoints.stack.is_empty());
        assert!(!checkpoint_dir.exists());
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn open_discards_legacy_array_store() -> Result<(), Error> {
        let root = temp_root("legacy-array");
        let home = root.join("home");
        let work = root.join("work");
        let checkpoint_dir = home.join("checkpoints/sess");
        write(&checkpoint_dir.join("stack.json"), "[]");
        write(&checkpoint_dir.join("shadow.git/objects/legacy"), "git");
        fs::create_dir_all(&work)?;

        let checkpoints = Checkpoints::open(&home, "sess", work);

        assert!(checkpoints.stack.is_empty());
        assert!(!checkpoint_dir.exists());
        let _ = fs::remove_dir_all(root);
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
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .arg("-c")
                .arg("user.name=yawl")
                .arg("-c")
                .arg("user.email=yawl@localhost")
                .arg("-c")
                .arg("commit.gpgsign=false")
                .args(["commit", "-m", "init"]),
        )?;
        write(&work.join("dirty.txt"), "user-dirty");
        let original_head = user_git(&work, &["rev-parse", "HEAD"])?.trim().to_string();

        let mut checkpoints = Checkpoints::open(&home, "sess", work.clone());
        checkpoints.snapshot()?;
        checkpoints.remember_path(&work.join("tracked.txt"))?;
        checkpoints.remember_path(&work.join("added.txt"))?;
        write(&work.join("tracked.txt"), "agent");
        write(&work.join("added.txt"), "new");
        user_git(&work, &["add", "-A"])?;
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .arg("-c")
                .arg("user.name=yawl")
                .arg("-c")
                .arg("user.email=yawl@localhost")
                .arg("-c")
                .arg("commit.gpgsign=false")
                .args(["commit", "-m", "agent"]),
        )?;

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
    fn git_head_restore_recreates_preturn_staged_paths() -> Result<(), Error> {
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
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .arg("-c")
                .arg("user.name=yawl")
                .arg("-c")
                .arg("user.email=yawl@localhost")
                .arg("-c")
                .arg("commit.gpgsign=false")
                .args(["commit", "-m", "base"]),
        )?;
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
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .arg("-c")
                .arg("user.name=yawl")
                .arg("-c")
                .arg("user.email=yawl@localhost")
                .arg("-c")
                .arg("commit.gpgsign=false")
                .args(["commit", "-m", "agent"]),
        )?;

        let report = checkpoints.restore_last()?;

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
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(&work)
                .arg("-c")
                .arg("user.name=yawl")
                .arg("-c")
                .arg("user.email=yawl@localhost")
                .arg("-c")
                .arg("commit.gpgsign=false")
                .args(["commit", "-m", "init"]),
        )?;
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
