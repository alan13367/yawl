//! `list_files` and `search_files`: bounded recursive listing and literal
//! text search for restricted children. No shell execution.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::{MAX_PAGE_BYTES, check_canceled, number, regular_file};
use crate::tools::{ToolEntry, ToolImpl, ToolOutcome, str_arg};

const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 32;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// The allowlisted `list_files` and `search_files` entries.
pub(in crate::tools) fn entries() -> Vec<ToolEntry> {
    [("list_files", false), ("search_files", true)]
        .into_iter()
        .map(|(name, search)| {
            let mut properties = json!({
                "path": {"type": "string", "description": "File or directory to inspect; default cwd."},
                "path_contains": {"type": "string", "description": "Optional literal substring filter on file paths."},
                "include_hidden": {"type": "boolean", "description": "Include nested dotfiles and hidden directories; default false. An explicitly named hidden path is always inspected."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum results; default 100."}
            });
            if search {
                properties["query"] = json!({"type": "string", "description": "Nonempty, case-sensitive literal text; not a regex."});
            }
            ToolEntry::new(crate::provider::ToolSpec {
                    name: name.into(),
                    description: format!(
                        "{}. Read-only, bounded recursion; skips hidden entries, symlinks, target and node_modules by default. Does not interpret ignore files. Narrow path when truncated.",
                        if search { "Search UTF-8 files, returning paths and 1-based line numbers" } else { "List files recursively" }
                    ),
                    input_schema: json!({
                        "type": "object", "properties": properties,
                        "required": if search { vec!["query"] } else { vec![] }
                    }),
                }, if search { ToolImpl::SearchFiles } else { ToolImpl::ListFiles })
        })
        .collect()
}

pub(in crate::tools) fn discover(args: &Value, search: bool) -> ToolOutcome {
    match inspect(args, search) {
        Ok(value) => ToolOutcome::ok(format_discovery(&value, search)),
        Err(error) => ToolOutcome::error(error),
    }
}

fn format_discovery(value: &Value, search: bool) -> String {
    let mut lines = Vec::new();
    if let Some(results) = value["results"].as_array() {
        for result in results {
            let path = escape_controls(result["path"].as_str().unwrap_or_default());
            if search {
                let text = escape_controls(result["text"].as_str().unwrap_or_default());
                let suffix = if result["line_truncated"] == true {
                    " [line excerpt]"
                } else {
                    ""
                };
                lines.push(format!(
                    "{path}:{}:{}: {text}{suffix}",
                    result["line"], result["column"]
                ));
            } else {
                lines.push(path);
            }
        }
    }
    if lines.is_empty() {
        lines.push(if search { "No matches." } else { "No files." }.into());
    }
    if value["truncated"] == true {
        lines.push("[truncated; narrow path or path_contains]".into());
    }
    if let Some(skipped) = value["skipped"].as_u64().filter(|skipped| *skipped > 0) {
        lines.push(format!(
            "[skipped {skipped} unreadable, non-text, oversized, special or non-UTF-8 path entries]"
        ));
    }
    lines.join("\n")
}

fn escape_controls(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

fn optional_text<'a>(args: &'a Value, key: &str, default: &'a str) -> Result<&'a str, String> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .ok_or_else(|| format!("'{key}' must be a string")),
    }
}

