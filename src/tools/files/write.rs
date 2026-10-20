//! `write_file` and `edit_file`: whole-file writes and exact string
//! replacement.

use std::borrow::Cow;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::reject_device;
use crate::tools::{ToolEntry, ToolImpl, ToolOutcome, str_arg};

/// Cap on `edit_file` input size. The whole file is read and rewritten, so a
/// pathologically large file would balloon memory before the match check.
const MAX_EDIT_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn write_entry() -> ToolEntry {
    ToolEntry::new(
        crate::provider::ToolSpec {
            name: "write_file".into(),
            description: "Write a file, creating parent directories; replaces existing content."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolImpl::WriteFile,
    )
}

pub(super) fn edit_entry() -> ToolEntry {
    ToolEntry::new(crate::provider::ToolSpec {
            name: "edit_file".into(),
            description: "Replace exact `old_string` text in a file. It must match once unless replace_all is true; include enough context for uniqueness."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence; default false."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }, ToolImpl::EditFile)
}

pub(in crate::tools) fn write_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match str_arg(args, "content") {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Err(error) = reject_device(path) {
        return ToolOutcome::error(error);
    }
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolOutcome::error(format!("cannot create {}: {e}", parent.display()));
    }
    let previous = fs::metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.len());
    match write_atomically(Path::new(path), content.as_bytes()) {
        Ok(()) => ToolOutcome::ok(match previous {
            Some(previous) => format!(
                "overwrote {path} ({} bytes; previously {previous} bytes)",
                content.len()
            ),
            None => format!("created {path} ({} bytes)", content.len()),
        }),
        Err(e) => ToolOutcome::error(format!("cannot write {path}: {e}")),
    }
}

pub(in crate::tools) fn edit_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old_string = match str_arg(args, "old_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let new_string = match str_arg(args, "new_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let replace_all = match args.get("replace_all") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return ToolOutcome::error("'replace_all' must be a boolean"),
    };
    if let Err(error) = reject_device(path) {
        return ToolOutcome::error(error);
    }
    if old_string.is_empty() {
        return ToolOutcome::error("old_string must not be empty");
    }
    if old_string == new_string {
        return ToolOutcome::error("old_string and new_string are identical; nothing to change");
    }
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_EDIT_FILE_BYTES => {
            return ToolOutcome::error(format!(
                "{path} exceeds the {MAX_EDIT_FILE_BYTES}-byte edit limit; use shell tools to edit it"
            ));
        }
        Ok(_) => {}
        Err(e) => return ToolOutcome::error(format!("cannot read {path}: {e}")),
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::error(format!("cannot read {path}: {e}")),
    };
    let (old_string, new_string, crlf) = match_line_endings(&text, old_string, new_string);
    let count = text.matches(old_string.as_ref()).count();
    if count == 0 {
        return ToolOutcome::error(format!(
            "old_string not found in {path}; {}",
            not_found_hint(&text, &old_string)
        ));
    }
    if count > 1 && !replace_all {
        return ToolOutcome::error(format!(
            "old_string appears {count} times in {path} (lines {}); add surrounding context to make it unique, or set replace_all",
            line_list(&match_lines(&text, &old_string))
        ));
    }
    let (updated, lines) = replace_matches(&text, &old_string, &new_string);
    if let Err(e) = write_atomically(Path::new(path), updated.as_bytes()) {
        return ToolOutcome::error(format!("cannot write {path}: {e}"));
    }
    let mut message = if count > 1 {
        format!(
            "edited {path} ({count} replacements at lines {})",
            line_list(&lines)
        )
    } else if new_string.is_empty() {
        format!("edited {path} (deleted text at line {})", lines[0])
    } else {
        let end = lines[0] + new_string.trim_end_matches('\n').matches('\n').count();
        if end > lines[0] {
            format!("edited {path} (lines {}-{end})", lines[0])
        } else {
            format!("edited {path} (line {})", lines[0])
        }
    };
    if crlf {
        message.push_str("; matched CRLF line endings");
    }
    ToolOutcome::ok(message)
}

