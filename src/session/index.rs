//! Bounded, process-local cache for repeatedly opening the session picker.

use super::{SessionHeader, read_header};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(PartialEq, Eq)]
struct Signature {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn signature(path: &Path) -> Option<Signature> {
    let metadata = fs::metadata(path).ok()?;
    Some(Signature {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[derive(Default)]
struct HeaderCache(HashMap<PathBuf, (Signature, SessionHeader)>);

impl HeaderCache {
    fn get(&self, path: &Path, signature: &Signature) -> Option<SessionHeader> {
        self.0
            .get(path)
            .filter(|(saved, _)| saved == signature)
            .map(|(_, header)| header.clone())
    }

    fn insert(&mut self, path: &Path, signature: Signature, header: SessionHeader) {
        // A picker needs little metadata. Bound retained entries even when
        // projects or session files disappear during this process's lifetime.
        if self.0.len() >= 256 && !self.0.contains_key(path) {
            self.0.clear();
        }
        self.0.insert(path.to_path_buf(), (signature, header));
    }
}

pub(super) fn header(path: &Path) -> SessionHeader {
    static CACHE: OnceLock<Mutex<HeaderCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HeaderCache::default()));
    let before = signature(path);
    if let Some(signature) = &before
        && let Ok(guard) = cache.lock()
        && let Some(header) = guard.get(path, signature)
    {
        return header;
    }
    // Never hold the cache lock while scanning a potentially large log.
    let header = read_header(path);
    if before == signature(path)
        && let Some(signature) = before
        && !header.model.is_empty()
        && let Ok(mut guard) = cache.lock()
    {
        guard.insert(path, signature, header.clone());
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use crate::session::Session;

    #[test]
    fn metadata_invalidates_on_append_and_file_replacement() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("yawl-index-{nonce}"));
        let mut session = Session::create(&directory, &directory, "first").unwrap();
        session.append_message(&Message::user("Preview")).unwrap();
        let path = directory.join(format!("{}.jsonl", session.id));
        assert_eq!(header(&path).model, "first");
        assert_eq!(header(&path).preview, "Preview");
        session.append_model_switch("second").unwrap();
        assert_eq!(header(&path).model, "second");
        let previous = signature(&path).unwrap();
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, fs::read(&path).unwrap()).unwrap();
        fs::rename(replacement, &path).unwrap();
        assert!(signature(&path).unwrap() != previous);
        assert_eq!(header(&path).model, "second");
        drop(session);
        fs::remove_dir_all(directory).unwrap();
    }
}