fn inspect(args: &Value, search: bool) -> Result<Value, String> {
    let root = PathBuf::from(optional_text(args, "path", ".")?);
    let filter = optional_text(args, "path_contains", "")?;
    let limit = number(args, "limit", 100, 1, 200)? as usize;
    let include_hidden = match args.get("include_hidden") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "'include_hidden' must be a boolean".to_string())?,
    };
    let query = if search {
        let query = str_arg(args, "query").map_err(|error| error.content)?;
        if query.is_empty() || query.len() > MAX_PAGE_BYTES {
            return Err("query must contain 1 through 32768 bytes".into());
        }
        Some(query)
    } else {
        None
    };
    let metadata =
        fs::symlink_metadata(&root).map_err(|error| format!("{}: {error}", root.display()))?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(
            "path must be a regular file or directory, not a symlink or special file".into(),
        );
    }
    let mut pending = vec![(root, 0)];
    let mut results = Vec::new();
    let mut visited = 0;
    let mut scanned = 0;
    let mut output_bytes = 0;
    let mut skipped = 0;
    let mut truncated = false;
    while let Some((path, depth)) = pending.pop() {
        check_canceled()?;
        if visited == MAX_ENTRIES {
            truncated = true;
            break;
        }
        visited += 1;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if metadata.is_dir() {
            if depth >= MAX_DEPTH {
                truncated = true;
                continue;
            }
            let children = match fs::read_dir(&path) {
                Ok(children) => children,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let remaining = MAX_ENTRIES.saturating_sub(visited + pending.len());
            let mut paths = Vec::new();
            for (index, child) in children.take(remaining + 1).enumerate() {
                check_canceled()?;
                let child = match child {
                    Ok(child) => child,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                if index == remaining {
                    truncated = true;
                    break;
                }
                if !include_hidden && child.file_name().as_encoded_bytes().starts_with(b".") {
                    continue;
                }
                if SKIP_DIRS.iter().any(|name| child.file_name() == *name)
                    && child.file_type().is_ok_and(|kind| kind.is_dir())
                {
                    continue;
                }
                paths.push(child.path());
            }
            paths.sort();
            pending.extend(paths.into_iter().rev().map(|path| (path, depth + 1)));
            continue;
        }
        if !metadata.is_file() {
            skipped += 1;
            continue;
        }
        let Some(path_text) = path.to_str() else {
            skipped += 1;
            continue;
        };
        if !path_text.contains(filter) {
            continue;
        }
        if let Some(query) = query {
            if metadata.len() > MAX_FILE_BYTES as u64 {
                skipped += 1;
                continue;
            }
            if scanned >= MAX_SCAN_BYTES {
                truncated = true;
                break;
            }
            let read_limit = MAX_FILE_BYTES.min(MAX_SCAN_BYTES - scanned);
            let mut file = match regular_file(&path) {
                Ok(file) => file,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                check_canceled()?;
                let remaining = read_limit + 1 - bytes.len();
                if remaining == 0 {
                    break;
                }
                let count = match file.read(&mut buffer[..remaining.min(8192)]) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(_) => {
                        skipped += 1;
                        bytes.clear();
                        break;
                    }
                };
                scanned += count;
                bytes.extend_from_slice(&buffer[..count]);
            }
            if bytes.len() > read_limit {
                truncated = true;
                skipped += 1;
                continue;
            }
            if bytes.contains(&0) {
                skipped += 1;
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                skipped += 1;
                continue;
            };
            for (line, text) in text.lines().enumerate() {
                check_canceled()?;
                let Some(column) = text.find(query) else {
                    continue;
                };
                let mut start = column.saturating_sub(256);
                while !text.is_char_boundary(start) {
                    start -= 1;
                }
                let snippet = prefix(&text[start..], 1024);
                let value = json!({"path": path_text, "line": line + 1, "column": text[..column].chars().count() + 1,
                    "text": snippet, "line_truncated": snippet.len() < text.len()});
                output_bytes += value.to_string().len();
                if results.len() == limit || output_bytes > MAX_OUTPUT_BYTES {
                    truncated = true;
                    break;
                }
                results.push(value);
            }
        } else {
            let value = json!({"path": path_text});
            output_bytes += value.to_string().len();
            if results.len() == limit || output_bytes > MAX_OUTPUT_BYTES {
                truncated = true;
                break;
            }
            results.push(value);
        }
        if output_bytes > MAX_OUTPUT_BYTES || results.len() == limit {
            truncated |= !pending.is_empty();
            break;
        }
    }
    Ok(
        json!({"results": results, "truncated": truncated, "skipped": skipped,
        "note": "Skips symlinks, special/binary/oversized/unreadable files, and nested .git/target/node_modules directories. Narrow path or path_contains if truncated."}),
    )
}

