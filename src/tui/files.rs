//! Lazy project file index backing `@` mention completion.
//!
//! The index is built on first use and cached for the session so typing `@`
//! stays cheap. Listing prefers `git ls-files` (which respects `.gitignore`);
//! outside a repository a bounded directory walk is used instead.

use std::path::Path;
use std::process::{Command, Stdio};

/// Upper bound on indexed paths so pathological trees stay cheap to rank.
const MAX_INDEX_ENTRIES: usize = 20_000;

/// Upper bound on matches offered to the menu. The menu window wraps with
/// Up/Down, so a short ranked list is more useful than an exhaustive one.
const MAX_MENTION_MATCHES: usize = 32;

/// Directories the fallback walk never descends into.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "build", "__pycache__"];

#[derive(Default)]
pub(super) struct FileIndex {
    entries: Option<Vec<String>>,
    cached_query: Option<String>,
    cached_matches: Vec<String>,
}

impl FileIndex {
    /// Ranked relative paths matching `query`, building the index on first
    /// use. An empty query lists the front of the index.
    pub(super) fn matches(&mut self, query: &str) -> &[String] {
        let entries = self
            .entries
            .get_or_insert_with(|| scan(&crate::config::working_dir()));
        if self.cached_query.as_deref() != Some(query) {
            self.cached_matches = rank_matches(entries, query);
            self.cached_query = Some(query.to_string());
        }
        &self.cached_matches
    }

    #[cfg(test)]
    pub(super) fn with_entries(entries: Vec<String>) -> Self {
        Self {
            entries: Some(entries),
            ..Self::default()
        }
    }
}

pub(super) fn scan(root: &Path) -> Vec<String> {
    let mut files = git_files(root).unwrap_or_else(|| walk(root));
    files.sort_unstable();
    files.dedup();
    files.truncate(MAX_INDEX_ENTRIES);
    files
}

/// Tracked and untracked-but-not-ignored files from git, or `None` when the
/// directory is not a repository or git is unavailable.
fn git_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| String::from_utf8(path.to_vec()).ok())
            .take(MAX_INDEX_ENTRIES)
            .collect(),
    )
}

/// Bounded, iterative walk used outside git repositories. Skips hidden
/// entries, symlinks, and well-known build directories.
fn walk(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_INDEX_ENTRIES {
                return files;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if let Ok(relative) = path.strip_prefix(root)
                && let Some(relative) = relative.to_str()
            {
                files.push(relative.replace(std::path::MAIN_SEPARATOR, "/"));
            }
        }
    }
    files
}

/// Match quality, ordered best-first. File-name hits outrank path hits so
/// typing a name surfaces the file before every directory that contains it.
fn score(path: &str, query: &str) -> Option<u8> {
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    if name.starts_with(query) {
        return Some(0);
    }
    if name.contains(query) {
        return Some(1);
    }
    let path = path.to_lowercase();
    if path.contains(query) {
        return Some(2);
    }
    is_subsequence(query, &path).then_some(3)
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}

pub(super) fn rank_matches(entries: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, &String)> = entries
        .iter()
        .filter_map(|path| score(path, &query).map(|rank| (rank, path)))
        .collect();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(b.1))
    });
    scored
        .into_iter()
        .take(MAX_MENTION_MATCHES)
        .map(|(_, path)| path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    #[test]
    fn cached_matches_follow_query_changes_and_keep_order() {
        let paths = entries(&["src/Config.rs", "README.md", "config/notes.md"]);
        let mut index = FileIndex::with_entries(paths.clone());
        for query in ["", "config", "config", "CONFIG", "zzzz", "zzzz", "rdme", ""] {
            assert_eq!(index.matches(query), rank_matches(&paths, query));
        }
        let storage = index.matches("").as_ptr();
        assert_eq!(index.matches("").as_ptr(), storage);
    }

    #[test]
    fn file_name_matches_outrank_directory_matches() {
        let entries = entries(&[
            "src/render/helpers.rs",
            "src/tui/render.rs",
            "docs/render-notes.md",
        ]);
        let matches = rank_matches(&entries, "render");
        assert_eq!(
            matches,
            [
                "src/tui/render.rs",
                "docs/render-notes.md",
                "src/render/helpers.rs",
            ]
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_supports_subsequences() {
        let entries = entries(&["src/Config.rs", "README.md"]);
        assert_eq!(rank_matches(&entries, "config"), ["src/Config.rs"]);
        assert_eq!(rank_matches(&entries, "rdme"), ["README.md"]);
        assert!(rank_matches(&entries, "zzz").is_empty());
    }

    #[test]
    fn empty_query_lists_entries() {
        let entries = entries(&["b.rs", "a.rs"]);
        assert_eq!(rank_matches(&entries, ""), ["a.rs", "b.rs"]);
    }

    #[test]
    fn walk_skips_hidden_and_build_directories() -> std::io::Result<()> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/files-tests")
            .join(format!("walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src"))?;
        std::fs::create_dir_all(dir.join(".git"))?;
        std::fs::create_dir_all(dir.join("target"))?;
        std::fs::write(dir.join("src/main.rs"), "")?;
        std::fs::write(dir.join(".hidden"), "")?;
        std::fs::write(dir.join(".git/config"), "")?;
        std::fs::write(dir.join("target/out"), "")?;
        std::fs::write(dir.join("README.md"), "")?;

        let mut files = walk(&dir);
        files.sort_unstable();
        assert_eq!(files, ["README.md", "src/main.rs"]);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
