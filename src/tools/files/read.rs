//! `read_file`: whole text files, numbered line pages, UTF-8 byte pages, and
//! image input.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde_json::{Value, json};

use super::{MAX_PAGE_BYTES, check_canceled, number, regular_file};
use crate::tools::{ToolEntry, ToolImpl, ToolOutcome, str_arg};

/// Largest text file `read_file` returns whole; it stays under the 60,000
/// character tool-result cap. Larger files return a numbered first page.
const FULL_READ_BYTES: usize = 56 * 1024;
const DEFAULT_LINE_COUNT: u64 = 400;
const MAX_LINE_COUNT: u64 = 2_000;

pub(super) fn entry() -> ToolEntry {
    ToolEntry::new(crate::provider::ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 text file or a PNG, JPEG, GIF, or WebP image. Text over 56 KiB returns a numbered first page. start_line/line_count read numbered line ranges; offset/limit read byte pages, for saved outputs and very long lines.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (absolute or relative to cwd)"},
                    "start_line": {"type": "integer", "minimum": 1, "description": "1-based first line; continue with returned next_start_line."},
                    "line_count": {"type": "integer", "minimum": 1, "maximum": MAX_LINE_COUNT, "description": "Lines to return; default 400."},
                    "offset": {"type": "integer", "minimum": 0, "description": "Byte offset; continue with returned next_offset."},
                    "limit": {"type": "integer", "minimum": 4, "maximum": 32768, "description": "Byte page size; default 16384."}
                },
                "required": ["path"]
            }),
        }, ToolImpl::ReadFile)
}

fn read_page(args: &Value) -> ToolOutcome {
    match page(args) {
        Ok(value) => {
            let content = value["content"].as_str().unwrap_or_default();
            let continuation = match value["next_offset"].as_u64() {
                Some(offset) => format!("next_offset={offset}"),
                None => "EOF".into(),
            };
            ToolOutcome::ok(format!(
                "bytes from {} of {}; {continuation}\n\n{content}",
                value["offset"], value["total_bytes"]
            ))
        }
        Err(error) => ToolOutcome::error(error),
    }
}

fn page(args: &Value) -> Result<Value, String> {
    check_canceled()?;
    let path = str_arg(args, "path").map_err(|error| error.content)?;
    let offset = number(args, "offset", 0, 0, u64::MAX)?;
    let limit = number(args, "limit", 16 * 1024, 4, MAX_PAGE_BYTES as u64)?;
    let mut file =
        regular_file(Path::new(path)).map_err(|error| format!("cannot read {path}: {error}"))?;
    let total = file.metadata().map_err(|error| error.to_string())?.len();
    if offset > total {
        return Err(format!("offset {offset} is beyond the file length {total}"));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    check_canceled()?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() && offset + (bytes.len() as u64) < total => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|error| error.to_string())?
        }
        Err(_) => {
            return Err(
                "paged reads require UTF-8 text and an offset at a character boundary".into(),
            );
        }
    };
    let end = offset + text.len() as u64;
    if text.is_empty() && offset < total {
        return Err("file changed while reading; retry from offset 0".into());
    }
    Ok(json!({"path": path, "offset": offset, "total_bytes": total,
        "next_offset": (end < total).then_some(end), "content": text}))
}

pub(in crate::tools) fn read_file_for_model(args: &Value, supports_images: bool) -> ToolOutcome {
    let byte_page = args.get("offset").is_some() || args.get("limit").is_some();
    let line_page = args.get("start_line").is_some() || args.get("line_count").is_some();
    if byte_page && line_page {
        return ToolOutcome::error("use either start_line/line_count or offset/limit, not both");
    }
    if byte_page {
        return read_page(args);
    }
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let file = match open_for_read(Path::new(path)) {
        Ok(file) => file,
        Err(e) => return ToolOutcome::error(format!("cannot read {path}: {e}")),
    };
    if !line_page {
        return read_bounded_file(path, file, supports_images);
    }
    let range = number(args, "start_line", 1, 1, u64::MAX).and_then(|start| {
        number(args, "line_count", DEFAULT_LINE_COUNT, 1, MAX_LINE_COUNT)
            .map(|count| (start, count))
    });
    match range.and_then(|(start, count)| line_page_text(BufReader::new(file), start, count)) {
        Ok(text) => ToolOutcome::ok(text),
        Err(error) => ToolOutcome::error(format!("cannot read {path}: {error}")),
    }
}

fn open_for_read(path: &Path) -> std::io::Result<File> {
    // O_NONBLOCK keeps opening a FIFO from blocking; symlinks are followed.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        return Err(std::io::Error::other("it is a directory"));
    }
    if !metadata.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

