//! Helpers shared by unit tests across modules.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A fresh path under the system temp directory, unique to this process,
/// moment, and call, for a test to create and remove.
pub(crate) fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "yawl-{label}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

/// Reads one HTTP request, headers plus its `Content-Length` body, from a
/// test server connection. Returns an empty string when the peer closes
/// first.
pub(crate) fn read_http_request(stream: &mut impl std::io::Read) -> std::io::Result<String> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut wanted = None;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(String::new());
        }
        request.extend_from_slice(&buffer[..read]);
        if wanted.is_none()
            && let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            wanted = Some(header_end + 4 + content_length);
        }
        if wanted.is_some_and(|wanted| request.len() >= wanted) {
            return Ok(String::from_utf8_lossy(&request).into_owned());
        }
    }
}

/// Writes a complete `text/event-stream` response carrying `body`.
pub(crate) fn write_event_stream(
    stream: &mut impl std::io::Write,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Writes the head of a chunked `200 OK` response followed by one chunk of
/// `chunk`, leaving the body open for the caller to end or abandon.
pub(crate) fn write_chunked_head(
    stream: &mut impl std::io::Write,
    chunk: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n",
        chunk.len()
    )?;
    stream.flush()
}

/// Streams `text` as one OpenAI-compatible chat completion.
pub(crate) fn write_completion(
    stream: &mut impl std::io::Write,
    text: &str,
) -> std::io::Result<()> {
    let event = serde_json::json!({
        "choices": [{"delta": {"content": text}, "finish_reason": null}]
    });
    write_event_stream(stream, &format!("data: {event}\n\ndata: [DONE]\n\n"))
}
