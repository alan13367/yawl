//! Keep large child reports outside model history, with durable read_file paths.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::bounded;

const INLINE_BYTES: usize = 2048;
const EXCERPT_BYTES: usize = 1024;
static NEXT_REPORT: AtomicU64 = AtomicU64::new(1);

/// Small results remain unchanged. Long results become an opening excerpt
/// plus an absolute path. Files outlive worker pruning and session resumption.
/// Save failures are explicit and never expand the result back into history.
pub(super) fn prepare(directory: &Path, text: &str) -> String {
    if text.len() <= INLINE_BYTES {
        return text.to_string();
    }
    let excerpt = bounded(text, EXCERPT_BYTES);
    match save(directory, text) {
        Ok(path) => format!(
            "Full report ({} bytes): {}\nUse read_file with this path, offset=0 and limit=16384; continue with next_offset as needed.\n\nOpening excerpt:\n{excerpt}\n[excerpt ends]",
            text.len(),
            path.display()
        ),
        Err(error) => format!(
            "[Full report could not be saved: {}. Remaining output omitted; request a shorter report or repair artifact storage.]\n\nOpening excerpt:\n{excerpt}",
            bounded(&error.to_string(), 512)
        ),
    }
}

fn save(directory: &Path, text: &str) -> std::io::Result<std::path::PathBuf> {
    fs::create_dir_all(directory)?;
    let directory = fs::canonicalize(directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..16 {
        let sequence = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "report-{}-{timestamp}-{sequence}.md",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(text.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        return Ok(path);
    }
    Err(std::io::Error::other(
        "could not allocate a unique report file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("yawl-reports-{}-{label}", std::process::id()))
    }

    #[test]
    fn short_results_do_not_touch_disk() {
        let directory = directory("short");
        assert_eq!(prepare(&directory, "short result"), "short result");
        assert!(!directory.exists());
    }

    #[test]
    fn reports_preserve_all_bytes_and_never_overwrite_earlier_runs() {
        let directory = directory("large");
        let _ = fs::remove_dir_all(&directory);
        let text = format!(
            "Summary: evidence follows.\n{}\nFINAL EVIDENCE",
            "é".repeat(600_000)
        );
        let first = prepare(&directory, &text);
        let second = prepare(&directory, &text);
        assert!(first.len() < 2048);
        assert!(first.contains("Summary: evidence follows"));
        assert!(!first.contains("FINAL EVIDENCE"));
        assert!(first.contains("next_offset"));
        assert_ne!(first, second);
        let paths = fs::read_dir(&directory)
            .expect("report directory")
            .map(|entry| entry.expect("report").path())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 2);
        for path in paths {
            assert!(
                first.contains(&path.display().to_string())
                    || second.contains(&path.display().to_string())
            );
            assert_eq!(fs::read_to_string(path).expect("full report"), text);
        }
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn unwritable_storage_is_explicit_and_keeps_delivery_bounded() {
        let directory = directory("failure");
        fs::write(&directory, "a file, not a directory").expect("fixture");
        let result = prepare(&directory, &"x".repeat(100_000));
        assert!(result.contains("could not be saved"));
        assert!(result.len() < 2048);
        fs::remove_file(directory).expect("cleanup");
    }
}