struct LinePage {
    text: String,
    first: u64,
    last: u64,
    next: Option<u64>,
    /// Byte offset where a truncated final line continues.
    cut: Option<u64>,
}

fn line_page_text(reader: impl BufRead, start: u64, count: u64) -> Result<String, String> {
    let page = read_line_page(reader, start, count)?;
    let mut header = if page.last < page.first {
        "empty file".to_string()
    } else {
        format!("lines {}-{}", page.first, page.last)
    };
    if let Some(offset) = page.cut {
        header.push_str(&format!(
            "; line {} exceeds the page, continue it with offset={offset}",
            page.last
        ));
    }
    match page.next {
        Some(next) => header.push_str(&format!("; next_start_line={next}")),
        None => header.push_str("; EOF"),
    }
    Ok(format!("{header}\n\n{}", page.text))
}

fn read_line_page(mut reader: impl BufRead, start: u64, count: u64) -> Result<LinePage, String> {
    let past_end =
        |lines: u64| format!("start_line {start} is past the end of the file ({lines} lines)");
    let mut line = 1;
    let mut offset = 0;
    let mut at_line_start = true;
    while line < start {
        check_canceled()?;
        let chunk = reader.fill_buf().map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            return Err(past_end(if at_line_start { line - 1 } else { line }));
        }
        let (used, newline) = match chunk.iter().position(|&byte| byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (chunk.len(), false),
        };
        reader.consume(used);
        offset += used as u64;
        at_line_start = newline;
        if newline {
            line += 1;
        }
    }

    let mut page = LinePage {
        text: String::new(),
        first: start,
        last: start - 1,
        next: None,
        cut: None,
    };
    let mut bytes = Vec::new();
    loop {
        check_canceled()?;
        if page.last - (start - 1) == count {
            if !reader
                .fill_buf()
                .map_err(|error| error.to_string())?
                .is_empty()
            {
                page.next = Some(page.last + 1);
            }
            break;
        }
        let number = page.last + 1;
        let prefix = format!("{number}\t");
        let budget = MAX_PAGE_BYTES.saturating_sub(page.text.len() + prefix.len());
        bytes.clear();
        let (used, complete) = read_line_prefix(&mut reader, &mut bytes, budget)?;
        if !complete && page.last >= start {
            page.next = Some(number);
            break;
        }
        if used == 0 {
            break;
        }
        let content = if complete {
            let content = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
            let content = content.strip_suffix(b"\r").unwrap_or(content);
            std::str::from_utf8(content).map_err(|_| format!("line {number} is not valid UTF-8"))?
        } else {
            match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(error) if error.error_len().is_none() => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()])
                        .map_err(|error| error.to_string())?
                }
                Err(_) => return Err(format!("line {number} is not valid UTF-8")),
            }
        };
        page.text.push_str(&prefix);
        page.text.push_str(content);
        page.text.push('\n');
        page.last = number;
        if !complete {
            page.cut = Some(offset + content.len() as u64);
            page.next = Some(number + 1);
            break;
        }
        offset += used as u64;
    }
    if page.last < page.first && start > 1 {
        return Err(past_end(start - 1));
    }
    Ok(page)
}

/// Appends one line, including its newline, unless its content exceeds
/// `budget`; then only the first `budget` bytes are read. Returns the bytes
/// consumed and whether the whole line fit.
fn read_line_prefix(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    budget: usize,
) -> Result<(usize, bool), String> {
    let mut consumed = 0;
    loop {
        let chunk = reader.fill_buf().map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            return Ok((consumed, true));
        }
        let (take, newline) = match chunk.iter().position(|&byte| byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (chunk.len(), false),
        };
        let room = budget.saturating_sub(line.len());
        if take - usize::from(newline) > room {
            line.extend_from_slice(&chunk[..room]);
            reader.consume(room);
            return Ok((consumed + room, false));
        }
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        consumed += take;
        if newline {
            return Ok((consumed, true));
        }
    }
}

