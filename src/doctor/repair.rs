//! Interactive repair. Walks fixable findings, asks before each write, and
//! backs the file up once before its first edit.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{read_json_object, write_json_object};
use crate::error::Error;

use super::{Finding, Fix};

/// Offers each fixable finding for confirmation. Returns whether anything
/// was applied, so the caller can re-run the checks.
///
/// # Errors
///
/// Returns an error when a confirmed repair fails on disk, and
/// [`Error::Interrupted`] when Ctrl+C interrupts a repair prompt.
pub(super) fn offer(findings: &[Finding]) -> Result<bool, Error> {
    let fixable = findings
        .iter()
        .filter(|finding| finding.fix.is_some())
        .count();
    if fixable == 0 {
        return Ok(false);
    }
    println!("\n{fixable} finding(s) can be repaired automatically.");
    let mut apply_all = false;
    let mut applied = false;
    let mut backed_up: HashSet<PathBuf> = HashSet::new();
    for finding in findings {
        let Some(fix) = &finding.fix else {
            continue;
        };
        if needs_confirmation(fix, apply_all) {
            print!("Fix: {}. Apply? [y/N, a=all] ", fix.describe());
            io::stdout().flush()?;
            let answer = read_answer()?;
            match answer.as_str() {
                "y" | "yes" => {}
                "a" | "all" => apply_all = true,
                _ => {
                    println!("Skipped.");
                    continue;
                }
            }
        }
        backup_once(fix, &mut backed_up)?;
        apply(fix)?;
        applied = true;
        println!("Repaired: {}.", fix.describe());
    }
    Ok(applied)
}

fn needs_confirmation(fix: &Fix, apply_all: bool) -> bool {
    !apply_all || matches!(fix, Fix::RestoreBackup { .. })
}

fn read_answer() -> Result<String, Error> {
    let stdin = io::stdin();
    read_answer_from(&mut stdin.lock())
}

fn read_answer_from(input: &mut impl Read) -> Result<String, Error> {
    let mut bytes = Vec::new();
    loop {
        if crate::interrupted() {
            return Err(Error::Interrupted);
        }
        let mut byte = [0u8; 1];
        match input.read(&mut byte) {
            Ok(_) if crate::interrupted() => return Err(Error::Interrupted),
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => bytes.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Err(Error::Interrupted);
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
    let line = String::from_utf8(bytes).map_err(|error| {
        Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            error.utf8_error(),
        ))
    })?;
    Ok(line.trim().to_lowercase())
}

/// Copies the target file aside before its first in-place edit. Quarantine
/// moves already preserve the original content. Restore handles its own
/// backup because it must preserve the live file at the moment of restore.
fn backup_once(fix: &Fix, backed_up: &mut HashSet<PathBuf>) -> Result<(), Error> {
    let target = match fix {
        Fix::QuarantineFile { .. } | Fix::RestoreBackup { .. } => return Ok(()),
        Fix::RemoveKey { path, .. } | Fix::SetValue { path, .. } | Fix::ChmodPrivate { path } => {
            path
        }
    };
    if !backed_up.insert(target.clone()) || !target.is_file() {
        return Ok(());
    }
    let backup = preserve_file(target)?;
    println!("Backed up to {}.", backup.display());
    Ok(())
}

fn apply(fix: &Fix) -> Result<(), Error> {
    match fix {
        Fix::QuarantineFile { path } => {
            let quarantined = sibling(path, "invalid");
            std::fs::rename(path, &quarantined)?;
            println!("Moved to {}.", quarantined.display());
        }
        Fix::RemoveKey { path, keys } => {
            let mut value = Value::Object(read_json_object(path)?);
            if super::remove_nested(&mut value, keys)
                && let Value::Object(updated) = value
            {
                write_json_object(path, &updated)?;
            }
        }
        Fix::SetValue { path, keys, value } => {
            let mut current = Value::Object(read_json_object(path)?);
            if super::set_nested(&mut current, keys, value.clone())
                && let Value::Object(updated) = current
            {
                write_json_object(path, &updated)?;
            }
        }
        Fix::ChmodPrivate { path } => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Fix::RestoreBackup { from, to } => {
            if to.is_file() {
                let backup = preserve_file(to)?;
                println!("Backed up to {}.", backup.display());
            }
            std::fs::rename(from, to)?;
        }
    }
    Ok(())
}

fn preserve_file(path: &Path) -> Result<PathBuf, Error> {
    let backup = sibling(path, "bak");
    std::fs::copy(path, &backup)?;
    Ok(backup)
}

