//! Bounded native file tools. No shell execution.
//!
//! `read_file`, `write_file`, and `edit_file` are builtins; `list_files` and
//! `search_files` are opt-in discovery tools for restricted children.

mod discovery;
mod read;
#[cfg(test)]
mod test_support;
mod write;

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde_json::Value;

pub(super) use discovery::{discover, entries};
pub(super) use read::read_file_for_model;
pub(super) use write::{edit_file, write_file};

const MAX_PAGE_BYTES: usize = 32 * 1024;

/// The `read_file`, `write_file`, and `edit_file` entries.
pub(super) fn builtin_entries() -> Vec<super::ToolEntry> {
    vec![read::entry(), write::write_entry(), write::edit_entry()]
}

fn number(args: &Value, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|value| (min..=max).contains(value))
            .ok_or_else(|| format!("'{key}' must be an integer between {min} and {max}")),
    }
}

fn check_canceled() -> Result<(), String> {
    if crate::cancellation::interrupted() {
        Err("file inspection interrupted".into())
    } else {
        Ok(())
    }
}

fn regular_file(path: &Path) -> std::io::Result<File> {
    // O_NONBLOCK prevents a replaced path pointing at a FIFO from blocking
    // before metadata can reject it; O_NOFOLLOW rejects final symlinks.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("path is not a regular file"));
    }
    Ok(file)
}
