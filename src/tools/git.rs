//! Fixed, read-only Git inspection for restricted children.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{ToolEntry, ToolImpl, ToolOutcome};

pub(super) fn entry() -> ToolEntry {
    ToolEntry::new(crate::provider::ToolSpec {
            name: "git_inspect".into(),
            description: "Read Git status or staged/unstaged diffs in the working directory. Fixed read-only operations; no shell. Untracked contents require read_file. Output is bounded; narrow path when truncated.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["status", "unstaged_diff", "staged_diff"]},
                    "path": {"type": "string", "description": "Optional literal relative file or directory filter."}
                },
                "required": ["operation"],
                "additionalProperties": false
            }),
        },
        ToolImpl::GitInspect,
    )
}

pub(super) fn inspect(args: &Value) -> ToolOutcome {
    inspect_at(args, Path::new("."))
}

fn inspect_at(args: &Value, cwd: &Path) -> ToolOutcome {
    let Some(object) = args.as_object() else {
        return ToolOutcome::error("Expected an object.");
    };
    if object.keys().any(|key| key != "operation" && key != "path") {
        return ToolOutcome::error("Only operation and path are supported.");
    }
    let operation = match args["operation"].as_str() {
        Some(value @ ("status" | "unstaged_diff" | "staged_diff")) => value,
        _ => return ToolOutcome::error("Use status, unstaged_diff, or staged_diff."),
    };
    let path = match object.get("path") {
        None => None,
        Some(Value::String(path))
            if !path.is_empty()
                && !Path::new(path).is_absolute()
                && !Path::new(path).components().any(|part| {
                    // On Windows `\dir` and `C:dir` are not absolute but still
                    // leave the working directory.
                    matches!(
                        part,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) =>
        {
            Some(path)
        }
        _ => return ToolOutcome::error("path must be a nonempty relative path without '..'."),
    };
    let started = Instant::now();
    let timeout = Duration::from_secs(15);
    // Git can run clean/process filters even with --no-ext-diff and
    // --no-textconv. Refuse configured filters before inspecting the tree.
    // Use the same config environment, including repository config includes.
    let mut config = inspection_command(cwd);
    config.args([
        "config",
        "--name-only",
        "--get-regexp",
        r"^filter\..*\.(clean|process)$",
    ]);
    match super::exec::run_with_timeout(config, None, timeout) {
        Err(error) => {
            return ToolOutcome::error(format!("Git configuration check failed: {error}"));
        }
        Ok(result) if result.interrupted => {
            return ToolOutcome::error("Git inspection interrupted.");
        }
        Ok(result) if result.timed_out => {
            return ToolOutcome::error("Git inspection exceeded 15 seconds; narrow path.");
        }
        Ok(result) if result.status == Some(0) => {
            return ToolOutcome::error(
                "Git inspection refused: repository config defines clean or process filters that may execute commands. Use the parent agent's shell for inspection.",
            );
        }
        Ok(result) if result.status == Some(1) => {}
        Ok(result) => {
            return ToolOutcome::error(format!(
                "Git configuration check failed: {}",
                result.stderr.trim()
            ));
        }
    }
    let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
        return ToolOutcome::error("Git inspection exceeded 15 seconds; narrow path.");
    };
    let mut command = inspection_command(cwd);
    if operation == "status" {
        command.args([
            "status",
            "--short",
            "--untracked-files=normal",
            "--ignore-submodules=all",
        ]);
    } else {
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--submodule=short",
            "--no-color",
        ]);
        if operation == "staged_diff" {
            command.arg("--cached");
        }
    }
    command.arg("--");
    if let Some(path) = path {
        command.arg(path);
    }
    match super::exec::run_with_timeout(command, None, remaining) {
        Err(error) => ToolOutcome::error(format!("Git inspection failed: {error}")),
        Ok(result) if result.interrupted => ToolOutcome::error("Git inspection interrupted."),
        Ok(result) if result.timed_out => {
            ToolOutcome::error("Git inspection exceeded 15 seconds; narrow path.")
        }
        Ok(result) if result.status != Some(0) => {
            ToolOutcome::error(format!("Git inspection failed: {}", result.stderr.trim()))
        }
        Ok(result) => ToolOutcome::ok(if result.stdout.is_empty() {
            "No changes.".into()
        } else {
            result.stdout
        }),
    }
}

