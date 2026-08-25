//! Per-session working-tree checkpoints for `/undo`.
//!
//! Snapshots live under `~/.yawl/checkpoints/<session-id>/` and do not use
//! the project's git repository as the restore engine. When `git` is on
//! PATH a shadow repository records the tree; otherwise files are copied.
//! If the working directory is a git repo, the user's HEAD is recorded so
//! `/undo` can reset it when the agent moved it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::Error;

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".yawl",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
];
const EXCLUDE_FILE: &str = "\
.git
.yawl
target
node_modules
dist
build
.venv
venv
__pycache__
.DS_Store
*.pyc
.next
.cache
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotKind {
    Git,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UserHead {
    None,
    Unborn,
    Sha { sha: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Overlay {
    path: String,
    existed: bool,
    rel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnRecord {
    kind: SnapshotKind,
    commit: Option<String>,
    copy_rel: Option<String>,
    user_head: UserHead,
    overlays: Vec<Overlay>,
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: bool,
    pub reset_head: bool,
    pub warning: Option<String>,
}

/// Stack of pre-turn snapshots for one session.
pub struct Checkpoints {
    dir: PathBuf,
    work_tree: PathBuf,
    stack: Vec<TurnRecord>,
    git_available: bool,
}

impl Checkpoints {
    pub fn open(home_dir: &Path, session_id: &str, work_tree: PathBuf) -> Self {
        let dir = home_dir.join("checkpoints").join(session_id);
        let stack = read_stack(&dir).unwrap_or_default();
        Self {
            dir,
            work_tree,
            stack,
            git_available: git_available(),
        }
    }

    #[cfg(test)]
    fn open_with_git(
        home_dir: &Path,
        session_id: &str,
        work_tree: PathBuf,
        git_available: bool,
    ) -> Self {
        let mut checkpoints = Self::open(home_dir, session_id, work_tree);
        checkpoints.git_available = git_available;
        checkpoints
    }

    pub fn remove(home_dir: &Path, session_id: &str) {
        let _ = fs::remove_dir_all(home_dir.join("checkpoints").join(session_id));
    }

    /// Records the current working tree as a restore point for the next turn.
    ///
    /// # Errors
    ///
    /// Returns I/O or git errors when the snapshot cannot be written.
    pub fn snapshot(&mut self) -> Result<(), Error> {
        fs::create_dir_all(&self.dir)?;
        let user_head = read_user_head(&self.work_tree);
        let index = self.stack.len();
        let record = if self.git_available {
            match self.snapshot_git(user_head.clone()) {
                Ok(record) => record,
                Err(_) => self.snapshot_copy(index, user_head)?,
            }
        } else {
            self.snapshot_copy(index, user_head)?
        };
        self.stack.push(record);
        persist_stack(&self.dir, &self.stack)
    }

    fn snapshot_git(&self, user_head: UserHead) -> Result<TurnRecord, Error> {
        let git_dir = self.git_dir();
        ensure_shadow_repo(&git_dir, &self.work_tree)?;
        shadow_git(&git_dir, &self.work_tree, &["add", "-A"])?;
        shadow_git(
            &git_dir,
            &self.work_tree,
            &["commit", "--allow-empty", "-m", "checkpoint"],
        )?;
        let commit = shadow_git(&git_dir, &self.work_tree, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        Ok(TurnRecord {
            kind: SnapshotKind::Git,
            commit: Some(commit),
            copy_rel: None,
            user_head,
            overlays: Vec::new(),
        })
    }

    fn snapshot_copy(&self, index: usize, user_head: UserHead) -> Result<TurnRecord, Error> {
        let rel = format!("copies/{index}");
        let dest = self.dir.join(&rel);
        let _ = fs::remove_dir_all(&dest);
        copy_tree(&self.work_tree, &dest)?;
        Ok(TurnRecord {
            kind: SnapshotKind::Copy,
            commit: None,
            copy_rel: Some(rel),
            user_head,
            overlays: Vec::new(),
        })
    }

    fn git_dir(&self) -> PathBuf {
        self.dir.join("shadow.git")
    }

    /// Saves the pre-image of a `write_file`/`edit_file` path when it lies
    /// outside the snapshot root. No-op when there is no current turn.
    ///
    /// # Errors
    ///
    /// Returns I/O errors while reading or storing the overlay.
    pub fn remember_path(&mut self, path: &Path) -> Result<(), Error> {
        let Some(index) = self.stack.len().checked_sub(1) else {
            return Ok(());
        };
        let abs = absolute(path, &self.work_tree);
        if is_inside(&abs, &self.work_tree) {
            return Ok(());
        }
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
            Ok(meta) if meta.len() > MAX_FILE_BYTES => return Ok(()),
            Ok(_) => {
                fs::copy(&abs, &dest)?;
                true
            }
            Err(_) => false,
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
        let Some(turn) = self.stack.pop() else {
            return Ok(RestoreReport::default());
        };
        persist_stack(&self.dir, &self.stack)?;
        let mut report = RestoreReport {
            restored: true,
            reset_head: false,
            warning: None,
        };
        if let UserHead::Sha { sha } = &turn.user_head {
            let current = read_user_head(&self.work_tree);
            if let UserHead::Sha { sha: current_sha } = current
                && current_sha != *sha
            {
                match user_git(
                    &self.work_tree,
                    &["-c", "core.hooksPath=/dev/null", "reset", "--hard", sha],
                ) {
                    Ok(_) => report.reset_head = true,
                    Err(error) => {
                        report.warning = Some(format!("could not reset git HEAD: {error}"));
                    }
                }
            }
        }
        let file_result = match turn.kind {
            SnapshotKind::Git => match turn.commit.as_deref() {
                Some(commit) => restore_git(&self.git_dir(), &self.work_tree, commit),
                None => Err(Error::Protocol("checkpoint is missing a git commit".into())),
            },
            SnapshotKind::Copy => match turn.copy_rel.as_deref() {
                Some(rel) => restore_copy(&self.dir.join(rel), &self.work_tree),
                None => Err(Error::Protocol("checkpoint is missing a copy tree".into())),
            },
        };
        if let Err(error) = file_result {
            report.restored = false;
            report.warning = Some(match report.warning.take() {
                Some(existing) => format!("{existing}; {error}"),
                None => error.to_string(),
            });
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

fn read_stack(dir: &Path) -> Result<Vec<TurnRecord>, Error> {
    let path = dir.join("stack.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_slice(&bytes)?)
}

fn persist_stack(dir: &Path, stack: &[TurnRecord]) -> Result<(), Error> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("stack.json"), serde_json::to_vec(stack)?)?;
    Ok(())
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_shadow_repo(git_dir: &Path, work_tree: &Path) -> Result<(), Error> {
    if !git_dir.join("HEAD").exists() {
        fs::create_dir_all(git_dir)?;
        shadow_git(git_dir, work_tree, &["init", "--template="])?;
        let info = git_dir.join("info");
        fs::create_dir_all(&info)?;
        fs::write(info.join("exclude"), EXCLUDE_FILE)?;
    }
    Ok(())
}

fn restore_git(git_dir: &Path, work_tree: &Path, commit: &str) -> Result<(), Error> {
    let restored = shadow_git(
        git_dir,
        work_tree,
        &[
            "restore",
            "--source",
            commit,
            "--worktree",
            "--staged",
            "--",
            ".",
        ],
    );
    if restored.is_err() {
        shadow_git(git_dir, work_tree, &["checkout", "-f", commit, "--", "."])?;
    }
    let _ = shadow_git(git_dir, work_tree, &["clean", "-fd"]);
    Ok(())
}

fn restore_copy(snapshot: &Path, work_tree: &Path) -> Result<(), Error> {
    remove_untracked(work_tree, snapshot, work_tree)?;
    copy_tree(snapshot, work_tree)
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

fn copy_tree(src: &Path, dest: &Path) -> Result<(), Error> {
    fs::create_dir_all(dest)?;
    copy_walk(src, src, dest)
}

fn copy_walk(src_root: &Path, current: &Path, dest_root: &Path) -> Result<(), Error> {
    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if skip_dir(name_str) {
            continue;
        }
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            copy_walk(src_root, &path, dest_root)?;
            continue;
        }
        if skip_file(name_str) || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let rel = path.strip_prefix(src_root).unwrap_or(&path);
        let dest = dest_root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&path, &dest)?;
    }
    Ok(())
}

fn remove_untracked(current: &Path, snapshot: &Path, work_tree: &Path) -> Result<(), Error> {
    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if skip_dir(name_str) {
            continue;
        }
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let rel = path.strip_prefix(work_tree).unwrap_or(&path);
        let snap_path = snapshot.join(rel);
        if meta.is_dir() {
            remove_untracked(&path, snapshot, work_tree)?;
            if !snap_path.exists() {
                let _ = fs::remove_dir(&path);
            }
            continue;
        }
        if skip_file(name_str) {
            continue;
        }
        if !snap_path.exists() {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

fn skip_file(name: &str) -> bool {
    name == ".DS_Store" || name.ends_with(".pyc")
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn is_inside(path: &Path, root: &Path) -> bool {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let resolved = if path.exists() {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else if let Some(parent) = path.parent().filter(|parent| parent.exists()) {
        let file_name = path.file_name().unwrap_or_default();
        fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(file_name)
    } else {
        path.to_path_buf()
    };
    resolved.starts_with(&root)
}

fn read_user_head(work_tree: &Path) -> UserHead {
    match user_git(work_tree, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(value) if value.trim() == "true" => {}
        _ => return UserHead::None,
    }
    match user_git(work_tree, &["rev-parse", "--verify", "HEAD"]) {
        Ok(sha) => UserHead::Sha {
            sha: sha.trim().to_string(),
        },
        Err(_) => UserHead::Unborn,
    }
}

fn shadow_git(git_dir: &Path, work_tree: &Path, args: &[&str]) -> Result<String, Error> {
    run_git(
        Command::new("git")
            .arg("-c")
            .arg("user.name=yawl")
            .arg("-c")
            .arg("user.email=yawl@localhost")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("init.defaultBranch=yawl")
            .arg("--git-dir")
            .arg(git_dir)
            .arg("--work-tree")
            .arg(work_tree)
            .args(args),
    )
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
    fn copy_fallback_restores_create_modify_and_delete() -> Result<(), Error> {
        let root = temp_root("copy");
        let home = root.join("home");
        let work = root.join("work");
        write(&work.join("keep.txt"), "keep");
        write(&work.join("edit.txt"), "before");
        write(&work.join("gone.txt"), "delete-me");
        let mut checkpoints = Checkpoints::open_with_git(&home, "sess", work.clone(), false);
        checkpoints.snapshot()?;

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
    fn copy_fallback_skips_target_dir() -> Result<(), Error> {
        let root = temp_root("skip-target");
        let home = root.join("home");
        let work = root.join("work");
        write(&work.join("src.rs"), "src");
        write(&work.join("target/built"), "artifact");
        let mut checkpoints = Checkpoints::open_with_git(&home, "sess", work.clone(), false);
        checkpoints.snapshot()?;
        write(&work.join("src.rs"), "changed");
        write(&work.join("target/built"), "new-artifact");
        checkpoints.restore_last()?;
        assert_eq!(fs::read_to_string(work.join("src.rs"))?, "src");
        assert_eq!(
            fs::read_to_string(work.join("target/built"))?,
            "new-artifact"
        );
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
        let mut checkpoints = Checkpoints::open_with_git(&home, "sess", work.clone(), false);
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
        let mut checkpoints =
            Checkpoints::open_with_git(&root.join("home"), "sess", root.join("work"), false);
        let report = checkpoints.restore_last()?;
        assert!(!report.restored);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn git_shadow_restores_files_and_resets_moved_head() -> Result<(), Error> {
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

        let mut checkpoints = Checkpoints::open_with_git(&home, "sess", work.clone(), true);
        checkpoints.snapshot()?;
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
        let mut checkpoints = Checkpoints::open_with_git(&home, "sess", work.clone(), true);
        checkpoints.snapshot()?;
        write(&work.join("file.txt"), "after");
        let report = checkpoints.restore_last()?;
        assert!(report.restored);
        assert!(!report.reset_head);
        assert_eq!(fs::read_to_string(work.join("file.txt"))?, "before");
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
