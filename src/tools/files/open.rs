//! Opening files for the read tools without hanging on special files or
//! following a final symlink the caller did not ask to follow.

use std::fs::File;
use std::io;
use std::path::Path;

/// Opens `path` read-only. A FIFO or device is opened without blocking so the
/// caller can reject it by its metadata. With `follow_links` false, a final
/// symlink is an error instead of being followed.
#[cfg(unix)]
pub(super) fn open_read(path: &Path, follow_links: bool) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    // O_NONBLOCK prevents a replaced path pointing at a FIFO from blocking
    // before metadata can reject it; O_NOFOLLOW rejects final symlinks.
    let flags = if follow_links {
        libc::O_NONBLOCK
    } else {
        libc::O_NONBLOCK | libc::O_NOFOLLOW
    };
    OpenOptions::new().read(true).custom_flags(flags).open(path)
}

/// Opens `path` read-only. Windows has no filesystem FIFOs, but a reserved
/// name such as `CON` or `NUL` opens a device that can block a read forever,
/// so those names are refused before any open. With `follow_links` false, a
/// final symlink or junction is an error instead of being followed.
#[cfg(windows)]
pub(super) fn open_read(path: &Path, follow_links: bool) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    if crate::paths::names_device(path) {
        return Err(io::Error::other("path names a device, not a regular file"));
    }
    let opened = if follow_links {
        OpenOptions::new().read(true).open(path)
    } else {
        // The flag opens a final symlink or junction itself, not its target.
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    };
    let file = match opened {
        Ok(file) => file,
        // CreateFile refuses directories without backup semantics and reports
        // only "access is denied".
        Err(_) if path.is_dir() => return Err(io::Error::other("it is a directory")),
        Err(error) => return Err(error),
    };
    if follow_links {
        return Ok(file);
    }
    let metadata = file.metadata()?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "too many levels of symbolic links: the path is a symlink or junction",
        ));
    }
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // A reparse point that is not a link, such as a cloud placeholder,
        // is an ordinary file whose content a filter driver supplies. The
        // handle opened above may not read it, so open it the usual way.
        drop(file);
        return OpenOptions::new().read(true).open(path);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_regular_files_and_follows_links_only_on_request() {
        let root = super::super::test_support::TestDir::new();
        let file = root.write("plain.txt", "text");
        assert!(open_read(&file, false).is_ok());
        assert!(open_read(&file, true).is_ok());
        assert!(open_read(&root.0.join("missing"), false).is_err());

        #[cfg(unix)]
        {
            let link = root.0.join("link.txt");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(open_read(&link, false).is_err());
            assert!(open_read(&link, true).is_ok());
        }
    }

    #[cfg(windows)]
    #[test]
    fn refuses_devices_and_names_directories() {
        let root = super::super::test_support::TestDir::new();
        for name in ["CON", "nul", "aux.txt", "COM1"] {
            let error = open_read(Path::new(name), true).unwrap_err();
            assert!(error.to_string().contains("device"), "{name}: {error}");
        }
        let error = open_read(&root.0, true).unwrap_err();
        assert!(error.to_string().contains("directory"), "{error}");
    }
}