/// The file that always reads as empty.
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// `-c` settings that stop Git running the filter commands a repository's own
/// config defines. A clean filter runs on `git status` and `git diff`, and
/// unlike hooks there is no single switch for it, so each one is named. Filters
/// from the user's own global config, such as Git LFS, keep working.
///
/// A blank filter means no filter, and `required` would otherwise make a blank
/// one fatal.
pub(crate) fn repository_filters_off(cwd: &Path) -> Vec<String> {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command.args([
        "config",
        "--show-scope",
        "--name-only",
        "-z",
        "--get-regexp",
        r"^filter\.",
    ]);
    let Ok(listed) = super::exec::run_with_timeout(command, None, Duration::from_secs(5)) else {
        return Vec::new();
    };
    filters_off(&listed.stdout)
}

/// Reads `scope NUL name NUL` pairs from `git config --show-scope`.
fn filters_off(listing: &str) -> Vec<String> {
    let fields = listing.split('\0').collect::<Vec<_>>();
    let (pairs, _) = fields.as_chunks::<2>();
    pairs
        .iter()
        .filter(|[scope, _]| matches!(*scope, "local" | "worktree"))
        .filter_map(|[_, name]| {
            if name.ends_with(".required") {
                Some(format!("{name}=false"))
            } else if name.ends_with(".clean")
                || name.ends_with(".smudge")
                || name.ends_with(".process")
            {
                Some(format!("{name}="))
            } else {
                None
            }
        })
        .collect()
}

