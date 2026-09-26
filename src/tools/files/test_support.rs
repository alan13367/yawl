//! Temporary-directory fixtures shared by the file tool tests.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct TestDir(pub(super) PathBuf);
impl TestDir {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "yawl-file-inspection-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture directory");
        Self(path)
    }

    pub(super) fn write(&self, relative: &str, text: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent directory");
        fs::write(&path, text).expect("fixture file");
        path
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("yawl-tools-{}-{name}", std::process::id()))
}
