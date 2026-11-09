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
            description: "Replace exact `old_string` text in a file. It must match once unless replace_all is true; include enough context for uniqueness. To change several places in one file, pass them together in `edits` instead of making separate calls: they apply in order, and if any fails, none are written."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence; default false."},
                    "edits": {
                        "type": "array",
                        "description": "Several replacements for this file, used instead of old_string and new_string.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path"]
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

/// One requested replacement.
struct Edit<'a> {
    old: &'a str,
    new: &'a str,
    replace_all: bool,
}

impl<'a> Edit<'a> {
    fn parse(args: &'a Value) -> Result<Self, String> {
        let text = |key: &str| str_arg(args, key).map_err(|error| error.content);
        let edit = Edit {
            old: text("old_string")?,
            new: text("new_string")?,
            replace_all: match args.get("replace_all") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => return Err("'replace_all' must be a boolean".into()),
            },
        };
        if edit.old.is_empty() {
            return Err("old_string must not be empty".into());
        }
        if edit.old == edit.new {
            return Err("old_string and new_string are identical; nothing to change".into());
        }
        Ok(edit)
    }
}

/// The replacements a call asks for: its top-level strings, or its `edits`.
fn parse_edits(args: &Value) -> Result<Vec<Edit<'_>>, String> {
    // Some models fill in every field of a schema, so a null, empty, or blank
    // value means the field is not in use.
    let edits = args
        .get("edits")
        .filter(|edits| !edits.is_null() && edits.as_array().is_none_or(|edits| !edits.is_empty()));
    let Some(edits) = edits else {
        return Edit::parse(args).map(|edit| vec![edit]);
    };
    let filled = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    };
    if filled("old_string") || filled("new_string") {
        return Err("use either old_string and new_string or edits, not both".into());
    }
    let edits = edits
        .as_array()
        .ok_or("'edits' must be a non-empty array")?;
    edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            Edit::parse(edit).map_err(|error| {
                format!(
                    "edit {} of {}: {error}; no edits were applied",
                    index + 1,
                    edits.len()
                )
            })
        })
        .collect()
}

/// What one replacement did to the text.
struct Applied {
    lines: Vec<usize>,
    crlf: bool,
    /// Where the new text ends when it replaced a single match, for reporting
    /// the range it now covers.
    end_line: Option<usize>,
    deleted: bool,
}

fn apply_edit(text: &str, edit: &Edit<'_>, path: &str) -> Result<(String, Applied), String> {
    let (old_string, new_string, crlf) = match_line_endings(text, edit.old, edit.new);
    let count = text.matches(old_string.as_ref()).count();
    if count == 0 {
        return Err(format!(
            "old_string not found in {path}; {}",
            not_found_hint(text, &old_string)
        ));
    }
    if count > 1 && !edit.replace_all {
        return Err(format!(
            "old_string appears {count} times in {path} (lines {}); add surrounding context to make it unique, or set replace_all",
            line_list(&match_lines(text, &old_string))
        ));
    }
    let (updated, lines) = replace_matches(text, &old_string, &new_string);
    let end_line = (count == 1)
        .then(|| lines[0] + new_string.trim_end_matches('\n').matches('\n').count())
        .filter(|end| *end > lines[0]);
    Ok((
        updated,
        Applied {
            lines,
            crlf,
            end_line,
            deleted: new_string.is_empty(),
        },
    ))
}

/// Where one edit landed, as line numbers at the time it was applied.
fn applied_location(applied: &Applied) -> String {
    match applied.end_line {
        Some(end) => format!("{}-{end}", applied.lines[0]),
        None => line_list(&applied.lines),
    }
}

