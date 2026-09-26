//! `write_file` and `edit_file`: whole-file writes and exact string
//! replacement.

use std::borrow::Cow;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

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
    match std::fs::write(path, content) {
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
    if let Err(e) = std::fs::write(path, updated) {
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
}
