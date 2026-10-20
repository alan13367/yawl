//! Bounded native file tools. No shell execution.
//!
//! `read_file`, `write_file`, and `edit_file` are builtins; `list_files` and
//! `search_files` are opt-in discovery tools for restricted children.

mod discovery;
mod open;
mod read;
#[cfg(test)]
mod test_support;
mod write;

use std::fs::File;
use std::path::Path;

use serde_json::Value;

pub(super) use discovery::{discover, entries};
use open::open_read;
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

/// Opens a regular file without following a final symlink.
fn regular_file(path: &Path) -> std::io::Result<File> {
    let file = open_read(path, false)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("path is not a regular file"));
    }
    Ok(file)
}

/// Windows opens a reserved name such as `CON`, `NUL`, or `COM1` as a device
/// wherever it appears, so writing to it would reach the terminal or a port
/// and reading it could block forever. Only Windows has such names.
#[cfg(windows)]
fn reject_device(path: &str) -> Result<(), String> {
    if crate::paths::names_device(Path::new(path)) {
        return Err(format!("{path} names a device, not a file"));
    }
    Ok(())
}

#[cfg(unix)]
fn reject_device(_path: &str) -> Result<(), String> {
    Ok(())
}