pub(in crate::tools) fn edit_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let edits = match parse_edits(args) {
        Ok(edits) => edits,
        Err(error) => return ToolOutcome::error(error),
    };
    if let Err(error) = reject_device(path) {
        return ToolOutcome::error(error);
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
    // Nothing is written until every edit has applied, so a failure part way
    // leaves the file as the model last read it.
    let total = edits.len();
    let mut current = text;
    let mut applied_edits = Vec::with_capacity(total);
    for (index, edit) in edits.iter().enumerate() {
        match apply_edit(&current, edit, path) {
            Ok((updated, applied)) => {
                current = updated;
                applied_edits.push(applied);
            }
            Err(error) if total > 1 => {
                let numbering = if index > 0 {
                    " Any line numbers above count the earlier edits in this batch."
                } else {
                    ""
                };
                return ToolOutcome::error(format!(
                    "edit {} of {total}: {error}; no edits were applied.{numbering}",
                    index + 1
                ));
            }
            Err(error) => return ToolOutcome::error(error),
        }
    }
    if let Err(e) = write_atomically(Path::new(path), current.as_bytes()) {
        return ToolOutcome::error(format!("cannot write {path}: {e}"));
    }
    let crlf = applied_edits.iter().any(|applied| applied.crlf);
    let mut message = if let [applied] = applied_edits.as_slice() {
        if applied.lines.len() > 1 {
            format!(
                "edited {path} ({} replacements at lines {})",
                applied.lines.len(),
                line_list(&applied.lines)
            )
        } else if applied.deleted {
            format!("edited {path} (deleted text at line {})", applied.lines[0])
        } else if applied.end_line.is_some() {
            format!("edited {path} (lines {})", applied_location(applied))
        } else {
            format!("edited {path} (line {})", applied.lines[0])
        }
    } else {
        format!(
            "edited {path} ({total} edits at lines {}); each line number is from when that edit applied",
            applied_edits
                .iter()
                .map(applied_location)
                .collect::<Vec<_>>()
                .join(", ")
        )
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
/// attributes, and alternate data streams. Replacing a read-only file, or one
/// that another program holds open without sharing deletes, fails with an
/// error that says so.
#[cfg(windows)]
fn replace_through_temporary(
    temporary: &Path,
    target: &Path,
    original: Option<&fs::Metadata>,
    contents: &[u8],
) -> std::io::Result<bool> {
    // Refused before the temporary file exists: a read-only copy of it could
    // not be deleted again once the rename fails.
    if original.is_some_and(|original| original.permissions().readonly()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("cannot replace {}: the file is read-only", target.display()),
        ));
    }
    let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
    else {
        return Ok(false);
    };
    let written = (|| {
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()
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
fn uses_only_crlf(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.contains("\r\n")
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte != b'\n' || (index > 0 && bytes[index - 1] == b'\r'))
}

fn match_line_endings<'a>(
    text: &str,
    old: &'a str,
    new: &'a str,
) -> (Cow<'a, str>, Cow<'a, str>, bool) {
    // New lines added to a file that uses CRLF throughout keep its line endings.
    if !old.contains('\n') && new.contains('\n') && !new.contains('\r') && uses_only_crlf(text) {
        return (old.into(), new.replace('\n', "\r\n").into(), true);
    }
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
    fn several_edits_apply_in_order_and_report_each_location() {
        let root = TestDir::new();
        let path = root.write("batch.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        let out = edit_file(&json!({"path": path, "edits": [
            {"old_string": "fn a() {}", "new_string": "fn a() {\n    one();\n}"},
            // Sees the text the first edit left behind.
            {"old_string": "one();", "new_string": "two();"},
            {"old_string": "fn", "new_string": "pub fn", "replace_all": true},
        ]}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "pub fn a() {\n    two();\n}\npub fn b() {}\npub fn c() {}\n"
        );
        assert!(
            out.content.contains("(3 edits at lines 1-3, 2, 1, 4, 5)"),
            "{}",
            out.content
        );
    }

    #[test]
    fn a_failed_edit_leaves_the_whole_batch_unwritten() {
        let root = TestDir::new();
        let original = "alpha\nbeta\nbeta\n";
        let path = root.write("atomic.txt", original);
        for (edits, expected) in [
            (
                json!([{"old_string": "alpha", "new_string": "A"}, {"old_string": "gone", "new_string": "B"}]),
                "edit 2 of 2: old_string not found",
            ),
            (
                json!([{"old_string": "alpha", "new_string": "A"}, {"old_string": "beta", "new_string": "B"}]),
                "edit 2 of 2: old_string appears 2 times",
            ),
            (
                json!([{"old_string": "alpha", "new_string": "A"}, {"old_string": "", "new_string": "B"}]),
                "edit 2 of 2: old_string must not be empty",
            ),
            (
                json!([{"old_string": "alpha", "new_string": "alpha"}]),
                "edit 1 of 1: old_string and new_string are identical",
            ),
            (
                json!([{"old_string": "alpha"}]),
                "edit 1 of 1: missing required",
            ),
        ] {
            let out = edit_file(&json!({"path": path, "edits": edits}));
            assert!(out.is_error, "{expected}");
            assert!(out.content.contains(expected), "{}", out.content);
            assert!(
                out.content.contains("no edits were applied"),
                "{}",
                out.content
            );
            assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
        }
    }

    #[test]
    fn fields_a_model_filled_in_without_using_them_do_not_fail_the_call() {
        let root = TestDir::new();
        for (args, expected) in [
            (
                json!({"old_string": "alpha", "new_string": "one", "edits": null}),
                "one\n",
            ),
            (
                json!({"old_string": "alpha", "new_string": "two", "edits": []}),
                "two\n",
            ),
            (
                json!({"old_string": "", "new_string": "", "edits": [{"old_string": "alpha", "new_string": "three"}]}),
                "three\n",
            ),
            (
                json!({"old_string": null, "new_string": null, "edits": [{"old_string": "alpha", "new_string": "four"}]}),
                "four\n",
            ),
        ] {
            let path = root.write("filled.txt", "alpha\n");
            let mut call = args.clone();
            call["path"] = json!(path);
            let out = edit_file(&call);
            assert!(!out.is_error, "{}", out.content);
            assert_eq!(std::fs::read_to_string(&path).expect("read"), expected);
        }
    }

    #[test]
    fn batch_line_numbers_say_what_they_count() {
        let root = TestDir::new();
        let path = root.write("numbers.txt", "a\nb\ndup\nc\ndup\n");
        let failed = edit_file(&json!({"path": path, "edits": [
            {"old_string": "a", "new_string": "a\nnew\nnew"},
            {"old_string": "dup", "new_string": "x"},
        ]}));
        assert!(failed.is_error);
        assert!(
            failed.content.contains("count the earlier edits"),
            "{}",
            failed.content
        );
        let first_fails = edit_file(&json!({"path": path, "edits": [
            {"old_string": "dup", "new_string": "x"},
            {"old_string": "a", "new_string": "z"},
        ]}));
        assert!(first_fails.is_error);
        assert!(
            !first_fails.content.contains("count the earlier"),
            "{}",
            first_fails.content
        );

        let done = edit_file(&json!({"path": path, "edits": [
            {"old_string": "a", "new_string": "A"},
            {"old_string": "b", "new_string": "B"},
        ]}));
        assert!(
            done.content.contains("from when that edit applied"),
            "{}",
            done.content
        );
    }

    #[test]
    fn new_lines_in_a_crlf_file_keep_its_line_endings() {
        let root = TestDir::new();
        let path = root.write("crlf.txt", "one\r\ntwo\r\n");
        let out =
            edit_file(&json!({"path": path, "old_string": "one", "new_string": "one\nextra"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "one\r\nextra\r\ntwo\r\n"
        );
        assert!(out.content.contains("CRLF"), "{}", out.content);

        // A file that mixes endings is left to what was written.
        let mixed = root.write("mixed.txt", "one\r\ntwo\nthree\r\n");
        edit_file(&json!({"path": mixed, "old_string": "one", "new_string": "one\nextra"}));
        assert_eq!(
            std::fs::read_to_string(&mixed).expect("read"),
            "one\nextra\r\ntwo\nthree\r\n"
        );
        // A plain file is untouched by any of this.
        let plain = root.write("plain.txt", "one\ntwo\n");
        edit_file(&json!({"path": plain, "old_string": "one", "new_string": "one\nextra"}));
        assert_eq!(
            std::fs::read_to_string(&plain).expect("read"),
            "one\nextra\ntwo\n"
        );
    }

    #[test]
    fn a_call_uses_old_string_or_edits_but_not_both() {
        let root = TestDir::new();
        let path = root.write("either.txt", "alpha\n");
        for args in [
            json!({"path": path, "edits": [], }),
            json!({"path": path, "edits": "alpha"}),
            json!({"path": path, "old_string": "alpha", "new_string": "b", "edits": [{"old_string": "alpha", "new_string": "c"}]}),
            json!({"path": path}),
        ] {
            assert!(edit_file(&args).is_error, "{args}");
        }
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "alpha\n");
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
        let leftovers = std::fs::read_dir(&dir.0)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        // Windows has no world-writable bits; this only clears the
        // read-only attribute so the test directory can be removed.
        let mut permissions = std::fs::metadata(&file)?.permissions();
        #[expect(clippy::permissions_set_readonly_false)]
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