/// Replaces a file's contents so that a crash or a full disk never leaves
/// it half written: the new contents go to a temporary file beside it,
/// which then takes its place. A symlink is written through to its target.
///
/// A file that a new file cannot stand in for is rewritten in place instead:
/// one with other hard links, anything other than a regular file, any file
/// whose temporary sibling cannot be created, and, on Unix, one owned by
/// another user or whose group the new file cannot take. What the
/// replacement carries over from the original differs by platform; see
/// [`replace_through_temporary`].
fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => match crate::paths::canonicalize(path)
        {
            Ok(target) => target,
            // A dangling link: create its target the usual way.
            Err(_) => return fs::write(path, contents),
        },
        Ok(_) => path.to_path_buf(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(error),
    };
    let original = match fs::metadata(&target) {
        Ok(metadata) if metadata.is_file() && has_one_link(&target, &metadata) => Some(metadata),
        Ok(_) => return fs::write(&target, contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let directory = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let name = target
        .file_name()
        .map_or_else(|| "file".into(), |name| name.to_string_lossy());
    let temporary = directory.join(format!(".{name}.yawl-{}.tmp", std::process::id()));
    match replace_through_temporary(&temporary, &target, original.as_ref(), contents) {
        Ok(true) => Ok(()),
        Ok(false) => fs::write(&target, contents),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn has_one_link(_path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() == 1
}

/// A file with other hard links keeps its identity so every name sees the new
/// contents. A file whose link count cannot be read is treated the same way.
#[cfg(windows)]
fn has_one_link(path: &Path, _metadata: &fs::Metadata) -> bool {
    crate::paths::file_info(path).is_ok_and(|info| info.links == 1)
}

/// Writes `contents` to `temporary` and renames it over `target`. Returns
/// `Ok(false)` when the replacement cannot stand in for the original and the
/// caller should rewrite `target` in place.
///
/// The temporary file gets the original group and mode before any contents
/// reach it. Extended attributes and ACLs are not copied to the replacement.
#[cfg(unix)]
fn replace_through_temporary(
    temporary: &Path,
    target: &Path,
    original: Option<&fs::Metadata>,
    contents: &[u8],
) -> std::io::Result<bool> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // An existing file's contents stay private until the temporary file has
    // its final mode; a new file starts with the default mode.
    let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if original.is_some() { 0o600 } else { 0o666 })
        .open(temporary)
    else {
        return Ok(false);
    };
    let replaced = (|| {
        if let Some(original) = original {
            let created = file.metadata()?;
            if created.uid() != original.uid() {
                return Ok(false);
            }
            if created.gid() != original.gid()
                && std::os::unix::fs::fchown(&file, None, Some(original.gid())).is_err()
            {
                return Ok(false);
            }
            // After the group change, which may clear set-id bits.
            file.set_permissions(original.permissions())?;
        }
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
        crate::paths::replace_file(temporary, target)?;
        Ok(true)
    })();
    if !matches!(replaced, Ok(true)) {
        let _ = fs::remove_file(temporary);
    }
    replaced
}

/// Windows has no owner, group, or mode to copy. The replacement inherits the
/// directory's ACL and loses the original's explicit ACL, hidden and system
/// attributes, and alternate data streams. The read-only attribute is the one
/// piece of `Permissions` there, and it is copied. Replacing a read-only file,
/// or one that another program holds open without sharing deletes, fails with
/// an error that says so.
#[cfg(windows)]
fn replace_through_temporary(
    temporary: &Path,
    target: &Path,
    original: Option<&fs::Metadata>,
    contents: &[u8],
) -> std::io::Result<bool> {
    let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
    else {
        return Ok(false);
    };
    let written = (|| {
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
        // After the contents, because a read-only file cannot be written.
        if let Some(original) = original {
            file.set_permissions(original.permissions())?;
        }
        Ok(())
    })();
    // Close the handle before the rename.
    drop(file);
    let replaced = written.and_then(|()| crate::paths::replace_file(temporary, target));
    if replaced.is_err() {
        let _ = fs::remove_file(temporary);
    }
    replaced.map(|()| true)
}

/// Files with CRLF line endings rarely match model text, which uses `\n`.
fn match_line_endings<'a>(
    text: &str,
    old: &'a str,
    new: &'a str,
) -> (Cow<'a, str>, Cow<'a, str>, bool) {
    if !text.contains("\r\n") || !old.contains('\n') || old.contains('\r') || text.contains(old) {
        return (old.into(), new.into(), false);
    }
    let crlf_old = old.replace('\n', "\r\n");
    if !text.contains(&crlf_old) {
        return (old.into(), new.into(), false);
    }
    let crlf_new = new.replace("\r\n", "\n").replace('\n', "\r\n");
    (crlf_old.into(), crlf_new.into(), true)
}