fn read_bounded_file(path: &str, mut reader: impl Read, supports_images: bool) -> ToolOutcome {
    const IMAGE_SIGNATURE_BYTES: usize = 12;
    let mut prefix = Vec::with_capacity(IMAGE_SIGNATURE_BYTES);
    if let Err(error) = reader.by_ref().take(12).read_to_end(&mut prefix) {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    let media_type = crate::image::media_type(&prefix);
    let reader = std::io::Cursor::new(prefix).chain(reader);
    let Some(media_type) = media_type else {
        return read_bounded_utf8(path, reader);
    };
    if !supports_images {
        return ToolOutcome::error("the selected model does not accept image input");
    }

    let mut bytes = Vec::new();
    if let Err(error) = reader
        .take(crate::image::MAX_IMAGE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
    {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    if bytes.len() > crate::image::MAX_IMAGE_BYTES {
        return ToolOutcome::error(format!(
            "{path} exceeds the {}-byte image limit",
            crate::image::MAX_IMAGE_BYTES
        ));
    }
    let size = bytes.len();
    ToolOutcome::image(
        format!("read {media_type} image from {path} ({size} bytes)"),
        crate::image::encode(media_type, &bytes),
    )
}

fn read_bounded_utf8(path: &str, mut reader: impl Read) -> ToolOutcome {
    let mut bytes = Vec::new();
    if let Err(error) = reader
        .by_ref()
        .take(FULL_READ_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
    {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    if bytes.len() <= FULL_READ_BYTES {
        return match String::from_utf8(bytes) {
            Ok(text) => ToolOutcome::ok(text),
            Err(_) => ToolOutcome::error(format!("{path} is not valid UTF-8 (binary file?)")),
        };
    }
    let reader = BufReader::new(std::io::Cursor::new(bytes).chain(reader));
    match line_page_text(reader, 1, DEFAULT_LINE_COUNT) {
        Ok(page) => ToolOutcome::ok(format!(
            "File too large to read at once; continue with start_line.\n{page}"
        )),
        Err(error) => ToolOutcome::error(format!("cannot read {path}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TestDir, temp_path};
    use super::*;

    #[test]
    fn large_text_reads_return_a_numbered_first_page() {
        let root = TestDir::new();
        let text = (1..=10_000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        assert!(text.len() > FULL_READ_BYTES);
        let path = root.write("large.txt", &text);

        let out = read_file_for_model(&json!({"path": path}), false);

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("File too large to read at once"));
        assert!(
            out.content
                .contains("lines 1-400; next_start_line=401\n\n1\tline 1\n")
        );
        assert!(out.content.ends_with("400\tline 400\n"));
        let small = root.write("small.txt", "plain\ntext\n");
        let whole = read_file_for_model(&json!({"path": small}), false);
        assert_eq!(whole.content, "plain\ntext\n");
    }

    #[test]
    fn line_range_reads_are_numbered_and_report_the_next_line() {
        let root = TestDir::new();
        let text = (1..=10)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let path = root.write("lines.txt", &text);
        let read = |args: Value| read_file_for_model(&args, false);

        let middle = read(json!({"path": path, "start_line": 3, "line_count": 2}));
        assert_eq!(
            middle.content,
            "lines 3-4; next_start_line=5\n\n3\tline 3\n4\tline 4\n"
        );
        let tail = read(json!({"path": path, "start_line": 9}));
        assert_eq!(tail.content, "lines 9-10; EOF\n\n9\tline 9\n10\tline 10\n");
        let past = read(json!({"path": path, "start_line": 11}));
        assert!(past.is_error);
        assert!(past.content.contains("past the end of the file (10 lines)"));
        let mixed = read(json!({"path": path, "start_line": 1, "offset": 0}));
        assert!(mixed.is_error);
        let empty = root.write("empty.txt", "");
        assert_eq!(
            read(json!({"path": empty, "start_line": 1})).content,
            "empty file; EOF\n\n"
        );
    }

    #[test]
    fn line_pages_split_at_the_byte_budget_without_losing_lines() {
        let root = TestDir::new();
        let lines = (1..=2_000)
            .map(|line| format!("{line:04} {}", "x".repeat(60)))
            .collect::<Vec<_>>();
        let path = root.write("wide.txt", lines.join("\n") + "\n");
        let mut start = 1;
        let mut seen = Vec::new();
        loop {
            let out = read_file_for_model(
                &json!({"path": path, "start_line": start, "line_count": 2000}),
                false,
            );
            assert!(!out.is_error, "{}", out.content);
            assert!(out.content.len() <= MAX_PAGE_BYTES + 128);
            let (header, body) = out.content.split_once("\n\n").expect("header");
            for row in body.lines() {
                let (_, text) = row.split_once('\t').expect("numbered row");
                seen.push(text.to_string());
            }
            match header.rsplit_once("next_start_line=") {
                Some((_, next)) => start = next.parse().expect("next line"),
                None => break,
            }
        }
        assert_eq!(seen, lines);
    }

    #[test]
    fn overlong_lines_point_to_byte_paging() {
        let root = TestDir::new();
        let text = format!("{}\nnext\n", "é".repeat(MAX_PAGE_BYTES));
        let path = root.write("long.txt", &text);
        let out = read_file_for_model(&json!({"path": path, "start_line": 1}), false);
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("next_start_line=2"));
        let offset = out
            .content
            .split_once("offset=")
            .and_then(|(_, rest)| rest.split(';').next())
            .and_then(|offset| offset.parse::<usize>().ok())
            .expect("continuation offset");
        let shown = out.content.split_once("\n\n1\t").expect("row").1;
        assert_eq!(shown.trim_end_matches('\n').len(), offset);
        let rest = page(&json!({"path": path, "offset": offset, "limit": MAX_PAGE_BYTES}))
            .expect("byte page continues the line");
        assert!(text[offset..].starts_with(rest["content"].as_str().expect("content")));
    }

    #[test]
    fn plain_reads_reject_fifos_and_directories_without_blocking() {
        let root = TestDir::new();
        let fifo = root.0.join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo")
                .success()
        );
        let out = read_file_for_model(&json!({"path": fifo}), false);
        assert!(out.is_error);
        assert!(out.content.contains("not a regular file"));
        assert!(page(&json!({"path": fifo, "offset": 0})).is_err());
        let dir = read_file_for_model(&json!({"path": root.0}), false);
        assert!(dir.is_error);
        assert!(dir.content.contains("directory"));
    }

    #[test]
    fn read_file_bounds_streams_without_relying_on_metadata() {
        let out = read_bounded_utf8("endless", std::io::repeat(b'a'));

        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains("line 1 exceeds the page, continue it with offset=")
        );
        assert!(out.content.len() < MAX_PAGE_BYTES + 256);
    }

    #[test]
    fn read_file_keeps_non_images_at_the_text_read_limit() {
        struct CountingRepeat(std::rc::Rc<std::cell::Cell<usize>>);

        impl Read for CountingRepeat {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                buffer.fill(b'a');
                self.0.set(self.0.get().saturating_add(buffer.len()));
                Ok(buffer.len())
            }
        }

        let bytes_read = std::rc::Rc::new(std::cell::Cell::new(0));
        let out = read_bounded_file("endless", CountingRepeat(bytes_read.clone()), false);

        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            bytes_read.get(),
            FULL_READ_BYTES + 1,
            "text detection must not read up to the larger image limit"
        );
    }

    #[test]
    fn read_file_returns_images_only_for_capable_models() -> std::io::Result<()> {
        let path = temp_path("read-image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\npayload")?;
        let args = json!({"path": path.to_string_lossy()});

        let supported = read_file_for_model(&args, true);
        assert!(!supported.is_error, "{}", supported.content);
        assert_eq!(supported.images.len(), 1);
        assert_eq!(supported.images[0].media_type, "image/png");

        let unsupported = read_file_for_model(&args, false);
        assert!(unsupported.is_error);
        assert!(unsupported.images.is_empty());
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn byte_pages_are_readable_text_not_json_envelopes() {
        let root = TestDir::new();
        let path = root.write("src/example.rs", "fn example() {}\n");
        let page = read_page(&json!({"path": path, "offset": 0, "limit": 1024}));
        assert!(page.content.contains("\n\nfn example() {}\n"));
        assert!(!page.content.starts_with('{'));
    }

    #[test]
    fn utf8_pages_round_trip_without_gaps_even_on_long_lines() {
        let root = TestDir::new();
        let text = "aé🦀\n".repeat(100);
        let path = root.write("report", &text);
        let mut offset = 0;
        let mut output = String::new();
        loop {
            let result = page(&json!({"path": path, "offset": offset, "limit": 5})).expect("page");
            output.push_str(result["content"].as_str().expect("content"));
            match result["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(output, text);
        assert!(page(&json!({"path": path, "offset": 2})).is_err());
        assert!(page(&json!({"path": path, "offset": text.len() + 1})).is_err());
        let eof = page(&json!({"path": path, "offset": text.len()})).expect("EOF");
        assert_eq!(eof["content"], "");
        assert!(eof["next_offset"].is_null());
    }

    #[test]
    fn paged_output_preserves_text_and_stays_below_the_output_cap() {
        let root = TestDir::new();
        let path = root.write("controls", "\0".repeat(MAX_PAGE_BYTES));
        let outcome = read_file_for_model(
            &json!({"path": path, "offset": 0, "limit": MAX_PAGE_BYTES}),
            false,
        );
        assert!(!outcome.is_error, "{}", outcome.content);
        let mut content = outcome.content;
        crate::tools::truncate_result(&mut content);
        let (header, body) = content.split_once("\n\n").expect("page header and body");
        assert!(header.ends_with("EOF"));
        assert_eq!(body, "\0".repeat(MAX_PAGE_BYTES));
    }
}