fn prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::super::test_support::TestDir;
    use super::*;

    #[test]
    fn default_discovery_excludes_hidden_dependency_trees() {
        let root = TestDir::new();
        root.write("src/main.rs", "needle");
        root.write(".pi/git/vendor/noise.rs", "needle");
        root.write(".DS_Store", "needle");
        for search in [false, true] {
            let value =
                inspect(&json!({"path": root.0, "query": "needle"}), search).expect("inspection");
            let paths = value["results"].as_array().expect("results");
            assert_eq!(
                paths.len(),
                1,
                "hidden dependencies polluted discovery: {value}"
            );
            assert!(
                paths[0]["path"]
                    .as_str()
                    .expect("path")
                    .ends_with("src/main.rs")
            );
        }
        let included = inspect(&json!({"path": root.0, "include_hidden": true}), false)
            .expect("include hidden");
        assert_eq!(included["results"].as_array().expect("results").len(), 3);
        let explicit =
            inspect(&json!({"path": root.0.join(".pi")}), false).expect("explicit hidden scope");
        assert_eq!(explicit["results"].as_array().expect("results").len(), 1);
    }

    #[test]
    fn file_tool_outputs_are_readable_lines_not_json_envelopes() {
        let root = TestDir::new();
        let path = root.write("src/example.rs", "fn example() {}\n");
        let listing = discover(&json!({"path": root.0}), false);
        assert!(!listing.is_error);
        assert_eq!(listing.content, path.display().to_string());
        let search = discover(&json!({"path": root.0, "query": "example"}), true);
        assert_eq!(
            search.content,
            format!("{}:1:4: fn example() {{}}", path.display())
        );
    }

    #[test]
    fn discovery_is_scoped_and_skips_generated_trees_and_symlinks() {
        let root = TestDir::new();
        root.write("src/a.rs", "first\nfind me here\n");
        root.write("src/b.txt", "find me here");
        root.write("target/generated.rs", "find me here");
        root.write("node_modules/package/a.rs", "find me here");
        root.write(".git/config", "find me here");
        root.write("src/binary.rs", b"find me here\0\xff");
        symlink(&root.0, root.0.join("loop")).expect("symlink loop");
        symlink(root.0.join("src/a.rs"), root.0.join("alias.rs")).expect("file symlink");

        let listed =
            inspect(&json!({"path": root.0, "path_contains": "a.rs"}), false).expect("list");
        assert_eq!(listed["results"].as_array().expect("files").len(), 1);
        assert!(
            listed["results"][0]["path"]
                .as_str()
                .expect("path")
                .ends_with("src/a.rs")
        );
        let found = inspect(
            &json!({"path": root.0, "path_contains": ".rs", "query": "find me"}),
            true,
        )
        .expect("search");
        assert_eq!(found["results"].as_array().expect("matches").len(), 1);
        assert_eq!(found["results"][0]["line"], 2);
        assert_eq!(found["results"][0]["column"], 1);
        assert_eq!(found["results"][0]["text"], "find me here");
        assert_eq!(found["truncated"], false);
        assert!(inspect(&json!({"path": root.0.join("alias.rs")}), false).is_err());
    }

    // macOS filesystems reject these names before discovery can encounter them.
    #[cfg(target_os = "linux")]
    #[test]
    fn discovery_skips_non_utf8_filenames_and_continues() {
        use std::os::unix::ffi::OsStringExt;

        let root = TestDir::new();
        root.write("valid.rs", "needle\n");
        let invalid = root
            .0
            .join(std::ffi::OsString::from_vec(b"bad-\xff.rs".to_vec()));
        fs::write(invalid, "needle\n").expect("non-UTF-8 filename");
        for search in [false, true] {
            let result = inspect(&json!({"path": root.0, "query": "needle"}), search)
                .expect("discovery must not panic");
            assert_eq!(result["skipped"], 1);
            assert_eq!(result["results"].as_array().expect("results").len(), 1);
            assert!(
                result["results"][0]["path"]
                    .as_str()
                    .expect("path")
                    .ends_with("valid.rs")
            );
            assert!(format_discovery(&result, search).contains("skipped 1"));
        }
    }

    #[test]
    fn literal_search_does_not_interpret_regex_or_shell_syntax() {
        let root = TestDir::new();
        let text = "a.b\naXb\n$(touch should-not-exist)\n";
        root.write("code.txt", text);
        for query in ["a.b", "$(touch should-not-exist)"] {
            let result =
                inspect(&json!({"path": root.0, "query": query}), true).expect("literal search");
            assert_eq!(result["results"].as_array().expect("matches").len(), 1);
            assert_eq!(result["results"][0]["text"], query);
        }
        assert!(!root.0.join("should-not-exist").exists());
        assert_eq!(
            fs::read_to_string(root.0.join("code.txt")).expect("unchanged"),
            text
        );
    }

    #[test]
    fn output_limits_are_explicit_and_long_line_snippets_include_the_match() {
        let root = TestDir::new();
        root.write("a", "match");
        root.write("b", "match");
        let listing = inspect(&json!({"path": root.0, "limit": 1}), false).expect("bounded list");
        assert_eq!(listing["results"].as_array().expect("files").len(), 1);
        assert_eq!(listing["truncated"], true);
        let text = format!("{}MATCH{}\n", "é".repeat(1000), "x".repeat(1000));
        let path = root.write("long", text.repeat(200));
        let result = inspect(&json!({"path": path, "query": "MATCH", "limit": 200}), true)
            .expect("bounded search");
        assert_eq!(result["truncated"], true);
        assert!(result.to_string().len() < MAX_OUTPUT_BYTES + 1024);
        assert!(
            result["results"][0]["text"]
                .as_str()
                .expect("snippet")
                .contains("MATCH")
        );
        assert_eq!(result["results"][0]["line_truncated"], true);
        assert_eq!(result["results"][0]["column"], 1001);
    }

    #[test]
    fn oversized_and_special_files_do_not_block_inspection() {
        let root = TestDir::new();
        root.write("large", vec![b'x'; MAX_FILE_BYTES + 1]);
        let fifo = root.0.join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo")
                .success()
        );
        let found = inspect(&json!({"path": root.0, "query": "x"}), true).expect("search");
        assert!(found["results"].as_array().expect("results").is_empty());
        assert_eq!(found["skipped"], 2);
    }

    #[test]
    fn invalid_inputs_are_rejected_and_cancellation_is_observed() {
        let root = TestDir::new();
        root.write("a", "hello");
        for args in [
            json!({"path": root.0, "limit": 0}),
            json!({"path": root.0, "limit": "2"}),
            json!({"path": root.0, "path_contains": 3}),
        ] {
            assert!(inspect(&args, false).is_err());
        }
        assert!(inspect(&json!({"path": root.0, "query": ""}), true).is_err());
        let token = crate::cancellation::CancellationToken::default();
        token.cancel();
        let result =
            crate::cancellation::scope(&token, || inspect(&json!({"path": root.0}), false));
        assert!(result.expect_err("canceled").contains("interrupted"));
    }
}