fn match_lines(text: &str, needle: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut line = 1;
    let mut last = 0;
    for (index, _) in text.match_indices(needle) {
        line += text[last..index].matches('\n').count();
        lines.push(line);
        last = index;
    }
    lines
}

/// Replaces every match and returns the updated text with the line where
/// each replacement starts in it.
fn replace_matches(text: &str, old: &str, new: &str) -> (String, Vec<usize>) {
    let mut updated = String::with_capacity(text.len());
    let mut lines = Vec::new();
    let mut line = 1;
    let mut last = 0;
    for (index, _) in text.match_indices(old) {
        let before = &text[last..index];
        line += before.matches('\n').count();
        updated.push_str(before);
        lines.push(line);
        updated.push_str(new);
        line += new.matches('\n').count();
        last = index + old.len();
    }
    updated.push_str(&text[last..]);
    (updated, lines)
}

fn line_list(lines: &[usize]) -> String {
    const SHOWN: usize = 10;
    let mut list = lines
        .iter()
        .take(SHOWN)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if lines.len() > SHOWN {
        list.push_str(", ...");
    }
    list
}

/// Points at text that matches apart from indentation or trailing spaces,
/// the most common reason an edit misses.
fn not_found_hint(text: &str, old: &str) -> String {
    let wanted = old.lines().map(str::trim).collect::<Vec<_>>();
    let first = wanted.iter().position(|line| !line.is_empty());
    let last = wanted.iter().rposition(|line| !line.is_empty());
    let (Some(first), Some(last)) = (first, last) else {
        return "re-read the file and copy the current text".into();
    };
    let wanted = &wanted[first..=last];
    let lines = text.lines().map(str::trim).collect::<Vec<_>>();
    let starts = lines
        .windows(wanted.len())
        .enumerate()
        .filter(|(_, window)| *window == wanted)
        .map(|(index, _)| index + 1)
        .take(3)
        .collect::<Vec<_>>();
    if let Some(&start) = starts.first() {
        let location = if wanted.len() == 1 {
            format!("line {start}")
        } else {
            format!("lines {start}-{}", start + wanted.len() - 1)
        };
        let more = if starts.len() > 1 {
            " (and others)"
        } else {
            ""
        };
        return format!(
            "{location}{more} match when leading and trailing whitespace is ignored; copy the exact text from the file"
        );
    }
    if wanted.len() > 1
        && let Some(index) = lines.iter().position(|line| *line == wanted[0])
    {
        return format!(
            "its first line matches line {}, but later lines differ; re-read the file and copy the current text",
            index + 1
        );
    }
    "re-read the file and copy the current text".into()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TestDir, temp_path};
    use super::*;

    #[test]
    fn edit_file_requires_unique_match() -> std::io::Result<()> {
        let path = temp_path("edit.txt");
        std::fs::write(&path, "aaa bbb aaa")?;
        let p = path.to_string_lossy();

        let dup = edit_file(&json!({"path": &p, "old_string": "aaa", "new_string": "x"}));
        assert!(dup.is_error);
        assert!(dup.content.contains("2 times"));

        let ok = edit_file(&json!({"path": &p, "old_string": "bbb", "new_string": "yyy"}));
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(&path)?, "aaa yyy aaa");
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn edit_file_rejects_files_over_the_edit_limit() -> std::io::Result<()> {
        let path = temp_path("too-large-to-edit.txt");
        let file = std::fs::File::create(&path)?;
        file.set_len(MAX_EDIT_FILE_BYTES + 1)?;
        drop(file);

        let out = edit_file(&serde_json::json!({
            "path": path.to_string_lossy(),
            "old_string": "a",
            "new_string": "b"
        }));
        assert!(out.is_error);
        assert!(out.content.contains("edit limit"));
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn edit_file_replaces_all_and_reports_lines() {
        let root = TestDir::new();
        let path = root.write("rename.rs", "let a = 1;\nuse_a(a);\n\nlet b = a;\n");
        let edit = |args: Value| edit_file(&args);

        let repeated = edit(json!({"path": path, "old_string": "a", "new_string": "count"}));
        assert!(repeated.is_error);
        assert!(repeated.content.contains("(lines 1, 2, 2, 4)"));
        assert!(repeated.content.contains("replace_all"));

        let all = edit(json!({
            "path": path, "old_string": "a)", "new_string": "count)", "replace_all": true
        }));
        assert_eq!(all.content, format!("edited {} (line 2)", path.display()));
        let multi = edit(json!({
            "path": path, "old_string": "let", "new_string": "let\n    ", "replace_all": true
        }));
        assert_eq!(
            multi.content,
            format!("edited {} (2 replacements at lines 1, 5)", path.display())
        );
        let same = edit(json!({"path": path, "old_string": "x", "new_string": "x"}));
        assert!(same.content.contains("identical"));
        let lines = edit(json!({
            "path": path, "old_string": "use_a(count);", "new_string": "one();\ntwo();\n"
        }));
        assert!(lines.content.ends_with("(lines 3-4)"), "{}", lines.content);
    }

    #[test]
    fn edit_file_matches_crlf_files_and_hints_at_whitespace_mismatches() {
        let root = TestDir::new();
        let path = root.write("windows.txt", "first\r\n    second\r\nthird\r\n");

        let crlf = edit_file(&json!({
            "path": path, "old_string": "first\n    second", "new_string": "one\n    two"
        }));
        assert!(!crlf.is_error, "{}", crlf.content);
        assert!(crlf.content.contains("matched CRLF line endings"));
        assert_eq!(
            fs::read_to_string(&path).expect("edited"),
            "one\r\n    two\r\nthird\r\n"
        );

        let indented = edit_file(&json!({
            "path": path, "old_string": "one\ntwo", "new_string": "x"
        }));
        assert!(indented.is_error);
        assert!(
            indented
                .content
                .contains("lines 1-2 match when leading and trailing whitespace is ignored"),
            "{}",
            indented.content
        );
        let stale = edit_file(&json!({
            "path": path, "old_string": "one\nchanged", "new_string": "x"
        }));
        assert!(stale.content.contains("its first line matches line 1"));
    }

    #[test]
    fn crlf_matching_applies_to_every_replacement_and_keeps_exact_matches_untouched() {
        let root = TestDir::new();
        let path = root.write("crlf.txt", "a\r\nb\r\nc\r\na\r\nb\r\n");
        let ambiguous =
            edit_file(&json!({"path": path, "old_string": "a\nb", "new_string": "x\ny"}));
        assert!(ambiguous.is_error);
        assert!(
            ambiguous.content.contains("2 times"),
            "{}",
            ambiguous.content
        );

        let all = edit_file(&json!({
            "path": path, "old_string": "a\nb", "new_string": "x\ny", "replace_all": true
        }));
        assert!(!all.is_error, "{}", all.content);
        assert_eq!(
            fs::read_to_string(&path).expect("edited"),
            "x\r\ny\r\nc\r\nx\r\ny\r\n"
        );

        // An old string that already matches exactly is used as written.
        let exact = edit_file(&json!({
            "path": path, "old_string": "y\r\nc", "new_string": "z"
        }));
        assert!(!exact.is_error, "{}", exact.content);
        assert!(!exact.content.contains("CRLF"));
        assert_eq!(
            fs::read_to_string(&path).expect("edited"),
            "x\r\nz\r\nx\r\ny\r\n"
        );

        // LF files never gain carriage returns.
        let lf = root.write("lf.txt", "a\nb\n");
        let edited = edit_file(&json!({"path": lf, "old_string": "a\nb", "new_string": "c\nd"}));
        assert!(!edited.is_error);
        assert_eq!(fs::read_to_string(&lf).expect("edited"), "c\nd\n");
    }

    #[test]
    fn write_file_reports_created_and_overwritten_files() {
        let root = TestDir::new();
        let path = root.0.join("notes.txt");
        let created = write_file(&json!({"path": path, "content": "hello"}));
        assert_eq!(
            created.content,
            format!("created {} (5 bytes)", path.display())
        );
        let overwritten = write_file(&json!({"path": path, "content": "hi"}));
        assert_eq!(
            overwritten.content,
            format!("overwrote {} (2 bytes; previously 5 bytes)", path.display())
        );
    }

    #[test]
    fn write_file_creates_parent_dirs() -> std::io::Result<()> {
        let dir = temp_path("nested");
        let file = dir.join("a/b.txt");
        let out = write_file(&json!({"path": file.to_string_lossy(), "content": "hi"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file)?, "hi");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn writes_replace_files_and_leave_no_temporaries() -> std::io::Result<()> {
        let dir = TestDir::new();
        let file = dir.0.join("notes.txt");
        write_atomically(&file, b"created")?;
        assert_eq!(std::fs::read_to_string(&file)?, "created");
        write_atomically(&file, b"replaced")?;
        assert_eq!(std::fs::read_to_string(&file)?, "replaced");

        // A hard-linked file is rewritten in place so both names see it.
        let alias = dir.0.join("alias.txt");
        std::fs::hard_link(&file, &alias)?;
        write_atomically(&file, b"shared")?;
        assert_eq!(std::fs::read_to_string(&alias)?, "shared");

        let leftovers = std::fs::read_dir(&dir.0)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn read_only_files_are_not_replaced() -> std::io::Result<()> {
        let dir = TestDir::new();
        let file = dir.write("locked.txt", "old");
        let mut permissions = std::fs::metadata(&file)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file, permissions)?;

        let error = write_atomically(&file, b"new").unwrap_err();
        assert!(error.to_string().contains("cannot replace"), "{error}");
        assert_eq!(std::fs::read_to_string(&file)?, "old");

        let mut permissions = std::fs::metadata(&file)?.permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&file, permissions)
    }

    #[cfg(unix)]
    #[test]
    fn writes_replace_atomically_and_keep_links_and_permissions() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        let file = dir.0.join("script.sh");
        std::fs::write(&file, "old")?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750))?;
        write_atomically(&file, b"new")?;
        assert_eq!(std::fs::read_to_string(&file)?, "new");
        assert_eq!(
            std::fs::metadata(&file)?.permissions().mode() & 0o777,
            0o750
        );

        // Writing through a symlink changes the target and keeps the link.
        let link = dir.0.join("link.sh");
        std::os::unix::fs::symlink(&file, &link)?;
        write_atomically(&link, b"through link")?;
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&file)?, "through link");

        // A hard-linked file is rewritten in place so both names see it.
        let alias = dir.0.join("alias.sh");
        std::fs::hard_link(&file, &alias)?;
        write_atomically(&file, b"shared")?;
        assert_eq!(std::fs::read_to_string(&alias)?, "shared");

        // So is a hard-linked file reached through a symlink.
        write_atomically(&link, b"shared through link")?;
        assert_eq!(std::fs::read_to_string(&alias)?, "shared through link");

        // A private file stays private.
        let secret = dir.0.join("secret");
        std::fs::write(&secret, "old")?;
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))?;
        write_atomically(&secret, b"new")?;
        assert_eq!(std::fs::read_to_string(&secret)?, "new");
        assert_eq!(
            std::fs::metadata(&secret)?.permissions().mode() & 0o777,
            0o600
        );

        // No temporary files are left behind.
        let leftovers = std::fs::read_dir(&dir.0)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        Ok(())
    }

    #[test]
    fn writes_fall_back_to_in_place_when_the_temporary_name_is_taken() -> std::io::Result<()> {
        let dir = TestDir::new();
        let file = dir.0.join("notes.txt");
        std::fs::write(&file, "old")?;
        let stale = dir
            .0
            .join(format!(".notes.txt.yawl-{}.tmp", std::process::id()));
        std::fs::write(&stale, "left by a crash")?;
        write_atomically(&file, b"new")?;
        assert_eq!(std::fs::read_to_string(&file)?, "new");
        assert_eq!(std::fs::read_to_string(&stale)?, "left by a crash");
        Ok(())
    }
}