fn inspection_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    // Do not inherit Git routing, config injection, or helper overrides.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "color.ui=false",
        ]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn only_filters_from_the_repository_itself_are_turned_off() {
        let listing = "global\0filter.lfs.clean\0local\0filter.evil.clean\0local\0filter.evil.required\0worktree\0filter.w.process\0local\0filter.evil.description\0";
        assert_eq!(
            filters_off(listing),
            [
                "filter.evil.clean=",
                "filter.evil.required=false",
                "filter.w.process="
            ]
        );
        assert!(filters_off("").is_empty());
    }

    #[test]
    fn a_planted_clean_filter_does_not_run() {
        let root = TestDir::new();
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(root.path())
                .args(args)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !git(&["init", "--template="]) {
            return;
        }
        let marker = root.path().join("MARKER");
        fs::write(root.path().join("a.txt"), "one\n").unwrap();
        fs::write(root.path().join(".gitattributes"), "a.txt filter=evil\n").unwrap();
        assert!(git(&["add", ".gitattributes"]));
        let planted = format!("touch '{}'; cat", marker.display());
        assert!(git(&["config", "--local", "filter.evil.clean", &planted]));
        assert!(git(&["config", "--local", "filter.evil.required", "true"]));
        assert!(git(&["config", "--local", "include.path", "extra.config"]));
        fs::write(
            root.path().join(".git/extra.config"),
            "[filter \"sneaky\"]\n\tclean = touch elsewhere\n",
        )
        .unwrap();

        let off = repository_filters_off(root.path());
        assert!(off.contains(&"filter.evil.clean=".to_string()), "{off:?}");
        assert!(
            off.contains(&"filter.sneaky.clean=".to_string()),
            "included too: {off:?}"
        );

        let mut status = Command::new("git");
        status.current_dir(root.path());
        for setting in &off {
            status.args(["-c", setting]);
        }
        status.args(["status", "--porcelain"]);
        let output = status.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!marker.exists(), "the planted filter ran");
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = crate::test_support::unique_temp_dir("git-inspection");
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn shows_staged_unstaged_and_untracked_without_changing_index() {
        let temp = TestDir::new();
        let root = temp.path();
        git(root, &["init", "-q"]);
        fs::write(root.join("file"), "staged\n").unwrap();
        git(root, &["add", "file"]);
        fs::write(root.join("file"), "unstaged\n").unwrap();
        fs::write(root.join("new"), "untracked\n").unwrap();
        let index = fs::read(root.join(".git/index")).unwrap();
        for (operation, expected) in [
            ("status", "?? new"),
            ("staged_diff", "+staged"),
            ("unstaged_diff", "+unstaged"),
        ] {
            let result = inspect_at(&json!({"operation": operation}), root);
            assert!(!result.is_error, "{}", result.content);
            assert!(result.content.contains(expected), "{}", result.content);
        }
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read_to_string(root.join("file")).unwrap(), "unstaged\n");
        let filtered = inspect_at(&json!({"operation": "status", "path": "file"}), root);
        assert!(!filtered.content.contains("new"));
        let literal = inspect_at(&json!({"operation": "status", "path": ":(glob)*"}), root);
        assert_eq!(literal.content, "No changes.");
    }

    #[test]
    fn large_diff_is_bounded_and_marked_truncated() {
        let temp = TestDir::new();
        let root = temp.path();
        git(root, &["init", "-q"]);
        fs::write(root.join("file"), "line of source text\n".repeat(20_000)).unwrap();
        git(root, &["add", "file"]);
        let result = inspect_at(&json!({"operation": "staged_diff"}), root);
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.len() < 66_000);
        assert!(result.content.contains("truncated"));
    }

    #[test]
    fn rejects_commands_flags_and_path_escape() {
        for args in [
            json!({"operation": "reset"}),
            json!({"operation": "status", "args": ["--hard"]}),
            json!({"operation": "status", "path": "../other"}),
            json!({"operation": "status", "path": "/tmp"}),
        ] {
            assert!(inspect_at(&args, Path::new(".")).is_error);
        }
    }

    #[test]
    fn refuses_clean_and_process_filters_in_included_config() {
        for kind in ["clean", "process"] {
            let temp = TestDir::new();
            let root = temp.path();
            git(root, &["init", "-q"]);
            fs::write(root.join("file"), "before\n").unwrap();
            git(root, &["add", "file"]);
            fs::write(root.join("file"), "after\n").unwrap();
            fs::write(root.join(".gitattributes"), "file filter=unsafe\n").unwrap();
            let included = root.join("filters.config");
            fs::write(
                &included,
                format!("[filter \"unsafe\"]\n{kind} = touch ran-filter; cat\nrequired = true\n"),
            )
            .unwrap();
            git(
                root,
                &["config", "include.path", included.to_str().unwrap()],
            );
            let index = fs::read(root.join(".git/index")).unwrap();
            for operation in ["status", "unstaged_diff", "staged_diff"] {
                let result = inspect_at(&json!({"operation": operation}), root);
                assert!(result.is_error, "{}", result.content);
                assert!(result.content.contains("clean or process filters"));
                assert!(!root.join("ran-filter").exists());
            }
            assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
            assert_eq!(fs::read_to_string(root.join("file")).unwrap(), "after\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn disables_repository_command_hooks() {
        let temp = TestDir::new();
        let root = temp.path();
        git(root, &["init", "-q"]);
        fs::write(root.join("file"), "before\n").unwrap();
        git(root, &["add", "file"]);
        fs::write(root.join("file"), "after\n").unwrap();
        fs::write(root.join(".gitattributes"), "file diff=unsafe\n").unwrap();
        let script = root.join("hook");
        fs::write(&script, "#!/bin/sh\ntouch ran-hook\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        for key in [
            "core.fsmonitor",
            "diff.external",
            "diff.unsafe.command",
            "diff.unsafe.textconv",
        ] {
            git(root, &["config", key, script.to_str().unwrap()]);
        }
        for operation in ["status", "unstaged_diff", "staged_diff"] {
            let result = inspect_at(&json!({"operation": operation}), root);
            assert!(!result.is_error, "{}", result.content);
            assert!(!root.join("ran-hook").exists());
        }
    }
}
