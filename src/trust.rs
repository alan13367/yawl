//! Persistent project trust used to gate project-controlled skill sources.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{read_json_object, write_json_object};
use crate::error::Error;

const PATH_KEY_PREFIX: &str = "path-bytes-v1:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Trusted,
    Denied,
    Unknown,
}

pub fn project_root(cwd: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    canonical
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map_or_else(|| canonical.clone(), Path::to_path_buf)
}

pub fn load(home_dir: &Path, root: &Path) -> Result<Decision, Error> {
    let entries = read_json_object(&home_dir.join("trust.json"))?;
    let value = entries
        .get(&path_key(root))
        .or_else(|| root.to_str().and_then(|legacy_key| entries.get(legacy_key)));
    Ok(match value {
        Some(Value::Bool(true)) => Decision::Trusted,
        Some(Value::Bool(false)) => Decision::Denied,
        _ => Decision::Unknown,
    })
}

pub fn store(home_dir: &Path, root: &Path, trusted: bool) -> Result<(), Error> {
    let path = home_dir.join("trust.json");
    let mut entries = read_json_object(&path)?;
    if let Some(legacy_key) = root.to_str() {
        entries.remove(legacy_key);
    }
    entries.insert(path_key(root), Value::Bool(trusted));
    write_json_object(&path, &entries)
}

fn path_key(root: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bytes = root.as_os_str().as_encoded_bytes();
    let mut key = String::with_capacity(PATH_KEY_PREFIX.len() + bytes.len() * 2);
    key.push_str(PATH_KEY_PREFIX);
    for byte in bytes {
        key.push(char::from(HEX[usize::from(*byte >> 4)]));
        key.push(char::from(HEX[usize::from(*byte & 0x0f)]));
    }
    key
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "yawl-trust-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn project_root_uses_nearest_git_ancestor() {
        let root = temp_root("git-root");
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo/src/nested")).unwrap();

        assert_eq!(
            project_root(&root.join("repo/src/nested")),
            std::fs::canonicalize(root.join("repo")).unwrap()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stores_boolean_decisions_with_private_permissions() {
        let root = temp_root("store");
        let home = root.join("home/.yawl");
        let project = root.join("project");

        assert_eq!(load(&home, &project).unwrap(), Decision::Unknown);
        store(&home, &project, true).unwrap();
        assert_eq!(load(&home, &project).unwrap(), Decision::Trusted);
        assert_eq!(
            std::fs::metadata(home.join("trust.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store(&home, &project, false).unwrap();
        assert_eq!(load(&home, &project).unwrap(), Decision::Denied);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn distinct_non_utf8_paths_do_not_share_trust() {
        let root = temp_root("non-utf8");
        let home = root.join("home/.yawl");
        let first = PathBuf::from(OsString::from_vec(b"/tmp/yawl-project-\x80".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/tmp/yawl-project-\x81".to_vec()));
        assert_eq!(first.display().to_string(), second.display().to_string());

        store(&home, &first, true).unwrap();

        assert_eq!(load(&home, &first).unwrap(), Decision::Trusted);
        assert_eq!(load(&home, &second).unwrap(), Decision::Unknown);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reads_legacy_utf8_keys_and_migrates_them_on_store() {
        let root = temp_root("legacy-key");
        let home = root.join("home/.yawl");
        let project = root.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("trust.json"),
            serde_json::to_vec(&serde_json::json!({project.to_str().unwrap(): true})).unwrap(),
        )
        .unwrap();

        assert_eq!(load(&home, &project).unwrap(), Decision::Trusted);
        store(&home, &project, false).unwrap();

        let entries = read_json_object(&home.join("trust.json")).unwrap();
        assert!(!entries.contains_key(project.to_str().unwrap()));
        assert_eq!(entries.get(&path_key(&project)), Some(&Value::Bool(false)));
        let _ = std::fs::remove_dir_all(root);
    }
}
