//! Windows file identity through `GetFileInformationByHandle`.

use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    GetFileInformationByHandle,
};

/// What names one file on an NTFS or ReFS volume, like a Unix device and
/// inode. A file replaced by rename gets a new index even when its length and
/// modification time match; unlike the creation time, the index is not
/// preserved by file tunneling after a delete and recreate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub(crate) volume: u32,
    pub(crate) index: u64,
    pub(crate) links: u32,
}

/// Reads the volume serial number, file index, and hard-link count of the
/// file at `path`, following symlinks.
///
/// # Errors
///
/// Returns the error from opening the file or querying its information.
pub(crate) fn file_info(path: &Path) -> io::Result<FileInfo> {
    // Attribute-only access cannot hit a sharing violation with writers, and
    // backup semantics lets the same call identify directories.
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: `BY_HANDLE_FILE_INFORMATION` is plain integers, so all-zero
    // bytes are a valid value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` owns a valid open handle for the whole call, and `info`
    // is a writable `BY_HANDLE_FILE_INFORMATION` the call fills in.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileInfo {
        volume: info.dwVolumeSerialNumber,
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        links: info.nNumberOfLinks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_replaced_files_and_hard_links() {
        let directory = crate::test_support::unique_temp_dir("file-info");
        std::fs::create_dir_all(&directory).unwrap();
        let first = directory.join("first");
        std::fs::write(&first, "a").unwrap();
        let before = file_info(&first).unwrap();
        assert_eq!(before.links, 1);

        let alias = directory.join("alias");
        std::fs::hard_link(&first, &alias).unwrap();
        let linked = file_info(&alias).unwrap();
        assert_eq!((linked.volume, linked.index), (before.volume, before.index));
        assert_eq!(linked.links, 2);

        let replacement = directory.join("replacement");
        std::fs::write(&replacement, "a").unwrap();
        std::fs::rename(&replacement, &first).unwrap();
        assert_ne!(file_info(&first).unwrap().index, before.index);
        assert!(file_info(&directory).is_ok());
        assert!(file_info(&directory.join("missing")).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
