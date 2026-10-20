//! Save verbose command and web output without filling every later request.

use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const INLINE_BYTES: usize = 16 * 1024;
const EXCERPT_BYTES: usize = 2 * 1024;
static NEXT_ARTIFACT: AtomicU64 = AtomicU64::new(0);

pub(super) fn prepare(directory: &Path, content: &mut String) {
    if content.len() <= INLINE_BYTES {
        return;
    }
    let result = save(directory, content);
    let head = content.floor_char_boundary(EXCERPT_BYTES);
    let tail = content.ceil_char_boundary(content.len().saturating_sub(EXCERPT_BYTES));
    let location = match result {
        Ok(path) => format!(
            "Full captured output ({} bytes): {}\nRead with read_file, offset=0, limit=16384; continue with next_offset.",
            content.len(),
            path.display()
        ),
        Err(error) => format!("[Could not save full output: {error}. Middle output omitted.]"),
    };
    *content = format!(
        "{location}\n\n{}\n\n[... middle omitted ...]\n\n{}",
        &content[..head],
        &content[tail..]
    );
}

pub(super) fn save(directory: &Path, content: &str) -> std::io::Result<PathBuf> {
    save_artifact(directory, "output", "txt", content)
}

/// Writes `content` to a new owner-only file named
/// `{prefix}-{pid}-{nanos}-{sequence}.{extension}` in `directory` and returns
/// its absolute path. Existing files are never replaced, and a failed write
/// leaves no partial file behind.
pub(crate) fn save_artifact(
    directory: &Path,
    prefix: &str,
    extension: &str,
    content: &str,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let directory = crate::paths::canonicalize(directory)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..16 {
        let sequence = NEXT_ARTIFACT.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "{prefix}-{}-{timestamp}-{sequence}.{extension}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Windows keeps files under the user profile private through the
        // inherited ACL, so only Unix needs an explicit mode.
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(content.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        return Ok(path);
    }
    Err(std::io::Error::other(format!(
        "could not allocate a unique {prefix} file"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_preserves_head_tail_and_full_unicode_file() {
        let directory = crate::test_support::unique_temp_dir("output");
        let original = format!("START\n{}\nFAILURE AT END", "é".repeat(20_000));
        let mut content = original.clone();
        prepare(&directory, &mut content);
        assert!(content.contains("START"));
        assert!(content.contains("FAILURE AT END"));
        assert!(content.len() < 6000);
        let path = fs::read_dir(&directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read_to_string(path).unwrap(), original);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_failure_is_explicit_and_small_outputs_need_no_storage() {
        // A directory cannot be created below a regular file.
        let root = crate::test_support::unique_temp_dir("output-blocked");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("blocker"), "file").unwrap();
        let directory = root.join("blocker/output");
        let directory = directory.as_path();
        let mut small = "small".to_string();
        prepare(directory, &mut small);
        assert_eq!(small, "small");
        let mut large = "x".repeat(20_000);
        prepare(directory, &mut large);
        assert!(large.contains("Could not save full output"));
        assert!(large.len() < 5000);
        fs::remove_dir_all(root).unwrap();
    }
}
