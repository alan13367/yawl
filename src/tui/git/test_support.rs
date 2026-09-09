//! Shared temporary repositories for Git unit tests.

use std::path::{Path, PathBuf};

pub(super) fn temp_dir_unique(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "yawl-git-init-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
/// Throwaway git identity as child-process env: `init_and_push_env`
/// commits without touching (or needing) the developer's real git
/// config. Passed per-`Command`, so — unlike process-environment
/// mutation — it stays confined to spawned children under the
/// multithreaded test harness. Returns the config path (kept alive by
/// the temp dir) for the env array.
pub(super) fn write_test_gitconfig(dir: &Path) -> Result<String, crate::error::Error> {
    let config = dir.join("gitconfig");
    std::fs::write(
        &config,
        "[user]\n\tname = yawl tests\n\temail = yawl@localhost\n[commit]\n\tgpgsign = false\n",
    )?;
    config
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| crate::error::Error::Protocol("non-UTF8 temp path".to_string()))
}