/// `config.json` becomes `config.json.kind-<timestamp>` beside itself.
fn sibling(path: &Path, kind: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.json".into());
    path.with_file_name(format!("{name}.{kind}-{nonce}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirs {
        root: PathBuf,
        global: PathBuf,
    }

    impl TestDirs {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "yawl-doctor-repair-{}-{nonce}-{name}",
                std::process::id()
            ));
            let dirs = Self {
                global: root.join("home/.yawl/config.json"),
                root,
            };
            fs::create_dir_all(dirs.global.parent().unwrap()).unwrap();
            dirs
        }

        fn write(&self, text: &str) {
            fs::write(&self.global, text).unwrap();
        }

        fn read(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.global).unwrap()).unwrap()
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn remove_key_deletes_nested_paths() -> Result<(), Error> {
        let dirs = TestDirs::new("remove");
        dirs.write(r#"{"providers":{"omlx":{"api_key":5}},"max_tokens":2048}"#);

        apply(&Fix::RemoveKey {
            path: dirs.global.clone(),
            keys: vec!["providers".into(), "omlx".into(), "api_key".into()],
        })?;

        let saved = dirs.read();
        assert!(saved["providers"]["omlx"].get("api_key").is_none());
        assert_eq!(saved["max_tokens"], 2048);
        assert!(saved.get("providers").is_some());
        Ok(())
    }

    #[test]
    fn invalid_array_entries_are_removed_without_index_shifting() -> Result<(), Error> {
        let dirs = TestDirs::new("array-removals");
        dirs.write(
            r#"{"providers":{"local":{"models":[{"id":"bad-a","contextWindow":0},{"id":"good","contextWindow":4096},{"id":"bad-b","contextWindow":0}]}}}"#,
        );
        let paths = super::super::Paths {
            global: dirs.global.clone(),
            project: dirs.root.join("project/.yawl/config.json"),
            auth: dirs.root.join("home/.yawl/auth.json"),
        };
        let findings = super::super::checks::run(&paths);

        for fix in findings.iter().filter_map(|finding| finding.fix.as_ref()) {
            if matches!(fix, Fix::RemoveKey { keys, .. } if keys.get(2).is_some_and(|key| key == "models"))
            {
                apply(fix)?;
            }
        }

        let models = dirs.read()["providers"]["local"]["models"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            models,
            vec![serde_json::json!({"id":"good","contextWindow":4096})]
        );
        Ok(())
    }

    #[test]
    fn set_value_writes_nested_defaults() -> Result<(), Error> {
        let dirs = TestDirs::new("set");
        dirs.write(r#"{"max_tokens":0}"#);

        apply(&Fix::SetValue {
            path: dirs.global.clone(),
            keys: vec!["max_tokens".into()],
            value: serde_json::json!(8192),
        })?;

        assert_eq!(dirs.read()["max_tokens"], 8192);
        Ok(())
    }

    #[test]
    fn quarantine_renames_the_broken_file_aside() -> Result<(), Error> {
        let dirs = TestDirs::new("quarantine");
        dirs.write("{ not json");

        apply(&Fix::QuarantineFile {
            path: dirs.global.clone(),
        })?;

        assert!(!dirs.global.exists());
        let aside = fs::read_dir(dirs.global.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("config.json.invalid-"));
        assert!(aside.is_some(), "the broken file should be renamed aside");
        Ok(())
    }

    #[test]
    fn restore_backup_replaces_and_preserves_the_live_file() -> Result<(), Error> {
        let dirs = TestDirs::new("restore");
        dirs.write(r#"{"model":"ghost:m"}"#);
        let backup = dirs.global.parent().unwrap().join("config.json.bak-1");
        fs::write(&backup, r#"{"model":"ollama:llama4"}"#).unwrap();

        apply(&Fix::RestoreBackup {
            from: backup,
            to: dirs.global.clone(),
        })?;

        assert_eq!(dirs.read()["model"], "ollama:llama4");
        let preserved_live = fs::read_dir(dirs.global.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.json.bak-")
            })
            .any(|entry| {
                fs::read_to_string(entry.path())
                    .is_ok_and(|text| text.contains(r#""model":"ghost:m""#))
            });
        assert!(
            preserved_live,
            "the previous live config should be backed up"
        );
        Ok(())
    }

    #[test]
    fn repair_all_still_requires_confirmation_for_restore() {
        let path = PathBuf::from("config.json");
        let ordinary = Fix::RemoveKey {
            path: path.clone(),
            keys: vec!["max_tokens".into()],
        };
        let restore = Fix::RestoreBackup {
            from: PathBuf::from("config.json.bak-1"),
            to: path,
        };

        assert!(!needs_confirmation(&ordinary, true));
        assert!(needs_confirmation(&restore, true));
    }

    struct InterruptedInput;

    impl std::io::Read for InterruptedInput {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(io::ErrorKind::Interrupted.into())
        }
    }

    #[test]
    fn interrupted_repair_prompt_propagates_interruption() {
        let error = read_answer_from(&mut InterruptedInput)
            .expect_err("an interrupted prompt should stop doctor repair");

        assert!(matches!(error, Error::Interrupted));
    }

    #[test]
    fn nested_navigation_handles_array_indexes() {
        let mut value =
            serde_json::json!({"models": [{"id": "a"}, {"id": "b", "contextWindow": 0}]});
        let keys = vec!["models".to_string(), "1".to_string()];

        assert!(super::super::value_at(&value, &keys).is_some());
        assert!(super::super::remove_nested(&mut value, &keys));
        assert_eq!(value["models"].as_array().map(Vec::len), Some(1));
        assert!(!super::super::remove_nested(&mut value, &keys));
    }
}
